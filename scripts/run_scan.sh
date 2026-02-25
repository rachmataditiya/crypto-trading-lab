#!/usr/bin/env bash
set -euo pipefail

ROOT="/Users/rachmataditiya/.openclaw/workspace"
TOOL_DIR="$ROOT/tools/crypto-scan-rs"
OUT="$ROOT/tmp/crypto_scan_latest.txt"

cd "$TOOL_DIR"
cargo run --release >/tmp/crypto_scan_run.log 2>&1 || {
  echo "scan_failed"
  tail -n 80 /tmp/crypto_scan_run.log || true
  exit 1
}

if [[ ! -f "$OUT" ]]; then
  echo "output_missing: $OUT"
  exit 1
fi

echo "scan_ok"
stat -f 'updated_at=%Sm' -t '%Y-%m-%d %H:%M:%S %Z' "$OUT"
head -n 60 "$OUT"
