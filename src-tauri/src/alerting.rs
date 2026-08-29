//! What the engine should be interrupted about, and who gets told.
//!
//! `telemetry.rs` carries everything the engine says; this carries the small
//! part of it somebody has to act on. The two are deliberately not the same
//! stream. Telemetry is a firehose that a window renders and a file records,
//! and a person watching it for the one line that matters is a person who will
//! miss it. An alert is the opposite shape: rare, thresholded, deduplicated,
//! and delivered somewhere that will wake somebody up.
//!
//! Four things are watched, and they are the four ways this build has been able
//! to lose money quietly.
//!
//! **Slippage.** Every route carries a bound and every fill is measured against
//! the quote it was priced from. A fill inside its route's bound is not an
//! alert however wide the bound was; a fill outside it means the reserves moved
//! between the simulation and the send, and two of those in a row means
//! somebody is trading against the bot.
//!
//! **Tips.** `TipPolicy` caps every bid at `Tip_max`, so a bid past the ceiling
//! should be impossible. It is watched anyway, because "impossible" here means
//! "no code path in this build constructs one", and the failure it would
//! represent — an escalation loop that stopped respecting its bound — spends
//! real lamports on every retry until somebody notices.
//!
//! **Confirmations.** A transaction that went out and has not come back is
//! money whose fate is decided and not yet known, and the interesting number is
//! how long it has been that way. Rebroadcasts are the same fact from the other
//! side: the loop giving up and trying again, repeatedly, is the shape of a
//! network problem that a person needs to be told about rather than a retry
//! policy working.
//!
//! **Wallet clusters.** `strategy::syndicate` finds groups; this watches for
//! one turning up where it was not expected — a cluster taking a large share of
//! a launch the engine is already in, or one whose holdings are concentrated
//! enough that the "crowd" is one wallet wearing hats.
//!
//! # Three rules the dispatcher keeps
//!
//! **Nothing here blocks the engine.** [`AlertDispatcher::observe`] evaluates
//! thresholds — integer comparisons, no allocation until something actually
//! fires — and hands whatever fired to the listeners. A listener that has to
//! talk to the network gets a queue and a thread of its own; see
//! [`WebhookSink`]. This is the same contract `TelemetrySink` states and it is
//! stated again because the consequence is worse here: the moment an alert path
//! can block is the moment a bad minute becomes a stalled engine.
//!
//! **The same alert does not fire twice in a row.** Every alert has a subject —
//! a trade id, or a mint — and a kind, and a cooldown per pair. Without it, one
//! bad launch delivers four hundred slippage alerts and the four hundredth is
//! the one that gets read. Suppressions are counted rather than dropped
//! silently, so the snapshot can say "nine of these, one shown".
//!
//! **Every number in an alert is an integer with a named unit.** Basis points,
//! lamports, milliseconds, a count, millionths. A payload that crossed IPC as a
//! float would arrive at a window that renders it with whatever precision
//! JavaScript felt like, in a message whose whole purpose is to be exact about
//! how far past a threshold something went.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tauri::ipc::Channel;

use crate::db::ExecutionMode;
use crate::journal::{FillRow, SignatureStatus, TipRow};
use crate::telemetry::{TelemetryHub, TelemetryLevel};

// ---------------------------------------------------------------------------
// what an alert is
// ---------------------------------------------------------------------------

/// What fired.
///
/// Six kinds and not one with a reason inside it, because the kind is half of
/// the cooldown key: a slippage spike and a late confirmation on the same trade
/// are two different things to be told about, and folding them into one kind
/// would let the first suppress the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AlertKind {
    /// A fill came in further under its quote than the route's own bound.
    SlippageSpike,
    /// A tip bid went past the ceiling it was priced under.
    TipOverrun,
    /// A transaction has been on the network longer than the budget allows.
    ConfirmationLate,
    /// The same bytes have gone out too many times.
    RebroadcastStorm,
    /// A transaction settled without landing.
    ExitFailed,
    /// A wallet cluster turned up somewhere it was not expected.
    ClusterActivity,
}

impl AlertKind {
    pub const ALL: [AlertKind; 6] = [
        AlertKind::SlippageSpike,
        AlertKind::TipOverrun,
        AlertKind::ConfirmationLate,
        AlertKind::RebroadcastStorm,
        AlertKind::ExitFailed,
        AlertKind::ClusterActivity,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            AlertKind::SlippageSpike => "slippage_spike",
            AlertKind::TipOverrun => "tip_overrun",
            AlertKind::ConfirmationLate => "confirmation_late",
            AlertKind::RebroadcastStorm => "rebroadcast_storm",
            AlertKind::ExitFailed => "exit_failed",
            AlertKind::ClusterActivity => "cluster_activity",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        AlertKind::ALL.into_iter().find(|k| k.as_str() == text)
    }
}

/// How loud.
///
/// Three levels and no `Debug` one: an alert nobody would act on is telemetry,
/// and the whole point of this module is that everything in it is worth
/// reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AlertSeverity {
    /// Worth knowing, not worth waking up for.
    Info,
    /// Past a threshold somebody set.
    Warn,
    /// Past the threshold where the answer is usually "stop".
    Critical,
}

impl AlertSeverity {
    pub const fn as_str(self) -> &'static str {
        match self {
            AlertSeverity::Info => "info",
            AlertSeverity::Warn => "warn",
            AlertSeverity::Critical => "critical",
        }
    }

    /// The telemetry level this rides the hub at, so a window that is only
    /// listening to telemetry still sees it at the right volume.
    pub const fn as_telemetry_level(self) -> TelemetryLevel {
        match self {
            AlertSeverity::Info => TelemetryLevel::Info,
            AlertSeverity::Warn => TelemetryLevel::Warn,
            AlertSeverity::Critical => TelemetryLevel::Error,
        }
    }
}

/// What `observed` and `threshold` are counted in.
///
/// Carried rather than implied by the kind, because the payload crosses IPC and
/// a window that has to switch on the kind to know whether `observed` is
/// milliseconds or lamports is a window that will eventually get it wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AlertUnit {
    BasisPoints,
    Lamports,
    Milliseconds,
    /// A plain count: rebroadcasts, cluster members.
    Count,
    /// Millionths, the unit the strategy module scores in.
    Micros,
}

impl AlertUnit {
    pub const fn as_str(self) -> &'static str {
        match self {
            AlertUnit::BasisPoints => "bps",
            AlertUnit::Lamports => "lamports",
            AlertUnit::Milliseconds => "ms",
            AlertUnit::Count => "count",
            AlertUnit::Micros => "micros",
        }
    }
}

/// One thing somebody has to be told about.
///
/// `Eq`, like everything in `journal.rs` and for the same reason: there is no
/// float in it, so "the alert that fired is the alert that arrived" is one
/// comparison at both ends of the webhook.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Alert {
    /// Monotonic per-dispatcher. A gap means a delivery was dropped, which is
    /// how a sink can tell "quiet" from "behind" — the same trick
    /// `TelemetryEvent::seq` plays.
    pub seq: u64,
    pub at_ms: i64,
    pub kind: AlertKind,
    pub severity: AlertSeverity,
    pub mode: ExecutionMode,
    /// What this is about, and half of the cooldown key. A trade id for
    /// anything with a trade behind it, a mint for a cluster sighting.
    pub subject: String,
    pub mint: Option<String>,
    /// The sentence a person reads.
    pub message: String,
    /// What was measured, and what it was measured against, both in `unit`.
    pub observed: u64,
    pub threshold: u64,
    pub unit: AlertUnit,
}

impl Alert {
    /// How far past the threshold this went, in the same unit. Zero for an
    /// alert that fires on something being *under* one.
    pub const fn overshoot(&self) -> u64 {
        self.observed.saturating_sub(self.threshold)
    }
}

// ---------------------------------------------------------------------------
// where the lines are
// ---------------------------------------------------------------------------

/// What counts as too much.
///
/// Every field is an integer in a unit the rest of the codebase already uses,
/// and the defaults are the numbers this build's policies already imply rather
/// than round figures picked to look reasonable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AlertThresholds {
    /// A fill this far under its quote is a spike even when the route's own
    /// bound was wider. The floor under a permissive route.
    pub slippage_bps: u16,
    /// And this far under is a critical one.
    pub critical_slippage_bps: u16,
    /// How far past `Tip_max` a bid may go before it is an alert. Zero: the
    /// ceiling is the ceiling, and there is no such thing as a little bit over.
    pub tip_grace_lamports: u64,
    /// How long a transaction may sit on the network unsettled.
    pub confirm_ms: u64,
    /// And how long before it is critical.
    pub critical_confirm_ms: u64,
    /// How many times the same bytes may go out before it is a storm.
    pub rebroadcasts: u32,
    /// A cluster taking at least this share of a launch's opening money is
    /// worth saying so about.
    pub cluster_share_bps: u16,
    /// ...as long as it has at least this many members. A single wallet with
    /// most of a launch is a whale, not a syndicate, and the two want different
    /// reactions.
    pub cluster_size: u32,
    /// A cluster whose holdings are spread *less* evenly than this is one
    /// funder with puppets. Millionths, and the comparison is the other way
    /// round from every other threshold here — see [`AlertThresholds::validate`].
    pub cluster_entropy_micros: u64,
    /// How long one kind of alert about one subject stays quiet after firing.
    pub cooldown_ms: i64,
}

impl Default for AlertThresholds {
    fn default() -> Self {
        AlertThresholds {
            // `ExitRoute::max_slippage_bps` on the policies this build ships is
            // three hundred, so a five-hundred floor fires on a fill that
            // cleared a route bound nobody would have set.
            slippage_bps: 500,
            critical_slippage_bps: 1_500,
            tip_grace_lamports: 0,
            // `BroadcastPolicy`'s total budget is the number this shadows: a
            // transaction still unsettled past it has outlived the loop that
            // was supposed to be managing it.
            confirm_ms: 30_000,
            critical_confirm_ms: 90_000,
            rebroadcasts: 3,
            cluster_share_bps: 4_000,
            cluster_size: 3,
            // §14's published low-entropy population scores 0.4690, and this
            // sits just above it so that shape fires.
            cluster_entropy_micros: 500_000,
            cooldown_ms: 60_000,
        }
    }
}

/// What is wrong with a set of thresholds somebody sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ThresholdError {
    /// The critical line is below the warning one, which would mean every
    /// warning is also a critical and the two levels say nothing.
    CriticalBelowWarning,
    /// A bound past ten thousand basis points is a bound on more than the whole
    /// of the thing being measured.
    ShareOverWhole,
    /// Millionths past a million.
    EntropyOverWhole,
    /// A negative cooldown, which would mean an alert fires before it fired.
    NegativeCooldown,
}

impl std::fmt::Display for ThresholdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ThresholdError::CriticalBelowWarning => {
                "a critical threshold below its warning makes the two levels the same"
            }
            ThresholdError::ShareOverWhole => {
                "a share cannot be more than ten thousand basis points"
            }
            ThresholdError::EntropyOverWhole => "entropy cannot be more than a million millionths",
            ThresholdError::NegativeCooldown => "a cooldown cannot run backwards",
        })
    }
}

impl AlertThresholds {
    /// Whether these can be applied.
    ///
    /// Checked when they are set rather than when they are read, so a
    /// contradictory pair is refused at the command that sent it and not
    /// discovered at three in the morning by an alert that never fires.
    pub fn validate(&self) -> Result<(), ThresholdError> {
        if self.critical_slippage_bps < self.slippage_bps
            || self.critical_confirm_ms < self.confirm_ms
        {
            return Err(ThresholdError::CriticalBelowWarning);
        }
        if self.slippage_bps > 10_000
            || self.critical_slippage_bps > 10_000
            || self.cluster_share_bps > 10_000
        {
            return Err(ThresholdError::ShareOverWhole);
        }
        // The one threshold that fires on being *below* it, so it is the one
        // whose ceiling is a whole rather than whose floor is.
        if self.cluster_entropy_micros > 1_000_000 {
            return Err(ThresholdError::EntropyOverWhole);
        }
        if self.cooldown_ms < 0 {
            return Err(ThresholdError::NegativeCooldown);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// what the engine hands over
// ---------------------------------------------------------------------------

/// One thing that just happened, for the thresholds to be held against.
///
/// Borrowed rather than owned: this is called on the engine's own thread after
/// every fill and every confirmation, and a variant that cloned two `String`s
/// to ask a question whose answer is almost always "nothing" would be paying
/// for the alert that did not fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation<'a> {
    /// A fill settled. `route_bound_bps` is the bound the route it went through
    /// was built with — the alert fires on the *wider* of that and the
    /// configured floor, so a deliberately permissive route does not alert on
    /// every fill and a permissive one still cannot hide a real spike.
    Filled {
        trade_id: &'a str,
        mint: &'a str,
        mode: ExecutionMode,
        fill: &'a FillRow,
        route_bound_bps: u16,
    },
    /// A tip was bid.
    Tipped {
        mint: &'a str,
        mode: ExecutionMode,
        tip: &'a TipRow,
    },
    /// A transaction's state was checked. `elapsed_ms` is how long it has been
    /// on the network, not how long the check took.
    Settled {
        trade_id: &'a str,
        mint: &'a str,
        mode: ExecutionMode,
        status: SignatureStatus,
        elapsed_ms: u64,
        rebroadcasts: u32,
    },
    /// A wallet cluster was seen on a mint. The fields are
    /// `strategy::syndicate::Cluster`'s, passed rather than the cluster itself
    /// so this module does not depend on the analyser's shape.
    Clustered {
        mint: &'a str,
        mode: ExecutionMode,
        cluster_id: &'a str,
        size: u32,
        share_of_open_bps: u16,
        /// `None` where the cluster was too small to measure, which is not a
        /// low-entropy cluster and does not fire.
        holding_entropy_micros: Option<u64>,
    },
}

impl Observation<'_> {
    /// The mode this happened in, for the payload.
    pub const fn mode(&self) -> ExecutionMode {
        match self {
            Observation::Filled { mode, .. }
            | Observation::Tipped { mode, .. }
            | Observation::Settled { mode, .. }
            | Observation::Clustered { mode, .. } => *mode,
        }
    }
}

/// Holds one observation against the thresholds and says what fired.
///
/// A free function and not a method, and pure: no clock, no counter, no
/// delivery. Every threshold in this file is tested through here, which is what
/// makes "does this fire at exactly the right number" a test that needs no
/// database, no hub and no socket.
///
/// `at_ms` is the alert's timestamp rather than a thing being compared — the
/// observation already carries whatever elapsed time it is being judged on,
/// because the engine knows when its own transaction went out and this does
/// not.
///
/// The `seq` on everything returned is zero; [`AlertDispatcher::observe`]
/// stamps the real one on the way past. Nothing here allocates until something
/// fires, and what fires is at most two alerts from one observation.
pub fn evaluate(
    thresholds: &AlertThresholds,
    observation: &Observation<'_>,
    at_ms: i64,
) -> Vec<Alert> {
    let mode = observation.mode();
    let mut fired = Vec::new();

    match *observation {
        Observation::Filled {
            trade_id,
            mint,
            fill,
            route_bound_bps,
            ..
        } => {
            // The wider of the two. A route that accepted five hundred basis
            // points said so on purpose; the configured floor is there for the
            // route that accepted five thousand.
            let bound = route_bound_bps.max(thresholds.slippage_bps);
            let observed = u64::from(fill.slippage_bps);
            if fill.slippage_bps > bound {
                let critical = fill.slippage_bps >= thresholds.critical_slippage_bps;
                fired.push(Alert {
                    seq: 0,
                    at_ms,
                    kind: AlertKind::SlippageSpike,
                    severity: if critical {
                        AlertSeverity::Critical
                    } else {
                        AlertSeverity::Warn
                    },
                    mode,
                    subject: trade_id.to_string(),
                    mint: Some(mint.to_string()),
                    message: format!(
                        "fill {} of {trade_id} came in {} bps under its quote, past a {} bps bound",
                        fill.seq, fill.slippage_bps, bound,
                    ),
                    observed,
                    threshold: u64::from(bound),
                    unit: AlertUnit::BasisPoints,
                });
            }
        }

        Observation::Tipped { mint, tip, .. } => {
            let allowed = tip
                .ceiling_lamports
                .saturating_add(thresholds.tip_grace_lamports);
            if tip.lamports > allowed {
                fired.push(Alert {
                    seq: 0,
                    at_ms,
                    kind: AlertKind::TipOverrun,
                    // Always critical. A bid past its ceiling is not a
                    // threshold being brushed, it is the cap not working, and
                    // there is no amount of it that is worth a quieter level.
                    severity: AlertSeverity::Critical,
                    mode,
                    subject: tip.trade_id.clone(),
                    mint: Some(mint.to_string()),
                    message: format!(
                        "attempt {} of {} bid {} lamports against a ceiling of {}",
                        tip.attempt, tip.trade_id, tip.lamports, tip.ceiling_lamports,
                    ),
                    observed: tip.lamports,
                    threshold: allowed,
                    unit: AlertUnit::Lamports,
                });
            }
        }

        Observation::Settled {
            trade_id,
            mint,
            status,
            elapsed_ms,
            rebroadcasts,
            ..
        } => {
            if status.is_in_flight() && elapsed_ms > thresholds.confirm_ms {
                let critical = elapsed_ms >= thresholds.critical_confirm_ms;
                fired.push(Alert {
                    seq: 0,
                    at_ms,
                    kind: AlertKind::ConfirmationLate,
                    severity: if critical {
                        AlertSeverity::Critical
                    } else {
                        AlertSeverity::Warn
                    },
                    mode,
                    subject: trade_id.to_string(),
                    mint: Some(mint.to_string()),
                    message: format!(
                        "{trade_id} has been on the network {elapsed_ms} ms with nothing back",
                    ),
                    observed: elapsed_ms,
                    threshold: thresholds.confirm_ms,
                    unit: AlertUnit::Milliseconds,
                });
            }

            if rebroadcasts > thresholds.rebroadcasts {
                fired.push(Alert {
                    seq: 0,
                    at_ms,
                    kind: AlertKind::RebroadcastStorm,
                    severity: AlertSeverity::Warn,
                    mode,
                    subject: trade_id.to_string(),
                    mint: Some(mint.to_string()),
                    message: format!("{trade_id} has gone out {rebroadcasts} times over"),
                    observed: u64::from(rebroadcasts),
                    threshold: u64::from(thresholds.rebroadcasts),
                    unit: AlertUnit::Count,
                });
            }

            // Dropped, expired and failed are three ways of not landing and one
            // thing to be told. Which one it was is in the message, because the
            // reaction is the same and a fourth alert kind would only split the
            // cooldown three ways.
            if matches!(
                status,
                SignatureStatus::Dropped | SignatureStatus::Expired | SignatureStatus::Failed
            ) {
                fired.push(Alert {
                    seq: 0,
                    at_ms,
                    kind: AlertKind::ExitFailed,
                    severity: AlertSeverity::Critical,
                    mode,
                    subject: trade_id.to_string(),
                    mint: Some(mint.to_string()),
                    message: format!("{trade_id} settled as {} without landing", status.as_str()),
                    observed: 1,
                    threshold: 0,
                    unit: AlertUnit::Count,
                });
            }
        }

        Observation::Clustered {
            mint,
            cluster_id,
            size,
            share_of_open_bps,
            holding_entropy_micros,
            ..
        } => {
            let big_enough = size >= thresholds.cluster_size;
            if big_enough && share_of_open_bps >= thresholds.cluster_share_bps {
                fired.push(Alert {
                    seq: 0,
                    at_ms,
                    kind: AlertKind::ClusterActivity,
                    severity: AlertSeverity::Warn,
                    mode,
                    subject: mint.to_string(),
                    mint: Some(mint.to_string()),
                    message: format!(
                        "cluster {cluster_id} of {size} wallets holds {share_of_open_bps} bps of the open on {mint}",
                    ),
                    observed: u64::from(share_of_open_bps),
                    threshold: u64::from(thresholds.cluster_share_bps),
                    unit: AlertUnit::BasisPoints,
                });
            }

            // The one threshold that fires on being under. An unmeasured
            // entropy is `None` and does not fire: §2.3's convention is that an
            // unmeasurable population is not a low-entropy one, and a zero here
            // would read as "one wallet holds everything".
            if let Some(entropy) = holding_entropy_micros {
                if big_enough && entropy < thresholds.cluster_entropy_micros {
                    fired.push(Alert {
                        seq: 0,
                        at_ms,
                        kind: AlertKind::ClusterActivity,
                        severity: AlertSeverity::Warn,
                        mode,
                        subject: mint.to_string(),
                        mint: Some(mint.to_string()),
                        message: format!(
                            "cluster {cluster_id} on {mint} spreads its holdings at {entropy} millionths, under {}",
                            thresholds.cluster_entropy_micros,
                        ),
                        observed: entropy,
                        threshold: thresholds.cluster_entropy_micros,
                        unit: AlertUnit::Micros,
                    });
                }
            }
        }
    }

    fired
}

// ---------------------------------------------------------------------------
// where alerts go
// ---------------------------------------------------------------------------

/// An alert destination that is not a window.
///
/// **`deliver` runs on the caller's thread**, which is the engine's, which is
/// the one that was in the middle of an execution when the alert fired. An
/// implementation that blocks in it — a socket, a lock somebody else holds —
/// holds up the trade that produced the alert. Buffer, or drop, but do not
/// wait. [`WebhookSink`] is the worked example: it does nothing in `deliver`
/// but put the alert on a bounded queue.
pub trait AlertSink: Send + Sync {
    fn deliver(&self, alert: &Alert);
    /// What to call this in a snapshot. A URL, a filename, whatever a person
    /// would recognise it by.
    fn name(&self) -> &str;
}

/// What is wrong with a webhook somebody configured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "detail", rename_all = "camelCase")]
pub enum WebhookError {
    /// Not `http://` or `https://`. No other scheme is dialled, and in
    /// particular a `file://` or a bare host is refused rather than guessed at.
    UnsupportedScheme(String),
    /// No host between the scheme and the path.
    NoHost,
    /// A port that is not a number, or is zero.
    BadPort(String),
}

impl std::fmt::Display for WebhookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebhookError::UnsupportedScheme(scheme) => {
                write!(f, "{scheme:?} is not a scheme this dials")
            }
            WebhookError::NoHost => f.write_str("the url names no host"),
            WebhookError::BadPort(port) => write!(f, "{port:?} is not a port"),
        }
    }
}

/// A parsed webhook URL.
///
/// Hand-rolled, for the reason `metrics.rs` hand-rolls the other end of the
/// same protocol: this build has no HTTP client and adding one to send a
/// two-hundred-byte POST would pull a runtime, a connection pool and a TLS
/// stack it already has. The parse is deliberately narrow — scheme, host, port,
/// path — because that is all a webhook URL is, and a parser that accepted more
/// would be a parser with more ways to be wrong about where an alert went.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WebhookTarget {
    pub host: String,
    pub port: u16,
    /// Everything after the host, query string included. Never empty.
    pub path: String,
    pub tls: bool,
}

impl WebhookTarget {
    pub fn parse(url: &str) -> Result<Self, WebhookError> {
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| WebhookError::UnsupportedScheme(url.to_string()))?;
        let tls = match scheme {
            "http" => false,
            "https" => true,
            other => return Err(WebhookError::UnsupportedScheme(other.to_string())),
        };

        let (authority, path) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err(WebhookError::NoHost);
        }

        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => {
                let port: u16 = port
                    .parse()
                    .map_err(|_| WebhookError::BadPort(port.to_string()))?;
                if port == 0 {
                    return Err(WebhookError::BadPort(port.to_string()));
                }
                (host, port)
            }
            None => (authority, if tls { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err(WebhookError::NoHost);
        }

        Ok(WebhookTarget {
            host: host.to_string(),
            port,
            path: path.to_string(),
            tls,
        })
    }

    /// The URL this came from, rebuilt. What a snapshot shows.
    pub fn as_url(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        let default = if self.tls { 443 } else { 80 };
        if self.port == default {
            format!("{scheme}://{}{}", self.host, self.path)
        } else {
            format!("{scheme}://{}:{}{}", self.host, self.port, self.path)
        }
    }
}

/// How a webhook behaves.
///
/// `#[serde(default)]` on the struct rather than on each field: a config that
/// names only a URL gets every number below at the value documented for it,
/// which is what a person writing one by hand expects, and is not the same as
/// getting a zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WebhookConfig {
    pub url: String,
    /// How long one POST may take, connect and read together. Past it the
    /// attempt is abandoned, and whether it is tried again is
    /// `max_retries`' business rather than this one's.
    pub timeout_ms: u64,
    /// How many alerts may be waiting before the newest is dropped.
    pub queue_depth: usize,
    /// How many times one alert goes out again after an attempt that failed in
    /// a way that might not repeat. Zero is one attempt and no more.
    ///
    /// Only some failures are retried; see [`Attempt`]. A `404` is the endpoint
    /// reading the request and saying no, and sending it again gets the same
    /// answer more slowly.
    pub max_retries: u32,
    /// The wait before the first retry.
    pub initial_backoff_ms: u64,
    /// What each wait multiplies the last by. One is a flat retry.
    pub backoff_factor: u32,
    /// The ceiling on any single wait.
    pub max_backoff_ms: u64,
    /// How many alerts may fail in a row before the endpoint is treated as
    /// down. Zero turns the breaker off entirely.
    pub failures_before_open: u32,
    /// How long the breaker stays open. One alert goes through when it expires,
    /// and what that one does decides whether it opens again.
    pub breaker_cooldown_ms: u64,
}

impl Default for WebhookConfig {
    /// Three retries doubling from 250ms, and a breaker that gives up on five
    /// failures in a row for half a minute.
    ///
    /// The retries are for the failure that is over in a moment — a dropped
    /// SYN, a load balancer moving a connection, a `503` while an endpoint
    /// restarts. The breaker is for the one that is not. Without it a webhook
    /// pointed at something that has been switched off spends
    /// `(1 + max_retries)` connect timeouts and three backoffs on every single
    /// alert, which at these numbers is about twenty-five seconds each — and
    /// the alerts behind it in the queue age by that much before anything even
    /// looks at them. Five in a row is enough to be sure it is not one bad
    /// moment, and thirty seconds is short enough that an endpoint which comes
    /// back is used again almost immediately.
    fn default() -> Self {
        WebhookConfig {
            url: String::new(),
            timeout_ms: 5_000,
            queue_depth: 256,
            max_retries: 3,
            initial_backoff_ms: 250,
            backoff_factor: 2,
            max_backoff_ms: 4_000,
            failures_before_open: 5,
            breaker_cooldown_ms: 30_000,
        }
    }
}

impl WebhookConfig {
    /// The wait before retry number `retry`, counting from zero.
    ///
    /// The same arithmetic and the same saturation as
    /// `execution::BroadcastPolicy::backoff_ms`, for the same reason: a factor
    /// of zero is read as one rather than collapsing every wait to nothing, and
    /// a multiplication that would overflow stops at the ceiling instead of
    /// wrapping to a shorter wait than the one before it.
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

    /// The longest one alert can occupy the worker, in milliseconds.
    ///
    /// Every attempt's timeout plus every wait between them. What a person
    /// wants this for is the other question — how far behind the queue can get
    /// while one alert is being retried — and multiplying it by `queue_depth`
    /// is how a configuration that cannot keep up is spotted before it is
    /// deployed rather than after.
    pub fn worst_case_ms(&self) -> u64 {
        let attempts = u64::from(self.max_retries).saturating_add(1);
        let posting = self.timeout_ms.saturating_mul(attempts);
        (0..self.max_retries)
            .map(|retry| self.backoff_ms(retry))
            .fold(posting, u64::saturating_add)
    }
}

/// What one webhook has done.
///
/// Every number is a count of alerts rather than of attempts, except `retried`,
/// which is the one question the others cannot answer: `delivered` counts an
/// alert that took four tries once, and a run where every alert needs four
/// tries is a run whose endpoint is about to trip the breaker.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookStats {
    pub url: String,
    pub queued: u64,
    /// Alerts thrown away because the queue was full. Non-zero means the
    /// endpoint is slower than the engine is alerting.
    pub dropped: u64,
    /// Alerts the far end answered with a 2xx, however many attempts that took.
    pub delivered: u64,
    /// Alerts that were attempted and not delivered — the endpoint refused
    /// them, or they ran out of attempts, or they were abandoned. Every alert
    /// that left the queue is in exactly one of `delivered` and this.
    pub failed: u64,
    /// Attempts past the first. Not a count of alerts: one alert retried three
    /// times adds three.
    pub retried: u64,
    /// How many of `failed` were given up on mid-retry because fresher alerts
    /// had filled the queue behind them. A subset of `failed` rather than a
    /// number beside it.
    ///
    /// Redelivery of an old alert is worth less than getting to a new one, and
    /// this is where that trade shows up. It counts toward the breaker like any
    /// other failure, deliberately: an endpoint slow enough to let the queue
    /// fill is one worth backing away from.
    pub abandoned: u64,
    /// Alerts not attempted at all because the breaker was open.
    pub shed: u64,
    /// How many times the endpoint has been declared down.
    pub breaker_trips: u64,
    /// Whether it is down now.
    pub breaker_open: bool,
    /// Failures since the last delivery. Resets to zero on any success.
    pub consecutive_failures: u32,
}

/// The counters the worker owns and the sink reports.
///
/// One `Arc` rather than eight, so adding a counter is a field here instead of
/// another clone threaded through the spawn.
#[derive(Debug, Default)]
struct WebhookCounters {
    delivered: AtomicU64,
    failed: AtomicU64,
    retried: AtomicU64,
    abandoned: AtomicU64,
    shed: AtomicU64,
    breaker_trips: AtomicU64,
    breaker_open: AtomicBool,
    consecutive_failures: AtomicU32,
}

/// One HTTP endpoint, fed from a queue by a thread of its own.
///
/// `deliver` puts the alert on the queue and returns. Everything that can block
/// — the DNS lookup, the connect, the TLS handshake, the write, the read —
/// happens on the worker, so the engine's thread pays a `try_send` and nothing
/// else.
///
/// The `Debug` is the target and the counters and not the channels: what a
/// person wants when one of these turns up in a panic message is where it was
/// posting and how that had been going.
pub struct WebhookSink {
    target: WebhookTarget,
    url: String,
    tx: Sender<Alert>,
    shutdown: Sender<()>,
    worker: Mutex<Option<JoinHandle<()>>>,
    queued: AtomicU64,
    dropped: AtomicU64,
    counters: Arc<WebhookCounters>,
}

impl std::fmt::Debug for WebhookSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookSink")
            .field("url", &self.url)
            .field("stats", &self.stats())
            .finish()
    }
}

impl WebhookSink {
    /// Parses the URL, starts the worker, and hands back the sink.
    pub fn start(config: &WebhookConfig) -> Result<Arc<Self>, WebhookError> {
        let target = WebhookTarget::parse(&config.url)?;
        let url = target.as_url();
        let (tx, rx) = bounded::<Alert>(config.queue_depth.max(1));
        let (shutdown, shutdown_rx) = bounded::<()>(1);
        let counters = Arc::new(WebhookCounters::default());

        let worker = std::thread::Builder::new()
            .name("sts-webhook".to_string())
            .spawn({
                let target = target.clone();
                let policy = config.clone();
                let counters = Arc::clone(&counters);
                move || webhook_loop(target, policy, rx, shutdown_rx, counters)
            })
            .map_err(|_| WebhookError::NoHost)?;

        Ok(Arc::new(WebhookSink {
            target,
            url,
            tx,
            shutdown,
            worker: Mutex::new(Some(worker)),
            queued: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            counters,
        }))
    }

    pub fn target(&self) -> &WebhookTarget {
        &self.target
    }

    pub fn stats(&self) -> WebhookStats {
        WebhookStats {
            url: self.url.clone(),
            queued: self.queued.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            delivered: self.counters.delivered.load(Ordering::Relaxed),
            failed: self.counters.failed.load(Ordering::Relaxed),
            retried: self.counters.retried.load(Ordering::Relaxed),
            abandoned: self.counters.abandoned.load(Ordering::Relaxed),
            shed: self.counters.shed.load(Ordering::Relaxed),
            breaker_trips: self.counters.breaker_trips.load(Ordering::Relaxed),
            breaker_open: self.counters.breaker_open.load(Ordering::Relaxed),
            consecutive_failures: self.counters.consecutive_failures.load(Ordering::Relaxed),
        }
    }

    /// Whether the endpoint is currently being treated as down.
    pub fn is_breaker_open(&self) -> bool {
        self.counters.breaker_open.load(Ordering::Relaxed)
    }

    /// Stops the worker and waits for it. Safe to call twice.
    ///
    /// The wait is bounded by one POST's timeout and no more, which is the
    /// reason the backoff between retries is a `recv_timeout` on the shutdown
    /// channel rather than a `sleep`. A worker asleep in a four-second backoff
    /// would otherwise make every shutdown four seconds long, and a shutdown
    /// that waits on a dead endpoint is the failure this whole file is arranged
    /// to avoid, arriving at the last possible moment.
    pub fn stop(&self) {
        let Some(handle) = self.worker.lock().take() else {
            return;
        };
        let _ = self.shutdown.try_send(());
        let _ = handle.join();
    }
}

impl AlertSink for WebhookSink {
    fn deliver(&self, alert: &Alert) {
        // The one clone, and it is on the queueing side rather than in the
        // worker so the borrow the engine handed us does not have to outlive
        // this call.
        if self.tx.try_send(alert.clone()).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        } else {
            self.queued.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn name(&self) -> &str {
        &self.url
    }
}

impl Drop for WebhookSink {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What one POST came back with, in the three shapes the retry loop acts on.
///
/// The split that matters is the middle one. A `404`, a `401` or a `422` is the
/// endpoint reading the request and saying no, and sending the identical bytes
/// again gets the identical answer — slower, and while the alerts behind it
/// age. A `503` or a socket that never connected is the endpoint not having
/// answered at all, which is the case retrying exists for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Attempt {
    /// A 2xx.
    Delivered,
    /// The endpoint answered and refused. Not retried.
    Refused(u16),
    /// Nothing answered, or what answered said to come back. Retried.
    Retryable(String),
}

/// What one status code means for whether to send the alert again.
///
/// Apart from `attempt` below so the rule can be stated against every code that
/// matters without standing up a socket per case.
fn attempt_from_status(status: u16) -> Attempt {
    match status {
        200..=299 => Attempt::Delivered,
        // The two 4xx that mean "the request was fine and the moment was not".
        408 | 429 => Attempt::Retryable(format!("http {status}")),
        400..=499 => Attempt::Refused(status),
        _ => Attempt::Retryable(format!("http {status}")),
    }
}

/// One POST, classified.
fn attempt(target: &WebhookTarget, timeout: Duration, alert: &Alert) -> Attempt {
    match post(target, timeout, alert) {
        Ok(status) => attempt_from_status(status),
        // Nothing answered at all: no connection, no route, a timeout, or a
        // hang-up before the status line. Every one of those is a reason to try
        // again rather than a decision the far end made.
        Err(err) => Attempt::Retryable(err.to_string()),
    }
}

/// What became of one alert, after however many attempts it got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Delivered,
    Failed,
    /// Given up on mid-retry because the queue behind it had filled.
    Abandoned,
}

/// Waits, unless told to stop. `false` means stop.
///
/// A `recv_timeout` on the shutdown channel rather than a `sleep`, so a backoff
/// in progress does not add itself to how long `stop` takes.
fn backoff(shutdown: &Receiver<()>, ms: u64) -> bool {
    if ms == 0 {
        // Still asked, so that this stays the retry loop's one shutdown check
        // whatever the backoff is configured to. A zero wait that returned
        // `true` unconditionally would mean a webhook with no backoff kept
        // posting through a stop, and `stop` would wait out every remaining
        // attempt's timeout rather than one.
        return matches!(
            shutdown.try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty)
        );
    }
    matches!(
        shutdown.recv_timeout(Duration::from_millis(ms)),
        Err(crossbeam_channel::RecvTimeoutError::Timeout)
    )
}

/// One alert, through as many attempts as the policy allows.
///
/// Stops early on three things: a delivery, a refusal, and a queue that has
/// filled past half while this one was being retried. The last is the load
/// shedding that keeps a slow endpoint from turning into a lost one — an alert
/// being retried for the third time is by definition an old alert, and the
/// fresh ones stacking up behind it are the ones somebody still needs.
fn deliver_with_retry(
    target: &WebhookTarget,
    policy: &WebhookConfig,
    timeout: Duration,
    alert: &Alert,
    rx: &Receiver<Alert>,
    shutdown: &Receiver<()>,
    counters: &WebhookCounters,
) -> Option<Delivery> {
    for retry in 0..=policy.max_retries {
        if retry > 0 {
            counters.retried.fetch_add(1, Ordering::Relaxed);
        }
        match attempt(target, timeout, alert) {
            Attempt::Delivered => return Some(Delivery::Delivered),
            Attempt::Refused(_) => return Some(Delivery::Failed),
            Attempt::Retryable(_) => {
                if retry == policy.max_retries {
                    return Some(Delivery::Failed);
                }
                if rx.len().saturating_mul(2) >= rx.capacity().unwrap_or(usize::MAX) {
                    return Some(Delivery::Abandoned);
                }
                if !backoff(shutdown, policy.backoff_ms(retry)) {
                    // Told to stop mid-backoff. Not counted here - the
                    // caller decides, because whether an interrupted alert is a
                    // failure is an accounting question rather than a delivery
                    // one, and the two callers of this answer it differently.
                    return None;
                }
            }
        }
    }
    Some(Delivery::Failed)
}

/// The worker. One endpoint, one queue, one thread, and nothing shared with the
/// engine but a bounded channel.
///
/// The breaker is the part worth reading. Without it a webhook pointed at
/// something switched off spends its whole life in connect timeouts, and every
/// alert behind it in the queue ages by the full retry budget before anything
/// looks at it — so a dead endpoint would not just fail to deliver, it would
/// make the queue useless for the endpoint's own recovery. Open, alerts are
/// counted and dropped immediately; when the cooldown expires exactly one goes
/// through, and what it does decides whether the breaker shuts or opens again.
fn webhook_loop(
    target: WebhookTarget,
    policy: WebhookConfig,
    rx: Receiver<Alert>,
    shutdown: Receiver<()>,
    counters: Arc<WebhookCounters>,
) {
    let timeout = Duration::from_millis(policy.timeout_ms.max(1));
    let cooldown = Duration::from_millis(policy.breaker_cooldown_ms);
    let mut open_until: Option<std::time::Instant> = None;

    loop {
        // Checked before the select rather than only inside it. `select!` picks
        // at random between two ready arms, so a stop arriving alongside a full
        // queue would otherwise be a stop that waits for the backlog to drain
        // into an endpoint that is probably the reason there is a backlog.
        if !matches!(
            shutdown.try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty)
        ) {
            break;
        }

        let alert = crossbeam_channel::select! {
            recv(rx) -> received => match received {
                Ok(alert) => alert,
                // Every sender is gone.
                Err(_) => break,
            },
            recv(shutdown) -> _ => break,
        };

        if let Some(until) = open_until {
            if std::time::Instant::now() < until {
                counters.shed.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            // The cooldown is up. This alert is the probe.
            open_until = None;
            counters.breaker_open.store(false, Ordering::Relaxed);
        }

        let Some(delivery) =
            deliver_with_retry(&target, &policy, timeout, &alert, &rx, &shutdown, &counters)
        else {
            // Interrupted mid-backoff by the stop. `deliver_with_retry` leaves
            // this one uncounted on the grounds that a shutdown should not
            // invent a failure; Sprint 2's accounting rule is the stronger of
            // the two and wins here, because an alert in no counter at all is
            // not neutral - it is lost, and `stats()` is where the loss is
            // supposed to be visible. Counted where the drain below counts what
            // it runs out of time for, and for the same reason.
            counters.failed.fetch_add(1, Ordering::Relaxed);
            break;
        };

        match delivery {
            Delivery::Delivered => {
                counters.delivered.fetch_add(1, Ordering::Relaxed);
                counters.consecutive_failures.store(0, Ordering::Relaxed);
            }
            Delivery::Failed | Delivery::Abandoned => {
                counters.failed.fetch_add(1, Ordering::Relaxed);
                if delivery == Delivery::Abandoned {
                    counters.abandoned.fetch_add(1, Ordering::Relaxed);
                }
                // Not reset when the breaker opens, which is what makes the
                // probe after a cooldown re-open it on one failure rather than
                // on another five.
                let in_a_row = counters
                    .consecutive_failures
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                        Some(n.saturating_add(1))
                    })
                    .unwrap_or(0)
                    .saturating_add(1);
                if policy.failures_before_open > 0 && in_a_row >= policy.failures_before_open {
                    open_until = Some(std::time::Instant::now() + cooldown);
                    counters.breaker_open.store(true, Ordering::Relaxed);
                    counters.breaker_trips.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    // `select!` picks at random between two ready arms, so a stop arriving
    // while an alert is still queued can win the race and leave it there — an
    // alert the engine was told had been dispatched, silently dropped by the
    // shutdown. Anything already accepted is sent.
    //
    // Under a deadline, because unlike the telemetry pump every delivery here
    // is a POST to somebody else's endpoint, and a shutdown that waits on one
    // that has stopped answering is not a graceful one. One `timeout` for the
    // whole drain; whatever is left when it passes is counted failed rather
    // than forgotten, so the loss shows up in `stats()` instead of nowhere.
    let deadline = Instant::now() + timeout;
    while let Ok(alert) = rx.try_recv() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            counters.failed.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        // A single POST rather than deliver_with_retry: the drain has one
        // deadline for the whole queue, and retrying one alert inside it is
        // spending the budget of the alerts still behind it.
        match post(&target, remaining, &alert) {
            Ok(status) if (200..300).contains(&status) => {
                counters.delivered.fetch_add(1, Ordering::Relaxed);
            }
            _ => {
                counters.failed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// One connection, one POST, one status code.
///
/// `Connection: close` and a fresh socket per alert rather than a kept-alive
/// one. Alerts are rare by construction, so the handshake is not a cost that
/// shows up, and a pooled connection that has gone stale between two alerts an
/// hour apart is a failure mode bought for nothing.
fn post(target: &WebhookTarget, timeout: Duration, alert: &Alert) -> std::io::Result<u16> {
    let body = serde_json::to_vec(alert)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;

    let address = (target.host.as_str(), target.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no address"))?;
    let stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.set_nodelay(true)?;

    let mut wire = if target.tls {
        let connector = native_tls::TlsConnector::new().map_err(std::io::Error::other)?;
        let tls = connector
            .connect(&target.host, stream)
            .map_err(|err| std::io::Error::other(err.to_string()))?;
        Wire::Tls(Box::new(tls))
    } else {
        Wire::Plain(stream)
    };

    let head = format!(
        "POST {} HTTP/1.1\r\n\
         Host: {}\r\n\
         User-Agent: sts/{}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        target.path,
        target.host,
        env!("CARGO_PKG_VERSION"),
        body.len(),
    );
    wire.write_all(head.as_bytes())?;
    wire.write_all(&body)?;
    wire.flush()?;

    // Only the status line is read. The body of a webhook response is not
    // something this can act on, and reading it to the end would mean waiting
    // for an endpoint that streams.
    let mut buffer = [0u8; 128];
    let read = wire.read(&mut buffer)?;
    parse_status(&buffer[..read])
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no status line"))
}

/// `HTTP/1.1 204 No Content` to `204`.
fn parse_status(response: &[u8]) -> Option<u16> {
    let text = std::str::from_utf8(response).ok()?;
    let line = text.lines().next()?;
    let mut parts = line.split(' ');
    let version = parts.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    parts.next()?.parse().ok()
}

/// A socket that may or may not have TLS over it.
enum Wire {
    Plain(TcpStream),
    Tls(Box<native_tls::TlsStream<TcpStream>>),
}

impl Read for Wire {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Wire::Plain(stream) => stream.read(buffer),
            Wire::Tls(stream) => stream.read(buffer),
        }
    }
}

impl Write for Wire {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        match self {
            Wire::Plain(stream) => stream.write(buffer),
            Wire::Tls(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Wire::Plain(stream) => stream.flush(),
            Wire::Tls(stream) => stream.flush(),
        }
    }
}

// ---------------------------------------------------------------------------
// the dispatcher
// ---------------------------------------------------------------------------

/// How many `(kind, subject)` pairs the cooldown remembers.
///
/// A long run touching thousands of mints would otherwise grow this map for the
/// length of the process. Past the cap, everything already outside its cooldown
/// is dropped — those entries can no longer suppress anything, so forgetting
/// them changes no decision.
const COOLDOWN_CAPACITY: usize = 4_096;

/// What `stream_alerts` hands back so the window knows the feed is live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlertSubscription {
    pub subscriber_id: u64,
    /// The sequence the next delivered alert will carry.
    pub from_seq: u64,
}

/// What the dispatcher has done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlertSnapshot {
    pub thresholds: AlertThresholds,
    /// Alerts that fired and were delivered.
    pub raised: u64,
    /// Alerts that fired and were inside another one's cooldown. Counted rather
    /// than dropped silently: "nine of these, one shown" is a different
    /// sentence from "one of these".
    pub suppressed: u64,
    /// How many times a sink panicked on being handed an alert. Zero on every
    /// build that has ever run; a number here means a sink is broken in a way
    /// that would have taken the engine's thread with it.
    pub sink_panics: u64,
    pub by_kind: Vec<KindCount>,
    pub subscribers: usize,
    pub webhooks: Vec<WebhookStats>,
}

/// One kind and how many of it. A `Vec` of pairs rather than a map, because
/// this crosses IPC and an object keyed by an enum name is harder for a window
/// to iterate in a stable order than a list already in one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KindCount {
    pub kind: AlertKind,
    pub raised: u64,
}

/// Holds observations against thresholds and tells whoever is listening.
///
/// One per process, behind an `Arc`, alongside the engine and the telemetry hub
/// it publishes through.
pub struct AlertDispatcher {
    thresholds: RwLock<AlertThresholds>,
    telemetry: Arc<TelemetryHub>,
    subscribers: RwLock<HashMap<u64, Channel<Alert>>>,
    sinks: RwLock<Vec<Arc<dyn AlertSink>>>,
    webhooks: RwLock<Vec<Arc<WebhookSink>>>,
    seq: AtomicU64,
    next_subscriber: AtomicU64,
    raised: AtomicU64,
    suppressed: AtomicU64,
    /// How many times a sink panicked while being handed an alert. Should be
    /// zero forever; see `dispatch` for why it is counted rather than trusted.
    sink_panics: AtomicU64,
    by_kind: [AtomicU64; AlertKind::ALL.len()],
    /// When each subject last fired, per kind.
    ///
    /// Nested rather than keyed by the pair, so the lookup on the firing path
    /// probes with a `&str` the observation already owns instead of allocating
    /// a `String` to build a tuple key with.
    fired_at: Mutex<HashMap<AlertKind, HashMap<String, i64>>>,
}

impl AlertDispatcher {
    /// A dispatcher at the default thresholds, publishing through this hub.
    pub fn new(telemetry: Arc<TelemetryHub>) -> Self {
        AlertDispatcher {
            thresholds: RwLock::new(AlertThresholds::default()),
            telemetry,
            subscribers: RwLock::new(HashMap::new()),
            sinks: RwLock::new(Vec::new()),
            webhooks: RwLock::new(Vec::new()),
            seq: AtomicU64::new(0),
            next_subscriber: AtomicU64::new(1),
            raised: AtomicU64::new(0),
            suppressed: AtomicU64::new(0),
            sink_panics: AtomicU64::new(0),
            by_kind: std::array::from_fn(|_| AtomicU64::new(0)),
            fired_at: Mutex::new(HashMap::new()),
        }
    }

    pub fn thresholds(&self) -> AlertThresholds {
        *self.thresholds.read()
    }

    /// Replaces the thresholds, or explains why it will not.
    ///
    /// The cooldown history is left alone. A threshold that was just lowered
    /// should start firing on the next observation past it, not re-fire
    /// everything it has already said.
    pub fn set_thresholds(&self, thresholds: AlertThresholds) -> Result<(), ThresholdError> {
        thresholds.validate()?;
        *self.thresholds.write() = thresholds;
        Ok(())
    }

    /// Registers a window's channel.
    pub fn subscribe(&self, channel: Channel<Alert>) -> AlertSubscription {
        let subscriber_id = self.next_subscriber.fetch_add(1, Ordering::Relaxed);
        self.subscribers.write().insert(subscriber_id, channel);
        AlertSubscription {
            subscriber_id,
            from_seq: self.seq.load(Ordering::Relaxed),
        }
    }

    /// Adds a destination that is not a window.
    pub fn attach_sink(&self, sink: Arc<dyn AlertSink>) {
        self.sinks.write().push(sink);
    }

    /// Starts a webhook and attaches it. The sink is kept in two places on
    /// purpose: once as the thing alerts are handed to, and once typed, so
    /// [`AlertDispatcher::snapshot`] can report its counters without
    /// downcasting.
    pub fn attach_webhook(&self, config: &WebhookConfig) -> Result<Arc<WebhookSink>, WebhookError> {
        let sink = WebhookSink::start(config)?;
        self.sinks
            .write()
            .push(Arc::clone(&sink) as Arc<dyn AlertSink>);
        self.webhooks.write().push(Arc::clone(&sink));
        Ok(sink)
    }

    /// Holds one observation against the thresholds and delivers what fired.
    ///
    /// Returns what was actually delivered, which is not everything that fired:
    /// an alert inside another one's cooldown is counted and dropped. The
    /// return is there so a caller that wants to record its own alerts — the
    /// journal, a test — sees exactly what the listeners saw.
    ///
    /// Runs on the caller's thread. The threshold comparisons are integer and
    /// allocate nothing; the cooldown lock is taken only when something fired,
    /// so an observation that clears every threshold — which is almost all of
    /// them — costs a read lock and some arithmetic.
    pub fn observe(&self, observation: &Observation<'_>, at_ms: i64) -> Vec<Alert> {
        let thresholds = *self.thresholds.read();
        let candidates = evaluate(&thresholds, observation, at_ms);
        if candidates.is_empty() {
            return Vec::new();
        }

        let mut delivered = Vec::with_capacity(candidates.len());
        {
            let mut history = self.fired_at.lock();
            if history.values().map(HashMap::len).sum::<usize>() >= COOLDOWN_CAPACITY {
                for subjects in history.values_mut() {
                    subjects.retain(|_, last| at_ms.saturating_sub(*last) < thresholds.cooldown_ms);
                }
                history.retain(|_, subjects| !subjects.is_empty());
            }
            for mut alert in candidates {
                let subjects = history.entry(alert.kind).or_default();
                let quiet_until = subjects
                    .get(alert.subject.as_str())
                    .map(|last| last.saturating_add(thresholds.cooldown_ms));
                if quiet_until.is_some_and(|until| at_ms < until) {
                    self.suppressed.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                subjects.insert(alert.subject.clone(), at_ms);
                alert.seq = self.seq.fetch_add(1, Ordering::Relaxed);
                delivered.push(alert);
            }
        }

        for alert in &delivered {
            self.raised.fetch_add(1, Ordering::Relaxed);
            if let Some(index) = AlertKind::ALL.iter().position(|k| *k == alert.kind) {
                self.by_kind[index].fetch_add(1, Ordering::Relaxed);
            }
            self.dispatch(alert);
        }
        delivered
    }

    /// Hands one alert to everything listening.
    ///
    /// The hub first, then the windows, then the sinks — cheapest and most
    /// certain first. A window whose channel has failed is swept here for the
    /// same reason `telemetry.rs` sweeps its own: a closed window that stayed
    /// in the map would be a failed send on every alert forever.
    fn dispatch(&self, alert: &Alert) {
        self.telemetry.publish(
            alert.severity.as_telemetry_level(),
            "alerting",
            alert.message.clone(),
            serde_json::to_value(alert).unwrap_or(serde_json::Value::Null),
        );

        let mut dead = Vec::new();
        for (id, channel) in self.subscribers.read().iter() {
            if channel.send(alert.clone()).is_err() {
                dead.push(*id);
            }
        }
        if !dead.is_empty() {
            let mut guard = self.subscribers.write();
            for id in dead {
                guard.remove(&id);
            }
        }

        // Every sink is called inside a `catch_unwind`, and the reason is the
        // thread this runs on. `deliver` is documented not to block, but
        // `AlertSink` is a public trait and the contract is a sentence in a doc
        // comment rather than something the type system holds anyone to — an
        // implementation that indexes a slice out of range would unwind through
        // here, through `observe`, and out of the middle of the exit that
        // raised the alert. A webhook that cannot reach its endpoint must not
        // be able to stop a position being sold, and neither must one that
        // panics.
        //
        // The panic is counted rather than swallowed silently, so a sink that
        // is failing this way is visible in the snapshot instead of just being
        // a sink nothing ever arrives at.
        for sink in self.sinks.read().iter() {
            let delivered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                sink.deliver(alert);
            }));
            if delivered.is_err() {
                self.sink_panics.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn snapshot(&self) -> AlertSnapshot {
        AlertSnapshot {
            thresholds: self.thresholds(),
            raised: self.raised.load(Ordering::Relaxed),
            suppressed: self.suppressed.load(Ordering::Relaxed),
            sink_panics: self.sink_panics.load(Ordering::Relaxed),
            by_kind: AlertKind::ALL
                .iter()
                .enumerate()
                .map(|(index, kind)| KindCount {
                    kind: *kind,
                    raised: self.by_kind[index].load(Ordering::Relaxed),
                })
                .collect(),
            subscribers: self.subscribers.read().len(),
            webhooks: self.webhooks.read().iter().map(|w| w.stats()).collect(),
        }
    }

    /// Stops every webhook and drops every listener. Called on the way out, so
    /// no worker is still holding a socket when the process exits.
    pub fn shutdown(&self) {
        for webhook in self.webhooks.write().drain(..) {
            webhook.stop();
        }
        self.sinks.write().clear();
        self.subscribers.write().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;

    use crate::journal::SignatureKind;
    use crate::telemetry::{TelemetryEvent, TelemetrySink};

    const AT_MS: i64 = 1_700_000_000_000;
    const MINT: &str = "So11111111111111111111111111111111111111112";

    /// A fill that came in `slippage_bps` under its quote.
    ///
    /// Built through `FillRow::settle` rather than by hand, so the slippage the
    /// thresholds are held against is the one the journal would have stored —
    /// a test that constructed the field directly would be testing a number
    /// this codebase cannot produce.
    fn fill_at(bps: u16) -> FillRow {
        let quoted = 1_000_000u64;
        let filled = quoted - u64::from(bps) * quoted / 10_000;
        FillRow::settle("t-1", 0, 1_000_000, filled, 0, quoted, 250_000_000, AT_MS)
            .expect("a real fill")
    }

    fn tip_at(lamports: u64, ceiling: u64) -> TipRow {
        TipRow {
            trade_id: "t-1".to_string(),
            attempt: 2,
            account: "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5".to_string(),
            lamports,
            stance: crate::execution::TipStance::Emergency,
            ev_net_lamports: None,
            ceiling_lamports: ceiling,
            at_ms: AT_MS,
        }
    }

    fn filled(fill: &FillRow, route_bound_bps: u16) -> Observation<'_> {
        Observation::Filled {
            trade_id: "t-1",
            mint: MINT,
            mode: ExecutionMode::Paper,
            fill,
            route_bound_bps,
        }
    }

    fn settled(
        status: SignatureStatus,
        elapsed_ms: u64,
        rebroadcasts: u32,
    ) -> Observation<'static> {
        Observation::Settled {
            trade_id: "t-1",
            mint: MINT,
            mode: ExecutionMode::Paper,
            status,
            elapsed_ms,
            rebroadcasts,
        }
    }

    fn clustered(size: u32, share_bps: u16, entropy: Option<u64>) -> Observation<'static> {
        Observation::Clustered {
            mint: MINT,
            mode: ExecutionMode::Paper,
            cluster_id: "c-1",
            size,
            share_of_open_bps: share_bps,
            holding_entropy_micros: entropy,
        }
    }

    fn kinds(alerts: &[Alert]) -> Vec<AlertKind> {
        alerts.iter().map(|a| a.kind).collect()
    }

    // -----------------------------------------------------------------------
    // where exactly each line is
    // -----------------------------------------------------------------------

    #[test]
    fn slippage_fires_one_basis_point_past_the_bound_and_not_at_it() {
        let thresholds = AlertThresholds {
            slippage_bps: 500,
            ..Default::default()
        };
        // The route's own bound is narrower than the floor, so the floor wins.
        assert!(evaluate(&thresholds, &filled(&fill_at(499), 300), AT_MS).is_empty());
        assert!(evaluate(&thresholds, &filled(&fill_at(500), 300), AT_MS).is_empty());
        let fired = evaluate(&thresholds, &filled(&fill_at(501), 300), AT_MS);
        assert_eq!(kinds(&fired), vec![AlertKind::SlippageSpike]);
        assert_eq!(fired[0].observed, 501);
        assert_eq!(fired[0].threshold, 500);
        assert_eq!(fired[0].overshoot(), 1);
        assert_eq!(fired[0].unit, AlertUnit::BasisPoints);
    }

    #[test]
    fn a_route_that_accepted_a_wide_bound_is_not_alerted_on_for_taking_it() {
        let thresholds = AlertThresholds {
            slippage_bps: 500,
            ..Default::default()
        };
        // A route built with a 2000 bps bound took 1500. That is the policy
        // working, not a spike, and the floor must not override the wider
        // number somebody set on purpose.
        assert!(evaluate(&thresholds, &filled(&fill_at(1_500), 2_000), AT_MS).is_empty());
        // Past its own bound, it fires against that bound rather than the floor.
        let fired = evaluate(&thresholds, &filled(&fill_at(2_100), 2_000), AT_MS);
        assert_eq!(fired[0].threshold, 2_000);
    }

    #[test]
    fn the_critical_line_is_where_it_says_it_is() {
        let thresholds = AlertThresholds {
            slippage_bps: 500,
            critical_slippage_bps: 1_500,
            ..Default::default()
        };
        let warn = evaluate(&thresholds, &filled(&fill_at(1_499), 300), AT_MS);
        assert_eq!(warn[0].severity, AlertSeverity::Warn);
        // At the line, not past it: a threshold somebody set to 1500 means
        // 1500 is critical.
        let critical = evaluate(&thresholds, &filled(&fill_at(1_500), 300), AT_MS);
        assert_eq!(critical[0].severity, AlertSeverity::Critical);
    }

    #[test]
    fn a_tip_inside_its_ceiling_is_silent_and_one_lamport_over_is_not() {
        let thresholds = AlertThresholds::default();
        assert!(evaluate(
            &thresholds,
            &Observation::Tipped {
                mint: MINT,
                mode: ExecutionMode::Paper,
                tip: &tip_at(1_000_000, 1_000_000),
            },
            AT_MS
        )
        .is_empty());

        let fired = evaluate(
            &thresholds,
            &Observation::Tipped {
                mint: MINT,
                mode: ExecutionMode::Paper,
                tip: &tip_at(1_000_001, 1_000_000),
            },
            AT_MS,
        );
        assert_eq!(kinds(&fired), vec![AlertKind::TipOverrun]);
        // Always critical. There is no small overrun.
        assert_eq!(fired[0].severity, AlertSeverity::Critical);
        assert_eq!(fired[0].unit, AlertUnit::Lamports);
        assert_eq!(fired[0].subject, "t-1");
    }

    #[test]
    fn the_tip_grace_moves_the_line_and_nothing_else() {
        let thresholds = AlertThresholds {
            tip_grace_lamports: 50_000,
            ..Default::default()
        };
        let observation = Observation::Tipped {
            mint: MINT,
            mode: ExecutionMode::Paper,
            tip: &tip_at(1_050_000, 1_000_000),
        };
        assert!(evaluate(&thresholds, &observation, AT_MS).is_empty());
        let over = Observation::Tipped {
            mint: MINT,
            mode: ExecutionMode::Paper,
            tip: &tip_at(1_050_001, 1_000_000),
        };
        assert_eq!(evaluate(&thresholds, &over, AT_MS)[0].threshold, 1_050_000);
    }

    #[test]
    fn only_something_still_out_there_can_be_late() {
        let thresholds = AlertThresholds {
            confirm_ms: 30_000,
            ..Default::default()
        };
        // Confirmed an hour ago is not a late confirmation, it is a finished
        // one, and the elapsed time on it means nothing.
        assert!(evaluate(
            &thresholds,
            &settled(SignatureStatus::Confirmed, 3_600_000, 0),
            AT_MS,
        )
        .is_empty());

        assert!(evaluate(
            &thresholds,
            &settled(SignatureStatus::Broadcast, 30_000, 0),
            AT_MS
        )
        .is_empty());
        let fired = evaluate(
            &thresholds,
            &settled(SignatureStatus::Broadcast, 30_001, 0),
            AT_MS,
        );
        assert_eq!(kinds(&fired), vec![AlertKind::ConfirmationLate]);
        assert_eq!(fired[0].unit, AlertUnit::Milliseconds);
    }

    #[test]
    fn one_observation_can_be_two_things_wrong_at_once() {
        let thresholds = AlertThresholds::default();
        // Still out there, too long, and having gone out five times.
        let fired = evaluate(
            &thresholds,
            &settled(SignatureStatus::Broadcast, 120_000, 5),
            AT_MS,
        );
        assert_eq!(
            kinds(&fired),
            vec![AlertKind::ConfirmationLate, AlertKind::RebroadcastStorm],
        );
        assert_eq!(
            fired[0].severity,
            AlertSeverity::Critical,
            "past the critical confirm line"
        );
        assert_eq!(fired[1].observed, 5);
    }

    #[test]
    fn the_three_ways_of_not_landing_are_one_alert() {
        let thresholds = AlertThresholds::default();
        for status in [
            SignatureStatus::Dropped,
            SignatureStatus::Expired,
            SignatureStatus::Failed,
        ] {
            let fired = evaluate(&thresholds, &settled(status, 1_000, 0), AT_MS);
            assert_eq!(kinds(&fired), vec![AlertKind::ExitFailed], "{status:?}");
            assert_eq!(fired[0].severity, AlertSeverity::Critical);
            assert!(fired[0].message.contains(status.as_str()));
        }
    }

    #[test]
    fn a_cluster_has_to_be_a_group_before_its_share_means_anything() {
        let thresholds = AlertThresholds {
            cluster_size: 3,
            cluster_share_bps: 4_000,
            ..Default::default()
        };
        // Two wallets with most of the launch is a whale, and the size gate is
        // what keeps that out of the syndicate alert.
        assert!(evaluate(&thresholds, &clustered(2, 9_000, None), AT_MS).is_empty());
        assert!(evaluate(&thresholds, &clustered(3, 3_999, None), AT_MS).is_empty());
        let fired = evaluate(&thresholds, &clustered(3, 4_000, None), AT_MS);
        assert_eq!(kinds(&fired), vec![AlertKind::ClusterActivity]);
        assert_eq!(fired[0].subject, MINT, "a cluster alert is about the mint");
    }

    #[test]
    fn an_entropy_nobody_measured_is_not_a_low_one() {
        let thresholds = AlertThresholds {
            cluster_size: 3,
            cluster_share_bps: 10_000,
            cluster_entropy_micros: 500_000,
            ..Default::default()
        };
        // The share gate is set out of reach, so only the entropy term can fire.
        assert!(evaluate(&thresholds, &clustered(5, 0, None), AT_MS).is_empty());
        assert!(evaluate(&thresholds, &clustered(5, 0, Some(500_000)), AT_MS).is_empty());
        let fired = evaluate(&thresholds, &clustered(5, 0, Some(499_999)), AT_MS);
        assert_eq!(kinds(&fired), vec![AlertKind::ClusterActivity]);
        assert_eq!(fired[0].unit, AlertUnit::Micros);
        assert_eq!(fired[0].observed, 499_999);
    }

    #[test]
    fn thresholds_that_contradict_themselves_are_refused() {
        assert_eq!(AlertThresholds::default().validate(), Ok(()));
        assert_eq!(
            AlertThresholds {
                slippage_bps: 900,
                critical_slippage_bps: 800,
                ..Default::default()
            }
            .validate(),
            Err(ThresholdError::CriticalBelowWarning),
        );
        assert_eq!(
            AlertThresholds {
                confirm_ms: 90_000,
                critical_confirm_ms: 1,
                ..Default::default()
            }
            .validate(),
            Err(ThresholdError::CriticalBelowWarning),
        );
        assert_eq!(
            AlertThresholds {
                cluster_share_bps: 10_001,
                ..Default::default()
            }
            .validate(),
            Err(ThresholdError::ShareOverWhole),
        );
        assert_eq!(
            AlertThresholds {
                cluster_entropy_micros: 1_000_001,
                ..Default::default()
            }
            .validate(),
            Err(ThresholdError::EntropyOverWhole),
        );
        assert_eq!(
            AlertThresholds {
                cooldown_ms: -1,
                ..Default::default()
            }
            .validate(),
            Err(ThresholdError::NegativeCooldown),
        );
    }

    // -----------------------------------------------------------------------
    // the dispatcher
    // -----------------------------------------------------------------------

    /// Collects what it is given, so a test can look at it.
    #[derive(Default)]
    struct Collector(Mutex<Vec<Alert>>);

    impl Collector {
        fn taken(&self) -> Vec<Alert> {
            self.0.lock().clone()
        }
    }

    impl AlertSink for Collector {
        fn deliver(&self, alert: &Alert) {
            self.0.lock().push(alert.clone());
        }

        fn name(&self) -> &str {
            "collector"
        }
    }

    /// One alert, for the tests that only need something to deliver.
    fn an_alert() -> Alert {
        Alert {
            seq: 1,
            at_ms: AT_MS,
            kind: AlertKind::ExitFailed,
            severity: AlertSeverity::Critical,
            mode: ExecutionMode::Live,
            subject: "t-1".to_string(),
            mint: Some(MINT.to_string()),
            message: "an exit did not land".to_string(),
            observed: 1,
            threshold: 0,
            unit: AlertUnit::Count,
        }
    }

    /// The same, on the telemetry side.
    #[derive(Default)]
    struct Watcher(Mutex<Vec<TelemetryEvent>>);

    impl TelemetrySink for Watcher {
        fn deliver(&self, event: &TelemetryEvent) {
            self.0.lock().push(event.clone());
        }
    }

    fn dispatcher() -> (Arc<AlertDispatcher>, Arc<TelemetryHub>, Arc<Collector>) {
        let hub = Arc::new(TelemetryHub::start());
        let dispatcher = Arc::new(AlertDispatcher::new(Arc::clone(&hub)));
        let collector = Arc::new(Collector::default());
        dispatcher.attach_sink(Arc::clone(&collector) as Arc<dyn AlertSink>);
        (dispatcher, hub, collector)
    }

    #[test]
    fn what_fires_reaches_the_sink_and_carries_a_rising_sequence() {
        let (dispatcher, hub, collector) = dispatcher();
        let first = dispatcher.observe(&filled(&fill_at(900), 300), AT_MS);
        let second = dispatcher.observe(&filled(&fill_at(900), 300), AT_MS + 1_000_000);
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0].seq, 0);
        assert_eq!(second[0].seq, 1);
        assert_eq!(collector.taken(), vec![first[0].clone(), second[0].clone()]);
        hub.shutdown();
    }

    #[test]
    fn the_same_thing_about_the_same_trade_does_not_fire_twice_in_a_row() {
        let (dispatcher, hub, collector) = dispatcher();
        dispatcher
            .set_thresholds(AlertThresholds {
                cooldown_ms: 60_000,
                ..Default::default()
            })
            .expect("valid");

        assert_eq!(
            dispatcher.observe(&filled(&fill_at(900), 300), AT_MS).len(),
            1
        );
        // Inside the cooldown, however many times it happens.
        for offset in [1, 100, 59_999] {
            assert!(dispatcher
                .observe(&filled(&fill_at(900), 300), AT_MS + offset)
                .is_empty());
        }
        // And at the far edge of it, again.
        assert_eq!(
            dispatcher
                .observe(&filled(&fill_at(900), 300), AT_MS + 60_000)
                .len(),
            1
        );

        assert_eq!(collector.taken().len(), 2);
        let snapshot = dispatcher.snapshot();
        assert_eq!(snapshot.raised, 2);
        assert_eq!(
            snapshot.suppressed, 3,
            "the quiet ones are counted, not forgotten"
        );
        hub.shutdown();
    }

    #[test]
    fn a_cooldown_is_per_subject_and_per_kind() {
        let (dispatcher, hub, _collector) = dispatcher();

        let one = Observation::Filled {
            trade_id: "t-1",
            mint: MINT,
            mode: ExecutionMode::Paper,
            fill: &fill_at(900),
            route_bound_bps: 300,
        };
        let two = Observation::Filled {
            trade_id: "t-2",
            mint: MINT,
            mode: ExecutionMode::Paper,
            fill: &fill_at(900),
            route_bound_bps: 300,
        };
        assert_eq!(dispatcher.observe(&one, AT_MS).len(), 1);
        // A different trade is a different subject, so it is not suppressed by
        // the first — which is the whole reason the key is not just the kind.
        assert_eq!(dispatcher.observe(&two, AT_MS).len(), 1);
        assert!(dispatcher.observe(&one, AT_MS + 1).is_empty());

        // And a different kind about the same trade is not suppressed either.
        let late = settled(SignatureStatus::Broadcast, 120_000, 0);
        assert_eq!(dispatcher.observe(&late, AT_MS + 2).len(), 1);
        hub.shutdown();
    }

    #[test]
    fn the_cooldown_history_does_not_grow_forever() {
        let (dispatcher, hub, _collector) = dispatcher();
        dispatcher
            .set_thresholds(AlertThresholds {
                cooldown_ms: 1_000,
                ..Default::default()
            })
            .expect("valid");

        // Past the cap, on subjects that are all long out of their cooldown.
        for index in 0..(COOLDOWN_CAPACITY + 100) {
            let trade = format!("t-{index}");
            let observation = Observation::Filled {
                trade_id: &trade,
                mint: MINT,
                mode: ExecutionMode::Paper,
                fill: &fill_at(900),
                route_bound_bps: 300,
            };
            // Each one a full cooldown after the last, so nothing suppresses
            // anything and every one of them fires.
            dispatcher.observe(&observation, AT_MS + (index as i64) * 2_000);
        }
        assert_eq!(dispatcher.snapshot().suppressed, 0);
        assert!(
            dispatcher
                .fired_at
                .lock()
                .values()
                .map(HashMap::len)
                .sum::<usize>()
                < COOLDOWN_CAPACITY,
            "the history was never pruned",
        );
        hub.shutdown();
    }

    #[test]
    fn lowering_a_threshold_does_not_re_fire_what_it_already_said() {
        let (dispatcher, hub, _collector) = dispatcher();
        assert_eq!(
            dispatcher.observe(&filled(&fill_at(900), 300), AT_MS).len(),
            1
        );
        dispatcher
            .set_thresholds(AlertThresholds {
                slippage_bps: 100,
                ..Default::default()
            })
            .expect("valid");
        assert!(
            dispatcher
                .observe(&filled(&fill_at(900), 300), AT_MS + 1)
                .is_empty(),
            "the cooldown survived the reconfiguration",
        );
        hub.shutdown();
    }

    #[test]
    fn a_threshold_that_will_not_validate_is_not_applied() {
        let (dispatcher, hub, _collector) = dispatcher();
        let before = dispatcher.thresholds();
        let err = dispatcher
            .set_thresholds(AlertThresholds {
                cooldown_ms: -5,
                ..Default::default()
            })
            .expect_err("is refused");
        assert_eq!(err, ThresholdError::NegativeCooldown);
        assert_eq!(
            dispatcher.thresholds(),
            before,
            "the refused set was applied anyway"
        );
        hub.shutdown();
    }

    #[test]
    fn an_alert_reaches_the_telemetry_hub_at_its_own_volume() {
        let hub = Arc::new(TelemetryHub::start());
        let watcher = Arc::new(Watcher::default());
        hub.observe(Arc::clone(&watcher) as Arc<dyn TelemetrySink>);
        let dispatcher = AlertDispatcher::new(Arc::clone(&hub));

        // Critical, so it must arrive at `Error` and not be buried at `Info`.
        dispatcher.observe(&settled(SignatureStatus::Dropped, 1, 0), AT_MS);

        // The hub delivers on its own thread, so this waits for it rather than
        // assuming it has caught up.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while watcher.0.lock().is_empty() && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        let seen = watcher.0.lock().clone();
        assert_eq!(seen.len(), 1, "the alert did not reach the hub");
        assert_eq!(seen[0].source, "alerting");
        assert_eq!(seen[0].level, TelemetryLevel::Error);
        assert_eq!(seen[0].data["kind"], "exitFailed");
        hub.shutdown();
    }

    #[test]
    fn the_snapshot_counts_what_fired_by_kind() {
        let (dispatcher, hub, _collector) = dispatcher();
        dispatcher.observe(&filled(&fill_at(900), 300), AT_MS);
        dispatcher.observe(&settled(SignatureStatus::Dropped, 1, 0), AT_MS);
        dispatcher.observe(&settled(SignatureStatus::Broadcast, 120_000, 9), AT_MS);

        let snapshot = dispatcher.snapshot();
        assert_eq!(snapshot.raised, 4);
        let counted = |kind: AlertKind| {
            snapshot
                .by_kind
                .iter()
                .find(|k| k.kind == kind)
                .map(|k| k.raised)
                .unwrap_or_default()
        };
        assert_eq!(counted(AlertKind::SlippageSpike), 1);
        assert_eq!(counted(AlertKind::ExitFailed), 1);
        assert_eq!(counted(AlertKind::ConfirmationLate), 1);
        assert_eq!(counted(AlertKind::RebroadcastStorm), 1);
        assert_eq!(counted(AlertKind::TipOverrun), 0);
        assert_eq!(
            snapshot.by_kind.len(),
            AlertKind::ALL.len(),
            "every kind is listed"
        );
        hub.shutdown();
    }

    #[test]
    fn an_alert_survives_the_trip_it_crosses_ipc_and_the_wire_as() {
        let alert = Alert {
            seq: 7,
            at_ms: AT_MS,
            kind: AlertKind::SlippageSpike,
            severity: AlertSeverity::Critical,
            mode: ExecutionMode::Live,
            subject: "t-1".to_string(),
            mint: Some(MINT.to_string()),
            message: "a fill came in badly".to_string(),
            observed: 1_800,
            threshold: 500,
            unit: AlertUnit::BasisPoints,
        };
        let json = serde_json::to_string(&alert).expect("serialises");
        assert_eq!(serde_json::from_str::<Alert>(&json).expect("reads"), alert);
        // Every number in it is an integer. A `.` in the JSON would be a float
        // that got in, which is the thing the module header promises against.
        let value: serde_json::Value = serde_json::from_str(&json).expect("parses");
        for field in ["seq", "atMs", "observed", "threshold"] {
            assert!(
                value[field].is_i64() || value[field].is_u64(),
                "{field} is not an integer"
            );
        }
    }

    // -----------------------------------------------------------------------
    // webhooks
    // -----------------------------------------------------------------------

    #[test]
    fn a_url_parses_into_what_gets_dialled() {
        let plain = WebhookTarget::parse("http://alerts.internal/hook").expect("parses");
        assert_eq!(
            plain,
            WebhookTarget {
                host: "alerts.internal".into(),
                port: 80,
                path: "/hook".into(),
                tls: false,
            }
        );
        assert_eq!(plain.as_url(), "http://alerts.internal/hook");

        let tls =
            WebhookTarget::parse("https://hooks.example.com/services/a/b?x=1").expect("parses");
        assert_eq!(tls.port, 443);
        assert!(tls.tls);
        assert_eq!(
            tls.path, "/services/a/b?x=1",
            "the query is part of the path"
        );

        let ported = WebhookTarget::parse("http://127.0.0.1:9099").expect("parses");
        assert_eq!(ported.port, 9099);
        assert_eq!(ported.path, "/", "a url with no path posts to the root");
        assert_eq!(ported.as_url(), "http://127.0.0.1:9099/");
    }

    #[test]
    fn a_url_this_will_not_dial_is_refused_at_configuration_time() {
        assert_eq!(
            WebhookTarget::parse("ftp://example.com/x"),
            Err(WebhookError::UnsupportedScheme("ftp".into())),
        );
        assert_eq!(
            WebhookTarget::parse("example.com/x"),
            Err(WebhookError::UnsupportedScheme("example.com/x".into())),
        );
        assert_eq!(WebhookTarget::parse("http:///x"), Err(WebhookError::NoHost));
        assert_eq!(
            WebhookTarget::parse("http://example.com:0/x"),
            Err(WebhookError::BadPort("0".into())),
        );
        assert_eq!(
            WebhookTarget::parse("http://example.com:https/x"),
            Err(WebhookError::BadPort("https".into())),
        );
    }

    #[test]
    fn a_status_line_is_read_and_anything_else_is_not() {
        assert_eq!(parse_status(b"HTTP/1.1 204 No Content\r\n\r\n"), Some(204));
        assert_eq!(
            parse_status(b"HTTP/1.0 500 Internal Server Error\r\n"),
            Some(500)
        );
        assert_eq!(parse_status(b"nonsense\r\n"), None);
        assert_eq!(parse_status(b""), None);
        assert_eq!(parse_status(b"HTTP/1.1 notanumber\r\n"), None);
    }

    #[test]
    fn an_alert_arrives_at_the_endpoint_as_a_json_post() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
        let address = listener.local_addr().expect("has an address");
        let (tx, rx) = bounded::<String>(1);

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accepts");
            let mut reader = std::io::BufReader::new(stream.try_clone().expect("clones"));
            let mut request = String::new();
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).expect("reads") == 0 {
                    break;
                }
                if let Some(value) = line.strip_prefix("Content-Length: ") {
                    length = value.trim().parse().expect("a length");
                }
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0u8; length];
            reader.read_exact(&mut body).expect("reads the body");
            request.push_str(std::str::from_utf8(&body).expect("utf8"));

            let mut stream = stream;
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .expect("answers");
            let _ = tx.send(request);
        });

        let hub = Arc::new(TelemetryHub::start());
        let dispatcher = AlertDispatcher::new(Arc::clone(&hub));
        let sink = dispatcher
            .attach_webhook(&WebhookConfig {
                url: format!("http://{address}/hook"),
                timeout_ms: 5_000,
                queue_depth: 8,
                ..WebhookConfig::default()
            })
            .expect("starts");

        let fired = dispatcher.observe(&settled(SignatureStatus::Dropped, 1, 0), AT_MS);
        assert_eq!(fired.len(), 1);

        let request = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the post arrived");
        server.join().expect("the server did not panic");

        assert!(request.starts_with("POST /hook HTTP/1.1\r\n"), "{request}");
        assert!(
            request.contains(&format!("Host: {}\r\n", address.ip())),
            "{request}"
        );
        assert!(
            request.contains("Content-Type: application/json\r\n"),
            "{request}"
        );
        assert!(request.contains("Connection: close\r\n"), "{request}");

        let body = request.split("\r\n\r\n").nth(1).expect("has a body");
        let received: Alert = serde_json::from_str(body).expect("is an alert");
        assert_eq!(received, fired[0], "what fired is not what arrived");

        // The worker counts what the far end said.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while sink.stats().delivered == 0 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        let stats = sink.stats();
        assert_eq!(stats.queued, 1);
        assert_eq!(stats.delivered, 1);
        assert_eq!(stats.dropped, 0);
        assert_eq!(stats.failed, 0);

        dispatcher.shutdown();
        hub.shutdown();
    }

    #[test]
    fn a_webhook_with_nowhere_to_put_an_alert_drops_it_rather_than_blocking() {
        let sink = WebhookSink::start(&WebhookConfig {
            // Nothing is listening, and nothing needs to be: the worker is
            // stopped before a single alert is offered, so the queue has no
            // reader at all. That is the same condition as a full queue from
            // the engine's side, and it is the one that can be arranged without
            // depending on how fast anything runs.
            url: "http://127.0.0.1:9/never".to_string(),
            timeout_ms: 10,
            queue_depth: 1,
            ..WebhookConfig::default()
        })
        .expect("starts");
        sink.stop();

        let alert = Alert {
            seq: 0,
            at_ms: AT_MS,
            kind: AlertKind::TipOverrun,
            severity: AlertSeverity::Critical,
            mode: ExecutionMode::Paper,
            subject: "t-1".to_string(),
            mint: None,
            message: "over".to_string(),
            observed: 2,
            threshold: 1,
            unit: AlertUnit::Lamports,
        };
        for _ in 0..16 {
            sink.deliver(&alert);
        }
        let stats = sink.stats();
        assert_eq!(
            stats.dropped, 16,
            "an alert waited instead of being dropped"
        );
        assert_eq!(stats.delivered, 0);
    }

    #[test]
    fn a_webhook_that_will_not_parse_never_starts_a_thread() {
        let hub = Arc::new(TelemetryHub::start());
        let dispatcher = AlertDispatcher::new(Arc::clone(&hub));
        let err = dispatcher
            .attach_webhook(&WebhookConfig {
                url: "wss://nope/x".into(),
                ..Default::default()
            })
            .expect_err("is refused");
        assert_eq!(err, WebhookError::UnsupportedScheme("wss".into()));
        assert!(dispatcher.snapshot().webhooks.is_empty());
        hub.shutdown();
    }

    // -- retry, backoff and isolation ---------------------------------------

    /// An endpoint that answers each connection from a script, in order, and
    /// then repeats the last answer forever.
    ///
    /// One thread and one connection at a time, which is what the worker does:
    /// `Connection: close` and a fresh socket per attempt, so "the second
    /// connection" and "the second attempt" are the same thing and the script
    /// reads as the sequence of answers the endpoint gave.
    fn scripted_endpoint(answers: Vec<&'static str>) -> (String, Arc<AtomicU64>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("binds");
        let address = listener.local_addr().expect("has an address");
        let connections = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&connections);
        let server = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let index = counter.fetch_add(1, Ordering::SeqCst) as usize;
                // Read the request head so the client's write always completes.
                let mut reader =
                    std::io::BufReader::new(stream.try_clone().expect("the socket clones"));
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if line == "\r\n" {
                        break;
                    }
                    line.clear();
                }
                let answer = answers[index.min(answers.len() - 1)];
                if answer.is_empty() {
                    // "the endpoint hung up without answering", which is a
                    // failure to read a status line rather than a status.
                    drop(stream);
                    continue;
                }
                let _ = stream.write_all(answer.as_bytes());
                let _ = stream.flush();
                drop(stream);
            }
        });
        (format!("http://{address}/hook"), connections, server)
    }

    /// Spins until the sink's counters say it is done with this alert, or gives
    /// up. Returns the stats either way, so the assertion is the caller's.
    fn settle(sink: &WebhookSink, alerts: u64) -> WebhookStats {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let stats = sink.stats();
            if stats.delivered + stats.failed + stats.shed >= alerts {
                return stats;
            }
            if std::time::Instant::now() >= deadline {
                return stats;
            }
            std::thread::yield_now();
        }
    }

    #[test]
    fn the_backoff_doubles_from_its_first_wait_and_stops_at_its_ceiling() {
        let policy = WebhookConfig {
            initial_backoff_ms: 250,
            backoff_factor: 2,
            max_backoff_ms: 1_000,
            ..Default::default()
        };
        assert_eq!(policy.backoff_ms(0), 250);
        assert_eq!(policy.backoff_ms(1), 500);
        assert_eq!(policy.backoff_ms(2), 1_000);
        assert_eq!(policy.backoff_ms(3), 1_000, "it stops at the ceiling");
        assert_eq!(policy.backoff_ms(64), 1_000, "and does not wrap past it");
    }

    #[test]
    fn a_backoff_factor_of_zero_is_a_flat_retry_rather_than_no_wait_at_all() {
        // The saturation `BroadcastPolicy` argues for, restated: a backoff that
        // collapses to nothing under a misconfigured factor is a retry loop
        // that hammers a struggling endpoint as fast as it can.
        let flat = WebhookConfig {
            initial_backoff_ms: 300,
            backoff_factor: 0,
            max_backoff_ms: 5_000,
            ..Default::default()
        };
        assert_eq!(flat.backoff_ms(0), 300);
        assert_eq!(flat.backoff_ms(5), 300);
    }

    #[test]
    fn the_worst_case_is_every_timeout_plus_every_wait_between_them() {
        let policy = WebhookConfig {
            timeout_ms: 1_000,
            max_retries: 2,
            initial_backoff_ms: 100,
            backoff_factor: 2,
            max_backoff_ms: 5_000,
            ..Default::default()
        };
        // Three attempts at a second each, and 100ms + 200ms between them.
        assert_eq!(policy.worst_case_ms(), 3_000 + 300);
    }

    #[test]
    fn a_failure_that_might_not_repeat_goes_out_again() {
        // Two 503s and then a 204. The alert is delivered once — `delivered`
        // counts alerts, not attempts — and `retried` is what says it took
        // three goes.
        let (url, connections, server) = scripted_endpoint(vec![
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
            "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n",
        ]);
        let sink = WebhookSink::start(&WebhookConfig {
            url,
            timeout_ms: 2_000,
            max_retries: 3,
            initial_backoff_ms: 1,
            max_backoff_ms: 2,
            ..Default::default()
        })
        .expect("starts");

        sink.deliver(&an_alert());
        let stats = settle(&sink, 1);
        assert_eq!(stats.delivered, 1, "a 503 is not a refusal: {stats:?}");
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.retried, 2, "two retries after the first attempt");
        assert!(
            !stats.breaker_open,
            "one recovered alert is not a down endpoint"
        );
        assert_eq!(connections.load(Ordering::SeqCst), 3);

        sink.stop();
        drop(server);
    }

    #[test]
    fn an_endpoint_that_reads_the_request_and_says_no_is_not_asked_twice() {
        // A 404 is the endpoint answering. The identical bytes will get the
        // identical answer, so sending them again buys nothing and costs the
        // alerts queued behind this one.
        let (url, connections, server) =
            scripted_endpoint(vec!["HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"]);
        let sink = WebhookSink::start(&WebhookConfig {
            url,
            timeout_ms: 2_000,
            max_retries: 5,
            initial_backoff_ms: 1,
            ..Default::default()
        })
        .expect("starts");

        sink.deliver(&an_alert());
        let stats = settle(&sink, 1);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.delivered, 0);
        assert_eq!(stats.retried, 0, "a refusal was retried");
        assert_eq!(
            connections.load(Ordering::SeqCst),
            1,
            "one attempt, one connection"
        );

        sink.stop();
        drop(server);
    }

    #[test]
    fn the_two_four_hundreds_that_mean_come_back_later_are_retried() {
        // 408 and 429 are the endpoint saying the request was fine and the
        // moment was not, which is the one case a 4xx is worth repeating.
        assert!(matches!(attempt_from_status(429), Attempt::Retryable(_)));
        assert!(matches!(attempt_from_status(408), Attempt::Retryable(_)));
        assert_eq!(attempt_from_status(404), Attempt::Refused(404));
        assert_eq!(attempt_from_status(401), Attempt::Refused(401));
        assert_eq!(attempt_from_status(204), Attempt::Delivered);
        assert!(matches!(attempt_from_status(500), Attempt::Retryable(_)));
    }

    #[test]
    fn an_endpoint_that_is_down_trips_the_breaker_and_stops_being_dialled() {
        // Port 9 is discard: nothing accepts, so every attempt fails to
        // connect. Without the breaker each alert would cost every retry and
        // every backoff, and the alerts behind it would age by that much before
        // anything looked at them.
        let sink = WebhookSink::start(&WebhookConfig {
            url: "http://127.0.0.1:9/never".to_string(),
            timeout_ms: 30,
            queue_depth: 64,
            max_retries: 0,
            failures_before_open: 3,
            breaker_cooldown_ms: 60_000,
            ..Default::default()
        })
        .expect("starts");

        for _ in 0..3 {
            sink.deliver(&an_alert());
        }
        let opened = settle(&sink, 3);
        assert_eq!(opened.failed, 3);
        assert!(
            opened.breaker_open,
            "three in a row and it is still dialling: {opened:?}"
        );
        assert_eq!(opened.breaker_trips, 1);

        // Everything after this is counted and dropped without a socket being
        // opened at all.
        for _ in 0..10 {
            sink.deliver(&an_alert());
        }
        let shed = settle(&sink, 13);
        assert_eq!(
            shed.shed, 10,
            "the breaker is open and it kept dialling: {shed:?}"
        );
        assert_eq!(shed.failed, 3, "no further attempt was made");
        assert!(sink.is_breaker_open());

        sink.stop();
    }

    #[test]
    fn the_breaker_lets_one_through_when_its_cooldown_is_up() {
        // A cooldown of nothing, so the probe happens on the next alert. What
        // the probe does is what decides whether the breaker shuts.
        let (url, _connections, server) =
            scripted_endpoint(vec!["HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n"]);
        let sink = WebhookSink::start(&WebhookConfig {
            url,
            timeout_ms: 2_000,
            max_retries: 0,
            failures_before_open: 1,
            breaker_cooldown_ms: 0,
            ..Default::default()
        })
        .expect("starts");

        // The endpoint answers, so nothing ever trips. The property under test
        // is the recovery path, and the way to reach it without sleeping is a
        // cooldown that has already expired by the time the next alert arrives.
        for _ in 0..4 {
            sink.deliver(&an_alert());
        }
        let stats = settle(&sink, 4);
        assert_eq!(stats.delivered, 4, "{stats:?}");
        assert_eq!(stats.shed, 0);
        assert!(!stats.breaker_open);
        assert_eq!(stats.consecutive_failures, 0, "a delivery resets the count");

        sink.stop();
        drop(server);
    }

    #[test]
    fn stopping_does_not_wait_out_a_backoff_that_is_in_progress() {
        // The reason the backoff is a `recv_timeout` on the shutdown channel
        // rather than a `sleep`. A worker asleep in a thirty-second wait would
        // make every shutdown thirty seconds long, and a shutdown that waits on
        // a dead endpoint is this whole file's failure mode arriving at the
        // last possible moment.
        let sink = WebhookSink::start(&WebhookConfig {
            url: "http://127.0.0.1:9/never".to_string(),
            timeout_ms: 30,
            max_retries: 4,
            initial_backoff_ms: 30_000,
            max_backoff_ms: 30_000,
            failures_before_open: 0,
            ..Default::default()
        })
        .expect("starts");

        sink.deliver(&an_alert());
        // Long enough for the connect to fail and the first backoff to begin.
        std::thread::sleep(Duration::from_millis(200));

        let started = std::time::Instant::now();
        sink.stop();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "stopping took {elapsed:?}, so it waited out the backoff",
        );
    }

    #[test]
    fn a_sink_that_panics_does_not_take_the_engine_thread_with_it() {
        // `AlertSink` is a public trait and "does not block, does not panic" is
        // a sentence in a doc comment rather than something the type system
        // holds anyone to. `dispatch` runs on the thread that was in the middle
        // of an exit, so an implementation that panics must not unwind through
        // it.
        struct Detonator;
        impl AlertSink for Detonator {
            fn deliver(&self, _alert: &Alert) {
                panic!("this sink is broken");
            }
            fn name(&self) -> &str {
                "detonator"
            }
        }

        let hub = Arc::new(TelemetryHub::start());
        let dispatcher = AlertDispatcher::new(Arc::clone(&hub));
        let collector = Arc::new(Collector::default());
        dispatcher.attach_sink(Arc::new(Detonator) as Arc<dyn AlertSink>);
        dispatcher.attach_sink(Arc::clone(&collector) as Arc<dyn AlertSink>);

        // The panic message on stderr is the point of the test, not a problem
        // with it; silencing the hook keeps the suite's output readable.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let fired = dispatcher.observe(&settled(SignatureStatus::Failed, 1, 0), AT_MS);
        std::panic::set_hook(hook);

        assert_eq!(fired.len(), 1, "the observation itself still returned");
        assert_eq!(
            collector.0.lock().len(),
            1,
            "the sink after the broken one was skipped"
        );
        assert_eq!(
            dispatcher.snapshot().sink_panics,
            1,
            "and it was counted rather than hidden"
        );

        dispatcher.shutdown();
        hub.shutdown();
    }

    #[test]
    fn a_signature_kind_is_not_part_of_an_alert_but_the_journal_types_still_line_up() {
        // The alerting module reads `journal`'s status enum and nothing else of
        // it. This is the compile-time check that the two agree, kept as a test
        // so a change to either is caught here rather than in a call site.
        assert!(SignatureStatus::Broadcast.is_in_flight());
        assert!(!SignatureStatus::Confirmed.is_in_flight());
        assert_eq!(SignatureKind::Exit.as_str(), "exit");
    }
}
