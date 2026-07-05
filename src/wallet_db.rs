//! Read-only wallet sqlite3 query surface for Mode 2 / Mode 3.
//!
//! Per PR #6 review threads 4.1 + 4.3: the canonical source of UTXO counts
//! for the new `minotari` CLI is the wallet's own sqlite3 DB at
//! `<data_dir>/wallet.sqlite3`, not the rejected stderr `event_count` parse.
//! Mode 1 (`old_wallet`) is gRPC-based and does NOT route through this
//! module — see `crate::modes::old_wallet` for that path.
//!
//! Schema source: `tari-project/minotari-cli@52a7287a` migrations
//! `00001-init` … `00031-add_maturity_to_outputs`. The predicate set
//! `WHERE deleted_at IS NULL AND is_burn = 0` matches the upstream
//! `minotari/src/db/outputs.rs::get_output_totals_for_account` (line 616).
//!
//! See `analysis/specs/THREADS_4_1_4_3_SPEC.md` for the full design.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

const LOG_TARGET: &str = "c::wallet_db";

/// Read-only query surface for the Mode 2/3 wallet sqlite3 DB.
///
/// Trait-shaped so scenario/mode unit tests can inject a fake without
/// spinning up a real sqlite3 file. The production [`LiveWalletDb`] impl
/// uses `rusqlite`; tests construct a [`FakeWalletDb`] under `#[cfg(test)]`.
pub trait WalletDb: Send + Sync {
    /// Total wallet output count — Q1 per the spec. Returns `Ok(0)` when
    /// the DB file does not yet exist (e.g. B0 pre-create state).
    fn count_outputs(&self, db_path: &Path) -> Result<u64>;

    /// Spendable UTXO count — Q2 per the spec. Returns `Ok(0)` when
    /// the DB file does not yet exist.
    fn count_spendable_utxos(&self, db_path: &Path) -> Result<u64>;

    /// Count of outputs that are actually spendable *now* by the `minotari`
    /// input selector: UNSPENT, not locked, and confirmed (`confirmed_height`
    /// set once the output is buried by the confirmation window). This
    /// differs from [`Self::count_spendable_utxos`], which counts every
    /// UNSPENT row including outputs a scan has seen mined but not yet
    /// buried (`confirmed_height` still NULL). Note the rows themselves are
    /// created only by scans; a send only locks inputs and writes a
    /// `pending_transactions` row. S1's settle-between-sends gate uses this
    /// count so it does not mistake not-yet-confirmed outputs for spendable
    /// funds. Returns `Ok(0)` when the DB file does not yet exist.
    fn count_confirmed_spendable_utxos(&self, db_path: &Path) -> Result<u64>;
}

/// Convenience alias for an injectable [`WalletDb`] handle. `Arc` lets the
/// `Mode` impls store a clone-able trait object while keeping the
/// `Send + Sync` bound that `#[async_trait]` requires.
pub type WalletDbArc = Arc<dyn WalletDb>;

/// Production [`WalletDb`] impl. Opens the DB read-only per call; queries
/// are cheap (`COUNT(*)` against an indexed predicate) and the harness
/// invokes them at most a handful of times per scenario.
#[derive(Debug, Default, Clone, Copy)]
pub struct LiveWalletDb;

impl WalletDb for LiveWalletDb {
    fn count_outputs(&self, db_path: &Path) -> Result<u64> {
        if !db_path.exists() {
            log::debug!(
                target: LOG_TARGET,
                "count_outputs: DB file missing at {} — returning 0",
                db_path.display(),
            );
            return Ok(0);
        }
        let conn = open_read_only(db_path)?;
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM outputs WHERE deleted_at IS NULL AND is_burn = 0",
                [],
                |row| row.get(0),
            )
            .with_context(|| format!("count_outputs query on {}", db_path.display()))?;
        // `COUNT(*)` is non-negative by spec; the clamp guards against the
        // impossible.
        #[allow(clippy::cast_sign_loss)]
        Ok(n.max(0) as u64)
    }

    fn count_spendable_utxos(&self, db_path: &Path) -> Result<u64> {
        if !db_path.exists() {
            log::debug!(
                target: LOG_TARGET,
                "count_spendable_utxos: DB file missing at {} — returning 0",
                db_path.display(),
            );
            return Ok(0);
        }
        let conn = open_read_only(db_path)?;
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM outputs \
                 WHERE deleted_at IS NULL AND is_burn = 0 AND status = ?1",
                rusqlite::params!["UNSPENT"],
                |row| row.get(0),
            )
            .with_context(|| format!("count_spendable_utxos query on {}", db_path.display()))?;
        #[allow(clippy::cast_sign_loss)]
        Ok(n.max(0) as u64)
    }

    fn count_confirmed_spendable_utxos(&self, db_path: &Path) -> Result<u64> {
        if !db_path.exists() {
            log::debug!(
                target: LOG_TARGET,
                "count_confirmed_spendable_utxos: DB file missing at {}; returning 0",
                db_path.display(),
            );
            return Ok(0);
        }
        let conn = open_read_only(db_path)?;
        // `confirmed_height IS NOT NULL` is the mined predicate the input
        // selector's "available" bucket uses (a NULL confirmed_height is the
        // unconfirmed/pending bucket). UNSPENT (not LOCKED, not SPENT) plus
        // mined equals "lockable by the next send".
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM outputs \
                 WHERE deleted_at IS NULL AND is_burn = 0 AND status = ?1 \
                 AND confirmed_height IS NOT NULL",
                rusqlite::params!["UNSPENT"],
                |row| row.get(0),
            )
            .with_context(|| {
                format!(
                    "count_confirmed_spendable_utxos query on {}",
                    db_path.display()
                )
            })?;
        #[allow(clippy::cast_sign_loss)]
        Ok(n.max(0) as u64)
    }
}

fn open_read_only(db_path: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening wallet DB read-only at {}", db_path.display()))
}

#[cfg(test)]
pub(crate) struct FakeWalletDb {
    pub canned_count_outputs: std::result::Result<u64, String>,
    pub canned_count_spendable: std::result::Result<u64, String>,
    pub canned_count_confirmed_spendable: std::result::Result<u64, String>,
}

#[cfg(test)]
impl FakeWalletDb {
    pub fn ok(count_outputs: u64, count_spendable: u64) -> Self {
        Self {
            canned_count_outputs: Ok(count_outputs),
            canned_count_spendable: Ok(count_spendable),
            // Default the confirmed-spendable count to the spendable count;
            // tests that exercise the settle gate override it explicitly.
            canned_count_confirmed_spendable: Ok(count_spendable),
        }
    }
}

#[cfg(test)]
impl WalletDb for FakeWalletDb {
    fn count_outputs(&self, _db_path: &Path) -> Result<u64> {
        match &self.canned_count_outputs {
            Ok(n) => Ok(*n),
            Err(msg) => anyhow::bail!("FakeWalletDb::count_outputs: {msg}"),
        }
    }
    fn count_spendable_utxos(&self, _db_path: &Path) -> Result<u64> {
        match &self.canned_count_spendable {
            Ok(n) => Ok(*n),
            Err(msg) => anyhow::bail!("FakeWalletDb::count_spendable_utxos: {msg}"),
        }
    }
    fn count_confirmed_spendable_utxos(&self, _db_path: &Path) -> Result<u64> {
        match &self.canned_count_confirmed_spendable {
            Ok(n) => Ok(*n),
            Err(msg) => anyhow::bail!("FakeWalletDb::count_confirmed_spendable_utxos: {msg}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Minimal subset of the upstream `outputs` table schema. The harness
    /// only queries `deleted_at`, `is_burn`, and `status` — the rest are
    /// listed here with sensible NOT NULL defaults so the schema is
    /// applied cleanly. Citations per migration file:
    ///   - `00001-init/up.sql` — base columns (id, account_id, output_hash,
    ///     mined_in_block_hash, mined_in_block_height, value, created_at)
    ///   - `00009-add_utxo_locking_to_outputs/up.sql` — status TEXT NOT NULL
    ///   - `00013-add_soft_delete_to_inputs_outputs/up.sql` — deleted_at TIMESTAMP
    ///   - `00029-add_is_burn_to_outputs/up.sql` — is_burn INTEGER NOT NULL
    const SCHEMA: &str = r#"
        CREATE TABLE outputs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            account_id INTEGER NOT NULL DEFAULT 1,
            output_hash BLOB NOT NULL DEFAULT x'',
            mined_in_block_hash BLOB NOT NULL DEFAULT x'',
            mined_in_block_height INTEGER NOT NULL DEFAULT 0,
            value INTEGER NOT NULL DEFAULT 0,
            created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            status TEXT NOT NULL DEFAULT 'UNSPENT',
            deleted_at TIMESTAMP,
            is_burn INTEGER NOT NULL DEFAULT 0,
            confirmed_height INTEGER
        );
    "#;

    /// Spawn an empty test DB at `<tempdir>/wallet.sqlite3` with the
    /// schema applied. Returns the tempdir (kept alive by the caller) and
    /// the DB path.
    fn fresh_db() -> (TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("wallet.sqlite3");
        let conn = Connection::open(&db_path).expect("create DB");
        conn.execute_batch(SCHEMA).expect("apply schema");
        (dir, db_path)
    }

    /// Insert one row with the supplied `(status, deleted_at, is_burn)`.
    /// `deleted_at` is `None` for live rows; pass `Some("2026-01-01")` (or
    /// any non-NULL TIMESTAMP string) to mark soft-deleted.
    fn insert_row(conn: &Connection, status: &str, deleted_at: Option<&str>, is_burn: i64) {
        conn.execute(
            "INSERT INTO outputs (status, deleted_at, is_burn) VALUES (?1, ?2, ?3)",
            rusqlite::params![status, deleted_at, is_burn],
        )
        .expect("insert row");
    }

    /// Insert one row with an explicit `confirmed_height` (`None` = the
    /// unconfirmed/pending bucket, a locally-created but not-yet-mined
    /// output; `Some(h)` = mined at height `h`).
    fn insert_row_with_height(conn: &Connection, status: &str, confirmed_height: Option<i64>) {
        conn.execute(
            "INSERT INTO outputs (status, deleted_at, is_burn, confirmed_height) \
             VALUES (?1, NULL, 0, ?2)",
            rusqlite::params![status, confirmed_height],
        )
        .expect("insert row with height");
    }

    #[test]
    fn count_outputs_empty_table_returns_zero() {
        let (_dir, db_path) = fresh_db();
        let db = LiveWalletDb;
        assert_eq!(db.count_outputs(&db_path).expect("count"), 0);
    }

    #[test]
    fn count_spendable_utxos_empty_table_returns_zero() {
        let (_dir, db_path) = fresh_db();
        let db = LiveWalletDb;
        assert_eq!(db.count_spendable_utxos(&db_path).expect("count"), 0);
    }

    #[test]
    fn count_outputs_counts_unspent_and_locked_and_spent() {
        let (_dir, db_path) = fresh_db();
        let conn = Connection::open(&db_path).expect("re-open");
        insert_row(&conn, "UNSPENT", None, 0);
        insert_row(&conn, "LOCKED", None, 0);
        insert_row(&conn, "SPENT", None, 0);
        let db = LiveWalletDb;
        assert_eq!(db.count_outputs(&db_path).expect("count"), 3);
    }

    #[test]
    fn count_spendable_utxos_filters_to_unspent() {
        let (_dir, db_path) = fresh_db();
        let conn = Connection::open(&db_path).expect("re-open");
        insert_row(&conn, "UNSPENT", None, 0);
        insert_row(&conn, "LOCKED", None, 0);
        insert_row(&conn, "SPENT", None, 0);
        let db = LiveWalletDb;
        assert_eq!(db.count_spendable_utxos(&db_path).expect("count"), 1);
    }

    #[test]
    fn count_confirmed_spendable_excludes_unmined_and_non_unspent() {
        let (_dir, db_path) = fresh_db();
        let conn = Connection::open(&db_path).expect("re-open");
        // Mined + UNSPENT: spendable now.
        insert_row_with_height(&conn, "UNSPENT", Some(725_000));
        // UNSPENT but not yet mined (pending change): NOT spendable, though
        // count_spendable_utxos (status-only) would wrongly include it.
        insert_row_with_height(&conn, "UNSPENT", None);
        // Mined but LOCKED / SPENT: not available to the next send.
        insert_row_with_height(&conn, "LOCKED", Some(725_000));
        insert_row_with_height(&conn, "SPENT", Some(725_000));
        let db = LiveWalletDb;
        assert_eq!(
            db.count_spendable_utxos(&db_path).expect("count"),
            2,
            "status-only count includes the unmined pending change",
        );
        assert_eq!(
            db.count_confirmed_spendable_utxos(&db_path).expect("count"),
            1,
            "confirmed-spendable count excludes the unmined pending change",
        );
    }

    #[test]
    fn count_confirmed_spendable_empty_table_returns_zero() {
        let (_dir, db_path) = fresh_db();
        let db = LiveWalletDb;
        assert_eq!(
            db.count_confirmed_spendable_utxos(&db_path).expect("count"),
            0,
        );
    }

    #[test]
    fn count_outputs_excludes_soft_deleted() {
        let (_dir, db_path) = fresh_db();
        let conn = Connection::open(&db_path).expect("re-open");
        insert_row(&conn, "UNSPENT", None, 0);
        insert_row(&conn, "UNSPENT", Some("2026-01-01 00:00:00"), 0);
        let db = LiveWalletDb;
        assert_eq!(db.count_outputs(&db_path).expect("count"), 1);
        assert_eq!(db.count_spendable_utxos(&db_path).expect("count"), 1);
    }

    #[test]
    fn count_outputs_excludes_burn_outputs() {
        let (_dir, db_path) = fresh_db();
        let conn = Connection::open(&db_path).expect("re-open");
        insert_row(&conn, "UNSPENT", None, 0);
        insert_row(&conn, "UNSPENT", None, 1);
        let db = LiveWalletDb;
        assert_eq!(db.count_outputs(&db_path).expect("count"), 1);
        assert_eq!(db.count_spendable_utxos(&db_path).expect("count"), 1);
    }

    #[test]
    fn count_outputs_missing_db_returns_zero() {
        let db = LiveWalletDb;
        assert_eq!(
            db.count_outputs(Path::new("/nonexistent/path/wallet.sqlite3"))
                .expect("missing DB → Ok(0)"),
            0,
        );
    }

    #[test]
    fn count_spendable_utxos_missing_db_returns_zero() {
        let db = LiveWalletDb;
        assert_eq!(
            db.count_spendable_utxos(Path::new("/nonexistent/path/wallet.sqlite3"))
                .expect("missing DB → Ok(0)"),
            0,
        );
    }

    #[test]
    fn count_outputs_corrupted_db_surfaces_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("garbage.sqlite3");
        std::fs::write(&db_path, b"not a sqlite database").expect("write garbage");
        let db = LiveWalletDb;
        let err = db.count_outputs(&db_path).expect_err("must error");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("count_outputs query") || msg.contains("opening wallet DB"),
            "error must surface the operation context: {msg}",
        );
    }
}
