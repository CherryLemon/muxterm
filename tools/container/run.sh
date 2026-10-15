#!/usr/bin/env bash
# Run a command inside the isolated test environment.
#
# Two runners exist and this script picks one:
#
#   1. Docker (preferred when the daemon is healthy): tools/container/docker-run.sh
#   2. Namespace sandbox (fallback): tools/sandbox/run.sh
#
# Both give the same practical isolation: only this repository is visible, /tmp is
# private, tmux servers live and die with the run, and no host process can be
# reached or signalled.  See docs/dev-environment.md.
#
# Force a runner with MUXTERM_RUNNER=docker|sandbox.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUNNER="${MUXTERM_RUNNER:-auto}"

docker_usable() {
  command -v docker >/dev/null 2>&1 || return 1
  docker info >/dev/null 2>&1 || return 1
  # A daemon that cannot create layers (full storage) reports ENOSPC here.
  docker run --rm hello-world >/dev/null 2>&1
}

case "$RUNNER" in
  docker)
    exec "$ROOT/tools/container/docker-run.sh" "$@"
    ;;
  sandbox)
    exec "$ROOT/tools/sandbox/run.sh" "$@"
    ;;
  auto)
    if docker_usable; then
      exec "$ROOT/tools/container/docker-run.sh" "$@"
    fi
    echo "[run] docker is unusable here; using the namespace sandbox" >&2
    exec "$ROOT/tools/sandbox/run.sh" "$@"
    ;;
  *)
    echo "MUXTERM_RUNNER must be docker, sandbox or auto" >&2
    exit 2
    ;;
esac