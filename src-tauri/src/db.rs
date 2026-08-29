//! `sts.db`: the connection, the schema, and everything that appends to it.
//!
//! This side owns the schema now. The Node watcher that used to create these
//! tables is archived under `docs/archive/legacy-node`, so a fresh checkout has
//! no `sts.db` until this file makes one, and `docs/architecture/SCHEMA.md` is
//! the description this implements.
//!
//! Three things here are worth knowing before changing anything:
//!
//! **One writer.** SQLite in WAL mode allows exactly one writer at a time. The
//! `Connection` below sits behind a mutex and every write in the process goes
//! through it, so contention cannot happen rather than being handled. Readers —
//! the status snapshot, anything doing forensics — are free: WAL gives them a
//! consistent snapshot from the moment their statement started and they are
//! never blocked by the writer.
//!
//! **The pragmas are per-connection, not per-file.** Only `journal_mode` is
//! stored in the file itself. Everything else in `CONNECTION_PRAGMAS` has to be
//! set again by the next connection that opens, including `foreign_keys`, which
//! SQLite defaults off forever for backwards compatibility.
//!
//! **Migrations run before anything reads.** `open` applies every numbered
//! migration the build knows about and refuses a file that is newer than it is.
//! An old build reading a new schema is the failure that corrupts things
//! quietly, and not starting is the only safe reading of it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::EngineError;
use crate::strategy::fixed::Q18;
use crate::types::{
    AbortOutcome, AbortReason, ExecutionState, ExitFailure, ExitState, OperatingMode,
    SybilClusterMetrics, Venue,
};

/// How long the panic path is willing to wait for the connection before giving
/// up. A panic that happened while this lock was held would otherwise hang the
/// process on the way out, which is the one thing a crash path must not do.
const PANIC_LOCK_TIMEOUT: Duration = Duration::from_millis(250);

/// A price as the `INTEGER` its column holds, or a refusal.
///
/// The same conversion `journal.rs` does for `journal_fills.price_q18` and the
/// same reason it refuses rather than saturating: past `i64::MAX` at `10^-18`
/// is nine lamports for one token base unit, and a price that large is a bug
/// upstream rather than a number to be quietly clamped into the file.
fn price_column(price: Q18) -> Result<i64, EngineError> {
    price.to_i64_raw().ok_or_else(|| {
        EngineError::Database(format!(
            "a price of {} at 10^-18 is past what a column holds",
            price.raw()
        ))
    })
}

/// A rate in millionths as the `INTEGER` its column holds.
fn store_rate(micros: u64) -> Result<i64, EngineError> {
    i64::try_from(micros).map_err(|_| {
        EngineError::Database(format!(
            "a rate of {micros} millionths is past what a column holds"
        ))
    })
}

/// How many rows one retention statement removes before committing and going
/// round again. One statement deleting a week of ticks is a single large
/// transaction that bloats the WAL and holds the writer; a few thousand rows at
/// a time keeps the pauses invisible.
const PRUNE_CHUNK: usize = 4_000;

/// Where the data lives. Honours `$STS_HOME`, exactly like the JSONL writer
/// does, so every part of the process agrees on one directory.
///
/// The fallback is resolved from this crate's location at compile time, which is
/// right for `cargo run` from a working copy and wrong for a bundled `.app`.
/// A packaged build is expected to set `STS_HOME`.
pub fn data_dir() -> PathBuf {
    match std::env::var_os("STS_HOME") {
        Some(home) => PathBuf::from(home),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .map(|root| root.join("data"))
            .unwrap_or_else(|| PathBuf::from("data")),
    }
}

/// The database file itself, under whichever directory `data_dir` resolved to.
pub fn database_path() -> PathBuf {
    data_dir().join("sts.db")
}

// ---------------------------------------------------------------------------
// connection setup
// ---------------------------------------------------------------------------

/// Run on every connection, reader or writer, before anything else.
///
/// The reasoning for each of these is in `SCHEMA.md`; the short version is that
/// `journal_mode` is the operating mode the whole system assumes, `synchronous`
/// is a deliberate durability trade against the NDJSON audit log being the
/// record of first resort, `busy_timeout` turns a lost race into a slow one,
/// and the last four are cache and I/O sizing that the 2 MiB defaults get
/// badly wrong for a file written from a socket path.
const CONNECTION_PRAGMAS: &str = "
    PRAGMA journal_mode       = WAL;
    PRAGMA synchronous        = NORMAL;
    PRAGMA foreign_keys       = ON;
    PRAGMA busy_timeout       = 5000;
    PRAGMA cache_size         = -65536;
    PRAGMA mmap_size          = 268435456;
    PRAGMA temp_store         = MEMORY;
    PRAGMA wal_autocheckpoint = 4000;
";

/// What the pragmas above actually produced, read back off the connection.
///
/// Worth having as a type rather than trusting the `PRAGMA` statements to have
/// worked: `journal_mode` is the one that can genuinely refuse — a file another
/// process holds open in rollback mode will not switch — and every guarantee in
/// this module rests on it having switched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PragmaReport {
    pub journal_mode: String,
    pub synchronous: i64,
    pub foreign_keys: bool,
    pub cache_size: i64,
    pub mmap_size: i64,
    pub temp_store: i64,
    pub wal_autocheckpoint: i64,
}

fn apply_pragmas(conn: &Connection) -> Result<PragmaReport, EngineError> {
    conn.execute_batch(CONNECTION_PRAGMAS)?;
    let report = read_pragmas(conn)?;

    // WAL is not a tuning option here, it is the assumption readers are written
    // against. Carrying on in rollback mode would mean every write took an
    // exclusive lock for its whole duration, which the ingest path cannot pay.
    if !report.journal_mode.eq_ignore_ascii_case("wal") {
        return Err(EngineError::Database(format!(
            "sts.db is in {} mode, not WAL — another process is probably holding it open",
            report.journal_mode
        )));
    }
    Ok(report)
}

fn read_pragmas(conn: &Connection) -> Result<PragmaReport, EngineError> {
    Ok(PragmaReport {
        journal_mode: conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?,
        synchronous: conn.query_row("PRAGMA synchronous", [], |row| row.get(0))?,
        foreign_keys: conn.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))? == 1,
        cache_size: conn.query_row("PRAGMA cache_size", [], |row| row.get(0))?,
        mmap_size: conn.query_row("PRAGMA mmap_size", [], |row| row.get(0))?,
        temp_store: conn.query_row("PRAGMA temp_store", [], |row| row.get(0))?,
        wal_autocheckpoint: conn.query_row("PRAGMA wal_autocheckpoint", [], |row| row.get(0))?,
    })
}

// ---------------------------------------------------------------------------
// migrations
// ---------------------------------------------------------------------------

/// One numbered schema change, applied inside its own transaction.
struct Migration {
    version: i64,
    /// Only used in error messages, so a refusal names something a person can
    /// find rather than a number.
    name: &'static str,
    sql: &'static str,
}

/// The whole schema, as migration 1.
///
/// Everything is `IF NOT EXISTS` even though the migration ledger already
/// guarantees this runs once: a database that predates the ledger — which is
/// every one written before this file existed — needs migration 1 to be able to
/// land on top of tables that are already there.
const MIGRATION_0001: &str = "
    CREATE TABLE IF NOT EXISTS candidates (
        id                   INTEGER PRIMARY KEY AUTOINCREMENT,

        mint                 TEXT,
        symbol               TEXT,
        curve_progress_bps   INTEGER NOT NULL
                               CHECK (curve_progress_bps BETWEEN 0 AND 10000),
        bonding_curve_pct    REAL GENERATED ALWAYS AS (curve_progress_bps / 100.0) VIRTUAL,
        liquidity_lamports   INTEGER NOT NULL CHECK (liquidity_lamports >= 0),
        creator_wallet       TEXT,
        detected_slot        INTEGER NOT NULL CHECK (detected_slot > 0),
        fast_path_eligible   INTEGER NOT NULL CHECK (fast_path_eligible IN (0, 1)),

        curve_account        TEXT NOT NULL,
        program              TEXT NOT NULL,
        source               TEXT NOT NULL,
        market_cap_usd_cents INTEGER CHECK (market_cap_usd_cents >= 0),
        observed_at_ms       INTEGER NOT NULL,
        dispatch_latency_us  INTEGER CHECK (dispatch_latency_us >= 0)
    );

    CREATE UNIQUE INDEX IF NOT EXISTS candidates_observation
        ON candidates (source, curve_account, detected_slot);
    CREATE INDEX IF NOT EXISTS candidates_recent
        ON candidates (observed_at_ms DESC);
    CREATE INDEX IF NOT EXISTS candidates_mint
        ON candidates (mint, observed_at_ms DESC)
        WHERE mint IS NOT NULL;
    CREATE INDEX IF NOT EXISTS candidates_creator
        ON candidates (creator_wallet, observed_at_ms DESC)
        WHERE creator_wallet IS NOT NULL;
    CREATE INDEX IF NOT EXISTS candidates_fast_path
        ON candidates (observed_at_ms DESC)
        WHERE fast_path_eligible = 1;

    CREATE TABLE IF NOT EXISTS clusters (
        cluster_id           TEXT NOT NULL,
        version              INTEGER NOT NULL,

        root_wallet          TEXT NOT NULL,
        wallet_count         INTEGER NOT NULL CHECK (wallet_count >= 1),

        hhi                  INTEGER NOT NULL CHECK (hhi BETWEEN 0 AND 10000),
        temporal_influence   REAL NOT NULL CHECK (temporal_influence BETWEEN 0.0 AND 1.0),
        spectral_separation  REAL NOT NULL CHECK (spectral_separation BETWEEN 0.0 AND 1.0),
        interaction_entropy  REAL NOT NULL CHECK (interaction_entropy BETWEEN 0.0 AND 1.0),

        flag_sybil           INTEGER NOT NULL CHECK (flag_sybil IN (0, 1)),
        computed_at_ms       INTEGER NOT NULL,

        PRIMARY KEY (cluster_id, version)
    );

    CREATE INDEX IF NOT EXISTS clusters_root_wallet
        ON clusters (root_wallet, version DESC);
    CREATE INDEX IF NOT EXISTS clusters_flagged
        ON clusters (computed_at_ms DESC)
        WHERE flag_sybil = 1;
    CREATE INDEX IF NOT EXISTS clusters_recent
        ON clusters (computed_at_ms DESC);

    CREATE TABLE IF NOT EXISTS execution_logs (
        intent_id     TEXT NOT NULL,
        seq           INTEGER NOT NULL CHECK (seq >= 0),

        mint          TEXT NOT NULL,
        state         TEXT NOT NULL CHECK (state IN (
                        'intent_created', 'validated', 'sent',
                        'confirmed', 'completed', 'aborted')),
        prev_state    TEXT CHECK (prev_state IN (
                        'intent_created', 'validated', 'sent',
                        'confirmed', 'completed', 'aborted')),
        side          TEXT NOT NULL CHECK (side IN ('buy', 'sell')),
        size_lamports INTEGER NOT NULL CHECK (size_lamports > 0),
        price         REAL CHECK (price IS NULL OR price > 0.0),
        signature     TEXT,
        latency_ms    INTEGER CHECK (latency_ms IS NULL OR latency_ms >= 0),
        needs_unwind  INTEGER NOT NULL DEFAULT 0 CHECK (needs_unwind IN (0, 1)),

        mode          TEXT NOT NULL CHECK (mode IN ('live', 'paper', 'replay')),
        abort_reason  TEXT CHECK (abort_reason IN (
                        'risk_gate', 'circuit_breaker', 'kill_switch', 'stale',
                        'send_failed', 'not_confirmed', 'operator')),
        at_ms         INTEGER NOT NULL,

        PRIMARY KEY (intent_id, seq)
    );

    CREATE INDEX IF NOT EXISTS execution_logs_at
        ON execution_logs (at_ms DESC);
    CREATE INDEX IF NOT EXISTS execution_logs_mint
        ON execution_logs (mint, at_ms DESC);
    CREATE INDEX IF NOT EXISTS execution_logs_unwind
        ON execution_logs (at_ms DESC)
        WHERE needs_unwind = 1;
    CREATE INDEX IF NOT EXISTS execution_logs_open
        ON execution_logs (state, at_ms DESC)
        WHERE state IN ('sent', 'confirmed');
    CREATE UNIQUE INDEX IF NOT EXISTS execution_logs_signature
        ON execution_logs (signature)
        WHERE signature IS NOT NULL;

    CREATE TABLE IF NOT EXISTS tick_metrics (
        rpc_endpoint   TEXT NOT NULL,
        timestamp      INTEGER NOT NULL,
        latency_ms     INTEGER NOT NULL CHECK (latency_ms >= 0),
        dropped_msgs   INTEGER NOT NULL CHECK (dropped_msgs >= 0),
        parsed_per_sec REAL NOT NULL CHECK (parsed_per_sec >= 0.0),
        PRIMARY KEY (rpc_endpoint, timestamp)
    ) WITHOUT ROWID;

    CREATE INDEX IF NOT EXISTS tick_metrics_time
        ON tick_metrics (timestamp DESC);

    CREATE TABLE IF NOT EXISTS audit_log (
        id         INTEGER PRIMARY KEY AUTOINCREMENT,
        event_type TEXT    NOT NULL,
        payload    TEXT    NOT NULL,
        created_at INTEGER NOT NULL
    );

    CREATE INDEX IF NOT EXISTS audit_log_created_at
        ON audit_log (created_at DESC);
";

/// The exit ledger, as migration 2.
///
/// `execution_logs` records the six-state machine in `types.rs`: what an intent
/// meant to do and how far it got. It cannot hold this. Its `state` column is
/// checked against those six names, and an exit's life —
/// `exit_constructed → exit_signed → exit_broadcast → exit_confirmed`, or
/// `exit_failed` — is a different machine at a different altitude, describing
/// one transaction rather than one intent.
///
/// So this is a second append-only ledger beside it, not a replacement and not
/// a duplicate. An exit writes both: its coarse steps go to `execution_logs` as
/// a new intent, because `RISK_AND_SYBIL_SPEC.md` U2 says a resolved obligation
/// is new rows and never an edit to the old ones, and its fine steps go here,
/// keyed by the same `intent_id` and carrying `origin_intent_id` — the
/// obligation being flattened — so the two halves of one unwind can be joined
/// back together afterwards.
///
/// The signature sits on exactly one row, the `exit_signed` step, for the same
/// reason it does in `execution_logs`: the unique partial index below means one
/// signature is one row forever, so it cannot be carried forward onto the
/// broadcast or the confirmation.
const MIGRATION_0002: &str = "
    CREATE TABLE IF NOT EXISTS intent_transitions (
        intent_id             TEXT NOT NULL,
        seq                   INTEGER NOT NULL CHECK (seq >= 0),

        origin_intent_id      TEXT NOT NULL,
        from_state            TEXT CHECK (from_state IN (
                                'exit_constructed', 'exit_signed', 'exit_broadcast',
                                'exit_confirmed', 'exit_failed')),
        to_state              TEXT NOT NULL CHECK (to_state IN (
                                'exit_constructed', 'exit_signed', 'exit_broadcast',
                                'exit_confirmed', 'exit_failed')),

        -- Null on an exit that failed before it was ever routed: there is no
        -- venue, no size and no floor for something that was never built, and a
        -- zero in those columns would read as a real number somebody computed.
        venue                 TEXT CHECK (venue IS NULL OR venue IN (
                                'pump_fun_curve', 'raydium_amm_v4')),
        mint                  TEXT NOT NULL,
        tokens                INTEGER CHECK (tokens IS NULL OR tokens > 0),
        min_out_lamports      INTEGER CHECK (min_out_lamports IS NULL OR min_out_lamports >= 0),
        out_lamports          INTEGER CHECK (out_lamports IS NULL OR out_lamports >= 0),
        cost_basis_lamports   INTEGER NOT NULL CHECK (cost_basis_lamports >= 0),
        realized_pnl_lamports INTEGER,

        signature             TEXT,
        failure               TEXT CHECK (failure IN (
                                'no_route', 'construction', 'signing', 'broadcast',
                                'not_confirmed', 'shutting_down')),
        -- The sentence a person reads. Required on a failure, where the bucket
        -- in `failure` says what kind and this says which one; also carried by
        -- the steps that need explaining — the tip an exit bid, and why one was
        -- broadcast more than once. Null everywhere else.
        detail                TEXT,
        mode                  TEXT NOT NULL CHECK (mode IN ('live', 'paper', 'replay')),
        at_ms                 INTEGER NOT NULL,

        PRIMARY KEY (intent_id, seq),

        -- A failure with no reason, or a reason on a step that did not fail,
        -- would both be rows that cannot be read back honestly.
        CHECK ((to_state = 'exit_failed' AND failure IS NOT NULL)
            OR (to_state <> 'exit_failed' AND failure IS NULL)),
        -- Profit with no proceeds is not a number anybody computed.
        CHECK (realized_pnl_lamports IS NULL OR out_lamports IS NOT NULL)
    );

    CREATE INDEX IF NOT EXISTS intent_transitions_at
        ON intent_transitions (at_ms DESC);
    CREATE INDEX IF NOT EXISTS intent_transitions_origin
        ON intent_transitions (origin_intent_id, at_ms DESC);
    CREATE INDEX IF NOT EXISTS intent_transitions_closed
        ON intent_transitions (mode, at_ms DESC)
        WHERE to_state = 'exit_confirmed';
    CREATE UNIQUE INDEX IF NOT EXISTS intent_transitions_signature
        ON intent_transitions (signature)
        WHERE signature IS NOT NULL;
";

/// The last six `REAL` columns, as integers, as migration 4.
///
/// `journal.rs` states the rule the whole file is now held to: money is
/// lamports, tokens are base units, a ratio is `Q18`, and none of them is a
/// float. Migration 3 arrived already obeying it. These four tables predate it
/// and did not, and a rule that holds for the newest three-fifths of a schema
/// is not a rule — it is a convention the next table gets to opt out of.
///
/// So this is the rest of it. Six columns, four tables, one migration, and
/// afterwards `SELECT typeof(...)` over every column of every table returns no
/// `real` — which `no_column_in_the_schema_is_a_real` asserts against the live
/// file rather than against this text.
///
/// # What each one becomes, and why that unit
///
/// **`candidates.bonding_curve_pct` is dropped.** It was
/// `curve_progress_bps / 100.0` — a generated column, never stored, never read
/// by anything but its own test, and derived from an integer that was already
/// the exact answer. There is no unit to move it to because there was no
/// information in it: the percentage is `curve_progress_bps / 100` and a caller
/// that wants it can divide. Dropping it is the only change here that removes a
/// number rather than converting one.
///
/// **The three cluster scores become millionths.** `strategy::syndicate`
/// already computes them as `u64` millionths and had a `store_unit` whose whole
/// job was rounding them to four decimal places on the way into a `REAL`,
/// because that was the only shape the column would take. The rounding existed
/// to absorb the last-bit difference a compiler's choice of fused multiply-add
/// would otherwise put between two runs of the same input — which is to say the
/// float was costing determinism and buying nothing. Storing the millionths the
/// analyser already had deletes the conversion, deletes the rounding, and makes
/// `SybilClusterMetrics` a type that can derive `Eq`.
///
/// **`execution_logs.price` becomes `price_q18`.** Lamports per token base
/// unit, `10^-18`, stored as its raw count — the same unit and the same
/// argument as `journal_fills.price_q18`, which is the column this one should
/// have been all along. Nothing has ever read it back: `fill_price` was the one
/// writer and every other caller binds `None`, so this converts a column that
/// was write-only into one that means something.
///
/// **`tick_metrics.parsed_per_sec` becomes `parsed_per_sec_micros`.** A rate,
/// in millionths of a message per second. Micros rather than the obvious
/// integer-per-second because the number it replaces is genuinely fractional
/// under a slow feed, and rounding a rate to whole messages would make "one
/// message every three seconds" and "silence" the same reading.
///
/// # How the tables are moved
///
/// Three of them are rebuilt rather than altered. SQLite can add and drop a
/// column but cannot change one's type or its `CHECK`, and both of those change
/// here — so this is the standard rebuild: new table, copy across, drop, rename,
/// recreate the indexes the drop took with it. None of the three is the parent
/// of a foreign key, which is what makes the `DROP TABLE` safe with
/// `foreign_keys = ON`; the only foreign keys in this schema point at
/// `journal_trades` and it is not touched here.
///
/// The `SELECT`s that copy do arithmetic in floating point, which is not a
/// contradiction: they are reading columns that are already `REAL` and this is
/// the last time anything in this system will. Each conversion is exact for the
/// values that can actually be in those columns — the cluster scores were
/// written as four-decimal-place values and the old `CHECK` bounded them to
/// `[0, 1]`, so `ROUND(x * 1000000)` lands on an integer rather than near one.
///
/// The price is the one that cannot always be converted, and it says so rather
/// than saturating. Past `i64::MAX` at `10^-18` — nine lamports for one token
/// base unit, which no token on either venue this build trades comes within
/// nine orders of magnitude of — and below `10^-18`, where the raw count would
/// floor to a zero the `CHECK` refuses, the row keeps a `NULL`. That reads as
/// "no price recorded", which is true, and is the same thing every other row in
/// that column already says.
const MIGRATION_0004: &str = "
    -- A percentage of an integer that was already exact.
    ALTER TABLE candidates DROP COLUMN bonding_curve_pct;

    CREATE TABLE clusters_0004 (
        cluster_id                 TEXT    NOT NULL,
        version                    INTEGER NOT NULL,

        root_wallet                TEXT    NOT NULL,
        wallet_count               INTEGER NOT NULL CHECK (wallet_count >= 1),

        hhi                        INTEGER NOT NULL CHECK (hhi BETWEEN 0 AND 10000),
        -- Millionths, as `strategy::syndicate` computed them.
        temporal_influence_micros  INTEGER NOT NULL
                                     CHECK (temporal_influence_micros BETWEEN 0 AND 1000000),
        spectral_separation_micros INTEGER NOT NULL
                                     CHECK (spectral_separation_micros BETWEEN 0 AND 1000000),
        interaction_entropy_micros INTEGER NOT NULL
                                     CHECK (interaction_entropy_micros BETWEEN 0 AND 1000000),

        flag_sybil                 INTEGER NOT NULL CHECK (flag_sybil IN (0, 1)),
        computed_at_ms             INTEGER NOT NULL,

        PRIMARY KEY (cluster_id, version)
    );

    INSERT INTO clusters_0004 (
        cluster_id, version, root_wallet, wallet_count, hhi,
        temporal_influence_micros, spectral_separation_micros, interaction_entropy_micros,
        flag_sybil, computed_at_ms
    )
    SELECT
        cluster_id, version, root_wallet, wallet_count, hhi,
        CAST(ROUND(temporal_influence  * 1000000) AS INTEGER),
        CAST(ROUND(spectral_separation * 1000000) AS INTEGER),
        CAST(ROUND(interaction_entropy * 1000000) AS INTEGER),
        flag_sybil, computed_at_ms
    FROM clusters;

    DROP TABLE clusters;
    ALTER TABLE clusters_0004 RENAME TO clusters;

    CREATE INDEX IF NOT EXISTS clusters_root_wallet
        ON clusters (root_wallet, version DESC);
    CREATE INDEX IF NOT EXISTS clusters_flagged
        ON clusters (computed_at_ms DESC)
        WHERE flag_sybil = 1;
    CREATE INDEX IF NOT EXISTS clusters_recent
        ON clusters (computed_at_ms DESC);

    CREATE TABLE execution_logs_0004 (
        intent_id     TEXT NOT NULL,
        seq           INTEGER NOT NULL CHECK (seq >= 0),

        mint          TEXT NOT NULL,
        state         TEXT NOT NULL CHECK (state IN (
                        'intent_created', 'validated', 'sent',
                        'confirmed', 'completed', 'aborted')),
        prev_state    TEXT CHECK (prev_state IN (
                        'intent_created', 'validated', 'sent',
                        'confirmed', 'completed', 'aborted')),
        side          TEXT NOT NULL CHECK (side IN ('buy', 'sell')),
        size_lamports INTEGER NOT NULL CHECK (size_lamports > 0),
        -- Lamports per token base unit, floored to 10^-18, as its raw count.
        -- The same unit and the same reasoning as `journal_fills.price_q18`.
        price_q18     INTEGER CHECK (price_q18 IS NULL OR price_q18 > 0),
        signature     TEXT,
        latency_ms    INTEGER CHECK (latency_ms IS NULL OR latency_ms >= 0),
        needs_unwind  INTEGER NOT NULL DEFAULT 0 CHECK (needs_unwind IN (0, 1)),

        mode          TEXT NOT NULL CHECK (mode IN ('live', 'paper', 'replay')),
        abort_reason  TEXT CHECK (abort_reason IN (
                        'risk_gate', 'circuit_breaker', 'kill_switch', 'stale',
                        'send_failed', 'not_confirmed', 'operator')),
        at_ms         INTEGER NOT NULL,

        PRIMARY KEY (intent_id, seq)
    );

    INSERT INTO execution_logs_0004 (
        intent_id, seq, mint, state, prev_state, side, size_lamports, price_q18,
        signature, latency_ms, needs_unwind, mode, abort_reason, at_ms
    )
    SELECT
        intent_id, seq, mint, state, prev_state, side, size_lamports,
        -- Outside what the column can hold, in either direction, the row keeps
        -- the NULL it almost certainly already had.
        CASE
            WHEN price IS NULL THEN NULL
            WHEN price * 1000000000000000000.0
                 BETWEEN 1 AND 9223372036854775807
                THEN CAST(price * 1000000000000000000.0 AS INTEGER)
            ELSE NULL
        END,
        signature, latency_ms, needs_unwind, mode, abort_reason, at_ms
    FROM execution_logs;

    DROP TABLE execution_logs;
    ALTER TABLE execution_logs_0004 RENAME TO execution_logs;

    CREATE INDEX IF NOT EXISTS execution_logs_at
        ON execution_logs (at_ms DESC);
    CREATE INDEX IF NOT EXISTS execution_logs_mint
        ON execution_logs (mint, at_ms DESC);
    CREATE INDEX IF NOT EXISTS execution_logs_unwind
        ON execution_logs (at_ms DESC)
        WHERE needs_unwind = 1;
    CREATE INDEX IF NOT EXISTS execution_logs_open
        ON execution_logs (state, at_ms DESC)
        WHERE state IN ('sent', 'confirmed');
    CREATE UNIQUE INDEX IF NOT EXISTS execution_logs_signature
        ON execution_logs (signature)
        WHERE signature IS NOT NULL;

    CREATE TABLE tick_metrics_0004 (
        rpc_endpoint          TEXT    NOT NULL,
        timestamp             INTEGER NOT NULL,
        latency_ms            INTEGER NOT NULL CHECK (latency_ms >= 0),
        dropped_msgs          INTEGER NOT NULL CHECK (dropped_msgs >= 0),
        -- Millionths of a message per second. A rate rounded to whole messages
        -- would make a slow feed and a dead one read the same.
        parsed_per_sec_micros INTEGER NOT NULL CHECK (parsed_per_sec_micros >= 0),
        PRIMARY KEY (rpc_endpoint, timestamp)
    ) WITHOUT ROWID;

    INSERT INTO tick_metrics_0004 (
        rpc_endpoint, timestamp, latency_ms, dropped_msgs, parsed_per_sec_micros
    )
    SELECT
        rpc_endpoint, timestamp, latency_ms, dropped_msgs,
        CAST(ROUND(parsed_per_sec * 1000000) AS INTEGER)
    FROM tick_metrics;

    DROP TABLE tick_metrics;
    ALTER TABLE tick_metrics_0004 RENAME TO tick_metrics;

    CREATE INDEX IF NOT EXISTS tick_metrics_time
        ON tick_metrics (timestamp DESC);
";

/// Every migration this build knows about, in the order they apply.
///
/// Append only, and never edit one that has shipped: the checksum recorded when
/// it ran is compared on every open, and a migration whose text changed after
/// the fact is how two machines end up claiming the same version with different
/// schemas.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial schema",
        sql: MIGRATION_0001,
    },
    Migration {
        version: 2,
        name: "exit ledger",
        sql: MIGRATION_0002,
    },
    // The journal's five tables. Written in `journal.rs`, beside the code that
    // reads and writes them, and registered here because there is one chain and
    // one runner: a second migration table against the same file is how two
    // builds end up disagreeing about what a version number means.
    Migration {
        version: 3,
        name: "trade journal",
        sql: crate::journal::MIGRATION_0003,
    },
    Migration {
        version: 4,
        name: "integers everywhere",
        sql: MIGRATION_0004,
    },
    // The forensic log, the checkpoints over it, and the monotonic counter that
    // orders both. Written in `forensics.rs` and registered here for the same
    // reason migration 3 is: one chain, one runner.
    Migration {
        version: 5,
        name: "forensic log and journal snapshots",
        sql: crate::forensics::MIGRATION_0005,
    },
];

/// The newest version this build can read. A file claiming anything higher is
/// refused.
pub fn latest_schema_version() -> i64 {
    MIGRATIONS.last().map(|m| m.version).unwrap_or(0)
}

/// FNV-1a over the migration text.
///
/// Not a cryptographic hash and not trying to be — the only thing it defends
/// against is a migration being edited after it shipped, which is an accident
/// rather than an attack. The audit log's hash chain is where SHA-256 belongs;
/// this needs to be cheap, stable across builds, and dependency-free.
fn checksum(sql: &str) -> String {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    for byte in sql.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("fnv1a64:{hash:016x}")
}

/// Brings the file up to `latest_schema_version`, or explains why it will not.
///
/// The ledger table is created outside any numbered migration, because it is
/// what records that the numbered migrations ran.
fn migrate(conn: &mut Connection, now_ms: i64) -> Result<i64, EngineError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
             version       INTEGER PRIMARY KEY,
             applied_at_ms INTEGER NOT NULL,
             checksum      TEXT    NOT NULL
         );",
    )?;

    let applied: BTreeMap<i64, String> = {
        let mut statement =
            conn.prepare("SELECT version, checksum FROM schema_migrations ORDER BY version")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };

    let latest = latest_schema_version();
    if let Some(&highest) = applied.keys().next_back() {
        if highest > latest {
            return Err(EngineError::Database(format!(
                "sts.db is at schema version {highest} and this build only knows {latest} — \
                 refusing to open it rather than read a newer schema as if it were this one"
            )));
        }
    }

    for migration in MIGRATIONS {
        let expected = checksum(migration.sql);
        match applied.get(&migration.version) {
            // Already applied. The only question left is whether it is still the
            // same migration it was when it ran.
            Some(recorded) => {
                if recorded != &expected {
                    return Err(EngineError::Database(format!(
                        "migration {} ({}) was applied as {recorded} but is {expected} in this \
                         build — the same version number now means two different schemas",
                        migration.version, migration.name
                    )));
                }
            }
            // One transaction per migration. SQLite applies DDL
            // transactionally, so a migration that fails halfway leaves the file
            // exactly as it was rather than half-changed.
            None => {
                let transaction = conn.transaction()?;
                transaction.execute_batch(migration.sql).map_err(|err| {
                    EngineError::Database(format!(
                        "migration {} ({}) failed: {err}",
                        migration.version, migration.name
                    ))
                })?;
                transaction.execute(
                    "INSERT INTO schema_migrations (version, applied_at_ms, checksum)
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![migration.version, now_ms, expected],
                )?;
                transaction.commit()?;
            }
        }
    }

    Ok(latest)
}

// ---------------------------------------------------------------------------
// the rows this side writes
// ---------------------------------------------------------------------------

/// One candidate the ingestion layer decided was worth writing down.
///
/// Plain owned data rather than the ingestion types themselves, so `db` stays
/// below ingestion in the dependency stack and does not need to know what a
/// bonding curve is. The base58 rendering happens on the WAL thread that builds
/// these, not on the socket task that produced them.
///
/// The field names here are ingestion's; three of them are the same thing the
/// `candidates` table calls something else, and `record_ingest_candidates` is
/// where the translation happens: `account` is `curve_account`, `slot` is
/// `detected_slot`, `pool_lamports` is `liquidity_lamports`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestCandidateRow {
    /// Which provider saw it. Part of the observation's identity.
    pub source: String,
    pub slot: i64,
    /// The bonding curve account, base58. The identity ingestion works with —
    /// the mint is resolved later, from the create instruction.
    pub account: String,
    /// The owning program, base58.
    pub program: String,
    /// `None` when the account layout predates the creator field.
    pub creator: Option<String>,
    /// `fast_path` or `standard`.
    pub route: String,
    pub market_cap_usd_cents: i64,
    pub pool_lamports: i64,
    pub curve_progress_bps: i64,
    pub observed_at_ms: i64,
    /// Receipt to dispatch, in microseconds. Stored per row so a slow minute can
    /// be found afterwards rather than only noticed at the time.
    pub dispatch_latency_us: i64,
}

impl IngestCandidateRow {
    /// The routing decision as the column stores it.
    ///
    /// A record of what happened, not a judgement to be recomputed later:
    /// re-deriving it from stored thresholds would silently rewrite history
    /// every time a threshold changed. Anything that is not the fast path is the
    /// standard path, which is the safe direction for an unrecognised value.
    fn fast_path_eligible(&self) -> i64 {
        i64::from(self.route == "fast_path")
    }
}

/// One cluster of wallets that look like one hand.
///
/// Derived intelligence rather than an observation: every row is reproducible
/// from the inputs and the version of the heuristic that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterRow {
    pub cluster_id: String,
    /// Which run of the heuristic produced this. Part of the primary key, so a
    /// recomputation adds a version rather than overwriting one — a decision
    /// made last week can still be explained with the numbers that were in
    /// front of it.
    pub version: i64,
    pub root_wallet: String,
    /// The four numbers, carried as the domain type so the basis points stay
    /// basis points and the 0-to-1 scores stay clamped.
    pub metrics: SybilClusterMetrics,
    /// A threshold applied to the metrics, not evidence on its own.
    pub flag_sybil: bool,
    pub computed_at_ms: i64,
}

/// Which side of the book an execution is on.
///
/// Here rather than in `types.rs` because the execution layer that will own it
/// does not exist yet; it should move once it does. The enum is what keeps the
/// column's `CHECK` unreachable — there is no way to spell a third side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub const fn as_str(self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }

    /// Reads back what `as_str` wrote. `None` for anything else, for the same
    /// reason `ExecutionState::parse` gives: a stored value this build
    /// cannot name is not a value to guess at.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "buy" => Some(Side::Buy),
            "sell" => Some(Side::Sell),
            _ => None,
        }
    }
}

/// The modes an execution can actually be logged under.
///
/// `OperatingMode` has a fourth variant, `Halted`, and halted opens nothing —
/// there is no execution to log. Narrowing here rather than rejecting at the
/// insert means the impossible row cannot be built in the first place, which is
/// the same argument `types.rs` makes for the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ExecutionMode {
    Live,
    Paper,
    Replay,
}

impl ExecutionMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            ExecutionMode::Live => "live",
            ExecutionMode::Paper => "paper",
            ExecutionMode::Replay => "replay",
        }
    }

    /// Reads back what `as_str` wrote. `None` for anything else, including
    /// `"halted"`, which is not a mode an execution can have been logged under.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "live" => Some(ExecutionMode::Live),
            "paper" => Some(ExecutionMode::Paper),
            "replay" => Some(ExecutionMode::Replay),
            _ => None,
        }
    }

    /// `None` for `Halted`, which is the whole point of the type.
    pub const fn from_operating_mode(mode: OperatingMode) -> Option<Self> {
        match mode {
            OperatingMode::Live => Some(ExecutionMode::Live),
            OperatingMode::Paper => Some(ExecutionMode::Paper),
            OperatingMode::Replay => Some(ExecutionMode::Replay),
            OperatingMode::Halted => None,
        }
    }
}

/// One state transition of one execution.
///
/// Append-only. A row is a step, not a status — where an order is now is the
/// newest row for its `intent_id`, and the sequence of rows is how it got
/// there, which is what a post-mortem, a replay and a reconciliation all need.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionLogRow {
    /// The UUIDv7 minted when the EV engine decided it wanted to do something.
    /// The correlation ID tying this row to the audit NDJSON, the telemetry
    /// event and the risk decision that allowed it.
    pub intent_id: String,
    /// Counts transitions within one intent, from 0.
    pub seq: i64,
    pub mint: String,
    pub state: ExecutionState,
    /// `None` on the first step, which came from nowhere.
    pub prev_state: Option<ExecutionState>,
    pub side: Side,
    pub size_lamports: i64,
    /// What one token base unit fetched, in lamports, floored to `10^-18`.
    /// `None` before a fill, because the states before one have no price.
    ///
    /// The same unit as `journal_fills.price_q18` and for the same reasons,
    /// which `journal.rs` sets out at length: a `REAL` here could not be
    /// summed, could not be compared exactly, and could not survive two runs of
    /// the same numbers agreeing to the byte.
    pub price_q18: Option<Q18>,
    /// `None` until `sent`, and forever on anything aborted before it.
    pub signature: Option<String>,
    /// Measured against the previous transition, not against the intent's
    /// creation, so the steps add up but a slow step can also be found alone.
    pub latency_ms: Option<i64>,
    /// True when this row is an open obligation: something is on chain that a
    /// person still has to flatten by hand. Never cleared, because the row is
    /// history — see `ExecutionLogRow::aborted`.
    pub needs_unwind: bool,
    pub mode: ExecutionMode,
    pub abort_reason: Option<AbortReason>,
    pub at_ms: i64,
}

impl ExecutionLogRow {
    /// Builds the row for an abort straight from the outcome.
    ///
    /// `needs_unwind`, `prev_state` and `abort_reason` all come from the one
    /// `AbortOutcome` rather than being assigned separately, because the three
    /// of them disagreeing is exactly the failure the flag exists to prevent.
    /// Aborting a `sent` or `confirmed` execution does not sell the position —
    /// there is no transaction that un-sends another one — and this is what
    /// records that something was left behind.
    #[allow(clippy::too_many_arguments)]
    pub fn aborted(
        intent_id: String,
        seq: i64,
        mint: String,
        outcome: AbortOutcome,
        side: Side,
        size_lamports: i64,
        signature: Option<String>,
        mode: ExecutionMode,
        at_ms: i64,
    ) -> Self {
        ExecutionLogRow {
            intent_id,
            seq,
            mint,
            state: outcome.state,
            prev_state: Some(outcome.from),
            side,
            size_lamports,
            price_q18: None,
            signature,
            latency_ms: None,
            needs_unwind: outcome.needs_unwind,
            mode,
            abort_reason: Some(outcome.reason),
            at_ms,
        }
    }
}

/// One intent that still has money out, as of the newest row it wrote.
///
/// There are two ways to be in this list and neither is visible from the
/// other's query, which is why `open_obligations` asks for both at once. An
/// intent whose newest state is still `sent` or `confirmed` was never finished
/// — the process died mid-flight. An intent whose newest state is `aborted`
/// with `needs_unwind` set was finished by the engine and left something behind.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenObligation {
    pub intent_id: String,
    /// The `seq` of the newest row, so the next row for this intent is
    /// `seq + 1`. Carried because appending to an intent's history requires
    /// knowing where the history got to.
    pub seq: i64,
    pub state: ExecutionState,
    /// The state before `state`. On an obligation that is already `aborted`
    /// this is the only record of which state the money was left at risk in,
    /// and `sent` and `confirmed` mean very different things to whoever has to
    /// deal with it.
    pub prev_state: Option<ExecutionState>,
    pub mint: String,
    pub side: Side,
    pub size_lamports: i64,
    /// The transaction that put the money out, when the intent got far enough
    /// to have one. Taken from this intent's `sent` step rather than from the
    /// newest row: the unique partial index means one signature is one row,
    /// forever, so no later row for the same intent can hold a copy of it.
    pub signature: Option<String>,
    pub needs_unwind: bool,
    pub mode: ExecutionMode,
    /// When the newest row for this intent was written. Where it has got to,
    /// as of when.
    pub at_ms: i64,
    /// When this intent's *first* row was written — when the trade opened.
    ///
    /// The earliest row and not the newest, and the difference matters because
    /// only one of them holds still. `at_ms` above moves every time anything
    /// appends to the intent, and an emergency unwind appends to it: a position
    /// it could not flatten gets an `aborted` row under its own id, stamped
    /// with the clock. So two unwind passes over the same position see two
    /// different `at_ms` and the same `opened_at_ms`.
    ///
    /// The journal is why that matters. `journal_trades` refuses an update that
    /// changes when a trade opened — the trigger is the point of the column —
    /// so a second exit attempt writing the newest timestamp would abort its
    /// own write rather than update the row, and the book would keep the first
    /// attempt's answer forever. `seq` 0 is written once and never edited,
    /// which is what makes this the stable answer.
    pub opened_at_ms: i64,
}

impl OpenObligation {
    /// The state the money was actually left at risk in.
    ///
    /// For an intent still in flight that is where it is now. For one the
    /// engine already gave up on, `state` is `aborted` — which says nothing
    /// about what is on chain — and the answer is the state it was aborted
    /// from.
    pub fn at_risk_in(&self) -> Option<ExecutionState> {
        if self.state.is_terminal() {
            self.prev_state.filter(|prev| prev.has_money_at_risk())
        } else if self.state.has_money_at_risk() {
            Some(self.state)
        } else {
            None
        }
    }
}

/// One endpoint's health for one tick.
#[derive(Debug, Clone, PartialEq)]
pub struct TickMetricRow {
    /// The **host only**, never the full URL. Every provider used here puts its
    /// credential in the URL, so a full URL in a table the UI reads is a leaked
    /// key.
    pub rpc_endpoint: String,
    pub timestamp_ms: i64,
    /// The endpoint's p50 for this tick.
    pub latency_ms: i64,
    /// A count for this tick, not a running total — the number that says the
    /// engine is slower than the feed right now.
    pub dropped_msgs: i64,
    /// Messages parsed per second, in millionths. Micros rather than whole
    /// messages because the number is genuinely fractional under a slow feed,
    /// and rounding it to integers would make "one message every three seconds"
    /// and "silence" the same reading.
    pub parsed_per_sec_micros: u64,
}

/// One step in the life of one exit transaction.
///
/// Append-only, exactly like `ExecutionLogRow`, and for the same reason: where
/// an exit is now is the newest row for its `intent_id`, and the sequence of
/// rows is how it got there. A reconciliation that has to answer "did we
/// actually put a sell on the network for this position" reads these.
///
/// `origin_intent_id` is the obligation being flattened. It is not the same as
/// `intent_id`: `RISK_AND_SYBIL_SPEC.md` U2 makes a resolved obligation a new
/// intent rather than an edit to the old one, so the exit has its own id, and
/// this column is the only thing joining the two back together.
#[derive(Debug, Clone, PartialEq)]
pub struct IntentTransitionRow {
    /// The exit's own intent id.
    pub intent_id: String,
    /// Counts steps within one exit, from 0.
    pub seq: i64,
    /// The obligation this exit is flattening.
    pub origin_intent_id: String,
    /// `None` on the first step, which came from nowhere.
    pub from_state: Option<ExitState>,
    pub to_state: ExitState,
    /// `None` on an exit that failed before it was routed anywhere.
    pub venue: Option<Venue>,
    pub mint: String,
    /// The position being sold, in token base units. `None` before there was a
    /// route to say how many.
    pub tokens: Option<i64>,
    /// The slippage bound the transaction was built with — what the engine
    /// promised itself it would not accept less than. `None` before there was
    /// anything to bound.
    pub min_out_lamports: Option<i64>,
    /// What actually came back, on the step that confirmed. `None` everywhere
    /// else, because nothing has come back yet.
    pub out_lamports: Option<i64>,
    /// What the position cost to open, from the obligation's own row.
    pub cost_basis_lamports: i64,
    /// `out_lamports - cost_basis_lamports`, on the step that confirmed.
    ///
    /// Stored rather than derived on read so the number that was true at the
    /// time survives a later change to how it is computed. The `CHECK` on the
    /// table is what stops it existing without proceeds behind it.
    pub realized_pnl_lamports: Option<i64>,
    /// On the `exit_signed` step only — the unique partial index means one
    /// signature is one row, forever.
    pub signature: Option<String>,
    /// On the `exit_failed` step only, and required there.
    pub failure: Option<ExitFailure>,
    /// The sentence a person reads.
    ///
    /// Required on `exit_failed`, where `failure` says what kind of failure it
    /// was and this says which one. Also carried by the steps that need
    /// explaining rather than only the ones that went wrong: what an exit
    /// tipped, on the step that signed it, and why the same bytes went out
    /// again, on each repeat of `exit_broadcast`. Null everywhere else.
    pub detail: Option<String>,
    pub mode: ExecutionMode,
    pub at_ms: i64,
}

/// The newest step of one exit, as of the last row it wrote.
///
/// This is what the unwind path reads before it flattens anything: an
/// obligation that already has an exit on the network must not get a second
/// one, and this is how that is known across processes rather than only within
/// the one that sent it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitAttempt {
    pub intent_id: String,
    pub origin_intent_id: String,
    /// The `seq` of the newest row, so the next row for this exit is `seq + 1`.
    pub seq: i64,
    pub state: ExitState,
    /// The state before `state`. On a failed exit this is the only record of
    /// how far it had got, and `exit_signed` and `exit_broadcast` mean very
    /// different things to whoever has to decide what to do next.
    pub from_state: Option<ExitState>,
    pub venue: Option<Venue>,
    pub mint: String,
    pub tokens: Option<i64>,
    pub out_lamports: Option<i64>,
    pub cost_basis_lamports: i64,
    pub realized_pnl_lamports: Option<i64>,
    /// Taken from this exit's `exit_signed` step rather than from the newest
    /// row, for the reason `OpenObligation::signature` gives.
    pub signature: Option<String>,
    pub failure: Option<ExitFailure>,
    pub detail: Option<String>,
    pub mode: ExecutionMode,
    pub at_ms: i64,
}

impl ExitAttempt {
    /// Whether this exit put a transaction somewhere it cannot be recalled
    /// from.
    ///
    /// True while it is broadcast or confirmed, and true for a failure that
    /// happened after the broadcast — that last case is the important one,
    /// because a failed exit that had already reached the network may still
    /// have sold the position.
    pub fn left_on_network(&self) -> bool {
        self.state.is_dispatched()
            || (self.state == ExitState::ExitFailed
                && self.from_state.is_some_and(ExitState::is_dispatched))
    }

    /// Whether the position this exit was for is closed and booked.
    pub fn is_settled(&self) -> bool {
        self.state == ExitState::ExitConfirmed
    }

    /// Whether a second exit for the same obligation must not be built.
    ///
    /// Anything still running is in flight and will finish or fail on its own.
    /// Anything that reached the network is ambiguous until it is reconciled.
    /// The only case that may be retried is an exit that failed before the
    /// network ever saw it, which by definition changed nothing.
    pub fn blocks_retry(&self) -> bool {
        self.state.is_active() || self.left_on_network()
    }
}

/// Closed positions and what they came to, for one mode.
///
/// Per mode rather than summed across them, because `SCHEMA.md` is explicit
/// that a query which forgets to filter on `mode` is reporting paper trades as
/// real ones, and a single total is that query with no way to notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RealizedPnl {
    /// How many positions were flattened and confirmed.
    pub closed: i64,
    /// What came back from selling them, in lamports.
    pub proceeds_lamports: i64,
    /// What they cost to open, in lamports.
    pub cost_basis_lamports: i64,
    /// Proceeds less cost. Negative is a loss, which is why every number on
    /// this path is signed.
    pub realized_lamports: i64,
}

/// The same three numbers for each mode, so nothing has to guess which one it
/// is looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RealizedPnlByMode {
    pub live: RealizedPnl,
    pub paper: RealizedPnl,
    pub replay: RealizedPnl,
}

// ---------------------------------------------------------------------------
// the connection
// ---------------------------------------------------------------------------

/// What `get_engine_status` reports about the database.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbHealth {
    /// The file this process actually opened, for when two of them disagree.
    pub path: String,
    /// False only if something dropped the tables out from under a live
    /// process; `open` migrates, so a database this build opened has a schema.
    pub schema_present: bool,
    /// Which migration the file is at.
    pub schema_version: i64,
    pub candidates: i64,
    pub clusters: i64,
    pub execution_logs: i64,
    /// Executions with money at risk: `sent` or `confirmed` and not yet past
    /// either. Read through `execution_logs_open`.
    pub open_executions: i64,
    /// Rows nobody has flattened. Every one is an open obligation.
    pub needs_unwind: i64,
    /// Steps recorded in `intent_transitions`: the exit ledger's size.
    pub intent_transitions: i64,
    /// Obligations that have an exit on the network right now — broadcast and
    /// not yet confirmed. Money whose fate is decided but not yet known.
    pub exits_in_flight: i64,
    /// Closed positions and what they came to, kept apart by mode.
    pub realized_pnl: RealizedPnlByMode,
    /// Epoch milliseconds of the newest audit row, or `None` if there are none.
    pub last_audit_at_ms: Option<i64>,
}

/// One connection to `sts.db`, behind a lock.
///
/// `rusqlite::Connection` is `Send` but not `Sync`, so shared access needs the
/// mutex regardless; `parking_lot`'s has no poisoning, which is what makes the
/// panic path below possible at all.
pub struct Database {
    path: PathBuf,
    schema_version: i64,
    pragmas: PragmaReport,
    conn: Mutex<Connection>,
}

impl Database {
    /// Opens `sts.db`, creating the directory if this process got here first,
    /// applying the pragmas, and migrating the schema up to this build.
    pub fn open(path: &Path) -> Result<Self, EngineError> {
        Self::open_at(path, crate::telemetry::now_ms())
    }

    /// `open`, with the clock passed in. Only the tests need this; everything
    /// else wants the real one.
    fn open_at(path: &Path, now_ms: i64) -> Result<Self, EngineError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|err| {
                EngineError::Database(format!("could not create {}: {err}", dir.display()))
            })?;
        }

        let mut conn = Connection::open(path)?;
        let pragmas = apply_pragmas(&conn)?;
        let schema_version = migrate(&mut conn, now_ms)?;

        Ok(Self {
            path: path.to_path_buf(),
            schema_version,
            pragmas,
            conn: Mutex::new(conn),
        })
    }

    /// Which migration this file is at.
    pub fn schema_version(&self) -> i64 {
        self.schema_version
    }

    /// What the connection pragmas actually came back as, read once on open.
    pub fn pragmas(&self) -> &PragmaReport {
        &self.pragmas
    }

    /// The one connection, for the modules that extend this schema.
    ///
    /// `journal.rs` owns migration 3 and the five tables in it, and it writes
    /// them through this rather than opening a second connection: the
    /// one-writer rule in the header is a rule about the file, and a second
    /// handle onto the same file is exactly what it exists to prevent. The
    /// guard comes back rather than the connection, so there is no way to hold
    /// one without holding the other.
    ///
    /// `pub(crate)` and not `pub`: nothing outside this build gets to write
    /// arbitrary SQL against a database somebody is trusting with money.
    pub(crate) fn connection(&self) -> parking_lot::MutexGuard<'_, Connection> {
        self.conn.lock()
    }

    /// A snapshot of what is in the file.
    pub fn health(&self) -> Result<DbHealth, EngineError> {
        let conn = self.conn.lock();

        let schema_present = table_exists(&conn, "candidates")?;
        let candidates = if schema_present {
            count(&conn, "SELECT COUNT(*) FROM candidates")?
        } else {
            0
        };
        let clusters = if table_exists(&conn, "clusters")? {
            count(&conn, "SELECT COUNT(*) FROM clusters")?
        } else {
            0
        };
        let (execution_logs, open_executions, needs_unwind) =
            if table_exists(&conn, "execution_logs")? {
                (
                    count(&conn, "SELECT COUNT(*) FROM execution_logs")?,
                    // Says `IN` rather than two lookups so the partial index
                    // `execution_logs_open` is usable; there is no ORDER BY, so
                    // the sort that index note warns about does not arise.
                    count(
                        &conn,
                        "SELECT COUNT(*) FROM execution_logs WHERE state IN ('sent', 'confirmed')",
                    )?,
                    count(
                        &conn,
                        "SELECT COUNT(*) FROM execution_logs WHERE needs_unwind = 1",
                    )?,
                )
            } else {
                (0, 0, 0)
            };
        let (intent_transitions, exits_in_flight, realized_pnl) =
            if table_exists(&conn, "intent_transitions")? {
                (
                    count(&conn, "SELECT COUNT(*) FROM intent_transitions")?,
                    // The newest step per exit, restricted to the ones still on
                    // the network. Counting `exit_broadcast` rows directly would
                    // count every exit that has since confirmed as still open.
                    count(
                        &conn,
                        "WITH latest AS (
                             SELECT intent_id, MAX(seq) AS seq
                               FROM intent_transitions GROUP BY intent_id
                         )
                         SELECT COUNT(*) FROM intent_transitions t
                           JOIN latest l ON l.intent_id = t.intent_id AND l.seq = t.seq
                          WHERE t.to_state = 'exit_broadcast'",
                    )?,
                    RealizedPnlByMode {
                        live: realized_pnl_for(&conn, ExecutionMode::Live)?,
                        paper: realized_pnl_for(&conn, ExecutionMode::Paper)?,
                        replay: realized_pnl_for(&conn, ExecutionMode::Replay)?,
                    },
                )
            } else {
                (0, 0, RealizedPnlByMode::default())
            };
        let last_audit_at_ms = if table_exists(&conn, "audit_log")? {
            conn.query_row("SELECT MAX(created_at) FROM audit_log", [], |row| {
                row.get(0)
            })
            .optional()?
            .flatten()
        } else {
            None
        };

        Ok(DbHealth {
            path: self.path.display().to_string(),
            schema_present,
            schema_version: self.schema_version,
            candidates,
            clusters,
            execution_logs,
            open_executions,
            needs_unwind,
            intent_transitions,
            exits_in_flight,
            realized_pnl,
            last_audit_at_ms,
        })
    }

    /// Appends one row to `audit_log` and returns its id.
    ///
    /// `docs/AUDIT_EVENTS.md` makes the NDJSON file the record of first resort
    /// and this table its mirror; the Rust side writes the mirror directly
    /// because it has no `AuditLogger` of its own yet.
    pub fn record_audit(
        &self,
        event_type: &str,
        payload: &serde_json::Value,
        at_ms: i64,
    ) -> Result<i64, EngineError> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO audit_log (event_type, payload, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![event_type, payload.to_string(), at_ms],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// The same append, but safe to call from a panic hook.
    ///
    /// Returns whether the row was written. It gives up rather than waiting if
    /// the panic happened while the connection was locked, and it swallows every
    /// error: a hook that fails loudly aborts the process before the real panic
    /// message is ever printed.
    pub fn record_audit_best_effort(
        &self,
        event_type: &str,
        payload: &serde_json::Value,
        at_ms: i64,
    ) -> bool {
        let Some(conn) = self.conn.try_lock_for(PANIC_LOCK_TIMEOUT) else {
            return false;
        };
        conn.execute(
            "INSERT INTO audit_log (event_type, payload, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![event_type, payload.to_string(), at_ms],
        )
        .is_ok()
    }

    /// Confirms the tables the ingestion path writes to are there.
    ///
    /// `open` already migrated, so this is a check rather than a step. It stays
    /// because the WAL worker calls it on the way up and needs an answer before
    /// it starts draining candidates into a table that might not exist — and
    /// because a schema that vanished under a running process should stop the
    /// writer rather than have it fail a row at a time.
    pub fn ensure_ingest_schema(&self) -> Result<(), EngineError> {
        let conn = self.conn.lock();
        if !table_exists(&conn, "candidates")? {
            return Err(EngineError::Database(
                "candidates is missing from a database that was migrated on open".to_string(),
            ));
        }
        Ok(())
    }

    /// Writes a batch of candidates in one transaction.
    ///
    /// Returns how many rows were new. `ON CONFLICT ... DO NOTHING` against
    /// `candidates_observation` is what makes a duplicate a no-op rather than a
    /// second row: a reconnect replays whatever the socket buffered, and a
    /// replayed fixture has to land the same way twice. Two different providers
    /// seeing the same account at the same slot are two rows, because the
    /// provider is part of the identity and their agreement is evidence.
    ///
    /// The conflict target is named rather than using `INSERT OR IGNORE`, and
    /// the difference matters: `OR IGNORE` skips a row that violates *any*
    /// constraint, so a candidate with a curve past 10000 basis points or a
    /// negative pool would be dropped as quietly as a duplicate. Naming the
    /// identity means only a duplicate is quiet, and a row that cannot be true
    /// fails the batch.
    ///
    /// One transaction for the batch rather than one per row. Under WAL that is
    /// one fsync instead of `n`, which is the difference between keeping up with
    /// a launch burst and falling behind it.
    ///
    /// `mint` and `symbol` are written null: the ingest path identifies a launch
    /// by its bonding curve account, and the curve does not derive back to the
    /// mint. A later pass fills them in from the create instruction.
    pub fn record_ingest_candidates(
        &self,
        rows: &[IngestCandidateRow],
    ) -> Result<usize, EngineError> {
        if rows.is_empty() {
            return Ok(0);
        }

        let mut conn = self.conn.lock();
        let transaction = conn.transaction()?;
        let mut written = 0usize;
        {
            let mut statement = transaction.prepare_cached(
                "INSERT INTO candidates (
                     mint, symbol, curve_progress_bps, liquidity_lamports, creator_wallet,
                     detected_slot, fast_path_eligible, curve_account, program, source,
                     market_cap_usd_cents, observed_at_ms, dispatch_latency_us
                 ) VALUES (NULL, NULL, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT (source, curve_account, detected_slot) DO NOTHING",
            )?;
            for row in rows {
                // Every one of these binds as it was computed. The basis
                // points stay an integer end to end: migration 4 took away the
                // generated `bonding_curve_pct` that used to offer the same
                // number as a percentage, so there is no longer a second
                // spelling of this quantity for anything to disagree with.
                written += statement.execute(rusqlite::params![
                    row.curve_progress_bps,
                    row.pool_lamports,
                    row.creator,
                    row.slot,
                    row.fast_path_eligible(),
                    row.account,
                    row.program,
                    row.source,
                    row.market_cap_usd_cents,
                    row.observed_at_ms,
                    row.dispatch_latency_us,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(written)
    }

    /// How many candidate rows are in the file. For `get_engine_status` and for
    /// the fixture assertions the roadmap's Phase 1 gate asks for.
    pub fn ingest_candidate_count(&self) -> Result<i64, EngineError> {
        let conn = self.conn.lock();
        if !table_exists(&conn, "candidates")? {
            return Ok(0);
        }
        count(&conn, "SELECT COUNT(*) FROM candidates")
    }

    /// Writes a batch of clusters in one transaction. Returns how many were new.
    ///
    /// A conflict on `(cluster_id, version)` does nothing: one version of the
    /// heuristic over one set of inputs produces one answer, so re-running it is
    /// a no-op rather than a conflict. A recomputation that should be recorded
    /// separately gets a new `version`.
    ///
    /// Only that conflict is quiet. `INSERT OR IGNORE` here would swallow the
    /// `NOT NULL` and `CHECK` pair the schema leans on — a score past a whole
    /// unit arrives as an out-of-range integer and would be dropped silently,
    /// leaving a cluster that looks clean because the maths broke. Naming the
    /// conflict target is what keeps those loud.
    ///
    /// Every value binds as the integer it was computed as: the `hhi` as the
    /// `u16` of basis points, the three scores as the millionths
    /// `strategy::syndicate` counted them in. Migration 4 is what made the last
    /// three of those true — they used to be rounded into a `REAL` on the way
    /// past, which is the one step on this path that could make two runs of the
    /// same input disagree.
    pub fn record_clusters(&self, rows: &[ClusterRow]) -> Result<usize, EngineError> {
        if rows.is_empty() {
            return Ok(0);
        }

        let mut conn = self.conn.lock();
        let transaction = conn.transaction()?;
        let mut written = 0usize;
        {
            let mut statement = transaction.prepare_cached(
                "INSERT INTO clusters (
                     cluster_id, version, root_wallet, wallet_count, hhi,
                     temporal_influence_micros, spectral_separation_micros,
                     interaction_entropy_micros,
                     flag_sybil, computed_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT (cluster_id, version) DO NOTHING",
            )?;
            for row in rows {
                written += statement.execute(rusqlite::params![
                    row.cluster_id,
                    row.version,
                    row.root_wallet,
                    row.metrics.wallet_count,
                    row.metrics.holding_hhi_bps,
                    row.metrics.temporal_influence_micros,
                    row.metrics.spectral_separation_micros,
                    row.metrics.interaction_entropy_micros,
                    i64::from(row.flag_sybil),
                    row.computed_at_ms,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(written)
    }

    /// Appends execution transitions. Returns how many rows were new.
    ///
    /// Two kinds of duplicate reach this statement and they are not the same
    /// thing, so the conflict target names only the first:
    ///
    /// - The same `(intent_id, seq)` twice is a retry writing a step it already
    ///   wrote. That is idempotent and is skipped, which is what makes the retry
    ///   safe rather than producing a history with a duplicated step in it.
    /// - The same `signature` twice is either one transaction recorded twice or
    ///   two intents believing they own one position. That is not the target, so
    ///   it fails here rather than being found later in a reconciliation, and
    ///   the whole batch rolls back.
    pub fn record_execution_logs(&self, rows: &[ExecutionLogRow]) -> Result<usize, EngineError> {
        if rows.is_empty() {
            return Ok(0);
        }

        let mut conn = self.conn.lock();
        let transaction = conn.transaction()?;
        let mut written = 0usize;
        {
            let mut statement = transaction.prepare_cached(
                "INSERT INTO execution_logs (
                     intent_id, seq, mint, state, prev_state, side, size_lamports,
                     price_q18, signature, latency_ms, needs_unwind, mode, abort_reason, at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                 ON CONFLICT (intent_id, seq) DO NOTHING",
            )?;
            for row in rows {
                written += statement.execute(rusqlite::params![
                    row.intent_id,
                    row.seq,
                    row.mint,
                    row.state.as_str(),
                    row.prev_state.map(ExecutionState::as_str),
                    row.side.as_str(),
                    row.size_lamports,
                    row.price_q18.map(price_column).transpose()?,
                    row.signature,
                    row.latency_ms,
                    i64::from(row.needs_unwind),
                    row.mode.as_str(),
                    row.abort_reason.map(AbortReason::as_str),
                    row.at_ms,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(written)
    }

    /// Every intent that still has money out, newest first.
    ///
    /// This is the query `RISK_AND_SYBIL_SPEC.md` §13.3 runs on the startup
    /// path, with one deliberate difference: it does not filter on `mode`. The
    /// spec filters because a startup reconciliation knows which mode it is
    /// resuming. An operator asking what is on chain does not, and a live
    /// obligation hidden behind the wrong mode filter is the expensive
    /// direction to be wrong in. Every row carries its own `mode` instead, so a
    /// paper obligation comes back labelled rather than dropped.
    ///
    /// Both arms of the `WHERE` are needed. Reads through `execution_logs_open`
    /// and `execution_logs_unwind`, which is what keeps this a lookup into two
    /// tiny B-trees rather than a scan of everything that has ever executed.
    ///
    /// The signature is the one column not taken from the newest row, and the
    /// unique partial index on it is why. A signature can exist on exactly one
    /// row in the whole table, so it lives on the `sent` step and cannot be
    /// carried forward onto `confirmed` or onto the abort — every row after the
    /// send has `NULL` there. Reading it off the newest row would therefore
    /// return nothing for every obligation past `sent`, which is every position
    /// that actually exists, and the signature is the only handle whoever has
    /// to flatten it has for finding it. The subquery walks that one intent's
    /// own rows, which is a seek on the primary key's leading column.
    pub fn open_obligations(&self) -> Result<Vec<OpenObligation>, EngineError> {
        let conn = self.conn.lock();
        // The panic path can reach this through an emergency unwind on a
        // process that never got a schema, and a missing table there should
        // read as "nothing recorded", not as an error on the way out.
        if !table_exists(&conn, "execution_logs")? {
            return Ok(Vec::new());
        }

        let mut statement = conn.prepare_cached(
            "WITH latest AS (
                 SELECT intent_id, MAX(seq) AS seq FROM execution_logs GROUP BY intent_id
             )
             SELECT e.intent_id, e.seq, e.state, e.prev_state, e.mint, e.side,
                    e.size_lamports,
                    (SELECT s.signature FROM execution_logs s
                      WHERE s.intent_id = e.intent_id AND s.signature IS NOT NULL
                      ORDER BY s.seq LIMIT 1),
                    e.needs_unwind, e.mode, e.at_ms,
                    (SELECT o.at_ms FROM execution_logs o
                      WHERE o.intent_id = e.intent_id
                      ORDER BY o.seq LIMIT 1)
               FROM execution_logs e
               JOIN latest l ON l.intent_id = e.intent_id AND l.seq = e.seq
              WHERE e.state IN ('sent', 'confirmed') OR e.needs_unwind = 1
              ORDER BY e.at_ms DESC",
        )?;

        let mut rows = statement.query([])?;
        let mut obligations = Vec::new();
        while let Some(row) = rows.next()? {
            let state: String = row.get(2)?;
            let prev_state: Option<String> = row.get(3)?;
            let side: String = row.get(5)?;
            let mode: String = row.get(9)?;
            obligations.push(OpenObligation {
                intent_id: row.get(0)?,
                seq: row.get(1)?,
                state: stored_as(&state, ExecutionState::parse, "execution_logs.state")?,
                prev_state: prev_state
                    .as_deref()
                    .map(|text| stored_as(text, ExecutionState::parse, "execution_logs.prev_state"))
                    .transpose()?,
                mint: row.get(4)?,
                side: stored_as(&side, Side::parse, "execution_logs.side")?,
                size_lamports: row.get(6)?,
                signature: row.get(7)?,
                needs_unwind: row.get::<_, i64>(8)? != 0,
                mode: stored_as(&mode, ExecutionMode::parse, "execution_logs.mode")?,
                at_ms: row.get(10)?,
                opened_at_ms: row.get(11)?,
            });
        }
        Ok(obligations)
    }

    /// Appends exit lifecycle steps. Returns how many rows were new.
    ///
    /// The same two kinds of duplicate reach this statement as reach
    /// `record_execution_logs`, and they are treated the same way. The same
    /// `(intent_id, seq)` twice is a retry writing a step it already wrote, and
    /// is skipped — which is what makes an unwind that is pressed twice, or
    /// resumed after a restart, converge rather than fork. The same `signature`
    /// twice is two exits believing they own one transaction, which is not the
    /// conflict target, so it fails here and rolls the batch back.
    pub fn record_intent_transitions(
        &self,
        rows: &[IntentTransitionRow],
    ) -> Result<usize, EngineError> {
        if rows.is_empty() {
            return Ok(0);
        }

        let mut conn = self.conn.lock();
        let transaction = conn.transaction()?;
        let mut written = 0usize;
        {
            let mut statement = transaction.prepare_cached(
                "INSERT INTO intent_transitions (
                     intent_id, seq, origin_intent_id, from_state, to_state, venue, mint,
                     tokens, min_out_lamports, out_lamports, cost_basis_lamports,
                     realized_pnl_lamports, signature, failure, detail, mode, at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                           ?17)
                 ON CONFLICT (intent_id, seq) DO NOTHING",
            )?;
            for row in rows {
                written += statement.execute(rusqlite::params![
                    row.intent_id,
                    row.seq,
                    row.origin_intent_id,
                    row.from_state.map(ExitState::as_str),
                    row.to_state.as_str(),
                    row.venue.map(Venue::as_str),
                    row.mint,
                    row.tokens,
                    row.min_out_lamports,
                    row.out_lamports,
                    row.cost_basis_lamports,
                    row.realized_pnl_lamports,
                    row.signature,
                    row.failure.map(ExitFailure::as_str),
                    row.detail,
                    row.mode.as_str(),
                    row.at_ms,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(written)
    }

    /// The newest step of every exit ever attempted, newest first.
    ///
    /// Read before anything is flattened. An obligation whose exit is already
    /// on the network must not get a second one, and this is the only record of
    /// that which survives the process that sent it — which is exactly the case
    /// that matters, because the process that sent an exit and then died is the
    /// one most likely to be asked to unwind again.
    ///
    /// The signature comes off this exit's own `exit_signed` step rather than
    /// off the newest row, for the reason `open_obligations` gives: the unique
    /// partial index means it exists on one row and no later row can hold a
    /// copy of it.
    pub fn latest_exit_attempts(&self) -> Result<Vec<ExitAttempt>, EngineError> {
        let conn = self.conn.lock();
        // Reachable from an emergency unwind on a process whose migrations
        // never ran, where a missing table means "nothing attempted" rather
        // than an error on the way out.
        if !table_exists(&conn, "intent_transitions")? {
            return Ok(Vec::new());
        }

        let mut statement = conn.prepare_cached(
            "WITH latest AS (
                 SELECT intent_id, MAX(seq) AS seq FROM intent_transitions GROUP BY intent_id
             )
             SELECT t.intent_id, t.origin_intent_id, t.seq, t.to_state, t.from_state,
                    t.venue, t.mint, t.tokens, t.out_lamports, t.cost_basis_lamports,
                    t.realized_pnl_lamports,
                    (SELECT s.signature FROM intent_transitions s
                      WHERE s.intent_id = t.intent_id AND s.signature IS NOT NULL
                      ORDER BY s.seq LIMIT 1),
                    t.failure, t.detail, t.mode, t.at_ms
               FROM intent_transitions t
               JOIN latest l ON l.intent_id = t.intent_id AND l.seq = t.seq
              ORDER BY t.at_ms DESC",
        )?;

        let mut rows = statement.query([])?;
        let mut attempts = Vec::new();
        while let Some(row) = rows.next()? {
            let to_state: String = row.get(3)?;
            let from_state: Option<String> = row.get(4)?;
            let venue: Option<String> = row.get(5)?;
            let failure: Option<String> = row.get(12)?;
            let mode: String = row.get(14)?;
            attempts.push(ExitAttempt {
                intent_id: row.get(0)?,
                origin_intent_id: row.get(1)?,
                seq: row.get(2)?,
                state: stored_as(&to_state, ExitState::parse, "intent_transitions.to_state")?,
                from_state: from_state
                    .as_deref()
                    .map(|text| stored_as(text, ExitState::parse, "intent_transitions.from_state"))
                    .transpose()?,
                venue: venue
                    .as_deref()
                    .map(|text| stored_as(text, Venue::parse, "intent_transitions.venue"))
                    .transpose()?,
                mint: row.get(6)?,
                tokens: row.get(7)?,
                out_lamports: row.get(8)?,
                cost_basis_lamports: row.get(9)?,
                realized_pnl_lamports: row.get(10)?,
                signature: row.get(11)?,
                failure: failure
                    .as_deref()
                    .map(|text| stored_as(text, ExitFailure::parse, "intent_transitions.failure"))
                    .transpose()?,
                detail: row.get(13)?,
                mode: stored_as(&mode, ExecutionMode::parse, "intent_transitions.mode")?,
                at_ms: row.get(15)?,
            });
        }
        Ok(attempts)
    }

    /// What has actually been closed and what it came to, for one mode.
    ///
    /// Summed over the `exit_confirmed` steps, which are the only rows that
    /// carry proceeds. Unconfirmed exits contribute nothing — an exit that is
    /// on the network is not a realized number and counting it as one is how
    /// unrealized gains end up in a total labelled realized.
    pub fn realized_pnl(&self, mode: ExecutionMode) -> Result<RealizedPnl, EngineError> {
        let conn = self.conn.lock();
        if !table_exists(&conn, "intent_transitions")? {
            return Ok(RealizedPnl::default());
        }
        realized_pnl_for(&conn, mode)
    }

    /// Appends one tick per endpoint. Returns how many rows were new.
    pub fn record_tick_metrics(&self, rows: &[TickMetricRow]) -> Result<usize, EngineError> {
        if rows.is_empty() {
            return Ok(0);
        }

        let mut conn = self.conn.lock();
        let transaction = conn.transaction()?;
        let mut written = 0usize;
        {
            let mut statement = transaction.prepare_cached(
                "INSERT INTO tick_metrics (
                     rpc_endpoint, timestamp, latency_ms, dropped_msgs, parsed_per_sec_micros
                 ) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (rpc_endpoint, timestamp) DO NOTHING",
            )?;
            for row in rows {
                written += statement.execute(rusqlite::params![
                    row.rpc_endpoint,
                    row.timestamp_ms,
                    row.latency_ms,
                    row.dropped_msgs,
                    store_rate(row.parsed_per_sec_micros)?,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(written)
    }

    /// Drops tick metrics older than `cutoff_ms`. Returns how many rows went.
    ///
    /// `tick_metrics` is the only table that grows at a fixed rate whether or
    /// not anything is happening, and the only one with a retention policy —
    /// `candidates`, `clusters` and `execution_logs` are the record, and the
    /// record is kept.
    ///
    /// Deleted in bounded chunks, one statement at a time. One statement
    /// removing a week of rows is a single large transaction that bloats the WAL
    /// and holds the writer for the length of it. The chunk is bounded by the
    /// primary key rather than by `rowid`, because `tick_metrics` is
    /// `WITHOUT ROWID` and has none. The file does not shrink either way: the
    /// freed pages go on a free list and the next ticks reuse them, which is
    /// exactly what should happen and is why there is no `VACUUM` here.
    pub fn prune_tick_metrics(&self, cutoff_ms: i64) -> Result<usize, EngineError> {
        let mut removed = 0usize;
        loop {
            // The lock is taken and dropped per chunk rather than held across
            // the loop. Holding it for the whole prune would block the ingest
            // path for the length of it, which is the pause the chunking exists
            // to avoid — bounding the transaction and then not letting anyone
            // else write between transactions would only move the stall.
            let gone = {
                let conn = self.conn.lock();
                conn.execute(
                    "DELETE FROM tick_metrics
                     WHERE (rpc_endpoint, timestamp) IN (
                         SELECT rpc_endpoint, timestamp FROM tick_metrics
                         WHERE timestamp < ?1
                         LIMIT ?2
                     )",
                    rusqlite::params![cutoff_ms, PRUNE_CHUNK],
                )?
            };
            removed += gone;
            if gone < PRUNE_CHUNK {
                return Ok(removed);
            }
        }
    }

    /// Copies what it can out of the WAL without blocking anyone.
    ///
    /// For the background timer. `PASSIVE` gives up on whatever a live reader or
    /// writer is holding rather than waiting, which is the right shape for
    /// something on a schedule. A WAL that keeps growing through this means a
    /// reader is being held open across a long operation — a checkpoint cannot
    /// advance past the oldest snapshot still in use — and that is a bug in the
    /// reader, which the WAL file size is how you notice.
    pub fn checkpoint_passive(&self) -> Result<(), EngineError> {
        let conn = self.conn.lock();
        conn.pragma_update(None, "wal_checkpoint", "PASSIVE")?;
        Ok(())
    }

    /// Runs the WAL back into the main file and drops the connection.
    ///
    /// Called once, on the way out. `TRUNCATE` folds the whole WAL back and
    /// resets it to zero length; it is the one place a blocking checkpoint is
    /// correct, because there is nothing left to block. Skipped if the lock
    /// cannot be taken, for the same reason the panic path gives up.
    pub fn close(&self) {
        if let Some(conn) = self.conn.try_lock_for(PANIC_LOCK_TIMEOUT) {
            let _ = conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        }
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Turns a column's text back into the enum that wrote it.
///
/// The `CHECK` constraints make an unknown value impossible for anything this
/// build wrote, which leaves a file touched by hand or by a newer build — and
/// neither is a value to guess at while reading what is on chain.
fn stored_as<T>(
    text: &str,
    from: impl Fn(&str) -> Option<T>,
    column: &str,
) -> Result<T, EngineError> {
    from(text).ok_or_else(|| {
        EngineError::Database(format!(
            "{column} holds {text:?}, which this build does not know"
        ))
    })
}

/// Sums one mode's confirmed exits. Takes the connection rather than the lock,
/// so `health` can ask for all three modes inside one snapshot.
fn realized_pnl_for(conn: &Connection, mode: ExecutionMode) -> Result<RealizedPnl, EngineError> {
    // `COALESCE` because `SUM` over no rows is NULL, and a mode nothing has
    // traded in should read as zero rather than fail to decode.
    Ok(conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(out_lamports), 0),
                COALESCE(SUM(cost_basis_lamports), 0),
                COALESCE(SUM(realized_pnl_lamports), 0)
           FROM intent_transitions
          WHERE to_state = 'exit_confirmed' AND mode = ?1",
        [mode.as_str()],
        |row| {
            Ok(RealizedPnl {
                closed: row.get(0)?,
                proceeds_lamports: row.get(1)?,
                cost_basis_lamports: row.get(2)?,
                realized_lamports: row.get(3)?,
            })
        },
    )?)
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool, EngineError> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

fn count(conn: &Connection, sql: &str) -> Result<i64, EngineError> {
    Ok(conn.query_row(sql, [], |row| row.get(0))?)
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::types::MICROS_DENOMINATOR;

    const AT_MS: i64 = 1_700_000_000_000;

    /// A file of its own per test. Every one of these runs against a real
    /// SQLite file rather than `:memory:`, because WAL, `mmap_size` and the
    /// checkpoint pragmas are the things under test and none of them mean
    /// anything without a file.
    struct TempDb(PathBuf);

    impl TempDb {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "sts-db-{name}-{}-{}.db",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
            TempDb(path)
        }

        fn open(&self) -> Database {
            Database::open_at(&self.0, AT_MS).expect("opens")
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
            }
        }
    }

    fn candidate() -> IngestCandidateRow {
        IngestCandidateRow {
            source: "helius".to_string(),
            slot: 1_020,
            account: "CurveAccount1111111111111111111111111111111".to_string(),
            program: "Program11111111111111111111111111111111111".to_string(),
            creator: Some("Creator111111111111111111111111111111111111".to_string()),
            route: "fast_path".to_string(),
            market_cap_usd_cents: 4_000_431,
            pool_lamports: 62_650_000_000,
            curve_progress_bps: 7_370,
            observed_at_ms: AT_MS,
            dispatch_latency_us: 412,
        }
    }

    fn cluster() -> ClusterRow {
        ClusterRow {
            cluster_id: "cluster-a".to_string(),
            version: 1,
            root_wallet: "Root1111111111111111111111111111111111111111".to_string(),
            metrics: SybilClusterMetrics::new(9, 6_412, 910_000, 770_000, 120_000),
            flag_sybil: true,
            computed_at_ms: AT_MS,
        }
    }

    fn step(seq: i64, state: ExecutionState, prev: Option<ExecutionState>) -> ExecutionLogRow {
        ExecutionLogRow {
            intent_id: "01912d3f-7a10-7c00-8000-000000000001".to_string(),
            seq,
            mint: "Mint1111111111111111111111111111111111111111".to_string(),
            state,
            prev_state: prev,
            side: Side::Buy,
            size_lamports: 250_000_000,
            price_q18: None,
            signature: None,
            latency_ms: Some(12),
            needs_unwind: false,
            mode: ExecutionMode::Paper,
            abort_reason: None,
            at_ms: AT_MS,
        }
    }

    // -- connection ---------------------------------------------------------

    #[test]
    fn every_pragma_in_the_specification_is_actually_set() {
        let temp = TempDb::new("pragmas");
        let db = temp.open();
        let pragmas = db.pragmas();

        assert_eq!(pragmas.journal_mode, "wal");
        assert_eq!(pragmas.synchronous, 1, "NORMAL");
        assert!(pragmas.foreign_keys, "off by default on every connection");
        assert_eq!(
            pragmas.cache_size, -65_536,
            "negative counts KiB, so 64 MiB"
        );
        assert_eq!(pragmas.mmap_size, 268_435_456);
        assert_eq!(pragmas.temp_store, 2, "MEMORY");
        assert_eq!(pragmas.wal_autocheckpoint, 4_000);
    }

    #[test]
    fn a_second_connection_gets_the_pragmas_too() {
        let temp = TempDb::new("second-conn");
        let first = temp.open();
        // `foreign_keys` is the one SQLite resets to off for every new
        // connection no matter what the last one did, so it is the one worth
        // asserting a second time.
        let second = temp.open();
        assert!(first.pragmas().foreign_keys);
        assert!(second.pragmas().foreign_keys);
        assert_eq!(second.pragmas().journal_mode, "wal");
    }

    // -- migrations ---------------------------------------------------------

    #[test]
    fn migrating_is_idempotent_and_records_what_it_did() {
        let temp = TempDb::new("migrate");
        let db = temp.open();
        assert_eq!(db.schema_version(), latest_schema_version());

        let applied: Vec<(i64, i64, String)> = {
            let conn = db.conn.lock();
            let mut statement = conn
                .prepare("SELECT version, applied_at_ms, checksum FROM schema_migrations")
                .expect("prepares");
            let rows = statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                .expect("queries");
            rows.collect::<Result<_, _>>().expect("collects")
        };
        assert_eq!(applied.len(), MIGRATIONS.len());
        assert_eq!(applied[0].0, 1);
        assert_eq!(applied[0].1, AT_MS);
        assert_eq!(applied[0].2, checksum(MIGRATION_0001));

        drop(db);
        // Opening again applies nothing and changes nothing.
        let again = Database::open_at(temp.path(), AT_MS + 5_000).expect("opens again");
        let rows: i64 = {
            let conn = again.conn.lock();
            count(&conn, "SELECT COUNT(*) FROM schema_migrations").expect("counts")
        };
        assert_eq!(rows, MIGRATIONS.len() as i64);
        assert_eq!(again.schema_version(), latest_schema_version());
    }

    #[test]
    fn a_database_from_a_newer_build_is_refused_rather_than_read() {
        let temp = TempDb::new("future");
        drop(temp.open());

        {
            let conn = Connection::open(temp.path()).expect("opens");
            conn.execute(
                "INSERT INTO schema_migrations (version, applied_at_ms, checksum)
                 VALUES (?1, ?2, 'fnv1a64:0000000000000000')",
                rusqlite::params![latest_schema_version() + 1, AT_MS],
            )
            .expect("writes a version from the future");
        }

        let Err(EngineError::Database(message)) = Database::open_at(temp.path(), AT_MS) else {
            panic!("a database from the future should be refused, not opened");
        };
        assert!(message.contains("refusing to open"), "{message}");
    }

    #[test]
    fn a_migration_edited_after_it_shipped_is_caught_on_the_next_open() {
        let temp = TempDb::new("checksum");
        drop(temp.open());

        {
            let conn = Connection::open(temp.path()).expect("opens");
            conn.execute(
                "UPDATE schema_migrations SET checksum = 'fnv1a64:deadbeefdeadbeef' WHERE version = 1",
                [],
            )
            .expect("rewrites the checksum");
        }

        let Err(EngineError::Database(message)) = Database::open_at(temp.path(), AT_MS) else {
            panic!("an edited migration should be refused, not opened");
        };
        assert!(message.contains("two different schemas"), "{message}");
    }

    #[test]
    fn migration_one_lands_on_a_database_that_predates_the_ledger() {
        let temp = TempDb::new("pre-ledger");
        // A file with some of the schema already in it and no `schema_migrations`
        // — which is every database written before this module existed.
        {
            let conn = Connection::open(temp.path()).expect("opens");
            conn.execute_batch(
                "CREATE TABLE audit_log (
                     id         INTEGER PRIMARY KEY AUTOINCREMENT,
                     event_type TEXT    NOT NULL,
                     payload    TEXT    NOT NULL,
                     created_at INTEGER NOT NULL
                 );",
            )
            .expect("creates the old table");
        }

        let db = temp.open();
        assert_eq!(db.schema_version(), latest_schema_version());
        assert_eq!(db.health().expect("health").candidates, 0);
        assert!(db
            .record_audit("test", &serde_json::json!({}), AT_MS)
            .is_ok());
    }

    // -- candidates ---------------------------------------------------------

    #[test]
    fn the_same_observation_twice_is_one_row_and_two_providers_are_two() {
        let temp = TempDb::new("candidates");
        let db = temp.open();
        db.ensure_ingest_schema().expect("the schema is there");
        db.ensure_ingest_schema().expect("and still is");

        let row = candidate();
        assert_eq!(
            db.record_ingest_candidates(std::slice::from_ref(&row))
                .expect("writes"),
            1
        );
        assert_eq!(
            db.record_ingest_candidates(std::slice::from_ref(&row))
                .expect("writes"),
            0,
            "replaying a fixture does not double the history"
        );

        let mut other_provider = row.clone();
        other_provider.source = "quicknode".to_string();
        assert_eq!(
            db.record_ingest_candidates(&[other_provider])
                .expect("writes"),
            1,
            "the provider is part of the identity, so agreement is visible"
        );

        let mut later_slot = row;
        later_slot.slot = 1_021;
        assert_eq!(
            db.record_ingest_candidates(&[later_slot]).expect("writes"),
            1
        );

        assert_eq!(db.ingest_candidate_count().expect("counts"), 3);
        assert_eq!(db.record_ingest_candidates(&[]).expect("writes"), 0);
    }

    #[test]
    fn an_ingest_row_maps_onto_the_candidates_columns() {
        let temp = TempDb::new("mapping");
        let db = temp.open();

        let mut standard = candidate();
        standard.route = "standard".to_string();
        standard.slot = 2_000;
        standard.creator = None;
        db.record_ingest_candidates(&[candidate(), standard])
            .expect("writes");

        let conn = db.conn.lock();
        let (curve_account, detected_slot, liquidity, fast_path, mint, symbol, creator): (
            String,
            i64,
            i64,
            i64,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT curve_account, detected_slot, liquidity_lamports, fast_path_eligible,
                        mint, symbol, creator_wallet
                 FROM candidates WHERE detected_slot = 1020",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .expect("reads it back");

        assert_eq!(
            curve_account,
            candidate().account,
            "account is curve_account"
        );
        assert_eq!(detected_slot, 1_020, "slot is detected_slot");
        assert_eq!(
            liquidity, 62_650_000_000,
            "pool_lamports is liquidity_lamports"
        );
        assert_eq!(fast_path, 1, "the fast path route is 1");
        assert_eq!(mint, None, "the curve does not derive back to the mint");
        assert_eq!(
            symbol, None,
            "there is no symbol without the create instruction"
        );
        assert_eq!(creator, candidate().creator);

        let standard_route: i64 = conn
            .query_row(
                "SELECT fast_path_eligible FROM candidates WHERE detected_slot = 2000",
                [],
                |row| row.get(0),
            )
            .expect("reads the standard row");
        assert_eq!(standard_route, 0);

        let no_creator: Option<String> = conn
            .query_row(
                "SELECT creator_wallet FROM candidates WHERE detected_slot = 2000",
                [],
                |row| row.get(0),
            )
            .expect("reads it");
        assert_eq!(
            no_creator, None,
            "an unknown creator is null, not thirty-two zero bytes"
        );
    }

    #[test]
    fn basis_points_are_stored_as_the_integer_they_were_computed_as() {
        let temp = TempDb::new("bps");
        let db = temp.open();
        db.record_ingest_candidates(&[candidate()]).expect("writes");

        let conn = db.conn.lock();
        // `get::<_, i64>` on a column holding a float would fail the type
        // conversion, so this passing is the assertion: nothing turned the
        // basis points into a float on the way in.
        let bps: i64 = conn
            .query_row("SELECT curve_progress_bps FROM candidates", [], |row| {
                row.get(0)
            })
            .expect("reads an integer");
        assert_eq!(bps, 7_370);

        let stored_type: String = conn
            .query_row(
                "SELECT typeof(curve_progress_bps) FROM candidates",
                [],
                |row| row.get(0),
            )
            .expect("reads the type");
        assert_eq!(stored_type, "integer");

        // Migration 4 took the `bonding_curve_pct` generated column with it.
        // The percentage was `curve_progress_bps / 100.0` — a float derived on
        // read from an integer that was already the exact answer — and a
        // caller that wants it can do that division without the schema keeping
        // a `REAL` around to offer it.
        //
        // `table_xinfo` and not `table_info`: the latter omits generated
        // columns altogether, so asking it whether this one is gone answers
        // "yes" whether or not it is, which is a test that cannot fail.
        let has_percent: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_xinfo('candidates') \
                 WHERE name = 'bonding_curve_pct'",
                [],
                |row| row.get(0),
            )
            .expect("reads the column list");
        assert_eq!(has_percent, 0);
    }

    #[test]
    fn the_candidates_checks_reject_what_cannot_be_true() {
        let temp = TempDb::new("candidate-checks");
        let db = temp.open();

        let mut graduated_and_then_some = candidate();
        graduated_and_then_some.curve_progress_bps = 10_001;
        assert!(db
            .record_ingest_candidates(&[graduated_and_then_some])
            .is_err());

        let mut before_the_chain_started = candidate();
        before_the_chain_started.slot = 0;
        assert!(db
            .record_ingest_candidates(&[before_the_chain_started])
            .is_err());

        let mut negative_pool = candidate();
        negative_pool.pool_lamports = -1;
        assert!(db.record_ingest_candidates(&[negative_pool]).is_err());

        assert_eq!(
            db.ingest_candidate_count().expect("counts"),
            0,
            "a rejected batch rolls back whole"
        );
    }

    // -- clusters -----------------------------------------------------------

    #[test]
    fn a_cluster_round_trips_with_its_basis_points_intact() {
        let temp = TempDb::new("clusters");
        let db = temp.open();
        assert_eq!(db.record_clusters(&[cluster()]).expect("writes"), 1);
        assert_eq!(
            db.record_clusters(&[cluster()]).expect("writes"),
            0,
            "the same version of the same cluster is one row"
        );

        let mut recomputed = cluster();
        recomputed.version = 2;
        recomputed.flag_sybil = false;
        assert_eq!(
            db.record_clusters(&[recomputed]).expect("writes"),
            1,
            "a recomputation adds a version rather than overwriting one"
        );

        let conn = db.conn.lock();
        let (hhi, hhi_type, temporal, temporal_type): (i64, String, i64, String) = conn
            .query_row(
                "SELECT hhi, typeof(hhi), temporal_influence_micros, \
                        typeof(temporal_influence_micros) \
                 FROM clusters WHERE version = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("reads it back");
        assert_eq!(hhi, 6_412, "the HHI is basis points, not a rounded percent");
        assert_eq!(hhi_type, "integer");
        // Exactly what went in, not a value near it. The `f32` this column used
        // to hold could not promise that, which is what migration 4 was for.
        assert_eq!(temporal, 910_000);
        assert_eq!(temporal_type, "integer");

        let versions: i64 = count(&conn, "SELECT COUNT(*) FROM clusters").expect("counts");
        assert_eq!(
            versions, 2,
            "last week's numbers are still there to point at"
        );
    }

    #[test]
    fn a_broken_score_cannot_be_stored_as_a_clean_one() {
        let temp = TempDb::new("cluster-scores");
        let db = temp.open();

        // `SybilClusterMetrics::new` clamps, so these are built past it — the
        // constraints exist for anything that reaches the file some other way.
        // There is no infinity and no NaN to test with any more: migration 4
        // made these columns integers, which is a shape that cannot spell
        // either. What is left is the overshoot, and the `CHECK` is what
        // rejects it.
        let mut past_a_whole_unit = cluster();
        past_a_whole_unit.metrics.spectral_separation_micros = MICROS_DENOMINATOR + 1;
        assert!(
            db.record_clusters(&[past_a_whole_unit]).is_err(),
            "a score past one is out of range, and the CHECK is what rejects it"
        );

        let mut entropy_past_one = cluster();
        entropy_past_one.metrics.interaction_entropy_micros = u32::MAX;
        assert!(db.record_clusters(&[entropy_past_one]).is_err());

        let mut impossible_hhi = cluster();
        impossible_hhi.metrics.holding_hhi_bps = 10_001;
        assert!(db.record_clusters(&[impossible_hhi]).is_err());

        let mut no_wallets = cluster();
        no_wallets.metrics.wallet_count = 0;
        assert!(db.record_clusters(&[no_wallets]).is_err());

        let conn = db.conn.lock();
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM clusters").expect("counts"),
            0
        );
    }

    // -- execution logs -----------------------------------------------------

    #[test]
    fn an_intents_history_is_one_row_per_step() {
        let temp = TempDb::new("executions");
        let db = temp.open();

        let mut walked = ExecutionState::IntentCreated;
        let mut rows = vec![step(0, walked, None)];
        for (seq, next) in [
            ExecutionState::Validated,
            ExecutionState::Sent,
            ExecutionState::Confirmed,
            ExecutionState::Completed,
        ]
        .into_iter()
        .enumerate()
        {
            let prev = walked;
            walked = prev.transition_to(next).expect("a legal step");
            rows.push(step(seq as i64 + 1, walked, Some(prev)));
        }

        assert_eq!(db.record_execution_logs(&rows).expect("writes"), 5);

        let conn = db.conn.lock();
        let newest: String = conn
            .query_row(
                "SELECT state FROM execution_logs ORDER BY seq DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("reads");
        assert_eq!(
            ExecutionState::parse(&newest),
            Some(ExecutionState::Completed),
            "where an order is now is the newest row for its intent"
        );
    }

    #[test]
    fn writing_the_same_step_twice_is_a_no_op_but_a_repeated_signature_is_not() {
        let temp = TempDb::new("execution-dupes");
        let db = temp.open();

        let first = step(0, ExecutionState::IntentCreated, None);
        assert_eq!(
            db.record_execution_logs(std::slice::from_ref(&first))
                .expect("writes"),
            1
        );
        assert_eq!(
            db.record_execution_logs(&[first]).expect("writes"),
            0,
            "a retry writing a step it already wrote is idempotent"
        );

        let mut sent = step(1, ExecutionState::Sent, Some(ExecutionState::Validated));
        sent.signature = Some("Sig1111111111111111111111111111111111111111".to_string());
        assert_eq!(
            db.record_execution_logs(&[sent.clone()]).expect("writes"),
            1
        );

        let mut someone_elses = sent.clone();
        someone_elses.intent_id = "01912d3f-7a10-7c00-8000-000000000002".to_string();
        assert!(
            db.record_execution_logs(&[someone_elses]).is_err(),
            "two intents claiming one signature fails at the insert, not in a reconciliation"
        );

        let conn = db.conn.lock();
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM execution_logs").expect("counts"),
            2,
            "and the failed batch rolled back"
        );
    }

    #[test]
    fn aborting_after_send_records_an_open_obligation() {
        let temp = TempDb::new("unwind");
        let db = temp.open();

        let outcome = ExecutionState::Confirmed
            .abort(AbortReason::KillSwitch)
            .expect("abort always works from an active state");
        let row = ExecutionLogRow::aborted(
            "01912d3f-7a10-7c00-8000-000000000003".to_string(),
            4,
            "Mint1111111111111111111111111111111111111111".to_string(),
            outcome,
            Side::Buy,
            250_000_000,
            Some("Sig2222222222222222222222222222222222222222".to_string()),
            ExecutionMode::Live,
            AT_MS,
        );
        assert!(
            row.needs_unwind,
            "money was at risk when it was given up on"
        );
        db.record_execution_logs(&[row]).expect("writes");

        // Aborting before anything was sent leaves nothing behind.
        let harmless = ExecutionState::Validated
            .abort(AbortReason::Stale)
            .expect("abort");
        let row = ExecutionLogRow::aborted(
            "01912d3f-7a10-7c00-8000-000000000004".to_string(),
            2,
            "Mint2222222222222222222222222222222222222222".to_string(),
            harmless,
            Side::Buy,
            250_000_000,
            None,
            ExecutionMode::Live,
            AT_MS,
        );
        assert!(!row.needs_unwind);
        db.record_execution_logs(&[row]).expect("writes");

        let health = db.health().expect("health");
        assert_eq!(health.execution_logs, 2);
        assert_eq!(health.needs_unwind, 1);
        assert_eq!(health.open_executions, 0, "both of these are aborted");

        let conn = db.conn.lock();
        let (state, prev, reason): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT state, prev_state, abort_reason FROM execution_logs WHERE needs_unwind = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("reads");
        assert_eq!(state, "aborted");
        assert_eq!(prev.as_deref(), Some("confirmed"));
        assert_eq!(reason.as_deref(), Some("kill_switch"));
    }

    #[test]
    fn halted_is_not_a_mode_an_execution_can_be_logged_under() {
        assert_eq!(
            ExecutionMode::from_operating_mode(OperatingMode::Halted),
            None,
            "halted opens nothing, so there is no execution to log"
        );
        for (mode, expected) in [
            (OperatingMode::Live, ExecutionMode::Live),
            (OperatingMode::Paper, ExecutionMode::Paper),
            (OperatingMode::Replay, ExecutionMode::Replay),
        ] {
            let narrowed = ExecutionMode::from_operating_mode(mode).expect("a real mode");
            assert_eq!(narrowed, expected);
            assert_eq!(narrowed.as_str(), mode.as_str(), "one spelling, both sides");
        }
    }

    #[test]
    fn the_execution_checks_reject_what_cannot_be_true() {
        let temp = TempDb::new("execution-checks");
        let db = temp.open();

        let mut nothing_at_stake = step(0, ExecutionState::Sent, None);
        nothing_at_stake.size_lamports = 0;
        assert!(db.record_execution_logs(&[nothing_at_stake]).is_err());

        let mut free = step(0, ExecutionState::Confirmed, None);
        free.price_q18 = Some(Q18::from_raw(0));
        assert!(db.record_execution_logs(&[free]).is_err());

        let mut backwards = step(0, ExecutionState::Sent, None);
        backwards.latency_ms = Some(-1);
        assert!(db.record_execution_logs(&[backwards]).is_err());

        let mut before_the_beginning = step(-1, ExecutionState::IntentCreated, None);
        before_the_beginning.seq = -1;
        assert!(db.record_execution_logs(&[before_the_beginning]).is_err());
    }

    // -- tick metrics -------------------------------------------------------

    #[test]
    fn tick_metrics_are_one_row_per_endpoint_per_tick_and_are_pruned_in_chunks() {
        let temp = TempDb::new("ticks");
        let db = temp.open();

        // Two endpoints across enough ticks to make the prune loop go round
        // more than once.
        let rows: Vec<TickMetricRow> = (0..(PRUNE_CHUNK as i64 + 500))
            .flat_map(|tick| {
                ["helius", "quicknode"]
                    .into_iter()
                    .map(move |host| TickMetricRow {
                        rpc_endpoint: host.to_string(),
                        timestamp_ms: AT_MS + tick,
                        latency_ms: 40 + (tick % 7),
                        dropped_msgs: 0,
                        parsed_per_sec_micros: 812_500_000,
                    })
            })
            .collect();
        let total = rows.len();
        assert_eq!(db.record_tick_metrics(&rows).expect("writes"), total);
        assert_eq!(
            db.record_tick_metrics(&rows).expect("writes"),
            0,
            "one row per endpoint per tick"
        );

        // Keep the last 100 ticks of each endpoint, drop the rest.
        let cutoff = AT_MS + PRUNE_CHUNK as i64 + 400;
        let removed = db.prune_tick_metrics(cutoff).expect("prunes");
        assert_eq!(removed, total - 200);

        let conn = db.conn.lock();
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM tick_metrics").expect("counts"),
            200
        );
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM tick_metrics WHERE timestamp < 0"
            )
            .expect("counts"),
            0
        );
    }

    #[test]
    fn pruning_an_empty_table_removes_nothing_and_returns() {
        let temp = TempDb::new("prune-empty");
        let db = temp.open();
        assert_eq!(db.prune_tick_metrics(AT_MS).expect("prunes"), 0);
    }

    // -- health, audit and checkpoints --------------------------------------

    #[test]
    fn health_reports_the_tables_that_exist_now() {
        let temp = TempDb::new("health");
        let db = temp.open();

        let empty = db.health().expect("health");
        assert!(
            empty.schema_present,
            "open migrated, so the schema is there"
        );
        assert_eq!(empty.schema_version, latest_schema_version());
        assert_eq!(empty.candidates, 0);
        assert_eq!(empty.last_audit_at_ms, None);
        assert_eq!(empty.path, temp.path().display().to_string());

        db.record_ingest_candidates(&[candidate()]).expect("writes");
        db.record_clusters(&[cluster()]).expect("writes");
        db.record_execution_logs(&[
            step(0, ExecutionState::IntentCreated, None),
            step(1, ExecutionState::Sent, Some(ExecutionState::Validated)),
        ])
        .expect("writes");
        let id = db
            .record_audit("kill_switch", &serde_json::json!({ "why": "test" }), AT_MS)
            .expect("audits");
        assert!(id > 0);

        let filled = db.health().expect("health");
        assert_eq!(filled.candidates, 1);
        assert_eq!(filled.clusters, 1);
        assert_eq!(filled.execution_logs, 2);
        assert_eq!(
            filled.open_executions, 1,
            "only the sent one has money at risk"
        );
        assert_eq!(filled.needs_unwind, 0);
        assert_eq!(filled.last_audit_at_ms, Some(AT_MS));
    }

    #[test]
    fn the_panic_path_writes_its_row_without_the_schema_check() {
        let temp = TempDb::new("panic-audit");
        let db = temp.open();
        assert!(db.record_audit_best_effort("panic", &serde_json::json!({ "at": "here" }), AT_MS));
        assert_eq!(db.health().expect("health").last_audit_at_ms, Some(AT_MS));
    }

    #[test]
    fn checkpointing_folds_the_wal_back_and_leaves_the_rows_where_they_were() {
        let temp = TempDb::new("checkpoint");
        let db = temp.open();
        db.record_ingest_candidates(&[candidate()]).expect("writes");

        db.checkpoint_passive().expect("passive checkpoint");
        assert_eq!(db.ingest_candidate_count().expect("counts"), 1);

        db.close();
        let wal = PathBuf::from(format!("{}-wal", temp.path().display()));
        let wal_len = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        assert_eq!(wal_len, 0, "TRUNCATE resets the WAL to zero length");

        // And the rows survived the fold.
        let reopened = temp.open();
        assert_eq!(reopened.ingest_candidate_count().expect("counts"), 1);
    }

    #[test]
    fn the_migration_checksum_changes_when_the_sql_does() {
        assert_eq!(checksum("SELECT 1"), checksum("SELECT 1"));
        assert_ne!(checksum("SELECT 1"), checksum("SELECT 2"));
        assert!(checksum(MIGRATION_0001).starts_with("fnv1a64:"));
    }

    // -- open obligations ---------------------------------------------------

    /// One intent's history, as the rows it would actually have written.
    fn history(intent: &str, steps: &[(ExecutionState, Option<&str>)]) -> Vec<ExecutionLogRow> {
        let mut prev = None;
        let mut rows = Vec::new();
        for (seq, (state, signature)) in steps.iter().enumerate() {
            rows.push(ExecutionLogRow {
                intent_id: intent.to_string(),
                seq: seq as i64,
                mint: format!("Mint{intent}"),
                state: *state,
                prev_state: prev,
                side: Side::Buy,
                size_lamports: 250_000_000,
                price_q18: None,
                signature: signature.map(str::to_string),
                latency_ms: None,
                // Exactly what `AbortOutcome` would have produced: set only on
                // an abort, and only when the state it aborted from had money
                // at risk. `is_terminal` would be wrong here — it also covers
                // `completed`, which is a position that was closed properly.
                needs_unwind: *state == ExecutionState::Aborted
                    && prev.map(ExecutionState::has_money_at_risk).unwrap_or(false),
                mode: ExecutionMode::Live,
                abort_reason: (*state == ExecutionState::Aborted).then_some(AbortReason::Operator),
                at_ms: AT_MS + seq as i64,
            });
            prev = Some(*state);
        }
        rows
    }

    #[test]
    fn open_obligations_finds_both_the_unfinished_and_the_abandoned() {
        use ExecutionState::*;
        let temp = TempDb::new("obligations");
        let db = temp.open();

        for rows in [
            // Died mid-flight: the newest state still has money at risk. Only
            // this arm's query finds it.
            history(
                "aaa",
                &[
                    (IntentCreated, None),
                    (Validated, None),
                    (Sent, Some("SigAAA")),
                ],
            ),
            history(
                "bbb",
                &[
                    (IntentCreated, None),
                    (Validated, None),
                    (Sent, Some("SigBBB")),
                    (Confirmed, None),
                ],
            ),
            // Finished by the engine, left something behind. Only the other
            // arm finds this one.
            history(
                "ccc",
                &[
                    (IntentCreated, None),
                    (Validated, None),
                    (Sent, Some("SigCCC")),
                    (Confirmed, None),
                    (Aborted, None),
                ],
            ),
            // Neither: closed and booked.
            history(
                "ddd",
                &[
                    (IntentCreated, None),
                    (Validated, None),
                    (Sent, Some("SigDDD")),
                    (Confirmed, None),
                    (Completed, None),
                ],
            ),
            // Neither: given up on before anything went out.
            history(
                "eee",
                &[(IntentCreated, None), (Validated, None), (Aborted, None)],
            ),
            // Neither: a plan nobody acted on.
            history("fff", &[(IntentCreated, None)]),
        ] {
            db.record_execution_logs(&rows).expect("writes");
        }

        let open = db.open_obligations().expect("reads");
        let ids: Vec<&str> = open.iter().map(|o| o.intent_id.as_str()).collect();
        assert_eq!(
            ids,
            ["ccc", "bbb", "aaa"],
            "both arms, newest first, and nothing that has no money out"
        );

        let by_id = |id: &str| {
            open.iter()
                .find(|o| o.intent_id == id)
                .expect("present")
                .clone()
        };

        let unfinished = by_id("aaa");
        assert_eq!(unfinished.state, Sent);
        assert_eq!(unfinished.seq, 2, "the newest row, so the next one is 3");
        assert_eq!(unfinished.at_risk_in(), Some(Sent));
        assert!(
            !unfinished.needs_unwind,
            "nothing aborted it, so nothing set the flag"
        );
        assert_eq!(unfinished.signature.as_deref(), Some("SigAAA"));
        assert_eq!(unfinished.side, Side::Buy);
        assert_eq!(unfinished.mode, ExecutionMode::Live);

        let abandoned = by_id("ccc");
        assert_eq!(abandoned.state, Aborted);
        assert!(abandoned.needs_unwind);
        assert_eq!(
            abandoned.at_risk_in(),
            Some(Confirmed),
            "aborted says nothing about what is on chain; the state before it does"
        );

        // The signature lives on the `sent` step and the unique index means no
        // later row can hold a copy, so an obligation past `sent` only has one
        // at all because the query goes back for it. Without that it would be
        // null for every position that actually exists.
        assert_eq!(
            by_id("bbb").signature.as_deref(),
            Some("SigBBB"),
            "newest row is confirmed"
        );
        assert_eq!(
            abandoned.signature.as_deref(),
            Some("SigCCC"),
            "newest row is aborted"
        );
    }

    #[test]
    fn open_obligations_reads_every_mode_rather_than_hiding_one() {
        use ExecutionState::*;
        let temp = TempDb::new("obligations-modes");
        let db = temp.open();

        for (intent, mode) in [
            ("live", ExecutionMode::Live),
            ("paper", ExecutionMode::Paper),
        ] {
            let mut rows = history(
                intent,
                &[(IntentCreated, None), (Validated, None), (Sent, None)],
            );
            for row in &mut rows {
                row.mode = mode;
            }
            db.record_execution_logs(&rows).expect("writes");
        }

        let open = db.open_obligations().expect("reads");
        assert_eq!(
            open.len(),
            2,
            "a paper obligation comes back labelled, not dropped"
        );
        assert!(open.iter().any(|o| o.mode == ExecutionMode::Paper));
        assert!(open.iter().any(|o| o.mode == ExecutionMode::Live));
    }

    #[test]
    fn an_empty_ledger_owes_nothing() {
        let temp = TempDb::new("obligations-empty");
        let db = temp.open();
        assert!(db.open_obligations().expect("reads").is_empty());
    }

    #[test]
    fn a_stored_value_this_build_cannot_name_is_not_guessed_at() {
        for (text, side) in [
            ("buy", Some(Side::Buy)),
            ("sell", Some(Side::Sell)),
            ("hold", None),
        ] {
            assert_eq!(Side::parse(text), side);
        }
        for (text, mode) in [
            ("live", Some(ExecutionMode::Live)),
            ("paper", Some(ExecutionMode::Paper)),
            ("replay", Some(ExecutionMode::Replay)),
            // Narrowed out of `OperatingMode` on purpose: halted opens nothing,
            // so no execution was ever logged under it.
            ("halted", None),
        ] {
            assert_eq!(ExecutionMode::parse(text), mode);
        }

        assert!(stored_as("buy", Side::parse, "side").is_ok());
        let refused = stored_as("hold", Side::parse, "side").expect_err("not a side");
        assert!(
            refused.to_string().contains("side") && refused.to_string().contains("hold"),
            "the error names the column and the value: {refused}"
        );
    }

    // -- the exit ledger ----------------------------------------------------

    fn exit_step(
        intent: &str,
        seq: i64,
        origin: &str,
        from: Option<ExitState>,
        to: ExitState,
    ) -> IntentTransitionRow {
        IntentTransitionRow {
            intent_id: intent.to_string(),
            seq,
            origin_intent_id: origin.to_string(),
            from_state: from,
            to_state: to,
            venue: Some(Venue::PumpFunCurve),
            mint: "MintExit".to_string(),
            tokens: Some(1_000_000_000),
            min_out_lamports: Some(180_000_000),
            out_lamports: None,
            cost_basis_lamports: 250_000_000,
            realized_pnl_lamports: None,
            signature: None,
            failure: None,
            detail: None,
            mode: ExecutionMode::Live,
            at_ms: AT_MS,
        }
    }

    #[test]
    fn the_exit_ledger_records_a_whole_exit_and_reads_back_the_newest_step() {
        let temp = TempDb::new("exit-ledger");
        let db = temp.open();

        let mut signed = exit_step(
            "exit-1",
            1,
            "origin-1",
            Some(ExitState::ExitConstructed),
            ExitState::ExitSigned,
        );
        signed.signature = Some("SigExit1".to_string());
        let mut confirmed = exit_step(
            "exit-1",
            3,
            "origin-1",
            Some(ExitState::ExitBroadcast),
            ExitState::ExitConfirmed,
        );
        confirmed.out_lamports = Some(240_000_000);
        confirmed.realized_pnl_lamports = Some(-10_000_000);

        let written = db
            .record_intent_transitions(&[
                exit_step("exit-1", 0, "origin-1", None, ExitState::ExitConstructed),
                signed,
                exit_step(
                    "exit-1",
                    2,
                    "origin-1",
                    Some(ExitState::ExitSigned),
                    ExitState::ExitBroadcast,
                ),
                confirmed,
            ])
            .expect("writes");
        assert_eq!(written, 4);

        let attempts = db.latest_exit_attempts().expect("reads");
        assert_eq!(attempts.len(), 1, "one exit, whatever its step count");
        let attempt = &attempts[0];
        assert_eq!(attempt.intent_id, "exit-1");
        assert_eq!(attempt.origin_intent_id, "origin-1");
        assert_eq!(attempt.seq, 3);
        assert_eq!(attempt.state, ExitState::ExitConfirmed);
        assert_eq!(attempt.from_state, Some(ExitState::ExitBroadcast));
        assert_eq!(attempt.venue, Some(Venue::PumpFunCurve));
        assert_eq!(attempt.out_lamports, Some(240_000_000));
        assert_eq!(attempt.realized_pnl_lamports, Some(-10_000_000));
        assert_eq!(
            attempt.signature.as_deref(),
            Some("SigExit1"),
            "taken off the step that has one, not off the newest row"
        );
        assert!(attempt.is_settled());
        assert!(
            attempt.blocks_retry(),
            "a settled position is not sold again"
        );
    }

    #[test]
    fn an_exit_that_failed_before_the_network_may_be_retried_and_one_after_it_may_not() {
        let temp = TempDb::new("exit-retry");
        let db = temp.open();

        let mut early = exit_step(
            "exit-early",
            1,
            "origin-early",
            Some(ExitState::ExitConstructed),
            ExitState::ExitFailed,
        );
        early.failure = Some(ExitFailure::Signing);
        early.detail = Some("the signer refused".to_string());
        let mut late = exit_step(
            "exit-late",
            1,
            "origin-late",
            Some(ExitState::ExitBroadcast),
            ExitState::ExitFailed,
        );
        late.failure = Some(ExitFailure::NotConfirmed);
        late.detail = Some("it never landed".to_string());

        db.record_intent_transitions(&[early, late])
            .expect("writes");

        let attempts = db.latest_exit_attempts().expect("reads");
        let by = |origin: &str| {
            attempts
                .iter()
                .find(|a| a.origin_intent_id == origin)
                .expect("present")
                .clone()
        };

        let early = by("origin-early");
        assert!(
            !early.left_on_network(),
            "nothing was ever sent for this one"
        );
        assert!(
            !early.blocks_retry(),
            "so building another exit changes nothing twice"
        );
        assert_eq!(early.failure, Some(ExitFailure::Signing));
        assert_eq!(early.detail.as_deref(), Some("the signer refused"));

        let late = by("origin-late");
        assert!(
            late.left_on_network(),
            "a broadcast that never confirmed may still have sold the position"
        );
        assert!(
            late.blocks_retry(),
            "so it is reconciled rather than sold again"
        );
    }

    #[test]
    fn a_failure_before_the_exit_was_routed_stores_nulls_rather_than_zeroes() {
        let temp = TempDb::new("exit-unrouted");
        let db = temp.open();

        let mut unrouted = exit_step("exit-x", 0, "origin-x", None, ExitState::ExitFailed);
        unrouted.venue = None;
        unrouted.tokens = None;
        unrouted.min_out_lamports = None;
        unrouted.failure = Some(ExitFailure::NoRoute);
        unrouted.detail = Some("the pool is depleted".to_string());
        db.record_intent_transitions(&[unrouted]).expect("writes");

        let attempt = db.latest_exit_attempts().expect("reads").remove(0);
        assert_eq!(
            attempt.venue, None,
            "there is no venue for something never routed"
        );
        assert_eq!(attempt.tokens, None);
        assert_eq!(attempt.failure, Some(ExitFailure::NoRoute));
        assert!(!attempt.blocks_retry(), "and it can be tried again");
    }

    #[test]
    fn the_exit_ledger_refuses_a_row_that_cannot_be_true() {
        let temp = TempDb::new("exit-checks");
        let db = temp.open();

        // A failure with no reason.
        let mut nameless = exit_step("exit-a", 0, "origin-a", None, ExitState::ExitFailed);
        nameless.failure = None;
        assert!(db.record_intent_transitions(&[nameless]).is_err());

        // A reason on a step that did not fail.
        let mut confused = exit_step("exit-b", 0, "origin-b", None, ExitState::ExitConstructed);
        confused.failure = Some(ExitFailure::Signing);
        assert!(db.record_intent_transitions(&[confused]).is_err());

        // Profit with nothing behind it.
        let mut invented = exit_step("exit-c", 0, "origin-c", None, ExitState::ExitConfirmed);
        invented.realized_pnl_lamports = Some(1_000);
        assert!(db.record_intent_transitions(&[invented]).is_err());

        // A position of no tokens.
        let mut empty = exit_step("exit-d", 0, "origin-d", None, ExitState::ExitConstructed);
        empty.tokens = Some(0);
        assert!(db.record_intent_transitions(&[empty]).is_err());
    }

    #[test]
    fn writing_the_same_exit_step_twice_is_a_no_op_but_a_repeated_signature_is_not() {
        let temp = TempDb::new("exit-idempotent");
        let db = temp.open();

        let mut first = exit_step(
            "exit-1",
            1,
            "origin-1",
            Some(ExitState::ExitConstructed),
            ExitState::ExitSigned,
        );
        first.signature = Some("SigOnce".to_string());
        db.record_intent_transitions(&[first.clone()])
            .expect("writes");
        assert_eq!(
            db.record_intent_transitions(&[first]).expect("writes"),
            0,
            "a retry writing a step it already wrote is idempotent"
        );

        // Two exits believing they own one transaction is not idempotent, it is
        // a position sold twice, and it fails here rather than in a later
        // reconciliation.
        let mut stolen = exit_step(
            "exit-2",
            1,
            "origin-2",
            Some(ExitState::ExitConstructed),
            ExitState::ExitSigned,
        );
        stolen.signature = Some("SigOnce".to_string());
        assert!(db.record_intent_transitions(&[stolen]).is_err());
    }

    #[test]
    fn realized_pnl_is_kept_apart_by_mode() {
        let temp = TempDb::new("exit-pnl");
        let db = temp.open();

        let mut live = exit_step(
            "exit-live",
            0,
            "origin-live",
            None,
            ExitState::ExitConfirmed,
        );
        live.out_lamports = Some(300_000_000);
        live.realized_pnl_lamports = Some(50_000_000);
        live.mode = ExecutionMode::Live;

        let mut paper = exit_step(
            "exit-paper",
            0,
            "origin-paper",
            None,
            ExitState::ExitConfirmed,
        );
        paper.out_lamports = Some(1_000_000_000);
        paper.realized_pnl_lamports = Some(750_000_000);
        paper.mode = ExecutionMode::Paper;

        // On the network and not yet landed. Not a realized number.
        let mut flying = exit_step(
            "exit-flying",
            0,
            "origin-flying",
            None,
            ExitState::ExitBroadcast,
        );
        flying.mode = ExecutionMode::Live;

        db.record_intent_transitions(&[live, paper, flying])
            .expect("writes");

        let real = db.realized_pnl(ExecutionMode::Live).expect("reads");
        assert_eq!(real.closed, 1, "the broadcast one has closed nothing");
        assert_eq!(real.realized_lamports, 50_000_000);
        assert_eq!(real.proceeds_lamports, 300_000_000);
        assert_eq!(real.cost_basis_lamports, 250_000_000);

        let on_paper = db.realized_pnl(ExecutionMode::Paper).expect("reads");
        assert_eq!(on_paper.realized_lamports, 750_000_000);
        assert_eq!(
            db.realized_pnl(ExecutionMode::Replay).expect("reads"),
            RealizedPnl::default(),
            "a mode nothing has traded in reads as zero rather than failing"
        );

        let health = db.health().expect("health");
        assert_eq!(health.intent_transitions, 3);
        assert_eq!(health.exits_in_flight, 1, "the broadcast one");
        assert_eq!(health.realized_pnl.live.realized_lamports, 50_000_000);
        assert_eq!(health.realized_pnl.paper.realized_lamports, 750_000_000);
        assert_eq!(
            health.realized_pnl.replay,
            RealizedPnl::default(),
            "one total across three modes would be paper trades reported as real ones"
        );
    }
}
