---
name: crypto-trading-lab
description: Manage and monitor the autonomous live crypto trading system on OKX spot market. Use when checking system status, reading PnL, restarting services, analyzing trade logs, improving the Rust engine (live-executor / realtime-engine / sim-engine), updating strategy params, or debugging trading issues.
---

# Crypto Trading Lab

## Overview
Fully automated live trading system on OKX spot market (no futures, no leverage, no short — syariat-compliant).
Built with Rust (3 binaries) + macOS LaunchAgents + SQLite + OpenClaw integration.

**Capital:** ~120 USDT | **Risk:** 2%/trade | **Max hold:** 60 min | **Max positions:** 3 concurrent

---

## Architecture

```
OKX REST/WebSocket API
        │
        ▼
realtime-engine  ←── dual WS: public (ticker/books5) + business (candles, HMAC auth)
  H4 RSI14, EMA20/50, ATR14, ADX14 for 19 coins
  Writes: realtime_cache.json
        │
        ▼
sim-engine (every 5 min)
  reads realtime_cache.json
  evaluates signal quality: tier A/B/C, regime, pump/dump, stale
  writes: sim_history.db (decision = PAPER_TRADE | NO_TRADE)
        │
        ▼
live-executor (every 60s)
  reads PAPER_TRADE signals from last 20 min
  correlation + cooldown + circuit-breaker gates
  Kelly sizing → OKX market buy
  60s polling: SL (algo) + TP1/TP2/Trail/Time exits (software)
  writes: live_positions.db
        │
        ▼
journal-writer + alert-guard
  daily PnL summary → WhatsApp 17:00 WIB
  alert if consecutive losses ≥ 3
```

---

## File Paths

| File | Path |
|---|---|
| Live executor src | `engine/src/bin/live_executor.rs` |
| Realtime engine src | `engine/src/realtime.rs` |
| Sim engine src | `engine/src/bin/sim-engine.rs` |
| OKX config | `data/trading-journal/okx_config.json` |
| Account config | `data/trading-journal/account_config.json` |
| Live DB | `data/trading-journal/live_positions.db` |
| Sim DB | `data/trading-journal/sim_history.db` |
| Realtime cache | `data/trading-journal/realtime_cache.json` |
| Executor log | `/tmp/openclaw/live-executor.log` |
| Realtime log | `/tmp/realtime-engine.err.log` |
| Sim log | `/tmp/openclaw/sim-engine.log` |
| OPS state | `OPS_STATE.md` |
| Scripts | `scripts/` |

All paths are relative to: `~/.openclaw/workspace/skills/crypto-trading-lab/`

---

## LaunchAgents (macOS)

| Agent ID | Binary | Type |
|---|---|---|
| `ai.openclaw.live-executor` | `live-executor` | KeepAlive |
| `ai.openclaw.realtime-engine` | `sim-engine realtime-daemon` | KeepAlive |
| `ai.openclaw.sim-engine` | `sim-engine` | StartInterval 300s |
| `ai.openclaw.crypto-scan` | `crypto-scan-rs` | StartInterval 7200s |
| `ai.openclaw.journal-writer` | `journal-writer` | (cron-driven) |
| `ai.openclaw.alert-guard` | `alert_guard.sh` | (cron-driven) |
| `ai.openclaw.self-heal` | `self_heal.sh` | (periodic) |

**Restart a service:**
```bash
launchctl kickstart -k gui/$(id -u)/ai.openclaw.live-executor
launchctl kickstart -k gui/$(id -u)/ai.openclaw.realtime-engine
launchctl kickstart -k gui/$(id -u)/ai.openclaw.sim-engine
```

**Check if running:**
```bash
pgrep -f "live-executor" && echo "✅ live-executor" || echo "❌ live-executor"
pgrep -f "realtime-daemon" && echo "✅ realtime-engine" || echo "❌ realtime-engine"
```

---

## Coin Universe (19 coins)

**Top watchlist (8):** BTCUSDT, ETHUSDT, SOLUSDT, BNBUSDT, ADAUSDT, DOGEUSDT, XRPUSDT, DOTUSDT

**Extended (11):** AVAXUSDT, APTUSDT, SUIUSDT, FILUSDT, UNIUSDT, LINKUSDT, NEARUSDT, ARBUSDT, OPUSDT, INJUSDT, LTCUSDT

**Correlation groups (max 1 open per group):**
- Group 1: `BTCUSDT, ETHUSDT`
- Group 2: `SOLUSDT, BNBUSDT, ADAUSDT`
- Group 3: `AVAXUSDT, APTUSDT, ARBUSDT, OPUSDT, NEARUSDT`
- Group 4: `DOGEUSDT, LTCUSDT`

---

## Risk Rules (hardcoded in executor)

- `TRAIL_PCT = 0.025` (2.5% trailing stop, activates after TP1)
- `MAX_SIGNAL_AGE_MIN = 20` (signal stale threshold)
- `MAX_HOLD_MIN = 60` (time stop)
- `MAX_OPEN_POSITIONS = 3`
- `MAX_PORTFOLIO_RISK = 0.06` (6% total deployed)
- Kelly fraction: 25% (Fractional Kelly)
- Tier A: 3-4% capital | Tier B: 1.5-2.5% | Tier C: skip
- Circuit breaker: ≥3 consecutive losses → no new entries
- Cooldown: 30 min per symbol after close
- Halal constraint: LONG ONLY, no futures, no leverage, no short

**Position exit priority:**
1. TP2 (full exit at target)
2. Trailing stop (after TP1, `trail_sl = high_water × 97.5%`, only if > entry)
3. Time stop (>60 min → market sell)
4. SL software check (backup if algo SL fails)

---

## Signal Status Codes

| Code | Meaning |
|---|---|
| `PAPER_TRADE` | Executable signal (executor will trade) |
| `NO_TRADE` | Signal evaluated, rejected |
| `SKIPPED_STALE` | Signal > 20 min old |
| `SKIPPED_CORR` | Same correlation group already open |
| `SKIPPED_COOLDOWN` | Symbol in 30-min cooldown |
| `SKIPPED_MAXPOS` | Already 3 open positions |
| `SKIPPED_DUPE` | Duplicate signal for same symbol |
| `SKIPPED_TIER_C` | Low EV signal |
| `SKIPPED_PUMP_DUMP` | Pump/dump pattern detected |
| `SKIPPED_NOISY_WINDOW` | High volatility noise window |
| `FILLED` | Buy order executed |
| `CLOSED_TP` | TP1 partial exit |
| `CLOSED_TP2` | Full exit at TP2 |
| `CLOSED_TRAIL` | Trailing stop triggered |
| `CLOSED_TIME` | 60-min time stop |
| `CLOSED_SL` | Stop-loss triggered |
| `CLOSED_BREAKEVEN` | SL moved to breakeven, stopped |
| `CLOSED_MANUAL` | Manual intervention |

---

## Build & Test

```bash
cd engine

# Build all
cargo build --release

# Test (must be 47+ passing, 0 warnings)
cargo test 2>&1

# Build specific binary
cargo build --release --bin live-executor
cargo build --release --bin sim-engine
```

**Current test count:** 47 tests | **Warnings:** 0

---

## Common Ops Commands

```bash
# Live balance + open positions (real OKX data)
bash scripts/okx_monitor.sh

# Today's PnL from DB
sqlite3 data/trading-journal/live_positions.db \
  "SELECT trade_date, starting_equity, realized_pnl, consecutive_losses, trade_count
   FROM daily_stats ORDER BY trade_date DESC LIMIT 3;"

# Recent sim signals
sqlite3 data/trading-journal/sim_history.db \
  "SELECT decision, setup_idea, inserted_at FROM simulation_events
   ORDER BY inserted_at DESC LIMIT 5;"

# Recent executor log
tail -50 /tmp/openclaw/live-executor.log

# Market pulse (realtime cache)
python3 -c "
import json, datetime
d=json.load(open('data/trading-journal/realtime_cache.json'))
ts=d.get('generated_at','')
gen=datetime.datetime.fromisoformat(ts)
if gen.tzinfo is None: gen=gen.replace(tzinfo=datetime.timezone.utc)
age=int((datetime.datetime.now(datetime.timezone.utc)-gen).total_seconds())
print(f'Cache age: {age}s')
for sym,v in d.get('symbols',{}).items():
    price=v.get('price',0); rsi=v.get('rsi14',0); vol=v.get('volume_ratio',0)
    ema20=v.get('ema20',0); ema50=v.get('ema50',0)
    align='✅' if (price>ema20 and ema20>ema50) else '❌'
    print(f'{sym}: {price:.2f} EMA{align} RSI={rsi:.0f} vol={vol:.2f}x')
"
```

---

## Key Design Decisions

- **Spot only, long only**: OKX `acctLv=1` (Simple Spot) → no algo conditional orders on futures, no margin
- **No `place_tp_limit`**: OKX freezes full coin balance for SL algo → TP limit order always fails. Executor polling handles TP instead.
- **`cancel_pending_algos()` before all exits**: Must cancel SL algo before selling, otherwise OKX blocks the sell (balance frozen by algo)
- **`get_avail_coin()` for sell qty**: Queries real available balance post-fee, prevents qty mismatch
- **Dual WS in realtime-engine**: Public WS (ticker/books) + Business WS (candles, HMAC authenticated)
- **H4 as primary indicator timeframe**: RSI/EMA/ATR/ADX from 4H candles — balances signal quality vs noise
- **Confirmed candles only** (`confirm=="1"`): Prevents duplicate deque entries from mid-candle pushes
- **Persist cache ≤1x/second**: ~40 msgs/sec from 19 coins × 4 channels; write-per-message would spike CPU

---

## Planned Features

### Profit Milestone Tracker
Notif otomatis ke WhatsApp setiap equity tembus threshold tertentu — reminder untuk TP sebagian profit ke cold wallet.

- Config: `baseline_equity` + `milestone_step_usdt` di `account_config.json`
- Logic: setiap `equity - baseline >= milestone_step × n` → kirim WA
- Tujuan: disiplin lock profit, hindari "kertas profit" yang belum pernah dipegang
- Detail implementasi: lihat `OPS_STATE.md` → Profit Milestone Tracker

### Dual Exchange Support (OKX + Binance)
- Abstraction layer `Exchange` trait di Rust
- 2 instance executor berjalan paralel dengan config masing-masing
- Untuk diversifikasi counterparty risk di modal besar

---

## References

- `references/trading-playbook.md` — compact discipline & checklist
- `references/setup-scoring.md` — A/B/C signal grading criteria
- `references/pro-trader-methods.md` — desk-style execution standards
- `OPS_STATE.md` — current ops snapshot (update after major changes)
