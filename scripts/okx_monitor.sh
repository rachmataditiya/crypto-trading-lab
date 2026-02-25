#!/usr/bin/env bash
# okx_monitor.sh — Realtime OKX position monitor. No LLM, pure Python/API.
# Usage: bash okx_monitor.sh [--json]

CFG="/Users/rachmataditiya/.openclaw/workspace/data/trading-journal/okx_config.json"
DB="/Users/rachmataditiya/.openclaw/workspace/data/trading-journal/live_positions.db"
CACHE="/Users/rachmataditiya/.openclaw/workspace/data/trading-journal/realtime_cache.json"

python3 - "$@" <<'PYEOF'
import sys, json, datetime

JSON_MODE = "--json" in sys.argv
NOW_WIB = datetime.datetime.now(datetime.timezone(datetime.timedelta(hours=7)))

cfg = json.load(open("/Users/rachmataditiya/.openclaw/workspace/data/trading-journal/okx_config.json"))

try:
    import okx.Account as AM
    import okx.Trade as TM
    import okx.MarketData as MDM
    acc = AM.AccountAPI(cfg['api_key'], cfg['secret_key'], cfg['passphrase'], False, '0')
    trd = TM.TradeAPI(cfg['api_key'], cfg['secret_key'], cfg['passphrase'], False, '0')
    mkt = MDM.MarketAPI(flag='0')
except ImportError as e:
    print(f"ERROR: okx library not found: {e}")
    sys.exit(1)

# ── 1. Account Balance ────────────────────────────────────────────────────────
bal_r = acc.get_account_balance()
balances = {}
total_usdt = 0.0
if bal_r.get('code') == '0':
    for det in bal_r['data'][0]['details']:
        eq = float(det.get('eq') or 0)
        if eq > 0:
            balances[det['ccy']] = {
                'eq': eq,
                'avail': float(det.get('availBal') or 0),
                'eq_usd': float(det.get('eqUsd') or 0),
            }
            total_usdt += float(det.get('eqUsd') or 0)

# ── 2. Open Orders (regular + algo) ─────────────────────────────────────────
open_orders = trd.get_order_list(instType='SPOT').get('data', [])
algo_orders = trd.order_algos_list(ordType='conditional', instType='SPOT').get('data', [])

# ── 3. Live prices for held coins ────────────────────────────────────────────
prices = {}
for ccy in balances:
    if ccy == 'USDT':
        prices['USDT'] = 1.0
        continue
    inst = f"{ccy}-USDT"
    r = mkt.get_ticker(instId=inst)
    if r.get('code') == '0' and r.get('data'):
        prices[ccy] = float(r['data'][0].get('last', 0))

# ── 4. Internal DB positions ─────────────────────────────────────────────────
import sqlite3
db_positions = []
try:
    conn = sqlite3.connect("/Users/rachmataditiya/.openclaw/workspace/data/trading-journal/live_positions.db")
    rows = conn.execute(
        "SELECT symbol, direction, qty, entry_price, stop_price, tp1_price, opened_at "
        "FROM open_positions WHERE status='OPEN' ORDER BY opened_at DESC"
    ).fetchall()
    for row in rows:
        sym, direction, qty, entry, sl, tp1, opened_at = row
        ccy = sym.replace('USDT', '')
        live_price = prices.get(ccy, 0)
        if live_price <= 0 and ccy in prices:
            live_price = prices[ccy]
        if live_price <= 0:
            # Fallback to cache
            try:
                cache = json.load(open("/Users/rachmataditiya/.openclaw/workspace/data/trading-journal/realtime_cache.json"))
                live_price = cache['symbols'].get(sym, {}).get('price', 0)
            except: pass
        pnl_usdt = (live_price - entry) * qty if direction == 'Long' else (entry - live_price) * qty
        pnl_pct = ((live_price - entry) / entry * 100) if direction == 'Long' else ((entry - live_price) / entry * 100)
        dist_sl_pct = abs((live_price - sl) / entry * 100) if sl else 0
        dist_tp_pct = abs((tp1 - live_price) / entry * 100) if tp1 else 0
        db_positions.append({
            'symbol': sym, 'direction': direction, 'qty': qty,
            'entry': entry, 'sl': sl, 'tp1': tp1,
            'live_price': live_price, 'pnl_usdt': pnl_usdt,
            'pnl_pct': pnl_pct, 'dist_sl_pct': dist_sl_pct,
            'dist_tp_pct': dist_tp_pct, 'opened_at': opened_at,
        })
    conn.close()
except Exception as e:
    db_positions = []

# ── 5. Daily PnL from balance snapshot ───────────────────────────────────────
today = str(NOW_WIB.date())
try:
    daily = json.load(open("/Users/rachmataditiya/.openclaw/workspace/data/trading-journal/daily_balance.json"))
    opening = float(daily.get(today, {}).get('opening', 0))
except: opening = 0

if JSON_MODE:
    print(json.dumps({
        'ts': NOW_WIB.isoformat(),
        'total_equity_usdt': round(total_usdt, 4),
        'opening_usdt': opening,
        'pnl_today': round(total_usdt - opening, 4) if opening else None,
        'balances': {k: {**v, 'price': prices.get(k, 1)} for k,v in balances.items()},
        'open_positions': db_positions,
        'open_orders': len(open_orders),
        'algo_orders': len(algo_orders),
    }, indent=2))
    sys.exit(0)

# ── Human-readable output ────────────────────────────────────────────────────
print(f"\n{'='*55}")
print(f"  📊 OKX Monitor  {NOW_WIB.strftime('%Y-%m-%d %H:%M WIB')}")
print(f"{'='*55}")

pnl_today = round(total_usdt - opening, 4) if opening else None
pnl_sign = '+' if (pnl_today or 0) >= 0 else ''
print(f"\n💰 Total Equity  : {total_usdt:.4f} USDT")
print(f"   Opening hari ini: {opening:.4f} USDT" if opening else "   Opening: (belum ada snapshot)")
if pnl_today is not None:
    pct = pnl_today / opening * 100 if opening else 0
    print(f"   PnL hari ini  : {pnl_sign}{pnl_today:.4f} USDT ({pnl_sign}{pct:.2f}%)")

print(f"\n🏦 Saldo:")
for ccy, b in balances.items():
    price = prices.get(ccy, 1.0)
    val = b['eq'] * price
    print(f"   {ccy:8s}: {b['eq']:.6f}  ≈ {val:.2f} USDT  (avail: {b['avail']:.6f})")

print(f"\n📋 Posisi Terbuka ({len(db_positions)}):")
if not db_positions:
    print("   (tidak ada posisi)")
else:
    for p in db_positions:
        pnl_sign = '+' if p['pnl_usdt'] >= 0 else ''
        status = '🟢' if p['pnl_pct'] >= 0 else '🔴'
        sl_warn = '⚠️ ' if p['dist_sl_pct'] < 1.0 else ''
        print(f"   {status} {p['symbol']} {p['direction']}")
        print(f"      Entry: {p['entry']:.4f} | Live: {p['live_price']:.4f}")
        print(f"      PnL: {pnl_sign}{p['pnl_usdt']:.4f} USDT ({pnl_sign}{p['pnl_pct']:.2f}%)")
        print(f"      SL: {p['sl']:.4f} ({sl_warn}jarak {p['dist_sl_pct']:.2f}%)")
        print(f"      TP1: {p['tp1']:.4f} (jarak {p['dist_tp_pct']:.2f}%)")
        print(f"      Qty: {p['qty']:.6f} | Buka: {p['opened_at'][:16]}")

print(f"\n📑 Open Orders  : {len(open_orders)} regular + {len(algo_orders)} algo")
if algo_orders:
    for o in algo_orders:
        print(f"   ALGO {o.get('instId')} {o.get('side')} sz={o.get('sz')} slTrigger={o.get('slTriggerPx','?')}")
if open_orders:
    for o in open_orders:
        print(f"   {o.get('instId')} {o.get('side')} {o.get('sz')} @ {o.get('px')} [{o.get('state')}]")

print(f"\n{'='*55}\n")
PYEOF
