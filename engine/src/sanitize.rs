pub const MIN_VOLUME_RATIO: f64 = 0.05;
pub const MAX_VOLUME_RATIO: f64 = 25.0;
pub const MIN_ADX: f64 = 0.0;
pub const MAX_ADX: f64 = 80.0;
pub const MIN_RETURN_PCT: f64 = -60.0;
pub const MAX_RETURN_PCT: f64 = 60.0;
pub const MIN_SCORE: i32 = -120;
pub const MAX_SCORE: i32 = 120;

pub fn volume_ratio(v: f64) -> f64 {
    finite_or(v, 1.0).clamp(MIN_VOLUME_RATIO, MAX_VOLUME_RATIO)
}

pub fn adx(v: f64) -> f64 {
    finite_or(v, 20.0).clamp(MIN_ADX, MAX_ADX)
}

pub fn ret_pct(v: f64) -> f64 {
    finite_or(v, 0.0).clamp(MIN_RETURN_PCT, MAX_RETURN_PCT)
}

pub fn score(v: i32) -> i32 {
    v.clamp(MIN_SCORE, MAX_SCORE)
}

pub fn score_from_f64(v: f64) -> i32 {
    let clamped = finite_or(v, 0.0)
        .round()
        .clamp(MIN_SCORE as f64, MAX_SCORE as f64);
    clamped as i32
}

fn finite_or(v: f64, fallback: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        fallback
    }
}
