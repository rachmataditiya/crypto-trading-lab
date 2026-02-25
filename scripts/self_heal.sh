#!/usr/bin/env bash
# self_heal.sh — pure shell, no LLM, no token burn
# Runs every 10 min via LaunchAgent. Restarts engines only if truly stale.

BASE="/Users/rachmataditiya/.openclaw/workspace"
CACHE="$BASE/data/trading-journal/realtime_cache.json"
DB="$BASE/data/trading-journal/sim_history.db"
LOG="$BASE/data/trading-journal/self_heal.log"
NOW=$(TZ=Asia/Jakarta date '+%Y-%m-%d %H:%M WIB')
RESTARTED=""

# ── 1. Realtime engine: compute REAL age from generated_at timestamp ──────────
if [ -f "$CACHE" ]; then
    AGE=$(python3 - <<'PYEOF'
import json, datetime
try:
    d = json.load(open("/Users/rachmataditiya/.openclaw/workspace/data/trading-journal/realtime_cache.json"))
    ts = d.get('generated_at') or d.get('health', {}).get('last_update_at')
    if ts:
        gen = datetime.datetime.fromisoformat(ts)
        if gen.tzinfo is None:
            gen = gen.replace(tzinfo=datetime.timezone.utc)
        age = int((datetime.datetime.now(datetime.timezone.utc) - gen).total_seconds())
        print(max(age, 0))
    else:
        print(9999)
except Exception:
    print(9999)
PYEOF
)
    STALE=$(python3 -c "
import json
try:
    d=json.load(open('/Users/rachmataditiya/.openclaw/workspace/data/trading-journal/realtime_cache.json'))
    print(str(d.get('health',{}).get('stale_feed',True)).lower())
except:
    print('true')
" 2>/dev/null || echo true)
else
    AGE=9999; STALE=true
fi

if [ "$AGE" -gt 180 ] || [ "$STALE" = "true" ]; then
    launchctl kickstart -k gui/"$(id -u)"/ai.openclaw.realtime-engine 2>/dev/null || true
    RESTARTED="$RESTARTED realtime-engine(age=${AGE}s)"
fi

# ── 2. Sim engine: check recent events ───────────────────────────────────────
COUNT=$(sqlite3 "$DB" "SELECT COUNT(*) FROM simulation_events WHERE inserted_at >= datetime('now','-25 minutes');" 2>/dev/null || echo 1)
if [ "$COUNT" -eq 0 ]; then
    launchctl kickstart -k gui/"$(id -u)"/ai.openclaw.sim-engine 2>/dev/null || true
    RESTARTED="$RESTARTED sim-engine(events=0)"
fi

# ── 3. Live executor: check process ──────────────────────────────────────────
if ! pgrep -x "live-executor" > /dev/null 2>&1; then
    launchctl kickstart -k gui/"$(id -u)"/ai.openclaw.live-executor 2>/dev/null || true
    RESTARTED="$RESTARTED live-executor(dead)"
fi

# ── Log + WA alert if something was restarted ────────────────────────────────
if [ -n "$RESTARTED" ]; then
    echo "[$NOW] RESTARTED:$RESTARTED" >> "$LOG"
    /Users/rachmataditiya/.local/bin/openclaw message send \
        --channel whatsapp \
        --target "+6281268289854" \
        --message "🚨 *Self-Heal Alert* [${NOW}]

Engine di-restart otomatis:
${RESTARTED}

Sistem kembali jalan. Cek posisi terbuka jika ada." 2>/dev/null || true
fi
