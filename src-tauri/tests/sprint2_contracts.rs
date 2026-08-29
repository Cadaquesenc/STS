//! The Sprint 2 seams, where the workstreams meet.
//!
//! Each module that landed this sprint has its own suite: `geyser_tests.rs`
//! covers the tick pipeline, `journal_alerting.rs` the book and the alert
//! engine, `forensics_tests.rs` the clustering. What none of them can see is
//! the property that only exists where two of them touch — a price that is
//! exact in `subslot` and rounded by the time the journal stores it, a tick the
//! chain took back that the book kept, a shutdown that drops the last alert.
//!
//! So these are the contracts across the seams, and nothing here asserts
//! anything a single-module test already asserts.
//!
//! Ordered as a tick travels:
//!
//! 1. **geyser → subslot.** Out-of-order arrivals leave in chain order, and a
//!    re-org takes back exactly the ticks at or above the dead slot.
//! 2. **subslot → journal.** The unit survives the crossing: a price built at
//!    `10^-18` in the pipeline is the same number the book returns, and a
//!    rolled-back tick never reaches the book at all.
//! 3. **journal → alerting.** One bad fill is one alert, the book and the feed
//!    agree about it, and a refusal upstream stops both.
//! 4. **everything → shutdown.** What was accepted before a stop is delivered
//!    by it — the property both shutdown paths got wrong in the same way.
//!
//! Two things this file deliberately does not cover. The UI's `revision`
//! counter and its stale-status handling live on `feat/ui-cockpit-wiring`,
//! which is not merged here — see the integration report. And the `geyser-grpc`
//! wire is exercised by `geyser_tests.rs` under that feature; what is asserted
//! here is the contract that matters to everything else, which is that the
//! pipeline is whole without it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use sts_lib::alerting::{AlertDispatcher, Observation};
use sts_lib::db::{Database, ExecutionMode, Side};
use sts_lib::fixed::{e18_to_micros, ratio_e18, Q18, Q18_ONE};
use sts_lib::journal::{FillRow, JournalFilter, SignatureStatus, TradeRow};
use sts_lib::subslot::{
    Commitment, LedgerChange, ReorgReason, RingConfig, SlotLedger, SlotPhase, TickClass, TickKey,
    TickRing,
};
use sts_lib::telemetry::{TelemetryEvent, TelemetryHub, TelemetryLevel, TelemetrySink};

const MINT: &str = "So11111111111111111111111111111111111111112";
const AT_MS: i64 = 1_700_000_000_000;

/// A file of its own per test, removed when the test ends.
struct TempDb(std::path::PathBuf);

impl TempDb {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "sts-sprint2-{name}-{}-{}.db",
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

/// Everything the hub handed out, in the order it handed it out.
#[derive(Default)]
struct Recorder(Mutex<Vec<TelemetryEvent>>);

impl TelemetrySink for Recorder {
    fn deliver(&self, event: &TelemetryEvent) {
        self.0.lock().push(event.clone());
    }
}

/// What the ring is asked to carry here.
///
/// The ring's contract is two questions wide on purpose — see [`TickClass`] —
/// so a test of *ordering* carries an integer rather than a bonding curve, and
/// nothing in these assertions depends on what a tick means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tick(u64);

impl TickClass for Tick {
    fn is_protected(&self) -> bool {
        false
    }

    fn priority(&self) -> u8 {
        0
    }
}

/// A slot ledger with `count` slots confirmed in a straight line from 1.
fn confirmed_chain(count: u64) -> SlotLedger {
    let mut ledger = SlotLedger::new();
    for slot in 1..=count {
        ledger.observe(slot, Some(slot - 1), SlotPhase::Confirmed);
    }
    ledger
}

// ---------------------------------------------------------------------------
// 1. geyser -> subslot
// ---------------------------------------------------------------------------

#[test]
fn ticks_that_arrive_in_any_order_leave_in_chain_order() {
    // The seam's whole promise. A provider may deliver a slot's writes in any
    // order it likes; what the strategy reads has to be the chain's order, or
    // a price ladder is built on a sequence that never happened.
    let mut ring: TickRing<Tick> = TickRing::new(RingConfig {
        capacity: 64,
        hold_slots: 4,
    });
    let ledger = confirmed_chain(12);

    // Deliberately scrambled across slots and within them.
    let arrivals = [
        (TickKey::new(3, 900, 2, 0), Tick(32)),
        (TickKey::new(1, 100, 1, 0), Tick(10)),
        (TickKey::new(3, 100, 1, 0), Tick(31)),
        (TickKey::new(2, 500, 9, 0), Tick(21)),
        (TickKey::new(1, 300, 4, 0), Tick(12)),
        (TickKey::new(1, 200, 2, 0), Tick(11)),
        (TickKey::new(2, 100, 1, 0), Tick(20)),
    ];
    for (key, payload) in arrivals {
        ring.push(key, payload);
    }

    let mut out = Vec::new();
    ring.drain_ready(&ledger, Commitment::Confirmed, &mut out);
    assert_eq!(
        out,
        vec![
            Tick(10),
            Tick(11),
            Tick(12),
            Tick(20),
            Tick(21),
            Tick(31),
            Tick(32)
        ],
        "the ring released ticks in an order the chain never had",
    );
    assert!(
        ring.metrics().out_of_order_arrivals > 0,
        "the scrambling was not even noticed, so this test is not testing it",
    );
}

#[test]
fn a_reorg_takes_back_the_dead_slot_and_everything_above_it_and_nothing_below() {
    // The boundary is the thing worth pinning. One slot too few leaves a tick
    // from a fork that lost; one too many throws away settled history.
    let mut ring: TickRing<Tick> = TickRing::new(RingConfig {
        capacity: 64,
        hold_slots: 64,
    });
    for slot in 1..=6u64 {
        ring.push(TickKey::new(slot, 0, 0, 0), Tick(slot * 10));
    }

    let mut ledger = confirmed_chain(6);
    let change = ledger.observe(4, Some(3), SlotPhase::Dead);
    let LedgerChange::Reorg { from_slot, reason } = change else {
        panic!("a dead slot is a re-org, not {change:?}");
    };
    assert_eq!(from_slot, 4);
    assert_eq!(reason, ReorgReason::DeadSlot);

    let rollback = ring.rollback(from_slot);
    assert_eq!(
        rollback.discarded,
        vec![Tick(40), Tick(50), Tick(60)],
        "the wrong side of the fork was kept"
    );
    assert_eq!(
        rollback.released, None,
        "nothing had been released yet, so nothing escaped the rollback",
    );

    let mut out = Vec::new();
    ring.drain_ready(&ledger, Commitment::Confirmed, &mut out);
    assert_eq!(
        out,
        vec![Tick(10), Tick(20), Tick(30)],
        "a slot below the fork was taken back with it"
    );
}

#[test]
fn a_reorg_below_the_watermark_reports_what_had_already_escaped() {
    // The case the buffer cannot undo, and the one it must therefore be loud
    // about: the ticks are already downstream, and a caller that treated this
    // as an ordinary rollback would leave the book holding a dead fork.
    let mut ring: TickRing<Tick> = TickRing::new(RingConfig {
        capacity: 64,
        hold_slots: 1,
    });
    let ledger = confirmed_chain(8);
    for slot in 1..=4u64 {
        ring.push(TickKey::new(slot, 0, 0, 0), Tick(slot * 10));
    }
    let mut out = Vec::new();
    ring.drain_ready(&ledger, Commitment::Confirmed, &mut out);
    assert_eq!(
        out,
        vec![Tick(10), Tick(20), Tick(30), Tick(40)],
        "nothing was released, so there is no watermark to test"
    );

    let rollback = ring.rollback(3);
    assert!(
        rollback.discarded.is_empty(),
        "there was nothing resident left to discard"
    );
    assert_eq!(
        rollback.released,
        Some(4),
        "the rollback reached past what the ring had released and did not say so",
    );
}

// ---------------------------------------------------------------------------
// 2. subslot -> journal
// ---------------------------------------------------------------------------

#[test]
fn a_price_built_in_the_pipeline_is_the_price_the_book_returns() {
    // The unit crossing that the two workstreams invented separately: the tick
    // pipeline computes at `10^-18` and the journal stores at `10^-18`, and
    // this asserts the number survives the file rather than the two agreeing
    // only in their own tests.
    //
    // A real pump.fun curve: 30 SOL against 1.073e15 base units.
    let lamports = 30u128 * 1_000_000_000;
    let tokens = 1_073_000_000_000_000u128;
    let from_pipeline = ratio_e18(lamports, tokens).expect("a real curve prices");

    let temp = TempDb::new("price-crossing");
    let db = temp.open();
    db.record_journal_trades(&[TradeRow::opened(
        "t-1",
        MINT,
        Side::Buy,
        ExecutionMode::Live,
        1_000,
        AT_MS,
    )])
    .expect("the book takes the trade");
    let fill = FillRow::settle(
        "t-1",
        0,
        tokens as u64,
        lamports as u64,
        0,
        lamports as u64,
        250,
        AT_MS,
    )
    .expect("a real fill");
    assert_eq!(
        fill.price,
        Q18::from_raw(from_pipeline),
        "the journal and the tick pipeline price the same curve differently",
    );
    db.record_journal_fills(&[fill])
        .expect("the book takes the fill");
    db.close();

    // Read back through a plain connection: the column holds the integer, not
    // a float, and it is the integer the pipeline computed.
    let conn = rusqlite::Connection::open(&temp.0).expect("the file opens");
    let (raw, kind): (i64, String) = conn
        .query_row(
            "SELECT price_q18, typeof(price_q18) FROM journal_fills WHERE trade_id = 't-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("the fill is in the file");
    assert_eq!(
        kind, "integer",
        "a price reached the file as something other than an integer"
    );
    assert_eq!(raw as u128, from_pipeline);

    // And the crossing down to millionths is the one both modules use.
    assert_eq!(
        e18_to_micros(from_pipeline),
        Q18::from_raw(from_pipeline).to_micros_floor() + 1
    );
}

#[test]
fn one_is_one_in_every_unit_the_sprint_added() {
    // Four workstreams each grew a face on the same `10^-18` unit. This is the
    // assertion that they are faces and not four units: if any of them ever
    // becomes a different number, every price that crosses that seam is wrong
    // by a factor nobody will notice until it is in the book.
    assert_eq!(Q18_ONE, sts_lib::fixed::ONE);
    assert_eq!(sts_lib::fixed::ONE_E18, sts_lib::fixed::ONE);
    assert_eq!(Q18::ONE.raw(), sts_lib::fixed::ONE);
    assert_eq!(sts_lib::fixed::Fixed::ONE.to_micros(), 1_000_000);
    assert_eq!(e18_to_micros(sts_lib::fixed::ONE), 1_000_000);
}

#[test]
fn the_same_fill_offered_twice_is_recorded_once() {
    // The duplicate the network actually produces: a rebroadcast confirms, the
    // engine books it, and the original confirmation arrives afterwards. The
    // book has to be idempotent about that or a position doubles on paper.
    let temp = TempDb::new("duplicate-fill");
    let db = temp.open();
    db.record_journal_trades(&[TradeRow::opened(
        "t-1",
        MINT,
        Side::Buy,
        ExecutionMode::Live,
        1_000,
        AT_MS,
    )])
    .expect("the book takes the trade");

    let fill =
        FillRow::settle("t-1", 0, 1_000_000, 500_000, 0, 500_000, 250, AT_MS).expect("a fill");
    let first = db
        .record_journal_fills(std::slice::from_ref(&fill))
        .expect("the first is taken");
    assert_eq!(first, 1);

    let second = db
        .record_journal_fills(&[fill])
        .expect("the second is not an error");
    assert_eq!(second, 0, "the same fill was recorded twice");

    db.close();
    let conn = rusqlite::Connection::open(&temp.0).expect("the file opens");
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM journal_fills WHERE trade_id = 't-1'",
            [],
            |row| row.get(0),
        )
        .expect("counts");
    assert_eq!(count, 1);
}

#[test]
fn a_journal_written_by_this_build_reopens_with_every_table_the_sprint_added() {
    // Migration compatibility across the seam rather than within it: the
    // journal's tables and the ledgers that predate them have to live in one
    // file, under one migration chain, and survive a close and a reopen with
    // the same contents.
    let temp = TempDb::new("reopen");
    {
        let db = temp.open();
        db.record_journal_trades(&[TradeRow::opened(
            "t-1",
            MINT,
            Side::Buy,
            ExecutionMode::Live,
            1_000,
            AT_MS,
        )])
        .expect("writes");
        db.close();
    }

    let db = temp.open();
    let rows = db.query_journal(&JournalFilter::default()).expect("reads");
    assert_eq!(rows.len(), 1, "the book did not survive the reopen");
    assert_eq!(rows[0].trade_id, "t-1");
    db.close();

    let conn = rusqlite::Connection::open(&temp.0).expect("the file opens");
    // One chain, and the journal is on the end of it.
    let versions: Vec<i64> = conn
        .prepare("SELECT version FROM schema_migrations ORDER BY version")
        .expect("prepares")
        .query_map([], |row| row.get(0))
        .expect("runs")
        .collect::<Result<_, _>>()
        .expect("reads");
    // Five now, not three: the sprint added 4 "integers everywhere" (the REAL
    // columns became INTEGER millionths, which is what deleted store_unit and
    // the last float on the storage path) and 5 "forensic log and journal
    // snapshots".
    assert_eq!(versions, vec![1, 2, 3, 4, 5]);
}

// ---------------------------------------------------------------------------
// 3. journal -> alerting
// ---------------------------------------------------------------------------

#[test]
fn a_fill_the_book_calls_bad_is_the_fill_the_feed_alerts_on() {
    // The two modules compute slippage from the same `FillRow`, and this is the
    // assertion that they read it the same way — the number in the book and the
    // number in the alert are one number, not two that happen to agree today.
    let hub = Arc::new(TelemetryHub::start());
    let dispatcher = AlertDispatcher::new(Arc::clone(&hub));
    let thresholds = dispatcher.thresholds();

    let quoted = 1_000_000u64;
    let bps = thresholds.slippage_bps + 50;
    let filled = quoted - u64::from(bps) * quoted / 10_000;
    let fill = FillRow::settle("t-1", 0, 1_000_000, filled, 0, quoted, 250, AT_MS).expect("a fill");

    let fired = dispatcher.observe(
        &Observation::Filled {
            trade_id: "t-1",
            mint: MINT,
            mode: ExecutionMode::Live,
            fill: &fill,
            route_bound_bps: 0,
        },
        AT_MS,
    );
    assert_eq!(fired.len(), 1, "a fill past the threshold did not alert");
    assert_eq!(
        u64::from(fill.slippage_bps),
        fired[0].observed,
        "the book and the feed disagree about how bad the fill was",
    );

    dispatcher.shutdown();
    hub.shutdown();
}

#[test]
fn a_fill_that_does_not_exist_raises_nothing_anywhere() {
    // The refusal cascade. `FillRow::settle` refuses a fill of no tokens, and
    // the point is that the refusal stops there: nothing downstream is given a
    // zero to alert on or a zero to store, because a fill that did not happen
    // is not a fill of nothing.
    assert!(
        FillRow::settle("t-1", 0, 0, 500_000, 0, 500_000, 250, AT_MS).is_none(),
        "a fill of no tokens was accepted, and everything downstream now has a price of infinity",
    );

    let hub = Arc::new(TelemetryHub::start());
    let dispatcher = AlertDispatcher::new(Arc::clone(&hub));
    let before = dispatcher.snapshot();
    // Nothing to observe, because nothing was built. The cascade is that the
    // engine never reaches the dispatcher at all.
    let after = dispatcher.snapshot();
    assert_eq!(before.raised, after.raised);
    dispatcher.shutdown();
    hub.shutdown();
}

#[test]
fn a_threshold_the_engine_would_not_honour_is_refused_rather_than_stored() {
    // The other refusal cascade: a bad configuration is rejected at the edge,
    // so no later observation is held against thresholds that never made sense.
    let hub = Arc::new(TelemetryHub::start());
    let dispatcher = AlertDispatcher::new(Arc::clone(&hub));
    let good = dispatcher.thresholds();

    let mut bad = good;
    bad.slippage_bps = 10_001;
    assert!(
        dispatcher.set_thresholds(bad).is_err(),
        "a slippage past 100% was accepted"
    );
    assert_eq!(
        dispatcher.thresholds(),
        good,
        "a refused threshold was stored anyway, so the refusal was cosmetic",
    );

    dispatcher.shutdown();
    hub.shutdown();
}

// ---------------------------------------------------------------------------
// 4. everything -> shutdown
// ---------------------------------------------------------------------------

#[test]
fn a_telemetry_shutdown_delivers_what_it_had_already_accepted() {
    // The regression for the race this integration found. `select!` picks at
    // random between two ready arms, so a shutdown arriving while events are
    // queued could win and drop them — silently, and only sometimes, which is
    // the worst shape a bug can have. Queue a burst and stop immediately.
    for attempt in 0..40 {
        let hub = Arc::new(TelemetryHub::start());
        let recorder = Arc::new(Recorder::default());
        hub.observe(Arc::clone(&recorder) as Arc<dyn TelemetrySink>);

        for index in 0..16 {
            hub.publish(
                TelemetryLevel::Info,
                "contract",
                format!("event {index}"),
                serde_json::json!({ "index": index }),
            );
        }
        hub.shutdown();

        let seen = recorder.0.lock().len();
        assert_eq!(
            seen,
            16,
            "attempt {attempt}: the shutdown dropped {} events it had accepted",
            16 - seen,
        );
    }
}

#[test]
fn an_alert_shutdown_does_not_forget_what_it_could_not_send() {
    // The same race on the alerting side, where the drain has to be bounded
    // because each delivery is a POST. The endpoint here never answers, so the
    // drain runs out its deadline — and what it could not deliver has to show
    // up as failed rather than vanish.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
    let address = listener.local_addr().expect("has an address");
    listener.set_nonblocking(true).expect("non-blocking");
    // Accept and hold, so every POST times out rather than being refused.
    //
    // Non-blocking with a deadline rather than a blocking `accept`: a test
    // server that waits for a connection the code under test decided not to
    // make hangs for ever instead of failing, which is exactly the shape of the
    // bug this file is the regression for.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server = std::thread::spawn({
        let stop = Arc::clone(&stop);
        move || {
            let mut held = Vec::new();
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => held.push(stream),
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
            held.len()
        }
    });

    let hub = Arc::new(TelemetryHub::start());
    let dispatcher = AlertDispatcher::new(Arc::clone(&hub));
    let sink = dispatcher
        .attach_webhook(&sts_lib::alerting::WebhookConfig {
            url: format!("http://{address}/hook"),
            timeout_ms: 150,
            queue_depth: 8,
            ..Default::default()
        })
        .expect("starts");

    let mut thresholds = dispatcher.thresholds();
    thresholds.cooldown_ms = 0;
    dispatcher
        .set_thresholds(thresholds)
        .expect("a legal threshold");

    for index in 0..4 {
        dispatcher.observe(
            &Observation::Settled {
                trade_id: "t-1",
                mint: MINT,
                mode: ExecutionMode::Live,
                status: SignatureStatus::Failed,
                elapsed_ms: 1,
                rebroadcasts: 0,
            },
            AT_MS + index,
        );
    }

    dispatcher.shutdown();
    let stats = sink.stats();
    assert_eq!(
        stats.queued,
        stats.delivered + stats.failed + stats.dropped,
        "an alert the sink accepted is in none of its counters: {stats:?}",
    );

    hub.shutdown();
    stop.store(true, Ordering::Relaxed);
    let _ = server.join();
}

#[test]
fn the_pipeline_is_whole_without_the_grpc_feature() {
    // `geyser-grpc` adds a wire, not a pipeline. Everything the rest of the
    // engine depends on — the ordering ring, the ledger, the tick types and the
    // decode path — has to be present in a default build, or a feature flag has
    // quietly become a requirement.
    let mut ring: TickRing<Tick> = TickRing::new(RingConfig::default());
    let ledger = confirmed_chain(4);
    ring.push(TickKey::new(1, 0, 0, 0), Tick(7));
    let mut out = Vec::new();
    ring.drain_ready(&ledger, Commitment::Confirmed, &mut out);
    assert_eq!(out, vec![Tick(7)]);

    // And the decoder the wire would feed is reachable without it.
    assert_eq!(
        sts_lib::geyser::parse_raw_amount("1073000000000000").expect("parses"),
        1_073_000_000_000_000
    );
}
