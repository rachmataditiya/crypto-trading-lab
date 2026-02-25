#!/usr/bin/env bash
# journal_write.sh — Write 15-min journal entries from DB data. Zero LLM.
# Usage: journal_write.sh [aggressive|predictive|both]

BASE="/Users/rachmataditiya/.openclaw/workspace"
MODE="${1:-both}"

python3 - "$BASE" "$MODE" <<'PY'
import sys, sqlite3, json, re, pathlib, datetime

BASE  = sys.argv[1]
MODE  = sys.argv[2]
DB    = f"{BASE}/data/trading-journal/sim_history.db"
LIVE  = f"{BASE}/data/trading-journal/live_positions.db"

TZ_OFFSET = datetime.timezone(datetime.timedelta(hours=7))
now     = datetime.datetime.now(TZ_OFFSET)
now_str = now.strftime("%H:%M")
today   = now.strftime("%Y-%m-%d")

con  = sqlite3.connect(DB)
live = sqlite3.connect(LIVE)

# ── Pull latest sim event ─────────────────────────────────────────────────────
row = con.execute("""
    SELECT id, trade_time, regime, decision, setup_idea,
           p_up, p_down, confidence, expected_value_r,
           news_sentiment, news_catalyst_score, raw_block
    FROM simulation_events
    ORDER BY id DESC LIMIT 1
""").fetchone()

if not row:
    print("No sim events"); sys.exit(1)

(sig_id, t_time, regime, decision, setup,
 p_up, p_down, conf, ev,
 news_sent, news_cat, raw) = row
raw = raw or ""

def extract(pattern, text, default="N/A"):
    m = re.search(pattern, text)
    return m.group(1) if m else default

entry   = extract(r'Entry zone:\s*([\d.]+)', raw)
stop    = extract(r'Invalidation/stop:\s*([\d.]+)', raw)
tp1     = extract(r'TP1/TP2:\s*([\d.]+)', raw)
tp2_m   = re.search(r'TP1/TP2:[^\n]*/\s*([\d.]+)', raw)
tp2     = tp2_m.group(1) if tp2_m else "N/A"
symbol  = extract(r'PAPER_TRADE.*?—\s*(\w+USDT)', raw, "—")
direc   = extract(r'PAPER_TRADE.*?—\s*\w+\s+(Long|Short)', raw, "—")

# Top watchlist lines
top3_lines = []
for line in raw.split("\n"):
    m = re.search(r'\*\*(\w+USDT)\*\*.*Score\s*\w\(([^)]+)\).*24h\s*([\d.+\-]+%)', line)
    if m:
        top3_lines.append(f"- {m.group(1)}: score={m.group(2)} 24h={m.group(3)}")
    if len(top3_lines) >= 3:
        break
top3 = "\n".join(top3_lines) if top3_lines else "- (no watchlist data)"

# Equity / risk
eq_row = live.execute(
    "SELECT starting_equity + COALESCE(realized_pnl,0) FROM daily_stats WHERE trade_date=? LIMIT 1",
    (today,)
).fetchone()
equity = round(float(eq_row[0]), 2) if eq_row else 116.15
risk_usdt = round(equity * 0.02, 2)
r1 = risk_usdt
r2 = round(risk_usdt * 2, 2)

# Lesson based on decision
if decision == "PAPER_TRADE":
    dec_icon = "🟢 TRADE"
    lesson = (
        f"1. Signal kualitas tinggi — momentum gate passed.\n"
        f"2. Risk 2% = {risk_usdt} USDT dikunci, EV={ev:.2f}R.\n"
        f"3. Monitor TP1 ketat; trail stop bila momentum lanjut.\n"
        f"4. Jika SL hit — evaluasi volume konfirmasi saat entry."
    )
else:
    dec_icon = "🔴 NO_TRADE"
    lesson = (
        f"1. Tidak ada setup kualitas — filter bekerja benar.\n"
        f"2. Pasar {regime} → disiplin lebih penting dari frekuensi.\n"
        f"3. Modal {equity} USDT terlindungi — tidak masuk sembarangan.\n"
        f"4. Tunggu breakout + volume konfirmasi sebelum entry."
    )

p_up_s   = f"{p_up:.3f}"   if p_up   is not None else "N/A"
p_down_s = f"{p_down:.3f}" if p_down is not None else "N/A"
conf_s   = f"{conf:.3f}"   if conf   is not None else "N/A"
ev_s     = f"{ev:.2f}"     if ev     is not None else "N/A"
ns_s     = f"{news_sent:.2f}" if news_sent is not None else "N/A"
nc_s     = f"{news_cat:.2f}"  if news_cat  is not None else "N/A"

header = f"## Journal {now_str} WIB — {decision}"

block = f"""{header}
- **Regime:** {regime or 'N/A'}
- **Keputusan:** {dec_icon}
- **Setup:** {(setup or 'N/A')[:120]}

### Watchlist
{top3}

### Level
- Entry : {entry}
- Stop  : {stop}
- TP1   : {tp1}
- TP2   : {tp2}

### Probabilitas
- P(up)={p_up_s} | P(down)={p_down_s} | Conf={conf_s} | EV={ev_s}R
- News: sentiment={ns_s} catalyst={nc_s}

### Skenario PnL (Risk 2% = {risk_usdt} USDT)
- -1R = -{r1} USDT
- +1R = +{r1} USDT
- +2R = +{r2} USDT

### Lesson Learned
{lesson}

### Top 3 Rules ✅
1. Volume ≥ 1.3x avg 20-bar (hard filter)
2. Long hanya pada breakout + retest + follow-through
3. Cooldown 30–45 mnt setelah SL hit
"""

def write_journal(subdir, label):
    d = pathlib.Path(f"{BASE}/data/trading-journal/{subdir}")
    d.mkdir(parents=True, exist_ok=True)
    f = d / f"{today}.md"
    if not f.exists():
        f.write_text(f"# {label} Journal — {today}\n\n")
    existing = f.read_text()
    if header in existing:
        print(f"[{now_str} WIB] {subdir}: already written, skip")
        return
    with f.open("a") as fh:
        fh.write("\n" + block + "\n")
    print(f"[{now_str} WIB] ✅ {subdir} journal → {f}")

if MODE in ("aggressive", "both"):
    write_journal("aggressive", "Aggressive")
if MODE in ("predictive", "both"):
    write_journal("predictive", "Predictive")
PY
