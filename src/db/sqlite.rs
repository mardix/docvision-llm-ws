//! SQLite specifics: pragmas for the writer connection and a read-only pool of 4.

use sqlx::any::AnyPoolOptions;
use sqlx::{AnyConnection, AnyPool, Executor};

const PRAGMAS: &str = "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA busy_timeout=5000; PRAGMA temp_store=MEMORY; PRAGMA mmap_size=268435456; PRAGMA foreign_keys=OFF;";

pub async fn configure(conn: &mut AnyConnection) -> sqlx::Result<()> {
    conn.execute(PRAGMAS).await?;
    Ok(())
}

pub async fn read_pool(url: &str) -> sqlx::Result<AnyPool> {
    AnyPoolOptions::new()
        .max_connections(4)
        .min_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                conn.execute("PRAGMA busy_timeout=5000; PRAGMA temp_store=MEMORY; PRAGMA mmap_size=268435456; PRAGMA query_only=ON;")
                    .await?;
                Ok(())
            })
        })
        .connect(url)
        .await
}
