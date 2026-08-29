//! # Flagged dead by the salvage audit — 2026-08-27
//!
//! **Nothing in the shipped application references this module.** It is
//! declared `pub mod` in `lib.rs` and reached by no file in `src/` at all;
//! only `tests/replay_tests.rs` touches it.
//!
//! It is left here, compiling and tested, on purpose. Removing it is a
//! decision for a human to make in one reviewed commit, not a sweep. See
//! `docs/SALVAGE.md` for what that decision involves. The whole tree as it
//! stood before any salvage action is recoverable with
//! `git checkout pre-salvage-2026-08-27`.
//!
//! ---
//!
//! Where a backtest's money went, split into lines somebody can argue with.
//!
//! `backtest.rs` reports what a run made. This reports *why*. A strategy that
//! returned −180 bps has one number and four possible stories behind it — the
//! edge was never there, the edge was there and the curve ate it, the edge was
//! there and the block market ate it, the edge was there and somebody else got
//! to it first — and a run that cannot tell them apart cannot be fixed.
//!
//! # The identity
//!
//! Every closed round trip decomposes into exactly this, in lamports:
//!
//! ```text
//! realised PnL = gross alpha
//!              − price impact
//!              − protocol fees
//!              − Jito tips
//!              − MEV penalty
//!              + residual
//! ```
//!
//! **Gross alpha** is the price move on the notional and nothing else: what the
//! entry stake would have become if it had traded at the curve's marginal price
//! at both ends, paid nothing, and had nobody in front of it. It is the only
//! term that can be positive.
//!
//! **The four cost lines** are what stood between that and the fill. Price
//! impact is the curve's own convexity — the difference between the marginal
//! price and the average one, which is a cost of size and exists with nobody
//! else on the network at all. Protocol fees are the venue's cut. Tips are what
//! the block market charged to land the exit. The MEV penalty is what
//! [`crate::mev_sim`]'s synthetic adversary took, and it is *only* the marginal
//! damage that adversary did: the impact our own order would have caused anyway
//! is already on the impact line and is not charged twice.
//!
//! **The residual** is arithmetic, not economics. Every term above is an integer
//! division that floors, and the floors do not cancel. It is bounded by
//! [`TradeAttribution::residual_bound_lamports`] — a handful of lamports against
//! notionals in the billions — and a residual outside that bound is a bug in
//! this file rather than a finding about the strategy, which is why the tests
//! assert on it rather than reporting it and moving on.
//!
//! # Why entry-side costs are carried forward
//!
//! A cost line here answers one question: *how many lamports of the final payout
//! did this destroy?* For an exit-side cost that is its face value. For a cost
//! taken **out of the stake** at entry it is not — a lamport the venue kept at
//! entry is a lamport that did not buy tokens, and the tokens it did not buy
//! would have ridden the price move. A 10 000-lamport entry fee on a trade that
//! tripled destroyed 30 000 lamports of proceeds, not 10 000.
//!
//! So the entry fee, the entry impact and the entry MEV penalty are each
//! multiplied by the trade's own price ratio before they are reported, and
//! [`TradeAttribution::carry_ratio_micros`] is that ratio in millionths. The
//! fees actually charged are reported beside it —
//! [`TradeAttribution::fees_charged_lamports`] — so nothing is hidden by the
//! choice; the two are answers to two different questions.
//!
//! **The tip is the exception, and it is not an oversight.** A tip is not taken
//! out of the stake, it is paid beside it: the same order buys the same tokens
//! whatever the bundle bid. So a tip lamport never had a position to ride the
//! move with, its cost to the payout is its face value, and carrying it would
//! put a term of `tip x (ratio - 1)` into the residual that is not rounding and
//! would not be bounded by anything.
//!
//! # No floating point, anywhere
//!
//! Every number in every struct here is an integer in a named unit, every
//! summary type derives `Eq`, and the transcendental the log-return column needs
//! comes from [`crate::strategy::fixed`], which computes it in `u128` at
//! `10^-18`. The reason is the one `backtest.rs` gives about its own arithmetic:
//! `f64::ln` is not specified to the last bit by IEEE 754, two machines that
//! link different libms genuinely disagree there, and a report that has to be
//! byte-identical between two runs cannot contain a number that depends on which
//! machine produced it.
//!
//! `Eq` on a summary struct is the part that makes that testable. Two reports
//! either are the same report or they are not, and `assert_eq!` is allowed to be
//! the whole of the equivalence check.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::backtest::{
    floor_div_i128, isqrt, lamports_to_usd_cents, mul_div_ceil, mul_div_floor, LaunchEvent, Side,
    MICROS,
};
use crate::execution::{
    EXIT_TIP_BASE_LAMPORTS, EXIT_TIP_ESCALATION_LAMPORTS, EXIT_TIP_MAX_LAMPORTS,
    EXIT_TIP_PARTICIPATION_BPS,
};
use crate::mev_sim::{
    buy_through, curve_price_micros, sell_through, AdversaryConfig, AdversaryProfile,
    MarketContext, MevOutcome, MevSummary,
};
use crate::replay::{
    CurveState, FeeSplit, QuoteError, BPS_DENOMINATOR, DEFAULT_CREATOR_FEE_BPS, DEFAULT_FEE_BPS,
    LAMPORTS_PER_SOL,
};
use crate::strategy::fixed::ln_fixed;

/// The schema string on the report this module emits.
pub const ATTRIBUTION_SCHEMA: &str = "sts.backtest.attribution.v2";

/// How many one-lamport floors the identity is allowed to accumulate per unit of
/// price ratio.
///
/// The entry-side chain floors four times before it is carried and the carry
/// floors once per line; the exit side is exact by construction except for the
/// marginal-value floor. Eight is roughly twice the count, which is the margin a
/// bound wants when the thing it is bounding would otherwise need a proof.
pub const RESIDUAL_FLOORS: u64 = 8;

/// The upper edges of the slippage histogram, in basis points.
///
/// Fixed rather than derived from the sample, because a histogram whose buckets
/// move with the data cannot be compared between two runs — and comparing two
/// runs is the entire job of this module's output.
pub const SLIPPAGE_BUCKET_EDGES_BPS: [u16; 9] = [10, 25, 50, 100, 250, 500, 1_000, 2_500, 10_000];

// ===========================================================================
// What a tip costs, as the backtest sees it
// ===========================================================================

/// Annex C's tip pricing, as a pure function.
///
/// `execution::TipPolicy` is the same arithmetic attached to an account list, a
/// round-robin cursor and the refusals a live send needs. A backtest wants none
/// of those — it is not choosing a tip account, and Annex C.2's discretionary
/// refusal would drop the tip on exactly the losing exits where the cost is
/// realest — so this carries the pricing alone. The two agree, and
/// `tip_schedule_matches_execution_policy` in the tests is that agreement
/// pinned: a change to Annex C that lands in one and not the other fails there.
///
/// # The congestion term
///
/// The one thing here that `TipPolicy` does not have. A tip is a bid into a
/// block market, and that market is not equally contested at every moment: the
/// moments an adversary finds worth attacking are the moments block space is
/// worth bidding for. So the same intensity blend that sizes the adversary in
/// [`crate::mev_sim`] moves the tip, and the two are deliberately the same
/// number — a run cannot be simultaneously told that a moment was quiet enough
/// to be safe and contested enough to be expensive.
///
/// It bids a share of the headroom between the floor and the ceiling rather than
/// a share of the trade, so a fully contested block is still bounded by
/// `Tip_max` and no amount of congestion can spend a position on tips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TipSchedule {
    /// `Tip_base`: what every bid starts from.
    pub base_lamports: u64,
    /// `Tip_max`: the ceiling, and the only thing standing between a retry loop
    /// and a position.
    pub max_lamports: u64,
    /// `α`, in basis points of expected profit.
    pub participation_bps: u16,
    /// `ΔTip`: what one more attempt adds.
    pub escalation_lamports: u64,
    /// What a fully contested block adds, in basis points of the headroom
    /// between the floor and the ceiling.
    pub congestion_bps: u16,
}

impl Default for TipSchedule {
    fn default() -> Self {
        TipSchedule::annex_c()
    }
}

impl TipSchedule {
    /// The published numbers: `execution`'s four constants, plus a congestion
    /// term that bids a quarter of the headroom at full contention.
    ///
    /// The quarter is a policy choice with nothing measured behind it, and it is
    /// a field rather than a literal in the arithmetic so a run that disagrees
    /// can say so in its own report.
    pub const fn annex_c() -> Self {
        TipSchedule {
            base_lamports: EXIT_TIP_BASE_LAMPORTS,
            max_lamports: EXIT_TIP_MAX_LAMPORTS,
            participation_bps: EXIT_TIP_PARTICIPATION_BPS,
            escalation_lamports: EXIT_TIP_ESCALATION_LAMPORTS,
            congestion_bps: 2_500,
        }
    }

    /// A schedule that never bids anything but the floor. The control against
    /// which the dynamic terms are read.
    pub const fn flat(base_lamports: u64) -> Self {
        TipSchedule {
            base_lamports,
            max_lamports: base_lamports,
            participation_bps: 0,
            escalation_lamports: 0,
            congestion_bps: 0,
        }
    }

    /// What to bid, in lamports.
    ///
    /// `ev_net_lamports` is what the trade is expected to make before the tip
    /// and after everything else. `None`, or anything at or below zero, adds no
    /// participation term — Annex C.2's rule that a share of a profit nobody
    /// computed is not a smaller share, it is a made-up one.
    ///
    /// Clamped to `[base, max]` at the end, so the floor holds even against a
    /// malformed schedule whose ceiling is under it.
    pub fn bid_lamports(
        &self,
        ev_net_lamports: Option<i64>,
        attempt: u32,
        congestion_micros: u64,
    ) -> u64 {
        let participation = match ev_net_lamports {
            Some(ev) if ev > 0 => mul_div_floor(
                u128::from(ev.unsigned_abs()),
                u128::from(self.participation_bps),
                u128::from(BPS_DENOMINATOR),
            )
            .min(u128::from(u64::MAX)) as u64,
            _ => 0,
        };
        let escalation = self.escalation_lamports.saturating_mul(u64::from(attempt));
        let headroom = self.max_lamports.saturating_sub(self.base_lamports);
        let congestion = mul_div_floor(
            u128::from(headroom).saturating_mul(u128::from(self.congestion_bps)),
            u128::from(congestion_micros.min(MICROS)),
            u128::from(BPS_DENOMINATOR) * u128::from(MICROS),
        )
        .min(u128::from(u64::MAX)) as u64;

        self.base_lamports
            .saturating_add(participation)
            .saturating_add(escalation)
            .saturating_add(congestion)
            .clamp(
                self.base_lamports,
                self.base_lamports.max(self.max_lamports),
            )
    }
}

// ===========================================================================
// The run configuration
// ===========================================================================

/// Everything the attribution depends on besides the trades themselves.
///
/// Serialised into the report for the reason `BacktestConfig` is: a cost line
/// without the policy it was computed under is not reproducible, and the whole
/// claim of this module is that the report is a function of the executions and
/// this struct and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributionConfig {
    /// The swap fee on the SOL leg, in basis points.
    pub fee_bps: u16,
    /// The creator's share of `fee_bps`, in basis points. The venue keeps the
    /// rest.
    ///
    /// A share of the fee rather than a fee of its own, deliberately: the sum is
    /// what a fill is charged and it is `fee_bps` whatever this is set to, so
    /// changing it moves a lamport from one column of the report to another and
    /// never moves the report's bottom line. `AttributionConfig::fee_split` is
    /// where that is arranged, and it clamps rather than trusting: a creator
    /// share larger than the whole fee would otherwise produce a negative
    /// protocol share, which is not a thing.
    ///
    /// Defaults to zero when it is missing from a stored config, which is the
    /// honest reading of a report written before this column existed: the run
    /// did not split the fee, so nothing should claim it did.
    #[serde(default)]
    pub creator_fee_bps: u16,
    pub starting_equity_lamports: u64,
    /// What SOL is worth, in whole US cents. Zero means the report carries no
    /// dollar figures rather than guessing at a price.
    pub cents_per_sol: u64,
    pub tips: TipSchedule,
    pub adversary: AdversaryConfig,
    /// How the charges a fill does not record are priced, and how the one it
    /// does record is split. See [`FeeSchedule`].
    pub fees: FeeSchedule,
}

impl Default for AttributionConfig {
    fn default() -> Self {
        AttributionConfig {
            fee_bps: DEFAULT_FEE_BPS,
            creator_fee_bps: DEFAULT_CREATOR_FEE_BPS,
            starting_equity_lamports: 10 * LAMPORTS_PER_SOL,
            cents_per_sol: 0,
            tips: TipSchedule::annex_c(),
            adversary: AdversaryConfig::default(),
            fees: FeeSchedule::mainnet(),
        }
    }
}

impl AttributionConfig {
    /// How this run's fee divides between the venue and the creator.
    ///
    /// Derived rather than stored, so the two numbers cannot drift: the total is
    /// always `fee_bps`, and `creator_fee_bps` only decides where inside it the
    /// line falls.
    pub const fn fee_split(&self) -> FeeSplit {
        // `min`, so that a creator share somebody set larger than the whole fee
        // takes all of it rather than underflowing the venue's.
        let creator_bps = if self.creator_fee_bps < self.fee_bps {
            self.creator_fee_bps
        } else {
            self.fee_bps
        };
        FeeSplit {
            total_bps: self.fee_bps,
            protocol_bps: self.fee_bps - creator_bps,
            creator_bps,
        }
    }

    /// The same run against a different adversary.
    ///
    /// The fee travels into the adversary's config as well, because both sides
    /// of a swap pay the venue and a model where the attacker paid a different
    /// fee from the victim would be pricing a venue that does not exist.
    pub const fn against(mut self, profile: AdversaryProfile) -> Self {
        self.adversary.profile = profile;
        self.adversary.fee_bps = self.fee_bps;
        self
    }
}

// ===========================================================================
// What was executed
// ===========================================================================

/// One side of one round trip, as it actually filled.
///
/// The reserves are the ones the *decision* was made against: the curve as it
/// stood before our order and before anybody who front-ran it. That is the
/// reference price the whole decomposition is taken from, and recording the
/// post-adversary curve here instead would quietly move the MEV penalty onto the
/// impact line, where it would look like a cost of trading size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionLeg {
    pub side: Side,
    pub at_ms: i64,
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    /// What we committed (buy) or what the curve paid out (sell), before fee.
    pub gross_lamports: u64,
    pub fee_lamports: u64,
    /// What entered the curve (buy) or reached us (sell), after fee.
    pub net_lamports: u64,
    /// The parcel: tokens received (buy) or handed over (sell).
    pub tokens: u64,
    /// What the bundle carrying this leg bid to land.
    pub tip_lamports: u64,
    /// What the adversary took, in the units the identity charges it in: on a
    /// buy, the displaced tokens valued at the pre-trade marginal price; on a
    /// sell, the gross the curve stopped paying.
    pub mev_penalty_lamports: u64,
    pub adversary: AdversaryProfile,
    pub intensity_micros: u64,
    pub slippage_bps: u16,
    /// Lamports the adversary put in front of a buy. Zero on a sell, and zero
    /// where nobody acted.
    pub attacker_lamports: u64,
    /// Tokens the adversary dumped in front of a sell. Zero on a buy, and zero
    /// where nobody acted.
    pub attacker_tokens: u64,
    /// The adversary's own profit where the model has one — never a lamport
    /// that came out of this fill; `mev_penalty_lamports` is that.
    pub attacker_profit_lamports: Option<i64>,
    /// Whether the ceiling on modelled damage cut the adversary back, including
    /// cutting it back to nothing. Kept on the leg because "nobody was there"
    /// and "this model has nothing to say about who was" are different facts.
    pub bounded: bool,
}

impl ExecutionLeg {
    /// The leg a [`MevOutcome`] describes, plus the tip its bundle bid.
    pub fn from_outcome(at_ms: i64, outcome: &MevOutcome, tip_lamports: u64) -> Self {
        ExecutionLeg {
            side: outcome.side,
            at_ms,
            virtual_token_reserves: 0,
            virtual_sol_reserves: 0,
            gross_lamports: match outcome.side {
                Side::Buy => outcome.notional_lamports,
                Side::Sell => outcome.filled_gross_lamports,
            },
            fee_lamports: outcome.fee_lamports,
            net_lamports: outcome.net_lamports,
            tokens: outcome.filled_tokens,
            tip_lamports,
            mev_penalty_lamports: outcome.penalty_lamports,
            adversary: outcome.profile,
            intensity_micros: outcome.intensity_micros,
            slippage_bps: outcome.slippage_bps,
            attacker_lamports: outcome.attacker_lamports,
            attacker_tokens: outcome.attacker_tokens,
            attacker_profit_lamports: outcome.attacker_profit_lamports,
            bounded: outcome.bounded,
        }
    }

    /// The same leg, against the curve the decision was made at.
    pub const fn against(mut self, curve: &CurveState) -> Self {
        self.virtual_token_reserves = curve.virtual_token_reserves;
        self.virtual_sol_reserves = curve.virtual_sol_reserves;
        self
    }

    /// Why this leg cannot be attributed, or nothing.
    fn malformed(&self, expected: Side) -> Option<String> {
        if self.side != expected {
            return Some(format!(
                "a {} leg cannot be the {} of a round trip",
                self.side.as_str(),
                expected.as_str()
            ));
        }
        if self.virtual_token_reserves == 0 || self.virtual_sol_reserves == 0 {
            return Some(
                "a leg with an empty reserve has no marginal price to attribute against"
                    .to_string(),
            );
        }
        if self.net_lamports.saturating_add(self.fee_lamports) != self.gross_lamports {
            return Some(format!(
                "net {} plus fee {} is not gross {}, so one of the three is wrong",
                self.net_lamports, self.fee_lamports, self.gross_lamports
            ));
        }
        if self.tokens == 0 {
            return Some("a leg that moved no tokens is not a leg".to_string());
        }
        None
    }
}

/// One position, opened and closed, with both fills.
///
/// The parcel has to match: the tokens the entry bought are the tokens the exit
/// sold. A partially closed position is two different questions — what the
/// closed part did, and what the open part is worth — and `backtest.rs` keeps
/// them apart with `ClosedTrade` and `StrandedPosition` for the reason this does
/// not fold them together either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeExecution {
    pub mint: String,
    pub entry: ExecutionLeg,
    pub exit: ExecutionLeg,
}

impl TradeExecution {
    /// Why this round trip cannot be attributed, or nothing.
    fn malformed(&self) -> Option<String> {
        if let Some(why) = self.entry.malformed(Side::Buy) {
            return Some(why);
        }
        if let Some(why) = self.exit.malformed(Side::Sell) {
            return Some(why);
        }
        if self.entry.tokens != self.exit.tokens {
            return Some(format!(
                "entry bought {} tokens and exit sold {}: a partial close is two questions, \
                 not one trade",
                self.entry.tokens, self.exit.tokens
            ));
        }
        None
    }
}

// ===========================================================================
// What one trade decomposes into
// ===========================================================================

/// One round trip, split into the lines the module doc's identity names.
///
/// Every field is lamports unless the name says otherwise, and the identity
/// [`TradeAttribution::balances`] checks is the invariant the whole struct
/// exists to carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeAttribution {
    pub mint: String,
    pub opened_at_ms: i64,
    pub closed_at_ms: i64,
    pub hold_ms: i64,
    /// The entry stake, gross. Every share below is taken against this.
    pub notional_lamports: u64,
    pub realized_pnl_lamports: i64,
    pub realized_pnl_usd_cents: i64,
    /// The price move on the notional, and nothing else.
    pub gross_alpha_lamports: i64,
    /// The curve's convexity on both legs, carried.
    pub price_impact_lamports: u64,
    /// The venue's cut on both legs, carried.
    pub protocol_fee_lamports: u64,
    /// What the block market charged to land both legs, at face value. The one
    /// cost line that is not carried — a tip is paid beside the stake, not out
    /// of it.
    pub tip_lamports: u64,
    /// What the synthetic adversary took on both legs, carried. Marginal to the
    /// impact line: our own order's impact is not charged here as well.
    pub mev_penalty_lamports: u64,
    /// The four cost lines, added up.
    pub total_cost_lamports: u64,
    /// Accumulated integer floors. Arithmetic, not economics — see the module
    /// doc, and [`TradeAttribution::residual_bound_lamports`] for what it is
    /// allowed to be.
    pub residual_lamports: i64,
    /// What actually left the wallet in fees, at face value, uncarried. The
    /// tip line above is already a face value, so it has no second column.
    pub fees_charged_lamports: u64,
    /// The venue's part of `fees_charged_lamports`, over both legs.
    ///
    /// Split off the charged number rather than off the carried one on purpose.
    /// The carried line is an answer to "how many lamports of the payout did
    /// this destroy", and carrying two parts separately would put a second floor
    /// into the identity for a column nobody adds up. This pair answers the
    /// other question — who took the money — and it is exact:
    /// `protocol + creator == fees_charged`, always, which
    /// [`TradeAttribution::fees_decompose`] says out loud.
    pub protocol_fee_charged_lamports: u64,
    /// The creator's part of the same number. Zero on a run configured with no
    /// creator share, which is every run recorded before the column existed.
    pub creator_fee_charged_lamports: u64,
    /// The exit marginal price over the entry marginal price, in millionths.
    /// One million is a flat trade.
    pub carry_ratio_micros: u64,
    /// `pnl / notional`, floored towards negative infinity.
    pub return_bps: i32,
    /// `ln(proceeds / notional)` in millionths, from
    /// [`crate::strategy::fixed::ln_fixed`]. `None` for a trade that ended at
    /// nothing: a total loss has a log return of negative infinity, which is not
    /// a number that belongs in a mean.
    pub log_return_micros: Option<i64>,
    /// The worse of the two legs' realised slippage.
    pub worst_slippage_bps: u16,
    pub entry_slippage_bps: u16,
    pub exit_slippage_bps: u16,
    pub adversary: AdversaryProfile,
}

impl TradeAttribution {
    /// What the residual is allowed to be, in lamports.
    ///
    /// Scales with the price ratio because every entry-side floor is multiplied
    /// by it on the way to the payout: a trade that went up tenfold carries its
    /// rounding up tenfold too.
    pub fn residual_bound_lamports(&self) -> u64 {
        let ratio = self.carry_ratio_micros.div_ceil(MICROS).max(1);
        RESIDUAL_FLOORS.saturating_mul(ratio.saturating_add(1))
    }

    /// Whether the identity closes exactly. Always true, by construction —
    /// `residual_lamports` is defined as what closes it — and asserted anyway,
    /// because the point of writing an invariant down is that a later edit which
    /// breaks it is caught by something other than a reader.
    pub fn balances(&self) -> bool {
        let attributed = i128::from(self.gross_alpha_lamports)
            - i128::from(self.price_impact_lamports)
            - i128::from(self.protocol_fee_lamports)
            - i128::from(self.tip_lamports)
            - i128::from(self.mev_penalty_lamports)
            + i128::from(self.residual_lamports);
        attributed == i128::from(self.realized_pnl_lamports)
    }

    /// Whether the rounding stayed where it was supposed to.
    pub fn residual_within_bound(&self) -> bool {
        self.residual_lamports.unsigned_abs() <= self.residual_bound_lamports()
    }

    /// Whether the fee split accounts for every lamport the venue took.
    ///
    /// True by construction — [`crate::replay::FeeSplit::decompose`] gives the
    /// remainder to the venue precisely so that it is — and asserted for the
    /// same reason [`TradeAttribution::balances`] is: an invariant nothing
    /// checks is a comment.
    pub const fn fees_decompose(&self) -> bool {
        match self
            .protocol_fee_charged_lamports
            .checked_add(self.creator_fee_charged_lamports)
        {
            Some(sum) => sum == self.fees_charged_lamports,
            None => false,
        }
    }

    /// The total order the run walks trades in.
    ///
    /// By close, then by open, then by mint. Every comparison falls through to
    /// the mint for the reason `strategy/mod.rs` gives about its own orderings:
    /// a tie broken by input order is a report that changes when the caller
    /// shuffles its inputs, and the equity curve below is walked in exactly this
    /// sequence.
    fn order_key(&self) -> (i64, i64, &str) {
        (self.closed_at_ms, self.opened_at_ms, self.mint.as_str())
    }
}

/// The natural log of `after / before`, in millionths.
///
/// Computed as `ln(after) − ln(before)` at `10^-18` through
/// [`crate::strategy::fixed::ln_fixed`] and floored to millionths at the end, so
/// the whole of the precision loss is one division in a known direction.
///
/// `None` when either side is at or below one lamport. `ln_fixed` reports zero
/// there — the right answer for one and a deliberate floor for zero — and a
/// difference of two floors is not a return.
pub fn log_return_micros(before: u64, after: u64) -> Option<i64> {
    if before <= 1 || after <= 1 {
        return None;
    }
    // 10^-18 to 10^-6 is twelve orders of magnitude.
    let scale: i128 = 1_000_000_000_000;
    let difference =
        i128::try_from(ln_fixed(after)).ok()? - i128::try_from(ln_fixed(before)).ok()?;
    Some(floor_div_i128(difference, scale).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64)
}

/// Splits one round trip into the identity's lines.
///
/// `Err` carries the reason the trade is not attributable, in a sentence meant
/// to be read in a report's refusal list rather than parsed.
pub fn attribute_trade(
    trade: &TradeExecution,
    config: &AttributionConfig,
) -> Result<TradeAttribution, String> {
    if let Some(why) = trade.malformed() {
        return Err(format!("{}: {why}", trade.mint));
    }

    let (entry, exit) = (&trade.entry, &trade.exit);
    let x0 = u128::from(entry.virtual_token_reserves);
    let y0 = u128::from(entry.virtual_sol_reserves);
    let x1 = u128::from(exit.virtual_token_reserves);
    let y1 = u128::from(exit.virtual_sol_reserves);

    // The price ratio, as a fraction rather than a number: p_exit / p_entry is
    // (y1/x1) / (y0/x0) = (x0·y1) / (y0·x1), and keeping it unevaluated is what
    // stops the carry below rounding twice.
    let ratio_num = x0.saturating_mul(y1);
    let ratio_den = y0.saturating_mul(x1);
    if ratio_den == 0 {
        return Err(format!(
            "{}: the entry curve has no price to carry from",
            trade.mint
        ));
    }
    let carry = |lamports: u128| -> u128 { mul_div_floor(lamports, ratio_num, ratio_den) };
    let carry_ratio_micros =
        mul_div_floor(u128::from(MICROS), ratio_num, ratio_den).min(u128::from(u64::MAX)) as u64;

    let notional = entry.gross_lamports;
    let tips_paid = entry.tip_lamports.saturating_add(exit.tip_lamports);
    let fees_charged = entry.fee_lamports.saturating_add(exit.fee_lamports);
    // §18's first row, split. Both legs pay a proportional fee on the SOL leg
    // and both are decomposed the same way, because the venue does not know
    // which side of a swap it is charging.
    let split = config.fee_split();
    let charged = split
        .decompose(entry.gross_lamports, entry.fee_lamports)
        .saturating_add(&split.decompose(exit.gross_lamports, exit.fee_lamports));

    // What actually happened: the exit's net, less the stake, less both tips.
    // Tips are cash out of the wallet on top of the stake, so they are not in
    // the gross of either leg and have to be subtracted here.
    let realized = i128::from(exit.net_lamports) - i128::from(notional) - i128::from(tips_paid);

    // ---- gross alpha: the stake, moved by the price, less the stake.
    let moved = carry(u128::from(notional)).min(u128::from(i64::MAX.unsigned_abs()));
    let gross_alpha = moved as i128 - i128::from(notional);

    // ---- price impact.
    //
    // Entry: the closed form of the convexity cost, in entry lamports. For a
    // net input N into a reserve y the average price paid is worse than the
    // marginal one by exactly N²/(y + N), which needs no reserve ratio and so
    // rounds once rather than three times.
    let net_in = u128::from(entry.net_lamports);
    let impact_in = net_in
        .saturating_mul(net_in)
        .checked_div(y0.saturating_add(net_in))
        .unwrap_or(0);
    // Exit: the marginal value of the parcel, less what the curve would have
    // paid for it with nobody in front of us. Recovering the solo gross by
    // adding the penalty back is exact — the penalty was defined as the
    // difference — which is what keeps the exit side of the identity clean.
    let parcel = u128::from(exit.tokens);
    let marginal_out = mul_div_floor(parcel, y1, x1);
    let solo_gross_out =
        u128::from(exit.gross_lamports).saturating_add(u128::from(exit.mev_penalty_lamports));
    let impact_out = marginal_out.saturating_sub(solo_gross_out);
    let price_impact = carry(impact_in).saturating_add(impact_out);

    // ---- the other three lines.
    let protocol_fee =
        carry(u128::from(entry.fee_lamports)).saturating_add(u128::from(exit.fee_lamports));
    // Not carried. See the module doc: a tip is paid beside the stake rather
    // than out of it, so it never bought tokens and never rode the move.
    let tip_cost = u128::from(entry.tip_lamports).saturating_add(u128::from(exit.tip_lamports));
    let mev_penalty = carry(u128::from(entry.mev_penalty_lamports))
        .saturating_add(u128::from(exit.mev_penalty_lamports));

    let clamp_u64 = |value: u128| -> u64 { value.min(u128::from(u64::MAX)) as u64 };
    let price_impact = clamp_u64(price_impact);
    let protocol_fee = clamp_u64(protocol_fee);
    let tip_cost = clamp_u64(tip_cost);
    let mev_penalty = clamp_u64(mev_penalty);

    let attributed = gross_alpha
        - i128::from(price_impact)
        - i128::from(protocol_fee)
        - i128::from(tip_cost)
        - i128::from(mev_penalty);
    let residual = realized - attributed;

    let return_bps = if notional == 0 {
        0
    } else {
        floor_div_i128(
            realized.saturating_mul(i128::from(BPS_DENOMINATOR)),
            i128::from(notional),
        )
        .clamp(i128::from(i32::MIN), i128::from(i32::MAX)) as i32
    };

    // The proceeds the log return is taken against are net of tips: a tip is a
    // lamport that did not come back, so a trade that broke even before tips did
    // not break even.
    let proceeds = i128::from(exit.net_lamports) - i128::from(tips_paid);
    let log_return = if proceeds <= 0 {
        None
    } else {
        log_return_micros(notional, proceeds.min(i128::from(u64::MAX)) as u64)
    };

    Ok(TradeAttribution {
        mint: trade.mint.clone(),
        opened_at_ms: entry.at_ms,
        closed_at_ms: exit.at_ms,
        hold_ms: exit.at_ms.saturating_sub(entry.at_ms),
        notional_lamports: notional,
        realized_pnl_lamports: realized.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
        realized_pnl_usd_cents: lamports_to_usd_cents(realized, config.cents_per_sol),
        gross_alpha_lamports: gross_alpha.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
        price_impact_lamports: price_impact,
        protocol_fee_lamports: protocol_fee,
        tip_lamports: tip_cost,
        mev_penalty_lamports: mev_penalty,
        total_cost_lamports: price_impact
            .saturating_add(protocol_fee)
            .saturating_add(tip_cost)
            .saturating_add(mev_penalty),
        residual_lamports: residual.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
        fees_charged_lamports: fees_charged,
        protocol_fee_charged_lamports: charged.protocol_lamports,
        creator_fee_charged_lamports: charged.creator_lamports,
        carry_ratio_micros,
        return_bps,
        log_return_micros: log_return,
        worst_slippage_bps: entry.slippage_bps.max(exit.slippage_bps),
        entry_slippage_bps: entry.slippage_bps,
        exit_slippage_bps: exit.slippage_bps,
        adversary: entry.adversary,
    })
}

// ===========================================================================
// Across a run
// ===========================================================================

/// Every line of the identity, summed over a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributionSummary {
    pub trades: u32,
    pub winners: u32,
    pub losers: u32,
    pub scratches: u32,
    pub notional_lamports: u64,
    pub realized_pnl_lamports: i64,
    pub realized_pnl_usd_cents: i64,
    pub gross_alpha_lamports: i64,
    pub price_impact_lamports: u64,
    pub protocol_fee_lamports: u64,
    pub tip_lamports: u64,
    pub mev_penalty_lamports: u64,
    pub total_cost_lamports: u64,
    pub residual_lamports: i64,
    /// The single worst rounding residue on any one trade, by magnitude.
    pub worst_residual_lamports: i64,
    pub fees_charged_lamports: u64,
    /// The venue's part of `fees_charged_lamports` over the run.
    pub protocol_fee_charged_lamports: u64,
    /// The creators' part of it.
    pub creator_fee_charged_lamports: u64,
    /// The four cost lines over the traded notional, in basis points, rounded
    /// up. What it cost to trade, before asking whether the trade was right.
    pub cost_bps_of_notional: u16,
    /// The four cost lines over gross alpha, in basis points. `None` when alpha
    /// was not positive — a share of an edge that did not exist is not a large
    /// share, it is a meaningless one.
    pub cost_share_of_alpha_bps: Option<u32>,
}

impl AttributionSummary {
    /// An empty book.
    pub const fn empty() -> Self {
        AttributionSummary {
            trades: 0,
            winners: 0,
            losers: 0,
            scratches: 0,
            notional_lamports: 0,
            realized_pnl_lamports: 0,
            realized_pnl_usd_cents: 0,
            gross_alpha_lamports: 0,
            price_impact_lamports: 0,
            protocol_fee_lamports: 0,
            tip_lamports: 0,
            mev_penalty_lamports: 0,
            total_cost_lamports: 0,
            residual_lamports: 0,
            worst_residual_lamports: 0,
            fees_charged_lamports: 0,
            protocol_fee_charged_lamports: 0,
            creator_fee_charged_lamports: 0,
            cost_bps_of_notional: 0,
            cost_share_of_alpha_bps: None,
        }
    }

    /// Whether the identity closes on the totals as well as on each trade.
    ///
    /// It is a separate question from [`TradeAttribution::balances`] and worth
    /// asking separately: every line here is a saturating sum, and a total that
    /// did not close would mean one of them saturated where another did not.
    pub fn balances(&self) -> bool {
        let attributed = i128::from(self.gross_alpha_lamports)
            - i128::from(self.price_impact_lamports)
            - i128::from(self.protocol_fee_lamports)
            - i128::from(self.tip_lamports)
            - i128::from(self.mev_penalty_lamports)
            + i128::from(self.residual_lamports);
        attributed == i128::from(self.realized_pnl_lamports)
    }

    /// Whether the fee split still accounts for every charged lamport once the
    /// run is summed. [`TradeAttribution::fees_decompose`], over the totals.
    pub const fn fees_decompose(&self) -> bool {
        match self
            .protocol_fee_charged_lamports
            .checked_add(self.creator_fee_charged_lamports)
        {
            Some(sum) => sum == self.fees_charged_lamports,
            None => false,
        }
    }
}

/// The equity curve, and how it felt.
///
/// Realised trades only, walked in [`TradeAttribution::order_key`] order.
/// Marking open positions into the curve would make every number here a function
/// of the model's opinion of something nobody sold, which is the reason
/// `backtest::drawdown` refuses to do it either.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReturnSummary {
    pub starting_equity_lamports: u64,
    pub ending_equity_lamports: i64,
    pub high_water_lamports: i64,
    /// Ending over starting, in basis points, floored towards negative infinity.
    pub cumulative_return_bps: i32,
    /// The sum of the per-trade log returns, in millionths. Additive where the
    /// basis-point column is not: two trades of +5 000 bps are +10 000 bps here
    /// only in the sense that logs add, which is the sense that composes.
    pub cumulative_log_return_micros: i64,
    /// Trades whose log return was not defined, and so are not in the sum above.
    pub log_returns_undefined: u32,
    pub max_drawdown_lamports: u64,
    pub max_drawdown_bps: u16,
    pub max_drawdown_at_ms: i64,
    pub longest_underwater_ms: i64,
    pub longest_losing_streak: u32,
    /// Mean per-trade return, in millionths of a basis point.
    pub mean_return_bps_micros: i64,
    /// Sample standard deviation of per-trade returns, same unit.
    pub stddev_return_bps_micros: u64,
    /// The same, counting only the trades that lost. Same unit.
    pub downside_deviation_bps_micros: u64,
    /// Mean over standard deviation, in millionths. Per trade, not annualised,
    /// at a risk-free rate of zero — `backtest::sharpe_micros` documents why
    /// none of those three is a number this corpus can supply.
    pub sharpe_micros: Option<i64>,
    /// Mean over downside deviation, in millionths, on the same three
    /// conventions. `None` when nothing lost: a strategy with no downside has an
    /// undefined Sortino, not an infinite one.
    pub sortino_micros: Option<i64>,
}

/// The Sortino ratio of a set of per-trade returns, in millionths.
///
/// Sortino is Sharpe with the denominator replaced by downside deviation: the
/// same dispersion measure, computed with every return above the target set to
/// zero, so a strategy is not penalised for the size of its winners.
///
/// Three conventions, each of which somebody could disagree with, so each is
/// here rather than buried:
///
/// **The target is zero,** the same risk-free rate `sharpe_micros` uses and for
/// the same reason: the holding period is measured in seconds.
///
/// **The denominator is `n - 1`,** matching `sharpe_micros` rather than the
/// textbook `n`. The two conventions differ by a factor that is a function of
/// the trade count alone, and having the two ratios differ from each other by
/// nothing but their numerators is worth more than matching a textbook.
///
/// **`None` when nothing lost.** A ratio whose denominator is zero is not a
/// perfect strategy, it is one this sample cannot rank — the same refusal
/// `PerformanceSummary::profit_factor_micros` makes about an infinite profit
/// factor.
pub fn sortino_micros(returns_bps: &[i32]) -> Option<i64> {
    let n = returns_bps.len();
    if n < 2 {
        return None;
    }
    let sum: i128 = returns_bps.iter().map(|&r| i128::from(r)).sum();
    let count = n as i128;
    let mean_scaled = sum.saturating_mul(i128::from(MICROS)) / count;

    let deviation = downside_deviation_bps_micros(returns_bps)?;
    if deviation == 0 {
        return None;
    }
    let sortino = mean_scaled.saturating_mul(i128::from(MICROS)) / i128::from(deviation);
    Some(sortino.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64)
}

/// The downside deviation of a set of per-trade returns, in millionths of a
/// basis point, against a target of zero. `None` for fewer than two returns.
pub fn downside_deviation_bps_micros(returns_bps: &[i32]) -> Option<u64> {
    let n = returns_bps.len();
    if n < 2 {
        return None;
    }
    let mut accumulator: u128 = 0;
    for &r in returns_bps {
        if r >= 0 {
            continue;
        }
        let scaled = i128::from(r)
            .saturating_mul(i128::from(MICROS))
            .unsigned_abs();
        accumulator = accumulator.saturating_add(scaled.saturating_mul(scaled));
    }
    Some(isqrt(accumulator / (n as u128 - 1)).min(u128::from(u64::MAX)) as u64)
}

/// One bar of the slippage histogram.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlippageBucket {
    /// The inclusive upper edge, in basis points.
    pub upper_bps: u16,
    pub count: u32,
}

/// What it cost to get on and off, across every leg of the run.
///
/// One sample per leg rather than per trade, because an entry and an exit are
/// two separate fills and averaging them into one observation would hide a
/// strategy that gets on cheaply and cannot get off.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlippageDistribution {
    pub samples: u32,
    pub min_bps: u16,
    pub p50_bps: u16,
    pub p90_bps: u16,
    pub p99_bps: u16,
    pub max_bps: u16,
    /// The arithmetic mean, in millionths of a basis point.
    pub mean_bps_micros: u64,
    pub buckets: Vec<SlippageBucket>,
}

impl SlippageDistribution {
    /// An empty distribution, with the buckets still declared.
    ///
    /// The bars are present at zero rather than absent, so a report with no
    /// fills has the same shape as one with a thousand and a diff between them
    /// is a diff of counts rather than of structure.
    pub fn empty() -> Self {
        SlippageDistribution {
            samples: 0,
            min_bps: 0,
            p50_bps: 0,
            p90_bps: 0,
            p99_bps: 0,
            max_bps: 0,
            mean_bps_micros: 0,
            buckets: SLIPPAGE_BUCKET_EDGES_BPS
                .iter()
                .map(|&upper_bps| SlippageBucket {
                    upper_bps,
                    count: 0,
                })
                .collect(),
        }
    }

    /// The distribution of a set of per-leg slippage figures.
    ///
    /// Quantiles by nearest rank on the sorted sample — the index `ceil(p·n)`,
    /// one-based — with no interpolation. Interpolating between two integer
    /// basis-point observations would invent a value neither leg paid, and the
    /// nearest rank is the one quantile definition that returns a number that
    /// actually happened.
    pub fn of(mut samples_bps: Vec<u16>) -> Self {
        let mut distribution = SlippageDistribution::empty();
        if samples_bps.is_empty() {
            return distribution;
        }
        samples_bps.sort_unstable();

        let n = samples_bps.len();
        distribution.samples = u32::try_from(n).unwrap_or(u32::MAX);
        distribution.min_bps = samples_bps[0];
        distribution.max_bps = samples_bps[n - 1];
        distribution.p50_bps = nearest_rank(&samples_bps, 50);
        distribution.p90_bps = nearest_rank(&samples_bps, 90);
        distribution.p99_bps = nearest_rank(&samples_bps, 99);

        let total: u128 = samples_bps.iter().map(|&bps| u128::from(bps)).sum();
        distribution.mean_bps_micros =
            mul_div_floor(total, u128::from(MICROS), n as u128).min(u128::from(u64::MAX)) as u64;

        for &sample in &samples_bps {
            for bucket in distribution.buckets.iter_mut() {
                if sample <= bucket.upper_bps {
                    bucket.count += 1;
                    break;
                }
            }
        }
        distribution
    }
}

/// The `p`th percentile of an ascending sample, by nearest rank.
fn nearest_rank(sorted: &[u16], p: u32) -> u16 {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len() as u128;
    let rank = mul_div_ceil(n, u128::from(p), 100).max(1).min(n) as usize;
    sorted[rank - 1]
}

/// The whole thing.
///
/// **No timestamp, deliberately**, for the reason `ForensicReport` gives: two
/// runs over one set of executions have to produce identical bytes, and a
/// `generated_at` field would break that on every run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributionReport {
    pub schema: String,
    pub config: AttributionConfig,
    pub summary: AttributionSummary,
    pub returns: ReturnSummary,
    pub slippage: SlippageDistribution,
    pub mev: MevSummary,
    /// Every charge across the run, split into lines. Computed from the same
    /// executions as `summary` by different code, and
    /// [`FeeDecomposition::reconciles`] is where the two are made to agree.
    pub fees: FeeDecomposition,
    pub trades: Vec<TradeAttribution>,
    /// Executions that could not be attributed, and why. Sorted, so two runs
    /// that refuse the same trades refuse them in the same order.
    pub refusals: Vec<String>,
}

impl AttributionReport {
    /// The report as indented JSON, ending in a newline.
    ///
    /// Every collection in the tree is a `Vec` that was explicitly sorted and
    /// serde writes fields in declaration order, so the bytes are a function of
    /// the report and nothing else.
    pub fn to_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self)
            .unwrap_or_else(|err| format!("{{\"error\":\"{err}\"}}"));
        text.push('\n');
        text
    }

    /// Whether every trade and the totals all close.
    pub fn balances(&self) -> bool {
        self.summary.balances()
            && self.trades.iter().all(TradeAttribution::balances)
            && self.fees.balances()
            && self.fees.reconciles(&self.summary)
    }
}

/// Splits a whole run into the identity's lines.
///
/// Trades are attributed independently and then sorted into
/// [`TradeAttribution::order_key`] order before the equity curve is walked, so
/// the drawdown is a function of the executions and not of the order the caller
/// happened to assemble them in.
pub fn attribute_run(trades: &[TradeExecution], config: &AttributionConfig) -> AttributionReport {
    let mut attributed = Vec::with_capacity(trades.len());
    let mut refusals = Vec::new();
    let mut legs = Vec::with_capacity(trades.len() * 2);
    let mut fee_rows = Vec::with_capacity(trades.len());

    for trade in trades {
        match attribute_trade(trade, config) {
            Ok(row) => {
                legs.push(leg_outcome(&trade.entry));
                legs.push(leg_outcome(&trade.exit));
                // Only the trades that were attributed. A refused execution is
                // not in the identity's totals, so charging its fees here would
                // make the two views of one run disagree by exactly the
                // refusals — and `reconciles` would be the thing that broke.
                fee_rows.push(decompose_trade(trade, &config.fees));
                attributed.push(row);
            }
            Err(why) => refusals.push(why),
        }
    }
    attributed.sort_by(|left, right| left.order_key().cmp(&right.order_key()));
    refusals.sort();

    let summary = summarise(&attributed, config);
    let returns = walk_equity(&attributed, config.starting_equity_lamports);
    let slippage = SlippageDistribution::of(
        attributed
            .iter()
            .flat_map(|row| [row.entry_slippage_bps, row.exit_slippage_bps])
            .collect(),
    );
    let mev = MevSummary::of(
        config.adversary.profile,
        config.adversary.max_penalty_bps,
        &legs,
    );

    AttributionReport {
        schema: ATTRIBUTION_SCHEMA.to_string(),
        config: *config,
        summary,
        returns,
        slippage,
        mev,
        fees: FeeDecomposition::of(fee_rows, config.fees),
        trades: attributed,
        refusals,
    }
}

/// The MEV view of one recorded leg.
///
/// A leg carries what the adversary took but not the fill it was taken from, so
/// this rebuilds the parts [`MevSummary`] folds over. The notional is the leg's
/// own gross either way, which is the denominator `MevOutcome` uses on both
/// sides.
fn leg_outcome(leg: &ExecutionLeg) -> MevOutcome {
    let notional = leg.gross_lamports.saturating_add(match leg.side {
        Side::Buy => 0,
        // A sell's notional is what the curve would have paid alone, and the
        // penalty is exactly the difference between that and what it did.
        Side::Sell => leg.mev_penalty_lamports,
    });
    MevOutcome {
        profile: leg.adversary,
        side: leg.side,
        intensity_micros: leg.intensity_micros,
        attacker_lamports: leg.attacker_lamports,
        attacker_tokens: leg.attacker_tokens,
        notional_lamports: notional,
        solo_tokens: leg.tokens,
        filled_tokens: leg.tokens,
        solo_gross_lamports: notional,
        filled_gross_lamports: leg.gross_lamports,
        fee_lamports: leg.fee_lamports,
        net_lamports: leg.net_lamports,
        penalty_lamports: leg.mev_penalty_lamports,
        penalty_bps: if notional == 0 {
            0
        } else {
            mul_div_ceil(
                u128::from(leg.mev_penalty_lamports),
                u128::from(BPS_DENOMINATOR),
                u128::from(notional),
            )
            .min(u128::from(BPS_DENOMINATOR)) as u16
        },
        slippage_bps: leg.slippage_bps,
        bounded: leg.bounded,
        attacker_profit_lamports: leg.attacker_profit_lamports,
        synthetic: true,
    }
}

/// Folds every attributed trade into one row.
fn summarise(trades: &[TradeAttribution], config: &AttributionConfig) -> AttributionSummary {
    let mut summary = AttributionSummary::empty();
    let mut notional: u128 = 0;
    let mut realized: i128 = 0;
    let mut alpha: i128 = 0;
    let mut impact: u128 = 0;
    let mut fee: u128 = 0;
    let mut tip: u128 = 0;
    let mut mev: u128 = 0;
    let mut residual: i128 = 0;

    for row in trades {
        summary.trades += 1;
        match row.realized_pnl_lamports {
            pnl if pnl > 0 => summary.winners += 1,
            pnl if pnl < 0 => summary.losers += 1,
            _ => summary.scratches += 1,
        }
        notional = notional.saturating_add(u128::from(row.notional_lamports));
        realized = realized.saturating_add(i128::from(row.realized_pnl_lamports));
        alpha = alpha.saturating_add(i128::from(row.gross_alpha_lamports));
        impact = impact.saturating_add(u128::from(row.price_impact_lamports));
        fee = fee.saturating_add(u128::from(row.protocol_fee_lamports));
        tip = tip.saturating_add(u128::from(row.tip_lamports));
        mev = mev.saturating_add(u128::from(row.mev_penalty_lamports));
        residual = residual.saturating_add(i128::from(row.residual_lamports));
        summary.fees_charged_lamports = summary
            .fees_charged_lamports
            .saturating_add(row.fees_charged_lamports);
        summary.protocol_fee_charged_lamports = summary
            .protocol_fee_charged_lamports
            .saturating_add(row.protocol_fee_charged_lamports);
        summary.creator_fee_charged_lamports = summary
            .creator_fee_charged_lamports
            .saturating_add(row.creator_fee_charged_lamports);
        if row.residual_lamports.unsigned_abs() > summary.worst_residual_lamports.unsigned_abs() {
            summary.worst_residual_lamports = row.residual_lamports;
        }
    }

    let clamp_u64 = |value: u128| -> u64 { value.min(u128::from(u64::MAX)) as u64 };
    let clamp_i64 =
        |value: i128| -> i64 { value.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64 };

    summary.notional_lamports = clamp_u64(notional);
    summary.realized_pnl_lamports = clamp_i64(realized);
    summary.realized_pnl_usd_cents = lamports_to_usd_cents(realized, config.cents_per_sol);
    summary.gross_alpha_lamports = clamp_i64(alpha);
    summary.price_impact_lamports = clamp_u64(impact);
    summary.protocol_fee_lamports = clamp_u64(fee);
    summary.tip_lamports = clamp_u64(tip);
    summary.mev_penalty_lamports = clamp_u64(mev);
    summary.residual_lamports = clamp_i64(residual);

    let costs = impact
        .saturating_add(fee)
        .saturating_add(tip)
        .saturating_add(mev);
    summary.total_cost_lamports = clamp_u64(costs);
    summary.cost_bps_of_notional = if notional == 0 {
        0
    } else {
        mul_div_ceil(costs, u128::from(BPS_DENOMINATOR), notional).min(u128::from(BPS_DENOMINATOR))
            as u16
    };
    summary.cost_share_of_alpha_bps = if alpha > 0 {
        Some(
            mul_div_ceil(costs, u128::from(BPS_DENOMINATOR), alpha.unsigned_abs())
                .min(u128::from(u32::MAX)) as u32,
        )
    } else {
        None
    };
    summary
}

/// Walks the realised equity curve and reports what it did.
fn walk_equity(trades: &[TradeAttribution], starting_equity_lamports: u64) -> ReturnSummary {
    let start = i128::from(starting_equity_lamports);
    let mut equity = start;
    let mut high_water = equity;
    // The opening equity has no timestamp — nothing here says when the account
    // was funded — so the underwater clock starts at the first close. That
    // understates a run whose first trade lost, by exactly the stretch nothing
    // can date. `backtest::drawdown` makes the same concession.
    let mut high_water_at_ms = trades.first().map(|row| row.closed_at_ms).unwrap_or(0);
    let mut max_drawdown: u128 = 0;
    let mut max_drawdown_bps: u16 = 0;
    let mut max_drawdown_at_ms: i64 = 0;
    let mut longest_underwater_ms: i64 = 0;
    let mut underwater = false;
    let mut streak: u32 = 0;
    let mut longest_streak: u32 = 0;
    let mut returns_bps: Vec<i32> = Vec::with_capacity(trades.len());
    let mut log_return: i128 = 0;
    let mut log_returns_undefined: u32 = 0;

    for row in trades {
        equity = equity.saturating_add(i128::from(row.realized_pnl_lamports));
        returns_bps.push(row.return_bps);
        match row.log_return_micros {
            Some(micros) => log_return = log_return.saturating_add(i128::from(micros)),
            None => log_returns_undefined += 1,
        }
        if row.realized_pnl_lamports < 0 {
            streak += 1;
            longest_streak = longest_streak.max(streak);
        } else {
            streak = 0;
        }

        if equity >= high_water {
            if underwater {
                longest_underwater_ms =
                    longest_underwater_ms.max(row.closed_at_ms.saturating_sub(high_water_at_ms));
                underwater = false;
            }
            high_water = equity;
            high_water_at_ms = row.closed_at_ms;
            continue;
        }
        underwater = true;

        let fall = (high_water - equity).unsigned_abs();
        // Against the high-water mark, which is the denominator the live breaker
        // uses. A peak at or below zero has no meaningful percentage fall, so the
        // basis-point column saturates and the lamport column carries the answer.
        let bps = if high_water > 0 {
            mul_div_ceil(fall, u128::from(BPS_DENOMINATOR), high_water.unsigned_abs())
                .min(u128::from(BPS_DENOMINATOR)) as u16
        } else {
            BPS_DENOMINATOR as u16
        };
        if fall > max_drawdown {
            max_drawdown = fall;
            max_drawdown_at_ms = row.closed_at_ms;
        }
        max_drawdown_bps = max_drawdown_bps.max(bps);
    }

    // A run that ends underwater never recovers, so the last stretch counts.
    if underwater {
        if let Some(last) = trades.last() {
            longest_underwater_ms =
                longest_underwater_ms.max(last.closed_at_ms.saturating_sub(high_water_at_ms));
        }
    }

    let cumulative_return_bps = if start <= 0 {
        0
    } else {
        floor_div_i128(
            (equity - start).saturating_mul(i128::from(BPS_DENOMINATOR)),
            start,
        )
        .clamp(i128::from(i32::MIN), i128::from(i32::MAX)) as i32
    };
    let (mean, stddev) = return_moments(&returns_bps);

    ReturnSummary {
        starting_equity_lamports,
        ending_equity_lamports: equity.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
        high_water_lamports: high_water.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
        cumulative_return_bps,
        cumulative_log_return_micros: log_return.clamp(i128::from(i64::MIN), i128::from(i64::MAX))
            as i64,
        log_returns_undefined,
        max_drawdown_lamports: max_drawdown.min(u128::from(u64::MAX)) as u64,
        max_drawdown_bps,
        max_drawdown_at_ms,
        longest_underwater_ms,
        longest_losing_streak: longest_streak,
        mean_return_bps_micros: mean,
        stddev_return_bps_micros: stddev,
        downside_deviation_bps_micros: downside_deviation_bps_micros(&returns_bps).unwrap_or(0),
        sharpe_micros: crate::backtest::sharpe_micros(&returns_bps),
        sortino_micros: sortino_micros(&returns_bps),
    }
}

/// Mean and sample standard deviation of per-trade returns, in millionths of a
/// basis point.
///
/// The same arithmetic `backtest::return_moments` does, which is private to that
/// module. Duplicated rather than made public there, because a second caller is
/// a reason to widen an interface only when the two callers want the same thing
/// to change together, and these two want the opposite: this one has to keep
/// agreeing with `sharpe_micros` forever, and that is a property worth a test
/// rather than a shared function.
fn return_moments(returns_bps: &[i32]) -> (i64, u64) {
    let n = returns_bps.len();
    if n == 0 {
        return (0, 0);
    }
    let sum: i128 = returns_bps.iter().map(|&r| i128::from(r)).sum();
    let mean_scaled = sum.saturating_mul(i128::from(MICROS)) / n as i128;
    let clamp =
        |value: i128| -> i64 { value.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64 };
    if n < 2 {
        return (clamp(mean_scaled), 0);
    }
    let mut accumulator: u128 = 0;
    for &r in returns_bps {
        let deviation = i128::from(r).saturating_mul(i128::from(MICROS)) - mean_scaled;
        accumulator = accumulator.saturating_add(
            deviation
                .unsigned_abs()
                .saturating_mul(deviation.unsigned_abs()),
        );
    }
    let stddev = isqrt(accumulator / (n as u128 - 1));
    (clamp(mean_scaled), stddev.min(u128::from(u64::MAX)) as u64)
}

// ===========================================================================
// Driving it from a curve
// ===========================================================================

/// One round trip to simulate: where the curve was when we got on, where it was
/// when we got off, and how much we put in.
///
/// The two curve positions are given as real SOL in the pool, which is what a
/// fixture records and what `CurveState::at_real_sol` reconstructs the rest of
/// the reserves from. The exit position is where the curve *ended up* — it
/// already includes everybody's flow, ours among it — so this does not apply our
/// own entry to it and then apply the flow again.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoundTripPlan {
    pub mint: String,
    pub opened_at_ms: i64,
    pub closed_at_ms: i64,
    pub entry_real_sol_lamports: u64,
    pub exit_real_sol_lamports: u64,
    /// What we commit at entry, gross.
    pub gross_lamports: u64,
    /// Price samples leading up to the entry, in the unit
    /// `mev_sim::curve_price_micros` produces. Empty is a volatility of zero,
    /// which makes the adversary *less* aggressive — see `MarketContext::at`.
    pub entry_ticks: Vec<u64>,
    /// The same, leading up to the exit.
    pub exit_ticks: Vec<u64>,
}

/// Runs one plan through the curve and the adversary, and returns the two legs.
///
/// The tips are priced by [`TipSchedule`] with the adversary's own intensity as
/// the congestion input, so a moment that is contested enough to be attacked is
/// the same moment that is expensive to land in. The entry bids with no expected
/// value — nothing at entry knows what the trade will make, and Annex C.2 is
/// explicit that a share of a number nobody computed is not a smaller share — and
/// the exit bids against what the round trip actually realised before tips.
pub fn simulate_round_trip(
    plan: &RoundTripPlan,
    config: &AttributionConfig,
) -> Result<TradeExecution, String> {
    let refuse = |stage: &str, err: QuoteError| format!("{}: {stage} — {err}", plan.mint);

    let entry_curve = CurveState::at_real_sol(plan.entry_real_sol_lamports);
    let exit_curve = CurveState::at_real_sol(plan.exit_real_sol_lamports);
    let entry_context = MarketContext::with_ticks(&entry_curve, &plan.entry_ticks);
    let exit_context = MarketContext::with_ticks(&exit_curve, &plan.exit_ticks);

    let bought = buy_through(
        &entry_curve,
        plan.gross_lamports,
        &config.adversary,
        entry_context,
    )
    .map_err(|err| refuse("entry", err))?;
    let sold = sell_through(
        &exit_curve,
        bought.filled_tokens,
        &config.adversary,
        exit_context,
    )
    .map_err(|err| refuse("exit", err))?;

    let entry_tip = config.tips.bid_lamports(None, 0, bought.intensity_micros);
    let before_tips = i64::try_from(sold.net_lamports).unwrap_or(i64::MAX)
        - i64::try_from(plan.gross_lamports).unwrap_or(i64::MAX);
    let exit_tip = config
        .tips
        .bid_lamports(Some(before_tips), 0, sold.intensity_micros);

    Ok(TradeExecution {
        mint: plan.mint.clone(),
        entry: ExecutionLeg::from_outcome(plan.opened_at_ms, &bought, entry_tip)
            .against(&entry_curve),
        exit: ExecutionLeg::from_outcome(plan.closed_at_ms, &sold, exit_tip).against(&exit_curve),
    })
}

/// Runs a whole corpus of plans and attributes what came back.
///
/// Plans that the curve refuses are collected as refusals rather than dropped:
/// a report that quietly omitted the trades whose exits were not executable
/// would be reporting the strategy's survivors as the strategy.
pub fn attribute_plans(plans: &[RoundTripPlan], config: &AttributionConfig) -> AttributionReport {
    let mut executed = Vec::with_capacity(plans.len());
    let mut refusals = Vec::new();
    for plan in plans {
        match simulate_round_trip(plan, config) {
            Ok(trade) => executed.push(trade),
            Err(why) => refusals.push(why),
        }
    }
    let mut report = attribute_run(&executed, config);
    report.refusals.extend(refusals);
    report.refusals.sort();
    report
}

// ===========================================================================
// What a fill actually cost, line by line
// ===========================================================================

/// The network's fee for one signature, in lamports.
///
/// The 5 000-lamport base fee. A protocol parameter that has moved before and
/// can again, which is why it is a field on [`FeeSchedule`] and this is only
/// its default.
pub const SIGNATURE_FEE_LAMPORTS: u64 = 5_000;

/// Signatures on one leg's transaction.
///
/// One: ours. A bundle carrying several of our legs would pay per leg anyway,
/// so the per-leg count is the one that composes.
pub const DEFAULT_SIGNATURES_PER_LEG: u32 = 1;

/// The compute budget one swap is given.
pub const DEFAULT_COMPUTE_UNITS_PER_LEG: u32 = 120_000;

/// What a compute unit is bid at, in micro-lamports.
///
/// The priority fee is `units × price`, and both halves are policy. This is a
/// number somebody picked for a backtest, not one measured off a block.
pub const DEFAULT_COMPUTE_UNIT_PRICE_MICRO_LAMPORTS: u64 = 20_000;

/// The rent-exempt minimum for one SPL token account, in lamports.
pub const ATA_RENT_LAMPORTS: u64 = 2_039_280;

/// The creator's share of the venue's swap fee, in basis points of that fee.
///
/// A twentieth. pump.fun has charged a protocol fee and a creator fee
/// separately and [`crate::replay::DEFAULT_FEE_BPS`] is documented as their
/// sum, so five basis points of a hundred is five percent of the fee. Splitting
/// the fee rather than adding a second one is what keeps this decomposition
/// reconcilable against the fills: the two halves add back to the lamports the
/// curve actually took.
pub const DEFAULT_CREATOR_SHARE_BPS: u16 = 500;

/// What every leg pays besides the curve.
///
/// The swap fee is not in here, because it is not a policy — it is on the fill,
/// and [`ExecutionLeg::fee_lamports`] carries what the venue actually took.
/// What this schedule does to that number is *split* it. Everything else here
/// is a cost the fill does not record and a backtest has to be told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeSchedule {
    /// The creator's share of the swap fee, in basis points of the fee.
    pub creator_share_bps: u16,
    pub signature_lamports: u64,
    pub signatures_per_leg: u32,
    pub compute_units_per_leg: u32,
    pub compute_unit_price_micro_lamports: u64,
    /// Rent posted to open the token account, once per round trip.
    pub ata_rent_lamports: u64,
    /// Whether the exit closes the account and takes the rent back.
    ///
    /// When it does, rent nets to zero over a round trip and the two legs carry
    /// it as `+r` and `−r` rather than as nothing: a cost that was posted and
    /// reclaimed is not a cost that never happened, and a strategy holding a
    /// thousand open positions has a thousand rents outstanding.
    pub reclaims_ata_rent: bool,
}

impl Default for FeeSchedule {
    fn default() -> Self {
        FeeSchedule::mainnet()
    }
}

impl FeeSchedule {
    /// The published parameters, with a priority bid somebody chose.
    pub const fn mainnet() -> Self {
        FeeSchedule {
            creator_share_bps: DEFAULT_CREATOR_SHARE_BPS,
            signature_lamports: SIGNATURE_FEE_LAMPORTS,
            signatures_per_leg: DEFAULT_SIGNATURES_PER_LEG,
            compute_units_per_leg: DEFAULT_COMPUTE_UNITS_PER_LEG,
            compute_unit_price_micro_lamports: DEFAULT_COMPUTE_UNIT_PRICE_MICRO_LAMPORTS,
            ata_rent_lamports: ATA_RENT_LAMPORTS,
            reclaims_ata_rent: true,
        }
    }

    /// A schedule that charges nothing but the venue's own cut.
    ///
    /// The control the other lines are read against: a run under this one and a
    /// run under [`FeeSchedule::mainnet`] differ by exactly the network's
    /// charges, which is the number somebody wants when they ask what the chain
    /// costs as opposed to what the venue costs.
    pub const fn free() -> Self {
        FeeSchedule {
            creator_share_bps: 0,
            signature_lamports: 0,
            signatures_per_leg: 0,
            compute_units_per_leg: 0,
            compute_unit_price_micro_lamports: 0,
            ata_rent_lamports: 0,
            reclaims_ata_rent: false,
        }
    }

    /// The network's base fee on one leg.
    pub fn signature_cost_lamports(&self) -> u64 {
        self.signature_lamports
            .saturating_mul(u64::from(self.signatures_per_leg))
    }

    /// The priority fee on one leg, in lamports.
    ///
    /// `units × price / 10^6`, **rounded up**, which is what the runtime does
    /// and also the direction every other cost in this module rounds: a
    /// simulator that under-reports its own costs flatters the backtest built
    /// on it.
    pub fn priority_cost_lamports(&self) -> u64 {
        mul_div_ceil(
            u128::from(self.compute_units_per_leg),
            u128::from(self.compute_unit_price_micro_lamports),
            u128::from(MICROS),
        )
        .min(u128::from(u64::MAX)) as u64
    }

    /// The creator's share of a swap fee, in lamports.
    ///
    /// Floored, and the protocol takes the remainder — see
    /// [`LegFees::venue_splits`] for why the split is done that way round.
    pub fn creator_cut_lamports(&self, venue_fee_lamports: u64) -> u64 {
        mul_div_floor(
            u128::from(venue_fee_lamports),
            u128::from(self.creator_share_bps),
            u128::from(BPS_DENOMINATOR),
        )
        .min(u128::from(venue_fee_lamports)) as u64
    }
}

/// Every charge on one leg, at face value.
///
/// Face value throughout, and deliberately not carried: this struct answers
/// "what left the wallet", which is a different question from the one
/// [`TradeAttribution`] answers about what each cost destroyed of the payout.
/// The two live side by side in the report for that reason — see
/// [`TradeAttribution::fees_charged_lamports`], which is this struct's venue
/// line summed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegFees {
    pub side: Side,
    /// The venue's cut on this leg, exactly as the fill charged it.
    pub venue_lamports: u64,
    /// The venue's cut, split. `protocol + creator == venue`, exactly.
    pub protocol_lamports: u64,
    pub creator_lamports: u64,
    /// Signatures × the per-signature price.
    pub signature_lamports: u64,
    /// Compute units × the price bid for them.
    pub priority_lamports: u64,
    /// What the bundle bid to land this leg.
    pub jito_tip_lamports: u64,
    /// Rent posted on the entry, reclaimed on the exit. Negative when it comes
    /// back.
    pub rent_lamports: i64,
    /// Everything above.
    pub total_lamports: i64,
}

impl LegFees {
    /// Whether the split of the venue's cut is exact.
    ///
    /// It is, by construction — the protocol gets the remainder rather than its
    /// own rounded share — and asserted anyway, because a split that lost a
    /// lamport would be a decomposition that stopped reconciling against the
    /// fills it came from, which is the one thing it is for.
    pub const fn venue_splits(&self) -> bool {
        self.protocol_lamports + self.creator_lamports == self.venue_lamports
    }
}

/// Splits one recorded leg into everything it paid.
///
/// The side decides the rent: a `TradeExecution` is a buy and then a sell of
/// the same parcel — [`TradeExecution::malformed`] refuses anything else — so
/// the buy is the leg that opens the account and the sell is the leg that
/// closes it.
pub fn decompose_leg(leg: &ExecutionLeg, schedule: &FeeSchedule) -> LegFees {
    let venue = leg.fee_lamports;
    let creator = schedule.creator_cut_lamports(venue);
    // The protocol takes the remainder rather than its own floored share, so
    // the two halves add back to the lamports the curve actually took. Rounding
    // both would lose a lamport to the floor and put a residue into a line that
    // is supposed to be exact.
    let protocol = venue.saturating_sub(creator);

    let rent = match (leg.side, schedule.reclaims_ata_rent) {
        (Side::Buy, _) => i64::try_from(schedule.ata_rent_lamports).unwrap_or(i64::MAX),
        (Side::Sell, true) => -i64::try_from(schedule.ata_rent_lamports).unwrap_or(i64::MAX),
        (Side::Sell, false) => 0,
    };

    let signature = schedule.signature_cost_lamports();
    let priority = schedule.priority_cost_lamports();
    let total = i128::from(venue)
        + i128::from(signature)
        + i128::from(priority)
        + i128::from(leg.tip_lamports)
        + i128::from(rent);

    LegFees {
        side: leg.side,
        venue_lamports: venue,
        protocol_lamports: protocol,
        creator_lamports: creator,
        signature_lamports: signature,
        priority_lamports: priority,
        jito_tip_lamports: leg.tip_lamports,
        rent_lamports: rent,
        total_lamports: total.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
    }
}

/// Both legs of one round trip, and their totals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradeFees {
    pub mint: String,
    pub opened_at_ms: i64,
    pub closed_at_ms: i64,
    pub entry: LegFees,
    pub exit: LegFees,
    pub venue_lamports: u64,
    pub protocol_lamports: u64,
    pub creator_lamports: u64,
    pub signature_lamports: u64,
    pub priority_lamports: u64,
    pub jito_tip_lamports: u64,
    pub rent_lamports: i64,
    pub total_lamports: i64,
    /// The entry stake, which every share below is taken against.
    pub notional_lamports: u64,
    /// The total over the notional, in basis points, rounded up.
    pub total_bps_of_notional: u16,
}

impl TradeFees {
    /// The order the run walks fee rows in — the same one
    /// [`TradeAttribution::order_key`] uses, so the two per-trade tables in a
    /// report line up row for row.
    fn order_key(&self) -> (i64, i64, &str) {
        (self.closed_at_ms, self.opened_at_ms, self.mint.as_str())
    }

    /// Whether both legs' splits are exact and the totals are their sum.
    pub fn balances(&self) -> bool {
        self.entry.venue_splits()
            && self.exit.venue_splits()
            && self.protocol_lamports + self.creator_lamports == self.venue_lamports
            && i128::from(self.total_lamports)
                == i128::from(self.entry.total_lamports) + i128::from(self.exit.total_lamports)
    }
}

/// Splits one round trip into everything it paid.
pub fn decompose_trade(trade: &TradeExecution, schedule: &FeeSchedule) -> TradeFees {
    let entry = decompose_leg(&trade.entry, schedule);
    let exit = decompose_leg(&trade.exit, schedule);
    let notional = trade.entry.gross_lamports;

    let add = |left: u64, right: u64| left.saturating_add(right);
    let venue = add(entry.venue_lamports, exit.venue_lamports);
    let protocol = add(entry.protocol_lamports, exit.protocol_lamports);
    let creator = add(entry.creator_lamports, exit.creator_lamports);
    let total = i128::from(entry.total_lamports) + i128::from(exit.total_lamports);

    TradeFees {
        mint: trade.mint.clone(),
        opened_at_ms: trade.entry.at_ms,
        closed_at_ms: trade.exit.at_ms,
        entry,
        exit,
        venue_lamports: venue,
        protocol_lamports: protocol,
        creator_lamports: creator,
        signature_lamports: add(entry.signature_lamports, exit.signature_lamports),
        priority_lamports: add(entry.priority_lamports, exit.priority_lamports),
        jito_tip_lamports: add(entry.jito_tip_lamports, exit.jito_tip_lamports),
        rent_lamports: entry.rent_lamports.saturating_add(exit.rent_lamports),
        total_lamports: total.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
        notional_lamports: notional,
        total_bps_of_notional: if notional == 0 || total <= 0 {
            0
        } else {
            mul_div_ceil(
                total.unsigned_abs(),
                u128::from(BPS_DENOMINATOR),
                u128::from(notional),
            )
            .min(u128::from(BPS_DENOMINATOR)) as u16
        },
    }
}

/// Every charge across a run, split into the lines somebody can argue with.
///
/// The shares are of the **total charged**, not of the notional, so they answer
/// "where did the fee budget go" rather than "what did trading cost". The
/// second question is [`AttributionSummary::cost_bps_of_notional`], and the two
/// are kept apart because a run can have a tiny fee budget spent almost
/// entirely on tips and a run can have an enormous one spent almost entirely on
/// the venue, and reporting either as one number loses the distinction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeDecomposition {
    /// The schedule the run was decomposed under, echoed so the split travels
    /// with the numbers it produced.
    pub schedule: FeeSchedule,
    pub trades: u32,
    pub legs: u32,
    pub notional_lamports: u64,
    pub venue_lamports: u64,
    pub protocol_lamports: u64,
    pub creator_lamports: u64,
    pub signature_lamports: u64,
    pub priority_lamports: u64,
    pub jito_tip_lamports: u64,
    pub rent_lamports: i64,
    pub total_lamports: i64,
    /// Each line over the total charged, in basis points, floored. Floored
    /// rather than rounded up because these are shares of one number and
    /// rounding every one of them up would make them sum to more than all of
    /// it — `shares_residual_bps` is what the floors left over.
    pub venue_share_bps: u16,
    pub signature_share_bps: u16,
    pub priority_share_bps: u16,
    pub jito_tip_share_bps: u16,
    pub rent_share_bps: i32,
    /// `10 000` less the four shares above, which is where the floors went. A
    /// handful of basis points at most; anything larger is a bug in this file.
    pub shares_residual_bps: i32,
    /// The total charged over the traded notional, in basis points, rounded up.
    pub total_bps_of_notional: u16,
    /// One row per trade, in [`TradeAttribution::order_key`] order so this
    /// table and the report's trade table line up row for row.
    pub rows: Vec<TradeFees>,
}

impl FeeDecomposition {
    /// An empty book under one schedule.
    pub fn empty(schedule: FeeSchedule) -> Self {
        FeeDecomposition {
            schedule,
            trades: 0,
            legs: 0,
            notional_lamports: 0,
            venue_lamports: 0,
            protocol_lamports: 0,
            creator_lamports: 0,
            signature_lamports: 0,
            priority_lamports: 0,
            jito_tip_lamports: 0,
            rent_lamports: 0,
            total_lamports: 0,
            venue_share_bps: 0,
            signature_share_bps: 0,
            priority_share_bps: 0,
            jito_tip_share_bps: 0,
            rent_share_bps: 0,
            shares_residual_bps: 0,
            total_bps_of_notional: 0,
            rows: Vec::new(),
        }
    }

    /// Folds a set of per-trade rows into one book.
    ///
    /// The rows are sorted here rather than by the caller, so the table is a
    /// function of the trades and not of the order somebody assembled them in.
    pub fn of(mut rows: Vec<TradeFees>, schedule: FeeSchedule) -> Self {
        rows.sort_by(|left, right| left.order_key().cmp(&right.order_key()));
        let mut book = FeeDecomposition::empty(schedule);

        let mut notional: u128 = 0;
        let mut venue: u128 = 0;
        let mut protocol: u128 = 0;
        let mut creator: u128 = 0;
        let mut signature: u128 = 0;
        let mut priority: u128 = 0;
        let mut tip: u128 = 0;
        let mut rent: i128 = 0;

        for row in &rows {
            book.trades = book.trades.saturating_add(1);
            book.legs = book.legs.saturating_add(2);
            notional = notional.saturating_add(u128::from(row.notional_lamports));
            venue = venue.saturating_add(u128::from(row.venue_lamports));
            protocol = protocol.saturating_add(u128::from(row.protocol_lamports));
            creator = creator.saturating_add(u128::from(row.creator_lamports));
            signature = signature.saturating_add(u128::from(row.signature_lamports));
            priority = priority.saturating_add(u128::from(row.priority_lamports));
            tip = tip.saturating_add(u128::from(row.jito_tip_lamports));
            rent = rent.saturating_add(i128::from(row.rent_lamports));
        }

        let clamp_u64 = |value: u128| -> u64 { value.min(u128::from(u64::MAX)) as u64 };
        let clamp_i64 =
            |value: i128| -> i64 { value.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64 };

        book.notional_lamports = clamp_u64(notional);
        book.venue_lamports = clamp_u64(venue);
        book.protocol_lamports = clamp_u64(protocol);
        book.creator_lamports = clamp_u64(creator);
        book.signature_lamports = clamp_u64(signature);
        book.priority_lamports = clamp_u64(priority);
        book.jito_tip_lamports = clamp_u64(tip);
        book.rent_lamports = clamp_i64(rent);

        let total = venue
            .saturating_add(signature)
            .saturating_add(priority)
            .saturating_add(tip) as i128
            + rent;
        book.total_lamports = clamp_i64(total);

        if total > 0 {
            let share = |part: u128| -> u16 {
                mul_div_floor(part, u128::from(BPS_DENOMINATOR), total.unsigned_abs())
                    .min(u128::from(BPS_DENOMINATOR)) as u16
            };
            book.venue_share_bps = share(venue);
            book.signature_share_bps = share(signature);
            book.priority_share_bps = share(priority);
            book.jito_tip_share_bps = share(tip);
            // Rent is the one line that can be negative, so its share is
            // signed and floored towards negative infinity like every other
            // signed ratio in this module.
            book.rent_share_bps =
                floor_div_i128(rent.saturating_mul(i128::from(BPS_DENOMINATOR)), total)
                    .clamp(i128::from(i32::MIN), i128::from(i32::MAX)) as i32;
            book.shares_residual_bps = i32::from(BPS_DENOMINATOR as u16)
                - i32::from(book.venue_share_bps)
                - i32::from(book.signature_share_bps)
                - i32::from(book.priority_share_bps)
                - i32::from(book.jito_tip_share_bps)
                - book.rent_share_bps;
        }

        book.total_bps_of_notional = if notional == 0 || total <= 0 {
            0
        } else {
            mul_div_ceil(total.unsigned_abs(), u128::from(BPS_DENOMINATOR), notional)
                .min(u128::from(BPS_DENOMINATOR)) as u16
        };

        book.rows = rows;
        book
    }

    /// Whether every line adds up.
    ///
    /// The venue split is exact on every leg, the totals are the sum of the
    /// rows, and the four positive lines plus rent are the total. Asserted in
    /// the tests rather than reported, because a decomposition that does not
    /// add up is a bug in this file rather than a finding about the run.
    pub fn balances(&self) -> bool {
        let lines = i128::from(self.venue_lamports)
            + i128::from(self.signature_lamports)
            + i128::from(self.priority_lamports)
            + i128::from(self.jito_tip_lamports)
            + i128::from(self.rent_lamports);
        lines == i128::from(self.total_lamports)
            && self.protocol_lamports.saturating_add(self.creator_lamports) == self.venue_lamports
            && self.rows.iter().all(TradeFees::balances)
    }

    /// Whether this decomposition and the identity's summary describe the same
    /// run.
    ///
    /// The two are computed by different code from the same executions, and
    /// they overlap in exactly two places: what the venue took and what the
    /// block market took. A disagreement means one of them is reading the legs
    /// wrongly, and there would otherwise be no way to say which.
    pub fn reconciles(&self, summary: &AttributionSummary) -> bool {
        self.venue_lamports == summary.fees_charged_lamports
            && self.jito_tip_lamports == summary.tip_lamports
    }
}

/// Splits a whole run into everything it paid.
pub fn decompose_run(trades: &[TradeExecution], schedule: &FeeSchedule) -> FeeDecomposition {
    FeeDecomposition::of(
        trades
            .iter()
            .map(|trade| decompose_trade(trade, schedule))
            .collect(),
        *schedule,
    )
}

// ===========================================================================
// Driving it from a recording
// ===========================================================================

/// Where one curve stood at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TracePoint {
    pub at_ms: i64,
    /// Real SOL in the pool, which is what a recording carries and what
    /// `CurveState::at_real_sol` reconstructs the other five reserves from.
    pub real_sol_lamports: u64,
}

/// One mint's history, in the order it happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MintTrace {
    pub mint: String,
    pub points: Vec<TracePoint>,
}

/// A historical replay trace, reduced to what an attribution needs.
///
/// # What is thrown away, and why that is safe
///
/// A recording carries every frame the sockets saw. What a decomposition needs
/// from it is one number per instant per mint: how much real SOL was in the
/// pool. Everything else about a swap — who made it, from what wallet, funded
/// by whom — moves the curve and is then finished, and the curve is what the
/// next fill prices against.
///
/// The reduction is exact for a curve that started at the protocol's launch
/// reserves, because `CurveState::at_real_sol` derives the rest from the
/// invariant rather than tracking them. It is *not* exact for a curve first
/// seen mid-life with reserves that do not sit on that invariant, and a trace
/// built from one would quietly re-anchor it. [`ReplayTrace::from_events`]
/// starts every mint at the curve its launch event carried, so the only way to
/// hit that is to record a launch whose reserves are off the invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReplayTrace {
    /// One entry per mint, sorted by mint, each with its points in time order.
    pub mints: Vec<MintTrace>,
}

impl ReplayTrace {
    /// Walks a decoded recording and records where each curve stood.
    ///
    /// Flow the curve refuses is skipped rather than applied: a swap that could
    /// not have executed did not move the pool, and forcing it through would
    /// put the trace on a curve nobody traded. Flow for a mint with no launch
    /// event is skipped for the same reason — there is nothing to anchor it to,
    /// and inventing a launch would be inventing a price.
    pub fn from_events(events: &[LaunchEvent], fee_bps: u16) -> Self {
        // A `BTreeMap` rather than a hash map, for the reason every ordering in
        // this module is explicit: the mints come out in key order on every
        // machine, and a report keyed by iteration order of a hash map is a
        // report that changes when the hasher is seeded differently.
        let mut curves: BTreeMap<String, (CurveState, Vec<TracePoint>)> = BTreeMap::new();

        for event in events {
            match event {
                LaunchEvent::Launch(open) => {
                    curves.entry(open.mint.clone()).or_insert_with(|| {
                        (
                            open.curve,
                            vec![TracePoint {
                                at_ms: open.at_ms,
                                real_sol_lamports: open.curve.real_sol_reserves,
                            }],
                        )
                    });
                }
                LaunchEvent::Flow(flow) => {
                    let Some((curve, points)) = curves.get_mut(&flow.mint) else {
                        continue;
                    };
                    let moved = match flow.side {
                        Side::Buy => curve
                            .quote_buy(flow.gross_lamports, fee_bps)
                            .ok()
                            .map(|fill| curve.after_buy(&fill)),
                        Side::Sell => curve
                            .quote_sell(flow.tokens, fee_bps)
                            .ok()
                            .map(|fill| curve.after_sell(&fill)),
                    };
                    let Some(next) = moved else {
                        continue;
                    };
                    *curve = next;
                    points.push(TracePoint {
                        at_ms: flow.at_ms,
                        real_sol_lamports: curve.real_sol_reserves,
                    });
                }
                // Our own decisions, and the labels a forensic pass attaches.
                // None of them moves a curve, and a trace that treated our
                // entries as market flow would be pricing our own order twice.
                LaunchEvent::Entry(_)
                | LaunchEvent::Exit(_)
                | LaunchEvent::Holders(_)
                | LaunchEvent::Pull(_)
                | LaunchEvent::Label(_) => {}
            }
        }

        ReplayTrace {
            mints: curves
                .into_iter()
                .map(|(mint, (_, points))| MintTrace { mint, points })
                .collect(),
        }
    }

    /// How many observations the trace holds.
    pub fn points(&self) -> usize {
        self.mints.iter().map(|trace| trace.points.len()).sum()
    }

    /// Turns the trace into round trips to attribute.
    ///
    /// Deterministic by construction: the rule is an arithmetic one on indices,
    /// so the same recording and the same rules produce the same corpus on
    /// every machine, and a report over it is a function of the recording.
    pub fn round_trips(&self, rules: &TraceRules) -> Vec<RoundTripPlan> {
        let mut plans = Vec::new();
        for trace in &self.mints {
            let points = &trace.points;
            let mut entry = rules.first_entry_point;
            while let Some(exit) = entry.checked_add(rules.hold_points) {
                if exit >= points.len() {
                    break;
                }
                plans.push(RoundTripPlan {
                    mint: trace.mint.clone(),
                    opened_at_ms: points[entry].at_ms,
                    closed_at_ms: points[exit].at_ms,
                    entry_real_sol_lamports: points[entry].real_sol_lamports,
                    exit_real_sol_lamports: points[exit].real_sol_lamports,
                    gross_lamports: rules.gross_lamports,
                    entry_ticks: window_prices(points, entry, rules.tick_window),
                    exit_ticks: window_prices(points, exit, rules.tick_window),
                });
                // A stride of zero is one round trip per mint rather than an
                // endless one: an index that does not advance is not a smaller
                // step, it is a loop.
                if rules.stride == 0 {
                    break;
                }
                entry += rules.stride;
            }
        }
        plans
    }
}

/// How a trace is cut into round trips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceRules {
    /// What we commit at each entry, gross.
    pub gross_lamports: u64,
    /// How many observations after the entry the exit is taken at.
    pub hold_points: usize,
    /// How many observations of price history feed the volatility term.
    pub tick_window: usize,
    /// The observation the first entry is taken at. One rather than zero by
    /// default, so the entry has at least one prior tick behind it.
    pub first_entry_point: usize,
    /// Observations between successive entries on one mint. Zero is one round
    /// trip per mint.
    pub stride: usize,
}

impl Default for TraceRules {
    fn default() -> Self {
        TraceRules {
            gross_lamports: LAMPORTS_PER_SOL / 10,
            hold_points: 4,
            tick_window: 8,
            first_entry_point: 1,
            stride: 0,
        }
    }
}

impl TraceRules {
    /// The same rules, taking every round trip a mint's history allows.
    pub const fn laddered(mut self, stride: usize) -> Self {
        self.stride = stride;
        self
    }

    /// The same rules at a different size.
    pub const fn sized(mut self, gross_lamports: u64) -> Self {
        self.gross_lamports = gross_lamports;
        self
    }
}

/// The prices over the `window` observations ending at `at`, in the unit
/// [`crate::mev_sim::curve_price_micros`] produces.
fn window_prices(points: &[TracePoint], at: usize, window: usize) -> Vec<u64> {
    if window == 0 || at >= points.len() {
        return Vec::new();
    }
    let start = at.saturating_sub(window.saturating_sub(1));
    points[start..=at]
        .iter()
        .map(|point| curve_price_micros(&CurveState::at_real_sol(point.real_sol_lamports)))
        .collect()
}

/// Attributes a whole recording.
///
/// The end of the road this module is for: a replay trace in, and every line of
/// the identity plus the fee decomposition out, as a function of the recording
/// and the two configurations and nothing else.
pub fn attribute_trace(
    trace: &ReplayTrace,
    rules: &TraceRules,
    config: &AttributionConfig,
) -> AttributionReport {
    attribute_plans(&trace.round_trips(rules), config)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backtest::{FlowEvent, LaunchOpen};
    use crate::execution::TipPolicy;
    use crate::mev_sim::DEFAULT_MAX_PENALTY_BPS;

    // -----------------------------------------------------------------------
    // fixtures
    // -----------------------------------------------------------------------

    /// One round trip, described by where the curve was at each end.
    ///
    /// Curve positions are in whole SOL of real reserve, which is the number a
    /// fixture records; `CurveState::at_real_sol` derives the rest. The price a
    /// position implies is `y²/k`, so a move from 10 SOL to 25 SOL is a move of
    /// `(55/40)²` — a little under double — and the winners and losers below are
    /// chosen by that arithmetic rather than by hope.
    fn plan(
        mint: &str,
        opened_at_ms: i64,
        closed_at_ms: i64,
        entry_sol: u64,
        exit_sol: u64,
        gross_lamports: u64,
    ) -> RoundTripPlan {
        RoundTripPlan {
            mint: mint.to_string(),
            opened_at_ms,
            closed_at_ms,
            entry_real_sol_lamports: entry_sol * LAMPORTS_PER_SOL,
            exit_real_sol_lamports: exit_sol * LAMPORTS_PER_SOL,
            gross_lamports,
            entry_ticks: Vec::new(),
            exit_ticks: Vec::new(),
        }
    }

    /// A corpus of five mints: two winners, two losers, and one that went
    /// nowhere, at curve positions from nearly-dead to nearly-graduated.
    fn corpus() -> Vec<RoundTripPlan> {
        vec![
            plan("MintAlpha", 1_000, 11_000, 10, 25, LAMPORTS_PER_SOL / 2),
            plan("MintBravo", 2_000, 12_000, 40, 20, LAMPORTS_PER_SOL),
            plan("MintCharlie", 3_000, 13_000, 60, 80, 2 * LAMPORTS_PER_SOL),
            plan("MintDelta", 4_000, 14_000, 30, 30, LAMPORTS_PER_SOL / 4),
            plan("MintEcho", 5_000, 15_000, 5, 2, LAMPORTS_PER_SOL / 10),
        ]
    }

    /// The passive baseline: fees, the curve and tips, and nobody in front.
    fn passive() -> AttributionConfig {
        AttributionConfig {
            cents_per_sol: 15_000,
            ..AttributionConfig::default()
        }
    }

    /// A run against an adversary funded well enough to actually find something.
    ///
    /// The default purse cannot clear its landing cost at these sizes, which is
    /// the right answer for that configuration and a useless one for testing the
    /// arithmetic of a penalty.
    fn against(profile: AdversaryProfile) -> AttributionConfig {
        let mut config = passive().against(profile);
        config.adversary = config
            .adversary
            .bounded(20 * LAMPORTS_PER_SOL, DEFAULT_MAX_PENALTY_BPS);
        config.adversary.landing_cost_lamports = 1_000_000;
        config
    }

    fn report_against(profile: AdversaryProfile) -> AttributionReport {
        attribute_plans(&corpus(), &against(profile))
    }

    // -----------------------------------------------------------------------
    // the fee split
    // -----------------------------------------------------------------------

    /// The one property the split has to have: it explains the charged number
    /// without changing it, on every trade and on the totals.
    #[test]
    fn the_fee_columns_account_for_every_charged_lamport() {
        for profile in AdversaryProfile::ALL {
            let report = report_against(profile);
            for row in &report.trades {
                assert!(
                    row.fees_decompose(),
                    "{profile:?} {}: {} + {} != {}",
                    row.mint,
                    row.protocol_fee_charged_lamports,
                    row.creator_fee_charged_lamports,
                    row.fees_charged_lamports
                );
                assert!(
                    row.creator_fee_charged_lamports > 0,
                    "{}: the fixture is not dust",
                    row.mint
                );
                assert!(
                    row.protocol_fee_charged_lamports > row.creator_fee_charged_lamports,
                    "{}: ninety-five is more than five",
                    row.mint
                );
            }
            assert!(
                report.summary.fees_decompose(),
                "{profile:?}: the totals do not decompose"
            );
        }
    }

    #[test]
    fn the_fee_totals_are_the_sum_of_the_trades() {
        let report = report_against(AdversaryProfile::PredatorySandwich);
        let sum =
            |get: fn(&TradeAttribution) -> u128| -> u128 { report.trades.iter().map(get).sum() };
        assert_eq!(
            u128::from(report.summary.protocol_fee_charged_lamports),
            sum(|row| u128::from(row.protocol_fee_charged_lamports))
        );
        assert_eq!(
            u128::from(report.summary.creator_fee_charged_lamports),
            sum(|row| u128::from(row.creator_fee_charged_lamports))
        );
    }

    /// Where the line falls is a report decision and never a price decision. Two
    /// runs that differ only in the creator share must produce the same PnL, the
    /// same identity, and the same charged total — and differ in exactly the two
    /// columns that say who took it.
    #[test]
    fn moving_the_split_moves_no_lamport_of_pnl() {
        let mut lumped = passive();
        lumped.creator_fee_bps = 0;
        let mut split = passive();
        split.creator_fee_bps = 40;

        let flat = attribute_plans(&corpus(), &lumped);
        let shared = attribute_plans(&corpus(), &split);
        assert_eq!(flat.trades.len(), shared.trades.len());

        for (left, right) in flat.trades.iter().zip(shared.trades.iter()) {
            assert_eq!(
                left.realized_pnl_lamports, right.realized_pnl_lamports,
                "{}",
                left.mint
            );
            assert_eq!(
                left.protocol_fee_lamports, right.protocol_fee_lamports,
                "{}",
                left.mint
            );
            assert_eq!(
                left.fees_charged_lamports, right.fees_charged_lamports,
                "{}",
                left.mint
            );
            assert_eq!(
                left.residual_lamports, right.residual_lamports,
                "{}",
                left.mint
            );

            // A whole fee to the venue is what a run with no creator share says.
            assert_eq!(left.creator_fee_charged_lamports, 0, "{}", left.mint);
            assert_eq!(
                left.protocol_fee_charged_lamports,
                left.fees_charged_lamports
            );
            // And four tenths of it is what the other one says.
            assert!(right.creator_fee_charged_lamports > left.creator_fee_charged_lamports);
            assert!(right.fees_decompose());
        }
        assert_eq!(
            flat.summary.realized_pnl_lamports, shared.summary.realized_pnl_lamports,
            "the split is a column, not a cost"
        );
    }

    #[test]
    fn a_creator_share_larger_than_the_fee_takes_the_fee_and_no_more() {
        let config = AttributionConfig {
            creator_fee_bps: 9_999,
            ..passive()
        };
        let split = config.fee_split();
        assert_eq!(
            split.total_bps, config.fee_bps,
            "the total is still the fee"
        );
        assert_eq!(split.creator_bps, config.fee_bps);
        assert_eq!(
            split.protocol_bps, 0,
            "and the venue's share does not go negative"
        );

        let report = attribute_plans(&corpus(), &config);
        for row in &report.trades {
            assert!(row.fees_decompose(), "{}", row.mint);
            assert_eq!(row.protocol_fee_charged_lamports, 0, "{}", row.mint);
        }
    }

    /// The column is defaulted rather than required, so a report written before
    /// it existed still reads — as a run that did not split its fee, which is
    /// what it was.
    #[test]
    fn a_stored_config_without_the_column_reads_as_no_creator_share() {
        let json = serde_json::to_value(passive()).expect("it serialises");
        assert_eq!(json["creator_fee_bps"], u64::from(DEFAULT_CREATOR_FEE_BPS));

        let mut older = json.clone();
        older
            .as_object_mut()
            .expect("an object")
            .remove("creator_fee_bps");
        let read: AttributionConfig = serde_json::from_value(older).expect("it still reads");
        assert_eq!(read.creator_fee_bps, 0);
        assert_eq!(
            read.fee_bps, DEFAULT_FEE_BPS,
            "and the fee it was priced at is untouched"
        );
        assert_eq!(read.fee_split().protocol_bps, DEFAULT_FEE_BPS);
    }

    // -----------------------------------------------------------------------
    // the identity
    // -----------------------------------------------------------------------

    #[test]
    fn the_identity_closes_on_every_trade_and_on_the_totals() {
        for profile in AdversaryProfile::ALL {
            let report = report_against(profile);
            assert_eq!(
                report.trades.len(),
                corpus().len(),
                "{profile:?}: a trade was dropped"
            );
            assert!(
                report.refusals.is_empty(),
                "{profile:?}: {:?}",
                report.refusals
            );

            for row in &report.trades {
                assert!(
                    row.balances(),
                    "{profile:?} {}: {} != {} - {} - {} - {} - {} + {}",
                    row.mint,
                    row.realized_pnl_lamports,
                    row.gross_alpha_lamports,
                    row.price_impact_lamports,
                    row.protocol_fee_lamports,
                    row.tip_lamports,
                    row.mev_penalty_lamports,
                    row.residual_lamports
                );
            }
            assert!(
                report.summary.balances(),
                "{profile:?}: the totals do not close"
            );
            assert!(report.balances());
        }
    }

    #[test]
    fn the_rounding_stays_inside_the_bound_it_declares() {
        for profile in AdversaryProfile::ALL {
            let report = report_against(profile);
            for row in &report.trades {
                assert!(
                    row.residual_within_bound(),
                    "{profile:?} {}: residual {} outside the bound of {} at a ratio of {} \
                     millionths",
                    row.mint,
                    row.residual_lamports,
                    row.residual_bound_lamports(),
                    row.carry_ratio_micros
                );
                // Against a notional in the hundreds of millions of lamports, a
                // residue of a few lamports is the point: it is arithmetic, not
                // a finding about the strategy.
                assert!(row.residual_lamports.unsigned_abs() * 1_000_000 < row.notional_lamports);
            }
        }
    }

    #[test]
    fn the_totals_are_the_sum_of_the_trades() {
        let report = report_against(AdversaryProfile::PredatorySandwich);
        let sum =
            |get: fn(&TradeAttribution) -> i128| -> i128 { report.trades.iter().map(get).sum() };
        assert_eq!(
            i128::from(report.summary.realized_pnl_lamports),
            sum(|row| i128::from(row.realized_pnl_lamports))
        );
        assert_eq!(
            i128::from(report.summary.gross_alpha_lamports),
            sum(|row| i128::from(row.gross_alpha_lamports))
        );
        assert_eq!(
            i128::from(report.summary.price_impact_lamports),
            sum(|row| i128::from(row.price_impact_lamports))
        );
        assert_eq!(
            i128::from(report.summary.protocol_fee_lamports),
            sum(|row| i128::from(row.protocol_fee_lamports))
        );
        assert_eq!(
            i128::from(report.summary.tip_lamports),
            sum(|row| i128::from(row.tip_lamports))
        );
        assert_eq!(
            i128::from(report.summary.mev_penalty_lamports),
            sum(|row| i128::from(row.mev_penalty_lamports))
        );
        assert_eq!(
            report.summary.trades as usize,
            report.summary.winners as usize
                + report.summary.losers as usize
                + report.summary.scratches as usize
        );
    }

    // -----------------------------------------------------------------------
    // what each line means
    // -----------------------------------------------------------------------

    #[test]
    fn a_flat_trade_is_all_cost_and_no_alpha() {
        let flat = vec![plan(
            "MintDelta",
            4_000,
            14_000,
            30,
            30,
            LAMPORTS_PER_SOL / 4,
        )];
        let report = attribute_plans(&flat, &passive());
        let row = &report.trades[0];

        assert_eq!(row.carry_ratio_micros, MICROS, "the price did not move");
        assert_eq!(row.gross_alpha_lamports, 0, "a flat trade earns nothing");
        assert!(
            row.realized_pnl_lamports < 0,
            "and pays for the round trip anyway"
        );
        assert_eq!(
            i128::from(row.realized_pnl_lamports),
            -i128::from(row.total_cost_lamports) + i128::from(row.residual_lamports)
        );
        assert!(row.price_impact_lamports > 0);
        assert!(row.protocol_fee_lamports > 0);
        assert!(row.tip_lamports > 0);
        assert_eq!(
            row.mev_penalty_lamports, 0,
            "nobody was in front of a passive run"
        );
    }

    #[test]
    fn gross_alpha_is_the_price_move_and_nothing_else() {
        // Alpha is a function of the stake and the two curves. Change the fee,
        // change the adversary, change the tip schedule: the alpha column is the
        // same, because none of those is a price.
        let baseline = attribute_plans(&corpus(), &passive());
        let mut cheaper = passive();
        cheaper.fee_bps = 25;
        cheaper.adversary.fee_bps = 25;
        cheaper.tips = TipSchedule::flat(0);
        let cheap = attribute_plans(&corpus(), &cheaper);

        for (left, right) in baseline.trades.iter().zip(cheap.trades.iter()) {
            assert_eq!(left.mint, right.mint);
            assert_eq!(
                left.gross_alpha_lamports, right.gross_alpha_lamports,
                "{}: alpha moved when only the costs did",
                left.mint
            );
        }
        // And the cheaper venue keeps more of it.
        assert!(cheap.summary.realized_pnl_lamports > baseline.summary.realized_pnl_lamports);
        assert!(cheap.summary.total_cost_lamports < baseline.summary.total_cost_lamports);
    }

    #[test]
    fn alpha_scales_with_the_stake() {
        let one = attribute_plans(
            &[plan("MintAlpha", 0, 1, 10, 25, LAMPORTS_PER_SOL)],
            &passive(),
        );
        let two = attribute_plans(
            &[plan("MintAlpha", 0, 1, 10, 25, 2 * LAMPORTS_PER_SOL)],
            &passive(),
        );
        let small = one.trades[0].gross_alpha_lamports;
        let large = two.trades[0].gross_alpha_lamports;
        // Twice the stake on the same price move is twice the alpha, to within
        // the one lamport the carry's floor is entitled to.
        assert!(
            (large - 2 * small).abs() <= 2,
            "{small} doubled is not {large}"
        );
    }

    #[test]
    fn an_entry_cost_is_carried_and_costs_more_than_its_face_value() {
        let winner = vec![plan(
            "MintAlpha",
            1_000,
            11_000,
            10,
            25,
            LAMPORTS_PER_SOL / 2,
        )];
        let report = attribute_plans(&winner, &passive());
        let row = &report.trades[0];

        assert!(
            row.carry_ratio_micros > MICROS,
            "this fixture has to be a winner"
        );
        assert!(
            row.protocol_fee_lamports > row.fees_charged_lamports,
            "a fee paid before a price move destroyed more proceeds than its face value"
        );

        // A loser carries the other way: a lamport paid at entry into a price
        // that then fell destroyed less than its face value.
        let loser = vec![plan("MintBravo", 2_000, 12_000, 40, 20, LAMPORTS_PER_SOL)];
        let fallen = attribute_plans(&loser, &passive());
        let row = &fallen.trades[0];
        assert!(row.carry_ratio_micros < MICROS);
        assert!(row.protocol_fee_lamports < row.fees_charged_lamports);
    }

    #[test]
    fn a_passive_run_has_no_mev_line_at_all() {
        let report = attribute_plans(&corpus(), &passive());
        assert_eq!(report.summary.mev_penalty_lamports, 0);
        assert_eq!(report.mev.total_penalty_lamports, 0);
        assert_eq!(report.mev.legs_attacked, 0);
        assert_eq!(report.mev.legs_modelled, 2 * corpus().len() as u32);
        assert_eq!(report.mev.profile, AdversaryProfile::PassiveTaker);
        assert!(report.mev.synthetic);
        for row in &report.trades {
            assert_eq!(row.mev_penalty_lamports, 0);
        }
    }

    #[test]
    fn an_adversary_only_ever_costs_money() {
        let baseline = attribute_plans(&corpus(), &passive());
        for profile in [
            AdversaryProfile::PredatorySandwich,
            AdversaryProfile::HighFrequencyBackrunner,
        ] {
            let attacked = report_against(profile);
            assert!(
                attacked.summary.mev_penalty_lamports > 0,
                "{profile:?} found nothing to do, so this proves nothing"
            );
            assert!(
                attacked.summary.realized_pnl_lamports < baseline.summary.realized_pnl_lamports,
                "{profile:?} did not cost anything"
            );
            assert!(attacked.mev.legs_attacked > 0);
            assert!(attacked.mev.worst_penalty_bps <= attacked.mev.max_penalty_bps);
        }
    }

    #[test]
    fn the_mev_line_does_not_double_count_our_own_impact() {
        let baseline = attribute_plans(&corpus(), &passive());
        let attacked = report_against(AdversaryProfile::PredatorySandwich);

        // Our own order's convexity is the same order into the same decision
        // curve either way. Being front-run leaves us a smaller parcel to sell,
        // so the impact line goes *down* while the MEV line appears — which is
        // the arrangement that proves the second is not a copy of the first.
        assert!(attacked.summary.mev_penalty_lamports > 0);
        assert!(attacked.summary.price_impact_lamports <= baseline.summary.price_impact_lamports);
    }

    #[test]
    fn a_backrunner_pays_nothing_on_the_way_in() {
        let report = report_against(AdversaryProfile::HighFrequencyBackrunner);
        assert_eq!(
            report.mev.entry_penalty_lamports, 0,
            "an order that lands after ours cannot change what ours received"
        );
        assert!(report.mev.exit_penalty_lamports > 0);
    }

    // -----------------------------------------------------------------------
    // determinism
    // -----------------------------------------------------------------------

    #[test]
    fn two_runs_of_one_corpus_produce_the_same_bytes() {
        for profile in AdversaryProfile::ALL {
            let first = report_against(profile);
            let second = report_against(profile);
            assert_eq!(first, second, "{profile:?}: two runs disagreed");
            assert_eq!(first.to_json(), second.to_json());
        }
    }

    #[test]
    fn the_order_the_caller_assembled_the_trades_in_does_not_matter() {
        let forwards = attribute_plans(&corpus(), &passive());
        let mut shuffled = corpus();
        shuffled.reverse();
        let backwards = attribute_plans(&shuffled, &passive());
        assert_eq!(forwards, backwards);
    }

    #[test]
    fn a_report_survives_a_round_trip_through_json() {
        let report = report_against(AdversaryProfile::PredatorySandwich);
        let decoded: AttributionReport =
            serde_json::from_str(&report.to_json()).expect("the report is its own schema");
        assert_eq!(report, decoded);
    }

    // -----------------------------------------------------------------------
    // zero states
    // -----------------------------------------------------------------------

    #[test]
    fn an_empty_run_reports_zeroes_rather_than_refusing() {
        let report = attribute_run(&[], &passive());
        assert_eq!(report.summary, AttributionSummary::empty());
        assert!(report.trades.is_empty());
        assert!(report.refusals.is_empty());
        assert!(report.balances());

        assert_eq!(
            report.returns.ending_equity_lamports,
            passive().starting_equity_lamports as i64
        );
        assert_eq!(report.returns.cumulative_return_bps, 0);
        assert_eq!(report.returns.max_drawdown_lamports, 0);
        assert_eq!(report.returns.sharpe_micros, None);
        assert_eq!(report.returns.sortino_micros, None);
        assert_eq!(report.returns.cumulative_log_return_micros, 0);

        // The bars are declared at zero rather than absent, so a diff against a
        // run that traded is a diff of counts rather than of structure.
        assert_eq!(report.slippage, SlippageDistribution::empty());
        assert_eq!(
            report.slippage.buckets.len(),
            SLIPPAGE_BUCKET_EDGES_BPS.len()
        );
        assert_eq!(report.slippage.samples, 0);
    }

    #[test]
    fn one_trade_has_no_sharpe_and_no_sortino() {
        let report = attribute_plans(&corpus()[..1], &passive());
        assert_eq!(report.summary.trades, 1);
        assert_eq!(
            report.returns.sharpe_micros, None,
            "one sample has no dispersion"
        );
        assert_eq!(report.returns.sortino_micros, None);
        assert_eq!(report.returns.stddev_return_bps_micros, 0);
        assert!(report.returns.mean_return_bps_micros != 0);
    }

    #[test]
    fn a_run_with_no_equity_reports_no_return_rather_than_dividing_by_it() {
        let mut broke = passive();
        broke.starting_equity_lamports = 0;
        let report = attribute_plans(&corpus(), &broke);
        assert_eq!(report.returns.starting_equity_lamports, 0);
        assert_eq!(report.returns.cumulative_return_bps, 0);
        // The lamport columns still say what happened.
        assert!(report.returns.ending_equity_lamports != 0);
    }

    #[test]
    fn no_sol_price_means_no_dollar_figures_rather_than_a_guess() {
        let mut priceless = passive();
        priceless.cents_per_sol = 0;
        let report = attribute_plans(&corpus(), &priceless);
        assert_eq!(report.summary.realized_pnl_usd_cents, 0);
        for row in &report.trades {
            assert_eq!(row.realized_pnl_usd_cents, 0);
        }
        // And with one, the dollars follow the lamports.
        let priced = attribute_plans(&corpus(), &passive());
        assert!(priced.summary.realized_pnl_usd_cents != 0);
        assert_eq!(
            priced.summary.realized_pnl_usd_cents.signum(),
            priced.summary.realized_pnl_lamports.signum()
        );
    }

    // -----------------------------------------------------------------------
    // refusals
    // -----------------------------------------------------------------------

    /// A well-formed round trip to spoil in the tests below.
    fn one_trade() -> TradeExecution {
        let plan = plan("MintAlpha", 1_000, 11_000, 10, 25, LAMPORTS_PER_SOL / 2);
        simulate_round_trip(&plan, &passive()).expect("the fixture quotes")
    }

    #[test]
    fn a_partial_close_is_refused_rather_than_attributed() {
        let mut trade = one_trade();
        trade.exit.tokens /= 2;
        let refusal =
            attribute_trade(&trade, &passive()).expect_err("half a parcel is not a trade");
        assert!(refusal.contains("MintAlpha"), "{refusal}");
        assert!(refusal.contains("partial close"), "{refusal}");

        let report = attribute_run(&[trade], &passive());
        assert!(report.trades.is_empty());
        assert_eq!(report.refusals.len(), 1);
        assert_eq!(report.summary, AttributionSummary::empty());
    }

    #[test]
    fn a_leg_that_does_not_add_up_is_refused() {
        let mut trade = one_trade();
        trade.entry.fee_lamports += 1;
        let refusal = attribute_trade(&trade, &passive()).expect_err("net plus fee is not gross");
        assert!(refusal.contains("is not gross"), "{refusal}");

        let mut backwards = one_trade();
        backwards.entry.side = Side::Sell;
        let refusal = attribute_trade(&backwards, &passive()).expect_err("a sell cannot open");
        assert!(refusal.contains("cannot be the buy"), "{refusal}");

        let mut priceless = one_trade();
        priceless.entry.virtual_sol_reserves = 0;
        let refusal = attribute_trade(&priceless, &passive()).expect_err("no price to carry from");
        assert!(refusal.contains("marginal price"), "{refusal}");
    }

    #[test]
    fn refusals_are_sorted_so_two_runs_refuse_in_the_same_order() {
        let mut first = one_trade();
        first.mint = "MintZulu".to_string();
        first.exit.tokens = 0;
        let mut second = one_trade();
        second.mint = "MintAlpha".to_string();
        second.exit.tokens = 0;

        let report = attribute_run(&[first, second], &passive());
        assert_eq!(report.refusals.len(), 2);
        assert!(report.refusals[0].starts_with("MintAlpha"));
        assert!(report.refusals[1].starts_with("MintZulu"));
    }

    #[test]
    fn a_plan_the_curve_refuses_is_reported_rather_than_dropped() {
        // A graduated curve has no quote at any size. §17 is explicit that this
        // is a hard branch rather than a continuous transition.
        let mut plans = corpus();
        plans.push(plan(
            "MintGraduated",
            6_000,
            16_000,
            90,
            95,
            LAMPORTS_PER_SOL,
        ));
        let report = attribute_plans(&plans, &passive());

        assert_eq!(report.trades.len(), corpus().len());
        assert_eq!(report.refusals.len(), 1);
        assert!(
            report.refusals[0].starts_with("MintGraduated"),
            "{:?}",
            report.refusals
        );
        assert!(
            report.refusals[0].contains("entry"),
            "{:?}",
            report.refusals
        );
    }

    // -----------------------------------------------------------------------
    // the equity curve
    // -----------------------------------------------------------------------

    #[test]
    fn the_drawdown_walks_realised_equity_in_close_order() {
        // Win, lose, lose, win. The deepest point is after the second loss and
        // the streak is two.
        let plans = vec![
            plan("MintWin1", 1_000, 11_000, 10, 25, LAMPORTS_PER_SOL / 2),
            plan("MintLoss1", 2_000, 12_000, 40, 20, LAMPORTS_PER_SOL),
            plan("MintLoss2", 3_000, 13_000, 60, 40, LAMPORTS_PER_SOL),
            plan("MintWin2", 4_000, 14_000, 20, 50, LAMPORTS_PER_SOL / 2),
        ];
        let report = attribute_plans(&plans, &passive());
        let pnl: Vec<i64> = report
            .trades
            .iter()
            .map(|row| row.realized_pnl_lamports)
            .collect();
        assert!(
            pnl[0] > 0 && pnl[1] < 0 && pnl[2] < 0 && pnl[3] > 0,
            "{pnl:?}"
        );

        let returns = &report.returns;
        assert_eq!(returns.longest_losing_streak, 2);
        assert_eq!(
            returns.max_drawdown_at_ms, 13_000,
            "the deepest point is the second loss"
        );
        assert_eq!(
            returns.max_drawdown_lamports,
            (pnl[1] + pnl[2]).unsigned_abs(),
            "the fall from the peak is both losses"
        );
        assert_eq!(returns.longest_underwater_ms, 14_000 - 11_000);
        assert_eq!(
            returns.ending_equity_lamports,
            passive().starting_equity_lamports as i64 + pnl.iter().sum::<i64>()
        );
        // The peak is after the first win and is never taken back: the closing
        // winner is smaller than the two losses between it and the peak.
        assert_eq!(
            returns.high_water_lamports,
            passive().starting_equity_lamports as i64 + pnl[0]
        );
        assert!(returns.ending_equity_lamports < returns.high_water_lamports);
        assert!(returns.max_drawdown_bps > 0);
    }

    #[test]
    fn a_run_that_never_goes_under_has_no_drawdown() {
        let plans = vec![
            plan("MintWin1", 1_000, 11_000, 10, 25, LAMPORTS_PER_SOL / 2),
            plan("MintWin2", 2_000, 12_000, 20, 50, LAMPORTS_PER_SOL / 2),
        ];
        let report = attribute_plans(&plans, &passive());
        assert_eq!(report.returns.max_drawdown_lamports, 0);
        assert_eq!(report.returns.max_drawdown_bps, 0);
        assert_eq!(report.returns.longest_underwater_ms, 0);
        assert_eq!(report.returns.longest_losing_streak, 0);
        assert!(report.returns.cumulative_return_bps > 0);
        assert_eq!(
            report.returns.sortino_micros, None,
            "nothing lost is not an infinite Sortino"
        );
    }

    // -----------------------------------------------------------------------
    // the statistics
    // -----------------------------------------------------------------------

    #[test]
    fn sortino_counts_only_the_downside() {
        assert_eq!(sortino_micros(&[]), None);
        assert_eq!(sortino_micros(&[500]), None, "one sample has no dispersion");
        assert_eq!(
            sortino_micros(&[500, 700]),
            None,
            "nothing lost has no downside deviation"
        );
        assert_eq!(downside_deviation_bps_micros(&[500, 700]), Some(0));

        // A sample with one loser: the deviation is that loser's distance from
        // zero, and Sortino beats Sharpe because the winners are not punished.
        let returns = [200, -100, 300];
        let deviation = downside_deviation_bps_micros(&returns).expect("two samples");
        assert_eq!(
            deviation,
            isqrt((100u128 * u128::from(MICROS)).pow(2) / 2) as u64
        );
        let sortino = sortino_micros(&returns).expect("a downside exists");
        let sharpe = crate::backtest::sharpe_micros(&returns).expect("a dispersion exists");
        assert!(
            sortino > sharpe,
            "sortino {sortino} should beat sharpe {sharpe} here"
        );
    }

    #[test]
    fn the_statistics_agree_with_the_columns_they_are_taken_from() {
        let report = attribute_plans(&corpus(), &passive());
        let returns: Vec<i32> = report.trades.iter().map(|row| row.return_bps).collect();
        assert_eq!(
            report.returns.sharpe_micros,
            crate::backtest::sharpe_micros(&returns)
        );
        assert_eq!(report.returns.sortino_micros, sortino_micros(&returns));
        assert_eq!(
            report.returns.downside_deviation_bps_micros,
            downside_deviation_bps_micros(&returns).unwrap_or(0)
        );
        assert!(report.returns.stddev_return_bps_micros > 0);
    }

    #[test]
    fn log_returns_add_where_basis_points_do_not() {
        // ln(2) is 0.693147… and the column is in millionths.
        let doubled = log_return_micros(LAMPORTS_PER_SOL, 2 * LAMPORTS_PER_SOL).expect("defined");
        assert!((doubled - 693_147).abs() <= 2, "ln 2 came out as {doubled}");
        let halved = log_return_micros(2 * LAMPORTS_PER_SOL, LAMPORTS_PER_SOL).expect("defined");
        assert!(
            (doubled + halved).abs() <= 2,
            "a double and a halve should cancel"
        );

        // A total loss has no log return rather than a made-up one.
        assert_eq!(log_return_micros(LAMPORTS_PER_SOL, 0), None);
        assert_eq!(log_return_micros(0, LAMPORTS_PER_SOL), None);
        assert_eq!(log_return_micros(LAMPORTS_PER_SOL, 1), None);
    }

    #[test]
    fn the_cumulative_log_return_is_the_sum_of_the_defined_ones() {
        let report = attribute_plans(&corpus(), &passive());
        let sum: i64 = report
            .trades
            .iter()
            .filter_map(|row| row.log_return_micros)
            .sum();
        assert_eq!(report.returns.cumulative_log_return_micros, sum);
        let undefined = report
            .trades
            .iter()
            .filter(|row| row.log_return_micros.is_none())
            .count() as u32;
        assert_eq!(report.returns.log_returns_undefined, undefined);
        // The sign has to agree with the basis-point column on the same trade.
        for row in &report.trades {
            if let Some(log) = row.log_return_micros {
                assert_eq!(
                    log.signum(),
                    i64::from(row.return_bps.signum()),
                    "{}",
                    row.mint
                );
            }
        }
    }

    #[test]
    fn the_slippage_distribution_is_nearest_rank_over_both_legs() {
        let distribution = SlippageDistribution::of(vec![10, 20, 30, 40, 50, 60, 70, 80, 90, 100]);
        assert_eq!(distribution.samples, 10);
        assert_eq!(distribution.min_bps, 10);
        assert_eq!(distribution.max_bps, 100);
        assert_eq!(distribution.p50_bps, 50);
        assert_eq!(distribution.p90_bps, 90);
        assert_eq!(distribution.p99_bps, 100);
        assert_eq!(distribution.mean_bps_micros, 55 * MICROS);

        let counts: Vec<u32> = distribution
            .buckets
            .iter()
            .map(|bucket| bucket.count)
            .collect();
        assert_eq!(counts, vec![1, 1, 3, 5, 0, 0, 0, 0, 0]);
        assert_eq!(counts.iter().sum::<u32>(), distribution.samples);

        // Every quantile is a number some leg actually paid, because nothing is
        // interpolated between two of them.
        let sample = vec![7u16, 7, 9];
        let odd = SlippageDistribution::of(sample.clone());
        for quantile in [odd.p50_bps, odd.p90_bps, odd.p99_bps] {
            assert!(sample.contains(&quantile));
        }
    }

    #[test]
    fn the_slippage_distribution_is_one_sample_per_leg() {
        let report = attribute_plans(&corpus(), &passive());
        assert_eq!(report.slippage.samples, 2 * report.summary.trades);
        for row in &report.trades {
            assert!(row.entry_slippage_bps > 0);
            assert!(row.exit_slippage_bps > 0);
            assert_eq!(
                row.worst_slippage_bps,
                row.entry_slippage_bps.max(row.exit_slippage_bps)
            );
        }
        assert!(report.slippage.min_bps <= report.slippage.p50_bps);
        assert!(report.slippage.p50_bps <= report.slippage.p90_bps);
        assert!(report.slippage.p90_bps <= report.slippage.p99_bps);
        assert!(report.slippage.p99_bps <= report.slippage.max_bps);
    }

    // -----------------------------------------------------------------------
    // tips
    // -----------------------------------------------------------------------

    #[test]
    fn the_tip_schedule_matches_the_execution_policy_it_mirrors() {
        let schedule = TipSchedule::annex_c();
        let policy = TipPolicy::emergency();
        for ev in [None, Some(-1), Some(0), Some(1_000_000), Some(500_000_000)] {
            for attempt in [0u32, 1, 3, 400] {
                let bid = policy
                    .bid("01912d4c-intent", ev, attempt)
                    .expect("an emergency bids");
                assert_eq!(
                    schedule.bid_lamports(ev, attempt, 0),
                    bid.lamports,
                    "Annex C disagreed with itself at ev {ev:?}, attempt {attempt}"
                );
            }
        }
    }

    #[test]
    fn a_tip_rises_with_congestion_and_stops_at_the_ceiling() {
        let schedule = TipSchedule::annex_c();
        let quiet = schedule.bid_lamports(None, 0, 0);
        assert_eq!(quiet, schedule.base_lamports);

        let headroom = schedule.max_lamports - schedule.base_lamports;
        let contested = schedule.bid_lamports(None, 0, MICROS);
        assert_eq!(
            contested,
            schedule.base_lamports + headroom * 2_500 / 10_000
        );
        assert!(contested > quiet);

        let mut previous = 0;
        for congestion in (0..=MICROS).step_by(37_000) {
            let bid = schedule.bid_lamports(None, 0, congestion);
            assert!(bid >= previous, "the tip fell at {congestion} millionths");
            assert!(bid <= schedule.max_lamports);
            previous = bid;
        }

        // Nothing gets past the ceiling: not an enormous expectation, not an
        // enormous retry count, not both at once in a fully contested block.
        assert_eq!(
            schedule.bid_lamports(Some(i64::MAX), u32::MAX, MICROS),
            schedule.max_lamports
        );
        // And a flat schedule bids its floor whatever it is told.
        let flat = TipSchedule::flat(7_777);
        assert_eq!(flat.bid_lamports(Some(i64::MAX), 9, MICROS), 7_777);
    }

    // -----------------------------------------------------------------------
    // the two properties the whole module is written for
    // -----------------------------------------------------------------------

    /// Names that reach floating point through somebody else's arithmetic.
    ///
    /// A file with no `f64` in it is not float-free if it calls something that
    /// has one, and the two below genuinely do: `replay::best_front_run` walks
    /// a geometric grid built with `f64::powf`, and `DrawSource::unit` scales a
    /// draw through an `f64` reciprocal. Both have integer counterparts that
    /// this pipeline uses instead — `backtest::best_front_run_deterministic`
    /// and `DrawSource::below` — so a call to either of these is a regression
    /// rather than a trade-off.
    const FLOAT_BEARING_CALLS: [&str; 2] = ["best_front_run(", ".unit("];

    #[test]
    fn nothing_in_either_module_computes_in_floating_point() {
        // `strategy_tests.rs` runs the same scan over `src/strategy` and allows
        // two lines there, because §7.2's schema column is an `f32` and
        // something has to make one. These two modules have no such exception:
        // nothing they compute is ever stored as a float.
        //
        // Above the test line only, exactly as that scan does — and here that is
        // load-bearing rather than inherited, because the scanner below names
        // the types it is looking for and would otherwise find itself.
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders: Vec<String> = Vec::new();

        for name in ["attribution.rs", "mev_sim.rs"] {
            let path = root.join(name);
            let source = std::fs::read_to_string(&path).expect("readable source");
            let code = source
                .split("#[cfg(test)]")
                .next()
                .expect("split always yields one");
            assert!(code.len() > 10_000, "{name}: the scan lost the module body");
            for (number, line) in code.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") {
                    continue;
                }
                if trimmed.contains("f64") || trimmed.contains("f32") {
                    offenders.push(format!("{name}:{}: {trimmed}", number + 1));
                }
                // And nothing that reaches a float through a call, which is
                // the way one would actually get in here now that the two
                // modules have been swept once.
                for call in FLOAT_BEARING_CALLS {
                    if trimmed.contains(call) {
                        offenders.push(format!(
                            "{name}:{}: reaches floating point through {call} — {trimmed}",
                            number + 1
                        ));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "floating point crept in:\n{}",
            offenders.join("\n")
        );
    }

    #[test]
    fn every_report_struct_is_comparable_to_the_byte() {
        // `Eq` rather than `PartialEq` alone is the property that makes the
        // equivalence gate a one-line assertion: two reports either are the same
        // report or they are not, with no `f64` in the tree to make a third
        // answer possible. This fails to compile rather than fails to pass if a
        // later edit puts a float into any of them.
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
        comparable::<crate::mev_sim::MevOutcome>();
        comparable::<crate::mev_sim::MevSummary>();
        comparable::<crate::mev_sim::AdversaryConfig>();
        comparable::<crate::mev_sim::AdversaryProfile>();
        comparable::<crate::mev_sim::MarketContext>();
        comparable::<RoundTripPlan>();

        // The fee decomposition.
        comparable::<FeeDecomposition>();
        comparable::<FeeSchedule>();
        comparable::<TradeFees>();
        comparable::<LegFees>();

        // The recording it can be driven from.
        comparable::<ReplayTrace>();
        comparable::<MintTrace>();
        comparable::<TracePoint>();
        comparable::<TraceRules>();

        // The simulation pipeline this module reports on. Named here rather
        // than only in `mev_sim`'s own tests because the claim is about the
        // whole pipeline: a float anywhere upstream of a report is a float in
        // the report, whichever file it was introduced in.
        comparable::<crate::mev_sim::FrontRunCost>();
        comparable::<crate::mev_sim::ReorgScenario>();
        comparable::<crate::mev_sim::ReorgOutcome>();
        comparable::<crate::mev_sim::ReorgSummary>();
        comparable::<crate::mev_sim::ReorgFate>();
        comparable::<crate::mev_sim::ReorgGrid>();
        comparable::<crate::mev_sim::PoolTarget>();
        comparable::<crate::mev_sim::PoolExtraction>();
        comparable::<crate::mev_sim::MultiPoolExtraction>();
    }

    #[test]
    fn the_tip_line_is_exactly_what_left_the_wallet() {
        // The one cost that is not carried. A tip is paid beside the stake, so
        // the same order buys the same tokens whatever the bundle bid, and a
        // carried tip would put `tip x (ratio - 1)` into the residual — which is
        // not rounding and is bounded by nothing.
        for profile in AdversaryProfile::ALL {
            let config = against(profile);
            for plan in corpus() {
                let trade = simulate_round_trip(&plan, &config).expect("quotes");
                let row = attribute_trade(&trade, &config).expect("attributes");
                assert_eq!(
                    row.tip_lamports,
                    trade.entry.tip_lamports + trade.exit.tip_lamports,
                    "{profile:?} {}: the tip line moved off its face value",
                    row.mint
                );
                assert!(row.tip_lamports > 0);
            }
        }
    }

    #[test]
    fn a_leg_records_what_was_in_front_of_it() {
        let config = against(AdversaryProfile::PredatorySandwich);
        let trade = simulate_round_trip(&corpus()[0], &config).expect("quotes");
        assert!(
            trade.entry.attacker_lamports > 0,
            "a funded front-run clears its fees here"
        );
        assert_eq!(
            trade.entry.attacker_tokens, 0,
            "a buy is front-run with lamports"
        );
        assert!(trade
            .entry
            .attacker_profit_lamports
            .is_some_and(|profit| profit > 0));
        assert!(
            trade.exit.attacker_tokens > 0,
            "a sell is front-run with tokens"
        );
        assert_eq!(trade.exit.attacker_lamports, 0);

        // And the run-level MEV summary is rebuilt from exactly those fields, so
        // a report can say how many legs were attacked without keeping a second
        // copy of the simulation beside it.
        let report = attribute_run(&[trade], &config);
        assert_eq!(report.mev.legs_modelled, 2);
        assert_eq!(report.mev.legs_attacked, 2);
        assert!(report.mev.attacker_profit_lamports > 0);
        // This fixture is a 10-SOL curve against a 20-SOL purse, so the ceiling
        // does bite — and the penalty it let through still respects it.
        assert!(report.mev.legs_bounded > 0);
        assert!(report.mev.worst_penalty_bps <= config.adversary.max_penalty_bps);

        // A passive run records nobody, which is not the same fact as a bounded
        // adversary that found nothing.
        let quiet = simulate_round_trip(&corpus()[0], &passive()).expect("quotes");
        assert_eq!(quiet.entry.attacker_lamports, 0);
        assert_eq!(quiet.exit.attacker_tokens, 0);
        assert!(!quiet.entry.bounded);
    }

    #[test]
    fn a_contested_moment_is_charged_for_twice_and_says_so() {
        // The same intensity blend sizes the adversary and prices the tip, so a
        // run cannot be told a moment was quiet enough to be safe and contested
        // enough to be expensive.
        let quiet = plan("MintCalm", 1_000, 11_000, 10, 25, LAMPORTS_PER_SOL / 2);
        let mut wild = quiet.clone();
        wild.mint = "MintWild".to_string();
        wild.entry_ticks = vec![1_000_000, 1_200_000, 1_000_000, 1_300_000];
        wild.exit_ticks = wild.entry_ticks.clone();

        let config = against(AdversaryProfile::PredatorySandwich);
        let calm = simulate_round_trip(&quiet, &config).expect("quotes");
        let storm = simulate_round_trip(&wild, &config).expect("quotes");

        assert!(storm.entry.intensity_micros > calm.entry.intensity_micros);
        assert!(storm.entry.tip_lamports > calm.entry.tip_lamports);
        assert!(storm.entry.mev_penalty_lamports > calm.entry.mev_penalty_lamports);
    }

    #[test]
    fn the_entry_bids_no_share_of_a_profit_nobody_has_computed_yet() {
        let trade = one_trade();
        // Annex C.2: a share of an expected value nobody computed is not a
        // smaller share, it is a made-up one. At entry there is no such number.
        assert_eq!(
            trade.entry.tip_lamports,
            passive()
                .tips
                .bid_lamports(None, 0, trade.entry.intensity_micros)
        );
        // The exit knows what the round trip made and bids a share of it.
        assert!(trade.exit.tip_lamports > trade.entry.tip_lamports);
    }

    // -----------------------------------------------------------------------
    // what a fill actually cost, line by line
    // -----------------------------------------------------------------------

    #[test]
    fn the_venue_split_is_exact_on_every_leg() {
        // The protocol takes the remainder rather than its own floored share,
        // so the two halves add back to the lamports the curve took. A split
        // that lost a lamport would be a decomposition that stopped
        // reconciling against the fills it came from.
        for profile in AdversaryProfile::ALL {
            let report = report_against(profile);
            for row in &report.fees.rows {
                assert!(row.balances(), "{profile:?} {}", row.mint);
                assert!(row.entry.venue_splits());
                assert!(row.exit.venue_splits());
            }
            assert_eq!(
                report.fees.protocol_lamports + report.fees.creator_lamports,
                report.fees.venue_lamports
            );
        }
        // And it holds at the awkward sizes as well as the tidy ones.
        let schedule = FeeSchedule::mainnet();
        for venue in [0u64, 1, 2, 19, 20, 21, 999, 1_000, 1_001, u64::MAX / 4] {
            let creator = schedule.creator_cut_lamports(venue);
            assert!(creator <= venue);
            assert_eq!(creator + (venue - creator), venue);
        }
    }

    #[test]
    fn the_decomposition_and_the_identity_describe_the_same_run() {
        // Two views computed by different code from the same executions. They
        // overlap in exactly two places, and a disagreement means one of them
        // is reading the legs wrongly.
        for profile in AdversaryProfile::ALL {
            let report = report_against(profile);
            assert!(
                report.fees.reconciles(&report.summary),
                "{profile:?}: venue {} against fees charged {}, tips {} against {}",
                report.fees.venue_lamports,
                report.summary.fees_charged_lamports,
                report.fees.jito_tip_lamports,
                report.summary.tip_lamports
            );
            assert!(report.fees.balances(), "{profile:?}");
            assert!(report.balances(), "{profile:?}");
        }
    }

    #[test]
    fn the_fee_lines_add_up_to_the_total() {
        let report = report_against(AdversaryProfile::PredatorySandwich);
        let fees = &report.fees;
        assert_eq!(
            i128::from(fees.total_lamports),
            i128::from(fees.venue_lamports)
                + i128::from(fees.signature_lamports)
                + i128::from(fees.priority_lamports)
                + i128::from(fees.jito_tip_lamports)
                + i128::from(fees.rent_lamports)
        );
        // And the totals are the sum of the rows.
        let sum = |get: fn(&TradeFees) -> i128| -> i128 { fees.rows.iter().map(get).sum() };
        assert_eq!(
            i128::from(fees.venue_lamports),
            sum(|r| i128::from(r.venue_lamports))
        );
        assert_eq!(
            i128::from(fees.jito_tip_lamports),
            sum(|r| i128::from(r.jito_tip_lamports))
        );
        assert_eq!(
            i128::from(fees.rent_lamports),
            sum(|r| i128::from(r.rent_lamports))
        );
        assert_eq!(
            i128::from(fees.total_lamports),
            sum(|r| i128::from(r.total_lamports))
        );
        assert_eq!(fees.trades as usize, fees.rows.len());
        assert_eq!(fees.legs, fees.trades * 2);
    }

    #[test]
    fn rent_comes_back_when_the_exit_closes_the_account_and_stays_when_it_does_not() {
        let corpus = corpus();
        let mut config = passive();

        config.fees = FeeSchedule::mainnet();
        assert!(config.fees.reclaims_ata_rent);
        let closed = attribute_plans(&corpus, &config);
        // Posted on the entry, taken back on the exit, so it nets to nothing
        // over a round trip — and is still on both legs, because a rent that
        // was posted and reclaimed is not a rent that never happened.
        assert_eq!(closed.fees.rent_lamports, 0);
        for row in &closed.fees.rows {
            assert_eq!(row.rent_lamports, 0);
            assert_eq!(row.entry.rent_lamports, ATA_RENT_LAMPORTS as i64);
            assert_eq!(row.exit.rent_lamports, -(ATA_RENT_LAMPORTS as i64));
        }

        config.fees.reclaims_ata_rent = false;
        let held = attribute_plans(&corpus, &config);
        assert_eq!(
            held.fees.rent_lamports,
            (ATA_RENT_LAMPORTS as i64) * held.fees.trades as i64
        );
        assert!(held.fees.total_lamports > closed.fees.total_lamports);
    }

    #[test]
    fn a_free_schedule_charges_nothing_the_curve_did_not() {
        let mut config = passive();
        config.fees = FeeSchedule::free();
        let report = attribute_plans(&corpus(), &config);

        assert_eq!(report.fees.signature_lamports, 0);
        assert_eq!(report.fees.priority_lamports, 0);
        assert_eq!(report.fees.rent_lamports, 0);
        assert_eq!(report.fees.creator_lamports, 0);
        // The venue's own cut and the block market's are still there: neither
        // is a policy this schedule sets.
        assert_eq!(report.fees.protocol_lamports, report.fees.venue_lamports);
        assert!(report.fees.venue_lamports > 0);
        assert!(report.fees.jito_tip_lamports > 0);
        assert_eq!(
            i128::from(report.fees.total_lamports),
            i128::from(report.fees.venue_lamports) + i128::from(report.fees.jito_tip_lamports)
        );
        assert!(report.fees.reconciles(&report.summary));
    }

    #[test]
    fn the_priority_fee_is_units_times_price_and_rounds_up() {
        let schedule = FeeSchedule::mainnet();
        // 120 000 units at 20 000 micro-lamports is 2 400 lamports exactly.
        assert_eq!(schedule.priority_cost_lamports(), 2_400);
        assert_eq!(schedule.signature_cost_lamports(), SIGNATURE_FEE_LAMPORTS);

        // A bid that divides to less than one lamport is charged one, not
        // nothing: the runtime rounds up and so does every cost in this module.
        let dust = FeeSchedule {
            compute_units_per_leg: 1,
            compute_unit_price_micro_lamports: 1,
            ..FeeSchedule::mainnet()
        };
        assert_eq!(dust.priority_cost_lamports(), 1);

        let none = FeeSchedule {
            compute_unit_price_micro_lamports: 0,
            ..FeeSchedule::mainnet()
        };
        assert_eq!(none.priority_cost_lamports(), 0);

        // Several signatures cost several base fees.
        let bundled = FeeSchedule {
            signatures_per_leg: 3,
            ..FeeSchedule::mainnet()
        };
        assert_eq!(
            bundled.signature_cost_lamports(),
            3 * SIGNATURE_FEE_LAMPORTS
        );
    }

    #[test]
    fn the_shares_are_shares_of_the_total_and_the_floors_are_reported() {
        let report = report_against(AdversaryProfile::PredatorySandwich);
        let fees = &report.fees;
        assert!(fees.total_lamports > 0);

        let share_of = |part: u64| -> u16 {
            mul_div_floor(
                u128::from(part),
                u128::from(BPS_DENOMINATOR),
                fees.total_lamports.unsigned_abs() as u128,
            ) as u16
        };
        assert_eq!(fees.venue_share_bps, share_of(fees.venue_lamports));
        assert_eq!(fees.signature_share_bps, share_of(fees.signature_lamports));
        assert_eq!(fees.priority_share_bps, share_of(fees.priority_lamports));
        assert_eq!(fees.jito_tip_share_bps, share_of(fees.jito_tip_lamports));

        // The floors are accounted for rather than absorbed. A handful of basis
        // points at most; anything larger is a bug in the fold.
        let sum = i32::from(fees.venue_share_bps)
            + i32::from(fees.signature_share_bps)
            + i32::from(fees.priority_share_bps)
            + i32::from(fees.jito_tip_share_bps)
            + fees.rent_share_bps
            + fees.shares_residual_bps;
        assert_eq!(sum, BPS_DENOMINATOR as i32);
        assert!(
            fees.shares_residual_bps.unsigned_abs() <= 8,
            "the floors lost {} bps",
            fees.shares_residual_bps
        );
    }

    #[test]
    fn a_run_with_nothing_in_it_has_an_empty_decomposition_rather_than_a_divided_zero() {
        let report = attribute_plans(&[], &passive());
        assert_eq!(report.fees, FeeDecomposition::empty(passive().fees));
        assert_eq!(report.fees.total_lamports, 0);
        assert_eq!(report.fees.venue_share_bps, 0);
        assert_eq!(report.fees.shares_residual_bps, 0);
        assert_eq!(report.fees.total_bps_of_notional, 0);
        assert!(report.fees.balances());
        assert!(report.fees.reconciles(&report.summary));
    }

    #[test]
    fn the_fee_rows_line_up_with_the_trade_rows() {
        // Two per-trade tables in one report, sorted the same way, so a reader
        // comparing line four of one against line four of the other is
        // comparing one trade.
        let report = report_against(AdversaryProfile::HighFrequencyBackrunner);
        assert_eq!(report.fees.rows.len(), report.trades.len());
        for (fees, trade) in report.fees.rows.iter().zip(report.trades.iter()) {
            assert_eq!(fees.mint, trade.mint);
            assert_eq!(fees.opened_at_ms, trade.opened_at_ms);
            assert_eq!(fees.closed_at_ms, trade.closed_at_ms);
            assert_eq!(fees.notional_lamports, trade.notional_lamports);
            // The identity's uncarried fee column is this row's venue line.
            assert_eq!(fees.venue_lamports, trade.fees_charged_lamports);
            assert_eq!(fees.jito_tip_lamports, trade.tip_lamports);
        }
    }

    #[test]
    fn the_order_the_caller_assembled_the_trades_in_does_not_move_the_fee_table() {
        let config = against(AdversaryProfile::PredatorySandwich);
        let forwards = attribute_plans(&corpus(), &config);
        let mut backwards_corpus = corpus();
        backwards_corpus.reverse();
        let backwards = attribute_plans(&backwards_corpus, &config);
        assert_eq!(forwards.fees, backwards.fees);
    }

    #[test]
    fn an_execution_that_could_not_be_attributed_is_not_charged_for() {
        // A partial close is two questions rather than one trade, so the
        // identity refuses it. Charging its fees anyway would make the two
        // views of the run disagree by exactly the refusals.
        let config = passive();
        let good = simulate_round_trip(&corpus()[0], &config).expect("a plan that fills");
        let mut partial = good.clone();
        partial.mint = "MintPartial".to_string();
        partial.exit.tokens = partial.entry.tokens / 2;

        let report = attribute_run(&[good, partial], &config);
        assert_eq!(report.refusals.len(), 1);
        assert_eq!(report.fees.trades, 1);
        assert!(report.fees.rows.iter().all(|row| row.mint != "MintPartial"));
        assert!(report.fees.reconciles(&report.summary));
    }

    #[test]
    fn a_bigger_tip_moves_the_tip_line_and_leaves_the_venue_alone() {
        let mut cheap = passive();
        cheap.tips = TipSchedule::flat(1_000);
        let mut dear = passive();
        dear.tips = TipSchedule::flat(50_000_000);

        let thin = attribute_plans(&corpus(), &cheap);
        let fat = attribute_plans(&corpus(), &dear);

        assert!(fat.fees.jito_tip_lamports > thin.fees.jito_tip_lamports);
        assert!(fat.fees.jito_tip_share_bps > thin.fees.jito_tip_share_bps);
        // The venue takes what the venue takes, whatever was bid to land.
        assert_eq!(fat.fees.venue_lamports, thin.fees.venue_lamports);
        assert_eq!(fat.fees.signature_lamports, thin.fees.signature_lamports);
        assert!(fat.fees.reconciles(&fat.summary));
        assert!(thin.fees.reconciles(&thin.summary));
    }

    #[test]
    fn the_decomposition_is_a_function_of_the_executions_and_the_schedule() {
        let config = against(AdversaryProfile::PredatorySandwich);
        let executions: Vec<TradeExecution> = corpus()
            .iter()
            .filter_map(|plan| simulate_round_trip(plan, &config).ok())
            .collect();
        assert!(!executions.is_empty());
        // The standalone entry point and the one the report takes agree.
        assert_eq!(
            decompose_run(&executions, &config.fees),
            attribute_run(&executions, &config).fees
        );
        assert_eq!(
            decompose_run(&executions, &config.fees),
            decompose_run(&executions, &config.fees)
        );
    }

    // -----------------------------------------------------------------------
    // driving it from a recording
    // -----------------------------------------------------------------------

    fn launch_at(mint: &str, at_ms: i64, real_sol_lamports: u64) -> LaunchEvent {
        LaunchEvent::Launch(LaunchOpen {
            mint: mint.to_string(),
            at_ms,
            creator: None,
            curve: CurveState::at_real_sol(real_sol_lamports),
        })
    }

    fn bought(mint: &str, at_ms: i64, gross_lamports: u64) -> LaunchEvent {
        LaunchEvent::Flow(FlowEvent {
            mint: mint.to_string(),
            at_ms,
            wallet: "SomeWallet".to_string(),
            funder: None,
            side: Side::Buy,
            gross_lamports,
            tokens: 0,
        })
    }

    fn sold(mint: &str, at_ms: i64, tokens: u64) -> LaunchEvent {
        LaunchEvent::Flow(FlowEvent {
            mint: mint.to_string(),
            at_ms,
            wallet: "SomeWallet".to_string(),
            funder: None,
            side: Side::Sell,
            gross_lamports: 0,
            tokens,
        })
    }

    /// A recording of one curve being walked up by other people's buys.
    fn recording() -> Vec<LaunchEvent> {
        let mut events = vec![launch_at("MintTrace", 1_000, 5 * LAMPORTS_PER_SOL)];
        for step in 0..12i64 {
            events.push(bought(
                "MintTrace",
                2_000 + step * 500,
                2 * LAMPORTS_PER_SOL,
            ));
        }
        events
    }

    #[test]
    fn a_trace_walks_the_curve_the_recording_moved() {
        let trace = ReplayTrace::from_events(&recording(), DEFAULT_FEE_BPS);
        assert_eq!(trace.mints.len(), 1);
        let points = &trace.mints[0].points;
        // The launch, plus one point per swap that executed.
        assert_eq!(points.len(), 13);
        assert_eq!(points[0].at_ms, 1_000);
        assert_eq!(points[0].real_sol_lamports, 5 * LAMPORTS_PER_SOL);
        // Buys only, so the pool fills monotonically and the clock never goes
        // backwards.
        for pair in points.windows(2) {
            assert!(pair[1].real_sol_lamports > pair[0].real_sol_lamports);
            assert!(pair[1].at_ms > pair[0].at_ms);
        }
    }

    #[test]
    fn a_sell_moves_the_trace_back_down() {
        let mut events = vec![launch_at("MintTrace", 1_000, 40 * LAMPORTS_PER_SOL)];
        events.push(bought("MintTrace", 2_000, 5 * LAMPORTS_PER_SOL));
        let parcel = CurveState::at_real_sol(40 * LAMPORTS_PER_SOL)
            .quote_buy(5 * LAMPORTS_PER_SOL, DEFAULT_FEE_BPS)
            .expect("quote")
            .tokens;
        events.push(sold("MintTrace", 3_000, parcel));

        let trace = ReplayTrace::from_events(&events, DEFAULT_FEE_BPS);
        let points = &trace.mints[0].points;
        assert_eq!(points.len(), 3);
        assert!(points[1].real_sol_lamports > points[0].real_sol_lamports);
        assert!(points[2].real_sol_lamports < points[1].real_sol_lamports);
    }

    #[test]
    fn flow_the_curve_would_refuse_does_not_move_the_trace() {
        // A sell of more than the pool can pay for did not happen, and forcing
        // it through would put the trace on a curve nobody traded.
        let events = vec![
            launch_at("MintTrace", 1_000, 5 * LAMPORTS_PER_SOL),
            sold("MintTrace", 2_000, u64::MAX / 2),
            bought("MintTrace", 3_000, LAMPORTS_PER_SOL),
        ];
        let trace = ReplayTrace::from_events(&events, DEFAULT_FEE_BPS);
        let points = &trace.mints[0].points;
        assert_eq!(points.len(), 2, "the refused sell is not a point");
        assert_eq!(points[1].at_ms, 3_000);
    }

    #[test]
    fn flow_with_no_launch_behind_it_has_nothing_to_anchor_to() {
        let events = vec![bought("MintOrphan", 2_000, LAMPORTS_PER_SOL)];
        assert_eq!(
            ReplayTrace::from_events(&events, DEFAULT_FEE_BPS),
            ReplayTrace::default()
        );
    }

    #[test]
    fn our_own_entries_and_exits_are_not_market_flow() {
        // A trace that treated our decisions as flow would price our own order
        // twice: once moving the curve, and once filling against it.
        let mut events = recording();
        let plain = ReplayTrace::from_events(&events, DEFAULT_FEE_BPS);
        events.push(LaunchEvent::Entry(crate::backtest::EntryEvent {
            mint: "MintTrace".to_string(),
            at_ms: 9_000,
            gross_lamports: LAMPORTS_PER_SOL,
            tag: None,
        }));
        events.push(LaunchEvent::Exit(crate::backtest::ExitEvent {
            mint: "MintTrace".to_string(),
            at_ms: 9_500,
            tokens: None,
            tag: None,
        }));
        assert_eq!(ReplayTrace::from_events(&events, DEFAULT_FEE_BPS), plain);
    }

    #[test]
    fn a_trace_comes_out_sorted_by_mint_whatever_order_the_recording_was_in() {
        let events = vec![
            launch_at("MintZulu", 1_000, 5 * LAMPORTS_PER_SOL),
            launch_at("MintAlpha", 1_100, 6 * LAMPORTS_PER_SOL),
            bought("MintZulu", 2_000, LAMPORTS_PER_SOL),
            launch_at("MintMike", 2_100, 7 * LAMPORTS_PER_SOL),
            bought("MintAlpha", 2_200, LAMPORTS_PER_SOL),
        ];
        let trace = ReplayTrace::from_events(&events, DEFAULT_FEE_BPS);
        let mints: Vec<&str> = trace.mints.iter().map(|m| m.mint.as_str()).collect();
        assert_eq!(mints, ["MintAlpha", "MintMike", "MintZulu"]);
        assert_eq!(trace.points(), 2 + 1 + 2);
    }

    #[test]
    fn a_stride_of_zero_is_one_round_trip_per_mint() {
        let trace = ReplayTrace::from_events(&recording(), DEFAULT_FEE_BPS);
        let rules = TraceRules::default();
        assert_eq!(rules.stride, 0);
        let plans = trace.round_trips(&rules);
        assert_eq!(plans.len(), 1);
        let points = &trace.mints[0].points;
        assert_eq!(plans[0].opened_at_ms, points[rules.first_entry_point].at_ms);
        assert_eq!(
            plans[0].closed_at_ms,
            points[rules.first_entry_point + rules.hold_points].at_ms
        );
        assert_eq!(plans[0].gross_lamports, rules.gross_lamports);
    }

    #[test]
    fn laddered_rules_take_every_round_trip_the_history_allows() {
        let trace = ReplayTrace::from_events(&recording(), DEFAULT_FEE_BPS);
        let rules = TraceRules::default().laddered(2);
        let plans = trace.round_trips(&rules);
        // Entries at 1, 3, 5, 7 with an exit four points later; the one at 9
        // would need a point 13 and the history has 13 of them, indices 0..12.
        assert_eq!(plans.len(), 4);
        for pair in plans.windows(2) {
            assert!(pair[1].opened_at_ms > pair[0].opened_at_ms);
        }
        // Every plan stays inside the history it came from.
        let last = trace.mints[0].points.last().expect("points").at_ms;
        assert!(plans.iter().all(|plan| plan.closed_at_ms <= last));
    }

    #[test]
    fn the_ticks_come_off_the_window_that_ended_at_the_leg() {
        let trace = ReplayTrace::from_events(&recording(), DEFAULT_FEE_BPS);
        let rules = TraceRules {
            first_entry_point: 6,
            tick_window: 3,
            ..TraceRules::default()
        };
        let plans = trace.round_trips(&rules);
        let plan = &plans[0];
        assert_eq!(plan.entry_ticks.len(), 3);
        assert_eq!(plan.exit_ticks.len(), 3);
        // The window ends at the leg, and the curve was rising, so the last
        // sample is the highest.
        assert_eq!(
            *plan.entry_ticks.last().expect("a sample"),
            curve_price_micros(&CurveState::at_real_sol(plan.entry_real_sol_lamports))
        );
        for pair in plan.entry_ticks.windows(2) {
            assert!(pair[1] > pair[0]);
        }
        // A window at the very start is short rather than padded: there is no
        // history before the first point, and inventing one would be inventing
        // a price.
        let early = TraceRules {
            first_entry_point: 0,
            tick_window: 8,
            ..rules
        };
        assert_eq!(trace.round_trips(&early)[0].entry_ticks.len(), 1);
        // And no window at all is no samples, which `MarketContext` reads as a
        // volatility of zero.
        let blind = TraceRules {
            tick_window: 0,
            ..rules
        };
        assert!(trace.round_trips(&blind)[0].entry_ticks.is_empty());
    }

    #[test]
    fn a_history_too_short_to_hold_a_round_trip_yields_none() {
        let events = vec![
            launch_at("MintShort", 1_000, 5 * LAMPORTS_PER_SOL),
            bought("MintShort", 2_000, LAMPORTS_PER_SOL),
        ];
        let trace = ReplayTrace::from_events(&events, DEFAULT_FEE_BPS);
        assert_eq!(trace.points(), 2);
        assert_eq!(trace.round_trips(&TraceRules::default()), Vec::new());
    }

    #[test]
    fn a_recording_attributes_to_the_same_bytes_twice() {
        // The end of the road: a recording in, every line of the identity and
        // the fee decomposition out, as a function of the recording and the
        // two configurations and nothing else.
        let trace = ReplayTrace::from_events(&recording(), DEFAULT_FEE_BPS);
        let rules = TraceRules::default().laddered(2);
        for profile in AdversaryProfile::ALL {
            let config = against(profile);
            let first = attribute_trace(&trace, &rules, &config);
            let second = attribute_trace(&trace, &rules, &config);
            assert_eq!(first, second, "{profile:?}");
            assert_eq!(first.to_json(), second.to_json(), "{profile:?}");
            assert!(first.balances(), "{profile:?}");
            assert!(first.fees.reconciles(&first.summary), "{profile:?}");
            assert!(
                first.trades.len() >= 4,
                "{profile:?}: {} trades",
                first.trades.len()
            );
            assert_eq!(first.schema, ATTRIBUTION_SCHEMA);
        }
    }

    #[test]
    fn attributing_a_recording_is_attributing_the_plans_it_reduces_to() {
        let trace = ReplayTrace::from_events(&recording(), DEFAULT_FEE_BPS);
        let rules = TraceRules::default().laddered(3);
        let config = against(AdversaryProfile::PredatorySandwich);
        assert_eq!(
            attribute_trace(&trace, &rules, &config),
            attribute_plans(&trace.round_trips(&rules), &config)
        );
    }

    #[test]
    fn a_trace_survives_the_wire() {
        let trace = ReplayTrace::from_events(&recording(), DEFAULT_FEE_BPS);
        let text = serde_json::to_string(&trace).expect("serialises");
        let back: ReplayTrace = serde_json::from_str(&text).expect("deserialises");
        assert_eq!(back, trace);
    }
}
