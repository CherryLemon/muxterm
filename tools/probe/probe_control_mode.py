#!/usr/bin/env python3
"""Black-box probe of tmux control mode.

Purpose (clean-room evidence): every protocol behaviour this repository relies on
must be traceable to *public documentation* or to a *reproducible black-box
observation*.  This script produces the latter: raw byte captures of a real tmux
server talking control mode, together with a manifest recording the tmux version,
the exact command sequence and the time of capture.

It never reads tmux source code; it only drives the public command line interface
and records what comes back on the wire.

Usage:
    python3 probe_control_mode.py <out-dir>

Requires: python3, tmux >= 3.3, a working /dev/ptmx.
"""

from __future__ import annotations

import json
import os
import pty
import select
import shutil
import signal
import subprocess
import sys
import time

QUIET = 0.45          # seconds of silence that ends a read burst
HARD_TIMEOUT = 6.0    # seconds before we give up on a burst
READ_SIZE = 65536


# --------------------------------------------------------------------------- #
# helpers
# --------------------------------------------------------------------------- #
def tmux_version() -> str:
    out = subprocess.run(["tmux", "-V"], capture_output=True, text=True, check=True)
    return out.stdout.strip()


class Server:
    """A private tmux server on its own socket."""

    def __init__(self, socket_name: str):
        self.socket = socket_name

    def run(self, *args: str, check: bool = True) -> subprocess.CompletedProcess:
        proc = subprocess.run(
            ["tmux", "-L", self.socket, *args],
            capture_output=True,
            text=True,
            env=self.env(),
        )
        if check and proc.returncode != 0:
            raise RuntimeError(f"tmux {args} failed: {proc.stderr.strip()}")
        return proc

    @staticmethod
    def env() -> dict:
        env = dict(os.environ)
        env.pop("TMUX", None)  # never nest inside an ambient tmux
        return env

    def kill(self) -> None:
        subprocess.run(
            ["tmux", "-L", self.socket, "kill-server"],
            capture_output=True,
            env=self.env(),
        )


def read_burst(fd: int, quiet: float = QUIET, hard: float = HARD_TIMEOUT) -> bytes:
    """Read from fd until it stays silent for `quiet` seconds."""
    chunks: list[bytes] = []
    deadline = time.time() + hard
    while time.time() < deadline:
        ready, _, _ = select.select([fd], [], [], quiet)
        if not ready:
            break
        try:
            data = os.read(fd, READ_SIZE)
        except OSError:
            break
        if not data:
            break
        chunks.append(data)
    return b"".join(chunks)


# --------------------------------------------------------------------------- #
# probes
# --------------------------------------------------------------------------- #
def probe_attach_and_commands(server: Server) -> dict[str, bytes]:
    """Attach a control client over plain pipes and send a few commands."""
    server.run("new-session", "-d", "-s", "dev", "-x", "80", "-y", "24", "/bin/sh")
    # make the pane quiet and deterministic
    server.run("send-keys", "-t", "dev", "stty -echo; PS1='$ '; clear", "Enter")
    time.sleep(0.4)

    proc = subprocess.Popen(
        ["tmux", "-L", server.socket, "-C", "attach", "-t", "dev"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=Server.env(),
    )
    assert proc.stdout is not None and proc.stdin is not None
    out: dict[str, bytes] = {}

    fd = proc.stdout.fileno()
    out["00_attach_burst"] = read_burst(fd)

    def command(name: str, line: str) -> None:
        proc.stdin.write(line.encode() + b"\n")
        proc.stdin.flush()
        out[name] = read_burst(fd)

    # 1. command numbering / response framing, three commands in sequence
    command("01_cmd_display_message", "display-message -p '#{session_name}'")
    command("02_cmd_list_windows", "list-windows -t dev -F '#{window_id} #{window_index} #{window_name}'")
    command("03_cmd_error", "definitely-not-a-command")
    # 2. object model queries used by the synchroniser
    command(
        "04_cmd_list_panes",
        "list-panes -t dev -F '#{session_id}|#{window_id}|#{pane_id}|#{pane_index}|"
        "#{pane_width}x#{pane_height}|#{pane_active}|#{pane_dead}|#{pane_title}'",
    )
    command("05_cmd_list_sessions", "list-sessions -F '#{session_id} #{session_name} #{session_windows}'")
    command("06_cmd_show_options", "display-message -p '#{window_width}x#{window_height} #{client_width}x#{client_height}'")
    # 3. binary / non UTF-8 pane output
    command(
        "07_cmd_binary_output",
        "send-keys -t dev 'printf \"A\\\\377\\\\376B\\\\033[31mRED\\\\033[0mC\\\\n\"' Enter",
    )
    command(
        "08_cmd_wide_output",
        "send-keys -t dev 'printf \"\\344\\270\\255\\346\\226\\207OK\\\\n\"' Enter",
    )
    # 4. capture-pane snapshot shapes
    command("09_capture_plain", "capture-pane -p -t dev")
    command(
        "10_capture_styled",
        "capture-pane -p -e -q -J -S -32768 -E - -t dev",
    )
    command(
        "11_capture_meta",
        "display-message -p -t dev '#{history_size}/#{history_limit} #{alternate_on} "
        "#{cursor_x},#{cursor_y} #{pane_in_mode} #{pane_mode}'",
    )
    # 5. an opaque command whose output block contains a '%'-prefixed line
    command("12_percent_inside_block", "display-message -p '%hello'")

    proc.stdin.write(b"kill-server\n")
    proc.stdin.flush()
    out["13_exit_burst"] = read_burst(fd)
    out["14_after_exit"] = read_burst(fd, quiet=0.6)
    try:
        proc.wait(timeout=3)
    except subprocess.TimeoutExpired:
        proc.kill()
    out["15_client_stderr"] = proc.stderr.read() if proc.stderr else b""
    return out


def probe_notifications(server: Server) -> dict[str, bytes]:
    """Drive structural changes from a second client and capture notifications."""
    server.run("new-session", "-d", "-s", "notif", "-x", "80", "-y", "24", "/bin/sh")
    time.sleep(0.4)
    proc = subprocess.Popen(
        ["tmux", "-L", server.socket, "-C", "attach", "-t", "notif"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        env=Server.env(),
    )
    assert proc.stdout is not None and proc.stdin is not None
    fd = proc.stdout.fileno()
    out: dict[str, bytes] = {"00_attach_burst": read_burst(fd)}

    def external(name: str, *args: str) -> None:
        server.run(*args)
        out[name] = read_burst(fd)

    external("01_new_window", "new-window", "-t", "notif", "-n", "second")
    external("02_rename_window", "rename-window", "-t", "notif:1", "renamed")
    external("03_split_window", "split-window", "-h", "-t", "notif:1")
    external("04_split_window_vertical", "split-window", "-v", "-t", "notif:1")
    external("05_select_pane", "select-pane", "-t", "notif:1.1")
    external("06_resize_pane", "resize-pane", "-t", "notif:1.1", "-R", "5")
    external("07_layout_odd", "select-layout", "-t", "notif:1", "tiled")
    external("08_new_session", "new-session", "-d", "-s", "other", "-x", "80", "-y", "24", "/bin/sh")
    external("09_kill_window", "kill-window", "-t", "notif:1")
    external("10_kill_session", "kill-session", "-t", "other")
    external("11_window_name_special", "rename-window", "-t", "notif:0", "a b'c\"d\\e%f")
    external(
        "12_format_special",
        "display-message", "-p", "-t", "notif:0",
        "#{window_id} #{window_name} #{window_layout}",
    )
    # pane mode (copy-mode) is reported differently from ordinary output
    server.run("send-keys", "-t", "notif:0", "C-b", "[")
    out["13_enter_copy_mode"] = read_burst(fd)
    server.run("send-keys", "-t", "notif:0", "q")
    out["14_leave_copy_mode"] = read_burst(fd)
    out["15_layout_string"] = server.run(
        "display-message", "-p", "-t", "notif:0", "#{window_layout}"
    ).stdout.encode()

    proc.stdin.write(b"kill-server\n")
    proc.stdin.flush()
    out["16_exit"] = read_burst(fd)
    try:
        proc.wait(timeout=3)
    except subprocess.TimeoutExpired:
        proc.kill()
    return out


def probe_cc_entry(server: Server) -> dict[str, bytes]:
    """Capture the -CC takeover prologue/epilogue on a real pty."""
    server.run("new-session", "-d", "-s", "cc", "-x", "80", "-y", "24", "/bin/sh")
    time.sleep(0.4)
    master, slave = pty.openpty()
    proc = subprocess.Popen(
        ["tmux", "-L", server.socket, "-CC", "attach", "-t", "cc"],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env={**Server.env(), "TERM": "xterm-256color"},
        preexec_fn=os.setsid,
    )
    os.close(slave)
    out: dict[str, bytes] = {}
    out["00_cc_prologue"] = read_burst(master)
    os.write(master, b"display-message -p '#{session_name}'\n")
    out["01_cc_command"] = read_burst(master)
    os.write(master, b"kill-server\n")
    out["02_cc_exit"] = read_burst(master)
    out["03_cc_after_exit"] = read_burst(master, quiet=0.6)
    try:
        proc.wait(timeout=3)
    except subprocess.TimeoutExpired:
        proc.kill()
    os.close(master)
    return out


def probe_plain_shell_leak(server: Server) -> dict[str, bytes]:
    """Shell bytes that arrive after the control client detaches.

    This is the material for the "control commands must never leak into a plain
    shell" test: we look at what a *plain* (non control mode) client sees and
    confirm the switching rules we implement on top of it.
    """
    server.run("new-session", "-d", "-s", "leak", "-x", "80", "-y", "24", "/bin/sh")
    time.sleep(0.4)
    server.run("send-keys", "-t", "leak", "echo BEFORE-CONTROL", "Enter")
    time.sleep(0.4)
    master, slave = pty.openpty()
    proc = subprocess.Popen(
        ["tmux", "-L", server.socket, "-CC", "attach", "-t", "leak"],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env={**Server.env(), "TERM": "xterm-256color"},
        preexec_fn=os.setsid,
    )
    os.close(slave)
    out: dict[str, bytes] = {"00_prologue": read_burst(master)}
    # detach the control client while the shell keeps running
    os.write(master, b"detach-client\n")
    out["01_detach"] = read_burst(master)
    try:
        proc.wait(timeout=3)
    except subprocess.TimeoutExpired:
        proc.kill()
    os.close(master)
    server.run("send-keys", "-t", "leak", "echo AFTER-CONTROL", "Enter")
    time.sleep(0.4)
    out["02_pane_after"] = server.run("capture-pane", "-p", "-t", "leak").stdout.encode()
    server.kill()
    return out


# --------------------------------------------------------------------------- #
def main() -> int:
    out_dir = sys.argv[1] if len(sys.argv) > 1 else "fixtures"
    os.makedirs(out_dir, exist_ok=True)
    socket = f"muxterm-probe-{os.getpid()}"
    server = Server(socket)
    server.kill()

    manifest = {
        "tmux_version": tmux_version(),
        "captured_at_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "probe_script": "tools/probe/probe_control_mode.py",
        "python": sys.version.split()[0],
        "probes": {},
    }

    probes = {
        "attach_and_commands": probe_attach_and_commands,
        "notifications": probe_notifications,
        "cc_entry": probe_cc_entry,
        "plain_shell_leak": probe_plain_shell_leak,
    }

    failures: list[str] = []
    for name, fn in probes.items():
        entries: dict[str, str] = {}
        try:
            server.kill()
            time.sleep(0.2)
            data = fn(server)
            for key, blob in data.items():
                path = os.path.join(out_dir, f"{name}.{key}.bin")
                with open(path, "wb") as fh:
                    fh.write(blob)
                entries[key] = f"{name}.{key}.bin"
        except Exception as exc:  # keep going: partial fixtures are still useful
            failures.append(f"{name}: {exc!r}")
        manifest["probes"][name] = entries
        print(f"[probe] {name}: {len(entries)} captures", file=sys.stderr)

    server.kill()
    manifest["failures"] = failures
    with open(os.path.join(out_dir, "manifest.json"), "w") as fh:
        json.dump(manifest, fh, indent=2, sort_keys=True)
        fh.write("\n")

    # a copy of the probe script itself travels with the fixtures
    here = os.path.dirname(os.path.abspath(__file__))
    shutil.copy2(os.path.join(here, "probe_control_mode.py"), out_dir)
    print(f"[probe] wrote fixtures + manifest.json to {out_dir}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())