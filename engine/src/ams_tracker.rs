/// AMS Tracker — EWA-based mode win-rate tracker for adaptive Kelly sizing.
///
/// Tracks recent win rates for trending vs reversion modes using exponentially
/// weighted averages. The C_recent multiplier adjusts Kelly sizing based on
/// whether the mode has been performing above or below its target win rate.

use std::path::Path;

const LAMBDA: f64 = 0.94;
const TARGET_WIN_TREND: f64 = 0.60;
const TARGET_WIN_REVERSION: f64 = 0.45;

#[derive(Debug, Clone)]
pub struct ModeWinRate {
    pub ewa_trend: f64,
    pub ewa_reversion: f64,
    pub trades_trend: u32,
    pub trades_reversion: u32,
}

impl ModeWinRate {
    /// New tracker — EWA starts at target (neutral assumption).
    pub fn new() -> Self {
        Self {
            ewa_trend: TARGET_WIN_TREND,
            ewa_reversion: TARGET_WIN_REVERSION,
            trades_trend: 0,
            trades_reversion: 0,
        }
    }

    /// Update trending EWA with a new trade result.
    pub fn update_trend(&mut self, won: bool) {
        let x = if won { 1.0 } else { 0.0 };
        self.ewa_trend = LAMBDA * self.ewa_trend + (1.0 - LAMBDA) * x;
        self.trades_trend += 1;
    }

    /// Update reversion EWA with a new trade result.
    pub fn update_reversion(&mut self, won: bool) {
        let x = if won { 1.0 } else { 0.0 };
        self.ewa_reversion = LAMBDA * self.ewa_reversion + (1.0 - LAMBDA) * x;
        self.trades_reversion += 1;
    }

    /// C_recent for trending: clamped ratio of actual vs target.
    pub fn c_recent_trend(&self) -> f64 {
        (self.ewa_trend / TARGET_WIN_TREND).clamp(0.5, 1.5)
    }

    /// C_recent for reversion: clamped ratio of actual vs target.
    pub fn c_recent_reversion(&self) -> f64 {
        (self.ewa_reversion / TARGET_WIN_REVERSION).clamp(0.5, 1.5)
    }

    /// Load from DB: read last 30 closed trades and compute EWA.
    pub fn load_from_db(db_path: &Path) -> Result<Self, String> {
        let conn = rusqlite::Connection::open(db_path)
            .map_err(|e| format!("open db: {e}"))?;

        let mut stmt = conn
            .prepare(
                "SELECT tier, pnl_usdt FROM live_positions \
                 WHERE status='closed' ORDER BY closed_at DESC LIMIT 30",
            )
            .map_err(|e| format!("prepare: {e}"))?;

        let rows: Vec<(String, f64)> = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })
            .map_err(|e| format!("query: {e}"))?
            .filter_map(|r| r.ok())
            .collect();

        let mut mwr = ModeWinRate::new();
        // Process oldest-first for correct EWA ordering
        for (tier, pnl) in rows.iter().rev() {
            let won = *pnl > 0.0;
            if tier.contains('R') {
                mwr.update_reversion(won);
            } else if tier.contains('A') || tier.contains('B') {
                mwr.update_trend(won);
            }
        }
        Ok(mwr)
    }

    /// Save snapshot to DB.
    pub fn save_snapshot(db_path: &Path, mwr: &ModeWinRate) -> Result<(), String> {
        let conn = rusqlite::Connection::open(db_path)
            .map_err(|e| format!("open db: {e}"))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS ams_tracker (
                id INTEGER PRIMARY KEY DEFAULT 1,
                ewa_trend REAL NOT NULL,
                ewa_reversion REAL NOT NULL,
                trades_trend INTEGER NOT NULL,
                trades_reversion INTEGER NOT NULL,
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            )",
        )
        .map_err(|e| format!("create table: {e}"))?;

        conn.execute(
            "INSERT OR REPLACE INTO ams_tracker \
             (id, ewa_trend, ewa_reversion, trades_trend, trades_reversion, updated_at) \
             VALUES (1, ?1, ?2, ?3, ?4, datetime('now'))",
            rusqlite::params![
                mwr.ewa_trend,
                mwr.ewa_reversion,
                mwr.trades_trend,
                mwr.trades_reversion,
            ],
        )
        .map_err(|e| format!("upsert: {e}"))?;

        Ok(())
    }
}

/// Compute Kelly multiplier for a given tier, adjusted by C_recent.
pub fn compute_kelly_multiplier(tier: char, mwr: &ModeWinRate) -> f64 {
    match tier {
        'A' | 'B' => 1.0 * mwr.c_recent_trend(),
        'R' => 0.5 * mwr.c_recent_reversion(),
        _ => 0.0,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_starts_at_target() {
        let mwr = ModeWinRate::new();
        assert!((mwr.ewa_trend - TARGET_WIN_TREND).abs() < 1e-10);
        assert!((mwr.ewa_reversion - TARGET_WIN_REVERSION).abs() < 1e-10);
        assert_eq!(mwr.trades_trend, 0);
        assert_eq!(mwr.trades_reversion, 0);
    }

    #[test]
    fn test_c_recent_neutral_at_start() {
        let mwr = ModeWinRate::new();
        assert!((mwr.c_recent_trend() - 1.0).abs() < 1e-10);
        assert!((mwr.c_recent_reversion() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_update_trend_win_increases_ewa() {
        let mut mwr = ModeWinRate::new();
        let before = mwr.ewa_trend;
        mwr.update_trend(true);
        assert!(mwr.ewa_trend > before, "Win should increase EWA");
        assert_eq!(mwr.trades_trend, 1);
    }

    #[test]
    fn test_update_trend_loss_decreases_ewa() {
        let mut mwr = ModeWinRate::new();
        let before = mwr.ewa_trend;
        mwr.update_trend(false);
        assert!(mwr.ewa_trend < before, "Loss should decrease EWA");
    }

    #[test]
    fn test_update_reversion_tracks_separately() {
        let mut mwr = ModeWinRate::new();
        mwr.update_reversion(true);
        mwr.update_reversion(true);
        assert_eq!(mwr.trades_reversion, 2);
        assert_eq!(mwr.trades_trend, 0); // trend unaffected
    }

    #[test]
    fn test_c_recent_clamped_upper() {
        let mut mwr = ModeWinRate::new();
        // Force EWA very high
        for _ in 0..100 {
            mwr.update_trend(true);
        }
        assert!(mwr.c_recent_trend() <= 1.5, "c_recent should be clamped to 1.5");
    }

    #[test]
    fn test_c_recent_clamped_lower() {
        let mut mwr = ModeWinRate::new();
        // Force EWA very low
        for _ in 0..100 {
            mwr.update_trend(false);
        }
        assert!(mwr.c_recent_trend() >= 0.5, "c_recent should be clamped to 0.5");
    }

    #[test]
    fn test_kelly_multiplier_tier_a() {
        let mwr = ModeWinRate::new();
        let k = compute_kelly_multiplier('A', &mwr);
        assert!((k - 1.0).abs() < 1e-10, "Tier A neutral → kelly=1.0");
    }

    #[test]
    fn test_kelly_multiplier_tier_r() {
        let mwr = ModeWinRate::new();
        let k = compute_kelly_multiplier('R', &mwr);
        assert!((k - 0.5).abs() < 1e-10, "Tier R neutral → kelly=0.5");
    }

    #[test]
    fn test_kelly_multiplier_tier_c() {
        let mwr = ModeWinRate::new();
        let k = compute_kelly_multiplier('C', &mwr);
        assert!((k - 0.0).abs() < 1e-10, "Tier C → kelly=0.0");
    }

    #[test]
    fn test_kelly_adjusted_by_c_recent() {
        let mut mwr = ModeWinRate::new();
        // Simulate strong reversion performance
        for _ in 0..20 {
            mwr.update_reversion(true);
        }
        let k = compute_kelly_multiplier('R', &mwr);
        assert!(k > 0.5, "Strong reversion performance should increase kelly above 0.5, got {:.4}", k);
    }
}
