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
and mean-reversion modes. A fixed-weight model cannot serve both — they are
mathematically opposite interpretations of the same data. AMS resolves this
with a continuous regime score $\rho \in (0,1)$.

### Layer 0 — Regime Score $\rho$

$$\rho = \sigma\!\left(\frac{\bar{ADX} - 22}{8}\right), \qquad \bar{ADX} = \frac{1}{4}\sum_{\tau \in \{1H,4H,6H,12H\}} ADX_\tau$$

where $\sigma(x) = \dfrac{1}{1+e^{-x}}$ is the sigmoid function.

- $\rho \to 0$: pure ranging / mean-reversion ($\bar{ADX} \ll 22$)
- $\rho = 0.5$: mixed regime, inflection point ($\bar{ADX} = 22$)
- $\rho \to 1$: pure trending ($\bar{ADX} \gg 22$)

### Layer 1 — Adaptive Feature Weights $\mathbf{W}(\rho)$

$$\mathbf{W}(\rho) = \rho \cdot \mathbf{W}_{\text{trend}} + (1-\rho) \cdot \mathbf{W}_{\text{rev}}$$

|  | $W_e$ | $W_r$ | $W_a$ | $W_v$ | $W_p$ |
|--|:-----:|:-----:|:-----:|:-----:|:-----:|
| $\mathbf{W}_{\text{trend}}$ | 0.40 | 0.20 | 0.30 | 0.05 | 0.05 |
| $\mathbf{W}_{\text{rev}}$ | 0.10 | 0.40 | 0.05 | 0.20 | 0.25 |

In trending regime ($\rho \to 1$): EMA gradient $e$ and ADX direction $a$ dominate.
In ranging regime ($\rho \to 0$): RSI $r$, volume $v$, and price position $p$ dominate.

### Layer 2 — Adaptive RSI Signal $r_{\text{adaptive}}$

$$r_{\text{adaptive}}(RSI,\,\rho) = \rho \cdot \underbrace{\tanh\!\left(\frac{RSI-50}{20}\right)}_{r_{\text{trend}}} + (1-\rho) \cdot \underbrace{\tanh\!\left(\frac{35-RSI}{10}\right)}_{r_{\text{rev}}}$$

> **Key insight — signal flip:**
> $$RSI=28,\;\rho=0.9 \;\Rightarrow\; r = -0.80 \quad \text{(bearish, momentum down)}$$
> $$RSI=28,\;\rho=0.1 \;\Rightarrow\; r = +0.51 \quad \text{(oversold bounce opportunity)}$$
> The same indicator value carries opposite meaning across regimes. The sigmoid blend makes this transition continuous and smooth.

### Layer 3 — Adaptive Gate Threshold $\theta(\text{ATR})$

$$\theta(\text{ATR\%}) = 0.10 + 0.10 \cdot \tanh\!\left(\frac{\text{ATR\%}}{1.5}\right)$$

Higher volatility raises the gate threshold automatically, suppressing false signals in turbulent conditions.

### Layer 4 — Per-TF Adaptive Score $s(\tau)$

For each $\tau \in \{1H, 4H, 6H, 12H\}$, five features are normalized to $[-1, +1]$:

$$e = \tanh\!\left(\frac{EMA_{20} - EMA_{50}}{P} \cdot 50\right), \quad r = r_{\text{adaptive}}, \quad a = \text{clamp}\!\left(\frac{ADX-25}{25} \cdot \text{sgn}(DI^+ - DI^-), -1, 1\right)$$

$$v = \tanh\!\left(\ln \frac{V}{V_{\text{avg}}}\right), \quad p = \frac{P - P_{\min,20}}{P_{\max,20} - P_{\min,20}} \cdot 2 - 1$$

$$s(\tau) = W_e \cdot e + W_r \cdot r + W_a \cdot a + W_v \cdot v + W_p \cdot p \;\in [-1,\,+1]$$

### Layer 5 — Divergence Score $D$

$$D = \left| s_{1H} - s_{12H} \right|$$

Measures the "rubber-band stretch" between the fastest and slowest timeframes.
Large $D$ in a ranging market ($\rho < 0.4$) indicates strong mean-reversion potential.

### Layer 6 — Entry Scores

**Adaptive gate chain:**

$$G_\tau = \sigma\!\left(\frac{s_\tau}{\theta_\tau}\right), \qquad \Gamma = G_{12H} \cdot G_{6H} \cdot G_{4H} \in (0,\,1)$$

**Trend component** (macro-gated weighted sum):

$$E_{\text{trend}} = \Gamma \cdot \bigl(0.40\,s_{12H} + 0.25\,s_{6H} + 0.20\,s_{4H} + 0.15\,s_{1H}\bigr)$$

**Reversion component** (divergence × oversold × volume):

$$E_{\text{rev}} = D \cdot \max\!\left(\tanh\frac{35-RSI_{1H}}{10},\;0\right) \cdot \max\!\left(\tanh\ln\frac{V}{V_{\text{avg}}},\;0\right)$$

**Unified entry score:**

$$\boxed{E_{\text{final}} = \rho \cdot E_{\text{trend}} + (1-\rho) \cdot E_{\text{rev}}}$$

### Layer 7 — Regime-Adaptive Tier Assignment

$$\text{alignment} = \frac{1}{4}\sum_\tau \mathbf{1}[s(\tau) > 0]$$

| Regime | Condition | Tier | $m_{\text{tier}}$ | Description |
|--------|-----------|:----:|:-----------------:|-------------|
| $\rho > 0.6$ | $E_{\text{trend}} > 0.30,\; \text{align} \geq 3$ | **A** | $1.0\times$ | Strong trend |
| $\rho > 0.6$ | $E_{\text{trend}} > 0.15,\; \text{align} \geq 3$ | **B** | $1.0\times$ | Moderate trend |
| $\rho < 0.4$ | $E_{\text{rev}} > 0.08,\; D > 0.15$ | **R** | $0.5\times$ | Reversion bounce |
| Mixed | Best of A/R | **A/R** | — | Hybrid |
| — | Otherwise | **C** | $0\times$ | No trade |

### Layer 8 — Adaptive Kelly via $C_{\text{recent}}$

Exponentially-weighted average win rate per mode with forgetting factor $\lambda = 0.94$ (half-life $\approx 16$ trades):

$$\hat{W}(t) = \lambda\,\hat{W}(t-1) + (1-\lambda)\cdot\mathbf{1}[\text{win}_t]$$

$$C_{\text{recent}} = \text{clamp}\!\left(\frac{\hat{W}}{W^{\ast}},\;0.5,\;1.5\right), \qquad W^{\ast}_{\text{trend}} = 0.60,\quad W^{\ast}_{\text{rev}} = 0.45$$

$$\boxed{f_{\text{final}} = f^{\ast}_{\text{Kelly}} \cdot m_{\text{tier}} \cdot C_{\text{recent}}}$$

The system automatically scales up when a mode is on a winning streak and reduces exposure during drawdowns — without any manual tuning.

---

### Worked Example: ETH/USDT 01:15 WIB — Missed Opportunity

**Scenario:** All timeframes broadly bearish, but 1H showing extreme oversold + volume spike at support.

**Step 1 — Regime:**

$$\bar{ADX} = \frac{15+14+13+12}{4} = 13.5 \implies \rho = \sigma\!\left(\frac{13.5-22}{8}\right) = \sigma(-1.06) \approx 0.26 \quad \Rightarrow \textbf{RANGING}$$

**Step 2 — Adaptive weights at $\rho = 0.26$:**

$$W_r = 0.26(0.20) + 0.74(0.40) = 0.35 \quad \text{(RSI now dominant)}$$

**Step 3 — RSI flip:**

$$r_{\text{adaptive}}(28,\,0.26) = 0.26\cdot\tanh(-1.10) + 0.74\cdot\tanh(0.70) = -0.21 + 0.44 = \mathbf{+0.24} \;\text{(BULLISH!)}$$

**Step 4 — Divergence:**

$$s_{1H} \approx -0.05, \quad s_{12H} \approx -0.43 \implies D = |-0.05 - (-0.43)| = 0.38 > 0.15 \;\checkmark$$

**Step 5 — Reversion score ($V/V_{\text{avg}} = 3.5$):**

$$E_{\text{rev}} = 0.38 \cdot \tanh(0.70) \cdot \tanh(\ln 3.5) = 0.38 \times 0.60 \times 0.85 = \mathbf{0.194} > 0.08 \;\checkmark$$

**Step 6 — Tier:**

$$\rho = 0.26 < 0.4,\quad E_{\text{rev}} = 0.194 > 0.08,\quad D = 0.38 > 0.15 \implies \textbf{Tier R}$$

**Step 7 — Kelly:**

$$f = 0.5 \times C_{\text{recent,rev}} = 0.5 \times 1.0 = 0.5\times \text{ (half-size, conservative)}$$

**Result:** Engine would have entered at \$1,986 with $TP_1 = +1.2\%$, $TP_2 = +2.5\%$, max 45 min hold.
Actual price reached \$2,050 by 03:00 WIB — **TP2 hit, +2.5% on half position.**

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
