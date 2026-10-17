#!/usr/bin/env python3
"""Pane-side mouse probe for `poc mouse`.

Enables SGR mouse reporting, disables the pty echo, and copies every byte the
pane receives into a file.  The scenario compares that file with the bytes
`term_core::input::encode_mouse` produced, so the check covers the whole path:
encoder -> `send-keys -H` -> tmux -> pane pty -> application.

Usage: mouse-probe.py <output-file>
"""

import os
import sys
import tty


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: mouse-probe.py <output-file>", file=sys.stderr)
        return 2
    output = sys.argv[1]

    # A real TUI puts the pty in raw mode.  Without it the line discipline keeps
    # an escape sequence without a newline in canonical mode, the read never
    # returns, and the probe would look broken even though the bytes arrived.
    tty.setraw(0)

    # Enable normal + SGR mouse reporting, then dump everything received.
    os.write(1, b"\x1b[?1000h\x1b[?1006h")
    with open(output, "wb", buffering=0) as sink:
        while True:
            chunk = os.read(0, 4096)
            if not chunk:
                return 0
            sink.write(chunk)


if __name__ == "__main__":
    sys.exit(main())