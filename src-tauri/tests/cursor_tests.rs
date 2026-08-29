//! `ChainCursor` across segment rolls and discontinuous streams.
//!
//! `backtest.rs` has the unit tests for auditing one file. These are the ones
//! that need two, because §3.3 makes segmentation a storage detail and not a
//! boundary in the evidence: segments roll at 64 MiB or at UTC midnight and the
//! chain runs across the roll. Everything that matters about a rolled stream is
//! therefore a property of the cursor handed from one file to the next, and a
//! test that audits a single file cannot see any of it.
//!
//! The forgery these guard against is the one that is invisible inside any one
//! file: a splice that breaks the chain in `000.jsonl` and reseals from there
//! on, so that `001.jsonl` is internally perfect. Read on its own it verifies
//! line by line. Only the cursor knows better.
//!
//! Everything here goes through the public API — `ChainWriter::seal`,
//! `to_line`, `audit_stream_from` — and builds its streams by sealing real
//! records rather than by pasting fixture text, so the hashes under test are
//! the ones the recorder actually produces.

use sts_lib::backtest::{
    audit_stream_from, BacktestConfig, ChainAudit, ChainCursor, Evaluator, ForensicReport,
    LineStatus,
};
use sts_lib::replay::{ChainWriter, RecordDraft, RecordKind, RecordOutcome};

const STREAM: &str = "cursor-tests";

/// One frame, with everything that does not bear on ordering held constant so
/// that the §6 key varies only by slot and sequence.
fn draft(slot: u64) -> RecordDraft {
    RecordDraft {
        event_id: format!("evt-{slot:04}"),
        slot,
        observed_at_ms: 1_700_000_000_000 + slot as i64,
        provider: "helius".to_string(),
        endpoint_index: 0,
        connection: 1,
        kind: RecordKind::Frame,
        frame: Some(b"{\"ok\":true}".to_vec()),
        outcome: RecordOutcome::Accepted,
        dispatch_latency_us: Some(900),
    }
}

/// Seals `slots` into one continuous chain and returns one JSONL line each.
///
/// Sealing the whole stream with a single writer is what makes the split below
/// a *rotation* rather than four separate chains: the sequence numbers and the
/// `prev_hash` links run straight through the cut.
fn sealed(slots: &[u64]) -> Vec<String> {
    let mut writer = ChainWriter::new(STREAM);
    slots
        .iter()
        .map(|&s| writer.seal(draft(s)).to_line())
        .collect()
}

/// The lines after `prefix`, when `prefix` then `rest` are sealed as one chain.
///
/// Two continuations built on the same prefix share its records exactly — the
/// drafts are deterministic — so both descend from the same cursor without
/// needing to reach inside `ChainWriter`.
fn continuation(prefix: &[u64], rest: &[u64]) -> String {
    let mut slots = prefix.to_vec();
    slots.extend_from_slice(rest);
    sealed(&slots)[prefix.len()..].join("\n")
}

/// Cuts sealed lines into segments at `at`, the way a roll does.
fn segments(lines: &[String], at: usize) -> (String, String) {
    (lines[..at].join("\n"), lines[at..].join("\n"))
}

/// Walks a rolled stream the way `Evaluator::ingest` does: one cursor, carried.
fn walk(texts: &[&str]) -> Vec<ChainAudit> {
    let mut cursor = ChainCursor::start(STREAM);
    let mut audits = Vec::new();
    for text in texts {
        let audit = audit_stream_from(STREAM, text, cursor);
        cursor = audit.cursor;
        audits.push(audit);
    }
    audits
}

/// Edits one sealed line in place, the way a forgery does: a field changed and
/// the hashes left alone, so the record no longer implies its own seal. The
/// timestamp is the field with no bearing on `seq`, the links or the §6 key, so
/// the only check it trips is self-integrity.
fn edit_in_place(line: &str) -> String {
    let edited = line.replacen("\"observed_at_ms\":17", "\"observed_at_ms\":19", 1);
    assert_ne!(edited, line, "the edit has to land");
    edited
}

/// The status of every line that parsed, in order.
fn statuses(audit: &ChainAudit) -> Vec<LineStatus> {
    audit.records.iter().map(|r| r.status).collect()
}

// ===========================================================================
// The roll itself
// ===========================================================================

#[test]
fn a_clean_roll_carries_the_chain_and_stays_quotable() {
    // §3.3. The baseline the rest of the file is measured against: a rotation
    // is not a break. If this fails, the cursor has started reporting honest
    // rotations as forgeries, which is the failure mode the carried break is
    // most likely to have introduced.
    let lines = sealed(&[10, 11, 12, 13, 14, 15]);
    let (first, second) = segments(&lines, 3);

    let audits = walk(&[&first, &second]);

    for audit in &audits {
        assert_eq!(audit.first_break, None);
        assert!(!audit.carried_break);
        assert_eq!(audit.rejected, 0);
        assert_eq!(audit.unverifiable, 0);
        assert_eq!(audit.verified, 3);
        assert!(audit.gate_ready().is_ok(), "{:?}", audit.gate_ready());
    }
    assert!(!audits[1].cursor.broken);
    // The head after the second segment is the head of the whole stream, which
    // is what the manifest pins.
    assert_eq!(audits[1].chain_head, audits[1].cursor.prev_hash);
}

#[test]
fn a_break_in_an_earlier_segment_is_not_laundered_by_the_roll() {
    // The regression this branch exists for. A field edited in place in
    // `000.jsonl` breaks that file at line two. `001.jsonl` is untouched and
    // perfect on its own — the resynchronisation after a break leaves the
    // cursor pointing at a chain that does link up.
    //
    // Before the fix every line of the second segment came back `Verified` and
    // `readable(gate)` handed all three to a gate run. The break has to survive
    // the roll or §3.3 is a laundry for exactly the splice `UnverifiableAfter\
    // Break` is documented to catch.
    let lines = sealed(&[10, 11, 12, 13, 14, 15]);
    let (first, second) = segments(&lines, 3);
    let first = first.replacen(&lines[1], &edit_in_place(&lines[1]), 1);

    let audits = walk(&[&first, &second]);

    // Segment one names the edited line and nothing else.
    assert_eq!(audits[0].first_break, Some(2));
    assert_eq!(
        statuses(&audits[0]),
        vec![
            LineStatus::Verified,
            LineStatus::SelfInconsistent,
            LineStatus::UnverifiableAfterBreak,
        ],
    );
    assert!(audits[0].cursor.broken);

    // Segment two is clean line by line and still may not be quoted.
    assert_eq!(audits[1].first_break, None, "nothing in this file is wrong");
    assert!(audits[1].carried_break);
    assert_eq!(
        statuses(&audits[1]),
        vec![LineStatus::UnverifiableAfterBreak; 3],
        "a clean segment downstream of a break is readable, never quotable",
    );
    assert_eq!(audits[1].verified, 0);
    assert_eq!(audits[1].unverifiable, 3);
    assert!(audits[1].gate_ready().is_err());
    assert!(
        audits[1].cursor.broken,
        "and it stays broken for segment three"
    );

    // The gate reads nothing; a debugging run still reads everything.
    assert_eq!(audits[1].readable(true).count(), 0);
    assert_eq!(audits[1].readable(false).count(), 3);
}

#[test]
fn the_break_survives_every_later_segment() {
    // Not just the next one. `broken` is a property of the stream, so a break
    // in the first of four files disqualifies the fourth.
    let lines = sealed(&[10, 11, 12, 13, 14, 15, 16, 17]);
    let mut lines = lines;
    lines[1] = edit_in_place(&lines[1].clone());

    let texts: Vec<String> = lines.chunks(2).map(|c| c.join("\n")).collect();
    let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
    let audits = walk(&refs);

    assert_eq!(audits.len(), 4);
    assert!(audits[0].cursor.broken);
    for audit in &audits[1..] {
        assert!(audit.carried_break);
        assert!(audit.gate_ready().is_err());
        assert_eq!(audit.verified, 0);
        assert_eq!(audit.readable(true).count(), 0);
    }
}

// ===========================================================================
// Discontinuous streams
// ===========================================================================

#[test]
fn a_corrupted_block_gap_at_the_roll_is_named_once() {
    // A record dropped at the end of `000.jsonl` — a truncated write, the
    // ordinary way a segment goes wrong. The cursor expects the sequence that
    // went missing, so the gap lands on the first line of the next file.
    //
    // "Once" is the property under test. Resynchronisation means one hole
    // produces one verdict, not one per line for the rest of the stream: a
    // report that says everything after line one is broken has not said which
    // record went missing.
    let lines = sealed(&[10, 11, 12, 13, 14, 15]);
    let truncated = lines[..2].join("\n");
    let second = lines[3..].join("\n");

    let audits = walk(&[&truncated, &second]);

    assert_eq!(
        audits[0].first_break, None,
        "a short file is not a broken one"
    );
    assert_eq!(
        audits[0].cursor.next_seq, 2,
        "the cursor expects the lost record"
    );

    let gap = &audits[1];
    assert_eq!(gap.first_break, Some(1));
    assert_eq!(gap.verdicts[0].status, LineStatus::SeqGap);
    assert_eq!(gap.verdicts[0].detail, "expected seq 2, found 3");
    assert_eq!(gap.rejected, 1, "the hole is one verdict, not three");
    assert_eq!(
        statuses(gap),
        vec![
            LineStatus::SeqGap,
            LineStatus::UnverifiableAfterBreak,
            LineStatus::UnverifiableAfterBreak,
        ],
    );
}

#[test]
fn a_gap_inside_a_segment_does_not_hide_behind_the_next_roll() {
    // The same hole, one file earlier, to check the two mechanisms compose:
    // the gap is named in the file it happens in, and the file after it is
    // still refused.
    let lines = sealed(&[10, 11, 12, 13, 14, 15]);
    let holed = format!("{}\n{}", lines[0], lines[2]);
    let second = lines[3..].join("\n");

    let audits = walk(&[&holed, &second]);

    assert_eq!(audits[0].first_break, Some(2));
    assert_eq!(audits[0].verdicts[0].status, LineStatus::SeqGap);
    assert_eq!(audits[0].verdicts[0].detail, "expected seq 1, found 2");
    assert!(audits[1].carried_break);
    assert!(audits[1].gate_ready().is_err());
}

// ===========================================================================
// §6 — monotonic slots across the roll
// ===========================================================================

#[test]
fn a_slot_that_goes_backwards_across_the_roll_is_caught() {
    // Monotonic slot validation is only as good as the key the cursor carries.
    // Here the chain is perfect — one writer sealed the lot, so sequences and
    // links run straight through — and the only thing wrong is that the first
    // record of `001.jsonl` claims a slot the previous file has already passed.
    //
    // Slot is the most significant field of the §6 key, so a regression always
    // drops the key below its predecessor. Without `previous_key` on the
    // cursor there is nothing in the second file to compare against and the
    // reorder reads as clean.
    let lines = sealed(&[10, 11, 12, 11, 13]);
    let (first, second) = segments(&lines, 3);

    let audits = walk(&[&first, &second]);

    assert_eq!(audits[0].first_break, None);
    assert_eq!(
        audits[0].cursor.previous_key.expect("carried").slot,
        12,
        "the roll hands on the slot to follow",
    );

    let rolled = &audits[1];
    assert_eq!(rolled.first_break, Some(1));
    assert_eq!(rolled.verdicts[0].status, LineStatus::OutOfOrder);
    assert!(
        rolled.verdicts[0].detail.contains("slot: 11")
            && rolled.verdicts[0].detail.contains("slot: 12"),
        "the verdict names both slots: {}",
        rolled.verdicts[0].detail,
    );
    assert!(rolled.cursor.broken);
}

#[test]
fn repeated_slots_are_ordinary_and_do_not_break_the_chain() {
    // Slots repeat: several frames arrive within one slot, and §6 breaks the
    // tie on provider, endpoint, connection and sequence. The rule is that the
    // key increases strictly, not that the slot does. Asserting the stronger
    // rule would reject every honest recording, so it is worth pinning that
    // this case stays clean across a roll too.
    let lines = sealed(&[10, 10, 10, 10, 11, 11]);
    let (first, second) = segments(&lines, 3);

    let audits = walk(&[&first, &second]);

    for audit in &audits {
        assert_eq!(audit.first_break, None);
        assert_eq!(audit.verified, 3);
    }
    assert!(!audits[1].cursor.broken);
    assert_eq!(audits[1].cursor.previous_key.expect("carried").slot, 11);
}

// ===========================================================================
// Reorg forks
// ===========================================================================

#[test]
fn a_reorg_fork_is_two_valid_continuations_that_the_head_tells_apart() {
    // A fork is the case no single audit can call. Both continuations descend
    // from the same cursor, both verify line by line, and neither is corrupt —
    // the chain does not know which branch the recording meant to keep.
    //
    // What distinguishes them is the head. The audit's job is to report a head
    // per branch and refuse to guess; the manifest pins one, and §3 checks the
    // stream against it. This test pins that contract: valid, valid, different.
    let common = sealed(&[10, 11, 12]);
    let cursor = {
        let audit = audit_stream_from(STREAM, &common.join("\n"), ChainCursor::start(STREAM));
        assert!(audit.gate_ready().is_ok());
        audit.cursor
    };

    // Two branches sealed from the same head, diverging at the first record —
    // the shape a reorg leaves behind.
    let kept = continuation(&[10, 11, 12], &[13, 14]);
    let orphaned = continuation(&[10, 11, 12], &[16, 17]);
    assert_ne!(kept, orphaned, "the branches have to actually differ");

    let kept = audit_stream_from(STREAM, &kept, cursor);
    let orphaned = audit_stream_from(STREAM, &orphaned, cursor);

    assert!(kept.gate_ready().is_ok(), "{:?}", kept.gate_ready());
    assert!(orphaned.gate_ready().is_ok(), "{:?}", orphaned.gate_ready());
    assert_eq!(kept.verified, 2);
    assert_eq!(orphaned.verified, 2);
    assert_ne!(
        kept.chain_head, orphaned.chain_head,
        "the fork is only visible in the head",
    );
    // Neither branch poisons the other: the cursor is per-branch state.
    assert!(!kept.cursor.broken);
    assert!(!orphaned.cursor.broken);
}

// ===========================================================================
// Telemetry
// ===========================================================================

#[test]
fn the_counters_account_for_every_line_in_integers() {
    // Zero float. Every number the audit reports is a count of lines, so the
    // four buckets and the blanks have to add up to the lines read exactly —
    // not approximately, and with no ratio or percentage anywhere that could
    // introduce a rounding difference between two machines.
    //
    // Run over a stream that exercises every bucket at once: a blank line, an
    // unparseable line, a real break, and the lines downstream of it.
    let lines = sealed(&[10, 11, 12, 13, 14, 15]);
    let first = format!(
        "{}\n\n{}\nnot json at all\n{}",
        lines[0], lines[1], lines[2]
    );
    let second = lines[3..].join("\n");

    let audits = walk(&[&first, &second]);

    for audit in &audits {
        let counted: usize =
            audit.verified + audit.unverifiable + audit.rejected + audit.blank_lines;
        assert_eq!(
            counted, audit.lines_read,
            "every line lands in exactly one bucket: {audit:?}",
        );
    }

    let first = &audits[0];
    assert_eq!(first.lines_read, 5);
    assert_eq!(first.blank_lines, 1);
    assert_eq!(first.rejected, 1, "the unparseable line");
    assert_eq!(first.first_break, Some(4));
    assert_eq!(first.verified, 2);
    assert_eq!(first.unverifiable, 1, "the line after the break");

    // The break carries, so the whole second segment is unverifiable and the
    // sum still holds there.
    let second = &audits[1];
    assert_eq!(second.verified, 0);
    assert_eq!(second.unverifiable, 3);
    assert_eq!(second.rejected, 0);
}

// ===========================================================================
// The whole report, not just the audit
// ===========================================================================

/// A pong seals into the chain exactly like a frame and carries no event, so
/// these exercise the chain and the gate without also exercising the decoder.
fn pong(slot: u64) -> RecordDraft {
    RecordDraft {
        kind: RecordKind::Pong,
        frame: None,
        ..draft(slot)
    }
}

fn sealed_pongs(slots: &[u64]) -> Vec<String> {
    let mut writer = ChainWriter::new(STREAM);
    slots
        .iter()
        .map(|&s| writer.seal(pong(s)).to_line())
        .collect()
}

/// Runs a rolled stream through the evaluator the way the CLI does.
fn report_for(files: &[(&str, &str)]) -> ForensicReport {
    let mut evaluator = Evaluator::new(BacktestConfig {
        gate: true,
        ..Default::default()
    });
    for (file, text) in files {
        evaluator.ingest(STREAM, file, text);
    }
    evaluator.finish("cursor-tests")
}

#[test]
fn an_honest_rotation_is_quotable_end_to_end() {
    // The baseline again, this time through `Evaluator` and `ForensicReport`
    // rather than one audit, because that is the path the gate actually takes.
    let lines = sealed_pongs(&[10, 11, 12, 13, 14, 15]);
    let (first, second) = segments(&lines, 3);

    let report = report_for(&[("000.jsonl", &first), ("001.jsonl", &second)]);

    assert!(report.gate_ready, "refusals: {:?}", report.refusals);
    assert!(report.refusals.is_empty());
    assert!(report.integrity.gate_ready);
    assert_eq!(report.integrity.streams_with_breaks, 0);
    assert!(report.streams.iter().all(|s| s.gate_ready));
}

#[test]
fn a_laundered_segment_is_refused_in_the_report() {
    // The end-to-end shape of the fix. Before it, `001.jsonl` came back
    // `gate_ready: true` with six verified records behind it, and a gate run
    // reading only the second segment would have quoted a spliced stream.
    let lines = sealed_pongs(&[10, 11, 12, 13, 14, 15]);
    let (first, second) = segments(&lines, 3);
    let first = first.replacen(&lines[1], &edit_in_place(&lines[1]), 1);

    let report = report_for(&[("000.jsonl", &first), ("001.jsonl", &second)]);

    assert!(!report.gate_ready);
    assert!(!report.integrity.gate_ready);

    let broken = &report.streams[0];
    assert_eq!(broken.first_break, Some(2));
    assert!(!broken.gate_ready);

    let laundered = &report.streams[1];
    assert_eq!(laundered.file, "001.jsonl");
    assert_eq!(
        laundered.first_break, None,
        "nothing in this file is wrong, which is the whole difficulty",
    );
    assert!(
        !laundered.gate_ready,
        "and it still may not back a gate dossier",
    );
    assert_eq!(laundered.verified, 0);
    assert_eq!(laundered.unverifiable, 3);
}

#[test]
fn the_break_is_named_once_however_many_segments_follow_it() {
    // The report says where the edit is, once, and marks every segment
    // downstream of it unquotable. It does not add a refusal per segment: the
    // file is explicit that a report nobody reads the failures in is worth
    // nothing, and ninety-eight lines saying "the segment before this one was
    // bad" would bury the one line saying which record was edited.
    let mut lines = sealed_pongs(&[10, 11, 12, 13, 14, 15, 16, 17]);
    lines[1] = edit_in_place(&lines[1].clone());
    let texts: Vec<String> = lines.chunks(2).map(|c| c.join("\n")).collect();
    let files: Vec<(String, &str)> = texts
        .iter()
        .enumerate()
        .map(|(i, t)| (format!("{i:03}.jsonl"), t.as_str()))
        .collect();
    let files: Vec<(&str, &str)> = files.iter().map(|(f, t)| (f.as_str(), *t)).collect();

    let report = report_for(&files);

    assert_eq!(report.streams.len(), 4);
    assert!(!report.gate_ready);
    assert!(
        report.streams.iter().all(|s| !s.gate_ready),
        "every segment of a broken stream is unquotable",
    );

    let named: Vec<&String> = report
        .refusals
        .iter()
        .filter(|r| r.contains("failed verification"))
        .collect();
    assert_eq!(named.len(), 1, "one edit, one refusal: {named:?}");
    assert!(named[0].contains("line 2"), "{}", named[0]);
    assert_eq!(
        report.integrity.streams_with_breaks, 1,
        "one segment is broken; the other three are downstream of it",
    );
}

#[test]
fn the_stream_report_schema_is_unchanged() {
    // `carried_break` lives on `ChainAudit` and `broken` on `ChainCursor`, and
    // neither type is serialised. The fix therefore changes what `gate_ready`
    // says and never the shape of the JSON, so every fixture golden and every
    // `VerifyReport` deserialiser still reads a report from this build.
    let lines = sealed_pongs(&[10, 11]);
    let report = report_for(&[("000.jsonl", &lines.join("\n"))]);

    let json = serde_json::to_value(&report.streams[0]).expect("serialises");
    let mut keys: Vec<&str> = json
        .as_object()
        .expect("an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();

    let mut expected = vec![
        "stream_id",
        "file",
        "lines",
        "blank_lines",
        "records",
        "verified",
        "unverifiable",
        "rejected",
        "first_break",
        "chain_head",
        "frames",
        "events_applied",
        "frames_dropped_live",
        "frames_backpressure_recovered",
        "gate_ready",
        "verdicts",
        "event_errors",
    ];
    expected.sort_unstable();
    assert_eq!(keys, expected, "StreamReport gained or lost a field");
}

#[test]
fn every_number_in_the_report_survives_a_json_round_trip_exactly() {
    // Zero float, checked the way it would actually bite: floats are what make
    // a round trip lossy and two machines disagree. Every counter here is an
    // integer, so the report that comes back is the report that went out --
    // and `ForensicReport` derives `Eq`, which a `f64` anywhere in the tree
    // would have made impossible to compile in the first place.
    let lines = sealed_pongs(&[10, 11, 12, 13, 14, 15]);
    let (first, second) = segments(&lines, 3);
    let first = first.replacen(&lines[1], &edit_in_place(&lines[1]), 1);
    let report = report_for(&[("000.jsonl", &first), ("001.jsonl", &second)]);

    let text = serde_json::to_string(&report).expect("serialises");
    let back: ForensicReport = serde_json::from_str(&text).expect("deserialises");

    assert_eq!(report, back, "the report did not survive its own JSON");
    assert!(
        !text.contains("e-"),
        "no exponent notation anywhere: floats"
    );
    assert!(
        !text.contains(".0,") && !text.contains(".0}"),
        "no trailing-zero decimals, which is what a serialised f64 looks like",
    );
}
