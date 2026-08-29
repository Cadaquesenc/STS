//! The engine with no window: `sts daemon`, and the scenario pipeline it runs.
//!
//! `lib.rs::run` builds the same engine this does and then hands it to Tauri.
//! Everything after that point — the commands, the fan-out to a `Channel`, the
//! close button — needs a window to exist. This module is the other half of the
//! same process: it builds the engine, wires a fixture corpus through it, and
//! waits for a signal instead of for a click.
//!
//! Six ideas hold it together.
//!
//! **The pipeline is the point, not the process.** [`Scenario`] is synchronous,
//! owns no runtime, opens no socket and takes no signal. It reads a fixture
//! directory, plays it, decides about every launch in it and reports what it
//! did. [`Daemon`] is the thing that gives it a database, a telemetry sink, a
//! metrics port and a way to be interrupted. They are separate because the
//! integration suite has to be able to run the first without the second: a test
//! that has to spawn a process, send it a signal and scrape its stdout is a test
//! that fails for reasons that are not the engine's.
//!
//! **The report has a deterministic half and says which half that is.**
//! Property R1 of the replay specification is that one fixture and one policy
//! produce byte-identical output. [`PipelineReport`] is that half: every number
//! in it comes off the recording and the parameters, there is no wall clock in
//! it, no host name, no elapsed time and no iteration over a hash map. The
//! process half — how long it ran for, what the latency histograms saw, which
//! signal stopped it — is [`ProcessReport`], and it is separate rather than
//! merged so that "these two runs agree" is a comparison somebody can actually
//! make.
//!
//! **Nothing is decided on evidence that arrived after the decision.** A launch
//! is evaluated when the replay clock passes its opening window and never
//! before, so the dump that follows a bundle is not in the record the gate
//! reads. A launch whose window had not closed when the recording ran out is
//! still reported — the funnel would lie by omission otherwise — and is never
//! executed on, because a verdict over a window that was cut short is a verdict
//! about a different launch.
//!
//! **The curve mirror replays the recording and does not join it.** Our own
//! simulated entry is priced against the curve as the recording left it, and is
//! then *not* applied to it. Injecting our size would silently re-fill every
//! participant that came after us at a price they never got, which is a much
//! larger modelling claim than this harness is in a position to make. What that
//! costs is stated rather than hidden: the fill this module reports includes our
//! own price impact on the quote and excludes it from everybody else's.
//!
//! **The execution is simulated and cannot become otherwise by accident.** The
//! backend is [`MockSolanaSigner`], whose `is_live` is false, and every row this
//! module writes is `ExecutionMode::Replay`. The exit path is the real one —
//! `build_exit` through `Flattener`, which produces signed bytes with a Jito tip
//! as the last instruction — so what is being exercised is the code that would
//! run, against a signer that cannot reach a network.
//!
//! **A signal stops the feed and sells nothing.** SIGINT and SIGTERM put the
//! run into teardown: the feed stops between records, no further launch is
//! entered, the exporter and the sockets close, and the database is
//! checkpointed. Positions that are open at that moment are reported as open.
//! Flattening is a trade, and a process that traded because somebody pressed
//! Ctrl-C would be making the decision that the person pressing it had not.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Serialize;

use crate::backtest::{decode_event, LaunchEvent, Side as EventSide};
use crate::db::{data_dir, Database, ExecutionLogRow, ExecutionMode, Side};
use crate::engine::{Engine, MaintenanceSchedule};
use crate::execution::{
    ExecutionEngine, ExitTarget, FlattenOutcome, Flattener, MockPosition, MockSolanaSigner, Waiter,
};
use crate::forensics::{Decision, StateLogger, StateRecord};
use crate::geyser::{GeyserConfig, GeyserFeed};
use crate::ingestion::{IngestionConfig, IngestionManager, WebSocketDialer};
use crate::metrics::{
    parse_addr, BoundExporter, MetricsCollector, MetricsExporter, MetricsSnapshot,
};
use crate::replay::{
    parse_stream, CurveState, RecordOutcome, ReplayCursor, ReplayDriver, ReplaySpeed,
    BPS_DENOMINATOR, DEFAULT_FEE_BPS,
};
use crate::strategy::fixed::Q18;
use crate::strategy::syndicate::GateVerdict;
use crate::strategy::{
    evaluate, ClusterParams, EntryQuote, FundingEdge, GateParams, GateReason, LaunchRecord,
    OpeningBuyer, RiskTag, SandwichCheck, SandwichGuard,
};
use crate::telemetry::{TelemetryEvent, TelemetryHub, TelemetryLevel, TelemetrySink};
use crate::types::{
    CircuitBreaker, ExecutionState, FastPathGate, LiquidityThresholds, OperatingMode, RiskSnapshot,
    Venue,
};

/// The schema string on the report this module emits.
pub const REPORT_SCHEMA: &str = "sts.daemon.report.v1";

/// The risk frame a fixture replay's forensic rows are recorded against.
///
/// A replay has no account behind it. Nothing is at risk, there is no equity to
/// draw down from, and there is no operator cap on how many positions may be
/// open at once — so the balances here are zero and the caps are wide, and both
/// say what they are rather than being plausible-looking numbers somebody would
/// later read as real ones. A forensic row from a replay is worth reading for
/// its verdict, its evidence, its mode and its `open_positions`; the equity
/// column on one is a zero meaning "no account", which is the truth.
///
/// `Scenario::record_forensics` overwrites `at_ms` with the replay clock and
/// `open_positions` with what the run is actually holding, then recomputes the
/// drawdown from the balances — so those three are the run's own numbers rather
/// than this constant's.
const REPLAY_RISK: RiskSnapshot = RiskSnapshot {
    at_ms: 0,
    mode: OperatingMode::Replay,
    equity_lamports: 0,
    high_water_lamports: 0,
    drawdown_bps: 0,
    max_drawdown_bps: crate::types::BPS_DENOMINATOR as u16,
    open_positions: 0,
    max_open_positions: u16::MAX,
    circuit_breaker: CircuitBreaker::Clear,
    fast_path: FastPathGate::CLOSED,
    liquidity: LiquidityThresholds {
        min_pool_lamports: 0,
        exit_only_below_lamports: 0,
        max_pool_share_bps: crate::replay::DEFAULT_MAX_POOL_SHARE_BPS,
    },
};

/// What one accepted launch buys, unless the operator says otherwise.
///
/// A quarter of a SOL: small enough that the 1.5% participation cap is the
/// binding constraint on a thin curve rather than this number, and large enough
/// that the fee and the slippage are visible in the fill rather than rounding
/// to nothing.
pub const DEFAULT_ENTRY_LAMPORTS: u64 = 250_000_000;

/// How long the runtime is given to finish in-flight work on the way out.
/// The same grace `lib.rs` gives the window, for the same reason.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// How many records the feed plays between progress lines.
///
/// Counted rather than timed, so the number of lines a corpus produces is a
/// property of the corpus. A progress line every five seconds would make the
/// telemetry stream a record of how fast this machine is.
const PROGRESS_RECORDS: u64 = 1_000;

// ===========================================================================
// policy
// ===========================================================================

/// Which entry rule a run is gated on.
///
/// Two, because [`GateParams::v1`] exists and is meant to be runnable: the only
/// honest way to say what the group checks cost is to replay both over the same
/// corpus at the same prices. A daemon that could only run the current rule
/// would make that comparison a matter of editing the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum GateProfile {
    /// The shipped rule: a score, a primary signal, and the three group checks.
    Default,
    /// The rule as it stood before the group checks.
    V1,
}

impl GateProfile {
    pub const ALL: [GateProfile; 2] = [GateProfile::Default, GateProfile::V1];

    pub const fn as_str(self) -> &'static str {
        match self {
            GateProfile::Default => "default",
            GateProfile::V1 => "v1",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        GateProfile::ALL.into_iter().find(|p| p.as_str() == text)
    }

    pub fn params(self) -> GateParams {
        match self {
            GateProfile::Default => GateParams::default(),
            GateProfile::V1 => GateParams::v1(),
        }
    }
}

/// Everything one scenario run needs to know.
#[derive(Debug, Clone)]
pub struct ScenarioConfig {
    /// A corpus root or a single case directory. Both are accepted: a corpus is
    /// a directory of case directories, a case is a directory of `.jsonl`.
    pub fixtures: PathBuf,
    pub cluster: ClusterParams,
    pub gate_profile: GateProfile,
    pub fee_bps: u16,
    /// What one accepted launch buys, before the participation cap.
    pub entry_lamports: u64,
    /// The share of the curve's executable liquidity one position may be.
    pub max_pool_share_bps: u16,
    /// Whether our own entry is modelled as a private bundle rather than a
    /// public send.
    ///
    /// It changes a verdict, not a fill: [`SandwichCheck::refuses`] never
    /// refuses a private send, because §15.1 prices a front-run that reads our
    /// transaction before it lands and a send nobody can read first is outside
    /// what that models. The exposure is still computed and still reported —
    /// §15.4's use for the number is justifying the tip against the adverse
    /// selection it buys out of, and a tip larger than the exposure is a tip
    /// buying nothing.
    pub private_entry: bool,
    /// Overrides the gate profile's own sandwich setting when present.
    ///
    /// Separate from [`GateProfile`] rather than a third profile, because it is
    /// the one gate parameter that is about our order rather than about the
    /// launch: an operator comparing the shipped rule against `v1` wants the
    /// same guard on both sides of that comparison, and an operator asking what
    /// the guard costs wants it varied with everything else held still.
    pub sandwich_guard: Option<SandwichGuard>,
    /// False makes the run detect and report and open nothing. The funnel is
    /// identical either way, which is the point of having the switch.
    pub execute: bool,
    /// Whether an open position is flattened when the recording runs out.
    pub flatten_at_end: bool,
    pub speed: ReplaySpeed,
}

impl Default for ScenarioConfig {
    fn default() -> Self {
        ScenarioConfig {
            fixtures: data_dir().join("fixtures"),
            cluster: ClusterParams::default(),
            gate_profile: GateProfile::Default,
            fee_bps: DEFAULT_FEE_BPS,
            entry_lamports: DEFAULT_ENTRY_LAMPORTS,
            max_pool_share_bps: crate::replay::DEFAULT_MAX_POOL_SHARE_BPS,
            private_entry: false,
            sandwich_guard: None,
            execute: true,
            flatten_at_end: true,
            speed: ReplaySpeed::Max,
        }
    }
}

impl ScenarioConfig {
    /// The parameters the gate decides on: the profile's, with the sandwich
    /// guard replaced if the operator named one.
    pub fn gate_params(&self) -> GateParams {
        let mut params = self.gate_profile.params();
        if let Some(guard) = self.sandwich_guard {
            params.sandwich_guard = guard;
        }
        params
    }

    /// The gross size one entry would actually send, in lamports.
    ///
    /// §10's 1.5% participation cap is a ceiling on what the curve can absorb,
    /// so a size above it is not a bigger position, it is a worse fill. The
    /// operator's `--entry-lamports` is therefore a request and this is the
    /// answer to it.
    ///
    /// The cap is read off `real_sol_reserves` — the SOL actually in the pool,
    /// which is what a position has to come back out through — while the
    /// sandwich arithmetic below is against `virtual_sol_reserves`, which is
    /// the `y` in the constant-product price. Two different reserves for two
    /// different questions, and mixing them is the mistake this comment exists
    /// to prevent.
    ///
    /// One function rather than two so that the size the gate is shown and the
    /// size [`SimulatedExecution::enter`] fills at cannot drift apart. A guard
    /// that refused an order the executor would then have shrunk would be
    /// refusing a trade nobody was going to make.
    pub fn entry_size(&self, curve: &CurveState) -> u64 {
        let room = curve.max_position_lamports(self.max_pool_share_bps);
        self.entry_lamports.min(room)
    }

    /// What the gate is shown about our own order, or `None` when there is no
    /// order to show it.
    ///
    /// Three ways there is nothing to price, and the distinction between them
    /// is on the record rather than in the verdict. A graduated curve and an
    /// implausible one cannot be quoted at all — [`CurveState::quote_buy`]
    /// refuses both, and a quote this module built anyway would be a number the
    /// executor could not reproduce. A curve the cap leaves no room in can be
    /// read perfectly well and simply has no position in it.
    ///
    /// All three answer `None`, which under [`SandwichGuard::Required`] is a
    /// refusal. That is the setting doing what it says — an entry nobody
    /// priced is not an entry found to be safe — and the funnel can still tell
    /// the three apart, because [`LaunchOutcome`] carries the size that was
    /// quoted next to the reserves it was quoted against.
    pub fn entry_quote(&self, curve: &CurveState) -> Option<EntryQuote> {
        if curve.complete || !curve.is_plausible() {
            return None;
        }
        let gross_lamports = self.entry_size(curve);
        if gross_lamports == 0 {
            return None;
        }
        Some(EntryQuote {
            gross_lamports,
            virtual_sol_reserves: curve.virtual_sol_reserves,
            fee_bps: self.fee_bps,
            private_bundle: self.private_entry,
        })
    }
}

// ===========================================================================
// the report
// ===========================================================================

/// How many launches reached each answer.
///
/// One counter per [`GateReason`], in `GateReason::ALL` order, so a funnel over
/// a corpus prints the same table shape whatever the corpus contained — and a
/// reason nobody hit is a zero rather than a missing row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Funnel {
    /// Launch events the feed read.
    ///
    /// Larger than `decided` exactly when a recording opened one mint twice:
    /// the second line is ignored, because the opening window is measured from
    /// the first and letting a later line move it would move every offset the
    /// gate reads. The gap is the only sign of that, so the two are counted
    /// apart rather than assumed equal.
    pub seen: u32,
    /// Launches a verdict was reached on.
    pub decided: u32,
    /// Of those, the ones whose opening window closed before the recording ran
    /// out. Only these are eligible to be executed on.
    pub window_closed: u32,
    /// Of those, the ones the gate said yes to.
    ///
    /// A verdict rather than a position: a launch the rule liked and the
    /// executor could not size — a curve with no room left in it under the
    /// participation cap — is counted here and has no `execution` behind it.
    /// The two are deliberately not the same number, because collapsing them
    /// would file a plumbing failure in the funnel as a strategy result.
    /// `PipelineReport::open_positions` and the per-launch `execution` are
    /// where money actually moved.
    pub entered: u32,
    /// Reason to count, worst first.
    pub reasons: Vec<(String, u32)>,
}

impl Funnel {
    fn empty() -> Self {
        Funnel {
            reasons: GateReason::ALL
                .iter()
                .map(|reason| (reason.as_str().to_string(), 0))
                .collect(),
            ..Funnel::default()
        }
    }

    /// How many launches reached one reason.
    ///
    /// The counts are a list of pairs so the serialised funnel keeps its order,
    /// which leaves a reader with a linear scan and a string to spell right.
    /// This is that scan, written once.
    pub fn reason_count(&self, reason: GateReason) -> u32 {
        self.reasons
            .iter()
            .find(|(name, _)| name == reason.as_str())
            .map_or(0, |(_, count)| *count)
    }

    fn count(&mut self, reason: GateReason) {
        if let Some(slot) = self
            .reasons
            .iter_mut()
            .find(|(name, _)| name == reason.as_str())
        {
            slot.1 += 1;
        }
    }

    fn absorb(&mut self, other: &Funnel) {
        self.seen += other.seen;
        self.decided += other.decided;
        self.window_closed += other.window_closed;
        self.entered += other.entered;
        for (name, count) in &other.reasons {
            if let Some(slot) = self.reasons.iter_mut().find(|(mine, _)| mine == name) {
                slot.1 += count;
            }
        }
    }
}

/// What became of one simulated exit.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitOutcome {
    pub exit_intent_id: Option<String>,
    pub signature: Option<String>,
    pub venue: Option<Venue>,
    /// What the sale came back with. Zero on anything that did not confirm.
    pub out_lamports: i64,
    /// Proceeds less what the position cost. Negative is a loss, which is what
    /// flattening into a curve somebody has pulled looks like.
    pub realized_pnl_lamports: i64,
    /// `flattened`, `in-flight`, `resolved-to-nothing`, `unresolved`,
    /// `skipped`, `failed`. The vocabulary is `FlattenOutcome`'s, flattened to
    /// one word because the report is read next to the ledger that holds the
    /// detail.
    pub state: String,
    /// Whether money is still on chain for this position. The only answer that
    /// matters when a run is being read to find out what it left behind.
    pub still_at_risk: bool,
    pub detail: Option<String>,
}

/// One position this run opened.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionOutcome {
    pub intent_id: String,
    pub signature: String,
    /// What went in, after the participation cap.
    pub size_lamports: u64,
    pub tokens: u64,
    pub fee_lamports: u64,
    pub entry_slippage_bps: u16,
    /// `None` while the position is still open, which is what a run stopped by
    /// a signal leaves behind.
    pub exit: Option<ExitOutcome>,
}

/// One launch, what the gate made of it, and what followed.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchOutcome {
    pub mint: String,
    pub creator: Option<String>,
    pub opened_at_ms: i64,
    /// Where the replay clock was when the gate read this launch. Fixture time,
    /// never host time.
    pub decided_at_ms: i64,
    /// False when the recording ended before the opening window did. Such a
    /// launch is reported and is never entered.
    pub window_closed: bool,
    /// Buyers inside the opening window, which is what the gate read.
    pub buyers: u32,
    /// The newest event timestamp that reached the record the gate read.
    ///
    /// Never later than `opened_at_ms + window_ms`. That is the no-leakage
    /// property, and it is a field rather than a comment so it can be checked
    /// against the recording instead of taken on trust — a detector that
    /// started reading one record too far would show up here as a number, not
    /// as a verdict that quietly changed.
    pub evidence_to_ms: i64,
    /// Wallets behind the single most common funder among those buyers.
    ///
    /// Not the gate's own number — it is here because it is the one piece of
    /// evidence a person can check against the fixture by hand, and a detector
    /// that silently stopped seeing funders would otherwise show up only as a
    /// verdict quietly changing.
    pub largest_funder_wallets: u32,
    pub enter: bool,
    pub reason: GateReason,
    pub confidence_micros: u64,
    pub tags: Vec<RiskTag>,
    pub bundle_wallets: u32,
    pub cohort_wallets: u32,
    pub cohort_lamports: u64,
    /// The curve as the recording left it at the moment of the decision.
    pub real_sol_lamports: u64,
    /// The curve's virtual SOL reserve at that moment — the `y` the sandwich
    /// threshold scales with, and not the same number as `real_sol_lamports`.
    pub virtual_sol_lamports: u64,
    /// The gross size the gate was shown for our own order, after the
    /// participation cap, or `None` when there was nothing to price.
    ///
    /// On the record next to `real_sol_lamports` so the three ways of arriving
    /// at `None` stay distinguishable: a graduated curve, reserves that do not
    /// describe a curve, and a curve the cap leaves no room in.
    pub quoted_lamports: Option<u64>,
    /// What the curve said about that order. `None` when the guard is off or
    /// nothing was quoted.
    pub sandwich: Option<SandwichCheck>,
    /// Whether the refusal was about our own order rather than about the
    /// launch. False on everything that was accepted.
    ///
    /// Derived rather than stored on the strategy side, and duplicated onto the
    /// outcome deliberately: it is the field a funnel groups by, and a reader
    /// who has to know which `GateReason` variants are size verdicts to read
    /// the report is a reader who will eventually get it wrong.
    pub refused_on_our_order: bool,
    pub execution: Option<ExecutionOutcome>,
}

/// One fixture case, played.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaseReport {
    /// The directory name.
    pub case: String,
    pub stream_id: Option<String>,
    /// Why the fixture was not played at all. A chain that does not verify is
    /// the ordinary reason and is not a failure of this harness — a corpus
    /// carries cases built to be refused.
    pub refused: Option<String>,
    /// `Some(true)` when the manifest's declared head is the one the records
    /// compute to, `None` when there was no manifest to check against. `None`
    /// is the absence of the check and is deliberately not a pass.
    pub chain_verified: Option<bool>,
    /// The manifest's `complete` flag: false means the recording has a hole.
    pub fixture_complete: Option<bool>,
    pub records: u64,
    pub frames: u64,
    /// Frames handed to the decoder: the accepted ones and the ones a bounded
    /// channel refused, which are replayed anyway because that is what recovery
    /// means here.
    pub replayed: u64,
    /// Frames the recording says the live filters rejected. Not replayed, so
    /// the curve they would have moved stays where it was.
    pub filtered: u64,
    pub events: u64,
    /// Frames that did not decode into an event.
    pub undecodable: u64,
    /// Records whose timestamp was behind the clock, and records whose slot
    /// was. Counted rather than corrected.
    pub clamped: u64,
    pub slot_regressions: u64,
    pub launches: Vec<LaunchOutcome>,
    pub funnel: Funnel,
    /// What went wrong that did not stop the run.
    pub problems: Vec<String>,
}

impl CaseReport {
    /// A case that was never played, and why.
    fn refused(case: &str, why: String) -> Self {
        CaseReport {
            refused: Some(why),
            ..CaseReport::empty(case)
        }
    }

    /// A case with a verified playhead on it and nothing played yet.
    fn opened(case: &str, feed: &Feed) -> Self {
        CaseReport {
            stream_id: Some(feed.stream_id.clone()),
            chain_verified: feed.chain_verified,
            fixture_complete: feed.complete,
            ..CaseReport::empty(case)
        }
    }

    fn empty(case: &str) -> Self {
        CaseReport {
            case: case.to_string(),
            stream_id: None,
            refused: None,
            chain_verified: None,
            fixture_complete: None,
            records: 0,
            frames: 0,
            replayed: 0,
            filtered: 0,
            events: 0,
            undecodable: 0,
            clamped: 0,
            slot_regressions: 0,
            launches: Vec::new(),
            funnel: Funnel::empty(),
            problems: Vec::new(),
        }
    }
}

/// The half of a run that is a function of the fixture and the policy.
///
/// Two runs over one corpus at one profile serialise to the same bytes. There
/// is deliberately nothing in here that a second machine, a second day or a
/// busier CPU could change.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineReport {
    pub schema: String,
    pub gate_profile: GateProfile,
    pub fee_bps: u16,
    pub entry_lamports: u64,
    pub max_pool_share_bps: u16,
    /// The guard the run actually decided on, after any override — not the
    /// override itself.
    ///
    /// On the report because it is policy, and R1 is that one fixture and one
    /// policy produce the same bytes. Two runs that disagreed about whether the
    /// curve was consulted would otherwise produce two different funnels while
    /// claiming the same rule.
    pub sandwich_guard: SandwichGuard,
    /// Whether the entry was modelled as a private bundle. Policy for the same
    /// reason: it decides whether an exposure is a refusal or a note.
    pub private_entry: bool,
    pub window_ms: i64,
    /// False when the run was told to detect and open nothing.
    pub executed: bool,
    /// Positions this run opened that still have money on chain behind them:
    /// the ones never flattened because a signal arrived, and the ones whose
    /// exit did not confirm.
    ///
    /// First-class rather than something a reader has to derive, because it is
    /// the one number that says what the run left behind, and a report that
    /// makes it easy to overlook is a report that makes it easy to walk away
    /// from an open position.
    pub open_positions: u32,
    pub cases: Vec<CaseReport>,
    pub totals: Funnel,
}

/// Why a run stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "stop", content = "detail")]
pub enum StopReason {
    /// The corpus ran out, which is how a `--once` run ends.
    Exhausted,
    /// SIGINT or SIGTERM.
    Signalled(String),
    /// The kill switch was armed while the feed was running.
    Halted,
}

/// The half of a run that is a fact about this process on this machine.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessReport {
    pub stopped_by: StopReason,
    /// The signer that did the flattening, and whether it can reach a network.
    /// False for every backend in this build.
    pub signer: String,
    pub signer_live: bool,
    pub kill_switch_armed: bool,
    /// Telemetry lines the sink wrote out.
    pub telemetry_exported: u64,
    /// Telemetry lines the hub threw away because nothing drained it fast
    /// enough. Non-zero means the export is behind the engine.
    pub telemetry_dropped: u64,
    /// Where `/metrics` was served, when it was.
    pub metrics_addr: Option<String>,
    pub metrics: MetricsSnapshot,
    pub problems: Vec<String>,
}

/// Everything one `sts daemon run` did.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonReport {
    pub pipeline: PipelineReport,
    pub process: ProcessReport,
}

// ===========================================================================
// stage 1 and 2 — the fixtures, and the feed over them
// ===========================================================================

/// A flag the feed checks between records.
///
/// Between, never during: a record is either wholly consumed or not started, so
/// a run stopped by a signal ends on a fixture boundary and its report describes
/// a state the recording actually passed through.
#[derive(Debug, Default)]
pub struct StopFlag {
    stopped: AtomicBool,
    reason: Mutex<Option<StopReason>>,
}

impl StopFlag {
    pub fn new() -> Self {
        StopFlag::default()
    }

    /// Asks for the run to stop. The first reason wins: a SIGTERM that arrives
    /// while a SIGINT is already being honoured has not changed why the process
    /// is stopping.
    pub fn stop(&self, reason: StopReason) {
        let mut held = self.reason.lock();
        if held.is_none() {
            *held = Some(reason);
        }
        self.stopped.store(true, Ordering::SeqCst);
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    pub fn reason(&self) -> Option<StopReason> {
        self.reason.lock().clone()
    }
}

/// One fixture case, opened and verified, with a playhead on it.
struct Feed {
    driver: ReplayDriver,
    stream_id: String,
    chain_verified: Option<bool>,
    complete: Option<bool>,
    record_count: u64,
}

/// The fields of one record the pipeline needs, copied off the borrow.
///
/// `ReplayDriver::step` hands back a reference tied to the driver, and the next
/// step cannot be taken while it is alive. Rather than thread that borrow
/// through the whole of the detector, the four fields that matter are taken out
/// here and the record is let go of.
struct Fed {
    seq: u64,
    slot: u64,
    at_ms: i64,
    frame: Option<Vec<u8>>,
    outcome: RecordOutcome,
}

/// Reads a case directory into a verified playhead, or says what was wrong.
///
/// Refuses rather than repairs, for the reason `replay.rs` gives about its own
/// loader: a fixture is evidence, and a reader that sorted a mis-ordered stream
/// into order would hide the recorder bug that produced it. A corpus carries
/// cases built to be refused, so a refusal here is a result and not an error.
fn open_case(dir: &Path) -> Result<Feed, String> {
    let manifest = crate::backtest::read_manifest(dir).map_err(|err| err.to_string())?;
    let files = crate::backtest::fixture_files(dir).map_err(|err| err.to_string())?;

    // With a manifest every file is a segment of the one stream it names.
    // Without one there is nothing saying two files belong to the same chain,
    // and a chain computed across two streams does not verify — which would
    // read as a tampered fixture rather than as a directory nobody wrote a
    // manifest for.
    let stream_id = match (&manifest, files.len()) {
        (Some(manifest), _) => manifest.stream_id.clone(),
        (None, 1) => files[0]
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("stream")
            .to_string(),
        (None, count) => {
            return Err(format!(
                "{} holds {count} streams and no manifest saying they are one",
                dir.display()
            ))
        }
    };

    let mut records = Vec::new();
    for file in &files {
        let text =
            std::fs::read_to_string(file).map_err(|err| format!("{}: {err}", file.display()))?;
        records.extend(parse_stream(&text).map_err(|err| format!("{}: {err}", file.display()))?);
    }

    let cursor = ReplayCursor::open(&stream_id, records).map_err(|err| err.to_string())?;
    let record_count = cursor.len() as u64;
    let chain_verified = manifest
        .as_ref()
        .map(|manifest| manifest.chain_head == crate::replay::hex(&cursor.chain_head()));
    let complete = manifest.as_ref().map(|manifest| manifest.complete);

    Ok(Feed {
        driver: ReplayDriver::new(cursor),
        stream_id,
        chain_verified,
        complete,
        record_count,
    })
}

/// Every case directory under a path, in name order.
///
/// A directory holding `.jsonl` files directly is one case. Anything else is
/// read as a corpus root and its sub-directories are the cases. Sorted, because
/// `read_dir` hands them back in whatever order the filesystem felt like and a
/// run whose case order depends on the filesystem is a run that is not
/// reproducible on another machine.
pub fn case_directories(root: &Path) -> Result<Vec<PathBuf>, String> {
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    if crate::backtest::fixture_files(root).is_ok() {
        return Ok(vec![root.to_path_buf()]);
    }

    let entries = std::fs::read_dir(root).map_err(|err| format!("{}: {err}", root.display()))?;
    let mut cases = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("{}: {err}", root.display()))?;
        let path = entry.path();
        if path.is_dir() && crate::backtest::fixture_files(&path).is_ok() {
            cases.push(path);
        }
    }
    cases.sort();
    if cases.is_empty() {
        return Err(format!(
            "{} holds no .jsonl fixture streams and no case directory that does",
            root.display()
        ));
    }
    Ok(cases)
}

/// Paces the feed against a wall clock.
///
/// `Max` does not pace at all and is what a one-shot run uses. The other three
/// sleep the gap between two records, divided by the multiplier, so a quiet
/// stretch of the fixture costs the same wall time as a busy one and playback
/// runs at the rate the thing was recorded at. That is what makes a signal
/// worth sending: a corpus that plays in four milliseconds cannot be
/// interrupted by anything a person does.
struct Pacer {
    multiplier: Option<u64>,
    previous_at_ms: Option<i64>,
}

/// The longest the feed sleeps without looking at the stop flag.
///
/// A recording with a quiet hour in it has an hour-long gap in it, and a daemon
/// that answered Ctrl-C an hour later would be a daemon nobody waits for. The
/// wait is spent in slices so the signal is honoured at roughly the rate a
/// person expects, whatever the fixture does.
const PACE_SLICE: Duration = Duration::from_millis(50);

impl Pacer {
    fn new(speed: ReplaySpeed) -> Self {
        Pacer {
            multiplier: speed.multiplier(),
            previous_at_ms: None,
        }
    }

    fn pace(&mut self, at_ms: i64, stop: Option<&StopFlag>) {
        let previous = self.previous_at_ms.replace(at_ms);
        let Some(multiplier) = self.multiplier else {
            return;
        };
        let Some(previous) = previous else {
            return;
        };
        let gap = at_ms.saturating_sub(previous).max(0) as u64;
        let mut left = Duration::from_millis(gap / multiplier.max(1));
        while !left.is_zero() {
            if stop.is_some_and(StopFlag::is_stopped) {
                return;
            }
            let slice = left.min(PACE_SLICE);
            std::thread::sleep(slice);
            left -= slice;
        }
    }
}

// ===========================================================================
// stage 3 — detection
// ===========================================================================

/// One wallet's opening behaviour, as the feed has seen it so far.
#[derive(Debug, Clone)]
struct BuyerAccumulator {
    /// Who paid for this wallet, if the recording knows. `None` is unknown and
    /// is never treated as "nobody" — an unfunded wallet and a wallet whose
    /// funder was not recorded are different facts.
    funder: Option<String>,
    first_buy_ms: i64,
    sol_in_lamports: u64,
    sol_out_lamports: u64,
    tx_count: u32,
}

/// One launch the feed has opened and the gate has not read yet.
struct OpenLaunch {
    mint: String,
    creator: Option<String>,
    opened_at_ms: i64,
    /// The recording's own curve, moved by the recording's own events. Our
    /// simulated entry is priced against this and is never applied to it.
    curve: CurveState,
    /// Tokens the *recording's* entries bought, so its exits can be applied to
    /// the mirror. Nothing to do with any position this run opens.
    recorded_tokens: u64,
    buyers: BTreeMap<String, BuyerAccumulator>,
    /// The newest timestamp that has entered `buyers`. `None` until somebody
    /// has bought.
    evidence_to_ms: Option<i64>,
    decided: bool,
}

impl OpenLaunch {
    /// When the gate is allowed to read this launch.
    fn decide_at_ms(&self, window_ms: i64) -> i64 {
        self.opened_at_ms.saturating_add(window_ms)
    }

    /// The launch as the entry rule is allowed to see it.
    ///
    /// Only buyers inside the opening window, which is the same window the
    /// analyser would apply itself — applying it here as well means the record
    /// carries what was knowable at the decision rather than being trimmed
    /// afterwards.
    ///
    /// The funding edges are a **lower bound and are named as one**: the
    /// recording says who funded a wallet and never how much, so the edge is
    /// weighted with what that wallet then spent. A wallet cannot have bought
    /// more than it was given, so the weight is never an overstatement, and the
    /// two places the weight is read — interaction entropy and the shared-funder
    /// traversal — are both monotone in it.
    fn record(&self, window_ms: i64) -> LaunchRecord {
        let mut buyers = Vec::with_capacity(self.buyers.len());
        let mut funding = Vec::new();
        for (wallet, seen) in &self.buyers {
            let offset = seen.first_buy_ms.saturating_sub(self.opened_at_ms);
            if offset > window_ms {
                continue;
            }
            buyers.push(OpeningBuyer {
                wallet: wallet.clone(),
                sol_in_lamports: seen.sol_in_lamports,
                sol_out_lamports: seen.sol_out_lamports,
                tx_count: seen.tx_count,
                first_seen_ms: offset,
            });
            if let Some(funder) = &seen.funder {
                funding.push(FundingEdge {
                    from: funder.clone(),
                    to: wallet.clone(),
                    lamports: seen.sol_in_lamports,
                });
            }
        }
        LaunchRecord {
            mint: self.mint.clone(),
            creator: self.creator.clone(),
            buyers,
            funding,
        }
    }

    /// Wallets behind the single most common funder among the opening buyers.
    fn largest_funder_wallets(&self, window_ms: i64) -> u32 {
        let mut counts: BTreeMap<&str, u32> = BTreeMap::new();
        for seen in self.buyers.values() {
            if seen.first_buy_ms.saturating_sub(self.opened_at_ms) > window_ms {
                continue;
            }
            if let Some(funder) = &seen.funder {
                *counts.entry(funder.as_str()).or_insert(0) += 1;
            }
        }
        counts.into_values().max().unwrap_or(0)
    }
}

// ===========================================================================
// stage 4 — the simulated execution
// ===========================================================================

/// A clock that advances a number instead of a thread.
///
/// The rebroadcast loop backs off between attempts, and a harness that sat
/// through those sleeps would spend its wall time proving that
/// `std::thread::sleep` works. Walking the schedule as fixture milliseconds
/// keeps both properties that matter: the ledger records the same intervals it
/// would have recorded, and the run stays a function of the fixture rather than
/// of how long this machine happened to take.
struct FixtureClock {
    at_ms: i64,
}

impl Waiter for FixtureClock {
    fn wait(&mut self, ms: u64) -> i64 {
        self.at_ms = self
            .at_ms
            .saturating_add(i64::try_from(ms).unwrap_or(i64::MAX));
        self.at_ms
    }

    fn now_ms(&self) -> i64 {
        self.at_ms
    }
}

/// The ledger and the signer one run opens positions against.
///
/// **Nothing here can reach a network.** The backend is a `MockSolanaSigner`,
/// whose `is_live` is false, and every row written is `ExecutionMode::Replay`.
/// What is being exercised is the code an entry and an exit would run through —
/// the six-state machine in `execution_logs`, the finer exit lifecycle in
/// `intent_transitions`, `build_exit`'s tip instruction and `Flattener`'s
/// refusal to sell a position twice — against a signer that produces bytes and
/// sends them nowhere.
///
/// **The exit's economics are the mock's, not the fixture's.** The route comes
/// from `MockSolanaSigner::pump_fun_route`, which tells a coherent story about a
/// curve of its own; it is not the curve the fixture recorded. So the realized
/// PnL this module reports says the exit path priced, signed, tipped, broadcast
/// and booked something. It does not say what the fixture was worth. That is
/// `sts backtest run`, which prices the recording against the recording, and the
/// two numbers must never be quoted as though they were the same claim.
pub struct SimulatedExecution<'a> {
    db: &'a Database,
    backend: &'a MockSolanaSigner,
    metrics: Option<&'a MetricsCollector>,
}

impl<'a> SimulatedExecution<'a> {
    pub fn new(db: &'a Database, backend: &'a MockSolanaSigner) -> Self {
        SimulatedExecution {
            db,
            backend,
            metrics: None,
        }
    }

    pub fn with_metrics(mut self, metrics: &'a MetricsCollector) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Opens one position: prices it, writes its history, and tells the backend
    /// it is on chain.
    ///
    /// The intent id and the signature are functions of the stream, the mint and
    /// the record the decision was made on, so a second run over one fixture
    /// mints the same identifiers. That is what makes the ledger's unique index
    /// on `signature` a useful thing rather than a nuisance: running the same
    /// corpus into the same database twice is recording the same decision twice,
    /// and it is refused.
    fn enter(
        &self,
        stream_id: &str,
        launch: &OpenLaunch,
        seq: u64,
        at_ms: i64,
        config: &ScenarioConfig,
    ) -> Result<ExecutionOutcome, String> {
        // The same call the gate priced its quote from, which is the point of it
        // being a method on the config rather than two lines here: a guard that
        // refused an order this function would then have shrunk would be
        // refusing a trade nobody was going to make.
        let room = launch
            .curve
            .max_position_lamports(config.max_pool_share_bps);
        let size = config.entry_size(&launch.curve);
        if size == 0 {
            return Err(format!(
                "{} has room for {room} lamports at {} bps, which is not a position",
                launch.mint, config.max_pool_share_bps
            ));
        }

        let fill = launch
            .curve
            .quote_buy(size, config.fee_bps)
            .map_err(|err| {
                format!(
                    "{} could not be entered at {size} lamports: {err}",
                    launch.mint
                )
            })?;
        if fill.tokens == 0 {
            return Err(format!(
                "{} quotes to no tokens at {size} lamports",
                launch.mint
            ));
        }

        let intent_id = format!("{stream_id}-{}-{seq:06}", launch.mint);
        let signature = format!("sim-{intent_id}");
        let size_lamports = i64::try_from(size)
            .map_err(|_| format!("{size} lamports does not fit the ledger's signed column"))?;

        // The whole history in one call. `record_execution_logs` is one
        // transaction, so an entry is either wholly on the ledger or wholly
        // absent — there is no state in which the engine believes it sent
        // something it has no `sent` row for.
        let steps = [
            (0, ExecutionState::IntentCreated, None, None),
            (
                1,
                ExecutionState::Validated,
                Some(ExecutionState::IntentCreated),
                None,
            ),
            (
                2,
                ExecutionState::Sent,
                Some(ExecutionState::Validated),
                Some(signature.clone()),
            ),
            (
                3,
                ExecutionState::Confirmed,
                Some(ExecutionState::Sent),
                None,
            ),
        ];
        let rows: Vec<ExecutionLogRow> = steps
            .iter()
            .map(|(seq, state, prev, sig)| ExecutionLogRow {
                intent_id: intent_id.clone(),
                seq: *seq,
                mint: launch.mint.clone(),
                state: *state,
                prev_state: *prev,
                side: Side::Buy,
                size_lamports,
                // What one token base unit cost, in lamports at 10^-18.
                //
                // This was a null until migration 4, and the reason given was
                // that the column was a `REAL` while every other number this
                // module reports is an integer in a named unit. That reason is
                // gone: `price_q18` is the integer count of a `Q18`, the same
                // unit `journal_fills.price_q18` uses. `None` now means only
                // what it says — the fill was of no tokens, or the ratio is
                // past what the column holds — rather than standing in for a
                // number the schema could not be trusted with.
                price_q18: Q18::ratio_floor(u128::from(size), u128::from(fill.tokens))
                    .filter(|price| price.raw() > 0 && price.to_i64_raw().is_some()),
                signature: sig.clone(),
                latency_ms: None,
                needs_unwind: false,
                mode: ExecutionMode::Replay,
                abort_reason: None,
                at_ms,
            })
            .collect();
        // The count matters, not just the absence of an error. `execution_logs`
        // is append-only and its insert is `ON CONFLICT (intent_id, seq) DO
        // NOTHING`, so writing a history that is already there succeeds and
        // writes nothing — which is right for a ledger replaying its own steps
        // and wrong to read as "a position was opened". Every identifier here
        // is a function of the fixture, so this is exactly what a corpus played
        // twice into one ledger looks like, and it is a repeat rather than a
        // second position.
        let written = self
            .db
            .record_execution_logs(&rows)
            .map_err(|err| format!("the entry for {} could not be recorded: {err}", launch.mint))?;
        if written != rows.len() {
            return Err(format!(
                "the entry for {} could not be recorded: {intent_id} is already on the ledger                  ({written} of {} steps were new), so nothing was opened",
                launch.mint,
                rows.len()
            ));
        }
        if let Some(metrics) = self.metrics {
            for row in &rows {
                metrics.record_intent(row.prev_state, row.state);
            }
        }

        // Only after the ledger has it. A backend that believed it held a
        // position the file has no record of is the state a restart cannot
        // reconcile.
        let route = self
            .backend
            .pump_fun_route(&launch.mint, size_lamports)
            .map_err(|err| format!("{} has no exit route: {err}", launch.mint))?;
        self.backend.hold(
            &intent_id,
            MockPosition {
                route,
                landed: true,
            },
        );

        Ok(ExecutionOutcome {
            intent_id,
            signature,
            size_lamports: size,
            tokens: fill.tokens,
            fee_lamports: fill.fee_lamports,
            entry_slippage_bps: fill.slippage_bps,
            exit: None,
        })
    }

    /// Flattens every position this run opened, and says what became of each.
    ///
    /// Keyed by intent id rather than returned in order, because `Flattener`
    /// takes the whole book at once — it has to, so that one position with no
    /// route does not stop the next one — and the caller has to put the answers
    /// back against the launches they came from.
    fn flatten(
        &self,
        at_ms: i64,
        mine: &std::collections::BTreeSet<String>,
        problems: &mut Vec<String>,
    ) -> BTreeMap<String, ExitOutcome> {
        if mine.is_empty() {
            return BTreeMap::new();
        }
        let obligations = match self.db.open_obligations() {
            Ok(found) => found,
            Err(err) => {
                problems.push(format!(
                    "the open obligations could not be read, so nothing was flattened: {err}"
                ));
                return BTreeMap::new();
            }
        };
        let targets: Vec<ExitTarget> = obligations
            .iter()
            // Two filters, and both are load-bearing. `mine` is the positions
            // this run opened: a harness that flattened whatever it found would
            // sell a previous run's leftovers, which is somebody else's
            // decision to make. The mode filter is the harder stop behind it —
            // point `--db` at a real ledger and the live and paper obligations
            // in it are not this harness's to touch, whatever the first filter
            // says.
            .filter(|obligation| mine.contains(&obligation.intent_id))
            .filter(|obligation| obligation.mode == ExecutionMode::Replay)
            .filter_map(ExitTarget::from_obligation)
            .filter(ExitTarget::is_actionable)
            .collect();
        if targets.is_empty() {
            return BTreeMap::new();
        }

        let mut flattener = Flattener::new(self.backend, self.db, at_ms)
            .waiting_with(Box::new(FixtureClock { at_ms }));
        if let Some(metrics) = self.metrics {
            flattener = flattener.with_metrics(metrics);
        }
        let report = flattener.flatten(&targets);
        problems.extend(report.problems.iter().cloned());

        report
            .results
            .into_iter()
            .map(|result| {
                let still_at_risk = result.outcome.still_at_risk();
                let outcome = match result.outcome {
                    FlattenOutcome::Flattened {
                        exit_intent_id,
                        signature,
                        venue,
                        out_lamports,
                        realized_pnl_lamports,
                        reused,
                        ..
                    } => ExitOutcome {
                        exit_intent_id: Some(exit_intent_id),
                        signature,
                        venue,
                        out_lamports,
                        realized_pnl_lamports,
                        state: "flattened".to_string(),
                        still_at_risk,
                        detail: reused.then(|| "found, not sent by this run".to_string()),
                    },
                    FlattenOutcome::InFlight {
                        exit_intent_id,
                        signature,
                        venue,
                        state,
                        ..
                    } => ExitOutcome {
                        exit_intent_id: Some(exit_intent_id),
                        signature,
                        venue,
                        out_lamports: 0,
                        realized_pnl_lamports: 0,
                        state: "in-flight".to_string(),
                        still_at_risk,
                        detail: Some(state.to_string()),
                    },
                    FlattenOutcome::ResolvedToNothing { detail } => ExitOutcome {
                        exit_intent_id: None,
                        signature: None,
                        venue: None,
                        out_lamports: 0,
                        realized_pnl_lamports: 0,
                        state: "resolved-to-nothing".to_string(),
                        still_at_risk,
                        detail: Some(detail),
                    },
                    FlattenOutcome::Unresolved { detail } => ExitOutcome {
                        exit_intent_id: None,
                        signature: None,
                        venue: None,
                        out_lamports: 0,
                        realized_pnl_lamports: 0,
                        state: "unresolved".to_string(),
                        still_at_risk,
                        detail: Some(detail),
                    },
                    FlattenOutcome::Skipped { detail } => ExitOutcome {
                        exit_intent_id: None,
                        signature: None,
                        venue: None,
                        out_lamports: 0,
                        realized_pnl_lamports: 0,
                        state: "skipped".to_string(),
                        still_at_risk,
                        detail: Some(detail),
                    },
                    FlattenOutcome::Failed {
                        exit_intent_id,
                        failure,
                        detail,
                        left_on_network,
                    } => ExitOutcome {
                        exit_intent_id,
                        signature: None,
                        venue: None,
                        out_lamports: 0,
                        realized_pnl_lamports: 0,
                        state: "failed".to_string(),
                        still_at_risk,
                        detail: Some(format!(
                            "{failure}: {detail}{}",
                            if left_on_network {
                                " — and a transaction is on the network"
                            } else {
                                ""
                            }
                        )),
                    },
                };
                (result.target.intent_id, outcome)
            })
            .collect()
    }
}

// ===========================================================================
// the scenario
// ===========================================================================

/// One pass of a fixture corpus through the whole pipeline.
///
/// Synchronous, and deliberately so. It owns no runtime, opens no socket and
/// installs no signal handler; the only thing it can be interrupted by is a
/// [`StopFlag`] somebody else sets. That is what lets the integration suite run
/// the pipeline in a test process rather than spawning a daemon and scraping it.
pub struct Scenario<'a> {
    config: ScenarioConfig,
    execution: Option<SimulatedExecution<'a>>,
    metrics: Option<&'a MetricsCollector>,
    hub: Option<&'a TelemetryHub>,
    stop: Option<&'a StopFlag>,
    forensics: Option<(&'a StateLogger, RiskSnapshot)>,
}

impl<'a> Scenario<'a> {
    pub fn new(config: ScenarioConfig) -> Self {
        Scenario {
            config,
            execution: None,
            metrics: None,
            hub: None,
            stop: None,
            forensics: None,
        }
    }

    /// Gives the run a ledger and a signer to open positions against.
    ///
    /// Without this the run detects and reports and opens nothing, which is the
    /// same funnel and no trades — the difference `ScenarioConfig::execute` also
    /// makes, from the other side.
    pub fn executing_with(mut self, execution: SimulatedExecution<'a>) -> Self {
        self.execution = Some(execution);
        self
    }

    pub fn with_metrics(mut self, metrics: &'a MetricsCollector) -> Self {
        self.metrics = Some(metrics);
        self
    }

    pub fn publishing_to(mut self, hub: &'a TelemetryHub) -> Self {
        self.hub = Some(hub);
        self
    }

    pub fn stopping_on(mut self, stop: &'a StopFlag) -> Self {
        self.stop = Some(stop);
        self
    }

    /// Records every verdict into the forensic log as well as into the report.
    ///
    /// The report is a JSON document somebody reads once; the log is a table
    /// somebody queries in six weeks. Both are written from the same verdict at
    /// the same instant, so the two can be checked against each other — which
    /// is the point of `state_funnel` returning the same shape `Funnel` prints.
    ///
    /// `risk` is the base the risk half of each row is taken from. A fixture
    /// replay has no live account behind it, so equity and the drawdown limits
    /// are whatever the operator declared; the two fields the run actually
    /// knows — `at_ms` and `open_positions` — are filled in per row from the
    /// replay clock and from what the run is holding, and the drawdown is
    /// recomputed from the balances rather than carried, so a row cannot state
    /// a drawdown its own numbers disagree with.
    pub fn recording_to(mut self, logger: &'a StateLogger, risk: RiskSnapshot) -> Self {
        self.forensics = Some((logger, risk));
        self
    }

    fn stopped(&self) -> bool {
        self.stop.is_some_and(StopFlag::is_stopped)
    }

    fn say(&self, level: TelemetryLevel, message: String, data: serde_json::Value) {
        if let Some(hub) = self.hub {
            hub.publish(level, "scenario", message, data);
        }
    }

    /// Plays every case under the configured path, in name order.
    pub fn run(self) -> Result<PipelineReport, String> {
        let cases = case_directories(&self.config.fixtures)?;
        self.say(
            TelemetryLevel::Info,
            format!(
                "{} case(s) under {}",
                cases.len(),
                self.config.fixtures.display()
            ),
            serde_json::json!({
                "cases": cases.len(),
                "gateProfile": self.config.gate_profile.as_str(),
                "executing": self.config.execute && self.execution.is_some(),
            }),
        );

        let mut totals = Funnel::empty();
        let mut reports = Vec::with_capacity(cases.len());
        for dir in &cases {
            if self.stopped() {
                break;
            }
            let report = self.run_case(dir);
            totals.absorb(&report.funnel);
            reports.push(report);
        }

        let open_positions = reports
            .iter()
            .flat_map(|case| case.launches.iter())
            .filter_map(|launch| launch.execution.as_ref())
            // `is_none_or` would say this, and it is stable in 1.82 against
            // a crate that declares 1.77.2. Spelled out instead, which also
            // names the two cases: a position nothing exited is open, and one
            // an exit did not finish flattening still is.
            .filter(|opened| match &opened.exit {
                Some(exit) => exit.still_at_risk,
                None => true,
            })
            .count() as u32;

        Ok(PipelineReport {
            schema: REPORT_SCHEMA.to_string(),
            gate_profile: self.config.gate_profile,
            fee_bps: self.config.fee_bps,
            entry_lamports: self.config.entry_lamports,
            max_pool_share_bps: self.config.max_pool_share_bps,
            sandwich_guard: self.config.gate_params().sandwich_guard,
            private_entry: self.config.private_entry,
            window_ms: self.config.cluster.window_ms,
            executed: self.config.execute && self.execution.is_some(),
            open_positions,
            cases: reports,
            totals,
        })
    }

    /// Plays one case: the feed, the detector, the entries, and the exits.
    fn run_case(&self, dir: &Path) -> CaseReport {
        let case = dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("case")
            .to_string();

        let mut feed = match open_case(dir) {
            Ok(feed) => feed,
            Err(why) => {
                self.say(
                    TelemetryLevel::Warn,
                    format!("{case} was refused: {why}"),
                    serde_json::json!({ "case": case, "refused": why }),
                );
                return CaseReport::refused(&case, why);
            }
        };

        let mut report = CaseReport::opened(&case, &feed);

        self.say(
            TelemetryLevel::Info,
            format!(
                "replaying {} · {} record(s)",
                feed.stream_id, feed.record_count
            ),
            serde_json::json!({
                "case": case,
                "streamId": feed.stream_id,
                "records": feed.record_count,
                "chainVerified": feed.chain_verified,
                "complete": feed.complete,
            }),
        );

        let window_ms = self.config.cluster.window_ms;
        let mut launches: BTreeMap<String, OpenLaunch> = BTreeMap::new();
        let mut outcomes: Vec<LaunchOutcome> = Vec::new();
        let mut pacer = Pacer::new(self.config.speed);
        let mut last_at_ms = i64::MIN;
        let mut ticked_slot = 0u64;

        loop {
            if self.stopped() {
                report
                    .problems
                    .push("the run was stopped before the fixture ran out".to_string());
                break;
            }

            let Some((advance, record)) = feed.driver.step() else {
                break;
            };
            let fed = Fed {
                seq: record.seq,
                slot: advance.slot,
                at_ms: advance.at_ms,
                frame: record.frame.clone(),
                outcome: record.outcome,
            };

            report.records += 1;
            last_at_ms = fed.at_ms;
            pacer.pace(fed.at_ms, self.stop);

            let started = std::time::Instant::now();

            // Decided **before** this record is applied, and that order is the
            // whole of the no-leakage property. This record's timestamp is past
            // the window — that is what makes the launch due — so applying it
            // first would put evidence from after the decision into the
            // decision. In the sybil case the record that closes the window is
            // the first record of the dump, and a gate that reads the dump is a
            // gate that cannot work in front of a live feed, where the dump has
            // not happened yet.
            //
            // Strictly past, so a record sharing an instant with the window's
            // last millisecond is still in the evidence: several buys land in
            // one slot at one timestamp, and a decision taken part way through
            // that instant would read half a bundle.
            self.decide_due(
                &mut launches,
                &mut outcomes,
                &mut report,
                &feed.stream_id,
                fed.seq,
                fed.at_ms,
                |launch| (launch.decide_at_ms(window_ms) < fed.at_ms).then_some(true),
            );
            self.feed_one(&fed, &mut launches, &mut report);

            if let Some(metrics) = self.metrics {
                if fed.slot > ticked_slot {
                    ticked_slot = fed.slot;
                    metrics.record_slot_tick(fed.slot, started.elapsed());
                }
            }
            if report.records % PROGRESS_RECORDS == 0 {
                self.say(
                    TelemetryLevel::Debug,
                    format!(
                        "{} · {} / {} records",
                        feed.stream_id, report.records, feed.record_count
                    ),
                    serde_json::json!({
                        "case": case,
                        "records": report.records,
                        "of": feed.record_count,
                    }),
                );
            }
        }

        // The recording ran out. Whatever is still open is decided on the
        // window it actually got, and `window_closed` says whether that was the
        // whole of one — which it is exactly when the last record was at or past
        // the window's end, since there is now nothing left that could have
        // arrived inside it. A launch whose window was cut short is reported and
        // is not executed on: a verdict over a window somebody truncated is a
        // verdict about a different launch, and `decide_due` enforces that
        // rather than leaving it to judgement.
        if !launches.is_empty() {
            let seq = feed.driver.cursor().position() as u64;
            self.decide_due(
                &mut launches,
                &mut outcomes,
                &mut report,
                &feed.stream_id,
                seq,
                last_at_ms,
                |launch| Some(last_at_ms >= launch.decide_at_ms(window_ms)),
            );
        }

        let clock = feed.driver.clock();
        report.clamped = clock.clamped();
        report.slot_regressions = clock.slot_regressions();

        if self.config.flatten_at_end && !self.stopped() {
            if let Some(execution) = &self.execution {
                let at_ms = if last_at_ms == i64::MIN {
                    0
                } else {
                    last_at_ms
                };
                let mine: std::collections::BTreeSet<String> = outcomes
                    .iter()
                    .filter_map(|outcome| outcome.execution.as_ref())
                    .map(|opened| opened.intent_id.clone())
                    .collect();
                let exits = execution.flatten(at_ms, &mine, &mut report.problems);
                for outcome in outcomes.iter_mut() {
                    let Some(opened) = outcome.execution.as_mut() else {
                        continue;
                    };
                    opened.exit = exits.get(&opened.intent_id).cloned();
                    if let Some(exit) = &opened.exit {
                        self.say(
                            TelemetryLevel::Info,
                            format!(
                                "{} exited: {} · {} lamports realized",
                                outcome.mint, exit.state, exit.realized_pnl_lamports
                            ),
                            serde_json::to_value(exit).unwrap_or_else(
                                |_| serde_json::json!({ "error": "the exit would not serialise" }),
                            ),
                        );
                    }
                }
            }
        }

        for outcome in &outcomes {
            report.funnel.decided += 1;
            if outcome.window_closed {
                report.funnel.window_closed += 1;
            }
            if outcome.enter {
                report.funnel.entered += 1;
            }
            report.funnel.count(outcome.reason);
        }
        report.launches = outcomes;
        report
    }

    /// Decodes one record and moves whatever it is about.
    ///
    /// A frame the recording says the live filters rejected is **not** decoded.
    /// It did not reach the engine when this was recorded and replaying it would
    /// move a curve that never moved. A frame a bounded channel refused is
    /// replayed, because that is the one disagreement with the recording that
    /// recovery is allowed to have, and it is counted separately so the
    /// disagreement is visible rather than assumed.
    fn feed_one(
        &self,
        fed: &Fed,
        launches: &mut BTreeMap<String, OpenLaunch>,
        report: &mut CaseReport,
    ) {
        let Some(frame) = fed.frame.as_deref() else {
            return;
        };
        report.frames += 1;

        match fed.outcome {
            RecordOutcome::Dropped(_) => {
                report.filtered += 1;
                if let Some(metrics) = self.metrics {
                    metrics.record_dropped(crate::metrics::DropReason::Filtered, 1);
                }
                return;
            }
            RecordOutcome::Backpressure(_) => {
                if let Some(metrics) = self.metrics {
                    metrics.record_dropped(crate::metrics::DropReason::Backpressure, 1);
                }
            }
            RecordOutcome::Accepted => {}
        }
        report.replayed += 1;
        if let Some(metrics) = self.metrics {
            metrics.record_ingested(1);
        }

        let event = match decode_event(frame, fed.seq) {
            Ok(event) => event,
            Err(err) => {
                report.undecodable += 1;
                report.problems.push(err.to_string());
                if let Some(metrics) = self.metrics {
                    metrics.record_dropped(crate::metrics::DropReason::Undecodable, 1);
                }
                return;
            }
        };
        report.events += 1;

        match event {
            LaunchEvent::Launch(open) => {
                // A second `launch` for one mint is the recording contradicting
                // itself. The first one is kept: the opening window is measured
                // from it, and letting a later line move the window would move
                // every offset the gate reads.
                report.funnel.seen += 1;
                if launches.contains_key(&open.mint) {
                    report.problems.push(format!(
                        "{} is launched twice; the second was ignored",
                        open.mint
                    ));
                    return;
                }
                launches.insert(
                    open.mint.clone(),
                    OpenLaunch {
                        mint: open.mint,
                        creator: open.creator,
                        opened_at_ms: open.at_ms,
                        curve: open.curve,
                        recorded_tokens: 0,
                        buyers: BTreeMap::new(),
                        evidence_to_ms: None,
                        decided: false,
                    },
                );
            }

            LaunchEvent::Flow(flow) => {
                let Some(launch) = launches.get_mut(&flow.mint) else {
                    return;
                };
                // The curve keeps moving after the verdict, because it is a
                // mirror of the recording and the recording carried on. The
                // *evidence* does not: what the gate read is fixed at the
                // moment it read it, and a buyers map that kept growing
                // afterwards would make `evidence_to_ms` describe the fixture
                // rather than the decision.
                let frozen = launch.decided;
                match flow.side {
                    EventSide::Buy => {
                        // A buy the curve refused did not happen, so its wallet
                        // is not a buyer and does not enter the gate's numbers.
                        let Ok(fill) = launch
                            .curve
                            .quote_buy(flow.gross_lamports, self.config.fee_bps)
                        else {
                            return;
                        };
                        launch.curve = launch.curve.after_buy(&fill);
                        if frozen {
                            return;
                        }
                        launch.evidence_to_ms =
                            Some(launch.evidence_to_ms.unwrap_or(flow.at_ms).max(flow.at_ms));
                        let seen = launch.buyers.entry(flow.wallet.clone()).or_insert_with(|| {
                            BuyerAccumulator {
                                funder: flow.funder.clone(),
                                first_buy_ms: flow.at_ms,
                                sol_in_lamports: 0,
                                sol_out_lamports: 0,
                                tx_count: 0,
                            }
                        });
                        seen.first_buy_ms = seen.first_buy_ms.min(flow.at_ms);
                        seen.sol_in_lamports =
                            seen.sol_in_lamports.saturating_add(flow.gross_lamports);
                        seen.tx_count = seen.tx_count.saturating_add(1);
                        // A funder learned later fills in an earlier unknown; a
                        // funder that disagrees with itself keeps the first
                        // answer, because the recording contradicting itself is
                        // not licence to pick the more incriminating reading.
                        if seen.funder.is_none() {
                            seen.funder.clone_from(&flow.funder);
                        }
                    }
                    EventSide::Sell => {
                        let Ok(fill) = launch.curve.quote_sell(flow.tokens, self.config.fee_bps)
                        else {
                            return;
                        };
                        launch.curve = launch.curve.after_sell(&fill);
                        if frozen {
                            return;
                        }
                        // Only against a wallet already known to have bought.
                        // A sell from an address the opening window never saw is
                        // somebody else's position and says nothing about the
                        // buyers this launch is being judged on.
                        if let Some(seen) = launch.buyers.get_mut(&flow.wallet) {
                            seen.sol_out_lamports =
                                seen.sol_out_lamports.saturating_add(fill.net_lamports);
                            launch.evidence_to_ms =
                                Some(launch.evidence_to_ms.unwrap_or(flow.at_ms).max(flow.at_ms));
                        }
                    }
                }
            }

            // The recording's own entries and exits. They moved the curve when
            // this was recorded, so they move the mirror now. They are not this
            // run's decisions and nothing is opened from them.
            LaunchEvent::Entry(entry) => {
                let Some(launch) = launches.get_mut(&entry.mint) else {
                    return;
                };
                if let Ok(fill) = launch
                    .curve
                    .quote_buy(entry.gross_lamports, self.config.fee_bps)
                {
                    launch.curve = launch.curve.after_buy(&fill);
                    launch.recorded_tokens = launch.recorded_tokens.saturating_add(fill.tokens);
                }
            }

            LaunchEvent::Exit(exit) => {
                let Some(launch) = launches.get_mut(&exit.mint) else {
                    return;
                };
                let wanted = exit
                    .tokens
                    .unwrap_or(launch.recorded_tokens)
                    .min(launch.recorded_tokens);
                if wanted == 0 {
                    return;
                }
                if let Ok(fill) = launch.curve.quote_sell(wanted, self.config.fee_bps) {
                    launch.curve = launch.curve.after_sell(&fill);
                    launch.recorded_tokens = launch.recorded_tokens.saturating_sub(wanted);
                }
            }

            LaunchEvent::Pull(pull) => {
                let Some(launch) = launches.get_mut(&pull.mint) else {
                    return;
                };
                let magnitude = pull.lamports.min(launch.curve.real_sol_reserves);
                if magnitude == 0 {
                    return;
                }
                let signed = i64::try_from(magnitude).unwrap_or(i64::MAX);
                if let Some(next) = launch.curve.displaced(-signed) {
                    launch.curve = next;
                }
            }

            // Neither moves the curve, and neither is read by the entry rule:
            // concentration over a holder snapshot is `backtest.rs`'s job, and a
            // label is ground truth, which is exactly the thing a decision is
            // not allowed to see.
            LaunchEvent::Holders(_) | LaunchEvent::Label(_) => {}
        }
    }

    /// Reads every launch the predicate says is due, and acts on the verdicts.
    ///
    /// `due` answers `None` for a launch that is not ready and
    /// `Some(window_closed)` for one that is, where the boolean is whether the
    /// opening window is known to have run its full length. Only a launch whose
    /// window closed can be entered.
    #[allow(clippy::too_many_arguments)]
    fn decide_due<F>(
        &self,
        launches: &mut BTreeMap<String, OpenLaunch>,
        outcomes: &mut Vec<LaunchOutcome>,
        report: &mut CaseReport,
        stream_id: &str,
        seq: u64,
        now_ms: i64,
        due: F,
    ) where
        F: Fn(&OpenLaunch) -> Option<bool>,
    {
        let ready: Vec<(String, bool)> = launches
            .iter()
            .filter(|(_, launch)| !launch.decided)
            .filter_map(|(mint, launch)| due(launch).map(|closed| (mint.clone(), closed)))
            .collect();

        for (mint, window_closed) in ready {
            let Some(launch) = launches.get_mut(&mint) else {
                continue;
            };
            launch.decided = true;
            let window_ms = self.config.cluster.window_ms;
            let record = launch.record(window_ms);
            let buyers = record.buyers.len() as u32;
            // Our own order, priced against the curve as the recording left it
            // at this instant — the same curve the fill will be quoted from, and
            // the same size, because both come from `ScenarioConfig::entry_size`.
            //
            // Built here rather than inside `enter` because the guard is a
            // reason to *not* enter, and a check that ran after the decision to
            // enter would be a check that could only ever report. The order this
            // puts the two questions in is the order `syndicate_gate` already
            // uses: every question about the launch is asked first, and this one
            // is asked last, of a launch the rest of the rule already liked.
            let quote = self.config.entry_quote(&launch.curve);
            let (_, verdict) = evaluate(
                &record,
                &self.config.cluster,
                &self.config.gate_params(),
                quote.as_ref(),
            );

            let mut outcome = LaunchOutcome {
                mint: mint.clone(),
                creator: launch.creator.clone(),
                opened_at_ms: launch.opened_at_ms,
                decided_at_ms: now_ms,
                window_closed,
                buyers,
                evidence_to_ms: launch.evidence_to_ms.unwrap_or(launch.opened_at_ms),
                largest_funder_wallets: launch.largest_funder_wallets(window_ms),
                enter: verdict.enter,
                reason: verdict.reason,
                confidence_micros: verdict.confidence_micros,
                tags: verdict.tags.clone(),
                bundle_wallets: verdict.bundle_wallets,
                cohort_wallets: verdict.cohort_wallets,
                cohort_lamports: verdict.cohort_lamports,
                real_sol_lamports: launch.curve.real_sol_reserves,
                virtual_sol_lamports: launch.curve.virtual_sol_reserves,
                quoted_lamports: quote.as_ref().map(|q| q.gross_lamports),
                sandwich: verdict.sandwich,
                refused_on_our_order: verdict.reason.is_about_our_order(),
                execution: None,
            };

            self.say(
                if verdict.enter {
                    TelemetryLevel::Info
                } else {
                    TelemetryLevel::Debug
                },
                format!(
                    "{mint}: {} at {} millionths over {buyers} buyer(s)",
                    verdict.reason.as_str(),
                    verdict.confidence_micros
                ),
                serde_json::to_value(&outcome).unwrap_or_else(
                    |_| serde_json::json!({ "error": "the verdict would not serialise" }),
                ),
            );

            // Three things have to be true, and the third is the one that is
            // easy to forget: a run that has been asked to stop does not open a
            // position. `run_case` skips the flattening once the flag is set,
            // so an entry taken after it would be one nothing was ever going to
            // close.
            let may_execute =
                verdict.enter && window_closed && self.config.execute && !self.stopped();
            if verdict.enter && !window_closed {
                report.problems.push(format!(
                    "{mint} cleared the gate on a window the recording cut short, so nothing was opened"
                ));
            }
            if verdict.enter && window_closed && self.config.execute && self.stopped() {
                report.problems.push(format!(
                    "{mint} cleared the gate after the run was asked to stop, so nothing was opened"
                ));
            }
            if let (true, Some(execution)) = (may_execute, &self.execution) {
                match execution.enter(stream_id, launch, seq, now_ms, &self.config) {
                    Ok(opened) => {
                        self.say(
                            TelemetryLevel::Info,
                            format!(
                                "{mint}: entered {} lamports for {} tokens at {} bps of slippage",
                                opened.size_lamports, opened.tokens, opened.entry_slippage_bps
                            ),
                            serde_json::to_value(&opened).unwrap_or_else(
                                |_| serde_json::json!({ "error": "the entry would not serialise" }),
                            ),
                        );
                        outcome.execution = Some(opened);
                    }
                    Err(why) => {
                        // The gate said yes and the position could not be
                        // opened. Reported as a problem and left as a verdict
                        // with no execution behind it, which is the honest shape
                        // — quietly turning it into a refusal would put a
                        // plumbing failure into the funnel as a strategy result.
                        report.problems.push(why.clone());
                        self.say(
                            TelemetryLevel::Warn,
                            format!("{mint} cleared the gate and could not be opened: {why}"),
                            serde_json::json!({ "mint": mint, "detail": why }),
                        );
                    }
                }
            }

            self.record_forensics(&outcome, &verdict, outcomes, buyers, window_closed, now_ms);
            outcomes.push(outcome);
        }
    }

    /// Puts one verdict into the forensic log.
    ///
    /// Nothing here can fail loudly: `observe` is a `try_send` onto a bounded
    /// queue and drops rather than blocking. That is the right trade for this
    /// caller — a replay that stalled because a writer was behind would be a
    /// replay whose timings no longer mean anything — and the drop is counted
    /// on [`StateLogger::stats`] rather than swallowed.
    fn record_forensics(
        &self,
        outcome: &LaunchOutcome,
        verdict: &GateVerdict,
        so_far: &[LaunchOutcome],
        buyers: u32,
        window_closed: bool,
        now_ms: i64,
    ) {
        let Some((logger, base)) = &self.forensics else {
            return;
        };

        // The three shapes, kept apart. A launch the gate refused is the
        // strategy saying no; a launch it accepted that opened nothing is
        // everything after the strategy saying no — the window, the risk gate,
        // the stop flag, `--no-execute`. Folding the second into the first
        // would make the funnel blame the rule for the account's decision.
        let decision = match (&outcome.execution, verdict.enter) {
            (Some(_), _) => Decision::Entered,
            (None, true) => Decision::Deferred,
            (None, false) => Decision::Refused,
        };

        // What this run is holding right now: opened, and not yet closed.
        let open = so_far
            .iter()
            .filter(|past| {
                past.execution
                    .as_ref()
                    .is_some_and(|execution| execution.exit.is_none())
            })
            .count();

        let risk = RiskSnapshot {
            at_ms: now_ms,
            open_positions: u16::try_from(open).unwrap_or(u16::MAX),
            ..*base
        }
        .with_recomputed_drawdown();

        logger.observe(StateRecord::decided(
            outcome.mint.clone(),
            verdict,
            &risk,
            decision,
            outcome
                .execution
                .as_ref()
                .map(|execution| execution.intent_id.clone()),
            buyers,
            outcome.real_sol_lamports,
            window_closed,
            outcome.evidence_to_ms.min(now_ms),
            now_ms,
        ));
    }
}

// ===========================================================================
// stage 5 — telemetry export
// ===========================================================================

/// Where the telemetry stream is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TelemetryTarget {
    /// Standard error, so the report on standard output stays a JSON document
    /// a pipe can read.
    Stderr,
    /// Appended to a file. Appended rather than truncated: a daemon restarted
    /// after an incident must not erase the recording of the incident.
    File(PathBuf),
}

/// The telemetry stream as one JSON object per line.
///
/// **The stream starts where the sink was registered, not where the process
/// was.** The hub belongs to the engine, so the handful of lines the engine
/// publishes while starting — the lifecycle line and the database health check
/// — are published before there is anything to register with it. They are
/// missing rather than silently absent: `seq` is a per-process counter, so the
/// first line in the file says how many came before it.
///
/// **Flushed on every line.** A buffered audit trail is one that loses exactly
/// the part somebody is reading it for — the last few seconds before the thing
/// that killed the process. The cost is a write syscall per event, which the
/// bounded queue in front of it already limits to a rate the engine has decided
/// it can afford to drop past.
pub struct NdjsonSink {
    out: Mutex<Box<dyn Write + Send>>,
    written: AtomicU64,
    failed: AtomicU64,
}

impl NdjsonSink {
    pub fn open(target: &TelemetryTarget) -> Result<Self, String> {
        let out: Box<dyn Write + Send> = match target {
            TelemetryTarget::Stderr => Box::new(std::io::stderr()),
            TelemetryTarget::File(path) => {
                if let Some(parent) = path.parent() {
                    if !parent.as_os_str().is_empty() {
                        std::fs::create_dir_all(parent)
                            .map_err(|err| format!("{}: {err}", parent.display()))?;
                    }
                }
                Box::new(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .map_err(|err| format!("{}: {err}", path.display()))?,
                )
            }
        };
        Ok(NdjsonSink {
            out: Mutex::new(out),
            written: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        })
    }

    /// Lines written out, and lines that could not be.
    pub fn counts(&self) -> (u64, u64) {
        (
            self.written.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
        )
    }

    pub fn flush(&self) {
        let _ = self.out.lock().flush();
    }
}

impl TelemetrySink for NdjsonSink {
    fn deliver(&self, event: &TelemetryEvent) {
        let Ok(line) = serde_json::to_string(event) else {
            self.failed.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let mut out = self.out.lock();
        if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
            // A full disk must not take the engine down, and there is nowhere
            // left to report this to — the reporting channel is the thing that
            // just failed. Counted, and the count is on the report.
            self.failed.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.written.fetch_add(1, Ordering::Relaxed);
    }
}

// ===========================================================================
// the daemon
// ===========================================================================

/// Everything the headless process needs to know.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub database: PathBuf,
    /// `None` runs the engine and no pipeline: sockets, telemetry and metrics,
    /// and nothing playing into them. That is the shape a daemon watching live
    /// feeds has.
    pub scenario: Option<ScenarioConfig>,
    pub telemetry: Option<TelemetryTarget>,
    pub metrics_addr: Option<std::net::SocketAddr>,
    /// True stops when the corpus runs out. False waits for a signal even after
    /// it has, which is what a daemon does.
    pub once: bool,
    pub maintenance: MaintenanceSchedule,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        DaemonConfig {
            database: crate::db::database_path(),
            scenario: None,
            telemetry: None,
            metrics_addr: None,
            once: true,
            maintenance: MaintenanceSchedule::default(),
        }
    }
}

/// Builds the engine, plays whatever it was given, and stops cleanly.
///
/// The teardown is the same sequence `lib.rs` runs when the window closes, in
/// the same order and for the same reasons: the exporter stops taking scrapes,
/// the sockets stop taking frames, the engine is told it is shutting down while
/// there is still a runtime to stop it on, and the database is checkpointed
/// last so a final audit row from anywhere still lands.
///
/// **A signal sells nothing.** Positions open when the signal arrives are
/// reported open. Flattening is a trade, and a process that traded because
/// somebody pressed Ctrl-C would be making a decision that the person pressing
/// it had not.
pub fn run(config: DaemonConfig) -> Result<DaemonReport, String> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("sts-daemon")
        .build()
        .map_err(|err| format!("the daemon has no scheduler without a tokio runtime: {err}"))?;
    // The same handle the window's build installs, so `spawn_candidate_observer`
    // and `spawn_feed_bridge` — which are `lib.rs`'s and are shared rather than
    // copied — land on this runtime rather than on one Tauri makes for itself.
    tauri::async_runtime::set(runtime.handle().clone());

    let database = Database::open(&config.database)
        .map_err(|err| format!("STS could not open {}: {err}", config.database.display()))?;
    let engine = Arc::new(Engine::start_with(database, config.maintenance));

    let metrics = Arc::new(MetricsCollector::new());
    engine.attach_metrics(Arc::clone(&metrics));

    // The one backend this build has, whose `is_live` is false. Installed
    // explicitly here and nowhere in `lib.rs::run`, and that difference is the
    // point rather than an oversight: the shipped window ships with no signer
    // at all, and a harness that cannot sign cannot exercise the path it exists
    // to exercise. Nothing about installing a mock crosses the promotion gate —
    // what would cross it is a backend that answers `is_live` with true, and
    // there is not one.
    let backend = Arc::new(MockSolanaSigner::new());
    engine.install_execution_engine(Arc::clone(&backend) as Arc<dyn ExecutionEngine>);

    let mut problems = Vec::new();
    let sink = match &config.telemetry {
        Some(target) => match NdjsonSink::open(target) {
            Ok(sink) => {
                let sink = Arc::new(sink);
                engine
                    .telemetry()
                    .observe(Arc::clone(&sink) as Arc<dyn TelemetrySink>);
                Some(sink)
            }
            Err(why) => {
                // Worth having, not worth refusing to start a trading engine
                // over — the same call `start_metrics_exporter` makes about its
                // own port.
                problems.push(format!("the telemetry stream is not being written: {why}"));
                None
            }
        },
        None => None,
    };

    // The same check the window's build runs on the way up, and it happens here
    // rather than beside `Engine::start_with` because of where the sink is.
    // A headless run has nobody watching a pane, so the NDJSON stream is the
    // only place a person sees this — and published before the sink subscribed,
    // the finding would go to a hub with no listeners and be gone. After the
    // sink and before the pipeline: the first thing on the stream, and before
    // anything has written a row.
    //
    // Nothing is blocked on the result. A file that does not verify is a
    // finding, and the run carries on; the audit row is the durable half.
    let warm = crate::forensics::verify_on_start(
        &engine.database(),
        &engine.telemetry(),
        crate::telemetry::now_ms(),
    );
    for report in warm.iter().filter(|report| !report.is_clean()) {
        problems.push(format!(
            "the {} book does not match its own checkpoints — {} broken link(s)",
            report.mode.as_str(),
            report.chain.breaks.len()
        ));
    }

    let stop = Arc::new(StopFlag::new());
    let (stopped_tx, stopped_rx) = tokio::sync::watch::channel(false);

    let guard = runtime.enter();
    let (ingestion, streams) = IngestionManager::start(
        IngestionConfig::from_env(),
        Arc::new(WebSocketDialer),
        Some(engine.database()),
        Some(engine.telemetry()),
    );
    crate::spawn_candidate_observer(streams, engine.telemetry(), Arc::clone(&metrics));
    crate::spawn_feed_bridge(
        Arc::clone(&ingestion),
        Arc::clone(&metrics),
        Arc::clone(&engine),
    );
    // The same second feed the window gets, wired the same way: sequenced by
    // `subslot::TickRing` and then handed to the same manager. A headless run
    // that filtered its Geyser stream differently from the windowed one would
    // be a harness that proves something about a build nobody ships.
    let geyser = GeyserFeed::start(
        GeyserConfig::from_env(),
        crate::geyser::default_transport(),
        Arc::clone(&ingestion),
        Some(engine.telemetry()),
    );

    let exporter = match config.metrics_addr {
        Some(addr) => match BoundExporter::bind(addr) {
            Ok(bound) => {
                let addr = bound.addr();
                engine.telemetry().publish(
                    TelemetryLevel::Info,
                    "metrics",
                    format!("metrics on http://{addr}/metrics"),
                    serde_json::json!({ "listening": true, "addr": addr.to_string() }),
                );
                Some(bound.serve(Arc::clone(&metrics)))
            }
            Err(err) => {
                problems.push(format!("the metrics exporter did not start: {err}"));
                None
            }
        },
        None => None,
    };

    watch_for_signals(Arc::clone(&stop), Arc::clone(&engine), stopped_tx);
    drop(guard);

    engine.telemetry().publish(
        TelemetryLevel::Info,
        "daemon",
        "headless engine up",
        serde_json::json!({
            "database": config.database.display().to_string(),
            "metrics": exporter.as_ref().map(|e| e.addr().to_string()),
            "signer": backend.name(),
            "signerLive": backend.is_live(),
            "once": config.once,
        }),
    );

    // The pipeline runs on this thread. Everything above it is on the runtime,
    // so a scenario that takes an hour does not starve the exporter or the
    // sockets.
    // Bound rather than called inline: both return an `Arc` by value, and a
    // borrow of a temporary would not outlive the statement it was taken in.
    let hub = engine.telemetry();
    let db = engine.database();
    // Every verdict this run reaches, into `journal_state_log`. Started here
    // rather than inside `Scenario` because it owns a thread and a queue and
    // has to be stopped on the way out — and because a scenario run from a test
    // process should be able to decide for itself whether it wants one.
    let forensics = StateLogger::start(Arc::clone(&db), ExecutionMode::Replay);
    let pipeline = match &config.scenario {
        Some(scenario_config) => {
            let mut scenario = Scenario::new(scenario_config.clone())
                .with_metrics(&metrics)
                .publishing_to(&hub)
                .stopping_on(&stop)
                .recording_to(&forensics, REPLAY_RISK);
            if scenario_config.execute {
                scenario = scenario
                    .executing_with(SimulatedExecution::new(&db, &backend).with_metrics(&metrics));
            }
            match scenario.run() {
                Ok(report) => report,
                Err(why) => {
                    stop.stop(StopReason::Exhausted);
                    // Before the engine goes: the writer drains what is queued
                    // and joins. A run that failed is the run whose verdicts are
                    // most worth still having.
                    forensics.stop();
                    teardown(&engine, &ingestion, &geyser, exporter.as_ref());
                    runtime.shutdown_timeout(SHUTDOWN_GRACE);
                    return Err(why);
                }
            }
        }
        None => empty_pipeline(),
    };

    if !config.once && !stop.is_stopped() {
        engine.telemetry().publish(
            TelemetryLevel::Info,
            "daemon",
            "waiting for SIGINT or SIGTERM",
            serde_json::json!({ "cases": pipeline.cases.len() }),
        );
        wait_for_stop(&runtime, stopped_rx);
    }

    let stopped_by = stop.reason().unwrap_or(StopReason::Exhausted);
    // The forensic writer goes before the engine does. It drains what is on its
    // queue and joins, so the last batch of verdicts is on disk before the
    // database handle it writes through is closed.
    let forensic_stats = {
        forensics.stop();
        forensics.stats()
    };
    if forensic_stats.dropped > 0 {
        problems.push(format!(
            "{} forensic state row(s) were dropped because the writer fell behind",
            forensic_stats.dropped
        ));
    }
    if forensic_stats.failed > 0 {
        problems.push(format!(
            "{} forensic batch(es) would not commit",
            forensic_stats.failed
        ));
    }
    // One last checkpoint over everything this run wrote, so the file it leaves
    // behind has its log accounted for rather than waiting on the next process
    // to notice. Best effort: a run that has already finished is not failed by
    // a checkpoint it could not take, and the next `verify_on_start` says so.
    if let Err(err) = db.take_journal_snapshot(ExecutionMode::Replay, crate::telemetry::now_ms()) {
        problems.push(format!("the replay book could not be checkpointed: {err}"));
    }
    teardown(&engine, &ingestion, &geyser, exporter.as_ref());

    let (exported, unwritable) = sink.as_ref().map(|s| s.counts()).unwrap_or((0, 0));
    if unwritable > 0 {
        problems.push(format!(
            "{unwritable} telemetry line(s) could not be written"
        ));
    }
    if let Some(sink) = &sink {
        sink.flush();
    }

    let report = DaemonReport {
        pipeline,
        process: ProcessReport {
            stopped_by,
            signer: backend.name().to_string(),
            signer_live: backend.is_live(),
            kill_switch_armed: engine.is_halted(),
            telemetry_exported: exported,
            telemetry_dropped: engine.telemetry().snapshot().dropped,
            metrics_addr: exporter.as_ref().map(|e| e.addr().to_string()),
            metrics: metrics.snapshot(),
            problems,
        },
    };

    runtime.shutdown_timeout(SHUTDOWN_GRACE);
    Ok(report)
}

/// A report for a run that was given no fixtures.
fn empty_pipeline() -> PipelineReport {
    let config = ScenarioConfig::default();
    PipelineReport {
        schema: REPORT_SCHEMA.to_string(),
        gate_profile: config.gate_profile,
        fee_bps: config.fee_bps,
        entry_lamports: config.entry_lamports,
        max_pool_share_bps: config.max_pool_share_bps,
        sandwich_guard: config.gate_params().sandwich_guard,
        private_entry: config.private_entry,
        window_ms: config.cluster.window_ms,
        executed: false,
        open_positions: 0,
        cases: Vec::new(),
        totals: Funnel::empty(),
    }
}

/// Stops taking work, then stops.
///
/// Idempotent in every part: `begin_shutdown` and `finish_shutdown` both check,
/// and `stop` on the exporter, on ingestion and on the Geyser feed is safe
/// twice.
fn teardown(
    engine: &Engine,
    ingestion: &IngestionManager,
    geyser: &GeyserFeed,
    exporter: Option<&MetricsExporter>,
) {
    if let Some(exporter) = exporter {
        exporter.stop();
    }
    ingestion.stop();
    geyser.stop();
    engine.begin_shutdown();
    engine.finish_shutdown();
}

/// Blocks the calling thread until something asks the run to stop.
fn wait_for_stop(
    runtime: &tokio::runtime::Runtime,
    mut stopped: tokio::sync::watch::Receiver<bool>,
) {
    runtime.block_on(async move {
        while !*stopped.borrow_and_update() {
            // An error means every sender is gone, which can only happen if the
            // watcher task died. Nothing is ever going to set the flag, so
            // waiting longer is waiting forever.
            if stopped.changed().await.is_err() {
                return;
            }
        }
    });
}

// ===========================================================================
// signals
// ===========================================================================

/// What a second signal does, when the first one is already being honoured.
///
/// 128 + SIGINT, which is the code a shell reports for a process that died on
/// one. The process leaves without checkpointing, which is the whole meaning of
/// pressing it twice — and is safe to do, because `sts.db` is in WAL mode and a
/// WAL is recovered on the next open. What is lost is the truncating checkpoint,
/// not the data.
const IMPATIENT_EXIT_CODE: i32 = 130;

/// Turns SIGINT and SIGTERM into a stop, and a second one into an exit.
///
/// The first signal sets the flag, which the feed reads between records, and
/// tells the engine it is shutting down so anything watching that flag stops
/// taking new work. The run then tears down on the thread that was driving it,
/// which is what makes the teardown ordered rather than racing the handler.
#[cfg(unix)]
fn watch_for_signals(
    stop: Arc<StopFlag>,
    engine: Arc<Engine>,
    stopped_tx: tokio::sync::watch::Sender<bool>,
) {
    tauri::async_runtime::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};

        let (mut interrupt, mut terminate) = match (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) {
            (Ok(interrupt), Ok(terminate)) => (interrupt, terminate),
            _ => {
                // Nothing can be done about this from in here and there is
                // no report to put it on yet. A daemon that cannot hear a
                // signal still runs; it just has to be stopped by a harder
                // one.
                engine.telemetry().publish(
                    TelemetryLevel::Warn,
                    "daemon",
                    "the signal handlers could not be installed; \
                         SIGINT will not tear this process down cleanly",
                    serde_json::json!({ "handlers": false }),
                );
                return;
            }
        };

        let name = tokio::select! {
            _ = interrupt.recv() => "SIGINT",
            _ = terminate.recv() => "SIGTERM",
        };
        engine.telemetry().publish(
            TelemetryLevel::Warn,
            "daemon",
            format!("{name}: stopping at the next record"),
            serde_json::json!({ "signal": name }),
        );
        stop.stop(StopReason::Signalled(name.to_string()));
        engine.begin_shutdown();
        let _ = stopped_tx.send(true);

        // Somebody who signals twice has said they are not waiting for the
        // teardown. Honoured rather than ignored: a daemon that swallows a
        // second Ctrl-C is one that has to be found and killed by hand.
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
        std::process::exit(IMPATIENT_EXIT_CODE);
    });
}

/// Ctrl-C only, which is all a non-unix target has here.
#[cfg(not(unix))]
fn watch_for_signals(
    stop: Arc<StopFlag>,
    engine: Arc<Engine>,
    stopped_tx: tokio::sync::watch::Sender<bool>,
) {
    tauri::async_runtime::spawn(async move {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        engine.telemetry().publish(
            TelemetryLevel::Warn,
            "daemon",
            "Ctrl-C: stopping at the next record",
            serde_json::json!({ "signal": "CTRL_C" }),
        );
        stop.stop(StopReason::Signalled("CTRL_C".to_string()));
        engine.begin_shutdown();
        let _ = stopped_tx.send(true);

        if tokio::signal::ctrl_c().await.is_ok() {
            std::process::exit(IMPATIENT_EXIT_CODE);
        }
    });
}

// ===========================================================================
// the command line
// ===========================================================================

/// `sts daemon`, the headless entrypoint.
///
/// Deliberately the same shape as `sts backtest`: one word after the binary
/// name, flags that report what they could not read rather than panicking, and
/// exit codes a shell script can branch on. `main.rs` asks
/// [`cli::is_subcommand`] before it builds a window, so a launch from Finder —
/// which passes macOS's own `-psn_...` argument — never lands here.
pub mod cli {
    use super::*;
    use std::io::Write;

    pub const USAGE: &str = "\
sts daemon — run the engine with no window

USAGE
  sts daemon run [--fixtures <dir>] [options]

WHAT IT DOES
  Builds the same engine the window builds — database, telemetry, metrics,
  ingestion — and then, given a fixture corpus, plays it through the whole
  pipeline: the replay feed, the entry rule, a simulated Jito execution
  against a signer that cannot reach a network, and the telemetry stream.
  Stops on SIGINT or SIGTERM, and stops cleanly: the feed ends between
  records, the ledger is checkpointed, and nothing is sold on the way out.

  Without --fixtures it runs the engine and no pipeline, which is the shape a
  daemon watching live provider feeds has. Provider URLs come from the
  environment; with none set, nothing is dialled.

OPTIONS
  --fixtures <dir>            A corpus of case directories, or one case
                              directory holding .jsonl streams.
  --db <file>                 The ledger. Default $STS_HOME/sts.db.
  --out <file>                Write the JSON report here instead of stdout.
  --telemetry <file>          Append the telemetry stream here as NDJSON.
                              `-` writes it to stderr.
  --metrics-addr <addr>       Serve /metrics here. A bare number is a port on
                              loopback. Loopback only, always.
  --speed <1|5|10|max>        How fast the feed plays a recording. Default
                              max, which is as fast as it parses.
  --gate-profile <name>       default (the shipped rule) or v1 (the rule
                              before the bundle checks). Default default.
  --window-ms <n>             The opening window the entry rule reads.
                              Default 3000.
  --entry-lamports <n>        What one accepted launch buys, before the
                              participation cap. Default 250000000.
  --fee-bps <n>               Swap fee on the SOL leg, under 10000. Default
                              100.
  --max-pool-share-bps <n>    The share of executable liquidity one position
                              may be, at most 10000. Default 150.
  --sandwich-guard <name>     off, when-quoted or required: whether our own
                              entry is priced against the curve before it is
                              allowed out, and whether an entry that could not
                              be priced is refused. Overrides the profile,
                              which is when-quoted on default and off on v1.
  --private-entry             Model the entry as a private bundle. The
                              exposure is still computed and reported; it
                              stops being a refusal, because a send nobody can
                              read first is not one §15.1 prices.
  --no-execute                Detect and report; open nothing. Same funnel.
  --no-flatten                Leave positions open when a recording ends.
  --wait                      Stay up after the corpus, until a signal.
  --gate                      Exit 2 if any case did not fully verify.
  --help

EXIT CODES
  0  the run finished, and under --gate every case verified
  1  the command line could not be read
  2  a case did not verify and --gate was given
  3  the engine could not be started, or a file could not be read or written
";

    /// Whether `sts <name>` belongs to this module.
    pub fn is_subcommand(name: &str) -> bool {
        name == "daemon"
    }

    /// The boolean flags. Everything else takes a value.
    const SWITCHES: [&str; 6] = [
        "no-execute",
        "no-flatten",
        "wait",
        "gate",
        "help",
        "private-entry",
    ];

    const KNOWN: [&str; 17] = [
        "fixtures",
        "db",
        "out",
        "telemetry",
        "metrics-addr",
        "speed",
        "gate-profile",
        "window-ms",
        "entry-lamports",
        "fee-bps",
        "max-pool-share-bps",
        "sandwich-guard",
        "private-entry",
        "no-execute",
        "no-flatten",
        "wait",
        "gate",
    ];

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
                    if SWITCHES.contains(&name) {
                        values.push((name.to_string(), "true".to_string()));
                        index += 1;
                        continue;
                    }
                    let value = args
                        .get(index + 1)
                        .ok_or_else(|| format!("--{name} needs a value"))?;
                    // `--telemetry -` is a value and not a missing one: a bare
                    // dash is the ordinary way to name a standard stream.
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

        /// The last one wins, so a wrapper script may append an override.
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

        fn unknown(&self) -> Vec<String> {
            self.values
                .iter()
                .map(|(name, _)| name.clone())
                .filter(|name| name != "help" && !KNOWN.contains(&name.as_str()))
                .collect()
        }
    }

    fn speed_from(text: &str) -> Option<ReplaySpeed> {
        [
            ReplaySpeed::Real,
            ReplaySpeed::Five,
            ReplaySpeed::Ten,
            ReplaySpeed::Max,
        ]
        .into_iter()
        .find(|speed| speed.as_str() == text)
    }

    /// Runs `sts daemon`.
    pub fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
        let Some(command) = args.first().map(String::as_str) else {
            let _ = out.write_all(USAGE.as_bytes());
            return 1;
        };
        if command == "daemon" {
            return run(&args[1..], out, err);
        }
        if matches!(command, "help" | "--help" | "-h") {
            let _ = out.write_all(USAGE.as_bytes());
            return 0;
        }

        let flags = match Flags::parse(&args[1..]) {
            Ok(flags) => flags,
            Err(detail) => {
                let _ = writeln!(err, "sts daemon: {detail}");
                return 1;
            }
        };
        if flags.has("help") {
            let _ = out.write_all(USAGE.as_bytes());
            return 0;
        }
        if let Some(extra) = flags.bare.first() {
            let _ = writeln!(err, "sts daemon: unexpected argument {extra:?}");
            return 1;
        }
        let unknown = flags.unknown();
        if !unknown.is_empty() {
            let _ = writeln!(err, "sts daemon: unknown option(s): {}", unknown.join(", "));
            return 1;
        }

        match command {
            "run" => start(&flags, out, err),
            other => {
                let _ = writeln!(err, "sts daemon: unknown command {other:?}");
                let _ = err.write_all(USAGE.as_bytes());
                1
            }
        }
    }

    fn configure(flags: &Flags) -> Result<DaemonConfig, String> {
        let mut config = DaemonConfig::default();

        if let Some(path) = flags.get("db") {
            config.database = PathBuf::from(path);
        }
        if let Some(text) = flags.get("metrics-addr") {
            config.metrics_addr = Some(parse_addr(text).map_err(|err| err.to_string())?);
        }
        if let Some(text) = flags.get("telemetry") {
            config.telemetry = Some(if text == "-" {
                TelemetryTarget::Stderr
            } else {
                TelemetryTarget::File(PathBuf::from(text))
            });
        }
        config.once = !flags.has("wait");

        if let Some(dir) = flags.get("fixtures") {
            let mut scenario = ScenarioConfig {
                fixtures: PathBuf::from(dir),
                ..ScenarioConfig::default()
            };
            if let Some(text) = flags.get("gate-profile") {
                scenario.gate_profile = GateProfile::parse(text).ok_or_else(|| {
                    format!(
                        "--gate-profile is not a profile: {text} — one of {}",
                        GateProfile::ALL
                            .iter()
                            .map(|p| p.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
            }
            if let Some(text) = flags.get("speed") {
                scenario.speed = speed_from(text).ok_or_else(|| {
                    format!("--speed is not a speed: {text} — one of 1, 5, 10, max")
                })?;
            }
            scenario.cluster.window_ms = flags.number("window-ms", scenario.cluster.window_ms)?;
            scenario.entry_lamports = flags.number("entry-lamports", scenario.entry_lamports)?;
            scenario.fee_bps = flags.number("fee-bps", scenario.fee_bps)?;
            scenario.max_pool_share_bps =
                flags.number("max-pool-share-bps", scenario.max_pool_share_bps)?;
            // Both of these feed §15.2's arithmetic, and both have a range
            // outside which that arithmetic stops meaning anything rather than
            // starting to answer wrongly — which is the worse failure, because
            // it is silent. A fee at or above 100% makes `beta_micros`, the
            // threshold and the break-even all degenerate to zero and
            // `sandwich_viable` answer false for every size, so the guard would
            // clear every order while reporting that it had read the curve. A
            // share above 100% of executable liquidity is not a participation
            // cap; §10 is a ceiling on what a position can come back out
            // through, and there is no reading of it under which a position is
            // larger than the pool.
            if scenario.fee_bps >= BPS_DENOMINATOR as u16 {
                return Err(format!(
                    "--fee-bps is a proportional fee and {} is all of it or more; \
                     the sandwich arithmetic has no answer above {}",
                    scenario.fee_bps,
                    BPS_DENOMINATOR - 1
                ));
            }
            if scenario.max_pool_share_bps > BPS_DENOMINATOR as u16 {
                return Err(format!(
                    "--max-pool-share-bps is a share of the pool and {} is more than all \
                     of it; the participation cap stops at {BPS_DENOMINATOR}",
                    scenario.max_pool_share_bps
                ));
            }
            if let Some(text) = flags.get("sandwich-guard") {
                scenario.sandwich_guard = Some(SandwichGuard::parse(text).ok_or_else(|| {
                    format!(
                        "--sandwich-guard is not a setting: {text} — one of {}",
                        SandwichGuard::ALL
                            .iter()
                            .map(|g| g.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?);
            }
            scenario.private_entry = flags.has("private-entry");
            scenario.execute = !flags.has("no-execute");
            scenario.flatten_at_end = !flags.has("no-flatten");
            config.scenario = Some(scenario);
        } else {
            for orphan in [
                "gate-profile",
                "speed",
                "window-ms",
                "entry-lamports",
                "fee-bps",
                "max-pool-share-bps",
                "sandwich-guard",
                "private-entry",
                "no-execute",
                "no-flatten",
            ] {
                if flags.has(orphan) {
                    return Err(format!(
                        "--{orphan} needs --fixtures; there is nothing to play"
                    ));
                }
            }
            // Without a corpus there is nothing that ends, so the only way this
            // stops is a signal. Saying so beats appearing to hang.
            config.once = false;
        }

        Ok(config)
    }

    fn start(flags: &Flags, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
        let config = match configure(flags) {
            Ok(config) => config,
            Err(detail) => {
                let _ = writeln!(err, "sts daemon: {detail}");
                return 1;
            }
        };

        let report = match super::run(config) {
            Ok(report) => report,
            Err(detail) => {
                let _ = writeln!(err, "sts daemon: {detail}");
                return 3;
            }
        };

        let text = match serde_json::to_string_pretty(&report) {
            Ok(text) => text,
            Err(detail) => {
                let _ = writeln!(err, "sts daemon: the report would not serialise: {detail}");
                return 3;
            }
        };
        match flags.get("out") {
            Some(path) => {
                if let Err(detail) = std::fs::write(path, format!("{text}\n")) {
                    let _ = writeln!(err, "sts daemon: {path}: {detail}");
                    return 3;
                }
                let _ = writeln!(out, "wrote {path}");
            }
            None => {
                let _ = writeln!(out, "{text}");
            }
        }

        for problem in &report.process.problems {
            let _ = writeln!(err, "sts daemon: {problem}");
        }

        // A refused case is a result and not a failure — a corpus carries cases
        // built to be refused — so it only decides the exit code when somebody
        // asked for a gate.
        if flags.has("gate") {
            let refused: Vec<&str> = report
                .pipeline
                .cases
                .iter()
                .filter(|case| case.refused.is_some() || case.chain_verified == Some(false))
                .map(|case| case.case.as_str())
                .collect();
            if !refused.is_empty() {
                let _ = writeln!(
                    err,
                    "sts daemon: refused under --gate: {}",
                    refused.join(", ")
                );
                return 2;
            }
        }
        0
    }
}

// ===========================================================================
// tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::{
        LAMPORTS_PER_SOL, LAUNCH_VIRTUAL_SOL_RESERVES, LAUNCH_VIRTUAL_TOKEN_RESERVES,
    };
    use crate::telemetry::TelemetryEvent;

    /// One SOL in lamports, spelled short because the sizing tests are unreadable
    /// otherwise.
    const SOL: u64 = LAMPORTS_PER_SOL;

    /// A directory of its own per test, removed when the test ends.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "sts-daemon-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("the temp directory is creatable");
            TempDir(path)
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("the parent is creatable");
        }
        std::fs::write(path, "").expect("the file is writable");
    }

    fn event(seq: u64, message: &str) -> TelemetryEvent {
        TelemetryEvent {
            seq,
            at_ms: 1_700_000_000_000,
            level: TelemetryLevel::Info,
            source: "test".to_string(),
            message: message.to_string(),
            data: serde_json::json!({}),
        }
    }

    // -----------------------------------------------------------------------
    // the funnel
    // -----------------------------------------------------------------------

    #[test]
    fn an_empty_funnel_still_names_every_reason() {
        let funnel = Funnel::empty();
        assert_eq!(funnel.reasons.len(), GateReason::ALL.len());
        for (index, reason) in GateReason::ALL.iter().enumerate() {
            assert_eq!(
                funnel.reasons[index].0,
                reason.as_str(),
                "worst first, always"
            );
            assert_eq!(funnel.reasons[index].1, 0);
        }
    }

    #[test]
    fn absorbing_adds_reason_by_reason_and_not_position_by_position() {
        let mut left = Funnel::empty();
        left.count(GateReason::Thin);
        left.seen = 1;

        let mut right = Funnel::empty();
        right.count(GateReason::Thin);
        right.count(GateReason::Accepted);
        right.seen = 2;
        right.entered = 1;

        left.absorb(&right);
        assert_eq!(left.seen, 3);
        assert_eq!(left.entered, 1);
        let counted = |name: &str| {
            left.reasons
                .iter()
                .find(|(reason, _)| reason == name)
                .map(|(_, count)| *count)
                .expect("the reason is named")
        };
        assert_eq!(counted("thin"), 2);
        assert_eq!(counted("accepted"), 1);
        assert_eq!(counted("low-score"), 0);
    }

    // -----------------------------------------------------------------------
    // policy
    // -----------------------------------------------------------------------

    #[test]
    fn every_gate_profile_round_trips_through_its_own_name() {
        for profile in GateProfile::ALL {
            assert_eq!(GateProfile::parse(profile.as_str()), Some(profile));
        }
        assert_eq!(
            GateProfile::parse("v2"),
            None,
            "a profile nobody wrote is not one"
        );
    }

    #[test]
    fn the_two_profiles_are_not_the_same_rule() {
        // The whole reason both exist. If these ever converge, the daemon's
        // profile switch is a switch between one thing and itself.
        assert_ne!(GateProfile::Default.params(), GateProfile::V1.params());
        assert_eq!(GateProfile::V1.params().min_bundle_wallets, 0);
        assert!(GateProfile::Default.params().min_bundle_wallets > 0);
    }

    // -----------------------------------------------------------------------
    // finding the cases
    // -----------------------------------------------------------------------

    #[test]
    fn a_directory_of_streams_is_one_case() {
        let root = TempDir::new("one-case");
        touch(&root.join("000.jsonl"));
        touch(&root.join("manifest.json"));
        let found = case_directories(&root.0).expect("the directory reads");
        assert_eq!(found, vec![root.0.clone()]);
    }

    #[test]
    fn a_directory_of_directories_is_a_corpus_in_name_order() {
        let root = TempDir::new("corpus");
        for case in ["zulu", "alpha", "mike"] {
            touch(&root.join(case).join("000.jsonl"));
        }
        // A directory with no streams in it is not a case and is not an error:
        // a corpus root also holds reports and notes.
        std::fs::create_dir_all(root.join("notes")).expect("the directory is creatable");

        let found = case_directories(&root.0).expect("the corpus reads");
        let names: Vec<&str> = found
            .iter()
            .map(|path| path.file_name().and_then(|n| n.to_str()).unwrap_or(""))
            .collect();
        assert_eq!(
            names,
            vec!["alpha", "mike", "zulu"],
            "sorted, because a run whose case order comes off the filesystem is not reproducible"
        );
    }

    #[test]
    fn a_directory_with_nothing_in_it_says_so_rather_than_running_empty() {
        let root = TempDir::new("empty");
        let refused = case_directories(&root.0).expect_err("an empty corpus is refused");
        assert!(refused.contains("no .jsonl"), "{refused}");

        let missing =
            case_directories(&root.join("nowhere")).expect_err("a missing path is refused");
        assert!(missing.contains("is not a directory"), "{missing}");
    }

    // -----------------------------------------------------------------------
    // pacing
    // -----------------------------------------------------------------------

    #[test]
    fn playing_at_max_speed_does_not_wait_for_the_recording() {
        let mut pacer = Pacer::new(ReplaySpeed::Max);
        let started = std::time::Instant::now();
        pacer.pace(0, None);
        // An hour of recording, in one step.
        pacer.pace(3_600_000, None);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "max speed paced an hour of recording in real time"
        );
    }

    #[test]
    fn a_stopped_run_does_not_sit_through_the_rest_of_a_gap() {
        let stop = StopFlag::new();
        stop.stop(StopReason::Signalled("SIGINT".to_string()));
        let mut pacer = Pacer::new(ReplaySpeed::Real);
        let started = std::time::Instant::now();
        pacer.pace(0, Some(&stop));
        pacer.pace(600_000, Some(&stop));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "a ten-minute gap held a stopped run for {:?}",
            started.elapsed()
        );
    }

    // -----------------------------------------------------------------------
    // the retry clock
    // -----------------------------------------------------------------------

    #[test]
    fn the_fixture_clock_advances_a_number_instead_of_a_thread() {
        let mut clock = FixtureClock { at_ms: 1_000 };
        let started = std::time::Instant::now();
        assert_eq!(clock.wait(30_000), 31_000);
        assert_eq!(clock.now_ms(), 31_000, "the schedule is walked, not slept");
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    // -----------------------------------------------------------------------
    // the export
    // -----------------------------------------------------------------------

    #[test]
    fn the_sink_writes_one_json_object_per_event_and_counts_them() {
        let root = TempDir::new("sink");
        let path = root.join("telemetry.ndjson");
        let sink = NdjsonSink::open(&TelemetryTarget::File(path.clone())).expect("the sink opens");

        sink.deliver(&event(0, "first"));
        sink.deliver(&event(1, "second"));
        sink.flush();

        assert_eq!(sink.counts(), (2, 0));
        let text = std::fs::read_to_string(&path).expect("the stream reads");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for (index, line) in lines.iter().enumerate() {
            let parsed: serde_json::Value =
                serde_json::from_str(line).expect("one object per line");
            assert_eq!(parsed["seq"], index as u64);
        }
    }

    #[test]
    fn the_sink_appends_rather_than_erasing_what_an_earlier_run_recorded() {
        // A daemon restarted after an incident must not erase the recording of
        // the incident.
        let root = TempDir::new("append");
        let path = root.join("telemetry.ndjson");

        let first = NdjsonSink::open(&TelemetryTarget::File(path.clone())).expect("opens");
        first.deliver(&event(0, "before the restart"));
        first.flush();
        drop(first);

        let second = NdjsonSink::open(&TelemetryTarget::File(path.clone())).expect("reopens");
        second.deliver(&event(1, "after it"));
        second.flush();

        let text = std::fs::read_to_string(&path).expect("the stream reads");
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("before the restart"));
        assert!(text.contains("after it"));
    }

    #[test]
    fn a_sink_creates_the_directory_its_file_was_asked_for_in() {
        let root = TempDir::new("mkdir");
        let path = root.join("logs").join("nested").join("telemetry.ndjson");
        let sink = NdjsonSink::open(&TelemetryTarget::File(path.clone())).expect("the sink opens");
        sink.deliver(&event(0, "hello"));
        sink.flush();
        assert!(path.is_file(), "the sink made room for its own file");
    }

    // -----------------------------------------------------------------------
    // stopping
    // -----------------------------------------------------------------------

    #[test]
    fn the_first_reason_for_stopping_is_the_one_that_is_kept() {
        let stop = StopFlag::new();
        assert!(!stop.is_stopped());
        assert_eq!(stop.reason(), None);

        stop.stop(StopReason::Signalled("SIGINT".to_string()));
        stop.stop(StopReason::Signalled("SIGTERM".to_string()));
        stop.stop(StopReason::Halted);

        assert!(stop.is_stopped());
        assert_eq!(
            stop.reason(),
            Some(StopReason::Signalled("SIGINT".to_string())),
            "a SIGTERM arriving while a SIGINT is being honoured has not changed why"
        );
    }

    // -----------------------------------------------------------------------
    // the command line
    // -----------------------------------------------------------------------

    fn cli(args: &[&str]) -> (i32, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let argv: Vec<String> = std::iter::once("daemon".to_string())
            .chain(args.iter().map(|a| a.to_string()))
            .collect();
        let code = cli::run(&argv, &mut out, &mut err);
        (
            code,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        )
    }

    #[test]
    fn the_subcommand_is_recognised_and_nothing_else_is() {
        assert!(cli::is_subcommand("daemon"));
        assert!(!cli::is_subcommand("backtest"));
        // The argument macOS hands a bundled binary launched from Finder. A
        // build that read this as a subcommand would refuse to open its window
        // when double-clicked.
        assert!(!cli::is_subcommand("-psn_0_12345"));
    }

    #[test]
    fn help_is_asked_for_in_every_spelling_and_answered_the_same_way() {
        for spelling in ["help", "--help", "-h"] {
            let (code, out, _) = cli(&[spelling]);
            assert_eq!(code, 0, "{spelling} is not an error");
            assert!(out.contains("sts daemon"), "{spelling} printed the usage");
        }
        let (code, out, _) = cli(&["run", "--help"]);
        assert_eq!(code, 0);
        assert!(out.contains("EXIT CODES"));
    }

    #[test]
    fn a_daemon_with_no_command_prints_the_usage_and_fails() {
        let (code, out, _) = cli(&[]);
        assert_eq!(code, 1);
        assert!(out.contains("sts daemon"));
    }

    #[test]
    fn an_unreadable_command_line_is_reported_rather_than_guessed_at() {
        let (code, _, err) = cli(&["dance"]);
        assert_eq!(code, 1);
        assert!(err.contains("unknown command"), "{err}");

        let (code, _, err) = cli(&["run", "--fixtures"]);
        assert_eq!(code, 1);
        assert!(err.contains("needs a value"), "{err}");

        let (code, _, err) = cli(&["run", "--fixtures", "--db", "x"]);
        assert_eq!(code, 1);
        assert!(err.contains("needs a value"), "{err}");

        let (code, _, err) = cli(&["run", "--nonsense", "1"]);
        assert_eq!(code, 1);
        assert!(err.contains("unknown option"), "{err}");

        let (code, _, err) = cli(&["run", "stray"]);
        assert_eq!(code, 1);
        assert!(err.contains("unexpected argument"), "{err}");
    }

    #[test]
    fn a_flag_about_the_pipeline_needs_a_pipeline_to_be_about() {
        // Silently ignoring `--gate-profile v1` on a run with no fixtures would
        // let somebody believe they had compared two rules.
        for orphan in [
            vec!["run", "--gate-profile", "v1"],
            vec!["run", "--speed", "5"],
            vec!["run", "--no-execute"],
        ] {
            let (code, _, err) = cli(&orphan);
            assert_eq!(code, 1, "{orphan:?}");
            assert!(err.contains("needs --fixtures"), "{orphan:?}: {err}");
        }
    }

    #[test]
    fn a_value_that_is_not_one_is_named_along_with_what_would_have_been() {
        let root = TempDir::new("cli-values");
        touch(&root.join("000.jsonl"));
        let dir = root.0.display().to_string();

        let (code, _, err) = cli(&["run", "--fixtures", &dir, "--gate-profile", "v9"]);
        assert_eq!(code, 1);
        assert!(err.contains("default, v1"), "{err}");

        let (code, _, err) = cli(&["run", "--fixtures", &dir, "--speed", "warp"]);
        assert_eq!(code, 1);
        assert!(err.contains("1, 5, 10, max"), "{err}");

        let (code, _, err) = cli(&["run", "--fixtures", &dir, "--fee-bps", "lots"]);
        assert_eq!(code, 1);
        assert!(err.contains("is not a number"), "{err}");

        let (code, _, err) = cli(&["run", "--metrics-addr", "example.com"]);
        assert_eq!(code, 1);
        assert!(err.contains("not an address"), "{err}");
    }

    // -----------------------------------------------------------------------
    // the entry quote: sizing, and the guard that reads it
    // -----------------------------------------------------------------------

    /// A curve deep enough that the participation cap is the only thing keeping
    /// our order under the front-run threshold. See
    /// `the_cap_is_what_keeps_the_shipped_size_safe` for where the number comes
    /// from.
    const DEEP_REAL_SOL: u64 = 70_000_000_000;

    fn curve_at(real_sol_lamports: u64) -> CurveState {
        CurveState::at_real_sol(real_sol_lamports)
    }

    /// A config that asks for more than any cap will give it, so the cap is
    /// always the binding constraint and the test is about the cap.
    fn unbounded_request() -> ScenarioConfig {
        ScenarioConfig {
            entry_lamports: u64::MAX,
            ..ScenarioConfig::default()
        }
    }

    #[test]
    fn the_gate_is_quoted_the_size_the_executor_would_actually_fill() {
        // The property the whole wiring rests on: one function answers "how big
        // is this order", and both the guard and the fill call it. If these two
        // could drift, the guard would be refusing a trade nobody was going to
        // make.
        let config = unbounded_request();
        for real_sol in [
            1_000_000_000u64,
            10_000_000_000,
            40_000_000_000,
            DEEP_REAL_SOL,
            84_000_000_000,
        ] {
            let curve = curve_at(real_sol);
            let quote = config.entry_quote(&curve).expect("a live curve quotes");
            assert_eq!(
                quote.gross_lamports,
                config.entry_size(&curve),
                "the quote and the fill disagreed about size at {real_sol} lamports of real SOL"
            );
        }
    }

    #[test]
    fn the_participation_cap_scales_the_quote_rather_than_the_request() {
        // §10's cap is a share of the pool, so the quote is linear in it and
        // has nothing to do with what was asked for once the ask is above it.
        let curve = curve_at(DEEP_REAL_SOL);
        let at = |bps: u16| {
            ScenarioConfig {
                max_pool_share_bps: bps,
                ..unbounded_request()
            }
            .entry_quote(&curve)
            .map(|q| q.gross_lamports)
        };

        assert_eq!(at(150), Some(1_050_000_000), "1.5% of 70 SOL");
        assert_eq!(
            at(300),
            Some(2_100_000_000),
            "twice the cap is twice the order"
        );
        assert_eq!(at(500), Some(3_500_000_000), "5% of 70 SOL");
        assert_eq!(
            at(0),
            None,
            "a cap of nothing is not a smaller position, it is none"
        );
    }

    #[test]
    fn a_request_under_the_cap_is_the_size_that_is_quoted() {
        // The cap is a ceiling, not a target. An operator asking for a quarter
        // of a SOL on a deep curve gets a quarter of a SOL.
        let curve = curve_at(DEEP_REAL_SOL);
        let config = ScenarioConfig {
            entry_lamports: 250_000_000,
            ..ScenarioConfig::default()
        };
        let quote = config.entry_quote(&curve).expect("a live curve quotes");
        assert_eq!(quote.gross_lamports, 250_000_000);
    }

    #[test]
    fn the_quote_is_against_the_virtual_reserve_and_the_cap_against_the_real_one() {
        // Two reserves, two questions. The cap is what a position has to come
        // back out through; the threshold scales with the price's `y`. Mixing
        // them is a silent factor-of-three error on a fresh curve, so it is
        // pinned rather than trusted.
        let curve = curve_at(10_000_000_000);
        assert_ne!(curve.real_sol_reserves, curve.virtual_sol_reserves);
        let quote = unbounded_request()
            .entry_quote(&curve)
            .expect("a live curve quotes");
        assert_eq!(quote.virtual_sol_reserves, curve.virtual_sol_reserves);
        assert_eq!(quote.gross_lamports, curve.real_sol_reserves * 150 / 10_000);
    }

    #[test]
    fn three_curves_that_cannot_be_priced_are_quoted_as_nothing() {
        let config = unbounded_request();

        let mut graduated = curve_at(10_000_000_000);
        graduated.complete = true;
        assert_eq!(
            config.entry_quote(&graduated),
            None,
            "a graduated curve is a dead pool"
        );

        let mut implausible = curve_at(10_000_000_000);
        implausible.virtual_sol_reserves = 0;
        assert_eq!(
            config.entry_quote(&implausible),
            None,
            "those reserves are not a curve"
        );

        // Readable, and simply has no position in it. A different fact from the
        // two above, which is why `LaunchOutcome` carries the reserves next to
        // the quote instead of only the quote.
        let empty = curve_at(0);
        assert_eq!(config.entry_quote(&empty), None);
    }

    #[test]
    fn the_guard_refuses_a_cap_sized_entry_once_the_pool_is_deep_enough() {
        // The cap scales with real SOL and the threshold with virtual, and
        // virtual is the real reserve plus a constant — so the two cross, and
        // above the crossing 1.5% of the pool is an order worth front-running.
        let config = unbounded_request();
        let deep = config
            .entry_quote(&curve_at(DEEP_REAL_SOL))
            .expect("a live curve quotes");
        let check = SandwichCheck::of(&deep);
        assert!(
            check.above_threshold,
            "1.5% of a 70 SOL pool is over the threshold"
        );
        assert!(
            check.refuses(),
            "and a public send at that size is a refusal"
        );
        assert!(deep.gross_lamports > check.breakeven_lamports);

        let shallow = config
            .entry_quote(&curve_at(10_000_000_000))
            .expect("a live curve quotes");
        let check = SandwichCheck::of(&shallow);
        assert!(!check.above_threshold, "1.5% of a 10 SOL pool is not");
        assert!(!check.refuses());
    }

    #[test]
    fn the_crossing_is_somewhere_and_the_guard_does_not_flap_across_it() {
        // A threshold that answered yes, no, yes as the pool filled would be an
        // arithmetic bug rather than a rule. Monotone in depth, and it does
        // cross — a guard that never fires is not being tested by the rest of
        // these.
        let config = unbounded_request();
        let refuses = |real_sol: u64| {
            config
                .entry_quote(&curve_at(real_sol))
                .map(|q| SandwichCheck::of(&q).refuses())
                .unwrap_or(false)
        };

        let mut crossings = 0;
        let mut previous = false;
        for real_sol in (1_000_000_000..=84_000_000_000).step_by(1_000_000_000) {
            let now = refuses(real_sol);
            if now != previous {
                crossings += 1;
                previous = now;
            }
        }
        assert_eq!(
            crossings, 1,
            "the guard crossed more than once as the pool filled"
        );
        assert!(previous, "and it never came back down");
    }

    #[test]
    fn the_cap_is_what_keeps_the_shipped_size_safe() {
        // Why wiring the quote in changed no verdict on the shipped config: a
        // quarter of a SOL is under the threshold at every depth a curve
        // reaches. The guard is live and silent, which is a different thing
        // from absent — and if a future entry size changes that, this test is
        // where it should be noticed.
        let config = ScenarioConfig::default();
        for real_sol in (0..=84_000_000_000).step_by(1_000_000_000) {
            let Some(quote) = config.entry_quote(&curve_at(real_sol)) else {
                continue;
            };
            assert!(
                !SandwichCheck::of(&quote).refuses(),
                "the shipped entry was refused at {real_sol} lamports of real SOL"
            );
        }
    }

    #[test]
    fn the_guard_is_exact_at_the_lamport_that_crosses_the_threshold() {
        // `sandwich_viable` is `b(10⁴ - F)² > F·10⁴·y` — two multiplications and
        // a comparison, no division and so no rounding. The reported breakeven
        // is the same threshold expressed as a size, rounded up. This pins the
        // two against each other at the one lamport where a rounding error
        // would show: monotone, one crossing, and it lands on the breakeven
        // rather than a lamport either side of it.
        let curve = curve_at(DEEP_REAL_SOL);
        let y = curve.virtual_sol_reserves;
        let fee = ScenarioConfig::default().fee_bps;
        let breakeven = crate::replay::sandwich_breakeven_victim_lamports(y, fee);

        assert!(
            !crate::backtest::sandwich_viable(breakeven - 1, y, fee),
            "a lamport under the breakeven is under the threshold"
        );
        assert!(
            crate::backtest::sandwich_viable(breakeven + 1, y, fee),
            "and a lamport over it is over"
        );

        // Monotone across the crossing, with no flapping from a rounded
        // intermediate.
        let mut previous = false;
        for gross in (breakeven - 64)..=(breakeven + 64) {
            let now = crate::backtest::sandwich_viable(gross, y, fee);
            assert!(
                !(previous && !now),
                "the comparison went backwards at {gross}"
            );
            previous = now;
        }
        assert!(previous, "and it did cross");
    }

    #[test]
    fn nothing_on_the_entry_quote_path_uses_floating_point() {
        // A verdict that is stored, compared and replayed must not depend on
        // whose libm the build linked against, and the strategy module is
        // integers in named units for that reason. This module and the gate are
        // where a float would most plausibly arrive by accident — a share, a
        // percentage, a ratio — so the absence is asserted rather than assumed.
        //
        // One exclusion now, deliberate. Test modules are skipped, because
        // `syndicate.rs` and `fixed.rs` both check their integer answers
        // against the real formula in `f64` and that is the point of those
        // tests.
        //
        // `store_unit` was a second exclusion until the cluster scores began
        // being stored as the millionths the analyser already had. It rounded
        // an already-integer input to four decimal places to fit a `REAL`
        // column, and `db.rs` says why it went: the float was costing
        // determinism and buying nothing. The allowance is kept as a tripwire
        // rather than deleted — if a float returns to that boundary, `skipped`
        // stops being zero and this fails.
        //
        // The needles are assembled rather than written out, because a test
        // that greps for a string is a test that contains it.
        let needles = [format!("f{}", 64), format!("f{}", 32)];
        let storage_boundary = ["fn store_unit(", "as f32 / 10_000.0"];

        for (name, source) in [
            ("daemon.rs", include_str!("daemon.rs")),
            (
                "strategy/syndicate.rs",
                include_str!("strategy/syndicate.rs"),
            ),
            ("fixed.rs", include_str!("fixed.rs")),
        ] {
            let production = source
                .split_once("\n#[cfg(test)]")
                .map_or(source, |(before, _)| before);
            assert!(
                production.len() < source.len(),
                "{name} has no test module, so the split found nothing to trim"
            );

            let mut skipped = 0;
            for (number, line) in production.lines().enumerate() {
                let code = line.split("//").next().unwrap_or(line);
                if storage_boundary
                    .iter()
                    .any(|allowed| code.contains(allowed))
                {
                    skipped += 1;
                    continue;
                }
                for needle in &needles {
                    assert!(
                        !code.contains(needle.as_str()),
                        "{name}:{} introduced a float: {line}",
                        number + 1
                    );
                }
            }

            // Nothing is allowed through any more, on any of the three. A
            // float reintroduced at the storage boundary makes this non-zero.
            let expected = 0;
            assert_eq!(
                skipped, expected,
                "{name}: the storage-boundary allowance covered {skipped} lines, not {expected}"
            );
        }
    }

    /// One function's body, by the name it is declared under.
    ///
    /// Lines from the signature to the `}` at the signature's own indentation.
    /// Crude, and it does not have to be more than that — it is reading Rust
    /// this repository wrote, where a closing brace is where `rustfmt` puts it,
    /// and it fails loudly rather than quietly if the shape it expects is gone.
    fn body_of<'a>(source: &'a str, signature: &str) -> &'a str {
        let start = source
            .find(signature)
            .unwrap_or_else(|| panic!("{signature} is no longer declared where this test looks"));
        let line_start = source[..start].rfind('\n').map_or(0, |at| at + 1);
        // The declaration's own indentation, which is whitespace only — the
        // `pub const ` between it and the `fn` is not part of what closes the
        // block.
        let indent: String = source[line_start..start]
            .chars()
            .take_while(|c| c.is_whitespace())
            .collect();
        let closing = format!("\n{indent}}}\n");
        let end = source[line_start..]
            .find(&closing)
            .unwrap_or_else(|| panic!("{signature} has no closing brace at its own indentation"));
        &source[line_start..line_start + end + closing.len()]
    }

    #[test]
    fn the_arithmetic_under_the_entry_quote_is_integers_too() {
        // The test above covers the three files the quote is *assembled* in.
        // The four numbers on it are computed somewhere else — the
        // participation cap in `replay.rs`, §15.2's comparison and its two
        // reported ratios in `backtest.rs` — and both of those files are full
        // of legitimate floats elsewhere, so scanning them whole would assert
        // nothing. Scanned by function instead, which is the unit the guarantee
        // is actually about: `β` and the cap are stored, compared and replayed,
        // so neither may depend on whose libm the build linked against.
        //
        // The names are spelled here rather than derived, so deleting one of
        // these functions or moving it off this path fails this test instead of
        // silently shrinking what it covers.
        let needles = [format!("f{}", 64), format!("f{}", 32)];
        // The fourth column is what the extraction has to have actually found,
        // so a `body_of` that silently returned the wrong span — or a signature
        // that now matches somewhere else in the file — fails here rather than
        // passing by scanning nothing.
        let path: [(&str, &str, &str, &str); 5] = [
            (
                "replay.rs",
                include_str!("replay.rs"),
                "fn max_position_lamports(",
                "real_sol_reserves as u128 * max_pool_share_bps as u128",
            ),
            (
                "replay.rs",
                include_str!("replay.rs"),
                "fn sandwich_breakeven_victim_lamports(",
                "numerator.div_ceil(denominator)",
            ),
            (
                "backtest.rs",
                include_str!("backtest.rs"),
                "fn beta_micros(",
                "numerator / denominator",
            ),
            (
                "backtest.rs",
                include_str!("backtest.rs"),
                "fn beta_threshold_micros(",
                "div_ceil(remainder)",
            ),
            (
                "backtest.rs",
                include_str!("backtest.rs"),
                "fn sandwich_viable(",
                "left > right",
            ),
        ];

        for (file, source, signature, landmark) in path {
            let production = source
                .split_once("\n#[cfg(test)]")
                .map_or(source, |(before, _)| before);
            let body = body_of(production, signature);
            assert!(
                body.lines().count() > 3,
                "{file}: {signature} came back as {} line(s), which is not a function body",
                body.lines().count()
            );
            assert!(
                body.contains(landmark),
                "{file}: the span read for {signature} does not contain {landmark:?}, \
                 so this test is scanning the wrong lines"
            );
            for (number, line) in body.lines().enumerate() {
                let code = line.split("//").next().unwrap_or(line);
                for needle in &needles {
                    assert!(
                        !code.contains(needle.as_str()),
                        "{file}: {signature} introduced a float on its line {}: {line}",
                        number + 1
                    );
                }
            }
        }
    }

    /// A curve whose two reserves are set independently.
    ///
    /// Every other curve in these tests comes from [`CurveState::at_real_sol`],
    /// which walks the launch curve and therefore ties `virtual_sol` to
    /// `real_sol + 30 SOL`. That tie is true of a pump.fun curve and it is
    /// exactly what makes those tests unable to say *which* reserve a number
    /// was read off: on such a curve, scaling with one is scaling with the
    /// other plus a constant. `ingestion::BondingCurve` decodes six independent
    /// numbers off an account, so this builds the pair directly.
    ///
    /// The token side is chosen to keep the constant product where the launch
    /// curve would put it, so the quote is priced against something that
    /// behaves like a curve rather than against a shape that could not exist.
    fn decoupled(virtual_sol: u64, real_sol: u64) -> CurveState {
        let k = u128::from(LAUNCH_VIRTUAL_TOKEN_RESERVES) * u128::from(LAUNCH_VIRTUAL_SOL_RESERVES);
        let virtual_token = (k / u128::from(virtual_sol.max(1))).min(u128::from(u64::MAX)) as u64;
        CurveState::from_parts(
            virtual_token,
            virtual_sol,
            virtual_token / 2,
            real_sol,
            crate::replay::TOKEN_TOTAL_SUPPLY,
            false,
        )
    }

    #[test]
    fn the_cap_moves_with_the_real_reserve_and_the_threshold_with_the_virtual_one() {
        // The claim `entry_size`'s comment makes, stated as an experiment that
        // can only be run on a curve where the two reserves are free of each
        // other: hold one still, move the other, and exactly one of the two
        // numbers responds.
        let config = unbounded_request();

        // Virtual held still. The cap is a share of the real reserve, so the
        // order doubles; the break-even is a function of `y` alone, so it does
        // not move at all.
        let shallow = config
            .entry_quote(&decoupled(40 * SOL, 10 * SOL))
            .expect("a live curve");
        let deep = config
            .entry_quote(&decoupled(40 * SOL, 20 * SOL))
            .expect("a live curve");
        assert_eq!(
            deep.gross_lamports,
            2 * shallow.gross_lamports,
            "twice the pool, twice the order"
        );
        assert_eq!(
            SandwichCheck::of(&deep).breakeven_lamports,
            SandwichCheck::of(&shallow).breakeven_lamports,
            "the threshold did not move, because `y` did not"
        );

        // Real held still. Now the order is the same and the threshold is the
        // thing that moved — and it moved in the safe direction, which is why a
        // deep curve tolerates an order a thin one does not.
        let cheap = config
            .entry_quote(&decoupled(40 * SOL, 20 * SOL))
            .expect("a live curve");
        let dear = config
            .entry_quote(&decoupled(80 * SOL, 20 * SOL))
            .expect("a live curve");
        assert_eq!(
            dear.gross_lamports, cheap.gross_lamports,
            "the cap reads the real reserve only"
        );

        // Linear in `y`, to within the lamport that `div_ceil` is: the
        // break-even rounds up, and rounding up a half and doubling it is not
        // the same as rounding up the whole. Asserted as a bound rather than an
        // equality because the exact answer is the one `sandwich_viable` gives
        // without dividing at all, and §15.2 is explicit that there is no sign
        // to assert at the threshold.
        let cheap_breakeven = SandwichCheck::of(&cheap).breakeven_lamports;
        let dear_breakeven = SandwichCheck::of(&dear).breakeven_lamports;
        assert!(
            dear_breakeven.abs_diff(2 * cheap_breakeven) <= 1,
            "the threshold reads the virtual reserve only: {dear_breakeven} is not twice {cheap_breakeven}"
        );
    }

    #[test]
    fn a_curve_the_two_reserves_disagree_about_is_judged_on_both_of_them() {
        // The pair a launch curve cannot produce and an account can: a pool
        // holding real SOL against a virtual reserve smaller than the launch
        // constant. The quote is large because the pool is deep and the
        // threshold is low because the price is thin, and the guard refuses —
        // which is the answer, and it is one neither reserve gives on its own.
        let config = unbounded_request();
        let thin_price = decoupled(SOL, 20 * SOL);
        let quote = config
            .entry_quote(&thin_price)
            .expect("a live curve quotes");

        assert_eq!(
            quote.gross_lamports,
            20 * SOL * 150 / 10_000,
            "1.5% of the real reserve"
        );
        assert_eq!(
            quote.virtual_sol_reserves, SOL,
            "priced against the reserve that sets the price"
        );

        let check = SandwichCheck::of(&quote);
        assert!(
            check.above_threshold,
            "an order that size against a reserve that thin is farmable"
        );
        assert!(check.refuses());
        assert!(quote.gross_lamports > check.breakeven_lamports);

        // And the mirror image, which is the pair that makes the tie in
        // `at_real_sol` look like the whole story: a deep price and a shallow
        // pool clears with room to spare.
        let deep_price = config
            .entry_quote(&decoupled(200 * SOL, SOL))
            .expect("a live curve");
        assert!(!SandwichCheck::of(&deep_price).refuses());
    }

    #[test]
    fn beta_and_the_break_even_agree_across_every_pair_of_reserves() {
        // `beta_micros` divides and `sandwich_viable` does not, so the two can
        // only be checked against each other where the rounding lands — and the
        // reported break-even is a third statement of the same threshold. All
        // three are pinned together here over reserves that vary independently,
        // which is the case the tied launch curve never produces.
        let config = unbounded_request();
        let fee = config.fee_bps;
        for virtual_sol in [SOL, 7 * SOL, 30 * SOL, 111 * SOL, 4_000 * SOL] {
            for real_sol in [SOL, 13 * SOL, 84 * SOL, 900 * SOL] {
                let curve = decoupled(virtual_sol, real_sol);
                let quote = config.entry_quote(&curve).expect("a live curve quotes");
                let check = SandwichCheck::of(&quote);
                let at = format!(
                    "y={virtual_sol}, real={real_sol}, b={}",
                    quote.gross_lamports
                );

                // The exact comparison is the one to believe, and the break-even
                // is that same threshold as a size: at or under it, no.
                assert_eq!(
                    check.above_threshold,
                    quote.gross_lamports > check.breakeven_lamports,
                    "the break-even and the comparison disagreed at {at}"
                );

                // The two reported ratios bracket it. `beta_threshold_micros`
                // rounds up, so a beta strictly above it is always genuinely
                // above the threshold; the converse is the rounding and is not
                // asserted, which is what §15.2 says about that point.
                if check.beta_micros > check.beta_threshold_micros {
                    assert!(
                        check.above_threshold,
                        "beta cleared the threshold and the exact comparison did not, at {at}"
                    );
                }

                // And `y` is the reserve the threshold read, never the pool.
                assert_eq!(check.virtual_sol_reserves, virtual_sol, "at {at}");
                assert_eq!(
                    check.breakeven_lamports,
                    crate::replay::sandwich_breakeven_victim_lamports(virtual_sol, fee),
                    "the break-even is a function of the virtual reserve alone, at {at}"
                );
            }
        }
    }

    #[test]
    fn the_cap_is_total_over_every_share_and_every_pool() {
        // §10's cap is arithmetic on numbers that come off somebody else's
        // account, so it has to be total: monotone in the share, never larger
        // than the pool it is a share of, and never wrapping. A cap that wrapped
        // would report a small number, which reads exactly like a cap doing its
        // job.
        for real_sol in [0u64, SOL, 85 * SOL, u64::MAX / 2, u64::MAX] {
            let curve = decoupled(30 * SOL, real_sol);
            let mut previous = 0u64;
            for bps in [0u16, 1, 150, 500, 5_000, 10_000, u16::MAX] {
                let room = curve.max_position_lamports(bps);
                assert!(
                    room >= previous,
                    "the cap fell as the share rose at {real_sol}/{bps}"
                );
                previous = room;
                if bps <= 10_000 {
                    assert!(
                        room <= real_sol,
                        "{room} is more than the whole pool at {real_sol}/{bps}"
                    );
                }
                // And the config's answer is the cap or the request, whichever
                // binds. `unbounded_request` asks for more than any cap gives,
                // so here that is the cap; the other side of the `min` is
                // `a_request_under_the_cap_is_the_size_that_is_quoted`.
                let asked = ScenarioConfig {
                    max_pool_share_bps: bps,
                    ..unbounded_request()
                };
                assert_eq!(asked.entry_size(&curve), room);
                let modest = ScenarioConfig {
                    entry_lamports: 1,
                    ..asked
                };
                assert_eq!(modest.entry_size(&curve), room.min(1));
            }
        }

        // The one pair that would wrap a `u64`, answered with the largest
        // position there is rather than with a truncated small one.
        let enormous = decoupled(30 * SOL, u64::MAX);
        assert_eq!(enormous.max_position_lamports(u16::MAX), u64::MAX);
    }

    #[test]
    fn a_private_bundle_is_priced_and_reported_and_not_refused() {
        // §15.4. The exposure is the whole justification for a tip, so it is
        // still computed — what changes is that it stops being a refusal,
        // because a send nobody reads first is not the thing §15.1 prices.
        let curve = curve_at(DEEP_REAL_SOL);
        let public = unbounded_request()
            .entry_quote(&curve)
            .expect("a live curve quotes");
        let private = ScenarioConfig {
            private_entry: true,
            ..unbounded_request()
        }
        .entry_quote(&curve)
        .expect("a live curve quotes");

        assert_eq!(
            private.gross_lamports, public.gross_lamports,
            "same order, different route"
        );
        let check = SandwichCheck::of(&private);
        assert!(check.above_threshold, "the exposure is still on the record");
        assert!(!check.refuses(), "and it is a note rather than a refusal");
        assert!(
            SandwichCheck::of(&public).refuses(),
            "which the public send is not"
        );
    }

    #[test]
    fn the_guard_override_beats_the_profile_on_both_of_them() {
        let default_profile = ScenarioConfig::default();
        assert_eq!(
            default_profile.gate_params().sandwich_guard,
            SandwichGuard::WhenQuoted
        );

        let v1 = ScenarioConfig {
            gate_profile: GateProfile::V1,
            ..ScenarioConfig::default()
        };
        assert_eq!(
            v1.gate_params().sandwich_guard,
            SandwichGuard::Off,
            "v1 shipped without it"
        );

        for guard in SandwichGuard::ALL {
            for profile in GateProfile::ALL {
                let config = ScenarioConfig {
                    gate_profile: profile,
                    sandwich_guard: Some(guard),
                    ..ScenarioConfig::default()
                };
                assert_eq!(
                    config.gate_params().sandwich_guard,
                    guard,
                    "{} did not take the override",
                    profile.as_str()
                );
            }
        }
    }

    #[test]
    fn the_override_changes_nothing_else_about_the_profile() {
        // The guard is the one gate parameter that is about our order rather
        // than the launch, so overriding it must leave the rule it is bolted to
        // exactly where it was.
        let overridden = ScenarioConfig {
            sandwich_guard: Some(SandwichGuard::Required),
            ..ScenarioConfig::default()
        };
        let expected = GateParams {
            sandwich_guard: SandwichGuard::Required,
            ..GateParams::default()
        };
        assert_eq!(overridden.gate_params(), expected);
    }

    #[test]
    fn the_two_questions_the_gate_answers_stay_apart() {
        // The whole point of the separation: a funnel that cannot tell these
        // apart cannot say whether the rule stopped finding launches or the
        // order outgrew the curve.
        for reason in GateReason::ALL {
            let ours = matches!(reason, GateReason::NoCurveQuote | GateReason::SandwichRisk);
            assert_eq!(
                reason.is_about_our_order(),
                ours,
                "{} is on the wrong side of the line",
                reason.as_str()
            );
        }
        assert!(
            !GateReason::Accepted.is_about_our_order(),
            "accepted is about neither"
        );
    }

    #[test]
    fn the_two_knobs_the_sandwich_arithmetic_reads_are_held_to_their_range() {
        // Both of these are proportions, and both have a value at which §15.2's
        // arithmetic stops describing anything. The dangerous one is the fee: at
        // or above 100% every number on the check degenerates — `beta_micros`,
        // the threshold and the break-even all answer zero and `sandwich_viable`
        // answers false for every size — so the guard would clear every order
        // while the report said it had read the curve. Refused at the command
        // line rather than discovered in a funnel.
        let dir = TempDir::new("knob-range");
        let fixtures = dir.join("corpus");
        touch(&fixtures.join("stream.jsonl"));
        let corpus = fixtures.to_str().expect("a utf-8 path").to_string();

        let (code, _, err) = cli(&["run", "--fixtures", &corpus, "--fee-bps", "10000"]);
        assert_eq!(code, 1, "a fee of the whole trade was accepted");
        assert!(err.contains("10000") && err.contains("9999"), "{err}");
        assert!(
            !err.contains("  "),
            "the refusal is one sentence, not a wrapped literal: {err:?}"
        );

        let (code, _, err) = cli(&[
            "run",
            "--fixtures",
            &corpus,
            "--max-pool-share-bps",
            "10001",
        ]);
        assert_eq!(code, 1, "a position larger than the pool was accepted");
        assert!(err.contains("10001") && err.contains("10000"), "{err}");
        assert!(
            !err.contains("  "),
            "the refusal is one sentence, not a wrapped literal: {err:?}"
        );

        // And the edges of the range are inside it. Shown by pairing each with a
        // flag that is read *after* these two and is unreadable: the refusal
        // names that flag, which it could only reach by having got past the
        // range check first. A run started here would build an engine, and a
        // unit test is not the place for one.
        for allowed in [
            vec!["--fee-bps", "9999"],
            vec!["--fee-bps", "0"],
            vec!["--max-pool-share-bps", "10000"],
            vec!["--max-pool-share-bps", "0"],
        ] {
            let mut args = vec!["run", "--fixtures", &corpus];
            args.extend_from_slice(&allowed);
            args.extend_from_slice(&["--sandwich-guard", "sometimes"]);
            let (code, _, err) = cli(&args);
            assert_eq!(code, 1);
            assert!(
                err.contains("sometimes"),
                "{allowed:?} was stopped before the guard was read: {err}"
            );
        }
    }

    #[test]
    fn a_fee_at_the_top_of_its_range_would_have_silently_disarmed_the_guard() {
        // Why the check above is worth having, stated as the behaviour it stops
        // reaching a run. This is the arithmetic the command line now refuses:
        // at a fee of 100% there is no order the model calls farmable, on any
        // curve, at any size.
        let curve = decoupled(SOL, 10_000 * SOL);
        let quote = EntryQuote {
            gross_lamports: u64::MAX / 2,
            virtual_sol_reserves: curve.virtual_sol_reserves,
            fee_bps: BPS_DENOMINATOR as u16,
            private_bundle: false,
        };
        let check = SandwichCheck::of(&quote);
        assert!(
            !check.above_threshold,
            "the comparison has no answer at a fee of everything"
        );
        assert!(!check.refuses(), "so the guard would have cleared it");
        assert_eq!(check.beta_micros, 0);
        assert_eq!(check.beta_threshold_micros, 0);
        assert_eq!(check.breakeven_lamports, 0);
    }

    #[test]
    fn the_guard_name_a_person_reads_is_the_one_they_type() {
        for guard in SandwichGuard::ALL {
            assert_eq!(SandwichGuard::parse(guard.as_str()), Some(guard));
            let json = serde_json::to_string(&guard).expect("it serialises");
            assert_eq!(
                json,
                format!("\"{}\"", guard.as_str()),
                "the report and the flag agree"
            );
        }
        assert_eq!(SandwichGuard::parse("when quoted"), None);
        assert_eq!(SandwichGuard::parse(""), None);
    }

    #[test]
    fn a_guard_setting_that_does_not_exist_is_refused_by_name() {
        let dir = TempDir::new("guard-unknown");
        let fixtures = dir.join("corpus");
        touch(&fixtures.join("stream.jsonl"));
        let corpus = fixtures.to_str().expect("a utf-8 path").to_string();

        let (code, _, err) = cli(&[
            "run",
            "--fixtures",
            &corpus,
            "--sandwich-guard",
            "sometimes",
        ]);
        assert_eq!(code, 1, "the command line could not be read");
        assert!(
            err.contains("sometimes"),
            "the refusal quotes what was typed: {err}"
        );
        assert!(
            err.contains("off") && err.contains("when-quoted") && err.contains("required"),
            "and lists what would have worked: {err}"
        );
    }

    #[test]
    fn the_guard_flags_need_a_corpus_to_mean_anything() {
        // Silently ignoring a guard on a run with nothing to play would be a
        // run that answered a question it was never asked. The sizing knobs are
        // here for the same reason and were the omission: `--fee-bps`,
        // `--window-ms` and `--max-pool-share-bps` were read into a
        // `ScenarioConfig` that was then thrown away, so a run asked for a
        // looser cap and given no corpus exited zero having applied nothing.
        for extra in [
            vec!["--sandwich-guard", "required"],
            vec!["--private-entry"],
            vec!["--max-pool-share-bps", "500"],
            vec!["--fee-bps", "250"],
            vec!["--window-ms", "5000"],
        ] {
            let mut args = vec!["run"];
            args.extend_from_slice(&extra);
            let (code, _, err) = cli(&args);
            assert_eq!(code, 1, "{extra:?} was accepted without a corpus");
            assert!(err.contains("needs --fixtures"), "{extra:?}: {err}");
        }
    }
}
