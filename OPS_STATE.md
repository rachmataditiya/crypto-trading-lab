# Crypto Trading Lab — Ops State (Persistent Snapshot)

Last updated: 2026-02-27 13:35 WIB (Asia/Jakarta)

---

## Current Operating Mode
- Engine mode: **live** — OKX SPOT, **LONG ONLY** (syariat-compliant, no futures/leverage/short)
- Live executor: `engine/target/release/live-executor` (LaunchAgent: `ai.openclaw.live-executor`)
- Realtime engine: `engine/target/release/sim-engine realtime-daemon` (LaunchAgent: `ai.openclaw.realtime-engine`)
- Sim engine: `engine/target/release/sim-engine` (LaunchAgent: `ai.openclaw.sim-engine`, every 5 min)
- Crypto scan: `engine/target/release/crypto-scan-rs` (LaunchAgent: `ai.openclaw.crypto-scan`, every 2 hr)

---

## Capital & Risk State
- **Balance:** ~122 USDT (check with `bash scripts/okx_monitor.sh`)
- **Risk per trade:** 2% (Kelly 25% fractional)
- **Max daily drawdown:** 10% → kill switch
- **Circuit breaker:** ≥3 consecutive losses → no new entries
- **Max open positions:** 3 concurrent
- **Max portfolio risk deployed:** 6%
- **Max hold:** 60 minutes → automatic time-stop market sell

---

## Watchlist (19 coins)

**Top 8 (main scanner):** BTCUSDT, ETHUSDT, SOLUSDT, BNBUSDT, ADAUSDT, DOGEUSDT, XRPUSDT, DOTUSDT

**Extended 11:** AVAXUSDT, APTUSDT, SUIUSDT, FILUSDT, UNIUSDT, LINKUSDT, NEARUSDT, ARBUSDT, OPUSDT, INJUSDT, LTCUSDT

**Correlation groups (max 1 open per group):**
- Group 1: BTCUSDT, ETHUSDT
- Group 2: SOLUSDT, BNBUSDT, ADAUSDT
- Group 3: AVAXUSDT, APTUSDT, ARBUSDT, OPUSDT, NEARUSDT
- Group 4: DOGEUSDT, LTCUSDT

---

## Architecture (current, stable)

```
OKX REST + WS
  │
  ▼
realtime-engine (KeepAlive)
  ├── Public WS: ticker + books5 (19 coins, 38 channels)
  ├── Business WS: candle1m/1H/4H/1D (19 coins, 76 channels, HMAC auth)
  ├── REST seed: 120×H1, 120×H4, 60×H6, 60×H12, 100×D1 at startup
  ├── Indicators: RSI14, EMA20, EMA50, ATR14, ADX14 from H4 candles
  ├── AMS Model: per-coin ρ (regime), D (divergence), E_trend, E_reversion, E_final
  └── Output: realtime_cache.json (persisted ≤1x/second, includes ams_* fields)
  │
  ▼
sim-engine (every 300s)
  ├── Reads realtime_cache.json
  ├── AMS tiers: A (E_trend>0.30, align≥3) / B (E_trend>0.15) / R (E_rev>0.08, D>0.15) / C
  ├── Tier R = mean-reversion: 0.5x Kelly, max 45min hold, EV=1.2R
  ├── Gates: ams_gate, regime, pump/dump, stale, noisy window
  └── Output: sim_history.db (decision = PAPER_TRADE | NO_TRADE)
  │
  ▼
live-executor (every 60s)
  ├── Reads PAPER_TRADE signals (last 20 min, ordered by EV desc)
  ├── Gates: correlation, cooldown (30min), circuit-breaker, portfolio risk
  ├── Kelly sizing → OKX market buy
  ├── Post-fill: query actual coin balance (post-fee), place SL algo
  ├── 60s poll loop:
  │   ├── TP2 check → cancel algos → market sell (full)
  │   ├── Trail stop (after TP1, 2.5% from high_water, only > entry)
  │   ├── Time stop (>60 min → market sell)
  │   └── SL software backup
  ├── TP1: cancel algos → sell 50% → re-place SL at breakeven
  └── Output: live_positions.db
```

---

## Executor Logic Constants (live_executor.rs)

| Constant | Value | Note |
|---|---|---|
| `TRAIL_PCT` | 0.025 | 2.5% trailing stop |
| `MAX_SIGNAL_AGE_MIN` | 20 | Signal stale threshold |
| `max_hold_min(sym)` | BTC/ETH=120, Mid=90, Others=60 | Per-coin time stop |
| `POLL_INTERVAL_SECS` | 60 | Main loop interval |
| `MAX_OPEN_POSITIONS` | 3 | Concurrent cap |
| `MAX_PORTFOLIO_RISK` | 0.06 | 6% total deployed |

---

## Bug History (fixes shipped)

| Bug | Fix |
|---|---|
| SL trigger not calling `market_sell()` | Fixed — SL now executes sell |
| `market_sell()` returned `()` → stranded positions | Changed to `bool` return |
| Qty rounding — floor missing | Added `qty_floor()` helper |
| Realtime indicators hardcoded | Fixed — real RSI14, ATR14, ADX14 |
| ADX additive bias (`dx*0.7 + 15`) | Removed — clamped 10-50 |
| Stale signal → chasing bad entry | `stale_decision_long()` → DYNAMIC/CHASING/DUMPED |
| Sell qty mismatch | `market_sell()` uses `get_avail_coin()` real balance |
| Buy qty mismatch (post-fee) | Post-fill queries actual coin balance |
| TP limit order always failing | Removed `place_tp_limit()` — OKX freezes balance for SL algo |
| TP sell stuck in loop | Added `cancel_pending_algos()` before all exit sells |
| No trailing stop | Added: 2.5% trail from `high_water`, activates after TP1 |
| PnL inaccurate at TP2 | Uses `current` price (market fill) not fixed TP2 level |
| No 30-min cooldown | Added `cooldown_active()` — blocks same symbol re-entry |
| OKX data source down | Replaced cryptodatadownload.com → OKX `/api/v5/market/candles` |
| Only 5 coins scanned | Expanded to 20 coins across all modules |
| Realtime only 1m candles | Full H1/H4/D1 via Business WS (confirmed candles only) |
| EMA was SMA | Fixed — proper EMA with alpha = 2/(n+1), warm-up period |
| `avail=0` → OKX borrow error | Return `false` immediately, retry next poll |
| Dead code warnings | `#[allow(dead_code)]` on `Signal` struct |
| Ghost position loop (780+ min!) | Added ghost detection: if `avail < 0.001` at cycle start → auto-close DB as `CLOSED_GHOST` |
| `journal_closed` wrong label for TP2/Trail/Breakeven | Fixed — all 7 exit types now have correct label + specific lesson learned |
| `is_risk_ok` not testable (inline) | Extracted as pure function — 10 unit tests added |
| Time-stop hardcoded 60min semua coin | Per-coin `max_hold_min()`: BTC/ETH=120min, mid-cap=90min, high-beta=60min |
| AMS Tier B coin never selected (DOT Tier B filtered before AMS check) | `strategy.rs` two-pass: filter ALL coins for AMS Tier A/B/R first → sort by `hmtf_entry_score` → Bayesian top-3 fallback if none qualified. `sel=ams/bayes` logged in quality_gate. 3 regression tests added. |

---

## Test Coverage

| # | Category | Count |
|---|---|---|
| 1-2 | Stale signal & dynamic entry | 5 |
| 3 | SL/TP recalc from fill | 3 |
| 4 | `qty_floor` basic | 3 |
| 5 | Kelly risk fraction | 3 |
| 6 | Signal tier | 3 |
| 7 | `should_trade` gate | 4 |
| 8 | Halal/long-only | 2 |
| 9 | Coin max alloc | 4 |
| 10 | Dynamic TP multiplier | 3 |
| 11 | TP2 calc | 3 |
| 12 | Partial exit math + pump/dump | 12 |
| 13 | Trail stop (`compute_trail_sl`) | 6 |
| 14 | Corr group blocked (in-memory SQLite) | 9 |
| 15 | Stale boundary exactness | 8 |
| 16 | Qty floor edge cases | 2 |
| 17 | Trail PCT constant verification | (merged into group 13) |
| 18 | Pump/exhaustion (temp cache) | 6 |
| 19 | Coin max allocation full | 9 |
| 20 | `compute_sl_tp_from_fill` edge cases | 3 |
| 21 | Kelly tier C + zero base | 2 |
| 22 | Circuit breaker (`is_risk_ok`) | 10 |
| 20 | Macro EMA aligned guard | 5 |
| 21 | Daily CLOSED_TIME re-entry guard | 6 |
| **Total live-executor** | | **118 passing** |
| realtime (confirmed_close + push_close) | 9 |
| sim-engine lib (strategy, hmtf, ams_tracker, etc.) | 89 |
| strategy AMS-first regression | 3 |
| strategy macro EMA gate regression | 2 |
| **Grand total** | | **216 passing, 0 warnings** |

---

## Directives (Owner Instructions — WAJIB)

1. **SPOT LONG ONLY** — no shorts, no futures, no leverage (halal)
2. **Watchlist:** BTCUSDT, ETHUSDT, SOLUSDT, BNBUSDT, ATOMUSDT + extended 14
3. **Lesson learned detail** setiap trade event (open/close/skip)
4. **Kembangkan algoritma terus** — catat improvement ideas di sini
5. **Fast trading:** max 60 menit hold, TP1 langsung partial exit
6. **Max capital:** 100 USDT aktif, risk 2%/trade, kill switch -3R/hari

---

## Cron Jobs (OpenClaw)

| Cron ID | Schedule | Task |
|---|---|---|
| `23dd9ce1` | 05:00 WIB daily | Brief morning check |
| `a12ad6a3` | 17:00 WIB daily | PnL summary → WA +6281268289854 |
| `2210036f` | 05:00 WIB daily | Reset preset to aggressive |

---

## Profit Milestone Tracker (Planned Feature)

Ketika equity tembus threshold tertentu, sistem kirim notif WhatsApp sebagai reminder untuk TP sebagian profit ke cold wallet.

### Konsep
```
Modal kerja tetap : $X di OKX (tidak berubah)
Profit threshold  : setiap +$50K profit di atas modal awal
Action saat tembus:
  → 70% profit tetap di OKX (compound)
  → 30% profit → notif "saatnya TP ke cold wallet"
```

### Implementasi (belum dibangun)
- Tambah field `baseline_equity` di `account_config.json` (modal awal yang jadi acuan)
- Tambah field `milestone_step_usdt` (interval notif, misal 50000)
- Tambah field `milestone_last_notified` (milestone terakhir yang sudah dinotif)
- Cron job atau heartbeat check equity harian
- Kalau `equity - baseline >= milestone_step * n` dan belum dinotif → kirim WA

### Contoh notif
> ⚠️ *Profit Milestone!* Equity tembus $550K (+$50K dari modal awal $500K).
> Pertimbangkan TP ~$15K ke cold wallet untuk lock profit.
> Sisa compound: $35K tetap di OKX.

### File yang perlu diubah
- `account_config.json` — tambah milestone config
- `scripts/okx_monitor.sh` — tambah milestone check
- OpenClaw cron — trigger notif WA saat milestone tercapai

---

## Algorithm Improvement Ideas (Backlog)

- [x] ~~Add more test coverage~~ → 101 live-executor + 37 realtime = 138 tests total, 0 warnings
- [ ] `closed_at` column in `open_positions` for accurate cooldown (currently uses `opened_at`)
- [ ] Suppress dead code warnings for unused Signal fields
- [ ] Multi-TF confluence: require H1 + H4 both aligned before Tier A
- [ ] Volume spike confirmation: reject if volume < 0.5x baseline even for Tier A
- [ ] News sentiment gate: pause entries during extreme macro events
- [ ] Backtesting loop: compare live results vs sim daily for drift detection
- [ ] Dynamic risk sizing: scale up when confidence > 0.7, scale down when mixed regime

---

## OKX API Notes

- `acctLv=1` = Simple Spot → no algo conditional orders on futures, no margin
- API key: `okx_config.json` (no withdraw permission)
- Required header: `User-Agent: okx-rs-client/1.0`
- WS URLs: public=`wss://ws.okx.com:8443/ws/v5/public`, business=`wss://ws.okx.com:8443/ws/v5/business`
- WS rate limits: 3 subscribe msgs/sec, 480 sub/unsub/login per hour per connection
- Cloudflare WARP aktif di Mac mini — OKX always accessible

---

## Notes

- `memory_search` may fail (OpenAI embeddings 401 error — API key pending config)
- If `avail=0` on sell → position retries next 60s cycle; if dust persists → auto-closed as `CLOSED_GHOST` (2026-02-27 fix)
- `journal-writer` and `alert-guard` check `consecutive_losses` from `daily_stats` table
