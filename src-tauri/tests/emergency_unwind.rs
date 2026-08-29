//! The unwind path, end to end, against a real `sts.db`.
//!
//! `engine.rs` has the unit tests for one engine's behaviour. These are the
//! ones that need two: the properties that only mean anything across a restart,
//! because they are what the ledger is for. A signature written before a
//! broadcast is worth nothing if the next process cannot read it, and "do not
//! sell the same position twice" is easy inside one `Engine` and is exactly the
//! thing a crash mid-exit breaks.
//!
//! Everything here goes through the public API — `Database::open`,
//! `Engine::start_with`, `emergency_unwind` — and then reopens the file with a
//! plain `rusqlite` connection to check what is actually on disk, rather than
//! asking the same code that wrote it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sts_lib::db::ExecutionLogRow;
use sts_lib::db::{Database, ExecutionMode, Side};
use sts_lib::engine::{Engine, MaintenanceSchedule, UnwindReceipt};
use sts_lib::execution::{ExecutionEngine, MockFault, MockSolanaSigner};
use sts_lib::metrics::MetricsCollector;
use sts_lib::types::ExecutionState;

/// A file of its own per test, removed when the test ends.
struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sts-unwind-{name}-{}-{}.db",
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

    fn path(&self) -> &Path {
        &self.0
    }

    fn read(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.0).expect("reopens")
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

/// Timers far enough apart that they never fire during a test. These are about
/// the unwind, not about maintenance.
fn quiet() -> MaintenanceSchedule {
    MaintenanceSchedule {
        checkpoint_every: Duration::from_secs(3_600),
        prune_every: Duration::from_secs(3_600),
        retain_ticks_for: Duration::from_secs(7 * 24 * 60 * 60),
        snapshot_every: Duration::from_secs(3_600),
        retain_state_log_for: Duration::from_secs(100 * 365 * 24 * 60 * 60),
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
            at_ms: 1_700_000_000_000 + seq as i64,
        });
        prev = Some(state);
    }
    db.record_execution_logs(&rows).expect("the history writes");
}

fn engine_with_signer(db: Database, signer: &Arc<MockSolanaSigner>) -> Engine {
    let engine = Engine::start_with(db, quiet());
    assert!(engine.install_execution_engine(Arc::clone(signer) as Arc<dyn ExecutionEngine>));
    engine
}

fn count(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).expect("counts")
}

#[test]
fn a_clean_unwind_flattens_every_position_and_the_file_agrees_with_the_receipt() {
    let temp = TempDb::new("clean");
    let db = temp.open();
    for intent in ["alpha", "beta", "gamma"] {
        a_position(&db, intent);
    }

    let signer = Arc::new(MockSolanaSigner::new());
    let engine = engine_with_signer(db, &signer);
    let receipt = engine.emergency_unwind(None, "get out of everything", "operator");

    assert_eq!(receipt.exits_sent, 3);
    assert_eq!(receipt.exits_confirmed, 3);
    assert_eq!(receipt.flattened.len(), 3);
    assert!(receipt.stranded.is_empty(), "{:?}", receipt.stranded);
    assert!(receipt.problems.is_empty(), "{:?}", receipt.problems);
    assert!(
        receipt.realized_pnl_lamports < 0,
        "a round trip costs fees and impact"
    );
    engine.finish_shutdown();

    // And now against the file itself, opened fresh.
    let conn = temp.read();
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM intent_transitions WHERE to_state = 'exit_confirmed'"
        ),
        3
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(DISTINCT signature) FROM intent_transitions WHERE signature IS NOT NULL"
        ),
        3,
        "three positions, three distinct exit signatures"
    );
    let booked: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(realized_pnl_lamports), 0) FROM intent_transitions
              WHERE to_state = 'exit_confirmed' AND mode = 'live'",
            [],
            |row| row.get(0),
        )
        .expect("sums");
    assert_eq!(
        booked, receipt.realized_pnl_lamports,
        "the number on the receipt is the number in the file"
    );

    // Every exit walked the ordinary state machine as a sell, and every
    // original obligation was left exactly as it was.
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM execution_logs WHERE side = 'sell' AND state = 'completed'"
        ),
        3
    );
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM execution_logs WHERE intent_id = 'alpha'"
        ),
        5,
        "four steps and the abort — the row is history and nothing edited it"
    );
}

#[test]
fn a_partial_failure_leaves_the_rest_flattened_and_the_failure_written_down() {
    let temp = TempDb::new("partial");
    let db = temp.open();
    for intent in ["sellable", "stuck"] {
        a_position(&db, intent);
    }

    let signer = Arc::new(MockSolanaSigner::new());
    signer.inject("stuck", MockFault::NoRoute);
    let engine = engine_with_signer(db, &signer);
    let receipt = engine.emergency_unwind(None, "one dead pool", "operator");

    assert_eq!(receipt.exits_sent, 1);
    assert_eq!(receipt.flattened.len(), 1);
    assert_eq!(receipt.stranded.len(), 1);
    assert_eq!(receipt.stranded[0].intent_id, "stuck");
    engine.finish_shutdown();

    let conn = temp.read();
    let (to_state, failure, detail, venue): (String, String, String, Option<String>) = conn
        .query_row(
            "SELECT to_state, failure, detail, venue FROM intent_transitions
              WHERE origin_intent_id = 'stuck'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("the failure was recorded");
    assert_eq!(to_state, "exit_failed");
    assert_eq!(failure, "no_route");
    assert!(
        detail.contains("depleted"),
        "the sentence survives too: {detail}"
    );
    assert_eq!(
        venue, None,
        "there is no venue for something that was never routed"
    );
}

#[test]
fn an_unwind_with_no_positions_writes_nothing_to_the_exit_ledger() {
    let temp = TempDb::new("empty");
    let signer = Arc::new(MockSolanaSigner::new());
    let engine = engine_with_signer(temp.open(), &signer);

    let receipt = engine.emergency_unwind(None, "belt and braces", "operator");
    assert_eq!(receipt.aborted, 0);
    assert_eq!(receipt.exits_sent, 0);
    assert_eq!(receipt.realized_pnl_lamports, 0);
    assert!(receipt.stranded.is_empty());
    assert!(receipt.stranded_known);
    assert!(receipt.problems.is_empty(), "{:?}", receipt.problems);
    engine.finish_shutdown();

    let conn = temp.read();
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM intent_transitions"), 0);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM execution_logs"), 0);
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM audit_log WHERE event_type = 'emergency_unwind'"
        ),
        1,
        "the unwind itself is still recorded — pressing it is a fact even when it found nothing"
    );
}

#[test]
fn a_second_process_does_not_sell_a_position_the_first_one_already_sold() {
    let temp = TempDb::new("across-restarts");
    {
        let db = temp.open();
        a_position(&db, "alpha");
        let signer = Arc::new(MockSolanaSigner::new());
        let engine = engine_with_signer(db, &signer);
        let first = engine.emergency_unwind(None, "once", "operator");
        assert_eq!(first.exits_sent, 1);
        engine.finish_shutdown();
    }

    // A new process, a new engine, a new signer with no memory of the first —
    // everything it knows comes off the file.
    let signer = Arc::new(MockSolanaSigner::new());
    let engine = engine_with_signer(temp.open(), &signer);
    let second = engine.emergency_unwind(None, "twice", "operator");

    assert_eq!(
        second.exits_sent, 0,
        "the exit ledger is what stops the second process re-selling what the first sold"
    );
    assert_eq!(second.exits_already_out, 1);
    assert_eq!(
        second.exits_confirmed, 1,
        "and the position is still closed"
    );
    assert!(second.stranded.is_empty());
    assert_eq!(
        signer.counters(),
        (0, 0, 0, 0),
        "this signer was never asked to sign anything"
    );
    engine.finish_shutdown();

    let conn = temp.read();
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(DISTINCT intent_id) FROM intent_transitions"
        ),
        1,
        "one exit for one position, across two processes"
    );
}

#[test]
fn a_signature_is_on_disk_before_the_broadcast_so_the_next_process_knows_about_it() {
    let temp = TempDb::new("durable-signature");
    let signature = {
        let db = temp.open();
        a_position(&db, "flying");
        let signer = Arc::new(MockSolanaSigner::new());
        // It reaches the network and never comes back — the ambiguous case.
        signer.inject("flying", MockFault::NotConfirmed);
        let engine = engine_with_signer(db, &signer);
        let receipt = engine.emergency_unwind(None, "sell it", "operator");
        assert_eq!(receipt.exits_sent, 1);
        assert_eq!(receipt.exits_confirmed, 0);
        engine.finish_shutdown();

        let exit = receipt.stranded[0].exit.clone().expect("an exit was tried");
        assert!(exit.on_network);
        exit.exit_intent_id.expect("it got as far as having an id")
    };

    let conn = temp.read();
    let stored: String = conn
        .query_row(
            "SELECT signature FROM intent_transitions
              WHERE intent_id = ?1 AND signature IS NOT NULL",
            [&signature],
            |row| row.get(0),
        )
        .expect("the signature was written before the broadcast, so it is here");
    assert!(!stored.is_empty());

    // A second process sees the ambiguity and refuses to sell again.
    let signer = Arc::new(MockSolanaSigner::new());
    let engine = engine_with_signer(temp.open(), &signer);
    let second = engine.emergency_unwind(None, "again", "operator");
    assert_eq!(signer.counters().0, 0, "nothing new was signed");
    let flying = second
        .stranded
        .iter()
        .find(|p| p.intent_id == "flying")
        .expect("the position is still out there");
    let exit = flying
        .exit
        .as_ref()
        .expect("with an answer about what was tried");
    assert!(
        exit.on_network,
        "a broadcast that never confirmed stays ambiguous until somebody follows the signature"
    );
    engine.finish_shutdown();
}

#[test]
fn the_receipt_reaches_the_window_in_the_shape_the_window_reads() {
    let temp = TempDb::new("receipt-shape");
    let db = temp.open();
    a_position(&db, "alpha");
    a_position(&db, "stuck");

    let signer = Arc::new(MockSolanaSigner::new());
    signer.inject("stuck", MockFault::NoRoute);
    let engine = engine_with_signer(db, &signer);
    let receipt: UnwindReceipt = engine.emergency_unwind(None, "show me", "ui");
    let json = serde_json::to_value(&receipt).expect("the receipt serialises");

    // The four fields `ui/app.js` actually branches on.
    assert_eq!(json["exitsSent"], serde_json::json!(1));
    assert_eq!(json["strandedKnown"], serde_json::json!(true));
    assert!(json["stranded"].is_array());
    assert!(json["problems"].is_array());

    // The stranded row carries what somebody flattening it by hand needs.
    let stranded = &json["stranded"][0];
    assert_eq!(stranded["intentId"], serde_json::json!("stuck"));
    assert_eq!(stranded["atRiskIn"], serde_json::json!("confirmed"));
    assert_eq!(stranded["conditional"], serde_json::json!(false));
    assert_eq!(stranded["exit"]["failure"], serde_json::json!("noRoute"));
    assert_eq!(stranded["exit"]["onNetwork"], serde_json::json!(false));

    // And the new half: what was closed, and what it came to.
    assert_eq!(json["exitsConfirmed"], serde_json::json!(1));
    assert_eq!(json["signer"], serde_json::json!("mock-solana-signer"));
    assert_eq!(json["signerLive"], serde_json::json!(false));
    assert_eq!(json["flattened"][0]["intentId"], serde_json::json!("alpha"));
    assert_eq!(
        json["flattened"][0]["venue"],
        serde_json::json!("pumpFunCurve")
    );
    assert!(json["realizedPnlLamports"].as_i64().expect("a number") < 0);

    engine.finish_shutdown();
}

#[test]
fn the_database_reports_what_is_closed_and_what_is_still_in_flight() {
    let temp = TempDb::new("health");
    let db = temp.open();
    a_position(&db, "closed");
    a_position(&db, "flying");

    let signer = Arc::new(MockSolanaSigner::new());
    signer.inject("flying", MockFault::NotConfirmed);
    let engine = engine_with_signer(db, &signer);
    engine.emergency_unwind(None, "sell what will sell", "operator");

    let health = engine.database().health().expect("health");
    assert_eq!(health.realized_pnl.live.closed, 1);
    assert!(health.realized_pnl.live.realized_lamports < 0);
    assert_eq!(
        health.realized_pnl.paper,
        Default::default(),
        "live results do not leak into paper ones"
    );
    assert_eq!(
        health.exits_in_flight, 0,
        "the one that failed to confirm is failed, not in flight"
    );
    assert!(health.intent_transitions >= 8, "four steps for each exit");
    assert!(
        health.needs_unwind >= 2,
        "both obligations, and the exit that may or may not have landed"
    );

    engine.finish_shutdown();
    let _ = temp.path();
}

/// How many of one thing a snapshot says there are.
fn entered(counts: &[sts_lib::metrics::StateCount], state: &str) -> u64 {
    counts
        .iter()
        .find(|count| count.state == state)
        .unwrap_or_else(|| panic!("{state} is one of the declared states"))
        .entered
}

fn in_state(counts: &[sts_lib::metrics::StateCount], state: &str) -> i64 {
    counts
        .iter()
        .find(|count| count.state == state)
        .unwrap_or_else(|| panic!("{state} is one of the declared states"))
        .in_state
}

#[test]
fn an_unwind_counts_every_step_it_takes_through_the_signer() {
    let temp = TempDb::new("metrics");
    let db = temp.open();
    for intent in ["alpha", "beta"] {
        a_position(&db, intent);
    }

    let signer = Arc::new(MockSolanaSigner::new());
    let engine = engine_with_signer(db, &signer);
    let metrics = Arc::new(MetricsCollector::new());
    assert!(engine.attach_metrics(Arc::clone(&metrics)));
    assert!(
        !engine.attach_metrics(Arc::new(MetricsCollector::new())),
        "a second collector would count the same exit twice"
    );

    let receipt = engine.emergency_unwind(None, "counting the exits", "operator");
    assert_eq!(receipt.exits_confirmed, 2, "{:?}", receipt.problems);
    engine.finish_shutdown();

    let snapshot = metrics.snapshot();

    // Two exits, each walking the signer's four states in order and stopping.
    for state in [
        "exit_constructed",
        "exit_signed",
        "exit_broadcast",
        "exit_confirmed",
    ] {
        assert_eq!(entered(&snapshot.execution.signer, state), 2, "{state}");
    }
    assert_eq!(entered(&snapshot.execution.signer, "exit_failed"), 0);
    assert_eq!(in_state(&snapshot.execution.signer, "exit_confirmed"), 2);
    assert_eq!(
        snapshot.execution.in_flight_exits, 0,
        "both exits confirmed, so the signer is carrying nothing"
    );

    // Each exit is also an intent of its own, walking the ordinary six states
    // as a sell — and each original obligation was abandoned.
    for state in [
        "intent_created",
        "validated",
        "sent",
        "confirmed",
        "completed",
    ] {
        assert_eq!(entered(&snapshot.execution.intents, state), 2, "{state}");
    }
    assert_eq!(
        entered(&snapshot.execution.intents, "aborted"),
        2,
        "the two originals"
    );
    assert_eq!(
        snapshot.execution.in_flight_intents, 0,
        "every exit completed and every original was abandoned"
    );
    assert_eq!(
        snapshot.execution.unobserved, 2,
        "the two originals were already on chain before this collector existed"
    );
}

#[test]
fn an_engine_with_no_collector_unwinds_exactly_the_same_way() {
    let temp = TempDb::new("uncounted");
    let db = temp.open();
    a_position(&db, "alpha");

    let signer = Arc::new(MockSolanaSigner::new());
    let engine = engine_with_signer(db, &signer);
    // No `attach_metrics` at all. Measuring the engine must be something it can
    // be run without.
    let receipt = engine.emergency_unwind(None, "no collector attached", "operator");
    assert_eq!(receipt.exits_confirmed, 1, "{:?}", receipt.problems);
    assert!(receipt.problems.is_empty(), "{:?}", receipt.problems);
    engine.finish_shutdown();

    let conn = temp.read();
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM intent_transitions WHERE to_state = 'exit_confirmed'"
        ),
        1
    );
}

/// Simulation is mandatory, and the file is where an operator can see that it
/// happened.
///
/// `build_exit` refuses anything that does not simulate, so there is no way to
/// reach this ledger with an exit that skipped the check — which is what makes
/// the line on the `exit_constructed` row meaningful rather than decorative:
/// the row existing *is* the evidence, and the detail is what it was.
#[test]
fn every_exit_is_simulated_before_it_is_signed_and_the_file_says_so() {
    let temp = TempDb::new("simulated");
    let db = temp.open();
    for intent in ["alpha", "beta", "gamma"] {
        a_position(&db, intent);
    }

    let signer = Arc::new(MockSolanaSigner::new());
    let engine = engine_with_signer(db, &signer);
    let receipt = engine.emergency_unwind(None, "get out of everything", "operator");
    assert_eq!(receipt.exits_confirmed, 3);
    engine.finish_shutdown();

    let conn = temp.read();
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM intent_transitions
              WHERE to_state = 'exit_constructed' AND detail LIKE 'simulated %'"
        ),
        3,
        "every exit that was built recorded the simulation that licensed it"
    );
    // Nothing was signed that was not constructed first, so the count of
    // simulations is the count of signatures.
    assert_eq!(
        count(
            &conn,
            "SELECT COUNT(*) FROM intent_transitions WHERE to_state = 'exit_signed'"
        ),
        3
    );

    // And the numbers on the line are the numbers on the row beside it: the
    // floor the simulation read out of the instruction data is the floor the
    // ledger recorded off the plan, so the bytes and the record agree.
    let mut statement = conn
        .prepare(
            "SELECT detail, min_out_lamports FROM intent_transitions
              WHERE to_state = 'exit_constructed' ORDER BY intent_id",
        )
        .expect("prepares");
    let rows: Vec<(String, i64)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("queries")
        .map(|row| row.expect("a readable row"))
        .collect();
    assert_eq!(rows.len(), 3);
    for (detail, min_out) in rows {
        assert!(min_out > 0, "{detail}");
        assert!(
            detail.contains(&format!("floor {min_out} lamports")),
            "the line and the column disagree: {detail} against {min_out}"
        );
        assert!(detail.contains("to send"), "{detail}");
    }
}
