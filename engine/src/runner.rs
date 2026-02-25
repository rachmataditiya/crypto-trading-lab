use crate::config::AccountConfig;
use crate::journal;
use crate::models::SimState;
use crate::simulator;
use std::path::Path;
use std::thread;
use std::time::Duration;

/// Run once: single simulation cycle.
pub fn run_once(workspace: &Path) -> Result<(), String> {
    let config = AccountConfig::load(workspace)?;
    let mut state = SimState::new();

    eprintln!("[SIM] Mode: once | Paper-only safeguard active");
    eprintln!(
        "[SIM] Config: preset={}, risk/trade={:.1}%, max_pos={}, kill_switch={}R",
        config.preset,
        config.max_risk_per_trade_pct,
        config.max_open_positions,
        config.daily_kill_switch_r
    );

    let result = simulator::run_cycle(workspace, &config, &mut state)?;

    // Print summary to stdout
    print_result_summary(&result);

    // Append to journal
    journal::append_journal(workspace, &result)?;

    eprintln!("[SIM] Done.");
    Ok(())
}

/// Run as daemon: repeat every N minutes.
pub fn run_daemon(workspace: &Path, interval_minutes: u64) -> Result<(), String> {
    let config = AccountConfig::load(workspace)?;
    let mut state = SimState::new();

    eprintln!(
        "[SIM] Mode: daemon (every {} min) | Paper-only safeguard active",
        interval_minutes
    );
    eprintln!(
        "[SIM] Config: preset={}, risk/trade={:.1}%, max_pos={}, kill_switch={}R",
        config.preset,
        config.max_risk_per_trade_pct,
        config.max_open_positions,
        config.daily_kill_switch_r
    );

    loop {
        match simulator::run_cycle(workspace, &config, &mut state) {
            Ok(result) => {
                print_result_summary(&result);
                if let Err(e) = journal::append_journal(workspace, &result) {
                    eprintln!("[JOURNAL ERROR] {}", e);
                }
            }
            Err(e) => {
                eprintln!("[SIM ERROR] {}: will retry next cycle", e);
            }
        }

        if state.kill_switch_hit {
            eprintln!("[SIM] Kill switch active — daemon pausing until next day");
        }

        eprintln!("[SIM] Sleeping {} minutes...", interval_minutes);
        thread::sleep(Duration::from_secs(interval_minutes * 60));
    }
}

/// Run backfill: simulate N past hours using current data.
pub fn run_backfill(workspace: &Path, hours: usize) -> Result<(), String> {
    let config = AccountConfig::load(workspace)?;

    eprintln!(
        "[SIM] Mode: backfill ({} hours) | Paper-only safeguard active",
        hours
    );

    let results = simulator::run_backfill(workspace, &config, hours)?;

    for result in &results {
        print_result_summary(result);
        journal::append_journal(workspace, result)?;
    }

    eprintln!(
        "[SIM] Backfill complete: {} cycles processed.",
        results.len()
    );
    Ok(())
}

fn print_result_summary(result: &crate::models::SimResult) {
    println!("━━━ {} WIB ━━━", result.hour_label);
    println!("Regime: {}", result.regime);
    print!("Watchlist: ");
    let wl: Vec<String> = result
        .watchlist_top3
        .iter()
        .map(|w| format!("{}({}{})", w.symbol, w.grade, w.score))
        .collect();
    println!("{}", wl.join(", "));
    println!("Setup: {}", result.setup_idea);
    println!(
        "Prob: P(up)={:.2} P(down)={:.2} conf={:.2} EV={:+.2}R",
        result.probability.p_up,
        result.probability.p_down,
        result.probability.confidence,
        result.probability.expected_value_r
    );
    match &result.decision {
        crate::models::Decision::NoTrade { rationale } => {
            println!("Decision: NO_TRADE");
            println!("Reason: {}", rationale);
        }
        crate::models::Decision::PaperTrade {
            symbol,
            direction,
            risk_pct,
            rationale,
            ..
        } => {
            println!(
                "Decision: PAPER_TRADE {} {} (risk {:.1}%)",
                symbol, direction, risk_pct
            );
            println!("Reason: {}", rationale);
        }
    }
    println!();
}
