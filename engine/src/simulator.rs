use crate::config::AccountConfig;
use crate::data_feed;
use crate::models::*;
use crate::risk;
use crate::strategy;
use std::path::Path;

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

    // 2. Update risk state
    risk::update_state(state, config);

    // 3. Evaluate strategy
    let result = strategy::evaluate(&snapshot, &signal, &news_context, state, config);

    // 4. If trade decision, validate through risk module
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
        let news_context = data_feed::load_news_context(workspace);

        risk::update_state(&mut state, config);
        let mut result = strategy::evaluate(&snapshot, &signal, &news_context, &state, config);
        result.hour_label = ts.format("%H:%M").to_string();
        results.push(result);
    }

    Ok(results)
}
