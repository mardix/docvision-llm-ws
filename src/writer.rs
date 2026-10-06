//! Single DB writer task with group commit: drains up to 500 ops or 10 ms, commits once.

use crate::db::{self, Db, WriteOp};
use crate::metrics::Metrics;
use sqlx::Connection;
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

const MAX_BATCH: usize = 500;
const WINDOW: Duration = Duration::from_millis(10);
const CHANNEL: usize = 16_384;

#[allow(clippy::large_enum_variant)] // ops are moved, not copied; boxing would add an allocation per op
enum Msg {
    One(WriteOp, Instant),
    Durable(Vec<WriteOp>, Instant, oneshot::Sender<bool>),
}

#[derive(Clone)]
pub struct Writer {
    tx: mpsc::Sender<Msg>,
    metrics: Arc<Metrics>,
}

impl Writer {
    pub async fn start(db: &Db, metrics: Arc<Metrics>) -> sqlx::Result<(Writer, tokio::task::JoinHandle<()>)> {
        let conn = db.writer_conn().await?;
        let (tx, rx) = mpsc::channel(CHANNEL);
        let m = metrics.clone();
        let handle = tokio::spawn(run(conn, rx, m));
        Ok((Writer { tx, metrics }, handle))
    }

    /// Fire-and-forget. Returns false (and logs a structured fallback) if the writer is saturated or down.
    pub fn send(&self, op: WriteOp) -> bool {
        match self.tx.try_send(Msg::One(op, Instant::now())) {
            Ok(()) => true,
            Err(e) => {
                self.metrics.writer_dropped.fetch_add(1, Relaxed);
                let op = match e {
                    mpsc::error::TrySendError::Full(Msg::One(op, _)) | mpsc::error::TrySendError::Closed(Msg::One(op, _)) => Some(op),
                    _ => None,
                };
                tracing::warn!(target: "docvision_llm_ws::writer", fallback = true, op = ?op.map(|o| op_name(&o)), "db writer unavailable; write dropped");
                false
            }
        }
    }

    /// Commit `ops` in the next batch and wait for the commit. Fails fast if the channel is full.
    pub async fn send_durable(&self, ops: Vec<WriteOp>) -> bool {
        let (ack, rx) = oneshot::channel();
        if self.tx.try_send(Msg::Durable(ops, Instant::now(), ack)).is_err() {
            self.metrics.writer_dropped.fetch_add(1, Relaxed);
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Wait until everything queued before this call is committed.
    pub async fn flush(&self) -> bool {
        let (ack, rx) = oneshot::channel();
        if self.tx.send(Msg::Durable(Vec::new(), Instant::now(), ack)).await.is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    pub fn lag_ms(&self) -> u64 {
        self.metrics.writer_lag_ms.load(Relaxed)
    }

    pub fn is_alive(&self) -> bool {
        !self.tx.is_closed()
    }

    pub fn pending(&self) -> usize {
        CHANNEL - self.tx.capacity()
    }
}

fn op_name(op: &WriteOp) -> &'static str {
    match op {
        WriteOp::InsertRequest(_) => "insert_request",
        WriteOp::FinishRequest(_) => "finish_request",
        WriteOp::RequestExecution { .. } => "request_execution",
        WriteOp::InsertJob(_) => "insert_job",
        WriteOp::JobStarted { .. } => "job_started",
        WriteOp::JobFinished { .. } => "job_finished",
        WriteOp::Events(_) => "events",
        WriteOp::Call { .. } => "call",
        WriteOp::CachePut { .. } => "cache_put",
        WriteOp::CacheDelete { .. } => "cache_delete",
        WriteOp::Webhook(_) => "webhook",
        WriteOp::Recover { .. } => "recover",
        WriteOp::Retention { .. } => "retention",
    }
}

async fn commit(conn: &mut sqlx::AnyConnection, batch: &[Msg]) -> sqlx::Result<()> {
    let mut tx = conn.begin().await?;
    for m in batch {
        match m {
            Msg::One(op, _) => db::apply(&mut tx, op).await?,
            Msg::Durable(ops, _, _) => {
                for op in ops {
                    db::apply(&mut tx, op).await?;
                }
            }
        }
    }
    tx.commit().await
}

async fn run(mut conn: sqlx::AnyConnection, mut rx: mpsc::Receiver<Msg>, metrics: Arc<Metrics>) {
    let mut batch: Vec<Msg> = Vec::with_capacity(MAX_BATCH);
    while let Some(first) = rx.recv().await {
        let deadline = tokio::time::Instant::now() + WINDOW;
        let mut n_ops = count(&first);
        batch.push(first);
        while n_ops < MAX_BATCH {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(m)) => {
                    n_ops += count(&m);
                    batch.push(m);
                }
                _ => break,
            }
        }
        let oldest = batch
            .iter()
            .map(|m| match m {
                Msg::One(_, t) | Msg::Durable(_, t, _) => *t,
            })
            .min();
        let results: Vec<bool> = match commit(&mut conn, &batch).await {
            Ok(()) => vec![true; batch.len()],
            Err(e) => {
                tracing::error!(target: "docvision_llm_ws::writer", error = %e, "batch commit failed; retrying ops individually");
                // Isolate the poison op: commit each message on its own.
                let mut r = Vec::with_capacity(batch.len());
                for m in batch.iter() {
                    let res = commit(&mut conn, std::slice::from_ref(m)).await;
                    if let Err(e) = &res {
                        tracing::error!(target: "docvision_llm_ws::writer", error = %e, "write op failed");
                    }
                    r.push(res.is_ok());
                }
                r
            }
        };
        metrics.writer_batches.fetch_add(1, Relaxed);
        metrics.writer_ops.fetch_add(n_ops as u64, Relaxed);
        metrics.writer_last_batch.store(n_ops as u64, Relaxed);
        if let Some(t) = oldest {
            metrics.writer_lag_ms.store(t.elapsed().as_millis() as u64, Relaxed);
        }
        for (m, ok) in batch.drain(..).zip(results) {
            if let Msg::Durable(_, _, ack) = m {
                let _ = ack.send(ok);
            }
        }
    }
    let _ = conn.close().await;
}

fn count(m: &Msg) -> usize {
    match m {
        Msg::One(..) => 1,
        Msg::Durable(ops, ..) => ops.len().max(1),
    }
}
