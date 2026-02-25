use crate::models::{MarketSnapshot, SignalReport};

/// Overall market regime.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Regime {
    RiskOn,
    Mixed,
    RiskOff,
}

impl std::fmt::Display for Regime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Regime::RiskOn => write!(f, "Risk-on"),
            Regime::Mixed => write!(f, "Mixed"),
            Regime::RiskOff => write!(f, "Risk-off"),
        }
    }
}

/// Determine market regime from snapshot data.
/// Logic: if both BTC and ETH are A-grade → risk-on,
///        if both BTC and ETH are C-grade → risk-off,
///        otherwise → mixed.
pub fn detect_regime(snapshot: &MarketSnapshot) -> Regime {
    let btc = snapshot.coins.iter().find(|c| c.symbol == "BTCUSDT");
    let eth = snapshot.coins.iter().find(|c| c.symbol == "ETHUSDT");

    match (btc, eth) {
        (Some(b), Some(e)) => {
            if b.grade == 'A' && e.grade == 'A' {
                Regime::RiskOn
            } else if b.grade == 'C' && e.grade == 'C' {
                Regime::RiskOff
            } else {
                Regime::Mixed
            }
        }
        _ => Regime::Mixed,
    }
}

/// Parse regime from signal report market_mode string.
pub fn regime_from_signal(signal: &SignalReport) -> Regime {
    match signal.market_mode.to_lowercase().as_str() {
        "risk-on" => Regime::RiskOn,
        "risk-off" => Regime::RiskOff,
        _ => Regime::Mixed,
    }
}

/// BTC/ETH alert string.
pub fn btc_eth_alert(snapshot: &MarketSnapshot) -> String {
    let btc = snapshot.coins.iter().find(|c| c.symbol == "BTCUSDT");
    let eth = snapshot.coins.iter().find(|c| c.symbol == "ETHUSDT");

    match (btc, eth) {
        (Some(b), Some(e)) => {
            if b.grade == 'A' && e.grade == 'A' {
                "breakout-confirmed".to_string()
            } else if (b.ret_24h < -1.5 || b.grade == 'C') && (e.ret_24h < -1.5 || e.grade == 'C') {
                "downside-pressure".to_string()
            } else {
                "none".to_string()
            }
        }
        _ => "none".to_string(),
    }
}

/// Position-size multiplier based on regime.
pub fn regime_size_multiplier(regime: Regime) -> f64 {
    match regime {
        Regime::RiskOn => 1.0,
        Regime::Mixed => 0.5,
        Regime::RiskOff => 0.0, // no new trades in risk-off
    }
}
