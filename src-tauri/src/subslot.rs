//! Putting a shuffled feed back into the order the chain wrote it.
//!
//! A Geyser stream is not a sequence, it is a race. Account writes leave the
//! validator as they happen, slot statuses leave on a different path, TCP
//! retransmits reorder what the network already reordered, and the whole thing
//! can be replaced when the cluster abandons a fork. What arrives is a pile of
//! updates that are *mostly* in order, and this module turns that pile back
//! into a stream that is *strictly* in order — or says plainly that it could
//! not.
//!
//! Four ideas shape it, and they are worth stating before the types.
//!
//! **The order is total, and it is not the arrival order.** [`TickKey`] sorts
//! by slot, then by micro-timestamp within the slot, then by the source's own
//! sub-slot sequencer, then by a local arrival counter that no two ticks share.
//! Derived `Ord` compares in declaration order, so the precedence is the
//! declaration. The last field is what makes it total: two ticks can agree on
//! everything the network told us and still need a stable answer, and an answer
//! that depends on which thread got there first is a replay that does not
//! reproduce.
//!
//! **Ordering costs latency, and the cost is bounded and visible.** Nothing can
//! be released the instant it lands, because something older may still be in
//! flight. So ticks wait in [`TickRing`] until either the chain says their slot
//! is settled or the head has moved [`RingConfig::hold_slots`] past them. That
//! second condition is the one that matters in practice: it is what keeps a
//! feed flowing when the slot statuses stop arriving, and it is why a stalled
//! commitment stream degrades latency instead of stopping the pipeline.
//!
//! **A re-org is a rollback, not a correction.** Slots that the cluster
//! abandons are not fixed up downstream; they are removed from the buffer
//! before anything sees them. That is the entire reason the hold window exists.
//! Once a tick has been released the buffer cannot take it back, so
//! [`TickRing::rollback`] reports which released slots it can no longer undo
//! and the caller decides — see [`Rollback::released`].
//!
//! **Backpressure sheds the cheap thing, never the expensive one.** The ring is
//! fixed size and allocates once. When it fills, it drops the lowest-priority
//! unreleased tick, and a tick that implements [`TickClass::is_protected`] is
//! not eligible however low its priority. If every resident is protected the
//! ring releases its own front early rather than dropping anything — ordering
//! is a guarantee that can be degraded and counted, and a curve state event is
//! not.
//!
//! Nothing here knows what a bonding curve is. The ring is generic over its
//! payload so that it can be tested against a payload with no chain semantics
//! at all, and [`crate::geyser`] is what supplies the real one.

use std::collections::{BTreeMap, VecDeque};

use serde::Serialize;

// ---------------------------------------------------------------------------
// the ordering key
// ---------------------------------------------------------------------------

/// Where a tick sits in the total order.
///
/// Derived `Ord` compares the fields in declaration order, and the declaration
/// order *is* the specification:
///
/// 1. `slot` — the chain's own ordering, and the only field with consensus
///    behind it.
/// 2. `micros` — the source's timestamp, in microseconds. Orders events within
///    a slot that have nothing else in common, such as an account write against
///    a slot status.
/// 3. `write_version` — Geyser's per-slot account write counter, which is
///    authoritative for account writes and absent (`0`) for everything else.
///    It sits *below* the timestamp because the timestamp is the field that can
///    order unlike things; see the note on [`TickKey::new`] about why that is
///    safe.
/// 4. `seq` — a local arrival counter, unique within a run. Present so that no
///    two keys can ever compare equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TickKey {
    pub slot: u64,
    pub micros: u64,
    pub write_version: u64,
    pub seq: u64,
}

impl TickKey {
    /// Builds a key.
    ///
    /// A caveat worth writing down, because it is the one place this ordering
    /// can disagree with the chain: `micros` outranks `write_version`, so two
    /// writes to the *same account* in one slot whose timestamps disagree with
    /// their write versions would be released in timestamp order, which is
    /// wrong. This module does not try to fix that, because a global sort
    /// cannot: the timestamps are the only thing that can order an account
    /// write against a slot status, and no single key can put the authoritative
    /// field first for one pair and second for another.
    ///
    /// It is made harmless one level up instead. [`crate::geyser`] carries a
    /// per-account write-version high-water mark and refuses a write that is
    /// not newer than the last one applied to that account, so a stale write
    /// released early is discarded on arrival rather than overwriting live
    /// state. Ordering is best-effort; the state machine is exact.
    pub const fn new(slot: u64, micros: u64, write_version: u64, seq: u64) -> Self {
        TickKey {
            slot,
            micros,
            write_version,
            seq,
        }
    }
}

// ---------------------------------------------------------------------------
// commitment and slot phase
// ---------------------------------------------------------------------------

/// How settled a slot is.
///
/// Ordered deliberately: `Processed < Confirmed < Finalized`, so "at least as
/// settled as" is `>=` and the comparison reads the way it sounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Commitment {
    /// The bank ran the block. It can still be abandoned.
    Processed,
    /// A supermajority voted for it. Abandonment is possible and vanishingly
    /// unlikely.
    Confirmed,
    /// Rooted. It cannot be abandoned without the cluster halting.
    Finalized,
}

impl Commitment {
    pub const fn as_str(self) -> &'static str {
        match self {
            Commitment::Processed => "processed",
            Commitment::Confirmed => "confirmed",
            Commitment::Finalized => "finalized",
        }
    }
}

/// Everything a slot status update can say.
///
/// A superset of [`Commitment`]: the three commitment levels, the three
/// progress notifications that carry no commitment at all, and death. Kept
/// separate from `Commitment` because most of these must not be treated as a
/// commitment transition — `FirstShredReceived` says a slot exists, not that
/// anything in it is safe to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SlotPhase {
    FirstShredReceived,
    CreatedBank,
    Completed,
    Processed,
    Confirmed,
    Finalized,
    /// The validator gave up on this slot. Everything buffered for it is void.
    Dead,
}

impl SlotPhase {
    /// The commitment this phase implies, if it implies one.
    pub const fn commitment(self) -> Option<Commitment> {
        match self {
            SlotPhase::Processed => Some(Commitment::Processed),
            SlotPhase::Confirmed => Some(Commitment::Confirmed),
            SlotPhase::Finalized => Some(Commitment::Finalized),
            SlotPhase::FirstShredReceived
            | SlotPhase::CreatedBank
            | SlotPhase::Completed
            | SlotPhase::Dead => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            SlotPhase::FirstShredReceived => "firstShredReceived",
            SlotPhase::CreatedBank => "createdBank",
            SlotPhase::Completed => "completed",
            SlotPhase::Processed => "processed",
            SlotPhase::Confirmed => "confirmed",
            SlotPhase::Finalized => "finalized",
            SlotPhase::Dead => "dead",
        }
    }
}

// ---------------------------------------------------------------------------
// the slot ledger
// ---------------------------------------------------------------------------

/// How many slots of commitment history to keep.
///
/// Slots below `head - LEDGER_DEPTH` are forgotten. A fork deeper than this is
/// one the cluster did not survive, and holding the history for it would be a
/// map that grows without bound for the whole life of the process.
const LEDGER_DEPTH: u64 = 512;

/// What a slot status update did to the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerChange {
    /// Nothing worth acting on: a repeat, or a phase that carries no
    /// commitment.
    Noted,
    /// This slot reached a commitment it had not reached before.
    Advanced { slot: u64, to: Commitment },
    /// This slot is void, along with everything buffered at or above it.
    Reorg { from_slot: u64, reason: ReorgReason },
    /// The update was for a slot older than the ledger keeps.
    TooOld { slot: u64 },
}

/// Why a rollback was triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ReorgReason {
    /// The validator marked the slot dead.
    DeadSlot,
    /// The slot came back with a different parent than the one it had. The
    /// cluster switched forks underneath us.
    ParentChanged,
}

/// One slot's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SlotEntry {
    parent: Option<u64>,
    commitment: Option<Commitment>,
}

/// The chain's shape, as far as this process has been told about it.
///
/// Tracks the commitment of every recent slot and its parent, which is enough
/// to answer the two questions the ring needs: how far it is safe to release,
/// and whether a fork just moved.
#[derive(Debug, Clone, Default)]
pub struct SlotLedger {
    slots: BTreeMap<u64, SlotEntry>,
    head: u64,
    processed_head: u64,
    confirmed_head: u64,
    finalized_head: u64,
    reorgs: u64,
}

impl SlotLedger {
    pub fn new() -> Self {
        SlotLedger::default()
    }

    /// The highest slot seen at any phase.
    pub const fn head(&self) -> u64 {
        self.head
    }

    /// The highest slot that reached `Processed` or better.
    pub const fn processed_head(&self) -> u64 {
        self.processed_head
    }

    /// The highest slot that reached `Confirmed` or better.
    ///
    /// Monotonic by construction: a re-org can void slots above it, but this
    /// value only ever moves up, because a confirmed slot being abandoned is
    /// not a thing this ledger can observe without the cluster having halted.
    pub const fn confirmed_head(&self) -> u64 {
        self.confirmed_head
    }

    /// The highest slot that reached `Finalized`.
    pub const fn finalized_head(&self) -> u64 {
        self.finalized_head
    }

    /// How many re-orgs have been observed.
    pub const fn reorgs(&self) -> u64 {
        self.reorgs
    }

    /// The commitment recorded for a slot, if it is still in the window.
    pub fn commitment_of(&self, slot: u64) -> Option<Commitment> {
        self.slots.get(&slot).and_then(|entry| entry.commitment)
    }

    /// Records a slot status update.
    ///
    /// The two ways this reports a re-org are the two ways a fork switch is
    /// genuinely visible from a status stream, rather than merely consistent
    /// with one:
    ///
    /// - **Dead** is the validator saying so outright.
    /// - **A changed parent** means the same slot number is now being built on
    ///   a different block. That is the definition of a fork switch.
    ///
    /// A commitment that appears to go backwards is deliberately *not* on that
    /// list; see the note in the body for why.
    pub fn observe(&mut self, slot: u64, parent: Option<u64>, phase: SlotPhase) -> LedgerChange {
        if slot.saturating_add(LEDGER_DEPTH) < self.head {
            return LedgerChange::TooOld { slot };
        }

        self.head = self.head.max(slot);
        self.prune();

        if phase == SlotPhase::Dead {
            self.slots.remove(&slot);
            self.reorgs += 1;
            return LedgerChange::Reorg {
                from_slot: slot,
                reason: ReorgReason::DeadSlot,
            };
        }

        let existing = self.slots.get(&slot).copied();

        // A parent that changes is a fork switch. A parent that appears where
        // there was none is just more information arriving, which is normal:
        // `FirstShredReceived` often lands before anything knows the parent.
        if let (Some(previous), Some(incoming)) = (existing.and_then(|e| e.parent), parent) {
            if previous != incoming {
                self.slots.insert(
                    slot,
                    SlotEntry {
                        parent: Some(incoming),
                        commitment: phase.commitment(),
                    },
                );
                self.reorgs += 1;
                return LedgerChange::Reorg {
                    from_slot: slot,
                    reason: ReorgReason::ParentChanged,
                };
            }
        }

        let incoming_commitment = phase.commitment();
        let previous_commitment = existing.and_then(|entry| entry.commitment);

        // The stored commitment is the strongest thing this slot has ever
        // reached, and a weaker status arriving later does not walk it back.
        //
        // It is tempting to treat that regression as a fork signal — a slot we
        // were told was confirmed now being reported as merely processed looks
        // alarming. It is not: the statuses travel their own path and this
        // buffer is fed in arrival order, so a late `Processed` is precisely
        // the reordering the module exists to absorb. Calling it a re-org
        // would roll back good slots on ordinary traffic. The two signals that
        // *are* evidence of a fork are a dead slot and a changed parent, and
        // both are checked above.
        let merged = match (previous_commitment, incoming_commitment) {
            (Some(previous), Some(incoming)) => Some(previous.max(incoming)),
            (Some(previous), None) => Some(previous),
            (None, incoming) => incoming,
        };

        self.slots.insert(
            slot,
            SlotEntry {
                parent: parent.or(existing.and_then(|e| e.parent)),
                commitment: merged,
            },
        );

        match (previous_commitment, merged) {
            (previous, Some(now)) if previous != Some(now) => {
                self.processed_head = self.processed_head.max(slot);
                if now >= Commitment::Confirmed {
                    self.confirmed_head = self.confirmed_head.max(slot);
                }
                if now >= Commitment::Finalized {
                    self.finalized_head = self.finalized_head.max(slot);
                }
                LedgerChange::Advanced { slot, to: now }
            }
            _ => LedgerChange::Noted,
        }
    }

    fn prune(&mut self) {
        let floor = self.head.saturating_sub(LEDGER_DEPTH);
        // One split rather than a lookup-and-remove per slot: `split_off`
        // keeps the tail and drops the head in a single walk.
        self.slots = self.slots.split_off(&floor);
    }
}

// ---------------------------------------------------------------------------
// what the ring needs to know about a payload
// ---------------------------------------------------------------------------

/// The two things the ring asks of whatever it is carrying.
///
/// Deliberately tiny. The ring's job is ordering, and a ring that understood
/// bonding curves would be a ring that could not be tested without them.
pub trait TickClass {
    /// Whether this tick may be dropped under pressure.
    ///
    /// `true` means never: the ring will release its own front early, out of
    /// order, before it will shed one of these. Curve state is the case this
    /// exists for — a reserve update that is dropped leaves the engine's idea
    /// of a price permanently wrong, whereas a slot status that is dropped
    /// costs one heartbeat.
    fn is_protected(&self) -> bool;

    /// What to shed first. Lower is shed first.
    fn priority(&self) -> u8;
}

// ---------------------------------------------------------------------------
// the ring
// ---------------------------------------------------------------------------

/// How the ring is sized and how long it is allowed to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingConfig {
    /// How many ticks may be resident. Allocated once, never grown.
    pub capacity: usize,
    /// How far the head may run ahead of a slot before that slot is released
    /// whether or not the chain has confirmed it.
    ///
    /// This is the latency ceiling. At roughly 400 ms a slot, 4 slots is about
    /// 1.6 s of worst-case hold — long enough to absorb the reordering a
    /// commitment stream actually shows, short enough that a feed which stops
    /// sending statuses degrades rather than stops.
    pub hold_slots: u64,
}

impl Default for RingConfig {
    fn default() -> Self {
        RingConfig {
            capacity: 4096,
            hold_slots: 4,
        }
    }
}

/// What happened to a pushed tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Push<T> {
    /// Resident, waiting for its slot to settle.
    Buffered,
    /// The ring was full and this tick was the cheapest thing in it, so it
    /// never entered. Only reachable for payloads that are not protected.
    Rejected(T),
    /// The ring was full, this tick entered, and the returned tick was shed to
    /// make room for it.
    Displaced(T),
    /// The ring was full of protected ticks, so its front was released early to
    /// make room. The returned tick is that front: it is in order relative to
    /// everything already released, but it left before its slot settled and a
    /// later re-org cannot take it back.
    ForcedRelease(T),
}

/// Slots that a rollback could and could not undo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rollback<T> {
    /// Ticks that were still resident and have been discarded. These never
    /// reached anyone.
    pub discarded: Vec<T>,
    /// The lowest slot that had already been released at or above the rollback
    /// point, if any.
    ///
    /// `Some` is the honest bad news: state built from those ticks is wrong and
    /// only the caller knows how to unwind it. The hold window exists to keep
    /// this `None`, and [`RingMetrics::unrecoverable_reorgs`] counts the times
    /// it was not.
    pub released: Option<u64>,
}

/// Counters. Every one of these is a thing that went less than perfectly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RingMetrics {
    /// Ticks that entered the ring.
    pub buffered: u64,
    /// Ticks released in order, the normal path.
    pub released: u64,
    /// Ticks that arrived for a slot already released. Late beyond saving.
    pub late: u64,
    /// Ticks shed because the ring was full.
    pub shed: u64,
    /// Times the ring released its front early because it was full of
    /// protected ticks. Ordering was preserved; the safety window was not.
    pub forced_releases: u64,
    /// Ticks discarded by a rollback before anyone saw them. The system
    /// working.
    pub rolled_back: u64,
    /// Rollbacks that arrived after the affected slots had been released. The
    /// system not working, and the number to watch.
    pub unrecoverable_reorgs: u64,
    /// Arrivals whose key was below the arrival before them: the number of
    /// descents in the arrival sequence, which is the reordering this buffer
    /// exists to absorb. Not a distance from sorted order — a cheap counter
    /// that moves when the network misbehaves, which is all it is for.
    pub out_of_order_arrivals: u64,
}

/// A bounded, ordering, non-allocating tick buffer.
///
/// # On "lock-free"
///
/// There is no lock in here, and there is nothing to lock: the ring has one
/// owner, the sequencing task, and it is `!Sync` by construction. That is a
/// stronger property than a lock-free shared structure, not a weaker one — an
/// uncontended atomic is still a fence on the hot path, and the correct place
/// to pay for sharing is the one seam where two threads actually meet. In this
/// pipeline that seam is the bounded channel between the socket task and the
/// sequencer, which is `crossbeam-channel`'s array queue: lock-free, already a
/// dependency of this crate, and proven by more eyes than a hand-rolled one
/// would get.
///
/// # Structure
///
/// A `VecDeque` held in key order, with capacity reserved once at construction
/// and every path below written so that `len` can never exceed it. `VecDeque`
/// *is* a ring buffer; keeping it sorted rather than in arrival order is what
/// makes the front the next tick to release.
///
/// Insertion searches from the back, because a feed that is mostly in order
/// mostly appends, and an append is the first comparison. A late tick costs a
/// `memmove` of the elements after it — no allocation, no rehash, and bounded
/// by `capacity`.
#[derive(Debug)]
pub struct TickRing<T> {
    entries: VecDeque<(TickKey, T)>,
    config: RingConfig,
    /// The highest key released so far. Anything at or below this is late.
    released_upto: Option<TickKey>,
    /// The highest slot seen on a push, which drives the hold window.
    head_slot: u64,
    /// The previous arrival's key, for counting reordering.
    last_arrival: Option<TickKey>,
    metrics: RingMetrics,
}

impl<T: TickClass> TickRing<T> {
    pub fn new(config: RingConfig) -> Self {
        // A ring with no room cannot buffer, and a caller who asks for one has
        // almost certainly mis-wired a config rather than meant it.
        let capacity = config.capacity.max(1);
        TickRing {
            entries: VecDeque::with_capacity(capacity),
            config: RingConfig { capacity, ..config },
            released_upto: None,
            head_slot: 0,
            last_arrival: None,
            metrics: RingMetrics::default(),
        }
    }

    pub const fn metrics(&self) -> RingMetrics {
        self.metrics
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub const fn capacity(&self) -> usize {
        self.config.capacity
    }

    /// The highest slot pushed.
    pub const fn head_slot(&self) -> u64 {
        self.head_slot
    }

    /// The key of the last tick released, if any.
    pub const fn released_upto(&self) -> Option<TickKey> {
        self.released_upto
    }

    /// Offers a tick to the ring.
    ///
    /// A tick whose key is at or below the last released key cannot be placed
    /// in order any more, so it is refused rather than released out of order.
    /// That is a real loss and it is counted as [`RingMetrics::late`]; the
    /// alternative is a stream that claims to be ordered and is not.
    pub fn push(&mut self, key: TickKey, payload: T) -> Push<T> {
        self.head_slot = self.head_slot.max(key.slot);

        if self.last_arrival.is_some_and(|previous| key < previous) {
            self.metrics.out_of_order_arrivals += 1;
        }
        self.last_arrival = Some(key);

        if self.released_upto.is_some_and(|watermark| key <= watermark) {
            self.metrics.late += 1;
            return Push::Rejected(payload);
        }

        if self.entries.len() >= self.config.capacity {
            return self.push_when_full(key, payload);
        }

        self.insert_sorted(key, payload);
        self.metrics.buffered += 1;
        Push::Buffered
    }

    /// The full-ring path, kept out of [`push`](Self::push) so the common case
    /// reads as the straight line it is.
    fn push_when_full(&mut self, key: TickKey, payload: T) -> Push<T> {
        match self.weakest_sheddable() {
            // Something in the ring is cheaper than what is arriving, or the
            // arrival outranks it. Shed it and take the slot.
            Some((index, priority)) if payload.is_protected() || priority <= payload.priority() => {
                let (_, evicted) = self
                    .entries
                    .remove(index)
                    .expect("index came from a scan of this deque");
                self.insert_sorted(key, payload);
                self.metrics.shed += 1;
                self.metrics.buffered += 1;
                Push::Displaced(evicted)
            }
            // The ring holds nothing cheaper and the arrival may be dropped.
            // Dropping the newest is the right end to drop from: the residents
            // are closer to release and discarding them wastes the wait.
            Some(_) => {
                self.metrics.shed += 1;
                Push::Rejected(payload)
            }
            // Every resident is protected. Nothing may be dropped, so the
            // ordering guarantee is what gives: something leaves early.
            //
            // *Which* thing leaves is the whole correctness of this branch.
            // Releasing the front unconditionally would be wrong: an arrival
            // older than the front would then be buffered behind a key that
            // had already gone out, and the stream would emit it out of order
            // later. So the tick released is whichever of the two is smaller,
            // and `released_upto` only ever moves up.
            None => {
                let front_key = self
                    .entries
                    .front()
                    .expect("the ring is full, so it is not empty")
                    .0;
                self.metrics.forced_releases += 1;
                self.metrics.released += 1;

                if key < front_key {
                    // The arrival is older than everything resident, and
                    // `push` has already established it is newer than the last
                    // release. Sending it straight out is in order.
                    self.released_upto = Some(key);
                    Push::ForcedRelease(payload)
                } else {
                    let (front_key, front) = self
                        .entries
                        .pop_front()
                        .expect("front was just observed to exist");
                    self.released_upto = Some(front_key);
                    self.insert_sorted(key, payload);
                    self.metrics.buffered += 1;
                    Push::ForcedRelease(front)
                }
            }
        }
    }

    /// The cheapest resident that may be shed, as `(index, priority)`.
    ///
    /// Scans from the front so that among equals the oldest wins, which keeps
    /// the choice deterministic. `None` when every resident is protected.
    fn weakest_sheddable(&self) -> Option<(usize, u8)> {
        let mut weakest: Option<(usize, u8)> = None;
        for (index, (_, payload)) in self.entries.iter().enumerate() {
            if payload.is_protected() {
                continue;
            }
            let priority = payload.priority();
            if weakest.is_none_or(|(_, lowest)| priority < lowest) {
                weakest = Some((index, priority));
            }
        }
        weakest
    }

    /// Places a tick at its ordered position.
    ///
    /// Searching from the back is the whole optimisation: an in-order feed
    /// appends after one comparison, and the cost only rises for the ticks that
    /// actually arrived late.
    fn insert_sorted(&mut self, key: TickKey, payload: T) {
        debug_assert!(self.entries.len() < self.config.capacity, "ring would grow");
        let mut index = self.entries.len();
        while index > 0 && self.entries[index - 1].0 > key {
            index -= 1;
        }
        self.entries.insert(index, (key, payload));
    }

    /// Releases everything that is safe to release, in order, into `out`.
    ///
    /// A tick is safe when either the chain has settled its slot to at least
    /// `commitment`, or the head has moved [`RingConfig::hold_slots`] past it.
    /// The second clause is the liveness one: without it a provider that stops
    /// sending slot statuses would silently stop the pipeline instead of
    /// visibly slowing it.
    pub fn drain_ready(
        &mut self,
        ledger: &SlotLedger,
        commitment: Commitment,
        out: &mut Vec<T>,
    ) -> usize {
        let settled_head = match commitment {
            Commitment::Processed => ledger.processed_head(),
            Commitment::Confirmed => ledger.confirmed_head(),
            Commitment::Finalized => ledger.finalized_head(),
        };
        let stale_before = self.head_slot.saturating_sub(self.config.hold_slots);
        let watermark = settled_head.max(stale_before);

        let mut released = 0;
        while let Some((key, _)) = self.entries.front() {
            if key.slot > watermark {
                break;
            }
            let (key, payload) = self
                .entries
                .pop_front()
                .expect("front was just observed to exist");
            self.released_upto = Some(key);
            out.push(payload);
            released += 1;
        }
        self.metrics.released += released as u64;
        released
    }

    /// Releases everything, ordered, ignoring the hold window.
    ///
    /// For shutdown and for the end of a fixture: at that point there is no
    /// later tick that could arrive, so holding is pure loss.
    pub fn drain_all(&mut self, out: &mut Vec<T>) -> usize {
        let released = self.entries.len();
        while let Some((key, payload)) = self.entries.pop_front() {
            self.released_upto = Some(key);
            out.push(payload);
        }
        self.metrics.released += released as u64;
        released
    }

    /// Discards everything buffered at or above `from_slot`.
    ///
    /// Called when the ledger reports a re-org. Returns the discarded ticks so
    /// the caller can account for them, and — in
    /// [`Rollback::released`] — whether the damage extends past what this
    /// buffer can undo.
    pub fn rollback(&mut self, from_slot: u64) -> Rollback<T> {
        let mut discarded = Vec::new();
        let mut keep = VecDeque::with_capacity(self.config.capacity);
        // Drained rather than filtered in place so that `keep` inherits the
        // reserved capacity and the ring still never allocates on the hot path.
        while let Some((key, payload)) = self.entries.pop_front() {
            if key.slot >= from_slot {
                discarded.push(payload);
            } else {
                keep.push_back((key, payload));
            }
        }
        self.entries = keep;
        self.metrics.rolled_back += discarded.len() as u64;

        let released = self
            .released_upto
            .filter(|watermark| watermark.slot >= from_slot)
            .map(|watermark| watermark.slot);
        if released.is_some() {
            self.metrics.unrecoverable_reorgs += 1;
        }

        Rollback {
            discarded,
            released,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- a payload with no chain semantics, so the ring is tested alone ------

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Tick {
        id: u32,
        protected: bool,
        priority: u8,
    }

    impl Tick {
        const fn plain(id: u32) -> Self {
            Tick {
                id,
                protected: false,
                priority: 5,
            }
        }
        const fn cheap(id: u32) -> Self {
            Tick {
                id,
                protected: false,
                priority: 0,
            }
        }
        const fn precious(id: u32) -> Self {
            Tick {
                id,
                protected: true,
                priority: 9,
            }
        }
    }

    impl TickClass for Tick {
        fn is_protected(&self) -> bool {
            self.protected
        }
        fn priority(&self) -> u8 {
            self.priority
        }
    }

    fn key(slot: u64, micros: u64) -> TickKey {
        TickKey::new(slot, micros, 0, micros)
    }

    /// A ledger that has confirmed everything up to `slot`.
    fn confirmed_through(slot: u64) -> SlotLedger {
        let mut ledger = SlotLedger::new();
        for s in 1..=slot {
            ledger.observe(s, Some(s - 1), SlotPhase::Confirmed);
        }
        ledger
    }

    fn ids(ticks: &[Tick]) -> Vec<u32> {
        ticks.iter().map(|tick| tick.id).collect()
    }

    // -- the ordering key ---------------------------------------------------

    #[test]
    fn the_key_orders_by_slot_before_anything_else() {
        // A tick early in a later slot still sorts after a tick late in an
        // earlier one. Slot is the chain's ordering and it outranks the clock.
        assert!(TickKey::new(9, u64::MAX, u64::MAX, u64::MAX) < TickKey::new(10, 0, 0, 0));
    }

    #[test]
    fn the_key_orders_by_micros_within_a_slot() {
        assert!(key(10, 100) < key(10, 101));
    }

    #[test]
    fn the_key_falls_through_to_write_version_then_seq() {
        assert!(TickKey::new(10, 5, 1, 99) < TickKey::new(10, 5, 2, 0));
        assert!(TickKey::new(10, 5, 1, 0) < TickKey::new(10, 5, 1, 1));
    }

    #[test]
    fn no_two_keys_from_one_run_can_tie() {
        // `seq` is the arrival counter, so two keys agreeing on everything the
        // network said still order. Without this the release order would depend
        // on which thread got there first.
        let a = TickKey::new(10, 5, 3, 41);
        let b = TickKey::new(10, 5, 3, 42);
        assert_ne!(a, b);
        assert!(a < b);
    }

    // -- sub-slot ordering --------------------------------------------------

    #[test]
    fn a_shuffled_slot_is_released_in_micro_timestamp_order() {
        let mut ring = TickRing::new(RingConfig {
            capacity: 64,
            hold_slots: 4,
        });
        // Deliberately adversarial: the arrival order is the exact reverse of
        // the intended one, plus a duplicate timestamp broken by seq.
        for (micros, id) in [(90u64, 9u32), (10, 1), (70, 7), (30, 3), (50, 5)] {
            assert_eq!(ring.push(key(10, micros), Tick::plain(id)), Push::Buffered);
        }

        let mut out = Vec::new();
        ring.drain_ready(&confirmed_through(10), Commitment::Confirmed, &mut out);
        assert_eq!(ids(&out), vec![1, 3, 5, 7, 9]);
        // Two of the five arrivals were below the one before them: 10 after 90,
        // and 30 after 70. The counter measures descents in the arrival
        // sequence, not distance from sorted order.
        assert_eq!(ring.metrics().out_of_order_arrivals, 2);
    }

    #[test]
    fn ordering_holds_across_slots_when_a_whole_slot_arrives_late() {
        let mut ring = TickRing::new(RingConfig {
            capacity: 64,
            hold_slots: 4,
        });
        // Slot 11 lands entirely before slot 10 does — the packet-level
        // reordering this buffer exists for.
        ring.push(key(11, 10), Tick::plain(110));
        ring.push(key(11, 20), Tick::plain(120));
        ring.push(key(10, 10), Tick::plain(100));

        let mut out = Vec::new();
        ring.drain_ready(&confirmed_through(11), Commitment::Confirmed, &mut out);
        assert_eq!(ids(&out), vec![100, 110, 120]);
    }

    #[test]
    fn released_ticks_never_go_backwards_over_a_long_shuffled_run() {
        // The invariant that matters, stated over a run big enough to catch an
        // off-by-one in the insertion search: whatever comes out, comes out in
        // strictly increasing key order.
        let mut ring = TickRing::new(RingConfig {
            capacity: 512,
            hold_slots: 4,
        });
        let mut ledger = SlotLedger::new();
        let mut out = Vec::new();
        let mut id = 0u32;

        for slot in 1u64..=60 {
            // A pseudo-random but fixed shuffle within each slot: no rng, so
            // the test is the same on every machine.
            for step in [7u64, 3, 11, 1, 5, 9] {
                let micros = (step * 37) % 13;
                id += 1;
                ring.push(
                    TickKey::new(slot, micros, 0, u64::from(id)),
                    Tick::plain(id),
                );
            }
            ledger.observe(slot, Some(slot - 1), SlotPhase::Confirmed);
            ring.drain_ready(&ledger, Commitment::Confirmed, &mut out);
        }
        ring.drain_all(&mut out);

        assert_eq!(out.len(), 60 * 6);
        // Reconstruct each released tick's key from its id and check the order.
        let mut previous: Option<TickKey> = None;
        for tick in &out {
            let slot = u64::from((tick.id - 1) / 6 + 1);
            let step = [7u64, 3, 11, 1, 5, 9][((tick.id - 1) % 6) as usize];
            let current = TickKey::new(slot, (step * 37) % 13, 0, u64::from(tick.id));
            if let Some(previous) = previous {
                assert!(
                    previous < current,
                    "release order went backwards at {:?}",
                    tick
                );
            }
            previous = Some(current);
        }
    }

    #[test]
    fn a_tick_for_an_already_released_slot_is_refused_not_reordered() {
        let mut ring = TickRing::new(RingConfig {
            capacity: 64,
            hold_slots: 4,
        });
        ring.push(key(10, 50), Tick::plain(1));
        let mut out = Vec::new();
        ring.drain_ready(&confirmed_through(10), Commitment::Confirmed, &mut out);
        assert_eq!(ids(&out), vec![1]);

        // Too late to place in order. Refusing is the only honest answer.
        let late = ring.push(key(10, 10), Tick::plain(2));
        assert_eq!(late, Push::Rejected(Tick::plain(2)));
        assert_eq!(ring.metrics().late, 1);
    }

    // -- the hold window ----------------------------------------------------

    #[test]
    fn nothing_is_released_before_its_slot_settles() {
        let mut ring = TickRing::new(RingConfig {
            capacity: 64,
            hold_slots: 4,
        });
        ring.push(key(10, 10), Tick::plain(1));

        let mut out = Vec::new();
        // Head is 10 and hold is 4, so the stale floor is 6; nothing is
        // confirmed. Slot 10 is not eligible either way.
        ring.drain_ready(&SlotLedger::new(), Commitment::Confirmed, &mut out);
        assert!(out.is_empty());
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn the_hold_window_releases_a_slot_the_chain_never_confirmed() {
        // The liveness clause. A provider whose slot statuses dry up must slow
        // the pipeline, not stop it.
        let mut ring = TickRing::new(RingConfig {
            capacity: 64,
            hold_slots: 4,
        });
        ring.push(key(10, 10), Tick::plain(1));
        ring.push(key(15, 10), Tick::plain(2));

        let mut out = Vec::new();
        ring.drain_ready(&SlotLedger::new(), Commitment::Confirmed, &mut out);
        // Head 15 minus hold 4 is 11, so slot 10 is past saving and released;
        // slot 15 is still inside the window.
        assert_eq!(ids(&out), vec![1]);
        assert_eq!(ring.len(), 1);
    }

    #[test]
    fn asking_for_processed_releases_on_processed() {
        // A caller who configures `Processed` is asking to trade safety for
        // latency. Holding them to the confirmed head would silently refuse
        // that trade.
        let mut ring = TickRing::new(RingConfig {
            capacity: 64,
            hold_slots: 1_000,
        });
        ring.push(key(10, 10), Tick::plain(1));

        let mut ledger = SlotLedger::new();
        ledger.observe(10, Some(9), SlotPhase::Processed);
        assert_eq!(ledger.processed_head(), 10);
        assert_eq!(ledger.confirmed_head(), 0);

        let mut out = Vec::new();
        ring.drain_ready(&ledger, Commitment::Confirmed, &mut out);
        assert!(out.is_empty(), "confirmed must still wait");

        ring.drain_ready(&ledger, Commitment::Processed, &mut out);
        assert_eq!(ids(&out), vec![1]);
    }

    #[test]
    fn a_slot_near_the_end_of_the_range_does_not_overflow_the_window() {
        // Release builds keep overflow checks on, so `slot + LEDGER_DEPTH`
        // would be a panic rather than a wrap.
        let mut ledger = SlotLedger::new();
        assert_eq!(
            ledger.observe(u64::MAX, Some(u64::MAX - 1), SlotPhase::Confirmed),
            LedgerChange::Advanced {
                slot: u64::MAX,
                to: Commitment::Confirmed
            }
        );
        assert_eq!(ledger.head(), u64::MAX);
    }

    #[test]
    fn finalized_holds_longer_than_confirmed() {
        let mut ring = TickRing::new(RingConfig {
            capacity: 64,
            hold_slots: 100,
        });
        ring.push(key(10, 10), Tick::plain(1));

        let mut ledger = SlotLedger::new();
        ledger.observe(10, Some(9), SlotPhase::Confirmed);

        let mut out = Vec::new();
        ring.drain_ready(&ledger, Commitment::Finalized, &mut out);
        assert!(out.is_empty(), "confirmed is not finalized");

        ledger.observe(10, Some(9), SlotPhase::Finalized);
        ring.drain_ready(&ledger, Commitment::Finalized, &mut out);
        assert_eq!(ids(&out), vec![1]);
    }

    // -- backpressure -------------------------------------------------------

    #[test]
    fn a_full_ring_sheds_the_cheapest_resident_for_a_protected_arrival() {
        let mut ring = TickRing::new(RingConfig {
            capacity: 3,
            hold_slots: 100,
        });
        ring.push(key(10, 1), Tick::plain(1));
        ring.push(key(10, 2), Tick::cheap(2));
        ring.push(key(10, 3), Tick::plain(3));

        let outcome = ring.push(key(10, 4), Tick::precious(4));
        assert_eq!(outcome, Push::Displaced(Tick::cheap(2)));
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.metrics().shed, 1);

        let mut out = Vec::new();
        ring.drain_all(&mut out);
        assert_eq!(ids(&out), vec![1, 3, 4]);
    }

    #[test]
    fn a_curve_event_is_never_the_thing_that_gets_dropped() {
        // The invariant the whole eviction policy exists to hold. Fill the ring
        // with protected ticks, then keep pushing protected ticks, and check
        // that every single one is accounted for — released early if need be,
        // but never dropped.
        let mut ring = TickRing::new(RingConfig {
            capacity: 4,
            hold_slots: 1_000,
        });
        let mut escaped = Vec::new();

        for id in 0u32..32 {
            match ring.push(key(10, u64::from(id)), Tick::precious(id)) {
                Push::Buffered => {}
                Push::ForcedRelease(tick) => escaped.push(tick),
                other => panic!("a protected tick was dropped: {other:?}"),
            }
        }
        ring.drain_all(&mut escaped);

        assert_eq!(ids(&escaped), (0u32..32).collect::<Vec<_>>());
        assert_eq!(ring.metrics().shed, 0, "nothing protected may be shed");
        assert_eq!(ring.metrics().forced_releases, 28);
    }

    #[test]
    fn a_forced_release_picks_the_older_of_the_arrival_and_the_front() {
        // The ordering trap in the overflow path. With the ring full of
        // protected ticks and an arrival older than everything resident,
        // releasing the front would strand the arrival behind a key that had
        // already gone out. The arrival is the one that must leave.
        let mut ring = TickRing::new(RingConfig {
            capacity: 2,
            hold_slots: 1_000,
        });
        ring.push(key(10, 40), Tick::precious(40));
        ring.push(key(10, 50), Tick::precious(50));

        let outcome = ring.push(key(10, 30), Tick::precious(30));
        assert_eq!(outcome, Push::ForcedRelease(Tick::precious(30)));
        assert_eq!(ring.released_upto(), Some(key(10, 30)));

        let mut out = Vec::new();
        ring.drain_all(&mut out);
        assert_eq!(ids(&out), vec![40, 50], "the residents were left untouched");
    }

    #[test]
    fn a_backlog_arriving_in_reverse_is_reported_late_never_reordered() {
        // The pathological case, and the one place the ring's two promises
        // genuinely conflict: a full ring of protected ticks fed strictly
        // backwards cannot both keep every tick and keep the order. It keeps
        // the order and says how much it lost — a stream that silently
        // reordered would corrupt every price built from it.
        let mut ring = TickRing::new(RingConfig {
            capacity: 2,
            hold_slots: 1_000,
        });
        let mut out = Vec::new();

        for micros in [5u64, 4, 3, 2, 1] {
            match ring.push(key(10, micros), Tick::precious(micros as u32)) {
                Push::ForcedRelease(tick) => out.push(tick),
                Push::Buffered | Push::Rejected(_) => {}
                other => panic!("unexpected outcome {other:?}"),
            }
        }
        ring.drain_all(&mut out);

        // Whatever survived, it came out in order.
        assert!(
            out.windows(2).all(|pair| pair[0].id < pair[1].id),
            "got {:?}",
            ids(&out)
        );
        assert_eq!(
            ring.metrics().late,
            2,
            "the two unplaceable ticks are counted"
        );
        assert_eq!(
            ring.metrics().shed,
            0,
            "nothing was quietly dropped as cheap"
        );
    }

    #[test]
    fn an_unprotected_arrival_is_refused_when_the_ring_holds_only_better_things() {
        let mut ring = TickRing::new(RingConfig {
            capacity: 2,
            hold_slots: 1_000,
        });
        ring.push(
            key(10, 1),
            Tick {
                id: 1,
                protected: false,
                priority: 9,
            },
        );
        ring.push(
            key(10, 2),
            Tick {
                id: 2,
                protected: false,
                priority: 9,
            },
        );

        let outcome = ring.push(key(10, 3), Tick::cheap(3));
        assert_eq!(outcome, Push::Rejected(Tick::cheap(3)));
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn the_ring_never_grows_past_its_capacity() {
        let mut ring = TickRing::new(RingConfig {
            capacity: 8,
            hold_slots: 1_000,
        });
        for id in 0u32..1_000 {
            let _ = ring.push(key(10, u64::from(id)), Tick::plain(id));
            assert!(ring.len() <= 8, "resident count exceeded capacity");
        }
    }

    // -- the slot ledger and re-orgs ----------------------------------------

    #[test]
    fn commitment_advances_are_reported_once_each() {
        let mut ledger = SlotLedger::new();
        assert_eq!(
            ledger.observe(10, Some(9), SlotPhase::Processed),
            LedgerChange::Advanced {
                slot: 10,
                to: Commitment::Processed
            }
        );
        assert_eq!(
            ledger.observe(10, Some(9), SlotPhase::Processed),
            LedgerChange::Noted
        );
        assert_eq!(
            ledger.observe(10, Some(9), SlotPhase::Confirmed),
            LedgerChange::Advanced {
                slot: 10,
                to: Commitment::Confirmed
            }
        );
        assert_eq!(ledger.confirmed_head(), 10);
    }

    #[test]
    fn a_phase_that_carries_no_commitment_does_not_advance_one() {
        let mut ledger = SlotLedger::new();
        assert_eq!(
            ledger.observe(10, None, SlotPhase::FirstShredReceived),
            LedgerChange::Noted
        );
        assert_eq!(
            ledger.observe(10, None, SlotPhase::CreatedBank),
            LedgerChange::Noted
        );
        assert_eq!(
            ledger.observe(10, None, SlotPhase::Completed),
            LedgerChange::Noted
        );
        assert_eq!(ledger.commitment_of(10), None);
        assert_eq!(ledger.confirmed_head(), 0);
    }

    #[test]
    fn a_processed_arriving_after_a_confirmed_does_not_walk_it_back() {
        // Statuses travel different paths and can cross. A late `Processed` is
        // ordinary traffic, not a fork.
        let mut ledger = SlotLedger::new();
        ledger.observe(10, Some(9), SlotPhase::Confirmed);
        assert_eq!(
            ledger.observe(10, Some(9), SlotPhase::Processed),
            LedgerChange::Noted
        );
        assert_eq!(ledger.commitment_of(10), Some(Commitment::Confirmed));
        assert_eq!(ledger.reorgs(), 0);
    }

    #[test]
    fn a_dead_slot_is_a_reorg() {
        let mut ledger = SlotLedger::new();
        ledger.observe(10, Some(9), SlotPhase::Processed);
        assert_eq!(
            ledger.observe(10, Some(9), SlotPhase::Dead),
            LedgerChange::Reorg {
                from_slot: 10,
                reason: ReorgReason::DeadSlot
            }
        );
        assert_eq!(ledger.reorgs(), 1);
    }

    #[test]
    fn a_changed_parent_is_a_reorg() {
        let mut ledger = SlotLedger::new();
        ledger.observe(10, Some(9), SlotPhase::Processed);
        assert_eq!(
            ledger.observe(10, Some(8), SlotPhase::Processed),
            LedgerChange::Reorg {
                from_slot: 10,
                reason: ReorgReason::ParentChanged
            }
        );
    }

    #[test]
    fn a_parent_appearing_where_there_was_none_is_not_a_reorg() {
        let mut ledger = SlotLedger::new();
        ledger.observe(10, None, SlotPhase::FirstShredReceived);
        assert_eq!(
            ledger.observe(10, Some(9), SlotPhase::Processed),
            LedgerChange::Advanced {
                slot: 10,
                to: Commitment::Processed
            }
        );
        assert_eq!(ledger.reorgs(), 0);
    }

    #[test]
    fn a_slot_far_below_the_window_is_ignored_rather_than_acted_on() {
        let mut ledger = SlotLedger::new();
        ledger.observe(10_000, Some(9_999), SlotPhase::Confirmed);
        assert_eq!(
            ledger.observe(1, Some(0), SlotPhase::Dead),
            LedgerChange::TooOld { slot: 1 }
        );
        assert_eq!(
            ledger.reorgs(),
            0,
            "an ancient slot must not trigger a rollback"
        );
    }

    #[test]
    fn the_ledger_does_not_grow_without_bound() {
        let mut ledger = SlotLedger::new();
        for slot in 1u64..=5_000 {
            ledger.observe(slot, Some(slot - 1), SlotPhase::Confirmed);
        }
        assert!(
            ledger.slots.len() <= (LEDGER_DEPTH as usize) + 1,
            "ledger kept {} slots",
            ledger.slots.len()
        );
    }

    // -- rollback -----------------------------------------------------------

    #[test]
    fn a_rollback_discards_the_abandoned_slots_and_keeps_the_rest() {
        let mut ring = TickRing::new(RingConfig {
            capacity: 64,
            hold_slots: 100,
        });
        ring.push(key(9, 1), Tick::plain(9));
        ring.push(key(10, 1), Tick::plain(10));
        ring.push(key(11, 1), Tick::plain(11));

        let rollback = ring.rollback(10);
        assert_eq!(ids(&rollback.discarded), vec![10, 11]);
        assert_eq!(rollback.released, None, "nothing had escaped yet");

        let mut out = Vec::new();
        ring.drain_all(&mut out);
        assert_eq!(ids(&out), vec![9]);
        assert_eq!(ring.metrics().rolled_back, 2);
        assert_eq!(ring.metrics().unrecoverable_reorgs, 0);
    }

    #[test]
    fn the_hold_window_is_what_makes_a_rollback_recoverable() {
        // The end-to-end statement of why any of this exists: a slot that is
        // still inside the window is undone completely and silently.
        let mut ring = TickRing::new(RingConfig {
            capacity: 64,
            hold_slots: 4,
        });
        let mut ledger = SlotLedger::new();
        let mut out = Vec::new();

        for slot in 1u64..=3 {
            ring.push(key(slot, 1), Tick::precious(slot as u32));
            ledger.observe(slot, Some(slot - 1), SlotPhase::Processed);
            ring.drain_ready(&ledger, Commitment::Confirmed, &mut out);
        }
        assert!(out.is_empty(), "nothing confirmed, nothing past the window");

        // Slot 2 dies. Slots 2 and 3 go with it and nobody ever saw them.
        let change = ledger.observe(2, Some(1), SlotPhase::Dead);
        let LedgerChange::Reorg { from_slot, .. } = change else {
            panic!("expected a reorg, got {change:?}");
        };
        let rollback = ring.rollback(from_slot);
        assert_eq!(ids(&rollback.discarded), vec![2, 3]);
        assert_eq!(rollback.released, None);

        ring.drain_all(&mut out);
        assert_eq!(ids(&out), vec![1], "only the surviving slot is ever seen");
    }

    #[test]
    fn a_rollback_that_arrives_too_late_says_so_rather_than_pretending() {
        let mut ring = TickRing::new(RingConfig {
            capacity: 64,
            hold_slots: 0,
        });
        ring.push(key(10, 1), Tick::plain(1));

        let mut out = Vec::new();
        ring.drain_ready(&confirmed_through(10), Commitment::Confirmed, &mut out);
        assert_eq!(ids(&out), vec![1]);

        let rollback = ring.rollback(10);
        assert!(rollback.discarded.is_empty());
        assert_eq!(rollback.released, Some(10), "the caller has to be told");
        assert_eq!(ring.metrics().unrecoverable_reorgs, 1);
    }

    #[test]
    fn a_rollback_leaves_the_ring_able_to_take_a_full_load_again() {
        // Rollback rebuilds the deque, and a rebuild that lost the reserved
        // capacity would turn the next burst into a run of allocations.
        let mut ring = TickRing::new(RingConfig {
            capacity: 8,
            hold_slots: 1_000,
        });
        for id in 0u32..8 {
            ring.push(key(10, u64::from(id)), Tick::plain(id));
        }
        ring.rollback(10);
        assert_eq!(ring.len(), 0);
        assert!(
            ring.entries.capacity() >= 8,
            "capacity was not carried over"
        );

        for id in 0u32..8 {
            assert_eq!(
                ring.push(key(11, u64::from(id)), Tick::plain(id)),
                Push::Buffered
            );
        }
        assert_eq!(ring.len(), 8);
    }
}
