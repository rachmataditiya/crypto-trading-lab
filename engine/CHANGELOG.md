# Changelog

## Realtime Data Layer v1

- Added `realtime` config block in `account_config.json`:
  - `realtime.enabled`
  - `realtime.provider` (`okx_public`)
  - `realtime.symbols`
  - `realtime.wsPingIntervalSecs`
  - `realtime.restPollIntervalSecs`
  - `realtime.staleAfterSecs`
  - batching/throttle knobs (`restBatchSize`, `restThrottleMs`)
- Added OKX public connector (`src/realtime.rs`):
  - WebSocket primary subscriptions: `tickers`, `candle1m`, `books5`
  - REST fallback polling when WS disconnects/stales
  - heartbeat ping handling + stale-feed detection
  - reconnect with exponential backoff + jitter
- Added rate-limit-aware fallback scheduler:
  - symbol batching
  - throttle windows
  - graceful degradation counters on 429/network errors
- Added local realtime cache/state store:
  - `data/trading-journal/realtime_cache.json`
  - latest per-symbol market features + health telemetry
- Integrated simulator to prefer realtime cache in paper mode, with safe fallback to existing file-based flow.
- Added CLI command: `realtime-daemon`
- Updated README with concise realtime section + safe runbook.

Safety note: data source uses **public read-only OKX market-data endpoints only** (no API key, no private/account/order endpoints, no live order submission).
