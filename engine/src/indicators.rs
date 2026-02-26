use crate::models::CoinData;

/// Re-score a coin using the same logic as backtest_v3.py.
/// Returns a composite score in roughly [-100, +100].
///
/// Volume thresholds updated (2026-02-25) based on empirical data:
/// - All historical PAPER_TRADE signals had volume_ratio >= 4.0x
/// - Research (Kamps & Kleinberg 2018): 2x = min for large-cap pump detection
/// - "Early impulse" zone = 3-5x; "surge/late" = 8x+
pub fn compute_score(c: &CoinData) -> i32 {
    let mut score: f64 = 0.0;

    // Price vs EMAs
    if c.price > c.ema20 && c.price > c.ema50 {
        score += 15.0;
    } else if c.price < c.ema20 && c.price < c.ema50 {
        score -= 15.0;
    }

    // Momentum composite
    let mom = c.ret_24h * 0.5 + c.ret_3d * 0.3 + c.ret_7d * 0.2;
    score += mom.clamp(-20.0, 20.0);

    // Volume ratio bonus/penalty
    // Thresholds raised: genuine early pump starts at 3x (large-cap empirical)
    if c.volume_ratio >= 3.0 {
        score += 15.0; // impulse / early pump zone
    } else if c.volume_ratio >= 1.5 {
        score += 5.0; // above average, mild momentum
    } else if c.volume_ratio < 0.8 {
        score -= 10.0; // below average — no conviction
    }

    // RSI extremes
    if c.rsi14 > 70.0 {
        score -= 10.0;
    } else if c.rsi14 < 30.0 {
        score += 10.0; // oversold bounce potential
    }

    // ADX trend strength
    if c.adx14 >= 25.0 {
        score += 15.0;
    } else if c.adx14 >= 18.0 {
        score += 5.0;
    } else {
        score -= 10.0; // no trend
    }

    // ATR volatility filter (prefer 0.35%-2.8%)
    if c.atr14_pct < 0.35 || c.atr14_pct > 2.8 {
        score -= 15.0;
    }

    score.round() as i32
}

/// Assign grade based on score.
pub fn grade_from_score(score: i32) -> char {
    if score >= 20 {
        'A'
    } else if score >= 0 {
        'B'
    } else {
        'C'
    }
}

/// Volume tier classification based on empirical data + Kamps & Kleinberg (2018).
/// "IMPULSE" zone (4-8x) = ideal early pump entry for large-cap coins.
pub fn volume_tier(volume_ratio: f64) -> &'static str {
    if volume_ratio >= 8.0 {
        "SURGE" // late-stage, chase risk high
    } else if volume_ratio >= 4.0 {
        "IMPULSE" // ideal: early impulse, strongest signal
    } else if volume_ratio >= 2.0 {
        "EARLY" // early accumulation, valid entry
    } else if volume_ratio >= 1.0 {
        "NORMAL" // baseline activity
    } else {
        "WEAK" // below average, no conviction
    }
}

/// Confidence level: strong / moderate / weak.
/// Volume thresholds raised to match impulse-zone empirical data.
pub fn confidence_label(score: i32, volume_ratio: f64) -> &'static str {
    if score >= 30 && volume_ratio >= 2.0 {
        "strong"
    } else if score >= 10 && volume_ratio >= 1.0 {
        "moderate"
    } else {
        "weak"
    }
}

/// Whether the coin has sufficient conditions for a trade.
pub fn is_tradeable(c: &CoinData) -> bool {
    let conf = confidence_label(c.score, c.volume_ratio);
    conf != "weak" && c.adx14 >= 20.0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::CoinData;

    fn coin_base() -> CoinData {
        CoinData {
            symbol: "SOLUSDT".to_string(),
            grade: 'A',
            score: 0,
            price: 82.0,
            ema20: 80.0,
            ema50: 75.0,
            rsi14: 55.0,
            adx14: 28.0,
            atr14_pct: 1.2,
            volume_ratio: 5.0,
            ret_24h: 2.5,
            ret_3d: 4.0,
            ret_7d: 6.0,
            range_pos_7d: 0.7,
            regime: "trending".to_string(),
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
        }
    }

    // --- volume_tier tests ---

    #[test]
    fn test_volume_tier_weak() {
        assert_eq!(volume_tier(0.5), "WEAK");
        assert_eq!(volume_tier(0.0), "WEAK");
    }

    #[test]
    fn test_volume_tier_normal() {
        assert_eq!(volume_tier(1.0), "NORMAL");
        assert_eq!(volume_tier(1.8), "NORMAL");
    }

    #[test]
    fn test_volume_tier_early() {
        assert_eq!(volume_tier(2.0), "EARLY"); // boundary
        assert_eq!(volume_tier(3.5), "EARLY");
    }

    #[test]
    fn test_volume_tier_impulse() {
        assert_eq!(volume_tier(4.0), "IMPULSE"); // boundary
        assert_eq!(volume_tier(5.3), "IMPULSE"); // our actual SOL data
        assert_eq!(volume_tier(7.9), "IMPULSE");
    }

    #[test]
    fn test_volume_tier_surge() {
        assert_eq!(volume_tier(8.0), "SURGE"); // boundary
        assert_eq!(volume_tier(12.0), "SURGE");
    }

    // --- confidence_label tests ---

    #[test]
    fn test_confidence_label_strong() {
        // score >= 30 AND vol >= 2.0 → strong
        assert_eq!(confidence_label(35, 2.5), "strong");
        assert_eq!(confidence_label(30, 2.0), "strong"); // boundary
    }

    #[test]
    fn test_confidence_label_strong_fails_low_vol() {
        // score OK but vol < 2.0 → drops to moderate or weak
        assert_ne!(confidence_label(38, 1.5), "strong");
    }

    #[test]
    fn test_confidence_label_moderate() {
        // score >= 10 AND vol >= 1.0 (but not strong)
        assert_eq!(confidence_label(15, 1.2), "moderate");
        assert_eq!(confidence_label(10, 1.0), "moderate"); // boundary
    }

    #[test]
    fn test_confidence_label_weak_low_score() {
        // score < 10 → weak regardless of volume
        assert_eq!(confidence_label(5, 5.0), "weak");
    }

    #[test]
    fn test_confidence_label_weak_low_vol() {
        // score OK but vol < 1.0 → weak
        assert_eq!(confidence_label(40, 0.7), "weak");
    }

    // --- compute_score volume bonus tests ---

    #[test]
    fn test_score_volume_impulse_zone() {
        // vol >= 3.0 → +15 bonus
        let mut c = coin_base();
        c.volume_ratio = 3.5;
        // Build expected score manually (EMA aligned +15, vol +15, ADX +15)
        let s = compute_score(&c);
        assert!(s >= 30, "Expected score >= 30 with vol 3.5x, got {}", s);
    }

    #[test]
    fn test_score_volume_above_avg() {
        // vol in [1.5, 3.0) → +5 bonus only
        let mut c = coin_base();
        c.volume_ratio = 2.0;
        let s_low = compute_score(&c);

        c.volume_ratio = 3.5;
        let s_high = compute_score(&c);

        // impulse zone should score higher than above-avg
        assert!(s_high > s_low, "3.5x vol should score higher than 2.0x vol");
    }

    #[test]
    fn test_score_volume_penalty_weak() {
        // vol < 0.8 → -10 penalty
        let mut c = coin_base();
        c.volume_ratio = 0.5;
        let s_weak = compute_score(&c);

        c.volume_ratio = 5.0;
        let s_strong = compute_score(&c);

        assert!(s_strong > s_weak, "5x vol should score much higher than 0.5x vol");
        let diff = s_strong - s_weak;
        assert!(diff >= 25, "Expected at least 25pt difference (vol penalty -10 vs bonus +15), got {}", diff);
    }

    #[test]
    fn test_score_volume_neutral_range() {
        // vol in [0.8, 1.5) → no bonus, no penalty (0 contribution)
        let mut c = coin_base();
        c.volume_ratio = 1.2; // no bonus
        let s_neutral = compute_score(&c);

        c.volume_ratio = 0.5; // penalty
        let s_penalized = compute_score(&c);

        c.volume_ratio = 3.5; // bonus
        let s_bonus = compute_score(&c);

        assert!(s_bonus > s_neutral, "bonus zone > neutral");
        assert!(s_neutral > s_penalized, "neutral > penalized");
    }
}
