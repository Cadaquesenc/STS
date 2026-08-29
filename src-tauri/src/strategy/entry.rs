//! From "this launch is one operator" to "this is the position, and this is the
//! way out of it".
//!
//! [`crate::strategy::syndicate`] decides whether a launch is worth following.
//! It says nothing about how much, and that is deliberate: the analyser is a
//! function of the launch and the entry is a function of the account, the pool,
//! the mode the engine is in and what a bad day would cost. This module is the
//! second half, and it is `RISK_AND_SYBIL_SPEC.md` §10 plus the confidence tiers
//! from `STS_CORE_IDEOLOGY.md` §5, in integers.
//!
//! Five ideas hold it together.
//!
//! **Size is the minimum of a chain of caps, and the chain says which one
//! bound.** Risk budget, pool participation, fast-path allowance, operator
//! limit, free equity — the smallest wins, and [`SizeCaps::binding`] names it.
//! A size without that name is a number nobody can argue with, and every one of
//! these caps exists because somebody will want to argue with it.
//!
//! **The loss is modelled before the size is chosen.** `stressed_loss_bps` is
//! the worst of the −30%/−50% gap buckets crossed with the 10/15/20/25%
//! slippage buckets, priced against the actual curve rather than assumed. §10 is
//! explicit that this is the worst modelled loss and not the expected one:
//! sizing off an expected loss sizes for the day that does not need risk
//! control.
//!
//! **Nothing is entered that cannot be left.** Phase 2's fifth acceptance
//! criterion wants a precomputed emergency exit on every candidate, so the exit
//! is quoted at the size being entered, before the entry is agreed, and a
//! position whose exit does not price — or prices past
//! [`crate::execution::EMERGENCY_MAX_SLIPPAGE_BPS`] — is refused. That is the
//! liveness invariant applied at the only moment it is cheap: before there is
//! anything to be stuck in.
//!
//! **The story can only ever make the position smaller.** The social weight
//! from [`crate::strategy::social`] enters the chain as a multiplier that is
//! never above one, after the tier and before Gate 6D's ceiling. That ordering
//! is the point: a story cannot lift a candidate into a tier, cannot clear a
//! hard block, and cannot widen a cap — it can only take size off one that
//! every other rule already allowed. `STS_CORE_IDEOLOGY.md` §1 requires that,
//! and the archived grading in `docs/archive/Log.md` is why it costs nothing to
//! obey: measured against a matched crowd size, a launch's story predicted
//! nothing in either direction.
//!
//! **The edge is an input and its default is zero.** The governing equation
//! needs `P(win)` and this system does not have a calibrated one — the
//! prototype's own measurement of the syndicate thesis was 22 trades and
//! −17.95%, recorded in `docs/archive/Log.md`. So [`EntryParams::edge_lcb_bps`]
//! is policy, it defaults to zero, and a zero edge makes the stressed expectancy
//! negative and refuses every entry. That is the roadmap's Phase 3 gate written
//! as code rather than as a comment: no envelope is signable until a positive
//! out-of-sample stressed EV lower bound exists to put in this field. It is the
//! same shape as `execution.rs` shipping a signer trait whose only
//! implementation is an honest mock.
//!
//! Nothing here signs, sends, or touches the network. The output is a plan, and
//! a plan that has expired cannot be acted on — see
//! [`EntryDecision::is_signable`].

use serde::{Deserialize, Serialize};

use crate::backtest::{mul_div_floor, MICROS};
use crate::execution::EMERGENCY_MAX_SLIPPAGE_BPS;
use crate::replay::{CurveState, BPS_DENOMINATOR, LAMPORTS_PER_SOL};
use crate::strategy::social::{weigh, SocialParams, SocialScan, SocialWeight};
use crate::strategy::syndicate::{
    analyse_launch, syndicate_gate, ClusterParams, ClusterReport, EntryQuote, GateParams,
    GateVerdict, LaunchRecord,
};
use crate::types::RiskSnapshot;

// ===========================================================================
// Policy constants
// ===========================================================================

/// Confidence at or above which a candidate is Tier 1. `0.85` in millionths.
pub const TIER_ONE_MICROS: u64 = 850_000;
/// Confidence at or above which a candidate is Tier 2. `0.70`.
pub const TIER_TWO_MICROS: u64 = 700_000;
/// Confidence at or above which a candidate is Tier 3. `0.55`. Below it there is
/// nothing to size.
pub const TIER_THREE_MICROS: u64 = 550_000;

/// The gap buckets the stress set is priced over, in basis points.
/// `STS_ROADMAP.md` Phase 3: −30% and −50%.
pub const GAP_BUCKETS_BPS: [u16; 2] = [3_000, 5_000];

/// The slippage buckets, in basis points. Phase 3: 10, 15, 20 and 25%.
pub const SLIPPAGE_BUCKETS_BPS: [u16; 4] = [1_000, 1_500, 2_000, 2_500];

/// The 1.5% executable-liquidity participation cap from doctrine.
///
/// Re-exported rather than restated. `types::MAX_POOL_SHARE_BPS` is the one
/// place the number lives now, and a second literal here would be the fourth
/// copy of a constant this sprint spent a commit reducing to one.
///
/// [`plan_entry`] still takes the **tighter** of it and whatever the risk
/// snapshot arrived with. Not because the two disagree today — they do not —
/// but because a snapshot is data reaching the sizer from outside it, and the
/// direction a cap is allowed to move under data is downwards only.
pub use crate::types::MAX_POOL_SHARE_BPS;

/// Below this a round trip costs more than the trade is worth. 0.01 SOL.
pub const MIN_NOTIONAL_LAMPORTS: u64 = LAMPORTS_PER_SOL / 100;

/// Gate 6D's ceiling: 0.05 SOL, whatever anything above computed.
///
/// On by default because it is the tightest gate the roadmap has authorised and
/// no later one has been reached. A backtest that wants the unbounded size sets
/// it to `u64::MAX` and says so in its configuration, which is the point of it
/// being a field.
pub const MICRO_LIVE_CAP_LAMPORTS: u64 = LAMPORTS_PER_SOL / 20;

/// The default bound on an entry fill. Phase 4's "1–3% default slippage bounds",
/// at the top of that range because a snipe that misses is a wasted fee and a
/// snipe that fills badly is a position.
pub const ENTRY_MAX_SLIPPAGE_BPS: u16 = 300;

/// How long a decision stays actionable. A decision made on a snapshot four
/// slots old is a decision about a market that has moved.
pub const DECISION_TTL_MS: i64 = 1_500;

/// How old the risk snapshot may be when the decision is made. One slot.
pub const MAX_SNAPSHOT_AGE_MS: i64 = 400;

// ===========================================================================
// Tiers
// ===========================================================================

/// What the confidence buys, from `STS_CORE_IDEOLOGY.md` §5 and
/// `RISK_AND_SYBIL_SPEC.md` §10.
///
/// A tier reduces size. It never grants permission — every hard block is checked
/// before a tier is consulted, and no tier can clear one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Tier {
    /// `>= 0.85`. Full size, subject to every cap.
    One,
    /// `0.70 – 0.849`. Half.
    Two,
    /// `0.55 – 0.699`. A tenth, and never automatically with real capital.
    Three,
    /// `< 0.55`. Watch it.
    ObserveOnly,
}

impl Tier {
    /// Worst first.
    pub const ALL: [Tier; 4] = [Tier::ObserveOnly, Tier::Three, Tier::Two, Tier::One];

    pub const fn as_str(self) -> &'static str {
        match self {
            Tier::One => "tier-1",
            Tier::Two => "tier-2",
            Tier::Three => "tier-3",
            Tier::ObserveOnly => "observe-only",
        }
    }

    /// Which tier a confidence lands in.
    pub const fn for_confidence(confidence_micros: u64) -> Tier {
        if confidence_micros >= TIER_ONE_MICROS {
            Tier::One
        } else if confidence_micros >= TIER_TWO_MICROS {
            Tier::Two
        } else if confidence_micros >= TIER_THREE_MICROS {
            Tier::Three
        } else {
            Tier::ObserveOnly
        }
    }

    /// What this tier multiplies the capped size by, in basis points.
    pub const fn multiplier_bps(self) -> u16 {
        match self {
            Tier::One => BPS_DENOMINATOR as u16,
            Tier::Two => 5_000,
            Tier::Three => 1_000,
            Tier::ObserveOnly => 0,
        }
    }

    /// Whether real capital may be committed without a person saying so.
    ///
    /// Tier 3 is "paper trade, alert, or operator-confirmed micro-size only".
    pub const fn is_automatic(self) -> bool {
        matches!(self, Tier::One | Tier::Two)
    }
}

// ===========================================================================
// Inputs
// ===========================================================================

/// The account limits a size is bounded by. State, not policy: these move
/// between one decision and the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// What may be lost on this trade if the stress case happens.
    pub risk_budget_lamports: u64,
    /// Equity not already committed to an open position.
    pub free_equity_lamports: u64,
    /// The operator's own ceiling on one position.
    pub operator_max_notional_lamports: u64,
}

impl Account {
    /// An account that can do nothing. The right thing to start from.
    pub const EMPTY: Account = Account {
        risk_budget_lamports: 0,
        free_equity_lamports: 0,
        operator_max_notional_lamports: 0,
    };
}

/// How the entry rule sizes and bounds a position. Every field is policy and
/// versioned with the rest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryParams {
    /// Total swap fee on the SOL leg, in basis points.
    pub fee_bps: u16,
    /// The participation cap. The tighter of this and the snapshot's binds.
    pub max_pool_share_bps: u16,
    pub min_notional_lamports: u64,
    /// Gate 6D's ceiling. `u64::MAX` turns it off.
    pub hard_cap_lamports: u64,
    pub max_slippage_bps: u16,
    /// The worst fill the precomputed emergency exit may plan for.
    pub emergency_max_slippage_bps: u16,
    pub decision_ttl_ms: i64,
    pub max_snapshot_age_ms: i64,
    /// The lower confidence bound on the edge, in basis points of the position.
    ///
    /// **Zero by default, and zero refuses every entry.** There is no calibrated
    /// `P(win)` for this thesis; the only measurement of it is negative. A number
    /// here is a claim, and the roadmap's Phase 3 gate is what has to produce it.
    pub edge_lcb_bps: u16,
    /// Whether the decision is being made for the fast route, which brings the
    /// fast-path gate's own allowance and slippage ceiling into the chain.
    pub fast_path: bool,
    /// Whether a Tier 3 candidate may commit real capital without a person.
    /// False is doctrine.
    pub allow_tier_three: bool,
    pub social: SocialParams,
}

impl Default for EntryParams {
    fn default() -> Self {
        EntryParams {
            fee_bps: crate::replay::DEFAULT_FEE_BPS,
            max_pool_share_bps: MAX_POOL_SHARE_BPS,
            min_notional_lamports: MIN_NOTIONAL_LAMPORTS,
            hard_cap_lamports: MICRO_LIVE_CAP_LAMPORTS,
            max_slippage_bps: ENTRY_MAX_SLIPPAGE_BPS,
            emergency_max_slippage_bps: EMERGENCY_MAX_SLIPPAGE_BPS,
            decision_ttl_ms: DECISION_TTL_MS,
            max_snapshot_age_ms: MAX_SNAPSHOT_AGE_MS,
            edge_lcb_bps: 0,
            fast_path: false,
            allow_tier_three: false,
            social: SocialParams::default(),
        }
    }
}

/// Everything versioned that a decision depends on, in one value.
///
/// Carried together because a decision is only reproducible next to the policy
/// it was made under, and three structs that travel separately are three structs
/// that can be replayed out of step with each other.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub cluster: ClusterParams,
    pub gate: GateParams,
    pub entry: EntryParams,
}

// ===========================================================================
// The stress set
// ===========================================================================

/// The worst the modelled scenarios do to a position of the probe size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StressReport {
    /// Whether the curve priced the round trip at all. False means every number
    /// below is UNKNOWN rather than zero.
    pub measured: bool,
    /// The size the stress was measured at.
    pub probe_lamports: u64,
    pub worst_gap_bps: u16,
    pub worst_slippage_bps: u16,
    /// The worst modelled loss, in basis points of the position.
    pub loss_bps: u16,
    /// Fees and curve impact both ways at the probe size, before any stress.
    pub round_trip_cost_lamports: u64,
    /// Some gap bucket left a pool too thin to pay for the exit at all. The
    /// no-executable-exit scenario, which is a total loss and not a quote that
    /// happened to come back small.
    pub no_executable_exit: bool,
}

impl StressReport {
    /// Nothing was priced.
    pub const UNMEASURED: StressReport = StressReport {
        measured: false,
        probe_lamports: 0,
        worst_gap_bps: 0,
        worst_slippage_bps: 0,
        loss_bps: 0,
        round_trip_cost_lamports: 0,
        no_executable_exit: false,
    };
}

/// The curve after other people have sold it down by `gap_bps`.
///
/// A gap is not a number applied to the proceeds. It is a fall somebody caused
/// by selling, and selling into a constant product does three things at once:
/// the price falls, the SOL those sellers took leaves the pool, and the tokens
/// they sold come back into it. Modelling only the first would price the fall
/// and miss both of the others, and the others are what decide whether the
/// position still has an exit.
///
/// So the fall is applied to the SOL side and the product is preserved:
///
/// ```text
/// y' = y × (1 - g)          the price falls by exactly g
/// x' = k / y'               the tokens the sellers handed back
/// real_sol'   = real_sol - (y - y')
/// real_token' = real_token + (x' - x)
/// ```
///
/// `real_sol'` saturates at zero, and that is the interesting case rather than
/// an edge case. `y` carries 30 SOL of virtual reserve that does not exist, so
/// the deepest fall selling can actually produce is `real_sol / (30 + real_sol)`
/// — about 25% on a curve holding ten SOL and about 57% on one holding forty. A
/// 50% bucket against a young curve therefore drains it: the model says the pool
/// is empty, every exit fails, and the position is a total loss. That is the
/// right reading. A fall that deep on a curve that shallow did not come from
/// the curve, and whatever did cause it is not something there is an exit
/// through.
fn gapped(curve: &CurveState, gap_bps: u16) -> CurveState {
    let remaining = u128::from(BPS_DENOMINATOR).saturating_sub(u128::from(gap_bps));
    let sol = mul_div_floor(
        u128::from(curve.virtual_sol_reserves),
        remaining,
        u128::from(BPS_DENOMINATOR),
    );
    if sol == 0 {
        // Nothing left to price against. `is_plausible` refuses it, every quote
        // fails, and the caller reads that as no executable exit.
        return CurveState {
            virtual_sol_reserves: 0,
            real_sol_reserves: 0,
            ..*curve
        };
    }
    let tokens = clamp_u64(curve.k() / sol);
    let handed_back = tokens.saturating_sub(curve.virtual_token_reserves);
    let withdrawn = curve.virtual_sol_reserves.saturating_sub(clamp_u64(sol));
    CurveState {
        virtual_sol_reserves: clamp_u64(sol),
        virtual_token_reserves: tokens,
        real_sol_reserves: curve.real_sol_reserves.saturating_sub(withdrawn),
        real_token_reserves: curve.real_token_reserves.saturating_add(handed_back),
        ..*curve
    }
}

/// Prices the round trip and every stress bucket at one size.
///
/// The round trip is a real buy against the curve, the curve as it stands after
/// that buy, and the sell of exactly what the buy produced. On a constant
/// product that reversal returns what went in less the two fees — the position's
/// own impact cancels, because it moved the price up and then back down through
/// the same reserves. That is the honest cost of a round trip nobody else
/// traded against, and it is why `round_trip_cost_lamports` is a little under
/// twice the fee and barely moves with size.
///
/// Everything that does move with size is in the buckets. Each gap bucket is the
/// pool after that fraction of its SOL has left, and the exit is quoted *into
/// that pool* — so a position that was 1.5% of a full pool is 3% of a halved one
/// and pays the impact accordingly. A pool that cannot pay for the exit at all
/// scores a total loss and sets `no_executable_exit`. The slippage bucket is the
/// execution drag on top of whatever came back, and it compounds with the gap
/// rather than adding to it: a 50% fall and a 25% bad fill leave 0.5 x 0.75 of
/// the proceeds, not 0.25 of them.
///
/// `None` when the curve cannot price the entry or the ordinary exit — a
/// complete curve, an implausible one, or a size it has no tokens for. That is
/// UNKNOWN, and a candidate carrying it is refused rather than sized against a
/// guess.
pub fn stress(curve: &CurveState, probe_lamports: u64, fee_bps: u16) -> Option<StressReport> {
    if probe_lamports == 0 {
        return None;
    }
    let entry = curve.quote_buy(probe_lamports, fee_bps).ok()?;
    let post = curve.after_buy(&entry);
    let ordinary = post.quote_sell(entry.tokens, fee_bps).ok()?;

    let mut worst = StressReport {
        measured: true,
        probe_lamports,
        worst_gap_bps: 0,
        worst_slippage_bps: 0,
        loss_bps: 0,
        round_trip_cost_lamports: probe_lamports.saturating_sub(ordinary.net_lamports),
        no_executable_exit: false,
    };
    for gap_bps in GAP_BUCKETS_BPS {
        let thin = gapped(&post, gap_bps);
        let proceeds = thin
            .quote_sell(entry.tokens, fee_bps)
            .map_or(0, |fill| fill.net_lamports);
        worst.no_executable_exit |= proceeds == 0;
        for slippage_bps in SLIPPAGE_BUCKETS_BPS {
            let loss = loss_bps(probe_lamports, haircut(proceeds, slippage_bps));
            if loss > worst.loss_bps {
                worst.loss_bps = loss;
                worst.worst_gap_bps = gap_bps;
                worst.worst_slippage_bps = slippage_bps;
            }
        }
    }
    Some(worst)
}

/// The worst the stress set does anywhere between the smallest position policy
/// allows and the largest the pool does.
///
/// Two probes rather than one because the loss is *nearly* but not exactly
/// size-independent, and the direction of the difference is the wrong one for a
/// single probe at the cap: an entry's own SOL deepens the pool it is about to
/// be gapped in, so a larger position loses a shade less of itself than a
/// smaller one — on a curve holding forty SOL the spread is 8 163 basis points
/// at a hundredth of a SOL against 7 841 at thirty, which is under four percent
/// of the figure across three orders of magnitude. Taking the worse of the two
/// ends bounds it without solving anything, and without a claim about
/// monotonicity that the next curve model might not honour.
///
/// `None` when neither probe prices, which is the same UNKNOWN [`stress`]
/// returns and is refused the same way.
fn stress_range(
    curve: &CurveState,
    floor_lamports: u64,
    cap_lamports: u64,
    fee_bps: u16,
) -> Option<StressReport> {
    let floor = floor_lamports.min(cap_lamports).max(1);
    let mut worst: Option<StressReport> = None;
    for probe in [floor, cap_lamports] {
        let Some(report) = stress(curve, probe, fee_bps) else {
            continue;
        };
        if worst.is_none_or(|best| report.loss_bps > best.loss_bps) {
            worst = Some(report);
        }
    }
    worst
}

/// What is left of `lamports` after a fall of `bps`. Floors, so the residual is
/// a cost rather than a windfall.
fn haircut(lamports: u64, bps: u16) -> u64 {
    let remaining = u128::from(BPS_DENOMINATOR).saturating_sub(u128::from(bps));
    mul_div_floor(u128::from(lamports), remaining, u128::from(BPS_DENOMINATOR)) as u64
}

/// The loss from `spent` to `recovered`, in basis points, capped at 100%.
///
/// Rounds up: a simulator that under-reports its own losses flatters every
/// backtest built on it, which is the same argument `replay::slippage_bps`
/// makes for rounding its residual towards the trader's cost.
fn loss_bps(spent: u64, recovered: u64) -> u16 {
    if spent == 0 {
        return 0;
    }
    let lost = u128::from(spent.saturating_sub(recovered));
    let bps = lost * u128::from(BPS_DENOMINATOR) + u128::from(spent) - 1;
    (bps / u128::from(spent)).min(u128::from(BPS_DENOMINATOR)) as u16
}

/// The worst gap the stress set models.
fn worst_gap_bps() -> u16 {
    GAP_BUCKETS_BPS.iter().copied().max().unwrap_or(0)
}

/// The worst execution drag the stress set models.
fn worst_slippage_bps() -> u16 {
    SLIPPAGE_BUCKETS_BPS.iter().copied().max().unwrap_or(0)
}

// ===========================================================================
// The size chain
// ===========================================================================

/// Which cap decided the size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SizeCap {
    RiskBudget,
    Pool,
    FastPathGate,
    Operator,
    Equity,
    /// Gate 6D's ceiling, which binds after the tier and the story have had
    /// their say rather than before.
    HardCap,
}

impl SizeCap {
    pub const fn as_str(self) -> &'static str {
        match self {
            SizeCap::RiskBudget => "risk-budget",
            SizeCap::Pool => "pool",
            SizeCap::FastPathGate => "fast-path-gate",
            SizeCap::Operator => "operator",
            SizeCap::Equity => "equity",
            SizeCap::HardCap => "hard-cap",
        }
    }
}

/// Every cap in the chain, what it allowed, and which one bound.
///
/// Carried on refusals as well as acceptances. A funnel over a corpus that can
/// say "nine in ten candidates were bound by the pool" is a different
/// conversation from one that can only say how many traded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizeCaps {
    pub stressed_loss_bps: u16,
    pub risk_budget_lamports: u64,
    pub pool_lamports: u64,
    /// `None` off the fast route, where the gate's allowance does not apply.
    pub gate_lamports: Option<u64>,
    pub operator_lamports: u64,
    pub equity_lamports: u64,
    /// The smallest of the above.
    pub base_lamports: u64,
    pub tier_multiplier_bps: u16,
    pub after_tier_lamports: u64,
    pub social_multiplier_bps: u16,
    pub after_social_lamports: u64,
    pub hard_cap_lamports: u64,
    pub size_lamports: u64,
    pub binding: SizeCap,
}

impl SizeCaps {
    /// Nothing was computed. Every cap zero and the binding one the tightest,
    /// so a refusal that never reached the chain cannot be read as a pool that
    /// happened to allow nothing.
    pub const NONE: SizeCaps = SizeCaps {
        stressed_loss_bps: 0,
        risk_budget_lamports: 0,
        pool_lamports: 0,
        gate_lamports: None,
        operator_lamports: 0,
        equity_lamports: 0,
        base_lamports: 0,
        tier_multiplier_bps: 0,
        after_tier_lamports: 0,
        social_multiplier_bps: 0,
        after_social_lamports: 0,
        hard_cap_lamports: 0,
        size_lamports: 0,
        binding: SizeCap::HardCap,
    };
}

/// The size chain from `RISK_AND_SYBIL_SPEC.md` §10.
///
/// ```text
/// base = min(risk_budget_size, pool_cap, gate_cap, operator_cap, equity_cap)
/// size = base × tier_bps / 10_000 × social_bps / 10_000, then the hard cap
/// ```
///
/// The tie in the minimum falls to the cap listed first, which is the order the
/// specification writes them in, so two runs over one candidate name the same
/// binding cap.
///
/// `stressed_loss_bps` of zero would divide by nothing. It cannot happen against
/// a real curve — the round trip pays two fees — and if a caller manufactures it
/// the risk budget simply stops binding rather than panicking inside a build
/// with `overflow-checks` on.
fn size_chain(
    stress: &StressReport,
    account: &Account,
    snapshot: &RiskSnapshot,
    pool_cap: u64,
    tier: Tier,
    social: &SocialWeight,
    params: &EntryParams,
) -> SizeCaps {
    let risk_budget = if stress.loss_bps == 0 {
        u64::MAX
    } else {
        clamp_u64(mul_div_floor(
            u128::from(account.risk_budget_lamports),
            u128::from(BPS_DENOMINATOR),
            u128::from(stress.loss_bps),
        ))
    };
    let gate = params
        .fast_path
        .then_some(snapshot.fast_path.max_notional_lamports);

    let chain: [(SizeCap, u64); 5] = [
        (SizeCap::RiskBudget, risk_budget),
        (SizeCap::Pool, pool_cap),
        (SizeCap::FastPathGate, gate.unwrap_or(u64::MAX)),
        (SizeCap::Operator, account.operator_max_notional_lamports),
        (SizeCap::Equity, account.free_equity_lamports),
    ];
    let (mut binding, base) = chain
        .iter()
        .copied()
        .min_by_key(|&(_, value)| value)
        .expect("the chain is a fixed five");

    let tier_bps = tier.multiplier_bps();
    let after_tier = clamp_u64(mul_div_floor(
        u128::from(base),
        u128::from(tier_bps),
        u128::from(BPS_DENOMINATOR),
    ));
    let after_social = social.apply(after_tier);
    let size = after_social.min(params.hard_cap_lamports);
    if size < after_social {
        binding = SizeCap::HardCap;
    }

    SizeCaps {
        stressed_loss_bps: stress.loss_bps,
        risk_budget_lamports: risk_budget,
        pool_lamports: pool_cap,
        gate_lamports: gate,
        operator_lamports: account.operator_max_notional_lamports,
        equity_lamports: account.free_equity_lamports,
        base_lamports: base,
        tier_multiplier_bps: tier_bps,
        after_tier_lamports: after_tier,
        social_multiplier_bps: social.effective_bps(),
        after_social_lamports: after_social,
        hard_cap_lamports: params.hard_cap_lamports,
        size_lamports: size,
        binding,
    }
}

/// A `u128` back into a `u64`, saturating. Every product here is bounded by a
/// lamport quantity, so the saturation is a belt on a brace.
fn clamp_u64(value: u128) -> u64 {
    value.min(u128::from(u64::MAX)) as u64
}

// ===========================================================================
// The way out, priced before the way in
// ===========================================================================

/// The exit that was quoted before the entry was agreed.
///
/// Phase 2's fifth acceptance criterion: every entry candidate carries a
/// precomputed emergency exit. Two quotes rather than one, because "can I get
/// out of this" and "what would it be worth if the market fell" are different
/// questions with different consequences — the first is a safety check and the
/// second is information. Both are taken against the curve as it stands *after*
/// the entry has moved it, since that is the pool the position would actually
/// be leaving through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitReadiness {
    /// What the entry would acquire.
    pub tokens: u64,
    /// What selling all of it would return at the current depth, net of fees.
    pub net_lamports: u64,
    pub slippage_bps: u16,
    /// The gap the second quote was taken under — the worst the stress set
    /// models.
    pub gap_bps: u16,
    /// What selling all of it would return after that gap. Zero means the gapped
    /// pool could not pay for the position at all.
    pub gapped_net_lamports: u64,
    /// Whether the exit at the current depth is inside the emergency ceiling.
    /// This is the half that gates.
    pub within_ceiling: bool,
}

/// Quotes the exit at `size`: what it is worth now, and what it would be worth
/// after the worst gap the stress set models.
///
/// The check that gates is the first one. "Can this position be sold through
/// the pool it is in, without moving the price more than the emergency ceiling
/// allows" is a question about depth, and depth is what an entry can do
/// something about — the participation cap exists to keep the answer yes, and
/// this is where that is verified rather than assumed. The gapped valuation is
/// carried beside it because an operator looking at a position is owed the
/// number, but it does not gate: a gap is a direction, and refusing every entry
/// that would hurt in a crash is refusing every entry.
///
/// `None` when the pool cannot price the exit at all — including
/// `ExceedsRealSol`, which is the pool not holding enough SOL to pay for the
/// position it is about to take.
fn exit_readiness(
    curve: &CurveState,
    size_lamports: u64,
    params: &EntryParams,
) -> Option<ExitReadiness> {
    let entry = curve.quote_buy(size_lamports, params.fee_bps).ok()?;
    let post = curve.after_buy(&entry);
    let ordinary = post.quote_sell(entry.tokens, params.fee_bps).ok()?;
    let gap_bps = worst_gap_bps();
    let gapped_net = gapped(&post, gap_bps)
        .quote_sell(entry.tokens, params.fee_bps)
        .map_or(0, |fill| fill.net_lamports);
    Some(ExitReadiness {
        tokens: entry.tokens,
        net_lamports: ordinary.net_lamports,
        slippage_bps: ordinary.slippage_bps,
        gap_bps,
        gapped_net_lamports: gapped_net,
        within_ceiling: ordinary.slippage_bps <= params.emergency_max_slippage_bps,
    })
}

// ===========================================================================
// Expectancy
// ===========================================================================

/// What the trade is worth before it is taken, and what it is worth if the
/// execution goes as badly as the model allows.
///
/// **The gap buckets are deliberately not in here.** A gap is a direction, and
/// pricing one as a certainty is assuming every trade loses; the model has no
/// calibrated probability to weigh it with. Gaps belong to survival, and that is
/// where they are used — sizing, through `stressed_loss_bps` against the risk
/// budget, and the emergency exit check. What gates expectancy is cost: the two
/// fees, the position's own impact, and the worst execution drag the stress set
/// models, all of which are paid whichever way the price goes.
///
/// `edge_lcb_bps` is the only input here that is not measured off the curve, and
/// it defaults to zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvReport {
    pub edge_lcb_bps: u16,
    /// The declared edge on this position, in lamports.
    pub gross_edge_lamports: u64,
    /// Fees and impact both ways at the current depth.
    pub round_trip_cost_lamports: u64,
    /// The drag the stressed cost was taken under.
    pub stress_slippage_bps: u16,
    /// The round trip with that drag on the way out.
    pub stressed_cost_lamports: u64,
    /// The worst modelled loss at this size, gaps included. Reported because
    /// sizing turned on it; it does not gate expectancy.
    pub stressed_loss_lamports: u64,
    /// Edge less the ordinary round-trip cost.
    pub net_ev_lamports: i64,
    /// Edge less the stressed cost. This is what gates.
    pub stressed_ev_lamports: i64,
    /// Whether the stressed lower bound is above zero.
    pub positive: bool,
}

impl EvReport {
    pub const UNMEASURED: EvReport = EvReport {
        edge_lcb_bps: 0,
        gross_edge_lamports: 0,
        round_trip_cost_lamports: 0,
        stress_slippage_bps: 0,
        stressed_cost_lamports: 0,
        stressed_loss_lamports: 0,
        net_ev_lamports: 0,
        stressed_ev_lamports: 0,
        positive: false,
    };
}

/// Prices the expectancy of a position of `size_lamports`.
///
/// `stressed_loss_lamports` is the stress set's basis points applied to this
/// size rather than a fresh simulation of it. [`stress_range`] has already taken
/// the worse of the two ends of the size range, so the figure is the higher of
/// what the two probes measured and not an extrapolation past either of them.
fn expectancy(
    size_lamports: u64,
    stress: &StressReport,
    exit: &ExitReadiness,
    params: &EntryParams,
) -> EvReport {
    let gross_edge = clamp_u64(mul_div_floor(
        u128::from(size_lamports),
        u128::from(params.edge_lcb_bps),
        u128::from(BPS_DENOMINATOR),
    ));
    let round_trip_cost = size_lamports.saturating_sub(exit.net_lamports);
    let stress_slippage_bps = worst_slippage_bps();
    let stressed_cost =
        size_lamports.saturating_sub(haircut(exit.net_lamports, stress_slippage_bps));
    let stressed_loss = clamp_u64(mul_div_floor(
        u128::from(size_lamports),
        u128::from(stress.loss_bps),
        u128::from(BPS_DENOMINATOR),
    ));
    let net = i128::from(gross_edge) - i128::from(round_trip_cost);
    let stressed = i128::from(gross_edge) - i128::from(stressed_cost);
    EvReport {
        edge_lcb_bps: params.edge_lcb_bps,
        gross_edge_lamports: gross_edge,
        round_trip_cost_lamports: round_trip_cost,
        stress_slippage_bps,
        stressed_cost_lamports: stressed_cost,
        stressed_loss_lamports: stressed_loss,
        net_ev_lamports: clamp_i64(net),
        stressed_ev_lamports: clamp_i64(stressed),
        positive: stressed > 0,
    }
}

fn clamp_i64(value: i128) -> i64 {
    value.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

// ===========================================================================
// The decision
// ===========================================================================

/// Every answer the entry rule can give, worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryReason {
    /// The syndicate gate said no. Its own reason is on the verdict.
    GateRefused,
    /// The engine is halted, the breaker is tripped, every slot is full, or the
    /// drawdown limit is reached.
    EntriesBlocked,
    /// The risk snapshot is older than policy allows, or is dated after the
    /// decision. Either way the numbers are not the current ones.
    StaleSnapshot,
    /// The pool is under the entry floor, or its participation cap is zero.
    PoolTooThin,
    /// The curve would not price the round trip. UNKNOWN, not zero.
    Unquotable,
    /// Confidence below 0.55. Nothing to size.
    ObserveOnly,
    /// The fast route's gate does not admit a position this size.
    FastPathRefused,
    /// What survived the chain is too small to be worth the round trip.
    BelowMinNotional,
    /// The way out does not price, or prices past the emergency ceiling.
    ExitNotReady,
    /// The stressed expectancy is not above zero. The default answer while no
    /// calibrated edge exists.
    NegativeStressedEv,
    /// Everything passed and the tier is one a person has to authorise.
    OperatorConfirmationRequired,
    /// The only reason that trades.
    Accepted,
}

impl EntryReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            EntryReason::GateRefused => "gate-refused",
            EntryReason::EntriesBlocked => "entries-blocked",
            EntryReason::StaleSnapshot => "stale-snapshot",
            EntryReason::PoolTooThin => "pool-too-thin",
            EntryReason::Unquotable => "unquotable",
            EntryReason::ObserveOnly => "observe-only",
            EntryReason::FastPathRefused => "fast-path-refused",
            EntryReason::BelowMinNotional => "below-min-notional",
            EntryReason::ExitNotReady => "exit-not-ready",
            EntryReason::NegativeStressedEv => "negative-stressed-ev",
            EntryReason::OperatorConfirmationRequired => "operator-confirmation-required",
            EntryReason::Accepted => "accepted",
        }
    }

    /// Every reason, worst first, so a funnel over a corpus has the same shape
    /// whatever the corpus contained.
    pub const ALL: [EntryReason; 12] = [
        EntryReason::GateRefused,
        EntryReason::EntriesBlocked,
        EntryReason::StaleSnapshot,
        EntryReason::PoolTooThin,
        EntryReason::Unquotable,
        EntryReason::ObserveOnly,
        EntryReason::FastPathRefused,
        EntryReason::BelowMinNotional,
        EntryReason::ExitNotReady,
        EntryReason::NegativeStressedEv,
        EntryReason::OperatorConfirmationRequired,
        EntryReason::Accepted,
    ];
}

/// The plan, and everything it was decided on.
///
/// Not an envelope: nothing here is signed, nothing names an account, and
/// nothing can reach the network. It is what a dispatcher would need before it
/// could build one, plus the evidence for showing a person why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryDecision {
    pub enter: bool,
    pub reason: EntryReason,
    pub tier: Tier,
    pub confidence_micros: u64,
    pub size_lamports: u64,
    pub max_slippage_bps: u16,
    /// The instant the risk snapshot was taken. The decision is dated by the
    /// numbers it was made against rather than by a clock read separately.
    pub decided_at_ms: i64,
    pub expires_at_ms: i64,
    pub caps: SizeCaps,
    pub stress: StressReport,
    /// `None` when the exit was never priced, which is not an exit worth zero.
    pub exit: Option<ExitReadiness>,
    pub ev: EvReport,
    pub social: SocialWeight,
    pub notes: Vec<String>,
}

impl EntryDecision {
    /// Whether a dispatcher could still act on this.
    ///
    /// Expiry is a strict comparison against the deadline, so a plan is dead on
    /// the millisecond it expires rather than one after.
    pub const fn is_signable(&self, now_ms: i64) -> bool {
        self.enter && now_ms < self.expires_at_ms
    }
}

/// Size a launch the gate has already accepted, and price the way out of it.
///
/// Pure. Same inputs, same plan, and no clock is read inside: `now_ms` is passed
/// so a replay produces the decision the live run produced.
///
/// The order of the checks is the order of the authority behind them. The hard
/// blocks come first and none of them can be cleared by a tier, a story or a
/// score — `STS_CORE_IDEOLOGY.md` §5's "a tier can never override a hard block".
/// The economic refusals come last, because they are the ones a better model
/// could legitimately change.
pub fn plan_entry(
    verdict: &GateVerdict,
    social: &SocialWeight,
    snapshot: &RiskSnapshot,
    account: &Account,
    curve: &CurveState,
    params: &EntryParams,
    now_ms: i64,
) -> EntryDecision {
    let tier = Tier::for_confidence(verdict.confidence_micros);
    let max_slippage_bps = if params.fast_path {
        params
            .max_slippage_bps
            .min(snapshot.fast_path.max_slippage_bps)
    } else {
        params.max_slippage_bps
    };
    let mut decision = EntryDecision {
        enter: false,
        reason: EntryReason::GateRefused,
        tier,
        confidence_micros: verdict.confidence_micros,
        size_lamports: 0,
        max_slippage_bps,
        decided_at_ms: snapshot.at_ms,
        expires_at_ms: snapshot.at_ms.saturating_add(params.decision_ttl_ms),
        caps: SizeCaps::NONE,
        stress: StressReport::UNMEASURED,
        exit: None,
        ev: EvReport::UNMEASURED,
        social: social.clone(),
        notes: Vec::new(),
    };
    let refuse = |mut decision: EntryDecision, reason: EntryReason, note: String| {
        decision.reason = reason;
        decision.enter = false;
        decision.size_lamports = 0;
        decision.notes.push(note);
        decision
    };

    // --- Hard blocks -----------------------------------------------------
    if !verdict.enter {
        return refuse(
            decision,
            EntryReason::GateRefused,
            format!("the syndicate gate said {}", verdict.reason.as_str()),
        );
    }
    if !snapshot.entries_allowed() {
        return refuse(
            decision,
            EntryReason::EntriesBlocked,
            format!(
                "the engine is in {} and is not opening positions",
                snapshot.mode
            ),
        );
    }
    let age_ms = now_ms.saturating_sub(snapshot.at_ms);
    if age_ms < 0 || age_ms > params.max_snapshot_age_ms {
        return refuse(
            decision,
            EntryReason::StaleSnapshot,
            format!("the risk snapshot is {age_ms}ms from the decision"),
        );
    }

    // --- The pool --------------------------------------------------------
    let pool_cap = pool_cap_lamports(curve, snapshot, params);
    if !snapshot.liquidity.admits_entry(curve.real_sol_reserves) || pool_cap == 0 {
        return refuse(
            decision,
            EntryReason::PoolTooThin,
            format!(
                "{} lamports of executable liquidity is under the entry floor",
                curve.real_sol_reserves
            ),
        );
    }

    if tier == Tier::ObserveOnly {
        return refuse(
            decision,
            EntryReason::ObserveOnly,
            format!(
                "confidence {} is under the {} a size starts at",
                verdict.confidence_micros, TIER_THREE_MICROS
            ),
        );
    }

    // --- What a bad day costs --------------------------------------------
    let Some(stress) = stress_range(
        curve,
        params.min_notional_lamports,
        pool_cap,
        params.fee_bps,
    ) else {
        return refuse(
            decision,
            EntryReason::Unquotable,
            "the curve would not price the round trip".to_string(),
        );
    };
    decision.stress = stress;

    // --- The chain -------------------------------------------------------
    let caps = size_chain(&stress, account, snapshot, pool_cap, tier, social, params);
    decision.caps = caps;
    decision.size_lamports = caps.size_lamports;
    decision.notes.push(format!(
        "{} lamports, bound by the {} cap",
        caps.size_lamports,
        caps.binding.as_str()
    ));
    if social.reduced() {
        decision.notes.push(format!(
            "the story took {} bps off",
            BPS_DENOMINATOR as u16 - social.effective_bps()
        ));
    }

    if params.fast_path && !snapshot.fast_path_allowed(caps.size_lamports) {
        return refuse(
            decision,
            EntryReason::FastPathRefused,
            "the fast-path gate does not admit a position this size".to_string(),
        );
    }
    if caps.size_lamports < params.min_notional_lamports {
        return refuse(
            decision,
            EntryReason::BelowMinNotional,
            format!(
                "{} lamports is under the {} floor a round trip needs",
                caps.size_lamports, params.min_notional_lamports
            ),
        );
    }

    // --- The way out, before the way in ----------------------------------
    let Some(exit) = exit_readiness(curve, caps.size_lamports, params) else {
        return refuse(
            decision,
            EntryReason::ExitNotReady,
            "the pool cannot pay for a position this size".to_string(),
        );
    };
    decision.exit = Some(exit);
    if !exit.within_ceiling {
        return refuse(
            decision,
            EntryReason::ExitNotReady,
            format!(
                "the exit would move the price {} bps on its own, past the {} bps ceiling",
                exit.slippage_bps, params.emergency_max_slippage_bps
            ),
        );
    }

    // --- Expectancy ------------------------------------------------------
    let ev = expectancy(caps.size_lamports, &stress, &exit, params);
    decision.ev = ev;
    if !ev.positive {
        return refuse(
            decision,
            EntryReason::NegativeStressedEv,
            format!(
                "a declared edge of {} bps does not cover the {} lamports a stressed \
                 round trip costs",
                ev.edge_lcb_bps, ev.stressed_cost_lamports
            ),
        );
    }

    // --- Who may say yes -------------------------------------------------
    if !tier.is_automatic() && !params.allow_tier_three {
        decision.reason = EntryReason::OperatorConfirmationRequired;
        decision.notes.push(format!(
            "{} commits no real capital without a person",
            tier.as_str()
        ));
        return decision;
    }

    decision.enter = true;
    decision.reason = EntryReason::Accepted;
    decision
}

/// The participation cap, taking the tighter of policy and the snapshot.
///
/// A snapshot that arrived with a laxer share limit must not be able to widen
/// the doctrine cap — that is the direction the 500-against-150 disagreement in
/// `ingestion::StreamFilters` would otherwise resolve itself in, silently, at
/// runtime.
fn pool_cap_lamports(curve: &CurveState, snapshot: &RiskSnapshot, params: &EntryParams) -> u64 {
    let share_bps = snapshot
        .liquidity
        .max_pool_share_bps
        .min(params.max_pool_share_bps);
    curve.max_position_lamports(share_bps)
}

/// Read a launch, judge it, weigh its story and size it, in one call.
///
/// The whole path from a recorded launch to a plan. `now_ms` is the instant the
/// decision is being made at; the snapshot carries its own, and a gap between
/// them wider than policy is what [`EntryReason::StaleSnapshot`] is for.
///
/// `quote` is the order the sandwich guard prices — the one refusal that is
/// about our own transaction rather than about the launch. It belongs to the
/// analyser's gate rather than to the size chain, so it is passed straight
/// through: an order that can be front-run is refused before there is a
/// position to size, and `None` leaves the guard to say whether that is
/// acceptable.
///
/// Eight arguments, and grouping them would cost more than it saves: the launch,
/// the story and the order are three different observations of one candidate,
/// and the snapshot, the account and the curve are three different parts of the
/// world. A struct over either group would read as one thing where the point is
/// that a decision names every input it depends on. `db.rs`, `journal.rs` and
/// `daemon.rs` take the same view at the same lint.
#[allow(clippy::too_many_arguments)]
pub fn decide(
    record: &LaunchRecord,
    scan: Option<&SocialScan>,
    quote: Option<&EntryQuote>,
    snapshot: &RiskSnapshot,
    account: &Account,
    curve: &CurveState,
    policy: &Policy,
    now_ms: i64,
) -> (ClusterReport, GateVerdict, EntryDecision) {
    let report = analyse_launch(record, &policy.cluster);
    let verdict = syndicate_gate(&report, &policy.gate, quote);
    let social = weigh(scan, &policy.entry.social);
    let decision = plan_entry(
        &verdict,
        &social,
        snapshot,
        account,
        curve,
        &policy.entry,
        now_ms,
    );
    (report, verdict, decision)
}

/// The confidence as the schema stores it, in millionths, for a caller that has
/// to put a tier next to a number a person reads.
pub fn confidence_percent(confidence_micros: u64) -> u64 {
    confidence_micros.min(MICROS) / 10_000
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::replay::{DEFAULT_FEE_BPS, LAUNCH_VIRTUAL_SOL_RESERVES};
    use crate::strategy::social::{SocialCaution, StoryKind, ViewSample};
    use crate::strategy::syndicate::{GateReason, OpeningBuyer, RiskTag};
    use crate::types::{
        CircuitBreaker, FastPathGate, LiquidityThresholds, OperatingMode, RiskSnapshot,
    };

    const SOL: u64 = LAMPORTS_PER_SOL;
    const NOW: i64 = 1_700_000_000_000;

    // -----------------------------------------------------------------------
    // Fixtures
    // -----------------------------------------------------------------------

    /// A curve with 40 SOL of real, executable liquidity in it. Deep enough that
    /// the participation cap allows 0.6 SOL, which is well clear of every floor.
    fn a_deep_curve() -> CurveState {
        CurveState::at_real_sol(40 * SOL)
    }

    fn thresholds() -> LiquidityThresholds {
        LiquidityThresholds {
            min_pool_lamports: 5 * SOL,
            exit_only_below_lamports: SOL,
            max_pool_share_bps: 500,
        }
    }

    fn a_healthy_snapshot() -> RiskSnapshot {
        RiskSnapshot {
            at_ms: NOW,
            // Paper, not live: this build has no signer and the roadmap keeps
            // the dispatcher simulation-only until Phase 4 is explicitly
            // promoted. Paper opens positions, which is what the chain needs.
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
            liquidity: thresholds(),
        }
    }

    fn an_account() -> Account {
        Account {
            risk_budget_lamports: SOL / 2,
            free_equity_lamports: 100 * SOL,
            operator_max_notional_lamports: 10 * SOL,
        }
    }

    /// A verdict the syndicate gate accepted, at a confidence the caller picks.
    fn accepted(confidence_micros: u64) -> GateVerdict {
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

    fn refused() -> GateVerdict {
        GateVerdict {
            enter: false,
            reason: GateReason::MixedSizing,
            ..accepted(900_000)
        }
    }

    /// Params with an edge declared, so the expectancy gate can be got past and
    /// everything downstream of it exercised.
    ///
    /// Fifty percent, which is the shape of claim the syndicate thesis makes —
    /// the bundle's exit is the trade and the target is a multiple — and which
    /// no holdout has made. Hence the name. It has to clear the stressed cost of
    /// a round trip, which is the two fees plus the worst execution bucket, or
    /// about 2 650 basis points at this depth.
    fn with_an_imagined_edge() -> EntryParams {
        EntryParams {
            edge_lcb_bps: 5_000,
            hard_cap_lamports: u64::MAX,
            ..EntryParams::default()
        }
    }

    fn plan(verdict: &GateVerdict, params: &EntryParams) -> EntryDecision {
        plan_entry(
            verdict,
            &SocialWeight::unscanned(),
            &a_healthy_snapshot(),
            &an_account(),
            &a_deep_curve(),
            params,
            NOW,
        )
    }

    // -----------------------------------------------------------------------
    // The default answer
    // -----------------------------------------------------------------------

    #[test]
    fn an_uncalibrated_edge_refuses_every_entry() {
        // The shipped default. Nothing trades until a holdout has produced an
        // edge to put in the field, which is the Phase 3 gate.
        let decision = plan(&accepted(950_000), &EntryParams::default());
        assert!(!decision.enter);
        assert_eq!(decision.reason, EntryReason::NegativeStressedEv);
        assert_eq!(decision.ev.edge_lcb_bps, 0);
        assert!(decision.ev.stressed_ev_lamports < 0);
    }

    #[test]
    fn a_declared_edge_that_covers_the_stress_case_trades() {
        let decision = plan(&accepted(950_000), &with_an_imagined_edge());
        assert!(decision.enter, "{:?}", decision.reason);
        assert_eq!(decision.reason, EntryReason::Accepted);
        assert!(decision.size_lamports > 0);
    }

    // -----------------------------------------------------------------------
    // Hard blocks, in order
    // -----------------------------------------------------------------------

    #[test]
    fn a_refused_gate_is_refused_here_too() {
        let decision = plan(&refused(), &with_an_imagined_edge());
        assert_eq!(decision.reason, EntryReason::GateRefused);
        assert_eq!(decision.size_lamports, 0);
        assert_eq!(decision.caps, SizeCaps::NONE);
    }

    #[test]
    fn a_halted_engine_opens_nothing_however_good_the_launch_looks() {
        let snapshot = RiskSnapshot {
            mode: OperatingMode::Halted,
            ..a_healthy_snapshot()
        };
        let decision = plan_entry(
            &accepted(1_000_000),
            &SocialWeight::unscanned(),
            &snapshot,
            &an_account(),
            &a_deep_curve(),
            &with_an_imagined_edge(),
            NOW,
        );
        assert_eq!(decision.reason, EntryReason::EntriesBlocked);
    }

    #[test]
    fn a_full_book_and_a_deep_drawdown_open_nothing_either() {
        let full = RiskSnapshot {
            open_positions: 3,
            ..a_healthy_snapshot()
        };
        let drawn_down = RiskSnapshot {
            equity_lamports: 100 * SOL,
            high_water_lamports: 200 * SOL,
            ..a_healthy_snapshot()
        }
        .with_recomputed_drawdown();
        for snapshot in [full, drawn_down] {
            let decision = plan_entry(
                &accepted(1_000_000),
                &SocialWeight::unscanned(),
                &snapshot,
                &an_account(),
                &a_deep_curve(),
                &with_an_imagined_edge(),
                NOW,
            );
            assert_eq!(decision.reason, EntryReason::EntriesBlocked);
        }
    }

    #[test]
    fn a_tripped_breaker_opens_nothing() {
        let snapshot = RiskSnapshot {
            circuit_breaker: CircuitBreaker::Tripped {
                reason: crate::types::BreakerReason::LosingStreak,
                at_ms: NOW - 1_000,
                clears_at_ms: None,
            },
            ..a_healthy_snapshot()
        };
        let decision = plan_entry(
            &accepted(1_000_000),
            &SocialWeight::unscanned(),
            &snapshot,
            &an_account(),
            &a_deep_curve(),
            &with_an_imagined_edge(),
            NOW,
        );
        assert_eq!(decision.reason, EntryReason::EntriesBlocked);
    }

    #[test]
    fn a_snapshot_older_than_a_slot_is_not_the_current_market() {
        let decision = plan_entry(
            &accepted(950_000),
            &SocialWeight::unscanned(),
            &a_healthy_snapshot(),
            &an_account(),
            &a_deep_curve(),
            &with_an_imagined_edge(),
            NOW + MAX_SNAPSHOT_AGE_MS + 1,
        );
        assert_eq!(decision.reason, EntryReason::StaleSnapshot);
    }

    #[test]
    fn a_snapshot_dated_after_the_decision_is_refused_rather_than_trusted() {
        let decision = plan_entry(
            &accepted(950_000),
            &SocialWeight::unscanned(),
            &a_healthy_snapshot(),
            &an_account(),
            &a_deep_curve(),
            &with_an_imagined_edge(),
            NOW - 1,
        );
        assert_eq!(decision.reason, EntryReason::StaleSnapshot);
    }

    #[test]
    fn a_pool_under_the_entry_floor_is_refused() {
        let decision = plan_entry(
            &accepted(950_000),
            &SocialWeight::unscanned(),
            &a_healthy_snapshot(),
            &an_account(),
            &CurveState::at_real_sol(SOL),
            &with_an_imagined_edge(),
            NOW,
        );
        assert_eq!(decision.reason, EntryReason::PoolTooThin);
    }

    #[test]
    fn a_completed_curve_will_not_price_and_is_not_guessed_at() {
        let curve = CurveState {
            complete: true,
            ..a_deep_curve()
        };
        let decision = plan_entry(
            &accepted(950_000),
            &SocialWeight::unscanned(),
            &a_healthy_snapshot(),
            &an_account(),
            &curve,
            &with_an_imagined_edge(),
            NOW,
        );
        assert_eq!(decision.reason, EntryReason::Unquotable);
        assert!(!decision.stress.measured);
    }

    // -----------------------------------------------------------------------
    // Tiers
    // -----------------------------------------------------------------------

    #[test]
    fn the_tier_boundaries_are_the_ones_doctrine_names() {
        assert_eq!(Tier::for_confidence(1_000_000), Tier::One);
        assert_eq!(Tier::for_confidence(850_000), Tier::One);
        assert_eq!(Tier::for_confidence(849_999), Tier::Two);
        assert_eq!(Tier::for_confidence(700_000), Tier::Two);
        assert_eq!(Tier::for_confidence(699_999), Tier::Three);
        assert_eq!(Tier::for_confidence(550_000), Tier::Three);
        assert_eq!(Tier::for_confidence(549_999), Tier::ObserveOnly);
        assert_eq!(Tier::for_confidence(0), Tier::ObserveOnly);
    }

    #[test]
    fn a_tier_two_position_is_half_a_tier_one_one() {
        let params = with_an_imagined_edge();
        let one = plan(&accepted(900_000), &params);
        let two = plan(&accepted(750_000), &params);
        assert_eq!(one.tier, Tier::One);
        assert_eq!(two.tier, Tier::Two);
        assert_eq!(
            two.caps.after_tier_lamports,
            one.caps.after_tier_lamports / 2
        );
    }

    #[test]
    fn tier_three_is_sized_and_then_handed_to_a_person() {
        let decision = plan(&accepted(600_000), &with_an_imagined_edge());
        assert_eq!(decision.tier, Tier::Three);
        assert!(!decision.enter);
        assert_eq!(decision.reason, EntryReason::OperatorConfirmationRequired);
        // Sized, so the person is confirming a number rather than a hunch.
        assert!(decision.size_lamports > 0);
        assert!(decision.exit.is_some());
        assert!(decision.ev.positive);
    }

    #[test]
    fn tier_three_trades_only_when_the_policy_says_a_person_already_said_yes() {
        let params = EntryParams {
            allow_tier_three: true,
            ..with_an_imagined_edge()
        };
        let decision = plan(&accepted(600_000), &params);
        assert!(decision.enter);
        assert_eq!(decision.caps.tier_multiplier_bps, 1_000);
    }

    #[test]
    fn below_the_bottom_tier_there_is_nothing_to_size() {
        let decision = plan(&accepted(500_000), &with_an_imagined_edge());
        assert_eq!(decision.tier, Tier::ObserveOnly);
        assert_eq!(decision.reason, EntryReason::ObserveOnly);
        assert_eq!(decision.size_lamports, 0);
    }

    // -----------------------------------------------------------------------
    // The chain
    // -----------------------------------------------------------------------

    #[test]
    fn the_participation_cap_is_the_tighter_of_policy_and_the_snapshot() {
        // The snapshot says 5%, doctrine says 1.5%. Doctrine binds.
        let decision = plan(&accepted(900_000), &with_an_imagined_edge());
        let curve = a_deep_curve();
        assert_eq!(
            decision.caps.pool_lamports,
            curve.max_position_lamports(MAX_POOL_SHARE_BPS)
        );
        assert!(decision.caps.pool_lamports < curve.max_position_lamports(500));
    }

    #[test]
    fn a_laxer_snapshot_cannot_widen_the_doctrine_cap() {
        let snapshot = RiskSnapshot {
            liquidity: LiquidityThresholds {
                max_pool_share_bps: 9_000,
                ..thresholds()
            },
            ..a_healthy_snapshot()
        };
        let decision = plan_entry(
            &accepted(900_000),
            &SocialWeight::unscanned(),
            &snapshot,
            &an_account(),
            &a_deep_curve(),
            &with_an_imagined_edge(),
            NOW,
        );
        assert_eq!(
            decision.caps.pool_lamports,
            a_deep_curve().max_position_lamports(MAX_POOL_SHARE_BPS)
        );
    }

    #[test]
    fn a_tighter_snapshot_does_bind() {
        let snapshot = RiskSnapshot {
            liquidity: LiquidityThresholds {
                max_pool_share_bps: 10,
                ..thresholds()
            },
            ..a_healthy_snapshot()
        };
        let decision = plan_entry(
            &accepted(900_000),
            &SocialWeight::unscanned(),
            &snapshot,
            &an_account(),
            &a_deep_curve(),
            &with_an_imagined_edge(),
            NOW,
        );
        assert_eq!(
            decision.caps.pool_lamports,
            a_deep_curve().max_position_lamports(10)
        );
    }

    #[test]
    fn the_binding_cap_is_named() {
        let account = Account {
            operator_max_notional_lamports: SOL / 1_000,
            ..an_account()
        };
        let decision = plan_entry(
            &accepted(900_000),
            &SocialWeight::unscanned(),
            &a_healthy_snapshot(),
            &account,
            &a_deep_curve(),
            &with_an_imagined_edge(),
            NOW,
        );
        assert_eq!(decision.caps.binding, SizeCap::Operator);
        assert_eq!(decision.caps.base_lamports, SOL / 1_000);
    }

    #[test]
    fn a_small_risk_budget_binds_before_the_pool_does() {
        let account = Account {
            risk_budget_lamports: SOL / 10_000,
            ..an_account()
        };
        let decision = plan_entry(
            &accepted(900_000),
            &SocialWeight::unscanned(),
            &a_healthy_snapshot(),
            &account,
            &a_deep_curve(),
            &with_an_imagined_edge(),
            NOW,
        );
        assert_eq!(decision.caps.binding, SizeCap::RiskBudget);
        // The budget buys `budget / stressed_loss` of position, and the stressed
        // loss is most of the position, so the size is a small multiple of it.
        assert!(decision.caps.base_lamports < 4 * account.risk_budget_lamports);
    }

    #[test]
    fn the_gate_ceiling_is_only_in_the_chain_on_the_fast_route() {
        let slow = plan(&accepted(900_000), &with_an_imagined_edge());
        assert_eq!(slow.caps.gate_lamports, None);

        let fast = EntryParams {
            fast_path: true,
            ..with_an_imagined_edge()
        };
        let decision = plan(&accepted(900_000), &fast);
        assert_eq!(decision.caps.gate_lamports, Some(SOL));
    }

    #[test]
    fn the_fast_route_also_takes_the_gates_slippage_ceiling() {
        let fast = EntryParams {
            fast_path: true,
            max_slippage_bps: 900,
            ..with_an_imagined_edge()
        };
        let decision = plan(&accepted(900_000), &fast);
        assert_eq!(decision.max_slippage_bps, 250);
    }

    #[test]
    fn a_shut_fast_path_refuses_the_fast_route() {
        let snapshot = RiskSnapshot {
            fast_path: FastPathGate::CLOSED,
            ..a_healthy_snapshot()
        };
        let params = EntryParams {
            fast_path: true,
            ..with_an_imagined_edge()
        };
        let decision = plan_entry(
            &accepted(900_000),
            &SocialWeight::unscanned(),
            &snapshot,
            &an_account(),
            &a_deep_curve(),
            &params,
            NOW,
        );
        assert_eq!(decision.reason, EntryReason::FastPathRefused);
    }

    #[test]
    fn the_micro_live_ceiling_binds_after_everything_else() {
        // Gate 6D: 0.05 SOL whatever the chain computed.
        let params = EntryParams {
            edge_lcb_bps: 5_000,
            ..EntryParams::default()
        };
        let decision = plan(&accepted(900_000), &params);
        assert_eq!(decision.size_lamports, MICRO_LIVE_CAP_LAMPORTS);
        assert_eq!(decision.caps.binding, SizeCap::HardCap);
        assert!(decision.caps.after_social_lamports > MICRO_LIVE_CAP_LAMPORTS);
    }

    #[test]
    fn a_position_too_small_to_be_worth_the_round_trip_is_refused() {
        let account = Account {
            free_equity_lamports: 1_000,
            ..an_account()
        };
        let decision = plan_entry(
            &accepted(900_000),
            &SocialWeight::unscanned(),
            &a_healthy_snapshot(),
            &account,
            &a_deep_curve(),
            &with_an_imagined_edge(),
            NOW,
        );
        assert_eq!(decision.reason, EntryReason::BelowMinNotional);
    }

    // -----------------------------------------------------------------------
    // The story
    // -----------------------------------------------------------------------

    #[test]
    fn a_farmed_story_takes_size_off_and_never_adds_any() {
        let params = with_an_imagined_edge();
        let clean = plan(&accepted(900_000), &params);

        let scan = SocialScan {
            reuse_nth: 9,
            ..SocialScan::no_link()
        };
        let weighed = plan_entry(
            &accepted(900_000),
            &weigh(Some(&scan), &params.social),
            &a_healthy_snapshot(),
            &an_account(),
            &a_deep_curve(),
            &params,
            NOW,
        );
        assert!(weighed.social.has(SocialCaution::FarmedStory));
        assert_eq!(weighed.caps.social_multiplier_bps, 5_000);
        assert_eq!(weighed.size_lamports, clean.size_lamports / 2);
        assert!(weighed.size_lamports < clean.size_lamports);
    }

    #[test]
    fn a_forged_weight_cannot_size_a_position_past_the_risk_chain() {
        // `plan_entry` takes a weight rather than a scan, so the thing that
        // reaches it may not have come from `weigh` at all — a replayed decision
        // and an IPC projection both deserialise into this type. A multiplier
        // above one would otherwise walk straight past the cap that bound the
        // position, which is the one thing this module promises it cannot do.
        let params = with_an_imagined_edge();
        let unscanned = plan(&accepted(900_000), &params);
        let forged = SocialWeight {
            multiplier_bps: u16::MAX,
            ..SocialWeight::unscanned()
        };
        let decision = plan_entry(
            &accepted(900_000),
            &forged,
            &a_healthy_snapshot(),
            &an_account(),
            &a_deep_curve(),
            &params,
            NOW,
        );
        assert!(unscanned.enter && decision.enter);
        assert_eq!(decision.size_lamports, unscanned.size_lamports);
        assert_eq!(decision.caps.social_multiplier_bps, BPS_DENOMINATOR as u16);
    }

    #[test]
    fn the_best_possible_story_sizes_exactly_like_no_story_at_all() {
        let params = with_an_imagined_edge();
        let unscanned = plan(&accepted(900_000), &params);
        let scan = SocialScan {
            kind: StoryKind::Tweet,
            handle: Some("someone".to_string()),
            followers: Some(4_000_000),
            account_age_days: Some(3_000),
            post_age_ms: Some(1_000),
            reuse_nth: 1,
            views: vec![
                ViewSample {
                    at_ms: 0,
                    views: 1_000,
                },
                ViewSample {
                    at_ms: 120_000,
                    views: 90_000,
                },
            ],
        };
        let weighed = plan_entry(
            &accepted(900_000),
            &weigh(Some(&scan), &params.social),
            &a_healthy_snapshot(),
            &an_account(),
            &a_deep_curve(),
            &params,
            NOW,
        );
        assert_eq!(weighed.size_lamports, unscanned.size_lamports);
    }

    #[test]
    fn a_story_can_shrink_a_position_under_the_floor_and_refuse_it() {
        let account = Account {
            operator_max_notional_lamports: MIN_NOTIONAL_LAMPORTS,
            ..an_account()
        };
        let params = with_an_imagined_edge();
        let scan = SocialScan {
            reuse_nth: 9,
            ..SocialScan::no_link()
        };
        let decision = plan_entry(
            &accepted(900_000),
            &weigh(Some(&scan), &params.social),
            &a_healthy_snapshot(),
            &account,
            &a_deep_curve(),
            &params,
            NOW,
        );
        assert_eq!(decision.reason, EntryReason::BelowMinNotional);
    }

    // -----------------------------------------------------------------------
    // The way out
    // -----------------------------------------------------------------------

    #[test]
    fn every_accepted_entry_carries_a_priced_exit() {
        let decision = plan(&accepted(900_000), &with_an_imagined_edge());
        let exit = decision.exit.expect("an accepted entry has an exit");
        assert!(exit.tokens > 0);
        assert!(exit.net_lamports > 0);
        assert!(exit.within_ceiling);
        assert!(exit.slippage_bps <= EMERGENCY_MAX_SLIPPAGE_BPS);
        // And what it would be worth in the worst modelled fall, which is
        // carried for the operator and does not gate.
        assert_eq!(exit.gap_bps, 5_000);
        assert!(exit.gapped_net_lamports < exit.net_lamports);
    }

    #[test]
    fn a_position_a_young_pool_could_not_pay_for_reports_no_gapped_value() {
        let curve = CurveState::at_real_sol(10 * SOL);
        let size = curve.max_position_lamports(MAX_POOL_SHARE_BPS);
        let exit = exit_readiness(&curve, size, &EntryParams::default())
            .expect("the exit prices at the current depth");
        assert!(exit.net_lamports > 0, "it can be sold today");
        assert_eq!(exit.gapped_net_lamports, 0, "and not after that fall");
    }

    #[test]
    fn an_exit_that_would_fill_past_the_emergency_ceiling_refuses_the_entry() {
        // A ceiling of one basis point cannot be met by any real fill, which is
        // the same refusal a pool too thin to leave would produce.
        let params = EntryParams {
            emergency_max_slippage_bps: 1,
            ..with_an_imagined_edge()
        };
        let decision = plan(&accepted(900_000), &params);
        assert_eq!(decision.reason, EntryReason::ExitNotReady);
        assert!(decision.exit.is_some(), "the priced exit is still reported");
        assert!(!decision.exit.expect("priced").within_ceiling);
    }

    // -----------------------------------------------------------------------
    // The stress set
    // -----------------------------------------------------------------------

    #[test]
    fn the_worst_bucket_is_the_worst_of_both_sets() {
        let report = stress(&a_deep_curve(), SOL / 10, DEFAULT_FEE_BPS).expect("priced");
        assert!(report.measured);
        assert_eq!(report.worst_gap_bps, 5_000);
        assert_eq!(report.worst_slippage_bps, 2_500);
        // Half the position gone to the gap, a quarter of the rest to the fill,
        // and the fees on top: well past 60%.
        assert!(report.loss_bps > 6_000, "{}", report.loss_bps);
        assert!(report.loss_bps <= BPS_DENOMINATOR as u16);
    }

    #[test]
    fn a_gap_costs_nearly_the_same_share_of_every_position() {
        // Nearly, and slightly less of a larger one: the entry's own SOL deepens
        // the pool it is about to be gapped in. The spread is what
        // `stress_range` exists to bound, so it is worth pinning down rather
        // than assuming — a curve model that made it wide would silently make
        // the risk-budget cap wrong.
        let curve = a_deep_curve();
        let sizes = [SOL / 100, SOL / 10, 6 * SOL / 10, SOL, 10 * SOL];
        let losses: Vec<u16> = sizes
            .iter()
            .map(|&size| {
                stress(&curve, size, DEFAULT_FEE_BPS)
                    .expect("priced")
                    .loss_bps
            })
            .collect();
        let smallest = *losses.first().expect("five sizes");
        let largest = *losses.last().expect("five sizes");
        assert!(smallest > 8_000, "{losses:?}");
        assert!(
            largest <= smallest,
            "a larger position lost more: {losses:?}"
        );
        assert!(
            smallest - largest < 400,
            "the spread is not small: {losses:?}"
        );
    }

    #[test]
    fn the_range_takes_the_worse_of_the_two_ends() {
        let curve = a_deep_curve();
        let floor = stress(&curve, MIN_NOTIONAL_LAMPORTS, DEFAULT_FEE_BPS).expect("priced");
        let cap = stress(
            &curve,
            curve.max_position_lamports(MAX_POOL_SHARE_BPS),
            DEFAULT_FEE_BPS,
        )
        .expect("priced");
        let ranged = stress_range(
            &curve,
            MIN_NOTIONAL_LAMPORTS,
            curve.max_position_lamports(MAX_POOL_SHARE_BPS),
            DEFAULT_FEE_BPS,
        )
        .expect("priced");
        assert_eq!(ranged.loss_bps, floor.loss_bps.max(cap.loss_bps));
    }

    #[test]
    fn a_curve_too_young_to_survive_the_deepest_gap_says_so() {
        // Ten SOL of real reserve against thirty of virtual: selling can move
        // the price 25% at the very most, so *both* buckets drain the pool and
        // the shallower one is already a total loss. The model says so rather
        // than quoting a number for an exit that does not exist — and the bucket
        // it names is the 30% one, because that is where the loss first reached
        // the ceiling and nothing after it could be worse.
        let report = stress(
            &CurveState::at_real_sol(10 * SOL),
            SOL / 10,
            DEFAULT_FEE_BPS,
        )
        .expect("priced");
        assert!(report.no_executable_exit);
        assert_eq!(report.loss_bps, BPS_DENOMINATOR as u16);
        assert_eq!(report.worst_gap_bps, 3_000);
    }

    #[test]
    fn a_deep_curve_survives_the_same_gap_with_something_left() {
        let report = stress(&a_deep_curve(), SOL / 10, DEFAULT_FEE_BPS).expect("priced");
        assert!(!report.no_executable_exit);
        assert!(report.loss_bps < BPS_DENOMINATOR as u16);
    }

    #[test]
    fn the_round_trip_costs_both_fees_and_nothing_else() {
        // A buy and an immediate sell move the price up and back down through
        // the same reserves, so the impact cancels exactly and what is left is
        // the two fees compounded: 1.99%, not 2%.
        let report = stress(&a_deep_curve(), SOL, DEFAULT_FEE_BPS).expect("priced");
        assert_eq!(report.round_trip_cost_lamports, 199 * SOL / 10_000);
    }

    #[test]
    fn a_size_of_nothing_has_nothing_to_stress() {
        assert_eq!(stress(&a_deep_curve(), 0, DEFAULT_FEE_BPS), None);
    }

    #[test]
    fn a_loss_is_never_reported_smaller_than_it_was() {
        // Rounds up. One lamport of ten thousand is one basis point exactly;
        // one of nine thousand is more than one and reports two.
        assert_eq!(loss_bps(10_000, 9_999), 1);
        assert_eq!(loss_bps(9_000, 8_999), 2);
        assert_eq!(loss_bps(100, 0), BPS_DENOMINATOR as u16);
        assert_eq!(loss_bps(100, 100), 0);
        assert_eq!(loss_bps(0, 0), 0);
    }

    // -----------------------------------------------------------------------
    // Expiry
    // -----------------------------------------------------------------------

    #[test]
    fn a_plan_is_dated_by_the_snapshot_it_was_made_against() {
        let decision = plan(&accepted(900_000), &with_an_imagined_edge());
        assert_eq!(decision.decided_at_ms, NOW);
        assert_eq!(decision.expires_at_ms, NOW + DECISION_TTL_MS);
    }

    #[test]
    fn a_plan_that_has_expired_cannot_be_signed() {
        let decision = plan(&accepted(900_000), &with_an_imagined_edge());
        assert!(decision.is_signable(NOW));
        assert!(decision.is_signable(NOW + DECISION_TTL_MS - 1));
        assert!(!decision.is_signable(NOW + DECISION_TTL_MS));
        assert!(!decision.is_signable(NOW + DECISION_TTL_MS + 1));
    }

    #[test]
    fn a_refusal_is_never_signable_however_fresh_it_is() {
        let decision = plan(&refused(), &with_an_imagined_edge());
        assert!(!decision.is_signable(NOW));
    }

    // -----------------------------------------------------------------------
    // The whole path
    // -----------------------------------------------------------------------

    fn the_script() -> LaunchRecord {
        LaunchRecord {
            mint: "MINT".to_string(),
            creator: None,
            buyers: (1..=6)
                .map(|n| OpeningBuyer {
                    wallet: format!("w{n}"),
                    sol_in_lamports: 777_700_000,
                    sol_out_lamports: 0,
                    tx_count: 1,
                    first_seen_ms: 2_000,
                })
                .collect(),
            funding: Vec::new(),
        }
    }

    #[test]
    fn a_scripted_launch_walks_all_the_way_to_a_plan() {
        let policy = Policy {
            entry: with_an_imagined_edge(),
            ..Policy::default()
        };
        let (report, verdict, decision) = decide(
            &the_script(),
            None,
            None,
            &a_healthy_snapshot(),
            &an_account(),
            &a_deep_curve(),
            &policy,
            NOW,
        );
        assert_eq!(verdict.reason, GateReason::Accepted);
        // Six wallets, one repeated size, one instant, and no funding graph to
        // corroborate it with: high conviction, not the highest.
        assert_eq!(report.confidence_micros, 800_000);
        assert_eq!(decision.tier, Tier::Two);
        assert!(decision.enter);
        assert_eq!(decision.caps.social_multiplier_bps, BPS_DENOMINATOR as u16);
    }

    #[test]
    fn the_same_launch_and_the_same_policy_produce_the_same_plan_twice() {
        let policy = Policy {
            entry: with_an_imagined_edge(),
            ..Policy::default()
        };
        let scan = SocialScan {
            reuse_nth: 3,
            views: vec![
                ViewSample {
                    at_ms: 0,
                    views: 900,
                },
                ViewSample {
                    at_ms: 120_000,
                    views: 1_000,
                },
            ],
            ..SocialScan::no_link()
        };
        let run = || {
            decide(
                &the_script(),
                Some(&scan),
                None,
                &a_healthy_snapshot(),
                &an_account(),
                &a_deep_curve(),
                &policy,
                NOW,
            )
        };
        let (first_report, first_verdict, first) = run();
        let (second_report, second_verdict, second) = run();
        assert_eq!(first_report, second_report);
        assert_eq!(first_verdict, second_verdict);
        assert_eq!(first, second);
        assert_eq!(
            serde_json::to_string(&first).expect("serialises"),
            serde_json::to_string(&second).expect("serialises")
        );
    }

    #[test]
    fn a_launch_the_analyser_refuses_never_reaches_the_chain() {
        let quiet = LaunchRecord {
            mint: "QUIET".to_string(),
            creator: None,
            buyers: Vec::new(),
            funding: Vec::new(),
        };
        let (_, verdict, decision) = decide(
            &quiet,
            None,
            None,
            &a_healthy_snapshot(),
            &an_account(),
            &a_deep_curve(),
            &Policy::default(),
            NOW,
        );
        assert_eq!(verdict.reason, GateReason::NoOpeningBuys);
        assert_eq!(decision.reason, EntryReason::GateRefused);
        assert!(!decision.stress.measured);
    }

    // -----------------------------------------------------------------------
    // Nothing panics, nothing leaves its range
    // -----------------------------------------------------------------------

    #[test]
    fn every_reason_has_a_distinct_name_and_they_are_all_listed() {
        let mut names: Vec<&str> = EntryReason::ALL.iter().map(|r| r.as_str()).collect();
        let listed = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), listed);
        for tier in Tier::ALL {
            assert!(!tier.as_str().is_empty());
        }
        for cap in [
            SizeCap::RiskBudget,
            SizeCap::Pool,
            SizeCap::FastPathGate,
            SizeCap::Operator,
            SizeCap::Equity,
            SizeCap::HardCap,
        ] {
            assert!(!cap.as_str().is_empty());
        }
    }

    #[test]
    fn nothing_panics_and_no_size_escapes_its_caps() {
        let curves = [
            CurveState::at_real_sol(0),
            CurveState::at_real_sol(SOL),
            CurveState::at_real_sol(40 * SOL),
            CurveState::at_real_sol(84 * SOL),
            CurveState {
                virtual_sol_reserves: LAUNCH_VIRTUAL_SOL_RESERVES,
                ..CurveState::at_real_sol(40 * SOL)
            },
        ];
        let accounts = [
            Account::EMPTY,
            an_account(),
            Account {
                risk_budget_lamports: u64::MAX,
                free_equity_lamports: u64::MAX,
                operator_max_notional_lamports: u64::MAX,
            },
        ];
        let params_set = [
            EntryParams::default(),
            with_an_imagined_edge(),
            EntryParams {
                fast_path: true,
                allow_tier_three: true,
                edge_lcb_bps: u16::MAX,
                hard_cap_lamports: 0,
                min_notional_lamports: 0,
                max_pool_share_bps: u16::MAX,
                ..EntryParams::default()
            },
        ];
        // The snapshot is the half of the input the engine supplies rather than
        // the launch, and a value in it that is nonsense — a window that admits
        // nothing, a cap of `u16::MAX`, a reading from the future — must still
        // come out as a refusal rather than as a panic or a position.
        let snapshots = [
            a_healthy_snapshot(),
            RiskSnapshot {
                liquidity: LiquidityThresholds {
                    min_pool_lamports: u64::MAX,
                    exit_only_below_lamports: u64::MAX,
                    max_pool_share_bps: u16::MAX,
                },
                ..a_healthy_snapshot()
            },
            RiskSnapshot {
                at_ms: i64::MAX,
                drawdown_bps: u16::MAX,
                max_drawdown_bps: 0,
                open_positions: u16::MAX,
                max_open_positions: 0,
                ..a_healthy_snapshot()
            },
            RiskSnapshot {
                fast_path: FastPathGate {
                    allowed: false,
                    remaining_in_window: 0,
                    max_notional_lamports: 0,
                    max_slippage_bps: u16::MAX,
                },
                ..a_healthy_snapshot()
            },
        ];
        // A weight `weigh` would never build, because `plan_entry` is handed a
        // weight rather than a scan and a replayed one deserialises from a file.
        let stories = [
            SocialWeight::unscanned(),
            SocialWeight {
                multiplier_bps: 0,
                ..SocialWeight::unscanned()
            },
            SocialWeight {
                multiplier_bps: u16::MAX,
                ..SocialWeight::unscanned()
            },
        ];
        for curve in curves {
            for account in accounts {
                for params in &params_set {
                    for snapshot in &snapshots {
                        for confidence in [0u64, 549_999, 700_000, 1_000_000, u64::MAX] {
                            let plain = plan_entry(
                                &accepted(confidence),
                                &SocialWeight::unscanned(),
                                snapshot,
                                &account,
                                &curve,
                                params,
                                NOW,
                            );
                            for story in &stories {
                                let decision = plan_entry(
                                    &accepted(confidence),
                                    story,
                                    snapshot,
                                    &account,
                                    &curve,
                                    params,
                                    NOW,
                                );
                                assert!(decision.size_lamports <= decision.caps.base_lamports);
                                assert!(decision.size_lamports <= params.hard_cap_lamports);
                                assert!(
                                    decision.caps.social_multiplier_bps <= BPS_DENOMINATOR as u16
                                );
                                // Whatever the story says, it never puts a
                                // lamport on what the risk chain allowed.
                                assert!(decision.size_lamports <= plain.size_lamports);
                                if decision.enter {
                                    assert!(decision.size_lamports >= params.min_notional_lamports);
                                    assert!(decision.exit.is_some());
                                    assert!(decision.ev.positive);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_percentage_a_person_reads_never_leaves_a_hundred() {
        assert_eq!(confidence_percent(0), 0);
        assert_eq!(confidence_percent(555_000), 55);
        assert_eq!(confidence_percent(MICROS), 100);
        assert_eq!(confidence_percent(u64::MAX), 100);
    }
}
