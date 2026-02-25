#!/usr/bin/env bash
set -euo pipefail
ROOT="/Users/rachmataditiya/.openclaw/workspace"
ENGINE_DIR="$ROOT/skills/crypto-trading-lab/engine"
export OPENCLAW_WORKSPACE="$ROOT"
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:$PATH"

CARGO_BIN="$(command -v cargo || true)"
if [[ -z "$CARGO_BIN" && -x "$HOME/.cargo/bin/cargo" ]]; then
  CARGO_BIN="$HOME/.cargo/bin/cargo"
fi
if [[ -z "$CARGO_BIN" ]]; then
  echo "[realtime_daemon] ERROR: cargo not found"
  exit 127
fi

cd "$ENGINE_DIR"
"$CARGO_BIN" run --release -- realtime-daemon >> /tmp/realtime-engine.out.log 2>> /tmp/realtime-engine.err.log
