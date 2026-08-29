//! # Flagged dead by the salvage audit — 2026-08-27
//!
//! **Nothing in the shipped application references this module.** It is
//! declared `pub mod` in `lib.rs` and reached by no file in `src/` at all;
//! only `tests/geyser_tests.rs` touches it.
//!
//! It is left here, compiling and tested, on purpose. Removing it is a
//! decision for a human to make in one reviewed commit, not a sweep. See
//! `docs/SALVAGE.md` for what that decision involves. The whole tree as it
//! stood before any salvage action is recoverable with
//! `git checkout pre-salvage-2026-08-27`.
//!
//! ---
//!
//! A mock Geyser, run hard enough to be evidence.
//!
//! [`crate::subslot`] claims three things that are cheap to write and expensive
//! to be wrong about: that a shuffled feed comes out strictly ordered, that a
//! fork switch is undone before anything downstream sees it, and that the cost
//! of both is bounded. [`crate::geyser::MockStream`] is enough to show each of
//! them on a script of a dozen updates. It is not enough to show any of them at
//! the rate a launch burst actually arrives, and a hand-written script is
//! exactly the wrong instrument for that: it contains the cases its author
//! thought of.
//!
//! So this module generates the load instead of scripting it. It models a
//! chain — curves that trade, slots that progress through their statuses, forks
//! that lose — emits the updates that chain would produce in the order the
//! chain produced them, and then *breaks that order on purpose* through a delay
//! wheel that displaces updates by a configurable distance. What comes out is
//! the same shape a real Yellowstone stream has on a bad network, at a rate a
//! real one never reaches, and it is reproducible from a `u64` seed.
//!
//! # What it is for
//!
//! **Throughput.** [`run_load`] measures generation and ingestion separately,
//! in integers, and reports both. The generator's own rate is the interesting
//! one for a load tool — a harness that cannot outrun the thing it is testing
//! measures itself — and [`LoadConfig::EXTREME`] is sized so that a debug build
//! clears fifty thousand updates a second on the generation pass.
//!
//! **Reordering.** [`GeneratorStats::descents`] counts how many times the
//! emitted stream stepped backwards, and [`GeneratorStats::max_displacement`]
//! how far the worst-displaced update travelled. Both are the generator's own account of
//! the damage it did, and they are what
//! [`crate::subslot::RingMetrics::out_of_order_arrivals`] should be seen
//! against.
//!
//! **Parent validation and rollback.** The generator injects dead slots and
//! parent changes at a known cadence and counts them. The ledger has to catch
//! every one: [`LoadReport::injected_reorgs`] against
//! [`LoadReport::ledger_reorgs`] is that check, and it is an inequality rather
//! than an equality for a reason the field's own documentation gives.
//!
//! **Order, checked rather than assumed.** Every released event's key is
//! compared against the one before it, and [`LoadReport::order_violations`] is
//! the count that must be zero. That single number is the whole claim of
//! [`crate::subslot`], measured over a few hundred thousand events instead of
//! asserted over twelve.
//!
//! # Zero float
//!
//! No `f64` reaches this file, including in the rates it reports: a rate is
//! `count * 1_000_000 / micros`, worked out in `u128` and reported as a `u64`.
//! A benchmark that reported `50000.0000001` would be a benchmark whose output
//! two machines could disagree about, and the same source-level check that
//! guards [`crate::geyser`] and [`crate::subslot`] guards this module.

use std::collections::VecDeque;
use std::time::Duration;

use bytes::Bytes;
use serde::Serialize;

use crate::geyser::{
    AccountUpdate, BoxFuture, GeyserConfig, GeyserError, GeyserStream, GeyserTransport,
    GeyserUpdate, SlotUpdate, SubscribeFilters, TickEvent, TickPayload, TickPipeline, TokenBalance,
    TransactionUpdate, UpdatePayload,
};
use crate::ingestion::{PUMP_FUN_PROGRAM, PUMP_GRADUATION_LAMPORTS};
use crate::subslot::{RingMetrics, SlotPhase, TickKey};
use crate::types::{Pubkey, Signature};

// ---------------------------------------------------------------------------
// the chain's own constants
// ---------------------------------------------------------------------------

/// How long a Solana slot is, in microseconds.
const SLOT_MICROS: u64 = 400_000;

/// The pump.fun launch reserves, in raw units.
///
/// These are the real numbers a curve starts at, and they are here rather than
/// invented because the whole point of the price assertions downstream is that
/// they hold at the magnitudes the chain actually produces. Thirty SOL against
/// 1.073e15 raw tokens is a price of about `2.8e-5` lamports per raw unit —
/// the figure [`crate::geyser`]'s own documentation quotes when it explains why
/// millionths are not enough resolution.
const LAUNCH_VIRTUAL_SOL: u64 = 30_000_000_000;
const LAUNCH_VIRTUAL_TOKENS: u64 = 1_073_000_000_000_000;
const LAUNCH_REAL_TOKENS: u64 = 793_100_000_000_000;
const TOTAL_SUPPLY: u64 = 1_000_000_000_000_000;

/// The SPL decimals a pump.fun mint has.
const TOKEN_DECIMALS: u8 = 6;

/// The bonding curve account's length, and the offsets inside it.
///
/// The layout is [`crate::ingestion::BondingCurve`]'s, written from the other
/// side. Encoding it here rather than reaching for a helper is deliberate: if
/// the decoder's offsets ever drift from the encoder's, the generated stream
/// stops decoding and the tests say so, which is exactly the alarm that a
/// shared helper would silence.
const CURVE_LEN: usize = 81;

// ---------------------------------------------------------------------------
// the generator's randomness
// ---------------------------------------------------------------------------

/// SplitMix64: sixty-four bits of state, integer arithmetic, no dependency.
///
/// A load generator whose stream cannot be reproduced is a load generator that
/// can only ever find a bug once. This is seeded from a `u64` and produces the
/// same sequence on every machine and every run, so a failure at update
/// 412,904 of seed 7 is a failure somebody else can look at.
///
/// SplitMix64 rather than something stronger because the requirement here is
/// *unpredictability of shape*, not cryptographic strength: it has to scatter
/// delays and trade sizes convincingly, and it has to be three instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub const fn new(seed: u64) -> Self {
        Rng { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound`. Zero for a zero bound rather than a panic.
    ///
    /// The modulo is very slightly biased towards the low end for bounds that
    /// do not divide `2^64`. That bias is irrelevant to every use here — trade
    /// sizes and delay distances — and rejection sampling would make the
    /// generator's own cost data-dependent, which a benchmark can do without.
    pub fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        self.next_u64() % bound
    }

    /// True with probability `bps / 10_000`.
    pub fn chance_bps(&mut self, bps: u16) -> bool {
        self.below(10_000) < u64::from(bps)
    }
}

// ---------------------------------------------------------------------------
// configuration
// ---------------------------------------------------------------------------

/// What load to generate.
///
/// Every field is an integer and the struct is [`Eq`], for the same reason
/// every event type in [`crate::geyser`] is: a float in a benchmark
/// configuration is a benchmark two machines can run differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadConfig {
    /// Reproducibility. The same seed is the same stream, byte for byte.
    pub seed: u64,
    /// How many slots to simulate.
    pub slots: u64,
    /// How many distinct bonding curves trade across the run.
    pub curves: u32,
    /// Account writes per slot. The dominant term in the update count, as it is
    /// on a real feed during a launch burst.
    pub writes_per_slot: u32,
    /// Transactions per slot.
    pub transactions_per_slot: u32,
    /// How far, in updates, the delay wheel may displace an arrival.
    ///
    /// This is the jitter knob. A span of one is a stream in perfect order; a
    /// span comfortably larger than one slot's worth of updates is a stream
    /// where a slot's events can arrive interleaved with two other slots',
    /// which is the case the ring exists for.
    pub jitter_span: u32,
    /// What share of updates are displaced at all, in basis points. The rest
    /// arrive in chain order, which is what a real feed mostly does.
    pub reorder_bps: u16,
    /// Every Nth slot is declared dead by the validator. Zero disables it.
    pub dead_slot_every: u64,
    /// Every Nth slot is re-reported with a different parent — a fork switch.
    /// Zero disables it.
    pub fork_every: u64,
    /// How many slots behind the head the `Confirmed` status runs.
    pub confirm_lag_slots: u64,
    /// How many slots behind the head the `Finalized` status runs.
    pub finalize_lag_slots: u64,
    /// The wall-clock microsecond the first slot is stamped with.
    pub base_micros: u64,
    /// The slot number the run starts from.
    ///
    /// Nine digits, because a real one is. Starting at zero looks harmless and
    /// is not: `TickRing`'s hold window is `head - hold_slots` and that
    /// saturates, so at slot 0 there is no window at all and the first slot's
    /// writes are released the instant they arrive. On a chain three hundred
    /// million slots old that case cannot occur, and a generator that produced
    /// it would be reporting a loss the engine will never see.
    pub base_slot: u64,
}

impl Default for LoadConfig {
    fn default() -> Self {
        LoadConfig {
            seed: 0x5757_0000_0000_0001,
            slots: 256,
            curves: 64,
            writes_per_slot: 16,
            transactions_per_slot: 8,
            jitter_span: 64,
            reorder_bps: 2_000,
            dead_slot_every: 97,
            fork_every: 61,
            confirm_lag_slots: 2,
            finalize_lag_slots: 8,
            base_micros: 1_700_000_000_000_000,
            base_slot: 300_000_000,
        }
    }
}

impl LoadConfig {
    /// The settings the throughput claim is made at.
    ///
    /// Sized so the run is large enough for a rate to mean something — a shade
    /// over fifty-five thousand updates — and jittered hard enough that the
    /// ring is doing real work on most of them. `jitter_span` is deliberately
    /// wider than a slot's worth of updates: at 32 writes and 16 transactions a
    /// slot, a span of 512 means an update can arrive nearly ten slots out of
    /// place, which is far past anything a real network does and exactly the
    /// point of a stress figure.
    pub const EXTREME: LoadConfig = LoadConfig {
        seed: 0xA53F_1D2B_9C44_0E17,
        slots: 1_000,
        curves: 512,
        writes_per_slot: 32,
        transactions_per_slot: 16,
        jitter_span: 512,
        reorder_bps: 6_000,
        dead_slot_every: 89,
        fork_every: 53,
        confirm_lag_slots: 2,
        finalize_lag_slots: 12,
        base_micros: 1_700_000_000_000_000,
        base_slot: 300_000_000,
    };

    /// The same load with jitter the hold window can absorb entirely.
    ///
    /// The two constants are a pair, and the pair is the point. At 48 updates a
    /// slot, `RingConfig::hold_slots` of 4 is about 190 positions of slack, so a
    /// span of 64 keeps every displaced update inside the window and the ring
    /// should lose *nothing*: no late arrivals, no forced releases, no shedding.
    /// [`EXTREME`](Self::EXTREME) then pushes the span to eight times that, past
    /// anything the window can cover, and the loss stops being zero and starts
    /// being *counted* — which is the other half of the claim.
    pub const ABSORBED: LoadConfig = LoadConfig {
        seed: 0xA53F_1D2B_9C44_0E17,
        slots: 1_000,
        curves: 512,
        writes_per_slot: 32,
        transactions_per_slot: 16,
        jitter_span: 64,
        reorder_bps: 6_000,
        dead_slot_every: 89,
        fork_every: 53,
        confirm_lag_slots: 2,
        finalize_lag_slots: 12,
        base_micros: 1_700_000_000_000_000,
        base_slot: 300_000_000,
    };

    /// How many updates a run of this configuration emits.
    ///
    /// Exact rather than approximate, so a caller can size a buffer once and a
    /// test can assert that the wheel dropped nothing. The per-slot term is the
    /// two progress notifications, the writes, the transactions, `Completed`,
    /// and the `Processed` or `Dead` that closes the slot; the lagged
    /// `Confirmed` and `Finalized` are counted for the slots that have one, and
    /// a slot that died has neither.
    pub const fn updates(&self) -> u64 {
        let per_slot = 4 + self.writes_per_slot as u64 + self.transactions_per_slot as u64;
        let mut total = self.slots * per_slot + self.forks();
        let mut slot = 0;
        while slot < self.slots {
            if slot >= self.confirm_lag_slots && !self.is_dead_slot(slot - self.confirm_lag_slots) {
                total += 1;
            }
            if slot >= self.finalize_lag_slots && !self.is_dead_slot(slot - self.finalize_lag_slots)
            {
                total += 1;
            }
            slot += 1;
        }
        total
    }

    /// How many extra fork statuses the run injects.
    const fn forks(&self) -> u64 {
        if self.fork_every == 0 {
            return 0;
        }
        // Slot 0, 1 and 2 are skipped: a fork needs a parent that can differ
        // from the one already recorded, and the first slots have nowhere to
        // move theirs to.
        let mut count = 0;
        let mut slot = 3;
        while slot < self.slots {
            if slot % self.fork_every == 0 && !self.is_dead_slot(slot) {
                count += 1;
            }
            slot += 1;
        }
        count
    }

    const fn is_dead_slot(&self, slot: u64) -> bool {
        self.dead_slot_every != 0 && slot > 0 && slot % self.dead_slot_every == 0
    }

    /// How many re-orgs the run deliberately causes.
    pub const fn injected_reorgs(&self) -> u64 {
        let mut deaths = 0;
        let mut slot = 1;
        while slot < self.slots {
            if self.is_dead_slot(slot) {
                deaths += 1;
            }
            slot += 1;
        }
        deaths + self.forks()
    }
}

// ---------------------------------------------------------------------------
// the modelled chain
// ---------------------------------------------------------------------------

/// One bonding curve, as it trades.
///
/// The reserves move along the program's own constant product, in `u128`, so
/// the prices the generator produces are prices the chain could have produced.
/// A generator that walked the reserves by a fixed step would be testing the
/// ring against a stream whose price series no real curve has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Curve {
    account: Pubkey,
    creator: Pubkey,
    virtual_sol_reserves: u64,
    virtual_token_reserves: u64,
    real_sol_reserves: u64,
    real_token_reserves: u64,
    complete: bool,
    write_version: u64,
}

impl Curve {
    fn launch(index: u32) -> Self {
        Curve {
            account: seeded_pubkey(0xC0, index),
            creator: seeded_pubkey(0xCE, index),
            virtual_sol_reserves: LAUNCH_VIRTUAL_SOL,
            virtual_token_reserves: LAUNCH_VIRTUAL_TOKENS,
            real_sol_reserves: 0,
            real_token_reserves: LAUNCH_REAL_TOKENS,
            complete: false,
            write_version: 0,
        }
    }

    /// Buys `lamports` of the curve, returning the raw tokens that came out.
    ///
    /// `k = virtual_sol * virtual_token` is held constant, which is what the
    /// program does. All of it in `u128` because the product is about `3.2e25`
    /// and a `u64` would have overflowed before the first trade.
    fn buy(&mut self, lamports: u64) -> u128 {
        let product =
            u128::from(self.virtual_sol_reserves) * u128::from(self.virtual_token_reserves);
        let sol_after = u128::from(self.virtual_sol_reserves) + u128::from(lamports);
        // The quotient is floored, which rounds the token side down and so
        // rounds the price up. That is the direction a constant-product AMM
        // rounds: never in the trader's favour.
        let tokens_after = (product / sol_after).max(1);
        let tokens_out = u128::from(self.virtual_token_reserves).saturating_sub(tokens_after);

        self.virtual_sol_reserves = sol_after.min(u128::from(u64::MAX)) as u64;
        self.virtual_token_reserves = tokens_after.min(u128::from(u64::MAX)) as u64;
        self.real_sol_reserves = self.real_sol_reserves.saturating_add(lamports);
        self.real_token_reserves = self
            .real_token_reserves
            .saturating_sub(tokens_out.min(u128::from(u64::MAX)) as u64);
        self.complete = self.real_sol_reserves >= PUMP_GRADUATION_LAMPORTS;
        tokens_out
    }

    /// Sells `tokens` back into the curve, returning the lamports that came out.
    fn sell(&mut self, tokens: u64) -> u128 {
        let product =
            u128::from(self.virtual_sol_reserves) * u128::from(self.virtual_token_reserves);
        let tokens_after = u128::from(self.virtual_token_reserves) + u128::from(tokens);
        let sol_after = (product / tokens_after).max(1);
        let sol_out = u128::from(self.virtual_sol_reserves).saturating_sub(sol_after);

        self.virtual_sol_reserves = sol_after.min(u128::from(u64::MAX)) as u64;
        self.virtual_token_reserves = tokens_after.min(u128::from(u64::MAX)) as u64;
        self.real_sol_reserves = self
            .real_sol_reserves
            .saturating_sub(sol_out.min(u128::from(u64::MAX)) as u64);
        self.real_token_reserves = self.real_token_reserves.saturating_add(tokens);
        sol_out
    }

    /// The account bytes, in the layout the decoder reads.
    ///
    /// [`Bytes`] because that is what a real account write carries — the wire
    /// decode hands the pipeline a view into the read buffer, and a generator
    /// that handed it a `Vec` instead would be loading a slightly different
    /// pipeline than the one that ships.
    fn encode(&self) -> Bytes {
        let mut bytes = vec![0u8; CURVE_LEN];
        // The Anchor discriminator. Never read by the decoder — see its own
        // note on why — but present because an account without one is not the
        // account this is pretending to be.
        bytes[0..8].copy_from_slice(&[0x17, 0xb7, 0xf8, 0x37, 0x60, 0xd8, 0xac, 0x60]);
        bytes[8..16].copy_from_slice(&self.virtual_token_reserves.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.virtual_sol_reserves.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.real_token_reserves.to_le_bytes());
        bytes[32..40].copy_from_slice(&self.real_sol_reserves.to_le_bytes());
        bytes[40..48].copy_from_slice(&TOTAL_SUPPLY.to_le_bytes());
        bytes[48] = u8::from(self.complete);
        bytes[49..CURVE_LEN].copy_from_slice(self.creator.as_bytes());
        // Free: `Bytes::from` takes the vector's allocation rather than
        // copying it.
        Bytes::from(bytes)
    }
}

/// A deterministic 32-byte key from a tag and an index.
///
/// Not a real ed25519 point and it does not need to be: nothing in the pipeline
/// verifies a curve address, and every one of these has to be distinct, stable
/// across runs, and free to make.
fn seeded_pubkey(tag: u8, index: u32) -> Pubkey {
    let mut bytes = [tag; 32];
    bytes[0..4].copy_from_slice(&index.to_le_bytes());
    bytes[4] = tag;
    Pubkey::new(bytes)
}

fn seeded_signature(index: u64) -> Signature {
    let mut bytes = [0x51u8; 64];
    bytes[0..8].copy_from_slice(&index.to_le_bytes());
    Signature::new(bytes)
}

/// The pump.fun program id, parsed once.
fn pump_fun() -> Pubkey {
    static PROGRAM: std::sync::OnceLock<Pubkey> = std::sync::OnceLock::new();
    *PROGRAM.get_or_init(|| {
        Pubkey::parse(PUMP_FUN_PROGRAM).expect("PUMP_FUN_PROGRAM is a valid address")
    })
}

// ---------------------------------------------------------------------------
// the delay wheel
// ---------------------------------------------------------------------------

/// What displaces the stream, and the reason it is a wheel.
///
/// The obvious way to reorder a stream is to shuffle a buffer. The obvious way
/// is wrong for this: a shuffle moves updates *forwards* as well as backwards,
/// so it produces arrivals that precede their own causes, and no network does
/// that. What a network does is delay — every update arrives at or after the
/// moment it was sent, and the disorder is entirely in how much later.
///
/// So each update is given a delay in positions and dropped into the bucket
/// that many steps ahead. Draining advances one bucket per update produced,
/// which keeps the buffer's occupancy flat however long the run is, and makes
/// the worst-case displacement exactly `span - 1` rather than a tail nobody
/// bounded. Constant time, one allocation per bucket amortised, and no
/// comparisons — a priority queue would have put its own `log n` into the
/// number this exists to measure.
struct DelayWheel {
    buckets: Vec<Vec<(u64, GeyserUpdate)>>,
    cursor: u64,
}

impl DelayWheel {
    fn new(span: u32) -> Self {
        let span = (span as usize).max(1);
        DelayWheel {
            buckets: (0..span).map(|_| Vec::new()).collect(),
            cursor: 0,
        }
    }

    fn span(&self) -> usize {
        self.buckets.len()
    }

    /// Places an update `delay` positions from now. A delay past the span is
    /// clamped rather than wrapped: a wrap would deliver it *early*, which is
    /// the one thing a delay must never do.
    fn push(&mut self, delay: u64, produced: u64, update: GeyserUpdate) {
        let span = self.span() as u64;
        let index = ((self.cursor + delay.min(span - 1)) % span) as usize;
        self.buckets[index].push((produced, update));
    }

    /// Advances one position, moving whatever is due into `out`.
    fn advance(&mut self, out: &mut VecDeque<(u64, GeyserUpdate)>) {
        let span = self.span() as u64;
        let index = (self.cursor % span) as usize;
        out.extend(self.buckets[index].drain(..));
        self.cursor += 1;
    }

    fn is_empty(&self) -> bool {
        self.buckets.iter().all(|bucket| bucket.is_empty())
    }
}

// ---------------------------------------------------------------------------
// the generator
// ---------------------------------------------------------------------------

/// What the generator did to the stream on its way out.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneratorStats {
    pub updates: u64,
    pub accounts: u64,
    pub transactions: u64,
    pub slot_statuses: u64,
    /// Emitted arrivals whose `(slot, micros)` was below the arrival before
    /// them. The generator's own count of the disorder it created, and the
    /// thing [`crate::subslot::RingMetrics::out_of_order_arrivals`] is measured
    /// against.
    pub descents: u64,
    /// The furthest any update was moved from its place in chain order, in
    /// positions.
    pub max_displacement: u64,
    /// Slots the validator was made to declare dead.
    pub dead_slots: u64,
    /// Slots re-reported with a different parent.
    pub forks: u64,
}

/// A mock Yellowstone stream with a chain behind it.
///
/// Pulls like an iterator and is one; the chain is advanced lazily, a slot at a
/// time, so a run of a million updates costs one slot's worth of memory plus
/// the delay wheel rather than a million updates' worth.
pub struct MockGeyser {
    config: LoadConfig,
    rng: Rng,
    curves: Vec<Curve>,
    /// How many slots have been produced. An index into the run rather than a
    /// slot number: the cadences — dead slots, forks — are counted from the
    /// start of the run, and the chain's own numbering is `base_slot` above it.
    /// Past `config.slots` means the chain is done and only the wheel is still
    /// draining.
    slot: u64,
    /// The index of the parent the next slot will name. Not simply `slot - 1`:
    /// a dead slot is skipped over, so its successor names the slot before it.
    parent: u64,
    wheel: DelayWheel,
    /// What has come due and not yet been handed out. A deque because it is
    /// filled at the back and emptied from the front, and doing that to a `Vec`
    /// is a memmove per update.
    ready: VecDeque<(u64, GeyserUpdate)>,
    produced: u64,
    emitted: u64,
    last_emitted: Option<(u64, u64)>,
    stats: GeneratorStats,
}

impl MockGeyser {
    pub fn new(config: LoadConfig) -> Self {
        let curve_count = config.curves.max(1);
        MockGeyser {
            config,
            rng: Rng::new(config.seed),
            curves: (0..curve_count).map(Curve::launch).collect(),
            slot: 0,
            parent: 0,
            wheel: DelayWheel::new(config.jitter_span),
            ready: VecDeque::new(),
            produced: 0,
            emitted: 0,
            last_emitted: None,
            stats: GeneratorStats::default(),
        }
    }

    pub const fn config(&self) -> LoadConfig {
        self.config
    }

    pub const fn stats(&self) -> GeneratorStats {
        self.stats
    }

    /// The next update off the mock wire, or `None` when the run is over.
    pub fn next_update(&mut self) -> Option<GeyserUpdate> {
        loop {
            if let Some((produced, update)) = self.ready.pop_front() {
                return Some(self.account_for(produced, update));
            }

            if self.slot >= self.config.slots {
                if self.wheel.is_empty() {
                    return None;
                }
                // The chain has stopped but the wheel has not. Kept turning
                // until it is empty, which is what makes the emitted count
                // equal the produced count rather than merely close to it.
                self.wheel.advance(&mut self.ready);
                continue;
            }

            self.produce_slot();
        }
    }

    /// Books one emitted update into the statistics.
    fn account_for(&mut self, produced: u64, update: GeyserUpdate) -> GeyserUpdate {
        self.emitted += 1;
        let displacement = self.emitted.saturating_sub(1).saturating_sub(produced);
        self.stats.max_displacement = self.stats.max_displacement.max(displacement);

        let position = (update.payload.slot().unwrap_or(0), update.created_at_micros);
        if self
            .last_emitted
            .is_some_and(|previous| position < previous)
        {
            self.stats.descents += 1;
        }
        self.last_emitted = Some(position);
        update
    }

    /// Produces one slot's worth of updates and turns the wheel that far.
    fn produce_slot(&mut self) {
        let index = self.slot;
        let slot = self.config.base_slot + index;
        let parent = self.config.base_slot + self.parent;
        let start = self.config.base_micros + index * SLOT_MICROS;
        let dead = self.config.is_dead_slot(index);

        let mut script: Vec<GeyserUpdate> = Vec::with_capacity(
            8 + self.config.writes_per_slot as usize + self.config.transactions_per_slot as usize,
        );

        script.push(status(
            start,
            slot,
            Some(parent),
            SlotPhase::FirstShredReceived,
        ));
        script.push(status(
            start + 20_000,
            slot,
            Some(parent),
            SlotPhase::CreatedBank,
        ));

        // The body of the slot: writes and transactions interleaved the way
        // they are on the wire, because an account write and the transaction
        // that caused it leave the validator on different paths and the ring's
        // job is to put those two paths back together.
        let writes = self.config.writes_per_slot as u64;
        let transactions = self.config.transactions_per_slot as u64;
        let body = writes + transactions;
        let step = if body == 0 { 0 } else { 300_000 / body.max(1) };
        let mut written = 0u64;
        let mut sent = 0u64;
        for index in 0..body {
            let micros = start + 40_000 + index * step;
            // Two writes for every transaction where the ratio allows it, which
            // is roughly what a curve account does: the account is rewritten by
            // every trade and by the fee accounts beside it.
            let want_write = written < writes && (sent >= transactions || index % 3 != 2);
            if want_write {
                script.push(self.write_update(slot, micros, written));
                written += 1;
            } else {
                script.push(self.transaction_update(slot, micros, sent));
                sent += 1;
            }
        }

        script.push(status(
            start + 350_000,
            slot,
            Some(parent),
            SlotPhase::Completed,
        ));

        if dead {
            // A dead slot never reaches a commitment. The validator says so
            // outright and everything buffered for it is void — which is the
            // rollback path, reached here as often as `dead_slot_every` says.
            script.push(status(start + 360_000, slot, Some(parent), SlotPhase::Dead));
            self.stats.dead_slots += 1;
        } else {
            script.push(status(
                start + 360_000,
                slot,
                Some(parent),
                SlotPhase::Processed,
            ));
        }

        // The lagged statuses. Sent now, about a slot that happened earlier,
        // which is what makes the commitment stream a second timeline rather
        // than an annotation on the first.
        if index >= self.config.confirm_lag_slots {
            let confirmed = index - self.config.confirm_lag_slots;
            if !self.config.is_dead_slot(confirmed) {
                let confirmed = self.config.base_slot + confirmed;
                script.push(status(
                    start + 370_000,
                    confirmed,
                    Some(confirmed.saturating_sub(1)),
                    SlotPhase::Confirmed,
                ));
            }
        }
        if index >= self.config.finalize_lag_slots {
            let finalized = index - self.config.finalize_lag_slots;
            if !self.config.is_dead_slot(finalized) {
                let finalized = self.config.base_slot + finalized;
                script.push(status(
                    start + 380_000,
                    finalized,
                    Some(finalized.saturating_sub(1)),
                    SlotPhase::Finalized,
                ));
            }
        }

        // The fork. The same slot number, built on a different block: the one
        // thing besides a dead slot that is evidence of a switch rather than
        // merely consistent with one.
        if !dead && index >= 3 && self.config.fork_every != 0 && index % self.config.fork_every == 0
        {
            script.push(status(
                start + 390_000,
                slot,
                Some(parent.saturating_sub(1)),
                SlotPhase::Processed,
            ));
            self.stats.forks += 1;
        }

        for update in script {
            self.stats.updates += 1;
            match &update.payload {
                UpdatePayload::Account(_) => self.stats.accounts += 1,
                UpdatePayload::Transaction(_) => self.stats.transactions += 1,
                _ => self.stats.slot_statuses += 1,
            }
            let delay = if self.rng.chance_bps(self.config.reorder_bps) {
                1 + self
                    .rng
                    .below(u64::from(self.config.jitter_span).max(2) - 1)
            } else {
                0
            };
            self.wheel.push(delay, self.produced, update);
            self.produced += 1;
            self.wheel.advance(&mut self.ready);
        }

        self.slot += 1;
        // The next slot builds on this one unless this one died, in which case
        // the chain carries on from the block before it.
        if !dead {
            self.parent = index;
        }
    }

    /// One curve account write, with the curve traded first so the bytes carry
    /// a price that moved.
    fn write_update(&mut self, slot: u64, micros: u64, index: u64) -> GeyserUpdate {
        let which = (self.rng.next_u64() % self.curves.len() as u64) as usize;
        let curve = &mut self.curves[which];

        // A buy four times out of five. Curves that have graduated stop
        // trading, which is what `complete` means, and are re-launched so the
        // population stays the size the configuration asked for.
        if curve.complete {
            *curve = Curve::launch(which as u32);
        } else if self.rng.chance_bps(8_000) {
            let lamports = 10_000_000 + self.rng.below(2_000_000_000);
            curve.buy(lamports);
        } else {
            let tokens = 1_000_000_000 + self.rng.below(20_000_000_000_000);
            curve.sell(tokens.min(curve.virtual_token_reserves / 4));
        }
        curve.write_version += 1;

        let curve = *curve;
        GeyserUpdate::new(
            micros,
            UpdatePayload::Account(AccountUpdate {
                slot,
                pubkey: curve.account,
                owner: pump_fun(),
                // The rent-exempt minimum for an 81-byte account, which is what
                // a real curve holds.
                lamports: 1_600_000 + index,
                write_version: curve.write_version,
                data: curve.encode(),
                is_startup: false,
            }),
        )
    }

    /// One transaction, with the metadata the pipeline actually reads.
    fn transaction_update(&mut self, slot: u64, micros: u64, index: u64) -> GeyserUpdate {
        let which = (self.rng.next_u64() % self.curves.len() as u64) as usize;
        let curve = self.curves[which];
        let traded = 1_000_000_000 + self.rng.below(50_000_000_000_000);
        let mint = seeded_pubkey(0x11, which as u32);
        let owner = seeded_pubkey(0x22, (index % 64) as u32);

        // One failure in fifty, which is about the rate a launch burst runs at
        // when the slippage guards start biting.
        let failed = self.rng.chance_bps(200);

        GeyserUpdate::new(
            micros,
            UpdatePayload::Transaction(TransactionUpdate {
                slot,
                signature: seeded_signature(slot * 1_000 + index),
                index,
                is_vote: false,
                failed,
                logs: vec![
                    format!("Program {PUMP_FUN_PROGRAM} invoke [1]"),
                    "Program log: Instruction: Buy".to_string(),
                    format!("Program {PUMP_FUN_PROGRAM} consumed 34160 of 200000 compute units"),
                    format!(
                        "Program {PUMP_FUN_PROGRAM} {}",
                        if failed { "failed" } else { "success" }
                    ),
                ],
                pre_token_balances: vec![TokenBalance {
                    account_index: 3,
                    mint,
                    owner,
                    raw: u128::from(curve.real_token_reserves),
                    decimals: TOKEN_DECIMALS,
                }],
                post_token_balances: vec![TokenBalance {
                    account_index: 3,
                    mint,
                    owner,
                    raw: u128::from(curve.real_token_reserves).saturating_sub(u128::from(traded)),
                    decimals: TOKEN_DECIMALS,
                }],
            }),
        )
    }
}

impl Iterator for MockGeyser {
    type Item = GeyserUpdate;

    fn next(&mut self) -> Option<GeyserUpdate> {
        self.next_update()
    }
}

fn status(micros: u64, slot: u64, parent: Option<u64>, phase: SlotPhase) -> GeyserUpdate {
    GeyserUpdate::new(
        micros,
        UpdatePayload::Slot(SlotUpdate {
            slot,
            parent,
            phase,
        }),
    )
}

// ---------------------------------------------------------------------------
// the generator as a transport
// ---------------------------------------------------------------------------

/// A [`GeyserStream`] over generated load.
///
/// What makes the whole subscriber loop testable at rate: `run_subscriber`
/// cannot tell this from a socket, so the reconnect handling, the startup-skip
/// and the sink wiring are all exercised by the same run that measures the
/// ring.
pub struct GeneratedStream {
    generator: MockGeyser,
    /// What to do once the load is exhausted.
    ///
    /// `None` ends the stream, which the subscriber reads as a disconnect and
    /// backs off from — the right shape for testing the reconnect. `Some` keeps
    /// the subscription open and sends a keepalive at that interval, which is
    /// what a real one does on a quiet chain, and is the only way a test can
    /// observe a feed that is *connected* rather than one that flickered.
    keepalive: Option<Duration>,
    last_micros: u64,
}

impl GeneratedStream {
    pub fn new(config: LoadConfig) -> Self {
        GeneratedStream {
            last_micros: config.base_micros,
            generator: MockGeyser::new(config),
            keepalive: None,
        }
    }

    /// The same stream, staying open after the load runs out.
    pub fn keeping_alive(config: LoadConfig, interval: Duration) -> Self {
        GeneratedStream {
            keepalive: Some(interval),
            ..GeneratedStream::new(config)
        }
    }
}

impl GeyserStream for GeneratedStream {
    fn recv(&mut self) -> BoxFuture<'_, Option<Result<GeyserUpdate, GeyserError>>> {
        Box::pin(async move {
            if let Some(update) = self.generator.next_update() {
                self.last_micros = update.created_at_micros;
                return Some(Ok(update));
            }
            // The server's keepalive: proof the socket is alive when nothing is
            // happening on chain, which is the only thing that distinguishes a
            // quiet feed from a dead one.
            let interval = self.keepalive?;
            tokio::time::sleep(interval).await;
            self.last_micros += interval.as_micros().min(u128::from(u64::MAX)) as u64;
            Some(Ok(GeyserUpdate::new(self.last_micros, UpdatePayload::Ping)))
        })
    }
}

/// A transport that hands out generated streams.
#[derive(Debug, Clone, Copy)]
pub struct LoadTransport {
    config: LoadConfig,
    keepalive: Option<Duration>,
}

impl LoadTransport {
    /// A transport whose streams end when the load does.
    pub const fn new(config: LoadConfig) -> Self {
        LoadTransport {
            config,
            keepalive: None,
        }
    }

    /// A transport whose streams stay open, keepalive-ing at `interval`.
    pub const fn keeping_alive(config: LoadConfig, interval: Duration) -> Self {
        LoadTransport {
            config,
            keepalive: Some(interval),
        }
    }
}

impl GeyserTransport for LoadTransport {
    fn subscribe(
        &self,
        _config: GeyserConfig,
        _filters: SubscribeFilters,
    ) -> BoxFuture<'static, Result<Box<dyn GeyserStream>, GeyserError>> {
        let config = self.config;
        let keepalive = self.keepalive;
        Box::pin(async move {
            let stream = match keepalive {
                Some(interval) => GeneratedStream::keeping_alive(config, interval),
                None => GeneratedStream::new(config),
            };
            Ok(Box::new(stream) as Box<dyn GeyserStream>)
        })
    }
}

// ---------------------------------------------------------------------------
// the benchmark
// ---------------------------------------------------------------------------

/// What one load run measured.
///
/// Integers all the way down, including the rates. See the module note on why a
/// benchmark that reports a float is a benchmark whose result cannot be
/// compared between two machines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadReport {
    /// What the generator produced and how badly it shuffled it.
    pub generator: GeneratorStats,

    /// Domain events released in order.
    pub released: u64,
    pub curve_events: u64,
    pub price_events: u64,
    pub slot_events: u64,
    pub log_events: u64,
    /// Events refused: too late to place in order, or shed for room.
    pub dropped: u64,
    /// How many of those were protected payloads — a curve, a price or a pool.
    ///
    /// The one that matters. A dropped slot status costs a heartbeat and the
    /// next one repairs it; a dropped curve write leaves the engine's idea of a
    /// price wrong until that curve trades again, because nothing re-sends it.
    /// Inside the hold window this must be zero.
    pub dropped_protected: u64,
    /// Curve writes that reached the front and turned out to be older than what
    /// had already been applied. The write-version guard doing its job.
    pub stale: u64,
    /// Events a rollback discarded before anyone saw them.
    pub rolled_back: u64,
    /// How many of those were protected payloads.
    ///
    /// Not a loss. These are curve writes from blocks the cluster abandoned,
    /// caught by the hold window and thrown away before anything acted on them,
    /// which is the entire reason the window exists. They are counted here so
    /// that the account of where every generated write went adds up exactly —
    /// see the accounting assertion in `tests/geyser_tests.rs`.
    pub rolled_back_protected: u64,
    /// Rollbacks that arrived after their slots had been released.
    pub unrecoverable: u64,
    /// Updates the pipeline could not decode.
    pub decode_failures: u64,

    /// Released events whose key was not strictly above the one before it.
    /// **The number that must be zero**, and the whole claim of
    /// [`crate::subslot`] measured rather than asserted.
    pub order_violations: u64,
    /// Curve events whose price was zero — a price that cannot be true and that
    /// the fixed-point path must never produce.
    pub zero_prices: u64,

    /// Re-orgs the generator deliberately caused.
    pub injected_reorgs: u64,
    /// Re-orgs the ledger detected.
    ///
    /// At least `injected_reorgs`, and possibly more. The excess is not a false
    /// positive: a fork status displaced into the middle of its own slot's
    /// statuses is seen once when it changes the parent and once more when a
    /// displaced original status changes it back, and both of those genuinely
    /// are the parent moving. Under-counting would be the bug; this cannot
    /// under-count.
    pub ledger_reorgs: u64,
    pub head_slot: u64,
    pub confirmed_head: u64,
    pub finalized_head: u64,

    pub ring: RingMetrics,

    /// How long the generation pass took, and what rate that is.
    pub generate_micros: u64,
    pub generated_per_second: u64,
    /// How long the ingestion pass took, and what rate that is.
    pub ingest_micros: u64,
    pub ingested_per_second: u64,
    /// Ordered domain events released per second of ingestion.
    pub events_per_second: u64,
}

/// A rate in whole units per second, from a count and a duration.
///
/// `u128` in the middle because `count * 1_000_000` overflows a `u64` at about
/// eighteen trillion, and a long run at a high rate should not be the thing
/// that discovers that. Zero micros reports zero rather than dividing: a run too
/// fast to time is a run whose rate nobody should quote.
const fn per_second(count: u64, micros: u64) -> u64 {
    if micros == 0 {
        return 0;
    }
    let rate = (count as u128 * 1_000_000) / micros as u128;
    if rate > u64::MAX as u128 {
        u64::MAX
    } else {
        rate as u64
    }
}

/// Generates the load, then feeds it through a real [`TickPipeline`].
///
/// Two passes rather than one, and the split is the measurement: the first pass
/// is what the *generator* can do, which is the number a load tool has to state
/// about itself, and the second is what the *pipeline* can do with it. Timing
/// them together would report their sum and let either one hide behind the
/// other.
///
/// The cost of the split is holding the whole run in memory —
/// [`LoadConfig::updates`] says exactly how many that is, so the caller sizes it
/// deliberately rather than discovering it.
pub fn run_load(load: LoadConfig, geyser: &GeyserConfig) -> LoadReport {
    let mut generator = MockGeyser::new(load);
    let mut updates: Vec<GeyserUpdate> = Vec::with_capacity(load.updates() as usize);

    let generating = std::time::Instant::now();
    while let Some(update) = generator.next_update() {
        updates.push(update);
    }
    let generate_micros = generating.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;

    let mut pipeline = TickPipeline::new(geyser);
    let mut report = LoadReport {
        generator: generator.stats(),
        injected_reorgs: load.injected_reorgs(),
        generate_micros,
        generated_per_second: per_second(generator.stats().updates, generate_micros),
        ..LoadReport::default()
    };
    let mut previous: Option<TickKey> = None;

    let ingesting = std::time::Instant::now();
    // `updates` is consumed rather than borrowed, because `ingest` takes its
    // update by value and the whole point of that signature is that nothing on
    // this path is copied to satisfy a borrow.
    for update in updates {
        let outcome = pipeline.ingest(update);
        if outcome.decode_error.is_some() {
            report.decode_failures += 1;
        }
        report.dropped += outcome.dropped.len() as u64;
        report.dropped_protected += protected(&outcome.dropped);
        report.stale += outcome.stale.len() as u64;
        report.rolled_back += outcome.rolled_back.len() as u64;
        report.rolled_back_protected += protected(&outcome.rolled_back);
        if outcome.unrecoverable_from_slot.is_some() {
            report.unrecoverable += 1;
        }
        record_released(&outcome.released, &mut previous, &mut report);
    }
    // Nothing later can arrive, so the hold window is pure loss and what is
    // still resident goes out in order.
    let tail = pipeline.flush();
    record_released(&tail, &mut previous, &mut report);
    let ingest_micros = ingesting.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;

    report.ingest_micros = ingest_micros;
    report.ingested_per_second = per_second(report.generator.updates, ingest_micros);
    report.events_per_second = per_second(report.released, ingest_micros);
    report.ring = pipeline.ring_metrics();
    report.ledger_reorgs = pipeline.ledger().reorgs();
    report.head_slot = pipeline.ledger().head();
    report.confirmed_head = pipeline.ledger().confirmed_head();
    report.finalized_head = pipeline.ledger().finalized_head();
    report
}

/// How many of these events are ones the ring may never shed.
///
/// The same question [`crate::subslot::TickClass::is_protected`] answers, asked
/// from outside the trait so that a report can be built without a payload in
/// hand.
fn protected(events: &[TickEvent]) -> u64 {
    events
        .iter()
        .filter(|event| {
            matches!(
                event.payload,
                TickPayload::Curve(_) | TickPayload::Price(_) | TickPayload::Pool(_)
            )
        })
        .count() as u64
}

/// Books a batch of released events, checking the order as it goes.
fn record_released(events: &[TickEvent], previous: &mut Option<TickKey>, report: &mut LoadReport) {
    for event in events {
        report.released += 1;
        if previous.is_some_and(|before| event.key <= before) {
            report.order_violations += 1;
        }
        *previous = Some(event.key);

        match &event.payload {
            TickPayload::Curve(curve) => {
                report.curve_events += 1;
                if curve.price_e18 == 0 {
                    report.zero_prices += 1;
                }
            }
            TickPayload::Price(price) => {
                report.price_events += 1;
                if price.current_e18 == 0 {
                    report.zero_prices += 1;
                }
            }
            TickPayload::Pool(_) => {}
            TickPayload::Slot(_) => report.slot_events += 1,
            TickPayload::Log(_) => report.log_events += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::subslot::{Commitment, RingConfig};

    fn geyser(capacity: usize, hold_slots: u64) -> GeyserConfig {
        GeyserConfig {
            ring: RingConfig {
                capacity,
                hold_slots,
            },
            commitment: Commitment::Confirmed,
            ..GeyserConfig::default()
        }
    }

    fn small() -> LoadConfig {
        LoadConfig {
            slots: 120,
            curves: 24,
            jitter_span: 48,
            ..LoadConfig::default()
        }
    }

    #[test]
    fn the_same_seed_is_the_same_stream() {
        let config = small();
        let first: Vec<GeyserUpdate> = MockGeyser::new(config).collect();
        let second: Vec<GeyserUpdate> = MockGeyser::new(config).collect();
        assert_eq!(
            first, second,
            "a load generator nobody can replay finds a bug once"
        );

        let other = MockGeyser::new(LoadConfig {
            seed: config.seed ^ 1,
            ..config
        })
        .collect::<Vec<_>>();
        assert_ne!(first, other, "two seeds should not produce one stream");
    }

    #[test]
    fn every_produced_update_is_emitted_exactly_once() {
        // The wheel delays; it must not drop and it must not duplicate. Counted
        // rather than trusted because a bucket left undrained at the end of a
        // run would silently shorten every measurement this module makes.
        let config = small();
        let mut generator = MockGeyser::new(config);
        let emitted = generator.by_ref().count() as u64;
        assert_eq!(emitted, generator.stats().updates);
        assert_eq!(
            emitted,
            config.updates(),
            "the update count is exact, not an estimate"
        );
    }

    /// Runs a configuration to exhaustion and hands back what it did.
    fn drain(config: LoadConfig) -> GeneratorStats {
        let mut generator = MockGeyser::new(config);
        while generator.next_update().is_some() {}
        generator.stats()
    }

    #[test]
    fn the_jitter_knob_is_what_reorders_the_stream() {
        // With the knob at zero nothing is displaced at all. The stream still
        // descends — the lagged `Confirmed` and `Finalized` statuses are about
        // older slots than the writes they travel beside, which is a real
        // property of a commitment stream and not disorder this module added —
        // so the honest zero to assert on is the displacement.
        let quiet = drain(LoadConfig {
            reorder_bps: 0,
            ..small()
        });
        assert_eq!(quiet.max_displacement, 0);

        let loud = drain(LoadConfig {
            reorder_bps: 9_000,
            ..small()
        });
        assert!(
            loud.descents > quiet.descents * 4,
            "extreme jitter should dwarf the baseline: {} descents against {}",
            loud.descents,
            quiet.descents
        );
        assert!(loud.max_displacement > 1);
    }

    #[test]
    fn a_displaced_update_never_arrives_before_it_was_sent() {
        // The delay wheel's one hard rule, and the reason it is a wheel rather
        // than a shuffle. An update may slip behind its place in chain order by
        // up to the span; it may never slip ahead of it, because no network
        // delivers a packet before it is sent.
        let config = LoadConfig {
            reorder_bps: 9_000,
            ..small()
        };
        let stats = drain(config);
        assert!(
            stats.max_displacement < u64::from(config.jitter_span),
            "displacement {} should stay inside the span {}",
            stats.max_displacement,
            config.jitter_span
        );
    }

    #[test]
    fn a_generated_run_comes_out_strictly_ordered() {
        let load = small();
        let report = run_load(load, &geyser(4_096, 4));
        assert_eq!(
            report.order_violations, 0,
            "the ring released events out of order"
        );
        assert_eq!(
            report.zero_prices, 0,
            "a curve priced at zero is not a price"
        );
        assert!(report.released > 0);
        assert!(report.curve_events > 0);
        assert!(
            report.generator.descents > 0,
            "the run has to be disordered for its ordering to mean anything"
        );
    }

    #[test]
    fn every_injected_fork_reaches_the_ledger() {
        let load = LoadConfig {
            dead_slot_every: 17,
            fork_every: 11,
            ..small()
        };
        let report = run_load(load, &geyser(4_096, 4));
        assert!(
            report.injected_reorgs > 0,
            "the run injected no re-orgs to catch"
        );
        assert!(
            report.ledger_reorgs >= report.injected_reorgs,
            "the ledger saw {} re-orgs but {} were injected",
            report.ledger_reorgs,
            report.injected_reorgs
        );
        assert!(
            report.rolled_back > 0,
            "a rollback that discards nothing undid nothing"
        );
    }

    #[test]
    fn the_reported_rates_are_integers_derived_from_the_counts() {
        assert_eq!(per_second(50_000, 1_000_000), 50_000);
        assert_eq!(per_second(1, 1), 1_000_000);
        // A run too fast to time reports nothing rather than dividing by zero.
        assert_eq!(per_second(10, 0), 0);
        // The multiply is done in `u128`, so a count that would overflow the
        // intermediate in `u64` still reports a true rate.
        assert_eq!(per_second(u64::MAX, 1_000_000), u64::MAX);
    }
}
