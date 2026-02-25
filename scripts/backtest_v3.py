#!/usr/bin/env python3
from __future__ import annotations
from dataclasses import dataclass
from pathlib import Path
from typing import List, Tuple
import urllib.request

ROOT = Path('/Users/rachmataditiya/.openclaw/workspace')
OUT = ROOT / 'tmp' / 'crypto_backtest_v3.md'

ASSETS = ['BTCUSDT', 'ETHUSDT', 'SOLUSDT']
THRESHOLDS = [20, 25, 30]
LOOKAHEAD = 12
STOP_ATR = 1.0
TP_ATR = 1.5
DAILY_KILL_R = -3.0

@dataclass
class Bar:
    o: float
    h: float
    l: float
    c: float
    v: float


def fetch_rows(symbol: str, limit: int = 2200) -> List[Bar]:
    url = f'https://www.cryptodatadownload.com/cdd/Binance_{symbol}_1h.csv'
    with urllib.request.urlopen(url, timeout=25) as r:
        txt = r.read().decode('utf-8', 'ignore')
    rows: List[Bar] = []
    for line in txt.splitlines():
        if line.startswith('https://') or line.startswith('Unix,') or not line.strip():
            continue
        c = line.split(',')
        if len(c) < 9:
            continue
        rows.append(Bar(float(c[3]), float(c[4]), float(c[5]), float(c[6]), float(c[7])))
        if len(rows) >= limit:
            break
    rows.reverse()
    return rows


def ema(vals: List[float], n: int) -> List[float]:
    if not vals:
        return []
    k = 2.0 / (n + 1)
    out = [vals[0]]
    p = vals[0]
    for v in vals[1:]:
        p = v * k + p * (1 - k)
        out.append(p)
    return out


def rsi(vals: List[float], n: int = 14) -> List[float]:
    out = [50.0] * len(vals)
    if len(vals) < n + 2:
        return out
    g = l = 0.0
    for i in range(1, n + 1):
        d = vals[i] - vals[i - 1]
        g += max(d, 0)
        l += max(-d, 0)
    ag, al = g / n, l / n
    out[n] = 100.0 if al == 0 else 100 - 100 / (1 + ag / al)
    for i in range(n + 1, len(vals)):
        d = vals[i] - vals[i - 1]
        ag = (ag * (n - 1) + max(d, 0)) / n
        al = (al * (n - 1) + max(-d, 0)) / n
        out[i] = 100.0 if al == 0 else 100 - 100 / (1 + ag / al)
    return out


def atr(rows: List[Bar], n: int = 14) -> List[float]:
    out = [0.0] * len(rows)
    if len(rows) < 2:
        return out
    tr = [0.0] * len(rows)
    for i in range(1, len(rows)):
        tr[i] = max(rows[i].h - rows[i].l, abs(rows[i].h - rows[i - 1].c), abs(rows[i].l - rows[i - 1].c))
    if len(rows) <= n:
        return out
    out[n] = sum(tr[1:n + 1]) / n
    for i in range(n + 1, len(rows)):
        out[i] = (out[i - 1] * (n - 1) + tr[i]) / n
    return out


def adx(rows: List[Bar], n: int = 14) -> List[float]:
    N = len(rows)
    out = [20.0] * N
    if N < n * 3:
        return out
    tr = [0.0] * N
    pdm = [0.0] * N
    mdm = [0.0] * N
    for i in range(1, N):
        up = rows[i].h - rows[i - 1].h
        dn = rows[i - 1].l - rows[i].l
        pdm[i] = up if up > dn and up > 0 else 0.0
        mdm[i] = dn if dn > up and dn > 0 else 0.0
        tr[i] = max(rows[i].h - rows[i].l, abs(rows[i].h - rows[i - 1].c), abs(rows[i].l - rows[i - 1].c))
    trn, pdmn, mdmn = sum(tr[1:n + 1]), sum(pdm[1:n + 1]), sum(mdm[1:n + 1])
    dx = [20.0] * N
    for i in range(n + 1, N):
        trn = trn - trn / n + tr[i]
        pdmn = pdmn - pdmn / n + pdm[i]
        mdmn = mdmn - mdmn / n + mdm[i]
        if trn == 0:
            continue
        pdi = 100 * pdmn / trn
        mdi = 100 * mdmn / trn
        den = pdi + mdi
        if den == 0:
            continue
        dx[i] = 100 * abs(pdi - mdi) / den
    seed = n * 2
    if seed < N:
        out[seed] = sum(dx[n + 1:seed + 1]) / n
        for i in range(seed + 1, N):
            out[i] = (out[i - 1] * (n - 1) + dx[i]) / n
    return out


def score_at(i: int, close: List[float], ema20: List[float], ema50: List[float], rsi14: List[float], atr14: List[float], adx14: List[float], vol: List[float]) -> int:
    if i < 170:
        return -999
    c = close[i]
    d24 = (c - close[i - 24]) / close[i - 24] * 100 if close[i - 24] else 0
    d3 = (c - close[i - 72]) / close[i - 72] * 100 if close[i - 72] else 0
    d7 = (c - close[i - 168]) / close[i - 168] * 100 if close[i - 168] else 0
    vavg = sum(vol[i - 168:i]) / 168
    vr = vol[i] / vavg if vavg else 0
    atr_pct = atr14[i] / c * 100 if c else 0

    s = 0
    s += 8 if c > ema20[i] else -8
    s += 10 if ema20[i] > ema50[i] else -10
    s += 6 if ema20[i] > ema20[i - 1] else -6
    s += 8 if d24 > 0.8 else (-8 if d24 < -0.8 else 0)
    s += 8 if d3 > 2.0 else (-8 if d3 < -2.0 else 0)
    s += 6 if d7 > 4.0 else (-6 if d7 < -4.0 else 0)
    s += 8 if vr >= 1.1 else (-5 if vr < 0.7 else 0)
    s += 6 if 50 <= rsi14[i] < 70 else (-4 if rsi14[i] < 35 or rsi14[i] > 78 else 0)
    s += 8 if adx14[i] > 25 else (-5 if adx14[i] < 18 else 0)
    if atr_pct > 2.8:
        s -= 4
    if atr_pct < 0.35:
        s -= 2
    return s


def size_mult(adx_v: float, atr_pct: float) -> float:
    mult = 1.0
    if adx_v < 22:
        mult *= 0.75
    if atr_pct > 2.4:
        mult *= 0.7
    return max(0.4, min(1.2, mult))


def trade_r_long(rows: List[Bar], i: int, atrv: float) -> float:
    entry = rows[i].c
    stop = entry - STOP_ATR * atrv
    tp = entry + TP_ATR * atrv
    for j in range(i + 1, i + LOOKAHEAD + 1):
        b = rows[j]
        if b.l <= stop:
            return -1.0
        if b.h >= tp:
            return TP_ATR / STOP_ATR
    end = rows[i + LOOKAHEAD].c
    risk = max(entry - stop, 1e-9)
    return (end - entry) / risk


def trade_r_short(rows: List[Bar], i: int, atrv: float) -> float:
    entry = rows[i].c
    stop = entry + STOP_ATR * atrv
    tp = entry - TP_ATR * atrv
    for j in range(i + 1, i + LOOKAHEAD + 1):
        b = rows[j]
        if b.h >= stop:
            return -1.0
        if b.l <= tp:
            return TP_ATR / STOP_ATR
    end = rows[i + LOOKAHEAD].c
    risk = max(stop - entry, 1e-9)
    return (entry - end) / risk


def summarize(rs: List[float]) -> Tuple[int, float, float, float, float, float]:
    t = len(rs)
    if t == 0:
        return 0, 0.0, 0.0, 0.0, 0.0, 0.0
    wins = sum(1 for x in rs if x > 0)
    wr = wins / t * 100
    ex = sum(rs) / t
    total = sum(rs)
    eq = peak = 0.0
    maxdd = 0.0
    for x in rs:
        eq += x
        peak = max(peak, eq)
        maxdd = min(maxdd, eq - peak)
    loss_sum = abs(sum(x for x in rs if x < 0))
    pf = (sum(x for x in rs if x > 0) / loss_sum) if loss_sum > 0 else 0.0
    return t, wr, ex, total, maxdd, pf


def run_backtest(rows: List[Bar], threshold: int, mode: str) -> Tuple[int, float, float, float, float, float]:
    close = [r.c for r in rows]
    vol = [r.v for r in rows]
    ema20 = ema(close, 20)
    ema50 = ema(close, 50)
    rsi14 = rsi(close, 14)
    atr14 = atr(rows, 14)
    adx14 = adx(rows, 14)

    results: List[float] = []
    i = 200
    current_day = i // 24
    day_r = 0.0

    while i < len(rows) - LOOKAHEAD - 1:
        day_idx = i // 24
        if day_idx != current_day:
            current_day = day_idx
            day_r = 0.0

        if day_r <= DAILY_KILL_R:
            i = (day_idx + 1) * 24
            continue

        atrv = atr14[i]
        c = close[i]
        atr_pct = (atrv / c * 100) if c else 0
        if atrv <= 0 or adx14[i] < 18:
            i += 1
            continue

        sc = score_at(i, close, ema20, ema50, rsi14, atr14, adx14, vol)
        placed = False

        if mode in ('long', 'both') and sc >= threshold and close[i] > ema50[i]:
            r = trade_r_long(rows, i, atrv) * size_mult(adx14[i], atr_pct)
            results.append(r)
            day_r += r
            placed = True

        if mode in ('short', 'both') and sc <= -threshold and close[i] < ema50[i]:
            r = trade_r_short(rows, i, atrv) * size_mult(adx14[i], atr_pct)
            results.append(r)
            day_r += r
            placed = True

        i += 3 if placed else 1

    return summarize(results)


def main() -> None:
    lines = ['# Crypto Backtest v3.2 (quick walk-forward)', '']
    lines.append(f'- Universe: {", ".join(ASSETS)}')
    lines.append(f'- Model: long/short + regime filter (ADX>=18) + adaptive sizing + daily kill-switch ({DAILY_KILL_R}R), stop={STOP_ATR} ATR, tp={TP_ATR} ATR, hold max {LOOKAHEAD}h')
    lines.append('')

    best_global = None
    for sym in ASSETS:
        rows = fetch_rows(sym)
        lines.append(f'## {sym}')
        best = None
        for mode in ('long', 'short', 'both'):
            for th in THRESHOLDS:
                t, wr, ex, tr, dd, pf = run_backtest(rows, th, mode)
                lines.append(f'- {mode}/th={th}: trades={t}, winrate={wr:.1f}%, expectancy={ex:.3f}R, total={tr:.2f}R, maxDD={dd:.2f}R, PF={pf:.2f}')
                cand = (ex, tr, -dd, pf, mode, th, t, wr)
                if best is None or cand > best:
                    best = cand
        if best:
            ex, tr, negdd, pf, mode, th, t, wr = best
            lines.append(f'- best: mode={mode}, th={th}, expectancy={ex:.3f}R, total={tr:.2f}R, trades={t}, winrate={wr:.1f}%, PF={pf:.2f}')
            if best_global is None or (ex, tr, negdd, pf) > best_global[0]:
                best_global = ((ex, tr, negdd, pf), sym, mode, th, t, wr)
        lines.append('')

    if best_global:
        _, sym, mode, th, t, wr = best_global
        lines.append('## Suggested v3.2 default (temporary)')
        lines.append(f'- Asset bias leader: {sym}')
        lines.append(f'- Mode: {mode}')
        lines.append(f'- Threshold: {th}')
        lines.append(f'- Confidence basis: trades={t}, winrate={wr:.1f}% (historical, not guarantee)')

    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text('\n'.join(lines) + '\n')
    print(OUT)


if __name__ == '__main__':
    main()
