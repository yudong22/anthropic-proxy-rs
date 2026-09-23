use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

/// Everything captured about one completed request for the stats DB.
#[derive(Debug, Clone)]
pub struct RequestOutcome<'a> {
    /// Client-facing model name (before upstream mapping).
    pub model: &'a str,
    /// Route that served the request, e.g. `/v1/messages`.
    pub route: &'a str,
    pub tokens: &'a TokenRecord,
    pub duration_ms: i64,
    pub streamed: bool,
    /// HTTP status returned to the client.
    pub status: u16,
    /// Error message when the request failed.
    pub error: Option<&'a str>,
}

impl RequestOutcome<'_> {
    /// Freeze into an owned row that can cross the writer-thread channel.
    fn to_row(&self, date: &str, created_at: &str) -> RequestRow {
        RequestRow {
            date: date.to_string(),
            created_at: created_at.to_string(),
            model: self.model.to_string(),
            route: self.route.to_string(),
            input_tokens: self.tokens.input,
            output_tokens: self.tokens.output,
            cache_read_tokens: self.tokens.cache_read,
            cache_write_tokens: self.tokens.cache_write,
            duration_ms: self.duration_ms,
            streamed: self.streamed,
            status: self.status,
            error: self.error.map(|e| e.to_string()),
        }
    }
}

/// Owned counterpart of [`RequestOutcome`], sent to the writer thread.
#[derive(Debug, Clone)]
struct RequestRow {
    date: String,
    created_at: String,
    model: String,
    route: String,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    duration_ms: i64,
    streamed: bool,
    status: u16,
    error: Option<String>,
}

impl RequestRow {
    fn insert_sql() -> &'static str {
        "INSERT INTO request_logs (
            date, created_at, model, route,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
            duration_ms, streamed, status, error
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
    }

    fn bind_to(&self, stmt: &mut rusqlite::Statement<'_>) -> rusqlite::Result<()> {
        stmt.execute(params![
            self.date,
            self.created_at,
            self.model,
            self.route,
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
            self.duration_ms,
            if self.streamed { 1 } else { 0 },
            self.status as i64,
            self.error,
        ])?;
        Ok(())
    }
}

/// A single request log entry stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestLogItem {
    pub id: i64,
    pub created_at: String,
    pub model: String,
    pub route: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub duration_ms: i64,
    pub streamed: bool,
    pub status: u16,
    pub error: Option<String>,
}

/// Filter criteria for querying request logs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestLogFilter {
    pub model: Option<String>,
    pub status_group: Option<String>,
    pub streamed: Option<bool>,
    pub search: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

/// Query result containing matched items, total count and model list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestLogsResult {
    pub items: Vec<RequestLogItem>,
    pub total: i64,
    pub models: Vec<String>,
}

/// Aggregated statistics for a single calendar day.
///
/// Derived on demand from `request_logs` — there is no separate daily table, so
/// the counters can never drift out of sync with the rows they summarize.
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

/// Persistent request-log store backed by a local SQLite database.
///
/// The database lives at `~/.proxy-rs/stats.db` and holds a single
/// `request_logs` table — one row per finished request. Daily aggregates are
/// computed from it with `GROUP BY date`.
///
/// Writes never run on a request handler: `record_request_log` pushes the row
/// onto an unbounded queue and returns, and a dedicated writer thread drains
/// that queue in batches inside a transaction. So a slow disk or a lock
/// contention with the GUI's read queries can never stall a proxied request.
///
/// Queries use their own connection; WAL mode lets them read while the writer
/// thread holds the write lock.
pub struct StatsDb {
    /// Connection used for reads (and for writes in inline/test mode).
    conn: Arc<Mutex<Connection>>,
    /// Queue feeding the background writer thread. `None` in inline mode,
    /// where writes go straight through `conn`.
    queue: Option<Sender<RequestRow>>,
}

impl StatsDb {
    /// Open (or create) the statistics database, ensure the schema exists, and
    /// spawn the background writer thread.
    pub fn open() -> Result<Arc<Self>> {
        let path = match data_dir() {
            Some(dir) => dir.join("stats.db"),
            None => {
                return Err(anyhow::anyhow!(
                    "Cannot determine data directory for stats.db"
                ))
            }
        };

        let read_conn = Self::connect(&path)?;
        Self::init_schema(&read_conn)?;
        let write_conn = Self::connect(&path)?;

        let (tx, rx) = channel::<RequestRow>();
        std::thread::Builder::new()
            .name("stats-writer".to_string())
            .spawn(move || writer_loop(write_conn, rx))?;

        Ok(Arc::new(Self {
            conn: Arc::new(Mutex::new(read_conn)),
            queue: Some(tx),
        }))
    }

    /// Construct from an existing connection (e.g. an in-memory DB for tests).
    ///
    /// Inline mode: no writer thread, so writes are synchronous and immediately
    /// visible to queries.
    pub fn from_conn(conn: Connection) -> Self {
        Self {
            conn: Arc::new(Mutex::new(conn)),
            queue: None,
        }
    }

    /// Open a fresh in-memory database with the schema applied. Useful as a
    /// non-persistent fallback and for tests.
    pub fn in_memory() -> Result<Arc<Self>> {
        let conn = Connection::open_in_memory()?;
        Self::init_schema(&conn)?;
        Ok(Arc::new(Self {
            conn: Arc::new(Mutex::new(conn)),
            queue: None,
        }))
    }

    fn connect(path: &std::path::Path) -> Result<Connection> {
        let conn = Connection::open(path)?;
        // Enable WAL mode so the GUI can read while the writer thread writes.
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        // Wait rather than fail outright if the write lock is momentarily held.
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(conn)
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS request_logs (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                date                TEXT NOT NULL DEFAULT '',
                created_at          TEXT NOT NULL,
                model               TEXT NOT NULL,
                route               TEXT NOT NULL,
                input_tokens        INTEGER NOT NULL DEFAULT 0,
                output_tokens       INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens   INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens  INTEGER NOT NULL DEFAULT 0,
                duration_ms         INTEGER NOT NULL DEFAULT 0,
                streamed            INTEGER NOT NULL DEFAULT 0,
                status              INTEGER NOT NULL DEFAULT 0,
                error               TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_request_logs_id_desc ON request_logs(id DESC);
            CREATE INDEX IF NOT EXISTS idx_request_logs_created_at ON request_logs(created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_request_logs_model ON request_logs(model);
            CREATE INDEX IF NOT EXISTS idx_request_logs_status ON request_logs(status);",
        )?;
        // Must run before the `date` index is created, since the column only
        // exists after migration on databases from older builds.
        Self::migrate(conn)?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_request_logs_date ON request_logs(date);",
        )?;
        Ok(())
    }

    /// Bring a database written by an older version up to the current schema.
    ///
    /// Older builds kept a separate `daily_stats` table and had no `date`
    /// column; both are reconciled here so existing installs keep their history.
    fn migrate(conn: &Connection) -> Result<()> {
        let has_date = conn
            .prepare("PRAGMA table_info(request_logs)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .flatten()
            .any(|col| col == "date");

        if !has_date {
            conn.execute("ALTER TABLE request_logs ADD COLUMN date TEXT", [])?;
        }

        // Backfill rows written before the column existed.
        conn.execute(
            "UPDATE request_logs
                SET date = substr(created_at, 1, 10)
              WHERE date IS NULL OR date = ''",
            [],
        )?;

        // daily_stats is now derived from request_logs on demand.
        conn.execute_batch("DROP TABLE IF EXISTS daily_stats;")?;

        Ok(())
    }

    /// Record the outcome of one finished request.
    ///
    /// Non-blocking in persistent mode: the row is queued for the writer thread.
    /// Call this exactly once per request, after the response has completed —
    /// never per streamed chunk.
    pub fn record_request_log(&self, outcome: RequestOutcome<'_>) -> Result<()> {
        let row = outcome.to_row(&local_date_string(), &local_datetime_string());
        match &self.queue {
            Some(tx) => tx
                .send(row)
                .map_err(|_| anyhow::anyhow!("stats writer thread stopped")),
            None => {
                let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
                let mut stmt = conn.prepare(RequestRow::insert_sql())?;
                row.bind_to(&mut stmt)?;
                Ok(())
            }
        }
    }

    /// Query request logs with optional filtering and pagination.
    pub fn query_request_logs(&self, filter: &RequestLogFilter) -> Result<RequestLogsResult> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());

        // 1. Fetch distinct models for filter dropdown
        let mut model_stmt =
            conn.prepare("SELECT DISTINCT model FROM request_logs ORDER BY model ASC")?;
        let models_iter = model_stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut models = Vec::new();
        for m in models_iter.flatten() {
            if !m.is_empty() {
                models.push(m);
            }
        }

        // 2. Build WHERE clause
        let mut where_clauses = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(ref m) = filter.model {
            let m_trimmed = m.trim();
            if !m_trimmed.is_empty() && m_trimmed != "all" {
                where_clauses.push("model = ?".to_string());
                params.push(Box::new(m_trimmed.to_string()));
            }
        }

        if let Some(ref s) = filter.status_group {
            match s.as_str() {
                "2xx" | "success" => {
                    where_clauses
                        .push("status >= 200 AND status < 300 AND error IS NULL".to_string());
                }
                "4xx" | "client_error" => {
                    where_clauses.push("status >= 400 AND status < 500".to_string());
                }
                "5xx" | "server_error" => {
                    where_clauses.push("status >= 500".to_string());
                }
                "error" => {
                    where_clauses.push("(status >= 400 OR error IS NOT NULL)".to_string());
                }
                _ => {}
            }
        }

        if let Some(streamed) = filter.streamed {
            where_clauses.push("streamed = ?".to_string());
            params.push(Box::new(if streamed { 1 } else { 0 }));
        }

        if let Some(ref q) = filter.search {
            let q_trimmed = q.trim();
            if !q_trimmed.is_empty() {
                where_clauses.push("(model LIKE ? OR route LIKE ? OR error LIKE ?)".to_string());
                let like_pat = format!("%{}%", q_trimmed);
                params.push(Box::new(like_pat.clone()));
                params.push(Box::new(like_pat.clone()));
                params.push(Box::new(like_pat));
            }
        }

        let where_sql = if where_clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", where_clauses.join(" AND "))
        };

        // 3. Count total matching rows
        let count_sql = format!("SELECT COUNT(*) FROM request_logs {}", where_sql);
        let params_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let total: i64 = conn.query_row(
            &count_sql,
            rusqlite::params_from_iter(params_refs.iter().copied()),
            |row| row.get(0),
        )?;

        // 4. Query page items
        let limit = filter.limit.unwrap_or(50).clamp(1, 500);
        let offset = filter.offset.unwrap_or(0);
        let query_sql = format!(
            "SELECT id, created_at, model, route,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                    duration_ms, streamed, status, error
             FROM request_logs
             {}
             ORDER BY id DESC
             LIMIT ? OFFSET ?",
            where_sql
        );

        let mut query_params: Vec<&dyn rusqlite::ToSql> = params_refs;
        let limit_i64 = limit as i64;
        let offset_i64 = offset as i64;
        query_params.push(&limit_i64);
        query_params.push(&offset_i64);

        let mut stmt = conn.prepare(&query_sql)?;
        let items_iter = stmt.query_map(rusqlite::params_from_iter(query_params), |row| {
            let streamed_int: i64 = row.get(9)?;
            let status_int: i64 = row.get(10)?;
            Ok(RequestLogItem {
                id: row.get(0)?,
                created_at: row.get(1)?,
                model: row.get(2)?,
                route: row.get(3)?,
                input_tokens: row.get(4)?,
                output_tokens: row.get(5)?,
                cache_read_tokens: row.get(6)?,
                cache_write_tokens: row.get(7)?,
                duration_ms: row.get(8)?,
                streamed: streamed_int != 0,
                status: status_int as u16,
                error: row.get(11)?,
            })
        })?;

        let mut items = Vec::new();
        for item in items_iter {
            items.push(item?);
        }

        Ok(RequestLogsResult {
            items,
            total,
            models,
        })
    }

    /// Clear all request logs from the database.
    pub fn clear_request_logs(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute("DELETE FROM request_logs", [])?;
        Ok(())
    }

    /// Return statistics for today (local date). Returns a zeroed `DayStats`
    /// with today's date if no requests have been recorded yet.
    pub fn query_today(&self) -> Result<DayStats> {
        let date = local_date_string();
        self.query_date(&date)
    }

    /// Return statistics for an arbitrary date (`YYYY-MM-DD`), aggregated from
    /// the request logs for that day.
    pub fn query_date(&self, date: &str) -> Result<DayStats> {
        let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        let result = conn.query_row(
            "SELECT
                COUNT(*),
                COALESCE(SUM(CASE WHEN status >= 200 AND status < 300 AND error IS NULL
                                  THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status >= 200 AND status < 300 AND error IS NULL
                                  THEN 0 ELSE 1 END), 0),
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_write_tokens), 0),
                COALESCE(SUM(output_tokens), 0)
             FROM request_logs WHERE date = ?1",
            params![date],
            |row| {
                Ok(DayStats {
                    date: date.to_string(),
                    requests_total: row.get(0)?,
                    requests_success: row.get(1)?,
                    requests_failed: row.get(2)?,
                    tokens_input: row.get(3)?,
                    tokens_cache_read: row.get(4)?,
                    tokens_cache_write: row.get(5)?,
                    tokens_output: row.get(6)?,
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

/// Drains the request queue and commits rows in batches.
///
/// Runs on its own OS thread for the lifetime of the `StatsDb`. The first
/// `recv` blocks until work arrives; everything already queued is then drained
/// into a single transaction, so a burst of requests costs one commit.
fn writer_loop(conn: Connection, rx: Receiver<RequestRow>) {
    let mut stmt = match conn.prepare(RequestRow::insert_sql()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("stats writer failed to prepare insert: {}", e);
            return;
        }
    };

    // recv() returns Err only when every sender has been dropped, i.e. the
    // StatsDb is gone — that is the signal to exit.
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while let Ok(next) = rx.try_recv() {
            batch.push(next);
        }

        if let Err(e) = insert_batch(&conn, &mut stmt, &batch) {
            tracing::warn!("stats writer dropped {} row(s): {}", batch.len(), e);
        }
    }
}

fn insert_batch(
    conn: &Connection,
    stmt: &mut rusqlite::Statement<'_>,
    batch: &[RequestRow],
) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    for row in batch {
        row.bind_to(stmt)?;
    }
    tx.commit()
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Return today's date as a `YYYY-MM-DD` string in the machine's local time.
///
/// Uses the same offset as log timestamps, so a day's stats cover exactly the
/// range the log file shows for that day.
fn local_date_string() -> String {
    let offset_secs = crate::util::local_utc_offset_secs();
    let local_secs = (now_secs() + offset_secs).max(0) as u64;
    let (y, m, d) = crate::util::civil_from_days((local_secs / 86400) as i64);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// Return current local datetime as `YYYY-MM-DD HH:MM:SS` string, in the same
/// zone the log file uses.
fn local_datetime_string() -> String {
    let offset_secs = crate::util::local_utc_offset_secs();
    let local_secs = (now_secs() + offset_secs).max(0) as u64;
    // `format_epoch_secs` renders an epoch-seconds value; feeding it the
    // already-shifted local seconds yields local wall-clock fields.
    crate::util::format_epoch_secs(local_secs)
}

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome<'a>(
        model: &'a str,
        route: &'a str,
        tokens: &'a TokenRecord,
        status: u16,
        error: Option<&'a str>,
        streamed: bool,
    ) -> RequestOutcome<'a> {
        RequestOutcome {
            model,
            route,
            tokens,
            duration_ms: 1250,
            streamed,
            status,
            error,
        }
    }

    #[test]
    fn daily_stats_are_derived_from_request_logs() {
        let db = StatsDb::in_memory().unwrap();

        let ok = TokenRecord {
            input: 100,
            cache_read: 900,
            cache_write: 0,
            output: 50,
        };
        db.record_request_log(outcome(
            "claude-3-5-sonnet-20241022",
            "/v1/messages",
            &ok,
            200,
            None,
            true,
        ))
        .unwrap();

        let failed = TokenRecord::default();
        db.record_request_log(outcome(
            "gpt-4o",
            "/v1/chat/completions",
            &failed,
            502,
            Some("Upstream gateway error"),
            false,
        ))
        .unwrap();

        let today = db.query_today().unwrap();
        assert_eq!(today.requests_total, 2);
        assert_eq!(today.requests_success, 1);
        assert_eq!(today.requests_failed, 1);
        assert_eq!(today.tokens_input, 100);
        assert_eq!(today.tokens_cache_read, 900);
        assert_eq!(today.tokens_output, 50);
        assert_eq!(today.cache_hit_pct(), 90); // 900/(100+900)*100
        assert_eq!(today.tokens_total(), 1050);
        assert!(!today.date.is_empty());
    }

    #[test]
    fn request_logs_crud_and_filtering() {
        let db = StatsDb::in_memory().unwrap();

        // 1. Record success log
        let ok_tokens = TokenRecord {
            input: 150,
            cache_read: 50,
            cache_write: 0,
            output: 200,
        };
        db.record_request_log(outcome(
            "claude-3-5-sonnet-20241022",
            "/v1/messages",
            &ok_tokens,
            200,
            None,
            true,
        ))
        .unwrap();

        // 2. Record failure log
        let err_tokens = TokenRecord::default();
        db.record_request_log(outcome(
            "gpt-4o",
            "/v1/chat/completions",
            &err_tokens,
            502,
            Some("Upstream gateway error"),
            false,
        ))
        .unwrap();

        // Query all
        let all = db.query_request_logs(&RequestLogFilter::default()).unwrap();
        assert_eq!(all.total, 2);
        assert_eq!(all.items.len(), 2);
        assert_eq!(all.models.len(), 2);
        assert_eq!(all.items[0].model, "gpt-4o"); // Most recent first (id DESC)
        assert_eq!(all.items[0].status, 502);
        assert!(!all.items[0].streamed);
        assert_eq!(all.items[1].model, "claude-3-5-sonnet-20241022");
        assert_eq!(all.items[1].status, 200);
        assert!(all.items[1].streamed);
        assert_eq!(all.items[1].input_tokens, 150);

        // Filter by model
        let claude_only = db
            .query_request_logs(&RequestLogFilter {
                model: Some("claude-3-5-sonnet-20241022".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(claude_only.total, 1);
        assert_eq!(claude_only.items[0].model, "claude-3-5-sonnet-20241022");

        // Filter by status success (2xx)
        let success_only = db
            .query_request_logs(&RequestLogFilter {
                status_group: Some("2xx".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(success_only.total, 1);
        assert_eq!(success_only.items[0].status, 200);

        // Filter by status error
        let error_only = db
            .query_request_logs(&RequestLogFilter {
                status_group: Some("error".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(error_only.total, 1);
        assert_eq!(error_only.items[0].status, 502);

        // Filter by streamed
        let stream_only = db
            .query_request_logs(&RequestLogFilter {
                streamed: Some(true),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(stream_only.total, 1);
        assert!(stream_only.items[0].streamed);

        // Clear logs
        db.clear_request_logs().unwrap();
        let empty = db.query_request_logs(&RequestLogFilter::default()).unwrap();
        assert_eq!(empty.total, 0);
        assert_eq!(empty.items.len(), 0);
    }

    #[test]
    fn daily_stats_reflect_cleared_logs() {
        // The two sources can no longer disagree: clearing the log zeroes the
        // counters, because the counters are computed from the log itself.
        let db = StatsDb::in_memory().unwrap();
        let tokens = TokenRecord {
            input: 10,
            cache_read: 0,
            cache_write: 0,
            output: 5,
        };
        db.record_request_log(outcome("m", "/v1/messages", &tokens, 200, None, false))
            .unwrap();
        assert_eq!(db.query_today().unwrap().requests_total, 1);

        db.clear_request_logs().unwrap();
        assert_eq!(db.query_today().unwrap().requests_total, 0);
    }

    #[test]
    fn migration_drops_legacy_daily_stats_and_backfills_dates() {
        // Simulate a database written by the previous schema: no `date` column,
        // plus the now-redundant `daily_stats` table.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE daily_stats (
                date TEXT PRIMARY KEY,
                requests_total INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE request_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at TEXT NOT NULL,
                model TEXT NOT NULL,
                route TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                streamed INTEGER NOT NULL DEFAULT 0,
                status INTEGER NOT NULL DEFAULT 0,
                error TEXT
             );
             INSERT INTO request_logs (created_at, model, route, status)
             VALUES ('2026-09-22 10:00:00', 'hy3', '/v1/responses', 200);",
        )
        .unwrap();

        StatsDb::init_schema(&conn).unwrap();

        let db = StatsDb::from_conn(conn);
        let stats = db.query_date("2026-09-22").unwrap();
        assert_eq!(stats.requests_total, 1, "date backfilled from created_at");
        assert_eq!(stats.requests_success, 1);
    }

    #[test]
    fn writer_thread_persists_queued_rows() {
        // Persistent mode: record_request_log only queues, a background thread
        // commits. The queue must eventually land every row.
        let dir = std::env::temp_dir().join(format!("proxy-rs-stats-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stats.db");

        let read_conn = StatsDb::connect(&path).unwrap();
        StatsDb::init_schema(&read_conn).unwrap();
        let write_conn = StatsDb::connect(&path).unwrap();
        let (tx, rx) = channel::<RequestRow>();
        std::thread::spawn(move || writer_loop(write_conn, rx));

        let db = StatsDb {
            conn: Arc::new(Mutex::new(read_conn)),
            queue: Some(tx),
        };

        let tokens = TokenRecord {
            input: 20,
            cache_read: 80,
            cache_write: 0,
            output: 20,
        };
        for _ in 0..50 {
            db.record_request_log(outcome("hy3", "/v1/responses", &tokens, 200, None, true))
                .unwrap();
        }

        // Poll until the writer thread has drained the queue.
        let mut total = 0;
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(10));
            total = db.query_today().unwrap().requests_total;
            if total == 50 {
                break;
            }
        }
        assert_eq!(total, 50);

        let today = db.query_today().unwrap();
        assert_eq!(today.tokens_input, 20 * 50);
        assert_eq!(today.tokens_cache_read, 80 * 50);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
