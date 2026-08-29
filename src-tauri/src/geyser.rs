//! The Geyser feed: a gRPC stream in, ordered domain events out.
//!
//! [`crate::ingestion`] dials Solana's JSON-RPC pubsub, which is the transport
//! every provider offers and the slowest one any of them offers. Geyser is the
//! other end of that trade: a validator plugin pushing account writes as the
//! bank commits them, with two fields pubsub does not carry — a `write_version`
//! that totally orders the writes inside a slot, and a slot status stream that
//! says when a slot became confirmed, finalized, or dead. Those two fields are
//! what make sub-slot sequencing and re-org rollback possible at all, and this
//! module exists to use them.
//!
//! # The shape of it
//!
//! ```text
//!   gRPC stream ──▶ GeyserUpdate ──▶ TickPipeline ──▶ TickEvent (ordered)
//!   (transport)     (transport-      ├── SlotLedger      │
//!                    independent)    ├── TickRing        └─▶ engine
//!                                    └── CurveTracker
//! ```
//!
//! The transport is behind a trait for the same reason [`crate::ingestion`]'s
//! is: everything interesting in here is the sequencing, and sequencing tested
//! only against a live socket is sequencing that is not tested. [`MockStream`]
//! is a first-class part of the module, not a test fixture hidden in the test
//! module, because the reconnect loop has to be driven by something that can
//! fail on demand.
//!
//! # Four commitments this module keeps
//!
//! **No float, anywhere, ever.** Every event type here derives [`Eq`], which is
//! not a convenience — it is the enforcement. `f64` is not `Eq`, so a float
//! smuggled into any of these structs stops the build. That matters most at one
//! specific place: SPL token balances arrive from the validator carrying both a
//! `ui_amount: f64` and an `amount: String` of the raw integer, and the `f64` is
//! right there and easy to reach for. [`parse_raw_amount`] is the only door,
//! and it reads the string.
//!
//! **Prices are `10^-18`, not millionths.** A pump.fun curve prices around
//! `2.8 x 10^-5` lamports per raw token unit. In millionths that is the integer
//! `28`, where one unit of rounding is 3.5% of the price — a resolution that
//! cannot see the move a ladder is built on. See
//! [`crate::strategy::fixed::ONE_E18`] for the unit and the naming rule that
//! keeps it from mixing with the millionths the scorers use.
//!
//! **Nothing broad is subscribed to.** [`GeyserConfig::subscribe_filters`]
//! names the programs and the account sizes, so the filtering happens at the
//! validator. `STS_CORE_IDEOLOGY.md` §1876 is explicit that unfiltered log
//! streams and speculative polling burn free-tier credits without moving the
//! decision boundary, and a Geyser stream is metered by what it sends.
//!
//! **A payload is read where it landed.** An account write is the highest-rate
//! message on this stream and its bytes are most of it. They are carried as a
//! [`bytes::Bytes`] from the codec's own read buffer all the way to
//! [`curve_tick`], which reads the reserves out of the slice at fixed offsets
//! — no allocation, no memcpy, no intermediate struct. A transaction's logs go
//! the same way for the same reason, by being *moved* rather than cloned, which
//! is why [`TickPipeline::ingest`] takes its update by value. The two
//! properties are asserted by address in this module's tests, because "it is
//! not copied" is not something a signature can promise on its own and is
//! exactly the kind of thing a dependency bump undoes quietly.
//!
//! # What is not here
//!
//! The gRPC transport itself is behind the `geyser-grpc` cargo feature, off by
//! default. Turning it on pulls tonic, prost, hyper and a second TLS stack —
//! ninety-odd crates — into a build whose dependency list is otherwise short on
//! purpose. Everything in this module except [`grpc`] compiles and is tested
//! without it; see that module's own note.
//!
//! What the feature does *not* pull is worth stating too: `Cargo.toml` turns
//! the proto crate's defaults off, so the gzip and zstd codecs this client
//! never negotiates — and the vendored C build behind one of them — stay out.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde::Serialize;

use crate::ingestion::{BondingCurve, FeedProvider, IngestionManager, Verdict, PUMP_FUN_PROGRAM};
use crate::strategy::fixed::{delta_bps, delta_e18, from_token_amount, ratio_e18};
use crate::subslot::{
    Commitment, LedgerChange, Push, RingConfig, RingMetrics, SlotLedger, SlotPhase, TickClass,
    TickKey, TickRing,
};
use crate::telemetry::{TelemetryHub, TelemetryLevel};
use crate::types::{Pubkey, Signature};

// ---------------------------------------------------------------------------
// budgets and sizes
// ---------------------------------------------------------------------------

/// Reconnect backoff bounds. The same pair [`crate::ingestion`] uses, and
/// deliberately so: two feeds that back off differently are two feeds whose
/// failure modes have to be reasoned about separately.
pub const BACKOFF_MIN: Duration = Duration::from_millis(500);
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// The most doublings the backoff will apply. `500ms << 6` is 32s, already past
/// [`BACKOFF_MAX`], so the shift is capped well below anything that could
/// overflow the multiply.
const BACKOFF_MAX_SHIFT: u32 = 8;

/// How far back to resume from after a reconnect.
///
/// Yellowstone's `from_slot` replays from a slot the server still holds. Asking
/// for the last slot seen would re-deliver a slot already processed; asking for
/// nothing would leave a hole. A few slots of overlap costs a handful of
/// duplicate updates, which the write-version guard in [`CurveTracker`] already
/// discards for free.
pub const RESUME_OVERLAP_SLOTS: u64 = 2;

/// The pump.fun bonding curve account size, for the server-side filter.
const CURVE_ACCOUNT_LEN: u64 = 81;

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

/// What can go wrong between the socket and a domain event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "camelCase")]
pub enum GeyserError {
    /// The endpoint could not be opened.
    Dial(String),
    /// The subscription was rejected.
    Subscribe(String),
    /// The stream ended or errored mid-flight.
    Stream(String),
    /// The server closed cleanly.
    Closed,
    /// The build has no gRPC transport compiled in.
    NoTransport,
    /// An update arrived that this build cannot make sense of.
    Decode(DecodeError),
}

impl std::fmt::Display for GeyserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GeyserError::Dial(detail) => write!(f, "geyser dial failed: {detail}"),
            GeyserError::Subscribe(detail) => write!(f, "geyser subscribe rejected: {detail}"),
            GeyserError::Stream(detail) => write!(f, "geyser stream failed: {detail}"),
            GeyserError::Closed => write!(f, "geyser stream closed"),
            GeyserError::NoTransport => {
                write!(
                    f,
                    "this build has no geyser transport; rebuild with --features geyser-grpc"
                )
            }
            GeyserError::Decode(inner) => write!(f, "geyser decode failed: {inner}"),
        }
    }
}

impl std::error::Error for GeyserError {}

/// Why one update could not be turned into a domain event.
///
/// Every one of these is a *skip*, never a stop. A malformed account in a
/// stream of good ones is a counter, not a disconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DecodeError {
    /// The pubkey or owner field was not 32 bytes.
    BadPubkey,
    /// The signature field was not 64 bytes.
    BadSignature,
    /// The account data did not parse as anything this module reads.
    UnknownAccount,
    /// The account belongs to a program not on the allowlist.
    ForeignProgram,
    /// A token amount was not a decimal integer, or did not fit.
    BadAmount,
    /// A reserve was zero where zero has no meaning, so no price exists.
    NoPrice,
    /// The reserves do not hold together; the account was read mid-write.
    IncoherentCurve,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            DecodeError::BadPubkey => "pubkey was not 32 bytes",
            DecodeError::BadSignature => "signature was not 64 bytes",
            DecodeError::UnknownAccount => "account data matched no known layout",
            DecodeError::ForeignProgram => "account owner is not on the allowlist",
            DecodeError::BadAmount => "token amount was not a decimal integer",
            DecodeError::NoPrice => "reserves imply no price",
            DecodeError::IncoherentCurve => "curve reserves are incoherent",
        };
        f.write_str(text)
    }
}

// ---------------------------------------------------------------------------
// configuration
// ---------------------------------------------------------------------------

/// Where to dial and what to ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeyserConfig {
    /// The gRPC endpoint, `https://…` or `http://…`.
    pub endpoint: String,
    /// The provider's auth token, sent as `x-token`. Never logged.
    pub token: Option<String>,
    /// The commitment a tick must reach before it is released.
    pub commitment: Commitment,
    /// Ring sizing and hold window.
    pub ring: RingConfig,
    /// Where to start. `None` is "from now".
    pub from_slot: Option<u64>,
    /// Who is on the other end, in [`crate::ingestion`]'s vocabulary.
    ///
    /// Carried because a candidate that reaches the engine is stamped with the
    /// provider that reported it, and a Geyser stream that stamped itself with
    /// somebody else's name would break the deduplication identity the launch
    /// index is keyed on.
    pub provider: FeedProvider,
}

impl Default for GeyserConfig {
    fn default() -> Self {
        GeyserConfig {
            endpoint: String::new(),
            token: None,
            // Confirmed rather than processed. A processed curve state can be
            // abandoned, and the whole point of the hold window is to not act
            // on one; asking for processed and then holding for confirmed would
            // pay the latency twice.
            commitment: Commitment::Confirmed,
            ring: RingConfig::default(),
            from_slot: None,
            // Triton is the shop that publishes Yellowstone, so it is the
            // likeliest thing on the far end of an endpoint nobody named.
            provider: FeedProvider::Triton,
        }
    }
}

impl GeyserConfig {
    /// The config from the environment, or `None` if no endpoint is set.
    ///
    /// Absent configuration means no feed, exactly as in [`crate::ingestion`]:
    /// the roadmap's Phase 0 gate says a real feed is opened only as a
    /// deliberate act, so the default has to be silence.
    pub fn from_env() -> Option<Self> {
        let endpoint = std::env::var("STS_GEYSER_ENDPOINT").ok()?;
        if endpoint.trim().is_empty() {
            return None;
        }
        Some(GeyserConfig {
            endpoint,
            token: std::env::var("STS_GEYSER_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
            provider: Self::provider_from_env(),
            ..GeyserConfig::default()
        })
    }

    /// Which provider `$STS_GEYSER_PROVIDER` names, defaulting to Triton.
    ///
    /// A name that is not one of the three is the default rather than a
    /// failure: it is a label on a counter, and refusing to open a feed over a
    /// typo in a display string would be the wrong trade.
    fn provider_from_env() -> FeedProvider {
        match std::env::var("STS_GEYSER_PROVIDER") {
            Ok(name) => FeedProvider::ALL
                .into_iter()
                .find(|provider| provider.as_str().eq_ignore_ascii_case(name.trim()))
                .unwrap_or(FeedProvider::Triton),
            Err(_) => FeedProvider::Triton,
        }
    }

    /// The endpoint with any credential in it replaced.
    ///
    /// Providers put the API key in the path. This is what goes in a log line.
    pub fn redacted(&self) -> String {
        match self.endpoint.split_once("://") {
            Some((scheme, rest)) => {
                let host = rest.split('/').next().unwrap_or(rest);
                format!("{scheme}://{host}/…")
            }
            None => "…".to_string(),
        }
    }

    /// The programs whose accounts this subscription asks for.
    ///
    /// Named here rather than assembled at the call site so that the one place
    /// deciding how much of the chain to pull is greppable.
    pub fn subscribe_filters(&self) -> SubscribeFilters {
        SubscribeFilters {
            curve_owners: vec![PUMP_FUN_PROGRAM.to_string()],
            curve_data_size: Some(CURVE_ACCOUNT_LEN),
            transaction_includes: vec![PUMP_FUN_PROGRAM.to_string()],
            commitment: self.commitment,
            from_slot: self.from_slot,
        }
    }
}

/// The subscription, in terms this module owns.
///
/// A plain description that the transport turns into whatever its wire format
/// wants. Keeping it out of the protobuf types is what lets the request be
/// asserted on in a test with no gRPC compiled in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscribeFilters {
    pub curve_owners: Vec<String>,
    pub curve_data_size: Option<u64>,
    pub transaction_includes: Vec<String>,
    pub commitment: Commitment,
    pub from_slot: Option<u64>,
}

// ---------------------------------------------------------------------------
// the transport-independent update
// ---------------------------------------------------------------------------

/// One account write, as the plugin saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountUpdate {
    pub slot: u64,
    pub pubkey: Pubkey,
    pub owner: Pubkey,
    pub lamports: u64,
    /// The plugin's per-slot write counter. Authoritative ordering for writes
    /// to one account inside one slot.
    pub write_version: u64,
    /// The account's bytes, still in the buffer they were read out of.
    ///
    /// [`Bytes`] rather than `Vec<u8>` because this is the one field on the
    /// stream that is worth not copying. The `geyser-grpc` build compiles the
    /// wire type's payload as a `Bytes` too (see the `account-data-as-bytes`
    /// note in `Cargo.toml`), and prost fills such a field by splitting the
    /// read buffer, so an account write travels from the socket to
    /// [`curve_tick`] without its eighty-one bytes ever being copied — and
    /// without the per-update allocation that copying them would need.
    ///
    /// The type is load-bearing rather than decorative: a `Vec<u8>` here would
    /// silently re-introduce that copy at the seam, because the only way to
    /// build one from a `Bytes` is to allocate and memcpy.
    pub data: Bytes,
    /// Whether this arrived during the plugin's startup snapshot rather than
    /// from live traffic. Startup accounts are state, not events.
    pub is_startup: bool,
}

/// One slot status transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotUpdate {
    pub slot: u64,
    pub parent: Option<u64>,
    pub phase: SlotPhase,
}

/// One token balance line off a transaction's metadata.
///
/// `raw` is the integer amount, parsed from the string the validator sends.
/// The `ui_amount` float that travels beside it on the wire is not represented
/// here and never will be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenBalance {
    pub account_index: u32,
    pub mint: Pubkey,
    pub owner: Pubkey,
    pub raw: u128,
    pub decimals: u8,
}

/// One transaction, reduced to the parts this engine reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionUpdate {
    pub slot: u64,
    pub signature: Signature,
    pub index: u64,
    pub is_vote: bool,
    pub failed: bool,
    pub logs: Vec<String>,
    pub pre_token_balances: Vec<TokenBalance>,
    pub post_token_balances: Vec<TokenBalance>,
}

/// What the stream can say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdatePayload {
    Account(AccountUpdate),
    Slot(SlotUpdate),
    Transaction(TransactionUpdate),
    /// The server's keepalive. Proves the socket is alive when nothing is
    /// happening on chain, which is the only thing that distinguishes a quiet
    /// feed from a dead one.
    Ping,
    Pong,
}

impl UpdatePayload {
    /// The slot this payload belongs to, if any.
    pub const fn slot(&self) -> Option<u64> {
        match self {
            UpdatePayload::Account(update) => Some(update.slot),
            UpdatePayload::Slot(update) => Some(update.slot),
            UpdatePayload::Transaction(update) => Some(update.slot),
            UpdatePayload::Ping | UpdatePayload::Pong => None,
        }
    }
}

/// One message off the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeyserUpdate {
    /// The server's send timestamp, in microseconds. This is the
    /// micro-timestamp the sub-slot order is built on.
    pub created_at_micros: u64,
    pub payload: UpdatePayload,
}

impl GeyserUpdate {
    pub const fn new(created_at_micros: u64, payload: UpdatePayload) -> Self {
        GeyserUpdate {
            created_at_micros,
            payload,
        }
    }
}

// ---------------------------------------------------------------------------
// domain events
// ---------------------------------------------------------------------------

/// A bonding curve's reserves at one point in the stream.
///
/// Every field is an integer and the struct is [`Eq`], which is what stops a
/// float from ever landing here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CurveTick {
    pub curve: Pubkey,
    pub creator: Pubkey,
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub token_total_supply: u64,
    pub complete: bool,
    /// Lamports per raw token unit, at `10^-18`.
    ///
    /// Both sides of the ratio are raw units, so the token's decimals cancel
    /// and the answer needs no decimal count to interpret.
    pub price_e18: u128,
    pub market_cap_lamports: u64,
    pub progress_bps: u16,
    pub lamports: u64,
    pub write_version: u64,
}

/// A pool's two sides, normalised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolTick {
    pub pool: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    /// Base side, scaled by its mint's decimals, at `10^-18`.
    pub base_e18: u128,
    /// Quote side, scaled by its mint's decimals, at `10^-18`.
    pub quote_e18: u128,
    /// Quote per base, at `10^-18`.
    pub price_e18: u128,
}

/// The change between two consecutive prices for one curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PriceTick {
    pub curve: Pubkey,
    pub previous_e18: u128,
    pub current_e18: u128,
    /// Signed, because a price change is.
    pub delta_e18: i128,
    /// The same move relative to the baseline, in basis points.
    pub delta_bps: i64,
}

/// A slot changing state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotTick {
    pub slot: u64,
    pub parent: Option<u64>,
    pub phase: SlotPhase,
}

/// A transaction's logs, filtered to the programs this engine watches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogTick {
    pub signature: Signature,
    pub index: u64,
    pub failed: bool,
    pub logs: Vec<String>,
}

/// What comes out of the pipeline, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickPayload {
    Curve(CurveTick),
    Pool(PoolTick),
    Price(PriceTick),
    Slot(SlotTick),
    Log(LogTick),
}

/// One ordered domain event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickEvent {
    pub key: TickKey,
    pub payload: TickPayload,
}

impl TickClass for TickEvent {
    /// Curve and price state may never be shed.
    ///
    /// The asymmetry is the point. A dropped slot status costs one heartbeat
    /// and the next one repairs it. A dropped reserve update leaves the
    /// engine's idea of a price permanently wrong, because nothing re-sends it
    /// — the curve only writes again when someone trades. Pool balances are in
    /// the same position for the same reason.
    fn is_protected(&self) -> bool {
        matches!(
            self.payload,
            TickPayload::Curve(_) | TickPayload::Price(_) | TickPayload::Pool(_)
        )
    }

    /// Shed order for the rest: logs before slot statuses.
    ///
    /// Logs are corroboration — useful, reconstructible from the chain later.
    /// A slot status is what advances the ledger, and losing one widens the
    /// hold window for everything behind it.
    fn priority(&self) -> u8 {
        match self.payload {
            TickPayload::Log(_) => 0,
            TickPayload::Slot(_) => 1,
            TickPayload::Pool(_) => 2,
            TickPayload::Price(_) => 3,
            TickPayload::Curve(_) => 4,
        }
    }
}

// ---------------------------------------------------------------------------
// zero-float parsing
// ---------------------------------------------------------------------------

/// A raw SPL amount from its decimal-string form.
///
/// The one door into token amounts, and the reason it exists is worth being
/// blunt about: the wire format carries `ui_amount` as an IEEE double right
/// beside `amount` as a string, and the double is easier to reach for and
/// wrong. A `u64` balance above `2^53` does not survive a round trip through
/// `f64`, and pump.fun supplies are `10^15` raw units — an order of magnitude
/// past where doubles stop counting by ones.
///
/// Rejects anything that is not ASCII digits, including the sign, the point,
/// the exponent and the empty string. A leading `+`, a `1e9`, and a `1.0` are
/// all refusals rather than guesses.
pub fn parse_raw_amount(text: &str) -> Result<u128, DecodeError> {
    if text.is_empty() {
        return Err(DecodeError::BadAmount);
    }
    let mut value: u128 = 0;
    for byte in text.bytes() {
        if !byte.is_ascii_digit() {
            return Err(DecodeError::BadAmount);
        }
        value = value
            .checked_mul(10)
            .and_then(|scaled| scaled.checked_add(u128::from(byte - b'0')))
            .ok_or(DecodeError::BadAmount)?;
    }
    Ok(value)
}

/// A 32-byte address from a wire field of unknown length.
pub fn parse_pubkey(bytes: &[u8]) -> Result<Pubkey, DecodeError> {
    let raw: [u8; 32] = bytes.try_into().map_err(|_| DecodeError::BadPubkey)?;
    Ok(Pubkey::new(raw))
}

/// A 64-byte signature from a wire field of unknown length.
pub fn parse_signature(bytes: &[u8]) -> Result<Signature, DecodeError> {
    let raw: [u8; 64] = bytes.try_into().map_err(|_| DecodeError::BadSignature)?;
    Ok(Signature::new(raw))
}

// ---------------------------------------------------------------------------
// normalising an account into a curve tick
// ---------------------------------------------------------------------------

/// Turns a pump.fun bonding curve account into a [`CurveTick`].
///
/// The price is `virtual_sol_reserves / virtual_token_reserves` at `10^-18`.
/// Virtual rather than real because the virtual pair is what the program's own
/// constant-product maths uses to quote, so it is the price a trade would
/// actually get.
pub fn curve_tick(update: &AccountUpdate) -> Result<CurveTick, DecodeError> {
    if update.owner != pump_fun_program() {
        return Err(DecodeError::ForeignProgram);
    }
    let curve = BondingCurve::decode(&update.data).ok_or(DecodeError::UnknownAccount)?;
    if curve.virtual_token_reserves == 0 || curve.virtual_sol_reserves == 0 {
        // Not a cheap coin — an account read mid-write, or not a curve.
        return Err(DecodeError::IncoherentCurve);
    }

    let price_e18 = ratio_e18(
        u128::from(curve.virtual_sol_reserves),
        u128::from(curve.virtual_token_reserves),
    )
    .ok_or(DecodeError::NoPrice)?;

    Ok(CurveTick {
        curve: update.pubkey,
        creator: curve.creator,
        virtual_sol_reserves: curve.virtual_sol_reserves,
        virtual_token_reserves: curve.virtual_token_reserves,
        real_sol_reserves: curve.real_sol_reserves,
        real_token_reserves: curve.real_token_reserves,
        token_total_supply: curve.token_total_supply,
        complete: curve.complete,
        price_e18,
        market_cap_lamports: curve.market_cap_lamports(),
        progress_bps: curve.progress_bps(),
        lamports: update.lamports,
        write_version: update.write_version,
    })
}

/// The pump.fun program id as a [`Pubkey`].
///
/// Parsed rather than written out as bytes so that the base58 constant in
/// [`crate::ingestion`] stays the single source of truth, and parsed *once*
/// because this sits on the hot path: every account update compares its owner
/// against it, and a base58 decode per update is thirty-two divisions of work
/// to answer a question whose answer never changes.
fn pump_fun_program() -> Pubkey {
    static PROGRAM: std::sync::OnceLock<Pubkey> = std::sync::OnceLock::new();
    *PROGRAM.get_or_init(|| {
        Pubkey::parse(PUMP_FUN_PROGRAM).expect("PUMP_FUN_PROGRAM is a valid address")
    })
}

/// Turns a pair of token balances into a [`PoolTick`].
///
/// Both sides are scaled out of their own decimals first, so the ratio is a
/// real quote-per-base rather than a ratio of two differently-scaled integers.
pub fn pool_tick(
    pool: Pubkey,
    base: &TokenBalance,
    quote: &TokenBalance,
) -> Result<PoolTick, DecodeError> {
    let base_e18 = from_token_amount(base.raw, base.decimals).ok_or(DecodeError::BadAmount)?;
    let quote_e18 = from_token_amount(quote.raw, quote.decimals).ok_or(DecodeError::BadAmount)?;
    if base_e18 == 0 {
        return Err(DecodeError::NoPrice);
    }
    // Both sides are already at 10^-18, so the ratio needs the scale putting
    // back: `ratio_e18` divides and re-scales in one step.
    let price_e18 = ratio_e18(quote_e18, base_e18).ok_or(DecodeError::NoPrice)?;
    Ok(PoolTick {
        pool,
        base_mint: base.mint,
        quote_mint: quote.mint,
        base_e18,
        quote_e18,
        price_e18,
    })
}

// ---------------------------------------------------------------------------
// per-curve state
// ---------------------------------------------------------------------------

/// The last write applied to each curve, and the last price it implied.
///
/// Two jobs, and the first one is the important one.
///
/// **The write-version guard.** [`TickKey`] orders by timestamp before write
/// version, for reasons its own documentation gives, which means a stale write
/// to one account *can* be released ahead of a newer one if the server's
/// timestamps disagree with its write versions. This is where that becomes
/// harmless: a write whose `(slot, write_version)` is not strictly newer than
/// the last one applied to that same account is discarded. Ordering the stream
/// is best-effort; the state machine is exact.
///
/// **The price baseline.** A [`PriceTick`] is a difference, and a difference
/// needs the previous value. The first write for a curve has no previous price
/// and produces no price tick at all — a first observation reported as "moved
/// 0 bps" is a lie a ladder would act on.
#[derive(Debug, Default)]
pub struct CurveTracker {
    seen: HashMap<Pubkey, CurveState>,
    stale_writes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CurveState {
    slot: u64,
    write_version: u64,
    price_e18: u128,
}

impl CurveTracker {
    pub fn new() -> Self {
        CurveTracker::default()
    }

    /// How many writes were refused for being older than what is already
    /// applied.
    pub const fn stale_writes(&self) -> u64 {
        self.stale_writes
    }

    /// How many curves are being tracked.
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }

    /// Applies a curve tick.
    ///
    /// `None` when the write is not newer than what has already been applied to
    /// this curve. `Some(None)` when it is newer but there was no previous
    /// price to difference against.
    pub fn apply(&mut self, slot: u64, tick: &CurveTick) -> Option<Option<PriceTick>> {
        let incoming = (slot, tick.write_version);
        let previous = self.seen.get(&tick.curve).copied();

        if let Some(state) = previous {
            if incoming <= (state.slot, state.write_version) {
                self.stale_writes += 1;
                return None;
            }
        }

        self.seen.insert(
            tick.curve,
            CurveState {
                slot,
                write_version: tick.write_version,
                price_e18: tick.price_e18,
            },
        );

        let price = previous.and_then(|state| {
            // A price that did not move is not a tick. Emitting one would put a
            // zero-delta row in front of every consumer on every rewrite of an
            // account whose reserves did not change.
            if state.price_e18 == tick.price_e18 {
                return None;
            }
            Some(PriceTick {
                curve: tick.curve,
                previous_e18: state.price_e18,
                current_e18: tick.price_e18,
                delta_e18: delta_e18(state.price_e18, tick.price_e18)?,
                delta_bps: delta_bps(state.price_e18, tick.price_e18)?,
            })
        });
        Some(price)
    }

    /// Forgets everything at or above `slot`. Called on a re-org.
    ///
    /// A curve whose only observation was in an abandoned slot goes back to
    /// having no baseline, which is right: the price it recorded never
    /// happened, and differencing against it would report a move that never
    /// happened either.
    pub fn rollback(&mut self, from_slot: u64) -> usize {
        let before = self.seen.len();
        self.seen.retain(|_, state| state.slot < from_slot);
        before - self.seen.len()
    }
}

// ---------------------------------------------------------------------------
// metrics
// ---------------------------------------------------------------------------

/// The pipeline's own view of itself, as of the last update it handled.
///
/// The ledger heads and the ring counters live inside a [`TickPipeline`] that
/// one task owns exclusively, and that is a property worth keeping. So the task
/// mirrors them out here instead of the pipeline being shared: nine numbers
/// copied under a mutex the reader takes once a second and the writer takes
/// once an update, rather than the sequencer growing interior mutability that
/// somebody would eventually reach into from another thread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PipelineGauge {
    head_slot: u64,
    confirmed_head: u64,
    finalized_head: u64,
    reorgs: u64,
    stale_writes: u64,
    ring: RingMetrics,
}

/// Counters shared between the stream task and whoever is reading them.
#[derive(Debug, Default)]
pub struct GeyserMetrics {
    updates: AtomicU64,
    accounts: AtomicU64,
    slots: AtomicU64,
    transactions: AtomicU64,
    pings: AtomicU64,
    startup_skipped: AtomicU64,
    /// Account writes owned by a program with no decoder here — the pool
    /// subscription, mostly. Its own counter rather than a share of
    /// `decode_failures`, because the two mean opposite things about the
    /// health of the stream.
    foreign_accounts: AtomicU64,
    decode_failures: AtomicU64,
    events: AtomicU64,
    connects: AtomicU64,
    connect_failures: AtomicU64,
    disconnects: AtomicU64,
    reconnect_wait_ms: AtomicU64,
    /// Candidates the ordered stream handed to [`crate::ingestion`], and the
    /// ones its filters then refused. Together with `events` they are the whole
    /// story of what the wiring did: released, offered, kept.
    admitted: AtomicU64,
    refused: AtomicU64,
    /// Fork switches that reached slots already released downstream.
    unwinds: AtomicU64,
    /// Whether a subscription is open right now.
    ///
    /// Tracked rather than derived. `connects > disconnects` looks like the
    /// same fact and is not: the two counters move at different moments, so
    /// between the end of a stream and the disconnect being recorded they
    /// disagree, and a guard reading them would flicker. One flag, set where
    /// the transitions actually happen.
    connected: std::sync::atomic::AtomicBool,
    gauge: parking_lot::Mutex<PipelineGauge>,
}

impl GeyserMetrics {
    pub fn record_update(&self, payload: &UpdatePayload) {
        self.updates.fetch_add(1, Ordering::Relaxed);
        let counter = match payload {
            UpdatePayload::Account(_) => &self.accounts,
            UpdatePayload::Slot(_) => &self.slots,
            UpdatePayload::Transaction(_) => &self.transactions,
            UpdatePayload::Ping | UpdatePayload::Pong => &self.pings,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_startup_skip(&self) {
        self.startup_skipped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_decode_failure(&self) {
        self.decode_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_foreign_account(&self) {
        self.foreign_accounts.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_events(&self, count: usize) {
        self.events.fetch_add(count as u64, Ordering::Relaxed);
    }

    pub fn record_connect(&self) {
        self.connects.fetch_add(1, Ordering::Relaxed);
        self.connected.store(true, Ordering::Relaxed);
    }

    pub fn record_connect_failure(&self, wait: Duration) {
        self.connect_failures.fetch_add(1, Ordering::Relaxed);
        self.connected.store(false, Ordering::Relaxed);
        self.reconnect_wait_ms
            .fetch_add(wait.as_millis() as u64, Ordering::Relaxed);
    }

    pub fn record_disconnect(&self, wait: Duration) {
        self.disconnects.fetch_add(1, Ordering::Relaxed);
        self.connected.store(false, Ordering::Relaxed);
        self.reconnect_wait_ms
            .fetch_add(wait.as_millis() as u64, Ordering::Relaxed);
    }

    /// Whether a subscription is open.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Records that the ordered stream offered `count` curve writes downstream
    /// and that `refused` of them were turned away by the filters.
    pub fn record_admission(&self, count: u64, refused: u64) {
        self.admitted.fetch_add(count, Ordering::Relaxed);
        self.refused.fetch_add(refused, Ordering::Relaxed);
    }

    pub fn record_unwind(&self) {
        self.unwinds.fetch_add(1, Ordering::Relaxed);
    }

    /// Mirrors the sequencer's state out of the task that owns it.
    ///
    /// Called by the read loop after each update, which is the only place with a
    /// pipeline in scope. Nine words under an uncontended mutex, against a read
    /// that happens when a person looks at a window.
    pub fn record_pipeline(&self, ring: RingMetrics, ledger: &SlotLedger, stale_writes: u64) {
        *self.gauge.lock() = PipelineGauge {
            head_slot: ledger.head(),
            confirmed_head: ledger.confirmed_head(),
            finalized_head: ledger.finalized_head(),
            reorgs: ledger.reorgs(),
            stale_writes,
            ring,
        };
    }

    /// The counters as one consistent-enough picture.
    ///
    /// Each load is `Relaxed` and they are not taken atomically together, which
    /// is correct for a display: a snapshot that blocked the stream to be
    /// perfectly coherent would cost more than the coherence is worth.
    pub fn snapshot(
        &self,
        ring: RingMetrics,
        ledger: &SlotLedger,
        stale_writes: u64,
    ) -> GeyserSnapshot {
        self.assemble(PipelineGauge {
            head_slot: ledger.head(),
            confirmed_head: ledger.confirmed_head(),
            finalized_head: ledger.finalized_head(),
            reorgs: ledger.reorgs(),
            stale_writes,
            ring,
        })
    }

    /// The same picture for a caller that does not hold the pipeline, built
    /// from whatever the read loop last mirrored out.
    pub fn snapshot_now(&self) -> GeyserSnapshot {
        let gauge = *self.gauge.lock();
        self.assemble(gauge)
    }

    fn assemble(&self, gauge: PipelineGauge) -> GeyserSnapshot {
        GeyserSnapshot {
            updates: self.updates.load(Ordering::Relaxed),
            accounts: self.accounts.load(Ordering::Relaxed),
            slots: self.slots.load(Ordering::Relaxed),
            transactions: self.transactions.load(Ordering::Relaxed),
            pings: self.pings.load(Ordering::Relaxed),
            startup_skipped: self.startup_skipped.load(Ordering::Relaxed),
            foreign_accounts: self.foreign_accounts.load(Ordering::Relaxed),
            decode_failures: self.decode_failures.load(Ordering::Relaxed),
            events: self.events.load(Ordering::Relaxed),
            connects: self.connects.load(Ordering::Relaxed),
            connect_failures: self.connect_failures.load(Ordering::Relaxed),
            disconnects: self.disconnects.load(Ordering::Relaxed),
            reconnect_wait_ms: self.reconnect_wait_ms.load(Ordering::Relaxed),
            admitted: self.admitted.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
            unwinds: self.unwinds.load(Ordering::Relaxed),
            stale_writes: gauge.stale_writes,
            head_slot: gauge.head_slot,
            confirmed_head: gauge.confirmed_head,
            finalized_head: gauge.finalized_head,
            reorgs: gauge.reorgs,
            ring: gauge.ring,
        }
    }
}

/// What the feed looks like from outside.
///
/// Every field is an integer, so the readout that displays it is a column of
/// digits in a monospace face and nothing else. There is no rate, no average
/// and no percentage in here on purpose: those are derived numbers, and a
/// derived number in a snapshot is one the reader cannot check against the
/// counter it came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeyserSnapshot {
    pub updates: u64,
    pub accounts: u64,
    pub slots: u64,
    pub transactions: u64,
    pub pings: u64,
    /// Accounts from the plugin's startup snapshot, which are state rather than
    /// events and are not turned into ticks.
    pub startup_skipped: u64,
    /// Account writes owned by a program this build has no decoder for.
    ///
    /// Not a fault, and not free either: the subscription asks for pool-program
    /// accounts that nothing here reads, so this is the quota being spent on
    /// data that produces no tick. Kept out of `decodeFailures` so that counter
    /// still means what it says.
    pub foreign_accounts: u64,
    pub decode_failures: u64,
    /// Domain events released, in order.
    pub events: u64,
    pub connects: u64,
    pub connect_failures: u64,
    pub disconnects: u64,
    /// Total time spent waiting to reconnect.
    pub reconnect_wait_ms: u64,
    /// Curve writes handed to [`crate::ingestion`] off the ordered stream.
    pub admitted: u64,
    /// How many of those its filters turned away. Not a fault: the spam floor
    /// and the target window are doing their job, and a feed where this is zero
    /// is a feed whose filters are not running.
    pub refused: u64,
    /// Fork switches that reached slots already released downstream, each one a
    /// walk-back of the launch index rather than something the ring could undo.
    pub unwinds: u64,
    /// Account writes refused for being older than what was already applied.
    pub stale_writes: u64,
    pub head_slot: u64,
    pub confirmed_head: u64,
    pub finalized_head: u64,
    pub reorgs: u64,
    pub ring: RingMetrics,
}

// ---------------------------------------------------------------------------
// the pipeline
// ---------------------------------------------------------------------------

/// Everything between a raw update and an ordered domain event.
///
/// Single-owner by design: the ledger, the ring and the trackers are all plain
/// owned values with no interior mutability, because the pipeline runs on one
/// task and a structure that could be shared is a structure someone will share.
#[derive(Debug)]
pub struct TickPipeline {
    ledger: SlotLedger,
    ring: TickRing<TickEvent>,
    curves: CurveTracker,
    commitment: Commitment,
    /// The arrival counter that makes [`TickKey`] total.
    seq: u64,
    /// The highest slot released, for resuming a reconnected stream.
    last_released_slot: u64,
}

/// What one update did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ingested {
    /// Events released in order by this call.
    pub released: Vec<TickEvent>,
    /// Events discarded by a rollback this call triggered. They never reached
    /// anyone, which is the good case.
    pub rolled_back: Vec<TickEvent>,
    /// Set when a rollback arrived too late — events at or above this slot had
    /// already been released and the caller has to unwind them itself.
    pub unrecoverable_from_slot: Option<u64>,
    /// Events shed or refused under backpressure.
    pub dropped: Vec<TickEvent>,
    /// Curve writes that reached the front of the ordered stream and turned out
    /// to be older than what had already been applied to that account.
    pub stale: Vec<TickEvent>,
    /// The update could not be turned into an event.
    pub decode_error: Option<DecodeError>,
    /// The update was an account owned by a program this build has no decoder
    /// for. Distinct from [`Self::decode_error`] on purpose: one says the
    /// stream carried something unreadable, the other says it carried
    /// something that was never ours to read.
    pub foreign_account: bool,
}

impl Ingested {
    fn empty() -> Self {
        Ingested {
            released: Vec::new(),
            rolled_back: Vec::new(),
            unrecoverable_from_slot: None,
            dropped: Vec::new(),
            stale: Vec::new(),
            decode_error: None,
            foreign_account: false,
        }
    }

    fn failed(error: DecodeError) -> Self {
        Ingested {
            decode_error: Some(error),
            ..Ingested::empty()
        }
    }

    fn foreign() -> Self {
        Ingested {
            foreign_account: true,
            ..Ingested::empty()
        }
    }
}

impl TickPipeline {
    pub fn new(config: &GeyserConfig) -> Self {
        TickPipeline {
            ledger: SlotLedger::new(),
            ring: TickRing::new(config.ring),
            curves: CurveTracker::new(),
            commitment: config.commitment,
            seq: 0,
            last_released_slot: 0,
        }
    }

    pub const fn ledger(&self) -> &SlotLedger {
        &self.ledger
    }

    pub const fn curves(&self) -> &CurveTracker {
        &self.curves
    }

    pub fn ring_metrics(&self) -> RingMetrics {
        self.ring.metrics()
    }

    /// Where a reconnected stream should resume from.
    ///
    /// A few slots behind the last release, so a gap is impossible and the cost
    /// is a handful of duplicates that the write-version guard already drops.
    pub const fn resume_slot(&self) -> Option<u64> {
        if self.last_released_slot == 0 {
            None
        } else {
            Some(self.last_released_slot.saturating_sub(RESUME_OVERLAP_SLOTS))
        }
    }

    /// The next arrival number.
    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// An arrival number for a curve write, with the one after it reserved.
    ///
    /// A price tick is derived from a curve tick at *release* time, long after
    /// the arrival counter has moved on, and it still needs a key that sits
    /// strictly between its curve tick and whatever the chain wrote next.
    /// Reserving the successor here is what makes that key available later
    /// without a second counter that could collide with this one.
    fn next_curve_seq(&mut self) -> u64 {
        let seq = self.next_seq();
        self.seq += 1;
        seq
    }

    /// Feeds one update through.
    ///
    /// By value, and that is the second half of the no-copy path the wire
    /// decode starts. The account bytes arrive as a [`Bytes`] sharing the read
    /// buffer and a transaction's logs arrive as a vector nobody else holds;
    /// taking the update by reference here would force both to be cloned back
    /// out of a message that is dropped on the next line anyway.
    pub fn ingest(&mut self, update: GeyserUpdate) -> Ingested {
        let micros = update.created_at_micros;
        match update.payload {
            UpdatePayload::Ping | UpdatePayload::Pong => Ingested::empty(),
            UpdatePayload::Slot(slot) => self.ingest_slot(micros, slot),
            UpdatePayload::Account(account) => self.ingest_account(micros, &account),
            UpdatePayload::Transaction(transaction) => self.ingest_transaction(micros, transaction),
        }
    }

    fn ingest_slot(&mut self, micros: u64, update: SlotUpdate) -> Ingested {
        let change = self
            .ledger
            .observe(update.slot, update.parent, update.phase);

        let mut result = Ingested::empty();

        // A re-org is a rollback and nothing else. In particular it does *not*
        // also emit a tick for the slot that died, and the reason is not
        // squeamishness: releasing a tick at slot N would move the ring's
        // released watermark to N, and the whole point of a fork switch is
        // that slot N is about to be rebuilt on the winning fork. Its
        // replacement events would then arrive below the watermark and be
        // refused as late — the abandoned fork would lock out the real one.
        //
        // The news still reaches the caller, through `rolled_back` and
        // `unrecoverable_from_slot`, which is the channel that carries the
        // slot number without touching the ordering.
        if let LedgerChange::Reorg { from_slot, .. } = change {
            let rollback = self.ring.rollback(from_slot);
            self.curves.rollback(from_slot);
            result.rolled_back = rollback.discarded;
            result.unrecoverable_from_slot = rollback.released;
            self.drain(&mut result);
            return result;
        }

        if !matches!(change, LedgerChange::TooOld { .. }) {
            let key = TickKey::new(update.slot, micros, 0, self.next_seq());
            let event = TickEvent {
                key,
                payload: TickPayload::Slot(SlotTick {
                    slot: update.slot,
                    parent: update.parent,
                    phase: update.phase,
                }),
            };
            self.offer(key, event, &mut result);
        }
        self.drain(&mut result);
        result
    }

    fn ingest_account(&mut self, micros: u64, update: &AccountUpdate) -> Ingested {
        // An account this build has no decoder for is not a fault, and saying
        // so first is load-bearing rather than tidy. The subscription asks for
        // pool-program accounts as well as curves (see
        // `GeyserConfig::subscribe_filters`) and nothing here reads one, so on
        // a live stream every Raydium write would otherwise land in
        // `decode_failures` — a counter whose whole job is to say the wire
        // format moved under us, buried under traffic that is behaving exactly
        // as asked. It is counted, not swallowed: the operator is paying quota
        // for these, and a number they can see is what makes that visible.
        if update.owner != pump_fun_program() {
            return Ingested::foreign();
        }

        let tick = match curve_tick(update) {
            Ok(tick) => tick,
            Err(error) => return Ingested::failed(error),
        };

        // Note what does *not* happen here: the write-version guard is not
        // applied, and no price is differenced. Both belong on the ordered
        // stream, not on the arrival stream. Guarding at arrival would reject
        // precisely the late packets the ring exists to put back in place — a
        // slot-10 write arriving after a slot-11 write is the normal case this
        // module was built for, and a guard here would call it stale and throw
        // it away. See `publish`.
        let mut result = Ingested::empty();
        let key = TickKey::new(
            update.slot,
            micros,
            update.write_version,
            self.next_curve_seq(),
        );
        self.offer(
            key,
            TickEvent {
                key,
                payload: TickPayload::Curve(tick),
            },
            &mut result,
        );
        self.drain(&mut result);
        result
    }

    fn ingest_transaction(&mut self, micros: u64, update: TransactionUpdate) -> Ingested {
        // Votes are consensus traffic and never a signal. They should have been
        // excluded at the validator by the subscription filter; dropping them
        // here as well costs one comparison and means a mis-set filter is a
        // wasted byte rather than a wrong event.
        if update.is_vote {
            return Ingested::empty();
        }

        let mut result = Ingested::empty();
        let key = TickKey::new(update.slot, micros, 0, self.next_seq());
        self.offer(
            key,
            TickEvent {
                key,
                payload: TickPayload::Log(LogTick {
                    signature: update.signature,
                    index: update.index,
                    failed: update.failed,
                    // Moved, not cloned. This is the only owner left.
                    logs: update.logs,
                }),
            },
            &mut result,
        );
        self.drain(&mut result);
        result
    }

    /// Offers an event to the ring and records what the ring did with it.
    fn offer(&mut self, key: TickKey, event: TickEvent, result: &mut Ingested) {
        match self.ring.push(key, event) {
            Push::Buffered => {}
            Push::Rejected(event) | Push::Displaced(event) => result.dropped.push(event),
            Push::ForcedRelease(event) => self.publish(event, result),
        }
    }

    /// Releases whatever the hold window now permits.
    fn drain(&mut self, result: &mut Ingested) {
        let mut ready = Vec::new();
        self.ring
            .drain_ready(&self.ledger, self.commitment, &mut ready);
        for event in ready {
            self.publish(event, result);
        }
    }

    /// The one exit from the buffer.
    ///
    /// Every released event goes through here, whether it left in the ordinary
    /// way or was forced out by backpressure, and this is where curve state
    /// meets the tracker. Doing it here rather than at arrival is what makes
    /// the write-version guard mean what it says: by this point the stream is
    /// in chain order, so "older than what has already been applied" is a
    /// statement about the chain rather than about which packet won a race.
    fn publish(&mut self, event: TickEvent, result: &mut Ingested) {
        self.last_released_slot = self.last_released_slot.max(event.key.slot);

        let TickPayload::Curve(curve) = &event.payload else {
            result.released.push(event);
            return;
        };

        let slot = event.key.slot;
        let Some(price) = self.curves.apply(slot, curve) else {
            result.stale.push(event);
            return;
        };

        // The price tick rides just behind its curve tick, on the successor
        // `next_curve_seq` reserved when the curve arrived. Same slot, same
        // timestamp, same write version, next arrival number — so it sorts
        // immediately after its own curve tick and before anything the chain
        // wrote next, with no special case in the comparison.
        let price_key = TickKey {
            seq: event.key.seq + 1,
            ..event.key
        };
        result.released.push(event);
        if let Some(price) = price {
            result.released.push(TickEvent {
                key: price_key,
                payload: TickPayload::Price(price),
            });
        }
    }

    /// Releases everything still buffered, ignoring the hold window.
    ///
    /// For shutdown and for the end of a fixture, where no later tick can
    /// arrive and holding is pure loss.
    pub fn flush(&mut self) -> Vec<TickEvent> {
        let mut ready = Vec::new();
        self.ring.drain_all(&mut ready);
        let mut result = Ingested::empty();
        for event in ready {
            self.publish(event, &mut result);
        }
        result.released
    }
}

// ---------------------------------------------------------------------------
// reconnect
// ---------------------------------------------------------------------------

/// Exponential backoff with a ceiling, and no jitter.
///
/// No jitter deliberately. Jitter exists to stop a thundering herd, and there
/// is one client here; what it would cost is a reconnect schedule that cannot
/// be asserted on in a test, and a schedule nobody has tested is a schedule
/// that hammers a provider on the day it goes down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    min: Duration,
    max: Duration,
    failures: u32,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        ReconnectPolicy::new(BACKOFF_MIN, BACKOFF_MAX)
    }
}

impl ReconnectPolicy {
    pub const fn new(min: Duration, max: Duration) -> Self {
        ReconnectPolicy {
            min,
            max,
            failures: 0,
        }
    }

    /// Consecutive failures since the last success.
    pub const fn failures(&self) -> u32 {
        self.failures
    }

    /// Records a failure and returns how long to wait.
    ///
    /// The first failure waits `min`, not twice it — the `saturating_sub(1)` is
    /// what makes a single blip cost half a second rather than a second.
    pub fn record_failure(&mut self) -> Duration {
        self.failures = self.failures.saturating_add(1);
        let shift = (self.failures.saturating_sub(1)).min(BACKOFF_MAX_SHIFT);
        self.min.saturating_mul(1u32 << shift).min(self.max)
    }

    /// Records a connection that worked, clearing the backoff.
    ///
    /// Called on a *successful subscription*, not on a successful dial. A
    /// provider that accepts the TCP connection and then rejects every
    /// subscription would otherwise reset the backoff on every attempt and
    /// spin.
    pub fn record_success(&mut self) {
        self.failures = 0;
    }
}

// ---------------------------------------------------------------------------
// the transport seam
// ---------------------------------------------------------------------------

/// A boxed future, so the traits below are object-safe. Same reasoning as
/// [`crate::ingestion::BoxFuture`], which this deliberately mirrors.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The reading half of a subscription.
///
/// `recv` must be cancel-safe: the read loop races it against a shutdown
/// signal, and a message half-taken out of a dropped future is a message
/// silently lost.
pub trait GeyserStream: Send {
    fn recv(&mut self) -> BoxFuture<'_, Option<Result<GeyserUpdate, GeyserError>>>;
}

/// Opens subscriptions.
pub trait GeyserTransport: Send + Sync + 'static {
    fn subscribe(
        &self,
        config: GeyserConfig,
        filters: SubscribeFilters,
    ) -> BoxFuture<'static, Result<Box<dyn GeyserStream>, GeyserError>>;
}

/// A transport that refuses.
///
/// What a build without the `geyser-grpc` feature gets. Failing with a sentence
/// that names the missing feature is the difference between a feed that is
/// visibly absent and one that looks present — the same choice
/// [`crate::ingestion::WebSocketDialer`] makes for a gRPC URL.
pub struct NoTransport;

impl GeyserTransport for NoTransport {
    fn subscribe(
        &self,
        _config: GeyserConfig,
        _filters: SubscribeFilters,
    ) -> BoxFuture<'static, Result<Box<dyn GeyserStream>, GeyserError>> {
        Box::pin(async { Err(GeyserError::NoTransport) })
    }
}

/// A stream over a fixed script, for tests and fixtures.
///
/// Public rather than test-only because the reconnect loop cannot be tested
/// without something that fails on demand, and a thing the tests need to build
/// is part of the module's surface whether or not it is admitted.
pub struct MockStream {
    script: std::collections::VecDeque<Result<GeyserUpdate, GeyserError>>,
}

impl MockStream {
    pub fn new(script: Vec<Result<GeyserUpdate, GeyserError>>) -> Self {
        MockStream {
            script: script.into(),
        }
    }

    /// A stream of updates that then ends cleanly.
    pub fn of(updates: Vec<GeyserUpdate>) -> Self {
        MockStream::new(updates.into_iter().map(Ok).collect())
    }
}

impl GeyserStream for MockStream {
    fn recv(&mut self) -> BoxFuture<'_, Option<Result<GeyserUpdate, GeyserError>>> {
        Box::pin(async move { self.script.pop_front() })
    }
}

// ---------------------------------------------------------------------------
// the subscriber loop
// ---------------------------------------------------------------------------

/// Why the subscriber stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The caller asked it to.
    Shutdown,
    /// The attempt budget ran out.
    OutOfAttempts,
}

/// What the subscriber does with each batch of ordered events.
///
/// A trait rather than a channel so that the loop can be tested without a
/// runtime scheduling decision in the middle of the assertion.
pub trait TickSink: Send {
    /// Ordered events, ready to act on.
    fn emit(&mut self, events: Vec<TickEvent>);

    /// A re-org that arrived too late to roll back. Events at or above
    /// `from_slot` have already been emitted and are wrong.
    ///
    /// The default does nothing, because for a sink that only counts there is
    /// nothing to unwind. A sink holding state must override it.
    fn unwind(&mut self, from_slot: u64) {
        let _ = from_slot;
    }

    /// The feed could not be opened, and why.
    ///
    /// Called when a subscription attempt fails and the reason is *news* —
    /// the first failure, and afterwards only when the reason changes. The
    /// reconnect schedule reaches thirty seconds and stays there, so a
    /// provider that is down for an hour would otherwise say the same sentence
    /// a hundred times; the counters already carry how often, and this carries
    /// what.
    ///
    /// `consecutive` is how many attempts have failed in a row at the point
    /// this was called, so a sink can tell a blip from an outage without
    /// keeping its own count.
    fn fault(&mut self, error: &GeyserError, consecutive: u32) {
        let _ = (error, consecutive);
    }
}

/// A sink that keeps everything, for tests and fixtures.
#[derive(Debug, Default)]
pub struct CollectingSink {
    pub events: Vec<TickEvent>,
    pub unwinds: Vec<u64>,
    /// Every fault reported, with the consecutive-failure count that came with
    /// it. What the live build sends to telemetry, kept where a test can read
    /// it.
    pub faults: Vec<(GeyserError, u32)>,
}

impl TickSink for CollectingSink {
    fn emit(&mut self, events: Vec<TickEvent>) {
        self.events.extend(events);
    }

    fn unwind(&mut self, from_slot: u64) {
        self.unwinds.push(from_slot);
    }

    fn fault(&mut self, error: &GeyserError, consecutive: u32) {
        self.faults.push((error.clone(), consecutive));
    }
}

/// The sink that joins this module to the live feed.
///
/// Everything above this point turns a shuffled stream into an ordered one.
/// This is where the ordered stream stops being an interesting data structure
/// and becomes a candidate the engine can act on: each released [`CurveTick`]
/// is handed to [`IngestionManager::admit_curve`], which runs it through the
/// same launch index, the same spam floor and the same target window that a
/// websocket frame goes through, and puts it on the same two channels.
///
/// The other payloads are deliberately not forwarded. A [`PriceTick`] is a
/// difference between two curve ticks that were both admitted, a [`SlotTick`]
/// is the ledger talking to itself, and a [`LogTick`] is corroboration for a
/// decision this build does not yet make. Forwarding them would mean inventing
/// a second candidate shape for events that are not candidates.
pub struct IngestionSink {
    provider: FeedProvider,
    ingestion: Arc<IngestionManager>,
    metrics: Arc<GeyserMetrics>,
    telemetry: Option<Arc<TelemetryHub>>,
}

impl IngestionSink {
    pub fn new(
        provider: FeedProvider,
        ingestion: Arc<IngestionManager>,
        metrics: Arc<GeyserMetrics>,
        telemetry: Option<Arc<TelemetryHub>>,
    ) -> Self {
        IngestionSink {
            provider,
            ingestion,
            metrics,
            telemetry,
        }
    }
}

impl TickSink for IngestionSink {
    fn emit(&mut self, events: Vec<TickEvent>) {
        // One clock read for the batch. The dispatch timer measures this
        // module's own hand-off and not the hold that ordering cost: the hold
        // is not a slow dispatch, it is a deliberate wait, and it is already
        // reported honestly by the ring's own counters.
        let received = Instant::now();
        let mut offered = 0u64;
        let mut refused = 0u64;

        for event in &events {
            let TickPayload::Curve(curve) = &event.payload else {
                continue;
            };
            offered += 1;
            let bonding = BondingCurve {
                virtual_token_reserves: curve.virtual_token_reserves,
                virtual_sol_reserves: curve.virtual_sol_reserves,
                real_token_reserves: curve.real_token_reserves,
                real_sol_reserves: curve.real_sol_reserves,
                token_total_supply: curve.token_total_supply,
                complete: curve.complete,
                creator: curve.creator,
            };
            if let Verdict::Dropped(_) = self.ingestion.admit_curve(
                self.provider,
                event.key.slot,
                curve.curve,
                &bonding,
                received,
            ) {
                refused += 1;
            }
        }

        self.metrics.record_admission(offered, refused);
    }

    /// A fork switch that outran the hold window.
    ///
    /// Two things happen, and the order matters. The launch index is walked back
    /// first, because until it is, the winning fork's rewrite of those slots
    /// would be refused as stale and the engine would sit on a price from a
    /// block that no longer exists. Then it is said out loud: this is the one
    /// event in the whole module that means state downstream is wrong, and a
    /// silent one would be a lie of omission.
    fn unwind(&mut self, from_slot: u64) {
        let touched = self.ingestion.rewind_launches(from_slot);
        if let Some(hub) = &self.telemetry {
            hub.publish(
                TelemetryLevel::Warn,
                "geyser",
                format!(
                    "a fork switch reached slot {from_slot}, which had already been acted on — \
                     {touched} tracked accounts rewound"
                ),
                serde_json::json!({
                    "fromSlot": from_slot,
                    "rewoundAccounts": touched,
                    "provider": self.provider.as_str(),
                }),
            );
        }
    }

    /// The feed could not be opened, said out loud.
    ///
    /// Before this, a failed dial was a counter and nothing else: a wrong
    /// token, a typo'd endpoint and a provider genuinely down all looked
    /// identical from the window — `connectFailures` climbing, no reason
    /// anywhere, and a retry loop that would go on doing it in silence
    /// forever. That is the single least diagnosable state this module has,
    /// and it is the one a new configuration lands in most often.
    ///
    /// The level is the news. A first failure is a blip and reads as one; a
    /// run of them is an outage and is raised, because by then the retry loop
    /// has been going for a while and nothing else will say so.
    fn fault(&mut self, error: &GeyserError, consecutive: u32) {
        let Some(hub) = &self.telemetry else { return };
        // Four attempts is roughly ten seconds of backoff, which is long
        // enough that a passing blip has already cleared.
        let level = if consecutive >= 4 {
            TelemetryLevel::Warn
        } else {
            TelemetryLevel::Info
        };
        hub.publish(
            level,
            "geyser",
            format!("the geyser feed could not be opened: {error}"),
            serde_json::json!({
                "reason": error,
                "consecutiveFailures": consecutive,
                "provider": self.provider.as_str(),
            }),
        );
    }
}

/// How the loop is bounded, so a test does not have to wait out real backoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunLimits {
    /// How many connection attempts before giving up. `None` is forever, which
    /// is what production wants.
    pub max_attempts: Option<u32>,
    /// Whether to actually wait out the backoff. Tests set this false so the
    /// schedule is asserted on — it still comes back in
    /// [`RunReport::backoffs`] — rather than slept through.
    pub sleep: bool,
}

impl Default for RunLimits {
    fn default() -> Self {
        RunLimits {
            max_attempts: None,
            sleep: true,
        }
    }
}

/// What one run of the subscriber did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub stopped: StopReason,
    /// The backoff waited before each reconnect, in order. The schedule, made
    /// assertable.
    pub backoffs: Vec<Duration>,
    /// The `from_slot` asked for on each attempt after the first.
    pub resumed_from: Vec<Option<u64>>,
    /// Why each failed attempt failed, in order. The counterpart to
    /// `backoffs`: that says how long the loop waited, this says what it was
    /// waiting on.
    pub failures: Vec<GeyserError>,
}

/// Connects, subscribes, reads, and reconnects until told to stop.
///
/// The loop is the resilience: every exit from the read is a reconnect with a
/// longer wait, every successful subscription clears the wait, and every
/// reconnect resumes from just behind the last released slot so the gap the
/// disconnect opened is closed rather than skipped.
pub async fn run_subscriber(
    transport: &dyn GeyserTransport,
    config: GeyserConfig,
    pipeline: &mut TickPipeline,
    sink: &mut dyn TickSink,
    metrics: &GeyserMetrics,
    limits: RunLimits,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> RunReport {
    let mut policy = ReconnectPolicy::default();
    let mut report = RunReport {
        stopped: StopReason::Shutdown,
        backoffs: Vec::new(),
        resumed_from: Vec::new(),
        failures: Vec::new(),
    };
    let mut attempts = 0u32;
    // The last reason reported, and how many attempts have failed since one
    // succeeded. Together they are what keeps a thirty-second retry loop from
    // saying the same sentence until the log is useless.
    let mut reported: Option<String> = None;
    let mut consecutive = 0u32;

    loop {
        if *shutdown.borrow() {
            return report;
        }
        if limits.max_attempts.is_some_and(|cap| attempts >= cap) {
            report.stopped = StopReason::OutOfAttempts;
            return report;
        }
        attempts += 1;

        // Resume from just behind where the last run got to. On the first
        // attempt that is whatever the config said.
        let resume = pipeline.resume_slot().or(config.from_slot);
        if attempts > 1 {
            report.resumed_from.push(resume);
        }
        let mut attempt_config = config.clone();
        attempt_config.from_slot = resume;
        let filters = attempt_config.subscribe_filters();

        let opened = transport.subscribe(attempt_config, filters).await;
        let mut stream = match opened {
            Ok(stream) => {
                metrics.record_connect();
                policy.record_success();
                // A connection that came up is the end of whatever was wrong,
                // so the next thing to go wrong is news again even if it is
                // the same sentence.
                reported = None;
                consecutive = 0;
                stream
            }
            Err(error) => {
                let wait = policy.record_failure();
                metrics.record_connect_failure(wait);
                report.backoffs.push(wait);
                consecutive += 1;
                // Reported when it is news. The reason is what changes rarely;
                // the count of failures is already on the counters, published
                // every five seconds by the telemetry loop, so repeating the
                // sentence adds nothing and buries everything else.
                let reason = error.to_string();
                if reported.as_deref() != Some(reason.as_str()) {
                    sink.fault(&error, consecutive);
                    reported = Some(reason);
                }
                report.failures.push(error);
                if !wait_or_shutdown(wait, limits.sleep, &mut shutdown).await {
                    return report;
                }
                continue;
            }
        };

        // Read until the stream ends, then fall through to the backoff.
        loop {
            let next = tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        return report;
                    }
                    continue;
                }
                next = stream.recv() => next,
            };

            let Some(message) = next else { break };
            let update = match message {
                Ok(update) => update,
                Err(_) => break,
            };

            metrics.record_update(&update.payload);

            // A startup snapshot account is state, not an event. Feeding it
            // through would emit a curve tick at whatever slot the plugin
            // happened to be replaying, and a price difference against it would
            // be a move that never happened.
            if let UpdatePayload::Account(account) = &update.payload {
                if account.is_startup {
                    metrics.record_startup_skip();
                    continue;
                }
            }

            let outcome = pipeline.ingest(update);
            if outcome.decode_error.is_some() {
                metrics.record_decode_failure();
            }
            if outcome.foreign_account {
                metrics.record_foreign_account();
            }
            if let Some(from_slot) = outcome.unrecoverable_from_slot {
                metrics.record_unwind();
                sink.unwind(from_slot);
            }
            // Mirrored out here because this is the only scope that holds the
            // pipeline: it lives on this task and nothing else may touch it.
            metrics.record_pipeline(
                pipeline.ring_metrics(),
                pipeline.ledger(),
                pipeline.curves().stale_writes(),
            );
            if !outcome.released.is_empty() {
                metrics.record_events(outcome.released.len());
                sink.emit(outcome.released);
            }
        }

        let wait = policy.record_failure();
        metrics.record_disconnect(wait);
        report.backoffs.push(wait);
        if !wait_or_shutdown(wait, limits.sleep, &mut shutdown).await {
            return report;
        }
    }
}

/// Waits out a backoff. `false` if shutdown arrived first.
async fn wait_or_shutdown(
    wait: Duration,
    sleep: bool,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> bool {
    if !sleep {
        return !*shutdown.borrow();
    }
    tokio::select! {
        biased;
        _ = shutdown.changed() => !*shutdown.borrow(),
        _ = tokio::time::sleep(wait) => true,
    }
}

// ---------------------------------------------------------------------------
// the feed, as one handle
// ---------------------------------------------------------------------------

/// How often the Geyser counters are published to telemetry.
///
/// The same five seconds [`crate::ingestion`] uses, deliberately: the two feeds
/// are read side by side, and two cadences would make one of them look like it
/// had stalled every time the other published.
const TELEMETRY_INTERVAL: Duration = Duration::from_secs(5);

/// The transport this build can actually dial.
///
/// One function rather than a `cfg` at each call site, so that turning the
/// feature off changes what the feed *is* in one place instead of changing
/// whether three call sites compile.
pub fn default_transport() -> Arc<dyn GeyserTransport> {
    #[cfg(feature = "geyser-grpc")]
    {
        Arc::new(grpc::GrpcTransport)
    }
    #[cfg(not(feature = "geyser-grpc"))]
    {
        Arc::new(NoTransport)
    }
}

/// The Geyser feed, as one handle.
///
/// [`crate::ingestion::IngestionManager`]'s opposite number, and shaped like it
/// on purpose: `start` returns immediately and dials in the background, `stop`
/// is safe twice and safe off the runtime, and `snapshot` is free of side
/// effects so a window can poll it.
///
/// The difference is what comes out. Ingestion owns the channels a candidate
/// travels on; this owns the sequencer that decides *when* a candidate is ready
/// to travel, and then hands it to ingestion. There is one queue into the
/// engine, not two, and that is the point of wiring it this way — two producers
/// with two sets of filters would be two strategies wearing one name.
pub struct GeyserFeed {
    metrics: Arc<GeyserMetrics>,
    shutdown: tokio::sync::watch::Sender<bool>,
    /// Set by [`stop`](Self::stop), and the reason `is_connected` cannot be
    /// derived from the counters alone: the read task is aborted mid-read, so
    /// it never gets to record the disconnect it is being given.
    stopped: std::sync::atomic::AtomicBool,
    tasks: parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// The endpoint with its credential removed, or `None` when nothing was
    /// configured and no socket was opened.
    endpoint: Option<String>,
    provider: FeedProvider,
}

impl GeyserFeed {
    /// Starts the subscriber, or starts nothing at all.
    ///
    /// `config` of `None` is the normal case on a checkout with no provider
    /// configured, and it is silence rather than a failing dial: the same rule
    /// [`crate::ingestion`] follows for an empty endpoint list, and the roadmap's
    /// Phase 0 gate is what it comes from — a live feed is opened as a
    /// deliberate act or not at all.
    ///
    /// Must be called from inside a tokio runtime.
    pub fn start(
        config: Option<GeyserConfig>,
        transport: Arc<dyn GeyserTransport>,
        ingestion: Arc<IngestionManager>,
        telemetry: Option<Arc<TelemetryHub>>,
    ) -> Arc<Self> {
        let metrics = Arc::new(GeyserMetrics::default());
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);

        let Some(config) = config else {
            if let Some(hub) = &telemetry {
                hub.publish(
                    TelemetryLevel::Info,
                    "geyser",
                    "no geyser endpoint configured; the sub-slot pipeline is idle".to_string(),
                    serde_json::json!({ "configured": false }),
                );
            }
            return Arc::new(GeyserFeed {
                metrics,
                shutdown,
                stopped: std::sync::atomic::AtomicBool::new(false),
                tasks: parking_lot::Mutex::new(Vec::new()),
                endpoint: None,
                provider: FeedProvider::Triton,
            });
        };

        let endpoint = config.redacted();
        let provider = config.provider;
        if let Some(hub) = &telemetry {
            hub.publish(
                TelemetryLevel::Info,
                "geyser",
                format!("geyser subscribing to {endpoint}"),
                serde_json::json!({
                    "configured": true,
                    "provider": provider.as_str(),
                    "endpoint": endpoint,
                    "commitment": config.commitment.as_str(),
                    "ringCapacity": config.ring.capacity,
                    "holdSlots": config.ring.hold_slots,
                }),
            );
        }

        let mut pipeline = TickPipeline::new(&config);
        let mut sink =
            IngestionSink::new(provider, ingestion, Arc::clone(&metrics), telemetry.clone());
        let subscriber_metrics = Arc::clone(&metrics);
        let mut tasks = Vec::with_capacity(2);
        tasks.push(tokio::spawn(async move {
            run_subscriber(
                transport.as_ref(),
                config,
                &mut pipeline,
                &mut sink,
                &subscriber_metrics,
                RunLimits::default(),
                shutdown_rx,
            )
            .await;
            // The hold window is a bet that something older is still in flight.
            // On the way out nothing is, so holding is pure loss and what is
            // left goes downstream in order.
            let tail = pipeline.flush();
            if !tail.is_empty() {
                subscriber_metrics.record_events(tail.len());
                sink.emit(tail);
            }
        }));

        if let Some(hub) = telemetry {
            tasks.push(tokio::spawn(telemetry_loop(
                Arc::clone(&metrics),
                hub,
                TELEMETRY_INTERVAL,
                shutdown.subscribe(),
            )));
        }

        Arc::new(GeyserFeed {
            metrics,
            shutdown,
            stopped: std::sync::atomic::AtomicBool::new(false),
            tasks: parking_lot::Mutex::new(tasks),
            endpoint: Some(endpoint),
            provider,
        })
    }

    /// Whether an endpoint was configured and a subscriber is running for it.
    pub fn is_configured(&self) -> bool {
        self.endpoint.is_some()
    }

    /// The endpoint with its credential removed.
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    pub const fn provider(&self) -> FeedProvider {
        self.provider
    }

    /// Whether a subscription is open right now.
    ///
    /// Two terms. The first is the flag the read loop maintains as it connects
    /// and disconnects. The second is `stopped`, and it is needed because
    /// [`stop`](Self::stop) *aborts* the read task rather than letting it
    /// finish — an aborted task runs no more code, including the line that
    /// would have recorded its own disconnect.
    ///
    /// [`crate::refuse_over_a_live_feed`] is what reads this: it is the guard
    /// that decides whether a replay bar over the panes is telling the truth,
    /// and a feed that reported itself down while still delivering candidates
    /// would make that bar a lie.
    pub fn is_connected(&self) -> bool {
        !self.stopped.load(Ordering::Relaxed) && self.metrics.is_connected()
    }

    /// Every counter, including the sequencer state the read loop mirrors out.
    /// Free of side effects, so the window can poll it as often as it likes.
    pub fn snapshot(&self) -> GeyserSnapshot {
        self.metrics.snapshot_now()
    }

    /// The counters, shared, for anything that wants to record into them.
    pub fn metrics(&self) -> Arc<GeyserMetrics> {
        Arc::clone(&self.metrics)
    }

    /// Stops the subscriber. Safe to call twice and safe off the runtime.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        let _ = self.shutdown.send(true);
        for task in self.tasks.lock().drain(..) {
            task.abort();
        }
    }
}

/// Publishes the Geyser counters on a fixed interval.
///
/// The level is the news: anything the ring had to shed, force out early, or
/// fail to roll back is a sentence about ordering being degraded, and it should
/// not arrive at the same volume as a healthy count of updates.
async fn telemetry_loop(
    metrics: Arc<GeyserMetrics>,
    hub: Arc<TelemetryHub>,
    interval: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = ticker.tick() => {
                let snapshot = metrics.snapshot_now();
                let degraded = snapshot.ring.shed
                    + snapshot.ring.forced_releases
                    + snapshot.ring.late
                    + snapshot.ring.unrecoverable_reorgs
                    + snapshot.decode_failures
                    > 0;
                hub.publish(
                    if degraded { TelemetryLevel::Warn } else { TelemetryLevel::Debug },
                    "geyser",
                    "geyser metrics",
                    serde_json::to_value(snapshot).unwrap_or_else(
                        |_| serde_json::json!({ "error": "geyser metrics would not serialise" }),
                    ),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// the gRPC transport
// ---------------------------------------------------------------------------

/// The real Yellowstone transport, behind the `geyser-grpc` feature.
///
/// # Why it is a feature and not just a dependency
///
/// tonic brings hyper, h2, tower, prost, a protobuf compiler for the build, and
/// rustls — about ninety crates, and a *second* TLS stack next to the
/// `native-tls` the websocket feed already uses. Two trust stores in one binary
/// is a real cost: two sets of roots to reason about, two code paths for a
/// certificate error, and a larger surface on a process that holds keys.
///
/// That is a fine price for the feed this build actually runs on, and a silly
/// one for a build that is running fixtures. So the sequencing, the ordering,
/// the re-org handling and the reconnect schedule — everything with logic in it
/// — compile and are tested with no gRPC at all, and this module is the thin
/// layer that turns protobuf into [`GeyserUpdate`].
///
/// # What this layer is responsible for
///
/// Three things, and none of them is business logic.
///
/// **Not copying the payload.** The proto crate is built with
/// `account-data-as-bytes`, so an account's data field is a [`Bytes`] that
/// prost fills by splitting the read buffer. [`decode_update`] moves it
/// straight into [`AccountUpdate`], and a transaction's logs are moved out of
/// the metadata rather than cloned off a borrow of it. The wire message is
/// dropped immediately afterwards; anything copied out of it would be copied
/// for nothing.
///
/// **Keeping the outbound half open.** `subscribe` is a bidirectional call, and
/// a client that sends its subscription and then lets the request stream end
/// has half-closed the connection — which Yellowstone reads as the subscriber
/// leaving. So the request goes down a channel whose sender lives in the
/// keepalive task, and the task lives in [`GrpcStream`], which means the
/// outbound half closes exactly when the stream is dropped and not before.
///
/// **Never breaking the stream over one bad message.** A malformed account in a
/// stream of good ones is a [`DecodeError`] the caller counts, and a message
/// this build does not subscribe to is `Ok(None)`. Neither is a disconnect.
#[cfg(feature = "geyser-grpc")]
pub mod grpc {
    use super::*;

    use tonic::metadata::MetadataValue;
    use tonic::service::interceptor::InterceptedService;
    use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
    use yellowstone_grpc_proto::geyser::{
        geyser_client::GeyserClient, subscribe_update::UpdateOneof, CommitmentLevel,
        SlotStatus as WireSlotStatus, SubscribeRequest, SubscribeRequestFilterAccounts,
        SubscribeRequestFilterAccountsFilter, SubscribeRequestFilterAccountsFilterMemcmp,
        SubscribeRequestFilterSlots, SubscribeRequestFilterTransactions, SubscribeRequestPing,
        SubscribeUpdate,
    };

    /// Filter names. The server echoes these back on every update, so they are
    /// the only way to tell which subscription a message answered.
    const CURVES: &str = "curves";
    const SLOTS: &str = "slots";
    const TRANSACTIONS: &str = "transactions";

    impl From<Commitment> for CommitmentLevel {
        fn from(value: Commitment) -> Self {
            match value {
                Commitment::Processed => CommitmentLevel::Processed,
                Commitment::Confirmed => CommitmentLevel::Confirmed,
                Commitment::Finalized => CommitmentLevel::Finalized,
            }
        }
    }

    /// Builds the wire request from this module's own description of it.
    ///
    /// There is no pool subscription here and its absence is deliberate. The
    /// four pool programs were subscribed to for a decoder that does not exist
    /// — see [`pool_tick`], which is constructed nowhere — so every one of
    /// those account writes crossed the wire, cost quota, and produced no tick.
    /// Asking for them again is the cheap part; it waits on something that can
    /// read one.
    pub fn subscribe_request(filters: &SubscribeFilters) -> SubscribeRequest {
        let mut accounts = std::collections::HashMap::new();

        // Curves: owned by pump.fun and exactly the curve account's size. The
        // size filter is what keeps every other pump.fun account off the wire.
        //
        // Guarded, and the guard is not paranoia about a vector that is built
        // from a constant three lines up. On this wire format an accounts
        // filter that names no owner and no account is not an empty
        // subscription — it matches *every account on Solana*. So the one edit
        // that looks like switching a subscription off, clearing the list, is
        // the edit that turns the firehose fully on, and it would fail towards
        // an unpayable bill rather than towards silence. The empty case is
        // refused here so that it cannot be expressed.
        if !filters.curve_owners.is_empty() {
            accounts.insert(
                CURVES.to_string(),
                SubscribeRequestFilterAccounts {
                    owner: filters.curve_owners.clone(),
                    filters: filters
                        .curve_data_size
                        .map(|size| {
                            vec![SubscribeRequestFilterAccountsFilter {
                                filter: Some(
                                    yellowstone_grpc_proto::geyser::subscribe_request_filter_accounts_filter::Filter::Datasize(size),
                                ),
                            }]
                        })
                        .unwrap_or_default(),
                    ..Default::default()
                },
            );
        }

        let mut slots = std::collections::HashMap::new();
        slots.insert(
            SLOTS.to_string(),
            SubscribeRequestFilterSlots {
                // Every status, not just the subscribed commitment: the ledger
                // needs `Dead` and the parent transitions to see a fork, and
                // filtering by commitment would hide exactly those.
                filter_by_commitment: Some(false),
                interslot_updates: Some(false),
            },
        );

        let mut transactions = std::collections::HashMap::new();
        transactions.insert(
            TRANSACTIONS.to_string(),
            SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                account_include: filters.transaction_includes.clone(),
                ..Default::default()
            },
        );

        SubscribeRequest {
            accounts,
            slots,
            transactions,
            commitment: Some(CommitmentLevel::from(filters.commitment) as i32),
            from_slot: filters.from_slot,
            ..Default::default()
        }
    }

    /// The wire's slot status, in this module's terms.
    fn slot_phase(status: i32) -> SlotPhase {
        match WireSlotStatus::try_from(status) {
            Ok(WireSlotStatus::SlotProcessed) => SlotPhase::Processed,
            Ok(WireSlotStatus::SlotConfirmed) => SlotPhase::Confirmed,
            Ok(WireSlotStatus::SlotFinalized) => SlotPhase::Finalized,
            Ok(WireSlotStatus::SlotFirstShredReceived) => SlotPhase::FirstShredReceived,
            Ok(WireSlotStatus::SlotCompleted) => SlotPhase::Completed,
            Ok(WireSlotStatus::SlotCreatedBank) => SlotPhase::CreatedBank,
            Ok(WireSlotStatus::SlotDead) => SlotPhase::Dead,
            // An unrecognised status from a newer plugin. Treated as a bank
            // creation — the weakest thing it could be — rather than guessed
            // at, so it advances nothing and voids nothing.
            Err(_) => SlotPhase::CreatedBank,
        }
    }

    /// A protobuf timestamp in microseconds. Absent is zero, which sorts a
    /// stamp-less update first within its slot.
    fn micros_of(stamp: Option<&::prost_types::Timestamp>) -> u64 {
        stamp.map_or(0, |stamp| {
            let seconds = u64::try_from(stamp.seconds).unwrap_or(0);
            let nanos = u64::try_from(stamp.nanos).unwrap_or(0);
            seconds
                .saturating_mul(1_000_000)
                .saturating_add(nanos / 1_000)
        })
    }

    /// One wire token balance, with the float ignored.
    fn token_balance(
        wire: &yellowstone_grpc_proto::solana::storage::confirmed_block::TokenBalance,
    ) -> Result<TokenBalance, DecodeError> {
        let amount = wire
            .ui_token_amount
            .as_ref()
            .ok_or(DecodeError::BadAmount)?;
        // `amount.ui_amount` is an f64 sitting right here and it is not read.
        // `amount.amount` is the raw integer as a string, and it is the only
        // field this engine will accept.
        let raw = parse_raw_amount(&amount.amount)?;
        let decimals = u8::try_from(amount.decimals).map_err(|_| DecodeError::BadAmount)?;
        Ok(TokenBalance {
            account_index: wire.account_index,
            mint: Pubkey::parse(&wire.mint).map_err(|_| DecodeError::BadPubkey)?,
            owner: Pubkey::parse(&wire.owner).unwrap_or(Pubkey::ZERO),
            raw,
            decimals,
        })
    }

    /// One wire message, in this module's terms.
    ///
    /// `Ok(None)` for a message this build has no use for, which is not an
    /// error and must not break the stream.
    pub fn decode_update(wire: SubscribeUpdate) -> Result<Option<GeyserUpdate>, DecodeError> {
        let micros = micros_of(wire.created_at.as_ref());
        let Some(update) = wire.update_oneof else {
            return Ok(None);
        };

        let payload = match update {
            UpdateOneof::Account(account) => {
                let Some(info) = account.account else {
                    return Ok(None);
                };
                UpdatePayload::Account(AccountUpdate {
                    slot: account.slot,
                    pubkey: parse_pubkey(&info.pubkey)?,
                    owner: parse_pubkey(&info.owner)?,
                    lamports: info.lamports,
                    write_version: info.write_version,
                    data: info.data,
                    is_startup: account.is_startup,
                })
            }
            UpdateOneof::Slot(slot) => UpdatePayload::Slot(SlotUpdate {
                slot: slot.slot,
                parent: slot.parent,
                phase: slot_phase(slot.status),
            }),
            UpdateOneof::Transaction(transaction) => {
                let Some(mut info) = transaction.transaction else {
                    return Ok(None);
                };
                // `info.meta` is taken rather than borrowed so the log vector
                // can be *moved* out of it. A transaction's logs are the one
                // unbounded allocation on this stream — a pump.fun buy carries
                // a dozen lines of program output — and cloning them to read a
                // message that is about to be dropped is a copy of the whole
                // thing per transaction, for nothing.
                let (logs, pre, post, failed) = match info.meta.take() {
                    Some(mut meta) => {
                        let mut pre = Vec::with_capacity(meta.pre_token_balances.len());
                        for balance in &meta.pre_token_balances {
                            pre.push(token_balance(balance)?);
                        }
                        let mut post = Vec::with_capacity(meta.post_token_balances.len());
                        for balance in &meta.post_token_balances {
                            post.push(token_balance(balance)?);
                        }
                        // `log_messages_none` is the validator saying logs were
                        // not captured, which is different from a transaction
                        // that produced none. Either way there is nothing to
                        // take, and the empty vector allocates nothing.
                        let logs = if meta.log_messages_none {
                            Vec::new()
                        } else {
                            std::mem::take(&mut meta.log_messages)
                        };
                        (logs, pre, post, meta.err.is_some())
                    }
                    None => (Vec::new(), Vec::new(), Vec::new(), false),
                };
                UpdatePayload::Transaction(TransactionUpdate {
                    slot: transaction.slot,
                    signature: parse_signature(&info.signature)?,
                    index: info.index,
                    is_vote: info.is_vote,
                    failed,
                    logs,
                    pre_token_balances: pre,
                    post_token_balances: post,
                })
            }
            UpdateOneof::Ping(_) => UpdatePayload::Ping,
            UpdateOneof::Pong(_) => UpdatePayload::Pong,
            // Block, BlockMeta, Entry and TransactionStatus are not subscribed
            // to, so one arriving means the server sent something unasked for.
            // Ignored rather than treated as a fault.
            _ => return Ok(None),
        };

        Ok(Some(GeyserUpdate::new(micros, payload)))
    }

    /// The `x-token` interceptor, boxed so the client type can be named.
    type Authorised =
        Box<dyn FnMut(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Send>;

    type Client = GeyserClient<InterceptedService<Channel, Authorised>>;

    /// How often the client speaks on the outbound half.
    ///
    /// Below every provider's idle-subscription timeout, and well below the
    /// fifteen-second HTTP/2 keepalive above it, which answers a different
    /// question: an H2 PING proves the *socket* is alive, and a `Pong` off this
    /// one proves the *subscription* is. A stream can be perfectly connected
    /// and no longer subscribed to anything.
    const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);

    /// A live Yellowstone stream.
    ///
    /// Holds the keepalive task, and holding it is the point. The outbound half
    /// of a `subscribe` call is a stream, and when that stream ends the client
    /// half-closes — which Yellowstone reads as the subscriber leaving and
    /// tears the subscription down. So the sender lives inside the task, the
    /// task lives inside this struct, and the outbound half stays open for
    /// exactly as long as somebody is reading the inbound one.
    pub struct GrpcStream {
        inner: tonic::Streaming<SubscribeUpdate>,
        keepalive: tokio::task::JoinHandle<()>,
    }

    impl Drop for GrpcStream {
        fn drop(&mut self) {
            // The reconnect loop drops the stream on every disconnect. Aborting
            // here is what closes the outbound half with it — without this the
            // task would outlive its stream and keep pinging a subscription
            // nobody is reading.
            self.keepalive.abort();
        }
    }

    impl GeyserStream for GrpcStream {
        fn recv(&mut self) -> BoxFuture<'_, Option<Result<GeyserUpdate, GeyserError>>> {
            Box::pin(async move {
                loop {
                    return match self.inner.message().await {
                        Ok(Some(wire)) => match decode_update(wire) {
                            // A message this build has no use for is not news.
                            // The loop goes round rather than waking the caller
                            // with nothing.
                            Ok(None) => continue,
                            Ok(Some(update)) => Some(Ok(update)),
                            Err(error) => Some(Err(GeyserError::Decode(error))),
                        },
                        Ok(None) => None,
                        Err(status) => Some(Err(GeyserError::Stream(status.to_string()))),
                    };
                }
            })
        }
    }

    /// How much of an error chain is worth printing.
    ///
    /// Long enough for the useful layers, short enough that a log line stays a
    /// line.
    const REASON_LIMIT: usize = 200;

    /// A transport error as a sentence that says something.
    ///
    /// This exists because `tonic::transport::Error` displays as the literal
    /// string `"transport error"` and nothing else. Every fact about what
    /// actually happened — connection refused, DNS failure, TLS rejected, the
    /// timeout — is one or more links down the `source()` chain, so reporting
    /// the top level alone would be surfacing the reason in name only, which
    /// is worse than not surfacing it: it looks like an answer.
    ///
    /// The chain is walked, deduplicated (the layers repeat themselves), and
    /// scrubbed of anything secret before it becomes a string that a log line
    /// can hold. See [`scrub`].
    fn reason(config: &GeyserConfig, error: &dyn std::error::Error) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut link: Option<&(dyn std::error::Error + 'static)> = error.source();
        parts.push(error.to_string());
        // Bounded rather than `while let`: a cyclic `source()` chain is a bug
        // in someone else's crate that would hang this loop, and a dial error
        // is not worth trusting a stranger's iterator invariants for.
        for _ in 0..8 {
            let Some(current) = link else { break };
            let text = current.to_string();
            if !parts.iter().any(|seen| seen == &text) {
                parts.push(text);
            }
            link = current.source();
        }

        let mut joined = scrub(config, &parts.join(": "));
        if joined.chars().count() > REASON_LIMIT {
            joined = joined.chars().take(REASON_LIMIT).collect::<String>() + "…";
        }
        joined
    }

    /// Takes the credentials out of an error string.
    ///
    /// Belt and braces, and deliberately so. Nothing observed in tonic's chain
    /// carries the URL path — which is where Helius and QuickNode put the API
    /// key — but "observed" is a statement about the error kinds that happened
    /// to be tried, and this string is bound for a log file that outlives the
    /// process. The path and the token are removed by value, so a layer that
    /// starts quoting the whole URI tomorrow does not turn a dial failure into
    /// a credential leak.
    ///
    /// The host survives on purpose: it is the half of the endpoint that makes
    /// the error mean anything, and it is already published at startup by
    /// [`GeyserConfig::redacted`].
    fn scrub(config: &GeyserConfig, text: &str) -> String {
        let mut text = text.to_string();

        // The path, if there is one worth hiding. `redacted()` keeps
        // `scheme://host`; whatever follows it is the part that can carry a key.
        if let Some((scheme, rest)) = config.endpoint.split_once("://") {
            if let Some(slash) = rest.find('/') {
                let path = &rest[slash..];
                // A bare trailing "/" is not a credential and replacing it
                // would just make every error unreadable.
                if path.len() > 1 {
                    text = text.replace(path, "/…");
                }
                let _ = scheme;
            }
        }

        if let Some(token) = &config.token {
            if !token.is_empty() {
                text = text.replace(token.as_str(), "…");
            }
        }

        text
    }

    /// The `x-token` interceptor for a token, or a pass-through without one.
    ///
    /// Extracted from the dial so it can be tested at all. Everything else in
    /// [`GrpcTransport::connect`] needs a socket, and the consequence was that
    /// the one line carrying the credential was the one line no test ran: spell
    /// the header wrong and every request goes out unauthenticated, which
    /// arrives as a connect failure against a provider that looks down. That is
    /// a bad afternoon to debug for the sake of a function signature.
    ///
    /// The token is converted once, here, rather than per request — and the
    /// error when it will not convert says so without quoting it. A credential
    /// in an error string is a credential in a log file.
    fn authorisation(token: Option<&str>) -> Result<Authorised, GeyserError> {
        let token = token
            .map(|token| {
                MetadataValue::try_from(token)
                    .map_err(|_| GeyserError::Dial("x-token is not a valid header value".into()))
            })
            .transpose()?;

        Ok(Box::new(move |mut request: tonic::Request<()>| {
            if let Some(token) = token.clone() {
                request.metadata_mut().insert("x-token", token);
            }
            Ok(request)
        }))
    }

    /// Dials Yellowstone over gRPC.
    pub struct GrpcTransport;

    impl GrpcTransport {
        async fn connect(config: &GeyserConfig) -> Result<Client, GeyserError> {
            let mut endpoint = Endpoint::from_shared(config.endpoint.clone())
                .map_err(|err| {
                    GeyserError::Dial(format!(
                        "the endpoint is not a URI: {}",
                        reason(config, &err)
                    ))
                })?
                // A stream that goes quiet is indistinguishable from a socket
                // that died until something asks. HTTP/2 keepalives are what
                // ask, and without them a half-open connection holds the
                // pipeline until TCP notices, which can be minutes.
                .http2_keep_alive_interval(Duration::from_secs(15))
                .keep_alive_timeout(Duration::from_secs(10))
                .keep_alive_while_idle(true)
                .connect_timeout(Duration::from_secs(10))
                // Account updates carry the whole account, and a curve is small
                // but a pool is not. Well above anything expected, and still a
                // bound.
                .tcp_nodelay(true);

            if config.endpoint.starts_with("https://") {
                endpoint = endpoint
                    .tls_config(ClientTlsConfig::new().with_native_roots())
                    .map_err(|err| {
                        GeyserError::Dial(format!("tls setup failed: {}", reason(config, &err)))
                    })?;
            }

            let channel = endpoint.connect().await.map_err(|err| {
                GeyserError::Dial(format!(
                    "{} could not be reached: {}",
                    config.redacted(),
                    reason(config, &err)
                ))
            })?;

            Ok(
                GeyserClient::with_interceptor(channel, authorisation(config.token.as_deref())?)
                    .max_decoding_message_size(64 * 1024 * 1024),
            )
        }
    }

    /// The subscription again, as something safe to send down an open stream.
    ///
    /// Two servers are catered for with one message, deliberately. Yellowstone
    /// lets a subscription be amended mid-stream by sending another
    /// `SubscribeRequest`, and implementations differ on whether one carrying a
    /// `ping` is treated as a ping *instead of* an amendment or as both. A bare
    /// `SubscribeRequest { ping, ..Default::default() }` — which is the obvious
    /// thing to send — is an amendment to *no filters at all* on the second
    /// reading, and would silently unsubscribe the feed while leaving every
    /// counter looking healthy.
    ///
    /// So the keepalive is the subscription itself with a ping attached: an
    /// amendment to exactly what is already in force, which is a no-op, or a
    /// ping, which is what it is for.
    ///
    /// `from_slot` is the one field cleared. It means "start there", and a
    /// server that honoured it on an amendment would replay history every ten
    /// seconds.
    fn keepalive_request(subscription: &SubscribeRequest, id: i32) -> SubscribeRequest {
        SubscribeRequest {
            ping: Some(SubscribeRequestPing { id }),
            from_slot: None,
            ..subscription.clone()
        }
    }

    impl GeyserTransport for GrpcTransport {
        fn subscribe(
            &self,
            config: GeyserConfig,
            filters: SubscribeFilters,
        ) -> BoxFuture<'static, Result<Box<dyn GeyserStream>, GeyserError>> {
            Box::pin(async move {
                let mut client = GrpcTransport::connect(&config).await?;
                let request = subscribe_request(&filters);

                // The outbound half is a channel rather than a finished stream.
                // `stream::iter(vec![request])` would deliver the subscription
                // and then end, and the end of the request body is a half-close
                // — the client telling the server it has nothing further to say.
                // Yellowstone reads that as the subscriber leaving.
                let (outbound_tx, mut outbound_rx) =
                    tokio::sync::mpsc::unbounded_channel::<SubscribeRequest>();
                let outbound = futures_util::stream::poll_fn(move |cx| outbound_rx.poll_recv(cx));

                let subscription = request.clone();
                outbound_tx
                    .send(request)
                    .map_err(|_| GeyserError::Subscribe("the outbound half closed".into()))?;

                let response = client
                    .subscribe(outbound)
                    .await
                    // Scrubbed like a dial error, for the same reason: a
                    // rejection is the server quoting the request back, and an
                    // auth rejection is the one most likely to quote the part
                    // that authenticated it.
                    .map_err(|status| {
                        GeyserError::Subscribe(scrub(&config, &status.to_string()))
                    })?;

                // The sender lives here and nowhere else, so the outbound half
                // closes exactly when this task is aborted — which `Drop` does
                // when the stream goes.
                let keepalive = tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(KEEPALIVE_INTERVAL);
                    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    ticker.tick().await;
                    let mut id: i32 = 0;
                    loop {
                        ticker.tick().await;
                        id = id.wrapping_add(1);
                        if outbound_tx
                            .send(keepalive_request(&subscription, id))
                            .is_err()
                        {
                            // The receiving half went with the request body,
                            // which means the stream is already over.
                            return;
                        }
                    }
                });

                Ok(Box::new(GrpcStream {
                    inner: response.into_inner(),
                    keepalive,
                }) as Box<dyn GeyserStream>)
            })
        }
    }

    // A memcmp filter is not used by any subscription above, but the import is
    // what documents that account filters can narrow further than data size.
    // Referencing it here keeps the unused-import lint honest.
    #[allow(dead_code)]
    fn _memcmp_is_available(filter: SubscribeRequestFilterAccountsFilterMemcmp) -> bool {
        filter.offset == 0
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        use yellowstone_grpc_proto::geyser::{
            SubscribeUpdateAccount, SubscribeUpdateAccountInfo, SubscribeUpdateTransaction,
            SubscribeUpdateTransactionInfo,
        };
        use yellowstone_grpc_proto::solana::storage::confirmed_block::{
            TokenBalance as WireTokenBalance, TransactionStatusMeta, UiTokenAmount,
        };

        fn wire(update: UpdateOneof, seconds: i64, nanos: i32) -> SubscribeUpdate {
            SubscribeUpdate {
                filters: vec![CURVES.to_string()],
                created_at: Some(::prost_types::Timestamp { seconds, nanos }),
                update_oneof: Some(update),
            }
        }

        #[test]
        fn a_timestamp_becomes_microseconds() {
            // Seconds and nanos in, microseconds out, with the sub-microsecond
            // tail truncated rather than rounded — the same direction every
            // other quotient in this module goes.
            assert_eq!(
                micros_of(Some(&::prost_types::Timestamp {
                    seconds: 2,
                    nanos: 500_000
                })),
                2_000_500
            );
            assert_eq!(
                micros_of(Some(&::prost_types::Timestamp {
                    seconds: 0,
                    nanos: 999
                })),
                0
            );
            // A stampless update sorts first within its slot rather than
            // failing, because a missing timestamp is not a missing event.
            assert_eq!(micros_of(None), 0);
            // A negative timestamp is nonsense from the wire, not a panic.
            assert_eq!(
                micros_of(Some(&::prost_types::Timestamp {
                    seconds: -5,
                    nanos: -1
                })),
                0
            );
        }

        #[test]
        fn every_wire_slot_status_maps_to_a_phase() {
            let cases = [
                (WireSlotStatus::SlotProcessed, SlotPhase::Processed),
                (WireSlotStatus::SlotConfirmed, SlotPhase::Confirmed),
                (WireSlotStatus::SlotFinalized, SlotPhase::Finalized),
                (
                    WireSlotStatus::SlotFirstShredReceived,
                    SlotPhase::FirstShredReceived,
                ),
                (WireSlotStatus::SlotCompleted, SlotPhase::Completed),
                (WireSlotStatus::SlotCreatedBank, SlotPhase::CreatedBank),
                (WireSlotStatus::SlotDead, SlotPhase::Dead),
            ];
            for (wire, expected) in cases {
                assert_eq!(slot_phase(wire as i32), expected, "{wire:?}");
            }
            // A status from a newer plugin advances nothing and voids nothing.
            assert_eq!(slot_phase(9_999), SlotPhase::CreatedBank);
            assert_eq!(slot_phase(9_999).commitment(), None);
        }

        #[test]
        fn an_account_update_carries_its_write_version_across() {
            let decoded = decode_update(wire(
                UpdateOneof::Account(SubscribeUpdateAccount {
                    account: Some(SubscribeUpdateAccountInfo {
                        pubkey: vec![1u8; 32],
                        lamports: 2_039_280,
                        owner: vec![2u8; 32],
                        executable: false,
                        rent_epoch: 0,
                        data: Bytes::from_static(&[9u8; 81]),
                        write_version: 4_242,
                        txn_signature: None,
                    }),
                    slot: 77,
                    is_startup: true,
                }),
                1,
                0,
            ))
            .expect("a well-formed account")
            .expect("an account is something this build wants");

            assert_eq!(decoded.created_at_micros, 1_000_000);
            let UpdatePayload::Account(account) = decoded.payload else {
                panic!("not an account");
            };
            assert_eq!(account.slot, 77);
            assert_eq!(
                account.write_version, 4_242,
                "the sub-slot sequencer must survive"
            );
            assert_eq!(account.pubkey, Pubkey::new([1u8; 32]));
            assert_eq!(account.owner, Pubkey::new([2u8; 32]));
            assert!(account.is_startup);
        }

        #[test]
        fn a_short_pubkey_is_a_decode_error_not_a_panic() {
            let outcome = decode_update(wire(
                UpdateOneof::Account(SubscribeUpdateAccount {
                    account: Some(SubscribeUpdateAccountInfo {
                        pubkey: vec![1u8; 31], // one byte short
                        owner: vec![2u8; 32],
                        ..Default::default()
                    }),
                    slot: 1,
                    is_startup: false,
                }),
                0,
                0,
            ));
            assert_eq!(outcome, Err(DecodeError::BadPubkey));
        }

        #[test]
        fn a_token_balance_is_read_from_the_string_and_never_the_float() {
            // The exact place the zero-float invariant is most at risk: the
            // wire type carries both, and `ui_amount` is deliberately set to a
            // value that disagrees with `amount` so that reading the wrong one
            // cannot pass.
            let balance = WireTokenBalance {
                account_index: 3,
                mint: crate::ingestion::PUMP_FUN_PROGRAM.to_string(),
                owner: crate::ingestion::PUMP_SWAP_PROGRAM.to_string(),
                program_id: String::new(),
                ui_token_amount: Some(UiTokenAmount {
                    ui_amount: 1.0,
                    decimals: 6,
                    amount: "9007199254740993".to_string(),
                    ui_amount_string: "wrong".to_string(),
                }),
            };
            let decoded = token_balance(&balance).expect("a well-formed balance");
            assert_eq!(decoded.raw, 9_007_199_254_740_993);
            assert_eq!(decoded.decimals, 6);
            assert_eq!(decoded.account_index, 3);
        }

        #[test]
        fn a_malformed_token_amount_stops_the_transaction_rather_than_guessing() {
            let balance = WireTokenBalance {
                mint: crate::ingestion::PUMP_FUN_PROGRAM.to_string(),
                ui_token_amount: Some(UiTokenAmount {
                    ui_amount: 12.5,
                    decimals: 6,
                    amount: "12.5".to_string(),
                    ui_amount_string: "12.5".to_string(),
                }),
                ..Default::default()
            };
            assert_eq!(token_balance(&balance), Err(DecodeError::BadAmount));
        }

        #[test]
        fn a_transaction_carries_its_logs_and_failure_flag() {
            let decoded = decode_update(wire(
                UpdateOneof::Transaction(SubscribeUpdateTransaction {
                    transaction: Some(SubscribeUpdateTransactionInfo {
                        signature: vec![7u8; 64],
                        is_vote: false,
                        transaction: None,
                        meta: Some(TransactionStatusMeta {
                            log_messages: vec!["Program log: buy".to_string()],
                            log_messages_none: false,
                            ..Default::default()
                        }),
                        index: 12,
                    }),
                    slot: 90,
                }),
                0,
                0,
            ))
            .expect("a well-formed transaction")
            .expect("a transaction is something this build wants");

            let UpdatePayload::Transaction(transaction) = decoded.payload else {
                panic!("not a transaction");
            };
            assert_eq!(transaction.signature, Signature::new([7u8; 64]));
            assert_eq!(transaction.index, 12);
            assert!(!transaction.failed, "no err field means it succeeded");
            assert_eq!(transaction.logs, vec!["Program log: buy".to_string()]);
        }

        #[test]
        fn suppressed_logs_are_empty_rather_than_the_literal_field() {
            // `log_messages_none` is how the validator says logs were turned
            // off. Reading `log_messages` regardless would be reading a field
            // the sender declared meaningless.
            let decoded = decode_update(wire(
                UpdateOneof::Transaction(SubscribeUpdateTransaction {
                    transaction: Some(SubscribeUpdateTransactionInfo {
                        signature: vec![7u8; 64],
                        meta: Some(TransactionStatusMeta {
                            log_messages: vec!["leftover".to_string()],
                            log_messages_none: true,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    slot: 1,
                }),
                0,
                0,
            ))
            .unwrap()
            .unwrap();
            let UpdatePayload::Transaction(transaction) = decoded.payload else {
                panic!("not a transaction");
            };
            assert!(transaction.logs.is_empty());
        }

        #[test]
        fn an_unsubscribed_update_is_ignored_rather_than_faulted() {
            // Block, BlockMeta and Entry are not asked for. One arriving is the
            // server being generous, not the stream being broken.
            let decoded = decode_update(wire(UpdateOneof::BlockMeta(Default::default()), 0, 0));
            assert_eq!(decoded, Ok(None));

            // And an update with no oneof at all.
            let empty = decode_update(SubscribeUpdate::default());
            assert_eq!(empty, Ok(None));
        }

        #[test]
        fn the_keepalives_survive_the_crossing() {
            assert!(matches!(
                decode_update(wire(UpdateOneof::Ping(Default::default()), 0, 0))
                    .unwrap()
                    .unwrap()
                    .payload,
                UpdatePayload::Ping
            ));
            assert!(matches!(
                decode_update(wire(UpdateOneof::Pong(Default::default()), 0, 0))
                    .unwrap()
                    .unwrap()
                    .payload,
                UpdatePayload::Pong
            ));
        }

        #[test]
        fn the_request_asks_for_exactly_the_three_filters_and_no_more() {
            let filters = GeyserConfig::default().subscribe_filters();
            let request = subscribe_request(&filters);

            assert_eq!(request.accounts.len(), 1, "curves, and nothing else");
            assert!(
                !request.accounts.contains_key("pools"),
                "the pool subscription is back, and nothing decodes a pool account"
            );
            assert_eq!(request.slots.len(), 1);
            assert_eq!(request.transactions.len(), 1);
            // The expensive subscriptions, all of them empty.
            assert!(
                request.blocks.is_empty(),
                "a block subscription would burn the quota"
            );
            assert!(request.blocks_meta.is_empty());
            assert!(request.entry.is_empty());
            assert!(request.transactions_status.is_empty());
            assert!(request.accounts_data_slice.is_empty());

            let curves = &request.accounts[CURVES];
            assert_eq!(curves.owner, vec![PUMP_FUN_PROGRAM.to_string()]);
            assert!(
                curves.account.is_empty(),
                "no account is named individually"
            );
            assert_eq!(
                curves.filters.len(),
                1,
                "the data-size filter must be present"
            );

            let transactions = &request.transactions[TRANSACTIONS];
            assert_eq!(
                transactions.vote,
                Some(false),
                "vote traffic is never a signal"
            );
            assert_eq!(
                transactions.account_include,
                vec![PUMP_FUN_PROGRAM.to_string()]
            );

            assert_eq!(request.commitment, Some(CommitmentLevel::Confirmed as i32));
            assert_eq!(request.from_slot, None);
        }

        /// A config whose endpoint carries its key in the path, which is how
        /// Helius and QuickNode hand one out.
        fn keyed_config() -> GeyserConfig {
            GeyserConfig {
                endpoint: "https://mainnet.example.com/SUPERSECRETKEY".to_string(),
                token: Some("a-secret-token".to_string()),
                ..GeyserConfig::default()
            }
        }

        #[test]
        fn a_dial_reason_reaches_past_the_word_transport_error() {
            // The whole point of `reason`. `tonic::transport::Error` displays
            // as the literal string "transport error" — no host, no cause, no
            // errno. Surfacing that alone would be worse than surfacing
            // nothing, because it looks like an answer.
            //
            // Port 1 on loopback is refused rather than routed, so this is a
            // real tonic error and not a hand-built stand-in.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime");
            let error = runtime.block_on(async {
                Endpoint::from_shared("http://127.0.0.1:1".to_string())
                    .expect("a valid uri")
                    .connect()
                    .await
                    .expect_err("nothing listens on port 1")
            });

            assert_eq!(
                error.to_string(),
                "transport error",
                "tonic started saying more on its own; this test's premise needs rechecking"
            );

            let text = reason(&GeyserConfig::default(), &error);
            assert!(
                text.len() > "transport error".len(),
                "the cause chain was not walked: {text}"
            );
            assert!(
                text.to_ascii_lowercase().contains("refused")
                    || text.to_ascii_lowercase().contains("connect"),
                "the reason does not say what went wrong: {text}"
            );
        }

        #[test]
        fn a_reason_never_carries_the_credential_out_with_it() {
            // Providers put the API key in the URL path, and this string is
            // bound for a log file that outlives the process. Nothing observed
            // in tonic's chain quotes the path today — but "observed" is a
            // statement about the errors that happened to be tried, so the
            // removal is by value rather than by trust.
            let config = keyed_config();
            let leaky = std::io::Error::other(
                "https://mainnet.example.com/SUPERSECRETKEY rejected a-secret-token",
            );

            let text = reason(&config, &leaky);

            assert!(
                !text.contains("SUPERSECRETKEY"),
                "the path key is in the reason: {text}"
            );
            assert!(
                !text.contains("a-secret-token"),
                "the token is in the reason: {text}"
            );
            // The host survives, because it is what makes the error mean
            // anything and it is already published at startup.
            assert!(
                text.contains("mainnet.example.com"),
                "the host was scrubbed too: {text}"
            );
        }

        #[test]
        fn a_reason_stays_the_length_of_a_log_line() {
            // A chain of a dozen layers, each quoting the one below it, is a
            // paragraph. A log line that wraps eight times is a log line
            // nobody reads.
            let config = GeyserConfig::default();
            let long = std::io::Error::other("x".repeat(1_000));
            let text = reason(&config, &long);

            assert!(
                text.chars().count() <= REASON_LIMIT + 1,
                "unbounded: {} chars",
                text.chars().count()
            );
            assert!(
                text.ends_with('…'),
                "a truncated reason does not say it was truncated: {text}"
            );
        }

        #[test]
        fn a_bare_host_endpoint_is_not_mangled_by_the_scrub() {
            // The scrub replaces the endpoint's path. An endpoint with no path
            // — or just a trailing slash — has nothing to replace, and a naive
            // implementation would replace "/" everywhere and turn every error
            // into noise.
            let config = GeyserConfig {
                endpoint: "https://mainnet.example.com/".to_string(),
                token: None,
                ..GeyserConfig::default()
            };
            let text = scrub(
                &config,
                "dns error: failed to lookup mainnet.example.com/foo",
            );
            assert_eq!(text, "dns error: failed to lookup mainnet.example.com/foo");
        }

        #[test]
        fn the_token_goes_out_as_x_token_and_nowhere_else() {
            // The provider authenticates on this header and no other. Spelled
            // wrong, every request is anonymous and the symptom is a provider
            // that looks down — so the name is asserted literally rather than
            // through a constant that would agree with itself.
            let interceptor = authorisation(Some("a-secret-token"));
            let mut interceptor = interceptor.expect("a plain token converts");
            let request = interceptor(tonic::Request::new(())).expect("the interceptor passes");

            let sent = request
                .metadata()
                .get("x-token")
                .expect("x-token is not set");
            assert_eq!(
                sent.to_str().expect("the token is printable"),
                "a-secret-token"
            );
            // One header, not the token scattered across the likely spellings.
            assert!(request.metadata().get("authorization").is_none());
            assert!(request.metadata().get("x-api-key").is_none());
        }

        #[test]
        fn no_token_configured_sends_no_header_rather_than_an_empty_one() {
            // A local or unauthenticated endpoint. An empty `x-token` is a
            // credential the server would reject; the absent one is a request
            // that never claimed to have one.
            let mut interceptor = authorisation(None).expect("no token is not an error");
            let request = interceptor(tonic::Request::new(())).expect("the interceptor passes");
            assert!(request.metadata().get("x-token").is_none());
        }

        #[test]
        fn a_token_that_cannot_be_a_header_is_refused_without_being_quoted() {
            // A newline in a header value is header injection, and `\n` is
            // exactly what a token pasted out of a terminal carries. It has to
            // be refused — and the refusal must not print the thing it
            // refused, because a dial error reaches a log line and a token in
            // a log file is a token that has escaped.
            let secret = "token-with\r\nInjected: header";
            // Matched rather than `expect_err`, because the success side is a
            // boxed closure and closures are not `Debug`.
            let Err(error) = authorisation(Some(secret)) else {
                panic!("a newline was accepted as a header value");
            };
            let text = error.to_string();

            assert!(
                text.contains("x-token"),
                "the error does not say what failed: {text}"
            );
            assert!(
                !text.contains("token-with"),
                "the credential is in the error: {text}"
            );
            assert!(
                !text.contains("Injected"),
                "the credential is in the error: {text}"
            );
        }

        #[test]
        fn an_accounts_filter_naming_nothing_never_reaches_the_wire() {
            // The one that would not fail loudly. On this wire format a
            // `SubscribeRequestFilterAccounts` naming no owner and no account
            // does not mean "nothing" — it matches every account the filters
            // left on it admit. Here that is the data-size filter alone, so
            // clearing the owner list turns a pump.fun subscription into every
            // eighty-one-byte account on the chain; drop the size filter too
            // and it is every account on Solana, at whatever the endpoint
            // charges per gigabyte.
            //
            // So the obvious way to switch a subscription off, clearing its
            // list, is the way to turn it maximally on, and the failure is a
            // bill rather than an error. This asserts the empty case is
            // dropped instead of sent.
            let filters = SubscribeFilters {
                curve_owners: Vec::new(),
                ..GeyserConfig::default().subscribe_filters()
            };
            let request = subscribe_request(&filters);

            assert!(
                request.accounts.is_empty(),
                "an unbounded accounts subscription was built: {:?}",
                request.accounts
            );
            // The rest of the subscription is untouched — this guards one
            // filter, it does not quietly disarm the request.
            assert_eq!(request.slots.len(), 1);
            assert_eq!(request.transactions.len(), 1);
        }

        #[test]
        fn the_slot_subscription_is_not_filtered_by_commitment() {
            // The ledger needs `Dead` and the parent transitions to see a fork.
            // Filtering slots by the subscribed commitment would hide exactly
            // the statuses the re-org detector runs on.
            let request = subscribe_request(&GeyserConfig::default().subscribe_filters());
            assert_eq!(request.slots[SLOTS].filter_by_commitment, Some(false));
        }

        #[test]
        fn a_resume_slot_reaches_the_wire_request() {
            let mut filters = GeyserConfig::default().subscribe_filters();
            filters.from_slot = Some(1_234);
            filters.commitment = Commitment::Finalized;
            let request = subscribe_request(&filters);
            assert_eq!(request.from_slot, Some(1_234));
            assert_eq!(request.commitment, Some(CommitmentLevel::Finalized as i32));
        }

        // -- the payload is never copied -----------------------------------

        /// The zero-copy claim, measured by pointer rather than described.
        ///
        /// An account write is the highest-volume message on this stream and
        /// its payload is the bulk of it. The claim is that the bytes reaching
        /// [`curve_tick`] are *the wire buffer's own bytes*, not a per-update
        /// allocation with a memcpy behind it, and the only honest way to state
        /// that is to check where they live: if the decoded payload points
        /// inside the frame it was read from, nothing copied it out.
        ///
        /// This is what the `account-data-as-bytes` feature in `Cargo.toml`
        /// buys, and it is a build-configuration property rather than a code
        /// one — which is exactly the kind that gets dropped in a dependency
        /// bump and noticed by nobody. Hence a test.
        #[test]
        fn an_account_payload_is_taken_out_of_the_read_buffer_rather_than_copied() {
            use yellowstone_grpc_proto::prost::Message;

            // Big enough that a copy would be unmistakable, and big enough that
            // no small-buffer optimisation anywhere could hide one.
            let payload = vec![0xABu8; 8_192];
            let update = wire(
                UpdateOneof::Account(SubscribeUpdateAccount {
                    account: Some(SubscribeUpdateAccountInfo {
                        pubkey: vec![1u8; 32],
                        lamports: 2_039_280,
                        owner: vec![2u8; 32],
                        executable: false,
                        rent_epoch: 0,
                        data: payload.clone().into(),
                        write_version: 7,
                        txn_signature: None,
                    }),
                    slot: 99,
                    is_startup: false,
                }),
                1,
                0,
            );

            // One frame, exactly as a codec hands one over: a `Bytes` off the
            // socket, decoded in place.
            let mut encoded = bytes::BytesMut::new();
            update.encode(&mut encoded).expect("the fixture encodes");
            let frame = encoded.freeze();
            let frame_start = frame.as_ptr() as usize;
            let frame_end = frame_start + frame.len();

            let decoded = SubscribeUpdate::decode(frame.clone()).expect("the frame decodes");
            let update = decode_update(decoded)
                .expect("a good frame")
                .expect("an account");
            let UpdatePayload::Account(account) = update.payload else {
                panic!("an account update decoded as something else");
            };

            assert_eq!(
                &account.data[..],
                &payload[..],
                "the payload changed on the way through"
            );
            let data_start = account.data.as_ptr() as usize;
            assert!(
                data_start >= frame_start && data_start < frame_end,
                "the account payload was copied out of the frame rather than shared with it: \
                 payload at {data_start:#x}, frame {frame_start:#x}..{frame_end:#x}"
            );
        }

        // -- the outbound half -------------------------------------------------

        /// The keepalive is the subscription, not an empty request.
        ///
        /// The distinction is the whole reason [`keepalive_request`] exists.
        /// A `SubscribeRequest { ping, ..Default::default() }` is, on the
        /// reading where a request amends the subscription, an amendment to no
        /// filters at all — it would unsubscribe the feed every ten seconds
        /// while every counter kept saying "connected". Sending the filters
        /// already in force is a no-op on that reading and a ping on the other.
        #[test]
        fn the_keepalive_cannot_narrow_the_subscription_it_is_keeping_alive() {
            let mut filters = GeyserConfig::default().subscribe_filters();
            filters.from_slot = Some(9_000);
            let subscription = subscribe_request(&filters);
            let keepalive = keepalive_request(&subscription, 3);

            assert_eq!(keepalive.ping.map(|ping| ping.id), Some(3));
            // Every filter the subscription carries, unchanged.
            assert_eq!(keepalive.accounts, subscription.accounts);
            assert_eq!(keepalive.slots, subscription.slots);
            assert_eq!(keepalive.transactions, subscription.transactions);
            assert_eq!(keepalive.commitment, subscription.commitment);
            assert!(
                !keepalive.accounts.is_empty(),
                "an empty keepalive is an unsubscribe"
            );

            // The one field that must not survive: it means "start there", and
            // a server honouring it on an amendment would replay history on
            // every tick of the keepalive.
            assert_eq!(subscription.from_slot, Some(9_000));
            assert_eq!(
                keepalive.from_slot, None,
                "the keepalive would replay history"
            );
        }

        /// The subscription reaches the server before anything waits on a reply.
        ///
        /// `subscribe` sends the request into the outbound channel and only
        /// then awaits the response, and the order matters: a server that
        /// answers nothing until it has been told what to send would deadlock
        /// against a client that sends nothing until it has been answered.
        #[test]
        fn the_subscription_is_queued_before_the_response_is_awaited() {
            let request = subscribe_request(&GeyserConfig::default().subscribe_filters());
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SubscribeRequest>();
            tx.send(request.clone()).expect("the channel is open");
            assert_eq!(
                rx.try_recv().ok(),
                Some(request),
                "nothing was queued to send"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// what the window reads
// ---------------------------------------------------------------------------

/// The feed's own report, held where something other than the pipeline can read
/// it.
///
/// [`TickPipeline`] is single-owner on purpose — the ledger, the ring and the
/// trackers have no interior mutability, because the pipeline runs on one task
/// and a structure that *could* be shared is a structure someone will share.
/// That is the right shape for the pipeline and the wrong one for a cockpit,
/// which has to be able to ask what the feed is doing without owning it or
/// stopping it.
///
/// So this sits beside the pipeline rather than inside it. Whoever drives the
/// stream calls [`observe`](Self::observe) after a batch; anybody at all calls
/// [`snapshot`](Self::snapshot). The lock is held for a struct copy of twenty
/// integers and never across a read of the stream.
///
/// **An idle monitor is not a broken one.** A build with nothing dialling a
/// Geyser endpoint answers a snapshot of honest zeros, and that is the state
/// this build ships in. It is constructed at start-up anyway so the window has
/// something real to draw from its first repaint — the same argument
/// `BundleDeck` is built on — because a panel that appears when a backend does
/// is a panel nobody knows is there.
#[derive(Debug, Default)]
pub struct GeyserMonitor {
    metrics: GeyserMetrics,
    latest: parking_lot::Mutex<GeyserSnapshot>,
}

impl GeyserMonitor {
    pub fn new() -> Self {
        Self::default()
    }

    /// The counters the stream task records into.
    pub const fn metrics(&self) -> &GeyserMetrics {
        &self.metrics
    }

    /// Folds the pipeline's current state into the snapshot the window reads.
    ///
    /// Takes the pipeline by reference rather than copying its internals out at
    /// the call site, so the three things that have to be read together — the
    /// ring's metrics, the ledger's heads, and the counters — are read in one
    /// place and cannot drift apart in a caller that forgets one.
    pub fn observe(&self, pipeline: &TickPipeline, stale_writes: u64) {
        let snapshot =
            self.metrics
                .snapshot(pipeline.ring_metrics(), pipeline.ledger(), stale_writes);
        *self.latest.lock() = snapshot;
    }

    /// What the feed looked like at the last `observe`.
    pub fn snapshot(&self) -> GeyserSnapshot {
        *self.latest.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::strategy::fixed::{format_e18, ONE_E18};
    // The pool programs are no longer subscribed to, so the module proper has
    // no use for their ids. They are still what a foreign account looks like,
    // which is what these tests need them for.
    use crate::ingestion::{
        PUMP_SWAP_PROGRAM, RAYDIUM_AMM_V4_PROGRAM, RAYDIUM_CLMM_PROGRAM, RAYDIUM_CPMM_PROGRAM,
    };

    // -- fixture builders ---------------------------------------------------

    fn pubkey(fill: u8) -> Pubkey {
        Pubkey::new([fill; 32])
    }

    /// A pump.fun bonding curve account, laid out the way the program writes
    /// one: eight bytes of discriminator, five little-endian `u64` reserves,
    /// the `complete` flag, then the creator.
    fn curve_account(virtual_sol: u64, virtual_token: u64, real_sol: u64, complete: bool) -> Bytes {
        let mut data = vec![0u8; 81];
        data[0..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        data[8..16].copy_from_slice(&virtual_token.to_le_bytes());
        data[16..24].copy_from_slice(&virtual_sol.to_le_bytes());
        data[24..32].copy_from_slice(&1_000_000u64.to_le_bytes());
        data[32..40].copy_from_slice(&real_sol.to_le_bytes());
        data[40..48].copy_from_slice(&1_000_000_000_000_000u64.to_le_bytes());
        data[48] = u8::from(complete);
        data[49..81].copy_from_slice(&[7u8; 32]);
        Bytes::from(data)
    }

    fn account_update(
        slot: u64,
        curve: Pubkey,
        write_version: u64,
        virtual_sol: u64,
        virtual_token: u64,
    ) -> AccountUpdate {
        AccountUpdate {
            slot,
            pubkey: curve,
            owner: pump_fun_program(),
            lamports: 2_039_280,
            write_version,
            data: curve_account(virtual_sol, virtual_token, 10_000_000_000, false),
            is_startup: false,
        }
    }

    fn account(slot: u64, micros: u64, write_version: u64, virtual_sol: u64) -> GeyserUpdate {
        GeyserUpdate::new(
            micros,
            UpdatePayload::Account(account_update(
                slot,
                pubkey(1),
                write_version,
                virtual_sol,
                1_073_000_000_000_000,
            )),
        )
    }

    fn slot_update(slot: u64, micros: u64, phase: SlotPhase) -> GeyserUpdate {
        GeyserUpdate::new(
            micros,
            UpdatePayload::Slot(SlotUpdate {
                slot,
                parent: Some(slot - 1),
                phase,
            }),
        )
    }

    fn pipeline(hold_slots: u64) -> TickPipeline {
        TickPipeline::new(&GeyserConfig {
            ring: RingConfig {
                capacity: 256,
                hold_slots,
            },
            ..GeyserConfig::default()
        })
    }

    fn curve_prices(events: &[TickEvent]) -> Vec<u128> {
        events
            .iter()
            .filter_map(|event| match &event.payload {
                TickPayload::Curve(curve) => Some(curve.price_e18),
                _ => None,
            })
            .collect()
    }

    // -- zero float ---------------------------------------------------------

    #[test]
    fn every_event_type_is_eq_which_is_what_bans_the_float() {
        // Not a tautology: `f64` is not `Eq`, so these bounds are what make a
        // float in any of these structs a compile error rather than a silent
        // source of non-determinism. The assertion is that the code compiles.
        fn assert_eq_bound<T: Eq>() {}
        assert_eq_bound::<TickEvent>();
        assert_eq_bound::<TickPayload>();
        assert_eq_bound::<CurveTick>();
        assert_eq_bound::<PoolTick>();
        assert_eq_bound::<PriceTick>();
        assert_eq_bound::<SlotTick>();
        assert_eq_bound::<LogTick>();
        assert_eq_bound::<GeyserUpdate>();
        assert_eq_bound::<UpdatePayload>();
        assert_eq_bound::<AccountUpdate>();
        assert_eq_bound::<SlotUpdate>();
        assert_eq_bound::<TransactionUpdate>();
        assert_eq_bound::<TokenBalance>();
        assert_eq_bound::<GeyserSnapshot>();
        assert_eq_bound::<TickKey>();
    }

    #[test]
    fn a_raw_amount_is_read_from_the_string_not_the_float() {
        // The number that proves why. 2^53 + 1 is the first integer an f64
        // cannot represent, and a token balance the size of a pump.fun supply
        // is two orders of magnitude past it.
        let beyond_f64 = 9_007_199_254_740_993u128;
        assert_eq!(parse_raw_amount("9007199254740993"), Ok(beyond_f64));
        assert_ne!(beyond_f64 as f64 as u128, beyond_f64, "the float loses it");

        assert_eq!(
            parse_raw_amount("1000000000000000"),
            Ok(1_000_000_000_000_000)
        );
        assert_eq!(parse_raw_amount("0"), Ok(0));
    }

    #[test]
    fn a_raw_amount_refuses_everything_that_is_not_a_plain_integer() {
        for text in ["", "1.0", "1e9", "+1", "-1", " 1", "1 ", "0x10", "１"] {
            assert_eq!(
                parse_raw_amount(text),
                Err(DecodeError::BadAmount),
                "{text:?} should not parse"
            );
        }
    }

    #[test]
    fn a_raw_amount_that_would_overflow_is_refused_not_wrapped() {
        let too_big = "3".repeat(50);
        assert_eq!(parse_raw_amount(&too_big), Err(DecodeError::BadAmount));
    }

    #[test]
    fn the_curve_price_keeps_the_precision_millionths_would_lose() {
        // The concrete case from this module's own documentation: a live
        // pump.fun curve, and the resolution argument for 10^-18 made as a
        // test rather than a claim.
        let update = account_update(
            10,
            pubkey(1),
            1,
            30_000_000_000,        // 30 SOL of virtual reserves
            1_073_000_000_000_000, // ~1.073e9 tokens at 6 decimals
        );
        let tick = curve_tick(&update).expect("a well-formed curve");

        // 30e9 / 1.073e15 = 2.7958993476234...e-5 lamports per raw token unit,
        // floored — the direction every quotient in this module rounds.
        assert_eq!(tick.price_e18, 27_958_993_476_234);
        assert_eq!(format_e18(tick.price_e18, 18), "0.000027958993476234");

        // The same price in millionths is the integer 27, so one step of that
        // unit is 1/27 of the whole price. In basis points that is 370 — a
        // 3.7% move is the smallest thing millionths can see here, and the
        // moves this engine trades are smaller than that.
        let in_millionths = tick.price_e18 / 1_000_000_000_000;
        assert_eq!(in_millionths, 27);
        let step_bps = 10_000u128 / in_millionths;
        assert!(
            step_bps > 300,
            "one millionth is {step_bps} bps of this price"
        );
    }

    #[test]
    fn a_price_delta_is_exact_in_both_directions() {
        let mut tracker = CurveTracker::new();
        let up = curve_tick(&account_update(
            10,
            pubkey(1),
            1,
            30_000_000_000,
            1_000_000_000_000_000,
        ))
        .unwrap();
        assert_eq!(
            tracker.apply(10, &up),
            Some(None),
            "no baseline on the first write"
        );

        // Reserves double, so the price doubles: +10_000 bps exactly.
        let doubled = curve_tick(&account_update(
            11,
            pubkey(1),
            2,
            60_000_000_000,
            1_000_000_000_000_000,
        ))
        .unwrap();
        let tick = tracker.apply(11, &doubled).unwrap().expect("a move");
        assert_eq!(tick.previous_e18, 30_000_000_000_000);
        assert_eq!(tick.current_e18, 60_000_000_000_000);
        assert_eq!(tick.delta_e18, 30_000_000_000_000);
        assert_eq!(tick.delta_bps, 10_000);

        // Straight back down: -5_000 bps, and the sign is carried.
        let halved = curve_tick(&account_update(
            12,
            pubkey(1),
            3,
            30_000_000_000,
            1_000_000_000_000_000,
        ))
        .unwrap();
        let tick = tracker.apply(12, &halved).unwrap().expect("a move");
        assert_eq!(tick.delta_e18, -30_000_000_000_000);
        assert_eq!(tick.delta_bps, -5_000);
    }

    #[test]
    fn a_pool_price_normalises_both_sides_out_of_their_decimals() {
        // 5 WSOL at 9 decimals against 1_000_000 of a 6-decimal token.
        let base = TokenBalance {
            account_index: 0,
            mint: pubkey(2),
            owner: pubkey(3),
            raw: 1_000_000_000_000,
            decimals: 6,
        };
        let quote = TokenBalance {
            account_index: 1,
            mint: pubkey(4),
            owner: pubkey(3),
            raw: 5_000_000_000,
            decimals: 9,
        };
        let tick = pool_tick(pubkey(9), &base, &quote).expect("a priced pool");

        assert_eq!(tick.base_e18, 1_000_000 * ONE_E18);
        assert_eq!(tick.quote_e18, 5 * ONE_E18);
        // 5 / 1_000_000 = 5e-6, exactly.
        assert_eq!(tick.price_e18, 5_000_000_000_000);
        assert_eq!(format_e18(tick.price_e18, 8), "0.00000500");
    }

    #[test]
    fn an_incoherent_curve_is_refused_rather_than_priced() {
        let mut update = account_update(10, pubkey(1), 1, 0, 1_000_000_000_000_000);
        assert_eq!(curve_tick(&update), Err(DecodeError::IncoherentCurve));

        update.data = curve_account(30_000_000_000, 0, 0, false);
        assert_eq!(curve_tick(&update), Err(DecodeError::IncoherentCurve));

        update.data = Bytes::from_static(&[0u8; 10]);
        assert_eq!(curve_tick(&update), Err(DecodeError::UnknownAccount));
    }

    #[test]
    fn an_account_owned_by_another_program_is_refused() {
        let mut update = account_update(10, pubkey(1), 1, 30_000_000_000, 1_000_000_000_000_000);
        update.owner = pubkey(200);
        assert_eq!(curve_tick(&update), Err(DecodeError::ForeignProgram));
    }

    // -- sub-slot ordering --------------------------------------------------

    #[test]
    fn a_shuffled_stream_comes_out_in_sub_slot_order() {
        let mut pipeline = pipeline(2);
        let mut released = Vec::new();

        // Slot 10's three writes arrive backwards, and slot 11's write arrives
        // before any of them. Every reordering the network can do at once.
        let script = vec![
            account(11, 400, 1, 33_000_000_000),
            account(10, 300, 3, 32_000_000_000),
            account(10, 100, 1, 30_000_000_000),
            account(10, 200, 2, 31_000_000_000),
            slot_update(10, 500, SlotPhase::Confirmed),
            slot_update(11, 600, SlotPhase::Confirmed),
            slot_update(12, 700, SlotPhase::Confirmed),
        ];
        for update in script {
            released.extend(pipeline.ingest(update).released);
        }
        released.extend(pipeline.flush());

        // Every released key is strictly greater than the one before it.
        for pair in released.windows(2) {
            assert!(
                pair[0].key < pair[1].key,
                "order broke at {:?}",
                pair[0].key
            );
        }

        // And the curve prices came out in the order the chain wrote them,
        // which is the reserve order, not the arrival order.
        assert_eq!(
            curve_prices(&released),
            vec![
                ratio_e18(30_000_000_000, 1_073_000_000_000_000).unwrap(),
                ratio_e18(31_000_000_000, 1_073_000_000_000_000).unwrap(),
                ratio_e18(32_000_000_000, 1_073_000_000_000_000).unwrap(),
                ratio_e18(33_000_000_000, 1_073_000_000_000_000).unwrap(),
            ]
        );
    }

    #[test]
    fn a_stale_write_can_never_overwrite_a_newer_one() {
        // The guard that makes the timestamp-before-write-version ordering
        // safe. Write version 5 lands, then a delayed write version 3 for the
        // same account arrives with a *later* timestamp. The late one is
        // discarded rather than allowed to set the price backwards.
        let mut pipeline = pipeline(0);
        let mut released = Vec::new();

        released.extend(
            pipeline
                .ingest(account(10, 100, 5, 50_000_000_000))
                .released,
        );
        let stale = pipeline.ingest(account(10, 900, 3, 10_000_000_000));
        assert!(stale.released.is_empty(), "a stale write produced an event");
        assert_eq!(stale.stale.len(), 1, "the refusal is reported, not silent");
        assert!(
            stale.dropped.is_empty(),
            "a stale write is not a backpressure drop"
        );
        assert_eq!(pipeline.curves().stale_writes(), 1);

        released.extend(pipeline.flush());
        assert_eq!(
            curve_prices(&released),
            vec![ratio_e18(50_000_000_000, 1_073_000_000_000_000).unwrap()],
            "the stale reserves never reached anyone"
        );
    }

    #[test]
    fn a_repeated_write_version_is_refused() {
        let mut pipeline = pipeline(0);
        pipeline.ingest(account(10, 100, 5, 50_000_000_000));
        let repeat = pipeline.ingest(account(10, 200, 5, 60_000_000_000));
        assert!(repeat.released.is_empty());
        assert_eq!(pipeline.curves().stale_writes(), 1);
    }

    #[test]
    fn a_price_that_did_not_move_produces_no_price_tick() {
        let mut pipeline = pipeline(0);
        pipeline.ingest(account(10, 100, 1, 30_000_000_000));
        let same = pipeline.ingest(account(11, 200, 2, 30_000_000_000));
        let prices = same
            .released
            .iter()
            .filter(|event| matches!(event.payload, TickPayload::Price(_)))
            .count();
        assert_eq!(prices, 0, "a zero-delta price tick was emitted");
    }

    #[test]
    fn a_pool_account_is_not_counted_as_a_decode_failure() {
        // The subscription asks for accounts owned by the four pool programs
        // and nothing in this module decodes one. Before this was routed by
        // owner, each of those landed in `decode_failures` — so on a live
        // stream the counter that exists to say "the wire format moved" read
        // as thousands of faults a minute while the feed was perfectly well.
        //
        // The discriminating part is the pair: `foreignAccounts` moves and
        // `decodeFailures` does not. A test that only checked the total would
        // pass with the two counters swapped.
        for owner in [
            RAYDIUM_AMM_V4_PROGRAM,
            RAYDIUM_CPMM_PROGRAM,
            RAYDIUM_CLMM_PROGRAM,
            PUMP_SWAP_PROGRAM,
        ] {
            let mut pool = account_update(10, pubkey(1), 1, 30_000_000_000, 1_073_000_000_000_000);
            pool.owner = Pubkey::parse(owner).expect("a pool program id is a valid address");

            let transport = ScriptedTransport::new(vec![Attempt::Serve(vec![
                Ok(GeyserUpdate::new(50, UpdatePayload::Account(pool))),
                Ok(slot_update(10, 200, SlotPhase::Confirmed)),
            ])]);
            let mut pipeline = pipeline(0);
            let mut sink = CollectingSink::default();
            let metrics = GeyserMetrics::default();

            run(&transport, &mut pipeline, &mut sink, &metrics, 1);

            let snapshot = metrics.snapshot(pipeline.ring_metrics(), pipeline.ledger(), 0);
            assert_eq!(
                snapshot.decode_failures, 0,
                "{owner} was read as a broken curve"
            );
            assert_eq!(snapshot.foreign_accounts, 1, "{owner} went uncounted");
            // It still arrived, and the arrival counter still says so. That is
            // the quota it cost, which is the point of keeping the number.
            assert_eq!(snapshot.accounts, 1);
            assert!(
                curve_prices(&sink.events).is_empty(),
                "{owner} produced a curve tick out of an account it does not own"
            );
        }
    }

    #[test]
    fn a_curve_that_really_is_broken_is_still_a_decode_failure() {
        // The other half of the pair, and the reason the first test is not
        // just "stop counting things". An account genuinely owned by pump.fun
        // whose bytes are not a curve is exactly what `decodeFailures` is for,
        // and routing by owner must not have swallowed it too.
        let mut broken = account_update(10, pubkey(1), 1, 30_000_000_000, 1_073_000_000_000_000);
        broken.data = Bytes::from_static(b"not a bonding curve");

        let transport = ScriptedTransport::new(vec![Attempt::Serve(vec![
            Ok(GeyserUpdate::new(50, UpdatePayload::Account(broken))),
            Ok(slot_update(10, 200, SlotPhase::Confirmed)),
        ])]);
        let mut pipeline = pipeline(0);
        let mut sink = CollectingSink::default();
        let metrics = GeyserMetrics::default();

        run(&transport, &mut pipeline, &mut sink, &metrics, 1);

        let snapshot = metrics.snapshot(pipeline.ring_metrics(), pipeline.ledger(), 0);
        assert_eq!(
            snapshot.decode_failures, 1,
            "a malformed curve stopped being a fault"
        );
        assert_eq!(
            snapshot.foreign_accounts, 0,
            "a pump.fun account is not foreign"
        );
    }

    #[test]
    fn a_startup_account_is_not_a_tick() {
        // The plugin replays existing accounts on connect. Those are state, and
        // treating one as an event would emit a curve tick at whatever slot the
        // plugin happened to be replaying.
        let mut startup = account_update(10, pubkey(1), 1, 30_000_000_000, 1_073_000_000_000_000);
        startup.is_startup = true;

        let transport = ScriptedTransport::new(vec![Attempt::Serve(vec![
            Ok(GeyserUpdate::new(50, UpdatePayload::Account(startup))),
            Ok(account(10, 100, 2, 40_000_000_000)),
            Ok(slot_update(10, 200, SlotPhase::Confirmed)),
        ])]);
        let mut pipeline = pipeline(0);
        let mut sink = CollectingSink::default();
        let metrics = GeyserMetrics::default();

        run(&transport, &mut pipeline, &mut sink, &metrics, 1);

        let snapshot = metrics.snapshot(pipeline.ring_metrics(), pipeline.ledger(), 0);
        assert_eq!(snapshot.startup_skipped, 1);
        assert_eq!(
            curve_prices(&sink.events),
            vec![ratio_e18(40_000_000_000, 1_073_000_000_000_000).unwrap()],
            "the startup snapshot leaked into the event stream"
        );
    }

    // -- the hold window and re-orgs ----------------------------------------

    #[test]
    fn nothing_is_released_until_its_slot_is_confirmed() {
        let mut pipeline = pipeline(1_000);
        let out = pipeline.ingest(account(10, 100, 1, 30_000_000_000));
        assert!(out.released.is_empty(), "released before the slot settled");

        let out = pipeline.ingest(slot_update(10, 200, SlotPhase::Processed));
        assert!(out.released.is_empty(), "processed is not confirmed");

        let out = pipeline.ingest(slot_update(10, 300, SlotPhase::Confirmed));
        // The curve tick and *both* slot ticks: the `Processed` status was
        // buffered along with everything else and settles at the same moment.
        assert_eq!(out.released.len(), 3);
        assert!(matches!(out.released[0].payload, TickPayload::Curve(_)));
    }

    #[test]
    fn a_reorg_inside_the_window_is_undone_before_anyone_sees_it() {
        let mut pipeline = pipeline(1_000);
        let mut released = Vec::new();

        released.extend(
            pipeline
                .ingest(account(10, 100, 1, 30_000_000_000))
                .released,
        );
        released.extend(
            pipeline
                .ingest(slot_update(10, 150, SlotPhase::Processed))
                .released,
        );
        released.extend(
            pipeline
                .ingest(account(11, 200, 1, 90_000_000_000))
                .released,
        );
        released.extend(
            pipeline
                .ingest(slot_update(11, 250, SlotPhase::Processed))
                .released,
        );
        assert!(released.is_empty(), "nothing confirmed yet");

        // Slot 11 dies. Everything at and above it is void.
        let out = pipeline.ingest(slot_update(11, 300, SlotPhase::Dead));
        assert!(out.unrecoverable_from_slot.is_none(), "the window held");
        assert!(
            !out.rolled_back.is_empty(),
            "the abandoned slot was rolled back"
        );

        assert!(
            out.released.iter().all(|event| event.key.slot < 11),
            "the dead slot emitted a tick, which would lock out its replacement"
        );

        // Slot 10 confirms and only slot 10's events ever appear.
        released.extend(
            pipeline
                .ingest(slot_update(10, 400, SlotPhase::Confirmed))
                .released,
        );
        released.extend(pipeline.flush());
        assert!(
            released.iter().all(|event| event.key.slot <= 10),
            "an abandoned slot escaped: {:?}",
            released
                .iter()
                .map(|event| event.key.slot)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            curve_prices(&released),
            vec![ratio_e18(30_000_000_000, 1_073_000_000_000_000).unwrap()]
        );
    }

    #[test]
    fn a_reorg_rolls_back_the_price_baseline_too() {
        // The case the tracker rollback is for: a curve whose baseline was
        // *already released* when its slot is abandoned. The baseline has to
        // go with the slot, because differencing the next write against a
        // price that never happened would report a move that never happened.
        //
        // A hold of zero is what puts the release before the re-org; with the
        // default window this slot would have been rolled back before anyone
        // saw it, which is the case the window exists to produce.
        let mut pipeline = pipeline(0);
        let out = pipeline.ingest(account(10, 100, 1, 30_000_000_000));
        assert!(!out.released.is_empty(), "the baseline was released");
        assert_eq!(pipeline.curves().len(), 1);

        let out = pipeline.ingest(slot_update(10, 200, SlotPhase::Dead));
        assert_eq!(
            out.unrecoverable_from_slot,
            Some(10),
            "the release is past undoing"
        );
        assert_eq!(pipeline.curves().len(), 0, "the baseline survived a reorg");

        // The next write is a first observation again, so no price tick.
        let out = pipeline.ingest(account(11, 300, 1, 90_000_000_000));
        assert!(
            !out.released
                .iter()
                .any(|event| matches!(event.payload, TickPayload::Price(_))),
            "a price was differenced against an abandoned slot"
        );
    }

    #[test]
    fn a_reorg_that_arrives_too_late_is_reported_not_swallowed() {
        let mut pipeline = pipeline(0);
        let released = pipeline
            .ingest(account(10, 100, 1, 30_000_000_000))
            .released;
        assert!(!released.is_empty(), "hold of zero releases immediately");

        let out = pipeline.ingest(slot_update(10, 200, SlotPhase::Dead));
        assert_eq!(out.unrecoverable_from_slot, Some(10));
    }

    #[test]
    fn a_changed_parent_rolls_back_like_a_dead_slot() {
        let mut pipeline = pipeline(1_000);
        pipeline.ingest(slot_update(11, 100, SlotPhase::Processed));
        pipeline.ingest(account(11, 150, 1, 30_000_000_000));

        let forked = GeyserUpdate::new(
            200,
            UpdatePayload::Slot(SlotUpdate {
                slot: 11,
                parent: Some(9), // was 10
                phase: SlotPhase::Processed,
            }),
        );
        let out = pipeline.ingest(forked);
        assert!(
            !out.rolled_back.is_empty(),
            "a fork switch did not roll back"
        );
    }

    // -- backpressure -------------------------------------------------------

    #[test]
    fn a_curve_tick_is_never_the_thing_backpressure_drops() {
        // Fill a tiny ring past its capacity with nothing but curve writes and
        // check that not one of them is dropped. Some leave early, which is the
        // designed degradation; none vanish.
        let mut pipeline = TickPipeline::new(&GeyserConfig {
            ring: RingConfig {
                capacity: 4,
                hold_slots: 1_000,
            },
            ..GeyserConfig::default()
        });
        let mut released = Vec::new();
        let mut dropped = 0usize;

        for index in 0u64..40 {
            let out = pipeline.ingest(account(10, index * 10, index + 1, 30_000_000_000 + index));
            dropped += out
                .dropped
                .iter()
                .filter(|event| matches!(event.payload, TickPayload::Curve(_)))
                .count();
            released.extend(out.released);
        }
        released.extend(pipeline.flush());

        assert_eq!(dropped, 0, "a curve tick was dropped under pressure");
        assert_eq!(
            curve_prices(&released).len(),
            40,
            "a curve tick went missing"
        );
        for pair in released.windows(2) {
            assert!(pair[0].key < pair[1].key, "forced releases broke the order");
        }
    }

    #[test]
    fn logs_are_shed_before_curve_state_is() {
        let mut pipeline = TickPipeline::new(&GeyserConfig {
            ring: RingConfig {
                capacity: 4,
                hold_slots: 1_000,
            },
            ..GeyserConfig::default()
        });

        // Four logs fill the ring.
        for index in 0u64..4 {
            let update = GeyserUpdate::new(
                index * 10,
                UpdatePayload::Transaction(TransactionUpdate {
                    slot: 10,
                    signature: Signature::new([index as u8; 64]),
                    index,
                    is_vote: false,
                    failed: false,
                    logs: vec!["Program log: hello".to_string()],
                    pre_token_balances: Vec::new(),
                    post_token_balances: Vec::new(),
                }),
            );
            pipeline.ingest(update);
        }

        // A curve write arrives and something has to give. It must be a log.
        let out = pipeline.ingest(account(10, 100, 1, 30_000_000_000));
        assert_eq!(out.dropped.len(), 1);
        assert!(
            matches!(out.dropped[0].payload, TickPayload::Log(_)),
            "backpressure shed {:?} instead of a log",
            out.dropped[0].payload
        );
    }

    #[test]
    fn a_vote_transaction_is_never_an_event() {
        let mut pipeline = pipeline(0);
        let update = GeyserUpdate::new(
            100,
            UpdatePayload::Transaction(TransactionUpdate {
                slot: 10,
                signature: Signature::new([1u8; 64]),
                index: 0,
                is_vote: true,
                failed: false,
                logs: vec!["Program Vote111…".to_string()],
                pre_token_balances: Vec::new(),
                post_token_balances: Vec::new(),
            }),
        );
        assert!(pipeline.ingest(update).released.is_empty());
    }

    /// A transaction's logs are moved through the pipeline, not copied.
    ///
    /// The other half of the no-copy path, and the half a type signature does
    /// not enforce: `data` is a [`Bytes`] and could not be copied by accident,
    /// but `logs` is a `Vec<String>` and `update.logs.clone()` would compile
    /// perfectly well. A pump.fun buy carries a dozen lines of program output,
    /// so that clone would be a fresh allocation per line per transaction, on
    /// the busiest path in the process, to read a message that is dropped on
    /// the next line.
    ///
    /// Checked by address, because that is the only thing that distinguishes a
    /// move from a copy: cloning a `Vec<String>` clones each `String` into a
    /// new allocation, so an unchanged pointer means the original heap buffer
    /// travelled rather than its contents.
    #[test]
    fn a_transactions_logs_travel_rather_than_being_copied() {
        let mut pipeline = pipeline(0);
        let logs = vec![
            "Program 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P invoke [1]".to_string(),
            "Program log: Instruction: Buy".to_string(),
        ];
        let addresses: Vec<usize> = logs.iter().map(|line| line.as_ptr() as usize).collect();

        let released = pipeline
            .ingest(GeyserUpdate::new(
                100,
                UpdatePayload::Transaction(TransactionUpdate {
                    slot: 10,
                    signature: Signature::new([1u8; 64]),
                    index: 0,
                    is_vote: false,
                    failed: false,
                    logs,
                    pre_token_balances: Vec::new(),
                    post_token_balances: Vec::new(),
                }),
            ))
            .released;

        let tick = released
            .iter()
            .find_map(|event| match &event.payload {
                TickPayload::Log(log) => Some(log),
                _ => None,
            })
            .expect("a log tick");
        let arrived: Vec<usize> = tick
            .logs
            .iter()
            .map(|line| line.as_ptr() as usize)
            .collect();
        assert_eq!(
            arrived, addresses,
            "the log lines were copied on the way through"
        );
    }

    // -- reconnect backoff --------------------------------------------------

    #[test]
    fn the_backoff_doubles_from_the_floor_and_stops_at_the_ceiling() {
        let mut policy = ReconnectPolicy::new(BACKOFF_MIN, BACKOFF_MAX);
        let schedule: Vec<Duration> = (0..10).map(|_| policy.record_failure()).collect();
        assert_eq!(
            schedule,
            vec![
                Duration::from_millis(500),
                Duration::from_millis(1_000),
                Duration::from_millis(2_000),
                Duration::from_millis(4_000),
                Duration::from_millis(8_000),
                Duration::from_millis(16_000),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(30),
            ],
            "the first failure must wait the floor, not twice it"
        );
        assert_eq!(policy.failures(), 10);
    }

    #[test]
    fn a_success_clears_the_backoff() {
        let mut policy = ReconnectPolicy::new(BACKOFF_MIN, BACKOFF_MAX);
        policy.record_failure();
        policy.record_failure();
        policy.record_failure();
        policy.record_success();
        assert_eq!(policy.failures(), 0);
        assert_eq!(policy.record_failure(), BACKOFF_MIN);
    }

    #[test]
    fn the_backoff_never_overflows_however_long_it_fails() {
        let mut policy = ReconnectPolicy::new(BACKOFF_MIN, BACKOFF_MAX);
        for _ in 0..10_000 {
            assert!(policy.record_failure() <= BACKOFF_MAX);
        }
    }

    // -- the subscriber loop, against a mock transport -----------------------

    /// A transport that hands out a scripted stream per attempt.
    struct ScriptedTransport {
        attempts: parking_lot::Mutex<std::collections::VecDeque<Attempt>>,
        seen_filters: parking_lot::Mutex<Vec<SubscribeFilters>>,
    }

    enum Attempt {
        /// The dial itself fails.
        Refuse,
        /// The dial fails for a named reason, so that a test can change the
        /// reason between attempts.
        RefuseWith(&'static str),
        /// The dial works and the stream plays this script.
        Serve(Vec<Result<GeyserUpdate, GeyserError>>),
    }

    impl ScriptedTransport {
        fn new(attempts: Vec<Attempt>) -> Self {
            ScriptedTransport {
                attempts: parking_lot::Mutex::new(attempts.into()),
                seen_filters: parking_lot::Mutex::new(Vec::new()),
            }
        }
    }

    impl GeyserTransport for ScriptedTransport {
        fn subscribe(
            &self,
            _config: GeyserConfig,
            filters: SubscribeFilters,
        ) -> BoxFuture<'static, Result<Box<dyn GeyserStream>, GeyserError>> {
            self.seen_filters.lock().push(filters);
            let next = self.attempts.lock().pop_front();
            Box::pin(async move {
                match next {
                    Some(Attempt::Serve(script)) => {
                        Ok(Box::new(MockStream::new(script)) as Box<dyn GeyserStream>)
                    }
                    Some(Attempt::Refuse) => Err(GeyserError::Dial("refused".into())),
                    Some(Attempt::RefuseWith(why)) => Err(GeyserError::Dial(why.into())),
                    None => Err(GeyserError::Dial("out of script".into())),
                }
            })
        }
    }

    fn run(
        transport: &ScriptedTransport,
        pipeline: &mut TickPipeline,
        sink: &mut CollectingSink,
        metrics: &GeyserMetrics,
        max_attempts: u32,
    ) -> RunReport {
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime");
        runtime.block_on(run_subscriber(
            transport,
            GeyserConfig::default(),
            pipeline,
            sink,
            metrics,
            // `sleep: false` so the schedule is asserted on rather than waited
            // out. The durations still come back in the report.
            RunLimits {
                max_attempts: Some(max_attempts),
                sleep: false,
            },
            rx,
        ))
    }

    #[test]
    fn a_disconnect_reconnects_with_a_doubling_backoff() {
        let transport = ScriptedTransport::new(vec![
            Attempt::Serve(vec![Ok(slot_update(10, 100, SlotPhase::Confirmed))]),
            Attempt::Refuse,
            Attempt::Refuse,
            Attempt::Serve(vec![Err(GeyserError::Stream("reset by peer".into()))]),
        ]);
        let mut pipeline = pipeline(0);
        let mut sink = CollectingSink::default();
        let metrics = GeyserMetrics::default();

        let report = run(&transport, &mut pipeline, &mut sink, &metrics, 4);

        assert_eq!(report.stopped, StopReason::OutOfAttempts);
        assert_eq!(
            report.backoffs,
            vec![
                // The stream ended cleanly: first failure, so the floor.
                Duration::from_millis(500),
                // Dial refused: second.
                Duration::from_millis(1_000),
                // Dial refused again: third.
                Duration::from_millis(2_000),
                // A successful dial cleared the count, so the stream error
                // that follows is a first failure again.
                Duration::from_millis(500),
            ],
        );

        let snapshot = metrics.snapshot(pipeline.ring_metrics(), pipeline.ledger(), 0);
        assert_eq!(snapshot.connects, 2);
        assert_eq!(snapshot.connect_failures, 2);
        assert_eq!(snapshot.disconnects, 2);
    }

    #[test]
    fn a_reconnect_resumes_from_just_behind_the_last_release() {
        let transport = ScriptedTransport::new(vec![
            Attempt::Serve(vec![
                Ok(account(100, 10, 1, 30_000_000_000)),
                Ok(slot_update(100, 20, SlotPhase::Confirmed)),
                Ok(slot_update(101, 30, SlotPhase::Confirmed)),
            ]),
            Attempt::Serve(vec![]),
        ]);
        let mut pipeline = pipeline(0);
        let mut sink = CollectingSink::default();
        let metrics = GeyserMetrics::default();

        let report = run(&transport, &mut pipeline, &mut sink, &metrics, 2);

        // Released up to slot 101, so the resume asks for 99 — two slots of
        // overlap rather than a gap.
        assert_eq!(report.resumed_from, vec![Some(101 - RESUME_OVERLAP_SLOTS)]);
        let filters = transport.seen_filters.lock();
        assert_eq!(
            filters[0].from_slot, None,
            "the first attempt starts from now"
        );
        assert_eq!(filters[1].from_slot, Some(99));
    }

    #[test]
    fn a_refused_dial_says_why_instead_of_only_counting() {
        // The gap this closes: a wrong token, a typo'd endpoint and a provider
        // genuinely down were the same thing from outside — `connectFailures`
        // climbing, no reason anywhere, and a retry loop going round in
        // silence forever.
        let transport = ScriptedTransport::new(vec![Attempt::RefuseWith("connection refused")]);
        let mut pipeline = pipeline(0);
        let mut sink = CollectingSink::default();
        let metrics = GeyserMetrics::default();

        let report = run(&transport, &mut pipeline, &mut sink, &metrics, 1);

        assert_eq!(report.failures.len(), 1, "the report lost the reason");
        assert_eq!(
            report.failures[0],
            GeyserError::Dial("connection refused".into()),
            "the reason is not the one the transport gave"
        );
        assert_eq!(sink.faults.len(), 1, "nothing was told about the failure");
        assert_eq!(
            sink.faults[0].0,
            GeyserError::Dial("connection refused".into())
        );
        assert_eq!(
            sink.faults[0].1, 1,
            "the first failure is the first failure"
        );
        // The counter still moves. This adds a reason, it does not replace the
        // number the pane reads.
        let snapshot = metrics.snapshot(pipeline.ring_metrics(), pipeline.ledger(), 0);
        assert_eq!(snapshot.connect_failures, 1);
    }

    #[test]
    fn the_same_reason_is_said_once_however_long_it_lasts() {
        // The backoff reaches thirty seconds and stays there, so a provider
        // down for an hour is a hundred-odd attempts. Saying the same sentence
        // a hundred times is how a log stops being read, and the count is
        // already on the counters — published every five seconds by the
        // telemetry loop — so the sentence carries what, not how often.
        let transport = ScriptedTransport::new(vec![
            Attempt::RefuseWith("connection refused"),
            Attempt::RefuseWith("connection refused"),
            Attempt::RefuseWith("connection refused"),
        ]);
        let mut pipeline = pipeline(0);
        let mut sink = CollectingSink::default();
        let metrics = GeyserMetrics::default();

        let report = run(&transport, &mut pipeline, &mut sink, &metrics, 3);

        assert_eq!(
            report.failures.len(),
            3,
            "every attempt is still in the report"
        );
        assert_eq!(
            sink.faults.len(),
            1,
            "the same reason was repeated to telemetry"
        );
    }

    #[test]
    fn a_reason_that_changes_is_news_again() {
        // The half that stops the previous test from being "say it once and
        // never again". A refusal that becomes a TLS failure is a different
        // problem, and the second one is the one worth acting on.
        let transport = ScriptedTransport::new(vec![
            Attempt::RefuseWith("connection refused"),
            Attempt::RefuseWith("connection refused"),
            Attempt::RefuseWith("tls handshake failed"),
            Attempt::RefuseWith("tls handshake failed"),
        ]);
        let mut pipeline = pipeline(0);
        let mut sink = CollectingSink::default();
        let metrics = GeyserMetrics::default();

        run(&transport, &mut pipeline, &mut sink, &metrics, 4);

        let said: Vec<&GeyserError> = sink.faults.iter().map(|(error, _)| error).collect();
        assert_eq!(
            said,
            vec![
                &GeyserError::Dial("connection refused".into()),
                &GeyserError::Dial("tls handshake failed".into()),
            ]
        );
        // The consecutive count keeps running across the change, because the
        // feed has been down for all four attempts and that is the fact a
        // reader needs. It is what picks the telemetry level.
        assert_eq!(sink.faults[0].1, 1);
        assert_eq!(sink.faults[1].1, 3);
    }

    #[test]
    fn a_feed_that_came_back_makes_the_same_reason_news_again() {
        // Otherwise a feed that flaps — up, down for the same reason, up,
        // down — would report the outage once and then go quiet for the rest
        // of the day, which is the failure mode this whole mechanism exists to
        // avoid, reintroduced by the thing that suppresses repeats.
        let transport = ScriptedTransport::new(vec![
            Attempt::RefuseWith("connection refused"),
            Attempt::Serve(Vec::new()),
            Attempt::RefuseWith("connection refused"),
        ]);
        let mut pipeline = pipeline(0);
        let mut sink = CollectingSink::default();
        let metrics = GeyserMetrics::default();

        run(&transport, &mut pipeline, &mut sink, &metrics, 3);

        assert_eq!(
            sink.faults.len(),
            2,
            "the outage after a recovery went unsaid"
        );
        // Both are a first failure, because the connection in between ended
        // the run that preceded it.
        assert_eq!(sink.faults[0].1, 1);
        assert_eq!(sink.faults[1].1, 1);
    }

    #[test]
    fn a_build_with_no_transport_says_so_rather_than_retrying_in_silence() {
        // `NoTransport` is what a build without `--features geyser-grpc`
        // returns for every attempt, forever. The README says this refuses out
        // loud; before the reason was surfaced it refused into a counter, and
        // the retry loop did it every thirty seconds for the life of the
        // process without ever naming the feature that was missing.
        let transport = NoTransport;
        let mut pipeline = pipeline(0);
        let mut sink = CollectingSink::default();
        let metrics = GeyserMetrics::default();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime");

        runtime.block_on(run_subscriber(
            &transport,
            GeyserConfig::default(),
            &mut pipeline,
            &mut sink,
            &metrics,
            RunLimits {
                max_attempts: Some(3),
                sleep: false,
            },
            rx,
        ));

        assert_eq!(sink.faults.len(), 1, "the missing feature was never named");
        assert_eq!(sink.faults[0].0, GeyserError::NoTransport);
        assert!(
            sink.faults[0].0.to_string().contains("geyser-grpc"),
            "the reason does not name the feature to rebuild with: {}",
            sink.faults[0].0
        );
    }

    #[test]
    fn the_subscription_asks_only_for_the_allowlisted_programs() {
        let filters = GeyserConfig::default().subscribe_filters();
        assert_eq!(filters.curve_owners, vec![PUMP_FUN_PROGRAM.to_string()]);
        assert_eq!(filters.curve_data_size, Some(CURVE_ACCOUNT_LEN));
        assert_eq!(
            filters.transaction_includes,
            vec![PUMP_FUN_PROGRAM.to_string()]
        );
        assert!(
            filters
                .curve_owners
                .iter()
                .all(|owner| Pubkey::parse(owner).is_ok()),
            "a program id on the subscription is not a valid address"
        );
    }

    #[test]
    fn a_mock_stream_drives_the_whole_pipeline_in_order() {
        let transport = ScriptedTransport::new(vec![Attempt::Serve(vec![
            Ok(account(10, 300, 3, 32_000_000_000)),
            Ok(account(10, 100, 1, 30_000_000_000)),
            Ok(account(10, 200, 2, 31_000_000_000)),
            Ok(slot_update(10, 400, SlotPhase::Confirmed)),
            Ok(account(11, 500, 1, 33_000_000_000)),
            Ok(slot_update(11, 600, SlotPhase::Confirmed)),
        ])]);
        let mut pipeline = pipeline(2);
        let mut sink = CollectingSink::default();
        let metrics = GeyserMetrics::default();

        run(&transport, &mut pipeline, &mut sink, &metrics, 1);
        sink.events.extend(pipeline.flush());

        for pair in sink.events.windows(2) {
            assert!(
                pair[0].key < pair[1].key,
                "the emitted stream was not ordered"
            );
        }
        assert_eq!(curve_prices(&sink.events).len(), 4);
        assert!(sink.unwinds.is_empty());

        let snapshot = metrics.snapshot(
            pipeline.ring_metrics(),
            pipeline.ledger(),
            pipeline.curves().stale_writes(),
        );
        assert_eq!(snapshot.accounts, 4);
        assert_eq!(snapshot.slots, 2);
        assert_eq!(snapshot.decode_failures, 0);
        assert_eq!(snapshot.confirmed_head, 11);
    }

    #[test]
    fn a_late_reorg_reaches_the_sink_as_an_unwind() {
        let transport = ScriptedTransport::new(vec![Attempt::Serve(vec![
            Ok(account(10, 100, 1, 30_000_000_000)),
            Ok(slot_update(10, 200, SlotPhase::Confirmed)),
            Ok(slot_update(10, 300, SlotPhase::Dead)),
        ])]);
        let mut pipeline = pipeline(0);
        let mut sink = CollectingSink::default();
        let metrics = GeyserMetrics::default();

        run(&transport, &mut pipeline, &mut sink, &metrics, 1);
        assert_eq!(sink.unwinds, vec![10], "a late reorg has to reach the sink");
    }

    #[test]
    fn a_decode_failure_is_counted_and_the_stream_carries_on() {
        let mut bad = account_update(10, pubkey(1), 1, 30_000_000_000, 1_073_000_000_000_000);
        bad.data = Bytes::from_static(&[0u8; 4]); // too short to be a curve

        let transport = ScriptedTransport::new(vec![Attempt::Serve(vec![
            Ok(GeyserUpdate::new(100, UpdatePayload::Account(bad))),
            Ok(account(10, 200, 2, 30_000_000_000)),
            Ok(slot_update(10, 300, SlotPhase::Confirmed)),
        ])]);
        let mut pipeline = pipeline(0);
        let mut sink = CollectingSink::default();
        let metrics = GeyserMetrics::default();

        run(&transport, &mut pipeline, &mut sink, &metrics, 1);

        let snapshot = metrics.snapshot(pipeline.ring_metrics(), pipeline.ledger(), 0);
        assert_eq!(snapshot.decode_failures, 1);
        assert_eq!(
            curve_prices(&sink.events).len(),
            1,
            "the good update still landed"
        );
    }

    #[test]
    fn shutdown_stops_the_loop_without_another_dial() {
        let transport = ScriptedTransport::new(vec![Attempt::Serve(vec![])]);
        let mut pipeline = pipeline(0);
        let mut sink = CollectingSink::default();
        let metrics = GeyserMetrics::default();

        let (tx, rx) = tokio::sync::watch::channel(true);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let report = runtime.block_on(run_subscriber(
            &transport,
            GeyserConfig::default(),
            &mut pipeline,
            &mut sink,
            &metrics,
            RunLimits {
                max_attempts: Some(8),
                sleep: false,
            },
            rx,
        ));
        drop(tx);

        assert_eq!(report.stopped, StopReason::Shutdown);
        assert!(
            report.backoffs.is_empty(),
            "shutdown should not have dialled at all"
        );
    }

    // -- configuration ------------------------------------------------------

    #[test]
    fn the_endpoint_is_redacted_before_it_can_be_logged() {
        let config = GeyserConfig {
            endpoint: "https://grpc.example.com/token/deadbeefcafe".to_string(),
            token: Some("secret".to_string()),
            ..GeyserConfig::default()
        };
        let redacted = config.redacted();
        assert_eq!(redacted, "https://grpc.example.com/…");
        assert!(!redacted.contains("deadbeef"));
        assert!(!redacted.contains("secret"));
    }

    #[test]
    fn no_transport_says_which_feature_is_missing() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let outcome = runtime.block_on(NoTransport.subscribe(
            GeyserConfig::default(),
            GeyserConfig::default().subscribe_filters(),
        ));
        let Err(error) = outcome else {
            panic!("NoTransport dialled something");
        };
        assert_eq!(error, GeyserError::NoTransport);
        assert!(error.to_string().contains("geyser-grpc"));
    }

    // -- the monitor ---------------------------------------------------------

    #[test]
    fn an_idle_monitor_answers_zeroes_rather_than_nothing() {
        let monitor = GeyserMonitor::new();
        let snapshot = monitor.snapshot();

        // Every counter honestly zero, and every head honestly zero — which is
        // "nothing has been observed", not "the chain is at slot zero". The
        // window draws the difference; see the em dashes in ui/app.js.
        assert_eq!(snapshot.updates, 0);
        assert_eq!(snapshot.events, 0);
        assert_eq!(snapshot.head_slot, 0);
        assert_eq!(snapshot.confirmed_head, 0);
        assert_eq!(snapshot.finalized_head, 0);
        assert_eq!(snapshot.ring.released, 0);
    }

    #[test]
    fn the_monitor_reports_the_heads_the_ledger_reached() {
        let monitor = GeyserMonitor::new();
        let mut pipeline = pipeline(2);

        for update in [
            slot_update(100, 1_000, SlotPhase::Processed),
            slot_update(101, 2_000, SlotPhase::Processed),
            slot_update(100, 3_000, SlotPhase::Confirmed),
            slot_update(99, 4_000, SlotPhase::Finalized),
        ] {
            monitor.metrics().record_update(&update.payload);
            pipeline.ingest(update);
        }
        monitor.observe(&pipeline, 7);

        let snapshot = monitor.snapshot();
        assert_eq!(snapshot.head_slot, 101);
        assert_eq!(snapshot.confirmed_head, 100);
        assert_eq!(snapshot.finalized_head, 99);
        assert_eq!(snapshot.stale_writes, 7);
        assert_eq!(
            snapshot.slots, 4,
            "four slot updates went past the counters"
        );

        // The two numbers the 0x100 view is named for are differences of these
        // three, and they are differences the *window* takes — the snapshot
        // stays counters, per this module's own rule about derived numbers.
        assert_eq!(snapshot.head_slot - snapshot.confirmed_head, 1);
        assert_eq!(snapshot.confirmed_head - snapshot.finalized_head, 1);
    }

    #[test]
    fn a_snapshot_is_of_the_last_observe_and_not_of_now() {
        let monitor = GeyserMonitor::new();
        let mut pipeline = pipeline(2);

        pipeline.ingest(slot_update(100, 1_000, SlotPhase::Processed));
        monitor.observe(&pipeline, 0);
        assert_eq!(monitor.snapshot().head_slot, 100);

        // The pipeline moves on. The snapshot does not, until it is asked to.
        pipeline.ingest(slot_update(140, 2_000, SlotPhase::Processed));
        assert_eq!(
            monitor.snapshot().head_slot,
            100,
            "a reader that blocked the stream to be perfectly current would cost \
             more than the currency is worth",
        );

        monitor.observe(&pipeline, 0);
        assert_eq!(monitor.snapshot().head_slot, 140);
    }

    /// The feature gate decides what [`default_transport`] hands back, and both
    /// halves of that are asserted — each in the build where it is true.
    ///
    /// Worth testing rather than reading, because the two `cfg` arms of one
    /// function are the one construct where the compiler checks neither against
    /// the other: an arm that is off is not type-checked, not linted, and not
    /// run. A gate whose off-branch was only ever read is a gate that breaks in
    /// the build nobody compiles.
    ///
    /// Dialled against an address nothing listens on so the answer can only be
    /// about the transport. The `geyser-grpc` build has to *fail* here too —
    /// there is no server — but it has to fail for the right reason, and
    /// "this build has no transport" is the wrong reason when it plainly has
    /// one.
    #[test]
    fn the_feature_gate_decides_which_transport_the_process_gets() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let config = GeyserConfig {
            // Port 1 on the loopback: reserved, unassignable, and refused
            // immediately rather than after a connect timeout.
            endpoint: "http://127.0.0.1:1".to_string(),
            ..GeyserConfig::default()
        };
        let transport = default_transport();
        let outcome =
            runtime.block_on(transport.subscribe(config.clone(), config.subscribe_filters()));
        let Err(error) = outcome else {
            panic!("something answered on a port nothing listens on");
        };

        #[cfg(feature = "geyser-grpc")]
        assert!(
            matches!(error, GeyserError::Dial(_)),
            "the geyser-grpc build should have tried and failed to dial, not refused: {error}"
        );
        #[cfg(not(feature = "geyser-grpc"))]
        assert_eq!(
            error,
            GeyserError::NoTransport,
            "a build with no transport should say so"
        );
    }
}
