//! Where a wallet's money came from, and how strongly one origin can claim it.
//!
//! This is `RISK_AND_SYBIL_SPEC.md` §3.1 through §3.4 — the part
//! `strategy/mod.rs` names as missing and defers: `Cluster::funder_share_bps`
//! is reachability within a couple of hops, and this is the path posterior it
//! is an approximation of. The difference is the whole point of the module. A
//! wallet is *reachable* from an exchange in two hops and that means almost
//! nothing; a wallet that received 4 SOL twenty minutes before the launch,
//! down a three-hop chain whose narrowest edge carried 4 SOL, from an address
//! that funded eleven other buyers the same way, is a different claim, and only
//! the second one survives being weighted by decay, bottleneck and confidence.
//!
//! # Four ideas do the work
//!
//! **A path is only as good as its weakest link.** Confidence multiplies along
//! the path, so one ambiguous hop discounts everything behind it. §3.1 puts
//! `c_e` below one for a program-mediated transfer, an edge only one provider
//! saw, or an amount small enough to be a rent top-up rather than funding.
//!
//! **Money that arrived long ago is weaker evidence.** The decay is
//! `exp(-lambda x age)` with a 24-hour half-life, measured from the launch back
//! to the *latest* edge on the path — the hop that actually delivered.
//!
//! **You cannot attribute more money down a path than its narrowest edge
//! carried.** The bottleneck is the minimum amount along the path, and a path
//! whose narrowest hop moved a hundredth of a SOL does not explain a wallet
//! that bought with four.
//!
//! **Exchanges, bridges and mixers are absorbing.** A path may end at one and
//! may never pass through one. This is structural rather than remembered, and
//! it is the single most important line in the module: an exchange hot wallet
//! pays out to hundreds of thousands of unrelated people, so transiting one
//! links every one of them to every other and the graph collapses into a single
//! meaningless blob. [`FundingGraph::build`] also *infers* the same treatment
//! for any address paying out to [`TracePolicy::hub_degree`] distinct
//! recipients inside the slice it was given, because the router contract that
//! did not make it onto the label list is the same hazard wearing a different
//! hat — see [`NodeKind::Router`].
//!
//! # What it refuses to say
//!
//! Every number here can come back UNKNOWN, and UNKNOWN is never a zero. A
//! wallet no root reaches inside the windows has [`WalletTrace::parent`] of
//! `None`; §3.3 is explicit that this is neither "self-funded" nor "clean". A
//! zero posterior would read as "we looked and nobody funded this", which is a
//! claim the traversal has not made.
//!
//! Every loop has a hard budget and hitting one sets [`WalletTrace::truncated`]
//! rather than extending the budget. A truncated influence is a **lower bound**:
//! more search could only find more funding, never less. Per the spec's
//! conventions a lower bound may block an entry and may never clear one, and
//! the asymmetry is why it is safe to put a ceiling on forensic work at all.
//!
//! The one place that reasoning needs care is the posterior, which is a
//! *ratio*. Dropping a path lowers one root's influence and so can raise
//! another root's share of the total. So the posterior is not itself a lower
//! bound, and [`WalletTrace::truncated`] travels with it everywhere:
//! `clustering.rs` is where the "may block, may not clear" rule is actually
//! enforced, and it enforces it on the flag rather than on the number.
//!
//! # Determinism
//!
//! Two runs over the same edges produce the same bytes. Addresses are interned
//! in sorted order so "in address order" is "in node id order"; in-edges are
//! held in `(amount desc, signature asc, source asc)` order, which is total
//! because a signature is unique and a source cannot repeat within one; the
//! frontier is processed in `(node, edge trail)` order; and every arithmetic
//! step is integer division. Nothing here calls a libm function, so two
//! machines cannot disagree in the last bit of a stored score.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::strategy::fixed::{exp_neg, Fixed};

// ===========================================================================
// Policy
// ===========================================================================

/// §3.2's `W_lookback`: how far before the launch a funding edge is considered
/// at all. Seventy-two hours.
pub const DEFAULT_LOOKBACK_MS: i64 = 72 * 60 * 60 * 1_000;

/// §3.2's `dt_hop`: the largest gap allowed between two consecutive hops on one
/// path. Six hours.
pub const DEFAULT_DT_HOP_MS: i64 = 6 * 60 * 60 * 1_000;

/// §3.3's decay half-life. Twenty-four hours.
pub const DEFAULT_HALF_LIFE_MS: i64 = 24 * 60 * 60 * 1_000;

/// §3.3's `theta`: the bottleneck flow at which a path counts fully. 0.1 SOL.
pub const DEFAULT_THETA_LAMPORTS: u64 = 100_000_000;

/// §3.3's `kappa`: the discount on corroborating edge-disjoint paths, in basis
/// points. 0.25.
pub const DEFAULT_KAPPA_BPS: u64 = 2_500;

/// Distinct recipients above which an unlabelled address is treated as a router
/// rather than a person. Matches `strategy::syndicate::HUB_DEGREE`, which makes
/// the same call one hop deep.
pub const DEFAULT_HUB_DEGREE: usize = 25;

/// §3.4's fan-out, node and edge budgets, verbatim.
pub const DEFAULT_FANOUT: usize = 64;
pub const DEFAULT_NODES: usize = 4_096;
pub const DEFAULT_EDGES: usize = 32_768;

/// How many hops back a lineage is reconstructed. Twenty-four.
///
/// §3.4 published four. Four is the depth at which §3.3's *unweighted* product
/// stops being safe, not the depth at which the money stops moving — and the
/// two are different numbers. A laundering chain is built out of fresh keypairs
/// precisely because each one costs nothing, so the shape §Y.1 describes (a CEX
/// withdrawal, an instant-swap service, three dormant hops, a bridge, the
/// wallet that buys) is eight or ten hops long before it arrives. A traversal
/// that stops at four answers `Truncation::Depth` and an origin of UNKNOWN for
/// every wallet in it. That is the honest answer under a four-hop budget, and
/// it is not a useful one: the attack is *designed* to sit just past the cap.
///
/// **Raising the depth does not raise the work.** The depth cap was never what
/// bounded this traversal — [`DEFAULT_NODES`] and [`DEFAULT_EDGES`] are, and
/// both are unchanged. Twenty-four spends the same edge budget on a longer
/// trail rather than refusing to spend it, so the worst case is the same worst
/// case and the only thing that moves is which [`Truncation`] a bound walk
/// reports.
///
/// **It does change what a long path is worth**, and that part had to come with
/// it. Under §3.3 alone a twenty-four-hop chain of unambiguous SOL transfers
/// scores exactly what one direct transfer scores, because nothing in that
/// product counts hops — so deepening the walk without the missing term would
/// hand an attacker a clean origin for the price of twenty-four keypairs.
/// §Y.2's path plausibility has the term, `exp(-lambda_hops x (hops - 1))`, and
/// [`TracePolicy::hop_half_life`] is it.
pub const DEFAULT_DEPTH: u32 = 24;

/// §Y.2's `lambda_hops`, stated as a half-life in hops. Four.
///
/// A half-life so that it reads the way [`DEFAULT_HALF_LIFE_MS`] does: every
/// four hops of distance halve what a path can claim. One hop is worth one,
/// four hops 0.59, eight 0.35, sixteen 0.12, and the twenty-fourth hop 0.019 —
/// small, reported, visible to an operator, and unable to carry a launch on its
/// own.
///
/// Deliberately not larger. §3.3's posterior is a *ratio*, so a decay applied
/// to every path of the same length cancels out of it entirely; the hop term
/// only ever moves short paths relative to long ones, which is the single thing
/// it exists to do. A half-life short enough to bite at four hops would be
/// re-imposing the old cap by arithmetic instead of by budget.
pub const DEFAULT_HOP_HALF_LIFE: u32 = 4;

/// The versioned policy every number in a trace was computed under.
///
/// §1's exclusion-list rule generalised: a metric computed under one policy and
/// a threshold tuned under another is not a comparison, and without the version
/// stamped next to the metric there is no way to notice. It rides in
/// [`WalletTrace::policy_version`] for exactly that reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TracePolicy {
    /// Bumped to 2 when the walk went to [`DEFAULT_DEPTH`] hops and
    /// [`TracePolicy::hop_half_life`] arrived with it. A version 1 influence and
    /// a version 2 influence are not comparable numbers, which is the whole
    /// reason this field is next to them.
    pub version: u32,
    pub lookback_ms: i64,
    pub dt_hop_ms: i64,
    pub half_life_ms: i64,
    /// §Y.2's hop decay, as a half-life in hops. See [`DEFAULT_HOP_HALF_LIFE`].
    ///
    /// Defaulted on the way in so that a message written against version 1 —
    /// which had no such field — deserialises into the current policy rather
    /// than being rejected. It will be *scored* under version 2, and the
    /// version stamped on the result is what says so.
    #[serde(default = "default_hop_half_life")]
    pub hop_half_life: u32,
    pub theta_lamports: u64,
    pub kappa_bps: u64,
    pub hub_degree: usize,
}

fn default_hop_half_life() -> u32 {
    DEFAULT_HOP_HALF_LIFE
}

impl Default for TracePolicy {
    fn default() -> Self {
        TracePolicy {
            version: 2,
            lookback_ms: DEFAULT_LOOKBACK_MS,
            dt_hop_ms: DEFAULT_DT_HOP_MS,
            half_life_ms: DEFAULT_HALF_LIFE_MS,
            hop_half_life: DEFAULT_HOP_HALF_LIFE,
            theta_lamports: DEFAULT_THETA_LAMPORTS,
            kappa_bps: DEFAULT_KAPPA_BPS,
            hub_degree: DEFAULT_HUB_DEGREE,
        }
    }
}

/// §3.4's traversal budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceBudget {
    /// The most edges one path may have.
    pub depth: u32,
    /// The most in-edges one node may contribute before the largest are kept
    /// and the rest are dropped.
    pub fanout: usize,
    /// Distinct nodes the whole traversal may touch.
    pub nodes: usize,
    /// Edges the whole traversal may walk.
    pub edges: usize,
}

impl Default for TraceBudget {
    fn default() -> Self {
        TraceBudget {
            depth: DEFAULT_DEPTH,
            fanout: DEFAULT_FANOUT,
            nodes: DEFAULT_NODES,
            edges: DEFAULT_EDGES,
        }
    }
}

// ===========================================================================
// Vocabulary
// ===========================================================================

/// What moved along an edge.
///
/// SOL and SPL tokens are traced through the same graph with the same
/// arithmetic — a wallet funded in USDC and a wallet funded in SOL are equally
/// funded — but the asset stays on the edge because the amounts are in
/// different units and a bottleneck taken across two of them would be a
/// comparison of lamports against base units.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Asset {
    Sol,
    Token(String),
}

/// What kind of thing a vertex is. §3.1's vertex set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NodeKind {
    Wallet,
    Exchange,
    Bridge,
    Mixer,
    /// A high-out-degree address the graph inferred rather than was told about.
    ///
    /// §3.1 makes exchanges absorbing because paying out to everybody links
    /// everybody. A router contract, an airdrop distributor and a launchpad fee
    /// account all have that shape and none of them is on anybody's exchange
    /// list, so the degree test catches them structurally. The name is
    /// deliberately not `Exchange`: the graph has observed a fan-out, which is
    /// not the same as having identified a venue, and a report that said
    /// "exchange" would be claiming the second.
    Router,
    Program,
    TokenAccount,
}

impl NodeKind {
    /// Whether a path may end here but never pass through.
    ///
    /// `Program` is deliberately **not** absorbing. §3.1 handles a
    /// program-mediated transfer with a confidence below one rather than by
    /// severing it, because a program is often just the instrument a person
    /// used — and the programs that really do pay out to everybody get caught
    /// by the degree test and relabelled [`NodeKind::Router`] anyway.
    pub fn is_absorbing(self) -> bool {
        matches!(
            self,
            NodeKind::Exchange | NodeKind::Bridge | NodeKind::Mixer | NodeKind::Router
        )
    }
}

/// One transfer, as §3.1 describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceEdge {
    pub from: String,
    pub to: String,
    /// `a_e` — lamports for SOL, base units for a token.
    pub lamports: u64,
    /// `t_e` — block time, epoch milliseconds.
    pub at_ms: i64,
    /// `slot_e` — for ordering inside one millisecond, which two providers
    /// disagreeing by a few hundred of them makes necessary.
    pub slot: u64,
    /// `sig_e` — the edge's identity and its evidence.
    pub signature: String,
    /// `asset_e`.
    pub asset: Asset,
    /// `c_e` in millionths: confidence that this is a real funding
    /// relationship. A million is certain.
    pub confidence_micros: u32,
}

/// What makes one edge a different edge from another: the signature, both
/// endpoints, the asset, the amount, the time and the slot.
///
/// Everything on [`TraceEdge`] except `confidence_micros`, which is this
/// build's opinion about the edge rather than part of the edge.
type EdgeIdentity = (String, String, String, Asset, u64, i64, u64);

/// One hop of a path, in the direction the money moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceHop {
    pub from: String,
    pub to: String,
    pub lamports: u64,
    pub at_ms: i64,
    pub slot: u64,
    pub signature: String,
    pub asset: Asset,
    pub confidence_micros: u32,
}

/// Which budget ran out first.
///
/// One value rather than a set: the traversal order is total, so "the first
/// budget to bind" is itself deterministic, and a set would invite reading the
/// absence of a reason as evidence about the ones that did not fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Truncation {
    /// One node had more in-edges than the fan-out cap; the largest were kept.
    Fanout,
    /// A path reached the depth cap with funding still behind it.
    Depth,
    /// The distinct-node budget ran out.
    Nodes,
    /// The edge budget ran out.
    Edges,
}

// ===========================================================================
// The graph
// ===========================================================================

/// One edge with its endpoints interned.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GraphEdge {
    from: u32,
    to: u32,
    lamports: u64,
    at_ms: i64,
    slot: u64,
    signature: String,
    asset: Asset,
    confidence_micros: u32,
}

/// The funding graph, assembled once and traversed many times.
///
/// Addresses are interned into ids in sorted order, which makes §3.4's "in
/// address order" a numeric comparison, and in-edges are pre-sorted into the
/// order the fan-out cap cuts at, so no traversal ever sorts anything.
///
/// `Eq` because the assembly is deterministic and that is worth being able to
/// assert: two graphs built from the same transfers in different orders are the
/// same graph, and a test can say so directly rather than by comparing the
/// traces that come out of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingGraph {
    addresses: Vec<String>,
    kinds: Vec<NodeKind>,
    edges: Vec<GraphEdge>,
    /// Edge indices into `edges`, per node, already in cut order.
    in_edges: Vec<Vec<u32>>,
    inferred_routers: Vec<String>,
    self_loops_dropped: u32,
    duplicates_dropped: u32,
}

/// What assembling the graph threw away, so a caller can see a trace resting on
/// a lot of discarded input for what it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphSummary {
    pub nodes: u32,
    pub edges: u32,
    /// Addresses the degree test relabelled [`NodeKind::Router`], sorted.
    ///
    /// Reported rather than silently dropped: "every buyer here came out of one
    /// router" is itself worth knowing, and it is the shape most likely to be
    /// mistaken for a syndicate.
    pub inferred_routers: Vec<String>,
    pub absorbing_nodes: u32,
    /// §7.1: a wallet sending to itself is not an interaction.
    pub self_loops_dropped: u32,
    pub duplicates_dropped: u32,
}

impl FundingGraph {
    /// Assembles a graph from raw transfers and whatever labels are known.
    ///
    /// Three things happen on the way in, and all three are §7.1's degenerate
    /// cases rather than tidying:
    ///
    /// - **Self-loops are dropped.** A wallet sending to itself is not an
    ///   interaction, and a cycle of length one would otherwise have to be
    ///   caught by every traversal instead of once here.
    /// - **Exact duplicates are dropped.** The same transfer reported by two
    ///   providers is one transfer, and counting it twice would let one edge
    ///   corroborate itself in §3.3's edge-disjoint sum.
    /// - **High-degree addresses become routers.** See [`NodeKind::Router`].
    ///   Explicit labels always win: an address the caller called an exchange
    ///   is never relabelled by a degree count.
    pub fn build(
        edges: Vec<TraceEdge>,
        labels: &[(String, NodeKind)],
        policy: &TracePolicy,
    ) -> FundingGraph {
        // Deduplicate on the whole tuple. A signature can legitimately carry
        // several transfers, so the signature alone is not the identity.
        let mut unique: BTreeMap<EdgeIdentity, u32> = BTreeMap::new();
        let mut self_loops_dropped = 0u32;
        let mut duplicates_dropped = 0u32;
        let mut kept: Vec<TraceEdge> = Vec::with_capacity(edges.len());

        for edge in edges {
            if edge.from == edge.to {
                self_loops_dropped += 1;
                continue;
            }
            let key = (
                edge.signature.clone(),
                edge.from.clone(),
                edge.to.clone(),
                edge.asset.clone(),
                edge.lamports,
                edge.at_ms,
                edge.slot,
            );
            if unique.insert(key, 0).is_some() {
                duplicates_dropped += 1;
                continue;
            }
            kept.push(edge);
        }

        let mut addresses: Vec<String> = Vec::with_capacity(kept.len() * 2 + labels.len());
        for edge in &kept {
            addresses.push(edge.from.clone());
            addresses.push(edge.to.clone());
        }
        for (address, _) in labels {
            addresses.push(address.clone());
        }
        addresses.sort();
        addresses.dedup();

        let index: BTreeMap<&str, u32> = addresses
            .iter()
            .enumerate()
            .map(|(id, address)| (address.as_str(), id as u32))
            .collect();

        let mut graph_edges: Vec<GraphEdge> = kept
            .into_iter()
            .map(|edge| GraphEdge {
                from: index[edge.from.as_str()],
                to: index[edge.to.as_str()],
                lamports: edge.lamports,
                at_ms: edge.at_ms,
                slot: edge.slot,
                signature: edge.signature,
                asset: edge.asset,
                confidence_micros: edge.confidence_micros.min(1_000_000),
            })
            .collect();

        // The canonical order, which is also the order §3.4's fan-out cap cuts
        // at: grouped by target, then largest first, then by signature. The
        // source is the final key so that two transfers sharing a signature and
        // an amount still have exactly one order.
        graph_edges.sort_by(|a, b| {
            a.to.cmp(&b.to)
                .then_with(|| b.lamports.cmp(&a.lamports))
                .then_with(|| a.signature.cmp(&b.signature))
                .then_with(|| a.from.cmp(&b.from))
                .then_with(|| a.at_ms.cmp(&b.at_ms))
                .then_with(|| a.slot.cmp(&b.slot))
        });

        let mut in_edges: Vec<Vec<u32>> = vec![Vec::new(); addresses.len()];
        for (id, edge) in graph_edges.iter().enumerate() {
            in_edges[edge.to as usize].push(id as u32);
        }

        // Distinct recipients per source, for the degree test.
        let mut recipients: Vec<BTreeSet<u32>> = vec![BTreeSet::new(); addresses.len()];
        for edge in &graph_edges {
            recipients[edge.from as usize].insert(edge.to);
        }

        let mut kinds = vec![NodeKind::Wallet; addresses.len()];
        let mut labelled = vec![false; addresses.len()];
        for (address, kind) in labels {
            if let Some(&id) = index.get(address.as_str()) {
                kinds[id as usize] = *kind;
                labelled[id as usize] = true;
            }
        }

        let mut inferred_routers = Vec::new();
        for id in 0..addresses.len() {
            if labelled[id] {
                continue;
            }
            if recipients[id].len() >= policy.hub_degree {
                kinds[id] = NodeKind::Router;
                inferred_routers.push(addresses[id].clone());
            }
        }

        FundingGraph {
            addresses,
            kinds,
            edges: graph_edges,
            in_edges,
            inferred_routers,
            self_loops_dropped,
            duplicates_dropped,
        }
    }

    pub fn summary(&self) -> GraphSummary {
        GraphSummary {
            nodes: self.addresses.len() as u32,
            edges: self.edges.len() as u32,
            inferred_routers: self.inferred_routers.clone(),
            absorbing_nodes: self.kinds.iter().filter(|k| k.is_absorbing()).count() as u32,
            self_loops_dropped: self.self_loops_dropped,
            duplicates_dropped: self.duplicates_dropped,
        }
    }

    pub fn node_count(&self) -> usize {
        self.addresses.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// The kind of one address, or [`NodeKind::Wallet`] for one the graph has
    /// never seen. An address with no edges has shown nothing that would make
    /// it anything else.
    pub fn kind_of(&self, address: &str) -> NodeKind {
        self.id_of(address)
            .map(|id| self.kinds[id as usize])
            .unwrap_or(NodeKind::Wallet)
    }

    fn id_of(&self, address: &str) -> Option<u32> {
        self.addresses
            .binary_search_by(|candidate| candidate.as_str().cmp(address))
            .ok()
            .map(|index| index as u32)
    }
}

// ===========================================================================
// The traversal
// ===========================================================================

/// One partial path, growing backwards from the wallet toward a root.
#[derive(Debug, Clone)]
struct Partial {
    node: u32,
    /// Edge indices, in the order they were walked — so `edges[0]` is the hop
    /// into the wallet, which is the chronologically *last* hop of the path.
    edges: Vec<u32>,
    /// Every node on the path, for §3.4's cycle test. At most `depth + 1` long.
    nodes: Vec<u32>,
    depth: u32,
    /// The block time and slot of the hop out of this node, for §3.2's
    /// causality and `dt_hop` tests. `None` at the wallet itself.
    last: Option<(i64, u64)>,
    /// This node is absorbing: record the path and never expand it.
    terminal: bool,
}

/// A completed path and what it is worth.
#[derive(Debug, Clone)]
struct ScoredPath {
    root: u32,
    influence: Fixed,
    edges: Vec<u32>,
    bottleneck_lamports: u64,
}

/// One origin's claim on one wallet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RootInfluence {
    pub root: String,
    pub kind: NodeKind,
    /// `I(r -> v)` from §3.3, in millionths.
    pub influence_micros: u64,
    /// `P(parent = r | v)` from §3.3, in millionths.
    ///
    /// Millionths rather than basis points because this is a score in the sense
    /// `strategy/mod.rs` uses the word, and because it is weighted by volume and
    /// summed downstream — a rounding to a ten-thousandth applied before the
    /// weighting would show up in the total.
    pub posterior_micros: u64,
    /// Hops on the strongest path.
    pub hops: u32,
    /// What §Y.2's hop term cost that path, in millionths.
    ///
    /// Reported rather than folded silently into `influence_micros` because the
    /// two readings of a small influence are different findings: evidence that
    /// is weak, and evidence that is strong but a long way away. A nine-hop
    /// trail discounted to a third is the second, and an operator who cannot
    /// see the discount cannot tell which one is on the screen.
    pub hop_decay_micros: u64,
    /// The narrowest edge on the strongest path — the most money this origin
    /// can be said to have delivered down it.
    pub bottleneck_lamports: u64,
    /// Edge-disjoint paths that corroborated the strongest one.
    pub corroborating_paths: u32,
    /// Block time of the strongest path's final hop, the one the decay is
    /// measured from.
    pub last_hop_ms: i64,
    /// The strongest path itself, root first, as evidence.
    pub best_path: Vec<TraceHop>,
}

/// Everything the traversal learned about one wallet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletTrace {
    pub wallet: String,
    /// Strongest claim first, then by address. Empty means no root reached this
    /// wallet inside the windows.
    pub roots: Vec<RootInfluence>,
    /// The likeliest origin, or `None` for UNKNOWN.
    ///
    /// §3.3: UNKNOWN is not "self-funded" and it is not "clean". A caller that
    /// treats `None` as a zero has inverted the meaning of the measurement.
    pub parent: Option<String>,
    pub parent_posterior_micros: u64,
    /// A budget bound. The influences are lower bounds and the posteriors are
    /// ratios of lower bounds, so this may raise risk and may never clear it.
    pub truncated: bool,
    pub truncation: Option<Truncation>,
    pub paths_found: u32,
    pub nodes_visited: u32,
    pub edges_walked: u32,
    pub policy_version: u32,
}

impl WalletTrace {
    /// A trace with nothing behind it: the wallet is not in the graph, or
    /// nothing funded it inside the windows.
    fn unknown(wallet: &str, policy: &TracePolicy) -> WalletTrace {
        WalletTrace {
            wallet: wallet.to_string(),
            roots: Vec::new(),
            parent: None,
            parent_posterior_micros: 0,
            truncated: false,
            truncation: None,
            paths_found: 0,
            nodes_visited: 0,
            edges_walked: 0,
            policy_version: policy.version,
        }
    }

    /// Whether this trace resolved to an origin at all.
    pub fn is_resolved(&self) -> bool {
        self.parent.is_some()
    }

    /// Whether the posterior may be used to *clear* something.
    ///
    /// The spec's asymmetry in one predicate, so that no caller has to remember
    /// it: a truncated traversal is a lower bound, a lower bound may block an
    /// entry and may never clear one.
    pub fn may_clear(&self) -> bool {
        self.is_resolved() && !self.truncated
    }
}

/// Traces one wallet back to its origins.
///
/// A bounded breadth-first expansion backwards, iterative rather than
/// recursive: §3.4's reason for insisting is that recursion here means an
/// unbounded stack on data whose shape an adversary picks.
pub fn trace_wallet(
    graph: &FundingGraph,
    wallet: &str,
    reference_ms: i64,
    policy: &TracePolicy,
    budget: &TraceBudget,
) -> WalletTrace {
    let Some(start) = graph.id_of(wallet) else {
        return WalletTrace::unknown(wallet, policy);
    };

    let mut frontier = vec![Partial {
        node: start,
        edges: Vec::new(),
        nodes: vec![start],
        depth: 0,
        last: None,
        terminal: false,
    }];
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    seen.insert(start);

    let mut paths: Vec<ScoredPath> = Vec::new();
    let mut truncation: Option<Truncation> = None;
    let mut edges_walked = 0usize;
    let mut exhausted = false;

    while !frontier.is_empty() {
        // §3.4's "in address order", with the edge trail underneath it so that
        // two partials sitting on the same node still have exactly one order.
        frontier.sort_by(|a, b| a.node.cmp(&b.node).then_with(|| a.edges.cmp(&b.edges)));

        let mut next: Vec<Partial> = Vec::new();
        for partial in &frontier {
            // An absorbing node ends the path. Recorded here rather than where
            // it was created so that every path is recorded in frontier order.
            if partial.terminal {
                paths.push(score_path(graph, partial, reference_ms, policy));
                continue;
            }

            let mut candidates = eligible_in_edges(graph, partial, reference_ms, policy);

            if candidates.is_empty() {
                // Nothing funded this node inside the windows, so it is where
                // the money came from. The wallet itself is not a root of its
                // own funding, hence the empty-path guard.
                if !partial.edges.is_empty() {
                    paths.push(score_path(graph, partial, reference_ms, policy));
                }
                continue;
            }

            if partial.depth >= budget.depth {
                // There is funding behind this node and the depth cap says stop.
                // The path is deliberately *not* recorded: recording it would
                // name an intermediate hop as an origin, and a lower bound that
                // invents roots is not a lower bound. Dropping it leaves the
                // influence strictly under-counted, which is the direction the
                // conventions require.
                truncation.get_or_insert(Truncation::Depth);
                continue;
            }

            // A budget ran out earlier in this pass. Nothing more is expanded,
            // but the loop keeps running so that paths already on the frontier
            // still get recorded — work already paid for is evidence already
            // gathered, and throwing it away would lower the bound for nothing.
            if exhausted {
                continue;
            }

            if candidates.len() > budget.fanout {
                // Already in (amount desc, signature asc) order, so keeping the
                // prefix is §3.4's "keep the largest by amount, ties by
                // signature ascending".
                candidates.truncate(budget.fanout);
                truncation.get_or_insert(Truncation::Fanout);
            }

            for index in candidates {
                edges_walked += 1;
                if edges_walked > budget.edges {
                    edges_walked -= 1;
                    truncation.get_or_insert(Truncation::Edges);
                    exhausted = true;
                    break;
                }

                let edge = &graph.edges[index as usize];
                let source = edge.from;

                if seen.insert(source) && seen.len() > budget.nodes {
                    seen.remove(&source);
                    truncation.get_or_insert(Truncation::Nodes);
                    exhausted = true;
                    break;
                }

                let mut edges = Vec::with_capacity(partial.edges.len() + 1);
                edges.extend_from_slice(&partial.edges);
                edges.push(index);
                let mut nodes = Vec::with_capacity(partial.nodes.len() + 1);
                nodes.extend_from_slice(&partial.nodes);
                nodes.push(source);

                next.push(Partial {
                    node: source,
                    edges,
                    nodes,
                    depth: partial.depth + 1,
                    last: Some((edge.at_ms, edge.slot)),
                    terminal: graph.kinds[source as usize].is_absorbing(),
                });
            }
        }

        frontier = next;
    }

    assemble(
        graph,
        wallet,
        paths,
        truncation,
        seen.len() as u32,
        edges_walked as u32,
        policy,
    )
}

/// The in-edges of one node that a path may actually take, in cut order.
///
/// Four filters, all §3.2: inside the lookback, not after the reference time,
/// non-decreasing in `(t, slot)` forward along the path, and within `dt_hop` of
/// the hop already taken. Plus §3.4's cycle test, which is what makes a
/// circular funding ring terminate rather than spin.
fn eligible_in_edges(
    graph: &FundingGraph,
    partial: &Partial,
    reference_ms: i64,
    policy: &TracePolicy,
) -> Vec<u32> {
    graph.in_edges[partial.node as usize]
        .iter()
        .copied()
        .filter(|&index| {
            let edge = &graph.edges[index as usize];

            // The window is the run-up to the launch. An edge stamped after the
            // reference time is outside what §3.2 considers at all — money that
            // arrived after the launch did not pay for the buy.
            if edge.at_ms > reference_ms {
                return false;
            }
            if reference_ms.saturating_sub(edge.at_ms) > policy.lookback_ms {
                return false;
            }

            if let Some((next_ms, next_slot)) = partial.last {
                // Forward along the path the times must be non-decreasing, so
                // walking backwards they must be non-increasing.
                if edge.at_ms > next_ms || (edge.at_ms == next_ms && edge.slot > next_slot) {
                    return false;
                }
                if next_ms.saturating_sub(edge.at_ms) > policy.dt_hop_ms {
                    return false;
                }
            }

            // §3.4: a source already on this path would be a cycle.
            !partial.nodes.contains(&edge.from)
        })
        .collect()
}

/// §3.3's `influence(p)` with §Y.2's hop term, composed at `10^-18` throughout.
///
/// Four factors, and the fourth is the one §3.3 leaves out. §3.3 was written
/// against a four-hop budget, where the difference between a one-hop claim and
/// a four-hop one is small enough to ignore; at [`DEFAULT_DEPTH`] it is not,
/// and §Y.2's `exp(-lambda_hops x (hops - 1))` is the published form of the
/// correction. It is composed here rather than applied afterwards because all
/// four factors are under one and multiplying them at `10^-18` is what keeps
/// three near-unit numbers from rounding each other away.
fn score_path(
    graph: &FundingGraph,
    partial: &Partial,
    reference_ms: i64,
    policy: &TracePolicy,
) -> ScoredPath {
    let mut confidence = Fixed::ONE;
    let mut bottleneck = u64::MAX;
    for &index in &partial.edges {
        let edge = &graph.edges[index as usize];
        confidence =
            confidence.saturating_mul(Fixed::from_micros(u64::from(edge.confidence_micros)));
        bottleneck = bottleneck.min(edge.lamports);
    }

    // `age(p) = t_ref - t_last_edge`. Walking backwards, the first hop taken is
    // the last one chronologically — it is the one that actually delivered.
    let delivered_at = graph.edges[partial.edges[0] as usize].at_ms;
    let age_ms = reference_ms.saturating_sub(delivered_at).max(0) as u128;
    let exponent = Fixed::LN2.saturating_mul(Fixed::ratio_unclamped(
        age_ms,
        policy.half_life_ms.max(1) as u128,
    ));
    let decay = exp_neg(exponent);

    // `min(1, flow / theta)`, and `from_ratio` is the clamp.
    let flow = Fixed::from_ratio(
        u128::from(bottleneck),
        u128::from(policy.theta_lamports.max(1)),
    );

    // §Y.2's hop term. `hops - 1` rather than `hops` so that a direct transfer
    // is undiscounted: the first hop is the funding, and every one after it is
    // distance between the origin and the claim.
    let hop_decay = hop_decay(partial.edges.len() as u32, policy);

    ScoredPath {
        root: partial.node,
        influence: confidence
            .saturating_mul(decay)
            .saturating_mul(flow)
            .saturating_mul(hop_decay),
        edges: partial.edges.clone(),
        bottleneck_lamports: if bottleneck == u64::MAX {
            0
        } else {
            bottleneck
        },
    }
}

/// §Y.2's `exp(-lambda_hops x (hops - 1))`, at a half-life in hops.
///
/// One for a direct transfer and for the degenerate zero-hop path, halving
/// every [`TracePolicy::hop_half_life`] hops after that. Public because the
/// number belongs in a report beside the influence it discounted: an operator
/// looking at a nine-hop trail is owed the fact that it was scored at a third
/// of what the same evidence one hop away would have been.
pub fn hop_decay(hops: u32, policy: &TracePolicy) -> Fixed {
    let exponent = Fixed::LN2.saturating_mul(Fixed::ratio_unclamped(
        u128::from(hops.saturating_sub(1)),
        u128::from(policy.hop_half_life.max(1)),
    ));
    exp_neg(exponent)
}

/// Turns scored paths into per-root influences and the posterior over them.
fn assemble(
    graph: &FundingGraph,
    wallet: &str,
    paths: Vec<ScoredPath>,
    truncation: Option<Truncation>,
    nodes_visited: u32,
    edges_walked: u32,
    policy: &TracePolicy,
) -> WalletTrace {
    let paths_found = paths.len() as u32;

    let mut by_root: BTreeMap<u32, Vec<ScoredPath>> = BTreeMap::new();
    for path in paths {
        by_root.entry(path.root).or_default().push(path);
    }

    struct Claim {
        root: u32,
        influence: Fixed,
        best: ScoredPath,
        corroborating: u32,
    }

    let mut claims: Vec<Claim> = Vec::with_capacity(by_root.len());
    for (root, mut group) in by_root {
        // Strongest first; the edge trail underneath makes the order total, so
        // which path becomes `p*` never depends on iteration order.
        group.sort_by(|a, b| {
            b.influence
                .cmp(&a.influence)
                .then_with(|| a.edges.cmp(&b.edges))
        });

        let best = group.remove(0);
        let best_edges: BTreeSet<u32> = best.edges.iter().copied().collect();

        // §3.3 exactly as written: `D` is the set of paths edge-disjoint **from
        // `p*`**, not from each other. Two members of `D` sharing an edge with
        // one another are both counted, and `kappa` is the discount that exists
        // because corroborating paths "are usually not as independent as they
        // look". Making `D` mutually disjoint instead would be a different,
        // stricter metric than the one the spec publishes.
        let mut corroboration = Fixed::ZERO;
        let mut corroborating = 0u32;
        for path in &group {
            if path.edges.iter().any(|edge| best_edges.contains(edge)) {
                continue;
            }
            corroboration = corroboration.saturating_add(path.influence);
            corroborating += 1;
        }

        // The corroboration sum saturates at one *before* the discount, so
        // corroboration can add at most `kappa` to a root's influence. One
        // strong path is the claim; the rest is a bounded top-up.
        let influence = best
            .influence
            .saturating_add(corroboration.scale_bps(policy.kappa_bps));

        claims.push(Claim {
            root,
            influence,
            best,
            corroborating,
        });
    }

    // §3.3's denominator. Unclamped: it is a total over roots and routinely
    // exceeds one.
    let total = claims
        .iter()
        .fold(Fixed::ZERO, |sum, claim| sum.add_unclamped(claim.influence));

    if total.is_zero() {
        // No root reaches this wallet inside the windows. UNKNOWN, and the
        // roots list stays empty rather than carrying zero-posterior rows that
        // would read as "we found these and they were worth nothing".
        return WalletTrace {
            wallet: wallet.to_string(),
            roots: Vec::new(),
            parent: None,
            parent_posterior_micros: 0,
            truncated: truncation.is_some(),
            truncation,
            paths_found,
            nodes_visited,
            edges_walked,
            policy_version: policy.version,
        };
    }

    let mut roots: Vec<RootInfluence> = claims
        .into_iter()
        .map(|claim| {
            let best_path = claim
                .best
                .edges
                .iter()
                .rev() // walked backwards; report it the way the money moved
                .map(|&index| {
                    let edge = &graph.edges[index as usize];
                    TraceHop {
                        from: graph.addresses[edge.from as usize].clone(),
                        to: graph.addresses[edge.to as usize].clone(),
                        lamports: edge.lamports,
                        at_ms: edge.at_ms,
                        slot: edge.slot,
                        signature: edge.signature.clone(),
                        asset: edge.asset.clone(),
                        confidence_micros: edge.confidence_micros,
                    }
                })
                .collect::<Vec<_>>();

            let last_hop_ms = graph.edges[claim.best.edges[0] as usize].at_ms;

            RootInfluence {
                root: graph.addresses[claim.root as usize].clone(),
                kind: graph.kinds[claim.root as usize],
                influence_micros: claim.influence.to_micros(),
                posterior_micros: claim.influence.share_of(total).to_micros(),
                hops: claim.best.edges.len() as u32,
                hop_decay_micros: hop_decay(claim.best.edges.len() as u32, policy).to_micros(),
                bottleneck_lamports: claim.best.bottleneck_lamports,
                corroborating_paths: claim.corroborating,
                last_hop_ms,
                best_path,
            }
        })
        .collect();

    // Strongest claim first, with a total order underneath so that two roots
    // that round to the same posterior never swap between runs.
    roots.sort_by(|a, b| {
        b.posterior_micros
            .cmp(&a.posterior_micros)
            .then_with(|| b.influence_micros.cmp(&a.influence_micros))
            .then_with(|| a.root.cmp(&b.root))
    });

    let parent = roots.first().map(|root| root.root.clone());
    let parent_posterior_micros = roots.first().map(|r| r.posterior_micros).unwrap_or(0);

    WalletTrace {
        wallet: wallet.to_string(),
        roots,
        parent,
        parent_posterior_micros,
        truncated: truncation.is_some(),
        truncation,
        paths_found,
        nodes_visited,
        edges_walked,
        policy_version: policy.version,
    }
}

/// Traces a set of wallets, each under its own full budget.
///
/// Returned in address order, and the input is deduplicated first: two requests
/// for one wallet are one trace, and leaving both in would double that wallet's
/// weight in every average taken downstream.
pub fn trace_wallets(
    graph: &FundingGraph,
    wallets: &[String],
    reference_ms: i64,
    policy: &TracePolicy,
    budget: &TraceBudget,
) -> Vec<WalletTrace> {
    let unique: BTreeSet<&String> = wallets.iter().collect();
    unique
        .into_iter()
        .map(|wallet| trace_wallet(graph, wallet, reference_ms, policy, budget))
        .collect()
}

// ===========================================================================
// Funding concentration
// ===========================================================================

/// §3.5's `fund(C)`: the largest volume-weighted share of a group pointing at
/// one origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FundingConcentration {
    pub root: String,
    pub kind: NodeKind,
    /// The share itself, in millionths.
    pub fund_micros: u64,
    /// Weight whose parent is known. The rest is UNKNOWN and is counted rather
    /// than assumed independent.
    pub attributed_weight: u64,
    pub unattributed_weight: u64,
    /// Any trace behind this was budget-bound, so the share is a lower bound.
    pub truncated: bool,
}

/// Computes §3.5's `fund(C)` over a weighted set of traces.
///
/// The weights are buy volumes, and §3.5 is explicit about why they are not
/// wallet counts: forty dust wallets behind one root matter less than two large
/// ones, and an unweighted mean is trivially gamed by generating empty
/// keypairs.
///
/// The denominator is the **whole** weight, including wallets whose parent is
/// UNKNOWN. That makes the result a lower bound — exactly as
/// `backtest::LaunchSybil::fund_bps` does it, and for the same reason: a high
/// value is evidence, a low one is the absence of evidence rather than its
/// opposite.
///
/// `None` when there is no weight to divide by, or when no trace resolved at
/// all. Never `Some(0)`.
pub fn funding_concentration(weighted: &[(&WalletTrace, u64)]) -> Option<FundingConcentration> {
    let total_weight: u128 = weighted.iter().map(|(_, weight)| u128::from(*weight)).sum();
    if total_weight == 0 {
        return None;
    }

    // Weight x posterior, summed per root, at millionths. The kind travels with
    // the sum because a root that is an exchange is not an owner, and the
    // caller has to be able to tell those apart without the graph in hand.
    let mut per_root: BTreeMap<&str, (u128, NodeKind)> = BTreeMap::new();
    let mut attributed: u128 = 0;
    let mut truncated = false;

    for (trace, weight) in weighted {
        if trace.truncated {
            truncated = true;
        }
        if trace.is_resolved() {
            attributed += u128::from(*weight);
        }
        for root in &trace.roots {
            let entry = per_root.entry(root.root.as_str()).or_insert((0, root.kind));
            entry.0 += u128::from(*weight) * u128::from(root.posterior_micros);
        }
    }

    // Largest share wins; the address breaks a tie, so the winner never depends
    // on map iteration order — which is why `per_root` is a `BTreeMap`.
    let (root, (weighted_sum, kind)) = per_root
        .into_iter()
        .max_by(|a, b| a.1 .0.cmp(&b.1 .0).then_with(|| b.0.cmp(a.0)))?;

    let fund_micros = (weighted_sum / total_weight).min(1_000_000) as u64;
    if fund_micros == 0 {
        return None;
    }

    Some(FundingConcentration {
        root: root.to_string(),
        kind,
        fund_micros,
        attributed_weight: attributed.min(u128::from(u64::MAX)) as u64,
        unattributed_weight: (total_weight - attributed).min(u128::from(u64::MAX)) as u64,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An arbitrary but fixed launch time. Every edge below is placed relative
    /// to it, so no test depends on a wall clock.
    const LAUNCH: i64 = 1_700_000_000_000;
    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    const SOL: u64 = 1_000_000_000;

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

    fn edge_with_confidence(
        from: &str,
        to: &str,
        lamports: u64,
        at_ms: i64,
        signature: &str,
        confidence_micros: u32,
    ) -> TraceEdge {
        TraceEdge {
            confidence_micros,
            ..edge(from, to, lamports, at_ms, signature)
        }
    }

    fn build_graph(edges: Vec<TraceEdge>) -> FundingGraph {
        FundingGraph::build(edges, &[], &TracePolicy::default())
    }

    fn labelled(edges: Vec<TraceEdge>, labels: &[(&str, NodeKind)]) -> FundingGraph {
        let owned: Vec<(String, NodeKind)> = labels
            .iter()
            .map(|(address, kind)| ((*address).to_string(), *kind))
            .collect();
        FundingGraph::build(edges, &owned, &TracePolicy::default())
    }

    fn trace_of(graph: &FundingGraph, wallet: &str) -> WalletTrace {
        trace_wallet(
            graph,
            wallet,
            LAUNCH,
            &TracePolicy::default(),
            &TraceBudget::default(),
        )
    }

    // -----------------------------------------------------------------
    // UNKNOWN is a value, not a zero
    // -----------------------------------------------------------------

    #[test]
    fn a_wallet_the_graph_has_never_seen_is_unknown() {
        let graph = build_graph(vec![edge("a", "b", SOL, LAUNCH - HOUR, "s1")]);
        let trace = trace_of(&graph, "stranger");
        assert_eq!(trace.parent, None);
        assert!(trace.roots.is_empty());
        assert_eq!(trace.parent_posterior_micros, 0);
        assert!(!trace.is_resolved());
    }

    #[test]
    fn a_wallet_nobody_funded_is_unknown_and_not_self_funded() {
        // `w` only ever sent money out. §3.3: no root reaching it is UNKNOWN,
        // which is neither "self-funded" nor "clean".
        let graph = build_graph(vec![edge("w", "other", SOL, LAUNCH - HOUR, "s1")]);
        let trace = trace_of(&graph, "w");
        assert_eq!(trace.parent, None);
        assert!(trace.roots.is_empty());
        assert!(!trace.truncated);
    }

    #[test]
    fn an_empty_graph_measures_nothing() {
        let graph = build_graph(Vec::new());
        assert_eq!(graph.node_count(), 0);
        assert_eq!(trace_of(&graph, "w").parent, None);
    }

    // -----------------------------------------------------------------
    // The basic shapes
    // -----------------------------------------------------------------

    #[test]
    fn one_direct_hop_names_its_funder() {
        let graph = build_graph(vec![edge(
            "origin",
            "w",
            5 * SOL,
            LAUNCH - 20 * MINUTE,
            "s1",
        )]);
        let trace = trace_of(&graph, "w");
        assert_eq!(trace.parent.as_deref(), Some("origin"));
        // One root takes the whole posterior.
        assert_eq!(trace.parent_posterior_micros, 1_000_000);
        assert_eq!(trace.roots.len(), 1);
        assert_eq!(trace.roots[0].hops, 1);
        assert_eq!(trace.roots[0].bottleneck_lamports, 5 * SOL);
        assert!(!trace.truncated);
        assert!(trace.may_clear());
    }

    #[test]
    fn a_multi_hop_chain_reaches_the_origin_not_the_middle() {
        // origin -> hop1 -> hop2 -> w, each an hour apart.
        let graph = build_graph(vec![
            edge("origin", "hop1", 9 * SOL, LAUNCH - 3 * HOUR, "s1"),
            edge("hop1", "hop2", 8 * SOL, LAUNCH - 2 * HOUR, "s2"),
            edge("hop2", "w", 7 * SOL, LAUNCH - HOUR, "s3"),
        ]);
        let trace = trace_of(&graph, "w");
        assert_eq!(trace.parent.as_deref(), Some("origin"));
        assert_eq!(trace.roots.len(), 1);
        assert_eq!(trace.roots[0].hops, 3);
        // The bottleneck is the narrowest edge on the path, not the last one.
        assert_eq!(trace.roots[0].bottleneck_lamports, 7 * SOL);
        // Reported in the direction the money moved.
        let path: Vec<&str> = trace.roots[0]
            .best_path
            .iter()
            .map(|hop| hop.from.as_str())
            .collect();
        assert_eq!(path, vec!["origin", "hop1", "hop2"]);
    }

    #[test]
    fn the_evidence_travels_with_the_path() {
        let graph = build_graph(vec![edge("origin", "w", SOL, LAUNCH - HOUR, "sig-abc")]);
        let trace = trace_of(&graph, "w");
        let hop = &trace.roots[0].best_path[0];
        assert_eq!(hop.signature, "sig-abc");
        assert_eq!(hop.asset, Asset::Sol);
        assert_eq!(hop.lamports, SOL);
        assert_eq!(hop.at_ms, LAUNCH - HOUR);
    }

    #[test]
    fn a_token_funded_wallet_traces_the_same_way_a_sol_funded_one_does() {
        let usdc = Asset::Token("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string());
        let mut hop = edge("origin", "w", 250_000_000, LAUNCH - 30 * MINUTE, "s1");
        hop.asset = usdc.clone();
        let graph = build_graph(vec![hop]);
        let trace = trace_of(&graph, "w");
        assert_eq!(trace.parent.as_deref(), Some("origin"));
        assert_eq!(trace.roots[0].best_path[0].asset, usdc);
    }

    // -----------------------------------------------------------------
    // Circular funding
    // -----------------------------------------------------------------

    #[test]
    fn a_circular_funding_path_terminates_without_repeating_a_node() {
        // A ring that feeds itself: a -> b -> c -> a, and c also pays w. Walking
        // back from w must not go round the ring forever, and no path may
        // contain the same address twice.
        let graph = build_graph(vec![
            edge("a", "b", 4 * SOL, LAUNCH - 4 * HOUR, "s1"),
            edge("b", "c", 4 * SOL, LAUNCH - 3 * HOUR, "s2"),
            edge("c", "a", 4 * SOL, LAUNCH - 2 * HOUR, "s3"),
            edge("c", "w", 4 * SOL, LAUNCH - HOUR, "s4"),
        ]);
        let trace = trace_of(&graph, "w");

        // c <- b <- a is the only way back, and `a` has no funder that is not
        // already on the path, so `a` is the origin.
        assert_eq!(trace.parent.as_deref(), Some("a"));
        for root in &trace.roots {
            let mut addresses: Vec<&str> = root
                .best_path
                .iter()
                .map(|hop| hop.from.as_str())
                .chain(root.best_path.last().map(|hop| hop.to.as_str()))
                .collect();
            let before = addresses.len();
            addresses.sort();
            addresses.dedup();
            assert_eq!(addresses.len(), before, "a node repeated on a path");
        }
    }

    #[test]
    fn a_two_wallet_cycle_between_the_same_pair_still_terminates() {
        // The tightest ring that is not a self-loop: a and b pay each other.
        let graph = build_graph(vec![
            edge("a", "b", SOL, LAUNCH - 3 * HOUR, "s1"),
            edge("b", "a", SOL, LAUNCH - 2 * HOUR, "s2"),
            edge("a", "w", SOL, LAUNCH - HOUR, "s3"),
        ]);
        let trace = trace_of(&graph, "w");
        // w <- a <- b, and b's only funder is a, already on the path.
        assert_eq!(trace.parent.as_deref(), Some("b"));
        assert_eq!(trace.roots.len(), 1);
    }

    #[test]
    fn a_self_loop_is_not_an_interaction() {
        // §7.1: dropped before assembly.
        let graph = build_graph(vec![
            edge("w", "w", SOL, LAUNCH - HOUR, "s1"),
            edge("origin", "w", SOL, LAUNCH - HOUR, "s2"),
        ]);
        assert_eq!(graph.summary().self_loops_dropped, 1);
        assert_eq!(graph.edge_count(), 1);
        assert_eq!(trace_of(&graph, "w").parent.as_deref(), Some("origin"));
    }

    #[test]
    fn the_same_transfer_from_two_providers_is_one_edge() {
        let one = edge("origin", "w", SOL, LAUNCH - HOUR, "s1");
        let graph = build_graph(vec![one.clone(), one]);
        assert_eq!(graph.summary().duplicates_dropped, 1);
        assert_eq!(graph.edge_count(), 1);
        // And it cannot corroborate itself.
        assert_eq!(trace_of(&graph, "w").roots[0].corroborating_paths, 0);
    }

    // -----------------------------------------------------------------
    // Absorbing nodes and router false positives
    // -----------------------------------------------------------------

    #[test]
    fn an_exchange_ends_a_path_and_is_never_transited() {
        let graph = labelled(
            vec![
                edge("deep-origin", "cex", 100 * SOL, LAUNCH - 5 * HOUR, "s1"),
                edge("cex", "w", 5 * SOL, LAUNCH - HOUR, "s2"),
            ],
            &[("cex", NodeKind::Exchange)],
        );
        let trace = trace_of(&graph, "w");
        assert_eq!(trace.parent.as_deref(), Some("cex"));
        assert_eq!(trace.roots.len(), 1);
        assert_eq!(trace.roots[0].kind, NodeKind::Exchange);
        // The address behind the exchange must not appear anywhere.
        assert!(trace.roots.iter().all(|root| root.root != "deep-origin"));
    }

    #[test]
    fn two_wallets_out_of_one_exchange_are_not_linked_to_what_is_behind_it() {
        // The doctrine line: transiting a regulated exchange never labels a
        // wallet on its own. Both wallets point at the exchange and neither
        // reaches the address that funded it.
        let graph = labelled(
            vec![
                edge("whale", "cex", 500 * SOL, LAUNCH - 6 * HOUR, "s0"),
                edge("cex", "w1", 5 * SOL, LAUNCH - 2 * HOUR, "s1"),
                edge("cex", "w2", 5 * SOL, LAUNCH - 2 * HOUR, "s2"),
            ],
            &[("cex", NodeKind::Exchange)],
        );
        for wallet in ["w1", "w2"] {
            let trace = trace_of(&graph, wallet);
            assert_eq!(trace.parent.as_deref(), Some("cex"));
            assert!(trace.roots.iter().all(|root| root.root != "whale"));
        }
    }

    #[test]
    fn a_high_volume_router_is_inferred_from_its_degree_and_absorbs() {
        // A contract paying out to thirty distinct addresses is the shape §3.1
        // makes absorbing, whether or not anybody put it on a list.
        let mut edges = Vec::new();
        for index in 0..30 {
            edges.push(edge(
                "router",
                &format!("customer{index:02}"),
                SOL,
                LAUNCH - 2 * HOUR,
                &format!("r{index:02}"),
            ));
        }
        edges.push(edge("behind", "router", 100 * SOL, LAUNCH - 5 * HOUR, "b1"));
        let graph = build_graph(edges);

        assert_eq!(graph.kind_of("router"), NodeKind::Router);
        assert_eq!(graph.summary().inferred_routers, vec!["router".to_string()]);

        let trace = trace_of(&graph, "customer07");
        assert_eq!(trace.parent.as_deref(), Some("router"));
        assert_eq!(trace.roots[0].kind, NodeKind::Router);
        // Nothing behind the router leaks through it.
        assert!(trace.roots.iter().all(|root| root.root != "behind"));
    }

    #[test]
    fn a_funder_below_the_degree_threshold_stays_a_wallet() {
        // The other side of the same test: a real operator funding a handful of
        // wallets must not be swept up by the router rule.
        let mut edges = Vec::new();
        for index in 0..5 {
            edges.push(edge(
                "operator",
                &format!("puppet{index}"),
                SOL,
                LAUNCH - 2 * HOUR,
                &format!("p{index}"),
            ));
        }
        edges.push(edge(
            "source",
            "operator",
            50 * SOL,
            LAUNCH - 5 * HOUR,
            "s1",
        ));
        let graph = build_graph(edges);

        assert_eq!(graph.kind_of("operator"), NodeKind::Wallet);
        assert!(graph.summary().inferred_routers.is_empty());
        // And the path runs through the operator to what funded it.
        let trace = trace_of(&graph, "puppet3");
        assert_eq!(trace.parent.as_deref(), Some("source"));
    }

    #[test]
    fn an_explicit_label_is_never_overridden_by_the_degree_count() {
        let mut edges = Vec::new();
        for index in 0..40 {
            edges.push(edge(
                "known",
                &format!("customer{index:02}"),
                SOL,
                LAUNCH - 2 * HOUR,
                &format!("k{index:02}"),
            ));
        }
        let graph = labelled(edges, &[("known", NodeKind::Bridge)]);
        assert_eq!(graph.kind_of("known"), NodeKind::Bridge);
        assert!(graph.summary().inferred_routers.is_empty());
    }

    #[test]
    fn a_program_is_not_absorbing_and_a_path_runs_through_it() {
        // §3.1 handles a program-mediated transfer with a confidence below one
        // rather than by severing the path.
        let graph = labelled(
            vec![
                edge("origin", "program", 10 * SOL, LAUNCH - 3 * HOUR, "s1"),
                edge_with_confidence("program", "w", 10 * SOL, LAUNCH - 2 * HOUR, "s2", 600_000),
            ],
            &[("program", NodeKind::Program)],
        );
        let trace = trace_of(&graph, "w");
        assert_eq!(trace.parent.as_deref(), Some("origin"));
        assert_eq!(trace.roots[0].hops, 2);
    }

    // -----------------------------------------------------------------
    // The windows
    // -----------------------------------------------------------------

    #[test]
    fn an_edge_older_than_the_lookback_is_not_considered() {
        let graph = build_graph(vec![edge("origin", "w", SOL, LAUNCH - 73 * HOUR, "s1")]);
        assert_eq!(trace_of(&graph, "w").parent, None);
        // One hour inside the window is a different answer.
        let graph = build_graph(vec![edge("origin", "w", SOL, LAUNCH - 71 * HOUR, "s1")]);
        assert_eq!(trace_of(&graph, "w").parent.as_deref(), Some("origin"));
    }

    #[test]
    fn money_that_arrived_after_the_launch_did_not_pay_for_the_buy() {
        let graph = build_graph(vec![edge("origin", "w", SOL, LAUNCH + MINUTE, "s1")]);
        assert_eq!(trace_of(&graph, "w").parent, None);
    }

    #[test]
    fn a_gap_wider_than_dt_hop_breaks_the_path() {
        // origin -> hop is seven hours before hop -> w, and dt_hop is six.
        let graph = build_graph(vec![
            edge("origin", "hop", 5 * SOL, LAUNCH - 9 * HOUR, "s1"),
            edge("hop", "w", 5 * SOL, LAUNCH - 2 * HOUR, "s2"),
        ]);
        let trace = trace_of(&graph, "w");
        assert_eq!(trace.parent.as_deref(), Some("hop"));
        assert_eq!(trace.roots.len(), 1);

        // Five hours apart and the same path reaches all the way back.
        let graph = build_graph(vec![
            edge("origin", "hop", 5 * SOL, LAUNCH - 7 * HOUR, "s1"),
            edge("hop", "w", 5 * SOL, LAUNCH - 2 * HOUR, "s2"),
        ]);
        assert_eq!(trace_of(&graph, "w").parent.as_deref(), Some("origin"));
    }

    #[test]
    fn money_must_move_forward_in_time_along_a_path() {
        // The hop into `w` happened *before* the hop that funded `hop`, so the
        // second cannot have paid for the first.
        let graph = build_graph(vec![
            edge("origin", "hop", 5 * SOL, LAUNCH - HOUR, "s1"),
            edge("hop", "w", 5 * SOL, LAUNCH - 3 * HOUR, "s2"),
        ]);
        let trace = trace_of(&graph, "w");
        assert_eq!(trace.parent.as_deref(), Some("hop"));
        assert_eq!(trace.roots.len(), 1);
    }

    #[test]
    fn a_slot_breaks_a_tie_inside_one_millisecond() {
        // Two providers can stamp the same millisecond; the slot orders them.
        let mut earlier = edge("origin", "hop", 5 * SOL, LAUNCH - HOUR, "s1");
        earlier.slot = 100;
        let mut later = edge("hop", "w", 5 * SOL, LAUNCH - HOUR, "s2");
        later.slot = 200;
        let graph = build_graph(vec![earlier, later]);
        assert_eq!(trace_of(&graph, "w").parent.as_deref(), Some("origin"));

        // Reverse the slots and the path is no longer causal.
        let mut first = edge("origin", "hop", 5 * SOL, LAUNCH - HOUR, "s1");
        first.slot = 200;
        let mut second = edge("hop", "w", 5 * SOL, LAUNCH - HOUR, "s2");
        second.slot = 100;
        let graph = build_graph(vec![first, second]);
        assert_eq!(trace_of(&graph, "w").parent.as_deref(), Some("hop"));
    }

    // -----------------------------------------------------------------
    // The three factors
    // -----------------------------------------------------------------

    #[test]
    fn confidence_multiplies_along_the_path() {
        // Two hops at 0.5 each, delivered at the launch so the decay is exactly
        // one, and both amply over theta so the flow factor is exactly one.
        let graph = build_graph(vec![
            edge_with_confidence("origin", "hop", 5 * SOL, LAUNCH - HOUR, "s1", 500_000),
            edge_with_confidence("hop", "w", 5 * SOL, LAUNCH, "s2", 500_000),
        ]);
        let trace = trace_of(&graph, "w");
        assert_eq!(trace.roots.len(), 1);
        // 0.5 x 0.5, and then §Y.2's term for the one hop of distance. Written
        // as the product rather than as the constant it evaluates to, because
        // the claim under test is that confidence multiplies — a literal here
        // would silently absorb a change to either factor.
        let two_hops = hop_decay(2, &TracePolicy::default());
        assert_eq!(
            trace.roots[0].influence_micros,
            Fixed::from_micros(250_000)
                .saturating_mul(two_hops)
                .to_micros()
        );
        assert_eq!(trace.roots[0].hop_decay_micros, two_hops.to_micros());
    }

    #[test]
    fn the_bottleneck_caps_what_a_path_can_carry() {
        // theta is 0.1 SOL. A path whose narrowest hop moved 0.02 SOL counts at
        // a fifth, however large the other hops were.
        let graph = build_graph(vec![
            edge("origin", "hop", 500 * SOL, LAUNCH - HOUR, "s1"),
            edge("hop", "w", SOL / 50, LAUNCH, "s2"),
        ]);
        let trace = trace_of(&graph, "w");
        let two_hops = hop_decay(2, &TracePolicy::default());
        assert_eq!(
            trace.roots[0].influence_micros,
            Fixed::from_micros(200_000)
                .saturating_mul(two_hops)
                .to_micros()
        );
        assert_eq!(trace.roots[0].bottleneck_lamports, SOL / 50);
    }

    #[test]
    fn a_path_at_or_above_theta_counts_fully() {
        let graph = build_graph(vec![edge(
            "origin",
            "w",
            DEFAULT_THETA_LAMPORTS,
            LAUNCH,
            "s1",
        )]);
        assert_eq!(trace_of(&graph, "w").roots[0].influence_micros, 1_000_000);
    }

    #[test]
    fn age_halves_the_influence_at_the_half_life() {
        // §3.3's decay, read through the traversal rather than through the
        // arithmetic: the same funding a day earlier is worth half as much.
        let fresh = build_graph(vec![edge("origin", "w", 5 * SOL, LAUNCH, "s1")]);
        assert_eq!(trace_of(&fresh, "w").roots[0].influence_micros, 1_000_000);

        let day_old = build_graph(vec![edge("origin", "w", 5 * SOL, LAUNCH - 24 * HOUR, "s1")]);
        assert_eq!(trace_of(&day_old, "w").roots[0].influence_micros, 500_000);

        let two_days = build_graph(vec![edge("origin", "w", 5 * SOL, LAUNCH - 48 * HOUR, "s1")]);
        assert_eq!(trace_of(&two_days, "w").roots[0].influence_micros, 250_000);
    }

    #[test]
    fn the_decay_is_measured_from_the_hop_that_delivered() {
        // Both paths end at the same moment; the older *first* hop must not
        // change the decay, because age is measured from the last edge.
        let recent_chain = build_graph(vec![
            edge("origin", "hop", 5 * SOL, LAUNCH - 5 * HOUR, "s1"),
            edge("hop", "w", 5 * SOL, LAUNCH, "s2"),
        ]);
        // One, less §Y.2's term for the second hop — which is the same for
        // both paths and so cannot be what the age test is reading.
        assert_eq!(
            trace_of(&recent_chain, "w").roots[0].influence_micros,
            hop_decay(2, &TracePolicy::default()).to_micros()
        );
    }

    // -----------------------------------------------------------------
    // Corroboration and the posterior
    // -----------------------------------------------------------------

    #[test]
    fn edge_disjoint_paths_corroborate_at_the_kappa_discount() {
        // Two entirely separate two-hop routes from one origin to one wallet.
        // Both are worth 1.0, so I = 1.0 + 0.25 x 1.0, which saturates at one.
        // What is testable without the clamp is the count.
        let graph = build_graph(vec![
            edge("origin", "left", 5 * SOL, LAUNCH - 2 * HOUR, "s1"),
            edge("left", "w", 5 * SOL, LAUNCH - HOUR, "s2"),
            edge("origin", "right", 5 * SOL, LAUNCH - 2 * HOUR, "s3"),
            edge("right", "w", 5 * SOL, LAUNCH - HOUR, "s4"),
        ]);
        let trace = trace_of(&graph, "w");
        assert_eq!(trace.roots.len(), 1);
        assert_eq!(trace.roots[0].corroborating_paths, 1);
    }

    #[test]
    fn a_corroborating_path_raises_influence_by_a_quarter_of_itself() {
        // The strong route is worth 1.0. The corroborating route shares no edge
        // with it and is worth a quarter of that, so it adds 0.25 x 0.25.
        let graph = build_graph(vec![
            edge("origin", "left", 5 * SOL, LAUNCH - 2 * HOUR, "s1"),
            edge("left", "w", 5 * SOL, LAUNCH, "s2"),
            edge("origin", "right", 5 * SOL, LAUNCH - 2 * HOUR, "s3"),
            edge("right", "w", DEFAULT_THETA_LAMPORTS / 4, LAUNCH, "s4"),
        ]);
        let trace = trace_of(&graph, "w");
        assert_eq!(trace.roots[0].corroborating_paths, 1);
        // Both routes are two hops, so §Y.2's term multiplies both and kappa
        // still adds a quarter of the weaker one: `h + 0.25 x (0.25 x h)`.
        // Under version 1 this saturated at one and the test could only read
        // the clamp; the hop term takes it off the ceiling, so the discount is
        // now visible in the number.
        let policy = TracePolicy::default();
        let h = hop_decay(2, &policy);
        assert_eq!(
            trace.roots[0].influence_micros,
            h.saturating_add(
                Fixed::from_micros(250_000)
                    .saturating_mul(h)
                    .scale_bps(DEFAULT_KAPPA_BPS)
            )
            .to_micros()
        );

        // The same shape with the strong route at half, which was the only
        // case version 1 could state without hitting the clamp.
        let graph = build_graph(vec![
            edge("origin", "left", 5 * SOL, LAUNCH - 2 * HOUR, "s1"),
            edge("left", "w", DEFAULT_THETA_LAMPORTS / 2, LAUNCH, "s2"),
            edge("origin", "right", 5 * SOL, LAUNCH - 2 * HOUR, "s3"),
            edge("right", "w", DEFAULT_THETA_LAMPORTS / 4, LAUNCH, "s4"),
        ]);
        let trace = trace_of(&graph, "w");
        assert_eq!(
            trace.roots[0].influence_micros,
            Fixed::from_micros(500_000)
                .saturating_mul(h)
                .saturating_add(
                    Fixed::from_micros(250_000)
                        .saturating_mul(h)
                        .scale_bps(DEFAULT_KAPPA_BPS)
                )
                .to_micros()
        );
    }

    #[test]
    fn paths_sharing_an_edge_do_not_corroborate_each_other() {
        // A diamond whose two routes both run through the same final hop: the
        // same money, counted once.
        let graph = build_graph(vec![
            edge("origin", "left", 5 * SOL, LAUNCH - 3 * HOUR, "s1"),
            edge("origin", "right", 5 * SOL, LAUNCH - 3 * HOUR, "s2"),
            edge("left", "hop", 5 * SOL, LAUNCH - 2 * HOUR, "s3"),
            edge("right", "hop", 5 * SOL, LAUNCH - 2 * HOUR, "s4"),
            edge("hop", "w", 5 * SOL, LAUNCH - HOUR, "s5"),
        ]);
        let trace = trace_of(&graph, "w");
        assert_eq!(trace.roots.len(), 1);
        assert_eq!(trace.roots[0].root, "origin");
        // Both routes end in edge s5, so neither corroborates the other.
        assert_eq!(trace.roots[0].corroborating_paths, 0);
    }

    #[test]
    fn the_posterior_normalises_across_competing_roots() {
        // Two roots, identical in every factor, so each takes half.
        let graph = build_graph(vec![
            edge("origin-a", "w", 5 * SOL, LAUNCH - HOUR, "s1"),
            edge("origin-b", "w", 5 * SOL, LAUNCH - HOUR, "s2"),
        ]);
        let trace = trace_of(&graph, "w");
        assert_eq!(trace.roots.len(), 2);
        assert_eq!(trace.roots[0].posterior_micros, 500_000);
        assert_eq!(trace.roots[1].posterior_micros, 500_000);
    }

    #[test]
    fn a_stronger_root_takes_the_larger_share() {
        // One funder moved 10 SOL, the other 0.025 — a quarter of theta.
        let graph = build_graph(vec![
            edge("big", "w", 10 * SOL, LAUNCH, "s1"),
            edge("small", "w", DEFAULT_THETA_LAMPORTS / 4, LAUNCH, "s2"),
        ]);
        let trace = trace_of(&graph, "w");
        assert_eq!(trace.parent.as_deref(), Some("big"));
        // 1.0 and 0.25 normalise to 0.8 and 0.2.
        assert_eq!(trace.roots[0].posterior_micros, 800_000);
        assert_eq!(trace.roots[1].posterior_micros, 200_000);
    }

    // -----------------------------------------------------------------
    // Budgets
    // -----------------------------------------------------------------

    /// A single chain `origin -> h(n-1) -> ... -> h1 -> w`, `hops` edges long,
    /// each one hop closer to the launch than the last so that §3.2's `dt_hop`
    /// and the causality rule are both satisfied all the way down.
    ///
    /// Minutes rather than hours between hops: at twenty-four hops an hourly
    /// spacing would put the far end outside the 72-hour lookback, and the test
    /// would be reading `W_lookback` while claiming to read the depth budget.
    fn chain_of(hops: u32) -> FundingGraph {
        let mut edges = Vec::new();
        for index in 0..hops {
            let from = if index == 0 {
                "origin".to_string()
            } else {
                format!("h{:02}", hops - index)
            };
            let to = if index + 1 == hops {
                "w".to_string()
            } else {
                format!("h{:02}", hops - index - 1)
            };
            edges.push(edge(
                &from,
                &to,
                5 * SOL,
                LAUNCH - i64::from(hops - index) * MINUTE,
                &format!("s{index:03}"),
            ));
        }
        build_graph(edges)
    }

    #[test]
    fn the_depth_cap_truncates_and_does_not_invent_a_root() {
        // A five-hop chain under a depth budget of four. The intermediate hop
        // the walk stops on must not be reported as an origin.
        let graph = chain_of(5);
        let budget = TraceBudget {
            depth: 4,
            ..TraceBudget::default()
        };
        let trace = trace_wallet(&graph, "w", LAUNCH, &TracePolicy::default(), &budget);

        assert!(trace.truncated);
        assert_eq!(trace.truncation, Some(Truncation::Depth));
        // Nothing was reported at all: no path completed inside the budget, so
        // the honest answer is UNKNOWN rather than "h04 funded this".
        assert_eq!(trace.parent, None);
        assert!(!trace.may_clear());
    }

    #[test]
    fn the_default_budget_walks_a_twenty_four_hop_lineage_to_its_origin() {
        // The deliverable, stated as the thing an operator would notice: a
        // laundering chain built to sit past the old four-hop cap now resolves
        // to the address that paid for it, under the budget the build ships.
        let graph = chain_of(DEFAULT_DEPTH);
        let trace = trace_of(&graph, "w");

        assert_eq!(trace.parent.as_deref(), Some("origin"));
        assert_eq!(trace.roots[0].hops, DEFAULT_DEPTH);
        assert!(!trace.truncated, "the chain fits inside every budget");
        assert!(trace.may_clear());
        // Root first, in the direction the money moved.
        assert_eq!(trace.roots[0].best_path.len() as u32, DEFAULT_DEPTH);
        assert_eq!(trace.roots[0].best_path[0].from, "origin");
        assert_eq!(trace.roots[0].best_path[DEFAULT_DEPTH as usize - 1].to, "w");
    }

    #[test]
    fn one_hop_past_the_budget_is_truncated_rather_than_answered() {
        // The cap moved; it did not stop existing. A chain one hop longer than
        // the budget still reports UNKNOWN and says why, which is what keeps
        // `may_clear` honest at the new depth as it was at the old one.
        let graph = chain_of(DEFAULT_DEPTH + 1);
        let trace = trace_of(&graph, "w");

        assert_eq!(trace.parent, None);
        assert!(trace.truncated);
        assert_eq!(trace.truncation, Some(Truncation::Depth));
        assert!(!trace.may_clear());
    }

    #[test]
    fn distance_costs_a_path_exactly_one_halving_per_hop_half_life() {
        // §Y.2's term, read through the traversal: the same funding, the same
        // amount, the same moment, four hops further away is worth half.
        let policy = TracePolicy::default();
        let near = trace_of(&chain_of(1), "w").roots[0].influence_micros;
        let far = trace_of(&chain_of(1 + policy.hop_half_life), "w").roots[0].influence_micros;

        // Within a millionth: the two walks compose a different number of
        // factors at 10^-18 before either is narrowed to micros.
        assert!(
            far.abs_diff(near / 2) <= 1,
            "four hops of distance must halve the claim: {near} -> {far}"
        );

        // And the twenty-fourth hop is worth about two per cent of the first,
        // which is the number that makes a deep walk safe to publish: present,
        // visible, and unable to carry a launch on its own.
        let deepest = trace_of(&chain_of(DEFAULT_DEPTH), "w").roots[0].influence_micros;
        assert!(
            (18_000..=20_000).contains(&deepest),
            "the deepest hop should land near 0.019, and it is {deepest}"
        );
    }

    #[test]
    fn a_long_chain_never_outweighs_a_short_one_from_the_same_origin() {
        // The property the deepening had to preserve, stated directly rather
        // than through the arithmetic: however many unambiguous hops an
        // attacker inserts, the laundered trail cannot claim a wallet more
        // strongly than paying it directly would have.
        let mut previous = u64::MAX;
        for hops in 1..=DEFAULT_DEPTH {
            let influence = trace_of(&chain_of(hops), "w").roots[0].influence_micros;
            assert!(
                influence <= previous,
                "{hops} hops scored {influence}, above the {previous} of one hop fewer"
            );
            previous = influence;
        }
    }

    #[test]
    fn a_deeper_budget_reaches_what_a_shallower_one_could_not() {
        let graph = chain_of(5);
        let shallow = TraceBudget {
            depth: 4,
            ..TraceBudget::default()
        };
        let trace = trace_wallet(&graph, "w", LAUNCH, &TracePolicy::default(), &shallow);
        assert_eq!(trace.parent, None);

        let budget = TraceBudget {
            depth: 5,
            ..TraceBudget::default()
        };
        let trace = trace_wallet(&graph, "w", LAUNCH, &TracePolicy::default(), &budget);
        assert_eq!(trace.parent.as_deref(), Some("origin"));
        assert!(!trace.truncated);
    }

    #[test]
    fn a_graph_that_is_wide_and_deep_stops_at_the_budget_rather_than_at_the_depth() {
        // The case raising the depth actually stresses. Twenty-four levels of
        // eight funders each is 8^24 paths, so what has to hold is that the
        // node and edge budgets bind first — they were always the real bound,
        // and the claim made when the depth went from four to twenty-four is
        // that this is still true. If it were not, the deeper budget would have
        // turned a bounded traversal into an unbounded one.
        let mut edges = Vec::new();
        for level in 0..24u32 {
            for parent in 0..8u32 {
                for child in 0..8u32 {
                    let from = format!("l{:02}n{parent}", level + 1);
                    let to = if level == 0 {
                        "w".to_string()
                    } else {
                        format!("l{:02}n{child}", level)
                    };
                    edges.push(edge(
                        &from,
                        &to,
                        u64::from(parent + 1) * SOL,
                        LAUNCH - i64::from(level + 1) * MINUTE,
                        &format!("s{level:02}-{parent}-{child}"),
                    ));
                }
            }
        }
        let graph = build_graph(edges);

        let budget = TraceBudget::default();
        let trace = trace_wallet(&graph, "w", LAUNCH, &TracePolicy::default(), &budget);

        assert!(trace.truncated, "a graph this size cannot be walked whole");
        assert!(
            matches!(
                trace.truncation,
                Some(Truncation::Edges) | Some(Truncation::Nodes)
            ),
            "a budget other than depth has to bind first, and {:?} did",
            trace.truncation
        );
        assert!(
            trace.edges_walked as usize <= budget.edges,
            "{} edges walked against a budget of {}",
            trace.edges_walked,
            budget.edges
        );
        assert!(
            trace.nodes_visited as usize <= budget.nodes,
            "{} nodes visited against a budget of {}",
            trace.nodes_visited,
            budget.nodes
        );
        // Truncated, so whatever it found is a lower bound and clears nothing.
        assert!(!trace.may_clear());
    }

    #[test]
    fn the_fanout_cap_keeps_the_largest_edges_and_says_so() {
        let mut edges = Vec::new();
        for index in 0..20u64 {
            edges.push(edge(
                &format!("funder{index:02}"),
                "w",
                (index + 1) * SOL,
                LAUNCH - HOUR,
                &format!("s{index:02}"),
            ));
        }
        let graph = build_graph(edges);
        let budget = TraceBudget {
            fanout: 3,
            ..TraceBudget::default()
        };
        let trace = trace_wallet(&graph, "w", LAUNCH, &TracePolicy::default(), &budget);

        assert!(trace.truncated);
        assert_eq!(trace.truncation, Some(Truncation::Fanout));
        assert_eq!(trace.roots.len(), 3);
        let kept: BTreeSet<&str> = trace.roots.iter().map(|r| r.root.as_str()).collect();
        assert_eq!(
            kept,
            ["funder17", "funder18", "funder19"]
                .into_iter()
                .collect::<BTreeSet<&str>>(),
            "the fan-out cap must keep the largest by amount"
        );
    }

    #[test]
    fn the_edge_budget_stops_the_walk_and_keeps_what_it_paid_for() {
        let mut edges = Vec::new();
        for index in 0..20u64 {
            edges.push(edge(
                &format!("funder{index:02}"),
                "w",
                (index + 1) * SOL,
                LAUNCH - HOUR,
                &format!("s{index:02}"),
            ));
        }
        let graph = build_graph(edges);
        let budget = TraceBudget {
            edges: 4,
            ..TraceBudget::default()
        };
        let trace = trace_wallet(&graph, "w", LAUNCH, &TracePolicy::default(), &budget);

        assert!(trace.truncated);
        assert_eq!(trace.truncation, Some(Truncation::Edges));
        assert_eq!(trace.edges_walked, 4);
        // The four edges that were walked still produced their four roots.
        assert_eq!(trace.roots.len(), 4);
        assert!(!trace.may_clear());
    }

    #[test]
    fn the_node_budget_stops_the_walk() {
        let mut edges = Vec::new();
        for index in 0..20u64 {
            edges.push(edge(
                &format!("funder{index:02}"),
                "w",
                (index + 1) * SOL,
                LAUNCH - HOUR,
                &format!("s{index:02}"),
            ));
        }
        let graph = build_graph(edges);
        let budget = TraceBudget {
            nodes: 3,
            ..TraceBudget::default()
        };
        let trace = trace_wallet(&graph, "w", LAUNCH, &TracePolicy::default(), &budget);

        assert!(trace.truncated);
        assert_eq!(trace.truncation, Some(Truncation::Nodes));
        // The wallet plus the two funders that fitted.
        assert_eq!(trace.nodes_visited, 3);
        assert_eq!(trace.roots.len(), 2);
    }

    #[test]
    fn a_truncated_trace_may_block_but_never_clear() {
        // §14's P14, asserted directly on the predicate every caller uses.
        let graph = build_graph(vec![edge("origin", "w", 5 * SOL, LAUNCH, "s1")]);
        let clean = trace_of(&graph, "w");
        assert!(clean.is_resolved() && clean.may_clear());

        // A wallet with two funders under a depth budget of one: the first is
        // an origin in its own right and resolves, the second has funding
        // behind it that the cap cuts off. So the trace is resolved *and*
        // bound — the case where the asymmetry actually bites, because there
        // is a number here and it still may not be used to clear.
        let budget = TraceBudget {
            depth: 1,
            ..TraceBudget::default()
        };
        let graph = build_graph(vec![
            edge("shallow-origin", "w", 5 * SOL, LAUNCH, "s1"),
            edge("deep-hop", "w", 5 * SOL, LAUNCH, "s2"),
            edge("deep-origin", "deep-hop", 5 * SOL, LAUNCH - HOUR, "s3"),
        ]);
        let bound = trace_wallet(&graph, "w", LAUNCH, &TracePolicy::default(), &budget);
        assert_eq!(bound.parent.as_deref(), Some("shallow-origin"));
        assert!(bound.is_resolved(), "it still found a funder");
        assert_eq!(bound.truncation, Some(Truncation::Depth));
        assert!(!bound.may_clear(), "a lower bound may not clear an entry");
    }

    // -----------------------------------------------------------------
    // Determinism
    // -----------------------------------------------------------------

    #[test]
    fn shuffling_the_input_does_not_change_the_answer() {
        // §15's P9 in the small: two runs over one fixture, reshuffled, must
        // produce identical rows.
        let base = vec![
            edge("origin", "left", 5 * SOL, LAUNCH - 3 * HOUR, "s1"),
            edge("origin", "right", 3 * SOL, LAUNCH - 3 * HOUR, "s2"),
            edge("left", "w", 4 * SOL, LAUNCH - HOUR, "s3"),
            edge("right", "w", 2 * SOL, LAUNCH - HOUR, "s4"),
            edge("other", "w", 6 * SOL, LAUNCH - 2 * HOUR, "s5"),
        ];
        let forward = trace_of(&build_graph(base.clone()), "w");

        let mut reversed = base.clone();
        reversed.reverse();
        assert_eq!(trace_of(&build_graph(reversed), "w"), forward);

        // And a rotation, which is a different permutation again.
        let mut rotated = base;
        rotated.rotate_left(2);
        assert_eq!(trace_of(&build_graph(rotated), "w"), forward);
    }

    #[test]
    fn ties_on_every_factor_still_have_exactly_one_order() {
        // Two funders identical in amount, time and confidence. Only the
        // address can break it, and it must break it the same way every run.
        let graph = build_graph(vec![
            edge("bbb", "w", 5 * SOL, LAUNCH - HOUR, "s2"),
            edge("aaa", "w", 5 * SOL, LAUNCH - HOUR, "s1"),
        ]);
        let trace = trace_of(&graph, "w");
        assert_eq!(trace.roots[0].root, "aaa");
        assert_eq!(trace.roots[1].root, "bbb");
        assert_eq!(trace.parent.as_deref(), Some("aaa"));
    }

    #[test]
    fn tracing_many_wallets_deduplicates_and_sorts() {
        let graph = build_graph(vec![
            edge("origin", "w1", 5 * SOL, LAUNCH - HOUR, "s1"),
            edge("origin", "w2", 5 * SOL, LAUNCH - HOUR, "s2"),
        ]);
        let traces = trace_wallets(
            &graph,
            &["w2".to_string(), "w1".to_string(), "w2".to_string()],
            LAUNCH,
            &TracePolicy::default(),
            &TraceBudget::default(),
        );
        let wallets: Vec<&str> = traces.iter().map(|t| t.wallet.as_str()).collect();
        assert_eq!(wallets, vec!["w1", "w2"]);
    }

    // -----------------------------------------------------------------
    // Funding concentration
    // -----------------------------------------------------------------

    #[test]
    fn funding_concentration_finds_the_shared_root() {
        let graph = build_graph(vec![
            edge("operator", "w1", 5 * SOL, LAUNCH - HOUR, "s1"),
            edge("operator", "w2", 5 * SOL, LAUNCH - HOUR, "s2"),
            edge("operator", "w3", 5 * SOL, LAUNCH - HOUR, "s3"),
        ]);
        let traces: Vec<WalletTrace> = ["w1", "w2", "w3"]
            .iter()
            .map(|wallet| trace_of(&graph, wallet))
            .collect();
        let weighted: Vec<(&WalletTrace, u64)> = traces.iter().map(|trace| (trace, SOL)).collect();

        let concentration = funding_concentration(&weighted).expect("a shared root");
        assert_eq!(concentration.root, "operator");
        assert_eq!(concentration.fund_micros, 1_000_000);
        assert_eq!(concentration.unattributed_weight, 0);
        assert!(!concentration.truncated);
    }

    #[test]
    fn funding_concentration_weights_by_volume_not_by_wallet_count() {
        // §3.5: forty dust wallets behind one root matter less than two large
        // ones. Here one big wallet points at `big` and three dust wallets point
        // at `small`; by count `small` would win, by volume `big` does.
        let graph = build_graph(vec![
            edge("big", "whale", 50 * SOL, LAUNCH - HOUR, "s1"),
            edge("small", "d1", 5 * SOL, LAUNCH - HOUR, "s2"),
            edge("small", "d2", 5 * SOL, LAUNCH - HOUR, "s3"),
            edge("small", "d3", 5 * SOL, LAUNCH - HOUR, "s4"),
        ]);
        let traces: Vec<WalletTrace> = ["whale", "d1", "d2", "d3"]
            .iter()
            .map(|wallet| trace_of(&graph, wallet))
            .collect();
        let weights = [100 * SOL, SOL / 100, SOL / 100, SOL / 100];
        let weighted: Vec<(&WalletTrace, u64)> =
            traces.iter().zip(weights.iter().copied()).collect();

        let concentration = funding_concentration(&weighted).expect("a root");
        assert_eq!(concentration.root, "big");
    }

    #[test]
    fn an_unknown_parent_lowers_the_share_rather_than_being_assumed_independent() {
        // Two of four wallets resolve to one root; the other two are UNKNOWN.
        // The denominator is the whole weight, so the answer is a half — a
        // lower bound, which may block and may not clear.
        let graph = build_graph(vec![
            edge("operator", "w1", 5 * SOL, LAUNCH, "s1"),
            edge("operator", "w2", 5 * SOL, LAUNCH, "s2"),
        ]);
        let traces: Vec<WalletTrace> = ["w1", "w2", "lonely1", "lonely2"]
            .iter()
            .map(|wallet| trace_of(&graph, wallet))
            .collect();
        let weighted: Vec<(&WalletTrace, u64)> = traces.iter().map(|trace| (trace, SOL)).collect();

        let concentration = funding_concentration(&weighted).expect("a root");
        assert_eq!(concentration.fund_micros, 500_000);
        assert_eq!(concentration.attributed_weight, 2 * SOL);
        assert_eq!(concentration.unattributed_weight, 2 * SOL);
    }

    #[test]
    fn funding_concentration_is_unknown_rather_than_zero_when_nothing_resolved() {
        let graph = build_graph(vec![edge("w1", "elsewhere", SOL, LAUNCH - HOUR, "s1")]);
        let traces = [trace_of(&graph, "w1")];
        let weighted: Vec<(&WalletTrace, u64)> = traces.iter().map(|trace| (trace, SOL)).collect();
        assert_eq!(funding_concentration(&weighted), None);

        // And with no weight at all there is nothing to take a share of.
        let zero: Vec<(&WalletTrace, u64)> = traces.iter().map(|trace| (trace, 0)).collect();
        assert_eq!(funding_concentration(&zero), None);
        assert_eq!(funding_concentration(&[]), None);
    }

    #[test]
    fn a_truncated_trace_marks_the_concentration_it_feeds() {
        let graph = build_graph(vec![
            edge("shallow-origin", "w", 5 * SOL, LAUNCH - HOUR, "s1"),
            edge("deep-hop", "w", 5 * SOL, LAUNCH - HOUR, "s2"),
            edge("deep-origin", "deep-hop", 5 * SOL, LAUNCH - 2 * HOUR, "s3"),
        ]);
        let budget = TraceBudget {
            depth: 1,
            ..TraceBudget::default()
        };
        let traces = [trace_wallet(
            &graph,
            "w",
            LAUNCH,
            &TracePolicy::default(),
            &budget,
        )];
        let weighted: Vec<(&WalletTrace, u64)> = traces.iter().map(|trace| (trace, SOL)).collect();
        let concentration = funding_concentration(&weighted).expect("a root");
        assert!(
            concentration.truncated,
            "the bound has to travel with the number"
        );
    }
}
