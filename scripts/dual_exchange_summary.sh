#!/usr/bin/env bash
# dual_exchange_summary.sh — 4-hour professional trading summary (OKX + Binance)
# Output: Telegram notification via openclaw

DB="/Users/rachmataditiya/.openclaw/workspace/data/trading-journal/live_positions.db"
CACHE="/Users/rachmataditiya/.openclaw/workspace/data/trading-journal/realtime_cache.json"
OKX_CFG="/Users/rachmataditiya/.openclaw/workspace/data/trading-journal/okx_config.json"
BNB_CFG="/Users/rachmataditiya/.openclaw/workspace/data/trading-journal/binance_config.json"

python3 - <<'PYEOF'
import json, sqlite3, datetime, hmac, hashlib, urllib.request, urllib.error, time, os, sys

DB       = "/Users/rachmataditiya/.openclaw/workspace/data/trading-journal/live_positions.db"
CACHE    = "/Users/rachmataditiya/.openclaw/workspace/data/trading-journal/realtime_cache.json"
OKX_CFG  = "/Users/rachmataditiya/.openclaw/workspace/data/trading-journal/okx_config.json"
BNB_CFG  = "/Users/rachmataditiya/.openclaw/workspace/data/trading-journal/binance_config.json"

NOW_WIB  = datetime.datetime.now(datetime.timezone(datetime.timedelta(hours=7)))
TODAY    = NOW_WIB.strftime("%Y-%m-%d")
HOUR     = NOW_WIB.hour

# ── Helpers ───────────────────────────────────────────────────────────────────
def okx_get(key, secret, passphrase, path):
    import base64, hmac as hm, hashlib as hs
    ts = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.000Z")
    msg = f"{ts}GET{path}"
    sig = base64.b64encode(hm.new(secret.encode(), msg.encode(), hs.sha256).digest()).decode()
    req = urllib.request.Request(
        f"https://www.okx.com{path}",
        headers={"OK-ACCESS-KEY": key, "OK-ACCESS-SIGN": sig,
                 "OK-ACCESS-TIMESTAMP": ts, "OK-ACCESS-PASSPHRASE": passphrase,
                 "Content-Type": "application/json"})
    try:
        return json.loads(urllib.request.urlopen(req, timeout=8).read())
    except Exception:
        return {}

def bnb_get(key, secret, path, params=""):
    ts = int(time.time()*1000)
    q = f"{params}&recvWindow=10000&timestamp={ts}" if params else f"recvWindow=10000&timestamp={ts}"
    sig = hmac.new(secret.encode(), q.encode(), hashlib.sha256).hexdigest()
    url = f"https://api.binance.com{path}?{q}&signature={sig}"
    req = urllib.request.Request(url, headers={"X-MBX-APIKEY": key})
    try:
        return json.loads(urllib.request.urlopen(req, timeout=8).read())
    except Exception:
        return {}

# ── Load configs ─────────────────────────────────────────────────────────────
try:
    okx = json.load(open(OKX_CFG))
    okx_key, okx_secret, okx_pass = okx['api_key'], okx['secret_key'], okx['passphrase']
except:
    okx_key = okx_secret = okx_pass = ""

try:
    bnb = json.load(open(BNB_CFG))
    bnb_key, bnb_secret = bnb['api_key'], bnb['secret_key']
except:
    bnb_key = bnb_secret = ""

# ── DB Stats ──────────────────────────────────────────────────────────────────
conn = sqlite3.connect(DB)
cur  = conn.cursor()

# Today's stats
cur.execute("SELECT COALESCE(SUM(realized_pnl),0), COALESCE(SUM(trade_count),0), COALESCE(MAX(consecutive_losses),0) FROM daily_stats WHERE trade_date=?", (TODAY,))
row = cur.fetchone(); pnl_today, trades_today, losses_today = (row[0] or 0), (row[1] or 0), (row[2] or 0)

# All-time
cur.execute("SELECT COALESCE(SUM(realized_pnl),0), COALESCE(SUM(trade_count),0) FROM daily_stats")
row2 = cur.fetchone(); pnl_total, trades_total = (row2[0] or 0), (row2[1] or 0)

# Today's closed trades
cur.execute("""
    SELECT symbol, COALESCE(ams_tier,'?'), status, entry_price, entry_price as exit_px,
           risk_usdt, opened_at, COALESCE(closed_at,'')
    FROM open_positions 
    WHERE status != 'OPEN' AND date(closed_at) = ?
    ORDER BY closed_at DESC
""", (TODAY,))
closed_today = cur.fetchall()

# Open positions
cur.execute("""
    SELECT symbol, ams_tier, entry_price, stop_price, tp1_price, qty, risk_usdt, opened_at, high_water
    FROM open_positions WHERE status='OPEN'
""")
open_pos = cur.fetchall()

# Best/worst trade this week - estimate pnl from price diff * qty
cur.execute("""
    SELECT symbol, (tp1_price - entry_price) * qty as est_pnl, status
    FROM open_positions 
    WHERE status IN ('CLOSED_TP','CLOSED_TP2','CLOSED_TRAIL','CLOSED_RSI_EXIT')
    AND date(closed_at) >= date('now', '-7 days')
    ORDER BY est_pnl DESC LIMIT 1
""")
best = cur.fetchone()

cur.execute("""
    SELECT symbol, (stop_price - entry_price) * qty as est_pnl, status
    FROM open_positions 
    WHERE status = 'CLOSED_SL'
    AND date(closed_at) >= date('now', '-7 days')
    ORDER BY est_pnl ASC LIMIT 1
""")
worst = cur.fetchone()

# Win rate last 7 days
cur.execute("""
    SELECT 
        COUNT(CASE WHEN status IN ('CLOSED_TP','CLOSED_TP2','CLOSED_TRAIL','CLOSED_RSI_EXIT') THEN 1 END) as wins,
        COUNT(*) as total
    FROM open_positions 
    WHERE status != 'OPEN' 
    AND date(closed_at) >= date('now', '-7 days')
""")
wins7, total7 = cur.fetchone()
winrate7 = (wins7/total7*100) if total7 > 0 else 0

# Lesson learned from recent closed trades
cur.execute("""
    SELECT status, symbol, ams_tier
    FROM open_positions 
    WHERE status != 'OPEN' AND date(closed_at) = ?
    ORDER BY closed_at DESC LIMIT 5
""", (TODAY,))
recent_exits = cur.fetchall()

conn.close()

# ── Live Equity ───────────────────────────────────────────────────────────────
okx_equity = 0.0
if okx_key:
    r = okx_get(okx_key, okx_secret, okx_pass, "/api/v5/account/balance")
    try:
        okx_equity = float(r["data"][0]["totalEq"])
    except:
        okx_equity = 0.0
# Fallback: read OKX balance from executor log (last known equity from DB stats)
if okx_equity == 0.0:
    try:
        conn2 = sqlite3.connect(DB)
        row = conn2.execute("SELECT starting_equity + realized_pnl FROM daily_stats ORDER BY trade_date DESC LIMIT 1").fetchone()
        conn2.close()
        if row: okx_equity = round(float(row[0]), 2)
    except: pass

bnb_equity = 0.0
if bnb_key:
    r = bnb_get(bnb_key, bnb_secret, "/api/v3/account")
    try:
        for b in r.get("balances", []):
            if b["asset"] == "USDT":
                bnb_equity = float(b["free"]) + float(b["locked"])
    except:
        bnb_equity = 0.0

total_equity = okx_equity + bnb_equity

# ── Market Pulse ──────────────────────────────────────────────────────────────
try:
    cache = json.load(open(CACHE))
    btc   = cache["symbols"].get("BTCUSDT", {})
    btc_price = btc.get("price", 0)
    btc_ret24 = btc.get("ret_24h", 0)
    btc_rsi   = btc.get("rsi14", 0)
    btc_rho   = btc.get("ams_regime_rho", 0)
    # Best signal candidates
    candidates = []
    for sym, v in cache["symbols"].items():
        tier = v.get("hmtf_tier", "C")
        rsi  = v.get("rsi14", 50)
        rho  = v.get("ams_regime_rho", 0)
        score = v.get("hmtf_entry_score", 0)
        vol  = v.get("volume_ratio", 0)
        if tier in ("A","B","R","V","F") or rsi < 35:
            candidates.append((sym, tier, rsi, rho, score, vol, v.get("price",0), v.get("ret_24h",0)))
    candidates.sort(key=lambda x: -x[4])
except:
    btc_price = btc_ret24 = btc_rsi = btc_rho = 0
    candidates = []

# ── Lesson Learned ────────────────────────────────────────────────────────────
exit_counts = {}
for status, sym, tier in recent_exits:
    exit_counts[status] = exit_counts.get(status, 0) + 1

lesson_parts = []
if exit_counts.get("CLOSED_TIME", 0) >= 2:
    lesson_parts.append("⏰ CLOSED_TIME dominan hari ini — mean reversion butuh waktu lebih. Tier R hold-time mungkin masih kurang.")
if exit_counts.get("CLOSED_TP", 0) > 0 or exit_counts.get("CLOSED_TP2", 0) > 0:
    lesson_parts.append("🎯 TP hit — OCO order bekerja sempurna, profit terkunci server-side.")
if exit_counts.get("CLOSED_RSI_EXIT", 0) > 0:
    lesson_parts.append("📈 RSI Exit aktif — reversion selesai sebelum price target, optimal.")
if exit_counts.get("CLOSED_SL", 0) >= 2:
    lesson_parts.append("🔴 2+ SL hari ini — evaluasi: apakah entry terlalu agresif atau pasar choppy?")
if not lesson_parts:
    lesson_parts.append("📋 Tidak ada trade signifikan hari ini — pasar belum kasih setup ideal.")

# ── Next Plan ─────────────────────────────────────────────────────────────────
next_plans = []
if btc_ret24 < -2:
    next_plans.append("⚠️ BTC turun >2% — BTC macro filter aktif, kemungkinan besar SKIP signal baru.")
elif btc_rsi > 70:
    next_plans.append("📊 BTC RSI overbought — pasar mungkin akan konsolidasi, Tier R lebih menarik dari Tier A.")
elif btc_rsi < 40:
    next_plans.append("🎯 BTC RSI oversold — watchlist Tier R akan aktif, siap entry mean reversion.")
else:
    next_plans.append("📊 Market neutral — tunggu sinyal AMS Tier A/B atau Tier R RSI <45.")

if candidates:
    top = candidates[0]
    next_plans.append(f"👀 Pantau {top[0]} — Tier {top[1]}, RSI={top[2]:.0f}, ρ={top[3]:.2f}, Vol={top[5]:.1f}x")
else:
    next_plans.append("💤 Tidak ada kandidat kuat saat ini — semua RSI di zona tinggi (>55).")

# ── Period label ──────────────────────────────────────────────────────────────
if 0 <= HOUR < 4:
    period = "🌙 Midnight"
elif 4 <= HOUR < 8:
    period = "🌅 Dawn"
elif 8 <= HOUR < 12:
    period = "🌞 Morning"
elif 12 <= HOUR < 16:
    period = "☀️ Afternoon"
elif 16 <= HOUR < 20:
    period = "🌇 Evening"
else:
    period = "🌆 Night"

# ── Format report ─────────────────────────────────────────────────────────────
lines = []
lines.append(f"⚡ *Trading Report — {period}*")
lines.append(f"📅 {NOW_WIB.strftime('%d %b %Y, %H:%M')} WIB\n")

lines.append("━━━━━━━━━━━━━━━━━━")
lines.append("💼 *PORTFOLIO*")
lines.append(f"OKX   : ${okx_equity:.2f} USDT")
lines.append(f"Binance: ${bnb_equity:.2f} USDT")
lines.append(f"Total  : *${total_equity:.2f} USDT*")
pnl_emoji = "🟢" if pnl_today >= 0 else "🔴"
lines.append(f"PnL Hari Ini: {pnl_emoji} ${pnl_today:+.2f} | {trades_today} trade(s)")
lines.append(f"PnL All-Time: ${pnl_total:+.2f} ({trades_total} total trades)")

if open_pos:
    lines.append("\n📌 *POSISI TERBUKA*")
    for sym, tier, entry, sl, tp1, qty, risk, opened_at, hw in open_pos:
        try:
            opened = datetime.datetime.fromisoformat(opened_at.replace("Z", "+00:00"))
            hold_min = int((datetime.datetime.now(datetime.timezone.utc) - opened).total_seconds() / 60)
        except:
            hold_min = 0
        pct_to_tp = ((tp1-entry)/entry*100) if entry > 0 else 0
        pct_to_sl = ((sl-entry)/entry*100) if entry > 0 else 0
        hw_str = f" HW={hw:.3f}" if hw and hw > entry else ""
        lines.append(f"• {sym} [T{tier}] entry={entry:.4f} | TP={tp1:.4f}({pct_to_tp:+.1f}%) | SL={sl:.4f}({pct_to_sl:+.1f}%) | {hold_min}min{hw_str}")
else:
    lines.append("\n📌 *POSISI*: Cash 100%")

if closed_today:
    lines.append("\n📋 *CLOSED HARI INI*")
    for sym, tier, status, entry, exit_px, risk, opened_at, closed_at in closed_today[:5]:
        pnl_est = (exit_px - entry) * (risk / abs(entry - entry*0.98)) if entry > 0 else 0
        status_short = status.replace("CLOSED_","").replace("_"," ")
        lines.append(f"• {sym} [{status_short}] entry={entry:.4f}→{exit_px:.4f}")

lines.append("\n━━━━━━━━━━━━━━━━━━")
lines.append("📡 *MARKET PULSE*")
btc_regime = "TRENDING" if btc_rho > 0.6 else "RANGING" if btc_rho < 0.4 else "MIXED"
btc_bias = "🟢" if btc_ret24 > 0 else "🔴"
lines.append(f"BTC: ${btc_price:,.0f} {btc_bias}{btc_ret24:+.1f}% | RSI={btc_rsi:.0f} | ρ={btc_rho:.2f} ({btc_regime})")

if candidates:
    lines.append("🔍 *Kandidat Signal:*")
    for sym, tier, rsi, rho, score, vol, price, ret24 in candidates[:3]:
        label = "🔴" if tier in ("A","B") else "🟡" if tier == "R" else "⚪"
        lines.append(f"  {label} {sym} T{tier} RSI={rsi:.0f} ρ={rho:.2f} Vol={vol:.1f}x {ret24:+.1f}%")
else:
    lines.append("🔍 Tidak ada kandidat aktif — semua RSI tinggi")

lines.append("\n━━━━━━━━━━━━━━━━━━")
lines.append("📚 *LESSON LEARNED*")
for l in lesson_parts:
    lines.append(l)

lines.append("\n🎯 *NEXT PLAN*")
for p in next_plans:
    lines.append(p)

lines.append("\n━━━━━━━━━━━━━━━━━━")
lines.append(f"7d Win Rate: {winrate7:.0f}% ({wins7}/{total7} trades)")
if best:
    lines.append(f"Best trade (7d): {best[0]} ${best[1]:+.2f} [{best[2].replace('CLOSED_','')}]")
if worst:
    lines.append(f"Worst trade (7d): {worst[0]} ${worst[1]:+.2f} [{worst[2].replace('CLOSED_','')}]")

print("\n".join(lines))
PYEOF
