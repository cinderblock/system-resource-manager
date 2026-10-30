//! Finished leases, kept so slot sizes can be checked against what work really
//! spends.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use srm_proto::{HistoryRow, Lease, Millis, Usage};
use std::path::Path;
use std::sync::Mutex;

pub struct History {
    db: Mutex<Connection>,
}

impl History {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS leases (
                id INTEGER PRIMARY KEY,
                pool TEXT NOT NULL,
                label TEXT NOT NULL,
                repo TEXT,
                session TEXT,
                amount INTEGER NOT NULL,
                requested_at INTEGER NOT NULL,
                granted_at INTEGER NOT NULL,
                released_at INTEGER NOT NULL,
                cpu_seconds REAL NOT NULL,
                peak_cores REAL NOT NULL,
                peak_rss INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS leases_label ON leases (pool, label);",
        )?;
        Ok(Self { db: Mutex::new(db) })
    }

    /// Leases that never started have nothing to say about spend.
    pub fn record(&self, lease: &Lease, usage: &Usage, released_at: Millis) -> Result<()> {
        let Some(granted_at) = lease.granted_at else {
            return Ok(());
        };
        self.db.lock().expect("history lock").execute(
            "INSERT INTO leases (pool, label, repo, session, amount, requested_at, granted_at,
                                 released_at, cpu_seconds, peak_cores, peak_rss)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                lease.pool,
                lease.label,
                lease.repo,
                lease.session,
                lease.amount,
                lease.requested_at,
                granted_at,
                released_at,
                usage.cpu_seconds,
                usage.peak_cores,
                usage.peak_rss_bytes as i64,
            ],
        )?;
        Ok(())
    }

    /// Per label, most recently used first.
    pub fn summary(&self, limit: u32) -> Result<Vec<HistoryRow>> {
        let db = self.db.lock().expect("history lock");
        let mut statement = db.prepare_cached(
            "SELECT pool, label, COUNT(*),
                    AVG(amount),
                    SUM(cpu_seconds) * 1000.0 / MAX(SUM(released_at - granted_at), 1),
                    MAX(peak_cores),
                    MAX(peak_rss),
                    AVG(released_at - granted_at),
                    AVG(granted_at - requested_at),
                    MAX(released_at)
             FROM leases
             GROUP BY pool, label
             ORDER BY MAX(released_at) DESC
             LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            Ok(HistoryRow {
                pool: row.get(0)?,
                label: row.get(1)?,
                runs: row.get::<_, i64>(2)? as u64,
                avg_amount: row.get(3)?,
                avg_cores: row.get(4)?,
                peak_cores: row.get(5)?,
                peak_rss_bytes: row.get::<_, i64>(6)? as u64,
                avg_held_ms: row.get(7)?,
                avg_wait_ms: row.get(8)?,
                last_at: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(label: &str, amount: u32, granted_at: Option<Millis>) -> Lease {
        Lease {
            id: 1,
            pool: "cpu".into(),
            amount,
            label: label.into(),
            pid: 1,
            cwd: None,
            repo: None,
            session: None,
            low: false,
            requested_at: 0,
            granted_at,
            usage: None,
        }
    }

    fn spent(cpu_seconds: f64, peak_cores: f32) -> Usage {
        Usage {
            cpu_seconds,
            peak_cores,
            ..Default::default()
        }
    }

    #[test]
    fn average_cores_weighs_by_time_held() {
        let history = History::open(Path::new(":memory:")).unwrap();
        // 4 cores for 10 s and 1 core for 30 s is 70 core-seconds over 40 s.
        history
            .record(&lease("build", 4, Some(0)), &spent(40.0, 4.0), 10_000)
            .unwrap();
        history
            .record(&lease("build", 4, Some(0)), &spent(30.0, 1.0), 30_000)
            .unwrap();
        let rows = history.summary(10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].runs, 2);
        assert!((rows[0].avg_cores - 1.75).abs() < 1e-9);
        assert_eq!(rows[0].peak_cores, 4.0);
    }

    #[test]
    fn leases_that_never_started_arent_recorded() {
        let history = History::open(Path::new(":memory:")).unwrap();
        history
            .record(&lease("gave up", 1, None), &Usage::default(), 5)
            .unwrap();
        assert!(history.summary(10).unwrap().is_empty());
    }
}
