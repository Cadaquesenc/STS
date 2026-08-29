//! What happens to a bundle between pricing it and giving up on it.
//!
//! A bundle is not a transaction and does not fail like one. There is no
//! signature to poll, no "not found" that settles into "expired": a block
//! engine either forwards it to a leader who includes it, or the moment passes
//! and nothing anywhere records that it existed. The only thing that can be
//! said afterwards is what *we* decided, so this module decides it explicitly
//! and writes it down.
//!
//! # Three slots, and the clock does not reset
//!
//! A bundle is retained for [`MAX_RETENTION_SLOTS`] slots from the slot it was
//! first priced in. Inside that window it is retried — re-priced at whatever
//! the floor says now, with the attempt counter advanced, which is the only
//! thing Annex C.2 lets move between retries. At the far edge it is dropped.
//!
//! The retention clock is anchored to the *first* pricing and a retry does not
//! restart it. That is the whole point: a clock that restarted on every retry
//! would let a bundle that keeps missing live forever, retried at a floor that
//! keeps climbing, which is the runaway `TipPolicy`'s ceiling exists to prevent
//! and would be reintroduced one slot at a time. Three slots from pricing means
//! at most two retries and a guaranteed terminal state at a known slot, whatever
//! happens in between.
//!
//! # The other way out
//!
//! Retention is not the only deterministic drop. A block engine forwards to the
//! validator it is connected to now, and Solana rotates leaders every
//! [`LEADER_SLOTS_PER_ROTATION`] slots. A bundle still in flight when the
//! rotation turns is not late — it is addressed to somebody who has stopped
//! listening, and no amount of remaining retention window will change that. So
//! crossing a leader boundary evicts, and it evicts *first*: when a bundle
//! reaches its retention limit and crosses a boundary in the same advance,
//! [`EvictionReason::LeaderBoundary`] is what gets recorded, because it is the
//! reason that would have applied on its own.
//!
//! Which of the two fires depends only on where in a rotation the bundle
//! opened. One priced at the first slot of a rotation has its whole three-slot
//! window inside that leader's turn and ages out normally; one priced at the
//! last slot has a single slot before the boundary takes it. Both are correct
//! and the difference is visible in telemetry rather than averaged into one
//! "dropped" counter.
//!
//! # Determinism
//!
//! Nothing here reads a clock, a random number or the network. Slots and
//! timestamps arrive as arguments, live bundles are held in a `BTreeMap` so
//! every sweep visits them in the same order, and [`BundleTransition`]s come
//! back in that order. Two trackers fed the same calls produce the same
//! transitions and the same telemetry, byte for byte, which is what lets a
//! replay assert on a bundle's whole life rather than on its ending.

use std::collections::BTreeMap;

use parking_lot::Mutex;
use serde::Serialize;

use crate::backtest::MICROS;
use crate::execution::LeaderHint;
use crate::jito::{
    leader_rotation, tip_floor, CongestionWindow, SlotObservation, TipFloor, TipFloorParams,
    LEADER_SLOTS_PER_ROTATION,
};
use crate::metrics::{Histogram, HistogramSnapshot};

/// How many slots a bundle is kept before it is dropped, counted from the slot
/// it was first priced in.
///
/// Three. Long enough that a bundle survives one leader's whole turn if it was
/// priced at the start of it; short enough that the position it belongs to is
/// still the position that was priced. At four hundred milliseconds a slot this
/// is one and a fifth seconds, which is inside the window an exit can afford to
/// spend deciding.
pub const MAX_RETENTION_SLOTS: u64 = 3;

// ---------------------------------------------------------------------------
// states
// ---------------------------------------------------------------------------

/// Where a bundle is.
///
/// Five, and the three terminal ones are kept apart rather than flattened into
/// "did not land". A bundle the engine refused, a bundle that aged out and a
/// bundle whose leader changed are three different things to fix, and a single
/// counter for them is a counter that cannot say which is happening.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BundleState {
    /// Priced and held, not yet handed to a block engine.
    Priced,
    /// Handed over. The only state a retry can happen from.
    InFlight,
    /// Included in a block. Terminal, and the only ending where the tip was
    /// actually paid.
    Landed,
    /// Dropped by this module, for one of the two deterministic reasons.
    /// Terminal.
    Evicted,
    /// Refused by the block engine — a malformed bundle, a duplicate, a
    /// simulation failure. Terminal, and not a timeout: something answered.
    Rejected,
}

impl BundleState {
    /// Whether nothing further can happen to a bundle in this state.
    pub fn terminal(self) -> bool {
        matches!(
            self,
            BundleState::Landed | BundleState::Evicted | BundleState::Rejected
        )
    }

    /// The name `sts.db` and the cockpit both use, so a metric and a row line up
    /// without a translation table.
    pub fn name(self) -> &'static str {
        match self {
            BundleState::Priced => "priced",
            BundleState::InFlight => "inFlight",
            BundleState::Landed => "landed",
            BundleState::Evicted => "evicted",
            BundleState::Rejected => "rejected",
        }
    }

    /// Every state, in the order the cockpit lists them.
    pub const ALL: [BundleState; 5] = [
        BundleState::Priced,
        BundleState::InFlight,
        BundleState::Landed,
        BundleState::Evicted,
        BundleState::Rejected,
    ];
}

/// Why a bundle was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum EvictionReason {
    /// It reached [`MAX_RETENTION_SLOTS`] without landing.
    Retention,
    /// The leader rotated out from under it. Takes precedence over retention
    /// when both apply in one advance.
    LeaderBoundary,
}

impl EvictionReason {
    pub fn name(self) -> &'static str {
        match self {
            EvictionReason::Retention => "retention",
            EvictionReason::LeaderBoundary => "leaderBoundary",
        }
    }
}

/// What moved, and why.
///
/// Returned from every call that can change a bundle's state, so a caller
/// records history rather than polling for it. `Eq`, because the assertion a
/// test wants to make is that one exact list of transitions came back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleTransition {
    pub id: String,
    pub from: BundleState,
    pub to: BundleState,
    pub at_slot: u64,
    /// Set only on a move into [`BundleState::Evicted`].
    pub eviction: Option<EvictionReason>,
    /// The attempt the bundle was on when this happened. A retry reports the
    /// attempt it is moving *to*, which is the one that will be sent.
    pub attempt: u32,
    /// What the bundle was tipping at this point, in lamports. On a retry this
    /// is the re-priced tip, not the one that missed.
    pub tip_lamports: u64,
}

/// Why a call was refused.
///
/// A refusal is never a state change: a tracker that rejected a call is in
/// exactly the state it was in before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "detail")]
pub enum BundleError {
    /// No bundle by that id is live, and none is remembered — either it never
    /// existed or it reached a terminal state and was swept.
    Unknown(String),
    /// A bundle by that id is already live. Ids are the caller's to keep unique
    /// and reusing one would silently overwrite a bundle still in flight.
    Duplicate(String),
    /// The bundle is live but not in a state this call can act on — submitting
    /// one already in flight, landing one never submitted.
    WrongState {
        id: String,
        state: BundleState,
        expected: BundleState,
    },
    /// The slot went backwards. Slots are the tracker's clock and a clock that
    /// runs backwards makes every age calculation below it meaningless.
    SlotRegression { id: String, from: u64, to: u64 },
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BundleError::Unknown(id) => {
                write!(
                    f,
                    "no live bundle {id:?}: it never opened, or it has already ended"
                )
            }
            BundleError::Duplicate(id) => write!(
                f,
                "bundle {id:?} is already live — reusing an id would overwrite a bundle that \
                 has not ended yet"
            ),
            BundleError::WrongState {
                id,
                state,
                expected,
            } => write!(
                f,
                "bundle {id:?} is {} and this call needs it to be {}",
                state.name(),
                expected.name()
            ),
            BundleError::SlotRegression { id, from, to } => write!(
                f,
                "bundle {id:?} was opened at slot {from} and the clock has been moved back to \
                 {to}; every retention age below that is meaningless"
            ),
        }
    }
}

impl std::error::Error for BundleError {}

// ---------------------------------------------------------------------------
// one bundle
// ---------------------------------------------------------------------------

/// A bundle's whole life, as the tracker holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleRecord {
    pub id: String,
    pub state: BundleState,
    /// The slot it was first priced in. The retention clock's origin, and it
    /// does not move when the bundle is retried.
    pub opened_slot: u64,
    /// The rotation `opened_slot` belongs to, kept rather than recomputed so
    /// the boundary test is a comparison rather than a division on every sweep.
    pub opened_rotation: u64,
    /// The slot of the most recent pricing — the first one, or the last retry.
    pub attempt_slot: u64,
    /// Zero on the first pricing, one more on each retry.
    pub attempt: u32,
    /// What it is tipping now, in lamports.
    pub tip_lamports: u64,
    /// When it was priced, in epoch milliseconds.
    pub priced_at_ms: i64,
    /// When it was handed to a block engine. `None` while it is still
    /// [`BundleState::Priced`].
    pub submitted_at_ms: Option<i64>,
    /// When it reached a terminal state.
    pub settled_at_ms: Option<i64>,
    /// Set only once it has been evicted.
    pub eviction: Option<EvictionReason>,
}

impl BundleRecord {
    /// How many slots old it is at `slot`. Saturating, so a clock that has been
    /// moved backwards reads as zero rather than wrapping to a huge age and
    /// evicting everything.
    pub fn age_slots(&self, slot: u64) -> u64 {
        slot.saturating_sub(self.opened_slot)
    }

    /// The slot it will be dropped at if nothing else happens to it first.
    pub fn expires_at_slot(&self) -> u64 {
        self.opened_slot.saturating_add(MAX_RETENTION_SLOTS)
    }

    /// The slot the leader changes at, which is the other way it can end.
    pub fn leader_boundary_slot(&self) -> u64 {
        self.opened_rotation
            .saturating_add(1)
            .saturating_mul(LEADER_SLOTS_PER_ROTATION)
    }

    /// What would happen to this bundle if the clock reached `slot`, or nothing
    /// if it would survive.
    ///
    /// The whole eviction policy, as one pure function of the record and a
    /// slot. Written this way so the precedence between the two reasons is a
    /// single readable expression rather than an ordering buried in a sweep,
    /// and so a test can ask about a hypothetical slot without moving a tracker
    /// to it.
    pub fn eviction_at(&self, slot: u64) -> Option<EvictionReason> {
        if self.state.terminal() {
            return None;
        }
        if leader_rotation(slot) != self.opened_rotation {
            return Some(EvictionReason::LeaderBoundary);
        }
        if self.age_slots(slot) >= MAX_RETENTION_SLOTS {
            return Some(EvictionReason::Retention);
        }
        None
    }
}

// ---------------------------------------------------------------------------
// counters
// ---------------------------------------------------------------------------

/// How many bundles ended each way, over the tracker's whole life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleCounts {
    pub opened: u64,
    pub submitted: u64,
    pub retried: u64,
    pub landed: u64,
    pub evicted_retention: u64,
    pub evicted_leader_boundary: u64,
    pub rejected: u64,
    /// Bundles that have not ended yet, in either live state.
    pub live: u64,
    /// Of the live ones, how many have been handed over.
    pub in_flight: u64,
}

impl BundleCounts {
    /// Everything that reached a terminal state.
    pub fn resolved(&self) -> u64 {
        self.landed
            .saturating_add(self.evicted_retention)
            .saturating_add(self.evicted_leader_boundary)
            .saturating_add(self.rejected)
    }

    /// Both eviction reasons together.
    pub fn evicted(&self) -> u64 {
        self.evicted_retention
            .saturating_add(self.evicted_leader_boundary)
    }
}

/// How often a bundle lands, from three angles.
///
/// Three rather than one because they answer different questions and a single
/// "land rate" hides which one is being asked. `overall` is how the tracker's
/// own bundles did. `first_attempt` is how they did without the retries, which
/// is the number that says whether the floor is priced correctly rather than
/// merely priced high enough eventually. `window` is what the market did,
/// including everybody else's bundles, and is the baseline the other two are
/// only meaningful against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LandRates {
    /// Landed over resolved, in millionths. `None` until something resolves —
    /// not zero, which would read as "nothing lands".
    pub overall_micros: Option<u64>,
    /// Landed on attempt zero over resolved, in millionths.
    pub first_attempt_micros: Option<u64>,
    /// What the congestion window saw, in millionths. `None` when it saw no
    /// bundles at all.
    pub window_micros: Option<u64>,
}

/// Where a bundle's time went.
///
/// Three stages, measured in microseconds and bucketed by
/// [`Histogram`](crate::metrics::Histogram), so the quantiles are the same
/// shape the rest of the process reports and the exporter can add two runs'
/// buckets together. `price_to_submit` is ours to fix — it is signing and
/// serialising. `submit_to_land` is the network's and the leader's.
/// `price_to_land` is what the position actually waited, which is the only one
/// of the three a risk limit is written against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LatencyBreakdown {
    pub price_to_submit: HistogramSnapshot,
    pub submit_to_land: HistogramSnapshot,
    pub price_to_land: HistogramSnapshot,
}

/// What tips cost, over the tracker's whole life.
///
/// `committed` and `paid` are deliberately different numbers. A tip is a
/// transfer inside a bundle, and a bundle that never lands transfers nothing —
/// so a run can commit a great deal and pay almost none of it, and a cockpit
/// that showed only the committed figure would report money that never left the
/// wallet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TipSummary {
    /// How many pricings there have been — openings plus retries.
    pub pricings: u64,
    /// Every tip ever priced, added up. Not money that moved.
    pub committed_lamports: u64,
    /// Tips on bundles that landed. Money that moved.
    pub paid_lamports: u64,
    /// Tips on bundles that ended without landing. Never transferred, and
    /// tracked because a large number here is the signal that the floor is
    /// being priced for slots the bundles are not reaching.
    pub forfeited_lamports: u64,
    pub min_lamports: Option<u64>,
    pub max_lamports: Option<u64>,
    /// The mean priced tip, floored. `None` before anything is priced.
    pub mean_lamports: Option<u64>,
}

/// Everything the cockpit renders, in one value.
///
/// Every field is an integer or a struct of integers — no strings to parse, no
/// units left implicit, and `Eq` all the way down so a test can assert on a
/// whole snapshot rather than on a field at a time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BundleTelemetry {
    /// The slot the tracker's clock is at.
    pub at_slot: u64,
    /// What a bundle priced right now would start from, and its working.
    pub floor: TipFloor,
    pub counts: BundleCounts,
    pub land: LandRates,
    pub latency: LatencyBreakdown,
    pub tip: TipSummary,
    /// The bundles that have not ended, in id order. Bounded by how many an
    /// exit opens within three slots, so this is a handful of rows rather than
    /// a log.
    pub live: Vec<BundleRecord>,
}

// ---------------------------------------------------------------------------
// the tracker
// ---------------------------------------------------------------------------

/// The state machine itself.
///
/// Single-owner and `&mut`: every method that can change a bundle takes an
/// exclusive borrow, so the transitions a caller gets back are the complete set
/// that happened while it held the tracker. [`BundleDeck`] is the shareable
/// wrapper for the parts of the process that need one.
#[derive(Debug)]
pub struct BundleTracker {
    params: TipFloorParams,
    window: CongestionWindow,
    live: BTreeMap<String, BundleRecord>,
    slot: u64,
    counts: BundleCounts,
    tip: TipSummary,
    landed_first_attempt: u64,
    price_to_submit: Histogram,
    submit_to_land: Histogram,
    price_to_land: Histogram,
}

impl Default for BundleTracker {
    fn default() -> Self {
        Self::new(TipFloorParams::default())
    }
}

impl BundleTracker {
    pub fn new(params: TipFloorParams) -> Self {
        BundleTracker {
            params,
            window: CongestionWindow::new(),
            live: BTreeMap::new(),
            slot: 0,
            counts: BundleCounts::default(),
            tip: TipSummary::default(),
            landed_first_attempt: 0,
            price_to_submit: Histogram::new(),
            submit_to_land: Histogram::new(),
            price_to_land: Histogram::new(),
        }
    }

    pub fn params(&self) -> &TipFloorParams {
        &self.params
    }

    /// The slot the tracker believes it is at.
    pub fn slot(&self) -> u64 {
        self.slot
    }

    /// One bundle, live or not yet swept. `None` once it has ended.
    pub fn get(&self, id: &str) -> Option<&BundleRecord> {
        self.live.get(id)
    }

    /// Takes one slot's evidence for the tip floor. Does not move the clock:
    /// evidence can arrive late, and a stale observation must not make the
    /// tracker think time has passed.
    pub fn observe_slot(&mut self, observation: SlotObservation) {
        self.window.observe(observation, &self.params);
    }

    /// What a bundle priced right now would start from.
    pub fn floor(&self, hint: LeaderHint) -> TipFloor {
        tip_floor(&self.window, hint, &self.params)
    }

    /// Prices a new bundle and holds it.
    ///
    /// The slot given here is the retention clock's origin and never moves
    /// again. A slot behind the tracker's own clock is refused rather than
    /// accepted and back-dated, since a bundle opened in the past is already
    /// part-way through a window it never had.
    pub fn open(
        &mut self,
        id: &str,
        slot: u64,
        at_ms: i64,
        tip_lamports: u64,
    ) -> Result<BundleTransition, BundleError> {
        if self.live.contains_key(id) {
            return Err(BundleError::Duplicate(id.to_string()));
        }
        if slot < self.slot {
            return Err(BundleError::SlotRegression {
                id: id.to_string(),
                from: self.slot,
                to: slot,
            });
        }
        if slot > self.slot {
            self.slot = slot;
            self.window.advance_to(slot, &self.params);
        }

        let record = BundleRecord {
            id: id.to_string(),
            state: BundleState::Priced,
            opened_slot: slot,
            opened_rotation: leader_rotation(slot),
            attempt_slot: slot,
            attempt: 0,
            tip_lamports,
            priced_at_ms: at_ms,
            submitted_at_ms: None,
            settled_at_ms: None,
            eviction: None,
        };

        self.counts.opened = self.counts.opened.saturating_add(1);
        self.record_pricing(tip_lamports);
        self.live.insert(id.to_string(), record);
        self.recount_live();

        Ok(BundleTransition {
            id: id.to_string(),
            from: BundleState::Priced,
            to: BundleState::Priced,
            at_slot: slot,
            eviction: None,
            attempt: 0,
            tip_lamports,
        })
    }

    /// Hands a priced bundle to a block engine.
    pub fn submit(&mut self, id: &str, at_ms: i64) -> Result<BundleTransition, BundleError> {
        let slot = self.slot;
        let record = self
            .live
            .get_mut(id)
            .ok_or_else(|| BundleError::Unknown(id.to_string()))?;
        if record.state != BundleState::Priced {
            return Err(BundleError::WrongState {
                id: id.to_string(),
                state: record.state,
                expected: BundleState::Priced,
            });
        }

        record.state = BundleState::InFlight;
        record.submitted_at_ms = Some(at_ms);
        let waited = at_ms.saturating_sub(record.priced_at_ms);
        let attempt = record.attempt;
        let tip_lamports = record.tip_lamports;

        self.price_to_submit.record_us(millis_to_micros(waited));
        self.counts.submitted = self.counts.submitted.saturating_add(1);
        self.recount_live();

        Ok(BundleTransition {
            id: id.to_string(),
            from: BundleState::Priced,
            to: BundleState::InFlight,
            at_slot: slot,
            eviction: None,
            attempt,
            tip_lamports,
        })
    }

    /// Moves the clock, and applies the retry and eviction policies to
    /// everything live.
    ///
    /// One sweep in id order. Each bundle is asked for its eviction first —
    /// [`BundleRecord::eviction_at`] is the whole policy including the
    /// precedence between the two reasons — and only a bundle that survives it
    /// is considered for a retry. That ordering is what stops a bundle being
    /// re-priced into a slot it is about to be dropped from, which would show
    /// as a tip committed against a bundle nobody ever sent.
    ///
    /// A retry needs the bundle to be in flight and the slot to have moved past
    /// its last pricing, so a caller that advances to the same slot twice gets
    /// transitions the first time and an empty list the second. Idempotent in
    /// the only sense that matters: the state after two identical advances is
    /// the state after one.
    ///
    /// A slot at or behind the current one changes nothing and returns nothing.
    pub fn advance_to_slot(
        &mut self,
        slot: u64,
        at_ms: i64,
        hint: LeaderHint,
    ) -> Vec<BundleTransition> {
        if slot <= self.slot {
            return Vec::new();
        }
        self.slot = slot;
        self.window.advance_to(slot, &self.params);

        let repriced = tip_floor(&self.window, hint, &self.params).lamports;
        let ids: Vec<String> = self.live.keys().cloned().collect();
        let mut transitions = Vec::new();

        for id in ids {
            let Some(record) = self.live.get_mut(&id) else {
                continue;
            };
            if record.state.terminal() {
                continue;
            }

            if let Some(reason) = record.eviction_at(slot) {
                let from = record.state;
                record.state = BundleState::Evicted;
                record.eviction = Some(reason);
                record.settled_at_ms = Some(at_ms);
                let attempt = record.attempt;
                let tip_lamports = record.tip_lamports;

                match reason {
                    EvictionReason::Retention => {
                        self.counts.evicted_retention =
                            self.counts.evicted_retention.saturating_add(1)
                    }
                    EvictionReason::LeaderBoundary => {
                        self.counts.evicted_leader_boundary =
                            self.counts.evicted_leader_boundary.saturating_add(1)
                    }
                }
                self.tip.forfeited_lamports =
                    self.tip.forfeited_lamports.saturating_add(tip_lamports);

                transitions.push(BundleTransition {
                    id: id.clone(),
                    from,
                    to: BundleState::Evicted,
                    at_slot: slot,
                    eviction: Some(reason),
                    attempt,
                    tip_lamports,
                });
                continue;
            }

            // Survived. A bundle that is in flight and has not been re-priced
            // in this slot gets one more attempt at the current floor.
            if record.state == BundleState::InFlight && slot > record.attempt_slot {
                record.attempt = record.attempt.saturating_add(1);
                record.attempt_slot = slot;
                record.tip_lamports = repriced;
                let attempt = record.attempt;

                self.counts.retried = self.counts.retried.saturating_add(1);
                self.record_pricing(repriced);

                transitions.push(BundleTransition {
                    id: id.clone(),
                    from: BundleState::InFlight,
                    to: BundleState::InFlight,
                    at_slot: slot,
                    eviction: None,
                    attempt,
                    tip_lamports: repriced,
                });
            }
        }

        self.sweep_terminal();
        self.recount_live();
        transitions
    }

    /// Records that a bundle was included in a block.
    pub fn land(&mut self, id: &str, at_ms: i64) -> Result<BundleTransition, BundleError> {
        let slot = self.slot;
        let record = self
            .live
            .get_mut(id)
            .ok_or_else(|| BundleError::Unknown(id.to_string()))?;
        if record.state != BundleState::InFlight {
            return Err(BundleError::WrongState {
                id: id.to_string(),
                state: record.state,
                expected: BundleState::InFlight,
            });
        }

        record.state = BundleState::Landed;
        record.settled_at_ms = Some(at_ms);
        let attempt = record.attempt;
        let tip_lamports = record.tip_lamports;
        let submitted_at_ms = record.submitted_at_ms.unwrap_or(record.priced_at_ms);
        let priced_at_ms = record.priced_at_ms;

        self.submit_to_land
            .record_us(millis_to_micros(at_ms.saturating_sub(submitted_at_ms)));
        self.price_to_land
            .record_us(millis_to_micros(at_ms.saturating_sub(priced_at_ms)));
        self.counts.landed = self.counts.landed.saturating_add(1);
        if attempt == 0 {
            self.landed_first_attempt = self.landed_first_attempt.saturating_add(1);
        }
        self.tip.paid_lamports = self.tip.paid_lamports.saturating_add(tip_lamports);

        self.sweep_terminal();
        self.recount_live();

        Ok(BundleTransition {
            id: id.to_string(),
            from: BundleState::InFlight,
            to: BundleState::Landed,
            at_slot: slot,
            eviction: None,
            attempt,
            tip_lamports,
        })
    }

    /// Records that the block engine refused the bundle.
    ///
    /// Allowed from either live state: an engine can refuse on the submit call
    /// itself, before this tracker has been told the bundle is in flight.
    pub fn reject(&mut self, id: &str, at_ms: i64) -> Result<BundleTransition, BundleError> {
        let slot = self.slot;
        let record = self
            .live
            .get_mut(id)
            .ok_or_else(|| BundleError::Unknown(id.to_string()))?;
        if record.state.terminal() {
            return Err(BundleError::WrongState {
                id: id.to_string(),
                state: record.state,
                expected: BundleState::InFlight,
            });
        }

        let from = record.state;
        record.state = BundleState::Rejected;
        record.settled_at_ms = Some(at_ms);
        let attempt = record.attempt;
        let tip_lamports = record.tip_lamports;

        self.counts.rejected = self.counts.rejected.saturating_add(1);
        self.tip.forfeited_lamports = self.tip.forfeited_lamports.saturating_add(tip_lamports);

        self.sweep_terminal();
        self.recount_live();

        Ok(BundleTransition {
            id: id.to_string(),
            from,
            to: BundleState::Rejected,
            at_slot: slot,
            eviction: None,
            attempt,
            tip_lamports,
        })
    }

    /// Everything the cockpit renders.
    pub fn telemetry(&self, hint: LeaderHint) -> BundleTelemetry {
        let floor = self.floor(hint);
        let resolved = self.counts.resolved();

        BundleTelemetry {
            at_slot: self.slot,
            counts: self.counts,
            land: LandRates {
                overall_micros: share_micros(self.counts.landed, resolved),
                first_attempt_micros: share_micros(self.landed_first_attempt, resolved),
                window_micros: floor.land_rate_micros,
            },
            latency: LatencyBreakdown {
                price_to_submit: self.price_to_submit.snapshot(),
                submit_to_land: self.submit_to_land.snapshot(),
                price_to_land: self.price_to_land.snapshot(),
            },
            tip: self.tip,
            live: self.live.values().cloned().collect(),
            floor,
        }
    }

    /// Adds one pricing — an opening or a retry — to the tip summary.
    fn record_pricing(&mut self, lamports: u64) {
        self.tip.pricings = self.tip.pricings.saturating_add(1);
        self.tip.committed_lamports = self.tip.committed_lamports.saturating_add(lamports);
        self.tip.min_lamports = Some(match self.tip.min_lamports {
            Some(current) => current.min(lamports),
            None => lamports,
        });
        self.tip.max_lamports = Some(match self.tip.max_lamports {
            Some(current) => current.max(lamports),
            None => lamports,
        });
        self.tip.mean_lamports = Some(self.tip.committed_lamports / self.tip.pricings.max(1));
    }

    /// Drops everything that has ended.
    ///
    /// A terminal bundle is history, and history belongs in the audit log
    /// rather than in a map that is swept every slot. The counters above are
    /// what survives it.
    fn sweep_terminal(&mut self) {
        self.live.retain(|_, record| !record.state.terminal());
    }

    /// Recomputes the two live counters from the map, rather than incrementing
    /// and decrementing them.
    ///
    /// Derived rather than maintained on purpose: a counter that is stepped at
    /// every transition is a counter that drifts the first time a path forgets
    /// to step it, and the map is small enough that counting it is free.
    fn recount_live(&mut self) {
        self.counts.live = self.live.len() as u64;
        self.counts.in_flight = self
            .live
            .values()
            .filter(|record| record.state == BundleState::InFlight)
            .count() as u64;
    }
}

/// Milliseconds to microseconds, floored at zero.
///
/// A negative interval is a clock that was corrected between two readings, not
/// a bundle that landed before it was priced, and it is recorded as an instant
/// rather than allowed to wrap a `u64`.
fn millis_to_micros(millis: i64) -> u64 {
    u64::try_from(millis.max(0))
        .unwrap_or(0)
        .saturating_mul(1_000)
}

/// `part / whole` in millionths, or `None` when there is no whole.
fn share_micros(part: u64, whole: u64) -> Option<u64> {
    if whole == 0 {
        return None;
    }
    Some(
        u64::try_from(u128::from(part).saturating_mul(u128::from(MICROS)) / u128::from(whole))
            .unwrap_or(MICROS)
            .min(MICROS),
    )
}

// ---------------------------------------------------------------------------
// the shareable wrapper
// ---------------------------------------------------------------------------

/// A [`BundleTracker`] the whole process can hold.
///
/// One `Arc`, one lock, and every method takes `&self`, which is what a Tauri
/// command needs from a `State`. The lock is held only for the duration of one
/// call — nothing here does IO, so there is no path on which a window's
/// telemetry poll waits on a network read.
#[derive(Debug)]
pub struct BundleDeck {
    inner: Mutex<BundleTracker>,
}

impl Default for BundleDeck {
    fn default() -> Self {
        Self::new(TipFloorParams::default())
    }
}

impl BundleDeck {
    pub fn new(params: TipFloorParams) -> Self {
        BundleDeck {
            inner: Mutex::new(BundleTracker::new(params)),
        }
    }

    pub fn observe_slot(&self, observation: SlotObservation) {
        self.inner.lock().observe_slot(observation);
    }

    pub fn open(
        &self,
        id: &str,
        slot: u64,
        at_ms: i64,
        tip_lamports: u64,
    ) -> Result<BundleTransition, BundleError> {
        self.inner.lock().open(id, slot, at_ms, tip_lamports)
    }

    pub fn submit(&self, id: &str, at_ms: i64) -> Result<BundleTransition, BundleError> {
        self.inner.lock().submit(id, at_ms)
    }

    pub fn advance_to_slot(
        &self,
        slot: u64,
        at_ms: i64,
        hint: LeaderHint,
    ) -> Vec<BundleTransition> {
        self.inner.lock().advance_to_slot(slot, at_ms, hint)
    }

    pub fn land(&self, id: &str, at_ms: i64) -> Result<BundleTransition, BundleError> {
        self.inner.lock().land(id, at_ms)
    }

    pub fn reject(&self, id: &str, at_ms: i64) -> Result<BundleTransition, BundleError> {
        self.inner.lock().reject(id, at_ms)
    }

    pub fn floor(&self, hint: LeaderHint) -> TipFloor {
        self.inner.lock().floor(hint)
    }

    pub fn telemetry(&self, hint: LeaderHint) -> BundleTelemetry {
        self.inner.lock().telemetry(hint)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jito::SLOT_MS;

    /// The wall clock a test uses: one slot is `SLOT_MS`, and slot zero is
    /// millisecond zero, so a timestamp and a slot never disagree about how
    /// much time has passed.
    fn at(slot: u64) -> i64 {
        i64::try_from(slot.saturating_mul(SLOT_MS)).expect("a test slot fits")
    }

    /// A tracker whose window has seen one slot at a known floor, so a retry
    /// re-prices to something a test can name.
    ///
    /// The observation sits at slot 100 because that is where these tests open
    /// their bundles. Evidence at slot zero would be a hundred slots stale by
    /// then and the window would — correctly — have dropped it, which is a
    /// property `a_window_that_stops_being_fed_ages_out_rather_than_pricing_forever`
    /// covers rather than something to trip over here.
    ///
    /// One observation means the weighted mean is that observation whatever
    /// weight it carries, so the floor holds steady at `floor_lamports` across
    /// every slot these tests advance through.
    fn tracker_pricing(floor_lamports: u64) -> BundleTracker {
        let mut tracker = BundleTracker::default();
        tracker.observe_slot(SlotObservation {
            landed_floor_lamports: floor_lamports,
            bundles_landed: 1,
            bundles_seen: 1,
            ..SlotObservation::idle(100)
        });
        tracker
    }

    /// Opens a bundle and hands it over in one step, which is what every test
    /// that is not about the submit transition itself wants.
    fn open_and_submit(tracker: &mut BundleTracker, id: &str, slot: u64, tip: u64) {
        tracker.open(id, slot, at(slot), tip).expect("opens");
        tracker.submit(id, at(slot)).expect("submits");
    }

    // --- the retention window ----------------------------------------------

    #[test]
    fn three_slots_is_the_whole_retention_window() {
        assert_eq!(MAX_RETENTION_SLOTS, 3);
    }

    #[test]
    fn a_bundle_is_dropped_exactly_three_slots_after_it_was_priced() {
        // Opened at the first slot of a rotation, so the leader boundary is
        // four slots out and retention is what ends it.
        let mut tracker = BundleTracker::default();
        open_and_submit(&mut tracker, "b", 100, 50_000);

        for slot in 101..103 {
            let moved = tracker.advance_to_slot(slot, at(slot), LeaderHint::Unknown);
            assert!(
                moved.iter().all(|t| t.to != BundleState::Evicted),
                "slot {slot} dropped it early: {moved:?}",
            );
            assert_eq!(
                tracker.get("b").map(|r| r.state),
                Some(BundleState::InFlight)
            );
        }

        let dropped = tracker.advance_to_slot(103, at(103), LeaderHint::Unknown);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].to, BundleState::Evicted);
        assert_eq!(dropped[0].eviction, Some(EvictionReason::Retention));
        assert_eq!(dropped[0].at_slot, 103);
        assert_eq!(tracker.get("b"), None, "a terminal bundle is swept");
    }

    #[test]
    fn the_expiry_slot_is_known_the_moment_it_is_priced() {
        let mut tracker = BundleTracker::default();
        tracker.open("b", 100, at(100), 50_000).expect("opens");
        let record = tracker.get("b").expect("live");

        assert_eq!(record.expires_at_slot(), 103);
        assert_eq!(record.eviction_at(102), None);
        assert_eq!(record.eviction_at(103), Some(EvictionReason::Retention));
    }

    #[test]
    fn a_bundle_never_handed_over_ages_out_the_same_way() {
        let mut tracker = BundleTracker::default();
        tracker.open("b", 100, at(100), 50_000).expect("opens");

        let dropped = tracker.advance_to_slot(103, at(103), LeaderHint::Unknown);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].from, BundleState::Priced);
        assert_eq!(dropped[0].to, BundleState::Evicted);
        assert_eq!(dropped[0].eviction, Some(EvictionReason::Retention));
    }

    #[test]
    fn skipping_past_the_window_still_drops_it_once() {
        // A stalled process comes back many slots later. The bundle is dropped,
        // not dropped repeatedly, and not forgotten.
        let mut tracker = BundleTracker::default();
        open_and_submit(&mut tracker, "b", 100, 50_000);

        let dropped = tracker.advance_to_slot(400, at(400), LeaderHint::Unknown);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].to, BundleState::Evicted);
        assert_eq!(tracker.telemetry(LeaderHint::Unknown).counts.evicted(), 1);

        assert!(tracker
            .advance_to_slot(401, at(401), LeaderHint::Unknown)
            .is_empty());
        assert_eq!(tracker.telemetry(LeaderHint::Unknown).counts.evicted(), 1);
    }

    // --- retries -----------------------------------------------------------

    #[test]
    fn a_bundle_in_flight_is_retried_once_per_slot() {
        let mut tracker = tracker_pricing(80_000);
        open_and_submit(&mut tracker, "b", 100, 50_000);

        let first = tracker.advance_to_slot(101, at(101), LeaderHint::Unknown);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].from, BundleState::InFlight);
        assert_eq!(first[0].to, BundleState::InFlight);
        assert_eq!(first[0].attempt, 1, "the attempt it is moving to");
        assert_eq!(tracker.get("b").expect("live").attempt, 1);

        let second = tracker.advance_to_slot(102, at(102), LeaderHint::Unknown);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].attempt, 2);
        assert_eq!(tracker.get("b").expect("live").attempt, 2);
    }

    #[test]
    fn a_retry_re_prices_at_what_the_floor_says_now() {
        let mut tracker = tracker_pricing(80_000);
        open_and_submit(&mut tracker, "b", 100, 50_000);
        assert_eq!(tracker.get("b").expect("live").tip_lamports, 50_000);

        let retried = tracker.advance_to_slot(101, at(101), LeaderHint::Unknown);
        assert_eq!(retried[0].tip_lamports, 80_000, "re-priced from the window");
        assert_eq!(tracker.get("b").expect("live").tip_lamports, 80_000);
    }

    #[test]
    fn a_retention_window_of_three_slots_allows_exactly_two_retries() {
        // The arithmetic the module doc states: three slots from pricing, one
        // retry per slot, a guaranteed terminal state at the third.
        let mut tracker = tracker_pricing(80_000);
        open_and_submit(&mut tracker, "b", 100, 50_000);

        let mut retries = 0;
        for slot in 101..=103 {
            for transition in tracker.advance_to_slot(slot, at(slot), LeaderHint::Unknown) {
                if transition.to == BundleState::InFlight {
                    retries += 1;
                }
            }
        }

        assert_eq!(retries, 2);
        assert_eq!(tracker.telemetry(LeaderHint::Unknown).counts.retried, 2);
        assert_eq!(
            tracker
                .telemetry(LeaderHint::Unknown)
                .counts
                .evicted_retention,
            1
        );
    }

    #[test]
    fn a_retry_does_not_restart_the_retention_clock() {
        // The property the whole design turns on. Retried at 101 and 102, the
        // bundle still dies at 103 rather than at 105.
        let mut tracker = tracker_pricing(80_000);
        open_and_submit(&mut tracker, "b", 100, 50_000);

        tracker.advance_to_slot(101, at(101), LeaderHint::Unknown);
        tracker.advance_to_slot(102, at(102), LeaderHint::Unknown);
        let record = tracker.get("b").expect("still live");
        assert_eq!(record.attempt, 2);
        assert_eq!(record.opened_slot, 100, "the origin never moved");
        assert_eq!(record.expires_at_slot(), 103);

        let dropped = tracker.advance_to_slot(103, at(103), LeaderHint::Unknown);
        assert_eq!(dropped[0].eviction, Some(EvictionReason::Retention));
    }

    #[test]
    fn advancing_to_the_same_slot_twice_retries_once() {
        let mut tracker = tracker_pricing(80_000);
        open_and_submit(&mut tracker, "b", 100, 50_000);

        assert_eq!(
            tracker
                .advance_to_slot(101, at(101), LeaderHint::Unknown)
                .len(),
            1
        );
        assert!(tracker
            .advance_to_slot(101, at(101), LeaderHint::Unknown)
            .is_empty());
        assert_eq!(tracker.get("b").expect("live").attempt, 1);
    }

    #[test]
    fn a_clock_that_goes_backwards_changes_nothing() {
        let mut tracker = tracker_pricing(80_000);
        open_and_submit(&mut tracker, "b", 100, 50_000);
        tracker.advance_to_slot(101, at(101), LeaderHint::Unknown);

        assert!(tracker
            .advance_to_slot(99, at(99), LeaderHint::Unknown)
            .is_empty());
        assert_eq!(tracker.slot(), 101);
        assert_eq!(tracker.get("b").expect("live").attempt, 1);
    }

    #[test]
    fn a_bundle_that_was_never_handed_over_is_not_retried() {
        // There is nothing to retry: no engine has it. It ages out instead.
        let mut tracker = tracker_pricing(80_000);
        tracker.open("b", 100, at(100), 50_000).expect("opens");

        assert!(tracker
            .advance_to_slot(101, at(101), LeaderHint::Unknown)
            .is_empty());
        let record = tracker.get("b").expect("live");
        assert_eq!(record.attempt, 0);
        assert_eq!(record.state, BundleState::Priced);
    }

    // --- leader boundaries -------------------------------------------------

    #[test]
    fn a_bundle_dies_when_the_leader_rotates_out_from_under_it() {
        // Priced at slot 103, the last slot of rotation 25. Slot 104 is a new
        // leader and the bundle is addressed to somebody who stopped listening
        // — one slot into a three-slot window.
        let mut tracker = BundleTracker::default();
        open_and_submit(&mut tracker, "b", 103, 50_000);
        assert_eq!(tracker.get("b").expect("live").leader_boundary_slot(), 104);

        let dropped = tracker.advance_to_slot(104, at(104), LeaderHint::Unknown);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].to, BundleState::Evicted);
        assert_eq!(dropped[0].eviction, Some(EvictionReason::LeaderBoundary));
        assert_eq!(dropped[0].attempt, 0, "it never got a retry");
    }

    #[test]
    fn where_in_a_rotation_a_bundle_opens_decides_how_it_ends() {
        // Every opening slot of one rotation, and the slot each dies at. This
        // is the table the module doc describes in prose.
        let expected = [
            // (opened slot, dies at, why)
            (100u64, 103u64, EvictionReason::Retention),
            (101, 104, EvictionReason::LeaderBoundary),
            (102, 104, EvictionReason::LeaderBoundary),
            (103, 104, EvictionReason::LeaderBoundary),
        ];

        for (opened, dies_at, why) in expected {
            let mut tracker = BundleTracker::default();
            open_and_submit(&mut tracker, "b", opened, 50_000);

            for slot in (opened + 1)..dies_at {
                let moved = tracker.advance_to_slot(slot, at(slot), LeaderHint::Unknown);
                assert!(
                    moved.iter().all(|t| t.to != BundleState::Evicted),
                    "opened at {opened}, died early at {slot}",
                );
            }

            let dropped = tracker.advance_to_slot(dies_at, at(dies_at), LeaderHint::Unknown);
            assert_eq!(dropped.len(), 1, "opened at {opened}");
            assert_eq!(dropped[0].eviction, Some(why), "opened at {opened}");
        }
    }

    #[test]
    fn a_boundary_and_a_retention_limit_at_once_report_the_boundary() {
        // Opened at slot 101: retention would end it at 104 and the rotation
        // ends at 104 too. The boundary is the reason that would have applied
        // on its own, so it is the one recorded.
        let mut tracker = BundleTracker::default();
        open_and_submit(&mut tracker, "b", 101, 50_000);
        let record = tracker.get("b").expect("live");
        assert_eq!(record.expires_at_slot(), 104);
        assert_eq!(record.leader_boundary_slot(), 104);

        let dropped = tracker.advance_to_slot(104, at(104), LeaderHint::Unknown);
        assert_eq!(dropped[0].eviction, Some(EvictionReason::LeaderBoundary));

        let counts = tracker.telemetry(LeaderHint::Unknown).counts;
        assert_eq!(counts.evicted_leader_boundary, 1);
        assert_eq!(
            counts.evicted_retention, 0,
            "counted once, under the right reason"
        );
    }

    #[test]
    fn a_bundle_is_never_retried_into_a_slot_it_is_about_to_be_dropped_from() {
        // The ordering inside the sweep: eviction is asked first, so a bundle
        // crossing a boundary is not re-priced on the way out. A tip committed
        // against a bundle nobody sent would be money invented in telemetry.
        let mut tracker = tracker_pricing(80_000);
        open_and_submit(&mut tracker, "b", 103, 50_000);

        let dropped = tracker.advance_to_slot(104, at(104), LeaderHint::Unknown);
        assert_eq!(
            dropped.len(),
            1,
            "one transition, not a retry and an eviction"
        );
        assert_eq!(dropped[0].tip_lamports, 50_000, "the tip it actually had");

        let tip = tracker.telemetry(LeaderHint::Unknown).tip;
        assert_eq!(tip.pricings, 1, "no phantom re-pricing");
        assert_eq!(tip.committed_lamports, 50_000);
        assert_eq!(tip.forfeited_lamports, 50_000);
        assert_eq!(tip.paid_lamports, 0);
    }

    // --- landing -----------------------------------------------------------

    #[test]
    fn a_landed_bundle_is_the_only_one_whose_tip_was_paid() {
        let mut tracker = BundleTracker::default();
        open_and_submit(&mut tracker, "b", 100, 50_000);

        let landed = tracker.land("b", at(101)).expect("lands");
        assert_eq!(landed.to, BundleState::Landed);
        assert_eq!(landed.attempt, 0);

        let telemetry = tracker.telemetry(LeaderHint::Unknown);
        assert_eq!(telemetry.counts.landed, 1);
        assert_eq!(telemetry.tip.paid_lamports, 50_000);
        assert_eq!(telemetry.tip.forfeited_lamports, 0);
        assert_eq!(tracker.get("b"), None, "swept once it ended");
    }

    #[test]
    fn a_bundle_that_landed_on_a_retry_pays_the_retried_tip() {
        let mut tracker = tracker_pricing(80_000);
        open_and_submit(&mut tracker, "b", 100, 50_000);
        tracker.advance_to_slot(101, at(101), LeaderHint::Unknown);
        tracker.land("b", at(101)).expect("lands");

        let telemetry = tracker.telemetry(LeaderHint::Unknown);
        assert_eq!(
            telemetry.tip.paid_lamports, 80_000,
            "the re-priced tip is what settled"
        );
        assert_eq!(
            telemetry.tip.committed_lamports, 130_000,
            "both pricings were committed"
        );
    }

    #[test]
    fn landing_a_bundle_that_was_never_handed_over_is_refused() {
        let mut tracker = BundleTracker::default();
        tracker.open("b", 100, at(100), 50_000).expect("opens");

        let refused = tracker
            .land("b", at(100))
            .expect_err("cannot land what was not sent");
        assert_eq!(
            refused,
            BundleError::WrongState {
                id: "b".to_string(),
                state: BundleState::Priced,
                expected: BundleState::InFlight,
            },
        );
        assert_eq!(
            tracker.get("b").expect("live").state,
            BundleState::Priced,
            "unchanged"
        );
    }

    // --- refusals ----------------------------------------------------------

    #[test]
    fn a_refused_call_never_changes_a_state() {
        let mut tracker = BundleTracker::default();
        open_and_submit(&mut tracker, "b", 100, 50_000);
        let before = tracker.telemetry(LeaderHint::Unknown);

        assert!(tracker.open("b", 100, at(100), 1).is_err(), "duplicate id");
        assert!(tracker.submit("b", at(100)).is_err(), "already in flight");
        assert!(tracker.land("nope", at(100)).is_err(), "unknown id");
        assert!(tracker.open("c", 99, at(99), 1).is_err(), "slot regression");

        assert_eq!(tracker.telemetry(LeaderHint::Unknown), before);
    }

    #[test]
    fn every_refusal_says_which_one_it_was() {
        let mut tracker = BundleTracker::default();
        open_and_submit(&mut tracker, "b", 100, 50_000);

        assert_eq!(
            tracker.open("b", 100, at(100), 1).expect_err("duplicate"),
            BundleError::Duplicate("b".to_string()),
        );
        assert_eq!(
            tracker.land("ghost", at(100)).expect_err("unknown"),
            BundleError::Unknown("ghost".to_string()),
        );
        assert_eq!(
            tracker.open("c", 42, at(42), 1).expect_err("regression"),
            BundleError::SlotRegression {
                id: "c".to_string(),
                from: 100,
                to: 42
            },
        );
    }

    #[test]
    fn a_rejected_bundle_is_not_an_eviction() {
        // Something answered. That is a different fact from a timeout and it is
        // counted separately.
        let mut tracker = BundleTracker::default();
        open_and_submit(&mut tracker, "b", 100, 50_000);
        let rejected = tracker.reject("b", at(100)).expect("rejects");

        assert_eq!(rejected.to, BundleState::Rejected);
        let counts = tracker.telemetry(LeaderHint::Unknown).counts;
        assert_eq!(counts.rejected, 1);
        assert_eq!(counts.evicted(), 0);
        assert_eq!(counts.landed, 0);
    }

    #[test]
    fn a_bundle_can_be_refused_before_it_is_ever_in_flight() {
        let mut tracker = BundleTracker::default();
        tracker.open("b", 100, at(100), 50_000).expect("opens");
        let rejected = tracker.reject("b", at(100)).expect("rejects from priced");
        assert_eq!(rejected.from, BundleState::Priced);
        assert_eq!(rejected.to, BundleState::Rejected);
    }

    // --- determinism -------------------------------------------------------

    #[test]
    fn a_sweep_visits_bundles_in_id_order_whatever_order_they_opened_in() {
        let mut forwards = BundleTracker::default();
        for id in ["a", "b", "c", "d"] {
            open_and_submit(&mut forwards, id, 100, 50_000);
        }
        let mut backwards = BundleTracker::default();
        for id in ["d", "c", "b", "a"] {
            open_and_submit(&mut backwards, id, 100, 50_000);
        }

        let one = forwards.advance_to_slot(103, at(103), LeaderHint::Unknown);
        let other = backwards.advance_to_slot(103, at(103), LeaderHint::Unknown);

        assert_eq!(one, other);
        assert_eq!(
            one.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c", "d"],
        );
    }

    #[test]
    fn two_trackers_fed_the_same_calls_end_in_the_same_state() {
        // The replay property, over a whole life rather than one transition.
        fn run() -> BundleTelemetry {
            let mut tracker = tracker_pricing(64_000);
            tracker.open("alpha", 100, at(100), 50_000).expect("opens");
            tracker.submit("alpha", at(100) + 3).expect("submits");
            tracker.open("beta", 100, at(100), 70_000).expect("opens");
            tracker.submit("beta", at(100) + 7).expect("submits");
            tracker.advance_to_slot(101, at(101), LeaderHint::Connected { wait_ms: 0 });
            tracker.land("alpha", at(101) + 40).expect("lands");
            tracker.advance_to_slot(102, at(102), LeaderHint::NoneInReach);
            tracker.open("gamma", 102, at(102), 90_000).expect("opens");
            tracker.advance_to_slot(103, at(103), LeaderHint::Unknown);
            tracker.telemetry(LeaderHint::Unknown)
        }

        let first = run();
        assert_eq!(first, run());
        assert_eq!(
            serde_json::to_string(&first).expect("serialises"),
            serde_json::to_string(&run()).expect("serialises"),
        );
    }

    // --- telemetry ---------------------------------------------------------

    #[test]
    fn an_untouched_tracker_reports_nothing_rather_than_zero() {
        let telemetry = BundleTracker::default().telemetry(LeaderHint::Unknown);

        assert_eq!(telemetry.counts, BundleCounts::default());
        assert_eq!(
            telemetry.land.overall_micros, None,
            "no rate, not a rate of zero"
        );
        assert_eq!(telemetry.land.first_attempt_micros, None);
        assert_eq!(telemetry.land.window_micros, None);
        assert_eq!(telemetry.tip, TipSummary::default());
        assert_eq!(telemetry.tip.mean_lamports, None);
        assert_eq!(telemetry.latency.price_to_submit.count, 0);
        assert_eq!(telemetry.latency.price_to_submit.p50_us, None);
        assert!(telemetry.live.is_empty());
    }

    #[test]
    fn the_land_rate_is_landings_over_everything_that_ended() {
        let mut tracker = BundleTracker::default();
        // Three land, one is dropped at its boundary: three of four.
        for (index, id) in ["a", "b", "c"].iter().enumerate() {
            open_and_submit(&mut tracker, id, 100, 50_000);
            tracker.land(id, at(100) + index as i64).expect("lands");
        }
        open_and_submit(&mut tracker, "d", 100, 50_000);
        tracker.advance_to_slot(103, at(103), LeaderHint::Unknown);

        let telemetry = tracker.telemetry(LeaderHint::Unknown);
        assert_eq!(telemetry.counts.landed, 3);
        assert_eq!(telemetry.counts.resolved(), 4);
        assert_eq!(telemetry.land.overall_micros, Some(750_000));
        assert_eq!(telemetry.land.first_attempt_micros, Some(750_000));
    }

    #[test]
    fn the_first_attempt_rate_separates_a_good_floor_from_a_lucky_retry() {
        // Both land. One needed a retry to do it, and the two rates say so.
        let mut tracker = tracker_pricing(80_000);
        open_and_submit(&mut tracker, "a", 100, 50_000);
        tracker.land("a", at(100)).expect("lands first time");

        open_and_submit(&mut tracker, "b", 100, 50_000);
        tracker.advance_to_slot(101, at(101), LeaderHint::Unknown);
        tracker.land("b", at(101)).expect("lands on the retry");

        let land = tracker.telemetry(LeaderHint::Unknown).land;
        assert_eq!(land.overall_micros, Some(MICROS), "everything landed");
        assert_eq!(
            land.first_attempt_micros,
            Some(500_000),
            "half of it first time"
        );
    }

    #[test]
    fn the_window_rate_is_the_market_and_not_our_own_bundles() {
        let mut tracker = BundleTracker::default();
        tracker.observe_slot(SlotObservation {
            bundles_landed: 1,
            bundles_seen: 5,
            ..SlotObservation::idle(100)
        });
        open_and_submit(&mut tracker, "b", 100, 50_000);
        tracker.land("b", at(100)).expect("lands");

        let land = tracker.telemetry(LeaderHint::Unknown).land;
        assert_eq!(land.overall_micros, Some(MICROS), "ours all landed");
        assert_eq!(land.window_micros, Some(200_000), "the market's did not");
    }

    #[test]
    fn the_latency_breakdown_splits_our_time_from_the_networks() {
        let mut tracker = BundleTracker::default();
        tracker.open("b", 100, at(100), 50_000).expect("opens");
        // 12ms to sign and serialise, then 250ms waiting on a block.
        tracker.submit("b", at(100) + 12).expect("submits");
        tracker.land("b", at(100) + 262).expect("lands");

        let latency = tracker.telemetry(LeaderHint::Unknown).latency;
        assert_eq!(latency.price_to_submit.count, 1);
        assert_eq!(latency.price_to_submit.sum_us, 12_000);
        assert_eq!(latency.submit_to_land.count, 1);
        assert_eq!(latency.submit_to_land.sum_us, 250_000);
        assert_eq!(latency.price_to_land.count, 1);
        assert_eq!(latency.price_to_land.sum_us, 262_000, "and the two add up");
    }

    #[test]
    fn a_clock_corrected_backwards_reads_as_an_instant_rather_than_wrapping() {
        let mut tracker = BundleTracker::default();
        tracker.open("b", 100, at(100), 50_000).expect("opens");
        tracker.submit("b", at(100) - 5_000).expect("submits");

        let latency = tracker.telemetry(LeaderHint::Unknown).latency;
        assert_eq!(latency.price_to_submit.sum_us, 0);
        assert_eq!(latency.price_to_submit.max_us, Some(0));
    }

    #[test]
    fn the_tip_summary_separates_money_that_moved_from_money_that_did_not() {
        let mut tracker = BundleTracker::default();
        open_and_submit(&mut tracker, "a", 100, 30_000);
        tracker.land("a", at(100)).expect("lands");
        open_and_submit(&mut tracker, "b", 100, 90_000);
        tracker.advance_to_slot(103, at(103), LeaderHint::Unknown);

        let tip = tracker.telemetry(LeaderHint::Unknown).tip;
        assert_eq!(tip.pricings, 2);
        assert_eq!(tip.committed_lamports, 120_000);
        assert_eq!(tip.paid_lamports, 30_000, "only the one that landed");
        assert_eq!(tip.forfeited_lamports, 90_000);
        assert_eq!(tip.min_lamports, Some(30_000));
        assert_eq!(tip.max_lamports, Some(90_000));
        assert_eq!(tip.mean_lamports, Some(60_000));
    }

    #[test]
    fn the_live_list_is_what_has_not_ended_yet_in_id_order() {
        let mut tracker = BundleTracker::default();
        for id in ["zulu", "alpha", "mike"] {
            open_and_submit(&mut tracker, id, 100, 50_000);
        }
        tracker.land("mike", at(100)).expect("lands");

        let live = tracker.telemetry(LeaderHint::Unknown).live;
        assert_eq!(
            live.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["alpha", "zulu"]
        );
        assert!(live.iter().all(|r| !r.state.terminal()));
    }

    #[test]
    fn the_live_counters_are_derived_rather_than_drifting() {
        let mut tracker = BundleTracker::default();
        tracker.open("a", 100, at(100), 50_000).expect("opens");
        open_and_submit(&mut tracker, "b", 100, 50_000);
        open_and_submit(&mut tracker, "c", 100, 50_000);
        tracker.land("c", at(100)).expect("lands");

        let counts = tracker.telemetry(LeaderHint::Unknown).counts;
        assert_eq!(counts.opened, 3);
        assert_eq!(counts.submitted, 2);
        assert_eq!(counts.live, 2, "a and b");
        assert_eq!(counts.in_flight, 1, "b only — a was never handed over");
    }

    #[test]
    fn telemetry_carries_the_floor_and_its_whole_working() {
        let mut tracker = BundleTracker::default();
        tracker.observe_slot(SlotObservation {
            landed_floor_lamports: 100_000,
            compute_units_used: 48_000_000,
            compute_unit_limit: 48_000_000,
            bundles_landed: 2,
            bundles_seen: 4,
            slot: 100,
        });

        let telemetry = tracker.telemetry(LeaderHint::Connected { wait_ms: 0 });
        assert_eq!(telemetry.floor.observed_lamports, 100_000);
        assert_eq!(telemetry.floor.saturation_micros, MICROS);
        assert_eq!(telemetry.floor.proximity_micros, Some(MICROS));
        assert_eq!(telemetry.floor.multiplier_micros, 2_250_000);
        assert_eq!(telemetry.floor.lamports, 225_000);
        assert_eq!(telemetry.land.window_micros, Some(500_000));
    }

    #[test]
    fn telemetry_serialises_in_the_shape_the_cockpit_reads() {
        // Every number the deck renders, reachable by a fixed path and already
        // a number. Nothing here needs parsing out of a string.
        let mut tracker = BundleTracker::default();
        tracker.observe_slot(SlotObservation {
            landed_floor_lamports: 100_000,
            bundles_landed: 1,
            bundles_seen: 2,
            ..SlotObservation::idle(100)
        });
        open_and_submit(&mut tracker, "b", 100, 50_000);
        tracker.land("b", at(100) + 30).expect("lands");

        let json = serde_json::to_value(tracker.telemetry(LeaderHint::NoneInReach))
            .expect("telemetry serialises");

        assert!(json["floor"]["lamports"].is_number());
        assert!(json["floor"]["multiplierMicros"].is_number());
        assert!(json["floor"]["saturationMicros"].is_number());
        assert_eq!(json["floor"]["proximityMicros"], 0);
        assert_eq!(json["floor"]["clamp"], "unclamped");
        assert_eq!(json["counts"]["landed"], 1);
        assert_eq!(json["counts"]["inFlight"], 0);
        assert_eq!(json["land"]["overallMicros"], 1_000_000);
        assert_eq!(json["land"]["windowMicros"], 500_000);
        assert_eq!(json["tip"]["paidLamports"], 50_000);
        assert_eq!(json["latency"]["priceToLand"]["sumUs"], 30_000);
        assert!(json["live"].as_array().expect("live is a list").is_empty());
    }

    #[test]
    fn an_unknown_schedule_leaves_the_proximity_field_null() {
        let telemetry = BundleTracker::default().telemetry(LeaderHint::Unknown);
        let json = serde_json::to_value(&telemetry).expect("serialises");
        assert!(
            json["floor"]["proximityMicros"].is_null(),
            "nobody looked, and it says so"
        );
    }

    // --- the shareable wrapper ---------------------------------------------

    #[test]
    fn the_deck_is_the_tracker_behind_a_lock() {
        let deck = BundleDeck::default();
        deck.observe_slot(SlotObservation {
            landed_floor_lamports: 100_000,
            bundles_landed: 1,
            bundles_seen: 1,
            ..SlotObservation::idle(100)
        });
        deck.open("b", 100, at(100), 50_000).expect("opens");
        deck.submit("b", at(100)).expect("submits");
        assert_eq!(deck.floor(LeaderHint::Unknown).lamports, 100_000);

        let dropped = deck.advance_to_slot(103, at(103), LeaderHint::Unknown);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].eviction, Some(EvictionReason::Retention));
        assert_eq!(
            deck.telemetry(LeaderHint::Unknown).counts.evicted_retention,
            1
        );
    }

    #[test]
    fn a_deck_is_shared_without_losing_a_transition() {
        // Every thread opens, submits and lands its own bundle. The counters
        // are exact afterwards whatever order the threads interleaved in.
        let deck = std::sync::Arc::new(BundleDeck::default());
        let mut threads = Vec::new();
        for index in 0..8u64 {
            let deck = std::sync::Arc::clone(&deck);
            threads.push(std::thread::spawn(move || {
                let id = format!("bundle-{index:02}");
                deck.open(&id, 100, at(100), 40_000).expect("opens");
                deck.submit(&id, at(100)).expect("submits");
                deck.land(&id, at(100) + 10).expect("lands");
            }));
        }
        for thread in threads {
            thread.join().expect("a thread finished");
        }

        let telemetry = deck.telemetry(LeaderHint::Unknown);
        assert_eq!(telemetry.counts.opened, 8);
        assert_eq!(telemetry.counts.landed, 8);
        assert_eq!(telemetry.counts.live, 0);
        assert_eq!(telemetry.tip.paid_lamports, 320_000);
    }

    // --- the states themselves ---------------------------------------------

    #[test]
    fn three_of_the_five_states_are_terminal() {
        let terminal: Vec<&str> = BundleState::ALL
            .iter()
            .filter(|state| state.terminal())
            .map(|state| state.name())
            .collect();
        assert_eq!(terminal, ["landed", "evicted", "rejected"]);
    }

    #[test]
    fn every_state_and_reason_has_a_name_the_cockpit_can_key_on() {
        for state in BundleState::ALL {
            let json = serde_json::to_string(&state).expect("serialises");
            assert_eq!(json, format!("\"{}\"", state.name()), "{state:?}");
        }
        for reason in [EvictionReason::Retention, EvictionReason::LeaderBoundary] {
            let json = serde_json::to_string(&reason).expect("serialises");
            assert_eq!(json, format!("\"{}\"", reason.name()), "{reason:?}");
        }
    }

    #[test]
    fn bundle_arithmetic_uses_no_floating_point() {
        let source = include_str!("bundle.rs");
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
                    number + 1
                );
            }
        }
    }
}
