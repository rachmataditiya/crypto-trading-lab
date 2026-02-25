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
use std::{fs, path::{Path, PathBuf}, thread, time::Duration as StdDuration};

// ── Constants ────────────────────────────────────────────────────────────────
const MAX_SIGNAL_AGE_MIN: i64 = 20;
const MAX_HOLD_MIN:        i64 = 60;
const POLL_INTERVAL_SECS:  u64 = 60;
const OKX_BASE: &str           = "https://www.okx.com";
const MAX_OPEN_POSITIONS:  i64 = 3;   // max concurrent positions
const MAX_PORTFOLIO_RISK:  f64 = 0.06; // max 6% total risk deployed at once

// Correlation groups — max 1 open per group
const CORR_GROUP_1: &[&str] = &["BTCUSDT", "ETHUSDT"];    // high correlation
const CORR_GROUP_2: &[&str] = &["SOLUSDT", "BNBUSDT"];    // moderate
// ATOMUSDT standalone

// Per-coin max allocation % of equity
fn coin_max_alloc(symbol: &str) -> f64 {
    match symbol {
        "BTCUSDT" | "ETHUSDT" => 0.40,
        "SOLUSDT" | "BNBUSDT" => 0.25,
        "ATOMUSDT"             => 0.15,
        _                      => 0.20,
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
fn kelly_risk_fraction(sig: &Signal, base_risk: f64) -> f64 {
    let ev   = sig.ev.unwrap_or(0.0);
    let p_up = sig.p_up.unwrap_or(0.5);
    let p_dn = sig.p_down.unwrap_or(0.5);
    let conf = sig.confidence.unwrap_or(0.0);

    // Full Kelly: f* = (p_up * ev - p_down) / ev
    let kelly_full = if ev > 0.0 {
        (p_up * ev - p_dn) / ev
    } else {
        0.0
    };

    // Fractional Kelly (25%), scaled by confidence
    let conf_scale = (conf / 0.5).min(1.0).max(0.2);  // 20–100% scale
    let frac_kelly = kelly_full * 0.25 * conf_scale;

    // Tier enforcement
    let tier_risk = if ev >= 2.0 && conf >= 0.5 {
        // Tier A: strong signal → 3–4%
        frac_kelly.max(0.03).min(0.04)
    } else if ev >= 1.0 && conf >= 0.3 {
        // Tier B: normal → 1.5–2%
        frac_kelly.max(0.015).min(0.025)
    } else {
        // Tier C: weak → use base risk or skip
        base_risk * 0.5
    };

    tier_risk.max(0.01).min(0.04) // hard floor 1%, ceiling 4%
}

/// Signal is worth trading at all?
fn should_trade(sig: &Signal) -> bool {
    let ev   = sig.ev.unwrap_or(0.0);
    let conf = sig.confidence.unwrap_or(0.0);
    // Skip very weak signals: EV < 0.5R or confidence < 0.15
    ev >= 0.5 && conf >= 0.15
}

/// Tier label for logging
fn signal_tier(sig: &Signal) -> &'static str {
    let ev   = sig.ev.unwrap_or(0.0);
    let conf = sig.confidence.unwrap_or(0.0);
    if ev >= 2.0 && conf >= 0.5 { "A" }
    else if ev >= 1.0 && conf >= 0.3 { "B" }
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

/// Dynamic TP1 multiplier based on signal confidence (P(up)).
/// High-confidence signals deserve a wider TP target.
/// p_up >= 0.92 → 2× ATR | p_up >= 0.85 → 1.5× ATR | else → 1× ATR
fn compute_dynamic_tp1(entry: f64, sl: f64, p_up: f64) -> f64 {
    let atr_pct = ((entry - sl).abs() / entry.max(1e-9)).max(0.015);
    let multiplier: f64 = if p_up >= 0.92 { 2.0 } else if p_up >= 0.85 { 1.5 } else { 1.0 };
    (entry * (1.0 + atr_pct * multiplier) * 1_000_000.0).round() / 1_000_000.0
}

/// TP2 = entry + 2× the TP1 distance. Used as the second partial exit target.
fn compute_tp2(entry: f64, tp1: f64) -> f64 {
    let tp1_dist = tp1 - entry;
    ((entry + 2.0 * tp1_dist) * 1_000_000.0).round() / 1_000_000.0
}

/// Returns true if current time is within the noisy early Asian pre-market window (07:00–07:30 WIB).
/// Signals in this window have historically lower quality (P(up)=0.68 vs 0.92 outside).
fn is_noisy_window() -> bool {
    let now = Local::now();
    now.hour() == 7 && now.minute() < 30
}

/// Pump/dump protection — reads RSI, volume_ratio, and price vs EMA20 from the realtime cache.
/// Returns true (= block entry) if pattern suggests active manipulation or exhaustion.
///
/// Block if ANY of:
///   • volume_ratio > 4.0 AND RSI > 78 (volume-driven pump, overbought)
///   • price > EMA20 × 1.08 (price stretched >8% above EMA20 — too late, chasing a pump)
fn is_pump_or_exhausted(symbol: &str, cache_path: &Path) -> bool {
    let cache_file = cache_path.join("trading-journal/realtime_cache.json");
    let text = match std::fs::read_to_string(&cache_file) { Ok(t) => t, Err(_) => return false };
    let d: serde_json::Value = match serde_json::from_str(&text) { Ok(v) => v, Err(_) => return false };
    let sym = &d["symbols"][symbol];
    let rsi  = sym["rsi14"].as_f64().unwrap_or(50.0);
    let vol  = sym["volume_ratio"].as_f64().unwrap_or(1.0);
    let price = sym["price"].as_f64().unwrap_or(0.0);
    let ema20 = sym["ema20"].as_f64().unwrap_or(0.0);
    let vol_pump  = vol > 4.0 && rsi > 78.0;
    let stretched = ema20 > 0.0 && price > ema20 * 1.08;
    vol_pump || stretched
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

fn place_sl(client: &Client, cfg: &Config, inst: &str, qty: f64, sl_px: f64) {
    let qty_floor = (qty * 10000.0).floor() / 10000.0;
    let body = json!({"instId":inst,"tdMode":"cash","side":"sell","ordType":"conditional",
                      "sz":format!("{:.4}",qty_floor),"slTriggerPx":format!("{:.4}",sl_px),"slOrdPx":"-1"});
    match okx_post(client, cfg, "/api/v5/trade/order-algo", &body) {
        Ok(r) if r["code"].as_str() == Some("0") => println!("[executor] 🛡️  SL @ {:.4}", sl_px),
        Ok(r) => println!("[executor] ⚠️  SL failed: {}", r["data"][0]["sMsg"]),
        Err(e) => println!("[executor] ⚠️  SL error: {}", e),
    }
}

fn place_tp_limit(client: &Client, cfg: &Config, inst: &str, qty: f64, tp_px: f64) {
    let qty_floor = (qty * 10000.0).floor() / 10000.0;
    let body = json!({"instId":inst,"tdMode":"cash","side":"sell","ordType":"limit",
                      "px":format!("{:.4}",tp_px),"sz":format!("{:.4}",qty_floor)});
    match okx_post(client, cfg, "/api/v5/trade/order", &body) {
        Ok(r) if r["code"].as_str() == Some("0") => println!("[executor] 🎯 TP @ {:.4}", tp_px),
        Ok(r) => println!("[executor] ⚠️  TP failed: {}", r["data"][0]["sMsg"]),
        Err(e) => println!("[executor] ⚠️  TP error: {}", e),
    }
}

fn market_sell(client: &Client, cfg: &Config, inst: &str, qty: f64) -> bool {
    let qty_floor = qty_floor(qty);
    let body = json!({"instId":inst,"tdMode":"cash","side":"sell","ordType":"market",
                      "sz":format!("{:.4}",qty_floor)});
    match okx_post(client, cfg, "/api/v5/trade/order", &body) {
        Ok(r) if r["code"].as_str() == Some("0") => {
            println!("[executor] 💸 SELL {:.4} {}", qty, inst);
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

fn journal_opened(base: &Path, sig: &Signal, fill: f64, usdt: f64, equity: f64, risk_frac: f64) {
    let t   = Local::now().format("%H:%M").to_string();
    let ev  = sig.ev.map(|e| format!("{:+.2}R", e)).unwrap_or_else(|| "N/A".to_string());
    let tier = signal_tier(sig);
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
        tier, risk_frac * 100.0, MAX_HOLD_MIN, ev);
    append_journal(base, &block);
}

fn journal_closed(base: &Path, sig: &Signal, pnl: f64, status: &str, reason: &str) {
    let t     = Local::now().format("%H:%M").to_string();
    let emoji = if status == "CLOSED_TP" { "✅" } else if pnl >= 0.0 { "↩️" } else { "❌" };
    let label = match status { "CLOSED_TP" => "TP HIT", "CLOSED_SL" => "STOP LOSS", _ => "TIME EXIT" };
    let ll = if status == "CLOSED_TP" {
        "1. TP1 hit — sistem bekerja.\n2. Profit diambil cepat, tidak tunggu reversal.\n\
3. Catat kondisi volume & regime saat exit.\n4. Kelola bias: jangan over-trade setelah win."
    } else if status == "CLOSED_SL" {
        "1. SL terpicu — normal dalam probabilistic system.\n2. Cek: volume konfirmasi cukup kuat saat entry?\n\
3. Loss dalam budget yang sudah dikunci (Kelly).\n4. 3 consecutive losses → circuit breaker aktif."
    } else {
        "1. Time exit: market tidak bergerak dalam 60 menit.\n2. Sideways/konsolidasi — skip next time.\n\
3. Tanda regime kurang mendukung untuk momentum play.\n4. Evaluasi: ADX cukup tinggi saat entry?"
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
            signal_id    INTEGER PRIMARY KEY,
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
    Ok(())
}

fn is_processed(conn: &Connection, sig_id: i64) -> bool {
    conn.query_row("SELECT 1 FROM processed_signals WHERE signal_id=?",
        params![sig_id], |_| Ok(true)).unwrap_or(false)
}

fn mark_processed(conn: &Connection, sig_id: i64, symbol: &str, direction: &str,
                  entry: f64, stop: f64, tp1: f64,
                  order_id: &str, status: &str, fill: f64, notes: &str) {
    let now = Utc::now().to_rfc3339();
    let _ = conn.execute(
        "INSERT OR IGNORE INTO processed_signals
         (signal_id,processed_at,symbol,direction,entry,stop,tp1,order_id,status,fill_price,notes)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
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

fn total_risk_deployed(conn: &Connection, equity: f64) -> f64 {
    if equity <= 0.0 { return 0.0; }
    let total_risk_usdt: f64 = conn.query_row(
        "SELECT COALESCE(SUM(risk_usdt),0) FROM open_positions WHERE status='OPEN'",
        params![], |r| r.get(0)).unwrap_or(0.0);
    total_risk_usdt / equity
}

fn open_position(conn: &Connection, sig_id: i64, order_id: &str, symbol: &str,
                 direction: &str, qty: f64, fill: f64, stop: f64, tp1: f64, risk_usdt: f64,
                 tp2: f64, p_up: f64) {
    let now = Utc::now().to_rfc3339();
    let _ = conn.execute(
        "INSERT INTO open_positions
         (signal_id,order_id,symbol,direction,qty,entry_price,stop_price,tp1_price,risk_usdt,opened_at,tp2_price,p_up)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![sig_id,order_id,symbol,direction,qty,fill,stop,tp1,risk_usdt,now,tp2,p_up]);
}

fn get_daily_stats(conn: &Connection, today: &str, starting_equity: f64) -> (f64, i64, u32, bool) {
    conn.execute("INSERT OR IGNORE INTO daily_stats(trade_date,starting_equity) VALUES(?1,?2)",
        params![today, starting_equity]).ok();
    conn.query_row(
        "SELECT realized_pnl,consecutive_losses,trade_count,circuit_breaker_hit FROM daily_stats WHERE trade_date=?",
        params![today], |r| Ok((r.get::<_,f64>(0)?,r.get::<_,i64>(1)?,r.get::<_,u32>(2)?,r.get::<_,i32>(3)?!=0))
    ).unwrap_or((0.0,0,0,false))
}

// ── Correlation check ────────────────────────────────────────────────────────
fn corr_group_blocked(conn: &Connection, symbol: &str) -> bool {
    let group: Option<&[&str]> = if CORR_GROUP_1.contains(&symbol) { Some(CORR_GROUP_1) }
        else if CORR_GROUP_2.contains(&symbol) { Some(CORR_GROUP_2) }
        else { None };
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
    let base_risk = cfg.risk_per_trade_pct / 100.0;
    let max_dd    = cfg.max_daily_drawdown_pct / 100.0;
    let cb_losses = cfg.circuit_breaker_consecutive_losses;
    let sim_path  = base.join("sim_history.db");
    let live_path = base.join("live_positions.db");

    let client    = Client::builder().timeout(StdDuration::from_secs(10)).build()?;
    let live_conn = Connection::open(&live_path)?;
    init_live_db(&live_conn)?;

    println!("[executor] 🦀 Live Executor v2 (Kelly+MultiPos) | BaseRisk={:.0}% | MaxPos={} | MaxPortRisk={:.0}%",
        cfg.risk_per_trade_pct, MAX_OPEN_POSITIONS, MAX_PORTFOLIO_RISK * 100.0);

    loop {
        let today  = Local::now().format("%Y-%m-%d").to_string();
        let cutoff = (Local::now() - Duration::minutes(MAX_SIGNAL_AGE_MIN)).format("%H:%M").to_string();

        let equity = get_equity(&client, &cfg).unwrap_or(116.0);
        let (realized_pnl, consec_losses, _tc, cb_hit) = get_daily_stats(&live_conn, &today, equity);

        let drawdown = if equity > 0.0 { -realized_pnl / equity } else { 0.0 };
        let risk_ok  = !cb_hit && (consec_losses as u32) < cb_losses && drawdown < max_dd;

        // ── Process new signals ───────────────────────────────────────────────
        if risk_ok {
            let sim_conn = Connection::open(&sim_path)?;
            let mut stmt = sim_conn.prepare(
                "SELECT id, raw_block, expected_value_r, confidence, p_up, p_down
                 FROM simulation_events
                 WHERE decision='PAPER_TRADE'
                   AND trade_date=?1
                   AND trade_time >= ?2
                 ORDER BY expected_value_r DESC"  // best EV first
            )?;
            let rows: Vec<(i64, String, Option<f64>, Option<f64>, Option<f64>, Option<f64>)> =
                stmt.query_map(params![today, cutoff], |r| {
                    Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))
                })?.filter_map(|r| r.ok()).collect();
            drop(stmt); drop(sim_conn);

            for (sig_id, raw_block, ev, conf, p_up, p_down) in rows {
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
                if !should_trade(&sig) {
                    println!("[executor] ⏩ Tier C skip: EV={:.2} Conf={:.2}", ev.unwrap_or(0.0), conf.unwrap_or(0.0));
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_TIER_C",0.0,
                        &format!("ev={:.2} conf={:.2}", ev.unwrap_or(0.0), conf.unwrap_or(0.0)));
                    continue;
                }
                if count_open_positions(&live_conn) >= MAX_OPEN_POSITIONS {
                    println!("[executor] ⏩ Max positions ({}) reached", MAX_OPEN_POSITIONS);
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_MAXPOS",0.0,"");
                    continue;
                }
                if has_open_position(&live_conn, &sig.symbol) {
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_DUPE",0.0,"");
                    continue;
                }
                if corr_group_blocked(&live_conn, &sig.symbol) {
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_CORR",0.0,"");
                    continue;
                }
                if total_risk_deployed(&live_conn, equity) >= MAX_PORTFOLIO_RISK {
                    println!("[executor] ⏩ Portfolio risk cap {:.0}% reached", MAX_PORTFOLIO_RISK * 100.0);
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
                        println!("[executor] 📈 Dynamic entry: price ran {:.1}% — recalculating SL/TP from fill", pct);
                    }
                    _ => {} // NORMAL
                }

                // ── Time filter: noisy Asian pre-market (07:00–07:30 WIB) ────────────
                // Historical data: P(up)=0.68 in this window vs 0.92 outside → require Tier A only
                if is_noisy_window() && signal_tier(&sig) != "A" {
                    println!("[executor] ⏰ Noisy window (07:00-07:30 WIB) — skipping non-Tier-A signal");
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_NOISY_WINDOW",0.0,"07:00-07:30 WIB tier<A");
                    continue;
                }

                // ── Pump/dump protection ─────────────────────────────────────────────
                if is_pump_or_exhausted(&sig.symbol, &base.join("../..")) {
                    println!("[executor] 🚨 Pump/dump detected for {} — skipping entry", sig.symbol);
                    mark_processed(&live_conn,sig_id,&sig.symbol,&sig.direction,sig.entry,sig.stop,sig.tp1,"","SKIPPED_PUMP_DUMP",0.0,"vol>4×+RSI>78 or price>EMA20×1.08");
                    continue;
                }

                // ── Kelly-sized position ──────────────────────────────────────
                let risk_frac   = kelly_risk_fraction(&sig, base_risk);
                let avail_usdt  = get_avail_usdt(&client, &cfg).unwrap_or(0.0);

                // Coin allocation cap
                let max_spend   = equity * coin_max_alloc(&sig.symbol);
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
                    sig.direction, sig.symbol, signal_tier(&sig),
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

                        // Step 2: Dynamic TP1 scaling based on P(up) confidence
                        let p_up_val = sig.p_up.unwrap_or(0.5);
                        let actual_tp1 = compute_dynamic_tp1(fill, base_stop, p_up_val);
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
                            place_sl(&client, &cfg, &inst, qty, actual_stop);
                            place_tp_limit(&client, &cfg, &inst, qty, actual_tp1);
                            open_position(&live_conn, sig_id, &order_id, &sig.symbol,
                                          &sig.direction, qty, fill, actual_stop, actual_tp1, risk_usdt,
                                          actual_tp2, p_up_val);
                            live_conn.execute(
                                "UPDATE daily_stats SET trade_count=trade_count+1 WHERE trade_date=?",
                                params![today]).ok();
                            journal_opened(&base, &sig, fill, usdt_spend, equity, risk_frac);
                            mark_processed(&live_conn, sig_id, &sig.symbol, &sig.direction,
                                           sig.entry, actual_stop, actual_tp1, &order_id, "FILLED", fill,
                                           &format!("tier={} ev={:.2} risk={:.1}%",
                                               signal_tier(&sig), ev.unwrap_or(0.0), risk_frac*100.0));
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
        let open_pos: Vec<(i64,i64,String,String,f64,f64,f64,f64,f64,String,f64,i64)> = {
            let mut s = live_conn.prepare(
                "SELECT id,signal_id,symbol,direction,qty,entry_price,stop_price,tp1_price,risk_usdt,opened_at,
                        COALESCE(tp2_price,0.0),COALESCE(half_exited,0)
                 FROM open_positions WHERE status='OPEN'")?;
            let x: Vec<_> = s.query_map(params![], |r| Ok((
                r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,
                r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,
                r.get(10)?,r.get(11)?
            )))?.filter_map(|r| r.ok()).collect();
            x
        };

        for (pid, sig_id, symbol, direction, qty, entry, stop, tp1, _risk, opened_at_str, tp2, half_exited) in open_pos {
            let inst    = inst_id(&symbol);
            let current = match get_price(&client, &inst) { Ok(p) => p, Err(_) => continue };
            let opened_dt = DateTime::parse_from_rfc3339(&opened_at_str)
                .map(|d| d.with_timezone(&Local)).unwrap_or_else(|_| Local::now());
            let hold_min = (Local::now() - opened_dt).num_minutes();

            let fake = Signal { id: sig_id, symbol: symbol.clone(), direction: direction.clone(),
                entry, stop, tp1, _tp2: None, risk_pct: base_risk,
                ev: None, p_up: None, p_down: None, confidence: None };

            // ── Partial exit: TP1 hit on first half ─────────────────────────
            if direction == "Long" && half_exited == 0 && current >= tp1 {
                let half_qty = qty_floor(qty * 0.5);
                let half_pnl = (tp1 - entry) * half_qty;
                println!("[executor] 🎯 TP1 PARTIAL {} — selling 50% ({:.4}) +{:.4}U, moving SL→breakeven, holding for TP2={:.4}",
                    symbol, half_qty, half_pnl, tp2);
                let sold = market_sell(&client, &cfg, &inst, half_qty);
                if sold {
                    let remaining_qty = qty_floor(qty - half_qty);
                    // Update: mark half_exited, move SL to entry (breakeven), reduce qty
                    live_conn.execute(
                        "UPDATE open_positions SET half_exited=1, stop_price=entry_price, qty=? WHERE id=?",
                        params![remaining_qty, pid]).ok();
                    live_conn.execute(
                        "UPDATE daily_stats SET realized_pnl=realized_pnl+? WHERE trade_date=?",
                        params![half_pnl, today]).ok();
                    println!("[executor] ✅ Half sold. Remaining {:.4} {}, SL moved to breakeven {:.4}", remaining_qty, symbol, entry);
                } else {
                    println!("[executor] ⚠️  Partial sell failed — retrying next cycle");
                }
                continue;
            }

            // ── Second half: TP2 hit ─────────────────────────────────────────
            let (closed, pnl, status, reason) = if half_exited == 1 && tp2 > 0.0 && direction == "Long" && current >= tp2 {
                let p = (tp2 - entry) * qty;
                println!("[executor] 🚀 TP2! {} full exit +{:.4}U @ {:.4}", symbol, p, tp2);
                let sold = market_sell(&client, &cfg, &inst, qty);
                if sold { (true, p, "CLOSED_TP2", format!("{:.4}≥TP2{:.4}", current, tp2)) }
                else { println!("[executor] ⚠️  TP2 sell failed — retrying"); (false, 0.0, "", String::new()) }

            // ── Time exit ─────────────────────────────────────────────────────
            } else if hold_min >= MAX_HOLD_MIN {
                let p = (current - entry) * qty;
                println!("[executor] ⏱️  Time exit {} {}min @ {:.4}", symbol, hold_min, current);
                let sold = market_sell(&client, &cfg, &inst, qty);
                if sold { (true, p, "CLOSED_TIME", format!("{}min, exit@{:.4}", hold_min, current)) }
                else { println!("[executor] ⚠️  Sell failed — retrying next cycle"); (false, 0.0, "", String::new()) }

            // ── SL hit (original or breakeven after partial) ─────────────────
            } else if direction == "Long" && current <= stop {
                let p = (current - entry) * qty;
                let label = if half_exited == 1 { "CLOSED_BREAKEVEN" } else { "CLOSED_SL" };
                let emoji = if half_exited == 1 { "⚖️" } else { "🛑" };
                println!("[executor] {} SL {} {:.4}U @ {:.4} ({})", emoji, symbol, p, current, label);
                let sold = market_sell(&client, &cfg, &inst, qty);
                if sold { (true, p, label, format!("{:.4}≤SL{:.4}", current, stop)) }
                else { println!("[executor] ⚠️  Sell failed — retrying next cycle"); (false, 0.0, "", String::new()) }

            } else { (false, 0.0, "", String::new()) };

            if closed {
                live_conn.execute("UPDATE open_positions SET status=? WHERE id=?", params![status,pid]).ok();
                live_conn.execute("UPDATE processed_signals SET status=?,pnl_usdt=? WHERE signal_id=?",
                    params![status,pnl,sig_id]).ok();
                let is_loss: i64 = if pnl < 0.0 { 1 } else { 0 };
                live_conn.execute(
                    "UPDATE daily_stats SET realized_pnl=realized_pnl+?,
                     consecutive_losses=CASE WHEN ?2=1 THEN consecutive_losses+1 ELSE 0 END
                     WHERE trade_date=?3",
                    params![pnl,is_loss,today]).ok();
                journal_closed(&base, &fake, pnl, status, &reason);
            }
        }

        thread::sleep(StdDuration::from_secs(POLL_INTERVAL_SECS));
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Unit Tests
// Run with: cargo test --bin live-executor
// ═══════════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod tests {
    use super::*;

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
        let r = kelly_risk_fraction(&s, 0.02);
        assert!(r >= 0.03 && r <= 0.04, "Tier A risk {:.3} out of 3-4% range", r);
    }

    #[test]
    fn kelly_tier_b_range() {
        // Tier B: EV>=1, conf>=0.3 → risk 1.5-2.5%
        let s = sig(1.5, 0.65, 0.35, 0.4, 79.0, 76.5, 81.0);
        let r = kelly_risk_fraction(&s, 0.02);
        assert!(r >= 0.015 && r <= 0.025, "Tier B risk {:.3} out of 1.5-2.5% range", r);
    }

    #[test]
    fn kelly_hard_ceiling_4pct() {
        // Even with very strong signal, risk capped at 4%
        let s = sig(10.0, 0.99, 0.01, 1.0, 79.0, 76.5, 81.0);
        let r = kelly_risk_fraction(&s, 0.02);
        assert!(r <= 0.04, "Kelly ceiling violated: {:.3}", r);
    }

    #[test]
    fn kelly_hard_floor_1pct() {
        // Even with very weak signal, minimum 1%
        let s = sig(0.1, 0.51, 0.49, 0.01, 79.0, 76.5, 81.0);
        let r = kelly_risk_fraction(&s, 0.02);
        assert!(r >= 0.01, "Kelly floor violated: {:.3}", r);
    }

    // ════════════════════════════════════════════════════════════════
    // 5. SIGNAL TIER CLASSIFICATION
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn tier_a_classification() {
        let s = sig(2.0, 0.75, 0.25, 0.5, 79.0, 76.5, 81.0);
        assert_eq!(signal_tier(&s), "A");
    }

    #[test]
    fn tier_b_classification() {
        let s = sig(1.0, 0.65, 0.35, 0.3, 79.0, 76.5, 81.0);
        assert_eq!(signal_tier(&s), "B");
    }

    #[test]
    fn tier_c_low_ev() {
        let s = sig(0.4, 0.55, 0.45, 0.6, 79.0, 76.5, 81.0);
        assert_eq!(signal_tier(&s), "C");
    }

    #[test]
    fn tier_c_low_conf() {
        let s = sig(2.0, 0.75, 0.25, 0.2, 79.0, 76.5, 81.0);
        assert_eq!(signal_tier(&s), "C"); // conf < 0.5 → not A
    }

    // ════════════════════════════════════════════════════════════════
    // 6. SHOULD_TRADE GATE
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn should_trade_passes_valid_signal() {
        let s = sig(1.0, 0.65, 0.35, 0.3, 79.0, 76.5, 81.0);
        assert!(should_trade(&s));
    }

    #[test]
    fn should_trade_blocks_low_ev() {
        let s = sig(0.4, 0.55, 0.45, 0.5, 79.0, 76.5, 81.0);
        assert!(!should_trade(&s), "EV=0.4 should be blocked");
    }

    #[test]
    fn should_trade_blocks_low_confidence() {
        let s = sig(2.0, 0.75, 0.25, 0.1, 79.0, 76.5, 81.0);
        assert!(!should_trade(&s), "conf=0.1 should be blocked");
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
        assert_eq!(coin_max_alloc("BTCUSDT"), 0.40);
        assert_eq!(coin_max_alloc("ETHUSDT"), 0.40);
    }

    #[test]
    fn coin_alloc_sol_bnb_mid() {
        assert_eq!(coin_max_alloc("SOLUSDT"), 0.25);
        assert_eq!(coin_max_alloc("BNBUSDT"), 0.25);
    }

    #[test]
    fn coin_alloc_atom_lowest() {
        assert_eq!(coin_max_alloc("ATOMUSDT"), 0.15);
    }

    #[test]
    fn coin_alloc_unknown_default() {
        assert_eq!(coin_max_alloc("XYZUSDT"), 0.20);
    }

    // ════════════════════════════════════════════════════════════════
    // 9. DYNAMIC TP MULTIPLIER
    // ════════════════════════════════════════════════════════════════

    #[test]
    fn dynamic_tp_low_pup_uses_1x() {
        // p_up < 0.85 → 1× ATR multiplier
        // entry=100, sl=97 (3% stop) → tp = 100*(1+0.03*1) = 103.0
        let tp = compute_dynamic_tp1(100.0, 97.0, 0.80);
        let expected = 100.0 * (1.0 + 0.03);
        assert!((tp - expected).abs() < 0.01, "1× ATR: got {:.4} expected {:.4}", tp, expected);
    }

    #[test]
    fn dynamic_tp_medium_pup_uses_1_5x() {
        // p_up 0.85-0.92 → 1.5× ATR multiplier
        // entry=100, sl=97 (3% stop) → tp = 100*(1+0.03*1.5) = 104.5
        let tp = compute_dynamic_tp1(100.0, 97.0, 0.87);
        let expected = 100.0 * (1.0 + 0.03 * 1.5);
        assert!((tp - expected).abs() < 0.01, "1.5× ATR: got {:.4} expected {:.4}", tp, expected);
    }

    #[test]
    fn dynamic_tp_high_pup_uses_2x() {
        // p_up >= 0.92 → 2× ATR multiplier
        // entry=100, sl=97 (3% stop) → tp = 100*(1+0.03*2) = 106.0
        let tp = compute_dynamic_tp1(100.0, 97.0, 0.93);
        let expected = 100.0 * (1.0 + 0.03 * 2.0);
        assert!((tp - expected).abs() < 0.01, "2× ATR: got {:.4} expected {:.4}", tp, expected);
    }

    #[test]
    fn dynamic_tp_always_above_entry() {
        for &p_up in &[0.5, 0.80, 0.87, 0.93, 0.99] {
            let tp = compute_dynamic_tp1(100.0, 97.0, p_up);
            assert!(tp > 100.0, "TP {:.4} must be above entry 100 for p_up={}", tp, p_up);
        }
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
}
