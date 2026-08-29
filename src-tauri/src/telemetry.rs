//! One fan-out point between the engine and every window watching it.
//!
//! Producers call `publish` from whatever thread they are on. A single pump
//! thread owns the receiving end and copies each event to every subscribed
//! window. The queue is bounded and `publish` never blocks: if the UI falls
//! behind, the newest event is dropped and counted rather than allowed to stall
//! the engine. A dropped frame is cheap; a stalled engine is not.
//!
//! Subscriptions clean themselves up. A `Channel` whose window has closed fails
//! on send, and the failing id is removed on the spot, so a window that is shut
//! and reopened does not leave a dead subscriber behind forever.
//!
//! A window is not the only thing that can be listening. `TelemetrySink` is the
//! same fan-out for a destination that has no IPC channel behind it — the
//! headless daemon writing the stream to a file is the one this build has — and
//! it exists because the alternative was for `daemon.rs` to reach past the hub
//! and read the engine's internals directly. Sinks and subscribers share one id
//! space and one pump; the only difference is that a sink cannot fail in a way
//! that means "the window went away", so a sink is removed when it is
//! unregistered and never on its own.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{SystemTime, UNIX_EPOCH};

use crossbeam_channel::{bounded, select, Receiver, Sender};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use tauri::ipc::Channel;

/// How many events may be in flight before the oldest producer starts losing
/// the newest ones. Sized for a busy launch minute, not for buffering forever.
const QUEUE_DEPTH: usize = 1024;

/// Epoch milliseconds, the clock every other table in `sts.db` is stamped with.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// How loud an event is. Mirrors the `level` field in the audit NDJSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TelemetryLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// One line of engine telemetry on its way to the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryEvent {
    /// Monotonic per-process counter. A gap in this is a dropped event, which is
    /// how the UI can tell "quiet" from "behind".
    pub seq: u64,
    pub at_ms: i64,
    pub level: TelemetryLevel,
    /// Which part of the engine spoke — `lifecycle`, `db`, `kill_switch`.
    pub source: String,
    pub message: String,
    pub data: serde_json::Value,
}

/// What `get_engine_status` reports about telemetry itself.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetrySnapshot {
    pub subscribers: usize,
    /// Listeners that are not windows: the headless daemon's exporter, and
    /// anything else registered through `observe`. Counted apart from
    /// `subscribers` because "a window is watching" and "a file is being
    /// written" are different answers to "is anybody reading this".
    pub sinks: usize,
    pub published: u64,
    /// Events thrown away because the queue was full. Non-zero means the UI is
    /// slower than the engine.
    pub dropped: u64,
    pub queue_depth: usize,
    pub running: bool,
}

/// What `stream_telemetry` hands back so the UI knows the stream is live.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetrySubscription {
    pub subscriber_id: u64,
    /// The sequence number the next delivered event will carry. Anything earlier
    /// happened before this window was listening.
    pub from_seq: u64,
}

/// A telemetry destination that is not a window.
///
/// The hub's other fan-out goes down a `tauri::ipc::Channel`, which needs an
/// application and a window on the other end of it. `sts daemon` has neither
/// and still has to be able to say what the engine did, so this is the seam:
/// implementors receive every event the pump delivers, on the pump thread, in
/// sequence order.
///
/// **`deliver` runs on the only pump there is.** An implementation that blocks
/// in it — a network write, a lock somebody else holds — stalls every other
/// listener behind it and eventually fills the queue, at which point the engine
/// starts dropping events rather than waiting, exactly as it does for a slow
/// window. Buffer, or drop, but do not wait.
pub trait TelemetrySink: Send + Sync {
    fn deliver(&self, event: &TelemetryEvent);
}

/// The fan-out itself.
pub struct TelemetryHub {
    tx: Sender<TelemetryEvent>,
    shutdown_tx: Sender<()>,
    subscribers: Arc<RwLock<HashMap<u64, Channel<TelemetryEvent>>>>,
    sinks: Arc<RwLock<HashMap<u64, Arc<dyn TelemetrySink>>>>,
    seq: AtomicU64,
    dropped: Arc<AtomicU64>,
    /// One counter for both maps, so an id names exactly one listener whichever
    /// kind it is.
    next_subscriber: AtomicU64,
    pump: Mutex<Option<JoinHandle<()>>>,
}

impl TelemetryHub {
    /// Starts the pump thread and returns the hub feeding it.
    pub fn start() -> Self {
        let (tx, rx) = bounded::<TelemetryEvent>(QUEUE_DEPTH);
        let (shutdown_tx, shutdown_rx) = bounded::<()>(1);
        let subscribers: Arc<RwLock<HashMap<u64, Channel<TelemetryEvent>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let sinks: Arc<RwLock<HashMap<u64, Arc<dyn TelemetrySink>>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let pump = std::thread::Builder::new()
            .name("sts-telemetry".to_string())
            .spawn({
                let subscribers = Arc::clone(&subscribers);
                let sinks = Arc::clone(&sinks);
                move || pump_loop(rx, shutdown_rx, subscribers, sinks)
            })
            .expect("the telemetry pump is the only path from the engine to the UI");

        Self {
            tx,
            shutdown_tx,
            subscribers,
            sinks,
            seq: AtomicU64::new(0),
            dropped: Arc::new(AtomicU64::new(0)),
            next_subscriber: AtomicU64::new(1),
            pump: Mutex::new(Some(pump)),
        }
    }

    /// Queues one event. Never blocks, never fails, may drop.
    pub fn publish(
        &self,
        level: TelemetryLevel,
        source: &str,
        message: impl Into<String>,
        data: serde_json::Value,
    ) {
        let event = TelemetryEvent {
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            at_ms: now_ms(),
            level,
            source: source.to_string(),
            message: message.into(),
            data,
        };

        if self.tx.try_send(event).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Registers a window's channel and returns the handle describing it.
    pub fn subscribe(&self, channel: Channel<TelemetryEvent>) -> TelemetrySubscription {
        let subscriber_id = self.next_subscriber.fetch_add(1, Ordering::Relaxed);
        self.subscribers.write().insert(subscriber_id, channel);
        TelemetrySubscription {
            subscriber_id,
            from_seq: self.seq.load(Ordering::Relaxed),
        }
    }

    /// Registers a listener that is not a window, and returns its id.
    ///
    /// The events it receives are the ones published from here on. There is no
    /// backfill, for the same reason `subscribe` does not offer one: the hub
    /// keeps a queue on its way to the listeners and not a history, and a sink
    /// that needs everything from the start of the run has to be registered
    /// before the run starts.
    pub fn observe(&self, sink: Arc<dyn TelemetrySink>) -> u64 {
        let id = self.next_subscriber.fetch_add(1, Ordering::Relaxed);
        self.sinks.write().insert(id, sink);
        id
    }

    /// Removes one sink. Unknown ids are not an error — a caller tearing down
    /// twice is tearing down.
    pub fn unobserve(&self, id: u64) {
        self.sinks.write().remove(&id);
    }

    /// Counters for `get_engine_status`.
    pub fn snapshot(&self) -> TelemetrySnapshot {
        TelemetrySnapshot {
            subscribers: self.subscribers.read().len(),
            sinks: self.sinks.read().len(),
            published: self.seq.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            queue_depth: self.tx.len(),
            running: self.pump.lock().is_some(),
        }
    }

    /// Stops the pump and waits for it, so no event is half-delivered when the
    /// process exits. Safe to call twice; the second call does nothing.
    ///
    /// **Everything published before this call is delivered first.** The pump
    /// drains its queue on the way out rather than dropping it, and the reason
    /// is the moment this runs: `RunEvent::Exit` is exactly when the last
    /// events matter most, and they are the ones still in flight. Without the
    /// drain the signal and the backlog are two ready operations that
    /// `select!` chooses between at random, so the events describing why a
    /// process is going down are the events most likely to go down with it —
    /// uncounted, because `dropped` only counts a queue that was full.
    ///
    /// The guarantee stops at "before this call". An event published from
    /// another thread while this one is inside `shutdown` may or may not land,
    /// and nothing here can change that.
    pub fn shutdown(&self) {
        let Some(handle) = self.pump.lock().take() else {
            return;
        };
        // A full shutdown channel means the signal is already sent, and a
        // disconnected one means the pump is already gone. Neither is an error.
        let _ = self.shutdown_tx.try_send(());
        let _ = handle.join();
        self.subscribers.write().clear();
        // After the join, so a sink is never dropped while the pump is still
        // inside its `deliver`.
        self.sinks.write().clear();
    }
}

fn pump_loop(
    rx: Receiver<TelemetryEvent>,
    shutdown: Receiver<()>,
    subscribers: Arc<RwLock<HashMap<u64, Channel<TelemetryEvent>>>>,
    sinks: Arc<RwLock<HashMap<u64, Arc<dyn TelemetrySink>>>>,
) {
    loop {
        select! {
            recv(rx) -> received => match received {
                Ok(event) => {
                    fan_out(&subscribers, &event);
                    // After the windows rather than before them. A sink is a
                    // file or a socket the operator asked for and a window is a
                    // person watching; when the two contend, the person waits
                    // less.
                    deliver_to_sinks(&sinks, &event);
                }
                // Every producer has been dropped; there will never be another.
                Err(_) => break,
            },
            // Asked to stop — but `select!` picks at random between operations
            // that are both ready, so arriving here says nothing about `rx`
            // being empty. It usually is not: the caller published and then
            // called `shutdown`, which is the ordinary shape, and every event
            // still in the queue at that moment is one somebody has already
            // been told went out.
            //
            // So the queue is drained before the loop ends. What that buys is
            // stated on `TelemetryHub::shutdown` and it is narrow: everything
            // published *before* the shutdown signal is delivered, because a
            // bounded channel is FIFO and those sends have already landed in
            // it. A publish racing the shutdown from another thread is still a
            // race, and no amount of draining settles one.
            recv(shutdown) -> _ => {
                for event in rx.try_iter() {
                    fan_out(&subscribers, &event);
                    deliver_to_sinks(&sinks, &event);
                }
                break;
            }
        }
    }

    // `select!` picks at random between two arms that are both ready, so a
    // shutdown arriving while events are still queued can win the race and
    // leave them in the channel. They were accepted for delivery before anyone
    // asked for a stop, so they are delivered. The channel is bounded and both
    // deliveries are in-process, which makes this a bounded amount of work
    // rather than an open wait on anything.
    while let Ok(event) = rx.try_recv() {
        fan_out(&subscribers, &event);
        deliver_to_sinks(&sinks, &event);
    }
}

/// Hands one event to every sink, under the read lock.
///
/// No dead-listener sweep here, unlike `fan_out`: a sink has no channel that
/// can fail, so there is no signal that would mean "this one has gone away".
/// It stays until it is unregistered, which is the caller's job and is what
/// `TelemetryHub::unobserve` is for.
fn deliver_to_sinks(
    sinks: &Arc<RwLock<HashMap<u64, Arc<dyn TelemetrySink>>>>,
    event: &TelemetryEvent,
) {
    for sink in sinks.read().values() {
        sink.deliver(event);
    }
}

fn fan_out(
    subscribers: &Arc<RwLock<HashMap<u64, Channel<TelemetryEvent>>>>,
    event: &TelemetryEvent,
) {
    // Collect the dead under the read lock, remove them after dropping it.
    // `parking_lot`'s read guard does not upgrade, so taking the write lock while
    // still holding this one would deadlock the only pump thread there is.
    let mut dead = Vec::new();
    {
        for (id, channel) in subscribers.read().iter() {
            if channel.send(event.clone()).is_err() {
                dead.push(*id);
            }
        }
    }

    if !dead.is_empty() {
        let mut guard = subscribers.write();
        for id in dead {
            guard.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    #[derive(Default)]
    struct Collector {
        events: Mutex<Vec<TelemetryEvent>>,
    }

    impl TelemetrySink for Collector {
        fn deliver(&self, event: &TelemetryEvent) {
            self.events
                .lock()
                .expect("not poisoned")
                .push(event.clone());
        }
    }

    /// Everything published before `shutdown` arrives, however much of it was
    /// still queued when the signal went in.
    ///
    /// Written after the failure rather than before it. This is one half of an
    /// intermittent in `forensics_tests.rs` that failed 23 runs in 60 — a
    /// contradiction event and a clustering event that had been published,
    /// counted, and then thrown away by the pump on its way out, because
    /// `select!` chooses at random between a shutdown signal and a queue that
    /// is also ready. The test that found it asserted on two events and so
    /// caught it about a third of the time; this one publishes enough that a
    /// pump which drops its backlog cannot pass by luck.
    ///
    /// Falsified: with the drain removed from `pump_loop` this fails on every
    /// run, naming how many of the hundred arrived.
    #[test]
    fn a_shutdown_delivers_what_was_already_queued() {
        let hub = TelemetryHub::start();
        let collector = Arc::new(Collector::default());
        hub.observe(Arc::clone(&collector) as Arc<dyn TelemetrySink>);

        // Comfortably inside `QUEUE_DEPTH`, so nothing here is a drop the
        // counter would own up to. Everything that goes missing went missing
        // in the pump.
        for index in 0..100 {
            hub.publish(
                TelemetryLevel::Info,
                "test",
                format!("event {index}"),
                serde_json::Value::Null,
            );
        }

        hub.shutdown();

        let events = collector.events.lock().expect("not poisoned").clone();
        assert_eq!(
            events.len(),
            100,
            "{} of 100 published events survived the shutdown",
            events.len()
        );
        // In order, and every one of them, so a drain that delivered a
        // shuffled or partial backlog is not mistaken for a working one.
        for (index, event) in events.iter().enumerate() {
            assert_eq!(event.message, format!("event {index}"));
        }
    }

    /// The counters do not quietly absorb the difference.
    ///
    /// `dropped` counts a queue that was full, and it is the only number an
    /// operator has for "the hub did not deliver something". A pump dropping
    /// its backlog on shutdown never touched it, which is why the defect was
    /// invisible in `snapshot()` and had to be found through a flaky test.
    #[test]
    fn nothing_was_dropped_on_the_way_to_that_shutdown() {
        let hub = TelemetryHub::start();
        let collector = Arc::new(Collector::default());
        hub.observe(Arc::clone(&collector) as Arc<dyn TelemetrySink>);

        for index in 0..50 {
            hub.publish(
                TelemetryLevel::Warn,
                "test",
                format!("event {index}"),
                serde_json::Value::Null,
            );
        }
        let before = hub.snapshot();
        hub.shutdown();

        assert_eq!(before.dropped, 0, "the queue was never full");
        assert_eq!(before.published, 50);
        assert_eq!(
            collector.events.lock().expect("not poisoned").len(),
            50,
            "published, not dropped, and therefore delivered"
        );
    }

    /// Twice is not an error, and the second call delivers nothing new.
    #[test]
    fn shutting_down_twice_is_shutting_down() {
        let hub = TelemetryHub::start();
        let collector = Arc::new(Collector::default());
        hub.observe(Arc::clone(&collector) as Arc<dyn TelemetrySink>);

        hub.publish(
            TelemetryLevel::Info,
            "test",
            "only one",
            serde_json::Value::Null,
        );
        hub.shutdown();
        hub.shutdown();

        assert_eq!(collector.events.lock().expect("not poisoned").len(), 1);
        assert!(!hub.snapshot().running);
    }
}
