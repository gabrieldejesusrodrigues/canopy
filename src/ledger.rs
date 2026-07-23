//! Economics ledger. Always local SQLite (even with the Linear tracker):
//! costs are harness-side data. Powers the article-style report — share of
//! tokens vs share of cost per role and per model.

use std::path::Path;
use std::sync::Mutex;

use anyhow::Result;
use rusqlite::Connection;

use crate::model::InvocationRecord;

pub struct Ledger {
    conn: Mutex<Connection>,
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Ledger> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS invocations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL,
                node_id TEXT NOT NULL,
                role TEXT NOT NULL,
                cli TEXT NOT NULL,
                model TEXT NOT NULL,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                cached_tokens INTEGER NOT NULL,
                cost_usd REAL,
                duration_ms INTEGER NOT NULL,
                attempt INTEGER NOT NULL,
                exit_ok INTEGER NOT NULL,
                created_at TEXT NOT NULL DEFAULT (datetime('now'))
            );",
        )?;
        Ok(Ledger {
            conn: Mutex::new(conn),
        })
    }

    pub fn record(&self, run_id: &str, rec: &InvocationRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO invocations
             (run_id, node_id, role, cli, model, input_tokens, output_tokens,
              cached_tokens, cost_usd, duration_ms, attempt, exit_ok)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            rusqlite::params![
                run_id,
                rec.node_id,
                rec.role.as_str(),
                rec.cli.as_str(),
                rec.model,
                rec.input_tokens as i64,
                rec.output_tokens as i64,
                rec.cached_tokens as i64,
                rec.cost_usd,
                rec.duration_ms as i64,
                rec.attempt as i64,
                rec.exit_ok as i64,
            ],
        )?;
        Ok(())
    }

    /// Total priced spend for the run (unpriced rows count as zero — agy
    /// reports nothing; the report marks those rows explicitly).
    pub fn total_cost(&self, run_id: &str) -> Result<f64> {
        let conn = self.conn.lock().unwrap();
        let v: f64 = conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM invocations WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        )?;
        Ok(v)
    }

    pub fn report(&self, run_id: &str) -> Result<String> {
        let conn = self.conn.lock().unwrap();
        let mut out = String::new();
        let total: f64 = conn.query_row(
            "SELECT COALESCE(SUM(cost_usd),0) FROM invocations WHERE run_id=?1",
            [run_id],
            |r| r.get(0),
        )?;
        let total_tokens: i64 = conn.query_row(
            "SELECT COALESCE(SUM(input_tokens+output_tokens),0) FROM invocations WHERE run_id=?1",
            [run_id],
            |r| r.get(0),
        )?;

        for (label, group) in [("by role", "role"), ("by model", "cli || ':' || model")] {
            out.push_str(&format!("\n== Spend {label} ==\n"));
            out.push_str(&format!(
                "{:<34} {:>6} {:>12} {:>8} {:>10} {:>7} {:>8} {:>8}\n",
                "group", "calls", "tokens", "tok %", "cost USD", "cost %", "agt min", "unpriced"
            ));
            let sql = format!(
                "SELECT {group} AS g, COUNT(*), SUM(input_tokens+output_tokens),
                        COALESCE(SUM(cost_usd),0), SUM(cost_usd IS NULL), SUM(duration_ms)
                 FROM invocations WHERE run_id=?1 GROUP BY g ORDER BY 4 DESC"
            );
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([run_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, f64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            })?;
            for row in rows {
                let (g, calls, toks, cost, unpriced, dur_ms) = row?;
                out.push_str(&format!(
                    "{:<34} {:>6} {:>12} {:>7.1}% {:>10.4} {:>6.1}% {:>8.1} {:>8}\n",
                    g,
                    calls,
                    toks,
                    pct(toks as f64, total_tokens as f64),
                    cost,
                    pct(cost, total),
                    dur_ms as f64 / 60_000.0,
                    if unpriced > 0 {
                        unpriced.to_string()
                    } else {
                        "-".into()
                    },
                ));
            }
        }
        out.push_str(&format!(
            "\nTOTAL: {} tokens, ${:.4} priced spend\n",
            total_tokens, total
        ));
        out.push_str(
            "The article's shape to look for: leaves own most tokens, trunks own most cost.\n",
        );
        Ok(out)
    }
}

fn pct(part: f64, whole: f64) -> f64 {
    if whole <= 0.0 {
        0.0
    } else {
        100.0 * part / whole
    }
}

/// Price a usage that lacks CLI-reported cost, from the config table.
pub fn price(
    usage: &crate::agent::Usage,
    model: &str,
    pricing: &std::collections::HashMap<String, crate::config::Price>,
) -> Option<f64> {
    usage.cost_usd.or_else(|| {
        pricing.get(model).map(|p| {
            (usage.input_tokens as f64 * p.input + usage.output_tokens as f64 * p.output) / 1e6
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CliKind, Role};

    #[test]
    fn record_and_report() {
        let dir = tempfile::tempdir().unwrap();
        let l = Ledger::open(&dir.path().join("ledger.db")).unwrap();
        let rec = |role: Role, cost: Option<f64>, out_tok: u64| InvocationRecord {
            node_id: "n1".into(),
            role,
            cli: CliKind::Stub,
            model: "m".into(),
            input_tokens: 1000,
            output_tokens: out_tok,
            cached_tokens: 0,
            cost_usd: cost,
            duration_ms: 10,
            attempt: 1,
            exit_ok: true,
        };
        l.record("r1", &rec(Role::Planner, Some(1.0), 500)).unwrap();
        l.record("r1", &rec(Role::Executor, Some(0.1), 9000))
            .unwrap();
        l.record("r1", &rec(Role::Executor, None, 9000)).unwrap();
        assert!((l.total_cost("r1").unwrap() - 1.1).abs() < 1e-9);
        let report = l.report("r1").unwrap();
        assert!(report.contains("planner"));
        assert!(report.contains("TOTAL"));
    }
}
