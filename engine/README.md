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
