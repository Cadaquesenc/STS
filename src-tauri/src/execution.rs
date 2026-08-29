//! Flattening a position: building the exit, signing it, sending it, and
//! writing down what happened.
//!
//! This is the module `engine.rs` reaches for when an unwind has to do more
//! than stop managing a position. Everything above it decides *whether* to get
//! out; this decides *how*, and it is the only place in the process that
//! produces bytes a signer could put on the network.
//!
//! Five ideas hold it together.
//!
//! **The signer is behind a trait and there is no live one.** `ExecutionEngine`
//! is the whole outbound surface, and the only implementation in this build is
//! `MockSolanaSigner`, whose `is_live` is `false`. The roadmap's Phase 4 risk
//! gate says the dispatcher stays simulation-only until it is explicitly
//! promoted; a trait with one honest mock is what that looks like in code
//! rather than in a comment. `Engine` starts with no backend at all, so an
//! unwind on the shipped build behaves exactly as it did before this module
//! existed: it halts, it abandons, it sends nothing, and it says so.
//!
//! **Every step is durable before it is taken.** The signature is written to
//! `intent_transitions` *before* the broadcast, not after. A process that dies
//! between the two must come back knowing a transaction with that signature may
//! be on the network, because the alternative is reconciling by selling the
//! position a second time. The writes are best-effort in the other direction —
//! a failing disk is recorded as a problem and never stops an exit, which is
//! `RISK_AND_SYBIL_SPEC.md` §12.4: the exit does not wait for a commit.
//!
//! **An exit is a new intent, never an edit.** U2 says a resolved obligation is
//! new rows. So a flattening mints its own `intent_id`, walks the ordinary
//! six-state machine in `execution_logs` as a `sell`, and records its finer
//! lifecycle in `intent_transitions` beside it, joined by `origin_intent_id`.
//! Nothing here updates a row that already exists.
//!
//! **A conditional obligation is reconciled, not sold.** An intent whose last
//! state was `sent` may never have landed. §13.1 is explicit that selling a
//! position which does not exist, because an abort assumed the worst, is its
//! own incident. So those go through `ExecutionEngine::resolve` first, and only
//! a backend that says the entry landed produces an exit.
//!
//! **A retry is the same bytes, never new ones.** A transaction that reaches a
//! node and is never heard of again is the ordinary case, not the exotic one,
//! and the answer to it is `broadcast_until_settled`: send the identical signed
//! transaction again, on a bounded backoff, until it lands or its blockhash is
//! gone. That is safe for exactly one reason — the signature has not changed,
//! so a cluster that already executed it drops the duplicate. Everything that
//! would change the bytes is therefore held out of that loop. A fresher
//! blockhash and a bigger tip are a *new* exit at the next attempt number, and
//! Annex C.2 is the rule they follow: the exit keeps its identity while the tip
//! escalation climbs, and it is only allowed once the first signature can no
//! longer land. The tip itself is priced by `TipPolicy` and paid by the last
//! instruction in the transaction, which makes it as atomic as the sale — there
//! is no state in which a validator was paid for a bundle that sold nothing.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::alerting::{AlertDispatcher, Observation};
use crate::db::{
    Database, ExecutionLogRow, ExecutionMode, ExitAttempt, IntentTransitionRow, OpenObligation,
    Side,
};
use crate::error::EngineError;
use crate::journal::{
    FillRow, RouteDecision, RouteRow, SignatureKind, SignatureRow, SignatureStatus, TipRow,
    TradeRow,
};
use crate::metrics::MetricsCollector;
use crate::replay::{
    slippage_bps, CurveState, Fill, QuoteError, TransactionCosts, BPS_DENOMINATOR, DEFAULT_FEE_BPS,
};
use crate::strategy::fixed::Q18;
use crate::types::{AbortReason, ExecutionState, ExitFailure, ExitState, Pubkey, Signature, Venue};

// ---------------------------------------------------------------------------
// programs
// ---------------------------------------------------------------------------

/// pump.fun's bonding curve program, as `ingestion.rs` spells it.
pub const PUMP_FUN_PROGRAM: &str = crate::ingestion::PUMP_FUN_PROGRAM;
/// Raydium's V4 constant-product AMM.
pub const RAYDIUM_AMM_V4_PROGRAM: &str = crate::ingestion::RAYDIUM_AMM_V4_PROGRAM;
/// The compute budget program, which every exit sets a price and a ceiling on.
pub const COMPUTE_BUDGET_PROGRAM: &str = "ComputeBudget111111111111111111111111111111";
/// The system program. All thirty-two zero bytes.
pub const SYSTEM_PROGRAM: &str = "11111111111111111111111111111111";
/// SPL Token.
pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
/// The associated token account program.
pub const ASSOCIATED_TOKEN_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";

/// The eight-byte Anchor discriminator for pump.fun's `sell`.
///
/// `sha256("global:sell")[..8]`. Held as a constant rather than hashed at
/// startup because it never changes and the hot path should not hash a string
/// literal — and `the_sell_discriminator_is_the_hash_it_claims_to_be` recomputes
/// it from the digest, so a typo here fails a test rather than a transaction.
pub const PUMP_FUN_SELL_DISCRIMINATOR: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];

/// Raydium AMM V4's `swapBaseIn`, which is a bare tag rather than a hash.
pub const RAYDIUM_SWAP_BASE_IN: u8 = 9;

/// Raydium V4's swap fee, in basis points. 0.25%.
pub const RAYDIUM_FEE_BPS: u16 = 25;

/// The compute unit ceiling an exit is built with.
///
/// Generous rather than tuned: an exit that fails because it ran out of compute
/// is the most expensive possible way to save a fraction of a lamport, and the
/// unused portion is not charged.
pub const EXIT_COMPUTE_UNIT_LIMIT: u32 = 200_000;

/// The default priority price, in micro-lamports per compute unit.
///
/// A floor, not a policy. The tip controller in the roadmap's Phase 4 owns the
/// real number; until it exists an exit still has to be built with something,
/// and something small and fixed is the honest placeholder.
///
/// **UNMEASURED, AND IT IS THE NUMBER THE WHOLE THING TURNS ON AT OUR SIZE.**
/// Read this before quoting any cost figure that includes it.
///
/// 10_000 micro-lamports over `EXIT_COMPUTE_UNIT_LIMIT` is 2_000 lamports of
/// priority fee. With the 5_000-lamport signature fee and, on the exit,
/// `EXIT_TIP_BASE_LAMPORTS`, a modelled round trip pays roughly 24_000 lamports
/// of flat landing cost — 0.05% of a 0.05 SOL order, which rounds to nothing
/// beside the two 1% swap fees, and that is exactly why it has never been
/// argued about.
///
/// It is not a measurement. Nobody has watched what a pump.fun launch snipe
/// actually has to pay to land in the first block, and the flat cost does not
/// scale with the order, so it is the term that dominates at the bankroll's
/// real order sizes. A competitive priority fee of a hundredth of a SOL would
/// be five hundred times this constant and would cost about a fifth of a
/// 0.01 SOL position before the trade has an opinion about anything.
///
/// Deliberately not "fixed" by picking a bigger constant: a made-up number that
/// looks careful is worse than a made-up number that says it is one. What this
/// needs is a measurement off real landed transactions, and until there is one,
/// every cost figure this constant feeds should be read as a lower bound.
pub const EXIT_COMPUTE_UNIT_PRICE: u64 = 10_000;

/// The worst fill an emergency exit will accept, in basis points.
///
/// Wide on purpose. This is the number that decides whether a position can be
/// left behind, and `RISK_AND_SYBIL_SPEC.md` §12.4 says degraded conditions
/// widen an exit's limits conservatively rather than blocking it. It is still a
/// bound: an exit is never built without one, because a sell with no floor is
/// how a thin pool takes the whole position.
pub const EMERGENCY_MAX_SLIPPAGE_BPS: u16 = 2_500;

// ---------------------------------------------------------------------------
// transaction wire format
// ---------------------------------------------------------------------------

/// One account an instruction touches, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountMeta {
    pub pubkey: Pubkey,
    pub is_signer: bool,
    pub is_writable: bool,
}

impl AccountMeta {
    pub const fn readonly(pubkey: Pubkey) -> Self {
        AccountMeta {
            pubkey,
            is_signer: false,
            is_writable: false,
        }
    }

    pub const fn writable(pubkey: Pubkey) -> Self {
        AccountMeta {
            pubkey,
            is_signer: false,
            is_writable: true,
        }
    }

    pub const fn signer(pubkey: Pubkey) -> Self {
        AccountMeta {
            pubkey,
            is_signer: true,
            is_writable: true,
        }
    }
}

/// One instruction, before it is compiled into a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    pub program_id: Pubkey,
    pub accounts: Vec<AccountMeta>,
    pub data: Vec<u8>,
}

/// Why a set of instructions could not become a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    /// More distinct accounts than an index byte can name.
    TooManyAccounts(usize),
    /// The fee payer has to sign, and nothing else can be first.
    NoFeePayer,
    /// An instruction named an account the message does not carry. Unreachable
    /// — every account is collected before any index is resolved — and an error
    /// rather than an `unwrap`, because a panic on the exit path arms the kill
    /// switch and takes the process down mid-unwind.
    UnknownAccount,
    /// A transaction with no instructions is not a transaction.
    Empty,
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::TooManyAccounts(n) => {
                write!(f, "{n} accounts is more than one transaction can index")
            }
            CompileError::NoFeePayer => f.write_str("the fee payer is missing"),
            CompileError::UnknownAccount => {
                f.write_str("an instruction named an account the message does not carry")
            }
            CompileError::Empty => f.write_str("a transaction with no instructions"),
        }
    }
}

impl std::error::Error for CompileError {}

/// An instruction with its accounts replaced by indexes into the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledInstruction {
    pub program_id_index: u8,
    pub accounts: Vec<u8>,
    pub data: Vec<u8>,
}

/// A legacy Solana message: the part of a transaction that gets signed.
///
/// Legacy rather than v0 because an exit built here touches a dozen accounts
/// that are all named directly. Address lookup tables save bytes on
/// transactions that reference far more than that, and they add a dependency —
/// the table has to exist on chain and be current — to the one path that must
/// have as few dependencies as possible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub num_required_signatures: u8,
    pub num_readonly_signed: u8,
    pub num_readonly_unsigned: u8,
    pub account_keys: Vec<Pubkey>,
    pub recent_blockhash: [u8; 32],
    pub instructions: Vec<CompiledInstruction>,
}

impl Message {
    /// Orders the accounts, resolves the indexes, and produces the message.
    ///
    /// The ordering is the protocol's, not a choice: writable signers, then
    /// readonly signers, then writable non-signers, then readonly non-signers,
    /// with the fee payer first. An account named twice with different
    /// permissions gets the union of them, because a validator sees one entry
    /// per account and the stricter requirement is the one that has to hold.
    pub fn compile(
        payer: Pubkey,
        instructions: &[Instruction],
        recent_blockhash: [u8; 32],
    ) -> Result<Message, CompileError> {
        if instructions.is_empty() {
            return Err(CompileError::Empty);
        }

        // Insertion-ordered so compilation is deterministic: the same
        // instructions always produce the same message, which is what makes a
        // replay byte-identical and a signature reproducible.
        let mut order: Vec<Pubkey> = Vec::new();
        let mut seen: HashMap<Pubkey, (bool, bool)> = HashMap::new();
        let mut note = |key: Pubkey, is_signer: bool, is_writable: bool| {
            let entry = seen.entry(key).or_insert_with(|| {
                order.push(key);
                (false, false)
            });
            entry.0 |= is_signer;
            entry.1 |= is_writable;
        };

        note(payer, true, true);
        for instruction in instructions {
            for account in &instruction.accounts {
                note(account.pubkey, account.is_signer, account.is_writable);
            }
        }
        // A program is never a signer and is never written to.
        for instruction in instructions {
            note(instruction.program_id, false, false);
        }

        let rank = |key: &Pubkey| -> u8 {
            let (is_signer, is_writable) = seen.get(key).copied().unwrap_or((false, false));
            match (is_signer, is_writable) {
                (true, true) => 0,
                (true, false) => 1,
                (false, true) => 2,
                (false, false) => 3,
            }
        };

        let mut account_keys = order;
        // Stable, so accounts of equal rank keep the order they were named in.
        account_keys.sort_by_key(|key| {
            if *key == payer {
                (0u8, 0usize)
            } else {
                (rank(key), 1)
            }
        });

        if account_keys.len() > u8::MAX as usize {
            return Err(CompileError::TooManyAccounts(account_keys.len()));
        }
        if account_keys.first() != Some(&payer) {
            return Err(CompileError::NoFeePayer);
        }

        let index_of = |key: &Pubkey| -> Option<u8> {
            account_keys
                .iter()
                .position(|candidate| candidate == key)
                .and_then(|i| u8::try_from(i).ok())
        };

        let mut compiled = Vec::with_capacity(instructions.len());
        for instruction in instructions {
            let program_id_index =
                index_of(&instruction.program_id).ok_or(CompileError::UnknownAccount)?;
            let mut accounts = Vec::with_capacity(instruction.accounts.len());
            for account in &instruction.accounts {
                accounts.push(index_of(&account.pubkey).ok_or(CompileError::UnknownAccount)?);
            }
            compiled.push(CompiledInstruction {
                program_id_index,
                accounts,
                data: instruction.data.clone(),
            });
        }

        let count = |want: (bool, bool)| -> u8 {
            let total = account_keys
                .iter()
                .filter(|key| seen.get(*key).copied().unwrap_or((false, false)) == want)
                .count();
            u8::try_from(total).unwrap_or(u8::MAX)
        };

        Ok(Message {
            num_required_signatures: count((true, true)).saturating_add(count((true, false))),
            num_readonly_signed: count((true, false)),
            num_readonly_unsigned: count((false, false)),
            account_keys,
            recent_blockhash,
            instructions: compiled,
        })
    }

    /// The bytes a signer signs.
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.account_keys.len() * 32);
        out.push(self.num_required_signatures);
        out.push(self.num_readonly_signed);
        out.push(self.num_readonly_unsigned);
        write_compact_len(&mut out, self.account_keys.len());
        for key in &self.account_keys {
            out.extend_from_slice(key.as_bytes());
        }
        out.extend_from_slice(&self.recent_blockhash);
        write_compact_len(&mut out, self.instructions.len());
        for instruction in &self.instructions {
            out.push(instruction.program_id_index);
            write_compact_len(&mut out, instruction.accounts.len());
            out.extend_from_slice(&instruction.accounts);
            write_compact_len(&mut out, instruction.data.len());
            out.extend_from_slice(&instruction.data);
        }
        out
    }
}

/// A message plus the signatures over it, in the form a node accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    pub signatures: Vec<Signature>,
    pub message: Message,
}

impl Transaction {
    /// The wire form: the signatures, then the message.
    pub fn serialize(&self) -> Vec<u8> {
        let message = self.message.serialize();
        let mut out = Vec::with_capacity(message.len() + self.signatures.len() * 64 + 2);
        write_compact_len(&mut out, self.signatures.len());
        for signature in &self.signatures {
            out.extend_from_slice(signature.as_bytes());
        }
        out.extend_from_slice(&message);
        out
    }

    /// Whether there are as many signatures as the message says it needs.
    ///
    /// A node rejects a transaction that is short of signatures, and it is
    /// worth catching here rather than at the far end of a network round trip
    /// during an emergency.
    pub fn is_fully_signed(&self) -> bool {
        self.signatures.len() == self.message.num_required_signatures as usize
    }
}

/// Solana's compact-u16: seven bits a byte, low group first, high bit set on
/// every byte but the last.
fn write_compact_len(out: &mut Vec<u8>, len: usize) {
    let mut remaining = len;
    loop {
        let mut byte = (remaining & 0x7f) as u8;
        remaining >>= 7;
        if remaining == 0 {
            out.push(byte);
            return;
        }
        byte |= 0x80;
        out.push(byte);
    }
}

/// Reads back what `write_compact_len` wrote. Only the tests need this, but it
/// is what makes the encoder checkable rather than merely plausible.
pub fn read_compact_len(bytes: &[u8]) -> Option<(usize, usize)> {
    let mut value = 0usize;
    for (i, byte) in bytes.iter().enumerate().take(3) {
        value |= ((byte & 0x7f) as usize) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// the well-known keys, decoded
// ---------------------------------------------------------------------------

// The decoded form of each program id above, so building an exit does not parse
// base58 and cannot fail on a key that is a compile-time constant.
// `program_keys_match_their_text` re-derives every one of these from the string
// beside it, which is what makes these numbers checkable rather than trusted —
// the same argument `ingestion.rs` makes for the allowlist.
const COMPUTE_BUDGET_BYTES: [u8; 32] = [
    0x03, 0x06, 0x46, 0x6f, 0xe5, 0x21, 0x17, 0x32, 0xff, 0xec, 0xad, 0xba, 0x72, 0xc3, 0x9b, 0xe7,
    0xbc, 0x8c, 0xe5, 0xbb, 0xc5, 0xf7, 0x12, 0x6b, 0x2c, 0x43, 0x9b, 0x3a, 0x40, 0x00, 0x00, 0x00,
];
const TOKEN_BYTES: [u8; 32] = [
    0x06, 0xdd, 0xf6, 0xe1, 0xd7, 0x65, 0xa1, 0x93, 0xd9, 0xcb, 0xe1, 0x46, 0xce, 0xeb, 0x79, 0xac,
    0x1c, 0xb4, 0x85, 0xed, 0x5f, 0x5b, 0x37, 0x91, 0x3a, 0x8c, 0xf5, 0x85, 0x7e, 0xff, 0x00, 0xa9,
];
const ASSOCIATED_TOKEN_BYTES: [u8; 32] = [
    0x8c, 0x97, 0x25, 0x8f, 0x4e, 0x24, 0x89, 0xf1, 0xbb, 0x3d, 0x10, 0x29, 0x14, 0x8e, 0x0d, 0x83,
    0x0b, 0x5a, 0x13, 0x99, 0xda, 0xff, 0x10, 0x84, 0x04, 0x8e, 0x7b, 0xd8, 0xdb, 0xe9, 0xf8, 0x59,
];
const PUMP_FUN_BYTES: [u8; 32] = [
    0x01, 0x56, 0xe0, 0xf6, 0x93, 0x66, 0x5a, 0xcf, 0x44, 0xdb, 0x15, 0x68, 0xbf, 0x17, 0x5b, 0xaa,
    0x51, 0x89, 0xcb, 0x97, 0xf5, 0xd2, 0xff, 0x3b, 0x65, 0x5d, 0x2b, 0xb6, 0xfd, 0x6d, 0x18, 0xb0,
];
const RAYDIUM_AMM_V4_BYTES: [u8; 32] = [
    0x4b, 0xd9, 0x49, 0xc4, 0x36, 0x02, 0xc3, 0x3f, 0x20, 0x77, 0x90, 0xed, 0x16, 0xa3, 0x52, 0x4c,
    0xa1, 0xb9, 0x97, 0x5c, 0xf1, 0x21, 0xa2, 0xa9, 0x0c, 0xff, 0xec, 0x7d, 0xf8, 0xb6, 0x8a, 0xcd,
];

/// The compute budget program.
pub const COMPUTE_BUDGET_KEY: Pubkey = Pubkey::new(COMPUTE_BUDGET_BYTES);
/// The system program, which is thirty-two zero bytes.
pub const SYSTEM_KEY: Pubkey = Pubkey::ZERO;
/// SPL Token.
pub const TOKEN_KEY: Pubkey = Pubkey::new(TOKEN_BYTES);
/// The associated token account program.
pub const ASSOCIATED_TOKEN_KEY: Pubkey = Pubkey::new(ASSOCIATED_TOKEN_BYTES);
/// pump.fun's bonding curve program.
pub const PUMP_FUN_KEY: Pubkey = Pubkey::new(PUMP_FUN_BYTES);
/// Raydium's V4 AMM.
pub const RAYDIUM_AMM_V4_KEY: Pubkey = Pubkey::new(RAYDIUM_AMM_V4_BYTES);

// ---------------------------------------------------------------------------
// compute budget
// ---------------------------------------------------------------------------

/// `SetComputeUnitLimit`. Instruction tag 2, then a `u32` of units.
pub fn set_compute_unit_limit(units: u32) -> Instruction {
    let mut data = Vec::with_capacity(5);
    data.push(2u8);
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_KEY,
        accounts: Vec::new(),
        data,
    }
}

/// `SetComputeUnitPrice`. Instruction tag 3, then micro-lamports per unit.
pub fn set_compute_unit_price(micro_lamports: u64) -> Instruction {
    let mut data = Vec::with_capacity(9);
    data.push(3u8);
    data.extend_from_slice(&micro_lamports.to_le_bytes());
    Instruction {
        program_id: COMPUTE_BUDGET_KEY,
        accounts: Vec::new(),
        data,
    }
}

// ---------------------------------------------------------------------------
// the system program
// ---------------------------------------------------------------------------

/// `SystemProgram::Transfer`. Instruction tag 2 as a little-endian `u32`, then
/// the lamports.
///
/// The tag is four bytes rather than one because the system program's
/// instruction enum is bincode-encoded, which is the one place in an exit where
/// the encoding differs from the compute budget program's single tag byte
/// beside it. `a_transfer_is_four_tag_bytes_and_eight_of_lamports` pins it.
pub fn system_transfer(from: Pubkey, to: Pubkey, lamports: u64) -> Instruction {
    let mut data = Vec::with_capacity(12);
    data.extend_from_slice(&2u32.to_le_bytes());
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: SYSTEM_KEY,
        accounts: vec![AccountMeta::signer(from), AccountMeta::writable(to)],
        data,
    }
}

// ---------------------------------------------------------------------------
// jito tips
// ---------------------------------------------------------------------------

/// Jito's mainnet tip accounts.
///
/// Eight rather than one, and the choice between them is a real decision rather
/// than a formality: a tip is a transfer, a transfer takes a write lock on its
/// destination, and every searcher paying into the same account would serialise
/// the lot of them behind that one lock at exactly the moment none of them can
/// afford to wait. Spreading across the list is what keeps a tip from becoming
/// the reason a bundle is late.
///
/// **These are published addresses and nothing in this build checks them
/// against a network.** The decoded bytes are pinned beside the text and
/// `jito_tip_accounts_match_their_text` proves each is the other, which catches
/// a typo and cannot catch a stale list — the same caveat the pump.fun account
/// layout above carries, for the same reason. A tip sent to an address that is
/// no longer a tip account is a donation, so promoting a live backend means
/// re-reading this list from the block engine it will actually talk to.
pub const JITO_TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

/// The same eight, decoded, so building an exit parses no base58 and cannot
/// fail on a value that is a compile-time constant.
pub const JITO_TIP_KEYS: [Pubkey; 8] = [
    Pubkey::new([
        0x78, 0x52, 0x1c, 0xb1, 0x79, 0xce, 0xbb, 0x85, 0x89, 0xb5, 0x56, 0xa2, 0xd5, 0xec, 0x94,
        0xd2, 0x49, 0x86, 0x82, 0xfd, 0xf9, 0xbb, 0x2a, 0xf5, 0xad, 0x64, 0xe4, 0x91, 0xcc, 0x41,
        0x53, 0xda,
    ]),
    Pubkey::new([
        0xf1, 0x87, 0xec, 0x87, 0xd1, 0xf7, 0x45, 0xcb, 0x3a, 0x03, 0x38, 0x4a, 0x26, 0xa6, 0x9e,
        0xda, 0x0c, 0xa2, 0xd1, 0xaa, 0x0f, 0x41, 0xe4, 0x24, 0x16, 0x37, 0x7e, 0x91, 0xff, 0x5b,
        0x5d, 0x31,
    ]),
    Pubkey::new([
        0xb1, 0x4e, 0x0d, 0xe5, 0x5e, 0x9f, 0xba, 0x86, 0x39, 0x6e, 0xbf, 0xd5, 0x48, 0xcf, 0xf8,
        0xc9, 0x20, 0x11, 0xea, 0xc7, 0xb7, 0x5b, 0xaa, 0x9b, 0x2d, 0x9c, 0x6a, 0x86, 0xf5, 0xa1,
        0x71, 0x41,
    ]),
    Pubkey::new([
        0x88, 0xf1, 0xff, 0xa3, 0xa2, 0xdf, 0xe6, 0x17, 0xbd, 0xc4, 0xe3, 0x57, 0x32, 0x51, 0xa3,
        0x22, 0xe3, 0xfc, 0xae, 0x81, 0xe5, 0xa4, 0x57, 0x39, 0x0e, 0x64, 0x75, 0x1c, 0x00, 0xa4,
        0x65, 0xe2,
    ]),
    Pubkey::new([
        0xbc, 0x2b, 0x57, 0x06, 0x5e, 0xf1, 0xdd, 0x66, 0x54, 0x30, 0xbe, 0x60, 0x6b, 0xa6, 0x59,
        0x6c, 0x02, 0x95, 0x30, 0x1b, 0xad, 0xef, 0x8b, 0x5a, 0xfc, 0x41, 0x01, 0x41, 0x50, 0xf4,
        0x12, 0x74,
    ]),
    Pubkey::new([
        0x89, 0x07, 0x7d, 0x55, 0xa5, 0xbb, 0x13, 0x30, 0x76, 0x3e, 0xb7, 0x67, 0xf5, 0x5e, 0xc0,
        0x77, 0xb4, 0x1a, 0x0d, 0x07, 0x5f, 0x7d, 0xe1, 0xd7, 0x3f, 0xba, 0xca, 0x3c, 0x63, 0xd5,
        0x54, 0x71,
    ]),
    Pubkey::new([
        0xbf, 0x97, 0x1b, 0x59, 0x10, 0x8b, 0x5b, 0x85, 0xa0, 0x4f, 0xb0, 0x93, 0xf1, 0xe2, 0x1b,
        0x4e, 0x3f, 0xd4, 0xc4, 0xc8, 0xf4, 0x87, 0xdd, 0x09, 0xb9, 0x57, 0x52, 0x76, 0x9f, 0x0d,
        0xd8, 0xc3,
    ]),
    Pubkey::new([
        0x20, 0x26, 0x10, 0x1e, 0xc2, 0x03, 0x28, 0x96, 0x4a, 0x32, 0xab, 0xab, 0x13, 0x6c, 0x54,
        0x05, 0xb9, 0x1f, 0x3a, 0xe3, 0x8e, 0xe4, 0xf6, 0x4c, 0xb6, 0xbd, 0xe8, 0x79, 0xb8, 0x68,
        0x38, 0xd2,
    ]),
];

/// The smallest tip a block engine will look at, in lamports.
///
/// A bid under this is not a cheap bundle, it is a bundle that does not exist,
/// and a policy whose ceiling is below it is refused rather than quietly
/// rounded up — spending more than an operator configured is not this module's
/// decision to make.
pub const JITO_MIN_TIP_LAMPORTS: u64 = 1_000;

/// The default floor an exit tips, in lamports. `Tip_base` in Annex C.
pub const EXIT_TIP_BASE_LAMPORTS: u64 = 10_000;

/// The default ceiling. `Tip_max` in Annex C, and the number C.2 says to block
/// on rather than exceed.
///
/// A hundredth of a SOL. Wide enough that a contested block is winnable,
/// narrow enough that a retry loop cannot spend a position on tips.
pub const EXIT_TIP_MAX_LAMPORTS: u64 = 10_000_000;

/// What each retry adds. `ΔTip` in Annex C's emergency escalation.
pub const EXIT_TIP_ESCALATION_LAMPORTS: u64 = 25_000;

/// The share of expected profit a discretionary tip will bid, in basis points.
/// `α` in Annex C, at the 15% the spec's worked example uses.
pub const EXIT_TIP_PARTICIPATION_BPS: u16 = 1_500;

/// Which of the tip accounts one exit pays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TipAccountSelection {
    /// One account, by index, taken modulo the list so a misconfigured index is
    /// a different account rather than a panic in an emergency exit.
    Fixed(usize),
    /// Derived from the exit's own intent id.
    ///
    /// The default, and the only one of the three that is a pure function of
    /// its inputs. That matters twice: the same exit tips the same account on
    /// every rebroadcast, so a retry does not take a second write lock; and
    /// Phase 3's first acceptance criterion is that one fixture and one seed
    /// produce byte-identical records, which a bid that depended on how many
    /// exits happened to come before it would fail on every run.
    ByIntent,
    /// The next account each time, in order.
    ///
    /// Spreads a burst of simultaneous exits perfectly evenly, and costs the
    /// determinism above to do it: the account depends on the order the process
    /// happened to build its exits in. Available because a live backend under
    /// real load may want it; not the default, because a replay cannot use it.
    RoundRobin,
}

/// What a tip is being bid for, which decides what it is allowed to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TipStance {
    /// An optional trade. Annex C.2 in full: a discretionary tip needs a real,
    /// positive expected value to be a share of, and a bid that would take more
    /// than the whole edge is blocked rather than shaved — a trade that is only
    /// worth doing if the validator takes the profit is not worth doing.
    Discretionary,
    /// Getting out of a position.
    ///
    /// The EV test does not apply, and that is deliberate rather than an
    /// oversight: an emergency exit has no edge to protect, it has a loss to
    /// stop, and blocking it for being unprofitable would leave the position on
    /// chain — which is the more expensive of the two outcomes every time. It
    /// is still bounded, by `max_lamports`, and it still escalates only within
    /// that bound.
    Emergency,
}

impl TipStance {
    pub const ALL: [TipStance; 2] = [TipStance::Discretionary, TipStance::Emergency];

    /// The name `journal_tips` stores and reads back. Here rather than in
    /// `journal.rs` for the reason every other stored enum keeps its own pair:
    /// the column's `CHECK` and this function have to agree, and they agree
    /// most reliably when adding a variant makes both of them fail to compile
    /// in the same place.
    pub const fn as_str(self) -> &'static str {
        match self {
            TipStance::Discretionary => "discretionary",
            TipStance::Emergency => "emergency",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        TipStance::ALL.into_iter().find(|s| s.as_str() == text)
    }
}

/// What one exit is allowed to tip, and where.
///
/// Annex C in a struct. The pricing is deliberately the simple policy form —
/// `Tip_base + α × EV_net`, escalating by a fixed step per retry — rather than
/// the full `α_eff` expansion, because every term the expansion adds
/// (block-space scarcity, landing probability, bundle competition) is an
/// observation of a network this build does not talk to. A coefficient
/// multiplied by a number nobody measured is not a better tip, it is the same
/// tip with a longer audit trail.
#[derive(Debug)]
pub struct TipPolicy {
    /// The accounts to choose between. Never empty in a policy that bids.
    pub accounts: Vec<Pubkey>,
    pub selection: TipAccountSelection,
    pub stance: TipStance,
    /// `Tip_base`: what every bid starts from.
    pub base_lamports: u64,
    /// `Tip_max`: the ceiling, and the thing C.2 blocks on when it is missing.
    pub max_lamports: u64,
    /// `α`, in basis points of expected profit.
    pub participation_bps: u16,
    /// `ΔTip`: what one more retry is worth.
    pub escalation_lamports: u64,
    /// Where `RoundRobin` has got to. Private, because it is the only mutable
    /// thing here and nothing outside this module should be able to move it.
    cursor: AtomicU64,
}

impl Clone for TipPolicy {
    /// Clones the configuration and *not* the cursor.
    ///
    /// A copy of a round-robin policy starts at the beginning rather than
    /// sharing a position with the original, because two policies quietly
    /// handing out the same sequence of accounts is worse than two that each
    /// spread evenly on their own.
    fn clone(&self) -> Self {
        TipPolicy {
            accounts: self.accounts.clone(),
            selection: self.selection,
            stance: self.stance,
            base_lamports: self.base_lamports,
            max_lamports: self.max_lamports,
            participation_bps: self.participation_bps,
            escalation_lamports: self.escalation_lamports,
            cursor: AtomicU64::new(0),
        }
    }
}

impl TipPolicy {
    /// The policy an exit uses: every published account, chosen by intent id,
    /// escalating per retry, capped.
    pub fn emergency() -> Self {
        TipPolicy {
            accounts: JITO_TIP_KEYS.to_vec(),
            selection: TipAccountSelection::ByIntent,
            stance: TipStance::Emergency,
            base_lamports: EXIT_TIP_BASE_LAMPORTS,
            max_lamports: EXIT_TIP_MAX_LAMPORTS,
            participation_bps: EXIT_TIP_PARTICIPATION_BPS,
            escalation_lamports: EXIT_TIP_ESCALATION_LAMPORTS,
            cursor: AtomicU64::new(0),
        }
    }

    /// The same list and the same numbers, under the rule that a tip may not
    /// cost more than the trade is worth.
    pub fn discretionary() -> Self {
        TipPolicy {
            stance: TipStance::Discretionary,
            ..TipPolicy::emergency()
        }
    }

    /// Chooses between the accounts differently.
    pub fn selecting(mut self, selection: TipAccountSelection) -> Self {
        self.selection = selection;
        self
    }

    /// Bids into a different set of accounts — a devnet list, or one account
    /// during a test that wants to name it.
    pub fn into_accounts(mut self, accounts: Vec<Pubkey>) -> Self {
        self.accounts = accounts;
        self
    }

    /// Moves the floor and the ceiling together, because a floor above a
    /// ceiling is the malformed policy C.2 blocks on and setting them one at a
    /// time is how a caller ends up with one.
    pub fn bounded(mut self, base_lamports: u64, max_lamports: u64) -> Self {
        self.base_lamports = base_lamports;
        self.max_lamports = max_lamports;
        self
    }

    /// Changes what a retry adds and what share of profit is bid.
    pub fn escalating(mut self, escalation_lamports: u64, participation_bps: u16) -> Self {
        self.escalation_lamports = escalation_lamports;
        self.participation_bps = participation_bps;
        self
    }

    /// Says why this policy could never produce a bid, or nothing.
    ///
    /// Checked before every bid rather than at construction: the fields are
    /// public, so a policy can be edited into an impossible one after it is
    /// built, and the only moment that can be caught for certain is the moment
    /// it is used.
    fn malformed(&self) -> Option<String> {
        if self.accounts.is_empty() {
            return Some(
                "a tip policy with no accounts cannot bid: there is nowhere to pay".to_string(),
            );
        }
        if self.max_lamports == 0 {
            return Some(
                "a tip policy with no ceiling cannot bid — Annex C.2 blocks on a missing \
                 Tip_max rather than inventing one"
                    .to_string(),
            );
        }
        if self.max_lamports < self.base_lamports {
            return Some(format!(
                "a ceiling of {} lamports is below the floor of {}, so no bid satisfies both",
                self.max_lamports, self.base_lamports
            ));
        }
        if self.max_lamports < JITO_MIN_TIP_LAMPORTS {
            return Some(format!(
                "a ceiling of {} lamports is under the {JITO_MIN_TIP_LAMPORTS} a block engine \
                 will look at, so every bundle it priced would be ignored",
                self.max_lamports
            ));
        }
        None
    }

    /// The account this exit pays.
    ///
    /// `ByIntent` hashes the id rather than taking bytes off it, because exit
    /// ids are UUIDv7 and share a timestamp prefix — two exits minted in the
    /// same millisecond would otherwise land on the same account, which is
    /// exactly the case the spreading exists for.
    pub fn account_for(&self, exit_intent_id: &str) -> Result<Pubkey, ExitError> {
        if let Some(why) = self.malformed() {
            return Err(ExitError::new(ExitFailure::Construction, why));
        }
        let index = match self.selection {
            TipAccountSelection::Fixed(index) => index,
            TipAccountSelection::ByIntent => {
                let digest = digest16(exit_intent_id.as_bytes(), 0);
                let mut lane = [0u8; 8];
                lane.copy_from_slice(&digest[..8]);
                u64::from_le_bytes(lane) as usize
            }
            TipAccountSelection::RoundRobin => self.cursor.fetch_add(1, Ordering::Relaxed) as usize,
        };
        Ok(self.accounts[index % self.accounts.len()])
    }

    /// What to bid, in lamports, for the `attempt`-th try at one exit.
    ///
    /// `ev_net_lamports` is the profit expected before the tip and after
    /// everything else. `None` means nobody computed one, which C.2 treats the
    /// same as a stale or negative number: no discretionary share is added, and
    /// a discretionary bid is refused outright.
    pub fn bid(
        &self,
        exit_intent_id: &str,
        ev_net_lamports: Option<i64>,
        attempt: u32,
    ) -> Result<TipBid, ExitError> {
        let account = self.account_for(exit_intent_id)?;

        // The α term, and only on a real, positive expectation.
        let participation = match ev_net_lamports {
            Some(ev) if ev > 0 => {
                let share = (ev as u128).saturating_mul(self.participation_bps as u128)
                    / BPS_DENOMINATOR as u128;
                u64::try_from(share).unwrap_or(u64::MAX)
            }
            _ => 0,
        };
        // The escalation term. Monotonic in the retry index, and it is the only
        // thing that moves between retries — Annex C.2 keeps the idempotency
        // key, the route and the slippage bound fixed while this climbs.
        let escalation = self.escalation_lamports.saturating_mul(u64::from(attempt));
        let raw = self
            .base_lamports
            .saturating_add(participation)
            .saturating_add(escalation);
        let lamports = raw.clamp(self.base_lamports, self.max_lamports);

        if lamports < JITO_MIN_TIP_LAMPORTS {
            return Err(ExitError::new(
                ExitFailure::Construction,
                format!(
                    "a bid of {lamports} lamports is under the {JITO_MIN_TIP_LAMPORTS} a block \
                     engine will look at, so the bundle would be dropped rather than lose"
                ),
            ));
        }

        if self.stance == TipStance::Discretionary {
            let Some(ev) = ev_net_lamports.filter(|ev| *ev > 0) else {
                return Err(ExitError::new(
                    ExitFailure::Construction,
                    "a discretionary tip needs an expected value to be a share of, and this \
                     trade has none that is fresh and positive"
                        .to_string(),
                ));
            };
            if i128::from(lamports) >= i128::from(ev) {
                return Err(ExitError::new(
                    ExitFailure::Construction,
                    format!(
                        "a tip of {lamports} lamports against an edge of {ev} would hand the \
                         whole trade to a validator"
                    ),
                ));
            }
        }

        Ok(TipBid {
            account,
            lamports,
            attempt,
            ev_net_lamports,
        })
    }
}

/// One priced tip, and the working behind it.
///
/// Carried on the plan rather than recomputed, because Annex J step 8 says the
/// tip is tracked alongside the bundle and a receipt that says only what was
/// paid — without which account, which retry, or what it was priced against —
/// cannot be audited afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TipBid {
    pub account: Pubkey,
    pub lamports: u64,
    /// The retry index this was priced for. Zero on a first attempt.
    pub attempt: u32,
    /// What it was a share of, or `None` where nothing was computed.
    pub ev_net_lamports: Option<i64>,
}

impl TipBid {
    /// The transfer that pays it.
    pub fn instruction(&self, payer: Pubkey) -> Instruction {
        system_transfer(payer, self.account, self.lamports)
    }
}

// ---------------------------------------------------------------------------
// who leads the next slots
// ---------------------------------------------------------------------------

/// What is known about the leader of the coming slots.
///
/// Two of the three answers send immediately and they are not the same fact.
/// They are kept apart for the reason `ConfirmOutcome` keeps `Dropped` and
/// `Expired` apart: flattened into one they could not say afterwards whether a
/// bundle went out blind or went out knowing there was nobody near enough to
/// send it to. `BroadcastRun::leader_hint` carries whichever it was, so the
/// difference survives the send rather than being lost inside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderHint {
    /// Nothing knows who leads the coming slots. Send now. This is the answer
    /// every backend in this build gives.
    Unknown,
    /// The schedule is known, and no connected leader is close enough to be
    /// worth holding a send for. Send now, on the ordinary path.
    NoneInReach,
    /// A connected block engine leads in `wait_ms`. Zero means it leads now.
    Connected { wait_ms: u64 },
}

/// Where the broadcast loop asks who leads next.
///
/// Deliberately a port with nothing behind it. Answering it needs the cluster's
/// leader schedule and the block engine's list of connected validators, and
/// both are network reads this crate cannot make: there is no HTTP client in
/// its dependencies and `is_live` is false for the only backend that exists.
/// What the seam buys today is that the send path already asks the question, so
/// a live backend adds an answer rather than a branch.
///
/// It is also, deliberately, **not** wired to the tip account choice. The eight
/// published accounts are interchangeable, and spreading across them exists to
/// keep simultaneous bundles off one write lock. A leader schedule says *when*
/// to send and never which of the eight to pay; a version of this that picked
/// an account from a schedule would be spreading on a number that means nothing
/// for the thing it was spreading.
pub trait LeaderSchedule: Send + Sync {
    /// What is known at `at_ms`, in epoch milliseconds.
    ///
    /// Called on the send path during an unwind, so an implementation answers
    /// from something it already holds. A schedule that blocks on a network
    /// read here is spending the exit's budget to find out how to spend the
    /// exit's budget.
    fn hint(&self, at_ms: i64) -> LeaderHint;
}

/// The schedule that knows nothing, which is every schedule in this build.
///
/// Exists so a backend can hold a `LeaderSchedule` without an `Option` inside
/// it, and so "what the loop does against an unfitted schedule" is a case a
/// test can name rather than one that only arises from a field being `None`.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnknownLeaderSchedule;

impl LeaderSchedule for UnknownLeaderSchedule {
    fn hint(&self, _at_ms: i64) -> LeaderHint {
        LeaderHint::Unknown
    }
}

// ---------------------------------------------------------------------------
// venue account layouts
// ---------------------------------------------------------------------------

/// The accounts pump.fun's `sell` names, in the order it names them.
///
/// The order and the flags here are the instruction's ABI. They are **not**
/// verified against a deployed program by anything in this build, because
/// nothing in this build sends a transaction: `ExecutionEngine::is_live` is
/// false for the only backend that exists, and checking this layout against the
/// live IDL is on the list of things that has to happen before that changes.
/// Writing it down explicitly, in one place, with a test that pins it, is what
/// makes that check a diff rather than an archaeology exercise.
///
/// The three fixed programs at the end are constants rather than fields, since
/// an exit that named a different token program would not be an exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PumpFunSellAccounts {
    /// The program's global config account.
    pub global: Pubkey,
    /// Where the protocol fee goes.
    pub fee_recipient: Pubkey,
    pub mint: Pubkey,
    /// The bonding curve account: the counterparty.
    pub bonding_curve: Pubkey,
    /// The curve's token account.
    pub associated_bonding_curve: Pubkey,
    /// The seller's token account.
    pub associated_user: Pubkey,
    /// The seller. The only signer.
    pub user: Pubkey,
    /// Where the creator's share of the fee goes.
    pub creator_vault: Pubkey,
    /// The program's CPI event authority.
    pub event_authority: Pubkey,
}

impl PumpFunSellAccounts {
    /// How many accounts the instruction names.
    pub const COUNT: usize = 12;

    fn metas(&self) -> Vec<AccountMeta> {
        vec![
            AccountMeta::readonly(self.global),
            AccountMeta::writable(self.fee_recipient),
            AccountMeta::readonly(self.mint),
            AccountMeta::writable(self.bonding_curve),
            AccountMeta::writable(self.associated_bonding_curve),
            AccountMeta::writable(self.associated_user),
            AccountMeta::signer(self.user),
            AccountMeta::readonly(SYSTEM_KEY),
            AccountMeta::writable(self.creator_vault),
            AccountMeta::readonly(TOKEN_KEY),
            AccountMeta::readonly(self.event_authority),
            AccountMeta::readonly(PUMP_FUN_KEY),
        ]
    }

    /// The sell itself: the discriminator, the token amount, and the floor.
    ///
    /// `min_sol_output` is the whole safety property of this instruction. The
    /// program reverts rather than filling below it, which is what turns a
    /// depleted curve into a failed transaction — visible, retryable, no money
    /// moved — instead of a fill at whatever was left.
    pub fn sell(&self, tokens: u64, min_sol_output: u64) -> Instruction {
        let mut data = Vec::with_capacity(24);
        data.extend_from_slice(&PUMP_FUN_SELL_DISCRIMINATOR);
        data.extend_from_slice(&tokens.to_le_bytes());
        data.extend_from_slice(&min_sol_output.to_le_bytes());
        Instruction {
            program_id: PUMP_FUN_KEY,
            accounts: self.metas(),
            data,
        }
    }
}

/// The accounts Raydium's V4 `swapBaseIn` names, in the order it names them.
///
/// The same caveat applies as to `PumpFunSellAccounts`: this is the layout this
/// build encodes, pinned by a test, and it is checked against the deployed
/// program before anything live is signed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RaydiumSwapAccounts {
    pub amm: Pubkey,
    pub amm_authority: Pubkey,
    pub amm_open_orders: Pubkey,
    pub amm_target_orders: Pubkey,
    pub pool_coin_token_account: Pubkey,
    pub pool_pc_token_account: Pubkey,
    pub serum_program: Pubkey,
    pub serum_market: Pubkey,
    pub serum_bids: Pubkey,
    pub serum_asks: Pubkey,
    pub serum_event_queue: Pubkey,
    pub serum_coin_vault: Pubkey,
    pub serum_pc_vault: Pubkey,
    pub serum_vault_signer: Pubkey,
    /// The token account the position is sold out of.
    pub user_source_token_account: Pubkey,
    /// The wrapped-SOL account the proceeds land in.
    pub user_destination_token_account: Pubkey,
    /// The owner of both. The only signer.
    pub user_owner: Pubkey,
}

impl RaydiumSwapAccounts {
    /// How many accounts the instruction names.
    pub const COUNT: usize = 18;

    fn metas(&self) -> Vec<AccountMeta> {
        vec![
            AccountMeta::readonly(TOKEN_KEY),
            AccountMeta::writable(self.amm),
            AccountMeta::readonly(self.amm_authority),
            AccountMeta::writable(self.amm_open_orders),
            AccountMeta::writable(self.amm_target_orders),
            AccountMeta::writable(self.pool_coin_token_account),
            AccountMeta::writable(self.pool_pc_token_account),
            AccountMeta::readonly(self.serum_program),
            AccountMeta::writable(self.serum_market),
            AccountMeta::writable(self.serum_bids),
            AccountMeta::writable(self.serum_asks),
            AccountMeta::writable(self.serum_event_queue),
            AccountMeta::writable(self.serum_coin_vault),
            AccountMeta::writable(self.serum_pc_vault),
            AccountMeta::readonly(self.serum_vault_signer),
            AccountMeta::writable(self.user_source_token_account),
            AccountMeta::writable(self.user_destination_token_account),
            AccountMeta::signer(self.user_owner),
        ]
    }

    /// `swapBaseIn`: a bare tag, the amount going in, and the floor coming out.
    pub fn swap_base_in(&self, amount_in: u64, minimum_amount_out: u64) -> Instruction {
        let mut data = Vec::with_capacity(17);
        data.push(RAYDIUM_SWAP_BASE_IN);
        data.extend_from_slice(&amount_in.to_le_bytes());
        data.extend_from_slice(&minimum_amount_out.to_le_bytes());
        Instruction {
            program_id: RAYDIUM_AMM_V4_KEY,
            accounts: self.metas(),
            data,
        }
    }
}

/// A Raydium pool's two sides, in the units they are held in.
///
/// Enough to price a swap and no more. The pool's own state has a dozen other
/// fields; none of them changes what a sell of `n` tokens comes back with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RaydiumPool {
    /// The token being sold, in base units.
    pub base_reserve: u64,
    /// SOL, in lamports.
    pub quote_reserve: u64,
}

impl RaydiumPool {
    /// Prices a sell of `tokens` into the pool.
    ///
    /// Constant product with the fee taken off the input, which is what the
    /// program does. Everything is `u128` on the way through: a pool with a
    /// large base reserve times a lamport quote overflows `u64` long before it
    /// overflows anything real, and an overflow here would price an exit at
    /// zero.
    pub fn quote_sell(&self, tokens: u64, fee_bps: u16) -> Result<Fill, QuoteError> {
        if tokens == 0 {
            return Err(QuoteError::ZeroSize);
        }
        if self.base_reserve == 0 || self.quote_reserve == 0 {
            return Err(QuoteError::Implausible);
        }

        let dx = u128::from(tokens);
        let x = u128::from(self.base_reserve);
        let y = u128::from(self.quote_reserve);
        let bps = u128::from(BPS_DENOMINATOR);
        // Clamped rather than trusted. A fee wider than the whole trade is not
        // a fee, and `overflow-checks` is on in release, so the subtraction
        // below would take the process down rather than misprice an exit.
        let fee_share = u128::from(fee_bps.min(BPS_DENOMINATOR as u16));

        let dx_after_fee = dx * (bps - fee_share) / bps;
        if dx_after_fee == 0 {
            return Err(QuoteError::ZeroSize);
        }
        let gross = y * dx / (x + dx);
        let net = y * dx_after_fee / (x + dx_after_fee);
        if net > u128::from(self.quote_reserve) {
            return Err(QuoteError::ExceedsRealSol {
                required: net.min(u128::from(u64::MAX)) as u64,
                available: self.quote_reserve,
            });
        }

        Ok(Fill {
            gross_lamports: gross.min(u128::from(u64::MAX)) as u64,
            fee_lamports: gross.saturating_sub(net).min(u128::from(u64::MAX)) as u64,
            net_lamports: net.min(u128::from(u64::MAX)) as u64,
            tokens,
            slippage_bps: slippage_bps(dx, x, fee_bps),
        })
    }
}

// ---------------------------------------------------------------------------
// what an exit is
// ---------------------------------------------------------------------------

/// One obligation, as something to be sold.
///
/// Built from an `OpenObligation` and carrying only what the exit path needs.
/// `at_risk_in` is the field that decides how it is treated: `Confirmed` is a
/// position and can be sold, `Sent` is a transaction with an unknown outcome
/// and has to be reconciled first.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitTarget {
    /// The obligation's intent id. The exit gets its own; this is what joins
    /// them.
    pub intent_id: String,
    pub mint: String,
    pub side: Side,
    /// What the entry put out, in lamports. The cost basis.
    pub size_lamports: i64,
    /// The entry's signature, when it got far enough to have one.
    pub signature: Option<String>,
    pub at_risk_in: ExecutionState,
    pub mode: ExecutionMode,
    /// When the obligation this is flattening was opened.
    ///
    /// What the journal records as the trade's `opened_at_ms`, and the reason
    /// it is carried rather than read from the clock: `journal_trades` refuses
    /// an update that changes when a trade opened, so a second exit attempt
    /// stamping the row with its own `now` would abort its own write rather
    /// than update the row.
    ///
    /// `OpenObligation::opened_at_ms` and specifically not its `at_ms` — see
    /// the note there. An unwind that cannot flatten a position appends an
    /// `aborted` row to the position's own intent, so "when it was last written
    /// to" moves between two passes over the same trade and "when it was first
    /// written" does not.
    pub opened_at_ms: i64,
}

impl ExitTarget {
    /// The obligation as something to sell, or `None` if there is in fact
    /// nothing on chain for it.
    pub fn from_obligation(obligation: &OpenObligation) -> Option<Self> {
        Some(ExitTarget {
            intent_id: obligation.intent_id.clone(),
            mint: obligation.mint.clone(),
            side: obligation.side,
            size_lamports: obligation.size_lamports,
            signature: obligation.signature.clone(),
            at_risk_in: obligation.at_risk_in()?,
            mode: obligation.mode,
            opened_at_ms: obligation.opened_at_ms,
        })
    }

    /// Whether this can be acted on without reconciling it first.
    ///
    /// False for anything left at `Sent`. §13.1: the transaction may never have
    /// landed, and selling a position that does not exist is its own incident.
    pub fn is_actionable(&self) -> bool {
        self.at_risk_in == ExecutionState::Confirmed
    }
}

/// The exact accounts and the exact liquidity one exit will go through.
///
/// One variant per venue, because the accounts and the pricing are the same
/// decision: a route that named pump.fun's accounts and priced against a
/// Raydium pool would compile and would be wrong.
///
/// The variants are 336 and 560 bytes and that gap is allowed to stand. Boxing
/// the larger one is the lint's fix and clippy says itself what it costs: the
/// type would stop being `Copy`, and so would [`ExitRoute`], which holds one by
/// value and is passed around by value through the whole exit path. Both are
/// pubkeys and reserves — arrays that a box would put behind a pointer without
/// making any smaller — and exactly one is built per exit, so the copy is not on
/// a path where 568 bytes is worth an allocation and a dereference.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitRouteKind {
    PumpFunCurve {
        accounts: PumpFunSellAccounts,
        /// The reserves the quote is taken against.
        curve: CurveState,
    },
    RaydiumAmmV4 {
        accounts: RaydiumSwapAccounts,
        pool: RaydiumPool,
    },
}

impl ExitRouteKind {
    pub const fn venue(&self) -> Venue {
        match self {
            ExitRouteKind::PumpFunCurve { .. } => Venue::PumpFunCurve,
            ExitRouteKind::RaydiumAmmV4 { .. } => Venue::RaydiumAmmV4,
        }
    }
}

/// A precomputed exit: where it goes, how much, and how bad a fill it will take.
///
/// This is `RISK_AND_SYBIL_SPEC.md` §12.3's "exact accounts, an exact path, a
/// slippage bound, and a simulation timestamp", in a struct. It comes from the
/// backend rather than from here, because resolving what is actually on chain
/// for a position — which pool, how many tokens, which blockhash is current —
/// is the one part of this that needs a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitRoute {
    pub kind: ExitRouteKind,
    /// The position, in token base units.
    pub tokens: u64,
    /// Who signs and pays.
    pub payer: Pubkey,
    pub recent_blockhash: [u8; 32],
    /// The worst fill this route will accept.
    pub max_slippage_bps: u16,
    /// The validity watermark. A route older than the policy window is
    /// re-resolved rather than sent.
    pub simulated_at_ms: i64,
}

impl ExitRoute {
    pub const fn venue(&self) -> Venue {
        self.kind.venue()
    }

    /// What this route says the position is worth right now.
    pub fn quote(&self) -> Result<Fill, QuoteError> {
        match &self.kind {
            ExitRouteKind::PumpFunCurve { curve, .. } => {
                curve.quote_sell(self.tokens, DEFAULT_FEE_BPS)
            }
            ExitRouteKind::RaydiumAmmV4 { pool, .. } => {
                pool.quote_sell(self.tokens, RAYDIUM_FEE_BPS)
            }
        }
    }

    /// The swap instruction for this route, with the floor already applied.
    pub fn swap(&self, min_out_lamports: u64) -> Instruction {
        match &self.kind {
            ExitRouteKind::PumpFunCurve { accounts, .. } => {
                accounts.sell(self.tokens, min_out_lamports)
            }
            ExitRouteKind::RaydiumAmmV4 { accounts, .. } => {
                accounts.swap_base_in(self.tokens, min_out_lamports)
            }
        }
    }
}

/// A built, unsigned exit.
///
/// Everything needed to sign it and everything needed to write down what it
/// was, in one value. The transaction inside carries no signatures yet, which
/// `ExitState::ExitConstructed` is the name for.
#[derive(Debug, Clone, PartialEq)]
pub struct ExitPlan {
    /// The exit's own intent id.
    pub exit_intent_id: String,
    /// The obligation it is flattening.
    pub origin_intent_id: String,
    pub mint: String,
    pub venue: Venue,
    pub mode: ExecutionMode,
    pub tokens: u64,
    /// What the route quoted, net of fees.
    pub expected_out_lamports: u64,
    /// The floor written into the instruction.
    pub min_out_lamports: u64,
    pub slippage_bps: u16,
    /// What the position cost to open.
    pub cost_basis_lamports: i64,
    /// What this exit bid to be included, or `None` where the backend does not
    /// tip at all. The transfer that pays it is the last instruction in the
    /// transaction below; this is the same number in a form a receipt can read
    /// without decoding instruction data.
    pub tip: Option<TipBid>,
    pub transaction: Transaction,
    /// What [`simulate_exit`] found when it read the transaction below back.
    ///
    /// A field rather than something a caller is trusted to have run: an
    /// `ExitPlan` is only built by [`build_exit`], and `build_exit` cannot
    /// produce one without a simulation to put here. That is what "simulation
    /// is mandatory" means when it is a property of the type instead of a step
    /// in a procedure somebody could reorder.
    pub simulation: ExitSimulation,
    pub constructed_at_ms: i64,
}

impl ExitPlan {
    /// The bytes a signer signs.
    pub fn message_bytes(&self) -> Vec<u8> {
        self.transaction.message.serialize()
    }
}

/// A signed exit, ready for the network.
#[derive(Debug, Clone, PartialEq)]
pub struct SignedExit {
    pub exit_intent_id: String,
    pub signature: Signature,
    pub transaction: Transaction,
}

impl SignedExit {
    /// The wire form a node would be handed.
    pub fn wire(&self) -> Vec<u8> {
        self.transaction.serialize()
    }
}

/// What an exit actually came back with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitFill {
    /// What reached the wallet, net of fees.
    pub out_lamports: u64,
    /// What was sold, in token base units.
    pub tokens: u64,
    pub fee_lamports: u64,
    pub slippage_bps: u16,
    pub slot: u64,
    pub at_ms: i64,
}

/// What following an entry's signature turned up.
#[derive(Debug, Clone, PartialEq)]
pub enum Reconciliation {
    /// There is a position, and here is the route out of it.
    Landed(Box<ExitRoute>),
    /// The transaction never landed and its blockhash has expired. There is
    /// nothing on chain, so there is nothing to sell — the obligation resolves
    /// to nothing and is closed with an audit event rather than a transaction.
    NeverLanded { detail: String },
    /// Not known yet. The obligation stays open and stays conditional; this is
    /// the honest answer and it must not be rounded to either of the others.
    Unresolved { detail: String },
}

/// What one look at a broadcast transaction found.
///
/// Three answers rather than two, and the middle one is the whole reason this
/// type exists. "It has not landed" and "it can no longer land" are the same
/// `Err` to a caller that only has `Result`, and they call for opposite
/// actions: the first is a transaction that is still live on the network and
/// wants pushing again, the second is a transaction that is gone and whose
/// position was never sold.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfirmOutcome {
    /// It landed, and this is what it filled at.
    Landed(ExitFill),
    /// No result yet, and the blockhash is still inside its window — these
    /// exact bytes can still be included by a leader that has not seen them.
    /// The same signed transaction may go out again, unchanged.
    Dropped { detail: String },
    /// The blockhash is past its window, so this signature can never land.
    /// Nothing from it is on chain and there is nothing to reconcile against
    /// it; the position is exactly where it was before the exit was built.
    Expired { detail: String },
}

impl ConfirmOutcome {
    /// The fill, if it landed. For a caller — or a test — that only cares about
    /// the one outcome that closed the position.
    pub fn landed(self) -> Option<ExitFill> {
        match self {
            ConfirmOutcome::Landed(fill) => Some(fill),
            _ => None,
        }
    }

    /// Whether this answer leaves the transaction able to land.
    pub const fn is_live(&self) -> bool {
        matches!(self, ConfirmOutcome::Dropped { .. })
    }
}

/// Why one step of an exit did not happen.
///
/// `failure` is the bucket a counter and the `intent_transitions` row use;
/// `detail` is the sentence a person reads. Both, because a receipt that says
/// only "signing" tells an operator nothing and one that says only the sentence
/// cannot be aggregated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitError {
    pub failure: ExitFailure,
    pub detail: String,
}

impl ExitError {
    pub fn new(failure: ExitFailure, detail: impl Into<String>) -> Self {
        ExitError {
            failure,
            detail: detail.into(),
        }
    }

    pub fn no_route(detail: impl Into<String>) -> Self {
        ExitError::new(ExitFailure::NoRoute, detail)
    }
}

impl std::fmt::Display for ExitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.failure, self.detail)
    }
}

impl std::error::Error for ExitError {}

// ---------------------------------------------------------------------------
// the signer interface
// ---------------------------------------------------------------------------

/// Everything the engine can ask of the outside world on the way out of a
/// position.
///
/// Four calls, in the order they happen, and each one is a place a real
/// implementation would touch a node or a key. The parts that do *not* need
/// either — deciding what to sell, encoding the instructions, compiling the
/// message, computing the floor, deciding what a fill was worth — are
/// deliberately not on this trait. They are ordinary code in this module with
/// ordinary tests, and keeping them out of the trait is what stops a backend
/// being able to quietly change what an exit means.
///
/// Implementations must be `Send + Sync`: the maintenance thread, the unwind
/// command and any future exit timer all reach the same backend, and none of
/// them owns it.
///
/// An implementation must never hold an `Arc<Engine>`. The engine holds the
/// backend, so a reference back would be a cycle that never drops, and it would
/// also let an exit re-enter the unwind that started it.
pub trait ExecutionEngine: Send + Sync {
    /// A name for the audit row and the telemetry line, so a receipt says which
    /// backend produced it.
    fn name(&self) -> &'static str;

    /// Whether this backend can put a transaction on a real network with real
    /// money behind it.
    ///
    /// The one question the promotion gate turns on. A backend that answers
    /// `true` is claiming the roadmap's Phase 4 criteria are met for it.
    fn is_live(&self) -> bool;

    /// Follows the entry, and produces the way out of what it finds.
    ///
    /// For a `Confirmed` obligation there is a position by definition and this
    /// is a routing call. For one left at `Sent` it is a reconciliation first:
    /// `NeverLanded` and `Unresolved` are both real answers and they mean very
    /// different things.
    fn resolve(&self, target: &ExitTarget) -> Result<Reconciliation, ExitError>;

    /// Signs the message. The signature is what the engine writes down before
    /// anything is broadcast.
    fn sign(&self, plan: &ExitPlan) -> Result<SignedExit, ExitError>;

    /// Puts it on the network. Returning `Ok` means a node accepted it, not
    /// that it landed.
    fn broadcast(&self, signed: &SignedExit) -> Result<(), ExitError>;

    /// Looks once at what became of it.
    ///
    /// `Ok(Dropped)` is an answer and not a failure: it is "not yet", it is the
    /// only answer a retry is allowed to act on, and an implementation that
    /// rounded it to either of the other two would be either abandoning a live
    /// transaction or re-sending a dead one. `Err` is reserved for not being
    /// able to ask — a node that would not answer, not a transaction that has
    /// not landed.
    fn confirm(&self, signed: &SignedExit) -> Result<ConfirmOutcome, ExitError>;

    /// What this backend bids to be included, or `None` if it does not tip.
    ///
    /// `None` is the honest answer for a backend that hands bytes to an
    /// ordinary RPC node: there is no block engine on that path and nobody to
    /// pay. A backend that submits bundles returns a policy, and the policy —
    /// not this module — is where the account list and the ceiling live, since
    /// which block engine is being talked to is a property of the backend.
    fn tip_policy(&self) -> Option<&TipPolicy> {
        None
    }

    /// Who leads the coming slots, or `None` where nothing knows.
    ///
    /// `None` is the honest answer for every backend here, and the reason this
    /// is a port rather than a lookup: a leader schedule is a fact about a
    /// network, it comes from the cluster and the block engine the backend
    /// talks to, and nothing in this build talks to either. A backend that has
    /// one returns it and the broadcast loop will hold a send for it. A backend
    /// that does not sends immediately, which is what happens today.
    fn leader_schedule(&self) -> Option<&dyn LeaderSchedule> {
        None
    }

    /// How hard a dropped broadcast is pushed before it is given up on.
    ///
    /// Defaulted rather than required because every backend wants the same
    /// shape and only a live one knows better numbers for its own network.
    fn broadcast_policy(&self) -> BroadcastPolicy {
        BroadcastPolicy::default()
    }
}

// ---------------------------------------------------------------------------
// broadcast, and broadcasting again
// ---------------------------------------------------------------------------

/// How long a dropped exit is pushed for, and how hard.
///
/// Every number here is a ceiling. There is no field that makes the loop run
/// longer under some condition, because the condition under which this code
/// runs at all is one where something has already gone wrong, and a retry
/// policy that can be talked into running longer during an incident is the
/// policy that turns a bad minute into a bad hour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BroadcastPolicy {
    /// How many times the *same* signed bytes may be sent again. Zero means one
    /// send and no more.
    pub max_rebroadcasts: u32,
    /// The wait before the first retry.
    pub initial_backoff_ms: u64,
    /// What each wait multiplies the last by. One is a flat retry.
    pub backoff_factor: u32,
    /// The ceiling on any single wait.
    pub max_backoff_ms: u64,
    /// The longest one send may be held back waiting for a connected leader.
    ///
    /// A ceiling like every other number here, and not a prediction: nothing in
    /// this build knows how far away the next connected leader is. Two slots at
    /// roughly 400ms each is the outer shape of a wait that could ever pay for
    /// itself — past that a bundle is better off going now and taking the
    /// ordinary path than sitting in a buffer while an unwind waits on it.
    pub max_leader_wait_ms: u64,
    /// The ceiling on the whole loop — the waits, the confirmations between
    /// them, and everything else. Measured against the clock rather than summed
    /// from the waits, so a backend that takes ten seconds to answer a
    /// confirmation cannot walk past the budget while technically obeying it.
    pub total_budget_ms: u64,
}

impl Default for BroadcastPolicy {
    /// Three retries, doubling from 400ms, inside fifteen seconds.
    ///
    /// The fifteen is the number that was chosen and the rest follow from it. A
    /// blockhash is good for about a minute, so a loop could in principle keep
    /// pushing for far longer than this — but this runs inside an unwind, an
    /// unwind is bounded by `FLATTEN_LOCK_TIMEOUT` and by an operator watching
    /// a receipt, and the positions after this one in the list are waiting.
    /// Fifteen seconds is long enough to survive a leader that dropped a packet
    /// and short enough that one stuck exit does not become the reason the rest
    /// of the book stayed open.
    fn default() -> Self {
        BroadcastPolicy {
            max_rebroadcasts: 3,
            initial_backoff_ms: 400,
            backoff_factor: 2,
            max_backoff_ms: 2_000,
            max_leader_wait_ms: 800,
            total_budget_ms: 15_000,
        }
    }
}

impl BroadcastPolicy {
    /// The wait before retry number `retry`, counting from zero.
    ///
    /// Saturating at both ends: a factor of zero is read as one rather than
    /// collapsing every wait to nothing, and a factor that would overflow stops
    /// at the ceiling instead of wrapping to a shorter wait than the one
    /// before it. A backoff that goes *down* under multiplication is the
    /// failure this arithmetic exists to make impossible.
    pub fn backoff_ms(&self, retry: u32) -> u64 {
        let factor = u64::from(self.backoff_factor.max(1));
        let mut wait = self.initial_backoff_ms;
        for _ in 0..retry {
            if wait >= self.max_backoff_ms {
                return self.max_backoff_ms;
            }
            wait = wait.saturating_mul(factor);
        }
        wait.min(self.max_backoff_ms)
    }
}

/// The clock the retry loop waits on.
///
/// Behind a trait so the backoff can be tested without a test that sleeps. A
/// test passes something that only moves a number, and its assertions are about
/// the waits that were asked for rather than about how long the suite took —
/// which is the difference between a test that proves the schedule and a test
/// that proves the machine was busy.
pub trait Waiter: Send {
    /// Waits `ms`, and answers with the wall clock afterwards in epoch
    /// milliseconds.
    fn wait(&mut self, ms: u64) -> i64;

    /// The clock now, without waiting.
    fn now_ms(&self) -> i64;
}

/// The real one: sleeps the calling thread.
#[derive(Debug, Clone, Copy, Default)]
pub struct SleepingWaiter;

impl Waiter for SleepingWaiter {
    fn wait(&mut self, ms: u64) -> i64 {
        std::thread::sleep(std::time::Duration::from_millis(ms));
        self.now_ms()
    }

    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }
}

/// One step the broadcast loop took, in the vocabulary the ledger stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadcastStep {
    pub from: ExitState,
    pub to: ExitState,
    /// Why it happened, for the row a person reads. `None` on the first
    /// broadcast, which needs no explanation.
    pub detail: Option<String>,
    pub at_ms: i64,
}

/// What one exit's time on the network came to.
#[derive(Debug)]
pub struct BroadcastRun {
    /// Every transition, in order, and the same ones handed to the recorder as
    /// they happened.
    pub steps: Vec<BroadcastStep>,
    /// How many times the same bytes went out again.
    pub rebroadcasts: u32,
    /// How long the loop asked to wait, in total. Not the same as elapsed time.
    pub waited_ms: u64,
    /// What the leader schedule last said, and `Unknown` where there was none
    /// to ask. The last rather than the first, because it is the answer that
    /// governed the send this run is reporting on.
    pub leader_hint: LeaderHint,
    /// How long sends were held back for a connected leader, in total.
    ///
    /// Apart from `waited_ms` because it is not backoff. Backoff is time spent
    /// after something failed; this is time spent before anything was tried,
    /// and an audit that added the two together could not tell a slow network
    /// from a cautious one.
    pub leader_waited_ms: u64,
    /// Where the exit ended up. `ExitSigned` means nothing ever reached a node.
    pub state: ExitState,
    /// What became of the signature, in the vocabulary `journal_signatures`
    /// stores.
    ///
    /// This loop is the only place that can tell these apart. By the time the
    /// caller has an `ExitError`, a blockhash that aged out and a transaction a
    /// node took and lost are both `ExitFailure::NotConfirmed` — and they are
    /// the two halves of the question a person asks a week later, which is
    /// whether the network was slow or the send was late. `Broadcast` means the
    /// loop is not finished; nothing returns with it set.
    pub settled_as: SignatureStatus,
    /// The fill, or why there is not one.
    pub outcome: Result<ExitFill, ExitError>,
}

/// Holds a send back until a connected block engine is up, if anything knows
/// when that is.
///
/// Answers what it was told and how long it waited. The hold is zero for every
/// backend in this build, and zero in three cases a live one can reach: nothing
/// to ask, nothing worth waiting for, and a wait the exit cannot afford. The
/// hint comes back beside it so the caller can still tell those apart, which a
/// bare zero could not.
///
/// The third case is the rule that matters. The hold sits inside the same
/// `total_budget_ms` as everything else, and a send that has to choose between
/// hitting a leader and passing the budget goes now — missing a block engine
/// costs a bundle, and passing the budget costs the exit.
fn hold_for_leader(
    schedule: Option<&dyn LeaderSchedule>,
    policy: &BroadcastPolicy,
    started_at_ms: i64,
    waiter: &mut dyn Waiter,
) -> (LeaderHint, u64) {
    let Some(schedule) = schedule else {
        return (LeaderHint::Unknown, 0);
    };
    let now = waiter.now_ms();
    let hint = schedule.hint(now);
    let LeaderHint::Connected { wait_ms } = hint else {
        return (hint, 0);
    };
    let wait = wait_ms.min(policy.max_leader_wait_ms);
    if wait == 0 {
        return (hint, 0);
    }
    let elapsed = now.saturating_sub(started_at_ms).max(0) as u64;
    if elapsed.saturating_add(wait) > policy.total_budget_ms {
        return (hint, 0);
    }
    waiter.wait(wait);
    (hint, wait)
}

/// Sends one signed exit, and keeps sending it until it lands, is known not to
/// be able to, or runs out of the budget it was given.
///
/// The retry here is the narrow one and the safe one: the *same signature*,
/// unchanged, sent again. That cannot sell the position twice, because a
/// cluster that has already executed a signature drops the duplicate — which is
/// the entire reason this is allowed to loop at all. Anything that would change
/// the bytes, a fresher blockhash or a bigger tip, produces a different
/// signature and is deliberately not done here: that is a new exit at the next
/// attempt number, and it is only safe once the first signature has expired.
///
/// Before each send — the first one and every push after it — the backend's
/// leader schedule is asked whether it is worth holding for a connected block
/// engine. Every backend in this build has no schedule and answers nothing, so
/// today this is a function call and no delay.
///
/// `record` is called with each step *before* the loop acts on the next one, so
/// a process that dies mid-retry comes back to a ledger that knows how many
/// times the bytes went out.
pub fn broadcast_until_settled(
    backend: &dyn ExecutionEngine,
    policy: &BroadcastPolicy,
    signed: &SignedExit,
    started_at_ms: i64,
    waiter: &mut dyn Waiter,
    record: &mut dyn FnMut(&BroadcastStep),
) -> BroadcastRun {
    let mut run = BroadcastRun {
        steps: Vec::new(),
        rebroadcasts: 0,
        waited_ms: 0,
        leader_hint: LeaderHint::Unknown,
        leader_waited_ms: 0,
        state: ExitState::ExitSigned,
        settled_as: SignatureStatus::Broadcast,
        outcome: Err(ExitError::new(
            ExitFailure::Broadcast,
            "the broadcast loop ended without an answer".to_string(),
        )),
    };

    let mut at = started_at_ms;
    let mut step = |run: &mut BroadcastRun, from, to, detail: Option<String>, at_ms| {
        let step = BroadcastStep {
            from,
            to,
            detail,
            at_ms,
        };
        record(&step);
        run.steps.push(step);
    };

    let schedule = backend.leader_schedule();
    let (hint, held) = hold_for_leader(schedule, policy, started_at_ms, waiter);
    run.leader_hint = hint;
    if held > 0 {
        run.leader_waited_ms = run.leader_waited_ms.saturating_add(held);
        at = waiter.now_ms();
    }

    if let Err(err) = backend.broadcast(signed) {
        // Nothing reached a node under this signature, so nothing is on the
        // network to come back later. `Failed` is the only status that says
        // that; `Dropped` and `Expired` both claim something went out.
        run.settled_as = SignatureStatus::Failed;
        run.outcome = Err(err);
        return run;
    }
    match run.state.broadcast() {
        Ok(next) => run.state = next,
        Err(transition) => {
            run.settled_as = SignatureStatus::Failed;
            // Unreachable: `state` is `ExitSigned` on the line above. Reported
            // rather than panicked on, because a panic here is a panic during
            // an emergency exit.
            run.outcome = Err(ExitError::new(
                ExitFailure::Broadcast,
                format!(
                    "{} could not be marked broadcast: {transition}",
                    signed.exit_intent_id
                ),
            ));
            return run;
        }
    }
    // `None` unless something held it: the first broadcast of an exit that went
    // straight out needs no explanation, and one that did not needs the only
    // explanation there is.
    let why = (held > 0).then(|| format!("held {held}ms for a connected leader"));
    step(
        &mut run,
        ExitState::ExitSigned,
        ExitState::ExitBroadcast,
        why,
        at,
    );

    loop {
        match backend.confirm(signed) {
            Ok(ConfirmOutcome::Landed(fill)) => {
                run.settled_as = SignatureStatus::Confirmed;
                run.outcome = Ok(fill);
                return run;
            }
            Ok(ConfirmOutcome::Expired { detail }) => {
                run.settled_as = SignatureStatus::Expired;
                run.outcome = Err(ExitError::new(ExitFailure::NotConfirmed, detail));
                return run;
            }
            Err(err) => {
                // The backend could not say. Something went out and its fate is
                // unknown, which is what `Dropped` means — not `Failed`, which
                // would claim it is over.
                run.settled_as = SignatureStatus::Dropped;
                run.outcome = Err(err);
                return run;
            }
            Ok(ConfirmOutcome::Dropped { detail }) => {
                if run.rebroadcasts >= policy.max_rebroadcasts {
                    run.settled_as = SignatureStatus::Dropped;
                    run.outcome = Err(ExitError::new(
                        ExitFailure::NotConfirmed,
                        format!(
                            "{} was sent {} time(s) and never landed: {detail}",
                            signed.signature,
                            run.rebroadcasts + 1
                        ),
                    ));
                    return run;
                }

                let wait = policy.backoff_ms(run.rebroadcasts);
                // Read afresh rather than carried from the last wait: the
                // confirmation that just returned took time of its own, and a
                // budget that only counted its own sleeping would let a slow
                // backend walk straight past it.
                at = waiter.now_ms();
                let elapsed = at.saturating_sub(started_at_ms).max(0) as u64;
                if elapsed.saturating_add(wait) > policy.total_budget_ms {
                    run.settled_as = SignatureStatus::Dropped;
                    run.outcome = Err(ExitError::new(
                        ExitFailure::NotConfirmed,
                        format!(
                            "{} had {}ms to land and waiting {wait}ms more would pass it: \
                             {detail}",
                            signed.signature, policy.total_budget_ms
                        ),
                    ));
                    return run;
                }

                at = waiter.wait(wait);
                run.waited_ms = run.waited_ms.saturating_add(wait);

                let (hint, held) = hold_for_leader(schedule, policy, started_at_ms, waiter);
                run.leader_hint = hint;
                if held > 0 {
                    run.leader_waited_ms = run.leader_waited_ms.saturating_add(held);
                    at = waiter.now_ms();
                }

                if let Err(err) = backend.broadcast(signed) {
                    // Already on the network from a previous send, so this is a
                    // failure to push rather than a failure to reach it. The
                    // state stays `ExitBroadcast`, which is what makes the
                    // abandonment that follows say the position may have sold.
                    run.settled_as = SignatureStatus::Dropped;
                    run.outcome = Err(err);
                    return run;
                }
                match run.state.rebroadcast() {
                    Ok(next) => run.state = next,
                    Err(transition) => {
                        run.settled_as = SignatureStatus::Dropped;
                        run.outcome = Err(ExitError::new(
                            ExitFailure::NotConfirmed,
                            format!(
                                "{} could not be sent again from {}: {transition}",
                                signed.exit_intent_id, run.state
                            ),
                        ));
                        return run;
                    }
                }
                run.rebroadcasts = run.rebroadcasts.saturating_add(1);
                let held_note = if held > 0 {
                    format!(" and {held}ms held for a connected leader")
                } else {
                    String::new()
                };
                let why = format!(
                    "sent again after {wait}ms{held_note}, attempt {} of {}: {detail}",
                    run.rebroadcasts + 1,
                    policy.max_rebroadcasts + 1
                );
                step(
                    &mut run,
                    ExitState::ExitBroadcast,
                    ExitState::ExitBroadcast,
                    Some(why),
                    at,
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// building one exit
// ---------------------------------------------------------------------------

/// Mints the exit's own intent id, deterministically.
///
/// UUIDv7 by layout — 48 bits of millisecond, version 7, then digest — so it
/// sorts by time exactly like every other `intent_id` in the schema. What it is
/// *not* is random, and that is the point. The bits below the timestamp are a
/// digest of `(origin_intent_id, attempt)`, which buys two things nothing else
/// does:
///
/// - **Idempotency.** The same unwind, run twice in the same millisecond,
///   produces the same id, so the `(intent_id, seq)` conflict target turns the
///   second pass into a no-op instead of a second exit for one position.
/// - **Replay determinism.** Phase 3's first acceptance criterion is that the
///   same fixture and seed produce byte-identical records. A random id would
///   fail it on every run.
///
/// Uniqueness comes from `(origin_intent_id, attempt)` being unique, not from
/// the digest; the digest only spreads that pair across the bits a UUID has.
pub fn exit_intent_id(origin_intent_id: &str, attempt: u32, at_ms: i64) -> String {
    let ms = (at_ms.max(0) as u64) & 0xffff_ffff_ffff;
    let digest = digest16(origin_intent_id.as_bytes(), attempt);

    let mut bytes = [0u8; 16];
    bytes[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    // Version 7 in the high nibble, twelve bits of digest under it.
    bytes[6] = 0x70 | (digest[0] & 0x0f);
    bytes[7] = digest[1];
    // The RFC 4122 variant, two bits, then sixty-two bits of digest.
    bytes[8] = 0x80 | (digest[2] & 0x3f);
    bytes[9..].copy_from_slice(&digest[3..10]);

    let mut out = String::with_capacity(36);
    for (i, byte) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push(nibble_char(byte >> 4));
        out.push(nibble_char(byte & 0x0f));
    }
    out
}

const fn nibble_char(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        _ => (b'a' + value - 10) as char,
    }
}

/// Sixteen bytes of FNV-1a over `seed` and `salt`, in two independent lanes.
///
/// Not a cryptographic hash and not trying to be, for the reason `db.rs` gives
/// about migration checksums: the only thing it defends against is two
/// obligations colliding by accident, and the pair it runs over is already
/// unique.
fn digest16(seed: &[u8], salt: u32) -> [u8; 16] {
    const OFFSET_A: u64 = 0xcbf2_9ce4_8422_2325;
    const OFFSET_B: u64 = 0x9e37_79b9_7f4a_7c15;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut a = OFFSET_A;
    let mut b = OFFSET_B;
    let mut feed = |byte: u8| {
        a ^= u64::from(byte);
        a = a.wrapping_mul(PRIME);
        b = b.rotate_left(7) ^ u64::from(byte);
        b = b.wrapping_mul(PRIME);
    };
    for byte in seed {
        feed(*byte);
    }
    feed(b'#');
    for byte in salt.to_le_bytes() {
        feed(byte);
    }

    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&a.to_be_bytes());
    out[8..].copy_from_slice(&b.to_be_bytes());
    out
}

/// The floor an exit will not fill below, from a quote and a slippage bound.
///
/// Saturating and clamped, and never zero while the quote is not: a floor of
/// zero is not a floor, and it is exactly the input a thin pool needs to take
/// the whole position. One lamport is a real constraint even when it is a small
/// one.
pub fn exit_floor_lamports(expected_out_lamports: u64, max_slippage_bps: u16) -> u64 {
    if expected_out_lamports == 0 {
        return 0;
    }
    let bps = u128::from(BPS_DENOMINATOR);
    let give = u128::from(max_slippage_bps.min(BPS_DENOMINATOR as u16));
    let floor = u128::from(expected_out_lamports) * (bps - give) / bps;
    (floor.min(u128::from(u64::MAX)) as u64).max(1)
}

/// Builds the one transaction that flattens one position.
///
/// **Atomic by construction, and simulated before it is returned.** A Solana
/// transaction either applies every one of its instructions or none of them, so
/// the compute budget, the swap, the slippage floor and the tip are one
/// all-or-nothing unit. There is no state in which the budget was set and the
/// sell was not, no state in which a validator was paid for a bundle that did
/// not sell anything, and no partial exit to reconcile from this side — a
/// partial fill can only come from the program itself, against the floor, and
/// the floor is in the instruction data.
///
/// That paragraph used to be a comment. It is now a check: the message this
/// function compiles is simulated before the [`ExitPlan`] that carries it
/// exists, every claim above is read back out of the compiled bytes, and an
/// exit that fails is never returned and therefore never signed. What the
/// simulation found travels on the plan as [`ExitPlan::simulation`].
///
/// `tip` is the policy to price a Jito tip with, or `None` for a backend that
/// broadcasts to an ordinary RPC node and has nobody to tip. `attempt` is the
/// retry index the tip escalates on: Annex C.2 keeps the exit's identity fixed
/// across a retry and moves only this.
pub fn build_exit(
    target: &ExitTarget,
    route: &ExitRoute,
    tip: Option<&TipPolicy>,
    exit_intent_id: String,
    attempt: u32,
    now_ms: i64,
) -> Result<ExitPlan, ExitError> {
    if target.size_lamports <= 0 {
        return Err(ExitError::new(
            ExitFailure::Construction,
            format!(
                "{} says it put {} lamports out, which is not a position",
                target.intent_id, target.size_lamports
            ),
        ));
    }
    if route.tokens == 0 {
        return Err(ExitError::no_route(format!(
            "{} routes to zero tokens, so there is nothing to sell",
            target.intent_id
        )));
    }

    let fill = route
        .quote()
        .map_err(|err| ExitError::no_route(format!("{} cannot be quoted: {err}", target.mint)))?;
    if fill.net_lamports == 0 {
        return Err(ExitError::no_route(format!(
            "{} quotes to nothing after fees",
            target.mint
        )));
    }

    let min_out_lamports = exit_floor_lamports(fill.net_lamports, route.max_slippage_bps);
    let mut instructions = vec![
        set_compute_unit_limit(EXIT_COMPUTE_UNIT_LIMIT),
        set_compute_unit_price(EXIT_COMPUTE_UNIT_PRICE),
        route.swap(min_out_lamports),
    ];

    // The tip is priced against what this exit is expected to *make*, which for
    // a sale is the proceeds less what the position cost. It is routinely
    // negative — that is what flattening a losing position is — and an
    // `Emergency` policy is the one that does not mind.
    let expected_out = i64::try_from(fill.net_lamports).unwrap_or(i64::MAX);
    let ev_net_lamports = expected_out.saturating_sub(target.size_lamports);
    let tip = match tip {
        Some(policy) => {
            let bid = policy.bid(&exit_intent_id, Some(ev_net_lamports), attempt)?;
            if bid.lamports >= fill.net_lamports {
                return Err(ExitError::new(
                    ExitFailure::Construction,
                    format!(
                        "a tip of {} lamports against a sale worth {} would hand the position \
                         to a validator instead of closing it",
                        bid.lamports, fill.net_lamports
                    ),
                ));
            }
            // Last, deliberately. The transaction is atomic either way, so the
            // order changes nothing about what can half-happen — but the sale
            // funds the tip, and a transfer placed before the swap has to be
            // covered by whatever the wallet was already holding. On the one
            // path that runs when things have gone wrong, that is the
            // difference between an exit that lands and an exit that fails for
            // want of the lamports the exit itself was about to produce.
            instructions.push(bid.instruction(route.payer));
            Some(bid)
        }
        None => None,
    };

    let message = Message::compile(route.payer, &instructions, route.recent_blockhash)
        .map_err(|err| ExitError::new(ExitFailure::Construction, err.to_string()))?;

    // Simulation is mandatory, and this is where "mandatory" is enforced rather
    // than remembered: the message is checked before there is an `ExitPlan` to
    // carry it, so a plan that exists is a plan that simulated. There is no
    // ordering a caller could get wrong and no second constructor that skips the
    // step. A breach is a `Construction` failure because that is what it is —
    // the bytes this function just produced are not bytes it will hand to a
    // signer — and it is caught here so that the last state from which giving up
    // costs nothing is still the state we are in.
    let simulation = simulate_message(
        &message,
        route.venue(),
        route.tokens,
        min_out_lamports,
        fill.net_lamports,
        tip,
        route,
    )
    .map_err(|breach| {
        ExitError::new(
            ExitFailure::Construction,
            format!("{exit_intent_id} did not simulate atomically: {breach}"),
        )
    })?;

    Ok(ExitPlan {
        exit_intent_id,
        origin_intent_id: target.intent_id.clone(),
        mint: target.mint.clone(),
        venue: route.venue(),
        mode: target.mode,
        tokens: route.tokens,
        expected_out_lamports: fill.net_lamports,
        min_out_lamports,
        slippage_bps: fill.slippage_bps,
        cost_basis_lamports: target.size_lamports,
        tip,
        transaction: Transaction {
            signatures: Vec::new(),
            message,
        },
        simulation,
        constructed_at_ms: now_ms,
    })
}

// ---------------------------------------------------------------------------
// simulation
// ---------------------------------------------------------------------------

/// Why a built exit is not something this process will sign.
///
/// Every variant is a way the atomicity claim in [`build_exit`]'s documentation
/// could stop being true — a claim about the *bytes*, which is why every one of
/// these is decided by reading the compiled message back rather than by reading
/// the struct that produced it. A field and the instruction data that is
/// supposed to encode it are two different things, and the whole point of a
/// simulation is to check the one the network will actually run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtomicityBreach {
    /// The message is empty, or has no fee payer, or the payer does not sign.
    NotSignable(String),
    /// An instruction goes to a program that has no business in an exit. The
    /// roadmap's "no public-mempool fallback" is structural rather than
    /// aspirational only if the transaction cannot reach one, and a program
    /// allowlist is what that means when it is checked instead of asserted.
    ForeignProgram { index: usize, program: Pubkey },
    /// Not exactly one swap. Zero is a transaction that pays fees and sells
    /// nothing; two is a position sold twice, and the second one against a
    /// curve the first one moved.
    NotOneSwap(usize),
    /// The instruction data does not encode what the plan says it does. A floor
    /// that lives in a struct and not in the bytes is not a floor.
    Mismatch {
        field: &'static str,
        planned: u64,
        encoded: u64,
    },
    /// The floor is zero, or above what the route quoted. Either way it is not
    /// a bound the fill can be checked against.
    Unfloored {
        min_out_lamports: u64,
        expected_out_lamports: u64,
    },
    /// The tip is not the last instruction, or it is not after the swap.
    ///
    /// This is the ordering the atomicity argument turns on. The transaction is
    /// all-or-nothing either way, so nothing can *half*-happen — but a transfer
    /// placed ahead of the sale has to be covered by lamports the wallet is
    /// already holding, and on the one path that runs when things have gone
    /// wrong that is the difference between an exit that lands and an exit that
    /// fails for want of the money it was about to make.
    TipOutOfOrder {
        tip_index: usize,
        swap_index: usize,
        instructions: usize,
    },
    /// A tip nobody planned, or a planned tip nobody encoded.
    TipUnaccounted(String),
    /// The tip is not covered by the worst fill the floor allows.
    ///
    /// [`build_exit`] already refuses a tip larger than the *quoted* proceeds.
    /// This is the stricter question, and it is the one that matters: the quote
    /// is what the sale is expected to make and `min_out_lamports` is what it
    /// is allowed to make, so a tip between the two is a tip that is unfunded
    /// in exactly the case the floor exists to describe.
    TipAboveFloor {
        tip_lamports: u64,
        min_out_lamports: u64,
    },
    /// Re-quoting the route the plan names does not reproduce the plan's own
    /// number. The plan was priced against something other than what it says.
    Repriced { planned: u64, requoted: u64 },
    /// The route cannot be quoted at all any more.
    Unquotable(String),
}

impl std::fmt::Display for AtomicityBreach {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AtomicityBreach::NotSignable(why) => {
                write!(f, "the message cannot be signed: {why}")
            }
            AtomicityBreach::ForeignProgram { index, program } => write!(
                f,
                "instruction {index} goes to {program}, which is not a program an exit touches"
            ),
            AtomicityBreach::NotOneSwap(count) => {
                write!(f, "an exit is exactly one swap and this one has {count}")
            }
            AtomicityBreach::Mismatch {
                field,
                planned,
                encoded,
            } => write!(
                f,
                "the plan says {field} is {planned} and the instruction data says {encoded}"
            ),
            AtomicityBreach::Unfloored {
                min_out_lamports,
                expected_out_lamports,
            } => write!(
                f,
                "a floor of {min_out_lamports} against a quote of {expected_out_lamports} is not \
                 a floor"
            ),
            AtomicityBreach::TipOutOfOrder {
                tip_index,
                swap_index,
                instructions,
            } => write!(
                f,
                "the tip is instruction {tip_index} of {instructions}, and the swap that funds it \
                 is {swap_index}"
            ),
            AtomicityBreach::TipUnaccounted(why) => write!(f, "{why}"),
            AtomicityBreach::TipAboveFloor {
                tip_lamports,
                min_out_lamports,
            } => write!(
                f,
                "a tip of {tip_lamports} lamports is not covered by the {min_out_lamports} the \
                 floor guarantees the sale"
            ),
            AtomicityBreach::Repriced { planned, requoted } => write!(
                f,
                "the plan expects {planned} lamports and the route it names quotes {requoted}"
            ),
            AtomicityBreach::Unquotable(why) => write!(f, "the route cannot be quoted: {why}"),
        }
    }
}

impl std::error::Error for AtomicityBreach {}

/// What the simulation read off the transaction, once it agreed to it.
///
/// Returned rather than discarded because "it simulated" is not evidence and
/// these numbers are: an operator reading a receipt wants the floor that is in
/// the bytes, not the one that was in the struct, and the two being the same is
/// exactly what was checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitSimulation {
    /// How many instructions are in the one atomic unit.
    pub instructions: usize,
    /// Where the sale is.
    pub swap_index: usize,
    /// Where the tip is, or `None` on a backend that does not tip.
    pub tip_index: Option<usize>,
    /// The floor, read out of the instruction data.
    pub min_out_lamports: u64,
    /// The position, read out of the instruction data.
    pub tokens: u64,
    /// What re-quoting the route the plan names came back with.
    pub requoted_out_lamports: u64,
    /// §18's network costs for this transaction, tip included.
    pub costs: TransactionCosts,
}

impl ExitSimulation {
    /// What this exit costs to send if it lands.
    pub const fn cost_lamports(&self) -> u64 {
        self.costs.total_lamports()
    }

    /// What it costs if it executes and reverts. The floor doing its job — a
    /// fill under the bound — is one of the ways that happens, which is why the
    /// number is worth having beside the one above.
    pub const fn failed_cost_lamports(&self) -> u64 {
        self.costs.failed_lamports()
    }
}

/// The programs an exit is allowed to touch, per venue.
///
/// A short list on purpose. Anything not on it is a foreign program, and the
/// reason to check rather than trust is that the check is what makes "there is
/// no public-mempool fallback" a property of the transaction instead of a
/// property of the code that happened to build it.
///
/// The two token programs are on it and nothing in this build names them: an
/// exit sells out of an account that already exists, so there is no account to
/// open on the way out. They are here because the list is what a *future*
/// instruction is measured against, and an allowlist that had to be widened
/// before a legitimate change could ship is one somebody widens carelessly. An
/// instruction to either is permitted and is not otherwise read — the checks
/// below are about the swap, the budget and the tip, which are the three that
/// decide what the transaction does with the position.
fn allowed_programs(venue: Venue) -> [Pubkey; 5] {
    let swap = match venue {
        Venue::PumpFunCurve => PUMP_FUN_KEY,
        Venue::RaydiumAmmV4 => RAYDIUM_AMM_V4_KEY,
    };
    [
        COMPUTE_BUDGET_KEY,
        swap,
        SYSTEM_KEY,
        TOKEN_KEY,
        ASSOCIATED_TOKEN_KEY,
    ]
}

/// Reads a little-endian `u64` out of instruction data.
fn le_u64(data: &[u8], at: usize) -> Option<u64> {
    data.get(at..at + 8)?
        .try_into()
        .ok()
        .map(u64::from_le_bytes)
}

/// Simulates one built exit and says whether it is atomic.
///
/// **This is the check the roadmap's Phase 4 criterion 2 asks for**, in the only
/// form this build can honestly give it: there is no node here, so this does not
/// claim the transaction would land. It claims the narrower and more useful
/// thing — that the transaction is *one all-or-nothing unit that either sells
/// the position under a real floor and pays for itself, or does nothing at all*.
/// Everything a partial exit could come from is a way that sentence stops being
/// true, and each one is a variant of [`AtomicityBreach`].
///
/// It re-reads the compiled message rather than the [`ExitPlan`] fields for the
/// same reason `the_sell_discriminator_is_the_hash_it_claims_to_be` recomputes a
/// constant: the struct is what somebody meant and the instruction data is what
/// a validator will execute, and a check that reads the first one cannot catch
/// an encoder that got the second one wrong.
///
/// `route` is the route the plan was built from. Quoting it again is the closest
/// thing to a `simulateTransaction` available without a cluster: it prices the
/// same sale against the same reserves and must come back with the same number
/// the plan carries.
pub fn simulate_exit(
    plan: &ExitPlan,
    route: &ExitRoute,
) -> Result<ExitSimulation, AtomicityBreach> {
    simulate_message(
        &plan.transaction.message,
        plan.venue,
        plan.tokens,
        plan.min_out_lamports,
        plan.expected_out_lamports,
        plan.tip,
        route,
    )
}

/// The simulation, against the numbers a plan carries rather than against the
/// plan itself.
///
/// Exists so that [`build_exit`] can simulate the message it has just compiled
/// *before* there is an `ExitPlan` to put the result on — which is what makes
/// the simulation a field on the plan rather than something a caller is trusted
/// to have run. [`simulate_exit`] is the same call with the numbers read back
/// off a plan that already exists.
fn simulate_message(
    message: &Message,
    venue: Venue,
    planned_tokens: u64,
    planned_min_out_lamports: u64,
    planned_expected_out_lamports: u64,
    planned_tip: Option<TipBid>,
    route: &ExitRoute,
) -> Result<ExitSimulation, AtomicityBreach> {
    if message.instructions.is_empty() {
        return Err(AtomicityBreach::NotSignable(
            "it has no instructions".to_string(),
        ));
    }
    if message.num_required_signatures == 0 {
        return Err(AtomicityBreach::NotSignable(
            "it requires no signatures".to_string(),
        ));
    }
    if message.account_keys.first() != Some(&route.payer) {
        return Err(AtomicityBreach::NotSignable(
            "the fee payer is not the first account".to_string(),
        ));
    }

    let swap_program = match venue {
        Venue::PumpFunCurve => PUMP_FUN_KEY,
        Venue::RaydiumAmmV4 => RAYDIUM_AMM_V4_KEY,
    };
    let allowed = allowed_programs(venue);

    let mut swap: Option<(usize, u64, u64)> = None;
    let mut swaps = 0usize;
    let mut tip: Option<(usize, u64)> = None;
    let mut tips = 0usize;
    let mut compute_unit_limit = 0u32;
    let mut compute_unit_price = 0u64;

    for (index, instruction) in message.instructions.iter().enumerate() {
        let program = match message
            .account_keys
            .get(usize::from(instruction.program_id_index))
        {
            Some(key) => *key,
            None => {
                return Err(AtomicityBreach::NotSignable(format!(
                    "instruction {index} names account {} and there are {}",
                    instruction.program_id_index,
                    message.account_keys.len()
                )))
            }
        };
        if !allowed.contains(&program) {
            return Err(AtomicityBreach::ForeignProgram { index, program });
        }

        if program == swap_program {
            swaps += 1;
            let data = &instruction.data;
            let (tokens, min_out) = match venue {
                // Eight bytes of discriminator, then the parcel, then the floor.
                Venue::PumpFunCurve => (le_u64(data, 8), le_u64(data, 16)),
                // One tag byte, then the same two.
                Venue::RaydiumAmmV4 => (le_u64(data, 1), le_u64(data, 9)),
            };
            match (tokens, min_out) {
                (Some(tokens), Some(min_out)) => swap = Some((index, tokens, min_out)),
                _ => {
                    return Err(AtomicityBreach::NotSignable(format!(
                        "instruction {index} is {} bytes, which is not a swap",
                        data.len()
                    )))
                }
            }
        } else if program == COMPUTE_BUDGET_KEY {
            match instruction.data.first() {
                Some(2) => {
                    compute_unit_limit = instruction
                        .data
                        .get(1..5)
                        .and_then(|bytes| bytes.try_into().ok())
                        .map(u32::from_le_bytes)
                        .unwrap_or(0);
                }
                Some(3) => compute_unit_price = le_u64(&instruction.data, 1).unwrap_or(0),
                _ => {}
            }
        } else if program == SYSTEM_KEY {
            // The only system instruction an exit builds is the tip transfer,
            // and the tag is checked rather than assumed: every other system
            // instruction — `CreateAccount`, `Assign`, `Allocate` — is also
            // four tag bytes followed by numbers, so reading lamports out of
            // one without looking at the tag would report a confident total for
            // an instruction that does something else entirely.
            tips += 1;
            let data = &instruction.data;
            let is_transfer = data.len() == 12 && data[..4] == 2u32.to_le_bytes();
            match (is_transfer, le_u64(data, 4)) {
                (true, Some(lamports)) => tip = Some((index, lamports)),
                _ => {
                    return Err(AtomicityBreach::TipUnaccounted(format!(
                        "instruction {index} is a system instruction that is not a transfer"
                    )))
                }
            }
        }
    }

    if swaps != 1 {
        return Err(AtomicityBreach::NotOneSwap(swaps));
    }
    let (swap_index, tokens, min_out_lamports) =
        swap.expect("a count of exactly one means it was set");

    if tokens != planned_tokens {
        return Err(AtomicityBreach::Mismatch {
            field: "tokens",
            planned: planned_tokens,
            encoded: tokens,
        });
    }
    if min_out_lamports != planned_min_out_lamports {
        return Err(AtomicityBreach::Mismatch {
            field: "the floor",
            planned: planned_min_out_lamports,
            encoded: min_out_lamports,
        });
    }
    if min_out_lamports == 0 || min_out_lamports > planned_expected_out_lamports {
        return Err(AtomicityBreach::Unfloored {
            min_out_lamports,
            expected_out_lamports: planned_expected_out_lamports,
        });
    }

    match (planned_tip, tip) {
        (None, Some((index, lamports))) => {
            return Err(AtomicityBreach::TipUnaccounted(format!(
                "instruction {index} pays {lamports} lamports to somebody the plan does not \
                 mention"
            )))
        }
        (Some(bid), None) => {
            return Err(AtomicityBreach::TipUnaccounted(format!(
                "the plan bid {} lamports and no instruction pays it",
                bid.lamports
            )))
        }
        (Some(bid), Some((index, lamports))) => {
            if tips != 1 {
                return Err(AtomicityBreach::TipUnaccounted(format!(
                    "one bid and {tips} transfers is not a tip anybody can audit"
                )));
            }
            if lamports != bid.lamports {
                return Err(AtomicityBreach::Mismatch {
                    field: "the tip",
                    planned: bid.lamports,
                    encoded: lamports,
                });
            }
            if index != message.instructions.len() - 1 || index < swap_index {
                return Err(AtomicityBreach::TipOutOfOrder {
                    tip_index: index,
                    swap_index,
                    instructions: message.instructions.len(),
                });
            }
            if lamports >= min_out_lamports {
                return Err(AtomicityBreach::TipAboveFloor {
                    tip_lamports: lamports,
                    min_out_lamports,
                });
            }
        }
        (None, None) => {}
    }

    let requoted = route
        .quote()
        .map_err(|err| AtomicityBreach::Unquotable(err.to_string()))?;
    if requoted.net_lamports != planned_expected_out_lamports {
        return Err(AtomicityBreach::Repriced {
            planned: planned_expected_out_lamports,
            requoted: requoted.net_lamports,
        });
    }

    Ok(ExitSimulation {
        instructions: message.instructions.len(),
        swap_index,
        tip_index: tip.map(|(index, _)| index),
        min_out_lamports,
        tokens,
        requoted_out_lamports: requoted.net_lamports,
        costs: TransactionCosts::new(
            u32::from(message.num_required_signatures),
            compute_unit_price,
            compute_unit_limit,
            // An exit sells out of an account that already holds the position.
            // Nothing here opens one.
            0,
            planned_tip.map(|bid| bid.lamports).unwrap_or(0),
        ),
    })
}

// ---------------------------------------------------------------------------
// flattening
// ---------------------------------------------------------------------------

/// What happened to one position.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "camelCase")]
pub enum FlattenOutcome {
    /// Sold, landed, and booked. This is the only variant that means the money
    /// is back.
    ///
    /// The signature, venue and size are `Option` because a `reused` outcome
    /// reads them back out of the file rather than from the exit it just sent.
    /// Every row this module writes fills all three in, so a missing one means
    /// the file disagrees with itself — and the right answer to that is to say
    /// so, not to render a plausible default for something nobody recorded.
    Flattened {
        exit_intent_id: String,
        signature: Option<String>,
        venue: Option<Venue>,
        tokens: Option<u64>,
        out_lamports: i64,
        realized_pnl_lamports: i64,
        /// True when a previous unwind sent this exit and this pass only found
        /// it. The position is just as closed either way; what differs is
        /// whether this call put anything on the network.
        reused: bool,
    },
    /// A transaction is on the network for this position and it has not
    /// confirmed. **Still at risk.**
    InFlight {
        exit_intent_id: String,
        signature: Option<String>,
        venue: Option<Venue>,
        /// Where the exit actually got to. `ExitBroadcast` while something is
        /// still waiting on it; `ExitFailed` when it went out and then failed
        /// to confirm. Both say the same thing about the money — a transaction
        /// is out there and nobody knows what it did — and different things
        /// about whether anything is still trying.
        state: ExitState,
        reused: bool,
    },
    /// The entry never landed, so there is no position and nothing to sell.
    ResolvedToNothing { detail: String },
    /// The entry's outcome is not known yet. Conditional, and not actionable
    /// until it is reconciled — which is a different thing from having failed.
    Unresolved { detail: String },
    /// Nothing was attempted, and why.
    Skipped { detail: String },
    /// An exit was attempted and did not go out, or went out and did not land.
    Failed {
        exit_intent_id: Option<String>,
        failure: ExitFailure,
        detail: String,
        /// True when a transaction was already on the network when this failed.
        /// The position may or may not have been sold and has to be reconciled
        /// against the signature before anything else is done to it.
        left_on_network: bool,
    },
}

impl FlattenOutcome {
    /// Whether this call put a signed transaction on the network for it.
    pub fn dispatched_now(&self) -> bool {
        match self {
            FlattenOutcome::Flattened { reused, .. } | FlattenOutcome::InFlight { reused, .. } => {
                !*reused
            }
            FlattenOutcome::Failed {
                left_on_network, ..
            } => *left_on_network,
            _ => false,
        }
    }

    /// Whether there is still money on chain for this position.
    ///
    /// The question the receipt's stranded list is built from. A confirmed exit
    /// is the only answer that is false — an exit that is merely on the network
    /// has not closed anything yet, and saying otherwise is the specific lie
    /// `UnwindReceipt` exists to prevent.
    pub fn still_at_risk(&self) -> bool {
        !matches!(
            self,
            FlattenOutcome::Flattened { .. } | FlattenOutcome::ResolvedToNothing { .. }
        )
    }
}

/// One position and what became of it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlattenResult {
    pub target: ExitTarget,
    pub outcome: FlattenOutcome,
}

/// Everything one flattening pass did.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlattenReport {
    /// Which backend produced this.
    pub backend: String,
    /// Whether that backend can reach a real network. False for every backend
    /// in this build.
    pub live: bool,
    pub results: Vec<FlattenResult>,
    /// What went wrong on the way that did not stop an exit — a row that could
    /// not be written, a ledger that could not be read.
    pub problems: Vec<String>,
}

impl FlattenReport {
    /// A pass that did nothing, because there was no backend to do it with.
    pub fn nothing_attempted() -> Self {
        FlattenReport {
            backend: "none".to_string(),
            live: false,
            results: Vec::new(),
            problems: Vec::new(),
        }
    }

    /// How many signed exit transactions **this pass** put on the network.
    ///
    /// Not how many positions have an exit out — that includes the ones a
    /// previous unwind sent, and this number is the receipt's answer to "what
    /// did pressing the button just now actually do".
    pub fn exits_sent(&self) -> usize {
        self.results
            .iter()
            .filter(|r| r.outcome.dispatched_now())
            .count()
    }

    /// How many positions are closed and booked, whoever sent the exit.
    pub fn exits_confirmed(&self) -> usize {
        self.results
            .iter()
            .filter(|r| matches!(r.outcome, FlattenOutcome::Flattened { .. }))
            .count()
    }

    /// How many exits failed.
    pub fn exits_failed(&self) -> usize {
        self.results
            .iter()
            .filter(|r| matches!(r.outcome, FlattenOutcome::Failed { .. }))
            .count()
    }

    /// How many positions already had an exit from an earlier pass, which this
    /// one found rather than sent.
    pub fn exits_already_out(&self) -> usize {
        self.results
            .iter()
            .filter(|r| {
                matches!(
                    r.outcome,
                    FlattenOutcome::Flattened { reused: true, .. }
                        | FlattenOutcome::InFlight { reused: true, .. }
                )
            })
            .count()
    }

    /// How many exits are on the network and not yet confirmed.
    pub fn exits_in_flight(&self) -> usize {
        self.results
            .iter()
            .filter(|r| matches!(r.outcome, FlattenOutcome::InFlight { .. }))
            .count()
    }

    /// What the confirmed exits came to, net of what the positions cost.
    ///
    /// Saturating, so a ledger with an implausible number in it produces a
    /// clamped total rather than an overflow panic on a path taken during an
    /// emergency.
    pub fn realized_pnl_lamports(&self) -> i64 {
        self.results
            .iter()
            .fold(0i64, |total, result| match result.outcome {
                FlattenOutcome::Flattened {
                    realized_pnl_lamports,
                    ..
                } => total.saturating_add(realized_pnl_lamports),
                _ => total,
            })
    }
}

/// The seq counter and the fixed fields for one exit's ledger rows.
struct ExitLedger<'p> {
    plan: &'p ExitPlan,
    seq: i64,
}

impl<'p> ExitLedger<'p> {
    fn new(plan: &'p ExitPlan) -> Self {
        ExitLedger { plan, seq: 0 }
    }

    /// The next row, with everything that is the same on every step of one exit
    /// already filled in.
    fn row(&mut self, from: Option<ExitState>, to: ExitState, at_ms: i64) -> IntentTransitionRow {
        let seq = self.seq;
        self.seq = self.seq.saturating_add(1);
        IntentTransitionRow {
            intent_id: self.plan.exit_intent_id.clone(),
            seq,
            origin_intent_id: self.plan.origin_intent_id.clone(),
            from_state: from,
            to_state: to,
            venue: Some(self.plan.venue),
            mint: self.plan.mint.clone(),
            tokens: i64::try_from(self.plan.tokens).ok(),
            min_out_lamports: i64::try_from(self.plan.min_out_lamports).ok(),
            out_lamports: None,
            cost_basis_lamports: self.plan.cost_basis_lamports.max(0),
            realized_pnl_lamports: None,
            signature: None,
            failure: None,
            detail: None,
            mode: self.plan.mode,
            at_ms,
        }
    }
}

/// Drives one unwind's worth of exits.
///
/// Owns no state that outlives the call. It borrows the backend and the
/// database, writes every step as it happens, and hands back a report — so
/// there is nothing here to leak, nothing to keep alive after the receipt is
/// returned, and no second copy of the truth kept in memory alongside the one
/// in `sts.db`.
pub struct Flattener<'a> {
    backend: &'a dyn ExecutionEngine,
    db: &'a Database,
    now_ms: i64,
    /// What the retry backoff waits on. A real clock by default; a test
    /// substitutes one that only moves a number, so the suite proves the
    /// schedule rather than sitting through it.
    waiter: Box<dyn Waiter + 'a>,
    problems: Vec<String>,
    /// Where the state counters go, when anybody is keeping them. `None` in the
    /// tests and in any caller that has not been given a collector, because a
    /// flattening must work identically whether or not it is being measured.
    metrics: Option<&'a MetricsCollector>,
    /// Where anomalies go, when anybody is listening. `None` is silence, not a
    /// different set of thresholds: an unwind must reach the same outcome
    /// whether or not somebody is being paged about it.
    ///
    /// The journal has no equivalent switch, and that asymmetry is deliberate.
    /// An alert is a message to a person and needs somewhere to go; the book is
    /// the record of what was traded and is written whether or not anybody is
    /// watching. `db` is already here, so there is nothing to attach.
    alerts: Option<&'a AlertDispatcher>,
}

impl<'a> Flattener<'a> {
    pub fn new(backend: &'a dyn ExecutionEngine, db: &'a Database, now_ms: i64) -> Self {
        Flattener {
            backend,
            db,
            now_ms,
            waiter: Box::new(SleepingWaiter),
            problems: Vec::new(),
            metrics: None,
            alerts: None,
        }
    }

    /// Runs the retry backoff against a different clock.
    pub fn waiting_with(mut self, waiter: Box<dyn Waiter + 'a>) -> Self {
        self.waiter = waiter;
        self
    }

    /// Counts every step this pass takes into the engine's metrics.
    pub fn with_metrics(mut self, metrics: &'a MetricsCollector) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Holds every fill and every confirmation against this dispatcher's
    /// thresholds.
    ///
    /// Nothing about the unwind changes. `AlertDispatcher::observe` compares
    /// integers and hands whatever fired to listeners that are documented not
    /// to block — a webhook queues and returns, a window's channel is a
    /// `try_send` — so the cost on the path this sits in is some arithmetic and
    /// a read lock per fill.
    pub fn alerting_through(mut self, alerts: &'a AlertDispatcher) -> Self {
        self.alerts = Some(alerts);
        self
    }

    /// Flattens every target it can, in order, and says what became of each.
    ///
    /// Never panics and never gives up early. One position that cannot be sold
    /// does not stop the next one — during an unwind the whole point is to get
    /// out of as much as possible, and a loop that returns on the first error
    /// would leave the rest of the book open because of one bad pool.
    pub fn flatten(mut self, targets: &[ExitTarget]) -> FlattenReport {
        // Read before anything is built. An obligation that already has an exit
        // on the network must not get a second one, and if this read fails the
        // safe answer is to send nothing at all: a duplicate exit sells a
        // position twice, and the second sale is of tokens the wallet does not
        // have.
        let attempts = match self.db.latest_exit_attempts() {
            Ok(attempts) => attempts,
            Err(err) => {
                self.problems.push(format!(
                    "the exit ledger could not be read, so nothing was flattened — \
                     an unwind that cannot see its own previous exits would send them twice: {err}"
                ));
                return FlattenReport {
                    backend: self.backend.name().to_string(),
                    live: self.backend.is_live(),
                    results: targets
                        .iter()
                        .map(|target| FlattenResult {
                            target: target.clone(),
                            outcome: FlattenOutcome::Skipped {
                                detail: "the exit ledger could not be read".to_string(),
                            },
                        })
                        .collect(),
                    problems: self.problems,
                };
            }
        };

        // `latest_exit_attempts` comes back newest first, so the first entry
        // for an origin is the newest exit for it.
        let mut newest_for: HashMap<&str, &ExitAttempt> = HashMap::new();
        let mut exit_intents: HashSet<&str> = HashSet::new();
        let mut attempts_for: HashMap<&str, u32> = HashMap::new();
        for attempt in &attempts {
            newest_for
                .entry(attempt.origin_intent_id.as_str())
                .or_insert(attempt);
            exit_intents.insert(attempt.intent_id.as_str());
            let counted = attempts_for
                .entry(attempt.origin_intent_id.as_str())
                .or_insert(0);
            *counted = counted.saturating_add(1);
        }

        let mut results = Vec::with_capacity(targets.len());
        for target in targets {
            let outcome = self.flatten_one(
                target,
                exit_intents.contains(target.intent_id.as_str()),
                newest_for.get(target.intent_id.as_str()).copied(),
                attempts_for
                    .get(target.intent_id.as_str())
                    .copied()
                    .unwrap_or(0),
            );
            results.push(FlattenResult {
                target: target.clone(),
                outcome,
            });
        }

        FlattenReport {
            backend: self.backend.name().to_string(),
            live: self.backend.is_live(),
            results,
            problems: self.problems,
        }
    }

    fn flatten_one(
        &mut self,
        target: &ExitTarget,
        is_itself_an_exit: bool,
        previous: Option<&ExitAttempt>,
        attempt: u32,
    ) -> FlattenOutcome {
        // An exit that was broadcast and never confirmed becomes an open
        // obligation of its own, which is correct — something is on the network
        // and nobody knows what it did. It must not then be treated as a
        // position to sell, because selling a sell is how one position becomes
        // two transactions and a short.
        if is_itself_an_exit {
            return FlattenOutcome::Skipped {
                detail: format!(
                    "{} is itself an exit that has not settled; it is reconciled against its \
                     signature, not sold again",
                    target.intent_id
                ),
            };
        }

        if let Some(previous) = previous {
            if previous.is_settled() {
                return FlattenOutcome::Flattened {
                    exit_intent_id: previous.intent_id.clone(),
                    signature: previous.signature.clone(),
                    venue: previous.venue,
                    tokens: previous.tokens.and_then(|t| u64::try_from(t).ok()),
                    out_lamports: previous.out_lamports.unwrap_or(0),
                    realized_pnl_lamports: previous.realized_pnl_lamports.unwrap_or(0),
                    reused: true,
                };
            }
            if previous.left_on_network() {
                return FlattenOutcome::InFlight {
                    exit_intent_id: previous.intent_id.clone(),
                    signature: previous.signature.clone(),
                    venue: previous.venue,
                    state: previous.state,
                    reused: true,
                };
            }
            if previous.blocks_retry() {
                // Signed or constructed and then nothing — the process that was
                // doing it went away mid-exit. The signature may still be
                // broadcastable by whoever holds it, so building a second one
                // is not safe until somebody has looked.
                return FlattenOutcome::Skipped {
                    detail: format!(
                        "{} already has an exit at {} that never finished; it is reconciled \
                         before another is built",
                        target.intent_id, previous.state
                    ),
                };
            }
        }

        let route = match self.backend.resolve(target) {
            Ok(Reconciliation::Landed(route)) => *route,
            Ok(Reconciliation::NeverLanded { detail }) => {
                if target.is_actionable() {
                    // The table says the entry confirmed and the backend says
                    // it never landed. One of them is wrong and neither can be
                    // trusted enough to sell against.
                    self.problems.push(format!(
                        "{} is recorded as confirmed but the network says it never landed: \
                         {detail}",
                        target.intent_id
                    ));
                    return FlattenOutcome::Failed {
                        exit_intent_id: None,
                        failure: ExitFailure::NoRoute,
                        detail: format!(
                            "confirmed in the ledger, absent on chain — reconcile before selling: \
                             {detail}"
                        ),
                        left_on_network: false,
                    };
                }
                return FlattenOutcome::ResolvedToNothing { detail };
            }
            Ok(Reconciliation::Unresolved { detail }) => {
                return FlattenOutcome::Unresolved { detail };
            }
            Err(err) => {
                let detail = err.detail.clone();
                self.record_early_failure(target, err.failure, &detail, attempt);
                return FlattenOutcome::Failed {
                    exit_intent_id: None,
                    failure: err.failure,
                    detail,
                    left_on_network: false,
                };
            }
        };

        let simulated_at_ms = route.simulated_at_ms;
        let exit_id = exit_intent_id(&target.intent_id, attempt, self.now_ms);
        let plan = match build_exit(
            target,
            &route,
            self.backend.tip_policy(),
            exit_id,
            attempt,
            self.now_ms,
        ) {
            Ok(plan) => plan,
            Err(err) => {
                let detail = err.detail.clone();
                self.record_early_failure(target, err.failure, &detail, attempt);
                return FlattenOutcome::Failed {
                    exit_intent_id: None,
                    failure: err.failure,
                    detail,
                    left_on_network: false,
                };
            }
        };

        self.run(
            plan,
            ExitContext {
                target,
                attempt,
                simulated_at_ms,
            },
        )
    }

    /// Walks one built exit through the rest of its life, writing every step.
    fn run(&mut self, plan: ExitPlan, context: ExitContext<'_>) -> FlattenOutcome {
        let mut ledger = ExitLedger::new(&plan);
        let at = self.now_ms;

        // The book, opened. Before anything is signed, because every other
        // journal row this exit writes is a child of this one by foreign key,
        // and a child whose parent has not been written is a row SQLite
        // refuses. It is also the earliest moment the trade can be described
        // honestly: the venue is decided, the size is decided, and what is left
        // is what it comes to.
        self.journal_opened(&context, &plan);

        let mut constructed = ledger.row(None, ExitState::ExitConstructed, at);
        // Phase 4 wants simulation to be mandatory, and a step nobody can see
        // afterwards is a step an operator has to take on trust. `build_exit`
        // has already refused anything that did not simulate, so this row is
        // never written for an exit that failed the check — which is exactly
        // why it is worth writing: `exit_constructed` in this ledger means
        // "simulated", and this line is what says so in the file.
        let sim = &plan.simulation;
        constructed.detail = Some(format!(
            "simulated {} instructions, floor {} lamports on {} tokens, {} to send and {} if it              reverts",
            sim.instructions,
            sim.min_out_lamports,
            sim.tokens,
            sim.cost_lamports(),
            sim.failed_cost_lamports()
        ));
        self.write_transition(&constructed);
        // The exit is a new intent in `execution_logs` too, per U2: a resolved
        // obligation is new rows, never an edit to the old ones. It is
        // `validated` on arrival because it went through the exit gate, which
        // `RiskSnapshot::exits_allowed` answers yes to unconditionally — U3.
        self.write_executions(&[
            exit_step(&plan, 0, ExecutionState::IntentCreated, None, None, at),
            exit_step(
                &plan,
                1,
                ExecutionState::Validated,
                Some(ExecutionState::IntentCreated),
                None,
                at,
            ),
        ]);

        let signed = match self.backend.sign(&plan) {
            Ok(signed) => signed,
            Err(err) => {
                // The route was priced and this attempt did not get past
                // signing, so the book records a path that lost. There is no
                // signature row to settle — nothing was signed, so nothing has
                // one — which is the difference between this and the
                // `journal_settled` below.
                self.journal_route(
                    &context,
                    &plan,
                    RouteDecision::Rejected {
                        because: format!("{}: {}", err.failure, err.detail),
                    },
                    at,
                );
                return self.fail(&plan, &mut ledger, ExitState::ExitConstructed, err, 2);
            }
        };

        // Written before the broadcast, deliberately. A process that dies
        // between these two lines has to come back knowing a transaction with
        // this signature may be on the network; the alternative is a
        // reconciliation that decides nothing went out and sells the position a
        // second time.
        let mut row = ledger.row(Some(ExitState::ExitConstructed), ExitState::ExitSigned, at);
        row.signature = Some(signed.signature.to_string());
        // Annex J step 8: the tip is tracked with the bundle. It goes on this
        // row rather than a column of its own because it belongs to exactly one
        // step — the one that has the signature — and because a receipt that
        // says only what was paid, without which account or which attempt,
        // cannot be audited afterwards.
        if let Some(bid) = plan.tip {
            row.detail = Some(format!(
                "tipped {} lamports to {} on attempt {}",
                bid.lamports, bid.account, bid.attempt
            ));
        }
        self.write_transition(&row);

        // Written before the broadcast for the reason the transition above is,
        // and it is the row `journal_in_flight` counts: money whose fate is
        // decided and not yet known. A process that dies between here and the
        // send comes back to a book that says a transaction with this signature
        // may be out there, which is the answer that makes somebody go and look
        // rather than sell the position again.
        self.journal_signature(
            &context,
            SignatureRow::broadcast(
                signed.signature.to_string(),
                context.target.intent_id.clone(),
                SignatureKind::Exit,
                at,
            ),
        );

        // Handed to the loop below rather than sent from here, because a
        // transaction that reaches a node and is never heard of again is the
        // ordinary case rather than the exotic one, and the answer to it is to
        // send the same bytes again — never to build new ones.
        let policy = self.backend.broadcast_policy();
        let backend = self.backend;
        let db = self.db;
        let plan_ref = &plan;
        // Lifted out of `self` for the same reason `db` and `backend` are: the
        // loop below borrows the waiter mutably, so the closure cannot also
        // hold `self`. Without this the rebroadcast path would write its rows
        // straight to the database and count none of them — the one step of an
        // exit that repeats would be the one step the metrics never saw.
        let metrics = self.metrics;
        let mut trouble: Vec<String> = Vec::new();
        let run = {
            let mut record = |step: &BroadcastStep| {
                let mut row = ledger.row(Some(step.from), step.to, step.at_ms);
                row.detail = step.detail.clone();
                // Counted before the write, exactly as `write_transition` does:
                // a broadcast that happened and could not be recorded is still
                // a broadcast.
                if let Some(metrics) = metrics {
                    metrics.record_exit(row.from_state, row.to_state);
                }
                if let Err(err) = db.record_intent_transitions(std::slice::from_ref(&row)) {
                    trouble.push(format!(
                        "the {} step of {} could not be recorded: {err}",
                        row.to_state, row.intent_id
                    ));
                }
                // The exit intent reaches `sent` once, on the first broadcast.
                // A rebroadcast is the same transaction on the same network and
                // does not move the intent's own history — the finer record of
                // how many times it went out is the `intent_transitions` row
                // just written above.
                if step.from == ExitState::ExitSigned {
                    let sent = exit_step(
                        plan_ref,
                        2,
                        ExecutionState::Sent,
                        Some(ExecutionState::Validated),
                        Some(signed.signature.to_string()),
                        step.at_ms,
                    );
                    if let Some(metrics) = metrics {
                        metrics.record_intent(sent.prev_state, sent.state);
                    }
                    if let Err(err) = db.record_execution_logs(std::slice::from_ref(&sent)) {
                        trouble.push(format!(
                            "the execution history for {} could not be recorded: {err}",
                            sent.intent_id
                        ));
                    }
                }
            };
            broadcast_until_settled(
                backend,
                &policy,
                &signed,
                at,
                self.waiter.as_mut(),
                &mut record,
            )
        };
        self.problems.extend(trouble);

        let rebroadcasts = run.rebroadcasts;
        let settlement = Settlement {
            status: run.settled_as,
            rebroadcasts,
            at_ms: run.steps.last().map_or(at, |step| step.at_ms),
        };
        let fill = match run.outcome {
            Ok(fill) => fill,
            Err(err) => {
                let (from, exec_seq) = if run.state.is_dispatched() {
                    (ExitState::ExitBroadcast, 3)
                } else {
                    (ExitState::ExitSigned, 2)
                };
                // The book, before the ledger. Both are written and neither can
                // fail the other — `journal_settled` pushes a problem exactly
                // the way `write_transition` does — but this one carries the
                // reason the route was passed over, and the reason is the error
                // that is about to be moved into the receipt.
                self.journal_settled(&context, &plan, &signed, settlement, &err);
                return self.fail(&plan, &mut ledger, from, err, exec_seq);
            }
        };

        let out_lamports = i64::try_from(fill.out_lamports).unwrap_or(i64::MAX);
        let realized = out_lamports.saturating_sub(plan.cost_basis_lamports);
        let mut row = ledger.row(Some(ExitState::ExitBroadcast), ExitState::ExitConfirmed, at);
        row.out_lamports = Some(out_lamports);
        row.realized_pnl_lamports = Some(realized);
        self.write_transition(&row);

        let mut confirmed = exit_step(
            &plan,
            3,
            ExecutionState::Confirmed,
            Some(ExecutionState::Sent),
            None,
            at,
        );
        confirmed.price_q18 = fill_price(out_lamports, fill.tokens);
        self.write_executions(&[
            confirmed,
            exit_step(
                &plan,
                4,
                ExecutionState::Completed,
                Some(ExecutionState::Confirmed),
                None,
                at,
            ),
        ]);

        self.journal_filled(&context, &plan, &signed, &fill, rebroadcasts);

        FlattenOutcome::Flattened {
            exit_intent_id: plan.exit_intent_id.clone(),
            signature: Some(signed.signature.to_string()),
            venue: Some(plan.venue),
            tokens: Some(plan.tokens),
            out_lamports,
            realized_pnl_lamports: realized,
            reused: false,
        }
    }

    /// Records a failure part way through a built exit, and abandons the exit
    /// intent in `execution_logs` from wherever it had got to.
    fn fail(
        &mut self,
        plan: &ExitPlan,
        ledger: &mut ExitLedger<'_>,
        from: ExitState,
        err: ExitError,
        exec_seq: i64,
    ) -> FlattenOutcome {
        let at = self.now_ms;
        let outcome = match from.fail(err.failure) {
            Ok(outcome) => outcome,
            Err(transition) => {
                // Unreachable while `from` is one of the three active states
                // this is called with, and reported rather than panicked on if
                // that ever stops being true.
                self.problems.push(format!(
                    "{} could not be marked failed from {from}: {transition}",
                    plan.exit_intent_id
                ));
                return FlattenOutcome::Failed {
                    exit_intent_id: Some(plan.exit_intent_id.clone()),
                    failure: err.failure,
                    detail: err.detail,
                    left_on_network: from.is_dispatched(),
                };
            }
        };

        let mut row = ledger.row(Some(from), ExitState::ExitFailed, at);
        row.failure = Some(outcome.reason);
        row.detail = Some(err.detail.clone());
        self.write_transition(&row);

        // The exit intent's own history. It is aborted from `validated` when
        // nothing reached the network and from `sent` when something did, and
        // `needs_unwind` follows from that through `AbortOutcome` rather than
        // being decided here — U1.
        let exec_from = if outcome.left_on_network {
            ExecutionState::Sent
        } else {
            ExecutionState::Validated
        };
        let reason = if outcome.left_on_network {
            AbortReason::NotConfirmed
        } else {
            AbortReason::SendFailed
        };
        match exec_from.abort(reason) {
            Ok(abort) => self.write_executions(&[ExecutionLogRow::aborted(
                plan.exit_intent_id.clone(),
                exec_seq,
                plan.mint.clone(),
                abort,
                Side::Sell,
                plan.cost_basis_lamports.max(1),
                // Never the signature off the `sent` step: the unique partial
                // index means it belongs to one row, and copying it forward
                // would fail the insert and roll the batch back.
                None,
                plan.mode,
                at,
            )]),
            Err(transition) => self.problems.push(format!(
                "{} could not be abandoned from {exec_from}: {transition}",
                plan.exit_intent_id
            )),
        }

        FlattenOutcome::Failed {
            exit_intent_id: Some(plan.exit_intent_id.clone()),
            failure: err.failure,
            detail: err.detail,
            left_on_network: outcome.left_on_network,
        }
    }

    /// Records an exit that failed before it was ever routed or built.
    ///
    /// There is no venue, no size and no floor for something that was never
    /// constructed, so those columns stay null rather than being filled with a
    /// zero somebody would later read as a number.
    fn record_early_failure(
        &mut self,
        target: &ExitTarget,
        failure: ExitFailure,
        detail: &str,
        attempt: u32,
    ) {
        let row = IntentTransitionRow {
            intent_id: exit_intent_id(&target.intent_id, attempt, self.now_ms),
            seq: 0,
            origin_intent_id: target.intent_id.clone(),
            from_state: None,
            to_state: ExitState::ExitFailed,
            venue: None,
            mint: target.mint.clone(),
            tokens: None,
            min_out_lamports: None,
            out_lamports: None,
            cost_basis_lamports: target.size_lamports.max(0),
            realized_pnl_lamports: None,
            signature: None,
            failure: Some(failure),
            detail: Some(detail.to_string()),
            mode: target.mode,
            at_ms: self.now_ms,
        };
        self.write_transition(&row);
    }

    /// Appends one ledger row. A failure here is a problem, never a reason to
    /// stop: `RISK_AND_SYBIL_SPEC.md` §12.4 puts a SQLite write on the list of
    /// things an exit must not depend on.
    fn write_transition(&mut self, row: &IntentTransitionRow) {
        // Counted before the write, not after it. The counter is about what the
        // signer did; the write is about whether the disk kept up. A broadcast
        // that happened and could not be recorded is still a broadcast, and a
        // metric that pretended otherwise would be describing the disk.
        if let Some(metrics) = self.metrics {
            metrics.record_exit(row.from_state, row.to_state);
        }
        if let Err(err) = self.db.record_intent_transitions(std::slice::from_ref(row)) {
            self.problems.push(format!(
                "the {} step of {} could not be recorded: {err}",
                row.to_state, row.intent_id
            ));
        }
    }

    fn write_executions(&mut self, rows: &[ExecutionLogRow]) {
        if let Some(metrics) = self.metrics {
            for row in rows {
                metrics.record_intent(row.prev_state, row.state);
            }
        }
        if let Err(err) = self.db.record_execution_logs(rows) {
            let ids: Vec<&str> = rows.iter().map(|row| row.intent_id.as_str()).collect();
            self.problems.push(format!(
                "the execution history for {} could not be recorded: {err}",
                ids.join(", ")
            ));
        }
    }
}

/// What one signature's time on the network came to.
///
/// The three facts `broadcast_until_settled` establishes and that nothing
/// downstream of it can reconstruct: by the time a caller has an `ExitError`, a
/// blockhash that aged out and a transaction a node took and lost are the same
/// `ExitFailure::NotConfirmed`.
#[derive(Debug, Clone, Copy)]
struct Settlement {
    /// Which of the four ways of ending it was.
    status: SignatureStatus,
    /// How many times the same bytes went out again.
    rebroadcasts: u32,
    /// When it was decided. Off the loop's own clock rather than the wall,
    /// which is what lets a replay reproduce the row.
    at_ms: i64,
}

/// What the journal needs about one exit that the plan does not carry.
///
/// Three facts, and none of them belongs on `ExitPlan`. The plan is one
/// transaction; these are about the position it is flattening — which trade the
/// book calls this, which attempt at getting out of it this is, and when the
/// reserves it was priced against were read.
#[derive(Debug, Clone, Copy)]
struct ExitContext<'t> {
    target: &'t ExitTarget,
    /// The retry index. `journal_routes` and `journal_fills` sequence on it, and
    /// `journal_tips` keys on it, so a second attempt at the same position adds
    /// rows beside the first rather than colliding with them.
    attempt: u32,
    simulated_at_ms: i64,
}

// ---------------------------------------------------------------------------
// the book
// ---------------------------------------------------------------------------

/// Everything this file writes to `journal.rs`'s five tables.
///
/// The whole section obeys one rule, which is the rule `write_transition`
/// already states and `RISK_AND_SYBIL_SPEC.md` §12.4 is the source of: **a
/// journal write that fails is a problem on the receipt and never a reason to
/// stop an exit.** Nothing here returns a `Result`, nothing here uses `?`, and
/// the only thing any of it does with an error is push a sentence somebody will
/// read afterwards. A position that could not be sold because the disk was full
/// is a far worse outcome than a trade that was sold and not written down.
///
/// The second rule is that the trade is keyed by the **origin** intent — the
/// position — and not by the exit. A position exited on the third attempt is
/// one row in `journal_trades` with three signatures hanging off it, which is
/// what a person means by "a trade". Keying on the exit would put three trades
/// in the book for one position, two of which never closed, and the totals
/// underneath would be wrong in a way that reads as real.
impl Flattener<'_> {
    /// Opens the trade, and records the tip that was bid to close it.
    ///
    /// Called once per attempt rather than once per position, and the upsert in
    /// `record_journal_trades` is what makes that safe: the identity columns
    /// assign themselves and pass the table's trigger, while the venue, the
    /// size and the tokens are re-stated from the attempt that is actually
    /// about to go out. A second attempt through a different pool corrects the
    /// venue instead of leaving the book pointing at the pool that did not work.
    fn journal_opened(&mut self, context: &ExitContext<'_>, plan: &ExitPlan) {
        let cost_basis = plan.cost_basis_lamports.max(0) as u64;
        let trade = TradeRow {
            trade_id: context.target.intent_id.clone(),
            mint: plan.mint.clone(),
            side: context.target.side,
            mode: plan.mode,
            venue: Some(plan.venue),
            notional_lamports: cost_basis,
            tokens: plan.tokens,
            cost_basis_lamports: cost_basis,
            proceeds_lamports: None,
            realized_pnl_lamports: None,
            fee_lamports: 0,
            tip_lamports: 0,
            slippage_bps: None,
            opened_at_ms: context.target.opened_at_ms,
            closed_at_ms: None,
        };
        let written = self.db.record_journal_trades(std::slice::from_ref(&trade));
        self.note_journal("the trade", &context.target.intent_id, written);

        self.journal_tip(context, plan);
    }

    /// Records one tip bid, and holds it against the ceiling it was priced
    /// under.
    ///
    /// Written when the bid is made rather than when the transaction lands,
    /// because a bid is a decision and the case worth being able to look at
    /// afterwards is the one where the decision did not work. What was actually
    /// *paid* is a different question and a different column —
    /// `journal_trades.tip_lamports`, which `journal_filled` sets from the
    /// attempt that landed, because a tip rides inside its exit transaction and
    /// a transaction that never landed never paid one.
    fn journal_tip(&mut self, context: &ExitContext<'_>, plan: &ExitPlan) {
        let Some(bid) = plan.tip else { return };
        // The stance and the ceiling come from the policy that priced the bid.
        // Kept on the row rather than read from config later, because the
        // question a month afterwards is whether the bid was inside the ceiling
        // *then*.
        let Some(policy) = self.backend.tip_policy() else {
            return;
        };
        let tip = TipRow::from_bid(
            context.target.intent_id.clone(),
            &bid,
            policy.stance,
            policy.max_lamports,
            self.now_ms,
        );
        let written = self.db.record_journal_tips(std::slice::from_ref(&tip));
        self.note_journal("the tip bid", &context.target.intent_id, written);

        if let Some(alerts) = self.alerts {
            alerts.observe(
                &Observation::Tipped {
                    mint: &plan.mint,
                    mode: plan.mode,
                    tip: &tip,
                },
                self.now_ms,
            );
        }
    }

    /// Writes one signature row, new or moved to its next status.
    fn journal_signature(&mut self, context: &ExitContext<'_>, row: SignatureRow) {
        let written = self
            .db
            .record_journal_signatures(std::slice::from_ref(&row));
        self.note_journal("the signature", &context.target.intent_id, written);
    }

    /// Records an exit that landed: the fill, the route it went through, the
    /// signature that carried it, and the trade closed against the proceeds.
    ///
    /// The order is deliberate and it is the order a foreign key requires: the
    /// trade already exists from `journal_opened`, so the children can go down,
    /// and the trade is re-stated last carrying the numbers the children just
    /// established.
    fn journal_filled(
        &mut self,
        context: &ExitContext<'_>,
        plan: &ExitPlan,
        signed: &SignedExit,
        fill: &ExitFill,
        rebroadcasts: u32,
    ) {
        // `settle` derives the price, the quote and the slippage from the
        // integers rather than taking them, so the three cannot disagree. It
        // answers `None` on a fill of nothing and on a price past what an
        // `i64` at 10^-18 holds — neither of which is a fill anybody can write
        // a row about, and both of which are a problem rather than a silent
        // zero.
        let Some(row) = FillRow::settle(
            context.target.intent_id.clone(),
            context.attempt,
            fill.tokens,
            fill.out_lamports,
            fill.fee_lamports,
            plan.expected_out_lamports,
            fill.slot,
            fill.at_ms,
        ) else {
            self.problems.push(format!(
                "the fill of {} could not be journalled: {} tokens at {} lamports is not a price \
                 a column holds",
                context.target.intent_id, fill.tokens, fill.out_lamports
            ));
            return;
        };

        let written = self.db.record_journal_fills(std::slice::from_ref(&row));
        self.note_journal("the fill", &context.target.intent_id, written);

        // Held against the thresholds before the rest is written. The alert and
        // the row are built from the same `FillRow`, so an alert that says a
        // fill came in 900 bps under its quote and a book that says 400 is a
        // disagreement this shape makes impossible.
        if let Some(alerts) = self.alerts {
            alerts.observe(
                &Observation::Filled {
                    trade_id: &context.target.intent_id,
                    mint: &plan.mint,
                    mode: plan.mode,
                    fill: &row,
                    route_bound_bps: plan.slippage_bps,
                },
                fill.at_ms,
            );
            alerts.observe(
                &Observation::Settled {
                    trade_id: &context.target.intent_id,
                    mint: &plan.mint,
                    mode: plan.mode,
                    status: SignatureStatus::Confirmed,
                    elapsed_ms: fill.at_ms.saturating_sub(self.now_ms).max(0) as u64,
                    rebroadcasts,
                },
                fill.at_ms,
            );
        }

        self.journal_route(context, plan, RouteDecision::Chosen, fill.at_ms);
        self.journal_signature(
            context,
            SignatureRow {
                signature: signed.signature.to_string(),
                trade_id: context.target.intent_id.clone(),
                kind: SignatureKind::Exit,
                status: SignatureStatus::Confirmed,
                slot: Some(fill.slot),
                rebroadcasts,
                at_ms: fill.at_ms,
            },
        );

        // The trade, closed. `closed_at` computes the profit from the proceeds,
        // the basis, the fee and the tip rather than taking it, so the column
        // and the four it comes from cannot drift apart. The fee and the tip
        // are assigned first for exactly that reason — a `closed_at` called
        // before them would compute a profit that ignored both.
        let cost_basis = plan.cost_basis_lamports.max(0) as u64;
        let trade = TradeRow {
            trade_id: context.target.intent_id.clone(),
            mint: plan.mint.clone(),
            side: context.target.side,
            mode: plan.mode,
            venue: Some(plan.venue),
            notional_lamports: cost_basis,
            tokens: plan.tokens,
            cost_basis_lamports: cost_basis,
            proceeds_lamports: None,
            realized_pnl_lamports: None,
            fee_lamports: fill.fee_lamports,
            // What the attempt that landed bid. A tip is the last instruction
            // in the transaction it rides in, so an attempt that never landed
            // never paid its bid — the bids themselves are all in
            // `journal_tips`, one row per attempt.
            tip_lamports: plan.tip.map_or(0, |bid| bid.lamports),
            slippage_bps: Some(row.slippage_bps),
            opened_at_ms: context.target.opened_at_ms,
            closed_at_ms: None,
        }
        .closed_at(fill.out_lamports, fill.at_ms);
        let written = self.db.record_journal_trades(std::slice::from_ref(&trade));
        self.note_journal("the closed trade", &context.target.intent_id, written);
    }

    /// Records an exit whose signature settled without landing.
    ///
    /// The route is written as rejected and the failure is its reason, which is
    /// what `journal_routes.rejected_because` is for: this path was priced, it
    /// was taken, and it did not fill. That leaves `chosen = 1` free for the
    /// attempt that eventually works, which is what the table's unique index
    /// requires and what makes "which liquidity did the money actually go
    /// through" answerable afterwards.
    ///
    /// The trade is deliberately not closed. Nothing came back, the position is
    /// still open, and a `closed_at_ms` with proceeds of zero would say the
    /// sale returned nothing — which is a different and much worse fact than
    /// the sale not having happened.
    fn journal_settled(
        &mut self,
        context: &ExitContext<'_>,
        plan: &ExitPlan,
        signed: &SignedExit,
        settlement: Settlement,
        err: &ExitError,
    ) {
        self.journal_route(
            context,
            plan,
            RouteDecision::Rejected {
                because: format!("{}: {}", err.failure, err.detail),
            },
            settlement.at_ms,
        );
        self.journal_signature(
            context,
            SignatureRow::broadcast(
                signed.signature.to_string(),
                context.target.intent_id.clone(),
                SignatureKind::Exit,
                self.now_ms,
            )
            .settled_as(settlement.status, settlement.at_ms),
        );

        if let Some(alerts) = self.alerts {
            alerts.observe(
                &Observation::Settled {
                    trade_id: &context.target.intent_id,
                    mint: &plan.mint,
                    mode: plan.mode,
                    status: settlement.status,
                    elapsed_ms: settlement.at_ms.saturating_sub(self.now_ms).max(0) as u64,
                    rebroadcasts: settlement.rebroadcasts,
                },
                settlement.at_ms,
            );
        }
    }

    /// One route row, taken or passed over.
    fn journal_route(
        &mut self,
        context: &ExitContext<'_>,
        plan: &ExitPlan,
        decision: RouteDecision,
        at_ms: i64,
    ) {
        let row = RouteRow {
            trade_id: context.target.intent_id.clone(),
            seq: context.attempt,
            venue: plan.venue,
            decision,
            tokens: plan.tokens,
            quoted_out_lamports: plan.expected_out_lamports,
            min_out_lamports: plan.min_out_lamports,
            max_slippage_bps: plan.slippage_bps,
            simulated_at_ms: context.simulated_at_ms,
            at_ms,
        };
        let written = self.db.record_journal_routes(std::slice::from_ref(&row));
        self.note_journal("the route", &context.target.intent_id, written);
    }

    /// Turns a journal write that failed into a sentence on the receipt.
    fn note_journal(&mut self, what: &str, trade_id: &str, written: Result<usize, EngineError>) {
        if let Err(err) = written {
            self.problems.push(format!(
                "{what} for {trade_id} could not be journalled: {err}"
            ));
        }
    }
}

/// One forward step of the exit intent's own history.
///
/// `size_lamports` is the position's cost basis on every row rather than the
/// proceeds on the last one, because the column answers "how much money is this
/// intent about" and one intent should not answer it two different ways
/// depending on which row you read. What came back is in
/// `intent_transitions.out_lamports`, once, on the step that confirmed.
fn exit_step(
    plan: &ExitPlan,
    seq: i64,
    state: ExecutionState,
    prev_state: Option<ExecutionState>,
    signature: Option<String>,
    at_ms: i64,
) -> ExecutionLogRow {
    ExecutionLogRow {
        intent_id: plan.exit_intent_id.clone(),
        seq,
        mint: plan.mint.clone(),
        state,
        prev_state,
        side: Side::Sell,
        size_lamports: plan.cost_basis_lamports.max(1),
        price_q18: None,
        signature,
        latency_ms: None,
        needs_unwind: false,
        mode: plan.mode,
        abort_reason: None,
        at_ms,
    }
}

/// What one token fetched, in lamports at `10^-18`, or `None` where that is not
/// a number anybody computed.
///
/// The column's `CHECK` refuses anything that is not strictly positive, and a
/// zero reaching it would fail the insert and roll back the whole batch —
/// including the rows that say the position was closed. So the three ways there
/// is no price — nothing came back, nothing was sold, and a price so small it
/// floors to zero at `10^-18` — all answer `None`, which is what the column
/// already says on every row before a fill.
fn fill_price(out_lamports: i64, tokens: u64) -> Option<Q18> {
    if out_lamports <= 0 || tokens == 0 {
        return None;
    }
    let price = Q18::ratio_floor(u128::from(out_lamports.unsigned_abs()), u128::from(tokens))?;
    // Refused here rather than at the bind, so a price past what the column
    // holds costs the price and not the rows around it.
    price.to_i64_raw()?;
    (price.raw() > 0).then_some(price)
}

// ---------------------------------------------------------------------------
// the mock signer
// ---------------------------------------------------------------------------

/// How much real SOL the mock assumes was in a curve when a position was
/// opened, if nothing told it otherwise.
///
/// Twenty SOL is a curve well short of the 85 it graduates at: deep enough that
/// an ordinary position quotes cleanly, shallow enough that a large one runs
/// into the executable-liquidity wall the way a real one would.
pub const MOCK_CURVE_REAL_SOL: u64 = 20 * crate::replay::LAMPORTS_PER_SOL;

/// Where a mock exit is told to go wrong.
///
/// One fault per obligation, injected by a test. Real backends fail for reasons
/// nobody chose; these exist so the paths that handle those reasons can be
/// exercised on purpose rather than waited for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockFault {
    /// The route cannot be resolved at all.
    NoRoute,
    /// The entry never landed, so there is nothing to sell.
    NeverLanded,
    /// The entry's outcome is not known yet.
    Unresolved,
    /// The signer refuses.
    Signing,
    /// The transaction never reaches the network.
    Broadcast,
    /// It reaches the network and never confirms: the blockhash goes past its
    /// window with nothing on chain, so the signature can never land.
    NotConfirmed,
    /// The first `n` confirmations come back with no answer and the one after
    /// that lands. An ordinary dropped packet, which is the case the
    /// rebroadcast loop exists for.
    Dropped(u32),
    /// Every confirmation comes back with no answer. The exit runs out of
    /// retries rather than landing or expiring, which is the case the retry
    /// ceiling exists for.
    AlwaysDropped,
}

/// What the mock believes is on chain for one obligation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MockPosition {
    pub route: ExitRoute,
    /// False for an entry that was broadcast and never landed. The
    /// `at_risk_in == Sent` case, which is the one that must be reconciled
    /// rather than sold.
    pub landed: bool,
}

/// One exit the mock has signed, and what it will fill at.
#[derive(Debug, Clone, Copy)]
struct PendingExit {
    fault: Option<MockFault>,
    fill: Fill,
    dispatched: bool,
    /// How many more confirmations answer "not yet" before this one settles.
    drops_left: u32,
}

#[derive(Debug, Default)]
struct MockState {
    positions: HashMap<String, MockPosition>,
    faults: HashMap<String, MockFault>,
    /// Exits that have been signed and have not settled.
    ///
    /// An entry is removed the moment its exit reaches a terminal state, in
    /// either direction, so this holds what is in flight rather than everything
    /// that has ever been sent. A mock whose memory grows with the number of
    /// exits a process has made is a leak with a long fuse, and the process
    /// this one is embedded in is meant to run for days.
    pending: HashMap<String, PendingExit>,
    slot: u64,
}

/// A signer that builds real bytes and sends them nowhere.
///
/// It is a full participant in the lifecycle — it resolves routes, produces a
/// signature over the actual serialized message, accepts a broadcast and
/// confirms a fill quoted off the same curve model the replay simulator uses —
/// and `is_live` is `false`, which is the only thing that keeps the promotion
/// gate honest. Nothing in `run()` installs one, so the shipped application has
/// no backend at all.
///
/// **The signature is not ed25519.** It is a digest of the message bytes and
/// the exit's intent id. A real node would reject it, which is the correct
/// behaviour for something that must never be mistaken for a live signer, and
/// the intent id is in it so two identical positions on one mint cannot produce
/// one signature — the unique index on `signature` would take the first and
/// roll the second's whole batch back.
///
/// Every field is behind one lock and the counters are atomics, so a test may
/// drive it from several threads and the engine may reach it from the unwind
/// command and a maintenance pass at once.
pub struct MockSolanaSigner {
    signer: Pubkey,
    blockhash: [u8; 32],
    tip: Option<TipPolicy>,
    broadcast_policy: BroadcastPolicy,
    state: Mutex<MockState>,
    signed: AtomicU64,
    broadcast: AtomicU64,
    confirmed: AtomicU64,
    failed: AtomicU64,
}

impl Default for MockSolanaSigner {
    fn default() -> Self {
        Self::new()
    }
}

impl MockSolanaSigner {
    /// A signer with a derived key, a fixed blockhash, the default exit tip
    /// policy and the default retry schedule.
    pub fn new() -> Self {
        MockSolanaSigner {
            signer: mock_key("signer", "sts"),
            blockhash: *mock_key("blockhash", "sts").as_bytes(),
            tip: Some(TipPolicy::emergency()),
            broadcast_policy: BroadcastPolicy::default(),
            state: Mutex::new(MockState::default()),
            signed: AtomicU64::new(0),
            broadcast: AtomicU64::new(0),
            confirmed: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        }
    }

    /// Bids with a different policy, or — with `None` — stops tipping at all,
    /// which is what a backend talking to a plain RPC node does.
    pub fn tipping(mut self, tip: Option<TipPolicy>) -> Self {
        self.tip = tip;
        self
    }

    /// Pushes a dropped broadcast on a different schedule.
    pub fn retrying(mut self, policy: BroadcastPolicy) -> Self {
        self.broadcast_policy = policy;
        self
    }

    /// The wallet the exits are signed by.
    pub fn signer(&self) -> Pubkey {
        self.signer
    }

    /// Declares what is on chain for one obligation.
    pub fn hold(&self, origin_intent_id: &str, position: MockPosition) {
        self.state
            .lock()
            .positions
            .insert(origin_intent_id.to_string(), position);
    }

    /// Makes one obligation's exit fail in a chosen way.
    pub fn inject(&self, origin_intent_id: &str, fault: MockFault) {
        self.state
            .lock()
            .faults
            .insert(origin_intent_id.to_string(), fault);
    }

    /// How many exits this backend is still carrying: signed and not yet
    /// settled. Bounded by what is in flight, never by what has been sent.
    pub fn in_flight(&self) -> usize {
        self.state.lock().pending.len()
    }

    /// How many exits have been signed, broadcast, confirmed, and failed.
    pub fn counters(&self) -> (u64, u64, u64, u64) {
        (
            self.signed.load(Ordering::Relaxed),
            self.broadcast.load(Ordering::Relaxed),
            self.confirmed.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
        )
    }

    /// A pump.fun route for a position of `size_lamports`, derived from the
    /// mint.
    ///
    /// The story it tells is a coherent one: the curve held
    /// `MOCK_CURVE_REAL_SOL` when the position was opened, the position is
    /// whatever that size bought at the time, and the exit sells it back into
    /// the curve as it stands after that buy. A round trip therefore comes back
    /// slightly down — fees and the engine's own price impact — which is what a
    /// round trip actually does, and a mock that returned the entry price would
    /// quietly make every test of realized PnL pass for the wrong reason.
    pub fn pump_fun_route(&self, mint: &str, size_lamports: i64) -> Result<ExitRoute, ExitError> {
        let size = u64::try_from(size_lamports).map_err(|_| {
            ExitError::new(
                ExitFailure::Construction,
                format!("{size_lamports} lamports is not a size"),
            )
        })?;
        let opened = CurveState::at_real_sol(MOCK_CURVE_REAL_SOL);
        let entry = opened.quote_buy(size, DEFAULT_FEE_BPS).map_err(|err| {
            ExitError::no_route(format!(
                "{mint} could not have been entered at this size: {err}"
            ))
        })?;

        Ok(ExitRoute {
            kind: ExitRouteKind::PumpFunCurve {
                accounts: PumpFunSellAccounts {
                    global: mock_key("global", "pump"),
                    fee_recipient: mock_key("fee_recipient", "pump"),
                    mint: mint_key(mint),
                    bonding_curve: mock_key("bonding_curve", mint),
                    associated_bonding_curve: mock_key("associated_bonding_curve", mint),
                    associated_user: mock_key("associated_user", mint),
                    user: self.signer,
                    creator_vault: mock_key("creator_vault", mint),
                    event_authority: mock_key("event_authority", "pump"),
                },
                curve: opened.after_buy(&entry),
            },
            tokens: entry.tokens,
            payer: self.signer,
            recent_blockhash: self.blockhash,
            max_slippage_bps: EMERGENCY_MAX_SLIPPAGE_BPS,
            simulated_at_ms: 0,
        })
    }

    /// A Raydium route for a graduated token, derived the same way.
    pub fn raydium_route(&self, mint: &str, tokens: u64, pool: RaydiumPool) -> ExitRoute {
        ExitRoute {
            kind: ExitRouteKind::RaydiumAmmV4 {
                accounts: RaydiumSwapAccounts {
                    amm: mock_key("amm", mint),
                    amm_authority: mock_key("amm_authority", "raydium"),
                    amm_open_orders: mock_key("amm_open_orders", mint),
                    amm_target_orders: mock_key("amm_target_orders", mint),
                    pool_coin_token_account: mock_key("pool_coin", mint),
                    pool_pc_token_account: mock_key("pool_pc", mint),
                    serum_program: mock_key("serum_program", "raydium"),
                    serum_market: mock_key("serum_market", mint),
                    serum_bids: mock_key("serum_bids", mint),
                    serum_asks: mock_key("serum_asks", mint),
                    serum_event_queue: mock_key("serum_event_queue", mint),
                    serum_coin_vault: mock_key("serum_coin_vault", mint),
                    serum_pc_vault: mock_key("serum_pc_vault", mint),
                    serum_vault_signer: mock_key("serum_vault_signer", mint),
                    user_source_token_account: mock_key("user_source", mint),
                    user_destination_token_account: mock_key("user_destination", mint),
                    user_owner: self.signer,
                },
                pool,
            },
            tokens,
            payer: self.signer,
            recent_blockhash: self.blockhash,
            max_slippage_bps: EMERGENCY_MAX_SLIPPAGE_BPS,
            simulated_at_ms: 0,
        }
    }
}

impl ExecutionEngine for MockSolanaSigner {
    fn name(&self) -> &'static str {
        "mock-solana-signer"
    }

    fn is_live(&self) -> bool {
        // The whole point. Changing this to `true` is a promotion decision, not
        // an implementation detail.
        false
    }

    fn resolve(&self, target: &ExitTarget) -> Result<Reconciliation, ExitError> {
        let (fault, declared) = {
            let state = self.state.lock();
            (
                state.faults.get(&target.intent_id).copied(),
                state.positions.get(&target.intent_id).copied(),
            )
        };

        match fault {
            Some(MockFault::NoRoute) => {
                self.failed.fetch_add(1, Ordering::Relaxed);
                return Err(ExitError::no_route(format!(
                    "{} has no executable route: the pool is depleted",
                    target.mint
                )));
            }
            Some(MockFault::NeverLanded) => {
                return Ok(Reconciliation::NeverLanded {
                    detail: format!(
                        "{} never landed and its blockhash has expired",
                        target.signature.as_deref().unwrap_or("the entry")
                    ),
                })
            }
            Some(MockFault::Unresolved) => {
                return Ok(Reconciliation::Unresolved {
                    detail: format!(
                        "{} is still in the blockhash window; it has neither landed nor expired",
                        target.signature.as_deref().unwrap_or("the entry")
                    ),
                })
            }
            _ => {}
        }

        let position = match declared {
            Some(position) => position,
            None => MockPosition {
                route: self.pump_fun_route(&target.mint, target.size_lamports)?,
                landed: true,
            },
        };
        if !position.landed {
            return Ok(Reconciliation::NeverLanded {
                detail: format!(
                    "{} never landed and its blockhash has expired",
                    target.signature.as_deref().unwrap_or("the entry")
                ),
            });
        }
        Ok(Reconciliation::Landed(Box::new(position.route)))
    }

    fn sign(&self, plan: &ExitPlan) -> Result<SignedExit, ExitError> {
        let fault = self
            .state
            .lock()
            .faults
            .get(&plan.origin_intent_id)
            .copied();
        if fault == Some(MockFault::Signing) {
            self.failed.fetch_add(1, Ordering::Relaxed);
            return Err(ExitError::new(
                ExitFailure::Signing,
                format!("the signer refused {}", plan.exit_intent_id),
            ));
        }

        // Over the real serialized message, plus the intent id so two identical
        // positions cannot collide on one signature.
        let mut material = plan.message_bytes();
        material.extend_from_slice(plan.exit_intent_id.as_bytes());
        let signature = Signature::new(mock_signature_bytes(&material));

        let fill = Fill {
            gross_lamports: plan.expected_out_lamports,
            fee_lamports: 0,
            net_lamports: plan.expected_out_lamports,
            tokens: plan.tokens,
            slippage_bps: plan.slippage_bps,
        };
        let drops_left = match fault {
            Some(MockFault::Dropped(times)) => times,
            Some(MockFault::AlwaysDropped) => u32::MAX,
            _ => 0,
        };
        self.state.lock().pending.insert(
            plan.exit_intent_id.clone(),
            PendingExit {
                fault,
                fill,
                dispatched: false,
                drops_left,
            },
        );
        self.signed.fetch_add(1, Ordering::Relaxed);

        Ok(SignedExit {
            exit_intent_id: plan.exit_intent_id.clone(),
            signature,
            transaction: Transaction {
                signatures: vec![signature],
                message: plan.transaction.message.clone(),
            },
        })
    }

    /// Accepts the same exit more than once, because that is what a node does.
    /// A repeat send is counted and advances the slot; it does not create a
    /// second exit, and there is nothing here that could turn one into two.
    fn broadcast(&self, signed: &SignedExit) -> Result<(), ExitError> {
        let mut state = self.state.lock();
        let Some(pending) = state.pending.get_mut(&signed.exit_intent_id) else {
            drop(state);
            self.failed.fetch_add(1, Ordering::Relaxed);
            return Err(ExitError::new(
                ExitFailure::Broadcast,
                format!("{} was never signed by this backend", signed.exit_intent_id),
            ));
        };
        if pending.fault == Some(MockFault::Broadcast) {
            // Terminal: nothing reached the network and nothing will.
            state.pending.remove(&signed.exit_intent_id);
            drop(state);
            self.failed.fetch_add(1, Ordering::Relaxed);
            return Err(ExitError::new(
                ExitFailure::Broadcast,
                format!("no node accepted {}", signed.signature),
            ));
        }
        pending.dispatched = true;
        state.slot = state.slot.saturating_add(1);
        drop(state);
        self.broadcast.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Looked at rather than taken: an exit that has not settled is still in
    /// flight, and a mock that forgot it on the first "not yet" could not be
    /// asked a second time.
    ///
    /// An exit the caller eventually gives up on stays in `pending`, and that
    /// is not a leak — it is the honest answer. The bytes really are still on
    /// the network after a retry loop stops pushing them, and a real node would
    /// say the same thing. What is bounded is what settles: every exit that
    /// lands, expires, or was never dispatched is removed here.
    fn confirm(&self, signed: &SignedExit) -> Result<ConfirmOutcome, ExitError> {
        let mut state = self.state.lock();
        let Some(pending) = state.pending.get_mut(&signed.exit_intent_id) else {
            drop(state);
            self.failed.fetch_add(1, Ordering::Relaxed);
            return Err(ExitError::new(
                ExitFailure::NotConfirmed,
                format!(
                    "{} was never broadcast by this backend",
                    signed.exit_intent_id
                ),
            ));
        };

        // Signed and never sent. Not "not yet" — there is nothing out there to
        // wait for, and answering `Dropped` would have the loop push bytes no
        // node was ever given.
        if !pending.dispatched {
            state.pending.remove(&signed.exit_intent_id);
            drop(state);
            self.failed.fetch_add(1, Ordering::Relaxed);
            return Err(ExitError::new(
                ExitFailure::NotConfirmed,
                format!("{} never landed", signed.signature),
            ));
        }

        if pending.fault == Some(MockFault::NotConfirmed) {
            state.pending.remove(&signed.exit_intent_id);
            drop(state);
            self.failed.fetch_add(1, Ordering::Relaxed);
            return Ok(ConfirmOutcome::Expired {
                detail: format!(
                    "{} never landed and its blockhash is past its window",
                    signed.signature
                ),
            });
        }

        if pending.drops_left > 0 {
            pending.drops_left = pending.drops_left.saturating_sub(1);
            drop(state);
            return Ok(ConfirmOutcome::Dropped {
                detail: format!("{} has not been seen in a block yet", signed.signature),
            });
        }

        let settled = state.pending.remove(&signed.exit_intent_id);
        let slot = state.slot;
        drop(state);
        // Unreachable: the entry was there a line ago and the lock was never
        // released. Answered rather than unwrapped, because this runs on the
        // path that closes a position.
        let Some(pending) = settled else {
            self.failed.fetch_add(1, Ordering::Relaxed);
            return Err(ExitError::new(
                ExitFailure::NotConfirmed,
                format!("{} settled twice at once", signed.exit_intent_id),
            ));
        };

        self.confirmed.fetch_add(1, Ordering::Relaxed);
        Ok(ConfirmOutcome::Landed(ExitFill {
            out_lamports: pending.fill.net_lamports,
            tokens: pending.fill.tokens,
            fee_lamports: pending.fill.fee_lamports,
            slippage_bps: pending.fill.slippage_bps,
            slot,
            at_ms: 0,
        }))
    }

    fn tip_policy(&self) -> Option<&TipPolicy> {
        self.tip.as_ref()
    }

    fn broadcast_policy(&self) -> BroadcastPolicy {
        self.broadcast_policy
    }
}

/// A deterministic key from a label and a seed. Stable across runs, distinct
/// per label, and obviously not a real account to anybody reading one.
fn mock_key(label: &str, seed: &str) -> Pubkey {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(&digest16(format!("{label}/{seed}").as_bytes(), 0));
    bytes[16..].copy_from_slice(&digest16(format!("{seed}/{label}").as_bytes(), 1));
    Pubkey::new(bytes)
}

/// The mint as a key.
///
/// Real mints are base58 and parse. Fixtures are full of mints that are not —
/// `Mintsent-only` and the like — and a mock that refused those would be
/// untestable against exactly the histories the unwind path has to handle, so
/// anything unparseable is derived instead.
fn mint_key(mint: &str) -> Pubkey {
    Pubkey::parse(mint).unwrap_or_else(|_| mock_key("mint", mint))
}

/// Sixty-four bytes over the message. Not a signature; a receipt shaped like
/// one.
fn mock_signature_bytes(material: &[u8]) -> [u8; 64] {
    let mut out = [0u8; 64];
    for lane in 0..4u32 {
        let start = lane as usize * 16;
        out[start..start + 16].copy_from_slice(&digest16(material, lane));
    }
    out
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::{
        priority_fee_lamports, sha256_hex, unhex, BASE_SIGNATURE_FEE_LAMPORTS, LAMPORTS_PER_SOL,
    };

    fn key(byte: u8) -> Pubkey {
        Pubkey::new([byte; 32])
    }

    fn pump_accounts(user: Pubkey) -> PumpFunSellAccounts {
        PumpFunSellAccounts {
            global: key(1),
            fee_recipient: key(2),
            mint: key(3),
            bonding_curve: key(4),
            associated_bonding_curve: key(5),
            associated_user: key(6),
            user,
            creator_vault: key(7),
            event_authority: key(8),
        }
    }

    fn a_target() -> ExitTarget {
        ExitTarget {
            intent_id: "01912d3f-7a10-7c00-8000-000000000001".to_string(),
            mint: "MintOne".to_string(),
            side: Side::Buy,
            size_lamports: 250_000_000,
            signature: Some("SigEntry".to_string()),
            at_risk_in: ExecutionState::Confirmed,
            mode: ExecutionMode::Paper,
            opened_at_ms: 1_699_999_000_000,
        }
    }

    /// A clock that never sleeps.
    ///
    /// It writes down what it was asked to wait for and moves a number instead,
    /// so the assertions below are about the schedule the policy produced
    /// rather than about how long the suite sat still.
    struct FakeClock {
        now_ms: i64,
        waits: Vec<u64>,
    }

    impl FakeClock {
        fn new(now_ms: i64) -> Self {
            FakeClock {
                now_ms,
                waits: Vec::new(),
            }
        }
    }

    impl Waiter for FakeClock {
        fn wait(&mut self, ms: u64) -> i64 {
            self.waits.push(ms);
            self.now_ms = self.now_ms.saturating_add(ms as i64);
            self.now_ms
        }

        fn now_ms(&self) -> i64 {
            self.now_ms
        }
    }

    /// A backend that remembers every set of bytes it was handed, wrapped round
    /// another one.
    ///
    /// The one thing the mock cannot show on its own is whether a rebroadcast
    /// sent the *same* bytes. Re-signing would be the dangerous version of this
    /// loop — a second signature is a second chance to sell the position — and
    /// from the outside it would look exactly like the safe one.
    struct RecordingSigner {
        inner: MockSolanaSigner,
        sent: Mutex<Vec<Vec<u8>>>,
        schedule: Option<Box<dyn LeaderSchedule>>,
    }

    impl RecordingSigner {
        fn new(inner: MockSolanaSigner) -> Self {
            RecordingSigner {
                inner,
                sent: Mutex::new(Vec::new()),
                schedule: None,
            }
        }

        /// Gives it something to ask about the coming slots. No backend outside
        /// this module has one.
        fn following(mut self, schedule: impl LeaderSchedule + 'static) -> Self {
            self.schedule = Some(Box::new(schedule));
            self
        }

        fn sent(&self) -> Vec<Vec<u8>> {
            self.sent.lock().clone()
        }
    }

    /// A leader schedule that answers off a list and repeats the last answer
    /// once the list runs out, counting how often it was asked.
    ///
    /// The counting is half the point: the seam is only worth having if the
    /// send path actually asks, and a schedule nobody consults would pass every
    /// assertion about waits by doing nothing.
    struct StubSchedule {
        answers: Vec<LeaderHint>,
        asked: std::sync::Arc<AtomicU64>,
    }

    impl StubSchedule {
        fn saying(answers: &[LeaderHint]) -> Self {
            assert!(
                !answers.is_empty(),
                "a schedule with no answers cannot answer"
            );
            StubSchedule {
                answers: answers.to_vec(),
                asked: std::sync::Arc::new(AtomicU64::new(0)),
            }
        }

        /// A handle on the counter, taken before the schedule is boxed into a
        /// backend and cannot be reached again.
        fn counter(&self) -> std::sync::Arc<AtomicU64> {
            std::sync::Arc::clone(&self.asked)
        }
    }

    impl LeaderSchedule for StubSchedule {
        fn hint(&self, _at_ms: i64) -> LeaderHint {
            let nth = self.asked.fetch_add(1, Ordering::Relaxed) as usize;
            self.answers[nth.min(self.answers.len() - 1)]
        }
    }

    impl ExecutionEngine for RecordingSigner {
        fn name(&self) -> &'static str {
            "recording-signer"
        }

        fn is_live(&self) -> bool {
            false
        }

        fn resolve(&self, target: &ExitTarget) -> Result<Reconciliation, ExitError> {
            self.inner.resolve(target)
        }

        fn sign(&self, plan: &ExitPlan) -> Result<SignedExit, ExitError> {
            self.inner.sign(plan)
        }

        fn broadcast(&self, signed: &SignedExit) -> Result<(), ExitError> {
            self.sent.lock().push(signed.wire());
            self.inner.broadcast(signed)
        }

        fn confirm(&self, signed: &SignedExit) -> Result<ConfirmOutcome, ExitError> {
            self.inner.confirm(signed)
        }

        fn tip_policy(&self) -> Option<&TipPolicy> {
            self.inner.tip_policy()
        }

        fn leader_schedule(&self) -> Option<&dyn LeaderSchedule> {
            self.schedule.as_deref()
        }

        fn broadcast_policy(&self) -> BroadcastPolicy {
            self.inner.broadcast_policy()
        }
    }

    /// One exit, resolved through the backend and signed by it.
    fn a_signed_exit(backend: &dyn ExecutionEngine, target: &ExitTarget) -> (ExitPlan, SignedExit) {
        let route = match backend.resolve(target).expect("resolves") {
            Reconciliation::Landed(route) => *route,
            other => panic!("expected a route, got {other:?}"),
        };
        let plan = build_exit(
            target,
            &route,
            backend.tip_policy(),
            "exit-1".to_string(),
            0,
            0,
        )
        .expect("builds");
        let signed = backend.sign(&plan).expect("signs");
        (plan, signed)
    }

    /// A database file of its own, removed when the test finishes with it.
    struct TempDb(std::path::PathBuf);

    impl TempDb {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let temp = TempDb(std::env::temp_dir().join(format!(
                "sts-execution-{name}-{}-{}.db",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )));
            temp.remove();
            temp
        }

        fn open(&self) -> Database {
            Database::open(&self.0).expect("opens")
        }

        fn remove(&self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
            }
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            self.remove();
        }
    }

    // -- the constants -------------------------------------------------------

    #[test]
    fn the_sell_discriminator_is_the_hash_it_claims_to_be() {
        let digest = unhex(&sha256_hex(b"global:sell")).expect("a digest is sixty-four hex digits");
        assert_eq!(
            digest[..8],
            PUMP_FUN_SELL_DISCRIMINATOR,
            "the constant is the first eight bytes of sha256(\"global:sell\") and nothing else"
        );
        // And it is not the buy, which is the mistake that would sell nothing
        // and spend everything.
        let buy = unhex(&sha256_hex(b"global:buy")).expect("digest");
        assert_ne!(digest[..8], buy[..8]);
    }

    #[test]
    fn program_keys_match_their_text() {
        for (text, expected) in [
            (COMPUTE_BUDGET_PROGRAM, COMPUTE_BUDGET_KEY),
            (SYSTEM_PROGRAM, SYSTEM_KEY),
            (TOKEN_PROGRAM, TOKEN_KEY),
            (ASSOCIATED_TOKEN_PROGRAM, ASSOCIATED_TOKEN_KEY),
            (PUMP_FUN_PROGRAM, PUMP_FUN_KEY),
            (RAYDIUM_AMM_V4_PROGRAM, RAYDIUM_AMM_V4_KEY),
        ] {
            let parsed = Pubkey::parse(text).expect("the text is base58");
            assert_eq!(
                parsed, expected,
                "{text} does not decode to the bytes beside it"
            );
            assert_eq!(expected.to_string(), text, "and it renders back to itself");
        }
    }

    // -- the wire format -----------------------------------------------------

    #[test]
    fn compact_lengths_round_trip_across_the_byte_boundaries() {
        for len in [0usize, 1, 126, 127, 128, 255, 256, 16_383, 16_384] {
            let mut out = Vec::new();
            write_compact_len(&mut out, len);
            let (read, used) = read_compact_len(&out).expect("reads back");
            assert_eq!(read, len);
            assert_eq!(used, out.len(), "no trailing bytes for {len}");
        }
        // One byte under 128, two under 16 384. The encoding is what makes a
        // serialized message the same length a node expects.
        let mut out = Vec::new();
        write_compact_len(&mut out, 127);
        assert_eq!(out.len(), 1);
        out.clear();
        write_compact_len(&mut out, 128);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn a_message_orders_its_accounts_the_way_the_protocol_requires() {
        let payer = key(100);
        let instructions = vec![Instruction {
            program_id: key(200),
            accounts: vec![
                AccountMeta::readonly(key(10)),
                AccountMeta::writable(key(11)),
                AccountMeta::signer(key(12)),
                AccountMeta::readonly(key(13)),
            ],
            data: vec![1, 2, 3],
        }];

        let message = Message::compile(payer, &instructions, [7u8; 32]).expect("compiles");

        assert_eq!(
            message.account_keys[0], payer,
            "the fee payer is always first"
        );
        assert_eq!(
            message.num_required_signatures, 2,
            "the payer and the one signing account"
        );
        assert_eq!(message.num_readonly_signed, 0, "both signers are writable");
        assert_eq!(
            message.num_readonly_unsigned, 3,
            "two readonly accounts and the program"
        );

        // Writable signers, readonly signers, writable non-signers, readonly
        // non-signers — in that order and no other.
        let index = |k: Pubkey| {
            message
                .account_keys
                .iter()
                .position(|c| *c == k)
                .expect("present")
        };
        assert!(index(payer) < index(key(12)) || index(key(12)) > 0);
        assert!(
            index(key(12)) < index(key(11)),
            "signers come before non-signers"
        );
        assert!(index(key(11)) < index(key(10)), "writable before readonly");
        assert!(index(key(13)) > index(key(11)));
        assert!(
            index(key(200)) >= index(key(10)),
            "a program is readonly and unsigned"
        );
    }

    #[test]
    fn an_account_named_twice_gets_the_stricter_of_its_permissions() {
        let payer = key(100);
        let instructions = vec![
            Instruction {
                program_id: key(200),
                accounts: vec![AccountMeta::readonly(key(10))],
                data: Vec::new(),
            },
            Instruction {
                program_id: key(200),
                accounts: vec![AccountMeta::writable(key(10))],
                data: Vec::new(),
            },
        ];

        let message = Message::compile(payer, &instructions, [0u8; 32]).expect("compiles");
        assert_eq!(
            message
                .account_keys
                .iter()
                .filter(|k| **k == key(10))
                .count(),
            1,
            "one entry per account, not one per mention"
        );
        // Writable, so it sorts above the program and below the payer.
        assert_eq!(message.account_keys[1], key(10));
        assert_eq!(
            message.num_readonly_unsigned, 1,
            "only the program is readonly"
        );
    }

    #[test]
    fn a_message_serialises_to_the_layout_a_node_reads() {
        let payer = key(100);
        let instructions = vec![Instruction {
            program_id: key(200),
            accounts: vec![AccountMeta::writable(key(11))],
            data: vec![9, 8, 7],
        }];
        let message = Message::compile(payer, &instructions, [3u8; 32]).expect("compiles");
        let bytes = message.serialize();

        assert_eq!(bytes[0], message.num_required_signatures);
        assert_eq!(bytes[1], message.num_readonly_signed);
        assert_eq!(bytes[2], message.num_readonly_unsigned);

        let (count, used) = read_compact_len(&bytes[3..]).expect("a key count");
        assert_eq!(count, message.account_keys.len());
        let mut at = 3 + used;
        for expected in &message.account_keys {
            assert_eq!(&bytes[at..at + 32], expected.as_bytes());
            at += 32;
        }
        assert_eq!(&bytes[at..at + 32], &[3u8; 32], "then the blockhash");
        at += 32;

        let (instruction_count, used) = read_compact_len(&bytes[at..]).expect("an ix count");
        assert_eq!(instruction_count, 1);
        at += used;
        assert_eq!(bytes[at], message.instructions[0].program_id_index);
        at += 1;
        let (accounts, used) = read_compact_len(&bytes[at..]).expect("an account count");
        assert_eq!(accounts, 1);
        at += used + accounts;
        let (data_len, used) = read_compact_len(&bytes[at..]).expect("a data length");
        assert_eq!(data_len, 3);
        at += used;
        assert_eq!(&bytes[at..at + 3], &[9, 8, 7]);
        assert_eq!(at + 3, bytes.len(), "and nothing after it");
    }

    #[test]
    fn a_transaction_is_short_of_signatures_until_it_is_signed() {
        let payer = key(100);
        let instructions = vec![Instruction {
            program_id: key(200),
            accounts: vec![AccountMeta::signer(key(12))],
            data: Vec::new(),
        }];
        let message = Message::compile(payer, &instructions, [0u8; 32]).expect("compiles");
        let unsigned = Transaction {
            signatures: Vec::new(),
            message: message.clone(),
        };
        assert!(!unsigned.is_fully_signed());

        let one = Transaction {
            signatures: vec![Signature::new([1u8; 64])],
            message,
        };
        assert!(
            !one.is_fully_signed(),
            "two accounts have to sign this one, so one signature is not enough"
        );
    }

    #[test]
    fn compiling_nothing_is_refused_rather_than_producing_an_empty_transaction() {
        assert_eq!(
            Message::compile(key(1), &[], [0u8; 32]),
            Err(CompileError::Empty)
        );
    }

    // -- the venue layouts ---------------------------------------------------

    #[test]
    fn the_pump_fun_sell_layout_is_pinned() {
        let accounts = pump_accounts(key(50));
        let metas = accounts.metas();
        assert_eq!(metas.len(), PumpFunSellAccounts::COUNT);

        // Exactly one signer, and it is the seller.
        let signers: Vec<Pubkey> = metas
            .iter()
            .filter(|m| m.is_signer)
            .map(|m| m.pubkey)
            .collect();
        assert_eq!(signers, vec![key(50)]);

        // The order, as roles. A change to this list is a change to the ABI and
        // has to be a deliberate one.
        let expected = [
            (accounts.global, false, false),
            (accounts.fee_recipient, false, true),
            (accounts.mint, false, false),
            (accounts.bonding_curve, false, true),
            (accounts.associated_bonding_curve, false, true),
            (accounts.associated_user, false, true),
            (accounts.user, true, true),
            (SYSTEM_KEY, false, false),
            (accounts.creator_vault, false, true),
            (TOKEN_KEY, false, false),
            (accounts.event_authority, false, false),
            (PUMP_FUN_KEY, false, false),
        ];
        for (i, (pubkey, is_signer, is_writable)) in expected.into_iter().enumerate() {
            assert_eq!(metas[i].pubkey, pubkey, "account {i}");
            assert_eq!(metas[i].is_signer, is_signer, "account {i} signer flag");
            assert_eq!(
                metas[i].is_writable, is_writable,
                "account {i} writable flag"
            );
        }
    }

    #[test]
    fn the_pump_fun_sell_carries_the_amount_and_the_floor() {
        let instruction = pump_accounts(key(50)).sell(123_456_789, 42_000);
        assert_eq!(instruction.program_id, PUMP_FUN_KEY);
        assert_eq!(instruction.data.len(), 24);
        assert_eq!(&instruction.data[..8], &PUMP_FUN_SELL_DISCRIMINATOR);
        assert_eq!(
            u64::from_le_bytes(instruction.data[8..16].try_into().expect("eight bytes")),
            123_456_789
        );
        assert_eq!(
            u64::from_le_bytes(instruction.data[16..24].try_into().expect("eight bytes")),
            42_000,
            "the floor is in the instruction, which is what makes it binding"
        );
    }

    #[test]
    fn the_raydium_swap_carries_its_tag_the_amount_and_the_floor() {
        let accounts = RaydiumSwapAccounts {
            amm: key(20),
            amm_authority: key(21),
            amm_open_orders: key(22),
            amm_target_orders: key(23),
            pool_coin_token_account: key(24),
            pool_pc_token_account: key(25),
            serum_program: key(26),
            serum_market: key(27),
            serum_bids: key(28),
            serum_asks: key(29),
            serum_event_queue: key(30),
            serum_coin_vault: key(31),
            serum_pc_vault: key(32),
            serum_vault_signer: key(33),
            user_source_token_account: key(34),
            user_destination_token_account: key(35),
            user_owner: key(36),
        };
        let metas = accounts.metas();
        assert_eq!(metas.len(), RaydiumSwapAccounts::COUNT);
        assert_eq!(
            metas[0].pubkey, TOKEN_KEY,
            "the token program leads the list"
        );
        let signers: Vec<Pubkey> = metas
            .iter()
            .filter(|m| m.is_signer)
            .map(|m| m.pubkey)
            .collect();
        assert_eq!(signers, vec![key(36)], "only the owner signs");

        let instruction = accounts.swap_base_in(7_000, 6_500);
        assert_eq!(instruction.program_id, RAYDIUM_AMM_V4_KEY);
        assert_eq!(instruction.data[0], RAYDIUM_SWAP_BASE_IN);
        assert_eq!(
            u64::from_le_bytes(instruction.data[1..9].try_into().expect("eight bytes")),
            7_000
        );
        assert_eq!(
            u64::from_le_bytes(instruction.data[9..17].try_into().expect("eight bytes")),
            6_500
        );
    }

    #[test]
    fn a_raydium_quote_takes_the_fee_off_what_goes_in() {
        let pool = RaydiumPool {
            base_reserve: 1_000_000_000_000,
            quote_reserve: 100 * LAMPORTS_PER_SOL,
        };
        let fill = pool
            .quote_sell(1_000_000_000, RAYDIUM_FEE_BPS)
            .expect("quotes");
        assert!(fill.net_lamports > 0);
        assert!(
            fill.net_lamports < fill.gross_lamports,
            "the fee is the difference and it is never zero on a real size"
        );
        assert_eq!(fill.tokens, 1_000_000_000);

        // A pool that cannot be priced against says so rather than quoting zero.
        let empty = RaydiumPool {
            base_reserve: 0,
            quote_reserve: 0,
        };
        assert_eq!(
            empty.quote_sell(10, RAYDIUM_FEE_BPS),
            Err(QuoteError::Implausible)
        );
        assert_eq!(
            pool.quote_sell(0, RAYDIUM_FEE_BPS),
            Err(QuoteError::ZeroSize)
        );
    }

    // -- building an exit ----------------------------------------------------

    #[test]
    fn an_exit_floor_is_never_zero_while_the_quote_is_not() {
        assert_eq!(
            exit_floor_lamports(0, 100),
            0,
            "nothing quoted, nothing to floor"
        );
        assert_eq!(
            exit_floor_lamports(10_000, 0),
            10_000,
            "no slippage, no give"
        );
        assert_eq!(exit_floor_lamports(10_000, 2_500), 7_500);
        assert_eq!(
            exit_floor_lamports(1, 9_999),
            1,
            "a floor that rounds to zero is not a floor, so it is one lamport"
        );
        assert_eq!(
            exit_floor_lamports(1_000, 20_000),
            1,
            "a slippage bound past 100% is clamped rather than wrapping"
        );
    }

    #[test]
    fn an_exit_is_one_atomic_transaction_with_a_budget_and_a_floor() {
        let signer = MockSolanaSigner::new();
        let target = a_target();
        let route = signer
            .pump_fun_route(&target.mint, target.size_lamports)
            .expect("routes");
        let plan = build_exit(
            &target,
            &route,
            None,
            "exit-1".to_string(),
            0,
            1_700_000_000_000,
        )
        .expect("builds");

        assert_eq!(plan.venue, Venue::PumpFunCurve);
        assert_eq!(
            plan.transaction.message.instructions.len(),
            3,
            "compute limit, compute price, and the sell — one transaction, all or none"
        );
        assert!(plan.expected_out_lamports > 0);
        assert!(
            plan.min_out_lamports <= plan.expected_out_lamports,
            "the floor is under the quote"
        );
        assert!(plan.min_out_lamports > 0, "and it is a real floor");
        assert_eq!(plan.cost_basis_lamports, target.size_lamports);
        assert!(
            !plan.transaction.is_fully_signed(),
            "a constructed exit carries no signature yet"
        );

        // The floor in the instruction is the floor on the plan, not a second
        // number that could disagree with it.
        let sell = plan
            .transaction
            .message
            .instructions
            .last()
            .expect("the sell");
        assert_eq!(
            u64::from_le_bytes(sell.data[16..24].try_into().expect("eight bytes")),
            plan.min_out_lamports
        );
    }

    #[test]
    fn a_position_with_no_size_is_refused_rather_than_sold() {
        let signer = MockSolanaSigner::new();
        let mut target = a_target();
        let route = signer
            .pump_fun_route(&target.mint, 250_000_000)
            .expect("routes");
        target.size_lamports = 0;
        let refused =
            build_exit(&target, &route, None, "exit-1".to_string(), 0, 0).expect_err("refused");
        assert_eq!(refused.failure, ExitFailure::Construction);
    }

    #[test]
    fn a_curve_that_cannot_pay_out_is_no_route_rather_than_a_bad_fill() {
        let signer = MockSolanaSigner::new();
        let target = a_target();
        // A curve holding almost nothing, against a position far larger than it.
        let curve = CurveState::at_real_sol(LAMPORTS_PER_SOL / 100);
        let mut route = signer
            .pump_fun_route(&target.mint, target.size_lamports)
            .expect("routes");
        route.kind = match route.kind {
            ExitRouteKind::PumpFunCurve { accounts, .. } => {
                ExitRouteKind::PumpFunCurve { accounts, curve }
            }
            other => other,
        };

        let refused =
            build_exit(&target, &route, None, "exit-1".to_string(), 0, 0).expect_err("refused");
        assert_eq!(
            refused.failure,
            ExitFailure::NoRoute,
            "a pool that cannot pay out is an alarm, not a discount: {refused}"
        );
    }

    #[test]
    fn a_graduated_curve_has_no_exit_on_it() {
        let signer = MockSolanaSigner::new();
        let target = a_target();
        let mut route = signer
            .pump_fun_route(&target.mint, target.size_lamports)
            .expect("routes");
        route.kind = match route.kind {
            ExitRouteKind::PumpFunCurve {
                accounts,
                mut curve,
            } => {
                curve.complete = true;
                ExitRouteKind::PumpFunCurve { accounts, curve }
            }
            other => other,
        };
        let refused =
            build_exit(&target, &route, None, "exit-1".to_string(), 0, 0).expect_err("refused");
        assert_eq!(refused.failure, ExitFailure::NoRoute);
    }

    // -- the exit intent id --------------------------------------------------

    #[test]
    fn an_exit_id_is_a_uuidv7_that_repeats_for_the_same_obligation() {
        let at = 1_700_000_000_000i64;
        let first = exit_intent_id("origin-a", 0, at);
        let again = exit_intent_id("origin-a", 0, at);
        assert_eq!(
            first, again,
            "the same unwind twice is the same exit, not two"
        );

        assert_eq!(first.len(), 36);
        let parts: Vec<&str> = first.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12]
        );
        assert!(first.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
        assert_eq!(parts[2].as_bytes()[0], b'7', "version 7");
        assert!(
            matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'),
            "the RFC 4122 variant: {first}"
        );

        // The timestamp is the leading forty-eight bits, so ids sort by time.
        let later = exit_intent_id("origin-a", 0, at + 1_000);
        assert!(later > first, "{later} should sort after {first}");

        // And a different obligation, or a second attempt at the same one, is a
        // different exit.
        assert_ne!(first, exit_intent_id("origin-b", 0, at));
        assert_ne!(first, exit_intent_id("origin-a", 1, at));
    }

    // -- the mock ------------------------------------------------------------

    #[test]
    fn the_mock_never_claims_to_be_live() {
        let signer = MockSolanaSigner::new();
        assert!(
            !signer.is_live(),
            "the promotion gate is this flag; a mock that lies about it is the whole failure"
        );
        assert_eq!(signer.name(), "mock-solana-signer");
    }

    #[test]
    fn the_mock_walks_an_exit_from_route_to_fill() {
        let signer = MockSolanaSigner::new();
        let target = a_target();

        let route = match signer.resolve(&target).expect("resolves") {
            Reconciliation::Landed(route) => *route,
            other => panic!("expected a route, got {other:?}"),
        };
        let plan = build_exit(&target, &route, None, "exit-1".to_string(), 0, 0).expect("builds");
        let signed = signer.sign(&plan).expect("signs");
        assert!(signed.transaction.is_fully_signed());
        assert!(!signed.wire().is_empty());

        signer.broadcast(&signed).expect("broadcasts");
        let fill = signer
            .confirm(&signed)
            .expect("confirms")
            .landed()
            .expect("and it landed the first time it was asked");
        assert_eq!(fill.out_lamports, plan.expected_out_lamports);
        assert_eq!(fill.tokens, plan.tokens);

        assert_eq!(signer.counters(), (1, 1, 1, 0));
    }

    #[test]
    fn a_round_trip_through_the_mock_comes_back_down_by_the_cost_of_trading() {
        let signer = MockSolanaSigner::new();
        let target = a_target();
        let route = match signer.resolve(&target).expect("resolves") {
            Reconciliation::Landed(route) => *route,
            other => panic!("expected a route, got {other:?}"),
        };
        let plan = build_exit(&target, &route, None, "exit-1".to_string(), 0, 0).expect("builds");
        assert!(
            plan.expected_out_lamports < target.size_lamports as u64,
            "buying and selling straight back costs two fees and two lots of impact; \
             a mock that returned the entry price would make every PnL test pass for the \
             wrong reason"
        );
    }

    #[test]
    fn the_mock_signs_two_identical_positions_to_two_signatures() {
        let signer = MockSolanaSigner::new();
        let target = a_target();
        let route = signer
            .pump_fun_route(&target.mint, target.size_lamports)
            .expect("routes");
        let first = build_exit(&target, &route, None, "exit-1".to_string(), 0, 0).expect("builds");
        let second = build_exit(&target, &route, None, "exit-2".to_string(), 0, 0).expect("builds");

        assert_eq!(
            first.transaction.message, second.transaction.message,
            "the two messages really are identical"
        );
        assert_ne!(
            signer.sign(&first).expect("signs").signature,
            signer.sign(&second).expect("signs").signature,
            "and the signatures are not, because the unique index would take the first \
             and roll the second's whole batch back"
        );
    }

    #[test]
    fn an_injected_fault_stops_the_exit_at_the_step_it_names() {
        for (fault, expected) in [
            (MockFault::NoRoute, ExitFailure::NoRoute),
            (MockFault::Signing, ExitFailure::Signing),
            (MockFault::Broadcast, ExitFailure::Broadcast),
            (MockFault::NotConfirmed, ExitFailure::NotConfirmed),
        ] {
            let signer = MockSolanaSigner::new();
            let target = a_target();
            signer.inject(&target.intent_id, fault);

            let route = match signer.resolve(&target) {
                Ok(Reconciliation::Landed(route)) => *route,
                Err(err) => {
                    assert_eq!(err.failure, expected, "{fault:?}");
                    continue;
                }
                other => panic!("{fault:?} produced {other:?}"),
            };
            let plan =
                build_exit(&target, &route, None, "exit-1".to_string(), 0, 0).expect("builds");
            let signed = match signer.sign(&plan) {
                Ok(signed) => signed,
                Err(err) => {
                    assert_eq!(err.failure, expected, "{fault:?}");
                    continue;
                }
            };
            if let Err(err) = signer.broadcast(&signed) {
                assert_eq!(err.failure, expected, "{fault:?}");
                continue;
            }
            // A transaction that reached the network and never landed is an
            // answer rather than an error — `Expired` is what the mock says and
            // `NotConfirmed` is what the loop above turns it into.
            match signer
                .confirm(&signed)
                .expect("the mock can always be asked")
            {
                ConfirmOutcome::Expired { .. } => {
                    assert_eq!(expected, ExitFailure::NotConfirmed, "{fault:?}")
                }
                other => panic!("{fault:?} produced {other:?}"),
            }
        }
    }

    #[test]
    fn an_entry_that_never_landed_reconciles_to_nothing_rather_than_a_sale() {
        let signer = MockSolanaSigner::new();
        let mut target = a_target();
        target.at_risk_in = ExecutionState::Sent;
        signer.inject(&target.intent_id, MockFault::NeverLanded);

        assert!(!target.is_actionable(), "a sent obligation is conditional");
        match signer.resolve(&target).expect("resolves") {
            Reconciliation::NeverLanded { detail } => assert!(detail.contains("SigEntry")),
            other => panic!("expected nothing on chain, got {other:?}"),
        }
    }

    #[test]
    fn an_entry_still_in_its_blockhash_window_is_unresolved_and_not_guessed_at() {
        let signer = MockSolanaSigner::new();
        let mut target = a_target();
        target.at_risk_in = ExecutionState::Sent;
        signer.inject(&target.intent_id, MockFault::Unresolved);

        assert!(matches!(
            signer.resolve(&target).expect("resolves"),
            Reconciliation::Unresolved { .. }
        ));
    }

    #[test]
    fn confirming_something_that_was_never_broadcast_fails_rather_than_inventing_a_fill() {
        let signer = MockSolanaSigner::new();
        let target = a_target();
        let route = signer
            .pump_fun_route(&target.mint, target.size_lamports)
            .expect("routes");
        let plan = build_exit(&target, &route, None, "exit-1".to_string(), 0, 0).expect("builds");
        let signed = signer.sign(&plan).expect("signs");

        let err = signer.confirm(&signed).expect_err("nothing was sent");
        assert_eq!(err.failure, ExitFailure::NotConfirmed);
        assert_eq!(
            signer.in_flight(),
            0,
            "and the mock stopped carrying an exit that can never settle"
        );
    }

    // -- the tip layer -------------------------------------------------------

    #[test]
    fn jito_tip_accounts_match_their_text() {
        assert_eq!(JITO_TIP_ACCOUNTS.len(), JITO_TIP_KEYS.len());
        for (text, expected) in JITO_TIP_ACCOUNTS.iter().zip(JITO_TIP_KEYS.iter()) {
            let parsed = Pubkey::parse(text).expect("the text is base58");
            assert_eq!(
                &parsed, expected,
                "{text} does not decode to the bytes beside it"
            );
            assert_eq!(expected.to_string(), *text, "and it renders back to itself");
        }
        // Eight *different* accounts. A list with a repeat in it would spread
        // less well than it claims to, and would do it silently.
        let unique: HashSet<Pubkey> = JITO_TIP_KEYS.iter().copied().collect();
        assert_eq!(
            unique.len(),
            JITO_TIP_KEYS.len(),
            "the list names an account twice"
        );
    }

    #[test]
    fn a_transfer_is_four_tag_bytes_and_eight_of_lamports() {
        let from = key(11);
        let to = key(12);
        let transfer = system_transfer(from, to, 12_345);

        assert_eq!(transfer.program_id, SYSTEM_KEY);
        assert_eq!(transfer.data.len(), 12);
        assert_eq!(
            &transfer.data[..4],
            &2u32.to_le_bytes(),
            "the system program's instruction enum is bincoded, so its tag is four bytes and \
             not the single byte the compute budget program beside it uses"
        );
        assert_eq!(
            u64::from_le_bytes(transfer.data[4..].try_into().expect("eight bytes")),
            12_345
        );

        assert_eq!(transfer.accounts.len(), 2);
        assert_eq!(transfer.accounts[0].pubkey, from);
        assert!(
            transfer.accounts[0].is_signer,
            "the payer signs for its own lamports"
        );
        assert!(transfer.accounts[0].is_writable);
        assert_eq!(transfer.accounts[1].pubkey, to);
        assert!(
            !transfer.accounts[1].is_signer,
            "and the tip account signs for nothing"
        );
        assert!(transfer.accounts[1].is_writable);
    }

    #[test]
    fn a_tip_is_the_floor_plus_a_share_of_the_edge_under_the_ceiling() {
        let policy = TipPolicy::emergency()
            .bounded(10_000, 1_000_000)
            .escalating(0, 1_500);

        // Fifteen per cent of a two-million-lamport edge, on top of the floor.
        let bid = policy.bid("exit-1", Some(2_000_000), 0).expect("bids");
        assert_eq!(bid.lamports, 310_000);
        assert_eq!(bid.attempt, 0);
        assert_eq!(
            bid.ev_net_lamports,
            Some(2_000_000),
            "and it records what it was a share of"
        );

        // No edge, or none anybody computed: the floor and nothing added.
        for ev in [Some(-5_000), Some(0), None] {
            assert_eq!(
                policy
                    .bid("exit-1", ev, 0)
                    .expect("an exit bids anyway")
                    .lamports,
                10_000,
                "a share of {ev:?} is not a number"
            );
        }

        // And the ceiling is a ceiling.
        assert_eq!(
            policy
                .bid("exit-1", Some(1_000_000_000), 0)
                .expect("bids")
                .lamports,
            1_000_000,
            "a share of a huge edge still stops at Tip_max"
        );
    }

    #[test]
    fn an_emergency_tip_escalates_with_the_retry_and_stops_at_the_ceiling() {
        let policy = TipPolicy::emergency()
            .bounded(10_000, 100_000)
            .escalating(25_000, 0);

        assert_eq!(
            policy.bid("exit-1", Some(-1), 0).expect("bids").lamports,
            10_000
        );
        assert_eq!(
            policy.bid("exit-1", Some(-1), 1).expect("bids").lamports,
            35_000
        );

        // Monotonic, always, and bounded. An escalation that ever went down
        // would be a retry bidding less than the try that already lost.
        let mut last = 0;
        for attempt in 0..16u32 {
            let bid = policy
                .bid("exit-1", Some(-1), attempt)
                .expect("an exit bids at a loss");
            assert!(
                bid.lamports >= last,
                "attempt {attempt} bid less than the one before it"
            );
            assert!(
                bid.lamports <= 100_000,
                "attempt {attempt} went over the ceiling"
            );
            assert_eq!(bid.attempt, attempt);
            last = bid.lamports;
        }
        assert_eq!(last, 100_000, "and it is pinned to the ceiling by the end");
    }

    #[test]
    fn a_discretionary_tip_will_not_take_the_whole_edge() {
        let policy = TipPolicy::discretionary()
            .bounded(10_000, 1_000_000)
            .escalating(0, 1_500);

        let refused = policy.bid("exit-1", Some(9_000), 0).expect_err("blocked");
        assert_eq!(refused.failure, ExitFailure::Construction);
        assert!(
            refused.detail.contains("hand the whole trade"),
            "{}",
            refused.detail
        );

        // A missing, stale or negative expectation is the same answer: there is
        // nothing to take a discretionary share of.
        for ev in [None, Some(0), Some(-1)] {
            let refused = policy.bid("exit-1", ev, 0).expect_err("blocked");
            assert_eq!(refused.failure, ExitFailure::Construction, "{ev:?}");
        }

        // The same numbers get a position closed, because an emergency exit has
        // no edge to protect — it has a loss to stop, and refusing to bid would
        // leave the position on chain.
        let emergency = TipPolicy::emergency()
            .bounded(10_000, 1_000_000)
            .escalating(0, 1_500);
        assert_eq!(
            emergency
                .bid("exit-1", Some(-500_000), 0)
                .expect("an exit bids at a loss")
                .lamports,
            10_000
        );
    }

    #[test]
    fn a_malformed_policy_blocks_the_bid_rather_than_inventing_a_number() {
        for (policy, expected) in [
            (
                TipPolicy::emergency().into_accounts(Vec::new()),
                "nowhere to pay",
            ),
            (TipPolicy::emergency().bounded(10_000, 0), "no ceiling"),
            (
                TipPolicy::emergency().bounded(50_000, 10_000),
                "below the floor",
            ),
            (TipPolicy::emergency().bounded(1, 999), "under the"),
        ] {
            let refused = policy
                .bid("exit-1", Some(1_000_000), 0)
                .expect_err("blocked");
            assert_eq!(refused.failure, ExitFailure::Construction);
            assert!(refused.detail.contains(expected), "{}", refused.detail);
            // And it refuses to choose an account, rather than choosing one and
            // then failing on the number.
            assert!(policy.account_for("exit-1").is_err());
        }
    }

    #[test]
    fn the_same_exit_tips_the_same_account_and_different_ones_spread() {
        let policy = TipPolicy::emergency();
        let first = policy.account_for("exit-1").expect("chooses");
        for _ in 0..8 {
            assert_eq!(
                policy.account_for("exit-1").expect("chooses"),
                first,
                "a rebroadcast of one exit must not reach for a second write lock"
            );
        }

        // Exit ids are UUIDv7 and ones minted in the same millisecond share a
        // long prefix, so taking bytes off the front would put a burst of exits
        // on one account. The digest is what stops that.
        let ids: Vec<String> = (0..64u32)
            .map(|n| format!("01912d3f-7a10-7c00-8000-0000000000{n:02x}"))
            .collect();
        let chosen: HashSet<Pubkey> = ids
            .iter()
            .map(|id| policy.account_for(id).expect("chooses"))
            .collect();
        assert_eq!(
            chosen.len(),
            JITO_TIP_KEYS.len(),
            "sixty-four exits from one millisecond used {} of the eight accounts",
            chosen.len()
        );
    }

    #[test]
    fn a_fixed_selection_names_one_account_and_a_bad_index_is_not_a_panic() {
        for (index, key) in JITO_TIP_KEYS.iter().enumerate() {
            let policy = TipPolicy::emergency().selecting(TipAccountSelection::Fixed(index));
            assert_eq!(policy.account_for("exit-1").expect("chooses"), *key);
        }
        // An index past the end wraps. An emergency exit is the worst possible
        // place to find out about a typo in a config file.
        let policy = TipPolicy::emergency().selecting(TipAccountSelection::Fixed(usize::MAX));
        assert_eq!(
            policy.account_for("exit-1").expect("chooses"),
            JITO_TIP_KEYS[usize::MAX % JITO_TIP_KEYS.len()]
        );
    }

    #[test]
    fn round_robin_walks_the_list_and_wraps_and_a_copy_starts_again() {
        let policy = TipPolicy::emergency().selecting(TipAccountSelection::RoundRobin);
        let walked: Vec<Pubkey> = (0..16)
            .map(|_| policy.account_for("exit-1").expect("chooses"))
            .collect();
        assert_eq!(&walked[..8], &JITO_TIP_KEYS[..]);
        assert_eq!(&walked[8..], &JITO_TIP_KEYS[..], "and then it wraps");

        assert_eq!(
            policy.clone().account_for("exit-1").expect("chooses"),
            JITO_TIP_KEYS[0],
            "a copy starts at the beginning rather than sharing a position with the original"
        );
    }

    // -- building a tipped exit ----------------------------------------------

    #[test]
    fn a_tipped_exit_pays_last_so_the_sale_funds_the_tip() {
        let signer = MockSolanaSigner::new();
        let target = a_target();
        let route = signer
            .pump_fun_route(&target.mint, target.size_lamports)
            .expect("routes");
        let policy = TipPolicy::emergency().selecting(TipAccountSelection::Fixed(3));
        let plan =
            build_exit(&target, &route, Some(&policy), "exit-1".to_string(), 0, 0).expect("builds");

        let bid = plan.tip.expect("a tipped exit records what it bid");
        assert_eq!(bid.account, JITO_TIP_KEYS[3]);
        assert_eq!(
            bid.lamports, EXIT_TIP_BASE_LAMPORTS,
            "a round trip is a loss, so there is no edge to take a share of"
        );

        let message = &plan.transaction.message;
        assert_eq!(
            message.instructions.len(),
            4,
            "budget, price, swap, then the tip"
        );
        let swap = &message.instructions[2];
        assert_eq!(
            message.account_keys[swap.program_id_index as usize], PUMP_FUN_KEY,
            "the sale comes before the tip, so the proceeds are there to pay it"
        );

        let tip = message.instructions.last().expect("four instructions");
        assert_eq!(
            message.account_keys[tip.program_id_index as usize],
            SYSTEM_KEY
        );
        assert_eq!(message.account_keys[tip.accounts[0] as usize], route.payer);
        assert_eq!(
            message.account_keys[tip.accounts[1] as usize],
            JITO_TIP_KEYS[3]
        );
        assert_eq!(
            u64::from_le_bytes(tip.data[4..].try_into().expect("eight bytes")),
            bid.lamports
        );
    }

    #[test]
    fn an_untipped_exit_is_the_same_transaction_without_the_transfer() {
        let signer = MockSolanaSigner::new();
        let target = a_target();
        let route = signer
            .pump_fun_route(&target.mint, target.size_lamports)
            .expect("routes");
        let policy = TipPolicy::emergency();

        let plain = build_exit(&target, &route, None, "exit-1".to_string(), 0, 0).expect("builds");
        let tipped =
            build_exit(&target, &route, Some(&policy), "exit-1".to_string(), 0, 0).expect("builds");

        assert!(
            plain.tip.is_none(),
            "a backend with nobody to tip does not tip"
        );
        assert_eq!(plain.transaction.message.instructions.len(), 3);
        assert_eq!(tipped.transaction.message.instructions.len(), 4);
        // The same instruction data, but *not* the same compiled indexes: the
        // tip account is a new writable key and every account after it in the
        // table shifts by one. Comparing indexes across two transactions would
        // be comparing two different tables.
        for (index, (left, right)) in plain
            .transaction
            .message
            .instructions
            .iter()
            .zip(tipped.transaction.message.instructions.iter())
            .enumerate()
        {
            assert_eq!(
                left.data, right.data,
                "instruction {index} is encoded differently"
            );
            assert_eq!(
                plain.transaction.message.account_keys[left.program_id_index as usize],
                tipped.transaction.message.account_keys[right.program_id_index as usize],
                "instruction {index} went to a different program"
            );
        }
        assert_eq!(
            (plain.expected_out_lamports, plain.min_out_lamports),
            (tipped.expected_out_lamports, tipped.min_out_lamports),
            "and the tip does not move the quote or the floor: it is paid out of the wallet, \
             not taken off the sale"
        );
    }

    #[test]
    fn a_tip_worth_more_than_the_sale_is_refused_rather_than_built() {
        let signer = MockSolanaSigner::new();
        let target = a_target();
        let route = signer
            .pump_fun_route(&target.mint, target.size_lamports)
            .expect("routes");
        let greedy = TipPolicy::emergency().bounded(u64::MAX / 2, u64::MAX);

        let refused = build_exit(&target, &route, Some(&greedy), "exit-1".to_string(), 0, 0)
            .expect_err("refused");
        assert_eq!(refused.failure, ExitFailure::Construction);
        assert!(
            refused.detail.contains("hand the position"),
            "{}",
            refused.detail
        );
    }

    #[test]
    fn a_tipped_exit_serialises_with_its_tip_account_among_the_keys() {
        let signer = MockSolanaSigner::new();
        let target = a_target();
        let route = signer
            .pump_fun_route(&target.mint, target.size_lamports)
            .expect("routes");
        let policy = TipPolicy::emergency().selecting(TipAccountSelection::Fixed(5));
        let plan =
            build_exit(&target, &route, Some(&policy), "exit-1".to_string(), 0, 0).expect("builds");
        let bytes = plan.message_bytes();

        let (keys, used) = read_compact_len(&bytes[3..]).expect("a key count");
        assert_eq!(keys, plan.transaction.message.account_keys.len());
        let mut at = 3 + used;
        let mut tip_account_is_in_there = false;
        for _ in 0..keys {
            let key = Pubkey::new(bytes[at..at + 32].try_into().expect("thirty-two bytes"));
            tip_account_is_in_there |= key == JITO_TIP_KEYS[5];
            at += 32;
        }
        assert!(
            tip_account_is_in_there,
            "the tip account never reached the wire"
        );

        at += 32; // the blockhash
        let (instructions, used) = read_compact_len(&bytes[at..]).expect("an instruction count");
        assert_eq!(instructions, 4);
        at += used;
        // Walk to the last instruction and read the amount back off the wire.
        for _ in 0..instructions {
            at += 1; // the program id index
            let (accounts, used) = read_compact_len(&bytes[at..]).expect("an account count");
            at += used + accounts;
            let (data_len, used) = read_compact_len(&bytes[at..]).expect("a data length");
            at += used;
            if at + data_len == bytes.len() {
                assert_eq!(data_len, 12, "the last instruction is the transfer");
                assert_eq!(&bytes[at..at + 4], &2u32.to_le_bytes());
                assert_eq!(
                    u64::from_le_bytes(bytes[at + 4..at + 12].try_into().expect("eight bytes")),
                    EXIT_TIP_BASE_LAMPORTS
                );
            }
            at += data_len;
        }
        assert_eq!(at, bytes.len(), "and nothing after it");

        // The wire form is the signature and then exactly those bytes.
        let signed = signer.sign(&plan).expect("signs");
        let wire = signed.wire();
        assert_eq!(wire[0], 1, "one signature");
        assert_eq!(&wire[1..65], signed.signature.as_bytes());
        assert_eq!(&wire[65..], &bytes[..]);
    }

    // -- the atomic simulation -----------------------------------------------

    /// Builds the plan every test below then breaks in a different way, so each
    /// one is exactly one difference from a transaction that simulates.
    fn a_tipped_plan() -> (MockSolanaSigner, ExitRoute, ExitPlan) {
        let signer = MockSolanaSigner::new();
        let target = a_target();
        let route = signer
            .pump_fun_route(&target.mint, target.size_lamports)
            .expect("routes");
        let policy = TipPolicy::emergency().selecting(TipAccountSelection::Fixed(3));
        let plan =
            build_exit(&target, &route, Some(&policy), "exit-1".to_string(), 0, 0).expect("builds");
        (signer, route, plan)
    }

    #[test]
    fn an_exit_this_module_built_simulates_atomically() {
        let (_signer, route, plan) = a_tipped_plan();
        let sim = simulate_exit(&plan, &route).expect("the exit this module builds is atomic");

        assert_eq!(sim.instructions, 4, "budget, price, swap, tip — one unit");
        assert_eq!(sim.swap_index, 2);
        assert_eq!(
            sim.tip_index,
            Some(3),
            "and the tip is after the sale that funds it"
        );
        assert_eq!(sim.min_out_lamports, plan.min_out_lamports);
        assert_eq!(sim.tokens, plan.tokens);
        assert_eq!(sim.requoted_out_lamports, plan.expected_out_lamports);
    }

    #[test]
    fn an_untipped_exit_simulates_with_nothing_to_pay() {
        let signer = MockSolanaSigner::new();
        let target = a_target();
        let route = signer
            .pump_fun_route(&target.mint, target.size_lamports)
            .expect("routes");
        let plan = build_exit(&target, &route, None, "exit-1".to_string(), 0, 0).expect("builds");

        let sim = simulate_exit(&plan, &route).expect("simulates");
        assert_eq!(sim.instructions, 3);
        assert_eq!(sim.tip_index, None);
        assert_eq!(sim.costs.tip_lamports, 0);
    }

    #[test]
    fn a_raydium_exit_simulates_against_its_own_pool() {
        let signer = MockSolanaSigner::new();
        let mut target = a_target();
        target.mint = "GraduatedMint".to_string();
        let pool = RaydiumPool {
            base_reserve: 900_000_000_000_000,
            quote_reserve: 400 * LAMPORTS_PER_SOL,
        };
        let route = signer.raydium_route(&target.mint, 5_000_000_000_000, pool);
        let plan = build_exit(&target, &route, None, "exit-1".to_string(), 0, 0).expect("builds");

        let sim = simulate_exit(&plan, &route).expect("simulates");
        assert_eq!(plan.venue, Venue::RaydiumAmmV4);
        assert_eq!(sim.swap_index, 2);
        // Raydium's layout is a tag byte and then the same two numbers, and the
        // simulation reads it off that layout rather than off pump.fun's.
        assert_eq!(sim.tokens, route.tokens);
        assert_eq!(sim.min_out_lamports, plan.min_out_lamports);
    }

    /// The check the whole thing exists for: a floor that is in the struct and
    /// not in the bytes is not a floor, and only reading the bytes finds that.
    #[test]
    fn a_floor_the_instruction_data_does_not_carry_is_caught() {
        let (_signer, route, mut plan) = a_tipped_plan();
        let sell = &mut plan.transaction.message.instructions[2];
        sell.data[16..24].copy_from_slice(&1u64.to_le_bytes());

        assert_eq!(
            simulate_exit(&plan, &route),
            Err(AtomicityBreach::Mismatch {
                field: "the floor",
                planned: plan.min_out_lamports,
                encoded: 1,
            })
        );
    }

    #[test]
    fn a_parcel_the_instruction_data_does_not_carry_is_caught() {
        let (_signer, route, mut plan) = a_tipped_plan();
        let sell = &mut plan.transaction.message.instructions[2];
        sell.data[8..16].copy_from_slice(&7u64.to_le_bytes());

        assert_eq!(
            simulate_exit(&plan, &route),
            Err(AtomicityBreach::Mismatch {
                field: "tokens",
                planned: plan.tokens,
                encoded: 7,
            })
        );
    }

    #[test]
    fn a_floor_of_nothing_is_not_a_floor() {
        let (_signer, route, mut plan) = a_tipped_plan();
        plan.min_out_lamports = 0;
        plan.transaction.message.instructions[2].data[16..24].copy_from_slice(&0u64.to_le_bytes());

        assert_eq!(
            simulate_exit(&plan, &route),
            Err(AtomicityBreach::Unfloored {
                min_out_lamports: 0,
                expected_out_lamports: plan.expected_out_lamports,
            })
        );
    }

    /// Two sells is a position sold twice, and the second one against a curve
    /// the first one moved. It is the exact shape of the partial state Phase 4
    /// says a bundle must not be able to leave behind.
    #[test]
    fn a_transaction_that_sells_twice_is_not_an_exit() {
        let (_signer, route, mut plan) = a_tipped_plan();
        let sell = plan.transaction.message.instructions[2].clone();
        plan.transaction.message.instructions.insert(3, sell);

        assert_eq!(
            simulate_exit(&plan, &route),
            Err(AtomicityBreach::NotOneSwap(2))
        );
    }

    #[test]
    fn a_transaction_that_sells_nothing_is_not_an_exit_either() {
        let (_signer, route, mut plan) = a_tipped_plan();
        plan.transaction.message.instructions.remove(2);

        assert_eq!(
            simulate_exit(&plan, &route),
            Err(AtomicityBreach::NotOneSwap(0))
        );
    }

    /// The ordering the funding argument turns on. Nothing half-happens either
    /// way — but a transfer ahead of the sale is covered by what the wallet
    /// already held, and on the path that runs when things have gone wrong that
    /// is the difference between landing and failing for want of the money the
    /// exit was about to make.
    #[test]
    fn a_tip_ahead_of_the_sale_that_funds_it_is_refused() {
        let (_signer, route, mut plan) = a_tipped_plan();
        plan.transaction.message.instructions.swap(2, 3);

        assert_eq!(
            simulate_exit(&plan, &route),
            Err(AtomicityBreach::TipOutOfOrder {
                tip_index: 2,
                swap_index: 3,
                instructions: 4,
            })
        );
    }

    /// `build_exit` refuses a tip larger than the *quote*. This is the stricter
    /// question and the one that matters: a tip between the floor and the quote
    /// is unfunded in exactly the case the floor exists to describe.
    #[test]
    fn a_tip_the_floor_does_not_cover_is_refused_even_though_the_quote_would() {
        let signer = MockSolanaSigner::new();
        let target = a_target();
        let route = signer
            .pump_fun_route(&target.mint, target.size_lamports)
            .expect("routes");
        let plain = build_exit(&target, &route, None, "exit-1".to_string(), 0, 0).expect("builds");

        // Strictly between the floor and the quote, so the old check passes it.
        let between = (plain.min_out_lamports + plain.expected_out_lamports) / 2;
        assert!(between > plain.min_out_lamports && between < plain.expected_out_lamports);

        let policy = TipPolicy::emergency().bounded(between, between);
        let refused = build_exit(&target, &route, Some(&policy), "exit-2".to_string(), 0, 0)
            .expect_err("the simulation refuses it");
        assert_eq!(refused.failure, ExitFailure::Construction);
        assert!(
            refused.detail.contains("did not simulate atomically")
                && refused.detail.contains("the floor guarantees"),
            "{}",
            refused.detail
        );
    }

    #[test]
    fn a_tip_nobody_planned_is_refused() {
        let signer = MockSolanaSigner::new();
        let target = a_target();
        let route = signer
            .pump_fun_route(&target.mint, target.size_lamports)
            .expect("routes");
        let mut plan =
            build_exit(&target, &route, None, "exit-1".to_string(), 0, 0).expect("builds");

        // A transfer added to a transaction whose plan bids nothing: the shape
        // of a tip that would never appear on a receipt.
        let stowaway = system_transfer(route.payer, JITO_TIP_KEYS[0], 9_000);
        plan.transaction.message = Message::compile(
            route.payer,
            &[
                set_compute_unit_limit(EXIT_COMPUTE_UNIT_LIMIT),
                set_compute_unit_price(EXIT_COMPUTE_UNIT_PRICE),
                route.swap(plan.min_out_lamports),
                stowaway,
            ],
            route.recent_blockhash,
        )
        .expect("compiles");

        let breach = simulate_exit(&plan, &route).expect_err("refused");
        assert!(
            matches!(breach, AtomicityBreach::TipUnaccounted(ref why) if why.contains("9000")),
            "{breach}"
        );
    }

    /// Every system instruction is four tag bytes and then numbers, so the tag
    /// is what tells a transfer from an `Assign`. Reading lamports out of one
    /// without checking it would report a confident tip for an instruction that
    /// does something else.
    #[test]
    fn a_system_instruction_that_is_not_a_transfer_is_not_a_tip() {
        let (_signer, route, mut plan) = a_tipped_plan();
        let stowaway = plan
            .transaction
            .message
            .instructions
            .last_mut()
            .expect("the tip");
        stowaway.data[..4].copy_from_slice(&1u32.to_le_bytes());

        let breach = simulate_exit(&plan, &route).expect_err("refused");
        assert!(
            matches!(breach, AtomicityBreach::TipUnaccounted(ref why)
                if why.contains("not a transfer")),
            "{breach}"
        );
    }

    #[test]
    fn a_planned_tip_nobody_pays_is_refused() {
        let (_signer, route, mut plan) = a_tipped_plan();
        plan.transaction.message.instructions.pop();

        let breach = simulate_exit(&plan, &route).expect_err("refused");
        assert!(
            matches!(breach, AtomicityBreach::TipUnaccounted(ref why) if why.contains("bid")),
            "{breach}"
        );
    }

    /// "No public-mempool fallback" is structural only if the transaction cannot
    /// reach one. An allowlist checked against the compiled message is what that
    /// means when it is checked rather than asserted.
    #[test]
    fn an_instruction_to_a_program_an_exit_does_not_touch_is_refused() {
        let (_signer, route, mut plan) = a_tipped_plan();
        let stranger = mock_key("some_other_program", "elsewhere");
        plan.transaction.message.account_keys.push(stranger);
        let index = u8::try_from(plan.transaction.message.account_keys.len() - 1).expect("fits");
        plan.transaction.message.instructions[0].program_id_index = index;

        assert_eq!(
            simulate_exit(&plan, &route),
            Err(AtomicityBreach::ForeignProgram {
                index: 0,
                program: stranger
            })
        );
    }

    #[test]
    fn a_message_with_somebody_else_at_the_front_is_not_signable() {
        let (_signer, route, mut plan) = a_tipped_plan();
        plan.transaction.message.account_keys[0] = mock_key("not_the_payer", "elsewhere");

        let breach = simulate_exit(&plan, &route).expect_err("refused");
        assert!(
            matches!(breach, AtomicityBreach::NotSignable(ref why) if why.contains("fee payer")),
            "{breach}"
        );
    }

    /// The closest thing to `simulateTransaction` without a cluster: price the
    /// same sale against the same reserves and get the same number back.
    #[test]
    fn a_plan_priced_against_reserves_it_does_not_name_is_repriced() {
        let (_signer, mut route, plan) = a_tipped_plan();
        route.kind = match route.kind {
            ExitRouteKind::PumpFunCurve { accounts, .. } => ExitRouteKind::PumpFunCurve {
                accounts,
                curve: CurveState::at_real_sol(MOCK_CURVE_REAL_SOL * 2),
            },
            other => other,
        };

        let breach = simulate_exit(&plan, &route).expect_err("refused");
        assert!(
            matches!(breach, AtomicityBreach::Repriced { planned, .. }
                if planned == plan.expected_out_lamports),
            "{breach}"
        );
    }

    /// §18's network rows, read off the transaction rather than off the
    /// constants that built it.
    #[test]
    fn the_simulation_reports_what_the_transaction_costs_to_send() {
        let (_signer, route, plan) = a_tipped_plan();
        let sim = simulate_exit(&plan, &route).expect("simulates");

        assert_eq!(sim.costs.signatures, 1);
        assert_eq!(sim.costs.base_lamports, BASE_SIGNATURE_FEE_LAMPORTS);
        assert_eq!(
            sim.costs.priority_lamports,
            priority_fee_lamports(EXIT_COMPUTE_UNIT_PRICE, EXIT_COMPUTE_UNIT_LIMIT)
        );
        assert_eq!(
            sim.costs.rent_lamports, 0,
            "an exit sells out of an account that exists"
        );
        assert_eq!(sim.costs.tip_lamports, EXIT_TIP_BASE_LAMPORTS);
        assert_eq!(
            sim.cost_lamports(),
            BASE_SIGNATURE_FEE_LAMPORTS + 2_000 + EXIT_TIP_BASE_LAMPORTS
        );
        // A fill under the floor reverts the whole transaction, and a revert is
        // the base and the priority and nothing else: the tip transfer went
        // back with everything around it.
        assert_eq!(
            sim.failed_cost_lamports(),
            BASE_SIGNATURE_FEE_LAMPORTS + 2_000
        );
    }

    // -- broadcast drops -----------------------------------------------------

    #[test]
    fn the_backoff_doubles_and_then_holds_at_its_ceiling() {
        let policy = BroadcastPolicy {
            max_rebroadcasts: 6,
            initial_backoff_ms: 400,
            backoff_factor: 2,
            max_backoff_ms: 2_000,
            max_leader_wait_ms: 800,
            total_budget_ms: 15_000,
        };
        assert_eq!(policy.backoff_ms(0), 400);
        assert_eq!(policy.backoff_ms(1), 800);
        assert_eq!(policy.backoff_ms(2), 1_600);
        assert_eq!(policy.backoff_ms(3), 2_000);
        assert_eq!(policy.backoff_ms(9), 2_000);

        let mut last = 0;
        for retry in 0..64 {
            let wait = policy.backoff_ms(retry);
            assert!(
                wait >= last,
                "retry {retry} waits less than the one before it"
            );
            last = wait;
        }
    }

    #[test]
    fn a_backoff_that_would_stand_still_or_overflow_does_neither() {
        // A factor of zero is read as one. A retry loop with no wait in it is a
        // busy loop against a node that has just failed to answer.
        let flat = BroadcastPolicy {
            backoff_factor: 0,
            initial_backoff_ms: 250,
            max_backoff_ms: 5_000,
            ..BroadcastPolicy::default()
        };
        for retry in 0..8 {
            assert_eq!(flat.backoff_ms(retry), 250, "retry {retry}");
        }

        // And a factor that would run off the end of a u64 stops at the
        // ceiling rather than wrapping to something shorter than it started.
        let steep = BroadcastPolicy {
            backoff_factor: u32::MAX,
            initial_backoff_ms: 1_000,
            max_backoff_ms: 9_000,
            ..BroadcastPolicy::default()
        };
        assert_eq!(steep.backoff_ms(1), 9_000);
        assert_eq!(steep.backoff_ms(60), 9_000);
    }

    #[test]
    fn a_dropped_broadcast_is_sent_again_until_it_lands() {
        let signer = RecordingSigner::new(MockSolanaSigner::new());
        let target = a_target();
        signer
            .inner
            .inject(&target.intent_id, MockFault::Dropped(2));
        let (plan, signed) = a_signed_exit(&signer, &target);

        let mut clock = FakeClock::new(1_700_000_000_000);
        let started = clock.now_ms();
        let mut steps: Vec<BroadcastStep> = Vec::new();
        let run = broadcast_until_settled(
            &signer,
            &BroadcastPolicy::default(),
            &signed,
            started,
            &mut clock,
            &mut |step| steps.push(step.clone()),
        );

        let fill = run.outcome.expect("it landed on the third push");
        assert_eq!(fill.out_lamports, plan.expected_out_lamports);
        assert_eq!(run.rebroadcasts, 2);
        assert_eq!(run.state, ExitState::ExitBroadcast);
        assert_eq!(
            clock.waits,
            vec![400, 800],
            "and it waited longer each time"
        );
        assert_eq!(run.waited_ms, 1_200);

        // One step onto the network, then one per push after it.
        assert_eq!(steps.len(), 3);
        assert_eq!(
            (steps[0].from, steps[0].to),
            (ExitState::ExitSigned, ExitState::ExitBroadcast)
        );
        assert!(
            steps[0].detail.is_none(),
            "the first broadcast needs no explanation"
        );
        for step in &steps[1..] {
            assert_eq!(
                (step.from, step.to),
                (ExitState::ExitBroadcast, ExitState::ExitBroadcast),
                "a rebroadcast moves the exit nowhere"
            );
            assert!(step
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("sent again"));
        }
        assert_eq!(
            run.steps, steps,
            "the recorder and the run saw the same thing"
        );

        // Three sends of the same bytes. A retry that re-signed anything would
        // be a second signature, and a second chance to sell the position.
        let sent = signer.sent();
        assert_eq!(sent.len(), 3);
        assert!(sent.iter().all(|wire| *wire == signed.wire()));
    }

    #[test]
    fn a_backend_with_no_schedule_holds_nothing_and_sends_straight_out() {
        // The baseline every backend in this build takes, written down so that
        // adding the seam is provably a function call and not a delay.
        let signer = RecordingSigner::new(MockSolanaSigner::new());
        let target = a_target();
        let (_, signed) = a_signed_exit(&signer, &target);

        let mut clock = FakeClock::new(1_700_000_000_000);
        let started = clock.now_ms();
        let mut steps: Vec<BroadcastStep> = Vec::new();
        let run = broadcast_until_settled(
            &signer,
            &BroadcastPolicy::default(),
            &signed,
            started,
            &mut clock,
            &mut |step| steps.push(step.clone()),
        );

        assert!(run.outcome.is_ok(), "it went out and it landed");
        assert!(
            signer.leader_schedule().is_none(),
            "there is nothing to ask"
        );
        assert_eq!(run.leader_hint, LeaderHint::Unknown);
        assert_eq!(run.leader_waited_ms, 0);
        assert!(clock.waits.is_empty(), "and the clock never moved");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].at_ms, started);
        assert!(
            steps[0].detail.is_none(),
            "the first broadcast needs no explanation"
        );
    }

    #[test]
    fn a_schedule_with_nothing_to_say_is_asked_and_changes_nothing() {
        // The stock schedule knows nothing, whenever it is asked.
        assert_eq!(UnknownLeaderSchedule.hint(0), LeaderHint::Unknown);
        assert_eq!(
            UnknownLeaderSchedule.hint(1_700_000_000_000),
            LeaderHint::Unknown
        );

        // Both send-now answers send now — and the run still says which one it
        // was. "Nobody knows" and "nobody is near" are the same delay and not
        // the same fact, and only one of them is a schedule working.
        for answer in [LeaderHint::Unknown, LeaderHint::NoneInReach] {
            let schedule = StubSchedule::saying(&[answer]);
            let asked = schedule.counter();
            let signer = RecordingSigner::new(MockSolanaSigner::new()).following(schedule);
            let target = a_target();
            let (_, signed) = a_signed_exit(&signer, &target);

            let mut clock = FakeClock::new(1_700_000_000_000);
            let started = clock.now_ms();
            let mut steps: Vec<BroadcastStep> = Vec::new();
            let run = broadcast_until_settled(
                &signer,
                &BroadcastPolicy::default(),
                &signed,
                started,
                &mut clock,
                &mut |step| steps.push(step.clone()),
            );

            assert!(run.outcome.is_ok(), "{answer:?} still lands");
            assert_eq!(
                asked.load(Ordering::Relaxed),
                1,
                "{answer:?} was asked once"
            );
            assert_eq!(run.leader_hint, answer);
            assert_eq!(run.leader_waited_ms, 0);
            assert!(clock.waits.is_empty(), "{answer:?} held nothing back");
            assert_eq!(steps.len(), 1);
            assert!(
                steps[0].detail.is_none(),
                "{answer:?} has nothing to explain"
            );
        }
    }

    #[test]
    fn a_connected_leader_holds_the_send_and_the_hold_is_written_down() {
        let schedule = StubSchedule::saying(&[LeaderHint::Connected { wait_ms: 250 }]);
        let asked = schedule.counter();
        let signer = RecordingSigner::new(MockSolanaSigner::new()).following(schedule);
        let target = a_target();
        let (_, signed) = a_signed_exit(&signer, &target);

        let mut clock = FakeClock::new(1_700_000_000_000);
        let started = clock.now_ms();
        let mut steps: Vec<BroadcastStep> = Vec::new();
        let run = broadcast_until_settled(
            &signer,
            &BroadcastPolicy::default(),
            &signed,
            started,
            &mut clock,
            &mut |step| steps.push(step.clone()),
        );

        assert!(run.outcome.is_ok());
        assert_eq!(asked.load(Ordering::Relaxed), 1);
        assert_eq!(
            clock.waits,
            vec![250],
            "it waited for the leader and only that"
        );
        assert_eq!(run.leader_hint, LeaderHint::Connected { wait_ms: 250 });
        assert_eq!(run.leader_waited_ms, 250);
        assert_eq!(
            run.waited_ms, 0,
            "a hold is not a backoff and is not counted as one"
        );
        assert_eq!(run.rebroadcasts, 0, "and it is not a retry either");

        // One send, timestamped after the hold rather than before it: a ledger
        // that dated the broadcast to the moment the hold started would put the
        // transaction on the network before it was there.
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].at_ms, started + 250);
        assert!(
            steps[0]
                .detail
                .as_deref()
                .unwrap_or_default()
                .contains("held 250ms"),
            "a send that was late says why: {:?}",
            steps[0].detail
        );

        // And the bytes are unchanged by any of it.
        assert_eq!(signer.sent(), vec![signed.wire()]);
    }

    #[test]
    fn a_hold_stops_at_its_ceiling_and_gives_way_to_the_budget() {
        // A wait longer than the policy allows is cut to the policy, not taken.
        // Nothing in this build measures how far a leader is, so the ceiling is
        // the only thing standing between an unwind and a schedule that answers
        // with a minute.
        let signer =
            RecordingSigner::new(MockSolanaSigner::new()).following(StubSchedule::saying(&[
                LeaderHint::Connected { wait_ms: 60_000 },
            ]));
        let target = a_target();
        let (_, signed) = a_signed_exit(&signer, &target);

        let policy = BroadcastPolicy::default();
        let mut clock = FakeClock::new(1_700_000_000_000);
        let started = clock.now_ms();
        let run =
            broadcast_until_settled(&signer, &policy, &signed, started, &mut clock, &mut |_| {});
        assert!(run.outcome.is_ok());
        assert_eq!(clock.waits, vec![policy.max_leader_wait_ms]);
        assert_eq!(run.leader_waited_ms, policy.max_leader_wait_ms);

        // And a hold that fits under the ceiling but not inside the loop's
        // whole budget is not taken at all. Missing a block engine costs a
        // bundle; passing the budget costs the exit.
        let tight = BroadcastPolicy {
            total_budget_ms: 100,
            ..BroadcastPolicy::default()
        };
        assert!(
            tight.max_leader_wait_ms > tight.total_budget_ms,
            "the ceiling must not be what stops this one, or it proves nothing"
        );
        let signer =
            RecordingSigner::new(MockSolanaSigner::new()).following(StubSchedule::saying(&[
                LeaderHint::Connected { wait_ms: 500 },
            ]));
        let target = a_target();
        let (_, signed) = a_signed_exit(&signer, &target);

        let mut clock = FakeClock::new(1_700_000_000_000);
        let started = clock.now_ms();
        let mut steps: Vec<BroadcastStep> = Vec::new();
        let run =
            broadcast_until_settled(&signer, &tight, &signed, started, &mut clock, &mut |step| {
                steps.push(step.clone())
            });

        assert!(run.outcome.is_ok(), "the exit still went out");
        assert!(clock.waits.is_empty(), "and it went out now");
        assert_eq!(run.leader_waited_ms, 0);
        assert_eq!(
            run.leader_hint,
            LeaderHint::Connected { wait_ms: 500 },
            "it was asked and it answered; the budget is why it did not wait"
        );
        assert!(
            steps[0].detail.is_none(),
            "nothing was held, so there is nothing to say"
        );
    }

    #[test]
    fn every_push_asks_the_schedule_again_because_the_leader_moved_on() {
        // A leader that was up when the first send went out is not the leader
        // two slots later, so a loop that asked once and reused the answer
        // would be routing a retry on a fact that expired before it.
        let schedule =
            StubSchedule::saying(&[LeaderHint::Unknown, LeaderHint::Connected { wait_ms: 100 }]);
        let asked = schedule.counter();
        let signer = RecordingSigner::new(MockSolanaSigner::new()).following(schedule);
        let target = a_target();
        signer
            .inner
            .inject(&target.intent_id, MockFault::Dropped(2));
        let (_, signed) = a_signed_exit(&signer, &target);

        let mut clock = FakeClock::new(1_700_000_000_000);
        let started = clock.now_ms();
        let mut steps: Vec<BroadcastStep> = Vec::new();
        let run = broadcast_until_settled(
            &signer,
            &BroadcastPolicy::default(),
            &signed,
            started,
            &mut clock,
            &mut |step| steps.push(step.clone()),
        );

        assert!(run.outcome.is_ok(), "it landed on the third push");
        assert_eq!(
            asked.load(Ordering::Relaxed),
            3,
            "once per send, not once per run"
        );
        assert_eq!(run.rebroadcasts, 2);

        // Backoff and hold alternate, and stay separately accounted for.
        assert_eq!(clock.waits, vec![400, 100, 800, 100]);
        assert_eq!(run.waited_ms, 1_200, "the backoff, and only the backoff");
        assert_eq!(run.leader_waited_ms, 200, "the holds, and only the holds");
        assert_eq!(
            run.leader_hint,
            LeaderHint::Connected { wait_ms: 100 },
            "the last answer"
        );

        // Still three sends and still three steps: holding a send back is not a
        // send, so it adds no transition for the flattener to count.
        assert_eq!(signer.sent().len(), 3);
        assert_eq!(steps.len(), 3);
        assert!(steps[0].detail.is_none(), "the first went out unheld");
        for step in &steps[1..] {
            assert_eq!(
                (step.from, step.to),
                (ExitState::ExitBroadcast, ExitState::ExitBroadcast)
            );
            let detail = step.detail.as_deref().unwrap_or_default();
            assert!(detail.contains("sent again"), "{detail}");
            assert!(
                detail.contains("100ms held for a connected leader"),
                "{detail}"
            );
        }
    }

    #[test]
    fn a_broadcast_that_never_lands_gives_up_at_the_retry_ceiling() {
        let signer = MockSolanaSigner::new();
        let target = a_target();
        signer.inject(&target.intent_id, MockFault::AlwaysDropped);
        let (_, signed) = a_signed_exit(&signer, &target);

        let policy = BroadcastPolicy {
            max_rebroadcasts: 2,
            ..BroadcastPolicy::default()
        };
        let mut clock = FakeClock::new(0);
        let run = broadcast_until_settled(&signer, &policy, &signed, 0, &mut clock, &mut |_| {});

        let err = run.outcome.expect_err("it never landed");
        assert_eq!(err.failure, ExitFailure::NotConfirmed);
        assert!(err.detail.contains("sent 3 time(s)"), "{}", err.detail);
        assert_eq!(
            run.rebroadcasts, 2,
            "a ceiling that can be talked past is not one"
        );
        assert_eq!(clock.waits, vec![400, 800]);
        assert!(
            run.state.is_dispatched(),
            "the bytes are still out there, so the position's status is unknown rather than \
             untouched"
        );
    }

    #[test]
    fn an_expired_blockhash_is_never_pushed_again() {
        let signer = MockSolanaSigner::new();
        let target = a_target();
        signer.inject(&target.intent_id, MockFault::NotConfirmed);
        let (_, signed) = a_signed_exit(&signer, &target);

        let mut clock = FakeClock::new(0);
        let run = broadcast_until_settled(
            &signer,
            &BroadcastPolicy::default(),
            &signed,
            0,
            &mut clock,
            &mut |_| {},
        );

        let err = run.outcome.expect_err("it expired");
        assert_eq!(err.failure, ExitFailure::NotConfirmed);
        assert_eq!(
            run.rebroadcasts, 0,
            "bytes that can no longer land are not worth sending again"
        );
        assert!(clock.waits.is_empty(), "and nothing waited for them");
    }

    #[test]
    fn the_loop_stops_at_its_budget_with_retries_still_unspent() {
        let signer = MockSolanaSigner::new();
        let target = a_target();
        signer.inject(&target.intent_id, MockFault::AlwaysDropped);
        let (_, signed) = a_signed_exit(&signer, &target);

        // Room for twenty retries and time for one.
        let policy = BroadcastPolicy {
            max_rebroadcasts: 20,
            initial_backoff_ms: 400,
            backoff_factor: 2,
            max_backoff_ms: 2_000,
            max_leader_wait_ms: 800,
            total_budget_ms: 1_000,
        };
        let mut clock = FakeClock::new(5_000);
        let run =
            broadcast_until_settled(&signer, &policy, &signed, 5_000, &mut clock, &mut |_| {});

        let err = run.outcome.expect_err("out of time");
        assert_eq!(err.failure, ExitFailure::NotConfirmed);
        assert!(err.detail.contains("1000ms to land"), "{}", err.detail);
        assert_eq!(
            run.rebroadcasts, 1,
            "the 400ms wait fitted; the 800ms after it did not"
        );
        assert_eq!(clock.waits, vec![400]);
    }

    #[test]
    fn bytes_no_node_accepted_never_reached_the_network() {
        let signer = MockSolanaSigner::new();
        let target = a_target();
        signer.inject(&target.intent_id, MockFault::Broadcast);
        let (_, signed) = a_signed_exit(&signer, &target);

        let mut clock = FakeClock::new(0);
        let mut steps: Vec<BroadcastStep> = Vec::new();
        let run = broadcast_until_settled(
            &signer,
            &BroadcastPolicy::default(),
            &signed,
            0,
            &mut clock,
            &mut |step| steps.push(step.clone()),
        );

        let err = run.outcome.expect_err("no node took it");
        assert_eq!(err.failure, ExitFailure::Broadcast);
        assert_eq!(run.state, ExitState::ExitSigned);
        assert!(
            !run.state.is_dispatched(),
            "nothing is on the network, so the position is exactly where it was"
        );
        assert!(steps.is_empty(), "and there is no broadcast to write down");
    }

    #[test]
    fn every_push_of_one_exit_is_written_down_as_its_own_step() {
        let temp = TempDb::new("rebroadcast");
        let db = temp.open();
        let signer = MockSolanaSigner::new();
        let target = a_target();
        signer.inject(&target.intent_id, MockFault::Dropped(2));

        let report = Flattener::new(&signer, &db, 1_700_000_000_000)
            .waiting_with(Box::new(FakeClock::new(1_700_000_000_000)))
            .flatten(std::slice::from_ref(&target));

        assert!(report.problems.is_empty(), "{:?}", report.problems);
        match &report.results[0].outcome {
            FlattenOutcome::Flattened { reused, .. } => assert!(!reused),
            other => panic!("expected a closed position, got {other:?}"),
        }

        // Constructed, signed, broadcast, two more pushes, confirmed. Written
        // as they happened rather than summarised at the end: a process that
        // died mid-retry has to come back knowing how many times the bytes went
        // out.
        assert_eq!(
            db.health().expect("health").intent_transitions,
            6,
            "an exit that had to be pushed twice leaves two more rows than one that did not"
        );
    }

    #[test]
    fn the_mock_is_shared_across_threads_without_losing_a_count() {
        use std::sync::Arc;
        let signer = Arc::new(MockSolanaSigner::new());
        let mut handles = Vec::new();
        for thread in 0..4u8 {
            let signer = Arc::clone(&signer);
            handles.push(std::thread::spawn(move || {
                for n in 0..25u8 {
                    let mut target = a_target();
                    target.intent_id = format!("origin-{thread}-{n}");
                    let route = signer
                        .pump_fun_route(&target.mint, target.size_lamports)
                        .expect("routes");
                    let plan =
                        build_exit(&target, &route, None, format!("exit-{thread}-{n}"), 0, 0)
                            .expect("builds");
                    let signed = signer.sign(&plan).expect("signs");
                    signer.broadcast(&signed).expect("broadcasts");
                    signer
                        .confirm(&signed)
                        .expect("confirms")
                        .landed()
                        .expect("lands");
                }
            }));
        }
        for handle in handles {
            handle.join().expect("the thread did not panic");
        }
        assert_eq!(signer.counters(), (100, 100, 100, 0));
        assert_eq!(
            signer.in_flight(),
            0,
            "a hundred exits settled and the backend is carrying none of them; \
             memory that grew with the number of exits ever sent would be a leak \
             with a long fuse in a process meant to run for days"
        );
    }
}
