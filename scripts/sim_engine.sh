#!/usr/bin/env bash
# sim_engine.sh — Wrapper for the Rust paper trading simulation engine.
# Usage:
#   sim_engine.sh once
#   sim_engine.sh daemon [interval_minutes]
#   sim_engine.sh backfill [hours]

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ENGINE_DIR="$(cd "$SCRIPT_DIR/../engine" && pwd)"
WORKSPACE="$(cd "$SCRIPT_DIR/../../.." && pwd)"

export OPENCLAW_WORKSPACE="$WORKSPACE"
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:$PATH"

CARGO_BIN="$(command -v cargo || true)"
if [[ -z "$CARGO_BIN" && -x "$HOME/.cargo/bin/cargo" ]]; then
  CARGO_BIN="$HOME/.cargo/bin/cargo"
fi
if [[ -z "$CARGO_BIN" ]]; then
  echo "[sim_engine.sh] ERROR: cargo not found in PATH"
  exit 127
fi

MODE="${1:-once}"
shift || true

cd "$ENGINE_DIR"

case "$MODE" in
    once)
        "$CARGO_BIN" run --release -- once
        ;;
    daemon)
        INTERVAL="${1:-60}"
        echo "[sim_engine.sh] Starting daemon, interval=${INTERVAL}m"
        "$CARGO_BIN" run --release -- daemon --interval-minutes "$INTERVAL"
        ;;
    backfill)
        HOURS="${1:-6}"
        echo "[sim_engine.sh] Backfilling ${HOURS} hours"
        "$CARGO_BIN" run --release -- backfill --hours "$HOURS"
        ;;
    *)
        echo "Usage: sim_engine.sh {once|daemon [minutes]|backfill [hours]}"
        exit 1
        ;;
esac
