//! Is this launch one person wearing several wallets, and is that worth trading.
//!
//! Two questions, deliberately kept apart. [`analyse_launch`] answers the first
//! and produces evidence; [`syndicate_gate`] answers the second and produces a
//! verdict with a reason attached. The thresholds live only in the second, so
//! the analyser can be re-run over a recorded corpus without the entry rule's
//! current tuning leaking into what it reports.
//!
//! Three things give a script away, and they are independent of each other:
//!
//!   1. **Size.** People pick amounts that mean something to them; a script
//!      picks one amount and repeats it.
//!   2. **Timing.** Separate people cannot land in the same slot. Addresses that
//!      all execute inside the same fraction of a second were sent together.
//!   3. **Money in.** Addresses funded from the same place a hop or two back are
//!      one wallet with extra steps. This is the only one of the three that is
//!      hard evidence rather than inference, and it is the one the recording
//!      usually does not have.
//!
//! The confidence number adds up what each of those is worth and the weights sum
//! to more than one on purpose: a missing signal then costs nothing. Almost
//! every live launch arrives without a funding graph, and a denominator that
//! included it would cap every one of them below the entry threshold — a
//! detector that can never fire on the data it actually gets.
//!
//! The entry thesis is not that coordinated launches are good. It is that they
//! are *predictable*: a bundle that bought together tends to sell together, and
//! the window between those two things is the whole trade. The prototype's own
//! measurement of that thesis was that it does not pay — 22 trades, no winners,
//! −17.95% — and that verdict is recorded in `docs/archive/Log.md` rather than
//! re-litigated here. What this module owes is a rule that computes the same
//! thing the same way twice, so the next measurement of it means something.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::backtest::{
    beta_micros, beta_threshold_micros, effective_holders_micros, hhi_bps, mul_div_floor,
    sandwich_viable, sync_micros, temporal_influence_micros, top_k_bps, DEFAULT_TAU_SYNC_MS,
    MICROS,
};
use crate::fixed::{normalised_entropy_micros, weighted_entropy_micros};
use crate::replay::{
    sandwich_breakeven_victim_lamports, BPS_DENOMINATOR, DEFAULT_FEE_BPS, LAMPORTS_PER_SOL,
};
use crate::types::SybilClusterMetrics;

// ===========================================================================
// Policy constants
// ===========================================================================

/// The opening window, in milliseconds. Three seconds, the cutoff the watcher
/// froze its opening figures at.
pub const WINDOW_MS: i64 = 3_000;

/// How many opening buyers to consider at most, earliest first.
pub const MAX_WALLETS: usize = 50;

/// A Solana slot, near enough, in milliseconds.
pub const SLOT_MS: i64 = 400;

/// Two positions count as near-identical within this relative distance. 200 bps
/// is wide enough to survive a different priority fee on the same scripted
/// amount, and tight enough that 1.0 and 1.05 SOL stay separate.
pub const SIZE_TOLERANCE_BPS: u64 = 200;

/// Timing gap under which consecutive buys are treated as one bundle.
pub const BUNDLE_MS: i64 = 250;

/// Buys this close together are the same transaction or the same slot.
pub const INSTANT_MS: i64 = 20;

/// A group of repeated positions has to be at least this big to mean anything.
pub const MIN_GROUP: usize = 3;

/// Below this many opening buyers, nothing here can be told apart from noise.
pub const MIN_PARTICIPANTS: usize = 3;

/// The most confidence a launch too thin to read is allowed to report.
pub const THIN_CEILING_MICROS: u64 = 250_000;

/// How much evidence two wallets need before they are called one operator.
pub const LINK_THRESHOLD_MICROS: u64 = 600_000;

/// Out-degree above which a funder is a hub rather than a person.
///
/// An exchange hot wallet pays out to hundreds of thousands of unrelated people.
/// Treating it as a shared funder makes every launch look like a syndicate, and
/// `RISK_AND_SYBIL_SPEC.md` §3.1 makes the same point structurally: these nodes
/// are absorbing, a path may end at one and may never pass through one.
pub const HUB_DEGREE: usize = 25;

/// How many hops back a funding search walks. One is the direct funder; two
/// catches the usual laundering through a fresh intermediate wallet.
pub const FUNDING_DEPTH: u32 = 2;

/// The score a launch has to reach before a primary signal is even consulted.
pub const MIN_CLUSTER_SCORE_MICROS: u64 = 600_000;

/// How many wallets have to be in the coordinated group before it is a group.
///
/// Two addresses doing the same thing is a coincidence with a 50% base rate; the
/// third is what makes it a pattern. The analyser already refuses to tag a
/// bundle below [`MIN_GROUP`], so on the recorded corpus this rejects nothing —
/// it is here so the entry rule does not silently inherit a constant from the
/// analyser, and so the funnel can name the case if it ever happens.
pub const MIN_BUNDLE_WALLETS: usize = 3;

/// How close two positions in the same bundle have to be to count as one script.
///
/// Tighter than [`SIZE_TOLERANCE_BPS`] on purpose, because it asks a different
/// question: not "did somebody repeat a size somewhere in this launch", but "did
/// the wallets that landed together also take the same position". A launch can
/// pass the first and fail the second — the identical-size group and the bundle
/// are often disjoint sets of wallets — and v1 of this rule could not tell those
/// two apart.
pub const BUNDLE_SIZE_TOLERANCE_BPS: u64 = 100;

/// The least a coordinated group can commit and still be worth following.
///
/// The thesis of the whole rule is that the bundle's exit is the trade. A group
/// that put in less than this cannot move the price on the way out either, so
/// there is nothing to be early to.
pub const MIN_BUNDLE_LAMPORTS: u64 = 1_500_000_000;

/// The concentration at which a cluster stops being a group and starts being one
/// wallet with company, in basis points.
///
/// `RISK_AND_SYBIL_SPEC.md` §14 publishes the population this number is: shares
/// of `[0.9, 0.1]` give an HHI of 8 200. Ninety percent of a cluster's opening
/// money in one address, and the rest of the addresses holding a tenth between
/// them, is not five bidders — it is one bidder and four costumes.
pub const RING_HHI_BPS: u16 = 8_200;

/// The other half of the same shape, in millionths.
///
/// §14 again, and the *same* population read a second way: shares of
/// `[0.9, 0.1]` have a normalised Shannon entropy of 0.4690. The two numbers are
/// one launch-shape stated twice, which is why they are calibrated together and
/// why both have to fire — see [`Cluster::ring_finding`] for what each of them
/// catches that the other misses.
pub const RING_ENTROPY_MICROS: u64 = 469_000;

// ===========================================================================
// Vocabulary
// ===========================================================================

/// Every tag the analyser can put on a launch.
///
/// Serialised in the spelling the Node prototype and the recorded corpus use, so
/// a tag read off an old record and a tag produced here are the same string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RiskTag {
    IdenticalSizing,
    NearIdenticalSizing,
    LowSizingEntropy,
    SameInstantBundle,
    SubSecondBundle,
    FirstSlotCrowd,
    SoloDevDominance,
    CreatorBoughtOwn,
    CreatorExit,
    WhaleConcentration,
    SharedFunder,
    NoOpeningBuys,
    InsufficientData,
}

impl RiskTag {
    pub const fn as_str(self) -> &'static str {
        match self {
            RiskTag::IdenticalSizing => "IDENTICAL_SIZING",
            RiskTag::NearIdenticalSizing => "NEAR_IDENTICAL_SIZING",
            RiskTag::LowSizingEntropy => "LOW_SIZING_ENTROPY",
            RiskTag::SameInstantBundle => "SAME_INSTANT_BUNDLE",
            RiskTag::SubSecondBundle => "SUB_SECOND_BUNDLE",
            RiskTag::FirstSlotCrowd => "FIRST_SLOT_CROWD",
            RiskTag::SoloDevDominance => "SOLO_DEV_DOMINANCE",
            RiskTag::CreatorBoughtOwn => "CREATOR_BOUGHT_OWN",
            RiskTag::CreatorExit => "CREATOR_EXIT",
            RiskTag::WhaleConcentration => "WHALE_CONCENTRATION",
            RiskTag::SharedFunder => "SHARED_FUNDER",
            RiskTag::NoOpeningBuys => "NO_OPENING_BUYS",
            RiskTag::InsufficientData => "INSUFFICIENT_DATA",
        }
    }
}

/// The tags that mean "these buyers are one person", as opposed to the ones that
/// only mean "this launch is unusual".
///
/// A confidence score on its own is a blend; requiring one of these means the
/// score has to be coming from coordination rather than from, say, a crowded
/// first slot.
pub const PRIMARY_SIGNALS: [RiskTag; 3] = [
    RiskTag::IdenticalSizing,
    RiskTag::SameInstantBundle,
    RiskTag::CreatorBoughtOwn,
];

/// What one wallet, on its own, was caught doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WalletFlag {
    IdenticalSize,
    NearIdenticalSize,
    SameInstant,
    Bundled,
    SharedFunder,
    SoldInWindow,
}

/// A kind of link between two wallets, and what it is worth.
///
/// Shared funding alone is enough. An identical position alone is enough, unless
/// the amount is a round number a human might also have picked — two people both
/// buying exactly 1 SOL is a coincidence that happens all day, so that link needs
/// corroboration from timing. Being in the same bundle is never enough on its
/// own: a bundle is where every sniper in the world is trying to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkKind {
    SharedFunder,
    IdenticalSize,
    IdenticalRoundSize,
    NearSize,
    SameInstant,
    Bundle,
}

impl LinkKind {
    /// What this link contributes towards [`LINK_THRESHOLD_MICROS`].
    pub const fn weight_micros(self) -> u64 {
        match self {
            LinkKind::SharedFunder => 1_000_000,
            LinkKind::IdenticalSize => 600_000,
            LinkKind::IdenticalRoundSize => 450_000,
            LinkKind::NearSize => 350_000,
            LinkKind::SameInstant => 500_000,
            LinkKind::Bundle => 350_000,
        }
    }
}

/// Every answer the gate can give, worst first, so a caller printing a funnel
/// gets the same order every time.
///
/// The prototype had one more, `unreadable`, for a record its analyser threw on.
/// [`analyse_launch`] cannot throw — a record that would have caused it is not
/// representable in [`LaunchRecord`] — so that reason is gone rather than kept
/// as a variant nothing can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GateReason {
    /// Nobody bought inside the opening window.
    NoOpeningBuys,
    /// Too few buyers to tell coordination from coincidence.
    Thin,
    /// The launch is ordinary.
    LowScore,
    /// Something unusual happened, but nothing that means "one person".
    NoPrimarySignal,
    /// Nobody landed together in a group the analyser would call a bundle.
    NoBundle,
    /// A bundle, but not enough addresses in it to be a pattern.
    ThinBundle,
    /// Enough of them landed together, but they took unrelated positions. That
    /// is a queue at a popular launch, which is what this rule is most often
    /// fooled by — and the reason it is named apart from `ThinBundle`.
    MixedSizing,
    /// A deployer buying its own launch, with nobody else in the group. That is
    /// rug risk, not a syndicate.
    SoloDev,
    /// The group is real and cannot move the price on the way out.
    SmallBundle,
    /// A cluster in the opening is at `RISK_AND_SYBIL_SPEC.md` §14's `[90, 10]`
    /// shape: nearly all of its money in one address and the rest spread thin.
    /// The buyers this rule was going to follow are one buyer.
    CoordinatedRing,
    /// The gate was told to price the entry against the curve and was not given
    /// a curve to price it against.
    NoCurveQuote,
    /// An entry this size on a public route is worth front-running,
    /// `REPLAY_AND_SIMULATION_SPEC.md` §15.2.
    SandwichRisk,
    /// The only reason that trades.
    Accepted,
}

impl GateReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            GateReason::NoOpeningBuys => "no-opening-buys",
            GateReason::Thin => "thin",
            GateReason::LowScore => "low-score",
            GateReason::NoPrimarySignal => "no-primary-signal",
            GateReason::NoBundle => "no-bundle",
            GateReason::ThinBundle => "thin-bundle",
            GateReason::MixedSizing => "mixed-sizing",
            GateReason::SoloDev => "solo-dev",
            GateReason::SmallBundle => "small-bundle",
            GateReason::CoordinatedRing => "coordinated-ring",
            GateReason::NoCurveQuote => "no-curve-quote",
            GateReason::SandwichRisk => "sandwich-risk",
            GateReason::Accepted => "accepted",
        }
    }

    /// Reads back what `as_str` wrote.
    ///
    /// Here because `forensics.rs` stores this vocabulary in a column and has
    /// to turn the text back into the enum. Written against `ALL` rather than
    /// as a second `match`, so a reason added to the enum cannot be one this
    /// forgets — the two lists are the same list.
    pub fn parse(text: &str) -> Option<Self> {
        GateReason::ALL
            .iter()
            .copied()
            .find(|reason| reason.as_str() == text)
    }

    /// Whether this verdict is about our own order rather than about the launch.
    ///
    /// Two questions reach one [`GateVerdict`] — is this launch worth buying,
    /// and can we get on at the size we want — and the second is answered by
    /// the last check in [`syndicate_gate`], after every question about the
    /// buyers has already passed. A caller that cannot separate the two cannot
    /// tell a rule that stopped finding launches from an order that outgrew the
    /// curve it was going to be filled on, so the separation is a function here
    /// rather than a comment at each call site.
    ///
    /// [`GateReason::Accepted`] is about neither and answers false.
    pub const fn is_about_our_order(self) -> bool {
        matches!(self, GateReason::NoCurveQuote | GateReason::SandwichRisk)
    }

    /// Every reason, worst first. A funnel over a corpus prints these in order
    /// and gets the same table shape whatever the corpus contained.
    pub const ALL: [GateReason; 13] = [
        GateReason::NoOpeningBuys,
        GateReason::Thin,
        GateReason::LowScore,
        GateReason::NoPrimarySignal,
        GateReason::NoBundle,
        GateReason::ThinBundle,
        GateReason::MixedSizing,
        GateReason::SoloDev,
        GateReason::SmallBundle,
        GateReason::CoordinatedRing,
        GateReason::NoCurveQuote,
        GateReason::SandwichRisk,
        GateReason::Accepted,
    ];
}

// ===========================================================================
// Inputs
// ===========================================================================

/// One opening buyer, as the watcher records them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpeningBuyer {
    pub wallet: String,
    /// SOL this wallet put in, lamports.
    pub sol_in_lamports: u64,
    /// SOL this wallet took out inside the follow window, lamports. The watcher
    /// records the total, not the moment, so this says whether the wallet sold
    /// and never when.
    pub sol_out_lamports: u64,
    pub tx_count: u32,
    /// Milliseconds after the launch transaction that this wallet was first
    /// seen. Negative values are possible from a provider whose block times
    /// disagree; they are kept rather than clamped, because a buy that appears
    /// to precede the launch is a fact about the recording and hiding it inside
    /// a clamp is how it stops being noticed.
    pub first_seen_ms: i64,
}

/// One funding edge: `from` sent SOL to `to`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FundingEdge {
    pub from: String,
    pub to: String,
    pub lamports: u64,
}

/// One launch, and everything the analyser is allowed to read about it.
///
/// An **empty** `funding` means the caller has no funding data, not that it
/// looked and found none. The distinction carries all the way through: with no
/// edges the funding signal is left out of the confidence sum rather than scored
/// as zero, because a missing test is not a passed test, and a launch analysed
/// before the watcher looked funders up must produce the same number it always
/// did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchRecord {
    pub mint: String,
    pub creator: Option<String>,
    pub buyers: Vec<OpeningBuyer>,
    pub funding: Vec<FundingEdge>,
}

/// What an entry would put on the curve, for `REPLAY_AND_SIMULATION_SPEC.md`
/// §15.2's guard.
///
/// This is the one input here that is not about the launch. Everything else in
/// this module reads what the opening buyers did; this says what *we* are about
/// to do, and it is separate because the answer changes with our own size while
/// the launch does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryQuote {
    /// The gross buy in lamports, before the fee comes off it.
    pub gross_lamports: u64,
    /// The curve's virtual SOL reserve at the moment of the buy. This is the `y`
    /// the threshold scales with: the same order is safe deep in the curve and
    /// worth farming at the launch.
    pub virtual_sol_reserves: u64,
    /// The total proportional fee on the SOL leg, `φ`.
    pub fee_bps: u16,
    /// Whether the order goes out as a private bundle rather than through the
    /// public mempool.
    ///
    /// §15.1 prices a front-run that reads our transaction before it lands, so a
    /// send nobody can read first is outside what the model describes. The check
    /// is still run and still reported on a private send — §15.4's whole use for
    /// this arithmetic is justifying the tip against the adverse selection it
    /// buys out of, and a tip larger than the exposure is a tip buying nothing —
    /// it just does not refuse.
    pub private_bundle: bool,
}

impl EntryQuote {
    /// A buy through the public mempool at the default fee.
    pub const fn public(gross_lamports: u64, virtual_sol_reserves: u64) -> Self {
        EntryQuote {
            gross_lamports,
            virtual_sol_reserves,
            fee_bps: DEFAULT_FEE_BPS,
            private_bundle: false,
        }
    }

    /// The same buy as a private bundle.
    pub const fn private(gross_lamports: u64, virtual_sol_reserves: u64) -> Self {
        EntryQuote {
            private_bundle: true,
            ..EntryQuote::public(gross_lamports, virtual_sol_reserves)
        }
    }
}

/// What the model says about the entry that was quoted.
///
/// Every number here is modelled and none of it is a measurement: nothing was
/// front-run and nothing here says anything about the block market. It is the
/// curve arithmetic and only that.
///
/// Serialised in camel case, unlike its neighbours in this module, and that is
/// the one place the convention has to bend: this is the only type here that is
/// embedded in `daemon::LaunchOutcome`, which is a camel-case document, and a
/// report carrying `quotedLamports` beside `above_threshold` is one nobody can
/// read with a single naming rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandwichCheck {
    pub gross_lamports: u64,
    pub virtual_sol_reserves: u64,
    pub fee_bps: u16,
    pub private_bundle: bool,
    /// `β = (1 - φ) b / y`, in millionths. For reading.
    pub beta_micros: u64,
    /// `φ / (1 - φ)`, in millionths, rounded up. For reading.
    pub beta_threshold_micros: u64,
    /// The threshold as a size instead of a ratio: `b* = φ y / (1 - φ)²`, the
    /// largest gross buy nobody can profitably front-run. Size at or under this
    /// and the guard has nothing to say.
    pub breakeven_lamports: u64,
    /// The exact integer comparison, done without dividing. **This is the field
    /// to believe** — the two millionths above differ from it by exactly the
    /// rounding at the threshold, and §15.2 is explicit that there is no sign to
    /// assert at that point.
    pub above_threshold: bool,
}

impl SandwichCheck {
    /// §15.2 against one quote.
    pub fn of(quote: &EntryQuote) -> Self {
        SandwichCheck {
            gross_lamports: quote.gross_lamports,
            virtual_sol_reserves: quote.virtual_sol_reserves,
            fee_bps: quote.fee_bps,
            private_bundle: quote.private_bundle,
            beta_micros: beta_micros(
                quote.gross_lamports,
                quote.virtual_sol_reserves,
                quote.fee_bps,
            ),
            beta_threshold_micros: beta_threshold_micros(quote.fee_bps),
            breakeven_lamports: sandwich_breakeven_victim_lamports(
                quote.virtual_sol_reserves,
                quote.fee_bps,
            ),
            above_threshold: sandwich_viable(
                quote.gross_lamports,
                quote.virtual_sol_reserves,
                quote.fee_bps,
            ),
        }
    }

    /// Whether this is a refusal, as opposed to something to read.
    ///
    /// A private send is never refused here, for the reason
    /// [`EntryQuote::private_bundle`] gives.
    pub const fn refuses(&self) -> bool {
        self.above_threshold && !self.private_bundle
    }
}

/// How hard the gate leans on the curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandwichGuard {
    /// The model is not consulted and no quote is asked for. This is what a
    /// funnel over a recorded corpus wants: those launches have no curve state
    /// attached and refusing all of them would measure the recording rather than
    /// the rule.
    Off,
    /// Refuse a public entry the model says is worth front-running. A launch
    /// that arrived without a quote is left exactly where the rest of the gate
    /// put it.
    WhenQuoted,
    /// The same, and a launch that arrived without a quote is refused. A curve
    /// nobody read is not a curve that was found to be safe, and this is the
    /// setting a live entry path wants.
    Required,
}

impl SandwichGuard {
    /// Every setting, most permissive first.
    pub const ALL: [SandwichGuard; 3] = [
        SandwichGuard::Off,
        SandwichGuard::WhenQuoted,
        SandwichGuard::Required,
    ];

    /// The name this is serialised and spelled on a command line as. The same
    /// string in both places on purpose: a report a person reads and a flag
    /// they then type should not need a translation table between them.
    pub const fn as_str(self) -> &'static str {
        match self {
            SandwichGuard::Off => "off",
            SandwichGuard::WhenQuoted => "when-quoted",
            SandwichGuard::Required => "required",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        SandwichGuard::ALL.into_iter().find(|g| g.as_str() == text)
    }
}

/// One cluster that came out at §14's `[90, 10]` shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RingFinding {
    pub cluster_id: String,
    pub wallets: u32,
    pub lamports: u64,
    pub share_of_open_bps: u16,
    pub holding_hhi_bps: u16,
    /// `None` only when the entropy half of the test was turned off and the
    /// cluster had nothing measurable to put here.
    pub holding_entropy_micros: Option<u64>,
    /// Whether this ring committed enough to be worth refusing a launch over.
    /// A ring is still reported when it did not, because "the opening had three
    /// of these and none of them was big" is a fact about the launch worth
    /// having in front of a decision.
    pub material: bool,
}

/// How the analyser reads a launch. Every field is policy and versioned with the
/// rest; the arithmetic below knows none of these numbers by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterParams {
    pub window_ms: i64,
    pub max_wallets: usize,
    pub size_tolerance_bps: u64,
    pub bundle_ms: i64,
    pub instant_ms: i64,
    pub slot_ms: i64,
    /// How many slots after the launch still count as "in with the dev".
    pub dev_slots: u32,
    pub min_group: usize,
    pub funding_depth: u32,
    pub hub_degree: usize,
    /// The bandwidth of §3.5's buy-synchrony kernel.
    pub tau_sync_ms: u64,
}

impl Default for ClusterParams {
    fn default() -> Self {
        ClusterParams {
            window_ms: WINDOW_MS,
            max_wallets: MAX_WALLETS,
            size_tolerance_bps: SIZE_TOLERANCE_BPS,
            bundle_ms: BUNDLE_MS,
            instant_ms: INSTANT_MS,
            slot_ms: SLOT_MS,
            dev_slots: 4,
            min_group: MIN_GROUP,
            funding_depth: FUNDING_DEPTH,
            hub_degree: HUB_DEGREE,
            tau_sync_ms: DEFAULT_TAU_SYNC_MS,
        }
    }
}

impl ClusterParams {
    /// How long after the launch a buy still counts as arriving with the dev.
    ///
    /// Saturating because both halves are caller-supplied policy, and a params
    /// struct built with a nonsense slot width should produce a nonsense window
    /// rather than an overflow panic inside a release build that has
    /// `overflow-checks` on.
    pub const fn dev_window_ms(&self) -> i64 {
        self.slot_ms.saturating_mul(self.dev_slots as i64)
    }
}

/// What the entry rule demands of a launch the analyser has already read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateParams {
    pub min_score_micros: u64,
    pub primary_signals: Vec<RiskTag>,
    /// Zero turns the three bundle checks off entirely.
    pub min_bundle_wallets: usize,
    /// `None` turns the size test off: every wallet in the bundle is then one
    /// group, however far apart their positions are. This is how the v1 rule
    /// stays runnable rather than being quoted from an old checkout.
    pub bundle_size_tolerance_bps: Option<u64>,
    /// Zero turns the commitment test off.
    pub min_bundle_lamports: u64,
    pub require_external_bundle: bool,
    /// §2.2: a cluster whose opening money is at least this concentrated has the
    /// ring shape. `None` turns that half of the test off.
    pub ring_min_hhi_bps: Option<u16>,
    /// §2.3: …and whose entropy over the same shares is at most this has it from
    /// the other side. `None` turns that half off, which leaves the index
    /// deciding alone and is the looser setting — see [`Cluster::ring_finding`].
    pub ring_max_entropy_micros: Option<u64>,
    /// The least a ring can be holding before it is worth refusing a launch
    /// over. The same commitment floor `min_bundle_lamports` applies to the
    /// group being followed, for the same reason: a ring too small to move the
    /// price is too small to be the thing that decides this.
    pub ring_min_lamports: u64,
    /// Whether an entry is priced against the curve before it is allowed out.
    pub sandwich_guard: SandwichGuard,
}

impl Default for GateParams {
    fn default() -> Self {
        GateParams {
            min_score_micros: MIN_CLUSTER_SCORE_MICROS,
            primary_signals: PRIMARY_SIGNALS.to_vec(),
            min_bundle_wallets: MIN_BUNDLE_WALLETS,
            bundle_size_tolerance_bps: Some(BUNDLE_SIZE_TOLERANCE_BPS),
            min_bundle_lamports: MIN_BUNDLE_LAMPORTS,
            require_external_bundle: true,
            ring_min_hhi_bps: Some(RING_HHI_BPS),
            ring_max_entropy_micros: Some(RING_ENTROPY_MICROS),
            ring_min_lamports: MIN_BUNDLE_LAMPORTS,
            sandwich_guard: SandwichGuard::WhenQuoted,
        }
    }
}

impl GateParams {
    /// The rule as it stood before the group checks: a score and a primary tag,
    /// and nothing about who the tag fired on.
    ///
    /// Kept runnable rather than deleted because the only honest way to state
    /// what the group checks did is to replay both over the same launches at the
    /// same costs.
    pub fn v1() -> Self {
        GateParams {
            min_bundle_wallets: 0,
            bundle_size_tolerance_bps: None,
            min_bundle_lamports: 0,
            require_external_bundle: false,
            ring_min_hhi_bps: None,
            ring_max_entropy_micros: None,
            ring_min_lamports: 0,
            sandwich_guard: SandwichGuard::Off,
            ..GateParams::default()
        }
    }
}

// ===========================================================================
// The report
// ===========================================================================

/// What the analyser was looking at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub window_ms: i64,
    pub dev_slots: u32,
    /// Buyers inside the window, after the cap.
    pub participants: u32,
    /// Buyers on the record, before the window and the cap.
    pub considered: u32,
    pub sol_in_lamports: u64,
}

/// A run of opening positions close enough in size to be one script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeGroup {
    /// The group's smallest member, which is the amount it is named by.
    pub value_lamports: u64,
    pub wallets: u32,
    /// Every member matches to four decimal places of SOL — the precision the
    /// record keeps, and finer than a human types.
    pub exact: bool,
    /// An amount a person might have typed rather than one a script produced.
    pub round_number: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizingSignal {
    pub entropy_micros: u64,
    /// Only the groups at or above `min_group`.
    pub groups: Vec<SizeGroup>,
    pub repeated_wallets: u32,
    pub largest_group: u32,
    pub score_micros: u64,
}

/// Buys that all landed inside one window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimingBundle {
    pub wallets: u32,
    pub at_ms: i64,
    pub span_ms: i64,
    pub same_instant: bool,
    pub lamports: u64,
    /// In the order they landed. The gate needs both these and the SOL, and
    /// re-deriving the bundling in a second place is how two answers to one
    /// question start disagreeing.
    pub members: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimingSignal {
    /// Only the bundles at or above `min_group`.
    pub bundles: Vec<TimingBundle>,
    pub largest_bundle: u32,
    pub span_ms: Option<i64>,
    pub same_instant: bool,
    /// The largest bundle sits on the launch itself, which is the block every
    /// sniper on the network is trying to be in.
    pub launch_block: bool,
    pub score_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevSignal {
    pub creator_bought: bool,
    pub creator_lamports: u64,
    pub creator_share_bps: u16,
    pub creator_sold: bool,
    pub with_dev: u32,
    pub with_dev_share_bps: u16,
    /// The largest single opening position as a share of the opening money.
    pub concentration_bps: u16,
    pub score_micros: u64,
}

/// One address that paid for more than one opening buyer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedFunderRow {
    pub funder: String,
    /// The fewest hops from any of its wallets back to it.
    pub hops: u32,
    pub wallets: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FundingSignal {
    /// The share of the buyers asked about that share a funder with at least one
    /// other buyer. Not the share of pairs — one funder behind five of twenty
    /// wallets is 25%, which is the sentence a person would say.
    pub overlap_bps: u16,
    pub linked_wallets: u32,
    /// Hubs excluded, loudest first.
    pub funders: Vec<SharedFunderRow>,
    /// Funders that reached several wallets and were set aside as exchanges,
    /// bridges or mixers. Reported rather than silently dropped, because
    /// "everyone here came from an exchange" is itself worth knowing.
    pub hubs_ignored: u32,
    pub score_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Participant {
    pub wallet: String,
    pub sol_in_lamports: u64,
    pub sol_out_lamports: u64,
    pub tx_count: u32,
    pub first_seen_ms: i64,
    pub cluster_id: Option<String>,
    pub flags: Vec<WalletFlag>,
}

/// Two wallets and the evidence that joined them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Relation {
    pub a: String,
    pub b: String,
    pub weight_micros: u64,
    pub kinds: Vec<LinkKind>,
}

/// `RISK_AND_SYBIL_SPEC.md` §2.2 and §2.3 over the opening money.
///
/// The population here is the opening buyers, not the token holders: this says
/// how the money that opened the launch was split, which is the question the
/// entry rule is asking. A holder-population concentration is the same
/// arithmetic on a different slice and belongs to the risk governor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Concentration {
    /// `None` is UNKNOWN and never `Some(0)`: an opening nobody bought has no
    /// concentration, it does not have a concentration of zero.
    pub hhi_bps: Option<u16>,
    pub top1_bps: u16,
    pub top5_bps: u16,
    pub top10_bps: u16,
    /// `10_000 / HHI_bps` in millionths — the number of equal-sized buyers that
    /// would produce this index. Against the raw buyer count it is the dust
    /// detector.
    pub effective_buyers_micros: u64,
    /// §2.3's `H / ln(N)` over the same shares, in millionths. One is every
    /// buyer the same size; zero is one buyer and nothing else.
    ///
    /// `None` below two buyers. §2.3 writes that case as a zero and this column
    /// does not, for the reason the rest of this module gives about zeros: a
    /// zero here reads as "one address took the whole opening", and a launch
    /// with one buyer on the record has not shown that — it has shown one row.
    /// §15's P8 is the rule being followed, not §14's `[1.0]` line being
    /// ignored, and the statistic itself is still zero wherever it is defined.
    pub entropy_micros: Option<u64>,
}

/// A set of opening buyers the analyser joined into one operator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cluster {
    pub id: String,
    pub size: u32,
    pub lamports: u64,
    pub share_of_open_bps: u16,
    pub first_at_ms: i64,
    /// Sorted, so two runs list them the same way.
    pub members: Vec<String>,
    /// The kinds of evidence that ended up inside this cluster.
    pub reasons: Vec<LinkKind>,
    /// §2.2 over the members' own opening positions: whether this is one funder
    /// with forty empty puppets, or forty wallets that each hold something.
    pub holding_hhi_bps: Option<u16>,
    /// §2.3's normalised entropy over those same positions, in millionths. The
    /// second reading of the number above, and the one that notices a tail the
    /// index cannot feel — see [`Cluster::ring_finding`].
    ///
    /// `None` below two members with money in, which a cluster cannot be: a
    /// cluster is built out of links between buyers, and a buyer with nothing in
    /// never enters the window.
    pub holding_entropy_micros: Option<u64>,
    /// §3.5's kernel over the members' first-buy times. `None` below two
    /// members, which cannot happen for a cluster and is checked anyway.
    pub sync_micros: Option<u64>,
    pub sync_truncated: bool,
    /// The largest share of this cluster's opening money that traces back to one
    /// non-hub funder within `funding_depth` hops.
    ///
    /// Named for what it is. §3.3's parent posterior weighs each path by
    /// confidence, age and bottleneck flow and discounts corroborating paths;
    /// this is plain reachability, which is an **upper** bound on that posterior
    /// and therefore the direction that must not be used to clear an entry on
    /// its own. `None` when the record carried no funding edges.
    pub funder_share_bps: Option<u16>,
    /// §3.5's `sqrt(sync × fund)`. `None` whenever `funder_share_bps` is, because
    /// the geometric mean of an unknown is not zero — and a zero in this column
    /// reads as "these wallets are unrelated", which is the opposite of what was
    /// learned.
    pub temporal_influence_micros: Option<u64>,
    /// §5.1 over the funding edges that run between members. `None` below two
    /// such edges: that is an unmeasurable cluster, not a low-entropy one.
    pub interaction_entropy_micros: Option<u64>,
}

impl Cluster {
    /// Is this cluster a coordinated ring, at the thresholds the caller brought.
    ///
    /// `RISK_AND_SYBIL_SPEC.md` §14 publishes one population, `[0.9, 0.1]`, and
    /// two numbers for it: an HHI of 8 200 and a normalised entropy of 0.4690.
    /// They are the same fact measured with different instruments, and each one
    /// is deaf where the other hears:
    ///
    /// The **index** is a sum of squares, so it is almost entirely the largest
    /// holder. Split the tenth that is not the whale across eight addresses
    /// instead of one and the index barely moves — which is exactly the edit an
    /// operator makes to get under a concentration limit.
    ///
    /// The **entropy** is a sum of logs, so it counts the tail. Eight addresses
    /// holding a fortieth each raise it, and a whale with two friends does not
    /// pass it. It is the one that says whether the small holders are a crowd or
    /// a costume.
    ///
    /// So both have to fire. A cluster that fails either one is a cluster where
    /// one of the two instruments found a real spread, and a rule that refuses a
    /// launch is a rule that should have to clear both.
    ///
    /// `lamports` is checked against `min_lamports` for materiality only; the
    /// finding is returned either way with [`RingFinding::material`] saying which.
    /// A threshold of `None` turns that half off. An entropy this module could
    /// not measure does not corroborate and does not substitute for the check:
    /// this test only ever refuses, so an unknown leaves the launch exactly
    /// where the rest of the gate put it.
    pub fn ring_finding(
        &self,
        min_hhi_bps: Option<u16>,
        max_entropy_micros: Option<u64>,
        min_lamports: u64,
    ) -> Option<RingFinding> {
        if min_hhi_bps.is_none() && max_entropy_micros.is_none() {
            return None;
        }
        let hhi = self.holding_hhi_bps?;
        if min_hhi_bps.is_some_and(|floor| hhi < floor) {
            return None;
        }
        if let Some(ceiling) = max_entropy_micros {
            match self.holding_entropy_micros {
                Some(measured) if measured <= ceiling => {}
                _ => return None,
            }
        }
        Some(RingFinding {
            cluster_id: self.id.clone(),
            wallets: self.size,
            lamports: self.lamports,
            share_of_open_bps: self.share_of_open_bps,
            holding_hhi_bps: hhi,
            holding_entropy_micros: self.holding_entropy_micros,
            material: self.lamports >= min_lamports,
        })
    }

    /// The stored row, once a caller has run the eigen-solver this module does
    /// not have.
    ///
    /// `None` when any input is UNKNOWN, which is `RISK_AND_SYBIL_SPEC.md`
    /// §3.5's rule: the cluster goes to the unresolved queue and the candidate
    /// is UNKNOWN to the gate. Writing the row with a zero in the missing column
    /// would make an unmeasured cluster indistinguishable from a measured clean
    /// one.
    pub fn metrics_with(&self, spectral_separation_micros: u64) -> Option<SybilClusterMetrics> {
        Some(SybilClusterMetrics::new(
            self.size,
            self.holding_hhi_bps?,
            self.temporal_influence_micros?,
            spectral_separation_micros,
            self.interaction_entropy_micros?,
        ))
    }
}

/// The whole read on one launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterReport {
    pub mint: String,
    pub creator: Option<String>,
    pub window: Window,
    pub confidence_micros: u64,
    /// Too few opening buyers to tell coordination from coincidence.
    pub thin: bool,
    pub risk_tags: Vec<RiskTag>,
    pub reasons: Vec<String>,
    pub sizing: SizingSignal,
    pub timing: TimingSignal,
    pub dev: DevSignal,
    /// `None` when the record carried no funding edges. Left out of the
    /// confidence sum rather than scored as zero: a missing test is not a passed
    /// test.
    pub funding: Option<FundingSignal>,
    pub clusters: Vec<Cluster>,
    pub participants: Vec<Participant>,
    pub relations: Vec<Relation>,
    /// How many wallets sit inside some cluster.
    pub syndicate_size: u32,
    pub largest_cluster: u32,
    /// The opening money inside clusters.
    pub exposure_lamports: u64,
    pub concentration: Concentration,
}

impl ClusterReport {
    pub fn has_tag(&self, tag: RiskTag) -> bool {
        self.risk_tags.contains(&tag)
    }

    /// The clustered share of the opening money, in basis points. The number
    /// that matters for trading: a syndicate holding 8% of a launch can be
    /// ignored, the same syndicate holding 80% is the entire order book and can
    /// leave in one transaction.
    pub fn exposure_bps(&self) -> u16 {
        share_bps(self.exposure_lamports, self.window.sol_in_lamports)
    }

    /// Every cluster in the opening that came out at §14's `[90, 10]` shape, in
    /// report order, which is cluster order, which is deterministic.
    ///
    /// The scan is over every cluster rather than only the one the gate is about
    /// to follow. A ring anywhere in the opening is a reason to distrust the
    /// whole read: the buyer count, the diversity and the exposure this rule
    /// leans on are all counts of addresses, and a ring is what an operator
    /// builds when it wants those counts to say something they should not.
    pub fn rings(
        &self,
        min_hhi_bps: Option<u16>,
        max_entropy_micros: Option<u64>,
        min_lamports: u64,
    ) -> Vec<RingFinding> {
        self.clusters
            .iter()
            .filter_map(|cluster| {
                cluster.ring_finding(min_hhi_bps, max_entropy_micros, min_lamports)
            })
            .collect()
    }

    /// How many of the opening buyers are actually different people.
    ///
    /// Each cluster collapses to one buyer — the operator is a real bidder, just
    /// one of them rather than six. This is the count every buyer threshold in
    /// the system was quietly assuming it already had.
    pub fn organic_buyers(&self) -> u32 {
        self.window
            .participants
            .saturating_sub(self.syndicate_size)
            .saturating_add(self.clusters.len() as u32)
    }
}

// ===========================================================================
// The analyser
// ===========================================================================

/// The whole read on one launch. Pure: same record and same params, same answer,
/// nothing read from disk and nothing written.
pub fn analyse_launch(record: &LaunchRecord, params: &ClusterParams) -> ClusterReport {
    // The opening window, earliest first, capped. A buy outside the window or
    // with nothing in it is not a buyer.
    let mut early: Vec<&OpeningBuyer> = record
        .buyers
        .iter()
        .filter(|b| b.first_seen_ms <= params.window_ms && b.sol_in_lamports > 0)
        .collect();
    early.sort_by(|a, b| {
        a.first_seen_ms
            .cmp(&b.first_seen_ms)
            .then_with(|| a.wallet.cmp(&b.wallet))
    });
    early.truncate(params.max_wallets);

    let considered = record.buyers.len() as u32;
    if early.is_empty() {
        return empty_report(record, params, considered);
    }

    let open_lamports = total_lamports(early.iter().map(|b| b.sol_in_lamports));

    // --- 1. Size ---------------------------------------------------------
    let size_groups = group_by_size(&early, params.size_tolerance_bps);
    let repeated: Vec<&RawSizeGroup> = size_groups
        .iter()
        .filter(|g| g.members.len() >= params.min_group)
        .collect();
    let repeated_wallets: usize = repeated.iter().map(|g| g.members.len()).sum();
    let largest_group = repeated.iter().map(|g| g.members.len()).max().unwrap_or(0);
    let any_exact = repeated.iter().any(|g| g.exact);

    // The same partition the entropy is taken over: one grouping, read twice.
    let partition: Vec<usize> = size_groups.iter().map(|g| g.members.len()).collect();
    let entropy_micros = normalised_entropy_micros(&partition, early.len());

    let sizing_score = {
        // Driven by the biggest repeated group rather than by what share of the
        // launch it is. Three addresses on one odd amount among forty buyers is
        // still three addresses run by one person; the share of the launch they
        // hold is a separate question. The entropy term is a small top-up for a
        // launch with no repeats big enough to count but hardly any variety.
        let strength = group_strength_micros(largest_group, params.min_group);
        let quality = size_quality_micros(&repeated);
        let variety = mul_div_floor(
            200_000,
            u128::from(MICROS - entropy_micros),
            u128::from(MICROS),
        );
        (mul_div_floor(
            u128::from(strength),
            u128::from(quality),
            u128::from(MICROS),
        ) + variety)
            .min(u128::from(MICROS)) as u64
    };

    let sizing = SizingSignal {
        entropy_micros,
        groups: repeated
            .iter()
            .map(|g| SizeGroup {
                value_lamports: g.value_lamports,
                wallets: g.members.len() as u32,
                exact: g.exact,
                round_number: g.round_number,
            })
            .collect(),
        repeated_wallets: repeated_wallets as u32,
        largest_group: largest_group as u32,
        score_micros: sizing_score,
    };

    // --- 2. Timing -------------------------------------------------------
    let bundles = find_bundles(&early, params.bundle_ms);
    // First strict maximum wins, and the bundles are in time order, so a tie
    // goes to the earliest. `reduce` rather than `max_by_key`, which documents
    // itself as returning the *last* maximum — the opposite convention, and a
    // silent one-bundle difference on every launch with two bundles of a size.
    let top = bundles
        .iter()
        .reduce(|best, current| {
            if current.len() > best.len() {
                current
            } else {
                best
            }
        })
        .filter(|members| members.len() >= params.min_group);
    let real_bundle = top.map(|members| {
        let span_ms = span_of(&early, members);
        (members, span_ms)
    });
    let same_instant = real_bundle.is_some_and(|(_, span)| span <= params.instant_ms);
    // A bundle sitting on the launch itself is the block every sniper on the
    // network is trying to be in, and the watcher records that whole block at the
    // same hundredth of a second whether or not the buyers know each other. So it
    // counts for half. A bundle that forms a second later, when there is no race
    // left to explain it, counts for all of it.
    let launch_block = real_bundle
        .is_some_and(|(members, _)| early[members[0]].first_seen_ms <= params.instant_ms);

    let timing_score = match real_bundle {
        None => 0,
        Some((members, _)) => {
            // One divide rather than three, so the two discounts round once
            // between them instead of once each.
            let strength = group_strength_micros(members.len(), params.min_group);
            let numerator = u128::from(if same_instant { 10u64 } else { 6 })
                * u128::from(if launch_block { 1u64 } else { 2 });
            mul_div_floor(u128::from(strength), numerator, 20).min(u128::from(MICROS)) as u64
        }
    };

    let timing = TimingSignal {
        bundles: bundles
            .iter()
            .filter(|members| members.len() >= params.min_group)
            .map(|members| {
                let span_ms = span_of(&early, members);
                TimingBundle {
                    wallets: members.len() as u32,
                    at_ms: early[members[0]].first_seen_ms,
                    span_ms,
                    same_instant: span_ms <= params.instant_ms,
                    lamports: total_lamports(members.iter().map(|&i| early[i].sol_in_lamports)),
                    members: members.iter().map(|&i| early[i].wallet.clone()).collect(),
                }
            })
            .collect(),
        largest_bundle: real_bundle.map_or(0, |(members, _)| members.len() as u32),
        span_ms: real_bundle.map(|(_, span)| span),
        same_instant,
        launch_block,
        score_micros: timing_score,
    };

    // --- 3. The dev ------------------------------------------------------
    // Found across every recorded buyer rather than only the windowed ones: a
    // creator that sold is a creator that sold whenever the row was written.
    let creator_row = record
        .creator
        .as_deref()
        .and_then(|creator| record.buyers.iter().find(|b| b.wallet == creator));
    let creator_lamports = creator_row
        .filter(|row| row.first_seen_ms <= params.window_ms)
        .map_or(0, |row| row.sol_in_lamports);
    let creator_sold = creator_row.is_some_and(|row| row.sol_out_lamports > 0);
    let creator_share_bps = share_bps(creator_lamports, open_lamports);
    let with_dev = early
        .iter()
        .filter(|b| b.first_seen_ms <= params.dev_window_ms())
        .count();
    let biggest = early.iter().map(|b| b.sol_in_lamports).max().unwrap_or(0);
    let concentration_bps = share_bps(biggest, open_lamports);

    let dev_score = {
        // Buying your own launch is common and only mildly interesting. Owning
        // half the opening money is the thing that lets one address leave
        // whenever it likes, and selling inside the window is that promise being
        // kept. `min(1, share/0.5) × 0.5` is `min(0.5, share)` written out.
        let bought = if creator_lamports > 0 { 400_000 } else { 0 };
        let share = mul_div_floor(
            u128::from(creator_lamports),
            u128::from(MICROS),
            u128::from(open_lamports),
        )
        .min(500_000) as u64;
        let sold = if creator_sold { 300_000 } else { 0 };
        (bought + share + sold).min(MICROS)
    };

    let dev = DevSignal {
        creator_bought: creator_lamports > 0,
        creator_lamports,
        creator_share_bps,
        creator_sold,
        with_dev: with_dev as u32,
        with_dev_share_bps: share_bps(with_dev as u64, early.len() as u64),
        concentration_bps,
        score_micros: dev_score,
    };

    // --- 4. Money in -----------------------------------------------------
    let addresses: Vec<&str> = early.iter().map(|b| b.wallet.as_str()).collect();
    let traced = (!record.funding.is_empty())
        .then(|| find_shared_funders(&record.funding, &addresses, params));
    let funding = traced.as_ref().map(|t| FundingSignal {
        overlap_bps: t.overlap_bps,
        linked_wallets: t.linked.len() as u32,
        funders: t
            .funders
            .iter()
            .filter(|row| !t.hubs.contains(&row.funder))
            .cloned()
            .collect(),
        hubs_ignored: t.hubs.len() as u32,
        // Any shared funder at all is most of the way to proof, so this starts at
        // a half rather than at nothing and reaches the top once it accounts for
        // half the opening. The difference between two linked wallets and ten is
        // the size of the syndicate, not whether there is one.
        score_micros: if t.linked.is_empty() {
            0
        } else {
            (500_000
                + mul_div_floor(
                    t.linked.len() as u128,
                    u128::from(MICROS),
                    early.len() as u128,
                ) as u64)
                .min(MICROS)
        },
    });

    // --- Clusters --------------------------------------------------------
    let funding_pairs = traced.as_ref().map_or(&[][..], |t| t.pairs.as_slice());
    let clustering = cluster_wallets(&early, &size_groups, &bundles, params, funding_pairs);

    let mut clusters: Vec<Cluster> = Vec::new();
    for (index, raw) in clustering.clusters.iter().enumerate() {
        let members: Vec<&OpeningBuyer> = raw.members.iter().map(|&i| early[i]).collect();
        let lamports = total_lamports(members.iter().map(|m| m.sol_in_lamports));
        let mut names: Vec<String> = members.iter().map(|m| m.wallet.clone()).collect();
        names.sort();

        // §2.2 wants the slice sorted by balance descending, ties by address
        // ascending, before it is handed over.
        let mut balances: Vec<(u64, &str)> = members
            .iter()
            .map(|m| (m.sol_in_lamports, m.wallet.as_str()))
            .collect();
        balances.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        let sorted: Vec<u64> = balances.iter().map(|(v, _)| *v).collect();

        let mut times: Vec<i64> = members.iter().map(|m| m.first_seen_ms).collect();
        times.sort_unstable();
        let synced = sync_micros(&times, params.tau_sync_ms);

        let funder_share_bps = traced
            .as_ref()
            .map(|t| t.cluster_funder_share_bps(&members));
        let temporal = match (synced, funder_share_bps) {
            (Some((sync, _)), Some(fund_bps)) => {
                Some(temporal_influence_micros(sync, u64::from(fund_bps) * 100))
            }
            _ => None,
        };

        let member_set: BTreeSet<&str> = members.iter().map(|m| m.wallet.as_str()).collect();
        let interaction_entropy_micros = internal_entropy(&record.funding, &member_set);

        clusters.push(Cluster {
            id: format!("c{}", index + 1),
            size: members.len() as u32,
            lamports,
            share_of_open_bps: share_bps(lamports, open_lamports),
            first_at_ms: members
                .iter()
                .map(|m| m.first_seen_ms)
                .min()
                .expect("non-empty"),
            members: names,
            reasons: raw.kinds.iter().copied().collect(),
            holding_hhi_bps: hhi_bps(&sorted),
            // The same slice, and deliberately so: §2.2's note that the sort
            // happens once at the boundary is what lets the index and the
            // entropy be two readings of one population rather than two
            // populations that happen to agree.
            holding_entropy_micros: weighted_entropy_micros(&sorted),
            sync_micros: synced.map(|(value, _)| value),
            sync_truncated: synced.is_some_and(|(_, truncated)| truncated),
            funder_share_bps,
            temporal_influence_micros: temporal,
            interaction_entropy_micros,
        });
    }

    let syndicate_size: u32 = clusters.iter().map(|c| c.size).sum();
    let largest_cluster = clusters.iter().map(|c| c.size).max().unwrap_or(0);
    let exposure_lamports = total_lamports(clusters.iter().map(|c| c.lamports));

    // --- Confidence ------------------------------------------------------
    let weighted = |weight: u64, score: u64| {
        mul_div_floor(u128::from(weight), u128::from(score), u128::from(MICROS)) as u64
    };
    let mut confidence_micros = (weighted(500_000, sizing.score_micros)
        + weighted(300_000, timing.score_micros)
        + weighted(200_000, dev.score_micros)
        + funding
            .as_ref()
            .map_or(0, |f| weighted(800_000, f.score_micros)))
    .min(MICROS);

    let thin = early.len() < MIN_PARTICIPANTS;
    if thin {
        confidence_micros = confidence_micros.min(THIN_CEILING_MICROS);
    }

    // --- Tags and plain words --------------------------------------------
    let mut risk_tags: Vec<RiskTag> = Vec::new();
    let mut reasons: Vec<String> = Vec::new();

    if let Some(group) = largest_of(&repeated) {
        risk_tags.push(if any_exact {
            RiskTag::IdenticalSizing
        } else {
            RiskTag::NearIdenticalSizing
        });
        let count = group.members.len();
        reasons.push(format!(
            "{count} wallets bought {} amount ({} SOL) — one operator, not {count} buyers",
            if group.exact {
                "the identical".to_string()
            } else {
                format!(
                    "within {}% of the same",
                    percent_of_bps(params.size_tolerance_bps)
                )
            },
            format_sol(group.value_lamports),
        ));
    }
    if early.len() >= 4 && entropy_micros <= 600_000 {
        risk_tags.push(RiskTag::LowSizingEntropy);
        reasons.push(format!(
            "{} buyers between them used very few distinct sizes",
            early.len()
        ));
    }
    if let Some((members, span)) = real_bundle {
        risk_tags.push(if same_instant {
            RiskTag::SameInstantBundle
        } else {
            RiskTag::SubSecondBundle
        });
        reasons.push(if same_instant {
            format!(
                "{} wallets landed in the same instant — they were sent together",
                members.len()
            )
        } else {
            format!(
                "{} wallets landed inside {}ms of each other",
                members.len(),
                span
            )
        });
    }
    // Strictly a rational comparison rather than a rounded share: a half is a
    // half, and a threshold decided by a rounding step is one an adversary can
    // sit on.
    if with_dev >= params.min_group && with_dev * 2 >= early.len() {
        risk_tags.push(RiskTag::FirstSlotCrowd);
        reasons.push(format!(
            "{with_dev} of {} opening buyers were in within {} slots of the launch",
            early.len(),
            params.dev_slots
        ));
    }
    if dev.creator_bought {
        risk_tags.push(RiskTag::CreatorBoughtOwn);
        reasons.push(format!(
            "the creator bought {} SOL of its own launch",
            format_sol(creator_lamports)
        ));
    }
    let creator_dominant =
        creator_lamports > 0 && u128::from(creator_lamports) * 2 >= u128::from(open_lamports);
    if creator_dominant {
        risk_tags.push(RiskTag::SoloDevDominance);
        reasons.push(format!(
            "the creator is {}% of the opening money",
            percent_of_bps(u64::from(creator_share_bps))
        ));
    }
    if creator_sold {
        risk_tags.push(RiskTag::CreatorExit);
        reasons.push("the creator sold inside the follow window".to_string());
    }
    if concentration_bps >= 7_000 && early.len() > 1 && !creator_dominant {
        risk_tags.push(RiskTag::WhaleConcentration);
        reasons.push(format!(
            "one wallet is {}% of the opening money",
            percent_of_bps(u64::from(concentration_bps))
        ));
    }
    if let Some(signal) = funding.as_ref().filter(|f| f.overlap_bps > 0) {
        risk_tags.push(RiskTag::SharedFunder);
        reasons.push(format!(
            "{} of {} opening buyers trace back to the same funding wallet within {} hops",
            signal.linked_wallets,
            early.len(),
            params.funding_depth
        ));
    }
    if thin {
        risk_tags.push(RiskTag::InsufficientData);
        reasons.push(format!(
            "only {} buyer{} in the opening — too few to tell coordination from coincidence",
            early.len(),
            if early.len() == 1 { "" } else { "s" }
        ));
    }

    // --- Concentration ---------------------------------------------------
    let mut ranked: Vec<(u64, &str)> = early
        .iter()
        .map(|b| (b.sol_in_lamports, b.wallet.as_str()))
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let descending: Vec<u64> = ranked.iter().map(|(value, _)| *value).collect();
    let opening_hhi = hhi_bps(&descending);

    let cluster_of: BTreeMap<usize, String> = clustering
        .clusters
        .iter()
        .enumerate()
        .flat_map(|(index, raw)| {
            raw.members
                .iter()
                .map(move |&member| (member, format!("c{}", index + 1)))
        })
        .collect();

    ClusterReport {
        mint: record.mint.clone(),
        creator: record.creator.clone(),
        window: Window {
            window_ms: params.window_ms,
            dev_slots: params.dev_slots,
            participants: early.len() as u32,
            considered,
            sol_in_lamports: open_lamports,
        },
        confidence_micros,
        thin,
        risk_tags,
        reasons,
        sizing,
        timing,
        dev,
        funding,
        clusters,
        participants: early
            .iter()
            .enumerate()
            .map(|(index, buyer)| Participant {
                wallet: buyer.wallet.clone(),
                sol_in_lamports: buyer.sol_in_lamports,
                sol_out_lamports: buyer.sol_out_lamports,
                tx_count: buyer.tx_count,
                first_seen_ms: buyer.first_seen_ms,
                cluster_id: cluster_of.get(&index).cloned(),
                flags: clustering
                    .flags
                    .get(&index)
                    .map(|set| set.iter().copied().collect())
                    .unwrap_or_default(),
            })
            .collect(),
        relations: clustering.relations,
        syndicate_size,
        largest_cluster,
        exposure_lamports,
        concentration: Concentration {
            hhi_bps: opening_hhi,
            top1_bps: top_k_bps(&descending, 1),
            top5_bps: top_k_bps(&descending, 5),
            top10_bps: top_k_bps(&descending, 10),
            effective_buyers_micros: opening_hhi.map_or(0, effective_holders_micros),
            entropy_micros: weighted_entropy_micros(&descending),
        },
    }
}

/// The report for a launch nobody bought inside the window.
///
/// Not an error and not a zero-confidence ordinary launch: `NO_OPENING_BUYS` is
/// its own tag because "we cannot see this" and "we looked and it was ordinary"
/// are different facts, and a funnel that merges them hides how much of a corpus
/// is simply too quiet to read.
fn empty_report(record: &LaunchRecord, params: &ClusterParams, considered: u32) -> ClusterReport {
    ClusterReport {
        mint: record.mint.clone(),
        creator: record.creator.clone(),
        window: Window {
            window_ms: params.window_ms,
            dev_slots: params.dev_slots,
            participants: 0,
            considered,
            sol_in_lamports: 0,
        },
        confidence_micros: 0,
        thin: true,
        risk_tags: vec![RiskTag::NoOpeningBuys],
        reasons: vec!["nobody bought inside the opening window".to_string()],
        sizing: SizingSignal {
            entropy_micros: MICROS,
            groups: Vec::new(),
            repeated_wallets: 0,
            largest_group: 0,
            score_micros: 0,
        },
        timing: TimingSignal {
            bundles: Vec::new(),
            largest_bundle: 0,
            span_ms: None,
            same_instant: false,
            launch_block: false,
            score_micros: 0,
        },
        dev: DevSignal {
            creator_bought: false,
            creator_lamports: 0,
            creator_share_bps: 0,
            creator_sold: false,
            with_dev: 0,
            with_dev_share_bps: 0,
            concentration_bps: 0,
            score_micros: 0,
        },
        funding: None,
        clusters: Vec::new(),
        participants: Vec::new(),
        relations: Vec::new(),
        syndicate_size: 0,
        largest_cluster: 0,
        exposure_lamports: 0,
        concentration: Concentration {
            hhi_bps: None,
            top1_bps: 0,
            top5_bps: 0,
            top10_bps: 0,
            effective_buyers_micros: 0,
            entropy_micros: None,
        },
    }
}

// ===========================================================================
// Internals
// ===========================================================================

/// Positions grouped by size, before the minimum-group filter.
struct RawSizeGroup {
    members: Vec<usize>,
    value_lamports: u64,
    exact: bool,
    round_number: bool,
}

/// Positions grouped by size.
///
/// Each group is bounded by the tolerance from its own smallest member rather
/// than chained neighbour to neighbour. Otherwise a long ladder of amounts one
/// percent apart collapses into a single group and every launch looks scripted.
///
/// Ties in the sort fall through to the wallet address so the partition does not
/// depend on the order the caller happened to hand the buyers over.
fn group_by_size(early: &[&OpeningBuyer], tolerance_bps: u64) -> Vec<RawSizeGroup> {
    let mut order: Vec<usize> = (0..early.len()).collect();
    order.sort_by(|&a, &b| {
        early[a]
            .sol_in_lamports
            .cmp(&early[b].sol_in_lamports)
            .then_with(|| early[a].wallet.cmp(&early[b].wallet))
    });

    let mut groups: Vec<Vec<usize>> = Vec::new();
    for index in order {
        let stake = early[index].sol_in_lamports;
        let extends = groups.last().is_some_and(|group| {
            let floor = early[group[0]].sol_in_lamports;
            u128::from(stake - floor) * u128::from(BPS_DENOMINATOR)
                <= u128::from(tolerance_bps) * u128::from(floor)
        });
        match (extends, groups.last_mut()) {
            (true, Some(group)) => group.push(index),
            _ => groups.push(vec![index]),
        }
    }

    groups
        .into_iter()
        .map(|members| {
            let value_lamports = early[members[0]].sol_in_lamports;
            let head = quantise_4dp(value_lamports);
            RawSizeGroup {
                // "Exact" is to four decimal places of SOL, which is the
                // precision the record keeps and finer than anybody types.
                exact: members
                    .iter()
                    .all(|&i| quantise_4dp(early[i].sol_in_lamports) == head),
                round_number: is_round_amount(value_lamports),
                value_lamports,
                members,
            }
        })
        .collect()
}

/// Buys that all landed inside one `gap_ms`-wide window.
///
/// The window is measured from the first buy in the run, not from the previous
/// one. Measuring gap to gap looks equivalent and is not: on a busy launch every
/// consecutive pair is a tenth of a second apart, the run never breaks, and
/// twenty-six wallets spread over a second and a half get reported as one
/// bundle. That reads as a conspiracy and is a queue.
///
/// `early` is already ordered by `(first_seen_ms, wallet)`, so this walks it
/// rather than sorting it again.
fn find_bundles(early: &[&OpeningBuyer], gap_ms: i64) -> Vec<Vec<usize>> {
    let mut bundles: Vec<Vec<usize>> = Vec::new();
    for index in 0..early.len() {
        let extends = bundles.last().is_some_and(|bundle| {
            early[index]
                .first_seen_ms
                .saturating_sub(early[bundle[0]].first_seen_ms)
                <= gap_ms
        });
        match (extends, bundles.last_mut()) {
            (true, Some(bundle)) => bundle.push(index),
            _ => bundles.push(vec![index]),
        }
    }
    bundles
}

/// The time from a run's first buy to its last.
///
/// Saturating: block times come from two providers that can disagree, and a
/// record carrying `i64::MIN` next to `i64::MAX` is a fact about the recording
/// rather than a reason to panic inside a build with `overflow-checks` on.
fn span_of(early: &[&OpeningBuyer], members: &[usize]) -> i64 {
    let first = early[members[0]].first_seen_ms;
    let last = early[*members.last().expect("a run is never empty")].first_seen_ms;
    last.saturating_sub(first)
}

struct RawCluster {
    members: Vec<usize>,
    kinds: BTreeSet<LinkKind>,
}

struct Clustering {
    clusters: Vec<RawCluster>,
    flags: BTreeMap<usize, BTreeSet<WalletFlag>>,
    relations: Vec<Relation>,
}

/// Join opening buyers into operators.
///
/// Every pair accumulates whatever evidence links it and is joined once that
/// evidence passes [`LINK_THRESHOLD_MICROS`], so no single weak signal can merge
/// a launch into one imaginary syndicate. The edges live in a `BTreeMap` keyed
/// by the index pair: connectivity is order-independent, but the relations list
/// is not, and a report that reorders itself between runs is not replayable.
fn cluster_wallets(
    early: &[&OpeningBuyer],
    size_groups: &[RawSizeGroup],
    bundles: &[Vec<usize>],
    params: &ClusterParams,
    funding_pairs: &[(String, String)],
) -> Clustering {
    let mut edges: BTreeMap<(usize, usize), (u64, BTreeSet<LinkKind>)> = BTreeMap::new();
    let mut link = |a: usize, b: usize, kind: LinkKind| {
        if a == b {
            return;
        }
        let key = if a < b { (a, b) } else { (b, a) };
        let entry = edges.entry(key).or_insert_with(|| (0, BTreeSet::new()));
        if entry.1.insert(kind) {
            entry.0 += kind.weight_micros();
        }
    };

    for group in size_groups {
        if group.members.len() < 2 {
            continue;
        }
        let kind = match (group.exact, group.round_number) {
            (true, true) => LinkKind::IdenticalRoundSize,
            (true, false) => LinkKind::IdenticalSize,
            (false, _) => LinkKind::NearSize,
        };
        for (position, &a) in group.members.iter().enumerate() {
            for &b in &group.members[position + 1..] {
                link(a, b, kind);
            }
        }
    }

    for bundle in bundles {
        if bundle.len() < 2 {
            continue;
        }
        let span = span_of(early, bundle);
        let kind = if span <= params.instant_ms {
            LinkKind::SameInstant
        } else {
            LinkKind::Bundle
        };
        for (position, &a) in bundle.iter().enumerate() {
            for &b in &bundle[position + 1..] {
                link(a, b, kind);
            }
        }
    }

    let index_of: BTreeMap<&str, usize> = early
        .iter()
        .enumerate()
        .map(|(index, buyer)| (buyer.wallet.as_str(), index))
        .collect();
    for (a, b) in funding_pairs {
        if let (Some(&i), Some(&j)) = (index_of.get(a.as_str()), index_of.get(b.as_str())) {
            link(i, j, LinkKind::SharedFunder);
        }
    }

    // Union-find over the edges that cleared the bar.
    let mut parent: Vec<usize> = (0..early.len()).collect();
    let mut relations: Vec<Relation> = Vec::new();
    let mut kept: Vec<((usize, usize), BTreeSet<LinkKind>)> = Vec::new();
    for (&(a, b), (weight, kinds)) in &edges {
        if *weight < LINK_THRESHOLD_MICROS {
            continue;
        }
        relations.push(Relation {
            a: early[a].wallet.clone(),
            b: early[b].wallet.clone(),
            weight_micros: *weight,
            kinds: kinds.iter().copied().collect(),
        });
        kept.push(((a, b), kinds.clone()));
        let (root_a, root_b) = (find_root(&mut parent, a), find_root(&mut parent, b));
        if root_a != root_b {
            parent[root_a] = root_b;
        }
    }

    let mut by_root: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for index in 0..early.len() {
        by_root
            .entry(find_root(&mut parent, index))
            .or_default()
            .push(index);
    }

    // Which kinds of evidence ended up inside each cluster, for the report.
    let mut kinds_by_root: BTreeMap<usize, BTreeSet<LinkKind>> = BTreeMap::new();
    for ((a, _), kinds) in &kept {
        let root = find_root(&mut parent, *a);
        kinds_by_root
            .entry(root)
            .or_default()
            .extend(kinds.iter().copied());
    }

    let mut clusters: Vec<RawCluster> = by_root
        .into_iter()
        .filter(|(_, members)| members.len() >= 2)
        .map(|(root, members)| RawCluster {
            kinds: kinds_by_root.remove(&root).unwrap_or_default(),
            members,
        })
        .collect();
    clusters.sort_by(|a, b| {
        let stake =
            |c: &RawCluster| total_lamports(c.members.iter().map(|&i| early[i].sol_in_lamports));
        b.members
            .len()
            .cmp(&a.members.len())
            .then_with(|| stake(b).cmp(&stake(a)))
            .then_with(|| early[a.members[0]].wallet.cmp(&early[b.members[0]].wallet))
    });

    // Per-wallet flags, including the ones that do not depend on being clustered.
    let mut flags: BTreeMap<usize, BTreeSet<WalletFlag>> = BTreeMap::new();
    for group in size_groups {
        if group.members.len() < params.min_group {
            continue;
        }
        let flag = if group.exact {
            WalletFlag::IdenticalSize
        } else {
            WalletFlag::NearIdenticalSize
        };
        for &member in &group.members {
            flags.entry(member).or_default().insert(flag);
        }
    }
    for bundle in bundles {
        if bundle.len() < params.min_group {
            continue;
        }
        let span = span_of(early, bundle);
        let flag = if span <= params.instant_ms {
            WalletFlag::SameInstant
        } else {
            WalletFlag::Bundled
        };
        for &member in bundle {
            flags.entry(member).or_default().insert(flag);
        }
    }
    for (a, b) in funding_pairs {
        for wallet in [a, b] {
            if let Some(&index) = index_of.get(wallet.as_str()) {
                flags
                    .entry(index)
                    .or_default()
                    .insert(WalletFlag::SharedFunder);
            }
        }
    }
    for (index, buyer) in early.iter().enumerate() {
        if buyer.sol_out_lamports > 0 {
            flags
                .entry(index)
                .or_default()
                .insert(WalletFlag::SoldInWindow);
        }
    }

    Clustering {
        clusters,
        flags,
        relations,
    }
}

/// Path-halving find. Iterative because the depth is data an adversary shapes.
fn find_root(parent: &mut [usize], mut index: usize) -> usize {
    while parent[index] != index {
        parent[index] = parent[parent[index]];
        index = parent[index];
    }
    index
}

/// Who paid for the opening buyers, and where those paths meet.
struct Traced {
    funders: Vec<SharedFunderRow>,
    hubs: BTreeSet<String>,
    pairs: Vec<(String, String)>,
    linked: BTreeSet<String>,
    /// Non-hub funder to the wallets it reaches.
    reach: BTreeMap<String, BTreeSet<String>>,
    overlap_bps: u16,
}

impl Traced {
    /// The largest share of a cluster's opening money that traces back to one
    /// non-hub funder.
    fn cluster_funder_share_bps(&self, members: &[&OpeningBuyer]) -> u16 {
        let total = total_lamports(members.iter().map(|m| m.sol_in_lamports));
        if total == 0 {
            return 0;
        }
        let best = self
            .reach
            .values()
            .map(|wallets| {
                total_lamports(
                    members
                        .iter()
                        .filter(|m| wallets.contains(&m.wallet))
                        .map(|m| m.sol_in_lamports),
                )
            })
            .max()
            .unwrap_or(0);
        share_bps(best, total)
    }
}

/// One node of the funding graph. Only the neighbour sets are kept: the walk
/// needs who paid a wallet, and the hub test needs how many addresses a funder
/// paid. Amounts belong to §3.3's path influence, which is not computed here.
#[derive(Default)]
struct FundingNode {
    out: BTreeSet<String>,
    inbound: BTreeSet<String>,
}

/// Walk backwards from each wallet looking for the address that paid for it, and
/// report where those paths meet.
///
/// The trap is the exchange. A hot wallet funds thousands of unrelated people,
/// and treating it as a shared funder makes every launch look like a syndicate.
/// Any address that has paid out to more than `hub_degree` distinct addresses in
/// the graph it was given is marked a hub and left out of the overlap —
/// reported, not silently dropped.
///
/// A hub is still *expanded* through, exactly as the prototype did, because a
/// wallet funded by a hub may itself have been funded by the person before the
/// hub laundered it. What the hub does not do is put its own name on the
/// overlap.
fn find_shared_funders(edges: &[FundingEdge], wallets: &[&str], params: &ClusterParams) -> Traced {
    let mut graph: BTreeMap<&str, FundingNode> = BTreeMap::new();
    for edge in edges {
        if edge.from.is_empty() || edge.to.is_empty() || edge.from == edge.to {
            continue;
        }
        graph
            .entry(edge.from.as_str())
            .or_default()
            .out
            .insert(edge.to.clone());
        graph
            .entry(edge.to.as_str())
            .or_default()
            .inbound
            .insert(edge.from.clone());
    }

    let unique: Vec<&str> = {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        wallets
            .iter()
            .copied()
            .filter(|wallet| seen.insert(wallet))
            .collect()
    };
    let skip: BTreeSet<&str> = unique.iter().copied().collect();

    let empty = |overlap_bps: u16| Traced {
        funders: Vec::new(),
        hubs: BTreeSet::new(),
        pairs: Vec::new(),
        linked: BTreeSet::new(),
        reach: BTreeMap::new(),
        overlap_bps,
    };
    if graph.is_empty() || unique.len() < 2 {
        return empty(0);
    }

    // funder -> wallet -> fewest hops from that wallet back to the funder.
    let mut hops_to: BTreeMap<String, BTreeMap<String, u32>> = BTreeMap::new();
    for &wallet in &unique {
        let mut frontier: Vec<String> = vec![wallet.to_string()];
        let mut seen: BTreeSet<String> = BTreeSet::from([wallet.to_string()]);
        for hop in 1..=params.funding_depth {
            let mut next: Vec<String> = Vec::new();
            for address in &frontier {
                let Some(node) = graph.get(address.as_str()) else {
                    continue;
                };
                for funder in &node.inbound {
                    if !seen.insert(funder.clone()) {
                        continue;
                    }
                    next.push(funder.clone());
                    if skip.contains(funder.as_str()) {
                        continue;
                    }
                    hops_to
                        .entry(funder.clone())
                        .or_default()
                        .entry(wallet.to_string())
                        .or_insert(hop);
                }
            }
            frontier = next;
        }
    }

    let mut funders: Vec<SharedFunderRow> = Vec::new();
    let mut hubs: BTreeSet<String> = BTreeSet::new();
    let mut linked: BTreeSet<String> = BTreeSet::new();
    let mut reach: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut pairs: Vec<(String, String)> = Vec::new();

    for (funder, reached) in &hops_to {
        if reached.len() < 2 {
            continue;
        }
        let hub = graph
            .get(funder.as_str())
            .is_some_and(|node| node.out.len() > params.hub_degree);
        funders.push(SharedFunderRow {
            funder: funder.clone(),
            hops: reached.values().copied().min().unwrap_or(0),
            wallets: reached.len() as u32,
        });
        if hub {
            hubs.insert(funder.clone());
            continue;
        }
        let members: Vec<&String> = reached.keys().collect();
        reach.insert(
            funder.clone(),
            members.iter().map(|m| (*m).clone()).collect(),
        );
        for member in &members {
            linked.insert((*member).clone());
        }
        for (position, a) in members.iter().enumerate() {
            for b in &members[position + 1..] {
                pairs.push(((*a).clone(), (*b).clone()));
            }
        }
    }

    // Loudest first, then nearest, then by address so nothing floats.
    funders.sort_by(|a, b| {
        b.wallets
            .cmp(&a.wallets)
            .then_with(|| a.hops.cmp(&b.hops))
            .then_with(|| a.funder.cmp(&b.funder))
    });

    let overlap_bps = share_bps(linked.len() as u64, unique.len() as u64);
    Traced {
        funders,
        hubs,
        pairs,
        linked,
        reach,
        overlap_bps,
    }
}

/// §5.1's interaction entropy over the funding edges that run between a
/// cluster's own members.
///
/// Transfers between the same ordered pair are one edge with their volumes
/// added: two payments from one wallet to another is one relationship, and
/// counting them twice would make a star look like a crowd. Self-loops are
/// dropped before assembly per §7.1 — a wallet sending to itself is not an
/// interaction.
fn internal_entropy(edges: &[FundingEdge], members: &BTreeSet<&str>) -> Option<u64> {
    let mut volumes: BTreeMap<(&str, &str), u64> = BTreeMap::new();
    for edge in edges {
        if edge.from == edge.to
            || !members.contains(edge.from.as_str())
            || !members.contains(edge.to.as_str())
        {
            continue;
        }
        let slot = volumes
            .entry((edge.from.as_str(), edge.to.as_str()))
            .or_insert(0);
        *slot = slot.saturating_add(edge.lamports);
    }
    let weights: Vec<u64> = volumes.into_values().collect();
    weighted_entropy_micros(&weights)
}

/// Lamports added up without a panic.
///
/// Saturating rather than checked because every caller is measuring a share of
/// something, and a corrupt record whose stakes sum past `u64::MAX` should make
/// the shares meaningless rather than take the engine down — `overflow-checks`
/// is on in release, so a plain `sum()` here is a panic, not a wrap.
fn total_lamports<I: IntoIterator<Item = u64>>(values: I) -> u64 {
    values.into_iter().fold(0u64, u64::saturating_add)
}

/// A part of a whole, in basis points, floored. Zero when there is no whole —
/// which is a share of nothing, not a share of everything.
fn share_bps(part: u64, whole: u64) -> u16 {
    mul_div_floor(
        u128::from(part),
        u128::from(BPS_DENOMINATOR),
        u128::from(whole),
    )
    .min(u128::from(BPS_DENOMINATOR)) as u16
}

/// How much a repeated group of `k` wallets is worth, from nothing below the
/// minimum to everything three past it. Three matching wallets could be an
/// accident; six could not.
fn group_strength_micros(k: usize, min_group: usize) -> u64 {
    if k == 0 || k < min_group {
        return 0;
    }
    mul_div_floor((k - min_group + 1) as u128, u128::from(MICROS), 3).min(u128::from(MICROS)) as u64
}

/// How trustworthy a repeat is. Matching to the fourth decimal is a script.
/// Matching on a number a person would type — 1 SOL, 0.5 SOL — is the one repeat
/// that happens honestly, so it is discounted and has to be corroborated.
fn size_quality_micros(repeated: &[&RawSizeGroup]) -> u64 {
    match largest_of(repeated) {
        None => 0,
        Some(group) if !group.exact => 600_000,
        Some(group) if group.round_number => 750_000,
        Some(_) => MICROS,
    }
}

/// The biggest group, first one winning a tie.
fn largest_of<'a>(groups: &[&'a RawSizeGroup]) -> Option<&'a RawSizeGroup> {
    groups.iter().copied().reduce(|best, current| {
        if current.members.len() > best.members.len() {
            current
        } else {
            best
        }
    })
}

/// Lamports to ten-thousandths of a SOL, rounded to nearest — the precision the
/// record keeps and the one "identical" is decided at.
fn quantise_4dp(lamports: u64) -> u64 {
    lamports.saturating_add(50_000) / 100_000
}

/// An amount a person might have typed, rather than one a script produced.
///
/// The step is a tenth of a SOL at or above one SOL and a twentieth below it,
/// and the tolerance is a millionth of the step — which is a hundred lamports,
/// enough to survive the record's own rounding and far too tight to admit an
/// arbitrary amount.
fn is_round_amount(lamports: u64) -> bool {
    if lamports == 0 {
        return false;
    }
    let step = if lamports >= LAMPORTS_PER_SOL {
        LAMPORTS_PER_SOL / 10
    } else {
        LAMPORTS_PER_SOL / 20
    };
    let tolerance = step / 1_000_000;
    let residue = lamports % step;
    residue <= tolerance || residue >= step - tolerance
}

/// Lamports as SOL to four decimal places, with trailing zeroes trimmed. For
/// the human sentences only; nothing compares against this.
fn format_sol(lamports: u64) -> String {
    let ten_thousandths = quantise_4dp(lamports);
    let whole = ten_thousandths / 10_000;
    let fraction = ten_thousandths % 10_000;
    if fraction == 0 {
        return whole.to_string();
    }
    let mut digits = format!("{fraction:04}");
    while digits.ends_with('0') {
        digits.pop();
    }
    format!("{whole}.{digits}")
}

/// Basis points as whole percent, rounded to nearest. Sentences only.
fn percent_of_bps(bps: u64) -> u64 {
    (bps + 50) / 100
}

// ===========================================================================
// The entry rule
// ===========================================================================

/// The wallets that both landed together and took the same position.
///
/// The syndicate itself, as opposed to everyone who happened to be in the same
/// fraction of a second as it. Read off the largest bundle the analyser found,
/// then narrowed to the widest run of positions inside it that sit within
/// `tolerance_bps` of each other.
///
/// Both halves are load-bearing. A bundle on a busy launch is mostly a queue —
/// on the recorded corpus the widest one spans twenty-six wallets whose
/// positions differ by a factor of a hundred and twenty-five — and a repeated
/// size somewhere in the opening says nothing about whether those particular
/// wallets arrived together. The intersection is the only part that means one
/// operator.
///
/// The run is bounded from its own smallest member rather than chained
/// neighbour to neighbour, for the same reason [`group_by_size`] is: a ladder of
/// amounts one percent apart would otherwise collapse into one group and every
/// launch would look scripted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cohort {
    /// `None` when no bundle cleared the analyser's own minimum.
    pub bundle: Option<TimingBundle>,
    pub wallets: u32,
    pub lamports: u64,
    /// Wallets in the group that are not the deployer. A deployer buying its own
    /// launch alongside two of its own wallets is a different thing from three
    /// strangers, and only this count can tell them apart.
    pub external: u32,
    /// The smallest position in the group, which is the size it is named by.
    pub size_lamports: Option<u64>,
    /// How far the group's largest position sits above its smallest.
    pub delta_bps: Option<u64>,
}

impl Cohort {
    fn none(bundle: Option<TimingBundle>) -> Self {
        Cohort {
            bundle,
            wallets: 0,
            lamports: 0,
            external: 0,
            size_lamports: None,
            delta_bps: None,
        }
    }
}

/// The coordinated group inside a report's largest bundle.
///
/// `tolerance_bps` of `None` turns the size test off: every wallet in the bundle
/// is then one group however far apart their positions are, which is what makes
/// [`GateParams::v1`] runnable rather than quoted.
pub fn coordinated_cohort(report: &ClusterReport, tolerance_bps: Option<u64>) -> Cohort {
    // The largest bundle, earliest one winning a tie — the same one the analyser
    // scored the timing signal on.
    let Some(bundle) = report.timing.bundles.iter().reduce(|best, current| {
        if current.wallets > best.wallets
            || (current.wallets == best.wallets && current.at_ms < best.at_ms)
        {
            current
        } else {
            best
        }
    }) else {
        return Cohort::none(None);
    };

    // Matched as a multiset rather than a set. One address can appear twice in
    // an opening — the watcher writes a row per observation and a corrupt or
    // re-observed record can repeat one — and a plain `contains` would let this
    // bundle claim that address's row from a *different* bundle, which is how a
    // group of three ends up reported as a group of four.
    let mut budget: BTreeMap<&str, usize> = BTreeMap::new();
    for member in &bundle.members {
        *budget.entry(member.as_str()).or_insert(0) += 1;
    }
    let mut rows: Vec<&Participant> = Vec::new();
    for participant in &report.participants {
        if participant.sol_in_lamports == 0 {
            continue;
        }
        if let Some(remaining) = budget.get_mut(participant.wallet.as_str()) {
            if *remaining > 0 {
                *remaining -= 1;
                rows.push(participant);
            }
        }
    }
    if rows.is_empty() {
        return Cohort::none(Some(bundle.clone()));
    }
    rows.sort_by(|a, b| {
        a.sol_in_lamports
            .cmp(&b.sol_in_lamports)
            .then_with(|| a.wallet.cmp(&b.wallet))
    });

    // Widest window over the sorted positions whose ends are within tolerance. A
    // window rather than a left-to-right sweep because the question is how big
    // the matching group is, not how the launch partitions: a sweep anchored on
    // the smallest position can be broken by one wallet and miss a larger group
    // sitting just above it.
    let (mut low, mut best_low, mut best_len) = (0usize, 0usize, 0usize);
    for high in 0..rows.len() {
        if let Some(tolerance) = tolerance_bps {
            while u128::from(rows[high].sol_in_lamports - rows[low].sol_in_lamports)
                * u128::from(BPS_DENOMINATOR)
                > u128::from(tolerance) * u128::from(rows[low].sol_in_lamports)
            {
                low += 1;
            }
        }
        if high - low + 1 > best_len {
            best_low = low;
            best_len = high - low + 1;
        }
    }

    let best = &rows[best_low..best_low + best_len];
    let smallest = best[0].sol_in_lamports;
    let largest = best[best_len - 1].sol_in_lamports;
    Cohort {
        bundle: Some(bundle.clone()),
        wallets: best_len as u32,
        lamports: total_lamports(best.iter().map(|row| row.sol_in_lamports)),
        external: best
            .iter()
            .filter(|row| report.creator.as_deref() != Some(row.wallet.as_str()))
            .count() as u32,
        size_lamports: Some(smallest),
        delta_bps: Some(mul_div_floor(
            u128::from(largest - smallest),
            u128::from(BPS_DENOMINATOR),
            u128::from(smallest),
        ) as u64),
    }
}

/// Does this launch look organised enough to follow, and if not, why not.
///
/// The reason is worth as much as the verdict: a rule that took four trades out
/// of three thousand launches is only interpretable next to the count of what
/// each rejection threw out. So every refusal carries the same facts an
/// acceptance does, and a caller can build that funnel without running a
/// backtest.
///
/// The order of the checks is the order of confidence in them. The two data
/// refusals come first and are absolute: a launch nobody bought, or one bought
/// by fewer than three wallets, cannot be told apart from noise whatever score
/// falls out of it. The analyser already caps a thin launch's confidence at
/// [`THIN_CEILING_MICROS`], so the score test would catch these anyway — they
/// are named separately because "we cannot see this" and "we looked and it was
/// ordinary" are different facts.
///
/// The last four checks all ask the same question from different sides: the tags
/// say a signal fired *somewhere* in the opening, and these ask whether it fired
/// on a group large enough, uniform enough and rich enough to be the thing the
/// trade is following.
pub fn syndicate_gate(
    report: &ClusterReport,
    params: &GateParams,
    quote: Option<&EntryQuote>,
) -> GateVerdict {
    let cohort = coordinated_cohort(report, params.bundle_size_tolerance_bps);

    // Both of these are computed before the first refusal rather than at the
    // step that reads them, so a launch turned down for being thin still carries
    // what its clusters looked like and what the curve said. A funnel that only
    // fills these columns in on the launches that got far enough to be measured
    // is a funnel that cannot tell a rule that never fires from a rule that
    // never got asked.
    let rings = report.rings(
        params.ring_min_hhi_bps,
        params.ring_max_entropy_micros,
        params.ring_min_lamports,
    );
    let sandwich = match params.sandwich_guard {
        SandwichGuard::Off => None,
        _ => quote.map(SandwichCheck::of),
    };

    let verdict = |reason: GateReason| GateVerdict {
        enter: reason == GateReason::Accepted,
        reason,
        confidence_micros: report.confidence_micros,
        tags: report.risk_tags.clone(),
        thin: report.thin,
        bundle_wallets: cohort.bundle.as_ref().map_or(0, |b| b.wallets),
        bundle_lamports: cohort.bundle.as_ref().map_or(0, |b| b.lamports),
        cohort_wallets: cohort.wallets,
        cohort_lamports: cohort.lamports,
        cohort_size_lamports: cohort.size_lamports,
        cohort_delta_bps: cohort.delta_bps,
        cohort_external: cohort.external,
        rings: rings.clone(),
        sandwich,
    };

    if report.has_tag(RiskTag::NoOpeningBuys) {
        return verdict(GateReason::NoOpeningBuys);
    }
    if report.thin {
        return verdict(GateReason::Thin);
    }
    if report.confidence_micros < params.min_score_micros {
        return verdict(GateReason::LowScore);
    }

    let primaries: Vec<RiskTag> = report
        .risk_tags
        .iter()
        .copied()
        .filter(|tag| params.primary_signals.contains(tag))
        .collect();
    if primaries.is_empty() {
        return verdict(GateReason::NoPrimarySignal);
    }

    if params.min_bundle_wallets > 0 {
        let Some(bundle) = cohort.bundle.as_ref() else {
            return verdict(GateReason::NoBundle);
        };
        if (bundle.wallets as usize) < params.min_bundle_wallets {
            return verdict(GateReason::ThinBundle);
        }
        if (cohort.wallets as usize) < params.min_bundle_wallets {
            return verdict(GateReason::MixedSizing);
        }
    }

    // A deployer buying its own coin is the weakest of the primary signals — the
    // analyser weights it lowest for the same reason — and on its own it says
    // "rug risk", not "syndicate". It only becomes an entry when somebody other
    // than the deployer bought in with it.
    if params.require_external_bundle
        && primaries == [RiskTag::CreatorBoughtOwn]
        && (cohort.external as usize) < params.min_bundle_wallets.max(1)
    {
        return verdict(GateReason::SoloDev);
    }

    if params.min_bundle_lamports > 0 && cohort.lamports < params.min_bundle_lamports {
        return verdict(GateReason::SmallBundle);
    }

    // Everything above asked whether the launch is organised. This asks whether
    // the thing that organised it is a group at all, and it comes last because
    // it is the only check that can throw out a launch the rest of the rule
    // liked — which is the position a new refusal belongs in if the funnel is to
    // show what it cost.
    if rings.iter().any(|ring| ring.material) {
        return verdict(GateReason::CoordinatedRing);
    }

    // And this one is not about the launch at all. It is about our own order,
    // which is why it is after every question about the buyers: a launch that
    // fails here failed on its size, and the same launch at a smaller size or
    // through a bundle is a different answer.
    match (params.sandwich_guard, sandwich) {
        (SandwichGuard::Off, _) => {}
        (SandwichGuard::Required, None) => return verdict(GateReason::NoCurveQuote),
        (_, Some(check)) if check.refuses() => return verdict(GateReason::SandwichRisk),
        _ => {}
    }

    verdict(GateReason::Accepted)
}

/// What the gate decided, and the numbers it decided on.
///
/// Carried on every verdict including the refusals, so a funnel can show what
/// the group actually looked like next to the reason it was turned down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateVerdict {
    pub enter: bool,
    pub reason: GateReason,
    pub confidence_micros: u64,
    pub tags: Vec<RiskTag>,
    pub thin: bool,
    pub bundle_wallets: u32,
    pub bundle_lamports: u64,
    pub cohort_wallets: u32,
    pub cohort_lamports: u64,
    pub cohort_size_lamports: Option<u64>,
    pub cohort_delta_bps: Option<u64>,
    pub cohort_external: u32,
    /// Every ring the scan found, material or not. Empty when the check is off.
    pub rings: Vec<RingFinding>,
    /// What the curve said about our own order. `None` when the guard is off or
    /// no quote came with the launch — and those two are not the same thing,
    /// which is what [`SandwichGuard::Required`] exists to say.
    pub sandwich: Option<SandwichCheck>,
}

/// Read a launch and decide about it, in one call.
pub fn evaluate(
    record: &LaunchRecord,
    cluster: &ClusterParams,
    gate: &GateParams,
    quote: Option<&EntryQuote>,
) -> (ClusterReport, GateVerdict) {
    let report = analyse_launch(record, cluster);
    let verdict = syndicate_gate(&report, gate, quote);
    (report, verdict)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOL: u64 = LAMPORTS_PER_SOL;

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    fn buyer(wallet: &str, lamports: u64, at_ms: i64) -> OpeningBuyer {
        OpeningBuyer {
            wallet: wallet.to_string(),
            sol_in_lamports: lamports,
            sol_out_lamports: 0,
            tx_count: 1,
            first_seen_ms: at_ms,
        }
    }

    fn seller(wallet: &str, lamports: u64, out: u64, at_ms: i64) -> OpeningBuyer {
        OpeningBuyer {
            sol_out_lamports: out,
            ..buyer(wallet, lamports, at_ms)
        }
    }

    fn edge(from: &str, to: &str, lamports: u64) -> FundingEdge {
        FundingEdge {
            from: from.to_string(),
            to: to.to_string(),
            lamports,
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

    fn gate(record: &LaunchRecord) -> GateVerdict {
        evaluate(
            record,
            &ClusterParams::default(),
            &GateParams::default(),
            None,
        )
        .1
    }

    /// The same, with an order to price against the curve.
    fn gate_quoted(record: &LaunchRecord, quote: &EntryQuote) -> GateVerdict {
        evaluate(
            record,
            &ClusterParams::default(),
            &GateParams::default(),
            Some(quote),
        )
        .1
    }

    /// Six wallets, one odd amount to the lamport, all landing in the same
    /// instant a full two seconds after the launch — long enough that the block
    /// race cannot explain it. This is the shape the rule exists to find.
    fn the_script() -> LaunchRecord {
        launch(
            (1..=6)
                .map(|n| buyer(&format!("w{n}"), 777_700_000, 2_000))
                .collect(),
        )
    }

    /// The script, with the wallet that is actually holding the position
    /// standing behind it.
    ///
    /// Six identical 0.7777 SOL buys and one 45 SOL buy, every one of them paid
    /// for by the same address a hop back, so all seven land in one cluster.
    /// That cluster is `RISK_AND_SYBIL_SPEC.md` §14's `[90, 10]` shape: the six
    /// are the buyer count and the one is the money.
    fn a_coordinated_ring() -> LaunchRecord {
        let mut buyers: Vec<OpeningBuyer> = (1..=6)
            .map(|n| buyer(&format!("w{n}"), 777_700_000, 2_000))
            .collect();
        buyers.push(buyer("whale", 45 * SOL, 2_000));
        let mut funding: Vec<FundingEdge> =
            (1..=6).map(|n| edge("F", &format!("w{n}"), SOL)).collect();
        funding.push(edge("F", "whale", 50 * SOL));
        LaunchRecord {
            funding,
            ..launch(buyers)
        }
    }

    /// A cluster with nothing interesting in it, to change one column of.
    fn a_cluster() -> Cluster {
        Cluster {
            id: "c1".to_string(),
            size: 2,
            lamports: 10 * SOL,
            share_of_open_bps: 10_000,
            first_at_ms: 2_000,
            members: vec!["a".to_string(), "b".to_string()],
            reasons: vec![LinkKind::SharedFunder],
            holding_hhi_bps: Some(5_000),
            holding_entropy_micros: Some(MICROS),
            sync_micros: Some(MICROS),
            sync_truncated: false,
            funder_share_bps: None,
            temporal_influence_micros: None,
            interaction_entropy_micros: None,
        }
    }

    /// The same ring at a size nobody could trade against.
    fn a_tiny_ring() -> LaunchRecord {
        let record = a_coordinated_ring();
        LaunchRecord {
            buyers: record
                .buyers
                .iter()
                .map(|b| OpeningBuyer {
                    sol_in_lamports: b.sol_in_lamports / 1_000,
                    ..b.clone()
                })
                .collect(),
            ..record
        }
    }

    /// The same six wallets, at a size the group cannot move the price with.
    fn a_poor_script() -> LaunchRecord {
        launch(
            (1..=6)
                .map(|n| buyer(&format!("w{n}"), 177_700_000, 2_000))
                .collect(),
        )
    }

    /// A queue at a busy launch with a scripted group somewhere else in it.
    ///
    /// Six wallets race one block with positions a factor of thirty-two apart —
    /// that is a queue — while four unrelated wallets spread across the opening
    /// happen to have taken the same size. v1 read the two signals as one
    /// syndicate; the whole point of the size test is that they are disjoint
    /// sets of wallets.
    fn a_queue_with_a_script_elsewhere() -> LaunchRecord {
        let mut buyers: Vec<OpeningBuyer> = vec![
            buyer("q1", SOL / 10, 2_000),
            buyer("q2", SOL / 5, 2_000),
            buyer("q3", 2 * SOL / 5, 2_000),
            buyer("q4", 4 * SOL / 5, 2_000),
            buyer("q5", 8 * SOL / 5, 2_000),
            buyer("q6", 16 * SOL / 5, 2_000),
        ];
        for (index, at) in [500, 800, 1_100, 1_400].into_iter().enumerate() {
            buyers.push(buyer(&format!("s{}", index + 1), 555_500_000, at));
        }
        launch(buyers)
    }

    /// A deployer that took two thirds of the opening and sold, with six of its
    /// own wallets spread far enough apart that nothing bundles.
    fn no_bundle_at_all() -> LaunchRecord {
        let mut buyers = vec![seller("DEV", 10 * SOL, SOL, 100)];
        for (index, at) in [400, 800, 1_200, 1_600, 2_000, 2_400]
            .into_iter()
            .enumerate()
        {
            buyers.push(buyer(&format!("w{}", index + 1), 777_700_000, at));
        }
        LaunchRecord {
            creator: Some("DEV".to_string()),
            ..launch(buyers)
        }
    }

    /// A deployer buying alongside two wallets at near-identical sizes: the one
    /// primary signal is `CREATOR_BOUGHT_OWN`, and the matching group is mostly
    /// the deployer itself.
    fn a_deployer_buying_alone() -> LaunchRecord {
        LaunchRecord {
            creator: Some("DEV".to_string()),
            ..launch(vec![
                buyer("DEV", SOL, 2_000),
                buyer("w1", 1_003_000_000, 2_030),
                buyer("w2", 1_006_000_000, 2_060),
            ])
        }
    }

    /// Six wallets funded from one address, at near-identical sizes, landing
    /// over a tenth of a second. Every tag it carries says "unusual"; none of
    /// them says "one person" in the sense the entry rule means.
    fn unusual_but_not_primary() -> LaunchRecord {
        let sizes = [
            1_000_000_000u64,
            1_004_000_000,
            1_008_000_000,
            1_012_000_000,
            1_016_000_000,
            1_019_900_000,
        ];
        let times = [2_000i64, 2_030, 2_060, 2_090, 2_120, 2_150];
        let buyers: Vec<OpeningBuyer> = sizes
            .iter()
            .zip(times)
            .enumerate()
            .map(|(index, (&size, at))| buyer(&format!("w{}", index + 1), size, at))
            .collect();
        let funding = (1..=6)
            .map(|n| edge("FUNDER", &format!("w{n}"), SOL))
            .collect();
        LaunchRecord {
            funding,
            ..launch(buyers)
        }
    }

    /// The weighted sum the confidence score is defined as, recomputed from the
    /// component scores the report carries.
    fn documented_confidence(report: &ClusterReport) -> u64 {
        let weighted = |weight: u64, score: u64| {
            mul_div_floor(u128::from(weight), u128::from(score), u128::from(MICROS)) as u64
        };
        let raw = weighted(500_000, report.sizing.score_micros)
            + weighted(300_000, report.timing.score_micros)
            + weighted(200_000, report.dev.score_micros)
            + report
                .funding
                .as_ref()
                .map_or(0, |f| weighted(800_000, f.score_micros));
        let capped = raw.min(MICROS);
        if report.thin {
            capped.min(THIN_CEILING_MICROS)
        } else {
            capped
        }
    }

    // -----------------------------------------------------------------------
    // Degenerate inputs — RISK_AND_SYBIL_SPEC.md §2.4 and §7.1
    // -----------------------------------------------------------------------

    #[test]
    fn a_launch_nobody_bought_is_unknown_rather_than_clean() {
        let report = read(&launch(Vec::new()));
        assert!(report.has_tag(RiskTag::NoOpeningBuys));
        assert!(report.thin);
        assert_eq!(report.confidence_micros, 0);
        assert_eq!(report.concentration.hhi_bps, None);
        assert_eq!(report.window.participants, 0);
        assert!(report.clusters.is_empty());
    }

    #[test]
    fn balances_of_zero_leave_nothing_to_measure() {
        let report = read(&launch(vec![
            buyer("a", 0, 0),
            buyer("b", 0, 0),
            buyer("c", 0, 0),
        ]));
        assert!(report.has_tag(RiskTag::NoOpeningBuys));
        assert_eq!(report.concentration.hhi_bps, None);
    }

    #[test]
    fn buys_outside_the_window_are_not_opening_buys() {
        let report = read(&launch(vec![
            buyer("a", SOL, 3_001),
            buyer("b", SOL, 60_000),
        ]));
        assert!(report.has_tag(RiskTag::NoOpeningBuys));
        assert_eq!(report.window.considered, 2);
    }

    #[test]
    fn a_launch_too_thin_to_read_says_so_and_is_capped() {
        let report = read(&launch(vec![
            buyer("a", 5 * SOL, 0),
            buyer("b", 5 * SOL, 0),
        ]));
        assert!(report.thin);
        assert!(report.has_tag(RiskTag::InsufficientData));
        assert!(report.confidence_micros <= THIN_CEILING_MICROS);
    }

    #[test]
    fn two_wallets_with_nothing_between_them_are_not_a_cluster() {
        let report = read(&launch(vec![
            buyer("a", SOL, 0),
            buyer("b", 9 * SOL, 2_000),
        ]));
        assert!(report.clusters.is_empty());
        assert_eq!(report.syndicate_size, 0);
    }

    #[test]
    fn identical_buy_times_give_a_synchrony_of_exactly_one() {
        // Three identical odd sizes far enough apart in time to bundle
        // separately, so the only link is the size and the cluster is real.
        let report = read(&launch(vec![
            buyer("a", 777_700_000, 0),
            buyer("b", 777_700_000, 0),
            buyer("c", 777_700_000, 0),
        ]));
        let cluster = report.clusters.first().expect("one cluster");
        assert_eq!(cluster.sync_micros, Some(MICROS));
        assert!(!cluster.sync_truncated);
    }

    #[test]
    fn a_self_transfer_is_not_an_interaction() {
        let record = LaunchRecord {
            funding: vec![
                edge("FUNDER", "a", SOL),
                edge("FUNDER", "b", SOL),
                edge("a", "a", SOL),
            ],
            ..launch(vec![buyer("a", 3 * SOL, 0), buyer("b", 7 * SOL, 1_000)])
        };
        let report = read(&record);
        let cluster = report.clusters.first().expect("the funder joins them");
        // One self-loop, dropped, leaves no internal edges at all.
        assert_eq!(cluster.interaction_entropy_micros, None);
    }

    #[test]
    fn a_cluster_with_one_internal_edge_is_unmeasurable_not_low_entropy() {
        let record = LaunchRecord {
            funding: vec![
                edge("FUNDER", "a", SOL),
                edge("FUNDER", "b", SOL),
                edge("a", "b", SOL),
            ],
            ..launch(vec![buyer("a", 3 * SOL, 0), buyer("b", 7 * SOL, 1_000)])
        };
        let cluster = read(&record).clusters.remove(0);
        assert_eq!(cluster.interaction_entropy_micros, None);
    }

    // -----------------------------------------------------------------------
    // Concentration — the §14 test vectors
    // -----------------------------------------------------------------------

    #[test]
    fn one_buyer_holding_everything_is_the_maximum_index() {
        let report = read(&launch(vec![buyer("a", 100 * SOL, 0)]));
        assert_eq!(report.concentration.hhi_bps, Some(10_000));
        assert_eq!(report.concentration.top1_bps, 10_000);
        assert_eq!(report.concentration.effective_buyers_micros, MICROS);
    }

    #[test]
    fn four_equal_buyers_behave_like_four_buyers() {
        let report = read(&launch(
            (1..=4)
                .map(|n| buyer(&format!("w{n}"), 25 * SOL, n))
                .collect(),
        ));
        assert_eq!(report.concentration.hhi_bps, Some(2_500));
        assert_eq!(report.concentration.effective_buyers_micros, 4 * MICROS);
    }

    #[test]
    fn ninety_ten_lands_on_the_published_vector() {
        let report = read(&launch(vec![
            buyer("a", 90 * SOL, 0),
            buyer("b", 10 * SOL, 1),
        ]));
        assert_eq!(report.concentration.hhi_bps, Some(8_200));
        assert_eq!(report.concentration.top1_bps, 9_000);
    }

    #[test]
    fn dust_does_not_dilute_control() {
        // The spec's `[50] + fifty [1]`: fifty-one accounts, two owners' worth
        // of concentration.
        let mut buyers = vec![buyer("whale", 50 * SOL, 0)];
        for n in 1..=50 {
            buyers.push(buyer(&format!("d{n:02}"), SOL, n));
        }
        let report = analyse_launch(
            &launch(buyers),
            &ClusterParams {
                max_wallets: 100,
                ..ClusterParams::default()
            },
        );
        assert_eq!(report.window.participants, 51);
        assert_eq!(report.concentration.hhi_bps, Some(2_550));
        // Fifty-one accounts, under four owners.
        assert_eq!(report.concentration.effective_buyers_micros, 3_921_568);
    }

    #[test]
    fn the_top_k_shares_never_go_backwards() {
        let report = read(&a_queue_with_a_script_elsewhere());
        let c = report.concentration;
        assert!(c.top1_bps <= c.top5_bps);
        assert!(c.top5_bps <= c.top10_bps);
        assert!(c.top10_bps <= 10_000);
    }

    #[test]
    fn the_cap_on_buyers_is_applied_after_the_window_and_by_arrival() {
        let buyers: Vec<OpeningBuyer> = (0..60)
            .map(|n| buyer(&format!("w{n:02}"), SOL, i64::from(n)))
            .collect();
        let report = read(&launch(buyers));
        assert_eq!(report.window.participants, MAX_WALLETS as u32);
        assert_eq!(report.window.considered, 60);
        assert_eq!(report.participants[0].wallet, "w00");
        assert_eq!(report.participants[49].wallet, "w49");
    }

    // -----------------------------------------------------------------------
    // Grouping by size
    // -----------------------------------------------------------------------

    #[test]
    fn identical_positions_are_tagged_as_identical() {
        let report = read(&the_script());
        assert!(report.has_tag(RiskTag::IdenticalSizing));
        assert!(!report.has_tag(RiskTag::NearIdenticalSizing));
        assert_eq!(report.sizing.largest_group, 6);
        assert!(report.sizing.groups[0].exact);
        assert!(!report.sizing.groups[0].round_number);
    }

    #[test]
    fn positions_a_hair_apart_are_near_identical_rather_than_identical() {
        let report = read(&unusual_but_not_primary());
        assert!(report.has_tag(RiskTag::NearIdenticalSizing));
        assert!(!report.has_tag(RiskTag::IdenticalSizing));
        assert!(!report.sizing.groups[0].exact);
    }

    #[test]
    fn a_ladder_of_amounts_does_not_collapse_into_one_group() {
        // Each rung is one percent above the last, which is inside the two
        // percent tolerance. Chained neighbour to neighbour they would be one
        // group of five and the launch would read as scripted; bounded from each
        // group's own smallest member they are three groups, none big enough to
        // tag.
        let report = read(&launch(vec![
            buyer("a", 1_000_000_000, 0),
            buyer("b", 1_010_000_000, 100),
            buyer("c", 1_020_100_000, 200),
            buyer("d", 1_030_301_000, 300),
            buyer("e", 1_040_604_010, 400),
        ]));
        assert!(!report.has_tag(RiskTag::IdenticalSizing));
        assert!(!report.has_tag(RiskTag::NearIdenticalSizing));
        assert_eq!(report.sizing.largest_group, 0);
    }

    #[test]
    fn a_round_number_is_a_repeat_that_happens_honestly() {
        // Three wallets on exactly one SOL. The group is exact, but one SOL is
        // an amount a person types, so the sizing signal is discounted from a
        // full score to three quarters of one.
        let round = read(&launch(
            (1..=3)
                .map(|n| buyer(&format!("w{n}"), SOL, 2_000))
                .collect(),
        ));
        let odd = read(&launch(
            (1..=3)
                .map(|n| buyer(&format!("w{n}"), 777_700_000, 2_000))
                .collect(),
        ));
        assert!(round.sizing.groups[0].round_number);
        assert!(!odd.sizing.groups[0].round_number);
        assert!(round.sizing.score_micros < odd.sizing.score_micros);
    }

    #[test]
    fn the_partition_does_not_depend_on_the_order_the_buyers_arrived_in() {
        let forwards = read(&launch(vec![
            buyer("a", SOL, 0),
            buyer("b", SOL, 0),
            buyer("c", 5 * SOL, 0),
        ]));
        let backwards = read(&launch(vec![
            buyer("c", 5 * SOL, 0),
            buyer("b", SOL, 0),
            buyer("a", SOL, 0),
        ]));
        assert_eq!(forwards.sizing, backwards.sizing);
    }

    // -----------------------------------------------------------------------
    // Bundling
    // -----------------------------------------------------------------------

    #[test]
    fn a_bundle_is_measured_from_its_first_buy_not_the_previous_one() {
        // Eight wallets a tenth of a second apart. Gap to gap, the run never
        // breaks and all eight read as one bundle — a conspiracy that is a
        // queue. From the first buy, it is three runs.
        let report = read(&launch(
            (0..8)
                .map(|n| buyer(&format!("w{n}"), (n as u64 + 1) * SOL, i64::from(n) * 100))
                .collect(),
        ));
        assert_eq!(report.timing.largest_bundle, 3);
        assert_eq!(report.timing.bundles.len(), 2);
    }

    #[test]
    fn a_bundle_with_no_measurable_span_is_one_transaction() {
        let report = read(&the_script());
        assert!(report.timing.same_instant);
        assert_eq!(report.timing.span_ms, Some(0));
        assert!(report.has_tag(RiskTag::SameInstantBundle));
        assert!(!report.has_tag(RiskTag::SubSecondBundle));
    }

    #[test]
    fn a_bundle_spread_over_a_tenth_of_a_second_is_a_crowded_slot() {
        let report = read(&unusual_but_not_primary());
        assert!(!report.timing.same_instant);
        assert_eq!(report.timing.span_ms, Some(150));
        assert!(report.has_tag(RiskTag::SubSecondBundle));
    }

    #[test]
    fn a_bundle_on_the_launch_itself_counts_for_half() {
        let at_launch = read(&launch(
            (1..=3)
                .map(|n| buyer(&format!("w{n}"), 777_700_000, 0))
                .collect(),
        ));
        let later = read(&launch(
            (1..=3)
                .map(|n| buyer(&format!("w{n}"), 777_700_000, 2_000))
                .collect(),
        ));
        assert!(at_launch.timing.launch_block);
        assert!(!later.timing.launch_block);
        assert_eq!(at_launch.timing.score_micros, 166_666);
        assert_eq!(later.timing.score_micros, 333_333);
    }

    // -----------------------------------------------------------------------
    // The deployer
    // -----------------------------------------------------------------------

    #[test]
    fn a_deployer_buying_its_own_launch_is_tagged_and_weighed_lowest() {
        let report = read(&no_bundle_at_all());
        assert!(report.has_tag(RiskTag::CreatorBoughtOwn));
        assert!(report.has_tag(RiskTag::SoloDevDominance));
        assert!(report.has_tag(RiskTag::CreatorExit));
        assert_eq!(report.dev.score_micros, MICROS);
        // Even a perfect deployer signal is worth a fifth of the score.
        assert_eq!(report.confidence_micros, 700_000);
    }

    #[test]
    fn one_wallet_owning_the_opening_is_a_whale_unless_it_is_the_deployer() {
        let whale = read(&launch(vec![
            buyer("big", 80 * SOL, 0),
            buyer("a", 10 * SOL, 100),
            buyer("b", 10 * SOL, 200),
        ]));
        assert!(whale.has_tag(RiskTag::WhaleConcentration));
        assert_eq!(whale.dev.concentration_bps, 8_000);

        let deployer = LaunchRecord {
            creator: Some("big".to_string()),
            ..launch(vec![
                buyer("big", 80 * SOL, 0),
                buyer("a", 10 * SOL, 100),
                buyer("b", 10 * SOL, 200),
            ])
        };
        let deployer = read(&deployer);
        assert!(deployer.has_tag(RiskTag::SoloDevDominance));
        assert!(!deployer.has_tag(RiskTag::WhaleConcentration));
    }

    #[test]
    fn a_creator_outside_the_window_bought_nothing_but_may_still_have_sold() {
        let record = LaunchRecord {
            creator: Some("DEV".to_string()),
            ..launch(vec![
                seller("DEV", 5 * SOL, SOL, 10_000),
                buyer("a", SOL, 0),
                buyer("b", SOL, 100),
                buyer("c", SOL, 200),
            ])
        };
        let report = read(&record);
        assert!(!report.has_tag(RiskTag::CreatorBoughtOwn));
        assert_eq!(report.dev.creator_lamports, 0);
        assert!(report.has_tag(RiskTag::CreatorExit));
    }

    // -----------------------------------------------------------------------
    // Funding
    // -----------------------------------------------------------------------

    #[test]
    fn a_shared_funder_joins_two_wallets_on_its_own() {
        let record = LaunchRecord {
            funding: vec![edge("FUNDER", "a", SOL), edge("FUNDER", "b", SOL)],
            ..launch(vec![buyer("a", 3 * SOL, 0), buyer("b", 7 * SOL, 2_000)])
        };
        let report = read(&record);
        assert!(report.has_tag(RiskTag::SharedFunder));
        let funding = report.funding.as_ref().expect("edges were supplied");
        assert_eq!(funding.linked_wallets, 2);
        assert_eq!(funding.overlap_bps, 10_000);
        assert_eq!(funding.funders.len(), 1);
        assert_eq!(funding.funders[0].hops, 1);
        // Shared funding alone clears the link threshold; nothing else here does.
        assert_eq!(report.clusters.len(), 1);
        assert_eq!(report.clusters[0].reasons, vec![LinkKind::SharedFunder]);
    }

    #[test]
    fn an_exchange_hot_wallet_does_not_make_everyone_related() {
        let mut funding: Vec<FundingEdge> = (0..30)
            .map(|n| edge("EXCHANGE", &format!("stranger{n}"), SOL))
            .collect();
        funding.push(edge("EXCHANGE", "a", SOL));
        funding.push(edge("EXCHANGE", "b", SOL));
        let record = LaunchRecord {
            funding,
            ..launch(vec![buyer("a", 3 * SOL, 0), buyer("b", 7 * SOL, 2_000)])
        };
        let report = read(&record);
        assert!(!report.has_tag(RiskTag::SharedFunder));
        let funding = report.funding.as_ref().expect("edges were supplied");
        assert_eq!(funding.linked_wallets, 0);
        assert_eq!(funding.hubs_ignored, 1);
        assert!(funding.funders.is_empty());
        assert!(report.clusters.is_empty());
    }

    #[test]
    fn one_hop_of_laundering_is_caught_at_depth_two_and_missed_at_depth_one() {
        let record = LaunchRecord {
            funding: vec![
                edge("FUNDER", "mule1", 2 * SOL),
                edge("FUNDER", "mule2", 2 * SOL),
                edge("mule1", "a", SOL),
                edge("mule2", "b", SOL),
            ],
            ..launch(vec![buyer("a", 3 * SOL, 0), buyer("b", 7 * SOL, 2_000)])
        };
        let deep = read(&record);
        assert!(deep.has_tag(RiskTag::SharedFunder));
        assert_eq!(deep.funding.as_ref().expect("edges").funders[0].hops, 2);

        let shallow = analyse_launch(
            &record,
            &ClusterParams {
                funding_depth: 1,
                ..ClusterParams::default()
            },
        );
        assert!(!shallow.has_tag(RiskTag::SharedFunder));
    }

    #[test]
    fn a_record_with_no_funding_data_reports_unknown_rather_than_none_found() {
        let report = read(&the_script());
        assert!(report.funding.is_none());
        for cluster in &report.clusters {
            assert_eq!(cluster.funder_share_bps, None);
            assert_eq!(cluster.temporal_influence_micros, None);
        }
    }

    #[test]
    fn the_funding_score_starts_at_a_half_and_reaches_the_top_at_half_the_opening() {
        // Two of four buyers share a funder: overlap is a half, so the score is
        // already at the ceiling.
        let record = LaunchRecord {
            funding: vec![edge("FUNDER", "a", SOL), edge("FUNDER", "b", SOL)],
            ..launch(vec![
                buyer("a", SOL, 0),
                buyer("b", 2 * SOL, 500),
                buyer("c", 3 * SOL, 1_000),
                buyer("d", 4 * SOL, 1_500),
            ])
        };
        let report = read(&record);
        let funding = report.funding.as_ref().expect("edges");
        assert_eq!(funding.overlap_bps, 5_000);
        assert_eq!(funding.score_micros, MICROS);
    }

    // -----------------------------------------------------------------------
    // Link weights — a change to any of them fails here with a mismatch
    // -----------------------------------------------------------------------

    /// Two wallets, the given evidence between them, and whether they were
    /// joined. Sizes and times are chosen so nothing else links them.
    fn joined_by(sizes: (u64, u64), times: (i64, i64)) -> bool {
        let report = read(&launch(vec![
            buyer("a", sizes.0, times.0),
            buyer("b", sizes.1, times.1),
        ]));
        !report.clusters.is_empty()
    }

    #[test]
    fn a_bundle_is_never_enough_on_its_own() {
        // Landing 100ms apart at sizes a factor of nine apart: `bundle`, 0.35.
        assert!(!joined_by((SOL, 9 * SOL), (2_000, 2_100)));
    }

    #[test]
    fn the_same_instant_is_not_enough_on_its_own() {
        // 10ms apart, sizes a factor of nine apart: `same_instant`, 0.5.
        assert!(!joined_by((SOL, 9 * SOL), (2_000, 2_010)));
    }

    #[test]
    fn an_identical_odd_amount_is_enough_on_its_own() {
        // `identical_size`, 0.6, at exactly the threshold.
        assert!(joined_by((777_700_000, 777_700_000), (0, 2_000)));
    }

    #[test]
    fn an_identical_round_amount_needs_corroboration() {
        // `identical_round_size`, 0.45, alone.
        assert!(!joined_by((SOL, SOL), (0, 2_000)));
        // The same pair landing together: 0.45 + 0.5.
        assert!(joined_by((SOL, SOL), (2_000, 2_010)));
    }

    #[test]
    fn a_near_size_needs_corroboration_too() {
        // `near_size`, 0.35, alone.
        assert!(!joined_by((SOL, 1_010_000_000), (0, 2_000)));
        // With `bundle`, 0.35: 0.7, over the line.
        assert!(joined_by((SOL, 1_010_000_000), (2_000, 2_100)));
    }

    #[test]
    fn the_evidence_inside_a_cluster_is_reported() {
        let report = read(&the_script());
        let cluster = &report.clusters[0];
        assert_eq!(cluster.size, 6);
        assert_eq!(
            cluster.reasons,
            vec![LinkKind::IdenticalSize, LinkKind::SameInstant]
        );
        assert_eq!(cluster.share_of_open_bps, 10_000);
        assert_eq!(report.exposure_bps(), 10_000);
        // Six wallets, one operator, and nobody else in the opening.
        assert_eq!(report.organic_buyers(), 1);
    }

    #[test]
    fn one_link_kind_counts_once_however_many_times_it_is_seen() {
        // Every pair in this six-wallet group is linked by the identical size
        // and by the shared instant, and by nothing else. If a kind could be
        // added twice the weight would be double what it is.
        let report = read(&the_script());
        for relation in &report.relations {
            assert_eq!(
                relation.weight_micros,
                LinkKind::IdenticalSize.weight_micros() + LinkKind::SameInstant.weight_micros()
            );
        }
        assert_eq!(report.relations.len(), 15);
    }

    // -----------------------------------------------------------------------
    // The confidence score
    // -----------------------------------------------------------------------

    #[test]
    fn the_confidence_is_the_documented_weighted_sum() {
        for record in [
            the_script(),
            a_poor_script(),
            a_queue_with_a_script_elsewhere(),
            no_bundle_at_all(),
            a_deployer_buying_alone(),
            unusual_but_not_primary(),
        ] {
            let report = read(&record);
            assert_eq!(
                report.confidence_micros,
                documented_confidence(&report),
                "{} drifted from the weights",
                report.mint
            );
        }
    }

    #[test]
    fn the_script_scores_exactly_what_the_weights_say_it_should() {
        let report = read(&the_script());
        // Six wallets is three past the minimum, so the group strength is at its
        // ceiling; the amount is exact and not round, so the quality is too; one
        // size means no variety at all, so the entropy top-up is its full fifth.
        assert_eq!(report.sizing.entropy_micros, 0);
        assert_eq!(report.sizing.score_micros, MICROS);
        // Same instant, and not the launch block, so neither discount applies.
        assert_eq!(report.timing.score_micros, MICROS);
        assert_eq!(report.dev.score_micros, 0);
        assert_eq!(report.confidence_micros, 800_000);
    }

    #[test]
    fn a_deployer_buying_alone_is_worked_out_by_hand() {
        let report = read(&a_deployer_buying_alone());
        assert_eq!(report.sizing.score_micros, 399_999);
        assert_eq!(report.timing.score_micros, 199_999);
        assert_eq!(report.dev.score_micros, 732_336);
        assert_eq!(report.confidence_micros, 406_465);
    }

    #[test]
    fn a_thin_launch_cannot_score_its_way_past_the_ceiling() {
        // Two wallets out of one funder is the strongest evidence this module
        // has — proof of shared funding is enough on its own — and it would
        // otherwise score 0.9. Two buyers cannot tell coordination from
        // coincidence whatever the evidence looks like, so the ceiling bites.
        let record = LaunchRecord {
            funding: vec![edge("FUNDER", "a", SOL), edge("FUNDER", "b", SOL)],
            ..launch(vec![buyer("a", SOL, 0), buyer("b", SOL, 0)])
        };
        let report = read(&record);
        assert!(report.thin);
        assert_eq!(documented_confidence(&report), THIN_CEILING_MICROS);
        assert_eq!(report.confidence_micros, THIN_CEILING_MICROS);
        // Without the ceiling it would have been nine tenths of the way up.
        let funding = report.funding.as_ref().expect("edges");
        assert_eq!(funding.score_micros, MICROS);
        assert_eq!(report.sizing.score_micros, 200_000);
    }

    #[test]
    fn a_repeated_address_cannot_inflate_the_group_it_landed_in() {
        // The same address recorded twice, once in each of two bundles. The
        // group is read off the larger bundle and may only count the rows that
        // bundle actually holds.
        let record = launch(vec![
            buyer("dup", SOL, 500),
            buyer("a", SOL, 505),
            buyer("b", SOL, 510),
            buyer("dup", SOL, 2_000),
            buyer("c", SOL, 2_005),
        ]);
        let report = read(&record);
        let cohort = coordinated_cohort(&report, Some(BUNDLE_SIZE_TOLERANCE_BPS));
        let bundle = cohort.bundle.as_ref().expect("a bundle");
        assert_eq!(bundle.wallets, 3);
        assert!(cohort.wallets <= bundle.wallets);
        assert_eq!(cohort.wallets, 3);
    }

    // -----------------------------------------------------------------------
    // The gate — one fixture per refusal
    // -----------------------------------------------------------------------

    #[test]
    fn the_rule_enters_a_script() {
        let verdict = gate(&the_script());
        assert_eq!(verdict.reason, GateReason::Accepted);
        assert!(verdict.enter);
        assert_eq!(verdict.cohort_wallets, 6);
        assert_eq!(verdict.cohort_lamports, 6 * 777_700_000);
        assert_eq!(verdict.cohort_external, 6);
        assert_eq!(verdict.cohort_delta_bps, Some(0));
    }

    #[test]
    fn nothing_else_in_the_funnel_enters() {
        let cases = [
            (launch(Vec::new()), GateReason::NoOpeningBuys),
            (
                launch(vec![buyer("a", SOL, 0), buyer("b", SOL, 0)]),
                GateReason::Thin,
            ),
            (
                launch(
                    (1..=3)
                        .map(|n| buyer(&format!("w{n}"), 777_700_000, 2_000))
                        .collect(),
                ),
                GateReason::LowScore,
            ),
            (unusual_but_not_primary(), GateReason::NoPrimarySignal),
            (no_bundle_at_all(), GateReason::NoBundle),
            (a_queue_with_a_script_elsewhere(), GateReason::MixedSizing),
            (a_poor_script(), GateReason::SmallBundle),
        ];
        for (record, expected) in cases {
            let verdict = gate(&record);
            assert_eq!(verdict.reason, expected);
            assert!(!verdict.enter);
        }
    }

    #[test]
    fn a_refusal_still_says_what_the_group_looked_like() {
        let verdict = gate(&a_queue_with_a_script_elsewhere());
        assert_eq!(verdict.reason, GateReason::MixedSizing);
        // Six wallets raced the block; one of them took a size the others did
        // not. Both halves of that sentence are on the verdict.
        assert_eq!(verdict.bundle_wallets, 6);
        assert_eq!(verdict.cohort_wallets, 1);
        assert!(verdict.confidence_micros >= MIN_CLUSTER_SCORE_MICROS);
    }

    #[test]
    fn the_wallet_count_check_is_only_reachable_above_the_analysers_own_minimum() {
        // The analyser will not report a bundle under three, so at the default
        // the check rejects nothing and `no-bundle` fires instead. It is in the
        // gate anyway so the entry rule does not inherit a constant from the
        // analyser.
        let verdict = evaluate(
            &the_script(),
            &ClusterParams::default(),
            &GateParams {
                min_bundle_wallets: 7,
                ..GateParams::default()
            },
            None,
        )
        .1;
        assert_eq!(verdict.reason, GateReason::ThinBundle);
    }

    #[test]
    fn a_deployer_buying_alone_is_not_a_syndicate() {
        // Unreachable at the production threshold: a launch whose only primary
        // tag is `CREATOR_BOUGHT_OWN` has, by construction, neither an exact
        // repeated size nor a same-instant bundle, and without those two it
        // cannot score high enough to be asked the question. So the fixture
        // lowers the threshold, exactly as the prototype's did, and both halves
        // are asserted.
        let record = a_deployer_buying_alone();
        assert_eq!(gate(&record).reason, GateReason::LowScore);

        let reachable = GateParams {
            min_score_micros: 300_000,
            ..GateParams::default()
        };
        let verdict = evaluate(&record, &ClusterParams::default(), &reachable, None).1;
        assert_eq!(verdict.reason, GateReason::SoloDev);
        assert_eq!(verdict.cohort_wallets, 3);
        assert_eq!(verdict.cohort_external, 2);
    }

    #[test]
    fn turning_the_external_check_off_lets_the_deployer_through() {
        let record = a_deployer_buying_alone();
        let verdict = evaluate(
            &record,
            &ClusterParams::default(),
            &GateParams {
                min_score_micros: 300_000,
                require_external_bundle: false,
                min_bundle_lamports: 0,
                ..GateParams::default()
            },
            None,
        )
        .1;
        assert_eq!(verdict.reason, GateReason::Accepted);
    }

    #[test]
    fn the_old_rule_takes_the_queue_the_new_one_turns_away() {
        let record = a_queue_with_a_script_elsewhere();
        let v2 = evaluate(
            &record,
            &ClusterParams::default(),
            &GateParams::default(),
            None,
        )
        .1;
        let v1 = evaluate(&record, &ClusterParams::default(), &GateParams::v1(), None).1;
        assert_eq!(v2.reason, GateReason::MixedSizing);
        assert_eq!(v1.reason, GateReason::Accepted);
        // The rules see the same launch and disagree only about the group.
        assert_eq!(v1.confidence_micros, v2.confidence_micros);
        assert_eq!(v1.bundle_wallets, v2.bundle_wallets);
        assert_eq!(v1.cohort_wallets, 6);
    }

    #[test]
    fn every_reason_the_gate_can_give_is_listed_worst_first() {
        let mut sorted = GateReason::ALL;
        sorted.sort();
        assert_eq!(sorted, GateReason::ALL);
        assert_eq!(
            *GateReason::ALL.last().expect("non-empty"),
            GateReason::Accepted
        );
        for reason in GateReason::ALL {
            assert!(!reason.as_str().is_empty());
        }
    }

    // -----------------------------------------------------------------------
    // The coordinated group
    // -----------------------------------------------------------------------

    #[test]
    fn the_widest_matching_run_wins_not_the_first_one() {
        // Scanning from the smallest position finds two wallets and stops at the
        // gap; the answer is the three sitting above it. A left-anchored sweep
        // gets this wrong, which is why the search is a window.
        let record = launch(vec![
            buyer("a", 1_000_000_000, 2_000),
            buyer("b", 1_005_000_000, 2_001),
            buyer("c", 3_000_000_000, 2_002),
            buyer("d", 3_010_000_000, 2_003),
            buyer("e", 3_020_000_000, 2_004),
        ]);
        let report = read(&record);
        let cohort = coordinated_cohort(&report, Some(BUNDLE_SIZE_TOLERANCE_BPS));
        assert_eq!(cohort.wallets, 3);
        assert_eq!(cohort.size_lamports, Some(3_000_000_000));
        assert_eq!(cohort.lamports, 9_030_000_000);
        // Twenty basis points from end to end of the group.
        assert_eq!(cohort.delta_bps, Some(66));
    }

    #[test]
    fn turning_the_size_test_off_makes_the_whole_bundle_one_group() {
        let report = read(&a_queue_with_a_script_elsewhere());
        assert_eq!(coordinated_cohort(&report, Some(100)).wallets, 1);
        assert_eq!(coordinated_cohort(&report, None).wallets, 6);
    }

    #[test]
    fn no_bundle_means_no_group_rather_than_an_empty_one() {
        let report = read(&no_bundle_at_all());
        let cohort = coordinated_cohort(&report, Some(BUNDLE_SIZE_TOLERANCE_BPS));
        assert!(cohort.bundle.is_none());
        assert_eq!(cohort.wallets, 0);
        assert_eq!(cohort.size_lamports, None);
    }

    #[test]
    fn the_group_is_read_off_the_largest_bundle_earliest_winning_a_tie() {
        // Two bundles of three. The earlier one is the group, and its members
        // are the ones the cohort measures.
        let record = launch(vec![
            buyer("a", SOL, 500),
            buyer("b", SOL, 505),
            buyer("c", SOL, 510),
            buyer("x", 4 * SOL, 2_000),
            buyer("y", 4 * SOL, 2_005),
            buyer("z", 4 * SOL, 2_010),
        ]);
        let report = read(&record);
        let cohort = coordinated_cohort(&report, Some(BUNDLE_SIZE_TOLERANCE_BPS));
        let bundle = cohort.bundle.as_ref().expect("a bundle");
        assert_eq!(bundle.at_ms, 500);
        assert_eq!(cohort.lamports, 3 * SOL);
    }

    // -----------------------------------------------------------------------
    // The stored row
    // -----------------------------------------------------------------------

    #[test]
    fn a_cluster_with_an_unknown_metric_writes_no_row() {
        let report = read(&the_script());
        let cluster = &report.clusters[0];
        assert_eq!(cluster.temporal_influence_micros, None);
        assert_eq!(cluster.metrics_with(500_000), None);
    }

    #[test]
    fn a_cluster_that_knows_everything_assembles_the_row() {
        let record = LaunchRecord {
            funding: vec![
                edge("FUNDER", "a", 5 * SOL),
                edge("FUNDER", "b", 5 * SOL),
                edge("a", "b", SOL),
                edge("b", "a", SOL),
            ],
            ..launch(vec![buyer("a", 4 * SOL, 0), buyer("b", 4 * SOL, 0)])
        };
        let report = read(&record);
        let cluster = &report.clusters[0];
        assert_eq!(cluster.holding_hhi_bps, Some(5_000));
        assert_eq!(cluster.sync_micros, Some(MICROS));
        assert_eq!(cluster.funder_share_bps, Some(10_000));
        assert_eq!(cluster.temporal_influence_micros, Some(MICROS));
        assert_eq!(cluster.interaction_entropy_micros, Some(MICROS));

        let metrics = cluster.metrics_with(500_000).expect("every input is known");
        assert!(metrics.is_normalised());
        assert!(metrics.is_measurable());
        assert_eq!(metrics.wallet_count, 2);
        assert_eq!(metrics.holding_hhi_bps, 5_000);
        assert_eq!(metrics.temporal_influence_micros, MICROS as u32);
        assert_eq!(metrics.spectral_separation_micros, 500_000);
        assert_eq!(metrics.interaction_entropy_micros, MICROS as u32);
    }

    #[test]
    fn the_synchrony_kernel_lands_on_the_published_vector() {
        // §14: buys at 0, 0.5 and 1.0 seconds with tau at five seconds give
        // 0.8761. The wallets are joined by an identical odd size alone, so the
        // cluster exists without the times touching it.
        let report = read(&launch(vec![
            buyer("a", 777_700_000, 0),
            buyer("b", 777_700_000, 500),
            buyer("c", 777_700_000, 1_000),
        ]));
        let cluster = &report.clusters[0];
        assert_eq!(cluster.size, 3);
        assert_eq!(cluster.sync_micros, Some(876_135));
    }

    #[test]
    fn a_funder_behind_half_a_cluster_is_reported_as_half() {
        // Four equal wallets joined by an identical odd size. Two of them trace
        // back to one funder and two to another, so no funder owns more than a
        // half of the cluster's money.
        let record = LaunchRecord {
            funding: vec![
                edge("F1", "a", SOL),
                edge("F1", "b", SOL),
                edge("F2", "c", SOL),
                edge("F2", "d", SOL),
            ],
            ..launch(
                ["a", "b", "c", "d"]
                    .iter()
                    .enumerate()
                    .map(|(n, w)| buyer(w, 777_700_000, n as i64 * 400))
                    .collect(),
            )
        };
        let report = read(&record);
        let cluster = &report.clusters[0];
        assert_eq!(cluster.size, 4);
        assert_eq!(cluster.funder_share_bps, Some(5_000));
    }

    // -----------------------------------------------------------------------
    // Determinism — property P9
    // -----------------------------------------------------------------------

    #[test]
    fn two_runs_of_one_record_produce_the_same_bytes() {
        let record = a_queue_with_a_script_elsewhere();
        let first = serde_json::to_string(&read(&record)).expect("serialisable");
        let second = serde_json::to_string(&read(&record)).expect("serialisable");
        assert_eq!(first, second);
    }

    #[test]
    fn reshuffling_the_input_does_not_move_a_single_number() {
        let mut record = LaunchRecord {
            funding: vec![
                edge("FUNDER", "w3", SOL),
                edge("FUNDER", "w5", SOL),
                edge("mule", "w1", SOL),
                edge("FUNDER", "mule", 2 * SOL),
            ],
            ..a_queue_with_a_script_elsewhere()
        };
        let straight = serde_json::to_string(&read(&record)).expect("serialisable");

        record.buyers.reverse();
        record.funding.reverse();
        let reversed = serde_json::to_string(&read(&record)).expect("serialisable");
        assert_eq!(straight, reversed);

        // And a rotation, which is neither the original order nor its reverse.
        record.buyers.rotate_left(3);
        record.funding.rotate_left(2);
        let rotated = serde_json::to_string(&read(&record)).expect("serialisable");
        assert_eq!(straight, rotated);
    }

    #[test]
    fn a_report_survives_a_round_trip_through_json() {
        let report = read(&unusual_but_not_primary());
        let text = serde_json::to_string(&report).expect("serialisable");
        let back: ClusterReport = serde_json::from_str(&text).expect("readable");
        assert_eq!(report, back);
    }

    #[test]
    fn the_tag_vocabulary_serialises_in_the_spelling_the_corpus_uses() {
        let text = serde_json::to_string(&RiskTag::SameInstantBundle).expect("serialisable");
        assert_eq!(text, "\"SAME_INSTANT_BUNDLE\"");
        assert_eq!(RiskTag::SameInstantBundle.as_str(), "SAME_INSTANT_BUNDLE");
        let reason = serde_json::to_string(&GateReason::NoPrimarySignal).expect("serialisable");
        assert_eq!(reason, "\"no-primary-signal\"");
        assert_eq!(GateReason::NoPrimarySignal.as_str(), "no-primary-signal");
    }

    // -----------------------------------------------------------------------
    // Properties P7, P8 and P13 — nothing panics, nothing leaves its range
    // -----------------------------------------------------------------------

    /// A fixed-increment generator, seeded by the case number. Not a good random
    /// source and does not need to be: what it has to be is the same sequence on
    /// every machine, so a failure is reproducible from the seed alone.
    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 11
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    fn adversarial_record(seed: u64) -> LaunchRecord {
        let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1));
        let names = ["a", "b", "c", "d", "e", "dup", "dup"];
        let amounts = [
            0u64,
            1,
            50_000_000,
            SOL,
            777_700_000,
            u64::MAX / 2,
            u64::MAX,
        ];
        let times = [i64::MIN, -1_000, 0, 1, 20, 250, 1_600, 3_000, i64::MAX];

        let buyers = (0..rng.below(13))
            .map(|_| OpeningBuyer {
                wallet: names[rng.below(names.len() as u64) as usize].to_string(),
                sol_in_lamports: amounts[rng.below(amounts.len() as u64) as usize],
                sol_out_lamports: amounts[rng.below(amounts.len() as u64) as usize],
                tx_count: rng.below(5) as u32,
                first_seen_ms: times[rng.below(times.len() as u64) as usize],
            })
            .collect();

        let funding = (0..rng.below(9))
            .map(|_| {
                let all = ["a", "b", "c", "d", "e", "dup", "FUNDER", "mule", ""];
                FundingEdge {
                    from: all[rng.below(all.len() as u64) as usize].to_string(),
                    to: all[rng.below(all.len() as u64) as usize].to_string(),
                    lamports: amounts[rng.below(amounts.len() as u64) as usize],
                }
            })
            .collect();

        LaunchRecord {
            mint: "MINT".to_string(),
            creator: Some(names[rng.below(names.len() as u64) as usize].to_string()),
            buyers,
            funding,
        }
    }

    // -----------------------------------------------------------------------
    // Concentration: RISK_AND_SYBIL_SPEC.md sections 2.2, 2.3 and 14
    // -----------------------------------------------------------------------

    /// The population §14 publishes both numbers for, in one place.
    ///
    /// Two wallets at nine to one is an HHI of 8 200 and an entropy of 0.4690,
    /// and those are exactly the two constants this module refuses on. The
    /// entropy lands four millionths *inside* its threshold rather than on it,
    /// because `RING_ENTROPY_MICROS` is §14's figure rounded to four places and
    /// the population that produced it is a hair under. Asserting the margin
    /// rather than the rounding is what makes a change to either one loud.
    #[test]
    fn the_published_population_is_the_shape_the_thresholds_name() {
        let report = read(&launch(vec![
            buyer("whale", 9 * SOL, 2_000),
            buyer("dust", SOL, 2_000),
        ]));
        let concentration = report.concentration;
        assert_eq!(concentration.hhi_bps, Some(RING_HHI_BPS));
        assert_eq!(concentration.hhi_bps, Some(8_200));

        let entropy = concentration.entropy_micros.expect("two buyers");
        assert_eq!((entropy + 50) / 100, 4_690, "rounds to §14's 0.4690");
        assert!(
            entropy <= RING_ENTROPY_MICROS,
            "the published population has to be inside its own threshold: \
             {entropy} against {RING_ENTROPY_MICROS}",
        );
        assert_eq!(RING_ENTROPY_MICROS - entropy, 4, "and by four millionths");
    }

    /// §14's HHI table, every row of it, through the column the gate reads.
    #[test]
    fn the_published_index_table_reproduces() {
        let cases: [(&[u64], Option<u16>); 7] = [
            (&[100], Some(10_000)),
            (&[50, 50], Some(5_000)),
            (&[25, 25, 25, 25], Some(2_500)),
            (&[10, 10, 10, 10, 10, 10, 10, 10, 10, 10], Some(1_000)),
            (&[90, 10], Some(8_200)),
            (&[0, 0, 0], None),
            (&[], None),
        ];
        for (population, want) in cases {
            let record = launch(
                population
                    .iter()
                    .enumerate()
                    .map(|(n, &share)| buyer(&format!("h{n}"), share * SOL / 100, 2_000))
                    .collect(),
            );
            assert_eq!(
                read(&record).concentration.hhi_bps,
                want,
                "population {population:?}",
            );
        }

        // One hundred equal holders is the last row, and it needs its own record
        // because the analyser caps the window at fifty buyers.
        let hundred: Vec<u64> = vec![SOL; 100];
        assert_eq!(hhi_bps(&hundred), Some(100));
    }

    /// §2.3's entropy next to the index, on the same population.
    #[test]
    fn the_published_entropy_table_reproduces() {
        let equal_pair = read(&launch(vec![
            buyer("a", SOL, 2_000),
            buyer("b", SOL, 2_000),
        ]));
        assert_eq!(equal_pair.concentration.entropy_micros, Some(MICROS));

        let four_equal = read(&launch(
            (1..=4)
                .map(|n| buyer(&format!("h{n}"), SOL, 2_000))
                .collect(),
        ));
        assert_eq!(four_equal.concentration.entropy_micros, Some(MICROS));

        // §14's `[1.0]` row is a defined zero and this column is not it: one row
        // on the record is one row, not proof that one address took everything.
        let alone = read(&launch(vec![buyer("only", SOL, 2_000)]));
        assert_eq!(alone.concentration.entropy_micros, None);
        assert_eq!(alone.concentration.hhi_bps, Some(10_000));

        // And an opening nobody bought has neither.
        let empty = read(&launch(Vec::new()));
        assert_eq!(empty.concentration.entropy_micros, None);
        assert_eq!(empty.concentration.hhi_bps, None);
    }

    /// The two halves of the ring test are read off one slice, so they cannot
    /// disagree about which population they measured.
    #[test]
    fn a_cluster_carries_both_readings_of_its_own_money() {
        let report = read(&a_coordinated_ring());
        let cluster = report.clusters.first().expect("one cluster");
        assert_eq!(cluster.size, 7);
        let hhi = cluster.holding_hhi_bps.expect("balances");
        let entropy = cluster.holding_entropy_micros.expect("seven members");
        assert!(hhi >= RING_HHI_BPS, "index {hhi}");
        assert!(entropy <= RING_ENTROPY_MICROS, "entropy {entropy}");
    }

    /// The group the rule was built to follow is not a ring, and must not be.
    #[test]
    fn six_wallets_that_each_hold_something_are_not_a_ring() {
        let report = read(&the_script());
        let cluster = report.clusters.first().expect("one cluster");
        // Six equal positions: 10 000 / 6, rounded to nearest as §2.2 requires.
        assert_eq!(cluster.holding_hhi_bps, Some(1_667));
        assert_eq!(cluster.holding_entropy_micros, Some(MICROS));
        assert!(report
            .rings(
                Some(RING_HHI_BPS),
                Some(RING_ENTROPY_MICROS),
                MIN_BUNDLE_LAMPORTS,
            )
            .is_empty());
        assert_eq!(gate(&the_script()).reason, GateReason::Accepted);
    }

    /// Both instruments have to agree, and each one is deaf where the other
    /// hears. A whale with a handful of near-empty friends clears the index and
    /// a whale with a genuine crowd behind it does not clear the entropy.
    #[test]
    fn one_reading_on_its_own_is_not_a_ring() {
        let spread_tail = Cluster {
            holding_hhi_bps: Some(RING_HHI_BPS),
            holding_entropy_micros: Some(RING_ENTROPY_MICROS + 1),
            ..a_cluster()
        };
        assert!(spread_tail
            .ring_finding(Some(RING_HHI_BPS), Some(RING_ENTROPY_MICROS), 0)
            .is_none());

        let spread_money = Cluster {
            holding_hhi_bps: Some(RING_HHI_BPS - 1),
            holding_entropy_micros: Some(RING_ENTROPY_MICROS),
            ..a_cluster()
        };
        assert!(spread_money
            .ring_finding(Some(RING_HHI_BPS), Some(RING_ENTROPY_MICROS), 0)
            .is_none());

        // Exactly on both is a ring: the thresholds are the published shape, and
        // a launch that reproduces it is the case they were written for.
        let on_the_nose = Cluster {
            holding_hhi_bps: Some(RING_HHI_BPS),
            holding_entropy_micros: Some(RING_ENTROPY_MICROS),
            ..a_cluster()
        };
        assert!(on_the_nose
            .ring_finding(Some(RING_HHI_BPS), Some(RING_ENTROPY_MICROS), 0)
            .is_some());
    }

    /// Turning a half off widens the test rather than narrowing it, and turning
    /// both off silences it.
    #[test]
    fn a_threshold_of_none_turns_its_own_half_off() {
        let whale_and_a_crowd = Cluster {
            holding_hhi_bps: Some(RING_HHI_BPS),
            holding_entropy_micros: Some(MICROS),
            ..a_cluster()
        };
        assert!(whale_and_a_crowd
            .ring_finding(Some(RING_HHI_BPS), Some(RING_ENTROPY_MICROS), 0)
            .is_none());
        assert!(whale_and_a_crowd
            .ring_finding(Some(RING_HHI_BPS), None, 0)
            .is_some());
        assert!(whale_and_a_crowd.ring_finding(None, None, 0).is_none());

        // An entropy nobody could measure does not corroborate and does not
        // stand in for the check.
        let unmeasured = Cluster {
            holding_hhi_bps: Some(RING_HHI_BPS),
            holding_entropy_micros: None,
            ..a_cluster()
        };
        assert!(unmeasured
            .ring_finding(Some(RING_HHI_BPS), Some(RING_ENTROPY_MICROS), 0)
            .is_none());
        assert!(unmeasured
            .ring_finding(Some(RING_HHI_BPS), None, 0)
            .is_some());
    }

    /// The refusal, end to end, on a launch the rest of the rule liked.
    #[test]
    fn the_rule_turns_away_a_launch_whose_group_is_one_wallet() {
        // Same six wallets, same instant, same identical size: without the whale
        // this is the launch the rule enters.
        assert_eq!(gate(&the_script()).reason, GateReason::Accepted);

        let verdict = gate(&a_coordinated_ring());
        assert_eq!(verdict.reason, GateReason::CoordinatedRing);
        assert!(!verdict.enter);

        // And it got there on the merits: the group checks all passed first.
        assert_eq!(verdict.cohort_wallets, 6);
        assert!(verdict.cohort_lamports >= MIN_BUNDLE_LAMPORTS);

        let ring = verdict.rings.first().expect("one ring");
        assert_eq!(ring.cluster_id, "c1");
        assert_eq!(ring.wallets, 7);
        assert!(ring.material);
        assert!(ring.holding_hhi_bps >= RING_HHI_BPS);
        assert!(ring.holding_entropy_micros.expect("measured") <= RING_ENTROPY_MICROS);
    }

    /// A ring too small to move the price is reported and not acted on.
    #[test]
    fn a_ring_that_committed_nothing_is_named_and_not_refused_on() {
        let record = a_tiny_ring();
        let report = read(&record);
        let rings = report.rings(
            Some(RING_HHI_BPS),
            Some(RING_ENTROPY_MICROS),
            MIN_BUNDLE_LAMPORTS,
        );
        let ring = rings.first().expect("still a ring");
        assert!(!ring.material, "{} lamports", ring.lamports);

        // The launch is turned away on the group's commitment, which is the
        // check that comes first, and the ring is on the verdict either way.
        let verdict = gate(&record);
        assert_eq!(verdict.reason, GateReason::SmallBundle);
        assert_eq!(verdict.rings.len(), 1);
    }

    /// Every refusal carries the ring column, not just the ones that got far
    /// enough to be measured on it.
    #[test]
    fn a_launch_turned_away_early_still_reports_what_its_clusters_looked_like() {
        let verdict = gate(&launch(Vec::new()));
        assert_eq!(verdict.reason, GateReason::NoOpeningBuys);
        assert!(verdict.rings.is_empty());
        assert_eq!(verdict.sandwich, None);
    }

    /// The v1 rule predates all of this and has to keep answering what it did.
    #[test]
    fn the_old_rule_does_not_inherit_the_ring_check() {
        let record = a_coordinated_ring();
        assert_eq!(gate(&record).reason, GateReason::CoordinatedRing);
        let v1 = evaluate(&record, &ClusterParams::default(), &GateParams::v1(), None).1;
        assert_eq!(v1.reason, GateReason::Accepted);
        assert!(v1.rings.is_empty());
    }

    // -----------------------------------------------------------------------
    // The sandwich guard: REPLAY_AND_SIMULATION_SPEC.md section 15.2
    // -----------------------------------------------------------------------

    /// §15.2's table of the smallest buy worth front-running, at three points on
    /// the curve. These are the numbers the guard is enforcing.
    #[test]
    fn the_published_breakeven_sizes_reproduce() {
        // `y` in SOL, and §15.2's minimum victim buy for it: 0.3061, 0.7652 and
        // 1.1733 SOL, to the lamport.
        let cases = [
            (30u64, 306_091_216u64),
            (75, 765_228_038),
            (115, 1_173_349_659),
        ];
        for (reserve_sol, breakeven) in cases {
            let reserve = reserve_sol * SOL;
            let check = SandwichCheck::of(&EntryQuote::public(0, reserve));
            assert_eq!(check.breakeven_lamports, breakeven, "y = {reserve_sol} SOL");

            // The figure is rounded up, so it is the smallest buy that is over
            // the line and the lamport under it is the largest that is not.
            // §15.2 says the true edge at the threshold is zero and the last
            // lamport is decided by rounding, so the assertion is on the two
            // sides of it rather than on it.
            let over = SandwichCheck::of(&EntryQuote::public(breakeven, reserve));
            let under = SandwichCheck::of(&EntryQuote::public(breakeven - 1, reserve));
            assert!(over.above_threshold, "y = {reserve_sol} SOL, at breakeven");
            assert!(!under.above_threshold, "y = {reserve_sol} SOL, one under");
        }
    }

    /// The guard refuses our own order, not the launch, and only on the route
    /// the model describes.
    #[test]
    fn a_public_buy_worth_front_running_does_not_go_out() {
        let launch_reserve = 30 * SOL;
        let script = the_script();
        assert_eq!(gate(&script).reason, GateReason::Accepted);

        // Half a SOL at the launch is over §15.2's 0.3061 threshold.
        let verdict = gate_quoted(&script, &EntryQuote::public(SOL / 2, launch_reserve));
        assert_eq!(verdict.reason, GateReason::SandwichRisk);
        assert!(!verdict.enter);
        let check = verdict.sandwich.expect("a quote was priced");
        assert!(check.above_threshold);
        assert!(check.refuses());
        assert_eq!(check.breakeven_lamports, 306_091_216);

        // A tenth of a SOL is under it, and the same launch trades.
        let small = gate_quoted(&script, &EntryQuote::public(SOL / 10, launch_reserve));
        assert_eq!(small.reason, GateReason::Accepted);
        assert!(!small.sandwich.expect("still priced").above_threshold);

        // The same order deeper in the curve is under the threshold as well,
        // because the threshold is a share of the reserve rather than a size.
        let deep = gate_quoted(&script, &EntryQuote::public(SOL / 2, 115 * SOL));
        assert_eq!(deep.reason, GateReason::Accepted);
    }

    /// A send nobody can read first is outside what §15.1 models, so it is
    /// priced and reported and not refused.
    #[test]
    fn a_private_bundle_is_measured_and_let_through() {
        let verdict = gate_quoted(&the_script(), &EntryQuote::private(SOL / 2, 30 * SOL));
        assert_eq!(verdict.reason, GateReason::Accepted);
        let check = verdict.sandwich.expect("priced anyway");
        assert!(
            check.above_threshold,
            "the exposure is still there to price a tip against",
        );
        assert!(!check.refuses());
    }

    /// "Nobody quoted this" and "this quote is fine" are different answers, and
    /// which one blocks is policy.
    #[test]
    fn a_launch_with_no_curve_behind_it_is_a_setting_not_a_pass() {
        let script = the_script();
        assert_eq!(gate(&script).reason, GateReason::Accepted);
        assert_eq!(gate(&script).sandwich, None);

        let required = GateParams {
            sandwich_guard: SandwichGuard::Required,
            ..GateParams::default()
        };
        let unquoted = evaluate(&script, &ClusterParams::default(), &required, None).1;
        assert_eq!(unquoted.reason, GateReason::NoCurveQuote);
        assert!(!unquoted.enter);

        let quoted = evaluate(
            &script,
            &ClusterParams::default(),
            &required,
            Some(&EntryQuote::public(SOL / 10, 30 * SOL)),
        )
        .1;
        assert_eq!(quoted.reason, GateReason::Accepted);

        // Off does not price the order at all, however big it is.
        let off = GateParams {
            sandwich_guard: SandwichGuard::Off,
            ..GateParams::default()
        };
        let ignored = evaluate(
            &script,
            &ClusterParams::default(),
            &off,
            Some(&EntryQuote::public(20 * SOL, 30 * SOL)),
        )
        .1;
        assert_eq!(ignored.reason, GateReason::Accepted);
        assert_eq!(ignored.sandwich, None);
    }

    /// The guard is the last thing asked, so a launch that was never going to
    /// trade is refused for the reason that is about the launch.
    #[test]
    fn the_curve_is_asked_after_the_launch_has_earned_the_question() {
        let verdict = gate_quoted(&a_poor_script(), &EntryQuote::public(20 * SOL, 30 * SOL));
        assert_eq!(verdict.reason, GateReason::SmallBundle);
        // And the number is still on the verdict for the funnel to read.
        assert!(verdict.sandwich.expect("priced").above_threshold);
    }

    /// The funnel's reason list and the checks the gate actually runs are the
    /// same set, in the same order.
    #[test]
    fn every_reason_the_gate_can_give_is_in_the_published_order() {
        assert_eq!(GateReason::ALL.len(), 13);
        let mut sorted = GateReason::ALL;
        sorted.sort();
        assert_eq!(sorted, GateReason::ALL, "ALL is worst-first and total");
        assert_eq!(GateReason::CoordinatedRing.as_str(), "coordinated-ring");
        assert_eq!(GateReason::NoCurveQuote.as_str(), "no-curve-quote");
        assert_eq!(GateReason::SandwichRisk.as_str(), "sandwich-risk");
    }

    #[test]
    fn nothing_panics_and_nothing_leaves_its_range() {
        for seed in 0..400u64 {
            let record = adversarial_record(seed);
            let (report, verdict) = evaluate(
                &record,
                &ClusterParams::default(),
                &GateParams::default(),
                None,
            );

            assert!(report.confidence_micros <= MICROS, "seed {seed}");
            assert!(report.sizing.score_micros <= MICROS, "seed {seed}");
            assert!(report.sizing.entropy_micros <= MICROS, "seed {seed}");
            assert!(report.timing.score_micros <= MICROS, "seed {seed}");
            assert!(report.dev.score_micros <= MICROS, "seed {seed}");
            assert!(report.dev.creator_share_bps <= 10_000, "seed {seed}");
            assert!(report.dev.concentration_bps <= 10_000, "seed {seed}");
            if let Some(funding) = &report.funding {
                assert!(funding.score_micros <= MICROS, "seed {seed}");
                assert!(funding.overlap_bps <= 10_000, "seed {seed}");
            }

            let concentration = report.concentration;
            assert!(concentration.hhi_bps.is_none_or(|hhi| hhi <= 10_000));
            assert!(
                concentration.top1_bps <= concentration.top5_bps,
                "seed {seed}"
            );
            assert!(
                concentration.top5_bps <= concentration.top10_bps,
                "seed {seed}"
            );
            assert!(concentration.top10_bps <= 10_000, "seed {seed}");

            for cluster in &report.clusters {
                assert!(cluster.size >= 2, "seed {seed}");
                assert_eq!(cluster.members.len(), cluster.size as usize, "seed {seed}");
                assert!(cluster.share_of_open_bps <= 10_000, "seed {seed}");
                assert!(cluster.holding_hhi_bps.is_none_or(|hhi| hhi <= 10_000));
                assert!(cluster.sync_micros.is_none_or(|s| s <= MICROS));
                assert!(cluster.funder_share_bps.is_none_or(|f| f <= 10_000));
                assert!(cluster
                    .temporal_influence_micros
                    .is_none_or(|t| t <= MICROS));
                assert!(cluster
                    .interaction_entropy_micros
                    .is_none_or(|e| e <= MICROS));
                // P7: no field of a built row is ever NaN or outside [0, 1].
                if let Some(metrics) = cluster.metrics_with(MICROS / 3) {
                    assert!(metrics.is_normalised(), "seed {seed}");
                }
            }

            assert!(
                report.organic_buyers() <= report.window.participants,
                "seed {seed}"
            );
            assert_eq!(
                verdict.enter,
                verdict.reason == GateReason::Accepted,
                "seed {seed}"
            );
            assert!(
                verdict.cohort_wallets <= verdict.bundle_wallets,
                "seed {seed}"
            );
        }
    }

    #[test]
    fn a_launch_of_one_amount_repeated_by_the_maximum_wallets_still_behaves() {
        let buyers: Vec<OpeningBuyer> = (0..MAX_WALLETS)
            .map(|n| buyer(&format!("w{n:02}"), u64::MAX / 64, 2_000))
            .collect();
        let report = read(&launch(buyers));
        assert_eq!(report.window.participants, MAX_WALLETS as u32);
        assert_eq!(report.confidence_micros, 800_000);
        assert_eq!(report.clusters.len(), 1);
        assert_eq!(report.concentration.hhi_bps, Some(200));
    }
}
