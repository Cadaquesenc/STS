//! # Flagged dead by the salvage audit — 2026-08-27
//!
//! **Nothing in the shipped application references this module.** The only
//! file in `src/` that reaches it is `attribution.rs`, which is itself
//! unreachable; beyond that pair, only `tests/replay_tests.rs` touches it.
//!
//! It is left here, compiling and tested, on purpose. Removing it is a
//! decision for a human to make in one reviewed commit, not a sweep. See
//! `docs/SALVAGE.md` for what that decision involves. The whole tree as it
//! stood before any salvage action is recoverable with
//! `git checkout pre-salvage-2026-08-27`.
//!
//! The header below is worth reading before deleting anything: it is the
//! project's best example of how to label an uncalibrated number so that
//! nobody can quote it without the flag attached.
//!
//! ---
//!
//! Synthetic MEV adversaries, for pricing what a fill would really have cost.
//!
//! `backtest.rs` already prices one thing this module is about: `assess_sandwich`
//! asks whether *somebody* could profitably front-run one of our buys, and
//! `AdverseSelectionSummary` reports the answer across a corpus. That number is
//! an exposure — what was on the table — and it is deliberately not applied to
//! the fill. The backtest's own trades execute against a clean curve.
//!
//! This module is the other half. It takes an adversary who *does* act, runs our
//! order through the curve they left behind, and reports how much worse we did.
//! `attribution.rs` then charges that difference to a line of its own, so a run
//! can say how much of a losing strategy was the strategy and how much was
//! everybody else's latency.
//!
//! # Nothing in here is a measurement
//!
//! Every number this module produces is arithmetic on a curve and a profile
//! somebody configured. STS does not sandwich anyone, and it has not observed
//! the adversaries modelled here doing it either — there is no mempool archive
//! behind these, no labelled attacker set, no landed-bundle sample. The
//! profiles are three shapes of adverse execution that are known to exist on
//! this venue, priced under assumptions written down beside them.
//! [`MevOutcome::synthetic`] is on every report for the same reason
//! `AdverseSelectionSummary::optimistic` is: the flag should be impossible to
//! quote the number without.
//!
//! # The three profiles
//!
//! [`AdversaryProfile::PassiveTaker`] does not act. It is the control: a run
//! against it prices fees, curve impact and tips and nothing else, and the
//! difference between it and the other two is the whole of what MEV cost.
//!
//! [`AdversaryProfile::PredatorySandwich`] is capital-bound. On our buy it is
//! the three-swap sandwich `replay::simulate_sandwich` already models, sized by
//! [`backtest::best_front_run_deterministic`] against the capital the profile
//! gives it. On our sell it dumps ahead of us — as much as that same capital
//! buys at the pre-trade price — and we fill into the hole.
//!
//! [`AdversaryProfile::HighFrequencyBackrunner`] is speed-bound rather than
//! capital-bound: it mirrors a share of whatever we do, and it gets there first.
//! **It costs us nothing on the entry**, and that is a modelling result rather
//! than an omission — a trade that lands *after* our buy does not change the
//! tokens our buy received. It shows up on the exit, where being second out of
//! a curve that only has so much real SOL in it is expensive.
//!
//! # Where the aggression comes from
//!
//! A profile is not a constant. Two things move it, and both are computed from
//! the fixture rather than assumed:
//!
//! **The curve transition.** A bonding curve near graduation is the one moment
//! on this venue where a buy is guaranteed to be followed by more buys, so it is
//! where the adversaries actually are. [`transition_pressure_micros`] is zero
//! for the first half of the curve and rises to one at the graduation line.
//!
//! **Recent volatility.** [`tick_volatility_micros`] is the mean absolute move
//! across a window of price samples. A quiet curve gets the profile's floor; a
//! curve that is moving several percent a tick gets the whole of its gain term.
//!
//! # The bound
//!
//! [`AdversaryConfig::max_penalty_bps`] is a ceiling on what this module will
//! claim, and it is enforced by making the adversary *smaller* rather than by
//! clipping the damage afterwards. A clipped number would be a fill nobody could
//! have got and a penalty that did not match it; a smaller adversary is a
//! coherent counterfactual, and the fill and the penalty stay two views of one
//! event. An adversary that cannot act at all inside the bound does not act, and
//! [`MevOutcome::bounded`] says so.
//!
//! # Determinism
//!
//! No floating point, no clock, no hash iteration, no randomness. Sizes come off
//! integer searches whose step counts are functions of their bounds — the same
//! ladder-and-bisection discipline `best_front_run_deterministic` uses, for the
//! same reason: a report that has to be byte-identical between two machines
//! cannot contain a number that came out of a convergence path.

use serde::{Deserialize, Serialize};

use crate::backtest::{
    best_front_run_deterministic, mul_div_ceil, mul_div_floor, sandwich_viable, Side, MICROS,
};
use crate::replay::{
    simulate_sandwich, CurveState, Fill, QuoteError, BPS_DENOMINATOR, LAMPORTS_PER_SOL,
    MIN_VIABLE_ATTACKER_LAMPORTS,
};

// ===========================================================================
// The knobs
// ===========================================================================

/// Where the curve starts being worth attacking, in basis points of progress.
///
/// Half way. Below it the pressure term is zero — not small, zero — because the
/// bottom half of a bonding curve is where the launch either fails or has not
/// been noticed yet, and a model that put a searcher on every dead mint would
/// charge the strategy for adversaries that were not there.
pub const TRANSITION_FLOOR_BPS: u16 = 5_000;

/// A move of this many millionths in one tick is as volatile as the blend gets.
///
/// Ten percent. Past it the volatility term is already saturated, so a curve
/// doing 10% a tick and one doing 60% get the same aggression: the difference
/// between them is not something this model can price, and pretending the
/// second is six times worse would be inventing a slope.
pub const VOLATILITY_SATURATION_MICROS: u64 = 100_000;

/// The default ceiling on modelled adverse execution, in basis points of the
/// leg's own notional.
///
/// Fifteen percent. Chosen as a reporting bound rather than an observation: it
/// is far above the damage the sandwich arithmetic produces at any size this
/// strategy trades, so it binds only when a configuration has asked for an
/// adversary the venue would not support — which is exactly when a backtest
/// should stop believing its own MEV line.
pub const DEFAULT_MAX_PENALTY_BPS: u16 = 1_500;

/// What a follower mirrors of our exit, in basis points, at full intensity.
///
/// A quarter. It is the one number here with no derivation behind it at all: no
/// sample of copy-trading bots on this venue exists in this repository, and this
/// is a share somebody picked. It is a config field rather than a constant in
/// the arithmetic so that a run which disagrees can say so in its report.
pub const DEFAULT_FOLLOW_SHARE_BPS: u16 = 2_500;

/// Which adversary is being modelled.
///
/// Ordered, so a summary keyed by profile has one iteration order on every
/// machine.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum AdversaryProfile {
    /// Nobody is in front of us. Fees and the curve, and nothing else.
    #[default]
    PassiveTaker,
    /// A searcher with capital, front-running our buy and dumping ahead of our
    /// sell.
    PredatorySandwich,
    /// A follower with speed and no capital to speak of, mirroring a share of
    /// our exit and getting there first.
    HighFrequencyBackrunner,
}

impl AdversaryProfile {
    pub const fn as_str(self) -> &'static str {
        match self {
            AdversaryProfile::PassiveTaker => "passive_taker",
            AdversaryProfile::PredatorySandwich => "predatory_sandwich",
            AdversaryProfile::HighFrequencyBackrunner => "high_frequency_backrunner",
        }
    }

    /// Whether this profile ever puts an order in front of ours.
    pub const fn hostile(self) -> bool {
        !matches!(self, AdversaryProfile::PassiveTaker)
    }

    /// Every profile, in one order, for a run that sweeps them.
    pub const ALL: [AdversaryProfile; 3] = [
        AdversaryProfile::PassiveTaker,
        AdversaryProfile::PredatorySandwich,
        AdversaryProfile::HighFrequencyBackrunner,
    ];
}

/// What the adversary is and how hard it is willing to work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdversaryConfig {
    pub profile: AdversaryProfile,
    /// The swap fee both sides pay, in basis points.
    pub fee_bps: u16,
    /// What it costs the adversary to land — signatures, priority fee, tip.
    /// Charged against their profit, never against our penalty: what they paid
    /// a validator is not a lamport that came out of our fill.
    pub landing_cost_lamports: u64,
    /// The most the adversary can deploy on one leg, at full intensity.
    pub capital_lamports: u64,
    /// What the adversary does on a quiet curve at the bottom of the range, in
    /// millionths. The floor of the blend.
    pub base_intensity_micros: u64,
    /// What a curve at the graduation line adds, in millionths.
    pub transition_gain_micros: u64,
    /// What a fully volatile window adds, in millionths.
    pub volatility_gain_micros: u64,
    /// What a follower mirrors of our exit, in basis points, before intensity.
    pub follow_share_bps: u16,
    /// The ceiling on what this model will claim, in basis points of the leg.
    pub max_penalty_bps: u16,
}

impl Default for AdversaryConfig {
    /// The control: nobody in front of us.
    ///
    /// A default that modelled an attacker would put adverse execution into
    /// every report that did not opt out, and the direction that errs is the one
    /// that makes a strategy look robust for reasons its author never chose.
    fn default() -> Self {
        AdversaryConfig {
            profile: AdversaryProfile::PassiveTaker,
            fee_bps: crate::replay::DEFAULT_FEE_BPS,
            landing_cost_lamports: 5_000_000,
            capital_lamports: LAMPORTS_PER_SOL,
            base_intensity_micros: 250_000,
            transition_gain_micros: 500_000,
            volatility_gain_micros: 250_000,
            follow_share_bps: DEFAULT_FOLLOW_SHARE_BPS,
            max_penalty_bps: DEFAULT_MAX_PENALTY_BPS,
        }
    }
}

impl AdversaryConfig {
    /// The same knobs pointed at a different adversary.
    pub const fn with_profile(mut self, profile: AdversaryProfile) -> Self {
        self.profile = profile;
        self
    }

    /// A different purse, and a different ceiling on the claim.
    pub const fn bounded(mut self, capital_lamports: u64, max_penalty_bps: u16) -> Self {
        self.capital_lamports = capital_lamports;
        self.max_penalty_bps = max_penalty_bps;
        self
    }

    /// The blend, for a moment described by `context`.
    ///
    /// `base + transition_gain × pressure + volatility_gain × volatility`, each
    /// term in millionths and the sum capped at one. Capped rather than
    /// saturating arithmetic on its own: an intensity above one would size an
    /// adversary above the capital the profile was given, which is not a more
    /// aggressive adversary, it is a different one.
    pub fn intensity_micros(&self, context: MarketContext) -> u64 {
        let transition = mul_div_floor(
            u128::from(self.transition_gain_micros),
            u128::from(transition_pressure_micros(context.progress_bps)),
            u128::from(MICROS),
        );
        let volatility = mul_div_floor(
            u128::from(self.volatility_gain_micros),
            u128::from(volatility_term_micros(context.volatility_micros)),
            u128::from(MICROS),
        );
        let blended = u128::from(self.base_intensity_micros)
            .saturating_add(transition)
            .saturating_add(volatility);
        blended.min(u128::from(MICROS)) as u64
    }

    /// The capital the adversary actually commits at this intensity.
    fn committed_lamports(&self, intensity_micros: u64) -> u64 {
        mul_div_floor(
            u128::from(self.capital_lamports),
            u128::from(intensity_micros),
            u128::from(MICROS),
        )
        .min(u128::from(u64::MAX)) as u64
    }
}

/// What the market looked like when the order went out.
///
/// Two numbers rather than the whole curve, because the curve is already a
/// parameter of every call here and these two are the things the curve does not
/// carry: where it is in its life, and what it has been doing lately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MarketContext {
    /// How far along the bonding curve is, in basis points. `CurveState` can
    /// supply it — see [`MarketContext::at`] — and a caller replaying a fixture
    /// that recorded it should pass the recorded one.
    pub progress_bps: u16,
    /// The mean absolute per-tick move over the recent window, in millionths.
    pub volatility_micros: u64,
}

impl MarketContext {
    /// The context a curve implies on its own, with no tick history.
    ///
    /// Volatility of zero, which is not a claim that the curve was quiet: it is
    /// the honest answer when nobody supplied a window, and it makes the
    /// adversary *less* aggressive, so a caller who forgets to pass ticks gets a
    /// cheaper MEV line rather than a more expensive one. That is the direction
    /// that shows up as a strategy underperforming its backtest, which is the
    /// failure mode somebody notices.
    pub const fn at(curve: &CurveState) -> Self {
        MarketContext {
            progress_bps: curve.progress_bps(),
            volatility_micros: 0,
        }
    }

    /// The same, with the volatility of a window of price samples.
    pub fn with_ticks(curve: &CurveState, samples: &[u64]) -> Self {
        MarketContext {
            progress_bps: curve.progress_bps(),
            volatility_micros: tick_volatility_micros(samples),
        }
    }
}

// ===========================================================================
// The two things that move a profile
// ===========================================================================

/// How much the coming transition is worth to somebody in front of us, in
/// millionths.
///
/// Zero below [`TRANSITION_FLOOR_BPS`], then linear to one at the graduation
/// line. Linear rather than a curve because there is nothing in this repository
/// to fit a curve to: the shape is an assumption, and the simplest assumption is
/// the one whose consequences are easiest to argue with.
pub fn transition_pressure_micros(progress_bps: u16) -> u64 {
    let progress = u32::from(progress_bps).min(BPS_DENOMINATOR);
    let floor = u32::from(TRANSITION_FLOOR_BPS).min(BPS_DENOMINATOR);
    if progress <= floor {
        return 0;
    }
    let span = BPS_DENOMINATOR - floor;
    if span == 0 {
        return MICROS;
    }
    mul_div_floor(
        u128::from(progress - floor),
        u128::from(MICROS),
        u128::from(span),
    )
    .min(u128::from(MICROS)) as u64
}

/// The price a curve is quoting, in micro-lamports per billion base units.
///
/// The unit is arbitrary and it cancels: [`tick_volatility_micros`] only ever
/// takes ratios of these. What matters is that it has ten significant digits at
/// launch reserves, so two adjacent ticks on a quiet curve are distinguishable
/// rather than both floored to the same integer.
pub fn curve_price_micros(curve: &CurveState) -> u64 {
    if curve.virtual_token_reserves == 0 {
        return 0;
    }
    mul_div_floor(
        u128::from(curve.virtual_sol_reserves),
        u128::from(MICROS) * u128::from(LAMPORTS_PER_SOL),
        u128::from(curve.virtual_token_reserves),
    )
    .min(u128::from(u64::MAX)) as u64
}

/// Mean absolute per-tick move across a window, in millionths.
///
/// The mean of `|p_i - p_{i-1}| / p_{i-1}`, each term floored, then the mean
/// floored. Absolute rather than signed, and per tick rather than annualised: a
/// window of nine samples over four seconds has no annualisation factor, and
/// inventing one would be reporting a number about a trading calendar this
/// fixture does not have. It is the same refusal `sharpe_micros` makes.
///
/// Zero for fewer than two samples, and for a window that did not move. A
/// sample of zero is skipped rather than divided by — a curve quoting nothing
/// has no return, and the alternative is a term that means "infinity" sitting in
/// a mean.
///
/// Capped at one, at which point [`VOLATILITY_SATURATION_MICROS`] has long since
/// saturated the term this feeds.
pub fn tick_volatility_micros(samples: &[u64]) -> u64 {
    if samples.len() < 2 {
        return 0;
    }
    let mut accumulator: u128 = 0;
    let mut counted: u128 = 0;
    for window in samples.windows(2) {
        let (previous, current) = (window[0], window[1]);
        if previous == 0 {
            continue;
        }
        let move_abs = u128::from(current.abs_diff(previous));
        accumulator = accumulator.saturating_add(mul_div_floor(
            move_abs,
            u128::from(MICROS),
            u128::from(previous),
        ));
        counted += 1;
    }
    if counted == 0 {
        return 0;
    }
    (accumulator / counted).min(u128::from(MICROS)) as u64
}

/// The volatility term as the blend sees it, in millionths.
///
/// Scaled so that [`VOLATILITY_SATURATION_MICROS`] and everything above it is
/// one. Separate from [`tick_volatility_micros`] because the raw number is worth
/// reporting unscaled — a report that only carried the saturated version could
/// not tell a curve at the saturation point from one at six times it.
pub fn volatility_term_micros(volatility_micros: u64) -> u64 {
    if VOLATILITY_SATURATION_MICROS == 0 {
        return MICROS;
    }
    mul_div_floor(
        u128::from(volatility_micros),
        u128::from(MICROS),
        u128::from(VOLATILITY_SATURATION_MICROS),
    )
    .min(u128::from(MICROS)) as u64
}

// ===========================================================================
// What one leg came out as
// ===========================================================================

/// One of our fills, with whoever was in front of it.
///
/// Field meanings differ by side, because a buy and a sell are quoted from
/// opposite ends:
///
/// * **Buy.** `notional_lamports` is what we committed. `solo_tokens` is what
///   the curve would have given us alone and `filled_tokens` is what we got.
///   `solo_gross_lamports` and `filled_gross_lamports` are both our own gross —
///   a front-run does not change what we spend, only what it buys.
/// * **Sell.** `notional_lamports` is the gross the curve would have paid us
///   alone. `solo_tokens` and `filled_tokens` are both the parcel we sold — a
///   dump ahead of us does not change how many tokens we hand over, only what
///   they fetch — and the two gross figures are what the curve would have paid
///   and what it did.
///
/// `penalty_lamports` is the difference either way, in lamports, and it is the
/// number [`crate::attribution`] charges to the MEV line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MevOutcome {
    pub profile: AdversaryProfile,
    pub side: Side,
    /// The blend that sized this adversary, in millionths.
    pub intensity_micros: u64,
    /// Lamports the adversary put in front of our buy. Zero on a sell.
    pub attacker_lamports: u64,
    /// Tokens the adversary dumped in front of our sell. Zero on a buy.
    pub attacker_tokens: u64,
    pub notional_lamports: u64,
    pub solo_tokens: u64,
    pub filled_tokens: u64,
    pub solo_gross_lamports: u64,
    pub filled_gross_lamports: u64,
    /// The protocol fee on our own leg, at the size it actually filled.
    pub fee_lamports: u64,
    /// What entered the curve (buy) or reached us (sell), after that fee.
    pub net_lamports: u64,
    /// What the adversary cost us. On a buy, the tokens they displaced valued at
    /// the pre-trade price; on a sell, the gross the curve stopped paying.
    pub penalty_lamports: u64,
    /// The same, in basis points of `notional_lamports`, rounded up.
    pub penalty_bps: u16,
    /// The realised slippage on our own leg, in basis points.
    pub slippage_bps: u16,
    /// Whether [`AdversaryConfig::max_penalty_bps`] cut the adversary back —
    /// including cutting it back to nothing.
    pub bounded: bool,
    /// The adversary's own profit, where the model has one. `None` on the sell
    /// side, where the adversary is selling inventory this simulation never
    /// gave it: a profit figure there would be a claim about a book nobody
    /// looked at.
    pub attacker_profit_lamports: Option<i64>,
    /// Always true, and here so the number cannot be quoted without it.
    pub synthetic: bool,
}

impl MevOutcome {
    /// The fill nobody interfered with.
    ///
    /// `solo` is that fill. Taken whole rather than as the five numbers on it:
    /// they were passed positionally and four of them are lamport counts, so a
    /// caller that swapped the gross for the net would have compiled and would
    /// have reported an untouched fill at the wrong price. `notional_lamports`
    /// stays separate because it is the leg the penalty is measured against and
    /// the two sides read it off different places — the requested size on a
    /// buy, the proceeds on a sell.
    fn clean(
        profile: AdversaryProfile,
        side: Side,
        intensity_micros: u64,
        notional_lamports: u64,
        solo: &Fill,
        bounded: bool,
    ) -> Self {
        MevOutcome {
            profile,
            side,
            intensity_micros,
            attacker_lamports: 0,
            attacker_tokens: 0,
            notional_lamports,
            solo_tokens: solo.tokens,
            filled_tokens: solo.tokens,
            solo_gross_lamports: solo.gross_lamports,
            filled_gross_lamports: solo.gross_lamports,
            fee_lamports: solo.fee_lamports,
            net_lamports: solo.net_lamports,
            penalty_lamports: 0,
            penalty_bps: 0,
            slippage_bps: solo.slippage_bps,
            bounded,
            attacker_profit_lamports: None,
            synthetic: true,
        }
    }

    /// Whether anybody actually got in front of this fill.
    pub const fn attacked(&self) -> bool {
        self.attacker_lamports > 0 || self.attacker_tokens > 0
    }
}

/// A penalty as a share of the leg it was taken out of, rounded up.
///
/// Up, for the reason `replay::slippage_bps` rounds up: a simulator that
/// under-reports its own costs flatters every backtest built on it.
fn penalty_bps(penalty_lamports: u64, notional_lamports: u64) -> u16 {
    if notional_lamports == 0 {
        return 0;
    }
    mul_div_ceil(
        u128::from(penalty_lamports),
        u128::from(BPS_DENOMINATOR),
        u128::from(notional_lamports),
    )
    .min(u128::from(BPS_DENOMINATOR)) as u16
}

/// The largest size in `low..=high` that `admits`, or `None` if not even `low`
/// does.
///
/// Bisection on a predicate the caller promises is downward-closed: if a size
/// stays inside the bound then every smaller one does. That holds for both uses
/// here, because damage rises with the adversary's size, and the integer floors
/// in the swaps flatten it into plateaus rather than breaking the ordering.
///
/// The step count is a function of the bracket alone, so two runs take the same
/// steps in the same order — the property the whole module is written for.
fn largest_admissible(low: u64, high: u64, admits: impl Fn(u64) -> bool) -> Option<u64> {
    if high < low || !admits(low) {
        return None;
    }
    let (mut lo, mut hi) = (low, high);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if admits(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    Some(lo)
}

// ===========================================================================
// Our buy, with somebody in front of it
// ===========================================================================

/// Runs a buy of `gross_lamports` through `curve` with the configured adversary
/// in front of it.
///
/// The error is the curve's, not the adversary's: a quote this curve refuses for
/// our own order refuses it here too, and the caller handles it the same way a
/// clean backtest would. An adversary who cannot act — no capital, nothing
/// profitable to do, nothing the bound allows — is not an error, it is a clean
/// fill with `bounded` saying why.
pub fn buy_through(
    curve: &CurveState,
    gross_lamports: u64,
    config: &AdversaryConfig,
    context: MarketContext,
) -> Result<MevOutcome, QuoteError> {
    let solo = curve.quote_buy(gross_lamports, config.fee_bps)?;
    let intensity = config.intensity_micros(context);

    let clean = |bounded: bool| {
        MevOutcome::clean(
            config.profile,
            Side::Buy,
            intensity,
            gross_lamports,
            &solo,
            bounded,
        )
    };

    // Two of the three profiles do nothing to a buy, and for different reasons.
    // The passive taker does not act at all; the backrunner acts *after* us, and
    // an order that lands after ours cannot change what ours received. Neither
    // is an approximation.
    if config.profile != AdversaryProfile::PredatorySandwich {
        return Ok(clean(false));
    }

    let committed = config.committed_lamports(intensity);
    if committed < MIN_VIABLE_ATTACKER_LAMPORTS {
        return Ok(clean(false));
    }

    // Below the threshold no front-run of any size clears its own fees, and the
    // search would come back with the one-lamport residues §15.2 warns about.
    if !sandwich_viable(gross_lamports, curve.virtual_sol_reserves, config.fee_bps) {
        return Ok(clean(false));
    }

    let Some((best_size, _)) = best_front_run_deterministic(
        curve,
        gross_lamports,
        config.fee_bps,
        config.landing_cost_lamports,
        committed,
    ) else {
        return Ok(clean(false));
    };

    // What a front-run of `size` costs us. `front_run_cost` prices the
    // displacement at the pre-trade mid rather than at our own fill, because
    // the question this answers is what the displaced tokens were worth and our
    // fill price is itself a function of the displacement.
    let cost_of = |size: u64| front_run_cost(curve, size, gross_lamports, config);

    let within_bound = |size: u64| -> bool {
        cost_of(size).is_some_and(|cost| cost.penalty_bps <= config.max_penalty_bps)
    };

    // The attacker's own optimum first. The bound only bites when the profile
    // has been given an adversary this venue would not support, and cutting one
    // back that was already inside it would report a fill nobody would have got.
    let (size, bounded) = if within_bound(best_size) {
        (best_size, false)
    } else {
        match largest_admissible(MIN_VIABLE_ATTACKER_LAMPORTS, best_size, within_bound) {
            Some(size) => (size, true),
            // Not even the smallest front-run worth modelling stays inside the
            // bound. The honest report is that this model has nothing to say
            // about this fill, not that the fill was clean — `bounded` carries
            // the difference.
            None => return Ok(clean(true)),
        }
    };

    let Some(cost) = cost_of(size) else {
        return Ok(clean(true));
    };

    Ok(MevOutcome {
        profile: config.profile,
        side: Side::Buy,
        intensity_micros: intensity,
        attacker_lamports: size,
        attacker_tokens: 0,
        notional_lamports: gross_lamports,
        solo_tokens: solo.tokens,
        filled_tokens: cost.victim_tokens,
        solo_gross_lamports: solo.gross_lamports,
        filled_gross_lamports: solo.gross_lamports,
        fee_lamports: solo.fee_lamports,
        net_lamports: solo.net_lamports,
        penalty_lamports: cost.penalty_lamports,
        penalty_bps: cost.penalty_bps,
        slippage_bps: solo.slippage_bps,
        bounded,
        attacker_profit_lamports: Some(cost.attacker_profit_lamports),
        synthetic: true,
    })
}

// ===========================================================================
// Our sell, with somebody in front of it
// ===========================================================================

/// How many tokens the profile puts in front of our sell, before the bound.
///
/// The two hostile profiles are sized by different things, and that is the whole
/// behavioural difference between them:
///
/// * The **sandwich** is capital-bound. It dumps what its purse is worth at the
///   pre-trade price, which on a thin curve is a large parcel and on a deep one
///   is not.
/// * The **backrunner** is speed-bound. It has no purse worth naming; what it
///   has is our order flow, and it mirrors a share of it.
fn tokens_ahead(
    curve: &CurveState,
    our_tokens: u64,
    config: &AdversaryConfig,
    intensity: u64,
) -> u64 {
    match config.profile {
        AdversaryProfile::PassiveTaker => 0,
        AdversaryProfile::PredatorySandwich => {
            let committed = config.committed_lamports(intensity);
            mul_div_floor(
                u128::from(committed),
                u128::from(curve.virtual_token_reserves),
                u128::from(curve.virtual_sol_reserves),
            )
            .min(u128::from(u64::MAX)) as u64
        }
        AdversaryProfile::HighFrequencyBackrunner => mul_div_floor(
            u128::from(our_tokens),
            u128::from(config.follow_share_bps) * u128::from(intensity),
            u128::from(BPS_DENOMINATOR) * u128::from(MICROS),
        )
        .min(u128::from(u64::MAX)) as u64,
    }
}

/// Runs a sell of `tokens` through `curve` with the configured adversary in
/// front of it.
///
/// The adversary's sell has to be a quote the curve would honour and so does
/// ours after it — a dump that drains the real SOL reserve leaves us with no
/// executable exit, which is a real outcome but not one this function reports as
/// a penalty. The bisection requires both quotes to succeed, so what comes back
/// is always a fill we could have got.
pub fn sell_through(
    curve: &CurveState,
    tokens: u64,
    config: &AdversaryConfig,
    context: MarketContext,
) -> Result<MevOutcome, QuoteError> {
    let solo = curve.quote_sell(tokens, config.fee_bps)?;
    let intensity = config.intensity_micros(context);

    let clean = |bounded: bool| {
        MevOutcome::clean(
            config.profile,
            Side::Sell,
            intensity,
            solo.gross_lamports,
            &solo,
            bounded,
        )
    };

    if !config.profile.hostile() {
        return Ok(clean(false));
    }

    let wanted = tokens_ahead(curve, tokens, config, intensity);
    if wanted == 0 {
        return Ok(clean(false));
    }

    // What a dump of `ahead` tokens does to our fill. `None` when either leg is
    // a quote this curve would refuse.
    let fill_after = |ahead: u64| -> Option<(crate::replay::Fill, u64, u64)> {
        let theirs = curve.quote_sell(ahead, config.fee_bps).ok()?;
        let after = curve.after_sell(&theirs);
        let ours = after.quote_sell(tokens, config.fee_bps).ok()?;
        let penalty = solo.gross_lamports.saturating_sub(ours.gross_lamports);
        Some((
            ours,
            penalty,
            u64::from(penalty_bps(penalty, solo.gross_lamports)),
        ))
    };

    let within_bound = |ahead: u64| -> bool {
        fill_after(ahead).is_some_and(|(_, _, bps)| bps <= u64::from(config.max_penalty_bps))
    };

    let (ahead, bounded) = if within_bound(wanted) {
        (wanted, false)
    } else {
        match largest_admissible(1, wanted, within_bound) {
            Some(ahead) => (ahead, true),
            None => return Ok(clean(true)),
        }
    };

    let Some((ours, penalty, bps)) = fill_after(ahead) else {
        return Ok(clean(true));
    };

    Ok(MevOutcome {
        profile: config.profile,
        side: Side::Sell,
        intensity_micros: intensity,
        attacker_lamports: 0,
        attacker_tokens: ahead,
        notional_lamports: solo.gross_lamports,
        solo_tokens: tokens,
        filled_tokens: tokens,
        solo_gross_lamports: solo.gross_lamports,
        filled_gross_lamports: ours.gross_lamports,
        fee_lamports: ours.fee_lamports,
        net_lamports: ours.net_lamports,
        penalty_lamports: penalty,
        penalty_bps: bps.min(u64::from(BPS_DENOMINATOR as u16)) as u16,
        slippage_bps: ours.slippage_bps,
        bounded,
        attacker_profit_lamports: None,
        synthetic: true,
    })
}

// ===========================================================================
// Across a run
// ===========================================================================

/// What the adversary did across every leg of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MevSummary {
    pub profile: AdversaryProfile,
    pub legs_modelled: u32,
    /// Legs somebody actually got in front of.
    pub legs_attacked: u32,
    /// Legs where the ceiling cut the adversary back, including to nothing.
    pub legs_bounded: u32,
    pub entry_penalty_lamports: u64,
    pub exit_penalty_lamports: u64,
    pub total_penalty_lamports: u64,
    pub worst_penalty_bps: u16,
    /// Size-weighted rather than per-leg: a dust exit and a one-SOL entry are
    /// not two equal observations of what this strategy pays to be second.
    pub mean_penalty_bps: u16,
    /// The ceiling the run was computed under, echoed so the bound travels with
    /// the number it bounds.
    pub max_penalty_bps: u16,
    pub max_intensity_micros: u64,
    /// The adversaries' own profit where the model has one. Their landing costs
    /// are already netted out of it, and none of it is a lamport that came out
    /// of our fill — see `MevOutcome::penalty_lamports` for that.
    pub attacker_profit_lamports: i64,
    /// Always true. Nothing in this struct was observed.
    pub synthetic: bool,
}

impl MevSummary {
    /// An empty book under one profile.
    pub const fn empty(profile: AdversaryProfile, max_penalty_bps: u16) -> Self {
        MevSummary {
            profile,
            legs_modelled: 0,
            legs_attacked: 0,
            legs_bounded: 0,
            entry_penalty_lamports: 0,
            exit_penalty_lamports: 0,
            total_penalty_lamports: 0,
            worst_penalty_bps: 0,
            mean_penalty_bps: 0,
            max_penalty_bps,
            max_intensity_micros: 0,
            attacker_profit_lamports: 0,
            synthetic: true,
        }
    }

    /// Folds every leg of a run into one row.
    ///
    /// The profile reported is the configured one, not one inferred from the
    /// legs: a run where the adversary never found anything worth doing is a run
    /// under that adversary, and reporting it as passive would lose the
    /// difference between "nobody attacked" and "nobody was there".
    pub fn of(profile: AdversaryProfile, max_penalty_bps: u16, legs: &[MevOutcome]) -> Self {
        let mut summary = MevSummary::empty(profile, max_penalty_bps);
        let mut weighted: u128 = 0;
        let mut notional: u128 = 0;
        let mut profit: i128 = 0;

        for leg in legs {
            summary.legs_modelled += 1;
            if leg.attacked() {
                summary.legs_attacked += 1;
            }
            if leg.bounded {
                summary.legs_bounded += 1;
            }
            match leg.side {
                Side::Buy => {
                    summary.entry_penalty_lamports = summary
                        .entry_penalty_lamports
                        .saturating_add(leg.penalty_lamports)
                }
                Side::Sell => {
                    summary.exit_penalty_lamports = summary
                        .exit_penalty_lamports
                        .saturating_add(leg.penalty_lamports)
                }
            }
            summary.worst_penalty_bps = summary.worst_penalty_bps.max(leg.penalty_bps);
            summary.max_intensity_micros = summary.max_intensity_micros.max(leg.intensity_micros);
            weighted = weighted.saturating_add(u128::from(leg.penalty_lamports));
            notional = notional.saturating_add(u128::from(leg.notional_lamports));
            profit = profit.saturating_add(i128::from(leg.attacker_profit_lamports.unwrap_or(0)));
        }

        summary.total_penalty_lamports = summary
            .entry_penalty_lamports
            .saturating_add(summary.exit_penalty_lamports);
        summary.mean_penalty_bps = if notional == 0 {
            0
        } else {
            mul_div_ceil(weighted, u128::from(BPS_DENOMINATOR), notional)
                .min(u128::from(BPS_DENOMINATOR)) as u16
        };
        summary.attacker_profit_lamports =
            profit.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
        summary
    }
}

// ===========================================================================
// What a front-run does to the order behind it
// ===========================================================================

/// One front-run, priced from both sides at once.
///
/// The victim's side and the attacker's side of the same three swaps. A caller
/// that needs both gets them from one simulation, which is not an optimisation:
/// two calls at two sizes would let a report quote a penalty taken from one
/// adversary beside a profit earned by a different one, and there would be
/// nothing in the numbers to say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontRunCost {
    /// What the attacker put in front, in lamports.
    pub attacker_lamports: u64,
    /// What the order behind it received.
    pub victim_tokens: u64,
    /// What that order would have received alone.
    pub victim_tokens_solo: u64,
    /// The displaced tokens, valued at the pre-trade marginal price.
    ///
    /// This is the number [`MevOutcome::penalty_lamports`] carries. Priced at
    /// the mid rather than at the victim's own fill, because the question it
    /// answers is what the displaced tokens were worth and the fill price is
    /// itself a function of the displacement.
    pub penalty_lamports: u64,
    /// The same, in basis points of the victim's gross, rounded up.
    pub penalty_bps: u16,
    /// The same loss in the unit [`crate::replay::Sandwich`] reports it in:
    /// basis points of the tokens the victim would have had. Both are here
    /// because they answer different questions — one is what it cost, the
    /// other is how much of the parcel went missing — and a report that
    /// carried only one would invite the other to be derived wrongly.
    pub damage_bps: u16,
    /// The attacker's own profit, net of their landing cost. Never a lamport
    /// that came out of the victim's fill.
    pub attacker_profit_lamports: i64,
    /// Gross extraction, before the attacker's own fees.
    pub extraction_lamports: i64,
}

/// Prices a front-run of `attacker_lamports` in front of a buy of
/// `victim_gross`.
///
/// `None` when any of the three swaps is a quote this curve would refuse, which
/// is the answer the bisections in this module want: a size whose legs do not
/// all execute is not a smaller adversary, it is not an adversary.
pub fn front_run_cost(
    curve: &CurveState,
    attacker_lamports: u64,
    victim_gross: u64,
    config: &AdversaryConfig,
) -> Option<FrontRunCost> {
    let sandwich = simulate_sandwich(
        curve,
        attacker_lamports,
        victim_gross,
        config.fee_bps,
        config.landing_cost_lamports,
    )
    .ok()?;
    let displaced = sandwich
        .victim_tokens_solo
        .saturating_sub(sandwich.victim_tokens);
    let penalty = mul_div_floor(
        u128::from(displaced),
        u128::from(curve.virtual_sol_reserves),
        u128::from(curve.virtual_token_reserves),
    )
    .min(u128::from(u64::MAX)) as u64;
    Some(FrontRunCost {
        attacker_lamports,
        victim_tokens: sandwich.victim_tokens,
        victim_tokens_solo: sandwich.victim_tokens_solo,
        penalty_lamports: penalty,
        penalty_bps: penalty_bps(penalty, victim_gross),
        damage_bps: sandwich.victim_damage_bps,
        attacker_profit_lamports: sandwich.attacker_profit_lamports,
        extraction_lamports: sandwich.extraction_lamports,
    })
}

/// What `tokens` are worth at a curve's marginal price, in lamports.
///
/// Floored, and the floor is the direction that under-values a holding — which
/// is the pessimistic direction for anybody holding one.
pub fn tokens_at_marginal(curve: &CurveState, tokens: u64) -> u64 {
    if curve.virtual_token_reserves == 0 {
        return 0;
    }
    mul_div_floor(
        u128::from(tokens),
        u128::from(curve.virtual_sol_reserves),
        u128::from(curve.virtual_token_reserves),
    )
    .min(u128::from(u64::MAX)) as u64
}

// ===========================================================================
// Re-orgs
// ===========================================================================

/// The deepest fork the grids below will build a scenario for, in slots.
///
/// Past it a fork stops being a leader losing a race and becomes a consensus
/// failure, which is not a thing a tip policy or a slippage bound can be tuned
/// against — so a scenario there would be pricing an event whose answer is
/// "stop trading", and this module has nothing useful to say about it.
pub const MAX_REORG_DEPTH_SLOTS: u32 = 64;

/// What became of our leg on the branch that won.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum ReorgFate {
    /// No fork reached our slot. The branch we priced against is the branch
    /// that won, and this is the control the other three are read against: a
    /// sweep whose untouched rows are not all exactly zero has a bug in it
    /// rather than a finding in it.
    #[default]
    Untouched,
    /// Our leg was re-included, against a curve the winning branch's own
    /// replayed flow had already moved.
    Reincluded,
    /// Our leg was not in the winning branch at all. Nothing was spent, nothing
    /// was received, and the tip was not paid — which is why a dropped leg is
    /// not simply a worse fill and has to be a fate of its own.
    Dropped,
    /// Our leg was re-included and the curve refused it. The same book as
    /// [`ReorgFate::Dropped`] and a different fact: the branch had our
    /// transaction and could not execute it, which is a liquidity problem
    /// rather than an inclusion one.
    Refused,
}

impl ReorgFate {
    pub const fn as_str(self) -> &'static str {
        match self {
            ReorgFate::Untouched => "untouched",
            ReorgFate::Reincluded => "reincluded",
            ReorgFate::Dropped => "dropped",
            ReorgFate::Refused => "refused",
        }
    }

    /// Whether our leg executed at all on the branch that won.
    pub const fn filled(self) -> bool {
        matches!(self, ReorgFate::Untouched | ReorgFate::Reincluded)
    }

    pub const ALL: [ReorgFate; 4] = [
        ReorgFate::Untouched,
        ReorgFate::Reincluded,
        ReorgFate::Dropped,
        ReorgFate::Refused,
    ];
}

/// One fork, and what our order was doing when it landed.
///
/// `Copy` and free of any heap, deliberately: a sweep is millions of these and
/// the whole design of the fold below is that a scenario can be rebuilt from
/// two integers rather than held.
///
/// # What the depth does and does not do
///
/// `depth_slots` is a label. The economics of a fork are entirely in the two
/// flow figures — what the branch we priced against had applied, and what the
/// branch that won applied instead — and a depth that did not change those
/// would be a fork that changed nothing. It is carried so a summary can be cut
/// by depth, and [`ReorgGrid`] derives the replacement flow *from* it, which is
/// where the two are tied together.
/// No `Default`, deliberately: a scenario has a side, and `Side` has no default
/// because a buy and a sell are not variations on one thing. Start from
/// [`ReorgScenario::untouched`] instead, which names the side it is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReorgScenario {
    /// How many slots the winning fork rolled back. Zero is the control.
    pub depth_slots: u32,
    /// Real SOL in the pool at the common ancestor — the last state both
    /// branches agree on.
    pub ancestor_real_sol_lamports: u64,
    /// Fee-adjusted net flow the branch we priced against had applied on top of
    /// the ancestor before our leg. Positive is net buying.
    pub canonical_flow_lamports: i64,
    /// The same on the branch that won.
    pub replacement_flow_lamports: i64,
    pub side: Side,
    /// Gross lamports for a buy, token base units for a sell.
    pub size: u64,
    /// Whether the winning branch re-included our leg. A fork that rolled back
    /// our slot does not have to re-broadcast what was in it.
    pub reincluded: bool,
    /// What the bundle carrying our leg bid. Charged on a branch our leg is in
    /// and refunded on one it is not — a bid that never landed never left the
    /// wallet.
    pub tip_lamports: u64,
    /// The volatility both branches are priced under, in millionths. One
    /// number rather than two, because a fork does not change what the curve
    /// had been doing before the ancestor.
    pub volatility_micros: u64,
}

impl ReorgScenario {
    /// The control: our leg, on a chain nobody forked.
    pub const fn untouched(
        ancestor_real_sol_lamports: u64,
        side: Side,
        size: u64,
        tip_lamports: u64,
    ) -> Self {
        ReorgScenario {
            depth_slots: 0,
            ancestor_real_sol_lamports,
            canonical_flow_lamports: 0,
            replacement_flow_lamports: 0,
            side,
            size,
            reincluded: true,
            tip_lamports,
            volatility_micros: 0,
        }
    }

    /// The same order, on a fork of `depth_slots` that replayed
    /// `replacement_flow_lamports` instead.
    pub const fn forked(
        mut self,
        depth_slots: u32,
        replacement_flow_lamports: i64,
        reincluded: bool,
    ) -> Self {
        self.depth_slots = depth_slots;
        self.replacement_flow_lamports = replacement_flow_lamports;
        self.reincluded = reincluded;
        self
    }

    /// Whether a fork reached this slot at all.
    pub const fn forked_at_all(&self) -> bool {
        self.depth_slots > 0
    }
}

/// One scenario, priced on both branches.
///
/// Every book column is what the position would liquidate for **on the
/// ancestor curve**, and that choice is the whole of what makes the two
/// branches comparable: the ancestor is the one state both of them agree on.
/// Valuing the token side at either branch's own post-trade price would make
/// the comparison a function of the thing being compared, and a deep fork would
/// look expensive for the arithmetic reason that it moved the price it was
/// being measured with.
///
/// # What a dropped entry reads as, and why
///
/// This module does not forecast. A dropped *buy* therefore books at exactly
/// zero, while the buy that landed books at minus its round-trip cost — so a
/// fork that dropped an entry comes back **favourable**, by about `2φ` plus
/// impact plus the tip.
///
/// That is arithmetic rather than advice, and it is the honest answer to the
/// question this struct asks: of the fill we thought we had, what did the fork
/// change? It is emphatically *not* the claim that losing entries is good. The
/// edge the entry was for is the one quantity here that would need a forecast,
/// and inventing one is how a backtest starts explaining its losses away.
/// [`crate::attribution`] is where an entry is judged against what it went on
/// to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReorgOutcome {
    pub scenario: ReorgScenario,
    pub fate: ReorgFate,
    pub profile: AdversaryProfile,
    /// The ancestor's price, in the unit [`curve_price_micros`] produces.
    pub ancestor_price_micros: u64,
    pub canonical_tokens: u64,
    pub canonical_net_lamports: u64,
    pub canonical_penalty_lamports: u64,
    pub canonical_slippage_bps: u16,
    /// What our book was worth after the leg on the branch we priced against,
    /// in lamports at the ancestor's price and net of the tip.
    pub canonical_book_lamports: i64,
    pub reorged_tokens: u64,
    pub reorged_net_lamports: u64,
    pub reorged_penalty_lamports: u64,
    pub reorged_slippage_bps: u16,
    /// The same on the branch that won.
    pub reorged_book_lamports: i64,
    /// `reorged_book − canonical_book`. Negative is what the fork cost us.
    pub book_delta_lamports: i64,
    /// The bid the bundle made and never paid.
    pub tip_refunded_lamports: u64,
    /// Whether the branch we priced against could be quoted at all. A scenario
    /// with no baseline has nothing to say, and every column above is zero
    /// rather than large — the same refusal the bound in this module makes.
    pub priced: bool,
    /// Always true. Nothing here was observed.
    pub synthetic: bool,
}

impl ReorgOutcome {
    /// A scenario whose own baseline is not a quote this curve would honour.
    fn unpriced(scenario: ReorgScenario, ancestor: &CurveState, profile: AdversaryProfile) -> Self {
        ReorgOutcome {
            scenario,
            fate: ReorgFate::Refused,
            profile,
            ancestor_price_micros: curve_price_micros(ancestor),
            canonical_tokens: 0,
            canonical_net_lamports: 0,
            canonical_penalty_lamports: 0,
            canonical_slippage_bps: 0,
            canonical_book_lamports: 0,
            reorged_tokens: 0,
            reorged_net_lamports: 0,
            reorged_penalty_lamports: 0,
            reorged_slippage_bps: 0,
            reorged_book_lamports: 0,
            book_delta_lamports: 0,
            tip_refunded_lamports: 0,
            priced: false,
            synthetic: true,
        }
    }

    /// Whether the fork left us worse off than the branch we priced against.
    pub const fn adverse(&self) -> bool {
        self.book_delta_lamports < 0
    }
}

/// Runs our leg through one branch's curve.
fn leg_through(
    curve: &CurveState,
    scenario: &ReorgScenario,
    config: &AdversaryConfig,
) -> Result<MevOutcome, QuoteError> {
    let context = MarketContext {
        progress_bps: curve.progress_bps(),
        volatility_micros: scenario.volatility_micros,
    };
    match scenario.side {
        Side::Buy => buy_through(curve, scenario.size, config, context),
        Side::Sell => sell_through(curve, scenario.size, config, context),
    }
}

/// What a parcel would fetch on the ancestor curve, net of the venue's fee.
///
/// The yardstick every book below is measured with, and the choice is
/// load-bearing. **Executable rather than marginal**: the mid over-values a
/// parcel by exactly the impact of selling it, so a book marked at the mid
/// would make every leg that *did not happen* look better than one that did,
/// and a sweep whose headline was "forks save us money" would be an artifact of
/// the yardstick rather than a finding about forks.
///
/// A parcel the ancestor cannot pay for at all falls back to the mid capped at
/// the pool's real SOL, which is the most that curve could ever hand over. That
/// is an upper bound rather than a fill, and it is reached only where the exit
/// was not executable in the first place — which is a fact about the pool, not
/// about the fork, and `replay::QuoteError::ExceedsRealSol` is where it belongs.
fn liquidation_lamports(ancestor: &CurveState, tokens: u64, fee_bps: u16) -> u64 {
    if tokens == 0 {
        return 0;
    }
    match ancestor.quote_sell(tokens, fee_bps) {
        Ok(fill) => fill.net_lamports,
        Err(_) => tokens_at_marginal(ancestor, tokens).min(ancestor.real_sol_reserves),
    }
}

/// What our book is worth after a leg that filled, on the ancestor's yardstick.
///
/// A buy has spent its gross and holds tokens; a sell has handed the parcel over
/// and holds lamports. The tip comes off either way, because a leg that landed
/// is a bundle that paid.
fn filled_book_lamports(
    ancestor: &CurveState,
    scenario: &ReorgScenario,
    outcome: &MevOutcome,
    fee_bps: u16,
) -> i64 {
    let tip = i128::from(scenario.tip_lamports);
    let book = match scenario.side {
        Side::Buy => {
            i128::from(liquidation_lamports(
                ancestor,
                outcome.filled_tokens,
                fee_bps,
            )) - i128::from(scenario.size)
                - tip
        }
        Side::Sell => i128::from(outcome.net_lamports) - tip,
    };
    book.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// What our book is worth after a leg that did not happen.
///
/// A buy that never landed spent nothing and holds nothing: zero, exactly. A
/// sell that never landed is still holding the parcel, which is worth what the
/// ancestor would pay for it. Neither pays a tip.
fn unfilled_book_lamports(ancestor: &CurveState, scenario: &ReorgScenario, fee_bps: u16) -> i64 {
    match scenario.side {
        Side::Buy => 0,
        Side::Sell => liquidation_lamports(ancestor, scenario.size, fee_bps)
            .min(i64::MAX.unsigned_abs()) as i64,
    }
}

/// Prices one scenario on both branches.
///
/// Never fails. A branch the curve refuses is a fate rather than an error —
/// [`ReorgFate::Refused`] on the winning branch, and `priced: false` when it is
/// the baseline that cannot be quoted — because a sweep of a million scenarios
/// that stopped at the first unquotable one would be a sweep of the scenarios
/// before it.
pub fn simulate_reorg(scenario: &ReorgScenario, config: &AdversaryConfig) -> ReorgOutcome {
    let ancestor = CurveState::at_real_sol(scenario.ancestor_real_sol_lamports);

    // The branch we priced against.
    let canonical = ancestor
        .displaced(scenario.canonical_flow_lamports)
        .and_then(|curve| {
            leg_through(&curve, scenario, config)
                .ok()
                .map(|fill| (curve, fill))
        });
    let Some((_, canonical)) = canonical else {
        return ReorgOutcome::unpriced(*scenario, &ancestor, config.profile);
    };
    let canonical_book = filled_book_lamports(&ancestor, scenario, &canonical, config.fee_bps);

    // The branch that won. Depth zero is not a fork: nothing was rolled back,
    // so the replacement flow is not applied and the two branches are one.
    let (fate, reorged) = if !scenario.forked_at_all() {
        (ReorgFate::Untouched, Some(canonical))
    } else if !scenario.reincluded {
        (ReorgFate::Dropped, None)
    } else {
        match ancestor
            .displaced(scenario.replacement_flow_lamports)
            .and_then(|curve| leg_through(&curve, scenario, config).ok())
        {
            Some(fill) => (ReorgFate::Reincluded, Some(fill)),
            None => (ReorgFate::Refused, None),
        }
    };

    let reorged_book = match &reorged {
        Some(fill) => filled_book_lamports(&ancestor, scenario, fill, config.fee_bps),
        None => unfilled_book_lamports(&ancestor, scenario, config.fee_bps),
    };
    let tip_refunded = if fate.filled() {
        0
    } else {
        scenario.tip_lamports
    };

    ReorgOutcome {
        scenario: *scenario,
        fate,
        profile: config.profile,
        ancestor_price_micros: curve_price_micros(&ancestor),
        canonical_tokens: canonical.filled_tokens,
        canonical_net_lamports: canonical.net_lamports,
        canonical_penalty_lamports: canonical.penalty_lamports,
        canonical_slippage_bps: canonical.slippage_bps,
        canonical_book_lamports: canonical_book,
        reorged_tokens: reorged.map(|fill| fill.filled_tokens).unwrap_or(0),
        reorged_net_lamports: reorged.map(|fill| fill.net_lamports).unwrap_or(0),
        reorged_penalty_lamports: reorged.map(|fill| fill.penalty_lamports).unwrap_or(0),
        reorged_slippage_bps: reorged.map(|fill| fill.slippage_bps).unwrap_or(0),
        reorged_book_lamports: reorged_book,
        book_delta_lamports: (i128::from(reorged_book) - i128::from(canonical_book))
            .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
        tip_refunded_lamports: tip_refunded,
        priced: true,
        synthetic: true,
    }
}

/// What a sweep of scenarios came out as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReorgSummary {
    pub profile: AdversaryProfile,
    pub scenarios: u32,
    pub untouched: u32,
    pub reincluded: u32,
    pub dropped: u32,
    pub refused: u32,
    /// Scenarios whose own baseline was not quotable, and which therefore say
    /// nothing about anything. Counted rather than dropped: a sweep that was
    /// mostly unquotable is a sweep of the wrong curve positions, and a
    /// summary of zeroes with no explanation looks like a chain nobody forked.
    pub unpriced: u32,
    /// Scenarios the fork left us worse off.
    pub adverse: u32,
    /// Scenarios the fork left us better off. Not a benefit to bank — it is
    /// the same coin as the losses, and a policy that counted one side of it
    /// would be reporting a strategy that profits from consensus failure.
    pub favourable: u32,
    pub total_loss_lamports: u64,
    pub total_gain_lamports: u64,
    pub net_delta_lamports: i64,
    /// The single worst scenario, by delta, and the scenario it was.
    pub worst_delta_lamports: i64,
    pub worst_scenario: Option<ReorgScenario>,
    /// The mean loss over the adverse scenarios only. Over the adverse ones
    /// rather than all of them, because a sweep padded with untouched controls
    /// would otherwise report a smaller number the more controls it ran.
    pub mean_adverse_loss_lamports: u64,
    pub tips_refunded_lamports: u64,
    pub max_depth_slots: u32,
    /// Always true. Nothing in this struct was observed.
    pub synthetic: bool,
}

impl ReorgSummary {
    pub const fn empty(profile: AdversaryProfile) -> Self {
        ReorgSummary {
            profile,
            scenarios: 0,
            untouched: 0,
            reincluded: 0,
            dropped: 0,
            refused: 0,
            unpriced: 0,
            adverse: 0,
            favourable: 0,
            total_loss_lamports: 0,
            total_gain_lamports: 0,
            net_delta_lamports: 0,
            worst_delta_lamports: 0,
            worst_scenario: None,
            mean_adverse_loss_lamports: 0,
            tips_refunded_lamports: 0,
            max_depth_slots: 0,
            synthetic: true,
        }
    }

    /// Folds one more outcome in.
    ///
    /// Public, and that is the point of the whole shape: a sweep too large to
    /// hold can be summarised as it is produced, so the memory a run needs is a
    /// function of this struct rather than of the scenario count.
    pub fn observe(&mut self, outcome: &ReorgOutcome) {
        self.scenarios = self.scenarios.saturating_add(1);
        self.max_depth_slots = self.max_depth_slots.max(outcome.scenario.depth_slots);
        if !outcome.priced {
            self.unpriced = self.unpriced.saturating_add(1);
            return;
        }
        match outcome.fate {
            ReorgFate::Untouched => self.untouched = self.untouched.saturating_add(1),
            ReorgFate::Reincluded => self.reincluded = self.reincluded.saturating_add(1),
            ReorgFate::Dropped => self.dropped = self.dropped.saturating_add(1),
            ReorgFate::Refused => self.refused = self.refused.saturating_add(1),
        }
        self.tips_refunded_lamports = self
            .tips_refunded_lamports
            .saturating_add(outcome.tip_refunded_lamports);

        let delta = outcome.book_delta_lamports;
        if delta < 0 {
            self.adverse = self.adverse.saturating_add(1);
            self.total_loss_lamports = self
                .total_loss_lamports
                .saturating_add(delta.unsigned_abs());
        } else if delta > 0 {
            self.favourable = self.favourable.saturating_add(1);
            self.total_gain_lamports = self
                .total_gain_lamports
                .saturating_add(delta.unsigned_abs());
        }
        self.net_delta_lamports = self.net_delta_lamports.saturating_add(delta);
        // Strictly worse, so the first scenario to reach a given depth of loss
        // is the one reported. The sweeps below enumerate in a fixed order, so
        // that makes the worst row a function of the grid rather than of which
        // equally bad scenario happened to be visited last.
        if delta < self.worst_delta_lamports || self.worst_scenario.is_none() {
            self.worst_delta_lamports = delta;
            self.worst_scenario = Some(outcome.scenario);
        }
    }

    /// The derived columns, once every outcome has been observed.
    pub fn finish(mut self) -> Self {
        self.mean_adverse_loss_lamports = if self.adverse == 0 {
            0
        } else {
            self.total_loss_lamports / u64::from(self.adverse)
        };
        self
    }

    /// Folds a slice that is already in hand.
    pub fn of(profile: AdversaryProfile, outcomes: &[ReorgOutcome]) -> Self {
        let mut summary = ReorgSummary::empty(profile);
        for outcome in outcomes {
            summary.observe(outcome);
        }
        summary.finish()
    }
}

/// Prices every scenario and folds them, holding one outcome at a time.
pub fn sweep_reorgs(scenarios: &[ReorgScenario], config: &AdversaryConfig) -> ReorgSummary {
    let mut summary = ReorgSummary::empty(config.profile);
    for scenario in scenarios {
        summary.observe(&simulate_reorg(scenario, config));
    }
    summary.finish()
}

/// SplitMix64's finaliser, as a pure integer function of `(seed, index)`.
///
/// A generator that carried state between draws would make the scenario at
/// index 900 000 depend on the 899 999 before it, and a sweep that had to be
/// replayed from the start to reproduce one failure is not a sweep anybody can
/// debug. This way two integers name a scenario.
///
/// Not a cryptographic hash and not asked to be one: `DrawSource` in
/// `replay.rs` is where a draw that has to resist being steered comes from.
/// This one only has to spread indices across axes without a pattern that
/// lines up with the axis lengths.
pub const fn mix64(seed: u64, index: u64) -> u64 {
    let mut z = seed.wrapping_add(index.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// The axes a sweep is built from.
///
/// A cross product rather than a distribution, because a distribution is a
/// claim about how often forks of each depth happen and there is no sample in
/// this repository to fit one to. A grid says only "these are the cases", which
/// is a claim a reader can check by reading the axes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReorgGrid {
    pub depths_slots: Vec<u32>,
    pub ancestors_real_sol_lamports: Vec<u64>,
    pub canonical_flows_lamports: Vec<i64>,
    /// What the winning branch replays instead, per slot of depth, on top of
    /// the canonical flow. A fork four slots deep replays four slots of it —
    /// which is the one place `depth_slots` is tied to the arithmetic.
    pub replacement_drift_per_slot_lamports: Vec<i64>,
    pub sides: Vec<Side>,
    /// Gross lamports. A sell's size is derived from one of these rather than
    /// given, so both sides of the grid trade the same parcel of the same
    /// curve — see [`ReorgGrid::scenarios`].
    pub sizes_lamports: Vec<u64>,
    pub volatilities_micros: Vec<u64>,
    pub tip_lamports: u64,
    /// One scenario in every `drop_every` has its leg dropped by the winning
    /// branch. Zero drops none and one drops all.
    pub drop_every: u32,
}

impl Default for ReorgGrid {
    fn default() -> Self {
        ReorgGrid::standard()
    }
}

impl ReorgGrid {
    /// A sweep across the whole life of a curve, at four fork depths.
    ///
    /// The positions run from a curve nobody has noticed to one a lamport short
    /// of graduating, because the two ends behave differently and a sweep of
    /// the middle would miss both: the bottom has no real SOL to exit into, and
    /// the top is where a positive replacement flow graduates the curve out
    /// from under an order and the quote is refused outright.
    pub fn standard() -> Self {
        let sol = LAMPORTS_PER_SOL as i64;
        ReorgGrid {
            depths_slots: vec![0, 1, 2, 4, 8],
            ancestors_real_sol_lamports: vec![
                LAMPORTS_PER_SOL,
                20 * LAMPORTS_PER_SOL,
                45 * LAMPORTS_PER_SOL,
                70 * LAMPORTS_PER_SOL,
                84 * LAMPORTS_PER_SOL,
            ],
            canonical_flows_lamports: vec![-sol / 2, 0, sol],
            replacement_drift_per_slot_lamports: vec![-sol / 4, 0, sol / 4],
            sides: vec![Side::Buy, Side::Sell],
            sizes_lamports: vec![
                LAMPORTS_PER_SOL / 100,
                LAMPORTS_PER_SOL / 4,
                LAMPORTS_PER_SOL,
            ],
            volatilities_micros: vec![0, VOLATILITY_SATURATION_MICROS],
            tip_lamports: 1_000_000,
            drop_every: 7,
        }
    }

    /// The control grid: no fork anywhere in it.
    ///
    /// What the standard sweep is read against. Every delta in it has to be
    /// exactly zero, which is a check on the fold rather than on the market.
    pub fn quiet() -> Self {
        ReorgGrid {
            depths_slots: vec![0],
            drop_every: 0,
            ..ReorgGrid::standard()
        }
    }

    /// How many scenarios the cross product has, before the ones the curve
    /// refuses to size are dropped.
    pub fn upper_bound(&self) -> usize {
        self.depths_slots.len()
            * self.ancestors_real_sol_lamports.len()
            * self.canonical_flows_lamports.len()
            * self.replacement_drift_per_slot_lamports.len()
            * self.sides.len()
            * self.sizes_lamports.len()
            * self.volatilities_micros.len()
    }

    /// The whole cross product, in one order on every machine.
    ///
    /// A sell's size is the parcel a buy of the same gross would have bought on
    /// the ancestor, derived through `config.fee_bps`. Derived rather than
    /// configured because a grid whose two sides traded unrelated amounts would
    /// report a side difference that was really a size difference. A gross the
    /// ancestor cannot fill at all yields no scenario, which is the same
    /// refusal every other sizer in this module makes.
    pub fn scenarios(&self, config: &AdversaryConfig) -> Vec<ReorgScenario> {
        let mut out = Vec::with_capacity(self.upper_bound());
        let mut index: u64 = 0;
        for &depth in &self.depths_slots {
            for &ancestor_lamports in &self.ancestors_real_sol_lamports {
                let ancestor = CurveState::at_real_sol(ancestor_lamports);
                for &canonical in &self.canonical_flows_lamports {
                    for &drift in &self.replacement_drift_per_slot_lamports {
                        let replacement =
                            canonical.saturating_add(drift.saturating_mul(i64::from(depth)));
                        for &side in &self.sides {
                            for &gross in &self.sizes_lamports {
                                let Some(size) = grid_size(&ancestor, side, gross, config.fee_bps)
                                else {
                                    continue;
                                };
                                for &volatility in &self.volatilities_micros {
                                    let reincluded = self.drop_every == 0
                                        || index % u64::from(self.drop_every) != 0;
                                    out.push(ReorgScenario {
                                        depth_slots: depth,
                                        ancestor_real_sol_lamports: ancestor_lamports,
                                        canonical_flow_lamports: canonical,
                                        replacement_flow_lamports: replacement,
                                        side,
                                        size,
                                        reincluded,
                                        tip_lamports: self.tip_lamports,
                                        volatility_micros: volatility,
                                    });
                                    index += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// `count` scenarios drawn from these axes by [`mix64`].
    ///
    /// For a sweep wider than the cross product is deep: the grid's axes stay
    /// the vocabulary and the mixer picks combinations out of it, so a run can
    /// be asked for a hundred thousand scenarios without a hundred-thousand-row
    /// grid having to be written down. Fewer than `count` come back when the
    /// curve refuses to size one, and which ones those are is a function of the
    /// seed alone.
    pub fn sampled(&self, config: &AdversaryConfig, seed: u64, count: u32) -> Vec<ReorgScenario> {
        let mut out = Vec::with_capacity(count as usize);
        for draw in 0..u64::from(count) {
            // One stream per axis, addressed by `(seed, draw x stride + axis)`,
            // so adding an axis moves that axis's draws and nothing else's —
            // the discipline `fixtures::GeneratorConfig` documents for its own.
            let base = draw.wrapping_mul(16);
            let pick = |axis: u64, len: usize| -> usize {
                if len == 0 {
                    0
                } else {
                    (mix64(seed, base.wrapping_add(axis)) % len as u64) as usize
                }
            };
            if self.depths_slots.is_empty()
                || self.ancestors_real_sol_lamports.is_empty()
                || self.canonical_flows_lamports.is_empty()
                || self.replacement_drift_per_slot_lamports.is_empty()
                || self.sides.is_empty()
                || self.sizes_lamports.is_empty()
                || self.volatilities_micros.is_empty()
            {
                return out;
            }
            let depth = self.depths_slots[pick(0, self.depths_slots.len())];
            let ancestor_lamports =
                self.ancestors_real_sol_lamports[pick(1, self.ancestors_real_sol_lamports.len())];
            let canonical =
                self.canonical_flows_lamports[pick(2, self.canonical_flows_lamports.len())];
            let drift = self.replacement_drift_per_slot_lamports
                [pick(3, self.replacement_drift_per_slot_lamports.len())];
            let side = self.sides[pick(4, self.sides.len())];
            let gross = self.sizes_lamports[pick(5, self.sizes_lamports.len())];
            let volatility = self.volatilities_micros[pick(6, self.volatilities_micros.len())];

            let ancestor = CurveState::at_real_sol(ancestor_lamports);
            let Some(size) = grid_size(&ancestor, side, gross, config.fee_bps) else {
                continue;
            };
            let reincluded = self.drop_every == 0
                || mix64(seed, base.wrapping_add(7)) % u64::from(self.drop_every) != 0;
            out.push(ReorgScenario {
                depth_slots: depth,
                ancestor_real_sol_lamports: ancestor_lamports,
                canonical_flow_lamports: canonical,
                replacement_flow_lamports: canonical
                    .saturating_add(drift.saturating_mul(i64::from(depth))),
                side,
                size,
                reincluded,
                tip_lamports: self.tip_lamports,
                volatility_micros: volatility,
            });
        }
        out
    }

    /// Builds and prices the whole cross product in one call.
    pub fn sweep(&self, config: &AdversaryConfig) -> ReorgSummary {
        sweep_reorgs(&self.scenarios(config), config)
    }
}

/// The size one grid cell trades, in the unit its side is quoted in.
///
/// `None` when the ancestor would refuse a buy of `gross` — which is what
/// happens at the top of the curve, where there are no real tokens left to sell
/// and therefore no parcel for the sell side of the grid to be built from.
fn grid_size(ancestor: &CurveState, side: Side, gross: u64, fee_bps: u16) -> Option<u64> {
    let size = match side {
        Side::Buy => gross,
        Side::Sell => ancestor.quote_buy(gross, fee_bps).ok()?.tokens,
    };
    (size > 0).then_some(size)
}

// ===========================================================================
// One adversary, several curves
// ===========================================================================

/// How many slices a purse is cut into before it is allocated across curves.
///
/// Sixteen. The allocation below is a greedy walk over slices, so this is the
/// resolution of the answer and the cost of computing it in one number: every
/// slice costs one sandwich simulation per candidate pool. Sixteen is fine
/// enough that the allocation tracks the profitable curve and coarse enough
/// that a sweep of a thousand pool sets finishes.
pub const DEFAULT_ALLOCATION_SLICES: u32 = 16;

/// One curve an adversary could work, and the order in front of it.
///
/// Not `Serialize`, and that is not an oversight: `CurveState` is the six
/// numbers off an account and belongs in the report as the *consequences* of
/// pricing against it rather than as a copy of the input. What comes out of the
/// allocation below carries the mint and the lamports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolTarget {
    pub mint: String,
    pub curve: CurveState,
    /// The public buy the adversary would wrap. Gross lamports.
    pub victim_gross_lamports: u64,
    pub context: MarketContext,
}

impl PoolTarget {
    /// A target at a curve position, with the context that curve implies.
    pub fn at_real_sol(mint: &str, real_sol_lamports: u64, victim_gross_lamports: u64) -> Self {
        let curve = CurveState::at_real_sol(real_sol_lamports);
        PoolTarget {
            mint: mint.to_string(),
            context: MarketContext::at(&curve),
            curve,
            victim_gross_lamports,
        }
    }

    /// The same target, in a window that has been moving.
    pub fn with_volatility(mut self, volatility_micros: u64) -> Self {
        self.context.volatility_micros = volatility_micros;
        self
    }
}

/// What the adversary did to one curve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolExtraction {
    pub mint: String,
    /// The blend this curve's moment produced, in millionths.
    pub intensity_micros: u64,
    /// The most this profile would put into this one curve at that intensity,
    /// after the ceiling on modelled damage has had its say.
    pub capacity_lamports: u64,
    pub attacker_lamports: u64,
    pub attacker_profit_lamports: i64,
    pub extraction_lamports: i64,
    pub victim_gross_lamports: u64,
    pub victim_tokens_solo: u64,
    pub victim_tokens: u64,
    pub victim_penalty_lamports: u64,
    pub victim_penalty_bps: u16,
    pub victim_damage_bps: u16,
    /// Whether [`AdversaryConfig::max_penalty_bps`] cut this curve's capacity
    /// back, including cutting it back to nothing.
    pub bounded: bool,
    /// Whether any front-run on this curve clears its own fees at all, before
    /// any question of capital. §15.2's threshold, per pool.
    pub viable: bool,
    /// Always true. Nothing here was observed.
    pub synthetic: bool,
}

impl PoolExtraction {
    /// A curve the adversary looked at and left alone.
    fn untouched(
        target: &PoolTarget,
        intensity_micros: u64,
        capacity_lamports: u64,
        bounded: bool,
        viable: bool,
    ) -> Self {
        PoolExtraction {
            mint: target.mint.clone(),
            intensity_micros,
            capacity_lamports,
            attacker_lamports: 0,
            attacker_profit_lamports: 0,
            extraction_lamports: 0,
            victim_gross_lamports: target.victim_gross_lamports,
            victim_tokens_solo: 0,
            victim_tokens: 0,
            victim_penalty_lamports: 0,
            victim_penalty_bps: 0,
            victim_damage_bps: 0,
            bounded,
            viable,
            synthetic: true,
        }
    }

    /// Whether the adversary actually worked this curve.
    pub const fn attacked(&self) -> bool {
        self.attacker_lamports > 0
    }
}

/// One purse, spread across every curve it could work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiPoolExtraction {
    pub profile: AdversaryProfile,
    pub pools_offered: u32,
    /// Pools where a front-run clears its own fees at all.
    pub pools_viable: u32,
    /// Pools the allocation actually put lamports into.
    pub pools_attacked: u32,
    /// Pools whose capacity the ceiling cut back.
    pub pools_bounded: u32,
    pub capital_lamports: u64,
    pub slices: u32,
    pub slice_lamports: u64,
    pub capital_deployed_lamports: u64,
    /// What the purse could not find a profitable home for. The interesting
    /// column: a searcher with idle capital is a venue with no edge left in it,
    /// and a model where the purse always empties is a model that has stopped
    /// being about profit.
    pub capital_idle_lamports: u64,
    pub total_profit_lamports: i64,
    pub total_extraction_lamports: i64,
    pub total_victim_penalty_lamports: u64,
    pub worst_victim_penalty_bps: u16,
    /// Size-weighted across the victims' own notionals, for the reason
    /// [`MevSummary::mean_penalty_bps`] is: a dust order and a one-SOL order
    /// are not two equal observations of what this venue costs.
    pub mean_victim_penalty_bps: u16,
    /// The ceiling the run was computed under, echoed so the bound travels
    /// with the numbers it bounds.
    pub max_penalty_bps: u16,
    /// One row per pool offered, sorted by mint. Pools that were left alone are
    /// present at zero rather than absent, so a diff between two runs is a diff
    /// of allocations rather than of structure.
    pub rows: Vec<PoolExtraction>,
    /// Always true. Nothing in this struct was observed.
    pub synthetic: bool,
}

impl MultiPoolExtraction {
    /// An adversary with nothing to work.
    pub fn empty(profile: AdversaryProfile, capital_lamports: u64, max_penalty_bps: u16) -> Self {
        MultiPoolExtraction {
            profile,
            pools_offered: 0,
            pools_viable: 0,
            pools_attacked: 0,
            pools_bounded: 0,
            capital_lamports,
            slices: 0,
            slice_lamports: 0,
            capital_deployed_lamports: 0,
            capital_idle_lamports: capital_lamports,
            total_profit_lamports: 0,
            total_extraction_lamports: 0,
            total_victim_penalty_lamports: 0,
            worst_victim_penalty_bps: 0,
            mean_victim_penalty_bps: 0,
            max_penalty_bps,
            rows: Vec::new(),
            synthetic: true,
        }
    }

    /// Whether the purse adds up: what went out plus what stayed home is what
    /// there was. Asserted in the tests rather than reported, because a purse
    /// that does not balance is a bug in the allocator.
    pub fn balances(&self) -> bool {
        self.capital_deployed_lamports
            .saturating_add(self.capital_idle_lamports)
            == self.capital_lamports
    }
}

/// The most this profile will put into one curve, after the ceiling.
///
/// Two bounds, and the tighter one wins. The **purse** bound is the profile's
/// own capital at this moment's intensity — the same number a single-curve run
/// uses. The **damage** bound is [`AdversaryConfig::max_penalty_bps`], enforced
/// the way the rest of this module enforces it: by making the adversary smaller
/// rather than by clipping the damage afterwards, so what comes back is a
/// counterfactual somebody could have executed.
///
/// Returns the capacity and whether the ceiling was what set it.
fn pool_capacity(
    target: &PoolTarget,
    config: &AdversaryConfig,
    intensity_micros: u64,
) -> (u64, bool) {
    let purse = config.committed_lamports(intensity_micros);
    if purse < MIN_VIABLE_ATTACKER_LAMPORTS {
        return (0, false);
    }
    let within_bound = |size: u64| {
        front_run_cost(&target.curve, size, target.victim_gross_lamports, config)
            .is_some_and(|cost| cost.penalty_bps <= config.max_penalty_bps)
    };
    if within_bound(purse) {
        return (purse, false);
    }
    match largest_admissible(MIN_VIABLE_ATTACKER_LAMPORTS, purse, within_bound) {
        Some(size) => (size, true),
        None => (0, true),
    }
}

/// Spreads one purse across several curves and reports what it did to each.
///
/// # The allocation
///
/// A greedy walk over slices: the purse is cut into `slices` equal parts and
/// each part goes to whichever curve gains the most from it, ties to the
/// earliest pool in the input. A part that gains nothing anywhere is not spent,
/// and the walk stops there.
///
/// The gain of the first slice into a curve is net of that curve's whole
/// landing cost, because [`front_run_cost`] charges it against profit — so a
/// venue where three small positions each fail to clear their own fees is
/// correctly reported as one where the adversary does nothing, rather than as
/// three tiny attacks.
///
/// # What this is a bound on
///
/// Profit against attacker size is unimodal rather than concave, so a greedy
/// walk can stop short of the true joint optimum. What comes back is therefore
/// a **lower bound** on what a searcher with this purse could have taken — the
/// same direction, and for the same reason, that
/// [`best_front_run_deterministic`] understates a single-curve optimum. A model
/// of adverse selection should err towards saying the adversary was weaker than
/// it was; the alternative is a backtest that explains its losses away.
pub fn extract_across_pools(
    pools: &[PoolTarget],
    config: &AdversaryConfig,
    slices: u32,
) -> MultiPoolExtraction {
    let mut report = MultiPoolExtraction::empty(
        config.profile,
        config.capital_lamports,
        config.max_penalty_bps,
    );
    report.pools_offered = u32::try_from(pools.len()).unwrap_or(u32::MAX);
    report.slices = slices;

    // A passive taker does not front-run anybody, on one curve or on a
    // thousand. The rows are still emitted at zero: "nobody was there" is a
    // result, and a report that omitted the pools would not be diffable against
    // one where somebody was.
    let hostile = config.profile.hostile();

    let mut intensities = Vec::with_capacity(pools.len());
    let mut capacities = Vec::with_capacity(pools.len());
    let mut bounded = Vec::with_capacity(pools.len());
    let mut viable = Vec::with_capacity(pools.len());
    for target in pools {
        let intensity = config.intensity_micros(target.context);
        let is_viable = hostile
            && sandwich_viable(
                target.victim_gross_lamports,
                target.curve.virtual_sol_reserves,
                config.fee_bps,
            );
        let (capacity, was_bounded) = if is_viable {
            pool_capacity(target, config, intensity)
        } else {
            (0, false)
        };
        intensities.push(intensity);
        capacities.push(capacity);
        bounded.push(was_bounded);
        viable.push(is_viable);
    }

    let slice = if slices == 0 {
        0
    } else {
        config.capital_lamports / u64::from(slices)
    };
    report.slice_lamports = slice;

    let mut allocation = vec![0u64; pools.len()];
    let mut profit = vec![0i64; pools.len()];
    let mut spent: u64 = 0;

    if slice >= MIN_VIABLE_ATTACKER_LAMPORTS {
        for _ in 0..slices {
            let mut best: Option<(usize, u64, i64, i64)> = None;
            for (index, target) in pools.iter().enumerate() {
                if !viable[index] {
                    continue;
                }
                let candidate = allocation[index].saturating_add(slice);
                if candidate > capacities[index] {
                    continue;
                }
                if spent.saturating_add(slice) > config.capital_lamports {
                    continue;
                }
                let Some(cost) = front_run_cost(
                    &target.curve,
                    candidate,
                    target.victim_gross_lamports,
                    config,
                ) else {
                    continue;
                };
                let gain = i128::from(cost.attacker_profit_lamports) - i128::from(profit[index]);
                if gain <= 0 {
                    continue;
                }
                // Strictly greater, so the earliest pool wins a tie. The input
                // order is the caller's and is fixed, which is what makes the
                // allocation a function of the pool set rather than of which
                // equally good curve was visited last.
                let gain = gain.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
                match best {
                    Some((_, _, best_gain, _)) if best_gain >= gain => {}
                    _ => best = Some((index, candidate, gain, cost.attacker_profit_lamports)),
                }
            }
            let Some((index, candidate, _, new_profit)) = best else {
                break;
            };
            allocation[index] = candidate;
            profit[index] = new_profit;
            spent = spent.saturating_add(slice);
        }
    }

    let mut weighted: u128 = 0;
    let mut notional: u128 = 0;
    let mut total_profit: i128 = 0;
    let mut total_extraction: i128 = 0;

    for (index, target) in pools.iter().enumerate() {
        if viable[index] {
            report.pools_viable += 1;
        }
        if bounded[index] {
            report.pools_bounded += 1;
        }
        let row = match allocation[index] {
            0 => PoolExtraction::untouched(
                target,
                intensities[index],
                capacities[index],
                bounded[index],
                viable[index],
            ),
            size => match front_run_cost(&target.curve, size, target.victim_gross_lamports, config)
            {
                Some(cost) => {
                    report.pools_attacked += 1;
                    total_profit =
                        total_profit.saturating_add(i128::from(cost.attacker_profit_lamports));
                    total_extraction =
                        total_extraction.saturating_add(i128::from(cost.extraction_lamports));
                    report.total_victim_penalty_lamports = report
                        .total_victim_penalty_lamports
                        .saturating_add(cost.penalty_lamports);
                    report.worst_victim_penalty_bps =
                        report.worst_victim_penalty_bps.max(cost.penalty_bps);
                    weighted = weighted.saturating_add(u128::from(cost.penalty_lamports));
                    PoolExtraction {
                        mint: target.mint.clone(),
                        intensity_micros: intensities[index],
                        capacity_lamports: capacities[index],
                        attacker_lamports: size,
                        attacker_profit_lamports: cost.attacker_profit_lamports,
                        extraction_lamports: cost.extraction_lamports,
                        victim_gross_lamports: target.victim_gross_lamports,
                        victim_tokens_solo: cost.victim_tokens_solo,
                        victim_tokens: cost.victim_tokens,
                        victim_penalty_lamports: cost.penalty_lamports,
                        victim_penalty_bps: cost.penalty_bps,
                        victim_damage_bps: cost.damage_bps,
                        bounded: bounded[index],
                        viable: viable[index],
                        synthetic: true,
                    }
                }
                // The allocator only ever assigns a size it has already priced,
                // so this is unreachable. Reported as untouched rather than
                // panicked on: a report that lost one pool is a smaller wrong
                // answer than a sweep that died.
                None => PoolExtraction::untouched(
                    target,
                    intensities[index],
                    capacities[index],
                    bounded[index],
                    viable[index],
                ),
            },
        };
        notional = notional.saturating_add(u128::from(target.victim_gross_lamports));
        report.rows.push(row);
    }

    report
        .rows
        .sort_by(|left, right| left.mint.cmp(&right.mint));
    report.capital_deployed_lamports = report.rows.iter().fold(0u64, |total, row| {
        total.saturating_add(row.attacker_lamports)
    });
    report.capital_idle_lamports = config
        .capital_lamports
        .saturating_sub(report.capital_deployed_lamports);
    report.total_profit_lamports =
        total_profit.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
    report.total_extraction_lamports =
        total_extraction.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
    report.mean_victim_penalty_bps = if notional == 0 {
        0
    } else {
        mul_div_ceil(weighted, u128::from(BPS_DENOMINATOR), notional)
            .min(u128::from(BPS_DENOMINATOR)) as u16
    };
    report
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::DEFAULT_FEE_BPS;

    /// A curve a third of the way up, which is where most of a launch's life is
    /// spent and where none of the thresholds in this module are near an edge.
    fn mid_curve() -> CurveState {
        CurveState::at_real_sol(30 * LAMPORTS_PER_SOL)
    }

    /// An adversary with a purse big enough that the front-run search finds
    /// something at the sizes these tests trade.
    ///
    /// The default profile deliberately cannot: a quarter of one SOL against a
    /// 60-SOL virtual reserve does not clear a 0.005-SOL landing cost, which is
    /// the correct answer for that configuration and a useless one to test the
    /// arithmetic with.
    fn predator() -> AdversaryConfig {
        let mut config = AdversaryConfig::default()
            .with_profile(AdversaryProfile::PredatorySandwich)
            .bounded(20 * LAMPORTS_PER_SOL, DEFAULT_MAX_PENALTY_BPS);
        config.landing_cost_lamports = 1_000_000;
        config
    }

    fn backrunner() -> AdversaryConfig {
        AdversaryConfig::default().with_profile(AdversaryProfile::HighFrequencyBackrunner)
    }

    // -----------------------------------------------------------------------
    // the two things that move a profile
    // -----------------------------------------------------------------------

    #[test]
    fn transition_pressure_is_zero_below_the_floor_and_one_at_the_line() {
        assert_eq!(transition_pressure_micros(0), 0);
        assert_eq!(transition_pressure_micros(TRANSITION_FLOOR_BPS), 0);
        assert_eq!(transition_pressure_micros(BPS_DENOMINATOR as u16), MICROS);
        // Half way between the floor and the line is half the pressure.
        assert_eq!(transition_pressure_micros(7_500), 500_000);
        // Past the line is still one rather than more than one.
        assert_eq!(transition_pressure_micros(u16::MAX), MICROS);
    }

    #[test]
    fn transition_pressure_never_falls_as_the_curve_fills() {
        let mut previous = 0;
        for progress in (0..=10_000u32).step_by(37) {
            let pressure = transition_pressure_micros(progress as u16);
            assert!(pressure >= previous, "pressure fell at {progress} bps");
            previous = pressure;
        }
    }

    #[test]
    fn a_flat_window_has_no_volatility() {
        assert_eq!(tick_volatility_micros(&[]), 0);
        assert_eq!(tick_volatility_micros(&[1_000]), 0);
        assert_eq!(tick_volatility_micros(&[1_000, 1_000, 1_000]), 0);
    }

    #[test]
    fn volatility_is_the_mean_absolute_move() {
        // +10%, then -5% of the new level: 100 000 and 50 000 millionths, and
        // the answer is the mean of the two rather than the move end to end.
        let samples = [1_000_000u64, 1_100_000, 1_045_000];
        assert_eq!(tick_volatility_micros(&samples), 75_000);
        // Absolute, so a round trip is volatile rather than flat.
        assert_eq!(
            tick_volatility_micros(&[1_000_000, 1_100_000, 1_000_000]),
            95_454
        );
        // A zero sample is skipped rather than divided by.
        assert_eq!(tick_volatility_micros(&[0, 1_000_000, 1_100_000]), 100_000);
        assert_eq!(tick_volatility_micros(&[0, 0]), 0);
    }

    #[test]
    fn the_volatility_term_saturates_at_a_ten_percent_tick() {
        assert_eq!(volatility_term_micros(0), 0);
        assert_eq!(
            volatility_term_micros(VOLATILITY_SATURATION_MICROS / 2),
            MICROS / 2
        );
        assert_eq!(volatility_term_micros(VOLATILITY_SATURATION_MICROS), MICROS);
        assert_eq!(volatility_term_micros(MICROS), MICROS);
    }

    #[test]
    fn intensity_rises_with_the_transition_and_with_volatility_and_stops_at_one() {
        let config = AdversaryConfig::default();
        let quiet = config.intensity_micros(MarketContext::default());
        assert_eq!(quiet, config.base_intensity_micros);

        let late = config.intensity_micros(MarketContext {
            progress_bps: BPS_DENOMINATOR as u16,
            volatility_micros: 0,
        });
        assert_eq!(
            late,
            config.base_intensity_micros + config.transition_gain_micros
        );

        let wild = config.intensity_micros(MarketContext {
            progress_bps: BPS_DENOMINATOR as u16,
            volatility_micros: MICROS,
        });
        assert_eq!(wild, MICROS);
        assert!(wild > late && late > quiet);
    }

    #[test]
    fn curve_price_rises_with_the_curve() {
        let low = curve_price_micros(&CurveState::at_real_sol(LAMPORTS_PER_SOL));
        let high = curve_price_micros(&CurveState::at_real_sol(50 * LAMPORTS_PER_SOL));
        assert!(high > low);
        // Ten significant digits at launch reserves, so two adjacent ticks on a
        // quiet curve are still distinguishable.
        assert!(low > 1_000_000_000);
    }

    // -----------------------------------------------------------------------
    // what each profile does
    // -----------------------------------------------------------------------

    #[test]
    fn a_passive_taker_never_touches_a_fill() {
        let curve = mid_curve();
        let config = AdversaryConfig::default();
        let size = LAMPORTS_PER_SOL;

        let solo_buy = curve.quote_buy(size, config.fee_bps).expect("quote");
        let bought = buy_through(&curve, size, &config, MarketContext::at(&curve)).expect("buy");
        assert_eq!(bought.filled_tokens, solo_buy.tokens);
        assert_eq!(bought.penalty_lamports, 0);
        assert_eq!(bought.penalty_bps, 0);
        assert!(!bought.attacked());
        assert!(!bought.bounded);

        let solo_sell = curve
            .quote_sell(solo_buy.tokens, config.fee_bps)
            .expect("quote");
        let sold = sell_through(&curve, solo_buy.tokens, &config, MarketContext::at(&curve))
            .expect("sell");
        assert_eq!(sold.net_lamports, solo_sell.net_lamports);
        assert_eq!(sold.penalty_lamports, 0);
        assert!(!sold.attacked());
    }

    #[test]
    fn a_backrunner_costs_nothing_on_the_way_in_and_something_on_the_way_out() {
        let curve = mid_curve();
        let config = backrunner();
        let context = MarketContext::at(&curve);

        // Landing after our buy cannot change what our buy received. This is a
        // result of the model, not a gap in it.
        let bought = buy_through(&curve, LAMPORTS_PER_SOL, &config, context).expect("buy");
        assert_eq!(bought.penalty_lamports, 0);
        assert!(!bought.attacked());

        let sold = sell_through(&curve, bought.filled_tokens, &config, context).expect("sell");
        assert!(sold.attacked(), "a follower mirrors a share of the exit");
        assert!(sold.penalty_lamports > 0);
        assert!(sold.filled_gross_lamports < sold.solo_gross_lamports);
        assert_eq!(
            sold.penalty_lamports,
            sold.solo_gross_lamports - sold.filled_gross_lamports
        );
    }

    #[test]
    fn a_sandwich_costs_on_both_sides() {
        let curve = mid_curve();
        let config = predator();
        let context = MarketContext::at(&curve);

        let bought = buy_through(&curve, LAMPORTS_PER_SOL, &config, context).expect("buy");
        assert!(bought.attacked(), "a funded front-run clears its fees here");
        assert!(bought.attacker_lamports >= MIN_VIABLE_ATTACKER_LAMPORTS);
        assert!(bought.filled_tokens < bought.solo_tokens);
        assert!(bought.penalty_lamports > 0);
        assert!(bought
            .attacker_profit_lamports
            .is_some_and(|profit| profit > 0));

        let sold = sell_through(&curve, bought.filled_tokens, &config, context).expect("sell");
        assert!(sold.attacked());
        assert!(sold.penalty_lamports > 0);
        // The sell-side adversary is selling inventory this simulation never
        // gave it, so its own book is not reported.
        assert_eq!(sold.attacker_profit_lamports, None);
    }

    #[test]
    fn a_front_run_that_cannot_clear_its_fees_does_not_happen() {
        // A dust buy against a deep reserve is below §15.2's threshold, so no
        // front-run of any size pays — before any landing cost at all.
        let curve = CurveState::at_real_sol(80 * LAMPORTS_PER_SOL);
        let dust = 10_000;
        assert!(!sandwich_viable(
            dust,
            curve.virtual_sol_reserves,
            predator().fee_bps
        ));

        let bought =
            buy_through(&curve, dust, &predator(), MarketContext::at(&curve)).expect("buy");
        assert!(!bought.attacked());
        assert_eq!(bought.penalty_lamports, 0);
        assert!(
            !bought.bounded,
            "nothing was cut back — nothing was worth doing"
        );
    }

    #[test]
    fn an_adversary_with_no_purse_does_nothing() {
        let curve = mid_curve();
        let mut config = predator();
        config.capital_lamports = MIN_VIABLE_ATTACKER_LAMPORTS - 1;
        let bought =
            buy_through(&curve, LAMPORTS_PER_SOL, &config, MarketContext::at(&curve)).expect("buy");
        assert!(!bought.attacked());
        assert_eq!(bought.penalty_lamports, 0);
    }

    // -----------------------------------------------------------------------
    // the bound
    // -----------------------------------------------------------------------

    #[test]
    fn no_modelled_penalty_ever_exceeds_the_bound() {
        let sizes = [
            10_000u64,
            LAMPORTS_PER_SOL / 100,
            LAMPORTS_PER_SOL,
            5 * LAMPORTS_PER_SOL,
        ];
        let positions = [1u64, 10, 30, 60, 84];
        let contexts = [
            MarketContext::default(),
            MarketContext {
                progress_bps: 9_500,
                volatility_micros: MICROS,
            },
        ];

        for profile in AdversaryProfile::ALL {
            let config = predator().with_profile(profile);
            for position in positions {
                let curve = CurveState::at_real_sol(position * LAMPORTS_PER_SOL);
                for size in sizes {
                    for context in contexts {
                        let Ok(bought) = buy_through(&curve, size, &config, context) else {
                            continue;
                        };
                        assert!(
                            bought.penalty_bps <= config.max_penalty_bps,
                            "{profile:?} buy of {size} at {position} SOL: {} bps over a {} bps \
                             ceiling",
                            bought.penalty_bps,
                            config.max_penalty_bps
                        );
                        let Ok(sold) = sell_through(&curve, bought.filled_tokens, &config, context)
                        else {
                            continue;
                        };
                        assert!(
                            sold.penalty_bps <= config.max_penalty_bps,
                            "{profile:?} sell at {position} SOL: {} bps over a {} bps ceiling",
                            sold.penalty_bps,
                            config.max_penalty_bps
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_tight_bound_shrinks_the_adversary_rather_than_clipping_the_damage() {
        let curve = mid_curve();
        let context = MarketContext::at(&curve);
        let loose = predator();
        let tight = predator().bounded(loose.capital_lamports, 25);

        let unbounded = buy_through(&curve, LAMPORTS_PER_SOL, &loose, context).expect("buy");
        let bounded = buy_through(&curve, LAMPORTS_PER_SOL, &tight, context).expect("buy");

        assert!(
            unbounded.penalty_bps > tight.max_penalty_bps,
            "the bound has to bite to be tested"
        );
        assert!(bounded.penalty_bps <= tight.max_penalty_bps);
        assert!(
            bounded.bounded,
            "the report has to say the adversary was cut back"
        );
        assert!(bounded.attacker_lamports < unbounded.attacker_lamports);
        // The fill and the penalty are still two views of one event: the tokens
        // reported are the tokens the smaller front-run actually leaves us.
        assert!(bounded.filled_tokens > unbounded.filled_tokens);
        assert!(bounded.filled_tokens < bounded.solo_tokens);
    }

    #[test]
    fn an_adversary_that_cannot_act_inside_the_bound_does_not_act() {
        let curve = mid_curve();
        let bound_of_zero = predator().bounded(20 * LAMPORTS_PER_SOL, 0);
        let context = MarketContext::at(&curve);
        let bought = buy_through(&curve, LAMPORTS_PER_SOL, &bound_of_zero, context).expect("buy");
        assert!(!bought.attacked());
        assert_eq!(bought.penalty_lamports, 0);
        assert!(
            bought.bounded,
            "\"nothing to say\" is not the same as \"nothing happened\""
        );
    }

    #[test]
    fn a_penalty_never_falls_when_the_adversary_gets_richer() {
        let curve = mid_curve();
        let context = MarketContext::at(&curve);
        let mut previous = 0;
        for capital in [1u64, 2, 4, 8, 16, 32] {
            let config = predator().bounded(capital * LAMPORTS_PER_SOL, BPS_DENOMINATOR as u16);
            let bought = buy_through(&curve, LAMPORTS_PER_SOL, &config, context).expect("buy");
            assert!(
                bought.penalty_lamports >= previous,
                "penalty fell from {previous} at {capital} SOL of capital"
            );
            previous = bought.penalty_lamports;
        }
        assert!(previous > 0);
    }

    // -----------------------------------------------------------------------
    // zero states and refusals
    // -----------------------------------------------------------------------

    #[test]
    fn the_curve_refuses_what_it_would_refuse_anyway() {
        let curve = mid_curve();
        let config = predator();
        let context = MarketContext::at(&curve);
        assert_eq!(
            buy_through(&curve, 0, &config, context),
            Err(QuoteError::ZeroSize)
        );
        assert_eq!(
            sell_through(&curve, 0, &config, context),
            Err(QuoteError::ZeroSize)
        );

        let graduated = CurveState::at_real_sol(90 * LAMPORTS_PER_SOL);
        assert_eq!(
            buy_through(
                &graduated,
                LAMPORTS_PER_SOL,
                &config,
                MarketContext::at(&graduated)
            ),
            Err(QuoteError::CurveComplete)
        );
    }

    #[test]
    fn a_dump_that_drains_the_exit_is_cut_back_until_ours_still_fills() {
        // A thin curve and an adversary with more capital than the pool holds.
        let curve = CurveState::at_real_sol(LAMPORTS_PER_SOL);
        let config = predator().bounded(500 * LAMPORTS_PER_SOL, BPS_DENOMINATOR as u16);
        let context = MarketContext::at(&curve);
        let bought = buy_through(&curve, LAMPORTS_PER_SOL / 10, &config, context).expect("buy");
        let sold = sell_through(&curve, bought.filled_tokens, &config, context).expect("sell");
        // Whatever came back is a fill that could actually have happened.
        assert!(sold.filled_gross_lamports > 0);
        assert!(sold.filled_gross_lamports <= curve.real_sol_reserves);
        assert_eq!(
            sold.net_lamports + sold.fee_lamports,
            sold.filled_gross_lamports
        );
    }

    // -----------------------------------------------------------------------
    // determinism and the summary
    // -----------------------------------------------------------------------

    #[test]
    fn the_same_moment_prices_the_same_way_twice() {
        let curve = mid_curve();
        let config = predator();
        let context = MarketContext {
            progress_bps: 9_000,
            volatility_micros: 42_000,
        };
        let first = buy_through(&curve, LAMPORTS_PER_SOL, &config, context).expect("buy");
        let second = buy_through(&curve, LAMPORTS_PER_SOL, &config, context).expect("buy");
        assert_eq!(first, second);
    }

    #[test]
    fn the_summary_adds_the_legs_up_and_keeps_the_profile_it_ran_under() {
        let curve = mid_curve();
        let config = predator();
        let context = MarketContext::at(&curve);
        let bought = buy_through(&curve, LAMPORTS_PER_SOL, &config, context).expect("buy");
        let sold = sell_through(&curve, bought.filled_tokens, &config, context).expect("sell");

        let summary = MevSummary::of(config.profile, config.max_penalty_bps, &[bought, sold]);
        assert_eq!(summary.profile, AdversaryProfile::PredatorySandwich);
        assert_eq!(summary.legs_modelled, 2);
        assert_eq!(summary.legs_attacked, 2);
        assert_eq!(summary.entry_penalty_lamports, bought.penalty_lamports);
        assert_eq!(summary.exit_penalty_lamports, sold.penalty_lamports);
        assert_eq!(
            summary.total_penalty_lamports,
            bought.penalty_lamports + sold.penalty_lamports
        );
        assert_eq!(
            summary.worst_penalty_bps,
            bought.penalty_bps.max(sold.penalty_bps)
        );
        assert!(summary.mean_penalty_bps <= summary.worst_penalty_bps);
        assert!(summary.synthetic);
    }

    #[test]
    fn an_empty_book_summarises_to_zero_under_the_profile_it_ran_under() {
        let summary = MevSummary::of(AdversaryProfile::PredatorySandwich, 1_500, &[]);
        assert_eq!(
            summary,
            MevSummary::empty(AdversaryProfile::PredatorySandwich, 1_500)
        );
        assert_eq!(summary.legs_modelled, 0);
        assert_eq!(summary.total_penalty_lamports, 0);
        // A run where the adversary found nothing is still a run under that
        // adversary — reporting it as passive would lose the difference.
        assert_eq!(summary.profile, AdversaryProfile::PredatorySandwich);
    }

    // -----------------------------------------------------------------------
    // what a front-run costs, from both sides at once
    // -----------------------------------------------------------------------

    #[test]
    fn the_front_run_pricer_agrees_with_the_sandwich_it_came_from() {
        let curve = mid_curve();
        let config = predator();
        let victim = LAMPORTS_PER_SOL;
        let size = 2 * LAMPORTS_PER_SOL;

        let cost = front_run_cost(&curve, size, victim, &config).expect("all three legs quote");
        let sandwich = simulate_sandwich(
            &curve,
            size,
            victim,
            config.fee_bps,
            config.landing_cost_lamports,
        )
        .expect("the same three legs");

        assert_eq!(cost.attacker_lamports, size);
        assert_eq!(cost.victim_tokens, sandwich.victim_tokens);
        assert_eq!(cost.victim_tokens_solo, sandwich.victim_tokens_solo);
        assert_eq!(cost.damage_bps, sandwich.victim_damage_bps);
        assert_eq!(
            cost.attacker_profit_lamports,
            sandwich.attacker_profit_lamports
        );
        assert_eq!(cost.extraction_lamports, sandwich.extraction_lamports);
        // The solo figure is the curve's own answer for the victim alone, which
        // is what makes the displacement a difference rather than an estimate.
        assert_eq!(
            cost.victim_tokens_solo,
            curve
                .quote_buy(victim, config.fee_bps)
                .expect("quote")
                .tokens
        );
        assert!(cost.victim_tokens < cost.victim_tokens_solo);
        assert!(cost.penalty_lamports > 0);
    }

    #[test]
    fn a_front_run_the_curve_refuses_is_not_a_smaller_one() {
        // A front-run bigger than the curve holds tokens for is not an
        // adversary at all, and the pricer says so rather than clamping.
        let curve = CurveState::at_real_sol(LAMPORTS_PER_SOL);
        assert_eq!(
            front_run_cost(&curve, u64::MAX, LAMPORTS_PER_SOL, &predator()),
            None
        );
    }

    #[test]
    fn a_parcel_is_worth_what_the_curve_would_pay_for_it() {
        let curve = mid_curve();
        assert_eq!(tokens_at_marginal(&curve, 0), 0);
        // The mid is above the executable price, because selling moves the
        // curve and the mid is the price before it moved.
        let tokens = curve
            .quote_buy(LAMPORTS_PER_SOL, DEFAULT_FEE_BPS)
            .expect("quote")
            .tokens;
        let mid = tokens_at_marginal(&curve, tokens);
        let executable = curve
            .quote_sell(tokens, DEFAULT_FEE_BPS)
            .expect("quote")
            .net_lamports;
        assert!(
            mid > executable,
            "the mid has to over-value or it is not a mid"
        );
        // An empty curve has no price and says zero rather than dividing.
        let empty = CurveState::from_parts(0, 0, 0, 0, 0, false);
        assert_eq!(tokens_at_marginal(&empty, 1_000), 0);
    }

    // -----------------------------------------------------------------------
    // re-orgs
    // -----------------------------------------------------------------------

    /// One SOL of real reserve, in lamports, as an `i64` of flow.
    const SOL_FLOW: i64 = LAMPORTS_PER_SOL as i64;

    /// A scenario against a curve in the middle of its life.
    fn scenario(side: Side, size: u64) -> ReorgScenario {
        ReorgScenario::untouched(30 * LAMPORTS_PER_SOL, side, size, 1_000_000)
    }

    /// The parcel a one-SOL buy gets on the curve those scenarios run against.
    fn mid_parcel() -> u64 {
        CurveState::at_real_sol(30 * LAMPORTS_PER_SOL)
            .quote_buy(LAMPORTS_PER_SOL, DEFAULT_FEE_BPS)
            .expect("quote")
            .tokens
    }

    #[test]
    fn a_chain_nobody_forked_costs_nothing_at_all() {
        // The control the whole sweep is read against. Not "small" — zero, on
        // every profile and both sides, because depth zero means the branch we
        // priced against is the branch that won.
        for profile in AdversaryProfile::ALL {
            let config = predator().with_profile(profile);
            for (side, size) in [(Side::Buy, LAMPORTS_PER_SOL), (Side::Sell, mid_parcel())] {
                let outcome = simulate_reorg(&scenario(side, size), &config);
                assert!(outcome.priced, "{profile:?} {side:?}");
                assert_eq!(outcome.fate, ReorgFate::Untouched, "{profile:?} {side:?}");
                assert_eq!(outcome.book_delta_lamports, 0, "{profile:?} {side:?}");
                assert_eq!(outcome.tip_refunded_lamports, 0, "{profile:?} {side:?}");
                assert_eq!(
                    outcome.canonical_book_lamports,
                    outcome.reorged_book_lamports
                );
                assert_eq!(outcome.canonical_tokens, outcome.reorged_tokens);
                assert!(outcome.synthetic);
            }
        }
    }

    #[test]
    fn the_replacement_flow_is_ignored_when_no_fork_reached_us() {
        // Depth zero is not a fork: nothing was rolled back, so a replacement
        // flow on the scenario is a description of a branch that never won.
        let config = AdversaryConfig::default();
        let quiet = scenario(Side::Buy, LAMPORTS_PER_SOL);
        let loud = ReorgScenario {
            replacement_flow_lamports: 20 * SOL_FLOW,
            ..quiet
        };

        // Everything computed has to match. The echoed scenario is the one
        // field that legitimately differs — it is the input, and the input is
        // what the two rows disagree about — so it is normalised away rather
        // than asserted on.
        let priced = simulate_reorg(&loud, &config);
        assert_eq!(
            ReorgOutcome {
                scenario: quiet,
                ..priced
            },
            simulate_reorg(&quiet, &config)
        );
        assert_eq!(
            priced.scenario, loud,
            "the row still says what it was asked"
        );
    }

    #[test]
    fn a_fork_that_bought_ahead_of_our_buy_costs_us_tokens() {
        let config = AdversaryConfig::default();
        let base = scenario(Side::Buy, LAMPORTS_PER_SOL);
        let forked = base.forked(4, 5 * SOL_FLOW, true);

        let outcome = simulate_reorg(&forked, &config);
        assert_eq!(outcome.fate, ReorgFate::Reincluded);
        assert!(outcome.priced);
        // A curve that somebody else pushed up before our order filled gives us
        // less of it for the same lamports.
        assert!(
            outcome.reorged_tokens < outcome.canonical_tokens,
            "{} tokens after the fork against {} before it",
            outcome.reorged_tokens,
            outcome.canonical_tokens
        );
        assert!(outcome.adverse());
        assert!(outcome.book_delta_lamports < 0);
        // The leg landed, so the bundle paid.
        assert_eq!(outcome.tip_refunded_lamports, 0);
    }

    #[test]
    fn a_fork_that_sold_ahead_of_our_buy_leaves_us_better_off() {
        let config = AdversaryConfig::default();
        let forked = scenario(Side::Buy, LAMPORTS_PER_SOL).forked(4, -5 * SOL_FLOW, true);
        let outcome = simulate_reorg(&forked, &config);
        assert_eq!(outcome.fate, ReorgFate::Reincluded);
        assert!(outcome.reorged_tokens > outcome.canonical_tokens);
        assert!(!outcome.adverse());
        assert!(outcome.book_delta_lamports > 0);
    }

    #[test]
    fn a_fork_that_sold_ahead_of_our_sell_costs_us_lamports() {
        let config = AdversaryConfig::default();
        let forked = scenario(Side::Sell, mid_parcel()).forked(4, -5 * SOL_FLOW, true);
        let outcome = simulate_reorg(&forked, &config);
        assert_eq!(outcome.fate, ReorgFate::Reincluded);
        assert!(
            outcome.reorged_net_lamports < outcome.canonical_net_lamports,
            "selling into a curve somebody already dumped on pays less"
        );
        assert!(outcome.adverse());
    }

    #[test]
    fn a_dropped_buy_holds_nothing_and_pays_nothing() {
        let config = AdversaryConfig::default();
        let forked = scenario(Side::Buy, LAMPORTS_PER_SOL).forked(2, 0, false);
        let outcome = simulate_reorg(&forked, &config);

        assert_eq!(outcome.fate, ReorgFate::Dropped);
        assert_eq!(outcome.reorged_tokens, 0);
        assert_eq!(outcome.reorged_net_lamports, 0);
        // Nothing was spent, so the book is zero exactly rather than nearly.
        assert_eq!(outcome.reorged_book_lamports, 0);
        // A bid that never landed never left the wallet.
        assert_eq!(outcome.tip_refunded_lamports, forked.tip_lamports);
        // And the entry that did land was under water by its round trip, which
        // is why this reads as favourable. See the note on `ReorgOutcome`: the
        // edge the entry was for is the one thing this module will not guess.
        assert!(outcome.canonical_book_lamports < 0);
        assert_eq!(
            outcome.book_delta_lamports,
            -outcome.canonical_book_lamports
        );
    }

    #[test]
    fn a_dropped_sell_is_still_holding_the_parcel() {
        let config = AdversaryConfig::default();
        let parcel = mid_parcel();
        let forked = scenario(Side::Sell, parcel).forked(2, 0, false);
        let outcome = simulate_reorg(&forked, &config);

        assert_eq!(outcome.fate, ReorgFate::Dropped);
        assert_eq!(outcome.reorged_net_lamports, 0);
        // Still holding, and the parcel is marked at what the ancestor would
        // actually pay for it rather than at the mid.
        let ancestor = CurveState::at_real_sol(30 * LAMPORTS_PER_SOL);
        let executable = ancestor
            .quote_sell(parcel, config.fee_bps)
            .expect("quote")
            .net_lamports;
        assert_eq!(outcome.reorged_book_lamports, executable as i64);
        assert_eq!(outcome.tip_refunded_lamports, forked.tip_lamports);
        // With no flow on either branch the whole difference is the tip we did
        // not pay: the exit is still ahead of us either way.
        assert_eq!(
            outcome.book_delta_lamports,
            i64::from(forked.tip_lamports as u32)
        );
    }

    #[test]
    fn a_fork_that_graduates_the_curve_refuses_our_leg_rather_than_filling_it() {
        // The winning branch buys the curve past the graduation line. §17 makes
        // that a hard branch, not a worse price, and the fate says so.
        let config = AdversaryConfig::default();
        let base = ReorgScenario::untouched(
            80 * LAMPORTS_PER_SOL,
            Side::Buy,
            LAMPORTS_PER_SOL,
            1_000_000,
        );
        let forked = base.forked(8, 10 * SOL_FLOW, true);
        let outcome = simulate_reorg(&forked, &config);

        assert!(outcome.priced, "the branch we priced against was fine");
        assert_eq!(outcome.fate, ReorgFate::Refused);
        assert_eq!(outcome.reorged_tokens, 0);
        assert_eq!(outcome.tip_refunded_lamports, forked.tip_lamports);
    }

    #[test]
    fn a_scenario_with_no_baseline_says_so_rather_than_reporting_a_loss() {
        // The branch we priced against has itself graduated, so there is no
        // fill for the fork to have changed. Every column is zero and `priced`
        // carries the difference between "nothing happened" and "nothing to
        // say" — the same distinction `MevOutcome::bounded` makes.
        let config = AdversaryConfig::default();
        let mut base = ReorgScenario::untouched(
            80 * LAMPORTS_PER_SOL,
            Side::Buy,
            LAMPORTS_PER_SOL,
            1_000_000,
        );
        base.canonical_flow_lamports = 10 * SOL_FLOW;
        let outcome = simulate_reorg(&base, &config);

        assert!(!outcome.priced);
        assert_eq!(outcome.fate, ReorgFate::Refused);
        assert_eq!(outcome.book_delta_lamports, 0);
        assert_eq!(outcome.canonical_book_lamports, 0);
        assert_eq!(outcome.tip_refunded_lamports, 0);
    }

    #[test]
    fn the_same_fork_prices_the_same_way_twice() {
        let config = predator();
        let forked = ReorgScenario {
            volatility_micros: 42_000,
            ..scenario(Side::Buy, LAMPORTS_PER_SOL).forked(3, 2 * SOL_FLOW, true)
        };
        assert_eq!(
            simulate_reorg(&forked, &config),
            simulate_reorg(&forked, &config)
        );
    }

    #[test]
    fn the_fates_partition_the_scenarios() {
        let config = predator();
        let scenarios = ReorgGrid::standard().scenarios(&config);
        assert!(
            scenarios.len() > 500,
            "a sweep of {} is not a sweep",
            scenarios.len()
        );

        let summary = sweep_reorgs(&scenarios, &config);
        assert_eq!(summary.scenarios as usize, scenarios.len());
        // Every scenario landed in exactly one bucket, and the unpriced ones
        // are counted rather than silently dropped.
        assert_eq!(
            summary.untouched
                + summary.reincluded
                + summary.dropped
                + summary.refused
                + summary.unpriced,
            summary.scenarios
        );
        assert!(summary.adverse + summary.favourable <= summary.scenarios);
        assert_eq!(summary.profile, config.profile);
        assert!(summary.synthetic);
        assert_eq!(summary.max_depth_slots, 8);
    }

    #[test]
    fn the_sweep_is_the_fold_of_the_outcomes_it_prices() {
        let config = predator();
        let scenarios = ReorgGrid::quiet().scenarios(&config);
        let outcomes: Vec<ReorgOutcome> = scenarios
            .iter()
            .map(|scenario| simulate_reorg(scenario, &config))
            .collect();
        // The streaming fold and the slice fold are the same arithmetic, which
        // is what lets a sweep too large to hold be summarised as it runs.
        assert_eq!(
            sweep_reorgs(&scenarios, &config),
            ReorgSummary::of(config.profile, &outcomes)
        );
    }

    #[test]
    fn a_grid_with_no_forks_in_it_has_no_losses_in_it() {
        for profile in AdversaryProfile::ALL {
            let config = predator().with_profile(profile);
            let summary = ReorgGrid::quiet().sweep(&config);
            assert!(summary.scenarios > 0);
            assert_eq!(summary.adverse, 0, "{profile:?}");
            assert_eq!(summary.favourable, 0, "{profile:?}");
            assert_eq!(summary.total_loss_lamports, 0, "{profile:?}");
            assert_eq!(summary.total_gain_lamports, 0, "{profile:?}");
            assert_eq!(summary.net_delta_lamports, 0, "{profile:?}");
            assert_eq!(summary.worst_delta_lamports, 0, "{profile:?}");
            assert_eq!(summary.mean_adverse_loss_lamports, 0, "{profile:?}");
            assert_eq!(summary.tips_refunded_lamports, 0, "{profile:?}");
            assert_eq!(summary.dropped, 0, "{profile:?}");
        }
    }

    #[test]
    fn the_grid_enumerates_the_same_scenarios_in_the_same_order_every_time() {
        let config = predator();
        let grid = ReorgGrid::standard();
        assert_eq!(grid.scenarios(&config), grid.scenarios(&config));
        assert!(grid.scenarios(&config).len() <= grid.upper_bound());
        // And the sweep over them is a function of the grid alone.
        assert_eq!(grid.sweep(&config), grid.sweep(&config));
    }

    #[test]
    fn the_mean_adverse_loss_is_taken_over_the_adverse_scenarios_only() {
        let config = AdversaryConfig::default();
        let summary = ReorgGrid::standard().sweep(&config);
        assert!(
            summary.adverse > 0,
            "the standard grid has to contain a loss"
        );
        assert_eq!(
            summary.mean_adverse_loss_lamports,
            summary.total_loss_lamports / u64::from(summary.adverse)
        );
        // Padding a sweep with controls must not move it: the mean is over the
        // losses, so a thousand untouched rows change the count and not this.
        let padded: Vec<ReorgScenario> = ReorgGrid::standard()
            .scenarios(&config)
            .into_iter()
            .chain(ReorgGrid::quiet().scenarios(&config))
            .collect();
        let padded = sweep_reorgs(&padded, &config);
        assert!(padded.scenarios > summary.scenarios);
        assert_eq!(
            padded.mean_adverse_loss_lamports,
            summary.mean_adverse_loss_lamports
        );
    }

    #[test]
    fn the_worst_scenario_is_the_one_the_worst_delta_came_from() {
        let config = AdversaryConfig::default();
        let scenarios = ReorgGrid::standard().scenarios(&config);
        let summary = sweep_reorgs(&scenarios, &config);
        let worst = summary.worst_scenario.expect("a non-empty sweep names one");
        assert_eq!(
            simulate_reorg(&worst, &config).book_delta_lamports,
            summary.worst_delta_lamports
        );
        // Nothing in the sweep is worse than the one it reported.
        for scenario in &scenarios {
            assert!(
                simulate_reorg(scenario, &config).book_delta_lamports
                    >= summary.worst_delta_lamports
            );
        }
    }

    #[test]
    fn a_sampled_sweep_is_a_function_of_its_seed_and_nothing_else() {
        let config = predator();
        let grid = ReorgGrid::standard();
        assert_eq!(
            grid.sampled(&config, 42, 200),
            grid.sampled(&config, 42, 200)
        );
        assert_ne!(
            grid.sampled(&config, 42, 200),
            grid.sampled(&config, 43, 200)
        );
        // A sample is a prefix-free address, not a walk: asking for two hundred
        // gives the same first hundred as asking for a hundred, so a failure at
        // index 190 reproduces without replaying the 189 before it.
        let long = grid.sampled(&config, 42, 200);
        let short = grid.sampled(&config, 42, 100);
        assert_eq!(&long[..short.len()], &short[..]);
        assert!(long.len() <= 200);
    }

    #[test]
    fn the_mixer_is_a_pure_function_of_two_integers() {
        assert_eq!(mix64(0, 0), mix64(0, 0));
        assert_ne!(mix64(0, 0), mix64(0, 1));
        assert_ne!(mix64(0, 0), mix64(1, 0));
        // Enough spread that a modulus against a short axis is not a constant.
        let picks: std::collections::BTreeSet<u64> =
            (0..64u64).map(|index| mix64(7, index) % 5).collect();
        assert_eq!(picks.len(), 5, "the mixer collapsed onto one bucket");
    }

    #[test]
    fn a_sampled_sweep_prices_every_scenario_it_returns() {
        // The sampler drops what the curve refuses to size rather than
        // returning a scenario nobody can price, so everything that comes back
        // has a baseline — a sweep whose rows are mostly unpriceable is a sweep
        // of the wrong curve positions.
        let config = predator();
        let scenarios = ReorgGrid::standard().sampled(&config, 2024, 400);
        assert!(scenarios.len() > 300, "{} survived sizing", scenarios.len());
        let summary = sweep_reorgs(&scenarios, &config);
        assert!(
            summary.unpriced * 2 < summary.scenarios,
            "{} of {} scenarios had no baseline",
            summary.unpriced,
            summary.scenarios
        );
    }

    #[test]
    fn an_empty_sweep_is_the_empty_summary() {
        let summary = sweep_reorgs(&[], &predator());
        assert_eq!(
            summary,
            ReorgSummary::empty(AdversaryProfile::PredatorySandwich)
        );
        assert_eq!(summary.worst_scenario, None);
    }

    #[test]
    fn every_fate_says_whether_the_leg_filled() {
        assert!(ReorgFate::Untouched.filled());
        assert!(ReorgFate::Reincluded.filled());
        assert!(!ReorgFate::Dropped.filled());
        assert!(!ReorgFate::Refused.filled());
        // The names round-trip into a report and back out of one.
        for fate in ReorgFate::ALL {
            let text = serde_json::to_string(&fate).expect("serialises");
            assert_eq!(text, format!("\"{}\"", fate.as_str()));
        }
    }

    // -----------------------------------------------------------------------
    // one adversary, several curves
    // -----------------------------------------------------------------------

    /// Three curves at three points of their life, each with a public buy in
    /// front of it worth wrapping.
    fn venue() -> Vec<PoolTarget> {
        vec![
            PoolTarget::at_real_sol("MintCharlie", 60 * LAMPORTS_PER_SOL, 3 * LAMPORTS_PER_SOL),
            PoolTarget::at_real_sol("MintAlpha", 20 * LAMPORTS_PER_SOL, 2 * LAMPORTS_PER_SOL),
            PoolTarget::at_real_sol("MintBravo", 40 * LAMPORTS_PER_SOL, LAMPORTS_PER_SOL),
        ]
    }

    fn multi_pool_predator() -> AdversaryConfig {
        let mut config = predator().bounded(8 * LAMPORTS_PER_SOL, BPS_DENOMINATOR as u16);
        config.base_intensity_micros = MICROS;
        config
    }

    #[test]
    fn a_passive_taker_works_no_curve_at_all() {
        let config = AdversaryConfig::default();
        let report = extract_across_pools(&venue(), &config, DEFAULT_ALLOCATION_SLICES);

        assert_eq!(report.pools_offered, 3);
        assert_eq!(report.pools_attacked, 0);
        assert_eq!(report.pools_viable, 0);
        assert_eq!(report.capital_deployed_lamports, 0);
        assert_eq!(report.capital_idle_lamports, config.capital_lamports);
        assert_eq!(report.total_profit_lamports, 0);
        // The rows are still there. "Nobody was there" is a result, and a
        // report that omitted the pools would not diff against one where
        // somebody was.
        assert_eq!(report.rows.len(), 3);
        assert!(report.rows.iter().all(|row| !row.attacked()));
        assert!(report.balances());
    }

    #[test]
    fn the_purse_is_conserved_whatever_the_allocation_does() {
        for slices in [0u32, 1, 4, DEFAULT_ALLOCATION_SLICES, 64] {
            for profile in AdversaryProfile::ALL {
                let config = multi_pool_predator().with_profile(profile);
                let report = extract_across_pools(&venue(), &config, slices);
                assert!(
                    report.balances(),
                    "{profile:?} at {slices} slices: {} deployed and {} idle against {}",
                    report.capital_deployed_lamports,
                    report.capital_idle_lamports,
                    report.capital_lamports
                );
                assert!(report.capital_deployed_lamports <= config.capital_lamports);
            }
        }
    }

    #[test]
    fn no_curve_gets_more_than_the_profile_would_put_into_it() {
        let config = multi_pool_predator();
        let report = extract_across_pools(&venue(), &config, DEFAULT_ALLOCATION_SLICES);
        assert!(
            report.pools_attacked > 0,
            "the allocator has to do something"
        );
        for row in &report.rows {
            assert!(
                row.attacker_lamports <= row.capacity_lamports,
                "{}: {} allocated against a capacity of {}",
                row.mint,
                row.attacker_lamports,
                row.capacity_lamports
            );
        }
    }

    #[test]
    fn the_allocation_goes_where_the_extraction_is() {
        // One deep curve with a large public buy in front of it, and one thin
        // curve with dust. The purse should end up on the first.
        let pools = vec![
            PoolTarget::at_real_sol("MintFat", 50 * LAMPORTS_PER_SOL, 5 * LAMPORTS_PER_SOL),
            PoolTarget::at_real_sol("MintThin", 50 * LAMPORTS_PER_SOL, 20_000),
        ];
        let config = multi_pool_predator();
        let report = extract_across_pools(&pools, &config, DEFAULT_ALLOCATION_SLICES);

        let fat = report
            .rows
            .iter()
            .find(|row| row.mint == "MintFat")
            .expect("row");
        let thin = report
            .rows
            .iter()
            .find(|row| row.mint == "MintThin")
            .expect("row");
        assert!(fat.attacker_lamports > thin.attacker_lamports);
        assert!(fat.attacked());
        // A dust buy is below §15.2's threshold, so no front-run of any size
        // pays and the allocator correctly leaves it alone.
        assert!(!thin.viable);
        assert!(!thin.attacked());
    }

    #[test]
    fn a_venue_with_nothing_worth_attacking_leaves_the_purse_at_home() {
        // Every buy in front of us is dust against a deep reserve, which is
        // §15.2's answer rather than a small one.
        let pools = vec![
            PoolTarget::at_real_sol("MintOne", 80 * LAMPORTS_PER_SOL, 10_000),
            PoolTarget::at_real_sol("MintTwo", 80 * LAMPORTS_PER_SOL, 20_000),
        ];
        let config = multi_pool_predator();
        let report = extract_across_pools(&pools, &config, DEFAULT_ALLOCATION_SLICES);

        assert_eq!(report.pools_viable, 0);
        assert_eq!(report.pools_attacked, 0);
        assert_eq!(report.capital_idle_lamports, config.capital_lamports);
        assert_eq!(report.total_profit_lamports, 0);
        assert!(report.balances());
    }

    #[test]
    fn an_allocator_with_no_slices_allocates_nothing() {
        let config = multi_pool_predator();
        let report = extract_across_pools(&venue(), &config, 0);
        assert_eq!(report.slice_lamports, 0);
        assert_eq!(report.capital_deployed_lamports, 0);
        assert_eq!(report.capital_idle_lamports, config.capital_lamports);
        assert!(report.balances());
    }

    #[test]
    fn a_slice_too_small_to_be_a_trade_is_not_one() {
        // Cutting a small purse into many slices makes each one smaller than
        // the two signatures a front-run has to pay for. §15.2's floor applies
        // to a slice as much as to a whole position.
        let mut config = multi_pool_predator();
        config.capital_lamports = MIN_VIABLE_ATTACKER_LAMPORTS;
        let report = extract_across_pools(&venue(), &config, 64);
        assert!(report.slice_lamports < MIN_VIABLE_ATTACKER_LAMPORTS);
        assert_eq!(report.capital_deployed_lamports, 0);
        assert!(report.balances());
    }

    #[test]
    fn every_row_stays_inside_the_bound_the_report_declares() {
        for max_penalty_bps in [10u16, 100, 1_500, BPS_DENOMINATOR as u16] {
            let config = multi_pool_predator().bounded(8 * LAMPORTS_PER_SOL, max_penalty_bps);
            let report = extract_across_pools(&venue(), &config, DEFAULT_ALLOCATION_SLICES);
            assert_eq!(report.max_penalty_bps, max_penalty_bps);
            for row in &report.rows {
                assert!(
                    row.victim_penalty_bps <= max_penalty_bps,
                    "{}: {} bps over a {} bps ceiling",
                    row.mint,
                    row.victim_penalty_bps,
                    max_penalty_bps
                );
            }
            assert!(report.worst_victim_penalty_bps <= max_penalty_bps);
            assert!(report.mean_victim_penalty_bps <= report.worst_victim_penalty_bps);
        }
    }

    #[test]
    fn a_tighter_bound_deploys_less_and_says_it_was_cut_back() {
        let loose = multi_pool_predator().bounded(8 * LAMPORTS_PER_SOL, BPS_DENOMINATOR as u16);
        let tight = multi_pool_predator().bounded(8 * LAMPORTS_PER_SOL, 10);
        let wide = extract_across_pools(&venue(), &loose, DEFAULT_ALLOCATION_SLICES);
        let narrow = extract_across_pools(&venue(), &tight, DEFAULT_ALLOCATION_SLICES);

        assert!(
            wide.worst_victim_penalty_bps > tight.max_penalty_bps,
            "the bound has to bite"
        );
        assert!(narrow.capital_deployed_lamports < wide.capital_deployed_lamports);
        assert!(
            narrow.pools_bounded > 0,
            "the report has to say which curves were cut back"
        );
    }

    #[test]
    fn the_rows_come_back_sorted_by_mint_whatever_order_they_went_in() {
        let config = multi_pool_predator();
        let report = extract_across_pools(&venue(), &config, DEFAULT_ALLOCATION_SLICES);
        let mints: Vec<&str> = report.rows.iter().map(|row| row.mint.as_str()).collect();
        assert_eq!(mints, ["MintAlpha", "MintBravo", "MintCharlie"]);
    }

    #[test]
    fn the_same_venue_allocates_the_same_way_twice() {
        let config = multi_pool_predator();
        assert_eq!(
            extract_across_pools(&venue(), &config, DEFAULT_ALLOCATION_SLICES),
            extract_across_pools(&venue(), &config, DEFAULT_ALLOCATION_SLICES)
        );
    }

    #[test]
    fn the_totals_are_the_sum_of_the_rows() {
        let config = multi_pool_predator();
        let report = extract_across_pools(&venue(), &config, DEFAULT_ALLOCATION_SLICES);
        let profit: i64 = report
            .rows
            .iter()
            .map(|row| row.attacker_profit_lamports)
            .sum();
        let extraction: i64 = report.rows.iter().map(|row| row.extraction_lamports).sum();
        let penalty: u64 = report
            .rows
            .iter()
            .map(|row| row.victim_penalty_lamports)
            .sum();

        assert_eq!(report.total_profit_lamports, profit);
        assert_eq!(report.total_extraction_lamports, extraction);
        assert_eq!(report.total_victim_penalty_lamports, penalty);
        assert_eq!(
            report.pools_attacked as usize,
            report.rows.iter().filter(|row| row.attacked()).count()
        );
        // A row that was worked has to have paid for itself: the allocator
        // only ever assigns a slice whose marginal gain was positive.
        assert!(report
            .rows
            .iter()
            .filter(|row| row.attacked())
            .all(|row| row.attacker_profit_lamports > 0));
        assert!(report.synthetic);
    }

    #[test]
    fn more_slices_never_extract_less_than_one() {
        // One slice puts the whole purse on one curve. More slices can only
        // help, because the allocator can always choose to do the same thing.
        let config = multi_pool_predator();
        let coarse = extract_across_pools(&venue(), &config, 1);
        let fine = extract_across_pools(&venue(), &config, DEFAULT_ALLOCATION_SLICES);
        assert!(
            fine.total_profit_lamports >= coarse.total_profit_lamports,
            "{} at sixteen slices against {} at one",
            fine.total_profit_lamports,
            coarse.total_profit_lamports
        );
    }

    #[test]
    fn an_empty_venue_is_an_empty_report() {
        let config = multi_pool_predator();
        let report = extract_across_pools(&[], &config, DEFAULT_ALLOCATION_SLICES);
        assert_eq!(report.pools_offered, 0);
        assert_eq!(report.rows, Vec::new());
        assert_eq!(report.capital_idle_lamports, config.capital_lamports);
        assert!(report.balances());
    }
}
