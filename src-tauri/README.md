# src-tauri

> **STATUS NOTE — 27 August 2026.** "The shipped policy trades nothing, and that
> is the correct state" is now the terminal state, not an interim one. This file
> says the roadmap's Phase 3 gate is what produces a number for `edge_lcb_bps`.
> **That gate was measured on 27 August 2026 and it failed** — expectancy is
> negative by about 5.7 points against break-even, and at a fee of zero the best
> realizable rule still loses 0.86% a trade. Wherever this file names a "target
> window" or "market-cap thresholds", that band ($25k–$80k) was tested: it shows
> no edge over buying everything and still loses, and it is a moment rather than a
> phase — median 89 seconds from launch to $25k. See
> [`../docs/VERDICT-2026-08-27.md`](../docs/VERDICT-2026-08-27.md).
>
> Note also that "the exit quoted before the entry is agreed" is a quoted *price*,
> not an exit rule. There is no stop, target, trailing stop or time exit anywhere
> in `src/`, and no entry-side transaction builder. The "What is not here" section
> below is accurate and is the right place to start.

The Rust backend for the STS desktop window. Phase 0: the parts that have to be
right before anything is built on them — how the process starts, how it stops,
what happens when it breaks, and how it talks to the window.

## Running it

```bash
cd src-tauri
cargo check      # what CI should gate on
cargo build      # produces target/debug/sts
```

The same binary is also the fixture harness. With a subcommand in front of it
`sts` runs on the terminal and never builds a window:

```bash
sts backtest generate --out data/replay/synthetic
sts backtest verify   --fixtures data/replay/phase3
sts backtest run      --fixtures data/replay/phase3 --gate --out reports/phase3.json
sts backtest sandwich --reserves-sol 30,75,115
```

`generate` writes the synthetic stress corpus. `verify` walks every JSONL line,
checks the hash chain and prints what failed. `run` does that and then replays
the events, prices every decision against the same integer curve model, and
writes the forensic report. `sandwich` prints the `β = φ / (1 - φ)` threshold
table and reads no fixture at all. `--help` lists the rest; the exit code is 0
for a clean run, 1 for a command line that could not be read, 2 for a corpus
that did not verify under `--gate`, and 3 for a file that could not be read or
written.

## Running it headless

`sts daemon` is the window's opposite number: the same engine — lifecycle,
database, telemetry fan-out, metrics port, ingestion — with nothing drawing it,
and a fixture corpus playing through it instead of somebody clicking.

```bash
sts daemon run --fixtures data/replay/synthetic --telemetry data/daemon.ndjson
sts daemon run --fixtures data/replay/synthetic --gate-profile v1 --out reports/daemon.json
sts daemon run --fixtures data/replay/synthetic --sandwich-guard required --private-entry
```

> `--telemetry -` currently deadlocks: `main.rs` holds `std::io::stderr().lock()`
> for the whole subcommand while the sink writes from the engine's own threads,
> so the first telemetry line blocks on a lock the main thread does not release
> until the run it is waiting for has finished. Write to a file until that is
> fixed.

The pipeline is five stages and the report is laid out as them:

1. **fixtures** — a corpus of case directories, or one case directory of
   `.jsonl` streams.
2. **the replay feed** — every segment under the manifest's stream id, chain
   verified before a record is played. A fixture whose chain does not verify is
   refused rather than repaired, and a corpus carries cases built to be refused,
   so a refusal is a result and not a failure. `--speed` plays the recording at
   the rate it was recorded at; the default plays as fast as it parses.
3. **the entry rule** — every launch is read when its opening window closes and
   never later, so nothing that happened after the decision is in the evidence
   the decision was made on. `evidenceToMs` on each launch is that boundary as a
   number you can check against the recording. `--gate-profile` picks between
   the shipped rule and the rule before the bundle checks, which is how the two
   are compared over one corpus at one price rather than by quoting an old
   checkout.

   The last question the rule asks is the only one that is not about the launch.
   Our own order is priced against the curve at the instant of the decision —
   `--entry-lamports` clipped by `--max-pool-share-bps`, which is the same size
   stage 4 fills at — and §15.2's `β > φ / (1 - φ)` says whether a public entry
   that size is worth front-running. `--sandwich-guard` is `off`, `when-quoted`
   or `required`: `when-quoted` is the shipped setting and refuses an order the
   curve says is exposed, `required` also refuses a launch whose curve could not
   be priced at all, and `off` is what v1 shipped with. `--private-entry` prices
   the same order as a bundle: the exposure is still computed and still on the
   report, and it stops being a refusal, because a send nobody can read first is
   not the one §15.1 models.

   The two questions stay apart on the report. `reason` is `sandwich-risk` or
   `no-curve-quote` only for a launch that had already passed every question
   about the launch itself, `refusedOnOurOrder` says which side of that line a
   refusal fell on, and `quotedLamports` is the size the verdict was reached
   about. A launch the rule liked and the cap left no room for is not a refusal
   at all — it is a problem on the case, because a plumbing failure recorded as
   a strategy result is a funnel that lies about the rule.

   Two reserves answer the two halves of that sizing, and they are not the same
   number. The participation cap is a share of `real_sol_reserves` — the SOL
   actually in the pool, which is what a position has to come back out through
   — while §15.2's threshold scales with `virtual_sol_reserves`, the `y` in the
   price. On a fresh curve they differ by a factor of three. Both are on each
   launch as `realSolLamports` and `virtualSolLamports` so the arithmetic can be
   checked against the report rather than taken on trust.

   `--fee-bps` and `--max-pool-share-bps` are held to the range they mean
   something in: a fee at or above 100% makes every number on the check
   degenerate, so the guard would clear every order while the report said it had
   read the curve, and a share above 100% of executable liquidity is not a
   participation cap. Both are refused at the command line, and neither — nor
   any other sizing knob — is accepted without `--fixtures`, because a flag that
   is parsed and then dropped is a run answering a question nobody asked.
4. **a simulated execution** — an accepted launch is priced against the curve as
   the recording left it, capped at the participation limit, and written through
   the six-state machine in `execution_logs`. The exit is the real path —
   `build_exit` through `Flattener`, which signs, pays a Jito tip as the last
   instruction of the transaction that sells the position, broadcasts and books
   — against `MockSolanaSigner`, whose `is_live` is false. Every row is
   `mode = 'replay'`, and the run only ever flattens positions it opened itself.
5. **telemetry export** — `--telemetry <file>` appends the stream as NDJSON, one
   JSON object per line, flushed per line. `-` writes it to stderr so the report
   on stdout stays a document a pipe can read. A signal that lands while the
   file is being appended to leaves it whole: the pump is joined rather than
   dropped, so the last line is a complete object and `telemetryExported` is the
   file's line count rather than a claim about it.

Two things about the report. Its `pipeline` half is a function of the corpus and
the flags and of nothing else — no wall clock, no host, no elapsed time — so two
runs of one corpus serialise to the same bytes; its `process` half is where the
latency histograms, the signal that stopped it and the machine-specific numbers
live. And `openPositions` is on the front of it, because it is the one number
that says what the run left behind.

SIGINT and SIGTERM stop the feed between records, close the exporter and the
sockets, and checkpoint the ledger. **Nothing is sold on the way out**: a
position open when the signal arrives is reported open, because flattening is a
trade and a process that traded because somebody pressed Ctrl-C would be making
a decision that the person pressing it had not. A second signal leaves
immediately with 130.

Exit codes match `sts backtest`: 0 for a clean run — including one a signal
stopped — 1 for a command line that could not be read, 2 for a case that did not
verify under `--gate`, 3 for an engine that would not start or a file that could
not be written.

## The synthetic corpus

`fixtures.rs` builds fixtures rather than reading them, and `sts backtest
generate` is it on the command line. Ten cases, one directory each, each holding
its rotated streams, the manifest that describes them, and an `expected.json`
saying what the harness should conclude about the pair:

| case | what it is for |
|---|---|
| `graduation` | the control: a clean curve walked to graduation, one round trip, nothing to flag |
| `sybil-rug` | one funder behind a same-slot bundle, a same-slot dump, a pull, and an exit that strands |
| `sandwich-boundary` | our entries one lamport under, on, and one lamport over `b*`, at three curve positions |
| `backpressure` | frames a full queue could not take, frames the filters rejected, and a reconnect |
| `corruption-*` | six clean chains, each edited one way after it was sealed |

```bash
sts backtest generate --out data/replay/synthetic --seed 0x100x --segments 3
sts backtest run      --fixtures data/replay/synthetic/sybil-rug --gate
```

Four things about it that are easy to get wrong.

**The corpus is a function of the scenario, the flags and the seed.** No clock,
no host name, no filesystem order. Regenerating on another machine produces the
same bytes, so a case is cited by name and seed rather than shipped as a blob.

**The generator carries a mirror of the curve and moves it exactly the way the
evaluator will.** That is what makes a boundary case a boundary: each rung is
sized against the virtual reserve as it stands at the instant it is emitted, not
against the reserve the launch started at. `expected.json` records where the
mirror ended, and the tests assert the evaluator ends in the same place — a gap
there means every size in the case was computed on a belief about the evaluator
that is wrong.

**Half of it exists to be refused.** The corruption cases are built by sealing a
clean chain, writing the manifest from *that*, and only then editing the bytes —
which is the shape real tampering has. Two of them reseal the chain forward from
the edit, so every hash verifies and the only thing left to catch them is the
link, the slot order, or the manifest.

**`--force` replaces a case directory's streams.** Without it an existing file is
a refusal, because a fixture directory is somebody's evidence until they say
otherwise. With it the old `.jsonl` files go first: regenerating with fewer
segments would otherwise leave a stale tail behind, and a run reads every
`.jsonl` in the directory.

Two properties the harness is built around. **The report is a function of the
fixture and the flags and of nothing else** — no timestamp, no host name, no
floating point anywhere in the financial path — so two runs of one fixture
`diff` clean, which is what the Phase 3 equivalence gate turns on. And **a broken
chain is reported rather than repaired**: the audit keeps walking past a bad
line, names it, and marks everything downstream *unverifiable* rather than
*wrong*, so one edited byte produces one finding instead of a thousand.

There is no `npm run tauri` yet. Wiring the Tauri CLI into `package.json` would
mean adding `@tauri-apps/cli` as a dependency, and that is a decision about how
the app is launched, not part of standing the backend up.

## What the window can ask for

Thirty commands, all typed on both ends.

| command | takes | gives back |
|---|---|---|
| `get_engine_status` | — | lifecycle, kill-switch state, uptime, telemetry counters, `sts.db` health |
| `trigger_kill_switch` | optional `reason` | a receipt: whether it was already armed, when, and the audit row id |
| `trigger_emergency_unwind` | optional `intentIds`, optional `reason` | a receipt: the halt, what was sold, and every position still on chain |
| `stream_telemetry` | a channel | a subscription id, then events on that channel until the window closes |
| `get_ingestion_metrics` | — | frame and candidate counters, rates, the health of every endpoint, and the last time two providers disagreed |
| `get_geyser_metrics` | — | stream counters, the slot ledger's heads, and what sub-slot ordering cost |
| `get_metrics` | — | tick latency and cadence, what the feed cost, where the executions are |
| `set_sol_price` | `centsPerSol` | the price the market-cap thresholds are now measured against |
| `get_replay_status` | — | whether a fixture is driving the clock, which one, where the playhead is |
| `set_replay_playback` | optional `active`, optional `speed` | the same status, after the change the session accepted |
| `set_replay_speed` | `speed` | the same status, with only the multiplier changed |
| `set_replay_transport` | `control`, optional `records`, optional `speed` | the same status, after play, pause, step, fast-forward or stop |
| `query_journal` | optional `filter` | the trade journal, newest first, filtered and paged |
| `journal_totals` | optional `filter` | what that whole filter adds up to, in lamports and counts |
| `journal_trade_detail` | `tradeId` | one trade with its fills, routes, tips and signatures |
| `query_state_log` | `filter` | the forensic log — one row per launch the gate read — paged, by revision |
| `state_funnel` | `filter` | the same filter counted: rows, entries, refusals, deferrals, and every gate reason |
| `journal_snapshots` | `mode` | every checkpoint of the book in that mode, oldest first, with its digest |
| `verify_journal_chain` | `mode` | every link in the checkpoint chain that did not verify — empty is the answer |
| `journal_warm_start` | `mode` | where the counter is, what the newest checkpoint is worth now, how much sits on top of it |
| `get_alert_status` | — | the thresholds in force, what has fired, by kind, and every webhook's counters |
| `set_alert_thresholds` | `thresholds` | the new status, or a refusal if the set contradicts itself |
| `stream_alerts` | a channel | a subscription id, then alerts on that channel until the window closes |

`trigger_emergency_unwind` reports at two levels and they answer different
questions. `exitsSent` is what *this* press dispatched, `exitsAlreadyOut` is what
an earlier press left flying and this one found, and `exitsConfirmed` is what
actually closed. None of them says which position is which — that is
`stranded[].exit.onNetwork`, and it is the field to branch on. A window reading
`exitsSent` alone tells the operator nothing was ever sold on the second press,
while a transaction of theirs is still in the air.

`set_replay_playback` refuses to start while a feed endpoint is connected. The
session plays a recording into the replay clock and into the cockpit, not into
ingestion — §5's `FixtureDialer` is a different seam and is not built — so a
fixture started over a live feed would leave live candidates filling the panes
under a bar saying they were recorded. The fixture is read from
`$STS_REPLAY_FIXTURE`, or from `fixtures/` beside `sts.db` when that is unset,
and it is read when playback is first asked for rather than at startup.

The three journal commands take the same filter and answer different questions
with it. `query_journal` returns a page — clamped to five thousand rows however
large the ask, because a window that asks for everything has not asked a
question — and `journal_totals` returns what the *whole* filter comes to, which
is why they are separate: an operator looking at the first fifty of nine hundred
losses wants the nine hundred.

The five forensic commands are the other half of that, and `state_funnel` is the
one to reach for first. It returns the same shape `daemon::Funnel` prints — one
entry per gate reason, in a fixed order, with a zero rather than a missing row
for a reason nobody hit — so the funnel over a live week and the funnel in a
backtest report can be read side by side. `query_state_log` requires its `mode`
where the journal filters make it optional: the three modes are three
independent revision sequences, and a page mixing them is ordered by nothing.

`journal_warm_start` is what a person checks after an unclean shutdown, and its
verdict has three arms rather than two. `superseded` means the checkpoint was
true when it was taken and the log has moved on, which is what a checkpoint
looks like from any moment after it — not a failure. `diverged` means the log
has *not* moved and the book is not what the checkpoint says, which no code path
in this build can produce.

Nothing has to be called to keep the book up to date. The exit path writes it as
it goes: `Flattener` opens the trade before it signs, records the signature
before the bytes go out, and records the fill, the route and the closed trade
when it lands — so the journal is complete because nobody has to remember a call
site, which is the only way a journal stays complete. The trade is keyed by the
**position's** intent id and not the exit's, so one position sold on the third
attempt is one row with three signatures under it rather than three trades, two
of which never closed.

`stream_alerts` is a second channel beside `stream_telemetry`, not a replacement.
Every alert is published to the telemetry hub as well, at the level its severity
maps to, so a window can take one feed or both — but a pane that only wants the
things somebody has to act on should not have to filter a firehose to find them.

Alerts are raised from the same place, against the same values: `run()` hands
the dispatcher to the engine at startup, and every fill and every settled
signature is held against the thresholds as it is written. The alert and the row
are built from one `FillRow`, so an alert saying a fill came in 900 bps under
its quote and a book saying 400 is a disagreement the shape makes impossible. An
engine with no dispatcher attached still writes the book — it simply has nowhere
to say that a fill came in badly.

A webhook never runs on the engine's thread. `deliver` is a `try_send` onto a
bounded queue and a worker of its own does the connecting, the retrying and the
waiting: failures that might not repeat (a `503`, a refused connection, a
timeout) go out again on a doubling backoff, failures the endpoint decided (a
`404`, a `401`) do not, and an endpoint that fails `failures_before_open` times
in a row trips a breaker so the alerts behind it are dropped immediately rather
than aging behind a queue of retries into something switched off. Every one of
those is a counter on `get_alert_status`.

`stream_telemetry` uses a Tauri channel rather than a polling loop, so the UI
asks once and events arrive as they happen:

```js
import { invoke, Channel } from '@tauri-apps/api/core';

const events = new Channel();
events.onmessage = (event) => console.log(event.seq, event.level, event.message);
await invoke('stream_telemetry', { onEvent: events });
```

Failures come back as `{ kind, message }` — switch on `kind`, show `message`.

## The three ideas worth knowing

**The kill switch only goes one way.** Pulling it halts the engine and writes a
`kill_switch` row into `audit_log`. There is no un-pull. Deciding it is safe to
start again is a judgement for a person restarting the process, not a button
next to the one that stopped it. Pulling it twice is not an error, and the
receipt says which press was the real one.

**A panic arms it.** Any panic on any thread halts the engine before the message
is printed. A process that panicked reached a state nobody wrote down what to do
about, and halting is the only safe reading of that. The panic path touches only
atomics and a best-effort database write with a 250ms timeout, because the thread
running it may already hold any lock in the process — the crash path must not be
able to hang the exit.

**Metrics are counts, telemetry is events.** `stream_telemetry` says a candidate
was dropped. `get_metrics` says four thousand were, which is the number somebody
fixes the machine over. Every counter behind it is an atomic in an array that
exists from startup, so recording one allocates nothing and locks nothing, and
reading them all takes no lock either — the window can poll `get_metrics` as
fast as it repaints without touching the engine's timing. Quantiles come back as
`null` rather than `0` when nothing has been measured, because a p99 of zero
reads as *instant* when it means *never*.

**Telemetry drops rather than blocks.** The queue holds 1024 events. If the UI
falls behind, the newest event is dropped and counted in `dropped`, and the gap
shows up as a jump in `seq`. A dropped frame is cheap; an engine stalled behind a
slow window is not.

## When two providers disagree

Three sockets watching one program deliver the same account write three times,
and the launch index keeps the first of them: one slot per account is news once.
Whether the other two were *copies* of that first one is a different question,
and it is the only question here that needs more than one provider to answer.

So a write the watermark calls a duplicate is checked against what was released
for that slot. The same state is corroboration and costs nothing. A different
state is a contradiction — two providers, one account, one slot, two answers.
It is counted, the most recent one is on `get_ingestion_metrics`, and telemetry
says it once, with the account, the slot, both providers and both fingerprints,
rather than once per tick for as long as the disagreement stands.

Nothing is rewritten. The write already released stays released, because there
is no third source here to break the tie and picking a winner would be inventing
one. What a divergence should cost an entry is Phase 2's decision; this layer's
job is to stop a disagreement being silently collapsed into agreement.

Only each provider's **first** write of a slot is compared, which is what makes
the number worth reading. A curve inside a launch burst is written several times
in one slot, every provider delivers every write, and two sockets interleave —
so comparing any two writes would report a disagreement on every busy slot. One
socket delivers one account's writes in the order the validator made them, so
each provider's first write of a slot is the same write, and comparing those
compares like with like.

Two things it deliberately does not claim. A provider running a few slots behind
is not disagreeing with anybody: there is no witness for a slot the account has
already moved past, so a late frame is dropped as stale and nothing is said
about it. And a slot walked back by a fork switch is forgotten rather than
defended — the winning fork's rewrite of it is a correction, not a second
opinion.

## The Geyser feed

Off unless asked for, like the metrics port and for a stronger reason: a live
feed is opened as a deliberate act or not at all. Set `STS_GEYSER_ENDPOINT` and
the process dials a Yellowstone gRPC stream beside the websocket feeds;
leave it unset and nothing is opened, nothing fails, and `get_geyser_metrics`
reports a column of honest zeroes.

```bash
STS_GEYSER_ENDPOINT=https://grpc.example.com \
STS_GEYSER_TOKEN=… \
STS_GEYSER_PROVIDER=triton \
cargo run --features geyser-grpc
```

The token goes on every request as `x-token` and is never logged; the endpoint
is reduced to `scheme://host/…` before it reaches a telemetry line, because
providers put the API key in the path. `STS_GEYSER_PROVIDER` is one of `helius`,
`quicknode` or `triton` and only labels the counters — an unrecognised name is
the default rather than a refusal to start.

**Without `--features geyser-grpc` the transport refuses.** Everything with
logic in it — the sub-slot sequencing, the re-org rollback, the reconnect
schedule — compiles and is tested with no gRPC at all; the feature adds tonic
and a second TLS stack, and that is a price paid by the build that dials a real
endpoint and by no other. It refuses out loud: `GeyserError::NoTransport` names
the missing feature, so a build that cannot dial says so instead of looking like
a feed with nothing on it. What the feature does *not* add is the proto crate's
default gzip and zstd codecs — this client negotiates neither, and one of them
is a vendored C build.

**The payload is never copied.** An account write is the highest-rate message on
the stream and its bytes are most of it. The proto crate is built with
`account-data-as-bytes`, so those bytes arrive as a slice of the codec's own
read buffer and travel to the curve decoder without an allocation or a memcpy in
between; a transaction's logs are moved out of the wire message rather than
cloned off it, which is why the pipeline takes its update by value. Both are
asserted by address in the tests, because a signature cannot promise it and a
dependency bump can undo it.

**The outbound half of the subscription stays open.** `subscribe` is a
bidirectional call. A client that sends its request and lets the stream end has
half-closed the connection, which Yellowstone reads as the subscriber leaving,
so the request goes down a channel that is held open for the life of the stream
and a keepalive travels down it every ten seconds. That keepalive is the
subscription itself with a ping attached rather than an empty request — on the
reading where an inbound request *amends* the subscription, an empty one is an
amendment to no filters at all, which would unsubscribe the feed while every
counter kept saying "connected".

**The subscription asks only for what something here can read.** Curves owned
by pump.fun and exactly the curve account's size, slot statuses at every status
rather than the subscribed commitment (the ledger needs `Dead` and the parent
transitions to see a fork), and pump.fun transactions. Pool-program accounts
were on it and are not any more: `pool_tick` exists, nothing constructs it, and
until something does, those writes crossed the wire and produced no tick. Pool
prices — and so any price for a token after it graduates off the bonding curve
— are therefore not in this stream yet. `foreignAccounts` is what that would
show up as if the subscription drifted back.

One filter is guarded rather than merely built. An accounts filter naming no
owner and no account does not subscribe to nothing on this wire format; it
matches everything the remaining filters admit. Clearing the owner list — which
is what switching a subscription off looks like — is the edit that turns it
maximally on, and it fails towards a bill rather than an error, so the empty
case is dropped before it can be sent.

**A failed dial says why.** A wrong token, a typo'd endpoint and a provider
genuinely down used to look identical from outside: `connectFailures` climbing,
no reason anywhere, and a retry loop going round in silence forever. The reason
now reaches telemetry — at `info` for the first failures and `warn` once four
have failed in a row, since by then it is an outage rather than a blip.

It is said when it is *news*: on the first failure, again whenever the reason
changes, and again after a reconnect, but not once per retry. The backoff tops
out at thirty seconds and stays there, so a provider down for an hour would
otherwise repeat one sentence a hundred times; how often is already on the
counters, published every five seconds, and this carries what.

Two things make that reason worth printing. `tonic::transport::Error` displays
as the literal string `transport error` and nothing else — refused, DNS, TLS,
timeout all live one or more links down the `source()` chain — so the chain is
walked and flattened, because surfacing the top level alone would look like an
answer without being one. And it is scrubbed first: the URL path and the token
are removed by value before the string exists, since providers put the API key
in the path and a log file outlives the process. The host survives, because it
is what makes the error mean anything.

**There are two feeds and one queue.** This is the part worth being clear about.
The Geyser stream does not get its own path to the engine: it is sequenced in
`subslot::TickRing` and then every released curve write is handed to the same
`IngestionManager`, through the same launch index, the same spam floor and the
same target window, onto the same two channels. A Geyser candidate and a pubsub
candidate are the same thing to everything downstream. Two producers with two
sets of thresholds would be two strategies wearing one name.

What the extra fields in `get_geyser_metrics` mean:

- **`ring`** — what ordering cost. `outOfOrderArrivals` is how often the network
  handed over an update older than the one before it; `late` is how often that
  happened *after* the update's slot had already been released, which is the
  loss the hold window exists to bound. `forcedReleases` is the ring giving up
  its safety window rather than shedding a curve write, and it should be zero
- **`reorgs`** and **`unwinds`** — fork switches the slot ledger caught, and the
  subset of those that arrived too late to roll back. The first is routine; the
  second means state downstream was built on a block that no longer exists, and
  it is published as a warning with the slot in it
- **`admitted`** against **`refused`** — curve writes offered to ingestion and
  the ones its filters turned away. A feed where `refused` is zero is a feed
  whose filters are not running
- **`foreignAccounts`** — account writes owned by a program with no decoder
  here. The subscription now asks only for pump.fun curves, so on a healthy
  feed this is zero and a number climbing off it means the server is sending
  what it was not asked for. It is kept out of `decodeFailures` deliberately:
  that counter means the wire format moved under us, and burying it under
  traffic that is behaving exactly as asked is how it stops being read

## Generating load

`loadgen` is a mock Geyser with a chain behind it: curves that trade along the
program's own constant product, slots that progress through their statuses,
forks that lose, and a delay wheel that displaces arrivals by a configurable
distance without ever delivering one before it was sent. It is reproducible from
a `u64` seed, and on a debug build it emits upwards of half a million updates a
second — an order of magnitude past the fifty thousand the ring tests require of
it.

`tests/geyser_tests.rs` is what it is for. Two configurations run the same load
at two jitter settings: `LoadConfig::ABSORBED`, where the displacement fits
inside the hold window and **no curve write is lost at all**, and
`LoadConfig::EXTREME`, where it is eight times wider than the window can cover
and the loss stops being zero and starts being *counted*. Both assert the same
thing about ordering — zero violations across forty thousand released events —
and both check that every generated write is still accounted for: released,
swallowed by the write-version guard, discarded by a rollback, or refused as
late. Nothing may simply disappear.

## The metrics port

Off unless asked for. Set `STS_METRICS_ADDR` and the process serves the same
snapshot `get_metrics` returns over plain HTTP:

```bash
STS_METRICS_ADDR=9464 cargo run
curl -s http://127.0.0.1:9464/metrics | jq .slots.processingUs
```

A bare number is a port on loopback; `127.0.0.1:9464` works too. **It refuses to
bind anywhere else** — engine internals are served to this machine or to nobody,
and an address that is not loopback is an error at startup rather than a port
quietly open to the network. `GET` and `HEAD` only, on `/metrics`, `/healthz`
and `/`; everything else is a 404 and every other method is a 405. Nothing about
it can change engine state.

If the port will not open — taken, unparseable, not this machine — the reason
goes to telemetry and the engine starts anyway. Monitoring is worth having; it
is not worth refusing to start a trading engine over.

Three families of number are in there:

- **the slot clock** — how long a tick took to handle, how far apart the ticks
  arrived, and how much each interval differed from the one before it, plus the
  slots that went by without a tick and the ones that arrived out of order
- **the feed** — delivered against dropped, split by reason, and how full the
  queue between the feed and the engine has been. Watch `overrunBps`, not
  `lossBps`: most of what a program feed sends is not a candidate and refusing
  it is the job, while a frame lost to a full queue is the engine failing to
  keep up
- **the executions** — how many intents are in flight, and how the exits are
  spread across the signer's states: constructed, signed, broadcast, confirmed,
  failed

## The database

`db.rs` owns `sts.db`: the connection, the schema, and everything that appends to
it. The Node process that used to create these tables is archived under
`docs/archive/legacy-node`, so a fresh checkout has no database until this side
makes one. There is no `npm run db:setup` step any more — `Database::open`
migrates the file up to the schema this build knows about.

`docs/architecture/SCHEMA.md` is the description this implements: four runtime
tables (`candidates`, `clusters`, `execution_logs`, `tick_metrics`),
`intent_transitions` as the exit ledger beside them, the five journal tables
`journal.rs` adds, the three `forensics.rs` adds, `audit_log`, and
`schema_migrations` recording what has been applied.

The journal is the one part of the file that is not a ledger. `execution_logs`
and `intent_transitions` answer "what happened, in order"; `journal_trades` and
its four children answer "what did this cost", which is a question neither can
answer without a join. It carries the quantities finer than a lamport — a fill's
price, as an integer at `10^-18` — and no `REAL` column at all.

`forensics.rs` answers the third question, which is the one a quiet week
actually raises: not what a trade cost, but why there were only four of them.
`journal_state_log` is one row per launch the gate read — the verdict, the
reason in `strategy::syndicate`'s own vocabulary, the evidence behind it, and
what the risk gate was saying at the same instant. Most of it is refusals, which
is the point: a file that records only the trades cannot tell a quiet week from
a broken detector.

Two things sit with it. `journal_revisions` is one monotonic counter per mode,
allocated inside the same transaction as the rows it stamps, and it is what the
log is ordered by instead of a wall clock — `now_ms` reads `SystemTime`, NTP
steps it, and a record ordered by a clock that can go backwards is scrambled
worst during exactly the minute worth reading. Per mode rather than per file, so
a replay's revisions do not depend on how much live traffic was flowing beside
it. `journal_snapshots` is a periodic SHA-256-chained checkpoint of the book, so
a restart does not have to add up a hundred thousand trades to know what it is
holding, and so an edit to a checkpoint breaks that row and every row after it.

The high-throughput end is `StateLogger`: a bounded queue and one writer thread,
one transaction per batch, `prepare_cached` throughout. `observe` is a
`try_send`, so nothing on the decision path waits for a disk. A full queue drops
and counts the drop — a forensic row is an annotation on a decision that is
already durable in the book and the two ledgers, so losing one under saturation
is survivable, and losing it *silently* is not.

`Engine`'s maintenance thread takes a checkpoint every five minutes over all
three modes and prunes the log at thirty days, and the pruner is not allowed to
reach above the newest checkpoint — a row no snapshot has accounted for is a row
whose disappearance would make the integrity check beside it read as a break
rather than as a prune.

Both `sts` and `sts daemon` run `forensics::verify_on_start` before taking work:
it walks each mode's chain, recomputes the newest checkpoint against the book,
and publishes plus writes an `audit_log` row when the two disagree. It repairs
nothing. No code path in this build can break a chain, so a break means the file
was edited by something outside it, and the useful response is a loud durable
record rather than a guess at what the numbers should have been.

That last rule started here and is now the whole file's: migration 4 moved the
six `REAL` columns that predated the journal onto integer units, so `sts.db` has
no column with `REAL` affinity anywhere. `tests/journal_execution.rs` checks it
against the live file — every column's declared type and every stored value —
rather than against the documentation. `SCHEMA.md` has the argument and the
before-and-after table.

Three things about it that are easy to get wrong:

**The pragmas are per-connection, not per-file.** Only `journal_mode` is stored
in the file. Every other setting — including `foreign_keys`, which SQLite
defaults *off* on every connection forever — has to be set again by the next
connection that opens. `Database::pragmas()` reads back what actually took, and
`open` refuses a file that would not switch to WAL.

**Migrations run before anything reads, and a newer file is refused.** A build
that opened a database from a later version would be reading a schema it does not
know, which is the failure that corrupts things quietly. Not starting is the only
safe reading of it. An edited migration is caught the same way, by checksum.

**Duplicates are quiet; impossible rows are not.** Every batch writer names its
conflict target rather than saying `INSERT OR IGNORE`, because `OR IGNORE` skips
a row that violates *any* constraint — a cluster score past a whole unit or a
curve past 10000 basis points would be dropped as silently as a replayed frame.
Naming the identity means a replay is a no-op and a row that cannot be true
fails loudly.

The location follows `$STS_HOME`, like everything else that touches `data/`. The
fallback is resolved from this crate's path at compile time, which is right for
`cargo run` from a working copy and wrong for a packaged `.app` — a bundled build
is expected to set `STS_HOME`.

## The strategy

`src/strategy/` is the decision, and there is no floating point in it. Times are
in milliseconds, money in lamports, scores in millionths, shares in basis points,
and the one logarithm the entropy and attention terms need lives in `src/fixed.rs`
as a `u128` at `10^-18` — because a score that is stored, compared and replayed
must not depend on whose libm the build linked against. That kernel sits outside
`strategy/` because the execution side prices Jito tips with it too.

`syndicate.rs` reads a launch and says whether the wallets that opened it are one
operator: repeated position sizes, wallets landing in the same instant, and money
traced back to a shared funder, weighted into one confidence and a list of tags —
then two refusals that are not about coordination, the ring check and the
sandwich guard. `social.rs` reads the story the launch links to and turns it into a multiplier
that is **never above one** — a story can take size off a position and can never
put any on. `entry.rs` turns an accepted launch into a position: the size chain
from `docs/architecture/RISK_AND_SYBIL_SPEC.md` §10, the confidence tiers, the
gap and slippage stress buckets priced against the real curve, and the exit
quoted before the entry is agreed.

Three things about it that are easy to get wrong:

**The shipped policy trades nothing, and that is the correct state.**
`EntryParams::edge_lcb_bps` is the lower confidence bound on the edge, it
defaults to zero, and a zero edge cannot cover the cost of a round trip — so
every candidate is refused with `negative-stressed-ev`. The roadmap's Phase 3
gate is what produces a number for that field. Until it has, the rule computes,
explains and refuses, which is exactly what `execution.rs` does by shipping a
signer trait whose only implementation is an honest mock.

**A missing input is not a passing one.** A launch with no funding graph leaves
the funding term out of the confidence sum rather than scoring it zero; a launch
nobody scanned for a story leaves the multiplier at one; a scan that came back
unreadable does not. The three cases are different facts and the report keeps
them apart, because a zero in a column reads as "we looked and it was clean".

**Every refusal carries the numbers.** `GateVerdict` and `EntryDecision` are the
same shape whether they accepted or refused, so a funnel over a corpus can show
what each rung threw out and how big the position would have been. A rule that
took four trades out of three thousand launches is only interpretable next to
that table.

Nothing in the module reads a clock, a socket or a disk, and `strategy::decide`
takes the instant it is deciding at as an argument, so a replay produces the
decision the live run produced.

## What is not here

The size chain is not wired to the daemon. `daemon.rs` runs the analyser and the
gate on every launch it assembles, and then sizes the entry itself — the
operator's `--entry-lamports`, clipped by the participation cap — rather than
through `strategy::entry`. So the tiers, the stress set, the precomputed exit and
the expectancy gate are tested and callable and nothing in the running system
asks them yet.

That leaves two sizing paths in one binary, which is the thing to settle when
they are connected rather than after. The daemon's is deliberately simple and
belongs to a build with no signer in it; the chain in `strategy::entry` is the
one the roadmap's Phase 2 acceptance criteria are written against. They should
not both survive.

This also does not replace Electron. `app/main.cjs` still exists, still builds
the dmg, and still serves the UI through the Node dashboard on port 4747. This
window points at `../ui` as static files, so the parts of the UI that call the
dashboard's HTTP API will not work under Tauri until that is decided. The bundle
identifier is `fun.sts.desktop`, deliberately different from Electron's
`fun.sts.app`, so the two can be installed side by side while that is worked out.

The icons in `icons/` are flat placeholder squares, not artwork.
