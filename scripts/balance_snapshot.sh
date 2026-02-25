#!/usr/bin/env bash
# balance_snapshot.sh — fetch OKX equity and save to daily_balance.json
# Returns JSON: {"today": "YYYY-MM-DD", "opening": 116.15, "yesterday_opening": 115.90, "pnl": 0.25}

set -euo pipefail
BASE="/Users/rachmataditiya/.openclaw/workspace"
CFG="$BASE/data/trading-journal/okx_config.json"
BAL="$BASE/data/trading-journal/daily_balance.json"

python3 - <<PY
import json, datetime, pathlib, sys
cfg  = json.load(open("$CFG"))
bal_file = pathlib.Path("$BAL")
bal  = json.loads(bal_file.read_text()) if bal_file.exists() else {}
today     = str(datetime.date.today())
yesterday = str(datetime.date.today() - datetime.timedelta(days=1))

try:
    import okx.Account as AM
    api    = AM.AccountAPI(cfg['api_key'], cfg['secret_key'], cfg['passphrase'], False, '0')
    r      = api.get_account_balance()
    equity = float(r['data'][0]['totalEq']) if r.get('code') == '0' else None
except Exception as e:
    equity = None
    print(f"OKX error: {e}", file=sys.stderr)

if equity:
    bal[today] = {'opening': round(equity, 4), 'date': today}
    bal_file.write_text(json.dumps(bal, indent=2))

yest_opening = bal.get(yesterday, {}).get('opening')
pnl = round(equity - yest_opening, 4) if equity and yest_opening else None

out = {
    "today": today,
    "opening": round(equity, 4) if equity else None,
    "yesterday_opening": yest_opening,
    "pnl_vs_yesterday": pnl,
    "pnl_pct": round(pnl / yest_opening * 100, 2) if pnl and yest_opening else None
}
print(json.dumps(out))
PY
