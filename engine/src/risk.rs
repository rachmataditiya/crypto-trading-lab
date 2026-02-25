use crate::config::AccountConfig;
use crate::models::SimState;
use crate::regime::Regime;

/// Check if the daily kill switch has been triggered.
pub fn check_kill_switch(state: &SimState, config: &AccountConfig) -> bool {
    state.daily_r <= config.daily_kill_switch_r
}

/// Update kill switch status in state.
pub fn update_state(state: &mut SimState, config: &AccountConfig) {
    if check_kill_switch(state, config) && !state.kill_switch_hit {
        state.kill_switch_hit = true;
        eprintln!(
            "[RISK] Kill switch triggered: daily R = {:.1} (limit {})",
            state.daily_r, config.daily_kill_switch_r
        );
    }
}

/// Adjusted risk percentage based on regime and confidence.
pub fn adjusted_risk_pct(config: &AccountConfig, regime: Regime, confidence: &str) -> f64 {
    let base = config.max_risk_per_trade_pct;
    let regime_mult = match regime {
        Regime::RiskOn => 1.0,
        Regime::Mixed => 0.5,
        Regime::RiskOff => 0.0,
    };
    let conf_mult = match confidence {
        "strong" => 1.0,
        "moderate" => 0.5,
        _ => 0.0, // weak → no trade
    };
    base * regime_mult * conf_mult
}

/// Calculate position size in USDT given risk % and ATR-based stop distance.
pub fn position_size(capital: f64, risk_pct: f64, entry_price: f64, stop_distance_pct: f64) -> f64 {
    if stop_distance_pct <= 0.0 || risk_pct <= 0.0 {
        return 0.0;
    }
    let risk_amount = capital * (risk_pct / 100.0);
    let stop_distance = entry_price * (stop_distance_pct / 100.0);
    let coins = risk_amount / stop_distance;
    coins * entry_price
}

/// Validate that a proposed trade passes all risk checks.
pub fn validate_trade(
    state: &SimState,
    config: &AccountConfig,
    risk_pct: f64,
) -> Result<(), String> {
    if state.kill_switch_hit {
        return Err("Daily kill switch is active".to_string());
    }
    if state.open_positions.len() >= config.max_open_positions {
        return Err(format!(
            "Max open positions reached ({}/{})",
            state.open_positions.len(),
            config.max_open_positions
        ));
    }
    if risk_pct > config.max_risk_per_trade_pct {
        return Err(format!(
            "Risk {:.1}% exceeds max {:.1}%",
            risk_pct, config.max_risk_per_trade_pct
        ));
    }
    if risk_pct <= 0.0 {
        return Err("Computed risk is zero (regime/confidence too weak)".to_string());
    }
    Ok(())
}
