//! Deterministic replay, fixture playback, and the pump.fun fill model.
//!
//! This is `docs/architecture/REPLAY_AND_SIMULATION_SPEC.md` in code. Section
//! numbers in the comments below refer to that document, and where this module
//! departs from it the departure says so and says why.
//!
//! Three things live here and they are the three halves of one claim.
//!
//! **A clock that can be mocked.** Nothing in the engine may read the host's
//! clock directly, because every `*_at_ms` column descends from it and those
//! columns are compared byte for byte between two runs. `Clock` is the single
//! seam; `SystemClock` behaves as the code does today and `ReplayClock` walks a
//! timeline the fixture drives (§2).
//!
//! **A fixture that cannot be edited without being noticed.** Records are the
//! exact bytes off the socket, in a fixed field order, hash-chained, and read
//! through a cursor that has no seek — so "the decision could not have seen a
//! later record" is a property of the type rather than of the reviewer's
//! attention (§3, §6, §9).
//!
//! **A fill model that does not flatter itself.** The pump.fun curve is
//! constant-product over virtual reserves with the fee taken outside the curve,
//! so `k` is preserved exactly and every number here is integer arithmetic with
//! a rounding direction chosen against the trader (§11–§15).
//!
//! `ReplaySession` is the fourth thing, and it is the smallest: the fixture,
//! the playhead and the multiplier behind one lock, so `lib.rs` has something
//! to hand the window when it asks what is driving the numbers. It plays a
//! recording into the replay clock and into the cockpit above the panes. It
//! does **not** put fixture frames through ingestion — §5 puts that behind
//! `FixtureDialer`, which is a different seam — so a session that is running is
//! not on its own a reason to believe the panes below the bar stopped being
//! live. `lib.rs` is where that gap is closed, because it is a question about
//! the application rather than about replay.
//!
//! It depends on nothing else in this crate on purpose. The replay path has to
//! be checkable while the rest of the engine is mid-change, and a module that
//! only compiles when ingestion does is a module that gets checked last. The one
//! seam that matters is `CurveState::from_parts`, which takes the same six
//! numbers `ingestion::BondingCurve` decodes; a `From<&BondingCurve>` belongs
//! next to that type once this module is wired into `lib.rs`.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// The schema string on every fixture record. A record without it is not a
/// record this build knows how to read, and guessing at one is how a fixture
/// from a future format gets replayed as if it were this one.
pub const RECORD_SCHEMA: &str = "sts.replay.v1";

/// The schema string on the manifest.
pub const MANIFEST_SCHEMA: &str = "sts.replay.manifest.v1";

// ===========================================================================
// SHA-256, vendored
// ===========================================================================

/// FIPS 180-4 SHA-256.
///
/// Vendored rather than depended on, for the same reason `base58` lives in
/// `types.rs` and `base64` lives in `ingestion.rs`: it is eighty lines, it is
/// fully specified, it has published test vectors, and adding a dependency to
/// `Cargo.toml` is a change to a file three sessions are editing. The vectors in
/// the tests are the ones from the standard, so a mistake here fails loudly
/// rather than producing a chain that is self-consistently wrong.
mod sha256 {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    /// The digest of `bytes`.
    pub fn digest(bytes: &[u8]) -> [u8; 32] {
        let mut h: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];

        // Message padding: a 0x80 byte, zeroes, then the bit length big-endian.
        let bit_len = (bytes.len() as u64).wrapping_mul(8);
        let mut padded = Vec::with_capacity(bytes.len() + 72);
        padded.extend_from_slice(bytes);
        padded.push(0x80);
        while padded.len() % 64 != 56 {
            padded.push(0);
        }
        padded.extend_from_slice(&bit_len.to_be_bytes());

        let mut w = [0u32; 64];
        for block in padded.chunks_exact(64) {
            for (i, word) in block.chunks_exact(4).enumerate() {
                w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }

            let (mut a, mut b, mut c, mut d) = (h[0], h[1], h[2], h[3]);
            let (mut e, mut f, mut g, mut hh) = (h[4], h[5], h[6], h[7]);

            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ ((!e) & g);
                let t1 = hh
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);

                hh = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }

            h[0] = h[0].wrapping_add(a);
            h[1] = h[1].wrapping_add(b);
            h[2] = h[2].wrapping_add(c);
            h[3] = h[3].wrapping_add(d);
            h[4] = h[4].wrapping_add(e);
            h[5] = h[5].wrapping_add(f);
            h[6] = h[6].wrapping_add(g);
            h[7] = h[7].wrapping_add(hh);
        }

        let mut out = [0u8; 32];
        for (i, word) in h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// Lowercase hex of a digest. The form every `*_hash` field in a fixture is in.
pub fn hex(digest: &[u8; 32]) -> String {
    const NIBBLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for &byte in digest {
        out.push(NIBBLE[(byte >> 4) as usize] as char);
        out.push(NIBBLE[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Reads back what `hex` wrote. `None` for anything that is not exactly 64
/// lowercase hex digits — an uppercase or short digest is a hand-edited fixture,
/// which is the thing the chain exists to catch.
pub fn unhex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let bytes = text.as_bytes();
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = nibble(bytes[i * 2])?;
        let lo = nibble(bytes[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

const fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

/// The digest of a byte string, as hex. What `frame_sha256` holds.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&sha256::digest(bytes))
}

// ===========================================================================
// base64, vendored
// ===========================================================================

/// Standard alphabet with padding, encode and decode.
///
/// `ingestion.rs` already vendors a decoder, and it decodes into a caller-owned
/// stack buffer because it runs per frame on the hot path. This one allocates,
/// because it runs once per record on the recording path and once per record on
/// the reading path, and neither is the loop that matters.
mod base64 {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    const DIGIT: [u8; 256] = {
        let mut table = [0xffu8; 256];
        let mut i = 0;
        while i < 64 {
            table[ALPHABET[i] as usize] = i as u8;
            i += 1;
        }
        table
    };

    pub fn encode(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let triple = (b0 << 16) | (b1 << 8) | b2;

            out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
            out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
            if chunk.len() > 1 {
                out.push(ALPHABET[(triple >> 6) as usize & 0x3f] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(ALPHABET[triple as usize & 0x3f] as char);
            } else {
                out.push('=');
            }
        }
        out
    }

    pub fn decode(text: &str) -> Option<Vec<u8>> {
        let text = text.as_bytes();
        let body = match text {
            [rest @ .., b'=', b'='] => rest,
            [rest @ .., b'='] => rest,
            rest => rest,
        };
        // A trailing group of one character encodes nothing.
        if body.len() % 4 == 1 {
            return None;
        }

        let mut out = Vec::with_capacity(body.len() / 4 * 3 + 2);
        let mut accumulator = 0u32;
        let mut bits = 0u32;
        for &c in body {
            let digit = DIGIT[c as usize];
            if digit == 0xff {
                return None;
            }
            accumulator = (accumulator << 6) | digit as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((accumulator >> bits) as u8);
            }
        }
        Some(out)
    }
}

// ===========================================================================
// Errors
// ===========================================================================

/// Everything that can be wrong with a fixture.
///
/// Each carries enough to find the record it is about, because the roadmap's
/// Phase 3 criterion is that a failure names the fixture, the correlation ID and
/// the expected and actual state. A bare `false` satisfies none of that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    /// The line was not JSON, or was not a JSON object.
    NotAnObject { line: usize },
    /// `schema` was absent or was not `sts.replay.v1`.
    WrongSchema { line: usize, found: String },
    /// A required field was absent.
    MissingField { line: usize, field: &'static str },
    /// A field was present and unusable.
    BadField {
        line: usize,
        field: &'static str,
        detail: String,
    },
    /// `frame_sha256` or `frame_len` disagrees with `frame_b64`.
    FrameMismatch { seq: u64, detail: String },
    /// A chain link does not verify. The first one that does not is reported;
    /// everything after it is unverifiable rather than wrong.
    ChainBroken {
        seq: u64,
        expected: String,
        found: String,
    },
    /// Records are not in the order §6 requires.
    OutOfOrder {
        seq: u64,
        previous: OrderKey,
        found: OrderKey,
    },
    /// `seq` skipped a value. A fixture with a hole is not a shorter fixture.
    SeqGap { expected: u64, found: u64 },
    /// The manifest says the recording was stopped by an error.
    Incomplete { stream_id: String },
    /// A stream with no records at all.
    Empty,
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReplayError::NotAnObject { line } => {
                write!(f, "line {line} is not a JSON object")
            }
            ReplayError::WrongSchema { line, found } => {
                write!(
                    f,
                    "line {line} has schema {found:?}, expected {RECORD_SCHEMA:?}"
                )
            }
            ReplayError::MissingField { line, field } => {
                write!(f, "line {line} is missing {field}")
            }
            ReplayError::BadField {
                line,
                field,
                detail,
            } => write!(f, "line {line} has an unusable {field}: {detail}"),
            ReplayError::FrameMismatch { seq, detail } => {
                write!(f, "record {seq} frame does not match its digest: {detail}")
            }
            ReplayError::ChainBroken {
                seq,
                expected,
                found,
            } => write!(
                f,
                "record {seq} breaks the chain: expected {expected}, found {found}"
            ),
            ReplayError::OutOfOrder {
                seq,
                previous,
                found,
            } => write!(
                f,
                "record {seq} is out of order: {found:?} does not follow {previous:?}"
            ),
            ReplayError::SeqGap { expected, found } => {
                write!(
                    f,
                    "seq jumped from {} to {found}",
                    expected.saturating_sub(1)
                )
            }
            ReplayError::Incomplete { stream_id } => write!(
                f,
                "fixture {stream_id} is marked incomplete and may not be used in a gate run"
            ),
            ReplayError::Empty => f.write_str("the stream has no records"),
        }
    }
}

impl std::error::Error for ReplayError {}

// ===========================================================================
// §2 — the clocks
// ===========================================================================

/// A point on whichever timeline the `Clock` behind it is running.
///
/// Microseconds, unsigned, and deliberately not `std::time::Instant`: an
/// `Instant` can only be built from the host's monotonic clock, which is the
/// thing being replaced. Durations are taken with `saturating_duration_since`
/// because a virtual timeline that has been clamped (below) can hand back two
/// instants in the wrong order, and a panic on the replay path is worse than a
/// zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClockInstant {
    micros: u64,
}

impl ClockInstant {
    pub const fn from_micros(micros: u64) -> Self {
        Self { micros }
    }

    pub const fn as_micros(self) -> u64 {
        self.micros
    }

    pub const fn saturating_duration_since(self, earlier: Self) -> Duration {
        Duration::from_micros(self.micros.saturating_sub(earlier.micros))
    }
}

/// The engine's only source of time.
///
/// `now_ms` is what every `*_at_ms` column is stamped with, `instant` is what
/// durations are measured against, and `slot` is the newest slot any provider
/// has reported. All three have to be virtualised together: mocking the wall
/// clock and leaving the timers real is the usual reason a replay is nearly
/// deterministic.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
    fn instant(&self) -> ClockInstant;
    fn slot(&self) -> u64;
}

/// The live clock. Behaves exactly as the code does today.
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
    slot: AtomicU64,
}

impl SystemClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            slot: AtomicU64::new(0),
        }
    }

    /// Records the newest slot seen on any feed. Monotonic: a provider that is
    /// behind cannot walk the engine's idea of the chain backwards.
    pub fn observe_slot(&self, slot: u64) {
        self.slot.fetch_max(slot, Ordering::SeqCst);
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    fn instant(&self) -> ClockInstant {
        ClockInstant::from_micros(self.origin.elapsed().as_micros() as u64)
    }

    fn slot(&self) -> u64 {
        self.slot.load(Ordering::SeqCst)
    }
}

/// What one call to `ReplayClock::advance_to` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockAdvance {
    /// Where the clock ended up. Never earlier than where it was.
    pub at_ms: i64,
    pub slot: u64,
    /// The record's timestamp was behind the clock and was clamped away.
    pub clamped: bool,
    /// The record's slot was behind the clock's. Counted, not corrected.
    pub slot_regressed: bool,
}

/// The replay clock: a timeline the fixture drives.
///
/// `advance_to` is called once per record, before the record is delivered, so
/// everything the engine does while handling a record sees one consistent
/// instant. Both axes are monotonic under `fetch_max` — §2's rule is that wall
/// time never regresses, because the fixture is ordered by slot and two
/// providers' block times can disagree by hundreds of milliseconds, and an
/// unordered walk through that disagreement is a walk through nothing.
///
/// The clamp is counted rather than hidden. A fixture where it fires often was
/// recorded against a provider with a broken clock, and that belongs in the
/// manifest where somebody can see it.
#[derive(Debug)]
pub struct ReplayClock {
    at_ms: AtomicI64,
    slot: AtomicU64,
    clamped: AtomicU64,
    slot_regressions: AtomicU64,
    advances: AtomicU64,
}

impl ReplayClock {
    /// A clock at the epoch, slot zero.
    pub fn new() -> Self {
        Self::start_at(0, 0)
    }

    pub fn start_at(at_ms: i64, slot: u64) -> Self {
        Self {
            at_ms: AtomicI64::new(at_ms),
            slot: AtomicU64::new(slot),
            clamped: AtomicU64::new(0),
            slot_regressions: AtomicU64::new(0),
            advances: AtomicU64::new(0),
        }
    }

    /// Moves the clock to a record's position, clamping backwards motion away.
    pub fn advance_to(&self, slot: u64, at_ms: i64) -> ClockAdvance {
        let previous_ms = self.at_ms.fetch_max(at_ms, Ordering::SeqCst);
        let previous_slot = self.slot.fetch_max(slot, Ordering::SeqCst);
        self.advances.fetch_add(1, Ordering::SeqCst);

        let clamped = previous_ms > at_ms;
        if clamped {
            self.clamped.fetch_add(1, Ordering::SeqCst);
        }
        let slot_regressed = previous_slot > slot;
        if slot_regressed {
            self.slot_regressions.fetch_add(1, Ordering::SeqCst);
        }

        ClockAdvance {
            at_ms: previous_ms.max(at_ms),
            slot: previous_slot.max(slot),
            clamped,
            slot_regressed,
        }
    }

    /// How many records had a timestamp behind the clock.
    pub fn clamped(&self) -> u64 {
        self.clamped.load(Ordering::SeqCst)
    }

    /// How many records had a slot behind the clock.
    pub fn slot_regressions(&self) -> u64 {
        self.slot_regressions.load(Ordering::SeqCst)
    }

    /// How many times the clock has been moved at all.
    pub fn advances(&self) -> u64 {
        self.advances.load(Ordering::SeqCst)
    }
}

impl Default for ReplayClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for ReplayClock {
    fn now_ms(&self) -> i64 {
        self.at_ms.load(Ordering::SeqCst)
    }

    /// The same virtual timeline as `now_ms`, in microseconds, so a duration
    /// measured across two records is the difference between their recorded
    /// timestamps rather than the time the host took to process them.
    ///
    /// Negative wall times — a fixture from before 1970, which is a decode bug
    /// rather than a recording — saturate to zero rather than wrapping.
    fn instant(&self) -> ClockInstant {
        let ms = self.at_ms.load(Ordering::SeqCst);
        ClockInstant::from_micros((ms.max(0) as u64).saturating_mul(1_000))
    }

    fn slot(&self) -> u64 {
        self.slot.load(Ordering::SeqCst)
    }
}

// ===========================================================================
// §3 — the fixture record
// ===========================================================================

/// What kind of thing happened on the socket.
///
/// `Frame` carries bytes; the rest are the connection lifecycle, and they are
/// recorded because replaying frames without them leaves every endpoint in
/// whatever health band it started in, and the health band feeds the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordKind {
    /// A data frame, exactly as it arrived.
    Frame,
    /// A pong, carrying the round trip the health bands are computed from.
    Pong,
    /// A dial succeeded.
    Connected,
    /// The socket closed.
    Closed,
    /// The socket errored.
    Error,
    /// A subscription acknowledgement.
    Ack,
}

impl RecordKind {
    pub const ALL: [RecordKind; 6] = [
        RecordKind::Frame,
        RecordKind::Pong,
        RecordKind::Connected,
        RecordKind::Closed,
        RecordKind::Error,
        RecordKind::Ack,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            RecordKind::Frame => "frame",
            RecordKind::Pong => "pong",
            RecordKind::Connected => "connected",
            RecordKind::Closed => "closed",
            RecordKind::Error => "error",
            RecordKind::Ack => "ack",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        RecordKind::ALL.into_iter().find(|k| k.as_str() == text)
    }

    /// Whether a record of this kind carries frame bytes.
    pub const fn carries_frame(self) -> bool {
        matches!(self, RecordKind::Frame)
    }
}

/// Why the live engine threw a frame away, mirroring `ingestion::DropReason`.
///
/// The vocabulary is copied rather than imported so this module stays free of
/// the ingestion layer; `parse` and `as_str` use the same strings
/// `DropReason::as_str` produces, so a recorder can write one and a reader can
/// read the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DropClass {
    NotAllowlisted,
    NotANotification,
    Undecodable,
    NoDecoder,
    TooSmall,
    LotterySlot,
    StaleSlot,
    PoolTooThin,
    Graduated,
}

impl DropClass {
    pub const ALL: [DropClass; 9] = [
        DropClass::NotAllowlisted,
        DropClass::NotANotification,
        DropClass::Undecodable,
        DropClass::NoDecoder,
        DropClass::TooSmall,
        DropClass::LotterySlot,
        DropClass::StaleSlot,
        DropClass::PoolTooThin,
        DropClass::Graduated,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            DropClass::NotAllowlisted => "not_allowlisted",
            DropClass::NotANotification => "not_a_notification",
            DropClass::Undecodable => "undecodable",
            DropClass::NoDecoder => "no_decoder",
            DropClass::TooSmall => "too_small",
            DropClass::LotterySlot => "lottery_slot",
            DropClass::StaleSlot => "stale_slot",
            DropClass::PoolTooThin => "pool_too_thin",
            DropClass::Graduated => "graduated",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        DropClass::ALL.into_iter().find(|d| d.as_str() == text)
    }
}

/// Which bounded channel dropped a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Queue {
    FastPath,
    Standard,
    Wal,
}

impl Queue {
    pub const ALL: [Queue; 3] = [Queue::FastPath, Queue::Standard, Queue::Wal];

    pub const fn as_str(self) -> &'static str {
        match self {
            Queue::FastPath => "fast_path",
            Queue::Standard => "standard",
            Queue::Wal => "wal",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Queue::ALL.into_iter().find(|q| q.as_str() == text)
    }
}

/// What the live engine did with a record.
///
/// **This extends the spec's §3 table, deliberately.** The document says
/// `outcome` is `accepted` or `dropped:<DropReason>`, and §5.1 says
/// "`DropReason` already distinguishes them" of backpressure drops. It does
/// not: `ingestion::DropReason` has nine variants and none of them is
/// backpressure — a full channel is counted in `IngestionMetrics` as
/// `dropped_fast_path`, `dropped_standard` or `dropped_wal` and never reaches a
/// `DropReason` at all. Since the whole fidelity rule in §5.1 turns on telling a
/// backpressure drop from a filtering drop, the vocabulary needs a third form,
/// and `backpressure:<queue>` is it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordOutcome {
    /// It went down a channel to the engine.
    Accepted,
    /// The filters rejected it.
    Dropped(DropClass),
    /// A bounded channel was full. The one class of disagreement replay is
    /// allowed to have with the recording.
    Backpressure(Queue),
}

impl RecordOutcome {
    pub fn encode(self) -> String {
        match self {
            RecordOutcome::Accepted => "accepted".to_string(),
            RecordOutcome::Dropped(reason) => format!("dropped:{}", reason.as_str()),
            RecordOutcome::Backpressure(queue) => format!("backpressure:{}", queue.as_str()),
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        if text == "accepted" {
            return Some(RecordOutcome::Accepted);
        }
        if let Some(rest) = text.strip_prefix("dropped:") {
            return DropClass::parse(rest).map(RecordOutcome::Dropped);
        }
        if let Some(rest) = text.strip_prefix("backpressure:") {
            return Queue::parse(rest).map(RecordOutcome::Backpressure);
        }
        None
    }

    pub const fn is_backpressure(self) -> bool {
        matches!(self, RecordOutcome::Backpressure(_))
    }

    pub const fn is_accepted(self) -> bool {
        matches!(self, RecordOutcome::Accepted)
    }
}

/// One line of a fixture.
///
/// The field order in the struct is the field order in the canonical bytes and
/// in the emitted JSON, and it is the order §3's table gives. That is not a
/// coincidence to be preserved by hand: `canonical_bytes` writes them out
/// explicitly rather than deriving `Serialize`, because serde's ordering is an
/// implementation detail and a hash chain built on an implementation detail is
/// a hash chain that breaks on a dependency upgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayRecord {
    /// UUIDv7 in the recorder; any stable unique string here.
    pub event_id: String,
    /// Position in the stream, from zero, no gaps.
    pub seq: u64,
    pub slot: u64,
    pub observed_at_ms: i64,
    /// `helius`, `quicknode`, `triton`.
    pub provider: String,
    pub endpoint_index: u16,
    /// Which dial this belongs to. Increments on every reconnect.
    pub connection: u32,
    pub kind: RecordKind,
    /// The exact bytes off the socket. `None` for every non-frame kind.
    pub frame: Option<Vec<u8>>,
    pub outcome: RecordOutcome,
    /// What the live run measured, in microseconds. Replay stamps this rather
    /// than measuring, per §7.1 — a host measurement inside the compared
    /// artefact would make every run differ for reasons that are not the
    /// engine's.
    pub dispatch_latency_us: Option<u32>,
    pub prev_hash: [u8; 32],
    pub integrity_hash: [u8; 32],
}

/// The chain's starting value: `SHA-256(stream_id)`.
pub fn genesis_hash(stream_id: &str) -> [u8; 32] {
    sha256::digest(stream_id.as_bytes())
}

/// Escapes a string into a JSON string body, per RFC 8259.
///
/// Only the escapes the standard requires, and `\u00XX` for the other control
/// characters — never `\u` for anything printable. Two encoders that disagree
/// about optional escaping produce two different digests for the same record.
fn json_escape(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

impl ReplayRecord {
    /// The base64 of the frame, or `None`.
    pub fn frame_b64(&self) -> Option<String> {
        self.frame.as_deref().map(base64::encode)
    }

    /// The frame's length in bytes before encoding.
    pub fn frame_len(&self) -> u64 {
        self.frame.as_ref().map(|f| f.len() as u64).unwrap_or(0)
    }

    /// The frame's digest, as hex. The digest of the empty string for a record
    /// with no frame — a fixed value rather than a null, so the field is always
    /// present and always the same width.
    pub fn frame_sha256(&self) -> String {
        sha256_hex(self.frame.as_deref().unwrap_or(&[]))
    }

    /// The bytes the chain is computed over: every field in §3's order except
    /// `integrity_hash`, as compact JSON with no whitespace.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = String::with_capacity(512);
        out.push('{');

        out.push_str("\"schema\":");
        json_escape(RECORD_SCHEMA, &mut out);

        out.push_str(",\"event_id\":");
        json_escape(&self.event_id, &mut out);

        out.push_str(",\"seq\":");
        out.push_str(&self.seq.to_string());

        out.push_str(",\"slot\":");
        out.push_str(&self.slot.to_string());

        out.push_str(",\"observed_at_ms\":");
        out.push_str(&self.observed_at_ms.to_string());

        out.push_str(",\"provider\":");
        json_escape(&self.provider, &mut out);

        out.push_str(",\"endpoint_index\":");
        out.push_str(&self.endpoint_index.to_string());

        out.push_str(",\"connection\":");
        out.push_str(&self.connection.to_string());

        out.push_str(",\"kind\":");
        json_escape(self.kind.as_str(), &mut out);

        out.push_str(",\"frame_b64\":");
        match self.frame_b64() {
            Some(encoded) => json_escape(&encoded, &mut out),
            None => out.push_str("null"),
        }

        out.push_str(",\"frame_len\":");
        out.push_str(&self.frame_len().to_string());

        out.push_str(",\"frame_sha256\":");
        json_escape(&self.frame_sha256(), &mut out);

        out.push_str(",\"outcome\":");
        json_escape(&self.outcome.encode(), &mut out);

        out.push_str(",\"dispatch_latency_us\":");
        match self.dispatch_latency_us {
            Some(us) => out.push_str(&us.to_string()),
            None => out.push_str("null"),
        }

        out.push_str(",\"prev_hash\":");
        json_escape(&hex(&self.prev_hash), &mut out);

        out.push('}');
        out.into_bytes()
    }

    /// `SHA-256(prev_hash_bytes || canonical(record))`, per §3.1.
    ///
    /// `prev_hash` appears twice — once as raw bytes in front and once inside
    /// the canonical form — which is what the specification says. It is
    /// redundant and it is cheap, and matching the written contract exactly
    /// matters more here than saving thirty-two bytes of hashing.
    pub fn compute_integrity(&self, prev: &[u8; 32]) -> [u8; 32] {
        let canonical = self.canonical_bytes();
        let mut buffer = Vec::with_capacity(32 + canonical.len());
        buffer.extend_from_slice(prev);
        buffer.extend_from_slice(&canonical);
        sha256::digest(&buffer)
    }

    /// Whether this record's own hash is the one its contents imply.
    pub fn verify_integrity(&self) -> bool {
        self.compute_integrity(&self.prev_hash) == self.integrity_hash
    }

    /// The ordering key from §6.
    pub fn order_key(&self) -> OrderKey {
        OrderKey {
            slot: self.slot,
            provider_rank: provider_rank(&self.provider),
            endpoint_index: self.endpoint_index,
            connection: self.connection,
            seq: self.seq,
        }
    }

    /// One line of JSONL: the canonical bytes with `integrity_hash` appended.
    pub fn to_line(&self) -> String {
        let mut line = String::from_utf8(self.canonical_bytes())
            .expect("canonical bytes are built from UTF-8 pieces");
        // Replace the closing brace with the last field.
        line.pop();
        line.push_str(",\"integrity_hash\":");
        json_escape(&hex(&self.integrity_hash), &mut line);
        line.push('}');
        line
    }
}

// ===========================================================================
// §6 — the total order
// ===========================================================================

/// The key every fixture record is ordered by.
///
/// Derived `Ord` compares the fields in declaration order, which is exactly the
/// precedence §6 gives: slot first because it is the chain's ordering and
/// arrival is the network's, then provider rank, then endpoint, then connection,
/// then sequence. `seq` is unique within a stream, so no two records can tie on
/// the whole key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct OrderKey {
    pub slot: u64,
    pub provider_rank: u16,
    pub endpoint_index: u16,
    pub connection: u32,
    pub seq: u64,
}

/// The provider's index in `ingestion::FeedProvider::ALL`.
///
/// A fixed array in the source, not a hash-map iteration and not a
/// configuration order, so two machines rank the same providers the same way.
/// An unrecognised provider sorts last rather than panicking — a fixture from a
/// build that knew about a fourth provider is still readable, and the endpoint,
/// connection and sequence components keep the order total.
pub fn provider_rank(provider: &str) -> u16 {
    match provider {
        "helius" => 0,
        "quicknode" => 1,
        "triton" => 2,
        _ => u16::MAX,
    }
}

// ===========================================================================
// §3 — recording: sealing records into a chain
// ===========================================================================

/// Everything about a record except its position in the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordDraft {
    pub event_id: String,
    pub slot: u64,
    pub observed_at_ms: i64,
    pub provider: String,
    pub endpoint_index: u16,
    pub connection: u32,
    pub kind: RecordKind,
    pub frame: Option<Vec<u8>>,
    pub outcome: RecordOutcome,
    pub dispatch_latency_us: Option<u32>,
}

/// Assigns sequence numbers and chain links, in order.
///
/// The recorder completes records out of order — `outcome` and
/// `dispatch_latency_us` are only known after the dispatcher has finished with a
/// frame — so drafts are buffered by arrival and sealed here in stream order.
/// Sealing is the point at which a record becomes evidence.
#[derive(Debug, Clone)]
pub struct ChainWriter {
    prev: [u8; 32],
    next_seq: u64,
    sealed: u64,
}

impl ChainWriter {
    pub fn new(stream_id: &str) -> Self {
        Self {
            prev: genesis_hash(stream_id),
            next_seq: 0,
            sealed: 0,
        }
    }

    pub fn seal(&mut self, draft: RecordDraft) -> ReplayRecord {
        let mut record = ReplayRecord {
            event_id: draft.event_id,
            seq: self.next_seq,
            slot: draft.slot,
            observed_at_ms: draft.observed_at_ms,
            provider: draft.provider,
            endpoint_index: draft.endpoint_index,
            connection: draft.connection,
            kind: draft.kind,
            frame: draft.frame,
            outcome: draft.outcome,
            dispatch_latency_us: draft.dispatch_latency_us,
            prev_hash: self.prev,
            integrity_hash: [0u8; 32],
        };
        record.integrity_hash = record.compute_integrity(&self.prev);

        self.prev = record.integrity_hash;
        self.next_seq += 1;
        self.sealed += 1;
        record
    }

    /// The chain head, which is what the manifest records.
    pub fn head(&self) -> [u8; 32] {
        self.prev
    }

    pub fn sealed(&self) -> u64 {
        self.sealed
    }
}

// ===========================================================================
// §3 — reading: JSONL
// ===========================================================================

fn field<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    line: usize,
    name: &'static str,
) -> Result<&'a serde_json::Value, ReplayError> {
    object
        .get(name)
        .ok_or(ReplayError::MissingField { line, field: name })
}

fn bad(line: usize, name: &'static str, detail: impl Into<String>) -> ReplayError {
    ReplayError::BadField {
        line,
        field: name,
        detail: detail.into(),
    }
}

/// Reads one JSONL line into a record.
///
/// Every failure names the line and the field. The `frame_len` and
/// `frame_sha256` fields are checked against the decoded bytes rather than
/// trusted: they are redundant by construction, and a redundant field that is
/// never checked is a field that silently drifts.
pub fn from_line(text: &str, line: usize) -> Result<ReplayRecord, ReplayError> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| ReplayError::NotAnObject { line })?;
    let object = value.as_object().ok_or(ReplayError::NotAnObject { line })?;

    let schema = field(object, line, "schema")?
        .as_str()
        .ok_or_else(|| bad(line, "schema", "not a string"))?;
    if schema != RECORD_SCHEMA {
        return Err(ReplayError::WrongSchema {
            line,
            found: schema.to_string(),
        });
    }

    let event_id = field(object, line, "event_id")?
        .as_str()
        .ok_or_else(|| bad(line, "event_id", "not a string"))?
        .to_string();

    let seq = field(object, line, "seq")?
        .as_u64()
        .ok_or_else(|| bad(line, "seq", "not an unsigned integer"))?;

    let slot = field(object, line, "slot")?
        .as_u64()
        .ok_or_else(|| bad(line, "slot", "not an unsigned integer"))?;

    let observed_at_ms = field(object, line, "observed_at_ms")?
        .as_i64()
        .ok_or_else(|| bad(line, "observed_at_ms", "not an integer"))?;

    let provider = field(object, line, "provider")?
        .as_str()
        .ok_or_else(|| bad(line, "provider", "not a string"))?
        .to_string();

    let endpoint_index = field(object, line, "endpoint_index")?
        .as_u64()
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| bad(line, "endpoint_index", "not a u16"))?;

    let connection = field(object, line, "connection")?
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| bad(line, "connection", "not a u32"))?;

    let kind_text = field(object, line, "kind")?
        .as_str()
        .ok_or_else(|| bad(line, "kind", "not a string"))?;
    let kind = RecordKind::parse(kind_text)
        .ok_or_else(|| bad(line, "kind", format!("unknown: {kind_text}")))?;

    let frame_value = field(object, line, "frame_b64")?;
    let frame = if frame_value.is_null() {
        None
    } else {
        let encoded = frame_value
            .as_str()
            .ok_or_else(|| bad(line, "frame_b64", "not a string"))?;
        Some(base64::decode(encoded).ok_or_else(|| bad(line, "frame_b64", "not base64"))?)
    };

    let outcome_text = field(object, line, "outcome")?
        .as_str()
        .ok_or_else(|| bad(line, "outcome", "not a string"))?;
    let outcome = RecordOutcome::parse(outcome_text)
        .ok_or_else(|| bad(line, "outcome", format!("unknown: {outcome_text}")))?;

    let latency_value = field(object, line, "dispatch_latency_us")?;
    let dispatch_latency_us = if latency_value.is_null() {
        None
    } else {
        Some(
            latency_value
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| bad(line, "dispatch_latency_us", "not a u32"))?,
        )
    };

    let prev_hash = unhex(
        field(object, line, "prev_hash")?
            .as_str()
            .ok_or_else(|| bad(line, "prev_hash", "not a string"))?,
    )
    .ok_or_else(|| bad(line, "prev_hash", "not 64 lowercase hex digits"))?;

    let integrity_hash = unhex(
        field(object, line, "integrity_hash")?
            .as_str()
            .ok_or_else(|| bad(line, "integrity_hash", "not a string"))?,
    )
    .ok_or_else(|| bad(line, "integrity_hash", "not 64 lowercase hex digits"))?;

    let record = ReplayRecord {
        event_id,
        seq,
        slot,
        observed_at_ms,
        provider,
        endpoint_index,
        connection,
        kind,
        frame,
        outcome,
        dispatch_latency_us,
        prev_hash,
        integrity_hash,
    };

    // The redundant frame fields are checked, not trusted.
    let declared_len = field(object, line, "frame_len")?
        .as_u64()
        .ok_or_else(|| bad(line, "frame_len", "not an unsigned integer"))?;
    if declared_len != record.frame_len() {
        return Err(ReplayError::FrameMismatch {
            seq,
            detail: format!(
                "frame_len says {declared_len}, the bytes are {}",
                record.frame_len()
            ),
        });
    }
    let declared_digest = field(object, line, "frame_sha256")?
        .as_str()
        .ok_or_else(|| bad(line, "frame_sha256", "not a string"))?;
    if declared_digest != record.frame_sha256() {
        return Err(ReplayError::FrameMismatch {
            seq,
            detail: "frame_sha256 does not match the frame".to_string(),
        });
    }

    Ok(record)
}

/// Reads a whole segment. Blank lines are skipped; everything else must parse.
pub fn parse_stream(text: &str) -> Result<Vec<ReplayRecord>, ReplayError> {
    let mut records = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        records.push(from_line(line, index + 1)?);
    }
    Ok(records)
}

/// Writes a whole segment.
pub fn write_stream(records: &[ReplayRecord]) -> String {
    let mut out = String::new();
    for record in records {
        out.push_str(&record.to_line());
        out.push('\n');
    }
    out
}

// ===========================================================================
// §3.2 — the manifest
// ===========================================================================

/// One JSONL segment of a stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub file: String,
    pub records: u64,
    pub sha256: String,
}

/// An interval in which no socket was connected.
///
/// The field that stops a cohort being computed across a hole. A backtest over a
/// day with a two-hour outage that does not say so is a survivorship claim
/// wearing a sample size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageGap {
    pub from_ms: i64,
    pub to_ms: i64,
    pub gap_reason: String,
}

/// Why `manifest.json` would not read.
///
/// One shape, because there is only one: a file that was named and a sentence
/// saying what was wrong with it. Whoever is reporting it wraps it in their own
/// error type rather than restating the path, which is how the same refusal
/// reads the same whether it ended a backtest run or refused a replay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestError {
    pub path: String,
    pub detail: String,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.detail)
    }
}

impl std::error::Error for ManifestError {}

/// What a fixture directory says about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: String,
    pub stream_id: String,
    pub created_at_ms: i64,
    pub first_slot: u64,
    pub last_slot: u64,
    pub record_count: u64,
    pub frame_count: u64,
    pub segments: Vec<Segment>,
    pub chain_head: String,
    pub providers: Vec<String>,
    pub filters_version: u32,
    pub exclusion_list_version: u32,
    pub sts_version: String,
    pub git_commit: String,
    #[serde(default)]
    pub coverage: Vec<CoverageGap>,
    /// False when the recording was stopped by an error. §4's rule is that a
    /// dropped fixture record ends the recording rather than being counted, so
    /// this is the flag that says a hole exists.
    pub complete: bool,
}

impl Manifest {
    /// Builds a manifest for a stream that was recorded in one piece.
    pub fn for_records(
        stream_id: &str,
        records: &[ReplayRecord],
        chain_head: [u8; 32],
        created_at_ms: i64,
    ) -> Self {
        let mut providers: Vec<String> = Vec::new();
        for record in records {
            if !providers.iter().any(|p| p == &record.provider) {
                providers.push(record.provider.clone());
            }
        }
        providers.sort_by_key(|p| (provider_rank(p), p.clone()));

        Manifest {
            schema: MANIFEST_SCHEMA.to_string(),
            stream_id: stream_id.to_string(),
            created_at_ms,
            first_slot: records.first().map(|r| r.slot).unwrap_or(0),
            last_slot: records.last().map(|r| r.slot).unwrap_or(0),
            record_count: records.len() as u64,
            frame_count: records.iter().filter(|r| r.kind.carries_frame()).count() as u64,
            segments: Vec::new(),
            chain_head: hex(&chain_head),
            providers,
            filters_version: 1,
            exclusion_list_version: 0,
            sts_version: env!("CARGO_PKG_VERSION").to_string(),
            git_commit: String::new(),
            coverage: Vec::new(),
            complete: true,
        }
    }

    /// Whether this fixture may back a gate dossier.
    ///
    /// §3.2: an incomplete recording may be replayed for debugging and may never
    /// be used in a gate run. Refusing is the whole mechanism — a warning next
    /// to a number is a warning nobody reads next to a number everybody quotes.
    pub fn gate_ready(&self) -> Result<(), ReplayError> {
        if !self.complete {
            return Err(ReplayError::Incomplete {
                stream_id: self.stream_id.clone(),
            });
        }
        Ok(())
    }

    /// Reads `manifest.json` out of a fixture directory, if it has one.
    ///
    /// A directory without one is a loose collection of independent fixtures and
    /// is read as such; a directory with one is a single rotated stream, and
    /// §3.3's rule that the chain runs across the roll only holds if every
    /// segment is fed under the same stream id. Guessing the stream id from a
    /// file stem gets a rotated fixture wrong in the direction that looks like
    /// tampering.
    ///
    /// A manifest that will not parse is a refusal rather than a shrug: the file
    /// is there and it is the thing that says what the streams are.
    pub fn read_dir(dir: &Path) -> Result<Option<Manifest>, ManifestError> {
        let path = dir.join("manifest.json");
        if !path.exists() {
            return Ok(None);
        }
        let refuse = |detail: String| ManifestError {
            path: path.display().to_string(),
            detail,
        };
        let text = fs::read_to_string(&path).map_err(|err| refuse(err.to_string()))?;
        let manifest: Manifest =
            serde_json::from_str(&text).map_err(|err| refuse(err.to_string()))?;
        if manifest.schema != MANIFEST_SCHEMA {
            return Err(refuse(format!(
                "schema is {:?}, expected {MANIFEST_SCHEMA:?}",
                manifest.schema
            )));
        }
        Ok(Some(manifest))
    }

    /// Whether `window` intersects any recorded outage, so a statistic computed
    /// over it can be labelled rather than quietly averaged.
    pub fn covers(&self, from_ms: i64, to_ms: i64) -> bool {
        !self
            .coverage
            .iter()
            .any(|gap| gap.from_ms < to_ms && from_ms < gap.to_ms)
    }
}

// ===========================================================================
// §9 — the forward-only cursor
// ===========================================================================

/// Reads a verified stream, forwards, once.
///
/// **There is deliberately no `seek`, no index, and no random access.** §9's
/// leakage rule is that a decision made at slot *s* can only have seen records
/// the cursor has already yielded, and the way to enforce that is for no method
/// to exist that returns anything else. A comment saying "do not read ahead" is
/// enforcement by attention; a missing method is enforcement.
///
/// `open` verifies three things before it hands anything back, and refuses
/// rather than repairing. A fixture is evidence: sorting a mis-ordered stream
/// into order would hide the recorder bug that produced it.
#[derive(Debug, Clone)]
pub struct ReplayCursor {
    records: Vec<ReplayRecord>,
    next: usize,
    stream_id: String,
}

impl ReplayCursor {
    /// Verifies sequence density, the §6 total order, and the whole hash chain.
    pub fn open(stream_id: &str, records: Vec<ReplayRecord>) -> Result<Self, ReplayError> {
        if records.is_empty() {
            return Err(ReplayError::Empty);
        }

        let mut expected_prev = genesis_hash(stream_id);
        let mut previous_key: Option<OrderKey> = None;

        for (index, record) in records.iter().enumerate() {
            let expected_seq = index as u64;
            if record.seq != expected_seq {
                return Err(ReplayError::SeqGap {
                    expected: expected_seq,
                    found: record.seq,
                });
            }

            if record.prev_hash != expected_prev {
                return Err(ReplayError::ChainBroken {
                    seq: record.seq,
                    expected: hex(&expected_prev),
                    found: hex(&record.prev_hash),
                });
            }

            let computed = record.compute_integrity(&record.prev_hash);
            if computed != record.integrity_hash {
                return Err(ReplayError::ChainBroken {
                    seq: record.seq,
                    expected: hex(&computed),
                    found: hex(&record.integrity_hash),
                });
            }

            let key = record.order_key();
            if let Some(previous) = previous_key {
                if key <= previous {
                    return Err(ReplayError::OutOfOrder {
                        seq: record.seq,
                        previous,
                        found: key,
                    });
                }
            }

            previous_key = Some(key);
            expected_prev = record.integrity_hash;
        }

        Ok(Self {
            records,
            next: 0,
            stream_id: stream_id.to_string(),
        })
    }

    /// Parses and opens a whole segment in one step.
    pub fn from_text(stream_id: &str, text: &str) -> Result<Self, ReplayError> {
        ReplayCursor::open(stream_id, parse_stream(text)?)
    }

    /// The next record, or `None` at the end of the stream.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<&ReplayRecord> {
        let record = self.records.get(self.next)?;
        self.next += 1;
        Some(record)
    }

    /// How many records have been yielded.
    pub fn position(&self) -> usize {
        self.next
    }

    pub fn remaining(&self) -> usize {
        self.records.len() - self.next
    }

    pub fn is_exhausted(&self) -> bool {
        self.next >= self.records.len()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn stream_id(&self) -> &str {
        &self.stream_id
    }

    /// The chain head after the last record, which is what the manifest carries.
    pub fn chain_head(&self) -> [u8; 32] {
        self.records
            .last()
            .map(|r| r.integrity_hash)
            .unwrap_or_else(|| genesis_hash(&self.stream_id))
    }

    /// Rewinds to the start. Not a seek: it is the only motion other than
    /// forward, it goes to one fixed place, and it exists so a run can be
    /// repeated from a cursor that has already been verified. It cannot be used
    /// to look ahead, which is the property §9 is about.
    pub fn restart(&mut self) {
        self.next = 0;
    }
}

/// Drives a `ReplayClock` from a cursor, one record at a time.
///
/// §2's rule that nothing reads the clock twice for one event is enforced here:
/// the clock is advanced before the record is handed over, and the handler is
/// given the advance it produced rather than being expected to ask again.
///
/// §5.1's delivery discipline is the other half. This yields record *n+1* only
/// after the caller has finished with record *n*, because it is a `&mut self`
/// borrow — the queues are never in a racing state, so a bounded-channel drop
/// becomes a deterministic function of the consumer rather than of the
/// scheduler.
#[derive(Debug)]
pub struct ReplayDriver {
    cursor: ReplayCursor,
    clock: ReplayClock,
}

impl ReplayDriver {
    pub fn new(cursor: ReplayCursor) -> Self {
        let start = ReplayClock::new();
        Self {
            cursor,
            clock: start,
        }
    }

    pub fn clock(&self) -> &ReplayClock {
        &self.clock
    }

    pub fn cursor(&self) -> &ReplayCursor {
        &self.cursor
    }

    /// Advances the clock to the next record's position and returns both.
    pub fn step(&mut self) -> Option<(ClockAdvance, &ReplayRecord)> {
        let record = self.cursor.next()?;
        let advance = self.clock.advance_to(record.slot, record.observed_at_ms);
        Some((advance, record))
    }

    /// Puts the driver back to the start of the fixture with a clock at the
    /// epoch.
    ///
    /// The clock is replaced rather than rewound, because a second run of one
    /// fixture has to produce the same numbers as the first and a clock that
    /// kept its clamp counters would not. It is the cursor's `restart` and not
    /// a seek: there is still exactly one place to go back to.
    pub fn restart(&mut self) {
        self.cursor.restart();
        self.clock = ReplayClock::new();
    }
}

// ===========================================================================
// §5.1 — fidelity
// ===========================================================================

/// One record where replay and the recording disagreed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FidelityMismatch {
    pub event_id: String,
    pub seq: u64,
    pub recorded: RecordOutcome,
    pub replayed: RecordOutcome,
}

/// Whether the replay did what the live run did.
///
/// Separate from equivalence, which is whether two replays agree with each
/// other. Serialised delivery means replay drops fewer frames than live did, so
/// the expected disagreement is exactly one shape: live dropped for
/// backpressure, replay accepted. That is tolerated and counted.
///
/// Every other disagreement fails. A frame live rejected as `not_allowlisted`
/// and replay accepted is a filtering bug, and letting it hide inside a
/// backpressure total is how a filtering bug survives a gate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FidelityReport {
    pub compared: u64,
    pub agreed: u64,
    pub tolerated: Vec<FidelityMismatch>,
    pub failures: Vec<FidelityMismatch>,
}

impl FidelityReport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&mut self, record: &ReplayRecord, replayed: RecordOutcome) {
        self.compared += 1;
        if record.outcome == replayed {
            self.agreed += 1;
            return;
        }

        let mismatch = FidelityMismatch {
            event_id: record.event_id.clone(),
            seq: record.seq,
            recorded: record.outcome,
            replayed,
        };

        // The one tolerated shape: live could not keep up, replay could.
        if record.outcome.is_backpressure() && replayed.is_accepted() {
            self.tolerated.push(mismatch);
        } else {
            self.failures.push(mismatch);
        }
    }

    pub fn passes(&self) -> bool {
        self.failures.is_empty()
    }
}

// ===========================================================================
// §19 — addressed draws
// ===========================================================================

/// Every random number the simulator uses, addressed rather than sequenced.
///
/// A draw is a function of `(run_seed, correlation_id, label, index)` and of
/// nothing else. The alternative — one generator advanced in order — makes every
/// draw depend on the order and the count of every draw before it, so adding one
/// sampled quantity anywhere, or logging one that used to be computed lazily,
/// shifts every number after it. That produces a simulator whose output changes
/// when the code is refactored, which makes the replay-equivalence gate
/// impossible to satisfy for real reasons and trivial to satisfy by accident.
///
/// **The inputs are length-prefixed, which the specification does not say.**
/// Plain concatenation makes `("ab", "c")` and `("a", "bc")` the same preimage,
/// so two different draws would return the same number. A four-byte
/// little-endian length in front of each string removes that, and the cost is
/// eight bytes of hashing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrawSource {
    seed: [u8; 32],
}

impl DrawSource {
    /// Seeds from the run's seed string — `--seed 0x100x` on the command line.
    pub fn new(run_seed: &str) -> Self {
        Self {
            seed: sha256::digest(run_seed.as_bytes()),
        }
    }

    pub const fn from_seed_bytes(seed: [u8; 32]) -> Self {
        Self { seed }
    }

    pub const fn seed(&self) -> &[u8; 32] {
        &self.seed
    }

    /// The raw 64 bits behind one draw.
    pub fn raw(&self, correlation_id: &str, label: &str, index: u64) -> u64 {
        let correlation = correlation_id.as_bytes();
        let label_bytes = label.as_bytes();

        let mut buffer = Vec::with_capacity(32 + 8 + correlation.len() + label_bytes.len() + 8);
        buffer.extend_from_slice(&self.seed);
        buffer.extend_from_slice(&(correlation.len() as u32).to_le_bytes());
        buffer.extend_from_slice(correlation);
        buffer.extend_from_slice(&(label_bytes.len() as u32).to_le_bytes());
        buffer.extend_from_slice(label_bytes);
        buffer.extend_from_slice(&index.to_le_bytes());

        let digest = sha256::digest(&buffer);
        u64::from_le_bytes([
            digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
        ])
    }

    /// A uniform draw in `[0, 1)`.
    pub fn unit(&self, correlation_id: &str, label: &str, index: u64) -> f64 {
        // 2^-64 exactly, so the result is in [0, 1) and never rounds to 1.0.
        self.raw(correlation_id, label, index) as f64 * (1.0 / 18_446_744_073_709_551_616.0)
    }

    /// A uniform draw in `[0, n)`, by the multiply-shift §19 describes. Unbiased
    /// to within one part in 2^64, which is closer than any distribution this
    /// simulator has been fitted to.
    pub fn below(&self, correlation_id: &str, label: &str, index: u64, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let raw = self.raw(correlation_id, label, index) as u128;
        ((raw * n as u128) >> 64) as u64
    }

    /// Picks a bucket from weights in basis points.
    ///
    /// Weights that do not sum to 10 000 are used as they are and the remainder
    /// falls to the last bucket, because silently normalising a set of
    /// probabilities that does not add up hides the bug that produced it.
    pub fn bucket(
        &self,
        correlation_id: &str,
        label: &str,
        index: u64,
        weights_bps: &[u16],
    ) -> usize {
        if weights_bps.is_empty() {
            return 0;
        }
        let draw = self.below(correlation_id, label, index, u64::from(BPS_DENOMINATOR));
        let mut cumulative = 0u64;
        for (position, &weight) in weights_bps.iter().enumerate() {
            cumulative += u64::from(weight);
            if draw < cumulative {
                return position;
            }
        }
        weights_bps.len() - 1
    }
}

// ===========================================================================
// §11 — the pump.fun bonding curve
// ===========================================================================

/// One basis point is 1/10 000.
pub const BPS_DENOMINATOR: u32 = 10_000;

pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

/// The total fee on the SOL leg of one swap, in basis points.
///
/// Policy and versioned, not a literal: pump.fun has charged a protocol fee and
/// a creator fee separately, and the sum is what matters to a fill. The default
/// is the one every number in the specification's tables was computed at.
pub const DEFAULT_FEE_BPS: u16 = 100;

/// The 1.5% executable-liquidity participation cap from doctrine.
///
/// Settled rather than split: [`crate::types::MAX_POOL_SHARE_BPS`] is the one
/// statement of it, and `ingestion::StreamFilters`, which used to hold 500 in
/// the same field, now reads the same constant. This name stays because the
/// simulator's public surface is written in terms of it.
pub const DEFAULT_MAX_POOL_SHARE_BPS: u16 = crate::types::MAX_POOL_SHARE_BPS;

/// Where the curve completes, in lamports of real SOL. Mirrors
/// `ingestion::PUMP_GRADUATION_LAMPORTS`.
pub const PUMP_GRADUATION_LAMPORTS: u64 = 85 * LAMPORTS_PER_SOL;

/// Launch reserves. Protocol parameters that have changed before and can again.
pub const LAUNCH_VIRTUAL_TOKEN_RESERVES: u64 = 1_073_000_000_000_000;
pub const LAUNCH_VIRTUAL_SOL_RESERVES: u64 = 30 * LAMPORTS_PER_SOL;
pub const LAUNCH_REAL_TOKEN_RESERVES: u64 = 793_100_000_000_000;
pub const TOKEN_TOTAL_SUPPLY: u64 = 1_000_000_000_000_000;

/// The reserves at one instant, in the same six numbers
/// `ingestion::BondingCurve` decodes off the account.
///
/// Virtual and real are both here and they answer different questions.
/// **Virtual reserves set the price. Real reserves set what is executable.** A
/// generic constant-product model that carries one pair gets the second question
/// wrong, and the second question is the one an exit depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurveState {
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub token_total_supply: u64,
    pub complete: bool,
}

/// Why a quote could not be produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteError {
    /// The curve has graduated. §17: a quote from a complete curve is a quote
    /// from a dead pool, and this is a hard branch rather than a continuous
    /// transition.
    CurveComplete,
    /// The reserves do not hold together well enough to price against.
    Implausible,
    /// A zero-sized order is not an order.
    ZeroSize,
    /// The curve cannot pay out that much real SOL. The first of §17's
    /// no-executable-exit conditions.
    ExceedsRealSol { required: u64, available: u64 },
    /// The curve does not hold that many real tokens to sell.
    ExceedsRealTokens { required: u64, available: u64 },
    /// No size reaches the target within the curve's asymptote.
    Unreachable,
}

impl fmt::Display for QuoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QuoteError::CurveComplete => f.write_str("the curve has graduated"),
            QuoteError::Implausible => f.write_str("the reserves are not plausible"),
            QuoteError::ZeroSize => f.write_str("the order has no size"),
            QuoteError::ExceedsRealSol {
                required,
                available,
            } => write!(
                f,
                "needs {required} lamports of real SOL, the curve holds {available}"
            ),
            QuoteError::ExceedsRealTokens {
                required,
                available,
            } => write!(
                f,
                "needs {required} real token base units, the curve holds {available}"
            ),
            QuoteError::Unreachable => f.write_str("no size on this curve reaches that target"),
        }
    }
}

impl std::error::Error for QuoteError {}

/// One side of one swap.
///
/// `gross` and `net` are both on the SOL leg and the fee is the difference. For
/// a buy, `gross` is what the trader pays and `net` is what enters the curve;
/// for a sell, `gross` is what leaves the curve and `net` is what the trader
/// receives. `tokens` is the other leg in base units either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fill {
    pub gross_lamports: u64,
    pub fee_lamports: u64,
    pub net_lamports: u64,
    pub tokens: u64,
    pub slippage_bps: u16,
}

/// Slippage in basis points for a swap of relative size `w = w_num / w_den`.
///
/// ```text
/// S = (w + φ) / (1 + w) = (w_num × 10_000 + φ_bps × w_den) / (w_den + w_num)
/// ```
///
/// **Rounds up.** `RISK_AND_SYBIL_SPEC.md` rounds concentration to nearest
/// because truncation biases toward "looks safe"; the same reasoning points a
/// different way here. A simulator that under-reports its own slippage flatters
/// every backtest built on it, so the residual goes to the trader's cost.
///
/// Total for every input. An overflow — only reachable from sizes far outside
/// anything a `u64` of lamports can hold — returns 100%, which is the same
/// pessimistic direction as the rounding.
pub fn slippage_bps(w_num: u128, w_den: u128, fee_bps: u16) -> u16 {
    if w_den == 0 {
        return BPS_DENOMINATOR as u16;
    }
    let scaled = match w_num.checked_mul(u128::from(BPS_DENOMINATOR)) {
        Some(value) => value,
        None => return BPS_DENOMINATOR as u16,
    };
    let fee_term = match w_den.checked_mul(u128::from(fee_bps)) {
        Some(value) => value,
        None => return BPS_DENOMINATOR as u16,
    };
    let numerator = match scaled.checked_add(fee_term) {
        Some(value) => value,
        None => return BPS_DENOMINATOR as u16,
    };
    let denominator = match w_den.checked_add(w_num) {
        Some(value) => value,
        None => return BPS_DENOMINATOR as u16,
    };
    let bps = numerator.div_ceil(denominator);
    bps.min(u128::from(BPS_DENOMINATOR)) as u16
}

/// Ceiling division for signed basis-point ratios.
///
/// Rounds toward positive infinity in both directions, so a cost is never
/// understated and a benefit is never overstated.
fn ceil_div_i128(numerator: i128, denominator: i128) -> i128 {
    debug_assert!(denominator > 0);
    let quotient = numerator.div_euclid(denominator);
    if numerator.rem_euclid(denominator) > 0 {
        quotient + 1
    } else {
        quotient
    }
}

impl CurveState {
    /// The reserves a pump.fun curve is created with.
    pub const LAUNCH: CurveState = CurveState {
        virtual_token_reserves: LAUNCH_VIRTUAL_TOKEN_RESERVES,
        virtual_sol_reserves: LAUNCH_VIRTUAL_SOL_RESERVES,
        real_token_reserves: LAUNCH_REAL_TOKEN_RESERVES,
        real_sol_reserves: 0,
        token_total_supply: TOKEN_TOTAL_SUPPLY,
        complete: false,
    };

    /// The same six numbers `ingestion::BondingCurve` decodes.
    ///
    /// This is the seam. A `From<&BondingCurve>` belongs beside that type once
    /// this module is declared in `lib.rs`; keeping the constructor here means
    /// the replay path can be built and tested while the ingestion layer is
    /// mid-change.
    pub const fn from_parts(
        virtual_token_reserves: u64,
        virtual_sol_reserves: u64,
        real_token_reserves: u64,
        real_sol_reserves: u64,
        token_total_supply: u64,
        complete: bool,
    ) -> Self {
        Self {
            virtual_token_reserves,
            virtual_sol_reserves,
            real_token_reserves,
            real_sol_reserves,
            token_total_supply,
            complete,
        }
    }

    /// The state after `real_sol_lamports` of real SOL has entered a curve that
    /// started at `LAUNCH`.
    ///
    /// Derived from the invariant rather than tracked, which is what makes the
    /// graduation identity in the tests a check on the model instead of a
    /// restatement of its inputs.
    pub fn at_real_sol(real_sol_lamports: u64) -> Self {
        let k = u128::from(LAUNCH_VIRTUAL_TOKEN_RESERVES) * u128::from(LAUNCH_VIRTUAL_SOL_RESERVES);
        let y = u128::from(LAUNCH_VIRTUAL_SOL_RESERVES) + u128::from(real_sol_lamports);
        let x = k / y;

        // Tokens sold is x0 - x, so what is left of the real reserve is
        // rt0 - (x0 - x) = x - (x0 - rt0).
        let floor = u128::from(LAUNCH_VIRTUAL_TOKEN_RESERVES - LAUNCH_REAL_TOKEN_RESERVES);
        let real_tokens = x.saturating_sub(floor);

        CurveState {
            virtual_token_reserves: x.min(u128::from(u64::MAX)) as u64,
            virtual_sol_reserves: y.min(u128::from(u64::MAX)) as u64,
            real_token_reserves: real_tokens.min(u128::from(u64::MAX)) as u64,
            real_sol_reserves: real_sol_lamports,
            token_total_supply: TOKEN_TOTAL_SUPPLY,
            complete: real_sol_lamports >= PUMP_GRADUATION_LAMPORTS,
        }
    }

    /// The constant product. Preserved exactly across every swap, because the
    /// fee is taken outside the curve rather than out of the reserve.
    pub const fn k(&self) -> u128 {
        self.virtual_token_reserves as u128 * self.virtual_sol_reserves as u128
    }

    /// Whether the numbers hold together well enough to price against. Mirrors
    /// `BondingCurve::is_plausible`.
    pub const fn is_plausible(&self) -> bool {
        self.virtual_token_reserves > 0
            && self.virtual_sol_reserves > 0
            && self.token_total_supply > 0
    }

    /// What the whole supply is worth at the curve's current price, in lamports.
    pub const fn market_cap_lamports(&self) -> u64 {
        if self.virtual_token_reserves == 0 {
            return 0;
        }
        let cap = self.token_total_supply as u128 * self.virtual_sol_reserves as u128
            / self.virtual_token_reserves as u128;
        if cap > u64::MAX as u128 {
            u64::MAX
        } else {
            cap as u64
        }
    }

    /// How far along the curve is, in basis points. Mirrors
    /// `BondingCurve::progress_bps`.
    pub const fn progress_bps(&self) -> u16 {
        if self.complete {
            return BPS_DENOMINATOR as u16;
        }
        let bps = self.real_sol_reserves as u128 * BPS_DENOMINATOR as u128
            / PUMP_GRADUATION_LAMPORTS as u128;
        if bps > BPS_DENOMINATOR as u128 {
            BPS_DENOMINATOR as u16
        } else {
            bps as u16
        }
    }

    /// The largest position this curve can take under the participation cap.
    ///
    /// Taken against `real_sol_reserves`, which is the executable liquidity —
    /// not market cap, which is a virtual-reserve quantity roughly five times
    /// larger, and not `virtual_sol_reserves`, which includes 30 SOL that does
    /// not exist. Using either would make every position several times too big
    /// while appearing to respect the same rule.
    ///
    /// Saturating rather than truncating. A share above `BPS_DENOMINATOR` is
    /// not a participation cap and `daemon::cli` refuses one, but this is a
    /// public method on a public type and the reserves come off an account
    /// somebody else writes — so the one input pair that would wrap a `u64` is
    /// answered with the largest position there is rather than with a small
    /// number that looks like a cap doing its job.
    pub const fn max_position_lamports(&self, max_pool_share_bps: u16) -> u64 {
        let room =
            self.real_sol_reserves as u128 * max_pool_share_bps as u128 / BPS_DENOMINATOR as u128;
        if room > u64::MAX as u128 {
            u64::MAX
        } else {
            room as u64
        }
    }

    fn guard(&self) -> Result<(), QuoteError> {
        if self.complete {
            return Err(QuoteError::CurveComplete);
        }
        if !self.is_plausible() {
            return Err(QuoteError::Implausible);
        }
        Ok(())
    }

    /// Prices a buy of `gross_lamports`.
    ///
    /// **KNOWN DISCREPANCY WITH THE REAL PROGRAM, ABOUT 1 BASIS POINT.** The fee
    /// is taken *out of* `gross_lamports` here — spend X, and `X - X*fee` reaches
    /// the curve. pump.fun charges it *on top*: you name the amount that reaches
    /// the curve and the fee is added, so spending X from the wallet puts
    /// `X / (1 + fee)` in. At 100 bps that is 0.990099·X against this model's
    /// 0.99·X, so this buys about one basis point fewer tokens than the real
    /// program would for the same wallet debit.
    ///
    /// Left alone rather than corrected, and the reason is worth writing down:
    /// the error is 1 bp against a round trip that costs 199, it is in the
    /// conservative direction — the model under-reports what a buy gets — and
    /// the correct form (`net = gross * 10_000 / (10_000 + fee_bps)`) moves the
    /// round-trip identity from 199 bps to 198 and perturbs every token count
    /// pinned in the suite. That is a change worth making deliberately, with the
    /// pinned numbers re-derived in the same commit, not folded into a merge.
    pub fn quote_buy(&self, gross_lamports: u64, fee_bps: u16) -> Result<Fill, QuoteError> {
        self.guard()?;
        if gross_lamports == 0 {
            return Err(QuoteError::ZeroSize);
        }

        let gross = u128::from(gross_lamports);
        let fee = gross * u128::from(fee_bps) / u128::from(BPS_DENOMINATOR);
        let net = gross - fee;

        let x = u128::from(self.virtual_token_reserves);
        let y = u128::from(self.virtual_sol_reserves);
        let tokens = x * net / (y + net);

        if tokens > u128::from(self.real_token_reserves) {
            return Err(QuoteError::ExceedsRealTokens {
                required: tokens.min(u128::from(u64::MAX)) as u64,
                available: self.real_token_reserves,
            });
        }

        Ok(Fill {
            gross_lamports,
            fee_lamports: fee as u64,
            net_lamports: net as u64,
            tokens: tokens as u64,
            slippage_bps: slippage_bps(net, y, fee_bps),
        })
    }

    /// Prices a sell of `tokens` base units.
    pub fn quote_sell(&self, tokens: u64, fee_bps: u16) -> Result<Fill, QuoteError> {
        self.guard()?;
        if tokens == 0 {
            return Err(QuoteError::ZeroSize);
        }

        let dx = u128::from(tokens);
        let x = u128::from(self.virtual_token_reserves);
        let y = u128::from(self.virtual_sol_reserves);
        let gross = y * dx / (x + dx);

        if gross > u128::from(self.real_sol_reserves) {
            return Err(QuoteError::ExceedsRealSol {
                required: gross.min(u128::from(u64::MAX)) as u64,
                available: self.real_sol_reserves,
            });
        }

        let fee = gross * u128::from(fee_bps) / u128::from(BPS_DENOMINATOR);
        let net = gross - fee;

        Ok(Fill {
            gross_lamports: gross as u64,
            fee_lamports: fee as u64,
            net_lamports: net as u64,
            tokens,
            slippage_bps: slippage_bps(dx, x, fee_bps),
        })
    }

    /// The state after a buy has executed.
    pub fn after_buy(&self, fill: &Fill) -> CurveState {
        CurveState {
            virtual_token_reserves: self.virtual_token_reserves.saturating_sub(fill.tokens),
            virtual_sol_reserves: self.virtual_sol_reserves.saturating_add(fill.net_lamports),
            real_token_reserves: self.real_token_reserves.saturating_sub(fill.tokens),
            real_sol_reserves: self.real_sol_reserves.saturating_add(fill.net_lamports),
            token_total_supply: self.token_total_supply,
            complete: self.real_sol_reserves.saturating_add(fill.net_lamports)
                >= PUMP_GRADUATION_LAMPORTS,
        }
    }

    /// The state after a sell has executed.
    ///
    /// The curve loses the whole `gross`; the trader receives `net` and the
    /// difference is the fee, which leaves the pool either way.
    pub fn after_sell(&self, fill: &Fill) -> CurveState {
        CurveState {
            virtual_token_reserves: self.virtual_token_reserves.saturating_add(fill.tokens),
            virtual_sol_reserves: self
                .virtual_sol_reserves
                .saturating_sub(fill.gross_lamports),
            real_token_reserves: self.real_token_reserves.saturating_add(fill.tokens),
            real_sol_reserves: self.real_sol_reserves.saturating_sub(fill.gross_lamports),
            token_total_supply: self.token_total_supply,
            complete: self.complete,
        }
    }

    /// The smallest token input whose net output reaches `net_target`.
    ///
    /// Bisection rather than a closed form, which is what an exit sizer actually
    /// has to do: the curve inverts in closed form only before the fee floor is
    /// applied, and the floor is what decides whether the last lamport arrives.
    /// `gross` is monotone in `tokens`, so the search is exact and terminates.
    pub fn sell_tokens_for_target(
        &self,
        net_target: u64,
        fee_bps: u16,
    ) -> Result<(u64, Fill), QuoteError> {
        self.guard()?;
        if net_target == 0 {
            return Err(QuoteError::ZeroSize);
        }
        if net_target > self.real_sol_reserves {
            return Err(QuoteError::ExceedsRealSol {
                required: net_target,
                available: self.real_sol_reserves,
            });
        }

        let net_for = |tokens: u64| -> u128 {
            let dx = u128::from(tokens);
            let x = u128::from(self.virtual_token_reserves);
            let y = u128::from(self.virtual_sol_reserves);
            let gross = y * dx / (x + dx);
            gross - gross * u128::from(fee_bps) / u128::from(BPS_DENOMINATOR)
        };

        // Find a bracket by doubling. The asymptote is `y`, so a target the
        // curve cannot reach at any size fails here rather than spinning.
        let target = u128::from(net_target);
        let mut high: u64 = 1;
        loop {
            if net_for(high) >= target {
                break;
            }
            match high.checked_mul(2) {
                Some(next) => high = next,
                None => return Err(QuoteError::Unreachable),
            }
        }

        let mut low: u64 = 1;
        while low < high {
            let mid = low + (high - low) / 2;
            if net_for(mid) >= target {
                high = mid;
            } else {
                low = mid + 1;
            }
        }

        let fill = self.quote_sell(low, fee_bps)?;
        Ok((low, fill))
    }

    /// The state after `net_flow_lamports` of other people's fee-adjusted flow.
    ///
    /// Positive is net buying, negative is net selling. §14.2's `δ` is this
    /// divided by the virtual SOL reserve.
    pub fn displaced(&self, net_flow_lamports: i64) -> Option<CurveState> {
        let k = self.k();
        let y = if net_flow_lamports >= 0 {
            u128::from(self.virtual_sol_reserves).checked_add(net_flow_lamports as u128)?
        } else {
            let magnitude = net_flow_lamports.unsigned_abs() as u128;
            if magnitude > u128::from(self.real_sol_reserves) {
                return None;
            }
            u128::from(self.virtual_sol_reserves).checked_sub(magnitude)?
        };
        if y == 0 {
            return None;
        }
        let x = k / y;

        let real_sol = if net_flow_lamports >= 0 {
            self.real_sol_reserves
                .saturating_add(net_flow_lamports as u64)
        } else {
            self.real_sol_reserves
                .saturating_sub(net_flow_lamports.unsigned_abs())
        };
        let sold = u128::from(self.virtual_token_reserves).abs_diff(x);
        let real_tokens = if net_flow_lamports >= 0 {
            u128::from(self.real_token_reserves).saturating_sub(sold)
        } else {
            u128::from(self.real_token_reserves).saturating_add(sold)
        };

        Some(CurveState {
            virtual_token_reserves: x.min(u128::from(u64::MAX)) as u64,
            virtual_sol_reserves: y.min(u128::from(u64::MAX)) as u64,
            real_token_reserves: real_tokens.min(u128::from(u64::MAX)) as u64,
            real_sol_reserves: real_sol,
            token_total_supply: self.token_total_supply,
            complete: real_sol >= PUMP_GRADUATION_LAMPORTS,
        })
    }
}

/// What a buy-then-sell of the same tokens costs, with nobody trading between
/// the legs, in basis points of the SOL put in.
///
/// §12.5: it is `2φ` at every point on the curve, because a constant-product
/// curve with no intervening flow returns to the same `k` and the impact
/// cancels. This function exists so that stays a measurement rather than a
/// claim.
pub fn round_trip_bps(
    state: &CurveState,
    gross_lamports: u64,
    fee_bps: u16,
) -> Result<u16, QuoteError> {
    let buy = state.quote_buy(gross_lamports, fee_bps)?;
    let after = state.after_buy(&buy);
    let sell = after.quote_sell(buy.tokens, fee_bps)?;

    let spent = u128::from(gross_lamports);
    let returned = u128::from(sell.net_lamports);
    let lost = spent.saturating_sub(returned);
    Ok((lost * u128::from(BPS_DENOMINATOR))
        .div_ceil(spent)
        .min(u128::from(BPS_DENOMINATOR)) as u16)
}

/// What being displaced by `net_flow_lamports` costs a buy of `gross_lamports`,
/// in basis points of the tokens that would have been received.
///
/// Positive when the displacement was against the order. Computed by simulating
/// both worlds rather than from the closed form, so the number is exactly what
/// the simulator would fill at, floors and all.
pub fn displacement_damage_bps(
    state: &CurveState,
    net_flow_lamports: i64,
    gross_lamports: u64,
    fee_bps: u16,
) -> Result<i32, QuoteError> {
    let solo = state.quote_buy(gross_lamports, fee_bps)?.tokens;
    if solo == 0 {
        return Err(QuoteError::ZeroSize);
    }
    let displaced = state
        .displaced(net_flow_lamports)
        .ok_or(QuoteError::Implausible)?
        .quote_buy(gross_lamports, fee_bps)?
        .tokens;

    let difference = i128::from(solo) - i128::from(displaced);
    let bps = ceil_div_i128(difference * i128::from(BPS_DENOMINATOR), i128::from(solo));
    Ok(bps.clamp(i128::from(i32::MIN), i128::from(i32::MAX)) as i32)
}

// ===========================================================================
// §15 — sandwich extraction
// ===========================================================================

/// What one sandwich did to both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sandwich {
    /// Tokens the attacker bought and then sold.
    pub attacker_tokens: u64,
    /// What the attacker's sell returned, net of their fee.
    pub attacker_out_lamports: u64,
    /// Net of the attacker's own gross input and their landing cost.
    pub attacker_profit_lamports: i64,
    /// Gross out minus what entered the curve: the `E` of §15.1, before the
    /// attacker's own fees.
    pub extraction_lamports: i64,
    /// What the victim received with the sandwich around them.
    pub victim_tokens: u64,
    /// What the victim would have received alone.
    pub victim_tokens_solo: u64,
    /// How much worse the victim did, in basis points, rounded up.
    pub victim_damage_bps: u16,
}

/// Runs the three swaps in order and reports both sides.
///
/// `cost_lamports` is the attacker's fixed landing cost — signatures, priority
/// fee, tip. It is charged against profit and not against extraction, because
/// extraction is a property of the curve and the cost is a property of the block
/// market.
pub fn simulate_sandwich(
    state: &CurveState,
    attacker_gross: u64,
    victim_gross: u64,
    fee_bps: u16,
    cost_lamports: u64,
) -> Result<Sandwich, QuoteError> {
    let front = state.quote_buy(attacker_gross, fee_bps)?;
    let after_front = state.after_buy(&front);

    let victim = after_front.quote_buy(victim_gross, fee_bps)?;
    let after_victim = after_front.after_buy(&victim);

    let back = after_victim.quote_sell(front.tokens, fee_bps)?;

    let victim_solo = state.quote_buy(victim_gross, fee_bps)?.tokens;
    let damage_bps = if victim_solo == 0 {
        0
    } else {
        let lost = i128::from(victim_solo) - i128::from(victim.tokens);
        ceil_div_i128(lost * i128::from(BPS_DENOMINATOR), i128::from(victim_solo))
            .clamp(0, i128::from(BPS_DENOMINATOR)) as u16
    };

    Ok(Sandwich {
        attacker_tokens: front.tokens,
        attacker_out_lamports: back.net_lamports,
        attacker_profit_lamports: i64::try_from(back.net_lamports).unwrap_or(i64::MAX)
            - i64::try_from(attacker_gross).unwrap_or(i64::MAX)
            - i64::try_from(cost_lamports).unwrap_or(i64::MAX),
        extraction_lamports: i64::try_from(back.gross_lamports).unwrap_or(i64::MAX)
            - i64::try_from(front.net_lamports).unwrap_or(i64::MAX),
        victim_tokens: victim.tokens,
        victim_tokens_solo: victim_solo,
        victim_damage_bps: damage_bps,
    })
}

/// The closed form for gross extraction, in lamports.
///
/// ```text
/// E = A·B·(2Y + A + B) / ((Y + A)² + A·B)
/// ```
///
/// with `Y` the virtual SOL reserve and `A`, `B` the attacker's and victim's
/// fee-adjusted inputs. This is §15.1's `E(α, β)` with the `α = A/Y`,
/// `β = B/Y` substitutions carried out, which turns it into integer arithmetic
/// with no rational intermediate.
///
/// `None` on overflow, which needs inputs far outside anything a `u64` of
/// lamports can carry.
pub fn sandwich_extraction_closed(
    virtual_sol_reserves: u64,
    attacker_net: u64,
    victim_net: u64,
) -> Option<u64> {
    let y = u128::from(virtual_sol_reserves);
    let a = u128::from(attacker_net);
    let b = u128::from(victim_net);

    let span = y.checked_mul(2)?.checked_add(a)?.checked_add(b)?;
    let numerator = a.checked_mul(b)?.checked_mul(span)?;
    let denominator = y
        .checked_add(a)?
        .checked_mul(y.checked_add(a)?)?
        .checked_add(a.checked_mul(b)?)?;
    if denominator == 0 {
        return None;
    }
    Some((numerator / denominator).min(u128::from(u64::MAX)) as u64)
}

/// The smallest front-run worth modelling, in lamports.
///
/// Two signatures at the network's 5 000-lamport base fee. Below this a
/// front-run cannot pay for its own transactions, so it is not a trade; and at
/// around this size the integer floors in the three swaps are worth more than
/// the edge, so a search that includes such sizes returns one-lamport "profits"
/// that are arithmetic residue rather than extraction. Excluding them is what
/// makes the break-even result in section 15.2 testable on integers.
pub const MIN_VIABLE_ATTACKER_LAMPORTS: u64 = 10_000;

/// The smallest victim buy a sandwich can clear fees on, in gross lamports.
///
/// ```text
/// b* = φ·y / (1 - φ)²    =    fee_bps × y × 10_000 / (10_000 - fee_bps)²
/// ```
///
/// §15.2 derives it from the sign of the profit derivative at a front-run of
/// zero. Below this no front-run of any size is profitable, before any landing
/// cost at all. Rounded up, so "strictly above the threshold" is what the tests
/// can assert.
pub fn sandwich_breakeven_victim_lamports(virtual_sol_reserves: u64, fee_bps: u16) -> u64 {
    if fee_bps == 0 || fee_bps >= BPS_DENOMINATOR as u16 {
        return 0;
    }
    let y = u128::from(virtual_sol_reserves);
    let fee = u128::from(fee_bps);
    let remainder = u128::from(BPS_DENOMINATOR) - fee;
    let numerator = fee * y * u128::from(BPS_DENOMINATOR);
    let denominator = remainder * remainder;
    numerator.div_ceil(denominator).min(u128::from(u64::MAX)) as u64
}

/// The most profitable front-run on a geometric grid, or `None` if none of the
/// sizes tried clears the cost.
///
/// A grid rather than an optimiser, and that is the point: an optimiser's answer
/// depends on its convergence path, and a convergence path is a thing that
/// changes when a compiler chooses differently. `steps` points spaced
/// geometrically between one lamport and `max_attacker` is reproducible on any
/// machine, and it is the shape the specification's tables were produced with.
///
/// Sizes below `MIN_VIABLE_ATTACKER_LAMPORTS` are skipped; see that constant for
/// why a dust front-run is arithmetic rather than a trade.
pub fn best_front_run(
    state: &CurveState,
    victim_gross: u64,
    fee_bps: u16,
    cost_lamports: u64,
    max_attacker: u64,
    steps: u32,
) -> Option<(u64, Sandwich)> {
    if max_attacker == 0 || steps == 0 {
        return None;
    }

    let mut best: Option<(u64, Sandwich)> = None;
    let ceiling = max_attacker as f64;
    for step in 0..=steps {
        let fraction = f64::from(step) / f64::from(steps);
        let size = ceiling.powf(fraction).round() as u64;
        let size = size.clamp(1, max_attacker);
        if size < MIN_VIABLE_ATTACKER_LAMPORTS {
            continue;
        }

        let Ok(sandwich) = simulate_sandwich(state, size, victim_gross, fee_bps, cost_lamports)
        else {
            continue;
        };
        if sandwich.attacker_profit_lamports <= 0 {
            continue;
        }
        match best {
            Some((_, ref current))
                if current.attacker_profit_lamports >= sandwich.attacker_profit_lamports => {}
            _ => best = Some((size, sandwich)),
        }
    }
    best
}

// ===========================================================================
// §18 — the cost stack
// ===========================================================================

/// The protocol's share of `φ`, in basis points.
///
/// Policy and versioned, exactly as [`DEFAULT_FEE_BPS`] is. pump.fun charges one
/// proportional fee on the SOL leg and then splits it: the creator of the mint
/// takes [`DEFAULT_CREATOR_FEE_BPS`] and the protocol takes the rest. A fill
/// sees only the sum — which is why the curve quotes against `φ` and nothing
/// here changes a single quoted lamport — but a cost report that cannot say
/// which of the two took a lamport cannot answer the one question the split
/// exists for: whether the venue's cut moves when the creator's does.
///
/// **Ninety-five and five is a starting point, not a measurement.** §18 of the
/// replay specification records the sum and does not record the split, and the
/// programme has changed both before. It is written down here so it can be
/// argued with against a recording of real swaps rather than carried around as
/// an assumption, and the one thing that does not depend on getting it right is
/// the run's PnL: moving the line moves a lamport from one column of a report
/// to the other and moves nothing else, which
/// `attribution::tests::moving_the_split_moves_no_lamport_of_pnl` holds to.
pub const DEFAULT_PROTOCOL_FEE_BPS: u16 = 95;

/// The creator's share of `φ`, in basis points. See
/// [`DEFAULT_PROTOCOL_FEE_BPS`] for what this number is and is not.
pub const DEFAULT_CREATOR_FEE_BPS: u16 = 5;

/// What one signature costs, in lamports, before anything is prioritised.
///
/// §18's table. Charged per signature on every transaction the cluster
/// processes, including one that executes and then errors, which is why
/// [`TransactionCosts::failed_lamports`] is not zero.
pub const BASE_SIGNATURE_FEE_LAMPORTS: u64 = 5_000;

/// Rent exemption for one SPL associated token account, in lamports.
///
/// §18's table, and the number the runtime charges for a 165-byte account. It is
/// a deposit rather than a fee — closing the account returns it — but a strategy
/// that opens an account per mint and never closes them is spending it, and a
/// cost stack that called a deposit free would be understating the entry.
pub const TOKEN_ACCOUNT_RENT_LAMPORTS: u64 = 2_039_280;

/// Micro-lamports in a lamport. The unit a compute unit price is quoted in.
pub const MICRO_LAMPORTS_PER_LAMPORT: u64 = 1_000_000;

/// How `φ` divides between the venue and the mint's creator.
///
/// The invariant is the whole point: `protocol_bps + creator_bps == total_bps`,
/// checked at construction, so a schedule cannot exist that charges a fill one
/// number and attributes another. [`FeeSplit::new`] returns `None` rather than
/// normalising, for the reason `types::ExitState::parse` gives about a
/// stored value: a split that does not add up is not a split to guess at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeSplit {
    /// `φ`, the total proportional fee on the SOL leg. The only part a quote
    /// ever sees.
    pub total_bps: u16,
    pub protocol_bps: u16,
    pub creator_bps: u16,
}

impl Default for FeeSplit {
    /// 100 bps, split 95/5.
    fn default() -> Self {
        FeeSplit {
            total_bps: DEFAULT_FEE_BPS,
            protocol_bps: DEFAULT_PROTOCOL_FEE_BPS,
            creator_bps: DEFAULT_CREATOR_FEE_BPS,
        }
    }
}

impl FeeSplit {
    /// A split, or `None` if the parts do not add up to the total.
    pub const fn new(protocol_bps: u16, creator_bps: u16) -> Option<Self> {
        let Some(total_bps) = protocol_bps.checked_add(creator_bps) else {
            return None;
        };
        if total_bps >= BPS_DENOMINATOR as u16 {
            return None;
        }
        Some(FeeSplit {
            total_bps,
            protocol_bps,
            creator_bps,
        })
    }

    /// The whole fee to the venue and nothing to anybody else.
    ///
    /// What a pool with no creator share charges — Raydium, and pump.fun before
    /// creator revenue existed. Reproduces the old lumped behaviour exactly.
    pub const fn protocol_only(total_bps: u16) -> Option<Self> {
        FeeSplit::new(total_bps, 0)
    }

    /// Splits one leg's fee into the two parts that make it up.
    ///
    /// `gross_lamports` is the SOL leg before the fee comes off — what the
    /// trader pays on a buy, what the curve pays out on a sell — and
    /// `fee_lamports` is what [`CurveState::quote_buy`] and
    /// [`CurveState::quote_sell`] already took off it.
    ///
    /// **The parts sum to `fee_lamports` exactly, always.** That is the reason
    /// this takes the charged number rather than recomputing it: the fill
    /// floored `gross × φ / 10 000` once, and two independent floors would
    /// disagree with it by a lamport in the ordinary case. So the creator's
    /// share is floored the way the program computes its own — `gross ×
    /// creator_bps / 10 000` — and **the protocol takes the remainder,
    /// rounding dust included**. The dust goes to the venue rather than the
    /// creator because the venue is the party whose share is defined as "the
    /// rest of it", and because a decomposition has to put it somewhere and
    /// saying where is better than a residual line nobody reads.
    ///
    /// A `fee_lamports` smaller than the creator's own floor — which only
    /// happens if a caller hands in a fee that did not come from this
    /// schedule — clamps the creator to the fee and leaves the protocol at
    /// zero, so the sum still holds.
    pub fn decompose(&self, gross_lamports: u64, fee_lamports: u64) -> SwapFees {
        let creator = (u128::from(gross_lamports) * u128::from(self.creator_bps)
            / u128::from(BPS_DENOMINATOR))
        .min(u128::from(fee_lamports)) as u64;
        SwapFees {
            gross_lamports,
            protocol_lamports: fee_lamports - creator,
            creator_lamports: creator,
            total_lamports: fee_lamports,
        }
    }

    /// The same split applied to a quoted [`Fill`], on whichever leg it is.
    pub fn of(&self, fill: &Fill) -> SwapFees {
        self.decompose(fill.gross_lamports, fill.fee_lamports)
    }
}

/// One swap's proportional fee, and who took it.
///
/// `protocol_lamports + creator_lamports == total_lamports` by construction and
/// [`SwapFees::balances`] says so out loud.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapFees {
    /// The SOL leg the fee was taken against, before it came off.
    pub gross_lamports: u64,
    pub protocol_lamports: u64,
    pub creator_lamports: u64,
    /// What the fill was actually charged. The sum of the two above.
    pub total_lamports: u64,
}

impl SwapFees {
    /// Whether the parts still add up. True by construction, asserted anyway.
    pub const fn balances(&self) -> bool {
        match self.protocol_lamports.checked_add(self.creator_lamports) {
            Some(sum) => sum == self.total_lamports,
            None => false,
        }
    }

    /// Two swaps' fees, added line by line.
    pub const fn saturating_add(&self, other: &SwapFees) -> SwapFees {
        SwapFees {
            gross_lamports: self.gross_lamports.saturating_add(other.gross_lamports),
            protocol_lamports: self
                .protocol_lamports
                .saturating_add(other.protocol_lamports),
            creator_lamports: self.creator_lamports.saturating_add(other.creator_lamports),
            total_lamports: self.total_lamports.saturating_add(other.total_lamports),
        }
    }
}

/// What one transaction costs to put on the network, whatever it does.
///
/// The rows of §18's table that are not proportional to size: the base fee per
/// signature, the priority fee the compute budget buys, the rent deposit each
/// new token account needs, and the tip the bundle bid. None of them is on the
/// curve and none of them appears in a [`Fill`], which is exactly why a backtest
/// that priced only the curve was flattering itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionCosts {
    /// How many signatures the message needs. At least one.
    pub signatures: u32,
    /// `signatures × 5 000`.
    pub base_lamports: u64,
    /// `ceil(price_micro_lamports × compute_units / 1 000 000)`.
    pub priority_lamports: u64,
    /// The rent deposit for accounts this transaction creates. Zero for an
    /// exit, which sells out of an account that already exists.
    pub rent_lamports: u64,
    /// What the bundle bid to land. Zero where there is no block engine to pay.
    pub tip_lamports: u64,
}

impl TransactionCosts {
    /// The costs of a transaction with the compute budget it was built with.
    ///
    /// `token_accounts_created` is the number of associated token accounts the
    /// transaction opens — one for an entry into a mint the wallet has never
    /// held, zero for an exit.
    pub fn new(
        signatures: u32,
        compute_unit_price_micro_lamports: u64,
        compute_unit_limit: u32,
        token_accounts_created: u32,
        tip_lamports: u64,
    ) -> Self {
        let signatures = signatures.max(1);
        TransactionCosts {
            signatures,
            base_lamports: BASE_SIGNATURE_FEE_LAMPORTS.saturating_mul(u64::from(signatures)),
            priority_lamports: priority_fee_lamports(
                compute_unit_price_micro_lamports,
                compute_unit_limit,
            ),
            rent_lamports: TOKEN_ACCOUNT_RENT_LAMPORTS
                .saturating_mul(u64::from(token_accounts_created)),
            tip_lamports,
        }
    }

    /// What a transaction that lands costs.
    pub const fn total_lamports(&self) -> u64 {
        self.base_lamports
            .saturating_add(self.priority_lamports)
            .saturating_add(self.rent_lamports)
            .saturating_add(self.tip_lamports)
    }

    /// What a transaction that executes and then errors costs.
    ///
    /// §18: base plus priority. The rent is never taken because no account is
    /// created, and the tip is never taken because the transfer that pays it is
    /// an instruction in the same transaction and it reverted with everything
    /// else. **That last part is only true while the tip is paid in-band**, and
    /// [`crate::execution::build_exit`] is where that is arranged; a tip paid by
    /// a separate transaction would belong on this line.
    pub const fn failed_lamports(&self) -> u64 {
        self.base_lamports.saturating_add(self.priority_lamports)
    }
}

/// The priority fee a compute budget buys, in lamports.
///
/// `ceil(price × units / 10^6)`, rounded up because the runtime rounds it up and
/// because [`slippage_bps`] gives the reason a simulator's residual goes to the
/// trader's cost rather than away from it.
pub fn priority_fee_lamports(price_micro_lamports: u64, compute_units: u32) -> u64 {
    u128::from(price_micro_lamports)
        .saturating_mul(u128::from(compute_units))
        .div_ceil(u128::from(MICRO_LAMPORTS_PER_LAMPORT))
        .min(u128::from(u64::MAX)) as u64
}

/// §18's whole table for one swap: what the curve took and what the network did.
///
/// The two halves are kept apart rather than summed into one number because
/// they answer different questions — the curve's cut scales with size and the
/// network's does not — and because only one of them survives a transaction
/// that fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostStack {
    pub swap: SwapFees,
    pub transaction: TransactionCosts,
}

impl CostStack {
    /// Every lamport this swap cost, assuming it landed.
    pub const fn total_lamports(&self) -> u64 {
        self.swap
            .total_lamports
            .saturating_add(self.transaction.total_lamports())
    }

    /// Every lamport it cost if it did not.
    ///
    /// The curve charges nothing for a swap that did not happen, so this is
    /// [`TransactionCosts::failed_lamports`] and nothing else.
    pub const fn failed_lamports(&self) -> u64 {
        self.transaction.failed_lamports()
    }
}

// ===========================================================================
// §5 — the session the window drives
// ===========================================================================

/// How fast a fixture is played, in the four steps the cockpit offers.
///
/// The wire form is the chip's own label — `"1"`, `"5"`, `"10"`, `"max"` —
/// because the window draws the pressed chip from the speed the engine reports
/// rather than from the click that asked for it, and a value that round-trips
/// through a different spelling is a chip that quietly stops matching the
/// engine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplaySpeed {
    /// A second of recording per second of wall clock.
    #[default]
    #[serde(rename = "1")]
    Real,
    #[serde(rename = "5")]
    Five,
    #[serde(rename = "10")]
    Ten,
    /// Every record the tick's budget allows. Not "instantly": `advance` is
    /// bounded by `MAX_RECORDS_PER_ADVANCE`, so one tick cannot take the
    /// runtime away from everything else for the length of a fixture.
    #[serde(rename = "max")]
    Max,
}

impl ReplaySpeed {
    /// How many milliseconds of recording one millisecond of wall clock buys,
    /// or `None` for "as many records as the budget allows".
    pub fn multiplier(self) -> Option<u64> {
        match self {
            ReplaySpeed::Real => Some(1),
            ReplaySpeed::Five => Some(5),
            ReplaySpeed::Ten => Some(10),
            ReplaySpeed::Max => None,
        }
    }

    /// The label on the chip, which is also the wire form.
    pub const fn as_str(self) -> &'static str {
        match self {
            ReplaySpeed::Real => "1",
            ReplaySpeed::Five => "5",
            ReplaySpeed::Ten => "10",
            ReplaySpeed::Max => "max",
        }
    }
}

impl fmt::Display for ReplaySpeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The most records one `advance` will step, however fast it was asked to go.
///
/// `max` has to mean "as fast as this machine will step it" without also
/// meaning "and nothing else runs until the fixture ends". A hundred thousand
/// records is a few milliseconds of stepping and rather more than a busy
/// launch second, so the playhead still moves in front of whoever is watching
/// it.
const MAX_RECORDS_PER_ADVANCE: usize = 100_000;

/// What the transport is doing, which is more than "on" or "off".
///
/// The switch on the bar is a boolean and playback is not: a paused fixture is
/// still a fixture behind the clock, and a fixture that reached its last record
/// is neither playing nor stopped. Collapsing those three into one flag is what
/// makes a window draw "replay off" over numbers that came out of a recording.
///
/// `active` on the status is derived from this and stays the safety answer —
/// see `PlaybackState::is_active`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlaybackState {
    /// Nothing is behind the clock. Either no fixture was ever opened, or one
    /// was opened and the operator stopped it.
    #[default]
    Stopped,
    /// The ticker is spending wall clock on it.
    Playing,
    /// A fixture is open and the playhead is held where it is. The ticker still
    /// runs; it just buys nothing.
    Paused,
    /// The playhead is past the last record. Distinct from `Paused` because
    /// there is nothing left to resume into, and distinct from `Stopped`
    /// because the numbers on screen still came out of the recording.
    Ended,
}

impl PlaybackState {
    /// Whether a recording — rather than a feed — is what the window is
    /// showing.
    ///
    /// True for everything except `Stopped`, and that includes `Ended`. The
    /// flag answers "is anything under this bar live", and a fixture that ran
    /// out of records did not put the feeds back: it left the clock on the
    /// recording's timeline with nothing arriving. Reporting that as live is
    /// the mistake the bar exists to prevent.
    pub const fn is_active(self) -> bool {
        !matches!(self, PlaybackState::Stopped)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            PlaybackState::Stopped => "stopped",
            PlaybackState::Playing => "playing",
            PlaybackState::Paused => "paused",
            PlaybackState::Ended => "ended",
        }
    }
}

impl fmt::Display for PlaybackState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One press on the transport.
///
/// A closed vocabulary in one command rather than a command per button, for the
/// reason `set_replay_playback` gives about the switch and the speed: two
/// entry points that can each decide whether a fixture is behind the clock are
/// two entry points that will eventually disagree about it, and the one that is
/// wrong is the one holding the bar up over live candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReplayControl {
    /// Open the fixture, rewind it, and play. What the switch sends.
    Play,
    /// Hold the playhead. Legal from `Playing` and a no-op from anywhere else.
    Pause,
    /// Release it. Legal from `Paused`, and from `Stopped` with a fixture still
    /// open, which is what makes stop-then-play resume rather than rewind.
    Resume,
    /// Put the fixture down. The playhead stays where it stopped.
    Stop,
    /// Step exactly `records` records, whatever the clock says, and pause.
    Step,
    /// Play up to `records` records now — or every one that is left, when no
    /// count is given — without spending wall clock on them.
    FastForward,
}

/// The economics of a replayed fixture, as they stand right now.
///
/// **This is a shape, not a calculation.** Nothing in this module fills it in:
/// `replay` knows about records, clocks and chains, and what a frame means
/// economically is `backtest`'s question. The struct lives here because it is
/// what `ReplayStatus` carries and what a `ReplayObserver` promises to
/// produce, and a seam whose data type lives on the far side of it is not a
/// seam.
///
/// Every field is an integer in a named unit, for the same reason
/// `PerformanceSummary` is: two runs of one fixture have to agree bit for bit,
/// and the last bit of an `f64` division is not something this engine controls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimulatedLedger {
    /// Frames that decoded into something the simulation understood and
    /// applied.
    pub events_applied: u64,
    /// Frames that were genuine records carrying a payload this build does not
    /// read. Counted rather than ignored: a fixture full of them is a fixture
    /// being replayed by the wrong version, and a ledger of zeroes with no
    /// explanation looks like a strategy that did nothing.
    pub events_undecodable: u64,
    /// Frames the recording says the live filters rejected. Not applied — the
    /// live engine did not see them either — and counted so the difference
    /// between "nothing happened" and "nothing was let through" is visible.
    pub events_filtered: u64,
    /// Distinct mints the fixture opened.
    pub launches: u64,
    pub entries: u64,
    pub exits: u64,
    /// Closed round trips, which is not `exits`: one exit can close several
    /// lots and one lot can be closed by several exits.
    pub trades: u64,
    pub entry_gross_lamports: u64,
    pub exit_net_lamports: u64,
    pub fees_lamports: u64,
    /// Closed-trade profit only. A position the fixture ended while still
    /// holding is never marked into this — see `StrandedPosition` on the
    /// backtest side for why a mark is not a realisation.
    pub realized_pnl_lamports: i64,
    /// Quotes the curve refused, ours and other people's.
    pub quote_failures: u64,
    /// What the tip policy would have bid across every simulated exit.
    pub tips_lamports: u64,
    pub tips_bid: u64,
    /// Exits the tip policy refused to bid for. Kept, because a tip that was
    /// never priced is not a tip of zero.
    pub tips_refused: u64,
    /// Size-weighted mean slippage across our own fills, in basis points.
    ///
    /// Weighted by the SOL leg rather than averaged per fill, because a
    /// hundred-lamport dust exit and a one-SOL entry are not two equal
    /// observations of what this strategy pays to trade.
    pub slippage_bps: u16,
    /// The worst single fill, which is the number a size limit is set from and
    /// the one a mean hides.
    pub worst_slippage_bps: u16,
}

/// Something that watches the records a session plays.
///
/// The seam between "a fixture is being streamed" and "a fixture is being
/// traded against". `ReplaySession` steps records and owns the playhead; what
/// those records are worth is somebody else's arithmetic, and this is the one
/// method it gets to do it through.
///
/// Fed strictly in stream order, once per record, after the clock has been
/// advanced to that record and before the next one is read — the same delivery
/// discipline `ReplayDriver` documents, for the same reason. An observer
/// therefore never needs to sort, deduplicate, or look ahead, and cannot.
pub trait ReplayObserver: fmt::Debug + Send {
    /// One record, with the clock advance that was applied for it.
    fn observe(&mut self, advance: ClockAdvance, record: &ReplayRecord);

    /// Everything it has worked out so far.
    fn ledger(&self) -> SimulatedLedger;

    /// Forgets the run and goes back to an empty book.
    ///
    /// Called whenever the playhead is rewound, which is what keeps §7's
    /// determinism claim true of the ledger as well as of the playhead: a
    /// second run of one fixture reports the same PnL as the first, rather than
    /// twice as much of it.
    fn reset(&mut self);
}

/// Why a fixture would not open.
///
/// Deliberately separate from `ReplayError`, which is about the records. These
/// are about the directory they were meant to be in, and the two need telling
/// apart: one says the fixture is wrong, the other says there is not one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// There is no fixture directory at that path.
    Missing { path: String },
    /// The directory is there and holds no `.jsonl` segment.
    NoSegments { path: String },
    /// Several segments and no manifest saying they are segments of one
    /// stream. Guessing which stream they belong to gets a rotated fixture
    /// wrong in the direction that looks like tampering, so it is refused.
    Ambiguous { path: String, files: usize },
    /// A file would not read, or the manifest would not parse.
    Io { path: String, detail: String },
    /// The records themselves are wrong.
    Fixture { path: String, source: ReplayError },
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::Missing { path } => {
                write!(f, "there is no fixture directory at {path}")
            }
            SessionError::NoSegments { path } => {
                write!(f, "{path} holds no .jsonl fixture segment")
            }
            SessionError::Ambiguous { path, files } => write!(
                f,
                "{path} holds {files} segments and no manifest.json saying which stream they \
                 belong to"
            ),
            SessionError::Io { path, detail } => write!(f, "{path} could not be read: {detail}"),
            SessionError::Fixture { path, source } => {
                write!(f, "{path} is not replayable: {source}")
            }
        }
    }
}

impl std::error::Error for SessionError {}

impl From<ManifestError> for SessionError {
    fn from(err: ManifestError) -> Self {
        SessionError::Io {
            path: err.path,
            detail: err.detail,
        }
    }
}

/// Everything the cockpit draws, as one answer.
///
/// Every replay command returns this and the replay telemetry line carries the
/// same shape, so the window has one renderer and one set of fields to trust
/// rather than a poll shape and a push shape free to drift apart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayStatus {
    /// Which edition of the session this answer was taken from, counting up
    /// from zero.
    ///
    /// Three things tell the window what replay is doing — the command it just
    /// called, the status it polls for every second, and the telemetry line the
    /// ticker pushes — and none of them arrives in a guaranteed order with
    /// respect to the other two. A poll issued before a `pause` and answered
    /// after it carries a status that was true when it was taken and is not
    /// true any more, and a window that drew it would put the transport back to
    /// `playing` over a fixture the engine is holding.
    ///
    /// So every answer carries the edition it came from, and a window that has
    /// already drawn a later one throws it away. This is the only field here
    /// that is not a fact about the recording: it is a fact about the answer,
    /// and it is what makes the other fields safe to draw.
    ///
    /// Bumped under the lock that made the change, and only when something
    /// actually changed — a repeated answer carries a repeated number.
    pub revision: u64,
    /// Whether a recording is driving the clock right now.
    ///
    /// Derived from `state` rather than stored beside it, so the flag the
    /// safety claim rests on and the word the transport draws cannot disagree.
    pub active: bool,
    /// Which of the four the transport is in. `active` is the coarse answer;
    /// this is the one a pause button is drawn from.
    pub state: PlaybackState,
    pub speed: ReplaySpeed,
    /// `None` until a fixture has been opened. Nothing is guessed from the
    /// directory name: a session that has not read the files does not know
    /// what is in them.
    pub stream_id: Option<String>,
    /// The chain head the records compute to, which is not necessarily the one
    /// the manifest declares — see `chain_verified`.
    pub chain_head: Option<String>,
    /// `Some(true)` when the manifest's declared head is the one the records
    /// compute to, `Some(false)` when it is not, `None` when there was no
    /// manifest to check against.
    ///
    /// `None` is the absence of the check and is deliberately not a pass. The
    /// internal chain did verify — `ReplayCursor::open` does not return
    /// otherwise — but nothing independent said what the head was supposed to
    /// be, and "every link I was given agrees with itself" is a weaker claim
    /// than the one the bar's "verified" makes.
    pub chain_verified: Option<bool>,
    /// The manifest's `complete` flag. False means §4 stopped the recording on
    /// an error and there is a hole in it. `None` when there was no manifest.
    pub fixture_complete: Option<bool>,
    /// Where the virtual slot clock is.
    pub slot: u64,
    pub first_slot: Option<u64>,
    pub last_slot: Option<u64>,
    pub records_played: u64,
    pub record_count: u64,
    /// Records whose timestamp was behind the clock, and records whose slot
    /// was. Counted rather than corrected, per §2, and reported rather than
    /// hidden so a fixture recorded against a provider with a broken clock is
    /// visible as one.
    pub clamped: u64,
    pub slot_regressions: u64,
    /// Where the virtual wall clock is, in the recording's own milliseconds.
    ///
    /// Beside `slot` rather than instead of it: §2 virtualises both axes, they
    /// are advanced from different fields of the same record, and a fixture
    /// recorded against a provider with a broken clock is exactly the case
    /// where they disagree.
    pub at_ms: i64,
    /// What the strategy made out of what has been played so far.
    ///
    /// All zeroes when no observer is attached, which is the honest answer for
    /// a session that is streaming records and pricing nothing — not a claim
    /// that the run broke even.
    pub ledger: SimulatedLedger,
}

/// One opened fixture and the playhead walking it.
#[derive(Debug)]
struct Fixture {
    driver: ReplayDriver,
    stream_id: String,
    chain_head: String,
    chain_verified: Option<bool>,
    complete: Option<bool>,
    first_slot: u64,
    last_slot: u64,
    record_count: u64,
}

#[derive(Debug, Default)]
struct SessionState {
    fixture: Option<Fixture>,
    state: PlaybackState,
    speed: ReplaySpeed,
    /// Whoever is pricing the records as they go past. `None` is a session that
    /// streams a fixture into the clock and books nothing.
    observer: Option<Box<dyn ReplayObserver>>,
    /// Counts every change to the fields above and to the playhead. See
    /// [`ReplayStatus::revision`] for what the window does with it.
    revision: u64,
}

impl SessionState {
    /// Records that something here changed.
    ///
    /// Called under the same lock as the change itself, so the edition and the
    /// state it describes are written together and can never be read apart. A
    /// caller that changed nothing must not call it: the whole value of the
    /// number is that a window can tell a new answer from a repeated one.
    fn touch(&mut self) {
        self.revision += 1;
    }
}

/// The replay control the window drives.
///
/// It owns the fixture, the playhead and the multiplier, and it is the only
/// thing in the process that can answer "is a recording driving these
/// numbers". Everything on it is synchronous and behind one lock: the ticker
/// that advances it and the command that reads it are on different threads,
/// and a status read halfway through a step would report a playhead that never
/// existed.
///
/// **It does not displace the live feeds.** §5 puts that behind
/// `FixtureDialer`, which is a different seam and a different change; this
/// session drives the replay clock and the cockpit above the panes, and
/// nothing else. Everything that follows from that — including who is allowed
/// to start it — is decided in `lib.rs`, because it is a question about the
/// application rather than about replay.
pub struct ReplaySession {
    dir: PathBuf,
    state: Mutex<SessionState>,
}

impl ReplaySession {
    /// A session over the directory a fixture is expected in.
    ///
    /// Nothing is read here. A window opening is not a reason to parse ninety
    /// thousand records off disk, and a directory that grows a fixture while
    /// the application is up should be found when somebody asks for it rather
    /// than never.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            state: Mutex::new(SessionState::default()),
        }
    }

    /// The same session with something pricing the records it plays.
    ///
    /// Taken at construction rather than swapped at runtime: an observer
    /// exchanged halfway through a fixture would produce a ledger that is
    /// half one strategy and half another, and there is no honest way to
    /// label that number.
    pub fn observing(mut self, observer: impl ReplayObserver + 'static) -> Self {
        self.state.get_mut().observer = Some(Box::new(observer));
        self
    }

    /// Where this session looks for a fixture.
    pub fn directory(&self) -> &Path {
        &self.dir
    }

    /// What the window draws.
    pub fn status(&self) -> ReplayStatus {
        status_of(&self.state.lock())
    }

    /// Whether a recording rather than a feed is behind the numbers.
    ///
    /// True while paused and true after the fixture ended — see
    /// `PlaybackState::is_active`.
    pub fn is_active(&self) -> bool {
        self.state.lock().state.is_active()
    }

    /// Which of the four states the transport is in.
    pub fn playback_state(&self) -> PlaybackState {
        self.state.lock().state
    }

    /// Changes the multiplier.
    ///
    /// Legal whether or not a fixture is playing: the chip the operator pressed
    /// is what the next `advance` spends its budget at, and refusing the press
    /// because playback has not started yet would make the control depend on
    /// the order two buttons were pressed in.
    pub fn set_speed(&self, speed: ReplaySpeed) -> ReplayStatus {
        let mut state = self.state.lock();
        if state.speed != speed {
            state.speed = speed;
            state.touch();
        }
        status_of(&state)
    }

    /// Opens the fixture if it is not open already, rewinds it, and plays.
    ///
    /// Rewinding is the point. Entering replay twice in one session has to
    /// produce the same run both times — that is the whole of §7's determinism
    /// claim — and a playhead left where the previous run stopped would make
    /// the second one a run of a different fixture.
    pub fn start(&self) -> Result<ReplayStatus, SessionError> {
        let mut state = self.state.lock();
        self.open(&mut state)?;
        rewind(&mut state);
        state.state = PlaybackState::Playing;
        // Always, even from a state that already read as playing: the rewind
        // put the playhead back at the start, so a window still drawing the
        // last run's record count is drawing a number from a run that is over.
        state.touch();
        Ok(status_of(&state))
    }

    /// Stops playing and leaves the playhead where it stopped.
    ///
    /// The fixture stays open. Which recording it was and whether its chain
    /// verified is still the true answer to "what was driving those numbers",
    /// and dropping it on the way out would leave the window with nothing to
    /// say about the run that just finished.
    pub fn stop(&self) -> ReplayStatus {
        let mut state = self.state.lock();
        if state.state != PlaybackState::Stopped {
            state.state = PlaybackState::Stopped;
            state.touch();
        }
        status_of(&state)
    }

    /// Holds the playhead without putting the fixture down.
    ///
    /// A no-op from anywhere but `Playing`, on purpose. Pausing a stopped
    /// session into `Paused` would raise the bar over a window nobody asked to
    /// put in replay, and pausing an ended one would offer a resume that has
    /// nothing to resume into.
    pub fn pause(&self) -> ReplayStatus {
        let mut state = self.state.lock();
        if state.state == PlaybackState::Playing {
            state.state = PlaybackState::Paused;
            state.touch();
        }
        status_of(&state)
    }

    /// Releases a held playhead, without rewinding it.
    ///
    /// This is the difference between `resume` and `start`: `start` is
    /// "play this fixture", which means from the beginning, and `resume` is
    /// "carry on", which means from here. A transport where the play button
    /// silently rewinds is one an operator loses a paused position to.
    ///
    /// Opens the fixture if there is not one yet, so a first press of resume on
    /// a fresh session plays rather than doing nothing. From `Ended` it is a
    /// no-op: there is no record left to carry on to, and restarting would make
    /// the button mean two different things depending on where the playhead
    /// happened to be.
    pub fn resume(&self) -> Result<ReplayStatus, SessionError> {
        let mut state = self.state.lock();
        if state.state == PlaybackState::Ended {
            return Ok(status_of(&state));
        }
        self.open(&mut state)?;
        state.state = PlaybackState::Playing;
        state.touch();
        Ok(status_of(&state))
    }

    /// Plays whatever `wall_ms` of real time buys at the current speed.
    ///
    /// Does nothing at all unless the transport is `Playing`, which is what
    /// makes it safe to call from a ticker that has no idea whether anybody has
    /// pressed anything.
    pub fn advance(&self, wall_ms: u64) -> ReplayStatus {
        let mut state = self.state.lock();
        if state.state != PlaybackState::Playing {
            return status_of(&state);
        }
        let speed = state.speed;
        let budget = speed.multiplier().map(|multiplier| {
            i64::try_from(wall_ms.saturating_mul(multiplier)).unwrap_or(i64::MAX)
        });
        play(&mut state, Budget::Clock(budget), MAX_RECORDS_PER_ADVANCE);
        // Only an exhausted cursor ends the run. A tick that spent its whole
        // budget and stopped mid-fixture is the ordinary case and must not be
        // mistaken for the last record.
        if exhausted(&state) {
            state.state = PlaybackState::Ended;
        }
        // A tick that reached here was playing, so the playhead moved or the
        // run ended. Either way this is a new edition.
        state.touch();
        status_of(&state)
    }

    /// Steps exactly `records` records, whatever the clock says, and pauses.
    ///
    /// The frame-advance button. It ignores the multiplier entirely — a step is
    /// a step at every speed — and it leaves the transport `Paused` rather than
    /// where it found it, because a step that kept playing would move the
    /// playhead off the record the operator stopped on before they had read it.
    ///
    /// Opens the fixture if there is not one open, so the first press works on
    /// a fresh session. Bounded by the same ceiling one tick is: a step of a
    /// hundred thousand records is a scrub, and a scrub that took the runtime
    /// away for the length of a fixture would be a hang.
    pub fn step(&self, records: u64) -> Result<ReplayStatus, SessionError> {
        let mut state = self.state.lock();
        self.open(&mut state)?;
        let wanted = usize::try_from(records).unwrap_or(usize::MAX);
        play(
            &mut state,
            Budget::Records,
            wanted.min(MAX_RECORDS_PER_ADVANCE),
        );
        state.state = if exhausted(&state) {
            PlaybackState::Ended
        } else {
            PlaybackState::Paused
        };
        state.touch();
        Ok(status_of(&state))
    }

    /// Plays records now instead of over the next several seconds.
    ///
    /// `records` of them, or every one that is left when it is `None` — which
    /// is what makes this the backtest runner as well as a transport control: a
    /// fast-forward to the end is a whole fixture priced, deterministically, in
    /// the time it takes to walk a vector that is already in memory.
    ///
    /// **It plays them, it does not skip them.** Every record goes through the
    /// clock and past the observer in order, exactly as a tick would deliver
    /// it, so the ledger after a fast-forward is the ledger after watching the
    /// same fixture at `1x` — which is §7's determinism claim, and would not
    /// survive a seek. §9 is the other half of the reason: `ReplayCursor` has
    /// no way to arrive at record *n* without having yielded the ones before
    /// it, and this deliberately does not add one.
    ///
    /// Leaves the transport `Paused` at wherever it stopped, for the reason
    /// `step` does.
    pub fn fast_forward(&self, records: Option<u64>) -> Result<ReplayStatus, SessionError> {
        let mut state = self.state.lock();
        self.open(&mut state)?;
        // The whole fixture is already parsed into a `Vec`, so "everything that
        // is left" is bounded by something that has already been sized and
        // allocated. That is why this one is not capped at a tick's ceiling.
        let wanted = match records {
            Some(records) => usize::try_from(records).unwrap_or(usize::MAX),
            None => state
                .fixture
                .as_ref()
                .map(|fixture| fixture.driver.cursor().remaining())
                .unwrap_or(0),
        };
        play(&mut state, Budget::Records, wanted);
        state.state = if exhausted(&state) {
            PlaybackState::Ended
        } else {
            PlaybackState::Paused
        };
        state.touch();
        Ok(status_of(&state))
    }

    /// Runs one press of the transport.
    ///
    /// One entry point rather than six, so a caller that has to check something
    /// before a fixture goes behind the clock — `lib.rs` and the live feeds —
    /// has one place to check it rather than one place per button.
    pub fn control(
        &self,
        control: ReplayControl,
        records: Option<u64>,
    ) -> Result<ReplayStatus, SessionError> {
        match control {
            ReplayControl::Play => self.start(),
            ReplayControl::Pause => Ok(self.pause()),
            ReplayControl::Resume => self.resume(),
            ReplayControl::Stop => Ok(self.stop()),
            // A step with no count is one record: the button is a frame
            // advance, and defaulting it to anything else would make a press
            // mean a different amount on different callers.
            ReplayControl::Step => self.step(records.unwrap_or(1)),
            ReplayControl::FastForward => self.fast_forward(records),
        }
    }

    /// Opens the fixture into `state` if it is not open already.
    ///
    /// Reading the directory is the expensive half of every control above, and
    /// it happens once: a fixture already open is left exactly as it is, so
    /// pressing step forty times parses ninety thousand records once rather
    /// than forty times.
    fn open(&self, state: &mut SessionState) -> Result<(), SessionError> {
        if state.fixture.is_none() {
            state.fixture = Some(open_fixture(&self.dir)?);
            // A fresh fixture and a stale ledger is the one combination that
            // reports another run's PnL against this recording.
            if let Some(observer) = state.observer.as_mut() {
                observer.reset();
            }
        }
        Ok(())
    }
}

/// What bounds one call to `play`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Budget {
    /// Milliseconds of recording, or `None` for `max` — as many as the record
    /// ceiling allows. What a tick spends.
    Clock(Option<i64>),
    /// Records, and nothing else. What a step and a fast-forward spend.
    Records,
}

/// Whether the playhead is past the last record.
///
/// The only thing that ends a run. A call that merely spent its budget and
/// stopped mid-fixture has not, and asking the cursor rather than tracking it
/// separately is what keeps those two from drifting apart.
fn exhausted(state: &SessionState) -> bool {
    state
        .fixture
        .as_ref()
        .map(|fixture| fixture.driver.cursor().is_exhausted())
        .unwrap_or(false)
}

/// Steps the driver forward, delivering every record it steps.
///
/// Under `Budget::Clock` the budget is spent against the *clock*, not against a
/// record count: `5x` means five seconds of recording per second of wall clock,
/// so a quiet stretch of the fixture costs the same wall time as a busy one and
/// playback runs at the rate the thing was recorded at rather than at the rate
/// this machine happens to parse it. Under `Budget::Records` the clock is not
/// consulted at all — that is what a step and a fast-forward are.
///
/// `ceiling` bounds both, and every record stepped goes past the observer
/// before the next one is read, which is what makes a fast-forward and a slow
/// watch of the same fixture produce the same ledger.
fn play(state: &mut SessionState, budget: Budget, ceiling: usize) {
    // Destructured because the observer and the fixture are two fields of one
    // struct and the loop below needs a mutable borrow of each at once.
    let SessionState {
        fixture, observer, ..
    } = state;
    let Some(fixture) = fixture.as_mut() else {
        return;
    };

    let mut records = 0usize;
    let mut observe = |advance: ClockAdvance, record: &ReplayRecord| {
        if let Some(observer) = observer.as_mut() {
            observer.observe(advance, record);
        }
    };

    // The first record is what sets the clock — it starts at the epoch and the
    // fixture starts wherever it was recorded — so there is no gap in front of
    // it to pay for. Charging the budget for that gap would spend every tick
    // for the next fifty-odd years on one record.
    if fixture.driver.clock().advances() == 0 && ceiling > 0 {
        match fixture.driver.step() {
            Some((advance, record)) => {
                observe(advance, record);
                records += 1;
            }
            None => return,
        }
    }

    let started_at = fixture.driver.clock().now_ms();

    while records < ceiling {
        // Checked before the step rather than after, so a record is never half
        // delivered: the budget may be overspent by one record's gap, and that
        // is the honest way to round it.
        if let Budget::Clock(Some(budget_ms)) = budget {
            if fixture.driver.clock().now_ms().saturating_sub(started_at) >= budget_ms {
                return;
            }
        }
        match fixture.driver.step() {
            Some((advance, record)) => {
                observe(advance, record);
                records += 1;
            }
            None => return,
        }
    }
}

/// Puts the playhead, the clock and the ledger back to the start together.
///
/// All three or none: a rewound cursor with a ledger still holding the last
/// run's trades reports one fixture's records against two fixtures' PnL.
fn rewind(state: &mut SessionState) {
    if let Some(fixture) = state.fixture.as_mut() {
        fixture.driver.restart();
    }
    if let Some(observer) = state.observer.as_mut() {
        observer.reset();
    }
}

/// The status a state is in, with or without a fixture behind it.
fn status_of(state: &SessionState) -> ReplayStatus {
    let ledger = state
        .observer
        .as_ref()
        .map(|observer| observer.ledger())
        .unwrap_or_default();

    let Some(fixture) = state.fixture.as_ref() else {
        return ReplayStatus {
            revision: state.revision,
            active: state.state.is_active(),
            state: state.state,
            speed: state.speed,
            stream_id: None,
            chain_head: None,
            chain_verified: None,
            fixture_complete: None,
            slot: 0,
            first_slot: None,
            last_slot: None,
            records_played: 0,
            record_count: 0,
            clamped: 0,
            slot_regressions: 0,
            at_ms: 0,
            ledger,
        };
    };

    let clock = fixture.driver.clock();
    ReplayStatus {
        revision: state.revision,
        active: state.state.is_active(),
        state: state.state,
        speed: state.speed,
        stream_id: Some(fixture.stream_id.clone()),
        chain_head: Some(fixture.chain_head.clone()),
        chain_verified: fixture.chain_verified,
        fixture_complete: fixture.complete,
        slot: clock.slot(),
        first_slot: Some(fixture.first_slot),
        last_slot: Some(fixture.last_slot),
        records_played: fixture.driver.cursor().position() as u64,
        record_count: fixture.record_count,
        clamped: clock.clamped(),
        slot_regressions: clock.slot_regressions(),
        at_ms: clock.now_ms(),
        ledger,
    }
}

/// Reads a fixture directory into a driver, or says exactly what was wrong.
///
/// Refuses rather than repairs, for §9's reason: a fixture is evidence, and a
/// loader that sorted a mis-ordered stream into order would hide the recorder
/// bug that produced it.
fn open_fixture(dir: &Path) -> Result<Fixture, SessionError> {
    if !dir.is_dir() {
        return Err(SessionError::Missing {
            path: dir.display().to_string(),
        });
    }

    let manifest = Manifest::read_dir(dir)?;
    let files = segments(dir)?;

    // With a manifest, every file is a segment of the one stream it names.
    // Without one there is nothing saying two files belong to the same chain,
    // and a chain computed across two streams does not verify — which would
    // reach the operator as a tampered fixture rather than as a directory
    // nobody wrote a manifest for.
    let stream_id = match (&manifest, files.len()) {
        (Some(manifest), _) => manifest.stream_id.clone(),
        (None, 1) => files[0]
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("stream")
            .to_string(),
        (None, count) => {
            return Err(SessionError::Ambiguous {
                path: dir.display().to_string(),
                files: count,
            })
        }
    };

    let mut records = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).map_err(|err| SessionError::Io {
            path: file.display().to_string(),
            detail: err.to_string(),
        })?;
        records.extend(parse_stream(&text).map_err(|source| SessionError::Fixture {
            path: file.display().to_string(),
            source,
        })?);
    }

    // Taken from the records rather than from the manifest, because these are
    // what the playhead is going to be compared against. A manifest that
    // disagrees with them is a manifest that disagrees with the fixture, which
    // is what `chain_verified` is for.
    let first_slot = records.first().map(|record| record.slot).unwrap_or(0);
    let last_slot = records.last().map(|record| record.slot).unwrap_or(0);
    let record_count = records.len() as u64;

    let cursor =
        ReplayCursor::open(&stream_id, records).map_err(|source| SessionError::Fixture {
            path: dir.display().to_string(),
            source,
        })?;
    let chain_head = hex(&cursor.chain_head());

    Ok(Fixture {
        // A head the manifest disagrees with is not a refusal. The chain itself
        // verified — `open` does not return otherwise — so the records are
        // internally sound and can be played; what is wrong is that the
        // recording no longer matches the document describing it, and that is a
        // fact for the bar to show rather than a reason to show nothing.
        chain_verified: manifest
            .as_ref()
            .map(|manifest| manifest.chain_head == chain_head),
        complete: manifest.as_ref().map(|manifest| manifest.complete),
        driver: ReplayDriver::new(cursor),
        stream_id,
        chain_head,
        first_slot,
        last_slot,
        record_count,
    })
}

/// The `.jsonl` files in a directory, in name order.
///
/// Sorted, because `read_dir` hands them back in whatever order the filesystem
/// felt like, and a run whose segment order depends on the filesystem is a run
/// that does not reproduce on another machine.
fn segments(dir: &Path) -> Result<Vec<PathBuf>, SessionError> {
    let entries = fs::read_dir(dir).map_err(|err| SessionError::Io {
        path: dir.display().to_string(),
        detail: err.to_string(),
    })?;

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| SessionError::Io {
            path: dir.display().to_string(),
            detail: err.to_string(),
        })?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
    files.sort();

    if files.is_empty() {
        return Err(SessionError::NoSegments {
            path: dir.display().to_string(),
        });
    }
    Ok(files)
}

// ===========================================================================
// tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const SOL: u64 = LAMPORTS_PER_SOL;

    // -----------------------------------------------------------------------
    // vendored primitives
    // -----------------------------------------------------------------------

    #[test]
    fn sha256_matches_the_published_vectors() {
        // FIPS 180-4 / NIST examples. If these pass, a chain built on this is
        // wrong only in ways that are also wrong for everybody else.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn sha256_spans_the_block_boundary() {
        // 55, 56, 63, 64 and 65 bytes exercise every padding branch there is.
        for length in [55usize, 56, 63, 64, 65, 119, 120] {
            let input = vec![b'a'; length];
            let digest = sha256::digest(&input);
            assert_ne!(digest, [0u8; 32], "length {length} hashed to zero");
        }
        assert_eq!(
            sha256_hex(&vec![b'a'; 1000]),
            "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3"
        );
    }

    #[test]
    fn hex_round_trips_and_rejects_hand_edits() {
        let digest = sha256::digest(b"chain");
        let text = hex(&digest);
        assert_eq!(text.len(), 64);
        assert_eq!(unhex(&text), Some(digest));

        assert_eq!(
            unhex(&text.to_uppercase()),
            None,
            "uppercase is not our form"
        );
        assert_eq!(unhex(&text[..63]), None, "short digest");
        assert_eq!(unhex(&format!("{text}0")), None, "long digest");
        assert_eq!(unhex(&"g".repeat(64)), None, "not hex");
    }

    #[test]
    fn base64_matches_rfc_4648() {
        for (raw, encoded) in [
            (&b""[..], ""),
            (&b"f"[..], "Zg=="),
            (&b"fo"[..], "Zm8="),
            (&b"foo"[..], "Zm9v"),
            (&b"foob"[..], "Zm9vYg=="),
            (&b"fooba"[..], "Zm9vYmE="),
            (&b"foobar"[..], "Zm9vYmFy"),
        ] {
            assert_eq!(base64::encode(raw), encoded, "encoding {raw:?}");
            assert_eq!(
                base64::decode(encoded).as_deref(),
                Some(raw),
                "decoding {encoded}"
            );
        }
    }

    #[test]
    fn base64_round_trips_arbitrary_bytes_and_rejects_rubbish() {
        for length in 0..200usize {
            let raw: Vec<u8> = (0..length).map(|i| (i * 37 + 11) as u8).collect();
            let encoded = base64::encode(&raw);
            assert_eq!(base64::decode(&encoded), Some(raw), "length {length}");
        }
        assert_eq!(
            base64::decode("Z"),
            None,
            "a lone character encodes nothing"
        );
        assert_eq!(base64::decode("Zm9v!!"), None, "not in the alphabet");
    }

    // -----------------------------------------------------------------------
    // section 2 - the clocks
    // -----------------------------------------------------------------------

    #[test]
    fn replay_clock_walks_the_fixture_timeline() {
        let clock = ReplayClock::start_at(1_000, 500);
        assert_eq!(clock.now_ms(), 1_000);
        assert_eq!(clock.slot(), 500);

        let advance = clock.advance_to(501, 1_400);
        assert_eq!(advance.at_ms, 1_400);
        assert_eq!(advance.slot, 501);
        assert!(!advance.clamped);
        assert!(!advance.slot_regressed);
        assert_eq!(clock.now_ms(), 1_400);
        assert_eq!(clock.slot(), 501);
    }

    #[test]
    fn replay_clock_never_walks_time_backwards() {
        let clock = ReplayClock::start_at(10_000, 100);

        // A provider whose block time is behind the one before it.
        let advance = clock.advance_to(101, 9_600);
        assert!(advance.clamped, "a backwards timestamp must be clamped");
        assert_eq!(advance.at_ms, 10_000, "the clock stays where it was");
        assert_eq!(clock.now_ms(), 10_000);
        assert_eq!(clock.clamped(), 1, "and the clamp is counted, not hidden");

        // A slot that regresses is counted separately: it is a different fault.
        let advance = clock.advance_to(99, 10_500);
        assert!(advance.slot_regressed);
        assert_eq!(clock.slot(), 101);
        assert_eq!(clock.slot_regressions(), 1);
        assert_eq!(clock.advances(), 2);
    }

    #[test]
    fn replay_clock_instant_is_the_same_virtual_timeline() {
        let clock = ReplayClock::start_at(1_000, 1);
        let first = clock.instant();
        clock.advance_to(2, 1_250);
        let second = clock.instant();

        // 250 ms of fixture time, regardless of how long the host took.
        assert_eq!(
            second.saturating_duration_since(first),
            Duration::from_millis(250)
        );
    }

    #[test]
    fn replay_clock_survives_a_pre_epoch_fixture() {
        let clock = ReplayClock::start_at(-5, 0);
        assert_eq!(clock.instant().as_micros(), 0, "no wrap, no panic");
    }

    #[test]
    fn system_clock_slot_is_monotonic() {
        let clock = SystemClock::new();
        clock.observe_slot(500);
        clock.observe_slot(499);
        assert_eq!(clock.slot(), 500);
        clock.observe_slot(501);
        assert_eq!(clock.slot(), 501);
        assert!(clock.now_ms() > 1_700_000_000_000, "a real wall clock");
    }

    // -----------------------------------------------------------------------
    // fixture helpers
    // -----------------------------------------------------------------------

    const STREAM: &str = "phase3-test";

    fn frame_bytes(account: &str) -> Vec<u8> {
        format!(r#"{{"method":"programNotification","account":"{account}"}}"#).into_bytes()
    }

    fn draft(
        slot: u64,
        at_ms: i64,
        provider: &str,
        connection: u32,
        kind: RecordKind,
        frame: Option<Vec<u8>>,
        outcome: RecordOutcome,
    ) -> RecordDraft {
        RecordDraft {
            event_id: format!("{provider}-{slot}-{connection}"),
            slot,
            observed_at_ms: at_ms,
            provider: provider.to_string(),
            endpoint_index: 0,
            connection,
            kind,
            frame,
            outcome,
            dispatch_latency_us: Some(412),
        }
    }

    /// A stream with two providers, a reconnect, a pong, and one drop of each
    /// interesting class.
    fn sample_stream() -> (Vec<ReplayRecord>, [u8; 32]) {
        let mut writer = ChainWriter::new(STREAM);
        let records = vec![
            writer.seal(draft(
                1_000,
                1_700_000_000_000,
                "helius",
                0,
                RecordKind::Connected,
                None,
                RecordOutcome::Accepted,
            )),
            writer.seal(draft(
                1_000,
                1_700_000_000_010,
                "helius",
                0,
                RecordKind::Frame,
                Some(frame_bytes("curve-a")),
                RecordOutcome::Accepted,
            )),
            writer.seal(draft(
                1_000,
                1_700_000_000_020,
                "quicknode",
                0,
                RecordKind::Frame,
                Some(frame_bytes("curve-a")),
                RecordOutcome::Dropped(DropClass::StaleSlot),
            )),
            writer.seal(draft(
                1_001,
                1_700_000_000_400,
                "helius",
                0,
                RecordKind::Pong,
                None,
                RecordOutcome::Accepted,
            )),
            writer.seal(draft(
                1_002,
                1_700_000_000_800,
                "helius",
                0,
                RecordKind::Frame,
                Some(frame_bytes("curve-b")),
                RecordOutcome::Backpressure(Queue::FastPath),
            )),
            writer.seal(draft(
                1_003,
                1_700_000_001_200,
                "helius",
                1,
                RecordKind::Frame,
                Some(frame_bytes("curve-c")),
                RecordOutcome::Dropped(DropClass::NotAllowlisted),
            )),
        ];
        let head = writer.head();
        (records, head)
    }

    // -----------------------------------------------------------------------
    // section 3 - the record and its chain
    // -----------------------------------------------------------------------

    #[test]
    fn canonical_bytes_are_the_documented_field_order() {
        let mut writer = ChainWriter::new(STREAM);
        let record = writer.seal(draft(
            7,
            1_234,
            "helius",
            0,
            RecordKind::Frame,
            Some(b"hi".to_vec()),
            RecordOutcome::Accepted,
        ));

        let canonical = String::from_utf8(record.canonical_bytes()).unwrap();
        let expected_prefix = concat!(
            r#"{"schema":"sts.replay.v1","event_id":"helius-7-0","seq":0,"slot":7,"#,
            r#""observed_at_ms":1234,"provider":"helius","endpoint_index":0,"#,
            r#""connection":0,"kind":"frame","frame_b64":"aGk=","frame_len":2,"#,
        );
        assert!(
            canonical.starts_with(expected_prefix),
            "field order drifted:\n{canonical}"
        );
        assert!(canonical.contains(r#""outcome":"accepted""#));
        assert!(canonical.contains(r#""dispatch_latency_us":412"#));
        assert!(
            !canonical.contains("integrity_hash"),
            "the chain is computed over everything except its own link"
        );
        assert!(
            !canonical.contains(' '),
            "canonical form carries no whitespace"
        );
    }

    #[test]
    fn canonical_bytes_escape_exactly_what_json_requires() {
        // Built rather than written literally, so this file carries no control
        // byte of its own.
        let input = format!("a\"b\\c\nd\te{}f", char::from(0x01u8));
        let mut out = String::new();
        json_escape(&input, &mut out);
        assert_eq!(
            out, r#""a\"b\\c\nd\te\u0001f""#,
            "control characters take the \\u form"
        );

        let mut plain = String::new();
        json_escape("hello there", &mut plain);
        assert_eq!(
            plain, "\"hello there\"",
            "printable characters are never escaped"
        );
    }

    #[test]
    fn a_record_with_no_frame_still_has_a_digest_and_a_length() {
        let mut writer = ChainWriter::new(STREAM);
        let record = writer.seal(draft(
            1,
            1,
            "helius",
            0,
            RecordKind::Pong,
            None,
            RecordOutcome::Accepted,
        ));
        assert_eq!(record.frame_len(), 0);
        assert_eq!(record.frame_sha256(), sha256_hex(b""));
        assert!(record.frame_b64().is_none());
    }

    #[test]
    fn the_chain_starts_at_the_stream_id_and_links_forward() {
        let (records, head) = sample_stream();

        assert_eq!(records[0].prev_hash, genesis_hash(STREAM));
        for window in records.windows(2) {
            assert_eq!(
                window[1].prev_hash, window[0].integrity_hash,
                "record {} does not follow record {}",
                window[1].seq, window[0].seq
            );
        }
        assert_eq!(head, records.last().unwrap().integrity_hash);
        assert!(records.iter().all(|r| r.verify_integrity()));
    }

    #[test]
    fn seq_is_dense_from_zero() {
        let (records, _) = sample_stream();
        for (index, record) in records.iter().enumerate() {
            assert_eq!(record.seq, index as u64);
        }
    }

    /// R9. A single changed byte anywhere in a record must break verification.
    #[test]
    fn a_single_byte_edit_breaks_the_chain() {
        let (records, _) = sample_stream();

        let mut edited = records[1].clone();
        edited.slot += 1;
        assert!(!edited.verify_integrity(), "slot");

        let mut edited = records[1].clone();
        edited.observed_at_ms += 1;
        assert!(!edited.verify_integrity(), "observed_at_ms");

        let mut edited = records[1].clone();
        edited.outcome = RecordOutcome::Dropped(DropClass::TooSmall);
        assert!(!edited.verify_integrity(), "outcome");

        let mut edited = records[1].clone();
        if let Some(frame) = edited.frame.as_mut() {
            frame[0] ^= 0x01;
        }
        assert!(!edited.verify_integrity(), "one bit of one frame byte");

        let mut edited = records[1].clone();
        edited.dispatch_latency_us = Some(413);
        assert!(!edited.verify_integrity(), "dispatch_latency_us");

        let mut edited = records[1].clone();
        edited.provider = "quicknode".to_string();
        assert!(!edited.verify_integrity(), "provider");
    }

    #[test]
    fn an_edit_to_a_middle_record_is_caught_by_the_cursor() {
        let (mut records, _) = sample_stream();
        records[2].slot += 1;
        records[2].integrity_hash = records[2].compute_integrity(&records[2].prev_hash);
        // The record now verifies on its own - and the link to record 3 does not.
        assert!(records[2].verify_integrity());

        let error = ReplayCursor::open(STREAM, records).unwrap_err();
        assert!(
            matches!(error, ReplayError::ChainBroken { seq: 3, .. }),
            "expected the break to be reported at record 3, got {error}"
        );
    }

    // -----------------------------------------------------------------------
    // section 3 - JSONL
    // -----------------------------------------------------------------------

    #[test]
    fn records_round_trip_through_jsonl() {
        let (records, _) = sample_stream();
        let text = write_stream(&records);
        let parsed = parse_stream(&text).expect("the stream we just wrote must parse");
        assert_eq!(parsed, records);
    }

    #[test]
    fn a_line_is_its_canonical_form_plus_the_link() {
        let (records, _) = sample_stream();
        let line = records[1].to_line();
        let canonical = String::from_utf8(records[1].canonical_bytes()).unwrap();
        assert!(line.starts_with(canonical.trim_end_matches('}')));
        assert!(line.ends_with(&format!(
            r#","integrity_hash":"{}"}}"#,
            hex(&records[1].integrity_hash)
        )));
    }

    #[test]
    fn parsing_refuses_what_it_cannot_stand_behind() {
        let (records, _) = sample_stream();
        let good = records[1].to_line();

        assert!(matches!(
            from_line("not json", 1),
            Err(ReplayError::NotAnObject { line: 1 })
        ));
        assert!(matches!(
            from_line("[1,2,3]", 1),
            Err(ReplayError::NotAnObject { line: 1 })
        ));
        assert!(matches!(
            from_line(&good.replace("sts.replay.v1", "sts.replay.v2"), 4),
            Err(ReplayError::WrongSchema { line: 4, .. })
        ));
        assert!(matches!(
            from_line(&good.replace(r#""slot":1000,"#, ""), 5),
            Err(ReplayError::MissingField {
                line: 5,
                field: "slot"
            })
        ));
        assert!(matches!(
            from_line(
                &good.replace(r#""kind":"frame""#, r#""kind":"telepathy""#),
                6
            ),
            Err(ReplayError::BadField { field: "kind", .. })
        ));
        assert!(matches!(
            from_line(
                &good.replace(r#""outcome":"accepted""#, r#""outcome":"maybe""#),
                7
            ),
            Err(ReplayError::BadField {
                field: "outcome",
                ..
            })
        ));
    }

    /// The redundant frame fields are checked rather than trusted: a redundant
    /// field nobody verifies is a field that silently drifts.
    #[test]
    fn a_frame_that_disagrees_with_its_digest_is_rejected() {
        let (records, _) = sample_stream();
        let line = records[1].to_line();

        let wrong_len = line.replace(
            &format!(r#""frame_len":{}"#, records[1].frame_len()),
            r#""frame_len":9999"#,
        );
        assert!(matches!(
            from_line(&wrong_len, 1),
            Err(ReplayError::FrameMismatch { .. })
        ));

        let wrong_digest = line.replace(&records[1].frame_sha256(), &sha256_hex(b"something else"));
        assert!(matches!(
            from_line(&wrong_digest, 1),
            Err(ReplayError::FrameMismatch { .. })
        ));
    }

    #[test]
    fn blank_lines_are_skipped() {
        let (records, _) = sample_stream();
        let text = format!("\n{}\n\n", write_stream(&records));
        assert_eq!(parse_stream(&text).unwrap().len(), records.len());
    }

    // -----------------------------------------------------------------------
    // section 6 - the total order
    // -----------------------------------------------------------------------

    #[test]
    fn provider_rank_follows_the_feed_provider_array() {
        assert_eq!(provider_rank("helius"), 0);
        assert_eq!(provider_rank("quicknode"), 1);
        assert_eq!(provider_rank("triton"), 2);
        assert_eq!(
            provider_rank("some-future-provider"),
            u16::MAX,
            "an unknown provider sorts last rather than panicking"
        );
    }

    #[test]
    fn the_order_key_puts_slot_before_arrival() {
        let earlier_slot = OrderKey {
            slot: 10,
            provider_rank: 2,
            endpoint_index: 9,
            connection: 9,
            seq: 999,
        };
        let later_slot = OrderKey {
            slot: 11,
            provider_rank: 0,
            endpoint_index: 0,
            connection: 0,
            seq: 0,
        };
        assert!(
            earlier_slot < later_slot,
            "slot dominates every other field"
        );

        let helius = OrderKey {
            slot: 10,
            provider_rank: 0,
            endpoint_index: 0,
            connection: 0,
            seq: 5,
        };
        let quicknode = OrderKey {
            provider_rank: 1,
            ..helius
        };
        assert!(helius < quicknode, "within a slot, provider rank decides");
    }

    #[test]
    fn the_cursor_refuses_a_stream_that_is_out_of_order() {
        // Two records in the same slot, the lower-ranked provider second.
        let mut writer = ChainWriter::new(STREAM);
        let records = vec![
            writer.seal(draft(
                5,
                1,
                "quicknode",
                0,
                RecordKind::Frame,
                Some(b"a".to_vec()),
                RecordOutcome::Accepted,
            )),
            writer.seal(draft(
                5,
                2,
                "helius",
                0,
                RecordKind::Frame,
                Some(b"b".to_vec()),
                RecordOutcome::Accepted,
            )),
        ];

        let error = ReplayCursor::open(STREAM, records).unwrap_err();
        assert!(
            matches!(error, ReplayError::OutOfOrder { seq: 1, .. }),
            "got {error}"
        );
    }

    #[test]
    fn the_cursor_refuses_a_hole() {
        let (mut records, _) = sample_stream();
        records.remove(2);
        let error = ReplayCursor::open(STREAM, records).unwrap_err();
        assert!(matches!(error, ReplayError::SeqGap { .. }), "got {error}");
    }

    #[test]
    fn the_cursor_refuses_a_stream_from_another_id() {
        let (records, _) = sample_stream();
        let error = ReplayCursor::open("a-different-stream", records).unwrap_err();
        assert!(
            matches!(error, ReplayError::ChainBroken { seq: 0, .. }),
            "the genesis link is the stream's identity, got {error}"
        );
    }

    #[test]
    fn the_cursor_refuses_an_empty_stream() {
        assert_eq!(
            ReplayCursor::open(STREAM, Vec::new()).unwrap_err(),
            ReplayError::Empty
        );
    }

    // -----------------------------------------------------------------------
    // section 9 - forward-only reading
    // -----------------------------------------------------------------------

    #[test]
    fn the_cursor_yields_every_record_once_and_then_stops() {
        let (records, head) = sample_stream();
        let mut cursor = ReplayCursor::open(STREAM, records.clone()).unwrap();

        assert_eq!(cursor.remaining(), records.len());
        let mut seen = Vec::new();
        while let Some(record) = cursor.next() {
            seen.push(record.seq);
        }
        assert_eq!(seen, (0..records.len() as u64).collect::<Vec<_>>());
        assert!(cursor.is_exhausted());
        assert_eq!(cursor.remaining(), 0);
        assert!(cursor.next().is_none(), "and stays exhausted");
        assert_eq!(cursor.chain_head(), head);
    }

    /// R1 in miniature: the same fixture walked twice produces the same bytes.
    #[test]
    fn two_walks_of_one_fixture_are_identical() {
        let (records, _) = sample_stream();
        let mut cursor = ReplayCursor::open(STREAM, records).unwrap();

        let mut first = String::new();
        while let Some(record) = cursor.next() {
            first.push_str(&record.to_line());
        }
        cursor.restart();
        let mut second = String::new();
        while let Some(record) = cursor.next() {
            second.push_str(&record.to_line());
        }
        assert_eq!(first, second);
    }

    #[test]
    fn the_driver_advances_the_clock_before_delivering() {
        let (records, _) = sample_stream();
        let cursor = ReplayCursor::open(STREAM, records).unwrap();
        let mut driver = ReplayDriver::new(cursor);

        let mut delivered = 0;
        while let Some((advance, record)) = driver.step() {
            assert_eq!(
                advance.at_ms, record.observed_at_ms,
                "the clock is at the record being handled"
            );
            assert_eq!(advance.slot, record.slot);
            delivered += 1;
        }
        assert_eq!(delivered, 6);
        assert_eq!(driver.clock().advances(), 6);
        assert_eq!(
            driver.clock().clamped(),
            0,
            "the sample stream is well ordered"
        );
        assert_eq!(driver.clock().now_ms(), 1_700_000_001_200);
        assert_eq!(driver.clock().slot(), 1_003);
    }

    // -----------------------------------------------------------------------
    // section 3.2 - the manifest
    // -----------------------------------------------------------------------

    #[test]
    fn a_manifest_describes_the_stream_it_was_built_from() {
        let (records, head) = sample_stream();
        let manifest = Manifest::for_records(STREAM, &records, head, 1_700_000_002_000);

        assert_eq!(manifest.schema, MANIFEST_SCHEMA);
        assert_eq!(manifest.record_count, 6);
        assert_eq!(manifest.frame_count, 4);
        assert_eq!(manifest.first_slot, 1_000);
        assert_eq!(manifest.last_slot, 1_003);
        assert_eq!(manifest.chain_head, hex(&head));
        assert_eq!(manifest.providers, vec!["helius", "quicknode"]);
        assert!(manifest.complete);
        assert!(manifest.gate_ready().is_ok());
    }

    /// R10. An incomplete recording may be replayed for debugging and may never
    /// back a gate dossier.
    #[test]
    fn an_incomplete_fixture_is_refused_for_a_gate_run() {
        let (records, head) = sample_stream();
        let mut manifest = Manifest::for_records(STREAM, &records, head, 0);
        manifest.complete = false;

        let error = manifest.gate_ready().unwrap_err();
        assert!(
            matches!(error, ReplayError::Incomplete { .. }),
            "got {error}"
        );
    }

    #[test]
    fn coverage_gaps_are_visible_to_anyone_computing_a_cohort() {
        let (records, head) = sample_stream();
        let mut manifest = Manifest::for_records(STREAM, &records, head, 0);
        manifest.coverage.push(CoverageGap {
            from_ms: 2_000,
            to_ms: 3_000,
            gap_reason: "disconnect".to_string(),
        });

        assert!(
            manifest.covers(0, 2_000),
            "a window that ends at the gap is covered"
        );
        assert!(
            !manifest.covers(1_500, 2_500),
            "a window that overlaps is not"
        );
        assert!(!manifest.covers(2_100, 2_200), "nor one inside it");
        assert!(
            manifest.covers(3_000, 4_000),
            "a window that starts at the end is"
        );
    }

    #[test]
    fn a_manifest_survives_json() {
        let (records, head) = sample_stream();
        let manifest = Manifest::for_records(STREAM, &records, head, 42);
        let text = serde_json::to_string(&manifest).unwrap();
        let parsed: Manifest = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, manifest);
    }

    // -----------------------------------------------------------------------
    // section 5.1 - fidelity
    // -----------------------------------------------------------------------

    #[test]
    fn outcomes_round_trip_through_their_wire_form() {
        let mut all = vec![RecordOutcome::Accepted];
        all.extend(DropClass::ALL.map(RecordOutcome::Dropped));
        all.extend(Queue::ALL.map(RecordOutcome::Backpressure));

        for outcome in all {
            let encoded = outcome.encode();
            assert_eq!(RecordOutcome::parse(&encoded), Some(outcome), "{encoded}");
        }
        assert_eq!(RecordOutcome::parse("dropped:not_a_reason"), None);
        assert_eq!(RecordOutcome::parse("backpressure:not_a_queue"), None);
        assert_eq!(RecordOutcome::parse(""), None);
    }

    #[test]
    fn drop_class_strings_match_the_ingestion_vocabulary() {
        // These are the strings `ingestion::DropReason::as_str` produces. A
        // recorder writes one and this reads the other, so they have to agree.
        assert_eq!(DropClass::NotAllowlisted.as_str(), "not_allowlisted");
        assert_eq!(DropClass::NotANotification.as_str(), "not_a_notification");
        assert_eq!(DropClass::Undecodable.as_str(), "undecodable");
        assert_eq!(DropClass::NoDecoder.as_str(), "no_decoder");
        assert_eq!(DropClass::TooSmall.as_str(), "too_small");
        assert_eq!(DropClass::LotterySlot.as_str(), "lottery_slot");
        assert_eq!(DropClass::StaleSlot.as_str(), "stale_slot");
        assert_eq!(DropClass::PoolTooThin.as_str(), "pool_too_thin");
        assert_eq!(DropClass::Graduated.as_str(), "graduated");
    }

    #[test]
    fn agreement_is_the_ordinary_case() {
        let (records, _) = sample_stream();
        let mut report = FidelityReport::new();
        for record in &records {
            report.observe(record, record.outcome);
        }
        assert_eq!(report.compared, 6);
        assert_eq!(report.agreed, 6);
        assert!(report.tolerated.is_empty());
        assert!(report.passes());
    }

    #[test]
    fn a_backpressure_drop_that_replay_accepted_is_tolerated() {
        let (records, _) = sample_stream();
        let backpressured = records
            .iter()
            .find(|r| r.outcome.is_backpressure())
            .expect("the sample stream has one");

        let mut report = FidelityReport::new();
        report.observe(backpressured, RecordOutcome::Accepted);

        assert_eq!(report.tolerated.len(), 1);
        assert!(report.failures.is_empty());
        assert!(
            report.passes(),
            "serialised delivery drops fewer frames than live did; that is the design"
        );
    }

    #[test]
    fn a_filtering_disagreement_fails_the_run() {
        let (records, _) = sample_stream();
        let filtered = records
            .iter()
            .find(|r| r.outcome == RecordOutcome::Dropped(DropClass::NotAllowlisted))
            .unwrap();

        let mut report = FidelityReport::new();
        report.observe(filtered, RecordOutcome::Accepted);

        assert_eq!(report.failures.len(), 1);
        assert!(report.tolerated.is_empty());
        assert!(
            !report.passes(),
            "a filtering bug may not hide in a backpressure total"
        );
        assert_eq!(report.failures[0].recorded, filtered.outcome);
        assert_eq!(report.failures[0].replayed, RecordOutcome::Accepted);
        assert_eq!(report.failures[0].event_id, filtered.event_id);
    }

    #[test]
    fn replay_dropping_more_than_live_is_a_failure_not_a_tolerance() {
        let (records, _) = sample_stream();
        let accepted = records.iter().find(|r| r.outcome.is_accepted()).unwrap();

        let mut report = FidelityReport::new();
        report.observe(accepted, RecordOutcome::Backpressure(Queue::Standard));
        assert!(
            !report.passes(),
            "serialised delivery cannot drop what live kept; if it did, something is wrong"
        );
    }

    // -----------------------------------------------------------------------
    // section 19 - addressed draws
    // -----------------------------------------------------------------------

    /// R19. Every draw is reproducible from its address alone.
    #[test]
    fn a_draw_is_a_function_of_its_address() {
        let source = DrawSource::new("0x100x");
        let again = DrawSource::new("0x100x");

        for index in 0..32 {
            assert_eq!(
                source.raw("corr-1", "gap_bucket", index),
                again.raw("corr-1", "gap_bucket", index),
                "index {index}"
            );
        }
    }

    #[test]
    fn a_different_seed_gives_different_draws() {
        let a = DrawSource::new("0x100x");
        let b = DrawSource::new("0xdead");
        assert_ne!(a.raw("corr", "land", 0), b.raw("corr", "land", 0));
    }

    #[test]
    fn draws_do_not_depend_on_the_order_or_count_of_other_draws() {
        let source = DrawSource::new("seed");

        // Draw the same address after doing wildly different amounts of other
        // work. A sequential generator would give a different answer here, which
        // is the entire reason this one is addressed.
        let direct = source.raw("corr-a", "land", 3);
        for index in 0..1_000 {
            let _ = source.raw("corr-b", "delta_slot_flow", index);
        }
        assert_eq!(source.raw("corr-a", "land", 3), direct);
    }

    /// The length prefixes the specification does not mention, and why.
    #[test]
    fn draw_addresses_cannot_collide_across_the_field_boundary() {
        let source = DrawSource::new("seed");
        assert_ne!(
            source.raw("ab", "c", 0),
            source.raw("a", "bc", 0),
            "plain concatenation would make these one address"
        );
        assert_ne!(source.raw("", "abc", 0), source.raw("abc", "", 0));
    }

    #[test]
    fn unit_draws_stay_inside_the_half_open_interval() {
        let source = DrawSource::new("seed");
        let mut sum = 0.0;
        for index in 0..2_000 {
            let value = source.unit("corr", "unit", index);
            assert!((0.0..1.0).contains(&value), "draw {index} was {value}");
            sum += value;
        }
        let mean = sum / 2_000.0;
        assert!(
            (0.4..0.6).contains(&mean),
            "mean {mean} is not plausibly uniform"
        );
    }

    #[test]
    fn bounded_draws_stay_below_their_bound() {
        let source = DrawSource::new("seed");
        for n in [1u64, 2, 7, 100, u64::MAX] {
            for index in 0..64 {
                assert!(
                    source.below("corr", "below", index, n) < n,
                    "n={n} index={index}"
                );
            }
        }
        assert_eq!(source.below("corr", "below", 0, 0), 0, "no bound, no draw");
    }

    #[test]
    fn buckets_respect_their_weights() {
        let source = DrawSource::new("seed");
        // 30% / 70%, in basis points.
        let weights = [3_000u16, 7_000];
        let mut first = 0;
        for index in 0..4_000 {
            if source.bucket("corr", "gap_bucket", index, &weights) == 0 {
                first += 1;
            }
        }
        let share = f64::from(first) / 4_000.0;
        assert!((0.27..0.33).contains(&share), "first bucket took {share}");

        assert_eq!(source.bucket("corr", "empty", 0, &[]), 0);
    }

    // -----------------------------------------------------------------------
    // section 11 - the curve
    // -----------------------------------------------------------------------

    #[test]
    fn the_launch_state_is_the_protocol_parameters() {
        let curve = CurveState::LAUNCH;
        assert_eq!(curve.virtual_token_reserves, 1_073_000_000_000_000);
        assert_eq!(curve.virtual_sol_reserves, 30 * SOL);
        assert_eq!(curve.real_token_reserves, 793_100_000_000_000);
        assert_eq!(curve.real_sol_reserves, 0);
        assert!(curve.is_plausible());
        assert!(!curve.complete);
        assert_eq!(curve.market_cap_lamports(), 27_958_993_476);
        assert_eq!(curve.progress_bps(), 0);
    }

    #[test]
    fn deriving_the_launch_state_from_the_invariant_reproduces_it() {
        assert_eq!(CurveState::at_real_sol(0), CurveState::LAUNCH);
    }

    /// The check that the model and `PUMP_GRADUATION_LAMPORTS` have not drifted
    /// apart. Selling the entire real token reserve should leave the curve at
    /// the graduation constant, and it does to within 0.007%.
    #[test]
    fn selling_out_the_real_reserve_lands_on_the_graduation_constant() {
        let sold_out =
            CurveState::LAUNCH.virtual_token_reserves - CurveState::LAUNCH.real_token_reserves;
        let k = CurveState::LAUNCH.k();
        let virtual_sol = (k / u128::from(sold_out)) as u64;
        let real_sol = virtual_sol - LAUNCH_VIRTUAL_SOL_RESERVES;

        assert_eq!(virtual_sol, 115_005_359_056);
        assert_eq!(real_sol, 85_005_359_056);
        assert!(
            real_sol.abs_diff(PUMP_GRADUATION_LAMPORTS) < 6_000_000,
            "the curve parameters and the graduation constant disagree by {} lamports",
            real_sol.abs_diff(PUMP_GRADUATION_LAMPORTS)
        );
    }

    /// The band table from section 11.1 of the specification.
    #[test]
    fn the_curve_walks_the_documented_band() {
        let expected: [(u64, u64, u64, u64); 7] = [
            //  real SOL,  virtual token reserves,  virtual SOL,  market cap
            (0, 1_073_000_000_000_000, 30_000_000_000, 27_958_993_476),
            (10, 804_750_000_000_000, 40_000_000_000, 49_704_877_291),
            (33, 510_952_380_952_380, 63_000_000_000, 123_299_161_230),
            (45, 429_200_000_000_000, 75_000_000_000, 174_743_709_226),
            (60, 357_666_666_666_666, 90_000_000_000, 251_630_941_286),
            (70, 321_900_000_000_000, 100_000_000_000, 310_655_483_069),
            (85, 279_913_043_478_260, 115_000_000_000, 410_841_876_359),
        ];

        for (real_sol, tokens, sol, cap) in expected {
            let curve = CurveState::at_real_sol(real_sol * SOL);
            assert_eq!(
                curve.virtual_token_reserves, tokens,
                "tokens at {real_sol} SOL"
            );
            assert_eq!(curve.virtual_sol_reserves, sol, "sol at {real_sol} SOL");
            assert_eq!(curve.market_cap_lamports(), cap, "cap at {real_sol} SOL");
        }
    }

    #[test]
    fn the_target_band_is_the_upper_half_of_the_curve() {
        // The strategy trades $25k to $80k. At SOL near $200 that is roughly
        // 125 to 410 SOL of market cap, which is 33 SOL of real reserves to
        // graduation.
        let low = CurveState::at_real_sol(33 * SOL);
        let high = CurveState::at_real_sol(85 * SOL);
        assert!(low.market_cap_lamports() > 120 * SOL);
        assert!(high.market_cap_lamports() < 420 * SOL);
        assert!(low.progress_bps() > 3_800 && low.progress_bps() < 3_900);
    }

    #[test]
    fn progress_is_measured_against_the_graduation_constant() {
        assert_eq!(CurveState::at_real_sol(0).progress_bps(), 0);
        assert_eq!(
            CurveState::at_real_sol(PUMP_GRADUATION_LAMPORTS / 2).progress_bps(),
            5_000
        );
        let done = CurveState::at_real_sol(PUMP_GRADUATION_LAMPORTS);
        assert!(done.complete);
        assert_eq!(done.progress_bps(), 10_000);
    }

    // -----------------------------------------------------------------------
    // section 12 - slippage
    // -----------------------------------------------------------------------

    /// The exact buy vectors from section 12.4.
    #[test]
    fn buy_vectors_are_exact() {
        let expected: [(u64, u64, u16); 6] = [
            (10_000_000, 353_973_188_847, 104),
            (100_000_000, 3_529_253_463_570, 133),
            (500_000_000, 17_417_117_560_255, 261),
            (1_000_000_000, 34_277_831_558_567, 417),
            (3_000_000_000, 96_657_870_791_628, 992),
            (10_000_000_000, 266_233_082_706_766, 2_557),
        ];

        for (gross, tokens, bps) in expected {
            let fill = CurveState::LAUNCH
                .quote_buy(gross, DEFAULT_FEE_BPS)
                .expect("the launch curve prices every one of these");
            assert_eq!(fill.tokens, tokens, "tokens for {gross} lamports");
            assert_eq!(fill.slippage_bps, bps, "slippage for {gross} lamports");
            assert_eq!(fill.gross_lamports, gross);
            assert_eq!(fill.fee_lamports + fill.net_lamports, gross);
        }
    }

    /// The exact sell vectors from section 12.4, at 45 SOL of real reserves.
    #[test]
    fn sell_for_target_vectors_are_exact() {
        let curve = CurveState::at_real_sol(45 * SOL);
        assert_eq!(curve.virtual_token_reserves, 429_200_000_000_000);
        assert_eq!(curve.virtual_sol_reserves, 75_000_000_000);

        let expected: [(u64, u64, u16); 5] = [
            (225_000_000, 1_304_559_268_947, 130),
            (675_000_000, 3_937_614_674_131, 190),
            (1_350_000_000, 7_948_148_144_371, 280),
            (2_250_000_000, 13_412_499_995_574, 400),
            (4_500_000_000, 27_690_322_577_698, 700),
        ];

        for (target, tokens, bps) in expected {
            let (sized, fill) = curve
                .sell_tokens_for_target(target, DEFAULT_FEE_BPS)
                .expect("every one of these is inside the real reserve");
            assert_eq!(sized, tokens, "tokens to realise {target} lamports");
            assert_eq!(fill.slippage_bps, bps, "slippage to realise {target}");
            assert!(
                fill.net_lamports >= target,
                "the sizer must reach the target, got {} for {target}",
                fill.net_lamports
            );
            // And it must be the *smallest* such size.
            let smaller = curve.quote_sell(sized - 1, DEFAULT_FEE_BPS).unwrap();
            assert!(
                smaller.net_lamports < target,
                "one base unit less must fall short"
            );
        }
    }

    /// R13. The round trip costs twice the fee at every point on the curve.
    #[test]
    fn a_round_trip_costs_two_fees_wherever_it_is_taken() {
        for real_sol in [10u64, 33, 45, 60, 80] {
            let curve = CurveState::at_real_sol(real_sol * SOL);
            let size = curve.max_position_lamports(DEFAULT_MAX_POOL_SHARE_BPS);
            let cost = round_trip_bps(&curve, size, DEFAULT_FEE_BPS).unwrap();
            assert_eq!(
                cost, 199,
                "round trip at {real_sol} SOL of real reserves cost {cost} bps"
            );
        }

        // The specification's section 12.5 table quotes a round trip at 85 SOL
        // of real reserves. Two things stop that being reachable, and both are
        // the model working rather than failing.
        //
        // At exactly the graduation constant the curve is complete, so section
        // 17's hard branch refuses to quote it at all.
        assert!(CurveState::at_real_sol(85 * SOL).complete);

        // And one SOL short of it there is not enough of the real token reserve
        // left to fill a position sized at 1.5% of the real SOL. The last SOL of
        // a curve can only buy the tokens that are still in it.
        let nearly = CurveState::at_real_sol(84 * SOL);
        let size = nearly.max_position_lamports(DEFAULT_MAX_POOL_SHARE_BPS);
        assert!(matches!(
            nearly.quote_buy(size, DEFAULT_FEE_BPS),
            Err(QuoteError::ExceedsRealTokens { .. })
        ));

        // 83 SOL is the last whole SOL where a cap-sized round trip still fits.
        assert_eq!(
            round_trip_bps(
                &CurveState::at_real_sol(83 * SOL),
                CurveState::at_real_sol(83 * SOL).max_position_lamports(DEFAULT_MAX_POOL_SHARE_BPS),
                DEFAULT_FEE_BPS
            ),
            Ok(199)
        );
    }

    #[test]
    fn the_round_trip_is_two_fees_at_other_sizes_too() {
        let curve = CurveState::at_real_sol(45 * SOL);
        for size in [1_000_000u64, 10_000_000, 100_000_000, 675_000_000] {
            let cost = round_trip_bps(&curve, size, DEFAULT_FEE_BPS).unwrap();
            assert!(
                (198..=200).contains(&cost),
                "size {size} cost {cost} bps, which is not two fees"
            );
        }
    }

    #[test]
    fn the_constant_product_survives_a_swap() {
        let curve = CurveState::at_real_sol(45 * SOL);
        let before = curve.k();
        let fill = curve.quote_buy(500_000_000, DEFAULT_FEE_BPS).unwrap();
        let after = curve.after_buy(&fill).k();

        // The fee is taken outside the curve, so k only moves by the floor in
        // the token division - never by the fee.
        let drift = before.abs_diff(after);
        let relative = drift * 1_000_000_000 / before;
        assert!(relative < 10, "k moved by {relative} parts per billion");
        assert!(after >= before, "flooring the token output can only grow k");
    }

    #[test]
    fn slippage_is_the_fee_at_vanishing_size_and_approaches_one_at_infinity() {
        assert_eq!(
            slippage_bps(0, 30_000_000_000, DEFAULT_FEE_BPS),
            DEFAULT_FEE_BPS,
            "an infinitesimal order pays the fee and nothing else"
        );
        assert_eq!(
            slippage_bps(u128::from(u64::MAX), 1, DEFAULT_FEE_BPS),
            10_000,
            "an unbounded order gets asymptotically nothing"
        );
        assert_eq!(slippage_bps(1, 1, 0), 5_000, "half the pool, no fee");
    }

    /// R12. Total for every input, including the ones no curve can hold.
    #[test]
    fn slippage_never_panics_or_overflows() {
        let extremes = [
            0u128,
            1,
            u128::from(u64::MAX),
            u128::from(u64::MAX) * 2,
            u128::MAX / 2,
            u128::MAX,
        ];
        for &num in &extremes {
            for &den in &extremes {
                let bps = slippage_bps(num, den, DEFAULT_FEE_BPS);
                assert!(bps <= 10_000, "num={num} den={den} produced {bps}");
            }
        }
    }

    #[test]
    fn slippage_rounds_against_the_trader() {
        // w = 1/3 of the reserve with no fee is exactly 2500 bps; a hair more
        // must round up rather than truncate down.
        assert_eq!(slippage_bps(1, 3, 0), 2_500);
        assert_eq!(slippage_bps(1_000_001, 3_000_000, 0), 2_501);
    }

    // -----------------------------------------------------------------------
    // section 13 - the participation cap
    // -----------------------------------------------------------------------

    #[test]
    fn the_participation_cap_is_taken_against_executable_liquidity() {
        let curve = CurveState::at_real_sol(45 * SOL);
        assert_eq!(
            curve.max_position_lamports(DEFAULT_MAX_POOL_SHARE_BPS),
            675_000_000,
            "1.5% of 45 SOL of real reserves"
        );
        assert!(
            curve.max_position_lamports(DEFAULT_MAX_POOL_SHARE_BPS)
                < curve.market_cap_lamports() / 100,
            "sizing off market cap would be several times larger"
        );
        assert_eq!(
            CurveState::LAUNCH.max_position_lamports(DEFAULT_MAX_POOL_SHARE_BPS),
            0,
            "a curve with no real SOL supports no position at all"
        );
    }

    // -----------------------------------------------------------------------
    // section 17 - the branches a dead pool takes
    // -----------------------------------------------------------------------

    /// R18. A complete curve is never quoted.
    #[test]
    fn a_graduated_curve_is_never_quoted() {
        let mut curve = CurveState::at_real_sol(45 * SOL);
        curve.complete = true;

        assert_eq!(
            curve.quote_buy(100_000_000, DEFAULT_FEE_BPS),
            Err(QuoteError::CurveComplete)
        );
        assert_eq!(
            curve.quote_sell(1_000_000_000, DEFAULT_FEE_BPS),
            Err(QuoteError::CurveComplete)
        );
        assert_eq!(
            curve.sell_tokens_for_target(1_000, DEFAULT_FEE_BPS),
            Err(QuoteError::CurveComplete)
        );
        assert_eq!(
            round_trip_bps(&curve, 100_000, DEFAULT_FEE_BPS),
            Err(QuoteError::CurveComplete)
        );
    }

    /// The first of the no-executable-exit conditions: the curve cannot pay.
    #[test]
    fn an_exit_larger_than_the_real_reserve_has_no_route() {
        let curve = CurveState::at_real_sol(5 * SOL);

        let error = curve
            .sell_tokens_for_target(6 * SOL, DEFAULT_FEE_BPS)
            .unwrap_err();
        assert!(
            matches!(error, QuoteError::ExceedsRealSol { available, .. } if available == 5 * SOL),
            "got {error}"
        );

        // And the same through the token-sized door.
        let error = curve
            .quote_sell(curve.virtual_token_reserves / 2, DEFAULT_FEE_BPS)
            .unwrap_err();
        assert!(
            matches!(error, QuoteError::ExceedsRealSol { .. }),
            "got {error}"
        );
    }

    #[test]
    fn a_buy_larger_than_the_real_token_reserve_has_no_route() {
        let curve = CurveState::at_real_sol(84 * SOL);
        let error = curve.quote_buy(50 * SOL, DEFAULT_FEE_BPS).unwrap_err();
        assert!(
            matches!(error, QuoteError::ExceedsRealTokens { .. }),
            "got {error}"
        );
    }

    #[test]
    fn implausible_and_empty_orders_are_refused_rather_than_guessed_at() {
        let empty = CurveState::from_parts(0, 0, 0, 0, 0, false);
        assert_eq!(
            empty.quote_buy(1_000, DEFAULT_FEE_BPS),
            Err(QuoteError::Implausible)
        );
        assert_eq!(empty.market_cap_lamports(), 0);

        let curve = CurveState::at_real_sol(45 * SOL);
        assert_eq!(
            curve.quote_buy(0, DEFAULT_FEE_BPS),
            Err(QuoteError::ZeroSize)
        );
        assert_eq!(
            curve.quote_sell(0, DEFAULT_FEE_BPS),
            Err(QuoteError::ZeroSize)
        );
        assert_eq!(
            curve.sell_tokens_for_target(0, DEFAULT_FEE_BPS),
            Err(QuoteError::ZeroSize)
        );
    }

    #[test]
    fn a_buy_and_its_sell_return_the_reserves_to_where_they_were() {
        let curve = CurveState::at_real_sol(45 * SOL);
        let buy = curve.quote_buy(675_000_000, DEFAULT_FEE_BPS).unwrap();
        let after_buy = curve.after_buy(&buy);
        let sell = after_buy.quote_sell(buy.tokens, DEFAULT_FEE_BPS).unwrap();
        let after_sell = after_buy.after_sell(&sell);

        assert!(
            after_sell
                .virtual_sol_reserves
                .abs_diff(curve.virtual_sol_reserves)
                < 2,
            "virtual SOL came back to within a lamport"
        );
        assert!(
            after_sell
                .virtual_token_reserves
                .abs_diff(curve.virtual_token_reserves)
                < 2,
            "and so did the tokens"
        );
    }

    // -----------------------------------------------------------------------
    // section 14 - displacement
    // -----------------------------------------------------------------------

    #[test]
    fn no_displacement_costs_nothing() {
        let curve = CurveState::at_real_sol(45 * SOL);
        assert_eq!(
            displacement_damage_bps(&curve, 0, 675_000_000, DEFAULT_FEE_BPS).unwrap(),
            0
        );
    }

    /// Section 14.2's bound: being displaced by a fraction of the SOL reserve
    /// costs between one and two times that fraction.
    #[test]
    fn displacement_costs_between_one_and_two_deltas() {
        let curve = CurveState::at_real_sol(45 * SOL);
        let size = curve.max_position_lamports(DEFAULT_MAX_POOL_SHARE_BPS);

        for flow_sol in [1u64, 2, 5, 10] {
            let flow = (flow_sol * SOL) as i64;
            let delta_bps = i32::try_from(
                u128::from(flow as u64) * u128::from(BPS_DENOMINATOR)
                    / u128::from(curve.virtual_sol_reserves),
            )
            .unwrap();

            let damage = displacement_damage_bps(&curve, flow, size, DEFAULT_FEE_BPS).unwrap();
            assert!(
                damage >= delta_bps && damage <= 2 * delta_bps + 1,
                "{flow_sol} SOL of displacement is {delta_bps} bps of reserve and cost {damage} bps"
            );
        }
    }

    #[test]
    fn displacement_the_other_way_is_a_discount_not_a_cost() {
        let curve = CurveState::at_real_sol(45 * SOL);
        let size = curve.max_position_lamports(DEFAULT_MAX_POOL_SHARE_BPS);

        let damage = displacement_damage_bps(&curve, -(2 * SOL as i64), size, DEFAULT_FEE_BPS)
            .expect("the curve holds 45 SOL, so 2 SOL of net selling is representable");
        assert!(
            damage < 0,
            "net selling ahead of a buy helps it, got {damage}"
        );
    }

    #[test]
    fn displacement_beyond_the_real_reserve_is_not_representable() {
        let curve = CurveState::at_real_sol(5 * SOL);
        assert!(
            curve.displaced(-(6 * SOL as i64)).is_none(),
            "more SOL cannot leave the pool than is in it"
        );
    }

    // -----------------------------------------------------------------------
    // section 15 - sandwich extraction
    // -----------------------------------------------------------------------

    /// R16. The closed form and the three-swap simulation agree.
    ///
    /// Not to the lamport, and the specification's R16 overstates that: the
    /// simulation floors at four separate divisions and the closed form is one
    /// exact rational, so a residual of a few lamports is arithmetic rather than
    /// disagreement. It is bounded, and it always falls in the attacker's
    /// disfavour, which is the direction that cannot flatter a backtest.
    #[test]
    fn the_closed_form_matches_the_three_swaps() {
        let curve = CurveState::at_real_sol(45 * SOL);

        for (attacker, victim) in [
            (100_000_000u64, 500_000_000u64),
            (250_000_000, 500_000_000),
            (500_000_000, 1_000_000_000),
            (1_000_000_000, 2_000_000_000),
            (3_000_000_000, 5_000_000_000),
        ] {
            let simulated =
                simulate_sandwich(&curve, attacker, victim, DEFAULT_FEE_BPS, 0).unwrap();
            let attacker_net = attacker - attacker * u64::from(DEFAULT_FEE_BPS) / 10_000;
            let victim_net = victim - victim * u64::from(DEFAULT_FEE_BPS) / 10_000;
            let closed =
                sandwich_extraction_closed(curve.virtual_sol_reserves, attacker_net, victim_net)
                    .expect("well inside u128");

            let difference = (simulated.extraction_lamports - closed as i64).abs();
            assert!(
                difference <= 8,
                "a={attacker} b={victim}: simulated {} vs closed {closed}, off by {difference}",
                simulated.extraction_lamports
            );
        }
    }

    /// R14. Extraction is strictly bounded by the victim's fee-adjusted spend.
    #[test]
    fn a_sandwich_can_never_take_more_than_the_victim_put_in() {
        let y = 75 * SOL;
        for attacker_net in [
            1_000_000u64,
            100_000_000,
            1_000_000_000,
            50 * SOL,
            5_000 * SOL,
        ] {
            for victim_net in [1_000_000u64, 100_000_000, 1_000_000_000, 10 * SOL] {
                let extraction =
                    sandwich_extraction_closed(y, attacker_net, victim_net).expect("inside u128");
                assert!(
                    extraction < victim_net,
                    "a={attacker_net} b={victim_net} extracted {extraction}"
                );
            }
        }
    }

    #[test]
    fn extraction_grows_with_both_sides_and_vanishes_without_a_victim() {
        let y = 75 * SOL;
        assert_eq!(sandwich_extraction_closed(y, 500_000_000, 0), Some(0));
        assert_eq!(sandwich_extraction_closed(y, 0, 500_000_000), Some(0));

        let small = sandwich_extraction_closed(y, 100_000_000, 500_000_000).unwrap();
        let bigger_attacker = sandwich_extraction_closed(y, 200_000_000, 500_000_000).unwrap();
        let bigger_victim = sandwich_extraction_closed(y, 100_000_000, 1_000_000_000).unwrap();
        assert!(bigger_attacker > small);
        assert!(bigger_victim > small);
    }

    #[test]
    fn extraction_approaches_twice_the_product_at_small_sizes() {
        // E is about 2·A·B/Y when both sides are small against the reserve.
        let y = 75 * SOL;
        let a = 99_000_000u64;
        let b = 495_000_000u64;
        let exact = sandwich_extraction_closed(y, a, b).unwrap();
        let approximation = 2 * u128::from(a) * u128::from(b) / u128::from(y);
        let drift = (exact as i128 - approximation as i128).unsigned_abs();
        assert!(
            drift * 100 / u128::from(exact) < 5,
            "the small-size approximation is {approximation} against an exact {exact}"
        );
    }

    /// The exact break-even thresholds from section 15.2.
    #[test]
    fn the_breakeven_threshold_is_the_documented_value() {
        assert_eq!(
            sandwich_breakeven_victim_lamports(30 * SOL, DEFAULT_FEE_BPS),
            306_091_216
        );
        assert_eq!(
            sandwich_breakeven_victim_lamports(75 * SOL, DEFAULT_FEE_BPS),
            765_228_038
        );
        assert_eq!(
            sandwich_breakeven_victim_lamports(115 * SOL, DEFAULT_FEE_BPS),
            1_173_349_659
        );
        assert_eq!(
            sandwich_breakeven_victim_lamports(75 * SOL, 0),
            0,
            "with no fee there is no threshold to clear"
        );
    }

    /// R15. Below the threshold, no front-run of any size is profitable, before
    /// any landing cost at all.
    ///
    /// Strictly below. At exactly the threshold the profit derivative is zero,
    /// so the true edge is zero and the integer floors decide the last lamport
    /// in either direction; asserting a sign there would be asserting the
    /// rounding.
    #[test]
    fn no_front_run_is_profitable_below_the_threshold() {
        for real_sol in [0u64, 45, 60] {
            let curve = CurveState::at_real_sol(real_sol * SOL);
            let threshold =
                sandwich_breakeven_victim_lamports(curve.virtual_sol_reserves, DEFAULT_FEE_BPS);

            for share in [50u64, 80, 95, 99] {
                let victim = threshold * share / 100;
                let best = best_front_run(&curve, victim, DEFAULT_FEE_BPS, 0, 40 * SOL, 200);
                assert!(
                    best.is_none(),
                    "at {real_sol} SOL a victim buy of {victim} ({share}% of the threshold) \
                     was sandwiched for {:?}",
                    best.map(|(size, s)| (size, s.attacker_profit_lamports))
                );
            }
        }
    }

    #[test]
    fn above_the_threshold_a_front_run_appears() {
        for real_sol in [0u64, 45, 60] {
            let curve = CurveState::at_real_sol(real_sol * SOL);
            let threshold =
                sandwich_breakeven_victim_lamports(curve.virtual_sol_reserves, DEFAULT_FEE_BPS);

            for share in [110u64, 130, 200] {
                let victim = threshold * share / 100;
                let (size, sandwich) =
                    best_front_run(&curve, victim, DEFAULT_FEE_BPS, 0, 40 * SOL, 200)
                        .unwrap_or_else(|| {
                            panic!(
                                "at {real_sol} SOL, {victim} lamports ({share}% of the \
                                 threshold) should be sandwichable"
                            )
                        });
                assert!(size >= MIN_VIABLE_ATTACKER_LAMPORTS);
                assert!(sandwich.attacker_profit_lamports > 0);
                assert!(sandwich.victim_damage_bps > 0);
            }
        }
    }

    #[test]
    fn a_landing_cost_raises_the_bar() {
        let curve = CurveState::at_real_sol(45 * SOL);
        let victim =
            sandwich_breakeven_victim_lamports(curve.virtual_sol_reserves, DEFAULT_FEE_BPS) * 105
                / 100;

        let free = best_front_run(&curve, victim, DEFAULT_FEE_BPS, 0, 40 * SOL, 200);
        let costly = best_front_run(&curve, victim, DEFAULT_FEE_BPS, 5_000_000, 40 * SOL, 200);
        assert!(free.is_some(), "just above the threshold and free to land");
        assert!(
            costly.is_none(),
            "a 0.005 SOL landing cost is more than the edge just above the threshold"
        );
    }

    /// The adverse-selection figures from section 15.3, against an attacker
    /// limited to one SOL of working capital.
    #[test]
    fn adverse_selection_in_the_target_band_is_a_few_hundred_basis_points() {
        let cost = 5_000_000u64; // 0.005 SOL
        let capital = SOL;

        let cases: [(u64, u64, u16, u16); 4] = [
            //  real SOL,  our buy,   damage floor, damage ceiling
            (10, 1_000_000_000, 440, 500),
            (45, 1_000_000_000, 230, 290),
            (45, 2_000_000_000, 230, 290),
            (70, 2_000_000_000, 170, 220),
        ];

        for (real_sol, our_buy, floor, ceiling) in cases {
            let curve = CurveState::at_real_sol(real_sol * SOL);
            let (_, sandwich) =
                best_front_run(&curve, our_buy, DEFAULT_FEE_BPS, cost, capital, 200)
                    .unwrap_or_else(|| {
                        panic!("a 1 SOL attacker should take {our_buy} at {real_sol}")
                    });
            assert!(
                (floor..=ceiling).contains(&sandwich.victim_damage_bps),
                "at {real_sol} SOL a {our_buy} lamport buy lost {} bps, outside {floor}..{ceiling}",
                sandwich.victim_damage_bps
            );
        }
    }

    #[test]
    fn a_sandwich_damages_the_victim_by_about_twice_the_front_run() {
        let curve = CurveState::at_real_sol(45 * SOL);
        let attacker = 500_000_000u64;
        let victim = 1_000_000_000u64;

        let sandwich = simulate_sandwich(&curve, attacker, victim, DEFAULT_FEE_BPS, 0).unwrap();

        // alpha = fee-adjusted attacker size over the virtual reserve.
        let alpha_bps =
            (attacker - attacker / 100) * u64::from(BPS_DENOMINATOR) / curve.virtual_sol_reserves;
        assert!(
            u64::from(sandwich.victim_damage_bps).abs_diff(2 * alpha_bps) <= 5,
            "damage {} bps against 2 alpha of {} bps",
            sandwich.victim_damage_bps,
            2 * alpha_bps
        );
        assert!(sandwich.victim_tokens < sandwich.victim_tokens_solo);
    }

    #[test]
    fn best_front_run_is_a_grid_and_therefore_reproducible() {
        let curve = CurveState::at_real_sol(45 * SOL);
        let victim = 2 * SOL;

        let first = best_front_run(&curve, victim, DEFAULT_FEE_BPS, 0, 10 * SOL, 128);
        let second = best_front_run(&curve, victim, DEFAULT_FEE_BPS, 0, 10 * SOL, 128);
        assert_eq!(first, second);

        assert!(best_front_run(&curve, victim, DEFAULT_FEE_BPS, 0, 0, 128).is_none());
        assert!(best_front_run(&curve, victim, DEFAULT_FEE_BPS, 0, 10 * SOL, 0).is_none());
    }

    #[test]
    fn a_sandwich_on_a_curve_with_no_real_sol_still_settles() {
        // The attacker's exit is funded by their own and the victim's deposits,
        // so an empty real reserve is not what stops a sandwich at launch. The
        // fee threshold is.
        let curve = CurveState::LAUNCH;
        let victim = SOL;
        let sandwich = simulate_sandwich(&curve, 500_000_000, victim, DEFAULT_FEE_BPS, 0)
            .expect("the back-run clears against the deposits made ahead of it");
        assert!(sandwich.attacker_out_lamports > 0);
    }

    // -----------------------------------------------------------------------
    // section 18 - the cost stack
    // -----------------------------------------------------------------------

    #[test]
    fn the_default_split_is_the_default_fee() {
        let split = FeeSplit::default();
        assert_eq!(split.total_bps, DEFAULT_FEE_BPS);
        assert_eq!(split.protocol_bps + split.creator_bps, split.total_bps);
        assert_eq!(split.protocol_bps, DEFAULT_PROTOCOL_FEE_BPS);
        assert_eq!(split.creator_bps, DEFAULT_CREATOR_FEE_BPS);
    }

    #[test]
    fn a_split_that_does_not_add_up_is_not_a_split() {
        // There is no constructor that takes a total and two parts, precisely so
        // that there is nothing to disagree with: the total is the sum.
        let split = FeeSplit::new(95, 5).expect("95 and 5 make a fee");
        assert_eq!(split.total_bps, 100);

        // A fee of a hundred percent or more is not a fee, on either part.
        assert_eq!(FeeSplit::new(10_000, 0), None);
        assert_eq!(FeeSplit::new(9_999, 1), None);
        assert_eq!(FeeSplit::new(u16::MAX, 1), None);

        // And the whole thing to the venue is a legal schedule — Raydium, and
        // pump.fun before creator revenue existed.
        let raydium = FeeSplit::protocol_only(25).expect("25 bps is a fee");
        assert_eq!((raydium.protocol_bps, raydium.creator_bps), (25, 0));
    }

    /// The property the whole decomposition exists to have: it decomposes. Two
    /// parts, one charged number, and never a lamport of drift between them.
    #[test]
    fn the_parts_always_sum_to_what_the_fill_was_charged() {
        let split = FeeSplit::default();
        let curve = CurveState::LAUNCH;
        for gross in [1u64, 7, 999, 10_000, 12_345_678, SOL / 2, SOL, 40 * SOL] {
            let fill = curve
                .quote_buy(gross, split.total_bps)
                .expect("the curve quotes it");
            let fees = split.of(&fill);
            assert!(fees.balances(), "gross {gross}: {fees:?} does not add up");
            assert_eq!(
                fees.total_lamports, fill.fee_lamports,
                "gross {gross}: the decomposition invented a fee"
            );
            assert_eq!(fees.gross_lamports, fill.gross_lamports);
        }
    }

    /// Both legs, and the sell side too — the fee comes off the SOL leg either
    /// way and the split does not know which side it is on.
    #[test]
    fn a_sell_leg_decomposes_the_same_way() {
        let split = FeeSplit::default();
        let curve = CurveState::at_real_sol(40 * SOL);
        let buy = curve.quote_buy(SOL, split.total_bps).expect("quotes");
        let sell = curve
            .quote_sell(buy.tokens, split.total_bps)
            .expect("quotes");

        let fees = split.of(&sell);
        assert!(fees.balances());
        assert_eq!(fees.total_lamports, sell.fee_lamports);
        assert!(
            fees.creator_lamports > 0,
            "a whole SOL of sale is not below the dust line"
        );
        assert!(
            fees.protocol_lamports > fees.creator_lamports,
            "95 is more than 5"
        );
    }

    /// Nineteen to one, because ninety-five is nineteen fives. The creator's
    /// share is floored the way the program floors its own, and the protocol
    /// takes the remainder — so the ratio holds to within that one floor.
    #[test]
    fn the_venue_takes_nineteen_lamports_for_every_one_the_creator_takes() {
        let split = FeeSplit::default();
        // A gross that divides cleanly: 10^6 lamports is 100 of total fee, 5 to
        // the creator and 95 to the venue, with nothing left over.
        let fees = split.decompose(1_000_000, 10_000);
        assert_eq!(fees.creator_lamports, 500);
        assert_eq!(fees.protocol_lamports, 9_500);
        assert_eq!(fees.protocol_lamports, 19 * fees.creator_lamports);
        assert!(fees.balances());
    }

    /// Where the rounding dust goes, said out loud, because "somewhere" is not
    /// an answer a report can be audited against.
    #[test]
    fn the_rounding_dust_goes_to_the_venue() {
        let split = FeeSplit::default();
        // 199 lamports of gross: the creator's floor is 0, the total floor is 1.
        let fees = split.decompose(199, 1);
        assert_eq!((fees.protocol_lamports, fees.creator_lamports), (1, 0));
        assert!(fees.balances());

        // And a fee too small for either floor is still decomposed, into zero.
        let nothing = split.decompose(3, 0);
        assert_eq!(
            (nothing.protocol_lamports, nothing.creator_lamports),
            (0, 0)
        );
        assert!(nothing.balances());
    }

    /// A caller who hands in a fee that did not come from this schedule still
    /// gets a decomposition that adds up. It has to: the sum is what the trade
    /// was charged, and the parts are an explanation of it.
    #[test]
    fn a_fee_smaller_than_the_creator_share_still_decomposes() {
        let split = FeeSplit::default();
        let fees = split.decompose(SOL, 3);
        assert_eq!(fees.total_lamports, 3);
        assert_eq!(fees.creator_lamports, 3);
        assert_eq!(fees.protocol_lamports, 0);
        assert!(fees.balances());
    }

    #[test]
    fn splitting_the_fee_does_not_move_a_quoted_lamport() {
        // The whole safety argument for the split: a fill sees `φ` and only
        // `φ`, so decomposing it is a report change and never a price change.
        let curve = CurveState::at_real_sol(20 * SOL);
        let lumped = curve.quote_buy(SOL, DEFAULT_FEE_BPS).expect("quotes");
        let split = FeeSplit::default();
        let after = curve.quote_buy(SOL, split.total_bps).expect("quotes");
        assert_eq!(lumped, after);
    }

    #[test]
    fn the_priority_fee_is_the_budget_rounded_up() {
        // 10 000 micro-lamports over 200 000 units is exactly 2 000 lamports.
        assert_eq!(priority_fee_lamports(10_000, 200_000), 2_000);
        // One micro-lamport over one unit is a millionth of a lamport, and the
        // trader pays a whole one for it.
        assert_eq!(priority_fee_lamports(1, 1), 1);
        assert_eq!(priority_fee_lamports(0, 200_000), 0);
        assert_eq!(priority_fee_lamports(1_000_000, 1), 1);
        // Rounds up rather than to nearest: 1 999 999 micro-lamports is two
        // lamports of cost, not one.
        assert_eq!(priority_fee_lamports(1_999_999, 1), 2);
        // And it saturates rather than wrapping.
        assert_eq!(priority_fee_lamports(u64::MAX, u32::MAX), u64::MAX);
    }

    #[test]
    fn the_network_costs_are_the_rows_of_the_table() {
        let costs = TransactionCosts::new(1, 10_000, 200_000, 0, 25_000);
        assert_eq!(costs.base_lamports, BASE_SIGNATURE_FEE_LAMPORTS);
        assert_eq!(costs.priority_lamports, 2_000);
        assert_eq!(costs.rent_lamports, 0);
        assert_eq!(costs.tip_lamports, 25_000);
        assert_eq!(costs.total_lamports(), 5_000 + 2_000 + 25_000);

        // An entry that has to open an associated account carries the deposit.
        let entry = TransactionCosts::new(1, 10_000, 200_000, 1, 0);
        assert_eq!(entry.rent_lamports, TOKEN_ACCOUNT_RENT_LAMPORTS);
        assert_eq!(
            entry.total_lamports(),
            5_000 + 2_000 + TOKEN_ACCOUNT_RENT_LAMPORTS
        );

        // Zero signatures is not a transaction; one is the floor.
        assert_eq!(TransactionCosts::new(0, 0, 0, 0, 0).signatures, 1);
        assert_eq!(TransactionCosts::new(2, 0, 0, 0, 0).base_lamports, 10_000);
    }

    /// The row that is easy to forget and expensive to forget: a transaction
    /// that executed and reverted was not free.
    #[test]
    fn a_failed_transaction_costs_the_base_and_the_priority_and_nothing_else() {
        let costs = TransactionCosts::new(1, 10_000, 200_000, 1, 25_000);
        assert_eq!(costs.failed_lamports(), 7_000);
        assert!(costs.failed_lamports() < costs.total_lamports());
        // The tip and the rent are the two that do not survive a revert, and
        // both are in-band: the transfer reverted with everything else.
        assert_eq!(
            costs.total_lamports() - costs.failed_lamports(),
            costs.tip_lamports + costs.rent_lamports
        );
    }

    #[test]
    fn the_whole_stack_adds_the_curve_to_the_network() {
        let split = FeeSplit::default();
        let curve = CurveState::at_real_sol(40 * SOL);
        let fill = curve.quote_buy(SOL, split.total_bps).expect("quotes");
        let stack = CostStack {
            swap: split.of(&fill),
            transaction: TransactionCosts::new(1, 10_000, 200_000, 1, 25_000),
        };

        assert_eq!(
            stack.total_lamports(),
            fill.fee_lamports + 5_000 + 2_000 + TOKEN_ACCOUNT_RENT_LAMPORTS + 25_000
        );
        // A swap that never happened was charged no proportional fee.
        assert_eq!(stack.failed_lamports(), 7_000);
        assert!(stack.swap.balances());
    }

    #[test]
    fn the_cost_stack_survives_the_wire_in_camel_case() {
        let split = FeeSplit::default();
        let stack = CostStack {
            swap: split.decompose(1_000_000, 10_000),
            transaction: TransactionCosts::new(1, 10_000, 200_000, 1, 25_000),
        };
        let json = serde_json::to_value(stack).expect("it serialises");
        assert_eq!(json["swap"]["creatorLamports"], 500);
        assert_eq!(json["swap"]["totalLamports"], 10_000);
        assert_eq!(json["transaction"]["priorityLamports"], 2_000);
        assert_eq!(
            json["transaction"]["rentLamports"],
            TOKEN_ACCOUNT_RENT_LAMPORTS
        );

        let back: CostStack = serde_json::from_value(json).expect("it reads back");
        assert_eq!(back, stack);
    }

    // -----------------------------------------------------------------------
    // determinism across the whole module
    // -----------------------------------------------------------------------

    #[test]
    fn the_simulator_is_a_pure_function_of_its_inputs() {
        let curve = CurveState::at_real_sol(45 * SOL);
        let source = DrawSource::new("0x100x");

        let run = || {
            let mut ledger = String::new();
            for index in 0..64u64 {
                let flow =
                    source.below("corr", "delta_slot_flow", index, 4 * SOL) as i64 - 2 * SOL as i64;
                let size = curve.max_position_lamports(DEFAULT_MAX_POOL_SHARE_BPS);
                let damage = displacement_damage_bps(&curve, flow, size, DEFAULT_FEE_BPS)
                    .unwrap_or(i32::MAX);
                let fill = curve.quote_buy(size, DEFAULT_FEE_BPS).unwrap();
                ledger.push_str(&format!("{index}:{flow}:{damage}:{}\n", fill.tokens));
            }
            ledger
        };

        assert_eq!(
            run(),
            run(),
            "two runs of one seed must agree byte for byte"
        );
    }

    // -----------------------------------------------------------------------
    // §5 — the session the window drives
    // -----------------------------------------------------------------------

    /// A scratch fixture directory, cleared going in as well as coming out, so
    /// a test that panicked last run does not poison the next one.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("sts-replay-session/{name}"));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("the scratch directory could not be created");
            Scratch { path }
        }

        fn write(&self, name: &str, text: &str) {
            fs::write(self.path.join(name), text).expect("the fixture could not be written");
        }

        /// One segment and the manifest describing it.
        fn write_fixture(&self, records: &[ReplayRecord], stream_id: &str, complete: bool) {
            self.write("000.jsonl", &write_stream(records));
            let mut manifest = Manifest::for_records(
                stream_id,
                records,
                records.last().expect("a record").integrity_hash,
                1_700_000_000_000,
            );
            manifest.complete = complete;
            self.write(
                "manifest.json",
                &serde_json::to_string(&manifest).expect("the manifest serialises"),
            );
        }

        fn session(&self) -> ReplaySession {
            ReplaySession::new(self.path.clone())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// `count` records, one slot apart, `gap_ms` of recorded time between each.
    ///
    /// Written out rather than taken from `sample_stream` because most of what
    /// is asserted below is arithmetic on those gaps, and a fixture whose
    /// timing is produced by the same code that produces the expectation is not
    /// a fixture.
    fn timed_stream(stream_id: &str, count: u64, gap_ms: i64) -> Vec<ReplayRecord> {
        let mut writer = ChainWriter::new(stream_id);
        (0..count)
            .map(|index| {
                writer.seal(draft(
                    1_000 + index,
                    1_700_000_000_000 + index as i64 * gap_ms,
                    "helius",
                    0,
                    RecordKind::Frame,
                    Some(frame_bytes("curve-a")),
                    RecordOutcome::Accepted,
                ))
            })
            .collect()
    }

    #[test]
    fn a_session_with_no_directory_answers_rather_than_guessing() {
        let session = ReplaySession::new(std::env::temp_dir().join("sts-replay-session/absent"));
        let status = session.status();

        assert!(!status.active);
        assert_eq!(status.stream_id, None, "nothing is guessed from the path");
        assert_eq!(status.chain_verified, None, "and nothing was checked");
        assert_eq!(status.record_count, 0);
        assert_eq!(
            status.speed,
            ReplaySpeed::Real,
            "the multiplier that cannot outrun the recording is the default"
        );
    }

    #[test]
    fn starting_without_a_fixture_names_the_directory_it_looked_in() {
        let path = std::env::temp_dir().join("sts-replay-session/still-absent");
        let session = ReplaySession::new(path.clone());

        let err = session.start().expect_err("there is nothing there to play");
        assert_eq!(
            err,
            SessionError::Missing {
                path: path.display().to_string()
            }
        );
        assert!(
            err.to_string().contains(&path.display().to_string()),
            "the sentence says where it looked: {err}"
        );
        assert!(
            !session.is_active(),
            "a refused start does not leave replay on"
        );
    }

    #[test]
    fn a_directory_with_no_segments_is_not_a_fixture() {
        let scratch = Scratch::new("empty-dir");
        let err = scratch.session().start().expect_err("no .jsonl in there");
        assert!(matches!(err, SessionError::NoSegments { .. }), "{err:?}");
    }

    #[test]
    fn several_segments_with_no_manifest_are_refused_rather_than_guessed_at() {
        let scratch = Scratch::new("ambiguous");
        let records = timed_stream("phase3-ambiguous", 4, 400);
        scratch.write("000.jsonl", &write_stream(&records[..2]));
        scratch.write("001.jsonl", &write_stream(&records[2..]));

        let err = scratch
            .session()
            .start()
            .expect_err("which stream are these?");
        assert_eq!(
            err,
            SessionError::Ambiguous {
                path: scratch.path.display().to_string(),
                files: 2
            }
        );
    }

    #[test]
    fn one_loose_segment_plays_but_is_never_called_verified() {
        let scratch = Scratch::new("loose-segment");
        let records = timed_stream("loose", 6, 400);
        scratch.write("loose.jsonl", &write_stream(&records));

        let status = scratch.session().start().expect("one file is unambiguous");
        assert_eq!(
            status.stream_id.as_deref(),
            Some("loose"),
            "the file stem is the stream id"
        );
        assert_eq!(
            status.chain_verified, None,
            "the links agree with each other, but nothing said what the head should be"
        );
        assert_eq!(
            status.fixture_complete, None,
            "and nothing said whether it finished"
        );
        assert_eq!(status.record_count, 6);
    }

    #[test]
    fn a_manifest_makes_the_head_checkable_and_the_check_is_reported() {
        let scratch = Scratch::new("verified");
        let records = timed_stream("phase3-verified", 8, 250);
        scratch.write_fixture(&records, "phase3-verified", true);

        let status = scratch.session().start().expect("a clean fixture");
        assert_eq!(status.chain_verified, Some(true));
        assert_eq!(status.fixture_complete, Some(true));
        assert_eq!(
            status.chain_head.as_deref(),
            Some(hex(&records.last().expect("a record").integrity_hash).as_str())
        );
        assert_eq!(status.first_slot, Some(1_000));
        assert_eq!(status.last_slot, Some(1_007));
    }

    #[test]
    fn a_manifest_the_records_do_not_compute_to_reads_as_broken_and_still_plays() {
        let scratch = Scratch::new("head-disagrees");
        let records = timed_stream("phase3-tampered", 8, 250);
        scratch.write_fixture(&records, "phase3-tampered", true);

        // The links still chain to each other. What has changed is the document
        // saying what they should have chained to — which is exactly the case
        // where the records are playable and the fixture is not evidence.
        let mut manifest = Manifest::for_records(
            "phase3-tampered",
            &records,
            records.last().expect("a record").integrity_hash,
            1_700_000_000_000,
        );
        manifest.chain_head = hex(&genesis_hash("somebody else"));
        scratch.write(
            "manifest.json",
            &serde_json::to_string(&manifest).expect("serialises"),
        );

        let status = scratch
            .session()
            .start()
            .expect("the records themselves are fine");
        assert_eq!(status.chain_verified, Some(false));
        assert!(
            status.active,
            "it plays: refusing would show the operator nothing at all"
        );
    }

    #[test]
    fn a_recording_that_was_cut_short_says_so() {
        let scratch = Scratch::new("incomplete");
        let records = timed_stream("phase3-cut", 5, 200);
        scratch.write_fixture(&records, "phase3-cut", false);

        let status = scratch
            .session()
            .start()
            .expect("an incomplete fixture still replays");
        assert_eq!(status.fixture_complete, Some(false));
        assert_eq!(
            status.chain_verified,
            Some(true),
            "every link that exists still verifies"
        );
    }

    #[test]
    fn a_broken_chain_refuses_to_open_at_all() {
        let scratch = Scratch::new("broken-chain");
        let records = timed_stream("phase3-broken", 6, 200);
        // A record taken out of the middle. Every hash after the hole now
        // points at something that is not in front of it.
        let mut kept = records.clone();
        kept.remove(3);
        // Named for the stream it is a segment of, so the genesis link is right
        // and the hole is the only thing wrong with it.
        scratch.write("phase3-broken.jsonl", &write_stream(&kept));

        let err = scratch
            .session()
            .start()
            .expect_err("this is not replayable");
        assert!(
            matches!(
                err,
                SessionError::Fixture {
                    source: ReplayError::SeqGap { .. },
                    ..
                }
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("not replayable"), "{err}");
    }

    #[test]
    fn a_manifest_that_will_not_parse_is_named_rather_than_ignored() {
        let scratch = Scratch::new("bad-manifest");
        scratch.write("000.jsonl", &write_stream(&timed_stream("x", 3, 100)));
        scratch.write("manifest.json", "{\"schema\":\"sts.replay.manifest.v9\"}");

        let err = scratch
            .session()
            .start()
            .expect_err("the manifest is the thing that says what this is");
        assert!(matches!(err, SessionError::Io { .. }), "{err:?}");
        assert!(err.to_string().contains("manifest.json"), "{err}");
    }

    #[test]
    fn a_stopped_session_plays_nothing_however_often_it_is_ticked() {
        let scratch = Scratch::new("stopped");
        let records = timed_stream("phase3-stopped", 20, 100);
        scratch.write_fixture(&records, "phase3-stopped", true);
        let session = scratch.session();

        for _ in 0..8 {
            let status = session.advance(250);
            assert_eq!(status.records_played, 0, "nobody pressed anything");
            assert!(!status.active);
        }
    }

    #[test]
    fn one_x_plays_a_second_of_recording_for_a_second_of_wall_clock() {
        let scratch = Scratch::new("real-time");
        // Ten records a second, for eight seconds.
        let records = timed_stream("phase3-rate", 80, 100);
        scratch.write_fixture(&records, "phase3-rate", true);
        let session = scratch.session();
        session.start().expect("a clean fixture");

        // One record to set the clock, then a second of recording on top of it.
        let after_one = session.advance(1_000);
        assert_eq!(
            after_one.records_played, 11,
            "a second of it, not the whole fixture"
        );

        let after_two = session.advance(1_000);
        assert_eq!(after_two.records_played, 21);
        assert_eq!(
            after_two.slot, 1_020,
            "and the slot clock is the fixture's, not the host's"
        );
    }

    #[test]
    fn ten_x_buys_ten_times_as_much_recording_for_the_same_wall_clock() {
        let scratch = Scratch::new("ten-x");
        let records = timed_stream("phase3-fast", 80, 100);
        scratch.write_fixture(&records, "phase3-fast", true);
        let session = scratch.session();
        session.start().expect("a clean fixture");
        session.set_speed(ReplaySpeed::Ten);

        let status = session.advance(1_000);
        assert_eq!(
            status.records_played, 80,
            "ten seconds of an eight-second recording is all of it"
        );
        assert_eq!(
            status.speed,
            ReplaySpeed::Ten,
            "and the chip the engine reports is the one that was set"
        );
    }

    #[test]
    fn max_is_bounded_by_the_budget_rather_than_by_the_clock() {
        let scratch = Scratch::new("max");
        // A day between records. At 1x this fixture would take a fortnight.
        let records = timed_stream("phase3-sparse", 12, 86_400_000);
        scratch.write_fixture(&records, "phase3-sparse", true);
        let session = scratch.session();
        session.start().expect("a clean fixture");
        session.set_speed(ReplaySpeed::Max);

        let status = session.advance(250);
        assert_eq!(status.records_played, 12, "the whole fixture, in one tick");
        assert_eq!(status.record_count, 12);

        // The end of the fixture is the end. Ticking past it is not an error and
        // does not wrap round to the beginning.
        let after = session.advance(250);
        assert_eq!(after.records_played, 12);
    }

    #[test]
    fn a_sparse_fixture_at_one_x_makes_progress_rather_than_stalling() {
        let scratch = Scratch::new("sparse-real-time");
        let records = timed_stream("phase3-sparse-1x", 4, 86_400_000);
        scratch.write_fixture(&records, "phase3-sparse-1x", true);
        let session = scratch.session();
        session.start().expect("a clean fixture");

        // The first record sets the clock rather than being charged for the
        // fifty-odd years between the epoch and the recording; the second is the
        // one record of overspend the budget check allows. A day-wide gap then
        // costs one record per tick rather than stalling the playhead forever,
        // which is the whole reason the budget is checked before the step and
        // not after it.
        let first = session.advance(250);
        assert_eq!(first.records_played, 2);
        assert_eq!(first.slot, 1_001);

        let second = session.advance(250);
        assert_eq!(second.records_played, 3, "still moving, one gap at a time");
    }

    #[test]
    fn entering_replay_twice_replays_the_fixture_rather_than_the_rest_of_it() {
        let scratch = Scratch::new("restart");
        let records = timed_stream("phase3-twice", 40, 100);
        scratch.write_fixture(&records, "phase3-twice", true);
        let session = scratch.session();

        session.start().expect("a clean fixture");
        let first = session.advance(1_000);
        assert_eq!(first.records_played, 11);

        session.stop();
        assert_eq!(
            session.status().records_played,
            11,
            "stopping leaves the playhead where it stopped"
        );

        session.start().expect("the fixture is already open");
        assert_eq!(
            session.status().records_played,
            0,
            "and starting rewinds it"
        );

        let second = session.advance(1_000);
        assert_eq!(
            second.records_played, first.records_played,
            "a second run of one fixture is the first one again"
        );
        assert_eq!(second.slot, first.slot);
        assert_eq!(second.clamped, first.clamped);
    }

    #[test]
    fn the_clamp_counters_are_the_clocks_own_and_they_reach_the_window() {
        let scratch = Scratch::new("clamped");
        let mut writer = ChainWriter::new("phase3-clamped");
        // Two providers on one slot. Helius sorts first and is a hundred
        // milliseconds later, so the record behind it arrives with a timestamp
        // the clock has already passed.
        let records = vec![
            writer.seal(draft(
                2_000,
                1_700_000_000_500,
                "helius",
                0,
                RecordKind::Frame,
                Some(frame_bytes("curve-a")),
                RecordOutcome::Accepted,
            )),
            writer.seal(draft(
                2_000,
                1_700_000_000_400,
                "quicknode",
                0,
                RecordKind::Frame,
                Some(frame_bytes("curve-a")),
                RecordOutcome::Accepted,
            )),
        ];
        scratch.write_fixture(&records, "phase3-clamped", true);

        let session = scratch.session();
        session.start().expect("a clean fixture");
        session.set_speed(ReplaySpeed::Max);
        let status = session.advance(250);

        assert_eq!(status.records_played, 2);
        assert_eq!(
            status.clamped, 1,
            "the second record's timestamp was behind the clock"
        );
        assert_eq!(status.slot_regressions, 0, "its slot was not");
    }

    #[test]
    fn the_speed_survives_a_stop_and_is_what_the_next_run_plays_at() {
        let scratch = Scratch::new("speed-sticks");
        let records = timed_stream("phase3-speed", 40, 100);
        scratch.write_fixture(&records, "phase3-speed", true);
        let session = scratch.session();

        assert_eq!(
            session.set_speed(ReplaySpeed::Five).speed,
            ReplaySpeed::Five,
            "a multiplier may be chosen before there is anything to play"
        );
        session.start().expect("a clean fixture");
        let status = session.advance(1_000);
        assert_eq!(status.speed, ReplaySpeed::Five);
        assert_eq!(
            status.records_played, 40,
            "five seconds of a four-second recording"
        );

        session.stop();
        assert_eq!(session.status().speed, ReplaySpeed::Five);
    }

    #[test]
    fn the_status_reaches_the_window_in_the_shape_the_window_reads() {
        let scratch = Scratch::new("status-shape");
        let records = timed_stream("phase3-shape", 12, 100);
        scratch.write_fixture(&records, "phase3-shape", true);
        let session = scratch.session();
        session.start().expect("a clean fixture");
        session.set_speed(ReplaySpeed::Max);
        session.advance(250);

        let json = serde_json::to_value(session.status()).expect("the status serialises");

        // Every key `ui/app.js` reads off the replay bar, spelled the way it
        // reads them.
        assert_eq!(json["active"], serde_json::json!(true));
        // The edition this answer was taken from, spelled the way the window
        // reads it. Compared against the session rather than against a literal:
        // what this pins is the key and the fact that asking does not change
        // it, not how many times this particular test happened to touch it.
        assert_eq!(
            json["revision"],
            serde_json::json!(session.status().revision),
            "the edition is on the wire under the name the window looks for"
        );
        assert_eq!(json["speed"], serde_json::json!("max"));
        assert_eq!(json["streamId"], serde_json::json!("phase3-shape"));
        assert_eq!(json["chainVerified"], serde_json::json!(true));
        assert_eq!(json["fixtureComplete"], serde_json::json!(true));
        assert_eq!(json["recordsPlayed"], serde_json::json!(12));
        assert_eq!(json["recordCount"], serde_json::json!(12));
        assert_eq!(json["firstSlot"], serde_json::json!(1_000));
        assert_eq!(json["lastSlot"], serde_json::json!(1_011));
        assert_eq!(json["slot"], serde_json::json!(1_011));
        assert_eq!(json["clamped"], serde_json::json!(0));
        assert_eq!(json["slotRegressions"], serde_json::json!(0));
        assert_eq!(
            json["chainHead"].as_str().expect("a hash").len(),
            64,
            "the whole head, not the shortened form the bar draws"
        );
    }

    #[test]
    fn the_four_multipliers_are_the_four_chips_and_they_round_trip() {
        for (speed, wire) in [
            (ReplaySpeed::Real, "1"),
            (ReplaySpeed::Five, "5"),
            (ReplaySpeed::Ten, "10"),
            (ReplaySpeed::Max, "max"),
        ] {
            assert_eq!(speed.as_str(), wire);
            assert_eq!(speed.to_string(), wire);
            assert_eq!(
                serde_json::to_value(speed).expect("serialises"),
                serde_json::json!(wire)
            );
            assert_eq!(
                serde_json::from_value::<ReplaySpeed>(serde_json::json!(wire))
                    .expect("and comes back"),
                speed
            );
        }

        // Anything else is refused rather than rounded to the nearest chip. A
        // window asking for a speed this build does not have has to be told so
        // rather than quietly given a different one.
        assert!(serde_json::from_value::<ReplaySpeed>(serde_json::json!("7")).is_err());
        assert!(serde_json::from_value::<ReplaySpeed>(serde_json::json!(1)).is_err());
    }

    // -----------------------------------------------------------------------
    // the edition counter
    // -----------------------------------------------------------------------

    #[test]
    fn a_session_nobody_has_touched_is_at_edition_zero() {
        let scratch = Scratch::new("revision-fresh");
        let status = scratch.session().status();
        assert_eq!(status.revision, 0, "nothing has happened to it yet");
    }

    #[test]
    fn every_change_is_a_new_edition_and_a_repeat_is_not() {
        let scratch = Scratch::new("revision-changes");
        scratch.write_fixture(
            &timed_stream("revision-changes", 6, 400),
            "revision-changes",
            true,
        );
        let session = scratch.session();

        // The default speed, set again. Nothing moved, so nothing is a new
        // answer — this is the case that makes the number worth reading.
        let before = session.status().revision;
        assert_eq!(
            session.set_speed(ReplaySpeed::Real).revision,
            before,
            "setting the speed it already had is not a change"
        );
        let after_speed = session.set_speed(ReplaySpeed::Max).revision;
        assert!(after_speed > before, "a different multiplier is a change");

        let playing = session.start().expect("the fixture opens").revision;
        assert!(playing > after_speed, "starting is a change");

        let paused = session.pause().revision;
        assert!(paused > playing, "pausing is a change");
        assert_eq!(
            session.pause().revision,
            paused,
            "pausing an already-held playhead is not a second change"
        );

        let stopped = session.stop().revision;
        assert!(stopped > paused, "stopping is a change");
        assert_eq!(
            session.stop().revision,
            stopped,
            "stopping a stopped session is not a second change"
        );
    }

    #[test]
    fn a_repeated_question_gets_a_repeated_edition() {
        let scratch = Scratch::new("revision-idempotent");
        scratch.write_fixture(
            &timed_stream("revision-idempotent", 4, 400),
            "revision-idempotent",
            true,
        );
        let session = scratch.session();
        session.start().expect("the fixture opens");
        session.pause();

        // Polling is not touching. A window that asks twice and is told two
        // different editions would throw away an answer it should have drawn.
        let first = session.status().revision;
        assert_eq!(session.status().revision, first);
        assert_eq!(session.status().revision, first);
    }

    #[test]
    fn the_edition_never_goes_backwards_across_a_run() {
        let scratch = Scratch::new("revision-monotone");
        scratch.write_fixture(
            &timed_stream("revision-monotone", 12, 200),
            "revision-monotone",
            true,
        );
        let session = scratch.session();

        let mut seen = session.status().revision;
        let mut editions = vec![seen];
        session.set_speed(ReplaySpeed::Max);
        session.start().expect("the fixture opens");
        for _ in 0..8 {
            editions.push(session.advance(250).revision);
        }
        editions.push(session.pause().revision);
        editions.push(session.step(1).expect("a step").revision);
        editions.push(session.fast_forward(None).expect("the rest of it").revision);
        editions.push(session.stop().revision);

        for edition in editions {
            assert!(
                edition >= seen,
                "the edition went backwards: {edition} after {seen}"
            );
            seen = edition;
        }
    }

    #[test]
    fn a_tick_that_plays_nothing_because_nothing_is_playing_is_not_a_change() {
        let scratch = Scratch::new("revision-idle-tick");
        scratch.write_fixture(
            &timed_stream("revision-idle-tick", 4, 400),
            "revision-idle-tick",
            true,
        );
        let session = scratch.session();

        // The ticker runs whether or not anybody opened a fixture. If it minted
        // an edition every 250ms the counter would say the session changed all
        // day and the window would redraw a status that never moved.
        let idle = session.status().revision;
        assert_eq!(session.advance(250).revision, idle);
        assert_eq!(session.advance(250).revision, idle);

        session.start().expect("the fixture opens");
        session.pause();
        let held = session.status().revision;
        assert_eq!(
            session.advance(250).revision,
            held,
            "a held playhead is the operator's, and a tick does not move it"
        );
    }
}
