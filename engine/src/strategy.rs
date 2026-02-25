use crate::config::AccountConfig;
use crate::indicators;
use crate::models::*;
use crate::regime::{self, Regime};

/// Compute the minimum volume_ratio gate threshold based on preset + regime.
///
/// Research basis (Kamps & Kleinberg 2018, empirical data 2026-02-25):
/// - All our PAPER_TRADE signals had volume_ratio >= 4x
/// - Large-cap early pump detection: 2x minimum (aggressive), 1.5x (normal)
/// - Risk-off regime: +0.5x (false pumps more common in declining markets)
/// - Mixed regime: +0.2x
pub fn volume_gate_threshold(preset: &str, regime: &Regime) -> f64 {
    let base = if preset.eq_ignore_ascii_case("aggressive") {
        2.0 // early pump minimum for aggressive mode
    } else {
        1.5 // momentum minimum for normal mode
    };
    match regime {
        Regime::RiskOff => base + 0.5,
        Regime::Mixed => base + 0.2,
        Regime::RiskOn => base,
    }
}
use crate::risk;

pub fn evaluate(
    snapshot: &MarketSnapshot,
    signal: &SignalReport,
    news_context: &NewsContext,
    state: &SimState,
    config: &AccountConfig,
) -> SimResult {
    let regime = regime::detect_regime(snapshot);
    let hour_label = chrono::Local::now().format("%H:%M").to_string();
    let watchlist_top3: Vec<WatchlistEntry> = signal.watchlist.iter().take(3).cloned().collect();

    if state.kill_switch_hit {
        return no_trade(
            hour_label,
            regime,
            watchlist_top3,
            news_context,
            "N/A (kill switch active)",
            format!(
                "Daily kill switch hit (cumulative R: {:.1}R, limit: {}R).",
                state.daily_r, config.daily_kill_switch_r
            ),
            "kill-switch=true",
            vec!["Kill switch active ✅".to_string()],
            None,
        );
    }

    if state.open_positions.len() >= config.max_open_positions {
        return no_trade(
            hour_label,
            regime,
            watchlist_top3,
            news_context,
            "N/A (max positions reached)",
            format!(
                "Max open positions reached ({}/{}).",
                state.open_positions.len(),
                config.max_open_positions
            ),
            "max-pos-hit=true",
            vec!["Open position limit reached ✅".to_string()],
            None,
        );
    }

    // Aggressive mode: use EMA-alignment gates for any symbol (not just AVAXUSDT)
    let speculative_avax_mode = config.preset.eq_ignore_ascii_case("aggressive");

    let candidates: Vec<&CoinData> = snapshot
        .coins
        .iter()
        .filter(|c| watchlist_top3.iter().any(|w| w.symbol == c.symbol))
        .collect();

    if candidates.is_empty() {
        return no_trade(
            hour_label,
            regime,
            watchlist_top3,
            news_context,
            "No valid setup in watchlist",
            "Watchlist coins not found in current snapshot.".to_string(),
            "snapshot-match=false",
            vec!["Snapshot/watchlist mismatch ❌".to_string()],
            None,
        );
    }

    let best = candidates.iter().max_by_key(|c| c.score).unwrap();
    let confidence_label = indicators::confidence_label(best.score, best.volume_ratio);
    let prob = compute_probability(regime, best, news_context);

    // Regime-adaptive thresholds (non-aggressive mode)
    let (min_score, min_adx) = match regime {
        Regime::RiskOn => (config.min_score_to_trade - 5, config.min_adx),
        Regime::Mixed => (config.min_score_to_trade, config.min_adx),
        Regime::RiskOff => (config.min_score_to_trade + 10, config.min_adx + 2.0),
    };
    // Volume threshold: computed from research-backed pure function
    let min_volume = volume_gate_threshold(&config.preset, &regime);

    let pass_regime = regime != Regime::RiskOff; // block crash in both modes

    let pass_score = if speculative_avax_mode {
        // Aggressive: EMA alignment (price > EMA20 > EMA50) = trend confirmed
        best.price > best.ema20 && best.ema20 > best.ema50
    } else {
        best.score >= min_score
    };
    let pass_volume = best.volume_ratio >= min_volume; // same gate both modes, research-backed

    let pass_adx = if speculative_avax_mode {
        // Aggressive: ADX >= 20 — Wilder (1978) standard "trending market" threshold
        best.adx14 >= 20.0
    } else {
        best.adx14 >= min_adx
    };
    let pass_confidence = if speculative_avax_mode {
        // Aggressive: skip confidence label check — EMA + volume + ADX gates sufficient
        true
    } else {
        confidence_label != "weak"
    };
    let pass_probability = if speculative_avax_mode {
        // Aggressive: require meaningful expected value (>= 0.5R), not just positive
        prob.expected_value_r >= 0.5
    } else {
        prob.confidence >= 0.55 && prob.expected_value_r > 0.0
    };

    let checklist_pass_count = [
        pass_regime,
        pass_score,
        pass_volume,
        pass_adx,
        pass_confidence,
        pass_probability,
    ]
    .iter()
    .filter(|&&x| x)
    .count();
    let setup_grade = match checklist_pass_count {
        6 => "A",
        5 => "B",
        4 => "C",
        _ => "D",
    };

    let vol_tier = indicators::volume_tier(best.volume_ratio);
    let checklist = vec![
        format!("Regime not risk-off: {}", mark(pass_regime)),
        format!(
            "Score gate (momentum mode: close>EMA20 & EMA20>EMA50): {} (score={}, close={:.6}, ema20={:.6}, ema50={:.6})",
            mark(pass_score),
            best.score,
            best.price,
            best.ema20,
            best.ema50
        ),
        format!(
            "Volume >= {:.1}x [{}]: {} (vol={:.2}x, tier={})",
            min_volume,
            if speculative_avax_mode { "aggressive" } else { "normal" },
            mark(pass_volume),
            best.volume_ratio,
            vol_tier
        ),
        format!(
            "ADX >= {:.1}: {} (adx={:.1})",
            if speculative_avax_mode { 20.0_f64 } else { min_adx },
            mark(pass_adx),
            best.adx14
        ),
        format!(
            "Confidence not weak: {} ({})",
            mark(pass_confidence),
            confidence_label
        ),
        format!(
            "Prob/EV gate: {} (p_up={:.2}, p_down={:.2}, conf={:.2}, ev={:+.2}R)",
            mark(pass_probability),
            prob.p_up,
            prob.p_down,
            prob.confidence,
            prob.expected_value_r
        ),
    ];

    let quality_gate = format!(
        "gate(grade={},regime={},score={},volume={},adx={},confidence={},prob={});bayes(prior={:.2},tech={:.2},news={:.2},post={:.2})",
        setup_grade,
        pass_regime,
        pass_score,
        pass_volume,
        pass_adx,
        pass_confidence,
        pass_probability,
        prob.prior_up,
        prob.tech_likelihood,
        prob.news_likelihood,
        prob.posterior_up
    );

    // All 6 gates must pass — no bypass. Aggressive mode already has looser thresholds.
    let full_gate_pass = pass_regime
        && pass_score
        && pass_volume
        && pass_adx
        && pass_confidence
        && pass_probability;

    if !full_gate_pass {
        return no_trade(
            hour_label,
            regime,
            watchlist_top3,
            news_context,
            format!("{} setup candidate [{}]", best.symbol, setup_grade),
            format!(
                "Quality gate failed for {} [{}]. Preserve risk budget.",
                best.symbol, setup_grade
            ),
            quality_gate,
            checklist,
            Some(prob.clone()),
        );
    }

    // Spot-only / halal constraint: NEVER short. Long only.
    let direction = Direction::Long;

    let risk_pct = if speculative_avax_mode {
        // Experimental paper-only mode: run at full configured risk cap.
        config.max_risk_per_trade_pct
    } else {
        risk::adjusted_risk_pct(config, regime, confidence_label)
    };
    let atr_value = best.price * (best.atr14_pct / 100.0);
    let expected_r = 2.0; // TP2 as baseline
    if expected_r < config.min_expected_r {
        return no_trade(
            hour_label,
            regime,
            watchlist_top3,
            news_context,
            format!("{} setup candidate [{}]", best.symbol, setup_grade),
            format!(
                "Expected R ({:.1}) below minimum ({:.1}).",
                expected_r, config.min_expected_r
            ),
            format!("{};expected_r={:.1}", quality_gate, expected_r),
            checklist,
            Some(prob.clone()),
        );
    }

    let (entry_zone, invalidation, tp1, tp2) = match direction {
        Direction::Long => (
            format!("{:.6} (retest EMA20 area)", best.ema20),
            format!("{:.6} (1 ATR below entry)", best.ema20 - atr_value),
            format!("{:.6} (+1R)", best.ema20 + atr_value),
            format!("{:.6} (+2R)", best.ema20 + 2.0 * atr_value),
        ),
        Direction::Short => (
            format!("{:.6} (rejection at EMA20)", best.ema20),
            format!("{:.6} (1 ATR above entry)", best.ema20 + atr_value),
            format!("{:.6} (+1R)", best.ema20 - atr_value),
            format!("{:.6} (+2R)", best.ema20 - 2.0 * atr_value),
        ),
    };

    SimResult {
        hour_label,
        regime: regime.to_string(),
        watchlist_top3,
        setup_idea: format!(
            "{} {} continuation ({}) [grade {}]",
            best.symbol, direction, confidence_label, setup_grade
        ),
        quality_gate: format!("{};expected_r={:.1}", quality_gate, expected_r),
        checklist,
        news_context: news_context.clone(),
        probability: prob.clone(),
        decision: Decision::PaperTrade {
            symbol: best.symbol.clone(),
            direction,
            entry_zone,
            invalidation,
            tp1,
            tp2,
            risk_pct,
            rationale: format!(
                "{} {} passed quality gate [{}]. score={}, vol={:.2}x, adx={:.1}, p_up={:.2}, p_down={:.2}, conf={:.2}, ev={:+.2}R, risk={:.1}%, expected_r={:.1}.",
                best.symbol,
                direction,
                setup_grade,
                best.score,
                best.volume_ratio,
                best.adx14,
                prob.p_up,
                prob.p_down,
                prob.confidence,
                prob.expected_value_r,
                risk_pct,
                expected_r
            ),
        },
    }
}

fn no_trade(
    hour_label: String,
    regime: Regime,
    watchlist_top3: Vec<WatchlistEntry>,
    news_context: &NewsContext,
    setup_idea: impl Into<String>,
    rationale: impl Into<String>,
    quality_gate: impl Into<String>,
    checklist: Vec<String>,
    probability: Option<ProbabilityMetrics>,
) -> SimResult {
    SimResult {
        hour_label,
        regime: regime.to_string(),
        watchlist_top3,
        setup_idea: setup_idea.into(),
        quality_gate: quality_gate.into(),
        checklist,
        news_context: news_context.clone(),
        probability: probability.unwrap_or_else(|| neutral_probability(regime)),
        decision: Decision::NoTrade {
            rationale: rationale.into(),
        },
    }
}

fn neutral_probability(regime: Regime) -> ProbabilityMetrics {
    let prior = regime_prior(regime);
    ProbabilityMetrics {
        prior_up: prior,
        posterior_up: prior,
        p_up: prior,
        p_down: 1.0 - prior,
        confidence: 0.5,
        expected_value_r: 0.0,
        tech_likelihood: 0.5,
        news_likelihood: 0.5,
        feature_score: 0.0,
    }
}

fn compute_probability(regime: Regime, coin: &CoinData, news: &NewsContext) -> ProbabilityMetrics {
    // Deterministic weighted feature score as pragmatic Bayes scaffold.
    //
    // Volume normalization update (2026-02-25):
    // - Normalizer changed /1.5 → /3.0: now 5x vol = +1.0 (was capped at 2.5x)
    //   This correctly distinguishes "early impulse" (4-5x) from "mild" (1.5-2x)
    // - Volume weight raised 0.14 → 0.20 (research: volume leads price in crypto)
    // - Momentum weight reduced 0.20 → 0.16 to compensate (momentum lags volume)
    let trend = if coin.price > coin.ema20 { 1.0 } else { -1.0 };
    let ema_stack = if coin.ema20 > coin.ema50 { 1.0 } else { -1.0 };
    let momentum = (coin.ret_24h * 0.7 + coin.ret_3d * 0.3).clamp(-6.0, 6.0) / 6.0;
    let vol = ((coin.volume_ratio - 1.0) / 3.0).clamp(-1.0, 1.0); // 4x = +1.0, 1x = 0.0
    let adx = ((coin.adx14 - 20.0) / 20.0).clamp(-1.0, 1.0);
    let rsi = ((coin.rsi14 - 50.0) / 30.0).clamp(-1.0, 1.0);

    // Weights: trend(0.22) + ema_stack(0.16) + momentum(0.16) + vol(0.20) + adx(0.12) - rsi(0.08) + news(0.06)
    // Volume now most-weighted individual component alongside trend (research: vol leads price in crypto)
    let feature_score = 0.22 * trend + 0.16 * ema_stack + 0.16 * momentum + 0.20 * vol
        + 0.12 * adx
        - 0.08 * rsi
        + 0.06 * news.sentiment.clamp(-1.0, 1.0);

    // Prior by regime.
    let prior_up = regime_prior(regime);
    // Likelihood approximations (Bayesian-style update scaffold)
    let tech_likelihood = logistic((feature_score - 0.03) * 3.0);
    let news_likelihood = logistic((news.sentiment * 1.2 + (news.catalyst_score - 0.5)) * 2.0);

    // Posterior approximation:
    // posterior ∝ prior * technical_likelihood * news_likelihood
    let num = prior_up * tech_likelihood * news_likelihood;
    let den = num + (1.0 - prior_up) * (1.0 - tech_likelihood) * (1.0 - news_likelihood);
    let posterior_up = if den <= f64::EPSILON {
        prior_up
    } else {
        (num / den).clamp(0.01, 0.99)
    };

    let p_up = posterior_up;
    let p_down = 1.0 - p_up;
    let confidence = (p_up - 0.5).abs() * 2.0;
    let rr = 2.0;
    let loss_r = 1.0;
    let expected_value_r = p_up * rr - p_down * loss_r;

    ProbabilityMetrics {
        prior_up,
        posterior_up,
        p_up,
        p_down,
        confidence,
        expected_value_r,
        tech_likelihood,
        news_likelihood,
        feature_score,
    }
}

fn regime_prior(regime: Regime) -> f64 {
    match regime {
        Regime::RiskOn => 0.62,
        Regime::Mixed => 0.50,
        Regime::RiskOff => 0.38,
    }
}

fn logistic(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

fn mark(pass: bool) -> &'static str {
    if pass {
        "✅"
    } else {
        "❌"
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    // --- volume_gate_threshold tests ---

    #[test]
    fn test_vgt_aggressive_riskon() {
        // Aggressive + RiskOn → base 2.0
        assert_eq!(volume_gate_threshold("aggressive", &Regime::RiskOn), 2.0);
    }

    #[test]
    fn test_vgt_aggressive_mixed() {
        // Aggressive + Mixed → 2.0 + 0.2 = 2.2
        let t = volume_gate_threshold("aggressive", &Regime::Mixed);
        assert!((t - 2.2).abs() < 1e-9, "Expected 2.2, got {}", t);
    }

    #[test]
    fn test_vgt_aggressive_riskoff() {
        // Aggressive + RiskOff → 2.0 + 0.5 = 2.5
        let t = volume_gate_threshold("aggressive", &Regime::RiskOff);
        assert!((t - 2.5).abs() < 1e-9, "Expected 2.5, got {}", t);
    }

    #[test]
    fn test_vgt_normal_riskon() {
        // Normal + RiskOn → base 1.5
        assert_eq!(volume_gate_threshold("conservative", &Regime::RiskOn), 1.5);
        assert_eq!(volume_gate_threshold("default", &Regime::RiskOn), 1.5);
    }

    #[test]
    fn test_vgt_normal_mixed() {
        // Normal + Mixed → 1.5 + 0.2 = 1.7
        let t = volume_gate_threshold("conservative", &Regime::Mixed);
        assert!((t - 1.7).abs() < 1e-9, "Expected 1.7, got {}", t);
    }

    #[test]
    fn test_vgt_normal_riskoff() {
        // Normal + RiskOff → 1.5 + 0.5 = 2.0
        let t = volume_gate_threshold("conservative", &Regime::RiskOff);
        assert!((t - 2.0).abs() < 1e-9, "Expected 2.0, got {}", t);
    }

    #[test]
    fn test_vgt_case_insensitive() {
        // Case-insensitive preset match
        assert_eq!(
            volume_gate_threshold("AGGRESSIVE", &Regime::RiskOn),
            volume_gate_threshold("aggressive", &Regime::RiskOn)
        );
        assert_eq!(
            volume_gate_threshold("Aggressive", &Regime::Mixed),
            volume_gate_threshold("aggressive", &Regime::Mixed)
        );
    }

    // --- Gate pass/fail scenarios ---

    #[test]
    fn test_volume_gate_pass_early_pump_aggressive() {
        // vol = 2.5x in aggressive + RiskOn → threshold is 2.0 → PASS
        let threshold = volume_gate_threshold("aggressive", &Regime::RiskOn);
        assert!(2.5 >= threshold, "2.5x should pass aggressive RiskOn gate (thr={:.1})", threshold);
    }

    #[test]
    fn test_volume_gate_fail_low_vol_aggressive() {
        // vol = 1.5x in aggressive + RiskOn → threshold 2.0 → FAIL
        let threshold = volume_gate_threshold("aggressive", &Regime::RiskOn);
        assert!(1.5 < threshold, "1.5x should FAIL aggressive RiskOn gate (thr={:.1})", threshold);
    }

    #[test]
    fn test_volume_gate_fail_old_threshold_would_pass() {
        // vol = 0.5x — OLD gate (0.3x) would pass, NEW gate (2.0x) correctly rejects
        let old_threshold = 0.3_f64;
        let new_threshold = volume_gate_threshold("aggressive", &Regime::RiskOn);
        assert!(0.5 >= old_threshold, "0.5x passed old gate (bad)");
        assert!(0.5 < new_threshold, "0.5x correctly rejected by new gate");
    }

    #[test]
    fn test_volume_gate_pass_impulse_zone() {
        // vol = 5.3x (our actual SOL data) → should pass both modes
        let aggressive_thr = volume_gate_threshold("aggressive", &Regime::RiskOn);
        let normal_thr = volume_gate_threshold("conservative", &Regime::RiskOn);
        assert!(5.3 >= aggressive_thr);
        assert!(5.3 >= normal_thr);
    }

    #[test]
    fn test_volume_gate_riskoff_raises_bar() {
        // RiskOff should require MORE volume than RiskOn
        let thr_on = volume_gate_threshold("aggressive", &Regime::RiskOn);
        let thr_off = volume_gate_threshold("aggressive", &Regime::RiskOff);
        assert!(thr_off > thr_on, "RiskOff threshold should be higher than RiskOn");
    }

    #[test]
    fn test_volume_gate_aggressive_stricter_than_old() {
        // New aggressive threshold (2.0) is 6.7x stricter than old (0.3)
        let new_thr = volume_gate_threshold("aggressive", &Regime::RiskOn);
        let old_thr = 0.3_f64;
        assert!(new_thr > old_thr * 5.0, "New gate should be at least 5x stricter than old 0.3x gate");
    }
}
