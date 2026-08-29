//! Batch evaluation of recorded fixtures: what a run of them was worth, what it
//! cost, and what was wrong with the recording.
//!
//! `replay.rs` establishes that a fixture cannot be edited without being
//! noticed and that a fill cannot flatter itself. This module is the thing that
//! consumes both: it walks a directory of hash-chained JSONL streams, prices
//! every decision in them against the same integer curve model, and emits one
//! forensic JSON report. It is `sts backtest` on the command line and
//! `evaluate_directory` in process, and those are the same code path.
//!
//! Four claims are load-bearing here.
//!
//! **The report is a function of the fixture and the config, and of nothing
//! else.** Property R1 of the replay specification is that two runs of one
//! fixture produce byte-identical output, so there is no wall-clock stamp in
//! the report, no host name, no elapsed time, no iteration order that comes off
//! a hash map, and no floating-point number anywhere in the financial path.
//! Every ratio is an integer in a named unit — basis points, lamports, cents,
//! millionths — and the two places that genuinely need a transcendental carry
//! their own fixed-point implementation rather than calling `f64::exp` or
//! `f64::sqrt`, whose last bit is a property of the host's libm.
//!
//! **A broken chain is reported, not repaired.** `ReplayCursor::open` refuses a
//! stream at the first bad link, which is right for a gate run and useless for
//! working out what went wrong. `audit_stream` walks every line instead, keeps
//! going past a break, and classifies each line into one of a fixed vocabulary.
//! Records after the first break are *unverifiable*, never *verified*: they may
//! be read for debugging and may never back a number anybody quotes.
//!
//! **Extraction is measured against the threshold the specification derives.**
//! §15.2 says a sandwich clears fees exactly when `β > φ / (1 - φ)`, and
//! `sandwich_viable` is that comparison carried out in integers with no
//! division, so the boundary is decided by the inequality rather than by
//! rounding. What sits above the threshold is then priced with a search that
//! has no floating-point step in it.
//!
//! **What is inferred says so.** Rug classification and Sybil clustering here
//! are heuristics over what the fixture recorded, not the metrics of
//! `RISK_AND_SYBIL_SPEC.md` Part I — that document's temporal influence needs a
//! funding-graph traversal this harness has no graph for. Where a number is a
//! bound rather than a measurement, the field name and the doc comment say so,
//! and `AdverseSelectionSummary::optimistic` exists because the one bound that
//! points the dangerous way should be impossible to quote without seeing it.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::execution::TipPolicy;
use crate::replay::{
    from_line, genesis_hash, hex, sandwich_breakeven_victim_lamports, sandwich_extraction_closed,
    simulate_sandwich, ClockAdvance, CurveState, Fill, Manifest, OrderKey, QuoteError,
    RecordOutcome, ReplayError, ReplayObserver, ReplayRecord, Sandwich, SimulatedLedger,
    BPS_DENOMINATOR, DEFAULT_FEE_BPS, LAMPORTS_PER_SOL, MIN_VIABLE_ATTACKER_LAMPORTS,
    PUMP_GRADUATION_LAMPORTS,
};
use crate::walkforward::LaunchCohort;

/// The schema string on a fixture event, carried inside a frame.
pub const EVENT_SCHEMA: &str = "sts.backtest.v1";

/// The schema string on the report this module emits.
pub const REPORT_SCHEMA: &str = "sts.backtest.report.v1";

// ===========================================================================
// Fixed-point arithmetic
// ===========================================================================

/// One million. Every ratio in this module that is not in basis points is in
/// millionths, and the field names say `_micros` so the two are never confused.
pub const MICROS: u64 = 1_000_000;

/// The internal precision the transcendentals are computed at: 10^18, which is
/// the largest power of ten whose square still fits a `u128` with room to add.
const FIXED_ONE: u128 = 1_000_000_000_000_000_000;

/// Integer square root, floored.
///
/// Newton's method from an initial guess that is provably above the answer, so
/// the iteration descends monotonically and stops exactly. `f64::sqrt` would be
/// correct on any one machine and is a rounding mode away from being different
/// on another; this is the same on all of them, which is the property the
/// equivalence gate is about.
pub fn isqrt(n: u128) -> u128 {
    if n < 2 {
        return n;
    }
    // 2^ceil(bits/2) is at or above sqrt(n) for every n, and no more than a
    // factor of two above it, so the descent takes a handful of steps.
    let bits = 128 - n.leading_zeros();
    let mut x = 1u128 << bits.div_ceil(2);
    loop {
        // x <= 2^64 and n / x <= 2^64, so the sum cannot overflow a u128.
        let next = (x + n / x) / 2;
        if next >= x {
            return x;
        }
        x = next;
    }
}

/// `exp(-x)` in millionths, for `x` given in millionths.
///
/// Range reduction by halving, a six-term Taylor series at the reduced
/// argument, then repeated squaring back up — all in `u128` at 10^-18, which
/// leaves nine orders of magnitude of headroom over the millionth this returns.
///
/// It exists because the buy-synchrony kernel in `RISK_AND_SYBIL_SPEC.md` §3.5
/// is an exponential and the score it feeds is stored and compared byte for
/// byte. `f64::exp` is not specified to the last bit by IEEE 754 and different
/// libm implementations genuinely differ there, so a run on one machine and a
/// run on another would disagree in the last stored digit for no reason the
/// engine is responsible for.
///
/// Monotone non-increasing in `x`, exactly 10^6 at zero, and zero above
/// `x = 42`, where the true value is below 10^-18 and rounds to nothing at any
/// precision this returns.
pub fn exp_neg_micros(x_micros: u64) -> u64 {
    if x_micros == 0 {
        return MICROS;
    }
    // exp(-42) is 5.7e-19: below one part in 10^18, so below the precision the
    // series is computed at, let alone the millionth it is reported at.
    if x_micros >= 42 * MICROS {
        return 0;
    }

    // Reduce until the argument is under 1/64, where six Taylor terms are
    // already better than one part in 10^13.
    let mut halvings = 0u32;
    while (x_micros >> halvings) > 15_625 {
        halvings += 1;
    }

    // x_micros < 42 * 10^6, so this product is below 4.2 * 10^19 and the shift
    // only makes it smaller.
    let u = (u128::from(x_micros) * 1_000_000_000_000) >> halvings;

    // exp(-u) = 1 - u + u^2/2 - u^3/6 + u^4/24 - u^5/120, each term carried at
    // 10^-18 and each division truncating by less than one part in 10^18.
    let t2 = u * u / FIXED_ONE / 2;
    let t3 = t2 * u / FIXED_ONE / 3;
    let t4 = t3 * u / FIXED_ONE / 4;
    let t5 = t4 * u / FIXED_ONE / 5;
    let mut value = FIXED_ONE + t2 + t4;
    value = value
        .saturating_sub(u)
        .saturating_sub(t3)
        .saturating_sub(t5);

    // Square back up. Each squaring stays inside a u128 because the value never
    // exceeds 10^18.
    for _ in 0..halvings {
        value = value * value / FIXED_ONE;
    }

    // Round to nearest millionth rather than truncating: this is a kernel whose
    // mean is taken, and a truncation that always points one way would bias
    // every synchrony score downwards.
    ((value + 500_000_000_000) / 1_000_000_000_000).min(u128::from(MICROS)) as u64
}

/// `a * b / c`, floored, in `u128`. Returns zero when `c` is zero, because
/// every caller here is computing a share of something and a denominator of
/// zero means there was nothing to take a share of.
pub fn mul_div_floor(a: u128, b: u128, c: u128) -> u128 {
    if c == 0 {
        return 0;
    }
    a.saturating_mul(b) / c
}

/// `a * b / c`, rounded to nearest, half away from zero.
///
/// Used for the concentration index, where `RISK_AND_SYBIL_SPEC.md` §2.2 is
/// explicit that truncation biases a concentrated token towards looking safe.
///
/// The rounding half is added saturatingly rather than plainly. `a * b`
/// saturates at `u128::MAX`, and `u128::MAX + c/2` is an overflow, which is a
/// panic under this crate's release profile rather than a wrap — so the
/// sibling of `mul_div_floor` would abort on exactly the inputs `mul_div_floor`
/// survives. Saturating there costs nothing: the product is already wrong by
/// however much it saturated, and half a denominator does not make it wronger.
pub fn mul_div_round(a: u128, b: u128, c: u128) -> u128 {
    if c == 0 {
        return 0;
    }
    a.saturating_mul(b).saturating_add(c / 2) / c
}

/// `a * b / c`, rounded up. The direction every cost in this module is rounded.
pub fn mul_div_ceil(a: u128, b: u128, c: u128) -> u128 {
    if c == 0 {
        return 0;
    }
    a.saturating_mul(b).div_ceil(c)
}

/// Floored signed division. Rounds towards negative infinity in both
/// directions, so a gain is never rounded up and a loss is never rounded away.
pub fn floor_div_i128(numerator: i128, denominator: i128) -> i128 {
    if denominator == 0 {
        return 0;
    }
    numerator.div_euclid(denominator)
        - i128::from(denominator < 0 && numerator.rem_euclid(denominator) != 0)
}

/// Lamports converted to whole US cents at `cents_per_sol`.
///
/// Floored towards negative infinity, so the sign of the residue always points
/// against the account: a gain loses its fraction of a cent and a loss keeps
/// it. The alternative — truncation towards zero — shrinks losses, which is the
/// one direction a backtest must never round.
pub fn lamports_to_usd_cents(lamports: i128, cents_per_sol: u64) -> i64 {
    if cents_per_sol == 0 {
        return 0;
    }
    floor_div_i128(
        lamports.saturating_mul(i128::from(cents_per_sol)),
        LAMPORTS_PER_SOL as i128,
    )
    .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

// ===========================================================================
// The fixture event vocabulary
// ===========================================================================

/// How a launch ended.
///
/// A closed vocabulary rather than a boolean, because "the curve completed",
/// "somebody pulled the floor out" and "it drifted down over an hour" are three
/// different things that a rug/not-rug flag turns into one. `Unknown` is a real
/// answer and is never rounded to `Held`: a stream that stops while a position
/// is open has not told us what happened next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RugClass {
    /// Real SOL fell off a cliff: a pull, or a sell cascade steep enough that
    /// the difference does not matter to whoever was holding.
    Rug,
    /// The curve completed and the pool moved on.
    Graduated,
    /// It went down, but gradually enough that an exit existed the whole way.
    Faded,
    /// Still standing when the stream ended.
    Held,
    /// The stream does not say.
    Unknown,
}

impl RugClass {
    pub const ALL: [RugClass; 5] = [
        RugClass::Rug,
        RugClass::Graduated,
        RugClass::Faded,
        RugClass::Held,
        RugClass::Unknown,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            RugClass::Rug => "rug",
            RugClass::Graduated => "graduated",
            RugClass::Faded => "faded",
            RugClass::Held => "held",
            RugClass::Unknown => "unknown",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        RugClass::ALL.into_iter().find(|c| c.as_str() == text)
    }

    /// Whether this class is the thing the rug detector is trying to catch.
    /// `Unknown` is deliberately not a rug and deliberately not a non-rug; the
    /// confusion matrix counts it separately rather than folding it either way.
    pub const fn is_rug(self) -> bool {
        matches!(self, RugClass::Rug)
    }
}

impl fmt::Display for RugClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which way somebody else's swap went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub const fn as_str(self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
}

/// A launch begins, and the curve it begins at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchOpen {
    pub mint: String,
    pub at_ms: i64,
    pub creator: Option<String>,
    pub curve: CurveState,
}

/// Somebody else's swap. It moves the curve and it is what our fills are
/// displaced by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowEvent {
    pub mint: String,
    pub at_ms: i64,
    pub wallet: String,
    /// Who paid for this wallet, if the recording knows. `None` is unknown and
    /// is never treated as "nobody" — an unfunded wallet and a wallet whose
    /// funder was not recorded are different facts.
    pub funder: Option<String>,
    pub side: Side,
    /// Gross lamports in, for a buy.
    pub gross_lamports: u64,
    /// Token base units in, for a sell.
    pub tokens: u64,
}

/// Our buy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryEvent {
    pub mint: String,
    pub at_ms: i64,
    pub gross_lamports: u64,
    pub tag: Option<String>,
}

/// Our sell. `tokens` absent means the whole position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitEvent {
    pub mint: String,
    pub at_ms: i64,
    pub tokens: Option<u64>,
    pub tag: Option<String>,
}

/// A holder snapshot, for concentration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldersEvent {
    pub mint: String,
    pub at_ms: i64,
    /// Sorted on ingest by balance descending, then address ascending — the
    /// order `RISK_AND_SYBIL_SPEC.md` §2.2 requires at the boundary, done once
    /// here so every metric downstream gets the same slice.
    pub holders: Vec<(String, u64)>,
}

/// Liquidity left the curve outside the swap path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullEvent {
    pub mint: String,
    pub at_ms: i64,
    pub wallet: Option<String>,
    pub lamports: u64,
}

/// Ground truth, for grading the classifier against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelEvent {
    pub mint: String,
    pub outcome: RugClass,
}

/// One decoded fixture event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchEvent {
    Launch(LaunchOpen),
    Flow(FlowEvent),
    Entry(EntryEvent),
    Exit(ExitEvent),
    Holders(HoldersEvent),
    Pull(PullEvent),
    Label(LabelEvent),
}

impl LaunchEvent {
    /// Which launch this is about. Every event names one; there is no
    /// stream-wide event, because a fixture directory holds several launches
    /// and an event that belonged to all of them would belong to none.
    pub fn mint(&self) -> &str {
        match self {
            LaunchEvent::Launch(e) => &e.mint,
            LaunchEvent::Flow(e) => &e.mint,
            LaunchEvent::Entry(e) => &e.mint,
            LaunchEvent::Exit(e) => &e.mint,
            LaunchEvent::Holders(e) => &e.mint,
            LaunchEvent::Pull(e) => &e.mint,
            LaunchEvent::Label(e) => &e.mint,
        }
    }

    pub const fn kind(&self) -> &'static str {
        match self {
            LaunchEvent::Launch(_) => "launch",
            LaunchEvent::Flow(_) => "flow",
            LaunchEvent::Entry(_) => "entry",
            LaunchEvent::Exit(_) => "exit",
            LaunchEvent::Holders(_) => "holders",
            LaunchEvent::Pull(_) => "pull",
            LaunchEvent::Label(_) => "label",
        }
    }
}

/// A frame that did not decode into an event.
///
/// Carries the sequence number rather than the line number, because by the time
/// the frame is being decoded the record it came from has already been placed in
/// the chain and `seq` is what names it everywhere else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventError {
    pub seq: u64,
    pub detail: String,
}

impl fmt::Display for EventError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "record {} carries an unusable event: {}",
            self.seq, self.detail
        )
    }
}

fn need<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Result<&'a serde_json::Value, String> {
    object.get(name).ok_or_else(|| format!("missing {name}"))
}

fn need_str(
    object: &serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Result<String, String> {
    need(object, name)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{name} is not a string"))
}

fn maybe_str(
    object: &serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Result<Option<String>, String> {
    match object.get(name) {
        None => Ok(None),
        Some(value) if value.is_null() => Ok(None),
        Some(value) => value
            .as_str()
            .map(|s| Some(s.to_string()))
            .ok_or_else(|| format!("{name} is not a string")),
    }
}

fn need_u64(
    object: &serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Result<u64, String> {
    need(object, name)?
        .as_u64()
        .ok_or_else(|| format!("{name} is not an unsigned integer"))
}

fn maybe_u64(
    object: &serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Result<Option<u64>, String> {
    match object.get(name) {
        None => Ok(None),
        Some(value) if value.is_null() => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("{name} is not an unsigned integer")),
    }
}

fn need_i64(
    object: &serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Result<i64, String> {
    need(object, name)?
        .as_i64()
        .ok_or_else(|| format!("{name} is not an integer"))
}

/// Reads the curve a launch starts at.
///
/// Three forms, most specific first: an explicit six-number `curve` object, a
/// `real_sol_lamports` position derived from the invariant, or the protocol's
/// launch reserves. The middle form is the one a recorder produces when it saw
/// the curve mid-life and only logged how much real SOL was in it.
fn parse_curve(object: &serde_json::Map<String, serde_json::Value>) -> Result<CurveState, String> {
    if let Some(value) = object.get("curve") {
        let curve = value
            .as_object()
            .ok_or_else(|| "curve is not an object".to_string())?;
        return Ok(CurveState::from_parts(
            need_u64(curve, "virtual_token_reserves")?,
            need_u64(curve, "virtual_sol_reserves")?,
            need_u64(curve, "real_token_reserves")?,
            need_u64(curve, "real_sol_reserves")?,
            need_u64(curve, "token_total_supply")?,
            match curve.get("complete") {
                None => false,
                Some(flag) => flag
                    .as_bool()
                    .ok_or_else(|| "complete is not a boolean".to_string())?,
            },
        ));
    }
    match maybe_u64(object, "real_sol_lamports")? {
        Some(lamports) => Ok(CurveState::at_real_sol(lamports)),
        None => Ok(CurveState::LAUNCH),
    }
}

/// Decodes one frame's bytes into an event.
///
/// The frame is the exact bytes the recorder saw, so this is the seam where a
/// transport-level fixture becomes something with an economic meaning. Failing
/// here is not the same as failing the chain: the record is genuine and the
/// payload is not one this build understands, which is a different problem with
/// a different remedy.
pub fn decode_event(frame: &[u8], seq: u64) -> Result<LaunchEvent, EventError> {
    decode_event_inner(frame).map_err(|detail| EventError { seq, detail })
}

fn decode_event_inner(frame: &[u8]) -> Result<LaunchEvent, String> {
    let value: serde_json::Value =
        serde_json::from_slice(frame).map_err(|_| "not JSON".to_string())?;
    let object = value
        .as_object()
        .ok_or_else(|| "not a JSON object".to_string())?;

    let schema = need_str(object, "schema")?;
    if schema != EVENT_SCHEMA {
        return Err(format!("schema is {schema:?}, expected {EVENT_SCHEMA:?}"));
    }

    let kind = need_str(object, "kind")?;
    let mint = need_str(object, "mint")?;

    match kind.as_str() {
        "launch" => Ok(LaunchEvent::Launch(LaunchOpen {
            mint,
            at_ms: need_i64(object, "at_ms")?,
            creator: maybe_str(object, "creator")?,
            curve: parse_curve(object)?,
        })),
        "flow" => {
            let side_text = need_str(object, "side")?;
            let side = match side_text.as_str() {
                "buy" => Side::Buy,
                "sell" => Side::Sell,
                other => return Err(format!("unknown side: {other}")),
            };
            let (gross_lamports, tokens) = match side {
                Side::Buy => (need_u64(object, "gross_lamports")?, 0),
                Side::Sell => (0, need_u64(object, "tokens")?),
            };
            Ok(LaunchEvent::Flow(FlowEvent {
                mint,
                at_ms: need_i64(object, "at_ms")?,
                wallet: need_str(object, "wallet")?,
                funder: maybe_str(object, "funder")?,
                side,
                gross_lamports,
                tokens,
            }))
        }
        "entry" => Ok(LaunchEvent::Entry(EntryEvent {
            mint,
            at_ms: need_i64(object, "at_ms")?,
            gross_lamports: need_u64(object, "gross_lamports")?,
            tag: maybe_str(object, "tag")?,
        })),
        "exit" => Ok(LaunchEvent::Exit(ExitEvent {
            mint,
            at_ms: need_i64(object, "at_ms")?,
            tokens: maybe_u64(object, "tokens")?,
            tag: maybe_str(object, "tag")?,
        })),
        "holders" => {
            let list = need(object, "holders")?
                .as_array()
                .ok_or_else(|| "holders is not an array".to_string())?;
            let mut holders = Vec::with_capacity(list.len());
            for entry in list {
                let entry = entry
                    .as_object()
                    .ok_or_else(|| "a holder is not an object".to_string())?;
                holders.push((need_str(entry, "wallet")?, need_u64(entry, "balance")?));
            }
            // Balance descending, address ascending. Sorted once, here.
            holders.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            Ok(LaunchEvent::Holders(HoldersEvent {
                mint,
                at_ms: need_i64(object, "at_ms")?,
                holders,
            }))
        }
        "pull" => Ok(LaunchEvent::Pull(PullEvent {
            mint,
            at_ms: need_i64(object, "at_ms")?,
            wallet: maybe_str(object, "wallet")?,
            lamports: need_u64(object, "lamports")?,
        })),
        "label" => {
            let outcome = need_str(object, "outcome")?;
            Ok(LaunchEvent::Label(LabelEvent {
                mint,
                outcome: RugClass::parse(&outcome)
                    .ok_or_else(|| format!("unknown outcome: {outcome}"))?,
            }))
        }
        other => Err(format!("unknown kind: {other}")),
    }
}

// ===========================================================================
// Chain audit: verification that keeps going
// ===========================================================================

/// What one line of a fixture turned out to be.
///
/// `ReplayCursor::open` collapses all of these into "the stream is refused",
/// which is the right answer for a gate and the wrong one for an investigation.
/// Keeping them apart is what lets a report say *which* line went wrong and
/// *how*, which is the Phase 3 acceptance criterion about a failure naming the
/// fixture and the expected and actual state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineStatus {
    /// Parsed, self-consistent, correctly linked, correctly ordered, and every
    /// line before it was too. The only status a gate run accepts.
    Verified,
    /// Not JSON, wrong schema, or a field that would not read.
    Unparseable,
    /// `seq` did not follow the previous record's.
    SeqGap,
    /// The record's own `integrity_hash` is not what its contents imply. The
    /// record was edited after it was sealed.
    SelfInconsistent,
    /// `prev_hash` is not the previous record's `integrity_hash`. A record was
    /// removed, inserted, or reordered.
    ChainBroken,
    /// The §6 total order was violated.
    OutOfOrder,
    /// Everything about this line checks out, and a line before it did not.
    /// Readable for debugging, never quotable: a splice that reseals the chain
    /// from the break onwards produces exactly this and nothing else.
    UnverifiableAfterBreak,
}

impl LineStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            LineStatus::Verified => "verified",
            LineStatus::Unparseable => "unparseable",
            LineStatus::SeqGap => "seq_gap",
            LineStatus::SelfInconsistent => "self_inconsistent",
            LineStatus::ChainBroken => "chain_broken",
            LineStatus::OutOfOrder => "out_of_order",
            LineStatus::UnverifiableAfterBreak => "unverifiable_after_break",
        }
    }

    /// Whether a record with this status may back a number in a gate run.
    pub const fn is_trustworthy(self) -> bool {
        matches!(self, LineStatus::Verified)
    }

    /// Whether this status means the line itself is broken, as opposed to being
    /// downstream of something else that is.
    pub const fn is_a_break(self) -> bool {
        !matches!(
            self,
            LineStatus::Verified | LineStatus::UnverifiableAfterBreak
        )
    }
}

impl fmt::Display for LineStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One line's verdict, with enough detail to find it in the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineVerdict {
    /// One-based, counting every line in the file including blanks, so it is
    /// the number an editor shows.
    pub line: usize,
    /// `None` when the line did not parse far enough to have one.
    pub seq: Option<u64>,
    pub status: LineStatus,
    pub detail: String,
}

/// A record that survived parsing, and what the audit thought of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditedRecord {
    pub line: usize,
    pub status: LineStatus,
    pub record: ReplayRecord,
}

/// Where a stream's chain stands, so the next segment can carry on from it.
///
/// §3.3: segments roll at 64 MiB or at UTC midnight and **the chain runs across
/// the roll** — the `prev_hash` of the first record in `001.jsonl` is the
/// `integrity_hash` of the last record in `000.jsonl`. Segmentation is a storage
/// detail and not a boundary in the evidence, so an audit that restarted from
/// genesis at every file would report a legitimate rotation as a forgery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainCursor {
    pub next_seq: u64,
    pub prev_hash: [u8; 32],
    /// The §6 key of the last record read, or `None` at the start of a stream.
    pub previous_key: Option<OrderKey>,
    /// Whether an earlier segment of this stream failed verification.
    ///
    /// Set the moment any line breaks and never cleared, because the chain is a
    /// property of the stream and not of the file a line happens to sit in.
    /// Within one segment the walk already marks everything downstream of a
    /// break `UnverifiableAfterBreak`; without carrying that across the roll,
    /// the next segment starts with a clean slate and its lines come back
    /// `Verified`. A splice that spans a roll reseals from the break onwards
    /// and lands entirely inside the new file — exactly the forgery the status
    /// exists to catch, laundered by §3.3's rotation.
    pub broken: bool,
}

impl ChainCursor {
    /// The state before a stream's first record: sequence zero, the genesis
    /// hash, and no order key to follow.
    pub fn start(stream_id: &str) -> Self {
        ChainCursor {
            next_seq: 0,
            prev_hash: genesis_hash(stream_id),
            previous_key: None,
            broken: false,
        }
    }
}

/// The result of walking one JSONL stream end to end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainAudit {
    pub stream_id: String,
    /// Every line in the file, blanks included.
    pub lines_read: usize,
    pub blank_lines: usize,
    pub verified: usize,
    pub unverifiable: usize,
    pub rejected: usize,
    /// The first line that went wrong, one-based. Lines are numbered within
    /// this segment, so `None` here does not mean the stream is sound — see
    /// `carried_break`.
    pub first_break: Option<usize>,
    /// Whether the incoming cursor already carried a break from an earlier
    /// segment. A segment can be clean line by line and still be unquotable.
    pub carried_break: bool,
    /// Only the lines that went wrong, plus the ones downstream of a break.
    /// A clean stream produces an empty vector rather than one entry per line,
    /// because a report that lists a million verified lines is a report nobody
    /// reads the failures in.
    pub verdicts: Vec<LineVerdict>,
    pub records: Vec<AuditedRecord>,
    /// The chain head after the last parsed record, which is what the manifest
    /// carries. Genesis when nothing parsed.
    pub chain_head: [u8; 32],
    /// Where to carry on from, for the next segment of this stream.
    pub cursor: ChainCursor,
}

impl ChainAudit {
    /// Whether every line verified and there was something to verify.
    ///
    /// §3.2's rule about incomplete fixtures generalised: a fixture that cannot
    /// account for every one of its own lines may be replayed for debugging and
    /// may never back a gate dossier.
    pub fn gate_ready(&self) -> Result<(), String> {
        if self.records.is_empty() {
            return Err(format!("{}: no records parsed", self.stream_id));
        }
        if self.carried_break {
            return Err(format!(
                "{}: an earlier segment of this stream failed verification",
                self.stream_id,
            ));
        }
        match self.first_break {
            None => Ok(()),
            Some(line) => Err(format!(
                "{}: line {line} failed verification ({})",
                self.stream_id,
                self.verdicts
                    .first()
                    .map(|v| v.status.as_str())
                    .unwrap_or("unknown"),
            )),
        }
    }

    /// The records a run may read, given whether it is a gate run.
    ///
    /// In gate mode this is the verified prefix and nothing else. Outside it,
    /// everything that parsed — which is what makes a corrupted fixture
    /// diagnosable rather than merely refused.
    pub fn readable(&self, gate: bool) -> impl Iterator<Item = &ReplayRecord> {
        self.records
            .iter()
            .filter(move |audited| !gate || audited.status.is_trustworthy())
            .map(|audited| &audited.record)
    }
}

/// Walks a JSONL stream, verifying every line and continuing past the ones that
/// fail.
///
/// Three checks per record and they catch different edits. Self-integrity
/// catches a field changed in place. The `prev_hash` link catches a record
/// removed, inserted or moved. The §6 order key catches a stream that was
/// resealed after being reordered — the chain would verify and the order would
/// not, which is the shape a plausible forgery has.
///
/// After a break the audit keeps walking rather than stopping, and it
/// re-synchronises `expected_prev` and `expected_seq` from the record it just
/// read. Without that resynchronisation one edited byte on line four turns
/// every later line into a chain error, and a report that says a million lines
/// are broken has said nothing about which one was edited.
pub fn audit_stream(stream_id: &str, text: &str) -> ChainAudit {
    audit_stream_from(stream_id, text, ChainCursor::start(stream_id))
}

/// The same walk, carried on from where a previous segment left off.
pub fn audit_stream_from(stream_id: &str, text: &str, cursor: ChainCursor) -> ChainAudit {
    let mut audit = ChainAudit {
        stream_id: stream_id.to_string(),
        lines_read: 0,
        blank_lines: 0,
        verified: 0,
        unverifiable: 0,
        rejected: 0,
        first_break: None,
        carried_break: cursor.broken,
        verdicts: Vec::new(),
        records: Vec::new(),
        chain_head: cursor.prev_hash,
        cursor,
    };

    let mut expected_prev = cursor.prev_hash;
    let mut expected_seq: u64 = cursor.next_seq;
    let mut previous_key: Option<OrderKey> = cursor.previous_key;

    for (index, raw) in text.lines().enumerate() {
        let line = index + 1;
        audit.lines_read = line;
        if raw.trim().is_empty() {
            audit.blank_lines += 1;
            continue;
        }

        let record = match from_line(raw, line) {
            Ok(record) => record,
            Err(err) => {
                audit.rejected += 1;
                audit.first_break.get_or_insert(line);
                audit.verdicts.push(LineVerdict {
                    line,
                    seq: seq_of(&err),
                    status: LineStatus::Unparseable,
                    detail: err.to_string(),
                });
                continue;
            }
        };

        // Every check runs, and the first that fails names the line. They are
        // ordered cheapest-evidence-first: a wrong seq is a hole, a wrong self
        // hash is an edit, a wrong link is a splice, a wrong key is a reorder.
        let mut status = LineStatus::Verified;
        let mut detail = String::new();

        if record.seq != expected_seq {
            status = LineStatus::SeqGap;
            detail = format!("expected seq {expected_seq}, found {}", record.seq);
        } else if !record.verify_integrity() {
            status = LineStatus::SelfInconsistent;
            detail = format!(
                "integrity_hash is {}, the contents imply {}",
                hex(&record.integrity_hash),
                hex(&record.compute_integrity(&record.prev_hash)),
            );
        } else if record.prev_hash != expected_prev {
            status = LineStatus::ChainBroken;
            detail = format!(
                "prev_hash is {}, the previous record sealed to {}",
                hex(&record.prev_hash),
                hex(&expected_prev),
            );
        } else if let Some(previous) = previous_key {
            let key = record.order_key();
            if key <= previous {
                status = LineStatus::OutOfOrder;
                detail = format!("{key:?} does not follow {previous:?}");
            }
        }

        if status.is_a_break() {
            audit.first_break.get_or_insert(line);
        } else if audit.first_break.is_some() {
            status = LineStatus::UnverifiableAfterBreak;
            detail = "a line before this one failed verification".to_string();
        } else if audit.carried_break {
            status = LineStatus::UnverifiableAfterBreak;
            detail = "an earlier segment of this stream failed verification".to_string();
        }

        match status {
            LineStatus::Verified => audit.verified += 1,
            LineStatus::UnverifiableAfterBreak => audit.unverifiable += 1,
            _ => audit.rejected += 1,
        }
        if status != LineStatus::Verified {
            audit.verdicts.push(LineVerdict {
                line,
                seq: Some(record.seq),
                status,
                detail,
            });
        }

        // Resynchronise from what was actually read, so one bad line does not
        // make every line after it look bad too.
        expected_seq = record.seq.saturating_add(1);
        expected_prev = record.integrity_hash;
        previous_key = Some(record.order_key());
        audit.chain_head = record.integrity_hash;
        audit.records.push(AuditedRecord {
            line,
            status,
            record,
        });
    }

    audit.cursor = ChainCursor {
        next_seq: expected_seq,
        prev_hash: expected_prev,
        previous_key,
        broken: audit.carried_break || audit.first_break.is_some(),
    };
    audit
}

/// The sequence number a parse error names, when it names one.
fn seq_of(err: &ReplayError) -> Option<u64> {
    match err {
        ReplayError::FrameMismatch { seq, .. }
        | ReplayError::ChainBroken { seq, .. }
        | ReplayError::OutOfOrder { seq, .. } => Some(*seq),
        ReplayError::SeqGap { found, .. } => Some(*found),
        _ => None,
    }
}

// ===========================================================================
// §15.2 — extraction against beta = phi / (1 - phi)
// ===========================================================================

/// The victim's fee-adjusted size relative to the virtual SOL reserve,
/// `β = (1 - φ) b / y`, in millionths.
///
/// Reporting only. The viability decision uses `sandwich_viable`, which does the
/// same comparison without dividing, because at the threshold the two differ by
/// exactly the rounding and the specification is explicit that there is no sign
/// to assert there.
pub fn beta_micros(victim_gross_lamports: u64, virtual_sol_reserves: u64, fee_bps: u16) -> u64 {
    if virtual_sol_reserves == 0 || fee_bps >= BPS_DENOMINATOR as u16 {
        return 0;
    }
    let remainder = u128::from(BPS_DENOMINATOR - u32::from(fee_bps));
    let numerator = remainder
        .saturating_mul(u128::from(victim_gross_lamports))
        .saturating_mul(u128::from(MICROS));
    let denominator = u128::from(BPS_DENOMINATOR).saturating_mul(u128::from(virtual_sol_reserves));
    (numerator / denominator).min(u128::from(u64::MAX)) as u64
}

/// The threshold `φ / (1 - φ)` in millionths, rounded up.
///
/// Rounded up so that a reported `beta_micros` strictly greater than this one is
/// always genuinely above the threshold; the reverse is not guaranteed, and
/// that asymmetry is why `above_threshold` on the verdict comes from the exact
/// comparison rather than from these two numbers.
pub fn beta_threshold_micros(fee_bps: u16) -> u64 {
    if fee_bps == 0 || fee_bps >= BPS_DENOMINATOR as u16 {
        return 0;
    }
    let fee = u128::from(fee_bps);
    let remainder = u128::from(BPS_DENOMINATOR) - fee;
    (fee.saturating_mul(u128::from(MICROS))).div_ceil(remainder) as u64
}

/// Whether any front-run at all can clear fees against this victim buy.
///
/// §15.2 derives the condition from the sign of the profit derivative at a
/// front-run of zero:
///
/// ```text
/// β > φ / (1 - φ)     ⟺     b (1 - φ)² > φ y     ⟺     b (10⁴ - F)² > F · 10⁴ · y
/// ```
///
/// The right-hand form is what this computes: two multiplications, one
/// comparison, no division and therefore no rounding. Strictly below the
/// threshold no attacker size is profitable *before any landing cost at all*,
/// so a false here is a statement about the curve rather than about the block
/// market.
pub fn sandwich_viable(
    victim_gross_lamports: u64,
    virtual_sol_reserves: u64,
    fee_bps: u16,
) -> bool {
    if fee_bps >= BPS_DENOMINATOR as u16 {
        return false;
    }
    let remainder = u128::from(BPS_DENOMINATOR - u32::from(fee_bps));
    let left = u128::from(victim_gross_lamports)
        .saturating_mul(remainder)
        .saturating_mul(remainder);
    let right = u128::from(fee_bps)
        .saturating_mul(u128::from(BPS_DENOMINATOR))
        .saturating_mul(u128::from(virtual_sol_reserves));
    left > right
}

/// What the model says a public buy of this size is exposed to.
///
/// Every field is a modelled quantity and none of them is a measurement of
/// anything that happened: STS does not sandwich anyone and doctrine forbids the
/// public path this prices. It is here for the reason §15.4 gives — the tip paid
/// for a private bundle is only justified against the adverse selection it
/// avoids, and this is where that number comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandwichVerdict {
    pub victim_gross_lamports: u64,
    pub virtual_sol_reserves: u64,
    pub fee_bps: u16,
    /// `β` in millionths, for reading.
    pub beta_micros: u64,
    /// `φ / (1 - φ)` in millionths, for reading.
    pub beta_threshold_micros: u64,
    /// The same threshold expressed as a victim size, `b* = φ y / (1 - φ)²`.
    pub breakeven_victim_lamports: u64,
    /// The exact integer comparison. This is the field to believe.
    pub above_threshold: bool,
    /// The best front-run the bounded search found, or zero when none pays.
    pub best_attacker_lamports: u64,
    pub attacker_profit_lamports: i64,
    /// Gross extraction at that size, from the three-swap simulation.
    pub extraction_lamports: i64,
    /// The same quantity from §15.1's closed form, for the differential check.
    pub extraction_closed_lamports: u64,
    /// What the sandwich costs the victim, in basis points of the tokens they
    /// would have received alone.
    pub damage_bps: u16,
}

impl SandwichVerdict {
    /// A curve where no front-run clears fees.
    fn none(victim_gross_lamports: u64, state: &CurveState, fee_bps: u16, above: bool) -> Self {
        SandwichVerdict {
            victim_gross_lamports,
            virtual_sol_reserves: state.virtual_sol_reserves,
            fee_bps,
            beta_micros: beta_micros(victim_gross_lamports, state.virtual_sol_reserves, fee_bps),
            beta_threshold_micros: beta_threshold_micros(fee_bps),
            breakeven_victim_lamports: sandwich_breakeven_victim_lamports(
                state.virtual_sol_reserves,
                fee_bps,
            ),
            above_threshold: above,
            best_attacker_lamports: 0,
            attacker_profit_lamports: 0,
            extraction_lamports: 0,
            extraction_closed_lamports: 0,
            damage_bps: 0,
        }
    }
}

/// The most profitable front-run on a ladder-and-bracket search, with no
/// floating-point step anywhere in it.
///
/// `replay::best_front_run` walks a geometric grid built with `f64::powf`, which
/// is fine for producing the specification's tables and is not fine inside a
/// report that has to be byte-identical between two runs on two machines:
/// `powf` is not correctly rounded and is not required to agree between
/// implementations. This walks a doubling ladder, brackets the best rung, and
/// finishes with an integer ternary search and a short linear scan.
///
/// The answer is the best size *the search visited*. Profit as a function of
/// attacker size is unimodal in the reals, so the bracket is right; the four
/// integer floors inside each of the three swaps can flatten it into plateaus a
/// lamport wide, so the final window is scanned rather than searched. What comes
/// back is therefore a lower bound on the attacker's optimum — which is the
/// direction that *understates* adverse selection, and is why
/// `AdverseSelectionSummary` carries a field saying so.
pub fn best_front_run_deterministic(
    state: &CurveState,
    victim_gross: u64,
    fee_bps: u16,
    cost_lamports: u64,
    max_attacker: u64,
) -> Option<(u64, Sandwich)> {
    if max_attacker < MIN_VIABLE_ATTACKER_LAMPORTS {
        return None;
    }

    let profit = |size: u64| -> i64 {
        match simulate_sandwich(state, size, victim_gross, fee_bps, cost_lamports) {
            Ok(sandwich) => sandwich.attacker_profit_lamports,
            Err(_) => i64::MIN,
        }
    };

    // The ladder. Doubling from the smallest front-run that could pay for its
    // own two signatures, with the cap itself appended so the constrained
    // optimum at the boundary is always visited.
    let mut ladder = Vec::with_capacity(64);
    let mut size = MIN_VIABLE_ATTACKER_LAMPORTS;
    loop {
        ladder.push(size);
        match size.checked_mul(2) {
            Some(next) if next <= max_attacker => size = next,
            _ => break,
        }
    }
    if *ladder.last().unwrap_or(&0) != max_attacker {
        ladder.push(max_attacker);
    }

    let mut best_rung = 0usize;
    let mut best_profit = i64::MIN;
    for (index, &rung) in ladder.iter().enumerate() {
        let value = profit(rung);
        if value > best_profit {
            best_profit = value;
            best_rung = index;
        }
    }
    if best_profit == i64::MIN {
        return None;
    }

    // Bracket the winning rung with its neighbours: the true optimum on a
    // unimodal curve lies between the rungs either side of the best one.
    let mut low = ladder[best_rung.saturating_sub(1)];
    let mut high = ladder[(best_rung + 1).min(ladder.len() - 1)];
    if high < low {
        high = low;
    }

    // Integer ternary search down to a window short enough to scan. The
    // iteration count is a function of the bracket alone, so two runs take the
    // same steps in the same order.
    while high.saturating_sub(low) > 32 {
        let third = (high - low) / 3;
        let first = low + third;
        let second = high - third;
        if profit(first) < profit(second) {
            low = first + 1;
        } else {
            high = second;
        }
    }

    // The plateau scan. Ties go to the smaller size: an attacker who can do the
    // same damage with less capital is the more likely attacker.
    let mut best: Option<(u64, Sandwich)> = None;
    let mut best_value = i64::MIN;
    for candidate in low..=high {
        if candidate < MIN_VIABLE_ATTACKER_LAMPORTS {
            continue;
        }
        let Ok(sandwich) =
            simulate_sandwich(state, candidate, victim_gross, fee_bps, cost_lamports)
        else {
            continue;
        };
        if sandwich.attacker_profit_lamports > best_value {
            best_value = sandwich.attacker_profit_lamports;
            best = Some((candidate, sandwich));
        }
    }

    best.filter(|(_, sandwich)| sandwich.attacker_profit_lamports > 0)
}

/// Prices the adverse selection on one public buy.
///
/// Checks the threshold first and only searches above it, which is not an
/// optimisation: below the threshold the search would return one-lamport
/// "profits" that are the floors in the three swaps rather than extraction, and
/// a report that carried those as adverse selection would be quoting arithmetic
/// residue as a cost.
pub fn assess_sandwich(
    state: &CurveState,
    victim_gross_lamports: u64,
    fee_bps: u16,
    landing_cost_lamports: u64,
    max_attacker_lamports: u64,
) -> SandwichVerdict {
    let above = sandwich_viable(victim_gross_lamports, state.virtual_sol_reserves, fee_bps);
    if !above {
        return SandwichVerdict::none(victim_gross_lamports, state, fee_bps, false);
    }

    let Some((attacker, sandwich)) = best_front_run_deterministic(
        state,
        victim_gross_lamports,
        fee_bps,
        landing_cost_lamports,
        max_attacker_lamports,
    ) else {
        return SandwichVerdict::none(victim_gross_lamports, state, fee_bps, true);
    };

    // The closed form takes fee-adjusted inputs; the simulation takes gross.
    let net_of_fee = |gross: u64| -> u64 {
        let gross = u128::from(gross);
        let fee = gross * u128::from(fee_bps) / u128::from(BPS_DENOMINATOR);
        (gross - fee).min(u128::from(u64::MAX)) as u64
    };

    SandwichVerdict {
        victim_gross_lamports,
        virtual_sol_reserves: state.virtual_sol_reserves,
        fee_bps,
        beta_micros: beta_micros(victim_gross_lamports, state.virtual_sol_reserves, fee_bps),
        beta_threshold_micros: beta_threshold_micros(fee_bps),
        breakeven_victim_lamports: sandwich_breakeven_victim_lamports(
            state.virtual_sol_reserves,
            fee_bps,
        ),
        above_threshold: true,
        best_attacker_lamports: attacker,
        attacker_profit_lamports: sandwich.attacker_profit_lamports,
        extraction_lamports: sandwich.extraction_lamports,
        extraction_closed_lamports: sandwich_extraction_closed(
            state.virtual_sol_reserves,
            net_of_fee(attacker),
            net_of_fee(victim_gross_lamports),
        )
        .unwrap_or(0),
        damage_bps: sandwich.victim_damage_bps,
    }
}

// ===========================================================================
// Concentration and Sybil clustering heuristics
// ===========================================================================

/// The traversal budget from `RISK_AND_SYBIL_SPEC.md` §3.4, applied to the one
/// quadratic loop this module has.
///
/// Buy synchrony is a mean over ordered pairs, so a launch with ten thousand
/// buyers is a hundred million exponentials. Above this many wallets the kernel
/// is computed over the earliest `SYNC_WALLET_BUDGET` of them and the result is
/// marked truncated. Earliest rather than largest: synchrony is about the
/// opening burst, and taking the largest would let one late whale displace the
/// burst that the metric exists to find.
pub const SYNC_WALLET_BUDGET: usize = 256;

/// The synchrony kernel's bandwidth, `tau_sync`, in milliseconds. §3.2's
/// default of five seconds.
pub const DEFAULT_TAU_SYNC_MS: u64 = 5_000;

/// Concentration of `balances` in basis points, or `None` when there is nothing
/// to measure.
///
/// `RISK_AND_SYBIL_SPEC.md` §2.2 in integers: shares are taken to parts per
/// trillion first, which bounds every square by 10^24 and keeps the whole sum
/// inside a `u128` with room to spare, and the final divide rounds to nearest
/// rather than truncating because truncation biases a concentrated token
/// towards looking safe.
///
/// `None` is UNKNOWN and is never `Some(0)`. An empty population has no
/// concentration; it does not have a concentration of zero.
pub fn hhi_bps(balances: &[u64]) -> Option<u16> {
    const SCALE: u128 = 1_000_000_000_000;

    if balances.is_empty() {
        return None;
    }
    let total: u128 = balances.iter().map(|&b| u128::from(b)).sum();
    if total == 0 {
        return None;
    }

    let mut sum_sq: u128 = 0;
    for &balance in balances {
        let share = u128::from(balance) * SCALE / total;
        sum_sq += share * share;
    }

    let bps = mul_div_round(sum_sq, u128::from(BPS_DENOMINATOR), SCALE * SCALE);
    Some(bps.min(u128::from(BPS_DENOMINATOR)) as u16)
}

/// The share held by the largest `k`, in basis points.
///
/// The slice must already be sorted by balance descending; `decode_event` does
/// that once at the fixture boundary. Rounded to nearest for the same reason
/// the index is: this is the number that carries the hard rejection.
pub fn top_k_bps(balances: &[u64], k: usize) -> u16 {
    let total: u128 = balances.iter().map(|&b| u128::from(b)).sum();
    if total == 0 {
        return 0;
    }
    let top: u128 = balances.iter().take(k).map(|&b| u128::from(b)).sum();
    mul_div_round(top, u128::from(BPS_DENOMINATOR), total).min(u128::from(BPS_DENOMINATOR)) as u16
}

/// The number of equally-sized holders that would produce this index, in
/// millionths.
///
/// The reciprocal-HHI form, `10_000 / HHI_bps`, which §2.1 states is exact.
/// Millionths rather than a whole number because the interesting range is
/// between one and about forty and the fractional part is where the reading is.
pub fn effective_holders_micros(hhi_bps: u16) -> u64 {
    if hhi_bps == 0 {
        return 0;
    }
    mul_div_floor(
        u128::from(BPS_DENOMINATOR),
        u128::from(MICROS),
        u128::from(hhi_bps),
    ) as u64
}

/// Buy-flow diversity, `1 - HHI(flow)`, in basis points.
///
/// §2.3's BDI, and the word doing the work there is *entity*: this takes
/// per-entity volumes, not per-wallet ones. Handing it raw wallets measures how
/// many keypairs somebody generated, which is free. `None` when there is no
/// flow to measure.
pub fn buyer_diversity_bps(entity_volumes: &[u64]) -> Option<u16> {
    hhi_bps(entity_volumes).map(|hhi| BPS_DENOMINATOR as u16 - hhi)
}

/// Buy synchrony: the mean of `exp(-|t_i - t_j| / τ)` over ordered pairs, in
/// millionths.
///
/// One when every wallet bought in the same instant, decaying smoothly to zero
/// as the buys spread out. A mean over pairs needs no binning, and §3.5 is
/// explicit about why that matters: a bin edge is a thing an adversary can
/// straddle.
///
/// Fewer than two wallets is not measurable and returns `None` rather than
/// zero — a single wallet is trivially synchronised with itself and reporting
/// that as zero would read as "these wallets are unrelated".
pub fn sync_micros(first_buy_ms: &[i64], tau_ms: u64) -> Option<(u64, bool)> {
    if first_buy_ms.len() < 2 || tau_ms == 0 {
        return None;
    }

    let truncated = first_buy_ms.len() > SYNC_WALLET_BUDGET;
    let window = &first_buy_ms[..first_buy_ms.len().min(SYNC_WALLET_BUDGET)];

    // Sum over unordered pairs; the mean over ordered pairs is the same number,
    // since the kernel is symmetric and the diagonal is excluded from both.
    let mut total: u128 = 0;
    let mut pairs: u128 = 0;
    for (index, &earlier) in window.iter().enumerate() {
        for &later in &window[index + 1..] {
            let gap_ms = later.abs_diff(earlier);
            // gap / tau, in millionths, saturating well before exp_neg's cutoff.
            let ratio = mul_div_floor(u128::from(gap_ms), u128::from(MICROS), u128::from(tau_ms));
            total += u128::from(exp_neg_micros(ratio.min(u128::from(u64::MAX)) as u64));
            pairs += 1;
        }
    }

    if pairs == 0 {
        return None;
    }
    Some(((total / pairs).min(u128::from(MICROS)) as u64, truncated))
}

/// The geometric mean of synchrony and funding concentration, in millionths.
///
/// §3.5 stores `sqrt(sync × fund)` and is explicit about why it is geometric:
/// fifty wallets buying in one slot behind fifty different funders is a bot
/// service with fifty customers, and one funder whose wallets bought over four
/// hours is somebody managing positions. An arithmetic mean scores both at a
/// half; only the product finds the thing the metric is for.
pub fn temporal_influence_micros(sync: u64, fund: u64) -> u64 {
    isqrt(u128::from(sync).saturating_mul(u128::from(fund))).min(u128::from(MICROS)) as u64
}

/// One wallet's participation in one launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuyerObservation {
    pub wallet: String,
    pub funder: Option<String>,
    pub first_buy_ms: i64,
    pub buy_volume_lamports: u64,
    pub buys: u32,
}

/// A set of wallets that share a funder, and how tightly they moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalletCluster {
    /// The funding root every wallet in this cluster points at.
    pub funder: String,
    /// Sorted, so two runs list them the same way.
    pub wallets: Vec<String>,
    pub wallet_count: u32,
    pub buy_volume_lamports: u64,
    /// This cluster's share of the launch's whole buy volume.
    pub flow_share_bps: u16,
    /// The kernel over the cluster's own first-buy times.
    pub sync_micros: u64,
    /// `sqrt(sync × flow_share)`.
    pub temporal_influence_micros: u64,
    /// The span from the cluster's first buy to its last.
    pub first_buy_span_ms: i64,
    /// The synchrony budget was hit and the kernel is a partial sum.
    pub sync_truncated: bool,
}

/// What the buyers of one launch look like as a population.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchSybil {
    pub buyer_count: u32,
    pub buy_volume_lamports: u64,
    /// Volume whose funder the recording knows. The rest is UNKNOWN, and it is
    /// counted rather than assumed independent.
    pub attributed_volume_lamports: u64,
    pub unattributed_volume_lamports: u64,
    /// The largest volume-weighted share pointing at one root, over the whole
    /// buy volume including the unattributed part.
    ///
    /// Taken over the whole rather than over the attributed part on purpose:
    /// that makes it a lower bound, and per the specification's conventions a
    /// lower bound may block an entry and may not clear one. A high value here
    /// is evidence; a low one is the absence of evidence, not its opposite.
    pub fund_bps: u16,
    pub sync_micros: Option<u64>,
    pub temporal_influence_micros: Option<u64>,
    /// `1 - HHI` over per-funder volumes, so wallets behind one root count once.
    pub buyer_diversity_bps: Option<u16>,
    /// Concentration over the last holder snapshot in the stream.
    pub holder_hhi_bps: Option<u16>,
    pub holder_top1_bps: u16,
    pub holder_top5_bps: u16,
    pub holder_top10_bps: u16,
    pub effective_holders_micros: u64,
    /// Clusters above the reporting floor, largest volume first.
    pub clusters: Vec<WalletCluster>,
    pub sync_truncated: bool,
}

/// Groups buyers by funder and scores each group.
///
/// Wallets whose funder the recording does not know are **not** recruited into a
/// cluster by synchrony alone. §3.3's rule is that an unknown parent is neither
/// self-funded nor clean, and inventing a cluster for wallets that merely bought
/// at the same time is how a bot service with many customers gets reported as
/// one hand.
pub fn cluster_by_funder(
    buyers: &[BuyerObservation],
    tau_ms: u64,
    min_cluster_wallets: usize,
) -> Vec<WalletCluster> {
    let total_volume: u128 = buyers
        .iter()
        .map(|b| u128::from(b.buy_volume_lamports))
        .sum();

    // A BTreeMap rather than a HashMap: the iteration order is the key order,
    // which is the same on every run and every machine.
    let mut groups: BTreeMap<&str, Vec<&BuyerObservation>> = BTreeMap::new();
    for buyer in buyers {
        if let Some(funder) = buyer.funder.as_deref() {
            groups.entry(funder).or_default().push(buyer);
        }
    }

    let mut clusters: Vec<WalletCluster> = Vec::new();
    for (funder, mut members) in groups {
        if members.len() < min_cluster_wallets {
            continue;
        }
        // Earliest first, then by address: the order the synchrony budget cuts
        // at, fixed before anything reads it.
        members.sort_by(|a, b| {
            a.first_buy_ms
                .cmp(&b.first_buy_ms)
                .then_with(|| a.wallet.cmp(&b.wallet))
        });

        let times: Vec<i64> = members.iter().map(|m| m.first_buy_ms).collect();
        let (sync, truncated) = sync_micros(&times, tau_ms).unwrap_or((0, false));
        let volume: u128 = members
            .iter()
            .map(|m| u128::from(m.buy_volume_lamports))
            .sum();
        let flow_share_bps =
            mul_div_floor(volume, u128::from(BPS_DENOMINATOR), total_volume) as u16;

        let mut wallets: Vec<String> = members.iter().map(|m| m.wallet.clone()).collect();
        wallets.sort();

        clusters.push(WalletCluster {
            funder: funder.to_string(),
            wallet_count: wallets.len() as u32,
            wallets,
            buy_volume_lamports: volume.min(u128::from(u64::MAX)) as u64,
            flow_share_bps,
            sync_micros: sync,
            temporal_influence_micros: temporal_influence_micros(
                sync,
                u64::from(flow_share_bps) * 100,
            ),
            first_buy_span_ms: times
                .last()
                .copied()
                .unwrap_or(0)
                .saturating_sub(times.first().copied().unwrap_or(0)),
            sync_truncated: truncated,
        });
    }

    // Loudest first, and a total order underneath so ties do not float.
    clusters.sort_by(|a, b| {
        b.buy_volume_lamports
            .cmp(&a.buy_volume_lamports)
            .then_with(|| b.wallet_count.cmp(&a.wallet_count))
            .then_with(|| a.funder.cmp(&b.funder))
    });
    clusters
}

// ===========================================================================
// Performance and risk analytics
// ===========================================================================

/// One position, opened and closed.
///
/// Lots are matched first in, first out. Average cost would be one line
/// shorter and would smear a 90-second scalp and a 40-minute hold into one
/// duration; the holding period is a reported statistic here, so the matching
/// rule has to be the one that keeps it meaningful.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosedTrade {
    pub mint: String,
    pub opened_at_ms: i64,
    pub closed_at_ms: i64,
    pub hold_ms: i64,
    pub tokens: u64,
    /// Gross lamports paid for this parcel, including the entry fee.
    pub cost_lamports: u64,
    /// Net lamports received, after the exit fee.
    pub proceeds_lamports: u64,
    pub pnl_lamports: i64,
    pub pnl_usd_cents: i64,
    /// `pnl / cost` in basis points, floored towards negative infinity.
    pub return_bps: i32,
}

/// A position the stream ended while still holding.
///
/// Never folded into realised PnL. §17's no-executable-exit case is the reason:
/// a position whose exit the curve cannot pay for is not worth its mark, and
/// quietly marking it at the model price is how a backtest reports money it
/// could not have got out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrandedPosition {
    pub mint: String,
    pub opened_at_ms: i64,
    pub tokens: u64,
    pub cost_lamports: u64,
    /// What the curve would pay for the whole parcel right now, net of fee.
    /// Zero when there is no executable exit at any size.
    pub marked_lamports: u64,
    pub marked_pnl_lamports: i64,
    pub no_executable_exit: bool,
    pub reason: String,
}

/// A quote the curve refused, and what was being attempted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteFailure {
    pub mint: String,
    pub at_ms: i64,
    /// `entry`, `exit`, `flow_buy`, `flow_sell`, or `mark`.
    pub context: String,
    pub reason: String,
}

/// What the run made, and out of how many trades.
///
/// Every field is an integer in a named unit. There is no `f64` in this struct
/// and none in the code that fills it, because two runs of one fixture have to
/// produce the same bytes and the last bit of a `f64` division is not a thing
/// this engine controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerformanceSummary {
    pub trades: u32,
    pub winners: u32,
    pub losers: u32,
    pub scratches: u32,
    pub starting_equity_lamports: u64,
    pub ending_equity_lamports: i64,
    pub gross_profit_lamports: i64,
    pub gross_loss_lamports: i64,
    pub realized_pnl_lamports: i64,
    pub realized_pnl_usd_cents: i64,
    /// The mark on positions the stream ended while holding. Reported beside
    /// realised PnL and never added into it.
    pub marked_pnl_lamports: i64,
    pub marked_pnl_usd_cents: i64,
    pub fees_paid_lamports: u64,
    /// Realised PnL over starting equity, in basis points.
    pub return_on_equity_bps: i32,
    pub win_rate_bps: u16,
    /// Gross profit over gross loss, in millionths. `None` when nothing lost —
    /// an infinite profit factor is not a large one.
    pub profit_factor_micros: Option<u64>,
    /// Mean per-trade return, in millionths of a basis point.
    pub mean_return_bps_micros: i64,
    /// Sample standard deviation of per-trade returns, same unit.
    pub stddev_return_bps_micros: u64,
    /// Mean over standard deviation, in millionths. Per trade, not annualised,
    /// and at a risk-free rate of zero — see `sharpe_micros` on the module's
    /// analytics for why neither of those is a number this fixture can supply.
    pub sharpe_micros: Option<i64>,
    pub average_hold_ms: i64,
    pub median_hold_ms: i64,
    pub total_hold_ms: i64,
    pub best_trade_lamports: i64,
    pub worst_trade_lamports: i64,
}

/// How bad it got on the way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskSummary {
    pub high_water_lamports: i64,
    pub max_drawdown_lamports: i64,
    pub max_drawdown_bps: u16,
    /// When the deepest point was reached, from the closing trade's timestamp.
    pub max_drawdown_at_ms: i64,
    /// How long equity spent below the high-water mark before recovering it, at
    /// its longest. Zero when it never went under.
    pub longest_underwater_ms: i64,
    pub longest_losing_streak: u32,
    pub positions_stranded: u32,
    pub no_executable_exits: u32,
}

/// The Sharpe ratio of a set of per-trade returns, in millionths.
///
/// Three deliberate choices, each of which is an assumption somebody could
/// disagree with, so each is visible in the signature rather than buried.
///
/// **Per trade, not annualised.** Annualising needs a trades-per-year figure,
/// and a fixture of nine launches over four minutes does not have one. A
/// harness that invented one would be reporting a number about a trading
/// calendar it made up.
///
/// **Risk-free rate zero.** The holding period here is measured in seconds. The
/// risk-free alternative over ninety seconds is zero to every digit this
/// reports.
///
/// **Sample standard deviation.** `n - 1`, so a single trade has no Sharpe at
/// all rather than an infinite one. `None` also when every trade returned
/// exactly the same, which is a zero denominator and not a perfect strategy.
pub fn sharpe_micros(returns_bps: &[i32]) -> Option<i64> {
    let n = returns_bps.len();
    if n < 2 {
        return None;
    }

    let sum: i128 = returns_bps.iter().map(|&r| i128::from(r)).sum();
    let count = n as i128;
    // Scale before dividing, so the mean keeps six digits past the basis point.
    let mean_scaled = sum.saturating_mul(i128::from(MICROS)) / count;

    let mut variance_accumulator: u128 = 0;
    for &r in returns_bps {
        let deviation = i128::from(r).saturating_mul(i128::from(MICROS)) - mean_scaled;
        variance_accumulator = variance_accumulator.saturating_add(
            deviation
                .unsigned_abs()
                .saturating_mul(deviation.unsigned_abs()),
        );
    }
    let variance = variance_accumulator / (count as u128 - 1);
    let stddev = isqrt(variance);
    if stddev == 0 {
        return None;
    }

    let sharpe = mean_scaled.saturating_mul(i128::from(MICROS)) / stddev as i128;
    Some(sharpe.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64)
}

/// Mean and sample standard deviation of per-trade returns, in millionths of a
/// basis point. Zero and zero when there is nothing to measure.
pub(crate) fn return_moments(returns_bps: &[i32]) -> (i64, u64) {
    let n = returns_bps.len();
    if n == 0 {
        return (0, 0);
    }
    let sum: i128 = returns_bps.iter().map(|&r| i128::from(r)).sum();
    let mean_scaled = sum.saturating_mul(i128::from(MICROS)) / n as i128;
    if n < 2 {
        return (
            mean_scaled.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
            0,
        );
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
    (
        mean_scaled.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
        stddev.min(u128::from(u64::MAX)) as u64,
    )
}

/// The equity curve's worst peak-to-trough fall, walked in closing order.
///
/// Realised equity only. Marking open positions into the curve would make the
/// drawdown a function of the model's opinion of a position nobody has sold,
/// and `RISK_AND_SYBIL_SPEC.md` §11.2 is explicit that the breaker measures
/// against something that has actually happened.
fn drawdown(
    trades: &[ClosedTrade],
    starting_equity_lamports: u64,
) -> (i64, i64, u16, i64, i64, u32) {
    let mut equity = i128::from(starting_equity_lamports);
    let mut high_water = equity;
    // The opening equity has no timestamp — nothing in the fixture says when the
    // account was funded — so the underwater clock starts at the first closing.
    // That understates a run whose very first trade lost, by exactly the stretch
    // the fixture cannot date.
    let mut high_water_at_ms = trades.first().map(|t| t.closed_at_ms).unwrap_or(0);
    let mut max_drawdown: i128 = 0;
    let mut max_drawdown_bps: u16 = 0;
    let mut max_drawdown_at_ms: i64 = 0;
    let mut longest_underwater_ms: i64 = 0;
    let mut underwater = false;
    let mut streak: u32 = 0;
    let mut longest_streak: u32 = 0;

    for trade in trades {
        equity = equity.saturating_add(i128::from(trade.pnl_lamports));
        if trade.pnl_lamports < 0 {
            streak += 1;
            longest_streak = longest_streak.max(streak);
        } else {
            streak = 0;
        }

        if equity >= high_water {
            // The stretch only counts if equity actually went under. A run of
            // consecutive new highs spends no time underwater, and measuring
            // from one peak to the next would say it spent all of it there.
            if underwater {
                longest_underwater_ms =
                    longest_underwater_ms.max(trade.closed_at_ms.saturating_sub(high_water_at_ms));
                underwater = false;
            }
            high_water = equity;
            high_water_at_ms = trade.closed_at_ms;
            continue;
        }
        underwater = true;

        let fall = high_water - equity;
        // Against the high-water mark, which is the denominator the breaker
        // uses. A high-water mark at or below zero has no meaningful percentage
        // fall, so the basis-point figure stays where it was and the lamport
        // figure carries the answer.
        let bps = if high_water > 0 {
            mul_div_ceil(
                fall.unsigned_abs(),
                u128::from(BPS_DENOMINATOR),
                high_water.unsigned_abs(),
            )
            .min(u128::from(BPS_DENOMINATOR)) as u16
        } else {
            BPS_DENOMINATOR as u16
        };
        if fall > max_drawdown {
            max_drawdown = fall;
            max_drawdown_at_ms = trade.closed_at_ms;
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

    (
        high_water.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
        max_drawdown.clamp(0, i128::from(i64::MAX)) as i64,
        max_drawdown_bps,
        max_drawdown_at_ms,
        longest_underwater_ms,
        longest_streak,
    )
}

// ===========================================================================
// The run configuration
// ===========================================================================

/// Everything the evaluation depends on besides the fixture itself.
///
/// Serialised into the report, because a number without the policy it was
/// computed under is not reproducible, and the whole point of this module is
/// that the report is a function of the fixture and this struct and nothing
/// else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BacktestConfig {
    /// Total swap fee on the SOL leg, in basis points.
    pub fee_bps: u16,
    /// What SOL is worth, in whole US cents. Zero means the report carries no
    /// dollar figures at all rather than guessing at a price.
    pub cents_per_sol: u64,
    pub starting_equity_lamports: u64,
    /// The attacker's fixed landing cost when pricing adverse selection.
    pub landing_cost_lamports: u64,
    /// The most capital the modelled attacker can deploy. §15.3's `A_max`.
    pub max_attacker_lamports: u64,
    /// The synchrony kernel's bandwidth.
    pub tau_sync_ms: u64,
    /// Below this many wallets a shared funder is a coincidence, not a cluster.
    pub min_cluster_wallets: usize,
    /// A fall this deep from the peak, inside `rug_window_ms`, is a rug.
    pub rug_drop_bps: u16,
    pub rug_window_ms: i64,
    /// A fall this deep at any speed is a fade.
    pub fade_drop_bps: u16,
    /// Refuse anything that did not fully verify. §3.2's rule.
    pub gate: bool,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        BacktestConfig {
            fee_bps: DEFAULT_FEE_BPS,
            cents_per_sol: 0,
            starting_equity_lamports: 10 * LAMPORTS_PER_SOL,
            landing_cost_lamports: 5_000_000,
            max_attacker_lamports: LAMPORTS_PER_SOL,
            tau_sync_ms: DEFAULT_TAU_SYNC_MS,
            min_cluster_wallets: 2,
            rug_drop_bps: 8_000,
            rug_window_ms: 60_000,
            fade_drop_bps: 5_000,
            gate: false,
        }
    }
}

// ===========================================================================
// The per-launch state machine
// ===========================================================================

/// A parcel of tokens bought at one moment for one price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Lot {
    at_ms: i64,
    tokens_remaining: u64,
    cost_remaining: u64,
}

/// What one launch did, and what we did about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchReport {
    pub mint: String,
    pub creator: Option<String>,
    pub first_event_ms: i64,
    pub last_event_ms: i64,
    pub events: u32,
    /// Somebody else's swaps that moved the curve.
    pub flow_events: u32,
    pub entries: u32,
    pub exits: u32,
    pub entry_gross_lamports: u64,
    pub exit_net_lamports: u64,
    pub fees_paid_lamports: u64,
    pub realized_pnl_lamports: i64,
    pub realized_pnl_usd_cents: i64,
    /// Real SOL in the curve at its highest, and at the end.
    pub peak_real_sol_lamports: u64,
    pub final_real_sol_lamports: u64,
    /// The deepest fall from the peak, in basis points, at any speed.
    pub max_drop_bps: u16,
    /// The deepest fall from the peak inside `rug_window_ms`.
    pub fastest_drop_bps: u16,
    pub graduated: bool,
    pub pulls: u32,
    pub pulled_lamports: u64,
    /// What this harness thinks happened.
    pub classified: RugClass,
    /// What the fixture says happened, when it says.
    pub labelled: Option<RugClass>,
    pub sybil: LaunchSybil,
    /// The adverse selection modelled on each of our entries, in the order they
    /// were made.
    pub adverse_selection: Vec<SandwichVerdict>,
    pub trades: Vec<ClosedTrade>,
    pub stranded: Option<StrandedPosition>,
    pub quote_failures: Vec<QuoteFailure>,
}

/// Walks one launch's events, keeping the curve and our position.
///
/// `Clone` so a report can be taken off a run that has not finished:
/// `finish` consumes the runner, which is right for an `Evaluator` walking a
/// directory once and wrong for a `PaperRunner` being asked what the books look
/// like halfway through. Cloning to read is a copy of a few vectors and leaves
/// the running state exactly where it was.
#[derive(Clone)]
struct LaunchRunner {
    config: BacktestConfig,
    mint: String,
    creator: Option<String>,
    curve: CurveState,
    opened: bool,
    first_event_ms: i64,
    last_event_ms: i64,
    events: u32,
    flow_events: u32,
    observations: u32,
    peak_real_sol: u64,
    peak_at_ms: i64,
    max_drop_bps: u16,
    fastest_drop_bps: u16,
    graduated: bool,
    pulls: u32,
    pulled_lamports: u64,
    lots: Vec<Lot>,
    position_tokens: u64,
    entries: u32,
    exits: u32,
    entry_gross_lamports: u64,
    exit_net_lamports: u64,
    fees_lamports: u64,
    trades: Vec<ClosedTrade>,
    /// The sum of every closed trade's PnL, kept as it goes rather than added
    /// up on demand. A streaming ledger reads this between events, and folding
    /// the whole trade list on every event would make one fixture quadratic in
    /// its own trade count.
    ///
    /// `finish` deliberately does not read it: a report sums the trades in
    /// `i128` and clamps once, which is the more careful arithmetic and the one
    /// a finished report should keep. The two are pinned to each other by
    /// `the_ledger_is_the_report_the_evaluator_gives_for_the_same_fixture`,
    /// which is where a divergence between them would surface.
    realized_pnl_lamports: i64,
    /// Slippage over our own fills, as the numerator and denominator of a
    /// weighted mean: `sum(bps x gross)` over `sum(gross)`. Kept unreduced so
    /// two runners' slippage can be combined by adding, which is what lets a
    /// ledger over many launches report one weighted number rather than a mean
    /// of means.
    ///
    /// **Ours only.** Other people's flow moves the curve we are quoted
    /// against; what it paid to do so is not a cost this strategy bore.
    slippage_num: u128,
    slippage_den: u128,
    worst_slippage_bps: u16,
    buyers: BTreeMap<String, BuyerObservation>,
    holders: Vec<(String, u64)>,
    label: Option<RugClass>,
    adverse: Vec<SandwichVerdict>,
    failures: Vec<QuoteFailure>,
}

impl LaunchRunner {
    fn new(mint: &str, config: BacktestConfig) -> Self {
        LaunchRunner {
            config,
            mint: mint.to_string(),
            creator: None,
            // A launch whose opening event never arrived is priced from the
            // protocol's launch reserves and is classified `Unknown`, so the
            // guess can never reach a reported outcome.
            curve: CurveState::LAUNCH,
            opened: false,
            first_event_ms: i64::MAX,
            last_event_ms: i64::MIN,
            events: 0,
            flow_events: 0,
            observations: 0,
            peak_real_sol: 0,
            peak_at_ms: 0,
            max_drop_bps: 0,
            fastest_drop_bps: 0,
            graduated: false,
            pulls: 0,
            pulled_lamports: 0,
            lots: Vec::new(),
            position_tokens: 0,
            entries: 0,
            exits: 0,
            entry_gross_lamports: 0,
            exit_net_lamports: 0,
            fees_lamports: 0,
            trades: Vec::new(),
            realized_pnl_lamports: 0,
            slippage_num: 0,
            slippage_den: 0,
            worst_slippage_bps: 0,
            buyers: BTreeMap::new(),
            holders: Vec::new(),
            label: None,
            adverse: Vec::new(),
            failures: Vec::new(),
        }
    }

    /// Books one of our own fills into the slippage weighting.
    ///
    /// Weighted by the SOL leg — `gross_lamports` on both sides, which is what
    /// the trader pays on a buy and what leaves the curve on a sell — because a
    /// dust exit and a one-SOL entry are not two equal observations of what it
    /// costs this strategy to trade. A fill with no SOL leg at all is not
    /// weightless-but-counted, it is not a fill, and it is left out of both
    /// halves rather than dividing by zero.
    fn slipped(&mut self, fill: &Fill) {
        self.worst_slippage_bps = self.worst_slippage_bps.max(fill.slippage_bps);
        if fill.gross_lamports == 0 {
            return;
        }
        self.slippage_num = self
            .slippage_num
            .saturating_add(u128::from(fill.slippage_bps) * u128::from(fill.gross_lamports));
        self.slippage_den = self
            .slippage_den
            .saturating_add(u128::from(fill.gross_lamports));
    }

    fn fail(&mut self, at_ms: i64, context: &str, reason: impl fmt::Display) {
        self.failures.push(QuoteFailure {
            mint: self.mint.clone(),
            at_ms,
            context: context.to_string(),
            reason: reason.to_string(),
        });
    }

    /// Records where the curve is now, for the classifier.
    ///
    /// Peak and fall are tracked against `real_sol_reserves` rather than market
    /// cap: the executable liquidity is what a holder can actually get out, and
    /// a market cap computed off virtual reserves stays high while the pool it
    /// implies has already emptied.
    ///
    /// **Called for the launch, for other people's flow and for a pull — never
    /// for our own entries and exits.** On a thin curve our own exit is most of
    /// the fall, so counting it would make the label a function of what the
    /// strategy did, and a rug detector graded against labels its own trading
    /// produced is grading itself. What happened to the launch has to be a fact
    /// about the launch.
    fn observe(&mut self, at_ms: i64) {
        self.observations += 1;
        if self.curve.complete || self.curve.real_sol_reserves >= PUMP_GRADUATION_LAMPORTS {
            self.graduated = true;
        }
        let real = self.curve.real_sol_reserves;
        if real >= self.peak_real_sol {
            self.peak_real_sol = real;
            self.peak_at_ms = at_ms;
            return;
        }
        if self.peak_real_sol == 0 {
            return;
        }
        let fall = u128::from(self.peak_real_sol - real);
        let bps = mul_div_floor(
            fall,
            u128::from(BPS_DENOMINATOR),
            u128::from(self.peak_real_sol),
        )
        .min(u128::from(BPS_DENOMINATOR)) as u16;
        self.max_drop_bps = self.max_drop_bps.max(bps);
        self.window_drop(at_ms, bps);
    }

    fn window_drop(&mut self, at_ms: i64, bps: u16) {
        if at_ms.saturating_sub(self.peak_at_ms) <= self.config.rug_window_ms {
            self.fastest_drop_bps = self.fastest_drop_bps.max(bps);
        }
    }

    fn touch(&mut self, at_ms: i64) {
        self.events += 1;
        self.first_event_ms = self.first_event_ms.min(at_ms);
        self.last_event_ms = self.last_event_ms.max(at_ms);
    }
}

impl LaunchRunner {
    /// Applies one event, in stream order.
    ///
    /// Order is the whole contract. Every quote is taken against the curve as it
    /// stands *before* the event that is being priced, and the curve only moves
    /// once that event has been priced — which is what stops a fill being
    /// quoted against liquidity its own trade put there.
    fn apply(&mut self, event: &LaunchEvent) {
        let fee = self.config.fee_bps;
        match event {
            LaunchEvent::Launch(open) => {
                self.touch(open.at_ms);
                self.creator.clone_from(&open.creator);
                self.curve = open.curve;
                self.opened = true;
                self.peak_real_sol = open.curve.real_sol_reserves;
                self.peak_at_ms = open.at_ms;
                self.observe(open.at_ms);
            }

            LaunchEvent::Flow(flow) => {
                self.touch(flow.at_ms);
                self.flow_events += 1;
                let moved = match flow.side {
                    Side::Buy => match self.curve.quote_buy(flow.gross_lamports, fee) {
                        Ok(fill) => {
                            self.curve = self.curve.after_buy(&fill);
                            true
                        }
                        Err(err) => {
                            self.fail(flow.at_ms, "flow_buy", err);
                            false
                        }
                    },
                    Side::Sell => match self.curve.quote_sell(flow.tokens, fee) {
                        Ok(fill) => {
                            self.curve = self.curve.after_sell(&fill);
                            true
                        }
                        Err(err) => {
                            self.fail(flow.at_ms, "flow_sell", err);
                            false
                        }
                    },
                };

                // A buy the curve refused did not happen, so its wallet is not a
                // buyer and does not enter the concentration numbers.
                if moved && flow.side == Side::Buy {
                    let entry = self.buyers.entry(flow.wallet.clone()).or_insert_with(|| {
                        BuyerObservation {
                            wallet: flow.wallet.clone(),
                            funder: flow.funder.clone(),
                            first_buy_ms: flow.at_ms,
                            buy_volume_lamports: 0,
                            buys: 0,
                        }
                    });
                    entry.first_buy_ms = entry.first_buy_ms.min(flow.at_ms);
                    entry.buy_volume_lamports = entry
                        .buy_volume_lamports
                        .saturating_add(flow.gross_lamports);
                    entry.buys += 1;
                    // A funder learned later fills in an earlier unknown; a
                    // funder that disagrees with itself keeps the first answer,
                    // because the recording contradicting itself is not licence
                    // to pick the more incriminating reading.
                    if entry.funder.is_none() {
                        entry.funder.clone_from(&flow.funder);
                    }
                }
                self.observe(flow.at_ms);
            }

            LaunchEvent::Entry(entry) => {
                self.touch(entry.at_ms);
                let before = self.curve;
                match before.quote_buy(entry.gross_lamports, fee) {
                    Ok(fill) => {
                        // Priced against the curve as it was, which is the state
                        // an attacker would have front-run us into.
                        self.adverse.push(assess_sandwich(
                            &before,
                            entry.gross_lamports,
                            fee,
                            self.config.landing_cost_lamports,
                            self.config.max_attacker_lamports,
                        ));
                        self.curve = before.after_buy(&fill);
                        self.lots.push(Lot {
                            at_ms: entry.at_ms,
                            tokens_remaining: fill.tokens,
                            cost_remaining: entry.gross_lamports,
                        });
                        self.position_tokens = self.position_tokens.saturating_add(fill.tokens);
                        self.entries += 1;
                        self.entry_gross_lamports = self
                            .entry_gross_lamports
                            .saturating_add(entry.gross_lamports);
                        self.fees_lamports = self.fees_lamports.saturating_add(fill.fee_lamports);
                        self.slipped(&fill);
                    }
                    Err(err) => self.fail(entry.at_ms, "entry", err),
                }
            }

            LaunchEvent::Exit(exit) => {
                self.touch(exit.at_ms);
                let wanted = exit
                    .tokens
                    .unwrap_or(self.position_tokens)
                    .min(self.position_tokens);
                if wanted == 0 {
                    self.fail(exit.at_ms, "exit", "there is no position to close");
                    return;
                }
                match self.curve.quote_sell(wanted, fee) {
                    Ok(fill) => {
                        self.curve = self.curve.after_sell(&fill);
                        let closed = self.consume(wanted, fill.net_lamports, exit.at_ms);
                        for trade in &closed {
                            self.realized_pnl_lamports = self
                                .realized_pnl_lamports
                                .saturating_add(trade.pnl_lamports);
                        }
                        self.trades.extend(closed);
                        self.position_tokens = self.position_tokens.saturating_sub(wanted);
                        self.exits += 1;
                        self.exit_net_lamports =
                            self.exit_net_lamports.saturating_add(fill.net_lamports);
                        self.fees_lamports = self.fees_lamports.saturating_add(fill.fee_lamports);
                        self.slipped(&fill);
                    }
                    Err(err) => self.fail(exit.at_ms, "exit", err),
                }
            }

            LaunchEvent::Pull(pull) => {
                self.touch(pull.at_ms);
                self.pulls += 1;
                let magnitude = pull.lamports.min(self.curve.real_sol_reserves);
                if magnitude > 0 {
                    let signed = i64::try_from(magnitude).unwrap_or(i64::MAX);
                    match self.curve.displaced(-signed) {
                        Some(next) => {
                            self.curve = next;
                            self.pulled_lamports = self.pulled_lamports.saturating_add(magnitude);
                        }
                        None => self.fail(
                            pull.at_ms,
                            "pull",
                            "the curve cannot give up that much real SOL",
                        ),
                    }
                }
                self.observe(pull.at_ms);
            }

            LaunchEvent::Holders(snapshot) => {
                self.touch(snapshot.at_ms);
                self.holders.clone_from(&snapshot.holders);
            }

            LaunchEvent::Label(label) => {
                self.events += 1;
                self.label = Some(label.outcome);
            }
        }
    }

    /// Matches a sale against the open lots, first in first out, and turns each
    /// matched parcel into a closed trade.
    ///
    /// Cost attributed to a partial consumption rounds **up**, so the parcel
    /// that leaves carries at least its share of what was paid and the residue
    /// stays with the position rather than appearing as profit. Proceeds are
    /// split down and the remainder goes to the last parcel, so the parts sum to
    /// exactly what the fill returned and no lamport is invented or lost.
    fn consume(&mut self, tokens: u64, proceeds: u64, at_ms: i64) -> Vec<ClosedTrade> {
        let mut plan: Vec<(i64, u64, u64)> = Vec::new();
        let mut remaining = tokens;

        for lot in self.lots.iter_mut() {
            if remaining == 0 {
                break;
            }
            if lot.tokens_remaining == 0 {
                continue;
            }
            let take = lot.tokens_remaining.min(remaining);
            let cost = if take == lot.tokens_remaining {
                lot.cost_remaining
            } else {
                mul_div_ceil(
                    u128::from(lot.cost_remaining),
                    u128::from(take),
                    u128::from(lot.tokens_remaining),
                )
                .min(u128::from(lot.cost_remaining)) as u64
            };
            lot.tokens_remaining -= take;
            lot.cost_remaining -= cost;
            remaining -= take;
            plan.push((lot.at_ms, take, cost));
        }
        self.lots.retain(|lot| lot.tokens_remaining > 0);

        let matched: u64 = plan.iter().map(|(_, take, _)| take).sum();
        let mut closed = Vec::with_capacity(plan.len());
        let mut handed_out: u64 = 0;
        for (index, (opened_at_ms, take, cost)) in plan.iter().enumerate() {
            let share = if index + 1 == plan.len() {
                proceeds.saturating_sub(handed_out)
            } else {
                mul_div_floor(u128::from(proceeds), u128::from(*take), u128::from(matched)) as u64
            };
            handed_out = handed_out.saturating_add(share);

            let pnl = i128::from(share) - i128::from(*cost);
            closed.push(ClosedTrade {
                mint: self.mint.clone(),
                opened_at_ms: *opened_at_ms,
                closed_at_ms: at_ms,
                hold_ms: at_ms.saturating_sub(*opened_at_ms),
                tokens: *take,
                cost_lamports: *cost,
                proceeds_lamports: share,
                pnl_lamports: pnl.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
                pnl_usd_cents: lamports_to_usd_cents(pnl, self.config.cents_per_sol),
                return_bps: floor_div_i128(
                    pnl.saturating_mul(i128::from(BPS_DENOMINATOR)),
                    i128::from(*cost),
                )
                .clamp(i128::from(i32::MIN), i128::from(i32::MAX))
                    as i32,
            });
        }
        closed
    }

    /// What this harness thinks happened to the launch.
    ///
    /// A pull outranks everything, including graduation: liquidity leaving
    /// outside the swap path is the fact, and a curve that completed on the way
    /// out does not make it a different one. Below that, a fall inside the
    /// window is a cliff and a fall at any speed is a fade — the difference
    /// being whether an exit existed while it was happening, which is the only
    /// difference that matters to whoever was holding.
    fn classify(&self) -> RugClass {
        if !self.opened || self.observations < 2 {
            return RugClass::Unknown;
        }
        if self.pulls > 0 {
            return RugClass::Rug;
        }
        if self.graduated {
            return RugClass::Graduated;
        }
        if self.fastest_drop_bps >= self.config.rug_drop_bps {
            return RugClass::Rug;
        }
        if self.max_drop_bps >= self.config.fade_drop_bps {
            return RugClass::Faded;
        }
        RugClass::Held
    }

    /// The buyer population, as concentration and clustering numbers.
    fn sybil(&self) -> LaunchSybil {
        let buyers: Vec<BuyerObservation> = self.buyers.values().cloned().collect();
        let total_volume: u128 = buyers
            .iter()
            .map(|b| u128::from(b.buy_volume_lamports))
            .sum();

        let mut per_funder: BTreeMap<&str, u128> = BTreeMap::new();
        let mut attributed: u128 = 0;
        let mut solo_entities: Vec<u64> = Vec::new();
        for buyer in &buyers {
            match buyer.funder.as_deref() {
                Some(funder) => {
                    *per_funder.entry(funder).or_insert(0) += u128::from(buyer.buy_volume_lamports);
                    attributed += u128::from(buyer.buy_volume_lamports);
                }
                // An unattributed wallet counts as its own entity for diversity.
                // That is the generous reading and it is the safe one: it can
                // only make the flow look more diverse than it is, and diversity
                // is the direction that does not clear an entry on its own.
                None => solo_entities.push(buyer.buy_volume_lamports),
            }
        }

        let fund_bps = per_funder
            .values()
            .copied()
            .max()
            .map(|largest| {
                mul_div_floor(largest, u128::from(BPS_DENOMINATOR), total_volume)
                    .min(u128::from(BPS_DENOMINATOR)) as u16
            })
            .unwrap_or(0);

        let mut times: Vec<i64> = buyers.iter().map(|b| b.first_buy_ms).collect();
        times.sort_unstable();
        let synchrony = sync_micros(&times, self.config.tau_sync_ms);

        let mut entity_volumes: Vec<u64> = per_funder
            .values()
            .map(|&v| v.min(u128::from(u64::MAX)) as u64)
            .collect();
        entity_volumes.extend(solo_entities);
        entity_volumes.sort_unstable_by(|a, b| b.cmp(a));

        let balances: Vec<u64> = self.holders.iter().map(|(_, balance)| *balance).collect();
        let holder_hhi_bps = hhi_bps(&balances);

        LaunchSybil {
            buyer_count: buyers.len() as u32,
            buy_volume_lamports: total_volume.min(u128::from(u64::MAX)) as u64,
            attributed_volume_lamports: attributed.min(u128::from(u64::MAX)) as u64,
            unattributed_volume_lamports: total_volume
                .saturating_sub(attributed)
                .min(u128::from(u64::MAX)) as u64,
            fund_bps,
            sync_micros: synchrony.map(|(value, _)| value),
            temporal_influence_micros: synchrony
                .map(|(value, _)| temporal_influence_micros(value, u64::from(fund_bps) * 100)),
            buyer_diversity_bps: buyer_diversity_bps(&entity_volumes),
            holder_hhi_bps,
            holder_top1_bps: top_k_bps(&balances, 1),
            holder_top5_bps: top_k_bps(&balances, 5),
            holder_top10_bps: top_k_bps(&balances, 10),
            effective_holders_micros: holder_hhi_bps.map(effective_holders_micros).unwrap_or(0),
            clusters: cluster_by_funder(
                &buyers,
                self.config.tau_sync_ms,
                self.config.min_cluster_wallets,
            ),
            sync_truncated: synchrony.map(|(_, truncated)| truncated).unwrap_or(false),
        }
    }

    /// This launch as the walk-forward splitter sees it.
    ///
    /// Buyers only. Our own entries are not buyers here: the split exists to
    /// keep *other people's* wallets off both sides of a fold boundary, and the
    /// strategy's own wallet is on every launch it traded by construction.
    ///
    /// Every buyer and every funder, not the clusters. `LaunchSybil` carries
    /// the groups that cleared the reporting floor, which is the right slice for
    /// a forensic report and the wrong one for a split: a wallet that bought
    /// once in each of two folds is on both sides of the line whether or not it
    /// was ever part of a group, and a funder below the cluster floor still
    /// links the two launches it funded.
    fn cohort(&self) -> LaunchCohort {
        let mut wallets: Vec<String> = self.buyers.keys().cloned().collect();
        wallets.sort();
        wallets.dedup();
        let mut funders: Vec<String> = self
            .buyers
            .values()
            .filter_map(|buyer| buyer.funder.clone())
            .collect();
        funders.sort();
        funders.dedup();
        LaunchCohort {
            mint: self.mint.clone(),
            first_event_ms: self.first_event_ms,
            last_event_ms: self.last_event_ms,
            creator: self.creator.clone(),
            funders,
            wallets,
        }
    }

    /// Closes the books on the launch.
    fn finish(mut self) -> LaunchReport {
        let stranded = if self.position_tokens > 0 {
            let cost: u64 = self
                .lots
                .iter()
                .map(|lot| lot.cost_remaining)
                .fold(0u64, |acc, c| acc.saturating_add(c));
            let opened_at_ms = self.lots.iter().map(|lot| lot.at_ms).min().unwrap_or(0);
            let tokens = self.position_tokens;
            let (marked, no_exit, reason) = match self.curve.quote_sell(tokens, self.config.fee_bps)
            {
                Ok(fill) => (
                    fill.net_lamports,
                    false,
                    "marked at the modelled fill".to_string(),
                ),
                Err(err) => {
                    let no_exit = matches!(
                        err,
                        QuoteError::ExceedsRealSol { .. } | QuoteError::CurveComplete
                    );
                    let at = self.last_event_ms;
                    self.fail(at, "mark", err);
                    (0, no_exit, err.to_string())
                }
            };
            Some(StrandedPosition {
                mint: self.mint.clone(),
                opened_at_ms,
                tokens,
                cost_lamports: cost,
                marked_lamports: marked,
                marked_pnl_lamports: (i128::from(marked) - i128::from(cost))
                    .clamp(i128::from(i64::MIN), i128::from(i64::MAX))
                    as i64,
                no_executable_exit: no_exit,
                reason,
            })
        } else {
            None
        };

        let realized: i128 = self.trades.iter().map(|t| i128::from(t.pnl_lamports)).sum();

        LaunchReport {
            classified: self.classify(),
            sybil: self.sybil(),
            mint: self.mint.clone(),
            creator: self.creator.clone(),
            first_event_ms: if self.first_event_ms == i64::MAX {
                0
            } else {
                self.first_event_ms
            },
            last_event_ms: if self.last_event_ms == i64::MIN {
                0
            } else {
                self.last_event_ms
            },
            events: self.events,
            flow_events: self.flow_events,
            entries: self.entries,
            exits: self.exits,
            entry_gross_lamports: self.entry_gross_lamports,
            exit_net_lamports: self.exit_net_lamports,
            fees_paid_lamports: self.fees_lamports,
            realized_pnl_lamports: realized.clamp(i128::from(i64::MIN), i128::from(i64::MAX))
                as i64,
            realized_pnl_usd_cents: lamports_to_usd_cents(realized, self.config.cents_per_sol),
            peak_real_sol_lamports: self.peak_real_sol,
            final_real_sol_lamports: self.curve.real_sol_reserves,
            max_drop_bps: self.max_drop_bps,
            fastest_drop_bps: self.fastest_drop_bps,
            graduated: self.graduated,
            pulls: self.pulls,
            pulled_lamports: self.pulled_lamports,
            labelled: self.label,
            adverse_selection: self.adverse,
            trades: self.trades,
            stranded,
            quote_failures: self.failures,
        }
    }

    /// What this runner has booked so far, in a shape that can be added up.
    ///
    /// Read between events rather than at the end, so a ledger can report a
    /// half-played fixture. Every field is a running total, so the difference
    /// between two calls either side of one `apply` is exactly what that event
    /// did — which is how `PaperRunner` books an event in constant time rather
    /// than re-summing every launch on every record.
    fn totals(&self) -> RunningTotals {
        RunningTotals {
            entries: u64::from(self.entries),
            exits: u64::from(self.exits),
            trades: self.trades.len() as u64,
            entry_gross_lamports: self.entry_gross_lamports,
            exit_net_lamports: self.exit_net_lamports,
            fees_lamports: self.fees_lamports,
            realized_pnl_lamports: self.realized_pnl_lamports,
            quote_failures: self.failures.len() as u64,
            slippage_num: self.slippage_num,
            slippage_den: self.slippage_den,
            worst_slippage_bps: self.worst_slippage_bps,
        }
    }
}

/// One launch's books at a moment, as numbers that add.
///
/// Separate from `LaunchReport` on purpose: a report is what a finished run
/// concluded, and this is what an unfinished one has so far. Folding the two
/// would mean either a report that can be taken mid-stream — which invites
/// quoting a rug classification of a launch that has not rugged yet — or a
/// ledger that has to build one to read a number off it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct RunningTotals {
    entries: u64,
    exits: u64,
    trades: u64,
    entry_gross_lamports: u64,
    exit_net_lamports: u64,
    fees_lamports: u64,
    realized_pnl_lamports: i64,
    quote_failures: u64,
    slippage_num: u128,
    slippage_den: u128,
    worst_slippage_bps: u16,
}

impl RunningTotals {
    /// What happened between two readings.
    ///
    /// Saturating throughout, and every counter here is monotonic, so `later`
    /// is never behind `self` and a saturation is a bug rather than a wrap.
    fn since(self, earlier: RunningTotals) -> RunningTotals {
        RunningTotals {
            entries: self.entries.saturating_sub(earlier.entries),
            exits: self.exits.saturating_sub(earlier.exits),
            trades: self.trades.saturating_sub(earlier.trades),
            entry_gross_lamports: self
                .entry_gross_lamports
                .saturating_sub(earlier.entry_gross_lamports),
            exit_net_lamports: self
                .exit_net_lamports
                .saturating_sub(earlier.exit_net_lamports),
            fees_lamports: self.fees_lamports.saturating_sub(earlier.fees_lamports),
            realized_pnl_lamports: self
                .realized_pnl_lamports
                .saturating_sub(earlier.realized_pnl_lamports),
            quote_failures: self.quote_failures.saturating_sub(earlier.quote_failures),
            slippage_num: self.slippage_num.saturating_sub(earlier.slippage_num),
            slippage_den: self.slippage_den.saturating_sub(earlier.slippage_den),
            // Not a difference. A worst case only ever climbs, so the later
            // reading already carries it and subtracting would turn "the worst
            // fill so far" into "the worst fill since the last event", which is
            // not a number anybody wants.
            worst_slippage_bps: self.worst_slippage_bps,
        }
    }
}

// ===========================================================================
// The streaming ledger
// ===========================================================================

/// Prices a fixture as it plays, one record at a time.
///
/// The other end of `replay`'s `ReplayObserver` seam, and the reason that seam
/// exists: `replay` owns records, clocks and chains, and what a frame is worth
/// is this module's question. Attach one to a `ReplaySession` and the transport
/// stops being a playhead over a file and becomes a backtest runner — the same
/// arithmetic `Evaluator` does over a whole directory, done incrementally, so
/// the number on the bar after ninety seconds of watching is the number the
/// report would give for the first ninety seconds.
///
/// **It is the same arithmetic, deliberately.** Every event goes through the
/// same `LaunchRunner` an `Evaluator` run would build, under the same
/// `BacktestConfig`, with the same rule about which records are replayed: a
/// frame the live filters dropped is not applied, a frame a bounded channel
/// could not take is. A ledger that priced a fixture differently from the
/// report over the same fixture would make one of the two wrong and give
/// nobody a way to tell which.
///
/// # Determinism
///
/// Nothing here reads a clock, a random number, or a hash map's iteration
/// order. Launches are kept in a `BTreeMap`, tips are priced from the record's
/// own event id, and every number is an integer. Two runs of one fixture — at
/// `1x`, at `max`, in one fast-forward, or one step at a time — produce the
/// same ledger, which is what makes it worth putting a number from it in a
/// dossier.
pub struct PaperRunner {
    config: BacktestConfig,
    /// What the executor would have bid to land each simulated exit.
    tips: TipPolicy,
    launches: BTreeMap<String, LaunchRunner>,
    ledger: SimulatedLedger,
    slippage_num: u128,
    slippage_den: u128,
}

impl fmt::Debug for PaperRunner {
    /// Prints the books rather than the machinery.
    ///
    /// `ReplayObserver` needs `Debug` so a session can be printed, and a
    /// session printed with one `LaunchRunner` per mint expanded is thousands
    /// of lines nobody reads. The ledger is the part anybody debugging a replay
    /// actually wants.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaperRunner")
            .field("launches", &self.launches.len())
            .field("ledger", &self.ledger)
            .finish()
    }
}

impl PaperRunner {
    /// A runner with an empty book.
    ///
    /// The tip policy is the exit policy — `TipPolicy::emergency` — rather than
    /// the discretionary one, and that is a decision about what is being
    /// measured. Annex C.2 has a discretionary bid refuse itself when the trade
    /// has no positive expectation, which is the correct rule for deciding
    /// whether to send one and the wrong one for counting what getting out
    /// cost: it would drop the tip on exactly the losing exits where the cost
    /// is realest, and report a cheaper strategy than the one that ran.
    pub fn new(config: BacktestConfig) -> Self {
        PaperRunner {
            config,
            tips: TipPolicy::emergency(),
            launches: BTreeMap::new(),
            ledger: SimulatedLedger::default(),
            slippage_num: 0,
            slippage_den: 0,
        }
    }

    /// The same runner bidding a different tip policy — a devnet account list,
    /// or a ceiling an operator set.
    pub fn tipping(mut self, tips: TipPolicy) -> Self {
        self.tips = tips;
        self
    }

    pub fn config(&self) -> BacktestConfig {
        self.config
    }

    /// The books as they stand.
    pub fn ledger(&self) -> SimulatedLedger {
        self.ledger
    }

    /// What each launch would report if the fixture ended here.
    ///
    /// In mint order rather than first-seen order, because a `BTreeMap` is what
    /// makes the ledger reproducible and a second ordering kept alongside it
    /// would be a second thing to keep right.
    pub fn reports(&self) -> Vec<LaunchReport> {
        self.launches
            .values()
            .map(|runner| runner.clone().finish())
            .collect()
    }

    /// Applies one decoded event and books what it did.
    ///
    /// Public because a caller with events but no fixture — a unit test, a
    /// generator checking its own arithmetic — should not have to build a
    /// record and a chain around each one to price it.
    pub fn apply(&mut self, event: &LaunchEvent, intent_id: &str) {
        let mint = event.mint().to_string();
        if !self.launches.contains_key(&mint) {
            self.launches
                .insert(mint.clone(), LaunchRunner::new(&mint, self.config));
            self.ledger.launches += 1;
        }
        let Some(runner) = self.launches.get_mut(&mint) else {
            return;
        };

        let before = runner.totals();
        runner.apply(event);
        let delta = runner.totals().since(before);

        self.ledger.events_applied += 1;
        self.ledger.entries += delta.entries;
        self.ledger.exits += delta.exits;
        self.ledger.trades += delta.trades;
        self.ledger.entry_gross_lamports = self
            .ledger
            .entry_gross_lamports
            .saturating_add(delta.entry_gross_lamports);
        self.ledger.exit_net_lamports = self
            .ledger
            .exit_net_lamports
            .saturating_add(delta.exit_net_lamports);
        self.ledger.fees_lamports = self
            .ledger
            .fees_lamports
            .saturating_add(delta.fees_lamports);
        self.ledger.realized_pnl_lamports = self
            .ledger
            .realized_pnl_lamports
            .saturating_add(delta.realized_pnl_lamports);
        self.ledger.quote_failures += delta.quote_failures;

        self.slippage_num = self.slippage_num.saturating_add(delta.slippage_num);
        self.slippage_den = self.slippage_den.saturating_add(delta.slippage_den);
        self.ledger.slippage_bps = weighted_bps(self.slippage_num, self.slippage_den);
        self.ledger.worst_slippage_bps =
            self.ledger.worst_slippage_bps.max(delta.worst_slippage_bps);

        // One bid per exit that actually filled, priced against what that exit
        // realised. An exit the curve refused is not a bundle anybody sent.
        if delta.exits > 0 {
            self.bid_for(intent_id, delta.realized_pnl_lamports);
        }
    }

    /// Prices the tip one filled exit would have paid.
    fn bid_for(&mut self, intent_id: &str, ev_net_lamports: i64) {
        // Attempt zero every time. A rebroadcast is a thing the network does to
        // a transaction this simulation never sends, and escalating a tip for
        // retries nobody made would be inventing a cost.
        match self.tips.bid(intent_id, Some(ev_net_lamports), 0) {
            Ok(bid) => {
                self.ledger.tips_lamports = self.ledger.tips_lamports.saturating_add(bid.lamports);
                self.ledger.tips_bid += 1;
            }
            Err(_) => self.ledger.tips_refused += 1,
        }
    }
}

/// A weighted mean in basis points, rounded against the trader.
///
/// The same direction `slippage_bps` itself rounds, and for the same reason: a
/// simulator that under-reports its own costs flatters every backtest built on
/// it. An empty book weighs nothing and is zero rather than an error — no
/// fills is not the same as a fill of unknown cost, and it is the honest
/// reading of a ledger that has not traded yet.
fn weighted_bps(num: u128, den: u128) -> u16 {
    if den == 0 {
        return 0;
    }
    num.div_ceil(den).min(u128::from(BPS_DENOMINATOR)) as u16
}

impl ReplayObserver for PaperRunner {
    /// Books one record, under exactly the rules `Evaluator::ingest` reads a
    /// stream by.
    ///
    /// The `ClockAdvance` is not used and that is the point rather than an
    /// oversight: every event carries its own `at_ms`, taken from the recording,
    /// and pricing against the clock instead would make the books depend on how
    /// many records a tick happened to deliver. The parameter stays because the
    /// seam is what guarantees the clock was advanced *before* this was called,
    /// and an observer that wanted the virtual now is entitled to it.
    fn observe(&mut self, _advance: ClockAdvance, record: &ReplayRecord) {
        if !record.kind.carries_frame() {
            return;
        }
        match record.outcome {
            // The live engine filtered it, so the ledger does too. Pricing it
            // here would be the filtering bug the fidelity check exists to
            // catch, arriving by the back door.
            RecordOutcome::Dropped(_) => {
                self.ledger.events_filtered += 1;
                return;
            }
            // A frame a bounded channel could not take is replayed anyway —
            // that is what recovery means — so it moves the curve.
            RecordOutcome::Backpressure(_) | RecordOutcome::Accepted => {}
        }
        let Some(frame) = record.frame.as_deref() else {
            return;
        };
        match decode_event(frame, record.seq) {
            // The record's own event id is the intent a tip is priced from: it
            // is unique within the stream and it is written into the fixture,
            // so the same exit bids the same account and the same lamports on
            // every run of it.
            Ok(event) => self.apply(&event, &record.event_id),
            Err(_) => self.ledger.events_undecodable += 1,
        }
    }

    fn ledger(&self) -> SimulatedLedger {
        self.ledger
    }

    fn reset(&mut self) {
        self.launches.clear();
        self.ledger = SimulatedLedger::default();
        self.slippage_num = 0;
        self.slippage_den = 0;
        // The tip policy is rebuilt rather than kept, because a round-robin
        // cursor left where the last run stopped would make the second run of
        // one fixture tip a different set of accounts. `TipPolicy::clone`
        // copies the configuration and starts the cursor over, which is exactly
        // the rewind wanted here.
        let rewound = self.tips.clone();
        self.tips = rewound;
    }
}

// ===========================================================================
// Aggregation into one report
// ===========================================================================

/// What one JSONL file turned out to be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamReport {
    pub stream_id: String,
    pub file: String,
    pub lines: usize,
    pub blank_lines: usize,
    pub records: usize,
    pub verified: usize,
    pub unverifiable: usize,
    pub rejected: usize,
    /// One-based line number of the first failure, if there was one.
    pub first_break: Option<usize>,
    pub chain_head: String,
    /// Frame records that carried an event.
    pub frames: usize,
    pub events_applied: usize,
    /// Frames the live engine dropped, which replay drops too.
    pub frames_dropped_live: usize,
    /// Frames live dropped for backpressure and replay accepted. §5.1's one
    /// tolerated disagreement between a replay and the run it came from.
    pub frames_backpressure_recovered: usize,
    pub gate_ready: bool,
    /// Only the lines that failed, plus the ones after a failure.
    pub verdicts: Vec<LineVerdict>,
    pub event_errors: Vec<EventError>,
}

/// Whether the corpus can back a number anybody quotes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegritySummary {
    pub streams: usize,
    pub lines: usize,
    pub records: usize,
    pub verified: usize,
    pub unverifiable: usize,
    pub rejected: usize,
    pub streams_with_breaks: usize,
    pub event_errors: usize,
    pub gate_ready: bool,
}

/// One cell of the classifier's confusion matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassPair {
    pub labelled: RugClass,
    pub classified: RugClass,
    pub count: u32,
}

/// How well the rug classifier did, and what it cost to be wrong.
///
/// Two things are kept apart on purpose, because the roadmap asks for exactly
/// that separation: how often the detector was right, and how much money the
/// answer was worth. A detector that catches every rug and also refuses every
/// winner has a perfect recall and a negative expectancy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RugSummary {
    pub launches: u32,
    pub labelled: u32,
    pub labelled_rugs: u32,
    pub classified_rugs: u32,
    pub true_positives: u32,
    pub false_positives: u32,
    pub true_negatives: u32,
    pub false_negatives: u32,
    /// Labelled launches the classifier would not call either way. Counted, not
    /// folded into the matrix — an abstention is not a wrong answer and is
    /// certainly not a right one.
    pub abstentions: u32,
    /// Launches with no label at all. There is nothing to grade against.
    pub ungraded: u32,
    pub precision_bps: Option<u16>,
    pub recall_bps: Option<u16>,
    pub f1_bps: Option<u16>,
    pub accuracy_bps: Option<u16>,
    /// Labelled rugs we put no money into, over all labelled rugs.
    pub rug_avoidance_bps: Option<u16>,
    pub entered_labelled_rugs: u32,
    pub entered_labelled_non_rugs: u32,
    pub pnl_on_labelled_rugs_lamports: i64,
    pub pnl_on_labelled_non_rugs_lamports: i64,
    pub confusion: Vec<ClassPair>,
}

/// What the sandwich model says our public buys were exposed to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdverseSelectionSummary {
    pub entries_priced: u32,
    /// Entries where `β > φ / (1 - φ)`, so some front-run clears fees.
    pub entries_above_threshold: u32,
    pub entries_below_threshold: u32,
    /// Entries above the threshold where the bounded search found a size that
    /// also cleared the landing cost.
    pub entries_with_viable_attacker: u32,
    pub mean_damage_bps: u16,
    pub worst_damage_bps: u16,
    pub total_extraction_lamports: i64,
    pub total_attacker_profit_lamports: i64,
    /// The largest discrepancy between §15.1's closed form and the three-swap
    /// simulation, in lamports. It should be single digits; anything larger
    /// means one of the two is wrong.
    pub worst_closed_form_residue_lamports: i64,
    /// Always true, and here so it cannot be quoted without being seen.
    ///
    /// The front-run search visits a bounded set of sizes, so the attacker it
    /// finds is at most as good as the best one. Every number in this struct is
    /// therefore a **lower** bound on adverse selection, which is the direction
    /// that flatters the strategy. Treat it as a floor under the cost, never as
    /// an estimate of it.
    pub optimistic: bool,
}

/// What the buyer populations looked like across the corpus.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SybilSummary {
    pub launches_with_buyers: u32,
    pub buyers_seen: u32,
    pub clusters_found: u32,
    pub largest_cluster_wallets: u32,
    pub max_fund_bps: u16,
    pub max_temporal_influence_micros: u64,
    /// Launches whose temporal influence reached the reporting floor below.
    pub launches_over_floor: u32,
    /// A reporting floor, not a policy threshold. `RISK_AND_SYBIL_SPEC.md` §6
    /// puts the flag on `P_group` out of a calibrated logistic this harness does
    /// not have; 0.8 here only decides what gets listed.
    pub reporting_floor_micros: u64,
    pub max_holder_top1_bps: u16,
    pub min_buyer_diversity_bps: Option<u16>,
    /// Launches where the synchrony budget was hit, so the kernel is partial.
    pub synchrony_truncated: u32,
}

/// What the fixture directory's manifest says about itself, and whether the
/// streams bear it out.
///
/// §3.2 gives the manifest three jobs this checks: it names the stream, so a
/// rotated fixture's segments can be recognised as one chain; it declares the
/// chain head and the record count, which the audit either reproduces or does
/// not; and it carries `complete`, which is the flag that says a hole exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestCheck {
    pub stream_id: String,
    pub complete: bool,
    pub declared_records: u64,
    pub observed_records: u64,
    pub declared_chain_head: String,
    pub observed_chain_head: String,
    /// Whether the count and the head both came out where the manifest said.
    pub agrees: bool,
}

/// The whole thing.
///
/// **There is no timestamp in here, and that is deliberate.** Property R1 is
/// that two runs of one fixture produce byte-identical output; a `generated_at`
/// field would break it on every run, and a report that cannot be diffed cannot
/// be the evidence a gate turns on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForensicReport {
    pub schema: String,
    /// What was read, as given on the command line.
    pub source: String,
    pub config: BacktestConfig,
    /// `None` when the directory carried no manifest, in which case every file
    /// is read as a stream of its own.
    pub manifest: Option<ManifestCheck>,
    pub integrity: IntegritySummary,
    pub performance: PerformanceSummary,
    pub risk: RiskSummary,
    pub adverse_selection: AdverseSelectionSummary,
    pub sybil: SybilSummary,
    pub rug: RugSummary,
    pub streams: Vec<StreamReport>,
    pub launches: Vec<LaunchReport>,
    pub stranded: Vec<StrandedPosition>,
    /// Why this report may not back a gate dossier. Empty when it may.
    pub refusals: Vec<String>,
    pub gate_ready: bool,
}

impl ForensicReport {
    /// The report as indented JSON, ending in a newline.
    ///
    /// Serde writes a struct's fields in declaration order and every collection
    /// in here is a `Vec` that was explicitly sorted, so the bytes are a
    /// function of the report and nothing else. A `HashMap` anywhere in this
    /// tree would put the iteration order of a hash into the output and quietly
    /// break the equivalence gate.
    pub fn to_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self)
            .unwrap_or_else(|err| format!("{{\"error\":\"{err}\"}}"));
        text.push('\n');
        text
    }
}

/// Anything that stops a run before it produces a report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BacktestError {
    Io {
        path: String,
        detail: String,
    },
    NoFixtures {
        path: String,
    },
    /// A gate run over a corpus that did not fully verify.
    Refused(Vec<String>),
}

impl fmt::Display for BacktestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BacktestError::Io { path, detail } => write!(f, "{path}: {detail}"),
            BacktestError::NoFixtures { path } => {
                write!(f, "{path} holds no .jsonl fixture streams")
            }
            BacktestError::Refused(reasons) => {
                write!(f, "refused: {}", reasons.join("; "))
            }
        }
    }
}

impl std::error::Error for BacktestError {}

// ===========================================================================
// The evaluator
// ===========================================================================

/// Walks one or more fixture streams and holds the state they share.
///
/// Launch state lives here rather than per stream, which is what makes §10's
/// segment-independence property hold: a fixture split into four files and the
/// same fixture in one file drive the same runner in the same order, so they
/// produce the same report.
pub struct Evaluator {
    config: BacktestConfig,
    launches: BTreeMap<String, LaunchRunner>,
    /// Mints in the order they were first seen, which is the order the report
    /// lists them in.
    order: Vec<String>,
    streams: Vec<StreamReport>,
    /// Where each stream's chain stands, keyed by stream id, so a segment that
    /// rolled at midnight carries on rather than looking like a forgery.
    cursors: BTreeMap<String, ChainCursor>,
    manifest: Option<Manifest>,
}

impl Evaluator {
    pub fn new(config: BacktestConfig) -> Self {
        Evaluator {
            config,
            launches: BTreeMap::new(),
            order: Vec::new(),
            streams: Vec::new(),
            cursors: BTreeMap::new(),
            manifest: None,
        }
    }

    /// Attaches the directory's manifest, so the run can be checked against
    /// what the recording says about itself.
    pub fn with_manifest(mut self, manifest: Manifest) -> Self {
        self.manifest = Some(manifest);
        self
    }

    /// Audits one stream and applies the events it carries.
    ///
    /// In gate mode only the verified prefix is read. Outside it, everything
    /// that parsed is read and the report says which parts were not verified —
    /// a corrupted fixture is meant to be diagnosable, and refusing to look at
    /// it makes the corruption the only thing anybody can say about it.
    pub fn ingest(&mut self, stream_id: &str, file: &str, text: &str) {
        let cursor = *self
            .cursors
            .entry(stream_id.to_string())
            .or_insert_with(|| ChainCursor::start(stream_id));
        let audit = audit_stream_from(stream_id, text, cursor);
        self.cursors.insert(stream_id.to_string(), audit.cursor);

        let mut frames = 0usize;
        let mut applied = 0usize;
        let mut dropped_live = 0usize;
        let mut backpressure = 0usize;
        let mut event_errors: Vec<EventError> = Vec::new();
        let mut events: Vec<LaunchEvent> = Vec::new();

        for record in audit.readable(self.config.gate) {
            if !record.kind.carries_frame() {
                continue;
            }
            match record.outcome {
                // The live engine filtered it, so replay does too. Reading it
                // here would be the filtering bug §5.1 exists to catch, arriving
                // by the back door.
                RecordOutcome::Dropped(_) => {
                    dropped_live += 1;
                    continue;
                }
                RecordOutcome::Backpressure(_) => backpressure += 1,
                RecordOutcome::Accepted => {}
            }
            let Some(frame) = record.frame.as_deref() else {
                continue;
            };
            frames += 1;

            match decode_event(frame, record.seq) {
                Ok(event) => events.push(event),
                Err(err) => event_errors.push(err),
            }
        }

        // Decoding and applying are two passes because `readable` borrows the
        // audit and applying borrows the evaluator. Splitting them costs one
        // vector and keeps the gate rule in one place instead of two.
        for event in events {
            let mint = event.mint().to_string();
            let config = self.config;
            if !self.launches.contains_key(&mint) {
                self.order.push(mint.clone());
                self.launches
                    .insert(mint.clone(), LaunchRunner::new(&mint, config));
            }
            if let Some(runner) = self.launches.get_mut(&mint) {
                runner.apply(&event);
                applied += 1;
            }
        }

        self.streams.push(StreamReport {
            stream_id: audit.stream_id.clone(),
            file: file.to_string(),
            lines: audit.lines_read,
            blank_lines: audit.blank_lines,
            records: audit.records.len(),
            verified: audit.verified,
            unverifiable: audit.unverifiable,
            rejected: audit.rejected,
            first_break: audit.first_break,
            chain_head: hex(&audit.chain_head),
            frames,
            events_applied: applied,
            frames_dropped_live: dropped_live,
            frames_backpressure_recovered: backpressure,
            gate_ready: audit.gate_ready().is_ok(),
            verdicts: audit.verdicts,
            event_errors,
        });
    }

    /// What the walk-forward splitter needs about each launch, without ending
    /// the run.
    ///
    /// Taken by reference and in first-seen order, so a cohort list and the
    /// report built from the same evaluator name the launches the same way.
    pub fn cohorts(&self) -> Vec<LaunchCohort> {
        let mut cohorts = Vec::with_capacity(self.order.len());
        for mint in &self.order {
            let Some(runner) = self.launches.get(mint) else {
                continue;
            };
            cohorts.push(runner.cohort());
        }
        cohorts
    }

    /// Closes every launch and assembles the report.
    pub fn finish(mut self, source: &str) -> ForensicReport {
        let mut launches: Vec<LaunchReport> = Vec::with_capacity(self.order.len());
        for mint in &self.order {
            if let Some(runner) = self.launches.remove(mint) {
                launches.push(runner.finish());
            }
        }

        // One chronological book across every launch. Ties broken by mint and
        // then by opening time, so the equity curve is a total order and two
        // runs walk it identically.
        let mut trades: Vec<ClosedTrade> = launches
            .iter()
            .flat_map(|launch| launch.trades.iter().cloned())
            .collect();
        trades.sort_by(|a, b| {
            a.closed_at_ms
                .cmp(&b.closed_at_ms)
                .then_with(|| a.mint.cmp(&b.mint))
                .then_with(|| a.opened_at_ms.cmp(&b.opened_at_ms))
                .then_with(|| a.tokens.cmp(&b.tokens))
        });

        let stranded: Vec<StrandedPosition> = launches
            .iter()
            .filter_map(|launch| launch.stranded.clone())
            .collect();

        let performance = summarise_performance(&trades, &stranded, &launches, &self.config);
        let risk = summarise_risk(&trades, &stranded, &self.config);
        let adverse = summarise_adverse_selection(&launches);
        let sybil = summarise_sybil(&launches);
        let rug = summarise_rug(&launches);
        let integrity = summarise_integrity(&self.streams);

        let mut refusals: Vec<String> = Vec::new();
        for stream in &self.streams {
            if let Some(line) = stream.first_break {
                let status = stream
                    .verdicts
                    .first()
                    .map(|v| v.status.as_str())
                    .unwrap_or("unknown");
                refusals.push(format!(
                    "{}: line {line} failed verification ({status})",
                    stream.stream_id
                ));
            }
            if !stream.event_errors.is_empty() {
                refusals.push(format!(
                    "{}: {} frame(s) carried an event this build cannot read",
                    stream.stream_id,
                    stream.event_errors.len()
                ));
            }
        }
        if self.streams.is_empty() {
            refusals.push("no streams were read".to_string());
        } else if integrity.records == 0 {
            refusals.push("no records parsed".to_string());
        }

        let manifest = self.manifest.as_ref().map(|manifest| {
            let observed_head = self
                .streams
                .last()
                .map(|stream| stream.chain_head.clone())
                .unwrap_or_default();
            let observed_records = integrity.records as u64;
            let check = ManifestCheck {
                stream_id: manifest.stream_id.clone(),
                complete: manifest.complete,
                declared_records: manifest.record_count,
                observed_records,
                declared_chain_head: manifest.chain_head.clone(),
                observed_chain_head: observed_head.clone(),
                agrees: manifest.record_count == observed_records
                    && manifest.chain_head == observed_head,
            };
            // R10: an incomplete recording may be replayed for debugging and may
            // never back a gate run. Refusing is the whole mechanism — a warning
            // beside a number is a warning nobody reads beside a number
            // everybody quotes.
            if let Err(err) = manifest.gate_ready() {
                refusals.push(err.to_string());
            }
            if !check.agrees {
                refusals.push(format!(
                    "{}: the manifest declares {} record(s) ending at {}, the streams hold {} \
                     ending at {}",
                    manifest.stream_id,
                    check.declared_records,
                    check.declared_chain_head,
                    check.observed_records,
                    check.observed_chain_head,
                ));
            }
            check
        });

        ForensicReport {
            schema: REPORT_SCHEMA.to_string(),
            source: source.to_string(),
            config: self.config,
            manifest,
            integrity,
            performance,
            risk,
            adverse_selection: adverse,
            sybil,
            rug,
            streams: self.streams,
            launches,
            stranded,
            gate_ready: refusals.is_empty(),
            refusals,
        }
    }
}

fn summarise_integrity(streams: &[StreamReport]) -> IntegritySummary {
    let mut summary = IntegritySummary {
        streams: streams.len(),
        lines: 0,
        records: 0,
        verified: 0,
        unverifiable: 0,
        rejected: 0,
        streams_with_breaks: 0,
        event_errors: 0,
        gate_ready: true,
    };
    for stream in streams {
        summary.lines += stream.lines;
        summary.records += stream.records;
        summary.verified += stream.verified;
        summary.unverifiable += stream.unverifiable;
        summary.rejected += stream.rejected;
        summary.event_errors += stream.event_errors.len();
        if stream.first_break.is_some() {
            summary.streams_with_breaks += 1;
        }
        summary.gate_ready &= stream.gate_ready;
    }
    summary.gate_ready &= !streams.is_empty() && summary.records > 0;
    summary
}

pub(crate) fn summarise_performance(
    trades: &[ClosedTrade],
    stranded: &[StrandedPosition],
    launches: &[LaunchReport],
    config: &BacktestConfig,
) -> PerformanceSummary {
    let mut gross_profit: i128 = 0;
    let mut gross_loss: i128 = 0;
    let mut winners = 0u32;
    let mut losers = 0u32;
    let mut scratches = 0u32;
    let mut total_hold: i128 = 0;
    let mut best = i64::MIN;
    let mut worst = i64::MAX;
    let mut returns: Vec<i32> = Vec::with_capacity(trades.len());
    let mut holds: Vec<i64> = Vec::with_capacity(trades.len());

    for trade in trades {
        match trade.pnl_lamports.cmp(&0) {
            std::cmp::Ordering::Greater => {
                winners += 1;
                gross_profit += i128::from(trade.pnl_lamports);
            }
            std::cmp::Ordering::Less => {
                losers += 1;
                gross_loss += i128::from(-trade.pnl_lamports);
            }
            std::cmp::Ordering::Equal => scratches += 1,
        }
        total_hold += i128::from(trade.hold_ms);
        holds.push(trade.hold_ms);
        returns.push(trade.return_bps);
        best = best.max(trade.pnl_lamports);
        worst = worst.min(trade.pnl_lamports);
    }

    let realized: i128 = gross_profit - gross_loss;
    let marked: i128 = stranded
        .iter()
        .map(|position| i128::from(position.marked_pnl_lamports))
        .sum();
    let fees: u64 = launches
        .iter()
        .fold(0u64, |acc, l| acc.saturating_add(l.fees_paid_lamports));

    holds.sort_unstable();
    let median_hold = if holds.is_empty() {
        0
    } else if holds.len() % 2 == 1 {
        holds[holds.len() / 2]
    } else {
        // Floored mean of the two middles: deterministic, and the direction is
        // consistent rather than chosen per call.
        let low = i128::from(holds[holds.len() / 2 - 1]);
        let high = i128::from(holds[holds.len() / 2]);
        floor_div_i128(low + high, 2) as i64
    };

    let count = trades.len() as i128;
    let (mean_return, stddev_return) = return_moments(&returns);
    let starting = config.starting_equity_lamports;

    PerformanceSummary {
        trades: trades.len() as u32,
        winners,
        losers,
        scratches,
        starting_equity_lamports: starting,
        ending_equity_lamports: (i128::from(starting) + realized)
            .clamp(i128::from(i64::MIN), i128::from(i64::MAX))
            as i64,
        gross_profit_lamports: gross_profit.clamp(0, i128::from(i64::MAX)) as i64,
        gross_loss_lamports: gross_loss.clamp(0, i128::from(i64::MAX)) as i64,
        realized_pnl_lamports: realized.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
        realized_pnl_usd_cents: lamports_to_usd_cents(realized, config.cents_per_sol),
        marked_pnl_lamports: marked.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
        marked_pnl_usd_cents: lamports_to_usd_cents(marked, config.cents_per_sol),
        fees_paid_lamports: fees,
        return_on_equity_bps: if starting == 0 {
            0
        } else {
            floor_div_i128(
                realized.saturating_mul(i128::from(BPS_DENOMINATOR)),
                i128::from(starting),
            )
            .clamp(i128::from(i32::MIN), i128::from(i32::MAX)) as i32
        },
        win_rate_bps: if count == 0 {
            0
        } else {
            mul_div_floor(
                u128::from(winners),
                u128::from(BPS_DENOMINATOR),
                count as u128,
            ) as u16
        },
        profit_factor_micros: if gross_loss == 0 {
            None
        } else {
            Some(
                mul_div_floor(
                    gross_profit.unsigned_abs(),
                    u128::from(MICROS),
                    gross_loss.unsigned_abs(),
                )
                .min(u128::from(u64::MAX)) as u64,
            )
        },
        mean_return_bps_micros: mean_return,
        stddev_return_bps_micros: stddev_return,
        sharpe_micros: sharpe_micros(&returns),
        average_hold_ms: if count == 0 {
            0
        } else {
            floor_div_i128(total_hold, count) as i64
        },
        median_hold_ms: median_hold,
        total_hold_ms: total_hold.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
        best_trade_lamports: if trades.is_empty() { 0 } else { best },
        worst_trade_lamports: if trades.is_empty() { 0 } else { worst },
    }
}

pub(crate) fn summarise_risk(
    trades: &[ClosedTrade],
    stranded: &[StrandedPosition],
    config: &BacktestConfig,
) -> RiskSummary {
    let (high_water, max_drawdown, max_drawdown_bps, at_ms, underwater, streak) =
        drawdown(trades, config.starting_equity_lamports);
    RiskSummary {
        high_water_lamports: high_water,
        max_drawdown_lamports: max_drawdown,
        max_drawdown_bps,
        max_drawdown_at_ms: at_ms,
        longest_underwater_ms: underwater,
        longest_losing_streak: streak,
        positions_stranded: stranded.len() as u32,
        no_executable_exits: stranded
            .iter()
            .filter(|position| position.no_executable_exit)
            .count() as u32,
    }
}

fn summarise_adverse_selection(launches: &[LaunchReport]) -> AdverseSelectionSummary {
    let mut priced = 0u32;
    let mut above = 0u32;
    let mut viable = 0u32;
    let mut damage_total: u128 = 0;
    let mut worst_damage = 0u16;
    let mut extraction: i128 = 0;
    let mut attacker_profit: i128 = 0;
    let mut worst_residue: i64 = 0;

    for launch in launches {
        for verdict in &launch.adverse_selection {
            priced += 1;
            if verdict.above_threshold {
                above += 1;
            }
            if verdict.best_attacker_lamports > 0 {
                viable += 1;
                let residue = verdict.extraction_lamports
                    - i64::try_from(verdict.extraction_closed_lamports).unwrap_or(i64::MAX);
                if residue.abs() > worst_residue.abs() {
                    worst_residue = residue;
                }
            }
            damage_total += u128::from(verdict.damage_bps);
            worst_damage = worst_damage.max(verdict.damage_bps);
            extraction += i128::from(verdict.extraction_lamports);
            attacker_profit += i128::from(verdict.attacker_profit_lamports);
        }
    }

    AdverseSelectionSummary {
        entries_priced: priced,
        entries_above_threshold: above,
        entries_below_threshold: priced - above,
        entries_with_viable_attacker: viable,
        mean_damage_bps: if priced == 0 {
            0
        } else {
            (damage_total / u128::from(priced)).min(u128::from(BPS_DENOMINATOR)) as u16
        },
        worst_damage_bps: worst_damage,
        total_extraction_lamports: extraction.clamp(i128::from(i64::MIN), i128::from(i64::MAX))
            as i64,
        total_attacker_profit_lamports: attacker_profit
            .clamp(i128::from(i64::MIN), i128::from(i64::MAX))
            as i64,
        worst_closed_form_residue_lamports: worst_residue,
        optimistic: true,
    }
}

fn summarise_sybil(launches: &[LaunchReport]) -> SybilSummary {
    const REPORTING_FLOOR_MICROS: u64 = 800_000;

    let mut summary = SybilSummary {
        launches_with_buyers: 0,
        buyers_seen: 0,
        clusters_found: 0,
        largest_cluster_wallets: 0,
        max_fund_bps: 0,
        max_temporal_influence_micros: 0,
        launches_over_floor: 0,
        reporting_floor_micros: REPORTING_FLOOR_MICROS,
        max_holder_top1_bps: 0,
        min_buyer_diversity_bps: None,
        synchrony_truncated: 0,
    };

    for launch in launches {
        let sybil = &launch.sybil;
        if sybil.buyer_count == 0 {
            continue;
        }
        summary.launches_with_buyers += 1;
        summary.buyers_seen += sybil.buyer_count;
        summary.clusters_found += sybil.clusters.len() as u32;
        summary.max_fund_bps = summary.max_fund_bps.max(sybil.fund_bps);
        summary.max_holder_top1_bps = summary.max_holder_top1_bps.max(sybil.holder_top1_bps);
        if sybil.sync_truncated {
            summary.synchrony_truncated += 1;
        }
        for cluster in &sybil.clusters {
            summary.largest_cluster_wallets =
                summary.largest_cluster_wallets.max(cluster.wallet_count);
        }
        if let Some(influence) = sybil.temporal_influence_micros {
            summary.max_temporal_influence_micros =
                summary.max_temporal_influence_micros.max(influence);
            if influence >= REPORTING_FLOOR_MICROS {
                summary.launches_over_floor += 1;
            }
        }
        if let Some(diversity) = sybil.buyer_diversity_bps {
            summary.min_buyer_diversity_bps = Some(match summary.min_buyer_diversity_bps {
                Some(current) => current.min(diversity),
                None => diversity,
            });
        }
    }

    summary
}

pub(crate) fn summarise_rug(launches: &[LaunchReport]) -> RugSummary {
    let mut summary = RugSummary {
        launches: launches.len() as u32,
        labelled: 0,
        labelled_rugs: 0,
        classified_rugs: 0,
        true_positives: 0,
        false_positives: 0,
        true_negatives: 0,
        false_negatives: 0,
        abstentions: 0,
        ungraded: 0,
        precision_bps: None,
        recall_bps: None,
        f1_bps: None,
        accuracy_bps: None,
        rug_avoidance_bps: None,
        entered_labelled_rugs: 0,
        entered_labelled_non_rugs: 0,
        pnl_on_labelled_rugs_lamports: 0,
        pnl_on_labelled_non_rugs_lamports: 0,
        confusion: Vec::new(),
    };

    let mut cells: BTreeMap<(RugClass, RugClass), u32> = BTreeMap::new();
    let mut rug_pnl: i128 = 0;
    let mut non_rug_pnl: i128 = 0;

    for launch in launches {
        if launch.classified.is_rug() {
            summary.classified_rugs += 1;
        }
        let Some(labelled) = launch.labelled else {
            summary.ungraded += 1;
            continue;
        };
        summary.labelled += 1;
        *cells.entry((labelled, launch.classified)).or_insert(0) += 1;

        if labelled == RugClass::Unknown {
            // A label of "the stream does not say" is not ground truth. It is
            // counted in the confusion table and graded nowhere.
            continue;
        }
        if labelled.is_rug() {
            summary.labelled_rugs += 1;
            rug_pnl += i128::from(launch.realized_pnl_lamports);
            if launch.entries > 0 {
                summary.entered_labelled_rugs += 1;
            }
        } else {
            non_rug_pnl += i128::from(launch.realized_pnl_lamports);
            if launch.entries > 0 {
                summary.entered_labelled_non_rugs += 1;
            }
        }

        match (labelled.is_rug(), launch.classified) {
            (_, RugClass::Unknown) => summary.abstentions += 1,
            (true, class) if class.is_rug() => summary.true_positives += 1,
            (true, _) => summary.false_negatives += 1,
            (false, class) if class.is_rug() => summary.false_positives += 1,
            (false, _) => summary.true_negatives += 1,
        }
    }

    summary.confusion = cells
        .into_iter()
        .map(|((labelled, classified), count)| ClassPair {
            labelled,
            classified,
            count,
        })
        .collect();

    let tp = u128::from(summary.true_positives);
    let fp = u128::from(summary.false_positives);
    let tn = u128::from(summary.true_negatives);
    let fnn = u128::from(summary.false_negatives);

    if tp + fp > 0 {
        summary.precision_bps =
            Some(mul_div_floor(tp, u128::from(BPS_DENOMINATOR), tp + fp) as u16);
    }
    if tp + fnn > 0 {
        summary.recall_bps = Some(mul_div_floor(tp, u128::from(BPS_DENOMINATOR), tp + fnn) as u16);
    }
    // 2TP / (2TP + FP + FN): the harmonic mean written so it never divides
    // twice and never loses a digit to the first division.
    if 2 * tp + fp + fnn > 0 {
        summary.f1_bps =
            Some(mul_div_floor(2 * tp, u128::from(BPS_DENOMINATOR), 2 * tp + fp + fnn) as u16);
    }
    if tp + tn + fp + fnn > 0 {
        summary.accuracy_bps =
            Some(mul_div_floor(tp + tn, u128::from(BPS_DENOMINATOR), tp + tn + fp + fnn) as u16);
    }
    if summary.labelled_rugs > 0 {
        let avoided = u128::from(summary.labelled_rugs - summary.entered_labelled_rugs);
        summary.rug_avoidance_bps = Some(mul_div_floor(
            avoided,
            u128::from(BPS_DENOMINATOR),
            u128::from(summary.labelled_rugs),
        ) as u16);
    }

    summary.pnl_on_labelled_rugs_lamports =
        rug_pnl.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
    summary.pnl_on_labelled_non_rugs_lamports =
        non_rug_pnl.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
    summary
}

// ===========================================================================
// Reading a fixture directory
// ===========================================================================

/// One fixture stream, already in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixtureSource {
    /// What the chain's genesis was computed from. The file stem, for a
    /// directory run.
    pub stream_id: String,
    /// What to call it in the report.
    pub file: String,
    pub text: String,
}

/// Evaluates streams that are already in memory, in the order given.
pub fn evaluate_streams(
    sources: &[FixtureSource],
    config: BacktestConfig,
    source: &str,
) -> ForensicReport {
    evaluate_streams_with(sources, config, source, None)
}

/// The same, with the directory's manifest to check the result against.
pub fn evaluate_streams_with(
    sources: &[FixtureSource],
    config: BacktestConfig,
    source: &str,
    manifest: Option<Manifest>,
) -> ForensicReport {
    let mut evaluator = Evaluator::new(config);
    if let Some(manifest) = manifest {
        evaluator = evaluator.with_manifest(manifest);
    }
    for fixture in sources {
        evaluator.ingest(&fixture.stream_id, &fixture.file, &fixture.text);
    }
    evaluator.finish(source)
}

/// Reads `manifest.json` from a fixture directory, if it has one.
///
/// The reading itself is `Manifest::read_dir`, next to the type it produces and
/// to the cursor the manifest describes. This is the harness's view of it: the
/// same refusals, carried in the error type the rest of this file reports
/// through, so a broken manifest still ends a run with the file named.
pub fn read_manifest(dir: &Path) -> Result<Option<Manifest>, BacktestError> {
    Manifest::read_dir(dir).map_err(|err| BacktestError::Io {
        path: err.path,
        detail: err.detail,
    })
}

/// Lists the `.jsonl` files in a directory, in file-name order.
///
/// Sorted, because `read_dir` hands them back in whatever order the filesystem
/// felt like and a run whose segment order depends on the filesystem is a run
/// that is not reproducible on another machine.
pub fn fixture_files(dir: &Path) -> Result<Vec<PathBuf>, BacktestError> {
    let entries = std::fs::read_dir(dir).map_err(|err| BacktestError::Io {
        path: dir.display().to_string(),
        detail: err.to_string(),
    })?;

    let mut files: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| BacktestError::Io {
            path: dir.display().to_string(),
            detail: err.to_string(),
        })?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
    files.sort();
    if files.is_empty() {
        return Err(BacktestError::NoFixtures {
            path: dir.display().to_string(),
        });
    }
    Ok(files)
}

/// Runs a whole fixture directory.
///
/// A file that is not a `.jsonl` is ignored rather than refused: a fixture
/// directory also holds a manifest, and refusing the run because the manifest is
/// not a stream would be an odd way to read a manifest.
pub fn evaluate_directory(
    dir: &Path,
    config: BacktestConfig,
) -> Result<ForensicReport, BacktestError> {
    let manifest = read_manifest(dir)?;
    let files = fixture_files(dir)?;
    let mut sources = Vec::with_capacity(files.len());
    for path in files {
        let text = std::fs::read_to_string(&path).map_err(|err| BacktestError::Io {
            path: path.display().to_string(),
            detail: err.to_string(),
        })?;
        // With a manifest, every file is a segment of the one stream it names.
        // Without one, each file stands alone and its stem is its stream id.
        let stream_id = match &manifest {
            Some(manifest) => manifest.stream_id.clone(),
            None => path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("stream")
                .to_string(),
        };
        sources.push(FixtureSource {
            file: path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("stream.jsonl")
                .to_string(),
            stream_id,
            text,
        });
    }
    Ok(evaluate_streams_with(
        &sources,
        config,
        &dir.display().to_string(),
        manifest,
    ))
}

// ===========================================================================
// The command line
// ===========================================================================

/// `sts backtest`: the harness as a command.
///
/// Hand-parsed rather than pulled from a dependency, for the reason the vendored
/// SHA-256 next door gives: this is a hundred lines of flag matching, and
/// `Cargo.toml` is a file several sessions are editing.
///
/// Everything writes through the `Write` handles it is given rather than to
/// `println!`, so the whole command is callable from a test with two `Vec<u8>`s
/// and no subprocess. A CLI that can only be tested by running it is a CLI that
/// is tested last.
pub mod cli {
    use super::*;
    use std::io::Write;

    use crate::fixtures::{self, FixtureError, GeneratorConfig, Scenario};
    use crate::walkforward;

    /// What `sts backtest` and `sts backtest --help` print.
    pub const USAGE: &str = "\
sts backtest — evaluate recorded fixtures

USAGE
  sts backtest verify   --fixtures <dir> [--out <file>]
  sts backtest run      --fixtures <dir> [--out <file>] [options]
  sts backtest sandwich [--reserves-sol <list>] [options]
  sts backtest generate --out <dir> [--scenario <name>] [options]
  sts backtest walk-forward --fixtures <dir> [--out <dir>] [options]

COMMANDS
  verify     Walk every JSONL line, check the hash chain, and report what
             failed. Reads no economics and makes no trades.
  run        Verify, then replay the events and price every decision. Writes
             the forensic JSON report.
  sandwich   Print the beta = phi / (1 - phi) threshold table for a list of
             curve positions. Reads no fixture.
  generate   Build the synthetic stress corpus and write it out: coordinated
             Sybil rugs, entries laddered across the extraction threshold,
             saturated queues, and chains broken one way each. One directory
             per case, each holding its streams, its manifest, and the
             expected.json saying what the harness should conclude.
  walk-forward
             Cut the corpus into time-ordered folds with a purge, an embargo
             and a whole-group split, and measure each test fold over the
             gap x slippage grid. Reports the two assertions the split turns
             on, the wallet overlap it cannot drive to zero, and a lower
             bound taken at a level divided by how many cells were looked at.
             Nothing is fitted on the training side; it is reported so a
             divergence is visible.

OPTIONS
  --fixtures <dir>              Directory of .jsonl fixture streams.
  --out <file>                  Write the report here instead of stdout. For
                                generate this is a directory, and the corpus
                                is written under it.
  --gate                        Refuse anything that did not fully verify.
  --fee-bps <n>                 Swap fee on the SOL leg. Default 100.
  --sol-usd-cents <n>           SOL price in whole US cents. Default 0, which
                                leaves every dollar figure at zero rather than
                                guessing at a price.
  --starting-lamports <n>       Opening equity for the drawdown curve.
  --landing-cost-lamports <n>   The modelled attacker's fixed landing cost.
  --max-attacker-lamports <n>   The modelled attacker's capital cap.
  --tau-sync-ms <n>             Buy-synchrony bandwidth. Default 5000.
  --min-cluster-wallets <n>     Smallest shared-funder group to report.
  --rug-drop-bps <n>            Fall from peak that counts as a rug.
  --rug-window-ms <n>           Window that fall has to happen inside.
  --fade-drop-bps <n>           Fall from peak that counts as a fade.
  --reserves-sol <list>         Comma-separated virtual SOL reserves, whole
                                SOL, for the sandwich table. Default 30,75,115.
  --victim-lamports <n>         Victim buy to price in the sandwich table.
                                Default is each position's own threshold.
  --scenario <name>             What generate builds. Default all; otherwise
                                one of graduation, sybil-rug,
                                sandwich-boundary, backpressure,
                                chain-corruption.
  --seed <text>                 Generator draw seed. Default 0x100x. Every
                                random number is addressed by seed, mint,
                                label and index, so the corpus is a function
                                of this and the flags and nothing else.
  --sybil-wallets <n>           Wallets in the coordinated bundle. Minimum 5.
  --organic-wallets <n>         Independent buyers for the bundle to hide
                                among.
  --segments <n>                Files to rotate each generated stream into.
                                Default 1. The chain runs across the roll.
  --force                       Let generate replace the streams in a case
                                directory that already exists.
  --first-at-ms <n>             Wall clock the generated recording starts at.
                                Default 1700000000000. A corpus that spans
                                time is several generate runs at several
                                starts, which is what walk-forward needs.
  --first-slot <n>              Slot the generated recording starts at.
                                Default 300000000.
  --folds <n>                   Blocks walk-forward cuts the corpus into. The
                                first is training-only, so n blocks give n-1
                                test folds. Default 5.
  --purge [<duration>]          Affirms the purge, which is applied whether or
                                not this is given: a training record whose
                                outcome window reaches the test window's start
                                is leakage. A duration widens it.
  --embargo [<duration>]        The interval held out after each training
                                fold. Default 1h, and that is policy. Zero
                                turns it off and leaves the purge.
  --group-by <rule>             funder, deployer or none. Default funder:
                                whole funding components go to one side.
  --gaps <list>                 Gap buckets as whole percentages. Default
                                30,50.
  --slippage <list>             Execution-drag buckets, whole percentages.
                                Default 10,15,20,25.
  --cvar-pct <n>                The tail the CVaR averages over. Default 5.
  --alpha-bps <n>               Family-wise error rate for the lower bounds,
                                in basis points. Default 500. The per-cell
                                level is this divided by the number of cells
                                across every fold.

DURATIONS
  A whole number of milliseconds, or one suffixed h, m, s or ms.

EXIT CODES
  0  the run finished and, under --gate, verified
  1  the command line could not be read
  2  the corpus did not verify and --gate was given
  3  a file could not be read or written
";

    /// Whether `sts <name>` belongs to this module.
    ///
    /// `main` asks before it builds a window, so a GUI launch with no arguments
    /// — and a launch from Finder, which passes its own — never lands here.
    pub fn is_subcommand(name: &str) -> bool {
        name == "backtest"
    }

    /// A flag list that reports what it could not read rather than panicking.
    struct Flags {
        values: Vec<(String, String)>,
        bare: Vec<String>,
    }

    impl Flags {
        fn parse(args: &[String]) -> Result<Self, String> {
            let mut values = Vec::new();
            let mut bare = Vec::new();
            let mut index = 0;
            while index < args.len() {
                let arg = &args[index];
                if let Some(name) = arg.strip_prefix("--") {
                    if let Some((name, inline)) = name.split_once('=') {
                        values.push((name.to_string(), inline.to_string()));
                        index += 1;
                        continue;
                    }
                    // Boolean flags take no value; everything else takes one.
                    if matches!(name, "gate" | "help" | "force") {
                        values.push((name.to_string(), "true".to_string()));
                        index += 1;
                        continue;
                    }
                    // Two flags mean something on their own and may also carry
                    // a value. `STS_ROADMAP.md` Phase 3 writes `--purge
                    // --embargo`; `REPLAY_AND_SIMULATION_SPEC.md` §29 writes
                    // `--embargo 1h`. Those are the same flag, and a parser that
                    // accepted only one spelling would make one of the two
                    // documented commands a typo.
                    if matches!(name, "purge" | "embargo") {
                        match args.get(index + 1) {
                            Some(value) if !value.starts_with("--") => {
                                values.push((name.to_string(), value.clone()));
                                index += 2;
                            }
                            _ => {
                                values.push((name.to_string(), "default".to_string()));
                                index += 1;
                            }
                        }
                        continue;
                    }
                    let value = args
                        .get(index + 1)
                        .ok_or_else(|| format!("--{name} needs a value"))?;
                    if value.starts_with("--") {
                        return Err(format!("--{name} needs a value"));
                    }
                    values.push((name.to_string(), value.clone()));
                    index += 2;
                } else {
                    bare.push(arg.clone());
                    index += 1;
                }
            }
            Ok(Flags { values, bare })
        }

        fn get(&self, name: &str) -> Option<&str> {
            self.values
                .iter()
                .rev()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        }

        fn has(&self, name: &str) -> bool {
            self.get(name).is_some()
        }

        fn number<T: std::str::FromStr>(&self, name: &str, fallback: T) -> Result<T, String> {
            match self.get(name) {
                None => Ok(fallback),
                Some(text) => text
                    .parse::<T>()
                    .map_err(|_| format!("--{name} is not a number: {text}")),
            }
        }

        /// Flags that were given and that this command does not know.
        ///
        /// Refused rather than ignored. A typo in `--starting-lamports` that is
        /// silently dropped produces a report computed against the default
        /// opening equity, and nothing in the output says so.
        fn unknown(&self, known: &[&str]) -> Vec<String> {
            self.values
                .iter()
                .map(|(name, _)| name.as_str())
                .filter(|name| !known.contains(name))
                .map(|name| format!("--{name}"))
                .collect()
        }
    }

    const RUN_FLAGS: [&str; 13] = [
        "fixtures",
        "out",
        "gate",
        "fee-bps",
        "sol-usd-cents",
        "starting-lamports",
        "landing-cost-lamports",
        "max-attacker-lamports",
        "tau-sync-ms",
        "min-cluster-wallets",
        "rug-drop-bps",
        "rug-window-ms",
        "fade-drop-bps",
    ];

    fn config_from(flags: &Flags) -> Result<BacktestConfig, String> {
        let base = BacktestConfig::default();
        Ok(BacktestConfig {
            fee_bps: flags.number("fee-bps", base.fee_bps)?,
            cents_per_sol: flags.number("sol-usd-cents", base.cents_per_sol)?,
            starting_equity_lamports: flags
                .number("starting-lamports", base.starting_equity_lamports)?,
            landing_cost_lamports: flags
                .number("landing-cost-lamports", base.landing_cost_lamports)?,
            max_attacker_lamports: flags
                .number("max-attacker-lamports", base.max_attacker_lamports)?,
            tau_sync_ms: flags.number("tau-sync-ms", base.tau_sync_ms)?,
            min_cluster_wallets: flags.number("min-cluster-wallets", base.min_cluster_wallets)?,
            rug_drop_bps: flags.number("rug-drop-bps", base.rug_drop_bps)?,
            rug_window_ms: flags.number("rug-window-ms", base.rug_window_ms)?,
            fade_drop_bps: flags.number("fade-drop-bps", base.fade_drop_bps)?,
            gate: flags.has("gate"),
        })
    }

    /// The chain audit on its own, with no economics attached.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct VerifyReport {
        pub schema: String,
        pub source: String,
        pub manifest: Option<ManifestCheck>,
        pub integrity: IntegritySummary,
        pub streams: Vec<StreamReport>,
        /// Why this corpus may not back a gate dossier. Empty when it may.
        pub refusals: Vec<String>,
    }

    /// One row of the `β = φ / (1 - φ)` table.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SandwichRow {
        pub virtual_sol_reserves: u64,
        pub fee_bps: u16,
        pub beta_threshold_micros: u64,
        pub breakeven_victim_lamports: u64,
        pub verdict: SandwichVerdict,
    }

    /// The whole table, plus what it was computed at.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SandwichTable {
        pub schema: String,
        pub fee_bps: u16,
        pub landing_cost_lamports: u64,
        pub max_attacker_lamports: u64,
        pub rows: Vec<SandwichRow>,
    }

    fn emit(
        text: &str,
        destination: Option<&str>,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> i32 {
        match destination {
            None => {
                let _ = out.write_all(text.as_bytes());
                0
            }
            Some(path) => match std::fs::write(path, text) {
                Ok(()) => {
                    let _ = writeln!(out, "wrote {path}");
                    0
                }
                Err(error) => {
                    let _ = writeln!(err, "sts backtest: {path}: {error}");
                    3
                }
            },
        }
    }

    /// Runs `sts backtest ...`. `args` starts with the subcommand name.
    pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
        let Some(command) = args.first().map(String::as_str) else {
            let _ = out.write_all(USAGE.as_bytes());
            return 1;
        };
        if command == "backtest" {
            return run(&args[1..], out, err);
        }
        if matches!(command, "help" | "--help" | "-h") {
            let _ = out.write_all(USAGE.as_bytes());
            return 0;
        }

        let flags = match Flags::parse(&args[1..]) {
            Ok(flags) => flags,
            Err(detail) => {
                let _ = writeln!(err, "sts backtest: {detail}");
                return 1;
            }
        };
        if flags.has("help") {
            let _ = out.write_all(USAGE.as_bytes());
            return 0;
        }
        if let Some(extra) = flags.bare.first() {
            let _ = writeln!(err, "sts backtest: unexpected argument {extra:?}");
            return 1;
        }

        match command {
            "verify" => verify(&flags, out, err),
            "run" => evaluate(&flags, out, err),
            "sandwich" => sandwich(&flags, out, err),
            "generate" => generate(&flags, out, err),
            "walk-forward" => walk_forward(&flags, out, err),
            other => {
                let _ = writeln!(err, "sts backtest: unknown command {other:?}");
                let _ = err.write_all(USAGE.as_bytes());
                1
            }
        }
    }

    fn fixtures_dir(flags: &Flags, err: &mut dyn Write) -> Option<PathBuf> {
        match flags.get("fixtures") {
            Some(path) => Some(PathBuf::from(path)),
            None => {
                let _ = writeln!(err, "sts backtest: --fixtures is required");
                None
            }
        }
    }

    fn refuse_unknown(flags: &Flags, known: &[&str], err: &mut dyn Write) -> bool {
        let unknown = flags.unknown(known);
        if unknown.is_empty() {
            return false;
        }
        let _ = writeln!(
            err,
            "sts backtest: unknown option(s): {}",
            unknown.join(", ")
        );
        true
    }

    fn verify(flags: &Flags, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
        if refuse_unknown(flags, &["fixtures", "out", "gate"], err) {
            return 1;
        }
        let Some(dir) = fixtures_dir(flags, err) else {
            return 1;
        };

        // Verification reads no economics, so it runs at the default policy and
        // never in gate mode: the point is to see everything that is wrong, and
        // gate mode stops reading at the first thing that is.
        let report = match evaluate_directory(&dir, BacktestConfig::default()) {
            Ok(report) => report,
            Err(error) => {
                let _ = writeln!(err, "sts backtest: {error}");
                return 3;
            }
        };

        let verify = VerifyReport {
            schema: REPORT_SCHEMA.to_string(),
            source: report.source.clone(),
            manifest: report.manifest.clone(),
            integrity: report.integrity.clone(),
            streams: report.streams.clone(),
            refusals: report.refusals.clone(),
        };
        let mut text = serde_json::to_string_pretty(&verify)
            .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}"));
        text.push('\n');

        let code = emit(&text, flags.get("out"), out, err);
        if code != 0 {
            return code;
        }
        if !verify.integrity.gate_ready {
            let _ = writeln!(
                err,
                "sts backtest: {} of {} record(s) did not verify",
                verify.integrity.rejected + verify.integrity.unverifiable,
                verify.integrity.records
            );
            return 2;
        }
        if !verify.refusals.is_empty() {
            let _ = writeln!(
                err,
                "sts backtest: {}",
                BacktestError::Refused(verify.refusals.clone())
            );
            return 2;
        }
        0
    }

    fn evaluate(flags: &Flags, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
        if refuse_unknown(flags, &RUN_FLAGS, err) {
            return 1;
        }
        let Some(dir) = fixtures_dir(flags, err) else {
            return 1;
        };
        let config = match config_from(flags) {
            Ok(config) => config,
            Err(detail) => {
                let _ = writeln!(err, "sts backtest: {detail}");
                return 1;
            }
        };

        let report = match evaluate_directory(&dir, config) {
            Ok(report) => report,
            Err(error) => {
                let _ = writeln!(err, "sts backtest: {error}");
                return 3;
            }
        };

        let code = emit(&report.to_json(), flags.get("out"), out, err);
        if code != 0 {
            return code;
        }
        if config.gate && !report.gate_ready {
            let _ = writeln!(
                err,
                "sts backtest: {}",
                BacktestError::Refused(report.refusals.clone())
            );
            return 2;
        }
        0
    }

    /// The flags `walk-forward` adds on top of the pricing ones.
    const WALK_FORWARD_FLAGS: [&str; 8] = [
        "folds",
        "purge",
        "embargo",
        "group-by",
        "gaps",
        "slippage",
        "cvar-pct",
        "alpha-bps",
    ];

    /// The file a `--out` directory receives.
    const WALK_FORWARD_FILE: &str = "walk-forward.json";

    fn walk_forward_config_from(flags: &Flags) -> Result<walkforward::WalkForwardConfig, String> {
        let base = walkforward::WalkForwardConfig::default();
        // The purge is unconditional: a training record whose outcome window
        // reaches the test window's start is leakage by definition, and a switch
        // that turned that off would be a switch for producing a number nobody
        // may quote. `--purge` affirms it, and a duration widens it.
        let purge_ms = match flags.get("purge") {
            None | Some("default") | Some("true") => base.purge_ms,
            Some(text) => {
                walkforward::parse_duration_ms(text).map_err(|e| format!("--purge: {e}"))?
            }
        };
        let embargo_ms = match flags.get("embargo") {
            None | Some("default") | Some("true") => base.embargo_ms,
            Some(text) => {
                walkforward::parse_duration_ms(text).map_err(|e| format!("--embargo: {e}"))?
            }
        };
        let group_by = match flags.get("group-by") {
            None => base.group_by,
            Some(text) => walkforward::GroupBy::parse(text)
                .ok_or_else(|| format!("--group-by is not a rule: {text}"))?,
        };
        let gaps_bps = match flags.get("gaps") {
            None => base.gaps_bps,
            Some(text) => {
                walkforward::parse_percent_list(text).map_err(|e| format!("--gaps: {e}"))?
            }
        };
        let slippage_bps = match flags.get("slippage") {
            None => base.slippage_bps,
            Some(text) => {
                walkforward::parse_percent_list(text).map_err(|e| format!("--slippage: {e}"))?
            }
        };
        let cvar_pct: u32 = flags.number("cvar-pct", base.cvar_pct)?;
        if cvar_pct == 0 || cvar_pct > 100 {
            return Err(format!("--cvar-pct is not a tail: {cvar_pct}"));
        }
        let family_alpha_bps: u16 = flags.number("alpha-bps", base.family_alpha_bps)?;
        if family_alpha_bps == 0 || u32::from(family_alpha_bps) >= BPS_DENOMINATOR {
            return Err(format!("--alpha-bps is not a level: {family_alpha_bps}"));
        }
        let folds: usize = flags.number("folds", base.folds)?;

        Ok(walkforward::WalkForwardConfig {
            folds,
            purge_ms,
            embargo_ms,
            group_by,
            gaps_bps,
            slippage_bps,
            cvar_pct,
            family_alpha_bps,
            backtest: config_from(flags)?,
        })
    }

    fn walk_forward(flags: &Flags, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
        let known: Vec<&str> = RUN_FLAGS
            .iter()
            .copied()
            .chain(WALK_FORWARD_FLAGS.iter().copied())
            .collect();
        if refuse_unknown(flags, &known, err) {
            return 1;
        }
        let Some(dir) = fixtures_dir(flags, err) else {
            return 1;
        };
        let config = match walk_forward_config_from(flags) {
            Ok(config) => config,
            Err(detail) => {
                let _ = writeln!(err, "sts backtest: {detail}");
                return 1;
            }
        };

        let corpus = match walkforward::read_corpus(&dir, config.backtest) {
            Ok(corpus) => corpus,
            Err(error) => {
                let _ = writeln!(err, "sts backtest: {error}");
                return 3;
            }
        };
        let report = walkforward::evaluate(&corpus, &config);

        // `--out` is a directory here rather than a file, because that is what
        // the roadmap's own command passes it — `--out reports/phase3` — and
        // because a phase dossier is several files even when this one writes one.
        let code = match flags.get("out") {
            None => {
                let _ = out.write_all(report.to_json().as_bytes());
                0
            }
            Some(path) => {
                let dir = PathBuf::from(path);
                if let Err(error) = std::fs::create_dir_all(&dir) {
                    let _ = writeln!(err, "sts backtest: {path}: {error}");
                    3
                } else {
                    let file = dir.join(WALK_FORWARD_FILE);
                    match std::fs::write(&file, report.to_json()) {
                        Ok(()) => {
                            let _ = writeln!(out, "wrote {}", file.display());
                            0
                        }
                        Err(error) => {
                            let _ = writeln!(err, "sts backtest: {}: {error}", file.display());
                            3
                        }
                    }
                }
            }
        };
        if code != 0 {
            return code;
        }

        // The split's own verdict is printed to stderr whatever the gate says,
        // because a report that may not be quoted is worth saying out loud
        // rather than leaving in a field somebody has to look for.
        if !report.gate_ready {
            let _ = writeln!(
                err,
                "sts backtest: {}",
                BacktestError::Refused(report.refusals.clone())
            );
            if config.backtest.gate {
                return 2;
            }
        }
        0
    }

    fn sandwich(flags: &Flags, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
        const KNOWN: [&str; 6] = [
            "reserves-sol",
            "victim-lamports",
            "fee-bps",
            "landing-cost-lamports",
            "max-attacker-lamports",
            "out",
        ];
        if refuse_unknown(flags, &KNOWN, err) {
            return 1;
        }

        let base = BacktestConfig::default();
        let fee_bps: u16 = match flags.number("fee-bps", base.fee_bps) {
            Ok(value) => value,
            Err(detail) => {
                let _ = writeln!(err, "sts backtest: {detail}");
                return 1;
            }
        };
        let landing = match flags.number("landing-cost-lamports", base.landing_cost_lamports) {
            Ok(value) => value,
            Err(detail) => {
                let _ = writeln!(err, "sts backtest: {detail}");
                return 1;
            }
        };
        let cap = match flags.number("max-attacker-lamports", base.max_attacker_lamports) {
            Ok(value) => value,
            Err(detail) => {
                let _ = writeln!(err, "sts backtest: {detail}");
                return 1;
            }
        };

        // The three positions §15.2's table is written at.
        let reserves_text = flags.get("reserves-sol").unwrap_or("30,75,115");
        let mut reserves: Vec<u64> = Vec::new();
        for piece in reserves_text.split(',') {
            let piece = piece.trim();
            if piece.is_empty() {
                continue;
            }
            match piece.parse::<u64>() {
                Ok(sol) => reserves.push(sol.saturating_mul(LAMPORTS_PER_SOL)),
                Err(_) => {
                    let _ = writeln!(err, "sts backtest: --reserves-sol is not a number: {piece}");
                    return 1;
                }
            }
        }
        if reserves.is_empty() {
            let _ = writeln!(err, "sts backtest: --reserves-sol listed no positions");
            return 1;
        }

        let victim_override: Option<u64> = match flags.number::<u64>("victim-lamports", 0) {
            Ok(0) => None,
            Ok(value) => Some(value),
            Err(detail) => {
                let _ = writeln!(err, "sts backtest: {detail}");
                return 1;
            }
        };

        let mut rows = Vec::with_capacity(reserves.len());
        for virtual_sol in reserves {
            // The curve at this virtual reserve, reached from the launch state,
            // so real reserves and the executable-exit checks are consistent
            // with it rather than invented alongside it.
            let real_sol = virtual_sol.saturating_sub(base_virtual_sol());
            let state = CurveState::at_real_sol(real_sol);
            let breakeven = sandwich_breakeven_victim_lamports(state.virtual_sol_reserves, fee_bps);
            // One lamport above the threshold is where §15.2 says the sign
            // appears, and it is the smallest victim worth tabulating.
            let victim = victim_override.unwrap_or_else(|| breakeven.saturating_add(1));
            rows.push(SandwichRow {
                virtual_sol_reserves: state.virtual_sol_reserves,
                fee_bps,
                beta_threshold_micros: beta_threshold_micros(fee_bps),
                breakeven_victim_lamports: breakeven,
                verdict: assess_sandwich(&state, victim, fee_bps, landing, cap),
            });
        }

        let table = SandwichTable {
            schema: "sts.backtest.sandwich.v1".to_string(),
            fee_bps,
            landing_cost_lamports: landing,
            max_attacker_lamports: cap,
            rows,
        };
        let mut text = serde_json::to_string_pretty(&table)
            .unwrap_or_else(|error| format!("{{\"error\":\"{error}\"}}"));
        text.push('\n');
        emit(&text, flags.get("out"), out, err)
    }

    const GENERATE_FLAGS: [&str; 10] = [
        "out",
        "scenario",
        "seed",
        "fee-bps",
        "sybil-wallets",
        "organic-wallets",
        "segments",
        "force",
        "first-slot",
        "first-at-ms",
    ];

    fn generator_config_from(flags: &Flags) -> Result<GeneratorConfig, String> {
        let base = GeneratorConfig::default();
        let seed = match flags.get("seed") {
            Some(text) => text.to_string(),
            None => base.seed.clone(),
        };
        Ok(GeneratorConfig {
            seed,
            // The generator prices its own boundary cases at this fee, so a run
            // that evaluates the corpus at a different one is evaluating a
            // boundary nobody built.
            fee_bps: flags.number("fee-bps", base.fee_bps)?,
            sybil_wallets: flags.number("sybil-wallets", base.sybil_wallets)?,
            organic_wallets: flags.number("organic-wallets", base.organic_wallets)?,
            segments: flags.number("segments", base.segments)?,
            // Where the recording's clock and its slot counter start. Exposed
            // because a walk-forward needs a corpus that spans time, and one
            // invocation of `generate` writes every case at one start: a corpus
            // with folds in it is several invocations at several starts, and
            // without these two flags there is no way to ask for that.
            first_slot: flags.number("first-slot", base.first_slot)?,
            first_at_ms: flags.number("first-at-ms", base.first_at_ms)?,
            provider: base.provider,
        })
    }

    /// `sts backtest generate`: writes the synthetic stress corpus.
    ///
    /// Nothing here reads a fixture. It is the other end of the same pipe —
    /// what it writes is exactly what `verify` and `run` are built to read, and
    /// the round trip is a test rather than a hope.
    fn generate(flags: &Flags, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
        if refuse_unknown(flags, &GENERATE_FLAGS, err) {
            return 1;
        }
        let Some(dir) = flags.get("out").map(PathBuf::from) else {
            let _ = writeln!(err, "sts backtest: generate needs --out <dir>");
            return 1;
        };
        let config = match generator_config_from(flags) {
            Ok(config) => config,
            Err(detail) => {
                let _ = writeln!(err, "sts backtest: {detail}");
                return 1;
            }
        };

        let wanted = flags.get("scenario").unwrap_or("all");
        let built = if wanted == "all" {
            fixtures::generate_all(&config)
        } else {
            match Scenario::parse(wanted) {
                Some(scenario) => fixtures::generate(scenario, &config),
                None => Err(FixtureError::UnknownScenario {
                    name: wanted.to_string(),
                }),
            }
        };
        let cases = match built {
            Ok(cases) => cases,
            Err(error) => {
                let _ = writeln!(err, "sts backtest: {error}");
                // A knob that cannot describe a fixture is a command line
                // problem, not a disk problem.
                return 1;
            }
        };

        let written = match fixtures::write_corpus(&dir, &cases, flags.has("force")) {
            Ok(written) => written,
            Err(error) => {
                let _ = writeln!(err, "sts backtest: {error}");
                return 3;
            }
        };

        let _ = writeln!(out, "wrote {} case(s) to {}", written.len(), dir.display());
        for case in &cases {
            // The refusal note is on the line rather than in a footnote: half
            // this corpus exists to be refused, and a case list that did not
            // say which half would read as a corpus that half fails.
            let refused = if case.expected.gate_ready {
                ""
            } else {
                "  built to be refused"
            };
            let _ = writeln!(
                out,
                "  {:<30} {:>4} record(s) in {} file(s){refused}",
                case.name,
                case.expected.records,
                case.files.len(),
            );
        }
        0
    }

    /// The virtual SOL a pump.fun curve starts with, so a requested virtual
    /// reserve can be turned back into the real SOL that produced it.
    fn base_virtual_sol() -> u64 {
        CurveState::LAUNCH.virtual_sol_reserves
    }
}

// ===========================================================================
// tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::{write_stream, ChainWriter, RecordDraft, RecordKind};
    use std::fs;

    // -----------------------------------------------------------------------
    // building fixtures
    // -----------------------------------------------------------------------

    /// Seals records into a chain the way the recorder would.
    ///
    /// The slot advances on every record, so the §6 order key is strictly
    /// increasing without the test having to think about it, and a test that
    /// wants an out-of-order stream has to say so explicitly.
    struct FixtureBuilder {
        stream_id: String,
        writer: ChainWriter,
        records: Vec<ReplayRecord>,
        slot: u64,
        at_ms: i64,
    }

    impl FixtureBuilder {
        fn new(stream_id: &str) -> Self {
            FixtureBuilder {
                stream_id: stream_id.to_string(),
                writer: ChainWriter::new(stream_id),
                records: Vec::new(),
                slot: 1_000,
                at_ms: 1_700_000_000_000,
            }
        }

        fn push(&mut self, kind: RecordKind, frame: Option<Vec<u8>>, outcome: RecordOutcome) {
            let seq = self.records.len();
            let record = self.writer.seal(RecordDraft {
                event_id: format!("evt-{seq:06}"),
                slot: self.slot,
                observed_at_ms: self.at_ms,
                provider: "helius".to_string(),
                endpoint_index: 0,
                connection: 1,
                kind,
                frame,
                outcome,
                dispatch_latency_us: Some(120),
            });
            self.records.push(record);
            self.slot += 1;
            self.at_ms += 400;
        }

        fn event(&mut self, json: &str) -> &mut Self {
            self.push(
                RecordKind::Frame,
                Some(json.as_bytes().to_vec()),
                RecordOutcome::Accepted,
            );
            self
        }

        fn event_with(&mut self, json: &str, outcome: RecordOutcome) -> &mut Self {
            self.push(RecordKind::Frame, Some(json.as_bytes().to_vec()), outcome);
            self
        }

        fn lifecycle(&mut self, kind: RecordKind) -> &mut Self {
            self.push(kind, None, RecordOutcome::Accepted);
            self
        }

        fn text(&self) -> String {
            write_stream(&self.records)
        }

        /// The same records split into `parts` files, each a whole number of
        /// lines. §10's segmentation, done the way the recorder's rotation does
        /// it: the chain runs on across the boundary.
        fn segments(&self, parts: usize) -> Vec<String> {
            let per = self.records.len().div_ceil(parts.max(1));
            self.records.chunks(per.max(1)).map(write_stream).collect()
        }
    }

    fn launch_json(mint: &str, at_ms: i64, real_sol: u64) -> String {
        format!(
            "{{\"schema\":\"{EVENT_SCHEMA}\",\"kind\":\"launch\",\"mint\":\"{mint}\",\
             \"at_ms\":{at_ms},\"creator\":\"creator-1\",\"real_sol_lamports\":{real_sol}}}"
        )
    }

    fn buy_json(mint: &str, at_ms: i64, wallet: &str, funder: Option<&str>, gross: u64) -> String {
        let funder = match funder {
            Some(f) => format!("\"{f}\""),
            None => "null".to_string(),
        };
        format!(
            "{{\"schema\":\"{EVENT_SCHEMA}\",\"kind\":\"flow\",\"mint\":\"{mint}\",\
             \"at_ms\":{at_ms},\"wallet\":\"{wallet}\",\"funder\":{funder},\
             \"side\":\"buy\",\"gross_lamports\":{gross}}}"
        )
    }

    fn sell_json(mint: &str, at_ms: i64, wallet: &str, tokens: u64) -> String {
        format!(
            "{{\"schema\":\"{EVENT_SCHEMA}\",\"kind\":\"flow\",\"mint\":\"{mint}\",\
             \"at_ms\":{at_ms},\"wallet\":\"{wallet}\",\"side\":\"sell\",\"tokens\":{tokens}}}"
        )
    }

    fn entry_json(mint: &str, at_ms: i64, gross: u64) -> String {
        format!(
            "{{\"schema\":\"{EVENT_SCHEMA}\",\"kind\":\"entry\",\"mint\":\"{mint}\",\
             \"at_ms\":{at_ms},\"gross_lamports\":{gross}}}"
        )
    }

    fn exit_json(mint: &str, at_ms: i64, tokens: Option<u64>) -> String {
        let tokens = match tokens {
            Some(t) => t.to_string(),
            None => "null".to_string(),
        };
        format!(
            "{{\"schema\":\"{EVENT_SCHEMA}\",\"kind\":\"exit\",\"mint\":\"{mint}\",\
             \"at_ms\":{at_ms},\"tokens\":{tokens}}}"
        )
    }

    fn pull_json(mint: &str, at_ms: i64, lamports: u64) -> String {
        format!(
            "{{\"schema\":\"{EVENT_SCHEMA}\",\"kind\":\"pull\",\"mint\":\"{mint}\",\
             \"at_ms\":{at_ms},\"wallet\":\"creator-1\",\"lamports\":{lamports}}}"
        )
    }

    fn holders_json(mint: &str, at_ms: i64, balances: &[(&str, u64)]) -> String {
        let entries: Vec<String> = balances
            .iter()
            .map(|(wallet, balance)| format!("{{\"wallet\":\"{wallet}\",\"balance\":{balance}}}"))
            .collect();
        format!(
            "{{\"schema\":\"{EVENT_SCHEMA}\",\"kind\":\"holders\",\"mint\":\"{mint}\",\
             \"at_ms\":{at_ms},\"holders\":[{}]}}",
            entries.join(",")
        )
    }

    fn label_json(mint: &str, outcome: RugClass) -> String {
        format!(
            "{{\"schema\":\"{EVENT_SCHEMA}\",\"kind\":\"label\",\"mint\":\"{mint}\",\
             \"outcome\":\"{}\"}}",
            outcome.as_str()
        )
    }

    fn one_stream(stream_id: &str, text: &str) -> Vec<FixtureSource> {
        vec![FixtureSource {
            stream_id: stream_id.to_string(),
            file: format!("{stream_id}.jsonl"),
            text: text.to_string(),
        }]
    }

    /// The manifest §3.2 describes, for a fixture that was recorded in one
    /// piece and then rotated into `parts` segments.
    fn manifest_for(fixture: &FixtureBuilder, complete: bool) -> Manifest {
        let mut manifest = Manifest::for_records(
            &fixture.stream_id,
            &fixture.records,
            fixture.writer.head(),
            0,
        );
        manifest.complete = complete;
        manifest
    }

    fn config() -> BacktestConfig {
        BacktestConfig {
            cents_per_sol: 15_000,
            ..BacktestConfig::default()
        }
    }

    /// A scratch directory that is cleared going in as well as coming out, so a
    /// test that panicked last run does not poison the next one.
    struct Scratch {
        path: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("sts-backtest-tests/{name}"));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("the scratch directory could not be created");
            Scratch { path }
        }

        fn write(&self, name: &str, text: &str) {
            fs::write(self.path.join(name), text).expect("the fixture could not be written");
        }

        fn path(&self) -> &Path {
            &self.path
        }

        /// Writes a fixture directory: the segments plus the manifest that says
        /// which stream they are segments of.
        fn write_fixture(&self, fixture: &FixtureBuilder, parts: usize, complete: bool) {
            for (index, text) in fixture.segments(parts).into_iter().enumerate() {
                self.write(&format!("{index:03}.jsonl"), &text);
            }
            self.write(
                "manifest.json",
                &serde_json::to_string_pretty(&manifest_for(fixture, complete))
                    .expect("the manifest serialises"),
            );
        }

        fn as_arg(&self) -> String {
            self.path.display().to_string()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// The fixture most tests below run against: a launch, other people's flow,
    /// one entry, one exit, a holder snapshot and a label.
    fn round_trip_fixture() -> FixtureBuilder {
        let mut builder = FixtureBuilder::new("phase3-a");
        builder
            .event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL))
            .event(&buy_json(
                "MINT1",
                1_400,
                "wallet-a",
                Some("funder-1"),
                500_000_000,
            ))
            .event(&buy_json(
                "MINT1",
                1_600,
                "wallet-b",
                Some("funder-1"),
                250_000_000,
            ))
            .event(&entry_json("MINT1", 2_000, LAMPORTS_PER_SOL))
            .event(&buy_json("MINT1", 2_400, "wallet-c", None, 750_000_000))
            .event(&exit_json("MINT1", 9_000, None))
            .event(&holders_json(
                "MINT1",
                9_500,
                &[("wallet-a", 600), ("wallet-b", 300), ("wallet-c", 100)],
            ))
            .event(&label_json("MINT1", RugClass::Held));
        builder
    }

    // -----------------------------------------------------------------------
    // fixed-point arithmetic
    // -----------------------------------------------------------------------

    #[test]
    fn integer_square_root_is_exact_on_squares_and_floors_between_them() {
        assert_eq!(isqrt(0), 0);
        assert_eq!(isqrt(1), 1);
        assert_eq!(isqrt(2), 1);
        assert_eq!(isqrt(3), 1);
        assert_eq!(isqrt(4), 2);
        for n in 1u128..2_000 {
            let square = n * n;
            assert_eq!(isqrt(square), n, "sqrt({square})");
            assert_eq!(isqrt(square - 1), n - 1, "sqrt({} )", square - 1);
            assert_eq!(isqrt(square + 1), n, "sqrt({})", square + 1);
        }
    }

    #[test]
    fn integer_square_root_survives_the_top_of_the_range() {
        // The one input where a naive Newton start overflows.
        let root = isqrt(u128::MAX);
        assert_eq!(root, (1u128 << 64) - 1);
        // Stated as the two facts that make that number the floor rather than
        // as `root * root <= u128::MAX`, which every u128 satisfies and which
        // therefore holds just as well for a root that came back too small.
        // The bound at the top of the range is the overflow, so that is what
        // is asserted: the square fits and the next one up does not.
        assert!(
            root.checked_mul(root).is_some(),
            "the root squared overflows"
        );
        assert!(
            (root + 1).checked_mul(root + 1).is_none(),
            "a larger root would square without overflowing, so this one is not the floor"
        );
    }

    #[test]
    fn the_exponential_matches_its_known_values() {
        // exp(0) is one exactly, which the synchrony kernel depends on: two
        // wallets that bought in the same millisecond must score 1.0, not
        // 0.999999.
        assert_eq!(exp_neg_micros(0), MICROS);
        // Reference values, rounded to the nearest millionth.
        assert_eq!(exp_neg_micros(500_000), 606_531); // exp(-0.5)
        assert_eq!(exp_neg_micros(1_000_000), 367_879); // exp(-1)
        assert_eq!(exp_neg_micros(2_000_000), 135_335); // exp(-2)
        assert_eq!(exp_neg_micros(5_000_000), 6_738); // exp(-5)
        assert_eq!(exp_neg_micros(10_000_000), 45); // exp(-10)
        assert_eq!(exp_neg_micros(20_000_000), 0); // exp(-20) rounds to nothing
        assert_eq!(exp_neg_micros(u64::MAX), 0);
    }

    #[test]
    fn the_exponential_never_increases() {
        let mut previous = MICROS + 1;
        for x in (0..30_000_000u64).step_by(7_919) {
            let value = exp_neg_micros(x);
            assert!(
                value <= previous,
                "exp(-{x}) rose to {value} from {previous}"
            );
            previous = value;
        }
    }

    #[test]
    fn signed_division_rounds_towards_negative_infinity() {
        assert_eq!(floor_div_i128(7, 2), 3);
        assert_eq!(floor_div_i128(-7, 2), -4);
        assert_eq!(floor_div_i128(-8, 2), -4);
        assert_eq!(floor_div_i128(8, 2), 4);
        assert_eq!(floor_div_i128(1, 0), 0);
    }

    #[test]
    fn a_loss_keeps_its_fraction_of_a_cent_and_a_gain_loses_it() {
        // One lamport at $150/SOL is 0.000015 cents, so both of these are
        // entirely fraction. The gain rounds to nothing; the loss rounds to a
        // whole cent against the account.
        assert_eq!(lamports_to_usd_cents(1, 15_000), 0);
        assert_eq!(lamports_to_usd_cents(-1, 15_000), -1);
        // One SOL at $150.00 is fifteen thousand cents, exactly.
        assert_eq!(
            lamports_to_usd_cents(i128::from(LAMPORTS_PER_SOL), 15_000),
            15_000
        );
        assert_eq!(
            lamports_to_usd_cents(-i128::from(LAMPORTS_PER_SOL), 15_000),
            -15_000
        );
        // No price means no dollar claim, rather than a claim of zero dollars
        // dressed up as a price of zero.
        assert_eq!(lamports_to_usd_cents(i128::from(LAMPORTS_PER_SOL), 0), 0);
    }

    #[test]
    fn the_three_rounding_directions_go_where_they_say() {
        assert_eq!(mul_div_floor(7, 1, 2), 3);
        assert_eq!(mul_div_round(7, 1, 2), 4);
        assert_eq!(mul_div_round(5, 1, 2), 3);
        assert_eq!(mul_div_ceil(7, 1, 2), 4);
        assert_eq!(mul_div_ceil(6, 1, 2), 3);
        // A denominator of zero is a share of nothing, not a panic.
        assert_eq!(mul_div_floor(7, 1, 0), 0);
        assert_eq!(mul_div_round(7, 1, 0), 0);
        assert_eq!(mul_div_ceil(7, 1, 0), 0);
    }

    #[test]
    fn a_product_too_big_to_hold_saturates_in_all_three_directions() {
        // Overflow checks are on in release, so a product that saturates and is
        // then added to must saturate on the way as well — otherwise the
        // rounding half is the panic the saturation was there to avoid, and
        // `mul_div_round` aborts on inputs its two siblings survive.
        let huge = u128::MAX;
        assert_eq!(mul_div_floor(huge, 2, 4), huge / 4);
        assert_eq!(mul_div_round(huge, 2, 4), huge / 4);
        assert_eq!(mul_div_ceil(huge, 2, 4), huge / 4 + 1);
        // The denominator is what the half is taken from, so the widest one is
        // the case that overflowed.
        assert_eq!(mul_div_round(huge, huge, huge), 1);
        assert_eq!(mul_div_round(huge, 1_000_000, huge), 1);
    }

    // -----------------------------------------------------------------------
    // fixture ingestion
    // -----------------------------------------------------------------------

    #[test]
    fn a_clean_fixture_verifies_every_line_and_lists_no_verdicts() {
        let fixture = round_trip_fixture();
        let audit = audit_stream("phase3-a", &fixture.text());

        assert_eq!(audit.records.len(), 8);
        assert_eq!(audit.verified, 8);
        assert_eq!(audit.rejected, 0);
        assert_eq!(audit.unverifiable, 0);
        assert_eq!(audit.first_break, None);
        assert!(audit.verdicts.is_empty(), "a clean stream lists nothing");
        assert!(audit.gate_ready().is_ok());
        assert_eq!(hex(&audit.chain_head), hex(&fixture.writer.head()));
    }

    #[test]
    fn blank_lines_are_skipped_rather_than_counted_as_records() {
        let fixture = round_trip_fixture();
        let padded = format!("\n{}\n\n", fixture.text());
        let audit = audit_stream("phase3-a", &padded);

        assert_eq!(audit.records.len(), 8);
        assert_eq!(audit.blank_lines, 3);
        assert!(audit.gate_ready().is_ok());
    }

    #[test]
    fn the_events_in_a_fixture_reach_the_launch_they_name() {
        let fixture = round_trip_fixture();
        let report = evaluate_streams(&one_stream("phase3-a", &fixture.text()), config(), "test");

        assert_eq!(report.launches.len(), 1);
        let launch = &report.launches[0];
        assert_eq!(launch.mint, "MINT1");
        assert_eq!(launch.creator.as_deref(), Some("creator-1"));
        assert_eq!(launch.flow_events, 3);
        assert_eq!(launch.entries, 1);
        assert_eq!(launch.exits, 1);
        assert_eq!(launch.labelled, Some(RugClass::Held));
        assert_eq!(report.streams[0].events_applied, 8);
        assert!(
            launch.quote_failures.is_empty(),
            "{:?}",
            launch.quote_failures
        );
    }

    #[test]
    fn a_frame_the_live_engine_filtered_is_filtered_here_too() {
        // §5.1: replay is allowed to disagree with the recording in exactly one
        // direction. A frame live rejected as `not_allowlisted` that replay
        // acted on would be the filtering bug the fidelity rule exists to catch.
        let mut fixture = FixtureBuilder::new("drops");
        fixture
            .event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL))
            .event_with(
                &buy_json("MINT1", 1_200, "wallet-a", None, LAMPORTS_PER_SOL),
                RecordOutcome::Dropped(crate::replay::DropClass::NotAllowlisted),
            )
            .event_with(
                &buy_json("MINT1", 1_400, "wallet-b", None, LAMPORTS_PER_SOL),
                RecordOutcome::Backpressure(crate::replay::Queue::FastPath),
            );

        let report = evaluate_streams(&one_stream("drops", &fixture.text()), config(), "test");
        let stream = &report.streams[0];

        assert_eq!(stream.frames_dropped_live, 1);
        assert_eq!(stream.frames_backpressure_recovered, 1);
        assert_eq!(
            stream.events_applied, 2,
            "the dropped frame was not applied"
        );
        // Only wallet-b bought, because wallet-a's frame never reached the
        // engine in the live run either.
        assert_eq!(report.launches[0].sybil.buyer_count, 1);
    }

    #[test]
    fn a_lifecycle_record_carries_no_event_and_breaks_nothing() {
        let mut fixture = FixtureBuilder::new("lifecycle");
        fixture
            .lifecycle(RecordKind::Connected)
            .event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL))
            .lifecycle(RecordKind::Pong)
            .event(&buy_json(
                "MINT1",
                1_400,
                "wallet-a",
                None,
                LAMPORTS_PER_SOL,
            ))
            .lifecycle(RecordKind::Closed);

        let report = evaluate_streams(&one_stream("lifecycle", &fixture.text()), config(), "test");
        assert_eq!(report.integrity.records, 5);
        assert_eq!(report.streams[0].frames, 2);
        assert_eq!(report.streams[0].events_applied, 2);
        assert!(report.gate_ready);
    }

    #[test]
    fn a_frame_this_build_cannot_read_is_named_and_does_not_stop_the_run() {
        let mut fixture = FixtureBuilder::new("badevent");
        fixture
            .event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL))
            .event("{\"schema\":\"sts.backtest.v9\",\"kind\":\"launch\",\"mint\":\"M\"}")
            .event("not json at all")
            .event(&buy_json(
                "MINT1",
                1_400,
                "wallet-a",
                None,
                LAMPORTS_PER_SOL,
            ));

        let report = evaluate_streams(&one_stream("badevent", &fixture.text()), config(), "test");
        let stream = &report.streams[0];

        // The records themselves are genuine: the chain verifies end to end. It
        // is the payload this build does not understand, which is a different
        // problem and is reported as one.
        assert_eq!(stream.verified, 4);
        assert_eq!(stream.first_break, None);
        assert_eq!(stream.event_errors.len(), 2);
        assert_eq!(stream.event_errors[0].seq, 1);
        assert!(stream.event_errors[0].detail.contains("schema"));
        assert_eq!(stream.event_errors[1].seq, 2);
        assert!(stream.event_errors[1].detail.contains("not JSON"));
        assert_eq!(stream.events_applied, 2);
        // A payload nobody can read is still a reason not to quote the run.
        assert!(!report.gate_ready);
        assert!(report.refusals.iter().any(|r| r.contains("cannot read")));
    }

    #[test]
    fn holders_are_sorted_at_the_boundary_not_by_the_metric() {
        // §2.2 requires the slice sorted by balance descending, address
        // ascending, before any metric sees it. Given in the wrong order here.
        let json = holders_json(
            "MINT1",
            1_000,
            &[("zeta", 10), ("alpha", 100), ("beta", 100)],
        );
        let LaunchEvent::Holders(event) = decode_event(json.as_bytes(), 0).expect("decodes") else {
            panic!("expected a holders event");
        };
        assert_eq!(
            event.holders,
            vec![
                ("alpha".to_string(), 100),
                ("beta".to_string(), 100),
                ("zeta".to_string(), 10),
            ]
        );
    }

    // -----------------------------------------------------------------------
    // determinism: R1 and R3
    // -----------------------------------------------------------------------

    #[test]
    fn two_runs_of_one_fixture_produce_byte_identical_json() {
        let fixture = round_trip_fixture();
        let sources = one_stream("phase3-a", &fixture.text());

        let first = evaluate_streams(&sources, config(), "fixtures/phase3").to_json();
        let second = evaluate_streams(&sources, config(), "fixtures/phase3").to_json();

        assert_eq!(first, second, "R1: two runs must agree byte for byte");
        // And the report carries nothing that could make them differ.
        assert!(!first.contains("generated_at"));
        assert!(!first.contains("elapsed"));
    }

    #[test]
    fn replaying_a_segmented_fixture_equals_replaying_it_whole() {
        // R3. The chain runs on across a rotation boundary, so the segments have
        // to be fed in order and the launch state has to survive the boundary.
        let fixture = round_trip_fixture();
        let whole = evaluate_streams(&one_stream("phase3-a", &fixture.text()), config(), "src");

        let segments: Vec<FixtureSource> = fixture
            .segments(3)
            .into_iter()
            .enumerate()
            .map(|(index, text)| FixtureSource {
                // One stream, several files: the genesis is the stream's, so
                // every segment carries the same stream id.
                stream_id: "phase3-a".to_string(),
                file: format!("phase3-a.{index:03}.jsonl"),
                text,
            })
            .collect();
        let split = evaluate_streams(&segments, config(), "src");

        assert_eq!(split.launches, whole.launches, "R3: launches must match");
        assert_eq!(split.performance, whole.performance);
        assert_eq!(split.risk, whole.risk);
        assert_eq!(split.rug, whole.rug);
        assert_eq!(split.adverse_selection, whole.adverse_selection);
        // Only the per-file integrity block differs, because there are three
        // files rather than one.
        assert_eq!(split.integrity.records, whole.integrity.records);
        assert_eq!(split.integrity.verified, whole.integrity.verified);
    }

    #[test]
    fn the_report_has_no_floating_point_numbers_in_it() {
        // The whole determinism argument rests on this: a JSON number with a
        // decimal point in it is a number whose last digit is a property of the
        // host's libm rather than of the fixture.
        let fixture = round_trip_fixture();
        let json =
            evaluate_streams(&one_stream("phase3-a", &fixture.text()), config(), "src").to_json();

        for (index, line) in json.lines().enumerate() {
            let Some((_, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim().trim_end_matches(',');
            if value.starts_with('"')
                || value.is_empty()
                || matches!(value, "true" | "false" | "null" | "[" | "{")
            {
                continue;
            }
            assert!(
                !value.contains('.') && !value.contains('e') && !value.contains('E'),
                "line {} carries a float: {line}",
                index + 1
            );
        }
    }

    // -----------------------------------------------------------------------
    // corrupted line recovery: R9
    // -----------------------------------------------------------------------

    /// Flips one character inside the `event_id` of the record on `line`,
    /// leaving the JSON valid and the hash wrong.
    fn edit_line(text: &str, line: usize, from: &str, to: &str) -> String {
        text.lines()
            .enumerate()
            .map(|(index, raw)| {
                if index + 1 == line {
                    raw.replacen(from, to, 1)
                } else {
                    raw.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_single_edited_byte_is_caught_and_named() {
        let fixture = round_trip_fixture();
        let edited = edit_line(&fixture.text(), 3, "\"slot\":1002", "\"slot\":1003");
        let audit = audit_stream("phase3-a", &edited);

        assert_eq!(audit.first_break, Some(3), "R9: the edit is on line three");
        let verdict = &audit.verdicts[0];
        assert_eq!(verdict.line, 3);
        assert_eq!(verdict.status, LineStatus::SelfInconsistent);
        assert!(verdict.detail.contains("integrity_hash is"));
        assert!(audit.gate_ready().is_err());
    }

    #[test]
    fn a_break_does_not_cascade_into_every_line_after_it() {
        // The property that makes the audit useful rather than merely correct:
        // one edited byte on line three must not produce six chain errors.
        let fixture = round_trip_fixture();
        let edited = edit_line(&fixture.text(), 3, "\"slot\":1002", "\"slot\":1003");
        let audit = audit_stream("phase3-a", &edited);

        assert_eq!(audit.rejected, 1, "exactly one line is broken");
        assert_eq!(audit.unverifiable, 5, "the rest are downstream, not broken");
        assert_eq!(audit.verified, 2, "the prefix before the edit still stands");
        assert!(audit
            .verdicts
            .iter()
            .skip(1)
            .all(|v| v.status == LineStatus::UnverifiableAfterBreak));
    }

    #[test]
    fn a_removed_record_breaks_the_sequence_and_the_chain() {
        let fixture = round_trip_fixture();
        let text = fixture.text();
        let without: Vec<&str> = text
            .lines()
            .enumerate()
            .filter(|(index, _)| *index != 3)
            .map(|(_, line)| line)
            .collect();
        let audit = audit_stream("phase3-a", &without.join("\n"));

        assert_eq!(audit.first_break, Some(4));
        assert_eq!(audit.verdicts[0].status, LineStatus::SeqGap);
        assert!(audit.verdicts[0].detail.contains("expected seq 3"));
    }

    #[test]
    fn a_resealed_but_reordered_stream_fails_on_the_order_key() {
        // The shape a plausible forgery has: the chain is rebuilt so every link
        // verifies, and the §6 order key is the thing that still does not.
        let mut writer = ChainWriter::new("reordered");
        let mut records = Vec::new();
        for (index, slot) in [1_000u64, 1_002, 1_001].into_iter().enumerate() {
            records.push(writer.seal(RecordDraft {
                event_id: format!("evt-{index}"),
                slot,
                observed_at_ms: 1_700_000_000_000 + index as i64,
                provider: "helius".to_string(),
                endpoint_index: 0,
                connection: 1,
                kind: RecordKind::Pong,
                frame: None,
                outcome: RecordOutcome::Accepted,
                dispatch_latency_us: None,
            }));
        }
        let audit = audit_stream("reordered", &write_stream(&records));

        assert_eq!(audit.first_break, Some(3));
        assert_eq!(audit.verdicts[0].status, LineStatus::OutOfOrder);
        assert!(audit.gate_ready().is_err());
    }

    #[test]
    fn an_unreadable_line_is_stepped_over_and_the_rest_still_reads() {
        let fixture = round_trip_fixture();
        let mut lines: Vec<String> = fixture.text().lines().map(str::to_string).collect();
        lines[2] = "{\"schema\":\"sts.replay.v1\"".to_string();
        let audit = audit_stream("phase3-a", &lines.join("\n"));

        assert_eq!(audit.verdicts[0].line, 3);
        assert_eq!(audit.verdicts[0].status, LineStatus::Unparseable);
        // Seven records still parsed: recovery is the point.
        assert_eq!(audit.records.len(), 7);
        assert_eq!(audit.verified, 2);
    }

    #[test]
    fn gate_mode_reads_the_verified_prefix_and_nothing_after_it() {
        let fixture = round_trip_fixture();
        let edited = edit_line(&fixture.text(), 4, "\"slot\":1003", "\"slot\":1009");

        let open = evaluate_streams(
            &one_stream("phase3-a", &edited),
            BacktestConfig {
                gate: false,
                ..config()
            },
            "src",
        );
        let gated = evaluate_streams(
            &one_stream("phase3-a", &edited),
            BacktestConfig {
                gate: true,
                ..config()
            },
            "src",
        );

        // Line four is our entry. Outside the gate it is read and priced;
        // inside it, everything from the break on is unreadable.
        assert_eq!(open.streams[0].events_applied, 8);
        assert_eq!(gated.streams[0].events_applied, 3);
        assert_eq!(gated.launches[0].entries, 0);
        assert!(!gated.gate_ready);
        assert!(gated.refusals[0].contains("line 4"));
        assert!(gated.refusals[0].contains("self_inconsistent"));
    }

    // -----------------------------------------------------------------------
    // deterministic PnL
    // -----------------------------------------------------------------------

    /// The curve everything below prices against: forty SOL of real reserves,
    /// inside the 33–85 SOL band doctrine puts the strategy in.
    fn band_curve() -> CurveState {
        CurveState::at_real_sol(40 * LAMPORTS_PER_SOL)
    }

    #[test]
    fn a_round_trip_with_no_flow_between_the_legs_costs_exactly_two_phi() {
        // R13, arrived at through the harness rather than through the curve API:
        // the realised loss on an immediate round trip has to be the two fees
        // and nothing else.
        let mut fixture = FixtureBuilder::new("roundtrip");
        fixture
            .event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL))
            .event(&entry_json("MINT1", 2_000, LAMPORTS_PER_SOL))
            .event(&exit_json("MINT1", 3_000, None));

        let report = evaluate_streams(&one_stream("roundtrip", &fixture.text()), config(), "src");

        // What the curve API says, computed independently.
        let state = band_curve();
        let buy = state
            .quote_buy(LAMPORTS_PER_SOL, DEFAULT_FEE_BPS)
            .expect("buy");
        let sell = state
            .after_buy(&buy)
            .quote_sell(buy.tokens, DEFAULT_FEE_BPS)
            .expect("sell");
        let expected =
            i64::try_from(sell.net_lamports).unwrap() - i64::try_from(LAMPORTS_PER_SOL).unwrap();

        assert_eq!(report.performance.realized_pnl_lamports, expected);
        assert_eq!(report.performance.trades, 1);
        assert_eq!(report.performance.losers, 1);

        let cost_bps = crate::replay::round_trip_bps(&state, LAMPORTS_PER_SOL, DEFAULT_FEE_BPS)
            .expect("round trip");
        assert!(
            (199..=201).contains(&cost_bps),
            "R13: an immediate round trip costs 2 phi, got {cost_bps} bps"
        );
        assert_eq!(
            report.launches[0].trades[0].return_bps,
            -i32::from(cost_bps)
        );
    }

    #[test]
    fn dollars_come_off_the_lamports_at_the_configured_price() {
        let mut fixture = FixtureBuilder::new("dollars");
        fixture
            .event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL))
            .event(&entry_json("MINT1", 2_000, LAMPORTS_PER_SOL))
            .event(&exit_json("MINT1", 3_000, None));

        let report = evaluate_streams(&one_stream("dollars", &fixture.text()), config(), "src");
        let lamports = report.performance.realized_pnl_lamports;

        assert_eq!(
            report.performance.realized_pnl_usd_cents,
            lamports_to_usd_cents(i128::from(lamports), 15_000)
        );
        // The loss is about two percent of a SOL, so a couple of hundred
        // thousandths of $150. It is negative and it is not zero.
        assert!(report.performance.realized_pnl_usd_cents < 0);
    }

    #[test]
    fn lots_are_matched_first_in_first_out() {
        let mut fixture = FixtureBuilder::new("fifo");
        fixture.event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL));

        // What the first entry will fill at, so the exit can name exactly it.
        let first_tokens = band_curve()
            .quote_buy(LAMPORTS_PER_SOL, DEFAULT_FEE_BPS)
            .expect("buy")
            .tokens;

        fixture
            .event(&entry_json("MINT1", 2_000, LAMPORTS_PER_SOL))
            .event(&entry_json("MINT1", 4_000, LAMPORTS_PER_SOL))
            .event(&exit_json("MINT1", 9_000, Some(first_tokens)));

        let report = evaluate_streams(&one_stream("fifo", &fixture.text()), config(), "src");
        let launch = &report.launches[0];

        assert_eq!(launch.trades.len(), 1);
        let trade = &launch.trades[0];
        assert_eq!(trade.opened_at_ms, 2_000, "the older lot goes first");
        assert_eq!(trade.tokens, first_tokens);
        assert_eq!(
            trade.cost_lamports, LAMPORTS_PER_SOL,
            "the whole lot's cost"
        );
        assert_eq!(trade.hold_ms, 7_000);

        // The second lot is still open, so it is stranded rather than realised.
        let stranded = launch.stranded.as_ref().expect("the second lot is open");
        assert_eq!(stranded.opened_at_ms, 4_000);
        assert_eq!(stranded.cost_lamports, LAMPORTS_PER_SOL);
        assert_eq!(report.risk.positions_stranded, 1);
    }

    #[test]
    fn a_partial_exit_splits_the_cost_up_and_the_proceeds_down() {
        let mut fixture = FixtureBuilder::new("partial");
        fixture.event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL));

        let tokens = band_curve()
            .quote_buy(LAMPORTS_PER_SOL, DEFAULT_FEE_BPS)
            .expect("buy")
            .tokens;
        let half = tokens / 2;

        fixture
            .event(&entry_json("MINT1", 2_000, LAMPORTS_PER_SOL))
            .event(&exit_json("MINT1", 3_000, Some(half)))
            .event(&exit_json("MINT1", 4_000, None));

        let report = evaluate_streams(&one_stream("partial", &fixture.text()), config(), "src");
        let trades = &report.launches[0].trades;

        assert_eq!(trades.len(), 2);
        // Not one lamport of cost is invented or lost across the two parcels.
        let total_cost: u64 = trades.iter().map(|t| t.cost_lamports).sum();
        assert_eq!(total_cost, LAMPORTS_PER_SOL);
        // The first parcel's cost rounds up, so it is at least its half.
        assert!(trades[0].cost_lamports >= LAMPORTS_PER_SOL / 2);
        let total_tokens: u64 = trades.iter().map(|t| t.tokens).sum();
        assert_eq!(total_tokens, tokens);
        assert!(report.launches[0].stranded.is_none());
    }

    #[test]
    fn proceeds_split_across_lots_sum_to_exactly_the_fill() {
        // Two lots closed by one sell. The parts must add up to what the curve
        // actually paid, with the residue landing on the last parcel rather
        // than evaporating.
        let mut fixture = FixtureBuilder::new("proceeds");
        fixture
            .event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL))
            .event(&entry_json("MINT1", 2_000, 333_333_333))
            .event(&entry_json("MINT1", 3_000, 777_777_777))
            .event(&exit_json("MINT1", 9_000, None));

        let report = evaluate_streams(&one_stream("proceeds", &fixture.text()), config(), "src");
        let launch = &report.launches[0];

        assert_eq!(launch.trades.len(), 2);
        let proceeds: u64 = launch.trades.iter().map(|t| t.proceeds_lamports).sum();
        assert_eq!(proceeds, launch.exit_net_lamports);
        let cost: u64 = launch.trades.iter().map(|t| t.cost_lamports).sum();
        assert_eq!(cost, 333_333_333 + 777_777_777);
        assert_eq!(
            launch.realized_pnl_lamports,
            i64::try_from(proceeds).unwrap() - i64::try_from(cost).unwrap()
        );
    }

    #[test]
    fn a_position_with_no_executable_exit_is_marked_at_nothing_and_flagged() {
        // §17. The curve cannot pay for the exit, so the position is worth what
        // it can be sold for, which is nothing. Marking it at the model price
        // would be reporting money that could not have been got out.
        let mut fixture = FixtureBuilder::new("noexit");
        fixture
            .event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL))
            .event(&entry_json("MINT1", 2_000, LAMPORTS_PER_SOL))
            .event(&pull_json("MINT1", 3_000, 41 * LAMPORTS_PER_SOL));

        let report = evaluate_streams(&one_stream("noexit", &fixture.text()), config(), "src");
        let stranded = report.launches[0]
            .stranded
            .as_ref()
            .expect("the position is still open");

        assert!(stranded.no_executable_exit, "{stranded:?}");
        assert_eq!(stranded.marked_lamports, 0);
        assert_eq!(stranded.marked_pnl_lamports, -(LAMPORTS_PER_SOL as i64));
        assert_eq!(report.risk.no_executable_exits, 1);
        // And it is never folded into realised PnL.
        assert_eq!(report.performance.realized_pnl_lamports, 0);
        assert_eq!(
            report.performance.marked_pnl_lamports,
            -(LAMPORTS_PER_SOL as i64)
        );
    }

    #[test]
    fn an_exit_with_no_position_is_a_named_failure_rather_than_a_panic() {
        let mut fixture = FixtureBuilder::new("noposition");
        fixture
            .event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL))
            .event(&exit_json("MINT1", 2_000, None));

        let report = evaluate_streams(&one_stream("noposition", &fixture.text()), config(), "src");
        let failures = &report.launches[0].quote_failures;

        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].context, "exit");
        assert!(failures[0].reason.contains("no position"));
    }

    #[test]
    fn a_graduated_curve_is_never_quoted() {
        // R18. Once the curve completes every quote is a quote from a dead pool,
        // and that is a hard branch rather than a continuous transition.
        // The smallest buy that pushes real SOL past graduation, derived from
        // the curve rather than guessed: much larger and it would be refused for
        // exceeding the real token reserve instead, which is a different
        // refusal and would not test this one.
        let state = CurveState::at_real_sol(84 * LAMPORTS_PER_SOL);
        let needed_net = PUMP_GRADUATION_LAMPORTS - state.real_sol_reserves + 1;
        let gross = needed_net * u64::from(BPS_DENOMINATOR)
            / u64::from(BPS_DENOMINATOR - u32::from(DEFAULT_FEE_BPS))
            + 1;

        let mut fixture = FixtureBuilder::new("graduated");
        fixture
            .event(&launch_json("MINT1", 1_000, 84 * LAMPORTS_PER_SOL))
            .event(&buy_json("MINT1", 1_500, "whale", None, gross))
            .event(&entry_json("MINT1", 2_000, LAMPORTS_PER_SOL));

        let report = evaluate_streams(&one_stream("graduated", &fixture.text()), config(), "src");
        let launch = &report.launches[0];

        assert!(launch.graduated);
        assert_eq!(launch.entries, 0, "the entry was refused, not filled");
        assert_eq!(launch.classified, RugClass::Graduated);
        assert!(launch
            .quote_failures
            .iter()
            .any(|f| f.context == "entry" && f.reason.contains("graduated")));
    }

    // -----------------------------------------------------------------------
    // risk metrics
    // -----------------------------------------------------------------------

    fn trade(closed_at_ms: i64, pnl: i64, hold_ms: i64) -> ClosedTrade {
        ClosedTrade {
            mint: "MINT1".to_string(),
            opened_at_ms: closed_at_ms - hold_ms,
            closed_at_ms,
            hold_ms,
            tokens: 1_000,
            cost_lamports: LAMPORTS_PER_SOL,
            proceeds_lamports: (i128::from(LAMPORTS_PER_SOL) + i128::from(pnl)).max(0) as u64,
            pnl_lamports: pnl,
            pnl_usd_cents: 0,
            return_bps: (i128::from(pnl) * 10_000 / i128::from(LAMPORTS_PER_SOL)) as i32,
        }
    }

    #[test]
    fn the_drawdown_is_measured_from_the_high_water_mark() {
        let sol = LAMPORTS_PER_SOL as i64;
        let trades = vec![
            trade(1_000, 2 * sol, 100),  // equity 12, high water 12
            trade(2_000, -3 * sol, 100), // equity  9, fall 3 of 12 = 2500 bps
            trade(3_000, sol, 100),      // equity 10
            trade(4_000, -4 * sol, 100), // equity  6, fall 6 of 12 = 5000 bps
        ];
        let (high_water, fall, bps, at_ms, underwater, streak) =
            drawdown(&trades, 10 * LAMPORTS_PER_SOL);

        assert_eq!(high_water, 12 * sol);
        assert_eq!(fall, 6 * sol);
        assert_eq!(bps, 5_000);
        assert_eq!(at_ms, 4_000);
        assert_eq!(underwater, 3_000, "from the peak at t=1000 to the end");
        assert_eq!(streak, 1);
    }

    #[test]
    fn the_losing_streak_counts_consecutive_losers_only() {
        let sol = LAMPORTS_PER_SOL as i64;
        let trades = vec![
            trade(1_000, -sol, 10),
            trade(2_000, -sol, 10),
            trade(3_000, -sol, 10),
            trade(4_000, sol, 10),
            trade(5_000, -sol, 10),
        ];
        let (_, _, _, _, _, streak) = drawdown(&trades, 10 * LAMPORTS_PER_SOL);
        assert_eq!(streak, 3);
    }

    #[test]
    fn an_account_that_never_falls_has_no_drawdown() {
        let sol = LAMPORTS_PER_SOL as i64;
        let trades = vec![trade(1_000, sol, 10), trade(2_000, sol, 10)];
        let (high_water, fall, bps, _, underwater, streak) =
            drawdown(&trades, 10 * LAMPORTS_PER_SOL);

        assert_eq!(high_water, 12 * sol);
        assert_eq!(fall, 0);
        assert_eq!(bps, 0);
        assert_eq!(underwater, 0);
        assert_eq!(streak, 0);
    }

    #[test]
    fn the_sharpe_ratio_is_the_mean_over_the_sample_deviation() {
        // Returns of 100, 200 and 300 bps: a mean of 200 and a sample deviation
        // of 100, so a Sharpe of exactly 2.
        assert_eq!(sharpe_micros(&[100, 200, 300]), Some(2 * MICROS as i64));
        // Symmetric losses give the same magnitude the other way.
        assert_eq!(sharpe_micros(&[-100, -200, -300]), Some(-2 * MICROS as i64));
    }

    #[test]
    fn a_sharpe_ratio_needs_two_trades_and_some_variation() {
        assert_eq!(sharpe_micros(&[]), None, "nothing to measure");
        assert_eq!(sharpe_micros(&[500]), None, "one trade has no deviation");
        assert_eq!(
            sharpe_micros(&[500, 500, 500]),
            None,
            "a zero denominator is not a perfect strategy"
        );
    }

    #[test]
    fn the_holding_period_statistics_are_floored_and_deterministic() {
        let sol = LAMPORTS_PER_SOL as i64;
        let trades = vec![
            trade(1_000, sol, 1_000),
            trade(2_000, -sol, 2_000),
            trade(3_000, sol, 3_001),
            trade(4_000, -sol, 4_000),
        ];
        let summary = summarise_performance(&trades, &[], &[], &BacktestConfig::default());

        assert_eq!(summary.total_hold_ms, 10_001);
        assert_eq!(summary.average_hold_ms, 2_500, "10001 / 4, floored");
        assert_eq!(summary.median_hold_ms, 2_500, "(2000 + 3001) / 2, floored");
        assert_eq!(summary.winners, 2);
        assert_eq!(summary.losers, 2);
        assert_eq!(summary.win_rate_bps, 5_000);
        assert_eq!(summary.profit_factor_micros, Some(MICROS));
    }

    #[test]
    fn a_run_that_lost_nothing_has_no_profit_factor_rather_than_an_infinite_one() {
        let sol = LAMPORTS_PER_SOL as i64;
        let trades = vec![trade(1_000, sol, 10), trade(2_000, sol, 10)];
        let summary = summarise_performance(&trades, &[], &[], &BacktestConfig::default());
        assert_eq!(summary.profit_factor_micros, None);
        assert_eq!(summary.gross_loss_lamports, 0);
    }

    // -----------------------------------------------------------------------
    // §15.2 — the extraction threshold
    // -----------------------------------------------------------------------

    /// What the integer floors in three swaps are worth, in lamports.
    ///
    /// Four divisions truncate on the way through `quote_buy`, `quote_buy` and
    /// `quote_sell`, so two ways of computing the same extraction disagree by
    /// at most this, and a "profit" this small is the arithmetic rather than the
    /// trade. Measured at one lamport across the sweeps below; the bound is set
    /// well above that so it is testing a property rather than a measurement.
    const INTEGER_RESIDUE_LAMPORTS: i64 = 8;

    /// The three curve positions §15.2's table is written at, in virtual SOL.
    const TABLE_POSITIONS: [u64; 3] = [
        30 * LAMPORTS_PER_SOL,
        75 * LAMPORTS_PER_SOL,
        115 * LAMPORTS_PER_SOL,
    ];

    #[test]
    fn the_breakeven_victim_reproduces_the_specification_table() {
        // §15.2: 0.3061 SOL at launch, 0.7652 at y = 75, 1.1733 at graduation.
        let expected = [306_091_216u64, 765_228_038, 1_173_349_659];
        for (position, want) in TABLE_POSITIONS.into_iter().zip(expected) {
            let got = sandwich_breakeven_victim_lamports(position, DEFAULT_FEE_BPS);
            assert_eq!(got, want, "at y = {position}");
        }
    }

    #[test]
    fn the_breakeven_size_is_the_smallest_one_the_threshold_admits() {
        // The two forms of the same condition have to agree exactly, because
        // one of them decides and the other one is printed.
        for position in TABLE_POSITIONS {
            let breakeven = sandwich_breakeven_victim_lamports(position, DEFAULT_FEE_BPS);
            assert!(
                sandwich_viable(breakeven, position, DEFAULT_FEE_BPS),
                "b* itself must be admitted at y = {position}"
            );
            assert!(
                !sandwich_viable(breakeven - 1, position, DEFAULT_FEE_BPS),
                "one lamport under b* must not be, at y = {position}"
            );
        }
    }

    #[test]
    fn the_threshold_is_the_same_comparison_written_two_ways() {
        // beta > phi / (1 - phi) and b > phi*y / (1 - phi)^2 are the same claim.
        // The rounded millionths may disagree at the boundary and the exact
        // comparison may not, which is why only one of them decides.
        for position in TABLE_POSITIONS {
            let breakeven = sandwich_breakeven_victim_lamports(position, DEFAULT_FEE_BPS);
            for victim in [breakeven / 4, breakeven, breakeven * 4, breakeven * 100] {
                let by_size = victim >= breakeven;
                let by_beta = sandwich_viable(victim, position, DEFAULT_FEE_BPS);
                assert_eq!(by_beta, by_size, "y = {position}, b = {victim}");
            }
        }
    }

    #[test]
    fn no_front_run_is_profitable_below_the_threshold() {
        // R15, at each of the three curve positions, with the attacker's landing
        // cost set to zero so the only thing being tested is the curve.
        for position in TABLE_POSITIONS {
            let real_sol = position - CurveState::LAUNCH.virtual_sol_reserves;
            let state = CurveState::at_real_sol(real_sol);
            let victim = sandwich_breakeven_victim_lamports(position, DEFAULT_FEE_BPS) - 1;
            assert!(!sandwich_viable(victim, position, DEFAULT_FEE_BPS));

            let mut attacker = MIN_VIABLE_ATTACKER_LAMPORTS;
            while attacker <= 100 * LAMPORTS_PER_SOL {
                if let Ok(sandwich) =
                    simulate_sandwich(&state, attacker, victim, DEFAULT_FEE_BPS, 0)
                {
                    // Not `<= 0`. §15.2's second boundary condition: the three
                    // swaps floor at four separate divisions, so a search below
                    // the threshold returns one-lamport "profits" that are the
                    // rounding rather than extraction. The bound is the same
                    // few lamports the closed form and the simulation differ by;
                    // anything above it would be a real edge.
                    assert!(
                        sandwich.attacker_profit_lamports <= INTEGER_RESIDUE_LAMPORTS,
                        "y = {position}: a front-run of {attacker} profited {} against a \
                         victim below the threshold",
                        sandwich.attacker_profit_lamports
                    );
                }
                // A ladder plus an offset, so the sweep does not only ever land
                // on powers of two.
                attacker = attacker * 2 + 7;
            }
        }
    }

    #[test]
    fn extraction_never_exceeds_what_the_victim_put_in() {
        // R14: E(alpha, beta) < beta*y, which is the victim's fee-adjusted spend.
        // No sandwich takes more than the victim brought, however much capital
        // the attacker has.
        for position in TABLE_POSITIONS {
            for victim_sol in [1u64, 2, 5, 20] {
                let victim_net = victim_sol * LAMPORTS_PER_SOL * 99 / 100;
                for attacker_sol in [1u64, 5, 50, 500, 5_000] {
                    let attacker_net = attacker_sol * LAMPORTS_PER_SOL * 99 / 100;
                    let extraction = sandwich_extraction_closed(position, attacker_net, victim_net)
                        .expect("the closed form fits");
                    assert!(
                        extraction < victim_net,
                        "y = {position}, a = {attacker_net}, b = {victim_net}: \
                         extracted {extraction} of {victim_net}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_closed_form_and_the_three_swaps_agree_to_a_few_lamports() {
        // R16. The closed form is exact in rationals and the simulation floors
        // at four separate divisions, so they agree to within a residue rather
        // than exactly — and the residue is always in the attacker's disfavour,
        // which is the direction that cannot flatter a backtest.
        let net_of_fee = |gross: u64| gross - gross * u64::from(DEFAULT_FEE_BPS) / 10_000;

        for position in TABLE_POSITIONS {
            let real_sol = position - CurveState::LAUNCH.virtual_sol_reserves;
            let state = CurveState::at_real_sol(real_sol);
            for victim_sol in [1u64, 2] {
                for attacker_tenths in [1u64, 5, 10] {
                    let victim = victim_sol * LAMPORTS_PER_SOL;
                    let attacker = attacker_tenths * LAMPORTS_PER_SOL / 10;
                    let Ok(sandwich) =
                        simulate_sandwich(&state, attacker, victim, DEFAULT_FEE_BPS, 0)
                    else {
                        continue;
                    };
                    let closed = sandwich_extraction_closed(
                        state.virtual_sol_reserves,
                        net_of_fee(attacker),
                        net_of_fee(victim),
                    )
                    .expect("the closed form fits");

                    let residue = i128::from(closed) - i128::from(sandwich.extraction_lamports);
                    assert!(
                        residue >= 0,
                        "y = {position}: the simulation beat the closed form by {residue}"
                    );
                    assert!(
                        residue <= 8,
                        "y = {position}, a = {attacker}, b = {victim}: residue {residue}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_front_run_search_gives_the_same_answer_every_time() {
        let state = CurveState::at_real_sol(45 * LAMPORTS_PER_SOL);
        let victim = LAMPORTS_PER_SOL;
        let first = best_front_run_deterministic(
            &state,
            victim,
            DEFAULT_FEE_BPS,
            5_000_000,
            LAMPORTS_PER_SOL,
        );
        let second = best_front_run_deterministic(
            &state,
            victim,
            DEFAULT_FEE_BPS,
            5_000_000,
            LAMPORTS_PER_SOL,
        );

        assert_eq!(first, second);
        let (size, sandwich) = first.expect("a 1 SOL buy at y = 75 is worth sandwiching");
        assert!(sandwich.attacker_profit_lamports > 0);
        // §15.3's table: inside the band, a public 1 SOL buy against an attacker
        // capped at 1 SOL costs 190 to 260 basis points.
        assert!(
            (190..=260).contains(&sandwich.victim_damage_bps),
            "damage was {} bps at a front-run of {size}",
            sandwich.victim_damage_bps
        );
    }

    #[test]
    fn a_buy_below_the_threshold_is_priced_at_nothing_and_says_why() {
        let state = CurveState::at_real_sol(45 * LAMPORTS_PER_SOL);
        let breakeven =
            sandwich_breakeven_victim_lamports(state.virtual_sol_reserves, DEFAULT_FEE_BPS);
        let verdict = assess_sandwich(
            &state,
            breakeven - 1,
            DEFAULT_FEE_BPS,
            5_000_000,
            LAMPORTS_PER_SOL,
        );

        assert!(!verdict.above_threshold);
        assert_eq!(verdict.best_attacker_lamports, 0);
        assert_eq!(verdict.damage_bps, 0);
        assert_eq!(verdict.extraction_lamports, 0);
        assert_eq!(verdict.breakeven_victim_lamports, breakeven);
    }

    #[test]
    fn adverse_selection_is_priced_on_every_entry_and_says_it_is_a_floor() {
        let mut fixture = FixtureBuilder::new("adverse");
        fixture
            .event(&launch_json("MINT1", 1_000, 45 * LAMPORTS_PER_SOL))
            .event(&entry_json("MINT1", 2_000, 2 * LAMPORTS_PER_SOL))
            .event(&exit_json("MINT1", 3_000, None));

        let report = evaluate_streams(&one_stream("adverse", &fixture.text()), config(), "src");
        let adverse = &report.adverse_selection;

        assert_eq!(adverse.entries_priced, 1);
        assert_eq!(adverse.entries_above_threshold, 1);
        assert_eq!(adverse.entries_with_viable_attacker, 1);
        assert!(adverse.worst_damage_bps > 0);
        assert!(adverse.optimistic, "the bound points the dangerous way");
        assert!(
            adverse.worst_closed_form_residue_lamports.abs() <= 8,
            "R16 inside the harness: residue {}",
            adverse.worst_closed_form_residue_lamports
        );
    }

    #[test]
    fn an_entry_that_was_refused_is_never_priced_for_adverse_selection() {
        // Pricing the front-run of a fill that did not happen would put a cost
        // in the report for a trade that was never made.
        let mut fixture = FixtureBuilder::new("refused");
        fixture
            .event(&launch_json("MINT1", 1_000, 90 * LAMPORTS_PER_SOL))
            .event(&entry_json("MINT1", 2_000, LAMPORTS_PER_SOL));

        let report = evaluate_streams(&one_stream("refused", &fixture.text()), config(), "src");
        assert_eq!(report.adverse_selection.entries_priced, 0);
        assert_eq!(report.launches[0].adverse_selection.len(), 0);
        assert_eq!(report.launches[0].quote_failures.len(), 1);
    }

    // -----------------------------------------------------------------------
    // concentration and Sybil clustering
    // -----------------------------------------------------------------------

    #[test]
    fn the_concentration_index_answers_the_degenerate_cases_the_way_the_table_says() {
        // RISK_AND_SYBIL_SPEC.md §2.4, row for row.
        assert_eq!(hhi_bps(&[]), None, "empty population");
        assert_eq!(hhi_bps(&[0, 0, 0]), None, "no supply to be concentrated");
        assert_eq!(hhi_bps(&[100]), Some(10_000), "one holder is the maximum");
        assert_eq!(
            hhi_bps(&[25, 25, 25, 25]),
            Some(2_500),
            "four equal holders"
        );
        let hundred = vec![1u64; 100];
        assert_eq!(hhi_bps(&hundred), Some(100), "a hundred equal holders");
        // One holder with the rest as dust stays at the top: dust does not
        // dilute control.
        let mut dusty = vec![1_000_000_000u64];
        dusty.extend(vec![1u64; 500]);
        assert!(hhi_bps(&dusty).expect("measurable") > 9_990);
    }

    #[test]
    fn the_index_rounds_to_nearest_rather_than_towards_looking_safe() {
        // Three holders at a third each: sum of squared shares is 1/3, so the
        // index is 3333.33 and must round up to 3333 rather than truncating
        // somewhere lower through the parts-per-trillion scaling.
        assert_eq!(hhi_bps(&[1, 1, 1]), Some(3_333));
        // Six holders: 1666.67, which rounds to 1667 and not to 1666.
        assert_eq!(hhi_bps(&[1, 1, 1, 1, 1, 1]), Some(1_667));
    }

    #[test]
    fn the_top_k_share_reads_off_a_sorted_slice() {
        let balances = [500u64, 300, 100, 100];
        assert_eq!(top_k_bps(&balances, 1), 5_000);
        assert_eq!(top_k_bps(&balances, 2), 8_000);
        assert_eq!(top_k_bps(&balances, 10), 10_000, "more than there are");
        assert_eq!(top_k_bps(&[], 1), 0);
        assert_eq!(top_k_bps(&[0, 0], 1), 0);
    }

    #[test]
    fn the_effective_holder_count_is_the_reciprocal_of_the_index() {
        // §2.1: 10_000 / HHI_bps is the number of equal-sized holders that
        // would produce the same index, and that reading is exact.
        assert_eq!(effective_holders_micros(10_000), MICROS);
        assert_eq!(effective_holders_micros(2_500), 4 * MICROS);
        assert_eq!(effective_holders_micros(100), 100 * MICROS);
        assert_eq!(effective_holders_micros(0), 0, "no index, no reading");
    }

    #[test]
    fn wallets_that_bought_in_the_same_instant_score_exactly_one() {
        // §7.1: "Buy times identical → sync = 1 exactly, no division by zero."
        let together = [1_700_000_000_000i64; 8];
        assert_eq!(
            sync_micros(&together, DEFAULT_TAU_SYNC_MS),
            Some((MICROS, false))
        );
    }

    #[test]
    fn synchrony_decays_at_the_kernel_bandwidth() {
        // Two wallets one bandwidth apart score exp(-1).
        let pair = [0i64, DEFAULT_TAU_SYNC_MS as i64];
        assert_eq!(
            sync_micros(&pair, DEFAULT_TAU_SYNC_MS),
            Some((exp_neg_micros(MICROS), false))
        );
        // An hour apart is nothing at a five-second bandwidth.
        let apart = [0i64, 3_600_000];
        assert_eq!(sync_micros(&apart, DEFAULT_TAU_SYNC_MS), Some((0, false)));
    }

    #[test]
    fn one_wallet_is_not_synchronised_with_itself() {
        assert_eq!(sync_micros(&[], DEFAULT_TAU_SYNC_MS), None);
        assert_eq!(sync_micros(&[1_000], DEFAULT_TAU_SYNC_MS), None);
        // A bandwidth of zero is not a kernel.
        assert_eq!(sync_micros(&[1_000, 2_000], 0), None);
    }

    #[test]
    fn the_synchrony_budget_truncates_and_says_so() {
        // §3.4: budget exhaustion sets a flag and does not extend the budget.
        let many: Vec<i64> = (0..(SYNC_WALLET_BUDGET as i64 + 44)).collect();
        let (_, truncated) = sync_micros(&many, DEFAULT_TAU_SYNC_MS).expect("measurable");
        assert!(truncated);

        let few: Vec<i64> = (0..(SYNC_WALLET_BUDGET as i64)).collect();
        let (_, truncated) = sync_micros(&few, DEFAULT_TAU_SYNC_MS).expect("measurable");
        assert!(!truncated, "exactly at the budget is not over it");
    }

    #[test]
    fn the_geometric_mean_needs_both_halves_to_be_true() {
        // §3.5: fifty wallets in one slot behind fifty funders is a bot service,
        // and one funder over four hours is somebody managing positions. Only
        // both together is what the metric is for.
        assert_eq!(temporal_influence_micros(MICROS, MICROS), MICROS);
        assert_eq!(temporal_influence_micros(0, MICROS), 0, "synchrony alone");
        assert_eq!(temporal_influence_micros(MICROS, 0), 0, "funding alone");
        // A quarter and one is a half, not five eighths.
        assert_eq!(temporal_influence_micros(250_000, MICROS), 500_000);
    }

    fn buyer(wallet: &str, funder: Option<&str>, at_ms: i64, volume: u64) -> BuyerObservation {
        BuyerObservation {
            wallet: wallet.to_string(),
            funder: funder.map(str::to_string),
            first_buy_ms: at_ms,
            buy_volume_lamports: volume,
            buys: 1,
        }
    }

    #[test]
    fn wallets_are_grouped_by_the_funder_the_recording_names() {
        let buyers = vec![
            buyer("w1", Some("root-a"), 1_000, 3 * LAMPORTS_PER_SOL),
            buyer("w2", Some("root-a"), 1_010, 2 * LAMPORTS_PER_SOL),
            buyer("w3", Some("root-b"), 5_000, LAMPORTS_PER_SOL),
            buyer("w4", Some("root-b"), 5_020, LAMPORTS_PER_SOL),
            buyer("w5", None, 9_000, 4 * LAMPORTS_PER_SOL),
        ];
        let clusters = cluster_by_funder(&buyers, DEFAULT_TAU_SYNC_MS, 2);

        assert_eq!(clusters.len(), 2, "the unfunded wallet forms no cluster");
        assert_eq!(clusters[0].funder, "root-a", "loudest first");
        assert_eq!(clusters[0].wallets, vec!["w1", "w2"]);
        assert_eq!(clusters[0].buy_volume_lamports, 5 * LAMPORTS_PER_SOL);
        assert_eq!(clusters[0].flow_share_bps, 4_545, "5 SOL of 11");
        assert_eq!(clusters[0].first_buy_span_ms, 10);
        assert!(clusters[0].sync_micros > 990_000, "ten milliseconds apart");
        assert_eq!(clusters[1].funder, "root-b");
    }

    #[test]
    fn a_wallet_whose_funder_is_unknown_is_not_recruited_by_timing_alone() {
        // §3.3: an unknown parent is neither self-funded nor clean. Inventing a
        // cluster out of wallets that merely bought together is how a bot
        // service with many customers gets reported as one hand.
        let buyers = vec![
            buyer("w1", None, 1_000, LAMPORTS_PER_SOL),
            buyer("w2", None, 1_000, LAMPORTS_PER_SOL),
            buyer("w3", None, 1_000, LAMPORTS_PER_SOL),
        ];
        assert!(cluster_by_funder(&buyers, DEFAULT_TAU_SYNC_MS, 2).is_empty());
    }

    #[test]
    fn a_lone_wallet_behind_a_funder_is_a_coincidence_not_a_cluster() {
        let buyers = vec![
            buyer("w1", Some("root-a"), 1_000, LAMPORTS_PER_SOL),
            buyer("w2", Some("root-b"), 1_000, LAMPORTS_PER_SOL),
        ];
        assert!(cluster_by_funder(&buyers, DEFAULT_TAU_SYNC_MS, 2).is_empty());
        assert_eq!(cluster_by_funder(&buyers, DEFAULT_TAU_SYNC_MS, 1).len(), 2);
    }

    #[test]
    fn buyer_diversity_counts_entities_and_not_keypairs() {
        // §2.3: wallets linked by funding count once. Four wallets behind one
        // root are one entity, and measuring the four would report a diversity
        // somebody got for free.
        assert_eq!(buyer_diversity_bps(&[1, 1, 1, 1]), Some(7_500));
        assert_eq!(
            buyer_diversity_bps(&[4]),
            Some(0),
            "one entity, no diversity"
        );
        assert_eq!(buyer_diversity_bps(&[]), None);
    }

    #[test]
    fn one_funder_behind_most_of_the_flow_shows_up_as_funding_concentration() {
        let mut fixture = FixtureBuilder::new("sybil");
        fixture
            .event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL))
            .event(&buy_json(
                "MINT1",
                2_000,
                "w1",
                Some("root"),
                3 * LAMPORTS_PER_SOL,
            ))
            .event(&buy_json(
                "MINT1",
                2_005,
                "w2",
                Some("root"),
                3 * LAMPORTS_PER_SOL,
            ))
            .event(&buy_json(
                "MINT1",
                2_010,
                "w3",
                Some("root"),
                2 * LAMPORTS_PER_SOL,
            ))
            .event(&buy_json("MINT1", 8_000, "w4", None, 2 * LAMPORTS_PER_SOL))
            .event(&holders_json(
                "MINT1",
                9_000,
                &[("w1", 400), ("w2", 400), ("w3", 100), ("w4", 100)],
            ));

        let report = evaluate_streams(&one_stream("sybil", &fixture.text()), config(), "src");
        let sybil = &report.launches[0].sybil;

        assert_eq!(sybil.buyer_count, 4);
        assert_eq!(sybil.buy_volume_lamports, 10 * LAMPORTS_PER_SOL);
        assert_eq!(sybil.attributed_volume_lamports, 8 * LAMPORTS_PER_SOL);
        assert_eq!(sybil.unattributed_volume_lamports, 2 * LAMPORTS_PER_SOL);
        // Eight of ten SOL behind one root, measured over the whole so it is a
        // floor rather than a flattering ratio over the attributed part.
        assert_eq!(sybil.fund_bps, 8_000);
        assert_eq!(sybil.clusters.len(), 1);
        assert_eq!(sybil.clusters[0].wallet_count, 3);
        assert_eq!(sybil.holder_top1_bps, 4_000);
        assert_eq!(sybil.holder_hhi_bps, Some(3_400));
        assert!(sybil.temporal_influence_micros.expect("measurable") > 0);
        assert_eq!(report.sybil.largest_cluster_wallets, 3);
        assert_eq!(report.sybil.max_fund_bps, 8_000);
    }

    #[test]
    fn a_launch_nobody_bought_carries_no_cluster_numbers() {
        let mut fixture = FixtureBuilder::new("quiet");
        fixture
            .event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL))
            .event(&entry_json("MINT1", 2_000, LAMPORTS_PER_SOL));

        let report = evaluate_streams(&one_stream("quiet", &fixture.text()), config(), "src");
        let sybil = &report.launches[0].sybil;

        assert_eq!(sybil.buyer_count, 0);
        assert_eq!(sybil.fund_bps, 0);
        assert_eq!(sybil.sync_micros, None, "not measurable, not zero");
        assert_eq!(sybil.temporal_influence_micros, None);
        assert_eq!(sybil.holder_hhi_bps, None);
        assert_eq!(report.sybil.launches_with_buyers, 0);
    }

    // -----------------------------------------------------------------------
    // rug classification
    // -----------------------------------------------------------------------

    /// The token input whose sale takes `net_target` net lamports out of a curve
    /// holding `real_sol`.
    fn sell_for(real_sol: u64, net_target: u64) -> u64 {
        CurveState::at_real_sol(real_sol)
            .sell_tokens_for_target(net_target, DEFAULT_FEE_BPS)
            .expect("the curve can pay for it")
            .0
    }

    #[test]
    fn liquidity_leaving_outside_the_swap_path_is_a_rug() {
        let mut fixture = FixtureBuilder::new("pulled");
        fixture
            .event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL))
            .event(&buy_json("MINT1", 2_000, "w1", None, LAMPORTS_PER_SOL))
            .event(&pull_json("MINT1", 3_000, 35 * LAMPORTS_PER_SOL));

        let report = evaluate_streams(&one_stream("pulled", &fixture.text()), config(), "src");
        let launch = &report.launches[0];

        assert_eq!(launch.classified, RugClass::Rug);
        assert_eq!(launch.pulls, 1);
        assert_eq!(launch.pulled_lamports, 35 * LAMPORTS_PER_SOL);
        assert!(launch.final_real_sol_lamports < launch.peak_real_sol_lamports);
    }

    #[test]
    fn a_cliff_inside_the_window_is_a_rug_and_a_slow_slide_is_a_fade() {
        // The difference the classes are about is whether an exit existed while
        // it was happening, which is the only difference that matters to
        // whoever was holding.
        let cliff_tokens = sell_for(40 * LAMPORTS_PER_SOL, 34 * LAMPORTS_PER_SOL);
        let mut cliff = FixtureBuilder::new("cliff");
        cliff
            .event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL))
            .event(&sell_json("MINT1", 5_000, "insider", cliff_tokens));
        let report = evaluate_streams(&one_stream("cliff", &cliff.text()), config(), "src");
        assert_eq!(report.launches[0].classified, RugClass::Rug);
        assert!(report.launches[0].fastest_drop_bps >= 8_000);

        let fade_tokens = sell_for(40 * LAMPORTS_PER_SOL, 22 * LAMPORTS_PER_SOL);
        let mut fade = FixtureBuilder::new("fade");
        fade.event(&launch_json("MINT2", 1_000, 40 * LAMPORTS_PER_SOL))
            // Well outside the sixty-second window, so the same size of fall is
            // a different fact about the launch.
            .event(&sell_json("MINT2", 91_000, "seller", fade_tokens));
        let report = evaluate_streams(&one_stream("fade", &fade.text()), config(), "src");
        assert_eq!(report.launches[0].classified, RugClass::Faded);
        assert_eq!(report.launches[0].fastest_drop_bps, 0);
        assert!(report.launches[0].max_drop_bps >= 5_000);
    }

    #[test]
    fn a_launch_that_only_went_up_is_still_standing() {
        let mut fixture = FixtureBuilder::new("held");
        fixture
            .event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL))
            .event(&buy_json("MINT1", 2_000, "w1", None, 2 * LAMPORTS_PER_SOL))
            .event(&buy_json("MINT1", 3_000, "w2", None, 2 * LAMPORTS_PER_SOL));

        let report = evaluate_streams(&one_stream("held", &fixture.text()), config(), "src");
        assert_eq!(report.launches[0].classified, RugClass::Held);
        assert_eq!(report.launches[0].max_drop_bps, 0);
    }

    #[test]
    fn a_stream_that_says_nothing_is_classified_unknown_rather_than_guessed_at() {
        let mut fixture = FixtureBuilder::new("thin");
        fixture.event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL));

        let report = evaluate_streams(&one_stream("thin", &fixture.text()), config(), "src");
        assert_eq!(report.launches[0].classified, RugClass::Unknown);
    }

    #[test]
    fn our_own_exit_is_not_evidence_about_the_launch() {
        // A one SOL position is most of a thin curve, so counting our own exit
        // as the collapse would make the label a function of what the strategy
        // did — and a rug detector graded against labels its own trading
        // produced is grading itself.
        let mut fixture = FixtureBuilder::new("selfrug");
        fixture
            .event(&launch_json("MINT1", 1_000, LAMPORTS_PER_SOL))
            .event(&buy_json("MINT1", 1_500, "w1", None, LAMPORTS_PER_SOL / 10))
            .event(&entry_json("MINT1", 2_000, LAMPORTS_PER_SOL))
            .event(&exit_json("MINT1", 2_100, None));

        let report = evaluate_streams(&one_stream("selfrug", &fixture.text()), config(), "src");
        assert_eq!(report.launches[0].trades.len(), 1, "the trade did happen");
        assert_eq!(
            report.launches[0].classified,
            RugClass::Held,
            "and it is not what happened to the launch"
        );
    }

    #[test]
    fn the_classifier_is_graded_against_the_labels_and_the_money_separately() {
        let cliff_tokens = sell_for(40 * LAMPORTS_PER_SOL, 34 * LAMPORTS_PER_SOL);
        let mut fixture = FixtureBuilder::new("graded");
        fixture
            // A rug we called a rug, and stayed out of.
            .event(&launch_json("RUG_TP", 1_000, 40 * LAMPORTS_PER_SOL))
            .event(&buy_json("RUG_TP", 1_500, "w1", None, LAMPORTS_PER_SOL))
            .event(&pull_json("RUG_TP", 2_000, 38 * LAMPORTS_PER_SOL))
            .event(&label_json("RUG_TP", RugClass::Rug))
            // A survivor we left alone.
            .event(&launch_json("OK_TN", 1_000, 40 * LAMPORTS_PER_SOL))
            .event(&buy_json("OK_TN", 1_500, "w2", None, 2 * LAMPORTS_PER_SOL))
            .event(&label_json("OK_TN", RugClass::Held))
            // A survivor we called a rug.
            .event(&launch_json("OK_FP", 1_000, 40 * LAMPORTS_PER_SOL))
            .event(&sell_json("OK_FP", 5_000, "seller", cliff_tokens))
            .event(&label_json("OK_FP", RugClass::Held))
            // A rug we missed, and bought.
            .event(&launch_json("RUG_FN", 1_000, 40 * LAMPORTS_PER_SOL))
            .event(&buy_json("RUG_FN", 1_500, "w3", None, LAMPORTS_PER_SOL))
            .event(&entry_json("RUG_FN", 2_000, LAMPORTS_PER_SOL))
            .event(&exit_json("RUG_FN", 3_000, None))
            .event(&label_json("RUG_FN", RugClass::Rug));

        let report = evaluate_streams(&one_stream("graded", &fixture.text()), config(), "src");
        let rug = &report.rug;

        assert_eq!(rug.launches, 4);
        assert_eq!(rug.labelled, 4);
        assert_eq!(rug.labelled_rugs, 2);
        assert_eq!(rug.true_positives, 1);
        assert_eq!(rug.false_positives, 1);
        assert_eq!(rug.true_negatives, 1);
        assert_eq!(rug.false_negatives, 1);
        assert_eq!(rug.precision_bps, Some(5_000));
        assert_eq!(rug.recall_bps, Some(5_000));
        assert_eq!(rug.f1_bps, Some(5_000));
        assert_eq!(rug.accuracy_bps, Some(5_000));
        // One of the two labelled rugs was bought, so half were avoided.
        assert_eq!(rug.entered_labelled_rugs, 1);
        assert_eq!(rug.rug_avoidance_bps, Some(5_000));
        // And the money is reported apart from the accuracy, because a detector
        // that catches every rug and refuses every winner scores well on one and
        // badly on the other.
        assert!(rug.pnl_on_labelled_rugs_lamports < 0);
        assert_eq!(rug.pnl_on_labelled_non_rugs_lamports, 0);
        assert_eq!(rug.confusion.len(), 4);
    }

    #[test]
    fn an_unlabelled_launch_is_counted_and_graded_nowhere() {
        let mut fixture = FixtureBuilder::new("ungraded");
        fixture
            .event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL))
            .event(&buy_json("MINT1", 1_500, "w1", None, LAMPORTS_PER_SOL))
            .event(&pull_json("MINT1", 2_000, 38 * LAMPORTS_PER_SOL));

        let report = evaluate_streams(&one_stream("ungraded", &fixture.text()), config(), "src");
        assert_eq!(report.rug.classified_rugs, 1);
        assert_eq!(report.rug.ungraded, 1);
        assert_eq!(report.rug.labelled, 0);
        assert_eq!(
            report.rug.precision_bps, None,
            "nothing to be precise about"
        );
        assert_eq!(report.rug.rug_avoidance_bps, None);
    }

    #[test]
    fn a_label_of_unknown_is_not_ground_truth() {
        let mut fixture = FixtureBuilder::new("unknownlabel");
        fixture
            .event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL))
            .event(&buy_json("MINT1", 1_500, "w1", None, LAMPORTS_PER_SOL))
            .event(&pull_json("MINT1", 2_000, 38 * LAMPORTS_PER_SOL))
            .event(&label_json("MINT1", RugClass::Unknown));

        let report = evaluate_streams(
            &one_stream("unknownlabel", &fixture.text()),
            config(),
            "src",
        );
        assert_eq!(report.rug.labelled, 1);
        assert_eq!(report.rug.labelled_rugs, 0);
        assert_eq!(report.rug.true_positives, 0);
        assert_eq!(report.rug.false_positives, 0);
        assert_eq!(report.rug.precision_bps, None);
        // It is still in the confusion table, where somebody can see it.
        assert_eq!(report.rug.confusion.len(), 1);
        assert_eq!(report.rug.confusion[0].labelled, RugClass::Unknown);
        assert_eq!(report.rug.confusion[0].classified, RugClass::Rug);
    }

    #[test]
    fn a_launch_the_classifier_will_not_call_is_an_abstention_not_a_wrong_answer() {
        let mut fixture = FixtureBuilder::new("abstain");
        fixture
            .event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL))
            .event(&label_json("MINT1", RugClass::Rug));

        let report = evaluate_streams(&one_stream("abstain", &fixture.text()), config(), "src");
        assert_eq!(report.launches[0].classified, RugClass::Unknown);
        assert_eq!(report.rug.abstentions, 1);
        assert_eq!(report.rug.false_negatives, 0, "not a wrong answer");
        assert_eq!(
            report.rug.true_positives, 0,
            "and certainly not a right one"
        );
    }

    // -----------------------------------------------------------------------
    // the command line
    // -----------------------------------------------------------------------

    /// Runs `sts backtest ...` in process and returns what it wrote.
    fn cli(args: &[&str]) -> (i32, String, String) {
        let owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        let mut out: Vec<u8> = Vec::new();
        let mut errors: Vec<u8> = Vec::new();
        let code = cli::run(&owned, &mut out, &mut errors);
        (
            code,
            String::from_utf8(out).expect("stdout is UTF-8"),
            String::from_utf8(errors).expect("stderr is UTF-8"),
        )
    }

    #[test]
    fn the_window_starts_unless_the_first_argument_names_a_subcommand() {
        assert!(cli::is_subcommand("backtest"));
        assert!(!cli::is_subcommand("-psn_0_12345"), "a launch from Finder");
        assert!(!cli::is_subcommand("--devtools"));
    }

    #[test]
    fn help_is_printed_on_request_and_on_a_command_that_does_not_exist() {
        let (code, out, _) = cli(&["backtest", "help"]);
        assert_eq!(code, 0);
        assert!(out.contains("sts backtest verify"));

        let (code, out, _) = cli(&["backtest", "run", "--help"]);
        assert_eq!(code, 0);
        assert!(out.contains("EXIT CODES"));

        let (code, _, errors) = cli(&["backtest", "frobnicate"]);
        assert_eq!(code, 1);
        assert!(errors.contains("unknown command"));
    }

    #[test]
    fn a_command_line_that_cannot_be_read_is_refused_rather_than_defaulted() {
        // A typo silently dropped would produce a report computed against the
        // default policy with nothing in the output saying so.
        let (code, _, errors) = cli(&["backtest", "run", "--fixtures", "/tmp", "--fee-bpz", "50"]);
        assert_eq!(code, 1);
        assert!(errors.contains("--fee-bpz"));

        let (code, _, errors) = cli(&["backtest", "run"]);
        assert_eq!(code, 1);
        assert!(errors.contains("--fixtures is required"));

        let (code, _, errors) = cli(&["backtest", "run", "--fixtures"]);
        assert_eq!(code, 1);
        assert!(errors.contains("needs a value"));

        let (code, _, errors) = cli(&["backtest", "run", "--fixtures", "/tmp", "--fee-bps", "big"]);
        assert_eq!(code, 1);
        assert!(errors.contains("not a number"));
    }

    #[test]
    fn a_directory_with_no_streams_in_it_is_named_rather_than_reported_as_empty() {
        let scratch = Scratch::new("empty");
        scratch.write("notes.txt", "nothing to see");
        let path = scratch.as_arg();

        let (code, _, errors) = cli(&["backtest", "run", "--fixtures", &path]);
        assert_eq!(code, 3);
        assert!(errors.contains("no .jsonl fixture streams"));
    }

    #[test]
    fn verify_reads_the_chain_and_says_nothing_about_the_money() {
        let scratch = Scratch::new("verify-clean");
        scratch.write_fixture(&round_trip_fixture(), 1, true);
        let path = scratch.as_arg();

        let (code, out, errors) = cli(&["backtest", "verify", "--fixtures", &path]);
        assert_eq!(code, 0, "{errors}");

        let report: cli::VerifyReport = serde_json::from_str(&out).expect("valid JSON");
        assert!(report.integrity.gate_ready);
        assert_eq!(report.integrity.verified, 8);
        assert_eq!(report.streams.len(), 1);
        assert_eq!(report.streams[0].file, "000.jsonl");
        assert!(report.refusals.is_empty());
        let manifest = report.manifest.expect("the directory carries one");
        assert!(manifest.agrees);
        assert_eq!(manifest.stream_id, "phase3-a");
        assert!(!out.contains("realized_pnl"), "verify prices nothing");
    }

    #[test]
    fn verify_names_the_line_that_failed_and_leaves_with_a_two() {
        let scratch = Scratch::new("verify-broken");
        let fixture = round_trip_fixture();
        scratch.write_fixture(&fixture, 1, true);
        scratch.write(
            "000.jsonl",
            &edit_line(&fixture.text(), 5, "\"slot\":1004", "\"slot\":1044"),
        );
        let path = scratch.as_arg();

        let (code, out, errors) = cli(&["backtest", "verify", "--fixtures", &path]);
        assert_eq!(code, 2);
        assert!(errors.contains("did not verify"));

        let report: cli::VerifyReport = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(report.streams[0].first_break, Some(5));
        assert_eq!(
            report.streams[0].verdicts[0].status,
            LineStatus::SelfInconsistent
        );
        assert!(!report.integrity.gate_ready);
    }

    #[test]
    fn a_run_writes_a_report_that_reads_back_as_one() {
        let scratch = Scratch::new("run-clean");
        scratch.write_fixture(&round_trip_fixture(), 1, true);
        let path = scratch.as_arg();

        let (code, out, errors) = cli(&[
            "backtest",
            "run",
            "--fixtures",
            &path,
            "--sol-usd-cents",
            "15000",
        ]);
        assert_eq!(code, 0, "{errors}");

        let report: ForensicReport = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(report.schema, REPORT_SCHEMA);
        assert_eq!(report.config.cents_per_sol, 15_000);
        assert_eq!(report.launches.len(), 1);
        assert_eq!(report.performance.trades, 1);
        assert!(report.gate_ready);
    }

    #[test]
    fn a_gate_run_over_a_corrupted_corpus_still_produces_the_evidence() {
        // The report is what says what went wrong, so a refusal that printed
        // nothing would be a refusal nobody could act on.
        let scratch = Scratch::new("run-gated");
        let fixture = round_trip_fixture();
        scratch.write_fixture(&fixture, 1, true);
        scratch.write(
            "000.jsonl",
            &edit_line(&fixture.text(), 5, "\"slot\":1004", "\"slot\":1044"),
        );
        let path = scratch.as_arg();

        let (code, out, errors) = cli(&["backtest", "run", "--fixtures", &path, "--gate"]);
        assert_eq!(code, 2);
        assert!(errors.contains("refused"));

        let report: ForensicReport = serde_json::from_str(&out).expect("valid JSON");
        assert!(!report.gate_ready);
        assert!(report.refusals[0].contains("line 5"));
        assert_eq!(report.streams[0].first_break, Some(5));
    }

    #[test]
    fn two_runs_of_the_same_command_write_the_same_bytes() {
        // R1 through the command line rather than through the library, because
        // the command line is what the milestone verification runs and diffs.
        let scratch = Scratch::new("run-twice");
        scratch.write_fixture(&round_trip_fixture(), 1, true);
        let path = scratch.as_arg();

        let (first_code, first, _) = cli(&["backtest", "run", "--fixtures", &path]);
        let (second_code, second, _) = cli(&["backtest", "run", "--fixtures", &path]);

        assert_eq!(first_code, 0);
        assert_eq!(second_code, 0);
        assert_eq!(first, second);
    }

    #[test]
    fn a_report_can_be_written_to_a_file_instead_of_the_terminal() {
        let scratch = Scratch::new("run-out");
        scratch.write_fixture(&round_trip_fixture(), 1, true);
        let path = scratch.as_arg();
        let destination = scratch.path().join("report.json").display().to_string();

        let (code, out, errors) = cli(&[
            "backtest",
            "run",
            "--fixtures",
            &path,
            "--out",
            &destination,
        ]);
        assert_eq!(code, 0, "{errors}");
        assert!(out.contains("wrote"));

        let written = fs::read_to_string(&destination).expect("the report was written");
        let report: ForensicReport = serde_json::from_str(&written).expect("valid JSON");
        assert_eq!(report.launches.len(), 1);

        // Somewhere unwritable is a named failure, not a silent one.
        let (code, _, errors) = cli(&[
            "backtest",
            "run",
            "--fixtures",
            &path,
            "--out",
            "/nonexistent-directory/report.json",
        ]);
        assert_eq!(code, 3);
        assert!(errors.contains("report.json"));
    }

    #[test]
    fn a_rotated_stream_verifies_across_its_segments() {
        // §3.3: the chain runs across the roll, so the segments have to be read
        // in file-name order and under the one stream id the manifest names.
        // `read_dir` order is the filesystem's opinion and differs between
        // machines, which is why the listing is sorted.
        let scratch = Scratch::new("segments");
        scratch.write_fixture(&round_trip_fixture(), 4, true);
        let path = scratch.as_arg();

        let (code, out, errors) = cli(&["backtest", "verify", "--fixtures", &path]);
        assert_eq!(code, 0, "{errors}");

        let report: cli::VerifyReport = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(report.streams.len(), 4);
        assert_eq!(report.integrity.verified, 8);
        assert!(report.integrity.gate_ready);
        assert!(report.manifest.expect("carried").agrees);
    }

    #[test]
    fn segments_without_a_manifest_are_read_as_separate_streams_and_do_not_verify() {
        // Without the manifest there is nothing that says these four files are
        // one chain, so each stem starts a chain of its own and only the first
        // can verify against genesis. Failing here is right: guessing that four
        // files are one stream is how a spliced fixture gets read as a rotated
        // one.
        let scratch = Scratch::new("segments-bare");
        for (index, text) in round_trip_fixture().segments(4).into_iter().enumerate() {
            scratch.write(&format!("{index:03}.jsonl"), &text);
        }
        let path = scratch.as_arg();

        let (code, out, _) = cli(&["backtest", "verify", "--fixtures", &path]);
        assert_eq!(code, 2);
        let report: cli::VerifyReport = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(report.manifest, None);
        assert_eq!(report.streams.len(), 4);
        assert!(report.streams[1].first_break.is_some());
    }

    #[test]
    fn an_incomplete_recording_cannot_back_a_gate_run() {
        // R10. §3.2: an incomplete recording may be replayed for debugging and
        // may never be used in a gate run.
        let scratch = Scratch::new("incomplete");
        scratch.write_fixture(&round_trip_fixture(), 1, false);
        let path = scratch.as_arg();

        let (code, out, errors) = cli(&["backtest", "run", "--fixtures", &path, "--gate"]);
        assert_eq!(code, 2);
        assert!(errors.contains("incomplete"), "{errors}");

        let report: ForensicReport = serde_json::from_str(&out).expect("valid JSON");
        assert!(!report.gate_ready);
        assert!(!report.manifest.expect("carried").complete);
        // Every line still verified. The refusal is about the recording having
        // been stopped by an error, not about the bytes being wrong.
        assert_eq!(report.integrity.rejected, 0);
        assert!(report.integrity.gate_ready);
    }

    #[test]
    fn a_manifest_the_streams_do_not_bear_out_is_a_refusal() {
        let scratch = Scratch::new("manifest-disagrees");
        let fixture = round_trip_fixture();
        scratch.write_fixture(&fixture, 1, true);
        let mut manifest = manifest_for(&fixture, true);
        manifest.record_count = 99;
        scratch.write(
            "manifest.json",
            &serde_json::to_string(&manifest).expect("serialises"),
        );
        let path = scratch.as_arg();

        let (code, out, errors) = cli(&["backtest", "run", "--fixtures", &path, "--gate"]);
        assert_eq!(code, 2);
        assert!(errors.contains("the manifest declares 99"), "{errors}");

        let report: ForensicReport = serde_json::from_str(&out).expect("valid JSON");
        let check = report.manifest.expect("carried");
        assert!(!check.agrees);
        assert_eq!(check.declared_records, 99);
        assert_eq!(check.observed_records, 8);
    }

    #[test]
    fn a_manifest_that_will_not_read_is_named_rather_than_ignored() {
        let scratch = Scratch::new("manifest-broken");
        scratch.write_fixture(&round_trip_fixture(), 1, true);
        scratch.write("manifest.json", "{\"schema\":\"sts.replay.manifest.v9\"}");
        let path = scratch.as_arg();

        let (code, _, errors) = cli(&["backtest", "run", "--fixtures", &path]);
        assert_eq!(code, 3);
        assert!(errors.contains("manifest.json"), "{errors}");
    }

    #[test]
    fn the_sandwich_table_prints_the_three_positions_by_default() {
        let (code, out, errors) = cli(&["backtest", "sandwich"]);
        assert_eq!(code, 0, "{errors}");

        let table: cli::SandwichTable = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(table.rows.len(), 3);
        assert_eq!(table.fee_bps, DEFAULT_FEE_BPS);
        assert_eq!(table.rows[0].breakeven_victim_lamports, 306_091_216);
        assert_eq!(table.rows[1].breakeven_victim_lamports, 765_228_038);
        assert_eq!(table.rows[2].breakeven_victim_lamports, 1_173_349_659);
        assert!(table.rows.iter().all(|row| row.verdict.above_threshold));
    }

    #[test]
    fn the_sandwich_table_prices_a_victim_size_when_it_is_given_one() {
        let (code, out, errors) = cli(&[
            "backtest",
            "sandwich",
            "--reserves-sol",
            "75",
            "--victim-lamports",
            "1000000000",
        ]);
        assert_eq!(code, 0, "{errors}");

        let table: cli::SandwichTable = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(table.rows.len(), 1);
        // §15.3: inside the band, a public 1 SOL buy against an attacker capped
        // at 1 SOL costs 190 to 260 basis points.
        let damage = table.rows[0].verdict.damage_bps;
        assert!((190..=260).contains(&damage), "damage was {damage} bps");

        let (code, _, errors) = cli(&["backtest", "sandwich", "--reserves-sol", "ten"]);
        assert_eq!(code, 1);
        assert!(errors.contains("not a number"));
    }

    // -----------------------------------------------------------------------
    // B1 — synthetic benchmarks with exact answers
    // -----------------------------------------------------------------------

    #[test]
    fn a_star_of_wallets_behind_one_funder_scores_exactly_one() {
        // §7.1's star row: one funder, N leaves, buying in the same instant.
        // Every number here has a closed form, so this is a check on the
        // arithmetic rather than a regression against whatever it printed last.
        let mut fixture = FixtureBuilder::new("star");
        fixture.event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL));
        for index in 0..4 {
            fixture.event(&buy_json(
                "MINT1",
                2_000,
                &format!("leaf-{index}"),
                Some("root"),
                LAMPORTS_PER_SOL,
            ));
        }
        fixture.event(&holders_json(
            "MINT1",
            3_000,
            &[
                ("leaf-0", 250),
                ("leaf-1", 250),
                ("leaf-2", 250),
                ("leaf-3", 250),
            ],
        ));

        let report = evaluate_streams(&one_stream("star", &fixture.text()), config(), "b1");
        let sybil = &report.launches[0].sybil;

        assert_eq!(sybil.buyer_count, 4);
        assert_eq!(sybil.fund_bps, 10_000, "every lamport points at one root");
        assert_eq!(sybil.sync_micros, Some(MICROS), "all four in one instant");
        assert_eq!(sybil.temporal_influence_micros, Some(MICROS));
        assert_eq!(
            sybil.buyer_diversity_bps,
            Some(0),
            "four keypairs, one entity, no diversity"
        );
        assert_eq!(sybil.holder_hhi_bps, Some(2_500), "four equal holders");
        assert_eq!(sybil.holder_top1_bps, 2_500);
        assert_eq!(sybil.effective_holders_micros, 4 * MICROS);

        let cluster = &sybil.clusters[0];
        assert_eq!(cluster.funder, "root");
        assert_eq!(cluster.wallet_count, 4);
        assert_eq!(cluster.flow_share_bps, 10_000);
        assert_eq!(cluster.sync_micros, MICROS);
        assert_eq!(cluster.temporal_influence_micros, MICROS);
        assert_eq!(cluster.first_buy_span_ms, 0);
        assert!(!cluster.sync_truncated);
        assert_eq!(report.sybil.launches_over_floor, 1);
    }

    #[test]
    fn the_same_wallets_spread_over_an_hour_score_nothing_for_synchrony() {
        // The other half of the geometric mean: the funding is identical and
        // only the timing changed, so `fund` stays at one and the stored score
        // collapses.
        let mut fixture = FixtureBuilder::new("spread");
        fixture.event(&launch_json("MINT1", 1_000, 40 * LAMPORTS_PER_SOL));
        for index in 0..4i64 {
            fixture.event(&buy_json(
                "MINT1",
                2_000 + index * 1_200_000,
                &format!("leaf-{index}"),
                Some("root"),
                LAMPORTS_PER_SOL,
            ));
        }

        let report = evaluate_streams(&one_stream("spread", &fixture.text()), config(), "b1");
        let sybil = &report.launches[0].sybil;

        assert_eq!(sybil.fund_bps, 10_000);
        assert_eq!(sybil.sync_micros, Some(0), "twenty minutes apart");
        assert_eq!(sybil.temporal_influence_micros, Some(0));
        assert_eq!(report.sybil.launches_over_floor, 0);
    }

    // -----------------------------------------------------------------------
    // end to end
    // -----------------------------------------------------------------------

    /// Four launches: two we made money on, two we lost on, and enough flow
    /// between the legs for the returns to differ.
    fn mixed_book() -> FixtureBuilder {
        let mut fixture = FixtureBuilder::new("phase3");
        let drop_tokens = sell_for(41 * LAMPORTS_PER_SOL, 15 * LAMPORTS_PER_SOL);

        for (index, winner) in [true, false, true, false].into_iter().enumerate() {
            let mint = format!("MINT{index}");
            let base = 100_000i64 * (index as i64 + 1);
            fixture
                .event(&launch_json(&mint, base, 40 * LAMPORTS_PER_SOL))
                .event(&buy_json(
                    &mint,
                    base + 100,
                    &format!("w{index}"),
                    Some("root"),
                    LAMPORTS_PER_SOL,
                ))
                .event(&entry_json(&mint, base + 200, LAMPORTS_PER_SOL));
            if winner {
                fixture.event(&buy_json(
                    &mint,
                    base + 300,
                    "whale",
                    None,
                    20 * LAMPORTS_PER_SOL,
                ));
            } else {
                fixture.event(&sell_json(&mint, base + 300, "dumper", drop_tokens));
            }
            fixture
                .event(&exit_json(&mint, base + 30_000, None))
                .event(&label_json(
                    &mint,
                    if winner {
                        RugClass::Held
                    } else {
                        RugClass::Faded
                    },
                ));
        }
        fixture
    }

    #[test]
    fn a_mixed_book_produces_every_headline_number() {
        let report = evaluate_streams(&one_stream("phase3", &mixed_book().text()), config(), "e2e");

        assert_eq!(report.launches.len(), 4);
        assert_eq!(report.performance.trades, 4);
        assert_eq!(report.performance.winners, 2);
        assert_eq!(report.performance.losers, 2);
        assert_eq!(report.performance.win_rate_bps, 5_000);
        assert!(report.performance.gross_profit_lamports > 0);
        assert!(report.performance.gross_loss_lamports > 0);
        assert!(report.performance.profit_factor_micros.is_some());
        assert!(report.performance.sharpe_micros.is_some(), "four trades");
        assert_eq!(report.performance.average_hold_ms, 29_800);
        assert_eq!(report.performance.median_hold_ms, 29_800);
        assert!(report.performance.fees_paid_lamports > 0);

        // The book alternates, so equity goes under its high-water mark.
        assert!(report.risk.max_drawdown_lamports > 0);
        assert!(report.risk.max_drawdown_bps > 0);
        assert_eq!(report.risk.longest_losing_streak, 1);
        assert_eq!(report.risk.positions_stranded, 0);

        // The mean and the deviation are consistent with the Sharpe beside them.
        let returns: Vec<i32> = report
            .launches
            .iter()
            .flat_map(|launch| launch.trades.iter().map(|t| t.return_bps))
            .collect();
        let (mean, stddev) = return_moments(&returns);
        assert_eq!(report.performance.mean_return_bps_micros, mean);
        assert_eq!(report.performance.stddev_return_bps_micros, stddev);
        assert_eq!(report.performance.sharpe_micros, sharpe_micros(&returns));

        assert_eq!(report.rug.labelled, 4);
        assert_eq!(report.rug.labelled_rugs, 0);
        assert_eq!(report.adverse_selection.entries_priced, 4);
        assert!(report.gate_ready, "{:?}", report.refusals);
    }

    #[test]
    fn a_whole_directory_runs_through_the_library_the_way_it_does_through_the_command() {
        let scratch = Scratch::new("directory");
        let fixture = mixed_book();
        scratch.write_fixture(&fixture, 3, true);

        let from_directory =
            evaluate_directory(scratch.path(), config()).expect("the directory reads");
        let from_memory = evaluate_streams_with(
            &fixture
                .segments(3)
                .into_iter()
                .enumerate()
                .map(|(index, text)| FixtureSource {
                    stream_id: "phase3".to_string(),
                    file: format!("{index:03}.jsonl"),
                    text,
                })
                .collect::<Vec<_>>(),
            config(),
            &scratch.path().display().to_string(),
            Some(manifest_for(&fixture, true)),
        );

        assert_eq!(from_directory.to_json(), from_memory.to_json());
        assert!(from_directory.gate_ready, "{:?}", from_directory.refusals);
    }

    #[test]
    fn every_knob_on_the_command_line_reaches_the_report_that_was_computed_with_it() {
        // A report that did not carry its own policy would not be reproducible,
        // and the whole determinism argument is that the report is a function of
        // the fixture and this configuration.
        let scratch = Scratch::new("knobs");
        scratch.write_fixture(&round_trip_fixture(), 1, true);
        let path = scratch.as_arg();

        let (code, out, errors) = cli(&[
            "backtest",
            "run",
            "--fixtures",
            &path,
            "--fee-bps",
            "125",
            "--sol-usd-cents",
            "22500",
            "--starting-lamports",
            "7000000000",
            "--landing-cost-lamports",
            "1234567",
            "--max-attacker-lamports",
            "3000000000",
            "--tau-sync-ms",
            "2500",
            "--min-cluster-wallets",
            "3",
            "--rug-drop-bps",
            "7000",
            "--rug-window-ms",
            "45000",
            "--fade-drop-bps",
            "4000",
        ]);
        assert_eq!(code, 0, "{errors}");

        let report: ForensicReport = serde_json::from_str(&out).expect("valid JSON");
        assert_eq!(
            report.config,
            BacktestConfig {
                fee_bps: 125,
                cents_per_sol: 22_500,
                starting_equity_lamports: 7_000_000_000,
                landing_cost_lamports: 1_234_567,
                max_attacker_lamports: 3_000_000_000,
                tau_sync_ms: 2_500,
                min_cluster_wallets: 3,
                rug_drop_bps: 7_000,
                rug_window_ms: 45_000,
                fade_drop_bps: 4_000,
                gate: false,
            }
        );
        assert_eq!(report.performance.starting_equity_lamports, 7_000_000_000);
    }

    #[test]
    fn a_corpus_of_many_launches_stays_deterministic_and_keeps_them_apart() {
        // The scale check. Forty launches interleaved in one stream, which is
        // what the recorder actually produces: events for different mints are
        // not grouped, so the runner has to keep forty sets of books at once.
        const LAUNCHES: usize = 40;
        let mut fixture = FixtureBuilder::new("scale");
        for index in 0..LAUNCHES {
            fixture.event(&launch_json(
                &format!("M{index:03}"),
                1_000,
                40 * LAMPORTS_PER_SOL,
            ));
        }
        for round in 0..6i64 {
            for index in 0..LAUNCHES {
                let mint = format!("M{index:03}");
                fixture.event(&buy_json(
                    &mint,
                    2_000 + round * 1_000,
                    &format!("w{index:03}-{round}"),
                    Some(&format!("root-{}", index % 5)),
                    LAMPORTS_PER_SOL / 2,
                ));
            }
        }
        for index in 0..LAUNCHES {
            let mint = format!("M{index:03}");
            fixture
                .event(&entry_json(&mint, 20_000, LAMPORTS_PER_SOL / 4))
                .event(&exit_json(&mint, 30_000, None));
        }

        let text = fixture.text();
        let first = evaluate_streams(&one_stream("scale", &text), config(), "scale");
        let second = evaluate_streams(&one_stream("scale", &text), config(), "scale");

        assert_eq!(first.to_json(), second.to_json());
        assert_eq!(first.launches.len(), LAUNCHES);
        assert_eq!(first.integrity.records, LAUNCHES * 9);
        assert_eq!(first.performance.trades, LAUNCHES as u32);
        assert!(first.gate_ready, "{:?}", first.refusals);
        // Each launch saw its own six buyers and nobody else's.
        assert!(first.launches.iter().all(|l| l.sybil.buyer_count == 6));
        // The mints are listed in the order they were first seen, which is the
        // order they appear in the stream.
        let order: Vec<&str> = first.launches.iter().map(|l| l.mint.as_str()).collect();
        assert_eq!(order[0], "M000");
        assert_eq!(order[LAUNCHES - 1], format!("M{:03}", LAUNCHES - 1));
    }

    // -----------------------------------------------------------------------
    // the vocabularies
    // -----------------------------------------------------------------------

    #[test]
    fn the_names_in_the_report_are_the_names_the_code_parses() {
        // Two encoders for one vocabulary: `as_str` writes the fixture side and
        // serde writes the report side. A rename that drifts between them would
        // silently break every consumer of the report and nothing would fail.
        for class in RugClass::ALL {
            let json = serde_json::to_string(&class).expect("serialises");
            assert_eq!(json, format!("\"{}\"", class.as_str()));
            assert_eq!(RugClass::parse(class.as_str()), Some(class));
            assert_eq!(class.to_string(), class.as_str());
        }
        assert_eq!(RugClass::parse("nonsense"), None);

        for status in [
            LineStatus::Verified,
            LineStatus::Unparseable,
            LineStatus::SeqGap,
            LineStatus::SelfInconsistent,
            LineStatus::ChainBroken,
            LineStatus::OutOfOrder,
            LineStatus::UnverifiableAfterBreak,
        ] {
            let json = serde_json::to_string(&status).expect("serialises");
            assert_eq!(json, format!("\"{}\"", status.as_str()));
            assert_eq!(status.to_string(), status.as_str());
        }

        for side in [Side::Buy, Side::Sell] {
            assert_eq!(
                serde_json::to_string(&side).expect("serialises"),
                format!("\"{}\"", side.as_str())
            );
        }
    }

    #[test]
    fn every_event_names_the_launch_it_is_about_and_says_what_it_is() {
        let events = [
            (launch_json("M", 1, 0), "launch"),
            (buy_json("M", 1, "w", None, 1), "flow"),
            (entry_json("M", 1, 1), "entry"),
            (exit_json("M", 1, None), "exit"),
            (holders_json("M", 1, &[("w", 1)]), "holders"),
            (pull_json("M", 1, 1), "pull"),
            (label_json("M", RugClass::Rug), "label"),
        ];
        for (json, kind) in events {
            let event = decode_event(json.as_bytes(), 7).expect("decodes");
            assert_eq!(event.kind(), kind);
            assert_eq!(event.mint(), "M");
        }
    }

    #[test]
    fn a_malformed_event_says_which_field_it_could_not_read() {
        let cases = [
            ("", "not JSON"),
            ("[]", "not a JSON object"),
            ("{\"kind\":\"launch\",\"mint\":\"M\"}", "missing schema"),
            (
                "{\"schema\":\"sts.backtest.v1\",\"mint\":\"M\"}",
                "missing kind",
            ),
            (
                "{\"schema\":\"sts.backtest.v1\",\"kind\":\"launch\"}",
                "missing mint",
            ),
            (
                "{\"schema\":\"sts.backtest.v1\",\"kind\":\"wobble\",\"mint\":\"M\"}",
                "unknown kind",
            ),
            (
                "{\"schema\":\"sts.backtest.v1\",\"kind\":\"flow\",\"mint\":\"M\",\
                 \"at_ms\":1,\"wallet\":\"w\",\"side\":\"sideways\"}",
                "unknown side",
            ),
            (
                "{\"schema\":\"sts.backtest.v1\",\"kind\":\"label\",\"mint\":\"M\",\
                 \"outcome\":\"fine\"}",
                "unknown outcome",
            ),
            (
                "{\"schema\":\"sts.backtest.v1\",\"kind\":\"launch\",\"mint\":\"M\",\
                 \"at_ms\":\"soon\"}",
                "at_ms is not an integer",
            ),
            (
                "{\"schema\":\"sts.backtest.v1\",\"kind\":\"holders\",\"mint\":\"M\",\
                 \"at_ms\":1,\"holders\":{}}",
                "holders is not an array",
            ),
        ];
        for (json, expected) in cases {
            let error = decode_event(json.as_bytes(), 3).expect_err("should not decode");
            assert_eq!(error.seq, 3);
            assert!(
                error.detail.contains(expected),
                "{json:?} said {:?}, expected something containing {expected:?}",
                error.detail
            );
            assert!(error.to_string().contains("record 3"));
        }
    }

    #[test]
    fn a_launch_can_start_from_six_explicit_reserves_or_from_a_position_on_the_curve() {
        // Three forms, most specific first. The middle one is what a recorder
        // produces when it saw the curve mid-life and only logged the real SOL.
        let explicit = format!(
            "{{\"schema\":\"{EVENT_SCHEMA}\",\"kind\":\"launch\",\"mint\":\"M\",\"at_ms\":1,\
             \"curve\":{{\"virtual_token_reserves\":10,\"virtual_sol_reserves\":20,\
             \"real_token_reserves\":30,\"real_sol_reserves\":40,\"token_total_supply\":50,\
             \"complete\":true}}}}"
        );
        let LaunchEvent::Launch(open) = decode_event(explicit.as_bytes(), 0).expect("decodes")
        else {
            panic!("expected a launch");
        };
        assert_eq!(open.curve, CurveState::from_parts(10, 20, 30, 40, 50, true));

        let derived = launch_json("M", 1, 40 * LAMPORTS_PER_SOL);
        let LaunchEvent::Launch(open) = decode_event(derived.as_bytes(), 0).expect("decodes")
        else {
            panic!("expected a launch");
        };
        assert_eq!(open.curve, CurveState::at_real_sol(40 * LAMPORTS_PER_SOL));

        let bare = format!(
            "{{\"schema\":\"{EVENT_SCHEMA}\",\"kind\":\"launch\",\"mint\":\"M\",\"at_ms\":1}}"
        );
        let LaunchEvent::Launch(open) = decode_event(bare.as_bytes(), 0).expect("decodes") else {
            panic!("expected a launch");
        };
        assert_eq!(open.curve, CurveState::LAUNCH);
        assert_eq!(open.creator, None);
    }
}
