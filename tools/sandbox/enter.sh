#!/usr/bin/env bash
# Enter the namespace sandbox: a private mount/PID/UTS/IPC namespace with a
# chroot into a rootfs that lives inside this repository.
#
# This is the fallback runner used when the Docker daemon is unusable (for
# example when its storage is full).  It provides the same practical isolation
# the project needs:
#
#   * the test process cannot see or signal the developer's processes (PID ns)
#   * it cannot reach the developer's tmux server (private /tmp, private socket)
#   * it cannot mount over or write outside the repository (chroot + bind mount)
#   * /proc, /dev and /dev/pts are fresh mounts inside the namespace
#
# Usage (from tools/container/run.sh, which selects the runner):
#   tools/sandbox/enter.sh <rootfs> <workdir> <command...>
#
# NOTE: creating a PTY (tmux panes) requires /dev/ptmx access, which the outer
# file sandbox may deny; run the caller with full access when that is the case.
set -euo pipefail

ROOTFS="$1"; shift
WORK="$1"; shift

# Inside the new namespaces:
mkdir -p "$ROOTFS/work" "$ROOTFS/proc" "$ROOTFS/dev" "$ROOTFS/dev/pts" "$ROOTFS/tmp" "$ROOTFS/sys"

mount --bind "$WORK" "$ROOTFS/work"
mount -t proc proc "$ROOTFS/proc"
mount --rbind /dev "$ROOTFS/dev"
mount --make-rslave "$ROOTFS/dev" 2>/dev/null || true
mount -t tmpfs -o mode=1777 tmpfs "$ROOTFS/tmp"
mount -t tmpfs -o mode=755 tmpfs "$ROOTFS/sys"

export MUXTERM_IN_SANDBOX=1
# bash once provisioned; busybox ash during bootstrap
SHELL_BIN=/bin/bash
[ -e "$ROOTFS/bin/bash" ] || SHELL_BIN=/bin/sh

exec chroot "$ROOTFS" /usr/bin/env -i \
  PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
  HOME=/root \
  TERM="${TERM:-xterm-256color}" \
  LANG=C.UTF-8 \
  LC_ALL=C.UTF-8 \
  TMUX_TMPDIR=/tmp/tmux-runtime \
  CARGO_HOME=/usr/local/cargo \
  RUSTUP_HOME=/usr/local/rustup \
  RUST_BACKTRACE="${RUST_BACKTRACE:-1}" \
  CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}" \
  MUXTERM_IN_SANDBOX=1 \
  http_proxy="${http_proxy:-}" https_proxy="${https_proxy:-}" \
  HTTP_PROXY="${HTTP_PROXY:-}" HTTPS_PROXY="${HTTPS_PROXY:-}" \
  no_proxy="${no_proxy:-localhost,127.0.0.1,::1}" NO_PROXY="${NO_PROXY:-localhost,127.0.0.1,::1}" \
  "$SHELL_BIN" -lc "$*"