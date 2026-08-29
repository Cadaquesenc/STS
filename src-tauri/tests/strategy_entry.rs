//! The whole strategy path over a corpus, through the public API only.
//!
//! `strategy/syndicate.rs`, `strategy/social.rs` and `strategy/entry.rs` each
//! have the unit tests for their own arithmetic. These are the ones that only
//! mean anything across all three at once, over more than one launch:
//!
//!   * the shipped policy trades nothing, because no calibrated edge exists yet;
//!   * two runs over one corpus produce the same bytes, and reordering the
//!     opening buyers changes nothing;
//!   * a story can take size off a position and can never put any on;
//!   * every acceptance carries a priced exit, a positive stressed expectancy,
//!     and a size no larger than every cap that was applied to it.
//!
//! The corpus is eight launches aimed at different rungs of the analyser's own
//! funnel, so a change that quietly collapses two of its reasons into one fails
//! here rather than in a report nobody reads. The entry rule's rungs are
//! reached separately, in `each_of_the_entry_rules_own_rungs_can_be_reached`,
//! because most of them are properties of the account and the pool rather than
//! of any launch.

use std::collections::BTreeMap;

use sts_lib::replay::{CurveState, LAMPORTS_PER_SOL};
use sts_lib::strategy::entry::{
    plan_entry, Account, EntryDecision, EntryParams, EntryReason, Policy, Tier, MAX_POOL_SHARE_BPS,
};
use sts_lib::strategy::social::{SocialScan, SocialWeight, StoryKind, ViewSample};
use sts_lib::strategy::syndicate::{FundingEdge, GateReason, LaunchRecord, OpeningBuyer, RiskTag};
use sts_lib::strategy::{decide, ClusterReport, GateVerdict};
use sts_lib::types::{
    CircuitBreaker, FastPathGate, LiquidityThresholds, OperatingMode, RiskSnapshot,
};

const SOL: u64 = LAMPORTS_PER_SOL;
const NOW: i64 = 1_700_000_000_000;
const MINUTE: i64 = 60_000;

// ---------------------------------------------------------------------------
// the corpus
// ---------------------------------------------------------------------------

fn buyer(wallet: &str, lamports: u64, at_ms: i64) -> OpeningBuyer {
    OpeningBuyer {
        wallet: wallet.to_string(),
        sol_in_lamports: lamports,
        sol_out_lamports: 0,
        tx_count: 1,
        first_seen_ms: at_ms,
    }
}

fn launch(mint: &str, buyers: Vec<OpeningBuyer>) -> LaunchRecord {
    LaunchRecord {
        mint: mint.to_string(),
        creator: None,
        buyers,
        funding: Vec::new(),
    }
}

/// Eight launches, each one aimed at a different rung of the analyser's funnel.
fn corpus() -> Vec<LaunchRecord> {
    // Six wallets, one odd amount to the lamport, all in the same instant two
    // seconds after the launch. The shape the rule exists to find.
    let script = launch(
        "SCRIPT",
        (1..=6)
            .map(|n| buyer(&format!("w{n}"), 777_700_000, 2_000))
            .collect(),
    );

    // The same six, corroborated by a funding graph: one wallet paid for all of
    // them, which is the only hard evidence the analyser gets.
    let funded = LaunchRecord {
        mint: "FUNDED".to_string(),
        funding: (1..=6)
            .map(|n| FundingEdge {
                from: "PAYER".to_string(),
                to: format!("w{n}"),
                lamports: SOL,
            })
            .collect(),
        ..launch(
            "FUNDED",
            (1..=6)
                .map(|n| buyer(&format!("w{n}"), 777_700_000, 2_000))
                .collect(),
        )
    };

    // Nobody bought inside the window.
    let quiet = launch("QUIET", Vec::new());

    // Two buyers: too few to tell coordination from coincidence.
    let thin = launch(
        "THIN",
        vec![buyer("a", 2 * SOL, 500), buyer("b", 3 * SOL, 900)],
    );

    // Five buyers, spread out, all different sizes. An ordinary launch.
    let ordinary = launch(
        "ORDINARY",
        vec![
            buyer("a", SOL / 3, 200),
            buyer("b", 7 * SOL / 4, 700),
            buyer("c", SOL / 8, 1_300),
            buyer("d", 9 * SOL / 2, 1_900),
            buyer("e", SOL / 2, 2_600),
        ],
    );

    // A queue: six wallets racing one block with positions a factor of thirty-two
    // apart. Landing together is not coordination.
    let queue = launch(
        "QUEUE",
        vec![
            buyer("q1", SOL / 10, 2_000),
            buyer("q2", SOL / 5, 2_000),
            buyer("q3", 2 * SOL / 5, 2_000),
            buyer("q4", 4 * SOL / 5, 2_000),
            buyer("q5", 8 * SOL / 5, 2_000),
            buyer("q6", 16 * SOL / 5, 2_000),
        ],
    );

    // The script again, at a size the group could not move the price with.
    let poor = launch(
        "POOR",
        (1..=6)
            .map(|n| buyer(&format!("p{n}"), 177_700_000, 2_000))
            .collect(),
    );

    // A deployer buying its own launch with nobody else in the group.
    let solo_dev = LaunchRecord {
        creator: Some("DEV".to_string()),
        ..launch(
            "SOLODEV",
            vec![
                buyer("DEV", SOL, 100),
                buyer("x", 1_010_000_000, 150),
                buyer("y", 990_000_000, 180),
            ],
        )
    };

    vec![script, funded, quiet, thin, ordinary, queue, poor, solo_dev]
}

// ---------------------------------------------------------------------------
// the world the corpus is decided in
// ---------------------------------------------------------------------------

fn a_deep_curve() -> CurveState {
    CurveState::at_real_sol(40 * SOL)
}

fn a_healthy_snapshot() -> RiskSnapshot {
    RiskSnapshot {
        at_ms: NOW,
        // Paper: this build ships no signer, and the roadmap keeps the
        // dispatcher simulation-only until Phase 4 is explicitly promoted.
        mode: OperatingMode::Paper,
        equity_lamports: 200 * SOL,
        high_water_lamports: 200 * SOL,
        drawdown_bps: 0,
        max_drawdown_bps: 2_000,
        open_positions: 0,
        max_open_positions: 3,
        circuit_breaker: CircuitBreaker::Clear,
        fast_path: FastPathGate {
            allowed: true,
            remaining_in_window: 4,
            max_notional_lamports: SOL,
            max_slippage_bps: 250,
        },
        liquidity: LiquidityThresholds {
            min_pool_lamports: 5 * SOL,
            exit_only_below_lamports: SOL,
            max_pool_share_bps: 500,
        },
    }
}

/// A verdict the analyser accepted, at a confidence the caller picks. The entry
/// rule takes one of these and does not care how it was produced, which is what
/// makes the rungs below the gate's own threshold reachable.
fn accepted_verdict(confidence_micros: u64) -> GateVerdict {
    GateVerdict {
        enter: true,
        reason: GateReason::Accepted,
        confidence_micros,
        tags: vec![RiskTag::IdenticalSizing, RiskTag::SameInstantBundle],
        thin: false,
        bundle_wallets: 6,
        bundle_lamports: 5 * SOL,
        cohort_wallets: 6,
        cohort_lamports: 5 * SOL,
        cohort_size_lamports: Some(777_700_000),
        cohort_delta_bps: Some(0),
        cohort_external: 6,
        // The ring scan and the sandwich guard are the analyser's own
        // two extra refusals. A verdict built here has already passed
        // them; what they found is reported, and this suite is about
        // what happens after the gate rather than inside it.
        rings: Vec::new(),
        sandwich: None,
    }
}

fn refused_verdict() -> GateVerdict {
    GateVerdict {
        enter: false,
        reason: GateReason::MixedSizing,
        ..accepted_verdict(900_000)
    }
}

fn an_account() -> Account {
    Account {
        risk_budget_lamports: SOL / 2,
        free_equity_lamports: 100 * SOL,
        operator_max_notional_lamports: 10 * SOL,
    }
}

/// A policy with an edge nobody has measured, so the rungs past expectancy can
/// be reached at all. The hard cap is lifted with it, because Gate 6D's 0.05 SOL
/// would otherwise be the binding cap on every launch and hide the rest.
fn a_policy_with_an_imagined_edge() -> Policy {
    Policy {
        entry: EntryParams {
            edge_lcb_bps: 5_000,
            hard_cap_lamports: u64::MAX,
            ..EntryParams::default()
        },
        ..Policy::default()
    }
}

fn run(
    record: &LaunchRecord,
    scan: Option<&SocialScan>,
    policy: &Policy,
) -> (ClusterReport, GateVerdict, EntryDecision) {
    decide(
        record,
        scan,
        None,
        &a_healthy_snapshot(),
        &an_account(),
        &a_deep_curve(),
        policy,
        NOW,
    )
}

/// What the analyser's own gate said about each launch, counted.
fn gate_funnel(policy: &Policy) -> BTreeMap<&'static str, usize> {
    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    for record in corpus() {
        let (_, verdict, _) = run(&record, None, policy);
        *counts.entry(verdict.reason.as_str()).or_insert(0) += 1;
    }
    counts
}

// ---------------------------------------------------------------------------
// what the shipped build does
// ---------------------------------------------------------------------------

#[test]
fn the_shipped_policy_trades_nothing_because_no_edge_has_been_measured() {
    // `EntryParams::edge_lcb_bps` defaults to zero and a zero edge cannot cover
    // the cost of a round trip, so every launch that gets as far as expectancy
    // is refused there. This is the roadmap's Phase 3 gate as code: an envelope
    // becomes signable when a holdout produces a number for that field, and not
    // before.
    for record in corpus() {
        let (_, _, decision) = run(&record, None, &Policy::default());
        assert!(
            !decision.enter,
            "{} entered on the shipped policy: {:?}",
            record.mint, decision.reason
        );
        assert!(!decision.is_signable(NOW));
    }
}

#[test]
fn a_launch_that_reaches_expectancy_is_refused_there_and_not_earlier() {
    let (_, verdict, decision) = run(&corpus()[0], None, &Policy::default());
    assert!(verdict.enter, "the analyser accepted it");
    assert_eq!(decision.reason, EntryReason::NegativeStressedEv);
    // Everything upstream was computed, so the refusal can be shown next to the
    // numbers rather than asserted.
    assert!(decision.stress.measured);
    assert!(decision.exit.is_some());
    assert!(decision.size_lamports == 0, "a refusal carries no size");
    assert!(
        decision.caps.size_lamports > 0,
        "but the chain is still on it"
    );
}

// ---------------------------------------------------------------------------
// the funnel
// ---------------------------------------------------------------------------

#[test]
fn the_corpus_lands_on_more_than_one_rung_of_the_analysers_gate() {
    // The entry rule collapses every one of the analyser's refusals into
    // `gate-refused` on purpose — its own reason is on the verdict beside it,
    // and that is the pair a funnel prints. This asserts the pair is worth
    // printing: a corpus that landed on one rung would prove nothing about the
    // rest of the ladder.
    let counts = gate_funnel(&a_policy_with_an_imagined_edge());
    assert!(
        counts.len() >= 4,
        "the corpus collapsed onto {} rungs: {counts:?}",
        counts.len()
    );
    assert_eq!(counts.values().sum::<usize>(), corpus().len());
    assert!(counts.contains_key("accepted"), "{counts:?}");
    assert!(counts.contains_key("no-opening-buys"), "{counts:?}");
    assert!(counts.contains_key("thin"), "{counts:?}");
}

#[test]
fn each_of_the_entry_rules_own_rungs_can_be_reached() {
    // `plan_entry` rather than `decide` for the two that need a verdict the
    // analyser will not produce under its own thresholds: nothing clears the
    // gate below 0.6 confidence, so tier 3 and observe-only are reachable only
    // by handing the entry rule a verdict directly. That is what the two halves
    // being separate functions is for, and a rung nobody can reach is a rung
    // nobody has tested.
    let curve = a_deep_curve();
    let edge = EntryParams {
        edge_lcb_bps: 5_000,
        hard_cap_lamports: u64::MAX,
        ..EntryParams::default()
    };
    let plan = |verdict: &GateVerdict,
                snapshot: &RiskSnapshot,
                account: &Account,
                curve: &CurveState,
                params: &EntryParams,
                now_ms: i64| {
        plan_entry(
            verdict,
            &SocialWeight::unscanned(),
            snapshot,
            account,
            curve,
            params,
            now_ms,
        )
        .reason
    };

    let healthy = a_healthy_snapshot();
    let good = accepted_verdict(900_000);

    assert_eq!(
        plan(
            &refused_verdict(),
            &healthy,
            &an_account(),
            &curve,
            &edge,
            NOW
        ),
        EntryReason::GateRefused
    );
    assert_eq!(
        plan(
            &good,
            &RiskSnapshot {
                mode: OperatingMode::Halted,
                ..healthy
            },
            &an_account(),
            &curve,
            &edge,
            NOW
        ),
        EntryReason::EntriesBlocked
    );
    assert_eq!(
        plan(&good, &healthy, &an_account(), &curve, &edge, NOW + 10_000),
        EntryReason::StaleSnapshot
    );
    assert_eq!(
        plan(
            &good,
            &healthy,
            &an_account(),
            &CurveState::at_real_sol(SOL),
            &edge,
            NOW
        ),
        EntryReason::PoolTooThin
    );
    assert_eq!(
        plan(
            &accepted_verdict(400_000),
            &healthy,
            &an_account(),
            &curve,
            &edge,
            NOW
        ),
        EntryReason::ObserveOnly
    );
    assert_eq!(
        plan(
            &good,
            &healthy,
            &Account {
                free_equity_lamports: 1_000,
                ..an_account()
            },
            &curve,
            &edge,
            NOW
        ),
        EntryReason::BelowMinNotional
    );
    assert_eq!(
        plan(
            &good,
            &healthy,
            &an_account(),
            &curve,
            &EntryParams {
                emergency_max_slippage_bps: 1,
                ..edge.clone()
            },
            NOW
        ),
        EntryReason::ExitNotReady
    );
    assert_eq!(
        plan(
            &good,
            &healthy,
            &an_account(),
            &curve,
            &EntryParams::default(),
            NOW
        ),
        EntryReason::NegativeStressedEv
    );
    assert_eq!(
        plan(
            &accepted_verdict(600_000),
            &healthy,
            &an_account(),
            &curve,
            &edge,
            NOW
        ),
        EntryReason::OperatorConfirmationRequired
    );
    assert_eq!(
        plan(&good, &healthy, &an_account(), &curve, &edge, NOW),
        EntryReason::Accepted
    );
}

#[test]
fn every_reason_the_rule_can_give_has_a_name_of_its_own() {
    let mut names: Vec<&str> = EntryReason::ALL.iter().map(|r| r.as_str()).collect();
    let listed = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), listed, "two reasons share a name");
}

// ---------------------------------------------------------------------------
// determinism
// ---------------------------------------------------------------------------

#[test]
fn two_runs_over_one_corpus_produce_the_same_bytes() {
    // Phase 3's equivalence property, at the scale this module can carry it: the
    // decision is a function of the record, the scan, the policy and the clock
    // it was handed, and of nothing else.
    let policy = a_policy_with_an_imagined_edge();
    let scan = a_shared_story();
    let once: Vec<String> = corpus()
        .iter()
        .map(|record| {
            let (report, verdict, decision) = run(record, Some(&scan), &policy);
            serde_json::to_string(&(report, verdict, decision)).expect("serialises")
        })
        .collect();
    let twice: Vec<String> = corpus()
        .iter()
        .map(|record| {
            let (report, verdict, decision) = run(record, Some(&scan), &policy);
            serde_json::to_string(&(report, verdict, decision)).expect("serialises")
        })
        .collect();
    assert_eq!(once, twice);
}

#[test]
fn reordering_the_opening_buyers_does_not_change_the_plan() {
    // Every tie in the analyser falls through to the wallet address, so the
    // order the watcher happened to hand the buyers over in cannot decide a
    // size.
    let policy = a_policy_with_an_imagined_edge();
    let record = &corpus()[0];
    let mut shuffled = record.clone();
    shuffled.buyers.reverse();
    shuffled.buyers.swap(0, 3);

    let (_, _, forwards) = run(record, None, &policy);
    let (_, _, backwards) = run(&shuffled, None, &policy);
    assert_eq!(forwards, backwards);
}

// ---------------------------------------------------------------------------
// the story
// ---------------------------------------------------------------------------

fn a_shared_story() -> SocialScan {
    SocialScan {
        kind: StoryKind::Tweet,
        handle: Some("someone".to_string()),
        followers: Some(40_000),
        account_age_days: Some(700),
        post_age_ms: Some(30_000),
        reuse_nth: 3,
        views: vec![
            ViewSample {
                at_ms: 0,
                views: 1_000,
            },
            ViewSample {
                at_ms: 2 * MINUTE,
                views: 9_000,
            },
        ],
    }
}

fn the_best_story_there_is() -> SocialScan {
    SocialScan {
        reuse_nth: 1,
        followers: Some(4_000_000),
        views: vec![
            ViewSample {
                at_ms: 0,
                views: 1_000,
            },
            ViewSample {
                at_ms: 2 * MINUTE,
                views: 100_000,
            },
        ],
        ..a_shared_story()
    }
}

#[test]
fn no_story_can_make_a_position_larger_than_no_story_at_all() {
    // The invariant doctrine asks for, over the whole corpus: social
    // corroboration, never a safety override. A story that is unimprovable in
    // every measured dimension sizes exactly like a launch nobody scanned.
    let policy = a_policy_with_an_imagined_edge();
    for record in corpus() {
        let (_, _, unscanned) = run(&record, None, &policy);
        for scan in [
            the_best_story_there_is(),
            a_shared_story(),
            SocialScan::unreadable(),
            SocialScan::no_link(),
        ] {
            let (_, _, weighed) = run(&record, Some(&scan), &policy);
            assert!(
                weighed.size_lamports <= unscanned.size_lamports,
                "{} was sized up by a {:?} story",
                record.mint,
                scan.kind
            );
            assert!(weighed.caps.social_multiplier_bps <= 10_000);
        }
    }
}

#[test]
fn a_reused_story_takes_a_quarter_off_a_position_that_would_have_traded() {
    let policy = a_policy_with_an_imagined_edge();
    let record = &corpus()[0];
    let (_, _, clean) = run(record, Some(&the_best_story_there_is()), &policy);
    let (_, _, shared) = run(record, Some(&a_shared_story()), &policy);
    assert!(clean.enter && shared.enter);
    assert_eq!(shared.caps.social_multiplier_bps, 7_500);
    assert_eq!(shared.size_lamports, clean.size_lamports * 3 / 4);
}

// ---------------------------------------------------------------------------
// what an acceptance guarantees
// ---------------------------------------------------------------------------

#[test]
fn every_acceptance_carries_an_exit_a_tier_and_a_size_inside_every_cap() {
    let policy = a_policy_with_an_imagined_edge();
    let curve = a_deep_curve();
    let pool_cap = curve.max_position_lamports(MAX_POOL_SHARE_BPS);
    let mut accepted = 0usize;

    for record in corpus() {
        for scan in [None, Some(a_shared_story()), Some(SocialScan::unreadable())] {
            let (_, verdict, decision) = run(&record, scan.as_ref(), &policy);
            if !decision.enter {
                continue;
            }
            accepted += 1;

            assert!(verdict.enter, "{} entered without the gate", record.mint);
            assert!(decision.tier.is_automatic(), "{}", record.mint);
            assert_ne!(decision.tier, Tier::ObserveOnly);

            // Phase 2's fifth criterion: a precomputed emergency exit.
            let exit = decision.exit.expect("an acceptance has a priced exit");
            assert!(exit.tokens > 0 && exit.net_lamports > 0);
            assert!(exit.within_ceiling);

            // Positive stressed expectancy, and the risk gate's own caps.
            assert!(decision.ev.positive);
            assert!(decision.ev.stressed_ev_lamports > 0);
            assert!(decision.size_lamports >= policy.entry.min_notional_lamports);
            assert!(decision.size_lamports <= pool_cap, "{}", record.mint);
            assert!(decision.size_lamports <= an_account().free_equity_lamports);
            assert!(decision.size_lamports <= an_account().operator_max_notional_lamports);
            assert!(decision.size_lamports <= decision.caps.base_lamports);
            assert!(decision.max_slippage_bps <= policy.entry.max_slippage_bps);

            // And it is only actionable for as long as the policy says.
            assert!(decision.is_signable(NOW));
            assert!(!decision.is_signable(decision.expires_at_ms));
        }
    }
    assert!(
        accepted > 0,
        "the corpus proved nothing: nothing was accepted"
    );
}

#[test]
fn the_participation_cap_holds_across_the_corpus_whatever_the_snapshot_says() {
    // The snapshot in this world carries 500 bps, the laxer of the two numbers
    // in the codebase. Doctrine's 150 is what binds, on every launch.
    let policy = a_policy_with_an_imagined_edge();
    let ceiling = a_deep_curve().max_position_lamports(MAX_POOL_SHARE_BPS);
    for record in corpus() {
        let (_, _, decision) = run(&record, None, &policy);
        assert!(
            decision.caps.pool_lamports <= ceiling,
            "{} was allowed {} of the pool",
            record.mint,
            decision.caps.pool_lamports
        );
    }
}

#[test]
fn a_halted_engine_refuses_the_whole_corpus() {
    let policy = a_policy_with_an_imagined_edge();
    let halted = RiskSnapshot {
        mode: OperatingMode::Halted,
        ..a_healthy_snapshot()
    };
    for record in corpus() {
        let (_, _, decision) = decide(
            &record,
            None,
            None,
            &halted,
            &an_account(),
            &a_deep_curve(),
            &policy,
            NOW,
        );
        assert!(
            !decision.enter,
            "{} entered on a halted engine",
            record.mint
        );
        assert!(matches!(
            decision.reason,
            EntryReason::GateRefused | EntryReason::EntriesBlocked
        ));
    }
}
