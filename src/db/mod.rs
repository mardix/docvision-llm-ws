//! Repository over `sqlx::Any`: one SQL dialect (`$N` placeholders, portable upserts)
//! serves SQLite (default) and PostgreSQL (`postgres` feature). Reads use a small pool;
//! all writes go through the single writer task (`crate::writer`).

#[cfg(feature = "postgres")]
pub mod postgres;
pub mod sqlite;

use serde::Serialize;
use sqlx::any::{AnyPoolOptions, AnyRow};
use sqlx::{AnyConnection, AnyPool, Connection, Row};

pub type DbResult<T> = Result<T, sqlx::Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Sqlite,
    Postgres,
}

#[derive(Clone)]
pub struct Db {
    pub pool: AnyPool,
    pub backend: Backend,
    url: String,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn rfc3339(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(ms as i128 * 1_000_000)
        .ok()
        .and_then(|t| t.format(&time::format_description::well_known::Rfc3339).ok())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Default)]
pub struct RequestRow {
    pub request_id: String,
    pub operation: String,
    pub mode: Option<String>,
    pub source: Option<String>,
    pub options: Option<String>,
    pub metadata: Option<String>,
    pub request_status: String,
    pub execution_status: Option<String>,
    pub job_id: Option<String>,
    pub http_status: Option<i64>,
    pub created_at: i64,
    pub requester_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RequestFinish {
    pub request_id: String,
    pub request_status: String,
    pub execution_status: Option<String>,
    pub execution_stage: Option<String>,
    pub http_status: Option<i64>,
    pub finished_at: i64,
    pub timings: Option<String>,
    pub statistics: Option<String>,
    pub usage: Option<String>,
    pub warnings: Option<String>,
    pub error: Option<String>,
    pub result_available: bool,
    /// Detected document format (`pdf`, `docx`, …), when known.
    pub format: Option<String>,
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub job_id: String,
    pub request_id: String,
    pub state: String,
    pub priority: String,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub result_path: Option<String>,
    pub error: Option<String>,
    pub secrets: Option<Vec<u8>>,
    pub cache_key: Option<String>,
    pub has_webhook: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventRow {
    #[serde(skip)]
    pub request_id: String,
    pub at: i64,
    pub stage: String,
    pub event: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WebhookRow {
    pub event_id: String,
    pub job_id: String,
    pub status: String,
    pub attempts: i64,
    pub last_http_status: Option<i64>,
    pub last_error: Option<String>,
    pub next_attempt_at: Option<i64>,
    pub created_at: i64,
    pub delivered_at: Option<i64>,
}

/// A single write executed by the writer task.
#[derive(Debug, Clone)]
pub enum WriteOp {
    InsertRequest(RequestRow),
    FinishRequest(RequestFinish),
    RequestExecution {
        job_id: String,
        execution_status: String,
        stage: Option<String>,
    },
    InsertJob(JobRow),
    JobStarted {
        job_id: String,
        at: i64,
    },
    JobFinished {
        job_id: String,
        state: String,
        at: i64,
        result_path: Option<String>,
        error: Option<String>,
    },
    Events(Vec<EventRow>),
    Call {
        call_id: String,
        request_id: String,
        record: String,
        at: i64,
    },
    CachePut {
        key: String,
        path: String,
        body_start: i64,
        body_end: i64,
        size: i64,
        at: i64,
    },
    CacheDelete {
        key: String,
    },
    Webhook(WebhookRow),
    /// Startup recovery: running jobs -> failed(interrupted), unfinished request logs -> interrupted.
    Recover {
        at: i64,
    },
    /// Batched retention; deletes at most `limit` rows per table.
    Retention {
        result_cutoff: i64,
        history_cutoff: i64,
        limit: i64,
    },
}

fn opt_i(row: &AnyRow, c: &str) -> Option<i64> {
    row.try_get::<Option<i64>, _>(c).ok().flatten()
}
fn opt_s(row: &AnyRow, c: &str) -> Option<String> {
    row.try_get::<Option<String>, _>(c).ok().flatten()
}

impl Db {
    pub async fn connect(url: &str) -> DbResult<Db> {
        sqlx::any::install_default_drivers();
        let backend = if url.starts_with("postgres") { Backend::Postgres } else { Backend::Sqlite };
        if backend == Backend::Postgres && !cfg!(feature = "postgres") {
            return Err(sqlx::Error::Configuration("PostgreSQL URL given but the `postgres` feature is not compiled".into()));
        }
        // Run migrations on a dedicated connection before opening the read pool.
        let mut conn = Self::open_conn(url, backend).await?;
        migrate(&mut conn, backend).await?;
        conn.close().await?;
        let pool = match backend {
            Backend::Sqlite => sqlite::read_pool(url).await?,
            Backend::Postgres => AnyPoolOptions::new().max_connections(4).connect(url).await?,
        };
        Ok(Db { pool, backend, url: url.to_string() })
    }

    pub async fn open_conn(url: &str, backend: Backend) -> DbResult<AnyConnection> {
        let mut c = AnyConnection::connect(url).await?;
        if backend == Backend::Sqlite {
            sqlite::configure(&mut c).await?;
        }
        Ok(c)
    }

    /// The single write connection, owned by the writer task.
    pub async fn writer_conn(&self) -> DbResult<AnyConnection> {
        Self::open_conn(&self.url, self.backend).await
    }

    pub async fn ping(&self) -> bool {
        sqlx::query("SELECT 1").fetch_one(&self.pool).await.is_ok()
    }

    pub async fn get_job(&self, job_id: &str) -> DbResult<Option<JobRow>> {
        let r = sqlx::query("SELECT * FROM jobs WHERE job_id = $1").bind(job_id).fetch_optional(&self.pool).await?;
        Ok(r.map(|r| job_from(&r)))
    }

    pub async fn jobs_in_state(&self, state: &str) -> DbResult<Vec<JobRow>> {
        let rows = sqlx::query("SELECT * FROM jobs WHERE state = $1 ORDER BY created_at").bind(state).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(job_from).collect())
    }

    pub async fn webhooks_for_job(&self, job_id: &str) -> DbResult<Vec<WebhookRow>> {
        let rows = sqlx::query("SELECT * FROM webhook_deliveries WHERE job_id = $1 ORDER BY created_at, event_id")
            .bind(job_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(webhook_from).collect())
    }

    pub async fn pending_webhooks(&self) -> DbResult<Vec<WebhookRow>> {
        let rows = sqlx::query("SELECT * FROM webhook_deliveries WHERE status = 'pending'").fetch_all(&self.pool).await?;
        Ok(rows.iter().map(webhook_from).collect())
    }

    pub async fn cache_get(&self, key: &str, min_created: i64) -> DbResult<Option<(String, i64, i64)>> {
        let r = sqlx::query("SELECT result_path, body_start, body_end FROM result_cache WHERE cache_key = $1 AND created_at >= $2")
            .bind(key)
            .bind(min_created)
            .fetch_optional(&self.pool)
            .await?;
        Ok(r.map(|r| (r.get::<String, _>(0), r.get::<i64, _>(1), r.get::<i64, _>(2))))
    }

    pub async fn max_request_seq(&self) -> DbResult<i64> {
        let r = sqlx::query("SELECT COALESCE(MAX(seq), 0) FROM request_logs").fetch_one(&self.pool).await?;
        Ok(r.get::<i64, _>(0))
    }

    pub async fn history_list(&self, f: &HistoryFilter, high: i64, after: Option<i64>, limit: i64) -> DbResult<Vec<AnyRow>> {
        let mut sql = String::from(
            "SELECT seq, request_id, operation, mode, source, request_status, execution_status, execution_stage, job_id, http_status, created_at, finished_at, timings, statistics, usage, warnings, error, result_available, requester_id, format FROM request_logs WHERE seq <= $1",
        );
        let mut n = 1;
        let mut push = |sql: &mut String, cond: &str| {
            n += 1;
            sql.push_str(&format!(" AND {cond} ${n}"));
        };
        if after.is_some() {
            push(&mut sql, "seq <");
        }
        if f.operation.is_some() {
            push(&mut sql, "operation =");
        }
        if f.execution.is_some() {
            push(&mut sql, "mode =");
        }
        if f.execution_status.is_some() {
            push(&mut sql, "execution_status =");
        }
        if f.requester_id.is_some() {
            push(&mut sql, "requester_id =");
        }
        if f.format.is_some() {
            push(&mut sql, "format =");
        }
        if f.in_progress {
            sql.push_str(" AND execution_status IN ('queued', 'running')");
        }
        if f.created_from.is_some() {
            push(&mut sql, "created_at >=");
        }
        if f.created_to.is_some() {
            push(&mut sql, "created_at <");
        }
        sql.push_str(&format!(" ORDER BY seq DESC LIMIT {limit}"));
        let mut q = sqlx::query(&sql).bind(high);
        if let Some(a) = after {
            q = q.bind(a);
        }
        if let Some(v) = &f.operation {
            q = q.bind(v.clone());
        }
        if let Some(v) = &f.execution {
            q = q.bind(v.clone());
        }
        if let Some(v) = &f.execution_status {
            q = q.bind(v.clone());
        }
        if let Some(v) = &f.requester_id {
            q = q.bind(v.clone());
        }
        if let Some(v) = &f.format {
            q = q.bind(v.clone());
        }
        if let Some(v) = f.created_from {
            q = q.bind(v);
        }
        if let Some(v) = f.created_to {
            q = q.bind(v);
        }
        q.fetch_all(&self.pool).await
    }

    /// Rows for the `stats` operation (newest first, bounded).
    pub async fn stats_rows(&self, since: i64, limit: i64) -> DbResult<Vec<AnyRow>> {
        sqlx::query("SELECT operation, mode, request_status, execution_status, http_status, error, timings, statistics, usage, created_at, requester_id FROM request_logs WHERE created_at >= $1 ORDER BY seq DESC LIMIT $2")
            .bind(since)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
    }

    pub async fn history_get(&self, request_id: &str) -> DbResult<Option<AnyRow>> {
        sqlx::query("SELECT * FROM request_logs WHERE request_id = $1").bind(request_id).fetch_optional(&self.pool).await
    }

    pub async fn events_page(&self, request_id: &str, after: i64, limit: i64) -> DbResult<Vec<(i64, EventRow)>> {
        let rows =
            sqlx::query("SELECT id, at, stage, event, detail FROM request_events WHERE request_id = $1 AND id > $2 ORDER BY id LIMIT $3")
                .bind(request_id)
                .bind(after)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.get::<i64, _>("id"),
                    EventRow {
                        request_id: request_id.to_string(),
                        at: r.get("at"),
                        stage: r.get("stage"),
                        event: r.get("event"),
                        detail: opt_s(r, "detail"),
                    },
                )
            })
            .collect())
    }

    pub async fn calls_page(&self, request_id: &str, after: i64, limit: i64) -> DbResult<Vec<(i64, String)>> {
        let rows = sqlx::query("SELECT id, record FROM llm_calls WHERE request_id = $1 AND id > $2 ORDER BY id LIMIT $3")
            .bind(request_id)
            .bind(after)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| (r.get::<i64, _>(0), r.get::<String, _>(1))).collect())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct HistoryFilter {
    pub operation: Option<String>,
    pub execution: Option<String>,
    pub execution_status: Option<String>,
    pub requester_id: Option<String>,
    pub format: Option<String>,
    /// Only `queued` and `running` requests.
    pub in_progress: bool,
    pub created_from: Option<i64>,
    pub created_to: Option<i64>,
}

fn job_from(r: &AnyRow) -> JobRow {
    JobRow {
        job_id: r.get("job_id"),
        request_id: r.get("request_id"),
        state: r.get("state"),
        priority: r.get("priority"),
        created_at: r.get("created_at"),
        started_at: opt_i(r, "started_at"),
        finished_at: opt_i(r, "finished_at"),
        result_path: opt_s(r, "result_path"),
        error: opt_s(r, "error"),
        secrets: r.try_get::<Option<Vec<u8>>, _>("secrets").ok().flatten(),
        cache_key: opt_s(r, "cache_key"),
        has_webhook: opt_i(r, "has_webhook").unwrap_or(0) != 0,
    }
}

fn webhook_from(r: &AnyRow) -> WebhookRow {
    WebhookRow {
        event_id: r.get("event_id"),
        job_id: r.get("job_id"),
        status: r.get("status"),
        attempts: r.get("attempts"),
        last_http_status: opt_i(r, "last_http_status"),
        last_error: opt_s(r, "last_error"),
        next_attempt_at: opt_i(r, "next_attempt_at"),
        created_at: r.get("created_at"),
        delivered_at: opt_i(r, "delivered_at"),
    }
}

const MIGRATIONS: &[(i64, &str, &str)] = &[
    (1, include_str!("migrations/0001_init.sqlite.sql"), include_str!("migrations/0001_init.postgres.sql")),
    (2, include_str!("migrations/0002_requester.sql"), include_str!("migrations/0002_requester.sql")),
    (3, include_str!("migrations/0003_format.sql"), include_str!("migrations/0003_format.sql")),
];

async fn migrate(conn: &mut AnyConnection, backend: Backend) -> DbResult<()> {
    sqlx::query("CREATE TABLE IF NOT EXISTS schema_migrations (version BIGINT PRIMARY KEY, applied_at BIGINT NOT NULL)")
        .execute(&mut *conn)
        .await?;
    for (version, lite, pg) in MIGRATIONS {
        let done = sqlx::query("SELECT 1 FROM schema_migrations WHERE version = $1").bind(*version).fetch_optional(&mut *conn).await?;
        if done.is_some() {
            continue;
        }
        let sql = if backend == Backend::Sqlite { lite } else { pg };
        let mut tx = conn.begin().await?;
        for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            sqlx::query(stmt).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO schema_migrations (version, applied_at) VALUES ($1, $2)")
            .bind(*version)
            .bind(now_ms())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    Ok(())
}

/// Execute one op inside the writer's open transaction.
pub async fn apply(conn: &mut AnyConnection, op: &WriteOp) -> DbResult<()> {
    match op {
        WriteOp::InsertRequest(r) => {
            sqlx::query("INSERT INTO request_logs (request_id, operation, mode, source, options, metadata, request_status, execution_status, job_id, http_status, created_at, requester_id) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)")
                .bind(&r.request_id).bind(&r.operation).bind(r.mode.clone()).bind(r.source.clone()).bind(r.options.clone())
                .bind(r.metadata.clone()).bind(&r.request_status).bind(r.execution_status.clone()).bind(r.job_id.clone())
                .bind(r.http_status).bind(r.created_at).bind(r.requester_id.clone())
                .execute(&mut *conn).await?;
        }
        WriteOp::FinishRequest(f) => {
            sqlx::query("UPDATE request_logs SET request_status=$2, execution_status=COALESCE($3, execution_status), execution_stage=$4, http_status=COALESCE($5, http_status), finished_at=$6, timings=$7, statistics=$8, usage=$9, warnings=$10, error=$11, result_available=$12, format=COALESCE($13, format) WHERE request_id=$1")
                .bind(&f.request_id).bind(&f.request_status).bind(f.execution_status.clone()).bind(f.execution_stage.clone())
                .bind(f.http_status).bind(f.finished_at).bind(f.timings.clone()).bind(f.statistics.clone()).bind(f.usage.clone())
                .bind(f.warnings.clone()).bind(f.error.clone()).bind(f.result_available as i64).bind(f.format.clone())
                .execute(&mut *conn).await?;
        }
        WriteOp::RequestExecution { job_id, execution_status, stage } => {
            sqlx::query("UPDATE request_logs SET execution_status=$2, execution_stage=$3 WHERE job_id=$1")
                .bind(job_id)
                .bind(execution_status)
                .bind(stage.clone())
                .execute(&mut *conn)
                .await?;
        }
        WriteOp::InsertJob(j) => {
            sqlx::query("INSERT INTO jobs (job_id, request_id, state, priority, created_at, secrets, cache_key, has_webhook) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
                .bind(&j.job_id).bind(&j.request_id).bind(&j.state).bind(&j.priority).bind(j.created_at)
                .bind(j.secrets.clone()).bind(j.cache_key.clone()).bind(j.has_webhook as i64)
                .execute(&mut *conn).await?;
        }
        WriteOp::JobStarted { job_id, at } => {
            sqlx::query("UPDATE jobs SET state='running', started_at=$2 WHERE job_id=$1")
                .bind(job_id)
                .bind(*at)
                .execute(&mut *conn)
                .await?;
        }
        WriteOp::JobFinished { job_id, state, at, result_path, error } => {
            // Secrets are no longer needed once a job is terminal, except for pending webhooks (kept).
            sqlx::query("UPDATE jobs SET state=$2, finished_at=$3, result_path=$4, error=$5 WHERE job_id=$1")
                .bind(job_id)
                .bind(state)
                .bind(*at)
                .bind(result_path.clone())
                .bind(error.clone())
                .execute(&mut *conn)
                .await?;
        }
        WriteOp::Events(evs) => {
            for e in evs {
                sqlx::query("INSERT INTO request_events (request_id, at, stage, event, detail) VALUES ($1,$2,$3,$4,$5)")
                    .bind(&e.request_id)
                    .bind(e.at)
                    .bind(&e.stage)
                    .bind(&e.event)
                    .bind(e.detail.clone())
                    .execute(&mut *conn)
                    .await?;
            }
        }
        WriteOp::Call { call_id, request_id, record, at } => {
            sqlx::query("INSERT INTO llm_calls (call_id, request_id, record, created_at) VALUES ($1,$2,$3,$4)")
                .bind(call_id)
                .bind(request_id)
                .bind(record)
                .bind(*at)
                .execute(&mut *conn)
                .await?;
        }
        WriteOp::CachePut { key, path, body_start, body_end, size, at } => {
            sqlx::query("INSERT INTO result_cache (cache_key, result_path, body_start, body_end, size, created_at) VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (cache_key) DO UPDATE SET result_path=excluded.result_path, body_start=excluded.body_start, body_end=excluded.body_end, size=excluded.size, created_at=excluded.created_at")
                .bind(key).bind(path).bind(*body_start).bind(*body_end).bind(*size).bind(*at)
                .execute(&mut *conn).await?;
        }
        WriteOp::CacheDelete { key } => {
            sqlx::query("DELETE FROM result_cache WHERE cache_key=$1").bind(key).execute(&mut *conn).await?;
        }
        WriteOp::Webhook(w) => {
            sqlx::query("INSERT INTO webhook_deliveries (event_id, job_id, status, attempts, last_http_status, last_error, next_attempt_at, created_at, delivered_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT (event_id) DO UPDATE SET status=excluded.status, attempts=excluded.attempts, last_http_status=excluded.last_http_status, last_error=excluded.last_error, next_attempt_at=excluded.next_attempt_at, delivered_at=excluded.delivered_at")
                .bind(&w.event_id).bind(&w.job_id).bind(&w.status).bind(w.attempts).bind(w.last_http_status)
                .bind(w.last_error.clone()).bind(w.next_attempt_at).bind(w.created_at).bind(w.delivered_at)
                .execute(&mut *conn).await?;
        }
        WriteOp::Recover { at } => {
            sqlx::query("UPDATE jobs SET state='failed', finished_at=$1, error='{\"code\":\"interrupted\",\"message\":\"service restarted while the job was running\",\"stage\":\"restart\"}' WHERE state='running'")
                .bind(*at).execute(&mut *conn).await?;
            sqlx::query("UPDATE request_logs SET execution_status='failed', execution_stage='interrupted' WHERE job_id IN (SELECT job_id FROM jobs WHERE state='failed' AND finished_at=$1)")
                .bind(*at).execute(&mut *conn).await?;
            sqlx::query("UPDATE request_logs SET request_status='interrupted', finished_at=$1 WHERE request_status='running'")
                .bind(*at)
                .execute(&mut *conn)
                .await?;
        }
        WriteOp::Retention { result_cutoff, history_cutoff, limit } => {
            sqlx::query("DELETE FROM result_cache WHERE cache_key IN (SELECT cache_key FROM result_cache WHERE created_at < $1 LIMIT $2)")
                .bind(*result_cutoff)
                .bind(*limit)
                .execute(&mut *conn)
                .await?;
            // Jobs with webhooks still pending are kept until delivery finishes.
            sqlx::query("DELETE FROM jobs WHERE job_id IN (SELECT job_id FROM jobs WHERE finished_at < $1 AND job_id NOT IN (SELECT job_id FROM webhook_deliveries WHERE status='pending') LIMIT $2)")
                .bind(*result_cutoff).bind(*limit).execute(&mut *conn).await?;
            sqlx::query("UPDATE request_logs SET result_available=0 WHERE seq IN (SELECT seq FROM request_logs WHERE result_available=1 AND finished_at < $1 LIMIT $2)")
                .bind(*result_cutoff).bind(*limit).execute(&mut *conn).await?;
            sqlx::query("DELETE FROM request_events WHERE id IN (SELECT id FROM request_events WHERE at < $1 LIMIT $2)")
                .bind(*history_cutoff)
                .bind(*limit)
                .execute(&mut *conn)
                .await?;
            sqlx::query("DELETE FROM llm_calls WHERE id IN (SELECT id FROM llm_calls WHERE created_at < $1 LIMIT $2)")
                .bind(*history_cutoff)
                .bind(*limit)
                .execute(&mut *conn)
                .await?;
            sqlx::query("DELETE FROM webhook_deliveries WHERE event_id IN (SELECT event_id FROM webhook_deliveries WHERE status <> 'pending' AND created_at < $1 LIMIT $2)")
                .bind(*history_cutoff).bind(*limit).execute(&mut *conn).await?;
            sqlx::query("DELETE FROM request_logs WHERE seq IN (SELECT seq FROM request_logs WHERE created_at < $1 LIMIT $2)")
                .bind(*history_cutoff)
                .bind(*limit)
                .execute(&mut *conn)
                .await?;
        }
    }
    Ok(())
}
