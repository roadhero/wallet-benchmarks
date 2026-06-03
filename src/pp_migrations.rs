//! Apply the vendored PP sqlite migrations into a fresh `payments.db`.
//!
//! Per `analysis/specs/MODE_3_REWORK_SPEC.md §3`, Mode 3 owns the lifecycle
//! of PP's sqlite database: the harness wipes the per-mode data dir on
//! startup, this module re-applies the six migration files vendored under
//! `vendor/minotari_payment_processor/migrations/`, and PP then opens the
//! ready-to-use database via its `DATABASE_URL` env var.
//!
//! Decision (per spec §3): `rusqlite` direct, no `sqlx-cli`. `rusqlite` is
//! already a dep with the `bundled` feature; `sqlx-cli` would be a separate
//! binary the operator has to install. The migrations are pure schema
//! (CREATE TABLE / CREATE INDEX) and need none of sqlx's compile-time
//! macros.
//!
//! Migration set is `include_str!`'d at compile time from the submodule, so
//! `cargo build` re-reads the files automatically when the submodule is
//! bumped. The submodule must be initialised
//! (`git submodule update --init --recursive`) before `cargo build` — same
//! caveat as any vendored content.

use std::path::Path;

use anyhow::Context;

const LOG_TARGET: &str = "c::pp_migrations";

/// The six vendored migration files, in lexical apply order. Verbatim mirror
/// of `vendor/minotari_payment_processor/migrations/` at submodule commit
/// `f0572c9`.
const MIGRATIONS: &[(&str, &str)] = &[
    (
        "20251017081013_init",
        include_str!(
            "../vendor/minotari_payment_processor/migrations/20251017081013_init.sql"
        ),
    ),
    (
        "20251201115213_add_intermediate_context",
        include_str!(
            "../vendor/minotari_payment_processor/migrations/20251201115213_add_intermediate_context.sql"
        ),
    ),
    (
        "20251209145502_add_payref_to_payments",
        include_str!(
            "../vendor/minotari_payment_processor/migrations/20251209145502_add_payref_to_payments.sql"
        ),
    ),
    (
        "20260102132241_add_index_to_payref",
        include_str!(
            "../vendor/minotari_payment_processor/migrations/20260102132241_add_index_to_payref.sql"
        ),
    ),
    (
        "20260123142522_add_events_table",
        include_str!(
            "../vendor/minotari_payment_processor/migrations/20260123142522_add_events_table.sql"
        ),
    ),
    (
        "20260126120000_add_block_headers_table",
        include_str!(
            "../vendor/minotari_payment_processor/migrations/20260126120000_add_block_headers_table.sql"
        ),
    ),
];

/// Tables the post-migration schema is required to contain. Spec §3 step 5
/// asserts each is present before returning success.
const REQUIRED_TABLES: &[&str] = &["payments", "payment_batches", "events", "block_headers"];

/// Open `<data_dir>/payments.db` and apply every vendored migration in
/// lexical order. Returns `Ok(())` when the post-migration schema contains
/// every entry in [`REQUIRED_TABLES`].
///
/// The caller (`PpLifecycle::new`) is responsible for wiping the data dir
/// before calling this — `rusqlite` will fail on duplicate `CREATE TABLE`
/// statements if the database already exists with schema. The wipe-vs-apply
/// split keeps each module's responsibility surgical.
pub fn apply_migrations(data_dir: &Path) -> anyhow::Result<()> {
    let db_path = data_dir.join("payments.db");
    log::info!(
        target: LOG_TARGET,
        "applying {} migrations into {}",
        MIGRATIONS.len(),
        db_path.display(),
    );
    let conn = rusqlite::Connection::open(&db_path)
        .with_context(|| format!("opening sqlite at {}", db_path.display()))?;
    // PP relies on foreign-key cascade behaviour (vendored 0001_init).
    // Enable here for parity with PP's own runtime PRAGMA.
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .context("enabling foreign_keys pragma")?;
    for (name, sql) in MIGRATIONS {
        log::debug!(target: LOG_TARGET, "applying migration {name}");
        conn.execute_batch(sql)
            .with_context(|| format!("applying migration {name}"))?;
    }
    // Verify the schema landed.
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .context("preparing post-migration table query")?;
    let table_names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .context("querying sqlite_master")?
        .collect::<Result<_, _>>()
        .context("collecting table names")?;
    for required in REQUIRED_TABLES {
        anyhow::ensure!(
            table_names.iter().any(|n| n == required),
            "post-migration sqlite at {} missing required table {required}; got {table_names:?}",
            db_path.display(),
        );
    }
    log::info!(
        target: LOG_TARGET,
        "applied migrations cleanly ({} tables present)",
        table_names.len(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Returns a (tempdir, db_path) pair. The tempdir is returned so the
    /// caller keeps it alive for the duration of the test — dropping it
    /// removes the directory and the sqlite file.
    fn fresh_data_dir() -> (TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("payments.db");
        (dir, db_path)
    }

    #[test]
    fn apply_migrations_creates_payments_table() {
        let (dir, db_path) = fresh_data_dir();
        apply_migrations(dir.path()).expect("apply_migrations succeeds");
        let conn = rusqlite::Connection::open(&db_path).expect("open db");
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='payments'",
                [],
                |row| row.get(0),
            )
            .expect("count payments table");
        assert_eq!(n, 1, "payments table must be present after migrations");
    }

    #[test]
    fn apply_migrations_creates_all_required_tables() {
        let (dir, db_path) = fresh_data_dir();
        apply_migrations(dir.path()).expect("apply_migrations succeeds");
        let conn = rusqlite::Connection::open(&db_path).expect("open db");
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .expect("prepare");
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("collect");
        for required in REQUIRED_TABLES {
            assert!(
                names.iter().any(|n| n == required),
                "post-migration schema must contain {required}; got {names:?}",
            );
        }
    }

    #[test]
    fn apply_migrations_is_idempotent_when_re_run() {
        // Per spec §13: "second call against the same DB doesn't error
        // (rusqlite will error on duplicate CREATE; spec calls for the
        // harness to wipe + re-create, so this test verifies wipe-first
        // behaviour)." Wipe first, then re-apply, then assert success
        // and the required tables are still present.
        let (dir, db_path) = fresh_data_dir();
        apply_migrations(dir.path()).expect("first apply");
        // Wipe the database file (mirrors the harness's wipe-then-apply
        // contract documented on apply_migrations' rustdoc).
        std::fs::remove_file(&db_path).expect("remove payments.db");
        apply_migrations(dir.path()).expect("second apply after wipe");
        let conn = rusqlite::Connection::open(&db_path).expect("re-open db");
        for required in REQUIRED_TABLES {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [required],
                    |row| row.get(0),
                )
                .expect("count required table");
            assert_eq!(
                n, 1,
                "{required} must still be present after wipe + re-apply",
            );
        }
    }
}
