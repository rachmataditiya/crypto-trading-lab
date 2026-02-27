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

    // ── Two-pass candidate selection ────────────────────────────────────────
    // Pre-filter: exclude symbols that are already held in live positions.
    //   Prevents infinite SKIPPED_DUPE loops when a Tier A/B coin is open —
    //   sim engine will now look for the *next best* opportunity instead.
    let open_symbols: std::collections::HashSet<&str> = state
        .open_positions
        .iter()
        .map(|p| p.symbol.as_str())
        .collect();

    // Pass 1 (AMS-first): any coin across full snapshot with AMS Tier A/B/R,
    //   NOT already held, ranked by AMS entry score.
    // Pass 2 (Bayesian fallback): if no AMS-qualified coin, fall back to the
    //   traditional watchlist top-3 ranked by Bayesian score.
    let ams_candidates: Vec<&CoinData> = snapshot
        .coins
        .iter()
        .filter(|c| !open_symbols.contains(c.symbol.as_str()))
        .filter(|c| matches!(c.ams_tier, 'A' | 'B' | 'R' | 'V'))
        .collect();

    let (candidates, ams_selected): (Vec<&CoinData>, bool) = if !ams_candidates.is_empty() {
        (ams_candidates, true)
    } else {
        let bayesian: Vec<&CoinData> = snapshot
            .coins
            .iter()
            .filter(|c| !open_symbols.contains(c.symbol.as_str()))
            .filter(|c| watchlist_top3.iter().any(|w| w.symbol == c.symbol))
            .collect();
        (bayesian, false)
    };

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

    // Pick best: AMS path → rank by entry score; Bayesian path → rank by score
    let best = if ams_selected {
        candidates.iter()
            .max_by(|a, b| a.hmtf_entry_score.partial_cmp(&b.hmtf_entry_score)
                .unwrap_or(std::cmp::Ordering::Equal))
            .unwrap()
    } else {
        candidates.iter().max_by_key(|c| c.score).unwrap()
    };
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

    // ── HMTF / AMS integration ──────────────────────────────────────────────
    // Use AMS tier when available (not C), else fall back to HMTF tier
    let effective_tier = if best.ams_tier != 'C' { best.ams_tier } else { best.hmtf_tier };
    let hmtf_active = best.hmtf_entry_score != 0.0 || best.ams_tier != 'C';
    let hmtf_gate_ok = !hmtf_active || effective_tier != 'C';
    let hmtf_gate_val = match effective_tier {
        'A' => 0.8,
        'B' => 0.5,
        'R' => 0.6,
        'V' => 0.4, // volume breakout — lower gate value, half Kelly
        _ => 0.1,
    };
    let _ = hmtf_gate_val;

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

    // Macro EMA alignment: price > EMA20 > EMA50 (bull stack).
    // Prevents entering LONG on a 1H bounce inside a macro downtrend where all 4H EMAs are DOWN.
    // Root cause of 2026-02-27 losses: system generated Tier A signals (DOT×2, SOL×1) while
    // most coins had EMA=DOWN on realtime cache (confirmed 4H candles).
    // This gate is UNCONDITIONAL — both modes must pass it. EMA stack is deterministic,
    // not confidence-weighted: either the macro trend supports the trade or it doesn't.
    let pass_macro_ema = best.price > best.ema20 && best.ema20 > best.ema50;

    let checklist_pass_count = [
        pass_regime,
        pass_score,
        pass_volume,
        pass_adx,
        pass_confidence,
        pass_probability,
        hmtf_gate_ok,
        pass_macro_ema,
    ]
    .iter()
    .filter(|&&x| x)
    .count();
    let setup_grade = match checklist_pass_count {
        8 => "A",
        7 => "B",
        6 => "C",
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
        format!(
            "HMTF/AMS gate (tier!=C): {} (ams={}, hmtf={}, E={:.4}, align={}, ρ={:.2}, div={:.3})",
            mark(hmtf_gate_ok),
            effective_tier,
            best.hmtf_tier,
            best.hmtf_entry_score,
            best.hmtf_alignment,
            best.ams_regime_rho,
            best.ams_divergence_d,
        ),
        format!(
            "Macro EMA aligned (price>EMA20>EMA50): {} (price={:.4}, ema20={:.4}, ema50={:.4})",
            mark(pass_macro_ema),
            best.price,
            best.ema20,
            best.ema50,
        ),
    ];

    let quality_gate = format!(
        "gate(grade={},regime={},score={},volume={},adx={},confidence={},prob={},macro_ema={});bayes(prior={:.2},tech={:.2},news={:.2},post={:.2});ams={},ams_regime={:.2},ams_div={:.3},sel={}",
        setup_grade,
        pass_regime,
        pass_score,
        pass_volume,
        pass_adx,
        pass_confidence,
        pass_probability,
        pass_macro_ema,
        prob.prior_up,
        prob.tech_likelihood,
        prob.news_likelihood,
        prob.posterior_up,
        effective_tier,
        best.ams_regime_rho,
        best.ams_divergence_d,
        if ams_selected { "ams" } else { "bayes" },
    );

    // All 8 gates must pass — no bypass. Aggressive mode already has looser thresholds.
    let full_gate_pass = pass_regime
        && pass_score
        && pass_volume
        && pass_adx
        && pass_confidence
        && pass_probability
        && hmtf_gate_ok
        && pass_macro_ema;

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

    // ── Tier V late-entry guard ──────────────────────────────────────────────
    // Volume breakouts (Tier V) are only worth entering EARLY — close to EMA20.
    // If price has already run > 2.5% above EMA20 at signal time, the impulse is
    // exhausted and reward/risk is unfavorable (entering too late in the candle).
    // Tier A/B have multi-TF confirmation that justifies higher EMA20 deviation.
    if effective_tier == 'V' && best.ema20 > 0.0 {
        let pct_above_ema = (best.price - best.ema20) / best.ema20 * 100.0;
        if pct_above_ema > 2.5 {
            return no_trade(
                hour_label,
                regime,
                watchlist_top3,
                news_context,
                format!("{} setup candidate [Tier V late]", best.symbol),
                format!(
                    "Tier V late entry blocked: {:.1}% above EMA20 (max 2.5%). \
                     Volume impulse likely exhausted.",
                    pct_above_ema
                ),
                format!("{};tier_v_late={:.1}%", quality_gate, pct_above_ema),
                checklist,
                Some(prob.clone()),
            );
        }
    }

    let base_risk = if speculative_avax_mode {
        config.max_risk_per_trade_pct
    } else {
        risk::adjusted_risk_pct(config, regime, confidence_label)
    };
    // Apply AMS kelly multiplier for R/V trades (half Kelly)
    let risk_pct = if matches!(effective_tier, 'R' | 'V') && best.ams_kelly_multiplier > 0.0 {
        base_risk * best.ams_kelly_multiplier
    } else {
        base_risk
    };
    let atr_value = best.price * (best.atr14_pct / 100.0);
    // Tier R/V: conservative EV (win_rate=0.45, reward=1.2R)
    // Tier V is a 1H volume breakout — lower multi-TF confidence → same treatment as R.
    // min_expected_r check uses 1.0 floor for R/V (half Kelly already accounts for risk;
    // applying the full A/B min_expected_r threshold would always block them).
    let expected_r = if matches!(effective_tier, 'R' | 'V') { 1.2 } else { 2.0 };
    let min_er = if matches!(effective_tier, 'R' | 'V') { 1.0 } else { config.min_expected_r };
    if expected_r < min_er {
        return no_trade(
            hour_label,
            regime,
            watchlist_top3,
            news_context,
            format!("{} setup candidate [{}]", best.symbol, setup_grade),
            format!(
                "Expected R ({:.1}) below minimum ({:.1}).",
                expected_r, min_er
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
                "{} {} passed quality gate [{}]. score={}, vol={:.2}x, adx={:.1}, p_up={:.2}, p_down={:.2}, conf={:.2}, ev={:+.2}R, risk={:.1}%, expected_r={:.1}. HMTF: tier={}, E={:.4}, align={}. AMS: ams={}, ρ={:.2}, div={:.3}, kelly={:.2}.",
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
                expected_r,
                best.hmtf_tier,
                best.hmtf_entry_score,
                best.hmtf_alignment,
                effective_tier,
                best.ams_regime_rho,
                best.ams_divergence_d,
                best.ams_kelly_multiplier,
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

    // ===========================================================
    // AMS-FIRST SELECTION — REGRESSION TESTS
    // These tests encode the specific architectural bug fixed on 2026-02-27:
    // strategy.rs pre-filtered candidates to watchlist_top3 (BTC/ETH/SOL),
    // so DOT (AMS Tier B) was never evaluated even when BTC/ETH/SOL were Tier C.
    // ===========================================================

    fn make_ams_coin(symbol: &str, grade: char, score: i32, ams_tier: char, hmtf_score: f64, vol: f64) -> CoinData {
        CoinData {
            symbol: symbol.to_string(),
            grade,
            score,
            price: 100.0,
            ret_24h: 3.0,
            ret_3d: 4.0,
            ret_7d: 6.0,
            ema20: 95.0,
            ema50: 88.0,
            rsi14: 55.0,
            atr14_pct: 2.0,
            adx14: 30.0,
            range_pos_7d: 0.65,
            volume_ratio: vol,
            regime: "trending".to_string(),
            hmtf_entry_score: hmtf_score,
            hmtf_alignment: 3,
            hmtf_tier: ams_tier,
            ams_regime_rho: 0.65,
            ams_regime_label: "TRENDING".to_string(),
            ams_divergence_d: 0.1,
            ams_e_trend: hmtf_score,
            ams_e_reversion: 0.2,
            ams_tier,
            ams_kelly_multiplier: 1.0,
        }
    }

    fn make_ams_snapshot(coins: Vec<CoinData>) -> MarketSnapshot {
        MarketSnapshot {
            timestamp: "2026-02-27T00:00:00Z".to_string(),
            market_regime: "RISK_ON".to_string(),
            breadth_above_ema50: 14,
            breadth_total: 19,
            coins,
            watchlist: vec![],
        }
    }

    fn make_ams_signal(top3: &[&str]) -> SignalReport {
        SignalReport {
            market_mode: "normal".to_string(),
            btc_eth_alert: "none".to_string(),
            watchlist: top3.iter().enumerate().map(|(i, s)| WatchlistEntry {
                symbol: s.to_string(),
                grade: 'B',
                score: 80 - i as i32 * 5,
                ret_24h: 2.0,
                ret_3d: 3.0,
                volume_ratio: 3.0,
            }).collect(),
        }
    }

    fn make_ams_config() -> AccountConfig {
        use crate::config::RealtimeConfig;
        AccountConfig {
            mode: "paper".to_string(),
            execution: "simulate".to_string(),
            base_currency: "USDT".to_string(),
            initial_capital: 100.0,
            max_risk_per_trade_pct: 2.0,
            max_open_positions: 3,
            daily_kill_switch_r: 3.0,
            min_volume_ratio: 1.5,
            min_score_to_trade: 60,
            min_adx: 25.0,
            min_expected_r: 1.0,
            preset: "normal".to_string(),
            realtime: RealtimeConfig::default(),
        }
    }

    /// Regression: DOT Tier B (outside watchlist top-3) must be selected
    /// over BTC/ETH/SOL Tier C (inside watchlist top-3).
    /// This is the specific AMS-selection bug fixed on 2026-02-27.
    #[test]
    fn test_ams_first_selects_tier_b_outside_watchlist() {
        let coins = vec![
            make_ams_coin("BTCUSDT", 'B', 80, 'C', 0.1, 2.0),
            make_ams_coin("ETHUSDT", 'B', 75, 'C', 0.1, 2.0),
            make_ams_coin("SOLUSDT", 'B', 70, 'C', 0.1, 2.0),
            make_ams_coin("DOTUSDT", 'B', 65, 'B', 0.7, 5.5), // Tier B — NOT in top-3
        ];
        let snapshot = make_ams_snapshot(coins);
        let signal = make_ams_signal(&["BTCUSDT", "ETHUSDT", "SOLUSDT"]);
        let state = SimState::new();
        let config = make_ams_config();
        let news = NewsContext::default();

        let result = evaluate(&snapshot, &signal, &news, &state, &config);

        // AMS-first path must evaluate DOTUSDT, not BTC/ETH/SOL
        let dot_evaluated = result.setup_idea.contains("DOTUSDT")
            || result.quality_gate.contains("DOTUSDT");
        assert!(
            dot_evaluated,
            "AMS Tier B (DOTUSDT) should be selected over Tier C top-3.\n\
             setup_idea='{}'\nquality_gate='{}'",
            result.setup_idea,
            result.quality_gate
        );
        // Confirm sel=ams is logged
        assert!(
            result.quality_gate.contains("sel=ams"),
            "quality_gate should log sel=ams when AMS path used: '{}'",
            result.quality_gate
        );
    }

    /// Tier A coin must beat Tier B coin (ranked by hmtf_entry_score).
    #[test]
    fn test_ams_tier_a_beats_tier_b_by_entry_score() {
        let coins = vec![
            make_ams_coin("BTCUSDT", 'B', 80, 'C', 0.1, 2.0),
            make_ams_coin("SOLUSDT", 'B', 65, 'B', 0.5, 4.0), // Tier B
            make_ams_coin("DOTUSDT", 'B', 60, 'A', 0.9, 5.0), // Tier A, higher score → wins
        ];
        let snapshot = make_ams_snapshot(coins);
        let signal = make_ams_signal(&["BTCUSDT", "ETHUSDT", "SOLUSDT"]);
        let state = SimState::new();
        let config = make_ams_config();
        let news = NewsContext::default();

        let result = evaluate(&snapshot, &signal, &news, &state, &config);

        // DOTUSDT (Tier A, score=0.9) must beat SOLUSDT (Tier B, score=0.5)
        let dot_evaluated = result.setup_idea.contains("DOTUSDT")
            || result.quality_gate.contains("ams=A");
        assert!(
            dot_evaluated,
            "Tier A (DOT) must beat Tier B (SOL) by entry score.\n\
             setup_idea='{}'\nquality_gate='{}'",
            result.setup_idea,
            result.quality_gate
        );
    }

    /// Regression: already-open coin must be excluded from AMS candidates.
    /// Fix for the SKIPPED_DUPE loop where sim kept generating signals for
    /// a coin already held in a live position (DOT bug 2026-02-27 session 2).
    #[test]
    fn test_already_open_coin_excluded_from_ams() {
        let coins = vec![
            make_ams_coin("BTCUSDT", 'B', 80, 'C', 0.1, 2.0),
            make_ams_coin("ETHUSDT", 'B', 75, 'C', 0.1, 2.0),
            make_ams_coin("SOLUSDT", 'B', 70, 'C', 0.1, 2.0),
            make_ams_coin("DOTUSDT", 'B', 65, 'B', 0.7, 5.5), // Tier B — but already open
        ];
        let snapshot = make_ams_snapshot(coins);
        let signal = make_ams_signal(&["BTCUSDT", "ETHUSDT", "SOLUSDT"]);
        // Simulate DOT already held in live position
        let mut state = SimState::new();
        state.open_positions.push(PaperPosition {
            symbol: "DOTUSDT".to_string(),
            direction: Direction::Long,
            entry_price: 1.60,
            stop_price: 1.45,
            tp_price: 1.80,
            size_usdt: 24.0,
            opened_at: chrono::Utc::now(),
        });
        let config = make_ams_config();
        let news = NewsContext::default();

        let result = evaluate(&snapshot, &signal, &news, &state, &config);

        // DOT must NOT appear in result — it's already open
        assert!(
            !result.setup_idea.contains("DOTUSDT"),
            "Already-open DOTUSDT must be excluded from candidates.\n\
             setup_idea='{}'",
            result.setup_idea
        );
        assert!(
            !result.quality_gate.contains("DOTUSDT"),
            "quality_gate must not reference DOTUSDT: '{}'",
            result.quality_gate
        );
    }

    // ===========================================================
    // MACRO EMA ALIGNMENT GATE — REGRESSION TESTS
    // Root cause of 2026-02-27 losses: system entered LONG on 1H bounce
    // while macro EMA (price vs EMA20 vs EMA50) was DOWN for DOT and SOL.
    // The 8th gate now enforces: price > EMA20 > EMA50 (bull stack) required.
    // ===========================================================

    fn make_ema_down_coin(symbol: &str) -> CoinData {
        // EMA stack DOWN: price < EMA20 < EMA50 (macro bearish)
        CoinData {
            symbol: symbol.to_string(),
            grade: 'A',
            score: 85,
            price: 82.0,   // below EMA20
            ema20: 87.0,   // below EMA50
            ema50: 92.0,   // highest — bearish stack
            ret_24h: 4.0,
            ret_3d: 5.0,
            ret_7d: 7.0,
            rsi14: 55.0,
            atr14_pct: 2.0,
            adx14: 35.0,
            range_pos_7d: 0.7,
            volume_ratio: 5.5,
            regime: "trending".to_string(),
            hmtf_entry_score: 0.85,
            hmtf_alignment: 4,
            hmtf_tier: 'A',
            ams_regime_rho: 0.72,
            ams_regime_label: "TRENDING".to_string(),
            ams_divergence_d: 0.15,
            ams_e_trend: 0.85,
            ams_e_reversion: 0.1,
            ams_tier: 'A',
            ams_kelly_multiplier: 1.0,
        }
    }

    /// Regression: Tier A coin with macro EMA DOWN (price<EMA20<EMA50) must be blocked.
    /// This is the specific pattern from 2026-02-27: DOT at 1.614 (below EMA20),
    /// SOL at 87.42 (below EMA20) — both hit CLOSED_TIME losses.
    #[test]
    fn test_macro_ema_down_blocks_tier_a_entry() {
        let coins = vec![make_ema_down_coin("SOLUSDT")];
        let snapshot = make_ams_snapshot(coins);
        let signal = make_ams_signal(&["SOLUSDT", "BTCUSDT", "ETHUSDT"]);
        let state = SimState::new();
        let config = make_ams_config();
        let news = NewsContext::default();

        let result = evaluate(&snapshot, &signal, &news, &state, &config);

        // Must be NoTrade — macro EMA gate blocks the entry
        let is_no_trade = matches!(result.decision, Decision::NoTrade { .. });
        assert!(
            is_no_trade,
            "Tier A coin with EMA DOWN must be blocked by macro EMA gate.\n\
             decision={:?}\nquality_gate='{}'",
            result.decision,
            result.quality_gate
        );
        // Confirm macro_ema=false is logged
        assert!(
            result.quality_gate.contains("macro_ema=false"),
            "quality_gate must log macro_ema=false when EMA stack is DOWN: '{}'",
            result.quality_gate
        );
    }

    /// Sanity: Tier A coin with macro EMA UP (price>EMA20>EMA50) should NOT be blocked.
    /// Verifies the gate doesn't break existing bull-trend entries.
    #[test]
    fn test_macro_ema_up_does_not_block_entry() {
        // make_ams_coin sets price=100, ema20=95, ema50=88 → EMA UP
        let coins = vec![make_ams_coin("SOLUSDT", 'A', 85, 'A', 0.85, 5.5)];
        let snapshot = make_ams_snapshot(coins);
        let signal = make_ams_signal(&["SOLUSDT", "BTCUSDT", "ETHUSDT"]);
        let state = SimState::new();
        let config = make_ams_config();
        let news = NewsContext::default();

        let result = evaluate(&snapshot, &signal, &news, &state, &config);

        // macro_ema=true must be in quality_gate (gate is logged regardless of pass/fail)
        assert!(
            result.quality_gate.contains("macro_ema=true"),
            "quality_gate must log macro_ema=true when EMA stack is UP: '{}'",
            result.quality_gate
        );
    }

    /// When no AMS-qualified coin exists, must fall back to Bayesian top-3 (sel=bayes).
    #[test]
    fn test_ams_fallback_to_bayes_when_all_tier_c() {
        let coins = vec![
            make_ams_coin("BTCUSDT", 'B', 80, 'C', 0.1, 2.0),
            make_ams_coin("ETHUSDT", 'B', 75, 'C', 0.1, 2.0),
            make_ams_coin("SOLUSDT", 'B', 65, 'C', 0.1, 2.0),
        ];
        let snapshot = make_ams_snapshot(coins);
        let signal = make_ams_signal(&["BTCUSDT", "ETHUSDT", "SOLUSDT"]);
        let state = SimState::new();
        let config = make_ams_config();
        let news = NewsContext::default();

        let result = evaluate(&snapshot, &signal, &news, &state, &config);

        assert!(
            result.quality_gate.contains("sel=bayes"),
            "Should use Bayesian fallback when no AMS-qualified coin.\n\
             quality_gate='{}'",
            result.quality_gate
        );
    }
}
