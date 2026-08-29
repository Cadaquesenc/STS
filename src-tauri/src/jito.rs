//! What a bundle has to pay to land, priced from what the last few slots did.
//!
//! `execution.rs` already knows how to bid: [`TipPolicy`](crate::execution::TipPolicy)
//! is Annex C's `Tip_base + α × EV_net`, escalating a fixed step per retry and
//! clamped into `[Tip_base, Tip_max]`. What it does not know is where
//! `Tip_base` should be, and it says so — the doc on that struct is explicit
//! that the terms Annex C's full `α_eff` expansion adds are "an observation of
//! a network this build does not talk to", and that a coefficient multiplied by
//! a number nobody measured is not a better tip.
//!
//! This module is where those observations land when somebody does measure
//! them. It moves the *floor* and leaves the bid alone: a policy still
//! escalates per retry, still refuses a discretionary tip that would eat the
//! edge, still clamps at `Tip_max`. All that changes is that the number it
//! starts from is the one the last thirty-two slots actually cleared instead of
//! a constant chosen in advance.
//!
//! # The two distances
//!
//! Both halves of the floor are weighted by a distance measured in slots, and
//! they are not the same distance.
//!
//! **How far back an observation is.** A slot that closed thirty slots ago is
//! evidence about a market that has since turned over. It is not worthless, and
//! it is not worth what the slot that closed a moment ago is worth, so each one
//! is weighted `decay^d` for `d` slots of age and the window ends where the
//! weight stops mattering. This is what makes the floor track a congestion
//! spike within a few slots instead of averaging it away.
//!
//! **How far ahead the leader is.** A bundle sent when a connected block engine
//! leads *now* is competing with every other searcher who noticed the same
//! thing. One sent eight slots early is competing with nobody, because it will
//! not be forwarded yet. [`LeaderHint`] carries which of those it is and the
//! proximity term decays the same way over the gap.
//!
//! # Nothing here observes anything
//!
//! [`CongestionWindow`] is fed; it does not read. The same seam
//! [`LeaderSchedule`](crate::execution::LeaderSchedule) is — a port with
//! nothing behind it in this build, because answering it needs a block engine's
//! bundle stream and this crate has no HTTP client in its dependencies. What
//! the seam buys is that the pricing path already asks the question, so a live
//! backend adds an answer rather than a branch, and that the arithmetic between
//! the answer and the lamport is written down and tested now rather than
//! improvised later against a live wallet.
//!
//! With no observations at all the floor is [`TipFloorParams::min_lamports`],
//! which defaults to the same `EXIT_TIP_BASE_LAMPORTS` the static policy uses.
//! An unfitted window therefore prices exactly what this build priced before
//! it existed, which is the property that makes turning it on safe.
//!
//! # No floating point, anywhere
//!
//! Every number below is an integer. The weights are [`crate::fixed`] at
//! `10^-18`, the ratios are millionths, money is lamports and time is
//! milliseconds. `tip_floor_arithmetic_uses_no_floating_point` reads this
//! file's own source and fails on an `f64` in it, for the reason the strategy
//! module gives about its scores: a tip that is stored, compared and replayed
//! must not depend on whose libm the build linked against, and a bundle whose
//! bid differed in the last digit between the machine that recorded a fixture
//! and the machine that replays it is a fixture that proves nothing.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::backtest::MICROS;
use crate::execution::{
    LeaderHint, EXIT_TIP_BASE_LAMPORTS, EXIT_TIP_MAX_LAMPORTS, JITO_MIN_TIP_LAMPORTS,
};
use crate::fixed::{self, ONE};

// ---------------------------------------------------------------------------
// the shape of the clock
// ---------------------------------------------------------------------------

/// A slot, in milliseconds. Solana's target, and what `fixtures.rs` steps its
/// synthetic clock by, so a fixture second and a real second are the same
/// number of slots.
pub const SLOT_MS: u64 = 400;

/// How many consecutive slots one leader holds.
///
/// Four, which is the cluster's rotation and the reason
/// [`bundle`](crate::bundle) has a leader-boundary eviction at all: a block
/// engine forwards to the validator it is connected to *now*, so a bundle still
/// queued when the rotation turns is not late, it is addressed to somebody who
/// is no longer listening.
pub const LEADER_SLOTS_PER_ROTATION: u64 = 4;

/// Which rotation a slot belongs to. Two slots in the same rotation have the
/// same leader; two in different rotations do not.
pub fn leader_rotation(slot: u64) -> u64 {
    slot / LEADER_SLOTS_PER_ROTATION
}

// ---------------------------------------------------------------------------
// what a slot said
// ---------------------------------------------------------------------------

/// One slot's evidence, as a caller that can see the chain reports it.
///
/// Everything on it is a count of something countable. There is no "congestion
/// score" field because a score is a conclusion, and the conclusion is what
/// this module computes rather than what it is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotObservation {
    pub slot: u64,
    /// Compute units the block actually packed.
    pub compute_units_used: u64,
    /// The block's ceiling. Zero means the reporter did not know it, and a
    /// saturation of "used out of nothing" is read as zero rather than as full
    /// — [`fixed::ratio`] returns zero on a zero denominator and that is the
    /// right answer here for the same reason it is there.
    pub compute_unit_limit: u64,
    /// What the cheapest bundle that actually landed in this slot tipped.
    ///
    /// The floor, not the average: the question this module answers is what it
    /// would have taken to get in, and the mean of what everybody paid is a
    /// number nobody had to pay. A slot where nothing landed reports zero and
    /// contributes weight to the saturation term without dragging the floor
    /// down, which is handled in [`CongestionWindow::weighted`].
    pub landed_floor_lamports: u64,
    pub bundles_landed: u32,
    /// Bundles the engine forwarded for this slot, landed or not. Never below
    /// `bundles_landed` in well-formed evidence; clamped rather than trusted,
    /// since a land rate above one would be a nonsense the cockpit renders.
    pub bundles_seen: u32,
}

impl SlotObservation {
    /// An empty slot: nothing landed, nothing was seen, the block was idle.
    ///
    /// Useful as a base to override fields on, and meaningful on its own — a
    /// run of these is what a quiet market looks like and the floor should fall
    /// through it.
    pub fn idle(slot: u64) -> Self {
        SlotObservation {
            slot,
            compute_units_used: 0,
            compute_unit_limit: 0,
            landed_floor_lamports: 0,
            bundles_landed: 0,
            bundles_seen: 0,
        }
    }

    /// How full the block was, at `10^-18`, capped at one.
    ///
    /// Capped rather than trusted: a reporter that counted units against a
    /// stale limit can hand back more than the ceiling, and a saturation above
    /// one would push the floor past what any real congestion justifies.
    fn saturation(&self) -> u128 {
        fixed::ratio(self.compute_units_used, self.compute_unit_limit).min(ONE)
    }
}

// ---------------------------------------------------------------------------
// the parameters
// ---------------------------------------------------------------------------

/// How far back the window looks. Thirty-two slots is about thirteen seconds,
/// which is eight leader rotations — long enough that one unlucky leader does
/// not set the price and short enough that the price is still about now.
pub const CONGESTION_WINDOW_SLOTS: u64 = 32;

/// What one slot of age costs an observation, in millionths.
///
/// At `0.85` a slot, evidence from eight slots back carries a quarter of the
/// weight of evidence from this one and the far edge of a 32-slot window
/// carries about half a percent. That is the shape wanted: the window is wide
/// enough to be stable and steep enough that a spike moves the floor inside a
/// leader rotation rather than after it.
pub const SLOT_DECAY_MICROS: u64 = 850_000;

/// What a full block adds to the floor, in millionths.
///
/// A block at its compute ceiling for the whole window prices half again what
/// the same window would price idle. Half rather than more because saturation
/// is evidence that landing is contested, not evidence of what the contest
/// costs — the contest's actual price is already in `landed_floor_lamports`,
/// and double-counting it is how a tip policy runs away from itself.
pub const SATURATION_GAIN_MICROS: u64 = 500_000;

/// What a leader that is up *now* adds, in millionths.
///
/// The largest of the three terms, and deliberately: this is the only one that
/// is about our own bundle rather than about the market. Everything in the
/// window happened whether we sent or not; the leader being up now is what
/// decides whether the next few hundred milliseconds are the contested ones.
pub const PROXIMITY_GAIN_MICROS: u64 = 750_000;

/// What one slot of distance to the leader costs the proximity term, in
/// millionths.
///
/// Steeper than the observation decay. A leader four slots out is a whole
/// rotation away and the bundle will be re-priced at least once before it is
/// forwarded, so paying now for a contest that has not started is paying twice.
pub const PROXIMITY_DECAY_MICROS: u64 = 600_000;

/// Everything the floor is computed from that is a choice rather than a
/// measurement.
///
/// `Copy` and `Eq` so a caller can hold one, hand it around and compare what it
/// got back with what it set — and so a test can state a whole configuration on
/// one line rather than mutating a default and hoping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TipFloorParams {
    pub window_slots: u64,
    pub slot_decay_micros: u64,
    pub saturation_gain_micros: u64,
    pub proximity_gain_micros: u64,
    pub proximity_decay_micros: u64,
    /// The floor's own floor, and the answer when nothing has been observed.
    pub min_lamports: u64,
    /// The floor's ceiling. Not `Tip_max` — this bounds where a bid *starts*,
    /// and Annex C's ceiling still bounds where it ends.
    pub max_lamports: u64,
}

impl Default for TipFloorParams {
    /// The published constants above, and Annex C's `Tip_base` and `Tip_max` as
    /// the bounds.
    ///
    /// The minimum matching `EXIT_TIP_BASE_LAMPORTS` is what makes an unfitted
    /// window price exactly what the static policy priced: with no observations
    /// the weighted floor is zero, the clamp lifts it to the minimum, and the
    /// minimum is the constant `TipPolicy::emergency` was already using.
    fn default() -> Self {
        TipFloorParams {
            window_slots: CONGESTION_WINDOW_SLOTS,
            slot_decay_micros: SLOT_DECAY_MICROS,
            saturation_gain_micros: SATURATION_GAIN_MICROS,
            proximity_gain_micros: PROXIMITY_GAIN_MICROS,
            proximity_decay_micros: PROXIMITY_DECAY_MICROS,
            min_lamports: EXIT_TIP_BASE_LAMPORTS,
            max_lamports: EXIT_TIP_MAX_LAMPORTS,
        }
    }
}

impl TipFloorParams {
    /// Says why these parameters could never produce a usable floor, or
    /// nothing.
    ///
    /// Checked where they are used rather than where they are built, for the
    /// reason [`TipPolicy::malformed`](crate::execution::TipPolicy) gives about
    /// its own: the fields are public, so a value can be edited into an
    /// impossible one after construction and the only moment that is certainly
    /// catchable is the moment it is read.
    pub fn malformed(&self) -> Option<String> {
        if self.window_slots == 0 {
            return Some(
                "a window of no slots has nothing to weigh, so every floor it priced would be \
                 the minimum with a congestion term attached to no evidence"
                    .to_string(),
            );
        }
        if self.slot_decay_micros == 0 || self.slot_decay_micros > MICROS {
            return Some(format!(
                "a slot decay of {} millionths is not a decay: it has to sit in (0, 1] for an \
                 older slot to count for less than a newer one",
                self.slot_decay_micros
            ));
        }
        if self.proximity_decay_micros == 0 || self.proximity_decay_micros > MICROS {
            return Some(format!(
                "a proximity decay of {} millionths is not a decay: it has to sit in (0, 1] for \
                 a leader further out to matter less than one that is up now",
                self.proximity_decay_micros
            ));
        }
        if self.max_lamports < self.min_lamports {
            return Some(format!(
                "a floor ceiling of {} lamports is below the floor minimum of {}, so no floor \
                 satisfies both",
                self.max_lamports, self.min_lamports
            ));
        }
        if self.max_lamports < JITO_MIN_TIP_LAMPORTS {
            return Some(format!(
                "a floor ceiling of {} lamports is under the {JITO_MIN_TIP_LAMPORTS} a block \
                 engine will look at, so every bundle it priced would be ignored",
                self.max_lamports
            ));
        }
        None
    }
}

// ---------------------------------------------------------------------------
// the window
// ---------------------------------------------------------------------------

/// The last `window_slots` slots of evidence, and the weighted reduction of it.
///
/// Keyed by slot in a `BTreeMap` rather than pushed onto a ring, because the
/// order the observations *arrive* in is not guaranteed to be the order the
/// slots *closed* in — a reporter catching up after a stall delivers a burst —
/// and every number this module produces has to be a function of the set of
/// observations rather than of their arrival order. Two windows holding the
/// same slots price the same floor whatever sequence they were fed in, which is
/// the property `a_window_prices_the_same_floor_whatever_order_it_was_fed_in`
/// pins.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CongestionWindow {
    slots: BTreeMap<u64, SlotObservation>,
    head_slot: u64,
}

/// The weighted reduction of a window: everything the floor needs from the
/// evidence, before any of the policy's own coefficients are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowSummary {
    /// The newest slot the window has seen. Zero when it has seen none.
    pub head_slot: u64,
    /// How many slots are inside the window and carry weight.
    pub slots_observed: u32,
    /// The weighted mean of the per-slot landed floors, in lamports. Taken over
    /// the slots where something landed, so a quiet slot does not vote the
    /// price down to nothing.
    pub observed_floor_lamports: u64,
    /// The weighted mean compute saturation, in millionths, over every slot in
    /// the window including the quiet ones — an idle block is evidence, and it
    /// is evidence of the opposite thing.
    pub saturation_micros: u64,
    /// Bundles that landed over bundles forwarded, weighted the same way, in
    /// millionths. `None` when the window saw no bundles at all, which is not
    /// the same as a window where none of them landed.
    pub land_rate_micros: Option<u64>,
}

impl CongestionWindow {
    pub fn new() -> Self {
        Self::default()
    }

    /// The newest slot observed. Zero before anything has been.
    pub fn head_slot(&self) -> u64 {
        self.head_slot
    }

    /// How many observations are being kept.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Takes one slot's evidence.
    ///
    /// Re-observing a slot replaces what was there. A reporter that revises a
    /// slot — a block that was still filling when it was first read — is
    /// telling the truth later, and holding both would weigh one slot twice.
    ///
    /// An observation older than the window is dropped on the spot rather than
    /// stored and filtered later, so the map's size is bounded by the window
    /// whatever a catching-up reporter does. One *newer* than the head advances
    /// the head and evicts whatever that leaves behind.
    pub fn observe(&mut self, observation: SlotObservation, params: &TipFloorParams) {
        let window = params.window_slots.max(1);
        if observation.slot > self.head_slot {
            self.head_slot = observation.slot;
        }
        if self.head_slot.saturating_sub(observation.slot) >= window {
            return;
        }
        self.slots.insert(observation.slot, observation);
        self.prune(params);
    }

    /// Moves the head forward without adding evidence.
    ///
    /// A slot that produced no observation still ages every observation before
    /// it. Without this, a window that stopped being fed would keep pricing off
    /// stale slots at full weight forever, which is the failure mode where a
    /// congestion spike that ended ten seconds ago is still being paid for.
    pub fn advance_to(&mut self, slot: u64, params: &TipFloorParams) {
        if slot > self.head_slot {
            self.head_slot = slot;
            self.prune(params);
        }
    }

    /// Drops everything the window has aged out of.
    fn prune(&mut self, params: &TipFloorParams) {
        let window = params.window_slots.max(1);
        let oldest = self.head_slot.saturating_sub(window - 1);
        self.slots.retain(|&slot, _| slot >= oldest);
    }

    /// The weight one slot carries: `decay^age`, at `10^-18`.
    fn weight(&self, slot: u64, params: &TipFloorParams) -> u128 {
        let age = self.head_slot.saturating_sub(slot);
        let decay = fixed::from_micros(params.slot_decay_micros.min(MICROS));
        fixed::pow(decay, u32::try_from(age).unwrap_or(u32::MAX))
    }

    /// Everything the floor needs from the evidence.
    ///
    /// Three weighted means over one pass. The floor's denominator counts only
    /// the slots where something landed and the saturation's counts every slot,
    /// which is the one asymmetry here and the comment on
    /// [`SlotObservation::landed_floor_lamports`] is why.
    pub fn weighted(&self, params: &TipFloorParams) -> WindowSummary {
        let mut floor_numerator: u128 = 0;
        let mut floor_denominator: u128 = 0;
        let mut saturation_numerator: u128 = 0;
        let mut saturation_denominator: u128 = 0;
        let mut landed_numerator: u128 = 0;
        let mut seen_numerator: u128 = 0;

        for observation in self.slots.values() {
            let weight = self.weight(observation.slot, params);
            if weight == 0 {
                continue;
            }

            saturation_numerator =
                saturation_numerator.saturating_add(fixed::mul(weight, observation.saturation()));
            saturation_denominator = saturation_denominator.saturating_add(weight);

            if observation.landed_floor_lamports > 0 {
                floor_numerator = floor_numerator.saturating_add(
                    weight.saturating_mul(u128::from(observation.landed_floor_lamports)),
                );
                floor_denominator = floor_denominator.saturating_add(weight);
            }

            // A reporter that counted more landings than forwards is clamped
            // rather than believed, so the rate cannot leave [0, 1].
            let landed = u64::from(observation.bundles_landed);
            let seen = u64::from(observation.bundles_seen).max(landed);
            landed_numerator =
                landed_numerator.saturating_add(weight.saturating_mul(u128::from(landed)));
            seen_numerator = seen_numerator.saturating_add(weight.saturating_mul(u128::from(seen)));
        }

        let observed_floor_lamports = floor_numerator
            .saturating_add(floor_denominator / 2)
            .checked_div(floor_denominator)
            .map_or(0, |mean| u64::try_from(mean).unwrap_or(u64::MAX));

        let saturation_micros = if saturation_denominator == 0 {
            0
        } else {
            fixed::to_micros(fixed::div(saturation_numerator, saturation_denominator).min(ONE))
        };

        let land_rate_micros = if seen_numerator == 0 {
            None
        } else {
            Some(fixed::to_micros(
                fixed::div(landed_numerator, seen_numerator).min(ONE),
            ))
        };

        WindowSummary {
            head_slot: self.head_slot,
            slots_observed: u32::try_from(self.slots.len()).unwrap_or(u32::MAX),
            observed_floor_lamports,
            saturation_micros,
            land_rate_micros,
        }
    }
}

// ---------------------------------------------------------------------------
// the floor
// ---------------------------------------------------------------------------

/// Which bound, if either, decided the answer.
///
/// Kept apart from the number for the reason `ConfirmOutcome` keeps `Dropped`
/// and `Expired` apart: a floor of ten thousand lamports because the window
/// said so and a floor of ten thousand because the window said four hundred and
/// the minimum lifted it are different facts about the market, and flattened
/// into one number the cockpit could not tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TipFloorClamp {
    /// The computed value was inside the bounds and is what is reported.
    Unclamped,
    /// The computed value was below `min_lamports` and was lifted to it. Every
    /// floor priced from an empty window is this.
    Lifted,
    /// The computed value was above `max_lamports` and was cut to it. The
    /// market is asking more than the operator configured a bid to start at.
    Cut,
}

/// One priced floor, and the working behind it.
///
/// Carried whole rather than reduced to a lamport count, for the reason
/// [`TipBid`](crate::execution::TipBid) carries its own working: a receipt that
/// says only what was paid, without what the window said or how close the
/// leader was, cannot be audited after the fact. Every field is an integer, so
/// two runs over the same observations serialise to the same bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TipFloor {
    /// What a bid should start from, in lamports. The answer.
    pub lamports: u64,
    /// The weighted floor the window alone implied, before the multiplier.
    pub observed_lamports: u64,
    /// The whole multiplier applied to it, in millionths. `1_000_000` is one.
    pub multiplier_micros: u64,
    /// The weighted compute saturation, in millionths.
    pub saturation_micros: u64,
    /// How close the leader is, in millionths, where one means leading now.
    ///
    /// `None` when the schedule does not know — which every schedule in this
    /// build does not. Not zero: "no connected leader is near" and "nobody
    /// looked" are different facts, and only the first of them is a reason to
    /// bid less.
    pub proximity_micros: Option<u64>,
    /// The weighted land rate over the window, in millionths. `None` when the
    /// window saw no bundles.
    pub land_rate_micros: Option<u64>,
    pub slots_observed: u32,
    pub head_slot: u64,
    pub clamp: TipFloorClamp,
}

impl TipFloor {
    /// The floor an unfitted window prices: the configured minimum, and every
    /// term saying it measured nothing.
    pub fn unobserved(params: &TipFloorParams) -> Self {
        TipFloor {
            lamports: params.min_lamports.max(JITO_MIN_TIP_LAMPORTS),
            observed_lamports: 0,
            multiplier_micros: MICROS,
            saturation_micros: 0,
            proximity_micros: None,
            land_rate_micros: None,
            slots_observed: 0,
            head_slot: 0,
            clamp: TipFloorClamp::Lifted,
        }
    }
}

/// How close the connected leader is, in `10^-18`, or `None` when unknown.
///
/// `Connected { wait_ms }` is converted to whole slots by flooring, so a leader
/// three hundred milliseconds out and one that leads this instant are the same
/// slot and price the same. That is the right resolution: the thing being
/// weighted is how many slots of competition are between us and the block, and
/// there is no fraction of a slot in that question.
fn proximity(hint: LeaderHint, params: &TipFloorParams) -> Option<u128> {
    match hint {
        LeaderHint::Unknown => None,
        LeaderHint::NoneInReach => Some(0),
        LeaderHint::Connected { wait_ms } => {
            let slots_out = wait_ms / SLOT_MS;
            let decay = fixed::from_micros(params.proximity_decay_micros.min(MICROS));
            Some(fixed::pow(
                decay,
                u32::try_from(slots_out).unwrap_or(u32::MAX),
            ))
        }
    }
}

/// Prices the floor from a window and a leader hint.
///
/// ```text
/// floor = clamp( observed x (1 + k_sat x saturation + k_prox x proximity) )
/// ```
///
/// The two gains are added rather than multiplied. Multiplied, a full block
/// during a leader's own slot would price `1.5 x 1.75 = 2.6` times the observed
/// floor and the two terms would compound every time both were true — which is
/// exactly the market state where a runaway bid is most expensive and least
/// recoverable. Added, the worst case is a bounded `2.25`, and the ceiling
/// below catches even that.
///
/// Malformed parameters price [`TipFloor::unobserved`] rather than returning an
/// error. A tip floor is read on the send path during an unwind, and Annex C.2
/// is explicit that an emergency exit is not blocked for being unpriceable —
/// falling back to the static constant gets the position closed, where a
/// `Result` nobody could act on in that moment would leave it open.
pub fn tip_floor(window: &CongestionWindow, hint: LeaderHint, params: &TipFloorParams) -> TipFloor {
    if params.malformed().is_some() {
        return TipFloor::unobserved(params);
    }

    let summary = window.weighted(params);
    let saturation = fixed::from_micros(summary.saturation_micros);
    let nearness = proximity(hint, params);

    let saturation_term = fixed::mul(
        fixed::from_micros(params.saturation_gain_micros),
        saturation,
    );
    let proximity_term = match nearness {
        Some(value) => fixed::mul(fixed::from_micros(params.proximity_gain_micros), value),
        None => 0,
    };
    let multiplier = ONE
        .saturating_add(saturation_term)
        .saturating_add(proximity_term);

    let raw = fixed::scale(summary.observed_floor_lamports, multiplier);
    let low = params.min_lamports.max(JITO_MIN_TIP_LAMPORTS);
    let high = params.max_lamports.max(low);
    let clamp = if raw < low {
        TipFloorClamp::Lifted
    } else if raw > high {
        TipFloorClamp::Cut
    } else {
        TipFloorClamp::Unclamped
    };

    TipFloor {
        lamports: raw.clamp(low, high),
        observed_lamports: summary.observed_floor_lamports,
        multiplier_micros: fixed::to_micros(multiplier),
        saturation_micros: summary.saturation_micros,
        proximity_micros: nearness.map(fixed::to_micros),
        land_rate_micros: summary.land_rate_micros,
        slots_observed: summary.slots_observed,
        head_slot: summary.head_slot,
        clamp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slot that landed bundles at `floor` lamports with an empty block.
    ///
    /// Saturation and the floor are moved independently by these helpers so a
    /// test can say which of the two terms it is exercising.
    fn priced(slot: u64, floor: u64) -> SlotObservation {
        SlotObservation {
            landed_floor_lamports: floor,
            bundles_landed: 1,
            bundles_seen: 1,
            ..SlotObservation::idle(slot)
        }
    }

    /// A slot at `used` of `limit` compute units, landing nothing.
    fn congested(slot: u64, used: u64, limit: u64) -> SlotObservation {
        SlotObservation {
            compute_units_used: used,
            compute_unit_limit: limit,
            ..SlotObservation::idle(slot)
        }
    }

    fn window_of(observations: &[SlotObservation], params: &TipFloorParams) -> CongestionWindow {
        let mut window = CongestionWindow::new();
        for observation in observations {
            window.observe(*observation, params);
        }
        window
    }

    // --- the empty case ----------------------------------------------------

    #[test]
    fn an_unfitted_window_prices_the_static_floor() {
        // The property that makes turning this on safe: with nothing observed
        // the answer is the constant `TipPolicy::emergency` already used.
        let params = TipFloorParams::default();
        let floor = tip_floor(&CongestionWindow::new(), LeaderHint::Unknown, &params);

        assert_eq!(floor.lamports, EXIT_TIP_BASE_LAMPORTS);
        assert_eq!(floor.observed_lamports, 0);
        assert_eq!(floor.slots_observed, 0);
        assert_eq!(
            floor.multiplier_micros, MICROS,
            "nothing measured multiplies by one"
        );
        assert_eq!(floor.clamp, TipFloorClamp::Lifted);
        assert_eq!(floor.proximity_micros, None);
        assert_eq!(floor.land_rate_micros, None);
    }

    #[test]
    fn an_unknown_schedule_is_not_a_schedule_that_says_no() {
        // `None` and `Some(0)` are different facts and only one of them is a
        // reason to bid less. Both add nothing to the multiplier here, but the
        // telemetry has to be able to tell them apart.
        let params = TipFloorParams::default();
        let window = window_of(&[priced(100, 100_000)], &params);

        let unknown = tip_floor(&window, LeaderHint::Unknown, &params);
        let none_near = tip_floor(&window, LeaderHint::NoneInReach, &params);

        assert_eq!(unknown.proximity_micros, None);
        assert_eq!(none_near.proximity_micros, Some(0));
        assert_eq!(unknown.lamports, none_near.lamports, "neither adds a term");
    }

    // --- the weighted floor ------------------------------------------------

    #[test]
    fn one_observation_prices_exactly_what_it_saw() {
        let params = TipFloorParams::default();
        let window = window_of(&[priced(100, 100_000)], &params);
        let floor = tip_floor(&window, LeaderHint::Unknown, &params);

        assert_eq!(floor.observed_lamports, 100_000);
        assert_eq!(floor.lamports, 100_000);
        assert_eq!(floor.multiplier_micros, MICROS);
        assert_eq!(floor.clamp, TipFloorClamp::Unclamped);
        assert_eq!(floor.slots_observed, 1);
        assert_eq!(floor.head_slot, 100);
    }

    #[test]
    fn a_newer_slot_outweighs_an_older_one() {
        // The whole point of the slot-distance weighting: the mean of 100k and
        // 200k is 150k, and the weighted mean leans towards the newer number.
        let params = TipFloorParams::default();
        let window = window_of(&[priced(100, 100_000), priced(101, 200_000)], &params);
        let floor = tip_floor(&window, LeaderHint::Unknown, &params);

        // w(101) = 1, w(100) = 0.85. (200000 + 0.85 x 100000) / 1.85 = 154054.05,
        // rounded to nearest.
        assert_eq!(floor.observed_lamports, 154_054);
        assert!(
            floor.observed_lamports > 150_000,
            "the newer slot has to pull it up"
        );
    }

    #[test]
    fn the_same_two_slots_the_other_way_round_lean_the_other_way() {
        let params = TipFloorParams::default();
        let window = window_of(&[priced(100, 200_000), priced(101, 100_000)], &params);
        let floor = tip_floor(&window, LeaderHint::Unknown, &params);

        // (100000 + 0.85 x 200000) / 1.85 = 145945.9, rounded.
        assert_eq!(floor.observed_lamports, 145_946);
    }

    #[test]
    fn a_window_prices_the_same_floor_whatever_order_it_was_fed_in() {
        // A reporter catching up after a stall delivers a burst out of order,
        // and the price must be a function of the set rather than the sequence.
        let params = TipFloorParams::default();
        let observations = [
            priced(100, 40_000),
            priced(101, 90_000),
            priced(102, 55_000),
            priced(103, 120_000),
        ];
        let forwards = tip_floor(
            &window_of(&observations, &params),
            LeaderHint::Unknown,
            &params,
        );

        let mut backwards_input = observations;
        backwards_input.reverse();
        let backwards = tip_floor(
            &window_of(&backwards_input, &params),
            LeaderHint::Unknown,
            &params,
        );

        let shuffled = [
            observations[2],
            observations[0],
            observations[3],
            observations[1],
        ];
        let jumbled = tip_floor(&window_of(&shuffled, &params), LeaderHint::Unknown, &params);

        assert_eq!(forwards, backwards);
        assert_eq!(forwards, jumbled);

        // And not merely the same price out of different states: the windows
        // themselves are equal, which is the stronger claim and the one that
        // stays true if the reduction ever changes.
        assert_eq!(
            window_of(&observations, &params),
            window_of(&backwards_input, &params)
        );
        assert_eq!(
            window_of(&observations, &params),
            window_of(&shuffled, &params)
        );
    }

    #[test]
    fn re_observing_a_slot_replaces_it_rather_than_weighing_it_twice() {
        let params = TipFloorParams::default();
        let mut window = CongestionWindow::new();
        window.observe(priced(100, 100_000), &params);
        window.observe(priced(100, 300_000), &params);

        assert_eq!(window.len(), 1);
        let floor = tip_floor(&window, LeaderHint::Unknown, &params);
        assert_eq!(
            floor.observed_lamports, 300_000,
            "the later reading is the true one"
        );
        assert_eq!(floor.slots_observed, 1);
    }

    #[test]
    fn a_quiet_slot_does_not_vote_the_price_down_to_nothing() {
        // A slot where nothing landed is evidence about congestion and no
        // evidence at all about what landing cost.
        let params = TipFloorParams::default();
        let busy = window_of(&[priced(100, 100_000)], &params);
        let busy_then_quiet =
            window_of(&[priced(100, 100_000), SlotObservation::idle(101)], &params);

        assert_eq!(
            tip_floor(&busy_then_quiet, LeaderHint::Unknown, &params).observed_lamports,
            tip_floor(&busy, LeaderHint::Unknown, &params).observed_lamports,
        );
    }

    // --- the window's edges ------------------------------------------------

    #[test]
    fn evidence_older_than_the_window_is_dropped_on_arrival() {
        let params = TipFloorParams {
            window_slots: 4,
            ..TipFloorParams::default()
        };
        let mut window = CongestionWindow::new();
        window.observe(priced(100, 100_000), &params);
        // Four slots later, the first is exactly at the edge and goes.
        window.observe(priced(104, 200_000), &params);

        assert_eq!(window.len(), 1);
        assert_eq!(
            tip_floor(&window, LeaderHint::Unknown, &params).observed_lamports,
            200_000,
        );
    }

    #[test]
    fn evidence_that_arrives_already_stale_is_never_stored() {
        let params = TipFloorParams {
            window_slots: 4,
            ..TipFloorParams::default()
        };
        let mut window = CongestionWindow::new();
        window.observe(priced(104, 200_000), &params);
        window.observe(priced(100, 999_999), &params);

        assert_eq!(
            window.len(),
            1,
            "the stale one was refused rather than stored"
        );
        assert_eq!(
            tip_floor(&window, LeaderHint::Unknown, &params).observed_lamports,
            200_000,
        );
    }

    #[test]
    fn a_window_that_stops_being_fed_ages_out_rather_than_pricing_forever() {
        // The failure this prevents: a spike that ended ten seconds ago still
        // being paid for because nothing arrived to push it out.
        let params = TipFloorParams {
            window_slots: 4,
            ..TipFloorParams::default()
        };
        let mut window = window_of(&[priced(100, 5_000_000)], &params);
        assert_eq!(
            tip_floor(&window, LeaderHint::Unknown, &params).observed_lamports,
            5_000_000
        );

        window.advance_to(200, &params);

        let floor = tip_floor(&window, LeaderHint::Unknown, &params);
        assert_eq!(window.len(), 0);
        assert_eq!(floor.observed_lamports, 0);
        assert_eq!(
            floor.lamports, EXIT_TIP_BASE_LAMPORTS,
            "back to the static floor"
        );
        assert_eq!(
            floor.head_slot, 200,
            "and the clock moved even though nothing arrived"
        );
    }

    #[test]
    fn the_window_never_holds_more_slots_than_it_is_configured_for() {
        let params = TipFloorParams {
            window_slots: 8,
            ..TipFloorParams::default()
        };
        let mut window = CongestionWindow::new();
        for slot in 1_000..1_100 {
            window.observe(priced(slot, 50_000), &params);
            assert!(
                window.len() <= 8,
                "at slot {slot} the window held {}",
                window.len()
            );
        }
        assert_eq!(window.len(), 8);
    }

    // --- congestion --------------------------------------------------------

    #[test]
    fn a_full_block_raises_the_floor_by_the_configured_gain() {
        let params = TipFloorParams::default();
        let saturated = SlotObservation {
            compute_units_used: 48_000_000,
            compute_unit_limit: 48_000_000,
            landed_floor_lamports: 100_000,
            bundles_landed: 1,
            bundles_seen: 1,
            slot: 100,
        };
        let floor = tip_floor(
            &window_of(&[saturated], &params),
            LeaderHint::Unknown,
            &params,
        );

        assert_eq!(floor.saturation_micros, MICROS, "the block was full");
        assert_eq!(floor.multiplier_micros, 1_500_000, "1 + 0.5 x 1");
        assert_eq!(floor.lamports, 150_000);
    }

    #[test]
    fn an_idle_block_adds_nothing() {
        let params = TipFloorParams::default();
        let floor = tip_floor(
            &window_of(&[priced(100, 100_000)], &params),
            LeaderHint::Unknown,
            &params,
        );

        assert_eq!(floor.saturation_micros, 0);
        assert_eq!(floor.multiplier_micros, MICROS);
        assert_eq!(floor.lamports, 100_000);
    }

    #[test]
    fn a_half_full_block_is_half_the_gain() {
        let params = TipFloorParams::default();
        let half = SlotObservation {
            landed_floor_lamports: 100_000,
            ..congested(100, 24_000_000, 48_000_000)
        };
        let floor = tip_floor(&window_of(&[half], &params), LeaderHint::Unknown, &params);

        assert_eq!(floor.saturation_micros, 500_000);
        assert_eq!(floor.multiplier_micros, 1_250_000, "1 + 0.5 x 0.5");
        assert_eq!(floor.lamports, 125_000);
    }

    #[test]
    fn a_limit_nobody_reported_reads_as_no_congestion_rather_than_full() {
        let params = TipFloorParams::default();
        let unknown_limit = SlotObservation {
            compute_units_used: 48_000_000,
            compute_unit_limit: 0,
            landed_floor_lamports: 100_000,
            ..SlotObservation::idle(100)
        };
        let floor = tip_floor(
            &window_of(&[unknown_limit], &params),
            LeaderHint::Unknown,
            &params,
        );

        assert_eq!(floor.saturation_micros, 0);
        assert_eq!(floor.lamports, 100_000);
    }

    #[test]
    fn a_block_reported_over_its_own_ceiling_is_capped_at_full() {
        let params = TipFloorParams::default();
        let overfull = SlotObservation {
            landed_floor_lamports: 100_000,
            ..congested(100, 96_000_000, 48_000_000)
        };
        let floor = tip_floor(
            &window_of(&[overfull], &params),
            LeaderHint::Unknown,
            &params,
        );

        assert_eq!(floor.saturation_micros, MICROS, "capped, not doubled");
        assert_eq!(floor.lamports, 150_000);
    }

    #[test]
    fn the_floor_never_falls_as_congestion_climbs() {
        let params = TipFloorParams::default();
        let mut previous = 0u64;
        for used in (0..=48_000_000u64).step_by(1_000_000) {
            let observation = SlotObservation {
                landed_floor_lamports: 100_000,
                ..congested(100, used, 48_000_000)
            };
            let floor = tip_floor(
                &window_of(&[observation], &params),
                LeaderHint::Unknown,
                &params,
            );
            assert!(
                floor.lamports >= previous,
                "{used} units priced below {previous}"
            );
            previous = floor.lamports;
        }
        assert_eq!(previous, 150_000);
    }

    // --- leader proximity --------------------------------------------------

    #[test]
    fn a_leader_that_is_up_now_is_the_full_proximity_term() {
        let params = TipFloorParams::default();
        let window = window_of(&[priced(100, 100_000)], &params);
        let floor = tip_floor(&window, LeaderHint::Connected { wait_ms: 0 }, &params);

        assert_eq!(floor.proximity_micros, Some(MICROS));
        assert_eq!(floor.multiplier_micros, 1_750_000, "1 + 0.75 x 1");
        assert_eq!(floor.lamports, 175_000);
    }

    #[test]
    fn a_leader_a_slot_out_is_one_decay_step_down() {
        let params = TipFloorParams::default();
        let window = window_of(&[priced(100, 100_000)], &params);
        let floor = tip_floor(&window, LeaderHint::Connected { wait_ms: SLOT_MS }, &params);

        assert_eq!(floor.proximity_micros, Some(600_000));
        assert_eq!(floor.multiplier_micros, 1_450_000, "1 + 0.75 x 0.6");
        assert_eq!(floor.lamports, 145_000);
    }

    #[test]
    fn a_wait_inside_one_slot_is_the_same_slot() {
        // There is no fraction of a slot in "how many slots of competition are
        // between us and the block", so everything under one slot prices alike.
        let params = TipFloorParams::default();
        let window = window_of(&[priced(100, 100_000)], &params);
        let now = tip_floor(&window, LeaderHint::Connected { wait_ms: 0 }, &params);

        for wait_ms in [1, 100, SLOT_MS - 1] {
            let near = tip_floor(&window, LeaderHint::Connected { wait_ms }, &params);
            assert_eq!(near, now, "{wait_ms}ms should price as this slot");
        }
    }

    #[test]
    fn the_proximity_term_falls_away_the_further_the_leader_is() {
        let params = TipFloorParams::default();
        let window = window_of(&[priced(100, 100_000)], &params);
        let mut previous = u64::MAX;
        for slots_out in 0..32u64 {
            let floor = tip_floor(
                &window,
                LeaderHint::Connected {
                    wait_ms: slots_out * SLOT_MS,
                },
                &params,
            );
            assert!(
                floor.lamports <= previous,
                "{slots_out} slots out priced above {previous}"
            );
            previous = floor.lamports;
        }

        // The decay is geometric, so the premium approaches nothing rather than
        // arriving at it. What matters is where it stops being a lamport: at
        // twenty-four slots out — under ten seconds — it has rounded away
        // entirely, and a leader that far ahead is priced as if it were not
        // there. One slot earlier it is still worth a single lamport, which is
        // the resolution this whole calculation bottoms out at.
        let vanished = tip_floor(
            &window,
            LeaderHint::Connected {
                wait_ms: 24 * SLOT_MS,
            },
            &params,
        );
        let last_lamport = tip_floor(
            &window,
            LeaderHint::Connected {
                wait_ms: 23 * SLOT_MS,
            },
            &params,
        );
        assert_eq!(
            vanished.lamports, 100_000,
            "far enough out is no premium at all"
        );
        assert_eq!(
            last_lamport.lamports, 100_001,
            "and one slot nearer is worth a lamport"
        );
    }

    #[test]
    fn a_leader_out_of_reach_prices_the_window_and_nothing_more() {
        let params = TipFloorParams::default();
        let window = window_of(&[priced(100, 100_000)], &params);
        let floor = tip_floor(&window, LeaderHint::NoneInReach, &params);

        assert_eq!(floor.proximity_micros, Some(0));
        assert_eq!(floor.multiplier_micros, MICROS);
        assert_eq!(floor.lamports, 100_000);
    }

    // --- escalation, which is what the two terms together are for -----------

    #[test]
    fn the_floor_escalates_as_the_market_tightens() {
        // Four states of the world, strictly increasing, and each step is one
        // more thing being true. This is the escalation the module exists for:
        // it is a function of the market, not of a retry counter.
        let params = TipFloorParams::default();
        let calm = window_of(&[priced(100, 100_000)], &params);
        let busy = window_of(
            &[SlotObservation {
                landed_floor_lamports: 100_000,
                ..congested(100, 48_000_000, 48_000_000)
            }],
            &params,
        );

        let quiet = tip_floor(&calm, LeaderHint::NoneInReach, &params).lamports;
        let contested = tip_floor(&busy, LeaderHint::NoneInReach, &params).lamports;
        let leader_soon = tip_floor(
            &busy,
            LeaderHint::Connected {
                wait_ms: 2 * SLOT_MS,
            },
            &params,
        )
        .lamports;
        let leader_now = tip_floor(&busy, LeaderHint::Connected { wait_ms: 0 }, &params).lamports;

        assert_eq!(quiet, 100_000);
        assert_eq!(contested, 150_000);
        assert_eq!(leader_now, 225_000, "1 + 0.5 + 0.75");
        assert!(
            quiet < contested && contested < leader_soon && leader_soon < leader_now,
            "{quiet} < {contested} < {leader_soon} < {leader_now}",
        );
    }

    #[test]
    fn the_two_gains_add_rather_than_compound() {
        // Multiplied, the worst case would be 1.5 x 1.75 = 2.625 and the two
        // would compound exactly when a runaway bid is least affordable.
        let params = TipFloorParams::default();
        let busy = window_of(
            &[SlotObservation {
                landed_floor_lamports: 100_000,
                ..congested(100, 48_000_000, 48_000_000)
            }],
            &params,
        );
        let worst = tip_floor(&busy, LeaderHint::Connected { wait_ms: 0 }, &params);

        assert_eq!(worst.multiplier_micros, 2_250_000);
        assert_ne!(
            worst.multiplier_micros, 2_625_000,
            "that would be compounding"
        );
    }

    #[test]
    fn a_congestion_spike_moves_the_floor_inside_a_leader_rotation() {
        // The reason the decay is as steep as it is: four slots of a spike has
        // to have visibly moved the price, or the window is too slow to be
        // worth consulting.
        let params = TipFloorParams::default();
        let mut window = CongestionWindow::new();
        for slot in 100..132 {
            window.observe(priced(slot, 20_000), &params);
        }
        let before = tip_floor(&window, LeaderHint::Unknown, &params).lamports;

        for slot in 132..136 {
            window.observe(priced(slot, 400_000), &params);
        }
        let after = tip_floor(&window, LeaderHint::Unknown, &params).lamports;

        assert_eq!(before, 20_000);
        assert!(
            after > 3 * before,
            "four slots of spike moved {before} to {after}"
        );
    }

    // --- the bounds --------------------------------------------------------

    #[test]
    fn a_floor_under_the_minimum_is_lifted_to_it() {
        let params = TipFloorParams::default();
        let window = window_of(&[priced(100, 400)], &params);
        let floor = tip_floor(&window, LeaderHint::Unknown, &params);

        assert_eq!(floor.observed_lamports, 400, "the window said what it said");
        assert_eq!(floor.lamports, EXIT_TIP_BASE_LAMPORTS);
        assert_eq!(floor.clamp, TipFloorClamp::Lifted);
    }

    #[test]
    fn a_floor_over_the_ceiling_is_cut_to_it() {
        let params = TipFloorParams::default();
        let window = window_of(&[priced(100, 900_000_000)], &params);
        let floor = tip_floor(&window, LeaderHint::Connected { wait_ms: 0 }, &params);

        assert_eq!(floor.lamports, EXIT_TIP_MAX_LAMPORTS);
        assert_eq!(floor.clamp, TipFloorClamp::Cut);
        assert!(
            floor.observed_lamports > floor.lamports,
            "the working survives the clamp"
        );
    }

    #[test]
    fn no_market_anywhere_prices_a_floor_outside_the_bounds() {
        let params = TipFloorParams::default();
        for floor_lamports in [0u64, 1, 999, 10_000, 5_000_000, 50_000_000, u32::MAX as u64] {
            for used in [0u64, 24_000_000, 48_000_000] {
                for hint in [
                    LeaderHint::Unknown,
                    LeaderHint::NoneInReach,
                    LeaderHint::Connected { wait_ms: 0 },
                    LeaderHint::Connected { wait_ms: 4_000 },
                ] {
                    let observation = SlotObservation {
                        landed_floor_lamports: floor_lamports,
                        ..congested(100, used, 48_000_000)
                    };
                    let priced_floor =
                        tip_floor(&window_of(&[observation], &params), hint, &params);
                    assert!(
                        priced_floor.lamports >= params.min_lamports
                            && priced_floor.lamports <= params.max_lamports,
                        "{floor_lamports}/{used}/{hint:?} priced {}",
                        priced_floor.lamports,
                    );
                    assert!(
                        priced_floor.lamports >= JITO_MIN_TIP_LAMPORTS,
                        "a floor a block engine would ignore",
                    );
                }
            }
        }
    }

    #[test]
    fn a_minimum_under_what_an_engine_looks_at_is_lifted_to_what_it_does() {
        let params = TipFloorParams {
            min_lamports: 1,
            ..TipFloorParams::default()
        };
        let floor = tip_floor(&CongestionWindow::new(), LeaderHint::Unknown, &params);
        assert_eq!(floor.lamports, JITO_MIN_TIP_LAMPORTS);
    }

    // --- malformed parameters ----------------------------------------------

    #[test]
    fn malformed_parameters_fall_back_rather_than_blocking_an_exit() {
        // Annex C.2: an emergency exit is not blocked for being unpriceable.
        let cases = [
            TipFloorParams {
                window_slots: 0,
                ..TipFloorParams::default()
            },
            TipFloorParams {
                slot_decay_micros: 0,
                ..TipFloorParams::default()
            },
            TipFloorParams {
                slot_decay_micros: 2 * MICROS,
                ..TipFloorParams::default()
            },
            TipFloorParams {
                proximity_decay_micros: 0,
                ..TipFloorParams::default()
            },
            TipFloorParams {
                min_lamports: 900_000,
                max_lamports: 1_000,
                ..TipFloorParams::default()
            },
            TipFloorParams {
                max_lamports: 10,
                ..TipFloorParams::default()
            },
        ];
        for params in cases {
            assert!(params.malformed().is_some(), "{params:?} should be refused");
            let window = window_of(&[priced(100, 100_000)], &params);
            let floor = tip_floor(&window, LeaderHint::Connected { wait_ms: 0 }, &params);
            assert_eq!(
                floor,
                TipFloor::unobserved(&params),
                "{params:?} should have priced the fallback",
            );
            assert!(floor.lamports >= JITO_MIN_TIP_LAMPORTS);
        }
    }

    #[test]
    fn well_formed_parameters_say_nothing() {
        assert_eq!(TipFloorParams::default().malformed(), None);
    }

    // --- land rate ---------------------------------------------------------

    #[test]
    fn the_land_rate_is_what_the_window_saw() {
        let params = TipFloorParams::default();
        let observation = SlotObservation {
            bundles_landed: 3,
            bundles_seen: 4,
            ..SlotObservation::idle(100)
        };
        let floor = tip_floor(
            &window_of(&[observation], &params),
            LeaderHint::Unknown,
            &params,
        );
        assert_eq!(floor.land_rate_micros, Some(750_000));
    }

    #[test]
    fn a_window_that_saw_no_bundles_has_no_land_rate() {
        // Not zero: "none landed" and "none were sent" are different facts.
        let params = TipFloorParams::default();
        let floor = tip_floor(
            &window_of(&[SlotObservation::idle(100)], &params),
            LeaderHint::Unknown,
            &params,
        );
        assert_eq!(floor.land_rate_micros, None);
    }

    #[test]
    fn a_window_where_nothing_landed_reports_zero_rather_than_nothing() {
        let params = TipFloorParams::default();
        let observation = SlotObservation {
            bundles_landed: 0,
            bundles_seen: 6,
            ..SlotObservation::idle(100)
        };
        let floor = tip_floor(
            &window_of(&[observation], &params),
            LeaderHint::Unknown,
            &params,
        );
        assert_eq!(floor.land_rate_micros, Some(0));
    }

    #[test]
    fn more_landings_than_forwards_is_clamped_rather_than_believed() {
        let params = TipFloorParams::default();
        let nonsense = SlotObservation {
            bundles_landed: 9,
            bundles_seen: 2,
            ..SlotObservation::idle(100)
        };
        let floor = tip_floor(
            &window_of(&[nonsense], &params),
            LeaderHint::Unknown,
            &params,
        );
        assert_eq!(
            floor.land_rate_micros,
            Some(MICROS),
            "a rate above one is not renderable"
        );
    }

    // --- leader rotation ---------------------------------------------------

    #[test]
    fn a_rotation_is_four_consecutive_slots() {
        assert_eq!(LEADER_SLOTS_PER_ROTATION, 4);
        assert_eq!(leader_rotation(0), 0);
        assert_eq!(leader_rotation(3), 0);
        assert_eq!(leader_rotation(4), 1);
        assert_eq!(leader_rotation(7), 1);
        assert_eq!(leader_rotation(8), 2);
    }

    // --- determinism and the no-float rule ---------------------------------

    #[test]
    fn the_same_window_prices_the_same_floor_every_time() {
        let params = TipFloorParams::default();
        let window = window_of(
            &[
                priced(100, 40_000),
                congested(101, 30_000_000, 48_000_000),
                priced(102, 90_000),
            ],
            &params,
        );
        let first = tip_floor(&window, LeaderHint::Connected { wait_ms: 800 }, &params);
        for _ in 0..16 {
            assert_eq!(
                tip_floor(&window, LeaderHint::Connected { wait_ms: 800 }, &params),
                first
            );
        }
    }

    #[test]
    fn a_priced_floor_serialises_to_the_same_bytes_twice() {
        // The Phase 3 acceptance criterion, at this module's scale: one window
        // and one hint produce byte-identical telemetry.
        let params = TipFloorParams::default();
        let window = window_of(&[priced(100, 123_456)], &params);
        let floor = tip_floor(&window, LeaderHint::Connected { wait_ms: 0 }, &params);

        let once = serde_json::to_string(&floor).expect("a floor serialises");
        let twice = serde_json::to_string(&floor).expect("a floor serialises");
        assert_eq!(once, twice);
        assert!(
            once.contains("\"multiplierMicros\""),
            "camelCase for the cockpit: {once}"
        );
    }

    #[test]
    fn tip_floor_arithmetic_uses_no_floating_point() {
        // The rule the module doc states, enforced against the module's own
        // source. Comments are stripped first so that a doc comment explaining
        // why `f64` is refused does not read as an `f64` being used.
        let source = include_str!("jito.rs");
        let implementation = source
            .split("#[cfg(test)]")
            .next()
            .expect("there is a source before the tests");

        for (number, line) in implementation.lines().enumerate() {
            let code = match line.find("//") {
                Some(at) => &line[..at],
                None => line,
            };
            for banned in ["f64", "f32", "as f", "sqrt()", "powf", "powi"] {
                assert!(
                    !code.contains(banned),
                    "line {} uses {banned}: {code}",
                    number + 1,
                );
            }
            assert!(
                !code.chars().collect::<Vec<_>>().windows(3).any(|window| {
                    window[0].is_ascii_digit() && window[1] == '.' && window[2].is_ascii_digit()
                }),
                "line {} has a decimal literal: {code}",
                number + 1,
            );
        }
    }
}
