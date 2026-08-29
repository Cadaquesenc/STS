//! The book: what was actually traded, and the four things that decided it.
//!
//! `db.rs` owns two ledgers already and this is not a third. `execution_logs`
//! is the six-state machine an intent walks; `intent_transitions` is the finer
//! machine one exit transaction walks. Both answer "what happened, in order",
//! both are append-only, and neither can answer "what did this cost" without a
//! join nobody wants to write twice — which is the question a person asks of a
//! trade journal, and the question every number below is arranged around.
//!
//! So this is migration 3 on the same file, through the same connection, under
//! the same one-writer rule. Five tables: one row per trade, and fills, routes,
//! tips and signatures hanging off it.
//!
//! # Four decisions shape it
//!
//! **The keys are deterministic and there is no `AUTOINCREMENT` anywhere.**
//! Every child row keys off `trade_id`, which is the intent id the rest of the
//! system already calls the trade by, and off a sequence the caller supplies.
//! An `INTEGER PRIMARY KEY` would number rows by the order they were inserted,
//! and Phase 3's acceptance criterion is that one fixture and one seed produce
//! byte-identical records — a key that depends on how many trades happened to
//! come first fails that on every second run. It also means a replay can be
//! written twice and the second pass is a no-op rather than a duplicate book.
//!
//! **There is no `REAL` column and no `f64` in any type here.** Money is
//! lamports and tokens are base units, both integers, both `SUM`-able in SQL
//! and both indexable — which is the whole reason the storage unit is not the
//! `10^-18` one. SQLite's `INTEGER` is an `i64`, and one SOL at `10^-18` is
//! `10^18`: ten SOL in that unit does not fit in a column, and a quantity kept
//! as text or a blob to make it fit is a quantity that cannot be compared in a
//! `WHERE` or added in a `SUM`. Lamports are already exact fixed point at
//! `10^-9` and the whole supply of SOL fits in an `i64` with a factor of
//! fifteen to spare.
//!
//! The one quantity that is genuinely a ratio is a price — lamports per token
//! base unit — and that is carried as [`Q18`], `u128` at `10^-18`, stored as
//! its raw count in an `INTEGER`. Two things keep that honest. The conversion
//! out refuses anything past `i64::MAX` rather than saturating, which at this
//! scale is nine lamports for one base unit and no token on either venue this
//! build trades comes within nine orders of magnitude of it. And the price is
//! never the only record of itself: the lamports and the tokens it was computed
//! from sit in the same row, so the exact value is always recoverable and the
//! `price_q18` column is what it says it is — a floored key to filter and sort
//! on, derived by [`FillRow::settle`] so it cannot disagree with the pair.
//!
//! **Every model derives `Eq`.** Free, once there is no float in them, and it
//! is what makes "the row that went in is the row that came out" a single
//! `assert_eq!` rather than a field-by-field comparison that quietly stops
//! covering the field somebody adds next.
//!
//! **The write path builds no strings.** Statements are `prepare_cached`, so
//! the SQL is parsed once per connection and never rebuilt; every parameter is
//! bound from a borrow of the model, so a batch of a thousand fills allocates
//! nothing per row. That is as far as "zero allocation" honestly goes here and
//! the claim is not stretched further: `rusqlite` allocates inside its own
//! binding, and the read path allocates the `String`s it hands back, because a
//! row of text has to live somewhere. The engine's tick path never blocks on
//! either — it hands rows over and the write happens behind the same mutex
//! every other writer in the process queues on.

use std::collections::BTreeMap;

use rusqlite::{OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::db::{Database, ExecutionMode, Side};
use crate::error::EngineError;
use crate::execution::{TipBid, TipStance};
use crate::strategy::fixed::Q18;
use crate::types::Venue;

/// The journal's tables, as migration 3.
///
/// Registered in `db.rs`'s `MIGRATIONS` beside the two ledgers, rather than
/// applied from here. One chain, one checksum per link, one version on the
/// file: a second migration runner against the same database is how two builds
/// end up disagreeing about what version means.
pub(crate) const MIGRATION_0003: &str = "
    CREATE TABLE IF NOT EXISTS journal_trades (
        -- The intent id. Deterministic, supplied, and never generated here.
        trade_id              TEXT    PRIMARY KEY,

        mint                  TEXT    NOT NULL,
        side                  TEXT    NOT NULL CHECK (side IN ('buy', 'sell')),
        mode                  TEXT    NOT NULL CHECK (mode IN ('live', 'paper', 'replay')),
        -- Null until a route is chosen. A trade that never routed has no venue,
        -- and naming one would be inventing where it would have gone.
        venue                 TEXT    CHECK (venue IS NULL OR venue IN (
                                        'pump_fun_curve', 'raydium_amm_v4')),

        notional_lamports     INTEGER NOT NULL CHECK (notional_lamports >= 0),
        tokens                INTEGER NOT NULL CHECK (tokens >= 0),
        cost_basis_lamports   INTEGER NOT NULL CHECK (cost_basis_lamports >= 0),
        -- Null while the position is open. Zero is a real answer that means the
        -- sale returned nothing, which is not the same fact.
        proceeds_lamports     INTEGER CHECK (proceeds_lamports IS NULL OR proceeds_lamports >= 0),
        -- Signed: a loss is the common case and the column has to be able to
        -- say so.
        realized_pnl_lamports INTEGER,
        fee_lamports          INTEGER NOT NULL DEFAULT 0 CHECK (fee_lamports >= 0),
        tip_lamports          INTEGER NOT NULL DEFAULT 0 CHECK (tip_lamports >= 0),
        slippage_bps          INTEGER CHECK (slippage_bps IS NULL
                                        OR (slippage_bps >= 0 AND slippage_bps <= 10000)),

        opened_at_ms          INTEGER NOT NULL,
        closed_at_ms          INTEGER CHECK (closed_at_ms IS NULL OR closed_at_ms >= opened_at_ms),

        -- Profit with no proceeds is not a number anybody computed. The same
        -- check `intent_transitions` carries, for the same reason.
        CHECK (realized_pnl_lamports IS NULL OR proceeds_lamports IS NOT NULL),
        -- A closed trade has proceeds and an open one does not.
        CHECK ((closed_at_ms IS NULL AND proceeds_lamports IS NULL)
            OR (closed_at_ms IS NOT NULL AND proceeds_lamports IS NOT NULL))
    );

    CREATE INDEX IF NOT EXISTS journal_trades_mode
        ON journal_trades (mode, opened_at_ms DESC);
    CREATE INDEX IF NOT EXISTS journal_trades_mint
        ON journal_trades (mint, opened_at_ms DESC);
    CREATE INDEX IF NOT EXISTS journal_trades_venue
        ON journal_trades (venue, opened_at_ms DESC)
        WHERE venue IS NOT NULL;
    CREATE INDEX IF NOT EXISTS journal_trades_closed
        ON journal_trades (mode, closed_at_ms DESC)
        WHERE closed_at_ms IS NOT NULL;
    CREATE INDEX IF NOT EXISTS journal_trades_open
        ON journal_trades (mode, opened_at_ms DESC)
        WHERE closed_at_ms IS NULL;
    CREATE INDEX IF NOT EXISTS journal_trades_slippage
        ON journal_trades (slippage_bps DESC, opened_at_ms DESC)
        WHERE slippage_bps IS NOT NULL;

    -- What a trade *is* cannot change once it is written. The row is a summary
    -- and summaries get updated — proceeds arrive, a position closes — but the
    -- mint, the side, the mode and when it opened are its identity, and an
    -- update that moved one of those would silently rewrite history under
    -- whoever was reading it. A trigger rather than a Rust check because the
    -- guarantee has to hold for every writer, including a person at a shell.
    CREATE TRIGGER IF NOT EXISTS journal_trades_identity_is_immutable
        BEFORE UPDATE ON journal_trades
        WHEN old.mint <> new.mint
          OR old.side <> new.side
          OR old.mode <> new.mode
          OR old.opened_at_ms <> new.opened_at_ms
    BEGIN
        SELECT RAISE(ABORT, 'a journal trade cannot change what it is');
    END;

    CREATE TABLE IF NOT EXISTS journal_fills (
        trade_id      TEXT    NOT NULL REFERENCES journal_trades (trade_id) ON DELETE CASCADE,
        seq           INTEGER NOT NULL CHECK (seq >= 0),

        tokens        INTEGER NOT NULL CHECK (tokens > 0),
        lamports      INTEGER NOT NULL CHECK (lamports >= 0),
        fee_lamports  INTEGER NOT NULL CHECK (fee_lamports >= 0),
        -- The two above, divided, floored to 10^-18. A derived key to sort and
        -- filter on; the pair is the record.
        price_q18     INTEGER NOT NULL CHECK (price_q18 >= 0),
        -- What the route said this would be worth, same unit.
        quoted_q18    INTEGER NOT NULL CHECK (quoted_q18 >= 0),
        slippage_bps  INTEGER NOT NULL CHECK (slippage_bps >= 0 AND slippage_bps <= 10000),
        slot          INTEGER NOT NULL CHECK (slot >= 0),
        at_ms         INTEGER NOT NULL,

        PRIMARY KEY (trade_id, seq)
    ) WITHOUT ROWID;

    CREATE INDEX IF NOT EXISTS journal_fills_at
        ON journal_fills (at_ms DESC);
    CREATE INDEX IF NOT EXISTS journal_fills_slippage
        ON journal_fills (slippage_bps DESC, at_ms DESC);

    CREATE TABLE IF NOT EXISTS journal_routes (
        trade_id            TEXT    NOT NULL REFERENCES journal_trades (trade_id) ON DELETE CASCADE,
        seq                 INTEGER NOT NULL CHECK (seq >= 0),

        venue               TEXT    NOT NULL CHECK (venue IN (
                                      'pump_fun_curve', 'raydium_amm_v4')),
        chosen              INTEGER NOT NULL CHECK (chosen IN (0, 1)),
        tokens              INTEGER NOT NULL CHECK (tokens > 0),
        quoted_out_lamports INTEGER NOT NULL CHECK (quoted_out_lamports >= 0),
        min_out_lamports    INTEGER NOT NULL CHECK (min_out_lamports >= 0),
        max_slippage_bps    INTEGER NOT NULL CHECK (max_slippage_bps >= 0
                                      AND max_slippage_bps <= 10000),
        -- The sentence saying why this path lost. Required on a path that was
        -- not taken, forbidden on the one that was: a rejection with no reason
        -- and a reason on the chosen route are both rows that cannot be read
        -- back honestly.
        rejected_because    TEXT,
        simulated_at_ms     INTEGER NOT NULL,
        at_ms               INTEGER NOT NULL,

        PRIMARY KEY (trade_id, seq),

        CHECK ((chosen = 1 AND rejected_because IS NULL)
            OR (chosen = 0 AND rejected_because IS NOT NULL)),
        -- A floor above the quote is a route that could never fill.
        CHECK (min_out_lamports <= quoted_out_lamports)
    ) WITHOUT ROWID;

    -- One trade goes one way. Two chosen routes would mean the book cannot say
    -- which liquidity the money actually went through.
    CREATE UNIQUE INDEX IF NOT EXISTS journal_routes_chosen
        ON journal_routes (trade_id)
        WHERE chosen = 1;
    CREATE INDEX IF NOT EXISTS journal_routes_venue
        ON journal_routes (venue, at_ms DESC);

    CREATE TABLE IF NOT EXISTS journal_tips (
        trade_id         TEXT    NOT NULL REFERENCES journal_trades (trade_id) ON DELETE CASCADE,
        -- The retry index the bid was priced for. Zero on a first attempt, which
        -- is why this and not a separate sequence: a rebroadcast that re-bid the
        -- same attempt is the same row, not a second one.
        attempt          INTEGER NOT NULL CHECK (attempt >= 0),

        account          TEXT    NOT NULL,
        lamports         INTEGER NOT NULL CHECK (lamports >= 0),
        stance           TEXT    NOT NULL CHECK (stance IN ('emergency', 'discretionary')),
        -- What the bid was a share of. Null where nothing was computed, which
        -- is every emergency exit: Annex C.2 does not apply an EV test to one.
        ev_net_lamports  INTEGER,
        -- `Tip_max` at the time. Kept per row rather than read from config,
        -- because the question a month later is whether the bid was inside the
        -- ceiling *then*.
        ceiling_lamports INTEGER NOT NULL CHECK (ceiling_lamports >= 0),
        at_ms            INTEGER NOT NULL,

        PRIMARY KEY (trade_id, attempt)
    ) WITHOUT ROWID;

    CREATE INDEX IF NOT EXISTS journal_tips_at
        ON journal_tips (at_ms DESC);
    -- The overruns, which is the only reason anybody scans this table whole.
    CREATE INDEX IF NOT EXISTS journal_tips_over_ceiling
        ON journal_tips (at_ms DESC)
        WHERE lamports > ceiling_lamports;

    CREATE TABLE IF NOT EXISTS journal_signatures (
        -- The signature is the key. It is unique on chain, so it is unique
        -- here, and the partial unique indexes the two ledgers need to say the
        -- same thing are unnecessary in a table that is keyed by it.
        signature    TEXT    PRIMARY KEY,
        trade_id     TEXT    NOT NULL REFERENCES journal_trades (trade_id) ON DELETE CASCADE,

        kind         TEXT    NOT NULL CHECK (kind IN ('entry', 'exit')),
        status       TEXT    NOT NULL CHECK (status IN (
                               'broadcast', 'confirmed', 'dropped', 'expired', 'failed')),
        slot         INTEGER CHECK (slot IS NULL OR slot >= 0),
        rebroadcasts INTEGER NOT NULL DEFAULT 0 CHECK (rebroadcasts >= 0),
        at_ms        INTEGER NOT NULL,

        -- A slot is what a node assigned when it landed. Nothing that never
        -- landed has one, and a zero there would read as slot zero.
        CHECK (slot IS NULL OR status = 'confirmed')
    );

    CREATE INDEX IF NOT EXISTS journal_signatures_trade
        ON journal_signatures (trade_id, at_ms DESC);
    CREATE INDEX IF NOT EXISTS journal_signatures_status
        ON journal_signatures (status, at_ms DESC);
    -- Sent and not yet settled: money whose fate is decided and not yet known.
    CREATE INDEX IF NOT EXISTS journal_signatures_in_flight
        ON journal_signatures (at_ms DESC)
        WHERE status = 'broadcast';
";

// ---------------------------------------------------------------------------
// the vocabulary the columns are checked against
// ---------------------------------------------------------------------------

/// Which side of a position a signature belongs to.
///
/// Two variants and not three: the tip rides inside the exit transaction as its
/// last instruction and shares that transaction's signature, so a third `tip`
/// kind would be a second name for a row that already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SignatureKind {
    Entry,
    Exit,
}

impl SignatureKind {
    pub const ALL: [SignatureKind; 2] = [SignatureKind::Entry, SignatureKind::Exit];

    pub const fn as_str(self) -> &'static str {
        match self {
            SignatureKind::Entry => "entry",
            SignatureKind::Exit => "exit",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        SignatureKind::ALL.into_iter().find(|k| k.as_str() == text)
    }
}

/// Where a transaction ended up.
///
/// `Dropped` and `Expired` are kept apart for the reason `ConfirmOutcome` keeps
/// them apart: one means a node took it and lost it, the other means its
/// blockhash aged out before anything did. Flattened together they could not
/// say afterwards whether the network was slow or the send was late.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SignatureStatus {
    /// On the network, nothing back yet.
    Broadcast,
    Confirmed,
    /// A node accepted it and it never landed.
    Dropped,
    /// The blockhash aged out.
    Expired,
    /// The runtime rejected it.
    Failed,
}

impl SignatureStatus {
    pub const ALL: [SignatureStatus; 5] = [
        SignatureStatus::Broadcast,
        SignatureStatus::Confirmed,
        SignatureStatus::Dropped,
        SignatureStatus::Expired,
        SignatureStatus::Failed,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            SignatureStatus::Broadcast => "broadcast",
            SignatureStatus::Confirmed => "confirmed",
            SignatureStatus::Dropped => "dropped",
            SignatureStatus::Expired => "expired",
            SignatureStatus::Failed => "failed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        SignatureStatus::ALL
            .into_iter()
            .find(|s| s.as_str() == text)
    }

    /// Whether there is still money on the network under this.
    pub const fn is_in_flight(self) -> bool {
        matches!(self, SignatureStatus::Broadcast)
    }
}

/// What the router did with one path it looked at.
///
/// An enum rather than a `bool` and a `String` beside it, because the two of
/// them disagreeing — a chosen route carrying a rejection, a rejected one
/// carrying none — is the row the table's `CHECK` refuses, and a type that
/// cannot spell it is better than a constraint that catches it at the insert.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "camelCase")]
pub enum RouteDecision {
    /// The path the money went through. At most one per trade, and the unique
    /// partial index is what makes "at most" true.
    Chosen,
    /// A path that was priced and passed over, and why.
    Rejected { because: String },
}

impl RouteDecision {
    pub const fn was_chosen(&self) -> bool {
        matches!(self, RouteDecision::Chosen)
    }

    /// The sentence, or `None` on the chosen route.
    pub fn because(&self) -> Option<&str> {
        match self {
            RouteDecision::Chosen => None,
            RouteDecision::Rejected { because } => Some(because.as_str()),
        }
    }
}

// ---------------------------------------------------------------------------
// the rows
// ---------------------------------------------------------------------------

/// One trade, and what it came to.
///
/// The summary row. It is written when the trade opens and written again when
/// it closes, and the trigger on the table is what stops the second write
/// changing what the first one said it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeRow {
    /// The intent id.
    pub trade_id: String,
    pub mint: String,
    pub side: Side,
    pub mode: ExecutionMode,
    /// `None` until a route is chosen.
    pub venue: Option<Venue>,
    /// What went out, in lamports.
    pub notional_lamports: u64,
    /// The position, in token base units.
    pub tokens: u64,
    pub cost_basis_lamports: u64,
    /// What came back. `None` while the position is open.
    pub proceeds_lamports: Option<u64>,
    /// Proceeds less cost less fees less tips. Signed, because most of them are
    /// negative.
    pub realized_pnl_lamports: Option<i64>,
    pub fee_lamports: u64,
    pub tip_lamports: u64,
    /// The worst fill this trade took, in basis points. `None` before the first
    /// fill.
    pub slippage_bps: Option<u16>,
    pub opened_at_ms: i64,
    pub closed_at_ms: Option<i64>,
}

impl TradeRow {
    /// A trade that has opened and not closed.
    pub fn opened(
        trade_id: impl Into<String>,
        mint: impl Into<String>,
        side: Side,
        mode: ExecutionMode,
        notional_lamports: u64,
        opened_at_ms: i64,
    ) -> Self {
        TradeRow {
            trade_id: trade_id.into(),
            mint: mint.into(),
            side,
            mode,
            venue: None,
            notional_lamports,
            tokens: 0,
            cost_basis_lamports: notional_lamports,
            proceeds_lamports: None,
            realized_pnl_lamports: None,
            fee_lamports: 0,
            tip_lamports: 0,
            slippage_bps: None,
            opened_at_ms,
            closed_at_ms: None,
        }
    }

    /// The same trade, closed at these proceeds.
    ///
    /// The profit is computed here rather than taken, so the column and the
    /// three it is derived from cannot disagree. Fees and tips come off it:
    /// what a person wants from this row is what the trade put in their wallet,
    /// and a tip paid to land the exit is as much a cost of the trade as the
    /// venue's fee is.
    pub fn closed_at(mut self, proceeds_lamports: u64, closed_at_ms: i64) -> Self {
        let out = i128::from(proceeds_lamports);
        let cost = i128::from(self.cost_basis_lamports)
            + i128::from(self.fee_lamports)
            + i128::from(self.tip_lamports);
        self.proceeds_lamports = Some(proceeds_lamports);
        // Every term is bounded by the supply of SOL in lamports, so the
        // difference is nowhere near an `i64`; the clamp is here because a
        // silent wrap in the profit column is the one bug this file exists to
        // make impossible.
        self.realized_pnl_lamports =
            Some((out - cost).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64);
        self.closed_at_ms = Some(closed_at_ms.max(self.opened_at_ms));
        self
    }

    /// Whether there is still a position under this.
    pub const fn is_open(&self) -> bool {
        self.closed_at_ms.is_none()
    }
}

/// One fill, at the price it actually got.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FillRow {
    pub trade_id: String,
    pub seq: u32,
    /// What moved, in token base units. Never zero — a fill of nothing has no
    /// price, which is why [`FillRow::settle`] refuses to build one.
    pub tokens: u64,
    /// What it came to, in lamports, net of the venue's fee.
    pub lamports: u64,
    pub fee_lamports: u64,
    /// `lamports / tokens`, floored to `10^-18`. Derived, never supplied.
    pub price: Q18,
    /// What the route quoted for the same tokens, same unit. Derived.
    pub quoted: Q18,
    /// How far the price came in under the quote. Derived, and floored, so a
    /// fill is never reported as better than it was.
    pub slippage_bps: u16,
    pub slot: u64,
    pub at_ms: i64,
}

impl FillRow {
    /// Builds a fill from the integers it happened in.
    ///
    /// The price, the quote and the slippage are all computed here and none of
    /// them can be passed in. That is the point: three numbers that have to
    /// agree, assigned from one place, is the same argument `AbortOutcome`
    /// makes about its three.
    ///
    /// `None` on zero tokens, which is not a fill, and on a price too large to
    /// hold — see [`Q18::to_i64_raw`] for how far away that is.
    #[allow(clippy::too_many_arguments)]
    pub fn settle(
        trade_id: impl Into<String>,
        seq: u32,
        tokens: u64,
        lamports: u64,
        fee_lamports: u64,
        quoted_lamports: u64,
        slot: u64,
        at_ms: i64,
    ) -> Option<Self> {
        if tokens == 0 {
            return None;
        }
        let denominator = u128::from(tokens);
        let price = Q18::ratio_floor(u128::from(lamports), denominator)?;
        let quoted = Q18::ratio_floor(u128::from(quoted_lamports), denominator)?;
        price.to_i64_raw()?;
        quoted.to_i64_raw()?;

        Some(FillRow {
            trade_id: trade_id.into(),
            seq,
            tokens,
            lamports,
            fee_lamports,
            price,
            quoted,
            // The bound is basis points in a `u16` everywhere else in the
            // codebase, and ten thousand of them is all of it.
            slippage_bps: price.shortfall_bps_floor(quoted).min(10_000) as u16,
            slot,
            at_ms,
        })
    }
}

/// One path the router priced, taken or not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteRow {
    pub trade_id: String,
    pub seq: u32,
    pub venue: Venue,
    #[serde(flatten)]
    pub decision: RouteDecision,
    pub tokens: u64,
    pub quoted_out_lamports: u64,
    /// The floor written into the instruction.
    pub min_out_lamports: u64,
    pub max_slippage_bps: u16,
    /// When the reserves this was priced against were read. A route older than
    /// the policy window is re-resolved rather than sent, and this is what says
    /// how old it was.
    pub simulated_at_ms: i64,
    pub at_ms: i64,
}

/// One tip bid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TipRow {
    pub trade_id: String,
    /// The retry index this was priced for.
    pub attempt: u32,
    /// Base58, as a person would paste it into an explorer.
    pub account: String,
    pub lamports: u64,
    pub stance: TipStance,
    pub ev_net_lamports: Option<i64>,
    /// `Tip_max` as it stood when the bid was made.
    pub ceiling_lamports: u64,
    pub at_ms: i64,
}

impl TipRow {
    /// The row for a bid the tip policy priced.
    pub fn from_bid(
        trade_id: impl Into<String>,
        bid: &TipBid,
        stance: TipStance,
        ceiling_lamports: u64,
        at_ms: i64,
    ) -> Self {
        TipRow {
            trade_id: trade_id.into(),
            attempt: bid.attempt,
            account: bid.account.to_string(),
            lamports: bid.lamports,
            stance,
            ev_net_lamports: bid.ev_net_lamports,
            ceiling_lamports,
            at_ms,
        }
    }

    /// Whether this bid went past the ceiling it was priced under.
    ///
    /// It should never be true — `TipPolicy` caps every bid — and it is
    /// recorded rather than asserted because the day it is true is the day
    /// somebody needs to be able to see it, not the day the process should
    /// panic in the middle of an exit.
    pub const fn is_over_ceiling(&self) -> bool {
        self.lamports > self.ceiling_lamports
    }
}

/// One transaction and where it ended up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureRow {
    /// Base58.
    pub signature: String,
    pub trade_id: String,
    pub kind: SignatureKind,
    pub status: SignatureStatus,
    /// Where it landed. `None` on anything that did not.
    pub slot: Option<u64>,
    /// How many times the same bytes went out again.
    pub rebroadcasts: u32,
    pub at_ms: i64,
}

impl SignatureRow {
    /// A transaction that has just gone out.
    pub fn broadcast(
        signature: impl Into<String>,
        trade_id: impl Into<String>,
        kind: SignatureKind,
        at_ms: i64,
    ) -> Self {
        SignatureRow {
            signature: signature.into(),
            trade_id: trade_id.into(),
            kind,
            status: SignatureStatus::Broadcast,
            slot: None,
            rebroadcasts: 0,
            at_ms,
        }
    }

    /// The same transaction, landed.
    pub fn confirmed_in(mut self, slot: u64, at_ms: i64) -> Self {
        self.status = SignatureStatus::Confirmed;
        self.slot = Some(slot);
        self.at_ms = at_ms;
        self
    }

    /// The same transaction, settled without landing. The slot is dropped
    /// rather than kept: the table refuses one on anything but a confirmation,
    /// because a slot on a dropped transaction reads as a landing.
    pub fn settled_as(mut self, status: SignatureStatus, at_ms: i64) -> Self {
        self.status = status;
        if status != SignatureStatus::Confirmed {
            self.slot = None;
        }
        self.at_ms = at_ms;
        self
    }
}

/// One trade with everything that decided it.
///
/// What `trade_detail` returns and what a journal row expands into when
/// somebody clicks it. The children are in the order they were sequenced, which
/// for a replay is the order they happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeDetail {
    pub trade: TradeRow,
    pub fills: Vec<FillRow>,
    pub routes: Vec<RouteRow>,
    pub tips: Vec<TipRow>,
    pub signatures: Vec<SignatureRow>,
}

// ---------------------------------------------------------------------------
// asking the book a question
// ---------------------------------------------------------------------------

/// How many trades one page holds when the caller does not say.
pub const DEFAULT_LIMIT: u32 = 200;

/// The most any one query will return, whatever it asks for.
///
/// A window that asks for everything and gets a hundred thousand rows has not
/// asked a question, it has copied the database into a JSON array and sent it
/// down an IPC channel. The cap is here rather than in the UI because the UI is
/// not the only caller.
pub const MAX_LIMIT: u32 = 5_000;

/// What to filter the book by.
///
/// Every field is optional and `None` means "do not filter on this", so the
/// default is the whole book newest-first. `Deserialize` with `default` on the
/// struct, because the window sends the two or three fields the operator
/// actually set and nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JournalFilter {
    pub mode: Option<ExecutionMode>,
    pub mint: Option<String>,
    pub venue: Option<Venue>,
    pub side: Option<Side>,
    /// Inclusive. Against `opened_at_ms`, which is the column the indexes are
    /// ordered by.
    pub since_ms: Option<i64>,
    /// Inclusive.
    pub until_ms: Option<i64>,
    /// At or above. The filter a person reaches for after a bad hour.
    pub min_slippage_bps: Option<u16>,
    /// At or below. Negative, to list the losses.
    pub max_realized_pnl_lamports: Option<i64>,
    pub min_realized_pnl_lamports: Option<i64>,
    /// Only trades that have closed. False lists both, and there is
    /// deliberately no "only open": that is `min_realized_pnl` unset and this
    /// unset, which is the default, and a third boolean that can contradict
    /// this one is not worth the row it would mis-select.
    pub only_closed: bool,
    /// Clamped to [`MAX_LIMIT`]. Zero means the default rather than nothing,
    /// because a caller that sent no limit and a caller that sent zero are the
    /// same caller with a different JSON serialiser.
    pub limit: u32,
    pub offset: u32,
}

impl Default for JournalFilter {
    fn default() -> Self {
        JournalFilter {
            mode: None,
            mint: None,
            venue: None,
            side: None,
            since_ms: None,
            until_ms: None,
            min_slippage_bps: None,
            max_realized_pnl_lamports: None,
            min_realized_pnl_lamports: None,
            only_closed: false,
            limit: DEFAULT_LIMIT,
            offset: 0,
        }
    }
}

impl JournalFilter {
    /// Everything, in one mode.
    pub fn in_mode(mode: ExecutionMode) -> Self {
        JournalFilter {
            mode: Some(mode),
            ..JournalFilter::default()
        }
    }

    /// The page size this actually asks for.
    pub fn effective_limit(&self) -> u32 {
        if self.limit == 0 {
            DEFAULT_LIMIT
        } else {
            self.limit.min(MAX_LIMIT)
        }
    }

    /// The `WHERE` this filter means, and the parameters to bind to it.
    ///
    /// One `String` per query and none per row. The fragments are static and
    /// the values are bound, so nothing a caller types reaches the parser —
    /// `mint` in particular arrives from whatever the window put in a text box.
    fn where_clause(&self) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
        let mut clauses: Vec<&'static str> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(mode) = self.mode {
            clauses.push("mode = ?");
            params.push(Box::new(mode.as_str()));
        }
        if let Some(mint) = &self.mint {
            clauses.push("mint = ?");
            params.push(Box::new(mint.clone()));
        }
        if let Some(venue) = self.venue {
            clauses.push("venue = ?");
            params.push(Box::new(venue.as_str()));
        }
        if let Some(side) = self.side {
            clauses.push("side = ?");
            params.push(Box::new(side.as_str()));
        }
        if let Some(since) = self.since_ms {
            clauses.push("opened_at_ms >= ?");
            params.push(Box::new(since));
        }
        if let Some(until) = self.until_ms {
            clauses.push("opened_at_ms <= ?");
            params.push(Box::new(until));
        }
        if let Some(slippage) = self.min_slippage_bps {
            clauses.push("slippage_bps >= ?");
            params.push(Box::new(i64::from(slippage)));
        }
        if let Some(max) = self.max_realized_pnl_lamports {
            clauses.push("realized_pnl_lamports <= ?");
            params.push(Box::new(max));
        }
        if let Some(min) = self.min_realized_pnl_lamports {
            clauses.push("realized_pnl_lamports >= ?");
            params.push(Box::new(min));
        }
        if self.only_closed {
            clauses.push("closed_at_ms IS NOT NULL");
        }

        if clauses.is_empty() {
            return (String::new(), params);
        }
        (format!(" WHERE {}", clauses.join(" AND ")), params)
    }
}

/// What a filtered slice of the book adds up to.
///
/// Computed in SQL over integer columns, so the sum of a hundred thousand
/// trades is one statement and is exact. Every field is a count or a lamport
/// total; nothing here is a ratio, because a ratio computed over a filtered set
/// is a number whose denominator the caller has to be told about, and the
/// caller already has both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalTotals {
    pub trades: i64,
    pub closed: i64,
    pub notional_lamports: i64,
    pub cost_basis_lamports: i64,
    pub proceeds_lamports: i64,
    pub realized_pnl_lamports: i64,
    pub fee_lamports: i64,
    pub tip_lamports: i64,
    /// The worst single trade in the slice. `None` when nothing in it has
    /// filled — which is not the same as zero slippage, and is why this is an
    /// `Option` rather than a `COALESCE` to nought.
    pub worst_slippage_bps: Option<u16>,
}

// ---------------------------------------------------------------------------
// writing and reading
// ---------------------------------------------------------------------------

/// Lamports and token counts on the way into an `INTEGER` column.
///
/// Nothing in this build can reach the ceiling — the whole supply of SOL is
/// about `6 x 10^17` lamports against an `i64`'s `9.2 x 10^18` — so this is a
/// guard against a caller that computed a number rather than against the
/// chain. It errors rather than saturating for the reason [`Q18::to_i64_raw`]
/// does: a clamped quantity in the book is a lie in the column the book exists
/// to be trusted about.
fn store_u64(value: u64, column: &str) -> Result<i64, EngineError> {
    i64::try_from(value).map_err(|_| {
        EngineError::Database(format!(
            "{column} is {value}, which is past what a column holds"
        ))
    })
}

/// The same on the way back out. Unreachable through a `CHECK`-guarded column,
/// and checked anyway, because a file touched by hand is the case these
/// conversions exist for.
fn load_u64(value: i64, column: &str) -> Result<u64, EngineError> {
    u64::try_from(value)
        .map_err(|_| EngineError::Database(format!("{column} holds {value}, which is negative")))
}

fn load_u32(value: i64, column: &str) -> Result<u32, EngineError> {
    u32::try_from(value)
        .map_err(|_| EngineError::Database(format!("{column} holds {value}, which does not fit")))
}

fn load_u16(value: i64, column: &str) -> Result<u16, EngineError> {
    u16::try_from(value)
        .map_err(|_| EngineError::Database(format!("{column} holds {value}, which does not fit")))
}

/// Turns a column's text back into the enum that wrote it. The sibling of
/// `db.rs`'s `stored_as`, kept here because that one is private to its module
/// and the argument for it is the same on both sides.
fn stored_as<T>(
    text: &str,
    from: impl Fn(&str) -> Option<T>,
    column: &str,
) -> Result<T, EngineError> {
    from(text).ok_or_else(|| {
        EngineError::Database(format!(
            "{column} holds {text:?}, which this build does not know"
        ))
    })
}

fn read_price(row: &Row<'_>, index: usize, column: &str) -> Result<Q18, EngineError> {
    let raw: i64 = row.get(index)?;
    Q18::from_i64_raw(raw)
        .ok_or_else(|| EngineError::Database(format!("{column} holds {raw}, which is negative")))
}

impl Database {
    /// Writes trades, opening or updating them.
    ///
    /// `ON CONFLICT DO UPDATE`, so recording the same trade twice is how it
    /// closes rather than a duplicate-key error.
    ///
    /// The identity columns are in the `SET` list too, and that is deliberate
    /// rather than an oversight. Leaving them out would make a write that
    /// changed the mint a write that quietly kept the old one — the wrong row
    /// updated and nobody told — and it would also mean the table's trigger
    /// could never fire from this path, because a column that is never assigned
    /// never differs from itself. Assigning them turns the trigger into the
    /// check it is written as: the same identity assigns itself and passes, a
    /// different one aborts the statement.
    ///
    /// One transaction for the batch: a thousand fills arriving from one tick
    /// are one commit and one `fsync`, not a thousand.
    pub fn record_journal_trades(&self, rows: &[TradeRow]) -> Result<usize, EngineError> {
        if rows.is_empty() {
            return Ok(0);
        }

        let mut conn = self.connection();
        let transaction = conn.transaction()?;
        let mut written = 0usize;
        {
            let mut statement = transaction.prepare_cached(
                "INSERT INTO journal_trades (
                     trade_id, mint, side, mode, venue, notional_lamports, tokens,
                     cost_basis_lamports, proceeds_lamports, realized_pnl_lamports,
                     fee_lamports, tip_lamports, slippage_bps, opened_at_ms, closed_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                 ON CONFLICT (trade_id) DO UPDATE SET
                     mint                  = excluded.mint,
                     side                  = excluded.side,
                     mode                  = excluded.mode,
                     opened_at_ms          = excluded.opened_at_ms,
                     venue                 = excluded.venue,
                     notional_lamports     = excluded.notional_lamports,
                     tokens                = excluded.tokens,
                     cost_basis_lamports   = excluded.cost_basis_lamports,
                     proceeds_lamports     = excluded.proceeds_lamports,
                     realized_pnl_lamports = excluded.realized_pnl_lamports,
                     fee_lamports          = excluded.fee_lamports,
                     tip_lamports          = excluded.tip_lamports,
                     slippage_bps          = excluded.slippage_bps,
                     closed_at_ms          = excluded.closed_at_ms",
            )?;
            for row in rows {
                written += statement.execute(rusqlite::params![
                    row.trade_id,
                    row.mint,
                    row.side.as_str(),
                    row.mode.as_str(),
                    row.venue.map(Venue::as_str),
                    store_u64(row.notional_lamports, "notional_lamports")?,
                    store_u64(row.tokens, "tokens")?,
                    store_u64(row.cost_basis_lamports, "cost_basis_lamports")?,
                    row.proceeds_lamports
                        .map(|p| store_u64(p, "proceeds_lamports"))
                        .transpose()?,
                    row.realized_pnl_lamports,
                    store_u64(row.fee_lamports, "fee_lamports")?,
                    store_u64(row.tip_lamports, "tip_lamports")?,
                    row.slippage_bps.map(i64::from),
                    row.opened_at_ms,
                    row.closed_at_ms,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(written)
    }

    /// Writes fills. `DO NOTHING` on a repeat, because `(trade_id, seq)` names
    /// one fill and a replay writing it twice is a replay, not a second fill.
    pub fn record_journal_fills(&self, rows: &[FillRow]) -> Result<usize, EngineError> {
        if rows.is_empty() {
            return Ok(0);
        }

        let mut conn = self.connection();
        let transaction = conn.transaction()?;
        let mut written = 0usize;
        {
            let mut statement = transaction.prepare_cached(
                "INSERT INTO journal_fills (
                     trade_id, seq, tokens, lamports, fee_lamports, price_q18, quoted_q18,
                     slippage_bps, slot, at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT (trade_id, seq) DO NOTHING",
            )?;
            for row in rows {
                let price = row.price.to_i64_raw().ok_or_else(|| {
                    EngineError::Database(format!(
                        "the price of {}#{} is past what a column holds",
                        row.trade_id, row.seq
                    ))
                })?;
                let quoted = row.quoted.to_i64_raw().ok_or_else(|| {
                    EngineError::Database(format!(
                        "the quote of {}#{} is past what a column holds",
                        row.trade_id, row.seq
                    ))
                })?;
                written += statement.execute(rusqlite::params![
                    row.trade_id,
                    row.seq,
                    store_u64(row.tokens, "tokens")?,
                    store_u64(row.lamports, "lamports")?,
                    store_u64(row.fee_lamports, "fee_lamports")?,
                    price,
                    quoted,
                    i64::from(row.slippage_bps),
                    store_u64(row.slot, "slot")?,
                    row.at_ms,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(written)
    }

    /// Writes routing decisions.
    pub fn record_journal_routes(&self, rows: &[RouteRow]) -> Result<usize, EngineError> {
        if rows.is_empty() {
            return Ok(0);
        }

        let mut conn = self.connection();
        let transaction = conn.transaction()?;
        let mut written = 0usize;
        {
            let mut statement = transaction.prepare_cached(
                "INSERT INTO journal_routes (
                     trade_id, seq, venue, chosen, tokens, quoted_out_lamports,
                     min_out_lamports, max_slippage_bps, rejected_because, simulated_at_ms, at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT (trade_id, seq) DO NOTHING",
            )?;
            for row in rows {
                written += statement.execute(rusqlite::params![
                    row.trade_id,
                    row.seq,
                    row.venue.as_str(),
                    i64::from(row.decision.was_chosen()),
                    store_u64(row.tokens, "tokens")?,
                    store_u64(row.quoted_out_lamports, "quoted_out_lamports")?,
                    store_u64(row.min_out_lamports, "min_out_lamports")?,
                    i64::from(row.max_slippage_bps),
                    row.decision.because(),
                    row.simulated_at_ms,
                    row.at_ms,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(written)
    }

    /// Writes tip bids.
    pub fn record_journal_tips(&self, rows: &[TipRow]) -> Result<usize, EngineError> {
        if rows.is_empty() {
            return Ok(0);
        }

        let mut conn = self.connection();
        let transaction = conn.transaction()?;
        let mut written = 0usize;
        {
            let mut statement = transaction.prepare_cached(
                "INSERT INTO journal_tips (
                     trade_id, attempt, account, lamports, stance, ev_net_lamports,
                     ceiling_lamports, at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (trade_id, attempt) DO NOTHING",
            )?;
            for row in rows {
                written += statement.execute(rusqlite::params![
                    row.trade_id,
                    row.attempt,
                    row.account,
                    store_u64(row.lamports, "lamports")?,
                    row.stance.as_str(),
                    row.ev_net_lamports,
                    store_u64(row.ceiling_lamports, "ceiling_lamports")?,
                    row.at_ms,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(written)
    }

    /// Writes signatures, or moves ones already written to their new status.
    ///
    /// `DO UPDATE` rather than `DO NOTHING`, unlike the three above: a
    /// signature is written once when it goes out and again when it settles,
    /// and those are the same transaction at two moments rather than two
    /// transactions. The `trade_id` and the `kind` are not in the `SET` list,
    /// for the reason the trade trigger exists.
    pub fn record_journal_signatures(&self, rows: &[SignatureRow]) -> Result<usize, EngineError> {
        if rows.is_empty() {
            return Ok(0);
        }

        let mut conn = self.connection();
        let transaction = conn.transaction()?;
        let mut written = 0usize;
        {
            let mut statement = transaction.prepare_cached(
                "INSERT INTO journal_signatures (
                     signature, trade_id, kind, status, slot, rebroadcasts, at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (signature) DO UPDATE SET
                     status       = excluded.status,
                     slot         = excluded.slot,
                     rebroadcasts = excluded.rebroadcasts,
                     at_ms        = excluded.at_ms",
            )?;
            for row in rows {
                written += statement.execute(rusqlite::params![
                    row.signature,
                    row.trade_id,
                    row.kind.as_str(),
                    row.status.as_str(),
                    row.slot.map(|s| store_u64(s, "slot")).transpose()?,
                    row.rebroadcasts,
                    row.at_ms,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(written)
    }

    /// The book, filtered, newest first.
    pub fn query_journal(&self, filter: &JournalFilter) -> Result<Vec<TradeRow>, EngineError> {
        let (where_clause, params) = filter.where_clause();
        let sql = format!(
            "SELECT trade_id, mint, side, mode, venue, notional_lamports, tokens,
                    cost_basis_lamports, proceeds_lamports, realized_pnl_lamports,
                    fee_lamports, tip_lamports, slippage_bps, opened_at_ms, closed_at_ms
               FROM journal_trades{where_clause}
              ORDER BY opened_at_ms DESC, trade_id
              LIMIT ?{limit} OFFSET ?{offset}",
            limit = params.len() + 1,
            offset = params.len() + 2,
        );

        let conn = self.connection();
        let mut statement = conn.prepare_cached(&sql)?;
        let mut bound: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let limit = i64::from(filter.effective_limit());
        let offset = i64::from(filter.offset);
        bound.push(&limit);
        bound.push(&offset);

        let mut trades = Vec::new();
        let mut rows = statement.query(bound.as_slice())?;
        while let Some(row) = rows.next()? {
            trades.push(read_trade(row)?);
        }
        Ok(trades)
    }

    /// What that slice adds up to. The same `WHERE`, so the totals are of the
    /// page's filter and not of the page — a caller looking at fifty of nine
    /// hundred losses wants the nine hundred.
    pub fn journal_totals(&self, filter: &JournalFilter) -> Result<JournalTotals, EngineError> {
        let (where_clause, params) = filter.where_clause();
        let sql = format!(
            "SELECT COUNT(*),
                    COUNT(closed_at_ms),
                    COALESCE(SUM(notional_lamports), 0),
                    COALESCE(SUM(cost_basis_lamports), 0),
                    COALESCE(SUM(proceeds_lamports), 0),
                    COALESCE(SUM(realized_pnl_lamports), 0),
                    COALESCE(SUM(fee_lamports), 0),
                    COALESCE(SUM(tip_lamports), 0),
                    MAX(slippage_bps)
               FROM journal_trades{where_clause}"
        );

        let conn = self.connection();
        let mut statement = conn.prepare_cached(&sql)?;
        let bound: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let totals = statement.query_row(bound.as_slice(), |row| {
            Ok(JournalTotals {
                trades: row.get(0)?,
                closed: row.get(1)?,
                notional_lamports: row.get(2)?,
                cost_basis_lamports: row.get(3)?,
                proceeds_lamports: row.get(4)?,
                realized_pnl_lamports: row.get(5)?,
                fee_lamports: row.get(6)?,
                tip_lamports: row.get(7)?,
                // `MAX` over no rows is NULL, which is the honest answer here
                // and the reason this column is not `COALESCE`d like the sums.
                worst_slippage_bps: row.get::<_, Option<i64>>(8)?.map(|bps| bps as u16),
            })
        })?;
        Ok(totals)
    }

    /// One trade with its fills, routes, tips and signatures.
    ///
    /// Five statements against one snapshot rather than one join with four
    /// left-outer arms: WAL gives the reader a consistent view from the moment
    /// its first statement started, so the five agree, and a join across four
    /// one-to-many children multiplies their rows together and has to be
    /// de-duplicated in Rust afterwards.
    pub fn journal_trade_detail(&self, trade_id: &str) -> Result<Option<TradeDetail>, EngineError> {
        let conn = self.connection();

        let trade = conn
            .prepare_cached(
                "SELECT trade_id, mint, side, mode, venue, notional_lamports, tokens,
                        cost_basis_lamports, proceeds_lamports, realized_pnl_lamports,
                        fee_lamports, tip_lamports, slippage_bps, opened_at_ms, closed_at_ms
                   FROM journal_trades WHERE trade_id = ?1",
            )?
            .query_row([trade_id], |row| Ok(read_trade(row)))
            .optional()?
            .transpose()?;
        let Some(trade) = trade else { return Ok(None) };

        let mut fills = Vec::new();
        {
            let mut statement = conn.prepare_cached(
                "SELECT trade_id, seq, tokens, lamports, fee_lamports, price_q18, quoted_q18,
                        slippage_bps, slot, at_ms
                   FROM journal_fills WHERE trade_id = ?1 ORDER BY seq",
            )?;
            let mut rows = statement.query([trade_id])?;
            while let Some(row) = rows.next()? {
                fills.push(read_fill(row)?);
            }
        }

        let mut routes = Vec::new();
        {
            let mut statement = conn.prepare_cached(
                "SELECT trade_id, seq, venue, chosen, tokens, quoted_out_lamports,
                        min_out_lamports, max_slippage_bps, rejected_because,
                        simulated_at_ms, at_ms
                   FROM journal_routes WHERE trade_id = ?1 ORDER BY seq",
            )?;
            let mut rows = statement.query([trade_id])?;
            while let Some(row) = rows.next()? {
                routes.push(read_route(row)?);
            }
        }

        let mut tips = Vec::new();
        {
            let mut statement = conn.prepare_cached(
                "SELECT trade_id, attempt, account, lamports, stance, ev_net_lamports,
                        ceiling_lamports, at_ms
                   FROM journal_tips WHERE trade_id = ?1 ORDER BY attempt",
            )?;
            let mut rows = statement.query([trade_id])?;
            while let Some(row) = rows.next()? {
                tips.push(read_tip(row)?);
            }
        }

        let mut signatures = Vec::new();
        {
            let mut statement = conn.prepare_cached(
                "SELECT signature, trade_id, kind, status, slot, rebroadcasts, at_ms
                   FROM journal_signatures WHERE trade_id = ?1 ORDER BY at_ms, signature",
            )?;
            let mut rows = statement.query([trade_id])?;
            while let Some(row) = rows.next()? {
                signatures.push(read_signature(row)?);
            }
        }

        Ok(Some(TradeDetail {
            trade,
            fills,
            routes,
            tips,
            signatures,
        }))
    }

    /// Every tip that went past its ceiling, newest first.
    ///
    /// Reads the partial index rather than scanning, which is why the index
    /// exists: the answer is almost always empty and asking must not cost a
    /// table scan to find that out.
    pub fn journal_tip_overruns(&self, limit: u32) -> Result<Vec<TipRow>, EngineError> {
        let conn = self.connection();
        let mut statement = conn.prepare_cached(
            "SELECT trade_id, attempt, account, lamports, stance, ev_net_lamports,
                    ceiling_lamports, at_ms
               FROM journal_tips
              WHERE lamports > ceiling_lamports
              ORDER BY at_ms DESC
              LIMIT ?1",
        )?;
        let mut overruns = Vec::new();
        let mut rows = statement.query([i64::from(limit.clamp(1, MAX_LIMIT))])?;
        while let Some(row) = rows.next()? {
            overruns.push(read_tip(row)?);
        }
        Ok(overruns)
    }

    /// How many transactions are on the network with nothing back yet, by mode.
    ///
    /// A `BTreeMap` rather than a `HashMap` so two runs list the modes in the
    /// same order, which is the rule the strategy module states and this file
    /// keeps for the same reason.
    pub fn journal_in_flight(&self) -> Result<BTreeMap<ExecutionMode, i64>, EngineError> {
        let conn = self.connection();
        let mut statement = conn.prepare_cached(
            "SELECT t.mode, COUNT(*)
               FROM journal_signatures s
               JOIN journal_trades t ON t.trade_id = s.trade_id
              WHERE s.status = 'broadcast'
              GROUP BY t.mode",
        )?;
        let mut counts = BTreeMap::new();
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let mode: String = row.get(0)?;
            let mode = stored_as(&mode, ExecutionMode::parse, "journal_trades.mode")?;
            counts.insert(mode, row.get::<_, i64>(1)?);
        }
        Ok(counts)
    }
}

// ---------------------------------------------------------------------------
// the readers
// ---------------------------------------------------------------------------

fn read_trade(row: &Row<'_>) -> Result<TradeRow, EngineError> {
    let side: String = row.get(2)?;
    let mode: String = row.get(3)?;
    let venue: Option<String> = row.get(4)?;
    Ok(TradeRow {
        trade_id: row.get(0)?,
        mint: row.get(1)?,
        side: stored_as(&side, Side::parse, "journal_trades.side")?,
        mode: stored_as(&mode, ExecutionMode::parse, "journal_trades.mode")?,
        venue: venue
            .map(|v| stored_as(&v, Venue::parse, "journal_trades.venue"))
            .transpose()?,
        notional_lamports: load_u64(row.get(5)?, "notional_lamports")?,
        tokens: load_u64(row.get(6)?, "tokens")?,
        cost_basis_lamports: load_u64(row.get(7)?, "cost_basis_lamports")?,
        proceeds_lamports: row
            .get::<_, Option<i64>>(8)?
            .map(|p| load_u64(p, "proceeds_lamports"))
            .transpose()?,
        realized_pnl_lamports: row.get(9)?,
        fee_lamports: load_u64(row.get(10)?, "fee_lamports")?,
        tip_lamports: load_u64(row.get(11)?, "tip_lamports")?,
        slippage_bps: row
            .get::<_, Option<i64>>(12)?
            .map(|bps| load_u16(bps, "slippage_bps"))
            .transpose()?,
        opened_at_ms: row.get(13)?,
        closed_at_ms: row.get(14)?,
    })
}

fn read_fill(row: &Row<'_>) -> Result<FillRow, EngineError> {
    Ok(FillRow {
        trade_id: row.get(0)?,
        seq: load_u32(row.get(1)?, "journal_fills.seq")?,
        tokens: load_u64(row.get(2)?, "journal_fills.tokens")?,
        lamports: load_u64(row.get(3)?, "journal_fills.lamports")?,
        fee_lamports: load_u64(row.get(4)?, "journal_fills.fee_lamports")?,
        price: read_price(row, 5, "journal_fills.price_q18")?,
        quoted: read_price(row, 6, "journal_fills.quoted_q18")?,
        slippage_bps: load_u16(row.get(7)?, "journal_fills.slippage_bps")?,
        slot: load_u64(row.get(8)?, "journal_fills.slot")?,
        at_ms: row.get(9)?,
    })
}

fn read_route(row: &Row<'_>) -> Result<RouteRow, EngineError> {
    let venue: String = row.get(2)?;
    let chosen: i64 = row.get(3)?;
    let because: Option<String> = row.get(8)?;
    // The table's `CHECK` makes exactly one of these two shapes storable, so
    // the mismatch below is only reachable on a file this build did not write.
    let decision = match (chosen != 0, because) {
        (true, None) => RouteDecision::Chosen,
        (false, Some(because)) => RouteDecision::Rejected { because },
        _ => {
            return Err(EngineError::Database(
                "a journal route is both chosen and rejected".to_string(),
            ))
        }
    };
    Ok(RouteRow {
        trade_id: row.get(0)?,
        seq: load_u32(row.get(1)?, "journal_routes.seq")?,
        venue: stored_as(&venue, Venue::parse, "journal_routes.venue")?,
        decision,
        tokens: load_u64(row.get(4)?, "journal_routes.tokens")?,
        quoted_out_lamports: load_u64(row.get(5)?, "journal_routes.quoted_out_lamports")?,
        min_out_lamports: load_u64(row.get(6)?, "journal_routes.min_out_lamports")?,
        max_slippage_bps: load_u16(row.get(7)?, "journal_routes.max_slippage_bps")?,
        simulated_at_ms: row.get(9)?,
        at_ms: row.get(10)?,
    })
}

fn read_tip(row: &Row<'_>) -> Result<TipRow, EngineError> {
    let stance: String = row.get(4)?;
    Ok(TipRow {
        trade_id: row.get(0)?,
        attempt: load_u32(row.get(1)?, "journal_tips.attempt")?,
        account: row.get(2)?,
        lamports: load_u64(row.get(3)?, "journal_tips.lamports")?,
        stance: stored_as(&stance, TipStance::parse, "journal_tips.stance")?,
        ev_net_lamports: row.get(5)?,
        ceiling_lamports: load_u64(row.get(6)?, "journal_tips.ceiling_lamports")?,
        at_ms: row.get(7)?,
    })
}

fn read_signature(row: &Row<'_>) -> Result<SignatureRow, EngineError> {
    let kind: String = row.get(2)?;
    let status: String = row.get(3)?;
    Ok(SignatureRow {
        signature: row.get(0)?,
        trade_id: row.get(1)?,
        kind: stored_as(&kind, SignatureKind::parse, "journal_signatures.kind")?,
        status: stored_as(&status, SignatureStatus::parse, "journal_signatures.status")?,
        slot: row
            .get::<_, Option<i64>>(4)?
            .map(|s| load_u64(s, "journal_signatures.slot"))
            .transpose()?,
        rebroadcasts: load_u32(row.get(5)?, "journal_signatures.rebroadcasts")?,
        at_ms: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    const AT_MS: i64 = 1_700_000_000_000;
    const MINT: &str = "So11111111111111111111111111111111111111112";
    const TIP_ACCOUNT: &str = "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5";

    /// A file of its own per test, for the reason `db.rs` gives about its own:
    /// WAL, the foreign-key pragma and the trigger are what is under test, and
    /// none of them mean anything against `:memory:`.
    struct TempDb(PathBuf);

    impl TempDb {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "sts-journal-{name}-{}-{}.db",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
            }
            TempDb(path)
        }

        fn open(&self) -> Database {
            Database::open(&self.0).expect("opens")
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
            }
        }
    }

    fn trade(id: &str) -> TradeRow {
        TradeRow::opened(
            id,
            MINT,
            Side::Buy,
            ExecutionMode::Paper,
            500_000_000,
            AT_MS,
        )
    }

    fn fill(id: &str, seq: u32) -> FillRow {
        FillRow::settle(
            id,
            seq,
            1_000_000_000,
            495_000_000,
            5_000_000,
            500_000_000,
            250_000_000,
            AT_MS + 10,
        )
        .expect("a real fill")
    }

    fn route(id: &str, seq: u32, decision: RouteDecision) -> RouteRow {
        RouteRow {
            trade_id: id.to_string(),
            seq,
            venue: Venue::PumpFunCurve,
            decision,
            tokens: 1_000_000_000,
            quoted_out_lamports: 500_000_000,
            min_out_lamports: 490_000_000,
            max_slippage_bps: 300,
            simulated_at_ms: AT_MS - 200,
            at_ms: AT_MS,
        }
    }

    fn tip(id: &str, attempt: u32, lamports: u64, ceiling: u64) -> TipRow {
        TipRow {
            trade_id: id.to_string(),
            attempt,
            account: TIP_ACCOUNT.to_string(),
            lamports,
            stance: TipStance::Emergency,
            ev_net_lamports: Some(12_000_000),
            ceiling_lamports: ceiling,
            at_ms: AT_MS + 5,
        }
    }

    // -----------------------------------------------------------------------
    // the schema
    // -----------------------------------------------------------------------

    #[test]
    fn a_fresh_file_carries_the_journal() {
        let temp = TempDb::new("schema");
        let db = temp.open();
        assert_eq!(db.schema_version(), crate::db::latest_schema_version());

        let conn = db.connection();
        for table in [
            "journal_trades",
            "journal_fills",
            "journal_routes",
            "journal_tips",
            "journal_signatures",
        ] {
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .expect("asks");
            assert_eq!(found, 1, "{table} is missing");
        }

        let trigger: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                  WHERE type = 'trigger' AND name = 'journal_trades_identity_is_immutable'",
                [],
                |row| row.get(0),
            )
            .expect("asks");
        assert_eq!(trigger, 1, "the identity trigger is missing");
    }

    #[test]
    fn no_column_in_the_journal_is_a_float() {
        // The claim the module header makes, checked against the file rather
        // than against the string that created it. A `REAL` anywhere here is a
        // quantity that cannot survive a round trip, and `execution_logs.price`
        // is the column in the older schema that this one refuses to repeat.
        let temp = TempDb::new("no-floats");
        let db = temp.open();
        let conn = db.connection();
        for table in [
            "journal_trades",
            "journal_fills",
            "journal_routes",
            "journal_tips",
            "journal_signatures",
        ] {
            let mut statement = conn
                .prepare(&format!(
                    "SELECT name, type FROM pragma_table_info('{table}')"
                ))
                .expect("prepares");
            let columns: Vec<(String, String)> = statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .expect("queries")
                .collect::<Result<_, _>>()
                .expect("reads");
            assert!(!columns.is_empty(), "{table} has no columns");
            for (name, kind) in columns {
                assert!(
                    kind == "INTEGER" || kind == "TEXT",
                    "{table}.{name} is {kind}, which is neither an integer nor text",
                );
            }
        }
    }

    #[test]
    fn migrating_twice_is_migrating_once() {
        let temp = TempDb::new("idempotent");
        {
            let db = temp.open();
            db.record_journal_trades(&[trade("t-1")]).expect("writes");
            db.close();
        }
        let again = temp.open();
        assert_eq!(again.schema_version(), crate::db::latest_schema_version());
        let rows = again
            .query_journal(&JournalFilter::default())
            .expect("reads");
        assert_eq!(
            rows.len(),
            1,
            "the reopen did not lose or duplicate the trade"
        );
    }

    // -----------------------------------------------------------------------
    // round trips
    // -----------------------------------------------------------------------

    #[test]
    fn every_row_comes_back_exactly_as_it_went_in() {
        let temp = TempDb::new("round-trip");
        let db = temp.open();

        let mut opened = trade("t-1");
        opened.venue = Some(Venue::RaydiumAmmV4);
        opened.tokens = 1_000_000_000;
        opened.fee_lamports = 5_000_000;
        opened.tip_lamports = 1_000_000;
        opened.slippage_bps = Some(101);
        let closed = opened.clone().closed_at(540_000_000, AT_MS + 60_000);

        db.record_journal_trades(std::slice::from_ref(&closed))
            .expect("writes");
        db.record_journal_fills(&[fill("t-1", 0), fill("t-1", 1)])
            .expect("writes");
        db.record_journal_routes(&[
            route("t-1", 0, RouteDecision::Chosen),
            route(
                "t-1",
                1,
                RouteDecision::Rejected {
                    because: "the curve quoted less".to_string(),
                },
            ),
        ])
        .expect("writes");
        db.record_journal_tips(&[tip("t-1", 0, 200_000, 1_000_000)])
            .expect("writes");
        db.record_journal_signatures(&[SignatureRow::broadcast(
            "5".repeat(64),
            "t-1",
            SignatureKind::Exit,
            AT_MS + 20,
        )])
        .expect("writes");

        let detail = db
            .journal_trade_detail("t-1")
            .expect("reads")
            .expect("is there");
        // One `assert_eq!` per child list, which is what deriving `Eq` buys:
        // a field added to any of these without being written or read is a
        // failure here rather than a column nobody noticed was empty.
        assert_eq!(detail.trade, closed);
        assert_eq!(detail.fills, vec![fill("t-1", 0), fill("t-1", 1)]);
        assert_eq!(
            detail.routes,
            vec![
                route("t-1", 0, RouteDecision::Chosen),
                route(
                    "t-1",
                    1,
                    RouteDecision::Rejected {
                        because: "the curve quoted less".to_string()
                    },
                ),
            ],
        );
        assert_eq!(detail.tips, vec![tip("t-1", 0, 200_000, 1_000_000)]);
        assert_eq!(detail.signatures.len(), 1);
        assert_eq!(detail.signatures[0].status, SignatureStatus::Broadcast);
        assert_eq!(detail.signatures[0].slot, None);
    }

    #[test]
    fn a_price_survives_the_column_to_the_last_digit() {
        let temp = TempDb::new("price");
        let db = temp.open();
        db.record_journal_trades(&[trade("t-1")]).expect("writes");

        // A launch-priced fill, and the same fill one 10^-18 away. Both have to
        // come back as themselves; a float column would land them on the same
        // number and a millionth would too.
        let a = FillRow::settle(
            "t-1",
            0,
            999_999_999_999_999_999,
            28_399,
            0,
            28_400,
            1,
            AT_MS,
        )
        .expect("a fill");
        let b = FillRow::settle(
            "t-1",
            1,
            999_999_999_999_999_999,
            28_400,
            0,
            28_400,
            1,
            AT_MS,
        )
        .expect("a fill");
        assert_ne!(a.price, b.price, "the two fills are not the same price");
        db.record_journal_fills(&[a.clone(), b.clone()])
            .expect("writes");

        let detail = db
            .journal_trade_detail("t-1")
            .expect("reads")
            .expect("is there");
        assert_eq!(detail.fills, vec![a, b]);
    }

    #[test]
    fn the_price_and_the_slippage_are_derived_from_the_pair_and_not_supplied() {
        // 5% under the quote, to the basis point, computed from integers.
        let filled =
            FillRow::settle("t-1", 0, 1_000_000, 950_000, 0, 1_000_000, 1, AT_MS).expect("a fill");
        assert_eq!(filled.slippage_bps, 500);
        assert_eq!(filled.price, Q18::ratio_floor(950_000, 1_000_000).unwrap());
        assert_eq!(
            filled.quoted,
            Q18::ratio_floor(1_000_000, 1_000_000).unwrap()
        );

        // A fill better than the quote is not negative slippage, it is none.
        let better = FillRow::settle("t-1", 1, 1_000_000, 1_100_000, 0, 1_000_000, 1, AT_MS)
            .expect("a fill");
        assert_eq!(better.slippage_bps, 0);

        // A fill of nothing has no price, so there is no row to build.
        assert!(FillRow::settle("t-1", 2, 0, 1_000, 0, 1_000, 1, AT_MS).is_none());
    }

    #[test]
    fn closing_a_trade_takes_the_fees_and_the_tip_off_the_profit() {
        let mut opened = trade("t-1");
        opened.cost_basis_lamports = 500_000_000;
        opened.fee_lamports = 5_000_000;
        opened.tip_lamports = 1_000_000;
        let closed = opened.closed_at(540_000_000, AT_MS + 1_000);
        assert_eq!(closed.realized_pnl_lamports, Some(34_000_000));
        assert_eq!(closed.proceeds_lamports, Some(540_000_000));
        assert!(!closed.is_open());

        // A loss is a negative number in the column and not a saturation.
        let lost = trade("t-2").closed_at(1_000_000, AT_MS + 1);
        assert_eq!(lost.realized_pnl_lamports, Some(-499_000_000));
    }

    // -----------------------------------------------------------------------
    // what the schema refuses
    // -----------------------------------------------------------------------

    #[test]
    fn a_child_with_no_trade_is_refused() {
        let temp = TempDb::new("fk");
        let db = temp.open();
        // No `journal_trades` row for `ghost`. The foreign key is the whole
        // point of the pragma `db.rs` sets on every connection.
        let err = db
            .record_journal_fills(&[fill("ghost", 0)])
            .expect_err("is refused");
        assert!(
            format!("{err}").to_lowercase().contains("foreign key"),
            "{err} does not name the constraint",
        );
    }

    #[test]
    fn deleting_a_trade_takes_its_children_with_it() {
        let temp = TempDb::new("cascade");
        let db = temp.open();
        db.record_journal_trades(&[trade("t-1")]).expect("writes");
        db.record_journal_fills(&[fill("t-1", 0)]).expect("writes");
        db.record_journal_tips(&[tip("t-1", 0, 1, 2)])
            .expect("writes");

        db.connection()
            .execute("DELETE FROM journal_trades WHERE trade_id = 't-1'", [])
            .expect("deletes");

        assert!(db.journal_trade_detail("t-1").expect("reads").is_none());
        let orphans: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM journal_fills", [], |row| row.get(0))
            .expect("counts");
        assert_eq!(orphans, 0, "a fill outlived its trade");
    }

    #[test]
    fn a_trade_cannot_change_what_it_is() {
        let temp = TempDb::new("identity");
        let db = temp.open();
        db.record_journal_trades(&[trade("t-1")]).expect("writes");

        let mut moved = trade("t-1");
        moved.mint = "a different mint entirely".to_string();
        let err = db.record_journal_trades(&[moved]).expect_err("is refused");
        assert!(
            format!("{err}").contains("cannot change what it is"),
            "{err} is not the trigger speaking",
        );

        // And the row is untouched.
        let still = db
            .journal_trade_detail("t-1")
            .expect("reads")
            .expect("is there");
        assert_eq!(still.trade.mint, MINT);
    }

    #[test]
    fn recording_a_trade_again_closes_it_rather_than_duplicating_it() {
        let temp = TempDb::new("upsert");
        let db = temp.open();
        let opened = trade("t-1");
        db.record_journal_trades(std::slice::from_ref(&opened))
            .expect("writes");
        db.record_journal_trades(&[opened.clone().closed_at(600_000_000, AT_MS + 5_000)])
            .expect("writes");

        let rows = db.query_journal(&JournalFilter::default()).expect("reads");
        assert_eq!(rows.len(), 1, "the second write made a second trade");
        assert_eq!(rows[0].proceeds_lamports, Some(600_000_000));
        assert_eq!(rows[0].realized_pnl_lamports, Some(100_000_000));
        assert_eq!(rows[0].closed_at_ms, Some(AT_MS + 5_000));
    }

    #[test]
    fn one_trade_goes_one_way() {
        let temp = TempDb::new("one-route");
        let db = temp.open();
        db.record_journal_trades(&[trade("t-1")]).expect("writes");
        db.record_journal_routes(&[route("t-1", 0, RouteDecision::Chosen)])
            .expect("writes");

        let mut second = route("t-1", 1, RouteDecision::Chosen);
        second.venue = Venue::RaydiumAmmV4;
        let err = db.record_journal_routes(&[second]).expect_err("is refused");
        assert!(
            format!("{err}").to_lowercase().contains("unique"),
            "{err} does not name the index",
        );
    }

    #[test]
    fn a_route_cannot_be_both_taken_and_passed_over() {
        let temp = TempDb::new("route-check");
        let db = temp.open();
        db.record_journal_trades(&[trade("t-1")]).expect("writes");
        // Unrepresentable through `RouteDecision`, so it takes raw SQL to try
        // it, which is the case the `CHECK` is actually defending against.
        let err = db.connection().execute(
            "INSERT INTO journal_routes (trade_id, seq, venue, chosen, tokens,
                 quoted_out_lamports, min_out_lamports, max_slippage_bps, rejected_because,
                 simulated_at_ms, at_ms)
             VALUES ('t-1', 0, 'pump_fun_curve', 1, 1, 1, 1, 0, 'and also rejected', 0, 0)",
            [],
        );
        assert!(err.is_err(), "the check let a contradictory route through");
    }

    #[test]
    fn only_something_that_landed_has_a_slot() {
        let temp = TempDb::new("slot");
        let db = temp.open();
        db.record_journal_trades(&[trade("t-1")]).expect("writes");

        let sent = SignatureRow::broadcast("a".repeat(64), "t-1", SignatureKind::Entry, AT_MS);
        db.record_journal_signatures(std::slice::from_ref(&sent))
            .expect("writes");

        // Confirming carries a slot through.
        let landed = sent.clone().confirmed_in(250_000_001, AT_MS + 400);
        db.record_journal_signatures(std::slice::from_ref(&landed))
            .expect("writes");
        let detail = db
            .journal_trade_detail("t-1")
            .expect("reads")
            .expect("is there");
        assert_eq!(detail.signatures, vec![landed.clone()]);

        // Settling any other way drops it, because the column refuses one.
        let dropped = landed.settled_as(SignatureStatus::Dropped, AT_MS + 800);
        assert_eq!(dropped.slot, None);
        db.record_journal_signatures(std::slice::from_ref(&dropped))
            .expect("writes");
        let detail = db
            .journal_trade_detail("t-1")
            .expect("reads")
            .expect("is there");
        assert_eq!(detail.signatures, vec![dropped]);

        // And forcing one past the type is refused by the file.
        let forced = db.connection().execute(
            "UPDATE journal_signatures SET status = 'expired', slot = 7",
            [],
        );
        assert!(
            forced.is_err(),
            "a slot survived on something that never landed"
        );
    }

    #[test]
    fn a_quantity_past_what_a_column_holds_is_refused_rather_than_wrapped() {
        let temp = TempDb::new("too-big");
        let db = temp.open();
        let mut huge = trade("t-1");
        huge.notional_lamports = u64::MAX;
        let err = db.record_journal_trades(&[huge]).expect_err("is refused");
        assert!(
            format!("{err}").contains("past what a column holds"),
            "{err} is not the range check speaking",
        );
    }

    // -----------------------------------------------------------------------
    // asking it questions
    // -----------------------------------------------------------------------

    /// Nine trades: three mints, three modes, alternating sides, opened a
    /// second apart, every third one closed.
    fn seed(db: &Database) {
        let mints = [MINT, "MintTwo", "MintThree"];
        let modes = [
            ExecutionMode::Live,
            ExecutionMode::Paper,
            ExecutionMode::Replay,
        ];
        let mut rows = Vec::new();
        for index in 0..9u32 {
            let mut row = TradeRow::opened(
                format!("t-{index}"),
                mints[index as usize % 3],
                if index % 2 == 0 {
                    Side::Buy
                } else {
                    Side::Sell
                },
                modes[index as usize % 3],
                100_000_000 + u64::from(index) * 1_000_000,
                AT_MS + i64::from(index) * 1_000,
            );
            row.venue = Some(if index % 2 == 0 {
                Venue::PumpFunCurve
            } else {
                Venue::RaydiumAmmV4
            });
            row.slippage_bps = Some((index * 25) as u16);
            if index % 3 == 0 {
                row = row.closed_at(150_000_000, AT_MS + i64::from(index) * 1_000 + 500);
            }
            rows.push(row);
        }
        db.record_journal_trades(&rows).expect("writes");
    }

    #[test]
    fn the_default_filter_is_the_whole_book_newest_first() {
        let temp = TempDb::new("filter-default");
        let db = temp.open();
        seed(&db);
        let rows = db.query_journal(&JournalFilter::default()).expect("reads");
        assert_eq!(rows.len(), 9);
        assert_eq!(rows[0].trade_id, "t-8", "the newest is not first");
        assert_eq!(rows[8].trade_id, "t-0");
    }

    #[test]
    fn every_filter_narrows_to_what_it_names() {
        let temp = TempDb::new("filter-each");
        let db = temp.open();
        seed(&db);

        let by_mode = db
            .query_journal(&JournalFilter::in_mode(ExecutionMode::Paper))
            .expect("reads");
        assert_eq!(by_mode.len(), 3);
        assert!(by_mode.iter().all(|t| t.mode == ExecutionMode::Paper));

        let by_mint = db
            .query_journal(&JournalFilter {
                mint: Some("MintTwo".into()),
                ..Default::default()
            })
            .expect("reads");
        assert_eq!(by_mint.len(), 3);

        let by_venue = db
            .query_journal(&JournalFilter {
                venue: Some(Venue::RaydiumAmmV4),
                ..Default::default()
            })
            .expect("reads");
        assert_eq!(by_venue.len(), 4);

        let by_side = db
            .query_journal(&JournalFilter {
                side: Some(Side::Sell),
                ..Default::default()
            })
            .expect("reads");
        assert_eq!(by_side.len(), 4);

        let by_window = db
            .query_journal(&JournalFilter {
                since_ms: Some(AT_MS + 2_000),
                until_ms: Some(AT_MS + 4_000),
                ..Default::default()
            })
            .expect("reads");
        assert_eq!(by_window.len(), 3, "the window is inclusive at both ends");

        let by_slippage = db
            .query_journal(&JournalFilter {
                min_slippage_bps: Some(150),
                ..Default::default()
            })
            .expect("reads");
        assert_eq!(by_slippage.len(), 3, "t-6, t-7 and t-8");

        let closed = db
            .query_journal(&JournalFilter {
                only_closed: true,
                ..Default::default()
            })
            .expect("reads");
        assert_eq!(closed.len(), 3);
        assert!(closed.iter().all(|t| !t.is_open()));

        let losses = db
            .query_journal(&JournalFilter {
                max_realized_pnl_lamports: Some(0),
                ..Default::default()
            })
            .expect("reads");
        assert!(
            losses.is_empty(),
            "every closed trade in the seed is a gain"
        );
    }

    #[test]
    fn the_filters_compose_rather_than_replace_each_other() {
        let temp = TempDb::new("filter-compose");
        let db = temp.open();
        seed(&db);
        let rows = db
            .query_journal(&JournalFilter {
                mode: Some(ExecutionMode::Live),
                side: Some(Side::Buy),
                only_closed: true,
                ..Default::default()
            })
            .expect("reads");
        // Live is 0, 3, 6; Buy is even; closed is every third. Only 0 and 6 are
        // all three.
        assert_eq!(
            rows.iter().map(|t| t.trade_id.as_str()).collect::<Vec<_>>(),
            vec!["t-6", "t-0"],
        );
    }

    #[test]
    fn a_page_is_a_page_and_the_ceiling_is_the_ceiling() {
        let temp = TempDb::new("paging");
        let db = temp.open();
        seed(&db);

        let first = db
            .query_journal(&JournalFilter {
                limit: 4,
                ..Default::default()
            })
            .expect("reads");
        let second = db
            .query_journal(&JournalFilter {
                limit: 4,
                offset: 4,
                ..Default::default()
            })
            .expect("reads");
        assert_eq!(first.len(), 4);
        assert_eq!(second.len(), 4);
        assert_eq!(first[3].trade_id, "t-5");
        assert_eq!(
            second[0].trade_id, "t-4",
            "the pages do not overlap or skip"
        );

        // Zero means the default rather than nothing, and nothing gets past the
        // ceiling however large the ask.
        assert_eq!(
            JournalFilter {
                limit: 0,
                ..Default::default()
            }
            .effective_limit(),
            DEFAULT_LIMIT
        );
        assert_eq!(
            JournalFilter {
                limit: u32::MAX,
                ..Default::default()
            }
            .effective_limit(),
            MAX_LIMIT,
        );
    }

    #[test]
    fn the_totals_are_of_the_filter_and_not_of_the_page() {
        let temp = TempDb::new("totals");
        let db = temp.open();
        seed(&db);

        let filter = JournalFilter {
            limit: 2,
            ..Default::default()
        };
        let page = db.query_journal(&filter).expect("reads");
        let totals = db.journal_totals(&filter).expect("sums");
        assert_eq!(page.len(), 2);
        assert_eq!(
            totals.trades, 9,
            "the totals counted the page instead of the book"
        );
        assert_eq!(totals.closed, 3);
        // 100 + 101 + ... + 108, in millions of lamports.
        assert_eq!(totals.notional_lamports, 936_000_000);
        assert_eq!(totals.proceeds_lamports, 450_000_000);
        assert_eq!(totals.worst_slippage_bps, Some(200));
    }

    #[test]
    fn the_totals_of_nothing_are_zero_and_an_unknown_worst() {
        let temp = TempDb::new("totals-empty");
        let db = temp.open();
        let totals = db.journal_totals(&JournalFilter::default()).expect("sums");
        assert_eq!(totals, JournalTotals::default());
        // Not `Some(0)`: nothing has filled, which is not the same as nothing
        // having slipped.
        assert_eq!(totals.worst_slippage_bps, None);
    }

    #[test]
    fn the_sums_are_exact_at_a_size_a_float_would_round() {
        let temp = TempDb::new("exact-sums");
        let db = temp.open();
        // Nine hundred trades of a size that lands the total past 2^53, where
        // an `f64` stops being able to count by ones. The odd lamport on each
        // is the digit that would go missing.
        let rows: Vec<TradeRow> = (0..900u32)
            .map(|index| {
                TradeRow::opened(
                    format!("t-{index}"),
                    MINT,
                    Side::Buy,
                    ExecutionMode::Paper,
                    20_000_000_000_001,
                    AT_MS + i64::from(index),
                )
            })
            .collect();
        db.record_journal_trades(&rows).expect("writes");

        let totals = db.journal_totals(&JournalFilter::default()).expect("sums");
        assert_eq!(totals.notional_lamports, 900 * 20_000_000_000_001);
        assert!(
            totals.notional_lamports > (1i64 << 53),
            "the total is not past where a float would start rounding",
        );
    }

    #[test]
    fn a_mint_from_a_text_box_is_a_parameter_and_not_sql() {
        let temp = TempDb::new("injection");
        let db = temp.open();
        seed(&db);
        let rows = db
            .query_journal(&JournalFilter {
                mint: Some("'; DROP TABLE journal_trades; --".into()),
                ..Default::default()
            })
            .expect("reads");
        assert!(rows.is_empty(), "no mint is named that");
        // And the table is still there.
        assert_eq!(
            db.query_journal(&JournalFilter::default())
                .expect("reads")
                .len(),
            9
        );
    }

    #[test]
    fn the_overruns_are_the_only_tips_that_come_back() {
        let temp = TempDb::new("overruns");
        let db = temp.open();
        db.record_journal_trades(&[trade("t-1")]).expect("writes");
        db.record_journal_tips(&[
            tip("t-1", 0, 200_000, 1_000_000),
            tip("t-1", 1, 900_000, 1_000_000),
            tip("t-1", 2, 1_400_000, 1_000_000),
        ])
        .expect("writes");

        let overruns = db.journal_tip_overruns(50).expect("reads");
        assert_eq!(overruns.len(), 1);
        assert_eq!(overruns[0].attempt, 2);
        assert!(overruns[0].is_over_ceiling());
        assert!(!tip("t-1", 0, 200_000, 1_000_000).is_over_ceiling());
    }

    #[test]
    fn what_is_still_out_there_is_counted_by_mode() {
        let temp = TempDb::new("in-flight");
        let db = temp.open();
        let mut live = trade("t-live");
        live.mode = ExecutionMode::Live;
        db.record_journal_trades(&[live, trade("t-paper")])
            .expect("writes");
        db.record_journal_signatures(&[
            SignatureRow::broadcast("a".repeat(64), "t-live", SignatureKind::Exit, AT_MS),
            SignatureRow::broadcast("b".repeat(64), "t-paper", SignatureKind::Exit, AT_MS),
            SignatureRow::broadcast("c".repeat(64), "t-paper", SignatureKind::Entry, AT_MS)
                .confirmed_in(1, AT_MS + 1),
        ])
        .expect("writes");

        let in_flight = db.journal_in_flight().expect("counts");
        assert_eq!(in_flight.get(&ExecutionMode::Live), Some(&1));
        assert_eq!(in_flight.get(&ExecutionMode::Paper), Some(&1));
        assert_eq!(in_flight.get(&ExecutionMode::Replay), None);
    }

    // -----------------------------------------------------------------------
    // under load
    // -----------------------------------------------------------------------

    #[test]
    fn every_row_lands_when_eight_threads_write_at_once() {
        // The shape of heavy tick ingestion: several producers, one file, no
        // coordination between them beyond the mutex `db.rs` puts every writer
        // behind. Nothing may be lost and nothing may be duplicated.
        const THREADS: u32 = 8;
        const PER_THREAD: u32 = 40;

        let temp = TempDb::new("concurrent");
        let db = Arc::new(temp.open());

        let trades: Vec<TradeRow> = (0..THREADS)
            .map(|t| {
                TradeRow::opened(
                    format!("t-{t}"),
                    MINT,
                    Side::Buy,
                    ExecutionMode::Paper,
                    1,
                    AT_MS,
                )
            })
            .collect();
        db.record_journal_trades(&trades).expect("writes");

        let mut handles = Vec::new();
        for thread in 0..THREADS {
            let db = Arc::clone(&db);
            handles.push(std::thread::spawn(move || {
                let id = format!("t-{thread}");
                for seq in 0..PER_THREAD {
                    let at = AT_MS + i64::from(seq);
                    db.record_journal_fills(&[FillRow::settle(
                        &id,
                        seq,
                        1_000_000,
                        900_000 + u64::from(seq),
                        1_000,
                        1_000_000,
                        u64::from(seq),
                        at,
                    )
                    .expect("a fill")])
                        .expect("writes");
                    db.record_journal_signatures(&[SignatureRow::broadcast(
                        format!("{thread:02}{seq:062}"),
                        &id,
                        SignatureKind::Exit,
                        at,
                    )])
                    .expect("writes");
                }
            }));
        }
        for handle in handles {
            handle.join().expect("the writer did not panic");
        }

        let conn = db.connection();
        let fills: i64 = conn
            .query_row("SELECT COUNT(*) FROM journal_fills", [], |row| row.get(0))
            .expect("counts");
        let signatures: i64 = conn
            .query_row("SELECT COUNT(*) FROM journal_signatures", [], |row| {
                row.get(0)
            })
            .expect("counts");
        assert_eq!(fills, i64::from(THREADS * PER_THREAD));
        assert_eq!(signatures, i64::from(THREADS * PER_THREAD));
    }

    #[test]
    fn writing_the_same_run_twice_is_writing_it_once() {
        // What a replay does. The keys are the caller's, so the second pass
        // conflicts with the first on every row and changes nothing.
        let temp = TempDb::new("replay");
        let db = temp.open();
        let trades = [trade("t-1")];
        let fills = [fill("t-1", 0), fill("t-1", 1)];
        let routes = [route("t-1", 0, RouteDecision::Chosen)];
        let tips = [tip("t-1", 0, 200_000, 1_000_000)];

        for pass in 0..2 {
            db.record_journal_trades(&trades).expect("writes");
            let written = db.record_journal_fills(&fills).expect("writes");
            db.record_journal_routes(&routes).expect("writes");
            db.record_journal_tips(&tips).expect("writes");
            if pass == 1 {
                assert_eq!(written, 0, "the second pass wrote a fill again");
            }
        }

        let detail = db
            .journal_trade_detail("t-1")
            .expect("reads")
            .expect("is there");
        assert_eq!(detail.fills.len(), 2);
        assert_eq!(detail.routes.len(), 1);
        assert_eq!(detail.tips.len(), 1);
    }

    #[test]
    fn an_empty_batch_is_not_a_transaction() {
        let temp = TempDb::new("empty");
        let db = temp.open();
        assert_eq!(db.record_journal_trades(&[]).expect("writes"), 0);
        assert_eq!(db.record_journal_fills(&[]).expect("writes"), 0);
        assert_eq!(db.record_journal_routes(&[]).expect("writes"), 0);
        assert_eq!(db.record_journal_tips(&[]).expect("writes"), 0);
        assert_eq!(db.record_journal_signatures(&[]).expect("writes"), 0);
    }

    #[test]
    fn a_trade_nobody_recorded_has_no_detail() {
        let temp = TempDb::new("missing");
        let db = temp.open();
        assert!(db
            .journal_trade_detail("never-happened")
            .expect("reads")
            .is_none());
    }

    #[test]
    fn the_filter_survives_the_trip_the_window_sends_it_on() {
        // The UI sends the two fields the operator set and nothing else, and
        // `default` on the struct is what makes the other ten mean "do not
        // filter" rather than a deserialisation error.
        let sparse: JournalFilter =
            serde_json::from_str(r#"{"mode":"live","onlyClosed":true}"#).expect("deserialises");
        assert_eq!(
            sparse,
            JournalFilter {
                mode: Some(ExecutionMode::Live),
                only_closed: true,
                ..JournalFilter::default()
            },
        );

        let full = JournalFilter {
            mint: Some(MINT.to_string()),
            venue: Some(Venue::PumpFunCurve),
            side: Some(Side::Sell),
            min_slippage_bps: Some(250),
            ..JournalFilter::default()
        };
        let json = serde_json::to_string(&full).expect("serialises");
        assert_eq!(
            serde_json::from_str::<JournalFilter>(&json).expect("reads"),
            full
        );
    }
}
