# STS deterministic replay and execution simulation

> **STATUS NOTE — 27 August 2026.** Three things in this document have been
> measured since it was written, and all three came back the other way. The
> mechanics below — the replay determinism, the curve arithmetic, the fill
> pricing, the grading discipline — are sound and are not affected. What is
> affected is every place the document names a *target regime* or quotes a
> *corpus statistic*. See [`../VERDICT-2026-08-27.md`](../VERDICT-2026-08-27.md).
>
> **And one constant is wrong, in the document and in the code.** This spec takes
> `φ`, the proportional fee on the SOL leg, as **100 bps**, and every worked example
> below follows from that — including the flat **199 bps** round-trip column in §14's
> table. Measured on 4,918 real sells, pump.fun charges **95 bps a side**: a round
> trip of **190 bps added, 189 compounded**, and a break-even of **+2.12% gross at
> 0.05 SOL** once the network leg is included. `DEFAULT_FEE_BPS` at
> `src-tauri/src/replay.rs:1784` is also 100. Neither has been changed here, because
> changing an engine default is a behaviour change with tests pinned to it and that is
> the owner's call — but **anything taking φ from this document is about 9 bps
> pessimistic per round trip**, and every fee-derived number below inherits that. The
> direction is conservative, which is why nobody noticed.
>
> - **The "$25k–$80k target window" is not a regime** (§11.1, §15.3, §20, and the
>   adverse-selection calibration anchored to it). Median time from launch to $25k
>   is **89 seconds**, so the document's claim that the band is "by construction
>   reached later" than 45–60 seconds is inverted — and only 3 of 53 coins are
>   still above $25k twelve hours later. This document's own table prices
>   graduation at $82,168, so the top of the band *is* graduation: the window is a
>   point, not a phase. Entering on the band crossing returns −8.43% net against
>   −11.40% for buying everything: no edge, still a loss, on n=86 with a wide
>   interval. Anything here calibrated to "the band that matters" is calibrated to
>   a moment.
> - **The corpus statistics are inflated by a decode defect.** On 18.4% of testable
>   coins `virtualSolReserves` is corrupt — the recorded price moves further than
>   the money that traded could have moved it. Nine of the ten largest peaks fail
>   that check. Every `peakMult` percentile quoted here, and any L3 outcome proxy
>   built on it, is high by an unknown amount. Verify a price against the curve
>   before trusting a multiple.
> - **There is no valid held-out day**, so §22's time-fold split does not do what
>   it says. The seven calendar files are **nine capture sessions, six usable**;
>   08-21 is the unbroken tail of the 08-20 run past midnight, so a calendar-day
>   boundary is not a boundary and a one-hour embargo does not create one.

This is the canonical description of three things: how a recorded feed is played
back so that two runs produce the same bytes, how a fill is priced against a
pump.fun bonding curve when other people are trading in the same slot, and how a
Sybil detector is graded against history without being graded against itself.

They live in one document because they are one claim. The roadmap's Phase 3 gate
asks for evidence that the engine behaves the same way twice and that its
economics survive contact with execution drag. Neither half is worth anything
alone: a replay that is deterministic but prices fills optimistically proves that
the engine is consistently wrong, and a simulator with honest cost models that
cannot be re-run proves nothing at all.

Everything below is either a formula with its exact arithmetic, a parameter with
its default and where the default lives, an invariant with the test that proves
it, or a measurement taken from the corpus already in `data/` and labelled as
such. Where a number is a policy choice it says so.

## A note on phase numbering

`RISK_AND_SYBIL_SPEC.md` opens with the same warning and it still applies.
`STS_ROADMAP.md` numbers Phase 2 "Dual-Speed Risk & Feature Pipeline" and Phase 3
"Deterministic Replay & Out-of-Sample Backtesting Engine". This document is
written against the roadmap's **Phase 3** acceptance criteria and its six exact
tests. The build sequence is tracking one number ahead of the roadmap — the risk
document's content is the roadmap's Phase 2 and is being called Phase 3 in the
build — so this work is build-sequence Phase 4 if that offset holds.

The two schemes need reconciling before a gate dossier is written. A gate record
citing the wrong phase is a gate record nobody can check.

## Conventions

Everything in `RISK_AND_SYBIL_SPEC.md`'s conventions section applies here
unchanged: lamports in `u64` with `u128` intermediates, ratios as integer basis
points or `f32` unit floats and never both, epoch milliseconds from one clock,
UNKNOWN as a value rather than a zero, and partial evidence that may raise risk
and may never lower it. Four more are specific to replay.

**Replay compares bytes, not numbers.** "The same result" means two files that
`diff` reports as identical, not two numbers a person judges to be close. A
tolerance is a place for a bug to live, and every tolerance in a replay
comparison eventually widens to cover the bug that was found last.

**The fixture is evidence and the code is the thing on trial.** A fixture is
never edited to make a test pass. When replay disagrees with what was recorded,
the disagreement is the finding. This is the difference between a regression
suite and a story about one.

**Nothing samples from a shared sequential generator.** Every random draw in the
simulator is addressed by name and index, not drawn from a stream (section 19).
A shared generator makes every draw depend on the order and the count of all
previous draws, so adding one log line silently changes every number downstream.

**A simulated number is labelled simulated everywhere it is stored.** There is
one `mode` vocabulary — `live`, `paper`, `replay` — it is already a `CHECK`
constraint on `execution_logs`, and any table that can hold a simulated row
carries it. A simulated fill that can be mistaken for a real one is worse than
no fill at all, because the mistake is only discovered when the two are averaged
together in a report.

---

# Part I — Deterministic replay

## 1. What "byte-identical" is a claim about

The roadmap's first Phase 3 criterion is that the same fixture, policy, model
and seed yield byte-identical decisions, fills, PnL, audit records and
projections across two runs. That is only checkable once the artefact is named,
so here it is.

A replay run produces exactly one directory:

```text
reports/<run_id>/
  run.json          the ordered decision and fill record — the comparison target
  audit.ndjson      the hash-chained audit log for the run
  metrics.json      host measurements: wall time, allocations, CPU. NOT compared
  manifest.json     fixture id, chain head, policy hash, model hash, seed, commit
```

`run.json` is compared byte for byte between runs. It contains, in stream order:
every decision envelope with its gate results and reasons, every simulated fill
with its price and cost decomposition, every position transition, the running
and final PnL, and the final integrity hash of `audit.ndjson`. Nothing in it is
a measurement of the host.

`metrics.json` is deliberately outside the comparison. It holds the numbers that
describe the machine rather than the engine — how long the run took, how much it
allocated, how hot the laptop got. Those are useful and they are not
reproducible, and mixing them into the artefact would make every run differ for
reasons that have nothing to do with the engine.

The line between the two is the rule: **if a field's value would change on a
faster laptop, it is not in `run.json`.** Section 7 lists every field in the
current code that fails that test and what happens to it.

## 2. The three clocks

There are three sources of time in this process and all three have to be
virtualised. Missing one is the usual reason a replay is nearly deterministic.

**The wall clock** is `telemetry::now_ms()`, which today calls
`SystemTime::now()` directly (`telemetry.rs:30`). Every `at_ms`, `observed_at_ms`
and `computed_at_ms` in the database descends from it, so it is the clock that
ends up compared.

**The timer clock** is `tokio::time` and `std::time::Instant`. Ingestion uses it
for reconnect backoff (`BACKOFF_MIN` to `BACKOFF_MAX`), the 15 s `HEARTBEAT`, the
45 s `IDLE_TIMEOUT`, the `WAL_LINGER` batch timer, endpoint selection
(`EndpointPool::pick_at`) and failure decay (`record_failure_at`). Several of
those already take an `Instant` argument rather than reading the clock
themselves, which is most of the seam already built.

**The slot clock** is the chain's, and it is the authoritative one. Solana slots
are the only ordering axis both providers agree on; block times from two
providers can disagree by hundreds of milliseconds, which
`RISK_AND_SYBIL_SPEC.md` §3.2 already relies on being true. In replay, wall time
is derived from slot, not the other way round.

The seam is one trait, and the important part is that there is exactly one:

```rust
/// The engine's only source of time.
pub trait Clock: Send + Sync {
    /// Epoch milliseconds. What every `*_at_ms` column is stamped with.
    fn now_ms(&self) -> i64;
    /// A monotonic instant for measuring durations.
    fn instant(&self) -> ClockInstant;
    /// The newest slot the engine has observed from any provider.
    fn slot(&self) -> u64;
}
```

`SystemClock` is the live implementation and behaves exactly as the code does
today. `ReplayClock` holds `(slot, at_ms)` and advances only when the playback
driver moves it, which happens once per fixture record, before that record is
delivered. Its `instant()` is the same virtual timeline, so a duration measured
across two records is the difference between their recorded timestamps rather
than the time the host took to process them.

Three rules make this work rather than merely compile:

**Nothing reads the clock twice for one event.** The dispatcher takes `now_ms()`
once when a frame arrives and passes it down. Two reads inside one event handler
can straddle a millisecond boundary in live mode, which makes a live-recorded
fixture internally inconsistent in a way replay then has to reproduce.

**Tokio's timers run paused.** A replay runs on
`#[tokio::main(flavor = "current_thread")]` with `tokio::time::pause()`, so
`sleep` returns when the runtime auto-advances rather than when the host's clock
does. This is the mechanism that makes a 45-second idle timeout testable in
microseconds and, more importantly, makes it fire at the same point in the
stream every time.

**Wall time never regresses.** The fixture is ordered by the key in section 6 and
`ReplayClock::now_ms` takes `max(current, record.observed_at_ms)`, so a provider
whose clock was behind cannot walk time backwards mid-run. The number of times
that clamp fires is recorded in the manifest, because a fixture where it fires
often was recorded against a provider with a broken clock.

## 3. The fixture format

One directory per recorded stream, under `data/replay/<stream_id>/`, containing
numbered JSONL segments and a manifest.

Each line of a segment is one record in the `sts.replay.v1` schema. The field set
is Annex P's mandatory envelope, narrowed to what a feed record actually carries:

| Field | Type | Meaning |
| --- | --- | --- |
| `schema` | string | `sts.replay.v1`, on every line |
| `event_id` | string | UUIDv7. Time-ordered, unique, the correlation anchor |
| `seq` | integer | Position in this stream, from 0, no gaps |
| `slot` | integer | The slot the frame reported, or the last known slot for a non-frame record |
| `observed_at_ms` | integer | When the socket handed the bytes over |
| `provider` | string | `helius`, `quicknode`, `triton` — matches `FeedProvider` |
| `endpoint_index` | integer | Which configured endpoint, for multi-endpoint providers |
| `connection` | integer | Which dial this record belongs to; increments on every reconnect |
| `kind` | string | `frame`, `pong`, `connected`, `closed`, `error`, `ack` |
| `frame_b64` | string or null | The exact bytes off the socket, base64. Null for non-frame kinds |
| `frame_len` | integer | Length in bytes before encoding |
| `frame_sha256` | string | Hex digest of the raw bytes |
| `outcome` | string | What the live run did with it: `accepted`, or `dropped:<DropReason>` |
| `dispatch_latency_us` | integer or null | What the live run measured, for `accepted` frames |
| `prev_hash` | string | The previous record's `integrity_hash` |
| `integrity_hash` | string | This record's chain link |

`frame_b64` holds the bytes as they arrived, not a parsed structure. The
pre-filter (`StreamFilters::admits_frame`), the base58 decode, the borrow-based
`serde_json` parse and the account decoder are all code under test, and a fixture
of parsed structures would replay none of them.

`outcome` is what makes a fixture more than a tape. The recorder writes down what
the live engine did with each frame, so replay can assert it did the same thing
rather than only that it did the same thing twice. Section 5 explains why the two
are different claims.

### 3.1 The hash chain

```text
canonical(r) = the record's fields, in the table's order, minus integrity_hash,
               serialised as compact JSON: no whitespace, keys in that fixed
               order, integers in decimal with no leading zeros or exponent,
               strings escaped with \u only where JSON requires it,
               base64 standard alphabet with padding.

integrity_hash(r) = hex( SHA-256( prev_hash_bytes || canonical(r) ) )
prev_hash(r_0)    = hex( SHA-256( stream_id ) )
```

The canonical form is spelled out because "canonical bytes" that are not
specified are two implementations that disagree on the day somebody writes a
verifier in another language. Serde's default map ordering is not a
specification.

Verification walks the chain from the genesis value and recomputes every link.
`sts replay verify --fixture <dir>` does exactly that and nothing else; it is the
cheapest possible check that a fixture has not been edited, and it runs before
every replay rather than on request.

### 3.2 The manifest

```json
{
  "schema": "sts.replay.manifest.v1",
  "stream_id": "phase3-2026-08-21",
  "created_at_ms": 1787270470002,
  "first_slot": 341002118, "last_slot": 341009944,
  "record_count": 184203, "frame_count": 171882,
  "segments": [{"file": "000.jsonl", "records": 100000, "sha256": "..."}],
  "chain_head": "9f3c...",
  "providers": ["helius", "quicknode"],
  "filters_version": 1, "exclusion_list_version": 7,
  "sts_version": "0.1.0", "git_commit": "53f339b",
  "coverage": [{"from_ms": ..., "to_ms": ..., "gap_reason": "disconnect"}],
  "complete": true
}
```

`coverage` is the field that stops a cohort being computed across a hole. Every
interval in which no socket was connected is listed with the reason, and any
statistic computed over a window that intersects a gap is labelled in the report
rather than quietly averaged. A backtest over a day with a two-hour outage that
does not say so is a survivorship claim wearing a sample size.

`complete: false` marks a recording that was stopped by an error, and a fixture
with `complete: false` may be replayed for debugging and may never be used in a
gate dossier.

### 3.3 Rotation

Segments roll at 64 MiB or at UTC midnight, whichever comes first, matching the
rotation discipline `AUDIT_EVENTS.md` describes for the audit log. The chain runs
across the roll — `prev_hash` of the first record in `001.jsonl` is the
`integrity_hash` of the last record in `000.jsonl` — so segmentation is a storage
detail and not a boundary in the evidence.

## 4. The recorder

The tap is inside `run_endpoint`, at the instant `FeedStream::recv` yields, and
before `StreamFilters::admits_frame` is called. That placement is the whole
point: the pre-filter rejects the large majority of frames for the cost of a
substring search, and it is the first piece of code a replay needs to exercise.

Recording must not slow the socket down, so it follows the same discipline as
everything else on that path — a bounded channel to a dedicated writer thread,
`try_send` from the socket task, never a blocking write. But the overflow policy
is the opposite of everywhere else in the engine, and the asymmetry is
deliberate:

**A dropped candidate is counted. A dropped fixture record ends the recording.**

Ingestion drops candidates under backpressure because a stalled socket is worse
than a lost candidate, and `IngestionSnapshot` counts the drops so the loss is
visible. A fixture cannot be treated that way. A stream with a hole in it is not
a shorter stream, it is a stream that replays into a state the live engine was
never in, and every conclusion drawn from it is unsound in a way no counter
communicates. So the recorder's channel is deep (`RECORD_DEPTH`, default 65 536),
and if it fills, the recording stops, the manifest is written with
`complete: false`, and a critical telemetry event fires.

The recorder writes `outcome` and `dispatch_latency_us` after the dispatcher has
finished with the frame, which means the record is completed out of order
relative to arrival. It is buffered by `seq` and flushed in order, so the file on
disk is always in stream order and the chain can be computed as it is written.

## 5. Playback

The seam already exists. `IngestionManager::start` takes an `Arc<dyn FeedDialer>`
and does not care what is behind it, and `MockDialer` (`ingestion.rs:3292`) is
already a scripted implementation with a `VecDeque` of frames per dial. Playback
is that type generalised to read from a fixture, and it needs no change to the
ingestion path.

```rust
/// Hands out one scripted socket per (provider, endpoint_index, connection)
/// tuple in the fixture, in the order the live run dialled them.
pub struct FixtureDialer {
    cursor: Arc<Mutex<ReplayCursor>>,
    clock: Arc<ReplayClock>,
}
```

Four things it has to reproduce that a naive frame-replayer does not:

**Connections.** The fixture is partitioned by `connection`, so the *n*th `dial`
gets the *n*th connection's records and then ends. A live run that reconnected
four times replays as four dials, which exercises the backoff, the resubscribe
and the `LaunchIndex` behaviour across a reconnect — the paths most likely to be
wrong and least likely to be covered otherwise.

**Pongs at their recorded latency.** `EndpointPool`'s health bands are computed
from ping round trips and the subscribe acknowledgement (`HEALTHY_P50_MS` 120,
`HEALTHY_P95_MS` 350, `DEGRADED_P95_MS` 500). Replaying frames without pongs
leaves every endpoint in whatever state it starts in, and the health band feeds
the gate. The `pong` records carry their measured round trip and `FixtureSink`
answers each ping by advancing the virtual clock by exactly that.

**Subscription acknowledgements.** The `ack` records replay too, so the
`awaiting_ack` timing that produces the first latency sample lands in the same
place.

**Endpoint selection state.** `EndpointPool::pick` is smooth weighted round
robin, which is stateful across calls. Its state is seeded from the manifest and
its output sequence is part of `run.json`, because which endpoint a one-shot RPC
went to changes what was seen.

### 5.1 The delivery discipline, and the drop problem

This is the subtle part, and getting it wrong is the usual reason a replay is
deterministic but wrong.

The live path drops on backpressure. Every channel out of ingestion is bounded
and every send is a `try_send`, so whether a given candidate is dropped depends
on whether the consumer had drained the queue yet — which depends on thread
scheduling, which depends on the machine. Replaying the same frames on a
different machine can produce a different set of drops, and therefore different
decisions.

There are two things a replay can mean, and they need separating:

**Self-consistency** — two replay runs of one fixture agree. The playback driver
delivers record *n+1* only after record *n* has been fully consumed, so the
queues are never in a racing state and drops become a deterministic function of
the consumer's behaviour. This is what the roadmap's byte-identical criterion
measures, and it is the mode every gate run uses.

**Fidelity** — the replay does what the live run did. This is what `outcome` is
for. Every record carries the live run's verdict, replay computes its own, and
any disagreement is reported per record with its `event_id`. Serialised delivery
means replay will drop *fewer* frames than live did, so the expected disagreement
is a set of frames live dropped for backpressure and replay accepted.

The rule: **fidelity disagreements on backpressure drops are reported and
tolerated; disagreements on any other `DropReason` fail the run.** A frame live
rejected as `NotAllowlisted` and replay accepted is a filtering bug. A frame live
dropped because the fast-path queue was full and replay accepted is the
serialisation working as designed. `DropReason` already distinguishes them, and
the report counts each category separately so a filtering bug cannot hide inside
a backpressure total.

## 6. The total order

```text
order_key = (slot, provider_rank, endpoint_index, connection, seq)
```

`provider_rank` is the index in `FeedProvider::ALL`, which is a fixed array in
the source, not a hash-map iteration or a configuration order. Every component
is an integer and `seq` is unique within a stream, so no two records can tie on
the whole key.

Ordering by slot first and arrival second is the deliberate choice. Arrival order
is a property of the network on the day of recording; slot order is a property of
the chain. Two providers reporting the same slot are ordered by provider rank, so
the same two frames land in the same order however the sockets behaved.

## 7. Every known source of variation in the current code

This table is the working list. Each row is a thing that will make two runs
differ today, where it is, and what happens to it.

| Source | Where | Rule |
| --- | --- | --- |
| `SystemTime::now()` | `telemetry::now_ms` (`telemetry.rs:30`) | Behind `Clock`; `ReplayClock` in replay |
| `Instant::now()` | `pick_at`, `record_failure_at`, `status_at`, heartbeat, ack timing | Behind `Clock::instant`; virtual timeline |
| `dispatch_latency_us` | `Dispatcher`, stored per row | Synthesised from the fixture's recorded value, never measured (7.1) |
| Bounded-channel drops | `fast_tx`, `standard_tx`, `wal_tx` | Serialised delivery (5.1); live verdict in `outcome` |
| Task interleaving | one `tokio::spawn` per endpoint | `current_thread` runtime, paused time |
| `Relaxed` atomics | `IngestionMetrics` | Single-threaded replay makes ordering moot; counters read only at quiesce |
| `HashMap` iteration | `TelemetryHub::fan_out` subscribers | Not decision-affecting; telemetry is not in `run.json` |
| `HashMap` iteration | `LaunchIndex::seen` | Never iterated — `get`/`insert`/`len` only. Ordering comes from the `VecDeque` |
| Smooth WRR state | `EndpointPool::pick` | Seeded from the manifest; the pick sequence is part of `run.json` |
| `AUTOINCREMENT` rowids | `ingest_candidates.id` | Excluded from comparison; compare on `(source, account, slot)` |
| Float last-bit drift | forensic scores | `RISK_AND_SYBIL_SPEC.md` §7.2 — `f64` compute, round to 4 dp, store `f32` |
| Wall-clock test polling | `until()` in the ingestion tests | Paused time; poll on the virtual clock |
| SOL price | `set_sol_price`, an atomic set out of band | Price changes are fixture records; replay never reads a live price |

That last row is easy to miss and changes everything downstream. `market_cap_usd_cents`
is computed from whatever `SolPrice` held at the moment the frame was dispatched,
and it is what the `$25k–$80k` target window is tested against. If the price is
set from a live source during a replay, the routing decision differs from the
recorded one for reasons that have nothing to do with the code under test. So
price updates are recorded as fixture records of their own and replayed in the
stream, and `SolPrice::UNKNOWN` at the start of a fixture stays unknown until the
first recorded update — which makes every candidate look too small to trade,
which is the safe direction and is already how the live default behaves.

### 7.1 Dispatch latency, specifically

`CandidateEvent::dispatch_latency_us` is `received.elapsed()` in microseconds
(`ingestion.rs:2020–2026`) and it is written into every `ingest_candidates` row.
It is a measurement of the host, and it is inside the artefact.

Two ways out, and only one of them is safe. Excluding the column from the
comparison works and is wrong: a column excluded from a hash is a column where a
bug can live forever. So instead the replay dispatcher takes the latency from the
fixture record and stamps that, and the host's actual elapsed time is accumulated
into `metrics.json` where it belongs. The row is byte-identical because it
carries the number the live run measured, and the fact that the replay was faster
is recorded somewhere it cannot be mistaken for evidence about the engine.

The same rule covers `DISPATCH_BUDGET` overruns: `IngestionSnapshot::over_budget`
in replay counts the fixture's recorded overruns, not the replay's own.

## 8. Where a replay is allowed to write

**A replay never opens the live database.** `STS_HOME` is repointed at the run
directory, so `sts.db` for a replay is a fresh file inside `reports/<run_id>/`.

This is not tidiness. `ingest_candidates` carries
`UNIQUE (source, account, slot)` and every insert is `INSERT OR IGNORE`
(`db.rs:244`), which is exactly right for live dedup across three providers
watching the same program — and it means **replaying a fixture into the database
that fixture was recorded from writes nothing at all.** Every row is already
there, every insert is ignored, the run reports success, and two runs agree
perfectly because neither did anything. A replay that proves the engine works by
doing nothing is the most expensive kind of green test.

The fresh-file rule also removes the `AUTOINCREMENT` problem from section 7:
rowids in a fresh file are a function of insert order alone, so they are
reproducible even though they are still excluded from the comparison on
principle.

## 9. Leakage, and how it is prevented structurally

The roadmap's second criterion is that replay never reads post-decision
information, and that leakage, survivorship, selection bias and time-split
violations fail the run. Comments do not enforce that. Types do.

**Forward-only reads.** `ReplayCursor` yields records in order and has no seek,
no index, and no random access. A decision made at slot *s* can only have seen
records the cursor has already yielded, because there is no method that returns
anything else.

**Outcomes are unnameable from the decision path.** The corpus's `outcome`
block — `peakMult`, `endMult`, `peakAtSec` — must live behind a type the decision
path cannot name. The enforcement is the dependency graph rather than discipline:
outcomes belong in a `sts-grade` crate that a `sts-decide` crate does not list in
its `Cargo.toml`, so a leak fails to compile rather than failing to be noticed.
`src-tauri` is one package today, so this is a split that has to happen before
the claim can be made; until it does, the weaker form is a named regression test
asserting the decision module never references the outcome module, in the same
spirit as `RISK_AND_SYBIL_SPEC.md`'s P5. A test is worth less than a compile
error here, and the difference is worth paying for.

**Future slots are unreachable.** The simulator prices a fill for a decision at
slot *s* from records at slot ≥ *s* only, and the fill is produced by the same
forward cursor rather than by a lookup into the whole stream.

**Survivorship is declared, not assumed away.** The corpus contains the launches
the collector was connected for. The manifest's `coverage` intervals say when it
was not, and every cohort statistic reports the coverage of its window. A
launch that rugged during an outage is absent from the corpus, and absence is
indistinguishable from non-existence unless the gap is written down.

**Time splits are checked, not intended.** The walk-forward harness asserts that
`max(train.observed_at_ms) + embargo <= min(test.observed_at_ms)` for every fold
and fails the run if it does not hold. Section 22 covers why a time split alone
is not enough for this particular corpus.

## 10. The equivalence check

```bash
sts replay run --stream data/replay/phase3 --seed 0x100x --out reports/a
sts replay run --stream data/replay/phase3 --seed 0x100x --out reports/b
diff -u reports/a/run.json reports/b/run.json
sha256sum reports/a/run.json reports/b/run.json
```

Both must pass. `diff` is what a person reads when it fails; the digest is what
CI compares.

Two further properties are worth asserting separately because they catch
different bugs:

**Decisions do not consume randomness.** Running the same fixture with two
different seeds must produce identical *decisions* and different *fills*. If the
decisions differ, something in the gate path is sampling, and a gate that samples
is a gate that cannot be explained. This is a stronger claim than determinism and
it is cheap to test.

**Segmenting does not change the result.** Replaying a fixture split into ten
segments must equal replaying it as one. If it does not, something is holding
state across a file boundary that it should not.

---

# Part II — The execution simulator

## 11. The curve, exactly

pump.fun's bonding curve is a constant-product market maker over *virtual*
reserves. `BondingCurve` (`ingestion.rs:1063`) already decodes the five numbers
that describe it:

| Field | Symbol | What it bounds |
| --- | --- | --- |
| `virtual_token_reserves` | `x` | Price and impact |
| `virtual_sol_reserves` | `y` | Price and impact |
| `real_token_reserves` | `x_r` | How many tokens can still be bought |
| `real_sol_reserves` | `y_r` | How much SOL can actually be paid out |
| `token_total_supply` | `S` | Market cap |
| `complete` | — | Whether the curve is still the venue at all |

The distinction between virtual and real is the one that matters and the one a
generic AMM model gets wrong. **Virtual reserves set the price. Real reserves set
what is executable.** `CandidateView::pool_lamports` is the real SOL — the
comment on it already says "what could actually be sold into" — and it is the
number the 1.5% participation cap is taken against. Market cap is computed from
the virtual pair, which is what `market_cap_lamports` does.

Trading moves both pairs by the same amounts, so:

```text
x = x0 - tokens_sold          y = y0 + sol_in
x_r = x_r0 - tokens_sold      y_r = sol_in
```

with `x0 = 1 073 000 000 × 10^6` base units, `y0 = 30 × 10^9` lamports and
`x_r0 = 793 100 000 × 10^6` base units at launch. Those are protocol parameters
of the same kind as `PUMP_GRADUATION_LAMPORTS` — they have changed before and can
change again — so they belong in configuration with a version, not in the
arithmetic.

**The constant product is preserved exactly**, because the fee is taken outside
the curve: the protocol fee is deducted from the SOL leg and sent to a fee
account, and only the remainder enters the reserve. So `k = x × y` is invariant
across every swap, and the algebra below is exact rather than approximate.

That gives a check worth running once, because it validates the whole model
against a constant the codebase already holds. At graduation the curve has sold
its entire real token reserve:

```text
x = x0 - x_r0 = (1 073 000 000 - 793 100 000) × 10^6 = 279 900 000 × 10^6
y = k / x     = (1 073 000 000 × 30) / 279 900 000  = 115.0 SOL
y_r = y - y0  = 85.0 SOL   =   PUMP_GRADUATION_LAMPORTS
```

The model reproduces the graduation constant to three significant figures from
the launch reserves alone. If a future change to any of those four parameters
stops that identity holding, the model and the constant have drifted apart and
one of them is wrong.

### 11.1 Market cap and the target band

```text
MC_lamports = S × y / x = S × y² / k
```

which is what `market_cap_lamports` computes. Inverting it maps the strategy's
`$25k–$80k` target window onto positions on the curve. At SOL = $200:

| `y_r` (real SOL) | `y` (virtual) | `x` (base units) | MC (SOL) | MC (USD) |
| --- | --- | --- | --- | --- |
| 0 | 30 | 1 073 000 000 × 10⁶ | 28.0 | $5 592 |
| 10 | 40 | 804 750 000 × 10⁶ | 49.7 | $9 941 |
| 33 | 63 | 510 952 380 × 10⁶ | 123.3 | $24 660 |
| 45 | 75 | 429 200 000 × 10⁶ | 174.7 | $34 949 |
| 60 | 90 | 357 666 666 × 10⁶ | 251.6 | $50 326 |
| 70 | 100 | 321 900 000 × 10⁶ | 310.7 | $62 131 |
| 85 | 115 | 279 913 043 × 10⁶ | 410.8 | $82 168 |

**STS's target regime is the upper half of the bonding curve.** The window opens
at roughly 33 SOL of real reserves and closes at graduation. Everything the
simulator models about depth, displacement and sandwiching should be calibrated
in that band, and a stress test run at launch reserves is testing a regime the
strategy explicitly excludes — `SpamFloor::DEFAULT` already rejects the first ten
slots and anything under $15k.

The USD column moves with the SOL price and the band does not. A doubling in SOL
shifts the whole target window down the curve, which changes the executable
depth behind every position. The simulator takes the SOL price from fixture
records (section 7) for exactly this reason.

## 12. Slippage

### 12.1 The two swaps

Let `φ` be the total proportional fee on the SOL leg, default 100 bps.
Configuration, versioned, and split into `φ_protocol + φ_creator` because
pump.fun has charged both.

**Buy**, paying `g` lamports gross:

```text
d  = g - floor(g × φ)                     SOL entering the curve
dx = floor(x × d / (y + d))               tokens out
x' = x - dx        y' = y + d
```

**Sell**, offering `dx` tokens:

```text
gross = floor(y × dx / (x + dx))
net   = gross - floor(gross × φ)          lamports out
x' = x + dx        y' = y - gross
```

### 12.2 One slippage formula

Slippage is the shortfall against the mid price `P = y / x`. Both directions
collapse to the same expression:

```text
S = (w + φ) / (1 + w)

with  w = (1 - φ) × g / y     for a buy
      w = dx / x              for a sell
```

Two things are worth reading off it. At small `w` it is `≈ φ + w(1 - φ)`, so
slippage is the fee plus the impact and the two are additive to first order. And
it is bounded above by 1 for any size, which is the correct behaviour — an
infinitely large order gets asymptotically nothing, it does not get a negative
price.

The derivation for the buy side, since it is where the `(1 - φ)` sits
asymmetrically:

```text
tokens at mid   = g × x / y
tokens received = x × d / (y + d),  d = (1-φ)g
S = 1 - received/at_mid = 1 - (1-φ)y/(y+d) = (w + φ)/(1 + w)
```

### 12.3 The integer implementation

Same discipline as `hhi_bps`: widen to `u128` before multiplying, multiply before
dividing, and pick the rounding direction deliberately.

```rust
/// Slippage in basis points for a swap of relative size `w`, expressed as
/// the pair (w_num, w_den) so no float enters the arithmetic.
///
/// Rounds **up**. The risk document rounds concentration to nearest because
/// truncation biases toward "looks safe"; the same reasoning here points a
/// different way. A simulator that under-reports its own slippage flatters
/// every backtest that uses it, so the residual goes to the trader's cost.
fn slippage_bps(w_num: u128, w_den: u128, phi_bps: u32) -> u16 {
    // S = (w + φ)/(1 + w) with w = w_num/w_den
    //   = (w_num × 10_000 + φ_bps × w_den) / (w_den + w_num)
    let num = w_num * 10_000 + phi_bps as u128 * w_den;
    let den = w_den + w_num;
    let bps = num.div_ceil(den);
    bps.min(10_000) as u16
}
```

Every quantity the simulator produces is an integer of lamports or base units.
There is no floating point anywhere in a fill price, because a fill price is
compared byte for byte between two runs and `f64` last-bit drift is exactly the
thing section 1 forbids.

### 12.4 Test vectors

Exact, and any implementation must reproduce them. Launch state,
`x = 1 073 000 000 × 10⁶`, `y = 30 × 10⁹`, `φ = 100 bps`:

| Buy (SOL) | Gross (lamports) | Tokens out (base units) | Slippage |
| --- | --- | --- | --- |
| 0.01 | 10 000 000 | 353 973 188 847 | 104 bps |
| 0.10 | 100 000 000 | 3 529 253 463 570 | 133 bps |
| 0.50 | 500 000 000 | 17 417 117 560 255 | 261 bps |
| 1.00 | 1 000 000 000 | 34 277 831 558 567 | 417 bps |
| 3.00 | 3 000 000 000 | 96 657 870 791 628 | 992 bps |
| 10.00 | 10 000 000 000 | 266 233 082 706 766 | 2 557 bps |

Sells out of the `y_r = 45` state (`x = 429 200 000 × 10⁶`, `y = 75 × 10⁹`),
sized by the SOL they are meant to realise:

| Target out | Share of `y_r` | Tokens in (base units) | Slippage |
| --- | --- | --- | --- |
| 0.225 SOL | 0.5% | 1 304 559 268 947 | 130 bps |
| 0.675 SOL | 1.5% | 3 937 614 674 131 | 190 bps |
| 1.350 SOL | 3.0% | 7 948 148 144 371 | 280 bps |
| 2.250 SOL | 5.0% | 13 412 499 995 574 | 400 bps |
| 4.500 SOL | 10.0% | 27 690 322 577 698 | 700 bps |

The token column is the smallest input whose net output reaches the target, found
by bisection, which is what an exit sizer actually has to do — the curve is
invertible in closed form only before the fee floor is applied.

### 12.5 The round trip, and what it means for the stress buckets

Buying and immediately selling back the same tokens, with nobody else trading in
between, at each point on the curve:

| `y_r` | Position at 1.5% cap | Buy | Sell | **Round trip** |
| --- | --- | --- | --- | --- |
| 10 SOL | 0.150 SOL | 137 bps | 137 bps | **199 bps** |
| 33 SOL | 0.495 SOL | 177 bps | 177 bps | **199 bps** |
| 45 SOL | 0.675 SOL | 188 bps | 188 bps | **199 bps** |
| 60 SOL | 0.900 SOL | 198 bps | 198 bps | **199 bps** |
| 80 SOL | 1.200 SOL | 206 bps | 206 bps | **199 bps** |

The table stops at 80 SOL for a reason worth stating, because it is the model
being right rather than running out. At 84 SOL of real reserves a position sized
at 1.5% of them needs more tokens than the curve still holds — the last SOL of a
curve can only buy the tokens still in it — and at 85 SOL the curve is complete
and §17's hard branch refuses to quote it at all. 83 SOL is the last whole SOL
where a cap-sized round trip fits, and it costs the same 199 bps.

The round trip costs `2φ` — 199 basis points, identically, at every point on the
curve where it is executable. This is not a coincidence and it is not an artefact of the sizes chosen.
A constant-product curve with no intervening flow returns to the same
`k`, so a buy followed by its own sell recovers the impact exactly and what is
left is the two fees. The per-leg slippage numbers in the middle columns are
real, they are what each leg pays against the mid at the moment it executes, and
they cancel.

The consequence is the most important thing in Part II, so it gets stated
flatly:

**The roadmap's 10%, 15%, 20% and 25% slippage stress buckets cannot be produced
by curve impact at the participation cap.** At 1.5% of real reserves the curve
charges about 1.9% for a round trip, and reaching 10% would take a position
around forty times the cap. Those buckets are *displacement* — the price moving
between the decision and the fill, because of other people's flow, a gap, or a
delay. A simulator that implements them by multiplying the impact function is
not stressing anything; it is asking what happens if the engine breaks its own
size limit by a factor of forty, which the size limit already prevents.

So the stress buckets are implemented as a displacement `δ` applied to the
reserves between decision and fill, and section 14 gives the arithmetic. The
impact function stays exactly as measured.

## 13. The participation cap

Doctrine's hard rule is that a position is at most 1.5% of executable liquidity,
and that this is a participation cap rather than a promise about impact.
`LiquidityThresholds::max_position_lamports` already computes it in `u128` and
saturates.

For a pump.fun curve, **executable liquidity is `real_sol_reserves`**. Not market
cap, which is a virtual-reserve quantity and is roughly five times larger. Not
`virtual_sol_reserves`, which includes 30 SOL that does not exist. The
distinction is worth a sentence because using market cap would make every
position roughly five times too big while appearing to respect the same rule.

There is a live disagreement about the number itself.
`RISK_AND_SYBIL_SPEC.md` §10 states `max_pool_share_bps` defaults to 150,
matching doctrine's 1.5%. `StreamFilters::DEFAULT` in `ingestion.rs` sets it to
**500**. Those are the same field on the same type with different values, and
the ingest-side value is the one a candidate is currently measured against. The
simulator uses 150, because that is the documented policy, and the discrepancy is
listed in section 30 as something to resolve rather than something to average.

## 14. Displacement under slot load

### 14.1 Why a slot is the unit

Every pump.fun swap writes the same bonding curve account, so every swap on one
token takes the same write lock and they serialise within a block. That gives a
structural ceiling on how many trades can land per slot per token:

```text
N_max ≈ CU_account_limit / CU_swap
```

Both are parameters, not laws. `CU_account_limit` is the per-account
compute-unit cap a validator applies within a block, default 12 000 000.
`CU_swap` is what one pump.fun swap actually costs, default 70 000, and it must
be **measured from landed transactions rather than assumed** — it varies with
account creation, the token program in use, and the program version. At the
defaults, `N_max ≈ 171` swaps per slot per curve.

That ceiling is worth sanity-checking against what the corpus in `data/` actually
recorded. Across 76 118 one-second candles:

| Per second | p50 | p90 | p99 | p99.9 | max |
| --- | --- | --- | --- | --- | --- |
| Trades | 2 | 9 | 28 | 92 | **347** |
| Volume (SOL) | 0.39 | 6.21 | 22.61 | 85.01 | **308.4** |

At roughly 2.5 slots per second the busiest second observed is about 139 trades
per slot, or 81% of the modelled ceiling. That agreement is a check that the
parameter is roughly right; it is not a measurement of the ceiling, and the
ceiling should be re-derived when `CU_swap` is measured properly.

The busiest candle in the corpus — 181 buys and 166 sells with 261 SOL of volume
— is at second 0 of a launch. That is the bot lottery `SpamFloor::DEFAULT`
already excludes with `min_slots_since_launch = 10`. The load regime the strategy
actually trades in is much quieter, and the tail of that distribution is what
matters, not its maximum.

### 14.2 The displacement model

Let our order be decided against reserves `(x, y)` and land after `j` other
transactions have executed against the same curve. Let `Δ` be their net
fee-adjusted SOL flow — buys positive, sells negative — and `δ = Δ / y`.

Our order executes against `(x/(1+δ), y(1+δ))`, and the tokens we receive are:

```text
received(δ) = x × w / [ (1 + δ)(1 + δ + w) ]
received(0) = x × w / (1 + w)
```

so the **displacement damage** is:

```text
D(δ, w) = 1 - (1 + w) / [ (1 + δ)(1 + δ + w) ]
```

For small `δ` this is `δ(2 + w)/(1 + w)`, which sits between `δ` and `2δ` for
every `w > 0`. That bound is the useful form: **being displaced by a fraction
`δ` of the SOL reserve costs between one and two times `δ`**, closer to two for
positions small relative to the pool, which is the regime the participation cap
puts us in.

`δ` is not modelled as a Gaussian. It is drawn from the empirical per-slot net
flow distribution conditioned on the load regime, taken from the corpus and
labelled by lifecycle phase and liquidity bucket, in the shape Annex B.2 already
requires for gap buckets. A Gaussian would understate exactly the tail the stress
buckets exist to probe: the p99.9 second in this corpus turns over 85 SOL of
gross volume against virtual reserves of the same order. Gross turnover is not
net flow and does not give `δ` directly, but it bounds it, and a bound of that
size one second in a thousand is not a tail any Gaussian fitted to the body will
reproduce.

### 14.3 Our position in the queue

Under a private Jito bundle our position is whatever the bundle specifies; under
a public path it is a draw from the priority-fee ordering. Both are modelled by
`j`, and the difference is which distribution `j` comes from.

The signed direction of `Δ` matters as much as its size. Predecessors that are
buys displace an entry against us and an exit in our favour; predecessors that
are sells do the reverse. The simulator draws the signed net, not a magnitude,
because using `|Δ|` would model every queue as adversarial and turn a
symmetrical cost into a systematic one.

### 14.4 The backtest's own bias, quantified

There is a bias here that no replay can remove and that should therefore be
measured and stated rather than hoped about. **The recorded flow did not see our
order.** The wallets in the fixture traded against the reserves as they actually
were, not as they would have been had we been in the pool. So the simulator
applies our impact to our own fill and does not alter any subsequent recorded
flow, which assumes our trades move nobody.

The size of that assumption is bounded by our own footprint, and at the
participation cap that footprint is the round trip from section 12.5: about 199
basis points, of which the impact half is roughly 90. So the claim is: **this
simulator's counterfactual error is bounded by approximately 90 bps per position
at the 1.5% cap**, growing linearly with participation. Reported in every
backtest, in the report, next to the expectancy it qualifies.

## 15. Sandwich extraction

### 15.1 The setup

An attacker front-runs our buy of `b` gross lamports with a buy of `a`, then
sells everything they bought, all within one slot. Using the fee-adjusted
reserve-relative sizes:

```text
α = (1 - φ) a / y        β = (1 - φ) b / y
```

Because `k` is preserved exactly (section 11), the whole sandwich has a closed
form. The attacker's gross SOL out is:

```text
G(α, β) = y × α (1 + α + β)² / [ (1 + α)² + αβ ]
```

and their gross extraction — what they get back beyond what they put in — is:

```text
E(α, β) = y × αβ (2 + α + β) / [ (1 + α)² + αβ ]
```

Both are exact, both were checked against a direct integer simulation of the
three swaps, and both agree to the lamport. The attacker's net profit after their
own two fees and a fixed landing cost `c` is:

```text
π(α, β) = (1 - φ) G(α, β) - αy/(1 - φ) - c
```

Our damage — the tokens we lose relative to executing alone — is section 14.2's
formula with `δ = α`:

```text
D(α, β) = 1 - (1 + β) / [ (1 + α)(1 + α + β) ]
```

which is the point of writing both models in one document. **A sandwich is queue
displacement whose predecessor is adversarial and which is followed by a matching
successor.** There is one damage function; the sandwich case just also has to
price the attacker's side to know whether the displacement will happen at all.

### 15.2 Two results worth naming

The closed form is exact in rationals and the three-swap simulation floors at
four separate divisions, so an implementation of both agrees to within a few
lamports rather than exactly — always in the attacker's disfavour, which is the
direction that cannot flatter a backtest.

**Extraction is strictly bounded by the victim's spend.** `E(α, β) < βy = b'` for
all `α, β > 0`, since

```text
E / (βy) = (2α + α² + αβ) / (1 + 2α + α² + αβ) < 1
```

by inspection. No sandwich can take more than the victim put in, however much
capital the attacker brings. The bound is tight only in the limit of infinite
attacker size, which is where the second result bites.

**A sandwich only clears fees above a size threshold.** The derivative of profit
at `α = 0` is `(1-φ)(1+β)² - 1/(1-φ)`, which is positive exactly when:

```text
β > φ / (1 - φ)          equivalently     b > φ y / (1 - φ)²
```

Strictly below that, no front-run of any size is profitable even before landing
costs. This was verified by an exhaustive search over attacker sizes at three
points on the curve; the search finds no profitable `α` below the threshold and
finds one immediately above it.

Two boundary conditions come with implementing this on integers rather than
reals. *At* the threshold the profit derivative is exactly zero, so the true edge
is zero and the last lamport is decided by rounding — there is no sign there to
assert. And below roughly ten thousand lamports the floors in the three swaps are
worth more than the edge, so a search that includes dust sizes returns one-lamport
"profits" that are arithmetic residue. Ten thousand lamports is two signatures at
the network's base fee, which is the smallest front-run that could pay for its own
transactions, so the search floor and the economics agree.

| Curve position | `y` | Minimum victim buy |
| --- | --- | --- |
| Launch | 30 SOL | **0.3061 SOL** |
| `y_r = 45` (~$35k) | 75 SOL | **0.7652 SOL** |
| Graduation | 115 SOL | **1.1733 SOL** |

For calibration against reality: of 37 288 first buys observed in the corpus, the
median is 0.52 SOL and **59.5% exceed the 0.3061 SOL launch threshold**. This is
not an exotic condition. It is the median case, and it is one of several reasons
doctrine puts the strategy above the launch window rather than in it.

### 15.3 What it costs us

The unconstrained profit-maximising front-run is enormous — tens of SOL against
a 75 SOL reserve — and produces damage of 45% to 80%. That number is a bound, not
a forecast: an attacker with 58 SOL to deploy into one launch is itself a target,
and treating the unconstrained optimum as the expected case would make the
adverse-selection term useless.

The realistic model constrains attacker capital to `A_max` and charges a fixed
landing cost `c`. At `c = 0.005 SOL` and `φ = 100 bps`:

| `y_r` | Our buy | `A ≤ 1 SOL`: `a*` / damage | `A ≤ 5 SOL`: `a*` / damage |
| --- | --- | --- | --- |
| 10 SOL | 0.25 SOL | not viable | not viable |
| 10 SOL | 0.50 SOL | not viable | 2.86 / 1 272 bps |
| 10 SOL | 1.00 SOL | 1.00 / **472 bps** | 5.00 / 2 061 bps |
| 45 SOL | 0.50 SOL | not viable | not viable |
| 45 SOL | 1.00 SOL | 1.00 / **258 bps** | 5.00 / 1 193 bps |
| 45 SOL | 2.00 SOL | 1.00 / **256 bps** | 5.00 / 1 186 bps |
| 70 SOL | 1.00 SOL | not viable | not viable |
| 70 SOL | 2.00 SOL | 1.00 / **194 bps** | 5.00 / 913 bps |

Inside the target band — `y_r` between 33 and 85 SOL — adverse selection on a
public buy against an attacker limited to 1 SOL runs **190 to 260 basis points**,
rising to 910–1 190 bps if that attacker can deploy 5 SOL. The 472 bps row is at
`y_r = 10`, below the band the strategy trades.

The first of those is a useful consistency check on doctrine from an unrelated
direction: §8 sets default private-bundle slippage at 1–3%, and the modelled cost
of *not* being private in the band that matters is 1.9–2.6%. The two numbers were
not derived from each other.

In the small-size regime the exact formulas reduce to arithmetic worth carrying
in one's head:

```text
E ≈ 2 a' b' / y        damage ≈ 2α
```

Checked against the exact integer simulation at `y_r = 45`: `a = 0.1, b = 0.5`
gives exact `E = 0.001309` SOL against an approximation of `0.001307`, and damage
of 26 bps against `2α = 26 bps`. At `a = 1.0, b = 2.0` the approximation drifts to
255 bps actual against 264 predicted, which is where it should stop being used.

### 15.4 What this model is for

STS does not sandwich anyone, and doctrine forbids the public path this model
prices. Three things it is actually for:

**Pricing the counterfactual.** The tip paid for a private bundle is justified
against the adverse selection avoided, and §15.3 is where that number comes from.
A tip larger than the modelled damage is a tip that is buying nothing.

**The `AS_cost` term in Annex B.4.** The expected adverse move conditional on
side, venue, latency and liquidity bucket is exactly `E[D]` over the displacement
distribution, at horizons of 1, 5 and 20 slots. The distribution is stored, not
only its mean, because the tail is what the stress buckets consume.

**Residual risk under a private bundle.** A private bundle removes the front-run
of our specific transaction. It does not empty the block. Ordinary flow still
lands in the same slot and still displaces us, so section 14's model applies with
the adversarial term removed and the ordinary term kept. Modelling a private
bundle as zero slippage beyond the curve is the optimism this whole part exists
to prevent.

## 16. Landing, expiry and failure

A decision that is not filled is not free, and the three ways it fails are
modelled separately because they cost different amounts.

**Landing.** `P_land(τ, load)` is the probability a bundle lands in the target
slot given tip `τ`. It is fit from observed landed and failed bundle rates by
validator and slot regime — Annex C's `W` — and the simulator refuses to run with
an unfit landing model rather than defaulting to one. A default here would be a
made-up number in the denominator of every expectancy.

**Expiry.** A decision carries an expiry in slots. Each slot it fails to land,
the reserves have moved by another draw of `δ`, and the decision is re-evaluated
against the gate rather than re-submitted blindly. Price drift over `d` slots is
`σ√d` with `σ` estimated from the corpus's one-second candles, per liquidity
bucket. An expired decision produces a `zero-trade` record with its reason, which
is what the roadmap's zero-trade decomposition consumes.

**Failure.** A transaction that lands and fails still pays the base fee and the
priority fee, and a bundle that does not land pays nothing but has consumed the
opportunity. Both are in the cost stack; the second is the one usually forgotten,
and it is charged as the difference between the decision-time price and the price
at which the position was eventually opened, or as a foregone-opportunity zero if
it never was.

## 17. Partial fills, no executable exit, and graduation

**Partial fills.** A pump.fun swap is all-or-nothing at the instruction level —
it carries a minimum-out and reverts below it. So "partial fill" on the curve
does not mean a fraction of one order; it means a multi-transaction exit where
some children landed and others did not. The simulator models an exit as an
ordered sequence of child orders, each with its own landing draw, and the
residual is what `ExecutionState` already calls `needs_unwind`. This matters for
`execution_logs`, which has one row per transition and a `needs_unwind` flag, and
which currently has nowhere to record how much of the position each child moved.

**No executable exit.** Annex B.2's worst bucket. It fires when any of these
holds, and the reason is recorded rather than collapsed into one flag:

| Condition | Meaning |
| --- | --- |
| Required SOL out > `real_sol_reserves` | The curve cannot pay |
| Slippage bound exceeded at every viable size | No size clears the constraint |
| `complete` flipped and no migrated route is validated | The venue moved |
| No route inside its validity window | The precomputed emergency bundle expired |
| `LiquidityThresholds::demands_exit` with no counterparty | Depth collapsed |

Its payoff is modelled as the worst supported exposure path until a valid exit
appears, not as a stop that filled at the stop price. A stop label is not a fill.

**Graduation.** When `complete` goes true the bonding curve stops being the
venue and liquidity migrates. Any quote taken from a complete curve is a quote
from a dead pool, and the simulator treats it as a hard branch rather than a
continuous transition — this is the one discontinuity in the whole model.
`BondingCurve::progress_bps` already returns 10 000 the moment `complete` is set,
so the flag is available. A fixture in which `complete` flips while a position is
open is a required test case, not an edge case.

## 18. The cost stack

Every cost, its source, and where its default lives. Nothing in a simulated
payoff may come from anywhere else.

| Cost | Symbol | Source | Default |
| --- | --- | --- | --- |
| Protocol + creator fee | `φ` | Curve, per swap | 100 bps |
| — protocol share | `φ_p` | Split of `φ` | 95 bps |
| — creator share | `φ_c` | Split of `φ` | 5 bps |
| Curve impact | `S` | §12, computed | — |
| Displacement | `D(δ,w)` | §14, drawn per fill | — |
| Adverse selection | `AS` | §15, conditional expectation | — |
| Base signature fee | — | Network, per signature | 5 000 lamports |
| Priority fee | — | Fitted from landed transactions | — |
| Jito tip | `τ` | Annex C, `min(p75_tip, 0.15 × NetEV)` | — |
| Failed-transaction cost | — | Base + priority on failure | — |
| Migration/route fee | — | Post-graduation venue | — |
| Rent for token accounts | — | Per new associated account | ~2 039 280 lamports |

`φ` is the only part of the split a fill ever sees: a quote is taken against the
sum and the two shares below it are a decomposition of the number the curve
already charged, never a second charge. `φ_p + φ_c = φ` exactly and by
construction — the creator's share is floored the way the programme floors its
own and the protocol takes the remainder, rounding dust included — so moving the
line between them moves a lamport from one column of a report to the other and
moves no realised PnL at all. The 95/5 default is a starting point to be
re-derived from a recording of real swaps, like every other policy number here;
what does not depend on getting it right is the bottom line.

The roadmap's Phase 4 tip rule is `tip = min(p75_tip, 0.15 × NetEV)` with
`NetEV - tip > 0` required, and the simulator enforces both as assertions rather
than as inputs. A backtest that lets a tip exceed the edge it is buying is
producing an expectancy for a policy the executor will refuse to run.

A tip is also bounded from below the other way, and that bound belongs to Phase
4 rather than here: an exit's tip is paid by a transfer inside the same
transaction as the sale, so it is funded by the proceeds — and the proceeds a
transaction *guarantees* are `min_out`, not the quote. `execution::simulate_exit`
refuses a tip at or above the floor for that reason, which is stricter than
refusing one above the quote and is the version that holds in the case the floor
exists to describe.

## 19. Simulator determinism

Everything in Part II is integer arithmetic except the probability weights, and
those follow `RISK_AND_SYBIL_SPEC.md` §7.2 — computed in `f64`, rounded to four
decimal places, and only then used. Two runs must agree on every lamport.

The sampling is the part that needs a rule. **Draws are addressed, not
sequenced:**

```text
draw(run_seed, correlation_id, label, index)
  = SHA-256( run_seed_bytes || correlation_id || label || index_le_bytes )
    → take the first 8 bytes as a little-endian u64
    → map to [0,1) by multiplying into a u128 and taking the high 64 bits
```

`label` is a fixed string naming what is being drawn — `"gap_bucket"`,
`"delta_slot_flow"`, `"land"`, `"queue_position"` — and `index` distinguishes
repeated draws of the same kind for the same decision.

The alternative, one generator advanced sequentially, is what most simulators do
and it is subtly fatal. Every draw then depends on the order and the count of
every draw before it, so adding a single new sampled quantity anywhere — or
logging one that was previously computed lazily — shifts every subsequent number
in the run. The result is a simulator whose output changes when the code is
refactored, which makes the entire replay-equivalence gate impossible to satisfy
for real reasons and trivial to satisfy by accident.

The labels are part of the artefact. `run.json` records, per decision, which
labels were drawn and how many times, so a diff between two runs points at which
draw diverged instead of at a number that is different for unknown reasons.

---

# Part III — Historical verification of Sybil cluster detection

`RISK_AND_SYBIL_SPEC.md` Part I specifies four cluster metrics and a logistic
that fuses them into `P_group`. It is careful to say the logistic "has a shape,
not a fit", and that its coefficients come from a calibration fixture with a
Brier score, a reliability diagram and a leakage check. This part specifies that
fixture: what data it is built from, where the labels come from, how it is split,
what is measured, and — the part that is usually skipped — what the available
data cannot support being claimed.

## 20. What the corpus actually is

There are 12 205 records in `data/coins-*.jsonl`, written by the archived Node
collector over seven days between 2026-08-10 and 2026-08-21, covering 12 089
distinct mints. Each record is one launch with its curve state at detection, its
opening flow, the wallets that traded it, an attempted funding resolution, and a
short outcome.

| Day | Records | With funding transfers |
| --- | --- | --- |
| 2026-08-10 | 173 | 0 |
| 2026-08-11 | 2 965 | 0 |
| 2026-08-12 | 34 | 0 |
| 2026-08-15 | 152 | 0 |
| 2026-08-16 | 1 822 | 40 |
| 2026-08-20 | 5 392 | 2 209 |
| 2026-08-21 | 1 667 | 750 |

What is in it, measured:

| Quantity | Value |
| --- | --- |
| `who[]` wallet observations | 148 986 |
| Distinct wallets | 36 769 |
| Wallets appearing in more than one launch | 16 651 (45.3%) |
| Wallets appearing in ten or more launches | 2 656 |
| Most launches touched by one wallet | 1 829 (15.0% of the corpus) |
| Distinct creators | 4 538 |
| Creators with more than one launch | 1 457 |
| Most launches by one creator | 192 |
| Records with any funding transfers resolved | 2 999 (24.6%) |
| Funding resolution depth | 2, on every record that has any |
| Records with a known `deployerFunder` | 1 922 |
| Distinct funder addresses | 2 395 |
| Records with an outcome block | 9 568 |
| Follow window | 60 s on 11 966 records, 45 s on 173, 40 s on 66 |

And the outcome distribution, which is the part that decides what can be graded:

| Statistic | p10 | p50 | p90 | p99 |
| --- | --- | --- | --- | --- |
| `peakMult` | 1.00 | 1.00 | 1.69 | 4.96 |
| `endMult` | 0.47 | 0.99 | 1.08 | — |

1 067 records end the follow window below half their entry price and 379 end
below a fifth.

### 20.1 What this corpus is not

**It is not frame-level.** These are summaries written by a different program in
a different language with a different schema. They cannot be replayed through
`ingestion.rs` — there are no provider frames in them, no slots on the individual
trades, no account bytes. So the corpus supports Part III and does not support
Part I. Frame-level fixtures for replay have to be recorded fresh by the recorder
in section 4, against live feeds, which is Phase 1 work that has not been done.

**It has no Sybil labels.** Nothing in any record says "these wallets were one
hand". The funding block is an *attempt* at the evidence, resolved on a quarter
of the records, at depth 2 only. `RISK_AND_SYBIL_SPEC.md` §3.4 specifies a
traversal to depth 4 with a 64-wide fanout; depth 2 is a third of that reach, and
the influence numbers computed from it are lower bounds — which, per the same
document's asymmetry rule, may block an entry and may not clear one.

**Its outcome window is 45 to 60 seconds.** This is the single most important
limitation and section 25 returns to it. A minute after detection is not enough
time for anything the word "rug" describes, and the strategy's own target regime
is the `$25k–$80k` consolidation base, which by construction is reached later
than that.

**It is one week, mostly two days.** 58% of the records and 99% of the funding
resolutions come from 2026-08-20 and 2026-08-21. A walk-forward evaluation over
seven days where two of them carry the evidence is a two-fold evaluation wearing
a seven-fold label.

## 21. Ground truth

There is no label column, so labels have to be manufactured, and the way they are
manufactured determines what the resulting number means. Three tiers, ranked by
what they can be used to claim.

### L1 — Adjudicated gold set

A human-adjudicated set of clusters, target 200, sampled stratified across
launch day, wallet count and whether funding resolved. Each candidate cluster is
presented to two independent adjudicators with the raw evidence — the funding
transfers, the buy times, the flow, the wallet histories — and no model output.
Each answers one question: *is there sufficient evidence that these wallets are
controlled by one party?* with three permitted answers: yes, no, insufficient
evidence. The third is not a hedge; it is the honest answer for most of this
corpus and it is a label in its own right.

Inter-adjudicator agreement is reported as Cohen's kappa alongside every metric
derived from the set. Disagreements are resolved by re-examining evidence and
recording why, never by majority or by asking a third adjudicator to break a tie
— a tie-break vote converts a genuine ambiguity into a confident label, which is
precisely the corruption this tier exists to avoid.

**L1 is the only tier that may support a precision claim.**

### L2 — Programmatic weak labels

High-precision rules applied at scale: three or more wallets funded by the same
non-absorbing parent within a bounded window, each opening a position within a
few slots of the others. These produce thousands of labels cheaply.

They also produce them **from the same features the detector uses**, which makes
any precision measured against them circular — the detector is being graded on
agreeing with a simpler version of itself. The rule:

**L2 may be used for recall estimation, for negative mining, and for regression
detection. It may never be used to claim precision on any feature it shares with
the detector.** Every L2 rule is recorded with the exact feature set it touches,
so the overlap is checkable rather than argued about.

### L3 — Outcome proxies

`endMult`, `peakMult`, seller counts. These measure whether a launch went badly.
They do not measure whether wallets were coordinated, and the two are different
questions with different answers: a coordinated cluster can hold, and an
uncoordinated launch can collapse under ordinary selling.

L3 is reported as its own metric with its own name — rug avoidance, not Sybil
detection — and the two are never combined into one accuracy figure. Doctrine
already requires this separation ("separate rug avoidance from profitability");
this is the same rule applied one level down.

## 22. Splitting

A random split of this corpus is meaningless, and the number that makes it
meaningless is in section 20: **45.3% of wallets appear in more than one launch,
2 656 appear in ten or more, and one appears in 1 829 of them.** Split at random
and nearly half the wallet population is on both sides of the line, along with
whatever the model learned about them.

The split is therefore two constraints applied together.

**Time.** Folds are ordered by launch time with a purge and an embargo. The purge
removes training records whose outcome window overlaps the test window's start;
the embargo holds out a further interval after each training fold so that
short-horizon correlation does not carry across the boundary. With a 60-second
outcome window the purge is small and the embargo is the binding one; one hour is
the default, and it is policy.

**Funder group.** Records are grouped by the root funder identified in the
funding block, with unresolved records grouped by deployer, and whole groups go
to one side. The top funder in the corpus appears in 1 107 records; without group
splitting it is on both sides of every fold.

The harness asserts both and fails the run rather than reporting a warning:

```text
assert max(train.observed_at_ms) + embargo_ms <= min(test.observed_at_ms)
assert funder_groups(train) ∩ funder_groups(test) == ∅
assert wallet_overlap_fraction(train, test) is computed and reported
```

The third one is a report rather than an assertion because it cannot be driven to
zero — the 1 829-launch wallet is in every fold no matter how the corpus is cut.
What it can be is *known*, and reported next to every metric, so a number computed
on folds with 30% wallet overlap is not compared against one computed on folds
with 5%.

## 23. What is measured

Three different things get measured and they are not interchangeable.

### 23.1 Partition quality

The detector produces a partition of wallets into clusters. Grading it against a
gold partition is a clustering problem, not a classification problem, and
accuracy is not defined for it.

```text
pairwise precision = |pairs together in both| / |pairs together in prediction|
pairwise recall    = |pairs together in both| / |pairs together in gold|
pairwise F1        = harmonic mean
```

Reported alongside B-cubed precision and recall, which weight by wallet rather
than by pair and are therefore not dominated by one large cluster, and the
Adjusted Rand Index, which is corrected for chance agreement. All three, because
each is blind to something: pairwise F1 rewards a single giant cluster, B-cubed
punishes it, and ARI is the chance-corrected summary that neither gives.

The base rate matters here. With 36 769 wallets the number of possible pairs is
about 6.8 × 10⁸ and the number that are genuinely linked is a vanishing fraction
of it, so a detector that predicts nothing scores near-perfect accuracy and a
detector that links one large exchange-funded group scores catastrophically. This
is why the absorbing-node rule in `RISK_AND_SYBIL_SPEC.md` §3.1 is a correctness
requirement and not an optimisation: the highest-degree funder in this corpus
appears in 1 107 records, a degree profile characteristic of an exchange hot
wallet rather than a launch operator. If it is one and the rule is not enforced,
it links 1 107 launches into a single cluster and every pairwise metric collapses.
Whether it is one is exactly what the versioned exclusion list has to answer, and
it is the first entry the benchmark should resolve.

### 23.2 The flag

`flag_sybil` is a threshold on `P_group` and is graded as a binary decision.

**PR-AUC, not ROC-AUC.** With a base rate this low, ROC-AUC is dominated by the
enormous true-negative population and a useless detector scores well on it.
Precision-recall is the curve that responds to what changes.

Reported at the operating threshold as well as integrated: precision, recall, and
the count of flagged clusters, because a precision of 0.9 on 12 flagged clusters
is a different statement from the same figure on 400.

### 23.3 Calibration

`P_group` is used for graded consequences — hard block above 0.95, quarantine
from 0.80, tier demotion from 0.55 — so its value has to mean something, not just
its ordering.

Brier score, a ten-bin reliability diagram, and expected calibration error,
computed on L1 only. A reliability diagram needs enough per bin to be read: at ten
bins and a floor of 30 per bin, that is 300 scored clusters, which is above the
L1 target of 200. So the reliability diagram is reported at five bins with the bin
counts printed next to it, and the ECE carries a bootstrap interval. Reporting a
smooth ten-bin curve over 200 points would be drawing a picture of the noise.

### 23.4 Confidence intervals

Every interval is a **cluster bootstrap resampled by launch**, not an i.i.d.
bootstrap over clusters. Wallets within a launch are correlated by construction —
that is what the detector is looking for — so resampling clusters independently
understates the variance, usually by a lot. The resampling unit is the launch,
which is the largest unit that is plausibly independent, and even that is
generous given the wallet overlap in section 22.

## 24. The three benchmark tiers

### B1 — Synthetic, with exact answers

Graphs generated from a known process, so the correct output is known exactly
rather than adjudicated. These exist to catch implementation bugs, and they are
the degenerate cases `RISK_AND_SYBIL_SPEC.md` §7.1 already enumerates, each with
its required output:

| Generator | Required behaviour |
| --- | --- |
| Star: one funder, N leaves, synchronised buys | `temporal_influence` high, `interaction_entropy` near 0, cluster found |
| Star with staggered buys over 4 h | `sync` low, geometric mean low, **not** flagged |
| N funders, synchronised buys | `fund` low, geometric mean low, **not** flagged |
| Complete graph, equal weights | `spectral_separation` near 0, `interaction_entropy` exactly 1 |
| Two cliques, one bridge edge | Two clusters, conductance matches Cheeger bound |
| Disconnected island in the neighbourhood | Component restriction applies; island does not affect the score |
| Self-loops present | Dropped before assembly |
| All balances zero | HHI `None`, no row written |
| Chain of 24 funding hops | Resolved to its origin, discounted by the hop term, not truncated |
| Chain of N funding hops beyond depth 24 | Truncated, marked, does not clear |
| Long chain scoring above the same money one hop away | Impossible; monotone in hops |
| Edge the chain has no transaction for | Dropped before assembly, contradiction event, does not clear |
| Edge one provider confirms and another denies | `SPLIT`, dropped, contradiction event |
| Edge no provider could answer for | Kept at the discount, UNKNOWN, does not clear |
| Every edge confirmed by two providers | Unchanged scores; the only state that may clear |

Every one of these must produce the exact documented value, and B1 runs on every
commit. It is the tier that fails when someone changes a summation order.

### B2 — Historical

The corpus, split per section 22, graded per section 23. This is the tier that
produces the numbers in a gate dossier, and the only one whose results may be
described as performance.

### B3 — Adversarial

Evasions applied to B1 and B2 inputs, each targeting a specific metric, each with
the degradation it is expected to cause. The point is not that the detector
survives all of them — it will not — but that the failure is known and priced
rather than discovered by an adversary.

| Evasion | Targets | Expected effect |
| --- | --- | --- |
| Fund through one extra hop | Depth budget | Influence falls; at depth 5 it truncates |
| Split funding across 8 parents | `fund(C)` | Funding concentration falls below threshold |
| Stagger buys over 30 s | `sync(C)`, `tau_sync = 5 s` | Synchrony collapses; geometric mean collapses with it |
| Route through a CEX deposit and withdrawal | Absorbing nodes | Link is correctly *not* made; this is a true negative, not a miss |
| Interleave wash trades with outsiders | `interaction_entropy` | Entropy rises toward 1 |
| Add 200 dust wallets | HHI, `N_eff` | HHI barely moves, `N_eff` inflates, dust detector must catch the gap |
| Vary buy amounts by ±40% | `amount_similarity` | Feature weakens; others must carry |
| Exceed the 64-wide fanout at the root | Fanout budget | Truncation flag set; result may not clear |

Each row is a test that asserts the expected effect, and a row whose effect is
*not* observed is as much a finding as one that fails — it means the metric was
not measuring what it was believed to measure.

## 25. What the corpus can and cannot support

Sample sizes first, because a claim without one is a claim about nothing.

To estimate a precision of around 0.90 with a 95% interval of ±5 percentage
points takes about **140** flagged clusters with labels. To distinguish a
precision of 0.90 from one of 0.80 at 80% power takes about **200 per arm**. The
L1 target of 200 adjudicated clusters is sized for the first and not the second,
which means L1 supports *reporting* precision and does not support *comparing*
two model versions on it. Comparing versions needs either a larger gold set or a
paired design on the same clusters, and the paired design is much cheaper.

Then the limitation that governs everything else:

**The 85–95% rug avoidance target is not testable on this corpus.** The outcome
window is 45 to 60 seconds. Whatever is measured in that window, it is not rug
avoidance; it is behaviour in the first minute after detection, and the strategy
does not trade in the first minute — `SpamFloor::DEFAULT` excludes the first ten
slots and the `$25k–$80k` target band is typically reached later still. A number
produced from these outcomes and labelled "rug avoidance" would be a category
error with a confidence interval attached.

What is needed instead is a corpus with an outcome horizon measured in hours,
carrying at least the migration event and the post-migration price. Producing it
means recording launches and following them, which is a Phase 1 and Phase 2
recording exercise, not an analysis that can be done on what already exists.

Until that corpus exists, the honest statements this data supports are:

- Partition quality against an adjudicated gold set of a stated size, with kappa.
- Calibration of `P_group` on that gold set, with bootstrap intervals.
- Recall against L2 weak labels, with the shared-feature overlap declared.
- Degradation under each B3 evasion, measured.
- First-minute price behaviour conditioned on cluster flags, named as such.

And the statements it does not support, which belong in the dossier as
exclusions rather than being left out:

- Any rug-avoidance rate.
- Any win rate.
- Precision on any feature shared with the L2 rule that produced the labels.
- Any claim about the `$25k–$80k` regime, which the 60-second window barely
  reaches.

## 26. Determinism of the benchmark

The benchmark is a replay and obeys Part I. Two runs of one benchmark over one
corpus with one model version produce byte-identical reports, and the report
carries the corpus manifest hash, the label-set hash, the model version, the
exclusion-list version and the split seed. A benchmark whose number moves between
runs is measuring the machine.

The funding evidence is cached, not fetched. Re-resolving funding trees needs
archival RPC calls, the roadmap's fixed infrastructure budget is exactly €0, and
a benchmark that hits a rate-limited free tier is one that produces different
numbers depending on the quota remaining that day. So every funding resolution
used by the benchmark lives in a versioned fixture directory with its own
manifest, resolved once, and the benchmark runs entirely offline. A resolution
that is missing is UNKNOWN, and UNKNOWN blocks a claim in exactly the way it
blocks an entry.

---

# Part IV — Acceptance

## 27. The eleven suites

The roadmap names eleven and requires all of them to pass, with a failure naming
the fixture, the correlation ID, the expected and actual state, and durable
evidence. Here is what each one covers in this document's scope and where its
evidence lands.

| # | Suite | Covers here | Evidence |
| --- | --- | --- | --- |
| 1 | Unit | Slippage arithmetic (§12.4), curve decode, hash chain links, canonical bytes | `cargo test` |
| 2 | Integration | Recorder → fixture → playback → database, end to end on a small stream | `reports/<run>/run.json` |
| 3 | Property | The obligations in §28 | `cargo test`, proptest |
| 4 | Replay | Two-run byte equivalence, seed independence of decisions, segment independence (§10) | `diff`, digests |
| 5 | Regression | B1 synthetic vectors (§24), every documented test vector | `cargo test` |
| 6 | Load | Playback of the p99.9 load fixture: 92 trades/s sustained, and the 347-trade second | `metrics.json` |
| 7 | Failover | Fixtures with reconnects, provider disagreement, one provider silent, quota exhaustion | Fidelity report |
| 8 | Chaos | Truncated fixture, corrupted chain link, wrong `prev_hash`, disk full mid-record, clock regression | Safe-mode evidence |
| 9 | Economics | Walk-forward with purge and embargo, gap and slippage buckets, CVaR, no-exit frequency | `reports/phase3` |
| 10 | Security / non-custody | No key material in fixtures, audit logs, `run.json` or `metrics.json`; replay cannot sign | Fixture scan |
| 11 | UI / IPC contract | Projections rebuilt from a replay match byte-for-byte | `projections compare` |

Suite 10 deserves one specific note. A fixture is a recording of network traffic
and it is committed to a repository. The scan that proves no key material is in
it must run on the fixture, not on the code that writes it, because the leak that
matters is the one already on disk. Provider URLs carry API keys for all three
providers — `EndpointConfig::redacted` exists precisely because of this — so the
fixture stores `endpoint_host` and never a URL.

## 28. Property obligations

Numbered from R1 to continue the risk document's P-series rather than collide
with it. Each must exist as a test and must not be skipped.

| # | Property | Method |
| --- | --- | --- |
| R1 | Two runs of one fixture produce byte-identical `run.json` | Digest comparison |
| R2 | Two runs with different seeds produce identical decisions | Diff the decision subset |
| R3 | Replaying a segmented fixture equals replaying it whole | Segment and compare |
| R4 | Nothing in `run.json` changes when the host is slower | Run under artificial load |
| R5 | The decision path cannot reference an outcome type | Dependency-graph assertion |
| R6 | The cursor has no seek and no random access | Type-level; no such method exists |
| R7 | Every fold satisfies purge and embargo | Assertion inside the harness |
| R8 | No funder group appears in both train and test | Set intersection assertion |
| R9 | Chain verification rejects any single-byte edit to any fixture record | Mutation test over the corpus |
| R10 | A fixture with `complete: false` cannot be used in a gate run | Refusal test |
| R11 | Replay never opens the live database | Path assertion plus a test that the live file is unmodified |
| R12 | `slippage_bps` never panics or overflows for any `u64` size | Property test including `u64::MAX` |
| R13 | Round-trip cost with no intervening flow equals `2φ` within 1 bp | §12.5 across the whole curve |
| R14 | `E(α, β) < βy` for all sampled `α, β` | Property test |
| R15 | No profitable front-run exists below `β = φ/(1-φ)` | Search over `α` at each of three curve states |
| R16 | The closed forms `G` and `E` match a direct three-swap simulation to within a bounded residue | Differential test |
| R17 | A tip never exceeds `min(p75_tip, 0.15 × NetEV)` and `NetEV - tip > 0` | Assertion in the fill path |
| R18 | A complete curve is never quoted | Fixture where `complete` flips mid-position |
| R19 | Every draw is reproducible from `(seed, correlation_id, label, index)` alone | Re-derive every draw in a run |
| R20 | No key material appears in any fixture, report or audit file | Scan, suite 10 |
| R21 | Wallet overlap between folds is computed and present in every report | Field-presence assertion |
| R22 | A benchmark run makes no network calls | Offline execution under a blocked network |

R4 is the one most likely to be skipped and the one that catches the most. Running
the replay suite on a machine that is simultaneously compiling something is a
cheap approximation and finds real timing leaks.

## 29. Milestone verification commands

Extending the roadmap's Phase 3 block with what this document adds:

```bash
cargo test --workspace --all-features

# fixtures: record, verify the chain, replay twice, compare
sts replay record --input fixtures/phase3 --out data/replay/phase3
sts replay verify --fixture data/replay/phase3 --assert-chain --assert-complete
sts replay run --stream data/replay/phase3 --seed 0x100x --out reports/phase3-a
sts replay run --stream data/replay/phase3 --seed 0x100x --out reports/phase3-b
diff -u reports/phase3-a/run.json reports/phase3-b/run.json

# decisions must not depend on the seed; fills may
sts replay run --stream data/replay/phase3 --seed 0xdead --out reports/phase3-c
sts replay compare --decisions-only reports/phase3-a reports/phase3-c

# fidelity against what the live run actually did
sts replay fidelity --stream data/replay/phase3 \
  --allow-drop backpressure --fail-drop not-allowlisted,decode,filtered

# economics
sts backtest walk-forward --purge --embargo 1h --group-by funder \
  --gaps 30,50 --slippage 10,15,20,25 --out reports/phase3

# the simulator's own arithmetic
sts sim verify-vectors --assert-exact
sts sim sandwich-threshold --states 30,75,115 --assert-breakeven

# sybil benchmark, offline
sts bench sybil --tier b1 --assert-exact
sts bench sybil --tier b2 --corpus data/coins --labels fixtures/labels/l1 \
  --split time,funder --bootstrap launch --out reports/phase3-sybil
sts bench sybil --tier b3 --assert-degradation-matrix

sts test-suites run --all-11 --fail-on-critical
```

## 30. What this needs that does not exist

Stated plainly so each is a decision rather than a discovery, in the same spirit
as the closing sections of `SCHEMA.md` and `RISK_AND_SYBIL_SPEC.md`.

**There is no `Clock` seam.** `telemetry::now_ms` calls `SystemTime::now()`
directly and `ingestion.rs` calls `Instant::now()` in five places. Nothing in
Part I is testable until time is injectable. This is the first piece of work and
everything else waits on it.

**The engine is one crate.** `src-tauri` is a single package, so §9's leakage
barrier — outcomes in a crate the decision path does not depend on — cannot be
expressed yet. Splitting it is the cheapest structural guarantee in this
document and it gets more expensive the longer it waits.

**There is no replay module, no `fixtures/`, no `tests/` and no `benches/`.** The
roadmap's Phase 0 and Phase 2 commands reference `fixtures/phase0`,
`cargo bench --bench hot_path` and `cargo bench --bench fast_path`; none of those
paths exist in the checkout. The 104 tests that do exist are `#[test]` modules
inside `ingestion.rs` and `types.rs`.

**`ingest_candidates` has no `mode` and no `run_id`.** `execution_logs` already
carries `mode TEXT CHECK (mode IN ('live','paper','replay'))`, which is exactly
right; the ingest table has nothing equivalent. Combined with its
`UNIQUE (source, account, slot)` index and `INSERT OR IGNORE`, a replay into the
live file writes nothing at all and reports success. Section 8's separate-file
rule works around this; a column would fix it.

**There is no table for fills, positions or PnL.** `execution_logs` records one
row per state transition with a `size_lamports` and an optional `price`, which is
the order's history and not its fills. A partial exit made of four child orders
has nowhere to record how much each one moved, so §17's residual accounting
cannot be persisted and a backtest cannot reproduce a realised PnL from the
database alone.

**There is no `replay_runs` or `sim_runs` table.** Run manifests currently live
only as files under `reports/`, so a run cannot be joined to the rows it
produced, and "which run wrote this row" has no answer.

**There is no Rust audit-log writer.** `SCHEMA.md` states that the durable record
of first resort is the append-only NDJSON audit log, written and flushed
separately, and that this is what justifies `synchronous = NORMAL`. On the Rust
side there is no NDJSON writer at all: `Database::record_audit` writes to an
`audit_log` **table** that `db.rs` does not create and that `SCHEMA.md` does not
document, and it returns an error saying to run `npm run db:setup` — a Node
script that now lives in `docs/archive/`. Part I's hash chain and suite 8's
corruption tests both depend on an audit log that exists.

**`max_pool_share_bps` has two values.** 150 in `RISK_AND_SYBIL_SPEC.md` §10, 500
in `StreamFilters::DEFAULT`. Section 13 uses 150 and the disagreement needs
settling, not splitting.

**The corpus has no Sybil labels and no long-horizon outcomes.** Sections 20.1
and 25. The gold set has to be built and the long-horizon corpus has to be
recorded; neither is analysis work.

**`AUDIT_EVENTS.md` describes a program that no longer runs.** It documents
`src/audit.js` and the `AuditLogger` API, both archived. Anyone implementing the
Rust audit writer against it will implement the Node one.

None of these block the specification. All of them block a Phase 3 replay dossier
that can explain a decision, which is the thing the phase exists to produce.

## 31. What this document does not decide

- **The EV model.** Part II produces costs. Turning costs and path probabilities
  into an expectancy is Annex B, and the gap-bucket priors, the smoothing
  strength `κ` and the data-risk penalties belong there.
- **Tip calibration.** §18 asserts the tip rule; fitting `α`, `Tip_base`,
  `Tip_max` and the landing probability `W` is Annex C and Phase 4.
- **Bundle construction.** Accounts, routes, simulation-before-signing and the
  private-relay contract are Phase 4 and have their own document. This one stops
  at "a fill was modelled".
- **The Sybil metrics themselves.** `RISK_AND_SYBIL_SPEC.md` Part I defines
  them. Part III grades them and does not redefine them.
- **Model weights and thresholds.** Every policy number here is a starting point
  to be re-derived from held-out data, written down so it can be argued with
  against evidence rather than treated as settled.
- **Whether the strategy is profitable.** A simulator that says yes is a
  simulator, and the roadmap is explicit that a profitable-looking result which
  fails any economic stress is rejected.

The line this document draws is between what was recorded and what is inferred.
Part I fixes the recording so that it cannot be argued with. Part II is entirely
inference, and every number in it is an assumption with a bound attached. Part
III is the seam: it takes a detector built on inference and grades it against a
recording, which only works if the recording is honest about what it does not
contain. The corpus in `data/` is not honest about that on its own — nothing in
it says the outcome window is a minute long — so saying it here is the point of
having written this down.
