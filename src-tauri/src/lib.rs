//! The STS desktop backend.
//!
//! `main.rs` is a one-liner into `run` below. Everything the window can ask for
//! goes through the commands here, and everything they touch lives in Tauri's
//! managed state: the `Engine`, the `IngestionManager`, the `GeyserFeed`, and
//! the `ReplaySession`.
//!
//! There are two feeds and one queue. `IngestionManager` dials Solana's
//! JSON-RPC pubsub, which every provider offers; `GeyserFeed` dials a validator
//! plugin over gRPC, which carries the write versions and slot statuses that
//! make sub-slot ordering and re-org rollback possible at all. What matters
//! here is that the second one does *not* get its own path to the engine: it
//! sequences its stream in `subslot::TickRing` and then hands each released
//! curve write to the same `IngestionManager`, through the same filters, onto
//! the same two channels. One queue, one set of thresholds, one strategy.
//!
//! Phase 0 scaffold: the lifecycle, the shutdown path, the panic hook, the
//! database handle and the telemetry fan-out are real, and ingestion and replay
//! now publish through them. What is still not here is the scoring engine, so
//! nothing between a candidate arriving and a decision being made exists yet.
//!
//! `metrics` is the other half of that reporting, and the two are not the same
//! thing. Telemetry is events — one line per thing that happened, fanned out to
//! whatever window is listening. Metrics are counts of the same run: tick
//! latency and cadence, what the feed cost, and where the executions are. `run`
//! builds one collector, gives it to the engine, feeds it from the observer
//! below, and opens an HTTP port for it only if `STS_METRICS_ADDR` says to.
//!
//! The replay commands are the one part of this file that changes what the rest
//! of the window *means*. `set_replay_playback` puts a recording behind the
//! clock and raises a bar over the panes saying nothing under it is live, and
//! this build cannot make that sentence true on its own — the session plays
//! into the clock and the cockpit, not into ingestion. So the command refuses
//! to start while a feed endpoint is connected. That refusal, not the bar, is
//! what stops the window claiming a live candidate was recorded.
//!
//! `execution` is the one module nothing here reaches into. It holds the
//! outbound signer interface and the position flattening the unwind command
//! runs, and `run` deliberately installs no backend into the engine: the
//! roadmap keeps the dispatcher simulation-only until it is explicitly
//! promoted, and an application that ships with a signer wired in is that
//! promotion whether or not anybody decided it.
//!
//! `daemon` is this file's opposite number: the same engine with no window in
//! front of it. It builds the lifecycle, the database, the telemetry fan-out
//! and the metrics port exactly as `run` does — the two share
//! `spawn_candidate_observer` and `spawn_feed_bridge` rather than each keeping
//! a copy — and then plays a fixture corpus through the entry rule and a
//! simulated execution instead of waiting for clicks. The one thing it installs
//! that `run` deliberately does not is an execution backend, and the backend is
//! the mock whose `is_live` is false.
//!
//! `tracer` and `clustering` are the forensic pair, and they are the one part
//! of this file that answers a question about *other people's* money rather
//! than about the engine's own. `tracer` walks funding edges backwards from a
//! wallet to whatever paid for it, under hard budgets, with exchanges and
//! routers absorbing so that a path may end at one and never pass through it.
//! `clustering` puts a launch's opening buyers behind their origins, measures
//! what those groups hold and when they bought, and says whether the shape is
//! an accumulation that happened before the curve migrated.
//!
//! Both are pure functions of their inputs — the graph arrives in the request
//! and no command here fetches anything from a chain — which is what makes a
//! report reproducible from the message that produced it. The only state is
//! `ClusterRegistry`, which keeps the last few reports so a window can ask for
//! one it did not request, and publishes each on telemetry as it lands.
//!
//! `types` is the exception to "everything goes through a command": it holds no
//! state and does no I/O, so nothing in it is reachable from the window yet. It
//! is the vocabulary the rest of the phases are written in — the execution
//! state machine, the operating mode, and the risk snapshot a decision is made
//! against — and it is here so those rules are settled and tested before there
//! is anything sending transactions to get them wrong.

pub mod alerting;
pub mod attribution;
pub mod backtest;
pub mod bundle;
pub mod chainproof;
pub mod clustering;
pub mod daemon;
pub mod db;
pub mod engine;
pub mod error;
pub mod execution;
pub mod fixed;
pub mod fixtures;
pub mod forensics;
pub mod geyser;
pub mod ingestion;
pub mod jito;
pub mod journal;
pub mod loadgen;
pub mod metrics;
pub mod mev_sim;
pub mod prometheus;
pub mod replay;
pub mod strategy;
pub mod subslot;
pub mod telemetry;
pub mod tracer;
pub mod types;
pub mod walkforward;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::ipc::Channel;
use tauri::{RunEvent, State};

use crate::alerting::{Alert, AlertDispatcher, AlertSnapshot, AlertSubscription, AlertThresholds};
use crate::backtest::{BacktestConfig, PaperRunner};
use crate::bundle::{BundleDeck, BundleTelemetry};
use crate::chainproof::{Attestation, LineageProof, VerificationPolicy};
use crate::clustering::{
    ClusterGraphReport, ClusterRegistry, ClusterRequest, ClusterSummary, ClusteringParams,
    TraceRequest, WalletTraceReport,
};
use crate::db::{data_dir, database_path, Database, ExecutionMode};
use crate::engine::{Engine, EngineStatus, KillSwitchReceipt, UnwindReceipt};
use crate::error::EngineError;
use crate::execution::LeaderHint;
use crate::forensics::{
    ChainReport, SnapshotRow, StateFunnel, StateLogFilter, StateRow, WarmStart,
};
use crate::geyser::{
    GeyserConfig, GeyserFeed, GeyserMonitor, GeyserSnapshot, TickEvent, TickPipeline,
};
use crate::ingestion::{
    IngestionConfig, IngestionManager, IngestionSnapshot, IngestionStreams, Route, SolPrice,
    WebSocketDialer, FAST_PATH_DEPTH, STANDARD_DEPTH,
};
use crate::journal::{JournalFilter, JournalTotals, TradeDetail, TradeRow};
use crate::metrics::{
    addr_from_env, BoundExporter, DropReason, MetricsCollector, MetricsExporter, MetricsSnapshot,
};
use crate::replay::{PlaybackState, ReplayControl, ReplaySession, ReplaySpeed, ReplayStatus};
use crate::telemetry::{TelemetryEvent, TelemetryHub, TelemetryLevel, TelemetrySubscription};
use crate::tracer::TraceEdge;

/// How long the runtime is given to finish in-flight work before the process
/// stops waiting for it. Long enough for a database write, short enough that
/// closing the window feels like closing a window.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// How often the replay session is asked to play a little more.
///
/// Four times a second: often enough that the playhead on the bar moves like a
/// playhead, rare enough that a fixture at `1x` is stepped in useful chunks
/// rather than one record at a time.
/// Public so a test can pin the cadence the transport is actually driven at
/// rather than a number retyped beside it. A ticker that quietly became a
/// second would still pass every test written against a private constant.
pub const REPLAY_TICK: Duration = Duration::from_millis(250);

/// The most often the replay status is published while a fixture is playing.
///
/// The window polls `get_replay_status` once a second as well, so this is not
/// the only way it finds out; it is what keeps a window opened halfway through
/// a run from waiting for the next poll. Rate limited because the audit trail
/// pane shows every telemetry line there is, and a playhead update four times a
/// second would bury everything else in it.
const REPLAY_TELEMETRY_INTERVAL: Duration = Duration::from_secs(1);

/// Where a fixture is expected to be, unless `$STS_REPLAY_FIXTURE` says
/// otherwise.
///
/// A sibling of `sts.db` under whichever directory `$STS_HOME` resolved to, so
/// the whole process still agrees on one place for its data. Nothing is created
/// here and nothing checks that it exists: a machine with no fixture on it is
/// the normal case, and it is answered when somebody asks for playback rather
/// than at startup.
fn fixture_dir() -> PathBuf {
    match std::env::var_os("STS_REPLAY_FIXTURE") {
        Some(path) => PathBuf::from(path),
        None => data_dir().join("fixtures"),
    }
}
/// How often ingestion's own counters are copied into the metrics collector.
///
/// Two seconds, which is slower than the window already polls
/// `get_ingestion_metrics` at while it is open. So the bridge below adds
/// strictly less contention on ingestion's locks than the UI does simply by
/// being visible — and the numbers it copies are totals over a whole run, which
/// nobody reads to two-second precision anyway.
const FEED_BRIDGE_INTERVAL: Duration = Duration::from_secs(2);

/// Everything the engine currently knows about itself: lifecycle, kill switch,
/// telemetry counters, and the state of `sts.db`.
#[tauri::command]
fn get_engine_status(engine: State<'_, Arc<Engine>>) -> Result<EngineStatus, EngineError> {
    engine.status()
}

/// Halts the engine and records why.
///
/// Deliberately infallible from the UI's point of view: the switch is armed
/// before anything that could fail is attempted, so the caller always gets a
/// receipt telling it what happened rather than an error it has to interpret
/// while trying to stop trading.
#[tauri::command]
fn trigger_kill_switch(
    engine: State<'_, Arc<Engine>>,
    reason: Option<String>,
) -> Result<KillSwitchReceipt, EngineError> {
    let reason = reason
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| "pulled from the UI, no reason given".to_string());
    Ok(engine.trigger_kill_switch(&reason, "ui"))
}

/// Halts the engine, stops managing every position with money on chain, sells
/// what it can, and returns the list of what is left out there.
///
/// **Nothing is sold on this build.** Selling needs an execution backend and
/// `run` installs none — see `execution.rs` and the roadmap's Phase 4 gate — so
/// `exitsSent` comes back zero and every entry in `stranded` is still a
/// position somebody has to flatten by hand.
///
/// `exitsSent` is the field to branch on, and it counts transactions
/// **dispatched**, not positions closed. A position stays in `stranded` until
/// its exit confirms; `exitsConfirmed` and `flattened` are what say it is
/// actually gone. A caller that reads `exitsSent` and renders "positions
/// closed" is reporting a lie about money.
///
/// Infallible from the UI's point of view for the same reason the kill switch
/// is: whoever pressed this is having a bad minute and needs a receipt, not an
/// error to interpret.
/// `intentIds` is the window's selection: the obligations it has reconciled and
/// has not already sent. Omitting it means every open obligation, which is the
/// panic-button reading rather than the operator's.
#[tauri::command]
fn trigger_emergency_unwind(
    engine: State<'_, Arc<Engine>>,
    intent_ids: Option<Vec<String>>,
    reason: Option<String>,
) -> Result<UnwindReceipt, EngineError> {
    let reason = reason
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| "emergency unwind from the UI, no reason given".to_string());
    Ok(engine.emergency_unwind(intent_ids.as_deref(), &reason, "ui"))
}

/// Opens a live telemetry stream into the calling window.
///
/// The UI passes a channel and events arrive on it until the window closes;
/// there is nothing to poll and nothing to unsubscribe. A closed window fails
/// its next send and is dropped by the hub on the spot.
#[tauri::command]
fn stream_telemetry(
    engine: State<'_, Arc<Engine>>,
    on_event: Channel<TelemetryEvent>,
) -> Result<TelemetrySubscription, EngineError> {
    engine.subscribe_telemetry(on_event)
}

/// What the ingestion layer has seen: counters, rates, and the health of every
/// configured endpoint.
///
/// Free of side effects, so the window may poll it as fast as it repaints.
#[tauri::command]
fn get_ingestion_metrics(ingestion: State<'_, Arc<IngestionManager>>) -> IngestionSnapshot {
    ingestion.snapshot()
}

/// What the Geyser feed has seen: the stream counters, the slot ledger's heads,
/// and the sub-slot ring's own account of what ordering cost.
///
/// Free of side effects for the same reason `get_ingestion_metrics` is. Every
/// field is an integer, including the ring's, so the pane that shows it is a
/// column of digits and nothing a reader has to interpret.
///
/// On a build with no endpoint configured this is every counter at zero, which
/// is the honest reading of a feed that was never opened.
#[tauri::command]
fn get_geyser_metrics(geyser: State<'_, Arc<GeyserFeed>>) -> GeyserSnapshot {
    geyser.snapshot()
}

/// Every number the engine keeps about itself: tick latency, what the feed
/// cost, and where the executions are.
///
/// Reads atomics and nothing else — no lock, no database, no side effect — so
/// the window may poll it as fast as it repaints without touching the engine's
/// timing. The same snapshot is what the HTTP exporter serves.
#[tauri::command]
fn get_metrics(metrics: State<'_, Arc<MetricsCollector>>) -> MetricsSnapshot {
    metrics.snapshot()
}

/// What the bundle deck is doing: the tip floor and its working, how bundles
/// are ending, where their time goes, and what tips actually cost.
///
/// Free of side effects, so the window may poll it as fast as it repaints. The
/// deck's lock is held for the length of the snapshot and nothing under it does
/// IO, so this cannot be the call a repaint waits on.
///
/// The leader hint comes from the schedule the execution side holds, which in
/// this build is [`UnknownLeaderSchedule`](crate::execution::UnknownLeaderSchedule)
/// and answers `Unknown` to everything. That is why `floor.proximityMicros`
/// renders as "unknown" rather than as a number: no proximity has been
/// measured, and the cockpit says so instead of showing a zero that would read
/// as "no leader near".
#[tauri::command]
fn get_bundle_telemetry(bundles: State<'_, Arc<BundleDeck>>) -> BundleTelemetry {
    bundles.telemetry(LeaderHint::Unknown)
}

/// Tells ingestion what SOL is worth, in whole US cents.
///
/// Every market cap threshold is written in dollars and every chain number
/// arrives in lamports, so without this the two never meet and every candidate
/// reads as too small to trade. That is the safe direction to fail in, which is
/// why it is the starting state rather than a guess.
#[tauri::command]
fn set_sol_price(
    ingestion: State<'_, Arc<IngestionManager>>,
    cents_per_sol: u64,
) -> Result<SolPrice, EngineError> {
    if cents_per_sol == 0 {
        return Err(EngineError::Ingestion(
            "a SOL price of zero would make every candidate look too small to trade".to_string(),
        ));
    }
    let price = SolPrice::from_usd_cents(cents_per_sol);
    ingestion.set_sol_price(price);
    Ok(price)
}

/// What is driving the numbers in this window: the live feeds, or a recording.
///
/// Free of side effects, so the window may poll it as fast as it repaints. It
/// answers before any fixture has been opened too — `active: false` with a
/// `streamId` of `None` is a real answer and means nobody has asked for
/// playback, which is not the same as this build having no replay control.
#[tauri::command]
fn get_replay_status(replay: State<'_, Arc<ReplaySession>>) -> ReplayStatus {
    replay.status()
}

/// Starts or stops fixture playback, and sets the multiplier on the way past.
///
/// One command for both halves because the window sends one field at a time —
/// the switch sends `active`, a speed chip sends `speed` — and two commands
/// that can disagree about whether replay is on is a control surface that
/// eventually will.
///
/// **It refuses to enter replay while a feed endpoint is connected.** The bar
/// this drives tells the operator that nothing below it is live, and on this
/// build that sentence is only true because nothing is arriving: §5's
/// `FixtureDialer` — the seam that would put fixture frames through ingestion
/// instead of the sockets — is not built. Starting a fixture over a connected
/// feed would leave live candidates filling the panes under a bar saying they
/// were recorded, which is the same class of mistake as reporting a position
/// closed that is still open.
///
/// Returns the session's own answer rather than the request, because the window
/// draws the switch and the pressed chip from what comes back.
#[tauri::command]
fn set_replay_playback(
    replay: State<'_, Arc<ReplaySession>>,
    ingestion: State<'_, Arc<IngestionManager>>,
    geyser: State<'_, Arc<GeyserFeed>>,
    active: Option<bool>,
    speed: Option<ReplaySpeed>,
) -> Result<ReplayStatus, EngineError> {
    if let Some(speed) = speed {
        replay.set_speed(speed);
    }

    match active {
        Some(true) => {
            set_replay_transport(replay, ingestion, geyser, ReplayControl::Play, None, None)
        }
        Some(false) => Ok(replay.stop()),
        // Neither field: a speed change on its own, already applied above.
        None => Ok(replay.status()),
    }
}

/// Every other press on the transport: pause, resume, step, fast-forward.
///
/// One command rather than four, for the reason `set_replay_playback` gives
/// about the switch and the multiplier. Four commands that can each put a
/// fixture behind the clock are four places the live-feed guard has to be
/// remembered, and the one it is forgotten in is the one that raises the bar
/// over live candidates.
///
/// `records` is what `step` and `fastForward` spend. A step with no count is
/// one record; a fast-forward with no count plays every record that is left,
/// which is how a whole fixture is run and priced in one call.
///
/// `speed` is applied first and independently, so a window may send a chip and
/// a button in one message and get one answer back.
#[tauri::command]
fn set_replay_transport(
    replay: State<'_, Arc<ReplaySession>>,
    ingestion: State<'_, Arc<IngestionManager>>,
    geyser: State<'_, Arc<GeyserFeed>>,
    control: ReplayControl,
    records: Option<u64>,
    speed: Option<ReplaySpeed>,
) -> Result<ReplayStatus, EngineError> {
    if let Some(speed) = speed {
        replay.set_speed(speed);
    }
    // Pausing and stopping take a fixture *off* the clock, or leave it exactly
    // where it is. Neither can put live candidates under the bar, and gating
    // them on the feeds would mean a connected feed could trap a session in
    // replay — which is the failure this guard exists to prevent, backwards.
    if !matches!(control, ReplayControl::Pause | ReplayControl::Stop) {
        refuse_over_a_live_feed(&ingestion, &geyser)?;
    }
    Ok(replay.control(control, records)?)
}

/// Refuses to put a fixture behind the clock while a feed endpoint is up.
///
/// The one check, in the one place every entry into replay goes through. The
/// bar this protects tells the operator that nothing below it is live, and on
/// this build that sentence is only true because nothing is arriving: §5's
/// `FixtureDialer` — the seam that would put fixture frames through ingestion
/// instead of the sockets — is not built. Starting a fixture over a connected
/// feed would leave live candidates filling the panes under a bar saying they
/// were recorded, which is the same class of mistake as reporting a position
/// closed that is still open.
///
/// **Both** feeds are checked, and the second one is the reason this takes two
/// arguments. The Geyser subscription is a separate socket that produces into
/// the same candidate channels, so a guard that only knew about the websocket
/// endpoints would raise the bar over live candidates the moment somebody set
/// `$STS_GEYSER_ENDPOINT` — the exact failure it exists to prevent, reached by
/// a route nobody changed.
///
/// Public so it can be tested against a manager with a socket actually open. A
/// guard nothing exercises is a guard that stops working quietly, and this is
/// the one that decides whether the bar over the panes is telling the truth.
pub fn refuse_over_a_live_feed(
    ingestion: &IngestionManager,
    geyser: &GeyserFeed,
) -> Result<(), EngineError> {
    let live = ingestion
        .snapshot()
        .endpoints
        .iter()
        .find(|endpoint| endpoint.connected)
        .map(|endpoint| endpoint.provider.as_str().to_string())
        .or_else(|| {
            geyser
                .is_connected()
                .then(|| format!("the {} geyser stream", geyser.provider().as_str()))
        });

    match live {
        Some(name) => Err(EngineError::Replay(format!(
            "{name} is still connected. This build replays a fixture into the clock and the \
             cockpit, not into ingestion, so live candidates would keep filling the panes \
             under a bar saying they were recorded. Stop the feeds first.",
        ))),
        None => Ok(()),
    }
}

/// The multiplier on its own.
///
/// The narrow half of `set_replay_playback`, for a caller that wants to change
/// how fast a fixture is playing and must not be able to start or stop one by
/// getting a field name wrong. Infallible: an unusable multiplier is refused by
/// the argument type before this is reached, and every value that gets here is
/// one of the four the cockpit offers.
#[tauri::command]
fn set_replay_speed(replay: State<'_, Arc<ReplaySession>>, speed: ReplaySpeed) -> ReplayStatus {
    replay.set_speed(speed)
}

/// Clusters one launch's opening buyers by where their money came from.
///
/// The graph travels in the message rather than being held in state, so the
/// same request always produces the same report — see [`ClusterRequest`] for
/// why that is the point rather than an inconvenience. The result is stored
/// under its mint and announced on telemetry before it is returned, so a window
/// that asked for it and a window that was only listening learn about it
/// together.
///
/// This does no chain lookups of its own. Assembling the transfers is the
/// caller's job, and keeping it that way is what lets a recorded launch be
/// re-analysed months later against a policy that has since changed.
#[tauri::command]
fn analyse_wallet_clusters(
    registry: State<'_, Arc<ClusterRegistry>>,
    request: ClusterRequest,
) -> Result<ClusterGraphReport, EngineError> {
    let report = request.analyse()?;
    registry.record(report.clone());
    Ok(report)
}

/// The stored report for one mint, or `None` if nothing has been analysed for
/// it.
///
/// Free of side effects, so the window may poll it as fast as it repaints.
/// `None` is a real answer and means nobody has run the analysis — which is not
/// the same as the analysis having found nothing, a distinction the report
/// itself carries in its UNKNOWN columns.
#[tauri::command]
fn get_cluster_report(
    registry: State<'_, Arc<ClusterRegistry>>,
    mint: String,
) -> Option<ClusterGraphReport> {
    registry.report(&mint)
}

/// Every stored report as a row, most recently analysed first.
#[tauri::command]
fn list_cluster_reports(registry: State<'_, Arc<ClusterRegistry>>) -> Vec<ClusterSummary> {
    registry.summaries()
}

/// One wallet's funding trail, without clustering anything.
///
/// The query behind live trail tracking: cheap enough to run while an operator
/// is reading a launch, and answering the one question that decides whether the
/// address in front of them is somebody's twelfth keypair.
///
/// Returns the trail *and* what the chain said about the edges under it, when
/// the request carried a witness. The two travel together because they are one
/// answer: a trail whose edges nothing corroborates is a different finding from
/// the same trail confirmed, and a window handed only the first would have no
/// way to tell them apart.
#[tauri::command]
fn trace_wallet_funding(request: TraceRequest) -> Result<WalletTraceReport, EngineError> {
    request.run()
}

/// Everything one verification needs, in one message.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct VerifyRequest {
    edges: Vec<TraceEdge>,
    #[serde(default)]
    witness: Vec<Attestation>,
    #[serde(default)]
    policy: Option<VerificationPolicy>,
}

/// Checks asserted funding edges against what providers say about them.
///
/// Separate from the analysis rather than only folded into it, because "is this
/// graph real" is a question an operator asks *about a graph* — before running
/// anything on it, or after a report has already said something alarming and
/// the next question is whether the evidence under it survives. Free of side
/// effects and holding no state: same edges, same attestations, same bytes.
#[tauri::command]
fn verify_lineage(request: VerifyRequest) -> Result<LineageProof, EngineError> {
    let policy = request.policy.unwrap_or_default();
    crate::chainproof::verify_request(&request.edges, &request.witness, &policy)
        .map(|verified| verified.proof)
}

/// The trade journal, filtered.
///
/// Reads and nothing else, so the window may ask as often as the operator
/// types. The filter arrives as whatever fields were set and the rest default
/// to "do not filter"; the page size is clamped by `journal::MAX_LIMIT` however
/// large the ask, because a window that asks for everything has not asked a
/// question.
#[tauri::command]
fn query_journal(
    engine: State<'_, Arc<Engine>>,
    filter: Option<JournalFilter>,
) -> Result<Vec<TradeRow>, EngineError> {
    engine.database().query_journal(&filter.unwrap_or_default())
}

/// What that same filter adds up to.
///
/// Separate from the page rather than folded into it, because the totals are of
/// the filter and the page is fifty rows of it: an operator looking at the
/// first page of nine hundred losses wants the nine hundred.
#[tauri::command]
fn journal_totals(
    engine: State<'_, Arc<Engine>>,
    filter: Option<JournalFilter>,
) -> Result<JournalTotals, EngineError> {
    engine
        .database()
        .journal_totals(&filter.unwrap_or_default())
}

/// One trade with its fills, routes, tips and signatures. `None` for a trade
/// nothing recorded, which is not an error — a window can ask about a row that
/// has since been pruned.
#[tauri::command]
fn journal_trade_detail(
    engine: State<'_, Arc<Engine>>,
    trade_id: String,
) -> Result<Option<TradeDetail>, EngineError> {
    engine.database().journal_trade_detail(&trade_id)
}

/// The forensic log, filtered.
///
/// The other half of `query_journal`. That one answers "what did this trade
/// cost"; this one answers "why were there only four of them", which is the
/// question a quiet week actually raises. The mode is required rather than
/// optional because the revisions each mode is ordered by are three independent
/// sequences, and a page mixing them is ordered by nothing.
#[tauri::command]
fn query_state_log(
    engine: State<'_, Arc<Engine>>,
    filter: StateLogFilter,
) -> Result<Vec<StateRow>, EngineError> {
    engine.database().query_state_log(&filter)
}

/// The same filter, counted rather than listed.
///
/// One entry per gate reason, in a fixed order, with a zero for a reason nobody
/// hit — the shape `daemon::Funnel` prints, so a live funnel and a backtest
/// funnel can be read side by side.
#[tauri::command]
fn state_funnel(
    engine: State<'_, Arc<Engine>>,
    filter: StateLogFilter,
) -> Result<StateFunnel, EngineError> {
    engine.database().state_funnel(&filter)
}

/// Every checkpoint of the book in one mode, oldest first.
#[tauri::command]
fn journal_snapshots(
    engine: State<'_, Arc<Engine>>,
    mode: ExecutionMode,
) -> Result<Vec<SnapshotRow>, EngineError> {
    engine.database().journal_snapshots(mode)
}

/// Walks the checkpoint chain and reports every link that did not verify.
///
/// A read, not a repair. An intact chain is the ordinary answer and a broken
/// one is an incident: it means a row in `journal_snapshots` is not the row
/// that was written, which no code path in this build can do.
#[tauri::command]
fn verify_journal_chain(
    engine: State<'_, Arc<Engine>>,
    mode: ExecutionMode,
) -> Result<ChainReport, EngineError> {
    engine.database().verify_journal_snapshot_chain(mode)
}

/// What the newest checkpoint is worth right now, and how much of the log sits
/// on top of it.
#[tauri::command]
fn journal_warm_start(
    engine: State<'_, Arc<Engine>>,
    mode: ExecutionMode,
) -> Result<WarmStart, EngineError> {
    engine.database().warm_start(mode)
}

/// What the alerting engine has done, and the thresholds it is doing it at.
#[tauri::command]
fn get_alert_status(alerting: State<'_, Arc<AlertDispatcher>>) -> AlertSnapshot {
    alerting.snapshot()
}

/// Moves the lines.
///
/// Validated before it is applied, so a set where the critical threshold sits
/// under the warning one is refused at the command rather than discovered later
/// by an alert level that never fires. The cooldown history is deliberately not
/// cleared: a threshold that was just lowered starts firing on the next
/// observation past it rather than re-announcing everything it has already
/// said.
#[tauri::command]
fn set_alert_thresholds(
    alerting: State<'_, Arc<AlertDispatcher>>,
    thresholds: AlertThresholds,
) -> Result<AlertSnapshot, EngineError> {
    alerting
        .set_thresholds(thresholds)
        .map_err(|err| EngineError::Telemetry(err.to_string()))?;
    Ok(alerting.snapshot())
}

/// Opens the alert feed into the calling window.
///
/// The same shape as `stream_telemetry` and a separate stream on purpose: every
/// alert also goes to the hub, so a window can have one or both, and a pane
/// that only wants the things somebody has to act on does not have to filter a
/// firehose to find them.
#[tauri::command]
fn stream_alerts(
    alerting: State<'_, Arc<AlertDispatcher>>,
    on_alert: Channel<Alert>,
) -> AlertSubscription {
    alerting.subscribe(on_alert)
}

/// What the Geyser feed is doing, as counters.
///
/// Free of side effects — a lock taken over a struct copy of twenty integers —
/// so the window may ask as fast as it repaints.
///
/// **Every field is an integer and none of them is derived.** `geyser.rs` says
/// why in its own words: a derived number in a snapshot is one the reader
/// cannot check against the counter it came from. So this reports the chain
/// head, the confirmed head and the finalized head, and the two numbers the
/// cockpit's sub-slot view is named for are differences the *window* takes —
/// slot drift is `headSlot - confirmedHead`, finality lag is
/// `confirmedHead - finalizedHead`.
///
/// **On this build it answers honest zeroes.** Nothing dials a Geyser endpoint
/// yet: the pipeline is compiled in and idle. That is a real answer and the
/// window renders it as one — "this build has no Geyser stream" rather than a
/// grid of zeroes, because a grid of zeroes is a claim that the feed is
/// perfectly steady.
#[tauri::command]
fn get_geyser_telemetry(geyser: State<'_, Arc<GeyserMonitor>>) -> GeyserSnapshot {
    geyser.snapshot()
}

/// Publishes one batch of released ticks, and folds the batch into the snapshot.
///
/// The seam between the pipeline and the cockpit, and the only place either of
/// the two things the window needs is produced. Whoever ends up driving a
/// Geyser stream calls this after each `ingest`; nothing else has to know what
/// the window reads.
///
/// **The `TickKey` is the payload, and `micros` is the reason.** The snapshot
/// above is counters by design, and a counter cannot carry the offset of an
/// arrival *within* its slot — which is the only thing sub-slot jitter can be
/// computed from. So each released tick goes out as its own line under
/// `source: "geyser"` with its key intact, and the window measures the wobble
/// from the gaps between them, against the feed's own cadence rather than an
/// assumed four hundred milliseconds. `metrics.rs` gives that argument for the
/// engine's tick and it is the same argument here: the chain's cadence moves,
/// and a wobble measured against a constant is mostly a measurement of the
/// constant.
///
/// Rate limiting is deliberately *not* done here. A released tick is one
/// observation and the ring is what bounds them; the hub already drops rather
/// than blocking when a window falls behind, and dropping a sample from a
/// jitter series is cheaper than holding the stream up to keep it.
pub fn publish_geyser_ticks(
    hub: &TelemetryHub,
    monitor: &GeyserMonitor,
    pipeline: &TickPipeline,
    released: &[TickEvent],
    stale_writes: u64,
) {
    monitor.observe(pipeline, stale_writes);
    if released.is_empty() {
        return;
    }

    let snapshot = monitor.snapshot();
    for event in released {
        hub.publish(
            TelemetryLevel::Debug,
            "geyser",
            "tick",
            serde_json::json!({
                "key": event.key,
                "snapshot": snapshot,
            }),
        );
    }
}

/// Arms the kill switch on any panic, on any thread, before the default hook
/// prints it.
///
/// A process that has panicked has, by definition, reached a state nobody wrote
/// down what to do about. Halting is the only safe reading of that, and it has
/// to happen here rather than in a catch-and-recover further up, because most
/// panics will be on threads nobody is joining.
fn install_panic_hook(engine: Arc<Engine>) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());

        // `PanicHookInfo::payload` is whatever was passed to `panic!`, which is
        // a `&str` for a literal and a `String` for a formatted message. Anything
        // else is some other type entirely and there is nothing to print.
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic with a non-string payload".to_string());

        engine.arm_from_panic(&location, &message);

        // The default hook last, so the panic still reaches stderr looking the
        // way a Rust panic is supposed to look.
        previous(info);
    }));
}

/// Drains the ingestion channels into telemetry.
///
/// A stand-in for the scoring engine, which is the phase after this one. It
/// exists because an unread channel is a full channel, and a full channel turns
/// every candidate into a counted drop — which would read as ingestion failing
/// when in fact nothing downstream had been built yet. Until there is something
/// to score with, a candidate reaching the window as a telemetry line is the
/// whole of shadow mode: watch, write it down, sign nothing.
pub(crate) fn spawn_candidate_observer(
    streams: IngestionStreams,
    hub: Arc<TelemetryHub>,
    metrics: Arc<MetricsCollector>,
) {
    let IngestionStreams {
        mut fast_path,
        mut standard,
    } = streams;
    tauri::async_runtime::spawn(async move {
        // The slot the last tick was recorded against. Several candidates can
        // arrive for one slot — three providers watching the same program is
        // the normal case — and counting each of them as a tick would report a
        // clock running three times too fast with no gaps between its beats.
        // The tick is the slot advancing.
        let mut ticked_slot = 0u64;
        loop {
            // Biased towards the fast path: it is the shallower queue and the
            // one whose contents go stale, so it is drained first when both have
            // something waiting. The standard queue only waits while the fast
            // path has something ready, and its own drops are counted, so the
            // worst this bias costs is visible in the numbers.
            let (event, level) = tokio::select! {
                biased;
                Some(event) = fast_path.recv() => (event, TelemetryLevel::Info),
                Some(event) = standard.recv() => (event, TelemetryLevel::Debug),
                else => return,
            };

            // Timed around the work this loop does for one candidate, which is
            // the whole of what the engine currently does with a slot. When
            // there is a scoring pass it goes inside this bracket too, and the
            // number keeps meaning the same thing: how long the tick took.
            let started = Instant::now();
            let route = match event.route {
                Route::FastPath => "fast path",
                Route::Standard => "standard",
            };
            hub.publish(
                level,
                "ingestion",
                format!("{route} candidate at ${}", event.market_cap_usd_cents / 100),
                serde_json::to_value(event).unwrap_or_else(
                    |_| serde_json::json!({ "error": "candidate would not serialise" }),
                ),
            );
            let handled = started.elapsed();

            // Whichever queue is fuller in proportion to its own depth is the
            // one that describes the pressure. Compared by cross-multiplying so
            // the two ratios can be ranked without dividing, and so a shallow
            // fast path at half capacity outranks a deep standard queue holding
            // more items but a smaller share of its room.
            let (depth, capacity) =
                if fast_path.len() * STANDARD_DEPTH >= standard.len() * FAST_PATH_DEPTH {
                    (fast_path.len(), FAST_PATH_DEPTH)
                } else {
                    (standard.len(), STANDARD_DEPTH)
                };
            metrics.observe_queue(depth, capacity);

            if event.view.slot > ticked_slot {
                ticked_slot = event.view.slot;
                metrics.record_slot_tick(event.view.slot, handled);
            }
        }
    });
}

/// Advances the replay session and publishes what it did.
///
/// The session itself is synchronous and knows nothing about tokio or
/// telemetry. This is the one place the two are joined, for the same reason
/// `spawn_candidate_observer` above is: the module that owns the rule should
/// not also own the scheduler it happens to run on.
///
/// The tick runs whether or not anybody has pressed anything — `advance`
/// returns immediately when playback is off — and it lives as long as the
/// runtime does. There is nothing to stop on the way out because there is
/// nothing it holds: the fixture is in memory, the clock is virtual, and
/// dropping the task mid-sleep loses nothing that was not already written down.
fn spawn_replay_ticker(replay: Arc<ReplaySession>, hub: Arc<TelemetryHub>) {
    tauri::async_runtime::spawn(async move {
        // Seeded with the status as it is now rather than with nothing, so a
        // process that starts and is never asked for replay publishes no lines
        // about it at all.
        let mut last = replay.status();
        let mut published_at = tokio::time::Instant::now();

        loop {
            tokio::time::sleep(REPLAY_TICK).await;
            let status = replay.advance(REPLAY_TICK.as_millis() as u64);
            if status == last {
                continue;
            }

            // A change to the transport, the multiplier or the fixture goes out
            // on the tick it happened: those are the facts a window is wrong
            // about until it hears them. A playhead that has only moved is the
            // same run saying the same thing, and it waits for the interval.
            //
            // `state` rather than `active` because pausing and ending are both
            // presses an operator made and neither changes the flag — a pause
            // that reached the window a second late would look like a button
            // that did not work.
            let same_run = status.state == last.state
                && status.speed == last.speed
                && status.stream_id == last.stream_id
                && status.chain_head == last.chain_head
                && status.chain_verified == last.chain_verified
                && status.fixture_complete == last.fixture_complete;
            if same_run && published_at.elapsed() < REPLAY_TELEMETRY_INTERVAL {
                continue;
            }

            let message = match (&status.stream_id, status.state) {
                (Some(stream), PlaybackState::Playing) => format!(
                    "replaying {stream} at {} · {} / {} records, slot {}",
                    status.speed, status.records_played, status.record_count, status.slot
                ),
                (Some(stream), PlaybackState::Paused) => format!(
                    "replay paused on {stream} · {} / {} records, slot {}",
                    status.records_played, status.record_count, status.slot
                ),
                // The end of a fixture is where the ledger is final, so it is
                // the one line that carries it: a run that finished and
                // published nothing but a record count is a run whose result
                // nobody saw go past.
                (Some(stream), PlaybackState::Ended) => format!(
                    "replay finished {stream} · {} records · {} trades, {} lamports realised, \
                     {} bps slippage, {} lamports tipped",
                    status.record_count,
                    status.ledger.trades,
                    status.ledger.realized_pnl_lamports,
                    status.ledger.slippage_bps,
                    status.ledger.tips_lamports
                ),
                (Some(stream), PlaybackState::Stopped) => format!(
                    "replay stopped on {stream} · {} / {} records",
                    status.records_played, status.record_count
                ),
                (None, _) => "replay has no fixture open".to_string(),
            };

            hub.publish(
                TelemetryLevel::Info,
                "replay",
                message,
                serde_json::to_value(&status).unwrap_or_else(
                    |_| serde_json::json!({ "error": "the replay status would not serialise" }),
                ),
            );
            last = status;
            published_at = tokio::time::Instant::now();
        }
    });
}

/// Copies ingestion's own counters into the metrics collector.
///
/// The drops are counted where they happen, deep inside the ingest path, and
/// that path has no collector and should not grow one — a frame is refused in
/// the middle of a socket read, and threading a second set of counters through
/// there would put work on the hottest path in the process to say the work was
/// slow. So the totals are copied out instead, at an interval, from the same
/// snapshot the window already reads.
///
/// The copy is deltas rather than absolutes because `record_dropped` counts up
/// and ingestion's counters are already cumulative. Both sides only ever go up,
/// so a missed interval loses nothing: the next one carries it.
pub(crate) fn spawn_feed_bridge(
    ingestion: Arc<IngestionManager>,
    metrics: Arc<MetricsCollector>,
    engine: Arc<Engine>,
) {
    tauri::async_runtime::spawn(async move {
        let mut previous = FeedTotals::default();
        loop {
            tokio::time::sleep(FEED_BRIDGE_INTERVAL).await;
            let current = FeedTotals::of(&ingestion.snapshot());
            current.record_since(previous, &metrics);
            previous = current;
            // Checked after the copy rather than before it, so the last window
            // — the one the shutdown happened in — is in the numbers. A final
            // scrape that stops two seconds short of the end would be missing
            // exactly the part somebody is reading it for.
            if engine.is_shutting_down() {
                return;
            }
        }
    });
}

/// Ingestion's counters, grouped the way the metrics collector names them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FeedTotals {
    delivered: u64,
    backpressure: u64,
    undecodable: u64,
    stale: u64,
    filtered: u64,
    persistence: u64,
}

impl FeedTotals {
    fn of(snapshot: &IngestionSnapshot) -> Self {
        // `candidates` counts every attempt to hand one downstream, and the two
        // dropped counters are the attempts that did not fit. What is left is
        // what something is actually holding.
        let backpressure = snapshot
            .dropped_fast_path
            .saturating_add(snapshot.dropped_standard);
        Self {
            delivered: snapshot.candidates.saturating_sub(backpressure),
            backpressure,
            undecodable: snapshot.parse_failures,
            stale: snapshot.stale,
            // Refused on raw bytes and refused after parsing are the same kind
            // of answer — no — and separating them here would only split one
            // healthy number in two.
            filtered: snapshot.prefiltered.saturating_add(snapshot.filtered),
            persistence: snapshot.dropped_wal,
        }
    }

    fn record_since(&self, previous: Self, metrics: &MetricsCollector) {
        metrics.record_ingested(self.delivered.saturating_sub(previous.delivered));
        for (reason, now, before) in [
            (
                DropReason::Backpressure,
                self.backpressure,
                previous.backpressure,
            ),
            (
                DropReason::Undecodable,
                self.undecodable,
                previous.undecodable,
            ),
            (DropReason::Stale, self.stale, previous.stale),
            (DropReason::Filtered, self.filtered, previous.filtered),
            (
                DropReason::Persistence,
                self.persistence,
                previous.persistence,
            ),
        ] {
            metrics.record_dropped(reason, now.saturating_sub(before));
        }
    }
}

/// Opens the metrics port, if anybody asked for one.
///
/// Unset means no socket at all, which is the default. Anything that goes wrong
/// — an address that will not parse, a port already taken, an address that is
/// not this machine — is reported to telemetry and the engine carries on
/// without an exporter. Monitoring is worth having; it is not worth refusing to
/// start a trading engine over.
fn start_metrics_exporter(
    collector: Arc<MetricsCollector>,
    hub: &TelemetryHub,
) -> Option<MetricsExporter> {
    let addr = match addr_from_env() {
        Ok(Some(addr)) => addr,
        Ok(None) => return None,
        Err(err) => {
            hub.publish(
                TelemetryLevel::Warn,
                "metrics",
                format!("the metrics exporter did not start: {err}"),
                serde_json::json!({ "listening": false }),
            );
            return None;
        }
    };

    match BoundExporter::bind(addr) {
        Ok(bound) => {
            let addr = bound.addr();
            hub.publish(
                TelemetryLevel::Info,
                "metrics",
                format!(
                    "metrics on http://{addr}/metrics — json, or prometheus text at /metrics.prom"
                ),
                serde_json::json!({
                    "listening": true,
                    "addr": addr.to_string(),
                    "formats": ["json", "prometheus"],
                }),
            );
            Some(bound.serve(collector))
        }
        Err(err) => {
            hub.publish(
                TelemetryLevel::Warn,
                "metrics",
                format!("the metrics exporter did not start: {err}"),
                serde_json::json!({ "listening": false }),
            );
            None
        }
    }
}

/// Builds the runtime, the engine and the window, then runs until the window
/// closes.
pub fn run() {
    // Tauri would build its own single-threaded runtime by default. The engine
    // is expected to hold long-running work — a socket, a scoring pass — so it
    // gets a multi-threaded one, installed before anything can spawn onto it.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("sts-worker")
        .build()
        .expect("the tokio runtime is the engine's only scheduler; STS cannot start without one");
    tauri::async_runtime::set(runtime.handle().clone());

    let path = database_path();
    let database = match Database::open(&path) {
        Ok(database) => database,
        Err(err) => {
            // Before the window exists there is nowhere to show this but the
            // terminal, and carrying on without a database would only move the
            // same failure somewhere harder to read.
            eprintln!("STS could not open {}: {err}", path.display());
            std::process::exit(1);
        }
    };

    let engine = Arc::new(Engine::start(database));
    install_panic_hook(Arc::clone(&engine));

    // Before anything writes: does the book still match the checkpoints the
    // last process left behind. Checked once, up front, rather than the first
    // time an operator happens to open a pane, and nothing here blocks startup
    // — a broken chain is a finding, not a reason to refuse to show it.
    //
    // The `audit_log` row it writes is the half that survives. `subscribe`
    // hands a new listener events from the sequence it joined at, so the
    // telemetry line published here is gone before any window exists to see it;
    // the window's route to this is the `journal_warm_start` command, which
    // asks the file rather than the stream and can be asked at any time.
    crate::forensics::verify_on_start(
        &engine.database(),
        &engine.telemetry(),
        crate::telemetry::now_ms(),
    );

    // Attached before anything can produce an execution to count, so the very
    // first intent of the run lands in the same counters as the last one.
    let metrics = Arc::new(MetricsCollector::new());
    engine.attach_metrics(Arc::clone(&metrics));

    // Spawning needs a runtime in scope, and the guard has to outlive the
    // `start` call rather than the statement it is on.
    let guard = runtime.enter();
    let (ingestion, streams) = IngestionManager::start(
        IngestionConfig::from_env(),
        Arc::new(WebSocketDialer),
        Some(engine.database()),
        Some(engine.telemetry()),
    );
    spawn_candidate_observer(streams, engine.telemetry(), Arc::clone(&metrics));
    spawn_feed_bridge(
        Arc::clone(&ingestion),
        Arc::clone(&metrics),
        Arc::clone(&engine),
    );
    // The Geyser feed is the second producer into the same candidate channels,
    // and the only one that arrives in chain order. It is started after
    // ingestion because it needs the manager to hand candidates to, and it
    // starts nothing at all unless `$STS_GEYSER_ENDPOINT` says to — the same
    // deliberate-act rule the websocket endpoints follow.
    let geyser_feed = Arc::new(GeyserFeed::start(
        GeyserConfig::from_env(),
        geyser::default_transport(),
        Arc::clone(&ingestion),
        Some(engine.telemetry()),
    ));
    let exporter = start_metrics_exporter(Arc::clone(&metrics), &engine.telemetry());
    // The session streams records; the runner prices them. Attached here rather
    // than inside `ReplaySession` because what a frame is worth is a question
    // about this application's strategy, and `replay` is the module that has to
    // stay true of any fixture whatever anybody is trading against it.
    let replay = Arc::new(
        ReplaySession::new(fixture_dir()).observing(PaperRunner::new(BacktestConfig::default())),
    );
    // Nothing feeds this yet: no backend in this build submits a bundle, and no
    // reporter observes a slot. It is constructed at start-up anyway so the
    // cockpit's deck has a real snapshot to render from the first repaint —
    // an unfitted floor at the static base, and every counter honestly zero —
    // rather than a panel that appears when a live backend does.
    let bundles = Arc::new(BundleDeck::default());
    // Nothing dials a Geyser endpoint on this build, so this stays at the
    // snapshot it was constructed with. Built at start-up anyway, for the same
    // reason the deck above it is: the sub-slot view has something real to draw
    // from the first repaint rather than appearing when a backend does.
    let geyser_monitor = Arc::new(GeyserMonitor::new());
    spawn_replay_ticker(Arc::clone(&replay), engine.telemetry());
    drop(guard);

    // The forensic reports the window can ask for. Given the telemetry hub so
    // that a cluster worth shouting about reaches every listener, not only the
    // window that happened to request the analysis.
    let clusters = Arc::new(ClusterRegistry::with_telemetry(
        engine.telemetry(),
        ClusteringParams::default().alert_score_micros,
    ));

    // The alerting dispatcher publishes through the same hub the engine does,
    // so an alert reaches a window that is only listening to telemetry as well
    // as one on the dedicated feed. It is built here rather than inside the
    // engine because what counts as an alert is a question about this
    // application's thresholds, and `engine.rs` has to stay true of a build
    // that sets them differently.
    let alerting = Arc::new(AlertDispatcher::new(engine.telemetry()));
    // And handed straight back, so the exit path holds every fill and every
    // confirmation against these thresholds as it writes them to the book. An
    // engine without this still journals — the book is not optional — it simply
    // has nowhere to say that a fill came in badly.
    engine.attach_alerting(Arc::clone(&alerting));

    let app = tauri::Builder::default()
        .manage(Arc::clone(&engine))
        .manage(Arc::clone(&alerting))
        .manage(Arc::clone(&ingestion))
        .manage(Arc::clone(&geyser_feed))
        .manage(Arc::clone(&replay))
        .manage(Arc::clone(&metrics))
        .manage(Arc::clone(&bundles))
        .manage(Arc::clone(&geyser_monitor))
        .manage(Arc::clone(&clusters))
        .invoke_handler(tauri::generate_handler![
            get_engine_status,
            trigger_kill_switch,
            trigger_emergency_unwind,
            stream_telemetry,
            get_ingestion_metrics,
            get_geyser_metrics,
            get_metrics,
            get_bundle_telemetry,
            set_sol_price,
            get_replay_status,
            set_replay_playback,
            set_replay_speed,
            set_replay_transport,
            analyse_wallet_clusters,
            get_cluster_report,
            list_cluster_reports,
            trace_wallet_funding,
            verify_lineage,
            query_journal,
            journal_totals,
            journal_trade_detail,
            query_state_log,
            state_funnel,
            journal_snapshots,
            verify_journal_chain,
            journal_warm_start,
            get_alert_status,
            set_alert_thresholds,
            stream_alerts,
            get_geyser_telemetry
        ])
        .build(tauri::generate_context!())
        .expect("the window could not be built from tauri.conf.json");

    app.run(move |_handle, event| match event {
        // The window is closing but has not closed. Stop taking new work now,
        // while there is still a runtime to stop it on.
        RunEvent::ExitRequested { .. } => {
            if let Some(exporter) = &exporter {
                exporter.stop();
            }
            ingestion.stop();
            geyser_feed.stop();
            replay.stop();
            engine.begin_shutdown();
        }
        // Last call. Join the pump, checkpoint the database, then let the
        // runtime finish whatever is left within the grace period.
        RunEvent::Exit => {
            if let Some(exporter) = &exporter {
                exporter.stop();
            }
            ingestion.stop();
            geyser_feed.stop();
            // Before the hub goes: a webhook worker still holding a socket
            // when the process exits is a delivery nobody can account for.
            alerting.shutdown();
            engine.begin_shutdown();
            engine.finish_shutdown();
        }
        _ => {}
    });

    runtime.shutdown_timeout(SHUTDOWN_GRACE);
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use crate::geyser::{GeyserConfig, GeyserUpdate, SlotUpdate, UpdatePayload};
    use crate::subslot::SlotPhase;
    use crate::telemetry::{TelemetryEvent, TelemetrySink};

    /// A sink that keeps every line, so a test can read what a window would.
    #[derive(Default)]
    struct Recorder {
        lines: Mutex<Vec<TelemetryEvent>>,
    }

    impl TelemetrySink for Recorder {
        fn deliver(&self, event: &TelemetryEvent) {
            self.lines
                .lock()
                .expect("a poisoned recorder")
                .push(event.clone());
        }
    }

    /// One slot update, which is the cheapest thing that reaches the ledger.
    fn slot_at(slot: u64, micros: u64, phase: SlotPhase) -> GeyserUpdate {
        GeyserUpdate {
            created_at_micros: micros,
            payload: UpdatePayload::Slot(SlotUpdate {
                slot,
                parent: None,
                phase,
            }),
        }
    }

    /// Waits for the pump to have delivered `want` lines, or gives up.
    ///
    /// The hub is a queue and a thread, so a publish is not a delivery. Bounded
    /// rather than open-ended: a test that hangs when the pump breaks is not a
    /// test of the pump.
    fn settle(recorder: &Recorder, want: usize) -> Vec<TelemetryEvent> {
        for _ in 0..200 {
            let lines = recorder.lines.lock().expect("a poisoned recorder");
            if lines.len() >= want {
                return lines.clone();
            }
            drop(lines);
            std::thread::sleep(Duration::from_millis(5));
        }
        recorder.lines.lock().expect("a poisoned recorder").clone()
    }

    #[test]
    fn a_released_tick_reaches_the_window_carrying_the_key_the_jitter_needs() {
        let hub = TelemetryHub::start();
        let recorder = Arc::new(Recorder::default());
        hub.observe(Arc::clone(&recorder) as Arc<dyn TelemetrySink>);

        let monitor = GeyserMonitor::new();
        let mut pipeline = TickPipeline::new(&GeyserConfig {
            endpoint: "https://example.invalid".to_string(),
            token: None,
            commitment: crate::subslot::Commitment::Confirmed,
            ring: crate::subslot::RingConfig {
                capacity: 64,
                hold_slots: 1,
            },
            from_slot: None,
            ..GeyserConfig::default()
        });

        // Three slots, the first two of which fall out of the hold window and
        // are therefore released.
        let mut released = Vec::new();
        for update in [
            slot_at(100, 1_000, SlotPhase::Processed),
            slot_at(101, 401_000, SlotPhase::Processed),
            slot_at(102, 802_000, SlotPhase::Processed),
        ] {
            let ingested = pipeline.ingest(update);
            released.extend(ingested.released);
        }

        assert!(!released.is_empty(), "the hold window let nothing go");
        publish_geyser_ticks(&hub, &monitor, &pipeline, &released, 0);

        let lines = settle(&recorder, released.len());
        let ticks: Vec<_> = lines
            .iter()
            .filter(|line| line.source == "geyser")
            .collect();
        assert_eq!(ticks.len(), released.len(), "one line per released tick");

        // The contract ui/app.js reads: `data.key` is the TickKey, and `micros`
        // is on it. Without that field there is nothing to compute a sub-slot
        // jitter from, and the 0x100 view has no samples.
        let key = &ticks[0].data["key"];
        assert!(key["slot"].is_number(), "no slot on the key: {key}");
        assert!(key["micros"].is_number(), "no micros on the key: {key}");
        assert_eq!(key["slot"].as_u64(), Some(released[0].key.slot));
        assert_eq!(key["micros"].as_u64(), Some(released[0].key.micros));

        // And the snapshot rides along, so a window that has not polled yet
        // still has the heads the drift is taken from.
        let snapshot = &ticks[0].data["snapshot"];
        assert!(snapshot["headSlot"].is_number(), "no headSlot: {snapshot}");
        assert!(snapshot["confirmedHead"].is_number());
        assert!(snapshot["finalizedHead"].is_number());
        assert!(snapshot["ring"]["released"].is_number());

        hub.shutdown();
    }

    #[test]
    fn a_batch_that_released_nothing_still_moves_the_snapshot_and_says_nothing() {
        let hub = TelemetryHub::start();
        let recorder = Arc::new(Recorder::default());
        hub.observe(Arc::clone(&recorder) as Arc<dyn TelemetrySink>);

        let monitor = GeyserMonitor::new();
        let mut pipeline = TickPipeline::new(&GeyserConfig {
            endpoint: "https://example.invalid".to_string(),
            token: None,
            commitment: crate::subslot::Commitment::Confirmed,
            ring: crate::subslot::RingConfig {
                capacity: 64,
                hold_slots: 8,
            },
            from_slot: None,
            ..GeyserConfig::default()
        });

        // One slot, held rather than released: the hold window is wider than
        // the head has moved.
        let ingested = pipeline.ingest(slot_at(100, 1_000, SlotPhase::Processed));
        assert!(
            ingested.released.is_empty(),
            "the hold window let something go"
        );
        publish_geyser_ticks(&hub, &monitor, &pipeline, &ingested.released, 3);

        // The counters moved even though nothing was said, which is the point:
        // a feed that is receiving and holding is not a feed that is quiet, and
        // the window has to be able to tell those apart.
        assert_eq!(monitor.snapshot().head_slot, 100);
        assert_eq!(monitor.snapshot().stale_writes, 3);

        std::thread::sleep(Duration::from_millis(30));
        let lines = recorder.lines.lock().expect("a poisoned recorder");
        assert!(
            lines.iter().all(|line| line.source != "geyser"),
            "a batch with nothing in it published a line anyway",
        );

        drop(lines);
        hub.shutdown();
    }

    fn ingestion_after(
        candidates: u64,
        dropped_fast: u64,
        dropped_standard: u64,
    ) -> IngestionSnapshot {
        IngestionSnapshot {
            candidates,
            dropped_fast_path: dropped_fast,
            dropped_standard,
            ..IngestionSnapshot::default()
        }
    }

    #[test]
    fn what_was_delivered_is_what_was_offered_minus_what_did_not_fit() {
        let totals = FeedTotals::of(&ingestion_after(100, 3, 7));
        assert_eq!(totals.delivered, 90);
        assert_eq!(totals.backpressure, 10);
    }

    #[test]
    fn a_full_queue_never_reads_as_more_delivered_than_offered() {
        // Two counters read a moment apart can disagree: the drops can be read
        // after a candidate that the total had not caught up with yet. The
        // answer is zero delivered, not a number that has wrapped around.
        let totals = FeedTotals::of(&ingestion_after(5, 4, 4));
        assert_eq!(totals.delivered, 0);
    }

    #[test]
    fn every_ingestion_counter_lands_in_exactly_one_reason() {
        let snapshot = IngestionSnapshot {
            candidates: 50,
            dropped_fast_path: 1,
            dropped_standard: 2,
            parse_failures: 3,
            stale: 4,
            prefiltered: 5,
            filtered: 6,
            dropped_wal: 7,
            ..IngestionSnapshot::default()
        };
        let totals = FeedTotals::of(&snapshot);
        assert_eq!(totals.delivered, 47);
        assert_eq!(totals.backpressure, 3);
        assert_eq!(totals.undecodable, 3);
        assert_eq!(totals.stale, 4);
        assert_eq!(
            totals.filtered, 11,
            "refused on bytes and refused after parsing are one answer"
        );
        assert_eq!(totals.persistence, 7);
    }

    #[test]
    fn the_bridge_copies_the_difference_not_the_total() {
        let metrics = MetricsCollector::new();

        let first = FeedTotals::of(&ingestion_after(10, 1, 0));
        first.record_since(FeedTotals::default(), &metrics);
        let after_first = metrics.snapshot().feed;
        assert_eq!(after_first.ingested, 9);
        assert_eq!(after_first.overrun, 1);

        // The same totals again — a window in which nothing happened — must add
        // nothing, or every poll would count the whole run over again.
        first.record_since(first, &metrics);
        assert_eq!(metrics.snapshot().feed.ingested, 9);

        let second = FeedTotals::of(&ingestion_after(30, 4, 0));
        second.record_since(first, &metrics);
        let after_second = metrics.snapshot().feed;
        assert_eq!(after_second.ingested, 26, "nine, then seventeen more");
        assert_eq!(after_second.overrun, 4);
    }

    #[test]
    fn a_missed_interval_costs_nothing_because_both_sides_only_go_up() {
        let straight_through = MetricsCollector::new();
        let stepwise = MetricsCollector::new();

        let steps = [
            FeedTotals::of(&ingestion_after(10, 1, 1)),
            FeedTotals::of(&ingestion_after(40, 2, 3)),
            FeedTotals::of(&ingestion_after(90, 6, 4)),
        ];

        // One poll that saw only the last reading, against three that saw all
        // of them. Both have to end up in the same place.
        steps[2].record_since(FeedTotals::default(), &straight_through);
        let mut previous = FeedTotals::default();
        for step in steps {
            step.record_since(previous, &stepwise);
            previous = step;
        }

        let one = straight_through.snapshot().feed;
        let many = stepwise.snapshot().feed;
        assert_eq!(one.ingested, many.ingested);
        assert_eq!(one.dropped, many.dropped);
        assert_eq!(one.overrun, many.overrun);
    }
}
