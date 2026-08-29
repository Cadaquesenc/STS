//! Numbers about the engine itself: how fast it ticks, what it is losing, and
//! what it is holding.
//!
//! `telemetry.rs` carries events — one line per thing that happened, fanned out
//! to whatever window is watching. This carries counts: the same run described
//! as totals and quantiles rather than as a story. The two answer different
//! questions. An event says a candidate was dropped; a counter says four
//! thousand were, which is the number somebody decides to fix the machine over.
//!
//! Four rules shape everything below.
//!
//! **Recording allocates nothing and locks nothing.** Every counter is an
//! atomic in a fixed-size array that exists from startup. Writing one is an
//! index and a `fetch_add`, on whatever thread was already there. There is no
//! queue behind it, no background thread draining it, and nothing a slow reader
//! can do to make a writer wait — which is the whole point, because the writers
//! here are the ingest path and the exit path, and neither can afford to block
//! on being measured. `STS_CORE_IDEOLOGY.md` §Annex V asks for observability;
//! §6 says the hot path does not wait on a mutex, and a metrics module that
//! took one would be the exception that ate its own budget.
//!
//! **Reading is a side-effect-free read of those same atomics.** `snapshot`
//! takes no lock, resets nothing, and never blocks a writer. Two snapshots in a
//! row report the same run over a slightly longer window. The UI may poll it as
//! fast as it repaints, and the HTTP exporter may be scraped as often as
//! somebody likes, without either of them touching the engine's timing.
//!
//! **Unavailable is not zero.** Every quantile is an `Option`. A histogram with
//! no samples reports `null`, not `0`, because a p99 of zero reads as "instant"
//! when it means "never measured" — and §Annex V is explicit that dashboards
//! have to be able to tell those apart.
//!
//! **Quantiles are bucketed, and the buckets are stated.** Exact percentiles
//! need every sample kept; these are counted into a fixed ladder of ranges
//! instead, so the memory is constant no matter how long the process runs. What
//! comes back is interpolated inside the bucket the rank landed in, so it is
//! accurate to the width of that bucket and no further. `count`, `sum`, `min`
//! and `max` are exact. The bucket counts are in the snapshot too, so anything
//! that wants to aggregate several runs properly can.
//!
//! The exporter at the bottom serves the same snapshot over HTTP on loopback,
//! for the monitoring that lives outside the window. It binds nothing unless it
//! is asked to, and it refuses to bind anywhere but the local machine.
//!
//! It answers in two shapes of the one snapshot: the JSON above, and the
//! Prometheus text format that `prometheus.rs` writes. Which one a client gets
//! is decided by what it asked for in `Accept`, and `/metrics.json` and
//! `/metrics.prom` name one outright for anybody who would rather not
//! negotiate. Both are rendered from a single `snapshot()` call, so the two can
//! never describe two different moments.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener as StdTcpListener};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::telemetry::now_ms;
use crate::types::{ExecutionState, ExitState};

// ---------------------------------------------------------------------------
// histogram
// ---------------------------------------------------------------------------

/// The inclusive upper bound of every bucket except the last, in microseconds.
///
/// A 1-2-5 ladder from one microsecond to five seconds. It is that shape
/// because the numbers it has to describe live at both ends: a slot tick that
/// is processed in forty microseconds and a socket that has been quiet for two
/// seconds are both ordinary readings, and a ladder fine enough for the first
/// would need thousands of buckets to reach the second.
///
/// Within a bucket the resolution is the width of that bucket, so a p99 of
/// "somewhere between 200µs and 500µs" is reported as a number in that range
/// rather than as a precise-looking figure that was never measured.
pub const BUCKET_BOUNDS_US: [u64; 21] = [
    1, 2, 5, 10, 20, 50, 100, 200, 500, 1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000,
    200_000, 500_000, 1_000_000, 2_000_000, 5_000_000,
];

/// Every bucket, including the overflow one that catches anything above the
/// last bound.
pub const BUCKETS: usize = BUCKET_BOUNDS_US.len() + 1;

/// Counts of how long things took, in fixed ranges.
///
/// Constant memory: `BUCKETS` counters plus four, whether it has seen ten
/// samples or ten billion. Recording is one binary search over a 21-element
/// constant array and five `Relaxed` atomic adds — no allocation, no lock, and
/// nothing that can fail.
#[derive(Debug)]
pub struct Histogram {
    buckets: [AtomicU64; BUCKETS],
    count: AtomicU64,
    sum_us: AtomicU64,
    /// `u64::MAX` while nothing has been recorded, so the first sample wins the
    /// `fetch_min` without needing a separate "is empty" flag.
    min_us: AtomicU64,
    max_us: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    pub fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            count: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
            min_us: AtomicU64::new(u64::MAX),
            max_us: AtomicU64::new(0),
        }
    }

    /// Which bucket a reading belongs in.
    ///
    /// Bucket `i` holds everything above `BUCKET_BOUNDS_US[i - 1]` and up to
    /// and including `BUCKET_BOUNDS_US[i]`. The last bucket has no upper bound.
    pub fn bucket_of(micros: u64) -> usize {
        BUCKET_BOUNDS_US.partition_point(|&bound| bound < micros)
    }

    /// Records one reading, in microseconds.
    pub fn record_us(&self, micros: u64) {
        self.buckets[Self::bucket_of(micros)].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(micros, Ordering::Relaxed);
        self.min_us.fetch_min(micros, Ordering::Relaxed);
        self.max_us.fetch_max(micros, Ordering::Relaxed);
    }

    /// Records one reading from a measured duration.
    ///
    /// Saturates rather than wrapping: a duration longer than half a million
    /// years is a broken clock, and it lands in the overflow bucket where a
    /// broken clock belongs.
    pub fn record(&self, elapsed: Duration) {
        self.record_us(elapsed.as_micros().min(u64::MAX as u128) as u64)
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Everything the histogram knows, with the quantiles worked out.
    ///
    /// Reads the counters one at a time without stopping the writers, so a
    /// sample recorded half way through this call may be counted in the buckets
    /// and not in `count`, or the other way round. The quantiles are therefore
    /// computed against the total of the buckets that were actually read rather
    /// than against `count`, which keeps the answer self-consistent even when
    /// the two disagree by a sample or two.
    pub fn snapshot(&self) -> HistogramSnapshot {
        let mut counts = [0u64; BUCKETS];
        let mut total: u64 = 0;
        for (index, bucket) in self.buckets.iter().enumerate() {
            counts[index] = bucket.load(Ordering::Relaxed);
            total = total.saturating_add(counts[index]);
        }

        let count = self.count.load(Ordering::Relaxed);
        let sum_us = self.sum_us.load(Ordering::Relaxed);
        let min_raw = self.min_us.load(Ordering::Relaxed);
        let max_raw = self.max_us.load(Ordering::Relaxed);

        if total == 0 {
            return HistogramSnapshot {
                count,
                sum_us,
                min_us: None,
                max_us: None,
                mean_us: None,
                p50_us: None,
                p95_us: None,
                p99_us: None,
                p999_us: None,
                buckets: Vec::new(),
            };
        }

        // A reader can catch the first-ever sample between its `fetch_min` and
        // its `fetch_max`, which would leave the minimum above the maximum.
        // Sorting the pair costs one comparison and means the clamp below can
        // never be handed a backwards range.
        let (low, high) = if min_raw <= max_raw {
            (min_raw, max_raw)
        } else {
            (max_raw, min_raw)
        };

        let buckets = counts
            .iter()
            .enumerate()
            .filter(|(_, &count)| count > 0)
            .map(|(index, &count)| HistogramBucket {
                le_us: BUCKET_BOUNDS_US.get(index).copied(),
                count,
            })
            .collect();

        HistogramSnapshot {
            count,
            sum_us,
            min_us: Some(low),
            max_us: Some(high),
            mean_us: Some(sum_us / total.max(1)),
            p50_us: quantile(&counts, total, low, high, 50, 100),
            p95_us: quantile(&counts, total, low, high, 95, 100),
            p99_us: quantile(&counts, total, low, high, 99, 100),
            p999_us: quantile(&counts, total, low, high, 999, 1_000),
            buckets,
        }
    }
}

/// The value at a quantile, interpolated inside the bucket the rank fell in.
///
/// Nearest-rank first: the rank is `ceil(q × n)`, the same definition
/// `ingestion.rs` uses for its endpoint percentiles, so the two agree about
/// what a p95 is. Then the position within the winning bucket is used to
/// interpolate between its bounds, which stops every reading in a busy bucket
/// from collapsing onto that bucket's ceiling.
///
/// The answer is clamped to the exact minimum and maximum, because a bucket
/// bound is not evidence — a run where everything took 30µs must not report a
/// p99 of 50µs just because that is where the bucket ends.
fn quantile(
    counts: &[u64; BUCKETS],
    total: u64,
    min_us: u64,
    max_us: u64,
    numerator: u64,
    denominator: u64,
) -> Option<u64> {
    if total == 0 {
        return None;
    }
    let rank = ((total as u128) * (numerator as u128))
        .div_ceil(denominator as u128)
        .max(1);

    let mut cumulative: u128 = 0;
    for (index, &count) in counts.iter().enumerate() {
        if count == 0 {
            continue;
        }
        let next = cumulative + count as u128;
        if next >= rank {
            let lower = if index == 0 {
                0
            } else {
                BUCKET_BOUNDS_US[index - 1]
            };
            // The overflow bucket has no ceiling of its own, so the largest
            // reading actually seen is used as its far edge.
            let upper = if index == BUCKETS - 1 {
                max_us.max(lower)
            } else {
                BUCKET_BOUNDS_US[index]
            };
            let within = rank - cumulative;
            let span = (upper.saturating_sub(lower)) as u128;
            let value = lower as u128 + span * within / count as u128;
            return Some((value.min(u64::MAX as u128) as u64).clamp(min_us, max_us));
        }
        cumulative = next;
    }

    Some(max_us)
}

/// One bucket, as it appears in a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistogramBucket {
    /// The inclusive upper bound in microseconds, or `null` for the overflow
    /// bucket, which has none.
    pub le_us: Option<u64>,
    pub count: u64,
}

/// What a histogram looks like from outside.
///
/// `count`, `min`, `max` and `mean` are exact. The quantiles are accurate to
/// the width of their bucket. Every one of them is `null` rather than zero when
/// nothing has been measured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistogramSnapshot {
    pub count: u64,
    /// Every reading added together. Exact, and the one number a bucket ladder
    /// cannot give back — an average rebuilt from `mean` has already been
    /// rounded once. The Prometheus exporter needs it unrounded.
    pub sum_us: u64,
    pub min_us: Option<u64>,
    pub max_us: Option<u64>,
    pub mean_us: Option<u64>,
    pub p50_us: Option<u64>,
    pub p95_us: Option<u64>,
    pub p99_us: Option<u64>,
    /// The tail `STS_CORE_IDEOLOGY.md` §AC.6 writes the ingestion degradation
    /// rule against.
    pub p999_us: Option<u64>,
    /// Every bucket that has something in it, in order. Present so a run can be
    /// aggregated with another one properly — quantiles cannot be averaged, but
    /// bucket counts can be added.
    pub buckets: Vec<HistogramBucket>,
}

// ---------------------------------------------------------------------------
// the slot clock
// ---------------------------------------------------------------------------

/// How the engine's tick is behaving.
///
/// A tick here is one slot advance the engine actually acted on, not one the
/// chain produced. The distinction matters: the gap between ticks is what the
/// engine experienced, and if a slot arrived and nothing processed it, that
/// shows up as a missed slot rather than as a fast tick.
#[derive(Debug)]
pub struct SlotMetrics {
    ticks: AtomicU64,
    newest_slot: AtomicU64,
    /// A tick whose slot was behind one already seen. A fork, a provider
    /// replaying, or a clock somewhere that is wrong.
    regressions: AtomicU64,
    /// Slots that went by without a tick of their own.
    missed: AtomicU64,
    /// When the last tick landed, in microseconds since the collector started,
    /// or `UNSET` if there has not been one.
    last_tick_us: AtomicU64,
    /// The interval before the current one, or `UNSET` if there is not one yet.
    last_gap_us: AtomicU64,
    gap: Histogram,
    processing: Histogram,
    jitter: Histogram,
}

/// What the two "previously" fields hold before there is a previously.
///
/// Not zero, which is a reading a real tick can produce: a tick in the first
/// microsecond of the run has an `at_us` of zero, and two ticks inside the same
/// microsecond have an interval of zero. A sentinel a real reading can collide
/// with is a sentinel that silently discards real measurements.
const UNSET: u64 = u64::MAX;

impl Default for SlotMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl SlotMetrics {
    pub fn new() -> Self {
        Self {
            ticks: AtomicU64::new(0),
            newest_slot: AtomicU64::new(0),
            regressions: AtomicU64::new(0),
            missed: AtomicU64::new(0),
            last_tick_us: AtomicU64::new(UNSET),
            last_gap_us: AtomicU64::new(UNSET),
            gap: Histogram::new(),
            processing: Histogram::new(),
            jitter: Histogram::new(),
        }
    }

    /// Records one tick: which slot it was, when it happened, and how long the
    /// engine spent on it.
    ///
    /// The three histograms answer three different questions. `processing` is
    /// how long the work took. `gap` is how far apart the ticks arrived.
    /// `jitter` is how much each gap differed from the one before it — the
    /// cadence wobble, which is the number that says whether the feed is
    /// steady, and it is measured that way rather than against an assumed
    /// four-hundred-millisecond slot because the chain's own cadence moves.
    ///
    /// Written for one ticking thread, which is what the engine has. Two
    /// threads ticking at once cannot corrupt anything — every field is
    /// swapped, never read-modify-written — but they can interleave their gaps,
    /// which makes the jitter noisier than it really was. That is a fair price
    /// for a recorder that never takes a lock.
    fn record_tick_at(&self, slot: u64, at_us: u64, processing_us: u64) {
        self.ticks.fetch_add(1, Ordering::Relaxed);
        self.processing.record_us(processing_us);

        let previous_slot = self.newest_slot.fetch_max(slot, Ordering::Relaxed);
        if previous_slot > slot {
            self.regressions.fetch_add(1, Ordering::Relaxed);
        } else if previous_slot != 0 && slot > previous_slot + 1 {
            self.missed
                .fetch_add(slot - previous_slot - 1, Ordering::Relaxed);
        }

        let previous_tick = self.last_tick_us.swap(at_us, Ordering::Relaxed);
        if previous_tick == UNSET || at_us < previous_tick {
            // The first tick has nothing to measure against, and a reading from
            // before the previous one is two threads interleaving rather than
            // an interval. Two ticks inside the same microsecond are neither:
            // that is a real interval that rounds to zero, and it is recorded.
            return;
        }
        let gap_us = at_us - previous_tick;
        self.gap.record_us(gap_us);

        let previous_gap = self.last_gap_us.swap(gap_us, Ordering::Relaxed);
        if previous_gap != UNSET {
            self.jitter.record_us(gap_us.abs_diff(previous_gap));
        }
    }

    fn snapshot(&self, now_us: u64) -> SlotSnapshot {
        let last_tick_us = self.last_tick_us.load(Ordering::Relaxed);
        SlotSnapshot {
            ticks: self.ticks.load(Ordering::Relaxed),
            newest_slot: self.newest_slot.load(Ordering::Relaxed),
            regressions: self.regressions.load(Ordering::Relaxed),
            missed: self.missed.load(Ordering::Relaxed),
            since_last_tick_ms: if last_tick_us == UNSET {
                None
            } else {
                Some(now_us.saturating_sub(last_tick_us) / 1_000)
            },
            processing_us: self.processing.snapshot(),
            gap_us: self.gap.snapshot(),
            jitter_us: self.jitter.snapshot(),
        }
    }
}

/// The tick, as the UI and the exporter see it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotSnapshot {
    pub ticks: u64,
    /// The highest slot any tick has carried. Zero means nothing has ticked.
    pub newest_slot: u64,
    pub regressions: u64,
    pub missed: u64,
    /// How long since the last tick. `null` when there has not been one, which
    /// is not the same as a tick that just happened.
    pub since_last_tick_ms: Option<u64>,
    /// Time spent handling a tick.
    pub processing_us: HistogramSnapshot,
    /// Time between one tick and the next.
    pub gap_us: HistogramSnapshot,
    /// How much each gap differed from the gap before it.
    pub jitter_us: HistogramSnapshot,
}

// ---------------------------------------------------------------------------
// the feed: what arrived, what was lost, and how full the pipe is
// ---------------------------------------------------------------------------

/// Why a frame did not make it through.
///
/// Five reasons, because a drop that was a deliberate refusal and a drop that
/// was the engine falling behind are different failures with different fixes,
/// and a single "dropped" counter hides which one is happening.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DropReason {
    /// The queue out was full. This is the one that means the engine is slower
    /// than the feed.
    Backpressure,
    /// The bytes could not be decoded into anything.
    Undecodable,
    /// Another provider had already reported this slot.
    Stale,
    /// Understood, and refused by a filter. A working system does this to most
    /// of what it sees.
    Filtered,
    /// The durable sink would not take it, so it is not in `sts.db`.
    Persistence,
}

impl DropReason {
    pub const ALL: [DropReason; 5] = [
        DropReason::Backpressure,
        DropReason::Undecodable,
        DropReason::Stale,
        DropReason::Filtered,
        DropReason::Persistence,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            DropReason::Backpressure => "backpressure",
            DropReason::Undecodable => "undecodable",
            DropReason::Stale => "stale",
            DropReason::Filtered => "filtered",
            DropReason::Persistence => "persistence",
        }
    }

    const fn index(self) -> usize {
        match self {
            DropReason::Backpressure => 0,
            DropReason::Undecodable => 1,
            DropReason::Stale => 2,
            DropReason::Filtered => 3,
            DropReason::Persistence => 4,
        }
    }

    /// Whether this drop happened because the engine could not keep up.
    ///
    /// The difference this draws is the whole reason the reasons exist. A
    /// filtered frame is the system working — most of what a program feed sends
    /// is not a candidate, and refusing it is the job. A frame lost to a full
    /// queue or a sink that would not take it is the system failing, and it is
    /// the only kind of loss that means something has to be fixed.
    pub const fn is_overrun(self) -> bool {
        matches!(self, DropReason::Backpressure | DropReason::Persistence)
    }
}

/// How full the queue between the feed and the engine is.
///
/// Three bands rather than a raw percentage because the interesting thing is
/// not the depth, it is the crossing: a queue that has just gone from half full
/// to nearly full is a system about to start losing frames, and that moment is
/// what the transition counters below record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[repr(u8)]
pub enum BackpressureState {
    /// Under half full. The engine is keeping up.
    Nominal = 0,
    /// Half full or more. Still fine, but the margin is going.
    Elevated = 1,
    /// Nearly full. The next burst is lost frames.
    Saturated = 2,
}

/// Where `Nominal` ends, as a percentage of capacity.
pub const ELEVATED_AT_PERCENT: u64 = 50;
/// Where `Elevated` ends, as a percentage of capacity.
pub const SATURATED_AT_PERCENT: u64 = 90;

/// How many bands there are, for the fixed arrays that count them.
const BACKPRESSURE_STATES: usize = 3;

impl BackpressureState {
    pub const ALL: [BackpressureState; BACKPRESSURE_STATES] = [
        BackpressureState::Nominal,
        BackpressureState::Elevated,
        BackpressureState::Saturated,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            BackpressureState::Nominal => "nominal",
            BackpressureState::Elevated => "elevated",
            BackpressureState::Saturated => "saturated",
        }
    }

    /// The band a depth falls in.
    ///
    /// A capacity of zero is a queue that cannot hold anything, so an empty one
    /// is nominal and anything at all in it is saturated. That is the honest
    /// reading rather than a division that cannot be done.
    pub const fn for_depth(depth: u64, capacity: u64) -> Self {
        if capacity == 0 {
            return if depth == 0 {
                BackpressureState::Nominal
            } else {
                BackpressureState::Saturated
            };
        }
        let percent = depth.saturating_mul(100) / capacity;
        if percent >= SATURATED_AT_PERCENT {
            BackpressureState::Saturated
        } else if percent >= ELEVATED_AT_PERCENT {
            BackpressureState::Elevated
        } else {
            BackpressureState::Nominal
        }
    }

    const fn index(self) -> usize {
        self as usize
    }

    const fn from_index(index: u8) -> Self {
        match index {
            1 => BackpressureState::Elevated,
            2 => BackpressureState::Saturated,
            _ => BackpressureState::Nominal,
        }
    }
}

/// What the feed delivered and what it cost.
#[derive(Debug)]
pub struct FeedMetrics {
    ingested: AtomicU64,
    dropped: [AtomicU64; 5],
    state: AtomicU8,
    transitions: AtomicU64,
    entries: [AtomicU64; BACKPRESSURE_STATES],
    dwell_us: [AtomicU64; BACKPRESSURE_STATES],
    /// When the current band was entered, in microseconds since the collector
    /// started.
    entered_at_us: AtomicU64,
    depth: AtomicU64,
    capacity: AtomicU64,
    /// The fullest the queue has ever been. A high-water mark survives the
    /// burst that caused it, which a live depth does not.
    deepest: AtomicU64,
    observations: AtomicU64,
}

impl Default for FeedMetrics {
    fn default() -> Self {
        Self::new()
    }
}

impl FeedMetrics {
    pub fn new() -> Self {
        Self {
            ingested: AtomicU64::new(0),
            dropped: std::array::from_fn(|_| AtomicU64::new(0)),
            state: AtomicU8::new(BackpressureState::Nominal as u8),
            transitions: AtomicU64::new(0),
            entries: std::array::from_fn(|index| {
                // The process starts nominal, and starting there is an entry
                // into it — otherwise a run that never leaves nominal reports
                // having never been in any band at all.
                AtomicU64::new(if index == 0 { 1 } else { 0 })
            }),
            dwell_us: std::array::from_fn(|_| AtomicU64::new(0)),
            entered_at_us: AtomicU64::new(0),
            depth: AtomicU64::new(0),
            capacity: AtomicU64::new(0),
            deepest: AtomicU64::new(0),
            observations: AtomicU64::new(0),
        }
    }

    pub fn state(&self) -> BackpressureState {
        BackpressureState::from_index(self.state.load(Ordering::Relaxed))
    }

    fn record_ingested(&self, frames: u64) {
        self.ingested.fetch_add(frames, Ordering::Relaxed);
    }

    fn record_dropped(&self, reason: DropReason, frames: u64) {
        self.dropped[reason.index()].fetch_add(frames, Ordering::Relaxed);
    }

    /// Notes how full the queue is, and records a band change if that is what
    /// this reading is.
    ///
    /// The compare-and-exchange is what makes the transition count honest under
    /// several observers: only the thread that actually moved the band gets to
    /// count the move, so two threads seeing the same crossing record one
    /// transition between them rather than two.
    fn observe_queue_at(&self, depth: u64, capacity: u64, now_us: u64) -> BackpressureState {
        self.observations.fetch_add(1, Ordering::Relaxed);
        self.depth.store(depth, Ordering::Relaxed);
        self.capacity.store(capacity, Ordering::Relaxed);
        self.deepest.fetch_max(depth, Ordering::Relaxed);

        let next = BackpressureState::for_depth(depth, capacity);
        let current = self.state.load(Ordering::Relaxed);
        if current == next as u8 {
            return next;
        }
        if self
            .state
            .compare_exchange(current, next as u8, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            // Somebody else moved it first. Their transition is counted; this
            // one is the same crossing seen twice.
            return self.state();
        }

        let entered_at = self.entered_at_us.swap(now_us, Ordering::Relaxed);
        let previous = BackpressureState::from_index(current);
        self.dwell_us[previous.index()]
            .fetch_add(now_us.saturating_sub(entered_at), Ordering::Relaxed);
        self.entries[next.index()].fetch_add(1, Ordering::Relaxed);
        self.transitions.fetch_add(1, Ordering::Relaxed);
        next
    }

    fn snapshot(&self, now_us: u64) -> FeedSnapshot {
        let ingested = self.ingested.load(Ordering::Relaxed);
        let mut drops = Vec::with_capacity(DropReason::ALL.len());
        let mut dropped_total: u64 = 0;
        let mut overrun_total: u64 = 0;
        for reason in DropReason::ALL {
            let frames = self.dropped[reason.index()].load(Ordering::Relaxed);
            dropped_total = dropped_total.saturating_add(frames);
            if reason.is_overrun() {
                overrun_total = overrun_total.saturating_add(frames);
            }
            drops.push(DropCount { reason, frames });
        }

        let state = self.state();
        let entered_at = self.entered_at_us.load(Ordering::Relaxed);
        let bands = BackpressureState::ALL
            .into_iter()
            .map(|band| {
                let mut dwell_us = self.dwell_us[band.index()].load(Ordering::Relaxed);
                // The band the engine is in right now has not finished, so its
                // dwell is what has been banked plus what is still running.
                if band == state {
                    dwell_us = dwell_us.saturating_add(now_us.saturating_sub(entered_at));
                }
                BackpressureBand {
                    state: band,
                    entries: self.entries[band.index()].load(Ordering::Relaxed),
                    dwell_ms: dwell_us / 1_000,
                }
            })
            .collect();

        let offered = ingested.saturating_add(dropped_total);
        let depth = self.depth.load(Ordering::Relaxed);
        let capacity = self.capacity.load(Ordering::Relaxed);

        FeedSnapshot {
            ingested,
            dropped: dropped_total,
            drops,
            // Basis points rather than a float, for the same reason every other
            // ratio in this codebase is an integer: two runs over the same
            // numbers have to agree exactly.
            loss_bps: dropped_total
                .saturating_mul(10_000)
                .checked_div(offered)
                .unwrap_or(0),
            overrun: overrun_total,
            overrun_bps: overrun_total
                .saturating_mul(10_000)
                .checked_div(offered)
                .unwrap_or(0),
            state,
            transitions: self.transitions.load(Ordering::Relaxed),
            bands,
            depth,
            capacity,
            deepest: self.deepest.load(Ordering::Relaxed),
            fill_percent: depth.saturating_mul(100).checked_div(capacity).unwrap_or(0),
            observations: self.observations.load(Ordering::Relaxed),
        }
    }
}

/// How many frames one reason cost.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DropCount {
    pub reason: DropReason,
    pub frames: u64,
}

/// How much of the run has been spent in one backpressure band.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackpressureBand {
    pub state: BackpressureState,
    /// How many times the engine entered this band.
    pub entries: u64,
    /// How long it has spent there in total, including the stretch it is in now.
    pub dwell_ms: u64,
}

/// The feed, as the UI and the exporter see it.
///
/// `ingested` is what made it all the way through and `dropped` is everything
/// else, so the two add up to every frame the feed offered.
///
/// **`loss_bps` is not the alarm.** On a healthy run it is high, because most
/// of what a Solana program feed sends is not a candidate and refusing it is
/// the job. `overrun_bps` is the alarm: it counts only the frames lost to a
/// full queue or a sink that would not take them, which is the engine failing
/// to keep up rather than the filters doing their work.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedSnapshot {
    pub ingested: u64,
    pub dropped: u64,
    pub drops: Vec<DropCount>,
    /// Every drop as a share of everything offered, in basis points. 10000 is
    /// everything lost. Deliberate refusals are in here too — see above.
    pub loss_bps: u64,
    /// Frames lost because the engine could not keep up.
    pub overrun: u64,
    /// Those, as a share of everything offered, in basis points. This is the
    /// number worth an alarm.
    pub overrun_bps: u64,
    pub state: BackpressureState,
    pub transitions: u64,
    pub bands: Vec<BackpressureBand>,
    pub depth: u64,
    pub capacity: u64,
    pub deepest: u64,
    pub fill_percent: u64,
    /// How many depth readings this is built from. Zero means nothing has ever
    /// looked, which is not the same as a queue that is empty.
    pub observations: u64,
}

// ---------------------------------------------------------------------------
// intents and the signer
// ---------------------------------------------------------------------------

/// One state's occupancy and its total traffic.
#[derive(Debug, Default)]
struct StateCell {
    /// How many are sitting in this state right now.
    occupancy: AtomicI64,
    /// How many have ever entered it.
    entered: AtomicU64,
}

/// Where the engine's executions are.
///
/// Two state machines, counted the same way. `types::ExecutionState` is the
/// intent's own six steps, and `types::ExitState` is the finer lifecycle an
/// exit walks through the signer — constructed, signed, broadcast, confirmed,
/// failed. Both get an occupancy, which is how many are in that state now, and
/// a total, which is how many have ever been.
///
/// In-flight is the sum of the occupancies of the states that are not terminal.
/// It is a live number and it can go down; the totals only ever go up. Keeping
/// both is deliberate: "three intents in flight" and "four hundred intents have
/// been sent today" are different questions and a single counter answers
/// neither well.
#[derive(Debug)]
pub struct ExecutionMetrics {
    intents: [StateCell; ExecutionState::ALL.len()],
    exits: [StateCell; ExitState::ALL.len()],
    /// Times something left a state this collector never saw it enter.
    ///
    /// Two causes, and they are not the same. One is ordinary: an intent that
    /// predates this process. Every open obligation read back out of `sts.db`
    /// is one, so an unwind after a restart produces a handful of these and
    /// nothing is wrong. The other is a caller reporting steps out of order,
    /// which is a bug. They are indistinguishable from in here, so this is a
    /// number to read next to the run rather than an alarm on its own — what it
    /// guarantees is that the gauge above it never went negative to hide it.
    unobserved: AtomicU64,
}

impl Default for ExecutionMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Where one execution state sits in the fixed arrays above.
///
/// A match rather than a search so it costs nothing, and
/// `the_state_indexes_match_the_declared_order` proves it still lines up with
/// `ExecutionState::ALL` if anybody reorders that.
const fn intent_index(state: ExecutionState) -> usize {
    match state {
        ExecutionState::IntentCreated => 0,
        ExecutionState::Validated => 1,
        ExecutionState::Sent => 2,
        ExecutionState::Confirmed => 3,
        ExecutionState::Completed => 4,
        ExecutionState::Aborted => 5,
    }
}

const fn exit_index(state: ExitState) -> usize {
    match state {
        ExitState::ExitConstructed => 0,
        ExitState::ExitSigned => 1,
        ExitState::ExitBroadcast => 2,
        ExitState::ExitConfirmed => 3,
        ExitState::ExitFailed => 4,
    }
}

impl ExecutionMetrics {
    pub fn new() -> Self {
        Self {
            intents: std::array::from_fn(|_| StateCell::default()),
            exits: std::array::from_fn(|_| StateCell::default()),
            unobserved: AtomicU64::new(0),
        }
    }

    /// Moves one intent from wherever it was to where it is now.
    ///
    /// `from` is `None` for the first step of an intent's life, which came from
    /// nowhere — the same shape `db::ExecutionLogRow::prev_state` uses, so the
    /// row that is written and the counter that is bumped are saying the same
    /// thing.
    fn record_intent(&self, from: Option<ExecutionState>, to: ExecutionState) {
        if let Some(from) = from {
            self.leave(&self.intents[intent_index(from)]);
        }
        self.enter(&self.intents[intent_index(to)]);
    }

    fn record_exit(&self, from: Option<ExitState>, to: ExitState) {
        if let Some(from) = from {
            self.leave(&self.exits[exit_index(from)]);
        }
        self.enter(&self.exits[exit_index(to)]);
    }

    fn enter(&self, cell: &StateCell) {
        cell.occupancy.fetch_add(1, Ordering::Relaxed);
        cell.entered.fetch_add(1, Ordering::Relaxed);
    }

    /// Takes one out of a state, and refuses to go below zero.
    ///
    /// `fetch_update` rather than a plain subtraction so the floor holds under
    /// several threads without a lock: the closure sees the real current value
    /// and declines when there is nothing to take.
    fn leave(&self, cell: &StateCell) {
        let taken = cell
            .occupancy
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                if current > 0 {
                    Some(current - 1)
                } else {
                    None
                }
            });
        if taken.is_err() {
            self.unobserved.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// How many intents are somewhere they can still move from.
    pub fn in_flight_intents(&self) -> i64 {
        ExecutionState::ALL
            .into_iter()
            .filter(|state| !state.is_terminal())
            .map(|state| {
                self.intents[intent_index(state)]
                    .occupancy
                    .load(Ordering::Relaxed)
            })
            .sum()
    }

    /// How many exits the signer has not finished with.
    pub fn in_flight_exits(&self) -> i64 {
        ExitState::ALL
            .into_iter()
            .filter(|state| !state.is_terminal())
            .map(|state| {
                self.exits[exit_index(state)]
                    .occupancy
                    .load(Ordering::Relaxed)
            })
            .sum()
    }

    fn snapshot(&self) -> ExecutionSnapshot {
        ExecutionSnapshot {
            in_flight_intents: self.in_flight_intents(),
            in_flight_exits: self.in_flight_exits(),
            intents: ExecutionState::ALL
                .into_iter()
                .map(|state| {
                    let cell = &self.intents[intent_index(state)];
                    StateCount {
                        state: state.as_str(),
                        terminal: state.is_terminal(),
                        in_state: cell.occupancy.load(Ordering::Relaxed),
                        entered: cell.entered.load(Ordering::Relaxed),
                    }
                })
                .collect(),
            signer: ExitState::ALL
                .into_iter()
                .map(|state| {
                    let cell = &self.exits[exit_index(state)];
                    StateCount {
                        state: state.as_str(),
                        terminal: state.is_terminal(),
                        in_state: cell.occupancy.load(Ordering::Relaxed),
                        entered: cell.entered.load(Ordering::Relaxed),
                    }
                })
                .collect(),
            unobserved: self.unobserved.load(Ordering::Relaxed),
        }
    }
}

/// How many are in one state, and how many have been.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StateCount {
    /// The name `sts.db` writes for this state, so a metric and a row can be
    /// lined up without a translation table.
    pub state: &'static str,
    pub terminal: bool,
    pub in_state: i64,
    pub entered: u64,
}

/// The execution side, as the UI and the exporter see it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionSnapshot {
    pub in_flight_intents: i64,
    pub in_flight_exits: i64,
    /// Every intent state, in the order `ExecutionState::ALL` declares them.
    pub intents: Vec<StateCount>,
    /// Every signer state, in the order `ExitState::ALL` declares them.
    pub signer: Vec<StateCount>,
    /// Steps out of a state this collector never saw entered. See
    /// `ExecutionMetrics::unobserved` — a restart makes this non-zero honestly.
    pub unobserved: u64,
}

// ---------------------------------------------------------------------------
// the collector
// ---------------------------------------------------------------------------

/// Everything above, in one thing the engine can hold.
///
/// Cheap to share: one `Arc`, no interior locking, and every method on it is
/// safe to call from any thread at any time including during shutdown.
#[derive(Debug)]
pub struct MetricsCollector {
    /// What every internal timestamp is measured from. A monotonic clock, so
    /// nothing here is affected by the wall clock being corrected mid-run.
    epoch: Instant,
    started_at_ms: i64,
    slots: SlotMetrics,
    feed: FeedMetrics,
    execution: ExecutionMetrics,
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsCollector {
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            started_at_ms: now_ms(),
            slots: SlotMetrics::new(),
            feed: FeedMetrics::new(),
            execution: ExecutionMetrics::new(),
        }
    }

    /// Microseconds since this collector started.
    fn elapsed_us(&self) -> u64 {
        self.epoch.elapsed().as_micros().min(u64::MAX as u128) as u64
    }

    /// One slot advance the engine handled, and what it cost to handle.
    pub fn record_slot_tick(&self, slot: u64, processing: Duration) {
        let processing_us = processing.as_micros().min(u64::MAX as u128) as u64;
        self.slots
            .record_tick_at(slot, self.elapsed_us(), processing_us);
    }

    /// Frames that made it through.
    pub fn record_ingested(&self, frames: u64) {
        self.feed.record_ingested(frames);
    }

    /// Frames that did not, and why.
    pub fn record_dropped(&self, reason: DropReason, frames: u64) {
        self.feed.record_dropped(reason, frames);
    }

    /// How full the queue between the feed and the engine is, right now.
    /// Returns the band that reading puts it in.
    pub fn observe_queue(&self, depth: usize, capacity: usize) -> BackpressureState {
        self.feed
            .observe_queue_at(depth as u64, capacity as u64, self.elapsed_us())
    }

    /// One step of an intent's life. `from` is `None` for its first.
    pub fn record_intent(&self, from: Option<ExecutionState>, to: ExecutionState) {
        self.execution.record_intent(from, to);
    }

    /// One step of an exit's trip through the signer.
    pub fn record_exit(&self, from: Option<ExitState>, to: ExitState) {
        self.execution.record_exit(from, to);
    }

    pub fn slots(&self) -> &SlotMetrics {
        &self.slots
    }

    pub fn feed(&self) -> &FeedMetrics {
        &self.feed
    }

    pub fn execution(&self) -> &ExecutionMetrics {
        &self.execution
    }

    /// Every number, with the quantiles worked out.
    ///
    /// Takes no lock and changes nothing, so it is safe to call from the UI
    /// thread, from the HTTP exporter, and from both at once while the engine
    /// is at its busiest.
    pub fn snapshot(&self) -> MetricsSnapshot {
        let now_us = self.elapsed_us();
        MetricsSnapshot {
            at_ms: now_ms(),
            started_at_ms: self.started_at_ms,
            uptime_ms: (now_us / 1_000) as i64,
            source: "sts.engine",
            aggregation: "counters and bucketed quantiles, in microseconds",
            resets: "process start; nothing here is ever reset while running",
            slots: self.slots.snapshot(now_us),
            feed: self.feed.snapshot(now_us),
            execution: self.execution.snapshot(),
        }
    }
}

/// Every number the engine keeps about itself.
///
/// The three descriptive fields are there because `STS_CORE_IDEOLOGY.md`
/// §Annex V asks that every metric carry its source, its period and its reset
/// semantics. A number without them invites the reader to guess, and the guess
/// is usually "this is a rate over the last minute" when it is a total since
/// the process started.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsSnapshot {
    pub at_ms: i64,
    pub started_at_ms: i64,
    /// The period every total below covers: the whole run, so far.
    pub uptime_ms: i64,
    pub source: &'static str,
    pub aggregation: &'static str,
    pub resets: &'static str,
    pub slots: SlotSnapshot,
    pub feed: FeedSnapshot,
    pub execution: ExecutionSnapshot,
}

// ---------------------------------------------------------------------------
// the exporter
// ---------------------------------------------------------------------------

/// The address the exporter uses when it is asked for a port and nothing else.
pub const DEFAULT_METRICS_PORT: u16 = 9464;

/// The environment variable that turns the exporter on.
///
/// Unset means no socket is opened at all. That is the default on purpose: this
/// is a process that will eventually hold a signer, and a listening port it did
/// not need is a door nobody asked for. Somebody who wants monitoring says so.
pub const METRICS_ADDR_VAR: &str = "STS_METRICS_ADDR";

/// The largest request head the exporter will read before giving up on it.
const MAX_REQUEST_BYTES: usize = 4096;
/// How long one client gets to send its request, and to take its answer.
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// How many scrapes may be in flight at once. A metrics endpoint that is being
/// hit harder than this is being used as something it is not.
const MAX_CONNECTIONS: usize = 8;
/// How long the accept loop waits after a failed accept, so a broken listener
/// cannot spin a core.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(50);
/// How many accepts may fail in a row before the loop gives up and stops.
const MAX_CONSECUTIVE_ACCEPT_ERRORS: u32 = 64;

/// Why the exporter is not listening.
#[derive(Debug)]
pub enum ExporterError {
    /// `STS_METRICS_ADDR` was set to something that is not an address.
    Address(String),
    /// The address was valid and not on this machine. Refused rather than
    /// bound: these numbers describe a trading engine's internals, and the
    /// difference between "my laptop can see them" and "the network can" is not
    /// a default anybody should get by typo.
    NotLoopback(IpAddr),
    Bind(io::Error),
}

impl std::fmt::Display for ExporterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExporterError::Address(text) => write!(
                f,
                "{METRICS_ADDR_VAR} is not an address: {text:?} — \
                 write it as a port like 9464, or as 127.0.0.1:9464"
            ),
            ExporterError::NotLoopback(ip) => write!(
                f,
                "the metrics exporter refuses to bind {ip}, because it is not this machine — \
                 engine internals are served on loopback only"
            ),
            ExporterError::Bind(err) => write!(f, "the metrics port could not be opened: {err}"),
        }
    }
}

impl std::error::Error for ExporterError {}

/// Reads an address out of text.
///
/// A bare number is a port on loopback, because that is what somebody setting
/// `STS_METRICS_ADDR=9464` means, and making them write the whole thing only
/// invites them to write `0.0.0.0` to make it work.
pub fn parse_addr(text: &str) -> Result<SocketAddr, ExporterError> {
    let text = text.trim();
    if let Ok(port) = text.parse::<u16>() {
        return Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
    }
    text.parse::<SocketAddr>()
        .map_err(|_| ExporterError::Address(text.to_string()))
}

/// What `STS_METRICS_ADDR` asks for, if anything.
///
/// `Ok(None)` is the ordinary case: the variable is unset, so no port is
/// opened. An unreadable value is an error rather than a silent fallback — a
/// typo that quietly starts the exporter somewhere else is worse than one that
/// says so at startup.
pub fn addr_from_env() -> Result<Option<SocketAddr>, ExporterError> {
    match std::env::var(METRICS_ADDR_VAR) {
        Ok(text) if text.trim().is_empty() => Ok(None),
        Ok(text) => parse_addr(&text).map(Some),
        Err(_) => Ok(None),
    }
}

/// Refuses any address that is not on this machine.
pub fn ensure_loopback(addr: SocketAddr) -> Result<SocketAddr, ExporterError> {
    if addr.ip().is_loopback() {
        Ok(addr)
    } else {
        Err(ExporterError::NotLoopback(addr.ip()))
    }
}

/// A port that is open but not yet being served.
///
/// Binding and serving are separate because they fail differently and at
/// different times. Binding is synchronous, happens once at startup, and either
/// works or gives the operator a reason it did not. Serving needs a running
/// async runtime and never fails afterwards — so `run()` can report a bad port
/// before the window exists, rather than discovering it in a background task
/// nobody is reading.
#[derive(Debug)]
pub struct BoundExporter {
    listener: StdTcpListener,
    addr: SocketAddr,
}

impl BoundExporter {
    /// Opens the port. Loopback only.
    ///
    /// Port zero is honoured and resolved: `addr()` afterwards is the port the
    /// operating system actually gave out, which is what the tests bind and
    /// what an operator needs to be told.
    pub fn bind(addr: SocketAddr) -> Result<Self, ExporterError> {
        let addr = ensure_loopback(addr)?;
        let listener = StdTcpListener::bind(addr).map_err(ExporterError::Bind)?;
        // The reactor takes it over from here, and it can only do that with a
        // socket that never blocks the thread it is polled on.
        listener
            .set_nonblocking(true)
            .map_err(ExporterError::Bind)?;
        let addr = listener.local_addr().map_err(ExporterError::Bind)?;
        Ok(Self { listener, addr })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Starts answering scrapes. Must be called with a tokio runtime in scope.
    pub fn serve(self, collector: Arc<MetricsCollector>) -> MetricsExporter {
        let addr = self.addr;
        let stats = Arc::new(ExporterStats::default());
        let listener = self.listener;
        let task = tokio::spawn({
            let stats = Arc::clone(&stats);
            async move {
                let listener = match TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(_) => {
                        // The socket was already set non-blocking, so this is
                        // effectively unreachable. If it ever is reached the
                        // exporter simply never serves, and `serving` says so.
                        stats.serving.store(false, Ordering::Relaxed);
                        return;
                    }
                };
                stats.serving.store(true, Ordering::Relaxed);
                accept_loop(listener, collector, Arc::clone(&stats)).await;
                stats.serving.store(false, Ordering::Relaxed);
            }
        });
        MetricsExporter { addr, task, stats }
    }
}

/// Counters about the exporter itself, so a scrape that never arrives can be
/// told apart from one that arrived and was refused.
#[derive(Debug, Default)]
struct ExporterStats {
    serving: AtomicBool,
    requests: AtomicU64,
    /// Connections turned away because `MAX_CONNECTIONS` were already open.
    rejected: AtomicU64,
    /// Connections from somewhere that is not this machine. Should be
    /// impossible on a loopback socket; counted because "should be impossible"
    /// is not a thing to assume about a socket.
    refused: AtomicU64,
    accept_errors: AtomicU64,
    open: AtomicUsize,
}

/// The running exporter.
///
/// Dropping this does not stop it — the accept loop owns everything it needs.
/// `stop` is what stops it, and it is safe to call from a thread that is
/// closing the window.
#[derive(Debug)]
pub struct MetricsExporter {
    addr: SocketAddr,
    task: JoinHandle<()>,
    stats: Arc<ExporterStats>,
}

impl MetricsExporter {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops accepting. Safe to call twice.
    ///
    /// Aborts rather than draining: the worst it can interrupt is half a
    /// metrics response to a monitoring tool, and making the window wait on a
    /// scraper would be the wrong trade at shutdown.
    pub fn stop(&self) {
        self.task.abort();
        self.stats.serving.store(false, Ordering::Relaxed);
    }

    pub fn status(&self) -> ExporterStatus {
        ExporterStatus {
            addr: self.addr.to_string(),
            serving: self.stats.serving.load(Ordering::Relaxed),
            requests: self.stats.requests.load(Ordering::Relaxed),
            rejected: self.stats.rejected.load(Ordering::Relaxed),
            refused: self.stats.refused.load(Ordering::Relaxed),
            accept_errors: self.stats.accept_errors.load(Ordering::Relaxed),
            open: self.stats.open.load(Ordering::Relaxed),
        }
    }
}

/// What the exporter has been asked for.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExporterStatus {
    pub addr: String,
    pub serving: bool,
    pub requests: u64,
    pub rejected: u64,
    pub refused: u64,
    pub accept_errors: u64,
    pub open: usize,
}

async fn accept_loop(
    listener: TcpListener,
    collector: Arc<MetricsCollector>,
    stats: Arc<ExporterStats>,
) {
    let mut consecutive_errors: u32 = 0;
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => {
                consecutive_errors = 0;
                accepted
            }
            Err(_) => {
                stats.accept_errors.fetch_add(1, Ordering::Relaxed);
                consecutive_errors = consecutive_errors.saturating_add(1);
                if consecutive_errors >= MAX_CONSECUTIVE_ACCEPT_ERRORS {
                    return;
                }
                tokio::time::sleep(ACCEPT_BACKOFF).await;
                continue;
            }
        };

        // A loopback socket cannot be reached from anywhere else, so this is
        // belt and braces. It costs one comparison and it means a future change
        // to the bind address cannot quietly turn into an open port.
        if !peer.ip().is_loopback() {
            stats.refused.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        let open = stats.open.fetch_add(1, Ordering::Relaxed);
        if open >= MAX_CONNECTIONS {
            stats.open.fetch_sub(1, Ordering::Relaxed);
            stats.rejected.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        let collector = Arc::clone(&collector);
        let stats = Arc::clone(&stats);
        tokio::spawn(async move {
            // The count comes back down when this guard is dropped, which
            // happens however the task ends. A plain decrement at the bottom
            // would be skipped by a panic, and eight skipped decrements would
            // leave the exporter permanently refusing everybody — a monitoring
            // endpoint that has quietly stopped answering is the worst way for
            // one to fail.
            let _open = OpenConnection(Arc::clone(&stats));
            // The result is deliberately ignored: a client that hangs up half
            // way through its own scrape is not an engine problem, and there is
            // nobody to report it to who would act on it.
            let _ = serve_connection(stream, &collector, &stats).await;
        });
    }
}

/// Holds one slot of `MAX_CONNECTIONS` for as long as it is alive.
struct OpenConnection(Arc<ExporterStats>);

impl Drop for OpenConnection {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn serve_connection(
    mut stream: TcpStream,
    collector: &MetricsCollector,
    stats: &ExporterStats,
) -> io::Result<()> {
    let mut buffer = [0u8; MAX_REQUEST_BYTES];
    let mut filled = 0usize;

    let head_len = loop {
        if let Some(end) = find_head_end(&buffer[..filled]) {
            break end;
        }
        if filled == buffer.len() {
            // Too much head for a request that should be one line. Answer, do
            // not parse.
            let response =
                HttpResponse::text(431, "Request Header Fields Too Large", "too large\n");
            return write_response(&mut stream, &response, true).await;
        }
        let read = tokio::time::timeout(READ_TIMEOUT, stream.read(&mut buffer[filled..]))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the client took too long"))??;
        if read == 0 {
            // Hung up before finishing. Nothing to answer.
            return Ok(());
        }
        filled += read;
    };

    let head = String::from_utf8_lossy(&buffer[..head_len]);
    let request_line = head.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or("/");

    let accept = header_value(&head, "accept").unwrap_or("");

    stats.requests.fetch_add(1, Ordering::Relaxed);
    let response = route(method, target, accept, collector);
    let with_body = !method.eq_ignore_ascii_case("HEAD");
    write_response(&mut stream, &response, with_body).await
}

/// One header's value out of a request head, matched without regard to case.
///
/// The first of a repeated header wins rather than the values being joined. A
/// client that sends two `Accept` lines is not something a read-only endpoint
/// needs to reconcile, and the first is what it would have meant anyway.
fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    // `skip(1)` steps over the request line, which has no colon-separated name
    // and would otherwise match anything looking for a header called `GET /x`.
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim().eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

/// Where the request head ends, in either of the two line endings a client
/// might send.
fn find_head_end(bytes: &[u8]) -> Option<usize> {
    if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
        return Some(index + 4);
    }
    bytes
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| index + 2)
}

async fn write_response(
    stream: &mut TcpStream,
    response: &HttpResponse,
    with_body: bool,
) -> io::Result<()> {
    let bytes = response.render(with_body);
    tokio::time::timeout(WRITE_TIMEOUT, stream.write_all(&bytes))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the client would not take it"))??;
    let _ = stream.shutdown().await;
    Ok(())
}

/// One answer, before it is turned into bytes.
///
/// A plain value rather than something written straight to the socket, so the
/// routing can be tested without a network: `route` is a pure function of the
/// request and the collector, and every case it can produce is checked below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub reason: &'static str,
    pub content_type: &'static str,
    pub body: String,
    /// Sent as `Allow` on a 405, so a client is told what it should have used.
    pub allow: Option<&'static str>,
}

impl HttpResponse {
    fn json(status: u16, reason: &'static str, body: String) -> Self {
        Self {
            status,
            reason,
            content_type: "application/json; charset=utf-8",
            body,
            allow: None,
        }
    }

    fn prometheus(body: String) -> Self {
        Self {
            status: 200,
            reason: "OK",
            content_type: crate::prometheus::CONTENT_TYPE,
            body,
            allow: None,
        }
    }

    fn text(status: u16, reason: &'static str, body: impl Into<String>) -> Self {
        Self {
            status,
            reason,
            content_type: "text/plain; charset=utf-8",
            body: body.into(),
            allow: None,
        }
    }

    /// The response as bytes. `with_body` is false for a HEAD, which gets the
    /// same headers — including the length the body would have had — and
    /// nothing after them.
    pub fn render(&self, with_body: bool) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
             Cache-Control: no-store\r\nConnection: close\r\n",
            self.status,
            self.reason,
            self.content_type,
            self.body.len()
        );
        if let Some(allow) = self.allow {
            out.push_str(&format!("Allow: {allow}\r\n"));
        }
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        if with_body {
            bytes.extend_from_slice(self.body.as_bytes());
        }
        bytes
    }
}

/// Which rendering of the same snapshot a client gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// The shape the window reads, and what this endpoint has always served.
    Json,
    /// The text exposition format a Prometheus server scrapes.
    Prometheus,
}

/// What an `Accept` header is asking for.
///
/// A Prometheus server always names a format it can read — `text/plain` with a
/// version on it, or the OpenMetrics type. Anything else gets JSON, and that
/// includes both an absent header and the `*/*` that curl sends by default:
/// adding a second format is not a reason to change what an existing reader
/// already receives from `/metrics`.
///
/// Quality values are deliberately not weighed. The only client whose header is
/// complicated enough to have them is Prometheus itself, and every branch of
/// its header wants the format this returns for it.
pub fn negotiate(accept: &str) -> Format {
    let accept = accept.to_ascii_lowercase();
    if accept.contains("application/openmetrics-text") || accept.contains("text/plain") {
        Format::Prometheus
    } else {
        Format::Json
    }
}

/// The whole routing table.
///
/// Six answers and nothing else. There is no way to change anything through
/// this endpoint, no path that takes a parameter, and nothing that reads a
/// request body — an exporter is a thing that is read.
///
/// `/metrics` answers in whichever format the client asked for, and the two
/// suffixed paths beside it answer in one regardless. Both exist because
/// negotiation is right for a scraper that sends a real `Accept` and useless
/// for a person with a terminal, who wants to name what they want and see it.
pub fn route(
    method: &str,
    target: &str,
    accept: &str,
    collector: &MetricsCollector,
) -> HttpResponse {
    if !method.eq_ignore_ascii_case("GET") && !method.eq_ignore_ascii_case("HEAD") {
        return HttpResponse {
            allow: Some("GET, HEAD"),
            ..HttpResponse::text(405, "Method Not Allowed", "read only\n")
        };
    }

    // Anything after a `?` is a scraper's cache-buster, not an instruction.
    let path = target.split(['?', '#']).next().unwrap_or("/");
    let path = if path.len() > 1 {
        path.trim_end_matches('/')
    } else {
        path
    };

    match path {
        "/metrics" => render(negotiate(accept), collector),
        "/metrics.prom" => render(Format::Prometheus, collector),
        "/metrics.json" => render(Format::Json, collector),
        "/healthz" => HttpResponse::text(200, "OK", "ok\n"),
        "/" => HttpResponse::text(
            200,
            "OK",
            "sts metrics\n/metrics\n/metrics.prom\n/metrics.json\n/healthz\n",
        ),
        _ => HttpResponse::text(404, "Not Found", "no such thing here\n"),
    }
}

/// One snapshot, in one format.
///
/// Taken once and rendered once either way, so the two formats can never
/// describe two different moments.
fn render(format: Format, collector: &MetricsCollector) -> HttpResponse {
    let snapshot = collector.snapshot();
    match format {
        Format::Prometheus => HttpResponse::prometheus(crate::prometheus::render(&snapshot)),
        Format::Json => match serde_json::to_string_pretty(&snapshot) {
            Ok(body) => HttpResponse::json(200, "OK", format!("{body}\n")),
            // Unreachable with the types above, which are all plain data. Kept
            // as an answer rather than an unwrap because a metrics endpoint
            // that panics takes a thread of the trading engine's runtime with
            // it.
            Err(_) => {
                HttpResponse::text(500, "Internal Server Error", "metrics would not render\n")
            }
        },
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // the histogram
    // -----------------------------------------------------------------------

    #[test]
    fn an_empty_histogram_reports_nothing_rather_than_zero() {
        let snapshot = Histogram::new().snapshot();
        assert_eq!(snapshot.count, 0);
        assert_eq!(
            snapshot.min_us, None,
            "a minimum of zero would read as instant"
        );
        assert_eq!(snapshot.max_us, None);
        assert_eq!(snapshot.mean_us, None);
        assert_eq!(snapshot.p50_us, None);
        assert_eq!(snapshot.p95_us, None);
        assert_eq!(snapshot.p99_us, None);
        assert_eq!(snapshot.p999_us, None);
        assert!(snapshot.buckets.is_empty());
    }

    #[test]
    fn a_reading_lands_in_the_bucket_that_contains_it() {
        // The bound itself belongs to its own bucket, not the next one.
        assert_eq!(Histogram::bucket_of(0), 0);
        assert_eq!(Histogram::bucket_of(1), 0);
        assert_eq!(Histogram::bucket_of(2), 1);
        assert_eq!(Histogram::bucket_of(3), 2);
        assert_eq!(Histogram::bucket_of(5), 2);
        assert_eq!(Histogram::bucket_of(6), 3);
        assert_eq!(Histogram::bucket_of(5_000_000), BUCKETS - 2);
        assert_eq!(
            Histogram::bucket_of(5_000_001),
            BUCKETS - 1,
            "over the top is the overflow"
        );
        assert_eq!(Histogram::bucket_of(u64::MAX), BUCKETS - 1);
    }

    #[test]
    fn one_sample_is_its_own_every_quantile() {
        let histogram = Histogram::new();
        histogram.record_us(750);
        let snapshot = histogram.snapshot();
        assert_eq!(snapshot.count, 1);
        assert_eq!(snapshot.min_us, Some(750));
        assert_eq!(snapshot.max_us, Some(750));
        assert_eq!(snapshot.mean_us, Some(750));
        assert_eq!(snapshot.p50_us, Some(750));
        assert_eq!(snapshot.p999_us, Some(750));
    }

    #[test]
    fn quantiles_are_the_nearest_rank_interpolated_inside_the_bucket() {
        let histogram = Histogram::new();
        for micros in 1..=1_000u64 {
            histogram.record_us(micros);
        }
        let snapshot = histogram.snapshot();
        assert_eq!(snapshot.count, 1_000);
        assert_eq!(snapshot.min_us, Some(1));
        assert_eq!(snapshot.max_us, Some(1_000));
        assert_eq!(snapshot.mean_us, Some(500));
        // Evenly spread readings interpolate back to exactly where they were,
        // which is the property that says the interpolation is doing its job.
        assert_eq!(snapshot.p50_us, Some(500));
        assert_eq!(snapshot.p95_us, Some(950));
        assert_eq!(snapshot.p99_us, Some(990));
        assert_eq!(snapshot.p999_us, Some(999));
    }

    #[test]
    fn quantiles_never_go_backwards() {
        let histogram = Histogram::new();
        for micros in [3u64, 90, 12, 480, 7, 1_500, 65, 220, 8, 41] {
            histogram.record_us(micros);
        }
        let snapshot = histogram.snapshot();
        let p50 = snapshot.p50_us.expect("ten samples");
        let p95 = snapshot.p95_us.expect("ten samples");
        let p99 = snapshot.p99_us.expect("ten samples");
        let p999 = snapshot.p999_us.expect("ten samples");
        assert!(
            p50 <= p95 && p95 <= p99 && p99 <= p999,
            "{p50} {p95} {p99} {p999}"
        );
        assert_eq!(snapshot.min_us, Some(3));
        assert_eq!(snapshot.max_us, Some(1_500));
    }

    #[test]
    fn a_quantile_is_never_outside_what_was_actually_measured() {
        let histogram = Histogram::new();
        for _ in 0..1_000 {
            histogram.record_us(30);
        }
        let snapshot = histogram.snapshot();
        // 30µs sits in the "up to 50µs" bucket. Reporting 50 would be quoting
        // the bucket's ceiling as if it were a measurement.
        assert_eq!(snapshot.p999_us, Some(30));
        assert_eq!(snapshot.p50_us, Some(30));
    }

    #[test]
    fn the_overflow_bucket_is_bounded_by_the_largest_reading_seen() {
        let histogram = Histogram::new();
        histogram.record_us(10_000_000);
        let snapshot = histogram.snapshot();
        assert_eq!(snapshot.p999_us, Some(10_000_000));
        let overflow = snapshot
            .buckets
            .last()
            .expect("one bucket has something in it");
        assert_eq!(overflow.le_us, None, "the last bucket has no ceiling");
        assert_eq!(overflow.count, 1);
    }

    #[test]
    fn only_buckets_with_something_in_them_are_reported() {
        let histogram = Histogram::new();
        histogram.record_us(7);
        histogram.record_us(7);
        histogram.record_us(3_000);
        let snapshot = histogram.snapshot();
        assert_eq!(snapshot.buckets.len(), 2);
        assert_eq!(snapshot.buckets[0].le_us, Some(10));
        assert_eq!(snapshot.buckets[0].count, 2);
        assert_eq!(snapshot.buckets[1].le_us, Some(5_000));
        assert_eq!(snapshot.buckets[1].count, 1);
    }

    #[test]
    fn a_duration_is_recorded_in_microseconds() {
        let histogram = Histogram::new();
        histogram.record(Duration::from_millis(3));
        assert_eq!(histogram.snapshot().max_us, Some(3_000));
    }

    // -----------------------------------------------------------------------
    // the slot clock
    // -----------------------------------------------------------------------

    #[test]
    fn the_first_tick_has_nothing_to_measure_against() {
        let collector = MetricsCollector::new();
        collector.slots().record_tick_at(100, 1_000, 40);
        let slots = collector.slots().snapshot(2_000);
        assert_eq!(slots.ticks, 1);
        assert_eq!(slots.newest_slot, 100);
        assert_eq!(slots.processing_us.count, 1);
        assert_eq!(slots.gap_us.count, 0, "one tick is not an interval");
        assert_eq!(slots.jitter_us.count, 0, "one interval is not a wobble");
    }

    #[test]
    fn ticks_measure_the_gap_the_work_and_the_wobble() {
        let collector = MetricsCollector::new();
        // Four ticks: 400ms, then 500ms, then 400ms apart.
        collector.slots().record_tick_at(100, 0, 50);
        collector.slots().record_tick_at(101, 400_000, 60);
        collector.slots().record_tick_at(102, 900_000, 55);
        collector.slots().record_tick_at(103, 1_300_000, 45);

        let slots = collector.slots().snapshot(1_300_000);
        assert_eq!(slots.ticks, 4);
        assert_eq!(slots.processing_us.count, 4);
        assert_eq!(slots.processing_us.min_us, Some(45));
        assert_eq!(slots.processing_us.max_us, Some(60));

        assert_eq!(slots.gap_us.count, 3);
        assert_eq!(slots.gap_us.min_us, Some(400_000));
        assert_eq!(slots.gap_us.max_us, Some(500_000));

        // Two gaps changed by 100ms each, in opposite directions. Jitter is
        // about the size of the change, not its sign.
        assert_eq!(slots.jitter_us.count, 2);
        assert_eq!(slots.jitter_us.min_us, Some(100_000));
        assert_eq!(slots.jitter_us.max_us, Some(100_000));

        assert_eq!(slots.since_last_tick_ms, Some(0));
    }

    #[test]
    fn a_slot_that_goes_backwards_is_counted_rather_than_believed() {
        let collector = MetricsCollector::new();
        collector.slots().record_tick_at(100, 0, 10);
        collector.slots().record_tick_at(90, 400_000, 10);
        let slots = collector.slots().snapshot(400_000);
        assert_eq!(slots.regressions, 1);
        assert_eq!(slots.newest_slot, 100, "the newest slot does not go down");
        assert_eq!(slots.missed, 0);
    }

    #[test]
    fn slots_that_went_by_without_a_tick_are_counted() {
        let collector = MetricsCollector::new();
        collector.slots().record_tick_at(100, 0, 10);
        collector.slots().record_tick_at(105, 400_000, 10);
        let slots = collector.slots().snapshot(400_000);
        assert_eq!(slots.missed, 4, "101 through 104 never got their own tick");
        assert_eq!(slots.regressions, 0);
    }

    #[test]
    fn a_clock_that_has_never_ticked_says_so() {
        let slots = MetricsCollector::new().snapshot().slots;
        assert_eq!(slots.ticks, 0);
        assert_eq!(
            slots.since_last_tick_ms, None,
            "never is not the same as just now"
        );
    }

    #[test]
    fn a_real_tick_is_measured_against_the_running_clock() {
        let collector = MetricsCollector::new();
        collector.record_slot_tick(1, Duration::from_micros(80));
        collector.record_slot_tick(2, Duration::from_micros(90));
        let slots = collector.snapshot().slots;
        assert_eq!(slots.ticks, 2);
        assert_eq!(slots.processing_us.count, 2);
        assert_eq!(
            slots.gap_us.count, 1,
            "the second tick has an interval behind it"
        );
        assert!(slots.since_last_tick_ms.is_some());
    }

    // -----------------------------------------------------------------------
    // the feed
    // -----------------------------------------------------------------------

    #[test]
    fn every_drop_is_counted_under_its_own_reason() {
        let collector = MetricsCollector::new();
        collector.record_ingested(10);
        collector.record_dropped(DropReason::Backpressure, 3);
        collector.record_dropped(DropReason::Stale, 2);

        let feed = collector.snapshot().feed;
        assert_eq!(feed.ingested, 10);
        assert_eq!(feed.dropped, 5);
        assert_eq!(
            feed.loss_bps, 3_333,
            "five of fifteen offered, in basis points"
        );
        assert_eq!(
            feed.overrun, 3,
            "only the queue-full drops mean the engine fell behind"
        );
        assert_eq!(feed.overrun_bps, 2_000, "three of fifteen offered");

        let by_reason = |reason: DropReason| {
            feed.drops
                .iter()
                .find(|drop| drop.reason == reason)
                .map(|drop| drop.frames)
        };
        assert_eq!(by_reason(DropReason::Backpressure), Some(3));
        assert_eq!(by_reason(DropReason::Stale), Some(2));
        assert_eq!(by_reason(DropReason::Filtered), Some(0));
        assert_eq!(
            feed.drops.len(),
            DropReason::ALL.len(),
            "every reason is reported, even at zero"
        );
    }

    #[test]
    fn nothing_offered_is_no_loss_rather_than_total_loss() {
        let feed = MetricsCollector::new().snapshot().feed;
        assert_eq!(feed.loss_bps, 0);
        assert_eq!(feed.overrun_bps, 0);
        assert_eq!(feed.observations, 0, "nobody has looked at the queue yet");
    }

    #[test]
    fn a_run_that_only_filters_has_lost_nothing_it_wanted() {
        let collector = MetricsCollector::new();
        collector.record_ingested(1);
        collector.record_dropped(DropReason::Filtered, 9_999);
        let feed = collector.snapshot().feed;
        assert_eq!(feed.loss_bps, 9_999, "almost everything was refused");
        assert_eq!(
            feed.overrun_bps, 0,
            "and none of it because the engine was slow"
        );
    }

    #[test]
    fn the_band_follows_how_full_the_queue_is() {
        use BackpressureState::*;
        assert_eq!(BackpressureState::for_depth(0, 100), Nominal);
        assert_eq!(BackpressureState::for_depth(49, 100), Nominal);
        assert_eq!(BackpressureState::for_depth(50, 100), Elevated);
        assert_eq!(BackpressureState::for_depth(89, 100), Elevated);
        assert_eq!(BackpressureState::for_depth(90, 100), Saturated);
        assert_eq!(BackpressureState::for_depth(100, 100), Saturated);
        // A queue with no room at all: empty is fine, anything else is not.
        assert_eq!(BackpressureState::for_depth(0, 0), Nominal);
        assert_eq!(BackpressureState::for_depth(1, 0), Saturated);
    }

    #[test]
    fn only_a_crossing_counts_as_a_transition() {
        let collector = MetricsCollector::new();
        assert_eq!(collector.observe_queue(10, 100), BackpressureState::Nominal);
        assert_eq!(
            collector.observe_queue(60, 100),
            BackpressureState::Elevated
        );
        assert_eq!(
            collector.observe_queue(70, 100),
            BackpressureState::Elevated
        );
        assert_eq!(
            collector.observe_queue(95, 100),
            BackpressureState::Saturated
        );
        assert_eq!(collector.observe_queue(10, 100), BackpressureState::Nominal);

        let feed = collector.snapshot().feed;
        assert_eq!(feed.transitions, 3, "five readings, three band changes");
        assert_eq!(feed.observations, 5);
        assert_eq!(feed.state, BackpressureState::Nominal);

        let entries = |state: BackpressureState| {
            feed.bands
                .iter()
                .find(|band| band.state == state)
                .map(|band| band.entries)
        };
        assert_eq!(
            entries(BackpressureState::Nominal),
            Some(2),
            "it started there and came back"
        );
        assert_eq!(entries(BackpressureState::Elevated), Some(1));
        assert_eq!(entries(BackpressureState::Saturated), Some(1));
    }

    #[test]
    fn dwell_adds_up_across_the_bands_including_the_one_it_is_in() {
        let collector = MetricsCollector::new();
        let feed = collector.feed();
        // A second nominal, then two seconds elevated, then one more second
        // nominal that has not finished yet.
        feed.observe_queue_at(60, 100, 1_000_000);
        feed.observe_queue_at(10, 100, 3_000_000);
        let snapshot = feed.snapshot(4_000_000);

        let dwell = |state: BackpressureState| {
            snapshot
                .bands
                .iter()
                .find(|band| band.state == state)
                .map(|band| band.dwell_ms)
        };
        assert_eq!(
            dwell(BackpressureState::Nominal),
            Some(2_000),
            "one second banked, one running"
        );
        assert_eq!(dwell(BackpressureState::Elevated), Some(2_000));
        assert_eq!(dwell(BackpressureState::Saturated), Some(0));
    }

    #[test]
    fn the_fullest_the_queue_ever_got_survives_the_burst() {
        let collector = MetricsCollector::new();
        collector.observe_queue(200, 256);
        collector.observe_queue(5, 256);
        let feed = collector.snapshot().feed;
        assert_eq!(feed.depth, 5, "the live depth is now");
        assert_eq!(feed.deepest, 200, "the high-water mark is what happened");
        assert_eq!(feed.capacity, 256);
        assert_eq!(feed.fill_percent, 1);
    }

    // -----------------------------------------------------------------------
    // intents and the signer
    // -----------------------------------------------------------------------

    #[test]
    fn the_state_indexes_match_the_declared_order() {
        for state in ExecutionState::ALL {
            assert_eq!(ExecutionState::ALL[intent_index(state)], state);
        }
        for state in ExitState::ALL {
            assert_eq!(ExitState::ALL[exit_index(state)], state);
        }
    }

    #[test]
    fn an_intent_is_only_ever_in_one_state() {
        let collector = MetricsCollector::new();
        collector.record_intent(None, ExecutionState::IntentCreated);
        collector.record_intent(
            Some(ExecutionState::IntentCreated),
            ExecutionState::Validated,
        );
        collector.record_intent(Some(ExecutionState::Validated), ExecutionState::Sent);

        let execution = collector.snapshot().execution;
        let in_state = |name: &str| {
            execution
                .intents
                .iter()
                .find(|count| count.state == name)
                .map(|count| count.in_state)
        };
        assert_eq!(in_state("intent_created"), Some(0));
        assert_eq!(in_state("validated"), Some(0));
        assert_eq!(in_state("sent"), Some(1));
        assert_eq!(execution.in_flight_intents, 1);
        assert_eq!(execution.unobserved, 0);

        let entered = |name: &str| {
            execution
                .intents
                .iter()
                .find(|count| count.state == name)
                .map(|count| count.entered)
        };
        assert_eq!(
            entered("intent_created"),
            Some(1),
            "the totals remember where it has been"
        );
        assert_eq!(entered("validated"), Some(1));
    }

    #[test]
    fn a_terminal_state_stops_counting_as_in_flight() {
        let collector = MetricsCollector::new();
        collector.record_intent(None, ExecutionState::IntentCreated);
        collector.record_intent(
            Some(ExecutionState::IntentCreated),
            ExecutionState::Validated,
        );
        collector.record_intent(Some(ExecutionState::Validated), ExecutionState::Sent);
        collector.record_intent(Some(ExecutionState::Sent), ExecutionState::Confirmed);
        collector.record_intent(Some(ExecutionState::Confirmed), ExecutionState::Completed);

        let execution = collector.snapshot().execution;
        assert_eq!(execution.in_flight_intents, 0);
        let completed = execution
            .intents
            .iter()
            .find(|count| count.state == "completed")
            .expect("completed is one of the six");
        assert!(completed.terminal);
        assert_eq!(completed.in_state, 1);
    }

    #[test]
    fn two_intents_at_once_are_two_in_flight() {
        let collector = MetricsCollector::new();
        collector.record_intent(None, ExecutionState::IntentCreated);
        collector.record_intent(None, ExecutionState::IntentCreated);
        collector.record_intent(
            Some(ExecutionState::IntentCreated),
            ExecutionState::Validated,
        );
        assert_eq!(collector.execution().in_flight_intents(), 2);
    }

    #[test]
    fn leaving_a_state_nothing_was_in_is_counted_rather_than_hidden() {
        let collector = MetricsCollector::new();
        // Nothing was ever seen entering `sent` — an obligation read back out
        // of the database after a restart looks exactly like this. The gauge
        // must not go negative over it, and the fact must not vanish either.
        collector.record_intent(Some(ExecutionState::Sent), ExecutionState::Confirmed);
        let execution = collector.snapshot().execution;
        assert_eq!(execution.unobserved, 1);
        let sent = execution
            .intents
            .iter()
            .find(|count| count.state == "sent")
            .expect("sent is one of the six");
        assert_eq!(
            sent.in_state, 0,
            "a gauge that has gone negative is worse than one that complains"
        );
        assert_eq!(
            execution.in_flight_intents, 1,
            "the confirmed one is still real"
        );
    }

    #[test]
    fn the_signer_distribution_walks_the_exit_states() {
        let collector = MetricsCollector::new();
        collector.record_exit(None, ExitState::ExitConstructed);
        collector.record_exit(Some(ExitState::ExitConstructed), ExitState::ExitSigned);
        assert_eq!(collector.execution().in_flight_exits(), 1);
        collector.record_exit(Some(ExitState::ExitSigned), ExitState::ExitBroadcast);
        assert_eq!(
            collector.execution().in_flight_exits(),
            1,
            "broadcast is still in flight"
        );
        collector.record_exit(Some(ExitState::ExitBroadcast), ExitState::ExitConfirmed);
        assert_eq!(
            collector.execution().in_flight_exits(),
            0,
            "confirmed is the end of it"
        );

        let execution = collector.snapshot().execution;
        let names: Vec<&str> = execution.signer.iter().map(|count| count.state).collect();
        assert_eq!(
            names,
            vec![
                "exit_constructed",
                "exit_signed",
                "exit_broadcast",
                "exit_confirmed",
                "exit_failed"
            ],
            "the distribution is reported in the order the state machine declares"
        );
        for count in &execution.signer {
            let expected = if count.state == "exit_failed" { 0 } else { 1 };
            assert_eq!(count.entered, expected, "{} was entered once", count.state);
        }
    }

    #[test]
    fn a_failed_exit_is_out_of_flight_too() {
        let collector = MetricsCollector::new();
        collector.record_exit(None, ExitState::ExitConstructed);
        collector.record_exit(Some(ExitState::ExitConstructed), ExitState::ExitFailed);
        assert_eq!(collector.execution().in_flight_exits(), 0);
        assert_eq!(collector.snapshot().execution.unobserved, 0);
    }

    // -----------------------------------------------------------------------
    // the collector as a whole
    // -----------------------------------------------------------------------

    #[test]
    fn a_snapshot_says_what_it_is_and_what_it_covers() {
        let snapshot = MetricsCollector::new().snapshot();
        assert_eq!(snapshot.source, "sts.engine");
        assert!(!snapshot.aggregation.is_empty());
        assert!(!snapshot.resets.is_empty());
        assert!(snapshot.started_at_ms > 0);
        assert!(snapshot.uptime_ms >= 0);
    }

    #[test]
    fn reading_the_counters_changes_none_of_them() {
        let collector = MetricsCollector::new();
        collector.record_ingested(4);
        collector.record_slot_tick(9, Duration::from_micros(20));
        let first = collector.snapshot();
        let second = collector.snapshot();
        assert_eq!(first.feed.ingested, second.feed.ingested);
        assert_eq!(first.slots.ticks, second.slots.ticks);
        assert_eq!(
            first.slots.processing_us.count,
            second.slots.processing_us.count
        );
    }

    #[test]
    fn counters_hold_up_under_every_thread_at_once() {
        let collector = Arc::new(MetricsCollector::new());
        let threads = 4u64;
        let each = 2_000u64;

        std::thread::scope(|scope| {
            for worker in 0..threads {
                let collector = Arc::clone(&collector);
                scope.spawn(move || {
                    for step in 0..each {
                        collector.record_ingested(1);
                        collector.record_dropped(DropReason::Backpressure, 1);
                        collector
                            .slots()
                            .record_tick_at(step + 1, step * 400 + worker, 30);
                        collector.record_intent(None, ExecutionState::IntentCreated);
                        collector.record_intent(
                            Some(ExecutionState::IntentCreated),
                            ExecutionState::Aborted,
                        );
                    }
                });
            }
            // Reading while every one of those is writing. If a snapshot could
            // block a writer, or a writer could tear a snapshot, this is where
            // it would show.
            scope.spawn(|| {
                for _ in 0..200 {
                    let snapshot = collector.snapshot();
                    assert!(snapshot.feed.ingested <= threads * each);
                    assert!(snapshot.execution.in_flight_intents >= 0);
                }
            });
        });

        let snapshot = collector.snapshot();
        assert_eq!(snapshot.feed.ingested, threads * each);
        assert_eq!(snapshot.feed.dropped, threads * each);
        assert_eq!(snapshot.slots.ticks, threads * each);
        assert_eq!(snapshot.slots.processing_us.count, threads * each);
        assert_eq!(
            snapshot.execution.in_flight_intents, 0,
            "every intent was aborted"
        );
        assert_eq!(snapshot.execution.unobserved, 0);
    }

    #[test]
    fn a_snapshot_serialises_to_the_shape_the_window_reads() {
        let collector = MetricsCollector::new();
        collector.record_ingested(7);
        let json = serde_json::to_value(collector.snapshot()).expect("plain data serialises");
        assert_eq!(json["feed"]["ingested"], 7);
        assert_eq!(json["source"], "sts.engine");
        assert!(
            json["slots"]["processingUs"]["p95Us"].is_null(),
            "no ticks means no p95"
        );
        assert!(json["execution"]["inFlightIntents"].is_i64());
    }

    // -----------------------------------------------------------------------
    // the exporter
    // -----------------------------------------------------------------------

    #[test]
    fn an_address_may_be_a_bare_port_or_the_whole_thing() {
        assert_eq!(
            parse_addr("9464").expect("a bare port is a port on this machine"),
            SocketAddr::from((Ipv4Addr::LOCALHOST, 9464))
        );
        assert_eq!(
            parse_addr(" 127.0.0.1:9999 ").expect("whitespace is not a typo worth failing over"),
            SocketAddr::from((Ipv4Addr::LOCALHOST, 9999))
        );
        assert!(parse_addr("not-an-address").is_err());
        assert!(
            parse_addr("127.0.0.1").is_err(),
            "an address with no port is not an address"
        );
    }

    #[test]
    fn the_exporter_refuses_to_bind_anywhere_but_this_machine() {
        assert!(ensure_loopback(SocketAddr::from((Ipv4Addr::LOCALHOST, 9464))).is_ok());
        assert!(ensure_loopback("[::1]:9464".parse().expect("a literal address")).is_ok());
        let public = ensure_loopback(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 9464)));
        assert!(matches!(public, Err(ExporterError::NotLoopback(_))));
        let routable = ensure_loopback("192.168.1.10:9464".parse().expect("a literal address"));
        assert!(matches!(routable, Err(ExporterError::NotLoopback(_))));
    }

    #[test]
    fn binding_refuses_a_public_address_before_it_opens_anything() {
        let attempt = BoundExporter::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)));
        assert!(matches!(attempt, Err(ExporterError::NotLoopback(_))));
    }

    #[test]
    fn metrics_come_back_as_json() {
        let collector = MetricsCollector::new();
        collector.record_ingested(3);
        let response = route("GET", "/metrics", "", &collector);
        assert_eq!(response.status, 200);
        assert!(response.content_type.starts_with("application/json"));
        let body: serde_json::Value =
            serde_json::from_str(&response.body).expect("the body is the snapshot");
        assert_eq!(body["feed"]["ingested"], 3);
    }

    #[test]
    fn a_query_string_is_not_part_of_the_path() {
        let collector = MetricsCollector::new();
        assert_eq!(
            route("GET", "/metrics?cachebust=17", "", &collector).status,
            200
        );
        assert_eq!(route("GET", "/metrics/", "", &collector).status, 200);
        assert_eq!(route("GET", "/metrics#anchor", "", &collector).status, 200);
    }

    #[test]
    fn the_exporter_only_reads() {
        let collector = MetricsCollector::new();
        for method in ["POST", "PUT", "DELETE", "PATCH"] {
            let response = route(method, "/metrics", "", &collector);
            assert_eq!(
                response.status, 405,
                "{method} is not something this answers"
            );
            assert_eq!(response.allow, Some("GET, HEAD"));
        }
    }

    #[test]
    fn anything_else_is_a_404() {
        let collector = MetricsCollector::new();
        assert_eq!(route("GET", "/", "", &collector).status, 200);
        assert_eq!(route("GET", "/healthz", "", &collector).status, 200);
        assert_eq!(route("GET", "/../etc/passwd", "", &collector).status, 404);
        assert_eq!(route("GET", "/metrics/extra", "", &collector).status, 404);
    }

    #[test]
    fn a_response_says_how_long_its_body_is() {
        let collector = MetricsCollector::new();
        let response = route("GET", "/healthz", "", &collector);
        let full = String::from_utf8(response.render(true)).expect("ascii headers");
        assert!(full.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(full.contains("Content-Length: 3\r\n"));
        assert!(full.contains("Connection: close\r\n"));
        assert!(full.ends_with("\r\n\r\nok\n"));

        // A HEAD gets the same headers and nothing after them.
        let head = String::from_utf8(response.render(false)).expect("ascii headers");
        assert!(head.contains("Content-Length: 3\r\n"));
        assert!(head.ends_with("\r\n\r\n"));
    }

    #[test]
    fn the_end_of_a_request_head_is_found_either_way_it_is_written() {
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n\r\n"), Some(18));
        assert_eq!(find_head_end(b"GET / HTTP/1.1\n\n"), Some(16));
        assert_eq!(
            find_head_end(b"GET / HTTP/1.1\r\n"),
            None,
            "headers are not over yet"
        );
    }

    #[tokio::test]
    async fn the_exporter_answers_a_real_scrape() {
        let collector = Arc::new(MetricsCollector::new());
        collector.record_ingested(11);
        collector.record_slot_tick(4_242, Duration::from_micros(120));

        // Port zero: the operating system picks, and the test asks it which.
        let bound = BoundExporter::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .expect("loopback with an ephemeral port is always available");
        let addr = bound.addr();
        let exporter = bound.serve(Arc::clone(&collector));
        assert_eq!(exporter.addr(), addr);

        let body = scrape(addr, "GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(body.starts_with("HTTP/1.1 200 OK\r\n"), "{body}");
        let json_start = body.find("{\n").expect("a json body");
        let snapshot: serde_json::Value =
            serde_json::from_str(body[json_start..].trim()).expect("the body parses back");
        assert_eq!(snapshot["feed"]["ingested"], 11);
        assert_eq!(snapshot["slots"]["newestSlot"], 4_242);

        let missing = scrape(addr, "GET /nope HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(
            missing.starts_with("HTTP/1.1 404 Not Found\r\n"),
            "{missing}"
        );

        let refused = scrape(addr, "POST /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(
            refused.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"),
            "{refused}"
        );
        assert!(refused.contains("Allow: GET, HEAD\r\n"));

        let status = exporter.status();
        assert!(status.serving);
        assert_eq!(status.requests, 3);
        assert_eq!(status.rejected, 0);
        assert_eq!(status.refused, 0);

        exporter.stop();
    }

    #[tokio::test]
    async fn a_scrape_never_makes_the_engine_wait() {
        let collector = Arc::new(MetricsCollector::new());
        let bound = BoundExporter::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .expect("loopback with an ephemeral port is always available");
        let addr = bound.addr();
        let exporter = bound.serve(Arc::clone(&collector));

        // A client that opens a connection and says nothing. The engine keeps
        // recording through it; nothing here is allowed to hold a writer up.
        let idle = TcpStream::connect(addr)
            .await
            .expect("the exporter is listening");
        for frame in 0..1_000 {
            collector.record_ingested(1);
            collector.observe_queue(frame % 256, 256);
        }
        assert_eq!(collector.snapshot().feed.ingested, 1_000);

        let body = scrape(addr, "GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(body.starts_with("HTTP/1.1 200 OK\r\n"), "{body}");
        drop(idle);
        exporter.stop();
    }

    // -----------------------------------------------------------------------
    // which format a client gets
    // -----------------------------------------------------------------------

    #[test]
    fn a_prometheus_server_is_recognised_by_what_it_asks_for() {
        // The real header a Prometheus server sends, quality values and all.
        let real = "application/openmetrics-text;version=1.0.0,\
                    application/openmetrics-text;version=0.0.1;q=0.75,\
                    text/plain;version=0.0.4;q=0.5,*/*;q=0.1";
        assert_eq!(negotiate(real), Format::Prometheus);
        assert_eq!(negotiate("text/plain"), Format::Prometheus);
        assert_eq!(negotiate("TEXT/PLAIN; VERSION=0.0.4"), Format::Prometheus);
    }

    #[test]
    fn everybody_else_still_gets_the_json_they_always_got() {
        // Adding a format is not a reason to change what an existing reader
        // receives. `*/*` is what curl sends when nobody said otherwise.
        assert_eq!(negotiate(""), Format::Json);
        assert_eq!(negotiate("*/*"), Format::Json);
        assert_eq!(negotiate("application/json"), Format::Json);
    }

    #[test]
    fn the_suffixed_paths_answer_in_one_format_whatever_was_asked_for() {
        let collector = MetricsCollector::new();

        // A person with a terminal wants to name the format and see it, not
        // negotiate for it.
        let forced_text = route("GET", "/metrics.prom", "application/json", &collector);
        assert_eq!(forced_text.status, 200);
        assert!(forced_text.content_type.starts_with("text/plain"));
        assert!(forced_text.body.starts_with("# HELP sts_exporter_info"));

        let forced_json = route("GET", "/metrics.json", "text/plain", &collector);
        assert_eq!(forced_json.status, 200);
        assert!(forced_json.content_type.starts_with("application/json"));
        serde_json::from_str::<serde_json::Value>(&forced_json.body).expect("still json");
    }

    #[test]
    fn the_negotiated_path_says_which_format_it_answered_in() {
        let collector = MetricsCollector::new();
        let text = route("GET", "/metrics", "text/plain;version=0.0.4", &collector);
        assert_eq!(text.content_type, crate::prometheus::CONTENT_TYPE);
        assert!(
            text.content_type.contains("version=0.0.4"),
            "a parser reads this to pick a mode"
        );
    }

    #[test]
    fn the_index_lists_every_path_that_answers() {
        let collector = MetricsCollector::new();
        let index = route("GET", "/", "", &collector);
        for path in ["/metrics", "/metrics.prom", "/metrics.json", "/healthz"] {
            assert!(index.body.contains(path), "{path} is not discoverable");
        }
        // Everything the index advertises has to actually answer.
        for path in ["/metrics", "/metrics.prom", "/metrics.json", "/healthz"] {
            assert_eq!(route("GET", path, "", &collector).status, 200, "{path}");
        }
    }

    #[test]
    fn a_header_is_found_however_it_was_capitalised() {
        let head = "GET /metrics HTTP/1.1\r\nHost: localhost\r\nACCEPT:  text/plain \r\n\r\n";
        assert_eq!(header_value(head, "accept"), Some("text/plain"));
        assert_eq!(header_value(head, "host"), Some("localhost"));
        assert_eq!(header_value(head, "authorization"), None);
    }

    #[test]
    fn the_request_line_is_not_mistaken_for_a_header() {
        // `GET /metrics HTTP/1.1` splits on a colon into something that looks
        // like a header called `GET /metrics HTTP/1`.
        let head = "GET /metrics HTTP/1.1\r\n\r\n";
        assert_eq!(header_value(head, "GET /metrics HTTP/1"), None);
    }

    #[tokio::test]
    async fn a_real_scraper_gets_the_text_format_over_the_socket() {
        let collector = Arc::new(MetricsCollector::new());
        collector.record_ingested(11);
        collector.record_slot_tick(4_242, Duration::from_micros(90));

        let bound = BoundExporter::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .expect("loopback with an ephemeral port is always available");
        let addr = bound.addr();
        let exporter = bound.serve(Arc::clone(&collector));

        let answer = scrape(
            addr,
            "GET /metrics HTTP/1.1\r\nHost: localhost\r\n\
             Accept: text/plain;version=0.0.4;q=0.5,*/*;q=0.1\r\n\r\n",
        )
        .await;
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
        assert!(
            answer.contains("Content-Type: text/plain; version=0.0.4"),
            "{answer}"
        );
        assert!(
            answer.contains("sts_feed_ingested_frames_total 11"),
            "{answer}"
        );
        assert!(answer.contains("sts_slot_newest 4242"), "{answer}");

        // The same socket, no Accept: the reader that was here first is
        // unaffected by the one that turned up second.
        let json = scrape(addr, "GET /metrics HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        assert!(json.contains("Content-Type: application/json"), "{json}");

        exporter.stop();
    }

    #[tokio::test]
    async fn a_head_on_the_text_format_says_how_long_the_body_would_be() {
        let collector = Arc::new(MetricsCollector::new());
        let bound = BoundExporter::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .expect("loopback with an ephemeral port is always available");
        let addr = bound.addr();
        let exporter = bound.serve(Arc::clone(&collector));

        let answer = scrape(
            addr,
            "HEAD /metrics.prom HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await;
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
        assert!(
            answer.contains("Content-Type: text/plain; version=0.0.4"),
            "{answer}"
        );
        assert!(
            answer.ends_with("\r\n\r\n"),
            "a HEAD gets the headers and nothing after them"
        );
        let length: usize = answer
            .split("Content-Length: ")
            .nth(1)
            .and_then(|rest| rest.split("\r\n").next())
            .expect("a length is always sent")
            .parse()
            .expect("a length is a number");
        assert!(length > 0, "the body it describes is not empty");

        exporter.stop();
    }

    /// One request, one answer, on a fresh connection.
    async fn scrape(addr: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(addr)
            .await
            .expect("the exporter is listening");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("the request goes out");
        let mut body = Vec::new();
        stream
            .read_to_end(&mut body)
            .await
            .expect("the answer comes back");
        String::from_utf8_lossy(&body).into_owned()
    }
}
