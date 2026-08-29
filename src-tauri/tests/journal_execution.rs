//! The journal and the alerting engine as the execution path actually drives
//! them.
//!
//! `journal_alerting.rs` covers the book on its own: rows written by hand,
//! survive a restart, come back the same. These are the ones that need the exit
//! path to be the thing writing — because the property being tested is not that
//! `record_journal_fills` works, it is that **nobody has to remember to call
//! it**. A trade journal maintained by hand at the call sites is a trade
//! journal that is complete until somebody adds a sixth call site.
//!
//! Four things are held here:
//!
//! - an unwind writes the whole book, and every number in it agrees with the
//!   receipt the operator was handed;
//! - an exit that never lands leaves the trade open and says so, rather than
//!   closing it at proceeds of zero;
//! - **no column anywhere in `sts.db` has `REAL` affinity, and no value stored
//!   in any of them is a float** — which is the rule `journal.rs` states and
//!   migration 4 finished applying to the four tables that predated it;
//! - none of that changes under a tick storm: many threads, many tables, one
//!   writer, sustained.
//!
//! Everything goes through the public API and then reopens the file with a
//! plain `rusqlite` connection, rather than asking the same code that wrote it.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use sts_lib::alerting::{Alert, AlertDispatcher, AlertKind, AlertSink, AlertThresholds};
use sts_lib::db::{
    ClusterRow, Database, ExecutionLogRow, ExecutionMode, IngestCandidateRow, Side, TickMetricRow,
};
use sts_lib::engine::{Engine, MaintenanceSchedule};
use sts_lib::execution::{ExecutionEngine, MockFault, MockSolanaSigner};
use sts_lib::forensics::{Decision, StateRecord};
use sts_lib::journal::{FillRow, JournalFilter};
use sts_lib::strategy::syndicate::GateVerdict;
use sts_lib::strategy::GateReason;
use sts_lib::types::{
    CircuitBreaker, ExecutionState, FastPathGate, LiquidityThresholds, OperatingMode, RiskSnapshot,
    SybilClusterMetrics,
};

const AT_MS: i64 = 1_700_000_000_000;

/// A file of its own per test, removed when the test ends.
struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sts-journal-exec-{name}-{}-{}.db",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let temp = TempDb(path);
        temp.remove();
        temp
    }

    fn open(&self) -> Database {
        Database::open(&self.0).expect("sts.db opens")
    }

    fn raw(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.0).expect("the file opens")
    }

    fn remove(&self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Timers far enough apart that they never fire during a test.
///
/// Both retention windows are a century rather than the shipped week and month,
/// and that is not laziness. `maintenance_loop` prunes once on the way up,
/// every timestamp in these tests is `AT_MS`, and `AT_MS` is in 2023 — so a
/// week-long window deletes the tick rows before a single assertion runs, and
/// the test that checks every table for floats would pass by finding nothing.
fn quiet() -> MaintenanceSchedule {
    MaintenanceSchedule {
        checkpoint_every: Duration::from_secs(3_600),
        prune_every: Duration::from_secs(3_600),
        retain_ticks_for: Duration::from_secs(100 * 365 * 24 * 60 * 60),
        snapshot_every: Duration::from_secs(3_600),
        retain_state_log_for: Duration::from_secs(100 * 365 * 24 * 60 * 60),
    }
}

/// Everything an alert was delivered to, kept.
#[derive(Debug, Default)]
struct Collector(Mutex<Vec<Alert>>);

impl AlertSink for Collector {
    fn deliver(&self, alert: &Alert) {
        self.0.lock().push(alert.clone());
    }

    fn name(&self) -> &str {
        "collector"
    }
}

/// One intent that reached `confirmed`: a position, on chain, being managed.
fn a_position(db: &Database, intent: &str) {
    use ExecutionState::*;
    let steps = [
        (IntentCreated, None),
        (Validated, None),
        (Sent, Some(format!("Sig{intent}"))),
        (Confirmed, None),
    ];
    let mut prev = None;
    let mut rows = Vec::new();
    for (seq, (state, signature)) in steps.into_iter().enumerate() {
        rows.push(ExecutionLogRow {
            intent_id: intent.to_string(),
            seq: seq as i64,
            mint: format!("Mint{intent}"),
            state,
            prev_state: prev,
            side: Side::Buy,
            size_lamports: 250_000_000,
            price_q18: None,
            signature,
            latency_ms: None,
            needs_unwind: false,
            mode: ExecutionMode::Live,
            abort_reason: None,
            at_ms: AT_MS + seq as i64,
        });
        prev = Some(state);
    }
    db.record_execution_logs(&rows).expect("the history writes");
}

/// An engine with a signer and somewhere to raise alerts, which is the shape
/// `run()` builds at startup.
fn engine_with(
    db: Database,
    signer: &Arc<MockSolanaSigner>,
) -> (Engine, Arc<AlertDispatcher>, Arc<Collector>) {
    let engine = Engine::start_with(db, quiet());
    assert!(engine.install_execution_engine(Arc::clone(signer) as Arc<dyn ExecutionEngine>));

    let alerting = Arc::new(AlertDispatcher::new(engine.telemetry()));
    let collector = Arc::new(Collector::default());
    alerting.attach_sink(Arc::clone(&collector) as Arc<dyn AlertSink>);
    assert!(engine.attach_alerting(Arc::clone(&alerting)));

    (engine, alerting, collector)
}

fn count(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).expect("counts")
}

// ---------------------------------------------------------------------------
// the exit path writes the book
// ---------------------------------------------------------------------------

#[test]
fn an_unwind_writes_the_whole_book_and_the_receipt_agrees_with_it() {
    let temp = TempDb::new("unwind-book");
    let db = temp.open();
    for intent in ["alpha", "beta", "gamma"] {
        a_position(&db, intent);
    }

    let signer = Arc::new(MockSolanaSigner::new());
    let (engine, _alerting, _collector) = engine_with(db, &signer);
    let receipt = engine.emergency_unwind(None, "get out of everything", "operator");
    assert_eq!(receipt.exits_confirmed, 3);
    assert!(receipt.problems.is_empty(), "{:?}", receipt.problems);
    engine.finish_shutdown();

    let conn = temp.raw();

    // One trade per *position*, not per exit transaction. That is the whole
    // reason the book keys on the origin intent: a position that took three
    // attempts to sell is one trade with three signatures, and three trades
    // would make every total underneath it wrong in a way that reads as real.
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM journal_trades"), 3);
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM journal_trades WHERE closed_at_ms IS NOT NULL"
        ),
        3,
        "every position was flattened, so every trade is closed"
    );
    let ids: Vec<String> = conn
        .prepare("SELECT trade_id FROM journal_trades ORDER BY trade_id")
        .expect("prepares")
        .query_map([], |row| row.get(0))
        .expect("runs")
        .collect::<Result<_, _>>()
        .expect("reads");
    assert_eq!(
        ids,
        vec!["alpha", "beta", "gamma"],
        "keyed by the position, not by the exit"
    );

    // Every child table filled in, and each one keyed back to a trade that
    // exists — which the foreign keys enforce, but which is worth counting
    // because a silently skipped write would leave a legal, empty table.
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM journal_fills"), 3);
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM journal_routes WHERE chosen = 1"
        ),
        3
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM journal_signatures WHERE status = 'confirmed'"
        ),
        3
    );
    // The mock tips on every exit, so every trade has exactly one bid.
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM journal_tips"), 3);

    // Nothing is left looking as though it is still on the network.
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM journal_signatures WHERE status = 'broadcast'"
        ),
        0
    );

    // The number on the receipt is the number in the book. This is the join
    // `journal.rs` exists to remove: the same fact, arrived at from the exit
    // ledger and from the trade journal, has to come out the same.
    let booked: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(realized_pnl_lamports), 0) FROM journal_trades WHERE mode = 'live'",
            [],
            |row| row.get(0),
        )
        .expect("sums");
    let ledgered: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(realized_pnl_lamports), 0) FROM intent_transitions
              WHERE to_state = 'exit_confirmed' AND mode = 'live'",
            [],
            |row| row.get(0),
        )
        .expect("sums");
    assert_eq!(
        ledgered, receipt.realized_pnl_lamports,
        "the ledger and the receipt"
    );
    // The book subtracts the fee and the tip; the exit ledger's column does
    // not. So the book is the more pessimistic of the two, always, and by
    // exactly what was paid to get out.
    let costs: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(fee_lamports + tip_lamports), 0) FROM journal_trades",
            [],
            |row| row.get(0),
        )
        .expect("sums");
    assert!(costs > 0, "the mock tips, so getting out cost something");
    assert_eq!(
        booked,
        ledgered - costs,
        "the book is the ledger less what was paid to land the exit"
    );

    // And the derived columns are derived, not asserted: a fill's price is the
    // lamports and the tokens beside it, divided.
    let (tokens, lamports, price): (i64, i64, i64) = conn
        .query_row(
            "SELECT tokens, lamports, price_q18 FROM journal_fills LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("reads a fill");
    let expected = FillRow::settle("x", 0, tokens as u64, lamports as u64, 0, 1, 0, 0)
        .expect("a fill")
        .price
        .to_i64_raw()
        .expect("fits");
    assert_eq!(
        price, expected,
        "the stored price is not the one the pair implies"
    );
}

#[test]
fn an_exit_that_never_lands_leaves_the_trade_open_and_says_why() {
    let temp = TempDb::new("unwind-stranded");
    let db = temp.open();
    a_position(&db, "alpha");

    let signer = Arc::new(MockSolanaSigner::new());
    // Reaches the network, and the blockhash goes past its window with nothing
    // on chain.
    signer.inject("alpha", MockFault::NotConfirmed);
    let (engine, _alerting, collector) = engine_with(db, &signer);
    let receipt = engine.emergency_unwind(None, "get out", "operator");
    assert_eq!(receipt.exits_confirmed, 0);
    engine.finish_shutdown();

    let conn = temp.raw();

    // The trade is open. Nothing came back, so there are no proceeds — and a
    // `closed_at_ms` with proceeds of zero would say the sale returned nothing,
    // which is a different and much worse fact than the sale not happening.
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM journal_trades"), 1);
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM journal_trades WHERE closed_at_ms IS NULL"
        ),
        1
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM journal_trades WHERE proceeds_lamports IS NULL"
        ),
        1
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM journal_fills"),
        0,
        "nothing filled"
    );

    // The route was priced and taken and did not work, so it is recorded as a
    // path that lost — which leaves `chosen` free for the attempt that
    // eventually works, and is what makes "which liquidity did the money go
    // through" answerable later.
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM journal_routes WHERE chosen = 0"
        ),
        1
    );
    let because: String = conn
        .query_row("SELECT rejected_because FROM journal_routes", [], |row| {
            row.get(0)
        })
        .expect("reads the reason");
    assert!(because.starts_with("not_confirmed: "), "{because}");
    assert!(
        because.contains("blockhash is past its window"),
        "{because}"
    );

    // The blockhash aged out, which is `expired` and not `dropped`. Keeping the
    // two apart is the only way to say afterwards whether the network was slow
    // or the send was late.
    let status: String = conn
        .query_row("SELECT status FROM journal_signatures", [], |row| {
            row.get(0)
        })
        .expect("reads the status");
    assert_eq!(status, "expired");

    // And somebody was told, without anybody at the exit call site having asked
    // for an alert.
    let raised = collector.0.lock();
    assert!(
        raised
            .iter()
            .any(|alert| alert.kind == AlertKind::ExitFailed),
        "an exit that never landed raised nothing: {raised:?}"
    );
}

#[test]
fn a_second_attempt_adds_to_the_trade_rather_than_starting_another_one() {
    // The property the origin-keyed trade id exists for — and the one that
    // would break loudly if `opened_at_ms` were read off the clock.
    // `journal_trades` refuses an update that changes when a trade opened, so a
    // second pass stamping its own `now` would abort the write instead of
    // updating the row, and the book would keep the first attempt's answer
    // forever.
    //
    // `Broadcast` and not `NotConfirmed` as the first fault, because those are
    // not the same situation: an exit that reached the network is ambiguous
    // until somebody reconciles it and `ExitAttempt::blocks_retry` refuses to
    // build a second one. A send that never left is the only failure a retry is
    // allowed to follow, which is what makes this the reachable two-attempt
    // case rather than a hypothetical one.
    let temp = TempDb::new("second-attempt");
    let db = temp.open();
    a_position(&db, "alpha");

    let signer = Arc::new(MockSolanaSigner::new());
    signer.inject("alpha", MockFault::Broadcast);
    let (engine, _alerting, _collector) = engine_with(db, &signer);
    let first = engine.emergency_unwind(None, "first go", "operator");
    assert_eq!(first.exits_confirmed, 0);

    // Nothing reached the network, so a second exit may be built. `Dropped(0)`
    // is the mock's way of spelling "no fault": no confirmation comes back
    // empty, and the first one lands.
    signer.inject("alpha", MockFault::Dropped(0));
    let receipt = engine.emergency_unwind(None, "second go", "operator");
    assert_eq!(receipt.exits_confirmed, 1, "{:?}", receipt.problems);
    engine.finish_shutdown();

    let conn = temp.raw();
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM journal_trades"),
        1,
        "one position, one trade"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM journal_signatures"),
        2,
        "two attempts, two signatures, both under the one trade"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM journal_signatures WHERE status = 'failed'"
        ),
        1,
        "the send that never left is failed, not dropped — nothing was on the network"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM journal_routes"),
        2,
        "each attempt priced its own route"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM journal_routes WHERE chosen = 1"
        ),
        1,
        "the money went one way, and it is the attempt that filled"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM journal_trades WHERE closed_at_ms IS NOT NULL"
        ),
        1,
        "the second attempt closed the trade the first one opened"
    );

    // And the trade still says it opened when the *position* opened, which is
    // the write that the table's trigger would have refused if the second pass
    // had used its own clock.
    let opened: i64 = conn
        .query_row("SELECT opened_at_ms FROM journal_trades", [], |row| {
            row.get(0)
        })
        .expect("reads");
    assert_eq!(
        opened, AT_MS,
        "the first row of the position, which is the one that holds still"
    );
}

#[test]
fn a_rebroadcast_moves_the_signature_it_already_wrote_rather_than_adding_one() {
    // The same bytes going out again is the same transaction, so it is the same
    // row. A rebroadcast that added a signature would make `journal_in_flight`
    // count one exit as several, and would make the count of exits that never
    // landed depend on how bad the network was that minute.
    let temp = TempDb::new("rebroadcast");
    let db = temp.open();
    a_position(&db, "alpha");

    let signer = Arc::new(MockSolanaSigner::new());
    // Two confirmations come back with no answer, and the third lands.
    signer.inject("alpha", MockFault::Dropped(2));
    let (engine, _alerting, _collector) = engine_with(db, &signer);
    let receipt = engine.emergency_unwind(None, "get out", "operator");
    assert_eq!(receipt.exits_confirmed, 1, "{:?}", receipt.problems);
    engine.finish_shutdown();

    let conn = temp.raw();
    let (count_of, status, rebroadcasts): (i64, String, i64) = conn
        .query_row(
            "SELECT COUNT(*), MIN(status), MIN(rebroadcasts) FROM journal_signatures",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("reads");
    assert_eq!(
        count_of, 1,
        "one transaction, sent three times, is one signature"
    );
    assert_eq!(status, "confirmed");
    assert_eq!(
        rebroadcasts, 2,
        "and the row says how many times it went out again"
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM journal_fills"), 1);
}

#[test]
fn an_attempt_that_never_got_signed_still_records_the_path_it_priced() {
    // The gap between "routed" and "sent". An exit that fails at the signer has
    // a venue, a size and a floor — everything the route row holds — and no
    // signature, because nothing was signed. The book should say the path was
    // priced and lost, not say nothing at all.
    let temp = TempDb::new("unsigned");
    let db = temp.open();
    a_position(&db, "alpha");

    let signer = Arc::new(MockSolanaSigner::new());
    signer.inject("alpha", MockFault::Signing);
    let (engine, _alerting, _collector) = engine_with(db, &signer);
    let receipt = engine.emergency_unwind(None, "get out", "operator");
    assert_eq!(receipt.exits_confirmed, 0);
    engine.finish_shutdown();

    let conn = temp.raw();
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM journal_trades"),
        1,
        "the trade was opened"
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM journal_routes WHERE chosen = 0"
        ),
        1
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM journal_signatures"),
        0,
        "nothing was signed, so there is no signature to record"
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM journal_fills"), 0);
    let because: String = conn
        .query_row("SELECT rejected_because FROM journal_routes", [], |row| {
            row.get(0)
        })
        .expect("reads the reason");
    assert!(because.starts_with("signing: "), "{because}");
}

#[test]
fn the_journal_is_written_whether_or_not_anybody_is_listening_for_alerts() {
    // The asymmetry `Flattener::alerting_through` documents. An alert is a
    // message to a person and needs somewhere to go; the book is the record of
    // what was traded and is not optional.
    let temp = TempDb::new("no-dispatcher");
    let db = temp.open();
    a_position(&db, "alpha");

    let signer = Arc::new(MockSolanaSigner::new());
    let engine = Engine::start_with(db, quiet());
    assert!(engine.install_execution_engine(Arc::clone(&signer) as Arc<dyn ExecutionEngine>));
    assert!(engine.alerting().is_none(), "nothing attached");

    let receipt = engine.emergency_unwind(None, "get out", "operator");
    assert_eq!(receipt.exits_confirmed, 1);
    engine.finish_shutdown();

    let conn = temp.raw();
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM journal_trades"), 1);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM journal_fills"), 1);
}

// ---------------------------------------------------------------------------
// no floats, anywhere
// ---------------------------------------------------------------------------

/// SQLite's column affinity rules, in the order the documentation applies them.
///
/// The declared type of a column is free text; what SQLite does with it is
/// decided by these five substring tests, and only the fourth produces a column
/// that will store a float. Spelling the rules out rather than grepping for
/// `REAL` is the difference between a test that catches `DOUBLE PRECISION` and
/// one that does not.
fn affinity(declared: &str) -> &'static str {
    let declared = declared.to_ascii_uppercase();
    if declared.contains("INT") {
        "INTEGER"
    } else if declared.contains("CHAR") || declared.contains("CLOB") || declared.contains("TEXT") {
        "TEXT"
    } else if declared.contains("BLOB") || declared.is_empty() {
        "BLOB"
    } else if declared.contains("REAL") || declared.contains("FLOA") || declared.contains("DOUB") {
        "REAL"
    } else {
        "NUMERIC"
    }
}

/// Every table in the file, in name order.
fn tables(conn: &rusqlite::Connection) -> Vec<String> {
    conn.prepare(
        "SELECT name FROM sqlite_master
          WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
          ORDER BY name",
    )
    .expect("prepares")
    .query_map([], |row| row.get(0))
    .expect("runs")
    .collect::<Result<_, _>>()
    .expect("reads")
}

/// Every column of one table, generated and hidden ones included.
///
/// `table_xinfo` and not `table_info`. The latter omits generated columns
/// entirely, so a check built on it would answer "there is no such column"
/// about a `REAL GENERATED ALWAYS AS ...` that is sitting right there — a test
/// that passes for the wrong reason and would have gone on passing if migration
/// 4 had never run.
fn columns(conn: &rusqlite::Connection, table: &str) -> Vec<(String, String)> {
    conn.prepare(&format!("PRAGMA table_xinfo('{table}')"))
        .expect("prepares")
        .query_map([], |row| Ok((row.get(1)?, row.get(2)?)))
        .expect("runs")
        .collect::<Result<_, _>>()
        .expect("reads")
}

/// Fills every table in the schema with at least one row.
///
/// So that the value half of the check below has something to look at. A schema
/// scan alone would pass on an empty file, and "no column is declared `REAL`"
/// and "no value stored anywhere is a float" are two different claims — SQLite
/// is dynamically typed, and a float bound into a `NUMERIC` or `BLOB` column
/// stays a float.
fn fill_every_table(temp: &TempDb) {
    let db = temp.open();
    a_position(&db, "alpha");

    db.record_ingest_candidates(&[IngestCandidateRow {
        source: "helius".to_string(),
        slot: 250_000_001,
        account: "Curve1111111111111111111111111111111111111".to_string(),
        program: "Prog11111111111111111111111111111111111111".to_string(),
        creator: Some("Creator11111111111111111111111111111111111".to_string()),
        route: "fast_path".to_string(),
        market_cap_usd_cents: 1_234_500,
        pool_lamports: 42_000_000_000,
        curve_progress_bps: 7_370,
        observed_at_ms: AT_MS,
        dispatch_latency_us: 412,
    }])
    .expect("writes a candidate");

    db.record_clusters(&[ClusterRow {
        cluster_id: "cluster-a".to_string(),
        version: 1,
        root_wallet: "Root1111111111111111111111111111111111111111".to_string(),
        metrics: SybilClusterMetrics::new(9, 6_412, 910_000, 770_000, 120_000),
        flag_sybil: true,
        computed_at_ms: AT_MS,
    }])
    .expect("writes a cluster");

    db.record_tick_metrics(&[TickMetricRow {
        rpc_endpoint: "helius".to_string(),
        timestamp_ms: AT_MS,
        latency_ms: 40,
        dropped_msgs: 0,
        parsed_per_sec_micros: 812_500_000,
    }])
    .expect("writes a tick");

    db.record_audit(
        "older_build",
        &serde_json::json!({"note": "something"}),
        AT_MS,
    )
    .expect("writes an audit row");

    // Migration 5's three. The counter moves as a side effect of the log write,
    // and the checkpoint is taken last so it has both the book and the log to
    // describe — a snapshot of nothing would leave half its columns at zero and
    // the value scan below with nothing to disagree with.
    let risk = RiskSnapshot {
        at_ms: AT_MS,
        mode: OperatingMode::Paper,
        equity_lamports: 200_000_000,
        high_water_lamports: 250_000_000,
        drawdown_bps: 2_000,
        max_drawdown_bps: 3_000,
        open_positions: 1,
        max_open_positions: 3,
        circuit_breaker: CircuitBreaker::Clear,
        fast_path: FastPathGate::CLOSED,
        liquidity: LiquidityThresholds {
            min_pool_lamports: 10_000_000,
            exit_only_below_lamports: 5_000_000,
            max_pool_share_bps: 150,
        },
    };
    let verdict = GateVerdict {
        enter: false,
        reason: GateReason::SmallBundle,
        confidence_micros: 612_500,
        tags: Vec::new(),
        thin: false,
        bundle_wallets: 5,
        bundle_lamports: 3_000_000_000,
        cohort_wallets: 7,
        cohort_lamports: 4_250_000_000,
        cohort_size_lamports: None,
        cohort_delta_bps: None,
        cohort_external: 1,
        rings: Vec::new(),
        sandwich: None,
    };
    db.record_state_log(
        ExecutionMode::Paper,
        &[StateRecord::decided(
            "Mint1111111111111111111111111111111111111111",
            &verdict,
            &risk,
            Decision::Refused,
            None,
            11,
            42_000_000_000,
            true,
            AT_MS - 100,
            AT_MS,
        )],
        AT_MS,
    )
    .expect("writes a forensic row");

    db.take_journal_snapshot(ExecutionMode::Paper, AT_MS)
        .expect("writes a checkpoint");

    // `intent_transitions` and the five journal tables, from the exit path
    // rather than by hand — which is also what puts a fill, a route, a tip and
    // a signature in the file.
    let signer = Arc::new(MockSolanaSigner::new());
    let (engine, _alerting, _collector) = engine_with(db, &signer);
    let receipt = engine.emergency_unwind(None, "fill the file", "operator");
    assert_eq!(receipt.exits_confirmed, 1);
    engine.finish_shutdown();
}

#[test]
fn no_column_in_the_schema_is_a_real_and_no_value_in_the_file_is_a_float() {
    let temp = TempDb::new("no-reals");
    fill_every_table(&temp);
    let conn = temp.raw();

    let tables = tables(&conn);
    // The whole schema, so that a table added later is covered by this without
    // anybody remembering to add it here.
    let expected: BTreeSet<&str> = [
        "audit_log",
        "candidates",
        "clusters",
        "execution_logs",
        "intent_transitions",
        "journal_fills",
        "journal_revisions",
        "journal_routes",
        "journal_signatures",
        "journal_snapshots",
        "journal_state_log",
        "journal_tips",
        "journal_trades",
        "schema_migrations",
        "tick_metrics",
    ]
    .into_iter()
    .collect();
    let found: BTreeSet<&str> = tables.iter().map(String::as_str).collect();
    assert_eq!(
        found, expected,
        "the schema is not the shape this test is scanning"
    );

    // -- the declarations ---------------------------------------------------
    let mut declared_real = Vec::new();
    let mut scanned = 0usize;
    for table in &tables {
        for (column, declared) in columns(&conn, table) {
            scanned += 1;
            if affinity(&declared) == "REAL" {
                declared_real.push(format!("{table}.{column} is {declared}"));
            }
        }
    }
    // 118 columns across 12 tables at the time of writing. The floor is a
    // guard against the scan silently covering nothing — `table_xinfo` on a
    // name that does not exist returns no rows rather than an error.
    assert!(
        scanned > 60,
        "only {scanned} columns scanned, which is not the whole schema"
    );
    assert!(
        declared_real.is_empty(),
        "every column in sts.db is meant to be an integer, a text or a blob, and these are not: {}",
        declared_real.join(", ")
    );

    // -- and the values -----------------------------------------------------
    //
    // The schema half is not enough on its own. SQLite stores what it is given:
    // a float bound into a `NUMERIC` column stays a float, and `typeof` is what
    // says so.
    let mut stored_real = Vec::new();
    for table in &tables {
        let rows = count(&conn, &format!("SELECT COUNT(*) FROM \"{table}\""));
        assert!(
            rows > 0,
            "{table} is empty, so this test says nothing about it"
        );
        for (column, _) in columns(&conn, table) {
            let floats = count(
                &conn,
                &format!("SELECT COUNT(*) FROM \"{table}\" WHERE typeof(\"{column}\") = 'real'"),
            );
            if floats > 0 {
                stored_real.push(format!("{table}.{column} holds {floats}"));
            }
        }
    }
    assert!(
        stored_real.is_empty(),
        "these columns are holding floats: {}",
        stored_real.join(", ")
    );
}

// ---------------------------------------------------------------------------
// under load
// ---------------------------------------------------------------------------

#[test]
fn a_tick_storm_loses_no_row_and_stores_no_float() {
    // A feed at full rate and an exit path working underneath it, through one
    // `Database` — which is one connection behind one mutex, because SQLite in
    // WAL mode allows exactly one writer. The point is not throughput. It is
    // that the three things that are shared — the writer, the dispatcher's
    // cooldown map, and the file's own constraints — are all still correct when
    // eight threads are pushing on them at once, and that nothing takes a
    // shortcut into a float under contention.
    const INGEST_THREADS: u32 = 4;
    const BOOK_THREADS: u32 = 4;
    const PER_THREAD: u32 = 250;

    let temp = TempDb::new("tick-storm");
    let db = Arc::new(temp.open());
    let hub = Arc::new(sts_lib::telemetry::TelemetryHub::start());
    let dispatcher = Arc::new(AlertDispatcher::new(Arc::clone(&hub)));
    // No cooldown, so the expected alert count is a number this test can state
    // rather than one it has to observe.
    dispatcher
        .set_thresholds(AlertThresholds {
            cooldown_ms: 0,
            ..AlertThresholds::default()
        })
        .expect("valid");
    let collector = Arc::new(Collector::default());
    dispatcher.attach_sink(Arc::clone(&collector) as Arc<dyn AlertSink>);

    // One trade per book thread, opened up front so the fills below have a
    // parent to hang off.
    let trades: Vec<sts_lib::journal::TradeRow> = (0..BOOK_THREADS)
        .map(|t| {
            sts_lib::journal::TradeRow::opened(
                format!("storm-{t}"),
                "So11111111111111111111111111111111111111112",
                Side::Sell,
                ExecutionMode::Paper,
                250_000_000,
                AT_MS,
            )
        })
        .collect();
    db.record_journal_trades(&trades).expect("writes");

    let mut handles = Vec::new();

    // The feed. Candidates and tick metrics, which is what a storm actually
    // looks like: two tables, high rate, nothing interesting in any one row.
    for thread in 0..INGEST_THREADS {
        let db = Arc::clone(&db);
        handles.push(std::thread::spawn(move || {
            for tick in 0..PER_THREAD {
                let slot = i64::from(thread) * 1_000_000 + i64::from(tick) + 1;
                db.record_ingest_candidates(&[IngestCandidateRow {
                    source: format!("feed-{thread}"),
                    slot,
                    account: format!("Curve{thread}-{tick}"),
                    program: "Prog11111111111111111111111111111111111111".to_string(),
                    creator: None,
                    route: if tick % 2 == 0 {
                        "fast_path"
                    } else {
                        "standard"
                    }
                    .to_string(),
                    market_cap_usd_cents: i64::from(tick) * 100,
                    pool_lamports: 42_000_000_000 + i64::from(tick),
                    curve_progress_bps: i64::from(tick % 10_001),
                    observed_at_ms: AT_MS + i64::from(tick),
                    dispatch_latency_us: i64::from(tick % 900),
                }])
                .expect("a candidate writes");
                db.record_tick_metrics(&[TickMetricRow {
                    rpc_endpoint: format!("feed-{thread}"),
                    timestamp_ms: AT_MS + i64::from(tick),
                    latency_ms: i64::from(tick % 120),
                    dropped_msgs: i64::from(tick % 3),
                    // A rate that is genuinely fractional, so the storage unit
                    // is doing work rather than happening to be whole.
                    parsed_per_sec_micros: 812_500_000 + u64::from(tick),
                }])
                .expect("a tick writes");
            }
            0usize
        }));
    }

    // And the book, underneath it.
    for thread in 0..BOOK_THREADS {
        let db = Arc::clone(&db);
        let dispatcher = Arc::clone(&dispatcher);
        handles.push(std::thread::spawn(move || {
            let id = format!("storm-{thread}");
            let mut fired = 0usize;
            for seq in 0..PER_THREAD {
                // Every fifth fill comes in badly, so the expected alert count
                // is arithmetic rather than observation.
                let bad = seq % 5 == 0;
                let filled = if bad { 800_000 } else { 995_000 };
                let at = AT_MS + i64::from(seq);
                let fill = FillRow::settle(
                    &id,
                    seq,
                    1_000_000,
                    filled,
                    0,
                    1_000_000,
                    u64::from(seq),
                    at,
                )
                .expect("a fill");
                db.record_journal_fills(std::slice::from_ref(&fill))
                    .expect("a fill writes");
                fired += dispatcher
                    .observe(
                        &sts_lib::alerting::Observation::Filled {
                            trade_id: &id,
                            mint: "So11111111111111111111111111111111111111112",
                            mode: ExecutionMode::Paper,
                            fill: &fill,
                            route_bound_bps: 300,
                        },
                        at,
                    )
                    .len();
            }
            fired
        }));
    }

    let fired: usize = handles
        .into_iter()
        .map(|h| h.join().expect("no panic"))
        .sum();
    let expected = (0..PER_THREAD).filter(|seq| seq % 5 == 0).count() * BOOK_THREADS as usize;
    assert_eq!(fired, expected, "an alert was lost or invented under load");
    assert_eq!(
        collector.0.lock().len(),
        expected,
        "the sink and the caller disagree"
    );
    assert_eq!(dispatcher.snapshot().raised as usize, expected);
    assert_eq!(dispatcher.snapshot().sink_panics, 0);

    // Every sequence number the dispatcher handed out is distinct, which is
    // what makes a gap in them mean a dropped delivery rather than a race.
    let seqs: BTreeSet<u64> = collector.0.lock().iter().map(|alert| alert.seq).collect();
    assert_eq!(seqs.len(), expected, "two alerts share a sequence number");

    // The book still reads as a book.
    let totals = db
        .journal_totals(&JournalFilter::in_mode(ExecutionMode::Paper))
        .expect("totals");
    assert_eq!(totals.trades, i64::from(BOOK_THREADS));

    dispatcher.shutdown();
    hub.shutdown();
    db.close();

    // And nothing got lost or turned into a float on the way through.
    let conn = temp.raw();
    let ingested = i64::from(INGEST_THREADS * PER_THREAD);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM candidates"),
        ingested,
        "a candidate was lost"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM tick_metrics"),
        ingested,
        "a tick was lost"
    );
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM journal_fills"),
        i64::from(BOOK_THREADS * PER_THREAD),
        "a fill was lost under contention"
    );

    let mut stored_real = Vec::new();
    for table in tables(&conn) {
        for (column, _) in columns(&conn, &table) {
            let floats = count(
                &conn,
                &format!("SELECT COUNT(*) FROM \"{table}\" WHERE typeof(\"{column}\") = 'real'"),
            );
            if floats > 0 {
                stored_real.push(format!("{table}.{column} holds {floats}"));
            }
        }
    }
    assert!(
        stored_real.is_empty(),
        "a float reached the file: {}",
        stored_real.join(", ")
    );
}
