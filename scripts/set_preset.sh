#!/usr/bin/env bash
set -euo pipefail

ROOT="/Users/rachmataditiya/.openclaw/workspace"
CFG="$ROOT/data/trading-journal/account_config.json"
PRESET="${1:-}"

if [[ -z "$PRESET" ]]; then
  echo "Usage: set_preset.sh {conservative|balanced|aggressive}"
  exit 1
fi

case "$PRESET" in
  conservative|balanced|aggressive) ;;
  *)
    echo "Invalid preset: $PRESET"
    echo "Use: conservative | balanced | aggressive"
    exit 1
    ;;
esac

python3 - <<PY
import json
p = "$CFG"
with open(p) as f:
    data = json.load(f)
data["preset"] = "$PRESET"
with open(p, "w") as f:
    json.dump(data, f, indent=2)
print(f"preset set to: {data['preset']}")
PY

echo "Done. Next cycle will use preset=$PRESET"
