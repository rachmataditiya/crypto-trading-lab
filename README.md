# 🤖 Crypto Trading Lab

> Autonomous live spot trading system — Rust-powered, OKX-connected, halal (spot long-only).

Private repository. Personal use only.

---

## 📋 Overview

A fully automated crypto trading system built from scratch with:
- **Dual WebSocket real-time data** — OKX public WS (ticker/orderbook) + business WS (candles, HMAC auth)
- **H4 indicator engine** — RSI14, EMA20/50, ATR14, ADX14 from 4H confirmed candles for 19 coins
- **Bayesian signal evaluation** — multi-gate quality filters with probabilistic EV scoring
- **Live order execution** — OKX spot market orders, Kelly sizing, software SL/TP
- **Multi-position management** — up to 3 concurrent, correlation-aware
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
│  ├── Business WS: candle1m/1H/4H/1D (76 ch, HMAC auth)│
│  ├── REST seed:   120×H1, 120×H4, 100×D1 at startup   │
│  └── Output:      realtime_cache.json (≤1 write/sec)   │
└────────────────────────┬────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────┐
│  sim-engine (every 300s)                                │
│  ├── Reads realtime_cache.json                         │
│  ├── 6-gate quality filter (regime/score/vol/adx/conf) │
│  ├── Bayesian EV scoring + signal tier (A/B/C)         │
│  └── Output: sim_history.db (PAPER_TRADE / NO_TRADE)   │
└────────────────────────┬────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────┐
│  live-executor (every 60s, KeepAlive)                   │
│  ├── Reads PAPER_TRADE signals (last 20 min, by EV)    │
│  ├── Gates: corr group, cooldown, circuit breaker      │
│  ├── Kelly sizing → OKX market buy                     │
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

## 🧠 Signal Generation

### 6-Gate Quality Filter
All gates must pass for a signal to become a trade:

| Gate | Condition |
|------|-----------|
| **Regime** | Market not Risk-off |
| **Score** | `price > EMA20 > EMA50` (momentum mode) |
| **Volume** | `≥ 2.0x` baseline (aggressive), tier EARLY–IMPULSE |
| **ADX** | `≥ 20.0` (Wilder trending threshold) |
| **Confidence** | Not "weak" (Bayesian posterior ≥ threshold) |
| **Prob/EV** | `p_up ≥ 0.5`, `EV ≥ 0.5R` |

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
| Cooldown per coin | 30 min after close |
| Per-coin max alloc | BTC/ETH 40%, mid 25%, alts 20%, high-beta 15% |

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
│       ├── strategy.rs            # Signal evaluation + 6-gate quality filter
│       ├── indicators.rs          # RSI, ADX, EMA, ATR, volume scoring
│       ├── realtime.rs            # Dual WS daemon + H4 indicator engine
│       ├── regime.rs              # Market regime detection (RiskOn/Mixed/RiskOff)
│       ├── risk.rs                # Kelly sizing + tier classification
│       ├── models.rs              # Data types
│       ├── config.rs              # Config structs
│       └── data_feed.rs           # Scan file parser
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

# Run tests (142 total, 0 warnings)
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

# Market pulse (H4 indicators, all 19 coins)
python3 -c "
import json, datetime
d = json.load(open('data/trading-journal/realtime_cache.json'))
for sym, v in d.get('symbols', {}).items():
    price = v.get('price', 0); rsi = v.get('rsi14', 0)
    ema20 = v.get('ema20', 0); ema50 = v.get('ema50', 0)
    vol   = v.get('volume_ratio', 0)
    align = '✅' if (price > ema20 > ema50) else '❌'
    print(f'{sym:<12} \${price:>10.2f}  EMA{align}  RSI={rsi:>5.1f}  vol={vol:.2f}x')
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
