//! The forensic log, the checkpoints and the counter, against a real run.
//!
//! `forensics.rs`'s own tests cover the three tables in isolation: what the
//! columns refuse, what the triggers refuse, what the chain notices. These are
//! the ones that need the whole pipeline, and they are all versions of the same
//! question — does the record agree with what actually happened.
//!
//! Four seams:
//!
//! 1. **verdict → log.** Every launch the run reached a verdict on is one row,
//!    and the funnel over the table is the funnel in the report. Two records of
//!    one run, written by different code, checked against each other rather
//!    than each against itself.
//! 2. **log → book.** Every row that says it entered names an intent the trade
//!    journal has a trade for, and every trade the journal has is named by a
//!    row. Neither table is allowed to know about a position the other does not.
//! 3. **run → checkpoint.** A snapshot taken after the run is the book, the
//!    chain verifies, and reopening the file finds all of that still true.
//! 4. **run → run.** Two identical runs into two fresh files produce identical
//!    logs, revision for revision, and identical digests. That is Phase 3's
//!    byte-identical criterion applied to the record of the decisions rather
//!    than to the decisions themselves — a log whose ordering depended on a
//!    wall clock would fail it, which is the reason the ordering is a counter.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use sts_lib::daemon::{
    GateProfile, PipelineReport, Scenario, ScenarioConfig, SimulatedExecution, StopFlag,
};
use sts_lib::db::{Database, ExecutionMode};
use sts_lib::execution::MockSolanaSigner;
use sts_lib::fixtures::{self, GeneratorConfig};
use sts_lib::forensics::{Decision, SnapshotVerdict, StateLogFilter, StateLogger, StateRow};
use sts_lib::journal::{JournalFilter, TradeRow};
use sts_lib::metrics::MetricsCollector;
use sts_lib::types::{
    CircuitBreaker, FastPathGate, LiquidityThresholds, OperatingMode, RiskSnapshot,
};

/// The profile that actually enters on this corpus. The strict default refuses
/// everything, which makes a fine funnel and a poor test of the entry path.
const ENTERING_PROFILE: GateProfile = GateProfile::V1;

/// A clock the tests declare rather than read.
///
/// The digest covers `taken_at_ms`, so two runs only produce the same digest if
/// they are checkpointed at the same declared instant. In a replay that is the
/// fixture clock; here it is this constant, which is the same argument.
const AT_MS: i64 = 1_700_000_000_000;

// ---------------------------------------------------------------------------
// scaffolding
// ---------------------------------------------------------------------------

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sts-forensics-e2e-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the temp root is creatable");
        TempRoot(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn corpus(&self) -> PathBuf {
        let root = self.join("corpus");
        let cases =
            fixtures::generate_all(&GeneratorConfig::default()).expect("the corpus generates");
        fixtures::write_corpus(&root, &cases, true).expect("the corpus writes");
        root
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The risk frame these runs record against.
///
/// The same shape `daemon::REPLAY_RISK` uses and for the same reason: there is
/// no account behind a fixture replay, so the balances are zero and say so.
/// Repeated here rather than exported, because a test that reused the constant
/// under test could not notice it changing.
fn replay_risk() -> RiskSnapshot {
    RiskSnapshot {
        at_ms: 0,
        mode: OperatingMode::Replay,
        equity_lamports: 0,
        high_water_lamports: 0,
        drawdown_bps: 0,
        max_drawdown_bps: 10_000,
        open_positions: 0,
        max_open_positions: u16::MAX,
        circuit_breaker: CircuitBreaker::Clear,
        fast_path: FastPathGate::CLOSED,
        liquidity: LiquidityThresholds {
            min_pool_lamports: 0,
            exit_only_below_lamports: 0,
            max_pool_share_bps: 150,
        },
    }
}

/// Plays a corpus into a fresh file with the forensic log attached, and hands
/// back both records of the run.
fn play(fixtures: &Path, db_path: &Path) -> (PipelineReport, Database) {
    let db = Arc::new(Database::open(db_path).expect("sts.db opens"));
    let backend = Arc::new(MockSolanaSigner::new());
    let metrics = MetricsCollector::new();
    let logger = StateLogger::start(Arc::clone(&db), ExecutionMode::Replay);

    let report = Scenario::new(ScenarioConfig {
        fixtures: fixtures.to_path_buf(),
        gate_profile: ENTERING_PROFILE,
        ..ScenarioConfig::default()
    })
    .executing_with(SimulatedExecution::new(&db, &backend).with_metrics(&metrics))
    .with_metrics(&metrics)
    .stopping_on(&StopFlag::new())
    .recording_to(&logger, replay_risk())
    .run()
    .expect("the corpus plays");

    // Everything queued is committed before anything is read back. The writer
    // is asynchronous by design; a test that asserted against it without
    // stopping it would be asserting against a race.
    logger.stop();
    let stats = logger.stats();
    assert_eq!(stats.dropped, 0, "the writer fell behind a fixture replay");
    assert_eq!(stats.failed, 0, "a batch would not commit");

    // The handle the caller reads through. A second `Database::open` rather
    // than unwrapping the `Arc`, because the point of reading it back is to
    // read what is on disk.
    let reopened = Database::open(db_path).expect("sts.db reopens");
    (report, reopened)
}

fn log_rows(db: &Database) -> Vec<StateRow> {
    db.query_state_log(&StateLogFilter::after_revision(ExecutionMode::Replay, 0))
        .expect("the log reads")
}

fn trades(db: &Database) -> Vec<TradeRow> {
    db.query_journal(&JournalFilter {
        limit: 5_000,
        ..JournalFilter::in_mode(ExecutionMode::Replay)
    })
    .expect("the book reads")
}

// ---------------------------------------------------------------------------
// verdict → log
// ---------------------------------------------------------------------------

#[test]
fn every_verdict_the_run_reached_is_one_row_in_the_log() {
    let root = TempRoot::new("one-row");
    let corpus = root.corpus();
    let (report, db) = play(&corpus, &root.join("sts.db"));

    let rows = log_rows(&db);
    assert_eq!(
        rows.len() as u32,
        report.totals.decided,
        "the report reached {} verdicts and the log holds {}",
        report.totals.decided,
        rows.len()
    );
    assert!(
        report.totals.decided > 0,
        "the corpus decided nothing, so nothing was tested"
    );

    // Gapless from one. The property everything else rests on.
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(
            row.revision,
            index as u64 + 1,
            "the revisions have a hole in them"
        );
        assert_eq!(row.mode, ExecutionMode::Replay);
    }
    assert_eq!(
        db.current_revision(ExecutionMode::Replay).expect("reads"),
        rows.len() as u64
    );
    assert_eq!(db.current_revision(ExecutionMode::Live).expect("reads"), 0);
    assert_eq!(db.current_revision(ExecutionMode::Paper).expect("reads"), 0);
}

#[test]
fn the_funnel_over_the_table_is_the_funnel_in_the_report() {
    // Two records of one run, written by different code from the same verdict.
    // If they disagree, one of them is lying about what the strategy did.
    let root = TempRoot::new("funnel");
    let corpus = root.corpus();
    let (report, db) = play(&corpus, &root.join("sts.db"));

    let funnel = db
        .state_funnel(&StateLogFilter::in_mode(ExecutionMode::Replay))
        .expect("counts");

    assert_eq!(funnel.rows as u32, report.totals.decided);
    assert_eq!(
        funnel.entered as u32, report.totals.entered,
        "the log and the report disagree about how many positions were opened"
    );

    let reported: BTreeMap<&str, u32> = report
        .totals
        .reasons
        .iter()
        .map(|(name, count)| (name.as_str(), *count))
        .collect();
    let logged: BTreeMap<&str, i64> = funnel
        .reasons
        .iter()
        .map(|(name, count)| (name.as_str(), *count))
        .collect();
    assert_eq!(
        reported.len(),
        logged.len(),
        "the two funnels are different shapes"
    );
    for (reason, count) in reported {
        assert_eq!(
            logged.get(reason).copied(),
            Some(i64::from(count)),
            "the two records disagree about {reason}"
        );
    }
}

#[test]
fn a_launch_the_gate_accepted_and_nothing_opened_is_a_deferral() {
    // The corpus carries launches whose window the recording cut short. The
    // gate accepts them and `decide_due` refuses to open on them, and the whole
    // reason `Decision` has a third arm is so that shows up as itself rather
    // than as a refusal the strategy never made.
    let root = TempRoot::new("deferred");
    let corpus = root.corpus();
    let (report, db) = play(&corpus, &root.join("sts.db"));

    let rows = log_rows(&db);
    for row in &rows {
        match row.record.decision {
            Decision::Entered => {
                assert!(row.record.intent_id.is_some(), "an entry named nothing");
                assert!(
                    row.record.window_closed,
                    "a position was opened on a short window"
                );
            }
            Decision::Deferred => {
                assert!(row.record.intent_id.is_none());
                assert_eq!(
                    row.record.reason,
                    sts_lib::strategy::GateReason::Accepted,
                    "a deferral is a launch the gate said yes to"
                );
            }
            Decision::Refused => {
                assert!(row.record.intent_id.is_none());
                assert_ne!(
                    row.record.reason,
                    sts_lib::strategy::GateReason::Accepted,
                    "a refusal cannot be the one reason that trades"
                );
            }
        }
    }

    let entered = rows
        .iter()
        .filter(|r| r.record.decision == Decision::Entered)
        .count();
    assert_eq!(entered as u32, report.totals.entered);
}

#[test]
fn nothing_in_the_log_read_evidence_from_after_its_own_decision() {
    // The column refuses it, so this cannot fail without the schema having
    // changed — which is the point of asserting it against a real run rather
    // than against a row a test built.
    let root = TempRoot::new("leakage");
    let corpus = root.corpus();
    let (_, db) = play(&corpus, &root.join("sts.db"));

    for row in log_rows(&db) {
        assert!(
            row.record.evidence_to_ms <= row.record.decided_at_ms,
            "{} read evidence from after it decided",
            row.record.mint
        );
    }
}

// ---------------------------------------------------------------------------
// log → book
// ---------------------------------------------------------------------------

#[test]
fn every_entry_in_the_log_is_a_trade_in_the_book_and_the_other_way_round() {
    let root = TempRoot::new("join");
    let corpus = root.corpus();
    let (_, db) = play(&corpus, &root.join("sts.db"));

    let named: BTreeMap<String, String> = log_rows(&db)
        .into_iter()
        .filter_map(|row| {
            row.record
                .intent_id
                .map(|intent| (intent, row.record.mint.clone()))
        })
        .collect();
    let booked: BTreeMap<String, String> = trades(&db)
        .into_iter()
        .map(|trade| (trade.trade_id, trade.mint))
        .collect();

    assert!(
        !named.is_empty(),
        "the run opened nothing, so the join was not tested"
    );
    assert_eq!(
        named.keys().collect::<Vec<_>>(),
        booked.keys().collect::<Vec<_>>(),
        "the log and the book name different positions"
    );
    for (intent, mint) in &named {
        assert_eq!(
            booked.get(intent),
            Some(mint),
            "{intent} is against {mint} in the log and something else in the book"
        );
    }
}

// ---------------------------------------------------------------------------
// run → checkpoint
// ---------------------------------------------------------------------------

#[test]
fn a_checkpoint_after_the_run_is_the_book_and_the_chain_verifies() {
    let root = TempRoot::new("checkpoint");
    let corpus = root.corpus();
    let (_, db) = play(&corpus, &root.join("sts.db"));

    let rows = log_rows(&db);
    let snapshot = db
        .take_journal_snapshot(ExecutionMode::Replay, AT_MS)
        .expect("takes");

    assert_eq!(snapshot.revision, rows.len() as u64);
    assert_eq!(snapshot.seq, 1, "the first checkpoint of this file");
    assert_eq!(snapshot.covers_from, 0, "and it speaks for the whole log");
    assert_eq!(snapshot.rows_since, rows.len() as i64);
    assert_eq!(
        snapshot.entered_since + snapshot.refused_since + snapshot.deferred_since,
        snapshot.rows_since
    );

    let book = db
        .journal_totals(&JournalFilter::in_mode(ExecutionMode::Replay))
        .expect("adds up");
    assert_eq!(snapshot.totals, book, "the checkpoint is not the book");
    assert_eq!(snapshot.totals.trades, trades(&db).len() as i64);

    // Nothing has moved since, so the strongest verdict is available.
    assert_eq!(
        db.verify_journal_snapshot(ExecutionMode::Replay)
            .expect("verifies"),
        SnapshotVerdict::Matches {
            revision: snapshot.revision
        }
    );

    let chain = db
        .verify_journal_snapshot_chain(ExecutionMode::Replay)
        .expect("walks");
    assert!(chain.is_intact(), "the chain did not verify: {chain:?}");
    assert_eq!(chain.snapshots, 1);
    assert_eq!(chain.intervals_checked, 1);
    assert_eq!(chain.intervals_pruned, 0);
}

#[test]
fn a_restart_finds_the_counter_the_checkpoints_and_the_book_where_it_left_them() {
    let root = TempRoot::new("restart");
    let corpus = root.corpus();
    let path = root.join("sts.db");

    let (before_revision, before_digest, before_trades) = {
        let (_, db) = play(&corpus, &path);
        let snapshot = db
            .take_journal_snapshot(ExecutionMode::Replay, AT_MS)
            .expect("takes");
        let trades = trades(&db).len();
        let revision = db.current_revision(ExecutionMode::Replay).expect("reads");
        db.close();
        (revision, snapshot.digest, trades)
    };

    // A different process, the same file.
    let db = Database::open(&path).expect("reopens");
    let warm = db.warm_start(ExecutionMode::Replay).expect("reads");

    assert!(
        warm.is_clean(),
        "a file nobody touched did not warm-start clean: {warm:?}"
    );
    assert_eq!(
        warm.revision, before_revision,
        "the counter moved across the restart"
    );
    assert_eq!(
        warm.uncheckpointed, 0,
        "the checkpoint covered the whole log"
    );
    assert_eq!(
        warm.snapshot.as_ref().map(|s| s.digest.as_str()),
        Some(before_digest.as_str()),
        "the checkpoint came back different"
    );
    assert_eq!(
        warm.verdict,
        SnapshotVerdict::Matches {
            revision: before_revision
        }
    );
    assert_eq!(trades(&db).len(), before_trades);

    // And the next thing written carries on from where the last process got to
    // rather than starting again.
    let logger = StateLogger::start(
        Arc::new(Database::open(&path).expect("opens")),
        ExecutionMode::Replay,
    );
    logger.stop();
    assert_eq!(
        db.current_revision(ExecutionMode::Replay).expect("reads"),
        before_revision
    );
}

#[test]
fn a_book_edited_between_processes_does_not_warm_start_clean() {
    // The failure this whole arrangement exists to notice: somebody with
    // `sqlite3` open and a number they would rather the book said.
    let root = TempRoot::new("tampered");
    let corpus = root.corpus();
    let path = root.join("sts.db");

    {
        let (_, db) = play(&corpus, &path);
        db.take_journal_snapshot(ExecutionMode::Replay, AT_MS)
            .expect("takes");
        db.close();
    }
    {
        let conn = rusqlite::Connection::open(&path).expect("opens");
        let changed = conn
            .execute(
                "UPDATE journal_trades SET cost_basis_lamports = cost_basis_lamports + 1
                  WHERE mode = 'replay'",
                [],
            )
            .expect("edits");
        assert!(changed > 0, "there was nothing in the book to edit");
    }

    let db = Database::open(&path).expect("reopens");
    let warm = db.warm_start(ExecutionMode::Replay).expect("reads");
    assert!(!warm.is_clean(), "an edited book warm-started clean");
    assert!(
        warm.verdict.is_divergence(),
        "the divergence was not reported: {warm:?}"
    );
    // The chain itself is untouched — nothing edited a snapshot — and the
    // report says so rather than blaming the wrong thing.
    assert!(
        warm.chain.is_intact(),
        "editing the book was reported as a broken chain: {:?}",
        warm.chain
    );
}

// ---------------------------------------------------------------------------
// run → run
// ---------------------------------------------------------------------------

#[test]
fn one_corpus_and_one_policy_produce_one_forensic_log() {
    // Phase 3's byte-identical criterion, applied to the record of the
    // decisions. A log ordered by a wall clock would fail this; a log ordered
    // by a per-mode counter cannot.
    let root = TempRoot::new("deterministic");
    let corpus = root.corpus();

    let (left_report, left_db) = play(&corpus, &root.join("left.db"));
    let (right_report, right_db) = play(&corpus, &root.join("right.db"));

    assert_eq!(
        left_report.totals, right_report.totals,
        "the two runs disagreed"
    );

    let left = log_rows(&left_db);
    let right = log_rows(&right_db);
    assert_eq!(left.len(), right.len());
    assert!(!left.is_empty(), "two empty logs match trivially");
    for (index, (a, b)) in left.iter().zip(right.iter()).enumerate() {
        assert_eq!(
            a, b,
            "row {index} differs between two runs of the same corpus"
        );
    }

    // And the checkpoints over them, taken at the same declared instant, hash
    // to the same thing.
    let left_snapshot = left_db
        .take_journal_snapshot(ExecutionMode::Replay, AT_MS)
        .expect("takes");
    let right_snapshot = right_db
        .take_journal_snapshot(ExecutionMode::Replay, AT_MS)
        .expect("takes");
    assert_eq!(
        left_snapshot.digest, right_snapshot.digest,
        "two identical books checkpointed at the same instant hashed differently"
    );
    assert_eq!(left_snapshot, right_snapshot);
}

#[test]
fn a_second_run_into_the_same_file_carries_the_counter_on() {
    // Two runs, one file. The book's keys are deterministic so the trades
    // conflict and update; the log's key is a counter so the second run's
    // verdicts are new rows. Both are right, and they are right for different
    // reasons — which is why the log has no `ON CONFLICT`.
    let root = TempRoot::new("twice");
    let corpus = root.corpus();
    let path = root.join("sts.db");

    let (_, first) = play(&corpus, &path);
    let after_one = first
        .current_revision(ExecutionMode::Replay)
        .expect("reads");
    let trades_after_one = trades(&first).len();
    first.close();

    let (_, second) = play(&corpus, &path);
    let after_two = second
        .current_revision(ExecutionMode::Replay)
        .expect("reads");

    assert_eq!(
        after_two,
        after_one * 2,
        "the second run's verdicts were not recorded"
    );
    assert_eq!(
        trades(&second).len(),
        trades_after_one,
        "the second run duplicated the book instead of updating it"
    );

    let rows = log_rows(&second);
    assert_eq!(rows.len() as u64, after_two);
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row.revision, index as u64 + 1, "the second run left a hole");
    }

    // One checkpoint over both runs, and it accounts for all of it.
    let snapshot = second
        .take_journal_snapshot(ExecutionMode::Replay, AT_MS)
        .expect("takes");
    assert_eq!(snapshot.revision, after_two);
    assert_eq!(snapshot.rows_since, after_two as i64);
    assert!(second
        .verify_journal_snapshot_chain(ExecutionMode::Replay)
        .expect("walks")
        .is_intact());
}
