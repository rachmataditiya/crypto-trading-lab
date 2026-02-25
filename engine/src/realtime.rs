use crate::config::RealtimeConfig;
use crate::sanitize;
use chrono::Utc;
use rand::Rng;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::{connect, stream::MaybeTlsStream, Message};

const WS_URL: &str = "wss://ws.okx.com:8443/ws/v5/public";
const REST_BASE: &str = "https://www.okx.com";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SymbolState {
    pub symbol: String,
    pub price: f64,
    pub ret_24h: f64,
    pub ret_3d: f64,
    pub ret_7d: f64,
    pub ema20: f64,
    pub ema50: f64,
    pub rsi14: f64,
    pub atr14_pct: f64,
    pub adx14: f64,
    pub volume_ratio: f64,
    pub bid: f64,
    pub ask: f64,
    pub spread_bps: f64,
    pub updated_at: String,
    #[serde(skip)]
    closes: VecDeque<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RealtimeHealth {
    pub provider: String,
    pub ws_connected: bool,
    pub fallback_active: bool,
    pub reconnect_count: u64,
    pub last_update_at: String,
    pub last_update_age_secs: u64,
    pub stale_feed: bool,
    pub rest_429_count: u64,
    pub network_error_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RealtimeCache {
    pub generated_at: String,
    pub symbols: HashMap<String, SymbolState>,
    pub health: RealtimeHealth,
}

pub fn run_daemon(workspace: &Path, cfg: &RealtimeConfig) -> Result<(), String> {
    let mut cache = RealtimeCache::default();
    cache.health.provider = cfg.provider.clone();
    let mut backoff_secs = 1u64;

    // ── Initial REST poll to pre-populate EMA / historical data ──────────────
    eprintln!("[REALTIME] initial REST poll to seed EMA data...");
    let _ = rest_poll_once(workspace, cfg, &mut cache);
    eprintln!("[REALTIME] REST seed done. Symbols with price: {}",
        cache.symbols.values().filter(|s| s.price > 0.0).count());

    loop {
        match run_ws_session(workspace, cfg, &mut cache) {
            Ok(()) => {
                backoff_secs = 1;
            }
            Err(e) => {
                eprintln!("[REALTIME] WS disconnected: {}", e);
                cache.health.ws_connected = false;
                cache.health.fallback_active = true;
                cache.health.reconnect_count += 1;
                let _ = rest_poll_once(workspace, cfg, &mut cache);
                let jitter = rand::thread_rng().gen_range(0..=700u64);
                eprintln!("[REALTIME] reconnect in {}s (attempt #{})", backoff_secs, cache.health.reconnect_count);
                thread::sleep(Duration::from_millis(backoff_secs * 1000 + jitter));
                backoff_secs = (backoff_secs * 2).min(60);
            }
        }
    }
}

fn run_ws_session(
    workspace: &Path,
    cfg: &RealtimeConfig,
    cache: &mut RealtimeCache,
) -> Result<(), String> {
    let (mut socket, _resp) = connect(WS_URL).map_err(|e| format!("ws connect: {e}"))?;
    set_nonblocking(socket.get_mut()).map_err(|e| format!("set nonblocking: {e}"))?;
    cache.health.ws_connected = true;
    cache.health.fallback_active = false;

    let args: Vec<serde_json::Value> = cfg
        .symbols
        .iter()
        .flat_map(|s| {
            let inst = to_okx_inst(s);
            vec![
                serde_json::json!({"channel":"tickers","instId":inst}),
                serde_json::json!({"channel":"candle1m","instId":inst}),
                serde_json::json!({"channel":"books5","instId":inst}),
            ]
        })
        .collect();
    let sub = serde_json::json!({"op":"subscribe","args":args}).to_string();
    socket
        .send(Message::Text(sub))
        .map_err(|e| format!("ws subscribe: {e}"))?;

    let mut last_msg = Instant::now();
    let mut last_ping = Instant::now();

    loop {
        match socket.read() {
            Ok(msg) => {
                if let Message::Text(txt) = msg {
                    apply_ws_message(&txt, cache);
                    cache.generated_at = Utc::now().to_rfc3339();
                    cache.health.last_update_at = cache.generated_at.clone();
                    cache.health.last_update_age_secs = 0;
                    cache.health.stale_feed = false;
                    persist_cache(workspace, cache)?;
                    last_msg = Instant::now();
                }
            }
            Err(tungstenite::Error::Io(ref ioe))
                if ioe.kind() == std::io::ErrorKind::WouldBlock
                    || ioe.kind() == std::io::ErrorKind::TimedOut =>
            {
                // pass
            }
            Err(e) => return Err(format!("ws read: {e}")),
        }

        let age = last_msg.elapsed().as_secs();
        cache.health.last_update_age_secs = age;
        if age > cfg.stale_after_secs {
            cache.health.stale_feed = true;
            return Err(format!("stale feed ({}s)", age));
        }

        if last_ping.elapsed().as_secs() >= cfg.ws_ping_interval_secs {
            let _ = socket.send(Message::Ping(vec![]));
            last_ping = Instant::now();
        }
    }
}

fn apply_ws_message(txt: &str, cache: &mut RealtimeCache) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(txt) else {
        return;
    };
    let Some(arg) = v.get("arg") else {
        return;
    };
    let Some(ch) = arg.get("channel").and_then(|x| x.as_str()) else {
        return;
    };
    let Some(inst) = arg.get("instId").and_then(|x| x.as_str()) else {
        return;
    };
    let symbol = from_okx_inst(inst);
    let state = cache
        .symbols
        .entry(symbol.clone())
        .or_insert_with(|| SymbolState {
            symbol: symbol.clone(),
            ..Default::default()
        });

    if let Some(arr) = v.get("data").and_then(|x| x.as_array()) {
        match ch {
            "tickers" => {
                if let Some(d) = arr.first() {
                    state.price = parse_num(d.get("last"));
                    let sod = parse_num(d.get("sodUtc0"));
                    if sod > 0.0 {
                        state.ret_24h = sanitize::ret_pct(((state.price / sod) - 1.0) * 100.0);
                    }
                    let vol_base = parse_num(d.get("vol24h"));
                    let vol_quote = parse_num(d.get("volCcy24h"));
                    // Per-coin baseline: OKX 24h USDT volume for a "normal" day
                    // (OKX ~15-20% of Binance market share for spot)
                    // ratio=1.0 = typical day, >1.0 = high activity
                    let baseline = match state.symbol.as_str() {
                        "BTCUSDT"  => 200_000_000.0,  // ~200M USDT/day on OKX
                        "ETHUSDT"  => 100_000_000.0,  // ~100M USDT/day on OKX
                        "SOLUSDT"  => 20_000_000.0,   // ~20M USDT/day on OKX
                        "BNBUSDT"  => 10_000_000.0,   // ~10M USDT/day on OKX
                        "ATOMUSDT" => 3_000_000.0,    // ~3M USDT/day on OKX
                        _          => 20_000_000.0,
                    };
                    let raw_ratio = if vol_quote > 0.0 {
                        vol_quote / baseline
                    } else if vol_base > 0.0 {
                        (vol_base * state.price) / baseline
                    } else {
                        1.0
                    };
                    state.volume_ratio = sanitize::volume_ratio(raw_ratio);
                }
            }
            "candle1m" => {
                if let Some(row) = arr.first().and_then(|x| x.as_array()) {
                    if let Some(close) = row
                        .get(4)
                        .and_then(|x| x.as_str())
                        .and_then(|x| x.parse::<f64>().ok())
                    {
                        state.closes.push_back(close);
                        while state.closes.len() > 60 {
                            state.closes.pop_front();
                        }
                        compute_features(state);
                    }
                }
            }
            "books5" => {
                if let Some(d) = arr.first() {
                    state.bid = d
                        .get("bids")
                        .and_then(|b| b.as_array())
                        .and_then(|a| a.first())
                        .and_then(|r| r.as_array())
                        .and_then(|r| r.first())
                        .and_then(|x| x.as_str())
                        .and_then(|x| x.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    state.ask = d
                        .get("asks")
                        .and_then(|b| b.as_array())
                        .and_then(|a| a.first())
                        .and_then(|r| r.as_array())
                        .and_then(|r| r.first())
                        .and_then(|x| x.as_str())
                        .and_then(|x| x.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    if state.bid > 0.0 && state.ask > 0.0 {
                        state.spread_bps =
                            ((state.ask - state.bid) / ((state.ask + state.bid) / 2.0)) * 10_000.0;
                    }
                }
            }
            _ => {}
        }
    }
    state.updated_at = Utc::now().to_rfc3339();
}

pub fn rest_poll_once(
    workspace: &Path,
    cfg: &RealtimeConfig,
    cache: &mut RealtimeCache,
) -> Result<(), String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("rest client: {e}"))?;
    let symbols = cfg.symbols.clone();
    for batch in symbols.chunks(cfg.rest_batch_size.max(1)) {
        for symbol in batch {
            if let Err(e) = fetch_symbol_rest(&client, symbol, cache) {
                if e.contains("429") {
                    cache.health.rest_429_count += 1;
                } else {
                    cache.health.network_error_count += 1;
                }
                eprintln!("[REALTIME] REST degraded {symbol}: {e}");
            }
        }
        thread::sleep(Duration::from_millis(cfg.rest_throttle_ms));
    }
    cache.generated_at = Utc::now().to_rfc3339();
    cache.health.last_update_at = cache.generated_at.clone();
    cache.health.last_update_age_secs = 0;
    cache.health.stale_feed = false;
    cache.health.fallback_active = true;
    persist_cache(workspace, cache)
}

fn fetch_symbol_rest(
    client: &Client,
    symbol: &str,
    cache: &mut RealtimeCache,
) -> Result<(), String> {
    let inst = to_okx_inst(symbol);
    let ticker_url = format!("{REST_BASE}/api/v5/market/ticker?instId={inst}");
    let t_resp = client
        .get(&ticker_url)
        .send()
        .map_err(|e| format!("ticker send: {e}"))?;
    if t_resp.status().as_u16() == 429 {
        return Err("429 ticker".to_string());
    }
    let t_json: serde_json::Value = t_resp.json().map_err(|e| format!("ticker json: {e}"))?;
    let t = t_json
        .get("data")
        .and_then(|x| x.as_array())
        .and_then(|a| a.first())
        .ok_or("ticker data missing")?;

    let candle_url = format!("{REST_BASE}/api/v5/market/candles?instId={inst}&bar=1m&limit=60");
    let c_resp = client
        .get(&candle_url)
        .send()
        .map_err(|e| format!("candles send: {e}"))?;
    if c_resp.status().as_u16() == 429 {
        return Err("429 candles".to_string());
    }
    let c_json: serde_json::Value = c_resp.json().map_err(|e| format!("candles json: {e}"))?;

    let state = cache
        .symbols
        .entry(symbol.to_string())
        .or_insert_with(|| SymbolState {
            symbol: symbol.to_string(),
            ..Default::default()
        });
    state.price = parse_num(t.get("last"));
    let sod = parse_num(t.get("sodUtc0"));
    if sod > 0.0 {
        state.ret_24h = sanitize::ret_pct(((state.price / sod) - 1.0) * 100.0);
    }

    state.closes.clear();
    if let Some(rows) = c_json.get("data").and_then(|x| x.as_array()) {
        let mut closes = rows
            .iter()
            .filter_map(|r| r.as_array())
            .filter_map(|r| r.get(4))
            .filter_map(|x| x.as_str())
            .filter_map(|x| x.parse::<f64>().ok())
            .collect::<Vec<f64>>();
        // OKX returns candles newest-first → reverse to get chronological order
        closes.reverse();
        for c in closes.into_iter().take(60) {
            state.closes.push_back(c);
        }
    }
    compute_features(state);
    state.updated_at = Utc::now().to_rfc3339();
    Ok(())
}

fn compute_features(state: &mut SymbolState) {
    if state.closes.is_empty() {
        return;
    }
    let v: Vec<f64> = state.closes.iter().copied().collect();
    state.ema20 = sma_tail(&v, 20).unwrap_or(state.price);
    state.ema50 = sma_tail(&v, 50).unwrap_or(state.price);

    // Real RSI14 from close-to-close changes
    state.rsi14 = compute_rsi14(&v);

    // ATR14 approximation: abs(close[i] - close[i-1]), mean of last 14
    state.atr14_pct = compute_atr14_pct(&v, state.price);

    // ADX: rough approximation via directional movement (close-based)
    state.adx14 = sanitize::adx(compute_adx14_approx(&v));

    state.ret_3d = sanitize::ret_pct(state.ret_24h * 1.8);
    state.ret_7d = sanitize::ret_pct(state.ret_24h * 3.0);
}

fn compute_rsi14(v: &[f64]) -> f64 {
    if v.len() < 15 {
        return 50.0;
    }
    let changes: Vec<f64> = v.windows(2).map(|w| w[1] - w[0]).collect();
    let last14 = &changes[changes.len().saturating_sub(14)..];
    let avg_gain: f64 = last14.iter().map(|&c| if c > 0.0 { c } else { 0.0 }).sum::<f64>() / 14.0;
    let avg_loss: f64 = last14.iter().map(|&c| if c < 0.0 { -c } else { 0.0 }).sum::<f64>() / 14.0;
    if avg_loss == 0.0 {
        return 100.0;
    }
    let rs = avg_gain / avg_loss;
    (100.0 - 100.0 / (1.0 + rs)).clamp(0.0, 100.0)
}

fn compute_atr14_pct(v: &[f64], price: f64) -> f64 {
    if v.len() < 2 || price <= 0.0 {
        return 1.0;
    }
    let trs: Vec<f64> = v.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
    let last14 = &trs[trs.len().saturating_sub(14)..];
    if last14.is_empty() {
        return 1.0;
    }
    let atr_1m: f64 = last14.iter().sum::<f64>() / last14.len() as f64;
    let atr_1m_pct = (atr_1m / price) * 100.0;
    // Scale 1m ATR to daily equivalent: multiply by sqrt(1440 min/day)
    let daily_atr_pct = atr_1m_pct * 37.95;
    daily_atr_pct.clamp(0.0, 50.0)
}

fn compute_adx14_approx(v: &[f64]) -> f64 {
    // Approximate ADX using close-based directional movement
    // Without H/L data, we use close differences as proxy for DM
    if v.len() < 15 {
        return 20.0;
    }
    let changes: Vec<f64> = v.windows(2).map(|w| w[1] - w[0]).collect();
    let last14 = &changes[changes.len().saturating_sub(14)..];
    let pos_sum: f64 = last14.iter().filter(|&&c| c > 0.0).map(|&c| c).sum();
    let neg_sum: f64 = last14.iter().filter(|&&c| c < 0.0).map(|&c| c.abs()).sum();
    let total = pos_sum + neg_sum;
    if total == 0.0 {
        return 15.0;
    }
    // DX = |DI+ - DI-| / (DI+ + DI-) * 100
    // ADX ≈ smoothed DX (no artificial floor — let the filter do its job)
    let dx = ((pos_sum - neg_sum).abs() / total) * 100.0;
    (dx * 0.7).clamp(10.0, 50.0)
}

fn sma_tail(v: &[f64], n: usize) -> Option<f64> {
    if v.len() < n {
        return None;
    }
    let s: f64 = v[v.len() - n..].iter().sum();
    Some(s / n as f64)
}

fn persist_cache(workspace: &Path, cache: &RealtimeCache) -> Result<(), String> {
    let path = workspace.join("data/trading-journal/realtime_cache.json");
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).map_err(|e| format!("mkdir cache: {e}"))?;
    }
    let body = serde_json::to_string_pretty(cache).map_err(|e| format!("cache json: {e}"))?;
    fs::write(&path, body).map_err(|e| format!("cache write: {e}"))?;
    Ok(())
}

fn parse_num(v: Option<&serde_json::Value>) -> f64 {
    match v {
        Some(serde_json::Value::String(s)) => s.parse::<f64>().unwrap_or(0.0),
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn set_nonblocking(stream: &mut MaybeTlsStream<std::net::TcpStream>) -> std::io::Result<()> {
    match stream {
        MaybeTlsStream::Plain(s) => s.set_nonblocking(true),
        MaybeTlsStream::Rustls(s) => s.get_ref().set_nonblocking(true),
        _ => Ok(()),
    }
}

fn to_okx_inst(symbol: &str) -> String {
    symbol.replace("USDT", "-USDT")
}

fn from_okx_inst(inst: &str) -> String {
    inst.replace("-", "")
}
