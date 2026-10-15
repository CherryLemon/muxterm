#!/usr/bin/env bash
# Full verification run.  Everything happens inside the container.
#
# Usage: tools/container/test.sh [--quick]
#   --quick   skip clippy (keeps the edit/test loop fast)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
QUICK=0
for arg in "$@"; do
  case "$arg" in
    --quick) QUICK=1 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

SCRIPT=$(cat <<'EOS'
set -euo pipefail
echo "== toolchain =="
rustc --version
cargo --version
tmux -V
python3 --version

echo "== fmt =="
cargo fmt --all -- --check

if [ "${QUICK}" != "1" ]; then
  echo "== clippy =="
  cargo clippy --workspace --all-targets -- -D warnings
fi

echo "== tests =="
cargo test --workspace --all-targets

echo "== fixtures =="
if [ -f crates/tmux-protocol/tests/fixtures/manifest.json ]; then
  python3 - <<'PY'
import json
with open("crates/tmux-protocol/tests/fixtures/manifest.json") as fh:
    manifest = json.load(fh)
print("fixtures recorded from:", manifest["tmux_version"], "at", manifest["captured_at_utc"])
if manifest.get("failures"):
    raise SystemExit(f"probe failures recorded: {manifest['failures']}")
PY
else
  echo "no fixtures recorded yet; run tools/container/run.sh python3 tools/probe/probe_control_mode.py crates/tmux-protocol/tests/fixtures"
fi
EOS
)

exec "$ROOT/tools/container/run.sh" env QUICK="$QUICK" bash -lc "$SCRIPT"