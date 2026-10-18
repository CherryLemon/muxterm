#!/usr/bin/env bash
# End-to-end PoC scenarios.  These need a real tmux with real panes, so they run
# inside the container, against a private socket.
#
# Usage: tools/container/e2e.sh [socket-name]
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SOCKET="${1:-poc-e2e}"

SCRIPT=$(cat <<EOS
set -euo pipefail
cargo build --locked -p poc-cli
POC=./target/debug/poc

echo "== doctor =="
\$POC doctor --socket ${SOCKET}

echo "== create =="
\$POC create --socket ${SOCKET}

echo "== snapshot =="
\$POC snapshot --socket ${SOCKET}

echo "== stream =="
\$POC stream --socket ${SOCKET} --seconds 3

echo "== resize =="
\$POC resize --socket ${SOCKET}

echo "== mouse =="
\$POC mouse --socket ${SOCKET}

echo "== modes =="
\$POC modes --socket ${SOCKET}

echo "== reconnect =="
\$POC reconnect --socket ${SOCKET} --detach-seconds 3

echo "== core-gate =="
\$POC core-gate --socket ${SOCKET}

echo "== leak (must pass) =="
\$POC leak --socket ${SOCKET}

echo "== leak --inject-bug (must fail: it injects the bug the check detects) =="
if \$POC leak --socket ${SOCKET} --inject-bug; then
  echo "FAIL: the leak detector did not fire on an injected leak" >&2
  exit 1
else
  echo "OK: the leak detector fired on the injected leak"
fi

echo "ALL E2E SCENARIOS PASSED"
EOS
)

exec "$ROOT/tools/container/run.sh" bash -lc "$SCRIPT"