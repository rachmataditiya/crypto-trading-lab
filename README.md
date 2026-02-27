# 🤖 Crypto Trading Lab

> Autonomous live spot trading system — Rust-powered, OKX-connected, halal (spot long-only).

Private repository. Personal use only.

---

## 📋 Overview

A fully automated crypto trading system built from scratch with:
- **Dual WebSocket real-time data** — OKX public WS (ticker/orderbook) + business WS (candles, HMAC auth)
- **AMS signal engine** — Adaptive Multi-regime Signal across 1H/4H/6H/12H timeframes (see [Math Model](#-mathematical-model--ams))
- **Chameleon regime detection** — continuous score ρ blends trend-following ↔ mean-reversion automatically
- **Live order execution** — OKX spot market orders, fractional Kelly sizing, software SL/TP
- **Multi-position management** — up to 3 concurrent, correlation-aware, 30-min cooldown per coin
- **Daily reporting** — WhatsApp summary via OpenClaw at 17:00 WIB

**Halal constraint enforced:** spot long-only, no shorts, no leverage, no derivatives, no riba.

---

## 🏗️ Architecture

```
OKX REST + WebSocket API
          │
          ▼
┌─────────────────────────────────────────────────────────┐
│  realtime-engine (KeepAlive)                            │
│  ├── Public WS:   ticker + books5 (19 coins, 38 ch)    │
│  ├── Business WS: candle1H/4H/6H/12H (HMAC auth)      │
│  ├── REST seed:   120×H1, 120×H4, 60×H6, 60×H12       │
│  ├── AMS compute: ρ, D, E_trend, E_rev per coin        │
│  └── Output:      realtime_cache.json (≤1 write/sec)   │
└────────────────────────┬────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────┐
│  sim-engine (every 300s)                                │
│  ├── Reads realtime_cache.json + AMS fields            │
│  ├── Tier A/B (trend) — Tier R (reversion, 0.5x Kelly) │
│  ├── AMS gate + 6-gate quality filter                  │
│  └── Output: sim_history.db (PAPER_TRADE / NO_TRADE)   │
└────────────────────────┬────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────┐
│  live-executor (every 60s, KeepAlive)                   │
│  ├── Reads PAPER_TRADE signals (last 20 min, by EV)    │
│  ├── Gates: corr group, 30-min cooldown, circuit breaker│
│  ├── Kelly × ams_kelly_multiplier → OKX market buy     │
│  ├── Post-fill: query actual qty, place SL algo        │
│  ├── Position monitor loop:                            │
│  │   ├── TP2   → cancel algos → full market sell       │
│  │   ├── Trail → 2.5% from high_water (after TP1)     │
│  │   ├── TP1   → 50% sell, SL moved to breakeven      │
│  │   ├── Time  → BTC/ETH 120min, mid 90min, alts 60min│
│  │   └── SL    → software backup (algo = crash guard)  │
│  └── Output: live_positions.db                         │
└────────────────────────┬────────────────────────────────┘
                         │
                         ▼
                OKX Spot Account (acctLv=1)
```

---

## 🪙 Coin Universe (19 coins)

**Top 8 (main scanner):**
`BTCUSDT` `ETHUSDT` `SOLUSDT` `BNBUSDT` `ADAUSDT` `DOGEUSDT` `XRPUSDT` `DOTUSDT`

**Extended 11:**
`AVAXUSDT` `APTUSDT` `SUIUSDT` `FILUSDT` `UNIUSDT` `LINKUSDT` `NEARUSDT` `ARBUSDT` `OPUSDT` `INJUSDT` `LTCUSDT`

**Correlation groups** (max 1 open per group simultaneously):
```
Group 1: BTC, ETH
Group 2: SOL, BNB, ADA
Group 3: AVAX, APT, ARB, OP, NEAR
Group 4: DOGE, LTC
```

---

## 🧮 Mathematical Model — AMS

> Full derivation with proofs: [`engine/README.md`](engine/README.md#mathematical-model-ams-adaptive-multi-regime-signal)

The core insight: **trend-following and mean-reversion are mathematically opposite interpretations of the same data.** A fixed-weight model cannot serve both. AMS uses a continuous regime score ρ to interpolate between two expert systems.

### Layer 0 — Regime Score ρ
```
ρ = σ((mean(ADX_1H, ADX_4H, ADX_6H, ADX_12H) − 22) / 8)

ρ → 1.0  strong trend (ADX >> 22)
ρ → 0.5  mixed regime (ADX ≈ 22)  ← most common
ρ → 0.0  ranging / reversion (ADX << 22)
```

### Layer 1 — Adaptive Weights W(ρ)
```
W(ρ) = ρ × W_trend + (1−ρ) × W_reversion

           EMA    RSI    ADX    Vol    Pos
W_trend  = [0.40, 0.20, 0.30, 0.05, 0.05]
W_rev    = [0.10, 0.40, 0.05, 0.20, 0.25]
```

### Layer 2 — Adaptive RSI (key insight — signal flip)
```
r_trend(RSI)      = tanh((RSI − 50) / 20)
r_reversion(RSI)  = tanh((35 − RSI) / 10)   ← positive when oversold!

r_adaptive = ρ × r_trend + (1−ρ) × r_reversion

RSI = 28, ρ = 0.9 (trending)  →  r = −0.80  (bearish)
RSI = 28, ρ = 0.1 (ranging)   →  r = +0.51  (bounce opportunity) ← FLIP
```

### Layer 3 — Adaptive Gate Threshold θ(ATR)
```
θ(atr%) = 0.10 + 0.10 × tanh(atr% / 1.5)
```
High volatility raises the gate automatically.

### Layers 4–6 — Score, Divergence, Entry
```
s(τ) = W.r·r_adaptive + W.e·e + W.a·a + W.v·v + W.p·p   ∈ [−1, +1]

D     = |s_1H − s_12H|                  ← rubber-band stretch
Γ     = σ(s_12H/θ) × σ(s_6H/θ) × σ(s_4H/θ)  ← gate chain

E_trend = Γ × (0.40·s_12H + 0.25·s_6H + 0.20·s_4H + 0.15·s_1H)
E_rev   = D × max(tanh((35−RSI_1H)/10), 0) × max(tanh(ln(vol)), 0)
E_final = ρ × E_trend + (1−ρ) × E_rev
```

### Layer 7 — Tier Assignment
| Regime | Condition | Tier | Kelly |
|--------|-----------|------|-------|
| ρ > 0.6 | E_trend > 0.30, align ≥ 3 | **A** | 1.0× |
| ρ > 0.6 | E_trend > 0.15, align ≥ 3 | **B** | 1.0× |
| ρ < 0.4 | E_rev > 0.08, D > 0.15 | **R** | 0.5× |
| Mixed | Best of A/R above | **A/R** | — |
| — | Otherwise | **C** | skip |

**Tier R** = mean-reversion bounce: half Kelly, conservative TP (1.2R), max 45 min hold.

### Layer 8 — C_recent Adaptive Kelly
```
ewa(t) = λ × ewa(t−1) + (1−λ) × outcome    λ = 0.94 (~16 trade half-life)
C_recent = clamp(ewa / target_win_rate, 0.5, 1.5)
kelly_final = base_kelly × tier_mult × C_recent
```
System scales position size up/down based on recent per-mode performance.

---

## 🧠 Signal Generation

### 6-Gate Quality Filter
All gates must pass for a signal to become a trade:

| Gate | Condition |
|------|-----------|
| **AMS** | `ams_tier != C` (E_final above threshold) |
| **Regime** | Market not Risk-off |
| **Score** | Composite indicator score threshold met |
| **Volume** | `≥ 2.0x` baseline, tier EARLY–IMPULSE |
| **ADX** | `≥ 20.0` (Wilder trending threshold) |
| **Prob/EV** | Bayesian posterior ≥ threshold, EV ≥ 0.5R |

### Bayesian Probability Model
```
feature = 0.22×trend + 0.16×ema_stack + 0.16×momentum
        + 0.20×volume + 0.12×adx − 0.08×rsi + 0.06×news

posterior = bayesian_update(prior=0.62, tech_lh, news_lh)
EV = p_up × 2.0R − p_down × 1.0R
```

### Volume Tiers
```
WEAK     < 1.0x  — no conviction, skip
NORMAL   1.0–2.0x — baseline
EARLY    2.0–4.0x — accumulation, valid entry zone
IMPULSE  4.0–8.0x — ideal target zone ← target
SURGE    > 8.0x  — late-stage, chase risk
```

### Stale Signal Handling
When price moves between signal generation and execution:
```
< −3%   from entry → DUMPED   (skip, invalidated)
−3% to +3%         → NORMAL   (execute as-is)
+3% to +10%        → DYNAMIC  (recalculate SL/TP from fill)
> +10%  from entry → CHASING  (skip, too late)
```

---

## ⚖️ Risk Management

### Fractional Kelly Sizing (25% Kelly)
```
Tier A  (EV ≥ 1.5R, conf ≥ 0.7)  → 3–4% per trade
Tier B  (EV ≥ 0.8R, conf ≥ 0.4)  → 1.5–2.5% per trade
Tier C  (EV < 0.5R or conf < 0.15) → SKIP
```

### Portfolio Rules
| Rule | Value |
|------|-------|
| Max open positions | 3 concurrent |
| Max portfolio risk | 6% total deployed |
| Daily kill-switch | −10% drawdown → halt |
| Circuit breaker | ≥3 consecutive losses → halt |
| Cooldown per coin | 30 min after any close (120 min after time-stop) |
| Per-coin max alloc | BTC/ETH 40%, mid 25%, alts 20%, high-beta 15% |

### Cooldown — Why It Exists

The 30-min cooldown per coin prevents **whipsaw spiral**:
```
Without cooldown:
  08:00 → Entry SOL, SL hit  −1.8%
  08:02 → Same signal fires again (conditions unchanged)
  08:03 → Entry SOL, SL hit  −1.8%   × 3 = −5.4% from one coin in 5 minutes

With cooldown:
  08:00 → Entry SOL, SL hit  −1.8%
  08:02 → Signal blocked (30-min window)
  08:30 → Conditions may have changed → re-evaluate
```

The **120-min extended cooldown for `CLOSED_TIME`** prevents re-entry into a setup that already failed to develop. If price didn't move in 60–120 min, the same H4 setup is still valid but momentum hasn't arrived — re-entry would just time-out again.

Mathematically: `cooldown ≈ 1 × timeframe_resolution` (one H4 candle reset window).

### Per-Coin Time-Stop
Large caps move slowly and need more time to develop a move:
```
BTC, ETH                              → 120 minutes
SOL, BNB, XRP, ADA, DOT, LINK, LTC, DOGE → 90 minutes
AVAX, APT, SUI, FIL, UNI, NEAR, ARB, OP, INJ → 60 minutes
```

### Exit Priority (position monitor loop)
```
1. TP2        → full exit at target
2. Trail stop → 2.5% from high_water, only after TP1, only if > entry
3. TP1        → 50% exit, SL moved to breakeven
4. Time stop  → per-coin limit above
5. SL soft    → software backup if algo SL fails
```

### Pump & Dump Filter
Blocks entry when:
- `volume_ratio > 4.0 AND RSI > 78` (volume-driven pump, overbought)
- `price > EMA20 × 1.08` (price stretched >8% above EMA20)

---

## 📊 Signal Status Codes

| Code | Meaning |
|------|---------|
| `FILLED` | Buy order executed, position open |
| `CLOSED_TP` | TP1 partial exit (50%), SL moved to breakeven |
| `CLOSED_TP2` | Full exit at TP2 target |
| `CLOSED_TRAIL` | Trailing stop triggered (2.5% from high_water) |
| `CLOSED_BREAKEVEN` | Breakeven SL triggered after TP1 |
| `CLOSED_TIME` | Per-coin time-stop reached |
| `CLOSED_SL` | Stop-loss triggered |
| `CLOSED_MANUAL` | Manual intervention |
| `SKIPPED_STALE` | Price dumped/chasing before execution |
| `SKIPPED_CORR` | Correlation group already occupied |
| `SKIPPED_COOLDOWN` | 30-min cooldown after same-coin close |
| `SKIPPED_MAXPOS` | Already at 3 open positions |
| `SKIPPED_DUPE` | Same symbol already open |
| `SKIPPED_PUMP_DUMP` | Pump/exhaustion pattern detected |
| `SKIPPED_TIER_C` | Signal EV/confidence too weak |
| `SKIPPED_BALANCE` | Insufficient USDT balance |

---

## 🗂️ Project Structure

```
crypto-trading-lab/
├── engine/
│   └── src/
│       ├── bin/
│       │   └── live_executor.rs   # OKX live order execution + position monitor
│       ├── hmtf.rs                # AMS model: ρ, adaptive weights, r_adaptive, tiers
│       ├── ams_tracker.rs         # EWA win-rate tracker → C_recent adaptive Kelly
│       ├── strategy.rs            # Signal evaluation + AMS + 6-gate quality filter
│       ├── indicators.rs          # RSI, ADX, EMA, ATR, volume scoring
│       ├── realtime.rs            # Dual WS daemon + 1H/4H/6H/12H indicator engine
│       ├── regime.rs              # Market regime detection (RiskOn/Mixed/RiskOff)
│       ├── risk.rs                # Kelly sizing + tier classification
│       ├── models.rs              # Data types (incl. AMS fields)
│       ├── config.rs              # Config structs
│       └── data_feed.rs           # Cache + scan file parser
├── scripts/
│   ├── okx_monitor.sh             # Real-time OKX balance + position monitor
│   ├── balance_snapshot.sh        # Daily opening balance snapshot
│   ├── self_heal.sh               # Process watchdog
│   └── alert_guard.sh             # Signal alert via OpenClaw
├── references/                    # Trading playbook, scoring guide
├── SKILL.md                       # OpenClaw skill definition
├── OPS_STATE.md                   # Current ops snapshot + bug history
└── README.md
```

---

## ⚙️ Configuration

### `okx_config.json`
```json
{
  "max_capital_usdt": 100,
  "risk_per_trade_pct": 2.0,
  "max_daily_drawdown_pct": 10.0,
  "circuit_breaker_consecutive_losses": 3,
  "allowed_symbols": ["BTCUSDT", "ETHUSDT", "SOLUSDT", "...19 total"]
}
```

### `account_config.json`
```json
{
  "preset": "aggressive",
  "maxOpenPositions": 3,
  "dailyKillSwitchR": -3,
  "realtimeSymbols": ["BTCUSDT", "ETHUSDT", "...19 total"]
}
```

---

## 🚀 Build & Run

```bash
cd engine

# Build all binaries
cargo build --release

# Run tests (87 total, 0 failures)
cargo test

# Specific binary
cargo build --release --bin live-executor
cargo build --release --bin sim-engine
```

### LaunchAgents (macOS, auto-start on login)
```bash
# Restart services
launchctl kickstart -k gui/$(id -u)/ai.openclaw.live-executor
launchctl kickstart -k gui/$(id -u)/ai.openclaw.realtime-engine
launchctl kickstart -k gui/$(id -u)/ai.openclaw.sim-engine

# Check status
pgrep -f "live-executor"  && echo "✅ executor"  || echo "❌ executor DEAD"
pgrep -f "realtime-daemon" && echo "✅ realtime" || echo "❌ realtime DEAD"
```

### Monitor
```bash
# Real-time OKX balance + positions
bash scripts/okx_monitor.sh

# AMS market pulse (all 19 coins — regime, divergence, tiers)
python3 -c "
import json, datetime
d = json.load(open('data/trading-journal/realtime_cache.json'))
syms = sorted(d.get('symbols',{}).items(), key=lambda x: -x[1].get('hmtf_entry_score',0))
for sym, v in syms:
    price = v.get('price', 0); rsi = v.get('rsi14', 0)
    rho   = v.get('ams_regime_rho', 0)
    label = v.get('ams_regime_label', '?')[:5]
    D     = v.get('ams_divergence_d', 0)
    et    = v.get('ams_e_trend', 0); er = v.get('ams_e_reversion', 0)
    tier  = v.get('hmtf_tier', 'C'); kelly = v.get('ams_kelly_multiplier', 0)
    flag  = ' 🔴' if tier in ['A','B','R'] else ''
    print(f'{sym:<12} \${price:>10.2f}  ρ={rho:.2f}({label}) D={D:.2f} Et={et:+.3f} Er={er:+.3f} T={tier} K={kelly:.1f}x{flag}')
"

# Recent signals
sqlite3 data/trading-journal/sim_history.db \
  "SELECT decision, setup_idea, inserted_at FROM simulation_events ORDER BY id DESC LIMIT 5;"

# Today's PnL
sqlite3 data/trading-journal/live_positions.db \
  "SELECT trade_date, starting_equity, realized_pnl, trade_count FROM daily_stats ORDER BY trade_date DESC LIMIT 3;"

# Executor log
tail -30 /tmp/openclaw/live-executor.log
```

---

## 📈 Performance

| Date | PnL | Trades | Notes |
|------|-----|--------|-------|
| 2026-02-18 | −$0.57 | 1 | First live trade, BNBUSDT, pre-fix |
| 2026-02-22 | +$3.83 | 3 | AVAXUSDT, bug era (SL di atas entry) |
| 2026-02-25 | +$2.90 | 5 | Best day — SOL TP×3, ETH+SOL TP2 |
| 2026-02-26 | +$1.99 | 3 | ETH ×2 CLOSED_TIME, sistem stabil |

**Starting capital:** ~$115 USDT
**Current equity:** ~$122 USDT
**Live since:** 2026-02-18

---

## 🔒 Security

- API keys in `okx_config.json` (gitignored)
- Trade permission only — **no withdraw**
- IP whitelist recommended for production
- `cancel_pending_algos()` called before every sell (prevents OKX balance freeze)

---

## 🏛️ Known Limitations

| Limitation | Impact | Mitigation |
|-----------|--------|-----------|
| OKX acctLv=1: no algo conditional orders | Software SL only | 60s poll + cancel_algos before sell |
| Software SL latency (~60s) | Flash crash risk | OKX algo SL as crash backup |
| 5-min signal cadence | May miss fast micro-pumps | Covers >90% large-cap moves |
| H4 indicators lag short-term | May block valid 15m setups | By design — filters noise |

---

*Built with love by beb & Radit* ❤️
