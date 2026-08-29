//! The forensic pair from outside the crate.
//!
//! `clustering.rs` and `tracer.rs` carry their own unit tests, and those cover
//! the arithmetic. What this file covers is what a *caller* sees: the shapes
//! that cross the IPC boundary, the invariants a window is entitled to rely on
//! without reading the implementation, and one launch put through the whole
//! path from raw transfers to a finding.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use sts_lib::chainproof::{Attestation, ChainTransaction, ChainTransfer, PROOF_SCHEMA};
use sts_lib::clustering::{
    analyse, ClusterGraphReport, ClusterParticipant, ClusterRegistry, ClusterRequest,
    ClusteringParams, InsiderReason, LaunchContext, NodeLabel, TraceRequest,
};
use sts_lib::telemetry::{TelemetryEvent, TelemetryHub, TelemetryLevel, TelemetrySink};
use sts_lib::tracer::{
    Asset, FundingGraph, NodeKind, TraceBudget, TraceEdge, TracePolicy, WalletTrace,
};

const LAUNCH: i64 = 1_700_000_000_000;
const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const SOL: u64 = 1_000_000_000;
const SUPPLY: u64 = 1_000_000_000_000_000;

fn edge(from: &str, to: &str, lamports: u64, at_ms: i64, signature: &str) -> TraceEdge {
    TraceEdge {
        from: from.to_string(),
        to: to.to_string(),
        lamports,
        at_ms,
        slot: 0,
        signature: signature.to_string(),
        asset: Asset::Sol,
        confidence_micros: 1_000_000,
    }
}

fn buyer(wallet: &str, first_buy_ms: i64, buy: u64, balance: u64) -> ClusterParticipant {
    ClusterParticipant {
        wallet: wallet.to_string(),
        first_buy_ms,
        buy_volume_lamports: buy,
        sell_volume_lamports: 0,
        token_balance: balance,
        buys: 1,
    }
}

/// One launch with three populations in it, which is what a real one looks like:
/// an operator running a bundle of puppets, a crowd that came out of an
/// exchange, and a couple of wallets nothing in the record explains.
///
/// The operator launders through one intermediary per puppet, funds each with
/// the same amount, and every puppet opens inside two seconds of the launch.
/// The dev wallet is paid by the same operator, two hops back.
fn syndicate_launch() -> (Vec<TraceEdge>, Vec<NodeLabel>, Vec<ClusterParticipant>) {
    let mut edges = Vec::new();
    let mut participants = Vec::new();

    // The operator's own money comes out of an exchange. Nothing behind that
    // exchange may ever be linked to any of this.
    edges.push(edge(
        "binance-hot",
        "operator",
        400 * SOL,
        LAUNCH - 20 * HOUR,
        "op-in",
    ));

    for index in 0..5i64 {
        let middle = format!("middle{index}");
        let puppet = format!("puppet{index}");
        edges.push(edge(
            "operator",
            &middle,
            4 * SOL,
            LAUNCH - 6 * HOUR,
            &format!("hop-a{index}"),
        ));
        edges.push(edge(
            &middle,
            &puppet,
            3 * SOL,
            LAUNCH - 2 * HOUR,
            &format!("hop-b{index}"),
        ));
        participants.push(buyer(
            &puppet,
            LAUNCH + index * 400,
            6 * SOL,
            SUPPLY / 100 * 3,
        ));
    }

    // The dev, paid by the same hand through its own intermediary.
    edges.push(edge(
        "operator",
        "dev-middle",
        9 * SOL,
        LAUNCH - 7 * HOUR,
        "dev-a",
    ));
    edges.push(edge(
        "dev-middle",
        "dev",
        8 * SOL,
        LAUNCH - 5 * HOUR,
        "dev-b",
    ));

    // A crowd out of an exchange, buying at their own pace with their own sizes.
    for index in 0..8i64 {
        let wallet = format!("public{index}");
        edges.push(edge(
            "binance-hot",
            &wallet,
            (index as u64 + 1) * SOL,
            LAUNCH - (10 - index) * HOUR,
            &format!("cex-out{index}"),
        ));
        participants.push(buyer(
            &wallet,
            LAUNCH + 5 * MINUTE + index * 3 * MINUTE,
            (index as u64 + 1) * SOL / 2,
            SUPPLY / 1_000,
        ));
    }

    // Two wallets the record cannot explain.
    participants.push(buyer("orphan0", LAUNCH + MINUTE, 2 * SOL, SUPPLY / 500));
    participants.push(buyer("orphan1", LAUNCH + 9 * MINUTE, SOL, SUPPLY / 500));

    let labels = vec![NodeLabel {
        address: "binance-hot".to_string(),
        kind: NodeKind::Exchange,
    }];

    (edges, labels, participants)
}

fn analyse_launch(migration_ms: Option<i64>) -> ClusterGraphReport {
    let (edges, labels, participants) = syndicate_launch();
    ClusterRequest {
        context: LaunchContext {
            mint: "So11111111111111111111111111111111111111112".to_string(),
            launch_ms: LAUNCH,
            migration_ms,
            circulating_supply: SUPPLY,
            dev_wallet: Some("dev".to_string()),
        },
        participants,
        edges,
        labels,
        witness: Vec::new(),
        verification: None,
        policy: None,
        budget: None,
        params: None,
    }
    .analyse()
    .expect("the request describes a launch")
}

// =======================================================================
// One launch, end to end
// =======================================================================

#[test]
fn a_laundered_syndicate_is_found_and_the_public_crowd_is_not_mistaken_for_one() {
    let report = analyse_launch(Some(LAUNCH + 3 * HOUR));

    // Two groups resolve: the operator's five puppets, and the eight wallets
    // that came out of the exchange. Only one of them is a claim about
    // ownership.
    let operator = report
        .clusters
        .iter()
        .find(|cluster| cluster.root == "operator")
        .expect("the operator's cluster");
    let exchange = report
        .clusters
        .iter()
        .find(|cluster| cluster.root == "binance-hot")
        .expect("the exchange's cluster");

    assert_eq!(operator.wallet_count, 5);
    assert!(!operator.shared_hub);
    assert_eq!(operator.root_kind, NodeKind::Wallet);

    assert_eq!(exchange.wallet_count, 8);
    assert!(exchange.shared_hub, "a hot wallet is not an owner");
    assert_eq!(exchange.root_kind, NodeKind::Exchange);

    // The finding is the operator's, and never the exchange's, however many
    // wallets came out of it.
    let insider = report.insider.as_ref().expect("a finding");
    assert_eq!(insider.root, "operator");
    assert!(insider.reasons.contains(&InsiderReason::SynchronisedOpen));
    assert!(insider.reasons.contains(&InsiderReason::UniformFunding));
    assert!(insider.reasons.contains(&InsiderReason::DevSharesOrigin));
    assert!(insider
        .reasons
        .contains(&InsiderReason::PreMigrationAccumulation));

    // The dev traces back through its own intermediary to the same hand.
    let dev = report.dev.as_ref().expect("a dev trace");
    assert_eq!(dev.origin.as_deref(), Some("operator"));
    assert_eq!(dev.hops, 2);
    assert_eq!(dev.siblings.len(), 5);
    assert_eq!(dev.cluster_root.as_deref(), Some("operator"));

    // The two unexplained wallets are counted, not filed under anybody.
    assert_eq!(report.unclustered_wallets, 2);
    assert_eq!(report.unattributed_volume_lamports, 3 * SOL);
    assert_eq!(report.participants, 15);
}

#[test]
fn nothing_behind_the_exchange_is_linked_to_anything_in_front_of_it() {
    // The operator was funded by the same hot wallet as the crowd. If the
    // exchange were transitable, every public buyer would be in the operator's
    // cluster and the finding would be nonsense.
    let report = analyse_launch(Some(LAUNCH + 3 * HOUR));

    let operator = report
        .clusters
        .iter()
        .find(|cluster| cluster.root == "operator")
        .expect("the operator's cluster");
    let members: BTreeSet<&str> = operator.wallets.iter().map(String::as_str).collect();
    for index in 0..8 {
        assert!(
            !members.contains(format!("public{index}").as_str()),
            "a wallet from the exchange crowd got into the operator's cluster"
        );
    }

    // And the operator's own trail stops at the exchange rather than running
    // through it.
    let dev = report.dev.as_ref().expect("a dev trace");
    assert!(dev
        .trace
        .roots
        .iter()
        .all(|root| root.root != "binance-hot"));
}

#[test]
fn the_same_launch_without_a_migration_does_not_claim_one() {
    let report = analyse_launch(None);
    let insider = report.insider.as_ref().expect("a finding");
    assert!(!insider
        .reasons
        .contains(&InsiderReason::PreMigrationAccumulation));
    assert_eq!(report.migration_ms, None);
}

// =======================================================================
// What a caller is entitled to rely on
// =======================================================================

#[test]
fn two_runs_of_one_record_agree_to_the_byte() {
    // §15's P9 at the boundary a window actually calls across.
    let first = analyse_launch(Some(LAUNCH + 3 * HOUR));
    let second = analyse_launch(Some(LAUNCH + 3 * HOUR));
    assert_eq!(first, second);

    let encoded = serde_json::to_string(&first).expect("serialises");
    let re_encoded = serde_json::to_string(&second).expect("serialises");
    assert_eq!(encoded, re_encoded);
}

#[test]
fn a_report_survives_the_wire_in_the_shape_it_left() {
    // These structs exist to cross IPC, so the round trip is the contract. It is
    // also what every `Eq` in the module is for: a report that came back
    // different from the one that went out would be undetectable without it.
    let report = analyse_launch(Some(LAUNCH + 3 * HOUR));
    let json = serde_json::to_string(&report).expect("serialises");
    let back: ClusterGraphReport = serde_json::from_str(&json).expect("deserialises");
    assert_eq!(back, report);

    // And the field naming a window reads by.
    let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
    assert!(value.get("launchMs").is_some(), "camelCase on the wire");
    assert!(value.get("unclusteredWallets").is_some());
    assert_eq!(value["schema"], "sts.clustering.report.v1");
}

#[test]
fn the_fields_a_window_reads_off_a_dev_trace_are_named_on_the_wire() {
    // The window's creator strip renders the dev trace and the insider finding
    // by these exact keys, and until this test nothing connected the two ends.
    // `ui/test/suites/cluster.mjs` builds its own fixture by hand — it has to,
    // there is no engine in a headless browser — so a rename here, or the
    // `camelCase` attribute coming off either struct, would leave all thirty-odd
    // of those assertions green and every cell in the strip an em dash. A
    // fixture agreeing with itself is not a contract. This is.
    let report = analyse_launch(Some(LAUNCH + 30 * MINUTE));
    let value = serde_json::to_value(&report).expect("serialises");

    let dev = value.get("dev").expect("a dev trace on the wire");
    for key in [
        "wallet",
        "origin",
        "originKind",
        "hops",
        "exitNode",
        "siblings",
        "siblingBuyLamports",
        "fundedBuyers",
        "fundedBuyLamports",
        "clusterRoot",
        "fundsCluster",
    ] {
        assert!(dev.get(key).is_some(), "the window reads dev.{key}");
    }
    // The one it reads a level further in, to say that a trail was budget-bound
    // and its numbers are therefore lower bounds.
    assert!(
        dev["trace"].get("truncated").is_some(),
        "the window reads dev.trace.truncated"
    );

    let insider = value.get("insider").expect("a finding on the wire");
    for key in ["scoreMicros", "measuredWeightBps", "reasons", "truncated"] {
        assert!(insider.get(key).is_some(), "the window reads insider.{key}");
    }

    // The reasons travel by name rather than as an index, which is what lets a
    // window that has not been taught a new one still show it to an operator
    // instead of dropping it.
    let reasons = insider["reasons"].as_array().expect("a list");
    assert!(
        reasons.iter().all(|reason| reason.is_string()),
        "reasons travel by name: {reasons:?}"
    );
}

#[test]
fn a_trace_survives_the_wire_too() {
    let (edges, labels, _) = syndicate_launch();
    let report = TraceRequest {
        wallet: "puppet2".to_string(),
        reference_ms: LAUNCH,
        edges,
        labels,
        witness: Vec::new(),
        verification: None,
        policy: None,
        budget: None,
    }
    .run()
    .expect("answerable");
    let trace = report.trace;

    assert_eq!(trace.parent.as_deref(), Some("operator"));
    // No witness was supplied, so nothing was checked — and that is reported as
    // an absence rather than as a pass.
    assert!(report.proof.is_none());
    let json = serde_json::to_string(&trace).expect("serialises");
    let back: WalletTrace = serde_json::from_str(&json).expect("deserialises");
    assert_eq!(back, trace);
    assert_eq!(back.roots[0].best_path.len(), 2);
}

#[test]
fn the_order_the_transfers_arrived_in_does_not_move_a_single_number() {
    let (edges, labels, participants) = syndicate_launch();
    let policy = TracePolicy::default();
    let owned: Vec<(String, NodeKind)> = labels
        .iter()
        .map(|label| (label.address.clone(), label.kind))
        .collect();
    let context = LaunchContext {
        mint: "MintOfTheLaunch".to_string(),
        launch_ms: LAUNCH,
        migration_ms: Some(LAUNCH + 3 * HOUR),
        circulating_supply: SUPPLY,
        dev_wallet: Some("dev".to_string()),
    };

    let forward = analyse(
        &FundingGraph::build(edges.clone(), &owned, &policy),
        &participants,
        &context,
        &policy,
        &TraceBudget::default(),
        &ClusteringParams::default(),
    );

    for rotation in [1usize, 7, 13, edges.len() - 1] {
        let mut shuffled = edges.clone();
        shuffled.rotate_left(rotation);
        let mut people = participants.clone();
        people.reverse();
        let again = analyse(
            &FundingGraph::build(shuffled, &owned, &policy),
            &people,
            &context,
            &policy,
            &TraceBudget::default(),
            &ClusteringParams::default(),
        );
        assert_eq!(again, forward, "rotation {rotation} moved the report");
    }
}

#[test]
fn a_truncated_report_never_says_it_may_clear() {
    // §15's P14 at the boundary: whatever else a bound report contains, the
    // predicate a caller uses to decide it is safe must be false.
    let (edges, labels, participants) = syndicate_launch();
    let policy = TracePolicy::default();
    let owned: Vec<(String, NodeKind)> = labels
        .iter()
        .map(|label| (label.address.clone(), label.kind))
        .collect();
    let graph = FundingGraph::build(edges, &owned, &policy);

    let report = analyse(
        &graph,
        &participants,
        &LaunchContext {
            mint: "MintOfTheLaunch".to_string(),
            launch_ms: LAUNCH,
            migration_ms: None,
            circulating_supply: SUPPLY,
            dev_wallet: Some("dev".to_string()),
        },
        &policy,
        // One hop is not enough to get through the laundering layer.
        &TraceBudget {
            depth: 1,
            ..TraceBudget::default()
        },
        &ClusteringParams::default(),
    );

    assert!(report.truncated);
    let dev = report.dev.as_ref().expect("a dev trace");
    assert!(!dev.trace.may_clear());
    if let Some(insider) = &report.insider {
        assert!(insider.truncated);
    }
}

#[test]
fn the_registry_answers_for_a_mint_nobody_analysed_without_pretending() {
    let registry = ClusterRegistry::new();
    assert_eq!(registry.report("never-seen"), None);
    assert!(registry.summaries().is_empty());

    let report = analyse_launch(Some(LAUNCH + 3 * HOUR));
    let mint = report.mint.clone();
    registry.record(report.clone());

    assert_eq!(registry.report(&mint), Some(report.clone()));
    let summaries = registry.summaries();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].mint, mint);
    assert_eq!(summaries[0].top_root.as_deref(), Some("operator"));
    assert_eq!(summaries[0].dev_origin.as_deref(), Some("operator"));
    assert_eq!(
        summaries[0].insider_score_micros,
        report.insider.map(|finding| finding.score_micros)
    );
}

// =======================================================================
// The invariant the whole design rests on
// =======================================================================

/// Neither forensic module computes in floating point, and unlike
/// `strategy/` there is no exception.
///
/// A source-level check for the reason `strategy_tests.rs` gives about its own:
/// every assertion in this file compares integers against expected integers, and
/// all of them stay true if somebody quietly computes an intermediate in `f64` —
/// right up until two machines disagree in the last bit about whether a cluster
/// cleared a threshold. `strategy/` allows two lines because the database column
/// it feeds is an `f32`; nothing here writes to that column, so nothing here has
/// a reason to make one.
#[test]
fn neither_forensic_module_computes_in_floating_point() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders: Vec<String> = Vec::new();
    let mut files = 0;

    for name in ["tracer.rs", "clustering.rs", "chainproof.rs"] {
        let path = root.join(name);
        let source = std::fs::read_to_string(&path).expect("readable source");
        files += 1;

        // Below the line are the tests, and these two modules do not use floats
        // there either — but the split is kept so the rule stays about shipped
        // code, the way the sibling check does it.
        let code = source
            .split("#[cfg(test)]")
            .next()
            .expect("split always yields one");
        for (number, line) in code.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") {
                continue;
            }
            if trimmed.contains("f64") || trimmed.contains("f32") {
                offenders.push(format!("{name}:{}: {trimmed}", number + 1));
            }
        }
    }

    assert_eq!(files, 3, "a forensic module went missing");
    assert!(
        offenders.is_empty(),
        "floating point crept in:\n{}",
        offenders.join("\n")
    );
}

/// Every score is in a named unit and inside its interval, whatever the input.
///
/// The shapes are `RISK_AND_SYBIL_SPEC.md` §7.1's table: the ones that have
/// produced a NaN or an infinity in some implementation of these formulas.
/// There is no float here to produce one, which is the point — this asserts the
/// integer arithmetic does not overflow into the same failure by another route.
#[test]
fn no_degenerate_input_produces_a_number_outside_its_interval() {
    let policy = TracePolicy::default();
    let params = ClusteringParams::default();
    let budget = TraceBudget::default();

    let shapes: Vec<(&str, Vec<TraceEdge>, Vec<ClusterParticipant>)> = vec![
        ("empty everything", Vec::new(), Vec::new()),
        (
            "one wallet",
            vec![edge("origin", "w", SOL, LAUNCH - HOUR, "s1")],
            vec![buyer("w", LAUNCH, SOL, SUPPLY)],
        ),
        (
            "two wallets, no edges",
            Vec::new(),
            vec![
                buyer("w1", LAUNCH, SOL, SUPPLY / 2),
                buyer("w2", LAUNCH, SOL, SUPPLY / 2),
            ],
        ),
        (
            "a self-loop and nothing else",
            vec![edge("w", "w", SOL, LAUNCH - HOUR, "s1")],
            vec![buyer("w", LAUNCH, SOL, SUPPLY)],
        ),
        (
            "all balances zero",
            vec![
                edge("origin", "w1", SOL, LAUNCH - HOUR, "s1"),
                edge("origin", "w2", SOL, LAUNCH - HOUR, "s2"),
                edge("origin", "w3", SOL, LAUNCH - HOUR, "s3"),
            ],
            vec![
                buyer("w1", LAUNCH, SOL, 0),
                buyer("w2", LAUNCH, SOL, 0),
                buyer("w3", LAUNCH, SOL, 0),
            ],
        ),
        (
            "no buy volume at all",
            vec![
                edge("origin", "w1", SOL, LAUNCH - HOUR, "s1"),
                edge("origin", "w2", SOL, LAUNCH - HOUR, "s2"),
                edge("origin", "w3", SOL, LAUNCH - HOUR, "s3"),
            ],
            vec![
                buyer("w1", LAUNCH, 0, SUPPLY / 3),
                buyer("w2", LAUNCH, 0, SUPPLY / 3),
                buyer("w3", LAUNCH, 0, SUPPLY / 3),
            ],
        ),
        (
            "identical buy times",
            vec![
                edge("origin", "w1", SOL, LAUNCH - HOUR, "s1"),
                edge("origin", "w2", SOL, LAUNCH - HOUR, "s2"),
                edge("origin", "w3", SOL, LAUNCH - HOUR, "s3"),
            ],
            vec![
                buyer("w1", LAUNCH, SOL, SUPPLY / 3),
                buyer("w2", LAUNCH, SOL, SUPPLY / 3),
                buyer("w3", LAUNCH, SOL, SUPPLY / 3),
            ],
        ),
        (
            "one holder with everything",
            vec![
                edge("origin", "w1", SOL, LAUNCH - HOUR, "s1"),
                edge("origin", "w2", SOL, LAUNCH - HOUR, "s2"),
                edge("origin", "w3", SOL, LAUNCH - HOUR, "s3"),
            ],
            vec![
                buyer("w1", LAUNCH, SOL, SUPPLY),
                buyer("w2", LAUNCH, SOL, 0),
                buyer("w3", LAUNCH, SOL, 0),
            ],
        ),
    ];

    for (name, edges, participants) in shapes {
        let graph = FundingGraph::build(edges, &[], &policy);
        let report = analyse(
            &graph,
            &participants,
            &LaunchContext {
                mint: "MintOfTheLaunch".to_string(),
                launch_ms: LAUNCH,
                migration_ms: Some(LAUNCH + HOUR),
                circulating_supply: SUPPLY,
                dev_wallet: None,
            },
            &policy,
            &budget,
            &params,
        );

        for cluster in &report.clusters {
            assert!(cluster.flow_share_bps <= 10_000, "{name}");
            assert!(cluster.ownership_bps.unwrap_or(0) <= 10_000, "{name}");
            assert!(cluster.holding_hhi_bps.unwrap_or(0) <= 10_000, "{name}");
            assert!(
                cluster.holding_entropy_micros.unwrap_or(0) <= 1_000_000,
                "{name}"
            );
            assert!(cluster.sync_micros.unwrap_or(0) <= 1_000_000, "{name}");
            assert!(cluster.fund_micros.unwrap_or(0) <= 1_000_000, "{name}");
            assert!(
                cluster.launch_share_micros.unwrap_or(0) <= 1_000_000,
                "{name}"
            );
            assert!(
                cluster.temporal_influence_micros.unwrap_or(0) <= 1_000_000,
                "{name}"
            );
            // UNKNOWN is never dressed up as a zero on the way out.
            assert_ne!(cluster.holding_hhi_bps, Some(0), "{name}");
        }

        if let Some(insider) = &report.insider {
            assert!(insider.score_micros <= 1_000_000, "{name}");
            assert!(insider.measured_weight_bps <= 10_000, "{name}");
            assert!(insider.pre_migration_share_bps <= 10_000, "{name}");
            assert!(insider.measured_weight_bps > 0, "{name}");
        }

        assert!(
            report.launch_fund_micros.unwrap_or(0) <= 1_000_000,
            "{name}"
        );
        // Every participant is either in a cluster or counted as unresolved.
        let clustered: u32 = report.clusters.iter().map(|c| c.wallet_count).sum();
        assert_eq!(
            clustered + report.unclustered_wallets,
            report.participants,
            "{name}: a buyer went missing"
        );
    }
}

// =======================================================================
// Twenty-four hops
// =======================================================================

/// A laundering chain `origin -> h(n-1) -> ... -> h1 -> buyer`, built to sit
/// wherever the depth budget is. Minutes between hops so that twenty-four of
/// them still fit inside §3.2's 72-hour lookback and 6-hour `dt_hop`.
fn laundered_chain(hops: u32, buyer_wallet: &str) -> Vec<TraceEdge> {
    let mut edges = Vec::new();
    for index in 0..hops {
        let from = if index == 0 {
            "launderer".to_string()
        } else {
            format!("{buyer_wallet}-h{:02}", hops - index)
        };
        let to = if index + 1 == hops {
            buyer_wallet.to_string()
        } else {
            format!("{buyer_wallet}-h{:02}", hops - index - 1)
        };
        edges.push(edge(
            &from,
            &to,
            4 * SOL,
            LAUNCH - i64::from(hops - index) * MINUTE,
            &format!("{buyer_wallet}-s{index:03}"),
        ));
    }
    edges
}

#[test]
fn a_ring_laundered_through_twenty_hops_is_still_one_hand() {
    // The shape §Y.1 describes and the four-hop budget could not see: three
    // buyers, each paid down its own twenty-hop chain of fresh keypairs, all
    // three chains starting at one address.
    let mut edges = Vec::new();
    let mut participants = Vec::new();
    for index in 0..3i64 {
        let wallet = format!("laundered{index}");
        edges.extend(laundered_chain(20, &wallet));
        participants.push(buyer(
            &wallet,
            LAUNCH + index * 300,
            5 * SOL,
            SUPPLY / 100 * 4,
        ));
    }

    let report = ClusterRequest {
        context: LaunchContext {
            mint: "LaunderedMint1111111111111111111111111111111".to_string(),
            launch_ms: LAUNCH,
            migration_ms: Some(LAUNCH + 30 * MINUTE),
            circulating_supply: SUPPLY,
            dev_wallet: None,
        },
        participants,
        edges,
        labels: Vec::new(),
        witness: Vec::new(),
        verification: None,
        policy: None,
        budget: None,
        params: None,
    }
    .analyse()
    .expect("the request describes a launch");

    assert_eq!(report.clusters.len(), 1, "one hand, three costumes");
    let cluster = &report.clusters[0];
    assert_eq!(cluster.root, "launderer");
    assert_eq!(cluster.wallet_count, 3);
    assert!(!report.truncated, "twenty hops fits inside the budget");

    // Under the four-hop budget this was three UNKNOWN wallets and no cluster
    // at all — which is exactly the answer an attacker builds the chain to get.
    let shallow = ClusterRequest {
        context: LaunchContext {
            mint: "LaunderedMint1111111111111111111111111111111".to_string(),
            launch_ms: LAUNCH,
            migration_ms: Some(LAUNCH + 30 * MINUTE),
            circulating_supply: SUPPLY,
            dev_wallet: None,
        },
        participants: (0..3)
            .map(|index| {
                buyer(
                    &format!("laundered{index}"),
                    LAUNCH + index * 300,
                    5 * SOL,
                    SUPPLY / 100 * 4,
                )
            })
            .collect(),
        edges: (0..3)
            .flat_map(|index| laundered_chain(20, &format!("laundered{index}")))
            .collect(),
        labels: Vec::new(),
        witness: Vec::new(),
        verification: None,
        policy: None,
        budget: Some(TraceBudget {
            depth: 4,
            ..TraceBudget::default()
        }),
        params: None,
    }
    .analyse()
    .expect("the request describes a launch");

    assert!(shallow.clusters.is_empty());
    assert_eq!(shallow.unclustered_wallets, 3);
    assert!(shallow.truncated);
}

#[test]
fn a_deep_trail_is_reported_at_a_discount_rather_than_at_full_strength() {
    // The other half of the deepening: the trail is found *and* it is weaker
    // than the same money one hop away. A twenty-hop claim that scored like a
    // direct transfer would make the depth an attacker's tool rather than ours.
    let near = TraceRequest {
        wallet: "near".to_string(),
        reference_ms: LAUNCH,
        edges: laundered_chain(1, "near"),
        labels: Vec::new(),
        witness: Vec::new(),
        verification: None,
        policy: None,
        budget: None,
    }
    .run()
    .expect("answerable")
    .trace;

    let far = TraceRequest {
        wallet: "far".to_string(),
        reference_ms: LAUNCH,
        edges: laundered_chain(20, "far"),
        labels: Vec::new(),
        witness: Vec::new(),
        verification: None,
        policy: None,
        budget: None,
    }
    .run()
    .expect("answerable")
    .trace;

    assert_eq!(near.parent.as_deref(), Some("launderer"));
    assert_eq!(far.parent.as_deref(), Some("launderer"));
    assert_eq!(far.roots[0].hops, 20);
    assert!(
        far.roots[0].influence_micros < near.roots[0].influence_micros / 8,
        "twenty hops of distance must cost more than three eighths: {} against {}",
        far.roots[0].influence_micros,
        near.roots[0].influence_micros
    );

    // The discount is reported next to the number it discounted, so an operator
    // reading a weak deep trail can tell "far away" from "flimsy".
    assert_eq!(near.roots[0].hop_decay_micros, 1_000_000);
    assert!(far.roots[0].hop_decay_micros < 100_000);

    // A posterior is a ratio, so being the only root still makes it the parent
    // outright — distance discounts the influence, never the identification.
    assert_eq!(far.parent_posterior_micros, 1_000_000);
}

// =======================================================================
// The dev tracer
// =======================================================================

#[test]
fn a_dev_that_paid_for_its_own_book_is_named_as_the_funder_not_as_a_sibling() {
    // The least subtle launch there is, and the one a parent-only reading walks
    // straight past: the dev funds the opening buyers directly, so it is
    // nobody's child and everybody's parent.
    let mut edges = Vec::new();
    let mut participants = Vec::new();
    for index in 0..4i64 {
        let wallet = format!("book{index}");
        edges.push(edge(
            "dev",
            &wallet,
            3 * SOL,
            LAUNCH - 2 * HOUR,
            &format!("dev-out{index}"),
        ));
        participants.push(buyer(
            &wallet,
            LAUNCH + index * 300,
            2 * SOL,
            SUPPLY / 100 * 2,
        ));
    }

    let report = ClusterRequest {
        context: LaunchContext {
            mint: "DevFunded111111111111111111111111111111111".to_string(),
            launch_ms: LAUNCH,
            migration_ms: Some(LAUNCH + 20 * MINUTE),
            circulating_supply: SUPPLY,
            dev_wallet: Some("dev".to_string()),
        },
        participants,
        edges,
        labels: Vec::new(),
        witness: Vec::new(),
        verification: None,
        policy: None,
        budget: None,
        params: None,
    }
    .analyse()
    .expect("the request describes a launch");

    let dev = report.dev.expect("a dev trace");
    // Nothing funded the dev inside the window, so its own origin is UNKNOWN
    // and it has no siblings. That is precisely the state in which the old
    // reading had nothing to say about the launch.
    assert_eq!(dev.origin, None);
    assert!(dev.siblings.is_empty());

    // And this is what it should have been saying.
    assert_eq!(dev.funded_buyers, vec!["book0", "book1", "book2", "book3"]);
    assert_eq!(dev.funded_buy_lamports, 8 * SOL);
    assert!(dev.funds_cluster);

    let insider = report.insider.expect("a finding");
    assert_eq!(insider.root, "dev");
    assert!(insider.reasons.contains(&InsiderReason::DevFundedCluster));
    // Sharing an origin is a different claim and is not being made here.
    assert!(!insider.reasons.contains(&InsiderReason::DevSharesOrigin));
}

#[test]
fn a_dev_funding_nobody_reports_an_empty_list_rather_than_a_missing_one() {
    let report = analyse_launch(Some(LAUNCH + 30 * MINUTE));
    let dev = report.dev.expect("a dev trace");
    assert_eq!(dev.origin.as_deref(), Some("operator"));
    assert!(dev.funded_buyers.is_empty());
    assert_eq!(dev.funded_buy_lamports, 0);
    assert!(!dev.funds_cluster);
    // The sibling reading still works and still says what it said.
    assert!(!dev.siblings.is_empty());
}

// =======================================================================
// On-chain verification
// =======================================================================

/// The transactions the chain would serve if every claimed edge were true.
///
/// One [`ChainTransaction`] per signature carrying every transfer claimed under
/// it, which is what a `getTransaction` actually returns — the fixture is built
/// the way the real answer is shaped rather than one row per edge.
fn truthful_chain(edges: &[TraceEdge], provider: &str) -> Vec<Attestation> {
    let mut by_signature: BTreeMap<&str, ChainTransaction> = BTreeMap::new();
    for edge in edges {
        let transaction = by_signature
            .entry(edge.signature.as_str())
            .or_insert_with(|| ChainTransaction {
                signature: edge.signature.clone(),
                slot: edge.slot,
                block_time_ms: Some(edge.at_ms),
                succeeded: true,
                transfers: Vec::new(),
            });
        transaction.transfers.push(ChainTransfer {
            from: edge.from.clone(),
            to: edge.to.clone(),
            lamports: edge.lamports,
            asset: edge.asset.clone(),
        });
    }
    by_signature
        .into_values()
        .map(|transaction| Attestation::found(provider, transaction))
        .collect()
}

fn analyse_with_witness(witness: Vec<Attestation>) -> ClusterGraphReport {
    let (edges, labels, participants) = syndicate_launch();
    ClusterRequest {
        context: LaunchContext {
            mint: "So11111111111111111111111111111111111111112".to_string(),
            launch_ms: LAUNCH,
            migration_ms: Some(LAUNCH + 30 * MINUTE),
            circulating_supply: SUPPLY,
            dev_wallet: Some("dev".to_string()),
        },
        participants,
        edges,
        labels,
        witness,
        verification: None,
        policy: None,
        budget: None,
        params: None,
    }
    .analyse()
    .expect("the request describes a launch")
}

#[test]
fn a_launch_whose_every_edge_two_providers_confirm_is_the_only_kind_that_may_clear() {
    let (edges, _, _) = syndicate_launch();
    let mut witness = truthful_chain(&edges, "helius");
    witness.extend(truthful_chain(&edges, "quicknode"));

    let report = analyse_with_witness(witness);
    let proof = report.proof.as_ref().expect("a proof");

    assert!(proof.complete);
    assert_eq!(proof.contradicted, 0);
    assert_eq!(proof.confirmed, proof.claimed);
    assert!(
        report.may_clear(),
        "nothing truncated and everything confirmed"
    );
    assert!(report.summary().chain_verified);

    // Confirmation changes no confidence, so the finding is the same finding it
    // was before anybody checked. That is the point: verification is a gate on
    // what the report may be *used for*, not a thumb on the score.
    let unverified = analyse_launch(Some(LAUNCH + 30 * MINUTE));
    assert_eq!(report.clusters, unverified.clusters);
    assert_eq!(report.insider, unverified.insider);
}

#[test]
fn a_report_with_no_witness_says_nothing_was_checked_rather_than_nothing_was_wrong() {
    let report = analyse_launch(Some(LAUNCH + 30 * MINUTE));
    assert!(report.proof.is_none());
    assert!(!report.summary().chain_verified);
    assert_eq!(report.summary().contradicted_edges, 0);
    // The finding stands and may block. What it may not do is clear anything.
    assert!(report.insider.is_some());
    assert!(!report.may_clear());
}

#[test]
fn an_edge_the_chain_does_not_have_never_becomes_a_vertex() {
    // The operator's funding of one puppet's intermediary is asserted and the
    // chain says it never happened. That puppet must lose its origin — not keep
    // it at a discount, and not keep it because the graph was already built.
    let (edges, labels, participants) = syndicate_launch();
    let mut witness = truthful_chain(&edges, "helius");
    witness.extend(truthful_chain(&edges, "quicknode"));
    witness.retain(|attestation| attestation.signature != "hop-a0");
    witness.push(Attestation::absent("helius", "hop-a0"));
    witness.push(Attestation::absent("quicknode", "hop-a0"));

    let report = ClusterRequest {
        context: LaunchContext {
            mint: "So11111111111111111111111111111111111111112".to_string(),
            launch_ms: LAUNCH,
            migration_ms: Some(LAUNCH + 30 * MINUTE),
            circulating_supply: SUPPLY,
            dev_wallet: Some("dev".to_string()),
        },
        participants,
        edges,
        labels,
        witness,
        verification: None,
        policy: None,
        budget: None,
        params: None,
    }
    .analyse()
    .expect("the request describes a launch");

    let proof = report.proof.as_ref().expect("a proof");
    assert_eq!(proof.contradicted, 1);
    assert!(!proof.complete);
    assert!(!report.may_clear());
    assert_eq!(report.summary().contradicted_edges, 1);

    let contradicted: Vec<&str> = proof
        .contradictions()
        .map(|edge| edge.signature.as_str())
        .collect();
    assert_eq!(contradicted, vec!["hop-a0"]);

    // The dropped edge is gone from the graph, so `middle0` is now funded by
    // nobody and `puppet0` traces back to it rather than to the operator.
    let cluster = report
        .clusters
        .iter()
        .find(|cluster| cluster.root == "operator")
        .expect("the operator still runs the other four");
    assert_eq!(cluster.wallet_count, 4);
    assert!(!cluster.wallets.iter().any(|wallet| wallet == "puppet0"));

    // And the counts moved with it rather than being patched afterwards.
    let clean = analyse_launch(Some(LAUNCH + 30 * MINUTE));
    assert_eq!(report.graph.edges + 1, clean.graph.edges);
}

#[test]
fn one_provider_is_not_a_quorum_and_the_launch_is_read_at_a_discount() {
    let (edges, _, _) = syndicate_launch();
    let report = analyse_with_witness(truthful_chain(&edges, "helius"));

    let proof = report.proof.as_ref().expect("a proof");
    assert_eq!(proof.confirmed, 0);
    assert_eq!(proof.single_source, proof.claimed);
    assert_eq!(proof.contradicted, 0);
    assert!(!proof.complete);
    assert!(!report.may_clear());

    // Every edge is halved, so every path's confidence product falls — and the
    // clusters still stand, because a discount is not a deletion.
    let clean = analyse_launch(Some(LAUNCH + 30 * MINUTE));
    let discounted = &report
        .clusters
        .iter()
        .find(|cluster| cluster.root == "operator")
        .expect("still there");
    let full = clean
        .clusters
        .iter()
        .find(|cluster| cluster.root == "operator")
        .expect("still there");
    assert_eq!(discounted.wallet_count, full.wallet_count);
}

#[test]
fn a_witness_that_could_not_answer_is_unknown_and_not_a_contradiction() {
    let (edges, _, _) = syndicate_launch();
    let witness: Vec<Attestation> = edges
        .iter()
        .map(|edge| Attestation::unavailable("helius", &edge.signature, "quota exhausted"))
        .collect();

    let report = analyse_with_witness(witness);
    let proof = report.proof.as_ref().expect("a proof");
    assert_eq!(proof.contradicted, 0, "nobody said anything was wrong");
    assert_eq!(proof.confirmed, 0, "and nobody said anything was right");
    assert!(proof.unverified > 0);
    assert!(!report.may_clear());
    // Nothing was dropped: an unanswerable edge is still the best evidence
    // there is, and the launch is still read.
    assert_eq!(report.graph.edges, analyse_launch(None).graph.edges);
}

#[test]
fn a_truncated_walk_cannot_be_cleared_by_a_perfect_proof() {
    // The two gates are independent and both have to hold. A budget-bound
    // traversal over edges the chain confirms is still a lower bound.
    let (edges, labels, participants) = syndicate_launch();
    let mut witness = truthful_chain(&edges, "helius");
    witness.extend(truthful_chain(&edges, "quicknode"));

    let report = ClusterRequest {
        context: LaunchContext {
            mint: "So11111111111111111111111111111111111111112".to_string(),
            launch_ms: LAUNCH,
            migration_ms: Some(LAUNCH + 30 * MINUTE),
            circulating_supply: SUPPLY,
            dev_wallet: Some("dev".to_string()),
        },
        participants,
        edges,
        labels,
        witness,
        verification: None,
        policy: None,
        budget: Some(TraceBudget {
            depth: 1,
            ..TraceBudget::default()
        }),
        params: None,
    }
    .analyse()
    .expect("the request describes a launch");

    assert!(report.proof.as_ref().expect("a proof").complete);
    assert!(report.truncated);
    assert!(!report.may_clear());
}

#[test]
fn a_verified_report_survives_the_wire_with_its_proof_on_it() {
    let (edges, _, _) = syndicate_launch();
    let mut witness = truthful_chain(&edges, "helius");
    witness.extend(truthful_chain(&edges, "quicknode"));
    witness.push(Attestation::absent("helius", "hop-b1"));

    let report = analyse_with_witness(witness);
    let json = serde_json::to_string(&report).expect("serialises");
    let back: ClusterGraphReport = serde_json::from_str(&json).expect("deserialises");
    assert_eq!(back, report);
    assert!(json.contains("\"SPLIT\""), "the verdict travels by name");
    assert_eq!(back.proof.expect("a proof").schema, PROOF_SCHEMA);
}

#[test]
fn a_report_written_before_verification_existed_still_reads() {
    // The compatibility this has to keep: a stored report from the version that
    // had no proof field deserialises, and comes back saying nothing was
    // checked rather than failing to parse or claiming it was.
    let report = analyse_launch(Some(LAUNCH + 30 * MINUTE));
    let mut json = serde_json::to_value(&report).expect("serialises");
    json.as_object_mut().expect("an object").remove("proof");

    let back: ClusterGraphReport = serde_json::from_value(json).expect("deserialises");
    assert!(back.proof.is_none());
    assert!(!back.may_clear());
}

#[test]
fn the_chain_contradicting_a_request_is_its_own_telemetry_event() {
    // The roadmap's rule: conflicting payloads produce a contradiction event
    // and no overwrite. Folding it into the clustering line would be the
    // overwrite — an operator told "3 clusters, loudest X" has been told about
    // the launch and not about the evidence under it failing.
    #[derive(Default)]
    struct Collector {
        events: Mutex<Vec<TelemetryEvent>>,
    }
    impl TelemetrySink for Collector {
        fn deliver(&self, event: &TelemetryEvent) {
            self.events
                .lock()
                .expect("not poisoned")
                .push(event.clone());
        }
    }

    let hub = Arc::new(TelemetryHub::start());
    let collector = Arc::new(Collector::default());
    hub.observe(Arc::clone(&collector) as Arc<dyn TelemetrySink>);

    let registry = ClusterRegistry::with_telemetry(Arc::clone(&hub), 100_000);

    let (edges, _, _) = syndicate_launch();
    let mut witness = truthful_chain(&edges, "helius");
    witness.extend(truthful_chain(&edges, "quicknode"));
    witness.retain(|attestation| attestation.signature != "hop-a0");
    witness.push(Attestation::absent("helius", "hop-a0"));
    witness.push(Attestation::absent("quicknode", "hop-a0"));
    registry.record(analyse_with_witness(witness));

    // A clean run publishes the finding and nothing else.
    let (clean_edges, _, _) = syndicate_launch();
    let mut clean = truthful_chain(&clean_edges, "helius");
    clean.extend(truthful_chain(&clean_edges, "quicknode"));
    registry.record(analyse_with_witness(clean));

    // Joins the pump, so the list is complete rather than complete-so-far.
    hub.shutdown();
    let events = collector.events.lock().expect("not poisoned").clone();

    let contradictions: Vec<&TelemetryEvent> = events
        .iter()
        .filter(|event| event.source == "contradiction")
        .collect();
    assert_eq!(contradictions.len(), 1, "one run contradicted, one did not");
    assert_eq!(contradictions[0].level, TelemetryLevel::Warn);
    assert!(
        contradictions[0].message.contains("hop-a0"),
        "the event names the signature: {}",
        contradictions[0].message
    );

    // And the clustering line still went out for both, unchanged.
    assert_eq!(
        events
            .iter()
            .filter(|event| event.source == "clustering")
            .count(),
        2
    );
}
