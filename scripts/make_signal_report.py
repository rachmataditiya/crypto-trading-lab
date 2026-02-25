#!/usr/bin/env python3
from pathlib import Path
import re

src = Path('/Users/rachmataditiya/.openclaw/workspace/tmp/crypto_scan_latest.txt')
out = Path('/Users/rachmataditiya/.openclaw/workspace/tmp/crypto_signal_latest.md')
if not src.exists():
    raise SystemExit('scan file missing')

t = src.read_text()
coins = []
for m in re.finditer(r'([A-Z0-9]+USDT)\n- Grade/Score: ([ABC]) / (-?\d+).*?24h/3d/7d: ([+\-0-9\.]+)% / ([+\-0-9\.]+)% / ([+\-0-9\.]+)%.*?Volume ratio: ([0-9\.]+)x', t, re.S):
    sym = m.group(1)
    grade = m.group(2)
    score = int(m.group(3))
    d24 = float(m.group(4))
    d3 = float(m.group(5))
    d7 = float(m.group(6))
    vol = float(m.group(7))
    coins.append((sym, grade, score, d24, d3, d7, vol))

btc = next((c for c in coins if c[0] == 'BTCUSDT'), None)
eth = next((c for c in coins if c[0] == 'ETHUSDT'), None)
market = 'mixed'
alert = 'none'
if btc and eth:
    if btc[1] == 'A' and eth[1] == 'A':
        market, alert = 'risk-on', 'breakout-confirmed'
    elif btc[1] == 'C' and eth[1] == 'C':
        market, alert = 'risk-off', 'downside-pressure'

coins_sorted = sorted(coins, key=lambda x: ({'A': 0, 'B': 1, 'C': 2}[x[1]], -x[2], -x[6]))
watch = coins_sorted[:3]

lines = []
lines.append('# Crypto Signal Report')
lines.append(f'- Market mode: **{market}**')
lines.append(f'- BTC/ETH alert: **{alert}**')
lines.append('')
lines.append('## Top Watchlist (A/B/C)')
for c in watch:
    lines.append(f'- {c[0]} | Score {c[1]}({c[2]}) | 24h {c[3]:+.2f}% | 3d {c[4]:+.2f}% | vol {c[6]:.2f}x')

lines.append('')
lines.append('## Entry Template (quick-fill)')
for c in watch:
    lines.append(f'- {c[0]}: entry zone __ / invalidation __ / T1 __ / T2 __')

out.write_text('\n'.join(lines) + '\n')
print(out)
