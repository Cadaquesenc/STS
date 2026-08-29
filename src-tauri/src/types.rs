//! The vocabulary the engine argues in.
//!
//! Everything here is a value: no locks, no I/O, no clock reads. That is what
//! makes the rules in this file testable without standing up a database or a
//! socket, and it is why the state machine lives here rather than next to the
//! code that sends transactions.
//!
//! Two ideas run through the whole file.
//!
//! The first is that **getting out is never gated**. An execution can always be
//! aborted from any state where it is still running, and `RiskSnapshot` will
//! refuse a new position for a dozen reasons but never refuses to close one.
//! Every limit in here is a limit on entering. That asymmetry is deliberate: a
//! risk check that can lock the engine out of an exit is worse than no risk
//! check at all.
//!
//! The second is that **money is counted in integers**. Lamports as `u64`,
//! everything proportional in basis points (10_000 = 100%). Nothing that
//! decides a position size is an `f32`, because two runs of the same numbers
//! have to agree, and floats do not reliably agree with themselves.
//!
//! Addresses are fixed byte arrays rather than strings. A `Pubkey` is what the
//! chain actually gives us and what a hash map wants as a key; the base58 text
//! is a rendering for people, produced only when something is on its way to the
//! UI or a log line.

use std::fmt;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// base58
// ---------------------------------------------------------------------------

/// Base58 for fixed-size keys, without allocating.
///
/// This exists instead of a dependency because it is forty lines and the two
/// sizes that matter here are known at compile time. Both directions work on
/// caller-provided buffers, so decoding a wallet address in the middle of a
/// launch burst does not touch the allocator.
mod base58 {
    /// Bitcoin's alphabet, which is what Solana uses. `0`, `O`, `I` and `l` are
    /// missing on purpose — they are the pairs people misread.
    const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

    /// Reverse lookup, built at compile time. `0xff` marks a byte that is not a
    /// base58 digit, which is every byte the alphabet does not mention.
    const DIGIT: [u8; 128] = {
        let mut table = [0xffu8; 128];
        let mut i = 0;
        while i < 58 {
            table[ALPHABET[i] as usize] = i as u8;
            i += 1;
        }
        table
    };

    /// The longest input this module handles, in bytes: a 64-byte signature.
    const MAX_INPUT: usize = 64;
    /// Enough base58 digits for `MAX_INPUT` bytes — 64 * log(256)/log(58) is
    /// 87.4, so 88 is the true maximum and 96 is a comfortable margin.
    const SCRATCH: usize = 96;

    /// Writes the base58 form of `bytes` into `out`.
    ///
    /// Returns how many bytes of `out` were used, or `None` if `out` was too
    /// small or `bytes` was longer than this module handles. Never panics and
    /// never writes a partial answer, because the callers are `Display` impls
    /// on a path that a panic hook would turn into a halted engine.
    pub fn encode(bytes: &[u8], out: &mut [u8]) -> Option<usize> {
        if bytes.len() > MAX_INPUT {
            return None;
        }

        // Long division by 58, keeping the digits little-endian in `scratch`.
        let mut scratch = [0u8; SCRATCH];
        let mut len = 0usize;
        for &byte in bytes {
            let mut carry = byte as u32;
            for digit in scratch[..len].iter_mut() {
                carry += (*digit as u32) << 8;
                *digit = (carry % 58) as u8;
                carry /= 58;
            }
            while carry > 0 {
                if len == SCRATCH {
                    return None;
                }
                scratch[len] = (carry % 58) as u8;
                carry /= 58;
                len += 1;
            }
        }

        // Leading zero bytes carry no magnitude, so the loop above dropped them.
        // Base58 puts them back as leading `1`s, which is why an all-zero key
        // renders as a run of ones rather than as nothing at all.
        let zeros = bytes.iter().take_while(|&&b| b == 0).count();
        let total = zeros + len;
        if out.len() < total {
            return None;
        }
        for slot in out[..zeros].iter_mut() {
            *slot = ALPHABET[0];
        }
        for (i, digit) in scratch[..len].iter().rev().enumerate() {
            out[zeros + i] = ALPHABET[*digit as usize];
        }
        Some(total)
    }

    /// Parses base58 text into exactly `N` bytes.
    ///
    /// Length is checked against `N` rather than merely against overflow, so a
    /// truncated address cannot quietly decode to a valid-looking key with a
    /// few zero bytes on the front.
    pub fn decode<const N: usize>(text: &str) -> Result<[u8; N], Base58Error> {
        let text = text.as_bytes();
        if text.is_empty() || text.len() > SCRATCH {
            return Err(Base58Error::Length);
        }

        let mut num = [0u8; N]; // little-endian, significant bytes only
        let mut len = 0usize;
        for &c in text {
            let digit = match DIGIT.get(c as usize) {
                Some(&d) if d != 0xff => d as u32,
                _ => return Err(Base58Error::Alphabet),
            };
            let mut carry = digit;
            for byte in num[..len].iter_mut() {
                carry += 58 * (*byte as u32);
                *byte = carry as u8;
                carry >>= 8;
            }
            while carry > 0 {
                if len == N {
                    return Err(Base58Error::Length);
                }
                num[len] = carry as u8;
                carry >>= 8;
                len += 1;
            }
        }

        let zeros = text.iter().take_while(|&&c| c == ALPHABET[0]).count();
        if zeros + len != N {
            return Err(Base58Error::Length);
        }

        let mut out = [0u8; N];
        for (i, byte) in num[..len].iter().rev().enumerate() {
            out[zeros + i] = *byte;
        }
        Ok(out)
    }

    /// Why a string is not the key it was supposed to be.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Base58Error {
        /// A character outside the base58 alphabet.
        Alphabet,
        /// Decoded to the wrong number of bytes.
        Length,
    }

    impl core::fmt::Display for Base58Error {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            match self {
                Base58Error::Alphabet => {
                    f.write_str("not base58: contains 0, O, I, l or punctuation")
                }
                Base58Error::Length => f.write_str("not the right length for this kind of key"),
            }
        }
    }

    impl std::error::Error for Base58Error {}
}

pub use base58::Base58Error;

/// The longest base58 rendering of 32 bytes.
const PUBKEY_TEXT_MAX: usize = 44;
/// The longest base58 rendering of 64 bytes.
const SIGNATURE_TEXT_MAX: usize = 88;

/// A Solana account address.
///
/// The raw 32 bytes, because that is what comes off the wire and what a map
/// keyed by wallet wants. Text is produced on demand at the edges.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pubkey([u8; 32]);

impl Pubkey {
    /// The all-zero address — the System Program, and also what an uninitialised
    /// field looks like, which is why `is_zero` is worth checking before trusting
    /// a decoded creator wallet.
    pub const ZERO: Pubkey = Pubkey([0u8; 32]);

    pub const fn new(bytes: [u8; 32]) -> Self {
        Pubkey(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub const fn is_zero(&self) -> bool {
        // No `iter` in const fn, so this is the honest long way round.
        let mut i = 0;
        while i < 32 {
            if self.0[i] != 0 {
                return false;
            }
            i += 1;
        }
        true
    }

    /// Parses the base58 form. Borrows the input and allocates nothing.
    pub fn parse(text: &str) -> Result<Self, Base58Error> {
        base58::decode::<32>(text).map(Pubkey)
    }
}

impl fmt::Display for Pubkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = [0u8; PUBKEY_TEXT_MAX];
        let len = base58::encode(&self.0, &mut buf).ok_or(fmt::Error)?;
        f.write_str(std::str::from_utf8(&buf[..len]).map_err(|_| fmt::Error)?)
    }
}

impl fmt::Debug for Pubkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // A wall of 32 numbers in a log line helps nobody.
        write!(f, "Pubkey({self})")
    }
}

impl Serialize for Pubkey {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// A transaction signature: the receipt for something that was actually sent.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Signature([u8; 64]);

impl Signature {
    pub const fn new(bytes: [u8; 64]) -> Self {
        Signature(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 64] {
        &self.0
    }

    pub fn parse(text: &str) -> Result<Self, Base58Error> {
        base58::decode::<64>(text).map(Signature)
    }
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = [0u8; SIGNATURE_TEXT_MAX];
        let len = base58::encode(&self.0, &mut buf).ok_or(fmt::Error)?;
        f.write_str(std::str::from_utf8(&buf[..len]).map_err(|_| fmt::Error)?)
    }
}

impl fmt::Debug for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Signature({self})")
    }
}

impl Serialize for Signature {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

// ---------------------------------------------------------------------------
// execution state machine
// ---------------------------------------------------------------------------

/// Where one execution has got to.
///
/// One order, one trip through this enum. The forward path is walked a step at
/// a time and cannot be skipped — a `Sent` that was never `Validated` means the
/// risk gate was bypassed, and that is a bug worth failing loudly on rather
/// than a shortcut worth allowing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ExecutionState {
    /// The EV engine wants to do something. Nothing has been checked yet.
    IntentCreated,
    /// Risk, size and liquidity all said yes. Nothing has been sent yet.
    Validated,
    /// A transaction is on the network. This is the first state that cannot be
    /// undone by deciding otherwise.
    Sent,
    /// The transaction landed. There is now a position.
    Confirmed,
    /// The position is closed and booked. Terminal.
    Completed,
    /// Given up on. Terminal, and reachable from every state above.
    Aborted,
}

impl ExecutionState {
    /// Every state, for exhaustive walks and tests.
    pub const ALL: [ExecutionState; 6] = [
        ExecutionState::IntentCreated,
        ExecutionState::Validated,
        ExecutionState::Sent,
        ExecutionState::Confirmed,
        ExecutionState::Completed,
        ExecutionState::Aborted,
    ];

    /// Nothing more happens to an execution in a terminal state.
    pub const fn is_terminal(self) -> bool {
        matches!(self, ExecutionState::Completed | ExecutionState::Aborted)
    }

    /// Still running, and therefore still abortable.
    pub const fn is_active(self) -> bool {
        !self.is_terminal()
    }

    /// True once funds are committed to something the engine cannot recall.
    ///
    /// This is the difference between abandoning a plan and abandoning a
    /// position. Aborting before `Sent` costs nothing; aborting after it leaves
    /// something on-chain that still has to be dealt with.
    pub const fn has_money_at_risk(self) -> bool {
        matches!(self, ExecutionState::Sent | ExecutionState::Confirmed)
    }

    /// The name written to `audit_log` and read back from it.
    pub const fn as_str(self) -> &'static str {
        match self {
            ExecutionState::IntentCreated => "intent_created",
            ExecutionState::Validated => "validated",
            ExecutionState::Sent => "sent",
            ExecutionState::Confirmed => "confirmed",
            ExecutionState::Completed => "completed",
            ExecutionState::Aborted => "aborted",
        }
    }

    /// Reads back what `as_str` wrote. `None` for anything else, because a row
    /// with a state this build does not know about is not a state to guess at.
    pub fn parse(text: &str) -> Option<Self> {
        ExecutionState::ALL.into_iter().find(|s| s.as_str() == text)
    }

    /// Whether `next` is a legal step from here.
    pub fn can_transition_to(self, next: Self) -> bool {
        use ExecutionState::*;
        match (self, next) {
            // The forward path, one step at a time.
            (IntentCreated, Validated) => true,
            (Validated, Sent) => true,
            (Sent, Confirmed) => true,
            (Confirmed, Completed) => true,
            // The liveness invariant. Every running state has this edge, and it
            // is the only edge that is unconditional.
            (from, Aborted) => from.is_active(),
            _ => false,
        }
    }

    /// Takes the step, or says why it could not.
    pub fn transition_to(self, next: Self) -> Result<Self, ExecutionError> {
        if self.is_terminal() {
            return Err(ExecutionError::AlreadyTerminal {
                state: self,
                attempted: next,
            });
        }
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(ExecutionError::InvalidTransition {
                from: self,
                to: next,
            })
        }
    }

    /// `IntentCreated` -> `Validated`: the risk gate said yes.
    pub fn validate(self) -> Result<Self, ExecutionError> {
        self.transition_to(ExecutionState::Validated)
    }

    /// `Validated` -> `Sent`: the transaction is on the network.
    pub fn send(self) -> Result<Self, ExecutionError> {
        self.transition_to(ExecutionState::Sent)
    }

    /// `Sent` -> `Confirmed`: it landed.
    pub fn confirm(self) -> Result<Self, ExecutionError> {
        self.transition_to(ExecutionState::Confirmed)
    }

    /// `Confirmed` -> `Completed`: the position is closed and booked.
    pub fn complete(self) -> Result<Self, ExecutionError> {
        self.transition_to(ExecutionState::Completed)
    }

    /// Bails out. Succeeds from every active state, always.
    ///
    /// The outcome says whether anything is left behind. Aborting a `Sent` or
    /// `Confirmed` execution stops the engine managing it — it does not sell
    /// the position, because there is no transaction that can un-send another
    /// one. Somebody still has to flatten it, and `needs_unwind` is how that
    /// gets noticed instead of being discovered later in a balance.
    pub fn abort(self, reason: AbortReason) -> Result<AbortOutcome, ExecutionError> {
        let state = self.transition_to(ExecutionState::Aborted)?;
        Ok(AbortOutcome {
            from: self,
            state,
            reason,
            needs_unwind: self.has_money_at_risk(),
        })
    }
}

impl fmt::Display for ExecutionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why an execution was given up on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AbortReason {
    /// The risk gate refused it.
    RiskGate,
    /// A circuit breaker was tripped.
    CircuitBreaker,
    /// The kill switch was pulled.
    KillSwitch,
    /// The edge was gone before the transaction went out.
    Stale,
    /// The simulation or the send itself failed.
    SendFailed,
    /// The transaction never landed.
    NotConfirmed,
    /// Somebody pressed the button.
    Operator,
}

impl AbortReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            AbortReason::RiskGate => "risk_gate",
            AbortReason::CircuitBreaker => "circuit_breaker",
            AbortReason::KillSwitch => "kill_switch",
            AbortReason::Stale => "stale",
            AbortReason::SendFailed => "send_failed",
            AbortReason::NotConfirmed => "not_confirmed",
            AbortReason::Operator => "operator",
        }
    }
}

/// What an abort left behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AbortOutcome {
    /// The state it was aborted from, for the audit row.
    pub from: ExecutionState,
    /// Always `Aborted`. Present so callers assign one thing, not two.
    pub state: ExecutionState,
    pub reason: AbortReason,
    /// True when something is still on-chain and has to be flattened by hand.
    pub needs_unwind: bool,
}

/// What a refused transition was.
///
/// Carries states rather than a message, so it is `Copy` and costs no
/// allocation on a path that may be taken while things are going wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ExecutionError {
    /// Both states are real, but there is no edge between them.
    InvalidTransition {
        from: ExecutionState,
        to: ExecutionState,
    },
    /// This execution already finished. Nothing more can happen to it, not even
    /// an abort — aborting something already completed would rewrite history.
    AlreadyTerminal {
        state: ExecutionState,
        attempted: ExecutionState,
    },
}

impl fmt::Display for ExecutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecutionError::InvalidTransition { from, to } => {
                write!(f, "an execution cannot go from {from} to {to}")
            }
            ExecutionError::AlreadyTerminal { state, attempted } => {
                write!(
                    f,
                    "this execution is already {state}; it cannot become {attempted}"
                )
            }
        }
    }
}

impl std::error::Error for ExecutionError {}

// ---------------------------------------------------------------------------
// operating mode
// ---------------------------------------------------------------------------

/// What the engine is allowed to do with what it decides.
///
/// The scoring is identical in every mode. All that changes is where the orders
/// go, which is the point: paper and replay results are only worth anything if
/// they came off the same code path as live ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OperatingMode {
    /// Real transactions, real money.
    Live,
    /// Live data, orders filled against the book instead of sent.
    Paper,
    /// Recorded data, replayed from `sts.db`. Never touches the network.
    Replay,
    /// Stopped. Scores, opens nothing, still closes what is open.
    Halted,
}

impl OperatingMode {
    /// The only mode that spends anything.
    pub const fn spends_real_money(self) -> bool {
        matches!(self, OperatingMode::Live)
    }

    /// Whether the mode itself permits opening a position. Not the whole
    /// answer — `RiskSnapshot::entries_allowed` is.
    pub const fn allows_new_entries(self) -> bool {
        !matches!(self, OperatingMode::Halted)
    }

    /// Replay must never reach the network: a backtest that quietly fetched a
    /// live price would be reporting the future.
    pub const fn touches_network(self) -> bool {
        !matches!(self, OperatingMode::Replay)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            OperatingMode::Live => "live",
            OperatingMode::Paper => "paper",
            OperatingMode::Replay => "replay",
            OperatingMode::Halted => "halted",
        }
    }
}

impl fmt::Display for OperatingMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// risk
// ---------------------------------------------------------------------------

/// Why a breaker tripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BreakerReason {
    /// Drawdown from the high-water mark passed its limit.
    Drawdown,
    /// Too many losers in a row for the edge to still be believable.
    LosingStreak,
    /// The RPC endpoint is slow or disagreeing with itself.
    RpcDegraded,
    /// Fills are coming back materially worse than quoted.
    SlippageSpike,
    /// The kill switch was pulled, or the process panicked.
    KillSwitch,
    /// Somebody stopped it by hand.
    Operator,
}

/// Whether something has stopped new entries, and if so what.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum CircuitBreaker {
    /// Nothing is in the way.
    Clear,
    /// A limit was hit. New entries are refused; exits are not affected, ever.
    Tripped {
        reason: BreakerReason,
        #[serde(rename = "atMs")]
        at_ms: i64,
        /// When it lifts by itself. `None` means it does not — somebody has to
        /// look at what happened first. That is the right default for anything
        /// that tripped because the engine was losing money.
        #[serde(rename = "clearsAtMs")]
        clears_at_ms: Option<i64>,
    },
}

impl CircuitBreaker {
    /// Trips the breaker for a fixed period.
    pub const fn trip_until(reason: BreakerReason, at_ms: i64, clears_at_ms: i64) -> Self {
        CircuitBreaker::Tripped {
            reason,
            at_ms,
            clears_at_ms: Some(clears_at_ms),
        }
    }

    /// Trips the breaker until a person clears it.
    pub const fn trip_hard(reason: BreakerReason, at_ms: i64) -> Self {
        CircuitBreaker::Tripped {
            reason,
            at_ms,
            clears_at_ms: None,
        }
    }

    /// Whether it is tripped at all, ignoring whether it has since expired.
    pub const fn is_tripped(&self) -> bool {
        matches!(self, CircuitBreaker::Tripped { .. })
    }

    /// Whether it is still stopping entries as of `now_ms`.
    ///
    /// A cool-off that has run out stops blocking on its own; a hard trip never
    /// does, no matter how long ago it happened.
    pub const fn blocks_entries_at(&self, now_ms: i64) -> bool {
        match self {
            CircuitBreaker::Clear => false,
            CircuitBreaker::Tripped { clears_at_ms, .. } => match clears_at_ms {
                Some(clears_at) => now_ms < *clears_at,
                None => true,
            },
        }
    }
}

/// How much room the fast path has left.
///
/// The fast path is the route that skips the slower confirmations to get in
/// while a launch is still moving. It is the most dangerous thing the engine
/// does, so its allowance is explicit and finite rather than a boolean somebody
/// can flip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FastPathGate {
    /// Whether the fast path may be taken at all right now.
    pub allowed: bool,
    /// How many fast-path entries are left in the current window.
    pub remaining_in_window: u16,
    /// The largest position it may open, in lamports.
    pub max_notional_lamports: u64,
    /// The worst fill it may accept, in basis points.
    pub max_slippage_bps: u16,
}

impl FastPathGate {
    /// Shut. The right thing to start from and the right thing to fall back to.
    pub const CLOSED: FastPathGate = FastPathGate {
        allowed: false,
        remaining_in_window: 0,
        max_notional_lamports: 0,
        max_slippage_bps: 0,
    };

    /// Whether a position of this size may go through the fast path.
    pub const fn admits(&self, notional_lamports: u64) -> bool {
        self.allowed
            && self.remaining_in_window > 0
            && notional_lamports > 0
            && notional_lamports <= self.max_notional_lamports
    }
}

/// The liquidity floor under everything.
///
/// Two separate numbers rather than one, because "too thin to enter" and "too
/// thin to still be here" are different questions with different answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiquidityThresholds {
    /// Below this, do not enter at all.
    pub min_pool_lamports: u64,
    /// Below this, close what is open. Set under `min_pool_lamports`, so the
    /// engine is not entering and exiting the same pool on the same tick.
    pub exit_only_below_lamports: u64,
    /// The most of a pool one position may be, in basis points. This is the
    /// number that stops the engine being the exit liquidity.
    pub max_pool_share_bps: u16,
}

/// The executable-liquidity participation cap, in basis points.
///
/// `STS_CORE_IDEOLOGY.md` §10 is a hard rule and not a tunable: "maximum
/// position size is no greater than 1.5% of current pool liquidity, measured
/// using executable depth at the relevant price bands, not headline TVL". 150
/// basis points is that, and it is stated once here because it was previously
/// stated three times — twice as a literal `500` and once, correctly, as
/// `replay::DEFAULT_MAX_POOL_SHARE_BPS`.
///
/// The disagreement was real and is settled here rather than split: nothing
/// sized a live position at 5%, because the only caller on the execution path
/// is `daemon.rs` and it reads the simulator's 150. The `500` sat in the risk
/// vocabulary waiting for the first thing to wire the gate to it.
pub const MAX_POOL_SHARE_BPS: u16 = 150;

impl LiquidityThresholds {
    /// Whether a pool this deep is worth entering.
    pub const fn admits_entry(&self, pool_lamports: u64) -> bool {
        pool_lamports >= self.min_pool_lamports
    }

    /// Whether a pool this deep has thinned out enough to leave.
    pub const fn demands_exit(&self, pool_lamports: u64) -> bool {
        pool_lamports < self.exit_only_below_lamports
    }

    /// The biggest position this pool can take without the share limit being
    /// broken. Saturating, so a pool of zero gives zero rather than a panic.
    pub const fn max_position_lamports(&self, pool_lamports: u64) -> u64 {
        (pool_lamports as u128 * self.max_pool_share_bps as u128 / 10_000) as u64
    }
}

/// Everything the risk gate knows, as one value, at one instant.
///
/// Passed by value into the decision path so a decision cannot be made against
/// numbers that changed halfway through making it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskSnapshot {
    /// When this was taken.
    pub at_ms: i64,
    pub mode: OperatingMode,
    /// Account value now, in lamports.
    pub equity_lamports: u64,
    /// The highest it has been, in lamports. Drawdown is measured from here.
    pub high_water_lamports: u64,
    /// How far below the high-water mark, in basis points.
    pub drawdown_bps: u16,
    /// The drawdown at which entries stop, in basis points.
    pub max_drawdown_bps: u16,
    pub open_positions: u16,
    pub max_open_positions: u16,
    pub circuit_breaker: CircuitBreaker,
    pub fast_path: FastPathGate,
    pub liquidity: LiquidityThresholds,
}

/// One basis point is 1/10_000. 100% is 10_000 bps.
pub const BPS_DENOMINATOR: u32 = 10_000;

/// One millionth is 1/1_000_000. A whole unit is 1_000_000 micros.
///
/// The unit every normalised score in this system is carried and stored in.
/// Basis points are the unit for money — a slippage bound, a fee, a share of a
/// launch — and are too coarse for the other kind of number here: an entropy or
/// an eigenvalue gap moves in the fourth decimal place, and rounding one to a
/// basis point would put two genuinely different clusters on the same reading.
pub const MICROS_DENOMINATOR: u32 = 1_000_000;

/// Drawdown from a high-water mark, in basis points, capped at 100%.
///
/// The multiply happens in `u128` because `high_water * 10_000` overflows `u64`
/// for large enough balances, and an overflow here would report a healthy
/// account as a ruined one — or worse, the other way round.
pub fn drawdown_bps(equity_lamports: u64, high_water_lamports: u64) -> u16 {
    if high_water_lamports == 0 || equity_lamports >= high_water_lamports {
        return 0;
    }
    let lost = (high_water_lamports - equity_lamports) as u128;
    let bps = lost * BPS_DENOMINATOR as u128 / high_water_lamports as u128;
    bps.min(BPS_DENOMINATOR as u128) as u16
}

impl RiskSnapshot {
    /// Whether a new position may be opened at all.
    ///
    /// Every clause here is a reason to say no. There is deliberately no clause
    /// that can say yes on its own.
    pub fn entries_allowed(&self) -> bool {
        self.mode.allows_new_entries()
            && !self.circuit_breaker.blocks_entries_at(self.at_ms)
            && self.open_positions < self.max_open_positions
            && self.drawdown_bps < self.max_drawdown_bps
    }

    /// Always true.
    ///
    /// This is a function rather than an omission on purpose. Closing a position
    /// goes through the same call as opening one, and this is the call that says
    /// the answer is not up for discussion — not when halted, not when the
    /// breaker is tripped, not at full drawdown. A limit that can trap the
    /// engine in a position is not a risk control, it is the risk.
    pub const fn exits_allowed(&self) -> bool {
        true
    }

    /// Whether a fast-path entry of this size is allowed, which needs both the
    /// gate and everything `entries_allowed` checks.
    pub fn fast_path_allowed(&self, notional_lamports: u64) -> bool {
        self.entries_allowed() && self.fast_path.admits(notional_lamports)
    }

    /// How many more positions may be opened before the cap.
    pub const fn free_slots(&self) -> u16 {
        self.max_open_positions.saturating_sub(self.open_positions)
    }

    /// Recomputes `drawdown_bps` from the two balances, so a snapshot cannot be
    /// built claiming a drawdown its own numbers disagree with.
    pub fn with_recomputed_drawdown(mut self) -> Self {
        self.drawdown_bps = drawdown_bps(self.equity_lamports, self.high_water_lamports);
        self
    }
}

// ---------------------------------------------------------------------------
// domain
// ---------------------------------------------------------------------------

/// A token the watcher has noticed and the engine has not yet judged.
///
/// Borrowed rather than owned. These are built directly over the buffer a
/// launch event was decoded from, at a point in the day when several arrive per
/// second, and the whole scoring pass finishes inside that buffer's lifetime.
/// Anything that has to outlive the event becomes a row in `sts.db`, not a
/// longer-lived struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenCandidate<'a> {
    pub mint: Pubkey,
    /// The wallet that created it. The single most useful field in here, since
    /// it is what links this launch to every previous one by the same person.
    pub creator: Pubkey,
    /// Ticker as it appeared in the create instruction. Untrusted text: it is
    /// whatever the creator typed, including nothing at all.
    pub symbol: &'a str,
    /// Epoch milliseconds of the create instruction.
    pub launched_at_ms: i64,
    /// How far along the bonding curve, in basis points. 10_000 is graduated.
    pub curve_progress_bps: u16,
    /// What was in the curve when it was first seen, in lamports.
    pub initial_liquidity_lamports: u64,
}

impl<'a> TokenCandidate<'a> {
    /// How old it is at `now_ms`. Saturating, because a clock that disagrees
    /// with the chain should give an odd age, not a negative one.
    pub const fn age_ms(&self, now_ms: i64) -> i64 {
        let age = now_ms.saturating_sub(self.launched_at_ms);
        if age < 0 {
            0
        } else {
            age
        }
    }

    /// Whether the curve has finished and it has moved to a real pool.
    pub const fn has_graduated(&self) -> bool {
        self.curve_progress_bps >= BPS_DENOMINATOR as u16
    }

    /// Whether it was deep enough to enter when it was seen.
    pub const fn meets(&self, thresholds: &LiquidityThresholds) -> bool {
        thresholds.admits_entry(self.initial_liquidity_lamports)
    }

    /// A creator of all zeroes means the decode did not find one, which is not
    /// the same as a launch by the System Program.
    pub const fn has_known_creator(&self) -> bool {
        !self.creator.is_zero()
    }
}

/// What a cluster of wallets looks like statistically.
///
/// One creator with fifty wallets is the thing this system exists to see. None
/// of these four numbers proves it on its own — a real crowd can look
/// concentrated, and a careful faker can look diffuse — so they are kept
/// separate here and weighed by the EV engine, which is where the thresholds
/// belong.
/// Every field is an integer, so `Eq` is free — and free `Eq` is what makes
/// "the metrics that went in are the metrics that came out" one `assert_eq!`
/// rather than a field-by-field comparison that quietly stops covering the
/// field somebody adds next. It is the same argument `journal.rs` makes about
/// its own rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SybilClusterMetrics {
    /// How many wallets are in the cluster.
    pub wallet_count: u32,
    /// Herfindahl-Hirschman index over how the cluster's holdings are split
    /// between its wallets, in basis points. 10_000 is one wallet holding
    /// everything; a few hundred is a genuinely spread-out crowd.
    pub holding_hhi_bps: u16,
    /// How tightly the cluster's buys land in the same moment, in millionths.
    /// Fifty wallets buying within one slot of each other is one hand, not
    /// fifty.
    pub temporal_influence_micros: u32,
    /// How cleanly this cluster separates from the rest of the transfer graph,
    /// in millionths, from the gap between eigenvalues. High means the wallets
    /// talk to each other far more than to anyone else.
    pub spectral_separation_micros: u32,
    /// Shannon entropy of who transacts with whom inside the cluster,
    /// normalised to millionths. Low means every path runs through one funder.
    pub interaction_entropy_micros: u32,
}

impl SybilClusterMetrics {
    /// Builds a set of metrics with every score forced into range.
    ///
    /// Clamping rather than trusting the caller is deliberate: these come out of
    /// an eigen-solver and an entropy sum, both of which can hand back a NaN or
    /// a value a hair outside [0, 1] on a degenerate graph. A NaN that reaches a
    /// comparison silently makes every gate answer false, which would look
    /// exactly like a clean cluster.
    pub const fn new(
        wallet_count: u32,
        holding_hhi_bps: u16,
        temporal_influence_micros: u64,
        spectral_separation_micros: u64,
        interaction_entropy_micros: u64,
    ) -> Self {
        SybilClusterMetrics {
            wallet_count,
            holding_hhi_bps: if holding_hhi_bps > BPS_DENOMINATOR as u16 {
                BPS_DENOMINATOR as u16
            } else {
                holding_hhi_bps
            },
            temporal_influence_micros: unit(temporal_influence_micros),
            spectral_separation_micros: unit(spectral_separation_micros),
            interaction_entropy_micros: unit(interaction_entropy_micros),
        }
    }

    /// True when every score is in range — which `new` guarantees, and which is
    /// worth asserting for anything built any other way.
    ///
    /// It can no longer be false for a value that came out of arithmetic rather
    /// than out of a struct literal. That is the point of the unit change: the
    /// NaN this used to have to catch was reachable only because the field was
    /// a float, and an integer has no way to spell one.
    pub const fn is_normalised(&self) -> bool {
        self.holding_hhi_bps <= BPS_DENOMINATOR as u16
            && self.temporal_influence_micros <= MICROS_DENOMINATOR
            && self.spectral_separation_micros <= MICROS_DENOMINATOR
            && self.interaction_entropy_micros <= MICROS_DENOMINATOR
    }

    /// A cluster of one wallet has nothing to measure. Its scores are whatever
    /// the maths happened to produce and mean nothing.
    pub const fn is_measurable(&self) -> bool {
        self.wallet_count >= 2
    }
}

/// Forces a score into [0, `MICROS_DENOMINATOR`].
///
/// Clamping rather than trusting the caller, for the reason `new` gives: the
/// analyser divides, and a degenerate graph can hand back a ratio a hair over
/// one. It takes a `u64` and returns a `u32` because that is the shape the
/// callers have — `strategy::syndicate` counts in `u64` millionths throughout —
/// and narrowing here is what makes the field's range a property of the type
/// rather than a comment on it.
const fn unit(micros: u64) -> u32 {
    if micros > MICROS_DENOMINATOR as u64 {
        MICROS_DENOMINATOR
    } else {
        micros as u32
    }
}

// ---------------------------------------------------------------------------
// exit lifecycle
// ---------------------------------------------------------------------------

/// Where one outbound exit transaction has got to.
///
/// `ExecutionState` is the life of an *intent*: what the engine means to do and
/// how far that got. This is one level below it — the life of the single
/// transaction that flattens a position, from its instructions being built to
/// its signature landing.
///
/// They are deliberately not the same vocabulary. `ExitSigned` has no analogue
/// in `ExecutionState`, and it is the state that matters most: a transaction
/// that is signed but never broadcast is the one case where the engine holds a
/// complete, valid, spendable instruction set and yet nothing is on the network
/// and nothing new is at risk. Collapsing that into `Sent` would lose the
/// distinction between "we failed before the network saw it" — recoverable,
/// retryable, no ambiguity — and "we failed after" — which is a position whose
/// status nobody knows.
///
/// The forward path is `ExitConstructed → ExitSigned → ExitBroadcast →
/// ExitConfirmed` one step at a time, with `ExitFailed` reachable from every
/// state that is still running. That is the same shape `ExecutionState` has and
/// for the same reason: a broadcast that was never signed means the signer was
/// bypassed, and that is worth failing loudly on rather than allowing.
///
/// `ExitBroadcast` is also the one state with an edge back to itself, taken by
/// `rebroadcast` when a confirmation window closes with no answer and the same
/// signed bytes are sent again. It is a self-edge rather than a state of its
/// own because nothing about the position changes when it is taken: the
/// transaction was already on the network, it still is, and its outcome is
/// still unknown. A reader of the ledger sees one row per attempt with the
/// reason in `detail`, which is the honest record of a transaction that had to
/// be pushed more than once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ExitState {
    /// The instructions and the message exist. Nothing is signed.
    ExitConstructed,
    /// The signer produced a signature. **Still nothing on the network** — this
    /// is the last state from which giving up costs nothing.
    ExitSigned,
    /// The transaction is on the network and its outcome is unknown.
    ExitBroadcast,
    /// It landed. The position is closed and the proceeds are real.
    ExitConfirmed,
    /// Given up on. Terminal, and reachable from every state above.
    ExitFailed,
}

impl ExitState {
    /// Every state, for exhaustive walks and tests.
    pub const ALL: [ExitState; 5] = [
        ExitState::ExitConstructed,
        ExitState::ExitSigned,
        ExitState::ExitBroadcast,
        ExitState::ExitConfirmed,
        ExitState::ExitFailed,
    ];

    /// Nothing more happens to an exit in a terminal state.
    pub const fn is_terminal(self) -> bool {
        matches!(self, ExitState::ExitConfirmed | ExitState::ExitFailed)
    }

    /// Still running, and therefore still abandonable.
    pub const fn is_active(self) -> bool {
        !self.is_terminal()
    }

    /// True once the transaction is somewhere the engine cannot recall it.
    ///
    /// This is the question that decides whether a failed exit leaves an
    /// obligation behind. An exit that failed before this was ever true left
    /// nothing new on chain — the original position is untouched and is still
    /// stranded exactly as it was. An exit that failed after it was true may or
    /// may not have sold the position, and that has to be reconciled against
    /// the signature rather than assumed either way.
    pub const fn is_dispatched(self) -> bool {
        matches!(self, ExitState::ExitBroadcast | ExitState::ExitConfirmed)
    }

    /// The name written to `intent_transitions` and read back from it.
    pub const fn as_str(self) -> &'static str {
        match self {
            ExitState::ExitConstructed => "exit_constructed",
            ExitState::ExitSigned => "exit_signed",
            ExitState::ExitBroadcast => "exit_broadcast",
            ExitState::ExitConfirmed => "exit_confirmed",
            ExitState::ExitFailed => "exit_failed",
        }
    }

    /// Reads back what `as_str` wrote. `None` for anything else, for the reason
    /// `ExecutionState::parse` gives: a stored value this build cannot name
    /// is not a value to guess at.
    pub fn parse(text: &str) -> Option<Self> {
        ExitState::ALL.into_iter().find(|s| s.as_str() == text)
    }

    /// Whether `next` is a legal step from here.
    pub fn can_transition_to(self, next: Self) -> bool {
        use ExitState::*;
        match (self, next) {
            (ExitConstructed, ExitSigned) => true,
            (ExitSigned, ExitBroadcast) => true,
            (ExitBroadcast, ExitConfirmed) => true,
            // The one self-edge in the machine, and the only one that will ever
            // be legal here. It means the *same signed transaction* was handed
            // to a node again after its confirmation window closed with no
            // answer — the signature is unchanged, so the cluster's own
            // deduplication is what stops it executing twice. A retry that
            // re-signed anything, at a higher tip or a fresher blockhash, is a
            // different transaction with a different signature and it is not
            // this edge: it is a new exit at a new attempt number, and it is
            // only safe once the first signature can no longer land.
            (ExitBroadcast, ExitBroadcast) => true,
            // The liveness invariant, same as `ExecutionState`: giving up is
            // available from every running state and is the only unconditional
            // edge in the machine.
            (from, ExitFailed) => from.is_active(),
            _ => false,
        }
    }

    /// Takes the step, or says why it could not.
    pub fn transition_to(self, next: Self) -> Result<Self, ExitTransitionError> {
        if self.is_terminal() {
            return Err(ExitTransitionError::AlreadyTerminal {
                state: self,
                attempted: next,
            });
        }
        if self.can_transition_to(next) {
            Ok(next)
        } else {
            Err(ExitTransitionError::InvalidTransition {
                from: self,
                to: next,
            })
        }
    }

    /// `ExitConstructed` -> `ExitSigned`: the signer produced a signature.
    pub fn sign(self) -> Result<Self, ExitTransitionError> {
        self.transition_to(ExitState::ExitSigned)
    }

    /// `ExitSigned` -> `ExitBroadcast`: it is on the network.
    pub fn broadcast(self) -> Result<Self, ExitTransitionError> {
        self.transition_to(ExitState::ExitBroadcast)
    }

    /// `ExitBroadcast` -> `ExitBroadcast`: the same bytes went out again.
    ///
    /// Legal only from `ExitBroadcast`, and only for a transaction whose
    /// signature has not changed. Sending identical bytes a second time is not
    /// a second sale: a validator that has already seen the signature drops the
    /// duplicate, so the worst case is a wasted packet and the best case is
    /// that the first one had been forgotten by a leader that never got it.
    ///
    /// It is a distinct method rather than a call to `broadcast` so that the
    /// two cannot be confused at a call site. `broadcast` is the step that puts
    /// a position at risk for the first time; this one is the step that does
    /// not.
    pub fn rebroadcast(self) -> Result<Self, ExitTransitionError> {
        if self != ExitState::ExitBroadcast {
            return Err(ExitTransitionError::InvalidTransition {
                from: self,
                to: ExitState::ExitBroadcast,
            });
        }
        self.transition_to(ExitState::ExitBroadcast)
    }

    /// `ExitBroadcast` -> `ExitConfirmed`: it landed.
    pub fn confirm(self) -> Result<Self, ExitTransitionError> {
        self.transition_to(ExitState::ExitConfirmed)
    }

    /// Gives up. Succeeds from every active state, always.
    ///
    /// The outcome carries `left_on_network`, which is the only thing a caller
    /// actually has to branch on: it is true exactly when the exit had already
    /// been dispatched, and it is the difference between "the position is
    /// untouched" and "the position's status is now unknown".
    pub fn fail(self, reason: ExitFailure) -> Result<ExitOutcome, ExitTransitionError> {
        let state = self.transition_to(ExitState::ExitFailed)?;
        Ok(ExitOutcome {
            from: self,
            state,
            reason,
            left_on_network: self.is_dispatched(),
        })
    }
}

impl fmt::Display for ExitState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why an exit was given up on.
///
/// Coarser than a message on purpose: it is `Copy`, it is what a counter is
/// bucketed by, and the detail belongs in the text alongside it rather than in
/// a variant per cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ExitFailure {
    /// The position could not be resolved to something sellable — no route, no
    /// reserves, a graduated curve, a pool too thin to pay out.
    NoRoute,
    /// The instructions or the message could not be built.
    Construction,
    /// The signer refused or was not there.
    Signing,
    /// The transaction never reached the network.
    Broadcast,
    /// It reached the network and did not land, or its outcome is unknown.
    NotConfirmed,
    /// The engine is going down and will not start what it cannot finish.
    ShuttingDown,
}

impl ExitFailure {
    pub const fn as_str(self) -> &'static str {
        match self {
            ExitFailure::NoRoute => "no_route",
            ExitFailure::Construction => "construction",
            ExitFailure::Signing => "signing",
            ExitFailure::Broadcast => "broadcast",
            ExitFailure::NotConfirmed => "not_confirmed",
            ExitFailure::ShuttingDown => "shutting_down",
        }
    }

    /// Reads back what `as_str` wrote.
    pub fn parse(text: &str) -> Option<Self> {
        const ALL: [ExitFailure; 6] = [
            ExitFailure::NoRoute,
            ExitFailure::Construction,
            ExitFailure::Signing,
            ExitFailure::Broadcast,
            ExitFailure::NotConfirmed,
            ExitFailure::ShuttingDown,
        ];
        ALL.into_iter().find(|f| f.as_str() == text)
    }
}

impl fmt::Display for ExitFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a position is sold back to SOL.
///
/// Two venues rather than the five programs the ingestion allowlist carries,
/// because this is the list of places an exit can actually be *built* for. A
/// program that can be watched but not sold into is not a venue; it is a gap,
/// and it should read as one rather than as an exit route that silently does
/// nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Venue {
    /// The pump.fun bonding curve, before graduation. The curve is the
    /// counterparty and its real SOL reserve is the whole of the executable
    /// liquidity.
    PumpFunCurve,
    /// Raydium's V4 constant-product AMM, where a graduated token trades.
    RaydiumAmmV4,
}

impl Venue {
    pub const ALL: [Venue; 2] = [Venue::PumpFunCurve, Venue::RaydiumAmmV4];

    /// The name written to `intent_transitions` and read back from it.
    pub const fn as_str(self) -> &'static str {
        match self {
            Venue::PumpFunCurve => "pump_fun_curve",
            Venue::RaydiumAmmV4 => "raydium_amm_v4",
        }
    }

    /// Reads back what `as_str` wrote.
    pub fn parse(text: &str) -> Option<Self> {
        Venue::ALL.into_iter().find(|v| v.as_str() == text)
    }
}

impl fmt::Display for Venue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What giving up on an exit left behind.
///
/// The mirror of `AbortOutcome`, and it exists for the same reason: the state
/// it failed from, the flag that says whether anything is out there, and the
/// reason are assigned from one value, because the three of them disagreeing is
/// the failure the flag exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitOutcome {
    /// The state it failed from.
    pub from: ExitState,
    /// Always `ExitFailed`. Present so callers assign one thing, not two.
    pub state: ExitState,
    pub reason: ExitFailure,
    /// True when a transaction was already on the network when this failed, so
    /// the position may or may not have been sold and has to be reconciled
    /// against the signature before anything else is done to it.
    pub left_on_network: bool,
}

/// What a refused exit transition was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ExitTransitionError {
    /// Both states are real, but there is no edge between them.
    InvalidTransition { from: ExitState, to: ExitState },
    /// This exit already finished. Nothing more can happen to it, not even a
    /// failure — failing something already confirmed would rewrite history.
    AlreadyTerminal {
        state: ExitState,
        attempted: ExitState,
    },
}

impl fmt::Display for ExitTransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExitTransitionError::InvalidTransition { from, to } => {
                write!(f, "an exit cannot go from {from} to {to}")
            }
            ExitTransitionError::AlreadyTerminal { state, attempted } => {
                write!(
                    f,
                    "this exit is already {state}; it cannot become {attempted}"
                )
            }
        }
    }
}

impl std::error::Error for ExitTransitionError {}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The states an execution can still be in the middle of.
    const ACTIVE: [ExecutionState; 4] = [
        ExecutionState::IntentCreated,
        ExecutionState::Validated,
        ExecutionState::Sent,
        ExecutionState::Confirmed,
    ];

    /// The states it can be finished in.
    const TERMINAL: [ExecutionState; 2] = [ExecutionState::Completed, ExecutionState::Aborted];

    // -- the forward path ---------------------------------------------------

    #[test]
    fn the_happy_path_walks_from_intent_to_completed() {
        let state = ExecutionState::IntentCreated;
        let state = state.validate().expect("intent validates");
        assert_eq!(state, ExecutionState::Validated);
        let state = state.send().expect("validated sends");
        assert_eq!(state, ExecutionState::Sent);
        let state = state.confirm().expect("sent confirms");
        assert_eq!(state, ExecutionState::Confirmed);
        let state = state.complete().expect("confirmed completes");
        assert_eq!(state, ExecutionState::Completed);
        assert!(state.is_terminal());
    }

    #[test]
    fn every_state_is_either_active_or_terminal_and_never_both() {
        for state in ExecutionState::ALL {
            assert_ne!(
                state.is_active(),
                state.is_terminal(),
                "{state} is both or neither"
            );
        }
        assert_eq!(ACTIVE.len() + TERMINAL.len(), ExecutionState::ALL.len());
    }

    // -- invalid transitions ------------------------------------------------

    #[test]
    fn the_forward_path_cannot_be_skipped() {
        // Sending something that was never validated is the risk gate being
        // bypassed, which is the one shortcut this machine exists to prevent.
        let err = ExecutionState::IntentCreated.send().unwrap_err();
        assert_eq!(
            err,
            ExecutionError::InvalidTransition {
                from: ExecutionState::IntentCreated,
                to: ExecutionState::Sent,
            }
        );
        assert!(ExecutionState::IntentCreated.confirm().is_err());
        assert!(ExecutionState::IntentCreated.complete().is_err());
        assert!(ExecutionState::Validated.confirm().is_err());
        assert!(ExecutionState::Validated.complete().is_err());
        assert!(ExecutionState::Sent.complete().is_err());
    }

    #[test]
    fn nothing_goes_backwards() {
        let backwards = [
            (ExecutionState::Validated, ExecutionState::IntentCreated),
            (ExecutionState::Sent, ExecutionState::Validated),
            (ExecutionState::Confirmed, ExecutionState::Sent),
        ];
        for (from, to) in backwards {
            assert!(!from.can_transition_to(to), "{from} should not reach {to}");
            assert_eq!(
                from.transition_to(to).unwrap_err(),
                ExecutionError::InvalidTransition { from, to }
            );
        }
    }

    #[test]
    fn no_state_transitions_to_itself() {
        for state in ExecutionState::ALL {
            assert!(
                !state.can_transition_to(state),
                "{state} should not step onto itself"
            );
        }
    }

    #[test]
    fn active_states_have_exactly_two_edges_out() {
        // One forward, one abort. Any third edge is a path somebody added
        // without deciding what it means.
        for state in ACTIVE {
            let edges = ExecutionState::ALL
                .into_iter()
                .filter(|next| state.can_transition_to(*next))
                .count();
            assert_eq!(edges, 2, "{state} has {edges} edges out, expected 2");
        }
    }

    // -- terminal states ----------------------------------------------------

    #[test]
    fn terminal_states_refuse_everything_including_abort() {
        for state in TERMINAL {
            for attempted in ExecutionState::ALL {
                assert_eq!(
                    state.transition_to(attempted).unwrap_err(),
                    ExecutionError::AlreadyTerminal { state, attempted },
                    "{state} accepted a move to {attempted}"
                );
            }
            // Aborting something already finished would rewrite what happened.
            assert_eq!(
                state.abort(AbortReason::Operator).unwrap_err(),
                ExecutionError::AlreadyTerminal {
                    state,
                    attempted: ExecutionState::Aborted,
                }
            );
        }
    }

    #[test]
    fn terminal_states_have_no_edges_out() {
        for state in TERMINAL {
            for next in ExecutionState::ALL {
                assert!(
                    !state.can_transition_to(next),
                    "{state} claims an edge to {next}"
                );
            }
        }
    }

    // -- the liveness invariant ---------------------------------------------

    #[test]
    fn every_active_state_can_abort() {
        // The invariant, stated as plainly as it can be: there is no state the
        // engine can get into where it has committed to something and cannot
        // then decide to stop.
        for state in ACTIVE {
            let outcome = state
                .abort(AbortReason::KillSwitch)
                .unwrap_or_else(|err| panic!("{state} refused an abort: {err}"));
            assert_eq!(outcome.state, ExecutionState::Aborted);
            assert_eq!(outcome.from, state);
            assert_eq!(outcome.reason, AbortReason::KillSwitch);
        }
    }

    #[test]
    fn an_exit_is_reachable_from_every_active_state() {
        // Walk the graph rather than reading the match arms back: a dead end
        // added three states from here would still be caught by this.
        for start in ACTIVE {
            let mut seen = vec![start];
            let mut queue = vec![start];
            let mut reached_terminal = false;
            while let Some(state) = queue.pop() {
                if state.is_terminal() {
                    reached_terminal = true;
                    break;
                }
                for next in ExecutionState::ALL {
                    if state.can_transition_to(next) && !seen.contains(&next) {
                        seen.push(next);
                        queue.push(next);
                    }
                }
            }
            assert!(reached_terminal, "{start} is a dead end");
        }
    }

    #[test]
    fn aborting_after_sending_says_something_is_left_behind() {
        // Nothing can un-send a transaction. An abort from here stops the
        // engine managing the position; it does not make the position go away.
        for state in [ExecutionState::Sent, ExecutionState::Confirmed] {
            let outcome = state.abort(AbortReason::NotConfirmed).expect("abortable");
            assert!(outcome.needs_unwind, "{state} left money unattended");
        }
        for state in [ExecutionState::IntentCreated, ExecutionState::Validated] {
            let outcome = state.abort(AbortReason::Stale).expect("abortable");
            assert!(!outcome.needs_unwind, "{state} has nothing to unwind");
        }
    }

    #[test]
    fn has_money_at_risk_starts_exactly_when_the_transaction_goes_out() {
        assert!(!ExecutionState::IntentCreated.has_money_at_risk());
        assert!(!ExecutionState::Validated.has_money_at_risk());
        assert!(ExecutionState::Sent.has_money_at_risk());
        assert!(ExecutionState::Confirmed.has_money_at_risk());
    }

    // -- persistence and the wire -------------------------------------------

    #[test]
    fn state_names_survive_a_round_trip_through_the_database() {
        for state in ExecutionState::ALL {
            assert_eq!(ExecutionState::parse(state.as_str()), Some(state));
        }
        assert_eq!(ExecutionState::parse("halfway"), None);
        assert_eq!(ExecutionState::parse("IntentCreated"), None);
    }

    #[test]
    fn state_names_are_all_different() {
        let mut names: Vec<&str> = ExecutionState::ALL.iter().map(|s| s.as_str()).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count);
    }

    #[test]
    fn the_ui_sees_camel_case_states_and_a_tagged_error() {
        let json = serde_json::to_string(&ExecutionState::IntentCreated).unwrap();
        assert_eq!(json, "\"intentCreated\"");

        let err = ExecutionState::Sent.complete().unwrap_err();
        let json = serde_json::to_value(err).unwrap();
        assert_eq!(json["kind"], "invalidTransition");
        assert_eq!(json["from"], "sent");
        assert_eq!(json["to"], "completed");
    }

    #[test]
    fn a_transition_error_reads_as_a_sentence() {
        let err = ExecutionState::IntentCreated.send().unwrap_err();
        assert_eq!(
            err.to_string(),
            "an execution cannot go from intent_created to sent"
        );
    }

    // -- operating mode -----------------------------------------------------

    #[test]
    fn only_live_spends_money_and_only_replay_stays_offline() {
        assert!(OperatingMode::Live.spends_real_money());
        for mode in [
            OperatingMode::Paper,
            OperatingMode::Replay,
            OperatingMode::Halted,
        ] {
            assert!(!mode.spends_real_money(), "{mode} would spend money");
        }
        assert!(!OperatingMode::Replay.touches_network());
        assert!(OperatingMode::Live.touches_network());
    }

    #[test]
    fn halted_is_the_only_mode_that_opens_nothing() {
        assert!(!OperatingMode::Halted.allows_new_entries());
        for mode in [
            OperatingMode::Live,
            OperatingMode::Paper,
            OperatingMode::Replay,
        ] {
            assert!(mode.allows_new_entries());
        }
    }

    // -- risk ---------------------------------------------------------------

    /// A snapshot with nothing wrong with it, for tests to break one field of.
    fn healthy() -> RiskSnapshot {
        RiskSnapshot {
            at_ms: 1_700_000_000_000,
            mode: OperatingMode::Live,
            equity_lamports: 100 * LAMPORTS_PER_SOL,
            high_water_lamports: 100 * LAMPORTS_PER_SOL,
            drawdown_bps: 0,
            max_drawdown_bps: 1_500,
            open_positions: 2,
            max_open_positions: 5,
            circuit_breaker: CircuitBreaker::Clear,
            fast_path: FastPathGate {
                allowed: true,
                remaining_in_window: 3,
                max_notional_lamports: 2 * LAMPORTS_PER_SOL,
                max_slippage_bps: 300,
            },
            liquidity: LiquidityThresholds {
                min_pool_lamports: 30 * LAMPORTS_PER_SOL,
                exit_only_below_lamports: 10 * LAMPORTS_PER_SOL,
                max_pool_share_bps: MAX_POOL_SHARE_BPS,
            },
        }
    }

    const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

    #[test]
    fn a_healthy_snapshot_lets_a_position_open() {
        assert!(healthy().entries_allowed());
    }

    #[test]
    fn exits_stay_open_no_matter_how_bad_things_are() {
        // The whole point of the file, in one test. Halted, breaker tripped by
        // a panic, at the position cap, wiped out — and still able to sell.
        let ruined = RiskSnapshot {
            mode: OperatingMode::Halted,
            equity_lamports: 0,
            drawdown_bps: BPS_DENOMINATOR as u16,
            open_positions: 5,
            max_open_positions: 5,
            circuit_breaker: CircuitBreaker::trip_hard(BreakerReason::KillSwitch, 1),
            fast_path: FastPathGate::CLOSED,
            ..healthy()
        };
        assert!(!ruined.entries_allowed());
        assert!(ruined.exits_allowed());

        // And there is no snapshot at all that says otherwise.
        for mode in [
            OperatingMode::Live,
            OperatingMode::Paper,
            OperatingMode::Replay,
            OperatingMode::Halted,
        ] {
            let snapshot = RiskSnapshot { mode, ..ruined };
            assert!(snapshot.exits_allowed(), "{mode} closed the exit");
        }
    }

    #[test]
    fn each_limit_on_its_own_stops_a_new_position() {
        let at_cap = RiskSnapshot {
            open_positions: 5,
            max_open_positions: 5,
            ..healthy()
        };
        assert!(!at_cap.entries_allowed());
        assert_eq!(at_cap.free_slots(), 0);

        let drawn_down = RiskSnapshot {
            drawdown_bps: 1_500,
            max_drawdown_bps: 1_500,
            ..healthy()
        };
        assert!(!drawn_down.entries_allowed());

        let halted = RiskSnapshot {
            mode: OperatingMode::Halted,
            ..healthy()
        };
        assert!(!halted.entries_allowed());

        let tripped = RiskSnapshot {
            circuit_breaker: CircuitBreaker::trip_hard(BreakerReason::LosingStreak, 1),
            ..healthy()
        };
        assert!(!tripped.entries_allowed());
    }

    #[test]
    fn free_slots_never_wraps_when_the_cap_is_lowered_underneath_us() {
        let over = RiskSnapshot {
            open_positions: 7,
            max_open_positions: 5,
            ..healthy()
        };
        assert_eq!(over.free_slots(), 0);
        assert!(!over.entries_allowed());
    }

    #[test]
    fn a_cool_off_expires_and_a_hard_trip_does_not() {
        let now = 1_700_000_000_000;
        let cooling = CircuitBreaker::trip_until(BreakerReason::SlippageSpike, now, now + 60_000);
        assert!(cooling.is_tripped());
        assert!(cooling.blocks_entries_at(now));
        assert!(cooling.blocks_entries_at(now + 59_999));
        assert!(!cooling.blocks_entries_at(now + 60_000));

        let hard = CircuitBreaker::trip_hard(BreakerReason::Drawdown, now);
        assert!(hard.blocks_entries_at(now));
        assert!(hard.blocks_entries_at(now + 86_400_000));

        assert!(!CircuitBreaker::Clear.is_tripped());
        assert!(!CircuitBreaker::Clear.blocks_entries_at(now));
    }

    #[test]
    fn a_snapshot_taken_after_a_cool_off_ended_can_enter_again() {
        let base = healthy();
        let cooling = CircuitBreaker::trip_until(
            BreakerReason::RpcDegraded,
            base.at_ms - 60_000,
            base.at_ms - 1,
        );
        let recovered = RiskSnapshot {
            circuit_breaker: cooling,
            ..base
        };
        assert!(recovered.entries_allowed());
    }

    #[test]
    fn the_fast_path_needs_the_gate_and_the_rest_of_the_risk_gate_too() {
        let snapshot = healthy();
        assert!(snapshot.fast_path_allowed(LAMPORTS_PER_SOL));
        // Over its own size limit.
        assert!(!snapshot.fast_path_allowed(3 * LAMPORTS_PER_SOL));
        // A zero-size entry is a bug upstream, not a free pass.
        assert!(!snapshot.fast_path_allowed(0));

        let used_up = RiskSnapshot {
            fast_path: FastPathGate {
                remaining_in_window: 0,
                ..snapshot.fast_path
            },
            ..snapshot
        };
        assert!(!used_up.fast_path_allowed(LAMPORTS_PER_SOL));

        // The gate is wide open but the portfolio is halted, so it stays shut.
        let halted = RiskSnapshot {
            mode: OperatingMode::Halted,
            ..snapshot
        };
        assert!(!halted.fast_path_allowed(LAMPORTS_PER_SOL));
        assert!(!FastPathGate::CLOSED.admits(1));
    }

    #[test]
    fn drawdown_is_measured_from_the_high_water_mark() {
        assert_eq!(drawdown_bps(100, 100), 0);
        assert_eq!(drawdown_bps(75, 100), 2_500);
        assert_eq!(drawdown_bps(0, 100), BPS_DENOMINATOR as u16);
        // Up on the day: no drawdown, and no underflow either.
        assert_eq!(drawdown_bps(120, 100), 0);
        // Nothing has ever been made, so nothing has been lost.
        assert_eq!(drawdown_bps(0, 0), 0);
    }

    #[test]
    fn drawdown_does_not_overflow_on_a_large_balance() {
        // `high_water * 10_000` leaves `u64` long before this. Getting this
        // wrong would report a full account as a wiped-out one.
        let high = u64::MAX;
        let equity = u64::MAX / 2;
        assert_eq!(drawdown_bps(equity, high), 5_000);
        assert_eq!(drawdown_bps(high, high), 0);
        assert_eq!(drawdown_bps(0, high), BPS_DENOMINATOR as u16);
    }

    #[test]
    fn recomputing_drawdown_overrides_whatever_the_field_claimed() {
        let lying = RiskSnapshot {
            equity_lamports: 50 * LAMPORTS_PER_SOL,
            high_water_lamports: 100 * LAMPORTS_PER_SOL,
            drawdown_bps: 0,
            ..healthy()
        };
        assert!(lying.entries_allowed());
        let honest = lying.with_recomputed_drawdown();
        assert_eq!(honest.drawdown_bps, 5_000);
        assert!(!honest.entries_allowed());
    }

    #[test]
    fn liquidity_has_a_gap_between_entering_and_leaving() {
        let thresholds = healthy().liquidity;
        assert!(thresholds.admits_entry(30 * LAMPORTS_PER_SOL));
        assert!(!thresholds.admits_entry(29 * LAMPORTS_PER_SOL));
        assert!(thresholds.demands_exit(9 * LAMPORTS_PER_SOL));
        assert!(!thresholds.demands_exit(10 * LAMPORTS_PER_SOL));
        // Between the two nothing happens, which is the point: no pool should
        // be one worth entering and one worth fleeing at the same time.
        assert!(!thresholds.admits_entry(20 * LAMPORTS_PER_SOL));
        assert!(!thresholds.demands_exit(20 * LAMPORTS_PER_SOL));
    }

    #[test]
    fn position_size_is_capped_at_a_share_of_the_pool() {
        // 1.5% — doctrine §10's hard rule, and the only cap in the codebase.
        let thresholds = healthy().liquidity;
        assert_eq!(thresholds.max_pool_share_bps, MAX_POOL_SHARE_BPS);
        assert_eq!(
            thresholds.max_position_lamports(100 * LAMPORTS_PER_SOL),
            3 * LAMPORTS_PER_SOL / 2
        );
        assert_eq!(thresholds.max_position_lamports(0), 0);
        // The u128 intermediate again: a huge pool must not wrap to a tiny cap.
        assert_eq!(
            thresholds.max_position_lamports(u64::MAX),
            ((u64::MAX as u128) * 150 / 10_000) as u64
        );
    }

    // -- domain types -------------------------------------------------------

    #[test]
    fn the_system_program_is_thirty_two_zero_bytes() {
        // The one address worth hard-coding: it is also what an empty field
        // decodes to, and the leading-zero handling in base58 is exactly what
        // this catches.
        let text = "11111111111111111111111111111111";
        let key = Pubkey::parse(text).expect("the system program parses");
        assert_eq!(key, Pubkey::ZERO);
        assert!(key.is_zero());
        assert_eq!(key.to_string(), text);
    }

    #[test]
    fn a_real_address_round_trips() {
        let text = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
        let key = Pubkey::parse(text).expect("the token program parses");
        assert!(!key.is_zero());
        assert_eq!(key.to_string(), text);
        assert_eq!(Pubkey::parse(&key.to_string()), Ok(key));
    }

    #[test]
    fn every_byte_pattern_round_trips() {
        let patterns: [[u8; 32]; 4] = [
            [0u8; 32],
            [0xff; 32],
            // One leading zero byte, which is the case a naive encoder loses.
            {
                let mut bytes = [7u8; 32];
                bytes[0] = 0;
                bytes
            },
            {
                let mut bytes = [0u8; 32];
                let mut i = 0;
                while i < 32 {
                    bytes[i] = (i as u8).wrapping_mul(37).wrapping_add(11);
                    i += 1;
                }
                bytes
            },
        ];
        for bytes in patterns {
            let key = Pubkey::new(bytes);
            let text = key.to_string();
            assert_eq!(Pubkey::parse(&text), Ok(key), "{text} did not round trip");
            assert_eq!(key.as_bytes(), &bytes);
        }
    }

    #[test]
    fn text_that_is_not_an_address_is_refused() {
        // Too short: this decodes to fewer than 32 bytes and must not be padded
        // into a valid-looking key.
        assert_eq!(Pubkey::parse("1111"), Err(Base58Error::Length));
        assert_eq!(Pubkey::parse(""), Err(Base58Error::Length));
        // The characters base58 leaves out precisely because they are misread.
        for bad in [
            "0000000000000000000000000000000O",
            "IIIIIIIIIIIIIIIIIIIIIIIIIIIIIIIl",
        ] {
            assert_eq!(Pubkey::parse(bad), Err(Base58Error::Alphabet));
        }
        // Too long: 44 ones is 44 zero bytes, not 32.
        assert_eq!(
            Pubkey::parse("11111111111111111111111111111111111111111111"),
            Err(Base58Error::Length)
        );
        // A signature is not an address.
        let signature = Signature::new([9u8; 64]).to_string();
        assert_eq!(Pubkey::parse(&signature), Err(Base58Error::Length));
    }

    #[test]
    fn signatures_round_trip_too() {
        for fill in [0u8, 1, 0xff] {
            let signature = Signature::new([fill; 64]);
            let text = signature.to_string();
            assert_eq!(Signature::parse(&text), Ok(signature));
            assert_eq!(signature.as_bytes(), &[fill; 64]);
        }
    }

    #[test]
    fn keys_reach_the_ui_as_text_not_as_a_list_of_numbers() {
        let key = Pubkey::parse("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA").unwrap();
        assert_eq!(
            serde_json::to_string(&key).unwrap(),
            "\"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA\""
        );
        assert_eq!(format!("{key:?}"), format!("Pubkey({key})"));
    }

    #[test]
    fn a_candidate_borrows_its_symbol_rather_than_copying_it() {
        // The symbol points into the decoded event, which is the reason
        // `TokenCandidate` carries a lifetime at all.
        let decoded = String::from("PUMPCAT");
        let candidate = TokenCandidate {
            mint: Pubkey::new([3u8; 32]),
            creator: Pubkey::new([4u8; 32]),
            symbol: &decoded[..],
            launched_at_ms: 1_700_000_000_000,
            curve_progress_bps: 4_200,
            initial_liquidity_lamports: 40 * LAMPORTS_PER_SOL,
        };
        assert_eq!(candidate.symbol.as_ptr(), decoded.as_ptr());
        assert!(candidate.has_known_creator());
        assert!(!candidate.has_graduated());
        assert!(candidate.meets(&healthy().liquidity));
        assert_eq!(candidate.age_ms(1_700_000_030_000), 30_000);
    }

    #[test]
    fn a_candidate_with_a_clock_running_backwards_is_not_negatively_old() {
        let candidate = TokenCandidate {
            mint: Pubkey::ZERO,
            creator: Pubkey::ZERO,
            symbol: "",
            launched_at_ms: 1_700_000_000_000,
            curve_progress_bps: BPS_DENOMINATOR as u16,
            initial_liquidity_lamports: 0,
        };
        assert_eq!(candidate.age_ms(1_699_999_000_000), 0);
        assert!(candidate.has_graduated());
        assert!(!candidate.has_known_creator());
        assert!(!candidate.meets(&healthy().liquidity));
    }

    #[test]
    fn sybil_scores_are_forced_into_range() {
        let metrics = SybilClusterMetrics::new(12, 20_000, 1_400_000, 0, 500_000);
        assert_eq!(metrics.holding_hhi_bps, BPS_DENOMINATOR as u16);
        assert_eq!(metrics.temporal_influence_micros, MICROS_DENOMINATOR);
        assert_eq!(metrics.spectral_separation_micros, 0);
        assert_eq!(metrics.interaction_entropy_micros, 500_000);
        assert!(metrics.is_normalised());
        assert!(metrics.is_measurable());
    }

    #[test]
    fn a_score_past_a_whole_unit_is_capped_rather_than_wrapped() {
        // The analyser divides, and a degenerate graph can hand back a ratio a
        // hair over one — or, before these were integers, a NaN that compared
        // false against every threshold and so made a fake cluster look clean.
        // There is no NaN to catch any more; what is left is the overshoot, and
        // it saturates rather than truncating into a small number.
        let metrics = SybilClusterMetrics::new(4, 100, u64::MAX, MICROS_DENOMINATOR as u64 + 1, 0);
        assert_eq!(metrics.temporal_influence_micros, MICROS_DENOMINATOR);
        assert_eq!(metrics.spectral_separation_micros, MICROS_DENOMINATOR);
        assert_eq!(metrics.interaction_entropy_micros, 0);
        assert!(metrics.is_normalised());
    }

    #[test]
    fn a_lone_wallet_is_not_a_cluster() {
        let alone = SybilClusterMetrics::new(1, 10_000, MICROS_DENOMINATOR as u64, 1_000_000, 0);
        assert!(!alone.is_measurable());
        let pair = SybilClusterMetrics::new(2, 5_000, 500_000, 500_000, 500_000);
        assert!(pair.is_measurable());
    }

    #[test]
    fn metrics_built_by_hand_can_be_checked() {
        let hand_built = SybilClusterMetrics {
            wallet_count: 3,
            holding_hhi_bps: 3_300,
            temporal_influence_micros: 900_000,
            spectral_separation_micros: MICROS_DENOMINATOR + 1,
            interaction_entropy_micros: 100_000,
        };
        assert!(!hand_built.is_normalised());
    }

    #[test]
    fn two_metrics_over_the_same_numbers_are_the_same_value() {
        // What `Eq` buys, and the reason it can be derived at all now. A float
        // field would have made this a field-by-field comparison that stops
        // covering whichever field is added next.
        let one = SybilClusterMetrics::new(6, 4_200, 310_000, 620_000, 155_000);
        let again = SybilClusterMetrics::new(6, 4_200, 310_000, 620_000, 155_000);
        assert_eq!(one, again);
    }

    // -- the exit lifecycle -------------------------------------------------

    /// The states an exit can still be in the middle of.
    const EXIT_ACTIVE: [ExitState; 3] = [
        ExitState::ExitConstructed,
        ExitState::ExitSigned,
        ExitState::ExitBroadcast,
    ];

    #[test]
    fn an_exit_walks_from_constructed_to_confirmed() {
        let state = ExitState::ExitConstructed;
        let state = state.sign().expect("constructed signs");
        assert_eq!(state, ExitState::ExitSigned);
        assert!(
            !state.is_dispatched(),
            "a signed transaction nobody broadcast is not on the network"
        );
        let state = state.broadcast().expect("signed broadcasts");
        assert_eq!(state, ExitState::ExitBroadcast);
        assert!(state.is_dispatched());
        let state = state.confirm().expect("broadcast confirms");
        assert_eq!(state, ExitState::ExitConfirmed);
        assert!(state.is_terminal());
    }

    #[test]
    fn an_exit_cannot_skip_the_signer_or_the_network() {
        use ExitState::*;
        assert!(
            ExitConstructed.broadcast().is_err(),
            "broadcasting unsigned bytes"
        );
        assert!(ExitConstructed.confirm().is_err());
        assert!(
            ExitSigned.confirm().is_err(),
            "confirming what was never sent"
        );
        // And it never goes backwards.
        assert!(ExitBroadcast.sign().is_err());
        assert!(ExitSigned.transition_to(ExitConstructed).is_err());
    }

    #[test]
    fn only_a_broadcast_exit_can_be_broadcast_again() {
        use ExitState::*;
        let state = ExitBroadcast
            .rebroadcast()
            .expect("the same bytes may go out again");
        assert_eq!(
            state, ExitBroadcast,
            "repeating a broadcast moves the exit nowhere"
        );
        assert!(
            state.is_dispatched(),
            "and it is still on the network afterwards"
        );

        // Every other state refuses it, so a retry can never be used to reach
        // the network without passing the signer first.
        for state in [ExitConstructed, ExitSigned, ExitConfirmed, ExitFailed] {
            assert!(
                state.rebroadcast().is_err(),
                "{state} has no broadcast to repeat"
            );
        }
    }

    #[test]
    fn the_repeated_broadcast_is_the_only_place_the_machine_stands_still() {
        for state in ExitState::ALL {
            let loops = state.can_transition_to(state);
            assert_eq!(
                loops,
                state == ExitState::ExitBroadcast,
                "{state} disagrees with the one self-edge the machine is allowed"
            );
        }
    }

    #[test]
    fn every_running_exit_can_fail_and_every_finished_one_cannot() {
        for state in EXIT_ACTIVE {
            let outcome = state
                .fail(ExitFailure::Broadcast)
                .expect("active exits fail");
            assert_eq!(outcome.from, state);
            assert_eq!(outcome.state, ExitState::ExitFailed);
            assert_eq!(outcome.reason, ExitFailure::Broadcast);
            assert_eq!(
                outcome.left_on_network,
                state.is_dispatched(),
                "{state} disagrees with itself about what is on the network"
            );
        }
        for state in [ExitState::ExitConfirmed, ExitState::ExitFailed] {
            assert!(
                matches!(
                    state.fail(ExitFailure::NotConfirmed),
                    Err(ExitTransitionError::AlreadyTerminal { .. })
                ),
                "{state} is finished and failing it would rewrite history"
            );
        }
    }

    #[test]
    fn only_a_dispatched_exit_leaves_anything_behind() {
        use ExitState::*;
        assert!(
            !ExitConstructed
                .fail(ExitFailure::Construction)
                .expect("fails")
                .left_on_network
        );
        assert!(
            !ExitSigned
                .fail(ExitFailure::Broadcast)
                .expect("fails")
                .left_on_network
        );
        assert!(
            ExitBroadcast
                .fail(ExitFailure::NotConfirmed)
                .expect("fails")
                .left_on_network,
            "a broadcast that never confirmed may still have sold the position"
        );
    }

    #[test]
    fn every_exit_state_is_either_active_or_terminal_and_never_both() {
        for state in ExitState::ALL {
            assert_ne!(
                state.is_active(),
                state.is_terminal(),
                "{state} is both or neither"
            );
        }
        assert_eq!(EXIT_ACTIVE.len() + 2, ExitState::ALL.len());
    }

    #[test]
    fn exit_state_names_survive_a_round_trip_through_the_database() {
        for state in ExitState::ALL {
            assert_eq!(ExitState::parse(state.as_str()), Some(state));
        }
        assert_eq!(
            ExitState::parse("sent"),
            None,
            "not this machine's vocabulary"
        );
        assert_eq!(ExitState::parse(""), None);
    }

    #[test]
    fn exit_failure_names_survive_a_round_trip_through_the_database() {
        for reason in [
            ExitFailure::NoRoute,
            ExitFailure::Construction,
            ExitFailure::Signing,
            ExitFailure::Broadcast,
            ExitFailure::NotConfirmed,
            ExitFailure::ShuttingDown,
        ] {
            assert_eq!(ExitFailure::parse(reason.as_str()), Some(reason));
        }
        assert_eq!(ExitFailure::parse("operator"), None);
    }

    #[test]
    fn the_ui_sees_camel_case_exit_states() {
        let json = serde_json::to_string(&ExitState::ExitBroadcast).expect("serialises");
        assert_eq!(json, "\"exitBroadcast\"");
        let outcome = ExitState::ExitBroadcast
            .fail(ExitFailure::NotConfirmed)
            .expect("fails");
        let json = serde_json::to_value(outcome).expect("serialises");
        assert_eq!(json["leftOnNetwork"], serde_json::json!(true));
        assert_eq!(json["reason"], serde_json::json!("notConfirmed"));
    }
}
