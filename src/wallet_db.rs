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
    /// funds.
    ///
    /// Maturity: the count also requires `maturity <= scanned tip`, mirroring
    /// the upstream input selector's clause (`minotari-cli@52a7287a
    /// minotari/src/db/outputs.rs::fetch_unspent_outputs`, `maturity >= 0 AND
    /// maturity <= :tip_height`). A coinbase output carries
    /// `maturity = mined_height + lock` (observed +6 on Esmeralda) and is
    /// confirmed several blocks before it becomes selectable; without this
    /// clause the gate reports ready while `create-unsigned-transaction`
    /// still fails "Funds are pending". The tip is the wallet's own view,
    /// `MAX(height)` over `scanned_tip_blocks` (0 when never scanned).
    /// Returns `Ok(0)` when the DB file does not yet exist.
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
        // mined plus mature equals "lockable by the next send". The maturity
        // clause and its `>= 0` wrap-guard mirror the selector verbatim
        // (upstream outputs.rs::fetch_unspent_outputs); the tip is the
        // wallet's own scanned view so the whole predicate stays a single
        // self-contained sqlite read.
        //
        // Schema compatibility: the `maturity` column only exists from
        // minotari-cli migration `00031-add_maturity_to_outputs` (introduced
        // by the pinned commit itself). A wallet DB created by an older
        // `minotari` binary has no such column, and the maturity clause
        // errors instantly - observed live as a maintainer's S0 erring in
        // 0.2 s straight after a clean 101-minute B0 (2026-07-31 report).
        // On that schema, fall back to the pre-maturity predicate with a
        // warning: an older CLI's own selector has no maturity clause
        // either, so the fallback matches what that binary will actually
        // spend.
        if has_maturity_schema(&conn) {
            let scanned_tip: i64 = conn
                .query_row(
                    "SELECT COALESCE(MAX(height), 0) FROM scanned_tip_blocks",
                    [],
                    |row| row.get(0),
                )
                .with_context(|| format!("scanned_tip_blocks query on {}", db_path.display()))?;
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM outputs \
                     WHERE deleted_at IS NULL AND is_burn = 0 AND status = ?1 \
                     AND confirmed_height IS NOT NULL \
                     AND maturity >= 0 AND maturity <= ?2",
                    rusqlite::params!["UNSPENT", scanned_tip],
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
        } else {
            log::warn!(
                target: LOG_TARGET,
                "wallet DB at {} has no `maturity` column (minotari binary \
                 predates migration 00031): counting confirmed spendable \
                 outputs without the maturity clause. Coinbase maturity \
                 cannot be respected on this schema - build the minotari \
                 CLI at the pinned commit (52a7287a) or later.",
                db_path.display(),
            );
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
                        "count_confirmed_spendable_utxos (pre-00031 schema) query on {}",
                        db_path.display()
                    )
                })?;
            #[allow(clippy::cast_sign_loss)]
            Ok(n.max(0) as u64)
        }
    }
}

/// True when the wallet DB carries the `outputs.maturity` column
/// (minotari-cli migration `00031-add_maturity_to_outputs`). Probed via
/// PRAGMA so the caller can pick a predicate the schema supports instead
/// of erroring on older wallets.
fn has_maturity_schema(conn: &Connection) -> bool {
    conn.prepare("SELECT maturity FROM outputs LIMIT 0").is_ok()
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
    ///   - `00031-add_maturity_to_outputs/up.sql` — maturity INTEGER NOT NULL
    ///     DEFAULT 0
    ///   - `00001-init/up.sql` — scanned_tip_blocks (height column read as
    ///     the wallet's tip view by the maturity clause)
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
            confirmed_height INTEGER,
            maturity INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE scanned_tip_blocks (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            account_id INTEGER NOT NULL DEFAULT 1,
            height INTEGER NOT NULL DEFAULT 0,
            hash BLOB NOT NULL DEFAULT x''
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

    /// Insert one row with explicit `confirmed_height` and `maturity`, and
    /// record the wallet's scanned tip — the coinbase-shaped fixture.
    fn insert_row_with_maturity(
        conn: &Connection,
        status: &str,
        confirmed_height: Option<i64>,
        maturity: i64,
    ) {
        conn.execute(
            "INSERT INTO outputs (status, deleted_at, is_burn, confirmed_height, maturity) \
             VALUES (?1, NULL, 0, ?2, ?3)",
            rusqlite::params![status, confirmed_height, maturity],
        )
        .expect("insert row with maturity");
    }

    fn set_scanned_tip(conn: &Connection, height: i64) {
        conn.execute(
            "INSERT INTO scanned_tip_blocks (height) VALUES (?1)",
            rusqlite::params![height],
        )
        .expect("insert scanned tip");
    }

    /// The live-observed coinbase shape (Esmeralda block 755167: confirmed
    /// at +3, maturity mined+6): confirmed_height set while maturity is
    /// still above the scanned tip. The selector refuses it, so the count
    /// must too.
    #[test]
    fn count_confirmed_spendable_excludes_immature_coinbase() {
        let (_dir, db_path) = fresh_db();
        let conn = Connection::open(&db_path).expect("re-open");
        insert_row_with_maturity(&conn, "UNSPENT", Some(755_170), 755_173);
        set_scanned_tip(&conn, 755_171);
        let db = LiveWalletDb;
        assert_eq!(
            db.count_confirmed_spendable_utxos(&db_path).expect("count"),
            0,
            "confirmed-but-immature coinbase is not selectable",
        );
        // Tip advances past maturity: now selectable.
        set_scanned_tip(&conn, 755_173);
        assert_eq!(
            db.count_confirmed_spendable_utxos(&db_path).expect("count"),
            1,
            "mature coinbase counts once the scanned tip reaches maturity",
        );
    }

    /// Standard outputs carry maturity = 0 (migration 00031 comment) and
    /// must count even when the wallet has never recorded a scanned tip
    /// (COALESCE(MAX(height), 0) = 0 >= 0).
    #[test]
    fn count_confirmed_spendable_zero_maturity_counts_without_scanned_tip() {
        let (_dir, db_path) = fresh_db();
        let conn = Connection::open(&db_path).expect("re-open");
        insert_row_with_maturity(&conn, "UNSPENT", Some(100), 0);
        let db = LiveWalletDb;
        assert_eq!(
            db.count_confirmed_spendable_utxos(&db_path).expect("count"),
            1,
        );
    }

    /// Negative maturity models the upstream wrap-guard (`maturity >= 0`):
    /// a u64 that wrapped to negative i64 must be excluded, matching
    /// fetch_unspent_outputs' comment about inverted comparisons.
    #[test]
    fn count_confirmed_spendable_excludes_wrapped_negative_maturity() {
        let (_dir, db_path) = fresh_db();
        let conn = Connection::open(&db_path).expect("re-open");
        insert_row_with_maturity(&conn, "UNSPENT", Some(100), -1);
        set_scanned_tip(&conn, 1_000_000);
        let db = LiveWalletDb;
        assert_eq!(
            db.count_confirmed_spendable_utxos(&db_path).expect("count"),
            0,
        );
    }

    /// The maintainer's 2026-07-31 shape: a wallet DB created by a
    /// minotari binary older than migration 00031 (no `maturity` column,
    /// no `scanned_tip_blocks` data guarantees). The count must fall back
    /// to the pre-maturity predicate instead of erroring instantly.
    #[test]
    fn count_confirmed_spendable_falls_back_on_pre_00031_schema() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("wallet.sqlite3");
        let conn = Connection::open(&db_path).expect("create DB");
        conn.execute_batch(
            r#"
            CREATE TABLE outputs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                status TEXT NOT NULL DEFAULT 'UNSPENT',
                deleted_at TIMESTAMP,
                is_burn INTEGER NOT NULL DEFAULT 0,
                confirmed_height INTEGER
            );
            "#,
        )
        .expect("apply pre-00031 schema");
        conn.execute(
            "INSERT INTO outputs (status, deleted_at, is_burn, confirmed_height) \
             VALUES ('UNSPENT', NULL, 0, 725000)",
            [],
        )
        .expect("seed confirmed row");
        conn.execute(
            "INSERT INTO outputs (status, deleted_at, is_burn, confirmed_height) \
             VALUES ('UNSPENT', NULL, 0, NULL)",
            [],
        )
        .expect("seed unconfirmed row");
        drop(conn);
        let db = LiveWalletDb;
        assert_eq!(
            db.count_confirmed_spendable_utxos(&db_path).expect("count"),
            1,
            "pre-00031 schema counts confirmed UNSPENT without maturity",
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
