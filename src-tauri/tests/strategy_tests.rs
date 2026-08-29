//! The entry rule from outside, against the published numbers.
//!
//! `strategy/syndicate.rs` has the unit tests for how each piece behaves. These
//! are the ones that only mean something through the front door: that the types
//! a caller needs are actually public, that the two specifications' test vectors
//! come back out of the shipped API rather than out of a private helper, and
//! that the same record read twice produces the same bytes.
//!
//! Two documents are being checked here and they are not the same one.
//! `RISK_AND_SYBIL_SPEC.md` §2.2, §2.3 and §14 are the concentration vectors —
//! the `[90, 10]` population that is an index of 8 200 and an entropy of 0.4690.
//! `REPLAY_AND_SIMULATION_SPEC.md` §15.2 is the sandwich threshold,
//! `β > φ / (1 - φ)`, and the three curve positions it publishes a minimum
//! victim buy for.

use sts_lib::strategy::{
    analyse_launch, evaluate, syndicate_gate, ClusterParams, ClusterReport, EntryQuote,
    FundingEdge, GateParams, GateReason, GateVerdict, LaunchRecord, OpeningBuyer, SandwichCheck,
    SandwichGuard, RING_ENTROPY_MICROS, RING_HHI_BPS,
};

const SOL: u64 = 1_000_000_000;
const MICROS: u64 = 1_000_000;

// ===========================================================================
// Fixtures
// ===========================================================================

fn buyer(wallet: &str, lamports: u64, at_ms: i64) -> OpeningBuyer {
    OpeningBuyer {
        wallet: wallet.to_string(),
        sol_in_lamports: lamports,
        sol_out_lamports: 0,
        tx_count: 1,
        first_seen_ms: at_ms,
    }
}

fn launch(buyers: Vec<OpeningBuyer>) -> LaunchRecord {
    LaunchRecord {
        mint: "MINT".to_string(),
        creator: None,
        buyers,
        funding: Vec::new(),
    }
}

fn read(record: &LaunchRecord) -> ClusterReport {
    analyse_launch(record, &ClusterParams::default())
}

fn gate(record: &LaunchRecord, quote: Option<&EntryQuote>) -> GateVerdict {
    evaluate(
        record,
        &ClusterParams::default(),
        &GateParams::default(),
        quote,
    )
    .1
}

/// A population given as shares, laid out as an opening. One wallet per share,
/// all landing together so the analyser reads them as one window.
fn population(shares: &[u64]) -> LaunchRecord {
    launch(
        shares
            .iter()
            .enumerate()
            .map(|(n, &share)| buyer(&format!("h{n}"), share * SOL / 100, 2_000))
            .collect(),
    )
}

/// Six wallets that each took the same 0.7777 SOL position in the same instant,
/// two seconds after the launch. The shape the rule exists to enter.
fn a_group() -> LaunchRecord {
    launch(
        (1..=6)
            .map(|n| buyer(&format!("w{n}"), 777_700_000, 2_000))
            .collect(),
    )
}

/// The same six, with the wallet that is actually holding the position standing
/// behind them and the funder that paid for all seven.
fn a_ring() -> LaunchRecord {
    let mut buyers: Vec<OpeningBuyer> = a_group().buyers;
    buyers.push(buyer("whale", 45 * SOL, 2_000));
    let funding: Vec<FundingEdge> = buyers
        .iter()
        .map(|b| FundingEdge {
            from: "F".to_string(),
            to: b.wallet.clone(),
            lamports: SOL,
        })
        .collect();
    LaunchRecord {
        funding,
        ..launch(buyers)
    }
}

// ===========================================================================
// RISK_AND_SYBIL_SPEC.md sections 2.2, 2.3 and 14
// ===========================================================================

/// §14's index table, every row, through the report a caller gets back.
#[test]
fn the_published_index_table_comes_back_out_of_the_api() {
    let cases: [(&[u64], Option<u16>); 7] = [
        (&[100], Some(10_000)),
        (&[50, 50], Some(5_000)),
        (&[25, 25, 25, 25], Some(2_500)),
        (&[10, 10, 10, 10, 10, 10, 10, 10, 10, 10], Some(1_000)),
        (&[90, 10], Some(8_200)),
        (&[0, 0, 0], None),
        (&[], None),
    ];
    for (shares, want) in cases {
        assert_eq!(
            read(&population(shares)).concentration.hhi_bps,
            want,
            "population {shares:?}",
        );
    }
}

/// §14's entropy table beside it, on the same populations.
#[test]
fn the_published_entropy_table_comes_back_out_of_the_api() {
    assert_eq!(
        read(&population(&[50, 50])).concentration.entropy_micros,
        Some(MICROS),
    );
    assert_eq!(
        read(&population(&[25, 25, 25, 25]))
            .concentration
            .entropy_micros,
        Some(MICROS),
    );

    // `[0.9, 0.1]` is 0.4690 to the four places §14 publishes.
    let split = read(&population(&[90, 10]))
        .concentration
        .entropy_micros
        .expect("two buyers");
    assert_eq!((split + 50) / 100, 4_690);

    // §14's `[1.0]` row is a defined zero, and this column stores whether the
    // entropy could be read rather than the limit it tends to. One buyer on the
    // record is one row, not a demonstration that one address took everything.
    assert_eq!(read(&population(&[100])).concentration.entropy_micros, None);
}

/// The two constants the gate refuses on are one population read twice.
///
/// This is the assertion the whole ring check rests on: §14's `[90, 10]` is an
/// index of exactly 8 200 and an entropy that sits just inside 0.4690, so the
/// published shape is a ring by both instruments at once, with nothing rounded
/// in its favour.
#[test]
fn the_two_thresholds_are_the_same_shape_stated_twice() {
    let report = read(&population(&[90, 10]));
    let concentration = report.concentration;

    assert_eq!(concentration.hhi_bps, Some(RING_HHI_BPS));
    let entropy = concentration.entropy_micros.expect("two buyers");
    assert!(
        entropy <= RING_ENTROPY_MICROS,
        "the published population must be inside its own threshold: \
         {entropy} against {RING_ENTROPY_MICROS}",
    );
}

/// A launch the rule likes, and the same launch with one wallet holding it.
#[test]
fn a_group_trades_and_a_ring_does_not() {
    let group = gate(&a_group(), None);
    assert_eq!(group.reason, GateReason::Accepted);
    assert!(group.enter);
    assert!(group.rings.is_empty());

    let ring = gate(&a_ring(), None);
    assert_eq!(ring.reason, GateReason::CoordinatedRing);
    assert!(!ring.enter);

    // The refusal happened after the group checks, not instead of them: the six
    // wallets are still there, still the same size, still committed enough.
    assert_eq!(ring.cohort_wallets, 6);
    assert_eq!(ring.cohort_lamports, group.cohort_lamports);

    let finding = ring.rings.first().expect("the ring is named");
    assert!(finding.material);
    assert!(finding.holding_hhi_bps >= RING_HHI_BPS);
    assert!(finding.holding_entropy_micros.expect("measured") <= RING_ENTROPY_MICROS);
    assert_eq!(finding.wallets, 7);
}

/// The thresholds are policy, and turning them off puts the rule back where it
/// was rather than somewhere new.
#[test]
fn the_ring_check_is_configuration_and_can_be_turned_off() {
    let record = a_ring();
    let off = GateParams {
        ring_min_hhi_bps: None,
        ring_max_entropy_micros: None,
        ..GateParams::default()
    };
    let verdict = evaluate(&record, &ClusterParams::default(), &off, None).1;
    assert_eq!(verdict.reason, GateReason::Accepted);
    assert!(verdict.rings.is_empty());

    // The rule as it stood before any of this is still runnable and still says
    // what it always said.
    let v1 = evaluate(&record, &ClusterParams::default(), &GateParams::v1(), None).1;
    assert_eq!(v1.reason, GateReason::Accepted);
}

// ===========================================================================
// REPLAY_AND_SIMULATION_SPEC.md section 15.2
// ===========================================================================

/// §15.2's table: the smallest buy worth front-running at three points on the
/// curve, in lamports, and the sign either side of it.
#[test]
fn the_published_sandwich_thresholds_come_back_out_of_the_api() {
    // Launch, `y_r = 45`, and graduation: 0.3061, 0.7652 and 1.1733 SOL.
    let cases = [
        (30u64, 306_091_216u64),
        (75, 765_228_038),
        (115, 1_173_349_659),
    ];
    for (reserve_sol, breakeven) in cases {
        let reserve = reserve_sol * SOL;
        let check = SandwichCheck::of(&EntryQuote::public(0, reserve));
        assert_eq!(check.breakeven_lamports, breakeven, "y = {reserve_sol} SOL");
        assert_eq!(
            check.beta_threshold_micros, 10_102,
            "φ / (1 - φ) at 100 bps"
        );

        // Rounded up, so the figure itself is the first size over the line.
        let over = SandwichCheck::of(&EntryQuote::public(breakeven, reserve));
        let under = SandwichCheck::of(&EntryQuote::public(breakeven - 1, reserve));
        assert!(over.above_threshold, "y = {reserve_sol} SOL, at breakeven");
        assert!(!under.above_threshold, "y = {reserve_sol} SOL, one under");

        // The two reported millionths are for reading and the boolean above is
        // the answer. At the breakeven they disagree by exactly one step —
        // `beta_micros` floors and `beta_threshold_micros` rounds up — which is
        // the asymmetry §15.2 names when it says there is no sign to assert at
        // the threshold. Anywhere off the boundary they agree.
        assert!(over.beta_micros + 1 >= over.beta_threshold_micros);
        let clear = SandwichCheck::of(&EntryQuote::public(2 * breakeven, reserve));
        assert!(clear.above_threshold);
        assert!(clear.beta_micros > clear.beta_threshold_micros);
    }
}

/// The guard refuses our own order rather than the launch.
#[test]
fn an_order_worth_front_running_does_not_go_out_in_public() {
    let record = a_group();
    let launch_reserve = 30 * SOL;

    let refused = gate(&record, Some(&EntryQuote::public(SOL / 2, launch_reserve)));
    assert_eq!(refused.reason, GateReason::SandwichRisk);
    assert!(!refused.enter);
    assert!(refused.sandwich.expect("priced").refuses());

    // The same launch, the same curve, a smaller order.
    let small = gate(&record, Some(&EntryQuote::public(SOL / 10, launch_reserve)));
    assert_eq!(small.reason, GateReason::Accepted);

    // The same order as a private bundle: still priced, because §15.4's use for
    // the number is justifying the tip, and no longer a refusal.
    let private = gate(&record, Some(&EntryQuote::private(SOL / 2, launch_reserve)));
    assert_eq!(private.reason, GateReason::Accepted);
    let check = private.sandwich.expect("priced anyway");
    assert!(check.above_threshold);
    assert!(!check.refuses());
}

/// A curve nobody read and a curve that was read and cleared are different
/// answers, and which of them blocks is a setting.
#[test]
fn a_missing_quote_is_a_policy_decision_rather_than_a_pass() {
    let record = a_group();

    assert_eq!(gate(&record, None).reason, GateReason::Accepted);
    assert_eq!(gate(&record, None).sandwich, None);

    let required = GateParams {
        sandwich_guard: SandwichGuard::Required,
        ..GateParams::default()
    };
    let unquoted = evaluate(&record, &ClusterParams::default(), &required, None).1;
    assert_eq!(unquoted.reason, GateReason::NoCurveQuote);
    assert!(!unquoted.enter);

    let off = GateParams {
        sandwich_guard: SandwichGuard::Off,
        ..GateParams::default()
    };
    let huge = EntryQuote::public(20 * SOL, 30 * SOL);
    let ignored = evaluate(&record, &ClusterParams::default(), &off, Some(&huge)).1;
    assert_eq!(ignored.reason, GateReason::Accepted);
    assert_eq!(ignored.sandwich, None);
}

// ===========================================================================
// Determinism, and the discipline the numbers depend on
// ===========================================================================

/// §7.2 and P9: two runs over one record produce identical bytes, verdict
/// included.
#[test]
fn two_runs_of_one_record_agree_to_the_byte() {
    for record in [a_group(), a_ring(), population(&[90, 10])] {
        let quote = EntryQuote::public(SOL / 2, 30 * SOL);
        let first = evaluate(
            &record,
            &ClusterParams::default(),
            &GateParams::default(),
            Some(&quote),
        );
        let second = evaluate(
            &record,
            &ClusterParams::default(),
            &GateParams::default(),
            Some(&quote),
        );
        assert_eq!(
            serde_json::to_string(&first.0).expect("report serialises"),
            serde_json::to_string(&second.0).expect("report serialises"),
        );
        assert_eq!(
            serde_json::to_string(&first.1).expect("verdict serialises"),
            serde_json::to_string(&second.1).expect("verdict serialises"),
        );
    }
}

/// Reshuffling the record changes nothing: every order in the analyser is total
/// down to the wallet address, and the two new columns are read off the slice
/// §2.2 already sorted.
#[test]
fn the_order_the_buyers_arrived_in_does_not_move_the_new_columns() {
    let record = a_ring();
    let mut shuffled = record.clone();
    shuffled.buyers.reverse();
    shuffled.funding.reverse();

    let straight = read(&record);
    let backwards = read(&shuffled);
    assert_eq!(straight.concentration, backwards.concentration);
    assert_eq!(
        straight.clusters.first().map(|c| c.holding_entropy_micros),
        backwards.clusters.first().map(|c| c.holding_entropy_micros),
    );
    assert_eq!(
        syndicate_gate(&straight, &GateParams::default(), None).rings,
        syndicate_gate(&backwards, &GateParams::default(), None).rings,
    );
}

/// Every reason the gate can give is in the list a funnel prints, and the list
/// is in the order the checks run.
#[test]
fn the_reason_list_covers_every_answer_the_gate_gives() {
    let mut sorted = GateReason::ALL;
    sorted.sort();
    assert_eq!(sorted, GateReason::ALL, "ALL is total and worst-first");

    for reason in GateReason::ALL {
        assert!(!reason.as_str().is_empty());
    }

    // The three answers this branch added, in the spelling they serialise with.
    assert!(GateReason::ALL.contains(&GateReason::CoordinatedRing));
    assert!(GateReason::ALL.contains(&GateReason::NoCurveQuote));
    assert!(GateReason::ALL.contains(&GateReason::SandwichRisk));
    assert_eq!(
        serde_json::to_string(&GateReason::CoordinatedRing).expect("serialises"),
        "\"coordinated-ring\"",
    );
}

/// Nothing that scores a launch or prices a bundle has floating point in it,
/// and the one exception is named.
///
/// This is a source-level check on purpose. Everything above compares integers
/// against published figures, and every one of those comparisons stays true if
/// somebody quietly computes an intermediate in `f64` — right up until two
/// machines disagree in the last bit about whether a launch cleared a threshold.
/// The rule is easier to enforce than to detect, so it is enforced.
///
/// The surface is the strategy module plus the three files the execution side
/// prices with: [`fixed`](sts::fixed), which used to live inside `strategy` and
/// moved out when both halves started depending on it, and
/// [`jito`](sts::jito) and [`bundle`](sts::bundle), which turn slot evidence
/// into a lamport. Those two carry their own copy of this check so that
/// `cargo test --lib jito` proves it on its own; this is the list that knows
/// about all of them at once, and the one that notices a fourth file appearing.
#[test]
fn nothing_that_scores_or_prices_computes_in_floating_point() {
    // §7.2's step between computation and storage, which is the whole exception:
    // the schema column is an `f32`, so something has to make one.
    const ALLOWED: [&str; 2] = [
        "fn store_unit(micros: u64) -> f32 {",
        "ten_thousandths as f32 / 10_000.0",
    ];

    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for entry in std::fs::read_dir(src.join("strategy")).expect("the strategy module is there") {
        paths.push(entry.expect("a readable entry").path());
    }
    // The shared kernel and the two modules that price with it. Named rather
    // than globbed: `src` is full of files that legitimately hold a float, and
    // a check that swept all of them would be a check nobody could keep green.
    for shared in ["fixed.rs", "jito.rs", "bundle.rs"] {
        paths.push(src.join(shared));
    }

    let mut offenders: Vec<String> = Vec::new();
    let mut files = 0;

    for path in paths {
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        files += 1;
        let source = std::fs::read_to_string(&path).expect("readable source");
        // The tests below the line are allowed floats: they cross-check the
        // fixed-point answers against the formulas they replace, which is the
        // only place a float belongs in this module.
        let code = source
            .split("#[cfg(test)]")
            .next()
            .expect("split always yields one");
        for line in code.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") {
                continue;
            }
            if !(trimmed.contains("f64") || trimmed.contains("f32")) {
                continue;
            }
            if ALLOWED.iter().any(|allowed| trimmed.contains(allowed)) {
                continue;
            }
            offenders.push(format!("{}: {trimmed}", path.display()));
        }
    }

    // Seven: `mod.rs`, `syndicate.rs`, `entry.rs` and `social.rs` under
    // `strategy`, and the three shared files named above. The count is asserted
    // so that a new file under `strategy` trips this test rather than quietly
    // escaping it — sizing and the story weighting joined the guard that way.
    assert_eq!(
        files, 7,
        "the float-free surface changed shape: {files} files read"
    );
    assert!(
        offenders.is_empty(),
        "floating point crept in:\n{}",
        offenders.join("\n")
    );
}

/// The naming rule this module follows, and the one place it is allowed to
/// bend.
///
/// `daemon::LaunchOutcome` is a camel-case document and it embeds
/// [`SandwichCheck`] whole, so that type is renamed and `e2e_integration.rs`'s
/// `the_whole_report_is_readable_with_one_naming_rule` walks the daemon report
/// to prove the rename took. This is the same walk from the other side, and it
/// exists because the rename has a cost the daemon's test cannot see: every
/// other struct here serialises in snake case, so `SandwichCheck` is a camel
/// case island inside `GateVerdict`, which is a public type callers serialise.
///
/// The island is deliberate and documented on the type. What is not acceptable
/// is a *second* one appearing by accident — somebody copying the attribute onto
/// a neighbouring struct, or a new type embedded from another module. So this
/// pins the bend to exactly one field: `sandwich` is camel case all the way
/// down, everything else is snake case all the way down, and there is no third
/// answer anywhere in the document.
#[test]
fn the_one_camel_cased_type_in_this_module_is_the_only_one() {
    let record = a_group();
    let quote = EntryQuote::public(SOL / 2, 30 * SOL);
    let (report, verdict) = evaluate(
        &record,
        &ClusterParams::default(),
        &GateParams::default(),
        Some(&quote),
    );

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
    keys(
        &serde_json::to_value(&report).expect("the report serialises"),
        "report",
        &mut found,
    );
    keys(
        &serde_json::to_value(&verdict).expect("the verdict serialises"),
        "verdict",
        &mut found,
    );
    assert!(
        found.len() > 30,
        "the module lost a column: {} keys",
        found.len()
    );

    let mut camel_cased = Vec::new();
    for (path, key) in &found {
        // `sandwich` itself is the field on `GateVerdict`, so it is snake case
        // like its neighbours; everything *under* it is the embedded type.
        let inside_the_exception = path.contains(".sandwich.");
        let is_camel = key.chars().any(|c| c.is_ascii_uppercase());
        assert!(
            !key.chars().next().is_some_and(|c| c.is_ascii_uppercase()),
            "{path} is neither convention"
        );
        if inside_the_exception {
            assert!(
                !key.contains('_'),
                "{path} is inside the camel-cased type and is snake case"
            );
            camel_cased.push(path.clone());
        } else {
            assert!(
                !is_camel,
                "{path} is a second camel-cased island, and the module has room for one"
            );
        }
    }

    // And the exception is actually exercised: a test that asserted a rule
    // against an empty set would pass for the wrong reason.
    assert_eq!(
        camel_cased.len(),
        8,
        "the exception is the eight fields on the check and nothing else: {camel_cased:?}"
    );
    assert!(camel_cased
        .iter()
        .any(|path| path.ends_with(".aboveThreshold")));
}

/// The same check, one level down: the type itself, on its own, with every
/// field named.
#[test]
fn the_sandwich_check_names_every_field_in_camel_case() {
    let check = SandwichCheck::of(&EntryQuote::public(SOL / 2, 30 * SOL));
    let json = serde_json::to_value(check).expect("it serialises");
    let map = json.as_object().expect("an object");

    let expected = [
        "grossLamports",
        "virtualSolReserves",
        "feeBps",
        "privateBundle",
        "betaMicros",
        "betaThresholdMicros",
        "breakevenLamports",
        "aboveThreshold",
    ];
    for name in expected {
        assert!(
            map.contains_key(name),
            "{name} is missing: {:?}",
            map.keys().collect::<Vec<_>>()
        );
    }
    assert_eq!(
        map.len(),
        expected.len(),
        "a field was added without a name in this list"
    );

    // And it reads back, so the rename is a wire change and not a one-way door.
    let back: SandwichCheck = serde_json::from_value(json).expect("it reads back");
    assert_eq!(back, check);
}
