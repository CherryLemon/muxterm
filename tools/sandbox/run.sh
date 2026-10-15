#!/usr/bin/env bash
# Run a command inside the namespace sandbox (see enter.sh for what that means).
#
# Usage: tools/sandbox/run.sh cargo test --workspace
#        tools/sandbox/run.sh bash
#
# The rootfs lives in .sandbox/rootfs (git-ignored) and is provisioned once by
# tools/sandbox/setup-rootfs.sh.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ROOTFS="${MUXTERM_SANDBOX_ROOTFS:-$ROOT/.sandbox/rootfs}"

# NOTE: absolute symlinks inside a rootfs (Alpine's /bin/sh -> /bin/busybox) do not
# resolve from outside the chroot, so test for the real binaries instead.
if [ ! -e "$ROOTFS/bin/busybox" ] && [ ! -e "$ROOTFS/bin/bash" ] && [ ! -e "$ROOTFS/bin/dash" ]; then
  echo "sandbox rootfs missing at $ROOTFS" >&2
  echo "run: tools/sandbox/setup-rootfs.sh (it prints how to fetch one)" >&2
  exit 2
fi

command="$*"
if [ "$#" -eq 0 ]; then
  command="bash"
fi

exec unshare --propagation unchanged --mount --pid --uts --ipc --fork \
  "$ROOT/tools/sandbox/enter.sh" "$ROOTFS" "$ROOT" "$command"