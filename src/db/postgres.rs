//! PostgreSQL support (`postgres` feature). The repository in `db/mod.rs` uses a portable
//! dialect (`$N` placeholders, `ON CONFLICT` upserts), so this module only documents the
//! backend and provides the opt-in contract-test URL.

/// URL of the PostgreSQL instance used by the shared repository contract suite, if any.
pub fn contract_test_url() -> Option<String> {
    std::env::var("DOCVISION_TEST_POSTGRES_URL").ok().filter(|s| !s.is_empty())
}
