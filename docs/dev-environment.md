# Development environment: everything runs in an isolated sandbox

**Rule: no test, probe or build artefact is ever created outside this repository.**
A test run that starts a tmux server, allocates a PTY or writes to `/tmp` on the
host can attach to a real session, steal the terminal, or leave a server behind
that outlives the run. None of that happens here.

## Two runners, one interface

```bash
tools/container/build-image.sh   # once: build the test image
tools/container/test.sh          # fmt + clippy + unit tests + fixture replay
tools/container/e2e.sh           # the PoC scenarios against a real tmux
tools/container/run.sh <cmd>     # any command inside the sandbox
```

`tools/container/run.sh` picks a runner (`MUXTERM_RUNNER=docker|sandbox|auto`):

| Runner | When | What it gives |
|---|---|---|
| **docker** | default when the daemon can create containers | the test image, private `/tmp` tmpfs, separate namespaces |
| **sandbox** | fallback when the daemon is unusable (for example its storage is full) | `unshare` mount/PID/UTS/IPC namespaces + `chroot` into a rootfs inside `.sandbox/` |

Both give the same practical isolation: only this repository is visible, `/tmp` is
private, tmux servers and sockets live and die with the run, and no host process
can be reached or signalled.

### Why a docker *volume* and not a bind mount

The repository lives on a network filesystem that does not propagate into a
container's mount namespace: `-v "$ROOT":/work` silently produces an **empty**
directory. `tools/container/docker-run.sh` therefore streams the sources into a
named volume, runs the command against it, and streams recorded artefacts back:

```
host repo ──tar──▶ muxterm-work volume ──▶ container (private /tmp, own namespaces)
                         │
                         └──tar──▶ host repo   (only crates/tmux-protocol/tests/fixtures)
```

Sources flow one way, so a container run can never clobber an edit made while it
was executing. A one-off run that really must write more back sets
`MUXTERM_SYNC_BACK_EXTRA=crates` (used for `cargo fmt`).

### Isolation summary

| Isolation | Mechanism |
|---|---|
| only the repository is visible | volume mirror (docker) / chroot (sandbox) |
| `/tmp` is private and dies with the run | `--tmpfs /tmp` / tmpfs mount in the namespace |
| the tmux socket directory is private | `TMUX_TMPDIR=/tmp/tmux-runtime` inside the run |
| no host tmux server can be reached | every scenario uses `tmux -L <private-socket>`, and that socket lives in the private `/tmp` |
| no port is published | no `-p`; networking is used only for the crate registry and the proxy |
| build artefacts do not fill the workspace | `target/` and the cargo caches live in named volumes |
| zombies are reaped | `--init` |

Package and crate downloads go through the caller's proxy
(`$http_proxy`/`$https_proxy`), which is forwarded into the container. The daemon
itself needs no proxy configuration.

## The test image

`docker/test-image.Dockerfile` installs the test dependency set - tmux (the system
under test), python3 (the fixture probe), a C toolchain, and the libraries needed to
build further tmux versions - on top of a Rust base image.

The base is a build argument with a public default:

```bash
tools/container/build-image.sh                                  # rust:1-slim-bookworm
MUXTERM_BASE_IMAGE=some-local-image tools/container/build-image.sh   # reuse a local image
```

Any Debian/Ubuntu based image that provides (or can install) Rust, tmux and python3
works.  Pointing the build at an image that is already present locally is the
supported way to avoid a large download on a slow link; it changes nothing about how
the tests run.

## Provisioning the sandbox runner

Needed only when the Docker daemon is unavailable:

```bash
mkdir -p .sandbox && cd .sandbox
curl -sSL -o alpine.tar.gz https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/alpine-minirootfs-3.20.3-x86_64.tar.gz
mkdir -p rootfs && tar -xzf alpine.tar.gz -C rootfs
cd .. && tools/sandbox/setup-rootfs.sh      # apk: tmux, python3, toolchain, rustup
```

Two notes for that path: the outer file sandbox forbids `mount(2)` and
`/dev/ptmx`, so those runs need a wider sandbox mode; and `rustup` needs `TMPDIR`
on the same filesystem as `RUSTUP_HOME` (the rootfs sits on a network FS where
cross-device renames fail). `tools/sandbox/setup-rootfs.sh` handles the latter.

## Running the PoC scenarios

```bash
tools/container/run.sh bash -lc '
  set -e
  cargo build -p poc-cli
  ./target/debug/poc doctor
  ./target/debug/poc create    --socket poc
  ./target/debug/poc snapshot  --socket poc
  ./target/debug/poc reconnect --socket poc --detach-seconds 3
  ./target/debug/poc leak      --socket poc
'
```

`leak --inject-bug` is expected to **fail**: it injects the bug the check exists to
detect, and the same run reports that the detector fired. Use it to convince
yourself the check is not vacuous.

## Recording new protocol fixtures

```bash
tools/container/run.sh python3 tools/probe/probe_control_mode.py \
    crates/tmux-protocol/tests/fixtures
```

The script records raw bytes plus a `manifest.json` (tmux version, timestamp,
command sequence), and the runner streams the directory back into the repository.
Commit both: the fixtures are the evidence behind `docs/spec/control-mode.md`.