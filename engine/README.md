# Sim Engine — Crypto Paper Trading

Rust-based paper trading simulation engine that reads market scan data,
evaluates trading setups, applies risk controls, and journals decisions.

## Architecture

```
┌──────────────┐     ┌──────────────┐     ┌──────────────┐
│  data_feed   │────>│  indicators  │────>│   strategy   │
│ (scan/signal │     │ (score, ADX, │     │ (evaluate    │
│  file parse) │     │  confidence) │     │  candidates) │
└──────────────┘     └──────────────┘     └──────┬───────┘
                                                 │
┌──────────────┐     ┌──────────────┐     ┌──────▼───────┐
│   journal    │<────│  simulator   │<────│    risk      │
│ (append MD)  │     │ (run cycle)  │     │ (kill switch,│
└──────────────┘     └──────────────┘     │  pos sizing) │
                                          └──────────────┘
```

### Modules

| Module       | Purpose                                          |
|-------------|--------------------------------------------------|
| `config`    | Load `account_config.json`, enforce paper mode   |
| `models`    | Core types: CoinData, SimState, Decision, etc.   |
| `data_feed` | Parse `crypto_scan_latest.txt` and signal report  |
| `indicators`| Compute scores, grades, confidence levels         |
| `regime`    | Market regime detection (risk-on/mixed/risk-off)  |
| `strategy`  | Setup evaluation, direction, entry/exit levels    |
| `risk`      | Position sizing, kill switch, trade validation    |
| `simulator` | Orchestrate simulation cycles                     |
| `journal`   | Append hourly blocks to daily journal markdown    |
| `runner`    | CLI mode handlers (once, daemon, backfill)        |
| `hmtf`      | AMS (Adaptive Multi-regime Signal) model           |
| `ams_tracker`| EWA win-rate tracker for adaptive Kelly sizing    |

## Run Commands

```bash
# Single simulation cycle
cargo run --release -- once

# Daemon mode (every 60 minutes)
cargo run --release -- daemon --interval-minutes 60

# Backfill last 6 hours
cargo run --release -- backfill --hours 6

# Specify workspace explicitly
cargo run --release -- --workspace /path/to/workspace once
```

Or use the wrapper script:
```bash
scripts/sim_engine.sh once
scripts/sim_engine.sh daemon 60
scripts/sim_engine.sh backfill 6
```

## Mathematical Model: AMS (Adaptive Multi-regime Signal)

The AMS model is a "chameleon" that continuously adapts between trend-following
and mean-reversion modes. Unlike static HMTF, AMS uses a sigmoid regime score
to smoothly blend two weight sets, flipping RSI interpretation in ranging markets.

### Layer 0 — Regime Score (rho)

```
rho = sigmoid((mean(ADX_1H, ADX_4H, ADX_6H, ADX_12H) - 22) / 8)
```
- rho -> 0: pure ranging (low ADX across all timeframes)
- rho -> 1: pure trending (high ADX across all timeframes)
- rho = 0.5 at mean ADX = 22 (inflection point)

### Layer 1 — Adaptive Weights W(rho)

```
W(rho) = rho * W_trend + (1 - rho) * W_reversion

W_trend     = [e:0.40, r:0.20, a:0.30, v:0.05, p:0.05]
W_reversion = [e:0.10, r:0.40, a:0.05, v:0.20, p:0.25]
```
In trending: EMA cross (e) and ADX direction (a) dominate.
In ranging: RSI (r), volume (v), and price position (p) dominate.

### Layer 2 — Adaptive RSI r_adaptive(RSI, rho)

```
r_adaptive = rho * tanh((RSI - 50) / 20) + (1 - rho) * tanh((35 - RSI) / 10)
```
**Key insight**: In trending mode, RSI 28 = bearish (momentum down).
In ranging mode, RSI 28 = BULLISH (mean-reversion opportunity). The sigmoid
blend makes this transition continuous.

### Layer 3 — Adaptive Gate Threshold theta(ATR)

```
theta(atr) = 0.10 + 0.10 * tanh(atr / 1.5)
```
Higher volatility raises the bar for gate passage (reduces false signals).

### Layer 4 — Per-TF Adaptive Score

```
s_adaptive(tau) = W.r * r_adaptive + W.e * e + W.a * a + W.v * v + W.p * p
```
Each timeframe (1H, 4H, 6H, 12H) gets scored with regime-adaptive weights.

### Layer 5 — Divergence D

```
D = |s_1H - s_12H|
```
Measures the "rubber-band stretch" between fast and slow timeframes.
Large D in ranging markets = strong mean-reversion signal.

### Layer 6 — Entry Scores

```
Gate: G_tau = sigmoid(s_tau / theta)
Gamma = G_12H * G_6H * G_4H

E_trend = Gamma * (0.40*s_12H + 0.25*s_6H + 0.20*s_4H + 0.15*s_1H)
E_rev   = D * max(tanh((35 - RSI_1H) / 10), 0) * max(tanh(ln(vol_ratio)), 0)
E_final = rho * E_trend + (1 - rho) * E_rev
```

### Layer 7 — Tier Assignment

| Condition | Tier | Description |
|-----------|------|-------------|
| rho > 0.6, E_trend > 0.30, align >= 3 | A | Strong trend |
| rho > 0.6, E_trend > 0.15, align >= 3 | B | Moderate trend |
| rho < 0.4, E_rev > 0.08, D > 0.15 | R | Reversion trade |
| Mixed: A conditions met | A | Trend in mixed regime |
| Mixed: R conditions met | R | Reversion in mixed |
| Otherwise | C | No trade |

Kelly sizing: A/B = 1.0x, R = 0.5x, C = 0.0x (no trade).

### Layer 8 — C_recent EWA (ams_tracker)

```
ewa = LAMBDA * ewa + (1 - LAMBDA) * outcome    (LAMBDA = 0.94)
c_recent = clamp(ewa / target_win_rate, 0.5, 1.5)
kelly_adjusted = base_kelly * c_recent
```
Tracks recent win rates per mode (trend target: 60%, reversion: 45%).
Scales Kelly up/down based on whether the mode has been performing.

### Worked Example: ETH 01:15 UTC

Scenario: All TFs bearish, 1H barely dropping, volume 3.5x spike.

1. **ADX**: 1H=15, 4H=14, 6H=13, 12H=12. Mean=13.5, rho=sigmoid((13.5-22)/8)=sigmoid(-1.06)=0.26 -> RANGING
2. **Weights**: W(0.26) -> r=0.35, e=0.18, a=0.11, v=0.16, p=0.20 (RSI dominates)
3. **RSI 1H=28**: r_adaptive = 0.26*tanh(-1.1) + 0.74*tanh(0.7) = 0.26*(-0.80)+0.74*(0.60) = -0.21+0.44 = +0.24 (BULLISH!)
4. **Scores**: s_1H=-0.05 (mild bear), s_12H=-0.43 (strong bear)
5. **Divergence**: D=|(-0.05)-(-0.43)|=0.38 > 0.15
6. **E_rev**: 0.38 * tanh(0.7) * tanh(ln(3.5)) = 0.38 * 0.60 * 0.85 = 0.194 > 0.08
7. **Tier**: rho=0.26 < 0.4, E_rev=0.194 > 0.08, D=0.38 > 0.15 -> **Tier R**
8. **Kelly**: 0.5 * c_recent_reversion (starts at 1.0) = 0.5x

Result: Engine identifies mean-reversion long opportunity despite all-bearish macro.

## Risk Controls

- **maxRiskPerTradePct**: Max risk per trade (default 2%)
- **maxOpenPositions**: Max concurrent positions (default 3)
- **dailyKillSwitchR**: Daily loss limit in R-multiples (default -3R)
- **No-trade filters**: Skip if confidence is weak or volume < threshold
- **Regime scaling**: Full size in risk-on, half in mixed, zero in risk-off

## Paper → Shadow → Live Design

The engine is designed for progressive deployment:

1. **Paper (current)**: All trades are simulated from file-based market data.
   No exchange API calls. Decisions journaled to markdown.

2. **Shadow (future)**: Connect to exchange WebSocket for real-time data.
   Still no order submission. Compare paper decisions to actual fills.

3. **Live (future)**: Submit real orders via exchange API.
   Requires `mode: "live"` in config (currently blocked by safety check).

The `config.mode` field enforces the current stage. The engine will refuse
to start if mode is not `"paper"`.

## Configuration

`data/trading-journal/account_config.json`:
```json
{
  "mode": "paper",
  "execution": "simulation_from_market_data",
  "baseCurrency": "USDT",
  "initialCapital": 1000,
  "maxRiskPerTradePct": 2,
  "maxOpenPositions": 3,
  "dailyKillSwitchR": -3,
  "realtime": {
    "enabled": false,
    "provider": "okx_public",
    "symbols": ["BTCUSDT", "ETHUSDT", "SOLUSDT"],
    "wsPingIntervalSecs": 15,
    "staleAfterSecs": 30,
    "restPollIntervalSecs": 20,
    "restBatchSize": 4,
    "restThrottleMs": 250
  }
}
```

## Realtime Data Layer v1 (OKX Public, Read-Only)

This engine now supports a **paper-only realtime data layer**:
- Primary feed: OKX public WebSocket (`tickers`, `candle1m`, `books5`)
- Fallback feed: OKX public REST polling when WS is stale/disconnected
- Reconnect: exponential backoff + jitter
- Scheduler: symbol batching + throttle window + graceful degradation on 429/network errors
- Local cache: `data/trading-journal/realtime_cache.json`
- Observability fields in cache health: `last_update_age_secs`, `updated_at`, `reconnect_count`, `fallback_active`, stale flags/counters
- Defensive sanitizers active for outliers (volume ratio, score, ADX, return percentages)

The simulator keeps the existing file-based scan/signal path as fallback, so current cron/service flows stay compatible.

### Safe runbook

1. Keep `mode: "paper"` (mandatory safety gate).
2. Enable realtime config (`realtime.enabled=true`, provider=`okx_public`).
3. Start realtime daemon:
   ```bash
   cargo run --release -- realtime-daemon
   ```
4. In another process, run simulation as usual:
   ```bash
   cargo run --release -- daemon --interval-minutes 60
   ```
5. If realtime cache is stale/unavailable, engine automatically falls back to file-based feed.

## Journal Output

Each simulation cycle appends a block to `data/trading-journal/YYYY-MM-DD.md`:

```markdown
## Hourly Simulation - HH:00 WIB
- Market regime saat ini: **Risk-off**
- Top 3 watchlist:
  1. **MATICUSDT** — Score A(43), 24h +1.25%, 3d +4.43%, vol 0.36x
  ...
- Ide setup simulasi (paper): **...**
- Keputusan: **NO_TRADE** / **PAPER_TRADE**
- Alasan: ...
```

## Troubleshooting / Known-Good Health Checks

**Realtime cache freshness + schema sanity**
```bash
python3 - <<'PY'
import json, datetime
p='/Users/rachmataditiya/.openclaw/workspace/data/trading-journal/realtime_cache.json'
j=json.load(open(p))
print('generated_at=', j.get('generated_at'))
print('health_age=', j.get('health',{}).get('last_update_age_secs'))
print('stale=', j.get('health',{}).get('stale_feed'))
print('symbols=', len(j.get('symbols',{})))
for s,v in list(j.get('symbols',{}).items())[:3]:
    print(s, 'updated_at=', v.get('updated_at'), 'vol=', v.get('volume_ratio'), 'adx=', v.get('adx14'))
PY
```

**Launchd services stable**
```bash
launchctl list | egrep 'ai.openclaw.(realtime-engine|sim-engine)'
plutil -lint ~/Library/LaunchAgents/ai.openclaw.realtime-engine.plist
plutil -lint ~/Library/LaunchAgents/ai.openclaw.sim-engine.plist
```

**Cadence check (sim every 15m)**
```bash
tail -n 80 /tmp/sim-engine-launchd.out.log
tail -n 80 /tmp/sim-engine-daemon.out.log
```

**SQLite sync + calibration report**
```bash
python3 scripts/sync_journal_sqlite.py
python3 scripts/calibration_report.py
# Optional anti-spam bucket floor
FAIL_BUCKET_MIN=3 python3 scripts/calibration_report.py
```
