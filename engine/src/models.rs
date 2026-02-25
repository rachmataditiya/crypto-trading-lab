use chrono::{DateTime, Utc};

/// Parsed per-asset data from the scan file.
#[derive(Debug, Clone)]
pub struct CoinData {
    pub symbol: String,
    pub grade: char,
    pub score: i32,
    pub price: f64,
    pub ret_24h: f64,
    pub ret_3d: f64,
    pub ret_7d: f64,
    pub ema20: f64,
    pub ema50: f64,
    pub rsi14: f64,
    pub atr14_pct: f64,
    pub adx14: f64,
    pub range_pos_7d: f64,
    pub volume_ratio: f64,
    pub regime: String,
}

/// Market-level summary from the scan file.
#[derive(Debug, Clone)]
pub struct MarketSnapshot {
    pub timestamp: String,
    pub market_regime: String,
    pub breadth_above_ema50: usize,
    pub breadth_total: usize,
    pub coins: Vec<CoinData>,
    pub watchlist: Vec<String>,
}

/// Signal report parsed from crypto_signal_latest.md.
#[derive(Debug, Clone)]
pub struct SignalReport {
    pub market_mode: String,
    pub btc_eth_alert: String,
    pub watchlist: Vec<WatchlistEntry>,
}

#[derive(Debug, Clone)]
pub struct WatchlistEntry {
    pub symbol: String,
    pub grade: char,
    pub score: i32,
    pub ret_24h: f64,
    pub ret_3d: f64,
    pub volume_ratio: f64,
}

#[derive(Debug, Clone)]
pub struct NewsContext {
    pub source: String,
    pub sentiment: f64,      // -1..1
    pub catalyst_score: f64, // 0..1
    pub headline: String,
}

impl Default for NewsContext {
    fn default() -> Self {
        Self {
            source: "none".to_string(),
            sentiment: 0.0,
            catalyst_score: 0.5,
            headline: "no-news-context".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProbabilityMetrics {
    pub prior_up: f64,
    pub posterior_up: f64,
    pub p_up: f64,
    pub p_down: f64,
    pub confidence: f64,
    pub expected_value_r: f64,
    pub tech_likelihood: f64,
    pub news_likelihood: f64,
    pub feature_score: f64,
}

/// The decision output for one simulation cycle.
#[derive(Debug, Clone)]
pub enum Decision {
    NoTrade {
        rationale: String,
    },
    PaperTrade {
        symbol: String,
        direction: Direction,
        entry_zone: String,
        invalidation: String,
        tp1: String,
        tp2: String,
        risk_pct: f64,
        rationale: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Direction {
    Long,
    Short,
}

impl std::fmt::Display for Direction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Direction::Long => write!(f, "Long"),
            Direction::Short => write!(f, "Short"),
        }
    }
}

/// A simulated open position (paper only).
#[derive(Debug, Clone)]
pub struct PaperPosition {
    pub symbol: String,
    pub direction: Direction,
    pub entry_price: f64,
    pub stop_price: f64,
    pub tp_price: f64,
    pub size_usdt: f64,
    pub opened_at: DateTime<Utc>,
}

/// Accumulated daily simulation state.
#[derive(Debug, Clone)]
pub struct SimState {
    pub open_positions: Vec<PaperPosition>,
    pub daily_r: f64,
    pub trade_count: usize,
    pub kill_switch_hit: bool,
}

impl SimState {
    pub fn new() -> Self {
        Self {
            open_positions: Vec::new(),
            daily_r: 0.0,
            trade_count: 0,
            kill_switch_hit: false,
        }
    }
}

/// Full simulation result for one cycle.
#[derive(Debug, Clone)]
pub struct SimResult {
    pub hour_label: String,
    pub regime: String,
    pub watchlist_top3: Vec<WatchlistEntry>,
    pub setup_idea: String,
    pub quality_gate: String,
    pub checklist: Vec<String>,
    pub news_context: NewsContext,
    pub probability: ProbabilityMetrics,
    pub decision: Decision,
}
