//! The engine's numbers in the format a Prometheus scraper reads.
//!
//! `metrics.rs` counts and `metrics.rs` serves; this turns one snapshot into
//! the text that goes between them. It is a pure function of a
//! `MetricsSnapshot` — no clock, no atomics, no socket — so every line it can
//! produce is checked below without opening a port.
//!
//! Four decisions shape the output.
//!
//! **Seconds, not microseconds.** Every duration a scraper sees is in seconds,
//! because that is the unit the rest of the Prometheus world assumes. A
//! dashboard that has to know this one exporter counts in microseconds is a
//! dashboard somebody will eventually read wrong, at a moment when reading it
//! wrong is expensive. The conversion is integer division into a fixed six
//! decimal places, so it is exact and two runs over the same numbers print the
//! same characters — the same reason `metrics.rs` reports ratios in basis
//! points rather than as floats.
//!
//! **A number that is not known is a line that is not printed.** The text
//! format has no way to write `null`. Printing a zero would claim a p99 of
//! "instant" for a histogram that has never been handed a sample, which is the
//! one reading `metrics.rs` exists to prevent. So an unknown quantile is
//! omitted entirely and the scraper is left with no series rather than a
//! confident wrong one.
//!
//! **The bucket ladder is printed in full, every time.** A snapshot carries
//! only the buckets with something in them, which is right for JSON and wrong
//! here: a histogram whose boundaries appear and disappear between scrapes
//! cannot be aggregated over time. So the whole of `BUCKET_BOUNDS_US` is
//! printed on every scrape, zeros included, and the counts are made cumulative
//! on the way out because cumulative is what `le` means.
//!
//! **Rendering allocates; recording still does not.** This builds a string, and
//! that is fine — it runs on a scrape, on the exporter's own task, reading
//! atomics that no writer waits on. The rule in `metrics.rs` is that the
//! *engine* never pays for being measured, and nothing here is on the engine's
//! path.

use std::fmt::Write;

use crate::metrics::{
    BackpressureState, FeedSnapshot, HistogramSnapshot, MetricsSnapshot, StateCount,
    BUCKET_BOUNDS_US,
};

/// What this format calls itself. Version 0.0.4 is the text exposition format
/// every Prometheus server since 2014 reads, and the one `Accept` asks for.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// The prefix on every metric name, so nothing here can collide with another
/// exporter on the same machine.
const PREFIX: &str = "sts";

/// How one quantile is read off a histogram snapshot.
type Quantile = fn(&HistogramSnapshot) -> Option<u64>;

/// The quantiles a snapshot carries, and the label each is printed under.
///
/// Strings rather than computed from a number: `0.999` formatted by anything
/// float-shaped is a coin flip between `0.999` and `0.9990000000000001`, and a
/// label that changes shape between builds silently becomes a second series.
const QUANTILES: [(&str, Quantile); 4] = [
    ("0.5", |h| h.p50_us),
    ("0.95", |h| h.p95_us),
    ("0.99", |h| h.p99_us),
    ("0.999", |h| h.p999_us),
];

/// One snapshot as an exposition-format document.
pub fn render(snapshot: &MetricsSnapshot) -> String {
    // Sized for the document this actually produces — roughly 150 series, most
    // of them a bucket line in the sixty-character range. One allocation up
    // front beats a dozen as it grows.
    let mut out = String::with_capacity(16 * 1024);

    push_meta(&mut out, snapshot);
    push_slots(&mut out, snapshot);
    push_feed(&mut out, &snapshot.feed);
    push_execution(&mut out, snapshot);

    out
}

// ---------------------------------------------------------------------------
// the sections
// ---------------------------------------------------------------------------

/// What these numbers are, where they came from, and when they reset.
///
/// `STS_CORE_IDEOLOGY.md` §Annex V asks that every metric carry its source, its
/// period and its reset semantics. The JSON carries them as three fields; the
/// text format has no place for that, so they ride on an info metric — the
/// standard way to attach facts that are strings to a scrape.
fn push_meta(out: &mut String, snapshot: &MetricsSnapshot) {
    help(
        out,
        "exporter_info",
        "What these numbers are and when they reset. Always 1.",
    );
    type_of(out, "exporter_info", "gauge");
    name(out, "exporter_info");
    out.push_str("{source=\"");
    label_value(out, snapshot.source);
    out.push_str("\",aggregation=\"");
    label_value(out, snapshot.aggregation);
    out.push_str("\",resets=\"");
    label_value(out, snapshot.resets);
    out.push_str("\"} 1\n");

    help(
        out,
        "uptime_seconds",
        "How long this process has been running. Every total below covers exactly this.",
    );
    type_of(out, "uptime_seconds", "gauge");
    name(out, "uptime_seconds");
    out.push(' ');
    // `uptime_ms` and `started_at_ms` are `i64` and cannot be negative in
    // practice, but a monotonic clock read is not something to assume about.
    millis_as_seconds(out, snapshot.uptime_ms.max(0) as u64);
    out.push('\n');

    help(
        out,
        "start_time_seconds",
        "When this process started, in seconds since the epoch.",
    );
    type_of(out, "start_time_seconds", "gauge");
    name(out, "start_time_seconds");
    out.push(' ');
    millis_as_seconds(out, snapshot.started_at_ms.max(0) as u64);
    out.push('\n');
}

/// The engine's tick: how often it happens, what it costs, and how steady it is.
fn push_slots(out: &mut String, snapshot: &MetricsSnapshot) {
    let slots = &snapshot.slots;

    counter(
        out,
        "slot_ticks_total",
        "Slot advances the engine has handled.",
        slots.ticks,
    );
    gauge(
        out,
        "slot_newest",
        "The highest slot any tick has carried. Zero means nothing has ticked yet.",
        slots.newest_slot,
    );
    counter(
        out,
        "slot_regressions_total",
        "Ticks that arrived carrying a slot older than one already seen.",
        slots.regressions,
    );
    counter(
        out,
        "slot_missed_total",
        "Slots that went by without a tick.",
        slots.missed,
    );

    // Absent rather than zero: "nothing has ticked yet" and "a tick just
    // happened" are the two readings a stalled engine has to be told apart by.
    if let Some(since_ms) = slots.since_last_tick_ms {
        help(
            out,
            "slot_since_last_tick_seconds",
            "How long since the last tick. Absent when there has never been one.",
        );
        type_of(out, "slot_since_last_tick_seconds", "gauge");
        name(out, "slot_since_last_tick_seconds");
        out.push(' ');
        millis_as_seconds(out, since_ms);
        out.push('\n');
    }

    histogram_family(
        out,
        "slot_processing",
        "Time the engine spent handling one tick.",
        &slots.processing_us,
    );
    histogram_family(
        out,
        "slot_gap",
        "Time between one tick and the next.",
        &slots.gap_us,
    );
    histogram_family(
        out,
        "slot_jitter",
        "How much each gap differed from the gap before it. This is the wobble in the clock.",
        &slots.jitter_us,
    );
}

/// What the feed delivered, what it lost, and how close the queue is to full.
fn push_feed(out: &mut String, feed: &FeedSnapshot) {
    counter(
        out,
        "feed_ingested_frames_total",
        "Frames that made it all the way through.",
        feed.ingested,
    );

    help(
        out,
        "feed_dropped_frames_total",
        "Frames that did not make it through, by reason. Most of a healthy run is 'filtered'.",
    );
    type_of(out, "feed_dropped_frames_total", "counter");
    for drop in &feed.drops {
        name(out, "feed_dropped_frames_total");
        out.push_str("{reason=\"");
        label_value(out, drop.reason.as_str());
        out.push_str("\"} ");
        integer(out, drop.frames);
        out.push('\n');
    }

    counter(
        out,
        "feed_overrun_frames_total",
        "Frames lost because the engine could not keep up: a full queue, or a sink that would \
         not take them. This is the one worth an alarm.",
        feed.overrun,
    );

    // Both ratios are printed from basis points, so what a scraper reads is
    // exactly what the JSON says divided by ten thousand — no third rounding.
    help(
        out,
        "feed_loss_ratio",
        "Every drop as a share of everything offered. High is normal: refusing what is not a \
         candidate is the job.",
    );
    type_of(out, "feed_loss_ratio", "gauge");
    name(out, "feed_loss_ratio");
    out.push(' ');
    bps_as_ratio(out, feed.loss_bps);
    out.push('\n');

    help(
        out,
        "feed_overrun_ratio",
        "Frames lost to the engine falling behind, as a share of everything offered.",
    );
    type_of(out, "feed_overrun_ratio", "gauge");
    name(out, "feed_overrun_ratio");
    out.push(' ');
    bps_as_ratio(out, feed.overrun_bps);
    out.push('\n');

    gauge(
        out,
        "feed_queue_depth",
        "How many frames are waiting between the feed and the engine right now.",
        feed.depth,
    );
    gauge(
        out,
        "feed_queue_capacity",
        "How many that queue can hold.",
        feed.capacity,
    );
    gauge(
        out,
        "feed_queue_deepest",
        "The fullest that queue has ever been. Survives the burst that caused it.",
        feed.deepest,
    );

    help(
        out,
        "feed_queue_fill_ratio",
        "How full that queue is right now, as a share of capacity.",
    );
    type_of(out, "feed_queue_fill_ratio", "gauge");
    name(out, "feed_queue_fill_ratio");
    out.push(' ');
    percent_as_ratio(out, feed.fill_percent);
    out.push('\n');

    counter(
        out,
        "feed_queue_observations_total",
        "Depth readings taken. Zero means nothing has ever looked, which is not an empty \
         queue.",
        feed.observations,
    );

    // The band as a set of zeros and a one, which is how an enum is carried in
    // this format. A single number would make `saturated` sort next to
    // `elevated` in a way that means nothing.
    help(
        out,
        "feed_backpressure_state",
        "Which saturation band the queue is in now. 1 on the current band, 0 on the others.",
    );
    type_of(out, "feed_backpressure_state", "gauge");
    for band in BackpressureState::ALL {
        name(out, "feed_backpressure_state");
        out.push_str("{state=\"");
        label_value(out, band.as_str());
        out.push_str("\"} ");
        out.push(if band == feed.state { '1' } else { '0' });
        out.push('\n');
    }

    counter(
        out,
        "feed_backpressure_transitions_total",
        "Times the queue crossed from one saturation band into another. The crossing is the \
         interesting part, not the depth.",
        feed.transitions,
    );

    help(
        out,
        "feed_backpressure_entries_total",
        "Times the queue entered each saturation band.",
    );
    type_of(out, "feed_backpressure_entries_total", "counter");
    for band in &feed.bands {
        name(out, "feed_backpressure_entries_total");
        out.push_str("{state=\"");
        label_value(out, band.state.as_str());
        out.push_str("\"} ");
        integer(out, band.entries);
        out.push('\n');
    }

    help(
        out,
        "feed_backpressure_dwell_seconds_total",
        "How long the queue has spent in each saturation band, including the stretch it is in \
         now.",
    );
    type_of(out, "feed_backpressure_dwell_seconds_total", "counter");
    for band in &feed.bands {
        name(out, "feed_backpressure_dwell_seconds_total");
        out.push_str("{state=\"");
        label_value(out, band.state.as_str());
        out.push_str("\"} ");
        millis_as_seconds(out, band.dwell_ms);
        out.push('\n');
    }
}

/// Where the engine's executions are, and how many have ever been there.
fn push_execution(out: &mut String, snapshot: &MetricsSnapshot) {
    let execution = &snapshot.execution;

    help(
        out,
        "execution_in_flight",
        "How many are somewhere that is not a terminal state right now. This one can go down.",
    );
    type_of(out, "execution_in_flight", "gauge");
    in_flight(out, "intent", execution.in_flight_intents);
    in_flight(out, "exit", execution.in_flight_exits);

    push_states(
        out,
        "execution_intent_state",
        "How many intents are sitting in each step right now.",
        "execution_intent_entered_total",
        "How many intents have ever entered each step.",
        &execution.intents,
    );
    push_states(
        out,
        "execution_signer_state",
        "How many exits are sitting in each signer step right now.",
        "execution_signer_entered_total",
        "How many exits have ever entered each signer step.",
        &execution.signer,
    );

    counter(
        out,
        "execution_unobserved_total",
        "Steps out of a state this process never saw entered. A restart makes this non-zero \
         honestly; read it beside the run rather than alarming on it.",
        execution.unobserved,
    );
}

/// The occupancy gauge and the entered counter for one state machine.
///
/// Both families carry the same `terminal` label, so a query can separate "in
/// flight" from "finished" without a list of state names baked into it.
fn push_states(
    out: &mut String,
    live_name: &str,
    live_help: &str,
    total_name: &str,
    total_help: &str,
    states: &[StateCount],
) {
    help(out, live_name, live_help);
    type_of(out, live_name, "gauge");
    for state in states {
        name(out, live_name);
        push_state_labels(out, state);
        out.push(' ');
        signed(out, state.in_state);
        out.push('\n');
    }

    help(out, total_name, total_help);
    type_of(out, total_name, "counter");
    for state in states {
        name(out, total_name);
        push_state_labels(out, state);
        out.push(' ');
        integer(out, state.entered);
        out.push('\n');
    }
}

fn push_state_labels(out: &mut String, state: &StateCount) {
    out.push_str("{state=\"");
    label_value(out, state.state);
    out.push_str("\",terminal=\"");
    out.push_str(if state.terminal { "true" } else { "false" });
    out.push_str("\"}");
}

fn in_flight(out: &mut String, kind: &str, value: i64) {
    name(out, "execution_in_flight");
    out.push_str("{kind=\"");
    label_value(out, kind);
    out.push_str("\"} ");
    signed(out, value);
    out.push('\n');
}

// ---------------------------------------------------------------------------
// histograms
// ---------------------------------------------------------------------------

/// One histogram as the three families a scraper can use.
///
/// The `histogram` is the aggregatable truth: bucket counts add up across
/// processes and across time, which is the only way a quantile over several
/// runs is honest. The two gauge families beside it carry what buckets cannot —
/// the quantiles this process computed for itself, accurate to the width of
/// their bucket, and the exact extremes.
///
/// They are separate names rather than labels on one, because a single family
/// cannot be a histogram and a summary at once and a scraper handed both under
/// one name will reject the scrape.
///
/// `stem` is the name without its unit — the three families add `_seconds`
/// themselves, so the unit lands at the end of each name where the convention
/// puts it and where a reader looks for it.
fn histogram_family(out: &mut String, stem: &str, what: &str, snapshot: &HistogramSnapshot) {
    push_buckets(out, &format!("{stem}_seconds"), what, snapshot);
    push_quantiles(out, &format!("{stem}_quantile_seconds"), what, snapshot);
    push_extremes(out, &format!("{stem}_extreme_seconds"), what, snapshot);
}

fn push_buckets(out: &mut String, base: &str, what: &str, snapshot: &HistogramSnapshot) {
    help(out, base, what);
    type_of(out, base, "histogram");

    let mut cumulative: u64 = 0;
    let mut reported = snapshot.buckets.iter().peekable();
    for &bound in BUCKET_BOUNDS_US.iter() {
        // The snapshot lists only the buckets that have something in them, in
        // ascending order. Walking the two together fills the gaps with the
        // running total, which is what a cumulative bucket is.
        if let Some(bucket) = reported.peek() {
            if bucket.le_us == Some(bound) {
                cumulative = cumulative.saturating_add(bucket.count);
                reported.next();
            }
        }
        name(out, base);
        out.push_str("_bucket{le=\"");
        micros_as_seconds(out, bound);
        out.push_str("\"} ");
        integer(out, cumulative);
        out.push('\n');
    }

    // Everything, including the overflow bucket that has no upper bound.
    //
    // `count` is read as its own atomic, one instruction away from the buckets,
    // so a sample landing mid-read can leave the two disagreeing by one. The
    // larger wins: `le` has to be non-decreasing for the series to be valid at
    // all, and a `+Inf` below the bucket under it is a scrape a server refuses.
    let total = snapshot.count.max(cumulative);
    name(out, base);
    out.push_str("_bucket{le=\"+Inf\"} ");
    integer(out, total);
    out.push('\n');

    name(out, base);
    out.push_str("_sum ");
    micros_as_seconds(out, snapshot.sum_us);
    out.push('\n');

    name(out, base);
    out.push_str("_count ");
    integer(out, total);
    out.push('\n');
}

fn push_quantiles(out: &mut String, quantile_name: &str, what: &str, snapshot: &HistogramSnapshot) {
    let mut wrote_header = false;
    for (label, read) in QUANTILES {
        // No sample, no line. A zero here would read as "instant" for something
        // that has never been measured at all.
        let Some(micros) = read(snapshot) else {
            continue;
        };
        if !wrote_header {
            help(
                out,
                quantile_name,
                &format!("{what} Computed in this process, accurate to the width of its bucket."),
            );
            type_of(out, quantile_name, "gauge");
            wrote_header = true;
        }
        name(out, quantile_name);
        out.push_str("{quantile=\"");
        out.push_str(label);
        out.push_str("\"} ");
        micros_as_seconds(out, micros);
        out.push('\n');
    }
}

fn push_extremes(out: &mut String, extreme_name: &str, what: &str, snapshot: &HistogramSnapshot) {
    let (Some(min_us), Some(max_us)) = (snapshot.min_us, snapshot.max_us) else {
        return;
    };
    help(
        out,
        extreme_name,
        &format!("{what} The exact smallest and largest readings, which buckets cannot give back."),
    );
    type_of(out, extreme_name, "gauge");

    name(out, extreme_name);
    out.push_str("{extreme=\"min\"} ");
    micros_as_seconds(out, min_us);
    out.push('\n');

    name(out, extreme_name);
    out.push_str("{extreme=\"max\"} ");
    micros_as_seconds(out, max_us);
    out.push('\n');
}

// ---------------------------------------------------------------------------
// the little writers
// ---------------------------------------------------------------------------

fn counter(out: &mut String, metric: &str, what: &str, value: u64) {
    help(out, metric, what);
    type_of(out, metric, "counter");
    name(out, metric);
    out.push(' ');
    integer(out, value);
    out.push('\n');
}

fn gauge(out: &mut String, metric: &str, what: &str, value: u64) {
    help(out, metric, what);
    type_of(out, metric, "gauge");
    name(out, metric);
    out.push(' ');
    integer(out, value);
    out.push('\n');
}

fn name(out: &mut String, metric: &str) {
    out.push_str(PREFIX);
    out.push('_');
    out.push_str(metric);
}

fn help(out: &mut String, metric: &str, what: &str) {
    out.push_str("# HELP ");
    name(out, metric);
    out.push(' ');
    // A HELP line ends at the newline, so an escaped one is the only way a
    // sentence with a break in it survives the trip.
    for ch in what.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
    out.push('\n');
}

fn type_of(out: &mut String, metric: &str, kind: &str) {
    out.push_str("# TYPE ");
    name(out, metric);
    out.push(' ');
    out.push_str(kind);
    out.push('\n');
}

/// A label value, with the three characters the format reserves escaped.
///
/// Every label this module writes is a fixed identifier from an `as_str`, so
/// nothing reaches here that needs escaping today. It is written anyway,
/// because the day something dynamic does reach it is not the day to discover
/// a quote in a state name closes the label early.
fn label_value(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
}

fn integer(out: &mut String, value: u64) {
    let _ = write!(out, "{value}");
}

fn signed(out: &mut String, value: i64) {
    let _ = write!(out, "{value}");
}

/// Microseconds as seconds, exactly.
///
/// Integer division into six fixed decimal places rather than a float divide:
/// the same reading prints the same characters on every machine and in every
/// build, and nothing here ever grows a `0.30000000000000004`.
fn micros_as_seconds(out: &mut String, micros: u64) {
    let _ = write!(out, "{}.{:06}", micros / 1_000_000, micros % 1_000_000);
}

/// Milliseconds as seconds, on the same terms.
fn millis_as_seconds(out: &mut String, millis: u64) {
    let _ = write!(out, "{}.{:03}", millis / 1_000, millis % 1_000);
}

/// Basis points as a plain share, so 10000 prints as `1.0000`.
fn bps_as_ratio(out: &mut String, bps: u64) {
    let _ = write!(out, "{}.{:04}", bps / 10_000, bps % 10_000);
}

/// A percentage as a plain share, so 100 prints as `1.00`.
fn percent_as_ratio(out: &mut String, percent: u64) {
    let _ = write!(out, "{}.{:02}", percent / 100, percent % 100);
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::{DropReason, Histogram, MetricsCollector};
    use crate::types::{ExecutionState, ExitState};
    use std::collections::{HashMap, HashSet};
    use std::time::Duration;

    /// A collector with something in every section, so a rendering that only
    /// works on zeros cannot pass.
    fn busy() -> MetricsCollector {
        let collector = MetricsCollector::new();
        for (step, slot) in (100u64..112).enumerate() {
            collector.record_slot_tick(slot, Duration::from_micros(40 + step as u64 * 13));
        }
        collector.record_ingested(9_000);
        collector.record_dropped(DropReason::Filtered, 800);
        collector.record_dropped(DropReason::Backpressure, 12);
        collector.observe_queue(700, 1_024);
        collector.record_intent(None, ExecutionState::IntentCreated);
        collector.record_intent(
            Some(ExecutionState::IntentCreated),
            ExecutionState::Validated,
        );
        collector.record_exit(None, ExitState::ExitConstructed);
        collector
    }

    /// Every line that is a sample rather than a comment.
    fn samples(body: &str) -> Vec<&str> {
        body.lines()
            .filter(|line| !line.starts_with('#') && !line.is_empty())
            .collect()
    }

    /// The metric name at the front of a sample line, without its labels.
    fn series_name(line: &str) -> &str {
        let end = line
            .find(['{', ' '])
            .expect("a sample line has a value after its name");
        &line[..end]
    }

    /// The name and labels together — what makes a series unique.
    fn series_key(line: &str) -> &str {
        let value_at = line.rfind(' ').expect("a sample line ends in its value");
        &line[..value_at]
    }

    // -----------------------------------------------------------------------
    // the document as a whole
    // -----------------------------------------------------------------------

    /// The structural check the rest of the tests lean on: everything printed
    /// is declared, everything declared is printed, and nothing is printed
    /// twice. A scrape that fails any of the three is one Prometheus rejects
    /// outright, and it would fail it silently, at three in the morning.
    #[test]
    fn every_series_is_declared_and_every_declaration_is_used() {
        let body = render(&busy().snapshot());

        let mut declared: HashMap<&str, &str> = HashMap::new();
        for line in body.lines().filter(|line| line.starts_with("# TYPE ")) {
            let mut parts = line["# TYPE ".len()..].split(' ');
            let name = parts.next().expect("a TYPE names a metric");
            let kind = parts.next().expect("a TYPE says which kind");
            assert!(
                declared.insert(name, kind).is_none(),
                "{name} is declared twice"
            );
        }

        let mut used: HashSet<&str> = HashSet::new();
        for line in samples(&body) {
            let name = series_name(line);
            // A histogram's samples are three suffixed families under one
            // declaration; everything else is named exactly what it declared.
            let family = declared
                .keys()
                .find(|candidate| {
                    name == **candidate
                        || (declared[*candidate] == "histogram"
                            && (name == format!("{candidate}_bucket")
                                || name == format!("{candidate}_sum")
                                || name == format!("{candidate}_count")))
                })
                .unwrap_or_else(|| panic!("{name} was printed without a # TYPE"));
            used.insert(family);
        }

        for name in declared.keys() {
            assert!(used.contains(name), "{name} was declared and never printed");
        }
    }

    #[test]
    fn nothing_is_printed_twice() {
        let body = render(&busy().snapshot());
        let lines = samples(&body);
        let unique: HashSet<&str> = lines.iter().map(|line| series_key(line)).collect();
        assert_eq!(
            unique.len(),
            lines.len(),
            "a repeated series makes the whole scrape invalid"
        );
    }

    #[test]
    fn every_help_line_comes_before_its_type_line() {
        let body = render(&busy().snapshot());
        let mut expecting: Option<&str> = None;
        for line in body.lines() {
            if let Some(rest) = line.strip_prefix("# HELP ") {
                let name = rest.split(' ').next().expect("a HELP names a metric");
                assert!(
                    expecting.is_none(),
                    "two HELP lines with no TYPE between them"
                );
                expecting = Some(name);
            } else if let Some(rest) = line.strip_prefix("# TYPE ") {
                let name = rest.split(' ').next().expect("a TYPE names a metric");
                assert_eq!(
                    expecting.take(),
                    Some(name),
                    "{name} has a TYPE with no HELP above it"
                );
            }
        }
        assert_eq!(expecting, None, "the last HELP never got a TYPE");
    }

    #[test]
    fn every_name_carries_the_prefix() {
        let body = render(&busy().snapshot());
        for line in samples(&body) {
            assert!(
                series_name(line).starts_with("sts_"),
                "{line} would collide with another exporter on this machine"
            );
        }
    }

    #[test]
    fn the_document_ends_in_a_newline() {
        // A body that stops mid-line is one a parser has to guess about.
        let body = render(&busy().snapshot());
        assert!(body.ends_with('\n'));
    }

    // -----------------------------------------------------------------------
    // histograms
    // -----------------------------------------------------------------------

    #[test]
    fn the_whole_ladder_is_printed_even_when_most_of_it_is_empty() {
        let collector = MetricsCollector::new();
        collector.record_slot_tick(1, Duration::from_micros(7));
        let body = render(&collector.snapshot());

        let bounds: Vec<&str> = body
            .lines()
            .filter(|line| line.starts_with("sts_slot_processing_seconds_bucket{"))
            .collect();
        assert_eq!(
            bounds.len(),
            BUCKET_BOUNDS_US.len() + 1,
            "boundaries that come and go between scrapes cannot be aggregated over time"
        );
        assert!(bounds
            .last()
            .expect("a ladder has a top")
            .contains("le=\"+Inf\""));
    }

    #[test]
    fn buckets_are_cumulative_and_the_top_one_holds_everything() {
        let histogram = Histogram::new();
        for micros in [3u64, 3, 40, 900, 7_000_000] {
            histogram.record_us(micros);
        }
        let mut out = String::new();
        push_buckets(&mut out, "test_seconds", "a test", &histogram.snapshot());

        let mut previous = 0u64;
        let mut top = 0u64;
        for line in out
            .lines()
            .filter(|line| line.starts_with("sts_test_seconds_bucket{"))
        {
            let count: u64 = line
                .rsplit(' ')
                .next()
                .expect("a bucket ends in its count")
                .parse()
                .expect("a count is a number");
            assert!(
                count >= previous,
                "a cumulative bucket cannot go down: {line}"
            );
            previous = count;
            top = count;
        }
        assert_eq!(
            top, 5,
            "the +Inf bucket holds every reading, the overflow one included"
        );
        assert!(out.contains("sts_test_seconds_count 5"));
    }

    #[test]
    fn the_sum_is_the_real_total_rather_than_the_mean_times_the_count() {
        let histogram = Histogram::new();
        // 49µs over three readings is a mean of 16 and a bit. A sum rebuilt
        // from that mean would print 48µs, which is the rounding this exists
        // to avoid — an average latency that is quietly wrong every scrape.
        for micros in [7u64, 13, 29] {
            histogram.record_us(micros);
        }
        let snapshot = histogram.snapshot();
        assert_eq!(snapshot.mean_us, Some(16), "the mean really is lossy here");

        let mut out = String::new();
        push_buckets(&mut out, "test_seconds", "a test", &snapshot);
        assert!(out.contains("sts_test_seconds_sum 0.000049"), "got: {out}");
    }

    #[test]
    fn a_histogram_nothing_has_been_put_in_still_prints_its_shape() {
        let mut out = String::new();
        let empty = Histogram::new().snapshot();
        push_buckets(&mut out, "test_seconds", "a test", &empty);

        assert!(out.contains("sts_test_seconds_bucket{le=\"+Inf\"} 0"));
        assert!(out.contains("sts_test_seconds_count 0"));
        assert!(out.contains("sts_test_seconds_sum 0.000000"));
    }

    #[test]
    fn a_quantile_nothing_was_measured_for_is_left_out_rather_than_called_zero() {
        let empty = Histogram::new().snapshot();

        let mut quantiles = String::new();
        push_quantiles(&mut quantiles, "test_quantile_seconds", "a test", &empty);
        assert!(
            quantiles.is_empty(),
            "a p99 of zero would read as instant: {quantiles}"
        );

        let mut extremes = String::new();
        push_extremes(&mut extremes, "test_extreme_seconds", "a test", &empty);
        assert!(extremes.is_empty());
    }

    #[test]
    fn the_jitter_quantiles_are_all_four_and_they_never_go_backwards() {
        let body = render(&busy().snapshot());
        let printed: Vec<&str> = body
            .lines()
            .filter(|line| line.starts_with("sts_slot_jitter_quantile_seconds{"))
            .collect();
        assert_eq!(printed.len(), 4, "p50, p95, p99 and p999");

        let mut previous = 0.0f64;
        for (line, label) in printed.iter().zip(["0.5", "0.95", "0.99", "0.999"]) {
            assert!(
                line.contains(&format!("quantile=\"{label}\"")),
                "{line} is out of order"
            );
            let seconds: f64 = line
                .rsplit(' ')
                .next()
                .expect("a value")
                .parse()
                .expect("seconds parse");
            assert!(seconds >= previous, "quantiles cannot go backwards: {line}");
            previous = seconds;
        }
    }

    #[test]
    fn a_histogram_and_its_quantiles_are_never_the_same_family() {
        // One name cannot be a histogram and a summary at once — a scraper
        // handed both under one name rejects the whole document.
        let body = render(&busy().snapshot());
        for line in samples(&body) {
            if line.contains("quantile=\"") {
                assert!(
                    series_name(line).ends_with("_quantile_seconds"),
                    "{line} puts a quantile label on a histogram family"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // the feed
    // -----------------------------------------------------------------------

    #[test]
    fn every_drop_reason_gets_a_series_even_the_ones_at_zero() {
        let body = render(&busy().snapshot());
        for reason in DropReason::ALL {
            let label = format!(
                "sts_feed_dropped_frames_total{{reason=\"{}\"}}",
                reason.as_str()
            );
            assert!(
                body.contains(&label),
                "{label} is missing, so a dashboard would show a gap"
            );
        }
    }

    #[test]
    fn the_band_the_queue_is_in_is_a_one_and_the_others_are_zeros() {
        let collector = MetricsCollector::new();
        collector.observe_queue(700, 1_024);
        let body = render(&collector.snapshot());

        assert!(body.contains("sts_feed_backpressure_state{state=\"nominal\"} 0"));
        assert!(body.contains("sts_feed_backpressure_state{state=\"elevated\"} 1"));
        assert!(body.contains("sts_feed_backpressure_state{state=\"saturated\"} 0"));
    }

    #[test]
    fn every_band_is_printed_whether_it_has_been_visited_or_not() {
        let body = render(&MetricsCollector::new().snapshot());
        for band in BackpressureState::ALL {
            assert!(
                body.contains(&format!(
                    "sts_feed_backpressure_dwell_seconds_total{{state=\"{}\"}}",
                    band.as_str()
                )),
                "{} has no dwell series",
                band.as_str()
            );
        }
    }

    #[test]
    fn the_ratios_are_exactly_the_basis_points_divided_by_ten_thousand() {
        let collector = MetricsCollector::new();
        collector.record_ingested(9_000);
        collector.record_dropped(DropReason::Filtered, 1_000);
        let snapshot = collector.snapshot();
        assert_eq!(
            snapshot.feed.loss_bps, 1_000,
            "a tenth of everything offered"
        );

        let body = render(&snapshot);
        assert!(body.contains("sts_feed_loss_ratio 0.1000"), "got: {body}");
    }

    // -----------------------------------------------------------------------
    // execution
    // -----------------------------------------------------------------------

    #[test]
    fn a_terminal_state_says_so_on_every_series_it_appears_in() {
        let body = render(&busy().snapshot());
        assert!(body.contains("sts_execution_intent_state{state=\"completed\",terminal=\"true\"}"));
        assert!(body.contains("sts_execution_intent_state{state=\"validated\",terminal=\"false\"}"));
        assert!(body.contains(
            "sts_execution_intent_entered_total{state=\"validated\",terminal=\"false\"} 1"
        ));
    }

    #[test]
    fn in_flight_is_split_by_what_is_in_flight() {
        let body = render(&busy().snapshot());
        assert!(body.contains("sts_execution_in_flight{kind=\"intent\"} 1"));
        assert!(body.contains("sts_execution_in_flight{kind=\"exit\"} 1"));
    }

    #[test]
    fn a_gauge_that_went_negative_is_printed_as_it_is_rather_than_clamped() {
        // The occupancy gauges are signed, and `metrics.rs` guarantees they do
        // not go below zero. If that guarantee ever breaks, the number a
        // dashboard shows should be the broken one, not a tidied one.
        let mut out = String::new();
        in_flight(&mut out, "intent", -3);
        assert!(
            out.contains("sts_execution_in_flight{kind=\"intent\"} -3"),
            "got: {out}"
        );
    }

    // -----------------------------------------------------------------------
    // the meta section, and how numbers are written
    // -----------------------------------------------------------------------

    #[test]
    fn a_tick_that_has_never_happened_prints_no_line_at_all() {
        let fresh = render(&MetricsCollector::new().snapshot());
        assert!(
            !fresh.contains("sts_slot_since_last_tick_seconds"),
            "zero here would read as a tick that just happened"
        );

        let collector = MetricsCollector::new();
        collector.record_slot_tick(1, Duration::from_micros(10));
        assert!(render(&collector.snapshot()).contains("sts_slot_since_last_tick_seconds"));
    }

    #[test]
    fn the_source_and_the_reset_rule_ride_along_with_the_numbers() {
        let body = render(&busy().snapshot());
        assert!(body.contains("sts_exporter_info{source=\"sts.engine\""));
        assert!(
            body.contains("resets=\"process start; nothing here is ever reset while running\"} 1")
        );
    }

    #[test]
    fn microseconds_become_seconds_exactly() {
        let mut out = String::new();
        micros_as_seconds(&mut out, 1);
        out.push(' ');
        micros_as_seconds(&mut out, 999_999);
        out.push(' ');
        micros_as_seconds(&mut out, 1_000_000);
        out.push(' ');
        micros_as_seconds(&mut out, 5_000_000);
        assert_eq!(out, "0.000001 0.999999 1.000000 5.000000");
    }

    #[test]
    fn a_ratio_at_its_limits_reads_the_way_it_should() {
        let mut out = String::new();
        bps_as_ratio(&mut out, 0);
        out.push(' ');
        bps_as_ratio(&mut out, 1);
        out.push(' ');
        bps_as_ratio(&mut out, 10_000);
        assert_eq!(out, "0.0000 0.0001 1.0000");

        let mut fill = String::new();
        percent_as_ratio(&mut fill, 100);
        assert_eq!(fill, "1.00");
    }

    #[test]
    fn a_quote_in_a_label_cannot_close_the_label_early() {
        let mut out = String::new();
        label_value(&mut out, r#"a "quoted" \ name"#);
        assert_eq!(out, r#"a \"quoted\" \\ name"#);
    }

    #[test]
    fn a_newline_in_a_help_line_cannot_end_it_early() {
        let mut out = String::new();
        help(&mut out, "thing", "one line\nand another");
        assert_eq!(out, "# HELP sts_thing one line\\nand another\n");
        assert_eq!(
            out.lines().count(),
            1,
            "a HELP is one line however it was written"
        );
    }
}
