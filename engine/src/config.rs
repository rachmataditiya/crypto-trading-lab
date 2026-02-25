use serde::Deserialize;
use std::fs;
use std::path::Path;

fn default_min_volume_ratio() -> f64 {
    0.5
}
fn default_min_score_to_trade() -> i32 {
    40
}
fn default_min_adx() -> f64 {
    18.0
}
fn default_min_expected_r() -> f64 {
    1.8
}
fn default_preset() -> String {
    "balanced".to_string()
}

fn default_realtime_provider() -> String {
    "okx_public".to_string()
}
fn default_realtime_symbols() -> Vec<String> {
    vec!["BTCUSDT".to_string(), "ETHUSDT".to_string()]
}
fn default_ws_ping_interval_secs() -> u64 {
    15
}
fn default_stale_after_secs() -> u64 {
    30
}
fn default_rest_poll_interval_secs() -> u64 {
    20
}
fn default_rest_batch_size() -> usize {
    4
}
fn default_rest_throttle_ms() -> u64 {
    250
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RealtimeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_realtime_provider")]
    pub provider: String,
    #[serde(default = "default_realtime_symbols")]
    pub symbols: Vec<String>,
    #[serde(default = "default_ws_ping_interval_secs")]
    pub ws_ping_interval_secs: u64,
    #[serde(default = "default_stale_after_secs")]
    pub stale_after_secs: u64,
    #[serde(default = "default_rest_poll_interval_secs")]
    pub rest_poll_interval_secs: u64,
    #[serde(default = "default_rest_batch_size")]
    pub rest_batch_size: usize,
    #[serde(default = "default_rest_throttle_ms")]
    pub rest_throttle_ms: u64,
}

impl Default for RealtimeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: default_realtime_provider(),
            symbols: default_realtime_symbols(),
            ws_ping_interval_secs: default_ws_ping_interval_secs(),
            stale_after_secs: default_stale_after_secs(),
            rest_poll_interval_secs: default_rest_poll_interval_secs(),
            rest_batch_size: default_rest_batch_size(),
            rest_throttle_ms: default_rest_throttle_ms(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountConfig {
    pub mode: String,
    pub execution: String,
    pub base_currency: String,
    pub initial_capital: f64,
    pub max_risk_per_trade_pct: f64,
    pub max_open_positions: usize,
    pub daily_kill_switch_r: f64,
    #[serde(default = "default_min_volume_ratio")]
    pub min_volume_ratio: f64,
    #[serde(default = "default_min_score_to_trade")]
    pub min_score_to_trade: i32,
    #[serde(default = "default_min_adx")]
    pub min_adx: f64,
    #[serde(default = "default_min_expected_r")]
    pub min_expected_r: f64,
    #[serde(default = "default_preset")]
    pub preset: String,
    #[serde(default)]
    pub realtime: RealtimeConfig,
}

impl AccountConfig {
    pub fn load(workspace: &Path) -> Result<Self, String> {
        let path = workspace.join("data/trading-journal/account_config.json");
        let content = fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
        let mut cfg: AccountConfig =
            serde_json::from_str(&content).map_err(|e| format!("Failed to parse config: {}", e))?;

        if cfg.mode != "paper" {
            return Err(format!(
                "SAFETY: mode is '{}', only 'paper' is allowed. Aborting.",
                cfg.mode
            ));
        }
        if cfg.realtime.enabled && cfg.realtime.provider != "okx_public" {
            return Err(format!(
                "Unsupported realtime provider '{}', expected 'okx_public'",
                cfg.realtime.provider
            ));
        }

        cfg.apply_preset();
        Ok(cfg)
    }

    fn apply_preset(&mut self) {
        match self.preset.to_lowercase().as_str() {
            "conservative" => {
                self.max_risk_per_trade_pct = self.max_risk_per_trade_pct.min(1.0);
                self.min_volume_ratio = self.min_volume_ratio.max(0.8);
                self.min_score_to_trade = self.min_score_to_trade.max(50);
                self.min_adx = self.min_adx.max(22.0);
                self.min_expected_r = self.min_expected_r.max(2.2);
            }
            "aggressive" => {
                self.max_risk_per_trade_pct = self.max_risk_per_trade_pct.min(2.0);
                self.min_volume_ratio = self.min_volume_ratio.max(0.35);
                self.min_score_to_trade = self.min_score_to_trade.max(30);
                self.min_adx = self.min_adx.max(14.0);
                self.min_expected_r = self.min_expected_r.max(1.4);
            }
            _ => {
                // balanced (default)
                self.preset = "balanced".to_string();
                self.max_risk_per_trade_pct = self.max_risk_per_trade_pct.min(1.5);
                self.min_volume_ratio = self.min_volume_ratio.max(0.5);
                self.min_score_to_trade = self.min_score_to_trade.max(40);
                self.min_adx = self.min_adx.max(18.0);
                self.min_expected_r = self.min_expected_r.max(1.8);
            }
        }
    }
}
