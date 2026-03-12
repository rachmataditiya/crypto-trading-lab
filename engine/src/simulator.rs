use crate::config::AccountConfig;
use crate::data_feed;
use crate::models::*;
use crate::risk;
use crate::strategy;
use std::path::Path;

/// Read positions that the sim should treat as "unavailable":
///   1. Currently OPEN (actively held by executor)
///   2. Recently CLOSED within max cooldown window (120 min for TIME exit,
///      30 min for normal close) — executor will SKIPPED_COOLDOWN these anyway,
///      so sim should not waste cycles generating signals for them.
///
/// Both sets are returned as PaperPosition entries; strategy.rs excludes
/// candidates by symbol regardless of whether the position is truly open.
/// Load symbols that were SKIPPED_STALE today from executor's processed_signals.
/// Sim will exclude these from candidate evaluation to stop the SKIPPED_STALE replay loop.
///
/// Root cause: sim generates PAPER_TRADE for a coin that dumped 7% from signal entry.
/// Executor correctly skips it (stale guard). But sim doesn't know price crashed, so it
/// keeps re-generating PAPER_TRADE every 5 min → executor skips again → infinite loop.
///
/// Fix: sim reads executor's processed_signals, excludes any coin with SKIPPED_STALE today.
/// The coin stays excluded until midnight UTC reset or it gets a new valid PAPER_TRADE.
fn load_stale_skipped_symbols(workspace: &Path) -> Vec<String> {
    use rusqlite::Connection;
    let db_path = workspace.join("data/trading-journal/live_positions.db");
    let conn = match Connection::open(&db_path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    // Use today UTC — processed_at is stored as UTC rfc3339
    let today_utc = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let cutoff = format!("{}T00:00:00Z", today_utc);
    let mut stmt = match conn.prepare(
        "SELECT DISTINCT symbol FROM processed_signals \
         WHERE status='SKIPPED_STALE' AND processed_at >= ?1",
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    stmt.query_map([&cutoff], |row| row.get::<_, String>(0))
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

fn load_live_positions(workspace: &Path) -> Vec<PaperPosition> {
    use rusqlite::Connection;
    let db_path = workspace.join("data/trading-journal/live_positions.db");
    let conn = match Connection::open(&db_path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    // Max cooldown window = 120 min (CLOSED_TIME). Use this as the lookback
    // so we catch coins under both cooldown tiers in one query.
    let cutoff = (chrono::Utc::now() - chrono::Duration::minutes(120))
        .to_rfc3339();

    let mut stmt = match conn.prepare(
        "SELECT symbol, entry_price, stop_price, tp1_price, risk_usdt, opened_at \
         FROM open_positions \
         WHERE status='OPEN' \
            OR (status IN ('CLOSED','CLOSED_TIME','CLOSED_GHOST','CLOSED_SL') \
                AND closed_at >= ?1)",
    ) {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    stmt.query_map([&cutoff], |row| {
        let opened_str: String = row.get(5).unwrap_or_default();
        let opened_at = chrono::DateTime::parse_from_rfc3339(&opened_str)
            .map(|d| d.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now());
        Ok(PaperPosition {
            symbol: row.get(0)?,
            direction: Direction::Long, // spot-only, always long
            entry_price: row.get(1)?,
            stop_price: row.get(2)?,
            tp_price: row.get(3).unwrap_or(0.0),
            size_usdt: row.get(4).unwrap_or(0.0),
            opened_at,
        })
    })
    .ok()
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

/// PAPER-ONLY SAFEGUARD: This simulator never makes live API calls.
/// All "trades" are simulated from parsed market data files.
/// No exchange connections, no order submission, no wallet access.

/// Run one simulation cycle: read data → evaluate → produce result.
pub fn run_cycle(
    workspace: &Path,
    config: &AccountConfig,
    state: &mut SimState,
) -> Result<SimResult, String> {
    // Safety check
    assert_eq!(config.mode, "paper", "CRITICAL: only paper mode is allowed");

    // 1. Parse latest market data (realtime cache preferred when enabled)
    let (snapshot, signal) = if config.realtime.enabled {
        match data_feed::parse_realtime_cache(workspace, config.realtime.stale_after_secs) {
            Ok((snap, sig, obs)) => {
                eprintln!("[SIM][REALTIME] using cache: {}", obs);
                (snap, sig)
            }
            Err(e) => {
                eprintln!("[SIM][REALTIME] fallback to file feed: {}", e);
                (
                    data_feed::parse_scan_file(workspace)?,
                    data_feed::parse_signal_file(workspace)?,
                )
            }
        }
    } else {
        (
            data_feed::parse_scan_file(workspace)?,
            data_feed::parse_signal_file(workspace)?,
        )
    };
    let news_context = data_feed::load_news_context(workspace);

    // 2. Sync live open positions from executor DB.
    //    This lets strategy exclude already-held symbols and correctly
    //    evaluate max_open_positions — previously SimState was always empty.
    state.open_positions = load_live_positions(workspace);

    // 2b. Exclude coins that were SKIPPED_STALE today by executor.
    //     These coins dumped from their signal entry price — re-generating
    //     PAPER_TRADE for them is pointless until tomorrow (or price recovers).
    //     Sim removes them from the snapshot so strategy never picks them.
    let stale_symbols = load_stale_skipped_symbols(workspace);
    let mut snapshot = snapshot;
    if !stale_symbols.is_empty() {
        let before = snapshot.coins.len();
        snapshot.coins.retain(|c| !stale_symbols.contains(&c.symbol));
        let after = snapshot.coins.len();
        if before != after {
            eprintln!("[SIM] Excluded {} stale-skipped coins today: {:?}",
                before - after, stale_symbols);
        }
    }

    // 3. Update risk state
    risk::update_state(state, config);

    // 4. Evaluate strategy
    let result = strategy::evaluate(&snapshot, &signal, &news_context, state, config);

    // 5. If trade decision, validate through risk module
    if let Decision::PaperTrade { risk_pct, .. } = &result.decision {
        if let Err(reason) = risk::validate_trade(state, config, *risk_pct) {
            // Override to no-trade if risk validation fails
            return Ok(SimResult {
                hour_label: result.hour_label,
                regime: result.regime,
                watchlist_top3: result.watchlist_top3,
                setup_idea: result.setup_idea,
                quality_gate: result.quality_gate,
                checklist: result.checklist,
                news_context: result.news_context,
                probability: result.probability,
                decision: Decision::NoTrade {
                    rationale: format!("Risk override: {}", reason),
                },
            });
        }
    }

    Ok(result)
}

/// Run a backfill simulation for N hours using current data.
/// Since we only have point-in-time snapshots (not historical bars),
/// backfill re-evaluates the same data N times with different hour labels.
pub fn run_backfill(
    workspace: &Path,
    config: &AccountConfig,
    hours: usize,
) -> Result<Vec<SimResult>, String> {
    let mut state = SimState::new();
    let mut results = Vec::new();
    let now = chrono::Local::now();

    // Sync live open positions so strategy excludes already-held symbols
    state.open_positions = load_live_positions(workspace);
    let stale_symbols = load_stale_skipped_symbols(workspace);

    for h in (0..hours).rev() {
        let ts = now - chrono::Duration::hours(h as i64);
        let (snapshot, signal) = if config.realtime.enabled {
            data_feed::parse_realtime_cache(workspace, config.realtime.stale_after_secs)
                .map(|(a, b, _)| (a, b))
                .unwrap_or((
                    data_feed::parse_scan_file(workspace)?,
                    data_feed::parse_signal_file(workspace)?,
                ))
        } else {
            (
                data_feed::parse_scan_file(workspace)?,
                data_feed::parse_signal_file(workspace)?,
            )
        };
        // Exclude stale-skipped coins (same as run_cycle)
        let mut snapshot = snapshot;
        if !stale_symbols.is_empty() {
            snapshot.coins.retain(|c| !stale_symbols.contains(&c.symbol));
        }
        let news_context = data_feed::load_news_context(workspace);

        risk::update_state(&mut state, config);
        let mut result = strategy::evaluate(&snapshot, &signal, &news_context, &state, config);
        result.hour_label = ts.format("%H:%M").to_string();
        results.push(result);
    }

    Ok(results)
}
