//! Which buyers were one hand, and whether that hand was inside the launch.
//!
//! `tracer.rs` answers "where did this wallet's money come from" one wallet at a
//! time. This module asks the question a launch actually turns on: given every
//! opening buyer traced back to an origin, **which of them were the same
//! operator**, how much of the token did that operator end up holding, and did
//! the accumulation happen before the curve migrated — when the price was
//! still whatever the operator decided it was.
//!
//! # How a cluster gets built
//!
//! A wallet joins the cluster named by its own parent posterior, and only when
//! that posterior is a majority ([`ClusteringParams::min_parent_posterior_micros`]).
//! Wallets whose origin is UNKNOWN are **not** recruited by synchrony: buying
//! at the same instant as somebody else is what a bot service with fifty
//! customers looks like, and `backtest::cluster_by_funder` refuses the same
//! recruitment for the same reason. An unresolved wallet is counted in
//! [`ClusterGraphReport::unclustered_wallets`] and in every denominator, which
//! is what keeps a partly-traced launch from reading as a cleanly-traced one.
//!
//! # The three false positives this is built to survive
//!
//! **Everyone came out of the same exchange.** Structural, and handled in
//! `tracer.rs`: a path may end at an exchange, a bridge, a mixer or an inferred
//! router, and may never pass through one. A cluster whose root is one of those
//! is still reported — "every buyer here came out of one router" is worth
//! knowing — but it is flagged [`FundingCluster::shared_hub`] and is excluded
//! from insider scoring entirely. Sharing an exit node is not evidence of
//! common ownership; it is evidence of a popular exit node.
//!
//! **One operator with a lot of small customers.** Weighting by volume rather
//! than by wallet count, everywhere, exactly as §3.5 requires. Forty dust
//! wallets behind one root move the number less than two large ones, so
//! generating empty keypairs buys nothing.
//!
//! **A cluster that is really one wallet in costumes.** Two published shapes
//! catch it, and they are read in opposite directions on purpose. The *holdings*
//! side is `RISK_AND_SYBIL_SPEC.md` §14's `[0.9, 0.1]` population — an index of
//! 8 200 and an entropy of 0.4690 — reused from `strategy::syndicate` rather
//! than re-derived, and a cluster reaching both is one bidder and some
//! costumes. The *funding* side is the mirror image: money handed out in
//! near-identical amounts is a script, so there the tell is entropy near **one**
//! rather than near zero. A ring usually shows both, and either alone is worth
//! seeing.
//!
//! # Insider accumulation
//!
//! [`InsiderFinding`] is four measurements and a weighted mean of them, and the
//! weighting rule is the one `strategy::syndicate` uses for its confidence sum:
//! **a component that could not be measured is left out of the mean rather than
//! scored as zero, and the weights renormalise over what survived.** A missing
//! test is not a passed test. If either of the two primary components — the
//! wallets share a funder, and they moved together — is UNKNOWN, there is no
//! finding at all, because §3.5's geometric mean is explicit that both have to
//! be true and neither implies the other.
//!
//! The migration timestamp is what turns a cluster into an *insider* cluster.
//! Accumulation before migration is accumulation at a price the operator set;
//! after it, they are buying from the same book as everybody else. When no
//! migration has been observed the share is reported as the whole and the
//! reason does not fire — the launch has not migrated, so nothing has been shown
//! to precede it.
//!
//! # Determinism
//!
//! Every map iterated here is a `BTreeMap`, every sort falls through to an
//! address, every score is an integer in a named unit, and nothing calls a libm
//! function. Two runs over one record produce byte-identical reports, which is
//! why every struct in this file derives `Eq`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::backtest::{
    hhi_bps, mul_div_floor, mul_div_round, sync_micros, temporal_influence_micros,
    DEFAULT_TAU_SYNC_MS, MICROS,
};
use crate::chainproof::{verify_request, Attestation, LineageProof, VerificationPolicy};
use crate::error::EngineError;
use crate::strategy::fixed::weighted_entropy_micros;
use crate::strategy::syndicate::{RING_ENTROPY_MICROS, RING_HHI_BPS};
use crate::telemetry::{TelemetryHub, TelemetryLevel};
use crate::tracer::{
    funding_concentration, trace_wallet, FundingGraph, GraphSummary, NodeKind, TraceBudget,
    TraceEdge, TracePolicy, WalletTrace,
};
use crate::types::BPS_DENOMINATOR;

/// The schema tag every stored report carries, so a row read back can be told
/// apart from one a later shape produced.
pub const REPORT_SCHEMA: &str = "sts.clustering.report.v1";

/// The most reports the registry keeps. A forensic report is a few kilobytes
/// and the window only ever looks at a handful, so this is a memory ceiling
/// rather than a policy: past it, the least recently recorded mint is dropped.
pub const MAX_REPORTS: usize = 256;

// ===========================================================================
// Parameters
// ===========================================================================

/// The thresholds the clustering runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusteringParams {
    /// How much of a wallet's parent posterior has to point at one origin
    /// before the wallet is filed under it, in millionths.
    ///
    /// A majority by default. Below a half the wallet has two origins with
    /// comparable claims, and filing it under the larger one would be reporting
    /// a coin flip as a finding.
    pub min_parent_posterior_micros: u64,
    /// How many wallets make a cluster. Two addresses doing the same thing is a
    /// coincidence with a 50% base rate; the third is what makes it a pattern —
    /// the same call `strategy::syndicate::MIN_GROUP` makes.
    pub min_cluster_wallets: usize,
    /// The synchrony kernel's bandwidth.
    pub tau_sync_ms: u64,
    /// The cluster ownership share that scores full marks on the concentration
    /// component, in basis points.
    ///
    /// Not a threshold for anything — a scale. A cluster holding a fifth of the
    /// circulating supply is already as concentrated as this score can read, and
    /// without a ceiling the component would spend its whole range on the
    /// difference between 60% and 90%, which are the same answer.
    pub ownership_full_bps: u16,
    /// How close two funding amounts have to be to count as the same amount, in
    /// basis points. Matches `strategy::syndicate::SIZE_TOLERANCE_BPS`.
    pub ring_tolerance_bps: u64,
    /// The share of a cluster that has to be funded in near-identical amounts
    /// before it is called a split-wallet ring, in basis points.
    pub ring_share_bps: u16,
    /// The share of a cluster's buying that has to land before migration before
    /// the pre-migration reason fires, in basis points.
    pub min_pre_migration_share_bps: u16,
    /// The score at which a finding is loud enough to publish at warning level,
    /// in millionths.
    pub alert_score_micros: u64,
    /// Component weights, in basis points of the whole score.
    pub weight_sync_bps: u64,
    pub weight_fund_bps: u64,
    pub weight_ownership_bps: u64,
    pub weight_uniformity_bps: u64,
}

impl Default for ClusteringParams {
    fn default() -> Self {
        ClusteringParams {
            min_parent_posterior_micros: 500_000,
            min_cluster_wallets: 3,
            tau_sync_ms: DEFAULT_TAU_SYNC_MS,
            ownership_full_bps: 2_000,
            ring_tolerance_bps: 200,
            ring_share_bps: 8_000,
            min_pre_migration_share_bps: 8_000,
            alert_score_micros: 600_000,
            // The two primary signals carry more than half between them: §3.5's
            // point is that a shared funder and a synchronised open are the pair
            // that means one hand, and the other two are corroboration.
            weight_sync_bps: 2_500,
            weight_fund_bps: 3_000,
            weight_ownership_bps: 2_500,
            weight_uniformity_bps: 2_000,
        }
    }
}

// ===========================================================================
// Inputs
// ===========================================================================

/// One opening buyer, and what the recording knows about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterParticipant {
    pub wallet: String,
    /// Milliseconds since the epoch, on the one clock. Not an offset: the
    /// traversal windows are absolute and an offset would need a second
    /// convention to convert.
    pub first_buy_ms: i64,
    pub buy_volume_lamports: u64,
    pub sell_volume_lamports: u64,
    /// What this wallet holds of the mint now, in base units.
    pub token_balance: u64,
    pub buys: u32,
}

/// An address whose kind the caller already knows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeLabel {
    pub address: String,
    pub kind: NodeKind,
}

/// The launch everything here is measured against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchContext {
    pub mint: String,
    /// `t_ref`: the moment the decay and the lookback are measured from.
    pub launch_ms: i64,
    /// When the curve migrated, if it has. `None` is "no migration observed",
    /// which is not the same as "migrated at zero".
    pub migration_ms: Option<i64>,
    /// Base units in circulation, for the ownership share. Zero means the
    /// caller does not know, and every ownership figure comes back UNKNOWN
    /// rather than being divided by a guess.
    pub circulating_supply: u64,
    pub dev_wallet: Option<String>,
}

// ===========================================================================
// Findings
// ===========================================================================

/// Funding handed out in near-identical amounts: a script, not a syndicate of
/// people who each decided what to risk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitRing {
    /// Wallets whose funding sat within the tolerance of the median.
    pub matched_wallets: u32,
    pub cluster_wallets: u32,
    pub share_bps: u16,
    pub median_funding_lamports: u64,
    /// Normalised entropy over the funding amounts, in millionths. Near one is
    /// the tell here — the opposite direction from the holdings-side ring.
    pub uniformity_micros: u64,
}

/// Why a cluster scored the way it did. Serialised in the spelling the rest of
/// the risk vocabulary uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InsiderReason {
    /// A single origin accounts for a majority of this launch's buying.
    SharedFunder,
    /// The cluster's first buys land inside the synchrony kernel's bandwidth.
    SynchronisedOpen,
    /// The cluster holds a material share of the circulating supply.
    ConcentratedOwnership,
    /// The cluster was funded in near-identical amounts.
    UniformFunding,
    /// The cluster's buying landed before the curve migrated.
    PreMigrationAccumulation,
    /// §14's `[0.9, 0.1]` shape in the holdings: one wallet and some costumes.
    CostumeRing,
    /// The dev wallet traces back to the same origin as the cluster.
    DevSharesOrigin,
    /// The dev wallet **is** the cluster's origin: it paid for this book
    /// itself. Stronger than sharing an origin, and kept apart from
    /// [`InsiderReason::DevSharesOrigin`] for that reason — a dev and a buyer
    /// out of one exchange share an origin too.
    DevFundedCluster,
}

/// The four components, each UNKNOWN rather than zero when it could not be
/// measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InsiderComponents {
    pub sync_micros: Option<u64>,
    /// This origin's volume-weighted share of the **whole launch**, not of its
    /// own cluster. The cluster's own share is high by construction — its
    /// members were selected for pointing at this root — so it would measure
    /// the selection rather than the launch.
    pub launch_share_micros: Option<u64>,
    pub ownership_micros: Option<u64>,
    pub uniformity_micros: Option<u64>,
}

/// One cluster judged as an accumulation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InsiderFinding {
    pub root: String,
    pub wallets: u32,
    /// The weighted mean over the components that were measurable, in
    /// millionths.
    pub score_micros: u64,
    /// The weight that mean was taken over, in basis points. Below 10 000 means
    /// a component was UNKNOWN and left out, and a score resting on half the
    /// evidence should be read as one.
    pub measured_weight_bps: u64,
    pub components: InsiderComponents,
    /// Sorted, so two runs list them the same way.
    pub reasons: Vec<InsiderReason>,
    pub pre_migration_share_bps: u16,
    pub pre_migration_buy_lamports: u64,
    /// A traversal behind this was budget-bound. The score is a lower bound and
    /// may block an entry; it may never clear one.
    pub truncated: bool,
}

/// A set of buyers the traversal put behind one origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FundingCluster {
    /// The origin every member points at. It is the cluster's identity: two
    /// runs over one record produce the same roots, so no counter is needed and
    /// no id can drift.
    pub root: String,
    pub root_kind: NodeKind,
    /// The root is absorbing — an exchange, bridge, mixer or inferred router.
    ///
    /// Sharing an exit node is not evidence of common ownership. The cluster is
    /// reported because the shape is worth seeing, and it is excluded from
    /// insider scoring because the shape is not a claim.
    pub shared_hub: bool,
    /// Sorted.
    pub wallets: Vec<String>,
    pub wallet_count: u32,
    pub buy_volume_lamports: u64,
    pub sell_volume_lamports: u64,
    /// This cluster's share of the launch's whole buy volume.
    pub flow_share_bps: u16,
    pub token_balance: u64,
    /// Share of the circulating supply. `None` when the supply is unknown.
    pub ownership_bps: Option<u16>,
    /// §2.2 over the members' balances: how the cluster's own holdings are
    /// split between its members.
    pub holding_hhi_bps: Option<u16>,
    /// §2.3's `H / ln(n)` over the same balances.
    pub holding_entropy_micros: Option<u64>,
    /// §14's `[0.9, 0.1]` shape: both the index and the entropy reached.
    pub costume_ring: bool,
    pub funding_ring: Option<SplitRing>,
    pub sync_micros: Option<u64>,
    /// §3.5's `fund(C)` over this cluster's own members.
    ///
    /// High by construction — the members were selected for pointing here — so
    /// it reports the *strength* of the assignment and not whether one exists.
    /// [`InsiderComponents::launch_share_micros`] is the discriminating number.
    pub fund_micros: Option<u64>,
    /// This origin's volume-weighted share of the whole launch.
    pub launch_share_micros: Option<u64>,
    /// §3.5's `sqrt(sync x fund)`. `None` when either half is UNKNOWN — the
    /// geometric mean would return zero, and a zero here reads as "these
    /// wallets are unrelated", which is the opposite of what was learned.
    pub temporal_influence_micros: Option<u64>,
    pub first_buy_ms: i64,
    pub first_buy_span_ms: i64,
    pub pre_migration_buy_lamports: u64,
    pub pre_migration_wallets: u32,
    /// The funding each member received down its strongest path, sorted
    /// descending. The evidence the ring test is computed from.
    pub member_funding_lamports: Vec<u64>,
    pub truncated: bool,
}

/// Where the dev wallet's money came from, and who else it came to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevTrace {
    pub wallet: String,
    pub trace: WalletTrace,
    /// The dev's own origin, or `None` for UNKNOWN.
    pub origin: Option<String>,
    pub origin_kind: Option<NodeKind>,
    /// Hops from the dev wallet back to that origin.
    pub hops: u32,
    /// The absorbing node the trail ends at, when it ends at one. This is the
    /// exit node in the ordinary sense: the venue the money came out of.
    pub exit_node: Option<String>,
    /// Opening buyers that trace back to the dev's own origin, sorted.
    ///
    /// The question this answers is the one that matters about a launch: not
    /// "did the dev buy" but "who else was paid by whoever paid the dev".
    pub siblings: Vec<String>,
    pub sibling_buy_lamports: u64,
    /// Opening buyers whose own origin is the **dev wallet itself**, sorted.
    ///
    /// The sibling test above cannot see this shape, and the reason is
    /// structural rather than an oversight: siblings are wallets sharing the
    /// dev's *parent*, so a dev that is nobody's child and everybody's parent
    /// comes back with an empty sibling list and an origin of UNKNOWN. That is
    /// the launch where the deployer funded its own opening book directly out
    /// of the wallet that deployed — the least subtle version of the thing this
    /// module exists to find, and the one a parent-only reading walks straight
    /// past.
    ///
    /// Kept as a separate list rather than merged into `siblings` because the
    /// two are different claims. A sibling shares a funder with the dev; these
    /// wallets *were funded by* the dev, and only the second one names the dev
    /// as the operator rather than as another customer.
    pub funded_buyers: Vec<String>,
    pub funded_buy_lamports: u64,
    /// The dev's origin is also a cluster root in this report.
    pub cluster_root: Option<String>,
    /// The dev wallet is itself a cluster root in this report — the operator
    /// being the origin rather than sharing one.
    pub funds_cluster: bool,
}

/// One launch, clustered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterGraphReport {
    pub schema: String,
    pub mint: String,
    pub launch_ms: i64,
    pub migration_ms: Option<i64>,
    pub policy_version: u32,
    pub graph: GraphSummary,
    pub participants: u32,
    pub buy_volume_lamports: u64,
    /// Volume whose origin the traversal resolved. The rest is UNKNOWN and is
    /// counted rather than assumed independent.
    pub attributed_volume_lamports: u64,
    pub unattributed_volume_lamports: u64,
    /// Loudest first, then by root address.
    pub clusters: Vec<FundingCluster>,
    pub unclustered_wallets: u32,
    /// Groups that resolved to a common origin but were too small to be a
    /// pattern. Counted rather than dropped silently: "this launch had nine
    /// pairs" is a different sentence from "this launch had no clusters".
    pub clusters_below_floor: u32,
    /// §3.5 over the whole buyer population — the launch-level answer to "did
    /// one hand pay for this".
    pub launch_fund_micros: Option<u64>,
    pub dev: Option<DevTrace>,
    /// The loudest cluster judged as an accumulation, when there is one.
    pub insider: Option<InsiderFinding>,
    /// Any traversal behind any number here was budget-bound.
    pub truncated: bool,
    /// What the chain said about the edges this request asserted, when a
    /// witness was supplied with it.
    ///
    /// `None` means **nothing was checked**, which is not the same as nothing
    /// being wrong and is emphatically not a pass. A report with no proof is
    /// a rigorous derivation from whatever the message claimed; every number
    /// above inherits the truth of that claim and none of them can test it.
    /// The distinction is carried rather than flattened for the same reason
    /// UNKNOWN is never a zero anywhere else in this module.
    #[serde(default)]
    pub proof: Option<LineageProof>,
}

/// The row a list view shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterSummary {
    pub mint: String,
    pub participants: u32,
    pub clusters: u32,
    pub top_root: Option<String>,
    pub top_flow_share_bps: u16,
    pub insider_score_micros: Option<u64>,
    pub dev_origin: Option<String>,
    pub truncated: bool,
    /// Edges the chain contradicted. A row with a number here is a row whose
    /// request asserted transfers the chain does not have, which is a finding
    /// about the request before it is a finding about the launch.
    pub contradicted_edges: u32,
    /// A witness was supplied and every edge came back confirmed.
    pub chain_verified: bool,
}

impl ClusterGraphReport {
    pub fn summary(&self) -> ClusterSummary {
        let top = self.clusters.first();
        ClusterSummary {
            mint: self.mint.clone(),
            participants: self.participants,
            clusters: self.clusters.len() as u32,
            top_root: top.map(|cluster| cluster.root.clone()),
            top_flow_share_bps: top.map(|cluster| cluster.flow_share_bps).unwrap_or(0),
            insider_score_micros: self.insider.as_ref().map(|f| f.score_micros),
            dev_origin: self.dev.as_ref().and_then(|dev| dev.origin.clone()),
            truncated: self.truncated,
            contradicted_edges: self
                .proof
                .as_ref()
                .map(|proof| proof.contradicted)
                .unwrap_or(0),
            chain_verified: self.proof.as_ref().is_some_and(|proof| proof.complete),
        }
    }

    /// Whether this report may be used to *clear* a launch.
    ///
    /// Three things have to hold and the module makes all three explicit
    /// elsewhere; this is the one place they are read together. Nothing was
    /// budget-bound, so no number here is a lower bound standing in for a
    /// larger one. A witness was supplied, so the edges are a claim about the
    /// chain rather than a claim about a message. And every edge came back
    /// confirmed by the quorum.
    ///
    /// A false is not a finding. It says this report cannot be the reason
    /// something was allowed — it can still be the reason something was
    /// refused, and that asymmetry is the whole convention.
    pub fn may_clear(&self) -> bool {
        !self.truncated && self.proof.as_ref().is_some_and(|proof| proof.may_clear())
    }
}

// ===========================================================================
// The analysis
// ===========================================================================

/// Traces every participant, groups them by origin and scores the groups.
///
/// The whole entry point: everything else in this module is either an input to
/// it or a way of reading what it produced.
pub fn analyse(
    graph: &FundingGraph,
    participants: &[ClusterParticipant],
    context: &LaunchContext,
    policy: &TracePolicy,
    budget: &TraceBudget,
    params: &ClusteringParams,
) -> ClusterGraphReport {
    // One trace per distinct wallet, in address order. A wallet listed twice is
    // one buyer, and leaving both in would double its weight in every average.
    let mut by_wallet: BTreeMap<&str, &ClusterParticipant> = BTreeMap::new();
    for participant in participants {
        by_wallet
            .entry(participant.wallet.as_str())
            .or_insert(participant);
    }

    let traces: Vec<(WalletTrace, &ClusterParticipant)> = by_wallet
        .values()
        .map(|participant| {
            (
                trace_wallet(
                    graph,
                    &participant.wallet,
                    context.launch_ms,
                    policy,
                    budget,
                ),
                *participant,
            )
        })
        .collect();

    let total_volume: u128 = traces
        .iter()
        .map(|(_, p)| u128::from(p.buy_volume_lamports))
        .sum();
    let attributed_volume: u128 = traces
        .iter()
        .filter(|(trace, _)| trace.is_resolved())
        .map(|(_, p)| u128::from(p.buy_volume_lamports))
        .sum();
    let any_truncated = traces.iter().any(|(trace, _)| trace.truncated);

    // §3.5 over the whole buyer population, the launch-level discriminator.
    let launch_weighted: Vec<(&WalletTrace, u64)> = traces
        .iter()
        .map(|(trace, p)| (trace, p.buy_volume_lamports))
        .collect();
    let launch_fund = funding_concentration(&launch_weighted);

    // Assign each wallet to the origin its posterior actually points at.
    let mut groups: BTreeMap<&str, Vec<&(WalletTrace, &ClusterParticipant)>> = BTreeMap::new();
    let mut unclustered = 0u32;
    for entry in &traces {
        let (trace, _) = entry;
        match trace.parent.as_deref() {
            Some(parent) if trace.parent_posterior_micros >= params.min_parent_posterior_micros => {
                groups.entry(parent).or_default().push(entry);
            }
            // Either UNKNOWN, or two origins with comparable claims. Both are
            // "we do not know", and neither is "self-funded".
            _ => unclustered += 1,
        }
    }

    let mut clusters = Vec::new();
    let mut below_floor = 0u32;
    for (root, members) in groups {
        if members.len() < params.min_cluster_wallets {
            below_floor += 1;
            unclustered += members.len() as u32;
            continue;
        }
        clusters.push(build_cluster(
            graph,
            root,
            &members,
            &traces,
            total_volume,
            context,
            params,
        ));
    }

    // Loudest first, with a total order underneath so ties never float.
    clusters.sort_by(|a, b| {
        b.buy_volume_lamports
            .cmp(&a.buy_volume_lamports)
            .then_with(|| b.wallet_count.cmp(&a.wallet_count))
            .then_with(|| a.root.cmp(&b.root))
    });

    let dev = context
        .dev_wallet
        .as_deref()
        .map(|wallet| build_dev_trace(graph, wallet, &traces, &clusters, context, policy, budget));

    let insider = clusters
        .iter()
        .filter_map(|cluster| score_insider(cluster, dev.as_ref(), context, params))
        .max_by(|a, b| {
            a.score_micros
                .cmp(&b.score_micros)
                .then_with(|| b.root.cmp(&a.root))
        });

    ClusterGraphReport {
        schema: REPORT_SCHEMA.to_string(),
        mint: context.mint.clone(),
        launch_ms: context.launch_ms,
        migration_ms: context.migration_ms,
        policy_version: policy.version,
        graph: graph.summary(),
        participants: traces.len() as u32,
        buy_volume_lamports: total_volume.min(u128::from(u64::MAX)) as u64,
        attributed_volume_lamports: attributed_volume.min(u128::from(u64::MAX)) as u64,
        unattributed_volume_lamports: total_volume
            .saturating_sub(attributed_volume)
            .min(u128::from(u64::MAX)) as u64,
        clusters,
        unclustered_wallets: unclustered,
        clusters_below_floor: below_floor,
        launch_fund_micros: launch_fund.map(|c| c.fund_micros),
        dev,
        insider,
        truncated: any_truncated,
        // `analyse` is given a graph, and verification is about the edges the
        // graph was built from. `ClusterRequest::analyse` is where both are in
        // scope, so it is where the proof is attached.
        proof: None,
    }
}

/// Everything measured about one group of wallets behind one origin.
fn build_cluster(
    graph: &FundingGraph,
    root: &str,
    members: &[&(WalletTrace, &ClusterParticipant)],
    all: &[(WalletTrace, &ClusterParticipant)],
    total_volume: u128,
    context: &LaunchContext,
    params: &ClusteringParams,
) -> FundingCluster {
    let root_kind = graph.kind_of(root);

    let mut wallets: Vec<String> = members.iter().map(|(_, p)| p.wallet.clone()).collect();
    wallets.sort();

    let buy_volume: u128 = members
        .iter()
        .map(|(_, p)| u128::from(p.buy_volume_lamports))
        .sum();
    let sell_volume: u128 = members
        .iter()
        .map(|(_, p)| u128::from(p.sell_volume_lamports))
        .sum();
    let token_balance: u128 = members
        .iter()
        .map(|(_, p)| u128::from(p.token_balance))
        .sum();

    // Earliest first, then by address: the order the synchrony budget cuts at,
    // fixed before anything reads it.
    let mut times: Vec<i64> = members.iter().map(|(_, p)| p.first_buy_ms).collect();
    times.sort();

    let balances: Vec<u64> = members.iter().map(|(_, p)| p.token_balance).collect();
    let holding_hhi_bps = hhi_bps(&balances);
    let holding_entropy_micros = weighted_entropy_micros(&balances);

    // §14's shape, read both ways. Both have to fire: the index is a sum of
    // squares and feels almost only the largest holder, the entropy is a sum of
    // logs and counts the tail, and splitting the small end moves one and not
    // the other.
    let costume_ring = matches!(holding_hhi_bps, Some(hhi) if hhi >= RING_HHI_BPS)
        && matches!(holding_entropy_micros, Some(e) if e <= RING_ENTROPY_MICROS);

    // What each member actually received down its strongest path to this root.
    let mut member_funding: Vec<u64> = members
        .iter()
        .filter_map(|(trace, _)| {
            trace
                .roots
                .iter()
                .find(|influence| influence.root == root)
                .map(|influence| influence.bottleneck_lamports)
        })
        .collect();
    member_funding.sort_unstable_by(|a, b| b.cmp(a));

    let funding_ring = detect_split_ring(&member_funding, members.len(), params);

    // The kernel caps how many wallets it will pair up; past that the sum is
    // partial, which is a bound like any other and travels with the row.
    let (sync, sync_truncated) = sync_micros(&times, params.tau_sync_ms).unzip();

    let weighted: Vec<(&WalletTrace, u64)> = members
        .iter()
        .map(|(trace, p)| (trace, p.buy_volume_lamports))
        .collect();
    let fund_micros = funding_concentration(&weighted).map(|c| c.fund_micros);

    let launch_share_micros = root_share_micros(all, root);

    // §3.5: no influence without both halves, and no zero standing in for a
    // missing one.
    let temporal_influence_micros = match (sync, fund_micros) {
        (Some(sync), Some(fund)) => Some(temporal_influence_micros(sync, fund)),
        _ => None,
    };

    let (pre_migration_buy_lamports, pre_migration_wallets) = match context.migration_ms {
        Some(migration_ms) => {
            let volume: u128 = members
                .iter()
                .filter(|(_, p)| p.first_buy_ms <= migration_ms)
                .map(|(_, p)| u128::from(p.buy_volume_lamports))
                .sum();
            let count = members
                .iter()
                .filter(|(_, p)| p.first_buy_ms <= migration_ms)
                .count() as u32;
            (volume.min(u128::from(u64::MAX)) as u64, count)
        }
        // No migration observed, so nothing has been shown to precede one. The
        // whole is reported as pre-migration because that is what it is — a
        // curve that has not migrated has only pre-migration buying — and the
        // reason that reads this deliberately does not fire without a timestamp.
        None => (
            buy_volume.min(u128::from(u64::MAX)) as u64,
            members.len() as u32,
        ),
    };

    let ownership_bps = if context.circulating_supply == 0 {
        None
    } else {
        Some(
            mul_div_round(
                token_balance,
                u128::from(BPS_DENOMINATOR),
                u128::from(context.circulating_supply),
            )
            .min(u128::from(BPS_DENOMINATOR)) as u16,
        )
    };

    FundingCluster {
        root: root.to_string(),
        root_kind,
        shared_hub: root_kind.is_absorbing(),
        wallet_count: wallets.len() as u32,
        wallets,
        buy_volume_lamports: buy_volume.min(u128::from(u64::MAX)) as u64,
        sell_volume_lamports: sell_volume.min(u128::from(u64::MAX)) as u64,
        flow_share_bps: mul_div_floor(buy_volume, u128::from(BPS_DENOMINATOR), total_volume)
            .min(u128::from(BPS_DENOMINATOR)) as u16,
        token_balance: token_balance.min(u128::from(u64::MAX)) as u64,
        ownership_bps,
        holding_hhi_bps,
        holding_entropy_micros,
        costume_ring,
        funding_ring,
        sync_micros: sync,
        fund_micros,
        launch_share_micros,
        temporal_influence_micros,
        first_buy_ms: times.first().copied().unwrap_or(0),
        first_buy_span_ms: times
            .last()
            .copied()
            .unwrap_or(0)
            .saturating_sub(times.first().copied().unwrap_or(0)),
        pre_migration_buy_lamports,
        pre_migration_wallets,
        member_funding_lamports: member_funding,
        truncated: sync_truncated.unwrap_or(false)
            || members.iter().any(|(trace, _)| trace.truncated),
    }
}

/// One origin's volume-weighted share of the whole launch, in millionths.
///
/// §3.5's `fund(C)` with `C` taken as every buyer rather than as one cluster,
/// and pinned to a named root rather than maximised over roots. That is the
/// number that says how much of a launch one hand paid for.
fn root_share_micros(all: &[(WalletTrace, &ClusterParticipant)], root: &str) -> Option<u64> {
    let total: u128 = all
        .iter()
        .map(|(_, p)| u128::from(p.buy_volume_lamports))
        .sum();
    if total == 0 {
        return None;
    }

    let weighted: u128 = all
        .iter()
        .map(|(trace, p)| {
            trace
                .roots
                .iter()
                .find(|influence| influence.root == root)
                .map(|influence| {
                    u128::from(p.buy_volume_lamports) * u128::from(influence.posterior_micros)
                })
                .unwrap_or(0)
        })
        .sum();

    let share = (weighted / total).min(u128::from(MICROS)) as u64;
    // Zero here would mean this root reaches nobody, which cannot be true of a
    // root that named a cluster — but the UNKNOWN convention holds either way.
    (share > 0).then_some(share)
}

/// The split-wallet ring test over one cluster's funding amounts.
///
/// The median rather than the mean, and a tolerance around it rather than a
/// variance: an operator who funds nine wallets with 0.5 SOL and one with 40 is
/// still running a script, and a mean would let the outlier hide it.
fn detect_split_ring(
    funding: &[u64],
    cluster_wallets: usize,
    params: &ClusteringParams,
) -> Option<SplitRing> {
    let amounts: Vec<u64> = funding.iter().copied().filter(|&a| a > 0).collect();
    if amounts.len() < params.min_cluster_wallets {
        return None;
    }

    // `funding` arrives sorted descending, so the lower median is at the middle
    // index counting from either end. Taken by index rather than averaged so the
    // value is always one that actually occurred.
    let median = amounts[amounts.len() / 2];
    if median == 0 {
        return None;
    }

    let tolerance = mul_div_ceil_u64(median, params.ring_tolerance_bps);
    let matched = amounts
        .iter()
        .filter(|&&amount| amount.abs_diff(median) <= tolerance)
        .count() as u32;

    let share_bps = mul_div_floor(
        u128::from(matched),
        u128::from(BPS_DENOMINATOR),
        cluster_wallets as u128,
    )
    .min(u128::from(BPS_DENOMINATOR)) as u16;

    if share_bps < params.ring_share_bps || (matched as usize) < params.min_cluster_wallets {
        return None;
    }

    // Near one is the tell: every wallet got the same amount.
    let uniformity_micros = weighted_entropy_micros(&amounts)?;

    Some(SplitRing {
        matched_wallets: matched,
        cluster_wallets: cluster_wallets as u32,
        share_bps,
        median_funding_lamports: median,
        uniformity_micros,
    })
}

fn mul_div_ceil_u64(value: u64, bps: u64) -> u64 {
    (u128::from(value) * u128::from(bps)).div_ceil(u128::from(BPS_DENOMINATOR)) as u64
}

/// Where the dev wallet's money came from, and who else it reached.
fn build_dev_trace(
    graph: &FundingGraph,
    wallet: &str,
    traces: &[(WalletTrace, &ClusterParticipant)],
    clusters: &[FundingCluster],
    context: &LaunchContext,
    policy: &TracePolicy,
    budget: &TraceBudget,
) -> DevTrace {
    // The dev is often not an opening buyer, so it may not be in `traces` — its
    // trace is taken directly rather than looked up.
    let trace = traces
        .iter()
        .find(|(trace, _)| trace.wallet == wallet)
        .map(|(trace, _)| trace.clone())
        .unwrap_or_else(|| trace_wallet(graph, wallet, context.launch_ms, policy, budget));

    let best = trace.roots.first();
    let origin = best.map(|root| root.root.clone());
    let origin_kind = best.map(|root| root.kind);
    let hops = best.map(|root| root.hops).unwrap_or(0);
    let exit_node = best
        .filter(|root| root.kind.is_absorbing())
        .map(|root| root.root.clone());

    // Opening buyers that came out of the same origin. Not "wallets the dev
    // funded" — the dev usually funds nobody directly, and the operator that
    // funded the dev is the one that funded the rest.
    let (siblings, sibling_buy_lamports) = match origin.as_deref() {
        Some(origin) => {
            let mut siblings: Vec<String> = Vec::new();
            let mut volume: u128 = 0;
            for (other, participant) in traces {
                if other.wallet == wallet {
                    continue;
                }
                if other.parent.as_deref() == Some(origin) {
                    siblings.push(other.wallet.clone());
                    volume += u128::from(participant.buy_volume_lamports);
                }
            }
            siblings.sort();
            (siblings, volume.min(u128::from(u64::MAX)) as u64)
        }
        None => (Vec::new(), 0),
    };

    // Opening buyers the dev paid for itself. Independent of `origin`: this is
    // the case where the dev *is* the root, which is exactly when the sibling
    // pass above has nothing to say.
    let mut funded_buyers: Vec<String> = Vec::new();
    let mut funded_volume: u128 = 0;
    for (other, participant) in traces {
        if other.wallet == wallet {
            continue;
        }
        if other.parent.as_deref() == Some(wallet) {
            funded_buyers.push(other.wallet.clone());
            funded_volume += u128::from(participant.buy_volume_lamports);
        }
    }
    funded_buyers.sort();

    let cluster_root = origin.as_deref().and_then(|origin| {
        clusters
            .iter()
            .find(|cluster| cluster.root == origin)
            .map(|cluster| cluster.root.clone())
    });
    let funds_cluster = clusters.iter().any(|cluster| cluster.root == wallet);

    DevTrace {
        wallet: wallet.to_string(),
        trace,
        origin,
        origin_kind,
        hops,
        exit_node,
        siblings,
        sibling_buy_lamports,
        funded_buyers,
        funded_buy_lamports: funded_volume.min(u128::from(u64::MAX)) as u64,
        cluster_root,
        funds_cluster,
    }
}

/// Scores one cluster as an accumulation, or declines to.
///
/// Declines when the root is absorbing — sharing an exit node is not evidence
/// of common ownership — or when either primary component is UNKNOWN. §3.5 is
/// explicit that a shared funder and a synchronised open are both necessary and
/// that neither implies the other, so a score resting on only one of them would
/// be claiming something the evidence has not shown.
fn score_insider(
    cluster: &FundingCluster,
    dev: Option<&DevTrace>,
    context: &LaunchContext,
    params: &ClusteringParams,
) -> Option<InsiderFinding> {
    if cluster.shared_hub {
        return None;
    }
    let sync = cluster.sync_micros?;
    let launch_share = cluster.launch_share_micros?;

    let ownership_micros = cluster.ownership_bps.map(|bps| {
        // Scaled against the ceiling rather than against 100%: a cluster holding
        // a fifth of the supply is already as concentrated as this reads.
        mul_div_floor(
            u128::from(bps),
            u128::from(MICROS),
            u128::from(params.ownership_full_bps.max(1)),
        )
        .min(u128::from(MICROS)) as u64
    });
    let uniformity_micros = cluster
        .funding_ring
        .as_ref()
        .map(|ring| ring.uniformity_micros);

    let components = InsiderComponents {
        sync_micros: Some(sync),
        launch_share_micros: Some(launch_share),
        ownership_micros,
        uniformity_micros,
    };

    // A component that could not be measured is left out of the mean and the
    // weights renormalise over what survived. A missing test is not a passed
    // test, and scoring it zero would be treating it as a passed one.
    let terms: [(Option<u64>, u64); 4] = [
        (Some(sync), params.weight_sync_bps),
        (Some(launch_share), params.weight_fund_bps),
        (ownership_micros, params.weight_ownership_bps),
        (uniformity_micros, params.weight_uniformity_bps),
    ];
    let measured_weight_bps: u64 = terms
        .iter()
        .filter_map(|(value, weight)| value.map(|_| *weight))
        .sum();
    if measured_weight_bps == 0 {
        return None;
    }
    let weighted: u128 = terms
        .iter()
        .filter_map(|(value, weight)| value.map(|v| u128::from(v) * u128::from(*weight)))
        .sum();
    let score_micros = (weighted / u128::from(measured_weight_bps)).min(u128::from(MICROS)) as u64;

    let pre_migration_share_bps = mul_div_floor(
        u128::from(cluster.pre_migration_buy_lamports),
        u128::from(BPS_DENOMINATOR),
        u128::from(cluster.buy_volume_lamports),
    )
    .min(u128::from(BPS_DENOMINATOR)) as u16;

    let mut reasons = Vec::new();
    if launch_share >= MICROS / 2 {
        reasons.push(InsiderReason::SharedFunder);
    }
    if sync >= MICROS / 2 {
        reasons.push(InsiderReason::SynchronisedOpen);
    }
    if matches!(ownership_micros, Some(value) if value >= MICROS / 2) {
        reasons.push(InsiderReason::ConcentratedOwnership);
    }
    if cluster.funding_ring.is_some() {
        reasons.push(InsiderReason::UniformFunding);
    }
    // Only a launch that actually migrated can have accumulation preceding it.
    if context.migration_ms.is_some()
        && pre_migration_share_bps >= params.min_pre_migration_share_bps
    {
        reasons.push(InsiderReason::PreMigrationAccumulation);
    }
    if cluster.costume_ring {
        reasons.push(InsiderReason::CostumeRing);
    }
    if dev
        .and_then(|dev| dev.origin.as_deref())
        .is_some_and(|origin| origin == cluster.root)
    {
        reasons.push(InsiderReason::DevSharesOrigin);
    }
    if dev.is_some_and(|dev| dev.wallet == cluster.root) {
        reasons.push(InsiderReason::DevFundedCluster);
    }
    reasons.sort();
    reasons.dedup();

    Some(InsiderFinding {
        root: cluster.root.clone(),
        wallets: cluster.wallet_count,
        score_micros,
        measured_weight_bps,
        components,
        reasons,
        pre_migration_share_bps,
        pre_migration_buy_lamports: cluster.pre_migration_buy_lamports,
        truncated: cluster.truncated,
    })
}

// ===========================================================================
// What crosses the IPC boundary
// ===========================================================================

/// Everything one clustering run needs, in one message.
///
/// The graph arrives with the request rather than being held in state, and that
/// is deliberate: the analysis is a pure function of its inputs, so a report can
/// be reproduced exactly by replaying the message that produced it. A registry
/// that owned the graph would make the same report depend on when it was asked
/// for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterRequest {
    pub context: LaunchContext,
    pub participants: Vec<ClusterParticipant>,
    pub edges: Vec<TraceEdge>,
    #[serde(default)]
    pub labels: Vec<NodeLabel>,
    /// What providers say about the signatures on those edges.
    ///
    /// Empty means the caller supplied no evidence, and the analysis then runs
    /// on the edges as asserted and reports `proof: None`. That is a real mode
    /// — a replay of a fixture that predates verification, an operator reading
    /// a graph they assembled by hand — and it is deliberately not the same as
    /// a witness that answered "unavailable" for everything, which produces a
    /// proof full of UNKNOWN and discounts every edge. "Nobody was asked" and
    /// "everybody was asked and nobody knew" are different states of the world.
    #[serde(default)]
    pub witness: Vec<Attestation>,
    #[serde(default)]
    pub verification: Option<VerificationPolicy>,
    #[serde(default)]
    pub policy: Option<TracePolicy>,
    #[serde(default)]
    pub budget: Option<TraceBudget>,
    #[serde(default)]
    pub params: Option<ClusteringParams>,
}

impl ClusterRequest {
    /// Checks the request describes a launch, then runs the analysis.
    ///
    /// Every refusal here is about the *request*, never about the evidence. A
    /// launch nobody bought, a graph with no edges and a wallet nothing funded
    /// are all answerable — the answer is UNKNOWN and it is inside the report.
    /// What cannot be answered is a message that does not name a mint, or one
    /// whose budgets would make the traversal do nothing at all.
    pub fn analyse(self) -> Result<ClusterGraphReport, EngineError> {
        if self.context.mint.trim().is_empty() {
            return Err(EngineError::Forensics(
                "a clustering request has to name a mint".to_string(),
            ));
        }
        if self.participants.is_empty() {
            return Err(EngineError::Forensics(format!(
                "{}: there are no opening buyers to cluster",
                self.context.mint
            )));
        }
        if let Some(migration_ms) = self.context.migration_ms {
            if migration_ms < self.context.launch_ms {
                return Err(EngineError::Forensics(format!(
                    "{}: the curve cannot have migrated before it launched",
                    self.context.mint
                )));
            }
        }

        let policy = self.policy.unwrap_or_default();
        if policy.half_life_ms <= 0 {
            return Err(EngineError::Forensics(
                "a half-life of zero would make every funding edge worthless".to_string(),
            ));
        }
        if policy.theta_lamports == 0 {
            return Err(EngineError::Forensics(
                "a flow threshold of zero would make every path count fully".to_string(),
            ));
        }

        let budget = self.budget.unwrap_or_default();
        if budget.depth == 0 || budget.fanout == 0 || budget.nodes == 0 || budget.edges == 0 {
            return Err(EngineError::Forensics(
                "a budget of zero would trace nothing and report it as UNKNOWN".to_string(),
            ));
        }

        let params = self.params.unwrap_or_default();
        if params.min_cluster_wallets == 0 {
            return Err(EngineError::Forensics(
                "a cluster needs at least one wallet in it".to_string(),
            ));
        }
        if params.weight_sync_bps
            + params.weight_fund_bps
            + params.weight_ownership_bps
            + params.weight_uniformity_bps
            == 0
        {
            return Err(EngineError::Forensics(
                "every component weight is zero, so no score could be taken".to_string(),
            ));
        }

        let labels: Vec<(String, NodeKind)> = self
            .labels
            .into_iter()
            .map(|label| (label.address, label.kind))
            .collect();

        // The chain gets asked before the graph is built, so a contradicted
        // edge never becomes a vertex. Dropping it afterwards would leave the
        // node counts, the router degree test and the fan-out cuts all resting
        // on transfers the chain says did not happen.
        let (edges, proof) = if self.witness.is_empty() {
            (self.edges, None)
        } else {
            let policy = self.verification.unwrap_or_default();
            let verified = verify_request(&self.edges, &self.witness, &policy)?;
            (verified.edges, Some(verified.proof))
        };

        let graph = FundingGraph::build(edges, &labels, &policy);

        let mut report = analyse(
            &graph,
            &self.participants,
            &self.context,
            &policy,
            &budget,
            &params,
        );
        report.proof = proof;
        Ok(report)
    }
}

/// One wallet's funding trail, on its own.
///
/// The cheap query behind live trail tracking: no launch, no participants, no
/// clustering — just "where did this address's money come from".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceRequest {
    pub wallet: String,
    /// What the decay and the lookback are measured from. The launch time when
    /// there is one, otherwise now.
    pub reference_ms: i64,
    pub edges: Vec<TraceEdge>,
    #[serde(default)]
    pub labels: Vec<NodeLabel>,
    /// What providers say about the signatures on those edges. See
    /// [`ClusterRequest::witness`].
    #[serde(default)]
    pub witness: Vec<Attestation>,
    #[serde(default)]
    pub verification: Option<VerificationPolicy>,
    #[serde(default)]
    pub policy: Option<TracePolicy>,
    #[serde(default)]
    pub budget: Option<TraceBudget>,
}

/// One trail, and what the chain said about the edges under it.
///
/// A pair rather than a `proof` field on [`WalletTrace`] itself, because a
/// cluster report holds one trace per participant and they all rest on the same
/// edges: hanging the proof off each of them would carry one answer a hundred
/// times and invite a reader to think the hundred were independent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletTraceReport {
    pub trace: WalletTrace,
    /// `None` when no witness was supplied — see [`ClusterRequest::witness`].
    pub proof: Option<LineageProof>,
}

impl WalletTraceReport {
    /// Whether this trail may be used to *clear* the wallet: nothing
    /// truncated, and every edge under it confirmed by the quorum.
    pub fn may_clear(&self) -> bool {
        self.trace.may_clear() && self.proof.as_ref().is_some_and(|proof| proof.may_clear())
    }
}

impl TraceRequest {
    pub fn run(self) -> Result<WalletTraceReport, EngineError> {
        if self.wallet.trim().is_empty() {
            return Err(EngineError::Forensics(
                "a funding trace has to name a wallet".to_string(),
            ));
        }

        let policy = self.policy.unwrap_or_default();
        if policy.half_life_ms <= 0 || policy.theta_lamports == 0 {
            return Err(EngineError::Forensics(
                "a half-life or flow threshold of zero would make the trace meaningless"
                    .to_string(),
            ));
        }

        let budget = self.budget.unwrap_or_default();
        if budget.depth == 0 || budget.fanout == 0 || budget.nodes == 0 || budget.edges == 0 {
            return Err(EngineError::Forensics(
                "a budget of zero would trace nothing and report it as UNKNOWN".to_string(),
            ));
        }

        let labels: Vec<(String, NodeKind)> = self
            .labels
            .into_iter()
            .map(|label| (label.address, label.kind))
            .collect();

        let (edges, proof) = if self.witness.is_empty() {
            (self.edges, None)
        } else {
            let verification = self.verification.unwrap_or_default();
            let verified = verify_request(&self.edges, &self.witness, &verification)?;
            (verified.edges, Some(verified.proof))
        };

        let graph = FundingGraph::build(edges, &labels, &policy);

        Ok(WalletTraceReport {
            trace: trace_wallet(&graph, &self.wallet, self.reference_ms, &policy, &budget),
            proof,
        })
    }
}

// ===========================================================================
// The registry
// ===========================================================================

/// The reports the window can ask for, and the fan-out that announces them.
///
/// Analysis is a pure function; this is the only stateful thing in the module.
/// It is bounded: past [`MAX_REPORTS`] the least recently recorded mint is
/// dropped, because a process that watches launches all day would otherwise
/// keep every one of them forever.
pub struct ClusterRegistry {
    reports: RwLock<BTreeMap<String, (u64, ClusterGraphReport)>>,
    /// Recording order, so eviction does not need a clock — which is what keeps
    /// a stored report byte-identical to the one the analysis produced.
    sequence: AtomicU64,
    telemetry: Option<Arc<TelemetryHub>>,
    alert_score_micros: u64,
}

impl ClusterRegistry {
    pub fn new() -> ClusterRegistry {
        ClusterRegistry {
            reports: RwLock::new(BTreeMap::new()),
            sequence: AtomicU64::new(0),
            telemetry: None,
            alert_score_micros: ClusteringParams::default().alert_score_micros,
        }
    }

    /// The same registry, announcing what it records.
    pub fn with_telemetry(
        telemetry: Arc<TelemetryHub>,
        alert_score_micros: u64,
    ) -> ClusterRegistry {
        ClusterRegistry {
            telemetry: Some(telemetry),
            alert_score_micros,
            ..ClusterRegistry::new()
        }
    }

    /// Stores one report, replacing any earlier one for the same mint, and
    /// publishes a line about it.
    pub fn record(&self, report: ClusterGraphReport) {
        let summary = report.summary();
        let mint = report.mint.clone();
        // Cloned before the report is moved into the map. Only when there is a
        // telemetry hub and something to say: a proof is the largest thing in a
        // report and copying it per recording to publish nothing would be a
        // cost paid on the common path for the rare one.
        let report_proof = match (&self.telemetry, &report.proof) {
            (Some(_), Some(proof)) if proof.contradicted > 0 => Some(proof.clone()),
            _ => None,
        };
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);

        {
            let mut reports = self.reports.write();
            reports.insert(mint.clone(), (sequence, report));
            while reports.len() > MAX_REPORTS {
                // The oldest recording, by the counter rather than by address —
                // a BTreeMap's first key is alphabetical, which would evict by
                // mint name.
                let Some(oldest) = reports
                    .iter()
                    .min_by_key(|(_, (sequence, _))| *sequence)
                    .map(|(mint, _)| mint.clone())
                else {
                    break;
                };
                reports.remove(&oldest);
            }
        }

        let Some(telemetry) = &self.telemetry else {
            return;
        };

        // A request that asserted transfers the chain does not have is its own
        // event, published before the finding and separately from it. The
        // roadmap's rule is that conflicting payloads produce a contradiction
        // event and no overwrite, and a contradiction folded into the clustering
        // line would be exactly the overwrite: an operator reading "3 clusters,
        // loudest X" has been told about the launch and not about the fact that
        // the evidence under it did not survive being checked.
        if let Some(proof) = report_proof.as_ref() {
            if proof.contradicted > 0 {
                let named: Vec<&str> = proof
                    .contradictions()
                    .take(8)
                    .map(|edge| edge.signature.as_str())
                    .collect();
                telemetry.publish(
                    TelemetryLevel::Warn,
                    "contradiction",
                    format!(
                        "{mint}: the chain contradicts {} of {} asserted funding edges ({})",
                        proof.contradicted,
                        proof.claimed,
                        named.join(", ")
                    ),
                    serde_json::to_value(proof).unwrap_or(serde_json::Value::Null),
                );
            }
        }

        // A finding at or above the alert threshold is the whole point of the
        // module, so it goes out loud. Everything else is a line in the log.
        let loud = summary
            .insider_score_micros
            .is_some_and(|score| score >= self.alert_score_micros);
        let level = if loud {
            TelemetryLevel::Warn
        } else {
            TelemetryLevel::Info
        };
        let message = match (&summary.top_root, summary.insider_score_micros) {
            (Some(root), Some(score)) => format!(
                "{mint}: {} clusters, loudest {root} at {score} millionths",
                summary.clusters
            ),
            (Some(root), None) => format!(
                "{mint}: {} clusters, loudest {root}, not scoreable",
                summary.clusters
            ),
            _ => format!("{mint}: no cluster resolved"),
        };

        telemetry.publish(
            level,
            "clustering",
            message,
            serde_json::to_value(&summary).unwrap_or(serde_json::Value::Null),
        );
    }

    pub fn report(&self, mint: &str) -> Option<ClusterGraphReport> {
        self.reports
            .read()
            .get(mint)
            .map(|(_, report)| report.clone())
    }

    /// Every stored report as a row, most recently recorded first.
    pub fn summaries(&self) -> Vec<ClusterSummary> {
        let reports = self.reports.read();
        let mut rows: Vec<(u64, ClusterSummary)> = reports
            .values()
            .map(|(sequence, report)| (*sequence, report.summary()))
            .collect();
        rows.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.mint.cmp(&b.1.mint)));
        rows.into_iter().map(|(_, summary)| summary).collect()
    }

    pub fn len(&self) -> usize {
        self.reports.read().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&self) {
        self.reports.write().clear();
    }
}

impl Default for ClusterRegistry {
    fn default() -> Self {
        ClusterRegistry::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracer::Asset;

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

    fn context() -> LaunchContext {
        LaunchContext {
            mint: "MintOfTheLaunch".to_string(),
            launch_ms: LAUNCH,
            migration_ms: None,
            circulating_supply: SUPPLY,
            dev_wallet: None,
        }
    }

    fn run(
        edges: Vec<TraceEdge>,
        labels: &[(&str, NodeKind)],
        participants: &[ClusterParticipant],
        context: &LaunchContext,
    ) -> ClusterGraphReport {
        let owned: Vec<(String, NodeKind)> = labels
            .iter()
            .map(|(address, kind)| ((*address).to_string(), *kind))
            .collect();
        let policy = TracePolicy::default();
        let graph = FundingGraph::build(edges, &owned, &policy);
        analyse(
            &graph,
            participants,
            context,
            &policy,
            &TraceBudget::default(),
            &ClusteringParams::default(),
        )
    }

    /// A four-wallet ring: one operator funds four fresh keypairs with the same
    /// amount, and all four open within the synchrony kernel's bandwidth.
    fn sybil_ring() -> (Vec<TraceEdge>, Vec<ClusterParticipant>) {
        let mut edges = Vec::new();
        let mut buyers = Vec::new();
        for index in 0..4i64 {
            let wallet = format!("puppet{index}");
            edges.push(edge(
                "operator",
                &wallet,
                2 * SOL,
                LAUNCH - 2 * HOUR,
                &format!("f{index}"),
            ));
            buyers.push(buyer(&wallet, LAUNCH + index * 500, SOL, SUPPLY / 40));
        }
        (edges, buyers)
    }

    // -----------------------------------------------------------------
    // Clustering
    // -----------------------------------------------------------------

    #[test]
    fn wallets_behind_one_operator_become_one_cluster() {
        let (edges, buyers) = sybil_ring();
        let report = run(edges, &[], &buyers, &context());

        assert_eq!(report.clusters.len(), 1);
        let cluster = &report.clusters[0];
        assert_eq!(cluster.root, "operator");
        assert_eq!(cluster.wallet_count, 4);
        assert_eq!(
            cluster.wallets,
            vec!["puppet0", "puppet1", "puppet2", "puppet3"]
        );
        assert_eq!(cluster.buy_volume_lamports, 4 * SOL);
        assert_eq!(cluster.flow_share_bps, 10_000);
        assert!(!cluster.shared_hub);
        assert_eq!(report.unclustered_wallets, 0);
        assert_eq!(report.schema, REPORT_SCHEMA);
    }

    #[test]
    fn an_unresolved_wallet_is_counted_and_never_recruited_by_timing() {
        // `stranger` bought in the same instant as the ring and nothing funded
        // it in the record. Buying together is not evidence of common
        // ownership — that is a bot service with many customers.
        let (edges, mut buyers) = sybil_ring();
        buyers.push(buyer("stranger", LAUNCH + 100, SOL, SUPPLY / 40));
        let report = run(edges, &[], &buyers, &context());

        assert_eq!(report.clusters.len(), 1);
        assert_eq!(report.clusters[0].wallet_count, 4);
        assert!(!report.clusters[0].wallets.contains(&"stranger".to_string()));
        assert_eq!(report.unclustered_wallets, 1);
        // And its volume is in the denominator rather than assumed independent.
        assert_eq!(report.participants, 5);
        assert_eq!(report.unattributed_volume_lamports, SOL);
        assert_eq!(report.attributed_volume_lamports, 4 * SOL);
        assert_eq!(report.clusters[0].flow_share_bps, 8_000);
    }

    #[test]
    fn a_group_too_small_to_be_a_pattern_is_counted_not_reported() {
        let edges = vec![
            edge("operator", "w1", 2 * SOL, LAUNCH - HOUR, "f1"),
            edge("operator", "w2", 2 * SOL, LAUNCH - HOUR, "f2"),
        ];
        let buyers = vec![
            buyer("w1", LAUNCH, SOL, SUPPLY / 40),
            buyer("w2", LAUNCH, SOL, SUPPLY / 40),
        ];
        let report = run(edges, &[], &buyers, &context());

        assert!(report.clusters.is_empty());
        assert_eq!(report.clusters_below_floor, 1);
        // "This launch had one pair" is a different sentence from "no clusters".
        assert_eq!(report.unclustered_wallets, 2);
    }

    #[test]
    fn two_operators_are_two_clusters_loudest_first() {
        let mut edges = Vec::new();
        let mut buyers = Vec::new();
        for index in 0..3 {
            edges.push(edge(
                "small-op",
                &format!("s{index}"),
                SOL,
                LAUNCH - HOUR,
                &format!("a{index}"),
            ));
            buyers.push(buyer(&format!("s{index}"), LAUNCH, SOL, SUPPLY / 100));
        }
        for index in 0..3 {
            edges.push(edge(
                "big-op",
                &format!("b{index}"),
                20 * SOL,
                LAUNCH - HOUR,
                &format!("c{index}"),
            ));
            buyers.push(buyer(&format!("b{index}"), LAUNCH, 10 * SOL, SUPPLY / 20));
        }
        let report = run(edges, &[], &buyers, &context());

        assert_eq!(report.clusters.len(), 2);
        assert_eq!(report.clusters[0].root, "big-op");
        assert_eq!(report.clusters[1].root, "small-op");
        assert!(report.clusters[0].flow_share_bps > report.clusters[1].flow_share_bps);
    }

    // -----------------------------------------------------------------
    // The router false positive
    // -----------------------------------------------------------------

    #[test]
    fn a_cluster_behind_a_high_volume_router_is_flagged_and_never_scored() {
        // Thirty addresses come out of one contract, six of them buy this
        // launch together. That is a popular exit node, not a syndicate.
        let mut edges = Vec::new();
        let mut buyers = Vec::new();
        for index in 0..30 {
            edges.push(edge(
                "router",
                &format!("customer{index:02}"),
                2 * SOL,
                LAUNCH - 2 * HOUR,
                &format!("r{index:02}"),
            ));
        }
        for index in 0..6 {
            buyers.push(buyer(
                &format!("customer{index:02}"),
                LAUNCH + index as i64 * 200,
                SOL,
                SUPPLY / 30,
            ));
        }
        let report = run(edges, &[], &buyers, &context());

        assert_eq!(report.clusters.len(), 1);
        let cluster = &report.clusters[0];
        assert_eq!(cluster.root, "router");
        assert_eq!(cluster.root_kind, NodeKind::Router);
        assert!(cluster.shared_hub, "an inferred router is a shared hub");
        assert!(
            report.insider.is_none(),
            "sharing an exit node is not evidence of common ownership"
        );
        assert_eq!(report.graph.inferred_routers, vec!["router".to_string()]);
    }

    #[test]
    fn a_cluster_behind_a_labelled_exchange_is_never_scored_either() {
        let mut edges = Vec::new();
        let mut buyers = Vec::new();
        for index in 0..5 {
            edges.push(edge(
                "cex",
                &format!("customer{index}"),
                2 * SOL,
                LAUNCH - 2 * HOUR,
                &format!("x{index}"),
            ));
            buyers.push(buyer(&format!("customer{index}"), LAUNCH, SOL, SUPPLY / 25));
        }
        let report = run(edges, &[("cex", NodeKind::Exchange)], &buyers, &context());

        assert_eq!(report.clusters[0].root_kind, NodeKind::Exchange);
        assert!(report.clusters[0].shared_hub);
        assert!(report.insider.is_none());
    }

    #[test]
    fn the_same_shape_behind_a_person_is_scored() {
        // The control for the two tests above: identical topology, a funder
        // under the degree threshold, and now there is a finding.
        let (edges, buyers) = sybil_ring();
        let report = run(edges, &[], &buyers, &context());
        let insider = report.insider.expect("a real operator is scoreable");
        assert_eq!(insider.root, "operator");
        assert!(insider.reasons.contains(&InsiderReason::SharedFunder));
        assert!(insider.reasons.contains(&InsiderReason::SynchronisedOpen));
    }

    // -----------------------------------------------------------------
    // Rings
    // -----------------------------------------------------------------

    #[test]
    fn identical_funding_amounts_are_a_split_wallet_ring() {
        let (edges, buyers) = sybil_ring();
        let report = run(edges, &[], &buyers, &context());
        let ring = report.clusters[0]
            .funding_ring
            .as_ref()
            .expect("four identical transfers is a script");

        assert_eq!(ring.matched_wallets, 4);
        assert_eq!(ring.cluster_wallets, 4);
        assert_eq!(ring.share_bps, 10_000);
        assert_eq!(ring.median_funding_lamports, 2 * SOL);
        // Equal amounts are maximum entropy: near one, the opposite direction
        // from the holdings-side ring test.
        assert_eq!(ring.uniformity_micros, MICROS);
        assert!(report
            .insider
            .as_ref()
            .expect("scoreable")
            .reasons
            .contains(&InsiderReason::UniformFunding));
    }

    #[test]
    fn funding_that_varies_like_people_do_is_not_a_ring() {
        let amounts = [SOL / 2, 3 * SOL, 17 * SOL, 40 * SOL];
        let mut edges = Vec::new();
        let mut buyers = Vec::new();
        for (index, amount) in amounts.iter().enumerate() {
            let wallet = format!("member{index}");
            edges.push(edge(
                "operator",
                &wallet,
                *amount,
                LAUNCH - HOUR,
                &format!("f{index}"),
            ));
            buyers.push(buyer(
                &wallet,
                LAUNCH + index as i64 * 4_000,
                SOL,
                SUPPLY / 40,
            ));
        }
        let report = run(edges, &[], &buyers, &context());
        assert!(report.clusters[0].funding_ring.is_none());
    }

    #[test]
    fn the_tolerance_lets_near_identical_amounts_still_count() {
        // A script that jitters each transfer by under a percent is still a
        // script; the tolerance is two percent.
        let amounts = [2 * SOL, 2 * SOL + SOL / 200, 2 * SOL - SOL / 200, 2 * SOL];
        let mut edges = Vec::new();
        let mut buyers = Vec::new();
        for (index, amount) in amounts.iter().enumerate() {
            let wallet = format!("member{index}");
            edges.push(edge(
                "operator",
                &wallet,
                *amount,
                LAUNCH - HOUR,
                &format!("f{index}"),
            ));
            buyers.push(buyer(&wallet, LAUNCH, SOL, SUPPLY / 40));
        }
        let report = run(edges, &[], &buyers, &context());
        let ring = report.clusters[0]
            .funding_ring
            .as_ref()
            .expect("a jittered script");
        assert_eq!(ring.matched_wallets, 4);
    }

    #[test]
    fn one_wallet_and_some_costumes_is_a_holdings_ring() {
        // §14's `[0.9, 0.1]` population: an index of 8 200 and an entropy of
        // 0.4690, which is the exact shape both thresholds were calibrated on.
        let mut edges = Vec::new();
        let mut buyers = Vec::new();
        let balances = [SUPPLY / 100 * 90, SUPPLY / 100 * 10];
        for (index, balance) in balances.iter().enumerate() {
            let wallet = format!("member{index}");
            edges.push(edge(
                "operator",
                &wallet,
                2 * SOL,
                LAUNCH - HOUR,
                &format!("f{index}"),
            ));
            buyers.push(buyer(&wallet, LAUNCH, SOL, *balance));
        }
        // A third member so the group clears the floor, holding nothing.
        edges.push(edge("operator", "member2", 2 * SOL, LAUNCH - HOUR, "f2"));
        buyers.push(buyer("member2", LAUNCH, SOL, 0));

        let report = run(edges, &[], &buyers, &context());
        let cluster = &report.clusters[0];
        assert!(
            cluster.costume_ring,
            "hhi {:?} entropy {:?}",
            cluster.holding_hhi_bps, cluster.holding_entropy_micros
        );
        assert!(report
            .insider
            .as_ref()
            .expect("scoreable")
            .reasons
            .contains(&InsiderReason::CostumeRing));
    }

    #[test]
    fn wallets_that_each_hold_something_are_not_a_costume_ring() {
        let (edges, buyers) = sybil_ring();
        let report = run(edges, &[], &buyers, &context());
        // Four equal holdings: index 2 500, entropy exactly one.
        assert_eq!(report.clusters[0].holding_hhi_bps, Some(2_500));
        assert_eq!(report.clusters[0].holding_entropy_micros, Some(MICROS));
        assert!(!report.clusters[0].costume_ring);
    }

    // -----------------------------------------------------------------
    // Insider accumulation
    // -----------------------------------------------------------------

    #[test]
    fn accumulation_before_migration_is_named_as_such() {
        let (edges, buyers) = sybil_ring();
        let mut context = context();
        // Every buy above lands within two seconds of the launch, so a
        // migration an hour later leaves all of it on the pre-migration side.
        context.migration_ms = Some(LAUNCH + HOUR);
        let report = run(edges, &[], &buyers, &context);

        let insider = report.insider.expect("scoreable");
        assert_eq!(insider.pre_migration_share_bps, 10_000);
        assert_eq!(insider.pre_migration_buy_lamports, 4 * SOL);
        assert!(insider
            .reasons
            .contains(&InsiderReason::PreMigrationAccumulation));
    }

    #[test]
    fn buying_after_migration_does_not_earn_the_reason() {
        let (edges, mut buyers) = sybil_ring();
        // Move every buy past the migration.
        for participant in &mut buyers {
            participant.first_buy_ms = LAUNCH + 2 * HOUR;
        }
        let mut context = context();
        context.migration_ms = Some(LAUNCH + HOUR);
        let report = run(edges, &[], &buyers, &context);

        let insider = report.insider.expect("scoreable");
        assert_eq!(insider.pre_migration_share_bps, 0);
        assert!(!insider
            .reasons
            .contains(&InsiderReason::PreMigrationAccumulation));
    }

    #[test]
    fn a_launch_that_never_migrated_does_not_claim_accumulation_preceded_one() {
        let (edges, buyers) = sybil_ring();
        let report = run(edges, &[], &buyers, &context());
        let insider = report.insider.expect("scoreable");
        // The whole is pre-migration because the curve has not migrated, and
        // the reason still does not fire, because nothing has been shown to
        // precede anything.
        assert_eq!(insider.pre_migration_share_bps, 10_000);
        assert!(!insider
            .reasons
            .contains(&InsiderReason::PreMigrationAccumulation));
    }

    #[test]
    fn an_unmeasurable_component_is_left_out_rather_than_scored_zero() {
        // No supply figure, so ownership is UNKNOWN. The score is taken over
        // the three weights that survived, and the report says so.
        let (edges, buyers) = sybil_ring();
        let mut context = context();
        context.circulating_supply = 0;
        let report = run(edges, &[], &buyers, &context);

        let insider = report.insider.expect("scoreable");
        assert_eq!(insider.components.ownership_micros, None);
        assert_eq!(report.clusters[0].ownership_bps, None);
        let params = ClusteringParams::default();
        assert_eq!(
            insider.measured_weight_bps,
            params.weight_sync_bps + params.weight_fund_bps + params.weight_uniformity_bps
        );
        assert!(insider.measured_weight_bps < 10_000);
        assert!(!insider
            .reasons
            .contains(&InsiderReason::ConcentratedOwnership));
    }

    #[test]
    fn the_score_is_the_weighted_mean_of_what_was_measured() {
        let (edges, buyers) = sybil_ring();
        let report = run(edges, &[], &buyers, &context());
        let insider = report.insider.expect("scoreable");
        let components = &insider.components;

        let params = ClusteringParams::default();
        let mut weighted = 0u128;
        let mut weight = 0u128;
        for (value, w) in [
            (components.sync_micros, params.weight_sync_bps),
            (components.launch_share_micros, params.weight_fund_bps),
            (components.ownership_micros, params.weight_ownership_bps),
            (components.uniformity_micros, params.weight_uniformity_bps),
        ] {
            if let Some(value) = value {
                weighted += u128::from(value) * u128::from(w);
                weight += u128::from(w);
            }
        }
        assert_eq!(insider.score_micros, (weighted / weight) as u64);
        assert_eq!(insider.measured_weight_bps as u128, weight);
    }

    #[test]
    fn a_cluster_whose_buys_are_hours_apart_scores_far_below_one_that_bundled() {
        let (edges, buyers) = sybil_ring();
        let together = run(edges.clone(), &[], &buyers, &context())
            .insider
            .expect("scoreable")
            .score_micros;

        let mut spread = buyers.clone();
        for (index, participant) in spread.iter_mut().enumerate() {
            participant.first_buy_ms = LAUNCH + index as i64 * HOUR;
        }
        let apart = run(edges, &[], &spread, &context())
            .insider
            .expect("scoreable")
            .score_micros;

        assert!(
            apart < together,
            "a group that bought over four hours is somebody managing positions"
        );
    }

    #[test]
    fn ownership_is_measured_against_the_ceiling_not_against_everything() {
        // A cluster holding a fifth of the supply reads as fully concentrated;
        // holding a tenth reads as half.
        let mut edges = Vec::new();
        let mut buyers = Vec::new();
        for index in 0..4 {
            let wallet = format!("m{index}");
            edges.push(edge(
                "operator",
                &wallet,
                2 * SOL,
                LAUNCH - HOUR,
                &format!("f{index}"),
            ));
            buyers.push(buyer(&wallet, LAUNCH, SOL, SUPPLY / 40));
        }
        // Four wallets at a fortieth each is a tenth of the supply.
        let report = run(edges, &[], &buyers, &context());
        assert_eq!(report.clusters[0].ownership_bps, Some(1_000));
        assert_eq!(
            report
                .insider
                .expect("scoreable")
                .components
                .ownership_micros,
            Some(500_000)
        );
    }

    // -----------------------------------------------------------------
    // The dev tracer
    // -----------------------------------------------------------------

    #[test]
    fn the_dev_wallet_is_traced_back_to_whoever_paid_it() {
        let (mut edges, buyers) = sybil_ring();
        edges.push(edge("operator", "dev", 5 * SOL, LAUNCH - 3 * HOUR, "d1"));
        let mut context = context();
        context.dev_wallet = Some("dev".to_string());
        let report = run(edges, &[], &buyers, &context);

        let dev = report.dev.expect("a dev trace");
        assert_eq!(dev.wallet, "dev");
        assert_eq!(dev.origin.as_deref(), Some("operator"));
        assert_eq!(dev.hops, 1);
        assert_eq!(dev.exit_node, None);
        // The question that matters: who else was paid by whoever paid the dev.
        assert_eq!(
            dev.siblings,
            vec!["puppet0", "puppet1", "puppet2", "puppet3"]
        );
        assert_eq!(dev.sibling_buy_lamports, 4 * SOL);
        assert_eq!(dev.cluster_root.as_deref(), Some("operator"));
        assert!(report
            .insider
            .expect("scoreable")
            .reasons
            .contains(&InsiderReason::DevSharesOrigin));
    }

    #[test]
    fn a_dev_out_of_an_exchange_names_the_exit_node_and_links_nobody() {
        let (mut edges, buyers) = sybil_ring();
        edges.push(edge("cex", "dev", 5 * SOL, LAUNCH - 3 * HOUR, "d1"));
        edges.push(edge(
            "someone-else",
            "cex",
            900 * SOL,
            LAUNCH - 8 * HOUR,
            "d0",
        ));
        let mut context = context();
        context.dev_wallet = Some("dev".to_string());
        let report = run(edges, &[("cex", NodeKind::Exchange)], &buyers, &context);

        let dev = report.dev.expect("a dev trace");
        assert_eq!(dev.origin.as_deref(), Some("cex"));
        assert_eq!(dev.origin_kind, Some(NodeKind::Exchange));
        assert_eq!(dev.exit_node.as_deref(), Some("cex"));
        // Nobody is linked to the dev by that, and nothing behind the exchange
        // leaked through it.
        assert!(dev.siblings.is_empty());
        assert_eq!(dev.cluster_root, None);
        assert!(!report
            .insider
            .expect("scoreable")
            .reasons
            .contains(&InsiderReason::DevSharesOrigin));
    }

    #[test]
    fn a_dev_nobody_funded_is_unknown_rather_than_clean() {
        let (edges, buyers) = sybil_ring();
        let mut context = context();
        context.dev_wallet = Some("unfunded-dev".to_string());
        let report = run(edges, &[], &buyers, &context);

        let dev = report.dev.expect("a dev trace");
        assert_eq!(dev.origin, None);
        assert_eq!(dev.exit_node, None);
        assert_eq!(dev.hops, 0);
        assert!(dev.siblings.is_empty());
    }

    #[test]
    fn a_dev_funded_through_an_intermediary_still_reaches_the_operator() {
        // The laundering shape: the dev is paid by a fresh keypair that the
        // operator funded an hour earlier.
        let (mut edges, buyers) = sybil_ring();
        edges.push(edge("operator", "middle", 6 * SOL, LAUNCH - 4 * HOUR, "d0"));
        edges.push(edge("middle", "dev", 5 * SOL, LAUNCH - 3 * HOUR, "d1"));
        let mut context = context();
        context.dev_wallet = Some("dev".to_string());
        let report = run(edges, &[], &buyers, &context);

        let dev = report.dev.expect("a dev trace");
        assert_eq!(dev.origin.as_deref(), Some("operator"));
        assert_eq!(dev.hops, 2);
        assert_eq!(dev.cluster_root.as_deref(), Some("operator"));
    }

    // -----------------------------------------------------------------
    // Multi-hop and circular funding, through the whole analysis
    // -----------------------------------------------------------------

    #[test]
    fn a_multi_hop_sybil_ring_still_lands_in_one_cluster() {
        // Each puppet is funded by its own fresh intermediary, and every
        // intermediary by one operator. One hand, three hops deep.
        let mut edges = Vec::new();
        let mut buyers = Vec::new();
        for index in 0..4i64 {
            let middle = format!("middle{index}");
            let wallet = format!("puppet{index}");
            edges.push(edge(
                "operator",
                &middle,
                3 * SOL,
                LAUNCH - 5 * HOUR,
                &format!("a{index}"),
            ));
            edges.push(edge(
                &middle,
                &wallet,
                2 * SOL,
                LAUNCH - 2 * HOUR,
                &format!("b{index}"),
            ));
            buyers.push(buyer(&wallet, LAUNCH + index * 400, SOL, SUPPLY / 40));
        }
        let report = run(edges, &[], &buyers, &context());

        assert_eq!(report.clusters.len(), 1);
        assert_eq!(report.clusters[0].root, "operator");
        assert_eq!(report.clusters[0].wallet_count, 4);
        // The ring test reads the bottleneck of each path, which is the second
        // hop here, not the first.
        assert_eq!(report.clusters[0].member_funding_lamports, vec![2 * SOL; 4]);
    }

    #[test]
    fn a_circular_funding_ring_terminates_and_produces_a_report() {
        // Money goes round a three-address loop and out to the buyers. The
        // analysis has to finish and has to name an origin on the loop.
        let mut edges = vec![
            edge("a", "b", 10 * SOL, LAUNCH - 10 * HOUR, "c1"),
            edge("b", "c", 10 * SOL, LAUNCH - 8 * HOUR, "c2"),
            edge("c", "a", 10 * SOL, LAUNCH - 6 * HOUR, "c3"),
        ];
        let mut buyers = Vec::new();
        for index in 0..3i64 {
            let wallet = format!("w{index}");
            edges.push(edge(
                "c",
                &wallet,
                2 * SOL,
                LAUNCH - 2 * HOUR,
                &format!("o{index}"),
            ));
            buyers.push(buyer(&wallet, LAUNCH + index * 300, SOL, SUPPLY / 30));
        }
        let report = run(edges, &[], &buyers, &context());

        assert_eq!(report.clusters.len(), 1);
        // Walking back from a buyer: c <- b <- a, and a's only funder is c,
        // already on the path. So `a` is where the loop bottoms out.
        assert_eq!(report.clusters[0].root, "a");
        assert_eq!(report.clusters[0].wallet_count, 3);
    }

    // -----------------------------------------------------------------
    // Determinism and degenerate inputs
    // -----------------------------------------------------------------

    #[test]
    fn shuffling_the_input_does_not_change_the_report() {
        let (edges, buyers) = sybil_ring();
        let forward = run(edges.clone(), &[], &buyers, &context());

        let mut reversed_edges = edges.clone();
        reversed_edges.reverse();
        let mut reversed_buyers = buyers.clone();
        reversed_buyers.reverse();
        assert_eq!(
            run(reversed_edges, &[], &reversed_buyers, &context()),
            forward
        );

        let mut rotated = edges;
        rotated.rotate_left(2);
        assert_eq!(run(rotated, &[], &buyers, &context()), forward);
    }

    #[test]
    fn a_wallet_listed_twice_is_one_buyer() {
        let (edges, mut buyers) = sybil_ring();
        let once = run(edges.clone(), &[], &buyers, &context());
        buyers.push(buyers[0].clone());
        let twice = run(edges, &[], &buyers, &context());
        assert_eq!(once, twice);
    }

    #[test]
    fn a_launch_with_no_funding_data_reports_unknown_rather_than_no_syndicate() {
        let (_, buyers) = sybil_ring();
        let report = run(Vec::new(), &[], &buyers, &context());

        assert!(report.clusters.is_empty());
        assert_eq!(report.unclustered_wallets, 4);
        assert_eq!(report.attributed_volume_lamports, 0);
        assert_eq!(report.unattributed_volume_lamports, 4 * SOL);
        assert_eq!(report.launch_fund_micros, None);
        assert_eq!(report.insider, None);
    }

    #[test]
    fn one_buyer_is_not_a_cluster() {
        let edges = vec![edge("operator", "only", 2 * SOL, LAUNCH - HOUR, "f1")];
        let buyers = vec![buyer("only", LAUNCH, SOL, SUPPLY / 10)];
        let report = run(edges, &[], &buyers, &context());
        assert!(report.clusters.is_empty());
        assert_eq!(report.insider, None);
    }

    #[test]
    fn balances_of_zero_leave_concentration_unknown() {
        let mut edges = Vec::new();
        let mut buyers = Vec::new();
        for index in 0..3 {
            let wallet = format!("m{index}");
            edges.push(edge(
                "operator",
                &wallet,
                2 * SOL,
                LAUNCH - HOUR,
                &format!("f{index}"),
            ));
            buyers.push(buyer(&wallet, LAUNCH, SOL, 0));
        }
        let report = run(edges, &[], &buyers, &context());
        // §2.4: an empty population has no concentration; it does not have a
        // concentration of zero.
        assert_eq!(report.clusters[0].holding_hhi_bps, None);
        assert_eq!(report.clusters[0].holding_entropy_micros, None);
        assert!(!report.clusters[0].costume_ring);
        assert_eq!(report.clusters[0].ownership_bps, Some(0));
    }

    #[test]
    fn every_buy_in_the_same_instant_is_full_synchrony_and_no_division_by_zero() {
        // §7.1's "buy times identical" row.
        let mut edges = Vec::new();
        let mut buyers = Vec::new();
        for index in 0..4 {
            let wallet = format!("m{index}");
            edges.push(edge(
                "operator",
                &wallet,
                2 * SOL,
                LAUNCH - HOUR,
                &format!("f{index}"),
            ));
            buyers.push(buyer(&wallet, LAUNCH, SOL, SUPPLY / 40));
        }
        let report = run(edges, &[], &buyers, &context());
        assert_eq!(report.clusters[0].sync_micros, Some(MICROS));
    }

    #[test]
    fn the_influence_is_unknown_rather_than_zero_when_a_half_is_missing() {
        // §3.5: the geometric mean would return zero, and a zero in that column
        // reads as "these wallets are unrelated".
        let (edges, buyers) = sybil_ring();
        let report = run(edges, &[], &buyers, &context());
        assert!(report.clusters[0].temporal_influence_micros.is_some());

        let empty = run(Vec::new(), &[], &buyers, &context());
        assert!(empty.clusters.is_empty());
        assert_eq!(empty.launch_fund_micros, None);
    }

    #[test]
    fn a_budget_bound_traversal_marks_the_whole_report() {
        let mut edges = Vec::new();
        let mut buyers = Vec::new();
        // A chain deeper than the budget behind every puppet, plus a shallow
        // funder so the cluster still resolves.
        for index in 0..4i64 {
            let wallet = format!("p{index}");
            edges.push(edge(
                "operator",
                &wallet,
                2 * SOL,
                LAUNCH - HOUR,
                &format!("s{index}"),
            ));
            edges.push(edge(
                "deep",
                &wallet,
                SOL,
                LAUNCH - 2 * HOUR,
                &format!("d{index}"),
            ));
            edges.push(edge("deeper", "deep", 50 * SOL, LAUNCH - 4 * HOUR, "z1"));
            buyers.push(buyer(&wallet, LAUNCH, SOL, SUPPLY / 40));
        }
        let policy = TracePolicy::default();
        let graph = FundingGraph::build(edges, &[], &policy);
        let report = analyse(
            &graph,
            &buyers,
            &context(),
            &policy,
            &TraceBudget {
                depth: 1,
                ..TraceBudget::default()
            },
            &ClusteringParams::default(),
        );

        assert!(report.truncated);
        assert!(report.clusters[0].truncated);
        assert!(report.insider.expect("scoreable").truncated);
    }

    // -----------------------------------------------------------------
    // Precision
    // -----------------------------------------------------------------

    #[test]
    fn a_lamport_of_supply_does_not_overflow_the_ownership_share() {
        let mut edges = Vec::new();
        let mut buyers = Vec::new();
        for index in 0..3 {
            let wallet = format!("m{index}");
            edges.push(edge(
                "operator",
                &wallet,
                2 * SOL,
                LAUNCH - HOUR,
                &format!("f{index}"),
            ));
            buyers.push(buyer(&wallet, LAUNCH, SOL, u64::MAX / 4));
        }
        let mut context = context();
        context.circulating_supply = u64::MAX;
        let report = run(edges, &[], &buyers, &context);
        // Three quarters of everything, and no overflow on the way.
        assert_eq!(report.clusters[0].ownership_bps, Some(7_500));
    }

    #[test]
    fn the_largest_balances_representable_still_measure() {
        // §15's P13 in this module's terms: the concentration index over
        // extreme balances neither panics nor leaves the interval.
        let mut edges = Vec::new();
        let mut buyers = Vec::new();
        for (index, balance) in [u64::MAX, u64::MAX / 2, 1].iter().enumerate() {
            let wallet = format!("m{index}");
            edges.push(edge(
                "operator",
                &wallet,
                2 * SOL,
                LAUNCH - HOUR,
                &format!("f{index}"),
            ));
            buyers.push(buyer(&wallet, LAUNCH, u64::MAX / 4, *balance));
        }
        let report = run(edges, &[], &buyers, &context());
        let cluster = &report.clusters[0];

        // The index widens to `u128` before squaring, so it measures.
        assert!(cluster.holding_hhi_bps.unwrap() <= 10_000);
        assert!(cluster.flow_share_bps <= 10_000);
        assert!(cluster.sync_micros.unwrap() <= MICROS);
        if let Some(influence) = cluster.temporal_influence_micros {
            assert!(influence <= MICROS);
        }

        // The entropy does not, and says so. `weighted_entropy_micros` needs the
        // total of its weights in a `u64` and these overflow it, so the answer is
        // UNKNOWN rather than a wrapped number — which is the whole doctrine
        // working at the one input where it is easiest to get wrong. A balance
        // vector this large cannot occur for a real mint, since the supply is
        // itself a `u64`, so nothing is lost by refusing it.
        assert_eq!(cluster.holding_entropy_micros, None);
        assert!(
            !cluster.costume_ring,
            "an unmeasured entropy must not complete the ring test"
        );
    }

    #[test]
    fn nothing_in_a_finding_ever_leaves_its_interval() {
        // §15's P7 over the shapes §7.1 enumerates: no score outside [0, 1] and
        // no share outside [0, 10 000], whatever the input looked like.
        let shapes: Vec<(Vec<TraceEdge>, Vec<ClusterParticipant>)> = vec![
            sybil_ring(),
            (Vec::new(), sybil_ring().1),
            (sybil_ring().0, Vec::new()),
            (
                vec![edge("a", "a", SOL, LAUNCH - HOUR, "self")],
                sybil_ring().1,
            ),
        ];
        for (edges, buyers) in shapes {
            let report = run(edges, &[], &buyers, &context());
            for cluster in &report.clusters {
                assert!(cluster.flow_share_bps <= 10_000);
                assert!(cluster.ownership_bps.unwrap_or(0) <= 10_000);
                assert!(cluster.sync_micros.unwrap_or(0) <= MICROS);
                assert!(cluster.fund_micros.unwrap_or(0) <= MICROS);
                assert!(cluster.launch_share_micros.unwrap_or(0) <= MICROS);
                assert!(cluster.temporal_influence_micros.unwrap_or(0) <= MICROS);
                assert!(cluster.holding_entropy_micros.unwrap_or(0) <= MICROS);
            }
            if let Some(insider) = &report.insider {
                assert!(insider.score_micros <= MICROS);
                assert!(insider.measured_weight_bps <= 10_000);
                assert!(insider.pre_migration_share_bps <= 10_000);
            }
        }
    }

    // -----------------------------------------------------------------
    // Requests and the registry
    // -----------------------------------------------------------------

    #[test]
    fn a_request_that_does_not_name_a_mint_is_refused() {
        let (edges, participants) = sybil_ring();
        let mut context = context();
        context.mint = "  ".to_string();
        let request = ClusterRequest {
            context,
            participants,
            edges,
            labels: Vec::new(),
            witness: Vec::new(),
            verification: None,
            policy: None,
            budget: None,
            params: None,
        };
        assert!(matches!(request.analyse(), Err(EngineError::Forensics(_))));
    }

    #[test]
    fn a_curve_cannot_migrate_before_it_launches() {
        let (edges, participants) = sybil_ring();
        let mut context = context();
        context.migration_ms = Some(LAUNCH - HOUR);
        let request = ClusterRequest {
            context,
            participants,
            edges,
            labels: Vec::new(),
            witness: Vec::new(),
            verification: None,
            policy: None,
            budget: None,
            params: None,
        };
        assert!(matches!(request.analyse(), Err(EngineError::Forensics(_))));
    }

    #[test]
    fn a_budget_of_zero_is_refused_rather_than_reported_as_unknown() {
        let (edges, participants) = sybil_ring();
        let request = ClusterRequest {
            context: context(),
            participants,
            edges,
            labels: Vec::new(),
            witness: Vec::new(),
            verification: None,
            policy: None,
            budget: Some(TraceBudget {
                depth: 0,
                ..TraceBudget::default()
            }),
            params: None,
        };
        assert!(matches!(request.analyse(), Err(EngineError::Forensics(_))));
    }

    #[test]
    fn a_launch_nobody_bought_is_refused_and_an_empty_graph_is_not() {
        let request = ClusterRequest {
            context: context(),
            participants: Vec::new(),
            edges: Vec::new(),
            labels: Vec::new(),
            witness: Vec::new(),
            verification: None,
            policy: None,
            budget: None,
            params: None,
        };
        assert!(matches!(request.analyse(), Err(EngineError::Forensics(_))));

        // A graph with no edges is a different thing: it is answerable, and the
        // answer is UNKNOWN.
        let (_, participants) = sybil_ring();
        let request = ClusterRequest {
            context: context(),
            participants,
            edges: Vec::new(),
            labels: Vec::new(),
            witness: Vec::new(),
            verification: None,
            policy: None,
            budget: None,
            params: None,
        };
        let report = request.analyse().expect("answerable");
        assert_eq!(report.unclustered_wallets, 4);
    }

    #[test]
    fn a_request_reproduces_the_report_it_produced() {
        let (edges, participants) = sybil_ring();
        let request = ClusterRequest {
            context: context(),
            participants,
            edges,
            labels: Vec::new(),
            witness: Vec::new(),
            verification: None,
            policy: None,
            budget: None,
            params: None,
        };
        let first = request.clone().analyse().expect("answerable");
        let second = request.analyse().expect("answerable");
        assert_eq!(first, second);
    }

    #[test]
    fn a_trace_request_answers_one_wallet() {
        let (edges, _) = sybil_ring();
        let request = TraceRequest {
            wallet: "puppet0".to_string(),
            reference_ms: LAUNCH,
            edges,
            labels: Vec::new(),
            witness: Vec::new(),
            verification: None,
            policy: None,
            budget: None,
        };
        let report = request.run().expect("answerable");
        assert_eq!(report.trace.parent.as_deref(), Some("operator"));
        assert!(
            report.proof.is_none(),
            "nobody was asked, so nothing was checked"
        );

        let request = TraceRequest {
            wallet: " ".to_string(),
            reference_ms: LAUNCH,
            edges: Vec::new(),
            labels: Vec::new(),
            witness: Vec::new(),
            verification: None,
            policy: None,
            budget: None,
        };
        assert!(matches!(request.run(), Err(EngineError::Forensics(_))));
    }

    #[test]
    fn the_registry_keeps_the_last_report_per_mint() {
        let registry = ClusterRegistry::new();
        assert!(registry.is_empty());

        let (edges, buyers) = sybil_ring();
        let report = run(edges.clone(), &[], &buyers, &context());
        registry.record(report.clone());
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.report(&report.mint), Some(report.clone()));
        assert_eq!(registry.report("nothing-here"), None);

        // Re-recording the same mint replaces rather than accumulates.
        registry.record(report.clone());
        assert_eq!(registry.len(), 1);

        let summaries = registry.summaries();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].mint, report.mint);
        assert_eq!(summaries[0].clusters, 1);
        assert_eq!(summaries[0].top_root.as_deref(), Some("operator"));

        registry.clear();
        assert!(registry.is_empty());
    }

    #[test]
    fn the_registry_lists_most_recent_first_and_evicts_the_oldest() {
        let registry = ClusterRegistry::new();
        let (edges, buyers) = sybil_ring();

        for index in 0..(MAX_REPORTS + 3) {
            let mut context = context();
            context.mint = format!("mint{index:04}");
            registry.record(run(edges.clone(), &[], &buyers, &context));
        }
        assert_eq!(registry.len(), MAX_REPORTS);

        let summaries = registry.summaries();
        // Most recently recorded first.
        assert_eq!(summaries[0].mint, format!("mint{:04}", MAX_REPORTS + 2));
        // And the first three recorded are gone.
        assert_eq!(registry.report("mint0000"), None);
        assert_eq!(registry.report("mint0002"), None);
        assert!(registry.report("mint0003").is_some());
    }
}
