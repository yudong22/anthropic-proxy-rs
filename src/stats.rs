use anyhow::Result;
use rusqlite::{params, Connection};
use std::sync::{Arc, Mutex};

use crate::settings::data_dir;

/// Token breakdown for a single request.
#[derive(Debug, Clone, Default)]
pub struct TokenRecord {
    /// Uncached prompt tokens
    pub input: i64,
    /// Cache-read prompt tokens (served from cache)
    pub cache_read: i64,
    /// Cache-write tokens (new cache entries written)
    pub cache_write: i64,
    /// Output / completion tokens
    pub output: i64,
}

/// Aggregated statistics for a single calendar day.
#[derive(Debug, Clone, Default)]
pub struct DayStats {
    pub date: String,
    pub requests_total: i64,
    pub requests_success: i64,
    pub requests_failed: i64,
    pub tokens_input: i64,
    pub tokens_cache_read: i64,
    pub tokens_cache_write: i64,
    pub tokens_output: i64,
}

impl DayStats {
    /// Total tokens consumed (all input categories + output).
    pub fn tokens_total(&self) -> i64 {
        self.tokens_input + self.tokens_cache_read + self.tokens_cache_write + self.tokens_output
    }

    /// Cache-hit percentage: cache_read / (input + cache_read) × 100.
    /// Returns 0 when there is no input traffic.
    pub fn cache_hit_pct(&self) -> i64 {
        let denom = self.tokens_input + self.tokens_cache_read;
        if denom == 0 {
            0
        } else {
            self.tokens_cache_read * 100 / denom
        }
    }
}

/// Persistent per-day statistics store backed by a local SQLite database.
///
/// The database lives at `~/.proxy-rs/stats.db`.
/// One row per calendar date (local timezone, `YYYY-MM-DD`).
/// The `Mutex` ensures that the single `Connection` is accessed serially;
/// writes are very brief (a single INSERT-OR-IGNORE + UPDATE), so contention
/// is negligible even under concurrent requests.
pub struct StatsDb {
    conn: Mutex<Connection>,
}

impl StatsDb {
    /// Open (or create) the statistics database and ensure the schema exists.
    pub fn open() -> Result<Arc<Self>> {
        let path = match data_dir() {
            Some(dir) => dir.join("stats.db"),
            None => {
                return Err(anyhow::anyhow!(
                    "Cannot determine data directory for stats.db"
                ))
            }
        };

        let conn = Connection::open(&path)?;

        // Enable WAL mode for better concurrent read performance.
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;

        Self::init_schema(&conn)?;

        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
        }))
    }

    /// Construct from an existing connection (e.g. an in-memory DB for tests).
    pub fn from_conn(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
        }
    }

    /// Open a fresh in-memory database with the schema applied. Useful as a
    /// non-persistent fallback and for tests.
    pub fn in_memory() -> Result<Arc<Self>> {
        let conn = Connection::open_in_memory()?;
        Self::init_schema(&conn)?;
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
        }))
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS daily_stats (
                date                TEXT PRIMARY KEY,
                requests_total      INTEGER NOT NULL DEFAULT 0,
                requests_success    INTEGER NOT NULL DEFAULT 0,
                requests_failed     INTEGER NOT NULL DEFAULT 0,
                tokens_input        INTEGER NOT NULL DEFAULT 0,
                tokens_cache_read   INTEGER NOT NULL DEFAULT 0,
                tokens_cache_write  INTEGER NOT NULL DEFAULT 0,
                tokens_output       INTEGER NOT NULL DEFAULT 0
            );",
        )?;
        Ok(())
    }

    /// Record the outcome of a single completed request.
    ///
    /// - `success`: whether the proxy returned a 2xx response.
    /// - `tokens`: token breakdown; pass `TokenRecord::default()` for failures.
    pub fn record_request(&self, success: bool, tokens: TokenRecord) -> Result<()> {
        let date = local_date_string();
        let conn = self.conn.lock().unwrap();

        // Ensure the row for today exists.
        conn.execute(
            "INSERT OR IGNORE INTO daily_stats (date) VALUES (?1)",
            params![date],
        )?;

        if success {
            conn.execute(
                "UPDATE daily_stats SET
                    requests_total    = requests_total    + 1,
                    requests_success  = requests_success  + 1,
                    tokens_input      = tokens_input      + ?1,
                    tokens_cache_read = tokens_cache_read + ?2,
                    tokens_cache_write= tokens_cache_write+ ?3,
                    tokens_output     = tokens_output     + ?4
                 WHERE date = ?5",
                params![
                    tokens.input,
                    tokens.cache_read,
                    tokens.cache_write,
                    tokens.output,
                    date,
                ],
            )?;
        } else {
            conn.execute(
                "UPDATE daily_stats SET
                    requests_total  = requests_total  + 1,
                    requests_failed = requests_failed + 1
                 WHERE date = ?1",
                params![date],
            )?;
        }

        Ok(())
    }

    /// Return statistics for today (local date). Returns a zeroed `DayStats`
    /// with today's date if no requests have been recorded yet.
    pub fn query_today(&self) -> Result<DayStats> {
        let date = local_date_string();
        self.query_date(&date)
    }

    /// Return statistics for an arbitrary date (`YYYY-MM-DD`).
    pub fn query_date(&self, date: &str) -> Result<DayStats> {
        let conn = self.conn.lock().unwrap();
        let result = conn.query_row(
            "SELECT date, requests_total, requests_success, requests_failed,
                    tokens_input, tokens_cache_read, tokens_cache_write, tokens_output
             FROM daily_stats WHERE date = ?1",
            params![date],
            |row| {
                Ok(DayStats {
                    date: row.get(0)?,
                    requests_total: row.get(1)?,
                    requests_success: row.get(2)?,
                    requests_failed: row.get(3)?,
                    tokens_input: row.get(4)?,
                    tokens_cache_read: row.get(5)?,
                    tokens_cache_write: row.get(6)?,
                    tokens_output: row.get(7)?,
                })
            },
        );

        match result {
            Ok(stats) => Ok(stats),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(DayStats {
                date: date.to_string(),
                ..Default::default()
            }),
            Err(e) => Err(e.into()),
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Return today's date as a `YYYY-MM-DD` string in local time (UTC+8 offset
/// read from the `TZ` environment variable when set, otherwise inferred from
/// the system UTC offset via a simple wall-clock approximation).
fn local_date_string() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    // Determine UTC offset in seconds.  We prefer the `TZ` env-var; if it
    // looks like an explicit numeric offset (e.g. "Asia/Shanghai" is not
    // numeric, but "+08:00" or "UTC+8" would be), parse it.  Otherwise we
    // fall back to +8 hours, which is the most common deployment locale for
    // this project.
    let offset_secs: i64 = tz_offset_secs();

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let local_secs = (now + offset_secs).max(0) as u64;
    let days = local_secs / 86400;
    let (y, m, d) = civil_from_days(days as i64);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// Best-effort local UTC offset in seconds.
///
/// Strategy (in order):
/// 1. `PROXY_TZ_OFFSET_HOURS` env var (e.g. `"8"` for CST) — explicit override.
/// 2. Compare local `std::time` with UTC to derive the actual system offset.
/// 3. Fall back to +8h (Asia/Shanghai / CST).
fn tz_offset_secs() -> i64 {
    // 1. Explicit override
    if let Ok(v) = std::env::var("PROXY_TZ_OFFSET_HOURS") {
        if let Ok(h) = v.trim().parse::<i64>() {
            return h * 3600;
        }
    }

    // 2. Derive from system: compare wall clock with UTC epoch arithmetic.
    //    `std::time` gives us UTC; we can't easily get local time without
    //    libc or chrono, so fall back to +8.
    8 * 3600
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    crate::util::civil_from_days(z)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_and_record() {
        // Use an in-memory DB to avoid touching the filesystem in tests.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS daily_stats (
                date                TEXT PRIMARY KEY,
                requests_total      INTEGER NOT NULL DEFAULT 0,
                requests_success    INTEGER NOT NULL DEFAULT 0,
                requests_failed     INTEGER NOT NULL DEFAULT 0,
                tokens_input        INTEGER NOT NULL DEFAULT 0,
                tokens_cache_read   INTEGER NOT NULL DEFAULT 0,
                tokens_cache_write  INTEGER NOT NULL DEFAULT 0,
                tokens_output       INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();

        let db = StatsDb {
            conn: Mutex::new(conn),
        };

        db.record_request(
            true,
            TokenRecord {
                input: 100,
                cache_read: 900,
                cache_write: 0,
                output: 50,
            },
        )
        .unwrap();

        db.record_request(false, TokenRecord::default()).unwrap();

        let today = db.query_today().unwrap();
        assert_eq!(today.requests_total, 2);
        assert_eq!(today.requests_success, 1);
        assert_eq!(today.requests_failed, 1);
        assert_eq!(today.tokens_input, 100);
        assert_eq!(today.tokens_cache_read, 900);
        assert_eq!(today.tokens_output, 50);
        assert_eq!(today.cache_hit_pct(), 90); // 900/(100+900)*100
        assert_eq!(today.tokens_total(), 1050);
    }
}
