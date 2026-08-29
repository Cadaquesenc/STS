//! The log of what the engine saw, the checkpoints over it, and the counter
//! that orders both.
//!
//! `journal.rs` is the book: one row per trade, and what it cost. This is the
//! other half of the same question, and it is the half that is usually empty —
//! because the gate refuses almost everything, and a book that only records the
//! trades cannot say why there were so few of them. A month later the question
//! is never "what did trade 41 cost", which the book answers; it is "there were
//! four trades last Tuesday and nine hundred launches, so what happened to the
//! other eight hundred and ninety-six", and nothing in this build could answer
//! that from disk. Phase 6B's soak asks for exactly that sentence — "zero-trade
//! periods decomposed" — and this is where the decomposition lives.
//!
//! Migration 5, three tables, one idea each.
//!
//! **`journal_state_log`** is one row per launch the gate read: the verdict, the
//! reason, the evidence behind it, and the risk gate's readings at that instant.
//! Append-only, never updated, several rows a second on a busy morning.
//!
//! **`journal_snapshots`** is a periodic checkpoint of the book, hash-chained,
//! so a restart does not have to add up a hundred thousand trades to know what
//! it is holding, and so a snapshot cannot be quietly edited afterwards.
//!
//! **`journal_revisions`** is one monotonic counter per mode, and it is what
//! makes the other two into a pair rather than two unrelated tables.
//!
//! # Why a revision and not a timestamp
//!
//! Every other table here orders itself by `at_ms`, and that is right for them:
//! they are describing when something happened on a chain. It is wrong for this
//! one. A wall clock is not monotonic — `now_ms` reads `SystemTime`, NTP steps
//! it, and a step of a few hundred milliseconds during a busy minute puts two
//! state rows in the wrong order or gives them the same key. Ordering the
//! forensic record by a clock that can go backwards means the record of a bad
//! minute is the record most likely to be scrambled, which is precisely
//! backwards.
//!
//! So a revision: an integer that only ever goes up, allocated by the writer
//! inside the same transaction as the rows it stamps. Three properties follow,
//! and all three are load-bearing.
//!
//! **It is gapless.** The allocation and the insert commit together, so a
//! revision is never issued for a row that then rolled back. A reader walking
//! `1..=N` therefore knows that a missing revision is a missing row rather than
//! a transaction that lost a race — which is the difference between "the log is
//! intact" and "the log is intact as far as I can tell".
//!
//! **It is per mode.** Live, paper and replay each have their own counter,
//! rather than one counter shared across the file. Phase 3's acceptance
//! criterion is that the same fixture and the same seed produce byte-identical
//! records, and a replay whose revisions depended on how much live traffic
//! happened to be flowing beside it fails that on the second run. Three
//! counters cost three rows.
//!
//! **It never goes backwards.** A trigger, not a Rust check, for the same
//! reason `journal_trades` guards its identity in a trigger: the guarantee has
//! to hold for every writer, including a person at a shell with `sqlite3` open.
//!
//! # What the snapshots are actually for
//!
//! Two things, and they want to be kept apart because only one of them is
//! checkable at any moment.
//!
//! The **chain** is tamper-evidence and is always checkable.
//! [`Database::verify_journal_snapshot_chain`] walks every snapshot in a mode
//! from the first, recomputes each digest over its own fields and its
//! predecessor's digest, and compares. Editing any number in any snapshot row
//! breaks that row and every row after it. This holds whatever the book has
//! done since, because it is a statement about the snapshots and not about the
//! book.
//!
//! The **recomputation** is a statement about the book, and it is only
//! conclusive while the book has not moved.
//! [`Database::verify_journal_snapshot`] adds the book up again and compares
//! against the newest snapshot — but a snapshot taken at revision 900 says
//! nothing false when the book has reached revision 950, it just no longer
//! describes now. So the verdict has three arms rather than two, and
//! [`SnapshotVerdict::Superseded`] is an answer rather than a failure. Warm
//! start is the caller that cares: matching means the process can trust the
//! checkpoint and skip the scan, superseded means rescan, diverged means
//! somebody has been editing the file underneath a running system and the
//! honest response is to say so loudly.
//!
//! There is one cross-check that does hold across time, and the chain
//! verification does it: the number of entries and refusals a snapshot claims,
//! minus what its predecessor claimed, must equal the rows actually in the log
//! between the two revisions. That is the checkpoint and the log agreeing about
//! the same interval, and it is checked whenever that interval is still intact
//! — which is to say, whenever nothing has been pruned out from under it.
//!
//! # The write path
//!
//! [`StateLogger`] is the high-throughput end: a bounded queue, one writer
//! thread, one transaction per batch, `prepare_cached` statements. The engine's
//! thread pays a `try_send` and returns.
//!
//! A full queue drops, and the drop is counted rather than silent. That is the
//! bounded-queue behaviour Phase 0 asks for and not a violation of "no critical
//! event is silently lost": a forensic state row is an observation of a
//! decision, and the decision itself — the trade, the intent, the exit — is
//! already durable in the book and the two ledgers before this row is ever
//! queued. Losing the annotation under saturation is survivable. Losing it
//! without saying so is not, which is why `dropped` is on the stats and why
//! [`StateLogger::flush`] exists for the paths that would rather wait.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;
use rusqlite::{OptionalExtension, Row, Transaction};
use serde::{Deserialize, Serialize};

use crate::db::{Database, ExecutionMode};
use crate::error::EngineError;
use crate::journal::JournalTotals;
use crate::strategy::syndicate::{GateReason, GateVerdict};
use crate::types::{OperatingMode, RiskSnapshot};

/// The forensic log, the checkpoints and the counter, as migration 5.
///
/// Registered in `db.rs`'s `MIGRATIONS` beside the rest, for the reason
/// migration 3 gives: one chain, one runner, one meaning per version number.
///
/// The three counter rows are seeded here rather than created on first use. A
/// counter that springs into existence on the first write is a counter whose
/// absence and whose zero are the same reading, and "this mode has never
/// written anything" is a fact worth being able to state.
pub(crate) const MIGRATION_0005: &str = "
    CREATE TABLE IF NOT EXISTS journal_revisions (
        stream       TEXT    PRIMARY KEY CHECK (stream IN ('live', 'paper', 'replay')),
        -- The last revision issued. Zero means none: the first row written in a
        -- mode is revision 1, so a revision is never the same integer as 'no
        -- revision'.
        revision     INTEGER NOT NULL CHECK (revision >= 0),
        issued_at_ms INTEGER NOT NULL
    ) WITHOUT ROWID;

    INSERT OR IGNORE INTO journal_revisions (stream, revision, issued_at_ms)
        VALUES ('live', 0, 0), ('paper', 0, 0), ('replay', 0, 0);

    -- The whole point of the counter, and the reason it is a trigger rather
    -- than a Rust assertion: a monotonic counter that only one caller respects
    -- is not monotonic. This holds against `sqlite3` on the command line too.
    CREATE TRIGGER IF NOT EXISTS journal_revisions_only_go_forward
        BEFORE UPDATE ON journal_revisions
        WHEN new.revision <= old.revision
    BEGIN
        SELECT RAISE(ABORT, 'a revision cannot go backwards');
    END;

    -- A mode is not something that gets added or removed at runtime. Three rows,
    -- seeded above, and nothing else.
    CREATE TRIGGER IF NOT EXISTS journal_revisions_are_the_three_modes
        BEFORE DELETE ON journal_revisions
    BEGIN
        SELECT RAISE(ABORT, 'the revision counters are the three modes and are not deleted');
    END;

    CREATE TABLE IF NOT EXISTS journal_state_log (
        mode              TEXT    NOT NULL CHECK (mode IN ('live', 'paper', 'replay')),
        -- Allocated by the writer inside the transaction that inserts the row.
        -- Gapless from 1, so a hole is a missing row and not a lost race.
        revision          INTEGER NOT NULL CHECK (revision > 0),

        mint              TEXT    NOT NULL,
        -- Set only where a position was actually opened, and then it is the
        -- intent id `journal_trades` keys by. Null on every refusal, which is
        -- most rows.
        intent_id         TEXT,

        decision          TEXT    NOT NULL CHECK (decision IN (
                                    'entered', 'refused', 'deferred')),
        -- `strategy::syndicate`'s vocabulary, unchanged. This table is the
        -- forensic record of that gate and borrowing its words is the point:
        -- a funnel over this column and a funnel over a backtest report have to
        -- be the same table or neither can be checked against the other.
        reason            TEXT    NOT NULL CHECK (reason IN (
                                    'no-opening-buys', 'thin', 'low-score',
                                    'no-primary-signal', 'no-bundle', 'thin-bundle',
                                    'mixed-sizing', 'solo-dev', 'small-bundle',
                                    'coordinated-ring', 'no-curve-quote',
                                    'sandwich-risk', 'accepted')),
        confidence_micros INTEGER NOT NULL CHECK (confidence_micros BETWEEN 0 AND 1000000),

        -- What the gate read.
        buyers            INTEGER NOT NULL CHECK (buyers >= 0),
        bundle_wallets    INTEGER NOT NULL CHECK (bundle_wallets >= 0),
        cohort_wallets    INTEGER NOT NULL CHECK (cohort_wallets >= 0),
        cohort_lamports   INTEGER NOT NULL CHECK (cohort_lamports >= 0),
        pool_lamports     INTEGER NOT NULL CHECK (pool_lamports >= 0),

        -- What the risk gate was saying at the same instant. A verdict of
        -- 'accepted' that opened nothing is one of the two shapes of zero-trade
        -- period, and without these columns it is indistinguishable from the
        -- other.
        operating_mode    TEXT    NOT NULL CHECK (operating_mode IN (
                                    'live', 'paper', 'replay', 'halted')),
        entries_allowed   INTEGER NOT NULL CHECK (entries_allowed IN (0, 1)),
        equity_lamports   INTEGER NOT NULL CHECK (equity_lamports >= 0),
        drawdown_bps      INTEGER NOT NULL CHECK (drawdown_bps BETWEEN 0 AND 10000),
        open_positions    INTEGER NOT NULL CHECK (open_positions >= 0),
        free_slots        INTEGER NOT NULL CHECK (free_slots >= 0),

        -- The no-leakage columns. `evidence_to_ms` is the newest event that
        -- reached the record the gate read, and it is a number rather than a
        -- comment so a detector that started reading one event too far shows up
        -- here instead of as a verdict that quietly changed.
        window_closed     INTEGER NOT NULL CHECK (window_closed IN (0, 1)),
        evidence_to_ms    INTEGER NOT NULL,
        decided_at_ms     INTEGER NOT NULL,

        PRIMARY KEY (mode, revision),

        -- A decision that entered names what it entered; the other two cannot.
        CHECK ((decision = 'entered' AND intent_id IS NOT NULL)
            OR (decision <> 'entered' AND intent_id IS NULL)),
        -- The gate's own vocabulary: 'accepted' is the only reason that trades,
        -- and it is not automatically an entry — the risk gate, the window and
        -- the kill switch all sit after it. So acceptance is necessary for an
        -- entry and not sufficient, which is exactly this check.
        CHECK (decision <> 'entered' OR reason = 'accepted'),
        -- Evidence from after the decision is leakage, and it is refused at the
        -- column rather than found in a review.
        CHECK (evidence_to_ms <= decided_at_ms)
    ) WITHOUT ROWID;

    -- What happened, newest first, in one mode. The read the cockpit does.
    CREATE INDEX IF NOT EXISTS journal_state_log_at
        ON journal_state_log (mode, decided_at_ms DESC);
    -- The funnel: how many of each reason over a window.
    CREATE INDEX IF NOT EXISTS journal_state_log_reason
        ON journal_state_log (mode, reason, decided_at_ms DESC);
    -- One launch, everything the engine ever thought about it.
    CREATE INDEX IF NOT EXISTS journal_state_log_mint
        ON journal_state_log (mint, decided_at_ms DESC);
    -- The rows that became trades, for the join back to the book.
    CREATE INDEX IF NOT EXISTS journal_state_log_entered
        ON journal_state_log (intent_id)
        WHERE intent_id IS NOT NULL;

    -- A forensic row is a record of what was believed at one instant. Belief at
    -- that instant does not change later; a later belief is a later row. An
    -- UPDATE here would rewrite the evidence a decision is being judged against,
    -- which is the one edit this table exists to make impossible.
    CREATE TRIGGER IF NOT EXISTS journal_state_log_is_append_only
        BEFORE UPDATE ON journal_state_log
    BEGIN
        SELECT RAISE(ABORT, 'a forensic state row records what was believed then and does not change');
    END;

    CREATE TABLE IF NOT EXISTS journal_snapshots (
        mode                  TEXT    NOT NULL CHECK (mode IN ('live', 'paper', 'replay')),
        -- Which checkpoint this is, counting from one. The key, and the order
        -- the chain is walked in.
        --
        -- Not `revision`, and the difference is the whole reason this column
        -- exists. A checkpoint is a statement about two things — the book and
        -- the log — and they do not move together: the exit path writes the
        -- book whether or not anything is logging verdicts, so the book can
        -- change while the counter stands still. Keyed by revision, the second
        -- of those changes could never be recorded, because the row naming that
        -- revision would already be there.
        seq                   INTEGER NOT NULL CHECK (seq > 0),
        -- The log revision this checkpoint accounts for. It does not consume a
        -- revision of its own: a snapshot is a statement about the first N rows,
        -- not an N+1th row. Two consecutive checkpoints may name the same one.
        revision              INTEGER NOT NULL CHECK (revision >= 0),
        taken_at_ms           INTEGER NOT NULL,

        -- The book, added up. `journal.rs`'s `JournalTotals`, column for column.
        trades                INTEGER NOT NULL CHECK (trades >= 0),
        closed                INTEGER NOT NULL CHECK (closed >= 0),
        notional_lamports     INTEGER NOT NULL CHECK (notional_lamports >= 0),
        cost_basis_lamports   INTEGER NOT NULL CHECK (cost_basis_lamports >= 0),
        proceeds_lamports     INTEGER NOT NULL CHECK (proceeds_lamports >= 0),
        realized_pnl_lamports INTEGER NOT NULL,
        fee_lamports          INTEGER NOT NULL CHECK (fee_lamports >= 0),
        tip_lamports          INTEGER NOT NULL CHECK (tip_lamports >= 0),
        -- Null when nothing in the mode has filled. Not zero: zero slippage is
        -- a reading and 'nobody has filled' is not.
        worst_slippage_bps    INTEGER CHECK (worst_slippage_bps IS NULL
                                OR (worst_slippage_bps BETWEEN 0 AND 10000)),

        -- The revision the previous checkpoint stood at. With `revision` above
        -- it, the row says exactly which slice of the log it speaks for:
        -- `covers_from < r <= revision`. Zero on the first checkpoint of a mode.
        covers_from           INTEGER NOT NULL CHECK (covers_from >= 0),

        -- The log over that slice, and over that slice only.
        --
        -- Deltas rather than running totals, and the difference is not a
        -- preference. Retention deletes rows from the old end of the log, so a
        -- running count of surviving rows *falls* every time the pruner runs —
        -- and a cross-check built on the difference between two running counts
        -- would read a successful prune as a checkpoint claiming a negative
        -- number of rows. A delta is computed once, over an interval the pruner
        -- cannot reach into (it never goes above the newest checkpoint, and at
        -- the moment of writing this row that is the previous one), and it
        -- stays true afterwards whatever retention does.
        rows_since            INTEGER NOT NULL CHECK (rows_since >= 0),
        entered_since         INTEGER NOT NULL CHECK (entered_since >= 0),
        refused_since         INTEGER NOT NULL CHECK (refused_since >= 0),
        deferred_since        INTEGER NOT NULL CHECK (deferred_since >= 0),

        -- The chain. Null on the first snapshot of a mode and on nothing else.
        prev_digest           TEXT,
        digest                TEXT    NOT NULL,

        PRIMARY KEY (mode, seq),

        CHECK (closed <= trades),
        CHECK (entered_since + refused_since + deferred_since = rows_since),
        -- The slice runs forwards, and holds no more rows than it has revisions
        -- to hold them in.
        CHECK (covers_from <= revision),
        CHECK (rows_since <= revision - covers_from)
    ) WITHOUT ROWID;

    CREATE INDEX IF NOT EXISTS journal_snapshots_taken
        ON journal_snapshots (mode, taken_at_ms DESC);
    -- Which checkpoint covers a given revision. The read the pruner does to
    -- find its watermark, and the one a warm start does to find where to
    -- replay from.
    CREATE INDEX IF NOT EXISTS journal_snapshots_revision
        ON journal_snapshots (mode, revision DESC);

    -- The chain is only tamper-evidence if the links cannot be re-tied. A
    -- snapshot that could be updated in place could be updated together with
    -- its own digest, and the walk would pass.
    CREATE TRIGGER IF NOT EXISTS journal_snapshots_are_immutable
        BEFORE UPDATE ON journal_snapshots
    BEGIN
        SELECT RAISE(ABORT, 'a snapshot cannot be rewritten');
    END;
";

// ---------------------------------------------------------------------------
// the vocabulary the columns are checked against
// ---------------------------------------------------------------------------

/// What became of one launch the gate read.
///
/// Three arms rather than the gate's own two, because `enter` is a verdict
/// about the launch and this is a record of what the engine did about it. A
/// launch the gate accepted and the risk breaker then refused is neither an
/// entry nor a rejection by the strategy, and folding it into either one is how
/// a funnel comes to blame the rule for a decision the account made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Decision {
    /// A position was opened. The only arm that carries an intent id.
    Entered,
    /// The gate said no.
    Refused,
    /// The gate said yes and nothing was opened: the window had not closed, the
    /// risk gate was shut, the run was stopping, or execution was off.
    Deferred,
}

impl Decision {
    pub const fn as_str(self) -> &'static str {
        match self {
            Decision::Entered => "entered",
            Decision::Refused => "refused",
            Decision::Deferred => "deferred",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "entered" => Some(Decision::Entered),
            "refused" => Some(Decision::Refused),
            "deferred" => Some(Decision::Deferred),
            _ => None,
        }
    }

    /// Every arm, in the order a funnel prints them.
    pub const ALL: [Decision; 3] = [Decision::Entered, Decision::Deferred, Decision::Refused];
}

// ---------------------------------------------------------------------------
// what a forensic row is
// ---------------------------------------------------------------------------

/// One launch, what the gate made of it, and what the risk gate was saying at
/// the same instant.
///
/// No revision field. The revision is allocated by the writer inside the
/// transaction that inserts the row, so a record that carried one would be
/// carrying a guess — and a caller that could set it could set it twice.
/// [`StateRow`] is this with the revision the file actually gave it.
///
/// The two `String`s are the only allocations, they happen once when the record
/// is built, and they are unavoidable: the record crosses a channel to a writer
/// thread and cannot borrow from the tick that made it. Everything else is an
/// integer in a named unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateRecord {
    pub mint: String,
    /// Set on [`Decision::Entered`] and on nothing else. The column enforces it.
    pub intent_id: Option<String>,

    pub decision: Decision,
    pub reason: GateReason,
    pub confidence_micros: u32,

    pub buyers: u32,
    pub bundle_wallets: u32,
    pub cohort_wallets: u32,
    pub cohort_lamports: u64,
    /// What was in the curve when the decision was made.
    pub pool_lamports: u64,

    pub operating_mode: OperatingMode,
    pub entries_allowed: bool,
    pub equity_lamports: u64,
    pub drawdown_bps: u16,
    pub open_positions: u16,
    pub free_slots: u16,

    pub window_closed: bool,
    /// The newest event that reached the record the gate read. Never later than
    /// `decided_at_ms`; the column refuses it if it is.
    pub evidence_to_ms: i64,
    pub decided_at_ms: i64,
}

impl StateRecord {
    /// Builds a record from the two values the decision was actually made out
    /// of, rather than from eighteen arguments in an order nobody can check.
    ///
    /// The risk half is taken whole from the snapshot the gate was evaluated
    /// against — including `entries_allowed` and `free_slots`, which are
    /// derived rather than passed, so a row cannot claim a permission its own
    /// numbers disagree with.
    ///
    /// The decision is passed in rather than inferred from `verdict.enter`,
    /// because only the caller knows whether the position was actually opened.
    /// [`Decision::Entered`] without an intent id is refused by the column, so
    /// a caller that gets this wrong finds out at the write rather than in a
    /// report six weeks later.
    #[allow(clippy::too_many_arguments)]
    pub fn decided(
        mint: impl Into<String>,
        verdict: &GateVerdict,
        risk: &RiskSnapshot,
        decision: Decision,
        intent_id: Option<String>,
        buyers: u32,
        pool_lamports: u64,
        window_closed: bool,
        evidence_to_ms: i64,
        decided_at_ms: i64,
    ) -> Self {
        StateRecord {
            mint: mint.into(),
            intent_id,
            decision,
            reason: verdict.reason,
            // Saturating rather than erroring: the column bounds this to a
            // whole unit in millionths and the gate cannot produce more, so a
            // larger number is a bug upstream whose right treatment is a row
            // that says "as confident as it gets" rather than a lost record.
            confidence_micros: verdict
                .confidence_micros
                .min(u64::from(crate::types::MICROS_DENOMINATOR))
                as u32,
            buyers,
            bundle_wallets: verdict.bundle_wallets,
            cohort_wallets: verdict.cohort_wallets,
            cohort_lamports: verdict.cohort_lamports,
            pool_lamports,
            operating_mode: risk.mode,
            entries_allowed: risk.entries_allowed(),
            equity_lamports: risk.equity_lamports,
            drawdown_bps: risk.drawdown_bps,
            open_positions: risk.open_positions,
            free_slots: risk.free_slots(),
            window_closed,
            evidence_to_ms,
            decided_at_ms,
        }
    }
}

/// A [`StateRecord`] with the revision the file gave it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateRow {
    pub mode: ExecutionMode,
    pub revision: u64,
    #[serde(flatten)]
    pub record: StateRecord,
}

/// The revisions one batch was written under. Half-open at neither end: `first`
/// and `last` are both rows that exist.
///
/// Empty batches never produce one of these — [`Database::record_state_log`]
/// returns `None` rather than an empty range, because a range whose `first` is
/// larger than its `last` is a shape every caller then has to remember to check
/// for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevisionRange {
    pub first: u64,
    pub last: u64,
}

impl RevisionRange {
    pub const fn len(&self) -> u64 {
        self.last - self.first + 1
    }

    /// Never true. A range is only built around at least one row, and the
    /// method is here because clippy asks for it beside `len`.
    pub const fn is_empty(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// asking the log a question
// ---------------------------------------------------------------------------

/// How many rows one page of the log holds when the caller does not say.
pub const DEFAULT_STATE_LIMIT: u32 = 500;

/// The most any one query will return. The same argument `journal::MAX_LIMIT`
/// makes, at a table that grows faster.
pub const MAX_STATE_LIMIT: u32 = 20_000;

/// What to filter the forensic log by.
///
/// `mode` is not optional. Every index on the table leads with it, the counter
/// is per mode, and a query across all three would be a page whose rows come
/// from three independent revision sequences — an ordering that means nothing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateLogFilter {
    pub mode: ExecutionMode,
    pub mint: Option<String>,
    pub decision: Option<Decision>,
    pub reason: Option<GateReason>,
    /// Inclusive, against `decided_at_ms`.
    pub since_ms: Option<i64>,
    /// Inclusive.
    pub until_ms: Option<i64>,
    /// Inclusive, against `revision`. The filter a replay uses to read
    /// everything a snapshot does not already account for.
    pub since_revision: Option<u64>,
    pub until_revision: Option<u64>,
    /// Newest first when false, which is what a person reading a cockpit wants.
    /// True for a replay, which has to apply the log in the order it happened.
    pub ascending: bool,
    pub limit: u32,
    pub offset: u32,
}

impl StateLogFilter {
    /// Everything in one mode, newest first.
    pub fn in_mode(mode: ExecutionMode) -> Self {
        StateLogFilter {
            mode,
            mint: None,
            decision: None,
            reason: None,
            since_ms: None,
            until_ms: None,
            since_revision: None,
            until_revision: None,
            ascending: false,
            limit: DEFAULT_STATE_LIMIT,
            offset: 0,
        }
    }

    /// Everything a snapshot at `revision` does not account for, oldest first.
    /// The read a warm start does to catch up.
    pub fn after_revision(mode: ExecutionMode, revision: u64) -> Self {
        StateLogFilter {
            since_revision: Some(revision.saturating_add(1)),
            ascending: true,
            limit: MAX_STATE_LIMIT,
            ..StateLogFilter::in_mode(mode)
        }
    }

    pub fn effective_limit(&self) -> u32 {
        if self.limit == 0 {
            DEFAULT_STATE_LIMIT
        } else {
            self.limit.min(MAX_STATE_LIMIT)
        }
    }

    /// The `WHERE` this filter means, and the parameters to bind to it.
    ///
    /// One `String` per query and none per row, and every value bound rather
    /// than formatted — `mint` in particular arrives from whatever the window
    /// put in a text box.
    fn where_clause(&self) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
        let mut clauses: Vec<&'static str> = vec!["mode = ?"];
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(self.mode.as_str())];

        if let Some(mint) = &self.mint {
            clauses.push("mint = ?");
            params.push(Box::new(mint.clone()));
        }
        if let Some(decision) = self.decision {
            clauses.push("decision = ?");
            params.push(Box::new(decision.as_str()));
        }
        if let Some(reason) = self.reason {
            clauses.push("reason = ?");
            params.push(Box::new(reason.as_str()));
        }
        if let Some(since) = self.since_ms {
            clauses.push("decided_at_ms >= ?");
            params.push(Box::new(since));
        }
        if let Some(until) = self.until_ms {
            clauses.push("decided_at_ms <= ?");
            params.push(Box::new(until));
        }
        if let Some(since) = self.since_revision {
            clauses.push("revision >= ?");
            params.push(Box::new(store_revision(since, "since_revision")));
        }
        if let Some(until) = self.until_revision {
            clauses.push("revision <= ?");
            params.push(Box::new(store_revision(until, "until_revision")));
        }

        (format!(" WHERE {}", clauses.join(" AND ")), params)
    }
}

/// How many launches reached each answer, over one slice of the log.
///
/// The counts a person actually asks for after a quiet day, and the shape
/// `daemon::Funnel` already prints: one entry per [`GateReason`], in
/// `GateReason::ALL` order, with a zero rather than a missing row for a reason
/// nobody hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateFunnel {
    pub mode: ExecutionMode,
    pub rows: i64,
    pub entered: i64,
    pub refused: i64,
    pub deferred: i64,
    /// Reason to count, in `GateReason::ALL` order.
    pub reasons: Vec<(String, i64)>,
    /// The revision range the counts cover. `None` when nothing matched.
    pub revisions: Option<RevisionRange>,
}

// ---------------------------------------------------------------------------
// the checkpoints
// ---------------------------------------------------------------------------

/// One checkpoint of the book, and its link in the chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotRow {
    pub mode: ExecutionMode,
    /// Which checkpoint this is, counting from one.
    pub seq: u64,
    /// The log revision this accounts for. Two consecutive checkpoints may name
    /// the same one — the book moves without the log whenever a position is
    /// written by a path that is not logging verdicts.
    pub revision: u64,
    pub taken_at_ms: i64,

    /// The book, whole. Cumulative, unlike the four counts below it.
    pub totals: JournalTotals,

    /// The revision the previous checkpoint stood at. This one speaks for
    /// `covers_from < r <= revision`.
    pub covers_from: u64,
    /// The log over that slice, and over that slice only. Deltas rather than
    /// running totals, so that retention deleting from the old end of the log
    /// cannot make a later checkpoint's arithmetic come out negative.
    pub rows_since: i64,
    pub entered_since: i64,
    pub refused_since: i64,
    pub deferred_since: i64,

    /// `None` on the first snapshot of a mode and on nothing else.
    pub prev_digest: Option<String>,
    pub digest: String,
}

impl SnapshotRow {
    /// The bytes the digest is taken over.
    ///
    /// One field per line, in a fixed order, each an integer in decimal or a
    /// single-character stand-in for a null. Deterministic across builds and
    /// machines because there is no float in it, no map iteration, and no
    /// serialiser: `serde_json` would be shorter and would also make the digest
    /// depend on a dependency's field ordering, which is a thing that can change
    /// under a `cargo update` and invalidate a chain nobody touched.
    ///
    /// `taken_at_ms` is in here deliberately. It makes the digest cover the
    /// whole row rather than most of it, so a timestamp cannot be edited
    /// without breaking the chain, and it costs nothing in replay — where the
    /// clock is fixture time and a second run produces the same number.
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = String::with_capacity(320);
        out.push_str("sts-journal-snapshot:v1\n");
        out.push_str(self.mode.as_str());
        out.push('\n');
        for value in [
            self.seq as i64,
            self.revision as i64,
            self.taken_at_ms,
            self.totals.trades,
            self.totals.closed,
            self.totals.notional_lamports,
            self.totals.cost_basis_lamports,
            self.totals.proceeds_lamports,
            self.totals.realized_pnl_lamports,
            self.totals.fee_lamports,
            self.totals.tip_lamports,
            self.covers_from as i64,
            self.rows_since,
            self.entered_since,
            self.refused_since,
            self.deferred_since,
        ] {
            out.push_str(itoa(value).as_str());
            out.push('\n');
        }
        match self.totals.worst_slippage_bps {
            Some(bps) => out.push_str(itoa(i64::from(bps)).as_str()),
            // A dash rather than an empty line: an empty line and a zero-length
            // number would be the same bytes, and 'nobody has filled' would
            // hash the same as a slippage somebody forgot to write.
            None => out.push('-'),
        }
        out.push('\n');
        match &self.prev_digest {
            Some(prev) => out.push_str(prev),
            // The genesis link. A named word rather than an empty string for
            // the same reason.
            None => out.push_str("genesis"),
        }
        out.push('\n');
        out.into_bytes()
    }

    /// What this row's digest should be, given everything else in it.
    fn compute_digest(&self) -> String {
        crate::replay::sha256_hex(&self.canonical_bytes())
    }
}

/// Whether the newest checkpoint still describes the book.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum SnapshotVerdict {
    /// No snapshot has ever been taken in this mode.
    None,
    /// The book is exactly what the snapshot says, and the log has not moved
    /// since. A warm start may trust the checkpoint and skip the scan.
    Matches { revision: u64 },
    /// The snapshot was true when it was taken and the log has moved on. Not a
    /// failure: this is what a checkpoint looks like from any moment after it.
    Superseded { revision: u64, now: u64 },
    /// The log has not moved and the book is not what the snapshot says.
    /// Somebody has edited the file underneath a running system.
    Diverged {
        revision: u64,
        recorded: Box<JournalTotals>,
        recomputed: Box<JournalTotals>,
    },
}

impl SnapshotVerdict {
    /// Whether this is the one arm that means something is wrong.
    pub const fn is_divergence(&self) -> bool {
        matches!(self, SnapshotVerdict::Diverged { .. })
    }
}

/// What walking the whole chain found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainReport {
    pub mode: ExecutionMode,
    pub snapshots: u64,
    /// Every link that did not verify, oldest first. Empty is the answer this
    /// is run for.
    pub breaks: Vec<ChainBreak>,
    /// Intervals where the snapshots' own counts were cross-checked against the
    /// rows actually in the log between them.
    pub intervals_checked: u64,
    /// Intervals that could not be checked because rows in them had been pruned.
    /// Not a break — a pruned interval is a deliberate act with a watermark, and
    /// the count is here so a report can say how much of the chain the
    /// cross-check actually covered rather than implying all of it.
    pub intervals_pruned: u64,
}

impl ChainReport {
    pub fn is_intact(&self) -> bool {
        self.breaks.is_empty()
    }
}

/// One link that did not verify, and what about it did not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainBreak {
    /// Which checkpoint. `seq` rather than `revision`, because two consecutive
    /// checkpoints may name the same revision and naming the broken one has to
    /// be unambiguous.
    pub seq: u64,
    pub revision: u64,
    pub detail: String,
}

/// What a process found when it opened the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WarmStart {
    pub mode: ExecutionMode,
    /// Where the counter is now. The first row this process writes is this
    /// plus one.
    pub revision: u64,
    /// The newest checkpoint, if there is one.
    pub snapshot: Option<SnapshotRow>,
    /// What that checkpoint is worth right now.
    pub verdict: SnapshotVerdict,
    /// Rows in the log the newest checkpoint does not account for. These are
    /// what a rebuild has to replay on top of it.
    pub uncheckpointed: u64,
    /// Whether the chain of checkpoints verified.
    pub chain: ChainReport,
}

impl WarmStart {
    /// Whether this file may be trusted without a rebuild.
    ///
    /// Deliberately strict. A broken chain or a divergence means something has
    /// edited the file, and Phase 0's answer to that is a documented safe mode
    /// rather than carrying on with a number that might be wrong.
    pub fn is_clean(&self) -> bool {
        self.chain.is_intact() && !self.verdict.is_divergence()
    }
}

// ---------------------------------------------------------------------------
// conversions
// ---------------------------------------------------------------------------

/// A revision on the way into an `INTEGER` column.
///
/// Saturating rather than erroring, and it is the one conversion here that is:
/// the counter would have to be incremented nine quintillion times to reach
/// this, at which point the file has other problems, and a `Result` on the read
/// path for a value that cannot occur is a `?` on every call site that buys
/// nothing.
fn store_revision(value: u64, _what: &str) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn load_revision(value: i64, column: &str) -> Result<u64, EngineError> {
    u64::try_from(value)
        .map_err(|_| EngineError::Database(format!("{column} holds {value}, which is negative")))
}

fn store_u64(value: u64, column: &str) -> Result<i64, EngineError> {
    i64::try_from(value).map_err(|_| {
        EngineError::Database(format!(
            "{column} is {value}, which is past what a column holds"
        ))
    })
}

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

/// An `i64` as decimal, without going through the formatting machinery.
///
/// `format!` here would be one allocation per field per snapshot. This is one
/// stack buffer and no allocation, which matters because the digest is
/// recomputed for every snapshot in the mode on every chain walk.
fn itoa(value: i64) -> String {
    let mut buffer = [0u8; 20];
    let negative = value < 0;
    // Through `u64` rather than negating, because `i64::MIN` has no positive.
    let mut magnitude = value.unsigned_abs();
    let mut index = buffer.len();
    loop {
        index -= 1;
        buffer[index] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    let digits = std::str::from_utf8(&buffer[index..]).unwrap_or("0");
    if negative {
        let mut out = String::with_capacity(digits.len() + 1);
        out.push('-');
        out.push_str(digits);
        out
    } else {
        digits.to_string()
    }
}

// ---------------------------------------------------------------------------
// writing and reading
// ---------------------------------------------------------------------------

/// Takes `count` revisions for `mode`, inside the caller's transaction.
///
/// The `UPDATE ... RETURNING` is one statement: the read and the write cannot be
/// separated by another writer, and there is only one writer anyway. Doing it
/// inside the caller's transaction is what makes the allocation gapless — a
/// batch that then fails rolls the counter back with it.
fn allocate_revisions(
    tx: &Transaction<'_>,
    mode: ExecutionMode,
    count: u64,
    now_ms: i64,
) -> Result<u64, EngineError> {
    let taken = store_u64(count, "revision count")?;
    let last: i64 = tx
        .prepare_cached(
            "UPDATE journal_revisions
                SET revision = revision + ?2, issued_at_ms = ?3
              WHERE stream = ?1
             RETURNING revision",
        )?
        .query_row(rusqlite::params![mode.as_str(), taken, now_ms], |row| {
            row.get(0)
        })
        .map_err(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => EngineError::Database(format!(
                "there is no revision counter for {} — migration 5 seeds all three, so this \
                 file has had one deleted",
                mode.as_str()
            )),
            other => EngineError::from(other),
        })?;

    let last = load_revision(last, "journal_revisions.revision")?;
    Ok(last - count + 1)
}

impl Database {
    /// The last revision issued in this mode. Zero means none.
    pub fn current_revision(&self, mode: ExecutionMode) -> Result<u64, EngineError> {
        let conn = self.connection();
        let value: i64 = conn
            .prepare_cached("SELECT revision FROM journal_revisions WHERE stream = ?1")?
            .query_row([mode.as_str()], |row| row.get(0))
            .map_err(|err| match err {
                rusqlite::Error::QueryReturnedNoRows => EngineError::Database(format!(
                    "there is no revision counter for {}",
                    mode.as_str()
                )),
                other => EngineError::from(other),
            })?;
        load_revision(value, "journal_revisions.revision")
    }

    /// Appends forensic rows and returns the revisions they were given.
    ///
    /// One transaction for the batch, one revision per row, allocated in order.
    /// `None` for an empty batch, which is not a transaction and does not move
    /// the counter.
    ///
    /// There is no `ON CONFLICT` and that is deliberate. Every other write in
    /// this build is idempotent because its key is deterministic and a replay
    /// can be written twice; this one's key is a counter, so the same record
    /// appended twice is genuinely two rows — two observations of the same
    /// launch — and a conflict is impossible rather than ignored. A caller that
    /// wants replay-idempotence writes into a fresh file, which is what replay
    /// already does.
    pub fn record_state_log(
        &self,
        mode: ExecutionMode,
        records: &[StateRecord],
        now_ms: i64,
    ) -> Result<Option<RevisionRange>, EngineError> {
        if records.is_empty() {
            return Ok(None);
        }

        let mut conn = self.connection();
        let tx = conn.transaction()?;
        let first = allocate_revisions(&tx, mode, records.len() as u64, now_ms)?;
        {
            let mut statement = tx.prepare_cached(
                "INSERT INTO journal_state_log (
                     mode, revision, mint, intent_id, decision, reason, confidence_micros,
                     buyers, bundle_wallets, cohort_wallets, cohort_lamports, pool_lamports,
                     operating_mode, entries_allowed, equity_lamports, drawdown_bps,
                     open_positions, free_slots, window_closed, evidence_to_ms, decided_at_ms
                 ) VALUES (
                     ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     ?17, ?18, ?19, ?20, ?21
                 )",
            )?;
            for (offset, record) in records.iter().enumerate() {
                statement.execute(rusqlite::params![
                    mode.as_str(),
                    store_revision(first + offset as u64, "revision"),
                    record.mint,
                    record.intent_id,
                    record.decision.as_str(),
                    record.reason.as_str(),
                    i64::from(record.confidence_micros),
                    i64::from(record.buyers),
                    i64::from(record.bundle_wallets),
                    i64::from(record.cohort_wallets),
                    store_u64(record.cohort_lamports, "cohort_lamports")?,
                    store_u64(record.pool_lamports, "pool_lamports")?,
                    record.operating_mode.as_str(),
                    i64::from(record.entries_allowed),
                    store_u64(record.equity_lamports, "equity_lamports")?,
                    i64::from(record.drawdown_bps),
                    i64::from(record.open_positions),
                    i64::from(record.free_slots),
                    i64::from(record.window_closed),
                    record.evidence_to_ms,
                    record.decided_at_ms,
                ])?;
            }
        }
        tx.commit()?;

        Ok(Some(RevisionRange {
            first,
            last: first + records.len() as u64 - 1,
        }))
    }

    /// Reads the log back.
    pub fn query_state_log(&self, filter: &StateLogFilter) -> Result<Vec<StateRow>, EngineError> {
        let (where_clause, params) = filter.where_clause();
        let sql = format!(
            "SELECT mode, revision, mint, intent_id, decision, reason, confidence_micros,
                    buyers, bundle_wallets, cohort_wallets, cohort_lamports, pool_lamports,
                    operating_mode, entries_allowed, equity_lamports, drawdown_bps,
                    open_positions, free_slots, window_closed, evidence_to_ms, decided_at_ms
               FROM journal_state_log{where_clause}
              ORDER BY revision {}
              LIMIT ?{} OFFSET ?{}",
            if filter.ascending { "ASC" } else { "DESC" },
            params.len() + 1,
            params.len() + 2,
        );

        let conn = self.connection();
        let mut statement = conn.prepare_cached(&sql)?;
        let mut bound: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let limit = i64::from(filter.effective_limit());
        let offset = i64::from(filter.offset);
        bound.push(&limit);
        bound.push(&offset);

        let rows = statement.query_map(bound.as_slice(), |row| Ok(read_state(row)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    /// The funnel over a slice of the log.
    ///
    /// Counted in SQL rather than by reading the rows: a month of refusals is
    /// several hundred thousand rows and the answer is thirteen integers.
    pub fn state_funnel(&self, filter: &StateLogFilter) -> Result<StateFunnel, EngineError> {
        let (where_clause, params) = filter.where_clause();
        let bound: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();

        let conn = self.connection();

        let mut reasons: Vec<(String, i64)> = GateReason::ALL
            .iter()
            .map(|reason| (reason.as_str().to_string(), 0))
            .collect();
        {
            let sql = format!(
                "SELECT reason, COUNT(*) FROM journal_state_log{where_clause} GROUP BY reason"
            );
            let mut statement = conn.prepare_cached(&sql)?;
            let rows = statement.query_map(bound.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (reason, count) = row?;
                if let Some(slot) = reasons.iter_mut().find(|(name, _)| name == &reason) {
                    slot.1 = count;
                }
            }
        }

        let sql = format!(
            "SELECT COUNT(*),
                    COALESCE(SUM(decision = 'entered'), 0),
                    COALESCE(SUM(decision = 'refused'), 0),
                    COALESCE(SUM(decision = 'deferred'), 0),
                    MIN(revision), MAX(revision)
               FROM journal_state_log{where_clause}"
        );
        let mut statement = conn.prepare_cached(&sql)?;
        let (rows, entered, refused, deferred, first, last) =
            statement.query_row(bound.as_slice(), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                ))
            })?;

        let revisions = match (first, last) {
            (Some(first), Some(last)) => Some(RevisionRange {
                first: load_revision(first, "journal_state_log.revision")?,
                last: load_revision(last, "journal_state_log.revision")?,
            }),
            _ => None,
        };

        Ok(StateFunnel {
            mode: filter.mode,
            rows,
            entered,
            refused,
            deferred,
            reasons,
            revisions,
        })
    }

    /// Takes a checkpoint of the book in one mode and links it to the last one.
    ///
    /// Everything happens inside one transaction, and it has to: the totals,
    /// the log counts and the counter's current value all have to be read from
    /// the same instant, or the snapshot describes a book that never existed.
    ///
    /// Taking a snapshot when nothing has changed returns the one that is
    /// already there rather than writing a second one. "Nothing has changed"
    /// means all three of the things a checkpoint states — the log revision,
    /// the book, and the counts over the log — and not just the revision: the
    /// exit path writes the book whether or not anything is logging verdicts,
    /// so a book that moved under a standing revision is a change and has to be
    /// recorded. A timer firing every five minutes against a genuinely quiet
    /// weekend writes nothing at all.
    pub fn take_journal_snapshot(
        &self,
        mode: ExecutionMode,
        now_ms: i64,
    ) -> Result<SnapshotRow, EngineError> {
        let mut conn = self.connection();
        let tx = conn.transaction()?;

        let revision: i64 = tx
            .prepare_cached("SELECT revision FROM journal_revisions WHERE stream = ?1")?
            .query_row([mode.as_str()], |row| row.get(0))?;
        let revision = load_revision(revision, "journal_revisions.revision")?;

        let totals = snapshot_totals(&tx, mode)?;
        let previous = read_latest_snapshot(&tx, mode)?;
        let covers_from = previous.as_ref().map(|p| p.revision).unwrap_or(0);

        if let Some(existing) = &previous {
            // Nothing has changed when the log has not moved and the book adds
            // up to what it did. Those two are the whole of what a checkpoint
            // states, so a pass finding both is a pass with nothing to record.
            // The interval counts need no comparison: an unmoved revision means
            // an empty interval, and an empty interval is zeroes.
            if existing.revision == revision && existing.totals == totals {
                return Ok(existing.clone());
            }
        }

        let (rows_since, entered_since, refused_since, deferred_since) =
            log_counts_between(&tx, mode, covers_from, revision)?;

        let mut row = SnapshotRow {
            mode,
            // Monotonic per mode, allocated in the transaction that inserts it.
            // `MAX + 1` rather than a counter of its own, because the primary
            // key already refuses a repeat and there is one writer: the read
            // and the insert cannot be separated by another.
            seq: previous.as_ref().map(|p| p.seq).unwrap_or(0) + 1,
            revision,
            taken_at_ms: now_ms,
            totals,
            covers_from,
            rows_since,
            entered_since,
            refused_since,
            deferred_since,
            prev_digest: previous.map(|p| p.digest),
            digest: String::new(),
        };
        row.digest = row.compute_digest();

        tx.prepare_cached(
            "INSERT INTO journal_snapshots (
                 mode, seq, revision, taken_at_ms, trades, closed, notional_lamports,
                 cost_basis_lamports, proceeds_lamports, realized_pnl_lamports,
                 fee_lamports, tip_lamports, worst_slippage_bps,
                 covers_from, rows_since, entered_since, refused_since, deferred_since,
                 prev_digest, digest
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                 ?18, ?19, ?20
             )",
        )?
        .execute(rusqlite::params![
            row.mode.as_str(),
            store_revision(row.seq, "seq"),
            store_revision(row.revision, "revision"),
            row.taken_at_ms,
            row.totals.trades,
            row.totals.closed,
            row.totals.notional_lamports,
            row.totals.cost_basis_lamports,
            row.totals.proceeds_lamports,
            row.totals.realized_pnl_lamports,
            row.totals.fee_lamports,
            row.totals.tip_lamports,
            row.totals.worst_slippage_bps.map(i64::from),
            store_revision(row.covers_from, "covers_from"),
            row.rows_since,
            row.entered_since,
            row.refused_since,
            row.deferred_since,
            row.prev_digest,
            row.digest,
        ])?;

        tx.commit()?;
        Ok(row)
    }

    /// The newest checkpoint in one mode, if there is one.
    pub fn latest_journal_snapshot(
        &self,
        mode: ExecutionMode,
    ) -> Result<Option<SnapshotRow>, EngineError> {
        let conn = self.connection();
        let mut statement = conn.prepare_cached(
            "SELECT mode, seq, revision, taken_at_ms, trades, closed, notional_lamports,
                    cost_basis_lamports, proceeds_lamports, realized_pnl_lamports,
                    fee_lamports, tip_lamports, worst_slippage_bps,
                    covers_from, rows_since, entered_since, refused_since, deferred_since,
                    prev_digest, digest
               FROM journal_snapshots
              WHERE mode = ?1
              ORDER BY seq DESC LIMIT 1",
        )?;
        let row = statement
            .query_row([mode.as_str()], |row| Ok(read_snapshot(row)))
            .optional()?;
        row.transpose()
    }

    /// Every checkpoint in one mode, oldest first.
    pub fn journal_snapshots(&self, mode: ExecutionMode) -> Result<Vec<SnapshotRow>, EngineError> {
        let conn = self.connection();
        let mut statement = conn.prepare_cached(
            "SELECT mode, seq, revision, taken_at_ms, trades, closed, notional_lamports,
                    cost_basis_lamports, proceeds_lamports, realized_pnl_lamports,
                    fee_lamports, tip_lamports, worst_slippage_bps,
                    covers_from, rows_since, entered_since, refused_since, deferred_since,
                    prev_digest, digest
               FROM journal_snapshots
              WHERE mode = ?1
              ORDER BY seq ASC",
        )?;
        let rows = statement.query_map([mode.as_str()], |row| Ok(read_snapshot(row)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row??);
        }
        Ok(out)
    }

    /// Recomputes the book and compares it against the newest checkpoint.
    ///
    /// Conclusive only while the log has not moved past the snapshot; see
    /// [`SnapshotVerdict`] for what the three arms mean.
    pub fn verify_journal_snapshot(
        &self,
        mode: ExecutionMode,
    ) -> Result<SnapshotVerdict, EngineError> {
        // All three reads under one guard, and that is not tidiness. The
        // interesting comparison is "the revision has not moved *and* the book
        // does not match", so a writer landing between reading the revision and
        // adding the book up would produce a `Diverged` out of an ordinary
        // trade — the loudest verdict this returns, from the most routine thing
        // that happens. One writer, one lock: holding it across the three makes
        // the race impossible rather than unlikely.
        let conn = self.connection();

        let Some(snapshot) = read_latest(&conn, mode)? else {
            return Ok(SnapshotVerdict::None);
        };
        let now = read_revision(&conn, mode)?;
        if now != snapshot.revision {
            return Ok(SnapshotVerdict::Superseded {
                revision: snapshot.revision,
                now,
            });
        }

        let recomputed = snapshot_totals(&conn, mode)?;
        if recomputed == snapshot.totals {
            Ok(SnapshotVerdict::Matches {
                revision: snapshot.revision,
            })
        } else {
            Ok(SnapshotVerdict::Diverged {
                revision: snapshot.revision,
                recorded: Box::new(snapshot.totals),
                recomputed: Box::new(recomputed),
            })
        }
    }

    /// Walks the chain of checkpoints and recomputes every link.
    ///
    /// Three things are checked per snapshot: that its digest is the digest of
    /// its own fields, that its `prev_digest` is the digest of the snapshot
    /// before it, and — where the log between the two revisions is still whole
    /// — that the entries and refusals it claims over that interval are the
    /// rows actually there.
    ///
    /// The third is the one that ties the checkpoint to the log rather than to
    /// itself, and it is why a snapshot carries counts it could have recomputed.
    pub fn verify_journal_snapshot_chain(
        &self,
        mode: ExecutionMode,
    ) -> Result<ChainReport, EngineError> {
        // One guard over the whole walk, for the reason above and one more: the
        // maintenance thread prunes the log on the same schedule it
        // checkpoints, and a prune landing between reading the snapshots and
        // counting the rows between them would show up here as an interval
        // that lost rows — a break, from retention working.
        let conn = self.connection();
        let snapshots = read_all_snapshots(&conn, mode)?;
        let mut report = ChainReport {
            mode,
            snapshots: snapshots.len() as u64,
            breaks: Vec::new(),
            intervals_checked: 0,
            intervals_pruned: 0,
        };
        let mut previous: Option<&SnapshotRow> = None;
        for snapshot in &snapshots {
            let expected = snapshot.compute_digest();
            if expected != snapshot.digest {
                report.breaks.push(ChainBreak {
                    seq: snapshot.seq,
                    revision: snapshot.revision,
                    detail: format!(
                        "the snapshot records digest {} and its own fields hash to {expected}",
                        snapshot.digest
                    ),
                });
            }

            // The sequence is the order the chain is in, so a hole in it is a
            // checkpoint that is not there any more — which the links below
            // would otherwise report only as a mismatched digest, blaming the
            // wrong row.
            let expected_seq = previous.map(|p| p.seq).unwrap_or(0) + 1;
            if snapshot.seq != expected_seq {
                report.breaks.push(ChainBreak {
                    seq: snapshot.seq,
                    revision: snapshot.revision,
                    detail: format!("checkpoint {expected_seq} is missing"),
                });
            }

            match (previous, &snapshot.prev_digest) {
                (None, Some(prev)) => report.breaks.push(ChainBreak {
                    seq: snapshot.seq,
                    revision: snapshot.revision,
                    detail: format!(
                        "this is the first snapshot in {} and it links back to {prev}",
                        mode.as_str()
                    ),
                }),
                (Some(before), None) => report.breaks.push(ChainBreak {
                    seq: snapshot.seq,
                    revision: snapshot.revision,
                    detail: format!(
                        "this snapshot links to nothing and checkpoint {} is before it",
                        before.seq
                    ),
                }),
                (Some(before), Some(prev)) if &before.digest != prev => {
                    report.breaks.push(ChainBreak {
                        seq: snapshot.seq,
                        revision: snapshot.revision,
                        detail: format!(
                            "this snapshot links to {prev} and checkpoint {} hashes to {}",
                            before.seq, before.digest
                        ),
                    });
                }
                _ => {}
            }

            // The slice this checkpoint speaks for has to be the one that
            // starts where the last one stopped. A `covers_from` that does not
            // is a checkpoint describing an interval nobody else's arithmetic
            // lines up with.
            let from = previous.map(|p| p.revision).unwrap_or(0);
            if snapshot.covers_from != from {
                report.breaks.push(ChainBreak {
                    seq: snapshot.seq,
                    revision: snapshot.revision,
                    detail: format!(
                        "this checkpoint covers from revision {} and the one before it stood at \
                         {from}",
                        snapshot.covers_from
                    ),
                });
            }

            // And the slice, counted against the log itself. This is the check
            // that ties a checkpoint to something other than itself, and it is
            // why the row carries counts it could have recomputed.
            let (rows, entered, refused, deferred) =
                log_counts_between(&conn, mode, snapshot.covers_from, snapshot.revision)?;
            if rows != snapshot.rows_since {
                // Fewer rows than the slice claims is what retention looks like
                // from here, and retention is a deliberate act with a watermark
                // in front of it. More rows than were ever claimed is not
                // anything this build can do.
                if rows < snapshot.rows_since {
                    report.intervals_pruned += 1;
                } else {
                    report.breaks.push(ChainBreak {
                        seq: snapshot.seq,
                        revision: snapshot.revision,
                        detail: format!(
                            "the slice after revision {} claims {} row(s) and the log holds {rows}",
                            snapshot.covers_from, snapshot.rows_since
                        ),
                    });
                }
            } else {
                report.intervals_checked += 1;
                let claimed = (
                    snapshot.entered_since,
                    snapshot.refused_since,
                    snapshot.deferred_since,
                );
                if claimed != (entered, refused, deferred) {
                    report.breaks.push(ChainBreak {
                        seq: snapshot.seq,
                        revision: snapshot.revision,
                        detail: format!(
                            "the slice after revision {} claims {}/{}/{} entered/refused/deferred \
                             and the log holds {entered}/{refused}/{deferred}",
                            snapshot.covers_from, claimed.0, claimed.1, claimed.2
                        ),
                    });
                }
            }

            previous = Some(snapshot);
        }

        Ok(report)
    }

    /// What a process should know about this file before it writes to it.
    pub fn warm_start(&self, mode: ExecutionMode) -> Result<WarmStart, EngineError> {
        // The verdict and the chain each take the lock for the length of their
        // own read, and the two are taken apart rather than together: holding
        // it across both would block the writer for the whole walk, and the
        // pair does not need to be atomic — a `superseded` verdict beside a
        // chain read a moment later still describes the same file, because
        // nothing either of them reports can be undone by a later write.
        //
        // `revision` and `snapshot` come from the verdict's own reading rather
        // than from two more queries, so `uncheckpointed` cannot come out
        // negative-by-a-race the way `current_revision() - snapshot.revision`
        // could if a batch landed between them.
        let verdict = self.verify_journal_snapshot(mode)?;
        let chain = self.verify_journal_snapshot_chain(mode)?;

        let conn = self.connection();
        let revision = read_revision(&conn, mode)?;
        let snapshot = read_latest(&conn, mode)?;
        drop(conn);

        let uncheckpointed =
            revision.saturating_sub(snapshot.as_ref().map(|s| s.revision).unwrap_or(0));

        Ok(WarmStart {
            mode,
            revision,
            snapshot,
            verdict,
            uncheckpointed,
            chain,
        })
    }

    /// Removes forensic rows older than `cutoff_ms`, in chunks.
    ///
    /// Two guards, and the second is the one worth stating. Nothing is removed
    /// above the newest checkpoint's revision, ever — a row a snapshot has not
    /// accounted for is a row whose disappearance would make the chain's
    /// interval check wrong rather than pruned, and a retention policy must not
    /// be able to break the integrity check that runs beside it. A mode with no
    /// snapshot therefore prunes nothing at all, which is the correct reading
    /// of "there is no checkpoint to be behind".
    pub fn prune_state_log(
        &self,
        mode: ExecutionMode,
        cutoff_ms: i64,
    ) -> Result<usize, EngineError> {
        let Some(snapshot) = self.latest_journal_snapshot(mode)? else {
            return Ok(0);
        };
        let watermark = store_revision(snapshot.revision, "revision");

        let mut removed = 0usize;
        loop {
            let mut conn = self.connection();
            let tx = conn.transaction()?;
            // By revision rather than by `rowid`: the table is `WITHOUT ROWID`
            // and has none. `(mode, revision)` is the primary key, so this is
            // the same lookup the rowid form would have done and it goes
            // straight down the key.
            let chunk = tx
                .prepare_cached(
                    "DELETE FROM journal_state_log
                      WHERE mode = ?1 AND revision IN (
                          SELECT revision FROM journal_state_log
                           WHERE mode = ?1 AND decided_at_ms < ?2 AND revision <= ?3
                           ORDER BY revision
                           LIMIT ?4
                      )",
                )?
                .execute(rusqlite::params![
                    mode.as_str(),
                    cutoff_ms,
                    watermark,
                    PRUNE_CHUNK as i64
                ])?;
            tx.commit()?;
            removed += chunk;
            if chunk < PRUNE_CHUNK {
                return Ok(removed);
            }
        }
    }
}

/// How many rows one retention statement removes before committing. The same
/// number and the same argument `db.rs` makes for `tick_metrics`: a week
/// deleted in one transaction bloats the WAL and holds the writer.
const PRUNE_CHUNK: usize = 4_000;

/// The book, added up, in one mode.
///
/// A free function over a `Connection` rather than a method, so the snapshot
/// path can call it inside its own transaction and the verify path can call it
/// outside one, without either taking the lock twice.
fn snapshot_totals(
    conn: &rusqlite::Connection,
    mode: ExecutionMode,
) -> Result<JournalTotals, EngineError> {
    let mut statement = conn.prepare_cached(
        "SELECT COUNT(*),
                COALESCE(SUM(closed_at_ms IS NOT NULL), 0),
                COALESCE(SUM(notional_lamports), 0),
                COALESCE(SUM(cost_basis_lamports), 0),
                COALESCE(SUM(proceeds_lamports), 0),
                COALESCE(SUM(realized_pnl_lamports), 0),
                COALESCE(SUM(fee_lamports), 0),
                COALESCE(SUM(tip_lamports), 0),
                MAX(slippage_bps)
           FROM journal_trades
          WHERE mode = ?1",
    )?;
    statement
        .query_row([mode.as_str()], |row| {
            Ok(JournalTotals {
                trades: row.get(0)?,
                closed: row.get(1)?,
                notional_lamports: row.get(2)?,
                cost_basis_lamports: row.get(3)?,
                proceeds_lamports: row.get(4)?,
                realized_pnl_lamports: row.get(5)?,
                fee_lamports: row.get(6)?,
                tip_lamports: row.get(7)?,
                worst_slippage_bps: row.get::<_, Option<i64>>(8)?.map(|bps| bps as u16),
            })
        })
        .map_err(EngineError::from)
}

/// Rows, entries, refusals and deferrals in the log over `from < revision <= to`.
fn log_counts_between(
    conn: &rusqlite::Connection,
    mode: ExecutionMode,
    from: u64,
    to: u64,
) -> Result<(i64, i64, i64, i64), EngineError> {
    let mut statement = conn.prepare_cached(
        "SELECT COUNT(*),
                COALESCE(SUM(decision = 'entered'), 0),
                COALESCE(SUM(decision = 'refused'), 0),
                COALESCE(SUM(decision = 'deferred'), 0)
           FROM journal_state_log
          WHERE mode = ?1 AND revision > ?2 AND revision <= ?3",
    )?;
    statement
        .query_row(
            rusqlite::params![
                mode.as_str(),
                store_revision(from, "from"),
                store_revision(to, "to")
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(EngineError::from)
}

/// The newest checkpoint in one mode, inside the caller's transaction.
///
/// `take_journal_snapshot` reads the previous link and writes the next one
/// without letting go of the lock in between, which is what makes `seq`
/// allocation by `MAX + 1` safe.
fn read_latest_snapshot(
    tx: &Transaction<'_>,
    mode: ExecutionMode,
) -> Result<Option<SnapshotRow>, EngineError> {
    read_latest(tx, mode)
}

/// The newest checkpoint in one mode, over any connection the caller is already
/// holding.
///
/// A free function rather than a method for the reason `snapshot_totals` is
/// one: the verification paths take the lock once and do several reads under
/// it, and a method would take it again.
fn read_latest(
    conn: &rusqlite::Connection,
    mode: ExecutionMode,
) -> Result<Option<SnapshotRow>, EngineError> {
    let mut statement = conn.prepare_cached(
        "SELECT mode, seq, revision, taken_at_ms, trades, closed, notional_lamports,
                cost_basis_lamports, proceeds_lamports, realized_pnl_lamports,
                fee_lamports, tip_lamports, worst_slippage_bps,
                covers_from, rows_since, entered_since, refused_since, deferred_since,
                prev_digest, digest
           FROM journal_snapshots
          WHERE mode = ?1
          ORDER BY seq DESC LIMIT 1",
    )?;
    let row = statement
        .query_row([mode.as_str()], |row| Ok(read_snapshot(row)))
        .optional()?;
    row.transpose()
}

/// Every checkpoint in one mode, oldest first, over a connection the caller
/// holds.
fn read_all_snapshots(
    conn: &rusqlite::Connection,
    mode: ExecutionMode,
) -> Result<Vec<SnapshotRow>, EngineError> {
    let mut statement = conn.prepare_cached(
        "SELECT mode, seq, revision, taken_at_ms, trades, closed, notional_lamports,
                cost_basis_lamports, proceeds_lamports, realized_pnl_lamports,
                fee_lamports, tip_lamports, worst_slippage_bps,
                covers_from, rows_since, entered_since, refused_since, deferred_since,
                prev_digest, digest
           FROM journal_snapshots
          WHERE mode = ?1
          ORDER BY seq ASC",
    )?;
    let rows = statement.query_map([mode.as_str()], |row| Ok(read_snapshot(row)))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

/// The counter, over a connection the caller holds.
fn read_revision(conn: &rusqlite::Connection, mode: ExecutionMode) -> Result<u64, EngineError> {
    let value: i64 = conn
        .prepare_cached("SELECT revision FROM journal_revisions WHERE stream = ?1")?
        .query_row([mode.as_str()], |row| row.get(0))
        .map_err(|err| match err {
            rusqlite::Error::QueryReturnedNoRows => EngineError::Database(format!(
                "there is no revision counter for {}",
                mode.as_str()
            )),
            other => EngineError::from(other),
        })?;
    load_revision(value, "journal_revisions.revision")
}

// ---------------------------------------------------------------------------
// rows back into models
// ---------------------------------------------------------------------------

fn read_state(row: &Row<'_>) -> Result<StateRow, EngineError> {
    let mode: String = row.get(0)?;
    let decision: String = row.get(4)?;
    let reason: String = row.get(5)?;
    let operating_mode: String = row.get(12)?;

    Ok(StateRow {
        mode: stored_as(&mode, ExecutionMode::parse, "journal_state_log.mode")?,
        revision: load_revision(row.get(1)?, "journal_state_log.revision")?,
        record: StateRecord {
            mint: row.get(2)?,
            intent_id: row.get(3)?,
            decision: stored_as(&decision, Decision::parse, "journal_state_log.decision")?,
            reason: stored_as(&reason, GateReason::parse, "journal_state_log.reason")?,
            confidence_micros: load_u32(row.get(6)?, "journal_state_log.confidence_micros")?,
            buyers: load_u32(row.get(7)?, "journal_state_log.buyers")?,
            bundle_wallets: load_u32(row.get(8)?, "journal_state_log.bundle_wallets")?,
            cohort_wallets: load_u32(row.get(9)?, "journal_state_log.cohort_wallets")?,
            cohort_lamports: load_u64(row.get(10)?, "journal_state_log.cohort_lamports")?,
            pool_lamports: load_u64(row.get(11)?, "journal_state_log.pool_lamports")?,
            operating_mode: stored_as(
                &operating_mode,
                operating_mode_from_str,
                "journal_state_log.operating_mode",
            )?,
            entries_allowed: row.get::<_, i64>(13)? != 0,
            equity_lamports: load_u64(row.get(14)?, "journal_state_log.equity_lamports")?,
            drawdown_bps: load_u16(row.get(15)?, "journal_state_log.drawdown_bps")?,
            open_positions: load_u16(row.get(16)?, "journal_state_log.open_positions")?,
            free_slots: load_u16(row.get(17)?, "journal_state_log.free_slots")?,
            window_closed: row.get::<_, i64>(18)? != 0,
            evidence_to_ms: row.get(19)?,
            decided_at_ms: row.get(20)?,
        },
    })
}

fn read_snapshot(row: &Row<'_>) -> Result<SnapshotRow, EngineError> {
    let mode: String = row.get(0)?;
    Ok(SnapshotRow {
        mode: stored_as(&mode, ExecutionMode::parse, "journal_snapshots.mode")?,
        seq: load_revision(row.get(1)?, "journal_snapshots.seq")?,
        revision: load_revision(row.get(2)?, "journal_snapshots.revision")?,
        taken_at_ms: row.get(3)?,
        totals: JournalTotals {
            trades: row.get(4)?,
            closed: row.get(5)?,
            notional_lamports: row.get(6)?,
            cost_basis_lamports: row.get(7)?,
            proceeds_lamports: row.get(8)?,
            realized_pnl_lamports: row.get(9)?,
            fee_lamports: row.get(10)?,
            tip_lamports: row.get(11)?,
            worst_slippage_bps: row
                .get::<_, Option<i64>>(12)?
                .map(|bps| load_u16(bps, "journal_snapshots.worst_slippage_bps"))
                .transpose()?,
        },
        covers_from: load_revision(row.get(13)?, "journal_snapshots.covers_from")?,
        rows_since: row.get(14)?,
        entered_since: row.get(15)?,
        refused_since: row.get(16)?,
        deferred_since: row.get(17)?,
        prev_digest: row.get(18)?,
        digest: row.get(19)?,
    })
}

/// `OperatingMode` has no `from_str` of its own — nothing had needed to read one
/// back out of a column until this table stored one.
fn operating_mode_from_str(text: &str) -> Option<OperatingMode> {
    match text {
        "live" => Some(OperatingMode::Live),
        "paper" => Some(OperatingMode::Paper),
        "replay" => Some(OperatingMode::Replay),
        "halted" => Some(OperatingMode::Halted),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// the high-throughput end
// ---------------------------------------------------------------------------

/// How many records the queue holds before it starts dropping.
///
/// Sized against the shape of the load rather than picked round. A busy morning
/// on pump.fun is a few launches a second; the writer commits a batch in
/// single-digit milliseconds, so the queue only fills if the writer is blocked
/// behind another writer for seconds at a time. Four thousand slots is roughly
/// twenty minutes of that, which is longer than any checkpoint or retention
/// pass this build runs.
pub const DEFAULT_QUEUE_DEPTH: usize = 4_096;

/// The most rows one transaction writes.
///
/// The batch is what makes this cheap — one `fsync` for a thousand rows rather
/// than a thousand — and the cap is what stops a backlog turning into one
/// enormous transaction that holds the writer and bloats the WAL. Everything
/// past it goes in the next batch, on the next turn of the same loop.
pub const MAX_BATCH: usize = 512;

/// How long the writer waits for a batch to fill before writing what it has.
///
/// The latency floor on a quiet feed: one record arriving alone is on disk
/// within this. Short enough that the log is useful during an incident, long
/// enough that a steady trickle still batches.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(250);

/// What the writer has done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateLoggerStats {
    /// Records accepted onto the queue.
    pub queued: u64,
    /// Records thrown away because the queue was full. Non-zero means the
    /// writer is behind the engine, and every one of them is a decision whose
    /// reasoning is not on disk.
    pub dropped: u64,
    /// Records committed.
    pub written: u64,
    /// Transactions committed. `written / batches` is the batching actually
    /// achieved, which is the number to look at when the writer is behind.
    pub batches: u64,
    /// Batches that failed to commit. Their records are lost and counted here
    /// rather than retried: a batch that will not commit is usually a schema or
    /// a disk problem, and retrying it forever behind a filling queue turns one
    /// failure into total loss.
    pub failed: u64,
    /// Records on the queue right now.
    pub depth: u64,
    /// The last revision the writer committed. Zero before the first batch.
    pub last_revision: u64,
    pub last_write_at_ms: Option<i64>,
    /// False once the writer has been joined.
    pub running: bool,
}

/// The counters the worker owns and the logger reports.
#[derive(Debug, Default)]
struct LoggerCounters {
    queued: AtomicU64,
    dropped: AtomicU64,
    written: AtomicU64,
    /// Records that have left the queue and reached an outcome, whichever it
    /// was. `written` plus whatever a failed batch took with it, and the number
    /// `flush` waits on — a caller wanting to know the table is settled is not
    /// helped by waiting forever for a batch that will never commit.
    settled: AtomicU64,
    batches: AtomicU64,
    failed: AtomicU64,
    last_revision: AtomicU64,
    last_write_at_ms: AtomicI64,
    running: AtomicBool,
}

/// The forensic log's writer: a bounded queue and one thread behind it.
///
/// [`StateLogger::observe`] is a `try_send` and a counter increment. Everything
/// that can block — the transaction, the `fsync`, the wait on the one writer
/// lock every other module queues on — happens on the worker.
///
/// A thread rather than a tokio task, for the reason the maintenance loop and
/// the ingestion WAL worker both give: SQLite is synchronous, and a commit on a
/// runtime worker blocks every socket sharing that thread.
pub struct StateLogger {
    mode: ExecutionMode,
    tx: Sender<StateRecord>,
    shutdown: Sender<()>,
    /// Pinged by the worker after each committed batch, so `flush` can wait for
    /// one rather than sleeping a guess.
    flushed: Receiver<()>,
    worker: Mutex<Option<JoinHandle<()>>>,
    counters: Arc<LoggerCounters>,
}

impl std::fmt::Debug for StateLogger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateLogger")
            .field("mode", &self.mode.as_str())
            .field("stats", &self.stats())
            .finish()
    }
}

impl StateLogger {
    /// Starts the writer.
    pub fn start(db: Arc<Database>, mode: ExecutionMode) -> Arc<Self> {
        Self::with_capacity(db, mode, DEFAULT_QUEUE_DEPTH, FLUSH_INTERVAL)
    }

    /// `start`, with the queue depth and the flush interval named. The tests
    /// want a queue small enough to fill and an interval short enough to wait
    /// for; nothing else builds one of these by hand.
    pub fn with_capacity(
        db: Arc<Database>,
        mode: ExecutionMode,
        depth: usize,
        flush_every: Duration,
    ) -> Arc<Self> {
        let (tx, rx) = bounded::<StateRecord>(depth.max(1));
        let (shutdown, shutdown_rx) = bounded::<()>(1);
        // Depth 1 and a `try_send` that ignores a full channel: this is a
        // wake-up, not a queue. A `flush` waiting on it wants to know that *a*
        // batch has committed since it asked, and a backlog of stale pings
        // would answer that question with a batch from before the call.
        let (flushed_tx, flushed) = bounded::<()>(1);
        let counters = Arc::new(LoggerCounters::default());
        counters.running.store(true, Ordering::Relaxed);

        let worker = std::thread::Builder::new()
            .name("sts-state-log".to_string())
            .spawn({
                let counters = Arc::clone(&counters);
                move || writer_loop(db, mode, rx, shutdown_rx, flushed_tx, flush_every, counters)
            })
            .expect("the forensic log is the only record of why the engine did not trade");

        Arc::new(StateLogger {
            mode,
            tx,
            shutdown,
            flushed,
            worker: Mutex::new(Some(worker)),
            counters,
        })
    }

    pub fn mode(&self) -> ExecutionMode {
        self.mode
    }

    /// Queues one record. Never blocks, never fails, counts what it drops.
    pub fn observe(&self, record: StateRecord) {
        if self.tx.try_send(record).is_err() {
            self.counters.dropped.fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters.queued.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn stats(&self) -> StateLoggerStats {
        StateLoggerStats {
            queued: self.counters.queued.load(Ordering::Relaxed),
            dropped: self.counters.dropped.load(Ordering::Relaxed),
            written: self.counters.written.load(Ordering::Relaxed),
            batches: self.counters.batches.load(Ordering::Relaxed),
            failed: self.counters.failed.load(Ordering::Relaxed),
            depth: self.tx.len() as u64,
            last_revision: self.counters.last_revision.load(Ordering::Relaxed),
            last_write_at_ms: match self.counters.last_write_at_ms.load(Ordering::Relaxed) {
                0 => None,
                at_ms => Some(at_ms),
            },
            running: self.counters.running.load(Ordering::Relaxed),
        }
    }

    /// Waits until every record queued before this call has been settled —
    /// committed, or counted as lost by a batch that would not commit.
    ///
    /// For the callers that would rather wait than read a half-written table: a
    /// shutdown, a test, an operator asking for a report.
    ///
    /// Against the settled count and not against an empty queue, and the
    /// difference is the whole value of the method. A record leaves the queue
    /// the moment the writer picks it up and is not on disk until the batch it
    /// joined commits, so "the queue is empty" is true for the entire length of
    /// the write it is waiting for. A caller that read the table on the
    /// strength of it would be racing the transaction.
    ///
    /// Bounded, because a writer that has died must not turn a report into a
    /// hang: it returns false if the target was not reached within `timeout`.
    pub fn flush(&self, timeout: Duration) -> bool {
        // Read before anything else. Records queued *after* this call are not
        // what the caller asked about, and waiting for them under a live feed
        // would be waiting forever.
        let target = self.counters.queued.load(Ordering::Relaxed);
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.counters.settled.load(Ordering::Relaxed) >= target {
                return true;
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return self.counters.settled.load(Ordering::Relaxed) >= target;
            }
            // A committed batch, or the interval — whichever comes first, so a
            // writer that is mid-batch is waited for rather than polled at.
            let _ = self.flushed.recv_timeout(left.min(FLUSH_INTERVAL));
        }
    }

    /// Stops the writer and waits for it to drain. Safe to call twice.
    ///
    /// The worker writes what is left on the queue before it returns, so a
    /// shutdown does not throw away the last batch — which, during an incident,
    /// is the batch that matters.
    pub fn stop(&self) {
        let Some(handle) = self.worker.lock().take() else {
            return;
        };
        let _ = self.shutdown.try_send(());
        let _ = handle.join();
        self.counters.running.store(false, Ordering::Relaxed);
    }
}

impl Drop for StateLogger {
    fn drop(&mut self) {
        self.stop();
    }
}

fn writer_loop(
    db: Arc<Database>,
    mode: ExecutionMode,
    rx: Receiver<StateRecord>,
    shutdown: Receiver<()>,
    flushed: Sender<()>,
    flush_every: Duration,
    counters: Arc<LoggerCounters>,
) {
    let mut batch: Vec<StateRecord> = Vec::with_capacity(MAX_BATCH);

    // Drains whatever is queued, writes it, and marks the writer stopped.
    // Called from both the ways this loop ends, because both of them mean the
    // same thing: nothing more is coming, and what is here is not to be thrown
    // away. During an incident the last batch is the one worth having.
    let finish = |batch: &mut Vec<StateRecord>| {
        for record in rx.try_iter() {
            batch.push(record);
            if batch.len() >= MAX_BATCH {
                commit(&db, mode, batch, &flushed, &counters);
            }
        }
        commit(&db, mode, batch, &flushed, &counters);
        counters.running.store(false, Ordering::Relaxed);
    };

    loop {
        // Nothing to do: block on both channels rather than polling, so a quiet
        // feed costs nothing at all.
        crossbeam_channel::select! {
            recv(rx) -> record => match record {
                Ok(record) => batch.push(record),
                // Every sender is gone, so nothing more can arrive.
                Err(_) => return finish(&mut batch),
            },
            recv(shutdown) -> _ => return finish(&mut batch),
        }

        // One record woke the loop; take whatever else is already waiting
        // rather than committing a batch of one.
        for record in rx.try_iter() {
            batch.push(record);
            if batch.len() >= MAX_BATCH {
                break;
            }
        }
        if batch.len() >= MAX_BATCH {
            commit(&db, mode, &mut batch, &flushed, &counters);
            continue;
        }

        // Under the batch size: wait out the interval to see whether more
        // arrives, and write what there is when it expires.
        //
        // The wait watches the shutdown channel as well as the queue, and that
        // is not a refinement. A wait that only watched the queue would make
        // every `stop` take a full interval — invisible at the shipped 250 ms
        // and a thirty-second hang for a caller that configured a longer one,
        // which is the shape of "closing the window stopped responding".
        let deadline = std::time::Instant::now() + flush_every;
        let mut stopping = false;
        while batch.len() < MAX_BATCH {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            crossbeam_channel::select! {
                recv(rx) -> record => match record {
                    Ok(record) => batch.push(record),
                    Err(_) => { stopping = true; break }
                },
                recv(shutdown) -> _ => { stopping = true; break },
                default(left) => break,
            }
        }
        if stopping {
            return finish(&mut batch);
        }
        commit(&db, mode, &mut batch, &flushed, &counters);
    }
}

/// Writes one batch and empties it, whatever happens.
///
/// The batch is cleared on failure as well as on success, and the records are
/// counted as lost rather than retried. See [`StateLoggerStats::failed`].
fn commit(
    db: &Database,
    mode: ExecutionMode,
    batch: &mut Vec<StateRecord>,
    flushed: &Sender<()>,
    counters: &LoggerCounters,
) {
    if batch.is_empty() {
        return;
    }
    let size = batch.len() as u64;
    let now_ms = crate::telemetry::now_ms();
    match db.record_state_log(mode, batch, now_ms) {
        Ok(Some(range)) => {
            counters.written.fetch_add(range.len(), Ordering::Relaxed);
            counters.batches.fetch_add(1, Ordering::Relaxed);
            counters.last_revision.store(range.last, Ordering::Relaxed);
            counters.last_write_at_ms.store(now_ms, Ordering::Relaxed);
        }
        Ok(None) => {}
        Err(_) => {
            counters.failed.fetch_add(1, Ordering::Relaxed);
        }
    }
    batch.clear();
    // Settled before the ping, never after: a `flush` woken by the ping reads
    // the counter, and the two in the other order would wake it to the number
    // it already had.
    counters.settled.fetch_add(size, Ordering::Relaxed);
    // A full channel means a `flush` has not collected the last ping yet, which
    // is exactly as good as this one.
    let _ = flushed.try_send(());
}

// ---------------------------------------------------------------------------
// what a process does with all this on the way up
// ---------------------------------------------------------------------------

/// The audit `event_type` written when a warm start finds something wrong.
///
/// A named constant because it is the row somebody greps for after an incident,
/// and because `docs/AUDIT_EVENTS.md` is the list it has to appear on.
pub const WARM_START_EVENT: &str = "warm_start_unclean";

/// Checks every mode's checkpoints on the way up, and says so if any of them
/// does not verify.
///
/// Called once, before the engine takes work. It repairs nothing and blocks
/// nothing: this build has no code path that can break a chain, so a break
/// means the file was edited by something outside it, and the useful response
/// is a loud, durable record rather than a guess at what the numbers should
/// have been. Phase 0's safe-mode ladder is what eventually reads this; until
/// that ladder exists, saying so precisely is the honest amount.
///
/// Returns one report per mode, in a fixed order, whether or not anything was
/// wrong — a caller that wants to gate on it has the same three answers every
/// time rather than a list whose length depends on the failure.
pub fn verify_on_start(
    db: &Database,
    hub: &crate::telemetry::TelemetryHub,
    now_ms: i64,
) -> Vec<WarmStart> {
    let mut reports = Vec::with_capacity(3);

    for mode in [
        ExecutionMode::Live,
        ExecutionMode::Paper,
        ExecutionMode::Replay,
    ] {
        let warm = match db.warm_start(mode) {
            Ok(warm) => warm,
            Err(err) => {
                // Not being able to read the checkpoints is itself the finding.
                hub.publish(
                    crate::telemetry::TelemetryLevel::Warn,
                    "forensics",
                    format!("the {} checkpoints could not be read", mode.as_str()),
                    serde_json::json!({ "mode": mode.as_str(), "error": err.to_string() }),
                );
                continue;
            }
        };

        if warm.is_clean() {
            // The ordinary case, and it is a `Debug` line rather than silence
            // because "the chain verified" is what an operator wants to see on
            // the way up, and "nothing was printed" cannot be told apart from
            // "the check did not run".
            hub.publish(
                crate::telemetry::TelemetryLevel::Debug,
                "forensics",
                format!(
                    "{}: revision {}, {} snapshot(s) verified, {} row(s) not yet checkpointed",
                    mode.as_str(),
                    warm.revision,
                    warm.chain.snapshots,
                    warm.uncheckpointed
                ),
                serde_json::json!({
                    "mode": mode.as_str(),
                    "revision": warm.revision,
                    "snapshots": warm.chain.snapshots,
                    "uncheckpointed": warm.uncheckpointed,
                    "intervalsChecked": warm.chain.intervals_checked,
                    "intervalsPruned": warm.chain.intervals_pruned,
                }),
            );
        } else {
            let payload = serde_json::json!({
                "mode": mode.as_str(),
                "revision": warm.revision,
                "snapshots": warm.chain.snapshots,
                "breaks": warm.chain.breaks,
                "verdict": warm.verdict,
            });
            hub.publish(
                crate::telemetry::TelemetryLevel::Warn,
                "forensics",
                format!(
                    "{}: the book does not match its own checkpoints — {} broken link(s)",
                    mode.as_str(),
                    warm.chain.breaks.len()
                ),
                payload.clone(),
            );
            // Durable as well as published: a telemetry line is gone when the
            // window closes, and this is a finding somebody has to still be
            // able to read tomorrow.
            let _ = db.record_audit(WARM_START_EVENT, &payload, now_ms);
        }

        reports.push(warm);
    }

    reports
}

// ===========================================================================
// tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    use crate::db::Side;
    use crate::journal::TradeRow;
    use crate::types::{BreakerReason, CircuitBreaker, FastPathGate, LiquidityThresholds};

    const AT_MS: i64 = 1_700_000_000_000;
    const MINT: &str = "So11111111111111111111111111111111111111112";

    /// A file per test, for the reason `journal.rs` gives about its own: the
    /// triggers, the `CHECK`s and the transaction boundaries are what is under
    /// test, and none of them mean anything against `:memory:`.
    struct TempDb(PathBuf);

    impl TempDb {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "sts-forensics-{name}-{}-{}.db",
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

    // -----------------------------------------------------------------------
    // fixtures
    // -----------------------------------------------------------------------

    fn risk(mode: OperatingMode, open: u16) -> RiskSnapshot {
        RiskSnapshot {
            at_ms: AT_MS,
            mode,
            equity_lamports: 200_000_000,
            high_water_lamports: 250_000_000,
            drawdown_bps: 2_000,
            max_drawdown_bps: 3_000,
            open_positions: open,
            max_open_positions: 3,
            circuit_breaker: CircuitBreaker::Clear,
            fast_path: FastPathGate::CLOSED,
            liquidity: LiquidityThresholds {
                min_pool_lamports: 10_000_000,
                exit_only_below_lamports: 5_000_000,
                max_pool_share_bps: 150,
            },
        }
    }

    fn verdict(reason: GateReason) -> GateVerdict {
        GateVerdict {
            enter: reason == GateReason::Accepted,
            reason,
            confidence_micros: 640_000,
            tags: Vec::new(),
            thin: false,
            bundle_wallets: 4,
            bundle_lamports: 3_000_000_000,
            cohort_wallets: 6,
            cohort_lamports: 4_500_000_000,
            cohort_size_lamports: None,
            cohort_delta_bps: None,
            cohort_external: 0,
            rings: Vec::new(),
            sandwich: None,
        }
    }

    /// A refusal, which is what most of the log is.
    fn refused(mint: &str, at_ms: i64) -> StateRecord {
        StateRecord::decided(
            mint,
            &verdict(GateReason::LowScore),
            &risk(OperatingMode::Paper, 0),
            Decision::Refused,
            None,
            9,
            80_000_000,
            true,
            at_ms - 50,
            at_ms,
        )
    }

    /// An entry, which is the rare one.
    fn entered(mint: &str, intent: &str, at_ms: i64) -> StateRecord {
        StateRecord::decided(
            mint,
            &verdict(GateReason::Accepted),
            &risk(OperatingMode::Paper, 1),
            Decision::Entered,
            Some(intent.to_string()),
            22,
            120_000_000,
            true,
            at_ms - 10,
            at_ms,
        )
    }

    /// The gate said yes and nothing was opened.
    fn deferred(mint: &str, at_ms: i64) -> StateRecord {
        StateRecord::decided(
            mint,
            &verdict(GateReason::Accepted),
            &risk(OperatingMode::Paper, 3),
            Decision::Deferred,
            None,
            18,
            90_000_000,
            false,
            at_ms - 5,
            at_ms,
        )
    }

    fn trade(id: &str, mode: ExecutionMode) -> TradeRow {
        TradeRow::opened(id, MINT, Side::Buy, mode, 500_000_000, AT_MS)
    }

    // -----------------------------------------------------------------------
    // the schema
    // -----------------------------------------------------------------------

    #[test]
    fn a_fresh_file_carries_the_forensic_tables() {
        let temp = TempDb::new("schema");
        let db = temp.open();
        assert_eq!(db.schema_version(), crate::db::latest_schema_version());

        let conn = db.connection();
        for table in [
            "journal_revisions",
            "journal_state_log",
            "journal_snapshots",
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
        for trigger in [
            "journal_revisions_only_go_forward",
            "journal_revisions_are_the_three_modes",
            "journal_state_log_is_append_only",
            "journal_snapshots_are_immutable",
        ] {
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND name = ?1",
                    [trigger],
                    |row| row.get(0),
                )
                .expect("asks");
            assert_eq!(found, 1, "{trigger} is missing");
        }
    }

    #[test]
    fn no_column_in_the_forensic_tables_is_a_float() {
        // The same claim `journal.rs` checks against its own five tables, made
        // against these three and checked against the file rather than against
        // the string that created it.
        let temp = TempDb::new("no-floats");
        let db = temp.open();
        let conn = db.connection();
        for table in [
            "journal_revisions",
            "journal_state_log",
            "journal_snapshots",
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
    fn the_three_counters_are_seeded_at_nothing() {
        let temp = TempDb::new("seeded");
        let db = temp.open();
        for mode in [
            ExecutionMode::Live,
            ExecutionMode::Paper,
            ExecutionMode::Replay,
        ] {
            assert_eq!(
                db.current_revision(mode).expect("reads"),
                0,
                "{} started somewhere other than zero",
                mode.as_str()
            );
        }
    }

    #[test]
    fn migrating_twice_is_migrating_once() {
        let temp = TempDb::new("idempotent");
        {
            let db = temp.open();
            db.record_state_log(ExecutionMode::Paper, &[refused("m", AT_MS)], AT_MS)
                .expect("writes");
            db.close();
        }
        let again = temp.open();
        assert_eq!(again.schema_version(), crate::db::latest_schema_version());
        // The reopen must not reseed the counter back to zero: `INSERT OR
        // IGNORE` is what keeps the second migration pass from erasing where
        // the first one got to.
        assert_eq!(
            again.current_revision(ExecutionMode::Paper).expect("reads"),
            1
        );
    }

    // -----------------------------------------------------------------------
    // the counter
    // -----------------------------------------------------------------------

    #[test]
    fn revisions_start_at_one_and_are_gapless() {
        let temp = TempDb::new("gapless");
        let db = temp.open();

        let first = db
            .record_state_log(ExecutionMode::Paper, &[refused("a", AT_MS)], AT_MS)
            .expect("writes")
            .expect("a batch of one is a range");
        assert_eq!(first, RevisionRange { first: 1, last: 1 });

        let batch: Vec<StateRecord> = (0..5)
            .map(|i| refused(&format!("m-{i}"), AT_MS + i))
            .collect();
        let second = db
            .record_state_log(ExecutionMode::Paper, &batch, AT_MS)
            .expect("writes")
            .expect("a range");
        assert_eq!(second, RevisionRange { first: 2, last: 6 });
        assert_eq!(second.len(), 5);
        assert_eq!(db.current_revision(ExecutionMode::Paper).expect("reads"), 6);

        // Every revision from 1 to 6 exists exactly once. This is the property
        // the whole design rests on: a hole is a missing row.
        let rows = db
            .query_state_log(&StateLogFilter::after_revision(ExecutionMode::Paper, 0))
            .expect("reads");
        let seen: Vec<u64> = rows.iter().map(|row| row.revision).collect();
        assert_eq!(seen, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn each_mode_counts_on_its_own() {
        // Phase 3 wants a replay whose records do not depend on what live
        // traffic happened to be flowing beside it. Three counters is how.
        let temp = TempDb::new("per-mode");
        let db = temp.open();

        db.record_state_log(ExecutionMode::Live, &[refused("a", AT_MS)], AT_MS)
            .expect("writes");
        db.record_state_log(ExecutionMode::Live, &[refused("b", AT_MS)], AT_MS)
            .expect("writes");
        let replay = db
            .record_state_log(ExecutionMode::Replay, &[refused("c", AT_MS)], AT_MS)
            .expect("writes")
            .expect("a range");

        assert_eq!(replay.first, 1, "replay's first row is replay's revision 1");
        assert_eq!(db.current_revision(ExecutionMode::Live).expect("reads"), 2);
        assert_eq!(
            db.current_revision(ExecutionMode::Replay).expect("reads"),
            1
        );
        assert_eq!(db.current_revision(ExecutionMode::Paper).expect("reads"), 0);
    }

    #[test]
    fn a_revision_cannot_go_backwards() {
        let temp = TempDb::new("monotonic");
        let db = temp.open();
        db.record_state_log(ExecutionMode::Paper, &[refused("a", AT_MS)], AT_MS)
            .expect("writes");

        let conn = db.connection();
        let err = conn
            .execute(
                "UPDATE journal_revisions SET revision = 0 WHERE stream = 'paper'",
                [],
            )
            .expect_err("the trigger refuses it");
        assert!(
            err.to_string().contains("cannot go backwards"),
            "the refusal did not say why: {err}"
        );

        // The same number is not forward either.
        let err = conn
            .execute(
                "UPDATE journal_revisions SET revision = 1 WHERE stream = 'paper'",
                [],
            )
            .expect_err("standing still is not going forward");
        assert!(err.to_string().contains("cannot go backwards"));
    }

    #[test]
    fn the_counters_are_the_three_modes_and_stay_that_way() {
        let temp = TempDb::new("three-modes");
        let db = temp.open();
        let conn = db.connection();
        let err = conn
            .execute("DELETE FROM journal_revisions WHERE stream = 'live'", [])
            .expect_err("the trigger refuses it");
        assert!(err.to_string().contains("are the three modes"));
    }

    #[test]
    fn an_empty_batch_does_not_move_the_counter() {
        let temp = TempDb::new("empty");
        let db = temp.open();
        assert_eq!(
            db.record_state_log(ExecutionMode::Paper, &[], AT_MS)
                .expect("writes"),
            None
        );
        assert_eq!(db.current_revision(ExecutionMode::Paper).expect("reads"), 0);
    }

    #[test]
    fn a_batch_that_fails_takes_its_revisions_back_with_it() {
        // The reason the allocation happens inside the caller's transaction.
        // A counter bumped outside it would leave a gap here, and a gap is
        // supposed to mean a missing row.
        let temp = TempDb::new("rollback");
        let db = temp.open();

        db.record_state_log(ExecutionMode::Paper, &[refused("a", AT_MS)], AT_MS)
            .expect("writes");

        // An entry with no intent id. The column refuses it, so the whole
        // batch — allocation included — rolls back.
        let mut bad = entered("b", "intent-1", AT_MS);
        bad.intent_id = None;
        let err = db
            .record_state_log(ExecutionMode::Paper, &[refused("ok", AT_MS), bad], AT_MS)
            .expect_err("the CHECK refuses it");
        assert!(matches!(err, EngineError::Database(_)));

        assert_eq!(
            db.current_revision(ExecutionMode::Paper).expect("reads"),
            1,
            "the failed batch left its revisions allocated"
        );

        // And the next write is revision 2, not revision 4.
        let next = db
            .record_state_log(ExecutionMode::Paper, &[refused("c", AT_MS)], AT_MS)
            .expect("writes")
            .expect("a range");
        assert_eq!(next, RevisionRange { first: 2, last: 2 });
    }

    // -----------------------------------------------------------------------
    // what the log will and will not hold
    // -----------------------------------------------------------------------

    #[test]
    fn every_row_comes_back_exactly_as_it_went_in() {
        let temp = TempDb::new("round-trip");
        let db = temp.open();

        let records = vec![
            refused("mint-a", AT_MS),
            entered("mint-b", "intent-b", AT_MS + 1),
            deferred("mint-c", AT_MS + 2),
        ];
        db.record_state_log(ExecutionMode::Paper, &records, AT_MS)
            .expect("writes");

        let back = db
            .query_state_log(&StateLogFilter::after_revision(ExecutionMode::Paper, 0))
            .expect("reads");
        assert_eq!(back.len(), 3);
        for (index, row) in back.iter().enumerate() {
            assert_eq!(row.mode, ExecutionMode::Paper);
            assert_eq!(row.revision, index as u64 + 1);
            assert_eq!(row.record, records[index], "row {index} came back changed");
        }
    }

    #[test]
    fn only_a_decision_that_entered_names_what_it_entered() {
        let temp = TempDb::new("intent-id");
        let db = temp.open();

        let mut orphan = entered("a", "intent-a", AT_MS);
        orphan.intent_id = None;
        assert!(
            db.record_state_log(ExecutionMode::Paper, &[orphan], AT_MS)
                .is_err(),
            "an entry with nothing to point at was accepted"
        );

        let mut claimed = refused("b", AT_MS);
        claimed.intent_id = Some("intent-b".to_string());
        assert!(
            db.record_state_log(ExecutionMode::Paper, &[claimed], AT_MS)
                .is_err(),
            "a refusal that named an intent was accepted"
        );
    }

    #[test]
    fn nothing_is_entered_on_a_reason_that_is_not_acceptance() {
        let temp = TempDb::new("accepted-only");
        let db = temp.open();
        let mut wrong = entered("a", "intent-a", AT_MS);
        wrong.reason = GateReason::SmallBundle;
        assert!(
            db.record_state_log(ExecutionMode::Paper, &[wrong], AT_MS)
                .is_err(),
            "a position was opened on a launch the gate refused"
        );
    }

    #[test]
    fn evidence_from_after_the_decision_is_refused_at_the_column() {
        // The no-leakage property, enforced rather than reviewed. A detector
        // that read one event past its window shows up as a failed insert.
        let temp = TempDb::new("leakage");
        let db = temp.open();
        let mut leaking = refused("a", AT_MS);
        leaking.evidence_to_ms = AT_MS + 1;
        assert!(
            db.record_state_log(ExecutionMode::Paper, &[leaking], AT_MS)
                .is_err(),
            "a row that read the future was accepted"
        );
    }

    #[test]
    fn a_forensic_row_records_what_was_believed_then_and_does_not_change() {
        let temp = TempDb::new("append-only");
        let db = temp.open();
        db.record_state_log(ExecutionMode::Paper, &[refused("a", AT_MS)], AT_MS)
            .expect("writes");

        let conn = db.connection();
        let err = conn
            .execute(
                "UPDATE journal_state_log SET reason = 'accepted' WHERE revision = 1",
                [],
            )
            .expect_err("the trigger refuses it");
        assert!(err.to_string().contains("does not change"));
    }

    #[test]
    fn a_confidence_past_a_whole_unit_is_clamped_rather_than_lost() {
        // The gate cannot produce one, so this is a guard against a caller that
        // computed a number. A row that says "as confident as it gets" is worth
        // more than a batch that failed to write.
        let mut over = verdict(GateReason::LowScore);
        over.confidence_micros = 9_000_000;
        let record = StateRecord::decided(
            "a",
            &over,
            &risk(OperatingMode::Paper, 0),
            Decision::Refused,
            None,
            1,
            0,
            true,
            AT_MS,
            AT_MS,
        );
        assert_eq!(record.confidence_micros, crate::types::MICROS_DENOMINATOR);

        let temp = TempDb::new("clamped");
        let db = temp.open();
        db.record_state_log(ExecutionMode::Paper, &[record], AT_MS)
            .expect("the clamped row writes");
    }

    #[test]
    fn the_risk_half_is_derived_and_cannot_disagree_with_itself() {
        // `entries_allowed` and `free_slots` are computed from the snapshot
        // rather than passed, so a row cannot claim a permission its own
        // numbers refuse.
        let full = risk(OperatingMode::Paper, 3);
        let record = StateRecord::decided(
            "a",
            &verdict(GateReason::Accepted),
            &full,
            Decision::Deferred,
            None,
            4,
            0,
            true,
            AT_MS,
            AT_MS,
        );
        assert!(
            !record.entries_allowed,
            "the position cap was open at the cap"
        );
        assert_eq!(record.free_slots, 0);

        let halted = risk(OperatingMode::Halted, 0);
        let stopped = StateRecord::decided(
            "b",
            &verdict(GateReason::Accepted),
            &halted,
            Decision::Deferred,
            None,
            4,
            0,
            true,
            AT_MS,
            AT_MS,
        );
        assert!(!stopped.entries_allowed);
        assert_eq!(stopped.operating_mode, OperatingMode::Halted);

        let temp = TempDb::new("halted");
        let db = temp.open();
        db.record_state_log(ExecutionMode::Paper, std::slice::from_ref(&stopped), AT_MS)
            .expect("a halted engine still records what it saw");
        let back = db
            .query_state_log(&StateLogFilter::in_mode(ExecutionMode::Paper))
            .expect("reads");
        assert_eq!(back[0].record.operating_mode, OperatingMode::Halted);
    }

    #[test]
    fn a_tripped_breaker_shows_up_as_a_deferral_rather_than_a_refusal() {
        // The distinction the third arm of `Decision` exists for: the strategy
        // liked this launch and the account refused it, and a funnel that
        // folded the two would blame the rule.
        let mut tripped = risk(OperatingMode::Paper, 0);
        tripped.circuit_breaker =
            CircuitBreaker::trip_until(BreakerReason::LosingStreak, AT_MS - 1_000, AT_MS + 60_000);
        let record = StateRecord::decided(
            "a",
            &verdict(GateReason::Accepted),
            &tripped,
            Decision::Deferred,
            None,
            30,
            500_000_000,
            true,
            AT_MS,
            AT_MS,
        );
        assert!(!record.entries_allowed);
        assert_eq!(record.reason, GateReason::Accepted);
        assert_eq!(record.decision, Decision::Deferred);

        let temp = TempDb::new("breaker");
        let db = temp.open();
        db.record_state_log(ExecutionMode::Paper, &[record], AT_MS)
            .expect("writes");

        let funnel = db
            .state_funnel(&StateLogFilter::in_mode(ExecutionMode::Paper))
            .expect("counts");
        assert_eq!(funnel.deferred, 1);
        assert_eq!(funnel.refused, 0);
        assert_eq!(
            funnel
                .reasons
                .iter()
                .find(|(n, _)| n == "accepted")
                .map(|(_, c)| *c),
            Some(1),
            "the gate's own verdict is still 'accepted' on a row nothing was opened for"
        );
    }

    // -----------------------------------------------------------------------
    // reading it back
    // -----------------------------------------------------------------------

    fn seed(db: &Database) {
        let records = vec![
            refused("mint-a", AT_MS),
            refused("mint-b", AT_MS + 1_000),
            entered("mint-c", "intent-c", AT_MS + 2_000),
            deferred("mint-a", AT_MS + 3_000),
            refused("mint-c", AT_MS + 4_000),
        ];
        db.record_state_log(ExecutionMode::Paper, &records, AT_MS)
            .expect("writes");
        // A different mode, to prove the filter's mode is not decorative.
        db.record_state_log(ExecutionMode::Live, &[refused("mint-z", AT_MS)], AT_MS)
            .expect("writes");
    }

    #[test]
    fn the_default_page_is_the_mode_newest_first() {
        let temp = TempDb::new("newest-first");
        let db = temp.open();
        seed(&db);

        let rows = db
            .query_state_log(&StateLogFilter::in_mode(ExecutionMode::Paper))
            .expect("reads");
        assert_eq!(rows.len(), 5, "the live row leaked into the paper page");
        let seen: Vec<u64> = rows.iter().map(|r| r.revision).collect();
        assert_eq!(seen, vec![5, 4, 3, 2, 1]);
    }

    #[test]
    fn every_filter_narrows_to_what_it_names() {
        let temp = TempDb::new("filters");
        let db = temp.open();
        seed(&db);

        let by_mint = StateLogFilter {
            mint: Some("mint-a".to_string()),
            ..StateLogFilter::in_mode(ExecutionMode::Paper)
        };
        assert_eq!(db.query_state_log(&by_mint).expect("reads").len(), 2);

        let by_decision = StateLogFilter {
            decision: Some(Decision::Entered),
            ..StateLogFilter::in_mode(ExecutionMode::Paper)
        };
        let entered_rows = db.query_state_log(&by_decision).expect("reads");
        assert_eq!(entered_rows.len(), 1);
        assert_eq!(
            entered_rows[0].record.intent_id.as_deref(),
            Some("intent-c")
        );

        let by_reason = StateLogFilter {
            reason: Some(GateReason::LowScore),
            ..StateLogFilter::in_mode(ExecutionMode::Paper)
        };
        assert_eq!(db.query_state_log(&by_reason).expect("reads").len(), 3);

        let by_time = StateLogFilter {
            since_ms: Some(AT_MS + 2_000),
            until_ms: Some(AT_MS + 3_000),
            ..StateLogFilter::in_mode(ExecutionMode::Paper)
        };
        assert_eq!(db.query_state_log(&by_time).expect("reads").len(), 2);

        let by_revision = StateLogFilter {
            since_revision: Some(2),
            until_revision: Some(3),
            ..StateLogFilter::in_mode(ExecutionMode::Paper)
        };
        assert_eq!(db.query_state_log(&by_revision).expect("reads").len(), 2);
    }

    #[test]
    fn a_page_is_a_page_and_the_ceiling_is_the_ceiling() {
        let temp = TempDb::new("paging");
        let db = temp.open();
        seed(&db);

        let page = StateLogFilter {
            limit: 2,
            offset: 1,
            ..StateLogFilter::in_mode(ExecutionMode::Paper)
        };
        let rows = db.query_state_log(&page).expect("reads");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].revision, 4);

        let unbounded = StateLogFilter {
            limit: u32::MAX,
            ..StateLogFilter::in_mode(ExecutionMode::Paper)
        };
        assert_eq!(unbounded.effective_limit(), MAX_STATE_LIMIT);
        let zero = StateLogFilter {
            limit: 0,
            ..StateLogFilter::in_mode(ExecutionMode::Paper)
        };
        assert_eq!(zero.effective_limit(), DEFAULT_STATE_LIMIT);
    }

    #[test]
    fn a_mint_from_a_text_box_is_a_parameter_and_not_sql() {
        let temp = TempDb::new("injection");
        let db = temp.open();
        seed(&db);

        let hostile = StateLogFilter {
            mint: Some("'; DROP TABLE journal_state_log; --".to_string()),
            ..StateLogFilter::in_mode(ExecutionMode::Paper)
        };
        assert!(db.query_state_log(&hostile).expect("reads").is_empty());
        // Still there.
        assert_eq!(
            db.query_state_log(&StateLogFilter::in_mode(ExecutionMode::Paper))
                .expect("reads")
                .len(),
            5
        );
    }

    #[test]
    fn the_funnel_counts_every_reason_including_the_zeroes() {
        let temp = TempDb::new("funnel");
        let db = temp.open();
        seed(&db);

        let funnel = db
            .state_funnel(&StateLogFilter::in_mode(ExecutionMode::Paper))
            .expect("counts");
        assert_eq!(funnel.rows, 5);
        assert_eq!(funnel.entered, 1);
        assert_eq!(funnel.refused, 3);
        assert_eq!(funnel.deferred, 1);
        assert_eq!(
            funnel.reasons.len(),
            GateReason::ALL.len(),
            "a reason nobody hit has to be a zero rather than a missing row"
        );
        for (index, reason) in GateReason::ALL.iter().enumerate() {
            assert_eq!(funnel.reasons[index].0, reason.as_str(), "the order moved");
        }
        assert_eq!(funnel.revisions, Some(RevisionRange { first: 1, last: 5 }));

        let empty = db
            .state_funnel(&StateLogFilter::in_mode(ExecutionMode::Replay))
            .expect("counts");
        assert_eq!(empty.rows, 0);
        assert_eq!(empty.revisions, None);
        assert_eq!(empty.reasons.len(), GateReason::ALL.len());
    }

    // -----------------------------------------------------------------------
    // the checkpoints
    // -----------------------------------------------------------------------

    #[test]
    fn a_snapshot_is_the_book_and_the_log_at_one_instant() {
        let temp = TempDb::new("snapshot");
        let db = temp.open();
        seed(&db);
        db.record_journal_trades(&[trade("t-1", ExecutionMode::Paper)])
            .expect("writes");

        let snapshot = db
            .take_journal_snapshot(ExecutionMode::Paper, AT_MS + 9_000)
            .expect("takes");
        assert_eq!(snapshot.revision, 5);
        assert_eq!(snapshot.totals.trades, 1);
        assert_eq!(snapshot.totals.notional_lamports, 500_000_000);
        assert_eq!(snapshot.rows_since, 5);
        assert_eq!(snapshot.covers_from, 0);
        assert_eq!(snapshot.entered_since, 1);
        assert_eq!(snapshot.refused_since, 3);
        assert_eq!(snapshot.deferred_since, 1);
        assert_eq!(snapshot.prev_digest, None, "the first link is genesis");
        assert_eq!(
            snapshot.digest.len(),
            64,
            "a SHA-256 is sixty-four hex digits"
        );

        // The counter is untouched: a snapshot is a statement about the first N
        // rows, not an N+1th row.
        assert_eq!(db.current_revision(ExecutionMode::Paper).expect("reads"), 5);
    }

    #[test]
    fn a_snapshot_of_a_book_that_has_not_moved_is_the_snapshot_that_is_there() {
        let temp = TempDb::new("idempotent-snapshot");
        let db = temp.open();
        seed(&db);

        let first = db
            .take_journal_snapshot(ExecutionMode::Paper, AT_MS)
            .expect("takes");
        let again = db
            .take_journal_snapshot(ExecutionMode::Paper, AT_MS + 300_000)
            .expect("takes");
        assert_eq!(first, again, "a quiet weekend wrote a second identical row");
        assert_eq!(
            db.journal_snapshots(ExecutionMode::Paper)
                .expect("reads")
                .len(),
            1
        );
    }

    #[test]
    fn the_chain_links_each_snapshot_to_the_one_before_it() {
        let temp = TempDb::new("chain");
        let db = temp.open();

        let mut previous: Option<String> = None;
        for round in 0..4 {
            let at = AT_MS + round * 1_000;
            db.record_state_log(
                ExecutionMode::Paper,
                &[refused("a", at), refused("b", at + 1)],
                at,
            )
            .expect("writes");
            db.record_journal_trades(&[trade(&format!("t-{round}"), ExecutionMode::Paper)])
                .expect("writes");
            let snapshot = db
                .take_journal_snapshot(ExecutionMode::Paper, at + 500)
                .expect("takes");
            assert_eq!(
                snapshot.prev_digest, previous,
                "round {round} linked to the wrong place"
            );
            previous = Some(snapshot.digest.clone());
        }

        let report = db
            .verify_journal_snapshot_chain(ExecutionMode::Paper)
            .expect("walks");
        assert!(
            report.is_intact(),
            "a chain nobody touched did not verify: {report:?}"
        );
        assert_eq!(report.snapshots, 4);
        assert_eq!(report.intervals_checked, 4);
        assert_eq!(report.intervals_pruned, 0);
    }

    #[test]
    fn a_snapshot_cannot_be_rewritten() {
        let temp = TempDb::new("immutable");
        let db = temp.open();
        seed(&db);
        db.take_journal_snapshot(ExecutionMode::Paper, AT_MS)
            .expect("takes");

        let conn = db.connection();
        let err = conn
            .execute("UPDATE journal_snapshots SET entered_since = 99", [])
            .expect_err("the trigger refuses it");
        assert!(err.to_string().contains("cannot be rewritten"));
    }

    #[test]
    fn an_edited_snapshot_breaks_the_chain_from_there_on() {
        // Tampering has to go through a delete and a re-insert, because the
        // trigger blocks the update. That is the realistic shape of it: a
        // person with `sqlite3` open and a number they would rather the book
        // said. The chain is what notices.
        let temp = TempDb::new("tampered");
        let db = temp.open();

        for round in 0..3 {
            let at = AT_MS + round * 1_000;
            db.record_state_log(ExecutionMode::Paper, &[refused("a", at)], at)
                .expect("writes");
            db.record_journal_trades(&[trade(&format!("t-{round}"), ExecutionMode::Paper)])
                .expect("writes");
            db.take_journal_snapshot(ExecutionMode::Paper, at + 1)
                .expect("takes");
        }
        assert!(db
            .verify_journal_snapshot_chain(ExecutionMode::Paper)
            .expect("walks")
            .is_intact());

        {
            let conn = db.connection();
            conn.execute("DELETE FROM journal_snapshots WHERE seq = 2", [])
                .expect("deletes");
            conn.execute(
                "INSERT INTO journal_snapshots (
                     mode, seq, revision, taken_at_ms, trades, closed, notional_lamports,
                     cost_basis_lamports, proceeds_lamports, realized_pnl_lamports,
                     fee_lamports, tip_lamports, worst_slippage_bps,
                     covers_from, rows_since, entered_since, refused_since, deferred_since,
                     prev_digest, digest
                 )
                 SELECT mode, 2, revision, taken_at_ms, 41, closed, notional_lamports,
                        cost_basis_lamports, proceeds_lamports, realized_pnl_lamports,
                        fee_lamports, tip_lamports, worst_slippage_bps,
                        covers_from, rows_since, entered_since, refused_since, deferred_since,
                        prev_digest, digest
                   FROM journal_snapshots WHERE seq = 1",
                [],
            )
            .expect("inserts the lie");
        }

        let report = db
            .verify_journal_snapshot_chain(ExecutionMode::Paper)
            .expect("walks");
        assert!(!report.is_intact(), "an edited number verified");
        assert!(
            report.breaks.iter().any(|b| b.seq == 2),
            "the edited row was not named: {report:?}"
        );
        assert!(
            report.breaks.iter().any(|b| b.seq == 3),
            "the rows after the edit still verified, so the chain is not a chain"
        );
    }

    #[test]
    fn a_checkpoint_that_is_gone_is_a_break_rather_than_a_shorter_chain() {
        let temp = TempDb::new("missing-link");
        let db = temp.open();

        for round in 0..3 {
            let at = AT_MS + round * 1_000;
            db.record_state_log(ExecutionMode::Paper, &[refused("a", at)], at)
                .expect("writes");
            db.take_journal_snapshot(ExecutionMode::Paper, at + 1)
                .expect("takes");
        }
        {
            let conn = db.connection();
            conn.execute("DELETE FROM journal_snapshots WHERE seq = 2", [])
                .expect("deletes");
        }

        let report = db
            .verify_journal_snapshot_chain(ExecutionMode::Paper)
            .expect("walks");
        assert!(
            !report.is_intact(),
            "a chain with a link cut out of it verified"
        );
        assert!(
            report
                .breaks
                .iter()
                .any(|b| b.detail.contains("checkpoint 2 is missing")),
            "the missing link was not named: {report:?}"
        );
    }

    #[test]
    fn a_book_that_moves_without_the_log_is_still_checkpointed() {
        // The bug the `seq` column exists to make impossible. The exit path
        // writes `journal_trades` whether or not anything is logging verdicts,
        // so the book can change while the counter stands still — and a
        // checkpoint keyed by revision alone could never record the second
        // change, because the row naming that revision would already be there.
        let temp = TempDb::new("book-moves");
        let db = temp.open();

        db.record_journal_trades(&[trade("t-1", ExecutionMode::Paper)])
            .expect("writes");
        let first = db
            .take_journal_snapshot(ExecutionMode::Paper, AT_MS)
            .expect("takes");
        assert_eq!(first.seq, 1);
        assert_eq!(first.revision, 0, "nothing has been logged");
        assert_eq!(first.totals.trades, 1);

        // A second trade, and not one verdict logged.
        db.record_journal_trades(&[trade("t-2", ExecutionMode::Paper)])
            .expect("writes");
        let second = db
            .take_journal_snapshot(ExecutionMode::Paper, AT_MS + 1_000)
            .expect("takes");
        assert_eq!(second.seq, 2, "the second book was not checkpointed");
        assert_eq!(second.revision, 0, "and it still accounts for no log rows");
        assert_eq!(second.totals.trades, 2);
        assert_eq!(second.prev_digest.as_ref(), Some(&first.digest));

        // A third pass with nothing changed writes nothing.
        let again = db
            .take_journal_snapshot(ExecutionMode::Paper, AT_MS + 2_000)
            .expect("takes");
        assert_eq!(again, second, "a quiet pass wrote a third row");
        assert_eq!(
            db.journal_snapshots(ExecutionMode::Paper)
                .expect("reads")
                .len(),
            2
        );

        // And the chain over two checkpoints that share a revision verifies:
        // the interval between them is empty and both claim no rows in it.
        let report = db
            .verify_journal_snapshot_chain(ExecutionMode::Paper)
            .expect("walks");
        assert!(
            report.is_intact(),
            "two checkpoints at one revision broke the chain: {report:?}"
        );
        assert_eq!(report.snapshots, 2);
    }

    #[test]
    fn the_verdict_is_superseded_once_the_log_moves() {
        let temp = TempDb::new("superseded");
        let db = temp.open();
        seed(&db);
        db.record_journal_trades(&[trade("t-1", ExecutionMode::Paper)])
            .expect("writes");

        db.take_journal_snapshot(ExecutionMode::Paper, AT_MS)
            .expect("takes");
        assert_eq!(
            db.verify_journal_snapshot(ExecutionMode::Paper)
                .expect("verifies"),
            SnapshotVerdict::Matches { revision: 5 }
        );

        db.record_state_log(
            ExecutionMode::Paper,
            &[refused("later", AT_MS + 9_000)],
            AT_MS,
        )
        .expect("writes");
        assert_eq!(
            db.verify_journal_snapshot(ExecutionMode::Paper)
                .expect("verifies"),
            SnapshotVerdict::Superseded {
                revision: 5,
                now: 6
            },
            "a checkpoint from before is not a failure, it is a checkpoint from before"
        );
    }

    #[test]
    fn a_book_edited_under_a_standing_snapshot_diverges() {
        let temp = TempDb::new("diverged");
        let db = temp.open();
        seed(&db);
        db.record_journal_trades(&[trade("t-1", ExecutionMode::Paper)])
            .expect("writes");
        db.take_journal_snapshot(ExecutionMode::Paper, AT_MS)
            .expect("takes");

        {
            let conn = db.connection();
            conn.execute(
                "UPDATE journal_trades SET notional_lamports = 1 WHERE trade_id = 't-1'",
                [],
            )
            .expect("the book itself is not immutable — a trade updates as it closes");
        }

        let verdict = db
            .verify_journal_snapshot(ExecutionMode::Paper)
            .expect("verifies");
        assert!(
            verdict.is_divergence(),
            "an edited book matched its snapshot: {verdict:?}"
        );
        match verdict {
            SnapshotVerdict::Diverged {
                recorded,
                recomputed,
                ..
            } => {
                assert_eq!(recorded.notional_lamports, 500_000_000);
                assert_eq!(recomputed.notional_lamports, 1);
            }
            other => panic!("expected a divergence, got {other:?}"),
        }
    }

    #[test]
    fn the_verdict_of_a_mode_nobody_has_checkpointed_is_none() {
        let temp = TempDb::new("no-snapshot");
        let db = temp.open();
        assert_eq!(
            db.verify_journal_snapshot(ExecutionMode::Live)
                .expect("verifies"),
            SnapshotVerdict::None
        );
    }

    #[test]
    fn the_digest_covers_every_field_and_is_stable() {
        // Determinism first: the same row hashes the same twice, because
        // nothing in `canonical_bytes` iterates a map or formats a float.
        let row = SnapshotRow {
            mode: ExecutionMode::Paper,
            seq: 4,
            revision: 12,
            taken_at_ms: AT_MS,
            totals: JournalTotals {
                trades: 3,
                closed: 1,
                notional_lamports: 500_000_000,
                cost_basis_lamports: 495_000_000,
                proceeds_lamports: 100,
                realized_pnl_lamports: -400_000_000,
                fee_lamports: 5_000,
                tip_lamports: 200,
                worst_slippage_bps: Some(310),
            },
            covers_from: 7,
            rows_since: 40,
            entered_since: 3,
            refused_since: 35,
            deferred_since: 2,
            prev_digest: Some("a".repeat(64)),
            digest: String::new(),
        };
        assert_eq!(row.compute_digest(), row.compute_digest());

        // And every field is actually in it.
        //
        // One edit per field, applied to a copy, and the digest has to move for
        // every one of them. A field left out of `canonical_bytes` is a field
        // somebody can change without breaking the chain, and the only way to
        // notice is to try each one.
        type Edit = (&'static str, Box<dyn Fn(&mut SnapshotRow)>);

        let base = row.compute_digest();
        let mutations: Vec<Edit> = vec![
            (
                "mode",
                Box::new(|r: &mut SnapshotRow| r.mode = ExecutionMode::Live),
            ),
            ("seq", Box::new(|r: &mut SnapshotRow| r.seq = 5)),
            ("revision", Box::new(|r: &mut SnapshotRow| r.revision = 13)),
            (
                "taken_at_ms",
                Box::new(|r: &mut SnapshotRow| r.taken_at_ms += 1),
            ),
            (
                "trades",
                Box::new(|r: &mut SnapshotRow| r.totals.trades += 1),
            ),
            (
                "closed",
                Box::new(|r: &mut SnapshotRow| r.totals.closed += 1),
            ),
            (
                "notional",
                Box::new(|r: &mut SnapshotRow| r.totals.notional_lamports += 1),
            ),
            (
                "cost_basis",
                Box::new(|r: &mut SnapshotRow| r.totals.cost_basis_lamports += 1),
            ),
            (
                "proceeds",
                Box::new(|r: &mut SnapshotRow| r.totals.proceeds_lamports += 1),
            ),
            (
                "pnl",
                Box::new(|r: &mut SnapshotRow| r.totals.realized_pnl_lamports += 1),
            ),
            (
                "fee",
                Box::new(|r: &mut SnapshotRow| r.totals.fee_lamports += 1),
            ),
            (
                "tip",
                Box::new(|r: &mut SnapshotRow| r.totals.tip_lamports += 1),
            ),
            (
                "slippage",
                Box::new(|r: &mut SnapshotRow| r.totals.worst_slippage_bps = Some(311)),
            ),
            (
                "no slippage",
                Box::new(|r: &mut SnapshotRow| r.totals.worst_slippage_bps = None),
            ),
            (
                "covers_from",
                Box::new(|r: &mut SnapshotRow| r.covers_from += 1),
            ),
            (
                "rows_since",
                Box::new(|r: &mut SnapshotRow| r.rows_since += 1),
            ),
            (
                "entered_since",
                Box::new(|r: &mut SnapshotRow| r.entered_since += 1),
            ),
            (
                "refused_since",
                Box::new(|r: &mut SnapshotRow| r.refused_since += 1),
            ),
            (
                "deferred_since",
                Box::new(|r: &mut SnapshotRow| r.deferred_since += 1),
            ),
            (
                "prev",
                Box::new(|r: &mut SnapshotRow| r.prev_digest = Some("b".repeat(64))),
            ),
            (
                "genesis",
                Box::new(|r: &mut SnapshotRow| r.prev_digest = None),
            ),
        ];
        for (what, mutate) in mutations {
            let mut edited = row.clone();
            mutate(&mut edited);
            assert_ne!(
                edited.compute_digest(),
                base,
                "editing {what} did not change the digest, so the chain does not cover it"
            );
        }
    }

    #[test]
    fn a_negative_number_survives_the_canonical_bytes() {
        // The realized PnL column is signed and a loss is the common case.
        assert_eq!(itoa(0), "0");
        assert_eq!(itoa(-1), "-1");
        assert_eq!(itoa(1_234_567_890), "1234567890");
        assert_eq!(itoa(i64::MAX), i64::MAX.to_string());
        assert_eq!(itoa(i64::MIN), i64::MIN.to_string());
    }

    // -----------------------------------------------------------------------
    // warm start and retention
    // -----------------------------------------------------------------------

    #[test]
    fn a_warm_start_says_what_still_has_to_be_replayed() {
        let temp = TempDb::new("warm-start");
        let db = temp.open();
        seed(&db);
        db.record_journal_trades(&[trade("t-1", ExecutionMode::Paper)])
            .expect("writes");
        db.take_journal_snapshot(ExecutionMode::Paper, AT_MS)
            .expect("takes");
        db.record_state_log(
            ExecutionMode::Paper,
            &[refused("x", AT_MS + 9_000), refused("y", AT_MS + 9_001)],
            AT_MS,
        )
        .expect("writes");

        let warm = db.warm_start(ExecutionMode::Paper).expect("reads");
        assert_eq!(warm.revision, 7);
        assert_eq!(warm.snapshot.as_ref().map(|s| s.revision), Some(5));
        assert_eq!(
            warm.uncheckpointed, 2,
            "two rows the checkpoint does not account for"
        );
        assert!(warm.is_clean());
        assert!(matches!(warm.verdict, SnapshotVerdict::Superseded { .. }));

        // And those two rows are exactly what the catch-up filter returns.
        let catch_up = db
            .query_state_log(&StateLogFilter::after_revision(ExecutionMode::Paper, 5))
            .expect("reads");
        assert_eq!(catch_up.len(), 2);
        assert_eq!(catch_up[0].revision, 6);
        assert_eq!(catch_up[1].revision, 7);
    }

    #[test]
    fn a_warm_start_on_an_empty_file_is_clean_and_says_nothing_happened() {
        let temp = TempDb::new("cold-start");
        let db = temp.open();
        let warm = db.warm_start(ExecutionMode::Live).expect("reads");
        assert_eq!(warm.revision, 0);
        assert_eq!(warm.snapshot, None);
        assert_eq!(warm.uncheckpointed, 0);
        assert_eq!(warm.verdict, SnapshotVerdict::None);
        assert!(warm.is_clean());
    }

    #[test]
    fn a_warm_start_over_a_tampered_file_is_not_clean() {
        let temp = TempDb::new("dirty-start");
        let db = temp.open();
        seed(&db);
        db.record_journal_trades(&[trade("t-1", ExecutionMode::Paper)])
            .expect("writes");
        db.take_journal_snapshot(ExecutionMode::Paper, AT_MS)
            .expect("takes");
        {
            let conn = db.connection();
            conn.execute("UPDATE journal_trades SET fee_lamports = 7", [])
                .expect("edits");
        }
        let warm = db.warm_start(ExecutionMode::Paper).expect("reads");
        assert!(!warm.is_clean(), "an edited book warm-started clean");
    }

    #[test]
    fn pruning_never_removes_what_no_snapshot_accounts_for() {
        let temp = TempDb::new("prune");
        let db = temp.open();

        let old: Vec<StateRecord> = (0..6)
            .map(|i| refused(&format!("old-{i}"), AT_MS + i))
            .collect();
        db.record_state_log(ExecutionMode::Paper, &old, AT_MS)
            .expect("writes");
        db.take_journal_snapshot(ExecutionMode::Paper, AT_MS + 100)
            .expect("takes");

        let recent: Vec<StateRecord> = (0..4)
            .map(|i| refused(&format!("new-{i}"), AT_MS + 1_000 + i))
            .collect();
        db.record_state_log(ExecutionMode::Paper, &recent, AT_MS)
            .expect("writes");

        // A cutoff past everything. Only the six the snapshot covers may go.
        let removed = db
            .prune_state_log(ExecutionMode::Paper, AT_MS + 100_000)
            .expect("prunes");
        assert_eq!(removed, 6);

        let left = db
            .query_state_log(&StateLogFilter::after_revision(ExecutionMode::Paper, 0))
            .expect("reads");
        assert_eq!(left.len(), 4);
        assert_eq!(
            left[0].revision, 7,
            "a row the checkpoint had not seen was removed"
        );

        // The counter does not move backwards to fill the hole, and the next
        // write carries on from where it was.
        assert_eq!(
            db.current_revision(ExecutionMode::Paper).expect("reads"),
            10
        );
    }

    #[test]
    fn a_mode_with_no_checkpoint_prunes_nothing() {
        let temp = TempDb::new("prune-nothing");
        let db = temp.open();
        db.record_state_log(ExecutionMode::Paper, &[refused("a", AT_MS)], AT_MS)
            .expect("writes");
        assert_eq!(
            db.prune_state_log(ExecutionMode::Paper, AT_MS + 100_000)
                .expect("prunes"),
            0
        );
    }

    #[test]
    fn a_pruned_interval_is_reported_rather_than_called_a_break() {
        let temp = TempDb::new("pruned-interval");
        let db = temp.open();

        for round in 0..3 {
            let at = AT_MS + round * 10_000;
            db.record_state_log(
                ExecutionMode::Paper,
                &[refused("a", at), refused("b", at + 1)],
                at,
            )
            .expect("writes");
            db.take_journal_snapshot(ExecutionMode::Paper, at + 2)
                .expect("takes");
        }
        // Everything in the first interval, and nothing after it.
        let removed = db
            .prune_state_log(ExecutionMode::Paper, AT_MS + 5_000)
            .expect("prunes");
        assert_eq!(removed, 2);

        let report = db
            .verify_journal_snapshot_chain(ExecutionMode::Paper)
            .expect("walks");
        assert!(
            report.is_intact(),
            "pruning under the watermark was reported as tampering: {report:?}"
        );
        assert_eq!(report.intervals_pruned, 1);
        assert_eq!(report.intervals_checked, 2);
    }

    #[test]
    fn a_checkpoint_taken_after_a_prune_still_verifies() {
        // The regression this whole `_since` shape exists for. A running count
        // of surviving rows falls every time retention runs, so a cross-check
        // built on the difference between two running counts reads a successful
        // prune as a checkpoint claiming a negative number of rows — and every
        // checkpoint after the first prune is a break, thirty days into a live
        // run, on a file nothing is wrong with.
        //
        // The earlier version of this suite missed it by verifying immediately
        // after the prune and never checkpointing again. So this one
        // checkpoints again, which is what the maintenance timer does.
        let temp = TempDb::new("prune-then-checkpoint");
        let db = temp.open();

        for round in 0..4 {
            let at = AT_MS + round * 10_000;
            db.record_state_log(
                ExecutionMode::Paper,
                &[
                    refused("a", at),
                    refused("b", at + 1),
                    entered("c", &format!("i-{round}"), at + 2),
                ],
                at,
            )
            .expect("writes");
            db.take_journal_snapshot(ExecutionMode::Paper, at + 3)
                .expect("takes");
        }

        // Retention removes the first two slices, exactly as the maintenance
        // thread would: below the cutoff, and under the newest checkpoint.
        let removed = db
            .prune_state_log(ExecutionMode::Paper, AT_MS + 15_000)
            .expect("prunes");
        assert_eq!(removed, 6, "two slices of three");

        // And now the timer comes round again on a log that has just lost rows.
        db.record_state_log(
            ExecutionMode::Paper,
            &[refused("d", AT_MS + 50_000)],
            AT_MS + 50_000,
        )
        .expect("writes");
        let after = db
            .take_journal_snapshot(ExecutionMode::Paper, AT_MS + 50_001)
            .expect("takes");
        assert_eq!(after.seq, 5);
        assert_eq!(
            after.covers_from, 12,
            "the slice starts where the last one stopped"
        );
        assert_eq!(after.rows_since, 1, "and holds only what arrived in it");

        let report = db
            .verify_journal_snapshot_chain(ExecutionMode::Paper)
            .expect("walks");
        assert!(
            report.is_intact(),
            "a checkpoint taken after retention ran was called tampering: {report:?}"
        );
        assert_eq!(
            report.intervals_pruned, 2,
            "the two slices retention emptied"
        );
        assert_eq!(
            report.intervals_checked, 3,
            "and the three it did not touch"
        );
    }

    #[test]
    fn a_row_that_vanished_without_a_prune_is_a_break() {
        // The other side of the same check: rows can only leave through the
        // pruner, and the pruner cannot touch what the newest snapshot has not
        // seen. A row deleted from above the watermark is unexplained, and the
        // interval check is what notices.
        let temp = TempDb::new("vanished");
        let db = temp.open();

        db.record_state_log(
            ExecutionMode::Paper,
            &[refused("a", AT_MS), refused("b", AT_MS + 1)],
            AT_MS,
        )
        .expect("writes");
        db.take_journal_snapshot(ExecutionMode::Paper, AT_MS + 2)
            .expect("takes");
        db.record_state_log(
            ExecutionMode::Paper,
            &[refused("c", AT_MS + 3), refused("d", AT_MS + 4)],
            AT_MS,
        )
        .expect("writes");
        db.take_journal_snapshot(ExecutionMode::Paper, AT_MS + 5)
            .expect("takes");

        {
            let conn = db.connection();
            conn.execute("DELETE FROM journal_state_log WHERE revision = 3", [])
                .expect("deletes");
        }

        let report = db
            .verify_journal_snapshot_chain(ExecutionMode::Paper)
            .expect("walks");
        // The digests still verify — nothing edited a snapshot. What does not
        // verify is the interval, which is the whole reason a snapshot carries
        // counts it could have recomputed.
        assert_eq!(report.intervals_pruned, 1);
        assert!(
            report.is_intact(),
            "a hole under the watermark reads as a prune"
        );

        // Above the watermark it does not: the same deletion, on rows no
        // snapshot has accounted for, cannot be reached by the pruner at all.
        db.record_state_log(ExecutionMode::Paper, &[refused("e", AT_MS + 6)], AT_MS)
            .expect("writes");
        assert_eq!(
            db.prune_state_log(ExecutionMode::Paper, AT_MS + 100_000)
                .expect("prunes"),
            3,
            "the pruner reached above the newest checkpoint"
        );
        let left = db
            .query_state_log(&StateLogFilter::after_revision(ExecutionMode::Paper, 0))
            .expect("reads");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].revision, 5);
    }

    // -----------------------------------------------------------------------
    // concurrency
    // -----------------------------------------------------------------------

    #[test]
    fn every_revision_is_issued_once_when_eight_threads_write_at_once() {
        // The shape of heavy ingestion: several producers, one file, no
        // coordination beyond the mutex `db.rs` puts every writer behind.
        // Nothing may be lost, nothing duplicated, and — the property this
        // table adds — no two rows may end up with the same revision.
        const THREADS: u64 = 8;
        const PER_THREAD: u64 = 25;

        let temp = TempDb::new("concurrent");
        let db = Arc::new(temp.open());

        let mut handles = Vec::new();
        for thread in 0..THREADS {
            let db = Arc::clone(&db);
            handles.push(std::thread::spawn(move || {
                for round in 0..PER_THREAD {
                    let at = AT_MS + (thread * PER_THREAD + round) as i64;
                    db.record_state_log(
                        ExecutionMode::Paper,
                        &[refused(&format!("m-{thread}-{round}"), at)],
                        at,
                    )
                    .expect("writes");
                }
            }));
        }
        for handle in handles {
            handle.join().expect("the writer did not panic");
        }

        let total = THREADS * PER_THREAD;
        assert_eq!(
            db.current_revision(ExecutionMode::Paper).expect("reads"),
            total
        );

        let conn = db.connection();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM journal_state_log", [], |row| {
                row.get(0)
            })
            .expect("counts");
        let distinct: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT revision) FROM journal_state_log",
                [],
                |row| row.get(0),
            )
            .expect("counts");
        let highest: i64 = conn
            .query_row("SELECT MAX(revision) FROM journal_state_log", [], |row| {
                row.get(0)
            })
            .expect("counts");
        assert_eq!(rows, total as i64);
        assert_eq!(distinct, total as i64, "two rows share a revision");
        assert_eq!(highest, total as i64, "the revisions are not gapless");
    }

    // -----------------------------------------------------------------------
    // the writer
    // -----------------------------------------------------------------------

    #[test]
    fn the_writer_batches_what_it_is_given_and_commits_it() {
        let temp = TempDb::new("writer");
        let db = Arc::new(temp.open());
        let logger = StateLogger::start(Arc::clone(&db), ExecutionMode::Paper);

        for i in 0..200 {
            logger.observe(refused(&format!("m-{i}"), AT_MS + i));
        }
        assert!(
            logger.flush(Duration::from_secs(10)),
            "the queue did not settle"
        );

        // Read *without* stopping the writer first. That is the guarantee under
        // test: `flush` returning means the records are committed, not merely
        // that the queue is empty — the difference being the length of the
        // transaction they are in, which is exactly when a caller reading the
        // table would race it.
        let flushed = logger.stats();
        assert_eq!(flushed.queued, 200);
        assert_eq!(flushed.dropped, 0);
        assert_eq!(flushed.written, 200);
        assert_eq!(flushed.failed, 0);
        assert_eq!(flushed.last_revision, 200);
        assert_eq!(
            db.current_revision(ExecutionMode::Paper).expect("reads"),
            200
        );
        assert_eq!(
            db.query_state_log(&StateLogFilter::after_revision(ExecutionMode::Paper, 0))
                .expect("reads")
                .len(),
            200
        );

        assert!(flushed.batches >= 1);
        assert!(
            flushed.batches < 200,
            "two hundred records went in as {} transactions, so nothing batched",
            flushed.batches
        );

        logger.stop();
        assert!(!logger.stats().running);
    }

    #[test]
    fn flushing_an_idle_writer_returns_rather_than_waiting_out_the_timeout() {
        // The empty case, and the reason `flush` reads its target before it
        // waits: nothing queued means nothing to wait for.
        let temp = TempDb::new("flush-idle");
        let db = Arc::new(temp.open());
        let logger = StateLogger::with_capacity(
            Arc::clone(&db),
            ExecutionMode::Paper,
            64,
            Duration::from_secs(30),
        );

        let began = std::time::Instant::now();
        assert!(logger.flush(Duration::from_secs(30)));
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "an idle writer waited out its own flush interval"
        );
        logger.stop();
    }

    #[test]
    fn a_full_queue_drops_and_says_how_many() {
        // The bounded-queue behaviour, and the counter that keeps it from being
        // a silent loss. The writer is kept out of the way by holding the one
        // connection lock, so the queue is guaranteed to fill.
        let temp = TempDb::new("full-queue");
        let db = Arc::new(temp.open());
        let logger = StateLogger::with_capacity(
            Arc::clone(&db),
            ExecutionMode::Paper,
            4,
            Duration::from_millis(20),
        );

        let held = db.connection();
        for i in 0..500 {
            logger.observe(refused(&format!("m-{i}"), AT_MS + i));
        }
        let stats = logger.stats();
        assert!(
            stats.dropped > 0,
            "a queue of four took five hundred records"
        );
        assert_eq!(
            stats.queued + stats.dropped,
            500,
            "a record was neither queued nor counted as dropped"
        );
        drop(held);

        logger.stop();
        let after = logger.stats();
        // Every record is in exactly one of the two buckets, and the drop is
        // the only way one leaves without being written.
        assert_eq!(after.queued + after.dropped, 500);
        assert_eq!(
            after.written, after.queued,
            "a queued record was lost silently"
        );
        assert_eq!(after.failed, 0);
        assert_eq!(
            db.current_revision(ExecutionMode::Paper).expect("reads"),
            after.written,
            "the counter and the writer disagree about how much got in"
        );
    }

    #[test]
    fn stopping_writes_what_is_still_on_the_queue_without_waiting_out_the_interval() {
        // Two properties in one, because they are the same moment. During an
        // incident the last batch is the one that matters, and a shutdown that
        // took a whole flush interval to get to it is the shape of "closing the
        // window stopped responding" — so the interval here is thirty seconds
        // and the test asserts the stop takes nowhere near it.
        let temp = TempDb::new("drain");
        let db = Arc::new(temp.open());
        let logger = StateLogger::with_capacity(
            Arc::clone(&db),
            ExecutionMode::Paper,
            256,
            // Far longer than any timer this test will wait for. Nothing here
            // is written because the interval expired.
            Duration::from_secs(30),
        );

        for i in 0..64 {
            logger.observe(refused(&format!("m-{i}"), AT_MS + i));
        }

        let began = std::time::Instant::now();
        logger.stop();
        let took = began.elapsed();

        assert_eq!(logger.stats().written, 64, "the last batch was thrown away");
        assert_eq!(
            db.current_revision(ExecutionMode::Paper).expect("reads"),
            64
        );
        assert!(
            took < Duration::from_secs(5),
            "stopping waited {took:?} — it sat out the flush interval instead of \
             being woken by the shutdown"
        );
    }

    #[test]
    fn a_writer_with_nothing_to_do_stops_at_once() {
        // The same wake-up from the other state: idle in the outer select
        // rather than mid-interval with a part-filled batch.
        let temp = TempDb::new("idle-stop");
        let db = Arc::new(temp.open());
        let logger = StateLogger::with_capacity(
            Arc::clone(&db),
            ExecutionMode::Paper,
            256,
            Duration::from_secs(30),
        );

        let began = std::time::Instant::now();
        logger.stop();
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "an idle writer had to be waited out"
        );
        assert_eq!(logger.stats().written, 0);
        assert_eq!(db.current_revision(ExecutionMode::Paper).expect("reads"), 0);
    }

    #[test]
    fn the_writer_writes_into_the_mode_it_was_started_for() {
        let temp = TempDb::new("writer-mode");
        let db = Arc::new(temp.open());
        let logger = StateLogger::start(Arc::clone(&db), ExecutionMode::Replay);
        assert_eq!(logger.mode(), ExecutionMode::Replay);
        logger.observe(refused("a", AT_MS));
        logger.stop();

        assert_eq!(
            db.current_revision(ExecutionMode::Replay).expect("reads"),
            1
        );
        assert_eq!(db.current_revision(ExecutionMode::Paper).expect("reads"), 0);
    }

    #[test]
    fn stopping_twice_is_stopping_once() {
        let temp = TempDb::new("stop-twice");
        let db = Arc::new(temp.open());
        let logger = StateLogger::start(Arc::clone(&db), ExecutionMode::Paper);
        logger.observe(refused("a", AT_MS));
        logger.stop();
        logger.stop();
        assert_eq!(logger.stats().written, 1);
    }

    #[test]
    fn the_filter_survives_the_trip_the_window_sends_it_on() {
        let filter = StateLogFilter {
            mint: Some(MINT.to_string()),
            decision: Some(Decision::Entered),
            reason: Some(GateReason::Accepted),
            since_ms: Some(AT_MS),
            since_revision: Some(4),
            ..StateLogFilter::in_mode(ExecutionMode::Live)
        };
        let json = serde_json::to_string(&filter).expect("serialises");
        let back: StateLogFilter = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(back, filter);
        assert!(
            json.contains("\"sinceRevision\":4"),
            "the window sees camelCase: {json}"
        );
    }
}
