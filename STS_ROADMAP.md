# STS Institutional-Grade Phased Execution Roadmap

Status: gated implementation plan | Owner: Ethan | Platform: MacBook Neo 2, Apple Silicon | Date: Friday, August 21, 2026

> **Read [`docs/VERDICT-2026-08-27.md`](docs/VERDICT-2026-08-27.md), revision 4,
> before acting on anything in this file.** The STOP block immediately below stands
> and is now firmer than when it was written — but several of the numbers *inside*
> it have since been corrected, and the corrections all made the answer worse. The
> STOP block itself is left exactly as written, because rewriting a stop signal is
> how it gets ignored; the corrections are listed in the section that follows it.
> Every retired number is tabulated in
> [`docs/sprint-2026-08-27/INDEX.md`](docs/sprint-2026-08-27/INDEX.md) under *Do not
> quote these numbers*. **One factual correction that applies wherever this file
> names a migration venue: graduates migrate to PumpSwap, not Raydium** (65 of 65
> confirmed). Last reconciled against revision 4 on 27 August 2026.

## STOP — Phase 3's gate FAILED on 2026-08-27. Phases 4, 5 and 6 are closed.

A twelve-run grading sprint measured out-of-sample expectancy against the real
captures. Six independent methods over different fields returned **−7.8% to −12.6%
per trade** (n up to 9,418) after the pump.fun round trip (95 bps each way, so
1.90% — not the 2% first assumed). The best of 108 exit
rules on real second-by-second price paths returns −2.65%; the same grid on
scrambled seconds returns −2.75% to −3.21%, so the entire tradeable structure in
the data is worth about 0.2 percentage points.

Phase 3's own gate reads: *"no shadow-live or execution dispatcher before positive
holdout stressed expectancy... A profitable-looking result that fails any economic
stress is rejected."* Expectancy is negative, so **the gate is closed by its own
terms.** This is the roadmap working as designed, not a setback to be routed around.

**Anything reading this file for work — human or automated — must not dispatch
Phase 4, 5 or 6 items.** The supervisor at `~/Code/tools/sts-supervisor` dispatches
from this file; this block is the stop signal.

Full reasoning: `docs/VERDICT-2026-08-27.md`. Summary of why:
1. It is over before we can act — 76% of peaks land within 5 seconds, and 66–79%
   of coins hit their 60s high at or before the second we are able to decide.
   Entering at s3 and s5 make the same money, so speed is not the bottleneck.
2. Every signal detects volatility, not direction. Found independently four times.
   Order flow predicts the peak at z=+10..+15 and predicts the finish equally
   strongly the other way. You cannot short a bonding curve.
3. ~~The €200 bankroll cannot pay the toll.~~ **STRUCK 2026-08-27, and it was
   wrong twice.** The 1,005,000-lamport figure was a p90 read as a median from 25
   samples — it is a retail sniper front-end's default preset. Real cost for the
   population we belong to is a median **0.107% of the SOL bought**; minimum viable
   order is **0.026–0.063 SOL**, not 0.25–0.4. Do not let this argument come back.
   We are cheap to trade and too slow to trade; only the second is fatal.

**What IS still permitted, and is the only sanctioned work:** re-run `flux enrich`
(11,528 signatures already queued, 25 ever returned, no new code) to replace the
25-sample fee estimate with a 10,000-sample measurement, then capture four more
*continuous* listener sessions — continuous, not more nights at 3–8% uptime — and
re-run the paired within-session comparison. Paired comparisons have a spread of
only 1.2–3.8pp, so a 3-point edge is resolvable with six sessions at ≥150 coins
each. That test is a few days of listening and no new code.

**The honest word is UNRESOLVED, not negative.** The best strategy found is
0.4–0.6 points short of break-even (2.22% gross at 0.05 SOL) against a dataset
resolution of ±3.1 points. Two leads are still under test — flow-based exits (worth
~1.8 points, and every one of ~1,700 rules tested so far was a price rule) and a
many-hands entry filter. Full reasoning in `docs/VERDICT-2026-08-27.md` revision 2.

Reopening Phases 4–6 requires a positive holdout number from that work. Not an
argument, a number.

**And read that sentence with the archival sweep beside it, or it will mislead you.**
It was written when the thesis was unresolved. It is no longer unresolved: seven
hour-matched windows from October 2024 to August 2026 are negative in every one,
uncorrelated with volume, rivals or time, and the best rule of a realisable grid still
loses **0.86% with every cost set to exactly zero**. More sessions of the same market
cannot produce a legitimate positive, and a positive out of a grid search on them
would be the sixth selection artefact this project has had to withdraw. **The
condition above is not a door left ajar. Nothing in Phases 4 to 6 is waiting on more
data.**

## Corrections to the STOP block above, and the facts that qualify everything below it

The STOP block is left exactly as it was written, because it is the stop signal
and rewriting stop signals is how they get ignored. These are the corrections it
has since acquired. **The stop itself is unchanged and is now firmer, not weaker.**
Everything here is against `docs/VERDICT-2026-08-27.md` **revision 4**, which
supersedes revisions 1 to 3.

1. **"UNRESOLVED, not negative" was revision 2. The answer is a flat no go.** The
   "0.4–0.6 points short" figure rested on pricing a trailing stop as though it
   filled *at* the stop level. There is no resting stop order on pump.fun: you
   watch the price break, then send a transaction, and you fill at whatever the
   market is doing when it lands. ~~Priced honestly the rule gives −1.67% gross and
   −3.57% net, so the real gap is about 5.7 points.~~ **That was revision 3's
   figure and it was not reproducible — it was the most optimistic of four
   reasonable ways of building the same rule.** All four give **−3.06% to −4.29%
   gross**, which is **−4.96% to −6.19% net** against a **+2.12%** break-even. **The
   gap is 7 to 8 points**, more than twice the ±3.1 resolution of this dataset.
   The general lesson: "a 5% trailing stop" is four different rules depending on
   whether the running peak tracks closes or highs and whether the stop is tested
   against the close or the low, and those four spread the answer by six points.
   Any future exit number in this project must state both choices or it is not a
   number.
2. **Flow-based exits are dead.** The "~1.8 points" was a holding-time effect, not
   a flow effect: it compared a flow rule exiting at 3.9 seconds against a price
   stop sitting in the trade for 32.7. Paired properly, flow against a plain
   five-second stopwatch with no inputs is −0.06 points, and the full flow grid
   lands at the 0th percentile of its own null. ~~Only the many-hands filter is
   still standing.~~ **The many-hands filter is dead too**, and it earned the kill:
   it was never "many hands" (it counts *buys*, not buyers — on distinct wallets,
   15 or more gives −2.3%), the hands are a bot fleet (the median first-3s buyer
   inside it appears in 52 other launches), **one coin is 52% of all its profit**,
   it inverts out of sample (−11.60% against −0.99% for buying everything on the
   largest separate session), and it sits at the 87th percentile of pure noise over
   5,263 filters re-searched on 60 shuffles. **There is nothing left under test that
   could make this trade pay** — which is not the same as nothing being true. The
   verdict is careful about this and this file was not: sell timing and order flow
   genuinely carry information about *when to leave*, and first-sell exit is the only
   input in the sprint that beat a matched-exposure baseline and went on beating it
   out of sample, at about half its published size and with its interval crossing
   zero. It does not reach break-even and nothing built on it did. Overstating the
   kill is the same failure as overstating the edge.
3. **The STOP block's summary numbers have all moved, and every one moved the
   wrong way for the strategy.**
   - ~~76% of peaks land within 5 seconds.~~ That was **08-21 alone**, the
     quietest day in the corpus. Pooled it is **69.8% by second 5**, 60.7% by
     second 3, 91.3% by second 30.
   - ~~The entire tradeable structure in the data is worth about 0.2 percentage
     points.~~ It is **0.4 to 0.9**. The best of 108 exit rules on real price paths
     loses **2.32%** (the STOP block says 2.65%); the same grid on scrambled
     seconds loses 2.75–3.21%.
   - ~~Break-even is 2.22% gross at 0.05 SOL.~~ It is **+2.12%** — the verified
     pump.fun round trip is 95 basis points a side, measured on 4,918 sells, not
     the 100 first assumed. That makes the bar slightly *easier* and it still is
     not cleared.
4. **"It is over before we can act" is right, but the reason given is not the whole
   reason — there was never anything to be early to.** An hour-matched sweep of
   seven windows from October 2024 to August 2026, on data nobody in this project
   had ever touched, buys every launch and sells inside the minute: it loses
   **−6.5% to −17.1%** in **every** window, mean −10.1%, uncorrelated with launch
   volume, with rival count, and with time. **In October 2024, with only 2.7 rival
   buyers in the first three seconds, this trade lost 7%.** The edge was not
   competed away by faster bots. There was never an edge to lose. Two beliefs die
   with it: **the market never collapsed** (43.9 launches a minute in October 2024
   against 37.6 in August 2026 — about 1.6 million a month at both ends, not the
   "1.7M then, 2,851 now" that was written down as fact), and **the evidence was
   never out of reach** — the free Helius endpoint serves full blocks, transactions
   and logs, back to at least 6 August 2024, at zero cost. Separate research the
   same day found the same thing by another route: **the official public Solana RPC,
   `api.mainnet-beta.solana.com`, is a free archive node serving the ledger from
   genesis**, no account and no API key. Two independent routes, both free, and
   nobody had swept either. **The €0 rule in the governing constraints below never
   blocked historical work** — the assumption that history was expensive did. See
   `docs/TRAINING_DATA_FREE.md`. A one-off historical sweep should now be the first
   response to any claim about how this market used to behave.
5. **The listener does not drop launches — it is almost never running, which is
   what qualifies the recapture plan in the STOP block.** Checked against the chain
   block-by-block rather than inferred from inside the capture, flux caught **62 of
   62** and the STS recorder **75 of 75** on its busiest minute: **137 of 137,
   `missing: 0`.** Three earlier figures are withdrawn — 40%, then 0–2.5%
   (inferred from inside the capture, which cannot see a launch it never received),
   then 90% (an off-by-a-denominator: 353 launches over a 100.7-minute *span* is
   3.5/min, but over the 10.6 minutes actually *connected* it is 33.2/min, so
   "nine in ten missing" and the 89.5% duty-cycle hole were one fact counted
   twice). **The captures are complete while connected and simply short**, and the
   hole is uptime at 3–72% — which no `gap` row can record, because gap rows are
   written by the running process. **08-21 is not a day; it is 48 minutes spread
   over ten hours.** The four-continuous-sessions plan is still the right next
   capture, but it should be run on the fixed recorder in `tools/capture`, and
   **the recorder still has no network-fee or cost block** — that needs offline
   enrichment against the signatures it now records. At 35 launches per connected
   minute, 150 coins a cell needs about 4 hours a session; four sessions is 16 hours
   and roughly 25,300 clean coins.
6. **Own-order price impact — the cost this roadmap never measured and most
   feared — is not the obstacle.** At 0.1 SOL it is **0.04% of the trade**. A buy
   and a sell on a constant-product curve walk the same path in opposite directions
   and cancel exactly, so a round trip is the two 1% fees and little else.

Three facts qualify every "Exact acceptance criteria" block in the rest of this
file, and all were established on 27 August 2026:

- **Nothing in `src-tauri/` has ever read a real capture.** No file in the crate
  opens `coins-*.jsonl`, `tracks-*.jsonl` or `tweets-*.jsonl`; there is not one
  `sts.replay.v1` fixture anywhere on disk; and `fixtures.rs` is a *synthetic*
  generator opening with the words "launches that never happened". So "1,654 tests
  pass" means the engine agrees with itself, and every number in the verdict came
  from Python written during the sprint, going around the engine entirely. Three of
  the things a trading system needs do not exist at all: **no exit rule** (no stop,
  target, trailing stop or time exit anywhere in `src-tauri/src/`), **no paper
  mode** (`OperatingMode::Paper` is never constructed outside `#[cfg(test)]`, which
  is why `paper_trades` is empty), and **no entry-side transaction builder**. Nor
  is what exists always what its name says: there is no Jito client (no HTTP, no
  gRPC, nothing is ever bundled), no ed25519 and no PDA derivation in the executor,
  and "24-hop tracing" is a depth counter that truncates at two or three hops on
  real branching data.

- **No gate in this roadmap had ever been enforced, at any phase, before that
  date.** Phase 0 criterion 1 had never passed; the true clippy warning count was
  62 and `cargo fmt` had never been run. Phase 1 criterion 4 — critical facts
  require two consistent providers or are UNKNOWN — was never implemented at all,
  and no second provider is configured, so it could not run if it were. Treat
  every criterion below as unverified until someone runs it.
- **The calendar-day split is not an out-of-sample test — but a real holdout does
  exist and it is not the one the brief named.** The seven calendar files are nine
  capture sessions, six usable; 08-21 is the tail of the 08-20 run past midnight
  (one recorder process, 14.98 hours, no restart across the boundary), and it is
  the *worst* day to validate on — 46.8% burst-truncated, 48.4% dead coins. The
  genuine holdout is **08-16**, which sits behind 3.78 days of total silence and
  five distinct recorder processes: fit on 08-20 + 08-21, score on 08-16. **It is a
  holdout in time, not in population** — 81.2% of its clean coins share a wallet
  with the fitting days — so it tests stability, not generalisation, and its
  `funding` block covers 4.3% of the day from one five-minute run, so **no
  wallet-graph feature can be held out on it.** Paired
  within-session comparison remains the right method for filters. The capture is also duty-cycled — median gap between
  launches is 0.8 seconds, so the 52 multi-minute gaps are outages, not quiet
  markets — and 14% of outcomes are silently truncated when the listener stops.
  **No day-level or era-level comparison drawn from this corpus should be believed
  until the same hours are captured on different days**; that duty cycle is this
  dataset's biggest single source of false findings and it produced two of them in
  one night.

Full reasoning: `docs/VERDICT-2026-08-27.md` revision 4.

## Phase 0, criterion 1 PASSES as of 2026-08-27, at `fda41e8`.

This changes nothing above it. Phase 3's gate is shut on a number and stays shut;
Phases 4, 5 and 6 remain closed. It is recorded here because the STOP block is
what anything reading this file reads first, and because criterion 1 is the one
gate in the roadmap that was closed by work rather than by a market.

```
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

exits 0. 1,654 tests, 0 failures — the same count as the baseline at `c626069`,
so nothing was bought by deleting or weakening a test.

It had never passed before. Two things were wrong with the way it was measured:

**The count was 52 and the real number was 62.** `-D warnings` turns each warning
into an error, and cargo stops the build at the first target that fails — which
is the `lib test` target. The twelve integration test targets and the binary were
therefore never linted at all, and the 52 that got quoted was one target's tally
rather than the workspace's. Running clippy *without* `-D warnings` lints
everything and reports 62: 52 in the lib, 1 in the binary, and 9 in
`tests/journal_alerting.rs` and `tests/sprint2_contracts.rs` that the gate had
never once looked at.

**`cargo fmt --check` had never been run either** — 1,576 hunks across all 44
Rust files, against stock style, since there is no `rustfmt.toml`.

Cleared in three commits: `145fd98` cherry-picks the lint work from the abandoned
`feat/daemon-sandwich-entryquote-wiring`, `a3545e8` clears the 27 findings that
commit did not reach, and `fda41e8` is the first `cargo fmt`. One narrow `allow`
exists in the tree, on `ExitRouteKind`'s `large_enum_variant`, with its reasoning
written at the site; nothing else is suppressed.

What this buys is that `-D warnings` is now a tripwire that means something: the
next warning to arrive in a diff is the only warning, instead of the sixty-third.

## Governing constraints

STS is local-first, non-custodial, and evidence-first. Fixed infrastructure spend is exactly €0; the €200 bankroll remains untouched through development, replay, paper, and shadow modes. Free Helius and QuickNode Geyser/RPC capacity is pooled only within current terms, quotas, and attribution requirements. No paid RPC, hosted database, subscription, or infrastructure upgrade is permitted until it is funded by realized surplus under the 50/50 rule. Rust owns keys, ingestion, persistence, gates, execution, and audit; Tauri/WebKit is a disposable projection layer.

Global release invariant: no phase may be promoted while a critical failure is unresolved. UNKNOWN, stale, contradictory, quota-exhausted, or unverifiable data may block new exposure, but must never block exits, reconciliation, or the hardware kill-switch. Every event is UUIDv7 identified, provenance-preserving, hash-chained, replayable, and linked by correlation_id.

Every phase produces: versioned code and schema, immutable evidence fixtures, operator runbook, threat/risk register, test output, and a signed gate record. Acceptance uses fresh replay fixtures and an out-of-sample holdout; passing targets are not profitability guarantees.

## Phase 0 — Environment & Foundation Setup

Objective: establish the Tauri/Rust workspace and bounded, zero-allocation core before live data.

Deliverables:
- Tauri desktop shell with Rust modules: domain, ingest, normalize, forensics, risk, execution, persistence, replay, ipc, audit.
- Typed IPC allowlist; WebKit receives projections only and has no key, SQLite, credential, or arbitrary filesystem access.
- Preallocated MPSC ingress ring buffer with sequence numbers, fixed slots, slab/object pools, backpressure counters, and no hot-path SQLite, JSON serialization, recursive traversal, or blocking I/O.
- Isolated SQLite writer in WAL mode plus read-only readers; numbered transactional migrations; JSONL writer, rotation, checksum, and hash-chain fields.
- UUIDv7 event/correlation IDs, canonical bytes, SHA-256 genesis/hash chain, contradiction events, immutable event envelope, schema/version registry.
- Resource governors for CPU, memory, disk, queue, thermal/battery; warm-start snapshot verification and EXIT_ONLY fallback; hardware kill-switch path.

Exact acceptance criteria:
1. `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` pass. **PASSES as of 2026-08-27 at `fda41e8`** — 1,654 tests, 0 failures. See the note under the STOP block for what was wrong with the way this was measured before.
2. SQLite initialization asserts `journal_mode=WAL`, `synchronous=NORMAL`, `foreign_keys=ON`, and `busy_timeout=5000`; one-writer enforcement is tested.
3. A 1,000,000-event synthetic run records zero critical-event loss, zero duplicate canonical IDs, byte-identical replay projections, and bounded queue behavior under saturation.
4. Hot-path benchmark demonstrates zero allocations and zero blocking I/O in the measured section; p99 event-loop work is under 10 ms on the MacBook, with p50/p95/p99/max and queue depth persisted.
5. Restart, corrupt hash, future schema, disk-full, WAL/checkpoint failure, and kill-switch tests enter the documented safe mode and leave durable evidence.
6. Key material never appears in UI DTOs, logs, SQLite, JSONL, crash output, or test fixtures.

Milestone verification commands:
```bash
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
cargo test --workspace -- --nocapture foundation::ring foundation::hash_chain foundation::recovery
cargo bench --bench hot_path -- --save-baseline phase0
sqlite3 "$STS_DB" 'PRAGMA journal_mode; PRAGMA synchronous; PRAGMA foreign_keys; PRAGMA busy_timeout;'
./target/debug/sts audit verify --all && ./target/debug/sts replay verify --fixture fixtures/phase0
```
Risk gate: do not ingest real feeds or create signing code until all six criteria pass. A performance miss is fixed by bounded design, not by weakening measurement.

## Phase 1 — Multi-Feed Data Ingestion & RPC Adapter Pool

Objective: acquire targeted, quorum-checked data without exhausting free tiers.

Deliverables:
- Helius and QuickNode free-tier Geyser/RPC adapters with independent health scores, reconnect/backfill/fork handling, quota counters, and per-source circuit breakers.
- Source-side allowlists for pump.fun bonding-curve accounts, Raydium migration/pool accounts, required mints/programs; no broad logs or speculative polling.
- Stable-identity client deduplication `(source, signature, instruction_index, event_type)`, conflict-to-contradiction handling, slot watermarking, gap detection, and JSONL/SQLite ingestion fixtures.
- Freshness quorum selecting the freshest consistent snapshot; provider divergence degrades or blocks entries while risk-reducing exits remain live.

Exact acceptance criteria:
1. Both adapters pass connect, reconnect, timeout, quota, malformed payload, fork, gap, and backfill tests with provider credentials supplied only via OS environment/keychain.
2. Target filters reject an unallowlisted account before transmission; a fixture proves no broad-chain subscription occurs.
3. Duplicate replay produces one canonical event; conflicting payloads produce a contradiction event and no overwrite.
4. Critical facts require two consistent providers or are UNKNOWN; slot lag and freshness budgets are visible and deterministic.
5. In a 24-hour shadow soak, zero critical events are silently dropped, all drops are counted, and no free-tier quota is exceeded.
6. Health bands and transitions implement p50 <=120 ms, p95 <=350 ms healthy, p95 <=500 ms degraded, and unhealthy above that or on repeated timeouts/disagreement.

Milestone verification commands:
```bash
cargo test --workspace -- ingest::adapters ingest::dedupe ingest::quorum
./target/debug/sts ingest fixture --input fixtures/phase1 --replay --assert-no-duplicates
./target/debug/sts providers health --window 24h --assert-quota-safe --assert-quorum
./target/debug/sts ingest shadow --providers helius,quicknode --filters config/account_allowlist.toml --duration 24h
./target/debug/sts audit verify --source ingest --since phase1-start
```
Risk gate: no live signing, no unfiltered subscription, and no entry on single-provider critical facts. Quota, slot, or thermal breach forces RESTRICTED_ENTRY/EXIT_ONLY.

## Phase 2 — Dual-Speed Risk & Feature Pipeline

Objective: make bounded risk decisions fast while forensic analysis remains asynchronous and explainable.

Deliverables:
- Fast Path immutable versioned snapshot engine: freshness/provenance checks, authorities, liquidity/depth, concentration, absorption, microstructure, RPC health, EV inputs; fixed hop/neighbor/route budgets; sub-10 ms p99 target.
- Async Forensic worker: graph entropy, Sybil/cluster probability with confidence interval, CEX funding trails, deployer lineage, mixer/bridge evidence, AgeQuality, model version/input hash/produced_at/validity watermark.
- Calibrated confidence vector and tier policy; hard invariants versus soft degradation; explicit NORMAL, RESTRICTED_ENTRY, EXIT_ONLY, HALTED modes.
- Deterministic sizing and stress inputs including 1.5% executable-liquidity cap, risk budget, -30/-50% gaps, 10/15/20/25% slippage, CVaR, no-exit probability, and emergency route readiness.

Exact acceptance criteria:
1. Fast Path p99 is <10 ms on a representative fixture, performs no recursive graph traversal, blocking I/O, gRPC fan-out, SQLite, JSON serialization, or forensic wait, and records allocation/latency budgets.
2. Async outputs publish atomically only with watermark, hashes, model version, validity interval, and unknown-feature count; stale outputs cannot be mistaken for current.
3. Hard-block, degraded, paper-only, and observe-only decisions match the master-spec pseudocode across a golden decision corpus.
4. Top-1/5/10, HHI, Shannon entropy, effective wallets, cluster probability, AgeQuality, absorption, depth, and authority fixtures are reproducible and provenance-linked.
5. Every entry candidate has stressed EV lower confidence bound, size caps, tier, reasons, and a precomputed emergency exit; failed facts block entries but preserve exits.
6. Calibration fixture reports Brier score, reliability diagram, cohort/drift metrics, and leakage check; no model promotion occurs from in-sample performance alone.

Milestone verification commands:
```bash
cargo test --workspace -- risk::golden risk::modes features::determinism forensic::provenance
cargo bench --bench fast_path -- --assert-p99-us 10000
./target/debug/sts features verify --fixtures fixtures/phase2 --assert-hash-stable
./target/debug/sts risk evaluate --corpus fixtures/phase2/golden --assert-replay-equivalent
./target/debug/sts calibrate report --train data/train --holdout data/holdout --out reports/phase2-calibration
```
Risk gate: no execution envelope is signable until a hard gate, freshness quorum, positive stressed EV LCB, and emergency route all pass. Forensic lag may reduce size or block entry; it cannot disable exit.

## Phase 3 — Deterministic Replay & Out-of-Sample Backtesting Engine

Objective: prove behavior and economics under historical information and adversarial execution drag.

Deliverables:
- Append-only JSONL stream recorder with rotation, checksums, hash-chain verification, fixture manifests, and decision-time-only replay.
- Deterministic simulator for fees, modeled depth/slippage, partial fills, latency/outages, provider disagreement, Jito tips, adverse selection, 1.5% participation, -30/-50% and empirical gap buckets.
- Walk-forward/purged/embargoed out-of-sample evaluation, multiple-testing controls, cohort/regime breakdowns, rug avoidance versus profitability separation, and zero-trade decomposition.
- All 11 named test suites: unit, integration, property, replay, regression, load, failover, chaos, economics, security/non-custody, and UI/IPC contract.

Exact acceptance criteria:
1. Same fixture, policy, model, and seed yields byte-identical decisions, fills, PnL, audit records, and projections across two runs.
2. Replay never reads post-decision information; leakage, survivorship, selection bias, and time-split violations fail the run.
3. Slippage, gap, partial-fill, failed bundle, outage, tip, and no-executable-exit scenarios are modeled and reported with CI, CVaR, exit time, and failure probability.
4. All 11 suites pass; a failure names fixture, correlation ID, expected/actual state, and durable evidence.
5. Holdout stressed EV LCB is positive under the approved policy; calibration is within policy; no claim of 85–95% rug avoidance or 40–55% win rate is accepted without adequate cohorts.
6. Replay equivalence and failure-mode matrix pass byte-for-byte for canonical data.

Milestone verification commands:
```bash
cargo test --workspace --all-features
./target/debug/sts replay record --input fixtures/phase3 --out data/replay/phase3
./target/debug/sts replay run --stream data/replay/phase3 --seed 0x100x --out reports/phase3-a.json
./target/debug/sts replay run --stream data/replay/phase3 --seed 0x100x --out reports/phase3-b.json && diff -u reports/phase3-a.json reports/phase3-b.json
./target/debug/sts backtest walk-forward --purge --embargo --gaps 30,50 --slippage 10,15,20,25 --out reports/phase3
./target/debug/sts test-suites run --all-11 --fail-on-critical
```
Risk gate: no shadow-live or execution dispatcher before positive holdout stressed expectancy, replay equivalence, and all 11 suites. A profitable-looking result that fails any economic stress is rejected.

> **GATE RESULT 2026-08-27: FAILED.** Holdout stressed expectancy is negative (−7.8% to −12.6%/trade across six independent methods). Phases 4–6 are closed. See the STOP block at the top of this file and `docs/VERDICT-2026-08-27.md`.

## Phase 4 — Execution Engine & Jito Bundle Dispatcher

> **CLOSED 2026-08-27.** Phase 3's gate failed on a number, and this phase does not
> authorise anything. The criteria below stay exactly as written, as the record of
> what was planned; none of them may be dispatched, and passing them would not
> reopen this phase. See the STOP block at the top of this file.

Objective: execute only signed, expiring, simulated, private, atomic decisions and reconcile reality.

Deliverables:
- Isolated Rust executor accepting complete signed envelopes only; OS-protected local keys never cross UI/analytics boundaries.
- Private Jito bundle dispatcher with simulation immediately before signing, exact accounts/routes, expiry, nonce/idempotency key, 1–3% default slippage bounds, and no public-mempool fallback.
- Tip controller implementing `tip = min(p75_tip, 0.15 * NetEV)` for this roadmap, subject to Tip_max, positive NetEV after tip, bounded increments, and logged calibration. Emergency escalation is separately capped.
- Atomic lifecycle and partial-fill reconciliation: PLANNED → SIMULATED → SUBMITTED → UNFILLED/PARTIALLY_FILLED/FILLED/EXPIRED/FAILED; CAS versions, reserve watermarks, fencing leases, receipts, residual accounting, STOP_PENDING/EXITING paths.

Exact acceptance criteria:
1. Invalid, incomplete, stale, expired, contradictory, or negative-NetEV envelopes are rejected before signing.
2. Simulation is mandatory; public fallback is structurally impossible; atomic bundle failure leaves no user-visible partial state without reconciliation.
3. Tip tests prove `tip <= min(p75_tip, 0.15*NetEV)` and `NetEV - tip > 0`, with no unbounded escalation.
4. Duplicate receipts, unknown confirmations, partial fills, provider disagreement, lease expiry, restart, and ambiguous state all reconcile idempotently and never increase exposure silently.
5. Kill-switch cancels eligible orders, blocks new submissions, and remains available during provider, UI, WAL, and forensic failures.
6. Testnet/devnet or fixture harness demonstrates private bundle lifecycle; mainnet broadcast remains disabled until Phase 6 gate.

Milestone verification commands:
```bash
cargo test --workspace execution::state_machine execution::partial_fill execution::idempotency
./target/debug/sts executor self-check --assert-no-public-fallback --assert-key-isolation
./target/debug/sts bundle simulate --fixtures fixtures/phase4 --assert-atomic
./target/debug/sts tip-calibrate --formula 'min(p75_tip,0.15*NetEV)' --fixtures fixtures/phase4/tips
./target/debug/sts chaos execution --scenarios unknown-confirmation,provider-split,kill-switch,disk-full
```
Risk gate: dispatcher remains simulation-only until all criteria pass and Ethan explicitly authorizes a later promotion. No ordinary-size capital is permitted.

## Phase 5 — 0x100x Command Centre UI

> **CLOSED 2026-08-27.** Phase 3's gate failed on a number, and this phase does not
> authorise anything. The criteria below stay exactly as written, as the record of
> what was planned; none of them may be dispatched, and passing them would not
> reopen this phase. See the STOP block at the top of this file.

Objective: provide a transparent operator console without making UI the source of truth.

Deliverables:
- Tauri + native WebKit split pane: left event/signal ledger, center forensic/mathematical inspector, right execution/risk/control pane; mirrored CLI (`sts observe`, `inspect`, `replay`, `approve`, `kill`, `reconcile`).
- Terminal telemetry with event-loop lag, queue/sink/WAL health, provider p50/p95/p99, slot freshness, dropped critical events, thermal/disk state, mode, and zero-trade decomposition.
- Holder entropy/HHI/cluster graphs, funding trees, AgeQuality, liquidity depth/impact, stress paths, real-time realized/unrealized PnL, fills, stops, tips, and correlation drill-downs.
- Authenticated global hardware/local kill-switch and immutable operator audit; accessibility labels distinguish UNKNOWN, STALE, BLOCKED, and ZERO.

Exact acceptance criteria:
1. UI displays every required state textually and never renders UNKNOWN as zero; stale timestamps, slots, sources, confidence, and rationale are visible.
2. Every panel drills to immutable event/correlation IDs and matches rebuilt projections byte-for-byte after restart.
3. WebKit cannot access secrets, SQLite, RPC credentials, or arbitrary filesystem; typed IPC rejects unknown commands/schema/deadlines.
4. Kill-switch works from independent local/GUI paths during UI freeze, provider outage, WAL failure, and executor ambiguity.
5. UI/IPC suite passes at target update load without blocking Rust ingestion; terminal telemetry includes actual measured values.

Milestone verification commands:
```bash
cargo test --workspace ipc::contract ui::projection ui::kill_switch
npm ci --offline && npm run build
./target/debug/sts ui audit --fixture fixtures/phase5 --assert-no-secret-leak --assert-state-labels
./target/debug/sts projections rebuild --from data/replay/phase3 --out /tmp/sts-projections && ./target/debug/sts projections compare --bytewise
./target/debug/sts chaos ui --scenarios freeze,ipc-fuzz,kill-switch
```
Risk gate: UI may observe and authorize typed actions only; it may not bypass Rust gates, mutate raw events, or become the sole kill path. Production bundle remains unsigned/non-broadcast until Phase 6.

## Phase 6 — Promotion Ladder

> **CLOSED 2026-08-27.** Phase 3's gate failed on a number, and this phase does not
> authorise anything. The criteria below stay exactly as written, as the record of
> what was planned; none of them may be dispatched, and passing them would not
> reopen this phase. See the STOP block at the top of this file.

Objective: promote only through measured, reversible exposure.

### Gate 6A — Deterministic Replay
Acceptance: Phase 3 all-11 pass; positive out-of-sample stressed EV LCB; calibration/drift and bias reports approved; replay byte-equivalence; gap/slippage/tip/partial-fill/outage tests pass. Deliverable: signed replay dossier and rollback policy. Command: `./target/debug/sts promotion gate replay --require all-11,oos-ev-lcb-positive,replay-equivalence`.

### Gate 6B — Shadow Live
Run pooled Helius/QuickNode streams with zero signing/broadcasting. Acceptance: 72-hour soak, no quota breach, no silent critical loss, provider quorum/slot freshness within policy, fast-path p99 <10 ms, persistence reconciliation clean, zero-trade periods decomposed. Deliverable: shadow report and incident log. Command: `./target/debug/sts promotion shadow --duration 72h --assert no-sign,no-broadcast,no-critical-loss`.

### Gate 6C — Capped Paper
Simulate every approved decision using live snapshots and modeled fills. Acceptance: 14 consecutive days, deterministic ledger, daily reconciliation, realistic fees/slippage/tips, no unexplained divergence from replay beyond predeclared tolerance, kill-switch and exit-only drills passed. Deliverable: paper-trading report and operator sign-off. Command: `./target/debug/sts promotion paper --duration 14d --assert deterministic,kill-switch,exit-only`.

### Gate 6D — Micro-Capital Live
Ethan explicitly authorizes mainnet; private Jito only; entries capped at 0.05 SOL maximum and also by risk budget, 1.5% executable liquidity, loss, correlation, and operator caps. One position at a time initially; no averaging down; protected €200 buffer remains untouched. Acceptance: 30 live days or 100 fully reconciled trades (whichever is later), zero public fallback, zero unbounded tips, zero unexplained exposure, every stop/exit drill passes, positive stressed expectancy after realized costs, and no critical incident. Deliverable: live promotion/rollback dossier. Command: `./target/debug/sts promotion micro-live --require operator-token --max-entry-sol 0.05 --private-only --no-average-down`.

### Gate 6E — Measured Promotion and 50/50 Flywheel
Promotion is never automatic. Increase limits only after a review of realized (not unrealized) gains, losses, fees, taxes, obligations, CVaR, drawdown, execution drag, calibration, and incidents. Compute `realized_surplus = gains - losses - fees - taxes - obligations`; allocate 50% to segregated savings/risk-free reserves and 50% to system reinvestment. Paid infrastructure spend is `<= max(0, 0.50 * realized_surplus)` and requires an approved bottleneck analysis, recurring-cost cap, expected improvement, and rollback measurement. Command: `./target/debug/sts treasury close --realized-only --split 50/50 --assert-infra-budget-safe`.

Final risk gates: any key exposure, public fallback, stale-price execution, silent contradiction, critical-event loss, kill-switch failure, negative stressed EV, unexplained divergence, quota exhaustion, disk/WAL integrity failure, or unbounded tip is an immediate HALT/EXIT_ONLY event. Reset requires fresh evidence, incident review, new correlation IDs, and authenticated operator approval.

## Operating cadence and phase dossier

At each gate Ethan receives: commit hash, hardware benchmark, test report, replay manifest/hash, quota/health report, risk register, open exceptions, capital ledger, and explicit PASS/FAIL/WAIVE decision. Waivers are never allowed for key isolation, public fallback prohibition, critical-event loss, kill-switch availability, replay non-determinism, or €0 fixed-infrastructure compliance.

This roadmap is an execution plan, not a profitability promise. The only acceptable proof is reproducible, out-of-sample, cost-adjusted behavior that preserves capital and keeps exits live under degraded conditions.
