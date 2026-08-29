//! Engine state, and the three signals that change it.
//!
//! Three flags describe where the process is, and all three are atomics rather
//! than fields behind a lock. That is not a micro-optimisation: the panic hook
//! reads and writes them from a thread that may be unwinding while any lock in
//! the process is held, and an atomic is the only thing safe to touch there.
//!
//! The flags are ordered `SeqCst` throughout. A kill switch is the wrong place
//! to reason about whether a weaker ordering would have been sufficient — the
//! cost is a few nanoseconds on a path taken once.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::{Mutex, RwLock};
use serde::Serialize;

use crate::alerting::AlertDispatcher;
use crate::db::{Database, DbHealth, ExecutionLogRow, ExecutionMode, OpenObligation};
use crate::error::EngineError;
use crate::execution::{ExecutionEngine, ExitTarget, FlattenOutcome, FlattenReport, Flattener};
use crate::metrics::MetricsCollector;
use crate::telemetry::{
    now_ms, TelemetryEvent, TelemetryHub, TelemetryLevel, TelemetrySnapshot, TelemetrySubscription,
};
use crate::types::{AbortReason, ExecutionState, ExitFailure, ExitState, Venue};
use tauri::ipc::Channel;

/// The audit `event_type` written when the switch is pulled. Kept as one
/// constant because the Node side will want to recognise it.
const KILL_SWITCH_EVENT: &str = "kill_switch";

/// The audit `event_type` written when an emergency unwind is asked for.
///
/// Separate from `KILL_SWITCH_EVENT` even though an unwind always halts,
/// because the two rows record different facts: one says the engine stopped,
/// the other says what it was holding when it did.
const EMERGENCY_UNWIND_EVENT: &str = "emergency_unwind";

/// How long an unwind waits for another one that is already flattening.
///
/// Two passes running at once would each read the exit ledger before the other
/// had written to it, and each would send an exit for the same position — which
/// is a sale of tokens the wallet no longer holds. So they are serialised. The
/// wait is bounded rather than indefinite because the caller is an operator
/// holding a button during an emergency: a pass that cannot get in says so on
/// the receipt and abandons the positions anyway, which is the half that never
/// needs the lock.
const FLATTEN_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// background maintenance
// ---------------------------------------------------------------------------

/// How often `PRAGMA wal_checkpoint(PASSIVE)` runs.
///
/// `SCHEMA.md` asks for a timer without naming a period. Thirty seconds is
/// chosen against `wal_autocheckpoint = 4000`, which is the other thing keeping
/// the WAL down: that one fires on whichever connection trips 16 MiB, which in
/// practice is the writer, mid-commit, on the ingest path. A passive checkpoint
/// every thirty seconds means a busy engine usually reaches the timer before it
/// reaches the page count, so the fold happens on this thread instead of in the
/// middle of somebody's insert.
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(30);

/// How often `tick_metrics` is pruned back to the retention window.
///
/// The table gains one row per endpoint per telemetry tick — a few thousand
/// rows an hour at the current five-second interval — against a seven-day
/// window. Checking hourly deletes a bounded amount each time and never has
/// enough backlog to matter.
const PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// How much `tick_metrics` history is kept. Seven days, from `SCHEMA.md`: long
/// enough to see a provider degrade over a weekend, short enough that the table
/// stays small enough to scan.
const TICK_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How often the book is checkpointed into `journal_snapshots`.
///
/// Five minutes, chosen against what a checkpoint is for rather than against
/// what it costs. The cost is near zero on a quiet period — `take_journal_snapshot`
/// returns the existing row without writing when the log has not moved — so the
/// number is really "how much of the forensic log is a restart willing to
/// replay", and five minutes of it is a few hundred rows.
///
/// It is deliberately not tied to the WAL checkpoint above. That one folds a
/// file; this one records a fact. Sharing a timer would mean tuning one for the
/// other's reasons.
const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How much forensic history is kept.
///
/// Thirty days against `tick_metrics`'s seven, because the two answer different
/// questions. A tick metric is only interesting while the provider that
/// produced it is still the provider; a refusal is evidence in an argument
/// about whether the strategy works, and that argument is had over months. The
/// pruner cannot reach above the newest checkpoint whatever this says, so the
/// window is a floor on what is kept rather than a promise about what is
/// removed.
const STATE_LOG_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// When the two maintenance timers fire and how far back the retention window
/// reaches.
///
/// A parameter rather than three constants read directly, so a test can drive
/// the real loop at a period it can wait for. Nothing outside the tests builds
/// one of these by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MaintenanceSchedule {
    pub checkpoint_every: Duration,
    pub prune_every: Duration,
    pub retain_ticks_for: Duration,
    pub snapshot_every: Duration,
    pub retain_state_log_for: Duration,
}

impl Default for MaintenanceSchedule {
    fn default() -> Self {
        Self {
            checkpoint_every: CHECKPOINT_INTERVAL,
            prune_every: PRUNE_INTERVAL,
            retain_ticks_for: TICK_RETENTION,
            snapshot_every: SNAPSHOT_INTERVAL,
            retain_state_log_for: STATE_LOG_RETENTION,
        }
    }
}

/// What the maintenance thread has done, for `get_engine_status`.
///
/// These counters are the surface the timers report through, rather than a
/// telemetry line per pass. A checkpoint every thirty seconds would be two
/// lines a minute of "nothing happened" in a stream someone is meant to be able
/// to read, and the number that actually matters — a WAL that keeps growing
/// anyway, meaning a reader is being held open across a long operation — is a
/// count and a file size, not an event.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MaintenanceSnapshot {
    /// False once the thread has been joined, or if it never started.
    pub running: bool,
    pub checkpoints: u64,
    pub checkpoint_failures: u64,
    pub prunes: u64,
    pub prune_failures: u64,
    /// Rows removed from `tick_metrics` since this process started.
    pub ticks_pruned: u64,
    pub last_checkpoint_at_ms: Option<i64>,
    pub last_prune_at_ms: Option<i64>,
    /// Checkpoints written to `journal_snapshots`. A pass over a mode whose log
    /// has not moved returns the existing row and is not counted here, so this
    /// is a count of the book actually changing rather than of the timer firing.
    pub snapshots: u64,
    pub snapshot_failures: u64,
    /// Rows removed from `journal_state_log` since this process started.
    pub state_rows_pruned: u64,
    pub last_snapshot_at_ms: Option<i64>,
    /// The newest revision any mode has been checkpointed at, per mode. Kept
    /// so the cockpit can say how far behind the checkpoints are without
    /// reading the table.
    pub last_snapshot_revision: u64,
}

/// The counters behind the snapshot. Atomics because the thread writing them is
/// not the thread reading them and neither should wait for the other.
#[derive(Debug, Default)]
struct MaintenanceMetrics {
    checkpoints: AtomicU64,
    checkpoint_failures: AtomicU64,
    prunes: AtomicU64,
    prune_failures: AtomicU64,
    ticks_pruned: AtomicU64,
    last_checkpoint_at_ms: AtomicI64,
    last_prune_at_ms: AtomicI64,
    snapshots: AtomicU64,
    snapshot_failures: AtomicU64,
    state_rows_pruned: AtomicU64,
    last_snapshot_at_ms: AtomicI64,
    last_snapshot_revision: AtomicU64,
}

/// The two timers `SCHEMA.md` asks for, on one thread.
///
/// A thread rather than a tokio task, for the reason the ingestion WAL worker
/// gives: SQLite is synchronous, so a checkpoint or a chunked delete on a
/// runtime worker blocks every socket sharing that thread. One thread covers
/// both timers because they contend for the same connection lock anyway — two
/// would only be two things waiting on each other.
struct Maintenance {
    metrics: Arc<MaintenanceMetrics>,
    /// Held only so `stop` can drop it. The loop waits on the receiving end, so
    /// dropping this wakes it now rather than at the end of an interval that
    /// may be an hour long.
    stop: Mutex<Option<crossbeam_channel::Sender<()>>>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Maintenance {
    fn start(db: Arc<Database>, hub: Arc<TelemetryHub>, schedule: MaintenanceSchedule) -> Self {
        let metrics = Arc::new(MaintenanceMetrics::default());
        let (stop, stop_rx) = crossbeam_channel::bounded::<()>(1);
        let handle = std::thread::Builder::new()
            .name("sts-maintenance".to_string())
            .spawn({
                let metrics = Arc::clone(&metrics);
                move || maintenance_loop(db, hub, schedule, metrics, stop_rx)
            })
            .expect(
                "the maintenance thread is the only thing keeping the WAL and tick_metrics bounded",
            );

        Self {
            metrics,
            stop: Mutex::new(Some(stop)),
            handle: Mutex::new(Some(handle)),
        }
    }

    fn snapshot(&self) -> MaintenanceSnapshot {
        let at = |value: &AtomicI64| match value.load(Ordering::Relaxed) {
            0 => None,
            at_ms => Some(at_ms),
        };
        MaintenanceSnapshot {
            running: self.handle.lock().is_some(),
            checkpoints: self.metrics.checkpoints.load(Ordering::Relaxed),
            checkpoint_failures: self.metrics.checkpoint_failures.load(Ordering::Relaxed),
            prunes: self.metrics.prunes.load(Ordering::Relaxed),
            prune_failures: self.metrics.prune_failures.load(Ordering::Relaxed),
            ticks_pruned: self.metrics.ticks_pruned.load(Ordering::Relaxed),
            last_checkpoint_at_ms: at(&self.metrics.last_checkpoint_at_ms),
            last_prune_at_ms: at(&self.metrics.last_prune_at_ms),
            snapshots: self.metrics.snapshots.load(Ordering::Relaxed),
            snapshot_failures: self.metrics.snapshot_failures.load(Ordering::Relaxed),
            state_rows_pruned: self.metrics.state_rows_pruned.load(Ordering::Relaxed),
            last_snapshot_at_ms: at(&self.metrics.last_snapshot_at_ms),
            last_snapshot_revision: self.metrics.last_snapshot_revision.load(Ordering::Relaxed),
        }
    }

    /// Wakes the loop, waits for the pass it may be in the middle of, and
    /// joins. Idempotent.
    fn stop(&self) {
        let Some(handle) = self.handle.lock().take() else {
            return;
        };
        drop(self.stop.lock().take());
        let _ = handle.join();
    }
}

fn maintenance_loop(
    db: Arc<Database>,
    hub: Arc<TelemetryHub>,
    schedule: MaintenanceSchedule,
    metrics: Arc<MaintenanceMetrics>,
    stop: crossbeam_channel::Receiver<()>,
) {
    let start = Instant::now();
    // The prune is due immediately and the checkpoint is not. Nothing was
    // written to the WAL while the process was down, so there is nothing to
    // fold on the way up; but the clock kept running, so a machine that was off
    // for a month opens with a table of nothing but expired rows.
    let mut next_prune = start;
    let mut next_checkpoint = start + schedule.checkpoint_every;
    // Due immediately, for the same reason the prune is: whatever the last
    // process was doing when it stopped, the book as it stands now has never
    // been checkpointed by this one, and the first pass is what turns an
    // unclean shutdown into a recorded fact rather than a gap.
    let mut next_snapshot = start;

    loop {
        let due = next_checkpoint.min(next_prune).min(next_snapshot);
        match stop.recv_timeout(due.saturating_duration_since(Instant::now())) {
            // Either end of `stop` moving means the same thing, and the pass
            // that is due can wait for the next process: shutdown runs a
            // `TRUNCATE` checkpoint anyway, which is strictly more than the
            // `PASSIVE` one being skipped here.
            Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }

        // Each next deadline is a full interval after the pass *finished*, not
        // after it was due. This is the same choice the ingestion timers make
        // with `MissedTickBehavior::Delay`, and it matters here for a specific
        // reason: measuring from when the pass was due means that a pass which
        // ran longer than its own interval — a checkpoint stuck behind a reader
        // held open across a long operation — comes back due immediately, and
        // the loop spins running checkpoints back to back against a database
        // that is already struggling. Falling behind should cost frequency,
        // never turn into a busy loop.
        let now = Instant::now();
        if now >= next_checkpoint {
            checkpoint_once(&db, &hub, &metrics);
            next_checkpoint = Instant::now() + schedule.checkpoint_every;
        }
        if now >= next_prune {
            prune_once(&db, &hub, &metrics, schedule.retain_ticks_for);
            next_prune = Instant::now() + schedule.prune_every;
        }
        if now >= next_snapshot {
            snapshot_once(&db, &hub, &metrics, schedule.retain_state_log_for);
            next_snapshot = Instant::now() + schedule.snapshot_every;
        }
    }
}

/// One `PRAGMA wal_checkpoint(PASSIVE)`.
///
/// Silent when it works: the counter is the record. A failure is published
/// once per occurrence because a checkpoint that cannot run is how the WAL
/// grows without bound, and that is worth interrupting somebody for.
fn checkpoint_once(db: &Database, hub: &TelemetryHub, metrics: &MaintenanceMetrics) {
    match db.checkpoint_passive() {
        Ok(()) => {
            metrics.checkpoints.fetch_add(1, Ordering::Relaxed);
            metrics
                .last_checkpoint_at_ms
                .store(now_ms(), Ordering::Relaxed);
        }
        Err(err) => {
            metrics.checkpoint_failures.fetch_add(1, Ordering::Relaxed);
            hub.publish(
                TelemetryLevel::Warn,
                "maintenance",
                "the WAL could not be checkpointed",
                serde_json::json!({ "error": err.to_string() }),
            );
        }
    }
}

/// One pass of the `tick_metrics` retention policy.
///
/// Published only when it actually removed something, so the ordinary case —
/// an hourly pass over a table with nothing old enough in it — costs no
/// telemetry at all.
fn prune_once(
    db: &Database,
    hub: &TelemetryHub,
    metrics: &MaintenanceMetrics,
    retain_for: Duration,
) {
    let now = now_ms();
    let cutoff_ms = now.saturating_sub(retain_for.as_millis() as i64);
    match db.prune_tick_metrics(cutoff_ms) {
        Ok(removed) => {
            metrics.prunes.fetch_add(1, Ordering::Relaxed);
            metrics.last_prune_at_ms.store(now, Ordering::Relaxed);
            if removed > 0 {
                metrics
                    .ticks_pruned
                    .fetch_add(removed as u64, Ordering::Relaxed);
                hub.publish(
                    TelemetryLevel::Debug,
                    "maintenance",
                    format!("pruned {removed} tick metrics past the retention window"),
                    serde_json::json!({ "removed": removed, "cutoffMs": cutoff_ms }),
                );
            }
        }
        Err(err) => {
            metrics.prune_failures.fetch_add(1, Ordering::Relaxed);
            hub.publish(
                TelemetryLevel::Warn,
                "maintenance",
                "tick metrics could not be pruned",
                serde_json::json!({ "error": err.to_string(), "cutoffMs": cutoff_ms }),
            );
        }
    }
}

/// One pass of the journal checkpoint, over all three modes.
///
/// All three every time, rather than only the one the engine is running in. A
/// process configured for paper still has a live book from last week in the
/// same file, and a checkpoint chain with a five-day hole in it is a chain that
/// cannot answer what the book looked like on the day somebody is asking about.
/// The cost of the other two is a `SELECT` that finds the log has not moved.
///
/// Published only when a checkpoint was actually written or something failed.
/// A timer firing every five minutes over a quiet weekend is not news.
fn snapshot_once(
    db: &Database,
    hub: &TelemetryHub,
    metrics: &MaintenanceMetrics,
    retain_for: Duration,
) {
    let now = now_ms();
    let cutoff_ms = now.saturating_sub(retain_for.as_millis() as i64);

    for mode in [
        ExecutionMode::Live,
        ExecutionMode::Paper,
        ExecutionMode::Replay,
    ] {
        let previous = db
            .latest_journal_snapshot(mode)
            .ok()
            .flatten()
            .map(|snapshot| snapshot.seq);

        match db.take_journal_snapshot(mode, now) {
            Ok(snapshot) => {
                metrics
                    .last_snapshot_revision
                    .fetch_max(snapshot.revision, Ordering::Relaxed);
                // `take_journal_snapshot` hands back the existing row when
                // nothing has changed, so a new `seq` is what means a
                // checkpoint was actually written. Not a new *revision*: the
                // book moves without the log whenever a position is written by
                // a path that is not logging verdicts, and those checkpoints
                // share a revision with the one before them.
                if previous != Some(snapshot.seq) {
                    metrics.snapshots.fetch_add(1, Ordering::Relaxed);
                    metrics.last_snapshot_at_ms.store(now, Ordering::Relaxed);
                    hub.publish(
                        TelemetryLevel::Debug,
                        "maintenance",
                        format!(
                            "checkpointed the {} book at revision {}",
                            mode.as_str(),
                            snapshot.revision
                        ),
                        serde_json::json!({
                            "mode": mode.as_str(),
                            "revision": snapshot.revision,
                            "trades": snapshot.totals.trades,
                            "rowsSince": snapshot.rows_since,
                            "coversFrom": snapshot.covers_from,
                            "digest": snapshot.digest,
                        }),
                    );
                }
            }
            Err(err) => {
                metrics.snapshot_failures.fetch_add(1, Ordering::Relaxed);
                hub.publish(
                    TelemetryLevel::Warn,
                    "maintenance",
                    format!("the {} book could not be checkpointed", mode.as_str()),
                    serde_json::json!({ "mode": mode.as_str(), "error": err.to_string() }),
                );
                // No pruning behind a checkpoint that did not happen. The
                // watermark the pruner reads would be the previous one, which
                // is safe, but a mode whose checkpoint is failing is a mode
                // whose forensic log should be left entirely alone.
                continue;
            }
        }

        match db.prune_state_log(mode, cutoff_ms) {
            Ok(removed) if removed > 0 => {
                metrics
                    .state_rows_pruned
                    .fetch_add(removed as u64, Ordering::Relaxed);
                hub.publish(
                    TelemetryLevel::Debug,
                    "maintenance",
                    format!("pruned {removed} forensic rows past the retention window"),
                    serde_json::json!({
                        "mode": mode.as_str(),
                        "removed": removed,
                        "cutoffMs": cutoff_ms,
                    }),
                );
            }
            Ok(_) => {}
            Err(err) => {
                metrics.snapshot_failures.fetch_add(1, Ordering::Relaxed);
                hub.publish(
                    TelemetryLevel::Warn,
                    "maintenance",
                    format!("the {} forensic log could not be pruned", mode.as_str()),
                    serde_json::json!({ "mode": mode.as_str(), "error": err.to_string() }),
                );
            }
        }
    }
}

/// Where the process is, as one word for the UI to render.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum EngineLifecycle {
    /// Up, and nothing is stopping it.
    Running,
    /// The kill switch is armed. Still up, deliberately doing nothing.
    Halted,
    /// Shutdown has begun; the window is on its way out.
    ShuttingDown,
    /// Everything has been joined and closed.
    Stopped,
}

/// Everything `get_engine_status` returns, in one object.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineStatus {
    pub state: EngineLifecycle,
    pub kill_switch_armed: bool,
    /// When the switch was armed, or `None` while it never has been.
    pub kill_switch_at_ms: Option<i64>,
    pub shutting_down: bool,
    pub started_at_ms: i64,
    pub uptime_ms: i64,
    pub version: &'static str,
    pub telemetry: TelemetrySnapshot,
    pub database: DbHealth,
    pub maintenance: MaintenanceSnapshot,
}

/// What the UI gets back from pulling the switch.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KillSwitchReceipt {
    /// Always true after this call. The switch does not un-arm.
    pub armed: bool,
    /// True if it was already armed before this call. Pulling twice is not an
    /// error — the UI just should not claim it stopped something twice.
    pub already_armed: bool,
    pub at_ms: i64,
    pub reason: String,
    /// The `audit_log` row id, or `None` if the row could not be written. The
    /// switch is armed either way; losing the paper trail does not un-halt it.
    pub audit_id: Option<i64>,
}

/// One position that is on chain and that the engine is no longer managing.
///
/// Everything here is what somebody flattening it by hand needs to find it: the
/// mint, which side they are on, how much, and the signature to look up.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StrandedPosition {
    pub intent_id: String,
    pub mint: String,
    pub side: crate::db::Side,
    pub size_lamports: i64,
    /// The transaction that put the money out, when the intent got far enough
    /// to have one.
    pub signature: Option<String>,
    /// The state the money was left at risk in — `sent` or `confirmed`, never
    /// `aborted`. An obligation the engine already gave up on reads `aborted`
    /// in the table, which tells whoever is looking nothing about what is
    /// actually out there, so this reports the state it was aborted from.
    pub at_risk_in: ExecutionState,
    pub mode: crate::db::ExecutionMode,
    /// True when the obligation is `sent` rather than `confirmed`, which makes
    /// it **conditional and not yet actionable**. The transaction may never
    /// have landed. It has to be followed until it lands or its blockhash
    /// expires before anything is sold against it — selling a position that
    /// does not exist because an abort assumed the worst is its own incident.
    pub conditional: bool,
    /// What the exit path did about it, or `None` when nothing was attempted —
    /// which is every position on a build with no execution backend installed.
    ///
    /// A position is in this list because it is **still at risk**. An entry
    /// here does not contradict that: an exit that is on the network has not
    /// closed anything until it confirms, and one that failed closed even less.
    pub exit: Option<StrandedExit>,
}

/// What was tried on the way out of a position that is still out there.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StrandedExit {
    /// The exit's own intent id, when it got as far as having one.
    pub exit_intent_id: Option<String>,
    /// The exit transaction's signature, when it got as far as being signed.
    pub signature: Option<String>,
    /// Where the exit got to. `None` when nothing was ever built.
    pub state: Option<ExitState>,
    /// Which kind of failure, when it failed.
    pub failure: Option<ExitFailure>,
    /// The sentence a person reads.
    pub detail: String,
    /// True when a transaction is on the network for this position right now.
    /// The position's fate is decided and not yet known, and it must not be
    /// sold again by hand until the signature has been followed.
    pub on_network: bool,
}

/// One position that was sold, landed, and booked.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FlattenedPosition {
    /// The obligation that was flattened.
    pub intent_id: String,
    /// The exit that flattened it.
    pub exit_intent_id: String,
    pub mint: String,
    /// `None` only if the ledger row this was read back from does not say.
    pub venue: Option<Venue>,
    /// `None` only if the ledger row this was read back from does not say.
    pub signature: Option<String>,
    /// What was sold, in token base units. `None` only if the ledger row this
    /// was read back from does not say.
    pub tokens: Option<u64>,
    /// What the position cost to open, in lamports.
    pub cost_basis_lamports: i64,
    /// What came back, in lamports.
    pub out_lamports: i64,
    /// The difference. Negative is a loss.
    pub realized_pnl_lamports: i64,
    pub mode: crate::db::ExecutionMode,
}

/// One obligation that turned out to have nothing behind it.
///
/// The `sent` case from `RISK_AND_SYBIL_SPEC.md` §13.1: the entry never landed
/// and its blockhash expired, so there is no position and never was one. It is
/// closed with a record rather than a transaction, and it is deliberately not
/// in `stranded` — telling somebody to go and flatten a position that does not
/// exist is the mirror image of hiding one that does.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedObligation {
    pub intent_id: String,
    pub mint: String,
    pub size_lamports: i64,
    pub detail: String,
}

/// What the UI gets back from asking for an emergency unwind.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnwindReceipt {
    pub at_ms: i64,
    pub reason: String,
    pub actor: String,
    /// The halt that came with it, armed before anything else was attempted.
    pub kill_switch: KillSwitchReceipt,
    /// How many intents this call stepped from an active state to `aborted`.
    /// Zero on a second press: there is nothing left to stop managing.
    pub aborted: usize,
    /// How many signed exit transactions **this call** put on the network.
    ///
    /// Zero unless an execution backend is installed, and nothing installs one
    /// in the shipped application — see `execution.rs` and the roadmap's Phase
    /// 4 gate. It counts what this press dispatched, not how many positions
    /// have an exit out: an exit a previous unwind sent is in
    /// `exits_already_out` instead, because "what did pressing this just now
    /// do" is the question the number answers.
    ///
    /// **Dispatched is not closed.** A position leaves `stranded` when its exit
    /// confirms and not before. A UI that reads `exitsSent` and tells the
    /// operator the position is gone is telling them money they still own is
    /// not theirs to worry about.
    pub exits_sent: usize,
    /// How many exits confirmed: positions that are actually closed. Equal to
    /// `flattened.len()`.
    pub exits_confirmed: usize,
    /// How many exits are on the network and have not confirmed. Every one of
    /// them is still in `stranded`, because none of them has closed anything.
    pub exits_in_flight: usize,
    /// How many exits were attempted and did not go out, or went out and did
    /// not land.
    pub exits_failed: usize,
    /// How many positions already had an exit from an earlier press, which this
    /// call found rather than sent.
    pub exits_already_out: usize,
    /// Which execution backend did the flattening, or `None` when there was
    /// none and nothing could be sold.
    pub signer: Option<String>,
    /// Whether that backend can reach a real network. False for every backend
    /// that exists in this build.
    pub signer_live: bool,
    /// The positions that were sold, landed, and booked.
    pub flattened: Vec<FlattenedPosition>,
    /// What those came to, net of what they cost. Negative is a loss.
    pub realized_pnl_lamports: i64,
    /// Obligations that turned out to have nothing on chain behind them.
    pub resolved: Vec<ResolvedObligation>,
    /// Every position on chain that the engine is no longer managing and that
    /// **still has money at risk** — the ones this call abandoned, the ones
    /// that were already stranded, and the ones whose exit is out but not yet
    /// confirmed. A position that was flattened and confirmed is in
    /// `flattened`; one that never existed is in `resolved`.
    pub stranded: Vec<StrandedPosition>,
    /// False when the obligations could not be read. An empty `stranded` with
    /// this false means "unknown", not "nothing out there", and the two must
    /// not be shown the same way.
    pub stranded_known: bool,
    /// The `audit_log` row id for the unwind itself, or `None` if it could not
    /// be written. The halt in `kill_switch` carries its own, separately.
    pub audit_id: Option<i64>,
    /// What went wrong on the way, in the order it went wrong. Never a reason
    /// the halt did not take — everything in here is a failure to write down or
    /// read back what happened, after the engine had already stopped.
    pub problems: Vec<String>,
}

/// What one obligation looks like to whoever has to deal with it, or `None` if
/// there is in fact nothing on chain for it.
///
/// The `None` case should not arise from `open_obligations` — both arms of that
/// query mean money is out — but it is checked rather than assumed, because the
/// alternative is telling somebody to go flatten a position that was never
/// opened.
fn stranded_by(obligation: &OpenObligation) -> Option<StrandedPosition> {
    let at_risk_in = obligation.at_risk_in()?;
    Some(StrandedPosition {
        intent_id: obligation.intent_id.clone(),
        mint: obligation.mint.clone(),
        side: obligation.side,
        size_lamports: obligation.size_lamports,
        signature: obligation.signature.clone(),
        at_risk_in,
        mode: obligation.mode,
        conditional: at_risk_in == ExecutionState::Sent,
        exit: None,
    })
}

/// What was tried on the way out, as the receipt reports it.
///
/// `None` only for the outcomes that leave the position entirely alone. Every
/// other outcome puts something here, because a position that is still on chain
/// after an unwind that tried to sell it is a different situation from one
/// nothing was ever attempted for, and the operator has to be able to tell them
/// apart.
fn stranded_exit(outcome: &FlattenOutcome) -> Option<StrandedExit> {
    match outcome {
        // Handled by the caller: neither is still at risk.
        FlattenOutcome::Flattened { .. } | FlattenOutcome::ResolvedToNothing { .. } => None,
        FlattenOutcome::InFlight {
            exit_intent_id,
            signature,
            state,
            ..
        } => Some(StrandedExit {
            exit_intent_id: Some(exit_intent_id.clone()),
            signature: signature.clone(),
            state: Some(*state),
            failure: None,
            detail: "an exit is on the network and has not confirmed; it closes nothing until \
                     it does"
                .to_string(),
            on_network: true,
        }),
        FlattenOutcome::Failed {
            exit_intent_id,
            failure,
            detail,
            left_on_network,
        } => Some(StrandedExit {
            exit_intent_id: exit_intent_id.clone(),
            signature: None,
            state: Some(ExitState::ExitFailed),
            failure: Some(*failure),
            detail: detail.clone(),
            on_network: *left_on_network,
        }),
        FlattenOutcome::Unresolved { detail } | FlattenOutcome::Skipped { detail } => {
            Some(StrandedExit {
                exit_intent_id: None,
                signature: None,
                state: None,
                failure: None,
                detail: detail.clone(),
                on_network: false,
            })
        }
    }
}

/// The one sentence the telemetry line leads with.
///
/// Ordered by how bad it is to get wrong. Not knowing comes first, because an
/// empty list that was never read must never be shown as an empty list that
/// was. Positions still out there come next. "Nothing on chain" is last and is
/// only ever said when it is true.
fn unwind_headline(
    stranded_known: bool,
    stranded: &[StrandedPosition],
    flattened: &[FlattenedPosition],
    exits_sent: usize,
) -> String {
    if !stranded_known {
        return "emergency unwind: halted, but what is on chain could not be read".to_string();
    }
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    if !stranded.is_empty() {
        let sent = if exits_sent > 0 {
            format!(", {exits_sent} exit{} out", plural(exits_sent))
        } else {
            String::new()
        };
        return format!(
            "emergency unwind: halted with {} position{} still on chain{sent}",
            stranded.len(),
            plural(stranded.len())
        );
    }
    if !flattened.is_empty() {
        return format!(
            "emergency unwind: halted, {} position{} flattened and nothing left on chain",
            flattened.len(),
            plural(flattened.len())
        );
    }
    "emergency unwind: halted with nothing on chain".to_string()
}

/// The long-lived state every command reads.
pub struct Engine {
    started_at_ms: i64,
    kill_switch: AtomicBool,
    shutting_down: AtomicBool,
    stopped: AtomicBool,
    kill_switch_at_ms: AtomicU64,
    // Held behind `Arc` so the ingestion layer's WAL worker and telemetry task
    // can hold the same database and the same hub the commands do, rather than
    // opening a second connection or a second fan-out point.
    db: Arc<Database>,
    telemetry: Arc<TelemetryHub>,
    maintenance: Maintenance,
    /// The outbound signer, or `None` on a build that cannot send anything.
    ///
    /// A lock rather than a field because it is installed after `start` — the
    /// engine has to exist before anything can be given one — and read from
    /// every thread that might have to get out of a position. It is never
    /// replaced once set; swapping a signer under an exit that is mid-flight is
    /// not a thing this should make possible.
    execution: RwLock<Option<Arc<dyn ExecutionEngine>>>,
    /// Held for the length of one flattening pass. See `FLATTEN_LOCK_TIMEOUT`.
    flattening: Mutex<()>,
    /// Where the execution and signer state counters go, once something has
    /// been attached.
    ///
    /// A `OnceLock` rather than the `RwLock` the backend uses, because this is
    /// read on the exit path and read by the panic hook's neighbours, and a
    /// lock taken to count something is a lock that can be held when the thing
    /// being counted is an emergency. It is also set exactly once, at startup,
    /// which is the shape `OnceLock` is for.
    metrics: OnceLock<Arc<MetricsCollector>>,
    /// Where anomalies go, once something has been attached.
    ///
    /// A `OnceLock` for the same reasons as `metrics` above, and set from the
    /// same place at startup. Nothing about an unwind depends on it being
    /// there: an engine with no dispatcher writes the same journal rows and
    /// reaches the same outcomes, and simply tells nobody about the ones that
    /// were unusual.
    alerting: OnceLock<Arc<AlertDispatcher>>,
}

impl Engine {
    /// Opens the database, starts the telemetry pump and the maintenance
    /// timers, and announces itself.
    pub fn start(db: Database) -> Self {
        Self::start_with(db, MaintenanceSchedule::default())
    }

    /// `start`, with the maintenance periods passed in. Only the tests want
    /// this; everything else wants the real schedule.
    pub fn start_with(db: Database, schedule: MaintenanceSchedule) -> Self {
        let db = Arc::new(db);
        let telemetry = Arc::new(TelemetryHub::start());
        let maintenance = Maintenance::start(Arc::clone(&db), Arc::clone(&telemetry), schedule);

        let engine = Self {
            started_at_ms: now_ms(),
            kill_switch: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            kill_switch_at_ms: AtomicU64::new(0),
            db,
            telemetry,
            maintenance,
            execution: RwLock::new(None),
            flattening: Mutex::new(()),
            metrics: OnceLock::new(),
            alerting: OnceLock::new(),
        };

        engine.telemetry.publish(
            TelemetryLevel::Info,
            "lifecycle",
            "engine started",
            serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "checkpointEveryMs": schedule.checkpoint_every.as_millis() as u64,
                "pruneEveryMs": schedule.prune_every.as_millis() as u64,
                "retainTicksForMs": schedule.retain_ticks_for.as_millis() as u64,
            }),
        );

        engine
    }

    /// The database, for the parts of the engine that write to it from their
    /// own thread. One file, one connection, one lock — see `db.rs`.
    pub fn database(&self) -> Arc<Database> {
        Arc::clone(&self.db)
    }

    /// Gives the engine somewhere to count its executions.
    ///
    /// Returns false, and changes nothing, if one is already attached. An
    /// engine that counted the same exit into two collectors would report the
    /// same position twice, which is the one way a gauge can lie about money.
    ///
    /// Nothing about the engine depends on this having been called. Without a
    /// collector the counters simply are not kept, and every path behaves the
    /// same way it did before there were any.
    pub fn attach_metrics(&self, metrics: Arc<MetricsCollector>) -> bool {
        self.metrics.set(metrics).is_ok()
    }

    /// The attached collector, if there is one.
    pub fn metrics(&self) -> Option<&Arc<MetricsCollector>> {
        self.metrics.get()
    }

    /// Gives the engine somewhere to raise anomalies. Once, like the collector.
    pub fn attach_alerting(&self, alerting: Arc<AlertDispatcher>) -> bool {
        self.alerting.set(alerting).is_ok()
    }

    /// The attached dispatcher, if there is one.
    pub fn alerting(&self) -> Option<&Arc<AlertDispatcher>> {
        self.alerting.get()
    }

    /// Gives the engine something that can sign and send an exit.
    ///
    /// Returns false, and changes nothing, if a backend is already installed.
    /// Refusing rather than replacing is deliberate: an exit that was signed by
    /// one backend and is confirmed by another is a position nobody can account
    /// for, and there is no case where swapping mid-session is the right answer
    /// — a different signer is a different process.
    ///
    /// Nothing in `run()` calls this. The application ships with no backend, so
    /// an unwind halts, abandons and reports, and sells nothing. Installing one
    /// is the roadmap's Phase 4 promotion, and `ExecutionEngine::is_live` is
    /// how a backend says which side of that gate it is on.
    pub fn install_execution_engine(&self, backend: Arc<dyn ExecutionEngine>) -> bool {
        let mut slot = self.execution.write();
        if slot.is_some() {
            return false;
        }
        let (name, live) = (backend.name(), backend.is_live());
        *slot = Some(backend);
        drop(slot);

        self.telemetry.publish(
            if live {
                TelemetryLevel::Warn
            } else {
                TelemetryLevel::Info
            },
            "execution",
            format!("execution backend installed: {name}"),
            serde_json::json!({ "backend": name, "live": live }),
        );
        true
    }

    /// The installed backend, if there is one.
    pub fn execution_engine(&self) -> Option<Arc<dyn ExecutionEngine>> {
        self.execution.read().clone()
    }

    /// The telemetry hub, for the parts of the engine that publish from their
    /// own thread.
    pub fn telemetry(&self) -> Arc<TelemetryHub> {
        Arc::clone(&self.telemetry)
    }

    /// True once the switch has been pulled, from anywhere.
    pub fn is_halted(&self) -> bool {
        self.kill_switch.load(Ordering::SeqCst)
    }

    /// True once shutdown has begun. Work started after this should not be.
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// When the switch was armed. `None` until it has been.
    pub fn kill_switch_at_ms(&self) -> Option<i64> {
        match self.kill_switch_at_ms.load(Ordering::SeqCst) {
            0 => None,
            at => Some(at as i64),
        }
    }

    fn lifecycle(&self) -> EngineLifecycle {
        if self.stopped.load(Ordering::SeqCst) {
            EngineLifecycle::Stopped
        } else if self.is_shutting_down() {
            EngineLifecycle::ShuttingDown
        } else if self.is_halted() {
            EngineLifecycle::Halted
        } else {
            EngineLifecycle::Running
        }
    }

    /// Reads the current state. Cheap enough for the UI to poll.
    pub fn status(&self) -> Result<EngineStatus, EngineError> {
        Ok(EngineStatus {
            state: self.lifecycle(),
            kill_switch_armed: self.is_halted(),
            kill_switch_at_ms: self.kill_switch_at_ms(),
            shutting_down: self.is_shutting_down(),
            started_at_ms: self.started_at_ms,
            uptime_ms: now_ms().saturating_sub(self.started_at_ms),
            version: env!("CARGO_PKG_VERSION"),
            telemetry: self.telemetry.snapshot(),
            database: self.db.health()?,
            maintenance: self.maintenance.snapshot(),
        })
    }

    /// Arms the kill switch: halts the engine and writes down that it happened.
    ///
    /// There is no matching `disarm`. Something went wrong enough to pull this,
    /// and deciding it is safe to resume is a judgement for a person restarting
    /// the process, not a button.
    pub fn trigger_kill_switch(&self, reason: &str, actor: &str) -> KillSwitchReceipt {
        let at_ms = now_ms();
        // `swap` rather than `store`, so a second press can be reported honestly
        // instead of silently looking like the first.
        let already_armed = self.kill_switch.swap(true, Ordering::SeqCst);
        if !already_armed {
            self.kill_switch_at_ms.store(at_ms as u64, Ordering::SeqCst);
        }

        let payload = serde_json::json!({
            "reason": reason,
            "actor": actor,
            "alreadyArmed": already_armed,
            "uptimeMs": at_ms.saturating_sub(self.started_at_ms),
        });

        self.telemetry.publish(
            TelemetryLevel::Error,
            "kill_switch",
            format!("kill switch armed: {reason}"),
            payload.clone(),
        );

        // The switch is already armed by this point. A database that cannot take
        // the row does not get to undo that, so the error becomes a `None` id and
        // a warning, not a failed command.
        let audit_id = match self.db.record_audit(KILL_SWITCH_EVENT, &payload, at_ms) {
            Ok(id) => Some(id),
            Err(err) => {
                self.telemetry.publish(
                    TelemetryLevel::Warn,
                    "kill_switch",
                    "kill switch armed but the audit row could not be written",
                    serde_json::json!({ "error": err.to_string() }),
                );
                None
            }
        };

        KillSwitchReceipt {
            armed: true,
            already_armed,
            at_ms,
            reason: reason.to_string(),
            audit_id,
        }
    }

    /// Halts the engine, stops managing every position that has money out,
    /// flattens what it can, and reports exactly what is left on chain.
    ///
    /// Three things happen, in this order, and the order is the point.
    ///
    /// **It halts.** The kill switch is armed before anything that can fail is
    /// attempted, so an operator holding this button always gets a receipt
    /// rather than an error to interpret while trying to stop trading.
    ///
    /// **It abandons.** Every selected intent with money out is walked to
    /// `aborted`, so nothing keeps acting on a position nobody is watching.
    /// Aborting has never meant closing — there is no transaction that un-sends
    /// another one — and `needs_unwind` is what records that something was left
    /// behind.
    ///
    /// **Then, and only then, it sells.** If an execution backend is installed,
    /// each abandoned position is routed, built, signed, broadcast and
    /// confirmed as its own new intent — never as an edit to the old one, per
    /// `RISK_AND_SYBIL_SPEC.md` U2. Nothing installs a backend in the shipped
    /// application, so on that build this step does nothing and `exits_sent`
    /// comes back zero. The selling is last because it is the only part that
    /// can hang, refuse or be absent, and none of that may delay the engine
    /// stopping.
    ///
    /// A position leaves `stranded` when its exit **confirms**, and not before.
    /// One with an exit merely on the network is still in the list, carrying
    /// `exit.on_network`, because it has closed nothing yet.
    ///
    /// Infallible on purpose, exactly like `trigger_kill_switch`: the switch is
    /// armed before anything that can fail is attempted, so an operator holding
    /// this button always gets a receipt describing what happened rather than
    /// an error to interpret while trying to stop trading. Failures below that
    /// point land in `problems` and, when they cost the list itself, in
    /// `stranded_known`.
    ///
    /// `intent_ids` selects which obligations to give up on. `None` is every
    /// one of them, which is the panic-button case. A list is the operator
    /// picking, which is what the window does — it sends only the obligations
    /// it has reconciled, because a `sent` transaction that never landed is not
    /// something to act on yet.
    ///
    /// Pressing it twice is safe and is not a no-op worth hiding: the second
    /// press aborts nothing, because there is nothing active left, and returns
    /// the same obligations — which are still open, because nothing here closed
    /// them.
    pub fn emergency_unwind(
        &self,
        intent_ids: Option<&[String]>,
        reason: &str,
        actor: &str,
    ) -> UnwindReceipt {
        // Before the read, before the writes, before anything that can fail.
        let kill_switch = self.trigger_kill_switch(reason, actor);
        let at_ms = now_ms();
        let mut problems = Vec::new();

        let (mut obligations, stranded_known) = match self.db.open_obligations() {
            Ok(found) => (found, true),
            Err(err) => {
                problems.push(format!("the open obligations could not be read: {err}"));
                (Vec::new(), false)
            }
        };

        // A selection narrows what is given up on; it never widens it. An id
        // that names nothing open is reported rather than ignored — the window
        // asked about a position it believes it has, and the two disagreeing is
        // worth saying out loud.
        if let Some(selected) = intent_ids {
            if stranded_known {
                for id in selected {
                    if !obligations.iter().any(|o| &o.intent_id == id) {
                        problems.push(format!("{id} has no open obligation to unwind"));
                    }
                }
            }
            obligations.retain(|o| selected.iter().any(|id| id == &o.intent_id));
        }

        let mut rows = Vec::with_capacity(obligations.len());
        let mut stranded = Vec::new();
        for obligation in &obligations {
            // `abort` refuses a terminal state, which is the obligation the
            // engine already gave up on and nobody has flattened since. That
            // row is history and stays untouched; it is still an open
            // obligation and still belongs in the list.
            if let Ok(outcome) = obligation.state.abort(AbortReason::Operator) {
                rows.push(ExecutionLogRow::aborted(
                    obligation.intent_id.clone(),
                    obligation.seq + 1,
                    obligation.mint.clone(),
                    outcome,
                    obligation.side,
                    obligation.size_lamports,
                    // Deliberately not the signature off the row this came
                    // from. That signature belongs to the `sent` step and the
                    // unique partial index on the column means it belongs to
                    // exactly one row; copying it forward would fail the insert
                    // and roll back the whole batch. This step sent nothing, so
                    // it has no signature of its own.
                    None,
                    obligation.mode,
                    at_ms,
                ));
            }
            match stranded_by(obligation) {
                Some(position) => stranded.push(position),
                // The row calls itself an open obligation and its own history
                // says nothing ever went out. `RISK_AND_SYBIL_SPEC.md` U1 gives
                // `needs_unwind` one source, so a row disagreeing with the
                // states around it means a writer set it by hand. Reported
                // rather than dropped: a receipt that quietly omits something
                // claiming to be an open obligation is the exact failure this
                // command exists to prevent.
                None if obligation.needs_unwind => problems.push(format!(
                    "{} is flagged as an open obligation but its history has nothing on chain",
                    obligation.intent_id
                )),
                None => {}
            }
        }

        // Counted before the write, for the reason the flattener counts before
        // its own: these positions are abandoned in fact — the engine is halted
        // and nothing is managing them — whether or not the rows saying so
        // reach the disk. Most of these will also bump `unobserved`, because an
        // obligation read back out of `sts.db` was in flight long before this
        // collector existed. That is the counter working, not a fault.
        if let Some(metrics) = self.metrics.get() {
            for row in &rows {
                metrics.record_intent(row.prev_state, row.state);
            }
        }

        let aborted = if rows.is_empty() {
            0
        } else {
            match self.db.record_execution_logs(&rows) {
                Ok(written) => written,
                Err(err) => {
                    // The positions are abandoned in fact either way — the
                    // engine is halted and nothing is managing them. What was
                    // lost is the record of it, which is why this is a problem
                    // and not a shorter `stranded` list.
                    problems.push(format!("the abort rows could not be written: {err}"));
                    0
                }
            }
        };

        // Only now, after the halt and after the abandonment, does anything
        // try to sell. Both of those are unconditional and neither depends on a
        // signer existing; the flattening is the part that does, and it is last
        // so that a backend which hangs, refuses or is absent cannot delay the
        // engine stopping.
        let targets: Vec<ExitTarget> = obligations
            .iter()
            .filter_map(ExitTarget::from_obligation)
            .collect();
        let report = self.flatten(&targets, &mut problems);
        problems.extend(report.problems.iter().cloned());

        let mut flattened = Vec::new();
        let mut resolved = Vec::new();
        let mut still_out = Vec::new();
        for position in stranded {
            let outcome = report
                .results
                .iter()
                .find(|result| result.target.intent_id == position.intent_id)
                .map(|result| &result.outcome);
            match outcome {
                Some(FlattenOutcome::Flattened {
                    exit_intent_id,
                    signature,
                    venue,
                    tokens,
                    out_lamports,
                    realized_pnl_lamports,
                    ..
                }) => flattened.push(FlattenedPosition {
                    intent_id: position.intent_id,
                    exit_intent_id: exit_intent_id.clone(),
                    mint: position.mint,
                    venue: *venue,
                    signature: signature.clone(),
                    tokens: *tokens,
                    cost_basis_lamports: position.size_lamports,
                    out_lamports: *out_lamports,
                    realized_pnl_lamports: *realized_pnl_lamports,
                    mode: position.mode,
                }),
                Some(FlattenOutcome::ResolvedToNothing { detail }) => {
                    resolved.push(ResolvedObligation {
                        intent_id: position.intent_id,
                        mint: position.mint,
                        size_lamports: position.size_lamports,
                        detail: detail.clone(),
                    })
                }
                Some(other) => still_out.push(StrandedPosition {
                    exit: stranded_exit(other),
                    ..position
                }),
                None => still_out.push(position),
            }
        }
        let stranded = still_out;
        let realized_pnl_lamports = report.realized_pnl_lamports();

        let payload = serde_json::json!({
            "reason": reason,
            "actor": actor,
            "aborted": aborted,
            "signer": report.backend,
            "signerLive": report.live,
            "exitsSent": report.exits_sent(),
            "exitsConfirmed": report.exits_confirmed(),
            "exitsInFlight": report.exits_in_flight(),
            "exitsFailed": report.exits_failed(),
            "exitsAlreadyOut": report.exits_already_out(),
            "realizedPnlLamports": realized_pnl_lamports,
            "flattened": flattened,
            "resolved": resolved,
            "stranded": stranded,
            "strandedKnown": stranded_known,
            "problems": problems,
        });

        // Deliberately not `unwind`. That source is the per-obligation channel
        // — `{ intentId, resolved, outcome }`, the engine's word that one
        // obligation is closed — and this event is the opposite fact: what is
        // still open. Publishing it there would put a payload with no
        // `intentId` through a handler whose whole job is closing one.
        self.telemetry.publish(
            TelemetryLevel::Error,
            "emergency_unwind",
            unwind_headline(stranded_known, &stranded, &flattened, report.exits_sent()),
            payload.clone(),
        );

        let audit_id = match self
            .db
            .record_audit(EMERGENCY_UNWIND_EVENT, &payload, at_ms)
        {
            Ok(id) => Some(id),
            Err(err) => {
                problems.push(format!("the audit row could not be written: {err}"));
                None
            }
        };

        UnwindReceipt {
            at_ms,
            reason: reason.to_string(),
            actor: actor.to_string(),
            kill_switch,
            aborted,
            exits_sent: report.exits_sent(),
            exits_confirmed: report.exits_confirmed(),
            exits_in_flight: report.exits_in_flight(),
            exits_failed: report.exits_failed(),
            exits_already_out: report.exits_already_out(),
            signer: match report.backend.as_str() {
                "none" => None,
                name => Some(name.to_string()),
            },
            signer_live: report.live,
            flattened,
            realized_pnl_lamports,
            resolved,
            stranded,
            stranded_known,
            audit_id,
            problems,
        }
    }

    /// Runs one flattening pass, or explains why it did not.
    ///
    /// Separate from `emergency_unwind` because it is the only part of an
    /// unwind that can be absent. Everything above it — the halt, the
    /// abandonment, the list — happens on every build; this happens only where
    /// something has been installed that can sign.
    fn flatten(&self, targets: &[ExitTarget], problems: &mut Vec<String>) -> FlattenReport {
        let Some(backend) = self.execution_engine() else {
            return FlattenReport::nothing_attempted();
        };
        if targets.is_empty() {
            return FlattenReport {
                backend: backend.name().to_string(),
                live: backend.is_live(),
                results: Vec::new(),
                problems: Vec::new(),
            };
        }

        // Bounded rather than indefinite: whoever pressed this is having a bad
        // minute and a receipt that arrives late is worse than one that says a
        // pass was skipped. The positions are abandoned either way — that half
        // has already happened by the time this is reached.
        let Some(_pass) = self.flattening.try_lock_for(FLATTEN_LOCK_TIMEOUT) else {
            problems.push(format!(
                "another unwind was still flattening after {FLATTEN_LOCK_TIMEOUT:?}, so this one                  sent nothing — the positions are abandoned and are still on chain"
            ));
            return FlattenReport::nothing_attempted();
        };

        let mut flattener = Flattener::new(backend.as_ref(), &self.db, now_ms());
        if let Some(metrics) = self.metrics.get() {
            flattener = flattener.with_metrics(metrics.as_ref());
        }
        if let Some(alerting) = self.alerting.get() {
            flattener = flattener.alerting_through(alerting.as_ref());
        }
        flattener.flatten(targets)
    }

    /// The panic-path version of the above.
    ///
    /// Touches only atomics and the best-effort audit write, because the thread
    /// calling this is unwinding and may already hold any lock in the process.
    /// It cannot fail and it cannot block for long.
    pub fn arm_from_panic(&self, location: &str, message: &str) {
        let at_ms = now_ms();
        if !self.kill_switch.swap(true, Ordering::SeqCst) {
            self.kill_switch_at_ms.store(at_ms as u64, Ordering::SeqCst);
        }
        // Deliberately not `shutting_down`: that would make
        // `subscribe_telemetry` refuse the window a stream, and a panic is
        // exactly when someone needs to read one. Halted is visible in the
        // status; quitting is their call.

        let payload = serde_json::json!({
            "reason": "panic",
            "actor": "panic_hook",
            "location": location,
            "message": message,
        });
        self.db
            .record_audit_best_effort(KILL_SWITCH_EVENT, &payload, at_ms);
    }

    /// Hands a window's channel to the telemetry hub.
    pub fn subscribe_telemetry(
        &self,
        channel: Channel<TelemetryEvent>,
    ) -> Result<TelemetrySubscription, EngineError> {
        if self.is_shutting_down() {
            return Err(EngineError::ShuttingDown(
                "the engine is closing and will not start a new telemetry stream".to_string(),
            ));
        }

        let subscription = self.telemetry.subscribe(channel);
        self.telemetry.publish(
            TelemetryLevel::Debug,
            "lifecycle",
            "a window subscribed to telemetry",
            serde_json::json!({ "subscriberId": subscription.subscriber_id }),
        );
        Ok(subscription)
    }

    /// Marks the start of shutdown. Returns true only for the call that won,
    /// so the caller can log once even if the window and the runtime both ask.
    pub fn begin_shutdown(&self) -> bool {
        let first = !self.shutting_down.swap(true, Ordering::SeqCst);
        if first {
            self.telemetry.publish(
                TelemetryLevel::Info,
                "lifecycle",
                "shutting down",
                serde_json::json!({ "killSwitchArmed": self.is_halted() }),
            );
        }
        first
    }

    /// Joins the workers and checkpoints the database. Idempotent.
    ///
    /// Ordering matters. Maintenance stops first because it is the one thing
    /// left that both publishes telemetry and takes the connection lock, and
    /// `Database::close` gives up on that lock after 250 ms rather than waiting
    /// — a prune still running here would cost the final `TRUNCATE` checkpoint.
    /// Telemetry stops next, so nothing is still trying to reach a window that
    /// is already gone. The database closes last, so a final audit row from
    /// anywhere still lands.
    pub fn finish_shutdown(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        self.maintenance.stop();
        self.telemetry.shutdown();
        self.db.close();
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{ExecutionMode, Side, TickMetricRow};
    use crate::execution::{
        MockFault, MockPosition, MockSolanaSigner, RaydiumPool, EMERGENCY_MAX_SLIPPAGE_BPS,
    };
    use std::path::{Path, PathBuf};

    /// How long a test will wait for the maintenance thread before deciding it
    /// is not going to happen. Generous on purpose: the assertion is that the
    /// timer runs at all, and a loaded machine should fail this suite for a
    /// real reason or not at all.
    const PATIENCE: Duration = Duration::from_secs(5);

    /// A schedule tight enough to watch. The two retention windows are short
    /// rather than zero so a row written now survives its own prune.
    fn brisk() -> MaintenanceSchedule {
        MaintenanceSchedule {
            checkpoint_every: Duration::from_millis(20),
            prune_every: Duration::from_millis(20),
            retain_ticks_for: Duration::from_secs(2),
            snapshot_every: Duration::from_millis(20),
            retain_state_log_for: Duration::from_secs(2),
        }
    }

    struct TempDb(PathBuf);

    impl TempDb {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "sts-engine-{name}-{}-{}.db",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let temp = TempDb(path);
            temp.remove();
            temp
        }

        fn open(&self) -> Database {
            Database::open(&self.0).expect("opens")
        }

        fn path(&self) -> &Path {
            &self.0
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

    /// Polls `status()` until `done` is happy or `PATIENCE` runs out.
    fn wait_for(
        engine: &Engine,
        what: &str,
        done: impl Fn(&MaintenanceSnapshot) -> bool,
    ) -> MaintenanceSnapshot {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let snapshot = engine.status().expect("status").maintenance;
            if done(&snapshot) {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "waited {PATIENCE:?} for {what} and it never happened: {snapshot:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// One intent's history, ending wherever the last step says.
    fn history(intent: &str, steps: &[(ExecutionState, Option<&str>)]) -> Vec<ExecutionLogRow> {
        let mut prev = None;
        let mut rows = Vec::new();
        for (seq, (state, signature)) in steps.iter().enumerate() {
            rows.push(ExecutionLogRow {
                intent_id: intent.to_string(),
                seq: seq as i64,
                mint: format!("Mint{intent}"),
                state: *state,
                prev_state: prev,
                side: Side::Buy,
                size_lamports: 250_000_000,
                price_q18: None,
                signature: signature.map(str::to_string),
                latency_ms: None,
                needs_unwind: false,
                mode: ExecutionMode::Live,
                abort_reason: None,
                at_ms: 1_700_000_000_000 + seq as i64,
            });
            prev = Some(*state);
        }
        rows
    }

    // -- the maintenance timers ---------------------------------------------

    #[test]
    fn the_checkpoint_timer_folds_the_wal_without_being_asked() {
        let temp = TempDb::new("checkpoint-timer");
        let engine = Engine::start_with(temp.open(), brisk());

        let after = wait_for(&engine, "a passive checkpoint", |m| m.checkpoints > 0);
        assert!(after.running, "the thread is still there between passes");
        assert_eq!(after.checkpoint_failures, 0);
        assert!(after.last_checkpoint_at_ms.is_some(), "and it said when");

        // It keeps going rather than firing once.
        let first = after.checkpoints;
        wait_for(&engine, "a second checkpoint", |m| m.checkpoints > first);

        engine.finish_shutdown();
    }

    #[test]
    fn the_prune_timer_drops_ticks_past_the_retention_window_and_keeps_the_rest() {
        let temp = TempDb::new("prune-timer");
        let db = temp.open();

        // Written before the engine exists, so the first pass has something to
        // find. The retention window is two seconds, so an hour ago is well
        // outside it and now is well inside.
        let now = now_ms();
        let stale: Vec<TickMetricRow> = (0..50)
            .map(|tick| TickMetricRow {
                rpc_endpoint: "helius".to_string(),
                timestamp_ms: now - 3_600_000 + tick,
                latency_ms: 40,
                dropped_msgs: 0,
                parsed_per_sec_micros: 812_500_000,
            })
            .collect();
        let fresh = TickMetricRow {
            rpc_endpoint: "helius".to_string(),
            timestamp_ms: now,
            latency_ms: 41,
            dropped_msgs: 0,
            parsed_per_sec_micros: 900_000_000,
        };
        db.record_tick_metrics(&stale).expect("writes");
        db.record_tick_metrics(&[fresh]).expect("writes");

        let engine = Engine::start_with(db, brisk());
        let after = wait_for(&engine, "a prune", |m| m.prunes > 0);
        assert_eq!(after.prune_failures, 0);
        assert_eq!(after.ticks_pruned, 50, "the stale ticks and only those");
        assert!(after.last_prune_at_ms.is_some());

        engine.finish_shutdown();

        // And it is true of the file, not just the counter.
        let conn = rusqlite::Connection::open(temp.path()).expect("reopens");
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM tick_metrics", [], |row| row.get(0))
            .expect("counts");
        assert_eq!(left, 1, "the tick inside the window survived its own prune");
    }

    #[test]
    fn the_snapshot_timer_checkpoints_the_book_and_only_when_it_moved() {
        use crate::forensics::{Decision, StateRecord};
        use crate::journal::TradeRow;
        use crate::strategy::GateReason;
        use crate::types::{
            CircuitBreaker, FastPathGate, LiquidityThresholds, OperatingMode, RiskSnapshot,
        };

        let temp = TempDb::new("snapshot-timer");
        let db = temp.open();

        // A book and a log for the first pass to find. Timestamps are `now`
        // rather than a fixed instant, because the brisk schedule's retention
        // window is two seconds and rows from 2023 would be pruned before the
        // first assertion.
        let now = now_ms();
        db.record_journal_trades(&[TradeRow::opened(
            "t-1",
            "So11111111111111111111111111111111111111112",
            crate::db::Side::Buy,
            ExecutionMode::Paper,
            500_000_000,
            now,
        )])
        .expect("writes");

        let risk = RiskSnapshot {
            at_ms: now,
            mode: OperatingMode::Paper,
            equity_lamports: 0,
            high_water_lamports: 0,
            drawdown_bps: 0,
            max_drawdown_bps: 10_000,
            open_positions: 0,
            max_open_positions: 4,
            circuit_breaker: CircuitBreaker::Clear,
            fast_path: FastPathGate::CLOSED,
            liquidity: LiquidityThresholds {
                min_pool_lamports: 0,
                exit_only_below_lamports: 0,
                max_pool_share_bps: 150,
            },
        };
        let verdict = crate::strategy::syndicate::GateVerdict {
            enter: false,
            reason: GateReason::LowScore,
            confidence_micros: 100_000,
            tags: Vec::new(),
            thin: false,
            bundle_wallets: 0,
            bundle_lamports: 0,
            cohort_wallets: 0,
            cohort_lamports: 0,
            cohort_size_lamports: None,
            cohort_delta_bps: None,
            cohort_external: 0,
            rings: Vec::new(),
            sandwich: None,
        };
        let record = StateRecord::decided(
            "mint-a",
            &verdict,
            &risk,
            Decision::Refused,
            None,
            3,
            0,
            true,
            now,
            now,
        );
        db.record_state_log(ExecutionMode::Paper, std::slice::from_ref(&record), now)
            .expect("writes");

        let engine = Engine::start_with(db, brisk());
        // Three, not one: the pass covers all three modes, and a mode with
        // nothing in it gets the genesis link of its own chain. Waiting for one
        // would be waiting for whichever came first, which is `live`.
        let after = wait_for(&engine, "a checkpoint of every mode", |m| m.snapshots >= 3);
        assert_eq!(after.snapshot_failures, 0);
        assert!(after.last_snapshot_at_ms.is_some(), "and it said when");
        assert_eq!(
            after.last_snapshot_revision, 1,
            "the highest revision checkpointed is paper's one logged row"
        );

        // The timer keeps firing; the counter does not, because a book that has
        // not moved is a book already checkpointed. Deliberately a sleep rather
        // than a `wait_for`: the assertion is that nothing happens, and the
        // only way to see nothing happen is to let the timer fire a few times.
        let settled = after.snapshots;
        std::thread::sleep(Duration::from_millis(200));
        let quiet = engine.status().expect("status").maintenance;
        assert!(
            quiet.checkpoints > after.checkpoints,
            "the loop stopped turning"
        );
        assert_eq!(
            quiet.snapshots,
            settled,
            "a quiet weekend wrote {} more identical checkpoints",
            quiet.snapshots.saturating_sub(settled)
        );

        // Move the log, and the next pass records it.
        engine
            .database()
            .record_state_log(ExecutionMode::Paper, &[record], now_ms())
            .expect("writes");
        wait_for(&engine, "a second checkpoint", |m| m.snapshots > settled);

        engine.finish_shutdown();

        // And it is true of the file rather than only of the counters. Two
        // checkpoints, chained, and the chain verifies.
        let db = Database::open(temp.path()).expect("reopens");
        let chain = db
            .verify_journal_snapshot_chain(ExecutionMode::Paper)
            .expect("walks");
        assert!(
            chain.is_intact(),
            "the timer wrote a chain that does not verify: {chain:?}"
        );
        assert_eq!(
            chain.snapshots, 2,
            "the first pass and the one after the log moved"
        );
        let warm = db.warm_start(ExecutionMode::Paper).expect("reads");
        assert!(warm.is_clean());
        assert_eq!(warm.uncheckpointed, 0);
    }

    #[test]
    fn shutting_down_joins_the_maintenance_thread_and_says_so() {
        let temp = TempDb::new("maintenance-shutdown");
        let engine = Engine::start_with(temp.open(), brisk());
        wait_for(&engine, "the timers to run at all", |m| {
            m.checkpoints > 0 && m.prunes > 0
        });

        engine.finish_shutdown();
        let stopped = engine.status().expect("status").maintenance;
        assert!(!stopped.running, "the thread was joined, not left behind");

        // Twice is not an error, and does not hang on a thread already joined.
        engine.finish_shutdown();
        assert!(!engine.status().expect("status").maintenance.running);
    }

    #[test]
    fn stopping_wakes_the_timer_out_of_a_sleep_it_would_otherwise_finish() {
        let temp = TempDb::new("maintenance-wake");
        // An interval far longer than any test will wait. If `stop` waited for
        // it rather than interrupting it, this test would take an hour.
        let engine = Engine::start_with(
            temp.open(),
            MaintenanceSchedule {
                checkpoint_every: Duration::from_secs(3_600),
                prune_every: Duration::from_secs(3_600),
                retain_ticks_for: TICK_RETENTION,
                snapshot_every: Duration::from_secs(3_600),
                retain_state_log_for: STATE_LOG_RETENTION,
            },
        );

        let began = Instant::now();
        engine.finish_shutdown();
        assert!(
            began.elapsed() < PATIENCE,
            "stop waited out the interval instead of interrupting it"
        );
        assert!(!engine.status().expect("status").maintenance.running);
    }

    // -- emergency unwind ---------------------------------------------------

    #[test]
    fn an_emergency_unwind_halts_abandons_what_is_open_and_names_what_is_left() {
        use ExecutionState::*;
        let temp = TempDb::new("unwind");
        let db = temp.open();

        db.record_execution_logs(&history(
            "sent-only",
            &[
                (IntentCreated, None),
                (Validated, None),
                (Sent, Some("SigSent")),
            ],
        ))
        .expect("writes");
        db.record_execution_logs(&history(
            "confirmed",
            &[
                (IntentCreated, None),
                (Validated, None),
                (Sent, Some("SigConf")),
                (Confirmed, None),
            ],
        ))
        .expect("writes");
        // Never left the ground: there is nothing on chain for this one.
        db.record_execution_logs(&history("planned", &[(IntentCreated, None)]))
            .expect("writes");

        let engine = Engine::start_with(db, brisk());
        let receipt = engine.emergency_unwind(None, "the feed went quiet", "test");

        assert!(engine.is_halted(), "the engine stops before anything else");
        assert!(receipt.kill_switch.armed);
        assert!(!receipt.kill_switch.already_armed);
        assert_eq!(receipt.aborted, 2, "the two with money out, not the plan");
        assert_eq!(receipt.exits_sent, 0, "nothing here can send a transaction");
        assert!(receipt.stranded_known);
        assert!(receipt.problems.is_empty(), "{:?}", receipt.problems);
        assert!(receipt.audit_id.is_some());

        assert_eq!(receipt.stranded.len(), 2);
        let by_id = |id: &str| {
            receipt
                .stranded
                .iter()
                .find(|p| p.intent_id == id)
                .expect("stranded")
                .clone()
        };

        let sent = by_id("sent-only");
        assert_eq!(sent.at_risk_in, Sent);
        assert!(
            sent.conditional,
            "a sent transaction may never have landed and has to be reconciled first"
        );
        assert_eq!(sent.signature.as_deref(), Some("SigSent"));
        assert_eq!(sent.size_lamports, 250_000_000);
        assert_eq!(sent.side, Side::Buy);

        let confirmed = by_id("confirmed");
        assert_eq!(confirmed.at_risk_in, Confirmed);
        assert!(
            !confirmed.conditional,
            "this one is a position, not a maybe"
        );
        assert_eq!(
            confirmed.signature.as_deref(),
            Some("SigConf"),
            "the handle for finding it, kept even though the newest row has no signature"
        );

        // Both abort rows landed, and none of them claimed the signature that
        // belongs to the `sent` step — the unique index would have failed the
        // insert and rolled the whole batch back, so `aborted` proves it.
        let health = engine.database().health().expect("health");
        assert_eq!(health.needs_unwind, 2, "both left an open obligation");

        // Nothing is being managed any more: every obligation still out there
        // is now one the engine has given up on rather than one in flight.
        let left = engine.database().open_obligations().expect("reads");
        assert_eq!(left.len(), 2);
        assert!(
            left.iter().all(|o| o.state == Aborted && o.needs_unwind),
            "an intent still in flight after an unwind is one nobody stopped: {left:?}"
        );

        engine.finish_shutdown();
    }

    #[test]
    fn an_unwind_with_nothing_on_chain_still_halts_and_says_there_is_nothing() {
        let temp = TempDb::new("unwind-empty");
        let engine = Engine::start_with(temp.open(), brisk());

        let receipt = engine.emergency_unwind(None, "belt and braces", "test");
        assert!(engine.is_halted());
        assert_eq!(receipt.aborted, 0);
        assert!(receipt.stranded.is_empty());
        assert!(
            receipt.stranded_known,
            "an empty list that was actually read is different from one that was not"
        );
        assert!(receipt.problems.is_empty());

        engine.finish_shutdown();
    }

    #[test]
    fn unwinding_while_already_halted_still_abandons_the_position() {
        use ExecutionState::*;
        let temp = TempDb::new("unwind-halted");
        let db = temp.open();
        db.record_execution_logs(&history(
            "confirmed",
            &[
                (IntentCreated, None),
                (Validated, None),
                (Sent, Some("SigHalt")),
                (Confirmed, None),
            ],
        ))
        .expect("writes");

        let engine = Engine::start_with(db, brisk());
        engine.trigger_kill_switch("something else tripped first", "test");

        // The named regression for RISK_AND_SYBIL_SPEC.md U3. An engine that
        // refuses to let go of the positions that tripped its own breaker looks
        // like correct risk management right up until it is not; exits are
        // never gated on entries being allowed.
        let receipt = engine.emergency_unwind(None, "and now this", "test");
        assert!(receipt.kill_switch.already_armed, "it was already halted");
        assert_eq!(receipt.aborted, 1, "which changed nothing about the unwind");
        assert_eq!(receipt.stranded.len(), 1);

        engine.finish_shutdown();
    }

    #[test]
    fn a_selection_gives_up_on_what_was_named_and_leaves_the_rest_alone() {
        use ExecutionState::*;
        let temp = TempDb::new("unwind-selection");
        let db = temp.open();

        for intent in ["chosen", "untouched"] {
            db.record_execution_logs(&history(
                intent,
                &[
                    (IntentCreated, None),
                    (Validated, None),
                    (Sent, None),
                    (Confirmed, None),
                ],
            ))
            .expect("writes");
        }

        let engine = Engine::start_with(db, brisk());
        let receipt = engine.emergency_unwind(
            Some(&["chosen".to_string(), "never-existed".to_string()]),
            "the reconciled one only",
            "test",
        );

        assert_eq!(receipt.aborted, 1, "a selection narrows, it does not widen");
        assert_eq!(receipt.stranded.len(), 1);
        assert_eq!(receipt.stranded[0].intent_id, "chosen");
        assert_eq!(
            receipt.problems.len(),
            1,
            "an id naming nothing open is said out loud: {:?}",
            receipt.problems
        );
        assert!(receipt.problems[0].contains("never-existed"));

        // The one nobody named is still exactly where it was, still managed.
        let left = engine.database().open_obligations().expect("reads");
        let untouched = left
            .iter()
            .find(|o| o.intent_id == "untouched")
            .expect("still open");
        assert_eq!(untouched.state, Confirmed, "nothing aborted it");

        engine.finish_shutdown();
    }

    #[test]
    fn a_row_that_disagrees_with_its_own_history_is_reported_not_dropped() {
        use ExecutionState::*;
        let temp = TempDb::new("unwind-inconsistent");
        let db = temp.open();

        // Aborted before anything was sent, but flagged as though something had
        // been. Only a writer ignoring `AbortOutcome` produces this, and the
        // operator must not have it silently disappear off the receipt.
        let mut rows = history(
            "liar",
            &[(IntentCreated, None), (Validated, None), (Aborted, None)],
        );
        rows[2].needs_unwind = true;
        db.record_execution_logs(&rows).expect("writes");

        let engine = Engine::start_with(db, brisk());
        let receipt = engine.emergency_unwind(None, "check the books", "test");

        assert!(
            receipt.stranded.is_empty(),
            "the history says nothing went out"
        );
        assert_eq!(receipt.problems.len(), 1, "{:?}", receipt.problems);
        assert!(
            receipt.problems[0].contains("liar"),
            "and it names the intent: {}",
            receipt.problems[0]
        );

        engine.finish_shutdown();
    }

    #[test]
    fn pressing_it_twice_abandons_nothing_new_and_reports_the_same_obligation() {
        use ExecutionState::*;
        let temp = TempDb::new("unwind-twice");
        let db = temp.open();
        db.record_execution_logs(&history(
            "confirmed",
            &[
                (IntentCreated, None),
                (Validated, None),
                (Sent, Some("SigTwice")),
                (Confirmed, None),
            ],
        ))
        .expect("writes");

        let engine = Engine::start_with(db, brisk());
        let first = engine.emergency_unwind(None, "once", "test");
        let second = engine.emergency_unwind(None, "twice", "test");

        assert_eq!(first.aborted, 1);
        assert_eq!(
            second.stranded[0].signature.as_deref(),
            Some("SigTwice"),
            "the second receipt is as useful as the first for actually finding it"
        );
        assert_eq!(
            second.aborted, 0,
            "there is nothing active left to give up on"
        );
        assert_eq!(
            second.stranded, first.stranded,
            "and the position is still out there, because nothing here closed it"
        );

        // `execution_logs` is append-only and an obligation is history: the
        // second press must not have edited or re-written the first one's row.
        let rows = engine.database().health().expect("health").execution_logs;
        assert_eq!(
            rows, 5,
            "four steps and one abort, with nothing added since"
        );

        engine.finish_shutdown();
    }

    // -- flattening ---------------------------------------------------------

    /// An engine with a mock signer already installed, and the signer back so a
    /// test can tell it how to fail.
    fn with_signer(db: Database) -> (Engine, Arc<MockSolanaSigner>) {
        let signer = Arc::new(MockSolanaSigner::new());
        let engine = Engine::start_with(db, brisk());
        assert!(engine.install_execution_engine(Arc::clone(&signer) as Arc<dyn ExecutionEngine>));
        (engine, signer)
    }

    /// One intent that reached `confirmed`: a position.
    fn a_position(db: &Database, intent: &str) {
        use ExecutionState::*;
        db.record_execution_logs(&history(
            intent,
            &[
                (IntentCreated, None),
                (Validated, None),
                (Sent, Some(&format!("Sig{intent}"))),
                (Confirmed, None),
            ],
        ))
        .expect("writes");
    }

    /// One intent left at `sent`: conditional, and not actionable.
    fn a_maybe(db: &Database, intent: &str) {
        use ExecutionState::*;
        db.record_execution_logs(&history(
            intent,
            &[
                (IntentCreated, None),
                (Validated, None),
                (Sent, Some(&format!("Sig{intent}"))),
            ],
        ))
        .expect("writes");
    }

    #[test]
    fn an_unwind_with_a_signer_flattens_the_position_and_books_what_it_came_to() {
        let temp = TempDb::new("flatten-clean");
        let db = temp.open();
        a_position(&db, "alpha");
        a_position(&db, "beta");

        let (engine, signer) = with_signer(db);
        let receipt = engine.emergency_unwind(None, "get out of everything", "test");

        assert!(engine.is_halted(), "the halt still comes first");
        assert_eq!(receipt.aborted, 2);
        assert_eq!(
            receipt.exits_sent, 2,
            "one signed exit dispatched per position"
        );
        assert_eq!(receipt.exits_confirmed, 2);
        assert_eq!(receipt.exits_failed, 0);
        assert_eq!(receipt.exits_already_out, 0);
        assert_eq!(receipt.signer.as_deref(), Some("mock-solana-signer"));
        assert!(!receipt.signer_live, "and it never claimed otherwise");
        assert!(
            receipt.stranded.is_empty(),
            "a confirmed exit is the only thing that empties this list: {:?}",
            receipt.stranded
        );
        assert!(receipt.problems.is_empty(), "{:?}", receipt.problems);

        assert_eq!(receipt.flattened.len(), 2);
        let alpha = receipt
            .flattened
            .iter()
            .find(|f| f.intent_id == "alpha")
            .expect("flattened");
        assert_eq!(alpha.mint, "Mintalpha");
        assert_eq!(alpha.venue, Some(Venue::PumpFunCurve));
        assert_ne!(
            alpha.exit_intent_id, alpha.intent_id,
            "an exit is a new intent"
        );
        assert!(alpha.signature.is_some());
        assert_eq!(alpha.cost_basis_lamports, 250_000_000);
        assert!(alpha.out_lamports > 0);
        assert_eq!(
            alpha.realized_pnl_lamports,
            alpha.out_lamports - alpha.cost_basis_lamports
        );
        // An immediate round trip costs the two fees and *nothing else*.
        //
        // It used to say "two fees and its own impact" and only assert the loss
        // was negative, which was false assurance twice over. The impact term is
        // exactly zero: the curve is constant-product, so a buy walks up it and
        // the sell walks straight back down the same path, and the price the
        // position is marked out at is the price it was marked in at.
        // `replay.rs` §12.5 says so in prose and `a_round_trip_costs_two_fees_
        // wherever_it_is_taken` pins it at 199 bps at every point on the curve.
        // Asserting only `< 0` would pass on a build that had grown a real
        // impact cost, which is the one regression this assertion is here for.
        //
        // 250_000_000 lamports of basis at 199 bps is 4_975_000, exactly.
        assert_eq!(
            alpha.realized_pnl_lamports, -4_975_000,
            "a round trip should cost two 1% fees and no impact at all; \
             out {} against a basis of {}",
            alpha.out_lamports, alpha.cost_basis_lamports
        );
        let loss_bps = (-i128::from(alpha.realized_pnl_lamports)) * 10_000
            / i128::from(alpha.cost_basis_lamports);
        assert_eq!(loss_bps, 199, "which is two fees and no more");
        assert_eq!(
            receipt.realized_pnl_lamports,
            receipt
                .flattened
                .iter()
                .map(|f| f.realized_pnl_lamports)
                .sum::<i64>()
        );

        assert_eq!(signer.counters(), (2, 2, 2, 0));
        engine.finish_shutdown();
    }

    #[test]
    fn the_exit_is_recorded_as_its_own_intent_and_never_as_an_edit() {
        let temp = TempDb::new("flatten-ledger");
        let db = temp.open();
        a_position(&db, "alpha");

        let (engine, _signer) = with_signer(db);
        let receipt = engine.emergency_unwind(None, "flatten it", "test");
        let flattened = receipt.flattened.first().expect("one position").clone();

        let db = engine.database();
        let attempts = db.latest_exit_attempts().expect("reads");
        assert_eq!(attempts.len(), 1);
        let attempt = &attempts[0];
        assert_eq!(attempt.intent_id, flattened.exit_intent_id);
        assert_eq!(attempt.origin_intent_id, "alpha");
        assert_eq!(attempt.state, ExitState::ExitConfirmed);
        assert_eq!(attempt.signature, flattened.signature);
        assert_eq!(attempt.out_lamports, Some(flattened.out_lamports));
        assert_eq!(
            attempt.realized_pnl_lamports,
            Some(flattened.realized_pnl_lamports),
            "the number on the receipt is the number in the file"
        );

        // Four steps in the exit ledger, and five in the execution history: the
        // exit walked the ordinary state machine as a sell.
        let conn = rusqlite::Connection::open(temp.path()).expect("reopens");
        let steps: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM intent_transitions WHERE intent_id = ?1",
                [&flattened.exit_intent_id],
                |row| row.get(0),
            )
            .expect("counts");
        assert_eq!(steps, 4, "constructed, signed, broadcast, confirmed");

        let states: Vec<String> = conn
            .prepare("SELECT state FROM execution_logs WHERE intent_id = ?1 ORDER BY seq")
            .expect("prepares")
            .query_map([&flattened.exit_intent_id], |row| row.get(0))
            .expect("queries")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(
            states,
            vec![
                "intent_created",
                "validated",
                "sent",
                "confirmed",
                "completed"
            ],
            "an exit is a new intent walking the same machine, per U2"
        );

        // And the obligation's own rows were not touched: it is still aborted
        // with its unwind flag, because the row is history.
        let origin: Vec<(String, i64)> = conn
            .prepare("SELECT state, needs_unwind FROM execution_logs WHERE intent_id = 'alpha' ORDER BY seq")
            .expect("prepares")
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("queries")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(
            origin.len(),
            5,
            "four steps and the abort, with nothing edited"
        );
        assert_eq!(origin[4], ("aborted".to_string(), 1));

        let pnl = db
            .realized_pnl(crate::db::ExecutionMode::Live)
            .expect("reads");
        assert_eq!(pnl.closed, 1);
        assert_eq!(pnl.realized_lamports, flattened.realized_pnl_lamports);

        engine.finish_shutdown();
    }

    #[test]
    fn a_graduated_token_is_flattened_through_raydium_rather_than_the_curve() {
        let temp = TempDb::new("flatten-raydium");
        let db = temp.open();
        a_position(&db, "graduated");

        let signer = Arc::new(MockSolanaSigner::new());
        // The curve completed and the token moved to a pool, which is the whole
        // reason there are two venues: an exit built against a graduated curve
        // is an exit against a dead pool.
        let pool = RaydiumPool {
            base_reserve: 200_000_000_000_000,
            quote_reserve: 90 * crate::replay::LAMPORTS_PER_SOL,
        };
        let route = signer.raydium_route("Mintgraduated", 400_000_000_000, pool);
        assert_eq!(route.max_slippage_bps, EMERGENCY_MAX_SLIPPAGE_BPS);
        signer.hold(
            "graduated",
            MockPosition {
                route,
                landed: true,
            },
        );

        let engine = Engine::start_with(db, brisk());
        engine.install_execution_engine(Arc::clone(&signer) as Arc<dyn ExecutionEngine>);
        let receipt = engine.emergency_unwind(None, "it graduated", "test");

        assert_eq!(receipt.exits_sent, 1);
        assert_eq!(receipt.exits_confirmed, 1);
        assert!(receipt.stranded.is_empty(), "{:?}", receipt.stranded);
        let flattened = receipt.flattened.first().expect("one position");
        assert_eq!(flattened.venue, Some(Venue::RaydiumAmmV4));
        assert_eq!(flattened.tokens, Some(400_000_000_000));
        assert!(flattened.out_lamports > 0);

        let attempt = engine
            .database()
            .latest_exit_attempts()
            .expect("reads")
            .remove(0);
        assert_eq!(
            attempt.venue,
            Some(Venue::RaydiumAmmV4),
            "and the file says so too"
        );

        engine.finish_shutdown();
    }

    #[test]
    fn a_partial_failure_flattens_the_rest_and_strands_only_what_failed() {
        let temp = TempDb::new("flatten-partial");
        let db = temp.open();
        for intent in ["sellable", "no-route", "unsigned", "undelivered"] {
            a_position(&db, intent);
        }

        let signer = Arc::new(MockSolanaSigner::new());
        signer.inject("no-route", MockFault::NoRoute);
        signer.inject("unsigned", MockFault::Signing);
        signer.inject("undelivered", MockFault::Broadcast);

        let engine = Engine::start_with(db, brisk());
        assert!(engine.install_execution_engine(Arc::clone(&signer) as Arc<dyn ExecutionEngine>));
        let receipt = engine.emergency_unwind(None, "one bad pool", "test");

        assert_eq!(
            receipt.aborted, 4,
            "every one of them is abandoned regardless"
        );
        assert_eq!(
            receipt.exits_sent, 1,
            "only the one that reached the network"
        );
        assert_eq!(receipt.exits_confirmed, 1);
        assert_eq!(receipt.exits_failed, 3);
        assert_eq!(receipt.flattened.len(), 1);
        assert_eq!(receipt.flattened[0].intent_id, "sellable");

        assert_eq!(
            receipt.stranded.len(),
            3,
            "the three that did not sell, and only those"
        );
        for position in &receipt.stranded {
            let exit = position.exit.as_ref().expect("something was tried");
            assert_eq!(exit.state, Some(ExitState::ExitFailed));
            assert!(
                !exit.on_network,
                "none of these three reached the network, so none is ambiguous"
            );
            assert!(!exit.detail.is_empty());
        }
        let failure_of = |intent: &str| {
            receipt
                .stranded
                .iter()
                .find(|p| p.intent_id == intent)
                .and_then(|p| p.exit.as_ref())
                .and_then(|e| e.failure)
        };
        assert_eq!(failure_of("no-route"), Some(ExitFailure::NoRoute));
        assert_eq!(failure_of("unsigned"), Some(ExitFailure::Signing));
        assert_eq!(failure_of("undelivered"), Some(ExitFailure::Broadcast));

        engine.finish_shutdown();
    }

    #[test]
    fn an_exit_that_never_confirms_stays_stranded_and_says_it_is_on_the_network() {
        let temp = TempDb::new("flatten-unconfirmed");
        let db = temp.open();
        a_position(&db, "flying");

        let signer = Arc::new(MockSolanaSigner::new());
        signer.inject("flying", MockFault::NotConfirmed);
        let engine = Engine::start_with(db, brisk());
        engine.install_execution_engine(Arc::clone(&signer) as Arc<dyn ExecutionEngine>);

        let receipt = engine.emergency_unwind(None, "sell it", "test");
        assert_eq!(receipt.exits_sent, 1, "it did reach the network");
        assert_eq!(receipt.exits_confirmed, 0, "and it closed nothing");
        assert_eq!(receipt.exits_failed, 1);
        assert!(receipt.flattened.is_empty());
        assert_eq!(receipt.stranded.len(), 1, "still at risk until it confirms");

        let exit = receipt.stranded[0]
            .exit
            .as_ref()
            .expect("an exit was tried");
        assert_eq!(exit.failure, Some(ExitFailure::NotConfirmed));
        assert!(
            exit.on_network,
            "a broadcast that never confirmed may still have sold it; selling again by \
             hand before reconciling is the incident this flag exists to prevent"
        );

        // The exit intent is itself an open obligation now, and a second unwind
        // must not treat it as a position to sell.
        let second = engine.emergency_unwind(None, "again", "test");
        assert_eq!(second.exits_sent, 0, "nothing new went out");
        assert!(
            second.stranded.iter().any(|p| p.intent_id == "flying"),
            "the original position is still out there"
        );
        engine.finish_shutdown();
    }

    #[test]
    fn an_unwind_with_nothing_on_chain_sends_nothing_even_with_a_signer() {
        let temp = TempDb::new("flatten-empty");
        let (engine, signer) = with_signer(temp.open());

        let receipt = engine.emergency_unwind(None, "belt and braces", "test");
        assert!(engine.is_halted());
        assert_eq!(receipt.aborted, 0);
        assert_eq!(receipt.exits_sent, 0);
        assert_eq!(receipt.exits_confirmed, 0);
        assert_eq!(receipt.realized_pnl_lamports, 0);
        assert!(receipt.stranded.is_empty());
        assert!(receipt.flattened.is_empty());
        assert!(receipt.resolved.is_empty());
        assert!(
            receipt.stranded_known,
            "an empty list that was read is not an unknown one"
        );
        assert!(receipt.problems.is_empty(), "{:?}", receipt.problems);
        assert_eq!(
            signer.counters(),
            (0, 0, 0, 0),
            "the signer was never asked for anything"
        );

        // A plan that never left the ground is not a position either.
        engine.finish_shutdown();
    }

    #[test]
    fn pressing_it_twice_does_not_sell_the_position_twice() {
        let temp = TempDb::new("flatten-twice");
        let db = temp.open();
        a_position(&db, "alpha");

        let (engine, signer) = with_signer(db);
        let first = engine.emergency_unwind(None, "once", "test");
        let second = engine.emergency_unwind(None, "twice", "test");

        assert_eq!(first.exits_sent, 1);
        assert_eq!(
            second.exits_sent, 0,
            "the second press put nothing on the network, and saying it did would be \
             a claim about money"
        );
        assert_eq!(
            second.exits_already_out, 1,
            "it found the first press's exit"
        );
        assert_eq!(second.exits_confirmed, 1, "which is still closed");
        assert!(second.stranded.is_empty());
        assert_eq!(
            second.aborted, 0,
            "and there was nothing left to give up on"
        );
        assert_eq!(
            second.flattened.first().map(|f| f.exit_intent_id.clone()),
            first.flattened.first().map(|f| f.exit_intent_id.clone()),
            "the same exit, not a second one"
        );
        assert_eq!(
            signer.counters(),
            (1, 1, 1, 0),
            "one signature, one broadcast, one confirmation, however many times it is pressed"
        );

        engine.finish_shutdown();
    }

    #[test]
    fn a_conditional_obligation_that_never_landed_is_resolved_rather_than_sold() {
        let temp = TempDb::new("flatten-never-landed");
        let db = temp.open();
        a_maybe(&db, "ghost");

        let signer = Arc::new(MockSolanaSigner::new());
        signer.inject("ghost", MockFault::NeverLanded);
        let engine = Engine::start_with(db, brisk());
        engine.install_execution_engine(Arc::clone(&signer) as Arc<dyn ExecutionEngine>);

        let receipt = engine.emergency_unwind(None, "what do we actually own", "test");
        assert_eq!(receipt.exits_sent, 0, "there was nothing to sell");
        assert_eq!(signer.counters().0, 0, "so nothing was even signed");
        assert!(
            receipt.stranded.is_empty(),
            "and nobody is sent to flatten a ghost"
        );
        assert_eq!(receipt.resolved.len(), 1);
        assert_eq!(receipt.resolved[0].intent_id, "ghost");
        assert!(receipt.resolved[0].detail.contains("Sigghost"));

        engine.finish_shutdown();
    }

    #[test]
    fn a_conditional_obligation_that_is_not_yet_known_is_left_alone_and_said_so() {
        let temp = TempDb::new("flatten-unresolved");
        let db = temp.open();
        a_maybe(&db, "pending");

        let signer = Arc::new(MockSolanaSigner::new());
        signer.inject("pending", MockFault::Unresolved);
        let engine = Engine::start_with(db, brisk());
        engine.install_execution_engine(Arc::clone(&signer) as Arc<dyn ExecutionEngine>);

        let receipt = engine.emergency_unwind(None, "what is out there", "test");
        assert_eq!(receipt.exits_sent, 0);
        assert_eq!(receipt.exits_failed, 0, "not knowing yet is not a failure");
        assert!(receipt.resolved.is_empty(), "and it is not nothing either");
        assert_eq!(receipt.stranded.len(), 1);
        assert!(
            receipt.stranded[0].conditional,
            "it is still a maybe and has to be reconciled before anything is sold"
        );
        let exit = receipt.stranded[0].exit.as_ref().expect("an answer");
        assert_eq!(exit.state, None, "nothing was ever built for it");
        assert!(!exit.on_network);

        engine.finish_shutdown();
    }

    #[test]
    fn a_confirmed_position_the_chain_disagrees_about_is_reported_rather_than_dropped() {
        let temp = TempDb::new("flatten-contradiction");
        let db = temp.open();
        a_position(&db, "phantom");

        let signer = Arc::new(MockSolanaSigner::new());
        signer.inject("phantom", MockFault::NeverLanded);
        let engine = Engine::start_with(db, brisk());
        engine.install_execution_engine(Arc::clone(&signer) as Arc<dyn ExecutionEngine>);

        let receipt = engine.emergency_unwind(None, "check the books", "test");
        assert!(
            receipt.resolved.is_empty(),
            "a confirmed entry is not a ghost"
        );
        assert_eq!(receipt.stranded.len(), 1, "so it stays on the list");
        assert_eq!(receipt.problems.len(), 1, "{:?}", receipt.problems);
        assert!(receipt.problems[0].contains("phantom"));

        engine.finish_shutdown();
    }

    #[test]
    fn an_unwind_flattens_while_the_engine_is_already_halted() {
        let temp = TempDb::new("flatten-halted");
        let db = temp.open();
        a_position(&db, "alpha");

        let (engine, _signer) = with_signer(db);
        engine.trigger_kill_switch("something else tripped first", "test");

        // RISK_AND_SYBIL_SPEC.md U3, with a signer attached. An engine that
        // refuses to sell the positions that tripped its own breaker looks like
        // correct risk management right up until it is not, and this is the
        // path where that bug would actually cost money rather than only a row.
        let receipt = engine.emergency_unwind(None, "and now this", "test");
        assert!(receipt.kill_switch.already_armed);
        assert_eq!(
            receipt.exits_sent, 1,
            "exits are never gated on entries being allowed"
        );
        assert_eq!(receipt.exits_confirmed, 1);
        assert!(receipt.stranded.is_empty());

        engine.finish_shutdown();
    }

    #[test]
    fn a_selection_only_flattens_what_was_named() {
        let temp = TempDb::new("flatten-selection");
        let db = temp.open();
        a_position(&db, "chosen");
        a_position(&db, "untouched");

        let (engine, signer) = with_signer(db);
        let receipt =
            engine.emergency_unwind(Some(&["chosen".to_string()]), "the reconciled one", "test");

        assert_eq!(receipt.exits_sent, 1);
        assert_eq!(receipt.flattened.len(), 1);
        assert_eq!(receipt.flattened[0].intent_id, "chosen");
        assert_eq!(signer.counters(), (1, 1, 1, 0));

        let left = engine.database().open_obligations().expect("reads");
        let untouched = left
            .iter()
            .find(|o| o.intent_id == "untouched")
            .expect("still open");
        assert_eq!(
            untouched.state,
            ExecutionState::Confirmed,
            "nothing aborted it"
        );

        engine.finish_shutdown();
    }

    #[test]
    fn an_engine_with_no_signer_sends_nothing_and_says_so() {
        let temp = TempDb::new("flatten-no-signer");
        let db = temp.open();
        a_position(&db, "alpha");

        let engine = Engine::start_with(db, brisk());
        assert!(
            engine.execution_engine().is_none(),
            "nothing is installed by default"
        );

        let receipt = engine.emergency_unwind(None, "no send path here", "test");
        assert_eq!(receipt.exits_sent, 0);
        assert_eq!(
            receipt.signer, None,
            "which is how the UI knows nothing was sold"
        );
        assert!(!receipt.signer_live);
        assert_eq!(receipt.stranded.len(), 1);
        assert_eq!(receipt.stranded[0].exit, None, "nothing was even attempted");
        assert!(receipt.flattened.is_empty());
        assert!(receipt.problems.is_empty(), "{:?}", receipt.problems);

        engine.finish_shutdown();
    }

    #[test]
    fn a_signer_is_installed_once_and_never_swapped_under_a_running_exit() {
        let temp = TempDb::new("flatten-install-once");
        let engine = Engine::start_with(temp.open(), brisk());

        let first = Arc::new(MockSolanaSigner::new());
        assert!(engine.install_execution_engine(Arc::clone(&first) as Arc<dyn ExecutionEngine>));
        assert!(
            !engine.install_execution_engine(Arc::new(MockSolanaSigner::new())),
            "a second signer is refused rather than swapped in"
        );
        assert_eq!(
            engine.execution_engine().map(|e| e.name()),
            Some("mock-solana-signer")
        );

        engine.finish_shutdown();
    }

    #[test]
    fn two_unwinds_at_once_do_not_put_two_exits_on_the_network_for_one_position() {
        let temp = TempDb::new("flatten-concurrent");
        let db = temp.open();
        for n in 0..6 {
            a_position(&db, &format!("pos-{n}"));
        }

        let signer = Arc::new(MockSolanaSigner::new());
        let engine = Arc::new(Engine::start_with(db, brisk()));
        engine.install_execution_engine(Arc::clone(&signer) as Arc<dyn ExecutionEngine>);

        let mut handles = Vec::new();
        for n in 0..4 {
            let engine = Arc::clone(&engine);
            handles.push(std::thread::spawn(move || {
                engine.emergency_unwind(None, &format!("thread {n}"), "test")
            }));
        }
        let receipts: Vec<UnwindReceipt> = handles
            .into_iter()
            .map(|h| h.join().expect("no panic"))
            .collect();

        let sent: usize = receipts.iter().map(|r| r.exits_sent).sum();
        assert_eq!(
            sent, 6,
            "six positions, six exits, however many threads pressed the button"
        );
        assert_eq!(
            signer.counters(),
            (6, 6, 6, 0),
            "and the signer was asked exactly once per position"
        );
        for receipt in &receipts {
            assert!(receipt.problems.is_empty(), "{:?}", receipt.problems);
        }

        let health = engine.database().health().expect("health");
        assert_eq!(health.exits_in_flight, 0, "every exit confirmed");
        assert_eq!(health.realized_pnl.live.closed, 6);

        engine.finish_shutdown();
    }
}
