# STS storage schema

> **STATUS NOTE — 27 August 2026.** The schema below is accurate. What is not
> obvious from it is how much of it has never held a row. `OperatingMode::Paper`
> is never constructed outside `#[cfg(test)]`, so the `'paper'` arm of every
> `mode` column is empty and always has been — **the engine has never produced a
> paper trade.** `candidates`, `positions`, `paper_trades`, `execution_logs`,
> `tick_metrics`, `clusters` and `forensic_snapshots` are all empty for the same
> family of reasons. Read a query against these tables as describing intent, not
> history. See [`../VERDICT-2026-08-27.md`](../VERDICT-2026-08-27.md).
>
> One control specified here was also never built: the note at line 46 that a
> value large enough to overflow "is a decode bug and should be rejected at the
> edge, not stored". Nothing rejects it, and 18.4% of captured coins carry a
> corrupt `virtualSolReserves` that this check would have caught.

This is the canonical description of `sts.db`, the SQLite file the engine writes
and the window reads. It covers how the connections are configured, the tables
the runtime depends on — the four original ones, the exit ledger, the five that
make up the trade journal, and the three that record why the book is as short as
it is — the indexes they carry, and the rules about growth and migration that
keep the file usable after a long session.

The file lives at `$STS_HOME/sts.db`, falling back to `data/sts.db` next to the
repository when `STS_HOME` is not set. `db.rs` resolves this; nothing else should
guess at the path.

## Who writes, who reads

One process writes. Inside it, one thread owns the write connection and every
other thread hands work to it through a channel. Readers — the UI queries, the
status snapshot, anything doing forensics on history — open their own
connections and never write.

This is not a style preference. SQLite in WAL mode allows exactly one writer at
a time, and a second writer turns every busy moment into a `SQLITE_BUSY` retry
storm on the path that is meant to be recording what just happened. One writer
means that contention cannot happen at all, rather than being handled.

WAL is what makes the reading side free: readers see a consistent snapshot from
the moment their statement started and are never blocked by the writer, and the
writer is never blocked by them.

## Conventions used by every table

**Time is epoch milliseconds in an `INTEGER` column.** Not seconds, not a
string, not SQLite's `CURRENT_TIMESTAMP`. One clock across the whole file means
two tables can be joined on time without conversion, and `telemetry::now_ms()`
is where that clock comes from.

**Keys and signatures are `TEXT`, base58, exactly as they appear on chain.**
Storing the 32 raw bytes would be smaller, but every query, every log line and
every explorer link needs the base58 form, and a schema that has to be decoded
before it can be read by a person is a schema nobody reads.

**Money is lamports in an `INTEGER` column.** SQLite's `INTEGER` is a signed
64-bit value, and total SOL supply in lamports is around 5.1 × 10^17, so every
real amount fits with three orders of magnitude to spare. Rust holds these as
`u64` and the cast to `i64` is safe for any value that came from the chain; a
value large enough to overflow it is a decode bug and should be rejected at the
edge, not stored.

**No column in this file is `REAL`, and no value stored in one is a float.**
Every fraction has an integer unit and is stored in it: basis points for
anything about money — a slippage bound, a fee, a share of a launch — millionths
for a normalised score, and `Q18` (a `u128` at `10^-18`, stored as its raw
`INTEGER` count) for a price. The unit is part of the column name wherever it is
not obvious: `_bps`, `_micros`, `_q18`, `_lamports`.

This used to be weaker. The original convention allowed a `REAL` for "a score
that is genuinely a 0-to-1 float", and six columns took it up. It was wrong for
three reasons that only showed up later. A float cannot be summed or compared
exactly, so a column holding one cannot answer the question it was added for. It
cannot survive a round trip, so "the row that went in is the row that came out"
stops being one `assert_eq!`. And it is not reproducible: `strategy::syndicate`
was rounding its scores to four decimal places purely to absorb the last-bit
difference a compiler's choice of fused multiply-add would otherwise put between
two runs of the same input — which is to say the float was costing determinism
and buying nothing, since the analyser had the exact millionths all along.
Migration 4 moved all six. See **Migrations** below for what each one became.

`tests/journal_execution.rs` enforces this against the live file rather than
against this document: it walks every column of every table and fails on any
whose declared type has `REAL` affinity, then walks every value and fails on any
whose `typeof()` is `real`. Both halves are needed — SQLite is dynamically
typed, so a float bound into a `NUMERIC` column stays a float — and the scan
uses `PRAGMA table_xinfo`, because `table_info` omits generated columns and a
check built on it would not see a `REAL GENERATED ALWAYS AS ...` sitting in
front of it.

**JSON payloads are `TEXT`.** SQLite has no JSON type — a column declared `JSON`
quietly gets `NUMERIC` affinity, which is the wrong home for a JSON string. The
`json_*()` functions work on `TEXT` regardless.

## Connection setup

Every connection, reader or writer, runs these on open. They are per-connection
settings unless noted, so setting them once somewhere central does not help the
next connection.

```sql
PRAGMA journal_mode = WAL;        -- persistent, stored in the file itself
PRAGMA synchronous  = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;       -- milliseconds
PRAGMA cache_size   = -65536;     -- negative means KiB, so 64 MiB
PRAGMA mmap_size    = 268435456;  -- 256 MiB
PRAGMA temp_store   = MEMORY;
PRAGMA wal_autocheckpoint = 4000; -- pages, so ~16 MiB at the 4 KiB page size
```

**`journal_mode = WAL`** is the one setting stored in the database file rather
than the connection, so it survives reopening and only needs setting once. It is
set on every open anyway, because a file that came back from a restore or a
manual `sqlite3` session in the wrong mode should be corrected on contact rather
than discovered later. Setting it is a no-op when it already matches.

WAL is the operating mode this system assumes, not a tuning option. Readers not
blocking the writer is what lets the UI query history while the ingest path is
committing, and the alternative — rollback journal — takes an exclusive lock for
the length of every write.

**`synchronous = NORMAL`** is the deliberate durability trade. In WAL mode,
`NORMAL` means the writer does not `fsync` on every commit; it syncs at
checkpoints. A process crash, a panic, or a `kill -9` loses nothing, because the
WAL is already written to the operating system. An OS crash or a power cut can
lose the last few commits.

That is acceptable here and it is worth being explicit about why: the durable
record of first resort is the append-only NDJSON audit log, which is written and
flushed separately, and the ingest stream is a live feed that will be behind
after any hard stop anyway. What must never be lost is the audit trail, and the
audit trail is not only in this file. `FULL` would cost an `fsync` per commit on
a path that commits several times a second, which is exactly the cost this
system cannot pay on the hot path.

**`foreign_keys = ON`** must be set per connection — SQLite defaults it off for
backwards compatibility, on every connection, forever. A connection that forgets
it silently skips every reference check.

**`busy_timeout = 5000`** turns a lost race into a slow one. With a single
writer the only contention left is a checkpoint against a long-running read, and
five seconds is far more than that needs. A timeout of zero would surface those
as errors on the write path, where there is nothing useful to do about them.

**`cache_size = -65536`** asks for 64 MiB of page cache per connection, replacing
the 2 MiB default. The default is small enough that one batch of inserts evicts
the index pages the next batch is about to need, so the same pages are read back
from disk over and over. This machine has 8 GB; three connections at 64 MiB is a
rounding error next to the read amplification it removes. The value is negative
on purpose: a positive `cache_size` counts pages and silently changes meaning if
the page size ever changes, while a negative one counts KiB and does not.

**`mmap_size = 268435456`** lets SQLite read up to 256 MiB of the database
through a memory mapping instead of `read()` calls, which removes a copy from
every page the readers touch. Three things to know about it: it caps at the
current file size, so a small file just uses less; it only helps reads, since
writes still go through the normal path; and an I/O error inside a mapping
arrives as a signal rather than an error code, which is why the size is bounded
rather than set to the whole file.

**`temp_store = MEMORY`** keeps the temporary B-trees that sorts and hash joins
build in memory rather than in a spill file. The queries here sort small result
sets — the newest few hundred rows of something — and a spill file for those is
pure latency.

**`wal_autocheckpoint = 4000`** raises the default of 1000 pages (about 4 MiB) to
about 16 MiB. Every automatic checkpoint runs on the connection that happened to
trip the threshold, which in practice is the writer, in the middle of a commit,
on the ingest path. Making the WAL four times larger before that happens trades
a little disk for four times fewer of those pauses.

The counterpart is that a WAL only shrinks when it is checkpointed, so the file
does not get to grow forever. Two rules cover it:

- A background task runs `PRAGMA wal_checkpoint(PASSIVE)` on a timer. `PASSIVE`
  copies what it can without ever blocking a reader or a writer and gives up on
  the rest, which is the right shape for something running on a schedule.
- Shutdown runs `PRAGMA wal_checkpoint(TRUNCATE)`, which folds the whole WAL
  back into the main file and resets it to zero length. `Database::close` in
  `db.rs` already does this. It is the one place a blocking checkpoint is
  correct, because there is nothing left to block.

A WAL that keeps growing through both of those means a reader is being held open
across a long operation — a checkpoint cannot advance past the oldest snapshot
still in use. That is a bug in the reader, and the WAL file size is how it gets
noticed.

## `candidates`

Every token the ingest path noticed and did not throw away. Append-only: a row
is one observation at one slot, not a token's current state. The same token
appears many times as its curve fills, and the newest row for a mint is its
latest known state.

Append-only matters because this table is written from the socket path at
several rows per second during a busy launch minute. Inserts at the end of a
B-tree are cheap and never wait; updates in place would turn every observation
into a read, a write and a possible page split on a path that is budgeted in
microseconds.

```sql
CREATE TABLE candidates (
  id                   INTEGER PRIMARY KEY AUTOINCREMENT,

  -- The requested core.
  mint                 TEXT,
  symbol               TEXT,
  curve_progress_bps   INTEGER NOT NULL
                         CHECK (curve_progress_bps BETWEEN 0 AND 10000),
  liquidity_lamports   INTEGER NOT NULL CHECK (liquidity_lamports >= 0),
  creator_wallet       TEXT,
  detected_slot        INTEGER NOT NULL CHECK (detected_slot > 0),
  fast_path_eligible   INTEGER NOT NULL CHECK (fast_path_eligible IN (0, 1)),

  -- Bookkeeping the core columns cannot do without.
  curve_account        TEXT NOT NULL,
  program              TEXT NOT NULL,
  source               TEXT NOT NULL,
  market_cap_usd_cents INTEGER CHECK (market_cap_usd_cents >= 0),
  observed_at_ms       INTEGER NOT NULL,
  dispatch_latency_us  INTEGER CHECK (dispatch_latency_us >= 0)
);
```

`curve_progress_bps` is the only spelling of how far along the curve is, and
that is deliberate. The engine works in basis points end to end —
`TokenCandidate::curve_progress_bps` is a `u16`, and 10000 is a graduated curve
— and storing a percentage beside it invites the two to disagree after some
future writer updates one of them.

There used to be a `bonding_curve_pct REAL GENERATED ALWAYS AS
(curve_progress_bps / 100.0) VIRTUAL` here, on the argument that a generated
column costs no space and cannot drift. That much was true; what was not is that
it was worth having. It was read by nothing but its own test, it was the file's
only remaining `REAL` outside `clusters` and `tick_metrics`, and the number it
offered is one division away from the column beside it. Migration 4 dropped it.
A caller who wants a percentage can divide by 100.

`mint` and `symbol` are nullable, and this is the schema's most important
caveat. The ingest path identifies a launch by its **bonding curve account**,
because that is what the account update carries. The curve is a program-derived
address of the mint, and that derivation only runs one way: from the mint you
can compute the curve, from the curve you cannot recover the mint. Resolving the
mint needs the create instruction, which arrives on a different subscription.
The symbol is worse — it is whatever the creator typed, including nothing.

So rows land with `curve_account` populated and `mint` null, and a later pass
fills the mint in once the create instruction has been matched. Anything joining
on `mint` must expect nulls and must not treat a null as a distinct token.
`curve_account` is the identity that is always present.

`creator_wallet` is nullable for a narrower reason: older curve account layouts
do not carry a creator, and the decoder writes null rather than an all-zero key.
An all-zero key would read as a launch by the System Program, which is a
different and false claim. `TokenCandidate::has_known_creator` is the same check
on the Rust side.

`fast_path_eligible` records the routing decision made at ingest — whether the
candidate went to the shallow fast-path queue or the deeper standard one. It is
a record of what happened, not a judgement to be recomputed later: re-deriving
it from stored thresholds would silently rewrite history every time a threshold
changed.

`symbol` is untrusted text from the create instruction. It is stored as it
arrived and must be escaped wherever it is displayed. It is not an identifier
and nothing should key on it.

### Indexes on `candidates`

```sql
CREATE UNIQUE INDEX candidates_observation
  ON candidates (source, curve_account, detected_slot);

CREATE INDEX candidates_recent
  ON candidates (observed_at_ms DESC);

CREATE INDEX candidates_mint
  ON candidates (mint, observed_at_ms DESC)
  WHERE mint IS NOT NULL;

CREATE INDEX candidates_creator
  ON candidates (creator_wallet, observed_at_ms DESC)
  WHERE creator_wallet IS NOT NULL;

CREATE INDEX candidates_fast_path
  ON candidates (observed_at_ms DESC)
  WHERE fast_path_eligible = 1;
```

`candidates_observation` is what makes the writer idempotent. A reconnect
replays whatever the socket buffered, and a replayed fixture has to land the same
way twice. Deduplication happens in the index, where it costs one B-tree probe,
rather than in a query.

`source` leads the index, and that is a deliberate choice about what an
observation *is*. Two providers reporting the same account at the same slot are
two observations, not one duplicate: their agreement is evidence about the feed,
and collapsing them would throw away the only signal that says a provider went
quiet or started lying. A reconnect replaying its own buffer still collapses,
because that is the same provider twice. The cost is that a fully agreed launch
is stored up to three times, which is three small rows against a table already
sized for several per second.

The writer names the conflict target rather than saying `INSERT OR IGNORE`:

```sql
INSERT INTO candidates (...) VALUES (...)
ON CONFLICT (source, curve_account, detected_slot) DO NOTHING;
```

The difference is not cosmetic. `OR IGNORE` skips a row that violates *any*
constraint, so a curve past 10000 basis points or a negative pool would be
dropped exactly as quietly as a duplicate — the ingest path would report a clean
run while silently discarding malformed rows. Naming the identity means only a
duplicate is quiet and anything that cannot be true fails the batch. The same
applies to `clusters`, where `OR IGNORE` would swallow the `NOT NULL` and `CHECK`
pair the next section is entirely about.

`candidates_recent` serves the radar pane, which is one query — the newest N
rows — run constantly. `DESC` in the index definition matters: SQLite can walk
an ascending index backwards, but stating the order the reader wants keeps the
plan a plain scan of a leading edge with no sort step.

`candidates_mint` and `candidates_creator` are partial, and the `WHERE` clause
is doing real work. Most rows have no mint yet and many have no creator; a full
index would store an entry for every one of them, all sorting together under
null, where they are never looked up. The partial index holds only the rows a
lookup by mint or creator can actually find. The trade is that SQLite only uses
a partial index when the query's `WHERE` clause implies the index's, so a query
must say `WHERE mint = ?` — which already implies non-null — and not
`WHERE mint IS ?`.

`candidates_fast_path` is the same idea for the one filter the UI toggles.

Five indexes on a table written from the hot path is close to the limit. Every
one of them is another B-tree to update per insert, and the batching in the WAL
writer — collect up to a batch, then commit once — is what keeps that affordable.
A sixth index needs a query that justifies it, not a query that might.

## `clusters`

The output of the wallet clustering pass: one row per cluster of wallets that
look like one hand, with the numbers that led to that reading.

Unlike `candidates`, this is derived intelligence. Every row is reproducible from
the inputs and the version of the heuristic that produced it, and rows are
replaced by a recomputation rather than edited.

```sql
CREATE TABLE clusters (
  cluster_id           TEXT NOT NULL,
  version              INTEGER NOT NULL,

  root_wallet          TEXT NOT NULL,
  wallet_count         INTEGER NOT NULL CHECK (wallet_count >= 1),

  hhi                  INTEGER NOT NULL CHECK (hhi BETWEEN 0 AND 10000),
  temporal_influence_micros  INTEGER NOT NULL
                               CHECK (temporal_influence_micros BETWEEN 0 AND 1000000),
  spectral_separation_micros INTEGER NOT NULL
                               CHECK (spectral_separation_micros BETWEEN 0 AND 1000000),
  interaction_entropy_micros INTEGER NOT NULL
                               CHECK (interaction_entropy_micros BETWEEN 0 AND 1000000),

  flag_sybil           INTEGER NOT NULL CHECK (flag_sybil IN (0, 1)),
  computed_at_ms       INTEGER NOT NULL,

  PRIMARY KEY (cluster_id, version)
);
```

`hhi` is the Herfindahl-Hirschman index over how the cluster's holdings are
split between its wallets, **in basis points**: 10000 is one wallet holding
everything, a few hundred is a genuinely spread-out crowd. The column is named
`hhi` and typed as an integer for the same reason `curve_progress_bps` is — it
is what `SybilClusterMetrics::holding_hhi_bps` computes, and storing a rounded
percentage would throw away resolution the EV engine uses.

The three score columns are normalised to millionths — 0 to 1_000_000 — and all
have both `NOT NULL` and a `CHECK` saying so.

Millionths and not a `REAL`, which is what they were until migration 4, and not
basis points either. `strategy::syndicate` computes all three as `u64`
millionths and used to round them into a float on the way past; storing what it
already had deletes that step, and with it the only place on this path where two
runs of the same input could disagree. Basis points would be too coarse: an
entropy or an eigenvalue gap moves in the fourth decimal place, and rounding one
to a basis point would put two genuinely different clusters on the same reading.

The `NOT NULL` and the `CHECK` are not belt-and-braces. They catch different
things, and the split is simpler than it was:

- **A score past a whole unit** — the ratio a degenerate graph can produce that
  comes out a hair over one — is out of range, and the `CHECK` rejects the row.
- **A score nobody measured** is `NULL`, and `NOT NULL` rejects it. This is the
  one that matters: `syndicate::Cluster::metrics_with` answers `None` when any
  input is UNKNOWN, because a zero in a missing column would make an unmeasured
  cluster indistinguishable from a measured clean one.

There is no longer a NaN or an infinity to defend against, and that is the point
of the unit change rather than a gap in it: an integer column has no way to
spell either. Before migration 4 a NaN was converted silently to `NULL` on the
way in and an infinity stored as an out-of-range `REAL`, and both would have
left a cluster looking clean because the maths broke.

- `temporal_influence_micros` — how tightly the cluster's buys land in the same
  moment. Fifty wallets buying within one slot of each other is one hand, not
  fifty.
- `spectral_separation_micros` — how cleanly the cluster separates from the rest
  of the transfer graph. High means these wallets talk to each other far more
  than to anyone else.
- `interaction_entropy_micros` — Shannon entropy of who transacts with whom
  inside the cluster. Low means every path runs through one funder.

`flag_sybil` is a judgement, and the only column here that is not a measurement.
It exists so the UI has something to filter on and the audit trail has something
to point at, but nothing downstream may treat it as evidence on its own. The
four numbers above are the evidence; the flag is a threshold applied to them,
and thresholds belong to the EV engine and change. This is why `version` is part
of the primary key: a recomputation writes a new version rather than overwriting
the old one, so a decision made last week can still be explained with the
numbers that were actually in front of it.

`wallet_count` allows 1 so that a singleton cluster can be recorded, but a
cluster of one wallet has nothing to measure — its scores are whatever the maths
produced and mean nothing. `SybilClusterMetrics::is_measurable` is the
`wallet_count >= 2` check, and any query feeding a decision must apply it.

### Indexes on `clusters`

```sql
CREATE INDEX clusters_root_wallet
  ON clusters (root_wallet, version DESC);

CREATE INDEX clusters_flagged
  ON clusters (computed_at_ms DESC)
  WHERE flag_sybil = 1;

CREATE INDEX clusters_recent
  ON clusters (computed_at_ms DESC);
```

The primary key already covers "the history of this cluster" and "this exact
version". `clusters_root_wallet` covers the question the forensic pane actually
asks, which is the other direction: given a wallet, what cluster does it root,
newest analysis first.

`clusters_flagged` is partial because flagged clusters are the small minority
and the review queue only ever wants those. A full index on `flag_sybil` would
be two enormous runs of identical values, which is the shape an index is worst
at.

This table is written by a batch pass, not the hot path, so an index here costs
much less than one on `candidates`.

## `execution_logs`

Every state transition of every execution, append-only. **Nothing in this table
is ever updated or deleted.** One order produces one row per step it takes, and
the sequence of rows is the order's history.

This is the difference between a log and a status table. A status table tells
you where an order is now; this tells you how it got there, which is what a
post-mortem, a replay and a reconciliation all need. Where an order is now is
the newest row for its `intent_id`.

```sql
CREATE TABLE execution_logs (
  intent_id     TEXT NOT NULL,
  seq           INTEGER NOT NULL CHECK (seq >= 0),

  mint          TEXT NOT NULL,
  state         TEXT NOT NULL CHECK (state IN (
                  'intent_created', 'validated', 'sent',
                  'confirmed', 'completed', 'aborted')),
  prev_state    TEXT CHECK (prev_state IN (
                  'intent_created', 'validated', 'sent',
                  'confirmed', 'completed', 'aborted')),
  side          TEXT NOT NULL CHECK (side IN ('buy', 'sell')),
  size_lamports INTEGER NOT NULL CHECK (size_lamports > 0),
  price_q18     INTEGER CHECK (price_q18 IS NULL OR price_q18 > 0),
  signature     TEXT,
  latency_ms    INTEGER CHECK (latency_ms IS NULL OR latency_ms >= 0),
  needs_unwind  INTEGER NOT NULL DEFAULT 0 CHECK (needs_unwind IN (0, 1)),

  mode          TEXT NOT NULL CHECK (mode IN ('live', 'paper', 'replay')),
  abort_reason  TEXT CHECK (abort_reason IN (
                  'risk_gate', 'circuit_breaker', 'kill_switch', 'stale',
                  'send_failed', 'not_confirmed', 'operator')),
  at_ms         INTEGER NOT NULL,

  PRIMARY KEY (intent_id, seq)
);
```

`intent_id` is a UUIDv7 minted when the EV engine decides it wants to do
something, and it is the correlation ID that ties this row to the audit NDJSON,
the telemetry event and the risk decision that allowed it. Version 7 because it
sorts by creation time, so an index on it is an index on time as well.

`seq` counts transitions within one intent, starting at 0. Together they are the
primary key, which gives idempotency for free: a retry that writes the same step
twice violates the key and is rejected, rather than producing a history with a
duplicated step in it.

The `state` values are exactly what `ExecutionState::as_str` writes, and
`ExecutionState::from_str` reads them back. A row whose state this build does not
recognise is not a state to guess at — `from_str` returns `None` and the caller
must treat the row as unreadable rather than default it to something.

The `CHECK` on `state` is a backstop and nothing more. The real rule is the state
machine in `types.rs`, which allows the forward path one step at a time —
`intent_created` → `validated` → `sent` → `confirmed` → `completed` — plus an
edge to `aborted` from any active state. A `sent` that was never `validated`
means the risk gate was bypassed. SQL cannot express that; only the writer can,
which is why every transition goes through `ExecutionState::transition_to` and
the row is written from its result.

`needs_unwind` is the column to be most careful about. Aborting an execution
that had already reached `sent` or `confirmed` does not sell the position —
there is no transaction that un-sends another one. It stops the engine managing
it, and something is left on chain that a person still has to flatten. That is
what this flag records, and it is set from `AbortOutcome::needs_unwind`, which
is `true` exactly when the aborted state had money at risk.

A row with `needs_unwind = 1` is an open obligation. It is never cleared,
because the row is history; the unwind is recorded as its own audit event and,
if it went through the engine, its own intent. The operator surface reads the
open obligations with the partial index below.

`price_q18` is nullable because the states before a fill have no price. It is
lamports per token base unit floored to `10^-18`, stored as its raw `INTEGER`
count — the same unit and the same argument as `journal_fills.price_q18`, which
is the column this one should always have been. Migration 4 converted it from
the `REAL` it was; nothing had ever read it back, so this turned a write-only
column into one that means something. `signature`
is nullable for the same reason and stays null on any execution that was aborted
before `sent`. `latency_ms` is measured against the previous transition, not
against the intent's creation, so the steps can be added up but a slow step can
also be found on its own.

`mode` is on every row rather than inferred from a session, because paper and
replay results are only worth anything if they came off the same code path as
live ones — which means the rows sit in the same table, and the only thing
separating them is this column. Every query that reports performance must filter
on it. A query that forgets is reporting paper trades as real ones.

### Indexes on `execution_logs`

```sql
CREATE INDEX execution_logs_at
  ON execution_logs (at_ms DESC);

CREATE INDEX execution_logs_mint
  ON execution_logs (mint, at_ms DESC);

CREATE INDEX execution_logs_unwind
  ON execution_logs (at_ms DESC)
  WHERE needs_unwind = 1;

CREATE INDEX execution_logs_open
  ON execution_logs (state, at_ms DESC)
  WHERE state IN ('sent', 'confirmed');

CREATE UNIQUE INDEX execution_logs_signature
  ON execution_logs (signature)
  WHERE signature IS NOT NULL;
```

`execution_logs_at` is the execution deck's feed, newest first.

`execution_logs_unwind` is the one that matters most and holds the fewest rows.
Positions needing a manual unwind are rare and urgent, and a partial index makes
finding them a lookup into a tiny B-tree instead of a scan of everything that
has ever executed. It stays fast in year two.

`execution_logs_open` narrows to the two states that have money at risk, which
is how the risk governor counts open exposure without reading history. Note that
`WHERE state IN ('sent', 'confirmed')` is two lookups rather than one ordered
run, so adding `ORDER BY at_ms DESC` to it costs a small sort. That is fine at
this size — the whole point of the index is that it holds only open positions —
but a query that wants ordering for free should ask for one state at a time.

`execution_logs_signature` is unique because one on-chain signature belongs to
one execution. If two rows claim the same signature, either the same
transaction was recorded twice or two intents believe they own one position —
both are serious, and both should fail at the insert rather than be found later
in a reconciliation. Partial, because unsent executions have no signature and
there can be many of those.

## `intent_transitions`

Every step in the life of every exit transaction, append-only. **Nothing in this
table is ever updated or deleted**, on the same terms as `execution_logs`.

The two tables sit at different altitudes and neither replaces the other.
`execution_logs` records the six-state machine in `types.rs` — what an intent
meant to do and how far it got. This records what one *transaction* did on the
way out of a position: `exit_constructed` → `exit_signed` → `exit_broadcast` →
`exit_confirmed`, or `exit_failed` from any of them. Those names are not in
`execution_logs`'s `CHECK` and should not be: `exit_signed` in particular has no
analogue up there, and it is the state that matters most, because a transaction
that is signed but never broadcast is the one case where a complete, spendable
instruction set exists and yet nothing is on the network.

An exit writes both tables. Its coarse steps go to `execution_logs` as a new
intent with `side = 'sell'`, because `RISK_AND_SYBIL_SPEC.md` U2 makes a
resolved obligation new rows and never an edit to the old ones. Its fine steps
come here, keyed by the same `intent_id`, with `origin_intent_id` naming the
obligation being flattened — that column is the only thing joining the two
halves of one unwind back together.

```sql
CREATE TABLE intent_transitions (
  intent_id             TEXT NOT NULL,
  seq                   INTEGER NOT NULL CHECK (seq >= 0),

  origin_intent_id      TEXT NOT NULL,
  from_state            TEXT CHECK (from_state IN (
                          'exit_constructed', 'exit_signed', 'exit_broadcast',
                          'exit_confirmed', 'exit_failed')),
  to_state              TEXT NOT NULL CHECK (to_state IN (
                          'exit_constructed', 'exit_signed', 'exit_broadcast',
                          'exit_confirmed', 'exit_failed')),

  venue                 TEXT CHECK (venue IS NULL OR venue IN (
                          'pump_fun_curve', 'raydium_amm_v4')),
  mint                  TEXT NOT NULL,
  tokens                INTEGER CHECK (tokens IS NULL OR tokens > 0),
  min_out_lamports      INTEGER CHECK (min_out_lamports IS NULL OR min_out_lamports >= 0),
  out_lamports          INTEGER CHECK (out_lamports IS NULL OR out_lamports >= 0),
  cost_basis_lamports   INTEGER NOT NULL CHECK (cost_basis_lamports >= 0),
  realized_pnl_lamports INTEGER,

  signature             TEXT,
  failure               TEXT CHECK (failure IN (
                          'no_route', 'construction', 'signing', 'broadcast',
                          'not_confirmed', 'shutting_down')),
  detail                TEXT,
  mode                  TEXT NOT NULL CHECK (mode IN ('live', 'paper', 'replay')),
  at_ms                 INTEGER NOT NULL,

  PRIMARY KEY (intent_id, seq),

  CHECK ((to_state = 'exit_failed' AND failure IS NOT NULL)
      OR (to_state <> 'exit_failed' AND failure IS NULL)),
  CHECK (realized_pnl_lamports IS NULL OR out_lamports IS NOT NULL)
);
```

`signature` sits on exactly one row, the `exit_signed` step, for the reason
`execution_logs.signature` does: the unique partial index below means one
signature is one row forever, so it cannot be carried forward onto the broadcast
or the confirmation. **It is written before the broadcast, not after.** A
process that dies between those two has to come back knowing a transaction with
that signature may be on the network; the alternative is a reconciliation that
concludes nothing went out and sells the position a second time.

`venue`, `tokens` and `min_out_lamports` are nullable because an exit can fail
before it is routed anywhere — a depleted pool, a curve that has graduated — and
there is no venue, no size and no floor for something that was never built. A
zero in those columns would read as a number somebody computed.

`realized_pnl_lamports` is `out_lamports - cost_basis_lamports` and exists only
on the `exit_confirmed` step. It is stored rather than derived on read so the
number that was true at the time survives a later change to how it is computed,
and the second `CHECK` is what stops it existing without proceeds behind it. An
exit that is merely on the network has realized nothing; counting one as a
realized number is how unrealized gains end up in a total labelled realized.

`cost_basis_lamports` comes from the obligation's own `size_lamports`. It is on
every row rather than only the last so a failed exit still records what was at
stake.

The two `CHECK`s at the bottom are there because both cases are rows that cannot
be read back honestly: a failure with no reason, and a profit with nothing
behind it.

### Indexes on `intent_transitions`

```sql
CREATE INDEX intent_transitions_at
  ON intent_transitions (at_ms DESC);

CREATE INDEX intent_transitions_origin
  ON intent_transitions (origin_intent_id, at_ms DESC);

CREATE INDEX intent_transitions_closed
  ON intent_transitions (mode, at_ms DESC)
  WHERE to_state = 'exit_confirmed';

CREATE UNIQUE INDEX intent_transitions_signature
  ON intent_transitions (signature)
  WHERE signature IS NOT NULL;
```

`intent_transitions_origin` is the one the unwind path runs before it builds
anything: given an obligation, has an exit already been sent for it? An unwind
that cannot answer that sends a second exit, and the second sale is of tokens
the wallet no longer holds.

`intent_transitions_closed` is partial and keyed on `mode` because every query
that reports performance must filter on it. Realized PnL is summed per mode and
never across them — one total over three modes is paper trades reported as real
ones, with no way to notice.

`intent_transitions_signature` is unique for exactly the reason
`execution_logs_signature` is: two exits claiming one transaction is a position
sold twice, and it should fail at the insert rather than be found later.

## The trade journal

Five tables, added by migration 3, that answer a question the two ledgers above
cannot: what did this trade cost. `execution_logs` records the state machine an
intent walks and `intent_transitions` records the finer one an exit transaction
walks; both are arranged around *what happened, in order*, and getting the money
out of them takes a join nobody wants to write twice.

So the journal is the book. `journal_trades` is one row per trade with the money
on it, and `journal_fills`, `journal_routes`, `journal_tips` and
`journal_signatures` are the four things that decided it, hanging off that row.
It does not replace either ledger and it is not derived from them by a job — the
execution path writes all three as it goes.

### Two rules these tables were the first to follow

**No column in these five tables is `REAL`.** This was the stricter rule when
the journal was added, and it is now the rule for the whole file — see
**Conventions** above. It started here because a book is where a float hurts
first: every number in these five tables is money, a quantity of money, or a
ratio between two of them, and none of those can be summed, compared or round
tripped through a `REAL`. `execution_logs.price` was the column in the older
schema this one deliberately did not repeat, and migration 4 went back and made
it `execution_logs.price_q18` instead.

**No key is generated.** Every primary key here is supplied by the caller —
`trade_id` is the intent id, and the child tables key off it plus a sequence the
execution path assigns. An `INTEGER PRIMARY KEY AUTOINCREMENT` would number rows
by the order they were inserted, and Phase 3's acceptance criterion is that one
fixture and one seed produce byte-identical records; a key that depends on how
many trades happened to come first fails that on every second run. It also makes
a replay idempotent: the second pass conflicts with the first on every row.

### `journal_trades`

```sql
CREATE TABLE journal_trades (
  trade_id              TEXT    PRIMARY KEY,
  mint                  TEXT    NOT NULL,
  side                  TEXT    NOT NULL CHECK (side IN ('buy', 'sell')),
  mode                  TEXT    NOT NULL CHECK (mode IN ('live', 'paper', 'replay')),
  venue                 TEXT    CHECK (venue IS NULL OR venue IN (
                                  'pump_fun_curve', 'raydium_amm_v4')),
  notional_lamports     INTEGER NOT NULL CHECK (notional_lamports >= 0),
  tokens                INTEGER NOT NULL CHECK (tokens >= 0),
  cost_basis_lamports   INTEGER NOT NULL CHECK (cost_basis_lamports >= 0),
  proceeds_lamports     INTEGER CHECK (proceeds_lamports IS NULL OR proceeds_lamports >= 0),
  realized_pnl_lamports INTEGER,
  fee_lamports          INTEGER NOT NULL DEFAULT 0 CHECK (fee_lamports >= 0),
  tip_lamports          INTEGER NOT NULL DEFAULT 0 CHECK (tip_lamports >= 0),
  slippage_bps          INTEGER CHECK (slippage_bps IS NULL
                                  OR (slippage_bps >= 0 AND slippage_bps <= 10000)),
  opened_at_ms          INTEGER NOT NULL,
  closed_at_ms          INTEGER CHECK (closed_at_ms IS NULL OR closed_at_ms >= opened_at_ms),

  CHECK (realized_pnl_lamports IS NULL OR proceeds_lamports IS NOT NULL),
  CHECK ((closed_at_ms IS NULL AND proceeds_lamports IS NULL)
      OR (closed_at_ms IS NOT NULL AND proceeds_lamports IS NOT NULL))
);
```

`venue` is null until a route is chosen; naming one before that would be
inventing where the money would have gone. `proceeds_lamports` is null while the
position is open, and zero is a different fact — the sale returned nothing. The
two `CHECK`s at the bottom are the same argument `intent_transitions` makes about
its own: profit with no proceeds is not a number anybody computed, and a closed
trade with nothing back is a row that cannot be read honestly.

`realized_pnl_lamports` is proceeds less cost less fees less tips. The tip is a
cost of the trade: it was paid to land the exit, and a book that reported profit
before it would be reporting a number that never reached the wallet.

This is the one table in the file that is updated rather than only appended to —
a trade opens and later closes, and both are the same row. What the trade *is*
still cannot change, and a trigger rather than a convention is what makes that
true:

```sql
CREATE TRIGGER journal_trades_identity_is_immutable
  BEFORE UPDATE ON journal_trades
  WHEN old.mint <> new.mint
    OR old.side <> new.side
    OR old.mode <> new.mode
    OR old.opened_at_ms <> new.opened_at_ms
BEGIN
  SELECT RAISE(ABORT, 'a journal trade cannot change what it is');
END;
```

The upsert in `record_journal_trades` assigns those four columns along with
everything else, which is what makes the trigger load-bearing rather than
decorative: a column that is never assigned never differs from itself, and
leaving them out of the `SET` list would turn a write that changed the mint into
a write that quietly kept the old one.

#### Indexes on `journal_trades`

```sql
CREATE INDEX journal_trades_mode     ON journal_trades (mode, opened_at_ms DESC);
CREATE INDEX journal_trades_mint     ON journal_trades (mint, opened_at_ms DESC);
CREATE INDEX journal_trades_venue    ON journal_trades (venue, opened_at_ms DESC)
  WHERE venue IS NOT NULL;
CREATE INDEX journal_trades_closed   ON journal_trades (mode, closed_at_ms DESC)
  WHERE closed_at_ms IS NOT NULL;
CREATE INDEX journal_trades_open     ON journal_trades (mode, opened_at_ms DESC)
  WHERE closed_at_ms IS NULL;
CREATE INDEX journal_trades_slippage ON journal_trades (slippage_bps DESC, opened_at_ms DESC)
  WHERE slippage_bps IS NOT NULL;
```

Every one of them carries `opened_at_ms DESC` as its tail, because the journal
is always read newest-first and an index that orders the filter but not the sort
leaves SQLite building a temporary B-tree for the `ORDER BY`. The two partial
ones on `closed_at_ms` split the table the way the questions do: "what is still
open" and "what did the closed ones come to" are asked far more often than
anything that wants both.

### `journal_fills`

```sql
CREATE TABLE journal_fills (
  trade_id      TEXT    NOT NULL REFERENCES journal_trades (trade_id) ON DELETE CASCADE,
  seq           INTEGER NOT NULL CHECK (seq >= 0),
  tokens        INTEGER NOT NULL CHECK (tokens > 0),
  lamports      INTEGER NOT NULL CHECK (lamports >= 0),
  fee_lamports  INTEGER NOT NULL CHECK (fee_lamports >= 0),
  price_q18     INTEGER NOT NULL CHECK (price_q18 >= 0),
  quoted_q18    INTEGER NOT NULL CHECK (quoted_q18 >= 0),
  slippage_bps  INTEGER NOT NULL CHECK (slippage_bps >= 0 AND slippage_bps <= 10000),
  slot          INTEGER NOT NULL CHECK (slot >= 0),
  at_ms         INTEGER NOT NULL,

  PRIMARY KEY (trade_id, seq)
) WITHOUT ROWID;

CREATE INDEX journal_fills_at       ON journal_fills (at_ms DESC);
CREATE INDEX journal_fills_slippage ON journal_fills (slippage_bps DESC, at_ms DESC);
```

**The price columns are the one place this file stores something finer than a
lamport, and they need the explanation.** A price is lamports per token base
unit, which is a ratio rather than an amount: a pump.fun launch prices around
`2.8 × 10^-5` of a lamport per base unit. Basis points cannot hold that and
millionths barely can — twenty-eight of them, two significant figures for the
number the whole book is denominated in, and fifty basis points of slippage on
it rounds to nothing.

So `price_q18` and `quoted_q18` are integers at `10^-18`, the unit
`strategy::fixed::Q18` carries. Three things keep that honest:

- **The pair is the record and the price is derived from it.** `tokens` and
  `lamports` are in the same row, so the exact value is always recoverable.
  `price_q18` is floored, and it is there to be filtered and sorted on.
- **`FillRow::settle` computes all three.** The price, the quote and the
  slippage cannot be passed in, so they cannot disagree with each other or with
  the pair.
- **The conversion out refuses rather than saturates.** `i64::MAX` at this scale
  is 9.22 lamports for one base unit — nine million SOL for one whole
  six-decimal token, which is nine orders of magnitude past anything on either
  venue. Past it the insert fails instead of storing a clamped number.

### `journal_routes`

```sql
CREATE TABLE journal_routes (
  trade_id            TEXT    NOT NULL REFERENCES journal_trades (trade_id) ON DELETE CASCADE,
  seq                 INTEGER NOT NULL CHECK (seq >= 0),
  venue               TEXT    NOT NULL CHECK (venue IN ('pump_fun_curve', 'raydium_amm_v4')),
  chosen              INTEGER NOT NULL CHECK (chosen IN (0, 1)),
  tokens              INTEGER NOT NULL CHECK (tokens > 0),
  quoted_out_lamports INTEGER NOT NULL CHECK (quoted_out_lamports >= 0),
  min_out_lamports    INTEGER NOT NULL CHECK (min_out_lamports >= 0),
  max_slippage_bps    INTEGER NOT NULL CHECK (max_slippage_bps >= 0 AND max_slippage_bps <= 10000),
  rejected_because    TEXT,
  simulated_at_ms     INTEGER NOT NULL,
  at_ms               INTEGER NOT NULL,

  PRIMARY KEY (trade_id, seq),

  CHECK ((chosen = 1 AND rejected_because IS NULL)
      OR (chosen = 0 AND rejected_because IS NOT NULL)),
  CHECK (min_out_lamports <= quoted_out_lamports)
) WITHOUT ROWID;

CREATE UNIQUE INDEX journal_routes_chosen ON journal_routes (trade_id) WHERE chosen = 1;
CREATE INDEX        journal_routes_venue  ON journal_routes (venue, at_ms DESC);
```

Every path the router priced, taken or not, so a bad exit can be read back as a
decision rather than as an outcome. `simulated_at_ms` is when the reserves it
was priced against were read, which is what says how stale the quote was by the
time it went out.

The unique partial index is the interesting one: one trade goes one way, and two
rows claiming to be the chosen route would mean the book cannot say which
liquidity the money went through. `RouteDecision` in Rust makes the
`CHECK`-refused shape — chosen *and* rejected, or neither — unspellable, so the
constraint only ever fires against something written by hand.

### `journal_tips`

```sql
CREATE TABLE journal_tips (
  trade_id         TEXT    NOT NULL REFERENCES journal_trades (trade_id) ON DELETE CASCADE,
  attempt          INTEGER NOT NULL CHECK (attempt >= 0),
  account          TEXT    NOT NULL,
  lamports         INTEGER NOT NULL CHECK (lamports >= 0),
  stance           TEXT    NOT NULL CHECK (stance IN ('emergency', 'discretionary')),
  ev_net_lamports  INTEGER,
  ceiling_lamports INTEGER NOT NULL CHECK (ceiling_lamports >= 0),
  at_ms            INTEGER NOT NULL,

  PRIMARY KEY (trade_id, attempt)
) WITHOUT ROWID;

CREATE INDEX journal_tips_at            ON journal_tips (at_ms DESC);
CREATE INDEX journal_tips_over_ceiling  ON journal_tips (at_ms DESC)
  WHERE lamports > ceiling_lamports;
```

Keyed by `(trade_id, attempt)` rather than by a sequence, because a rebroadcast
that re-bid the same attempt is the same bid and not a second one.
`ev_net_lamports` is what the bid was a share of, and is null on every emergency
exit — Annex C.2 does not apply an EV test to one.

`ceiling_lamports` is `Tip_max` as it stood when the bid was made, kept per row
rather than read from configuration, because the question a month later is
whether the bid was inside the ceiling *then*.

`journal_tips_over_ceiling` indexes rows that should not exist. `TipPolicy` caps
every bid, so the answer is almost always empty — and asking must not cost a
table scan to find that out, because the alerting engine asks after every exit.

### `journal_signatures`

```sql
CREATE TABLE journal_signatures (
  signature    TEXT    PRIMARY KEY,
  trade_id     TEXT    NOT NULL REFERENCES journal_trades (trade_id) ON DELETE CASCADE,
  kind         TEXT    NOT NULL CHECK (kind IN ('entry', 'exit')),
  status       TEXT    NOT NULL CHECK (status IN (
                         'broadcast', 'confirmed', 'dropped', 'expired', 'failed')),
  slot         INTEGER CHECK (slot IS NULL OR slot >= 0),
  rebroadcasts INTEGER NOT NULL DEFAULT 0 CHECK (rebroadcasts >= 0),
  at_ms        INTEGER NOT NULL,

  CHECK (slot IS NULL OR status = 'confirmed')
);

CREATE INDEX journal_signatures_trade     ON journal_signatures (trade_id, at_ms DESC);
CREATE INDEX journal_signatures_status    ON journal_signatures (status, at_ms DESC);
CREATE INDEX journal_signatures_in_flight ON journal_signatures (at_ms DESC)
  WHERE status = 'broadcast';
```

The signature is the primary key, so the partial unique indexes the two ledgers
need to say "one signature appears once" are unnecessary here. There is no `tip`
kind: the tip rides inside the exit transaction as its last instruction and
shares its signature, so a third kind would be a second name for a row that
already exists.

The last `CHECK` is the one worth reading twice. A slot is what a node assigned
when the transaction landed; nothing that failed to land has one, and a zero
there would read as slot zero. Anything settled as other than `confirmed` drops
its slot, and the column refuses to keep one.

`journal_signatures_in_flight` is money whose fate is decided and not yet known —
the set the alerting engine walks to find transactions that have been out too
long.

### Foreign keys and deletion

All four child tables reference `journal_trades (trade_id)` with
`ON DELETE CASCADE`, which needs `PRAGMA foreign_keys = ON` — set on every
connection by `CONNECTION_PRAGMAS`, and off by default in SQLite forever for
backwards compatibility. A child row for a trade that was never recorded is
refused at the insert, and deleting a trade takes its fills, routes, tips and
signatures with it. There is no retention policy on the journal yet; when one
arrives it deletes trades in bounded chunks and the cascade does the rest.

## The forensic log and its checkpoints

Three tables, added by migration 5, that answer the question the journal cannot:
not what a trade cost, but why there were only four of them. The gate refuses
almost everything — that is what a gate is for — and a file that records only
the trades cannot tell a quiet week from a broken detector. Phase 6B's soak
acceptance asks for "zero-trade periods decomposed" in those words, and this is
where the decomposition lives.

`journal_state_log` is one row per launch the gate read. `journal_snapshots` is
a periodic, hash-chained checkpoint of the book. `journal_revisions` is one
monotonic counter per mode, and it is what makes the other two a pair rather
than two unrelated tables.

### Why a revision and not a timestamp

Every other table here orders itself by `at_ms`, which is right for them: they
describe when something happened on a chain. It is wrong for this one. A wall
clock is not monotonic — `now_ms()` reads `SystemTime`, NTP steps it, and a step
of a few hundred milliseconds during a busy minute either reorders two rows or
gives them the same key. Ordering the forensic record by a clock that can go
backwards means the record of a bad minute is the record most likely to be
scrambled, which is exactly backwards.

So the ordering is a counter, allocated by the writer inside the same
transaction as the rows it stamps. Three properties follow.

**It is gapless.** The allocation and the insert commit together, so a revision
is never issued for a row that then rolled back. A reader walking `1..=N`
therefore knows a missing revision is a missing row, rather than a transaction
that lost a race.

**It is per mode.** Live, paper and replay each have their own counter. Phase
3's acceptance criterion is that one fixture and one seed produce byte-identical
records, and a replay whose revisions depended on how much live traffic happened
to be flowing beside it fails that on the second run. Three counters cost three
rows.

**It never goes backwards.** A trigger, not a Rust check, for the same reason
`journal_trades` guards its identity in one: the guarantee has to hold for every
writer, including a person at a shell.

### `journal_revisions`

```sql
CREATE TABLE journal_revisions (
  stream       TEXT    PRIMARY KEY CHECK (stream IN ('live', 'paper', 'replay')),
  revision     INTEGER NOT NULL CHECK (revision >= 0),
  issued_at_ms INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TRIGGER journal_revisions_only_go_forward
  BEFORE UPDATE ON journal_revisions
  WHEN new.revision <= old.revision
BEGIN
  SELECT RAISE(ABORT, 'a revision cannot go backwards');
END;

CREATE TRIGGER journal_revisions_are_the_three_modes
  BEFORE DELETE ON journal_revisions
BEGIN
  SELECT RAISE(ABORT, 'the revision counters are the three modes and are not deleted');
END;
```

The three rows are seeded by the migration at zero, with `INSERT OR IGNORE` so
that reopening a file does not reset a counter that has moved. Zero means "none
issued": the first row written in a mode is revision 1, so a revision is never
the same integer as *no revision*.

Allocation is one statement — `UPDATE ... SET revision = revision + ?
RETURNING revision` — inside the caller's transaction. The read and the write
cannot be separated, and a batch that then fails takes its revisions back with
it.

### `journal_state_log`

```sql
CREATE TABLE journal_state_log (
  mode              TEXT    NOT NULL CHECK (mode IN ('live', 'paper', 'replay')),
  revision          INTEGER NOT NULL CHECK (revision > 0),

  mint              TEXT    NOT NULL,
  intent_id         TEXT,

  decision          TEXT    NOT NULL CHECK (decision IN ('entered', 'refused', 'deferred')),
  reason            TEXT    NOT NULL CHECK (reason IN (...thirteen gate reasons...)),
  confidence_micros INTEGER NOT NULL CHECK (confidence_micros BETWEEN 0 AND 1000000),

  buyers            INTEGER NOT NULL CHECK (buyers >= 0),
  bundle_wallets    INTEGER NOT NULL CHECK (bundle_wallets >= 0),
  cohort_wallets    INTEGER NOT NULL CHECK (cohort_wallets >= 0),
  cohort_lamports   INTEGER NOT NULL CHECK (cohort_lamports >= 0),
  pool_lamports     INTEGER NOT NULL CHECK (pool_lamports >= 0),

  operating_mode    TEXT    NOT NULL CHECK (operating_mode IN ('live','paper','replay','halted')),
  entries_allowed   INTEGER NOT NULL CHECK (entries_allowed IN (0, 1)),
  equity_lamports   INTEGER NOT NULL CHECK (equity_lamports >= 0),
  drawdown_bps      INTEGER NOT NULL CHECK (drawdown_bps BETWEEN 0 AND 10000),
  open_positions    INTEGER NOT NULL CHECK (open_positions >= 0),
  free_slots        INTEGER NOT NULL CHECK (free_slots >= 0),

  window_closed     INTEGER NOT NULL CHECK (window_closed IN (0, 1)),
  evidence_to_ms    INTEGER NOT NULL,
  decided_at_ms     INTEGER NOT NULL,

  PRIMARY KEY (mode, revision),

  CHECK ((decision =  'entered' AND intent_id IS NOT NULL)
      OR (decision <> 'entered' AND intent_id IS NULL)),
  CHECK (decision <> 'entered' OR reason = 'accepted'),
  CHECK (evidence_to_ms <= decided_at_ms)
) WITHOUT ROWID;
```

`reason` borrows `strategy::syndicate`'s vocabulary unchanged — the same
thirteen names `GateReason::as_str` writes and `daemon::Funnel` prints. That is
deliberate: a funnel over this column and a funnel in a backtest report have to
be the same table, or neither can be checked against the other.

`decision` has three arms where the gate has two, and the third is the one that
earns its place. A launch the gate refused is the strategy saying no; a launch
it accepted that opened nothing is everything *after* the strategy — the opening
window cut short by the end of a recording, a tripped breaker, the position cap,
the kill switch, `--no-execute`. Folding the second into the first makes a
funnel blame the rule for a decision the account made. `deferred` keeps them
apart, and the `CHECK` on `reason` is what stops an `entered` row claiming any
verdict but `accepted`.

The last three columns are the no-leakage record. `evidence_to_ms` is the newest
event that reached the record the gate read, and the `CHECK` against
`decided_at_ms` refuses a row that read past its own decision *at the column*
rather than leaving it to a review. A detector that started reading one event
too far shows up as a failed insert, not as a verdict that quietly changed.

```sql
CREATE INDEX journal_state_log_at      ON journal_state_log (mode, decided_at_ms DESC);
CREATE INDEX journal_state_log_reason  ON journal_state_log (mode, reason, decided_at_ms DESC);
CREATE INDEX journal_state_log_mint    ON journal_state_log (mint, decided_at_ms DESC);
CREATE INDEX journal_state_log_entered ON journal_state_log (intent_id) WHERE intent_id IS NOT NULL;

CREATE TRIGGER journal_state_log_is_append_only
  BEFORE UPDATE ON journal_state_log
BEGIN
  SELECT RAISE(ABORT, 'a forensic state row records what was believed then and does not change');
END;
```

`journal_state_log_entered` is the join back to the book: every row that names
an intent has a trade in `journal_trades` under that id.

The append-only trigger is the one rule this table has that the journal does
not. A trade row is a summary and summaries get updated — proceeds arrive, a
position closes. A forensic row is a record of what was believed at one instant,
and belief at that instant does not change later; a later belief is a later row.

**There is no `ON CONFLICT` on the insert, unlike every other write in this
file.** The rest are idempotent because their keys are deterministic and a
replay can be written twice. This one's key is a counter, so the same record
appended twice is genuinely two rows — two observations of the same launch — and
a conflict is impossible rather than ignored. A caller that wants
replay-idempotence writes into a fresh file, which is what replay already does.

### `journal_snapshots`

```sql
CREATE TABLE journal_snapshots (
  mode                  TEXT    NOT NULL CHECK (mode IN ('live', 'paper', 'replay')),
  seq                   INTEGER NOT NULL CHECK (seq > 0),
  revision              INTEGER NOT NULL CHECK (revision >= 0),
  taken_at_ms           INTEGER NOT NULL,

  trades                INTEGER NOT NULL CHECK (trades >= 0),
  closed                INTEGER NOT NULL CHECK (closed >= 0),
  notional_lamports     INTEGER NOT NULL CHECK (notional_lamports >= 0),
  cost_basis_lamports   INTEGER NOT NULL CHECK (cost_basis_lamports >= 0),
  proceeds_lamports     INTEGER NOT NULL CHECK (proceeds_lamports >= 0),
  realized_pnl_lamports INTEGER NOT NULL,
  fee_lamports          INTEGER NOT NULL CHECK (fee_lamports >= 0),
  tip_lamports          INTEGER NOT NULL CHECK (tip_lamports >= 0),
  worst_slippage_bps    INTEGER CHECK (worst_slippage_bps IS NULL
                          OR (worst_slippage_bps BETWEEN 0 AND 10000)),

  covers_from           INTEGER NOT NULL CHECK (covers_from >= 0),
  rows_since            INTEGER NOT NULL CHECK (rows_since >= 0),
  entered_since         INTEGER NOT NULL CHECK (entered_since >= 0),
  refused_since         INTEGER NOT NULL CHECK (refused_since >= 0),
  deferred_since        INTEGER NOT NULL CHECK (deferred_since >= 0),

  prev_digest           TEXT,
  digest                TEXT    NOT NULL,

  PRIMARY KEY (mode, seq),

  CHECK (closed <= trades),
  CHECK (entered_since + refused_since + deferred_since = rows_since),
  CHECK (covers_from <= revision),
  CHECK (rows_since <= revision - covers_from)
) WITHOUT ROWID;

CREATE INDEX journal_snapshots_taken    ON journal_snapshots (mode, taken_at_ms DESC);
CREATE INDEX journal_snapshots_revision ON journal_snapshots (mode, revision DESC);

CREATE TRIGGER journal_snapshots_are_immutable
  BEFORE UPDATE ON journal_snapshots
BEGIN
  SELECT RAISE(ABORT, 'a snapshot cannot be rewritten');
END;
```

The columns between `taken_at_ms` and `covers_from` are `journal::JournalTotals`,
column for column: the book in one mode, added up.

A snapshot does **not** consume a revision of its own — it is a statement about
the first N rows, not an N+1th row — so `revision` here is the counter's value
at the moment it was taken, and two consecutive checkpoints may name the same
one. That is why the key is `seq` and not `revision`, and the reason is worth
being precise about: a checkpoint states two things, the book and the log, and
they do not move together. The exit path writes `journal_trades` whether or not
anything is logging verdicts, so the book can change while the counter stands
still. Keyed by revision, that second change could never be recorded, because
the row naming that revision would already be there.

Idempotence is therefore against the *content*, not the key: a pass finding the
same revision, the same totals and the same counts as the previous checkpoint
returns that checkpoint and writes nothing. A five-minute timer against a
genuinely quiet weekend writes nothing at all; one against a weekend where a
position closed writes exactly one row.

A mode that has never done anything still gets one checkpoint — the genesis link
of its own chain, stating that at revision 0 the book was empty. Three rows per
file, once, and every later checkpoint links back to one of them.

**The book columns are cumulative and the log columns are not**, and the
asymmetry is the point. `covers_from` and `revision` bracket the slice of the
log this checkpoint speaks for — `covers_from < r <= revision` — and
`rows_since`, `entered_since`, `refused_since` and `deferred_since` count only
that slice.

Deltas rather than running totals because retention deletes from the old end of
the log. A running count of *surviving* rows falls every time the pruner runs,
so a cross-check built on the difference between two running counts reads a
successful prune as a checkpoint claiming a negative number of rows — and every
checkpoint after the first prune becomes a break, thirty days into a live run,
on a file with nothing wrong with it. A delta is computed once over an interval
the pruner cannot reach into (retention never goes above the newest checkpoint,
and at the moment the row is written that is the previous one) and stays true
afterwards whatever retention does. The running totals are recoverable by
summing the chain.

**The chain.** Ordered by `seq`. `digest` is SHA-256 over a fixed-order,
one-field-per-line rendering of the row and its predecessor's digest, hex,
sixty-four characters.
Deterministic across builds and machines because there is no float in it, no map
iteration, and no serialiser — `serde_json` would be shorter and would make the
digest depend on a dependency's field ordering, which is a thing that can change
under a `cargo update` and invalidate a chain nobody touched. `prev_digest` is
`NULL` on the first snapshot of a mode and nothing else; the genesis link hashes
the literal word `genesis`, and a `NULL` slippage hashes a `-`, because an empty
line and a zero-length number would otherwise be the same bytes.

This is the file's first actual SHA-256 hash chain, and it is a chain over the
*snapshots*, not over the audit log. `audit_log` is still the NDJSON writer's
mirror and still has no chain of its own; see below.

`taken_at_ms` is inside the digest on purpose. It makes the digest cover the
whole row rather than most of it, so a timestamp cannot be edited without
breaking the chain, and it costs nothing in replay — where the clock is fixture
time and a second run produces the same number.

### What verification can and cannot say

Two checks, and they are kept apart because only one of them is available at any
moment.

**The chain** is tamper-evidence and is always checkable.
`verify_journal_snapshot_chain` walks every snapshot in a mode in `seq` order,
recomputes each digest over its own fields and its predecessor's, and compares.
Editing any number in any snapshot breaks that row and every row after it, and a
checkpoint deleted outright is reported as the missing `seq` it is rather than
as a shorter chain. This holds whatever the book has done since, because it is a
statement about the snapshots rather than about the book.

**The recomputation** is a statement about the book, and it is conclusive only
while the book has not moved. `verify_journal_snapshot` adds the book up again
and compares against the newest snapshot, and its verdict has three arms rather
than two:

| verdict | means |
| --- | --- |
| `matches` | the log is still at the snapshot's revision and the book is what it says. A warm start may trust the checkpoint and skip the scan. |
| `superseded` | the snapshot was true when taken and the log has moved on. Not a failure — this is what a checkpoint looks like from any moment after it. |
| `diverged` | the log has not moved and the book is not what the snapshot says. Something edited the file underneath a running system. |

There is one cross-check that *does* hold across time, and the chain walk does
it: the rows, entries, refusals and deferrals a snapshot claims over its own
slice must be the rows actually in the log between `covers_from` and `revision`.
That is the checkpoint and the log agreeing about one interval, and it is the
reason a snapshot carries counts it could have recomputed. A slice holding
*fewer* rows than it claims is what retention looks like from here and is
reported as `intervalsPruned`; a slice holding *more* is a break, because
nothing in this build adds a row below a revision that has already been
checkpointed. A `covers_from` that does not meet the previous checkpoint's
`revision` is also a break — a checkpoint describing an interval nobody else's
arithmetic lines up with.

### Retention

`journal_state_log` is pruned on the same maintenance thread as `tick_metrics`,
in the same bounded chunks, at a thirty-day window against that table's seven.
The two answer different questions: a tick metric is interesting only while the
provider that produced it is still the provider, whereas a refusal is evidence
in an argument about whether the strategy works, and that argument is had over
months.

One extra guard, and it is the one worth stating. **Nothing is removed above the
newest checkpoint's revision, ever.** A row no snapshot has accounted for is a
row whose disappearance would make the chain's interval check read as a break
rather than as a prune, and a retention policy must not be able to break the
integrity check running beside it. A mode with no snapshot therefore prunes
nothing at all, which is the correct reading of "there is no checkpoint to be
behind".

`journal_snapshots` has no retention policy. It gains one row per mode per
period in which the book actually changed, the rows are small, and deleting the
front of a hash chain is how a chain stops being one.

## `tick_metrics`

The health of each RPC endpoint over time: one row per endpoint per tick. This
is the only table that grows at a fixed rate whether or not anything is
happening, which shapes both its layout and its retention.

```sql
CREATE TABLE tick_metrics (
  rpc_endpoint   TEXT NOT NULL,
  timestamp      INTEGER NOT NULL,
  latency_ms     INTEGER NOT NULL CHECK (latency_ms >= 0),
  dropped_msgs   INTEGER NOT NULL CHECK (dropped_msgs >= 0),
  parsed_per_sec_micros INTEGER NOT NULL CHECK (parsed_per_sec_micros >= 0),
  PRIMARY KEY (rpc_endpoint, timestamp)
) WITHOUT ROWID;

CREATE INDEX tick_metrics_time
  ON tick_metrics (timestamp DESC);
```

`WITHOUT ROWID` because the rows are tiny — five small columns, well under a
hundred bytes — and an ordinary table would store each of them twice: once in
the rowid B-tree and again in the index on the primary key. A `WITHOUT ROWID`
table stores the row inside the primary key's own B-tree, which halves both the
space and the write cost for a table that is nothing but small rows. This is the
case that optimisation is for.

The key is `(rpc_endpoint, timestamp)` in that order, so one endpoint's history
is one contiguous run of pages. That is the shape of the common read: draw the
latency line for Helius over the last ten minutes.

`tick_metrics_time` covers the other two reads. The status bar wants the newest
tick across all endpoints at once, and retention wants to delete everything
older than a cutoff; both walk time regardless of endpoint, which the primary
key cannot serve because time is its second column.

`rpc_endpoint` is the **host only**, never the full URL. Every provider used
here puts its credential in the URL, so a full URL in a table the UI reads is a
leaked key. `EndpointStatus::url` already carries the host alone and this column
follows it.

`dropped_msgs` is a count for the tick, not a running total — the number that
says the engine is slower than the feed right now, which an ever-growing
lifetime counter cannot tell you. It should be non-zero rarely and the UI should
say so loudly when it is.

`latency_ms` is the endpoint's p50 for that tick. The p95 is worth having too,
and is deliberately not in this table yet: adding a column later is a migration,
and adding one that is null for all history is a query hazard for as long as
that history is kept. Add it when there is a reader for it.

### Retention

`tick_metrics` needs a cutoff or it will eventually be most of the file. The
policy:

```sql
DELETE FROM tick_metrics WHERE timestamp < :cutoff_ms;
```

run on a timer, with a cutoff of seven days. Seven days is long enough to see a
provider degrade over a weekend and short enough that the table stays small
enough to scan.

Two things about deleting from SQLite that this policy depends on:

- The file does not shrink. The freed pages go on a free list and get reused by
  the next inserts, which — since the next inserts are more tick metrics — is
  exactly what should happen. The file reaches a steady size and stays there.
  There is no need to `VACUUM` on a schedule, and every reason not to: `VACUUM`
  rewrites the whole database while holding a lock.
- The delete must be bounded. One statement removing a week of rows is a single
  large transaction that bloats the WAL and holds the writer. Deleting in
  chunks — a few thousand rows per statement, in a loop — keeps the pauses
  invisible.

  The chunk is bounded by the primary key, not by `rowid`. `tick_metrics` is
  `WITHOUT ROWID` and therefore has no `rowid` to select on at all, which is the
  one place that optimisation costs something:

  ```sql
  DELETE FROM tick_metrics
  WHERE (rpc_endpoint, timestamp) IN (
    SELECT rpc_endpoint, timestamp FROM tick_metrics
    WHERE timestamp < :cutoff_ms
    LIMIT :chunk
  );
  ```

  `DELETE ... LIMIT` directly would be simpler and is not available: it needs
  SQLite compiled with `SQLITE_ENABLE_UPDATE_DELETE_LIMIT`, which the bundled
  build does not set.

No other table has a retention policy. `candidates`, `clusters` and
`execution_logs` are the record, and the record is kept.

## Migrations

Schema changes are numbered, applied in order, and each one runs inside its own
transaction. SQLite applies DDL transactionally, so a migration that fails
halfway leaves the file exactly as it was.

```sql
CREATE TABLE schema_migrations (
  version     INTEGER PRIMARY KEY,
  applied_at_ms INTEGER NOT NULL,
  checksum    TEXT NOT NULL
);
```

`checksum` is a hash of the migration's SQL as it was applied. A migration whose
text changed after it ran is how two machines end up with the same version
number and different schemas, and comparing the checksum on startup is what
catches that. It is FNV-1a, rendered as `fnv1a64:<16 hex digits>`. Not a
cryptographic hash and not trying to be — the only thing it defends against is a
migration being edited after it shipped, which is an accident rather than an
attack, and it needs to be cheap, stable across builds and dependency-free. The
audit log's hash chain is where SHA-256 belongs.

Migration 1 is the first four runtime tables — `candidates`, `clusters`,
`execution_logs`, `tick_metrics` — plus `audit_log` and every index on them. It is written with `IF NOT EXISTS` throughout, even though the ledger
already guarantees it runs once, so that it can land on a database that predates
the ledger — which is every file written before `db.rs` owned the schema.

Migration 2 is `intent_transitions` and its four indexes. It is a new table
rather than columns on `execution_logs`, because the exit lifecycle is a
different state machine over a different subject and folding it into that
table's `state` column would mean widening a `CHECK` that the whole execution
path is written against.

Migration 3 is the trade journal: `journal_trades`, `journal_fills`,
`journal_routes`, `journal_tips`, `journal_signatures`, their indexes, and the
trigger that holds a trade's identity still. Its SQL lives in `journal.rs`,
beside the code that reads and writes it, and is registered in `db.rs`'s
migration list from there — one chain and one runner, because a second migration
table against the same file is how two builds end up disagreeing about what a
version number means.

Migration 4 is the last six `REAL` columns, as integers. No new tables: it
finishes applying the journal's rule to the four that predated it, so that after
it there is no column anywhere in `sts.db` with `REAL` affinity.

| was | is | unit |
| --- | --- | --- |
| `candidates.bonding_curve_pct` | *dropped* | — |
| `clusters.temporal_influence` | `clusters.temporal_influence_micros` | millionths |
| `clusters.spectral_separation` | `clusters.spectral_separation_micros` | millionths |
| `clusters.interaction_entropy` | `clusters.interaction_entropy_micros` | millionths |
| `execution_logs.price` | `execution_logs.price_q18` | lamports per token base unit at `10^-18` |
| `tick_metrics.parsed_per_sec` | `tick_metrics.parsed_per_sec_micros` | millionths of a message per second |

`bonding_curve_pct` is the only one dropped rather than converted, because there
was no information in it: it was a `VIRTUAL` generated column computing
`curve_progress_bps / 100.0`, never stored, read by nothing but its own test,
and derived from an integer that was already the exact answer.

`clusters`, `execution_logs` and `tick_metrics` are rebuilt rather than altered
— SQLite can add and drop a column but cannot change one's type or its `CHECK`,
and both change here — so each is the standard new-table-copy-drop-rename, with
its indexes recreated afterwards. None of the three is the parent of a foreign
key, which is what makes the `DROP TABLE` safe under `foreign_keys = ON`; the
only foreign keys in this schema point at `journal_trades`, which is untouched.

The `SELECT`s that copy do arithmetic in floating point, which is not a
contradiction — they are reading columns that are already `REAL` and it is the
last time anything in this system will. The conversions are exact for the values
those columns can hold. The price is the one that cannot always be converted and
says so rather than saturating: past `i64::MAX` at `10^-18`, and below `10^-18`
where the raw count would floor to a zero the new `CHECK` refuses, the row keeps
a `NULL`. That reads as "no price recorded", which is true, and is what every
other row in that column already said — nothing ever read it back.

Migration 5 is the forensic log and its checkpoints: `journal_state_log`,
`journal_snapshots`, `journal_revisions`, their indexes, and the four triggers
that hold the counter monotonic, the log append-only and the snapshots
immutable. Its SQL lives in `forensics.rs` and is registered from there, for the
same reason migration 3's lives in `journal.rs`.

It is the first migration to seed rows rather than only create tables: the three
revision counters, at zero, with `INSERT OR IGNORE` so that reopening a file
does not reset a counter that has moved. A counter that sprang into existence on
first use would make "this mode has never written anything" and "there is no
counter" the same reading.

`schema_migrations` itself is created outside any numbered migration, because it
is the thing that records that the numbered migrations ran.

Rules that keep this workable:

- Only the writer connection migrates, and it does so before any reader opens.
- A database whose `version` is **higher** than the running build knows about is
  not opened. An older build reading a newer schema is the case that corrupts
  things quietly, and refusing to start is the only safe reading of it.
- Prefer adding a nullable column over rewriting a table. SQLite's `ALTER TABLE`
  can add a column and rename things but cannot drop a constraint; anything more
  is the twelve-step rebuild, which means a full table copy.
- Backfills run in bounded chunks, for the same reason retention deletes do.

## `audit_log`

Not one of the five runtime tables, and here because the code needs it. The kill
switch and the panic hook both append a row, `db.rs` has no `AuditLogger` of its
own yet, and the Node process that used to create this table is archived — so
without it in the schema, pulling the kill switch fails to write its own record.

```sql
CREATE TABLE audit_log (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  event_type TEXT    NOT NULL,
  payload    TEXT    NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE INDEX audit_log_created_at
  ON audit_log (created_at DESC);
```

`payload` is JSON as `TEXT`, per the convention above. This table is the
*mirror*, not the record: `docs/AUDIT_EVENTS.md` makes the append-only NDJSON
file the record of first resort, which is the whole reason `synchronous = NORMAL`
is an acceptable trade. It has no retention policy and no hash chain of its own;
the hash chain belongs with the NDJSON writer, and this table follows it once
that exists. `journal_snapshots` is chained and this is not, and the two are not
in tension — a chain over the book's checkpoints says the book has not been
rewritten, which is a narrower claim than a chain over every event, and it is
the one that could be made without the NDJSON writer existing first.

## How ingest rows map onto `candidates`

`ingestion.rs` builds an `IngestCandidateRow` in the vocabulary of the socket
path and `Database::record_ingest_candidates` translates it. Three fields are the
same thing under a different name:

| ingest row      | column               |
| --------------- | -------------------- |
| `account`       | `curve_account`      |
| `slot`          | `detected_slot`      |
| `pool_lamports` | `liquidity_lamports` |
| `route`         | `fast_path_eligible` (`'fast_path'` → 1, anything else → 0) |

`mint` and `symbol` have no source in that row and are written null, as the
`candidates` section describes. Nothing else is transformed: the basis points,
the lamports and the microsecond latency all bind as the integers they were
computed as.

Route is mapped rather than stored as text so the partial index
`candidates_fast_path` has something to filter on, and an unrecognised route
reads as the standard path — the safe direction, since the fast path is the one
that skips confirmations.
