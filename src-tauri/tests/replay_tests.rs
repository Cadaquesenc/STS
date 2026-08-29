//! The backtest runner, end to end, against fixtures written to a real disk.
//!
//! `replay.rs` has the unit tests for one clock, one cursor and one chain, and
//! `backtest.rs` has them for one curve and one trade. These are the ones that
//! need the whole stack: a fixture generated, written out, opened off the
//! filesystem, streamed through the transport a tick at a time, and priced on
//! the way past.
//!
//! Three properties are what this file exists for, and each is the kind that
//! only fails once several parts are put together.
//!
//! **The transport does not change the answer.** A fixture watched at `1x`, a
//! fixture fast-forwarded in one call, and a fixture stepped one record at a
//! time have to produce the same ledger. That is what makes a number taken off
//! the bar quotable, and it is a claim about `play`, the clock budget and the
//! observer seam jointly — no one of them can be tested into it.
//!
//! **The ledger agrees with the report.** The same fixture through
//! `evaluate_directory` and through a `PaperRunner` has to come out at the same
//! PnL. Two ways of pricing one recording that disagree make one of them wrong
//! and give nobody a way to say which.
//!
//! **Replay refuses to start over a live feed.** The bar the transport raises
//! says nothing under it is live, and on this build that is only true because
//! the sockets are shut. That guard is checked here against a manager with a
//! socket genuinely open, rather than against a mock of the check itself.
//!
//! Everything goes through the public API — `fixtures::generate`,
//! `ReplaySession`, `evaluate_directory`, `refuse_over_a_live_feed` — and
//! nothing reaches inside the crate to arrange a result.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use sts_lib::attribution::{
    attribute_plans, attribute_trace, AttributionConfig, AttributionReport, AttributionSummary,
    ExecutionLeg, FeeDecomposition, FeeSchedule, LegFees, MintTrace, ReplayTrace, ReturnSummary,
    RoundTripPlan, SlippageBucket, SlippageDistribution, TipSchedule, TracePoint, TraceRules,
    TradeAttribution, TradeExecution, TradeFees,
};
use sts_lib::backtest::{
    decode_event, evaluate_directory, BacktestConfig, EntryEvent, LaunchEvent, LaunchOpen,
    PaperRunner, Side,
};
use sts_lib::fixtures::{generate, FixtureCase, GeneratorConfig, Scenario};
use sts_lib::geyser::{default_transport, GeyserFeed};
use sts_lib::ingestion::{
    BoxFuture, EndpointConfig, FeedDialer, FeedMessage, FeedProvider, FeedSink, FeedStream,
    IngestError, IngestionConfig, IngestionManager, SolPrice, StreamFilters,
};
use sts_lib::mev_sim::{
    buy_through, extract_across_pools, sell_through, simulate_reorg, sweep_reorgs, AdversaryConfig,
    AdversaryProfile, FrontRunCost, MarketContext, MevOutcome, MevSummary, MultiPoolExtraction,
    PoolExtraction, PoolTarget, ReorgFate, ReorgGrid, ReorgOutcome, ReorgScenario, ReorgSummary,
    DEFAULT_ALLOCATION_SLICES, DEFAULT_MAX_PENALTY_BPS,
};
use sts_lib::replay::{
    parse_stream, slippage_bps, CurveState, PlaybackState, QuoteError, ReplayControl,
    ReplaySession, ReplaySpeed, SimulatedLedger, BPS_DENOMINATOR, DEFAULT_FEE_BPS,
    LAMPORTS_PER_SOL, PUMP_GRADUATION_LAMPORTS,
};
use sts_lib::{refuse_over_a_live_feed, REPLAY_TICK};

// ===========================================================================
// the fixture on disk
// ===========================================================================

/// A generated corpus case written into a directory of its own.
///
/// Written out rather than held in memory because reading a fixture off a
/// filesystem is part of what is being tested: segment ordering comes from
/// `read_dir` and a sort, and a test that handed the session a `Vec` would
/// never exercise it.
struct Fixture {
    root: PathBuf,
    dir: PathBuf,
    case: FixtureCase,
}

impl Fixture {
    fn new(name: &str, scenario: Scenario) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "sts-replay-runner-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);

        let config = GeneratorConfig::default();
        let cases = generate(scenario, &config).expect("the generator builds its own scenario");
        let case = cases
            .into_iter()
            .next()
            .expect("every scenario produces at least one case");
        let dir = sts_lib::fixtures::write_case(&root, &case, true).expect("the case writes");

        Fixture { root, dir, case }
    }

    /// A session over the fixture with a runner pricing it.
    ///
    /// The runner's fee has to be the generator's fee — every size in the case
    /// was computed against it — so it is taken from the case rather than from
    /// a default that could drift away from it.
    fn session(&self) -> ReplaySession {
        ReplaySession::new(&self.dir).observing(PaperRunner::new(self.config()))
    }

    /// A session that streams the fixture and prices nothing.
    fn bare_session(&self) -> ReplaySession {
        ReplaySession::new(&self.dir)
    }

    fn config(&self) -> BacktestConfig {
        BacktestConfig {
            fee_bps: self.case.expected.config.fee_bps,
            ..BacktestConfig::default()
        }
    }

    fn dir(&self) -> &Path {
        &self.dir
    }

    fn records(&self) -> u64 {
        self.case.expected.records
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Ticks a session the way the runtime does until it ends, or gives up.
///
/// The bound is on ticks rather than on wall time so the test is the same on a
/// loaded machine as on an idle one. It is deliberately generous: what is being
/// asserted is that the transport arrives, not how few ticks it took.
/// A Geyser feed with no endpoint behind it.
///
/// The guard checks both feeds now, and every test in this file is about the
/// websocket half of it. An unconfigured feed opens no socket and reports
/// itself down, so it is the neutral second argument that leaves each of these
/// tests asserting exactly what it asserted before.
fn idle_geyser(ingestion: &Arc<IngestionManager>) -> Arc<GeyserFeed> {
    GeyserFeed::start(None, default_transport(), Arc::clone(ingestion), None)
}

fn play_out(session: &ReplaySession, max_ticks: usize) -> usize {
    let tick_ms = REPLAY_TICK.as_millis() as u64;
    for tick in 1..=max_ticks {
        let status = session.advance(tick_ms);
        if status.state == PlaybackState::Ended {
            return tick;
        }
    }
    panic!("the fixture did not finish inside {max_ticks} ticks");
}

// ===========================================================================
// streaming a fixture off disk
// ===========================================================================

#[test]
fn a_generated_fixture_opens_off_the_filesystem_and_says_what_it_is() {
    let fixture = Fixture::new("opens", Scenario::Graduation);
    let session = fixture.session();

    let status = session.start().expect("the generated fixture opens");

    assert_eq!(status.state, PlaybackState::Playing);
    assert!(status.active, "a fixture behind the clock is not live");
    assert_eq!(
        status.stream_id.as_deref(),
        Some(fixture.case.stream_id.as_str()),
        "the stream id comes from the manifest, not from the directory name"
    );
    assert_eq!(status.record_count, fixture.records());
    assert_eq!(
        status.chain_verified,
        Some(true),
        "a freshly generated fixture computes to the head its own manifest declares"
    );
    assert_eq!(status.fixture_complete, Some(true));
    assert_eq!(
        status.clamped, 0,
        "the generator writes a stream whose wall clock never goes backwards"
    );
    assert_eq!(status.slot_regressions, 0);
}

#[test]
fn a_missing_fixture_is_named_rather_than_played_as_an_empty_one() {
    let session = ReplaySession::new(std::env::temp_dir().join("sts-replay-runner-nothing-here"));

    let err = session.start().expect_err("there is no fixture there");
    let message = err.to_string();

    assert!(
        message.contains("sts-replay-runner-nothing-here"),
        "the refusal names the directory it looked in: {message}"
    );
    assert_eq!(
        session.status().state,
        PlaybackState::Stopped,
        "a start that refused did not put the window in replay"
    );
}

// ===========================================================================
// the 250ms ticker
// ===========================================================================

#[test]
fn the_ticker_runs_at_four_hertz() {
    // The cadence the transport is actually driven at, pinned where a change to
    // it has to be a change to a test as well. Four times a second is what
    // makes a playhead look like a playhead; a tick a second makes `1x`
    // playback arrive in visible jumps.
    assert_eq!(REPLAY_TICK, Duration::from_millis(250));
}

#[test]
fn a_tick_at_one_x_buys_a_ticks_worth_of_recording() {
    let fixture = Fixture::new("one-x", Scenario::Graduation);
    let session = fixture.session();
    session.start().expect("opens");

    let tick_ms = REPLAY_TICK.as_millis() as i64;
    let first = session.advance(REPLAY_TICK.as_millis() as u64);
    let second = session.advance(REPLAY_TICK.as_millis() as u64);

    let bought = second.at_ms - first.at_ms;
    assert!(
        bought >= 0,
        "the virtual clock never goes backwards between ticks"
    );
    // One tick may overspend by the gap in front of the record that broke the
    // budget — `play` checks before stepping so a record is never half
    // delivered — but it cannot spend two ticks' worth plus a gap and still be
    // playing at `1x`.
    assert!(
        bought <= tick_ms * 2 || second.state == PlaybackState::Ended,
        "a 1x tick bought {bought}ms of recording out of a {tick_ms}ms budget"
    );
    assert!(
        second.records_played > first.records_played || second.state == PlaybackState::Ended,
        "a tick that bought time and no records is a stalled playhead"
    );
}

#[test]
fn ten_x_reaches_the_end_in_fewer_ticks_than_one_x() {
    let fixture = Fixture::new("ten-x", Scenario::Graduation);

    let slow = fixture.session();
    slow.start().expect("opens");
    slow.set_speed(ReplaySpeed::Real);
    let slow_ticks = play_out(&slow, 100_000);

    let quick = fixture.session();
    quick.start().expect("opens");
    quick.set_speed(ReplaySpeed::Ten);
    let quick_ticks = play_out(&quick, 100_000);

    assert!(
        quick_ticks < slow_ticks,
        "10x took {quick_ticks} ticks and 1x took {slow_ticks}"
    );
    assert_eq!(
        slow.status().ledger,
        quick.status().ledger,
        "the multiplier is how fast it is watched, not what it is worth"
    );
}

#[test]
fn a_stopped_session_ignores_the_ticker_entirely() {
    let fixture = Fixture::new("stopped", Scenario::Graduation);
    let session = fixture.session();
    session.start().expect("opens");
    session.advance(REPLAY_TICK.as_millis() as u64);

    let stopped = session.stop();
    for _ in 0..40 {
        session.advance(REPLAY_TICK.as_millis() as u64);
    }
    let after = session.status();

    assert!(!stopped.active, "stop takes the fixture off the clock");
    assert_eq!(after.state, PlaybackState::Stopped);
    assert_eq!(
        after.records_played, stopped.records_played,
        "a stopped session plays nothing however often it is ticked"
    );
}

// ===========================================================================
// pause, step, fast-forward
// ===========================================================================

#[test]
fn pausing_holds_the_playhead_while_the_ticker_keeps_running() {
    let fixture = Fixture::new("pause", Scenario::Graduation);
    let session = fixture.session();
    session.start().expect("opens");
    for _ in 0..4 {
        session.advance(REPLAY_TICK.as_millis() as u64);
    }

    let paused = session.pause();
    for _ in 0..40 {
        session.advance(REPLAY_TICK.as_millis() as u64);
    }
    let after = session.status();

    assert_eq!(paused.state, PlaybackState::Paused);
    assert!(
        paused.active,
        "a paused fixture is still what the panes are showing, so the bar stays up"
    );
    assert_eq!(
        after.records_played, paused.records_played,
        "ten seconds of ticks bought nothing while paused"
    );
    assert_eq!(
        after.at_ms, paused.at_ms,
        "and the virtual clock stood still with it"
    );
    assert_eq!(
        after.ledger, paused.ledger,
        "so the books did not move either"
    );
}

#[test]
fn resume_carries_on_from_where_pause_left_it() {
    let fixture = Fixture::new("resume", Scenario::Graduation);
    let session = fixture.session();
    session.start().expect("opens");
    session.advance(REPLAY_TICK.as_millis() as u64);
    let paused = session.pause();

    let resumed = session.resume().expect("a paused fixture resumes");
    session.advance(REPLAY_TICK.as_millis() as u64);
    let after = session.status();

    assert_eq!(resumed.state, PlaybackState::Playing);
    assert_eq!(
        resumed.records_played, paused.records_played,
        "resume is not a rewind: the playhead is where pause left it"
    );
    assert!(
        after.records_played > paused.records_played,
        "and the next tick moves it on rather than replaying what was already played"
    );
}

#[test]
fn a_step_moves_exactly_one_record_and_pauses() {
    let fixture = Fixture::new("step", Scenario::Graduation);
    let session = fixture.session();
    session.start().expect("opens");
    session.advance(REPLAY_TICK.as_millis() as u64);
    let before = session.pause();

    let after = session.step(1).expect("a step steps");

    assert_eq!(
        after.records_played,
        before.records_played + 1,
        "a step is one record, whatever the multiplier says"
    );
    assert_eq!(
        after.state,
        PlaybackState::Paused,
        "a step that kept playing would move the playhead off the record it stopped on"
    );
}

#[test]
fn a_step_at_max_speed_is_still_one_record() {
    let fixture = Fixture::new("step-max", Scenario::Graduation);
    let session = fixture.session();
    session.start().expect("opens");
    session.set_speed(ReplaySpeed::Max);
    let before = session.pause();

    let after = session.step(1).expect("a step steps");

    assert_eq!(after.records_played, before.records_played + 1);
}

#[test]
fn a_step_on_a_fresh_session_opens_the_fixture_and_stops_on_the_first_record() {
    let fixture = Fixture::new("step-fresh", Scenario::Graduation);
    let session = fixture.session();

    let after = session
        .step(1)
        .expect("the fixture opens on the first press");

    assert_eq!(after.records_played, 1);
    assert_eq!(after.state, PlaybackState::Paused);
    assert_eq!(after.record_count, fixture.records());
    assert!(
        after.active,
        "the first step put a recording behind the clock, and the bar has to say so"
    );
}

#[test]
fn stepping_through_the_whole_fixture_ends_it() {
    let fixture = Fixture::new("step-out", Scenario::Graduation);
    let session = fixture.session();

    let mut status = session.step(1).expect("opens");
    let mut steps = 1u64;
    while status.state != PlaybackState::Ended {
        status = session.step(1).expect("steps");
        steps += 1;
        assert!(steps <= fixture.records() + 1, "stepping did not terminate");
    }

    assert_eq!(status.records_played, fixture.records());
    assert_eq!(steps, fixture.records(), "one step per record, no more");
}

#[test]
fn a_fast_forward_with_no_count_plays_the_whole_fixture() {
    let fixture = Fixture::new("ff-all", Scenario::Graduation);
    let session = fixture.session();

    let after = session.fast_forward(None).expect("opens and runs");

    assert_eq!(after.state, PlaybackState::Ended);
    assert_eq!(after.records_played, fixture.records());
    assert_eq!(
        after.records_played, after.record_count,
        "the end of the fixture is every record played, not a playhead parked near it"
    );
}

#[test]
fn a_bounded_fast_forward_stops_where_it_was_told_to() {
    let fixture = Fixture::new("ff-bounded", Scenario::Graduation);
    let session = fixture.session();

    let after = session.fast_forward(Some(5)).expect("opens and runs");

    assert_eq!(after.records_played, 5);
    assert_eq!(after.state, PlaybackState::Paused);
}

#[test]
fn a_fast_forward_past_the_end_is_the_end_rather_than_an_error() {
    let fixture = Fixture::new("ff-past", Scenario::Graduation);
    let session = fixture.session();

    let after = session
        .fast_forward(Some(fixture.records() * 10))
        .expect("asking for more than there is, is not a refusal");

    assert_eq!(after.state, PlaybackState::Ended);
    assert_eq!(after.records_played, fixture.records());

    // And again, on a fixture that has already ended.
    let again = session.fast_forward(None).expect("still not a refusal");
    assert_eq!(again.records_played, fixture.records());
}

#[test]
fn resume_at_the_end_is_a_no_op_rather_than_a_rewind() {
    let fixture = Fixture::new("resume-ended", Scenario::Graduation);
    let session = fixture.session();
    let ended = session.fast_forward(None).expect("runs");

    let after = session.resume().expect("resume answers");

    assert_eq!(after.state, PlaybackState::Ended);
    assert_eq!(
        after.records_played, ended.records_played,
        "a play button that silently rewound would lose the run an operator was reading"
    );
    assert_eq!(after.ledger, ended.ledger, "and the books with it");
}

#[test]
fn the_transport_control_is_the_same_thing_as_the_methods() {
    let fixture = Fixture::new("control", Scenario::Graduation);

    let direct = fixture.session();
    direct.start().expect("opens");
    direct.advance(REPLAY_TICK.as_millis() as u64);
    direct.pause();
    direct.step(3).expect("steps");
    let by_method = direct.status();

    let control = fixture.session();
    control.control(ReplayControl::Play, None).expect("opens");
    control.advance(REPLAY_TICK.as_millis() as u64);
    control.control(ReplayControl::Pause, None).expect("pauses");
    control
        .control(ReplayControl::Step, Some(3))
        .expect("steps");
    let by_control = control.status();

    assert_eq!(by_method.records_played, by_control.records_played);
    assert_eq!(by_method.state, by_control.state);
    assert_eq!(by_method.ledger, by_control.ledger);
}

#[test]
fn a_step_with_no_count_is_one_record() {
    let fixture = Fixture::new("control-step", Scenario::Graduation);
    let session = fixture.session();

    let after = session
        .control(ReplayControl::Step, None)
        .expect("a bare step is a frame advance");

    assert_eq!(after.records_played, 1);
}

// ===========================================================================
// determinism: the transport does not change the answer
// ===========================================================================

/// The ledger a fixture produces when it is played out in one call.
fn ledger_by_fast_forward(fixture: &Fixture) -> SimulatedLedger {
    let session = fixture.session();
    session.fast_forward(None).expect("runs").ledger
}

/// The same fixture, watched a tick at a time at `1x`.
fn ledger_by_ticking(fixture: &Fixture) -> SimulatedLedger {
    let session = fixture.session();
    session.start().expect("opens");
    play_out(&session, 100_000);
    session.status().ledger
}

/// The same fixture again, one record per press.
fn ledger_by_stepping(fixture: &Fixture) -> SimulatedLedger {
    let session = fixture.session();
    let mut status = session.step(1).expect("opens");
    while status.state != PlaybackState::Ended {
        status = session.step(1).expect("steps");
    }
    status.ledger
}

#[test]
fn watching_stepping_and_fast_forwarding_agree_on_the_ledger() {
    // The property the whole runner rests on. A number taken off the bar after
    // watching a fixture is only worth quoting if it is the number the same
    // fixture produces when it is run flat out, and a budget spent against the
    // clock rather than against a record count is exactly the kind of thing
    // that quietly makes those two differ.
    for scenario in [
        Scenario::Graduation,
        Scenario::SybilRug,
        Scenario::SandwichBoundary,
    ] {
        let fixture = Fixture::new("agree", scenario);

        let quick = ledger_by_fast_forward(&fixture);
        let watched = ledger_by_ticking(&fixture);
        let stepped = ledger_by_stepping(&fixture);

        assert_eq!(quick, watched, "{scenario}: fast-forward and 1x disagree");
        assert_eq!(
            quick, stepped,
            "{scenario}: fast-forward and stepping disagree"
        );

        // Three equal ledgers prove nothing if all three are empty, and a
        // runner that quietly stopped booking would produce exactly that.
        // Every case in this list opens a launch, applies its events and takes
        // at least one position, so a zero in any of these is a regression
        // rather than a scenario that had nothing to say.
        assert!(quick.launches > 0, "{scenario}: no launch was opened");
        assert!(quick.events_applied > 0, "{scenario}: no event was applied");
        assert!(quick.entries > 0, "{scenario}: nothing was ever bought");
        assert!(
            quick.slippage_bps > 0,
            "{scenario}: fills that cost nothing are fills that were not priced"
        );
    }
}

#[test]
fn a_second_run_of_one_fixture_is_the_first_run_again() {
    // Twelve round trips across three launches, rather than the single trip
    // `Graduation` has: a determinism test wants the case with the most moving
    // parts in it, not the tidiest one.
    let fixture = Fixture::new("twice", Scenario::SandwichBoundary);
    let session = fixture.session();

    let first = session.fast_forward(None).expect("runs").ledger;
    // `start` rewinds, which has to rewind the books as well: a ledger left
    // where the last run finished would report one fixture's records against
    // two fixtures' PnL.
    session.start().expect("restarts");
    let second = session.fast_forward(None).expect("runs again").ledger;

    assert_eq!(first, second);
    assert!(
        first.trades > 0,
        "a determinism test over a fixture that never traded proves nothing"
    );
}

#[test]
fn a_session_with_no_runner_streams_the_fixture_and_books_nothing() {
    let fixture = Fixture::new("bare", Scenario::Graduation);
    let session = fixture.bare_session();

    let after = session.fast_forward(None).expect("runs");

    assert_eq!(after.records_played, fixture.records(), "it still played");
    assert_eq!(
        after.ledger,
        SimulatedLedger::default(),
        "a session pricing nothing reports zeroes rather than a break-even run"
    );
}

// ===========================================================================
// the books: PnL, slippage, tips
// ===========================================================================

#[test]
fn the_ledger_is_the_report_the_evaluator_gives_for_the_same_fixture() {
    // Two ways of pricing one recording. If they disagree, one of them is
    // wrong and nothing in either says which — so this is the test that keeps
    // the streaming runner honest against the batch harness it is an
    // incremental version of.
    for scenario in [
        Scenario::Graduation,
        Scenario::SybilRug,
        Scenario::SandwichBoundary,
    ] {
        let fixture = Fixture::new("cross-check", scenario);
        let ledger = ledger_by_fast_forward(&fixture);
        let report = evaluate_directory(fixture.dir(), fixture.config())
            .expect("the same directory evaluates");

        assert_eq!(
            ledger.realized_pnl_lamports, report.performance.realized_pnl_lamports,
            "{scenario}: realised PnL"
        );
        assert_eq!(
            ledger.fees_lamports, report.performance.fees_paid_lamports,
            "{scenario}: fees"
        );
        assert_eq!(
            ledger.trades,
            u64::from(report.performance.trades),
            "{scenario}: closed trades"
        );
        assert_eq!(
            ledger.launches,
            report.launches.len() as u64,
            "{scenario}: launches seen"
        );

        let entries: u64 = report.launches.iter().map(|l| u64::from(l.entries)).sum();
        let exits: u64 = report.launches.iter().map(|l| u64::from(l.exits)).sum();
        assert_eq!(ledger.entries, entries, "{scenario}: entries");
        assert_eq!(ledger.exits, exits, "{scenario}: exits");

        let gross: u64 = report.launches.iter().map(|l| l.entry_gross_lamports).sum();
        let net: u64 = report.launches.iter().map(|l| l.exit_net_lamports).sum();
        assert_eq!(
            ledger.entry_gross_lamports, gross,
            "{scenario}: entry gross"
        );
        assert_eq!(ledger.exit_net_lamports, net, "{scenario}: exit net");
    }
}

#[test]
fn the_ledger_fills_in_as_the_fixture_plays_rather_than_at_the_end() {
    let fixture = Fixture::new("incremental", Scenario::Graduation);
    let session = fixture.session();
    let final_ledger = ledger_by_fast_forward(&fixture);

    session.start().expect("opens");
    let mut seen_a_partial_book = false;
    let mut previous = session.status().ledger;
    for _ in 0..100_000 {
        let status = session.advance(REPLAY_TICK.as_millis() as u64);
        let ledger = status.ledger;

        assert!(
            ledger.events_applied >= previous.events_applied,
            "the books only ever go forward"
        );
        assert!(
            ledger.entries >= previous.entries && ledger.exits >= previous.exits,
            "and so do the counters on them"
        );
        // Against applied events rather than against entries: a case with one
        // round trip in it goes from zero entries to its last entry in a single
        // record, and there is no moment in between for a test to catch.
        if ledger.events_applied > 0 && ledger.events_applied < final_ledger.events_applied {
            seen_a_partial_book = true;
        }
        previous = ledger;
        if status.state == PlaybackState::Ended {
            break;
        }
    }

    assert!(
        seen_a_partial_book,
        "a ledger that is only right at the end is a report, not a running book"
    );
    assert_eq!(previous, final_ledger, "and it lands on the same place");
}

#[test]
fn slippage_is_size_weighted_and_never_flatters_the_trader() {
    let fixture = Fixture::new("slippage", Scenario::SandwichBoundary);
    let ledger = ledger_by_fast_forward(&fixture);

    assert!(ledger.entries > 0, "the case has to have traded");
    assert!(
        ledger.slippage_bps > 0,
        "every fill on a bonding curve costs something; a zero mean is a counter that is not wired up"
    );
    assert!(
        ledger.worst_slippage_bps >= ledger.slippage_bps,
        "the worst fill cannot be better than the mean of every fill"
    );
    assert!(
        ledger.slippage_bps <= 10_000 && ledger.worst_slippage_bps <= 10_000,
        "slippage is a share and cannot exceed the whole"
    );
}

#[test]
fn the_weighting_is_by_size_and_not_a_mean_of_means() {
    // The assertions above are all satisfied by a flat per-fill average, which
    // is the wrong number and the easy mistake: it lets a dust trade and a
    // one-SOL trade vote equally on what this strategy pays to trade. So this
    // one feeds two fills three orders of magnitude apart and pins the answer
    // against an expectation computed from the curve directly, rather than
    // from the runner being tested.
    let big = 2 * LAMPORTS_PER_SOL;
    let small = LAMPORTS_PER_SOL / 1_000;
    let fee = BacktestConfig::default().fee_bps;

    // What the curve says each fill costs, walked by hand in the same order
    // the runner will see them.
    let curve = CurveState::LAUNCH;
    let first = curve.quote_buy(big, fee).expect("the big entry quotes");
    let second = curve
        .after_buy(&first)
        .quote_buy(small, fee)
        .expect("the small entry quotes");

    let weighted = (u128::from(first.slippage_bps) * u128::from(first.gross_lamports)
        + u128::from(second.slippage_bps) * u128::from(second.gross_lamports))
    .div_ceil(u128::from(first.gross_lamports) + u128::from(second.gross_lamports))
        as u16;
    let flat =
        (u128::from(first.slippage_bps) + u128::from(second.slippage_bps)).div_ceil(2) as u16;
    assert_ne!(
        weighted, flat,
        "a test of the weighting needs two fills the two rules disagree about"
    );

    let mut runner = PaperRunner::new(BacktestConfig::default());
    runner.apply(
        &LaunchEvent::Launch(LaunchOpen {
            mint: "weighting".to_string(),
            at_ms: 1_700_000_000_000,
            creator: None,
            curve: CurveState::LAUNCH,
        }),
        "open",
    );
    for (index, gross) in [big, small].into_iter().enumerate() {
        runner.apply(
            &LaunchEvent::Entry(EntryEvent {
                mint: "weighting".to_string(),
                at_ms: 1_700_000_000_001 + index as i64,
                gross_lamports: gross,
                tag: None,
            }),
            &format!("entry-{index}"),
        );
    }

    let ledger = runner.ledger();
    assert_eq!(ledger.entries, 2, "both entries filled");
    assert_eq!(
        ledger.slippage_bps, weighted,
        "the mean is weighted by the SOL leg of each fill"
    );
    assert_eq!(
        ledger.worst_slippage_bps,
        first.slippage_bps.max(second.slippage_bps),
        "and the worst case is the worst single fill, not the worst weighted one"
    );
}

#[test]
fn every_filled_exit_is_tipped_and_the_bid_is_the_same_on_every_run() {
    let fixture = Fixture::new("tips", Scenario::Graduation);

    let first = ledger_by_fast_forward(&fixture);
    let second = ledger_by_stepping(&fixture);

    assert!(first.exits > 0, "the case has to have exited something");
    assert_eq!(
        first.tips_bid, first.exits,
        "one bid per filled exit — an exit the curve refused is not a bundle anybody sent"
    );
    assert_eq!(
        first.tips_refused, 0,
        "an exit-stance policy bids for losing exits too, because getting out still costs"
    );
    assert!(
        first.tips_lamports > 0,
        "a tip of zero lamports is a bundle a block engine will not look at"
    );
    assert_eq!(
        first.tips_lamports, second.tips_lamports,
        "the bid is priced from the record's own event id, so it does not depend on how it was played"
    );
}

#[test]
fn a_frame_the_live_filters_dropped_is_counted_and_never_priced() {
    // The backpressure case is recorded through a saturated engine: some frames
    // the queues could not take, some the filters threw away. The first are
    // replayed — that is what recovery means — and the second are not, because
    // the live engine never saw them either and pricing them here would be the
    // filtering bug the fidelity check exists to catch, arriving by the back
    // door.
    let fixture = Fixture::new("filtered", Scenario::Backpressure);
    let expected = &fixture.case.expected;
    let ledger = ledger_by_fast_forward(&fixture);

    assert!(
        expected.frames_dropped_live > 0,
        "this scenario is only a test of the rule if it has dropped frames in it"
    );
    assert_eq!(
        ledger.events_filtered, expected.frames_dropped_live,
        "every frame the recording says was filtered is counted as filtered"
    );
    assert_eq!(
        ledger.events_undecodable, 0,
        "and none of the ones that were replayed failed to decode"
    );
}

#[test]
fn the_ledger_survives_the_wire_in_the_shape_the_window_reads() {
    let fixture = Fixture::new("wire", Scenario::Graduation);
    let session = fixture.session();
    let status = session.fast_forward(None).expect("runs");

    let json = serde_json::to_value(&status).expect("the status serialises");

    assert_eq!(json["state"], "ended");
    assert_eq!(json["active"], true);
    let ledger = &json["ledger"];
    assert_eq!(ledger["trades"], status.ledger.trades);
    assert_eq!(
        ledger["realizedPnlLamports"], status.ledger.realized_pnl_lamports,
        "camel case, because that is what the window reads"
    );
    assert_eq!(ledger["slippageBps"], status.ledger.slippage_bps);
    assert_eq!(ledger["tipsLamports"], status.ledger.tips_lamports);
    assert!(
        json["atMs"].as_i64().unwrap_or(0) > 0,
        "the virtual wall clock reaches the window beside the slot"
    );
}

// ===========================================================================
// the live feed guard
// ===========================================================================

/// A dialer that opens a socket and then says nothing.
///
/// The quiet is the point: the endpoint connects, reports itself connected, and
/// stays that way, which is the state the guard is about. A dialer that fed
/// frames would drag the whole ingest path into a test about one refusal.
struct SilentDialer {
    dials: AtomicU64,
}

impl SilentDialer {
    fn new() -> Self {
        SilentDialer {
            dials: AtomicU64::new(0),
        }
    }

    fn dial_count(&self) -> u64 {
        self.dials.load(Ordering::SeqCst)
    }
}

impl FeedDialer for SilentDialer {
    fn dial(
        &self,
        _endpoint: EndpointConfig,
    ) -> BoxFuture<'static, Result<(Box<dyn FeedSink>, Box<dyn FeedStream>), IngestError>> {
        self.dials.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok((
                Box::new(SilentSink) as Box<dyn FeedSink>,
                Box::new(SilentSource(Mutex::new(VecDeque::new()))) as Box<dyn FeedStream>,
            ))
        })
    }
}

struct SilentSink;

impl FeedSink for SilentSink {
    fn send_text(&mut self, _text: String) -> BoxFuture<'_, Result<(), IngestError>> {
        Box::pin(async { Ok(()) })
    }

    fn ping(&mut self) -> BoxFuture<'_, Result<(), IngestError>> {
        Box::pin(async { Ok(()) })
    }
}

struct SilentSource(Mutex<VecDeque<FeedMessage>>);

impl FeedStream for SilentSource {
    fn recv(&mut self) -> BoxFuture<'_, Option<Result<FeedMessage, IngestError>>> {
        let next = self.0.lock().pop_front();
        Box::pin(async move {
            match next {
                Some(message) => Some(Ok(message)),
                // Parks rather than ending, so the endpoint stays connected for
                // as long as the test needs it to.
                None => std::future::pending().await,
            }
        })
    }
}

fn one_endpoint() -> IngestionConfig {
    IngestionConfig {
        endpoints: vec![EndpointConfig::new(
            FeedProvider::ALL[0],
            "wss://guard.test/?api-key=x",
            1,
        )],
        filters: StreamFilters::DEFAULT,
        price: SolPrice::UNKNOWN,
        // Long enough that nothing here races the telemetry task.
        telemetry_interval: Duration::from_secs(3_600),
    }
}

/// Polls until `condition` holds or the budget runs out.
async fn until(max_ms: u64, mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..max_ms / 10 {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    condition()
}

#[tokio::test]
async fn replay_refuses_to_start_while_a_feed_endpoint_is_connected() {
    let dialer = Arc::new(SilentDialer::new());
    let (manager, _streams) = IngestionManager::start(
        one_endpoint(),
        Arc::clone(&dialer) as Arc<dyn FeedDialer>,
        None,
        None,
    );

    assert!(
        until(2_000, || dialer.dial_count() >= 1
            && manager.snapshot().endpoints.iter().any(|e| e.connected))
        .await,
        "the mock endpoint never reported itself connected"
    );

    let refusal = refuse_over_a_live_feed(&manager, &idle_geyser(&manager))
        .expect_err("a fixture must not go behind the clock over a live socket");
    let message = refusal.to_string();

    assert!(
        message.contains("still connected"),
        "the refusal says what is in the way: {message}"
    );
    assert!(
        message.contains("Stop the feeds first"),
        "and what to do about it: {message}"
    );

    manager.stop();
}

#[tokio::test]
async fn with_nothing_connected_the_guard_lets_replay_through() {
    // The other half of the guard, and the one that matters for a normal
    // desktop: a checkout with no provider URLs configures no endpoints, and a
    // manager that dials nothing must not be mistaken for a live one.
    let dialer = Arc::new(SilentDialer::new());
    let config = IngestionConfig {
        endpoints: Vec::new(),
        ..one_endpoint()
    };
    let (manager, _streams) = IngestionManager::start(
        config,
        Arc::clone(&dialer) as Arc<dyn FeedDialer>,
        None,
        None,
    );

    assert!(
        refuse_over_a_live_feed(&manager, &idle_geyser(&manager)).is_ok(),
        "nothing is connected, so nothing is in the way"
    );
    assert_eq!(dialer.dial_count(), 0, "and nothing was dialled");

    manager.stop();
}

#[tokio::test]
async fn a_fixture_plays_once_the_feeds_are_stopped() {
    // The guard is a refusal to start over a live feed, not a refusal to ever
    // start. A build where stopping the sockets did not clear it would leave
    // replay unreachable on any machine with providers configured.
    let fixture = Fixture::new("after-stop", Scenario::Graduation);
    let dialer = Arc::new(SilentDialer::new());
    let (manager, _streams) = IngestionManager::start(
        one_endpoint(),
        Arc::clone(&dialer) as Arc<dyn FeedDialer>,
        None,
        None,
    );

    assert!(
        until(2_000, || manager
            .snapshot()
            .endpoints
            .iter()
            .any(|e| e.connected))
        .await,
        "the mock endpoint never connected"
    );
    assert!(refuse_over_a_live_feed(&manager, &idle_geyser(&manager)).is_err());

    manager.stop();
    assert!(
        until(2_000, || manager
            .snapshot()
            .endpoints
            .iter()
            .all(|e| !e.connected))
        .await,
        "the sockets never reported themselves shut"
    );

    refuse_over_a_live_feed(&manager, &idle_geyser(&manager))
        .expect("with the feeds down, replay may start");
    let session = fixture.session();
    let status = session.start().expect("and it does");
    assert_eq!(status.state, PlaybackState::Playing);
}

// ===========================================================================
// the simulation pipeline, against a fixture that was actually recorded
// ===========================================================================

/// Every event a fixture recorded, decoded, in stream order.
///
/// The seam this section is built on. `parse_stream` gives the records back in
/// the order the chain sealed them and `decode_event` turns each frame into
/// something with an economic meaning, so what comes out is the recording as
/// the engine saw it rather than as a test arranged it.
///
/// Frames the recording says the live filters dropped are skipped: the live
/// engine never saw them, so a trace that applied them would be a trace of a
/// curve nobody traded.
fn recorded_events(fixture: &Fixture) -> Vec<LaunchEvent> {
    let text = fixture.case.text();
    let records = parse_stream(&text).expect("a generated fixture parses");
    let mut events = Vec::new();
    for record in &records {
        if !record.outcome.is_accepted() {
            continue;
        }
        let Some(frame) = record.frame.as_ref() else {
            continue;
        };
        // A frame this build does not understand is not an error here: the
        // record is genuine and the payload is one a later version added.
        if let Ok(event) = decode_event(frame, record.seq) {
            events.push(event);
        }
    }
    assert!(!events.is_empty(), "the fixture decoded to nothing");
    events
}

/// The historical replay trace a fixture reduces to.
fn recorded_trace(fixture: &Fixture) -> ReplayTrace {
    ReplayTrace::from_events(&recorded_events(fixture), fixture.config().fee_bps)
}

/// The attribution configuration these tests price under.
///
/// The fee has to be the generator's — every size in the case was computed
/// against it — and the adversary has to be funded well enough to find
/// something, or the MEV line is zero for a reason that has nothing to do with
/// the code under test.
fn attribution_config(fixture: &Fixture, profile: AdversaryProfile) -> AttributionConfig {
    let mut config = AttributionConfig {
        fee_bps: fixture.config().fee_bps,
        cents_per_sol: 15_000,
        ..AttributionConfig::default()
    }
    .against(profile);
    config.adversary = config
        .adversary
        .bounded(20 * LAMPORTS_PER_SOL, DEFAULT_MAX_PENALTY_BPS);
    config.adversary.landing_cost_lamports = 1_000_000;
    config
}

/// Rules that fit the recordings these tests are run against.
///
/// The **size** is the load-bearing part, and it is not a round number chosen
/// for looks. §15.2's threshold is `b* = φy/(1 − φ)²`, which at this venue's
/// hundred basis points works out at about `y / 98` — so on a curve holding
/// between 30 and 115 SOL of virtual reserve, a front-run only pays against a
/// victim buying somewhere north of a third of a SOL. `TraceRules::default`
/// commits a tenth of one, which is below the threshold at every point on the
/// curve: a sweep at that size would report an MEV line of zero for a reason
/// that has nothing to do with the code under test, and would keep reporting
/// zero if the adversary were deleted outright.
///
/// Two SOL is comfortably above the threshold everywhere on the curve, which
/// is what makes "the hostile run costs more" a test of the model rather than
/// of the sizing.
fn trace_rules() -> TraceRules {
    TraceRules::default()
        .sized(2 * LAMPORTS_PER_SOL)
        .laddered(2)
}

#[test]
fn a_recorded_fixture_reduces_to_a_trace_that_walks_its_curves() {
    let fixture = Fixture::new("trace", Scenario::Graduation);
    let trace = recorded_trace(&fixture);

    assert!(!trace.mints.is_empty(), "the recording opened no curve");
    assert!(trace.points() > trace.mints.len(), "no curve ever moved");
    for mint in &trace.mints {
        // Time never runs backwards inside one mint's history, which is what
        // makes an entry index and an exit index an ordering.
        for pair in mint.points.windows(2) {
            assert!(
                pair[1].at_ms >= pair[0].at_ms,
                "{}: {} then {}",
                mint.mint,
                pair[0].at_ms,
                pair[1].at_ms
            );
        }
        // A curve may cross the graduation line on the buy that completes it,
        // and then it stops: §17 makes a complete curve unquotable, so the
        // crossing observation is the last one there is. At most one point can
        // sit past the line, and it has to be the final one — a trace with two
        // would mean the walk kept filling a dead pool.
        let past: Vec<usize> = mint
            .points
            .iter()
            .enumerate()
            .filter(|(_, point)| point.real_sol_lamports > PUMP_GRADUATION_LAMPORTS)
            .map(|(index, _)| index)
            .collect();
        assert!(
            past.len() <= 1,
            "{}: {} observations past the graduation line",
            mint.mint,
            past.len()
        );
        if let Some(&index) = past.first() {
            assert_eq!(
                index,
                mint.points.len() - 1,
                "{}: the curve kept moving after it graduated",
                mint.mint
            );
        }
    }
    // Sorted by mint, so two runs over one recording produce one order.
    let mints: Vec<&str> = trace.mints.iter().map(|m| m.mint.as_str()).collect();
    let mut sorted = mints.clone();
    sorted.sort_unstable();
    assert_eq!(mints, sorted);
}

#[test]
fn attributing_one_recording_twice_produces_the_same_bytes() {
    // The property the whole attribution module is written for, asserted at the
    // level that can actually fail: a fixture off a real filesystem, decoded
    // through the real parser, priced through the real adversary.
    let fixture = Fixture::new("bytes", Scenario::Graduation);
    let trace = recorded_trace(&fixture);
    let rules = trace_rules();

    for profile in AdversaryProfile::ALL {
        let config = attribution_config(&fixture, profile);
        let first = attribute_trace(&trace, &rules, &config);
        let second = attribute_trace(&trace, &rules, &config);
        assert_eq!(first, second, "{profile:?}: two runs disagreed");
        assert_eq!(
            first.to_json(),
            second.to_json(),
            "{profile:?}: the bytes disagreed"
        );
        assert_eq!(first.schema, "sts.backtest.attribution.v2");
    }
}

#[test]
fn the_identity_and_the_fee_decomposition_close_over_a_real_recording() {
    // Graduation rather than SandwichBoundary: that scenario's mints are four
    // observations long, which is shorter than one round trip, so a corpus
    // built from it would be empty and every assertion below would pass
    // vacuously.
    let fixture = Fixture::new("identity", Scenario::Graduation);
    let trace = recorded_trace(&fixture);
    let rules = trace_rules();

    for profile in AdversaryProfile::ALL {
        let config = attribution_config(&fixture, profile);
        let report = attribute_trace(&trace, &rules, &config);
        assert!(
            !report.trades.is_empty(),
            "{profile:?}: the recording priced no trade"
        );

        // Every trade, the totals, the fee split, and the two views agreeing
        // with each other. `balances` is all four.
        assert!(report.balances(), "{profile:?}");
        for row in &report.trades {
            assert!(row.balances(), "{profile:?} {}", row.mint);
            assert!(
                row.residual_within_bound(),
                "{profile:?} {}: residual {} outside a bound of {}",
                row.mint,
                row.residual_lamports,
                row.residual_bound_lamports()
            );
        }
        assert!(report.fees.reconciles(&report.summary), "{profile:?}");
        assert_eq!(
            report.fees.protocol_lamports + report.fees.creator_lamports,
            report.fees.venue_lamports,
            "{profile:?}: the venue split lost a lamport"
        );
    }
}

#[test]
fn a_hostile_recording_costs_more_than_the_same_one_with_nobody_in_front() {
    // The whole point of the MEV line: the difference between a run against a
    // passive taker and the same run against somebody who acts is what being
    // second cost, and it has to be visible in the totals rather than only in
    // the per-leg detail.
    let fixture = Fixture::new("hostile", Scenario::Graduation);
    let trace = recorded_trace(&fixture);
    let rules = trace_rules();

    let passive = attribute_trace(
        &trace,
        &rules,
        &attribution_config(&fixture, AdversaryProfile::PassiveTaker),
    );
    let hostile = attribute_trace(
        &trace,
        &rules,
        &attribution_config(&fixture, AdversaryProfile::PredatorySandwich),
    );

    assert_eq!(
        passive.summary.mev_penalty_lamports, 0,
        "nobody was in front"
    );
    assert!(
        hostile.summary.mev_penalty_lamports > 0,
        "a funded sandwich found nothing to do at a size above §15.2's threshold"
    );
    assert!(hostile.summary.realized_pnl_lamports < passive.summary.realized_pnl_lamports);
    assert!(hostile.summary.total_cost_lamports > passive.summary.total_cost_lamports);

    // The venue's cut does not go *up*, and that is worth pinning rather than
    // assuming it is unchanged. An adversary in front of us does not raise the
    // fee rate — it shrinks the fill the rate is charged on, because a
    // front-run leaves our buy fewer tokens and the exit then sells fewer. So
    // a hostile run pays the venue slightly less while paying far more overall,
    // and a decomposition that showed the venue line rising under an adversary
    // would be charging the MEV penalty to the wrong line.
    assert!(
        hostile.fees.venue_lamports <= passive.fees.venue_lamports,
        "the venue took {} under an adversary against {} without one",
        hostile.fees.venue_lamports,
        passive.fees.venue_lamports
    );
    assert!(hostile.fees.reconciles(&hostile.summary));
    assert!(passive.fees.reconciles(&passive.summary));
    // Both runs priced the same corpus, so the comparison is like for like.
    assert_eq!(hostile.trades.len(), passive.trades.len());
    assert_eq!(hostile.mev.profile, AdversaryProfile::PredatorySandwich);
    assert!(hostile.mev.synthetic, "nothing in that line was observed");
}

// ===========================================================================
// pool reserve exhaustion
// ===========================================================================

#[test]
fn a_sell_of_more_than_the_pool_holds_is_refused_at_the_exact_lamport() {
    // The first of §17's no-executable-exit conditions, checked at its
    // boundary rather than near it. A simulator that is one lamport loose here
    // is one that reports exits nobody could have taken.
    let curve = CurveState::at_real_sol(10 * LAMPORTS_PER_SOL);
    let fee = DEFAULT_FEE_BPS;
    let available = curve.real_sol_reserves;

    // The largest parcel the pool can still pay for.
    let (tokens, fill) = curve
        .sell_tokens_for_target(available - available / 100 - 1, fee)
        .expect("a target inside the reserve is reachable");
    assert!(fill.gross_lamports <= available);
    assert_eq!(fill.net_lamports + fill.fee_lamports, fill.gross_lamports);

    // Doubling the parcel takes the gross past what the pool holds, and the
    // refusal names both numbers rather than clamping the fill.
    match curve.quote_sell(tokens.saturating_mul(4), fee) {
        Err(QuoteError::ExceedsRealSol {
            required,
            available: held,
        }) => {
            assert!(required > held);
            assert_eq!(held, available);
        }
        other => panic!("a sell past the reserve should be refused, got {other:?}"),
    }
}

#[test]
fn a_buy_of_more_than_the_pool_holds_in_tokens_is_refused() {
    // The other exhaustion condition. Near graduation there are almost no real
    // tokens left, so a buy that would need more of them is not a worse price,
    // it is not a fill.
    let curve = CurveState::at_real_sol(84 * LAMPORTS_PER_SOL);
    assert!(curve.real_token_reserves < CurveState::LAUNCH.real_token_reserves / 100);
    match curve.quote_buy(50 * LAMPORTS_PER_SOL, DEFAULT_FEE_BPS) {
        Err(QuoteError::ExceedsRealTokens {
            required,
            available,
        }) => {
            assert!(required > available);
            assert_eq!(available, curve.real_token_reserves);
        }
        other => panic!("a buy past the token reserve should be refused, got {other:?}"),
    }
}

#[test]
fn an_adversary_never_dumps_the_pool_dry_underneath_our_exit() {
    // The bisection in `sell_through` requires both the adversary's leg and
    // ours to be quotes the curve would honour, so what comes back is always a
    // fill we could actually have got — even when the profile has been handed
    // more capital than the pool has ever seen.
    for position in [1u64, 2, 5, 20, 60, 84] {
        let curve = CurveState::at_real_sol(position * LAMPORTS_PER_SOL);
        let mut config = AdversaryConfig::default()
            .with_profile(AdversaryProfile::PredatorySandwich)
            .bounded(10_000 * LAMPORTS_PER_SOL, BPS_DENOMINATOR as u16);
        config.landing_cost_lamports = 1_000;

        let context = MarketContext::at(&curve);
        let Ok(bought) = buy_through(&curve, LAMPORTS_PER_SOL / 20, &config, context) else {
            continue;
        };
        let Ok(sold) = sell_through(&curve, bought.filled_tokens, &config, context) else {
            continue;
        };

        assert!(
            sold.filled_gross_lamports > 0,
            "at {position} SOL: an exit that pays nothing"
        );
        assert!(
            sold.filled_gross_lamports <= curve.real_sol_reserves,
            "at {position} SOL: paid {} out of a pool holding {}",
            sold.filled_gross_lamports,
            curve.real_sol_reserves
        );
        assert_eq!(
            sold.net_lamports + sold.fee_lamports,
            sold.filled_gross_lamports
        );
        assert!(sold.penalty_lamports <= sold.solo_gross_lamports);
    }
}

#[test]
fn an_exhausted_pool_refuses_a_reorg_baseline_rather_than_pricing_one() {
    // A fork whose replayed flow drains the pool leaves our exit unexecutable.
    // That is a real outcome and it is not a penalty: the fate says the leg was
    // refused, and every lamport column stays at zero.
    let parcel = CurveState::at_real_sol(3 * LAMPORTS_PER_SOL)
        .quote_buy(LAMPORTS_PER_SOL / 2, DEFAULT_FEE_BPS)
        .expect("quote")
        .tokens;
    let scenario = ReorgScenario::untouched(3 * LAMPORTS_PER_SOL, Side::Sell, parcel, 1_000_000)
        .forked(4, -(3 * LAMPORTS_PER_SOL as i64), true);

    let outcome = simulate_reorg(&scenario, &AdversaryConfig::default());
    assert!(
        outcome.priced,
        "the branch we priced against was executable"
    );
    assert_eq!(outcome.fate, ReorgFate::Refused);
    assert_eq!(outcome.reorged_net_lamports, 0);
    assert_eq!(outcome.tip_refunded_lamports, scenario.tip_lamports);
}

#[test]
fn a_graduated_curve_is_a_hard_refusal_rather_than_a_worse_price() {
    // §17 makes graduation a branch, not a continuous transition, and every
    // entry point into the pipeline has to agree about that.
    let done = CurveState::at_real_sol(PUMP_GRADUATION_LAMPORTS);
    assert!(done.complete);
    let config = AdversaryConfig::default();
    let context = MarketContext::at(&done);

    assert_eq!(
        done.quote_buy(LAMPORTS_PER_SOL, DEFAULT_FEE_BPS),
        Err(QuoteError::CurveComplete)
    );
    assert_eq!(
        buy_through(&done, LAMPORTS_PER_SOL, &config, context),
        Err(QuoteError::CurveComplete)
    );
    assert_eq!(
        sell_through(&done, 1_000_000, &config, context),
        Err(QuoteError::CurveComplete)
    );
    // And a plan that ends there is reported as a refusal rather than dropped.
    let report = attribute_plans(
        &[RoundTripPlan {
            mint: "MintGraduated".to_string(),
            opened_at_ms: 1_000,
            closed_at_ms: 2_000,
            entry_real_sol_lamports: PUMP_GRADUATION_LAMPORTS,
            exit_real_sol_lamports: PUMP_GRADUATION_LAMPORTS,
            gross_lamports: LAMPORTS_PER_SOL,
            entry_ticks: Vec::new(),
            exit_ticks: Vec::new(),
        }],
        &AttributionConfig::default(),
    );
    assert!(report.trades.is_empty());
    assert_eq!(report.refusals.len(), 1);
    assert!(report.refusals[0].contains("MintGraduated"));
}

#[test]
fn a_venue_of_exhausted_pools_leaves_the_whole_purse_at_home() {
    // Every curve is a lamport short of graduating, so there are no real tokens
    // left to front-run into. The allocator has to report an idle purse rather
    // than an allocation nobody could have executed.
    let pools: Vec<PoolTarget> = ["MintOne", "MintTwo", "MintThree"]
        .iter()
        .map(|mint| PoolTarget::at_real_sol(mint, PUMP_GRADUATION_LAMPORTS - 1, LAMPORTS_PER_SOL))
        .collect();
    let mut config = AdversaryConfig::default()
        .with_profile(AdversaryProfile::PredatorySandwich)
        .bounded(50 * LAMPORTS_PER_SOL, BPS_DENOMINATOR as u16);
    config.base_intensity_micros = 1_000_000;

    let report = extract_across_pools(&pools, &config, DEFAULT_ALLOCATION_SLICES);
    assert_eq!(report.pools_offered, 3);
    assert_eq!(report.pools_attacked, 0);
    assert_eq!(report.capital_deployed_lamports, 0);
    assert_eq!(report.capital_idle_lamports, config.capital_lamports);
    assert!(report.balances());
}

// ===========================================================================
// extreme slippage bounds
// ===========================================================================

#[test]
fn slippage_saturates_at_a_hundred_percent_and_never_passes_it() {
    // The column is a `u16` of basis points and 10 000 is all of it. A quote
    // that would compute past that reports the whole of it rather than
    // wrapping, which is the pessimistic direction.
    assert_eq!(slippage_bps(0, 1, 0), 0);
    assert_eq!(slippage_bps(u128::MAX, 1, 0), BPS_DENOMINATOR as u16);
    // A zero reserve has no price to slip against, so the answer is the whole
    // of it rather than a division.
    assert_eq!(slippage_bps(1, 0, 0), BPS_DENOMINATOR as u16);
    // And a swap the size of the reserve is half of it plus the fee, rounded
    // up — the closed form, checked where it is easiest to read.
    assert_eq!(slippage_bps(1, 1, 0), 5_000);
    assert_eq!(slippage_bps(1, 1, 100), 5_050);

    // Monotone in size at a fixed reserve, which is what makes a bound on the
    // worst fill a bound on every fill under it.
    let mut previous = 0;
    for size in [1u128, 10, 100, 1_000, 10_000, 100_000, 1_000_000] {
        let bps = slippage_bps(size, 1_000_000, DEFAULT_FEE_BPS);
        assert!(bps >= previous, "slippage fell at a size of {size}");
        previous = bps;
    }
}

#[test]
fn an_enormous_order_against_a_thin_curve_stays_inside_the_column() {
    // The extreme this pipeline actually has to survive: a curve with almost
    // nothing in it and an order far larger than it.
    let thin = CurveState::at_real_sol(LAMPORTS_PER_SOL / 100);
    let config = AdversaryConfig::default();
    let context = MarketContext::at(&thin);

    for size in [
        LAMPORTS_PER_SOL,
        100 * LAMPORTS_PER_SOL,
        10_000 * LAMPORTS_PER_SOL,
    ] {
        match buy_through(&thin, size, &config, context) {
            Ok(fill) => {
                assert!(fill.slippage_bps <= BPS_DENOMINATOR as u16);
                assert!(fill.penalty_bps <= BPS_DENOMINATOR as u16);
                // On a buy the notional is what we committed, gross.
                assert_eq!(
                    fill.net_lamports + fill.fee_lamports,
                    fill.notional_lamports
                );
                assert!(fill.filled_tokens <= thin.real_token_reserves);
            }
            // A refusal is the other correct answer, and the only two.
            Err(QuoteError::ExceedsRealTokens { .. }) => {}
            other => panic!("a huge buy on a thin curve gave {other:?}"),
        }
    }
}

#[test]
fn no_modelled_penalty_ever_leaves_the_basis_point_column() {
    // Swept across the whole curve, both sides, every profile, at sizes from
    // dust to far past what the pool holds. The claim is not that these numbers
    // are right — it is that none of them is outside the column that carries
    // them, which is what stops a report quoting a 70 000 bps cost.
    let positions = [1u64, 5, 20, 45, 70, 84];
    let sizes = [
        1_000u64,
        10_000,
        LAMPORTS_PER_SOL / 100,
        LAMPORTS_PER_SOL,
        50 * LAMPORTS_PER_SOL,
    ];
    let contexts = [
        MarketContext::default(),
        MarketContext {
            progress_bps: 9_999,
            volatility_micros: 1_000_000,
        },
    ];

    for profile in AdversaryProfile::ALL {
        let mut config = AdversaryConfig::default()
            .with_profile(profile)
            .bounded(100 * LAMPORTS_PER_SOL, BPS_DENOMINATOR as u16);
        config.landing_cost_lamports = 1_000;

        for position in positions {
            let curve = CurveState::at_real_sol(position * LAMPORTS_PER_SOL);
            for size in sizes {
                for context in contexts {
                    if let Ok(bought) = buy_through(&curve, size, &config, context) {
                        assert!(bought.slippage_bps <= BPS_DENOMINATOR as u16);
                        assert!(bought.penalty_bps <= BPS_DENOMINATOR as u16);
                        assert!(bought.penalty_lamports <= bought.notional_lamports);
                        assert!(bought.filled_tokens <= bought.solo_tokens);

                        if let Ok(sold) =
                            sell_through(&curve, bought.filled_tokens, &config, context)
                        {
                            assert!(sold.slippage_bps <= BPS_DENOMINATOR as u16);
                            assert!(sold.penalty_bps <= BPS_DENOMINATOR as u16);
                            assert!(sold.filled_gross_lamports <= sold.solo_gross_lamports);
                            assert!(sold.penalty_lamports <= sold.solo_gross_lamports);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn the_slippage_histogram_accounts_for_every_leg_including_the_worst() {
    // A distribution that dropped its tail would hide exactly the fills a size
    // limit is set from. The top bucket's edge is 10 000, and slippage
    // saturates there, so every sample has a bucket by construction.
    let fixture = Fixture::new("slippage", Scenario::Graduation);
    let trace = recorded_trace(&fixture);
    let report = attribute_trace(
        &trace,
        &trace_rules(),
        &attribution_config(&fixture, AdversaryProfile::PredatorySandwich),
    );
    assert!(
        !report.trades.is_empty(),
        "an empty histogram proves nothing"
    );

    let counted: u32 = report
        .slippage
        .buckets
        .iter()
        .map(|bucket| bucket.count)
        .sum();
    assert_eq!(counted, report.slippage.samples);
    assert_eq!(report.slippage.samples as usize, report.trades.len() * 2);
    assert!(report.slippage.max_bps <= BPS_DENOMINATOR as u16);
    assert!(report.slippage.min_bps <= report.slippage.p50_bps);
    assert!(report.slippage.p50_bps <= report.slippage.p90_bps);
    assert!(report.slippage.p90_bps <= report.slippage.p99_bps);
    assert!(report.slippage.p99_bps <= report.slippage.max_bps);
    // The worst single leg is the worst of the per-trade columns, not an
    // average of them.
    let worst = report
        .trades
        .iter()
        .map(|row| row.worst_slippage_bps)
        .max()
        .unwrap_or(0);
    assert_eq!(report.slippage.max_bps, worst);
}

#[test]
fn a_tighter_ceiling_never_admits_a_worse_fill() {
    // The bound shrinks the adversary rather than clipping the damage, so
    // tightening it can only make every reported fill better. Swept, because
    // the property is about the whole ladder rather than one pair of points.
    let curve = CurveState::at_real_sol(30 * LAMPORTS_PER_SOL);
    let context = MarketContext::at(&curve);
    let mut previous_penalty = u64::MAX;
    let mut previous_tokens = 0u64;

    for ceiling in [BPS_DENOMINATOR as u16, 2_000, 1_500, 500, 100, 25, 5, 0] {
        let mut config = AdversaryConfig::default()
            .with_profile(AdversaryProfile::PredatorySandwich)
            .bounded(20 * LAMPORTS_PER_SOL, ceiling);
        config.landing_cost_lamports = 1_000_000;

        let fill = buy_through(&curve, LAMPORTS_PER_SOL, &config, context).expect("buy");
        assert!(
            fill.penalty_bps <= ceiling,
            "{} bps over a {ceiling} bps ceiling",
            fill.penalty_bps
        );
        assert!(
            fill.penalty_lamports <= previous_penalty,
            "tightening from the previous ceiling to {ceiling} made the penalty worse"
        );
        assert!(
            fill.filled_tokens >= previous_tokens,
            "tightening to {ceiling} gave us fewer tokens"
        );
        previous_penalty = fill.penalty_lamports;
        previous_tokens = fill.filled_tokens;
    }
    // At a ceiling of zero the adversary cannot act at all, and the report says
    // so rather than reporting a clean fill.
    assert_eq!(previous_penalty, 0);
}

// ===========================================================================
// the two properties the pipeline is written for
// ===========================================================================

#[test]
fn every_report_struct_in_the_pipeline_compares_by_equality() {
    // `Eq` rather than `PartialEq` alone is what makes the equivalence gate a
    // one-line assertion: two reports either are the same report or they are
    // not, with no `f64` in the tree to make a third answer possible. This
    // fails to compile rather than fails to pass if a later edit puts a float
    // into any of them.
    //
    // Asserted here as well as inside the crate because these are the types
    // that cross the module boundary, and a bound that only holds where it was
    // declared is not one a consumer can rely on.
    fn comparable<T: Eq>() {}

    comparable::<AttributionReport>();
    comparable::<AttributionSummary>();
    comparable::<AttributionConfig>();
    comparable::<ReturnSummary>();
    comparable::<SlippageDistribution>();
    comparable::<SlippageBucket>();
    comparable::<TradeAttribution>();
    comparable::<TradeExecution>();
    comparable::<ExecutionLeg>();
    comparable::<TipSchedule>();
    comparable::<RoundTripPlan>();

    comparable::<FeeDecomposition>();
    comparable::<FeeSchedule>();
    comparable::<TradeFees>();
    comparable::<LegFees>();

    comparable::<ReplayTrace>();
    comparable::<MintTrace>();
    comparable::<TracePoint>();
    comparable::<TraceRules>();

    comparable::<MevOutcome>();
    comparable::<MevSummary>();
    comparable::<AdversaryConfig>();
    comparable::<AdversaryProfile>();
    comparable::<MarketContext>();
    comparable::<FrontRunCost>();

    comparable::<ReorgScenario>();
    comparable::<ReorgOutcome>();
    comparable::<ReorgSummary>();
    comparable::<ReorgFate>();
    comparable::<ReorgGrid>();

    comparable::<PoolTarget>();
    comparable::<PoolExtraction>();
    comparable::<MultiPoolExtraction>();
}

#[test]
fn no_part_of_the_simulation_pipeline_computes_in_floating_point() {
    // The same scan `attribution.rs` runs over itself, run from outside the
    // crate so it covers the pipeline as a unit rather than one module at a
    // time — and extended past `mev_sim` and `attribution` to the two files
    // they price through.
    //
    // `backtest.rs` and `replay.rs` are *not* float-free and are not asked to
    // be: `replay::best_front_run` builds its grid with `f64::powf` and
    // `DrawSource::unit` scales a draw through an `f64`. Both have integer
    // counterparts, and what this asserts is that the pipeline calls those —
    // which is a stronger claim than "these two files contain no float", and
    // the one that actually keeps a report reproducible.
    const FLOAT_BEARING_CALLS: [&str; 2] = ["best_front_run(", ".unit("];

    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders: Vec<String> = Vec::new();
    let mut scanned = 0;

    for name in ["mev_sim.rs", "attribution.rs"] {
        let path = src.join(name);
        let source = std::fs::read_to_string(&path).expect("readable source");
        // Above the test line only. The tests below it name the types they are
        // looking for and would otherwise find themselves.
        let code = source
            .split("#[cfg(test)]")
            .next()
            .expect("split yields one");
        assert!(code.len() > 10_000, "{name}: the scan lost the module body");
        scanned += 1;

        for (number, line) in code.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            if trimmed.contains("f64") || trimmed.contains("f32") {
                offenders.push(format!("{name}:{}: {trimmed}", number + 1));
            }
            for call in FLOAT_BEARING_CALLS {
                if trimmed.contains(call) {
                    offenders.push(format!(
                        "{name}:{}: reaches a float through {call} — {trimmed}",
                        number + 1
                    ));
                }
            }
        }
    }

    assert_eq!(scanned, 2, "the float-free surface changed shape");
    assert!(
        offenders.is_empty(),
        "floating point crept in:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_reorg_sweep_is_reproducible_and_its_fates_partition() {
    // High throughput and determinism are one property here: a sweep worth
    // running is one whose failures can be reproduced from the seed rather than
    // by replaying it.
    let mut config = AdversaryConfig::default()
        .with_profile(AdversaryProfile::PredatorySandwich)
        .bounded(20 * LAMPORTS_PER_SOL, DEFAULT_MAX_PENALTY_BPS);
    config.landing_cost_lamports = 1_000_000;

    let grid = ReorgGrid::standard();
    let scenarios = grid.scenarios(&config);
    assert!(
        scenarios.len() > 500,
        "a sweep of {} is not a sweep",
        scenarios.len()
    );

    let first = sweep_reorgs(&scenarios, &config);
    let second = sweep_reorgs(&scenarios, &config);
    assert_eq!(first, second);

    assert_eq!(
        first.untouched + first.reincluded + first.dropped + first.refused + first.unpriced,
        first.scenarios
    );
    assert!(first.adverse + first.favourable <= first.scenarios);
    assert!(first.dropped > 0, "a grid with a drop rate dropped nothing");
    assert!(first.adverse > 0, "a grid with forks in it cost nothing");
    assert!(first.synthetic);

    // A sampled sweep is addressed by its seed, so a failure at any index
    // reproduces without replaying the ones before it.
    assert_eq!(grid.sampled(&config, 7, 250), grid.sampled(&config, 7, 250));
    assert_ne!(grid.sampled(&config, 7, 250), grid.sampled(&config, 8, 250));
}
