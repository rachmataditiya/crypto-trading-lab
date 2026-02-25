#!/usr/bin/env bash
set -euo pipefail
ROOT="/Users/rachmataditiya/.openclaw/workspace"

bash "$ROOT/skills/crypto-trading-lab/scripts/run_scan.sh" >/tmp/crypto_runscan.log 2>&1
python3 "$ROOT/skills/crypto-trading-lab/scripts/make_signal_report.py" >/tmp/crypto_signal_path.txt
python3 "$ROOT/skills/crypto-trading-lab/scripts/backtest_v3.py" >/tmp/crypto_backtest_path.txt

SIG=$(cat /tmp/crypto_signal_path.txt)
BT=$(cat /tmp/crypto_backtest_path.txt)

echo "pro_scan_ok"
echo "signal_file=$SIG"
echo "backtest_file=$BT"
sed -n '1,60p' "$SIG"
echo "---"
sed -n '1,40p' "$BT"
