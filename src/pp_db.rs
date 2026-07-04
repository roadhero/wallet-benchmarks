//! Provision the empty `payments.db` the PP daemon migrates on boot.
//!
//! The PP daemon runs its OWN embedded migrations at startup
//! (`vendor/minotari_payment_processor/minotari_payment_processor/src/db/mod.rs:12`:
//! `sqlx::migrate!("../migrations").run(&pool)`), but its sqlite opener does
//! not create the database file: `SqlitePoolOptions::connect` uses the
//! default `create_if_missing = false`, so a missing file exits the daemon
//! with `(code: 14) unable to open database file`.
//!
//! So the harness hands PP an empty-but-existing sqlite file and lets PP own
//! the schema. An earlier version of this module applied the six vendored
//! migrations here; that collided with PP's own migrator, which re-ran every
//! migration against the already-populated schema and exited 1 during the
//! readiness probe with `duplicate column name: intermediate_context_json`
//! (from `20251201115213_add_intermediate_context`). Verified live: PP boots
//! and `GET /health/version` returns 200 against a freshly-created empty
//! `payments.db`.
//!
//! The caller wipes the per-mode data dir before spawning PP, so this only
//! ever creates a brand-new file.

use std::path::Path;

use anyhow::Context;

const LOG_TARGET: &str = "c::pp_db";

/// Create an empty `<data_dir>/payments.db` for the PP daemon to migrate on
/// boot. Opening a `rusqlite` connection with the default create flag and
/// touching the header writes a valid (empty) sqlite database the daemon's
/// `sqlx` opener can then open and migrate.
pub fn create_empty_db(data_dir: &Path) -> anyhow::Result<()> {
    let db_path = data_dir.join("payments.db");
    log::info!(
        target: LOG_TARGET,
        "creating empty PP database at {} (PP migrates it on boot)",
        db_path.display(),
    );
    let conn = rusqlite::Connection::open(&db_path)
        .with_context(|| format!("creating empty sqlite at {}", db_path.display()))?;
    // Write the file header to disk so PP's sqlx opener sees a valid (empty)
    // database rather than a zero-byte file. Setting user_version is a cheap
    // write that materialises page 1.
    conn.execute_batch("PRAGMA user_version = 0;")
        .context("initialising empty PP database header")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_empty_db_creates_a_valid_empty_sqlite_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        create_empty_db(dir.path()).expect("create_empty_db succeeds");
        let db_path = dir.path().join("payments.db");
        assert!(db_path.exists(), "payments.db must exist");
        // Re-open and confirm it is a valid sqlite db with no application
        // tables; PP owns the schema and the harness must not pre-create it.
        let conn = rusqlite::Connection::open(&db_path).expect("re-open");
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE type='table' AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )
            .expect("count tables");
        assert_eq!(n, 0, "harness must hand PP an empty schema, got {n} tables");
    }

    #[test]
    fn create_empty_db_is_safe_to_call_twice() {
        // Production wipes the data dir first, but a redundant call against an
        // existing empty file must not error.
        let dir = tempfile::tempdir().expect("tempdir");
        create_empty_db(dir.path()).expect("first create");
        create_empty_db(dir.path()).expect("second create is harmless");
    }
}
