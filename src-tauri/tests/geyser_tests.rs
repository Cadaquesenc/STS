//! The tick pipeline from outside the crate.
//!
//! `geyser.rs` and `subslot.rs` carry unit tests for their own parts. What is
//! here is the two things a unit test cannot say: that the invariant holds
//! across the whole module rather than at the places somebody remembered to
//! assert it, and that the public surface is usable by a caller who has only
//! the public surface.

use std::time::Duration;

use sts_lib::geyser::GeyserError;
use sts_lib::geyser::{
    parse_raw_amount, AccountUpdate, CurveTick, DecodeError, GeyserConfig, GeyserFeed,
    GeyserMetrics, GeyserUpdate, IngestionSink, ReconnectPolicy, SlotUpdate, TickEvent,
    TickPayload, TickPipeline, TickSink, TransactionUpdate, UpdatePayload,
};
use sts_lib::ingestion::{
    BondingCurve, DropReason, FeedProvider, IngestionConfig, IngestionManager, IngestionStreams,
    Route, SolPrice, Verdict, WebSocketDialer,
};
use sts_lib::loadgen::{run_load, LoadConfig, LoadTransport, MockGeyser};
use sts_lib::strategy::fixed::{format_e18, ratio_e18};
use sts_lib::subslot::{Commitment, RingConfig, SlotPhase, TickKey};
use sts_lib::telemetry::{TelemetryHub, TelemetryLevel};
use sts_lib::types::{Pubkey, Signature};

// ===========================================================================
// the zero-float invariant, enforced at the source
// ===========================================================================

/// No file in the tick pipeline computes in floating point.
///
/// The same source-level check `strategy_tests.rs` makes about the strategy
/// module, for the same reason and one more. The reason: every assertion in
/// this file compares integers against published figures, and all of them stay
/// true if somebody quietly computes an intermediate in `f64` — right up until
/// two machines disagree in the last bit.
///
/// The extra reason is specific to this pipeline. The Geyser wire format hands
/// every SPL balance over as a `ui_amount: f64` sitting directly beside the raw
/// integer, so here the float is not something a careless author would have to
/// introduce — it is already in scope, one field access away, and it is the
/// obvious thing to reach for. A rule that convenient to break is one worth
/// checking mechanically.
#[test]
fn nothing_in_the_tick_pipeline_computes_in_floating_point() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders: Vec<String> = Vec::new();

    for name in ["geyser.rs", "subslot.rs", "loadgen.rs"] {
        let path = root.join(name);
        let source = std::fs::read_to_string(&path).expect("the pipeline source is there");

        // Everything below the first `#[cfg(test)]` is test code, where a float
        // is allowed: the tests cross-check fixed-point answers against the
        // arithmetic they replace, which is the one place a float belongs.
        let code = source
            .split("#[cfg(test)]")
            .next()
            .expect("split yields one");

        for (number, line) in code.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            if trimmed.contains("f64") || trimmed.contains("f32") {
                offenders.push(format!("{name}:{}: {trimmed}", number + 1));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "floating point crept in:\n{}",
        offenders.join("\n")
    );
}

/// The pipeline's own types cannot hold a float, and the compiler is what says
/// so.
///
/// `f64` is not `Eq`. Every event type deriving `Eq` therefore makes a float
/// field a compile error rather than a runtime surprise, which is why the
/// derive is a design decision and not a convenience.
#[test]
fn every_event_type_carries_the_eq_bound_that_bans_the_float() {
    fn assert_eq_bound<T: Eq>() {}
    assert_eq_bound::<TickEvent>();
    assert_eq_bound::<TickPayload>();
    assert_eq_bound::<CurveTick>();
    assert_eq_bound::<TickKey>();
    assert_eq_bound::<GeyserUpdate>();
    assert_eq_bound::<UpdatePayload>();
    assert_eq_bound::<AccountUpdate>();
    // The readout the window polls, and the load generator's own numbers. A
    // benchmark that reported a float would be a benchmark whose result two
    // machines could disagree about, so the same bound guards it.
    assert_eq_bound::<sts_lib::geyser::GeyserSnapshot>();
    assert_eq_bound::<sts_lib::subslot::RingMetrics>();
    assert_eq_bound::<LoadConfig>();
    assert_eq_bound::<sts_lib::loadgen::LoadReport>();
    assert_eq_bound::<sts_lib::loadgen::GeneratorStats>();
}

// ===========================================================================
// fixtures
// ===========================================================================

fn pubkey(fill: u8) -> Pubkey {
    Pubkey::new([fill; 32])
}

fn pump_fun() -> Pubkey {
    Pubkey::parse(sts_lib::ingestion::PUMP_FUN_PROGRAM).expect("a valid program id")
}

/// A pump.fun bonding curve account: discriminator, five little-endian `u64`
/// reserves, the `complete` flag, the creator.
fn curve_account(virtual_sol: u64, virtual_token: u64, real_sol: u64) -> Vec<u8> {
    let mut data = vec![0u8; 81];
    data[8..16].copy_from_slice(&virtual_token.to_le_bytes());
    data[16..24].copy_from_slice(&virtual_sol.to_le_bytes());
    data[32..40].copy_from_slice(&real_sol.to_le_bytes());
    data[40..48].copy_from_slice(&1_000_000_000_000_000u64.to_le_bytes());
    data[49..81].copy_from_slice(&[7u8; 32]);
    data
}

/// One curve write.
fn write(slot: u64, micros: u64, write_version: u64, virtual_sol: u64) -> GeyserUpdate {
    GeyserUpdate::new(
        micros,
        UpdatePayload::Account(AccountUpdate {
            slot,
            pubkey: pubkey(1),
            owner: pump_fun(),
            lamports: 2_039_280,
            write_version,
            // `Bytes::from` takes the vector's allocation; nothing is copied
            // to get from the fixture's shape to the wire's.
            data: curve_account(virtual_sol, 1_073_000_000_000_000, 10_000_000_000).into(),
            is_startup: false,
        }),
    )
}

fn status(slot: u64, micros: u64, phase: SlotPhase) -> GeyserUpdate {
    GeyserUpdate::new(
        micros,
        UpdatePayload::Slot(SlotUpdate {
            slot,
            parent: Some(slot - 1),
            phase,
        }),
    )
}

fn log(slot: u64, micros: u64, index: u64) -> GeyserUpdate {
    GeyserUpdate::new(
        micros,
        UpdatePayload::Transaction(TransactionUpdate {
            slot,
            signature: Signature::new([index as u8; 64]),
            index,
            is_vote: false,
            failed: false,
            logs: vec!["Program log: Instruction: Buy".to_string()],
            pre_token_balances: Vec::new(),
            post_token_balances: Vec::new(),
        }),
    )
}

fn pipeline(capacity: usize, hold_slots: u64) -> TickPipeline {
    TickPipeline::new(&GeyserConfig {
        ring: RingConfig {
            capacity,
            hold_slots,
        },
        commitment: Commitment::Confirmed,
        ..GeyserConfig::default()
    })
}

/// Every curve price in a released stream, in order.
fn prices(events: &[TickEvent]) -> Vec<u128> {
    events
        .iter()
        .filter_map(|event| match &event.payload {
            TickPayload::Curve(curve) => Some(curve.price_e18),
            _ => None,
        })
        .collect()
}

// ===========================================================================
// a shuffled session, end to end
// ===========================================================================

/// A whole session of adversarially shuffled traffic comes out strictly
/// ordered, with nothing invented and nothing lost.
///
/// The script is built to break a naive implementation four different ways at
/// once: writes arrive backwards inside a slot, a later slot's writes arrive
/// before an earlier slot's, the slot statuses trail the data they settle, and
/// logs are interleaved between them.
#[test]
fn a_shuffled_session_comes_out_strictly_ordered() {
    let mut pipeline = pipeline(256, 2);
    let mut released = Vec::new();

    let script = vec![
        // Slot 11's write arrives before any of slot 10's.
        write(11, 1_100, 1, 34_000_000_000),
        log(10, 250, 2),
        // Slot 10's three writes, backwards.
        write(10, 300, 3, 32_000_000_000),
        write(10, 100, 1, 30_000_000_000),
        log(10, 150, 1),
        write(10, 200, 2, 31_000_000_000),
        // The statuses trail the data.
        status(10, 900, SlotPhase::Processed),
        status(10, 950, SlotPhase::Confirmed),
        status(11, 1_500, SlotPhase::Confirmed),
        status(12, 2_000, SlotPhase::Confirmed),
    ];

    for update in script {
        let outcome = pipeline.ingest(update);
        assert!(
            outcome.dropped.is_empty(),
            "nothing should be shed at this size"
        );
        assert!(outcome.stale.is_empty(), "no write in this script is stale");
        assert_eq!(outcome.decode_error, None);
        released.extend(outcome.released);
    }
    released.extend(pipeline.flush());

    // 1. The order is strict. Not "mostly sorted", not "sorted by slot".
    for pair in released.windows(2) {
        assert!(
            pair[0].key < pair[1].key,
            "release order broke at {:?}",
            pair[0].key
        );
    }

    // 2. Every curve write survived, in the order the chain wrote them, which
    //    is the reserve order and not the arrival order.
    let denominator = 1_073_000_000_000_000u128;
    assert_eq!(
        prices(&released),
        vec![
            ratio_e18(30_000_000_000, denominator).unwrap(),
            ratio_e18(31_000_000_000, denominator).unwrap(),
            ratio_e18(32_000_000_000, denominator).unwrap(),
            ratio_e18(34_000_000_000, denominator).unwrap(),
        ]
    );

    // 3. Both logs survived, and every price tick has a curve tick in front of
    //    it — a price is a difference and cannot exist without its baseline.
    let logs = released
        .iter()
        .filter(|event| matches!(event.payload, TickPayload::Log(_)))
        .count();
    assert_eq!(logs, 2);
    assert_eq!(
        released
            .iter()
            .filter(|event| matches!(event.payload, TickPayload::Price(_)))
            .count(),
        3,
        "three moves between four observations"
    );
}

/// Nothing is released before the chain settles the slot it belongs to.
#[test]
fn the_hold_window_is_what_a_reorg_undoes() {
    let mut pipeline = pipeline(256, 1_000);
    let mut released = Vec::new();

    for update in [
        write(10, 100, 1, 30_000_000_000),
        status(10, 150, SlotPhase::Processed),
        write(11, 200, 1, 90_000_000_000),
        status(11, 250, SlotPhase::Processed),
    ] {
        released.extend(pipeline.ingest(update).released);
    }
    assert!(
        released.is_empty(),
        "nothing is confirmed, so nothing has been released"
    );

    // Slot 11 dies. It never reached anyone, so it can be removed completely.
    let outcome = pipeline.ingest(status(11, 300, SlotPhase::Dead));
    assert_eq!(outcome.unrecoverable_from_slot, None, "the window held");
    assert!(!outcome.rolled_back.is_empty());

    released.extend(
        pipeline
            .ingest(status(10, 400, SlotPhase::Confirmed))
            .released,
    );
    released.extend(pipeline.flush());

    assert!(
        released.iter().all(|event| event.key.slot <= 10),
        "an abandoned slot escaped: {:?}",
        released
            .iter()
            .map(|event| event.key.slot)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        prices(&released),
        vec![ratio_e18(30_000_000_000, 1_073_000_000_000_000).unwrap()],
        "the abandoned reserves were never priced"
    );
}

/// Under sustained overflow, curve state degrades in latency and never in
/// completeness.
#[test]
fn backpressure_costs_latency_and_never_a_curve_write() {
    // A ring four deep against forty writes, so it overflows ten times over.
    let mut pipeline = pipeline(4, 1_000);
    let mut released = Vec::new();
    let mut dropped_curves = 0usize;

    for index in 0u64..40 {
        let outcome = pipeline.ingest(write(10, index * 10, index + 1, 30_000_000_000 + index));
        dropped_curves += outcome
            .dropped
            .iter()
            .filter(|event| matches!(event.payload, TickPayload::Curve(_)))
            .count();
        released.extend(outcome.released);
    }
    released.extend(pipeline.flush());

    assert_eq!(dropped_curves, 0, "backpressure dropped a curve write");
    assert_eq!(prices(&released).len(), 40, "a curve write went missing");
    for pair in released.windows(2) {
        assert!(pair[0].key < pair[1].key, "forced releases broke the order");
    }

    // The ring reports what it had to do to keep that promise.
    let metrics = pipeline.ring_metrics();
    assert!(
        metrics.forced_releases > 0,
        "the degradation should be visible"
    );
    assert_eq!(metrics.shed, 0);
}

// ===========================================================================
// precision
// ===========================================================================

/// The precision claim, made against a live-sized curve rather than a round
/// number.
#[test]
fn a_real_sized_curve_keeps_the_precision_millionths_would_lose() {
    let mut pipeline = pipeline(64, 0);

    // 30 SOL of virtual reserves against ~1.073e9 tokens at 6 decimals: the
    // shape of a pump.fun curve just after launch.
    let events = pipeline.ingest(write(10, 100, 1, 30_000_000_000)).released;
    let TickPayload::Curve(curve) = &events[0].payload else {
        panic!("expected a curve tick");
    };

    assert_eq!(curve.price_e18, 27_958_993_476_234);
    assert_eq!(format_e18(curve.price_e18, 18), "0.000027958993476234");

    // The same price in millionths is the integer 27. One step of that unit is
    // 1/27 of the price — about 370 basis points — so millionths cannot see any
    // move smaller than 3.7%, and the moves this engine trades are smaller.
    let millionths = curve.price_e18 / 1_000_000_000_000;
    assert_eq!(millionths, 27);
    assert!(10_000 / millionths > 300);

    // A 50-basis-point move. At this precision it is exact.
    let moved = pipeline.ingest(write(11, 200, 1, 30_150_000_000)).released;
    let price = moved
        .iter()
        .find_map(|event| match &event.payload {
            TickPayload::Price(price) => Some(price),
            _ => None,
        })
        .expect("a price tick");
    assert_eq!(price.delta_bps, 50);

    // The same move, if the prices had been stored in millionths first. The
    // quantised pair is 27 and 28, so the move is reported as one whole step —
    // 370 basis points against a true 50. Not "millionths are noisy": the
    // answer is off by more than seven times, and it is off in the direction
    // that fires an entry.
    let quantised_before = price.previous_e18 / 1_000_000_000_000;
    let quantised_after = price.current_e18 / 1_000_000_000_000;
    assert_eq!((quantised_before, quantised_after), (27, 28));
    let quantised_bps = (quantised_after - quantised_before) * 10_000 / quantised_before;
    assert_eq!(quantised_bps, 370);
    assert!(
        quantised_bps > u128::try_from(price.delta_bps).unwrap() * 7,
        "millionths reported {quantised_bps} bps for a {} bps move",
        price.delta_bps
    );
}

/// The raw-integer door, and the number that shows why it has to exist.
#[test]
fn a_token_amount_is_parsed_from_its_string_form() {
    // 2^53 + 1 is the first integer an f64 cannot represent. A pump.fun supply
    // is 10^15 raw units, two orders of magnitude past that.
    let beyond = 9_007_199_254_740_993u128;
    assert_eq!(parse_raw_amount("9007199254740993"), Ok(beyond));
    assert_ne!(beyond as f64 as u128, beyond, "the float loses it");

    for bad in ["", "1.0", "1e15", "-1", "+1", " 7"] {
        assert_eq!(
            parse_raw_amount(bad),
            Err(DecodeError::BadAmount),
            "{bad:?}"
        );
    }
}

// ===========================================================================
// reconnection
// ===========================================================================

/// The published backoff schedule, asserted rather than described.
#[test]
fn the_reconnect_schedule_is_the_one_the_documentation_claims() {
    let mut policy = ReconnectPolicy::default();
    let schedule: Vec<Duration> = (0..8).map(|_| policy.record_failure()).collect();

    assert_eq!(
        schedule[0],
        Duration::from_millis(500),
        "the first retry is the floor"
    );
    for pair in schedule.windows(2) {
        assert!(pair[1] >= pair[0], "the backoff went down");
    }
    assert_eq!(*schedule.last().unwrap(), Duration::from_secs(30), "capped");

    policy.record_success();
    assert_eq!(
        policy.record_failure(),
        Duration::from_millis(500),
        "a success resets it"
    );
}

/// A reconnect resumes behind the last release, so a disconnect costs
/// duplicates rather than a hole.
#[test]
fn a_reconnect_resumes_with_overlap_rather_than_a_gap() {
    let mut pipeline = pipeline(64, 0);
    assert_eq!(
        pipeline.resume_slot(),
        None,
        "a fresh pipeline starts from now"
    );

    for update in [
        write(500, 100, 1, 30_000_000_000),
        status(500, 200, SlotPhase::Confirmed),
        status(501, 300, SlotPhase::Confirmed),
    ] {
        pipeline.ingest(update);
    }

    let resume = pipeline.resume_slot().expect("something has been released");
    assert!(
        resume < 501,
        "resuming at the last released slot would re-deliver it"
    );
    assert!(501 - resume <= 4, "the overlap should be small: {resume}");
}

// ===========================================================================
// the metrics surface
// ===========================================================================

/// The snapshot is integers only, so a readout of it is a column of digits.
///
/// Not a style preference. A snapshot carrying a rate or an average is one the
/// reader cannot check against the counter it came from, and a derived number
/// in a monitoring surface is where a wrong denominator hides for months.
#[test]
fn the_snapshot_is_integers_all_the_way_down() {
    let mut pipeline = pipeline(64, 0);
    let metrics = GeyserMetrics::default();

    for update in [
        write(10, 100, 1, 30_000_000_000),
        status(10, 200, SlotPhase::Confirmed),
        log(10, 150, 1),
    ] {
        metrics.record_update(&update.payload);
        let outcome = pipeline.ingest(update);
        metrics.record_events(outcome.released.len());
    }

    let snapshot = metrics.snapshot(
        pipeline.ring_metrics(),
        pipeline.ledger(),
        pipeline.curves().stale_writes(),
    );
    assert_eq!(snapshot.accounts, 1);
    assert_eq!(snapshot.slots, 1);
    assert_eq!(snapshot.transactions, 1);
    assert_eq!(snapshot.confirmed_head, 10);

    let json = serde_json::to_string(&snapshot).expect("the snapshot serialises");
    // camelCase, matching every other snapshot the UI reads.
    assert!(json.contains("\"confirmedHead\":10"), "{json}");
    assert!(json.contains("\"decodeFailures\":0"), "{json}");
    // No float ever reaches the wire: JSON writes a float with a point in it,
    // and there is not one here.
    assert!(!json.contains('.'), "a float reached the snapshot: {json}");
}

/// The exact set of keys `get_geyser_metrics` puts on the wire.
///
/// This is an IPC contract test rather than a serde test. The window reads this
/// object by key name across a boundary the compiler does not cross: rename a
/// field, drop one, or let the `rename_all` attribute fall off, and every Rust
/// test still passes while the pane silently reads `undefined` and renders a
/// blank column. Nothing else in this crate would notice.
///
/// So the whole key set is written out. A new counter is *meant* to fail this
/// test once — that failure is the reminder that the pane and the README have
/// to learn about it too.
#[test]
fn the_ipc_snapshot_carries_exactly_the_keys_the_window_reads() {
    let snapshot = GeyserMetrics::default().snapshot_now();
    let value = serde_json::to_value(snapshot).expect("the snapshot serialises");
    let object = value.as_object().expect("the snapshot is a JSON object");

    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();

    let mut expected = vec![
        "accounts",
        "admitted",
        "confirmedHead",
        "connectFailures",
        "connects",
        "decodeFailures",
        "disconnects",
        "events",
        "finalizedHead",
        "foreignAccounts",
        "headSlot",
        "pings",
        "reconnectWaitMs",
        "refused",
        "reorgs",
        "ring",
        "slots",
        "staleWrites",
        "startupSkipped",
        "transactions",
        "unwinds",
        "updates",
    ];
    expected.sort_unstable();
    assert_eq!(keys, expected, "the shape the window reads changed");

    // The ring is nested and read the same way, so it is held to the same
    // contract rather than trusted because its parent passed.
    let ring = object["ring"].as_object().expect("ring is an object");
    let mut ring_keys: Vec<&str> = ring.keys().map(String::as_str).collect();
    ring_keys.sort_unstable();
    let mut expected_ring = vec![
        "buffered",
        "forcedReleases",
        "late",
        "outOfOrderArrivals",
        "released",
        "rolledBack",
        "shed",
        "unrecoverableReorgs",
    ];
    expected_ring.sort_unstable();
    assert_eq!(ring_keys, expected_ring, "the ring readout changed shape");

    // Every leaf is an unsigned integer. `serde_json` writes a float with a
    // point in it, so a `.` anywhere in the rendering is a float that reached
    // a surface documented as having none.
    for (name, field) in object {
        if name == "ring" {
            continue;
        }
        assert!(field.is_u64(), "{name} is not an unsigned integer: {field}");
    }
    for (name, field) in ring {
        assert!(
            field.is_u64(),
            "ring.{name} is not an unsigned integer: {field}"
        );
    }
}

// ===========================================================================
// the mock-Geyser load generator, and what it proves about the ring
// ===========================================================================

/// The ring the load runs against: production's hold window, and a ring wide
/// enough that nothing is shed for want of room.
///
/// `hold_slots` is deliberately the shipped default rather than something
/// generous. A benchmark that widened the safety window to make its own numbers
/// look better would be measuring a build nobody runs.
fn load_ring() -> GeyserConfig {
    GeyserConfig {
        ring: RingConfig {
            capacity: 8_192,
            hold_slots: 4,
        },
        commitment: Commitment::Confirmed,
        ..GeyserConfig::default()
    }
}

/// The rate the load generator is required to reach, in updates per second.
///
/// Fifty thousand is the requirement, not the observation: an unoptimised debug
/// build clears roughly half a million a second on a laptop, so the floor is
/// about an order of magnitude below what is actually measured. That headroom
/// is the point — a threshold set just under the current number is a threshold
/// that fails on a busy machine and teaches nobody anything.
const REQUIRED_UPDATES_PER_SECOND: u64 = 50_000;

/// How many times the rate is measured before the floor is applied.
///
/// A throughput floor is a claim about what the generator *can* do, and this
/// test does not run alone: `cargo test` puts twenty threads on the machine at
/// once, several of them running this same load, on top of whatever else the
/// laptop is doing. A single sample under that is a measurement of the
/// scheduler, and it is the scheduler that made this assertion fail
/// intermittently at roughly one run in five while the number it is guarding —
/// upwards of a million updates a second — never moved.
///
/// So the run is repeated and the *best* sample is the one the floor is applied
/// to, which is the right estimator for a capability: contention can only ever
/// make a sample slower, never faster, so the fastest of a few is the closest
/// any of them got to measuring the generator rather than the machine. A real
/// regression fails every sample and is still caught; this is not a retry that
/// hides one.
const RATE_SAMPLES: usize = 3;

#[test]
fn the_mock_geyser_outruns_the_rate_it_has_to_generate() {
    let report = run_load(LoadConfig::EXTREME, &load_ring());
    let mut generated = report.generated_per_second;
    let mut ingested = report.ingested_per_second;
    for _ in 1..RATE_SAMPLES {
        let again = run_load(LoadConfig::EXTREME, &load_ring());
        // One seed, one load: the samples differ in nothing but how much of the
        // machine each was given, so every count below reads the same whichever
        // one it comes from and only the two rates are worth keeping.
        assert_eq!(
            again.generator.updates, report.generator.updates,
            "the load is not seeded"
        );
        assert_eq!(again.released, report.released, "the load is not seeded");
        // The two passes are timed separately and contend separately, so they
        // are maxed separately. Taking both numbers off whichever run generated
        // fastest would let a scheduling stall in the *second* pass of that run
        // fail an assertion about the first.
        generated = generated.max(again.generated_per_second);
        ingested = ingested.max(again.ingested_per_second);
    }

    assert!(
        report.generator.updates > REQUIRED_UPDATES_PER_SECOND,
        "a rate measured over less than a second is not a rate: {} updates",
        report.generator.updates
    );
    assert!(
        generated >= REQUIRED_UPDATES_PER_SECOND,
        "the generator managed {generated} updates/s, under the \
         {REQUIRED_UPDATES_PER_SECOND} required"
    );
    // The pipeline has to keep up with it too, or the benchmark is measuring a
    // queue rather than a sequencer.
    assert!(
        ingested >= REQUIRED_UPDATES_PER_SECOND,
        "the pipeline managed {ingested} updates/s, under the \
         {REQUIRED_UPDATES_PER_SECOND} required"
    );

    // The load is account writes and transactions, which is what the claim is
    // about; slot statuses are the overhead beside them and should stay a
    // minority of the stream.
    let subslot_events = report.generator.accounts + report.generator.transactions;
    assert!(
        subslot_events > report.generator.slot_statuses * 4,
        "the run should be dominated by account and transaction traffic: \
         {subslot_events} against {} statuses",
        report.generator.slot_statuses
    );

    // Captured unless the test fails, and worth reading with `--nocapture`: the
    // assertions above are a floor, and the floor is not the interesting number.
    eprintln!(
        "load: {} updates ({} accounts, {} transactions, {} statuses)\n\
         rate: {} generated/s, {} ingested/s, {} ordered events/s\n\
         jitter: {} descents, {} positions at worst\n\
         ring: {} released, {} late, {} rolled back, {} unrecoverable",
        report.generator.updates,
        report.generator.accounts,
        report.generator.transactions,
        report.generator.slot_statuses,
        generated,
        ingested,
        report.events_per_second,
        report.generator.descents,
        report.generator.max_displacement,
        report.ring.released,
        report.ring.late,
        report.ring.rolled_back,
        report.ring.unrecoverable_reorgs,
    );
}

#[test]
fn extreme_jitter_comes_out_strictly_ordered_and_loses_nothing_silently() {
    let report = run_load(LoadConfig::EXTREME, &load_ring());

    // The whole claim of `subslot`, measured over forty thousand events rather
    // than asserted over a dozen.
    assert_eq!(
        report.order_violations, 0,
        "the ring released {} events out of order",
        report.order_violations
    );
    assert_eq!(
        report.zero_prices, 0,
        "a curve priced at zero is not a price"
    );
    assert_eq!(
        report.decode_failures, 0,
        "the generator produced bytes the decoder refused"
    );

    // The jitter has to be real for the ordering to mean anything. A span of
    // 512 positions is a little over ten slots at this event rate, so most of
    // the stream arrives displaced and a fifth of it arrives *descending*.
    assert!(
        report.generator.descents > report.generator.updates / 5,
        "only {} of {} arrivals descended; that is not extreme jitter",
        report.generator.descents,
        report.generator.updates
    );
    assert!(report.generator.max_displacement > 200);
    // The ring's own count of the disorder should agree with the generator's,
    // to within the handful of arrivals the pipeline never offers it — dead
    // slots and the statuses of slots already rolled back.
    let counted = report.ring.out_of_order_arrivals;
    assert!(
        counted <= report.generator.descents
            && counted + report.generator.updates / 100 >= report.generator.descents,
        "the ring counted {counted} descents where the generator made {}",
        report.generator.descents
    );

    // Past the hold window, loss is not optional — an event whose slot was
    // released before it arrived cannot be put back in order. What *is* required
    // is that every one of them is counted. Nothing may simply disappear.
    assert!(
        report.ring.late > 0,
        "jitter this far past the hold window should have produced late arrivals"
    );
    // Past the window the protection stops protecting, and saying so is the
    // point of measuring it. `hold_slots` is the promise, `jitter_span` is what
    // the network did, and when the second exceeds the first a curve write can
    // arrive for a slot that has already been acted on. It is counted, not
    // hidden.
    assert!(
        report.dropped_protected > 0,
        "at this jitter some curve writes must have outrun the window"
    );
    assert!(
        report.dropped_protected < report.curve_events,
        "most curve writes should still survive: {} lost against {} released",
        report.dropped_protected,
        report.curve_events
    );
    // Lost or kept, every write the generator made is still accounted for.
    assert_eq!(
        report.curve_events
            + report.stale
            + report.rolled_back_protected
            + report.dropped_protected,
        report.generator.accounts,
        "a curve write went missing between the wire and the engine"
    );
    assert_eq!(
        report.dropped, report.ring.late,
        "every refused arrival should be reported to the caller, not just counted"
    );
    // Everything that entered the ring either left it in order or was rolled
    // back by a fork switch. The books balance exactly.
    assert_eq!(
        report.ring.buffered,
        report.ring.released + report.ring.rolled_back,
        "the ring is holding events it never accounted for"
    );
    // And what the ring released is what the caller saw, less the price ticks
    // the pipeline derives on the way out and plus the stale writes it swallows.
    assert_eq!(
        report.ring.released,
        report.released - report.price_events + report.stale,
        "the released count and the ring's disagree"
    );
}

#[test]
fn jitter_inside_the_hold_window_never_costs_a_curve_write() {
    // The other half of the same claim, and the half with the sharp edge on it.
    //
    // The hold window exists to absorb reordering, and when the reordering fits
    // inside it the absorption is total *for the payloads that matter*. What
    // still gets refused is slot statuses, and the reason is worth writing
    // down because it looks like a bug and is not: the commitment stream is a
    // second timeline, so a `Confirmed` for slot N travels beside slot N+2's
    // account writes. Displace that status and a later one can overtake it,
    // moving the released watermark past slot N — at which point the original
    // arrives for a slot that has already gone out, and cannot be placed in
    // order any more. It is refused and counted, the next status repairs the
    // ledger, and nothing downstream is wrong.
    //
    // A curve write in that position would be a different matter entirely,
    // because nothing re-sends one. That is the zero here.
    let report = run_load(LoadConfig::ABSORBED, &load_ring());

    assert!(
        report.generator.descents > 0,
        "the run was not jittered at all"
    );
    assert_eq!(report.order_violations, 0);
    assert_eq!(
        report.dropped_protected, 0,
        "{} curve or price events were lost to jitter the hold window should have absorbed",
        report.dropped_protected
    );
    assert_eq!(
        report.ring.shed, 0,
        "the ring shed an event it had room for"
    );
    assert_eq!(
        report.ring.forced_releases, 0,
        "the ring gave up its safety window"
    );
    assert!(report.curve_events > 0 && report.price_events > 0);

    // And every curve write the generator made is accounted for, one way or
    // another. The pipeline emits one curve tick per account update, so the
    // released ones plus the ones the write-version guard swallowed plus the
    // ones a fork rollback discarded have to come to exactly what was sent.
    // A shortfall here would be a write that vanished with nobody counting it,
    // which is the failure mode the whole module is written against.
    assert_eq!(
        report.curve_events
            + report.stale
            + report.rolled_back_protected
            + report.dropped_protected,
        report.generator.accounts,
        "a curve write went missing between the wire and the engine"
    );
    assert!(
        report.rolled_back_protected > 0,
        "no curve write was ever caught by a rollback, so the window proved nothing"
    );
}

#[test]
fn every_injected_fork_is_caught_and_undone_before_release() {
    let report = run_load(LoadConfig::EXTREME, &load_ring());

    assert!(
        report.injected_reorgs > 0,
        "the run injected no fork to catch"
    );
    // The ledger may see more re-orgs than were injected and may never see
    // fewer: a fork status displaced into the middle of its own slot's statuses
    // moves the parent once on the way out and once on the way back, and both
    // of those are the parent genuinely moving. Under-counting would mean a
    // fork went unnoticed, which is the failure this asserts against.
    assert!(
        report.ledger_reorgs >= report.injected_reorgs,
        "the ledger saw {} re-orgs where {} were injected — a fork went unnoticed",
        report.ledger_reorgs,
        report.injected_reorgs
    );

    // A rollback that discards nothing has undone nothing. The hold window is
    // what makes these discards possible, and most of the damage should land
    // inside it.
    assert!(
        report.rolled_back > 0,
        "no buffered event was discarded, so no rollback actually rolled anything back"
    );
    assert_eq!(
        report.rolled_back, report.ring.rolled_back,
        "the caller was told about a different number of discards than the ring made"
    );
    // The ones that outran the window are the honest bad news, and they are
    // reported rather than swallowed.
    assert_eq!(report.unrecoverable, report.ring.unrecoverable_reorgs);
    assert!(
        report.rolled_back > report.unrecoverable,
        "the hold window is meant to catch most fork damage: {} discarded against \
         {} that got away",
        report.rolled_back,
        report.unrecoverable
    );
}

#[test]
fn the_generated_stream_is_reproducible_from_its_seed() {
    // A load run that cannot be replayed can only find a bug once. Two runs of
    // one configuration have to agree on every number, including the counts of
    // things that went wrong.
    let ring = load_ring();
    let first = run_load(LoadConfig::ABSORBED, &ring);
    let second = run_load(LoadConfig::ABSORBED, &ring);

    assert_eq!(first.generator, second.generator);
    assert_eq!(first.released, second.released);
    assert_eq!(first.ring, second.ring);
    assert_eq!(first.ledger_reorgs, second.ledger_reorgs);
    assert_eq!(first.rolled_back, second.rolled_back);

    let other = run_load(
        LoadConfig {
            seed: LoadConfig::ABSORBED.seed ^ 1,
            ..LoadConfig::ABSORBED
        },
        &ring,
    );
    assert_ne!(first.generator.descents, other.generator.descents);
}

// ===========================================================================
// the wiring: an ordered tick is a candidate on the same queue
// ===========================================================================

/// What SOL is worth for the wiring tests, in whole cents.
///
/// Two hundred dollars, because every threshold the filters use is written in
/// dollars and the generated curves have to land inside the band the strategy
/// actually wants. At this price a pump.fun curve enters the $25k–$80k target
/// window somewhere around 35 SOL of real reserves, which is most of the way up
/// a curve and exactly where a launch worth looking at sits.
const SOL_PRICE: SolPrice = SolPrice::from_usd_cents(20_000);

/// Load shaped so the curves in it actually trade into the strategy's band.
///
/// The benchmark configurations spread their writes over five hundred curves,
/// which is right for stressing the ring — many distinct accounts, many
/// interleaved write-version sequences — and wrong here. A curve that receives
/// a dozen buys across the whole run never leaves the spam floor, so a wiring
/// test built on one would assert that nothing routes and pass for the wrong
/// reason. Fewer curves, the same traffic: each one climbs its curve, crosses
/// the $25k–$80k window, graduates, and relaunches.
fn wiring_load(slots: u64) -> LoadConfig {
    LoadConfig {
        slots,
        curves: 32,
        ..LoadConfig::ABSORBED
    }
}

/// A curve priced into the target window, for the tests that need one fact
/// rather than a distribution.
fn routable_curve() -> BondingCurve {
    BondingCurve {
        // 80 SOL of virtual reserves against a token side that keeps the
        // constant product where a real curve's is. Market cap works out at
        // about 199 SOL — near $40k, in the middle of the window.
        virtual_sol_reserves: 80_000_000_000,
        virtual_token_reserves: 402_375_000_000_000,
        real_sol_reserves: 50_000_000_000,
        real_token_reserves: 400_000_000_000_000,
        token_total_supply: 1_000_000_000_000_000,
        complete: false,
        creator: Pubkey::new([9u8; 32]),
    }
}

/// Captures published telemetry so a test can read what the window would see.
#[derive(Default)]
struct CapturedTelemetry {
    events: std::sync::Mutex<Vec<sts_lib::telemetry::TelemetryEvent>>,
}

impl sts_lib::telemetry::TelemetrySink for CapturedTelemetry {
    fn deliver(&self, event: &sts_lib::telemetry::TelemetryEvent) {
        self.events
            .lock()
            .expect("the capture is not poisoned")
            .push(event.clone());
    }
}

/// The reason a dial failed reaches telemetry, which is where a person is.
///
/// The unit tests prove the loop *reports* a fault. This proves the live sink
/// does something with it — that the reporting is wired to the hub the window
/// reads and not only to the collecting sink the tests use. Those are different
/// claims, and the second one is the one an operator depends on.
#[test]
fn a_dial_failure_reaches_the_telemetry_the_window_reads() {
    let hub = TelemetryHub::start();
    let captured = std::sync::Arc::new(CapturedTelemetry::default());
    hub.observe(
        std::sync::Arc::clone(&captured) as std::sync::Arc<dyn sts_lib::telemetry::TelemetrySink>
    );
    let hub = std::sync::Arc::new(hub);

    let (ingestion, _streams) = IngestionManager::start(
        IngestionConfig {
            price: SOL_PRICE,
            ..IngestionConfig::default()
        },
        std::sync::Arc::new(WebSocketDialer),
        None,
        None,
    );
    let mut sink = IngestionSink::new(
        FeedProvider::Triton,
        std::sync::Arc::clone(&ingestion),
        std::sync::Arc::new(GeyserMetrics::default()),
        Some(std::sync::Arc::clone(&hub)),
    );

    // A first failure is a blip; a run of them is an outage. Both are sent,
    // and they are sent at different levels, because a reader who is paged by
    // every transient reconnect stops reading.
    sink.fault(&GeyserError::Dial("connection refused".into()), 1);
    sink.fault(&GeyserError::Dial("connection refused".into()), 7);

    hub.shutdown();

    let events = captured.events.lock().expect("the capture is not poisoned");
    let geyser: Vec<_> = events
        .iter()
        .filter(|event| event.source == "geyser")
        .collect();
    assert_eq!(geyser.len(), 2, "the fault did not reach telemetry");

    assert!(
        geyser[0].message.contains("connection refused"),
        "the reason is not in the message: {}",
        geyser[0].message
    );
    assert_eq!(
        geyser[0].level,
        TelemetryLevel::Info,
        "a first failure was raised as an outage"
    );
    assert_eq!(
        geyser[1].level,
        TelemetryLevel::Warn,
        "a sustained outage was reported as routine"
    );
    assert_eq!(geyser[1].data["consecutiveFailures"], 7);
    // The structured half carries the error in the shape the UI already knows,
    // rather than only as prose it would have to parse.
    assert_eq!(geyser[1].data["reason"]["kind"], "dial");
    assert_eq!(geyser[1].data["reason"]["detail"], "connection refused");
}

#[tokio::test]
async fn an_ordered_tick_arrives_on_the_channel_a_websocket_frame_arrives_on() {
    // The wiring, end to end and without a socket: generated load into the
    // sequencer, released events into `IngestionSink`, candidates out of the
    // very same two channels the websocket feed writes to. If this passes, a
    // Geyser candidate and a pubsub candidate are the same thing to everything
    // downstream — which is the whole reason the sink hands events to the
    // manager instead of keeping a queue of its own.
    let (ingestion, mut streams) = IngestionManager::start(
        IngestionConfig {
            price: SOL_PRICE,
            ..IngestionConfig::default()
        },
        std::sync::Arc::new(WebSocketDialer),
        None,
        None,
    );
    let metrics = std::sync::Arc::new(GeyserMetrics::default());
    let mut sink = IngestionSink::new(
        FeedProvider::Triton,
        std::sync::Arc::clone(&ingestion),
        std::sync::Arc::clone(&metrics),
        None,
    );
    let mut pipeline = TickPipeline::new(&load_ring());

    let mut fast = 0u64;
    let mut standard = 0u64;
    let drain = |streams: &mut IngestionStreams, fast: &mut u64, standard: &mut u64| {
        while let Ok(event) = streams.fast_path.try_recv() {
            assert_eq!(event.route, Route::FastPath);
            assert_eq!(event.view.provider, FeedProvider::Triton);
            assert!(
                !event.view.curve_complete,
                "a graduated curve is not a candidate"
            );
            *fast += 1;
        }
        while let Ok(event) = streams.standard.try_recv() {
            assert_eq!(event.route, Route::Standard);
            *standard += 1;
        }
    };

    // A shorter run than the benchmark's: this test is about the seam, and the
    // channels are shallow on purpose, so it drains as it goes rather than
    // proving how much it can lose.
    for update in MockGeyser::new(wiring_load(200)) {
        let outcome = pipeline.ingest(update);
        sink.emit(outcome.released);
        drain(&mut streams, &mut fast, &mut standard);
    }
    sink.emit(pipeline.flush());
    drain(&mut streams, &mut fast, &mut standard);

    let snapshot = ingestion.snapshot();
    let geyser = metrics.snapshot_now();

    assert!(geyser.admitted > 0, "the sink offered nothing to ingestion");
    assert_eq!(
        geyser.admitted, snapshot.ordered_ticks,
        "the sink and the manager disagree about how many ticks crossed the seam"
    );
    assert!(
        geyser.refused > 0,
        "not one candidate was filtered, so the filters were not running"
    );
    assert!(fast > 0, "no candidate reached the fast path");
    assert!(standard > 0, "no candidate reached the standard path");
    assert_eq!(
        snapshot.candidates,
        fast + standard,
        "a candidate was counted that nothing received"
    );
    assert_eq!(
        snapshot.dropped_fast_path + snapshot.dropped_standard,
        0,
        "a candidate was lost to backpressure in a test that drains every iteration"
    );
    // The counters have to add up: everything offered was either routed onto a
    // channel or refused by a named filter.
    assert_eq!(geyser.admitted, geyser.refused + snapshot.candidates);
}

#[tokio::test]
async fn a_fork_that_outran_the_hold_window_rewinds_the_launch_index() {
    // The one case the ring cannot fix, and what the wiring does about it.
    //
    // Once a slot has been released, a fork switch at that slot is damage the
    // buffer cannot undo. What it *can* do is stop the damage compounding: the
    // launch index watermark is walked back so that the winning fork's rewrite
    // of the same slot is heard rather than refused as a duplicate. Without
    // this the engine would sit on a price from a block that no longer exists
    // until the curve happened to trade again.
    let (ingestion, _streams) = IngestionManager::start(
        IngestionConfig {
            price: SOL_PRICE,
            ..IngestionConfig::default()
        },
        std::sync::Arc::new(WebSocketDialer),
        None,
        None,
    );
    let curve = routable_curve();
    let account = Pubkey::new([7u8; 32]);
    let at = std::time::Instant::now();
    let admit = |slot: u64| ingestion.admit_curve(FeedProvider::Triton, slot, account, &curve, at);

    // First sighting: the lottery floor refuses it, and the index remembers it.
    assert!(matches!(
        admit(100),
        Verdict::Dropped(DropReason::LotterySlot)
    ));
    // The same slot again is a duplicate, whoever reported it.
    assert!(matches!(
        admit(100),
        Verdict::Dropped(DropReason::StaleSlot)
    ));
    // Eleven slots on it has an age, and it routes.
    assert!(matches!(admit(111), Verdict::Routed(Route::FastPath)));
    // And that slot is now watermarked, so a repeat is refused.
    assert!(matches!(
        admit(111),
        Verdict::Dropped(DropReason::StaleSlot)
    ));

    // The fork arrives late. One record is walked back, not dropped: the
    // account's first sighting was at slot 100, on a block that is still there.
    assert_eq!(ingestion.rewind_launches(111), 1);
    assert!(
        matches!(admit(111), Verdict::Routed(Route::FastPath)),
        "the winning fork's rewrite was still being refused as stale"
    );

    let snapshot = ingestion.snapshot();
    assert_eq!(snapshot.rewinds, 1);
    assert_eq!(snapshot.rewound_accounts, 1);
    assert_eq!(
        snapshot.ordered_ticks, 5,
        "every admission should be counted"
    );

    // An account whose only sighting was on the abandoned fork is forgotten
    // outright rather than rewound. Its age was measured from a launch that did
    // not happen, and the lottery floor has to start counting again.
    let doomed = Pubkey::new([8u8; 32]);
    assert!(matches!(
        ingestion.admit_curve(FeedProvider::Triton, 500, doomed, &curve, at),
        Verdict::Dropped(DropReason::LotterySlot)
    ));
    assert_eq!(ingestion.rewind_launches(500), 1);
    assert!(
        matches!(
            ingestion.admit_curve(FeedProvider::Triton, 500, doomed, &curve, at),
            Verdict::Dropped(DropReason::LotterySlot)
        ),
        "a curve whose launch was abandoned should be aged from scratch"
    );
}

#[tokio::test]
async fn a_feed_with_no_endpoint_configured_opens_nothing_and_says_so() {
    // The Phase 0 gate, in the one place the window touches it. An unconfigured
    // build must not dial, must not fail, and must report a feed that plainly
    // was never opened — the same rule `IngestionManager` follows for an empty
    // endpoint list.
    let (ingestion, _streams) = IngestionManager::start(
        IngestionConfig::default(),
        std::sync::Arc::new(WebSocketDialer),
        None,
        None,
    );
    let feed = GeyserFeed::start(None, sts_lib::geyser::default_transport(), ingestion, None);

    assert!(!feed.is_configured());
    assert_eq!(feed.endpoint(), None);
    let snapshot = feed.snapshot();
    assert_eq!(snapshot.connects, 0);
    assert_eq!(snapshot.connect_failures, 0);
    assert_eq!(snapshot.updates, 0);
    assert_eq!(snapshot.events, 0);
    // Stopping something that was never started is not an error.
    feed.stop();
    feed.stop();
}

#[tokio::test]
async fn the_subscriber_loop_drives_generated_load_all_the_way_to_a_candidate() {
    // The same wiring as above, but through `run_subscriber` rather than around
    // it — so the startup-account skip, the metrics, the sink dispatch and the
    // shutdown path are all in the run rather than assumed. `LoadTransport`
    // stands in for the socket, and nothing else changes.
    let (ingestion, mut streams) = IngestionManager::start(
        IngestionConfig {
            price: SOL_PRICE,
            ..IngestionConfig::default()
        },
        std::sync::Arc::new(WebSocketDialer),
        None,
        None,
    );
    let load = wiring_load(120);
    let feed = GeyserFeed::start(
        Some(GeyserConfig {
            endpoint: "https://geyser.example.com/key/secret".to_string(),
            ring: RingConfig {
                capacity: 8_192,
                hold_slots: 4,
            },
            provider: FeedProvider::Triton,
            ..GeyserConfig::default()
        }),
        std::sync::Arc::new(LoadTransport::new(load)),
        std::sync::Arc::clone(&ingestion),
        None,
    );

    // Drained until there is enough to judge by, with a hard ceiling on how
    // long that may take. The generated stream ends and the subscriber treats
    // that as a disconnect, so it backs off and reconnects to a fresh one
    // forever — a loop that waited for the feed to go quiet would wait for
    // something that never happens.
    let mut received = 0u64;
    for _ in 0..600 {
        while streams.fast_path.try_recv().is_ok() {
            received += 1;
        }
        while streams.standard.try_recv().is_ok() {
            received += 1;
        }
        if received >= 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    feed.stop();

    assert!(feed.is_configured());
    assert_eq!(
        feed.endpoint(),
        Some("https://geyser.example.com/…"),
        "the credential in the endpoint must never reach a readout"
    );
    assert!(
        received > 0,
        "the subscriber loop produced no candidates at all"
    );

    let snapshot = feed.snapshot();
    assert!(snapshot.connects >= 1);
    assert!(snapshot.updates > 0);
    assert!(snapshot.accounts > 0);
    assert!(snapshot.slots > 0);
    assert!(snapshot.events > 0);
    assert!(snapshot.admitted > 0);
    assert_eq!(snapshot.decode_failures, 0);
    // The sequencer's own state, mirrored out of the task that owns it.
    assert!(snapshot.head_slot > 0);
    assert!(snapshot.confirmed_head > 0);
    assert!(snapshot.ring.buffered > 0);
    assert!(snapshot.ring.released > 0);
    assert_eq!(ingestion.snapshot().ordered_ticks, snapshot.admitted);
}

#[tokio::test]
async fn a_live_geyser_stream_keeps_a_fixture_off_the_clock() {
    // The replay bar says nothing under it is live. That sentence is only true
    // while nothing is arriving, and this build now has a second thing that can
    // arrive — so the guard that protects the bar has to know about it. A
    // version of this that only looked at the websocket endpoints would raise
    // the bar over live Geyser candidates the moment somebody set an endpoint,
    // which is the exact failure the guard exists to prevent, reached by a route
    // nobody changed.
    let (ingestion, _streams) = IngestionManager::start(
        IngestionConfig {
            price: SOL_PRICE,
            ..IngestionConfig::default()
        },
        std::sync::Arc::new(WebSocketDialer),
        None,
        None,
    );
    // No endpoints configured, so the websocket half of the guard is silent and
    // whatever it refuses is the Geyser half talking.
    let feed = GeyserFeed::start(
        Some(GeyserConfig {
            endpoint: "https://geyser.example.com/key/secret".to_string(),
            provider: FeedProvider::QuickNode,
            ..GeyserConfig::default()
        }),
        // Keeping alive rather than ending, because the question this test asks
        // is whether the guard sees a feed that is *up*. A stream that ends the
        // moment its load runs out is a feed that is up for a few microseconds
        // between half-second backoffs, and catching that would be a test of
        // the scheduler rather than of the guard.
        std::sync::Arc::new(LoadTransport::keeping_alive(
            wiring_load(60),
            Duration::from_millis(20),
        )),
        std::sync::Arc::clone(&ingestion),
        None,
    );

    let mut connected = false;
    for _ in 0..400 {
        if feed.is_connected() {
            connected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        connected,
        "the mock transport never reported a subscription"
    );

    let refusal = sts_lib::refuse_over_a_live_feed(&ingestion, &feed)
        .expect_err("a fixture must not go behind the clock over a live geyser stream");
    let message = refusal.to_string();
    assert!(
        message.contains("geyser stream"),
        "the refusal names what is in the way: {message}"
    );
    assert!(
        message.contains("quicknode"),
        "and which provider it is: {message}"
    );
    assert!(
        message.contains("Stop the feeds first"),
        "and what to do about it: {message}"
    );
    // The credential must not travel in a message that ends up in front of a
    // person, however alarming the message is.
    assert!(
        !message.contains("secret"),
        "the endpoint's credential reached a refusal: {message}"
    );

    // Stopping the feed clears it. A guard that could not be cleared would make
    // replay unreachable on any machine with a Geyser endpoint configured.
    feed.stop();
    assert!(!feed.is_connected());
    sts_lib::refuse_over_a_live_feed(&ingestion, &feed)
        .expect("with the feed stopped, replay may start");
}
