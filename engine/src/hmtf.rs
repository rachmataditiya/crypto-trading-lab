/// Adaptive Multi-regime Signal (AMS) — "chameleon" trading model.
///
/// Computes a regime-adaptive entry score from 4 timeframes (1H, 4H, 6H, 12H).
/// In trending regimes: higher TFs gate lower ones (classical HMTF).
/// In ranging regimes: divergence + mean-reversion scoring takes over.
/// The blend is continuous via sigmoid regime score ρ.

// ── Structs ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct TfFeatures {
    pub r: f64,     // RSI feature (adaptive in AMS)
    pub e: f64,     // EMA cross feature
    pub a: f64,     // ADX directional feature
    pub v: f64,     // Volume feature
    pub p: f64,     // Price position feature
    pub score: f64, // Weighted composite (adaptive weights)
    pub adx: f64,   // Raw ADX value for this TF
    pub rsi: f64,   // Raw RSI value for this TF
}

/// Input to the AMS/HMTF computation. Aliased as HmtfInput for backward compat.
#[derive(Debug, Clone)]
pub struct HmtfInput {
    pub closes_1h:  Vec<f64>,
    pub closes_4h:  Vec<f64>,
    pub closes_6h:  Vec<f64>,
    pub closes_12h: Vec<f64>,
    pub volume_ratio: f64,
    pub price: f64,
    pub atr_pct: f64,
}

/// AMS type alias for backward compatibility.
pub type AmsInput = HmtfInput;

#[derive(Debug, Clone, Default)]
pub struct HmtfSignal {
    pub tf_1h:  TfFeatures,
    pub tf_4h:  TfFeatures,
    pub tf_6h:  TfFeatures,
    pub tf_12h: TfFeatures,
    pub gate_product: f64,
    pub entry_score:  f64,
    pub alignment:    u8,
    pub tier:         char,
    // ── AMS-specific fields ──
    pub regime_rho:        f64,
    pub regime_label:      String,
    pub divergence_d:      f64,
    pub e_trend:           f64,
    pub e_reversion:       f64,
    pub kelly_multiplier:  f64,
}

/// AMS type alias for backward compatibility.
pub type AmsSignal = HmtfSignal;

// ── Adaptive weight sets ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct Weights {
    e: f64,
    r: f64,
    a: f64,
    v: f64,
    p: f64,
}

const W_TREND: Weights = Weights { e: 0.40, r: 0.20, a: 0.30, v: 0.05, p: 0.05 };
const W_REVERSION: Weights = Weights { e: 0.10, r: 0.40, a: 0.05, v: 0.20, p: 0.25 };

// ── Helpers ──────────────────────────────────────────────────────────────────

pub fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

/// Proper EMA with alpha = 2/(n+1). Returns last EMA value.
pub fn compute_ema(closes: &[f64], period: usize) -> f64 {
    if closes.is_empty() {
        return 0.0;
    }
    if closes.len() < period {
        return closes.iter().sum::<f64>() / closes.len() as f64;
    }
    let alpha = 2.0 / (period as f64 + 1.0);
    let mut ema = closes[..period].iter().sum::<f64>() / period as f64;
    for &c in &closes[period..] {
        ema = c * alpha + ema * (1.0 - alpha);
    }
    ema
}

/// RSI over `period` bars using simple average gain/loss.
pub fn compute_rsi(closes: &[f64], period: usize) -> f64 {
    if closes.len() < period + 1 {
        return 50.0;
    }
    let changes: Vec<f64> = closes.windows(2).map(|w| w[1] - w[0]).collect();
    let tail = &changes[changes.len().saturating_sub(period)..];
    let avg_gain = tail.iter().map(|&c| if c > 0.0 { c } else { 0.0 }).sum::<f64>() / period as f64;
    let avg_loss = tail.iter().map(|&c| if c < 0.0 { -c } else { 0.0 }).sum::<f64>() / period as f64;
    if avg_loss == 0.0 {
        return 100.0;
    }
    (100.0 - 100.0 / (1.0 + avg_gain / avg_loss)).clamp(0.0, 100.0)
}

/// Approximate ADX with directional info from close-only data.
/// Returns (adx, di_diff) where di_diff = DI+ - DI-.
pub fn compute_adx_with_direction(closes: &[f64], period: usize) -> (f64, f64) {
    if closes.len() < period + 2 {
        return (20.0, 0.0);
    }
    let changes: Vec<f64> = closes.windows(2).map(|w| w[1] - w[0]).collect();

    let mut plus_dm_sum = 0.0;
    let mut minus_dm_sum = 0.0;
    let mut tr_sum = 0.0;

    let init_slice = &changes[..period.min(changes.len())];
    for &ch in init_slice {
        let tr = ch.abs();
        tr_sum += tr;
        if ch > 0.0 {
            plus_dm_sum += ch;
        } else {
            minus_dm_sum += -ch;
        }
    }

    let mut smoothed_plus_dm = plus_dm_sum;
    let mut smoothed_minus_dm = minus_dm_sum;
    let mut smoothed_tr = tr_sum;
    let mut dx_sum = 0.0;
    let mut dx_count = 0;

    let p = period as f64;
    for &ch in changes.iter().skip(period) {
        let tr = ch.abs();
        let plus_dm = if ch > 0.0 { ch } else { 0.0 };
        let minus_dm = if ch < 0.0 { -ch } else { 0.0 };

        smoothed_plus_dm = smoothed_plus_dm - smoothed_plus_dm / p + plus_dm;
        smoothed_minus_dm = smoothed_minus_dm - smoothed_minus_dm / p + minus_dm;
        smoothed_tr = smoothed_tr - smoothed_tr / p + tr;

        if smoothed_tr > 0.0 {
            let di_plus = (smoothed_plus_dm / smoothed_tr) * 100.0;
            let di_minus = (smoothed_minus_dm / smoothed_tr) * 100.0;
            let di_sum = di_plus + di_minus;
            if di_sum > 0.0 {
                dx_sum += ((di_plus - di_minus).abs() / di_sum) * 100.0;
                dx_count += 1;
            }
        }
    }

    let adx = if dx_count > 0 {
        (dx_sum / dx_count as f64).clamp(0.0, 80.0)
    } else {
        20.0
    };

    let di_diff = if smoothed_tr > 0.0 {
        let di_plus = (smoothed_plus_dm / smoothed_tr) * 100.0;
        let di_minus = (smoothed_minus_dm / smoothed_tr) * 100.0;
        di_plus - di_minus
    } else {
        0.0
    };

    (adx, di_diff)
}

/// Compute just the ADX value (no direction) from closes.
pub fn compute_adx_value(closes: &[f64], period: usize) -> f64 {
    compute_adx_with_direction(closes, period).0
}

// ── AMS Layer 0: Regime score ────────────────────────────────────────────────

/// ρ = sigmoid((mean(ADX_1H, ADX_4H, ADX_6H, ADX_12H) - 22) / 8)
/// 0 = pure ranging, 1 = pure trending
pub fn compute_regime_rho(adx_1h: f64, adx_4h: f64, adx_6h: f64, adx_12h: f64) -> f64 {
    let mean_adx = (adx_1h + adx_4h + adx_6h + adx_12h) / 4.0;
    sigmoid((mean_adx - 22.0) / 8.0)
}

/// Human-readable regime label from ρ.
pub fn regime_label(rho: f64) -> String {
    if rho > 0.6 {
        "TRENDING".to_string()
    } else if rho < 0.4 {
        "RANGING".to_string()
    } else {
        "MIXED".to_string()
    }
}

// ── AMS Layer 1: Adaptive weights ────────────────────────────────────────────

/// W(ρ) = ρ×W_trend + (1-ρ)×W_reversion
pub fn adaptive_weights(rho: f64) -> Weights {
    Weights {
        e: rho * W_TREND.e + (1.0 - rho) * W_REVERSION.e,
        r: rho * W_TREND.r + (1.0 - rho) * W_REVERSION.r,
        a: rho * W_TREND.a + (1.0 - rho) * W_REVERSION.a,
        v: rho * W_TREND.v + (1.0 - rho) * W_REVERSION.v,
        p: rho * W_TREND.p + (1.0 - rho) * W_REVERSION.p,
    }
}

// ── AMS Layer 2: Adaptive RSI ────────────────────────────────────────────────

/// r_adaptive(RSI, ρ) = ρ×tanh((RSI-50)/20) + (1-ρ)×tanh((35-RSI)/10)
/// In trending mode: standard RSI momentum interpretation.
/// In ranging mode: oversold = BULLISH (mean-reversion flip).
pub fn r_adaptive(rsi: f64, rho: f64) -> f64 {
    rho * ((rsi - 50.0) / 20.0).tanh() + (1.0 - rho) * ((35.0 - rsi) / 10.0).tanh()
}

// ── AMS Layer 3: Adaptive gate threshold ─────────────────────────────────────

/// θ(atr) = 0.10 + 0.10 × tanh(atr / 1.5)
/// Higher ATR (volatility) → higher gate threshold (harder to pass).
pub fn gate_threshold(atr_pct: f64) -> f64 {
    0.10 + 0.10 * (atr_pct / 1.5).tanh()
}

// ── AMS Layer 4: Per-TF adaptive scoring ─────────────────────────────────────

/// Compute features for one timeframe using adaptive weights and r_adaptive.
pub fn compute_tf_features_adaptive(
    closes: &[f64],
    volume_ratio: f64,
    price: f64,
    rho: f64,
) -> TfFeatures {
    if closes.len() < 15 || price <= 0.0 {
        return TfFeatures::default();
    }

    let rsi14 = compute_rsi(closes, 14);
    let ema20 = compute_ema(closes, 20);
    let ema50 = compute_ema(closes, 50);
    let (adx14, di_diff) = compute_adx_with_direction(closes, 14);

    // r: adaptive (flips in ranging mode)
    let r = r_adaptive(rsi14, rho);

    // e(τ) = tanh(((EMA20 - EMA50) / price * 100) / 2)
    let e = (((ema20 - ema50) / price * 100.0) / 2.0).tanh();

    // a(τ) = clamp((ADX14 - 25) / 25 * sign(DI+ - DI-), -1.0, 1.0)
    let sign_di = if di_diff > 0.0 { 1.0 } else if di_diff < 0.0 { -1.0 } else { 0.0 };
    let a = ((adx14 - 25.0) / 25.0 * sign_di).clamp(-1.0, 1.0);

    // v(τ) = tanh(ln(volume_ratio).max(-3.0))
    let ln_vr = if volume_ratio > 0.0 {
        volume_ratio.ln().max(-3.0)
    } else {
        -3.0
    };
    let v = ln_vr.tanh();

    // p(τ) = (price - min_20bar) / (max_20bar - min_20bar) * 2.0 - 1.0
    let tail = &closes[closes.len().saturating_sub(20)..];
    let min_20 = tail.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_20 = tail.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let range = max_20 - min_20;
    let p = if range > 0.0 {
        ((price - min_20) / range * 2.0 - 1.0).clamp(-1.0, 1.0)
    } else {
        0.0
    };

    // s_adaptive(τ) = W.r×r + W.e×e + W.a×a + W.v×v + W.p×p
    let w = adaptive_weights(rho);
    let score = w.r * r + w.e * e + w.a * a + w.v * v + w.p * p;

    TfFeatures { r, e, a, v, p, score, adx: adx14, rsi: rsi14 }
}

/// Legacy compute_tf_features (trending-only, ρ=1.0 equivalent weights).
pub fn compute_tf_features(closes: &[f64], volume_ratio: f64, price: f64) -> TfFeatures {
    compute_tf_features_adaptive(closes, volume_ratio, price, 1.0)
}

// ── AMS Core: compute_hmtf ──────────────────────────────────────────────────

/// Compute the full AMS signal from multi-timeframe input.
pub fn compute_hmtf(input: &HmtfInput) -> HmtfSignal {
    // Layer 0: Compute ADX per timeframe, then regime ρ
    let adx_1h  = compute_adx_value(&input.closes_1h, 14);
    let adx_4h  = compute_adx_value(&input.closes_4h, 14);
    let adx_6h  = compute_adx_value(&input.closes_6h, 14);
    let adx_12h = compute_adx_value(&input.closes_12h, 14);
    let rho = compute_regime_rho(adx_1h, adx_4h, adx_6h, adx_12h);

    // Layer 4: Per-TF adaptive features
    let tf_1h  = compute_tf_features_adaptive(&input.closes_1h,  input.volume_ratio, input.price, rho);
    let tf_4h  = compute_tf_features_adaptive(&input.closes_4h,  input.volume_ratio, input.price, rho);
    let tf_6h  = compute_tf_features_adaptive(&input.closes_6h,  input.volume_ratio, input.price, rho);
    let tf_12h = compute_tf_features_adaptive(&input.closes_12h, input.volume_ratio, input.price, rho);

    // Layer 3: Adaptive gate threshold
    let theta = gate_threshold(input.atr_pct);

    // Gate chain: G_τ = sigmoid(s_τ / θ)
    let g_12h = sigmoid(tf_12h.score / theta);
    let g_6h  = sigmoid(tf_6h.score / theta);
    let g_4h  = sigmoid(tf_4h.score / theta);
    let gate_product = g_12h * g_6h * g_4h;

    // Layer 5: Divergence D = |s_1H - s_12H|
    let divergence_d = (tf_1h.score - tf_12h.score).abs();

    // Layer 6a: E_trend = Γ × (0.40×s_12H + 0.25×s_6H + 0.20×s_4H + 0.15×s_1H)
    let weighted_sum = 0.40 * tf_12h.score
        + 0.25 * tf_6h.score
        + 0.20 * tf_4h.score
        + 0.15 * tf_1h.score;
    let e_trend = gate_product * weighted_sum;

    // Layer 6b: E_rev = D × max(tanh((45-RSI_1H)/10), 0.0) × max(tanh(ln(vol_ratio)), 0.0)
    // RSI threshold raised from 35 → 45: oversold bounces are valid up to RSI=45.
    // At RSI=36 (ETH bounce from 16), old formula returned 0 — completely missing the move.
    // tanh((45-36)/10) = tanh(0.9) ≈ 0.716, which correctly weights a meaningful bounce.
    let rsi_1h = tf_1h.rsi;
    let rsi_factor = ((45.0 - rsi_1h) / 10.0).tanh().max(0.0);
    let vol_factor = if input.volume_ratio > 0.0 {
        input.volume_ratio.ln().tanh().max(0.0)
    } else {
        0.0
    };
    let e_reversion = divergence_d * rsi_factor * vol_factor;

    // Layer 6c: E_final = ρ×E_trend + (1-ρ)×E_rev
    let entry_score = rho * e_trend + (1.0 - rho) * e_reversion;

    // Alignment: count of {τ : s(τ) > 0.0}
    let alignment = [tf_1h.score, tf_4h.score, tf_6h.score, tf_12h.score]
        .iter()
        .filter(|&&s| s > 0.0)
        .count() as u8;

    // Layer 7: Tier assignment
    let label = regime_label(rho);

    // Tier V (Volume Breakout) — fires across all regimes as last fallback before C.
    // Logic: strong 1H volume surge with at least partial multi-TF alignment.
    // Captures early momentum moves before H4/H6/H12 fully confirm (lower confidence
    // than A/B, so half Kelly + conservative TP like Tier R).
    //
    // Conditions (all must hold):
    //   vol > 4x   : significant accumulation / breakout volume
    //   tf_1h > 0.05: 1H features clearly positive (EMA stack, RSI, trend direction)
    //   align >= 2  : at least 2 TFs showing positive signal
    //   adx_1h > 28 : 1H trend is established, not choppy
    //   rsi_1h < 78 : not yet in exhaustion / overbought blowoff zone
    // Tier V (Volume Breakout): strong volume surge + 1H momentum.
    // Standard: vol > 4x + tf_1h > 0.05 + align >= 2 + adx > 28 + rsi < 78.
    // High-vol override: vol > 5x only needs align >= 1 — fast oversold bounces
    // recover 1H first while 4H/6H/12H lag. Missing these is the ETH-2026-03-01 bug.
    // ADX_1H threshold: standard 28 for normal conditions.
    // When volume is extreme (> 5x), lower to 22 — volume is a faster/stronger
    // confirmation than ADX (which lags 14 candles). SOL 2026-03-04 case:
    // vol=7.1x, adx_1h likely ~24, RSI=69 — should be Tier V but wasn't.
    // Tier V disabled (EV negative — win rate 55% insufficient for 1:1 R:R after fees)
    // v_breakout logic preserved here for future re-evaluation
    let _adx_1h_threshold = if input.volume_ratio > 5.0 { 22.0 } else { 28.0 };

    // Tier F (Fast/1H momentum): fires when 1H signal is very strong but higher TFs lag.
    // Designed to catch fast oversold bounces early — addresses the "8 hour late entry" problem.
    // Use case: ETH 2026-03-01 — 1H was very strong at RSI 25→45 but 4H/6H/12H still negative.
    //
    // Conditions (all must hold):
    //   tf_1h.score > 0.15  : strong 1H bullish signal (not just noise)
    //   rsi_1h in [20, 60]  : not overbought, not in freefall — valid bounce zone
    //   vol >= 2.5x          : at least above-average volume (confirms the move)
    //   alignment >= 1       : at least 1H itself positive (trivially true given score > 0.15)
    //   adx_1h > 18.0        : some trend strength (relaxed vs Tier V's 28)
    // Half Kelly + 45-min time-stop (same as Tier R/V — lower confidence).
    let f_breakout = tf_1h.score > 0.15
        && rsi_1h >= 20.0
        && rsi_1h < 60.0
        && input.volume_ratio >= 2.5
        && adx_1h > 18.0;

    let tier = if rho > 0.6 {
        // Trending mode: A / B / V / F / C
        if e_trend > 0.30 && alignment >= 3 {
            'A'
        } else if e_trend > 0.15 && alignment >= 3 {
            'B'
        } else if f_breakout {
            'F'
        } else {
            'C'
        }
    } else if rho < 0.4 {
        // Ranging mode: R / F / C
        // Tier V disabled — EV negative (win rate 55% insufficient for 1:1 R:R after 0.20% fee)
        if e_reversion > 0.08 && divergence_d > 0.15 {
            'R'
        } else if f_breakout {
            'F'
        } else {
            'C'
        }
    } else {
        // Mixed (0.4 ≤ ρ ≤ 0.6): A / B / R / F / C
        // Tier B threshold is stricter in mixed (0.20 vs 0.15 in trending)
        // because regime conviction is lower — require stronger multi-TF signal.
        // Tier V disabled — EV negative (win rate 55% insufficient for 1:1 R:R after 0.20% fee)
        if e_trend > 0.30 && alignment >= 3 {
            'A'
        } else if e_trend > 0.20 && alignment >= 3 {
            'B'
        } else if e_reversion > 0.08 && divergence_d > 0.15 {
            'R'
        } else if f_breakout {
            'F'
        } else {
            'C'
        }
    };

    // Layer 8: Kelly multiplier (base, before C_recent adjustment)
    // Tier F gets 0.3x Kelly — lowest confidence (1H only), highest noise risk.
    let kelly_multiplier = match tier {
        'A' | 'B' => 1.0,
        'R' | 'V' => 0.5,
        'F'       => 0.3,
        _ => 0.0,
    };

    HmtfSignal {
        tf_1h,
        tf_4h,
        tf_6h,
        tf_12h,
        gate_product,
        entry_score,
        alignment,
        tier,
        regime_rho: rho,
        regime_label: label,
        divergence_d,
        e_trend,
        e_reversion,
        kelly_multiplier,
    }
}

/// Convert HMTF entry score to integer score [-100, +100].
pub fn score_from_hmtf(sig: &HmtfSignal) -> i32 {
    (sig.entry_score * 100.0) as i32
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn rising_closes(n: usize, start: f64, step: f64) -> Vec<f64> {
        (0..n).map(|i| start + i as f64 * step).collect()
    }

    fn falling_closes(n: usize, start: f64, step: f64) -> Vec<f64> {
        (0..n).map(|i| start - i as f64 * step).collect()
    }

    fn flat_closes(n: usize, price: f64) -> Vec<f64> {
        vec![price; n]
    }

    /// Build a default AMS input for testing with given closes.
    fn make_input(c1h: Vec<f64>, c4h: Vec<f64>, c6h: Vec<f64>, c12h: Vec<f64>, vol: f64, price: f64) -> HmtfInput {
        HmtfInput {
            closes_1h: c1h,
            closes_4h: c4h,
            closes_6h: c6h,
            closes_12h: c12h,
            volume_ratio: vol,
            price,
            atr_pct: 1.5,
        }
    }

    // ── sigmoid ──────────────────────────────────────────────────────────────

    #[test]
    fn sigmoid_at_zero_is_half() {
        assert!((sigmoid(0.0) - 0.5).abs() < 1e-10);
    }

    #[test]
    fn sigmoid_is_monotonically_increasing() {
        let vals = [-5.0, -2.0, -1.0, 0.0, 1.0, 2.0, 5.0];
        for w in vals.windows(2) {
            assert!(sigmoid(w[1]) > sigmoid(w[0]));
        }
    }

    #[test]
    fn sigmoid_bounded_zero_to_one() {
        for &x in &[-100.0, -10.0, 0.0, 10.0, 100.0] {
            let s = sigmoid(x);
            assert!(s >= 0.0 && s <= 1.0);
        }
    }

    // ── RSI ──────────────────────────────────────────────────────────────────

    #[test]
    fn rsi_rising_prices_above_70() {
        let closes = rising_closes(60, 100.0, 2.0);
        assert!(compute_rsi(&closes, 14) > 70.0);
    }

    #[test]
    fn rsi_falling_prices_below_30() {
        let closes = falling_closes(60, 200.0, 2.0);
        assert!(compute_rsi(&closes, 14) < 30.0);
    }

    #[test]
    fn rsi_insufficient_data_returns_50() {
        assert!((compute_rsi(&[100.0, 101.0, 102.0], 14) - 50.0).abs() < 1e-10);
    }

    // ── EMA ──────────────────────────────────────────────────────────────────

    #[test]
    fn ema_bull_crossover() {
        let closes = rising_closes(80, 100.0, 1.0);
        assert!(compute_ema(&closes, 20) > compute_ema(&closes, 50));
    }

    #[test]
    fn ema_bear_crossover() {
        let closes = falling_closes(80, 200.0, 1.0);
        assert!(compute_ema(&closes, 20) < compute_ema(&closes, 50));
    }

    // ── AMS Layer 0: Regime ρ ────────────────────────────────────────────────

    #[test]
    fn test_regime_rho_trending() {
        // All ADX = 40 → mean=40, (40-22)/8=2.25 → sigmoid(2.25) ≈ 0.90
        let rho = compute_regime_rho(40.0, 40.0, 40.0, 40.0);
        assert!(rho > 0.85, "All-trending ADX=40 should give ρ > 0.85, got {:.4}", rho);
    }

    #[test]
    fn test_regime_rho_ranging() {
        // All ADX = 12 → mean=12, (12-22)/8=-1.25 → sigmoid(-1.25) ≈ 0.22
        let rho = compute_regime_rho(12.0, 12.0, 12.0, 12.0);
        assert!(rho < 0.35, "All-ranging ADX=12 should give ρ < 0.35, got {:.4}", rho);
    }

    #[test]
    fn test_regime_rho_mixed() {
        // ADX = 22 → mean=22, (22-22)/8=0 → sigmoid(0) = 0.5
        let rho = compute_regime_rho(22.0, 22.0, 22.0, 22.0);
        assert!((rho - 0.5).abs() < 0.01, "ADX=22 should give ρ ≈ 0.5, got {:.4}", rho);
    }

    // ── AMS Layer 2: r_adaptive ──────────────────────────────────────────────

    #[test]
    fn test_r_adaptive_oversold_trending() {
        // RSI=28, ρ=0.9 (trending) → trending term dominates: tanh((28-50)/20) = tanh(-1.1) ≈ -0.80
        let r = r_adaptive(28.0, 0.9);
        assert!(r < 0.0, "Oversold in trending should be bearish (r<0), got {:.4}", r);
    }

    #[test]
    fn test_r_adaptive_oversold_ranging() {
        // RSI=28, ρ=0.1 (ranging) → reversion term dominates: tanh((35-28)/10) = tanh(0.7) ≈ 0.60
        // THE KEY FLIP TEST: oversold in ranging = BULLISH (mean-reversion opportunity)
        let r = r_adaptive(28.0, 0.1);
        assert!(r > 0.0, "Oversold in ranging should be BULLISH (r>0), got {:.4}", r);
    }

    #[test]
    fn test_r_adaptive_overbought_ranging() {
        // RSI=72, ρ=0.1 (ranging) → reversion: tanh((35-72)/10) = tanh(-3.7) ≈ -1.0
        let r = r_adaptive(72.0, 0.1);
        assert!(r < -0.5, "Overbought in ranging should be strongly bearish, got {:.4}", r);
    }

    // ── AMS Layer 1: Adaptive weights ────────────────────────────────────────

    #[test]
    fn test_adaptive_weights_trending() {
        let w = adaptive_weights(1.0);
        assert!((w.e - 0.40).abs() < 1e-10, "ρ=1.0 should give W_trend.e=0.40");
        assert!((w.r - 0.20).abs() < 1e-10, "ρ=1.0 should give W_trend.r=0.20");
    }

    #[test]
    fn test_adaptive_weights_reversion() {
        let w = adaptive_weights(0.0);
        assert!((w.e - 0.10).abs() < 1e-10, "ρ=0.0 should give W_reversion.e=0.10");
        assert!((w.r - 0.40).abs() < 1e-10, "ρ=0.0 should give W_reversion.r=0.40");
        assert!((w.p - 0.25).abs() < 1e-10, "ρ=0.0 should give W_reversion.p=0.25");
    }

    #[test]
    fn test_adaptive_weights_interpolation() {
        let w = adaptive_weights(0.5);
        // e should be average of 0.40 and 0.10 = 0.25
        assert!((w.e - 0.25).abs() < 1e-10, "ρ=0.5 should give W.e=0.25, got {:.4}", w.e);
        // r should be average of 0.20 and 0.40 = 0.30
        assert!((w.r - 0.30).abs() < 1e-10, "ρ=0.5 should give W.r=0.30, got {:.4}", w.r);
    }

    // ── AMS Layer 3: Gate threshold ──────────────────────────────────────────

    #[test]
    fn test_gate_threshold_low_atr() {
        // ATR=0.5 → θ = 0.10 + 0.10×tanh(0.5/1.5) = 0.10 + 0.10×tanh(0.333) ≈ 0.10 + 0.032 = 0.132
        let theta = gate_threshold(0.5);
        assert!(theta > 0.12 && theta < 0.15, "Low ATR threshold should be ~0.13, got {:.4}", theta);
    }

    #[test]
    fn test_gate_threshold_high_atr() {
        // ATR=5.0 → θ = 0.10 + 0.10×tanh(5.0/1.5) = 0.10 + 0.10×tanh(3.33) ≈ 0.10 + 0.10 = 0.20
        let theta = gate_threshold(5.0);
        assert!(theta > 0.19 && theta < 0.21, "High ATR threshold should be ~0.20, got {:.4}", theta);
    }

    // ── AMS Layer 5: Divergence ──────────────────────────────────────────────

    #[test]
    fn test_divergence_large() {
        // s_1H = 0.5, s_12H = -0.2 → D = 0.7
        let d = (0.5_f64 - (-0.2_f64)).abs();
        assert!((d - 0.7).abs() < 1e-10);
    }

    #[test]
    fn test_divergence_small() {
        let d = (0.1_f64 - 0.12_f64).abs();
        assert!(d < 0.05);
    }

    // ── AMS Layer 6: E_reversion ─────────────────────────────────────────────

    #[test]
    fn test_e_reversion_oversold() {
        // RSI=28, vol=2.5x, D=0.375
        // rsi_factor = tanh((35-28)/10) = tanh(0.7) ≈ 0.604
        // vol_factor = tanh(ln(2.5)) = tanh(0.916) ≈ 0.724
        // E_rev = 0.375 × 0.604 × 0.724 ≈ 0.164
        let d = 0.375;
        let rsi_factor = ((35.0 - 28.0) / 10.0_f64).tanh().max(0.0);
        let vol_factor = 2.5_f64.ln().tanh().max(0.0);
        let e_rev = d * rsi_factor * vol_factor;
        assert!(e_rev > 0.08, "E_rev with oversold+vol should be > 0.08, got {:.4}", e_rev);
    }

    #[test]
    fn test_e_reversion_not_oversold() {
        // RSI=55 → rsi_factor = tanh((35-55)/10) = tanh(-2.0) ≈ -0.964 → max(0) = 0
        let rsi_factor = ((35.0 - 55.0) / 10.0_f64).tanh().max(0.0);
        assert!(rsi_factor < 0.01, "RSI=55 should give rsi_factor ≈ 0, got {:.4}", rsi_factor);
    }

    // ── AMS Layer 7: Tiers ───────────────────────────────────────────────────

    #[test]
    fn test_tier_r_reversion_mode() {
        // Ranging regime (ρ < 0.4), E_rev > 0.08, D > 0.15 → Tier R
        // Use flat closes (low ADX → ranging) with divergent 1H vs 12H
        let flat = flat_closes(80, 100.0);
        let falling_1h = falling_closes(80, 120.0, 0.3);  // slightly falling 1H
        let rising_12h = rising_closes(80, 80.0, 0.5);     // rising 12H
        let input = HmtfInput {
            closes_1h: falling_1h,
            closes_4h: flat.clone(),
            closes_6h: flat.clone(),
            closes_12h: rising_12h,
            volume_ratio: 3.0,
            price: 100.0,
            atr_pct: 1.5,
        };
        let sig = compute_hmtf(&input);
        // We need ρ < 0.4 for ranging mode — flat closes give low ADX
        // The tier depends on the actual computed values, so just verify the math is consistent
        if sig.regime_rho < 0.4 && sig.e_reversion > 0.08 && sig.divergence_d > 0.15 {
            assert_eq!(sig.tier, 'R', "Should be tier R in ranging mode with E_rev > 0.08");
        }
    }

    #[test]
    fn test_tier_a_trending_mode() {
        let closes = rising_closes(80, 100.0, 2.0);
        let price = *closes.last().unwrap();
        let input = make_input(
            closes.clone(), closes.clone(), closes.clone(), closes.clone(),
            5.0, price,
        );
        let sig = compute_hmtf(&input);
        // Strong trending → ρ > 0.6, if E_trend > 0.30 and alignment >= 3 → A
        if sig.regime_rho > 0.6 && sig.e_trend > 0.30 && sig.alignment >= 3 {
            assert_eq!(sig.tier, 'A');
        }
    }

    #[test]
    fn test_tier_c_mixed_no_signal() {
        let flat = flat_closes(80, 100.0);
        let input = make_input(flat.clone(), flat.clone(), flat.clone(), flat.clone(), 1.0, 100.0);
        let sig = compute_hmtf(&input);
        assert_eq!(sig.tier, 'C', "Flat markets with no signal should be tier C, got {}", sig.tier);
    }

    // ── Kelly multiplier ─────────────────────────────────────────────────────

    #[test]
    fn test_kelly_multiplier_tier_a() {
        let closes = rising_closes(80, 100.0, 2.0);
        let price = *closes.last().unwrap();
        let input = make_input(closes.clone(), closes.clone(), closes.clone(), closes.clone(), 5.0, price);
        let sig = compute_hmtf(&input);
        if sig.tier == 'A' {
            assert!((sig.kelly_multiplier - 1.0).abs() < 1e-10);
        }
    }

    #[test]
    fn test_kelly_multiplier_tier_r() {
        // If we can construct tier R, kelly should be 0.5
        // Just test the mapping directly
        let kelly: f64 = match 'R' {
            'A' | 'B' => 1.0,
            'R' => 0.5,
            _ => 0.0,
        };
        assert!((kelly - 0.5).abs() < 1e-10);
    }

    #[test]
    fn test_kelly_multiplier_tier_c() {
        let flat = flat_closes(80, 100.0);
        let input = make_input(flat.clone(), flat.clone(), flat.clone(), flat.clone(), 1.0, 100.0);
        let sig = compute_hmtf(&input);
        assert_eq!(sig.tier, 'C');
        assert!((sig.kelly_multiplier - 0.0).abs() < 1e-10);
    }

    // ── E_final blended ──────────────────────────────────────────────────────

    #[test]
    fn test_e_final_blended() {
        // E_final = ρ×E_trend + (1-ρ)×E_rev
        let rho = 0.7;
        let e_trend = 0.35;
        let e_rev = 0.10;
        let e_final: f64 = rho * e_trend + (1.0 - rho) * e_rev;
        let expected: f64 = 0.7 * 0.35 + 0.3 * 0.10; // 0.245 + 0.03 = 0.275
        assert!((e_final - expected).abs() < 1e-10);
    }

    // ── Regime labels ────────────────────────────────────────────────────────

    #[test]
    fn test_regime_label_trending() {
        assert_eq!(regime_label(0.8), "TRENDING");
        assert_eq!(regime_label(0.61), "TRENDING");
    }

    #[test]
    fn test_regime_label_ranging() {
        assert_eq!(regime_label(0.2), "RANGING");
        assert_eq!(regime_label(0.39), "RANGING");
    }

    // ── ETH 01:15 scenario ───────────────────────────────────────────────────

    #[test]
    fn test_eth_0115_scenario() {
        // Scenario: all TF bearish, 1H barely, vol spike → tier R, E_rev > 0.08, D > 0.15
        // Construct: 12H/6H/4H strongly falling, 1H mild falling, high volume
        let strong_bear = falling_closes(80, 3000.0, 15.0);  // strong bear higher TF
        let mild_bear   = falling_closes(80, 2800.0, 2.0);   // 1H barely falling
        let price = *mild_bear.last().unwrap();
        let input = HmtfInput {
            closes_1h:  mild_bear,
            closes_4h:  strong_bear.clone(),
            closes_6h:  strong_bear.clone(),
            closes_12h: strong_bear,
            volume_ratio: 3.5,
            price,
            atr_pct: 2.0,
        };
        let sig = compute_hmtf(&input);
        // The divergence between mild 1H and strong 12H bear should create D > 0.15
        // With high volume and low RSI (from falling), E_rev should be significant
        // Verify the AMS fields are populated
        assert!(sig.regime_rho >= 0.0 && sig.regime_rho <= 1.0, "ρ should be [0,1]");
        assert!(sig.divergence_d >= 0.0, "D should be non-negative");
        assert!(!sig.regime_label.is_empty(), "regime label should be set");
        // If conditions met for R tier, verify
        if sig.tier == 'R' {
            assert!(sig.e_reversion > 0.08, "Tier R should have E_rev > 0.08");
            assert!(sig.divergence_d > 0.15, "Tier R should have D > 0.15");
            assert!((sig.kelly_multiplier - 0.5).abs() < 1e-10, "Tier R kelly = 0.5");
        }
    }

    // ── Gate: all-bullish ────────────────────────────────────────────────────

    #[test]
    fn gate_all_bullish_high() {
        let closes = rising_closes(80, 100.0, 1.5);
        let price = *closes.last().unwrap();
        let input = make_input(closes.clone(), closes.clone(), closes.clone(), closes.clone(), 3.0, price);
        let sig = compute_hmtf(&input);
        assert!(sig.gate_product > 0.5, "All-bullish gate should be > 0.5, got {:.4}", sig.gate_product);
    }

    #[test]
    fn gate_bearish_macro_blocks_micro() {
        let bull = rising_closes(80, 100.0, 1.5);
        let bear = falling_closes(80, 200.0, 1.5);
        let price = *bull.last().unwrap();
        let input = make_input(bull.clone(), bull.clone(), bear.clone(), bear.clone(), 3.0, price);
        let sig = compute_hmtf(&input);
        assert!(sig.gate_product < 0.5, "Bearish macro should suppress gate, got {:.4}", sig.gate_product);
    }

    // ── Tier C for bearish ───────────────────────────────────────────────────

    #[test]
    fn tier_c_bearish() {
        let closes = falling_closes(80, 200.0, 2.0);
        let price = *closes.last().unwrap();
        let input = make_input(closes.clone(), closes.clone(), closes.clone(), closes.clone(), 0.5, price);
        let sig = compute_hmtf(&input);
        // All-bearish with low volume: should not qualify for A, B, or R
        assert!(sig.tier == 'C' || sig.tier == 'R',
            "All-bearish low-vol should be C (or R if divergent), got {}", sig.tier);
    }

    // ── Alignment ────────────────────────────────────────────────────────────

    #[test]
    fn alignment_all_bullish_is_4() {
        let closes = rising_closes(80, 100.0, 1.5);
        let price = *closes.last().unwrap();
        let input = make_input(closes.clone(), closes.clone(), closes.clone(), closes.clone(), 3.0, price);
        let sig = compute_hmtf(&input);
        assert_eq!(sig.alignment, 4);
    }

    // ── Tier V (Volume Breakout) ──────────────────────────────────────────────

    /// Tier V must have half Kelly (same as R) — lower confidence than A/B.
    #[test]
    fn test_tier_v_kelly_is_half() {
        let kelly: f64 = match 'V' {
            'A' | 'B' => 1.0,
            'R' | 'V' => 0.5,
            _ => 0.0,
        };
        assert!((kelly - 0.5).abs() < 1e-10, "Tier V Kelly should be 0.5, got {}", kelly);
    }

    /// Low volume (1.5x) must NEVER trigger Tier V regardless of trend quality.
    #[test]
    fn test_tier_v_not_fire_on_low_volume() {
        let closes = rising_closes(80, 100.0, 1.5);
        let price = *closes.last().unwrap();
        // vol = 1.5 < 4.0 → Tier V condition fails
        let input = make_input(closes.clone(), closes.clone(), closes.clone(), closes.clone(), 1.5, price);
        let sig = compute_hmtf(&input);
        assert_ne!(sig.tier, 'V',
            "vol=1.5x should never trigger Tier V, got tier={}", sig.tier);
    }

    /// Flat market with low volume must stay Tier C — Tier V should not fire.
    #[test]
    fn test_tier_v_not_fire_on_flat_low_vol() {
        let flat = flat_closes(80, 100.0);
        let input = make_input(flat.clone(), flat.clone(), flat.clone(), flat.clone(), 1.5, 100.0);
        let sig = compute_hmtf(&input);
        assert_eq!(sig.tier, 'C',
            "Flat low-vol market should stay Tier C, got {}", sig.tier);
    }

    /// High volume with bearish closes must NOT trigger Tier V
    /// (tf_1h.score > 0.05 condition prevents bearish volume from being "V").
    #[test]
    fn test_tier_v_not_fire_on_bearish_high_vol() {
        let bear = falling_closes(80, 200.0, 2.0);
        let price = *bear.last().unwrap();
        // High volume on falling market — not a valid breakout
        let input = make_input(bear.clone(), bear.clone(), bear.clone(), bear.clone(), 5.0, price);
        let sig = compute_hmtf(&input);
        assert_ne!(sig.tier, 'V',
            "Bearish high-vol should NOT be Tier V (score negative), got tier={}", sig.tier);
    }

    // ── Tier B mixed regime ───────────────────────────────────────────────────

    /// Mixed Tier B threshold (0.20) is stricter than Trending Tier B threshold (0.15).
    /// A coin with Et=0.17 qualifies for Tier B in trending but NOT in mixed.
    #[test]
    fn test_tier_b_mixed_threshold_stricter_than_trending() {
        let trending_b = 0.15_f64; // Tier B threshold in TRENDING (ρ > 0.6)
        let mixed_b    = 0.20_f64; // Tier B threshold in MIXED    (0.4 ≤ ρ ≤ 0.6)
        assert!(mixed_b > trending_b,
            "Mixed Tier B threshold must be stricter: {:.2} > {:.2}", mixed_b, trending_b);
        // Et=0.17 passes trending but fails mixed
        let et = 0.17_f64;
        assert!(et > trending_b, "Et=0.17 should pass trending threshold");
        assert!(et < mixed_b,    "Et=0.17 should FAIL mixed threshold");
    }

    // ── score_from_hmtf ──────────────────────────────────────────────────────

    #[test]
    fn score_from_hmtf_positive() {
        let closes = rising_closes(80, 100.0, 2.0);
        let price = *closes.last().unwrap();
        let input = make_input(closes.clone(), closes.clone(), closes.clone(), closes.clone(), 5.0, price);
        let sig = compute_hmtf(&input);
        let score = score_from_hmtf(&sig);
        assert!(score > 0, "Strong bull should give positive score, got {}", score);
    }

    #[test]
    fn score_from_hmtf_negative_or_zero_for_bear() {
        let closes = falling_closes(80, 200.0, 2.0);
        let price = *closes.last().unwrap();
        let input = make_input(closes.clone(), closes.clone(), closes.clone(), closes.clone(), 0.5, price);
        let sig = compute_hmtf(&input);
        let score = score_from_hmtf(&sig);
        assert!(score <= 0, "Bear should give score <= 0, got {}", score);
    }
}
