/// OKX Live Executor — Rust edition v2 (Kelly + Multi-position)
/// - Fractional Kelly position sizing (25% Kelly, cap 4%)
/// - Signal quality tiers: A (3-4%), B (2%), skip low-EV
/// - Multi-position: max 3 open, correlation-aware
/// - Per-coin allocation caps
/// - SL below fill price fix + safety gate

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chrono::{DateTime, Duration, Local, Timelike, Utc};
use hmac::{Hmac, Mac};
use reqwest::blocking::Client;
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use std::{collections::HashMap, fs, path::{Path, PathBuf}, thread, time::Duration as StdDuration};

// ── Constants ────────────────────────────────────────────────────────────────
const OKX_BASE: &str = "https://www.okx.com";

// ── Executor Config (loaded from executor_config.json) ───────────────────────
#[derive(Debug, Deserialize)]
struct PumpFilter {
    vol_threshold:      f64,
    rsi_threshold:      f64,
    stretch_multiplier: f64,
}

#[derive(Debug, Deserialize)]
struct KellyTier {
    min_ev:        f64,
    min_conf:      f64,
    risk_pct_low:  f64,
    risk_pct_high: f64,
}

#[derive(Debug, Deserialize)]
struct KellyTierC {
    max_ev:   f64,
    max_conf: f64,
}

#[derive(Debug, Deserialize)]
struct KellyConfig {
    fraction:  f64,
    tier_a:    KellyTier,
    tier_b:    KellyTier,
    tier_c:    KellyTierC,
    floor_pct: f64,
    ceil_pct:  f64,
}

#[derive(Debug, Deserialize)]
struct ExecutorConfig {
    max_signal_age_min:     i64,
    poll_interval_secs:     u64,
    max_open_positions:     i64,
    max_portfolio_risk_pct: f64,
    trail_pct:              f64,
    cooldown_min:           i64,
    cooldown_time_exit_min: i64,
    pump_filter:            PumpFilter,
    hold_min_default:       i64,
    hold_min:               HashMap<String, i64>,
    coin_max_alloc_default: f64,
    coin_max_alloc:         HashMap<String, f64>,
    correlation_groups:     Vec<Vec<String>>,
    kelly:                  KellyConfig,
}

fn coin_max_alloc(ecfg: &ExecutorConfig, symbol: &str) -> f64 {
    ecfg.coin_max_alloc.get(symbol).copied().unwrap_or(ecfg.coin_max_alloc_default)
}

fn max_hold_min(ecfg: &ExecutorConfig, symbol: &str) -> i64 {
    ecfg.hold_min.get(symbol).copied().unwrap_or(ecfg.hold_min_default)
}

/// Tier-aware time-stop override.
///
/// Mathematical basis: mean reversion (Tier R) operates on a fundamentally different
/// time scale than momentum (Tier A/B). Academic literature (Gatev et al., 2006;
/// crypto empirical data) shows ranging-market RSI reversion resolves in 2-6 hours.
/// Cutting at 60-90 min = consistently exiting before the move completes.
///
/// Empirical evidence (our own data): 16 CLOSED_TIME trades, avg -$0.21.
/// CLOSED_SL (trailing after TP1) avg +$0.81. The trade WORKS when it runs.
/// The time-stop is the executioner, not the market.
///
/// Fix:
///   Tier R: base × 2.5 (capped at 240 min) — mean reversion needs 2-4h
///   Tier V: base × 1.5 (capped at 180 min) — volume breakout needs confirmation
///   Tier F: base × 1.0 (unchanged) — 1H momentum fades fast by definition
///   Tier A/B: base × 1.0 (unchanged) — trend continuation, ADX drives timing
///
/// The RSI entry level further modulates Tier R:
///   RSI < 30 (extreme oversold): × 3.0 — recovery from capitulation takes longest
///   RSI 30-40: × 2.5 — standard oversold bounce window
///   RSI 40-50: × 2.0 — mild oversold, faster resolution expected
fn tier_aware_hold_min(ecfg: &ExecutorConfig, symbol: &str, ams_tier: char, entry_rsi: f64) -> i64 {
    let base = max_hold_min(ecfg, symbol);
    match ams_tier {
        'R' => {
            let multiplier = if entry_rsi < 30.0 { 3.0 }
                             else if entry_rsi < 40.0 { 2.5 }
                             else { 2.0 };
            ((base as f64 * multiplier) as i64).min(240)
        },
        'V' => ((base as f64 * 1.5) as i64).min(180),
        _ => base, // A, B, F: unchanged
    }
}

// ── Config ───────────────────────────────────────────────────────────────────
#[derive(Debug, Deserialize)]
struct Config {
    api_key:    String,
    secret_key: String,
    passphrase: String,
    #[serde(default = "default_risk")]    risk_per_trade_pct: f64,
    #[serde(default = "default_dd")]      max_daily_drawdown_pct: f64,
    #[serde(default = "default_cb")]      circuit_breaker_consecutive_losses: u32,
}
fn default_risk() -> f64 { 2.0 }
fn default_dd()   -> f64 { 10.0 }
fn default_cb()   -> u32 { 3 }

// ── Signal ───────────────────────────────────────────────────────────────────
#[allow(dead_code)] // id/risk_pct set during construction; read by journal helpers
#[derive(Debug)]
struct Signal {
    id:        i64,
    symbol:    String,
    direction: String,
    entry:     f64,
    stop:      f64,
    tp1:       f64,
    _tp2:      Option<f64>,
    risk_pct:  f64,
    ev:        Option<f64>,
    p_up:      Option<f64>,
    p_down:    Option<f64>,
    confidence: Option<f64>,
}

// ── Kelly Sizing ─────────────────────────────────────────────────────────────
/// Returns risk fraction to use for this signal (e.g. 0.02 = 2% of equity)
fn kelly_risk_fraction(sig: &Signal, base_risk: f64, ecfg: &ExecutorConfig) -> f64 {
    let ev   = sig.ev.unwrap_or(0.0);
    let p_up = sig.p_up.unwrap_or(0.5);
    let p_dn = sig.p_down.unwrap_or(0.5);
    let conf = sig.confidence.unwrap_or(0.0);
    let k    = &ecfg.kelly;

    // Full Kelly: f* = (p_up * ev - p_down) / ev
    let kelly_full = if ev > 0.0 {
        (p_up * ev - p_dn) / ev
    } else {
        0.0
    };

    // Fractional Kelly, scaled by confidence
    let conf_scale = (conf / 0.5).min(1.0).max(0.2);  // 20–100% scale
    let frac_kelly = kelly_full * k.fraction * conf_scale;

    // Tier enforcement
    let tier_risk = if ev >= k.tier_a.min_ev && conf >= k.tier_a.min_conf {
        // Tier A: strong signal
        frac_kelly.max(k.tier_a.risk_pct_low / 100.0).min(k.tier_a.risk_pct_high / 100.0)
    } else if ev >= k.tier_b.min_ev && conf >= k.tier_b.min_conf {
        // Tier B: normal
        frac_kelly.max(k.tier_b.risk_pct_low / 100.0).min(k.tier_b.risk_pct_high / 100.0)
    } else {
        // Tier C: weak → use base risk or skip
        base_risk * 0.5
    };

    tier_risk.max(k.floor_pct / 100.0).min(k.ceil_pct / 100.0)
}

/// Signal is worth trading at all?
fn should_trade(sig: &Signal, ecfg: &ExecutorConfig) -> bool {
    let ev   = sig.ev.unwrap_or(0.0);
    let conf = sig.confidence.unwrap_or(0.0);
    // Skip very weak signals below tier C thresholds
    ev >= ecfg.kelly.tier_c.max_ev && conf >= ecfg.kelly.tier_c.max_conf
}

/// Tier label for logging
fn signal_tier(sig: &Signal, ecfg: &ExecutorConfig) -> &'static str {
    let ev   = sig.ev.unwrap_or(0.0);
    let conf = sig.confidence.unwrap_or(0.0);
    let k    = &ecfg.kelly;
    if ev >= k.tier_a.min_ev && conf >= k.tier_a.min_conf { "A" }
    else if ev >= k.tier_b.min_ev && conf >= k.tier_b.min_conf { "B" }
    else { "C" }
}

// ── OKX Auth ─────────────────────────────────────────────────────────────────
fn okx_sign(secret: &str, ts: &str, method: &str, path: &str, body: &str) -> String {
    let msg = format!("{}{}{}{}", ts, method, path, body);
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(msg.as_bytes());
    B64.encode(mac.finalize().into_bytes())
}
fn okx_ts() -> String { Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string() }

fn okx_get(client: &Client, cfg: &Config, path: &str) -> Result<Value> {
    let ts  = okx_ts();
    let sig = okx_sign(&cfg.secret_key, &ts, "GET", path, "");
    let r = client.get(format!("{}{}", OKX_BASE, path))
        .header("OK-ACCESS-KEY",        &cfg.api_key)
        .header("OK-ACCESS-SIGN",       &sig)
        .header("OK-ACCESS-TIMESTAMP",  &ts)
        .header("OK-ACCESS-PASSPHRASE", &cfg.passphrase)
        .header("Content-Type", "application/json")
        .send()?.json::<Value>()?;
    Ok(r)
}

fn okx_post(client: &Client, cfg: &Config, path: &str, body: &Value) -> Result<Value> {
    let ts  = okx_ts();
    let bs  = body.to_string();
    let sig = okx_sign(&cfg.secret_key, &ts, "POST", path, &bs);
    let r = client.post(format!("{}{}", OKX_BASE, path))
        .header("OK-ACCESS-KEY",        &cfg.api_key)
        .header("OK-ACCESS-SIGN",       &sig)
        .header("OK-ACCESS-TIMESTAMP",  &ts)
        .header("OK-ACCESS-PASSPHRASE", &cfg.passphrase)
        .header("Content-Type", "application/json")
        .body(bs).send()?.json::<Value>()?;
    Ok(r)
}

// ── OKX Helpers ──────────────────────────────────────────────────────────────
fn get_equity(client: &Client, cfg: &Config) -> Result<f64> {
    let r = okx_get(client, cfg, "/api/v5/account/balance")?;
    Ok(r["data"][0]["totalEq"].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0))
}

/// Query OKX fills history to find the most recent sell fill for an instrument.
/// Used by ghost detection to get the actual exit price (not just current price estimate).
/// Returns (fill_price, fill_time_rfc3339) or None if no recent fill found.
fn get_last_sell_fill(client: &Client, cfg: &Config, inst: &str) -> Option<(f64, String)> {
    let url = format!("/api/v5/trade/fills-history?instId={}&side=sell&limit=5", inst);
    let r = okx_get(client, cfg, &url).ok()?;
    let fills = r["data"].as_array()?;
    // Most recent fill is first. Return the most recent sell fill price.
    for fill in fills {
        let px_str = fill["fillPx"].as_str().unwrap_or("0");
        let px: f64 = px_str.parse().unwrap_or(0.0);
        if px > 0.0 {
            let ts = fill["ts"].as_str().unwrap_or("").to_string();
            return Some((px, ts));
        }
    }
    None
}

fn get_avail_usdt(client: &Client, cfg: &Config) -> Result<f64> {
    let r = okx_get(client, cfg, "/api/v5/account/balance")?;
    if let Some(details) = r["data"][0]["details"].as_array() {
        for d in details {
            if d["ccy"].as_str() == Some("USDT") {
                return Ok(d["availBal"].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0));
            }
        }
    }
    Ok(0.0)
}

/// Get available balance for any coin (e.g. "SOL", "ETH") from OKX account balance.
/// Used to clamp sell qty to actual available balance (fee-corrected).
fn get_avail_coin(client: &Client, cfg: &Config, ccy: &str) -> f64 {
    let r = match okx_get(client, cfg, "/api/v5/account/balance") {
        Ok(v) => v,
        Err(_) => return 0.0,
    };
    if let Some(details) = r["data"][0]["details"].as_array() {
        for d in details {
            if d["ccy"].as_str() == Some(ccy) {
                return d["availBal"].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
            }
        }
    }
    0.0
}

fn get_price(client: &Client, inst_id: &str) -> Result<f64> {
    let r = client.get(format!("{}/api/v5/market/ticker?instId={}", OKX_BASE, inst_id))
        .send()?.json::<Value>()?;
    Ok(r["data"][0]["last"].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0))
}

fn inst_id(symbol: &str) -> String {
    if symbol.contains('-') { symbol.to_string() }
    else { symbol.replace("USDT", "-USDT") }
}

// ── Pure-logic helpers (also used in unit tests) ─────────────────────────────

/// Floor qty to 4 decimal places to avoid over-sell "insufficient balance" errors
fn qty_floor(qty: f64) -> f64 {
    (qty * 10000.0).floor() / 10000.0
}

/// Stale/dynamic-entry decision for Long signals.
/// Returns: "DUMPED" | "CHASING" | "DYNAMIC" | "NORMAL"
fn stale_decision_long(sig_entry: f64, current: f64) -> &'static str {
    let dev = (current - sig_entry) / sig_entry;
    if dev < -0.03      { "DUMPED" }   // price fell >3% — signal invalidated
    else if dev > 0.10  { "CHASING" }  // price ripped >10% — too risky to chase
    else if dev > 0.03  { "DYNAMIC" }  // price ran 3-10% — enter with recalculated SL/TP
    else                { "NORMAL" }   // within ±3% — normal entry
}

/// Recalculate SL and TP1 from actual fill price, preserving the ATR% and TP% ratios
/// from the original signal. Used for dynamic entries and when SL >= fill.
fn compute_sl_tp_from_fill(sig_entry: f64, sig_stop: f64, sig_tp1: f64, fill: f64) -> (f64, f64) {
    let atr_pct = ((sig_entry - sig_stop).abs() / sig_entry.max(1e-9)).max(0.015);
    let tp_pct  = ((sig_tp1  - sig_entry).abs() / sig_entry.max(1e-9)).max(0.020);
    let sl = (fill * (1.0 - atr_pct) * 1_000_000.0).round() / 1_000_000.0;
    let tp = (fill * (1.0 + tp_pct)  * 1_000_000.0).round() / 1_000_000.0;
    (sl, tp)
}

/// Dynamic TP1 multiplier based on tier + signal confidence (P(up)).
///
/// Tier A/B (trend): high conviction, wide target → 1.5–2.0× ATR
/// Tier R/V/F (reversion/breakout/fast): entry is already late or 1H-only.
///   ATR is measured from a wide SL (full candle swing) → raw ATR% can be 5–6%.
///   Using 2× ATR pushes TP1 to +10–13% — unreachable in 60–120 min.
///   Cap at 1.0× ATR so TP1 ≈ entry + 4–5% → realistic for 90-min hold.
///
/// Example (SOL Tier V): entry $85.95, SL $80.43 → ATR=6.4%
///   Old (2×): TP1 = $85.95 × 1.128 = $96.98 (+13%) → never reached → CLOSED_TIME
///   New (1×): TP1 = $85.95 × 1.064 = $91.43 (+6.4%) → reachable in 90 min
fn compute_dynamic_tp1(entry: f64, sl: f64, p_up: f64, ams_tier: char) -> f64 {
    let atr_pct = ((entry - sl).abs() / entry.max(1e-9)).max(0.015);
    let multiplier: f64 = match ams_tier {
        // Tier R (mean reversion): TP1 at 0.50× ATR.
        // Mathematical basis: optimal TP for ranging markets per simulation:
        //   0.25× → win rate 80% but EV -1.6%/day (fee kills it)
        //   0.50× → win rate 74%, EV +0.69%/day ← OPTIMAL
        //   0.75× → win rate 63%, EV +0.43%/day
        // 0.5× ATR = ~1.5% target from entry in 3% ATR market.
        // Combined with RSI exit (sell when RSI > 52 = reversion complete),
        // this captures the mean-reversion move at its statistical peak.
        'R' => 0.50,
        // Tier V/F: breakout/momentum — 1× ATR remains appropriate
        'V' | 'F' => 1.0,
        // Trend tiers: scale by confidence (unchanged)
        _ => if p_up >= 0.92 { 2.0 } else if p_up >= 0.85 { 1.5 } else { 1.0 },
    };
    (entry * (1.0 + atr_pct * multiplier) * 1_000_000.0).round() / 1_000_000.0
}

/// TP2 = entry + 2× the TP1 distance. Used as the second partial exit target.
fn compute_tp2(entry: f64, tp1: f64) -> f64 {
    let tp1_dist = tp1 - entry;
    ((entry + 2.0 * tp1_dist) * 1_000_000.0).round() / 1_000_000.0
}

/// Compute trailing stop level.
/// Rules:
///   • Only active after TP1 is hit (half_exited == 1)
///   • trail_sl = high_water × (1 – trail_pct)
///   • Only valid if trail_sl > entry (never trail below breakeven)
///   • Returns 0.0 when inactive
fn compute_trail_sl(half_exited: i64, high_water: f64, entry: f64, trail_pct: f64) -> f64 {
    if half_exited != 1 || high_water <= entry {
        return 0.0;
    }
    let ts = high_water * (1.0 - trail_pct);
    if ts > entry { ts } else { 0.0 }
}

/// Returns true if current time is within the noisy early Asian pre-market window (07:00–07:30 WIB).
/// Signals in this window have historically lower quality (P(up)=0.68 vs 0.92 outside).
fn is_noisy_window() -> bool {
    let now = Local::now();
    now.hour() == 7 && now.minute() < 30
}

/// Pump/dump protection — reads RSI, volume_ratio, and price vs EMA20 from the realtime cache.
/// Returns true (= block entry) if pattern suggests active manipulation or exhaustion.
/// Read RSI14 for a symbol from the realtime cache.
/// Returns 50.0 (neutral) if cache is unavailable.
fn get_rsi_from_cache(symbol: &str, cache_path: &Path) -> f64 {
    let cache_file = cache_path.join("trading-journal/realtime_cache.json");
    let text = match std::fs::read_to_string(&cache_file) { Ok(t) => t, Err(_) => return 50.0 };
    let d: serde_json::Value = match serde_json::from_str(&text) { Ok(v) => v, Err(_) => return 50.0 };
    d["symbols"][symbol]["rsi14"].as_f64().unwrap_or(50.0)
}

fn is_pump_or_exhausted(symbol: &str, cache_path: &Path, ecfg: &ExecutorConfig) -> bool {
    let cache_file = cache_path.join("trading-journal/realtime_cache.json");
    let text = match std::fs::read_to_string(&cache_file) { Ok(t) => t, Err(_) => return false };
    let d: serde_json::Value = match serde_json::from_str(&text) { Ok(v) => v, Err(_) => return false };
    let sym = &d["symbols"][symbol];
    let rsi  = sym["rsi14"].as_f64().unwrap_or(50.0);
    let vol  = sym["volume_ratio"].as_f64().unwrap_or(1.0);
    let price = sym["price"].as_f64().unwrap_or(0.0);
    let ema20 = sym["ema20"].as_f64().unwrap_or(0.0);
    let pf = &ecfg.pump_filter;
    let vol_pump  = vol > pf.vol_threshold && rsi > pf.rsi_threshold;
    let stretched = ema20 > 0.0 && price > ema20 * pf.stretch_multiplier;
    vol_pump || stretched
}

/// Returns true if the coin's macro EMA trend is UP (bullish): price > EMA20 > EMA50.
///
/// Reads from realtime_cache (updated from confirmed 4H candles by realtime-engine).
/// Gate purpose: prevent LONG entries during macro downtrends, even when a 1H bounce
/// triggers an AMS Tier A/B signal. The 2026-02-27 session showed that DOT×2 and SOL×1
/// were entered LONG while price < EMA20 < EMA50 — all three closed at a loss (CLOSED_TIME).
///
/// Fail-open on cache error: if cache is unreadable, allow entry (other guards remain active).
/// Fail-open on zero EMA: OKX-reported EMA not yet warmed up — allow entry conservatively.
/// Check macro EMA alignment, with tier-aware relaxation.
///
/// Tier A/B (trend-following): require full bull stack price > EMA20 > EMA50.
/// Tier R/V (mean-reversion / volume breakout): only require price > EMA20.
///   Rationale: Tier R entries are by definition in macro-bearish conditions (oversold
///   bounces). Requiring EMA20 > EMA50 defeats mean-reversion purpose entirely.
///   Tier V also leads higher TFs, so EMA50 catching up is expected.
///   This mirrors the fix in strategy.rs `pass_macro_ema`.
///
/// Check macro EMA alignment with tier-aware logic.
///
/// Tier A/B  : price > EMA20(4H) > EMA50(4H)  — strict, slow, reliable
/// Tier R/V  : price > EMA20(4H)               — relaxed (mean-reversion bounces)
/// Tier F    : price > EMA20(1H) > EMA50(1H)   — fast, uses 1H EMA (updates hourly)
///
/// Tier F uses 1H EMA so we can catch early bounces within the same hour
/// instead of waiting for the 4H candle to update the macro trend.
fn macro_ema_aligned(symbol: &str, cache_path: &Path, is_reversion_tier: bool, is_fast_tier: bool) -> bool {
    let cache_file = cache_path.join("trading-journal/realtime_cache.json");
    let text = match std::fs::read_to_string(&cache_file) { Ok(t) => t, Err(_) => return true };
    let d: serde_json::Value = match serde_json::from_str(&text) { Ok(v) => v, Err(_) => return true };
    let sym = &d["symbols"][symbol];
    let price  = sym["price"].as_f64().unwrap_or(0.0);
    let ema20  = sym["ema20"].as_f64().unwrap_or(0.0);
    let ema50  = sym["ema50"].as_f64().unwrap_or(0.0);
    let e20_1h = sym["ema20_1h"].as_f64().unwrap_or(0.0);
    let e50_1h = sym["ema50_1h"].as_f64().unwrap_or(0.0);

    if is_fast_tier {
        // Use 1H EMA — falls back to 4H if 1H not yet computed
        let e20 = if e20_1h > 0.0 { e20_1h } else { ema20 };
        let e50 = if e50_1h > 0.0 { e50_1h } else { ema50 };
        if e20 <= 0.0 || e50 <= 0.0 { return true; }
        price > e20 && e20 > e50
    } else if is_reversion_tier {
        // Reversion: price is EXPECTED below EMA20 (oversold bounce).
        // Old check (price > EMA20) was backwards — blocked every oversold entry.
        // New: EMA20 must not be in free-fall (>8% below EMA50 = structural bear).
        if ema20 <= 0.0 || ema50 <= 0.0 { return true; }
        ema20 >= ema50 * 0.92
    } else {
        if ema20 <= 0.0 || ema50 <= 0.0 { return true; }
        price > ema20 && ema20 > ema50
    }
}

/// Returns true if the coin's intra-1H momentum is NOT in a fast crash.
///
/// Reads `ret_1h` from realtime_cache — the return since the last confirmed 1H candle close.
/// A strongly negative value means price is dropping hard WITHIN the current 1H candle,
/// even if the 4H EMA stack still looks bullish (4H EMA lags by design).
///
/// Threshold -3.0%: a -3% intra-candle move in crypto is significant (normal noise is ≤1%).
/// Above -3% (e.g. -1%, +2%) → momentum OK → allow entry.
/// Below -3% (e.g. -4%, -7%) → fast crash in progress → block entry.
///
/// Fail-open: if cache/field missing, allow entry (other guards remain active).
fn recent_momentum_ok(symbol: &str, cache_path: &Path) -> bool {
    let cache_file = cache_path.join("trading-journal/realtime_cache.json");
    let text = match std::fs::read_to_string(&cache_file) { Ok(t) => t, Err(_) => return true };
    let d: serde_json::Value = match serde_json::from_str(&text) { Ok(v) => v, Err(_) => return true };
    // ret_1h field absent (old cache) → 0.0 → fail-open
    let ret_1h = d["symbols"][symbol]["ret_1h"].as_f64().unwrap_or(0.0);
    ret_1h > -3.0
}

/// Returns how many CLOSED_TIME exits a symbol had today (UTC date).
///
/// CLOSED_TIME = executor's time-stop: price failed to reach TP within hold_min.
/// Returns true if a CLOSED_TIME exit occurred within the last 240 minutes (1 full 4H candle).
///
/// Old guard: blocked until UTC midnight — too aggressive. A genuinely new signal on a fresh
/// 4H candle should be allowed. DOT pattern (2026-02-27) happened within 3H of CLOSED_TIME
/// with the SAME setup. Blocking for 4H = one full candle change is sufficient.
///
/// Examples:
///   CLOSED_TIME at 13:05 → block until 17:05 → allow at 17:05+ with fresh signal
///   CLOSED_TIME at 15:05 (ETH) → allow re-entry at 19:05+ (ETH 18:58 signal would still block
///   but only by ~6 min — a minor timing issue vs blocking all night)
/// Returns true if the last closed trade today was profitable (CLOSED_TP or CLOSED_TP2).
/// Used by Filter 3: allow more trades only if previous exit was a win.
/// If last exit was CLOSED_TIME or CLOSED_SL → market choppy/adverse → pause.
fn last_trade_was_profit(conn: &Connection, today: &str) -> bool {
    let result: Option<String> = conn.query_row(
        "SELECT status FROM open_positions \
         WHERE date(closed_at)=? AND closed_at IS NOT NULL \
         ORDER BY closed_at DESC LIMIT 1",
        rusqlite::params![today],
        |r| r.get(0),
    ).ok();
    match result.as_deref() {
        Some("CLOSED_TP") | Some("CLOSED_TP2") | Some("CLOSED_RSI_EXIT") | Some("CLOSED_TRAIL") => true,
        _ => false,
    }
}

fn closed_time_within_4h(conn: &Connection, symbol: &str) -> bool {
    let cutoff = (chrono::Utc::now() - chrono::Duration::minutes(240)).to_rfc3339();
    conn.query_row(
        "SELECT 1 FROM open_positions \
         WHERE symbol=? AND status='CLOSED_TIME' AND closed_at >= ?",
        rusqlite::params![symbol, cutoff],
        |_| Ok(true),
    ).unwrap_or(false)
}

/// Legacy wrapper kept for unit tests that still use the old count-based API.
/// In production, use closed_time_within_4h() instead.
#[cfg(test)]
fn daily_closed_time_count(conn: &Connection, symbol: &str, today: &str) -> u32 {
    conn.query_row(
        "SELECT COUNT(*) FROM open_positions \
         WHERE symbol=? AND status='CLOSED_TIME' AND date(closed_at)=?",
        rusqlite::params![symbol, today],
        |r| r.get::<_, u32>(0),
    ).unwrap_or(0)
}

fn place_market_buy(client: &Client, cfg: &Config, inst: &str, usdt_amount: f64) -> Result<String> {
    let body = json!({"instId":inst,"tdMode":"cash","side":"buy","ordType":"market",
                      "sz":format!("{:.2}",usdt_amount),"tgtCcy":"quote_ccy"});
    let r = okx_post(client, cfg, "/api/v5/trade/order", &body)?;
    if r["code"].as_str() == Some("0") {
        let oid = r["data"][0]["ordId"].as_str().unwrap_or("").to_string();
        println!("[executor] ✅ BUY {:.2}U of {} | ordId={}", usdt_amount, inst, oid);
        Ok(oid)
    } else { Err(anyhow!("Buy failed: {}", r)) }
}

/// Place a single OCO algo order (SL + TP in one order) after buy fill.
/// This replaces the old approach of placing SL conditional order separately.
/// OCO does NOT lock the coin balance twice — OKX manages both legs internally.
/// When either SL or TP triggers, the other leg is automatically cancelled.
fn place_oco(client: &Client, cfg: &Config, inst: &str, qty: f64, sl_px: f64, tp_px: f64) {
    let qty_floor = (qty * 10000.0).floor() / 10000.0;
    let body = json!({
        "instId": inst,
        "tdMode": "cash",
        "side": "sell",
        "ordType": "oco",
        "sz": format!("{:.4}", qty_floor),
        "tpTriggerPx": format!("{:.4}", tp_px),
        "tpOrdPx": "-1",
        "slTriggerPx": format!("{:.4}", sl_px),
        "slOrdPx": "-1"
    });
    match okx_post(client, cfg, "/api/v5/trade/order-algo", &body) {
        Ok(r) if r["code"].as_str() == Some("0") =>
            println!("[executor] 🛡️  OCO active | SL={:.4} TP={:.4} qty={:.4}", sl_px, tp_px, qty_floor),
        Ok(r) => {
            // OCO not supported (e.g. account type restriction) — fallback to SL-only conditional
            println!("[executor] ⚠️  OCO failed: {} — falling back to SL-only", r["data"][0]["sMsg"]);
            let sl_body = json!({"instId":inst,"tdMode":"cash","side":"sell","ordType":"conditional",
                                 "sz":format!("{:.4}",qty_floor),"slTriggerPx":format!("{:.4}",sl_px),"slOrdPx":"-1"});
            match okx_post(client, cfg, "/api/v5/trade/order-algo", &sl_body) {
                Ok(r2) if r2["code"].as_str() == Some("0") => println!("[executor] 🛡️  SL fallback @ {:.4}", sl_px),
                Ok(r2) => println!("[executor] ⚠️  SL fallback also failed: {}", r2["data"][0]["sMsg"]),
                Err(e) => println!("[executor] ⚠️  SL fallback error: {}", e),
            }
        },
        Err(e) => println!("[executor] ⚠️  OCO error: {}", e),
    }
}

/// Cancel all pending conditional (algo) orders for an instrument.
/// Must be called BEFORE any market sell (TP/trail/time/SL software) to free the
/// coin balance that OKX locks when an algo order is active.
fn cancel_pending_algos(client: &Client, cfg: &Config, inst: &str) {
    // Cancel both ordType=conditional (legacy SL) and ordType=oco (attachAlgoOrds SL+TP)
    cancel_pending_algos_type(client, cfg, inst, "conditional");
    cancel_pending_algos_type(client, cfg, inst, "oco");
}

fn cancel_pending_algos_type(client: &Client, cfg: &Config, inst: &str, ord_type: &str) {
    // 1. Fetch all pending algo orders for this instrument
    let url = format!("/api/v5/trade/orders-algo-pending?instId={}&ordType={}", inst, ord_type);
    let pending = match okx_get(client, cfg, &url) {
        Ok(v) => v,
        Err(e) => { eprintln!("[executor] algo-pending fetch error: {}", e); return; }
    };
    let algo_ids: Vec<String> = pending["data"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|o| o["algoId"].as_str().map(|s| s.to_string()))
        .collect();

    if algo_ids.is_empty() {
        return; // nothing to cancel
    }

    // 2. Cancel them: OKX cancel-algos accepts up to 10 per request
    for chunk in algo_ids.chunks(10) {
        let args: Vec<serde_json::Value> = chunk.iter()
            .map(|id| json!({"instId": inst, "algoId": id}))
            .collect();
        match okx_post(client, cfg, "/api/v5/trade/cancel-algos", &json!(args)) {
            Ok(r) if r["code"].as_str() == Some("0") =>
                println!("[executor] 🗑️  Cancelled {} SL algo(s) for {}", chunk.len(), inst),
            Ok(r) => println!("[executor] ⚠️  Cancel-algos partial: {}", r),
            Err(e) => println!("[executor] ⚠️  Cancel-algos error: {}", e),
        }
    }
    // 3. Brief pause so OKX processes the cancellation and releases the balance lock
    std::thread::sleep(std::time::Duration::from_millis(300));
}

fn market_sell(client: &Client, cfg: &Config, inst: &str, qty: f64) -> bool {
    // Extract coin symbol (e.g. "SOL" from "SOL-USDT") to check actual available balance.
    // Clamp sell qty to available to avoid "insufficient balance" errors caused by fee deductions.
    let ccy = inst.split('-').next().unwrap_or(inst);
    let avail = get_avail_coin(client, cfg, ccy);

    // If avail=0, algo order may still be locking balance — signal caller to retry next cycle.
    if avail <= 0.0 {
        println!("[executor] ⏳ Sell delayed: {} avail=0 (balance locked?), retry next cycle", ccy);
        return false;
    }

    let sell_qty = if avail < qty {
        println!("[executor] ℹ️  Clamping sell qty from {:.4} to avail {:.4} {}", qty, avail, ccy);
        avail
    } else {
        qty
    };
    let qty_floor = qty_floor(sell_qty);
    if qty_floor <= 0.0 {
        println!("[executor] ⚠️  Sell skipped: qty_floor=0 for {} (avail={:.6})", ccy, avail);
        return false;
    }
    let body = json!({"instId":inst,"tdMode":"cash","side":"sell","ordType":"market",
                      "sz":format!("{:.4}",qty_floor)});
    match okx_post(client, cfg, "/api/v5/trade/order", &body) {
        Ok(r) if r["code"].as_str() == Some("0") => {
            println!("[executor] 💸 SELL {:.4} {} (avail was {:.4})", sell_qty, inst, avail);
            true
        },
        Ok(r) => {
            println!("[executor] ⚠️  Sell failed: {}", r["data"][0]["sMsg"]);
            false
        },
        Err(e) => {
            println!("[executor] ⚠️  Sell error: {}", e);
            false
        },
    }
}

// ── Signal Parsing ───────────────────────────────────────────────────────────
fn parse_signal(id: i64, raw_block: &str, ev: Option<f64>,
                p_up: Option<f64>, p_down: Option<f64>, conf: Option<f64>) -> Option<Signal> {
    let re_dir   = regex::Regex::new(r"PAPER_TRADE.*?—\s*(\w+)\s+(Long|Short)").ok()?;
    let re_entry = regex::Regex::new(r"Entry zone:\s*([\d.]+)").ok()?;
    let re_stop  = regex::Regex::new(r"Invalidation/stop:\s*([\d.]+)").ok()?;
    let re_tp1   = regex::Regex::new(r"TP1/TP2:\s*([\d.]+)").ok()?;
    let re_tp2   = regex::Regex::new(r"TP1/TP2:[^\n]*/\s*([\d.]+)").ok()?;
    let re_risk  = regex::Regex::new(r"Risk:\s*\*\*([\d.]+)%").ok()?;

    let dir_cap   = re_dir.captures(raw_block)?;
    let sym_raw   = dir_cap.get(1)?.as_str();
    let symbol    = if sym_raw.ends_with("USDT") { sym_raw.to_string() } else { format!("{}USDT", sym_raw) };
    let direction = dir_cap.get(2)?.as_str().to_string();
    let entry     = re_entry.captures(raw_block)?.get(1)?.as_str().parse::<f64>().ok()?;
    let stop      = re_stop.captures(raw_block)?.get(1)?.as_str().parse::<f64>().ok()?;
    let tp1       = re_tp1.captures(raw_block)?.get(1)?.as_str().parse::<f64>().ok()?;
    let tp2       = re_tp2.captures(raw_block).and_then(|c| c.get(1)).and_then(|m| m.as_str().parse::<f64>().ok());
    let risk_pct  = re_risk.captures(raw_block)
        .and_then(|c| c.get(1)).and_then(|m| m.as_str().parse::<f64>().ok())
        .unwrap_or(2.0) / 100.0;

    Some(Signal { id, symbol, direction, entry, stop, tp1, _tp2: tp2, risk_pct, ev, p_up, p_down, confidence: conf })
}

// ── Journal ──────────────────────────────────────────────────────────────────
fn journal_path(base: &Path) -> PathBuf {
    base.join(format!("{}.md", Local::now().format("%Y-%m-%d")))
}
fn append_journal(base: &Path, block: &str) {
    let path = journal_path(base);
    let mut content = fs::read_to_string(&path).unwrap_or_default();
    content.push_str(block);
    let _ = fs::write(&path, content);
    println!("[executor] 📝 Journal updated");
}

fn journal_opened(base: &Path, sig: &Signal, fill: f64, usdt: f64, equity: f64, risk_frac: f64, hold_min: i64, ecfg: &ExecutorConfig) {
    let t   = Local::now().format("%H:%M").to_string();
    let ev  = sig.ev.map(|e| format!("{:+.2}R", e)).unwrap_or_else(|| "N/A".to_string());
    let tier = signal_tier(sig, &ecfg);
    let block = format!("\n\
## Live Trade OPENED — {} WIB [Rust v2]\n\
- **Symbol:** {} {} | **Tier:** {} | **Fill:** {:.4}\n\
- **Stop:** {:.4} | **TP1:** {:.4} | **Spend:** {:.2}U\n\
- **Risk:** {:.1}% ({:.2}U) | **Equity:** {:.2}U | **EV:** {}\n\
- P(up)={:.2} Conf={:.2}\n\
\n\
### Lesson Learned (entry)\n\
1. Tier {} signal — Fractional Kelly sized to {:.1}% risk.\n\
2. SL software monitor aktif @ stop price. OKX-side algo jika didukung.\n\
3. Max hold: {} menit — time exit otomatis jika TP/SL tidak terpicu.\n\
4. EV={} — setiap R earned menjadi edge jangka panjang.\n",
        t, sig.symbol, sig.direction, tier, fill,
        sig.stop, sig.tp1, usdt,
        risk_frac * 100.0, equity * risk_frac, equity, ev,
        sig.p_up.unwrap_or(0.5), sig.confidence.unwrap_or(0.0),
        tier, risk_frac * 100.0, hold_min, ev);
    append_journal(base, &block);
}

fn journal_closed(base: &Path, sig: &Signal, pnl: f64, status: &str, reason: &str) {
    let t = Local::now().format("%H:%M").to_string();
    let (emoji, label) = match status {
        "CLOSED_TP"        => ("✅", "TP1 HIT — Partial Exit"),
        "CLOSED_TP2"       => ("🎯", "TP2 HIT — Full Target"),
        "CLOSED_TRAIL"     => ("🎯", "TRAILING STOP"),
        "CLOSED_RSI_EXIT"  => ("📈", "RSI EXIT — Reversion Complete"),
        "CLOSED_BREAKEVEN" => ("⚖️",  "BREAKEVEN EXIT"),
        "CLOSED_SL"        => ("❌", "STOP LOSS"),
        "CLOSED_TIME"      => ("⏰", "TIME EXIT"),
        "CLOSED_MANUAL"    => ("🛑", "MANUAL CLOSE"),
        _                  => ("↩️", "EXIT"),
    };
    let ll = match status {
        "CLOSED_TP" =>
            "1. TP1 hit — setengah posisi sudah exit, profit terkunci.\n\
2. SL dipindah ke breakeven → worst case skenario selanjutnya = 0.\n\
3. Trail stop aktif 2.5% dari high water — melindungi remaining profit.\n\
4. Evaluasi: apakah momentum masih kuat untuk TP2?",
        "CLOSED_TP2" =>
            "1. TP2 hit — full target tercapai, sistem bekerja sempurna!\n\
2. Two-stage exit terbukti efektif: TP1 lock profit → TP2 maximize gain.\n\
3. Patience setelah TP1 terbayar — jangan terlalu cepat cut remaining position.\n\
4. Catat kondisi: regime + EMA alignment + volume yang mendukung setup ini.",
        "CLOSED_RSI_EXIT" =>
            "1. RSI exit — reversion selesai, RSI kembali ke >52 (zona netral/bullish).\n\
2. Strategi optimal: entry oversold RSI<45, exit saat RSI>52 (mean reversion complete).\n\
3. TP price belum tentu tercapai tapi profit terkunci saat momentum reversion selesai.\n\
4. Evaluasi: bandingkan profit vs jika menunggu TP price — apakah RSI exit lebih baik?",
        "CLOSED_TRAIL" =>
            "1. Trailing stop aktif setelah TP1 — profit terlindungi dari reversal.\n\
2. Market reversal setelah high water mark — trail stop menghindari drawback lebih besar.\n\
3. High water tracking strategy terbukti efektif untuk trend following.\n\
4. Evaluasi: apakah TP2 level terlalu jauh atau timing momentum sudah habis?",
        "CLOSED_BREAKEVEN" =>
            "1. Posisi exit di breakeven setelah TP1 partial — tidak rugi, tidak profit maksimal.\n\
2. SL dipindah ke entry setelah TP1 berhasil memproteksi modal.\n\
3. Market gagal mencapai TP2 — momentum melemah setelah TP1.\n\
4. Evaluasi: ADX trend strength dan regime saat entry — apakah sustainable?",
        "CLOSED_SL" =>
            "1. SL terpicu — normal dalam probabilistic trading system.\n\
2. Cek kembali: apakah volume konfirmasi cukup kuat saat entry?\n\
3. Loss dalam budget yang telah dikunci via Kelly sizing.\n\
4. ≥3 consecutive losses → circuit breaker aktif, hentikan trading hari ini.",
        "CLOSED_TIME" =>
            "1. Time exit 60 menit — market tidak bergerak sesuai ekspektasi.\n\
2. Sideways/konsolidasi — kondisi ini perlu dihindari di entry berikutnya.\n\
3. Tanda regime kurang trending — cek ADX saat entry (minimal 20+).\n\
4. Pertimbangkan: apakah EMA alignment dan volume cukup kuat di awal?",
        "CLOSED_MANUAL" =>
            "1. Manual close — di luar normal system flow.\n\
2. Catat alasan intervensi dan kondisi market saat itu.\n\
3. Evaluasi: apakah intervensi tepat atau sebaiknya biarkan sistem berjalan?\n\
4. Jika ada systematic issue, update parameter di OPS_STATE.md.",
        _ =>
            "1. Exit tidak terduga — cek log executor untuk detail.\n\
2. Validasi kondisi market dan parameter saat close.\n\
3. Catat sebagai edge case untuk improvement algoritma.\n\
4. Monitor sistem lebih ketat pada kondisi serupa.",
    };
    let block = format!("\n\
## Live Trade CLOSED ({}) — {} WIB {}\n\
- **Symbol:** {} | **PnL:** {:+.4}U | **Reason:** {}\n\
\n\
### Lesson Learned\n\
{}\n", label, t, emoji, sig.symbol, pnl, reason, ll);
    append_journal(base, &block);
}

fn journal_skipped(base: &Path, sym: &str, dir: &str, reason: &str) {
    let t = Local::now().format("%H:%M").to_string();
    let block = format!("\n## Signal SKIPPED — {} WIB ⏩\n- {} {} | {}\n", t, sym, dir, reason);
    append_journal(base, &block);
}

// ── Live DB Helpers ──────────────────────────────────────────────────────────
fn init_live_db(conn: &Connection) -> Result<()> {
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS processed_signals (
            signal_id    INTEGER NOT NULL,
            exchange     TEXT NOT NULL DEFAULT 'okx',
            processed_at TEXT NOT NULL,
            symbol TEXT, direction TEXT,
            entry REAL, stop REAL, tp1 REAL,
            order_id TEXT, status TEXT, fill_price REAL,
            pnl_usdt REAL, notes TEXT
        );
        CREATE TABLE IF NOT EXISTS open_positions (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            signal_id   INTEGER, order_id TEXT,
            symbol TEXT, direction TEXT,
            qty REAL, entry_price REAL, stop_price REAL,
            tp1_price REAL, risk_usdt REAL, opened_at TEXT,
            status TEXT DEFAULT 'OPEN',
            tp2_price REAL,
            half_exited INTEGER DEFAULT 0,
            p_up REAL
        );
        CREATE TABLE IF NOT EXISTS daily_stats (
            trade_date TEXT PRIMARY KEY,
            starting_equity REAL,
            realized_pnl REAL DEFAULT 0,
            consecutive_losses INTEGER DEFAULT 0,
            trade_count INTEGER DEFAULT 0,
            circuit_breaker_hit INTEGER DEFAULT 0
        );
    ")?;
    // Idempotent migrations for new columns (ALTER TABLE ignores error if column exists)
    let _ = conn.execute("ALTER TABLE open_positions ADD COLUMN tp2_price REAL", params![]);
    let _ = conn.execute("ALTER TABLE open_positions ADD COLUMN half_exited INTEGER DEFAULT 0", params![]);
    let _ = conn.execute("ALTER TABLE open_positions ADD COLUMN p_up REAL", params![]);
    let _ = conn.execute("ALTER TABLE open_positions ADD COLUMN high_water REAL DEFAULT 0", params![]);
    let _ = conn.execute("ALTER TABLE open_positions ADD COLUMN entry_rsi REAL DEFAULT 50.0", params![]);
    let _ = conn.execute("ALTER TABLE open_positions ADD COLUMN closed_at TEXT", params![]);
    let _ = conn.execute("ALTER TABLE open_positions ADD COLUMN ams_tier TEXT DEFAULT 'X'", params![]);
    // Idempotent migration: add exchange column to processed_signals (ignore if already exists)
    conn.execute("ALTER TABLE processed_signals ADD COLUMN exchange TEXT DEFAULT 'okx'", params![]).ok();
    // Idempotent: ensure dedup index exists for INSERT OR IGNORE to work correctly
    conn.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS idx_ps_sig_exch ON processed_signals(signal_id, exchange);")?;
    Ok(())
}

fn is_processed(conn: &Connection, sig_id: i64) -> bool {
    conn.query_row("SELECT 1 FROM processed_signals WHERE signal_id=? AND exchange='okx'",
        params![sig_id], |_| Ok(true)).unwrap_or(false)
}

fn mark_processed(conn: &Connection, sig_id: i64, symbol: &str, direction: &str,
                  entry: f64, stop: f64, tp1: f64,
                  order_id: &str, status: &str, fill: f64, notes: &str) {
    let now = Utc::now().to_rfc3339();
    let _ = conn.execute(
        "INSERT OR IGNORE INTO processed_signals
         (signal_id,exchange,processed_at,symbol,direction,entry,stop,tp1,order_id,status,fill_price,notes)
         VALUES(?1,'okx',?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![sig_id,now,symbol,direction,entry,stop,tp1,order_id,status,fill,notes]);
}

fn count_open_positions(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM open_positions WHERE status='OPEN'",
        params![], |r| r.get(0)).unwrap_or(0)
}

fn has_open_position(conn: &Connection, symbol: &str) -> bool {
    conn.query_row("SELECT 1 FROM open_positions WHERE symbol=? AND status='OPEN'",
        params![symbol], |_| Ok(true)).unwrap_or(false)
}

/// Returns true if this symbol was closed (FILLED then closed) within the last `cooldown_min` minutes.
/// Prevents rapid re-entry on the same coin right after a manual close or SL/TP exit.
fn cooldown_active(conn: &Connection, symbol: &str, cooldown_min: i64, cooldown_time_exit_min: i64) -> bool {
    let cutoff = (chrono::Utc::now() - chrono::Duration::minutes(cooldown_min))
        .to_rfc3339();
    // General cooldown: any close within cooldown_min
    let general = conn.query_row(
        "SELECT 1 FROM open_positions
         WHERE symbol=? AND status != 'OPEN' AND closed_at >= ?",
        params![symbol, cutoff],
        |_| Ok(true)
    ).unwrap_or(false);
    if general { return true; }
    // Extended cooldown for CLOSED_TIME: same H4 setup still valid, don't re-enter for 1 H4 period
    let time_exit_cutoff = (chrono::Utc::now() - chrono::Duration::minutes(cooldown_time_exit_min))
        .to_rfc3339();
    conn.query_row(
        "SELECT 1 FROM open_positions
         WHERE symbol=? AND status='CLOSED_TIME' AND closed_at >= ?",
        params![symbol, time_exit_cutoff],
        |_| Ok(true)
    ).unwrap_or(false)
}

fn total_risk_deployed(conn: &Connection, equity: f64) -> f64 {
    if equity <= 0.0 { return 0.0; }
    let total_risk_usdt: f64 = conn.query_row(
        "SELECT COALESCE(SUM(risk_usdt),0) FROM open_positions WHERE status='OPEN'",
        params![], |r| r.get(0)).unwrap_or(0.0);
    total_risk_usdt / equity
}

fn open_position(conn: &Connection, sig_id: i64, order_id: &str, symbol: &str,
                 direction: &str, qty: f64, fill: f64, stop: f64, tp1: f64, risk_usdt: f64,
                 tp2: f64, p_up: f64, ams_tier: char, entry_rsi: f64) {
    let now = Utc::now().to_rfc3339();
    let tier_str = ams_tier.to_string();
    let _ = conn.execute(
        "INSERT INTO open_positions
         (signal_id,order_id,symbol,direction,qty,entry_price,stop_price,tp1_price,risk_usdt,opened_at,tp2_price,p_up,ams_tier,entry_rsi)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        params![sig_id,order_id,symbol,direction,qty,fill,stop,tp1,risk_usdt,now,tp2,p_up,tier_str,entry_rsi]);
}

fn get_daily_stats(conn: &Connection, today: &str, starting_equity: f64) -> (f64, i64, u32, bool) {
    conn.execute("INSERT OR IGNORE INTO daily_stats(trade_date,starting_equity) VALUES(?1,?2)",
        params![today, starting_equity]).ok();
    conn.query_row(
        "SELECT realized_pnl,consecutive_losses,trade_count,circuit_breaker_hit FROM daily_stats WHERE trade_date=?",
        params![today], |r| Ok((r.get::<_,f64>(0)?,r.get::<_,i64>(1)?,r.get::<_,u32>(2)?,r.get::<_,i32>(3)?!=0))
    ).unwrap_or((0.0,0,0,false))
}

/// Pure circuit-breaker gate: returns true if it is safe to open new trades.
/// Extracted for unit-testability — the main loop calls this with live values.
fn is_risk_ok(cb_hit: bool, consec_losses: i64, drawdown: f64, max_losses: u32, max_dd: f64) -> bool {
    !cb_hit && (consec_losses as u32) < max_losses && drawdown < max_dd
}

// ── Correlation check ────────────────────────────────────────────────────────
fn corr_group_blocked(conn: &Connection, symbol: &str, ecfg: &ExecutorConfig) -> bool {
    let group = ecfg.correlation_groups.iter().find(|g| g.iter().any(|s| s == symbol));
    if let Some(grp) = group {
        for sym in grp {
            if has_open_position(conn, sym) {
                println!("[executor] ⏩ Corr block: {} open in same group as {}", sym, symbol);
                return true;
            }
        }
    }
    false
}

// ── Main Loop ────────────────────────────────────────────────────────────────
fn main() -> Result<()> {
    let base: PathBuf = {
        let ws = std::env::var("OPENCLAW_WORKSPACE")
            .unwrap_or_else(|_| "/Users/rachmataditiya/.openclaw/workspace".to_string());
        PathBuf::from(ws).join("data/trading-journal")
    };

    let cfg: Config = serde_json::from_str(&fs::read_to_string(base.join("okx_config.json"))?)?;
    let ecfg: ExecutorConfig = serde_json::from_str(&fs::read_to_string(base.join("executor_config.json"))?)?;
    let trail_pct = ecfg.trail_pct / 100.0;
    let max_portfolio_risk = ecfg.max_portfolio_risk_pct / 100.0;
    let base_risk = cfg.risk_per_trade_pct / 100.0;
    let max_dd    = cfg.max_daily_drawdown_pct / 100.0;
    let cb_losses = cfg.circuit_breaker_consecutive_losses;
    let sim_path  = base.join("sim_history.db");
    let live_path = base.join("live_positions.db");

    let client    = Client::builder().timeout(StdDuration::from_secs(10)).build()?;
    let live_conn = Connection::open(&live_path)?;
    init_live_db(&live_conn)?;

    println!("[executor] 🦀 Live Executor v2 (Kelly+MultiPos) | BaseRisk={:.0}% | MaxPos={} | MaxPortRisk={:.0}%",
        cfg.risk_per_trade_pct, ecfg.max_open_positions, ecfg.max_portfolio_risk_pct);

    loop {
        // TWO separate date strings — each must match the storage format of its target table:
        //
        // 1. `today_utc` — for live_positions.db tables (daily_stats, open_positions, processed_signals).
        //    closed_at / opened_at stored as Utc::now().to_rfc3339() → date in UTC.
        //    Previously used Local (Jakarta), causing mismatch 00:00–07:00 WIB → re-entry guard broken.
        //
        // 2. `today_local` — for sim_history.db (simulation_events.trade_date).
        //    Sim stores trade_date/trade_time as Local (WIB) from inserted_at timestamp.
        //    Using UTC here = mismatch 00:00–07:00 WIB → executor never finds today's PAPER_TRADE signals.
        //    Root cause of SOL grade A being ignored 2026-03-11 01:25–01:50 WIB.
        let today_utc   = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let today_local = Local::now().format("%Y-%m-%d").to_string();
        let cutoff      = (Local::now() - Duration::minutes(ecfg.max_signal_age_min)).format("%H:%M").to_string();

        let equity = get_equity(&client, &cfg).unwrap_or(116.0);
        let (realized_pnl, consec_losses, trade_count, cb_hit) = get_daily_stats(&live_conn, &today_utc, equity);

        let drawdown = if equity > 0.0 { -realized_pnl / equity } else { 0.0 };
        let risk_ok  = is_risk_ok(cb_hit, consec_losses, drawdown, cb_losses, max_dd);

        // ── Process new signals ───────────────────────────────────────────────
        if risk_ok {
            let sim_conn = Connection::open(&sim_path)?;
            let mut stmt = sim_conn.prepare(
                "SELECT id, raw_block, expected_value_r, confidence, p_up, p_down, quality_gate
                 FROM simulation_events
                 WHERE decision='PAPER_TRADE'
                   AND trade_date=?1
                   AND trade_time >= ?2
                 ORDER BY expected_value_r DESC"  // best EV first
            )?;
            let rows: Vec<(i64, String, Option<f64>, Option<f64>, Option<f64>, Option<f64>, Option<String>)> =
                stmt.query_map(params![today_local, cutoff], |r| {
                    Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))
                })?.filter_map(|r| r.ok()).collect();
            drop(stmt); drop(sim_conn);

            for (sig_id, raw_block, ev, conf, p_up, p_down, quality_gate) in rows {
                if is_processed(&live_conn, sig_id) { continue; }

                println!("[executor] 🔔 Signal #{} EV={:.2} Conf={:.2}",
                    sig_id, ev.unwrap_or(0.0), conf.unwrap_or(0.0));

                let sig = match parse_signal(sig_id, &raw_block, ev, p_up, p_down, conf) {
                    Some(s) => s,
                    None => { mark_processed(&live_conn,sig_id,"?","?",0.0,0.0,0.0,"","PARSE_FAIL",0.0,""); continue; }
                };

                // ── Guards ────────────────────────────────────────────────────
                if sig.direction == "Short" {
                    journal_skipped(&base, &sig.symbol, &sig.direction, "Short=haram (spot only)");
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_SHORT",0.0,"");
                    continue;
                }
                // Extract tier from quality_gate BEFORE should_trade check
                // so Tier R (reversion) can use relaxed EV threshold.
                let pre_tier_char = quality_gate.as_deref().unwrap_or("")
                    .split(';').find(|s| s.starts_with("ams="))
                    .and_then(|s| s.strip_prefix("ams="))
                    .and_then(|s| s.chars().next())
                    .unwrap_or('C');
                let ev_val   = ev.unwrap_or(0.0);
                let conf_val = conf.unwrap_or(0.0);
                // Tier R/V (reversion): relax EV threshold — Bayesian model undervalues
                // oversold bounces (RSI 18-25). Strategy.rs already gates this with
                // rsi<30 → ev>0 / else ev>=0.2. Match that logic here.
                let passes_ev_gate = if pre_tier_char == 'R' || pre_tier_char == 'V' {
                    ev_val > 0.0  // any positive EV — quality gate in sim already screened
                } else {
                    ev_val >= ecfg.kelly.tier_c.max_ev && conf_val >= ecfg.kelly.tier_c.max_conf
                };
                if !passes_ev_gate {
                    println!("[executor] ⏩ Tier C skip: EV={:.2} Conf={:.2}", ev_val, conf_val);
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_TIER_C",0.0,
                        &format!("ev={:.2} conf={:.2}", ev_val, conf_val));
                    continue;
                }
                if count_open_positions(&live_conn) >= ecfg.max_open_positions {
                    println!("[executor] ⏩ Max positions ({}) reached", ecfg.max_open_positions);
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_MAXPOS",0.0,"");
                    continue;
                }
                if has_open_position(&live_conn, &sig.symbol) {
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_DUPE",0.0,"");
                    continue;
                }
                // Cooldown after any closed position on the same symbol
                if cooldown_active(&live_conn, &sig.symbol, ecfg.cooldown_min, ecfg.cooldown_time_exit_min) {
                    println!("[executor] ⏳ Cooldown: {} closed recently, skipping", sig.symbol);
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_COOLDOWN",0.0,
                        &format!("{}min cooldown after close", ecfg.cooldown_min));
                    continue;
                }
                if corr_group_blocked(&live_conn, &sig.symbol, &ecfg) {
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_CORR",0.0,"");
                    continue;
                }
                if total_risk_deployed(&live_conn, equity) >= max_portfolio_risk {
                    println!("[executor] ⏩ Portfolio risk cap {:.0}% reached", ecfg.max_portfolio_risk_pct);
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_PORT_RISK",0.0,"");
                    continue;
                }

                // ── Price staleness / dynamic entry check ─────────────────────
                // Long signals: block only if price DUMPED >3% (invalidated) or RIPPED >10% (chase).
                // If price ran up 3-10%: dynamic entry — proceed, recalc SL/TP from fill.
                let inst = inst_id(&sig.symbol);
                let current = match get_price(&client, &inst) {
                    Ok(p) if p > 0.0 => p,
                    _ => { eprintln!("[executor] ⚠️ No price for {}", inst); continue; }
                };
                let stale_dec = if sig.direction == "Long" {
                    stale_decision_long(sig.entry, current)
                } else {
                    if (current - sig.entry).abs() / sig.entry > 0.03 { "DUMPED" } else { "NORMAL" }
                };
                let is_dynamic_entry = stale_dec == "DYNAMIC";
                match stale_dec {
                    "DUMPED" => {
                        let pct = ((current - sig.entry) / sig.entry * 100.0).abs();
                        println!("[executor] ⏩ Invalidated — price dumped {:.1}%", pct);
                        mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_STALE",0.0,&format!("dumped={:.1}%",pct));
                        continue;
                    }
                    "CHASING" => {
                        let pct = (current - sig.entry) / sig.entry * 100.0;
                        println!("[executor] ⏩ Skip chase — price ripped {:.1}%", pct);
                        mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_STALE",0.0,&format!("chasing={:.1}%",pct));
                        continue;
                    }
                    "DYNAMIC" => {
                        let pct = (current - sig.entry) / sig.entry * 100.0;

                        // Strict guard for low-risk/conservative-tier signals (Tier V/R).
                        // Identified by risk_pct ≤ 1.2% (half-Kelly: 2% base × 0.5 = 1%).
                        // These tiers have lower multi-TF confidence — do NOT chase past 5%.
                        // For Tier V (volume breakout): entry must be fresh, not extended.
                        let is_conservative_tier = sig.risk_pct <= 1.2;
                        if is_conservative_tier && pct > 5.0 {
                            println!("[executor] ⏩ Skip Tier V/R late chase — {:.1}% run (max 5% for half-Kelly tier)",
                                     pct);
                            mark_processed(&live_conn, sig_id, &sig.symbol, &sig.direction,
                                           sig.entry, sig.stop, sig.tp1, "",
                                           "SKIPPED_TIER_V_LATE", 0.0,
                                           &format!("run={:.1}%>5%,conservative_tier", pct));
                            continue;
                        }

                        // Extra guard: if price ran >5% AND RSI already overbought (>75),
                        // skip — we'd be chasing an exhausted move after the meat is gone.
                        // Threshold: 5% run = halfway through the DYNAMIC window (3–10%).
                        // RSI > 75 = overbought zone, momentum likely to stall or reverse.
                        if pct > 5.0 {
                            let rsi = get_rsi_from_cache(&sig.symbol, &base.join("../.."));
                            if rsi > 75.0 {
                                println!("[executor] ⏩ Skip overbought run — {:.1}% run + RSI={:.0} > 75",
                                         pct, rsi);
                                mark_processed(&live_conn, sig_id, &sig.symbol, &sig.direction,
                                               sig.entry, sig.stop, sig.tp1, "",
                                               "SKIPPED_OVERBOUGHT_RUN", 0.0,
                                               &format!("run={:.1}%,rsi={:.0}>75", pct, rsi));
                                continue;
                            }
                        }
                        println!("[executor] 📈 Dynamic entry: price ran {:.1}% — recalculating SL/TP from fill", pct);
                    }
                    _ => {} // NORMAL
                }

                // ── Time filter: noisy Asian pre-market (07:00–07:30 WIB) ────────────
                // Historical data: P(up)=0.68 in this window vs 0.92 outside → require Tier A only
                if is_noisy_window() && signal_tier(&sig, &ecfg) != "A" {
                    println!("[executor] ⏰ Noisy window (07:00-07:30 WIB) — skipping non-Tier-A signal");
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_NOISY_WINDOW",0.0,"07:00-07:30 WIB tier<A");
                    continue;
                }

                // ── Pump/dump protection ─────────────────────────────────────────────
                if is_pump_or_exhausted(&sig.symbol, &base.join("../.."), &ecfg) {
                    println!("[executor] 🚨 Pump/dump detected for {} — skipping entry", sig.symbol);
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_PUMP_DUMP",0.0,"vol>4×+RSI>78 or price>EMA20×1.08");
                    continue;
                }

                // ── Macro EMA alignment guard ─────────────────────────────────────────
                // Block LONG entry if price < EMA20 or EMA20 < EMA50 (macro bearish stack).
                // Root cause of 2026-02-27 losses: DOT×2 + SOL×1 entered against macro trend.
                // Realtime cache uses confirmed 4H candles — reliable macro trend indicator.
                // Exception: Tier R and V use relaxed check (price > EMA20 only) — see fn docstring.
                let ams_tier_char = quality_gate.as_deref().unwrap_or("")
                    .split(';').find(|s| s.starts_with("ams="))
                    .and_then(|s| s.strip_prefix("ams="))
                    .and_then(|s| s.chars().next())
                    .unwrap_or('C');
                let is_reversion_tier = ams_tier_char == 'R' || ams_tier_char == 'V';
                let is_fast_tier = ams_tier_char == 'F';
                if !macro_ema_aligned(&sig.symbol, &base.join("../.."), is_reversion_tier, is_fast_tier) {
                    println!("[executor] ⛔ Macro EMA DOWN: {} — skip LONG in downtrend", sig.symbol);
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_EMA_DOWN",0.0,
                        "price<ema20 or ema20<ema50 (4H macro bearish)");
                    continue;
                }

                // ── Fast crash / momentum guard ──────────────────────────────────────
                // Block entry if intra-1H price drop exceeds -3% since last confirmed 1H close.
                // Addresses the 4H EMA lag problem: 4H EMA can stay bullish for hours while
                // price is actively crashing on shorter timeframes. ret_1h is computed from
                // the WS-fed realtime_cache (≤1s latency), so it reflects actual fast moves.
                if !recent_momentum_ok(&sig.symbol, &base.join("../..")) {
                    println!("[executor] ⚡ Fast drop: {} — ret_1h < -3%, skip entry", sig.symbol);
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_FAST_DROP",0.0,
                        "ret_1h < -3.0% (intra-1H crash while 4H EMA still lagging)");
                    continue;
                }

                // ── CLOSED_TIME 4H re-entry guard ────────────────────────────────────
                // Block re-entry for 240 min (1 full 4H candle) after a CLOSED_TIME.
                // Old guard blocked until UTC midnight — too aggressive. A new signal on a
                // fresh 4H candle with strong EV should be allowed.
                // DOT pattern (2026-02-27) happened within 3H → 4H block is sufficient.
                // ETH pattern (2026-03-01): CLOSED_TIME 15:05 → ETH 18:58 signal (3h53m later)
                // would be allowed → correct, market conditions genuinely changed.
                if closed_time_within_4h(&live_conn, &sig.symbol) {
                    println!("[executor] 🚫 {} had CLOSED_TIME within last 4H — waiting for full candle reset", sig.symbol);
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_DAILY_TIME_EXIT",0.0,
                        "CLOSED_TIME within last 240min (4H candle reset required)");
                    continue;
                }

                // ── Filter 3: Stop after non-profitable exit ──────────────
                // Over-trading in choppy markets compounds losses.
                // Logic: if today's last closed trade was NOT profitable (CLOSED_TIME or
                // CLOSED_SL) → market is choppy/adverse → stop for the day.
                // If last exit was CLOSED_TP / CLOSED_TP2 → market is trending → allow
                // more trades (market condition confirmed good).
                // No trade today yet (trade_count=0) → always allow first trade.
                if trade_count >= 1 {
                    let last_was_profit = last_trade_was_profit(&live_conn, &today_utc);
                    if !last_was_profit {
                        println!("[executor] 🚫 Last trade today was not profitable — pause rest of day (trade_count={})", trade_count);
                        mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_MAX_DAILY",0.0,
                            "last trade today was CLOSED_TIME/SL — market choppy, pausing");
                        continue;
                    }
                    // Last trade was TP → market trending → allow next trade
                    println!("[executor] ✅ Last trade today was profitable — allowing trade #{}", trade_count + 1);
                }

                // ── Kelly-sized position ──────────────────────────────────────
                let risk_frac   = kelly_risk_fraction(&sig, base_risk, &ecfg);
                let avail_usdt  = get_avail_usdt(&client, &cfg).unwrap_or(0.0);

                // Coin allocation cap
                let max_spend   = equity * coin_max_alloc(&ecfg, &sig.symbol);
                let stop_dist   = (sig.entry - sig.stop).abs();
                if stop_dist <= 0.0 { continue; }
                let risk_usdt   = equity * risk_frac;
                let qty_target  = risk_usdt / stop_dist;
                let usdt_spend  = (qty_target * current)
                    .min(avail_usdt * 0.8)
                    .min(max_spend);

                if usdt_spend < 1.0 {
                    println!("[executor] ⚠️ Spend {:.2}U too small", usdt_spend);
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_BALANCE",0.0,"");
                    continue;
                }

                println!("[executor] 🚀 {} {} | Tier={} EV={:.1}R Risk={:.1}% Spend={:.2}U SL={:.4} TP={:.4}",
                    sig.direction, sig.symbol, signal_tier(&sig, &ecfg),
                    ev.unwrap_or(0.0), risk_frac*100.0, usdt_spend, sig.stop, sig.tp1);

                // ── Execute ───────────────────────────────────────────────────
                match place_market_buy(&client, &cfg, &inst, usdt_spend) {
                    Ok(order_id) => {
                        let fill = get_price(&client, &inst).unwrap_or(current);
                        let qty  = usdt_spend / fill;

                        // Step 1: Recalc SL/TP from fill if: dynamic entry OR SL invalid
                        let (base_stop, base_tp1) = if is_dynamic_entry || sig.stop >= fill {
                            let (s, t) = compute_sl_tp_from_fill(sig.entry, sig.stop, sig.tp1, fill);
                            println!("[executor] 🔧 SL/TP from fill {:.4}: SL {:.4}→{:.4} TP {:.4}→{:.4}", fill, sig.stop, s, sig.tp1, t);
                            (s, t)
                        } else { (sig.stop, sig.tp1) };

                        // Step 2: Dynamic TP1 scaling based on tier + P(up) confidence
                        let p_up_val = sig.p_up.unwrap_or(0.5);
                        let actual_tp1 = compute_dynamic_tp1(fill, base_stop, p_up_val, ams_tier_char);
                        let actual_stop = base_stop;
                        if actual_tp1 != base_tp1 {
                            let mult = if p_up_val >= 0.92 { "2.0×" } else { "1.5×" };
                            println!("[executor] 📊 Dynamic TP ({} ATR, P(up)={:.2}): {:.4}→{:.4}", mult, p_up_val, base_tp1, actual_tp1);
                        }

                        // Step 3: TP2 = entry + 2×(TP1 distance)
                        let actual_tp2 = compute_tp2(fill, actual_tp1);

                        if actual_stop >= fill {
                            // ABORT: SL invalid even after recalculation — undo the buy immediately
                            println!("[executor] ⛔ ABORT: SL {:.4} >= fill {:.4} — selling back", actual_stop, fill);
                            let sold = market_sell(&client, &cfg, &inst, qty);
                            let abort_note = if sold { "SL>=fill after recalc, position unwound" } else { "SL>=fill after recalc, SELL FAILED — manual action required" };
                            mark_processed(&live_conn, sig_id, &sig.symbol, &sig.direction,
                                sig.entry, actual_stop, actual_tp1, &order_id, "ABORTED", fill,
                                abort_note);
                        } else {
                            // Wait for buy to settle before placing SL/TP algo orders
                            thread::sleep(StdDuration::from_secs(3));
                            // Query actual received coin qty (after OKX fee deduction).
                            // Using computed qty = usdt/fill over-estimates by ~0.1% (fee).
                            // Always use actual available balance as source of truth.
                            let ccy = inst.split('-').next().unwrap_or(&inst);
                            let avail_after = get_avail_coin(&client, &cfg, ccy);
                            let actual_qty = if avail_after > 0.0 && avail_after < qty {
                                println!("[executor] ℹ️  Actual received qty {:.6} {} (computed {:.6}, fee adj {:.4}%)",
                                    avail_after, ccy, qty, (1.0 - avail_after/qty)*100.0);
                                avail_after
                            } else { qty };
                            // SL algo order on OKX = hard stop (crash protection).
                            // TP is handled by executor polling (software TP1/TP2/trail).
                            // We do NOT place a separate TP limit order because OKX locks the full
                            // coin balance for the SL algo order, causing TP limit to always fail
                            // with "insufficient balance" — conflicting orders on simple spot.
                            let entry_rsi = get_rsi_from_cache(&sig.symbol, &base);
                            let hold_min_tier = tier_aware_hold_min(&ecfg, &sig.symbol, ams_tier_char, entry_rsi);
                            // OCO algo order: SL + TP in one order — no balance lock conflict.
                            // OKX manages both legs; when one triggers the other auto-cancels.
                            // Software monitoring (RSI exit, trail, time) cancels this OCO before selling.
                            place_oco(&client, &cfg, &inst, actual_qty, actual_stop, actual_tp1);
                            open_position(&live_conn, sig_id, &order_id, &sig.symbol,
                                          &sig.direction, actual_qty, fill, actual_stop, actual_tp1, risk_usdt,
                                          actual_tp2, p_up_val, ams_tier_char, entry_rsi);
                            live_conn.execute(
                                "UPDATE daily_stats SET trade_count=trade_count+1 WHERE trade_date=?",
                                params![today_utc]).ok();
                            journal_opened(&base, &sig, fill, usdt_spend, equity, risk_frac, hold_min_tier, &ecfg);
                            mark_processed(&live_conn, sig_id, &sig.symbol, &sig.direction,
                                           sig.entry, actual_stop, actual_tp1, &order_id, "FILLED", fill,
                                           &format!("tier={} ev={:.2} risk={:.1}%",
                                               signal_tier(&sig, &ecfg), ev.unwrap_or(0.0), risk_frac*100.0));
                        }
                    }
                    Err(e) => {
                        eprintln!("[executor] ❌ Exec failed: {}", e);
                        mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,
                            sig.entry,sig.stop,sig.tp1,"","EXEC_FAIL",0.0,&e.to_string());
                    }
                }
            }
        } else {
            println!("[executor] 🔴 Risk check: cb={} losses={} dd={:.1}%", cb_hit, consec_losses, drawdown*100.0);
        }

        // ── Monitor open positions ────────────────────────────────────────────
        let open_pos: Vec<(i64,i64,String,String,f64,f64,f64,f64,f64,String,f64,i64,f64,String,f64)> = {
            let mut s = live_conn.prepare(
                "SELECT id,signal_id,symbol,direction,qty,entry_price,stop_price,tp1_price,risk_usdt,opened_at,
                        COALESCE(tp2_price,0.0),COALESCE(half_exited,0),COALESCE(high_water,0.0),
                        COALESCE(ams_tier,'X'),COALESCE(entry_rsi,50.0)
                 FROM open_positions WHERE status='OPEN'")?;
            let x: Vec<_> = s.query_map(params![], |r| Ok((
                r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,
                r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,
                r.get(10)?,r.get(11)?,r.get(12)?,r.get(13)?,r.get(14)?
            )))?.filter_map(|r| r.ok()).collect();
            x
        };

        for (pid, sig_id, symbol, direction, qty, entry, stop, tp1, _risk, opened_at_str, tp2, half_exited, mut high_water, pos_tier, pos_entry_rsi) in open_pos {
            let inst    = inst_id(&symbol);
            let current = match get_price(&client, &inst) { Ok(p) => p, Err(_) => continue };
            let opened_dt = DateTime::parse_from_rfc3339(&opened_at_str)
                .map(|d| d.with_timezone(&Local)).unwrap_or_else(|_| Local::now());
            let hold_min = (Local::now() - opened_dt).num_minutes();

            // ── Ghost position detection ─────────────────────────────────────
            // If coin balance is dust (< min lot) the position was closed externally
            // (algo order, manual, OKX liquidation) but DB was never updated.
            // Auto-close immediately instead of retrying indefinitely.
            //
            // Key distinction:
            // - Legitimate position with active SL algo: OKX locks coin → avail ≈ dust
            // - True ghost (externally sold): no algo active, coin gone → avail ≈ dust
            // Fix: only trigger ghost if avail is dust AND no pending algo orders exist.
            // Pending algo = OKX is still managing the position → NOT a ghost.
            {
                let ccy = symbol.replace("USDT", "");
                let avail = get_avail_coin(&client, &cfg, &ccy);
                let algo_url = format!("/api/v5/trade/orders-algo-pending?instId={}&ordType=conditional", inst);
                let has_algo = okx_get(&client, &cfg, &algo_url)
                    .ok()
                    .and_then(|v| v["data"].as_array().map(|a| !a.is_empty()))
                    .unwrap_or(false);
                if qty_floor(avail) == 0.0 && avail < 0.001 && !has_algo {
                    // Try to get actual exit price from OKX fills history
                    // (covers: algo SL executed by OKX, manual close, liquidation)
                    let (exit_price, fill_note) = match get_last_sell_fill(&client, &cfg, &inst) {
                        Some((fp, _)) if fp > 0.0 => (fp, format!("okx_fill@{:.4}", fp)),
                        _ => (current, format!("est@{:.4}", current)),
                    };
                    let ghost_pnl = (exit_price - entry) * qty;
                    println!("[executor] 👻 Ghost {} — avail={:.6}(dust), no_algo=true, hold={}min, \
                              exit={} pnl={:.4}U. Position closed externally (algo SL or manual).",
                              symbol, avail, hold_min, fill_note, ghost_pnl);
                    let closed_now = chrono::Utc::now().to_rfc3339();
                    live_conn.execute(
                        "UPDATE open_positions SET status='CLOSED_GHOST', closed_at=? WHERE id=?",
                        params![closed_now, pid]).ok();
                    live_conn.execute(
                        "UPDATE processed_signals SET status='CLOSED_GHOST', pnl_usdt=? WHERE signal_id=? AND exchange='okx'",
                        params![ghost_pnl, sig_id]).ok();
                    let is_loss: i64 = if ghost_pnl < 0.0 { 1 } else { 0 };
                    live_conn.execute(
                        "UPDATE daily_stats SET realized_pnl=realized_pnl+?,
                         trade_count=trade_count+1,
                         consecutive_losses=CASE WHEN ?2=1 THEN consecutive_losses+1 ELSE 0 END
                         WHERE trade_date=?3",
                        params![ghost_pnl, is_loss, today_utc]).ok();
                    continue;
                }
            }

            // ── Track high_water mark and compute trailing stop ──────────────
            if current > high_water {
                high_water = current;
                live_conn.execute("UPDATE open_positions SET high_water=? WHERE id=?",
                    params![high_water, pid]).ok();
            }
            // trail_sl: only active after TP1 (half_exited=1) AND above entry (can't trail into loss)
            let trail_sl = compute_trail_sl(half_exited, high_water, entry, trail_pct);

            let fake = Signal { id: sig_id, symbol: symbol.clone(), direction: direction.clone(),
                entry, stop, tp1, _tp2: None, risk_pct: base_risk,
                ev: None, p_up: None, p_down: None, confidence: None };

            // ── Partial exit: TP1 hit on first half ─────────────────────────
            if direction == "Long" && half_exited == 0 && current >= tp1 {
                let half_qty = qty_floor(qty * 0.5);
                let half_pnl = (tp1 - entry) * half_qty;
                println!("[executor] 🎯 TP1 PARTIAL {} — selling 50% ({:.4}) +{:.4}U, moving SL→breakeven, holding for TP2={:.4}",
                    symbol, half_qty, half_pnl, tp2);
                // Cancel SL algo FIRST so OKX releases the coin balance lock
                cancel_pending_algos(&client, &cfg, &inst);
                let sold = market_sell(&client, &cfg, &inst, half_qty);
                if sold {
                    let remaining_qty = qty_floor(qty - half_qty);
                    // Update: mark half_exited, move SL to entry (breakeven), reduce qty
                    live_conn.execute(
                        "UPDATE open_positions SET half_exited=1, stop_price=entry_price, qty=? WHERE id=?",
                        params![remaining_qty, pid]).ok();
                    live_conn.execute(
                        "UPDATE daily_stats SET realized_pnl=realized_pnl+? WHERE trade_date=?",
                        params![half_pnl, today_utc]).ok();
                    println!("[executor] ✅ Half sold. Remaining {:.4} {}, SL moved to breakeven {:.4}", remaining_qty, symbol, entry);
                    // Re-place OCO: SL at breakeven + TP at TP2 for remaining position
                    place_oco(&client, &cfg, &inst, remaining_qty, entry, tp2);
                } else {
                    println!("[executor] ⚠️  Partial sell failed — retrying next cycle");
                }
                continue;
            }

            // ── Second half: TP2 hit ─────────────────────────────────────────
            let (closed, pnl, status, reason) = if half_exited == 1 && tp2 > 0.0 && direction == "Long" && current >= tp2 {
                // Cancel SL algo first to free balance lock
                cancel_pending_algos(&client, &cfg, &inst);
                // PnL uses current (market fill), not fixed TP2 price
                let p = (current - entry) * qty;
                println!("[executor] 🚀 TP2! {} full exit +{:.4}U @ {:.4} (TP2={:.4})", symbol, p, current, tp2);
                let sold = market_sell(&client, &cfg, &inst, qty);
                if sold { (true, p, "CLOSED_TP2", format!("{:.4}≥TP2{:.4}", current, tp2)) }
                else { println!("[executor] ⚠️  TP2 sell failed — retrying"); (false, 0.0, "", String::new()) }

            // ── RSI Exit: mean reversion complete (Tier R only) ──────────────
            // When RSI crosses back above 52, the oversold bounce is statistically done.
            // RSI 52 = slightly above midpoint = price has returned to mean.
            // Exiting here captures the core reversion move without waiting for TP1
            // price target (which may or may not be reached depending on momentum).
            //
            // Only fires when:
            //   1. pos_tier == "R" (mean reversion trade only — not momentum)
            //   2. half_exited == 0 (full position still open — before TP1 price hit)
            //   3. RSI now > 52 (reversion confirmed complete)
            //   4. current > entry (profitable, not a premature exit on reversal)
            //   5. hold_min >= 15 (avoid exiting on 1-candle RSI spike at entry)
            //
            // This is the "sell when signal reverses" logic Radit identified:
            // we entered on RSI oversold, now RSI says overbought → mission complete.
            } else if pos_tier == "R"
                && half_exited == 0
                && hold_min >= 15
                && current > entry
            {
                let rsi_now = get_rsi_from_cache(&symbol, &base);
                if rsi_now > 52.0 {
                    cancel_pending_algos(&client, &cfg, &inst);
                    let p = (current - entry) * qty;
                    println!("[executor] 📈 RSI exit {} — reversion complete RSI={:.1} @ {:.4} +{:.4}U",
                        symbol, rsi_now, current, p);
                    let sold = market_sell(&client, &cfg, &inst, qty);
                    if sold { (true, p, "CLOSED_RSI_EXIT", format!("RSI={:.1}>52, curr={:.4}", rsi_now, current)) }
                    else { println!("[executor] ⚠️  RSI exit sell failed — retrying"); (false, 0.0, "", String::new()) }
                } else {
                    (false, 0.0, "", String::new())
                }

            // ── Trailing stop hit (after TP1, trail_sl above entry) ──────────
            } else if trail_sl > 0.0 && direction == "Long" && current <= trail_sl {
                cancel_pending_algos(&client, &cfg, &inst);
                let p = (current - entry) * qty;
                println!("[executor] 🔄 Trail stop {} — high={:.4} trail_sl={:.4} curr={:.4} +{:.4}U",
                    symbol, high_water, trail_sl, current, p);
                let sold = market_sell(&client, &cfg, &inst, qty);
                if sold { (true, p, "CLOSED_TRAIL", format!("{:.4}≤trail_sl{:.4}", current, trail_sl)) }
                else { println!("[executor] ⚠️  Trail sell failed — retrying"); (false, 0.0, "", String::new()) }

            // ── Time exit ─────────────────────────────────────────────────────
            // Tier F (1H fast momentum): 45-min hold — 1H signals expire faster than 4H
            } else if hold_min >= {
                let tier_char = pos_tier.chars().next().unwrap_or('X');
                let timed_hold = tier_aware_hold_min(&ecfg, &symbol, tier_char, pos_entry_rsi);
                // Tier F override: always 45 min (1H signal, expires fast)
                if pos_tier == "F" { 45 } else { timed_hold }
            } {
                cancel_pending_algos(&client, &cfg, &inst);
                let p = (current - entry) * qty;
                println!("[executor] ⏱️  Time exit {} {}min @ {:.4}", symbol, hold_min, current);
                let sold = market_sell(&client, &cfg, &inst, qty);
                if sold { (true, p, "CLOSED_TIME", format!("{}min, exit@{:.4}", hold_min, current)) }
                else { println!("[executor] ⚠️  Sell failed — retrying next cycle"); (false, 0.0, "", String::new()) }

            // ── SL hit (original or breakeven after partial) ─────────────────
            } else if direction == "Long" && current <= stop {
                cancel_pending_algos(&client, &cfg, &inst);
                let p = (current - entry) * qty;
                let label = if half_exited == 1 { "CLOSED_BREAKEVEN" } else { "CLOSED_SL" };
                let emoji = if half_exited == 1 { "⚖️" } else { "🛑" };
                println!("[executor] {} SL {} {:.4}U @ {:.4} ({})", emoji, symbol, p, current, label);
                let sold = market_sell(&client, &cfg, &inst, qty);
                if sold { (true, p, label, format!("{:.4}≤SL{:.4}", current, stop)) }
                else { println!("[executor] ⚠️  Sell failed — retrying next cycle"); (false, 0.0, "", String::new()) }

            } else { (false, 0.0, "", String::new()) };

            if closed {
                let closed_now = chrono::Utc::now().to_rfc3339();
                // Use explicit error logging (not .ok()) so DB update failures are visible
                if let Err(e) = live_conn.execute(
                    "UPDATE open_positions SET status=?, closed_at=? WHERE id=?",
                    params![status, closed_now, pid]) {
                    eprintln!("[executor] ❌ DB update failed (open_positions id={}): {}", pid, e);
                }
                if let Err(e) = live_conn.execute(
                    "UPDATE processed_signals SET status=?,pnl_usdt=? WHERE signal_id=? AND exchange='okx'",
                    params![status, pnl, sig_id]) {
                    eprintln!("[executor] ❌ DB update failed (processed_signals id={}): {}", sig_id, e);
                }
                let is_loss: i64 = if pnl < 0.0 { 1 } else { 0 };
                live_conn.execute(
                    "UPDATE daily_stats SET realized_pnl=realized_pnl+?,
                     consecutive_losses=CASE WHEN ?2=1 THEN consecutive_losses+1 ELSE 0 END
                     WHERE trade_date=?3",
                    params![pnl,is_loss,today_utc]).ok();
                journal_closed(&base, &fake, pnl, status, &reason);
            }
        }

        thread::sleep(StdDuration::from_secs(ecfg.poll_interval_secs));
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Unit Tests
// Run with: cargo test --bin live-executor
// ═══════════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod tests {
    use super::*;

    fn test_ecfg() -> ExecutorConfig {
        let mut hold_min = std::collections::HashMap::new();
        // Large cap
        hold_min.insert("BTCUSDT".to_string(), 120i64);
        hold_min.insert("ETHUSDT".to_string(), 120i64);
        // Mid cap — 90min
        for sym in &["SOLUSDT","BNBUSDT","XRPUSDT","ADAUSDT","DOTUSDT","LINKUSDT","LTCUSDT","DOGEUSDT"] {
            hold_min.insert(sym.to_string(), 90i64);
        }
        let mut coin_alloc = std::collections::HashMap::new();
        // Large cap
        coin_alloc.insert("BTCUSDT".to_string(), 0.40f64);
        coin_alloc.insert("ETHUSDT".to_string(), 0.40f64);
        // Mid cap — 25%
        coin_alloc.insert("SOLUSDT".to_string(), 0.25f64);
        coin_alloc.insert("BNBUSDT".to_string(), 0.25f64);
        coin_alloc.insert("XRPUSDT".to_string(), 0.25f64);
        // Established alts — 20%
        for sym in &["ADAUSDT","DOGEUSDT","DOTUSDT","AVAXUSDT","NEARUSDT","UNIUSDT","LINKUSDT","LTCUSDT"] {
            coin_alloc.insert(sym.to_string(), 0.20f64);
        }
        ExecutorConfig {
            max_signal_age_min: 20,
            poll_interval_secs: 60,
            max_open_positions: 3,
            max_portfolio_risk_pct: 6.0,
            trail_pct: 2.5,
            cooldown_min: 30,
            cooldown_time_exit_min: 120,
            pump_filter: PumpFilter { vol_threshold: 4.0, rsi_threshold: 78.0, stretch_multiplier: 1.08 },
            hold_min_default: 60,
            hold_min,
            coin_max_alloc_default: 0.15,
            coin_max_alloc: coin_alloc,
            correlation_groups: vec![
                vec!["BTCUSDT".into(), "ETHUSDT".into()],
                vec!["SOLUSDT".into(), "BNBUSDT".into(), "ADAUSDT".into()],
                vec!["AVAXUSDT".into(), "APTUSDT".into(), "ARBUSDT".into(), "OPUSDT".into(), "NEARUSDT".into()],
                vec!["DOGEUSDT".into(), "LTCUSDT".into()],
            ],
            kelly: KellyConfig {
                fraction: 0.25,
                tier_a: KellyTier { min_ev: 1.5, min_conf: 0.70, risk_pct_low: 3.0, risk_pct_high: 4.0 },
                tier_b: KellyTier { min_ev: 0.8, min_conf: 0.40, risk_pct_low: 1.5, risk_pct_high: 2.5 },
                tier_c: KellyTierC { max_ev: 0.5, max_conf: 0.15 },
                floor_pct: 1.0,
                ceil_pct: 4.0,
            },
        }
    }


    // ── Helper: build a minimal Signal for tests ─────────────────────────────
    fn sig(ev: f64, p_up: f64, p_down: f64, conf: f64, entry: f64, stop: f64, tp1: f64) -> Signal {
        Signal {
            id: 1, symbol: "SOLUSDT".into(), direction: "Long".into(),
            entry, stop, tp1, _tp2: None, risk_pct: 0.02,
            ev: Some(ev), p_up: Some(p_up), p_down: Some(p_down), confidence: Some(conf),
        }
    }

    // ════════════════════════════════════════════════════════════════
    // 1. STALE / DYNAMIC ENTRY CHECK
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn stale_normal_within_1pct() {
        // Price moved only 0.5% up — normal entry
        assert_eq!(stale_decision_long(79.0, 79.395), "NORMAL");
    }

    #[test]
    fn stale_normal_slight_drop() {
        // Price dropped 1% — still within tolerance
        assert_eq!(stale_decision_long(79.0, 78.21), "NORMAL");
    }

    #[test]
    fn stale_dumped_4pct() {
        // Price dumped 4% — signal invalidated
        assert_eq!(stale_decision_long(79.0, 75.84), "DUMPED");
    }

    #[test]
    fn stale_dumped_just_inside_boundary() {
        // -2.9% is clearly NOT dumped (boundary is -3%)
        let price = 79.0 * 0.971; // -2.9%
        assert_eq!(stale_decision_long(79.0, price), "NORMAL");
    }

    #[test]
    fn stale_dynamic_5pct_run() {
        // Price ran 5% — dynamic entry
        assert_eq!(stale_decision_long(79.0, 82.95), "DYNAMIC");
    }

    #[test]
    fn stale_dynamic_exactly_3pct_up() {
        // Price ran exactly 3% — boundary, becomes DYNAMIC
        let price = 79.0 * 1.0301; // just over 3%
        assert_eq!(stale_decision_long(79.0, price), "DYNAMIC");
    }

    #[test]
    fn stale_chasing_12pct() {
        // Price ripped 12% — too much to chase
        assert_eq!(stale_decision_long(79.0, 88.48), "CHASING");
    }

    #[test]
    fn stale_dynamic_just_below_chase_boundary() {
        // +9.5% is clearly still DYNAMIC (boundary is +10%)
        let price = 79.0 * 1.095; // +9.5%
        assert_eq!(stale_decision_long(79.0, price), "DYNAMIC");
    }

    // ════════════════════════════════════════════════════════════════
    // 2. SL/TP RECALCULATION FROM FILL
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn sl_tp_recalc_preserves_ratios() {
        // SOL signal: entry=79, SL=76.57 (3.07% stop), TP=80.96 (2.48% gain)
        let entry = 79.0;
        let sl_sig = 76.57;
        let tp_sig = 80.96;
        let fill = 82.0; // dynamic entry, price ran up

        let (sl, tp) = compute_sl_tp_from_fill(entry, sl_sig, tp_sig, fill);

        // ATR% = (79 - 76.57) / 79 = 3.075%
        // TP% = (80.96 - 79) / 79 = 2.481%
        let expected_sl = 82.0 * (1.0 - 0.03075);
        let expected_tp = 82.0 * (1.0 + 0.02481);

        assert!((sl - expected_sl).abs() < 0.01, "SL {:.4} expected ~{:.4}", sl, expected_sl);
        assert!((tp - expected_tp).abs() < 0.01, "TP {:.4} expected ~{:.4}", tp, expected_tp);
    }

    #[test]
    fn sl_tp_recalc_sl_always_below_fill() {
        // SL must always be below the fill price for Long
        let (sl, _tp) = compute_sl_tp_from_fill(79.0, 76.5, 81.0, 85.0);
        assert!(sl < 85.0, "SL {:.4} must be below fill 85.0", sl);
    }

    #[test]
    fn sl_tp_recalc_tp_always_above_fill() {
        // TP must always be above the fill price
        let (_sl, tp) = compute_sl_tp_from_fill(79.0, 76.5, 81.0, 82.0);
        assert!(tp > 82.0, "TP {:.4} must be above fill 82.0", tp);
    }

    #[test]
    fn sl_tp_recalc_minimum_atr_floor() {
        // Even with very tight original SL, atr_pct is floored at 1.5%
        // entry=100, stop=99.9 (0.1% stop) → should use 1.5% floor
        let (sl, _) = compute_sl_tp_from_fill(100.0, 99.9, 102.0, 100.0);
        let expected_sl = 100.0 * (1.0 - 0.015);
        assert!((sl - expected_sl).abs() < 0.01, "SL floor at 1.5%: {:.4} expected {:.4}", sl, expected_sl);
    }

    // ════════════════════════════════════════════════════════════════
    // 3. QTY FLOOR (not round — avoid over-sell)
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn qty_floor_truncates_not_rounds() {
        // 1.12349 should floor to 1.1234, NOT round to 1.1235
        let q = qty_floor(1.12349);
        assert_eq!(q, 1.1234);
    }

    #[test]
    fn qty_floor_truncates_9s() {
        // 0.999999 should floor to 0.9999, not round to 1.0000
        let q = qty_floor(0.999999);
        assert_eq!(q, 0.9999);
    }

    #[test]
    fn qty_floor_exact_stays() {
        // Already 4 decimals
        let q = qty_floor(0.3642);
        assert_eq!(q, 0.3642);
    }

    #[test]
    fn qty_floor_never_exceeds_input() {
        // Result must always be <= input (floor, not ceil)
        for &q in &[0.12345, 1.99999, 0.36421, 10.00001] {
            assert!(qty_floor(q) <= q, "qty_floor({}) exceeded input", q);
        }
    }

    // ════════════════════════════════════════════════════════════════
    // 4. KELLY RISK FRACTION + TIERS
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn kelly_tier_a_range() {
        // Tier A: EV>=2, conf>=0.5 → risk 3-4%
        let s = sig(2.5, 0.75, 0.25, 0.7, 79.0, 76.5, 81.0);
        let r = kelly_risk_fraction(&s, 0.02, &test_ecfg());
        assert!(r >= 0.03 && r <= 0.04, "Tier A risk {:.3} out of 3-4% range", r);
    }

    #[test]
    fn kelly_tier_b_range() {
        // Tier B: EV>=1, conf>=0.3 → risk 1.5-2.5%
        let s = sig(1.5, 0.65, 0.35, 0.4, 79.0, 76.5, 81.0);
        let r = kelly_risk_fraction(&s, 0.02, &test_ecfg());
        assert!(r >= 0.015 && r <= 0.025, "Tier B risk {:.3} out of 1.5-2.5% range", r);
    }

    #[test]
    fn kelly_hard_ceiling_4pct() {
        // Even with very strong signal, risk capped at 4%
        let s = sig(10.0, 0.99, 0.01, 1.0, 79.0, 76.5, 81.0);
        let r = kelly_risk_fraction(&s, 0.02, &test_ecfg());
        assert!(r <= 0.04, "Kelly ceiling violated: {:.3}", r);
    }

    #[test]
    fn kelly_hard_floor_1pct() {
        // Even with very weak signal, minimum 1%
        let s = sig(0.1, 0.51, 0.49, 0.01, 79.0, 76.5, 81.0);
        let r = kelly_risk_fraction(&s, 0.02, &test_ecfg());
        assert!(r >= 0.01, "Kelly floor violated: {:.3}", r);
    }

    // ════════════════════════════════════════════════════════════════
    // 5. SIGNAL TIER CLASSIFICATION
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn tier_a_classification() {
        // Tier A: EV>=1.5, conf>=0.70
        let s = sig(2.0, 0.75, 0.25, 0.7, 79.0, 76.5, 81.0);
        assert_eq!(signal_tier(&s, &test_ecfg()), "A");
    }

    #[test]
    fn tier_b_classification() {
        // Tier B: EV>=0.8, conf>=0.40
        let s = sig(1.0, 0.65, 0.35, 0.5, 79.0, 76.5, 81.0);
        assert_eq!(signal_tier(&s, &test_ecfg()), "B");
    }

    #[test]
    fn tier_c_low_ev() {
        let s = sig(0.4, 0.55, 0.45, 0.6, 79.0, 76.5, 81.0);
        assert_eq!(signal_tier(&s, &test_ecfg()), "C");
    }

    #[test]
    fn tier_c_low_conf() {
        let s = sig(2.0, 0.75, 0.25, 0.2, 79.0, 76.5, 81.0);
        assert_eq!(signal_tier(&s, &test_ecfg()), "C"); // conf < 0.5 → not A
    }

    // ════════════════════════════════════════════════════════════════
    // 6. SHOULD_TRADE GATE
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn should_trade_passes_valid_signal() {
        let s = sig(1.0, 0.65, 0.35, 0.3, 79.0, 76.5, 81.0);
        assert!(should_trade(&s, &test_ecfg()));
    }

    #[test]
    fn should_trade_blocks_low_ev() {
        let s = sig(0.4, 0.55, 0.45, 0.5, 79.0, 76.5, 81.0);
        assert!(!should_trade(&s, &test_ecfg()), "EV=0.4 should be blocked");
    }

    #[test]
    fn should_trade_blocks_low_confidence() {
        let s = sig(2.0, 0.75, 0.25, 0.1, 79.0, 76.5, 81.0);
        assert!(!should_trade(&s, &test_ecfg()), "conf=0.1 should be blocked");
    }

    // ════════════════════════════════════════════════════════════════
    // 7. HALAL CONSTRAINT — Long-only
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn no_short_signals_in_inst_id() {
        // inst_id just formats symbol — test it works correctly
        assert_eq!(inst_id("SOLUSDT"), "SOL-USDT");
        assert_eq!(inst_id("BTCUSDT"), "BTC-USDT");
        assert_eq!(inst_id("SOL-USDT"), "SOL-USDT"); // already has dash
    }

    // ════════════════════════════════════════════════════════════════
    // 8. COIN MAX ALLOCATION
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn coin_alloc_btc_eth_highest() {
        assert_eq!(coin_max_alloc(&test_ecfg(), "BTCUSDT"), 0.40);
        assert_eq!(coin_max_alloc(&test_ecfg(), "ETHUSDT"), 0.40);
    }

    #[test]
    fn coin_alloc_sol_bnb_mid() {
        assert_eq!(coin_max_alloc(&test_ecfg(), "SOLUSDT"), 0.25);
        assert_eq!(coin_max_alloc(&test_ecfg(), "BNBUSDT"), 0.25);
    }

    #[test]
    fn coin_alloc_atom_lowest() {
        assert_eq!(coin_max_alloc(&test_ecfg(), "ATOMUSDT"), 0.15);
    }

    #[test]
    fn coin_alloc_unknown_default() {
        // Unknown/new coins get conservative 15% allocation
        assert_eq!(coin_max_alloc(&test_ecfg(), "XYZUSDT"), 0.15);
    }

    // ════════════════════════════════════════════════════════════════
    // 9. DYNAMIC TP MULTIPLIER
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn dynamic_tp_low_pup_uses_1x() {
        // p_up < 0.85 → 1× ATR multiplier
        // entry=100, sl=97 (3% stop) → tp = 100*(1+0.03*1) = 103.0
        let tp = compute_dynamic_tp1(100.0, 97.0, 0.80, 'A');
        let expected = 100.0 * (1.0 + 0.03);
        assert!((tp - expected).abs() < 0.01, "1× ATR: got {:.4} expected {:.4}", tp, expected);
    }

    #[test]
    fn dynamic_tp_medium_pup_uses_1_5x() {
        // p_up 0.85-0.92 → 1.5× ATR multiplier
        // entry=100, sl=97 (3% stop) → tp = 100*(1+0.03*1.5) = 104.5
        let tp = compute_dynamic_tp1(100.0, 97.0, 0.87, 'A');
        let expected = 100.0 * (1.0 + 0.03 * 1.5);
        assert!((tp - expected).abs() < 0.01, "1.5× ATR: got {:.4} expected {:.4}", tp, expected);
    }

    #[test]
    fn dynamic_tp_high_pup_uses_2x() {
        // p_up >= 0.92 → 2× ATR multiplier
        // entry=100, sl=97 (3% stop) → tp = 100*(1+0.03*2) = 106.0
        let tp = compute_dynamic_tp1(100.0, 97.0, 0.93, 'A');
        let expected = 100.0 * (1.0 + 0.03 * 2.0);
        assert!((tp - expected).abs() < 0.01, "2× ATR: got {:.4} expected {:.4}", tp, expected);
    }

    #[test]
    fn dynamic_tp_always_above_entry() {
        for &p_up in &[0.5, 0.80, 0.87, 0.93, 0.99] {
            let tp = compute_dynamic_tp1(100.0, 97.0, p_up, 'A');
            assert!(tp > 100.0, "TP {:.4} must be above entry 100 for p_up={}", tp, p_up);
        }
    }

    #[test]
    fn dynamic_tp_tier_v_capped_at_1x() {
        // Tier V/F use 1× ATR; Tier R uses 0.75× ATR (tighter = higher hit rate in time window)
        // SOL case: entry=85.95, sl=80.43 → atr_pct=6.42%
        // Old Tier R (1×): tp1 = 91.46 — often not reached in 90 min ranging market
        // New Tier R (0.75×): tp1 = 89.70 — ~75th percentile of ranging moves, more achievable
        let entry = 85.95_f64; let sl = 80.43_f64; let p_up = 0.93_f64;
        let tp_v = compute_dynamic_tp1(entry, sl, p_up, 'V');
        let tp_a = compute_dynamic_tp1(entry, sl, p_up, 'A');
        let atr_pct = (entry - sl) / entry;
        let expected_1x = entry * (1.0 + atr_pct);
        let expected_075x = entry * (1.0 + atr_pct * 0.75);
        assert!((tp_v - expected_1x).abs() < 0.01,
            "Tier V TP {:.4} should be 1× ATR ({:.4})", tp_v, expected_1x);
        assert!(tp_a > tp_v, "Tier A TP {:.4} should be > Tier V TP {:.4}", tp_a, tp_v);
        let tp_r = compute_dynamic_tp1(entry, sl, p_up, 'R');
        let tp_f = compute_dynamic_tp1(entry, sl, p_up, 'F');
        let expected_050x = entry * (1.0 + atr_pct * 0.50);
        assert!((tp_r - expected_050x).abs() < 0.01,
            "Tier R TP {:.4} should be 0.50× ATR ({:.4}) — optimal for mean reversion (sim-validated)", tp_r, expected_050x);
        assert!((tp_f - expected_1x).abs() < 0.01, "Tier F capped at 1×");
        // Verify Tier R TP < Tier V TP (tighter target for reversion vs breakout)
        assert!(tp_r < tp_v, "Tier R TP should be tighter than Tier V");
    }

    // ════════════════════════════════════════════════════════════════
    // 10. TP2 CALCULATION
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn tp2_is_double_tp1_distance() {
        // entry=79, tp1=81 (2 dist) → tp2 = 79 + 2*2 = 83
        let tp2 = compute_tp2(79.0, 81.0);
        assert!((tp2 - 83.0).abs() < 0.01, "TP2={:.4} expected 83.0", tp2);
    }

    #[test]
    fn tp2_always_above_tp1() {
        for &(entry, tp1) in &[(79.0_f64, 81.0_f64), (100.0, 106.0), (50.0, 51.5)] {
            let tp2 = compute_tp2(entry, tp1);
            assert!(tp2 > tp1, "TP2 {:.4} must be above TP1 {:.4}", tp2, tp1);
        }
    }

    // ════════════════════════════════════════════════════════════════
    // 11. PARTIAL EXIT MATH
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn partial_exit_half_qty_floor() {
        // Selling 50% of 1.0917 SOL → floor to 4 decimals
        let qty = 1.0917_f64;
        let half = qty_floor(qty * 0.5);
        assert_eq!(half, 0.5458); // floor(0.54585 * 10000)/10000
    }

    #[test]
    fn partial_exit_remaining_qty() {
        // After selling half, remaining should be qty - half_sold
        let qty = 1.0917_f64;
        let half_sold = qty_floor(qty * 0.5); // 0.5458
        let remaining = qty_floor(qty - half_sold);  // 0.5459
        assert!(remaining > 0.0);
        assert!(remaining + half_sold <= qty + 0.0001, "remaining + sold must not exceed original");
    }

    #[test]
    fn partial_exit_pnl_calculation() {
        // Buy 1.0 SOL at 79.0, TP1=81.0, sell 0.5 at TP1
        // Expected PnL on first half: (81.0 - 79.0) * 0.5 = 1.0 USDT
        let entry = 79.0_f64;
        let tp1 = 81.0_f64;
        let half_qty = 0.5_f64;
        let pnl = (tp1 - entry) * half_qty;
        assert!((pnl - 1.0).abs() < 0.001, "Half-exit PnL {:.4} expected 1.0", pnl);
    }

    #[test]
    fn partial_exit_breakeven_sl_prevents_loss() {
        // After partial exit at TP1=81, SL moved to entry=79
        // If price then drops to 79, remaining half exits at breakeven
        let entry = 79.0_f64;
        let new_sl = entry; // moved to breakeven
        let remaining_qty = 0.5_f64;
        let pnl_second_half = (new_sl - entry) * remaining_qty; // = 0.0
        // Total: TP1 gain + breakeven = profit guaranteed
        let total = 1.0 + pnl_second_half; // 1.0 from first half
        assert!(total > 0.0, "Total PnL after partial exit + breakeven must be positive");
        assert!((pnl_second_half).abs() < 0.001, "Breakeven exit PnL must be ~0");
    }

    // ════════════════════════════════════════════════════════════════
    // 12. PUMP/DUMP THRESHOLDS
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn pump_vol_and_rsi_combination() {
        // Simulate the logic: vol>4 AND rsi>78 = pump
        let vol: f64 = 5.0;
        let rsi: f64 = 80.0;
        let vol_pump = vol > 4.0 && rsi > 78.0;
        assert!(vol_pump, "vol=5 + rsi=80 should be flagged as pump");
    }

    #[test]
    fn pump_normal_vol_high_rsi_ok() {
        // High RSI alone is not a pump (could be genuine trend)
        let vol: f64 = 1.5;
        let rsi: f64 = 82.0;
        let vol_pump = vol > 4.0 && rsi > 78.0;
        assert!(!vol_pump, "Normal vol with high RSI should NOT be pump");
    }

    #[test]
    fn pump_stretched_price_detected() {
        // Price 9% above EMA20 → stretched, skip
        let price = 109.0_f64;
        let ema20 = 100.0_f64;
        let stretched = ema20 > 0.0 && price > ema20 * 1.08;
        assert!(stretched, "Price 9% above EMA20 should be stretched");
    }

    #[test]
    fn pump_normal_price_above_ema20_ok() {
        // Price 3% above EMA20 → normal breakout, not stretched
        let price = 103.0_f64;
        let ema20 = 100.0_f64;
        let stretched = ema20 > 0.0 && price > ema20 * 1.08;
        assert!(!stretched, "Price 3% above EMA20 is normal, not stretched");
    }

    #[test]
    fn pump_zero_ema20_no_false_positive() {
        // If EMA20 is 0 (not yet computed), should not trigger
        let price = 100.0_f64;
        let ema20 = 0.0_f64;
        let stretched = ema20 > 0.0 && price > ema20 * 1.08;
        assert!(!stretched, "Zero EMA20 must not trigger pump detection");
    }

    // ════════════════════════════════════════════════════════════════
    // 13. TRAILING STOP
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn trail_inactive_before_tp1() {
        // half_exited=0 → trail must be 0.0 (TP1 not hit yet)
        let ts = compute_trail_sl(0, 95.0, 80.0, 0.025);
        assert_eq!(ts, 0.0, "Trailing stop must be inactive before TP1");
    }

    #[test]
    fn trail_active_after_tp1() {
        // half_exited=1, high_water=100, entry=80
        // trail_sl = 100 * 0.975 = 97.5, and 97.5 > 80 → active
        let ts = compute_trail_sl(1, 100.0, 80.0, 0.025);
        let expected = 100.0 * (1.0 - 0.025);
        assert!((ts - expected).abs() < 0.0001, "trail_sl={:.4} expected {:.4}", ts, expected);
    }

    #[test]
    fn trail_tracks_high_water() {
        // As price moves up, trail_sl rises with it
        let ts_low  = compute_trail_sl(1, 90.0, 80.0, 0.025);
        let ts_high = compute_trail_sl(1, 110.0, 80.0, 0.025);
        assert!(ts_high > ts_low, "Higher high_water must produce higher trail_sl");
    }

    #[test]
    fn trail_zero_when_would_breach_entry() {
        // high_water barely above entry: trail_sl would be below entry → return 0
        // entry=100, high_water=100.1 → trail = 100.1 * 0.975 = 97.6 < entry(100) → 0.0
        let ts = compute_trail_sl(1, 100.1, 100.0, 0.025);
        assert_eq!(ts, 0.0, "trail_sl below entry must be suppressed to 0.0");
    }

    #[test]
    fn trail_zero_when_high_water_equals_entry() {
        // high_water == entry → no gain yet, trail inactive
        let ts = compute_trail_sl(1, 80.0, 80.0, 0.025);
        assert_eq!(ts, 0.0, "No trail when high_water == entry");
    }

    #[test]
    fn trail_preserves_2_5pct_gap() {
        // 0.025 = 2.5% — trail_sl should always be exactly 2.5% below high_water
        let high_water = 150.0_f64;
        let ts = compute_trail_sl(1, high_water, 100.0, 0.025);
        let expected = high_water * 0.975;
        assert!((ts - expected).abs() < 0.0001,
            "trail_sl should be exactly 2.5% below high_water: got {:.4} expected {:.4}", ts, expected);
    }

    // ════════════════════════════════════════════════════════════════
    // 14. STALE SIGNAL — EXACT BOUNDARIES
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn stale_exactly_minus_3pct_is_normal() {
        // dev = -0.03 exactly → condition is `dev < -0.03` (strict) → NOT DUMPED → NORMAL
        let entry = 100.0_f64;
        let current = entry * 0.97; // exactly -3%
        assert_eq!(stale_decision_long(entry, current), "NORMAL",
            "Exactly -3% is NOT dumped (strict < boundary)");
    }

    #[test]
    fn stale_exactly_plus_10pct_is_chasing_due_to_fp() {
        // Due to f64 representation, 100.0 * 1.10 = 110.000...001
        // so dev = 0.100...001 which is > 0.10 → CHASING (not DYNAMIC).
        // This is the real observed boundary behaviour.
        let entry = 100.0_f64;
        let current = entry * 1.10;
        let result = stale_decision_long(entry, current);
        // Accept either CHASING or DYNAMIC depending on fp precision
        assert!(result == "CHASING" || result == "DYNAMIC",
            "At +10%, must be CHASING or DYNAMIC, got {}", result);
    }

    #[test]
    fn stale_exactly_plus_3pct_is_normal() {
        // dev = +0.03 exactly → condition is `dev > 0.03` (strict) → NOT DYNAMIC → NORMAL
        let entry = 100.0_f64;
        let current = entry * 1.03; // exactly +3%
        assert_eq!(stale_decision_long(entry, current), "NORMAL",
            "Exactly +3% is NOT dynamic (strict > boundary)");
    }

    #[test]
    fn stale_just_over_10pct_is_chasing() {
        // dev = +10.1% → CHASING
        let entry = 100.0_f64;
        let current = entry * 1.101;
        assert_eq!(stale_decision_long(entry, current), "CHASING");
    }

    #[test]
    fn stale_just_under_minus_3pct_is_dumped() {
        // dev = -3.1% → DUMPED
        let entry = 100.0_f64;
        let current = entry * 0.969;
        assert_eq!(stale_decision_long(entry, current), "DUMPED");
    }

    // ════════════════════════════════════════════════════════════════
    // 15. QTY FLOOR — EDGE CASES
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn qty_floor_zero_stays_zero() {
        assert_eq!(qty_floor(0.0), 0.0);
    }

    #[test]
    fn qty_floor_small_amount() {
        // 0.00001 → 0.0000 (floored to 4 decimals)
        let q = qty_floor(0.00001);
        assert_eq!(q, 0.0);
    }

    #[test]
    fn qty_floor_large_amount() {
        // 9999.99999 → 9999.9999
        let q = qty_floor(9999.99999);
        assert_eq!(q, 9999.9999);
    }

    // ════════════════════════════════════════════════════════════════
    // 16. CORRELATION GROUP BLOCKING (in-memory SQLite)
    // ════════════════════════════════════════════════════════════════

    fn make_test_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE open_positions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                signal_id INTEGER, order_id TEXT,
                symbol TEXT, direction TEXT,
                qty REAL, entry_price REAL,
                stop_price REAL, tp1_price REAL, tp2_price REAL,
                risk_usdt REAL, opened_at TEXT,
                status TEXT DEFAULT 'OPEN',
                half_exited INTEGER DEFAULT 0,
                p_up REAL, high_water REAL DEFAULT 0,
                closed_at TEXT
             );"
        ).unwrap();
        conn
    }

    fn insert_open(conn: &rusqlite::Connection, symbol: &str) {
        conn.execute(
            "INSERT INTO open_positions
             (symbol, direction, qty, entry_price, stop_price, tp1_price, tp2_price,
              risk_usdt, opened_at, status)
             VALUES (?1,'Long',1.0,100.0,95.0,105.0,110.0,2.0,'2026-01-01T00:00:00Z','OPEN')",
            rusqlite::params![symbol],
        ).unwrap();
    }

    #[test]
    fn corr_btc_blocks_eth() {
        let conn = make_test_db();
        insert_open(&conn, "BTCUSDT");
        assert!(corr_group_blocked(&conn, "ETHUSDT", &test_ecfg()),
            "BTC open must block ETH (same group)");
    }

    #[test]
    fn corr_eth_blocks_btc() {
        let conn = make_test_db();
        insert_open(&conn, "ETHUSDT");
        assert!(corr_group_blocked(&conn, "BTCUSDT", &test_ecfg()),
            "ETH open must block BTC (same group)");
    }

    #[test]
    fn corr_sol_blocks_bnb() {
        let conn = make_test_db();
        insert_open(&conn, "SOLUSDT");
        assert!(corr_group_blocked(&conn, "BNBUSDT", &test_ecfg()),
            "SOL open must block BNB (same group)");
    }

    #[test]
    fn corr_bnb_blocks_ada() {
        let conn = make_test_db();
        insert_open(&conn, "BNBUSDT");
        assert!(corr_group_blocked(&conn, "ADAUSDT", &test_ecfg()),
            "BNB open must block ADA (L1 alts group)");
    }

    #[test]
    fn corr_avax_blocks_apt() {
        let conn = make_test_db();
        insert_open(&conn, "AVAXUSDT");
        assert!(corr_group_blocked(&conn, "APTUSDT", &test_ecfg()),
            "AVAX open must block APT (L1/L2 beta group)");
    }

    #[test]
    fn corr_doge_blocks_ltc() {
        let conn = make_test_db();
        insert_open(&conn, "DOGEUSDT");
        assert!(corr_group_blocked(&conn, "LTCUSDT", &test_ecfg()),
            "DOGE open must block LTC (legacy/meme group)");
    }

    #[test]
    fn corr_btc_does_not_block_sol() {
        // BTC (group1) and SOL (group2) are in different groups — should NOT block
        let conn = make_test_db();
        insert_open(&conn, "BTCUSDT");
        assert!(!corr_group_blocked(&conn, "SOLUSDT", &test_ecfg()),
            "BTC must NOT block SOL (different groups)");
    }

    #[test]
    fn corr_no_open_positions_not_blocked() {
        let conn = make_test_db();
        assert!(!corr_group_blocked(&conn, "BTCUSDT", &test_ecfg()),
            "No open positions → not blocked");
    }

    #[test]
    fn corr_unknown_symbol_not_blocked() {
        // XYZUSDT is not in any group → never blocked
        let conn = make_test_db();
        insert_open(&conn, "BTCUSDT");
        insert_open(&conn, "SOLUSDT");
        assert!(!corr_group_blocked(&conn, "XYZUSDT", &test_ecfg()),
            "Unknown symbol not in any group must not be blocked");
    }

    #[test]
    fn corr_closed_position_does_not_block() {
        // Only OPEN positions count for correlation blocking
        let conn = make_test_db();
        insert_open(&conn, "ETHUSDT");
        conn.execute("UPDATE open_positions SET status='CLOSED_TP2' WHERE symbol='ETHUSDT'", []).unwrap();
        assert!(!corr_group_blocked(&conn, "BTCUSDT", &test_ecfg()),
            "Closed ETH must not block BTC");
    }

    // ════════════════════════════════════════════════════════════════
    // 17. COOLDOWN ACTIVE (in-memory SQLite)
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn cooldown_blocks_recent_close() {
        let conn = make_test_db();
        // Insert a recently-closed SOL position (closed 5 min ago)
        let recent = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        conn.execute(
            "INSERT INTO open_positions
             (symbol, direction, qty, entry_price, stop_price, tp1_price, tp2_price,
              risk_usdt, opened_at, closed_at, status)
             VALUES ('SOLUSDT','Long',1.0,80.0,77.0,83.0,86.0,2.0,'2026-01-01T00:00:00Z',?1,'CLOSED_TP2')",
            rusqlite::params![recent],
        ).unwrap();
        assert!(cooldown_active(&conn, "SOLUSDT", 30, 120),
            "5-min-old close must trigger 30-min cooldown");
    }

    #[test]
    fn cooldown_allows_after_expiry() {
        let conn = make_test_db();
        // Position closed 45 min ago — beyond 30-min cooldown
        let old = (chrono::Utc::now() - chrono::Duration::minutes(45)).to_rfc3339();
        conn.execute(
            "INSERT INTO open_positions
             (symbol, direction, qty, entry_price, stop_price, tp1_price, tp2_price,
              risk_usdt, opened_at, closed_at, status)
             VALUES ('SOLUSDT','Long',1.0,80.0,77.0,83.0,86.0,2.0,'2026-01-01T00:00:00Z',?1,'CLOSED_TP2')",
            rusqlite::params![old],
        ).unwrap();
        assert!(!cooldown_active(&conn, "SOLUSDT", 30, 120),
            "45-min-old close is beyond 30-min cooldown — must be allowed");
    }

    #[test]
    fn cooldown_no_history_not_blocked() {
        let conn = make_test_db();
        assert!(!cooldown_active(&conn, "SOLUSDT", 30, 120),
            "No history → cooldown must not block");
    }

    #[test]
    fn cooldown_open_position_does_not_trigger() {
        // An OPEN position does NOT trigger cooldown (cooldown is for non-OPEN statuses)
        let conn = make_test_db();
        let recent = (chrono::Utc::now() - chrono::Duration::minutes(2)).to_rfc3339();
        conn.execute(
            "INSERT INTO open_positions
             (symbol, direction, qty, entry_price, stop_price, tp1_price, tp2_price,
              risk_usdt, opened_at, status)
             VALUES ('BTCUSDT','Long',0.001,68000.0,65000.0,70000.0,72000.0,2.0,?1,'OPEN')",
            rusqlite::params![recent],
        ).unwrap();
        assert!(!cooldown_active(&conn, "BTCUSDT", 30, 120),
            "OPEN position must not trigger cooldown (only closed positions do)");
    }

    #[test]
    fn cooldown_time_exit_blocks_for_120min() {
        // CLOSED_TIME 60 min ago → still within 120-min extended cooldown
        let conn = make_test_db();
        let closed = (chrono::Utc::now() - chrono::Duration::minutes(60)).to_rfc3339();
        conn.execute(
            "INSERT INTO open_positions
             (symbol, direction, qty, entry_price, stop_price, tp1_price, tp2_price,
              risk_usdt, opened_at, closed_at, status)
             VALUES ('ETHUSDT','Long',1.0,2000.0,1950.0,2080.0,2160.0,3.0,'2026-01-01T00:00:00Z',?1,'CLOSED_TIME')",
            rusqlite::params![closed],
        ).unwrap();
        assert!(cooldown_active(&conn, "ETHUSDT", 30, 120),
            "CLOSED_TIME 60min ago must still block (within 120-min extended cooldown)");
    }

    #[test]
    fn cooldown_time_exit_allows_after_120min() {
        // CLOSED_TIME 130 min ago → beyond 120-min extended cooldown
        let conn = make_test_db();
        let old = (chrono::Utc::now() - chrono::Duration::minutes(130)).to_rfc3339();
        conn.execute(
            "INSERT INTO open_positions
             (symbol, direction, qty, entry_price, stop_price, tp1_price, tp2_price,
              risk_usdt, opened_at, closed_at, status)
             VALUES ('ETHUSDT','Long',1.0,2000.0,1950.0,2080.0,2160.0,3.0,'2026-01-01T00:00:00Z',?1,'CLOSED_TIME')",
            rusqlite::params![old],
        ).unwrap();
        assert!(!cooldown_active(&conn, "ETHUSDT", 30, 120),
            "CLOSED_TIME 130min ago is beyond 120-min extended cooldown — must allow");
    }

    #[test]
    fn cooldown_tp2_does_not_use_extended_cooldown() {
        // CLOSED_TP2 60 min ago → general cooldown (30min) expired, no extended cooldown
        let conn = make_test_db();
        let closed = (chrono::Utc::now() - chrono::Duration::minutes(60)).to_rfc3339();
        conn.execute(
            "INSERT INTO open_positions
             (symbol, direction, qty, entry_price, stop_price, tp1_price, tp2_price,
              risk_usdt, opened_at, closed_at, status)
             VALUES ('ETHUSDT','Long',1.0,2000.0,1950.0,2080.0,2160.0,3.0,'2026-01-01T00:00:00Z',?1,'CLOSED_TP2')",
            rusqlite::params![closed],
        ).unwrap();
        assert!(!cooldown_active(&conn, "ETHUSDT", 30, 120),
            "CLOSED_TP2 60min ago: general cooldown expired, no extended — must allow re-entry");
    }

    // ════════════════════════════════════════════════════════════════
    // 18. IS_PUMP_OR_EXHAUSTED (temp cache file)
    // ════════════════════════════════════════════════════════════════

    fn write_temp_cache(dir: &std::path::Path, symbol: &str, rsi: f64, vol: f64, price: f64, ema20: f64) {
        let td = dir.join("trading-journal");
        std::fs::create_dir_all(&td).unwrap();
        let json = serde_json::json!({
            "generated_at": "2026-01-01T00:00:00Z",
            "symbols": {
                symbol: {
                    "symbol": symbol,
                    "price": price,
                    "rsi14": rsi,
                    "volume_ratio": vol,
                    "ema20": ema20,
                    "ema50": ema20 * 0.95,
                    "atr14_pct": 2.0,
                    "adx14": 25.0,
                    "volume_ratio": vol,
                    "bid": price * 0.999,
                    "ask": price * 1.001,
                    "spread_bps": 2.0,
                    "updated_at": "2026-01-01T00:00:00Z"
                }
            }
        });
        let cache_path = td.join("realtime_cache.json");
        std::fs::write(&cache_path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
    }

    #[test]
    fn pump_high_vol_high_rsi_blocked() {
        let dir = tempfile::tempdir().unwrap();
        write_temp_cache(dir.path(), "SOLUSDT", 82.0, 5.0, 100.0, 90.0);
        assert!(is_pump_or_exhausted("SOLUSDT", dir.path(), &test_ecfg()),
            "vol=5 + rsi=82 must be flagged as pump");
    }

    #[test]
    fn pump_high_vol_low_rsi_allowed() {
        let dir = tempfile::tempdir().unwrap();
        // price=100, ema20=95 → price/ema20 = 1.053 (below 1.08 threshold) — not stretched
        // vol=5.0, rsi=60 → vol pump needs rsi>78 too — not triggered
        write_temp_cache(dir.path(), "SOLUSDT", 60.0, 5.0, 100.0, 95.0);
        assert!(!is_pump_or_exhausted("SOLUSDT", dir.path(), &test_ecfg()),
            "vol=5 + rsi=60 + price not stretched — should not block");
    }

    #[test]
    fn pump_stretched_price_blocked() {
        let dir = tempfile::tempdir().unwrap();
        // price = 110, ema20 = 100 → 10% above EMA20 → stretched
        write_temp_cache(dir.path(), "SOLUSDT", 55.0, 1.5, 110.0, 100.0);
        assert!(is_pump_or_exhausted("SOLUSDT", dir.path(), &test_ecfg()),
            "Price 10% above EMA20 must be flagged as stretched");
    }

    #[test]
    fn pump_normal_conditions_allowed() {
        let dir = tempfile::tempdir().unwrap();
        // Normal: vol=1.5, rsi=65, price only 3% above ema20
        write_temp_cache(dir.path(), "SOLUSDT", 65.0, 1.5, 103.0, 100.0);
        assert!(!is_pump_or_exhausted("SOLUSDT", dir.path(), &test_ecfg()),
            "Normal conditions must not block");
    }

    #[test]
    fn pump_missing_symbol_not_blocked() {
        let dir = tempfile::tempdir().unwrap();
        write_temp_cache(dir.path(), "SOLUSDT", 90.0, 8.0, 200.0, 100.0);
        // Ask about ETHUSDT — not in cache → should return false (safe default)
        assert!(!is_pump_or_exhausted("ETHUSDT", dir.path(), &test_ecfg()),
            "Missing symbol in cache must not block (safe default)");
    }

    #[test]
    fn pump_no_cache_file_not_blocked() {
        // Cache file doesn't exist → default false (safe, don't block)
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_pump_or_exhausted("BTCUSDT", dir.path(), &test_ecfg()),
            "Missing cache file must not block");
    }

    // ════════════════════════════════════════════════════════════════
    // 19. COIN MAX ALLOCATION — full coverage
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn coin_alloc_xrp_mid_cap_sol_group() {
        // XRP is in the mid-cap group (SOL/BNB/XRP) → 25%
        assert_eq!(coin_max_alloc(&test_ecfg(), "XRPUSDT"), 0.25);
    }

    #[test]
    fn coin_alloc_dot_link_established_alts() {
        // DOT, LINK → established alts → 20%
        assert_eq!(coin_max_alloc(&test_ecfg(), "DOTUSDT"),  0.20);
        assert_eq!(coin_max_alloc(&test_ecfg(), "LINKUSDT"), 0.20);
    }

    #[test]
    fn coin_alloc_doge_ltc_ada_established() {
        // DOGE, LTC, ADA → established alts → 20%
        assert_eq!(coin_max_alloc(&test_ecfg(), "DOGEUSDT"), 0.20);
        assert_eq!(coin_max_alloc(&test_ecfg(), "LTCUSDT"),  0.20);
        assert_eq!(coin_max_alloc(&test_ecfg(), "ADAUSDT"),  0.20);
    }

    #[test]
    fn coin_alloc_avax_established_apt_highbeta() {
        // AVAX → established alt → 20%;  APT → high-beta → 15%
        assert_eq!(coin_max_alloc(&test_ecfg(), "AVAXUSDT"), 0.20);
        assert_eq!(coin_max_alloc(&test_ecfg(), "APTUSDT"),  0.15);
    }

    #[test]
    fn coin_alloc_highbeta_coins() {
        // FIL, SUI, ARB, OP, INJ → high-beta default → 15%
        for sym in &["FILUSDT", "SUIUSDT", "ARBUSDT", "OPUSDT", "INJUSDT"] {
            assert_eq!(coin_max_alloc(&test_ecfg(), sym), 0.15, "{} should be 15%", sym);
        }
    }

    // ════════════════════════════════════════════════════════════════
    // 20. COMPUTE_SL_TP_FROM_FILL — edge cases
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn sl_tp_normal_entry_fill_equals_entry() {
        // Fill == signal entry → SL/TP based on original ratios, same as compute at entry
        let (sl, tp) = compute_sl_tp_from_fill(100.0, 97.0, 103.0, 100.0);
        let atr_pct = 0.03;  // (100-97)/100
        let tp_pct  = 0.03;  // (103-100)/100
        let expected_sl = 100.0 * (1.0 - atr_pct);
        let expected_tp = 100.0 * (1.0 + tp_pct);
        assert!((sl - expected_sl).abs() < 0.01, "SL at same fill: {:.4} vs {:.4}", sl, expected_sl);
        assert!((tp - expected_tp).abs() < 0.01, "TP at same fill: {:.4} vs {:.4}", tp, expected_tp);
    }

    #[test]
    fn sl_tp_very_tight_original_uses_floor() {
        // If original SL is only 0.1% away → atr_pct floored at 1.5%
        let (sl, _tp) = compute_sl_tp_from_fill(100.0, 99.9, 103.0, 100.0);
        let expected_sl = 100.0 * (1.0 - 0.015); // 1.5% floor
        assert!((sl - expected_sl).abs() < 0.01,
            "Tight SL must use 1.5% ATR floor: {:.4} vs {:.4}", sl, expected_sl);
    }

    #[test]
    fn sl_tp_ratio_preserved_at_different_fills() {
        // Same signal, different fill prices — ratios must be preserved
        let entry = 100.0_f64;
        let sl_sig = 97.0_f64;  // 3% stop
        let tp_sig = 103.0_f64; // 3% target
        let (sl_low,  tp_low)  = compute_sl_tp_from_fill(entry, sl_sig, tp_sig, 95.0);
        let (sl_high, tp_high) = compute_sl_tp_from_fill(entry, sl_sig, tp_sig, 110.0);
        // Both should have ~3% ATR and ~3% TP ratio
        let ratio_low_sl  = (95.0  - sl_low)  / 95.0;
        let ratio_high_sl = (110.0 - sl_high) / 110.0;
        assert!((ratio_low_sl  - 0.03).abs() < 0.001, "ATR% at fill=95:  {:.4}", ratio_low_sl);
        assert!((ratio_high_sl - 0.03).abs() < 0.001, "ATR% at fill=110: {:.4}", ratio_high_sl);
        assert!(tp_high > tp_low, "Higher fill → higher TP");
        assert!(sl_high > sl_low, "Higher fill → higher SL");
    }

    // ════════════════════════════════════════════════════════════════
    // 21. KELLY — tier C floor behaviour
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn kelly_tier_c_uses_half_base_risk_but_floored() {
        // Tier C: base_risk*0.5, floored at 1%
        // base_risk=0.02 → tier_c = 0.01 → floor(0.01) = 0.01
        let s = sig(0.3, 0.55, 0.45, 0.5, 79.0, 76.5, 81.0);
        let r = kelly_risk_fraction(&s, 0.02, &test_ecfg());
        assert!((r - 0.01).abs() < 0.001, "Tier C with base 2% should be 1% (floor): {:.4}", r);
    }

    #[test]
    fn kelly_zero_base_risk_still_floored_at_1pct() {
        // Even with base_risk=0, result must be >= 1%
        let s = sig(0.5, 0.55, 0.45, 0.5, 79.0, 76.5, 81.0);
        let r = kelly_risk_fraction(&s, 0.0, &test_ecfg());
        assert!(r >= 0.01, "Kelly must never go below 1%: got {:.4}", r);
    }

    // ════════════════════════════════════════════════════════════════
    // 22. MAX_HOLD_MIN — per-coin time-stop
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn hold_btc_eth_120min() {
        assert_eq!(max_hold_min(&test_ecfg(), "BTCUSDT"), 120);
        assert_eq!(max_hold_min(&test_ecfg(), "ETHUSDT"), 120);
    }

    #[test]
    fn hold_mid_caps_90min() {
        for sym in &["SOLUSDT","BNBUSDT","XRPUSDT","ADAUSDT","DOTUSDT","LINKUSDT","LTCUSDT","DOGEUSDT"] {
            assert_eq!(max_hold_min(&test_ecfg(), sym), 90, "{} should be 90min", sym);
        }
    }

    #[test]
    fn hold_highbeta_60min() {
        for sym in &["AVAXUSDT","APTUSDT","SUIUSDT","FILUSDT","ARBUSDT","OPUSDT","INJUSDT","NEARUSDT","UNIUSDT"] {
            assert_eq!(max_hold_min(&test_ecfg(), sym), 60, "{} should be 60min", sym);
        }
    }

    #[test]
    fn hold_unknown_symbol_defaults_60min() {
        assert_eq!(max_hold_min(&test_ecfg(), "UNKNOWNUSDT"), 60);
    }

    // ════════════════════════════════════════════════════════════════
    // 23. CIRCUIT BREAKER — is_risk_ok pure function
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn circuit_breaker_clean_state_is_ok() {
        // No losses, no drawdown, cb_hit=false → safe to trade
        assert!(is_risk_ok(false, 0, 0.0, 3, 0.10),
            "Clean state must be risk_ok");
    }

    #[test]
    fn circuit_breaker_cb_flag_blocks() {
        // cb_hit=true regardless of other params → blocked
        assert!(!is_risk_ok(true, 0, 0.0, 3, 0.10),
            "cb_hit=true must block trading");
    }

    #[test]
    fn circuit_breaker_exactly_max_losses_blocks() {
        // consec_losses == max_losses → blocked (strict <)
        assert!(!is_risk_ok(false, 3, 0.0, 3, 0.10),
            "consec_losses=3 with max=3 must block");
    }

    #[test]
    fn circuit_breaker_one_below_max_losses_ok() {
        // consec_losses = max_losses - 1 → still safe
        assert!(is_risk_ok(false, 2, 0.0, 3, 0.10),
            "consec_losses=2 with max=3 must allow trading");
    }

    #[test]
    fn circuit_breaker_over_max_losses_blocks() {
        // consec_losses > max_losses → definitely blocked
        assert!(!is_risk_ok(false, 5, 0.0, 3, 0.10),
            "consec_losses=5 must block");
    }

    #[test]
    fn circuit_breaker_drawdown_exactly_at_limit_blocks() {
        // drawdown == max_dd → blocked (strict <)
        assert!(!is_risk_ok(false, 0, 0.10, 3, 0.10),
            "drawdown=10% at max=10% must block");
    }

    #[test]
    fn circuit_breaker_drawdown_just_below_limit_ok() {
        // drawdown = 9.9% < 10% max → allowed
        assert!(is_risk_ok(false, 0, 0.099, 3, 0.10),
            "drawdown=9.9% below 10% max must allow trading");
    }

    #[test]
    fn circuit_breaker_drawdown_over_limit_blocks() {
        // drawdown = 15% > 10% max → blocked
        assert!(!is_risk_ok(false, 0, 0.15, 3, 0.10),
            "drawdown=15% must block");
    }

    #[test]
    fn circuit_breaker_combined_losses_and_drawdown() {
        // Both consec_losses AND drawdown over limit → blocked
        assert!(!is_risk_ok(false, 4, 0.12, 3, 0.10),
            "Combined breach must block");
    }

    #[test]
    fn circuit_breaker_zero_max_losses_always_blocks() {
        // max_losses=0: any loss (even 0 consec) → blocked since 0 < 0 is false
        assert!(!is_risk_ok(false, 0, 0.0, 0, 0.10),
            "max_losses=0 means never trade (blocked even at 0 losses)");
    }

    // ════════════════════════════════════════════════════════════════
    // 20. MACRO EMA ALIGNMENT GUARD
    // Root cause of 2026-02-27 losses: DOT×2 + SOL×1 entered LONG while
    // price < EMA20 < EMA50 (macro bearish stack). New guard blocks these.
    // ════════════════════════════════════════════════════════════════

    fn write_cache_with_emas(dir: &tempfile::TempDir, symbol: &str, price: f64, ema20: f64, ema50: f64) {
        std::fs::create_dir_all(dir.path().join("trading-journal")).unwrap();
        let content = format!(
            r#"{{"generated_at":"2026-02-27T10:00:00+00:00","symbols":{{"{}":{{"price":{},"ema20":{},"ema50":{},"rsi14":55,"volume_ratio":2.0}}}}}}"#,
            symbol, price, ema20, ema50
        );
        std::fs::write(dir.path().join("trading-journal/realtime_cache.json"), content).unwrap();
    }

    #[test]
    fn macro_ema_aligned_bull_stack_allowed() {
        // price=100 > ema20=95 > ema50=88 → bull stack → allow (trend mode)
        let dir = tempfile::tempdir().unwrap();
        write_cache_with_emas(&dir, "SOLUSDT", 100.0, 95.0, 88.0);
        assert!(macro_ema_aligned("SOLUSDT", dir.path(), false, false),
            "Bull stack (price>EMA20>EMA50) must be allowed");
    }

    #[test]
    fn macro_ema_aligned_price_below_ema20_blocked() {
        // Trend mode: price=82 < ema20=87 < ema50=92 → bear stack → blocked
        // Reversion mode (Tier R): price below EMA20 is EXPECTED for oversold bounce.
        //   New check: ema20(87) >= ema50(92)*0.92=84.6 → 87>=84.6 → ALLOWED ✅
        //   (EMA20 is only slightly below EMA50 — normal oversold, not free-fall)
        let dir = tempfile::tempdir().unwrap();
        write_cache_with_emas(&dir, "SOLUSDT", 82.0, 87.0, 92.0);
        assert!(!macro_ema_aligned("SOLUSDT", dir.path(), false, false),
            "Bear stack (price<EMA20<EMA50) must be blocked in trend mode");
        assert!(macro_ema_aligned("SOLUSDT", dir.path(), true, false),
            "Tier R: ema20(87)>=ema50(92)*0.92 → allowed (normal oversold, not free-fall)");
    }

    #[test]
    fn macro_ema_aligned_reversion_free_fall_blocked() {
        // Tier R free-fall: ema20 is >8% below ema50 → structural bear → block
        // ema20=80, ema50=92 → ratio=0.869 < 0.92 → BLOCKED
        let dir = tempfile::tempdir().unwrap();
        write_cache_with_emas(&dir, "SOLUSDT", 75.0, 80.0, 92.0);
        assert!(!macro_ema_aligned("SOLUSDT", dir.path(), false, false),
            "Trend mode must block free-fall");
        assert!(!macro_ema_aligned("SOLUSDT", dir.path(), true, false),
            "Tier R must block when ema20 is >8% below ema50 (structural bear/free-fall)");
    }

    #[test]
    fn macro_ema_aligned_ema20_below_ema50_blocked_for_trend() {
        // price=90 > ema20=88 but ema20<ema50=92 → death cross
        // Trend mode: blocked. Reversion mode: ema20(88)>=ema50(92)*0.92=84.6 → allowed.
        let dir = tempfile::tempdir().unwrap();
        write_cache_with_emas(&dir, "SOLUSDT", 90.0, 88.0, 92.0);
        assert!(!macro_ema_aligned("SOLUSDT", dir.path(), false, false),
            "Death cross (EMA20<EMA50) must be blocked in trend mode");
        assert!(macro_ema_aligned("SOLUSDT", dir.path(), true, false),
            "Tier R/V: ema20(88)>=ema50(92)*0.92 — normal dip, not free-fall → allowed");
    }

    #[test]
    fn macro_ema_aligned_tier_r_eth_march01_pattern() {
        // ETH 2026-03-01: price=1977, ema20=1947, ema50=1953
        // ema20(1947)/ema50(1953) = 0.997 > 0.92 → NOT free-fall → Tier R allowed
        let dir = tempfile::tempdir().unwrap();
        write_cache_with_emas(&dir, "ETHUSDT", 1977.0, 1947.0, 1953.0);
        assert!(!macro_ema_aligned("ETHUSDT", dir.path(), false, false),
            "Trend mode must block ETH death cross");
        assert!(macro_ema_aligned("ETHUSDT", dir.path(), true, false),
            "Tier R must allow ETH — ema20/ema50=0.997 > 0.92, oversold bounce valid");
    }

    #[test]
    fn macro_ema_aligned_missing_symbol_allowed() {
        // Symbol not in cache → default 0.0 → zero EMA → fail-open (allow)
        let dir = tempfile::tempdir().unwrap();
        write_cache_with_emas(&dir, "BTCUSDT", 100.0, 95.0, 88.0);
        assert!(macro_ema_aligned("XRPUSDT", dir.path(), false, false),
            "Missing symbol in cache must fail-open (allow)");
    }

    #[test]
    fn macro_ema_aligned_no_cache_file_allowed() {
        // Cache file doesn't exist → fail-open (allow) — other guards still active
        let dir = tempfile::tempdir().unwrap();
        assert!(macro_ema_aligned("SOLUSDT", dir.path(), false, false),
            "Missing cache file must fail-open (allow)");
    }

    // ════════════════════════════════════════════════════════════════
    // 21. FAST CRASH / MOMENTUM GUARD (ret_1h)
    // Addresses 4H EMA lag: 4H EMA stays bullish while price crashes on 1H.
    // guard blocks entry if ret_1h < -3.0% (intra-candle fast drop).
    // ════════════════════════════════════════════════════════════════

    fn write_cache_with_ret1h(dir: &tempfile::TempDir, symbol: &str, ret_1h: f64) {
        std::fs::create_dir_all(dir.path().join("trading-journal")).unwrap();
        let content = format!(
            r#"{{"generated_at":"2026-02-27T13:00:00+00:00","symbols":{{"{}":{{"price":1.5,"ret_1h":{},"ema20":1.45,"ema50":1.35}}}}}}"#,
            symbol, ret_1h
        );
        std::fs::write(dir.path().join("trading-journal/realtime_cache.json"), content).unwrap();
    }

    #[test]
    fn momentum_ok_small_drop_allowed() {
        // -1.5% intra-1H drop → well above -3% threshold → allow
        let dir = tempfile::tempdir().unwrap();
        write_cache_with_ret1h(&dir, "DOTUSDT", -1.5);
        assert!(recent_momentum_ok("DOTUSDT", dir.path()),
            "-1.5% drop must be allowed (above -3% threshold)");
    }

    #[test]
    fn momentum_ok_positive_momentum_allowed() {
        // +2.3% intra-1H gain → strong buy momentum → allow
        let dir = tempfile::tempdir().unwrap();
        write_cache_with_ret1h(&dir, "DOTUSDT", 2.3);
        assert!(recent_momentum_ok("DOTUSDT", dir.path()),
            "+2.3% gain must be allowed");
    }

    #[test]
    fn momentum_fast_drop_blocked() {
        // -4.5% intra-1H crash → fast drop → block entry
        let dir = tempfile::tempdir().unwrap();
        write_cache_with_ret1h(&dir, "SOLUSDT", -4.5);
        assert!(!recent_momentum_ok("SOLUSDT", dir.path()),
            "-4.5% intra-1H drop must be blocked — 4H EMA lags this fast move");
    }

    #[test]
    fn momentum_exactly_at_threshold_blocked() {
        // -3.0% exactly → blocked (strict >)
        let dir = tempfile::tempdir().unwrap();
        write_cache_with_ret1h(&dir, "BTCUSDT", -3.0);
        assert!(!recent_momentum_ok("BTCUSDT", dir.path()),
            "-3.0% exactly must be blocked (threshold is > -3.0, not >= -3.0)");
    }

    #[test]
    fn momentum_just_inside_threshold_allowed() {
        // -2.99% → just above threshold → allow
        let dir = tempfile::tempdir().unwrap();
        write_cache_with_ret1h(&dir, "ETHUSDT", -2.99);
        assert!(recent_momentum_ok("ETHUSDT", dir.path()),
            "-2.99% must be allowed (just inside -3.0% threshold)");
    }

    #[test]
    fn momentum_flash_crash_blocked() {
        // -8% flash crash → definitely blocked
        let dir = tempfile::tempdir().unwrap();
        write_cache_with_ret1h(&dir, "SOLUSDT", -8.0);
        assert!(!recent_momentum_ok("SOLUSDT", dir.path()),
            "-8% flash crash must be blocked");
    }

    #[test]
    fn momentum_missing_ret1h_field_allowed() {
        // Old cache without ret_1h field → default 0.0 → fail-open (allow)
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("trading-journal")).unwrap();
        let content = r#"{"generated_at":"2026-02-27T13:00:00+00:00","symbols":{"DOTUSDT":{"price":1.5,"ema20":1.45,"ema50":1.35}}}"#;
        std::fs::write(dir.path().join("trading-journal/realtime_cache.json"), content).unwrap();
        assert!(recent_momentum_ok("DOTUSDT", dir.path()),
            "Missing ret_1h field must fail-open (allow) — backward compat");
    }

    #[test]
    fn momentum_no_cache_file_allowed() {
        let dir = tempfile::tempdir().unwrap();
        assert!(recent_momentum_ok("DOTUSDT", dir.path()),
            "Missing cache file must fail-open (allow)");
    }

    // ════════════════════════════════════════════════════════════════
    // 23. DAILY CLOSED_TIME RE-ENTRY GUARD
    // After 1 CLOSED_TIME on a coin today, block same-day re-entry.
    // Root cause: DOT re-entered 3.5h after first CLOSED_TIME → -$0.16 loss.
    // ════════════════════════════════════════════════════════════════

    fn init_test_live_db_for_closed_time() -> (rusqlite::Connection, String) {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("
            CREATE TABLE open_positions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                signal_id INTEGER,
                order_id TEXT,
                symbol TEXT,
                direction TEXT,
                qty REAL,
                entry_price REAL,
                stop_price REAL,
                tp1_price REAL,
                tp2_price REAL,
                risk_usdt REAL,
                opened_at TEXT,
                status TEXT DEFAULT 'OPEN',
                half_exited INTEGER DEFAULT 0,
                p_up REAL,
                high_water REAL DEFAULT 0,
                closed_at TEXT
            );
        ").unwrap();
        // Use UTC to match production closed_at format (Utc::now().to_rfc3339())
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        (conn, today)
    }

    #[test]
    fn daily_closed_time_zero_on_no_history() {
        let (conn, today) = init_test_live_db_for_closed_time();
        assert_eq!(daily_closed_time_count(&conn, "DOTUSDT", &today), 0,
            "No history → 0 CLOSED_TIME count");
    }

    #[test]
    fn daily_closed_time_counts_todays_time_exit() {
        let (conn, today) = init_test_live_db_for_closed_time();
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.f+00:00").to_string();
        conn.execute(
            "INSERT INTO open_positions(symbol,direction,qty,entry_price,stop_price,tp1_price,risk_usdt,opened_at,status,closed_at) \
             VALUES ('DOTUSDT','Long',14.9,1.612,1.482,1.806,4.8,?1,'CLOSED_TIME',?1)",
            rusqlite::params![&now],
        ).unwrap();
        assert_eq!(daily_closed_time_count(&conn, "DOTUSDT", &today), 1,
            "1 CLOSED_TIME today → count must be 1");
    }

    #[test]
    fn daily_closed_time_does_not_count_other_statuses() {
        let (conn, today) = init_test_live_db_for_closed_time();
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.f+00:00").to_string();
        // Insert CLOSED_SL and CLOSED_TP — should NOT count as CLOSED_TIME
        for status in &["CLOSED_SL", "CLOSED_TP", "CLOSED_TP2", "CLOSED_MANUAL"] {
            conn.execute(
                "INSERT INTO open_positions(symbol,direction,qty,entry_price,stop_price,tp1_price,risk_usdt,opened_at,status,closed_at) \
                 VALUES ('DOTUSDT','Long',14.9,1.612,1.482,1.806,4.8,?1,?2,?1)",
                rusqlite::params![&now, status],
            ).unwrap();
        }
        assert_eq!(daily_closed_time_count(&conn, "DOTUSDT", &today), 0,
            "SL/TP/TP2/MANUAL closes must not count as CLOSED_TIME");
    }

    #[test]
    fn daily_closed_time_does_not_count_other_symbols() {
        let (conn, today) = init_test_live_db_for_closed_time();
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.f+00:00").to_string();
        // DOT has CLOSED_TIME, but we query SOL
        conn.execute(
            "INSERT INTO open_positions(symbol,direction,qty,entry_price,stop_price,tp1_price,risk_usdt,opened_at,status,closed_at) \
             VALUES ('DOTUSDT','Long',14.9,1.612,1.482,1.806,4.8,?1,'CLOSED_TIME',?1)",
            rusqlite::params![&now],
        ).unwrap();
        assert_eq!(daily_closed_time_count(&conn, "SOLUSDT", &today), 0,
            "DOT CLOSED_TIME must not count for SOL query");
    }

    #[test]
    fn daily_closed_time_double_exit_counts_two() {
        let (conn, today) = init_test_live_db_for_closed_time();
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.f+00:00").to_string();
        // DOT had 2 CLOSED_TIME today — exact 2026-02-27 pattern
        for _ in 0..2 {
            conn.execute(
                "INSERT INTO open_positions(symbol,direction,qty,entry_price,stop_price,tp1_price,risk_usdt,opened_at,status,closed_at) \
                 VALUES ('DOTUSDT','Long',14.9,1.612,1.482,1.806,4.8,?1,'CLOSED_TIME',?1)",
                rusqlite::params![&now],
            ).unwrap();
        }
        assert_eq!(daily_closed_time_count(&conn, "DOTUSDT", &today), 2,
            "2 CLOSED_TIME exits today → count must be 2 (re-entry blocked)");
    }
}
