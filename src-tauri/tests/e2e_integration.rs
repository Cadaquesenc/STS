//! The whole engine, end to end, against a corpus it generated for itself.
//!
//! `backtest.rs`, `replay.rs`, `strategy/` and `execution.rs` each have unit
//! tests for what they do on their own, and `emergency_unwind.rs` has the ones
//! that need two processes. These are the ones that need every module at once:
//! the properties that only exist where the stages meet, and that no test
//! confined to one module can see.
//!
//! Five stages, and something is asserted about each seam:
//!
//! 1. **fixtures → feed.** Every case built to be refused is refused, every
//!    case built to verify plays, and the record counts are the ones the
//!    generator wrote down. Graded against each case's own `expected.json`
//!    rather than against whatever the harness did last time, which is the
//!    difference between a regression suite and one that ratifies its own bugs.
//! 2. **feed → detector.** The bundle the generator planted is the bundle the
//!    detector finds, wallet for wallet.
//! 3. **detector → gate.** A launch is decided when its opening window closes
//!    and not when the recording ends, so the dump that follows a bundle is not
//!    in the evidence the gate read.
//! 4. **gate → execution.** Exactly the accepted launches open positions, the
//!    exit signs, tips a published Jito account, broadcasts and books — checked
//!    by reopening `sts.db` with a plain `rusqlite` connection rather than by
//!    asking the code that wrote it — and nothing anywhere is live.
//! 5. **execution → telemetry.** Every line of the run is on the exported
//!    stream, and the count the report claims is the count in the file.
//!
//! Then two more that are about our own order rather than about the launch:
//!
//! 7. **gate → the curve our fill comes off.** The entry is priced at the size
//!    the executor would actually send, the participation cap and §15.2's
//!    threshold read the two different reserves they are each about, and a
//!    refusal on our order is kept apart from a refusal on the launch.
//! 8. **many pools at once.** Every pool in a run is judged against its own
//!    curve, the refusals cascade pool by pool as the cap widens and never
//!    unwind, and `--private-entry` moves the verdicts without moving a size or
//!    hiding an exposure.
//!
//! And the properties that are about the run rather than about a stage: one
//! fixture and one policy produce byte-identical reports, the whole report reads
//! under one naming rule, and a real SIGINT or SIGTERM to a real `sts daemon`
//! process tears it down cleanly, writes its report, leaves the telemetry file
//! whole, and sells nothing on the way out.
//!
//! Everything goes through the public API. The signal tests spawn the built
//! binary — `CARGO_BIN_EXE_sts`, which Cargo points at the same `sts` this
//! suite was compiled beside — because a signal handler that is only ever
//! called by a test is a signal handler that has never been tested.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use sts_lib::daemon::{
    GateProfile, NdjsonSink, PipelineReport, Scenario, ScenarioConfig, SimulatedExecution,
    StopFlag, StopReason, TelemetryTarget,
};
use sts_lib::db::Database;
use sts_lib::execution::{ExecutionEngine, MockSolanaSigner};
use sts_lib::fixtures::{self, Expected, GeneratorConfig};
use sts_lib::metrics::MetricsCollector;
use sts_lib::strategy::{GateReason, SandwichGuard};
use sts_lib::telemetry::{TelemetryHub, TelemetrySink};

// ---------------------------------------------------------------------------
// scaffolding
// ---------------------------------------------------------------------------

/// A directory of its own per test, removed when the test ends.
///
/// Per test rather than shared, so the suite can run its tests in parallel and
/// so a failure leaves nothing behind that the next run would read.
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sts-e2e-{name}-{}-{}",
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

    /// Writes the whole synthetic stress corpus under this root.
    ///
    /// Generated here rather than checked in: the corpus is a function of the
    /// scenario, the knobs and the seed, so a test that builds it is testing
    /// the generator too, and a corpus in the repository would be a set of
    /// opaque blobs nobody can regenerate to check.
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

/// What each case's `expected.json` says the harness should conclude.
fn expectations(corpus: &Path) -> BTreeMap<String, Expected> {
    let mut found = BTreeMap::new();
    for entry in std::fs::read_dir(corpus).expect("the corpus is readable") {
        let dir = entry.expect("the entry reads").path();
        let path = dir.join("expected.json");
        if !path.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("expected.json reads");
        let expected: Expected = serde_json::from_str(&text).expect("expected.json parses");
        found.insert(expected.case.clone(), expected);
    }
    assert!(!found.is_empty(), "the corpus carries its own expectations");
    found
}

fn config(fixtures: &Path, profile: GateProfile) -> ScenarioConfig {
    ScenarioConfig {
        fixtures: fixtures.to_path_buf(),
        gate_profile: profile,
        ..ScenarioConfig::default()
    }
}

/// Plays a corpus into a ledger and hands back what happened.
fn play(fixtures: &Path, db_path: &Path, profile: GateProfile) -> PipelineReport {
    play_until(fixtures, db_path, profile, &StopFlag::new())
}

fn play_until(
    fixtures: &Path,
    db_path: &Path,
    profile: GateProfile,
    stop: &StopFlag,
) -> PipelineReport {
    let db = Database::open(db_path).expect("sts.db opens");
    let backend = Arc::new(MockSolanaSigner::new());
    let metrics = MetricsCollector::new();
    let report = Scenario::new(config(fixtures, profile))
        .executing_with(SimulatedExecution::new(&db, &backend).with_metrics(&metrics))
        .with_metrics(&metrics)
        .stopping_on(stop)
        .run()
        .expect("the corpus plays");
    db.close();
    report
}

/// Plays a corpus under a config the caller built, rather than a profile.
fn play_with(config: ScenarioConfig, db_path: &Path) -> PipelineReport {
    let db = Database::open(db_path).expect("sts.db opens");
    let backend = Arc::new(MockSolanaSigner::new());
    let metrics = MetricsCollector::new();
    let report = Scenario::new(config)
        .executing_with(SimulatedExecution::new(&db, &backend).with_metrics(&metrics))
        .with_metrics(&metrics)
        .run()
        .expect("the corpus plays");
    db.close();
    report
}

/// The one launch in the corpus the v1 rule likes, which is the only launch
/// that ever reaches the sandwich guard — every other one dies on a question
/// about the launch long before the gate asks about our order.
fn the_accepted_mint(report: &PipelineReport) -> String {
    let accepted: Vec<&str> = launches(report)
        .into_iter()
        .filter(|(_, launch)| launch.reason == GateReason::Accepted)
        .map(|(mint, _)| mint)
        .collect();
    assert_eq!(
        accepted.len(),
        1,
        "the corpus has exactly one launch the v1 rule accepts"
    );
    accepted[0].to_string()
}

/// Every launch the run reached a verdict on, by mint.
fn launches(report: &PipelineReport) -> BTreeMap<&str, &sts_lib::daemon::LaunchOutcome> {
    report
        .cases
        .iter()
        .flat_map(|case| case.launches.iter())
        .map(|launch| (launch.mint.as_str(), launch))
        .collect()
}

fn rows(conn: &rusqlite::Connection, sql: &str) -> Vec<Vec<String>> {
    let mut statement = conn.prepare(sql).expect("the query prepares");
    let columns = statement.column_count();
    let found = statement
        .query_map([], |row| {
            Ok((0..columns)
                .map(|index| match row.get_ref(index) {
                    Ok(rusqlite::types::ValueRef::Null) => String::new(),
                    Ok(rusqlite::types::ValueRef::Text(text)) => {
                        String::from_utf8_lossy(text).into_owned()
                    }
                    Ok(rusqlite::types::ValueRef::Integer(number)) => number.to_string(),
                    Ok(rusqlite::types::ValueRef::Real(number)) => number.to_string(),
                    _ => String::new(),
                })
                .collect())
        })
        .expect("the query runs")
        .collect::<Result<Vec<Vec<String>>, _>>()
        .expect("every row reads");
    found
}

// ---------------------------------------------------------------------------
// stage 1 — the fixtures, and the feed over them
// ---------------------------------------------------------------------------

#[test]
fn every_case_is_played_or_refused_exactly_as_its_own_expectations_say() {
    let root = TempRoot::new("stage1");
    let corpus = root.corpus();
    let expected = expectations(&corpus);
    let report = play(&corpus, &root.join("sts.db"), GateProfile::Default);

    assert_eq!(
        report.cases.len(),
        expected.len(),
        "every case directory in the corpus is a case in the report"
    );

    for case in &report.cases {
        let claim = expected
            .get(&case.case)
            .unwrap_or_else(|| panic!("{} carries an expected.json", case.case));

        // A break in the chain is the one thing the loader refuses over. The
        // generator names the file and the line it broke, so "was it refused"
        // and "was it built to be refused" are the same question asked twice.
        assert_eq!(
            case.refused.is_some(),
            claim.break_file.is_some(),
            "{}: refused={:?}, and the generator says the break is at {:?}",
            case.case,
            case.refused,
            claim.break_file
        );

        if case.refused.is_some() {
            assert_eq!(
                case.records, 0,
                "{}: a refused fixture plays nothing",
                case.case
            );
            assert!(
                case.launches.is_empty(),
                "{}: a refused fixture decides nothing",
                case.case
            );
            continue;
        }

        assert_eq!(
            case.records, claim.records,
            "{}: the feed read the number of records the generator wrote",
            case.case
        );
        // `gate_ready` is the generator's word for "a run over this should be
        // accepted". The only thing separating the played cases is whether the
        // manifest's declared head is the one the records compute to, so the
        // two have to agree — and a `None` here would be the absence of the
        // check, which is deliberately not a pass.
        assert_eq!(
            case.chain_verified,
            Some(claim.gate_ready),
            "{}: the manifest check and the generator's gate_ready agree",
            case.case
        );
        assert!(
            case.replayed + case.filtered == case.frames,
            "{}: every frame was either replayed or filtered, never neither",
            case.case
        );
        assert_eq!(
            case.undecodable, 0,
            "{}: a frame the generator wrote decodes; {:?}",
            case.case, case.problems
        );
    }
}

#[test]
fn the_corpus_carries_cases_that_verify_and_cases_that_do_not() {
    // Guards the test above from passing vacuously. A corpus that happened to
    // contain only clean cases would satisfy every assertion up there while
    // proving nothing about the refusal path.
    let root = TempRoot::new("stage1-shape");
    let corpus = root.corpus();
    let report = play(&corpus, &root.join("sts.db"), GateProfile::Default);

    let refused = report.cases.iter().filter(|c| c.refused.is_some()).count();
    let played = report.cases.iter().filter(|c| c.refused.is_none()).count();
    assert!(
        refused >= 4,
        "the corpus stresses the refusal path: {refused} refused"
    );
    assert!(
        played >= 4,
        "the corpus stresses the happy path: {played} played"
    );
    assert!(
        report
            .cases
            .iter()
            .any(|c| c.refused.is_none() && c.chain_verified == Some(false)),
        "one case plays and still fails its manifest check — a truncated recording \
         is not the same failure as a tampered one"
    );
}

// ---------------------------------------------------------------------------
// stage 2 — detection
// ---------------------------------------------------------------------------

#[test]
fn the_detector_finds_the_bundle_the_generator_planted() {
    let root = TempRoot::new("stage2");
    let corpus = root.corpus();
    let expected = expectations(&corpus);
    let report = play(&corpus, &root.join("sts.db"), GateProfile::Default);
    let found = launches(&report);

    let mut checked = 0;
    for claim in expected.values() {
        for launch in &claim.launches {
            let outcome = found.get(launch.mint.as_str()).unwrap_or_else(|| {
                panic!(
                    "{} is a launch the generator wrote and the feed opened",
                    launch.mint
                )
            });
            assert_eq!(
                outcome.largest_funder_wallets, launch.bundled_wallets,
                "{}: the generator put {} wallet(s) behind one funder",
                launch.mint, launch.bundled_wallets
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 5,
        "every launch the generator described was checked"
    );

    // And the one that is supposed to be a syndicate is read as one, all the
    // way through to the tags. Without this the assertion above is satisfied by
    // a detector that finds funders and understands nothing.
    let sybil = found
        .get("mint-sybil-rug")
        .expect("the sybil case opens a launch");
    assert_eq!(sybil.largest_funder_wallets, 6);
    assert_eq!(
        sybil.bundle_wallets, 6,
        "all six landed in one bundle, which is what the case is"
    );
    assert_eq!(
        sybil.confidence_micros, 1_000_000,
        "one funder, one instant and a dev buying its own launch is as clear as this gets"
    );
    for tag in ["SHARED_FUNDER", "SAME_INSTANT_BUNDLE", "CREATOR_BOUGHT_OWN"] {
        assert!(
            sybil.tags.iter().any(|found| found.as_str() == tag),
            "the sybil case raises {tag}; it raised {:?}",
            sybil.tags
        );
    }
}

// ---------------------------------------------------------------------------
// stage 3 — the decision, and when it is taken
// ---------------------------------------------------------------------------

#[test]
fn no_launch_reads_evidence_from_after_its_own_opening_window() {
    // The property the whole decision-time discipline comes down to. Stated as
    // a number on the report rather than inferred from a verdict, so a detector
    // that started reading one record too far fails here loudly instead of
    // showing up months later as a backtest that will not reproduce live.
    let root = TempRoot::new("stage3");
    let corpus = root.corpus();
    let report = play(&corpus, &root.join("sts.db"), GateProfile::Default);
    let window_ms = report.window_ms;

    let mut checked = 0;
    for launch in launches(&report).values() {
        assert!(
            launch.evidence_to_ms >= launch.opened_at_ms,
            "{}: evidence stamped {} predates the launch at {}",
            launch.mint,
            launch.evidence_to_ms,
            launch.opened_at_ms
        );
        assert!(
            launch.evidence_to_ms <= launch.opened_at_ms + window_ms,
            "{}: read evidence from {}, which is {} ms past its window",
            launch.mint,
            launch.evidence_to_ms,
            launch.evidence_to_ms - (launch.opened_at_ms + window_ms)
        );
        if launch.window_closed {
            assert!(
                launch.decided_at_ms > launch.opened_at_ms + window_ms,
                "{}: a closed window is decided on a record past the window, not on one inside it",
                launch.mint
            );
        }
        checked += 1;
    }
    assert!(checked >= 5, "several launches were checked, not one");
}

#[test]
fn the_recording_carries_on_after_a_decision_that_did_not_read_it() {
    // Guards the test above from passing on a corpus whose recordings all
    // happen to stop at the window. The point is that there is more in the file
    // and the gate did not read it.
    let root = TempRoot::new("stage3-tail");
    let corpus = root.corpus();
    let report = play(&corpus, &root.join("sts.db"), GateProfile::Default);

    let mut proved = 0;
    for case in &report.cases {
        if case.refused.is_some() {
            continue;
        }
        let last = last_record_ms(&corpus.join(&case.case));
        for launch in &case.launches {
            if launch.evidence_to_ms < last {
                proved += 1;
            }
        }
    }
    assert!(
        proved >= 3,
        "at least three launches were decided on less than the whole recording"
    );
}

#[test]
fn the_sybil_dump_lands_after_the_decision_and_is_not_in_the_evidence() {
    // The case is a bundle that buys together and then dumps together. The dump
    // is what makes it a rug and it is the one thing a decision must not have
    // seen: a gate that reads it is a gate that cannot work in front of a live
    // feed, where the dump has not happened yet.
    //
    // This is also the case that catches the ordering mistake. The first record
    // of the dump is the record whose timestamp closes the window, so a feed
    // that applied each record before checking what was due would read the
    // first sell into the evidence and never notice.
    let root = TempRoot::new("stage3-leak");
    let corpus = root.corpus();
    let report = play(&corpus, &root.join("sts.db"), GateProfile::Default);
    let found = launches(&report);
    let sybil = found.get("mint-sybil-rug").expect("the sybil case decides");

    let sells = sell_times(&corpus.join("sybil-rug"));
    assert!(!sells.is_empty(), "the sybil case records a dump");
    let first_sell = sells.into_iter().min().expect("there is a first sell");
    assert!(
        sybil.evidence_to_ms < first_sell,
        "the gate read evidence up to {} and the first sell is at {first_sell}",
        sybil.evidence_to_ms
    );
}

/// Every `at_ms` on a sell in one case's streams, read straight off the files.
///
/// Through the same two public functions the daemon reads a fixture with, so
/// this is a second reader over the same bytes rather than a second opinion
/// about what the bytes mean.
fn last_record_ms(case: &Path) -> i64 {
    let mut last = i64::MIN;
    for file in sts_lib::backtest::fixture_files(case).expect("the case holds streams") {
        let text = std::fs::read_to_string(&file).expect("the stream reads");
        for record in sts_lib::replay::parse_stream(&text).expect("the stream parses") {
            last = last.max(record.observed_at_ms);
        }
    }
    last
}

fn sell_times(case: &Path) -> Vec<i64> {
    let mut sells = Vec::new();
    for file in sts_lib::backtest::fixture_files(case).expect("the case holds streams") {
        let text = std::fs::read_to_string(&file).expect("the stream reads");
        for record in sts_lib::replay::parse_stream(&text).expect("the stream parses") {
            let Some(frame) = record.frame.as_deref() else {
                continue;
            };
            let Ok(event) = sts_lib::backtest::decode_event(frame, record.seq) else {
                continue;
            };
            if let sts_lib::backtest::LaunchEvent::Flow(flow) = event {
                if flow.side == sts_lib::backtest::Side::Sell {
                    sells.push(flow.at_ms);
                }
            }
        }
    }
    sells
}

// ---------------------------------------------------------------------------
// stage 4 — the simulated execution
// ---------------------------------------------------------------------------

/// The profile the corpus produces an entry under.
///
/// The shipped rule refuses `sybil-rug` for `mixed-sizing` — six wallets behind
/// one funder in one slot, taking six different-sized positions — and that is
/// the rule working, not the corpus failing. `v1` is the same rule without the
/// group checks, it is kept runnable for exactly this reason, and it is what
/// gives this suite a launch to follow through the execution path.
const ENTERING_PROFILE: GateProfile = GateProfile::V1;

#[test]
fn the_shipped_rule_and_the_rule_before_it_disagree_about_the_sybil_case() {
    // The premise `ENTERING_PROFILE` rests on. If the shipped rule ever starts
    // accepting this case, the tests below stop being a test of the group
    // checks and nobody would otherwise notice.
    let root = TempRoot::new("stage4-profiles");
    let corpus = root.corpus();

    let shipped = play(&corpus, &root.join("a.db"), GateProfile::Default);
    let before = play(&corpus, &root.join("b.db"), GateProfile::V1);

    assert_eq!(
        shipped.totals.entered, 0,
        "the shipped rule enters nothing here"
    );
    assert_eq!(
        before.totals.entered, 1,
        "the rule before the group checks enters one"
    );

    let refused = launches(&shipped);
    let sybil = refused.get("mint-sybil-rug").expect("the case is decided");
    assert!(!sybil.enter);
    assert_eq!(
        sybil.reason.as_str(),
        "mixed-sizing",
        "the group landed together and took unrelated positions"
    );
}

#[test]
fn exactly_the_accepted_launches_open_positions() {
    let root = TempRoot::new("stage4-gate");
    let corpus = root.corpus();
    let report = play(&corpus, &root.join("sts.db"), ENTERING_PROFILE);

    assert!(report.executed, "the run was given a ledger and a signer");
    assert!(
        report.totals.entered > 0,
        "something was entered, or this proves nothing"
    );

    for launch in launches(&report).values() {
        assert_eq!(
            launch.enter,
            launch.execution.is_some(),
            "{}: enter={} and execution={:?}",
            launch.mint,
            launch.enter,
            launch.execution.as_ref().map(|e| &e.intent_id)
        );
        if !launch.enter {
            continue;
        }
        assert!(
            launch.window_closed,
            "{}: nothing is entered on a window the recording cut short",
            launch.mint
        );
        let opened = launch.execution.as_ref().expect("an accepted launch opens");
        assert!(
            opened.size_lamports > 0,
            "{}: a position has a size",
            launch.mint
        );
        assert!(
            opened.tokens > 0,
            "{}: a position holds tokens",
            launch.mint
        );
        assert!(
            opened.size_lamports <= report.entry_lamports,
            "{}: the participation cap only ever makes a position smaller",
            launch.mint
        );
    }
}

#[test]
fn a_run_that_opens_nothing_reaches_the_same_verdicts() {
    // The only thing `--no-execute` changes is whether a decision is acted on.
    // A funnel that moved when the trading was switched off would mean the
    // execution was feeding back into the detection.
    let root = TempRoot::new("stage4-dry");
    let corpus = root.corpus();

    let wet = play(&corpus, &root.join("wet.db"), ENTERING_PROFILE);
    let dry = {
        let db = Database::open(&root.join("dry.db")).expect("sts.db opens");
        let backend = Arc::new(MockSolanaSigner::new());
        let metrics = MetricsCollector::new();
        let report = Scenario::new(ScenarioConfig {
            execute: false,
            ..config(&corpus, ENTERING_PROFILE)
        })
        .executing_with(SimulatedExecution::new(&db, &backend).with_metrics(&metrics))
        .run()
        .expect("the corpus plays");
        db.close();
        report
    };

    assert_eq!(
        wet.totals, dry.totals,
        "the funnel does not depend on the trading"
    );
    assert_eq!(
        dry.open_positions, 0,
        "a run that opens nothing leaves nothing open"
    );
    assert!(
        dry.cases
            .iter()
            .flat_map(|case| case.launches.iter())
            .all(|launch| launch.execution.is_none()),
        "no position was opened"
    );
}

#[test]
fn the_exit_signs_tips_a_jito_account_broadcasts_and_books() {
    let root = TempRoot::new("stage4-ledger");
    let corpus = root.corpus();
    let db_path = root.join("sts.db");
    let report = play(&corpus, &db_path, ENTERING_PROFILE);

    let entered: Vec<_> = launches(&report)
        .into_values()
        .filter_map(|launch| launch.execution.as_ref())
        .collect();
    assert!(!entered.is_empty(), "a position was opened");

    // Reopened with a plain connection, so what is asserted is what is on disk
    // rather than what the code that wrote it believes it wrote.
    let conn = rusqlite::Connection::open(&db_path).expect("sts.db reopens");

    for opened in &entered {
        let states = rows(
            &conn,
            &format!(
                "SELECT state, side, mode FROM execution_logs \
                 WHERE intent_id = '{}' ORDER BY seq",
                opened.intent_id
            ),
        );
        let walk: Vec<&str> = states.iter().map(|row| row[0].as_str()).collect();
        assert_eq!(
            walk,
            vec!["intent_created", "validated", "sent", "confirmed"],
            "the entry walked the whole state machine"
        );
        assert!(
            states.iter().all(|row| row[1] == "buy"),
            "an entry is a buy"
        );
        assert!(
            states.iter().all(|row| row[2] == "replay"),
            "every row this harness writes is a replay row"
        );

        let exit = opened.exit.as_ref().expect("the position was flattened");
        assert_eq!(exit.state, "flattened", "the exit confirmed: {exit:?}");
        assert!(!exit.still_at_risk, "a confirmed exit closes the position");
        let exit_intent = exit
            .exit_intent_id
            .as_deref()
            .expect("a flattened position has an exit intent");

        let lifecycle = rows(
            &conn,
            &format!(
                "SELECT to_state, detail, out_lamports FROM intent_transitions \
                 WHERE intent_id = '{exit_intent}' ORDER BY seq"
            ),
        );
        let steps: Vec<&str> = lifecycle.iter().map(|row| row[0].as_str()).collect();
        assert_eq!(
            steps,
            vec![
                "exit_constructed",
                "exit_signed",
                "exit_broadcast",
                "exit_confirmed"
            ],
            "the exit walked its own lifecycle"
        );

        // Annex J step 8: the tip is tracked with the bundle, on the row that
        // has the signature. This is the whole of "simulated Jito execution"
        // being more than a phrase — there is a transfer to a published tip
        // account in the transaction that sold the position.
        let signed = &lifecycle[1];
        let tip = &signed[1];
        assert!(
            tip.starts_with("tipped ") && tip.contains(" lamports to "),
            "the signed step records what it bid: {tip:?}"
        );
        let account = tip
            .split(" lamports to ")
            .nth(1)
            .and_then(|rest| rest.split(" on attempt ").next())
            .expect("the tip names an account");
        assert!(
            sts_lib::execution::JITO_TIP_KEYS
                .iter()
                .any(|key| key.to_string() == account),
            "{account} is one of the published Jito tip accounts"
        );

        let booked: i64 = lifecycle[3][2]
            .parse()
            .expect("the confirmed step books proceeds");
        assert!(booked > 0, "a confirmed sale came back with something");
    }

    // Nothing was left behind, and nothing was recorded as anything but a
    // replay. The second half is the one that matters: a row that said `live`
    // would be this harness claiming a real trade.
    assert_eq!(
        report.open_positions, 0,
        "every position this run opened was closed"
    );
    let live = rows(
        &conn,
        "SELECT COUNT(*) FROM execution_logs WHERE mode <> 'replay'",
    );
    assert_eq!(
        live[0][0], "0",
        "no row in the ledger is anything but a replay"
    );
    let live_exits = rows(
        &conn,
        "SELECT COUNT(*) FROM intent_transitions WHERE mode <> 'replay'",
    );
    assert_eq!(
        live_exits[0][0], "0",
        "no exit in the ledger is anything but a replay"
    );
}

#[test]
fn the_only_signer_in_the_build_cannot_reach_a_network() {
    // The promotion gate, as a test rather than as a comment. If this ever
    // fails, something in this build has become able to move real money and the
    // roadmap's Phase 4 criteria are what decides whether that is allowed.
    let backend = MockSolanaSigner::new();
    assert!(!backend.is_live(), "{} claims it is live", backend.name());
}

// ---------------------------------------------------------------------------
// stage 5 — telemetry export
// ---------------------------------------------------------------------------

#[test]
fn every_stage_of_the_run_reaches_the_exported_telemetry_stream() {
    let root = TempRoot::new("stage5");
    let corpus = root.corpus();
    let stream = root.join("telemetry.ndjson");

    let hub = TelemetryHub::start();
    let sink =
        Arc::new(NdjsonSink::open(&TelemetryTarget::File(stream.clone())).expect("the sink opens"));
    hub.observe(Arc::clone(&sink) as Arc<dyn TelemetrySink>);

    let db = Database::open(&root.join("sts.db")).expect("sts.db opens");
    let backend = Arc::new(MockSolanaSigner::new());
    let metrics = MetricsCollector::new();
    let report = Scenario::new(config(&corpus, ENTERING_PROFILE))
        .executing_with(SimulatedExecution::new(&db, &backend).with_metrics(&metrics))
        .with_metrics(&metrics)
        .publishing_to(&hub)
        .run()
        .expect("the corpus plays");
    db.close();

    // Joins the pump, so the file is complete rather than complete-so-far.
    hub.shutdown();
    sink.flush();

    let text = std::fs::read_to_string(&stream).expect("the stream was written");
    let lines: Vec<serde_json::Value> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("every line is one JSON object"))
        .collect();
    assert!(!lines.is_empty(), "the stream is not empty");

    let (written, unwritable) = sink.counts();
    assert_eq!(unwritable, 0, "every line was writable");
    assert_eq!(
        written as usize,
        lines.len(),
        "the sink's count is the file's line count"
    );

    for line in &lines {
        for field in ["seq", "atMs", "level", "source", "message", "data"] {
            assert!(
                line.get(field).is_some(),
                "a telemetry line carries {field}: {line}"
            );
        }
    }

    // `seq` is a per-process counter and the pump preserves order, so the
    // exported stream is strictly increasing. A repeat would mean a line was
    // delivered twice; the gaps that are allowed are the ones a full queue
    // makes, and the report counts those separately.
    let seqs: Vec<u64> = lines
        .iter()
        .map(|line| line["seq"].as_u64().expect("seq is a number"))
        .collect();
    assert!(
        seqs.windows(2).all(|pair| pair[0] < pair[1]),
        "the exported stream is in order and carries no line twice"
    );

    let said = |needle: &str| {
        lines.iter().any(|line| {
            line["message"]
                .as_str()
                .is_some_and(|message| message.contains(needle))
        })
    };
    assert!(said("case(s) under"), "the run said what it was given");
    assert!(said("replaying"), "the feed said what it was playing");
    assert!(said("was refused"), "a refused fixture is on the stream");
    assert!(said("accepted at"), "the verdict is on the stream");
    assert!(said("entered"), "the entry is on the stream");
    assert!(said("exited"), "the exit is on the stream");

    assert!(report.totals.entered > 0, "there was an entry to report");
}

// ---------------------------------------------------------------------------
// the run as a whole
// ---------------------------------------------------------------------------

#[test]
fn one_corpus_and_one_policy_produce_byte_identical_reports() {
    // Property R1 of the replay specification, over the whole pipeline rather
    // than over one module. Two ledgers, because the ledger is append-only and
    // running the same decisions into one file twice is a duplicate the unique
    // index on `signature` is supposed to refuse.
    let root = TempRoot::new("determinism");
    let corpus = root.corpus();

    let first = play(&corpus, &root.join("a.db"), ENTERING_PROFILE);
    let second = play(&corpus, &root.join("b.db"), ENTERING_PROFILE);

    let left = serde_json::to_string(&first).expect("the report serialises");
    let right = serde_json::to_string(&second).expect("the report serialises");
    assert_eq!(
        left, right,
        "two runs of one corpus produced different reports"
    );

    // Including the identifiers, which is the part that is easy to get wrong:
    // an intent id minted from a clock or a counter would differ here while
    // every number stayed the same.
    let entered: Vec<String> = launches(&first)
        .into_values()
        .filter_map(|launch| launch.execution.as_ref())
        .map(|opened| opened.signature.clone())
        .collect();
    assert!(!entered.is_empty(), "there was a signature to compare");
}

#[test]
fn a_second_run_into_one_ledger_is_refused_rather_than_recorded_twice() {
    // The other half of the property above. The identifiers being a function of
    // the fixture is what makes a repeat detectable, and the ledger refusing it
    // is what stops one decision becoming two positions.
    let root = TempRoot::new("replays");
    let corpus = root.corpus();
    let db = root.join("sts.db");

    let first = play(&corpus, &db, ENTERING_PROFILE);
    assert!(first.totals.entered > 0);
    assert!(
        first.cases.iter().all(|case| case
            .problems
            .iter()
            .all(|p| !p.contains("could not be recorded"))),
        "the first run recorded cleanly"
    );

    let again = play(&corpus, &db, ENTERING_PROFILE);
    assert_eq!(
        again.totals, first.totals,
        "the same corpus reaches the same verdicts whatever is already on the ledger"
    );
    assert!(
        again
            .cases
            .iter()
            .flat_map(|case| case.problems.iter())
            .any(|problem| problem.contains("could not be recorded")),
        "the duplicate entry was refused and said so: {:?}",
        again
            .cases
            .iter()
            .flat_map(|case| case.problems.iter())
            .collect::<Vec<_>>()
    );
    assert!(
        again
            .cases
            .iter()
            .flat_map(|case| case.launches.iter())
            .all(|launch| launch.execution.is_none()),
        "and no second position was opened"
    );
}

#[test]
fn a_run_that_is_stopped_opens_nothing_after_the_stop() {
    // A stopped run does not flatten — flattening is a trade — so an entry
    // taken after the stop would be one nothing was ever going to close.
    let root = TempRoot::new("stopped");
    let corpus = root.corpus();

    let stop = StopFlag::new();
    stop.stop(StopReason::Signalled("SIGINT".to_string()));
    let report = play_until(&corpus, &root.join("sts.db"), ENTERING_PROFILE, &stop);

    assert_eq!(
        report.totals.entered, 0,
        "a run stopped before it started enters nothing"
    );
    assert_eq!(report.open_positions, 0, "and leaves nothing open");
    assert!(report.cases.is_empty(), "and plays no case at all");
    assert!(stop.is_stopped());
    assert_eq!(
        stop.reason(),
        Some(StopReason::Signalled("SIGINT".to_string())),
        "the first reason is the one that is kept"
    );
}

#[test]
fn the_first_reason_for_stopping_is_the_one_that_is_kept() {
    let stop = StopFlag::new();
    stop.stop(StopReason::Signalled("SIGINT".to_string()));
    stop.stop(StopReason::Halted);
    assert_eq!(
        stop.reason(),
        Some(StopReason::Signalled("SIGINT".to_string()))
    );
}

// ---------------------------------------------------------------------------
// the teardown, against a real process and a real signal
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// stage 7 — our own order, priced against the curve the fill comes off
// ---------------------------------------------------------------------------

/// The v1 rule, which is the only profile under which anything in this corpus
/// gets far enough to be asked about our order.
///
/// A launch the shipped rule refuses for `mixed-sizing` never reaches the
/// sandwich check at all — that ordering is the point of `stage 7` and is
/// asserted directly in `the_guard_is_only_ever_asked_about_a_launch_the_rule_liked`.
fn sized(fixtures: &Path, entry_lamports: u64, max_pool_share_bps: u16) -> ScenarioConfig {
    ScenarioConfig {
        fixtures: fixtures.to_path_buf(),
        gate_profile: GateProfile::V1,
        entry_lamports,
        max_pool_share_bps,
        ..ScenarioConfig::default()
    }
}

/// 5% — the number `ingestion::StreamFilters` holds where doctrine's §10 says
/// 150. The disagreement is listed as open in §30 of the replay specification,
/// and these tests are what it costs.
const LOOSE_CAP_BPS: u16 = 500;

#[test]
fn the_shipped_size_is_priced_against_the_curve_and_cleared() {
    // The baseline the next three tests move away from: the guard runs, the
    // curve is read, the exposure is on the record, and the answer is no.
    let root = TempRoot::new("stage7-baseline");
    let corpus = root.corpus();
    let report = play_with(
        ScenarioConfig {
            sandwich_guard: Some(SandwichGuard::WhenQuoted),
            ..sized(
                &corpus,
                1_000_000_000,
                sts_lib::replay::DEFAULT_MAX_POOL_SHARE_BPS,
            )
        },
        &root.join("sts.db"),
    );

    let mint = the_accepted_mint(&report);
    let all = launches(&report);
    let launch = all
        .get(mint.as_str())
        .expect("the accepted launch is on the report");

    assert_eq!(launch.reason, GateReason::Accepted);
    assert!(
        !launch.refused_on_our_order,
        "nothing here was about our order"
    );

    let quoted = launch.quoted_lamports.expect("the gate was shown an order");
    assert_eq!(
        quoted,
        launch.real_sol_lamports * u64::from(sts_lib::replay::DEFAULT_MAX_POOL_SHARE_BPS) / 10_000,
        "the cap bound the order rather than the request"
    );

    let check = launch.sandwich.expect("the curve was read");
    assert_eq!(
        check.gross_lamports, quoted,
        "priced at the size that would be sent"
    );
    assert_eq!(check.virtual_sol_reserves, launch.virtual_sol_lamports);
    assert!(
        !check.above_threshold,
        "1.5% of this pool is under the threshold"
    );
    assert!(quoted < check.breakeven_lamports);

    // And it was actually entered, at the size it was quoted at.
    let execution = launch.execution.as_ref().expect("the position opened");
    assert_eq!(
        execution.size_lamports, quoted,
        "the fill is the size the guard cleared"
    );
}

#[test]
fn the_same_launch_at_a_looser_cap_is_refused_on_its_size() {
    // Nothing about the launch changed. The cap did, and the order it allows is
    // now one the curve says is worth front-running.
    let root = TempRoot::new("stage7-loose");
    let corpus = root.corpus();
    let report = play_with(
        ScenarioConfig {
            sandwich_guard: Some(SandwichGuard::WhenQuoted),
            ..sized(&corpus, 1_000_000_000, LOOSE_CAP_BPS)
        },
        &root.join("sts.db"),
    );

    let baseline = play_with(
        sized(
            &corpus,
            1_000_000_000,
            sts_lib::replay::DEFAULT_MAX_POOL_SHARE_BPS,
        ),
        &root.join("baseline.db"),
    );
    let mint = the_accepted_mint(&baseline);

    let all = launches(&report);
    let launch = all
        .get(mint.as_str())
        .expect("the same launch is on both reports");

    assert_eq!(
        launch.reason,
        GateReason::SandwichRisk,
        "refused, and named for why"
    );
    assert!(!launch.enter);
    assert!(
        launch.refused_on_our_order,
        "and the report says which question it died on"
    );

    let check = launch.sandwich.expect("the curve was read");
    assert!(check.above_threshold);
    assert!(check.refuses(), "a public send at this size is a refusal");
    assert!(
        check.gross_lamports > check.breakeven_lamports,
        "{} is not over {}",
        check.gross_lamports,
        check.breakeven_lamports
    );

    assert!(launch.execution.is_none(), "a refused launch opens nothing");
    assert_eq!(report.totals.entered, 0);
    assert_eq!(report.open_positions, 0);
}

#[test]
fn the_private_route_is_priced_and_reported_and_still_entered() {
    // §15.4. The exposure is the justification for a tip, so it is still
    // computed and still on the record — it just stops being a refusal, because
    // a send nobody can read first is not the one §15.1 prices.
    let root = TempRoot::new("stage7-private");
    let corpus = root.corpus();
    let report = play_with(
        ScenarioConfig {
            sandwich_guard: Some(SandwichGuard::WhenQuoted),
            private_entry: true,
            ..sized(&corpus, 1_000_000_000, LOOSE_CAP_BPS)
        },
        &root.join("sts.db"),
    );

    let mint = the_accepted_mint(&report);
    let all = launches(&report);
    let launch = all
        .get(mint.as_str())
        .expect("the accepted launch is on the report");

    assert_eq!(
        launch.reason,
        GateReason::Accepted,
        "the same order, on a route that is not read"
    );
    assert!(!launch.refused_on_our_order);

    let check = launch.sandwich.expect("the curve was read anyway");
    assert!(
        check.above_threshold,
        "the exposure is not hidden by the route"
    );
    assert!(
        !check.refuses(),
        "it is a number to justify a tip against, not a refusal"
    );
    assert!(check.private_bundle);

    assert!(launch.execution.is_some(), "and the position opened");
    assert_eq!(report.totals.entered, 1);
    assert!(
        report.private_entry,
        "the report says which route it priced"
    );
}

#[test]
fn the_guard_is_only_ever_asked_about_a_launch_the_rule_liked() {
    // The non-conflation property, stated as a comparison rather than a
    // comment. Turning the guard up to `required` and leaving the curve
    // unpriceable moves exactly one verdict: the one launch that had already
    // passed every question about the launch itself. Every other refusal keeps
    // the reason it had.
    let root = TempRoot::new("stage7-ordering");
    let corpus = root.corpus();

    let baseline = play_with(
        sized(&corpus, 1_000_000_000, 150),
        &root.join("baseline.db"),
    );

    // A cap of nothing leaves no order to price anywhere in the corpus, and
    // `required` refuses what it could not price.
    let starved = play_with(
        ScenarioConfig {
            sandwich_guard: Some(SandwichGuard::Required),
            ..sized(&corpus, 1_000_000_000, 0)
        },
        &root.join("starved.db"),
    );

    let accepted = the_accepted_mint(&baseline);
    let before = launches(&baseline);
    let after = launches(&starved);
    assert_eq!(
        before.len(),
        after.len(),
        "the same launches were seen either way"
    );

    for (mint, was) in &before {
        let now = after.get(mint).expect("every launch is on both reports");
        if *mint == accepted {
            assert_eq!(
                now.reason,
                GateReason::NoCurveQuote,
                "the one that got that far"
            );
            assert!(now.refused_on_our_order);
            assert_eq!(now.quoted_lamports, None, "there was no order to price");
            continue;
        }
        assert_eq!(
            now.reason,
            was.reason,
            "{mint} changed its reason from {} to {}, and the guard is not about the launch",
            was.reason.as_str(),
            now.reason.as_str()
        );
        assert!(
            !now.refused_on_our_order,
            "{mint} died on the launch, not on our order"
        );
    }
}

#[test]
fn an_order_the_cap_leaves_no_room_for_is_a_problem_rather_than_a_verdict() {
    // The other half of the separation. Under `when-quoted` the same starved
    // cap is not a refusal at all: the gate still likes the launch, and the
    // failure to size it surfaces as a problem on the case. A plumbing failure
    // recorded as a strategy result would be a funnel that lies about the rule.
    let root = TempRoot::new("stage7-noroom");
    let corpus = root.corpus();
    let report = play_with(
        ScenarioConfig {
            sandwich_guard: Some(SandwichGuard::WhenQuoted),
            ..sized(&corpus, 1_000_000_000, 0)
        },
        &root.join("sts.db"),
    );

    let mint = the_accepted_mint(&report);
    let all = launches(&report);
    let launch = all
        .get(mint.as_str())
        .expect("the accepted launch is on the report");

    assert_eq!(
        launch.reason,
        GateReason::Accepted,
        "the rule still likes the launch"
    );
    assert!(!launch.refused_on_our_order);
    assert_eq!(
        launch.quoted_lamports, None,
        "and there was still nothing to price"
    );
    assert_eq!(
        launch.sandwich, None,
        "an unquoted order has no exposure to report"
    );
    assert!(launch.execution.is_none(), "nothing opened");

    let problems: Vec<&String> = report
        .cases
        .iter()
        .flat_map(|case| case.problems.iter())
        .collect();
    assert!(
        problems
            .iter()
            .any(|p| p.contains(&mint) && p.contains("not a position")),
        "the sizing failure is on the record as a problem: {problems:?}"
    );

    // The funnel counts the verdict, and the verdict was yes. What did not
    // happen is the position — which is the distinction the two numbers exist
    // to keep, and the reason a sizing failure must not be written down as a
    // refusal.
    assert_eq!(report.totals.entered, 1, "the gate said yes");
    assert_eq!(
        report.open_positions, 0,
        "and nothing was opened on the back of it"
    );
    assert_eq!(report.totals.reason_count(GateReason::SandwichRisk), 0);
    assert_eq!(report.totals.reason_count(GateReason::NoCurveQuote), 0);
}

#[test]
fn the_report_carries_the_guard_it_ran_and_two_runs_at_one_policy_agree() {
    // R1 extended to the new policy. Two runs at one guard are byte-identical;
    // two runs at different guards are not, and the report says which was which
    // rather than leaving a reader to infer it from the funnel.
    let root = TempRoot::new("stage7-policy");
    let corpus = root.corpus();

    let required = |db: &str| {
        play_with(
            ScenarioConfig {
                sandwich_guard: Some(SandwichGuard::Required),
                ..sized(&corpus, 1_000_000_000, LOOSE_CAP_BPS)
            },
            &root.join(db),
        )
    };
    let first = required("first.db");
    let second = required("second.db");
    assert_eq!(
        serde_json::to_string(&first).expect("it serialises"),
        serde_json::to_string(&second).expect("it serialises"),
        "one corpus and one policy produce the same bytes"
    );
    assert_eq!(first.sandwich_guard, SandwichGuard::Required);
    assert!(!first.private_entry);

    let off = play_with(
        ScenarioConfig {
            sandwich_guard: Some(SandwichGuard::Off),
            ..sized(&corpus, 1_000_000_000, LOOSE_CAP_BPS)
        },
        &root.join("off.db"),
    );
    assert_eq!(off.sandwich_guard, SandwichGuard::Off);
    assert_ne!(
        serde_json::to_string(&first).expect("it serialises"),
        serde_json::to_string(&off).expect("it serialises"),
        "two guards are two policies"
    );

    // With the guard off the launch is entered at a size the curve says is
    // worth front-running, which is exactly what the guard exists to stop.
    let mint = the_accepted_mint(&off);
    let all = launches(&off);
    let launch = all
        .get(mint.as_str())
        .expect("the accepted launch is on the report");
    assert_eq!(launch.sandwich, None, "a guard that is off reads no curve");
    assert!(launch.execution.is_some());
}

// ---------------------------------------------------------------------------
// stage 8 — many pools at once, and the route the order goes out on
// ---------------------------------------------------------------------------

/// A corpus of `count` pools, one case each, each on its own curve.
///
/// The stock corpus has exactly one launch that gets as far as being asked
/// about our order, which is enough to test the question and not enough to test
/// what happens when it is asked of several curves in one run. Each pool here is
/// the `sybil-rug` scenario at its own seed, so each walks its curve to its own
/// depth and each is judged against reserves nobody else's verdict can reach.
///
/// **These pools share a mint and a stream id, and so they share an entry
/// identity.** Both are literals in the generator rather than functions of the
/// seed, and `SimulatedExecution::enter` mints its intent id from
/// `(stream_id, mint, seq)` — so to the ledger these four cases are one
/// recording played four times, and its unique index on `signature` admits the
/// first and refuses the rest. That is the ledger doing its job; it is asserted
/// directly in `pools_that_share_an_entry_identity_collide_on_the_ledger_and_say_so`.
///
/// The consequence for everything else here is that a *cascade* is tested on
/// verdicts and not on fills. That is the right place for it anyway — the guard
/// is a decision, `execute` is what is done with one, and
/// `a_run_that_opens_nothing_reaches_the_same_verdicts` is the standing proof
/// that the funnel does not depend on the trading — but it is a choice and not
/// an accident, so it is written down.
fn pool_corpus(root: &TempRoot, count: usize) -> PathBuf {
    let dir = root.join("pools");
    std::fs::create_dir_all(&dir).expect("the pool corpus root is creatable");
    for index in 0..count {
        let config = GeneratorConfig {
            seed: format!("0x{}00x", index + 1),
            ..GeneratorConfig::default()
        };
        let cases = fixtures::generate(fixtures::Scenario::SybilRug, &config)
            .expect("the scenario generates");
        for mut case in cases {
            case.name = format!("pool-{index}");
            fixtures::write_case(&dir, &case, true).expect("the case writes");
        }
    }
    dir
}

/// The one launch in each pool case, by case name.
fn by_case(report: &PipelineReport) -> BTreeMap<&str, &sts_lib::daemon::LaunchOutcome> {
    report
        .cases
        .iter()
        .filter_map(|case| {
            let launch = case.launches.first()?;
            assert_eq!(
                case.launches.len(),
                1,
                "{} is one pool, one launch",
                case.case
            );
            Some((case.case.as_str(), launch))
        })
        .collect()
}

/// How many pools the cascade tests run. Four rather than two, so "some refused
/// and some not" is a state the ladder can actually pass through.
const POOLS: usize = 4;

/// A pool run that decides and trades nothing, at one cap and one route.
///
/// Not executing is the point rather than a convenience: a cascade is a
/// statement about what the guard *decided* on four curves, and the pools share
/// an entry identity (see [`pool_corpus`]) so at most one of them could ever
/// reach the ledger. Deciding without trading is the shape that asks the
/// question these tests are asking.
fn deciding(corpus: &Path, cap: u16, guard: SandwichGuard, private: bool) -> ScenarioConfig {
    ScenarioConfig {
        sandwich_guard: Some(guard),
        private_entry: private,
        execute: false,
        ..sized(corpus, 5_000_000_000, cap)
    }
}

#[test]
fn every_pool_in_a_run_is_judged_against_its_own_curve_and_nobody_elses() {
    // The property that makes a multi-pool run mean anything: the guard is
    // arithmetic on one curve, so N launches in one process must produce the
    // answer each of them would have produced alone. A quote priced against the
    // wrong pool's reserves is the bug this is looking for, and on a corpus of
    // one launch it is invisible.
    let root = TempRoot::new("stage8-independence");
    let corpus = pool_corpus(&root, POOLS);
    let report = play_with(
        deciding(&corpus, LOOSE_CAP_BPS, SandwichGuard::WhenQuoted, false),
        &root.join("sts.db"),
    );

    let pools = by_case(&report);
    assert_eq!(pools.len(), POOLS, "every pool reached a verdict");

    // Distinct curves, or the independence this test claims to check is a
    // coincidence rather than a property.
    let depths: BTreeMap<u64, &str> = pools
        .iter()
        .map(|(case, launch)| (launch.real_sol_lamports, *case))
        .collect();
    assert_eq!(
        depths.len(),
        POOLS,
        "two pools walked to the same depth: {depths:?}"
    );

    for (case, launch) in &pools {
        // The quote is this pool's own liquidity at the run's cap, and the
        // threshold is this pool's own price. Recomputed here from the two
        // reserves on the report rather than read off the check, so this is a
        // second reader over the same numbers.
        let quoted = launch.quoted_lamports.expect("{case} was priced");
        assert_eq!(
            quoted,
            launch.real_sol_lamports * u64::from(LOOSE_CAP_BPS) / 10_000,
            "{case} was sized against liquidity that is not its own"
        );
        assert!(
            quoted < 5_000_000_000,
            "{case}: the cap bound the order rather than the request"
        );

        let check = launch
            .sandwich
            .expect("{case} was quoted, so it was checked");
        assert_eq!(
            check.gross_lamports, quoted,
            "{case}: priced at a size it would not send"
        );
        assert_eq!(
            check.virtual_sol_reserves, launch.virtual_sol_lamports,
            "{case}: priced against a reserve that is not its own"
        );
        assert_eq!(
            check.above_threshold,
            sts_lib::backtest::sandwich_viable(quoted, launch.virtual_sol_lamports, report.fee_bps),
            "{case}: the verdict and the arithmetic disagree"
        );
        assert_eq!(
            check.breakeven_lamports,
            sts_lib::replay::sandwich_breakeven_victim_lamports(
                launch.virtual_sol_lamports,
                report.fee_bps
            ),
            "{case}: the break-even is not this pool's"
        );
    }
}

#[test]
fn the_refusals_cascade_pool_by_pool_as_the_cap_widens_and_never_unwind() {
    // What a cascade is, as a property rather than a picture. Widening the cap
    // makes every order bigger and no order smaller, so the set of pools the
    // guard refuses can only grow — a pool that came back from a refusal would
    // mean the comparison is not monotone in size, which is an arithmetic bug
    // and not a rule. At the ends the ladder is total: nothing refused at the
    // bottom, everything refused at the top.
    let root = TempRoot::new("stage8-cascade");
    let corpus = pool_corpus(&root, POOLS);

    let ladder = [150u16, 300, 340, 350, 400, 500, LOOSE_CAP_BPS * 2];
    let mut refused_before: BTreeMap<String, bool> = BTreeMap::new();
    let mut ever_refused = 0usize;

    for cap in ladder {
        let report = play_with(
            deciding(&corpus, cap, SandwichGuard::WhenQuoted, false),
            &root.join(&format!("cap-{cap}.db")),
        );

        let pools = by_case(&report);
        let mut refused_now = 0u32;
        for (case, launch) in &pools {
            let quoted = launch.quoted_lamports.expect("every pool is priceable");
            let farmable = sts_lib::backtest::sandwich_viable(
                quoted,
                launch.virtual_sol_lamports,
                report.fee_bps,
            );

            // Each pool's verdict is what its own reserves say it should be, at
            // every rung — which is the cascade stated per pool rather than as
            // a count.
            if farmable {
                assert_eq!(
                    launch.reason,
                    GateReason::SandwichRisk,
                    "{case} at {cap} bps is over its own threshold and was not refused"
                );
                assert!(launch.refused_on_our_order, "{case} at {cap} bps");
                refused_now += 1;
            } else {
                assert_eq!(
                    launch.reason,
                    GateReason::Accepted,
                    "{case} at {cap} bps is under its own threshold and was refused anyway"
                );
                assert!(!launch.refused_on_our_order, "{case} at {cap} bps");
            }

            // Monotone, per pool. This is the half a count cannot see: two
            // pools swapping places would keep the total still.
            if let Some(&was) = refused_before.get(*case) {
                assert!(
                    farmable || !was,
                    "{case} was refused below {cap} bps and is not at it, so the guard is not monotone in size"
                );
            }
            refused_before.insert((*case).to_string(), farmable);
        }

        ever_refused = ever_refused.max(refused_now as usize);
        assert_eq!(
            report.totals.reason_count(GateReason::SandwichRisk),
            refused_now,
            "the funnel at {cap} bps counts a different number of refusals than the cases carry"
        );
        assert_eq!(
            report.totals.entered,
            (POOLS as u32) - refused_now,
            "at {cap} bps the launches that were not refused are the ones that entered"
        );

        if cap == ladder[0] {
            assert_eq!(refused_now, 0, "the bottom of the ladder refuses nothing");
        }
        if cap == ladder[ladder.len() - 1] {
            assert_eq!(
                refused_now, POOLS as u32,
                "the top of the ladder refuses everything"
            );
        }
    }

    assert_eq!(
        ever_refused, POOLS,
        "the ladder never reached a full cascade"
    );
}

#[test]
fn a_pool_refused_on_our_order_leaves_the_pools_beside_it_alone() {
    // The other half of a cascade, and the one that matters when it is partial:
    // a refusal is a fact about one curve, so every other pool in the run has to
    // reach the verdict it would have reached alone. A guard that failed a whole
    // run on one pool's size would be a guard nobody could leave on.
    let root = TempRoot::new("stage8-partial");
    let corpus = pool_corpus(&root, POOLS);

    // The cap where the pools disagree. Found rather than asserted: the depths
    // are a property of the generator, and a test that hard-coded the crossing
    // would fail the day a fixture gains one more buyer.
    let split = (300u16..=500)
        .find(|cap| {
            let report = play_with(
                deciding(&corpus, *cap, SandwichGuard::WhenQuoted, false),
                &root.join(&format!("probe-{cap}.db")),
            );
            let refused = report.totals.reason_count(GateReason::SandwichRisk);
            refused > 0 && refused < POOLS as u32
        })
        .expect("somewhere between 3% and 5% the pools disagree about their own size");

    let report = play_with(
        deciding(&corpus, split, SandwichGuard::WhenQuoted, false),
        &root.join("split.db"),
    );
    let pools = by_case(&report);

    let refused: Vec<&str> = pools
        .iter()
        .filter(|(_, launch)| launch.reason == GateReason::SandwichRisk)
        .map(|(case, _)| *case)
        .collect();
    let accepted: Vec<&str> = pools
        .iter()
        .filter(|(_, launch)| launch.reason == GateReason::Accepted)
        .map(|(case, _)| *case)
        .collect();

    assert!(
        !refused.is_empty() && !accepted.is_empty(),
        "the split cap split nothing"
    );
    assert_eq!(
        refused.len() + accepted.len(),
        POOLS,
        "a pool reached neither answer, so something other than the guard decided it"
    );

    // The pools that cleared are the ones whose own curve says they should
    // have, and they cleared while their neighbours were being refused. Every
    // launch-quality column is untouched on both sides — the guard is the last
    // check and it is not allowed to reach back.
    for (case, launch) in &pools {
        let quoted = launch
            .quoted_lamports
            .expect("every pool is priceable at this cap");
        let farmable =
            sts_lib::backtest::sandwich_viable(quoted, launch.virtual_sol_lamports, report.fee_bps);
        assert_eq!(
            farmable,
            refused.contains(case),
            "{case} was decided by something else"
        );
        assert_eq!(
            launch.refused_on_our_order,
            refused.contains(case),
            "{case}: the report disagrees about which question it died on"
        );
        assert!(
            launch.window_closed,
            "{case} was decided on a window the recording cut short"
        );
        assert!(
            launch.buyers > 0,
            "{case} reached the guard with no buyers behind it"
        );
    }

    // And the funnel is the two sets, with nothing else in it. Every pool in
    // this corpus passes every launch-quality check, so a third reason
    // appearing would mean the guard had changed one of them.
    assert_eq!(
        report.totals.reason_count(GateReason::SandwichRisk),
        refused.len() as u32
    );
    assert_eq!(report.totals.entered, accepted.len() as u32);
    assert_eq!(
        report.totals.reason_count(GateReason::Accepted),
        accepted.len() as u32
    );
    for reason in [
        GateReason::NoCurveQuote,
        GateReason::CoordinatedRing,
        GateReason::Thin,
        GateReason::LowScore,
    ] {
        assert_eq!(
            report.totals.reason_count(reason),
            0,
            "a partial cascade moved {} onto the funnel",
            reason.as_str()
        );
    }
}

#[test]
fn pools_that_share_an_entry_identity_collide_on_the_ledger_and_say_so() {
    // What the pool corpus costs, pinned rather than worked around. These four
    // cases are one recording at four seeds: four curves and four verdicts, but
    // one mint and one stream id, and `SimulatedExecution::enter` mints its
    // intent id from those two and the record number. So the ledger's unique
    // index on `signature` takes the first and refuses the rest.
    //
    // The reason this is worth a test rather than a comment is what the daemon
    // does with the refusal: it is a problem on the case and the verdict stays
    // what it was. A plumbing failure written down as a strategy result would be
    // a funnel that lies about the rule, and that rule has to hold for a
    // duplicate identity exactly as it does for a cap that leaves no room.
    let root = TempRoot::new("stage8-identity");
    let corpus = pool_corpus(&root, POOLS);
    let report = play_with(
        ScenarioConfig {
            sandwich_guard: Some(SandwichGuard::WhenQuoted),
            ..sized(
                &corpus,
                5_000_000_000,
                sts_lib::replay::DEFAULT_MAX_POOL_SHARE_BPS,
            )
        },
        &root.join("sts.db"),
    );

    let pools = by_case(&report);
    assert_eq!(pools.len(), POOLS);

    let opened: Vec<&str> = pools
        .iter()
        .filter(|(_, launch)| launch.execution.is_some())
        .map(|(case, _)| *case)
        .collect();
    assert_eq!(opened.len(), 1, "one identity, one position: {opened:?}");

    // Every pool was accepted, and the funnel counts verdicts.
    for (case, launch) in &pools {
        assert_eq!(launch.reason, GateReason::Accepted, "{case}");
        assert!(!launch.refused_on_our_order, "{case}");
    }
    assert_eq!(
        report.totals.entered, POOLS as u32,
        "the gate said yes four times"
    );
    assert_eq!(
        report.totals.reason_count(GateReason::SandwichRisk),
        0,
        "a duplicate identity is not a size verdict"
    );

    // The three that could not be recorded are on the record as problems, by
    // name and by what went wrong.
    let problems: Vec<&String> = report
        .cases
        .iter()
        .flat_map(|case| case.problems.iter())
        .collect();
    assert_eq!(
        problems.len(),
        POOLS - 1,
        "one problem per pool that could not be opened: {problems:?}"
    );
    assert!(
        problems.iter().all(|p| p.contains("already on the ledger")),
        "the refusal says what it was: {problems:?}"
    );

    // And the ledger holds exactly the one entry, so nothing was recorded twice.
    // Restricted to the buy leg: the position that did open was then flattened,
    // and an exit is its own intent under an id of its own.
    let conn = rusqlite::Connection::open(root.join("sts.db")).expect("sts.db reopens");
    let bought = rows(
        &conn,
        "SELECT DISTINCT intent_id FROM execution_logs WHERE side = 'buy'",
    );
    assert_eq!(bought.len(), 1, "the ledger holds one entry: {bought:?}");
    assert_eq!(
        bought[0][0], "sybil-rug-mint-sybil-rug-000015",
        "and it is the identity all four pools were minting"
    );
}

#[test]
fn the_private_route_carries_the_whole_cascade_through() {
    // §15.4 across every pool at once. The route does not change an order's
    // size and does not hide its exposure — it changes whether the exposure is
    // a refusal. At a cap where the public run refuses all four, the private run
    // accepts all four and still reports all four exposures.
    let root = TempRoot::new("stage8-private");
    let corpus = pool_corpus(&root, POOLS);
    let wide = LOOSE_CAP_BPS * 2;

    let public = play_with(
        deciding(&corpus, wide, SandwichGuard::WhenQuoted, false),
        &root.join("public.db"),
    );
    let private = play_with(
        deciding(&corpus, wide, SandwichGuard::WhenQuoted, true),
        &root.join("private.db"),
    );

    assert_eq!(
        public.totals.reason_count(GateReason::SandwichRisk),
        POOLS as u32
    );
    assert_eq!(
        public.totals.entered, 0,
        "every public order at this cap is farmable"
    );
    assert_eq!(private.totals.reason_count(GateReason::SandwichRisk), 0);
    assert_eq!(
        private.totals.entered, POOLS as u32,
        "and none of them is a refusal on a bundle"
    );
    assert!(
        private.private_entry,
        "the report says which route it priced"
    );
    assert!(!public.private_entry);

    let before = by_case(&public);
    let after = by_case(&private);
    assert_eq!(before.len(), POOLS);

    for (case, was) in &before {
        let now = after.get(case).expect("every pool is on both reports");

        // Same order, same curve, same exposure. The one thing that moved is the
        // verdict, which is the whole claim §15.1 makes about a send nobody
        // reads first.
        assert_eq!(
            now.quoted_lamports, was.quoted_lamports,
            "{case}: the route resized the order"
        );
        assert_eq!(now.real_sol_lamports, was.real_sol_lamports, "{case}");
        assert_eq!(now.virtual_sol_lamports, was.virtual_sol_lamports, "{case}");
        assert_eq!(
            now.confidence_micros, was.confidence_micros,
            "{case}: the route moved the score"
        );
        assert_eq!(now.tags, was.tags, "{case}: the route moved the risk tags");

        let public_check = was.sandwich.unwrap_or_else(|| panic!("{case} was checked"));
        let private_check = now
            .sandwich
            .unwrap_or_else(|| panic!("{case} is checked on a bundle too"));
        assert_eq!(
            private_check.gross_lamports, public_check.gross_lamports,
            "{case}"
        );
        assert_eq!(
            private_check.beta_micros, public_check.beta_micros,
            "{case}"
        );
        assert_eq!(
            private_check.beta_threshold_micros, public_check.beta_threshold_micros,
            "{case}"
        );
        assert_eq!(
            private_check.breakeven_lamports, public_check.breakeven_lamports,
            "{case}"
        );
        assert!(
            private_check.above_threshold,
            "{case}: the route hid the exposure"
        );
        assert!(
            !private_check.refuses(),
            "{case}: a bundle is not refused on it"
        );
        assert!(private_check.private_bundle, "{case}");
        assert!(public_check.refuses(), "{case}");
        assert!(
            was.refused_on_our_order,
            "{case}: the public run died on its size"
        );
        assert!(!now.refused_on_our_order, "{case}: the private run did not");
    }
}

#[test]
fn a_private_route_is_not_a_licence_to_skip_the_curve() {
    // The one thing `--private-entry` must not do. `required` says a curve
    // nobody read is not a curve found to be safe, and that is a statement about
    // whether we looked — not about what we would have seen or which route the
    // order was going out on. A private send with no quote is still refused, and
    // still refused for the reason that says so.
    let root = TempRoot::new("stage8-private-required");
    let corpus = pool_corpus(&root, POOLS);
    let report = play_with(
        deciding(&corpus, 0, SandwichGuard::Required, true),
        &root.join("sts.db"),
    );

    let pools = by_case(&report);
    assert_eq!(pools.len(), POOLS);
    for (case, launch) in &pools {
        assert_eq!(
            launch.reason,
            GateReason::NoCurveQuote,
            "{case} was let through unpriced"
        );
        assert!(launch.refused_on_our_order, "{case}");
        assert_eq!(
            launch.quoted_lamports, None,
            "{case}: a cap of nothing leaves nothing to price"
        );
        assert_eq!(
            launch.sandwich, None,
            "{case}: an unquoted order has no exposure"
        );
        assert!(launch.execution.is_none(), "{case}");
    }
    assert_eq!(
        report.totals.reason_count(GateReason::NoCurveQuote),
        POOLS as u32
    );
    assert_eq!(report.totals.entered, 0);
    assert!(report.private_entry, "and the route is still on the report");

    // Under `when-quoted` the same unpriceable run is not a refusal at all, on
    // any route. The two settings differ by exactly this, and the private flag
    // does not move the line between them.
    let lenient = play_with(
        deciding(&corpus, 0, SandwichGuard::WhenQuoted, true),
        &root.join("lenient.db"),
    );
    assert_eq!(lenient.totals.reason_count(GateReason::NoCurveQuote), 0);
    assert_eq!(
        lenient.totals.entered, POOLS as u32,
        "the gate still likes every launch"
    );
}

#[test]
fn the_route_is_policy_and_two_runs_at_one_route_agree_to_the_byte() {
    // R1 over the flag. The route decides verdicts, so it belongs on the report
    // beside the guard, and two runs that differ only in it must not be able to
    // claim the same policy.
    let root = TempRoot::new("stage8-policy");
    let corpus = pool_corpus(&root, POOLS);
    let at = |private: bool, db: &str| {
        play_with(
            deciding(
                &corpus,
                LOOSE_CAP_BPS * 2,
                SandwichGuard::WhenQuoted,
                private,
            ),
            &root.join(db),
        )
    };

    let first = serde_json::to_string(&at(true, "first.db")).expect("it serialises");
    let second = serde_json::to_string(&at(true, "second.db")).expect("it serialises");
    assert_eq!(
        first, second,
        "one corpus and one route produce the same bytes"
    );

    let public = serde_json::to_string(&at(false, "public.db")).expect("it serialises");
    assert_ne!(first, public, "two routes are two policies");
}

#[test]
fn the_whole_report_is_readable_with_one_naming_rule() {
    // `sts.daemon.report.v1` is a document an operator reads and a funnel
    // parses, and every key on it is camel case — except that `SandwichCheck`
    // comes from the strategy module, whose structs serialise in snake case,
    // and it is embedded whole. A report carrying `quotedLamports` beside
    // `above_threshold` is one nobody can read with a single rule, so the one
    // type that crosses that boundary is renamed and this is what says so.
    //
    // Walked over every key at every depth rather than asserted on the field
    // that prompted it: the next type embedded from another module would
    // otherwise reintroduce it silently.
    let root = TempRoot::new("stage8-casing");
    let corpus = pool_corpus(&root, POOLS);
    let report = play_with(
        deciding(&corpus, LOOSE_CAP_BPS * 2, SandwichGuard::WhenQuoted, false),
        &root.join("sts.db"),
    );

    let json = serde_json::to_value(&report).expect("the report serialises");

    fn keys(value: &serde_json::Value, path: &str, found: &mut Vec<(String, String)>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, child) in map {
                    found.push((format!("{path}.{key}"), key.clone()));
                    keys(child, &format!("{path}.{key}"), found);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    keys(item, &format!("{path}[]"), found);
                }
            }
            _ => {}
        }
    }

    let mut found = Vec::new();
    keys(&json, "report", &mut found);
    assert!(
        found.len() > 40,
        "the report has fewer keys than a report should: {}",
        found.len()
    );

    for (path, key) in &found {
        assert!(
            !key.contains('_'),
            "{path} is snake case, and the rest of the report is not"
        );
        assert!(
            !key.chars().next().is_some_and(|c| c.is_ascii_uppercase()),
            "{path} is not camel case either"
        );
    }

    // And the field that prompted it is there under the name it should have.
    let checked = json["cases"]
        .as_array()
        .expect("cases is a list")
        .iter()
        .flat_map(|case| case["launches"].as_array().expect("launches is a list"))
        .filter(|launch| launch["sandwich"]["aboveThreshold"] == true)
        .count();
    assert_eq!(
        checked, POOLS,
        "every pool's exposure is on the report, camel cased"
    );
}

#[test]
fn the_v1_profile_still_ships_with_the_guard_off() {
    // The override is a separate field precisely so that comparing the two
    // rules does not silently compare two guards as well.
    let root = TempRoot::new("stage7-profiles");
    let corpus = root.corpus();

    let v1 = play_with(
        sized(&corpus, 1_000_000_000, LOOSE_CAP_BPS),
        &root.join("v1.db"),
    );
    assert_eq!(
        v1.sandwich_guard,
        SandwichGuard::Off,
        "v1 shipped before the guard existed"
    );

    let shipped = play_with(
        ScenarioConfig {
            gate_profile: GateProfile::Default,
            ..sized(&corpus, 1_000_000_000, LOOSE_CAP_BPS)
        },
        &root.join("default.db"),
    );
    assert_eq!(
        shipped.sandwich_guard,
        SandwichGuard::WhenQuoted,
        "the shipped rule reads it"
    );
}

/// SIGINT and SIGTERM, sent to the binary this suite was built beside.
///
/// In a child process rather than in this one, and with `kill(1)` rather than a
/// library call, because the thing under test is what the operating system does
/// to `sts` and not what a function does to a flag. A signal handler that is
/// only ever reached by a test is a signal handler that has never been tested.
///
/// Unix only, and that is what `kill(1)` means rather than a preference: the
/// Windows build has no SIGTERM to send and `daemon::watch_for_signals` there
/// listens for Ctrl-C instead, which is not a thing one process sends another.
#[cfg(unix)]
mod signals {
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// How long a test waits for the daemon to reach a state before giving up.
    /// Generous, so a loaded machine fails this suite for a real reason or not
    /// at all.
    const PATIENCE: Duration = Duration::from_secs(30);

    fn daemon(args: &[&str]) -> Child {
        Command::new(env!("CARGO_BIN_EXE_sts"))
            .arg("daemon")
            .arg("run")
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the daemon starts")
    }

    fn send(child: &Child, name: &str) {
        let status = Command::new("kill")
            .arg(format!("-{name}"))
            .arg(child.id().to_string())
            .status()
            .expect("kill runs");
        assert!(status.success(), "{name} was delivered to {}", child.id());
    }

    /// Waits until the telemetry stream says the daemon has got somewhere.
    ///
    /// Reading the daemon's own report of where it is, rather than sleeping a
    /// guessed number of seconds, is what keeps these tests from being a race
    /// against a build machine's load average.
    fn wait_for(stream: &Path, needle: &str) {
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if let Ok(text) = std::fs::read_to_string(stream) {
                if text.contains(needle) {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let seen = std::fs::read_to_string(stream).unwrap_or_default();
        panic!("the daemon never said {needle:?}; it said:\n{seen}");
    }

    fn finish(child: Child) -> (i32, String) {
        let output = child.wait_with_output().expect("the daemon exits");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn report(path: &Path) -> serde_json::Value {
        let text =
            std::fs::read_to_string(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()));
        serde_json::from_str(&text).expect("the report is JSON")
    }

    #[test]
    fn sigint_stops_a_playing_daemon_between_records_and_writes_its_report() {
        let root = TempRoot::new("sigint");
        let corpus = root.corpus();
        let stream = root.join("telemetry.ndjson");
        let out = root.join("report.json");
        let db = root.join("sts.db");

        // At `1x` the corpus plays at the rate it was recorded at, which is the
        // only reason there is a middle to interrupt.
        let child = daemon(&[
            "--fixtures",
            corpus.to_str().expect("a utf-8 path"),
            "--db",
            db.to_str().expect("a utf-8 path"),
            "--telemetry",
            stream.to_str().expect("a utf-8 path"),
            "--out",
            out.to_str().expect("a utf-8 path"),
            "--speed",
            "1",
            "--gate-profile",
            "v1",
        ]);

        wait_for(&stream, "replaying");
        send(&child, "INT");
        let (code, stderr) = finish(child);
        assert_eq!(
            code, 0,
            "a clean teardown is a successful exit; stderr:\n{stderr}"
        );

        let report = report(&out);
        assert_eq!(
            report["process"]["stoppedBy"]["stop"], "signalled",
            "the report says what stopped it"
        );
        assert_eq!(report["process"]["stoppedBy"]["detail"], "SIGINT");
        assert_eq!(
            report["process"]["signerLive"], false,
            "nothing in the run could reach a network"
        );
        assert_eq!(
            report["pipeline"]["openPositions"], 0,
            "the run left no position open"
        );

        // Interrupted part way through a fixture, not between two of them. A
        // run that only ever stopped on a case boundary would leave the
        // between-records path untested.
        let cases = report["pipeline"]["cases"]
            .as_array()
            .expect("the report lists cases");
        assert!(
            cases.iter().any(|case| {
                case["problems"].as_array().is_some_and(|problems| {
                    problems.iter().any(|p| {
                        p.as_str()
                            .is_some_and(|text| text.contains("stopped before the fixture ran out"))
                    })
                })
            }),
            "one case was cut short and says so: {cases:#?}"
        );

        // The teardown is on the exported stream, which means the pump was
        // joined rather than dropped mid-flight.
        let telemetry = std::fs::read_to_string(&stream).expect("the stream was written");
        assert!(
            telemetry.contains("stopping at the next record"),
            "the signal is on the stream"
        );
        assert!(
            telemetry.contains("shutting down"),
            "the shutdown reached the stream before the process left"
        );

        // And the ledger survived it. `finish_shutdown` checkpoints last so a
        // final row from anywhere still lands; a file that will not verify here
        // is a teardown that raced its own database.
        let conn = rusqlite::Connection::open(&db).expect("sts.db reopens");
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .expect("the check runs");
        assert_eq!(
            integrity, "ok",
            "the ledger came through the teardown intact"
        );
    }

    #[test]
    fn sigterm_stops_a_daemon_that_is_only_waiting() {
        // No fixtures, so nothing ends on its own and the only way out is a
        // signal. This is the shape a daemon watching live provider feeds has.
        let root = TempRoot::new("sigterm");
        let stream = root.join("telemetry.ndjson");
        let out = root.join("report.json");

        let child = daemon(&[
            "--db",
            root.join("sts.db").to_str().expect("a utf-8 path"),
            "--telemetry",
            stream.to_str().expect("a utf-8 path"),
            "--out",
            out.to_str().expect("a utf-8 path"),
        ]);

        wait_for(&stream, "waiting for SIGINT or SIGTERM");
        send(&child, "TERM");
        let (code, stderr) = finish(child);
        assert_eq!(
            code, 0,
            "a clean teardown is a successful exit; stderr:\n{stderr}"
        );

        let report = report(&out);
        assert_eq!(report["process"]["stoppedBy"]["stop"], "signalled");
        assert_eq!(report["process"]["stoppedBy"]["detail"], "SIGTERM");
        assert_eq!(
            report["pipeline"]["cases"]
                .as_array()
                .expect("cases is a list")
                .len(),
            0,
            "a daemon given nothing to play plays nothing"
        );
    }

    #[test]
    fn a_second_signal_leaves_without_waiting() {
        // Somebody who signals twice has said they are not waiting for the
        // teardown, and a daemon that swallows the second one is a daemon that
        // has to be found and killed by hand. The second is sent only after the
        // stream shows the first was taken, so this is a sequence rather than a
        // race.
        let root = TempRoot::new("impatient");
        let corpus = root.corpus();
        let stream = root.join("telemetry.ndjson");

        let child = daemon(&[
            "--fixtures",
            corpus.to_str().expect("a utf-8 path"),
            "--db",
            root.join("sts.db").to_str().expect("a utf-8 path"),
            "--telemetry",
            stream.to_str().expect("a utf-8 path"),
            "--out",
            root.join("report.json").to_str().expect("a utf-8 path"),
            "--speed",
            "1",
        ]);

        wait_for(&stream, "replaying");
        send(&child, "INT");
        wait_for(&stream, "stopping at the next record");
        send(&child, "INT");

        let (code, _) = finish(child);
        assert_eq!(code, 130, "128 + SIGINT, which is what a shell reports");
    }

    /// `--telemetry -`, which points the sink at the stream this process is
    /// already using to talk.
    ///
    /// A regression test for a deadlock rather than a feature test. `main` used
    /// to hand the subcommand `stderr().lock()` and hold it for the whole run.
    /// That lock is per process: the pump is a second thread, so it blocked on
    /// the first line it tried to write, the queue behind it filled and began
    /// dropping, and the teardown that joins the pump waited on a thread that
    /// was waiting on the main one. The symptom was total — not one byte on the
    /// stream, and a daemon no signal could stop.
    ///
    /// Hence the two things asserted: a line arrives, and the process leaves.
    /// The exit is waited for against a deadline rather than with `wait`,
    /// because the failure being guarded against is a hang, and a test that
    /// hangs instead of failing is not a guard.
    #[test]
    fn telemetry_on_stderr_streams_and_still_stops() {
        use std::io::Read;

        let root = TempRoot::new("stderr-telemetry");
        let out = root.join("report.json");

        let mut child = daemon(&[
            "--db",
            root.join("sts.db").to_str().expect("a utf-8 path"),
            "--telemetry",
            "-",
            "--out",
            out.to_str().expect("a utf-8 path"),
        ]);

        // Drained on a thread of its own. The pipe holds a page or so, and a
        // sink that filled it would stall for a reason that is not the bug.
        let stream = Arc::new(std::sync::Mutex::new(String::new()));
        let mut pipe = child.stderr.take().expect("stderr is piped");
        let reader = std::thread::spawn({
            let stream = Arc::clone(&stream);
            move || {
                let mut buffer = [0u8; 4096];
                while let Ok(read) = pipe.read(&mut buffer) {
                    if read == 0 {
                        break;
                    }
                    stream
                        .lock()
                        .expect("the buffer is not poisoned")
                        .push_str(&String::from_utf8_lossy(&buffer[..read]));
                }
            }
        });

        // The same line the file-backed sink writes, so this is one stream
        // reaching a second target and not a weaker check of a different thing.
        let deadline = Instant::now() + PATIENCE;
        loop {
            let seen = stream.lock().expect("the buffer is not poisoned").clone();
            if seen.contains("headless engine up") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "no telemetry reached stderr in {PATIENCE:?}, so the sink is wedged; seen:\n{seen}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }

        send(&child, "TERM");

        let deadline = Instant::now() + PATIENCE;
        let code = loop {
            match child.try_wait().expect("the daemon is waitable") {
                Some(status) => break status.code().unwrap_or(-1),
                None if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                None => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("SIGTERM did not stop a daemon streaming telemetry to stderr");
                }
            }
        };
        let _ = reader.join();
        assert_eq!(code, 0, "a clean teardown is a successful exit");

        let text = stream.lock().expect("the buffer is not poisoned").clone();
        assert!(
            text.contains("shutting down"),
            "the teardown reached the stream too:\n{text}"
        );

        // Every line is a whole line. Not shredding one write into another is
        // what the lock was for, and taking it per write is what replaced it.
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|err| panic!("{err}: a telemetry line came out torn: {line}"));
        }

        // And the report still arrived, so releasing the locks cost the other
        // stream nothing.
        assert_eq!(report(&out)["process"]["stoppedBy"]["detail"], "SIGTERM");
    }

    /// Every line of an exported stream, refused if any of them is not whole.
    ///
    /// The point of reading it this way is the last line. A sink that buffers,
    /// or a teardown that leaves before the pump has drained, loses exactly the
    /// part somebody reads an audit trail for — and it loses it as a truncated
    /// final object, which `serde_json` is the right thing to notice.
    fn whole_lines(stream: &Path) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(stream).expect("the stream was written");
        assert!(!text.is_empty(), "the stream is not empty");
        assert!(
            text.ends_with('\n'),
            "the stream ends mid-line: {:?}",
            text.split('\n').next_back()
        );
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .enumerate()
            .map(|(number, line)| {
                serde_json::from_str(line).unwrap_or_else(|err| {
                    panic!(
                        "line {} of the stream is not one JSON object: {err}\n{line}",
                        number + 1
                    )
                })
            })
            .collect()
    }

    /// The checks that are about the file rather than about the run.
    fn stream_is_intact(stream: &Path, report: &serde_json::Value) {
        let lines = whole_lines(stream);

        let seqs: Vec<u64> = lines
            .iter()
            .map(|line| line["seq"].as_u64().expect("seq is a number"))
            .collect();
        assert!(
            seqs.windows(2).all(|pair| pair[0] < pair[1]),
            "the stream is out of order or carries a line twice"
        );
        for line in &lines {
            for field in ["seq", "atMs", "level", "source", "message", "data"] {
                assert!(
                    line.get(field).is_some(),
                    "a line is missing {field}: {line}"
                );
            }
        }

        // The count on the report is the count in the file. This is the
        // assertion that says the pump was joined rather than dropped: a
        // teardown that left early would write fewer lines than it claimed.
        assert_eq!(
            report["process"]["telemetryExported"]
                .as_u64()
                .expect("a count"),
            lines.len() as u64,
            "the report and the file disagree about how many lines were written"
        );
        assert_eq!(
            report["process"]["telemetryDropped"], 0,
            "the export fell behind the engine"
        );

        // The last thing said is the teardown, not a verdict that happened to be
        // in flight when the signal landed.
        let said = |needle: &str| {
            lines
                .iter()
                .any(|line| line["message"].as_str().is_some_and(|m| m.contains(needle)))
        };
        assert!(
            said("stopping at the next record"),
            "the signal is on the stream"
        );
        assert!(
            said("shutting down"),
            "the shutdown reached the stream before the process left"
        );
    }

    #[test]
    fn sigterm_stops_a_playing_daemon_and_leaves_the_telemetry_file_whole() {
        // SIGINT mid-play is covered above. This is the other signal, on the
        // same path, and it is not a copy: `watch_for_signals` selects over two
        // separate `tokio::signal::unix` streams, so one of them working says
        // nothing about the other. What is added on top is the file — the
        // stream is being appended to from the engine's threads at the moment
        // the signal lands, which is the case where a teardown that races its
        // own sink shows up as a half-written last line.
        let root = TempRoot::new("sigterm-playing");
        let corpus = root.corpus();
        let stream = root.join("telemetry.ndjson");
        let out = root.join("report.json");
        let db = root.join("sts.db");

        let child = daemon(&[
            "--fixtures",
            corpus.to_str().expect("a utf-8 path"),
            "--db",
            db.to_str().expect("a utf-8 path"),
            "--telemetry",
            stream.to_str().expect("a utf-8 path"),
            "--out",
            out.to_str().expect("a utf-8 path"),
            "--speed",
            "1",
            "--gate-profile",
            "v1",
        ]);

        wait_for(&stream, "replaying");
        send(&child, "TERM");
        let (code, stderr) = finish(child);
        assert_eq!(
            code, 0,
            "a clean teardown is a successful exit; stderr:\n{stderr}"
        );

        let report = report(&out);
        assert_eq!(report["process"]["stoppedBy"]["stop"], "signalled");
        assert_eq!(report["process"]["stoppedBy"]["detail"], "SIGTERM");
        assert_eq!(
            report["pipeline"]["openPositions"], 0,
            "a signal sells nothing and opens nothing"
        );
        assert_eq!(report["process"]["signerLive"], false);

        stream_is_intact(&stream, &report);

        // Cut off part way through a recording rather than between two of them,
        // which is what makes this a test of the between-records path.
        let cases = report["pipeline"]["cases"]
            .as_array()
            .expect("the report lists cases");
        assert!(
            cases.iter().any(|case| {
                case["problems"].as_array().is_some_and(|problems| {
                    problems.iter().any(|p| {
                        p.as_str()
                            .is_some_and(|t| t.contains("stopped before the fixture ran out"))
                    })
                })
            }),
            "one case was cut short and says so: {cases:#?}"
        );

        let conn = rusqlite::Connection::open(&db).expect("sts.db reopens");
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .expect("the check runs");
        assert_eq!(
            integrity, "ok",
            "the ledger came through the teardown intact"
        );
    }

    #[test]
    fn a_signal_through_a_guarded_multi_pool_run_stops_it_clean() {
        // The two halves of this change in one process: four pools being
        // quoted, checked and decided on while a signal arrives, with the
        // telemetry file open and being appended to throughout. The guard reads
        // a curve on every decision, so this is the shape where a teardown that
        // interrupted the entry path mid-quote would show — as a torn line, a
        // missing verdict, or a position nobody closed.
        let root = TempRoot::new("sigint-pools");
        let corpus = pool_corpus(&root, POOLS);
        let stream = root.join("telemetry.ndjson");
        let out = root.join("report.json");
        let db = root.join("sts.db");

        let child = daemon(&[
            "--fixtures",
            corpus.to_str().expect("a utf-8 path"),
            "--db",
            db.to_str().expect("a utf-8 path"),
            "--telemetry",
            stream.to_str().expect("a utf-8 path"),
            "--out",
            out.to_str().expect("a utf-8 path"),
            "--speed",
            "1",
            "--gate-profile",
            "v1",
            "--sandwich-guard",
            "required",
            "--entry-lamports",
            "5000000000",
            "--max-pool-share-bps",
            "1000",
        ]);

        // Sequenced on the stream rather than raced against it: the signal goes
        // after a verdict has been reached, so the run is interrupted with the
        // entry path already exercised rather than while it is still opening the
        // first file. `decide_due` says this line for every launch it reads.
        wait_for(&stream, "replaying");
        wait_for(&stream, "millionths over");
        send(&child, "INT");
        let (code, stderr) = finish(child);
        assert_eq!(
            code, 0,
            "a clean teardown is a successful exit; stderr:\n{stderr}"
        );

        let report = report(&out);
        assert_eq!(report["process"]["stoppedBy"]["detail"], "SIGINT");
        assert_eq!(
            report["pipeline"]["sandwichGuard"], "required",
            "the run that was interrupted is the run that was asked for"
        );
        assert_eq!(
            report["pipeline"]["maxPoolShareBps"], 1000,
            "and at the cap it was given"
        );
        assert_eq!(
            report["pipeline"]["openPositions"], 0,
            "a signal leaves no position open, however many pools were in flight"
        );

        stream_is_intact(&stream, &report);

        // Every launch that was decided before the signal carries a complete
        // verdict — the reserves it was priced against and, unless the cap left
        // nothing to price, the exposure. A decision half-written by a teardown
        // would show up here as a launch with a verdict and no curve behind it.
        let cases = report["pipeline"]["cases"]
            .as_array()
            .expect("the report lists cases");
        let mut decided = 0;
        for case in cases {
            for launch in case["launches"].as_array().expect("launches is a list") {
                decided += 1;
                assert!(launch["realSolLamports"].is_u64(), "{launch}");
                assert!(launch["virtualSolLamports"].is_u64(), "{launch}");
                assert!(launch["reason"].is_string(), "{launch}");
                let quoted = launch["quotedLamports"].as_u64();
                let checked = launch["sandwich"].is_object();
                assert_eq!(
                    quoted.is_some(),
                    checked,
                    "a launch was quoted and not checked, or checked and not quoted: {launch}"
                );
                if launch["reason"] == "sandwich-risk" || launch["reason"] == "no-curve-quote" {
                    assert_eq!(launch["refusedOnOurOrder"], true, "{launch}");
                }
            }
        }
        assert!(
            decided > 0,
            "the signal arrived before anything was decided"
        );
    }

    #[test]
    fn a_run_under_the_guard_still_stops_cleanly_on_a_signal() {
        // The guard decides whether a launch is entered. It has nothing to do
        // with how a run ends, and this is here so that stays true: the same
        // clean teardown, the same exit code, the same report, with the curve
        // being read on every decision along the way.
        let root = TempRoot::new("sigint-guarded");
        let corpus = root.corpus();
        let stream = root.join("telemetry.ndjson");
        let out = root.join("report.json");

        let child = daemon(&[
            "--fixtures",
            corpus.to_str().expect("a utf-8 path"),
            "--db",
            root.join("sts.db").to_str().expect("a utf-8 path"),
            "--telemetry",
            stream.to_str().expect("a utf-8 path"),
            "--out",
            out.to_str().expect("a utf-8 path"),
            "--speed",
            "1",
            "--sandwich-guard",
            "required",
            "--private-entry",
        ]);

        wait_for(&stream, "replaying");
        send(&child, "INT");
        let (code, stderr) = finish(child);
        assert_eq!(
            code, 0,
            "a clean teardown is still a successful exit; stderr:\n{stderr}"
        );

        let report = report(&out);
        assert_eq!(report["process"]["stoppedBy"]["stop"], "signalled");
        assert_eq!(report["process"]["stoppedBy"]["detail"], "SIGINT");
        assert_eq!(
            report["pipeline"]["openPositions"], 0,
            "and it still sells nothing"
        );
        assert_eq!(
            report["pipeline"]["sandwichGuard"], "required",
            "the run that was interrupted is the run that was asked for"
        );
        assert_eq!(report["pipeline"]["privateEntry"], true);
    }
}

// ---------------------------------------------------------------------------
// the command line, through the binary a person actually runs
// ---------------------------------------------------------------------------

/// Separate from `signals` because nothing here sends one, and separate from
/// the tests above because those build a [`ScenarioConfig`] in process and so
/// cannot catch a flag that is parsed and then dropped on the floor.
mod command_line {
    use super::*;
    use std::process::Command;

    /// Telemetry goes to a file rather than to `-`.
    ///
    /// Not a preference: `--telemetry -` deadlocks this binary. `main.rs` takes
    /// `std::io::stderr().lock()` for the whole subcommand and the sink writes
    /// from the engine's own threads, so the first telemetry line blocks
    /// forever on a lock the main thread is holding until the run it is waiting
    /// for finishes. Filed apart from this change; these tests would hang on it.
    fn run(args: &[&str]) -> (i32, serde_json::Value, String) {
        let output = Command::new(env!("CARGO_BIN_EXE_sts"))
            .arg("daemon")
            .arg("run")
            .args(args)
            .output()
            .expect("the daemon runs");
        let code = output.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let report = serde_json::from_slice(&output.stdout).unwrap_or(serde_json::Value::Null);
        (code, report, stderr)
    }

    #[test]
    fn the_guard_and_the_route_reach_the_policy_the_run_decided_on() {
        let root = TempRoot::new("cli-guard");
        let corpus = root.corpus();
        let corpus = corpus.to_str().expect("a utf-8 path");

        let (code, report, stderr) = run(&[
            "--fixtures",
            corpus,
            "--db",
            root.join("a.db").to_str().expect("a utf-8 path"),
            "--telemetry",
            root.join("telemetry.ndjson")
                .to_str()
                .expect("a utf-8 path"),
        ]);
        assert_eq!(code, 0, "stderr:\n{stderr}");
        assert_eq!(
            report["pipeline"]["sandwichGuard"], "when-quoted",
            "the shipped default"
        );
        assert_eq!(report["pipeline"]["privateEntry"], false);

        let (code, report, stderr) = run(&[
            "--fixtures",
            corpus,
            "--db",
            root.join("b.db").to_str().expect("a utf-8 path"),
            "--telemetry",
            root.join("telemetry.ndjson")
                .to_str()
                .expect("a utf-8 path"),
            "--sandwich-guard",
            "required",
            "--private-entry",
        ]);
        assert_eq!(code, 0, "stderr:\n{stderr}");
        assert_eq!(report["pipeline"]["sandwichGuard"], "required");
        assert_eq!(report["pipeline"]["privateEntry"], true);

        // The report says what the run decided on, not what was overridden:
        // v1 ships with the guard off, and asking for it on has to show.
        let (code, report, stderr) = run(&[
            "--fixtures",
            corpus,
            "--db",
            root.join("c.db").to_str().expect("a utf-8 path"),
            "--telemetry",
            root.join("telemetry.ndjson")
                .to_str()
                .expect("a utf-8 path"),
            "--gate-profile",
            "v1",
        ]);
        assert_eq!(code, 0, "stderr:\n{stderr}");
        assert_eq!(report["pipeline"]["gateProfile"], "v1");
        assert_eq!(
            report["pipeline"]["sandwichGuard"], "off",
            "v1 shipped without it"
        );

        let (code, report, stderr) = run(&[
            "--fixtures",
            corpus,
            "--db",
            root.join("d.db").to_str().expect("a utf-8 path"),
            "--telemetry",
            root.join("telemetry.ndjson")
                .to_str()
                .expect("a utf-8 path"),
            "--gate-profile",
            "v1",
            "--sandwich-guard",
            "when-quoted",
        ]);
        assert_eq!(code, 0, "stderr:\n{stderr}");
        assert_eq!(report["pipeline"]["gateProfile"], "v1");
        assert_eq!(
            report["pipeline"]["sandwichGuard"], "when-quoted",
            "the override shows"
        );
    }

    #[test]
    fn the_two_knobs_the_guard_reads_are_refused_outside_their_range() {
        // Both feed §15.2's arithmetic and both have a value past which it stops
        // describing anything. The fee is the dangerous one: at 100% every
        // number on the check degenerates to zero and `sandwich_viable` answers
        // false for every size, so a run would clear every order while the
        // report said `sandwich-guard: required`. A guard that is silently off
        // is the one thing `required` exists to rule out.
        let root = TempRoot::new("cli-range");
        let corpus = root.corpus();
        let corpus = corpus.to_str().expect("a utf-8 path");

        let (code, _, err) = run(&["--fixtures", corpus, "--fee-bps", "10000"]);
        assert_eq!(code, 1, "a fee of the whole trade was accepted: {err}");
        assert!(err.contains("10000") && err.contains("9999"), "{err}");

        let (code, _, err) = run(&["--fixtures", corpus, "--max-pool-share-bps", "10001"]);
        assert_eq!(
            code, 1,
            "a position larger than the pool was accepted: {err}"
        );
        assert!(err.contains("10001") && err.contains("10000"), "{err}");
    }

    #[test]
    fn a_sizing_knob_with_no_corpus_is_refused_rather_than_dropped() {
        // The failure this catches is the quiet one: these three were read into
        // a `ScenarioConfig` that was then thrown away, so `sts daemon run
        // --max-pool-share-bps 500` exited zero having applied nothing. A run
        // that answers a question it was never asked is worse than one that
        // refuses to start.
        //
        // Refusals only, and that is not a gap: without `--fixtures` there is
        // nothing that ends, so a run that got past this check would wait for a
        // signal that no test is going to send it.
        for orphan in [
            vec!["--max-pool-share-bps", "500"],
            vec!["--fee-bps", "250"],
            vec!["--window-ms", "5000"],
            vec!["--entry-lamports", "1000000000"],
            vec!["--sandwich-guard", "required"],
            vec!["--private-entry"],
        ] {
            let (code, _, err) = run(&orphan);
            assert_eq!(code, 1, "{orphan:?} was accepted with nothing to play");
            assert!(err.contains("needs --fixtures"), "{orphan:?}: {err}");
        }
    }

    #[test]
    fn the_route_flag_moves_the_whole_funnel_and_says_so_on_the_report() {
        // `--private-entry` end to end, at a cap where it decides every verdict
        // in the run rather than one of them. Through the binary, because the
        // in-process tests build a `ScenarioConfig` themselves and so cannot
        // catch a flag that is parsed and dropped.
        let root = TempRoot::new("cli-route");
        let corpus = root.corpus();
        let corpus = corpus.to_str().expect("a utf-8 path");

        let telemetry = root.join("telemetry.ndjson");
        let telemetry = telemetry.to_str().expect("a utf-8 path");
        let at = |private: bool, db: &str| {
            let db = root.join(db);
            let db = db.to_str().expect("a utf-8 path");
            let mut args = vec![
                "--fixtures",
                corpus,
                "--db",
                db,
                "--telemetry",
                telemetry,
                "--gate-profile",
                "v1",
                "--sandwich-guard",
                "when-quoted",
                "--entry-lamports",
                "1000000000",
                "--max-pool-share-bps",
                "500",
            ];
            if private {
                args.push("--private-entry");
            }
            let (code, report, stderr) = run(&args);
            assert_eq!(code, 0, "stderr:\n{stderr}");
            report
        };

        let public = at(false, "public.db");
        let private = at(true, "private.db");

        assert_eq!(public["pipeline"]["privateEntry"], false);
        assert_eq!(private["pipeline"]["privateEntry"], true);

        let refusals = |report: &serde_json::Value| {
            report["pipeline"]["totals"]["reasons"]
                .as_array()
                .expect("the funnel is a list")
                .iter()
                .find(|row| row[0] == "sandwich-risk")
                .expect("every reason has a row")[1]
                .as_u64()
                .expect("a count")
        };

        assert_eq!(
            refusals(&public),
            1,
            "the public order at this cap is farmable"
        );
        assert_eq!(public["pipeline"]["totals"]["entered"], 0);
        assert_eq!(refusals(&private), 0, "and a bundle is not refused on it");
        assert_eq!(private["pipeline"]["totals"]["entered"], 1);

        // The exposure is still on the record either way — that is the whole
        // §15.4 claim, and a route that hid it would make the tip unjustifiable.
        for report in [&public, &private] {
            let checked = report["pipeline"]["cases"]
                .as_array()
                .expect("cases is a list")
                .iter()
                .flat_map(|case| case["launches"].as_array().expect("launches is a list"))
                .filter(|launch| launch["sandwich"]["aboveThreshold"] == true)
                .count();
            assert!(
                checked > 0,
                "an exposure over the threshold is on the report either way"
            );
        }
    }

    #[test]
    fn a_looser_cap_on_the_command_line_moves_a_verdict_and_names_it() {
        // The end to end shape of the whole change: one flag, and the launch
        // the rule liked is refused on the size that flag allows.
        let root = TempRoot::new("cli-cap");
        let corpus = root.corpus();
        let corpus = corpus.to_str().expect("a utf-8 path");

        let at_cap = |bps: &str, db: &str| {
            let (code, report, stderr) = run(&[
                "--fixtures",
                corpus,
                "--db",
                root.join(db).to_str().expect("a utf-8 path"),
                "--telemetry",
                root.join("telemetry.ndjson")
                    .to_str()
                    .expect("a utf-8 path"),
                "--gate-profile",
                "v1",
                "--sandwich-guard",
                "when-quoted",
                "--entry-lamports",
                "1000000000",
                "--max-pool-share-bps",
                bps,
            ]);
            assert_eq!(code, 0, "stderr:\n{stderr}");
            report
        };

        let shipped = at_cap("150", "shipped.db");
        assert_eq!(
            shipped["pipeline"]["totals"]["entered"], 1,
            "the cap keeps it enterable"
        );

        let loose = at_cap("500", "loose.db");
        assert_eq!(
            loose["pipeline"]["totals"]["entered"], 0,
            "and a looser one does not"
        );

        let refusals = loose["pipeline"]["totals"]["reasons"]
            .as_array()
            .expect("the funnel is a list")
            .iter()
            .find(|row| row[0] == "sandwich-risk")
            .expect("every reason has a row")[1]
            .clone();
        assert_eq!(refusals, 1, "and it is named for the question it died on");
    }
}
