use crate::models::*;
use chrono::Local;
use rusqlite::{params, Connection};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

/// Append a simulation result block to the daily journal file and SQLite.
/// Format: `data/trading-journal/YYYY-MM-DD.md` and `data/trading-journal/sim_history.db`
pub fn append_journal(workspace: &Path, result: &SimResult) -> Result<(), String> {
    let date_str = Local::now().format("%Y-%m-%d").to_string();
    let journal_path = workspace.join(format!("data/trading-journal/{}.md", date_str));

    // Create journal file with header if it doesn't exist
    if !journal_path.exists() {
        let dir = journal_path.parent().unwrap();
        fs::create_dir_all(dir).map_err(|e| format!("Failed to create journal dir: {}", e))?;
        let header = format!("# Trading Journal - {}\n\n", date_str);
        fs::write(&journal_path, header).map_err(|e| format!("Failed to create journal: {}", e))?;
    }

    let block = format_journal_block(result);

    let mut file = OpenOptions::new()
        .append(true)
        .open(&journal_path)
        .map_err(|e| format!("Failed to open journal: {}", e))?;

    file.write_all(block.as_bytes())
        .map_err(|e| format!("Failed to write journal: {}", e))?;

    write_sqlite(workspace, &date_str, result, &block)?;

    eprintln!("[JOURNAL] Appended to {}", journal_path.display());
    Ok(())
}

fn format_journal_block(result: &SimResult) -> String {
    let mut block = String::new();

    // Header
    block.push_str(&format!(
        "\n## Hourly Simulation - {} WIB\n",
        result.hour_label
    ));

    // Regime
    block.push_str(&format!(
        "- Market regime saat ini: **{}**\n",
        result.regime
    ));

    // Top 3 watchlist
    block.push_str("- Top 3 watchlist:\n");
    for (i, w) in result.watchlist_top3.iter().enumerate() {
        block.push_str(&format!(
            "  {}. **{}** — Score {}({}), 24h {:+.2}%, 3d {:+.2}%, vol {:.2}x\n",
            i + 1,
            w.symbol,
            w.grade,
            w.score,
            w.ret_24h,
            w.ret_3d,
            w.volume_ratio,
        ));
    }

    block.push_str(&format!(
        "- News catalyst context: source={}, sentiment={:+.2}, catalyst={:.2}, note=\"{}\"\n",
        result.news_context.source,
        result.news_context.sentiment,
        result.news_context.catalyst_score,
        result.news_context.headline
    ));

    block.push_str(&format!(
        "- Probability engine: P(up)={:.3}, P(down)={:.3}, confidence={:.3}, EV={:+.3}R\n",
        result.probability.p_up,
        result.probability.p_down,
        result.probability.confidence,
        result.probability.expected_value_r,
    ));
    block.push_str(&format!(
        "  - Bayes scaffold: prior_up={:.3} -> posterior_up={:.3} (tech_lh={:.3}, news_lh={:.3}, feature={:+.3})\n",
        result.probability.prior_up,
        result.probability.posterior_up,
        result.probability.tech_likelihood,
        result.probability.news_likelihood,
        result.probability.feature_score,
    ));

    // Setup idea
    block.push_str(&format!(
        "- Ide setup simulasi (paper): **{}**\n",
        result.setup_idea
    ));
    block.push_str(&format!("- Quality gate: `{}`\n", result.quality_gate));
    block.push_str("- Checklist trigger entry:\n");
    for item in &result.checklist {
        block.push_str(&format!("  - {}\n", item));
    }

    // Decision details
    match &result.decision {
        Decision::NoTrade { rationale } => {
            block.push_str("- Keputusan: **NO_TRADE**\n");
            block.push_str(&format!("- Alasan: {}\n", rationale));
        }
        Decision::PaperTrade {
            symbol,
            direction,
            entry_zone,
            invalidation,
            tp1,
            tp2,
            risk_pct,
            rationale,
        } => {
            block.push_str(&format!(
                "- Keputusan: **PAPER_TRADE** — {} {}\n",
                symbol, direction
            ));
            block.push_str(&format!("  - Entry zone: {}\n", entry_zone));
            block.push_str(&format!("  - Invalidation/stop: {}\n", invalidation));
            block.push_str(&format!("  - TP1/TP2: {} / {}\n", tp1, tp2));
            block.push_str(&format!("  - Risk: **{:.1}%**\n", risk_pct));
            block.push_str(&format!("- Alasan: {}\n", rationale));
        }
    }

    block
}

fn write_sqlite(
    workspace: &Path,
    trade_date: &str,
    result: &SimResult,
    raw_block: &str,
) -> Result<(), String> {
    let db_path = workspace.join("data/trading-journal/sim_history.db");
    if let Some(parent) = db_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Create sqlite dir: {}", e))?;
    }

    let conn = Connection::open(&db_path).map_err(|e| format!("Open sqlite: {}", e))?;

    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS simulation_events (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          trade_date TEXT NOT NULL,
          trade_time TEXT NOT NULL,
          regime TEXT,
          setup_idea TEXT,
          quality_gate TEXT,
          decision TEXT,
          rationale TEXT,
          checklist_json TEXT,
          raw_block TEXT,
          p_up REAL,
          p_down REAL,
          confidence REAL,
          expected_value_r REAL,
          prior_up REAL,
          posterior_up REAL,
          tech_likelihood REAL,
          news_likelihood REAL,
          news_source TEXT,
          news_sentiment REAL,
          news_catalyst_score REAL,
          inserted_at TEXT NOT NULL,
          UNIQUE(trade_date, trade_time, setup_idea)
        );
        CREATE TABLE IF NOT EXISTS quality_gate_fail_reasons (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          trade_date TEXT NOT NULL,
          trade_time TEXT NOT NULL,
          reason TEXT NOT NULL,
          inserted_at TEXT NOT NULL,
          UNIQUE(trade_date, trade_time, reason)
        );
        ",
    )
    .map_err(|e| format!("Init sqlite schema: {}", e))?;

    // best effort migrations for pre-existing DB
    for alter in [
        "ALTER TABLE simulation_events ADD COLUMN p_up REAL",
        "ALTER TABLE simulation_events ADD COLUMN p_down REAL",
        "ALTER TABLE simulation_events ADD COLUMN confidence REAL",
        "ALTER TABLE simulation_events ADD COLUMN expected_value_r REAL",
        "ALTER TABLE simulation_events ADD COLUMN prior_up REAL",
        "ALTER TABLE simulation_events ADD COLUMN posterior_up REAL",
        "ALTER TABLE simulation_events ADD COLUMN tech_likelihood REAL",
        "ALTER TABLE simulation_events ADD COLUMN news_likelihood REAL",
        "ALTER TABLE simulation_events ADD COLUMN news_source TEXT",
        "ALTER TABLE simulation_events ADD COLUMN news_sentiment REAL",
        "ALTER TABLE simulation_events ADD COLUMN news_catalyst_score REAL",
    ] {
        let _ = conn.execute(alter, []);
    }
    let _ = conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_qg_fail_unique ON quality_gate_fail_reasons(trade_date, trade_time, reason)",
        [],
    );

    let (decision, rationale) = match &result.decision {
        Decision::NoTrade { rationale } => ("NO_TRADE".to_string(), rationale.clone()),
        Decision::PaperTrade { rationale, .. } => ("PAPER_TRADE".to_string(), rationale.clone()),
    };

    let checklist_json = serde_json::to_string(&result.checklist)
        .map_err(|e| format!("Serialize checklist: {}", e))?;
    let inserted_at = Local::now().to_rfc3339();

    conn.execute(
        "INSERT OR REPLACE INTO simulation_events (
            trade_date, trade_time, regime, setup_idea, quality_gate, decision, rationale,
            checklist_json, raw_block,
            p_up, p_down, confidence, expected_value_r, prior_up, posterior_up,
            tech_likelihood, news_likelihood, news_source, news_sentiment, news_catalyst_score,
            inserted_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        params![
            trade_date,
            result.hour_label,
            result.regime,
            result.setup_idea,
            result.quality_gate,
            decision,
            rationale,
            checklist_json,
            raw_block,
            result.probability.p_up,
            result.probability.p_down,
            result.probability.confidence,
            result.probability.expected_value_r,
            result.probability.prior_up,
            result.probability.posterior_up,
            result.probability.tech_likelihood,
            result.probability.news_likelihood,
            result.news_context.source,
            result.news_context.sentiment,
            result.news_context.catalyst_score,
            inserted_at,
        ],
    )
    .map_err(|e| format!("Insert simulation row: {}", e))?;

    if decision == "NO_TRADE" {
        for reason in extract_fail_reasons(&result.checklist) {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO quality_gate_fail_reasons (trade_date, trade_time, reason, inserted_at)
                 VALUES (?, ?, ?, ?)",
                params![trade_date, result.hour_label, reason, Local::now().to_rfc3339()],
            );
        }
    }

    Ok(())
}

fn extract_fail_reasons(checklist: &[String]) -> Vec<String> {
    checklist
        .iter()
        .filter(|item| item.contains('❌'))
        .map(|item| item.split(':').next().unwrap_or(item).trim().to_string())
        .collect()
}
