//! Where chain data enters the process.
//!
//! Several provider sockets run at once, all of them saying roughly the same
//! thing at slightly different times, and this module turns that into one
//! ordered stream of things worth looking at. Four ideas shape it.
//!
//! **Nothing broad is ever subscribed to.** The socket asks for pump.fun and
//! Raydium accounts by program id and by account size, so the filtering starts
//! at the provider rather than here. That is a quota decision as much as a CPU
//! one: the free tiers this runs on are metered by what they send.
//!
//! **A frame is rejected before it is parsed.** `serde_json` on a 2 KB frame
//! costs more than the scan that proves the frame is spam, so `StreamFilters`
//! searches the raw bytes for an allowlisted program id first and throws away
//! everything that does not mention one. Base58 keys travel through JSON as
//! plain ASCII, so this is an honest substring search, not a heuristic.
//!
//! **The clock starts when the frame lands.** Every dispatch is timed from the
//! moment the socket hands over bytes to the moment a candidate is in the
//! channel, and anything past `DISPATCH_BUDGET` is counted. The budget is a
//! measurement, not a promise: `IngestionSnapshot` reports how often it was
//! missed rather than the code pretending it never is.
//!
//! **Falling behind is counted, never blocking.** Every channel out of here is
//! bounded and every send is a `try_send`. If the engine downstream stalls, the
//! newest candidate is dropped and the drop shows up in telemetry. A socket
//! task that blocks on a full channel stops reading the socket, and a provider
//! whose reads stall disconnects — one slow consumer would take out the feed.
//!
//! With no provider URLs in the environment there are no endpoints, nothing
//! dials, and the manager idles. That is the intended default: the roadmap's
//! Phase 0 gate says no real feed is opened until the fixtures pass, so opening
//! one has to be a deliberate act of configuration.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

use crate::db::{Database, IngestCandidateRow};
use crate::telemetry::{now_ms, TelemetryHub, TelemetryLevel};
use crate::types::{LiquidityThresholds, Pubkey, TokenCandidate, BPS_DENOMINATOR};

// ---------------------------------------------------------------------------
// budgets and sizes
// ---------------------------------------------------------------------------

/// What "receipt to internal dispatch" is allowed to cost. Every frame is timed
/// against this and the misses are counted; see `IngestionSnapshot::over_budget`.
pub const DISPATCH_BUDGET: Duration = Duration::from_millis(2);

/// How many candidates may be waiting on the fast path. Deliberately shallow:
/// the fast path exists to act on something while it is still moving, and a
/// deep queue there would only be a way to act on stale things later.
pub const FAST_PATH_DEPTH: usize = 256;

/// How many candidates may be waiting on the ordinary path. Deeper, because
/// nothing on it is time-critical and the scoring pass runs in bursts.
pub const STANDARD_DEPTH: usize = 4096;

/// How many rows may be waiting for the WAL writer. Deeper still: SQLite writes
/// in bursts of its own, and losing the record of a candidate the engine acted
/// on is worse than losing the candidate.
const WAL_DEPTH: usize = 8192;

/// How many rows the WAL worker will put in one transaction.
const WAL_BATCH: usize = 128;

/// How long the WAL worker waits for a batch to fill before writing what it has.
const WAL_LINGER: Duration = Duration::from_millis(250);

/// How long a socket may say nothing at all before it is treated as dead.
/// Longer than the heartbeat, so a quiet market is not mistaken for a dead
/// socket, and shorter than a person would take to notice.
const IDLE_TIMEOUT: Duration = Duration::from_secs(45);

/// How often a ping goes out. The pong that comes back is the only round-trip
/// measurement a pubsub socket offers, and it doubles as the liveness check.
const HEARTBEAT: Duration = Duration::from_secs(15);

/// Reconnect backoff: doubles from the first up to the second, per endpoint.
const BACKOFF_MIN: Duration = Duration::from_millis(500);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// How many mints the launch index remembers. At a few launches a second this
/// is several hours of history, which is far longer than anything the engine
/// still cares about, and it is a fixed ceiling rather than a growing map.
const LAUNCH_INDEX_CAPACITY: usize = 16_384;

/// How many latency samples make up an endpoint's health picture.
const LATENCY_SAMPLES: usize = 64;

/// Lamports in one SOL.
pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

/// Micro-dollars in one cent. Prices are carried in micro-dollars so a cent is
/// still four digits of headroom away from a rounding error.
const MICRO_USD_PER_CENT: u64 = 10_000;

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

/// Anything a feed can fail with. Cloneable, because one failure is reported to
/// the pool, to telemetry and to the reconnect loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
pub enum IngestError {
    /// The endpoint could not be reached at all.
    Dial(String),
    /// The socket was up and then was not.
    Socket(String),
    /// The socket is up and sending something this code does not understand.
    Protocol(String),
    /// The far end closed cleanly. Not an error, but it ends a read loop.
    Closed,
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestError::Dial(m) => write!(f, "dial: {m}"),
            IngestError::Socket(m) => write!(f, "socket: {m}"),
            IngestError::Protocol(m) => write!(f, "protocol: {m}"),
            IngestError::Closed => f.write_str("the far end closed the stream"),
        }
    }
}

impl std::error::Error for IngestError {}

// ---------------------------------------------------------------------------
// programs worth listening to
// ---------------------------------------------------------------------------

/// pump.fun's bonding curve program.
pub const PUMP_FUN_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
/// PumpSwap, where a pump.fun token goes once its curve completes.
pub const PUMP_SWAP_PROGRAM: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
/// Raydium's V4 constant-product AMM.
pub const RAYDIUM_AMM_V4_PROGRAM: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
/// Raydium's constant-product pools.
pub const RAYDIUM_CPMM_PROGRAM: &str = "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C";
/// Raydium's concentrated liquidity pools.
pub const RAYDIUM_CLMM_PROGRAM: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";

// The decoded form of each of the five, so the hot path compares 32 bytes
// instead of parsing base58. `program_ids_match_their_text` re-derives every one
// of these from the string above it, which is what makes the numbers checkable.
const PUMP_FUN_BYTES: [u8; 32] = [
    0x01, 0x56, 0xe0, 0xf6, 0x93, 0x66, 0x5a, 0xcf, 0x44, 0xdb, 0x15, 0x68, 0xbf, 0x17, 0x5b, 0xaa,
    0x51, 0x89, 0xcb, 0x97, 0xf5, 0xd2, 0xff, 0x3b, 0x65, 0x5d, 0x2b, 0xb6, 0xfd, 0x6d, 0x18, 0xb0,
];
const PUMP_SWAP_BYTES: [u8; 32] = [
    0x0c, 0x14, 0xde, 0xfc, 0x82, 0x5e, 0xc6, 0x76, 0x94, 0x25, 0x08, 0x18, 0xbb, 0x65, 0x40, 0x65,
    0xf4, 0x29, 0x8d, 0x31, 0x56, 0xd5, 0x71, 0xb4, 0xd4, 0xf8, 0x09, 0x0c, 0x18, 0xe9, 0xa8, 0x63,
];
const RAYDIUM_AMM_V4_BYTES: [u8; 32] = [
    0x4b, 0xd9, 0x49, 0xc4, 0x36, 0x02, 0xc3, 0x3f, 0x20, 0x77, 0x90, 0xed, 0x16, 0xa3, 0x52, 0x4c,
    0xa1, 0xb9, 0x97, 0x5c, 0xf1, 0x21, 0xa2, 0xa9, 0x0c, 0xff, 0xec, 0x7d, 0xf8, 0xb6, 0x8a, 0xcd,
];
const RAYDIUM_CPMM_BYTES: [u8; 32] = [
    0xa9, 0x2a, 0x5a, 0x8b, 0x4f, 0x29, 0x59, 0x52, 0x84, 0x25, 0x50, 0xaa, 0x93, 0xfd, 0x5b, 0x95,
    0xb5, 0xac, 0xe6, 0xa8, 0xeb, 0x92, 0x0c, 0x93, 0x94, 0x2e, 0x43, 0x69, 0x0c, 0x20, 0xec, 0x73,
];
const RAYDIUM_CLMM_BYTES: [u8; 32] = [
    0xa5, 0xd5, 0xca, 0x9e, 0x04, 0xcf, 0x5d, 0xb5, 0x90, 0xb7, 0x14, 0xba, 0x2f, 0xe3, 0x2c, 0xb1,
    0x59, 0x13, 0x3f, 0xc1, 0xc1, 0x92, 0xb7, 0x22, 0x57, 0xfd, 0x07, 0xd3, 0x9c, 0xb0, 0x40, 0x1e,
];

/// The five programs, as text and as bytes, in one place.
///
/// The text is what goes into a subscription request and what the pre-filter
/// searches raw frames for; the key is what the parsed comparison uses.
pub struct AllowedProgram {
    pub text: &'static str,
    pub key: Pubkey,
}

/// Every program this engine will accept an event from.
///
/// It is a fixed array rather than a configurable list on purpose. An allowlist
/// somebody can widen at runtime is not an allowlist, and the roadmap's Phase 1
/// gate is specifically that an unallowlisted account is rejected before
/// transmission.
pub const ALLOWED_PROGRAMS: [AllowedProgram; 5] = [
    AllowedProgram {
        text: PUMP_FUN_PROGRAM,
        key: Pubkey::new(PUMP_FUN_BYTES),
    },
    AllowedProgram {
        text: PUMP_SWAP_PROGRAM,
        key: Pubkey::new(PUMP_SWAP_BYTES),
    },
    AllowedProgram {
        text: RAYDIUM_AMM_V4_PROGRAM,
        key: Pubkey::new(RAYDIUM_AMM_V4_BYTES),
    },
    AllowedProgram {
        text: RAYDIUM_CPMM_PROGRAM,
        key: Pubkey::new(RAYDIUM_CPMM_BYTES),
    },
    AllowedProgram {
        text: RAYDIUM_CLMM_PROGRAM,
        key: Pubkey::new(RAYDIUM_CLMM_BYTES),
    },
];

/// Whether this program is one of the five.
pub fn is_allowed_program(key: &Pubkey) -> bool {
    ALLOWED_PROGRAMS.iter().any(|p| &p.key == key)
}

// ---------------------------------------------------------------------------
// providers and endpoints
// ---------------------------------------------------------------------------

/// Who is sending the data.
///
/// Kept as an enum rather than a string because it is half of the deduplication
/// identity and a typo in a provider name would silently turn one event into
/// two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FeedProvider {
    Helius,
    QuickNode,
    Triton,
}

impl FeedProvider {
    pub const ALL: [FeedProvider; 3] = [
        FeedProvider::Helius,
        FeedProvider::QuickNode,
        FeedProvider::Triton,
    ];

    /// Where this provider sits in [`FeedProvider::ALL`], which is what indexes
    /// the per-provider arrays in this module.
    ///
    /// Written out rather than found by searching `ALL`, because it is read on
    /// the hot path, and matched exhaustively so that a fourth provider is a
    /// compile error here rather than an out-of-bounds index somewhere else.
    pub const fn index(self) -> usize {
        match self {
            FeedProvider::Helius => 0,
            FeedProvider::QuickNode => 1,
            FeedProvider::Triton => 2,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            FeedProvider::Helius => "helius",
            FeedProvider::QuickNode => "quicknode",
            FeedProvider::Triton => "triton",
        }
    }

    /// The environment variable holding this provider's stream URL.
    ///
    /// Credentials live in the URL for all three providers, which is why they
    /// are read from the environment and never written to a config file in the
    /// repository or logged in full.
    pub const fn url_var(self) -> &'static str {
        match self {
            FeedProvider::Helius => "STS_HELIUS_STREAM_URL",
            FeedProvider::QuickNode => "STS_QUICKNODE_STREAM_URL",
            FeedProvider::Triton => "STS_TRITON_STREAM_URL",
        }
    }
}

impl std::fmt::Display for FeedProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How the stream is carried.
///
/// Both are pubsub over one long-lived connection and both produce the same
/// notification shape, so the manager treats them identically and only the
/// dialer differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FeedTransport {
    /// Solana JSON-RPC pubsub over WSS. What all three providers offer on the
    /// free tier, and the only transport `WebSocketDialer` speaks.
    WebSocket,
    /// A Geyser gRPC stream. Recognised, configurable, and not dialable by the
    /// dialer in this file — see `WebSocketDialer::dial`.
    Grpc,
}

impl FeedTransport {
    pub const fn as_str(self) -> &'static str {
        match self {
            FeedTransport::WebSocket => "websocket",
            FeedTransport::Grpc => "grpc",
        }
    }

    /// Guessed from the URL scheme, so one environment variable configures both
    /// the address and the transport.
    pub fn from_url(url: &str) -> Self {
        if url.starts_with("http://") || url.starts_with("https://") || url.starts_with("grpc") {
            FeedTransport::Grpc
        } else {
            FeedTransport::WebSocket
        }
    }
}

/// One place to connect to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointConfig {
    pub provider: FeedProvider,
    pub transport: FeedTransport,
    /// The full URL, credentials included. Never serialised — see `redacted`.
    pub url: String,
    /// How much of the load-balanced share this endpoint takes when its latency
    /// ties with another's. Zero means "failover only": it is never picked while
    /// anything else is healthy.
    pub weight: u16,
}

impl EndpointConfig {
    pub fn new(provider: FeedProvider, url: impl Into<String>, weight: u16) -> Self {
        let url = url.into();
        Self {
            provider,
            transport: FeedTransport::from_url(&url),
            url,
            weight,
        }
    }

    /// The URL with everything after the host removed.
    ///
    /// Every provider puts the API key in the path or the query string, so the
    /// whole URL is a credential. This is what goes in telemetry and audit rows.
    pub fn redacted(&self) -> String {
        let (scheme, rest) = match self.url.split_once("://") {
            Some((scheme, rest)) => (scheme, rest),
            None => ("", self.url.as_str()),
        };
        let host = rest.split(['/', '?']).next().unwrap_or(rest);
        if scheme.is_empty() {
            host.to_string()
        } else {
            format!("{scheme}://{host}/…")
        }
    }
}

/// Reads the configured endpoints out of the environment.
///
/// An empty result is the normal state of a checkout that has never been given
/// credentials, and it is why starting the manager is safe by default: no URL,
/// no endpoint, no socket.
pub fn endpoints_from_env() -> Vec<EndpointConfig> {
    FeedProvider::ALL
        .iter()
        .filter_map(|&provider| {
            let url = std::env::var(provider.url_var()).ok()?;
            let url = url.trim();
            if url.is_empty() {
                return None;
            }
            Some(EndpointConfig::new(provider, url, 1))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// endpoint health
// ---------------------------------------------------------------------------

/// How well an endpoint is keeping up.
///
/// The bands are the ones the roadmap's Phase 1 gate names: healthy is p50
/// within 120 ms and p95 within 350 ms, degraded is p95 within 500 ms, and
/// anything worse — or anything that keeps failing to connect — is unhealthy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum EndpointHealth {
    /// Never connected, or connected and not yet measured.
    Unknown,
    Healthy,
    Degraded,
    Unhealthy,
}

impl EndpointHealth {
    /// Whether the pool will route a request here. Unknown counts as usable:
    /// an endpoint has to be tried before it can be measured.
    pub const fn is_usable(self) -> bool {
        !matches!(self, EndpointHealth::Unhealthy)
    }
}

const HEALTHY_P50_MS: u32 = 120;
const HEALTHY_P95_MS: u32 = 350;
const DEGRADED_P95_MS: u32 = 500;

/// The last `LATENCY_SAMPLES` round trips, and the percentiles over them.
///
/// A ring buffer rather than a running average because the number that matters
/// is p95, and an average hides exactly the tail that decides whether a fill
/// arrives in time.
#[derive(Debug, Clone)]
struct LatencyWindow {
    samples: [u32; LATENCY_SAMPLES],
    len: usize,
    next: usize,
}

impl LatencyWindow {
    const fn new() -> Self {
        Self {
            samples: [0; LATENCY_SAMPLES],
            len: 0,
            next: 0,
        }
    }

    fn record(&mut self, millis: u32) {
        self.samples[self.next] = millis;
        self.next = (self.next + 1) % LATENCY_SAMPLES;
        self.len = (self.len + 1).min(LATENCY_SAMPLES);
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The two nearest-rank percentiles the health bands are written in, sorted
    /// on the stack.
    ///
    /// One call rather than two, and a fixed array rather than a `Vec`, because
    /// this runs behind the pool's lock every time an endpoint is chosen or the
    /// status is rendered — and a heap allocation to read a counter is the kind
    /// of cost that only shows up when everything else is already going wrong.
    fn percentiles(&self) -> (u32, u32) {
        if self.len == 0 {
            return (0, 0);
        }
        let mut buffer = [0u32; LATENCY_SAMPLES];
        let sorted = &mut buffer[..self.len];
        sorted.copy_from_slice(&self.samples[..self.len]);
        sorted.sort_unstable();
        // Nearest rank: ceil(p/100 * n), clamped into the slice.
        let at = |percentile: usize| -> u32 {
            let rank = (percentile * self.len).div_ceil(100);
            sorted[rank.saturating_sub(1).min(self.len - 1)]
        };
        (at(50), at(95))
    }

    fn health(&self) -> EndpointHealth {
        if self.is_empty() {
            return EndpointHealth::Unknown;
        }
        let (p50, p95) = self.percentiles();
        if p50 <= HEALTHY_P50_MS && p95 <= HEALTHY_P95_MS {
            EndpointHealth::Healthy
        } else if p95 <= DEGRADED_P95_MS {
            EndpointHealth::Degraded
        } else {
            EndpointHealth::Unhealthy
        }
    }
}

// ---------------------------------------------------------------------------
// the endpoint pool: failover and load balancing
// ---------------------------------------------------------------------------

/// How many connection failures in a row before an endpoint is called unhealthy
/// regardless of what its latency window says. Three, because one is a blip and
/// two is a coincidence.
const FAILURES_TO_UNHEALTHY: u32 = 3;

/// What one endpoint is doing right now.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointStatus {
    pub provider: FeedProvider,
    pub transport: FeedTransport,
    /// Host only. The rest of the URL is a credential.
    pub url: String,
    pub health: EndpointHealth,
    pub connected: bool,
    pub latency_p50_ms: u32,
    pub latency_p95_ms: u32,
    pub consecutive_failures: u32,
    pub connects: u64,
    pub frames: u64,
    /// How much of the backoff is left, or 0 if it is free to be tried now.
    pub backoff_remaining_ms: u64,
    pub last_frame_at_ms: Option<i64>,
}

#[derive(Debug)]
struct EndpointSlot {
    config: EndpointConfig,
    latency: LatencyWindow,
    connected: bool,
    consecutive_failures: u32,
    connects: u64,
    frames: u64,
    last_frame_at_ms: Option<i64>,
    /// When this endpoint may next be dialed. `None` means now.
    backoff_until: Option<Instant>,
    /// Smooth weighted round robin's running credit. Only consulted when two
    /// endpoints are equally healthy and equally fast.
    credit: i64,
}

impl EndpointSlot {
    fn new(config: EndpointConfig) -> Self {
        Self {
            config,
            latency: LatencyWindow::new(),
            connected: false,
            consecutive_failures: 0,
            connects: 0,
            frames: 0,
            last_frame_at_ms: None,
            backoff_until: None,
            credit: 0,
        }
    }

    fn health(&self) -> EndpointHealth {
        if self.consecutive_failures >= FAILURES_TO_UNHEALTHY {
            return EndpointHealth::Unhealthy;
        }
        self.latency.health()
    }

    fn available_at(&self, now: Instant) -> bool {
        match self.backoff_until {
            Some(until) => now >= until,
            None => true,
        }
    }
}

/// Every endpoint, and the decision of which one to use.
///
/// Two jobs live here. Failover is the reconnect side: an endpoint that will not
/// connect backs off for longer and longer while the other providers carry the
/// stream, and a provider that keeps failing stops being chosen at all. Load
/// balancing is the request side: `pick` answers "who should this one-shot RPC
/// go to", by health first, then measured p95, then weight.
///
/// Ordering the tie-break by weight rather than by index matters for the free
/// tiers this runs on — the endpoint with the larger monthly allowance is given
/// the larger share of the calls.
pub struct EndpointPool {
    slots: Mutex<Vec<EndpointSlot>>,
}

impl EndpointPool {
    pub fn new(configs: Vec<EndpointConfig>) -> Self {
        Self {
            slots: Mutex::new(configs.into_iter().map(EndpointSlot::new).collect()),
        }
    }

    pub fn len(&self) -> usize {
        self.slots.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.lock().is_empty()
    }

    pub fn config(&self, index: usize) -> Option<EndpointConfig> {
        self.slots.lock().get(index).map(|s| s.config.clone())
    }

    /// Which endpoint a one-shot request should go to.
    ///
    /// Returns `None` only when the pool is empty. When everything is unhealthy
    /// it still returns something: refusing to answer would turn a degraded feed
    /// into no feed, and the caller already knows the health from the snapshot.
    pub fn pick(&self) -> Option<usize> {
        self.pick_at(Instant::now())
    }

    fn pick_at(&self, now: Instant) -> Option<usize> {
        let mut slots = self.slots.lock();
        if slots.is_empty() {
            return None;
        }

        // Preference order, each falling through to the next only when it is
        // empty: usable and out of backoff, then usable, then anything at all.
        let usable_now: Vec<usize> = (0..slots.len())
            .filter(|&i| slots[i].health().is_usable() && slots[i].available_at(now))
            .collect();
        let candidates = if !usable_now.is_empty() {
            usable_now
        } else {
            let usable: Vec<usize> = (0..slots.len())
                .filter(|&i| slots[i].health().is_usable())
                .collect();
            if usable.is_empty() {
                (0..slots.len()).collect()
            } else {
                usable
            }
        };

        // Rank by health band, then by measured p95. An endpoint that has never
        // been measured sorts as if it were exactly at the healthy limit, so it
        // is tried without being preferred over something known to be fast.
        let rank = |slot: &EndpointSlot| -> (u8, u32) {
            let band = match slot.health() {
                EndpointHealth::Healthy => 0,
                EndpointHealth::Unknown => 1,
                EndpointHealth::Degraded => 2,
                EndpointHealth::Unhealthy => 3,
            };
            let p95 = if slot.latency.is_empty() {
                HEALTHY_P95_MS
            } else {
                slot.latency.percentiles().1
            };
            (band, p95)
        };

        let best = candidates.iter().map(|&i| rank(&slots[i])).min()?;
        let tied: Vec<usize> = candidates
            .into_iter()
            .filter(|&i| rank(&slots[i]) == best)
            .collect();
        if tied.len() == 1 {
            return Some(tied[0]);
        }

        // Smooth weighted round robin over the tie. Each pass adds every tied
        // endpoint's weight to its credit, the largest credit wins, and the
        // winner pays the total back — which spreads picks in proportion to
        // weight without bursting one endpoint and then the next.
        let total: i64 = tied
            .iter()
            .map(|&i| slots[i].config.weight.max(1) as i64)
            .sum();
        for &i in &tied {
            slots[i].credit += slots[i].config.weight.max(1) as i64;
        }
        let winner = *tied.iter().max_by_key(|&&i| slots[i].credit)?;
        slots[winner].credit -= total;
        Some(winner)
    }

    /// A connection was established. Clears the failure count and the backoff.
    pub fn record_connected(&self, index: usize) {
        let mut slots = self.slots.lock();
        if let Some(slot) = slots.get_mut(index) {
            slot.connected = true;
            slot.consecutive_failures = 0;
            slot.backoff_until = None;
            slot.connects += 1;
        }
    }

    /// Every socket is gone, because the manager was told to stop.
    ///
    /// Not a failure: no backoff is set and no failure is counted, since
    /// nothing went wrong — the endpoints were shut on purpose and there is
    /// nothing to retry. What it does clear is `connected`, because after
    /// `IngestionManager::stop` the read tasks have been aborted and an
    /// endpoint still reporting itself connected is a snapshot describing
    /// sockets that are not there.
    ///
    /// That matters beyond the pane that draws it: `refuse_over_a_live_feed`
    /// reads exactly this flag to decide whether a fixture may go behind the
    /// clock, and a `connected` left standing after a stop would refuse replay
    /// over feeds that had already been shut.
    pub fn record_all_disconnected(&self) {
        for slot in self.slots.lock().iter_mut() {
            slot.connected = false;
        }
    }

    /// A connection failed or dropped. Returns how long to wait before trying
    /// this endpoint again.
    pub fn record_failure(&self, index: usize) -> Duration {
        self.record_failure_at(index, Instant::now())
    }

    fn record_failure_at(&self, index: usize, now: Instant) -> Duration {
        let mut slots = self.slots.lock();
        let Some(slot) = slots.get_mut(index) else {
            return BACKOFF_MIN;
        };
        slot.connected = false;
        slot.consecutive_failures = slot.consecutive_failures.saturating_add(1);
        // Doubling, capped. `saturating_sub(1)` so the first failure waits
        // `BACKOFF_MIN` rather than twice it.
        let shift = (slot.consecutive_failures.saturating_sub(1)).min(16);
        let backoff = BACKOFF_MIN.saturating_mul(1u32 << shift).min(BACKOFF_MAX);
        slot.backoff_until = Some(now + backoff);
        backoff
    }

    /// A measured round trip, in milliseconds.
    pub fn record_latency(&self, index: usize, millis: u32) {
        let mut slots = self.slots.lock();
        if let Some(slot) = slots.get_mut(index) {
            slot.latency.record(millis);
        }
    }

    /// A frame arrived, which is the other thing that proves an endpoint is live.
    pub fn record_frame(&self, index: usize, at_ms: i64) {
        let mut slots = self.slots.lock();
        if let Some(slot) = slots.get_mut(index) {
            slot.frames += 1;
            slot.last_frame_at_ms = Some(at_ms);
        }
    }

    /// How many endpoints are currently good enough to route to.
    pub fn healthy_count(&self) -> usize {
        self.slots
            .lock()
            .iter()
            .filter(|s| s.health().is_usable())
            .count()
    }

    pub fn status(&self) -> Vec<EndpointStatus> {
        self.status_at(Instant::now())
    }

    fn status_at(&self, now: Instant) -> Vec<EndpointStatus> {
        self.slots
            .lock()
            .iter()
            .map(|slot| {
                let (p50, p95) = slot.latency.percentiles();
                EndpointStatus {
                    provider: slot.config.provider,
                    transport: slot.config.transport,
                    url: slot.config.redacted(),
                    health: slot.health(),
                    connected: slot.connected,
                    latency_p50_ms: p50,
                    latency_p95_ms: p95,
                    consecutive_failures: slot.consecutive_failures,
                    connects: slot.connects,
                    frames: slot.frames,
                    backoff_remaining_ms: slot
                        .backoff_until
                        .map(|until| until.saturating_duration_since(now).as_millis() as u64)
                        .unwrap_or(0),
                    last_frame_at_ms: slot.last_frame_at_ms,
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// money, in integers
// ---------------------------------------------------------------------------

/// What one SOL is worth, in micro-dollars.
///
/// Every threshold in this module is written in dollars because that is how the
/// strategy is written, and every chain number arrives in lamports. This is the
/// one place the two meet, and it does the conversion in integers for the same
/// reason `types.rs` does: two runs over the same numbers have to agree.
///
/// The price itself comes from outside — it is set by whatever is watching the
/// oracle — and a price of zero makes every conversion zero, which reads as "no
/// candidate is big enough" and stops entries rather than starting them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SolPrice {
    pub micro_usd_per_sol: u64,
}

impl SolPrice {
    /// A price of zero: the safe starting value, because it makes everything
    /// look too small to trade rather than too big.
    pub const UNKNOWN: SolPrice = SolPrice {
        micro_usd_per_sol: 0,
    };

    pub const fn from_usd_cents(cents_per_sol: u64) -> Self {
        SolPrice {
            micro_usd_per_sol: cents_per_sol * MICRO_USD_PER_CENT,
        }
    }

    pub const fn is_known(&self) -> bool {
        self.micro_usd_per_sol > 0
    }

    /// Lamports to whole US cents, rounded down.
    ///
    /// `u128` throughout: lamports times micro-dollars overflows `u64` at around
    /// 18 SOL, which is well inside the range this is called with.
    pub const fn lamports_to_usd_cents(&self, lamports: u64) -> u64 {
        let micro_usd =
            lamports as u128 * self.micro_usd_per_sol as u128 / LAMPORTS_PER_SOL as u128;
        (micro_usd / MICRO_USD_PER_CENT as u128) as u64
    }
}

/// The floor everything has to clear to be worth a single further CPU cycle.
///
/// Both numbers describe the same thing from different angles: a launch nobody
/// has bought yet. Most of what comes off a pump.fun subscription is exactly
/// that — a mint created, sniped in the same slot by bots and abandoned — and
/// forwarding it costs the parse, the channel slot and the SQLite row for
/// something the engine would refuse anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpamFloor {
    /// Below this market cap, drop it.
    pub min_market_cap_usd_cents: u64,
    /// Within this many slots of the first sighting, drop it however big it
    /// looks. The first handful of slots after a launch are the bot lottery:
    /// the reserves move violently, the market cap they imply is noise, and
    /// nothing measured there survives to the next slot.
    pub min_slots_since_launch: u64,
}

impl SpamFloor {
    /// Sub-$15k, or inside the first ten slots.
    pub const DEFAULT: SpamFloor = SpamFloor {
        min_market_cap_usd_cents: 1_500_000,
        min_slots_since_launch: 10,
    };
}

/// The band the strategy actually wants.
///
/// Anything landing inside it goes to the fast path. Outside it — bigger or
/// smaller — is still real data worth scoring and storing, it just does not get
/// the route that skips confirmations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetWindow {
    pub low_usd_cents: u64,
    pub high_usd_cents: u64,
}

impl TargetWindow {
    /// $25k to $80k.
    pub const DEFAULT: TargetWindow = TargetWindow {
        low_usd_cents: 2_500_000,
        high_usd_cents: 8_000_000,
    };

    pub const fn contains(&self, usd_cents: u64) -> bool {
        usd_cents >= self.low_usd_cents && usd_cents <= self.high_usd_cents
    }
}

// ---------------------------------------------------------------------------
// routing
// ---------------------------------------------------------------------------

/// Which channel a candidate goes down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Route {
    /// Inside the target window. Shallow queue, no confirmations waited on.
    FastPath,
    /// Real, but not what the fast path is for.
    Standard,
}

impl Route {
    pub const fn as_str(self) -> &'static str {
        match self {
            Route::FastPath => "fast_path",
            Route::Standard => "standard",
        }
    }
}

/// Why a frame or a candidate went no further.
///
/// Every one of these is counted. The roadmap's soak criterion is that no
/// critical event is *silently* dropped, which means the drop itself is fine and
/// the silence is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DropReason {
    /// The frame never mentioned an allowlisted program. Decided on raw bytes.
    NotAllowlisted,
    /// A subscription acknowledgement, an error, or a keepalive — not data.
    NotANotification,
    /// It looked like a notification and would not parse as one.
    Undecodable,
    /// The program is one of the five and this build has no decoder for its
    /// account layout. Only pump.fun's bonding curve is decoded here; the
    /// Raydium pool layouts are a later phase. Counted rather than ignored, so
    /// the gap is visible in the numbers instead of looking like a quiet feed.
    NoDecoder,
    /// Below the market cap floor.
    TooSmall,
    /// Inside the first few slots of its life.
    LotterySlot,
    /// This provider is behind: another one already reported this mint at this
    /// slot or a later one.
    StaleSlot,
    /// The pool is too thin to enter under the configured thresholds.
    PoolTooThin,
    /// The curve has already completed; the launch window is over.
    Graduated,
}

impl DropReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            DropReason::NotAllowlisted => "not_allowlisted",
            DropReason::NotANotification => "not_a_notification",
            DropReason::Undecodable => "undecodable",
            DropReason::NoDecoder => "no_decoder",
            DropReason::TooSmall => "too_small",
            DropReason::LotterySlot => "lottery_slot",
            DropReason::StaleSlot => "stale_slot",
            DropReason::PoolTooThin => "pool_too_thin",
            DropReason::Graduated => "graduated",
        }
    }
}

/// What the filters decided about one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Dropped(DropReason),
    Routed(Route),
}

// ---------------------------------------------------------------------------
// the filters themselves
// ---------------------------------------------------------------------------

/// Everything the socket decides with.
#[derive(Debug, Clone, Copy)]
pub struct StreamFilters {
    pub spam_floor: SpamFloor,
    pub target_window: TargetWindow,
    pub liquidity: LiquidityThresholds,
}

impl StreamFilters {
    /// The defaults the strategy is written against: sub-$15k and slot 0–10 are
    /// spam, $25k–$80k is the target, and a pool under 5 SOL is not enterable.
    pub const DEFAULT: StreamFilters = StreamFilters {
        spam_floor: SpamFloor::DEFAULT,
        target_window: TargetWindow::DEFAULT,
        liquidity: LiquidityThresholds {
            min_pool_lamports: 5 * LAMPORTS_PER_SOL,
            exit_only_below_lamports: 2 * LAMPORTS_PER_SOL,
            max_pool_share_bps: crate::types::MAX_POOL_SHARE_BPS,
        },
    };

    /// The cheapest possible look at a frame: does it mention a program this
    /// engine cares about?
    ///
    /// Runs on the raw socket bytes with nothing parsed. A base58 key is plain
    /// ASCII inside JSON, so finding the program id is a substring search, and a
    /// frame that fails it is dropped for the cost of that search rather than
    /// the cost of building a `serde_json::Value` first. On a launch burst that
    /// is the difference between the pre-filter being free and it being the
    /// most expensive thing in the loop.
    pub fn admits_frame(&self, frame: &[u8]) -> Result<(), DropReason> {
        // Notifications carry data; acks, errors and keepalives do not, and they
        // are the majority of frames on a quiet socket.
        if find(frame, b"Notification").is_none() {
            return Err(DropReason::NotANotification);
        }
        if !ALLOWED_PROGRAMS
            .iter()
            .any(|p| find(frame, p.text.as_bytes()).is_some())
        {
            return Err(DropReason::NotAllowlisted);
        }
        Ok(())
    }

    /// The decision on a decoded candidate.
    ///
    /// Order matters: the two spam clauses come first because they reject most
    /// of what gets this far, and the target window is checked last because it
    /// is the only clause that promotes rather than rejects.
    pub fn route(&self, view: &CandidateView, price: SolPrice) -> Verdict {
        if view.curve_complete {
            return Verdict::Dropped(DropReason::Graduated);
        }
        if view.slots_since_launch < self.spam_floor.min_slots_since_launch {
            return Verdict::Dropped(DropReason::LotterySlot);
        }

        let market_cap = price.lamports_to_usd_cents(view.market_cap_lamports);
        if market_cap < self.spam_floor.min_market_cap_usd_cents {
            return Verdict::Dropped(DropReason::TooSmall);
        }
        if !self.liquidity.admits_entry(view.pool_lamports) {
            return Verdict::Dropped(DropReason::PoolTooThin);
        }

        if self.target_window.contains(market_cap) {
            Verdict::Routed(Route::FastPath)
        } else {
            Verdict::Routed(Route::Standard)
        }
    }
}

/// First index of `needle` in `haystack`.
///
/// Written out rather than pulled in, for the same reason `types.rs` has its own
/// base58: it is ten lines, it is on the hot path, and the standard library has
/// no slice equivalent of `str::find`. The first-byte check is what keeps it at
/// roughly one pass over the frame instead of one pass per position.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let first = needle[0];
    let last = haystack.len() - needle.len();
    for i in 0..=last {
        if haystack[i] == first && &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// base64, into a caller's buffer
// ---------------------------------------------------------------------------

/// Standard base64, decoding into a slice the caller already has.
///
/// The accounts this decodes are under a hundred bytes, so the buffer is a
/// stack array and the decode allocates nothing at all. That is the whole point
/// of it being here rather than a dependency: a general base64 crate hands back
/// a `Vec`, and a `Vec` per frame on a launch burst is the allocator doing the
/// work the pre-filter just saved.
mod base64 {
    /// Reverse lookup for the standard alphabet. `0xff` is "not a base64 digit".
    const DIGIT: [u8; 256] = {
        let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut table = [0xffu8; 256];
        let mut i = 0;
        while i < 64 {
            table[alphabet[i] as usize] = i as u8;
            i += 1;
        }
        table
    };

    /// Decodes `text` into `out`, returning how many bytes were written.
    ///
    /// `None` for anything that is not well-formed base64 or that would not fit,
    /// because a half-decoded account is worse than no account: it would parse
    /// as a bonding curve holding some other number.
    pub fn decode(text: &[u8], out: &mut [u8]) -> Option<usize> {
        // Padding is stripped rather than validated. The providers all emit it
        // correctly and the length check below is what actually matters.
        let text = match text {
            [body @ .., b'=', b'='] => body,
            [body @ .., b'='] => body,
            body => body,
        };
        // A base64 group is four characters; a trailing group of one character
        // encodes nothing and is malformed.
        if text.len() % 4 == 1 {
            return None;
        }
        if out.len() < text.len() / 4 * 3 {
            return None;
        }

        let mut written = 0usize;
        let mut accumulator = 0u32;
        let mut bits = 0u32;
        for &c in text {
            let digit = DIGIT[c as usize];
            if digit == 0xff {
                return None;
            }
            accumulator = (accumulator << 6) | digit as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                if written == out.len() {
                    return None;
                }
                out[written] = (accumulator >> bits) as u8;
                written += 1;
            }
        }
        Some(written)
    }
}

// ---------------------------------------------------------------------------
// the pump.fun bonding curve account
// ---------------------------------------------------------------------------

/// The largest account this module will decode. The bonding curve is 81 bytes;
/// the margin covers a layout that grows a field without this needing a change.
const ACCOUNT_BUFFER: usize = 256;

/// Where a pump.fun curve completes, in lamports of real SOL.
///
/// A protocol parameter rather than a law of nature — it has been changed before
/// and can be again — which is why it is one named constant and not sprinkled
/// through the arithmetic. Curve progress is measured against it and nothing
/// else in the engine hardcodes it.
pub const PUMP_GRADUATION_LAMPORTS: u64 = 85 * LAMPORTS_PER_SOL;

/// The fields of a pump.fun `BondingCurve` account this engine reads.
///
/// Anchor layout, little-endian: eight bytes of discriminator, then five `u64`
/// reserves, then the `complete` flag, then — in the current version — the
/// creator. The older 49-byte version has no creator, which is why that field
/// is zero rather than absent: `Pubkey::is_zero` already means "the decode did
/// not find one" everywhere else in this codebase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BondingCurve {
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves: u64,
    pub real_token_reserves: u64,
    pub real_sol_reserves: u64,
    pub token_total_supply: u64,
    pub complete: bool,
    pub creator: Pubkey,
}

/// The smallest layout with all five reserves and the flag.
const CURVE_MIN_LEN: usize = 49;
/// The layout that also carries the creator.
const CURVE_WITH_CREATOR_LEN: usize = 81;

impl BondingCurve {
    /// Reads the account bytes. `None` if they are too short to be a curve.
    ///
    /// The discriminator is deliberately not checked. It is the first thing that
    /// changes when a program is upgraded, and refusing every account after an
    /// upgrade would take the feed down for a field this code does not read.
    /// The length check and the reserve sanity check below are what guard the
    /// numbers that matter.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < CURVE_MIN_LEN {
            return None;
        }
        let u64_at = |offset: usize| -> u64 {
            let mut raw = [0u8; 8];
            raw.copy_from_slice(&bytes[offset..offset + 8]);
            u64::from_le_bytes(raw)
        };

        let creator = if bytes.len() >= CURVE_WITH_CREATOR_LEN {
            let mut raw = [0u8; 32];
            raw.copy_from_slice(&bytes[49..81]);
            Pubkey::new(raw)
        } else {
            Pubkey::ZERO
        };

        Some(BondingCurve {
            virtual_token_reserves: u64_at(8),
            virtual_sol_reserves: u64_at(16),
            real_token_reserves: u64_at(24),
            real_sol_reserves: u64_at(32),
            token_total_supply: u64_at(40),
            complete: bytes[48] != 0,
            creator,
        })
    }

    /// What the whole supply is worth at the curve's current price, in lamports.
    ///
    /// Price is `virtual_sol_reserves / virtual_token_reserves`; both sides are
    /// raw units, so the token decimals cancel and the answer is lamports. Zero
    /// token reserves would be a division by zero and is also the shape of a
    /// half-written account, so it reports nothing rather than guessing.
    pub const fn market_cap_lamports(&self) -> u64 {
        if self.virtual_token_reserves == 0 {
            return 0;
        }
        let cap = self.token_total_supply as u128 * self.virtual_sol_reserves as u128
            / self.virtual_token_reserves as u128;
        if cap > u64::MAX as u128 {
            u64::MAX
        } else {
            cap as u64
        }
    }

    /// How far along the curve is, in basis points, capped at 10_000.
    pub const fn progress_bps(&self) -> u16 {
        if self.complete {
            return BPS_DENOMINATOR as u16;
        }
        let bps = self.real_sol_reserves as u128 * BPS_DENOMINATOR as u128
            / PUMP_GRADUATION_LAMPORTS as u128;
        if bps > BPS_DENOMINATOR as u128 {
            BPS_DENOMINATOR as u16
        } else {
            bps as u16
        }
    }

    /// Whether the numbers hold together well enough to act on.
    ///
    /// A curve with no virtual reserves is not a cheap coin, it is an account
    /// that was read while it was being written or that is not a curve at all.
    pub const fn is_plausible(&self) -> bool {
        self.virtual_token_reserves > 0
            && self.virtual_sol_reserves > 0
            && self.token_total_supply > 0
    }
}

// ---------------------------------------------------------------------------
// the wire format, borrowed rather than copied
// ---------------------------------------------------------------------------

// These mirror a Solana JSON-RPC pubsub notification. Every string field is a
// borrow out of the frame buffer, so `serde_json` writes nothing to the heap for
// them, and `data` is a two-element tuple rather than a `Vec` for the same
// reason — `["<base64>", "base64"]` is a fixed-shape pair, and modelling it as
// one avoids a heap allocation per frame.

#[derive(Deserialize)]
struct ProgramNotification<'a> {
    #[serde(borrow)]
    params: ProgramParams<'a>,
}

#[derive(Deserialize)]
struct ProgramParams<'a> {
    #[serde(borrow)]
    result: ProgramResult<'a>,
}

#[derive(Deserialize)]
struct ProgramResult<'a> {
    context: SlotContext,
    #[serde(borrow)]
    value: ProgramValue<'a>,
}

#[derive(Deserialize)]
struct ProgramValue<'a> {
    #[serde(borrow)]
    pubkey: &'a str,
    #[serde(borrow)]
    account: AccountView<'a>,
}

#[derive(Deserialize)]
struct AccountNotification<'a> {
    #[serde(borrow)]
    params: AccountParams<'a>,
}

#[derive(Deserialize)]
struct AccountParams<'a> {
    #[serde(borrow)]
    result: AccountResult<'a>,
}

#[derive(Deserialize)]
struct AccountResult<'a> {
    context: SlotContext,
    #[serde(borrow)]
    value: AccountView<'a>,
}

#[derive(Deserialize)]
struct SlotContext {
    slot: u64,
}

#[derive(Deserialize)]
struct AccountView<'a> {
    lamports: u64,
    /// `[payload, "base64"]`, which is what `encoding: "base64"` produces.
    #[serde(borrow)]
    data: (&'a str, &'a str),
    #[serde(borrow)]
    owner: &'a str,
}

/// One notification, with nothing copied out of the frame yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountFrame<'a> {
    pub slot: u64,
    /// The account that changed. `None` on an `accountNotification`, where the
    /// account is whatever the subscription was opened for.
    pub account: Option<&'a str>,
    pub owner: &'a str,
    pub lamports: u64,
    pub data_base64: &'a str,
}

/// Parses a pubsub frame without copying any of it.
///
/// Which of the two shapes to expect is decided by searching the raw bytes for
/// the method name rather than by an untagged enum: `serde`'s untagged
/// deserialisation buffers the whole document into an owned intermediate first,
/// which would undo every borrow in the structs above.
pub fn decode_frame(frame: &[u8]) -> Result<AccountFrame<'_>, DropReason> {
    if find(frame, b"\"programNotification\"").is_some() {
        let parsed: ProgramNotification =
            serde_json::from_slice(frame).map_err(|_| DropReason::Undecodable)?;
        let value = parsed.params.result.value;
        return Ok(AccountFrame {
            slot: parsed.params.result.context.slot,
            account: Some(value.pubkey),
            owner: value.account.owner,
            lamports: value.account.lamports,
            data_base64: value.account.data.0,
        });
    }
    if find(frame, b"\"accountNotification\"").is_some() {
        let parsed: AccountNotification =
            serde_json::from_slice(frame).map_err(|_| DropReason::Undecodable)?;
        let value = parsed.params.result.value;
        return Ok(AccountFrame {
            slot: parsed.params.result.context.slot,
            account: None,
            owner: value.owner,
            lamports: value.lamports,
            data_base64: value.data.0,
        });
    }
    Err(DropReason::NotANotification)
}

/// The allowlisted program this frame belongs to, matched on text.
///
/// A string compare rather than a base58 decode: the pre-filter already proved
/// the id is somewhere in the frame, and this proves it is in the field that
/// matters, for the cost of comparing 44 bytes.
fn allowed_program_for(owner: &str) -> Option<Pubkey> {
    ALLOWED_PROGRAMS
        .iter()
        .find(|p| p.text == owner)
        .map(|p| p.key)
}

// ---------------------------------------------------------------------------
// what a decoded frame becomes
// ---------------------------------------------------------------------------

/// One candidate, as the filters see it.
///
/// Every field is a number or a key; nothing borrows, so this crosses a channel
/// without an allocation and without a lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateView {
    pub provider: FeedProvider,
    pub slot: u64,
    /// The bonding curve account. This is the identity ingestion works with:
    /// the curve is a PDA of the mint, so the mapping only runs one way, and
    /// resolving the mint needs the create instruction rather than this frame.
    pub account: Pubkey,
    /// Which of the five allowlisted programs owns it.
    pub program: Pubkey,
    /// From the curve account when the layout carries it, `Pubkey::ZERO` when
    /// the account is the older version that does not.
    pub creator: Pubkey,
    pub market_cap_lamports: u64,
    /// Real SOL in the curve — what could actually be sold into.
    pub pool_lamports: u64,
    /// The price-setting reserves, carried alongside the executable one.
    ///
    /// Both are decoded off the account and they answer different questions:
    /// **virtual reserves set the price, real reserves set what is executable**
    /// (`replay::CurveState` says the same thing about the same six numbers).
    /// The window needs the pair rather than either alone — the ratio between
    /// them is how much of the quoted price is actually there to sell into, and
    /// the sandwich threshold of `REPLAY_AND_SIMULATION_SPEC.md` §15.2 is
    /// written in terms of the virtual SOL reserve `y`. Deriving `y` from
    /// `pool_lamports` and a launch constant would be a guess about a protocol
    /// parameter that has changed before, so it is carried instead.
    pub virtual_sol_reserves: u64,
    pub virtual_token_reserves: u64,
    pub curve_progress_bps: u16,
    pub curve_complete: bool,
    /// Slots since this account was *first seen by this process*, which is not
    /// the same as slots since the create instruction. See `LaunchIndex`.
    pub slots_since_launch: u64,
}

impl CandidateView {
    /// The `types.rs` view of the same thing, for the scoring pass.
    ///
    /// The mint and the symbol are arguments rather than fields because this
    /// layer does not know either one: the curve account carries neither, and
    /// inventing them here would put a `Pubkey::ZERO` mint into the one type the
    /// rest of the engine keys its decisions on. The caller that resolved them
    /// passes them in.
    pub fn as_token_candidate<'a>(
        &self,
        mint: Pubkey,
        symbol: &'a str,
        launched_at_ms: i64,
    ) -> TokenCandidate<'a> {
        TokenCandidate {
            mint,
            creator: self.creator,
            symbol,
            launched_at_ms,
            curve_progress_bps: self.curve_progress_bps,
            initial_liquidity_lamports: self.pool_lamports,
        }
    }
}

/// A candidate that survived the filters, with the timing of its own trip
/// through them attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateEvent {
    pub view: CandidateView,
    pub route: Route,
    /// Market cap at the price the filters used, so a row in `sts.db` can be
    /// read back without needing to know what SOL was worth at the time.
    pub market_cap_usd_cents: u64,
    pub received_at_ms: i64,
    /// Receipt to dispatch, in microseconds. The number `DISPATCH_BUDGET` is
    /// about.
    pub dispatch_latency_us: u32,
}

// ---------------------------------------------------------------------------
// the launch index: slot watermarking, cross-provider deduplication, and what
// to do when two providers disagree
// ---------------------------------------------------------------------------

/// A fingerprint of the state one curve write carried.
///
/// FNV-1a over the seven fields [`BondingCurve`] decodes rather than over the
/// account bytes, because what makes two payloads *conflict* is that they would
/// be acted on differently, and these seven numbers are everything a decision
/// downstream reads. Two writes that differ only in bytes this build does not
/// decode are one state to this engine, and saying so is more honest than
/// reporting a disagreement nothing could act on.
///
/// Not a cryptographic hash and nothing is defended with it: the only failure
/// it has is a collision, and a collision costs a disagreement that goes
/// unreported rather than one that is invented.
fn curve_digest(curve: &BondingCurve) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = OFFSET_BASIS;
    let mut eat = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(PRIME);
        }
    };
    eat(&curve.virtual_token_reserves.to_le_bytes());
    eat(&curve.virtual_sol_reserves.to_le_bytes());
    eat(&curve.real_token_reserves.to_le_bytes());
    eat(&curve.real_sol_reserves.to_le_bytes());
    eat(&curve.token_total_supply.to_le_bytes());
    eat(&[curve.complete as u8]);
    eat(curve.creator.as_bytes());
    hash
}

/// A digest as sixteen hex digits rather than as a JSON number.
///
/// Every reader of the telemetry stream parses JSON numbers as doubles, the
/// window included, and a `u64` past `2^53` does not survive that. The same
/// reason `geyser::parse_raw_amount` refuses to go near `ui_amount`.
fn hex_digest<S: serde::Serializer>(digest: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&format_args!("{digest:016x}"))
}

/// Two providers, one account, one slot, and two different states.
///
/// What is being claimed here is narrow and worth stating exactly: the first
/// write each provider delivered for that account in that slot did not agree.
/// It is not a claim that either provider is wrong — there is no third source
/// here to break the tie — it is a claim that the engine cannot tell which of
/// two states it is holding, which is something a decision should know.
///
/// The write that was released for the slot is `held`, and it stays released. A
/// disagreement never rewrites what downstream has already acted on: it is
/// counted, published, and left to the phase that decides what provider
/// divergence should cost an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Contradiction {
    pub account: Pubkey,
    pub slot: u64,
    /// The provider whose write was released for this slot.
    pub held_by: FeedProvider,
    #[serde(serialize_with = "hex_digest")]
    pub held: u64,
    /// The provider that reported something else for the same slot.
    pub reported_by: FeedProvider,
    #[serde(serialize_with = "hex_digest")]
    pub reported: u64,
}

/// What has been heard about one account at one slot.
///
/// `canonical` is the write that was released for that slot — the first to
/// arrive — and `heard` is which providers have already had their say about it.
/// Both describe [`LaunchRecord::last_slot`] and are thrown away the moment the
/// watermark moves.
#[derive(Debug, Clone, Copy)]
struct SlotWitness {
    canonical_by: FeedProvider,
    canonical: u64,
    /// Indexed by [`FeedProvider::index`]. A provider is written here once per
    /// slot, on its *first* write of that slot.
    heard: [bool; FeedProvider::ALL.len()],
}

impl SlotWitness {
    fn opened(provider: FeedProvider, digest: u64) -> Self {
        let mut heard = [false; FeedProvider::ALL.len()];
        heard[provider.index()] = true;
        SlotWitness {
            canonical_by: provider,
            canonical: digest,
            heard,
        }
    }

    /// Records this provider's first word on the slot, and says what it
    /// disagrees with.
    ///
    /// `None` for a provider that has already been heard, and that exception is
    /// the whole reason this compares first writes rather than any two writes.
    /// A curve is written several times in one slot and every provider delivers
    /// every write, so a provider's second write differs from another
    /// provider's first *by design* — comparing those would report a
    /// disagreement every time two sockets interleaved, which is every busy
    /// slot. One socket delivers one account's writes in the order the
    /// validator made them, so each provider's first write of a slot is the
    /// same write, and comparing those compares like with like.
    fn hear(&mut self, provider: FeedProvider, digest: u64) -> Option<(FeedProvider, u64)> {
        let seat = &mut self.heard[provider.index()];
        if *seat {
            return None;
        }
        *seat = true;
        (digest != self.canonical).then_some((self.canonical_by, self.canonical))
    }
}

#[derive(Debug, Clone, Copy)]
struct LaunchRecord {
    first_slot: u64,
    last_slot: u64,
    /// Who has been heard about `last_slot` and what was released for it.
    /// `None` after a rewind: a slot on an abandoned fork is not something to
    /// hold anybody to.
    witness: Option<SlotWitness>,
}

/// What the index knew about an account when it was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Observation {
    slots_since_launch: u64,
    /// This slot has already been reported for this account, by this provider or
    /// another one. Three providers watching the same program means the same
    /// update three times, and only the first is news.
    stale: bool,
    /// Another provider reported a different state for this same slot. Always
    /// arrives with `stale`, because a disagreement is only visible on the
    /// second report of a slot and the first one is what was released.
    conflict: Option<Contradiction>,
}

/// A bounded memory of which accounts have been seen and how far they have got.
///
/// Three jobs, one map. It watermarks slots, so the second and third providers
/// to report an update are dropped rather than tripling the work downstream. It
/// dates accounts, so the lottery filter has an age to work with. And it holds
/// the witness for the slot it is watermarking, so that a provider dropped as a
/// duplicate is first checked for *being* one — a second copy of the released
/// write is a duplicate, and something else is a [`Contradiction`].
///
/// The age it reports is measured from the first sighting, not from the create
/// instruction — which this layer never sees. The practical difference is at
/// startup: a coin that was already alive is held for `min_slots_since_launch`
/// before it can route. Four seconds of caution after a restart is cheap; a
/// slot-zero bot lottery mistaken for a $40k coin is not.
///
/// It is capacity-bounded and evicts oldest-first, so a process that runs for a
/// week uses the same memory as one that has just started.
struct LaunchIndex {
    seen: HashMap<Pubkey, LaunchRecord>,
    order: VecDeque<Pubkey>,
    capacity: usize,
}

impl LaunchIndex {
    fn new(capacity: usize) -> Self {
        Self {
            seen: HashMap::with_capacity(capacity),
            order: VecDeque::with_capacity(capacity),
            capacity: capacity.max(1),
        }
    }

    /// Watermarks one write and says what the index made of it.
    ///
    /// `digest` is [`curve_digest`] of the state the write carried. It is only
    /// ever compared against another digest for the same account at the same
    /// slot, and only across providers — see [`SlotWitness::hear`].
    ///
    /// A write for a slot *older* than the watermark is stale and nothing is
    /// claimed about it. There is no witness for that slot any more, and a
    /// provider running a few slots behind is not disagreeing with anyone: it
    /// is describing a moment this account has already moved on from.
    fn observe(
        &mut self,
        account: Pubkey,
        slot: u64,
        provider: FeedProvider,
        digest: u64,
    ) -> Observation {
        if let Some(record) = self.seen.get_mut(&account) {
            let stale = slot <= record.last_slot;
            let mut conflict = None;
            if !stale {
                record.last_slot = slot;
                record.witness = Some(SlotWitness::opened(provider, digest));
            } else if slot == record.last_slot {
                if let Some(witness) = record.witness.as_mut() {
                    conflict =
                        witness
                            .hear(provider, digest)
                            .map(|(held_by, held)| Contradiction {
                                account,
                                slot,
                                held_by,
                                held,
                                reported_by: provider,
                                reported: digest,
                            });
                }
            }
            return Observation {
                slots_since_launch: slot.saturating_sub(record.first_slot),
                stale,
                conflict,
            };
        }

        while self.order.len() >= self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.seen.remove(&evicted);
            }
        }
        self.seen.insert(
            account,
            LaunchRecord {
                first_slot: slot,
                last_slot: slot,
                witness: Some(SlotWitness::opened(provider, digest)),
            },
        );
        self.order.push_back(account);
        Observation {
            slots_since_launch: 0,
            stale: false,
            conflict: None,
        }
    }

    fn len(&self) -> usize {
        self.seen.len()
    }

    /// Forgets everything this index learned at or above `from_slot`.
    ///
    /// Called when the Geyser pipeline reports a fork switch. The watermark is
    /// the reason this exists: an account whose `last_slot` was set on the
    /// abandoned fork would reject the winning fork's rewrite of the same slot
    /// as stale, and the engine would then be holding curve numbers from a
    /// block that no longer exists with no way to hear the correction.
    ///
    /// An account whose *first* sighting was on the abandoned fork is dropped
    /// outright rather than rewound. Its age is measured from a launch that did
    /// not happen, and a launch that did not happen is not one the lottery
    /// filter should be counting slots from.
    ///
    /// Returns how many records were touched, which is what the caller reports.
    fn rewind(&mut self, from_slot: u64) -> usize {
        let floor = from_slot.saturating_sub(1);
        let mut touched = 0;
        self.seen.retain(|_, record| {
            if record.first_slot >= from_slot {
                touched += 1;
                return false;
            }
            if record.last_slot >= from_slot {
                record.last_slot = floor;
                // The witness described the slot that was just abandoned, and
                // the floor it lands on is a slot nobody here saw released.
                // Forgetting it is what stops the winning fork's rewrite being
                // reported as a provider disagreeing about a block that no
                // longer exists.
                record.witness = None;
                touched += 1;
            }
            true
        });
        // `order` is the eviction queue and it may now name accounts that are
        // gone. Filtered rather than left alone so that a long run of rollbacks
        // cannot fill it with names that evict nothing.
        self.order.retain(|account| self.seen.contains_key(account));
        touched
    }
}

// ---------------------------------------------------------------------------
// telemetry
// ---------------------------------------------------------------------------

/// Every counter the ingestion layer keeps.
///
/// All atomics, all `Relaxed`. These are counters nobody makes a decision from
/// inside the loop — they are read by the snapshot and by the UI — so ordering
/// between them buys nothing and costs a fence on the hottest path in the
/// process. The engine's actual invariants live on `Engine`'s `SeqCst` flags.
#[derive(Debug, Default)]
pub struct IngestionMetrics {
    frames: AtomicU64,
    bytes: AtomicU64,
    prefiltered: AtomicU64,
    parse_failures: AtomicU64,
    stale: AtomicU64,
    filtered: AtomicU64,
    candidates: AtomicU64,
    fast_path: AtomicU64,
    dropped_fast_path: AtomicU64,
    dropped_standard: AtomicU64,
    dropped_wal: AtomicU64,
    wal_rows: AtomicU64,
    wal_failures: AtomicU64,
    connects: AtomicU64,
    connect_failures: AtomicU64,
    disconnects: AtomicU64,
    dispatch_total_us: AtomicU64,
    dispatch_max_us: AtomicU64,
    dispatches: AtomicU64,
    over_budget: AtomicU64,
    /// Curve writes admitted from the Geyser pipeline rather than off a
    /// websocket frame. Counted separately because the two paths are not
    /// interchangeable: one arrives in chain order and the other in arrival
    /// order, and a run where the ordered path went quiet is a run whose
    /// sequencing stopped even though `candidates` kept climbing.
    ordered_ticks: AtomicU64,
    /// Fork switches the Geyser pipeline reported after it had already released
    /// events for the abandoned slots.
    rewinds: AtomicU64,
    /// Launch-index records those rewinds had to walk back.
    rewound_accounts: AtomicU64,
    /// Times two providers reported different states for one account at one
    /// slot. See [`Contradiction`].
    contradictions: AtomicU64,
    /// The most recent of those, for the sentence telemetry publishes about it.
    ///
    /// A lock rather than a row of atomics, and it is worth saying why: this is
    /// six fields that are only true together, and six atomics read one at a
    /// time is how a reader ends up with the account from one disagreement and
    /// the slot from the next. The lock is taken when there is a disagreement
    /// to record, which on a healthy feed is never.
    last_contradiction: Mutex<Option<Contradiction>>,
    /// The counters as of the previous snapshot, so rates are per-window rather
    /// than averaged over the whole run. An average over an eight-hour session
    /// says nothing about whether the socket is keeping up right now.
    last_snapshot_ms: AtomicU64,
    last_snapshot_frames: AtomicU64,
    last_snapshot_candidates: AtomicU64,
}

/// What the counters look like from outside, with the rates worked out.
///
/// `Default` is every counter at zero and no endpoints, which is what a process
/// that has just started reports. It is derived so that a test can state the
/// two or three numbers it is about and leave the rest alone.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestionSnapshot {
    pub at_ms: i64,
    /// Frames off the socket, before any filtering.
    pub frames: u64,
    pub bytes: u64,
    /// Rejected on raw bytes, never parsed. On a normal day this is most of them.
    pub prefiltered: u64,
    pub parse_failures: u64,
    /// Dropped because another provider already reported this slot.
    pub stale: u64,
    /// Parsed, understood, and refused by a filter.
    pub filtered: u64,
    /// Made it to a channel.
    pub candidates: u64,
    pub fast_path: u64,
    /// Lost because the channel out was full. The number that says the engine
    /// downstream is slower than the feed.
    pub dropped_fast_path: u64,
    pub dropped_standard: u64,
    pub dropped_wal: u64,
    pub wal_rows: u64,
    pub wal_failures: u64,
    pub connects: u64,
    pub connect_failures: u64,
    pub disconnects: u64,
    /// Receipt to dispatch, averaged over every frame since start.
    pub dispatch_mean_us: u64,
    pub dispatch_max_us: u64,
    /// How many dispatches took longer than `DISPATCH_BUDGET`.
    pub over_budget: u64,
    pub budget_us: u64,
    /// Frames per second since the previous snapshot.
    pub frames_per_sec: f64,
    /// Candidates per second since the previous snapshot.
    pub candidates_per_sec: f64,
    pub endpoints: Vec<EndpointStatus>,
    pub healthy_endpoints: usize,
    /// How many accounts the launch index is remembering.
    pub tracked_accounts: usize,
    /// Curve writes admitted from the ordered Geyser stream.
    pub ordered_ticks: u64,
    /// Fork switches that arrived after their slots had been released.
    pub rewinds: u64,
    /// Launch-index records walked back by those rewinds.
    pub rewound_accounts: u64,
    /// Times two providers reported different states for one account at one
    /// slot. Counted apart from `stale`, which every one of them is also: the
    /// frame was a duplicate by the watermark's reckoning, and the news is that
    /// it was not a copy of what the watermark released.
    pub contradictions: u64,
    /// The most recent disagreement, or `None` if there has not been one.
    pub last_contradiction: Option<Contradiction>,
}

impl IngestionMetrics {
    fn observe_frame(&self, len: usize) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(len as u64, Ordering::Relaxed);
    }

    fn observe_dispatch(&self, elapsed: Duration) {
        let micros = elapsed.as_micros().min(u64::MAX as u128) as u64;
        self.dispatches.fetch_add(1, Ordering::Relaxed);
        self.dispatch_total_us.fetch_add(micros, Ordering::Relaxed);
        self.dispatch_max_us.fetch_max(micros, Ordering::Relaxed);
        if elapsed > DISPATCH_BUDGET {
            self.over_budget.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Records one disagreement between two providers.
    ///
    /// Off the hot path in every sense that matters: it is reached only when
    /// two providers have actually disagreed, and it neither allocates nor
    /// serialises. The sentence about it is written by `telemetry_loop`, which
    /// runs on its own five-second tick — a JSON value built inside `dispatch`
    /// would put an allocation on the path Phase 0 says has none.
    fn count_contradiction(&self, contradiction: Contradiction) {
        self.contradictions.fetch_add(1, Ordering::Relaxed);
        *self.last_contradiction.lock() = Some(contradiction);
    }

    fn count_drop(&self, reason: DropReason) {
        match reason {
            DropReason::NotAllowlisted | DropReason::NotANotification => {
                self.prefiltered.fetch_add(1, Ordering::Relaxed);
            }
            DropReason::Undecodable => {
                self.parse_failures.fetch_add(1, Ordering::Relaxed);
            }
            DropReason::StaleSlot => {
                self.stale.fetch_add(1, Ordering::Relaxed);
            }
            _ => {
                self.filtered.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Moves the rate window forward.
    ///
    /// Separate from `snapshot` so that reading the counters has no side effect
    /// at all: the UI can poll as fast as it likes without shrinking the window
    /// out from under the telemetry task, which is the only caller of this.
    pub fn roll_window(&self, at_ms: i64, frames: u64, candidates: u64) {
        self.last_snapshot_ms.store(at_ms as u64, Ordering::Relaxed);
        self.last_snapshot_frames.store(frames, Ordering::Relaxed);
        self.last_snapshot_candidates
            .store(candidates, Ordering::Relaxed);
    }

    /// Reads every counter and works out the rates over the current window.
    ///
    /// Pure: two calls in a row report the same totals and rates over a slightly
    /// longer window. `roll_window` is what starts a new one.
    pub fn snapshot(&self, pool: &EndpointPool, tracked_accounts: usize) -> IngestionSnapshot {
        let at_ms = now_ms();
        let frames = self.frames.load(Ordering::Relaxed);
        let candidates = self.candidates.load(Ordering::Relaxed);

        let previous_ms = self.last_snapshot_ms.load(Ordering::Relaxed) as i64;
        let previous_frames = self.last_snapshot_frames.load(Ordering::Relaxed);
        let previous_candidates = self.last_snapshot_candidates.load(Ordering::Relaxed);

        // The first snapshot has no window behind it, so it reports no rate
        // rather than dividing by the epoch.
        let window_ms = if previous_ms == 0 {
            0
        } else {
            at_ms.saturating_sub(previous_ms)
        };
        let per_sec = |now: u64, before: u64| -> f64 {
            if window_ms <= 0 {
                0.0
            } else {
                now.saturating_sub(before) as f64 * 1000.0 / window_ms as f64
            }
        };

        let dispatches = self.dispatches.load(Ordering::Relaxed);
        let dispatch_mean_us = self
            .dispatch_total_us
            .load(Ordering::Relaxed)
            .checked_div(dispatches)
            .unwrap_or(0);

        IngestionSnapshot {
            at_ms,
            frames,
            bytes: self.bytes.load(Ordering::Relaxed),
            prefiltered: self.prefiltered.load(Ordering::Relaxed),
            parse_failures: self.parse_failures.load(Ordering::Relaxed),
            stale: self.stale.load(Ordering::Relaxed),
            filtered: self.filtered.load(Ordering::Relaxed),
            candidates,
            fast_path: self.fast_path.load(Ordering::Relaxed),
            dropped_fast_path: self.dropped_fast_path.load(Ordering::Relaxed),
            dropped_standard: self.dropped_standard.load(Ordering::Relaxed),
            dropped_wal: self.dropped_wal.load(Ordering::Relaxed),
            wal_rows: self.wal_rows.load(Ordering::Relaxed),
            wal_failures: self.wal_failures.load(Ordering::Relaxed),
            connects: self.connects.load(Ordering::Relaxed),
            connect_failures: self.connect_failures.load(Ordering::Relaxed),
            disconnects: self.disconnects.load(Ordering::Relaxed),
            dispatch_mean_us,
            dispatch_max_us: self.dispatch_max_us.load(Ordering::Relaxed),
            over_budget: self.over_budget.load(Ordering::Relaxed),
            budget_us: DISPATCH_BUDGET.as_micros() as u64,
            frames_per_sec: per_sec(frames, previous_frames),
            candidates_per_sec: per_sec(candidates, previous_candidates),
            endpoints: pool.status(),
            healthy_endpoints: pool.healthy_count(),
            tracked_accounts,
            ordered_ticks: self.ordered_ticks.load(Ordering::Relaxed),
            rewinds: self.rewinds.load(Ordering::Relaxed),
            rewound_accounts: self.rewound_accounts.load(Ordering::Relaxed),
            contradictions: self.contradictions.load(Ordering::Relaxed),
            last_contradiction: *self.last_contradiction.lock(),
        }
    }
}

// ---------------------------------------------------------------------------
// subscriptions
// ---------------------------------------------------------------------------

/// Which commitment the streams ask for.
///
/// `processed` is the fastest thing a validator will say and the only one fast
/// enough to be worth a fast path. It can also be rolled back, which is why
/// nothing here treats a candidate as a fact — the fork and confirmation
/// handling belongs to the phase that decides to spend money on one.
const COMMITMENT: &str = "processed";

/// A program, and the account sizes worth asking for inside it.
///
/// A `dataSize` filter is applied by the validator before the frame is sent, so
/// it is the cheapest filter in the whole system: the bytes never leave the
/// provider and never count against the quota. The array is empty where the
/// layout size is not something this code can state with confidence, and an
/// empty array means the subscription is still program-scoped — never broad.
struct SubscriptionSpec {
    program: &'static str,
    data_sizes: &'static [u64],
}

/// The subscriptions opened on every endpoint.
const SUBSCRIPTIONS: [SubscriptionSpec; 5] = [
    // Both pump.fun bonding curve layouts: the original, and the one that added
    // the creator field.
    SubscriptionSpec {
        program: PUMP_FUN_PROGRAM,
        data_sizes: &[49, 81],
    },
    SubscriptionSpec {
        program: PUMP_SWAP_PROGRAM,
        data_sizes: &[],
    },
    SubscriptionSpec {
        program: RAYDIUM_AMM_V4_PROGRAM,
        data_sizes: &[],
    },
    SubscriptionSpec {
        program: RAYDIUM_CPMM_PROGRAM,
        data_sizes: &[],
    },
    SubscriptionSpec {
        program: RAYDIUM_CLMM_PROGRAM,
        data_sizes: &[],
    },
];

/// Every `programSubscribe` request an endpoint is sent on connect.
///
/// One per program, or one per size where sizes are known, because a filter
/// array is a conjunction — two sizes cannot be asked for in one subscription.
pub fn subscription_requests() -> Vec<String> {
    let mut requests = Vec::new();
    let mut id = 1u64;
    for spec in SUBSCRIPTIONS.iter() {
        if spec.data_sizes.is_empty() {
            requests.push(subscribe_request(id, spec.program, None));
            id += 1;
            continue;
        }
        for &size in spec.data_sizes {
            requests.push(subscribe_request(id, spec.program, Some(size)));
            id += 1;
        }
    }
    requests
}

/// One JSON-RPC `programSubscribe`.
pub fn subscribe_request(id: u64, program: &str, data_size: Option<u64>) -> String {
    let mut config = serde_json::json!({
        "encoding": "base64",
        "commitment": COMMITMENT,
    });
    if let Some(size) = data_size {
        config["filters"] = serde_json::json!([{ "dataSize": size }]);
    }
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "programSubscribe",
        "params": [program, config],
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// the transport seam
// ---------------------------------------------------------------------------

/// A boxed future, which is what makes the traits below usable as `dyn`.
///
/// `async fn` in a trait would be tidier to write and would not be object-safe,
/// and object safety is the point: the manager holds one `Box<dyn FeedDialer>`
/// and does not care whether the thing behind it is a socket or a test fixture.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One thing off a feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedMessage {
    /// A data frame, still raw.
    Frame(Bytes),
    /// The reply to a ping. The only round-trip measurement a pubsub socket
    /// offers, and what the health bands are computed from.
    Pong,
}

/// The writing half of a feed.
pub trait FeedSink: Send {
    fn send_text(&mut self, text: String) -> BoxFuture<'_, Result<(), IngestError>>;
    fn ping(&mut self) -> BoxFuture<'_, Result<(), IngestError>>;
}

/// The reading half of a feed.
///
/// `recv` must be cancel-safe: the read loop races it against the heartbeat and
/// the shutdown signal, and a message half-taken out of a dropped future would
/// be a message silently lost. Both implementations here satisfy this because
/// both are `StreamExt::next` underneath.
pub trait FeedStream: Send {
    fn recv(&mut self) -> BoxFuture<'_, Option<Result<FeedMessage, IngestError>>>;
}

/// The two halves of an open feed: somewhere to write subscriptions, and
/// somewhere to read frames back.
pub type FeedPair = (Box<dyn FeedSink>, Box<dyn FeedStream>);

/// Opens feeds.
pub trait FeedDialer: Send + Sync + 'static {
    fn dial(&self, endpoint: EndpointConfig) -> BoxFuture<'static, Result<FeedPair, IngestError>>;
}

// ---------------------------------------------------------------------------
// the real socket
// ---------------------------------------------------------------------------

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct WsSink(SplitSink<WsStream, Message>);

impl FeedSink for WsSink {
    fn send_text(&mut self, text: String) -> BoxFuture<'_, Result<(), IngestError>> {
        Box::pin(async move {
            self.0
                .send(Message::text(text))
                .await
                .map_err(|err| IngestError::Socket(err.to_string()))
        })
    }

    fn ping(&mut self) -> BoxFuture<'_, Result<(), IngestError>> {
        Box::pin(async move {
            self.0
                .send(Message::Ping(Bytes::from_static(b"sts")))
                .await
                .map_err(|err| IngestError::Socket(err.to_string()))
        })
    }
}

struct WsSource(SplitStream<WsStream>);

impl FeedStream for WsSource {
    fn recv(&mut self) -> BoxFuture<'_, Option<Result<FeedMessage, IngestError>>> {
        Box::pin(async move {
            loop {
                return match self.0.next().await? {
                    Ok(Message::Text(text)) => Some(Ok(FeedMessage::Frame(Bytes::from(text)))),
                    Ok(Message::Binary(bytes)) => Some(Ok(FeedMessage::Frame(bytes))),
                    Ok(Message::Pong(_)) => Some(Ok(FeedMessage::Pong)),
                    Ok(Message::Close(_)) => Some(Err(IngestError::Closed)),
                    // Pings are answered by tungstenite itself and raw frames
                    // never surface from a read. Neither is news, so the loop
                    // goes round rather than waking the caller for nothing.
                    Ok(Message::Ping(_)) | Ok(Message::Frame(_)) => continue,
                    Err(err) => Some(Err(IngestError::Socket(err.to_string()))),
                };
            }
        })
    }
}

/// Dials Solana JSON-RPC pubsub over WSS.
///
/// TLS is `native-tls`, which is Security.framework on macOS and SChannel on
/// Windows, so the trust roots are the ones the operating system already
/// maintains and the build needs no C crypto toolchain.
pub struct WebSocketDialer;

impl FeedDialer for WebSocketDialer {
    fn dial(&self, endpoint: EndpointConfig) -> BoxFuture<'static, Result<FeedPair, IngestError>> {
        Box::pin(async move {
            // A Geyser endpoint is a valid thing to configure and not a thing
            // this dialer can open. Failing here with a sentence rather than
            // silently treating it as a websocket is the difference between a
            // feed that is visibly absent and one that looks present.
            if endpoint.transport == FeedTransport::Grpc {
                return Err(IngestError::Dial(format!(
                    "{} is configured as a gRPC endpoint; this build dials websockets only",
                    endpoint.redacted()
                )));
            }

            let (socket, _response) = tokio_tungstenite::connect_async(endpoint.url.clone())
                .await
                .map_err(|err| IngestError::Dial(err.to_string()))?;
            let (sink, stream) = socket.split();
            Ok((
                Box::new(WsSink(sink)) as Box<dyn FeedSink>,
                Box::new(WsSource(stream)) as Box<dyn FeedStream>,
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// the SQLite WAL worker
// ---------------------------------------------------------------------------

/// Carries validated candidates to `sts.db` without ever making a socket wait.
///
/// SQLite is synchronous and `rusqlite::Connection` is not `Sync`, so the write
/// happens on a thread of its own rather than on the runtime — a blocking write
/// on a tokio worker would stall every other socket sharing that thread. The
/// channel in is bounded and the send is a `try_send`, so a slow disk costs rows
/// and never costs frames.
///
/// Rows are batched: up to `WAL_BATCH` in one transaction, or whatever has
/// arrived after `WAL_LINGER`. One transaction per row would be one fsync per
/// row, which on a launch burst is the slowest thing in the process.
struct WalWorker {
    tx: crossbeam_channel::Sender<CandidateEvent>,
    /// Set by `stop`. The worker checks it whenever a batch window closes, so
    /// shutdown costs at most one `WAL_LINGER` rather than waiting for every
    /// sender clone held by an aborted task to actually be dropped.
    stopping: Arc<AtomicBool>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl WalWorker {
    fn start(database: Arc<Database>, metrics: Arc<IngestionMetrics>) -> Self {
        let (tx, rx) = crossbeam_channel::bounded::<CandidateEvent>(WAL_DEPTH);
        let stopping = Arc::new(AtomicBool::new(false));
        let handle = std::thread::Builder::new()
            .name("sts-ingest-wal".to_string())
            .spawn({
                let stopping = Arc::clone(&stopping);
                move || wal_loop(rx, database, metrics, stopping)
            })
            .expect("the ingestion WAL worker is the only path from a candidate to sts.db");
        Self {
            tx,
            stopping,
            handle: Mutex::new(Some(handle)),
        }
    }

    fn sender(&self) -> crossbeam_channel::Sender<CandidateEvent> {
        self.tx.clone()
    }

    /// Asks for the last batch and waits for it. Idempotent.
    fn stop(&self) {
        let Some(handle) = self.handle.lock().take() else {
            return;
        };
        self.stopping.store(true, Ordering::SeqCst);
        let _ = handle.join();
    }
}

fn wal_loop(
    rx: crossbeam_channel::Receiver<CandidateEvent>,
    database: Arc<Database>,
    metrics: Arc<IngestionMetrics>,
    stopping: Arc<AtomicBool>,
) {
    // Without the table there is nothing this thread can usefully do, and it
    // must not spin failing on every row. It keeps draining so the socket tasks
    // never block, and every drained row is counted as lost — that count is what
    // tells anyone looking that the trail is not being kept.
    let schema_ok = match database.ensure_ingest_schema() {
        Ok(()) => true,
        Err(err) => {
            eprintln!("STS ingestion: {err}");
            false
        }
    };

    let mut batch: Vec<IngestCandidateRow> = Vec::with_capacity(WAL_BATCH);
    loop {
        match rx.recv_timeout(WAL_LINGER) {
            Ok(event) => {
                if !schema_ok {
                    metrics.dropped_wal.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                batch.push(row_for(&event));
                if batch.len() >= WAL_BATCH {
                    flush(&database, &metrics, &mut batch);
                }
            }
            // The window closed with room to spare: write what there is, and
            // take the chance to notice that shutdown has been asked for.
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                flush(&database, &metrics, &mut batch);
                if stopping.load(Ordering::SeqCst) {
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    flush(&database, &metrics, &mut batch);
}

fn flush(database: &Database, metrics: &IngestionMetrics, batch: &mut Vec<IngestCandidateRow>) {
    if batch.is_empty() {
        return;
    }
    match database.record_ingest_candidates(batch) {
        Ok(written) => {
            metrics
                .wal_rows
                .fetch_add(written as u64, Ordering::Relaxed);
        }
        Err(err) => {
            metrics.wal_failures.fetch_add(1, Ordering::Relaxed);
            metrics
                .dropped_wal
                .fetch_add(batch.len() as u64, Ordering::Relaxed);
            eprintln!(
                "STS ingestion: could not write {} candidates: {err}",
                batch.len()
            );
        }
    }
    batch.clear();
}

/// Renders one event into the row `sts.db` stores.
///
/// This is where keys become base58 text, and it happens on the WAL thread on
/// purpose: rendering a key allocates, and the socket task has a two-millisecond
/// budget it should not spend on formatting.
fn row_for(event: &CandidateEvent) -> IngestCandidateRow {
    IngestCandidateRow {
        source: event.view.provider.as_str().to_string(),
        slot: event.view.slot as i64,
        account: event.view.account.to_string(),
        program: event.view.program.to_string(),
        creator: if event.view.creator.is_zero() {
            None
        } else {
            Some(event.view.creator.to_string())
        },
        route: event.route.as_str().to_string(),
        market_cap_usd_cents: event.market_cap_usd_cents as i64,
        pool_lamports: event.view.pool_lamports as i64,
        curve_progress_bps: event.view.curve_progress_bps as i64,
        observed_at_ms: event.received_at_ms,
        dispatch_latency_us: event.dispatch_latency_us as i64,
    }
}

// ---------------------------------------------------------------------------
// the dispatcher: one frame, start to finish
// ---------------------------------------------------------------------------

/// Everything a socket task needs to turn bytes into a routed candidate.
///
/// Shared by every endpoint task. The only contended state is the launch index,
/// and the lock around it is held for a hash lookup — microseconds, on a path
/// budgeted in milliseconds.
struct Dispatcher {
    filters: StreamFilters,
    /// Micro-dollars per SOL. An atomic rather than a field, so the price can be
    /// updated while the sockets are running without restarting them.
    price: Arc<AtomicU64>,
    metrics: Arc<IngestionMetrics>,
    index: Arc<Mutex<LaunchIndex>>,
    pool: Arc<EndpointPool>,
    fast_tx: mpsc::Sender<CandidateEvent>,
    standard_tx: mpsc::Sender<CandidateEvent>,
    wal_tx: Option<crossbeam_channel::Sender<CandidateEvent>>,
    telemetry: Option<Arc<TelemetryHub>>,
}

impl Dispatcher {
    /// The whole hot path. Everything before the first `?`-shaped early return
    /// is what runs on a frame that turns out to be spam.
    ///
    /// `received` is taken by the caller the instant the socket handed the bytes
    /// over, not here, so the measurement includes this function's own cost
    /// rather than starting after it.
    fn dispatch(&self, provider: FeedProvider, endpoint: usize, frame: &[u8], received: Instant) {
        let at_ms = now_ms();
        self.metrics.observe_frame(frame.len());
        self.pool.record_frame(endpoint, at_ms);

        match self.decide(provider, frame) {
            Err(reason) => {
                self.metrics.count_drop(reason);
                // A rejected frame is still a dispatch: it is the case the
                // budget is mostly spent on, so leaving it out of the timing
                // would flatter the numbers.
                self.metrics.observe_dispatch(received.elapsed());
            }
            Ok((view, route, market_cap_usd_cents)) => {
                // One clock read for both the counter and the row, so a slow
                // frame in `sts.db` is the same slow frame the snapshot counted.
                let elapsed = received.elapsed();
                self.emit(CandidateEvent {
                    view,
                    route,
                    market_cap_usd_cents,
                    received_at_ms: at_ms,
                    dispatch_latency_us: elapsed.as_micros().min(u32::MAX as u128) as u32,
                });
                self.metrics.observe_dispatch(elapsed);
            }
        }
    }

    /// Bytes to verdict, with no side effects other than the launch index.
    fn decide(
        &self,
        provider: FeedProvider,
        frame: &[u8],
    ) -> Result<(CandidateView, Route, u64), DropReason> {
        // 1. Raw bytes. Nothing parsed, nothing allocated.
        self.filters.admits_frame(frame)?;

        // 2. Borrowed parse. Every string below points into `frame`.
        let decoded = decode_frame(frame)?;
        let program = allowed_program_for(decoded.owner).ok_or(DropReason::NotAllowlisted)?;

        // Only pump.fun's bonding curve has a decoder in this build. The other
        // four programs are allowlisted, subscribed to, and counted — and their
        // account layouts are a later phase's work. Saying so with a reason is
        // the difference between a gap and a silent one.
        if program != ALLOWED_PROGRAMS[0].key {
            return Err(DropReason::NoDecoder);
        }

        // Every subscription this build opens is a `programSubscribe`, which
        // always names the account. An `accountNotification` would not, and
        // there is no way to tell which account it was about from the frame.
        let account = decoded.account.ok_or(DropReason::Undecodable)?;
        let account = Pubkey::parse(account).map_err(|_| DropReason::Undecodable)?;

        // 3. Account bytes, decoded into the stack.
        let mut buffer = [0u8; ACCOUNT_BUFFER];
        let len = base64::decode(decoded.data_base64.as_bytes(), &mut buffer)
            .ok_or(DropReason::Undecodable)?;
        let curve = BondingCurve::decode(&buffer[..len]).ok_or(DropReason::Undecodable)?;
        if !curve.is_plausible() {
            return Err(DropReason::Undecodable);
        }

        // 4. Slot watermark. Three providers watching one program report the
        // same update three times; only the first of them is news.
        //
        // Whether the other two are *copies* of that first one is a separate
        // question, and this is the only place in the process that can answer
        // it: two providers describing one slot differently is a disagreement
        // about a fact, and it is counted here rather than resolved. Nothing is
        // rewritten — the write already released stays released — because there
        // is no third source to break the tie and picking a winner would be
        // inventing one.
        let observation =
            self.index
                .lock()
                .observe(account, decoded.slot, provider, curve_digest(&curve));
        if let Some(contradiction) = observation.conflict {
            self.metrics.count_contradiction(contradiction);
        }
        if observation.stale {
            return Err(DropReason::StaleSlot);
        }

        let view = CandidateView {
            provider,
            slot: decoded.slot,
            account,
            program,
            creator: curve.creator,
            market_cap_lamports: curve.market_cap_lamports(),
            pool_lamports: curve.real_sol_reserves,
            virtual_sol_reserves: curve.virtual_sol_reserves,
            virtual_token_reserves: curve.virtual_token_reserves,
            curve_progress_bps: curve.progress_bps(),
            curve_complete: curve.complete,
            slots_since_launch: observation.slots_since_launch,
        };

        let price = SolPrice {
            micro_usd_per_sol: self.price.load(Ordering::Relaxed),
        };
        match self.filters.route(&view, price) {
            Verdict::Dropped(reason) => Err(reason),
            Verdict::Routed(route) => Ok((
                view,
                route,
                price.lamports_to_usd_cents(view.market_cap_lamports),
            )),
        }
    }

    /// The ordered way in: a curve the Geyser pipeline has already decoded,
    /// sequenced and released.
    ///
    /// Steps 1 to 3 of [`decide`](Self::decide) — the raw-bytes pre-filter, the
    /// JSON parse and the base64 account decode — have no counterpart here, and
    /// that is the whole point of the seam. A Geyser subscription is filtered at
    /// the validator, arrives as a typed account write, and has already been put
    /// back into chain order by `subslot::TickRing`. What is left is the part
    /// that is a *decision* rather than a parse, and it is deliberately the same
    /// code: the same launch index, the same watermark, the same routing
    /// thresholds and the same channels. Two feeds that filtered differently
    /// would be two strategies.
    fn admit_curve(
        &self,
        provider: FeedProvider,
        slot: u64,
        account: Pubkey,
        curve: &BondingCurve,
        received: Instant,
    ) -> Verdict {
        let at_ms = now_ms();
        self.metrics.ordered_ticks.fetch_add(1, Ordering::Relaxed);

        if !curve.is_plausible() {
            self.metrics.count_drop(DropReason::Undecodable);
            self.metrics.observe_dispatch(received.elapsed());
            return Verdict::Dropped(DropReason::Undecodable);
        }

        // The watermark still applies, and it is doing a different job here than
        // it does on the websocket path. There it deduplicates three providers
        // reporting one update; here it deduplicates the handful of slots a
        // reconnect deliberately replays — see `geyser::RESUME_OVERLAP_SLOTS`.
        //
        // That replay is also why the disagreement check is across providers
        // and never within one: a resumed stream re-delivers writes this very
        // provider already sent, and a feed reporting itself as contradicting
        // itself every time it reconnects is a feed nobody would read.
        let observation = self
            .index
            .lock()
            .observe(account, slot, provider, curve_digest(curve));
        if let Some(contradiction) = observation.conflict {
            self.metrics.count_contradiction(contradiction);
        }
        if observation.stale {
            self.metrics.count_drop(DropReason::StaleSlot);
            self.metrics.observe_dispatch(received.elapsed());
            return Verdict::Dropped(DropReason::StaleSlot);
        }

        let view = CandidateView {
            provider,
            slot,
            account,
            program: ALLOWED_PROGRAMS[0].key,
            creator: curve.creator,
            market_cap_lamports: curve.market_cap_lamports(),
            pool_lamports: curve.real_sol_reserves,
            virtual_sol_reserves: curve.virtual_sol_reserves,
            virtual_token_reserves: curve.virtual_token_reserves,
            curve_progress_bps: curve.progress_bps(),
            curve_complete: curve.complete,
            slots_since_launch: observation.slots_since_launch,
        };

        let price = SolPrice {
            micro_usd_per_sol: self.price.load(Ordering::Relaxed),
        };
        let verdict = self.filters.route(&view, price);
        let elapsed = received.elapsed();
        match verdict {
            Verdict::Dropped(reason) => {
                self.metrics.count_drop(reason);
            }
            Verdict::Routed(route) => {
                self.emit(CandidateEvent {
                    view,
                    route,
                    market_cap_usd_cents: price.lamports_to_usd_cents(view.market_cap_lamports),
                    received_at_ms: at_ms,
                    dispatch_latency_us: elapsed.as_micros().min(u32::MAX as u128) as u32,
                });
            }
        }
        self.metrics.observe_dispatch(elapsed);
        verdict
    }

    /// Walks the launch index back to before a fork switch.
    ///
    /// Returns how many records were touched.
    fn rewind(&self, from_slot: u64) -> usize {
        let touched = self.index.lock().rewind(from_slot);
        self.metrics.rewinds.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .rewound_accounts
            .fetch_add(touched as u64, Ordering::Relaxed);
        touched
    }

    /// Puts a candidate on its channels. Never blocks, on any of them.
    fn emit(&self, event: CandidateEvent) {
        self.metrics.candidates.fetch_add(1, Ordering::Relaxed);

        let channel = match event.route {
            Route::FastPath => {
                self.metrics.fast_path.fetch_add(1, Ordering::Relaxed);
                &self.fast_tx
            }
            Route::Standard => &self.standard_tx,
        };
        if channel.try_send(event).is_err() {
            match event.route {
                Route::FastPath => self
                    .metrics
                    .dropped_fast_path
                    .fetch_add(1, Ordering::Relaxed),
                Route::Standard => self
                    .metrics
                    .dropped_standard
                    .fetch_add(1, Ordering::Relaxed),
            };
        }

        // The audit trail is a separate send from the scoring path on purpose:
        // a full scoring queue must not also cost the record of what arrived.
        if let Some(wal) = &self.wal_tx {
            if wal.try_send(event).is_err() {
                self.metrics.dropped_wal.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn report(&self, level: TelemetryLevel, message: impl Into<String>, data: serde_json::Value) {
        if let Some(hub) = &self.telemetry {
            hub.publish(level, "ingestion", message, data);
        }
    }
}

// ---------------------------------------------------------------------------
// one endpoint, forever
// ---------------------------------------------------------------------------

/// Dials, subscribes, reads, and does it again.
///
/// The reconnect is the failover: while this endpoint is backing off, the other
/// providers are still reading, and `EndpointPool` has already stopped routing
/// one-shot requests here. There is no explicit "switch to the backup" step
/// because there is no backup — every endpoint is always running.
async fn run_endpoint(
    index: usize,
    config: EndpointConfig,
    dialer: Arc<dyn FeedDialer>,
    dispatcher: Arc<Dispatcher>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            return;
        }

        let dialed = tokio::select! {
            _ = shutdown.changed() => return,
            result = dialer.dial(config.clone()) => result,
        };

        let (mut sink, mut stream) = match dialed {
            Ok(pair) => pair,
            Err(err) => {
                dispatcher
                    .metrics
                    .connect_failures
                    .fetch_add(1, Ordering::Relaxed);
                let backoff = dispatcher.pool.record_failure(index);
                dispatcher.report(
                    TelemetryLevel::Warn,
                    format!("{} could not be reached", config.provider),
                    serde_json::json!({
                        "provider": config.provider,
                        "endpoint": config.redacted(),
                        "error": err.to_string(),
                        "retryInMs": backoff.as_millis() as u64,
                    }),
                );
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = tokio::time::sleep(backoff) => continue,
                }
            }
        };

        dispatcher.pool.record_connected(index);
        dispatcher.metrics.connects.fetch_add(1, Ordering::Relaxed);
        dispatcher.report(
            TelemetryLevel::Info,
            format!("{} stream open", config.provider),
            serde_json::json!({ "provider": config.provider, "endpoint": config.redacted() }),
        );

        // The reply to the first subscription is the first round trip this
        // socket has ever been measured on, which is why the clock starts here.
        let mut awaiting_ack = Some(Instant::now());
        let mut subscribe_failed = false;
        for request in subscription_requests() {
            if let Err(err) = sink.send_text(request).await {
                dispatcher.report(
                    TelemetryLevel::Warn,
                    format!("{} refused a subscription", config.provider),
                    serde_json::json!({ "provider": config.provider, "error": err.to_string() }),
                );
                subscribe_failed = true;
                break;
            }
        }

        if !subscribe_failed {
            read_until_closed(
                index,
                &config,
                &dispatcher,
                &mut sink,
                &mut stream,
                &mut shutdown,
                &mut awaiting_ack,
            )
            .await;
        }

        if *shutdown.borrow() {
            return;
        }

        dispatcher
            .metrics
            .disconnects
            .fetch_add(1, Ordering::Relaxed);
        let backoff = dispatcher.pool.record_failure(index);
        dispatcher.report(
            TelemetryLevel::Warn,
            format!("{} stream closed", config.provider),
            serde_json::json!({
                "provider": config.provider,
                "endpoint": config.redacted(),
                "retryInMs": backoff.as_millis() as u64,
            }),
        );
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tokio::time::sleep(backoff) => {}
        }
    }
}

/// The read loop for one connection. Returns when the socket ends, goes quiet,
/// or shutdown is signalled.
async fn read_until_closed(
    index: usize,
    config: &EndpointConfig,
    dispatcher: &Arc<Dispatcher>,
    sink: &mut Box<dyn FeedSink>,
    stream: &mut Box<dyn FeedStream>,
    shutdown: &mut watch::Receiver<bool>,
    awaiting_ack: &mut Option<Instant>,
) {
    let mut heartbeat = tokio::time::interval(HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // `interval` fires immediately the first time, which would ping before the
    // subscriptions have been answered.
    heartbeat.tick().await;

    let mut ping_sent_at: Option<Instant> = None;
    let mut quiet_ticks: u32 = 0;

    loop {
        tokio::select! {
            _ = shutdown.changed() => return,

            _ = heartbeat.tick() => {
                quiet_ticks += 1;
                // A socket that has said nothing for `IDLE_TIMEOUT` is treated
                // as dead even though TCP still believes in it. Reconnecting a
                // live-but-silent socket costs a handshake; trusting one costs
                // the feed.
                if HEARTBEAT * quiet_ticks > IDLE_TIMEOUT {
                    dispatcher.report(
                        TelemetryLevel::Warn,
                        format!("{} went quiet", config.provider),
                        serde_json::json!({
                            "provider": config.provider,
                            "idleMs": (HEARTBEAT * quiet_ticks).as_millis() as u64,
                        }),
                    );
                    return;
                }
                ping_sent_at = Some(Instant::now());
                if sink.ping().await.is_err() {
                    return;
                }
            }

            message = stream.recv() => {
                // Taken before anything else happens to the message, so the
                // budget covers the whole trip rather than part of it.
                let received = Instant::now();
                match message {
                    None | Some(Err(_)) => return,
                    Some(Ok(FeedMessage::Pong)) => {
                        if let Some(sent) = ping_sent_at.take() {
                            dispatcher.pool.record_latency(index, millis_since(sent));
                        }
                    }
                    Some(Ok(FeedMessage::Frame(bytes))) => {
                        quiet_ticks = 0;
                        if let Some(sent) = awaiting_ack.take() {
                            dispatcher.pool.record_latency(index, millis_since(sent));
                        }
                        dispatcher.dispatch(config.provider, index, &bytes, received);
                    }
                }
            }
        }
    }
}

/// Elapsed milliseconds, saturating, because a latency window holds `u32`.
fn millis_since(start: Instant) -> u32 {
    start.elapsed().as_millis().min(u32::MAX as u128) as u32
}

// ---------------------------------------------------------------------------
// the manager
// ---------------------------------------------------------------------------

/// How often the ingestion counters are published to telemetry.
const TELEMETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Everything the manager needs to be told.
#[derive(Debug, Clone)]
pub struct IngestionConfig {
    pub endpoints: Vec<EndpointConfig>,
    pub filters: StreamFilters,
    /// What SOL is worth when the sockets open. Can be changed later with
    /// `set_sol_price`; starts unknown, which makes every candidate look too
    /// small to trade rather than too big.
    pub price: SolPrice,
    pub telemetry_interval: Duration,
}

impl IngestionConfig {
    /// Whatever the environment says, with the standard filters.
    ///
    /// On a checkout with no provider URLs this is an empty endpoint list, and
    /// an empty endpoint list is a manager that dials nothing.
    pub fn from_env() -> Self {
        Self {
            endpoints: endpoints_from_env(),
            filters: StreamFilters::DEFAULT,
            price: SolPrice::UNKNOWN,
            telemetry_interval: TELEMETRY_INTERVAL,
        }
    }
}

impl Default for IngestionConfig {
    fn default() -> Self {
        Self {
            endpoints: Vec::new(),
            filters: StreamFilters::DEFAULT,
            price: SolPrice::UNKNOWN,
            telemetry_interval: TELEMETRY_INTERVAL,
        }
    }
}

/// The two channels out of ingestion.
///
/// Handed to the caller rather than kept, because the thing that consumes them
/// is the scoring engine and it does not exist yet. Whoever holds these is the
/// consumer; nobody holding them means every candidate is counted as dropped,
/// which is the honest reading of a feed nothing is listening to.
pub struct IngestionStreams {
    /// Candidates inside the target window. Shallow queue, meant to be drained
    /// immediately.
    pub fast_path: mpsc::Receiver<CandidateEvent>,
    /// Everything else that passed the filters.
    pub standard: mpsc::Receiver<CandidateEvent>,
}

/// The ingestion layer, as one handle.
pub struct IngestionManager {
    pool: Arc<EndpointPool>,
    metrics: Arc<IngestionMetrics>,
    index: Arc<Mutex<LaunchIndex>>,
    price: Arc<AtomicU64>,
    filters: StreamFilters,
    shutdown: watch::Sender<bool>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    wal: Option<WalWorker>,
    /// Kept rather than only handed to the socket tasks, because the Geyser
    /// pipeline is a second producer into the same channels and it needs the
    /// same filters, the same index and the same routing to reach them.
    dispatcher: Arc<Dispatcher>,
}

impl IngestionManager {
    /// Starts one task per endpoint, plus the telemetry task and the WAL worker.
    ///
    /// Must be called from inside a tokio runtime. Returns immediately: the
    /// sockets dial in the background and the pool reports what happened.
    ///
    /// `database` and `telemetry` are optional so the whole layer can be stood
    /// up in a test without a `sts.db` or a window to publish into. In the
    /// application both are always present.
    pub fn start(
        config: IngestionConfig,
        dialer: Arc<dyn FeedDialer>,
        database: Option<Arc<Database>>,
        telemetry: Option<Arc<TelemetryHub>>,
    ) -> (Arc<Self>, IngestionStreams) {
        let pool = Arc::new(EndpointPool::new(config.endpoints.clone()));
        let metrics = Arc::new(IngestionMetrics::default());
        let index = Arc::new(Mutex::new(LaunchIndex::new(LAUNCH_INDEX_CAPACITY)));
        let price = Arc::new(AtomicU64::new(config.price.micro_usd_per_sol));
        let (shutdown, shutdown_rx) = watch::channel(false);

        let (fast_tx, fast_path) = mpsc::channel(FAST_PATH_DEPTH);
        let (standard_tx, standard) = mpsc::channel(STANDARD_DEPTH);

        let wal = database.map(|db| WalWorker::start(db, Arc::clone(&metrics)));

        let dispatcher = Arc::new(Dispatcher {
            filters: config.filters,
            price: Arc::clone(&price),
            metrics: Arc::clone(&metrics),
            index: Arc::clone(&index),
            pool: Arc::clone(&pool),
            fast_tx,
            standard_tx,
            wal_tx: wal.as_ref().map(|w| w.sender()),
            telemetry: telemetry.clone(),
        });

        let mut tasks = Vec::with_capacity(config.endpoints.len() + 1);
        for (index_of, endpoint) in config.endpoints.iter().enumerate() {
            tasks.push(tokio::spawn(run_endpoint(
                index_of,
                endpoint.clone(),
                Arc::clone(&dialer),
                Arc::clone(&dispatcher),
                shutdown_rx.clone(),
            )));
        }

        if let Some(hub) = telemetry {
            hub.publish(
                TelemetryLevel::Info,
                "ingestion",
                if config.endpoints.is_empty() {
                    "ingestion started with no configured endpoints".to_string()
                } else {
                    format!(
                        "ingestion started across {} endpoints",
                        config.endpoints.len()
                    )
                },
                serde_json::json!({
                    "endpoints": config.endpoints.iter().map(|e| serde_json::json!({
                        "provider": e.provider,
                        "transport": e.transport,
                        "url": e.redacted(),
                    })).collect::<Vec<_>>(),
                    "dispatchBudgetUs": DISPATCH_BUDGET.as_micros() as u64,
                }),
            );
            tasks.push(tokio::spawn(telemetry_loop(
                Arc::clone(&metrics),
                Arc::clone(&pool),
                Arc::clone(&index),
                hub,
                config.telemetry_interval,
                shutdown_rx.clone(),
            )));
        }

        let manager = Arc::new(Self {
            pool,
            metrics,
            index,
            price,
            filters: config.filters,
            shutdown,
            tasks: Mutex::new(tasks),
            wal,
            dispatcher,
        });

        (
            manager,
            IngestionStreams {
                fast_path,
                standard,
            },
        )
    }

    /// The counters, the endpoint health, and the rates as of the last window.
    /// Free of side effects, so the UI can poll it as often as it likes.
    pub fn snapshot(&self) -> IngestionSnapshot {
        self.metrics.snapshot(&self.pool, self.index.lock().len())
    }

    /// Which endpoint a one-shot RPC should use, by health then latency then
    /// weight. `None` only when nothing is configured.
    pub fn pick_endpoint(&self) -> Option<EndpointConfig> {
        let index = self.pool.pick()?;
        self.pool.config(index)
    }

    pub fn endpoint_count(&self) -> usize {
        self.pool.len()
    }

    pub fn filters(&self) -> StreamFilters {
        self.filters
    }

    /// Admits one curve write that arrived over Geyser, already in chain order.
    ///
    /// This is the seam the sub-slot pipeline plugs into. `geyser::TickPipeline`
    /// does the sequencing, the re-org rollback and the write-version guard; the
    /// event it releases arrives here and takes exactly the path a websocket
    /// frame takes from the moment it has been decoded — launch index, spam
    /// floor, liquidity, target window, fast path or standard, WAL.
    ///
    /// `received` starts this candidate's dispatch clock, and the caller reads
    /// it once per released batch rather than per event. It measures the
    /// hand-off and not the hold that ordering cost — the hold is a deliberate
    /// wait rather than a slow dispatch, and `subslot::RingMetrics` is where it
    /// is reported.
    ///
    /// Returns the verdict so the caller can count what the filters said without
    /// reading it back out of a snapshot.
    pub fn admit_curve(
        &self,
        provider: FeedProvider,
        slot: u64,
        account: Pubkey,
        curve: &BondingCurve,
        received: Instant,
    ) -> Verdict {
        self.dispatcher
            .admit_curve(provider, slot, account, curve, received)
    }

    /// Walks the launch index back to before a fork switch, and reports how many
    /// records it touched.
    ///
    /// Called when the Geyser pipeline says a re-org reached slots it had
    /// already released. Nothing downstream can be unsent, but the index can be
    /// put back into a state where the winning fork's rewrite of those slots is
    /// heard rather than dropped as stale — which is the difference between a
    /// bad minute and a wrong price held until the curve trades again.
    pub fn rewind_launches(&self, from_slot: u64) -> usize {
        self.dispatcher.rewind(from_slot)
    }

    /// Updates what SOL is worth. Takes effect on the next frame; the sockets
    /// are not restarted and nothing in flight is re-judged.
    pub fn set_sol_price(&self, price: SolPrice) {
        self.price.store(price.micro_usd_per_sol, Ordering::Relaxed);
    }

    pub fn sol_price(&self) -> SolPrice {
        SolPrice {
            micro_usd_per_sol: self.price.load(Ordering::Relaxed),
        }
    }

    /// Stops every socket, then waits for the last rows to reach `sts.db`.
    ///
    /// Safe to call twice and safe to call from a non-async context, which is
    /// what the Tauri exit handler is. Ordering matters: the tasks are told to
    /// stop before the WAL worker is joined, so nothing is still producing rows
    /// while the writer is trying to finish.
    pub fn stop(&self) {
        let _ = self.shutdown.send(true);
        for task in self.tasks.lock().drain(..) {
            task.abort();
        }
        // The tasks are gone, so nothing is connected any more. Said here
        // rather than left to the read loops to notice, because `abort` does
        // not give them a chance to: a cancelled task runs no more code, and
        // the endpoint it was reading would otherwise report itself connected
        // for the rest of the process.
        self.pool.record_all_disconnected();
        if let Some(wal) = &self.wal {
            wal.stop();
        }
    }
}

/// Publishes the counters on a fixed interval and rolls the rate window.
async fn telemetry_loop(
    metrics: Arc<IngestionMetrics>,
    pool: Arc<EndpointPool>,
    index: Arc<Mutex<LaunchIndex>>,
    hub: Arc<TelemetryHub>,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await;

    // How many disagreements have already been spoken about. A tick that adds
    // none says nothing: the counter is published every time round regardless,
    // and a warning repeated every five seconds for a divergence that happened
    // once is how a warning stops being read.
    let mut reported_contradictions = 0u64;

    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = ticker.tick() => {
                let snapshot = metrics.snapshot(&pool, index.lock().len());
                // Anything lost, or any dispatch past budget, is worth a louder
                // line than the routine one — those are the two numbers that say
                // the engine is not keeping up with the market.
                let losing = snapshot.dropped_fast_path
                    + snapshot.dropped_standard
                    + snapshot.dropped_wal
                    + snapshot.over_budget
                    > 0;
                hub.publish(
                    if losing { TelemetryLevel::Warn } else { TelemetryLevel::Debug },
                    "ingestion",
                    "ingestion metrics",
                    serde_json::to_value(&snapshot)
                        .unwrap_or_else(|_| serde_json::json!({ "error": "metrics would not serialise" })),
                );
                if snapshot.contradictions > reported_contradictions {
                    hub.publish(
                        TelemetryLevel::Warn,
                        "ingestion",
                        "providers disagree about a curve write",
                        serde_json::json!({
                            "since": snapshot.contradictions - reported_contradictions,
                            "total": snapshot.contradictions,
                            // The most recent one, which is not necessarily the
                            // only one this line is about. `since` is the count;
                            // this is the example.
                            "last": snapshot.last_contradiction,
                        }),
                    );
                    reported_contradictions = snapshot.contradictions;
                }
                metrics.roll_window(snapshot.at_ms, snapshot.frames, snapshot.candidates);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// $150.00 per SOL. Every dollar figure asserted below is at this price.
    const PRICE: SolPrice = SolPrice {
        micro_usd_per_sol: 150_000_000,
    };
    /// One billion tokens at six decimals, which is what pump.fun mints.
    const SUPPLY: u64 = 1_000_000_000_000_000;

    // Curve states, as (virtual_sol, virtual_token, real_sol). The market caps
    // they work out to are asserted in `the_fixtures_are_worth_what_they_claim`,
    // so every routing test below is anchored to a number rather than to itself.
    const FRESH_LAUNCH: (u64, u64, u64) = (30_000_000_000, 1_073_000_000_000_000, 0);
    const BELOW_WINDOW: (u64, u64, u64) = (73_000_000_000, 441_000_000_000_000, 43_000_000_000);
    const IN_WINDOW: (u64, u64, u64) = (92_650_000_000, 347_400_000_000_000, 62_650_000_000);
    const ABOVE_WINDOW: (u64, u64, u64) = (160_000_000_000, 200_000_000_000_000, 100_000_000_000);
    // `IN_WINDOW` with ten million more lamports of real SOL in the curve. The
    // market cap is set by the virtual pair, so this routes exactly as
    // `IN_WINDOW` does and is a different state — which is what a test about
    // two providers disagreeing needs, rather than a state one of them would
    // have filtered anyway.
    const IN_WINDOW_MOVED: (u64, u64, u64) = (92_650_000_000, 347_400_000_000_000, 62_660_000_000);

    // -- fixtures -----------------------------------------------------------

    fn key(seed: u8) -> Pubkey {
        let mut bytes = [seed; 32];
        // Never all-zero: that is the System Program and `is_zero` means
        // "the decode found nothing" everywhere else in this codebase.
        bytes[0] = seed.wrapping_add(1);
        Pubkey::new(bytes)
    }

    /// A pump.fun `BondingCurve` account, laid out the way the program does.
    fn curve_account(state: (u64, u64, u64), complete: bool, creator: Option<Pubkey>) -> Vec<u8> {
        let (virtual_sol, virtual_token, real_sol) = state;
        let mut bytes = Vec::with_capacity(CURVE_WITH_CREATOR_LEN);
        bytes.extend_from_slice(&[0xa1u8; 8]); // discriminator, deliberately not checked
        bytes.extend_from_slice(&virtual_token.to_le_bytes());
        bytes.extend_from_slice(&virtual_sol.to_le_bytes());
        bytes.extend_from_slice(&(SUPPLY / 2).to_le_bytes()); // real token reserves
        bytes.extend_from_slice(&real_sol.to_le_bytes());
        bytes.extend_from_slice(&SUPPLY.to_le_bytes());
        bytes.push(u8::from(complete));
        if let Some(creator) = creator {
            bytes.extend_from_slice(creator.as_bytes());
        }
        bytes
    }

    /// Base64 the other way round, so the tests build what the decoder reads.
    /// The same account, decoded: what the ordered path admits and what a
    /// digest is taken over.
    fn curve(state: (u64, u64, u64)) -> BondingCurve {
        BondingCurve::decode(&curve_account(state, false, None)).expect("the fixture is a curve")
    }

    fn b64(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let triple = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let packed = (triple[0] as u32) << 16 | (triple[1] as u32) << 8 | triple[2] as u32;
            out.push(ALPHABET[(packed >> 18) as usize & 63] as char);
            out.push(ALPHABET[(packed >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(packed >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[packed as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    /// A `programNotification` frame shaped the way a provider sends one.
    fn program_frame(slot: u64, account: &Pubkey, owner: &str, data: &[u8]) -> String {
        format!(
            concat!(
                r#"{{"jsonrpc":"2.0","method":"programNotification","params":{{"#,
                r#""result":{{"context":{{"slot":{slot}}},"value":{{"pubkey":"{account}","#,
                r#""account":{{"lamports":1461600,"data":["{data}","base64"],"#,
                r#""owner":"{owner}","executable":false,"rentEpoch":18446744073709551615,"#,
                r#""space":81}}}}}},"subscription":7}}}}"#
            ),
            slot = slot,
            account = account,
            data = b64(data),
            owner = owner,
        )
    }

    /// A pump.fun frame for one account in one curve state.
    fn pump_frame(slot: u64, account: &Pubkey, state: (u64, u64, u64)) -> String {
        program_frame(
            slot,
            account,
            PUMP_FUN_PROGRAM,
            &curve_account(state, false, Some(key(9))),
        )
    }

    // -- the constants ------------------------------------------------------

    #[test]
    fn program_ids_match_their_text() {
        // The byte arrays are what the hot path compares and the strings are
        // what a person checks against an explorer. This is the only thing
        // keeping the two honest.
        for program in ALLOWED_PROGRAMS.iter() {
            let parsed = Pubkey::parse(program.text)
                .unwrap_or_else(|err| panic!("{} is not base58: {err}", program.text));
            assert_eq!(
                parsed, program.key,
                "{} decodes to different bytes",
                program.text
            );
            assert_eq!(
                program.key.to_string(),
                program.text,
                "{} does not round trip",
                program.text
            );
        }
    }

    #[test]
    fn the_allowlist_is_only_pump_fun_and_raydium() {
        assert_eq!(ALLOWED_PROGRAMS.len(), 5);
        assert!(is_allowed_program(&ALLOWED_PROGRAMS[0].key));
        assert!(
            !is_allowed_program(&key(3)),
            "an arbitrary key is not a program we listen to"
        );
        // The dispatcher relies on the first entry being pump.fun, because it is
        // the only one with an account decoder.
        assert_eq!(ALLOWED_PROGRAMS[0].text, PUMP_FUN_PROGRAM);
    }

    // -- base64 -------------------------------------------------------------

    #[test]
    fn base64_decodes_into_a_buffer_the_caller_already_has() {
        let cases: [(&str, &[u8]); 5] = [
            ("", b""),
            ("QQ==", b"A"),
            ("QUI=", b"AB"),
            ("QUJD", b"ABC"),
            ("QUJDRA==", b"ABCD"),
        ];
        for (text, expected) in cases {
            let mut buffer = [0u8; 16];
            let len = base64::decode(text.as_bytes(), &mut buffer).expect("valid base64");
            assert_eq!(&buffer[..len], expected, "{text} decoded wrong");
        }
    }

    #[test]
    fn base64_refuses_what_it_cannot_decode_rather_than_half_decoding_it() {
        let mut buffer = [0u8; 64];
        // A character outside the alphabet.
        assert!(base64::decode(b"QUJD!!!!", &mut buffer).is_none());
        // A trailing group of one, which encodes nothing.
        assert!(base64::decode(b"QUJDQ", &mut buffer).is_none());
        // More bytes than the caller has room for. A half-written account would
        // parse as a curve holding some other number, which is worse than none.
        let mut tiny = [0u8; 2];
        assert!(base64::decode(b"QUJDRA==", &mut tiny).is_none());
    }

    #[test]
    fn base64_round_trips_every_byte() {
        let all: Vec<u8> = (0..=255u8).collect();
        for length in [1usize, 2, 3, 47, 49, 81, 200] {
            let source = &all[..length.min(all.len())];
            let encoded = b64(source);
            let mut buffer = [0u8; 512];
            let len = base64::decode(encoded.as_bytes(), &mut buffer).expect("round trips");
            assert_eq!(&buffer[..len], source, "length {length} did not survive");
        }
    }

    // -- the bonding curve --------------------------------------------------

    #[test]
    fn a_curve_decodes_from_either_layout() {
        let with_creator = curve_account(IN_WINDOW, false, Some(key(9)));
        assert_eq!(with_creator.len(), CURVE_WITH_CREATOR_LEN);
        let decoded = BondingCurve::decode(&with_creator).expect("decodes");
        assert_eq!(decoded.creator, key(9));
        assert!(decoded.is_plausible());

        // The older account has no creator field, and an absent creator reads as
        // zero — which `Pubkey::is_zero` already means "not found" for.
        let without_creator = curve_account(IN_WINDOW, false, None);
        assert_eq!(without_creator.len(), CURVE_MIN_LEN);
        let decoded = BondingCurve::decode(&without_creator).expect("decodes");
        assert!(decoded.creator.is_zero());
    }

    #[test]
    fn an_account_too_short_to_be_a_curve_is_refused() {
        let short = curve_account(IN_WINDOW, false, None);
        assert!(BondingCurve::decode(&short[..CURVE_MIN_LEN - 1]).is_none());
        assert!(BondingCurve::decode(&[]).is_none());
    }

    #[test]
    fn the_fixtures_are_worth_what_they_claim() {
        // Every routing test below leans on these four numbers, so they are
        // stated once, in dollars, against the curve maths rather than against
        // whatever the code happens to produce.
        let cases = [
            (FRESH_LAUNCH, 419_384u64),
            (BELOW_WINDOW, 2_482_993),
            (IN_WINDOW, 4_000_431),
            (ABOVE_WINDOW, 12_000_000),
        ];
        for (state, expected_cents) in cases {
            let curve = BondingCurve::decode(&curve_account(state, false, None)).expect("decodes");
            let cents = PRICE.lamports_to_usd_cents(curve.market_cap_lamports());
            assert_eq!(
                cents, expected_cents,
                "curve {state:?} is not worth what it should be"
            );
        }
    }

    #[test]
    fn a_curve_with_no_token_reserves_reports_nothing_rather_than_dividing_by_zero() {
        let curve = BondingCurve {
            virtual_token_reserves: 0,
            virtual_sol_reserves: 30_000_000_000,
            real_token_reserves: 0,
            real_sol_reserves: 0,
            token_total_supply: SUPPLY,
            complete: false,
            creator: Pubkey::ZERO,
        };
        assert_eq!(curve.market_cap_lamports(), 0);
        assert!(
            !curve.is_plausible(),
            "an account read mid-write is not a cheap coin"
        );
    }

    #[test]
    fn progress_is_measured_against_graduation_and_capped_there() {
        let curve = BondingCurve::decode(&curve_account(IN_WINDOW, false, None)).expect("decodes");
        // 62.65 of the 85 SOL it takes to graduate.
        assert_eq!(curve.progress_bps(), 7370);

        let past = BondingCurve::decode(&curve_account(
            (160_000_000_000, 200_000_000_000_000, 500 * LAMPORTS_PER_SOL),
            false,
            None,
        ))
        .expect("decodes");
        assert_eq!(
            past.progress_bps(),
            BPS_DENOMINATOR as u16,
            "progress never exceeds 100%"
        );

        let done = BondingCurve::decode(&curve_account(IN_WINDOW, true, None)).expect("decodes");
        assert_eq!(done.progress_bps(), BPS_DENOMINATOR as u16);
    }

    #[test]
    fn a_view_carries_the_price_setting_reserves_beside_the_executable_one() {
        let view = view_for(IN_WINDOW, 40);

        // Real SOL is what an exit can be sold into; virtual SOL is what the
        // quoted price is computed against. A window given only the first can
        // report neither the ratio between them nor the sandwich threshold of
        // REPLAY_AND_SIMULATION_SPEC.md 15.2, which is written in terms of the
        // virtual reserve.
        assert_eq!(view.pool_lamports, 62_650_000_000);
        assert_eq!(view.virtual_sol_reserves, 92_650_000_000);
        assert_eq!(view.virtual_token_reserves, 347_400_000_000_000);

        // And they must not be the same number under two names: the gap is the
        // part of the price that is not there to sell into.
        assert!(view.virtual_sol_reserves > view.pool_lamports);
    }

    // -- money --------------------------------------------------------------

    #[test]
    fn lamports_become_cents_in_integers() {
        assert_eq!(
            PRICE.lamports_to_usd_cents(LAMPORTS_PER_SOL),
            15_000,
            "one SOL is $150.00"
        );
        assert_eq!(PRICE.lamports_to_usd_cents(0), 0);
        // Rounds down rather than up: a candidate is never reported as bigger
        // than it is, because every threshold here is a floor.
        assert_eq!(PRICE.lamports_to_usd_cents(1), 0);
        assert_eq!(SolPrice::from_usd_cents(15_000), PRICE);
    }

    #[test]
    fn an_unknown_price_makes_everything_look_too_small_to_trade() {
        assert!(!SolPrice::UNKNOWN.is_known());
        assert_eq!(
            SolPrice::UNKNOWN.lamports_to_usd_cents(1_000 * LAMPORTS_PER_SOL),
            0
        );

        // Which means the filters refuse it, rather than letting an unpriced
        // candidate through as if it were free.
        let view = view_for(IN_WINDOW, 100);
        assert_eq!(
            StreamFilters::DEFAULT.route(&view, SolPrice::UNKNOWN),
            Verdict::Dropped(DropReason::TooSmall)
        );
    }

    #[test]
    fn a_market_cap_far_past_a_u64_does_not_wrap() {
        let curve = BondingCurve {
            virtual_token_reserves: 1,
            virtual_sol_reserves: u64::MAX,
            real_token_reserves: 0,
            real_sol_reserves: 0,
            token_total_supply: u64::MAX,
            complete: false,
            creator: Pubkey::ZERO,
        };
        assert_eq!(
            curve.market_cap_lamports(),
            u64::MAX,
            "saturates instead of wrapping to zero"
        );
    }

    // -- filters ------------------------------------------------------------

    /// A candidate as the filters see it, with an age in slots.
    fn view_for(state: (u64, u64, u64), slots_since_launch: u64) -> CandidateView {
        let curve = BondingCurve::decode(&curve_account(state, false, None)).expect("decodes");
        CandidateView {
            provider: FeedProvider::Helius,
            slot: 1_000 + slots_since_launch,
            account: key(1),
            program: ALLOWED_PROGRAMS[0].key,
            creator: curve.creator,
            market_cap_lamports: curve.market_cap_lamports(),
            pool_lamports: curve.real_sol_reserves,
            virtual_sol_reserves: curve.virtual_sol_reserves,
            virtual_token_reserves: curve.virtual_token_reserves,
            curve_progress_bps: curve.progress_bps(),
            curve_complete: curve.complete,
            slots_since_launch,
        }
    }

    #[test]
    fn a_frame_that_never_names_an_allowlisted_program_is_refused_on_raw_bytes() {
        let filters = StreamFilters::DEFAULT;
        let stranger = program_frame(
            1_000,
            &key(1),
            "So11111111111111111111111111111111111111112",
            b"x",
        );
        assert_eq!(
            filters.admits_frame(stranger.as_bytes()),
            Err(DropReason::NotAllowlisted)
        );
    }

    #[test]
    fn an_acknowledgement_is_not_a_notification() {
        let filters = StreamFilters::DEFAULT;
        // What a provider sends back the moment a subscription is accepted.
        assert_eq!(
            filters.admits_frame(br#"{"jsonrpc":"2.0","result":24040,"id":1}"#),
            Err(DropReason::NotANotification)
        );
        // And what it sends when it is not accepted.
        assert_eq!(
            filters.admits_frame(br#"{"jsonrpc":"2.0","error":{"code":-32602},"id":1}"#),
            Err(DropReason::NotANotification)
        );
    }

    #[test]
    fn a_pump_fun_frame_gets_past_the_pre_filter() {
        let filters = StreamFilters::DEFAULT;
        let frame = pump_frame(1_000, &key(1), IN_WINDOW);
        assert_eq!(filters.admits_frame(frame.as_bytes()), Ok(()));
    }

    #[test]
    fn the_target_window_is_the_only_thing_that_reaches_the_fast_path() {
        let filters = StreamFilters::DEFAULT;
        let cases = [
            (FRESH_LAUNCH, Verdict::Dropped(DropReason::TooSmall)),
            (BELOW_WINDOW, Verdict::Routed(Route::Standard)),
            (IN_WINDOW, Verdict::Routed(Route::FastPath)),
            (ABOVE_WINDOW, Verdict::Routed(Route::Standard)),
        ];
        for (state, expected) in cases {
            let view = view_for(state, 100);
            assert_eq!(
                filters.route(&view, PRICE),
                expected,
                "curve {state:?} routed wrong"
            );
        }
    }

    #[test]
    fn the_window_is_inclusive_at_both_ends() {
        let window = TargetWindow::DEFAULT;
        assert!(window.contains(window.low_usd_cents));
        assert!(window.contains(window.high_usd_cents));
        assert!(!window.contains(window.low_usd_cents - 1));
        assert!(!window.contains(window.high_usd_cents + 1));
    }

    #[test]
    fn the_first_ten_slots_are_a_bot_lottery_however_big_they_look() {
        let filters = StreamFilters::DEFAULT;
        for age in 0..filters.spam_floor.min_slots_since_launch {
            assert_eq!(
                filters.route(&view_for(IN_WINDOW, age), PRICE),
                Verdict::Dropped(DropReason::LotterySlot),
                "slot {age} should still be the lottery"
            );
        }
        assert_eq!(
            filters.route(
                &view_for(IN_WINDOW, filters.spam_floor.min_slots_since_launch),
                PRICE
            ),
            Verdict::Routed(Route::FastPath),
            "the eleventh slot is the first one worth believing"
        );
    }

    #[test]
    fn a_completed_curve_is_past_the_window_this_engine_trades() {
        let mut view = view_for(IN_WINDOW, 100);
        view.curve_complete = true;
        assert_eq!(
            StreamFilters::DEFAULT.route(&view, PRICE),
            Verdict::Dropped(DropReason::Graduated)
        );
    }

    #[test]
    fn a_pool_too_thin_to_sell_into_is_refused_even_inside_the_window() {
        let filters = StreamFilters::DEFAULT;
        let mut view = view_for(IN_WINDOW, 100);
        view.pool_lamports = filters.liquidity.min_pool_lamports - 1;
        assert_eq!(
            filters.route(&view, PRICE),
            Verdict::Dropped(DropReason::PoolTooThin)
        );
    }

    #[test]
    fn find_locates_a_needle_anywhere_or_nowhere() {
        assert_eq!(find(b"abcdef", b"abc"), Some(0));
        assert_eq!(find(b"abcdef", b"def"), Some(3));
        assert_eq!(find(b"abcdef", b"cd"), Some(2));
        assert_eq!(find(b"abcdef", b"xyz"), None);
        assert_eq!(
            find(b"ab", b"abc"),
            None,
            "a needle longer than the haystack"
        );
        assert_eq!(find(b"abc", b""), None, "an empty needle matches nothing");
        assert_eq!(
            find(b"aaab", b"aab"),
            Some(1),
            "a false start does not stop the search"
        );
    }

    // -- the wire format ----------------------------------------------------

    #[test]
    fn a_program_notification_parses() {
        let account = key(1);
        let frame = pump_frame(310_000_001, &account, IN_WINDOW);
        let decoded = decode_frame(frame.as_bytes()).expect("parses");
        assert_eq!(decoded.slot, 310_000_001);
        assert_eq!(decoded.account, Some(account.to_string().as_str()));
        assert_eq!(decoded.owner, PUMP_FUN_PROGRAM);
        assert_eq!(decoded.lamports, 1_461_600);
    }

    #[test]
    fn a_parsed_frame_borrows_the_socket_buffer_rather_than_copying_it() {
        // The whole reason the wire structs are written with `#[serde(borrow)]`
        // and a tuple instead of a `Vec`. If this ever fails, every frame is
        // costing three heap allocations it does not need to.
        let frame = pump_frame(310_000_001, &key(1), IN_WINDOW);
        let bytes = frame.as_bytes();
        let decoded = decode_frame(bytes).expect("parses");

        let buffer = bytes.as_ptr_range();
        for (name, pointer) in [
            ("owner", decoded.owner.as_ptr()),
            ("data", decoded.data_base64.as_ptr()),
            (
                "account",
                decoded
                    .account
                    .expect("a program notification names one")
                    .as_ptr(),
            ),
        ] {
            assert!(
                buffer.contains(&pointer),
                "{name} was copied out of the frame instead of borrowed"
            );
        }
    }

    #[test]
    fn an_account_notification_does_not_say_which_account() {
        let data = b64(&curve_account(IN_WINDOW, false, None));
        let frame = format!(
            concat!(
                r#"{{"jsonrpc":"2.0","method":"accountNotification","params":{{"result":"#,
                r#"{{"context":{{"slot":42}},"value":{{"lamports":1461600,"#,
                r#""data":["{data}","base64"],"owner":"{owner}","executable":false,"#,
                r#""rentEpoch":0,"space":81}}}},"subscription":3}}}}"#
            ),
            data = data,
            owner = PUMP_FUN_PROGRAM,
        );
        let decoded = decode_frame(frame.as_bytes()).expect("parses");
        assert_eq!(decoded.slot, 42);
        assert_eq!(
            decoded.account, None,
            "there is no way to tell which account this was about"
        );
    }

    #[test]
    fn a_notification_that_is_not_json_is_undecodable_rather_than_a_panic() {
        let broken = br#"{"jsonrpc":"2.0","method":"programNotification","params":{"result":{"#;
        assert_eq!(decode_frame(broken), Err(DropReason::Undecodable));
        assert_eq!(
            decode_frame(b"not json at all"),
            Err(DropReason::NotANotification)
        );
    }

    #[test]
    fn only_the_owner_field_counts_as_the_program() {
        // The pre-filter finds the id anywhere in the frame; this is what stops
        // an id that merely appeared in some other field from being treated as
        // the program that owns the account.
        assert_eq!(
            allowed_program_for(PUMP_FUN_PROGRAM),
            Some(ALLOWED_PROGRAMS[0].key)
        );
        assert_eq!(
            allowed_program_for("So11111111111111111111111111111111111111112"),
            None
        );
    }

    // -- the launch index ---------------------------------------------------

    // Two states, as digests. The index never asks what a digest means — only
    // whether two of them are equal — so these are literals rather than
    // fingerprints of a curve, and the tests below read as what they are about.
    const ONE_STATE: u64 = 0x1111_1111_1111_1111;
    const ANOTHER_STATE: u64 = 0x2222_2222_2222_2222;

    #[test]
    fn an_account_ages_in_slots_from_its_first_sighting() {
        let mut index = LaunchIndex::new(8);
        let seen = |index: &mut LaunchIndex, slot| {
            index
                .observe(key(1), slot, FeedProvider::Helius, ONE_STATE)
                .slots_since_launch
        };
        assert_eq!(seen(&mut index, 1_000), 0);
        assert_eq!(seen(&mut index, 1_005), 5);
        assert_eq!(seen(&mut index, 1_050), 50);
    }

    #[test]
    fn the_second_provider_to_report_a_slot_is_not_news() {
        let mut index = LaunchIndex::new(8);
        let mut seen = |slot, provider| index.observe(key(1), slot, provider, ONE_STATE);
        assert!(!seen(1_000, FeedProvider::Helius).stale);
        // Helius and QuickNode both watching pump.fun means the same update
        // arrives twice. The watermark is what stops it being scored twice.
        assert!(seen(1_000, FeedProvider::QuickNode).stale);
        assert!(
            seen(999, FeedProvider::Triton).stale,
            "and a provider running behind is not news either"
        );
        assert!(!seen(1_001, FeedProvider::Helius).stale);
    }

    #[test]
    fn the_launch_index_stays_bounded_and_evicts_the_oldest_first() {
        let mut index = LaunchIndex::new(4);
        for seed in 0..10u8 {
            index.observe(
                key(seed),
                1_000 + seed as u64,
                FeedProvider::Helius,
                ONE_STATE,
            );
        }
        assert_eq!(
            index.len(),
            4,
            "a week of launches uses the same memory as a minute of them"
        );
        // The oldest is gone, so it reads as never seen: age zero, not stale.
        let reobserved = index.observe(key(0), 2_000, FeedProvider::Helius, ONE_STATE);
        assert_eq!(reobserved.slots_since_launch, 0);
        assert!(!reobserved.stale);
    }

    // -- when two providers disagree ----------------------------------------

    #[test]
    fn two_providers_describing_one_slot_differently_is_a_contradiction() {
        let mut index = LaunchIndex::new(8);
        let account = key(1);
        let released = index.observe(account, 1_000, FeedProvider::Helius, ONE_STATE);
        assert!(
            released.conflict.is_none(),
            "there was nothing yet to disagree with"
        );

        let other = index.observe(account, 1_000, FeedProvider::QuickNode, ANOTHER_STATE);
        assert!(
            other.stale,
            "a disagreement does not make the second frame news"
        );
        let conflict = other.conflict.expect("one account, one slot, two states");
        assert_eq!(conflict.account, account);
        assert_eq!(conflict.slot, 1_000);
        assert_eq!(conflict.held_by, FeedProvider::Helius);
        assert_eq!(
            conflict.held, ONE_STATE,
            "the state that was released is the one named as held"
        );
        assert_eq!(conflict.reported_by, FeedProvider::QuickNode);
        assert_eq!(conflict.reported, ANOTHER_STATE);
    }

    #[test]
    fn two_providers_agreeing_about_a_slot_is_a_duplicate_and_nothing_more() {
        let mut index = LaunchIndex::new(8);
        index.observe(key(1), 1_000, FeedProvider::Helius, ONE_STATE);
        let echo = index.observe(key(1), 1_000, FeedProvider::QuickNode, ONE_STATE);
        assert!(echo.stale);
        assert!(
            echo.conflict.is_none(),
            "a second copy of the released write is corroboration"
        );
    }

    #[test]
    fn a_second_write_in_one_slot_is_not_a_second_opinion() {
        // The case that decides whether this counter is worth reading. A curve
        // inside a launch burst is written several times in one slot, every
        // provider delivers every write, and the sockets interleave — so if
        // any two writes were compared, a healthy feed would report a
        // disagreement on every busy slot. Only each provider's first write of
        // the slot is compared, and these four frames are two writes seen twice.
        let mut index = LaunchIndex::new(8);
        let account = key(1);
        let first_write = ONE_STATE;
        let second_write = ANOTHER_STATE;

        index.observe(account, 1_000, FeedProvider::Helius, first_write);
        let same_socket_again = index.observe(account, 1_000, FeedProvider::Helius, second_write);
        assert!(
            same_socket_again.conflict.is_none(),
            "the same socket's next write"
        );

        let other_socket = index.observe(account, 1_000, FeedProvider::QuickNode, first_write);
        assert!(
            other_socket.conflict.is_none(),
            "the other socket's copy of the first write"
        );

        let other_socket_again =
            index.observe(account, 1_000, FeedProvider::QuickNode, second_write);
        assert!(
            other_socket_again.conflict.is_none(),
            "and the other socket's copy of the second write, which is not a disagreement either"
        );
    }

    #[test]
    fn a_provider_running_behind_is_not_disagreeing_with_anybody() {
        let mut index = LaunchIndex::new(8);
        index.observe(key(1), 1_000, FeedProvider::Helius, ONE_STATE);
        let behind = index.observe(key(1), 999, FeedProvider::QuickNode, ANOTHER_STATE);
        assert!(behind.stale);
        assert!(
            behind.conflict.is_none(),
            "an older slot is a moment this account has moved on from, not a contested one"
        );
    }

    #[test]
    fn the_watermark_moving_leaves_the_old_slot_behind() {
        let mut index = LaunchIndex::new(8);
        index.observe(key(1), 1_000, FeedProvider::Helius, ONE_STATE);
        // A newer slot from the other provider: news, and the witness starts
        // again from what it carried.
        let newer = index.observe(key(1), 1_001, FeedProvider::QuickNode, ANOTHER_STATE);
        assert!(!newer.stale);
        assert!(
            newer.conflict.is_none(),
            "a later slot is not a second opinion on an earlier one"
        );

        let echo = index.observe(key(1), 1_001, FeedProvider::Helius, ANOTHER_STATE);
        assert!(
            echo.conflict.is_none(),
            "and the new slot is judged against what it released"
        );
    }

    #[test]
    fn a_rewind_forgets_what_it_witnessed() {
        let mut index = LaunchIndex::new(8);
        index.observe(key(1), 1_000, FeedProvider::Helius, ONE_STATE);
        index.observe(key(1), 1_002, FeedProvider::Helius, ONE_STATE);
        assert_eq!(index.rewind(1_002), 1);

        // The watermark is back at 1_001, a slot nothing was released for. A
        // provider describing it is not contradicting a block that no longer
        // exists.
        let after = index.observe(key(1), 1_001, FeedProvider::QuickNode, ANOTHER_STATE);
        assert!(
            after.stale,
            "the watermark still stands where the rewind left it"
        );
        assert!(after.conflict.is_none());
    }

    #[test]
    fn a_digest_answers_for_every_field_the_engine_reads() {
        let base = curve(IN_WINDOW);
        let mut variants = vec![base];
        for change in 0..7 {
            let mut curve = base;
            match change {
                0 => curve.virtual_token_reserves += 1,
                1 => curve.virtual_sol_reserves += 1,
                2 => curve.real_token_reserves += 1,
                3 => curve.real_sol_reserves += 1,
                4 => curve.token_total_supply += 1,
                5 => curve.complete = !curve.complete,
                _ => curve.creator = key(200),
            }
            variants.push(curve);
        }

        let mut digests: Vec<u64> = variants.iter().map(curve_digest).collect();
        let states = digests.len();
        digests.sort_unstable();
        digests.dedup();
        assert_eq!(
            digests.len(),
            states,
            "two states the engine would act on differently fingerprinted the same"
        );
        assert_eq!(
            curve_digest(&base),
            curve_digest(&curve(IN_WINDOW)),
            "and one state twice is one fingerprint, or nothing above holds"
        );
    }

    // -- the endpoint pool --------------------------------------------------

    fn pool_of(specs: &[(FeedProvider, u16)]) -> EndpointPool {
        EndpointPool::new(
            specs
                .iter()
                .map(|(provider, weight)| {
                    EndpointConfig::new(
                        *provider,
                        format!("wss://{provider}.example/?api-key=secret"),
                        *weight,
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn an_empty_pool_picks_nothing() {
        let pool = EndpointPool::new(Vec::new());
        assert!(pool.is_empty());
        assert_eq!(pool.pick(), None);
        assert_eq!(pool.healthy_count(), 0);
        assert!(pool.status().is_empty());
    }

    #[test]
    fn the_pool_routes_to_the_endpoint_that_is_actually_faster() {
        let pool = pool_of(&[(FeedProvider::Helius, 1), (FeedProvider::QuickNode, 1)]);
        for _ in 0..LATENCY_SAMPLES {
            pool.record_latency(0, 400); // degraded
            pool.record_latency(1, 40); // healthy
        }
        for _ in 0..8 {
            assert_eq!(
                pool.pick(),
                Some(1),
                "a healthy endpoint always beats a degraded one"
            );
        }
    }

    #[test]
    fn health_follows_the_latency_bands() {
        let pool = pool_of(&[(FeedProvider::Helius, 1)]);
        assert_eq!(
            pool.status()[0].health,
            EndpointHealth::Unknown,
            "nothing measured yet"
        );

        for _ in 0..LATENCY_SAMPLES {
            pool.record_latency(0, 100);
        }
        assert_eq!(pool.status()[0].health, EndpointHealth::Healthy);

        for _ in 0..LATENCY_SAMPLES {
            pool.record_latency(0, 450);
        }
        assert_eq!(pool.status()[0].health, EndpointHealth::Degraded);

        for _ in 0..LATENCY_SAMPLES {
            pool.record_latency(0, 900);
        }
        assert_eq!(pool.status()[0].health, EndpointHealth::Unhealthy);
        assert_eq!(pool.healthy_count(), 0);
    }

    #[test]
    fn a_failing_endpoint_backs_off_for_longer_each_time_up_to_a_ceiling() {
        let pool = pool_of(&[(FeedProvider::Helius, 1)]);
        let first = pool.record_failure(0);
        let second = pool.record_failure(0);
        let third = pool.record_failure(0);
        assert_eq!(first, BACKOFF_MIN);
        assert_eq!(second, BACKOFF_MIN * 2);
        assert_eq!(third, BACKOFF_MIN * 4);

        for _ in 0..20 {
            assert!(
                pool.record_failure(0) <= BACKOFF_MAX,
                "backoff never runs away"
            );
        }
        assert_eq!(pool.record_failure(0), BACKOFF_MAX);
        assert_eq!(pool.status()[0].health, EndpointHealth::Unhealthy);
    }

    #[test]
    fn connecting_clears_the_backoff_and_the_failure_count() {
        let pool = pool_of(&[(FeedProvider::Helius, 1)]);
        pool.record_failure(0);
        pool.record_failure(0);
        assert_eq!(pool.status()[0].consecutive_failures, 2);

        pool.record_connected(0);
        let status = &pool.status()[0];
        assert_eq!(status.consecutive_failures, 0);
        assert_eq!(status.backoff_remaining_ms, 0);
        assert!(status.connected);
        assert_eq!(status.connects, 1);
    }

    #[test]
    fn an_endpoint_in_backoff_is_skipped_until_its_backoff_expires() {
        let pool = pool_of(&[(FeedProvider::Helius, 1), (FeedProvider::QuickNode, 1)]);
        let now = Instant::now();
        pool.record_failure_at(0, now);

        assert_eq!(
            pool.pick_at(now),
            Some(1),
            "the other provider carries it meanwhile"
        );
        assert!(pool.status_at(now)[0].backoff_remaining_ms > 0);
        // Once the backoff has run out the endpoint is a candidate again, and
        // the tie between two unmeasured endpoints goes round.
        let later = now + BACKOFF_MIN * 2;
        assert_eq!(pool.status_at(later)[0].backoff_remaining_ms, 0);
        let picks: Vec<Option<usize>> = (0..2).map(|_| pool.pick_at(later)).collect();
        assert!(
            picks.contains(&Some(0)),
            "the recovered endpoint is used again: {picks:?}"
        );
    }

    #[test]
    fn weight_spreads_a_tie_rather_than_bursting_one_endpoint() {
        // Three unmeasured endpoints tie on health and latency, so weight is all
        // that is left to decide — which is how a larger free-tier allowance is
        // given the larger share of the calls.
        let pool = pool_of(&[
            (FeedProvider::Helius, 3),
            (FeedProvider::QuickNode, 1),
            (FeedProvider::Triton, 1),
        ]);
        let now = Instant::now();
        let mut counts = [0usize; 3];
        for _ in 0..50 {
            counts[pool.pick_at(now).expect("a pick")] += 1;
        }
        assert_eq!(counts.iter().sum::<usize>(), 50);
        assert_eq!(counts[0], 30, "three fifths of the picks");
        assert_eq!(counts[1], 10);
        assert_eq!(counts[2], 10);
    }

    #[test]
    fn a_pool_where_everything_is_unhealthy_still_answers() {
        // Refusing to answer would turn a degraded feed into no feed. The caller
        // already knows the health from the snapshot.
        let pool = pool_of(&[(FeedProvider::Helius, 1), (FeedProvider::QuickNode, 1)]);
        for index in 0..2 {
            for _ in 0..FAILURES_TO_UNHEALTHY {
                pool.record_failure(index);
            }
        }
        assert_eq!(pool.healthy_count(), 0);
        assert!(pool.pick().is_some());
    }

    #[test]
    fn an_endpoint_url_is_reduced_to_its_host_before_it_is_ever_logged() {
        // The api key is in the query string for all three providers, so the
        // whole URL is a credential.
        let endpoint = EndpointConfig::new(
            FeedProvider::Helius,
            "wss://mainnet.helius-rpc.com/?api-key=00000000-dead-beef-0000-000000000000",
            1,
        );
        let redacted = endpoint.redacted();
        assert_eq!(redacted, "wss://mainnet.helius-rpc.com/…");
        assert!(
            !redacted.contains("api-key"),
            "{redacted} still carries the credential"
        );
        // And it survives being serialised into telemetry, which is the path
        // that actually leaves the process.
        let status = EndpointPool::new(vec![endpoint]).status();
        assert!(!serde_json::to_string(&status)
            .expect("serialises")
            .contains("api-key"));
    }

    #[test]
    fn the_transport_is_read_off_the_url_scheme() {
        assert_eq!(
            EndpointConfig::new(FeedProvider::Helius, "wss://x.example/", 1).transport,
            FeedTransport::WebSocket
        );
        assert_eq!(
            EndpointConfig::new(FeedProvider::Triton, "https://x.example:443", 1).transport,
            FeedTransport::Grpc
        );
    }

    // -- subscriptions ------------------------------------------------------

    #[test]
    fn every_subscription_names_one_allowlisted_program_and_nothing_broader() {
        let requests = subscription_requests();
        // Four programs with no size filter, plus pump.fun's two layouts.
        assert_eq!(requests.len(), 6);

        for request in &requests {
            let parsed: serde_json::Value = serde_json::from_str(request).expect("valid json");
            assert_eq!(
                parsed["method"], "programSubscribe",
                "no broad subscription is ever opened"
            );
            let program = parsed["params"][0].as_str().expect("a program");
            assert!(
                ALLOWED_PROGRAMS.iter().any(|p| p.text == program),
                "{program} is not on the allowlist"
            );
            assert_eq!(parsed["params"][1]["encoding"], "base64");
            assert_eq!(parsed["params"][1]["commitment"], COMMITMENT);
        }
    }

    #[test]
    fn pump_fun_is_subscribed_to_by_account_size_so_the_provider_filters_first() {
        let sizes: Vec<u64> = subscription_requests()
            .iter()
            .filter_map(|request| {
                let parsed: serde_json::Value = serde_json::from_str(request).ok()?;
                if parsed["params"][0] != PUMP_FUN_PROGRAM {
                    return None;
                }
                parsed["params"][1]["filters"][0]["dataSize"].as_u64()
            })
            .collect();
        assert_eq!(
            sizes,
            vec![CURVE_MIN_LEN as u64, CURVE_WITH_CREATOR_LEN as u64]
        );
    }

    #[test]
    fn subscription_ids_are_all_different() {
        let ids: Vec<u64> = subscription_requests()
            .iter()
            .map(|r| {
                serde_json::from_str::<serde_json::Value>(r).expect("valid json")["id"]
                    .as_u64()
                    .expect("an id")
            })
            .collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            ids.len(),
            "a repeated id would collide two subscriptions"
        );
    }

    // -- mock streams -------------------------------------------------------

    /// One scripted thing a mock socket says.
    #[derive(Debug, Clone)]
    enum Scripted {
        Frame(String),
        Pong,
        /// The far end goes away, which is what makes the manager reconnect.
        End,
    }

    /// A dialer that hands out scripted sockets instead of opening any.
    ///
    /// One script per `dial`, in order, so a test can say "this connection sends
    /// these frames and then dies, and the next one sends these". Once the
    /// scripts run out, dialling fails — which is what stops a test from
    /// spinning forever on a manager that reconnects on principle.
    struct MockDialer {
        scripts: Mutex<VecDeque<Vec<Scripted>>>,
        dials: AtomicU64,
        subscriptions: Arc<Mutex<Vec<String>>>,
    }

    impl MockDialer {
        fn new(scripts: Vec<Vec<Scripted>>) -> Self {
            Self {
                scripts: Mutex::new(scripts.into_iter().collect()),
                dials: AtomicU64::new(0),
                subscriptions: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn dial_count(&self) -> u64 {
            self.dials.load(Ordering::SeqCst)
        }
    }

    impl FeedDialer for MockDialer {
        fn dial(
            &self,
            _endpoint: EndpointConfig,
        ) -> BoxFuture<'static, Result<FeedPair, IngestError>> {
            self.dials.fetch_add(1, Ordering::SeqCst);
            let script = self.scripts.lock().pop_front();
            let subscriptions = Arc::clone(&self.subscriptions);
            Box::pin(async move {
                let script =
                    script.ok_or_else(|| IngestError::Dial("the script ran out".into()))?;
                Ok((
                    Box::new(MockSink(subscriptions)) as Box<dyn FeedSink>,
                    Box::new(MockSource(script.into())) as Box<dyn FeedStream>,
                ))
            })
        }
    }

    struct MockSink(Arc<Mutex<Vec<String>>>);

    impl FeedSink for MockSink {
        fn send_text(&mut self, text: String) -> BoxFuture<'_, Result<(), IngestError>> {
            self.0.lock().push(text);
            Box::pin(async { Ok(()) })
        }

        fn ping(&mut self) -> BoxFuture<'_, Result<(), IngestError>> {
            Box::pin(async { Ok(()) })
        }
    }

    struct MockSource(VecDeque<Scripted>);

    impl FeedStream for MockSource {
        fn recv(&mut self) -> BoxFuture<'_, Option<Result<FeedMessage, IngestError>>> {
            let next = self.0.pop_front();
            Box::pin(async move {
                match next {
                    Some(Scripted::Frame(text)) => Some(Ok(FeedMessage::Frame(Bytes::from(text)))),
                    Some(Scripted::Pong) => Some(Ok(FeedMessage::Pong)),
                    Some(Scripted::End) => None,
                    // The script is finished and the socket has not been told to
                    // die, so it parks — which is what a quiet socket does, and
                    // what stops the read loop spinning.
                    None => std::future::pending().await,
                }
            })
        }
    }

    fn config_for(endpoints: usize) -> IngestionConfig {
        IngestionConfig {
            endpoints: FeedProvider::ALL
                .iter()
                .take(endpoints)
                .map(|&provider| {
                    EndpointConfig::new(provider, format!("wss://{provider}.test/?api-key=x"), 1)
                })
                .collect(),
            filters: StreamFilters::DEFAULT,
            price: PRICE,
            // Long enough that no test races the telemetry task.
            telemetry_interval: Duration::from_secs(3_600),
        }
    }

    /// Polls until the condition holds or the deadline passes. Returns whether
    /// it held — every caller asserts on that, so a timeout fails the test with
    /// its own message rather than with a bare `false`.
    async fn until(millis: u64, mut condition: impl FnMut() -> bool) -> bool {
        for _ in 0..(millis / 5).max(1) {
            if condition() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        condition()
    }

    /// The two frames it takes for one account to be old enough to route: one to
    /// register it with the launch index, one past the lottery window.
    fn aged_pair(account: &Pubkey, state: (u64, u64, u64)) -> Vec<Scripted> {
        vec![
            Scripted::Frame(pump_frame(1_000, account, state)),
            Scripted::Frame(pump_frame(1_020, account, state)),
        ]
    }

    #[tokio::test]
    async fn a_mock_stream_routes_a_target_window_candidate_to_the_fast_path() {
        let account = key(1);
        let dialer = Arc::new(MockDialer::new(vec![aged_pair(&account, IN_WINDOW)]));
        let (manager, mut streams) = IngestionManager::start(config_for(1), dialer, None, None);

        let event = tokio::time::timeout(Duration::from_secs(2), streams.fast_path.recv())
            .await
            .expect("a candidate arrives")
            .expect("the channel is open");

        assert_eq!(event.route, Route::FastPath);
        assert_eq!(event.view.account, account);
        assert_eq!(event.view.provider, FeedProvider::Helius);
        assert_eq!(event.view.slot, 1_020);
        assert_eq!(event.view.slots_since_launch, 20);
        assert_eq!(event.market_cap_usd_cents, 4_000_431, "$40,004.31");
        assert_eq!(event.view.creator, key(9));

        let snapshot = manager.snapshot();
        assert_eq!(snapshot.frames, 2);
        assert_eq!(snapshot.candidates, 1);
        assert_eq!(snapshot.fast_path, 1);
        assert_eq!(snapshot.dropped_fast_path, 0);
        assert_eq!(
            snapshot.filtered, 1,
            "the first frame was inside the lottery window"
        );
        manager.stop();
    }

    #[tokio::test]
    async fn spam_is_counted_and_never_reaches_a_channel() {
        let account = key(2);
        let dialer = Arc::new(MockDialer::new(vec![aged_pair(&account, FRESH_LAUNCH)]));
        let (manager, mut streams) = IngestionManager::start(config_for(1), dialer, None, None);

        assert!(
            until(1_000, || manager.snapshot().frames == 2).await,
            "both frames arrive"
        );
        let snapshot = manager.snapshot();
        assert_eq!(snapshot.candidates, 0, "a $4k launch is not a candidate");
        assert_eq!(
            snapshot.filtered, 2,
            "one for the lottery window, one for the floor"
        );
        assert_eq!(
            snapshot.prefiltered, 0,
            "it is a pump.fun frame, so it was parsed"
        );
        assert_eq!(streams.fast_path.try_recv().ok(), None);
        assert_eq!(streams.standard.try_recv().ok(), None);
        manager.stop();
    }

    #[tokio::test]
    async fn a_frame_from_a_program_nobody_asked_about_is_dropped_before_it_is_parsed() {
        let frame = program_frame(
            1_000,
            &key(3),
            "So11111111111111111111111111111111111111112",
            &curve_account(IN_WINDOW, false, None),
        );
        let dialer = Arc::new(MockDialer::new(vec![vec![Scripted::Frame(frame)]]));
        let (manager, _streams) = IngestionManager::start(config_for(1), dialer, None, None);

        assert!(
            until(1_000, || manager.snapshot().frames == 1).await,
            "the frame arrives"
        );
        let snapshot = manager.snapshot();
        assert_eq!(snapshot.prefiltered, 1);
        assert_eq!(
            snapshot.parse_failures, 0,
            "it never got as far as being parsed"
        );
        assert_eq!(snapshot.candidates, 0);
        manager.stop();
    }

    #[tokio::test]
    async fn a_raydium_frame_is_allowlisted_and_counted_but_has_no_decoder_yet() {
        // The seam this build stops at. Raydium pools are subscribed to and the
        // frames are counted; their account layouts are a later phase. Counting
        // them as `NoDecoder` is what keeps that a visible gap rather than a
        // feed that merely looks quiet.
        let frame = program_frame(
            1_000,
            &key(10),
            RAYDIUM_AMM_V4_PROGRAM,
            b"not a bonding curve",
        );
        let dialer = Arc::new(MockDialer::new(vec![vec![Scripted::Frame(frame)]]));
        let (manager, _streams) = IngestionManager::start(config_for(1), dialer, None, None);

        assert!(until(1_000, || manager.snapshot().frames == 1).await);
        let snapshot = manager.snapshot();
        assert_eq!(
            snapshot.prefiltered, 0,
            "it named an allowlisted program, so it was parsed"
        );
        assert_eq!(snapshot.filtered, 1);
        assert_eq!(snapshot.candidates, 0);
        manager.stop();
    }

    #[tokio::test]
    async fn two_providers_watching_the_same_program_produce_one_candidate() {
        let account = key(4);
        let dialer = Arc::new(MockDialer::new(vec![
            aged_pair(&account, IN_WINDOW),
            aged_pair(&account, IN_WINDOW),
        ]));
        let (manager, mut streams) = IngestionManager::start(config_for(2), dialer, None, None);

        assert!(
            until(2_000, || manager.snapshot().frames == 4).await,
            "all four frames arrive"
        );
        assert!(
            until(500, || manager.snapshot().candidates == 1).await,
            "and settle"
        );

        let snapshot = manager.snapshot();
        assert_eq!(snapshot.candidates, 1, "the same update twice is news once");
        assert_eq!(snapshot.stale, 2, "the second provider's copy of each slot");
        assert_eq!(snapshot.filtered, 1, "the lottery-window frame");

        let event = streams.fast_path.try_recv().expect("the one candidate");
        assert_eq!(event.view.slot, 1_020);
        assert!(streams.fast_path.try_recv().is_err(), "and only the one");
        manager.stop();
    }

    /// A telemetry destination that keeps everything, so a test can read what
    /// the engine said rather than only what it counted.
    #[derive(Default)]
    struct Captured {
        events: Mutex<Vec<crate::telemetry::TelemetryEvent>>,
    }

    impl crate::telemetry::TelemetrySink for Captured {
        fn deliver(&self, event: &crate::telemetry::TelemetryEvent) {
            self.events.lock().push(event.clone());
        }
    }

    /// How many lines the hub has carried about providers disagreeing.
    fn disagreements(captured: &Captured) -> usize {
        captured
            .events
            .lock()
            .iter()
            .filter(|event| event.message.contains("disagree"))
            .count()
    }

    #[tokio::test]
    async fn a_provider_that_disagrees_does_not_replace_what_was_released() {
        // Through `admit_curve` rather than through two sockets, because the
        // claim is about which of two states survives and two socket tasks
        // would race to decide which one arrived first. The path is the same
        // one either way: the same launch index, the same watermark, the same
        // channels.
        let (manager, mut streams) =
            IngestionManager::start(config_for(0), Arc::new(MockDialer::new(vec![])), None, None);
        let account = key(21);
        let released = curve(IN_WINDOW);
        let disagreement = curve(IN_WINDOW_MOVED);
        let at = Instant::now();

        // One write to register the account, one past the lottery window, and
        // then the other provider's account of that same slot.
        manager.admit_curve(FeedProvider::Helius, 1_000, account, &released, at);
        assert_eq!(
            manager.admit_curve(FeedProvider::Helius, 1_020, account, &released, at),
            Verdict::Routed(Route::FastPath)
        );
        assert_eq!(
            manager.admit_curve(FeedProvider::QuickNode, 1_020, account, &disagreement, at),
            Verdict::Dropped(DropReason::StaleSlot),
            "a disagreement is still a duplicate by the watermark's reckoning"
        );

        let snapshot = manager.snapshot();
        assert_eq!(snapshot.contradictions, 1);
        assert_eq!(
            snapshot.candidates, 1,
            "and it did not produce a second candidate"
        );

        let event = streams.fast_path.try_recv().expect("the one candidate");
        assert_eq!(
            event.view.pool_lamports, released.real_sol_reserves,
            "what was released stayed released"
        );
        assert!(
            streams.fast_path.try_recv().is_err(),
            "and nothing followed it"
        );

        let last = snapshot
            .last_contradiction
            .expect("the disagreement reached the snapshot");
        assert_eq!(last.account, account);
        assert_eq!(last.slot, 1_020);
        assert_eq!(last.held_by, FeedProvider::Helius);
        assert_eq!(last.reported_by, FeedProvider::QuickNode);
        manager.stop();
    }

    #[tokio::test]
    async fn a_disagreement_is_published_once_rather_than_on_every_tick() {
        let hub = Arc::new(TelemetryHub::start());
        let captured = Arc::new(Captured::default());
        hub.observe(Arc::clone(&captured) as Arc<dyn crate::telemetry::TelemetrySink>);

        let mut config = config_for(0);
        config.telemetry_interval = Duration::from_millis(20);
        let (manager, _streams) = IngestionManager::start(
            config,
            Arc::new(MockDialer::new(vec![])),
            None,
            Some(Arc::clone(&hub)),
        );

        let account = key(22);
        let at = Instant::now();
        manager.admit_curve(FeedProvider::Helius, 1_000, account, &curve(IN_WINDOW), at);
        manager.admit_curve(FeedProvider::Helius, 1_020, account, &curve(IN_WINDOW), at);
        manager.admit_curve(
            FeedProvider::QuickNode,
            1_020,
            account,
            &curve(IN_WINDOW_MOVED),
            at,
        );

        assert!(
            until(2_000, || disagreements(&captured) == 1).await,
            "the disagreement never reached telemetry"
        );
        // Ten more ticks with nothing new to say.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            disagreements(&captured),
            1,
            "one divergence was repeated on every tick after it"
        );

        let events = captured.events.lock();
        let line = events
            .iter()
            .find(|event| event.message.contains("disagree"))
            .expect("the line just counted");
        assert_eq!(line.level, TelemetryLevel::Warn);
        assert_eq!(line.data["since"], 1);
        assert_eq!(line.data["total"], 1);
        assert_eq!(line.data["last"]["account"], account.to_string());
        assert_eq!(line.data["last"]["slot"], 1_020);
        assert_eq!(line.data["last"]["heldBy"], "helius");
        // Camel-cased by serde, which is not what `FeedProvider::as_str` says.
        // Both spellings are already in the stream — `endpoints[].provider`
        // has been the serde one since this struct existed — so this asserts
        // what the reader receives rather than what looks tidier here.
        assert_eq!(line.data["last"]["reportedBy"], "quickNode");
        assert!(
            line.data["last"]["held"].is_string(),
            "a digest past 2^53 does not survive a JSON reader that parses numbers as doubles"
        );
        drop(events);

        manager.stop();
        hub.shutdown();
    }

    #[tokio::test]
    async fn the_manager_dials_again_after_a_stream_ends() {
        let account = key(5);
        let mut first = aged_pair(&account, IN_WINDOW);
        first.push(Scripted::End);
        let dialer = Arc::new(MockDialer::new(vec![first, aged_pair(&key(6), IN_WINDOW)]));
        let (manager, _streams) = IngestionManager::start(
            config_for(1),
            Arc::clone(&dialer) as Arc<dyn FeedDialer>,
            None,
            None,
        );

        // The first backoff is `BACKOFF_MIN`, so this waits out one of them.
        assert!(
            until(3_000, || dialer.dial_count() >= 2).await,
            "a stream that ends is dialled again, not abandoned"
        );
        assert!(until(1_000, || manager.snapshot().disconnects >= 1).await);
        assert!(manager.snapshot().connects >= 2);
        manager.stop();
    }

    #[tokio::test]
    async fn the_subscriptions_go_out_the_moment_the_socket_opens() {
        let dialer = Arc::new(MockDialer::new(vec![vec![Scripted::Pong]]));
        let subscriptions = Arc::clone(&dialer.subscriptions);
        let (manager, _streams) = IngestionManager::start(config_for(1), dialer, None, None);

        assert!(
            until(1_000, || subscriptions.lock().len() == 6).await,
            "one per subscription"
        );
        for request in subscriptions.lock().iter() {
            assert!(
                request.contains("programSubscribe"),
                "{request} is not scoped to a program"
            );
        }
        manager.stop();
    }

    #[tokio::test]
    async fn a_full_fast_path_channel_costs_candidates_rather_than_frames() {
        // Nothing is reading `streams`, which is what a stalled scoring engine
        // looks like from here. The feed must keep reading regardless.
        let account = key(7);
        let mut script = vec![Scripted::Frame(pump_frame(1_000, &account, IN_WINDOW))];
        let overflow = FAST_PATH_DEPTH + 50;
        for step in 0..overflow {
            script.push(Scripted::Frame(pump_frame(
                1_020 + step as u64,
                &account,
                IN_WINDOW,
            )));
        }
        let dialer = Arc::new(MockDialer::new(vec![script]));
        let (manager, _streams) = IngestionManager::start(config_for(1), dialer, None, None);

        assert!(
            until(5_000, || manager.snapshot().frames == overflow as u64 + 1).await,
            "every frame is still read off the socket"
        );
        let snapshot = manager.snapshot();
        assert_eq!(snapshot.fast_path, overflow as u64, "all of them routed");
        assert!(
            snapshot.dropped_fast_path > 0,
            "and the ones that did not fit were counted"
        );
        assert_eq!(
            snapshot.fast_path - snapshot.dropped_fast_path,
            FAST_PATH_DEPTH as u64,
            "exactly a queue's worth got through"
        );
        manager.stop();
    }

    #[tokio::test]
    async fn stopping_the_manager_stops_the_sockets() {
        let dialer = Arc::new(MockDialer::new(vec![vec![Scripted::End]]));
        let (manager, _streams) = IngestionManager::start(
            config_for(1),
            Arc::clone(&dialer) as Arc<dyn FeedDialer>,
            None,
            None,
        );

        assert!(until(1_000, || dialer.dial_count() >= 1).await);
        manager.stop();
        let dials_at_stop = dialer.dial_count();
        // Longer than the reconnect backoff, so a task that was still running
        // would have dialled again by now and this would fail.
        tokio::time::sleep(BACKOFF_MIN + Duration::from_millis(200)).await;
        assert_eq!(
            dialer.dial_count(),
            dials_at_stop,
            "nothing dials after stop"
        );
        manager.stop();
        // Twice is not an error: the window and the runtime both ask.
    }

    // -- metrics ------------------------------------------------------------

    #[test]
    fn a_dispatch_past_its_budget_is_counted_and_one_inside_it_is_not() {
        // The budget itself is a target measured in production, not something a
        // test on an unloaded machine can prove. What is worth pinning down is
        // that the accounting is right, so the number reported in a soak means
        // what it says.
        let metrics = IngestionMetrics::default();
        metrics.observe_dispatch(Duration::from_micros(150));
        metrics.observe_dispatch(DISPATCH_BUDGET);
        assert_eq!(
            metrics.over_budget.load(Ordering::Relaxed),
            0,
            "the budget itself is not over it"
        );

        metrics.observe_dispatch(DISPATCH_BUDGET + Duration::from_micros(1));
        assert_eq!(metrics.over_budget.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.dispatches.load(Ordering::Relaxed), 3);
        assert_eq!(
            metrics.dispatch_max_us.load(Ordering::Relaxed),
            DISPATCH_BUDGET.as_micros() as u64 + 1
        );
    }

    #[test]
    fn every_drop_lands_in_exactly_one_counter() {
        let reasons = [
            DropReason::NotAllowlisted,
            DropReason::NotANotification,
            DropReason::Undecodable,
            DropReason::NoDecoder,
            DropReason::TooSmall,
            DropReason::LotterySlot,
            DropReason::StaleSlot,
            DropReason::PoolTooThin,
            DropReason::Graduated,
        ];
        let metrics = IngestionMetrics::default();
        for reason in reasons {
            metrics.count_drop(reason);
        }
        let counted = metrics.prefiltered.load(Ordering::Relaxed)
            + metrics.parse_failures.load(Ordering::Relaxed)
            + metrics.stale.load(Ordering::Relaxed)
            + metrics.filtered.load(Ordering::Relaxed);
        assert_eq!(
            counted,
            reasons.len() as u64,
            "a drop nobody counts is a silent one"
        );

        // And every reason has a distinct name, because the names are what end
        // up in an incident log.
        let mut names: Vec<&str> = reasons.iter().map(|r| r.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), reasons.len());
    }

    #[test]
    fn reading_the_counters_does_not_move_the_rate_window() {
        let pool = pool_of(&[(FeedProvider::Helius, 1)]);
        let metrics = IngestionMetrics::default();
        metrics.observe_frame(100);

        let first = metrics.snapshot(&pool, 0);
        let second = metrics.snapshot(&pool, 0);
        assert_eq!(first.frames, second.frames);
        assert_eq!(
            first.frames_per_sec, 0.0,
            "the first window has nothing behind it"
        );

        metrics.roll_window(first.at_ms - 1_000, first.frames, first.candidates);
        metrics.observe_frame(100);
        metrics.observe_frame(100);
        let rolled = metrics.snapshot(&pool, 0);
        assert!(
            rolled.frames_per_sec > 0.0,
            "two frames in about a second is a rate"
        );
        assert_eq!(rolled.frames, 3);
    }

    #[test]
    fn a_snapshot_carries_the_budget_it_was_measured_against() {
        let pool = pool_of(&[(FeedProvider::Helius, 1)]);
        let snapshot = IngestionMetrics::default().snapshot(&pool, 7);
        assert_eq!(snapshot.budget_us, DISPATCH_BUDGET.as_micros() as u64);
        assert_eq!(snapshot.tracked_accounts, 7);
        assert_eq!(snapshot.endpoints.len(), 1);
        // It has to survive the trip to the window, since that is the only place
        // anybody reads it.
        serde_json::to_string(&snapshot).expect("a snapshot reaches the UI as JSON");
    }

    // -- the manager's own controls -----------------------------------------

    #[tokio::test]
    async fn the_sol_price_can_be_changed_without_restarting_the_sockets() {
        let dialer = Arc::new(MockDialer::new(vec![vec![Scripted::Pong]]));
        let (manager, _streams) = IngestionManager::start(config_for(1), dialer, None, None);
        assert_eq!(manager.sol_price(), PRICE);
        manager.set_sol_price(SolPrice::from_usd_cents(20_000));
        assert_eq!(manager.sol_price(), SolPrice::from_usd_cents(20_000));
        assert_eq!(manager.filters().target_window, TargetWindow::DEFAULT);
        manager.stop();
    }

    #[tokio::test]
    async fn a_manager_with_no_configured_endpoints_dials_nothing() {
        // The state of a checkout that has never been given credentials, and the
        // reason starting the manager at boot is safe.
        let dialer = Arc::new(MockDialer::new(Vec::new()));
        let (manager, _streams) = IngestionManager::start(
            IngestionConfig::default(),
            Arc::clone(&dialer) as Arc<dyn FeedDialer>,
            None,
            None,
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(dialer.dial_count(), 0);
        assert_eq!(manager.endpoint_count(), 0);
        assert_eq!(manager.pick_endpoint(), None);
        assert_eq!(manager.snapshot().frames, 0);
        manager.stop();
    }

    #[tokio::test]
    async fn a_one_shot_request_is_routed_to_a_configured_endpoint() {
        let dialer = Arc::new(MockDialer::new(vec![
            vec![Scripted::Pong],
            vec![Scripted::Pong],
        ]));
        let (manager, _streams) = IngestionManager::start(config_for(2), dialer, None, None);
        let picked = manager.pick_endpoint().expect("something to route to");
        assert!(FeedProvider::ALL.contains(&picked.provider));
        assert_eq!(manager.endpoint_count(), 2);
        manager.stop();
    }

    // -- sqlite -------------------------------------------------------------

    fn temp_db(name: &str) -> std::path::PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "sts-ingest-{name}-{}-{}.db",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn remove_db(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    fn sample_row() -> IngestCandidateRow {
        IngestCandidateRow {
            source: FeedProvider::Helius.as_str().to_string(),
            slot: 1_020,
            account: key(1).to_string(),
            program: ALLOWED_PROGRAMS[0].key.to_string(),
            creator: Some(key(9).to_string()),
            route: Route::FastPath.as_str().to_string(),
            market_cap_usd_cents: 4_000_431,
            pool_lamports: 62_650_000_000,
            curve_progress_bps: 7_370,
            observed_at_ms: 1_700_000_000_000,
            dispatch_latency_us: 412,
        }
    }

    #[test]
    fn the_same_candidate_twice_is_one_row_and_two_providers_are_two() {
        let path = temp_db("dedupe");
        let database = Database::open(&path).expect("opens");
        database.ensure_ingest_schema().expect("creates the table");
        // Idempotent, because the WAL worker runs it on every start.
        database.ensure_ingest_schema().expect("and again");

        let row = sample_row();
        assert_eq!(
            database
                .record_ingest_candidates(std::slice::from_ref(&row))
                .expect("writes"),
            1
        );
        assert_eq!(
            database
                .record_ingest_candidates(std::slice::from_ref(&row))
                .expect("writes"),
            0,
            "replaying a fixture does not double the history"
        );

        let mut second_provider = row.clone();
        second_provider.source = FeedProvider::QuickNode.as_str().to_string();
        assert_eq!(
            database
                .record_ingest_candidates(&[second_provider])
                .expect("writes"),
            1,
            "the provider is part of the identity, so agreement is visible"
        );

        let mut later_slot = row;
        later_slot.slot = 1_021;
        assert_eq!(
            database
                .record_ingest_candidates(&[later_slot])
                .expect("writes"),
            1
        );

        assert_eq!(database.ingest_candidate_count().expect("counts"), 3);
        assert_eq!(database.record_ingest_candidates(&[]).expect("writes"), 0);
        database.close();
        remove_db(&path);
    }

    #[tokio::test]
    async fn a_candidate_reaches_sqlite_by_way_of_the_wal_worker() {
        let path = temp_db("wal");
        let database = Arc::new(Database::open(&path).expect("opens"));
        let account = key(11);

        let dialer = Arc::new(MockDialer::new(vec![aged_pair(&account, IN_WINDOW)]));
        let (manager, _streams) =
            IngestionManager::start(config_for(1), dialer, Some(Arc::clone(&database)), None);
        assert!(
            until(3_000, || manager.snapshot().candidates == 1).await,
            "one candidate routes"
        );
        // `stop` joins the writer, so the row is on disk by the time it returns.
        manager.stop();

        assert_eq!(manager.snapshot().wal_rows, 1);
        assert_eq!(manager.snapshot().wal_failures, 0);
        assert_eq!(database.ingest_candidate_count().expect("counts"), 1);

        // The same frames again, through a fresh manager with a fresh launch
        // index — which is what a replay of the same fixture looks like.
        let dialer = Arc::new(MockDialer::new(vec![aged_pair(&account, IN_WINDOW)]));
        let (replay, _streams) =
            IngestionManager::start(config_for(1), dialer, Some(Arc::clone(&database)), None);
        assert!(until(3_000, || replay.snapshot().candidates == 1).await);
        replay.stop();
        assert_eq!(
            database.ingest_candidate_count().expect("counts"),
            1,
            "one canonical row, however many times it is replayed"
        );

        database.close();
        remove_db(&path);
    }

    #[tokio::test]
    async fn ingestion_publishes_its_counters_to_telemetry() {
        let path = temp_db("telemetry");
        let database = Arc::new(Database::open(&path).expect("opens"));
        let hub = Arc::new(TelemetryHub::start());
        let published_before = hub.snapshot().published;

        let mut config = config_for(1);
        config.telemetry_interval = Duration::from_millis(20);
        let dialer = Arc::new(MockDialer::new(vec![aged_pair(&key(12), IN_WINDOW)]));
        let (manager, _streams) = IngestionManager::start(
            config,
            dialer,
            Some(Arc::clone(&database)),
            Some(Arc::clone(&hub)),
        );

        assert!(
            until(2_000, || hub.snapshot().published > published_before + 2).await,
            "the start line, the connect line, and at least one metrics line"
        );
        manager.stop();
        hub.shutdown();
        database.close();
        remove_db(&path);
    }
}
