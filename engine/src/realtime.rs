use crate::config::RealtimeConfig;
use crate::hmtf;
use crate::sanitize;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chrono::Utc;
use hmac::{Hmac, Mac};
use rand::Rng;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::{connect, stream::MaybeTlsStream, Message};

// Ticker/orderbook: public WS (no auth needed)
const WS_TICKER_URL: &str = "wss://ws.okx.com:8443/ws/v5/public";
// Candle data: business WS (authenticated for better rate limits)
const WS_CANDLE_URL: &str = "wss://ws.okx.com:8443/ws/v5/business";
const REST_BASE: &str = "https://www.okx.com";

// Binance combined stream endpoint (no auth needed)
const BINANCE_REST_BASE: &str = "https://api.binance.com";

// Deque capacity per timeframe
const MAX_1M: usize = 120; // 2h of 1m closes
const MAX_1H: usize = 120; // 5d of H1 closes
const MAX_4H: usize = 120; // 20d of H4 closes
const MAX_8H: usize = 60;  // ~15d of H6 closes (OKX has no 8H; using 6H)
const MAX_12H: usize = 60; // ~30d of H12 closes
const MAX_1D: usize = 100; // 100d of D1 closes

/// All 19 symbols tracked by the engine.
const BINANCE_SYMBOLS: &[&str] = &[
    "BTCUSDT", "ETHUSDT", "SOLUSDT", "BNBUSDT", "XRPUSDT",
    "ADAUSDT", "DOTUSDT", "LINKUSDT", "LTCUSDT", "DOGEUSDT",
    "AVAXUSDT", "MATICUSDT", "ATOMUSDT", "UNIUSDT", "FILUSDT",
    "NEARUSDT", "APTUSDT", "ARBUSDT", "OPUSDT",
];

/// Binance kline timeframes we subscribe to.
const BINANCE_TIMEFRAMES: &[&str] = &["1h", "4h", "6h", "12h"];

// ── Credentials ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct OkxConfig {
    api_key:    String,
    secret_key: String,
    passphrase: String,
}

fn load_creds(workspace: &Path) -> Result<OkxConfig, String> {
    let path = workspace.join("data/trading-journal/okx_config.json");
    let raw  = fs::read_to_string(&path)
        .map_err(|e| format!("read okx_config: {e}"))?;
    serde_json::from_str(&raw)
        .map_err(|e| format!("parse okx_config: {e}"))
}

fn ws_login_msg(creds: &OkxConfig) -> String {
    let ts  = format!("{}", Utc::now().timestamp());
    let msg = format!("{}GET/users/self/verify", ts);
    let mut mac = Hmac::<Sha256>::new_from_slice(creds.secret_key.as_bytes())
        .expect("hmac key");
    mac.update(msg.as_bytes());
    let sig = B64.encode(mac.finalize().into_bytes());
    serde_json::json!({
        "op": "login",
        "args": [{
            "apiKey":     creds.api_key,
            "passphrase": creds.passphrase,
            "timestamp":  ts,
            "sign":       sig
        }]
    })
    .to_string()
}

// ── State ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SymbolState {
    pub symbol:      String,
    pub price:       f64,
    pub ret_24h:     f64,
    pub ret_3d:      f64,
    pub ret_7d:      f64,
    /// Intra-candle return since last confirmed 1H close.
    /// Positive = price rising above last 1H close; Negative = fast drop (crash signal).
    /// Used by live-executor fast-crash guard: block entry if ret_1h < -3.0%.
    #[serde(default)]
    pub ret_1h:      f64,
    pub ema20:       f64,  // 4H-based macro EMA20 (updates every 4H candle)
    pub ema50:       f64,  // 4H-based macro EMA50
    /// 1H-based EMA20 — updates every 1H candle. Used by Tier R/V/F for faster macro filter.
    #[serde(default)]
    pub ema20_1h:    f64,
    /// 1H-based EMA50 — updates every 1H candle.
    #[serde(default)]
    pub ema50_1h:    f64,
    pub rsi14:       f64,
    pub atr14_pct:   f64,
    pub adx14:       f64,
    pub volume_ratio: f64,
    pub bid:         f64,
    pub ask:         f64,
    pub spread_bps:  f64,
    pub updated_at:  String,
    /// Indicates which timeframe is driving the indicators
    pub indicator_tf: String,

    // ── HMTF fields (serialised to cache) ──
    #[serde(default)]
    pub hmtf_entry_score: f64,
    #[serde(default)]
    pub hmtf_alignment: u8,
    #[serde(default = "default_hmtf_tier")]
    pub hmtf_tier: char,
    #[serde(default)]
    pub hmtf_gate: f64,

    // ── AMS fields (serialised to cache) ──
    #[serde(default)]
    pub ams_regime_rho: f64,
    #[serde(default = "default_ams_regime_label")]
    pub ams_regime_label: String,
    #[serde(default)]
    pub ams_divergence_d: f64,
    #[serde(default)]
    pub ams_e_trend: f64,
    #[serde(default)]
    pub ams_e_reversion: f64,
    #[serde(default)]
    pub ams_kelly_multiplier: f64,

    // ── close-price history (not serialised, rebuilt from WS / REST seed) ──
    #[serde(skip)]
    closes:     VecDeque<f64>, // 1m confirmed closes
    #[serde(skip)]
    closes_1h:  VecDeque<f64>, // H1 confirmed closes
    #[serde(skip)]
    closes_4h:  VecDeque<f64>, // H4 confirmed closes
    #[serde(skip)]
    closes_8h:  VecDeque<f64>, // H6 confirmed closes (OKX has no 8H, using 6H)
    #[serde(skip)]
    closes_12h: VecDeque<f64>, // H12 confirmed closes
    #[serde(skip)]
    closes_1d:  VecDeque<f64>, // D1 confirmed closes
}

fn default_hmtf_tier() -> char {
    'C'
}

fn default_ams_regime_label() -> String {
    "UNKNOWN".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RealtimeHealth {
    pub provider:            String,
    pub ws_connected:        bool,
    pub fallback_active:     bool,
    pub reconnect_count:     u64,
    pub last_update_at:      String,
    pub last_update_age_secs: u64,
    pub stale_feed:          bool,
    pub rest_429_count:      u64,
    pub network_error_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RealtimeCache {
    pub generated_at: String,
    pub symbols:      HashMap<String, SymbolState>,
    pub health:       RealtimeHealth,
}

// ── Entry point ─────────────────────────────────────────────────────────────

pub fn run_daemon(workspace: &Path, cfg: &RealtimeConfig) -> Result<(), String> {
    let creds = load_creds(workspace)?;
    eprintln!("[REALTIME] OKX credentials loaded (key: {}...)", &creds.api_key[..8]);

    // ── Spawn Binance WS session in a separate thread ────────────────────
    let binance_workspace = workspace.to_path_buf();
    thread::spawn(move || {
        eprintln!("[BINANCE] starting Binance WS thread...");
        let mut backoff = 1u64;
        loop {
            match run_binance_ws_session(&binance_workspace) {
                Ok(()) => backoff = 1,
                Err(e) => {
                    eprintln!("[BINANCE] session error: {e}");
                    let jitter = rand::thread_rng().gen_range(0..=700u64);
                    eprintln!("[BINANCE] reconnect in {backoff}s...");
                    thread::sleep(Duration::from_millis(backoff * 1000 + jitter));
                    backoff = (backoff * 2).min(60);
                }
            }
        }
    });

    let mut cache = RealtimeCache::default();
    cache.health.provider = cfg.provider.clone();
    let mut backoff_secs = 1u64;

    // Seed historical candle data via REST before WS connects
    eprintln!("[REALTIME] seeding H1/H4/H8/H12/D1 candle history via REST...");
    let _ = rest_poll_once(workspace, cfg, &mut cache);
    eprintln!(
        "[REALTIME] REST seed done — symbols with price: {}",
        cache.symbols.values().filter(|s| s.price > 0.0).count()
    );

    loop {
        match run_ws_dual_session(workspace, cfg, &mut cache, &creds) {
            Ok(()) => {
                backoff_secs = 1;
            }
            Err(e) => {
                eprintln!("[REALTIME] WS disconnected: {}", e);
                cache.health.ws_connected = false;
                cache.health.fallback_active = true;
                cache.health.reconnect_count += 1;
                // Fallback to REST while waiting to reconnect
                let _ = rest_poll_once(workspace, cfg, &mut cache);
                let jitter = rand::thread_rng().gen_range(0..=700u64);
                eprintln!(
                    "[REALTIME] reconnect in {}s (attempt #{})",
                    backoff_secs, cache.health.reconnect_count
                );
                thread::sleep(Duration::from_millis(backoff_secs * 1000 + jitter));
                backoff_secs = (backoff_secs * 2).min(60);
            }
        }
    }
}

// ── Binance WebSocket session ────────────────────────────────────────────────

fn run_binance_ws_session(workspace: &Path) -> Result<(), String> {
    let mut cache = RealtimeCache::default();
    cache.health.provider = "binance".to_string();

    // ── Seed historical klines via REST ─────────────────────────────────
    eprintln!("[BINANCE] seeding candle history via REST...");
    let client = Client::builder()
        .timeout(Duration::from_secs(12))
        .user_agent("binance-rs-client/1.0")
        .build()
        .map_err(|e| format!("binance rest client: {e}"))?;

    for &sym in BINANCE_SYMBOLS {
        for &(tf, deque_max) in &[("1h", MAX_1H), ("4h", MAX_4H), ("6h", MAX_8H), ("12h", MAX_12H)] {
            if let Err(e) = fetch_binance_klines(&client, sym, tf, 120, deque_max, &mut cache) {
                eprintln!("[BINANCE] REST seed {sym}/{tf}: {e}");
            }
            thread::sleep(Duration::from_millis(100)); // rate-limit courtesy
        }
        // Recompute indicators after seeding all timeframes for this symbol
        if let Some(state) = cache.symbols.get_mut(sym) {
            recompute_indicators(state);
            state.updated_at = Utc::now().to_rfc3339();
        }
    }
    eprintln!(
        "[BINANCE] REST seed done — symbols with data: {}",
        cache.symbols.values().filter(|s| s.closes_1h.len() >= 15).count()
    );
    persist_binance_cache(workspace, &cache)?;

    // ── Build combined stream URL ──────────────────────────────────────
    let streams: Vec<String> = BINANCE_SYMBOLS
        .iter()
        .flat_map(|s| {
            let lower = s.to_lowercase();
            BINANCE_TIMEFRAMES.iter().map(move |tf| format!("{lower}@kline_{tf}"))
        })
        .collect();
    let url = format!("wss://stream.binance.com/stream?streams={}", streams.join("/"));

    eprintln!("[BINANCE] connecting WS ({} streams)...", streams.len());
    let (mut sock, _) = connect(&url).map_err(|e| format!("binance ws connect: {e}"))?;
    set_nonblocking(sock.get_mut()).map_err(|e| format!("binance nonblocking: {e}"))?;

    cache.health.ws_connected = true;
    cache.health.fallback_active = false;

    let mut last_msg     = Instant::now();
    let mut last_persist = Instant::now();

    loop {
        let mut changed = false;

        match sock.read() {
            Ok(Message::Text(txt)) => {
                if apply_binance_kline(&txt, &mut cache) {
                    changed = true;
                }
                last_msg = Instant::now();
            }
            Ok(Message::Ping(d)) => {
                let _ = sock.send(Message::Pong(d));
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("binance ws read: {e}")),
        }

        // Persist at most once per second
        if changed && last_persist.elapsed() >= Duration::from_secs(1) {
            let now = Utc::now().to_rfc3339();
            cache.generated_at = now.clone();
            cache.health.last_update_at = now;
            cache.health.last_update_age_secs = 0;
            cache.health.stale_feed = false;
            persist_binance_cache(workspace, &cache)?;
            last_persist = Instant::now();
        }

        // Stale feed detection (90s for Binance — klines come less frequently than tickers)
        let age = last_msg.elapsed().as_secs();
        cache.health.last_update_age_secs = age;
        if age > 90 {
            cache.health.stale_feed = true;
            return Err(format!("binance stale feed ({age}s)"));
        }

        if !changed {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Parse a Binance combined-stream kline event and update cache.
/// Returns true if a confirmed candle was processed.
fn apply_binance_kline(txt: &str, cache: &mut RealtimeCache) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(txt) else { return false };
    let data = match v.get("data") {
        Some(d) => d,
        None => return false,
    };
    let k = match data.get("k") {
        Some(k) => k,
        None => return false,
    };

    let symbol = match k.get("s").and_then(|x| x.as_str()) {
        Some(s) => s.to_string(), // Already uppercase from Binance
        None => return false,
    };
    let interval = match k.get("i").and_then(|x| x.as_str()) {
        Some(i) => i,
        None => return false,
    };
    let is_closed = k.get("x").and_then(|x| x.as_bool()).unwrap_or(false);

    let close: f64 = parse_binance_num(k.get("c"));
    if close <= 0.0 { return false; }

    let state = cache.symbols.entry(symbol.clone()).or_insert_with(|| SymbolState {
        symbol: symbol.clone(),
        ..Default::default()
    });

    // Always update latest price
    state.price = close;

    if !is_closed { return false; }

    match interval {
        "1h" => push_close(&mut state.closes_1h, close, MAX_1H),
        "4h" => push_close(&mut state.closes_4h, close, MAX_4H),
        "6h" => push_close(&mut state.closes_8h, close, MAX_8H),
        "12h" => push_close(&mut state.closes_12h, close, MAX_12H),
        _ => return false,
    }

    // Volume ratio from kline volume
    let vol_quote: f64 = parse_binance_num(k.get("q")); // quote asset volume
    let baseline = volume_baseline(&symbol);
    if vol_quote > 0.0 {
        state.volume_ratio = sanitize::volume_ratio(vol_quote / baseline);
    }

    recompute_indicators(state);
    state.updated_at = Utc::now().to_rfc3339();
    true
}

/// Parse a number from a Binance JSON field (always string in kline data).
fn parse_binance_num(v: Option<&serde_json::Value>) -> f64 {
    match v {
        Some(serde_json::Value::String(s)) => s.parse::<f64>().unwrap_or(0.0),
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Fetch historical klines from Binance REST and populate the correct deque.
fn fetch_binance_klines(
    client: &Client,
    symbol: &str,
    interval: &str,
    limit: usize,
    max_deque: usize,
    cache: &mut RealtimeCache,
) -> Result<(), String> {
    let url = format!(
        "{BINANCE_REST_BASE}/api/v3/klines?symbol={symbol}&interval={interval}&limit={limit}"
    );
    let resp = client.get(&url).send()
        .map_err(|e| format!("binance klines({interval}) send: {e}"))?;
    if resp.status().as_u16() == 429 {
        return Err(format!("429 binance klines {interval}"));
    }
    let rows: Vec<serde_json::Value> = resp.json()
        .map_err(|e| format!("binance klines({interval}) json: {e}"))?;

    let state = cache.symbols.entry(symbol.to_string()).or_insert_with(|| SymbolState {
        symbol: symbol.to_string(),
        ..Default::default()
    });

    // Binance REST klines: [[open_time, open, high, low, close, volume, close_time, ...], ...]
    // close = index [4], already in chronological order (oldest first)
    let deque = match interval {
        "1h"  => &mut state.closes_1h,
        "4h"  => &mut state.closes_4h,
        "6h"  => &mut state.closes_8h,
        "12h" => &mut state.closes_12h,
        _ => return Ok(()),
    };

    deque.clear();
    for row in &rows {
        if let Some(arr) = row.as_array() {
            if let Some(close) = arr.get(4).and_then(|x| x.as_str()).and_then(|s| s.parse::<f64>().ok()) {
                push_close(deque, close, max_deque);
            }
        }
    }

    // Update price from the last kline close
    if let Some(&last) = deque.back() {
        if state.price <= 0.0 {
            state.price = last;
        }
    }

    Ok(())
}

fn persist_binance_cache(workspace: &Path, cache: &RealtimeCache) -> Result<(), String> {
    let path = workspace.join("data/trading-journal/binance_cache.json");
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).map_err(|e| format!("mkdir binance cache: {e}"))?;
    }
    let body = serde_json::to_string_pretty(cache).map_err(|e| format!("binance cache json: {e}"))?;
    fs::write(&path, body).map_err(|e| format!("binance cache write: {e}"))?;
    Ok(())
}

// ── Dual WebSocket session ──────────────────────────────────────────────────

fn run_ws_dual_session(
    workspace: &Path,
    cfg:       &RealtimeConfig,
    cache:     &mut RealtimeCache,
    creds:     &OkxConfig,
) -> Result<(), String> {
    // ── Connection 1: public — tickers + books5 ───────────────────────────
    eprintln!("[REALTIME] connecting ticker WS (public)...");
    let (mut ticker_sock, _) =
        connect(WS_TICKER_URL).map_err(|e| format!("ticker ws connect: {e}"))?;
    set_nonblocking(ticker_sock.get_mut()).map_err(|e| format!("ticker nonblocking: {e}"))?;

    let ticker_args: Vec<serde_json::Value> = cfg
        .symbols
        .iter()
        .flat_map(|s| {
            let inst = to_okx_inst(s);
            vec![
                serde_json::json!({"channel":"tickers","instId":inst}),
                serde_json::json!({"channel":"books5","instId":inst}),
            ]
        })
        .collect();
    let sub = serde_json::json!({"op":"subscribe","args":ticker_args}).to_string();
    ticker_sock
        .send(Message::Text(sub))
        .map_err(|e| format!("ticker subscribe: {e}"))?;
    eprintln!(
        "[REALTIME] ticker WS subscribed ({} channels)",
        cfg.symbols.len() * 2
    );

    // ── Connection 2: business — candles 1m/1H/4H/1D (authenticated) ─────
    eprintln!("[REALTIME] connecting candle WS (business, authenticated)...");
    let (mut candle_sock, _) =
        connect(WS_CANDLE_URL).map_err(|e| format!("candle ws connect: {e}"))?;
    set_nonblocking(candle_sock.get_mut()).map_err(|e| format!("candle nonblocking: {e}"))?;

    // Send login — temporarily block to get response (max 2s)
    let login = ws_login_msg(creds);
    candle_sock
        .send(Message::Text(login))
        .map_err(|e| format!("candle login send: {e}"))?;

    // Wait for login ack (briefly block; it's early startup so blocking is OK)
    let login_deadline = Instant::now() + Duration::from_secs(3);
    let mut logged_in = false;
    while Instant::now() < login_deadline {
        match candle_sock.read() {
            Ok(Message::Text(txt)) => {
                if txt.contains("\"login\"") {
                    logged_in = true;
                    eprintln!("[REALTIME] candle WS login OK");
                    break;
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("[REALTIME] candle login wait error: {e} (continuing without auth)");
                break;
            }
        }
    }
    if !logged_in {
        eprintln!("[REALTIME] candle WS login timeout — proceeding (public channels still work)");
    }

    // Subscribe all candle channels in one message (rate: 3 msgs/sec, but 1 msg = fine)
    let candle_args: Vec<serde_json::Value> = cfg
        .symbols
        .iter()
        .flat_map(|s| {
            let inst = to_okx_inst(s);
            vec![
                serde_json::json!({"channel":"candle1m","instId":inst}),
                serde_json::json!({"channel":"candle1H","instId":inst}),
                serde_json::json!({"channel":"candle4H","instId":inst}),
                serde_json::json!({"channel":"candle6H","instId":inst}),
                serde_json::json!({"channel":"candle12H","instId":inst}),
                serde_json::json!({"channel":"candle1D","instId":inst}),
            ]
        })
        .collect();

    // OKX rate limit: 3 subscribe *messages* per second.
    // We batch into groups of 30 args per message (arbitrary grouping), one message per 400ms.
    for chunk in candle_args.chunks(30) {
        let msg = serde_json::json!({"op":"subscribe","args":chunk}).to_string();
        candle_sock
            .send(Message::Text(msg))
            .map_err(|e| format!("candle subscribe: {e}"))?;
        thread::sleep(Duration::from_millis(400));
    }
    eprintln!(
        "[REALTIME] candle WS subscribed ({} channels: 1m+1H+4H+6H+12H+1D × {} symbols)",
        cfg.symbols.len() * 6,
        cfg.symbols.len()
    );

    cache.health.ws_connected = true;
    cache.health.fallback_active = false;

    // ── Dual-socket poll loop ─────────────────────────────────────────────
    let mut last_msg     = Instant::now();
    let mut last_ping    = Instant::now();
    let mut last_persist = Instant::now();

    loop {
        let mut changed = false;

        // Poll ticker socket
        match ticker_sock.read() {
            Ok(Message::Text(txt)) => {
                apply_ws_message(&txt, cache);
                changed = true;
                last_msg = Instant::now();
            }
            Ok(Message::Ping(d)) => {
                let _ = ticker_sock.send(Message::Pong(d));
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("ticker ws read: {e}")),
        }

        // Poll candle socket
        match candle_sock.read() {
            Ok(Message::Text(txt)) => {
                apply_ws_message(&txt, cache);
                changed = true;
                last_msg = Instant::now();
            }
            Ok(Message::Ping(d)) => {
                let _ = candle_sock.send(Message::Pong(d));
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(format!("candle ws read: {e}")),
        }

        // Persist at most once per second (avoid hammering disk)
        if changed && last_persist.elapsed() >= Duration::from_secs(1) {
            let now = Utc::now().to_rfc3339();
            cache.generated_at = now.clone();
            cache.health.last_update_at = now;
            cache.health.last_update_age_secs = 0;
            cache.health.stale_feed = false;
            persist_cache(workspace, cache)?;
            last_persist = Instant::now();
        }

        // Stale feed detection
        let age = last_msg.elapsed().as_secs();
        cache.health.last_update_age_secs = age;
        if age > cfg.stale_after_secs {
            cache.health.stale_feed = true;
            return Err(format!("stale feed ({}s)", age));
        }

        // Periodic ping to keep both connections alive
        if last_ping.elapsed().as_secs() >= cfg.ws_ping_interval_secs {
            let _ = ticker_sock.send(Message::Ping(vec![]));
            let _ = candle_sock.send(Message::Ping(vec![]));
            last_ping = Instant::now();
        }

        if !changed {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

// ── Message handler ─────────────────────────────────────────────────────────

fn apply_ws_message(txt: &str, cache: &mut RealtimeCache) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(txt) else {
        return;
    };
    let Some(arg) = v.get("arg") else { return };
    let Some(ch)  = arg.get("channel").and_then(|x| x.as_str()) else { return };
    let Some(inst) = arg.get("instId").and_then(|x| x.as_str()) else { return };
    let symbol = from_okx_inst(inst);

    let state = cache.symbols.entry(symbol.clone()).or_insert_with(|| SymbolState {
        symbol: symbol.clone(),
        ..Default::default()
    });

    let Some(arr) = v.get("data").and_then(|x| x.as_array()) else { return };

    match ch {
        // ── Ticker: price + volume ──────────────────────────────────────
        "tickers" => {
            if let Some(d) = arr.first() {
                state.price = parse_num(d.get("last"));
                let sod = parse_num(d.get("sodUtc0"));
                if sod > 0.0 {
                    state.ret_24h =
                        sanitize::ret_pct(((state.price / sod) - 1.0) * 100.0);
                }
                let vol_quote = parse_num(d.get("volCcy24h"));
                let vol_base  = parse_num(d.get("vol24h"));
                let baseline  = volume_baseline(&state.symbol);
                let raw_ratio = if vol_quote > 0.0 {
                    vol_quote / baseline
                } else if vol_base > 0.0 && state.price > 0.0 {
                    (vol_base * state.price) / baseline
                } else {
                    1.0
                };
                state.volume_ratio = sanitize::volume_ratio(raw_ratio);
            }
        }

        // ── Orderbook: bid/ask/spread ────────────────────────────────────
        "books5" => {
            if let Some(d) = arr.first() {
                state.bid = book_top(d.get("bids"));
                state.ask = book_top(d.get("asks"));
                if state.bid > 0.0 && state.ask > 0.0 {
                    let mid = (state.ask + state.bid) / 2.0;
                    state.spread_bps = ((state.ask - state.bid) / mid) * 10_000.0;
                }
            }
        }

        // ── Candles: only accepted when confirmed (confirm field == "1") ─
        // OKX candle format: [ts, open, high, low, close, vol, volCcy, volCcyQuote, confirm]
        //                      0    1     2    3    4     5    6       7            8
        "candle1m" => {
            if let Some(close) = confirmed_close(arr) {
                push_close(&mut state.closes, close, MAX_1M);
                recompute_indicators(state);
            }
        }
        "candle1H" => {
            if let Some(close) = confirmed_close(arr) {
                push_close(&mut state.closes_1h, close, MAX_1H);
                recompute_indicators(state);
            }
        }
        "candle4H" => {
            if let Some(close) = confirmed_close(arr) {
                push_close(&mut state.closes_4h, close, MAX_4H);
                recompute_indicators(state);
            }
        }
        "candle6H" => {
            if let Some(close) = confirmed_close(arr) {
                push_close(&mut state.closes_8h, close, MAX_8H);
                recompute_indicators(state);
            }
        }
        "candle12H" => {
            if let Some(close) = confirmed_close(arr) {
                push_close(&mut state.closes_12h, close, MAX_12H);
                recompute_indicators(state);
            }
        }
        "candle1D" => {
            if let Some(close) = confirmed_close(arr) {
                push_close(&mut state.closes_1d, close, MAX_1D);
                recompute_indicators(state);
            }
        }
        _ => {}
    }

    state.updated_at = Utc::now().to_rfc3339();
}

/// Extract close from a candle push, only when confirm=="1" (candle closed).
fn confirmed_close(arr: &[serde_json::Value]) -> Option<f64> {
    let row = arr.first()?.as_array()?;
    // confirm field is at index 8; "1" = confirmed close
    let confirmed = row
        .get(8)
        .and_then(|x| x.as_str())
        .map(|s| s == "1")
        .unwrap_or(false);
    if !confirmed {
        return None;
    }
    row.get(4)
        .and_then(|x| x.as_str())
        .and_then(|s| s.parse::<f64>().ok())
}

fn push_close(deque: &mut VecDeque<f64>, close: f64, max: usize) {
    deque.push_back(close);
    while deque.len() > max {
        deque.pop_front();
    }
}

// ── Indicator computation ────────────────────────────────────────────────────

/// Recompute all indicators from best available timeframe.
/// Priority: H4 (aligns with crypto-scan) → H1 → 1m fallback
fn recompute_indicators(state: &mut SymbolState) {
    let (closes, tf, atr_scale) = if state.closes_4h.len() >= 20 {
        (
            state.closes_4h.iter().copied().collect::<Vec<_>>(),
            "4H",
            6.0_f64.sqrt(), // ATR4H → daily: sqrt(24/4) = sqrt(6) ≈ 2.45
        )
    } else if state.closes_1h.len() >= 15 {
        (
            state.closes_1h.iter().copied().collect::<Vec<_>>(),
            "1H",
            24.0_f64.sqrt(), // ATR1H → daily: sqrt(24) ≈ 4.90
        )
    } else if state.closes.len() >= 15 {
        (
            state.closes.iter().copied().collect::<Vec<_>>(),
            "1m",
            1440.0_f64.sqrt(), // ATR1m → daily: sqrt(1440) ≈ 37.95
        )
    } else {
        return; // Not enough data yet
    };

    state.indicator_tf = tf.to_string();

    // EMA20 and EMA50 (4H-based macro trend — primary for Tier A/B)
    state.ema20 = ema_tail(&closes, 20).unwrap_or(state.price);
    state.ema50 = ema_tail(&closes, 50).unwrap_or(state.price);

    // 1H-based EMA (faster macro filter for Tier R/V/F — updates every 1H candle)
    if state.closes_1h.len() >= 20 {
        let c1h: Vec<f64> = state.closes_1h.iter().copied().collect();
        state.ema20_1h = ema_tail(&c1h, 20).unwrap_or(state.price);
        state.ema50_1h = ema_tail(&c1h, 50).unwrap_or(state.price);
    }

    // RSI14
    state.rsi14 = compute_rsi14(&closes);

    // ATR14 scaled to daily equivalent
    state.atr14_pct = compute_atr14_pct(&closes, state.price, atr_scale);

    // ADX14
    state.adx14 = sanitize::adx(compute_adx14_approx(&closes));

    // Multi-day returns (rough approximation from 24h ret)
    state.ret_3d = sanitize::ret_pct(state.ret_24h * 1.8);
    state.ret_7d = sanitize::ret_pct(state.ret_24h * 3.0);

    // Intra-1H return: how much price moved since the last confirmed 1H candle closed.
    // Newest close is at the BACK of the deque (push_back order).
    // A strongly negative value (< -3%) means a fast intra-candle crash — used by
    // live-executor to block entries even when 4H EMA alignment still looks bullish.
    state.ret_1h = if let Some(&prev_1h_close) = state.closes_1h.back() {
        if prev_1h_close > 0.0 {
            sanitize::ret_pct((state.price - prev_1h_close) / prev_1h_close * 100.0)
        } else {
            0.0
        }
    } else {
        0.0 // no 1H data yet — fail-open (allow)
    };

    // ── HMTF computation ──────────────────────────────────────────────────
    if state.closes_1h.len() >= 15
        && state.closes_4h.len() >= 15
        && state.closes_8h.len() >= 15
        && state.closes_12h.len() >= 15
    {
        let input = hmtf::HmtfInput {
            closes_1h:  state.closes_1h.iter().copied().collect(),
            closes_4h:  state.closes_4h.iter().copied().collect(),
            closes_6h:  state.closes_8h.iter().copied().collect(),
            closes_12h: state.closes_12h.iter().copied().collect(),
            volume_ratio: state.volume_ratio,
            price: state.price,
            atr_pct: state.atr14_pct,
        };
        let sig = hmtf::compute_hmtf(&input);
        state.hmtf_entry_score = sig.entry_score;
        state.hmtf_alignment   = sig.alignment;
        state.hmtf_tier        = sig.tier;
        state.hmtf_gate        = sig.gate_product;
        // AMS fields
        state.ams_regime_rho       = sig.regime_rho;
        state.ams_regime_label     = sig.regime_label.clone();
        state.ams_divergence_d     = sig.divergence_d;
        state.ams_e_trend          = sig.e_trend;
        state.ams_e_reversion      = sig.e_reversion;
        state.ams_kelly_multiplier = sig.kelly_multiplier;
    }
}

fn compute_rsi14(v: &[f64]) -> f64 {
    if v.len() < 15 {
        return 50.0;
    }
    let changes: Vec<f64> = v.windows(2).map(|w| w[1] - w[0]).collect();
    let last14 = &changes[changes.len().saturating_sub(14)..];
    let avg_gain = last14.iter().map(|&c| if c > 0.0 { c } else { 0.0 }).sum::<f64>() / 14.0;
    let avg_loss = last14.iter().map(|&c| if c < 0.0 { -c } else { 0.0 }).sum::<f64>() / 14.0;
    if avg_loss == 0.0 {
        return 100.0;
    }
    (100.0 - 100.0 / (1.0 + avg_gain / avg_loss)).clamp(0.0, 100.0)
}

fn compute_atr14_pct(v: &[f64], price: f64, daily_scale: f64) -> f64 {
    if v.len() < 2 || price <= 0.0 {
        return 1.0;
    }
    let trs: Vec<f64> = v.windows(2).map(|w| (w[1] - w[0]).abs()).collect();
    let last14 = &trs[trs.len().saturating_sub(14)..];
    if last14.is_empty() {
        return 1.0;
    }
    let atr  = last14.iter().sum::<f64>() / last14.len() as f64;
    let atr_pct = (atr / price) * 100.0;
    (atr_pct * daily_scale).clamp(0.0, 50.0)
}

fn compute_adx14_approx(v: &[f64]) -> f64 {
    const PERIOD: usize = 14;
    if v.len() < PERIOD + 2 {
        return 20.0;
    }
    let changes: Vec<f64> = v.windows(2).map(|w| w[1] - w[0]).collect();
    let mut dx_series: Vec<f64> = Vec::new();
    for i in PERIOD..=changes.len() {
        let window   = &changes[i - PERIOD..i];
        let pos_sum: f64 = window.iter().filter(|&&c| c > 0.0).sum();
        let neg_sum: f64 = window.iter().filter(|&&c| c < 0.0).map(|c| c.abs()).sum();
        let total = pos_sum + neg_sum;
        if total > 0.0 {
            dx_series.push(((pos_sum - neg_sum).abs() / total) * 100.0);
        }
    }
    if dx_series.is_empty() {
        return 20.0;
    }
    let alpha = 1.0 / PERIOD as f64;
    let mut adx = dx_series[0];
    for &dx in dx_series.iter().skip(1) {
        adx = adx * (1.0 - alpha) + dx * alpha;
    }
    adx.clamp(10.0, 80.0)
}

/// Exponential moving average of the last `n` values.
fn ema_tail(v: &[f64], n: usize) -> Option<f64> {
    if v.len() < n {
        return None;
    }
    // Seed with SMA of first n values in the window, then apply EMA
    let window = &v[v.len().saturating_sub(n * 3)..]; // use up to 3× n for warm-up
    if window.len() < n {
        return None;
    }
    let alpha = 2.0 / (n as f64 + 1.0);
    let seed: f64 = window[..n].iter().sum::<f64>() / n as f64;
    let mut ema = seed;
    for &c in window[n..].iter() {
        ema = c * alpha + ema * (1.0 - alpha);
    }
    Some(ema)
}

// ── REST fallback / seed ─────────────────────────────────────────────────────

pub fn rest_poll_once(
    workspace: &Path,
    cfg:       &RealtimeConfig,
    cache:     &mut RealtimeCache,
) -> Result<(), String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(12))
        .user_agent("okx-rs-client/1.0")
        .build()
        .map_err(|e| format!("rest client: {e}"))?;

    for symbol in &cfg.symbols {
        if let Err(e) = fetch_symbol_rest(&client, symbol, cache) {
            if e.contains("429") {
                cache.health.rest_429_count += 1;
            } else {
                cache.health.network_error_count += 1;
            }
            eprintln!("[REALTIME] REST degraded {symbol}: {e}");
        }
        thread::sleep(Duration::from_millis(cfg.rest_throttle_ms));
    }

    let now = Utc::now().to_rfc3339();
    cache.generated_at               = now.clone();
    cache.health.last_update_at      = now;
    cache.health.last_update_age_secs = 0;
    cache.health.stale_feed          = false;
    cache.health.fallback_active     = true;
    persist_cache(workspace, cache)
}

fn fetch_symbol_rest(
    client: &Client,
    symbol: &str,
    cache:  &mut RealtimeCache,
) -> Result<(), String> {
    let inst = to_okx_inst(symbol);

    // ── Ticker ─────────────────────────────────────────────────────────────
    let t_resp = client
        .get(format!("{REST_BASE}/api/v5/market/ticker?instId={inst}"))
        .send()
        .map_err(|e| format!("ticker send: {e}"))?;
    if t_resp.status().as_u16() == 429 { return Err("429 ticker".into()); }
    let t_json: serde_json::Value = t_resp.json().map_err(|e| format!("ticker json: {e}"))?;
    let t = t_json.get("data").and_then(|x| x.as_array())
        .and_then(|a| a.first()).ok_or("ticker data missing")?;

    let state = cache.symbols.entry(symbol.to_string()).or_insert_with(|| SymbolState {
        symbol: symbol.to_string(),
        ..Default::default()
    });
    state.price   = parse_num(t.get("last"));
    let sod = parse_num(t.get("sodUtc0"));
    if sod > 0.0 {
        state.ret_24h = sanitize::ret_pct(((state.price / sod) - 1.0) * 100.0);
    }

    // ── Seed 1m closes (100 candles) ───────────────────────────────────────
    fetch_candles_into(client, &inst, "1m",  100, &mut state.closes)?;

    // ── Seed H1 closes (120 candles ≈ 5 days) ──────────────────────────────
    fetch_candles_into(client, &inst, "1H",  120, &mut state.closes_1h)?;

    // ── Seed H4 closes (120 candles ≈ 20 days) ─────────────────────────────
    fetch_candles_into(client, &inst, "4H",  120, &mut state.closes_4h)?;

    // ── Seed H8 closes (60 candles ≈ 20 days) ──────────────────────────────
    fetch_candles_into(client, &inst, "6H",  60, &mut state.closes_8h)?;

    // ── Seed H12 closes (60 candles ≈ 30 days) ─────────────────────────────
    fetch_candles_into(client, &inst, "12H", 60, &mut state.closes_12h)?;

    // ── Seed D1 closes (100 candles ≈ 3 months) ────────────────────────────
    fetch_candles_into(client, &inst, "1D",  100, &mut state.closes_1d)?;

    recompute_indicators(state);
    state.updated_at = Utc::now().to_rfc3339();
    Ok(())
}

/// Fetch historical candles and populate a VecDeque (chronological order).
fn fetch_candles_into(
    client: &Client,
    inst:   &str,
    bar:    &str,
    limit:  usize,
    deque:  &mut VecDeque<f64>,
) -> Result<(), String> {
    let url = format!("{REST_BASE}/api/v5/market/candles?instId={inst}&bar={bar}&limit={limit}");
    let resp = client.get(&url).send().map_err(|e| format!("candles({bar}) send: {e}"))?;
    if resp.status().as_u16() == 429 { return Err(format!("429 candles {bar}")); }
    let json: serde_json::Value = resp.json().map_err(|e| format!("candles({bar}) json: {e}"))?;

    if let Some(rows) = json.get("data").and_then(|x| x.as_array()) {
        let mut closes: Vec<f64> = rows
            .iter()
            .filter_map(|r| r.as_array())
            .filter_map(|r| r.get(4))
            .filter_map(|x| x.as_str())
            .filter_map(|x| x.parse::<f64>().ok())
            .collect();
        // OKX returns newest-first → reverse for chronological order
        closes.reverse();
        deque.clear();
        for c in closes {
            deque.push_back(c);
        }
    }
    Ok(())
}

// ── Helpers ─────────────────────────────────────────────────────────────────

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

fn book_top(v: Option<&serde_json::Value>) -> f64 {
    v.and_then(|b| b.as_array())
        .and_then(|a| a.first())
        .and_then(|r| r.as_array())
        .and_then(|r| r.first())
        .and_then(|x| x.as_str())
        .and_then(|x| x.parse::<f64>().ok())
        .unwrap_or(0.0)
}

fn volume_baseline(symbol: &str) -> f64 {
    match symbol {
        "BTCUSDT"  => 200_000_000.0,
        "ETHUSDT"  => 100_000_000.0,
        "SOLUSDT"  => 20_000_000.0,
        "BNBUSDT"  => 10_000_000.0,
        "XRPUSDT"  => 15_000_000.0,
        "ADAUSDT"  => 8_000_000.0,
        "DOGEUSDT" => 10_000_000.0,
        "AVAXUSDT" => 8_000_000.0,
        "LINKUSDT" => 5_000_000.0,
        "DOTUSDT"  => 5_000_000.0,
        _          => 5_000_000.0,
    }
}

fn set_nonblocking(stream: &mut MaybeTlsStream<std::net::TcpStream>) -> std::io::Result<()> {
    match stream {
        MaybeTlsStream::Plain(s)   => s.set_nonblocking(true),
        MaybeTlsStream::Rustls(s)  => s.get_ref().set_nonblocking(true),
        _ => Ok(()),
    }
}

fn to_okx_inst(symbol: &str) -> String {
    symbol.replace("USDT", "-USDT")
}

fn from_okx_inst(inst: &str) -> String {
    inst.replace('-', "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Build a candle WS push array as OKX sends it:
    /// outer = [inner_row], inner = [ts, open, high, low, close, vol, volCcy, volCcyQuote, confirm]
    fn candle_row(close: &str, confirm: &str) -> Vec<serde_json::Value> {
        vec![json!(["1700000000000", "100.0", "105.0", "98.0", close,
                    "1000.0", "100000.0", "100000.0", confirm])]
    }

    // ── confirmed_close ────────────────────────────────────────────────────

    #[test]
    fn confirmed_close_returns_price_when_confirmed() {
        let arr = candle_row("86.39", "1");
        let result = confirmed_close(&arr);
        assert!(result.is_some(), "confirm=1 must return Some(close)");
        assert!((result.unwrap() - 86.39).abs() < 0.001,
            "Close value must match: {:.4}", result.unwrap());
    }

    #[test]
    fn confirmed_close_returns_none_when_unconfirmed() {
        let arr = candle_row("86.39", "0");
        assert!(confirmed_close(&arr).is_none(),
            "confirm=0 (in-progress candle) must return None");
    }

    #[test]
    fn confirmed_close_returns_none_for_empty_outer_array() {
        let arr: Vec<serde_json::Value> = vec![];
        assert!(confirmed_close(&arr).is_none(),
            "Empty outer array must return None");
    }

    #[test]
    fn confirmed_close_returns_none_for_empty_inner_row() {
        let arr = vec![json!([])];
        assert!(confirmed_close(&arr).is_none(),
            "Empty inner row must return None");
    }

    #[test]
    fn confirmed_close_returns_none_when_confirm_field_missing() {
        // Inner row has only 8 elements — no confirm at index 8
        let arr = vec![json!(["1700000000000", "100.0", "105.0", "98.0",
                               "86.39", "1000.0", "100000.0", "100000.0"])];
        assert!(confirmed_close(&arr).is_none(),
            "Missing confirm field must return None (safe default)");
    }

    #[test]
    fn confirmed_close_returns_none_for_unparseable_close() {
        // confirm=1 but close is not a valid f64
        let arr = vec![json!(["1700000000000", "100.0", "105.0", "98.0",
                               "NOT_A_NUMBER", "1000.0", "100000.0", "100000.0", "1"])];
        assert!(confirmed_close(&arr).is_none(),
            "Non-numeric close must return None even when confirmed");
    }

    #[test]
    fn confirmed_close_rejects_numeric_confirm_field() {
        // confirm field is integer 1 (not string "1") — must be treated as unconfirmed
        let arr = vec![json!(["1700000000000", "100.0", "105.0", "98.0",
                               "86.39", "1000.0", "100000.0", "100000.0", 1])];
        assert!(confirmed_close(&arr).is_none(),
            "Numeric 1 in confirm field must not match string '1'");
    }

    // ── push_close (deque bounded growth) ─────────────────────────────────

    #[test]
    fn push_close_respects_max_size() {
        let mut dq: std::collections::VecDeque<f64> = std::collections::VecDeque::new();
        for i in 0..150 {
            push_close(&mut dq, i as f64, 120);
        }
        assert_eq!(dq.len(), 120, "Deque must not exceed max=120");
    }

    #[test]
    fn push_close_evicts_oldest_first() {
        let mut dq: std::collections::VecDeque<f64> = std::collections::VecDeque::new();
        for i in 0..5 {
            push_close(&mut dq, i as f64, 3);
        }
        // After pushing 0,1,2,3,4 with max=3, should have [2,3,4]
        assert_eq!(dq[0], 2.0, "Oldest must be evicted first");
        assert_eq!(dq[2], 4.0, "Newest must be at back");
    }

    // ── ret_1h intra-candle momentum ───────────────────────────────────────

    fn make_state_with_closes_1h(price: f64, prev_1h_close: f64) -> SymbolState {
        let mut state = SymbolState::default();
        state.price = price;
        state.ret_24h = 2.0;
        // Need enough closes for indicators; fill closes_4h with dummy data
        for i in 0..25 { push_close(&mut state.closes_4h, 100.0 + i as f64 * 0.1, MAX_4H); }
        push_close(&mut state.closes_1h, prev_1h_close, MAX_1H);
        state
    }

    #[test]
    fn ret_1h_positive_when_price_above_last_close() {
        // prev close=1.560, current price=1.594 → ret_1h ≈ +2.18%
        let mut state = make_state_with_closes_1h(1.594, 1.560);
        recompute_indicators(&mut state);
        assert!(state.ret_1h > 0.0,
            "Price above prev 1H close must yield positive ret_1h, got {:.4}", state.ret_1h);
        let expected = (1.594 - 1.560) / 1.560 * 100.0;
        assert!((state.ret_1h - expected).abs() < 0.01,
            "ret_1h={:.4} expected≈{:.4}", state.ret_1h, expected);
    }

    #[test]
    fn ret_1h_negative_on_fast_crash() {
        // prev close=1.630, current price=1.550 → ret_1h ≈ -4.9% (flash crash)
        let mut state = make_state_with_closes_1h(1.550, 1.630);
        recompute_indicators(&mut state);
        assert!(state.ret_1h < -3.0,
            "Fast crash -4.9% must yield ret_1h < -3.0, got {:.4}", state.ret_1h);
    }

    #[test]
    fn ret_1h_zero_when_no_1h_closes() {
        // No 1H closes yet (fresh warmup) → ret_1h must be 0.0 (fail-open)
        let mut state = SymbolState::default();
        state.price = 1.594;
        state.ret_24h = 2.0;
        for i in 0..25 { push_close(&mut state.closes_4h, 100.0 + i as f64 * 0.1, MAX_4H); }
        recompute_indicators(&mut state);
        assert_eq!(state.ret_1h, 0.0,
            "No 1H closes → ret_1h must be 0.0 (fail-open), got {:.4}", state.ret_1h);
    }

    // ── Binance kline parsing ────────────────────────────────────────────

    fn binance_kline_event(symbol: &str, interval: &str, close: &str, is_closed: bool) -> String {
        json!({
            "stream": format!("{}@kline_{}", symbol.to_lowercase(), interval),
            "data": {
                "k": {
                    "s": symbol,
                    "i": interval,
                    "o": "57000.00",
                    "h": "57600.00",
                    "l": "56900.00",
                    "c": close,
                    "v": "1000.00",
                    "q": "57500000.00",
                    "x": is_closed
                }
            }
        }).to_string()
    }

    #[test]
    fn binance_kline_confirmed_close_updates_cache() {
        let mut cache = RealtimeCache::default();
        let msg = binance_kline_event("BTCUSDT", "1h", "57500.00", true);
        let changed = apply_binance_kline(&msg, &mut cache);
        assert!(changed, "Confirmed kline must return true");
        let state = cache.symbols.get("BTCUSDT").expect("BTCUSDT must exist");
        assert!((state.price - 57500.0).abs() < 0.01, "Price must be 57500");
        assert_eq!(state.closes_1h.len(), 1, "1h deque must have 1 entry");
        assert!((state.closes_1h[0] - 57500.0).abs() < 0.01);
    }

    #[test]
    fn binance_kline_unconfirmed_updates_price_only() {
        let mut cache = RealtimeCache::default();
        let msg = binance_kline_event("ETHUSDT", "4h", "3200.00", false);
        let changed = apply_binance_kline(&msg, &mut cache);
        assert!(!changed, "Unconfirmed kline must return false");
        let state = cache.symbols.get("ETHUSDT").expect("ETHUSDT must exist");
        assert!((state.price - 3200.0).abs() < 0.01, "Price must be updated");
        assert_eq!(state.closes_4h.len(), 0, "4h deque must be empty (not confirmed)");
    }

    #[test]
    fn binance_kline_routes_to_correct_timeframe_deque() {
        let mut cache = RealtimeCache::default();
        // 4h confirmed close
        apply_binance_kline(&binance_kline_event("SOLUSDT", "4h", "150.00", true), &mut cache);
        // 6h confirmed close
        apply_binance_kline(&binance_kline_event("SOLUSDT", "6h", "151.00", true), &mut cache);
        // 12h confirmed close
        apply_binance_kline(&binance_kline_event("SOLUSDT", "12h", "152.00", true), &mut cache);

        let state = cache.symbols.get("SOLUSDT").unwrap();
        assert_eq!(state.closes_4h.len(), 1, "4h deque must have 1");
        assert_eq!(state.closes_8h.len(), 1, "6h→8h deque must have 1");
        assert_eq!(state.closes_12h.len(), 1, "12h deque must have 1");
        assert!((state.closes_8h[0] - 151.0).abs() < 0.01, "6h close must be 151");
    }

    #[test]
    fn binance_kline_ignores_malformed_json() {
        let mut cache = RealtimeCache::default();
        assert!(!apply_binance_kline("not json", &mut cache));
        assert!(!apply_binance_kline("{}", &mut cache));
        assert!(!apply_binance_kline(r#"{"data":{}}"#, &mut cache));
    }

    #[test]
    fn parse_binance_num_handles_string_and_number() {
        let s = json!("123.45");
        assert!((parse_binance_num(Some(&s)) - 123.45).abs() < 0.001);
        let n = json!(678.9);
        assert!((parse_binance_num(Some(&n)) - 678.9).abs() < 0.001);
        assert_eq!(parse_binance_num(None), 0.0);
    }
}
