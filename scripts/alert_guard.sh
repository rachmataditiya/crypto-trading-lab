#!/usr/bin/env bash
# alert_guard.sh — Check DB for new PAPER_TRADE signals, send WA alert
# Runs every 15 min. Zero LLM. Pure data.

BASE="/Users/rachmataditiya/.openclaw/workspace"
DB="$BASE/data/trading-journal/sim_history.db"
LIVE_DB="$BASE/data/trading-journal/live_positions.db"
OPENCLAW=$(which openclaw 2>/dev/null || ls /Users/rachmataditiya/.local/bin/openclaw /usr/local/bin/openclaw 2>/dev/null | head -1)
WA_TARGET="+6281268289854"
NOW=$(TZ=Asia/Jakarta date '+%H:%M WIB')

# ── Find unalerted PAPER_TRADE in last 15 minutes ────────────────────────────
python3 - <<'PY'
import sqlite3, json, subprocess, os, datetime, pathlib

BASE = "/Users/rachmataditiya/.openclaw/workspace"
DB   = f"{BASE}/data/trading-journal/sim_history.db"
LIVE = f"{BASE}/data/trading-journal/live_positions.db"

con  = sqlite3.connect(DB)
live = sqlite3.connect(LIVE)

# Find new PAPER_TRADE not yet alerted, within last 15 min
cutoff = (datetime.datetime.now(datetime.timezone.utc)
          - datetime.timedelta(minutes=15)).isoformat()

rows = con.execute("""
    SELECT id, trade_time, setup_idea, raw_block,
           p_up, confidence, expected_value_r, regime, inserted_at
    FROM simulation_events
    WHERE decision = 'PAPER_TRADE'
    ORDER BY id DESC LIMIT 20
""").fetchall()

# Filter by proper UTC-aware timestamp comparison
row = None
for r in rows:
    ts_str = r[8]  # inserted_at
    try:
        ts = datetime.datetime.fromisoformat(ts_str)
        if ts.tzinfo is None:
            ts = ts.replace(tzinfo=datetime.timezone.utc)
        if ts >= datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(minutes=15):
            row = r[:8]  # drop inserted_at
            break
    except Exception:
        pass

if not row:
    exit(0)

sig_id, t_time, setup, raw_block, p_up, conf, ev, regime = row

# Check if already alerted via tracker file
TRACKER = pathlib.Path(f"{BASE}/data/trading-journal/alerted_signals.json")
alerted = json.loads(TRACKER.read_text()) if TRACKER.exists() else []
if str(sig_id) in alerted:
    exit(0)
raw_block = raw_block or ""

import re
def extract(pattern, text):
    m = re.search(pattern, text)
    return m.group(1) if m else "N/A"

entry  = extract(r'Entry zone:\s*([\d.]+)', raw_block)
stop   = extract(r'Invalidation/stop:\s*([\d.]+)', raw_block)
tp1    = extract(r'TP1/TP2:\s*([\d.]+)', raw_block)
symbol = extract(r'PAPER_TRADE.*?—\s*(\w+USDT)', raw_block)
direc  = extract(r'PAPER_TRADE.*?—\s*\w+\s+(Long|Short)', raw_block)

# Check live_positions execution status
ex_row = live.execute(
    "SELECT status FROM processed_signals WHERE signal_id=? LIMIT 1", (sig_id,)
).fetchone()
status = ex_row[0] if (ex_row and ex_row[0]) else "PENDING"

# Format message
msg = (
    f"🚨 *PAPER TRADE — {t_time} WIB*\n\n"
    f"🪙 {symbol} {direc} | Regime: {regime}\n"
    f"📌 {(setup or '')[:100]}\n\n"
    f"📊 Entry: {entry} | SL: {stop} | TP1: {tp1}\n"
    f"📈 P(up)={p_up:.2f} | Conf={conf:.2f} | EV={ev:.2f}R\n\n"
    f"🤖 Executor: {status}\n"
    f"💰 Mode: LIVE OKX — order NYATA"
)

# Send via openclaw CLI
import shutil
oc = shutil.which("openclaw") or "/Users/rachmataditiya/.local/bin/openclaw"
result = subprocess.run(
    [oc, "message", "send", "--channel", "whatsapp",
     "--target", "+6281268289854", "--message", msg],
    capture_output=True, text=True
)
if result.returncode == 0:
    print(f"[{t_time}] ✅ Alert sent for PAPER_TRADE id={sig_id} {symbol} {entry}→{tp1}")
else:
    print(f"[{t_time}] ⚠️ Send failed: {result.stderr[:200]}")

# Mark as alerted in tracker file
alerted.append(str(sig_id))
# Keep only last 500 entries
alerted = alerted[-500:]
TRACKER.write_text(json.dumps(alerted))
PY
