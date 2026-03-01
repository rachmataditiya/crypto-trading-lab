use crate::indicators;
use crate::models::{CoinData, MarketSnapshot, NewsContext, SignalReport, WatchlistEntry};
use crate::realtime::RealtimeCache;
use crate::sanitize;
use regex::Regex;
use std::fs;
use std::path::Path;

pub fn parse_realtime_cache(
    workspace: &Path,
    max_age_secs: u64,
) -> Result<(MarketSnapshot, SignalReport, String), String> {
    let path = workspace.join("data/trading-journal/realtime_cache.json");
    let content = fs::read_to_string(&path)
        .map_err(|e| format!("Read realtime cache {}: {}", path.display(), e))?;
    let cache: RealtimeCache =
        serde_json::from_str(&content).map_err(|e| format!("Parse realtime cache: {}", e))?;

    let parsed_last_update = chrono::DateTime::parse_from_rfc3339(&cache.health.last_update_at)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc));
    let age_from_timestamp = parsed_last_update
        .map(|dt| (chrono::Utc::now() - dt).num_seconds().max(0) as u64)
        .unwrap_or(cache.health.last_update_age_secs);
    let effective_age = age_from_timestamp.max(cache.health.last_update_age_secs);

    if effective_age > max_age_secs || cache.health.stale_feed {
        return Err(format!(
            "Realtime cache stale (age={}s, stale={})",
            effective_age, cache.health.stale_feed
        ));
    }

    let mut coins: Vec<CoinData> = cache
        .symbols
        .values()
        .map(|s| {
            let ema20 = if s.ema20 > 0.0 { s.ema20 } else { s.price };
            let ema50 = if s.ema50 > 0.0 { s.ema50 } else { s.price };
            let ema20_1h = if s.ema20_1h > 0.0 { s.ema20_1h } else { ema20 }; // fallback to 4H
            let ema50_1h = if s.ema50_1h > 0.0 { s.ema50_1h } else { ema50 };
            let rsi14 = if s.rsi14 > 0.0 { s.rsi14 } else { 50.0 };
            let atr14_pct = if s.atr14_pct > 0.0 { s.atr14_pct } else { 1.0 };
            let adx14 = if s.adx14 > 0.0 { s.adx14 } else { 20.0 };

            let ams_tier_char = if s.hmtf_tier != 'C' { s.hmtf_tier } else { 'C' };
            let mut coin = CoinData {
                symbol: s.symbol.clone(),
                grade: 'C',
                score: 0,
                price: s.price,
                ret_24h: sanitize::ret_pct(s.ret_24h),
                ret_3d: sanitize::ret_pct(s.ret_3d),
                ret_7d: sanitize::ret_pct(s.ret_7d),
                ema20,
                ema50,
                ema20_1h,
                ema50_1h,
                rsi14: rsi14.clamp(0.0, 100.0),
                atr14_pct: atr14_pct.clamp(0.0, 50.0),
                adx14: sanitize::adx(adx14),
                range_pos_7d: 50.0,
                volume_ratio: sanitize::volume_ratio(s.volume_ratio),
                regime: "Realtime".to_string(),
                hmtf_entry_score: s.hmtf_entry_score,
                hmtf_alignment: s.hmtf_alignment,
                hmtf_tier: s.hmtf_tier,
                ams_regime_rho: s.ams_regime_rho,
                ams_regime_label: if s.ams_regime_label.is_empty() { "UNKNOWN".to_string() } else { s.ams_regime_label.clone() },
                ams_divergence_d: s.ams_divergence_d,
                ams_e_trend: s.ams_e_trend,
                ams_e_reversion: s.ams_e_reversion,
                ams_tier: ams_tier_char,
                ams_kelly_multiplier: s.ams_kelly_multiplier,
            };
            coin.score = sanitize::score(indicators::compute_score(&coin));
            coin.grade = indicators::grade_from_score(coin.score);
            coin
        })
        .collect();
    coins.sort_by(|a, b| b.score.cmp(&a.score));

    let watchlist: Vec<String> = coins.iter().take(5).map(|c| c.symbol.clone()).collect();
    let signal_watchlist = coins
        .iter()
        .take(5)
        .map(|c| WatchlistEntry {
            symbol: c.symbol.clone(),
            grade: c.grade,
            score: c.score,
            ret_24h: c.ret_24h,
            ret_3d: c.ret_3d,
            volume_ratio: c.volume_ratio,
        })
        .collect();

    let market_regime =
        if coins.iter().filter(|c| c.price > c.ema50).count() * 2 >= coins.len().max(1) {
            "risk-on".to_string()
        } else {
            "mixed".to_string()
        };

    let snapshot = MarketSnapshot {
        timestamp: cache.generated_at.clone(),
        market_regime,
        breadth_above_ema50: coins.iter().filter(|c| c.price > c.ema50).count(),
        breadth_total: coins.len(),
        coins,
        watchlist,
    };
    let signal = SignalReport {
        market_mode: "realtime".to_string(),
        btc_eth_alert: "none".to_string(),
        watchlist: signal_watchlist,
    };
    Ok((
        snapshot,
        signal,
        format!(
            "ws_connected={}, fallback_active={}, reconnects={}, age={}s",
            cache.health.ws_connected,
            cache.health.fallback_active,
            cache.health.reconnect_count,
            effective_age
        ),
    ))
}

/// Parse `tmp/crypto_scan_latest.txt` into a MarketSnapshot.
pub fn parse_scan_file(workspace: &Path) -> Result<MarketSnapshot, String> {
    let path = workspace.join("tmp/crypto_scan_latest.txt");
    if !path.exists() {
        return Err(format!("Scan file not found: {}", path.display()));
    }
    let content = fs::read_to_string(&path).map_err(|e| format!("Read scan file: {}", e))?;
    parse_scan_content(&content)
}

fn parse_scan_content(content: &str) -> Result<MarketSnapshot, String> {
    let lines: Vec<&str> = content.lines().collect();

    // Line 0: "Crypto Scan v2 (2026-02-19T07:04:26.091626+00:00)"
    let timestamp = lines.first().map(|l| l.to_string()).unwrap_or_default();

    // Line 1: "Market regime: risk-off"
    let market_regime = lines
        .get(1)
        .and_then(|l| l.strip_prefix("Market regime: "))
        .unwrap_or("unknown")
        .to_string();

    // Line 2: "Breadth (above EMA50): 3/15"
    let (breadth_above, breadth_total) = lines
        .get(2)
        .and_then(|l| {
            let re = Regex::new(r"(\d+)/(\d+)").ok()?;
            let caps = re.captures(l)?;
            Some((
                caps[1].parse::<usize>().unwrap_or(0),
                caps[2].parse::<usize>().unwrap_or(0),
            ))
        })
        .unwrap_or((0, 0));

    // Parse coin blocks: each starts with a line like "BTCUSDT"
    let symbol_re = Regex::new(r"^([A-Z]{3,}USDT)$").unwrap();
    let grade_re = Regex::new(r"Grade/Score:\s*([ABC])\s*/\s*(-?\d+)").unwrap();
    let price_re = Regex::new(r"Price:\s*([\d.]+)").unwrap();
    let ret_re =
        Regex::new(r"24h/3d/7d:\s*([+-]?[\d.]+)%\s*/\s*([+-]?[\d.]+)%\s*/\s*([+-]?[\d.]+)%")
            .unwrap();
    let ema_re = Regex::new(r"EMA20/EMA50:\s*([\d.]+)\s*/\s*([\d.]+)").unwrap();
    let ind_re =
        Regex::new(r"RSI14:\s*([\d.]+)\s*\|\s*ATR14%:\s*([\d.]+)\s*\|\s*ADX14:\s*([\d.]+)")
            .unwrap();
    let range_re =
        Regex::new(r"7d range pos:\s*([\d.]+)%\s*\|\s*Volume ratio:\s*([\d.]+)x").unwrap();
    let regime_re = Regex::new(r"Regime:\s*(\w+)").unwrap();

    let mut coins: Vec<CoinData> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if let Some(caps) = symbol_re.captures(lines[i]) {
            let symbol = caps[1].to_string();
            // Collect the next ~6 lines for this coin
            let block: String = lines[i..std::cmp::min(i + 8, lines.len())].join("\n");

            let (grade, score) = grade_re
                .captures(&block)
                .map(|c| {
                    (
                        c[1].chars().next().unwrap_or('C'),
                        c[2].parse::<i32>().unwrap_or(0),
                    )
                })
                .unwrap_or(('C', 0));

            let price = price_re
                .captures(&block)
                .map(|c| c[1].parse::<f64>().unwrap_or(0.0))
                .unwrap_or(0.0);

            let (r24, r3d, r7d) = ret_re
                .captures(&block)
                .map(|c| {
                    (
                        c[1].parse::<f64>().unwrap_or(0.0),
                        c[2].parse::<f64>().unwrap_or(0.0),
                        c[3].parse::<f64>().unwrap_or(0.0),
                    )
                })
                .unwrap_or((0.0, 0.0, 0.0));

            let (ema20, ema50) = ema_re
                .captures(&block)
                .map(|c| {
                    (
                        c[1].parse::<f64>().unwrap_or(0.0),
                        c[2].parse::<f64>().unwrap_or(0.0),
                    )
                })
                .unwrap_or((0.0, 0.0));

            let (rsi, atr_pct, adx) = ind_re
                .captures(&block)
                .map(|c| {
                    (
                        c[1].parse::<f64>().unwrap_or(50.0),
                        c[2].parse::<f64>().unwrap_or(1.0),
                        c[3].parse::<f64>().unwrap_or(10.0),
                    )
                })
                .unwrap_or((50.0, 1.0, 10.0));

            let (range_pos, vol_ratio) = range_re
                .captures(&block)
                .map(|c| {
                    (
                        c[1].parse::<f64>().unwrap_or(50.0),
                        c[2].parse::<f64>().unwrap_or(0.0),
                    )
                })
                .unwrap_or((50.0, 0.0));

            let coin_regime = regime_re
                .captures(&block)
                .map(|c| c[1].to_string())
                .unwrap_or_else(|| "Range".to_string());

            coins.push(CoinData {
                symbol,
                grade,
                score: sanitize::score(score),
                price,
                ret_24h: sanitize::ret_pct(r24),
                ret_3d: sanitize::ret_pct(r3d),
                ret_7d: sanitize::ret_pct(r7d),
                ema20,
                ema50,
                ema20_1h: ema20, // legacy scan: no 1H data, fall back to 4H
                ema50_1h: ema50,
                rsi14: rsi.clamp(0.0, 100.0),
                atr14_pct: atr_pct.clamp(0.0, 50.0),
                adx14: sanitize::adx(adx),
                range_pos_7d: range_pos.clamp(0.0, 100.0),
                volume_ratio: sanitize::volume_ratio(vol_ratio),
                regime: coin_regime,
                hmtf_entry_score: 0.0,
                hmtf_alignment: 0,
                hmtf_tier: 'C',
                ams_regime_rho: 0.0,
                ams_regime_label: "UNKNOWN".to_string(),
                ams_divergence_d: 0.0,
                ams_e_trend: 0.0,
                ams_e_reversion: 0.0,
                ams_tier: 'C',
                ams_kelly_multiplier: 0.0,
            });
        }
        i += 1;
    }

    // Parse watchlist from "Top watchlist" section
    let wl_re = Regex::new(r"^\d+\.\s+(\w+USDT)").unwrap();
    let watchlist: Vec<String> = lines
        .iter()
        .filter_map(|l| wl_re.captures(l).map(|c| c[1].to_string()))
        .collect();

    Ok(MarketSnapshot {
        timestamp,
        market_regime,
        breadth_above_ema50: breadth_above,
        breadth_total: breadth_total,
        coins,
        watchlist,
    })
}

/// Parse `tmp/crypto_signal_latest.md` into a SignalReport.
pub fn parse_signal_file(workspace: &Path) -> Result<SignalReport, String> {
    let path = workspace.join("tmp/crypto_signal_latest.md");
    if !path.exists() {
        return Err(format!("Signal file not found: {}", path.display()));
    }
    let content = fs::read_to_string(&path).map_err(|e| format!("Read signal file: {}", e))?;
    parse_signal_content(&content)
}

fn parse_signal_content(content: &str) -> Result<SignalReport, String> {
    let mode_re = Regex::new(r"Market mode:\s*\*\*(\S+)\*\*").unwrap();
    let alert_re = Regex::new(r"BTC/ETH alert:\s*\*\*(.+?)\*\*").unwrap();
    let entry_re = Regex::new(
        r"(\w+USDT)\s*\|\s*Score\s*([ABC])\((-?\d+)\)\s*\|\s*24h\s*([+-]?[\d.]+)%\s*\|\s*3d\s*([+-]?[\d.]+)%\s*\|\s*vol\s*([\d.]+)x",
    )
    .unwrap();

    let market_mode = mode_re
        .captures(content)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let btc_eth_alert = alert_re
        .captures(content)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| "none".to_string());

    let watchlist: Vec<WatchlistEntry> = entry_re
        .captures_iter(content)
        .map(|c| WatchlistEntry {
            symbol: c[1].to_string(),
            grade: c[2].chars().next().unwrap_or('C'),
            score: sanitize::score(c[3].parse().unwrap_or(0)),
            ret_24h: sanitize::ret_pct(c[4].parse().unwrap_or(0.0)),
            ret_3d: sanitize::ret_pct(c[5].parse().unwrap_or(0.0)),
            volume_ratio: sanitize::volume_ratio(c[6].parse().unwrap_or(0.0)),
        })
        .collect();

    Ok(SignalReport {
        market_mode,
        btc_eth_alert,
        watchlist,
    })
}

/// Read latest news catalyst context from journal markdown.
/// Falls back to neutral context when no block is found.
pub fn load_news_context(workspace: &Path) -> NewsContext {
    let date_str = chrono::Local::now().format("%Y-%m-%d").to_string();
    let journal_path = workspace.join(format!("data/trading-journal/{}.md", date_str));
    let Ok(content) = fs::read_to_string(journal_path) else {
        return NewsContext::default();
    };

    let blocks = split_section_blocks(&content, "## News Catalyst - ");
    let Some(last) = blocks.last() else {
        return NewsContext::default();
    };

    let mut sentiment: f64 = 0.0;
    let lower = last.to_lowercase();
    let positive = [
        "bullish",
        "breakout",
        "upside",
        "buy",
        "inflow",
        "etf inflow",
        "positive",
    ];
    let negative = [
        "bearish",
        "breakdown",
        "downside",
        "sell",
        "outflow",
        "hack",
        "liquidation",
        "negative",
    ];

    for k in positive {
        if lower.contains(k) {
            sentiment += 0.2;
        }
    }
    for k in negative {
        if lower.contains(k) {
            sentiment -= 0.2;
        }
    }
    sentiment = sentiment.clamp(-1.0, 1.0);

    let catalyst_score = (0.5 + sentiment * 0.4).clamp(0.0, 1.0);
    let headline = last
        .lines()
        .nth(1)
        .unwrap_or("news-catalyst")
        .trim()
        .to_string();

    NewsContext {
        source: "journal".to_string(),
        sentiment,
        catalyst_score,
        headline,
    }
}

fn split_section_blocks(content: &str, section_prefix: &str) -> Vec<String> {
    let lines: Vec<&str> = content.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].starts_with(section_prefix) {
            let start = i;
            i += 1;
            while i < lines.len() && !lines[i].starts_with("## ") {
                i += 1;
            }
            out.push(lines[start..i].join("\n"));
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_scan_sample() {
        let sample = r#"Crypto Scan v2 (2026-02-19T07:04:26+00:00)
Market regime: risk-off
Breadth (above EMA50): 3/15

BTCUSDT
- Grade/Score: C / -50
- Price: 67503.520000
- 24h/3d/7d: -2.02% / -3.48% / -3.46%
- EMA20/EMA50: 67832.715955 / 68214.760807
- RSI14: 43.59 | ATR14%: 0.94 | ADX14: 15.95
- 7d range pos: 40.7% | Volume ratio: 0.28x
- Regime: Range

Top watchlist (v2):
1. BTCUSDT (C / score -50)
"#;
        let snap = parse_scan_content(sample).unwrap();
        assert_eq!(snap.market_regime, "risk-off");
        assert_eq!(snap.breadth_above_ema50, 3);
        assert_eq!(snap.coins.len(), 1);
        assert_eq!(snap.coins[0].symbol, "BTCUSDT");
        assert_eq!(snap.coins[0].grade, 'C');
        assert_eq!(snap.coins[0].score, -50);
    }
}
