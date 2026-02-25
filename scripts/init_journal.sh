#!/usr/bin/env bash
set -euo pipefail

ROOT="/Users/rachmataditiya/.openclaw/workspace"
DIR="$ROOT/data/trading-journal"
DATE="$(date +%F)"
OUT="$DIR/$DATE.md"
mkdir -p "$DIR"

if [[ -f "$OUT" ]]; then
  echo "$OUT"
  exit 0
fi

cat > "$OUT" <<'MD'
# Trading Journal - DATE_PLACEHOLDER

## Market Regime
- [ ] Risk-on
- [ ] Mixed
- [ ] Risk-off
- Notes:

## Watchlist (Top 3)
1. 
2. 
3. 

## Account Setup
- Initial capital (paper): 1000 USDT
- Current equity:

## Trade Plan
- Max risk/trade: 1%-2%
- Max open positions: 3
- No-trade conditions:

## Executed Trades
| Time | Asset | Setup | Entry | Stop | TP | Size | Rationale | Outcome |
|---|---|---|---:|---:|---:|---:|---|---|

## Mistakes / Rule Violations
-

## End of Day Review
- Best decision:
- Worst decision:
- Improvement tomorrow:
- PnL summary:
MD

# replace placeholder safely on macOS
/usr/bin/sed -i '' "s/DATE_PLACEHOLDER/$DATE/g" "$OUT"
echo "$OUT"
