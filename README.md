# 🤖 Crypto Trading Lab

> Autonomous spot trading system — Rust-powered, OKX-connected, halal (spot long-only).

Private repository. Personal use only.

---

## 📋 Overview

A fully automated crypto trading system built from scratch with:
- **Real-time market data** via OKX public WebSocket + REST API
- **Bayesian signal evaluation** with multi-gate quality filters
- **Live order execution** on OKX spot market (no futures, no leverage)
- **Software stop-loss** with 60s polling (OKX acctLv=1 limitation)
- **Daily reporting** via WhatsApp (OpenClaw integration)

**Halal constraint enforced:** spot long-only, no shorts, no leverage, no derivatives.

---

## 🏗️ Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    macOS LaunchAgents                    │
│                                                         │
│  ┌──────────────────┐    ┌──────────────────────────┐   │
│  │  realtime-engine │    │      sim-engine          │   │
│  │  (KeepAlive)     │    │  (StartInterval: 300s)   │   │
│  │                  │    │                          │   │
│  │  OKX REST/WS     │───▶│  strategy.rs             │   │
│  │  20s poll        │    │  indicators.rs           │   │
│  │  RSI14, ATR14    │    │  regime.rs               │   │
│  │  ADX14, EMA20/50 │    │  risk.rs                 │   │
│  └────────┬─────────┘    └────────────┬─────────────┘   │
│           │                           │                  │
│           ▼                           ▼                  │
│    realtime_cache.json          sim_history.db           │
│                                       │                  │
│                          ┌────────────▼─────────────┐   │
│                          │     live-executor         │   │
│                          │     (KeepAlive)           │   │
│                          │                          │   │
│                          │  60s poll                │   │
│                          │  Kelly sizing            │   │
│                          │  OKX market orders       │   │
│                          │  Software SL/TP          │   │
│                          └────────────┬─────────────┘   │
│                                       │                  │
│                                       ▼                  │
│                               live_positions.db          │
└─────────────────────────────────────────────────────────┘
                                        │
                                        ▼
                          OKX Spot Account (acctLv=1)
```

---

## 🧠 Signal Generation Logic

### Quality Gate (6-gate system)
All 6 gates must pass before a signal becomes a trade:

| Gate | Aggressive Mode | Normal Mode |
|------|----------------|-------------|
| **Regime** | Not RiskOff | Not RiskOff |
| **Score** | `price > EMA20 > EMA50` | `score >= min_score` |
| **Volume** | `>= 2.0x` (RiskOn) / `2.2x` (Mixed) / `2.5x` (RiskOff) | `>= 1.5x` (+0.2/+0.5 by regime) |
| **ADX** | `>= 20.0` (Wilder trending standard) | `>= min_adx` |
| **Confidence** | Skip (EMA+vol sufficient) | Not "weak" |
| **Prob/EV** | `>= 0.5R` | `conf >= 0.55 && EV > 0` |

### Volume Tier Classification
Research-backed thresholds (Kamps & Kleinberg 2018, empirical OKX data):

```
WEAK    < 1.0x  — no conviction, skip
NORMAL  1.0–2.0x — baseline activity
EARLY   2.0–4.0x — early accumulation, valid entry zone
IMPULSE 4.0–8.0x — ideal: early pump confirmed ← target zone
SURGE   > 8.0x  — late-stage, chase risk high
```

### Bayesian Probability Model
```rust
feature_score = 0.22*trend + 0.16*ema_stack + 0.16*momentum
              + 0.20*vol   + 0.12*adx       - 0.08*rsi
              + 0.06*news_sentiment

posterior = bayesian_update(prior, tech_likelihood, news_likelihood)
EV = p_up * 2.0R - p_down * 1.0R
```

Volume weight (0.20) is highest individual component — *volume leads price in crypto*.

---

## ⚖️ Risk Management

### Fractional Kelly Sizing
```
Tier A  (EV >= 1.5R, conf >= 0.7) → 3–4% risk
Tier B  (EV >= 0.8R, conf >= 0.4) → 1.5–2.5% risk
Tier C  (EV < 0.5R  OR conf < 0.15) → SKIP
```

### Portfolio Rules
- **Max open positions**: 1 (conservative start)
- **Daily kill-switch**: -3R cumulative loss → halt all trading
- **Max capital**: 100 USDT deployed at any time
- **Correlation groups**: BTC+ETH max 1 simultaneously, SOL+BNB max 1
- **Max hold**: 60 minutes per position (auto time-exit)

### Stop-Loss Mechanism
OKX acctLv=1 does not support algo/conditional orders. SL is implemented in software:
- Poll every 60 seconds
- If `live_price <= stop_price` → market sell immediately
- `market_sell()` returns `bool` — DB only updated on confirmed OKX success
- Partial TP: 50% exit at TP1, trail SL to breakeven

---

## 📊 Stale Signal Detection

When price moves before execution:

```
< -3%  from signal entry → DUMPED   (invalidated, skip)
-3%–+3%                  → NORMAL   (execute as planned)
+3%–+10%                 → DYNAMIC  (recalculate SL/TP from fill)
> +10% from signal entry → CHASING  (late, skip)
```

For DYNAMIC entries: SL/TP are recalculated preserving original ATR% and TP% ratios from fill price.

---

## 🗂️ Project Structure

```
crypto-trading-lab/
├── engine/                        # Rust workspace
│   ├── src/
│   │   ├── bin/
│   │   │   └── live_executor.rs   # OKX live order execution
│   │   ├── strategy.rs            # Signal evaluation + quality gates
│   │   ├── indicators.rs          # RSI, ADX, EMA, volume scoring
│   │   ├── realtime.rs            # OKX REST polling + WS daemon
│   │   ├── regime.rs              # Market regime detection (RiskOn/Mixed/RiskOff)
│   │   ├── risk.rs                # Kelly sizing + tier classification
│   │   ├── models.rs              # Data types
│   │   ├── config.rs              # Config structs
│   │   └── data_feed.rs           # Scan file parser
│   └── Cargo.toml
├── scripts/
│   ├── okx_monitor.sh             # Real-time OKX balance + position monitor
│   ├── balance_snapshot.sh        # Daily opening balance snapshot
│   ├── self_heal.sh               # Process watchdog
│   ├── alert_guard.sh             # Signal alert via OpenClaw
│   ├── journal_write.sh           # Daily journal writer
│   ├── sim_engine_daemon.sh       # Sim-engine launcher (every 5 min)
│   └── realtime_daemon.sh         # Realtime-engine launcher (KeepAlive)
└── README.md
```

---

## ⚙️ Configuration

### `data/trading-journal/account_config.json`
```json
{
  "mode": "paper",
  "execution": "paper_with_live_executor",
  "preset": "aggressive",
  "maxRiskPerTradePct": 2,
  "maxOpenPositions": 1,
  "dailyKillSwitchR": -3,
  "minVolumeRatio": 1.5,
  "minScoreToTrade": 40,
  "minAdx": 18,
  "minExpectedR": 1.8
}
```

### `data/trading-journal/okx_config.json`
```json
{
  "max_capital_usdt": 100,
  "risk_per_trade_pct": 2.0,
  "max_daily_drawdown_pct": 10.0,
  "circuit_breaker_consecutive_losses": 3
}
```

---

## 🚀 Setup & Running

### Prerequisites
- macOS (uses LaunchAgents)
- Rust + Cargo
- Python 3 (for OKX SDK)
- OKX API key (read + trade permissions, acctLv=1)

### Build
```bash
cd engine
cargo build --release
```

### Run Tests
```bash
cargo test --bin live-executor  # 47 tests
cargo test --bin sim-engine     # 28 tests (indicators + strategy)
# Total: 75 tests
```

### LaunchAgents (auto-start on login)
```bash
# Register all services
launchctl load ~/Library/LaunchAgents/ai.openclaw.live-executor.plist
launchctl load ~/Library/LaunchAgents/ai.openclaw.realtime-engine.plist
launchctl load ~/Library/LaunchAgents/ai.openclaw.sim-engine.plist

# Check status
launchctl list | grep openclaw

# Manual restart
launchctl kickstart -k gui/$(id -u)/ai.openclaw.live-executor
launchctl kickstart -k gui/$(id -u)/ai.openclaw.realtime-engine
```

### Monitor
```bash
# Real-time OKX balance + open positions
bash scripts/okx_monitor.sh

# Check all services
pgrep -f "live-executor" && echo "✅ executor" || echo "❌ executor DEAD"
pgrep -f "realtime-daemon" && echo "✅ realtime" || echo "❌ realtime DEAD"
launchctl list | grep "openclaw.sim-engine"
```

---

## 📈 Performance Tracking

### Database Schema
```sql
-- Signal decisions
processed_signals (signal_id, symbol, direction, entry, stop, tp1, status, fill_price, pnl_usdt)

-- Open positions
open_positions (signal_id, symbol, qty, entry_price, stop_price, tp1_price, risk_usdt, opened_at)

-- Daily stats
daily_stats (trade_date, starting_equity, realized_pnl, consecutive_losses, trade_count)

-- Simulation history (15-min cadence)
simulation_events (trade_date, trade_time, decision, quality_gate, confidence, expected_value_r, rationale)
```

### Status Values
```
FILLED          → order executed, position open
CLOSED_TP       → take profit hit
CLOSED_SL       → stop loss hit
CLOSED_TIME     → 60-min max hold reached
SKIPPED_STALE   → price moved 3-10% (DYNAMIC path taken)
SKIPPED_CHASING → price ran >10% (skip)
SKIPPED_DUMPED  → price fell >3% (skip)
SKIPPED_TIER_C  → signal EV/confidence too weak
SKIPPED_CORR    → correlation limit reached
ABORTED         → order placed but SL invalid
```

---

## 🏛️ Known Limitations

| Limitation | Impact | Mitigation |
|-----------|--------|-----------|
| OKX acctLv=1: no algo orders | Software SL only | 60s poll, market sell on breach |
| Software SL latency | Flash crash risk | Acceptable trade-off vs unlimited downside |
| 5-min signal cadence | May miss fast pumps | Reduced from 15min; covers >90% of large-cap moves |
| Small sample size | Edge not fully proven | Track 50+ trades before scaling capital |

---

## 📅 Live Performance

| Date | PnL | Trades | Notes |
|------|-----|--------|-------|
| 2026-02-18 | -0.50% | 1 | First live trade (BNBUSDT, SL hit, pre-fix) |
| 2026-02-25 | +2.53% | 3 | Best day — 3 SOL profitable, manual fix |
| 2026-02-25 (eve) | +4.12%+ | 2 | ETH+SOL open, both near TP1 |

Starting capital: ~$115 USDT | Current: ~$120 USDT

---

## 🔒 Security

- API keys stored in `okx_config.json` (gitignored)
- Read + Trade permissions only (no withdraw)
- All secrets excluded from this repo

---

*Built with ❤️ + lots of debugging. Iteration > perfection.*
