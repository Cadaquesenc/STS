> # STATUS NOTE — 27 August 2026: the market thesis in this file was tested and it lost.
>
> This document was written on 21 August 2026, before any of it had been measured
> against real captured launches. On 27 August 2026 a grading sprint measured it,
> and a later pass the same day found that fourteen of the sprint's own load-bearing
> numbers were themselves wrong (tabulated in
> [`docs/sprint-2026-08-27/INDEX.md`](docs/sprint-2026-08-27/INDEX.md) under *Do not
> quote these numbers*). **One factual correction that runs through this whole file:
> graduated coins migrate to PumpSwap, not Raydium** — 65 of 65 confirmed — so every
> passage below that names Raydium as the migration venue has the wrong venue. **Read
> [`docs/VERDICT-2026-08-27.md`](docs/VERDICT-2026-08-27.md), revision 4, first** —
> earlier revisions are superseded, and so is anything in this file that quotes
> them.
>
> The body below is kept as it was written, because it is a record of what was
> believed and that record is worth having. Where a passage states something now
> known to be false, a short dated note has been added beside it. Nothing has been
> deleted. **Last reconciled against revision 4 of the verdict on 27 August 2026.**
>
> **Disproved.** Treat these as false, not as pending:
>
> - **There was never an edge here to lose.** This is the finding that governs all
>   the others, and it was the last one found. An hour-matched historical sweep —
>   the third Wednesday of each month at 18:00 UTC, October 2024 through August
>   2026, seven windows — buys every launch and sells it inside the minute. It
>   loses in **every** window: **−6.5% to −17.1%, mean −10.1%.** The loss does not
>   track launch volume, does not track how many rivals are in the first three
>   seconds, and does not track time. **In October 2024, with only 2.7 rival buyers
>   in the first three seconds, this trade lost 7%.** So the strategy did not stop
>   working and was not competed away by faster bots. It never worked. **And the
>   evidence was never out of reach** — the sweep cost nothing, on free endpoints
>   that have been fully archival the whole time. See the note at §16.2.
> - **The market never collapsed, and no argument in this project may rest on the
>   idea that it did.** ~~Activity fell from 1.7M launches a month to 2,851 a
>   month.~~ **False, and off by about 500x.** The archival sweep counted **43.9
>   launches a minute in October 2024 against 37.6 a minute in August 2026** —
>   roughly 1.6 million a month at both ends. The 2,851 was a count of what our own
>   listener happened to record, mistaken for the market.
> - **The recorder does not drop launches. It is simply almost never running.**
>   Checked against the chain rather than inferred from inside the capture —
>   block-by-block, launch for launch — flux caught **62 of 62** and the STS
>   recorder **75 of 75** on its busiest minute: `skipped: 0, missing: 0`, **137 of
>   137.** Three earlier figures for this are all withdrawn: 40%, then 0–2.5%
>   (inferred from inside the capture, which cannot see a launch it never
>   received), then 90% (an off-by-a-denominator — 353 launches over a 100.7-minute
>   *span* is 3.5/min, but over the 10.6 minutes actually *connected* it is
>   33.2/min, so "nine in ten missing" and the 89.5% duty-cycle hole were one fact
>   counted twice). **The captures are essentially complete while connected. They
>   are just short.** The hole is uptime, 3–72%, and nothing else — and a `gap` row
>   can never show it, because gap rows are written by the running process, so the
>   interval between a `stop` and the next `start` is structurally invisible. The
>   sharpest statement of it is not a percentage: **08-21 is not a day, it is 48
>   minutes spread over ten hours.**
> - **"Phase 2: $25k–$80k consolidation base — target regime" (§ below, and the
>   "$25k–$80k regime" test case in the testing section) is not a regime.** The
>   durable finding is structural: **the "middle" is a moment, not a phase.**
>   Median time from launch to $25k is **89 seconds** (bounds 82–106s), and only 3
>   of 53 coins are still above it twelve hours later, with a median 12-hour low
>   back at launch. There is no consolidation base sitting there to be traded, so
>   the eligibility conditions in that section — adequate history, compression,
>   base reclaim, confirmed higher low — have nothing to attach to. $80k is above
>   pump.fun's graduation point, so the top of the band was never on the bonding
>   curve at all. Entering when a coin crosses into the band returns −6.53% gross,
>   −8.43% net against −11.40% for buying everything: no edge, still a loss, on
>   n=86 with a wide interval and one unmeasured quantity (the post-migration
>   price, which nobody ever captured) that could move it either way.
> - **Every signal in this document detects volatility, not direction.** Wallet
>   clustering, funding-graph forensics, social gating and order flow all sort
>   outcomes strongly and none of them is profitable. Order flow predicts the peak
>   at 10 to 15 standard deviations and predicts the finish equally strongly the
>   other way. The wallet-graph slice returns −3.92% where taking everything
>   returns −3.87%. Selecting for "this one sometimes pops" selects for "this one
>   usually craters"; they are the same coins, and there is no way to be short.
> - **Cost is not the obstacle, so none of the cost engineering here rescues it.**
>   At a fee of zero the best realizable rule still loses 0.86% a trade. Priority
>   fees do not buy an earlier slot — paired within the same launch over 303,681
>   comparisons, the higher-fee transaction landed earlier 50.3% of the time.
>   Nor is the bankroll the obstacle: the minimum viable order is **0.026–0.063
>   SOL**, not the 0.25–0.4 that was believed, and being the first buyer costs
>   about what being the fifty-first costs.
> - **Our own order barely moves the price, and the fear that it would was
>   wrong.** At 0.1 SOL — the middle of this bankroll's range — own-order impact
>   costs **0.04% of the trade.** The reason is a property of the curve nobody had
>   worked out: pump.fun is a constant-product curve, so if you buy and then sell,
>   you walk up the curve and back down the exact same path. The bad price going in
>   is the good price coming out and they cancel **exactly**. A round trip is the
>   two 1% fees and essentially nothing else. Every impact, depth and participation
>   rule below is therefore solving a problem this bankroll does not have.
> - **The wallet ladder that justified continuing (2.6% → 44%) was never our
>   data.** It came from Dune history for 6 February 2025, not from anything this
>   project measured, and on our own held-out data it is **flat above two wallets**.
>   ~~It described a market running 1.7M launches a month against today's 2,851.~~
>   **That reason for dismissing it was itself false** — see the market-collapse
>   entry above. The ladder is still not ours and still does not replicate; a dead
>   market is not why.
> - **The fat tail this design is built to ride is largely manufactured — but it is
>   not a recording defect and it is not empty.** ~~It is substantially a recording
>   defect.~~ Corrected 27 August 2026: those prices really printed. On 18.4% of
>   testable coins the recorded price moves further than the money that traded could
>   possibly have moved it, and **9 of the 10 largest peaks fail that check** (a
>   second, simpler test — could the money that entered this coin have paid for the
>   price it printed? — fails **10 of the 10** largest). But the chain itself priced
>   each following trade off the "impossible" number, so it was the live pool state,
>   not a decoder bug: one actor, running one program, quotes a SOL reserve rescaled
>   to the curve's real balance instead of moved by the trade. **It sells a chart.**
>   Removing the affected coins makes every number worse. And there is a real
>   remainder: **86 coins peaked above 3x with 50 or more distinct wallets in them,
>   and not one of those fails the money test or contains that actor's program.**
>   The thing that separates the real tail from the manufactured one is how many
>   different wallets were there — free, and available at capture time. It does not
>   rescue the strategy: a 3x that fifty wallets found is still a 3x nobody could
>   have decided on at second 3.
>
> **Nothing in this specification has ever been run against a real capture.** No
> file under `src-tauri/` opens `coins-*.jsonl`, `tracks-*.jsonl` or
> `tweets-*.jsonl`; there is not one `sts.replay.v1` fixture anywhere on disk; and
> `fixtures.rs` is a *synthetic* generator whose first words are "launches that
> never happened", which is the one thing this project's own first rule forbids. So
> "1,654 tests pass" means the engine agrees with itself. Every number quoted in
> this status note came from Python written during the sprint, reading the raw
> capture files and going around the engine entirely. That is also why the absences
> below went unnoticed for months: nothing ever ran end to end against real data, so
> nothing surfaced them.
>
> **Unbuilt, not merely undone.** Annex G's stops and take-profit ladder, the paper
> trading stage in the promotion ladder, and the entry dispatcher do not exist in
> `src-tauri/src/`. There is no stop, no target, no trailing stop and no time exit
> anywhere in the codebase; `OperatingMode::Paper` is never constructed outside
> `#[cfg(test)]`; there is no entry-side transaction builder. Annex J's Jito
> pipeline has **no Jito client** — no HTTP, no gRPC, no endpoint, and the crate has
> no HTTP dependency at all; nothing is ever bundled. Section 10's execution
> contract could not produce a transaction a validator would accept: there is **no
> ed25519 anywhere in the crate** and **no PDA derivation**, so the signer returns
> sixty-four bytes over the message rather than a signature and the sell's account
> list is built out of label hashes. Phase 1's requirement that critical facts need
> two consistent providers or be marked UNKNOWN was never implemented, and no second
> provider is configured. The MEV simulator says so about itself, at the top of its
> own file: *"Nothing in here is a measurement."*
>
> **What still stands.** The engineering doctrine — local-first, deterministic,
> time-aware, explainable, auditable, exits never blocked by stale data, the
> kill-switch and circuit-breaker discipline, the €200 buffer left untouched — is
> good and is not affected by the market result. So is the curve arithmetic:
> `replay.rs` holds integer-exact constant-product maths with the fee taken outside
> the curve and rounding always against the trader, and it is **the one component
> in this project that has ever been checked against reality and passed** —
> predicting `initialBuyTokens` from `initialBuySol` exactly on 5,927 of 5,959 real
> trades. It is the *market thesis* that failed, not the way it was engineered.
>
> One lesson from the sprint belongs here rather than in the verdict, because it is
> about how this document gets used: the catalogue of errors corrected on 27 August
> — a p90 read as a median, a stop priced at the stop level, a table with the
> awkward row removed, a market collapse invented out of our own listener's output —
> was made in scripts that hurriedly reimplemented what the engine already had
> right. **Every one of those errors ran the same way: toward a tidier story.**
> Correcting all of them made the answer worse, not better.

# 00. ETHAN DIRECTIVE LOG — STS PROJECT

Chronological, Ethan-only record of direct inputs, requests, design pivots, and architectural requirements represented in STS_TRANSCRIPT.md (Thursday, August 20, 2026 through Friday, August 21, 2026), followed by the current directive.

## Thursday, August 20, 2026

1. No Ethan-authored message is explicitly represented in the supplied transcript for the first recorded STS event at 05:46 PM UTC. Poke's event description is intentionally excluded.

## Friday, August 21, 2026

2. No Ethan-authored message is explicitly represented in the supplied transcript for the 12:32 AM, 12:44 AM, 12:49 AM, or 01:01 AM UTC events. Poke's event descriptions are intentionally excluded.

3. 09:27 AM UTC — Ethan requested that the five identified failure modes be fixed in the master Markdown file.

4. 09:29 AM UTC — Ethan requested a final recursive self-debunking loop to make the STS specification essentially undebunkable and consistently profitable.

5. Later — “Send it”.

6. Later — “It’s still feasible for me to make?”

7. Later — “Then it needs to be a software not a web app at this point?”

8. Later — “That’s a great idea”.

9. Later — “How do you think the ui should be maybe a gui or a cli or a mix? Kinda like the Claude app? Where there is the terminal and then like could be where the chat is but it’s just like token info and visual stuff”

10. Later — “Then does it remove the part of needed to be run on local host when it can just me a dmg file?”

11. Later — “I don’t like electron it feels clunky”.

12. Latest transcript message — “Okay now since the first message related to sts that I sent you last night send me a transcript of your messages plus mine all until now in a file please”.

## Current directive — Friday, August 21, 2026

13. Update /workspace/user/STS_CORE_IDEOLOGY.md: remove Poke’s side from section 00; format section 00 strictly as Ethan’s direct directives, requests, and instructions across the Aug 20–21 conversation timeline; audit the master specification below for mathematical consistency and completeness; preserve all master-spec content and technical annexes; upload the updated file as a new media file and return its media ID.

## Scope note

Only Ethan-authored inputs are included above. Where the supplied transcript contains only a Poke event and no Ethan message, that absence is recorded rather than filled with an inference.

# STS Core Ideology & Production Technical Specification

Status: Production-grade overhaul
System: Solana Trading System (STS)
Operating standard: 0x100x engineering, forensic transparency, non-custodial execution

## 1. Mission and governing principles

STS is a forensic intelligence and execution-control system for identifying positive-expectancy Solana opportunities while prioritizing capital preservation. It is not a guarantee engine, a custody service, or a substitute for operator authorization. Every decision must be explainable, replayable, timestamped, and tied to raw evidence.

The governing equation is:

EV = (P(win) × average win) − (P(loss) × average loss) − fees − slippage − adverse selection − failure costs.

STS optimizes cost-adjusted expectancy, calibrated risk, and survival—not trade count or headline win rate.

Non-negotiables:
- Raw observations are immutable and provenance-preserving.
- Unknown, stale, contradictory, or unverifiable data is never silently treated as safe.
- Risk controls are deterministic, versioned, and replayable.
- Social hype cannot override on-chain forensic risk.
- The system is non-custodial: private keys remain local and isolated from storage, UI, and analysis services.
- The operator has an authenticated kill switch and final governance authority.
- A missed trade is preferable to unquantified risk, but minor telemetry jitter must not cause a zero-trade death loop.

## 2. System architecture

```text
RPC/WebSocket/indexers
        |
        v
Lock-free ingress ring buffer -> normalizer/deduplicator -> hot state
        |                                      |
        |                                      +-> forensic/features -> confidence/EV gate
        v                                                         |
Persistence workers: SQLite WAL + streamed JSONL                    v
        |                                             signed decision envelope
        v                                                         |
Replay/backtest store                         preflight -> private Jito/RPC executor
                                                                  |
                                                        fill/risk reconciliation
                                                                  v
                                                   0x100x Command Centre + audit log
```

Collection, persistence, analysis, UI, and execution are separate failure domains. The executor accepts only a complete, signed, expiring decision envelope and cannot fill missing values or bypass a failed hard safety gate.


### 2.1 Dual-Speed Risk Pipeline

The critical path is a bounded fast path with a sub-10 ms p99 target: it reads only versioned precomputed feature snapshots, performs zero recursive graph traversal and zero blocking I/O, enforces fixed hop/neighbor/route budgets, and uses slab allocation/object pools. Budget overflow returns bounded UNKNOWN and invokes mode policy; it never expands work inline.

The asynchronous forensic path runs on background workers and computes graph entropy, CEX funding trees, and spectral clustering. Results publish atomically with versioned watermarks, input hashes, model versions, produced_at, and validity intervals. Fast-path readers use immutable snapshots and never wait for forensic workers.

## 3. High-throughput ingestion and persistence

### 3.1 Ingestion contract

Capture pump.fun, Raydium, RPC, WebSocket, and approved indexer observations with event ID, schema version, observed_at, slot, signature, instruction index, program, mint, pool, source, event type, raw payload, and derived fields. Deduplicate on stable identity such as signature plus instruction index. Detect gaps, reconnect, backfill, forks, and provider divergence. Raw payloads are immutable; derived features are versioned.

### 3.2 Lock-free asynchronous pipeline

The hot path must not perform synchronous SQLite or JSON serialization. It writes a bounded, preallocated in-memory multi-producer/single-consumer ring buffer (LMAX-disruptor style), using sequence numbers, cache-aware slots, and backpressure metrics. Ingress acknowledges only after the event is accepted into the configured durability boundary; it never waits on a disk mutex.

A dedicated persistence worker drains the ring buffer and performs batched writes. If the buffer approaches capacity, the system raises a visible degraded-mode alarm, sheds only explicitly non-critical derived telemetry, and never drops trades, signatures, gate decisions, or risk events. Dropped data is counted and recorded.

### 3.3 SQLite and JSONL requirements

SQLite runs in WAL mode with `PRAGMA synchronous=NORMAL`, busy timeout, indexed tables, and one dedicated writer. Write batches flush every 100 ms or 500 events, whichever comes first. Transactions are atomic, bounded, and idempotent. Readers use read-only connections and never block the ingress path.

JSONL is appended through a dedicated non-blocking streaming file writer with buffered chunks, rotation, checksums, and fsync policy appropriate to the configured durability tier. Both sinks share event IDs and a hash-chain/integrity field. A reconciliation worker detects one-sided writes, corrupt lines, sequence gaps, and sink lag. On restart, the ring buffer is rebuilt from the durable queue/store and hot state is replayed from SQLite/JSONL.

Event-loop lag is continuously measured. The target for ingestion/execution event-loop work is zero blocking milliseconds; actual p50, p95, p99, maximum lag, queue depth, sink lag, and dropped-event count are displayed. “0 ms” is an architectural target, not an unmeasured claim.


### 3.4 MacBook Resource Governance, WAL Decoupling, and Warm Starts

Declare explicit memory, CPU, disk, queue, and battery governors. Require M_hot + M_forensic + M_runtime <= M_cap; on breach, shed optional forensic work, reduce concurrency, and reset epoch-tagged caches without evicting the safety snapshot or blocking exits.

Persist versioned materialized snapshots containing schema, event watermark, feature/model versions, reserves, positions, orders, mode, and integrity hash. Warm start is load -> verify hash/schema -> replay only delta after watermark -> CAS publish; failure enters EXIT_ONLY. SQLite WAL checkpoints, fsync, compaction, and JSONL writes run on a dedicated asynchronous persistence queue completely decoupled from the critical path.

## 4. Forensic risk engine

> **MEASURED 2026-08-27 — it works, and it does not make money.** The forensics are
> real: tweet reuse sorts outcomes 14.4% → 0.5% across reuse buckets, and "a wallet
> we have seen buy early three times before just bought this" moves the 2x rate from
> 0.40% to 9.39% on unseen data. They are not noise and not overfitting. But
> **selecting a slice with the wallet graph returns −3.92% where taking everything
> returns −3.87%** — it detects volatility, not direction, and there is no way to be
> short a bonding curve. Two honest uses survive: as a **reject** filter it drops
> 47% of launches while losing only 3 of 65 winners, and excluding coins on a reused
> or stale tweet drops coins half of which never reach a tradeable price.
> Separately, the forensic score should be **frozen for trading** — it improves
> ranking in the middle, which we never trade, and makes the top 5% *worse* (19.28%
> → 15.66%). The path-scoring in `tracer.rs` is worth keeping on its own merits as
> forensics. One caution if anyone quotes the depth of the tracing: **"24-hop
> tracing" is a depth counter** — on real branching data at the default node budget
> it truncates at two or three hops. `docs/VERDICT-2026-08-27.md`.

Inspect mint/freeze authorities, supply, decimals, metadata mutability, LP controls, holder concentration, entropy, deployer lineage, parent-funded trees, known CEX hot wallets, bridges, mixers, bundlers, and synchronized wallet behavior. Failed reads remain unknown.

Track top-1/5/10 shares, non-bonding-curve concentration, HHI, Shannon entropy, effective wallet count, and changes over time. A default hard rejection applies when non-protocol, non-locked concentration exceeds 25%. Holder independence is measured by funding, timing, behavior, and exit correlation—not wallet count alone.

CEX funding is not proof of misconduct, but removes the presumption of independence. Replace binary CEX-funding rejection with heuristic cluster probability:

`P(cluster) = calibrated_model(shared_parent, time_proximity, amount_similarity, fanout, synchronized_entry, instruction_similarity, exit_correlation)`

Maintain a score from 0 to 1 with confidence interval, feature provenance, model version, and unknown-feature count. High probability reduces confidence and size; a hard block remains appropriate for proven coordinated insider distribution, mixer-linked concealment with corroborating evidence, or explicit policy violations. Borderline clusters are quarantined or Tier-3 filtered rather than causing universal rejection.

## 5. Multi-dimensional confidence and graceful degradation

A binary gate must not convert harmless latency jitter into a zero-trades state. Separate hard safety invariants from soft data-quality confidence. Hard blocks include unbounded authority risk, impossible/contradictory state, inadequate liquidity, failed simulation, known coordinated exit, missing price for execution, or inability to enforce the stop. Soft degradation lowers tier, size, or execution venue.

Compute a calibrated confidence vector, not a single opaque score:
- forensic integrity and authority certainty;
- holder independence and cluster probability;
- liquidity depth and impact certainty;
- market microstructure/absorption quality;
- RPC freshness and execution reliability;
- EV calibration and sample support;
- social corroboration, never as a safety override.

Use an out-of-sample calibrated model (isotonic or Platt calibration as validated) with reliability diagrams, Brier score, cohort breakdowns, and drift monitoring. Every decision stores raw features, normalized values, weights/model version, confidence interval, and degradation reason.

### Confidence tiers and sizing

Tier 1 — Ultra-high conviction: all hard invariants pass; confidence >= 0.85; no material unknowns; calibrated EV remains positive under stress; RPC and liquidity are healthy. Maximum permitted strategy size is the minimum of risk-budget size, 1.5% of current pool liquidity, maximum-notional limit, and any stricter operator cap.

Tier 2 — High conviction: hard invariants pass; confidence 0.70–0.849; limited soft uncertainty or latency degradation; EV remains positive after conservative stress. Size is 50% of the Tier-1 result, still subject to every liquidity and account limit.

Tier 3 — Speculative/filtered: confidence 0.55–0.699, borderline cluster/absorption, or insufficient calibration support. No automatic real-capital entry by default; paper trade, alert, or operator-confirmed micro-size only at 10% of Tier-1 sizing. Below 0.55, observe only.

A tier can never override a hard block. Position size is also capped by `risk_budget / stressed_loss`, expected impact, daily loss, concurrent exposure, and correlated-position limits. No averaging down into a failed thesis.

### RPC latency tolerance and failover

Measure each provider continuously using rolling windows. Default operating bands (configurable only through versioned governance) are p50 <= 120 ms and p95 <= 350 ms for reads; p95 <= 500 ms is degraded; p95 > 500 ms or repeated timeouts is unhealthy. A provider may be auto-switched when two consecutive windows breach thresholds, error rate exceeds policy, slot lag is non-zero beyond tolerance, or responses disagree.

Use at least two independently operated endpoints, health-score them by latency, slot freshness, error rate, simulation success, and response consistency, and require quorum/consistency for critical facts. A soft breach moves Tier 1 to Tier 2 or Tier 2 to Tier 3; an unavailable critical fact remains a hard block. Recovery requires sustained healthy windows, not one fast response.


### 5.1 Tiered Operating Modes and Liveness Invariants

UNKNOWN is a data state, not an action. Classify actions as irreversible entry/risk-increasing or risk-reducing exit. Modes are NORMAL, RESTRICTED_ENTRY (degraded size/slippage), EXIT_ONLY, and HALTED. Degradation transitions are NORMAL -> RESTRICTED_ENTRY -> EXIT_ONLY -> HALTED; recovery requires fresh validated snapshots and hysteresis.

Strict invariant: stale, missing, contradictory, or UNKNOWN data NEVER blocks exits, stop-losses, reductions, reconciliation, or kill-switch actions. Such actions use the last valid bounded route and conservative limits, with emergency escalation and an alarm if no executable route exists. Only new exposure may be blocked or reduced.


## 6. Adaptive accumulation, absorption, and entry regime

> **DISPROVED 2026-08-27 — there is no base to accumulate into.** This whole
> section assumes a coin dumps, gets absorbed, and then builds a quiet base you can
> buy. Measured on the real captures, the coin is over before that can happen:
> **60.7% of coins hit their 60-second high at or before second 3, 69.8% by second
> 5, 91.3% by second 30**, and 66–79% have already peaked by the second we are able
> to decide. 21% never trade at all and 38.5% are dead by second 3. The "middle"
> the base was supposed to form in is a **moment, not a phase** — median 89 seconds
> from launch to $25k. Holding longer is monotonically worse, at every horizon
> tested out to twelve hours. The features listed below are real and they do sort
> coins; what they sort for is **volatility, not direction** — see the status note
> at the top of this file. `docs/VERDICT-2026-08-27.md`.

Retire the static “50-block look-ahead” as a sufficient condition. History length remains a minimum-data feature, not an entry thesis. Prefer low-volatility consolidation bases to breakout exhaustion spikes.

Track:
- microstructure volume profile by price band and time window;
- buyer diversity index and independent-wallet entropy;
- net buy/sell delta during consolidation;
- holder turnover velocity and retention;
- volatility compression, range efficiency, and liquidity resilience;
- volume-weighted absorption at support;
- non-correlated wallet growth.

Define dip-absorption ratio as absorbed sell volume divided by the first major insider/sniper sell volume, adjusted for price impact and liquidity. A healthy response shows bounded drawdown, higher lows, replenished bids, rising independent-wallet count, and non-correlated follow-through. A dump that merely transfers supply to correlated wallets is not absorption. Enter only when the base has positive delta, improving diversity, stable or improving depth, and EV that survives the first-dump stress test. Breakout spikes require stricter confirmation and reduced size.

## 7. Liquidity-constrained risk and gap-down modeling

> **MEASURED 2026-08-27 — at our size this is not the binding constraint, and it
> was assumed to be.** Own-order price impact was the one cost this project had
> never measured and long feared. It is **0.04% of the trade at 0.1 SOL**, the
> middle of the €200 bankroll's range. On a constant-product curve a buy and a sell
> walk the same path in opposite directions and cancel **exactly**, so an instant
> round trip returns what you put in and the only thing you lose is the two 1%
> fees. Impact alone reaches 0.5% only above about 0.26 SOL on a 1.5x exit and 0.15
> SOL on a 2x. The rules below are sound and cost nothing to keep; they are simply
> solving a problem this bankroll does not have. **What does bite at these sizes is
> the flat landing fee divided by a tiny order** — at 0.01 SOL you lose roughly 22%
> of the order before the trade has an opinion — which sets a floor on order size of
> **0.026–0.063 SOL** and has nothing to do with depth. `docs/VERDICT-2026-08-27.md`.

Hard rule: maximum position size is no greater than 1.5% of current pool liquidity, measured using executable depth at the relevant price bands, not headline TVL. This is a participation cap, not a guarantee that an exit moves price only 3–5%; actual impact must be simulated, and any pool failing the modeled impact bound is blocked or reduced. Emergency exits must be stress-tested against depth depletion.

Model discontinuous gaps of -30% and -50% as explicit scenarios, plus empirical gap buckets from historical pools. Stops are not assumed to fill cleanly. Compute:

`EV_stressed = Σ probability(gap bucket) × payoff(fill path, impact, fees, partial fill, adverse selection)`

Use empirical, time-split distributions with confidence intervals and regime labels. Report expected loss, CVaR/expected shortfall, liquidation/exit time, and probability of no executable exit. The default automated stop remains within -15% to -20% where enforceable; a gap can produce worse realized loss and must be reflected in sizing, not hidden by the stop label.

Pre-compute emergency exit transactions/bundles before entry, validate routes and exact token accounts, and maintain a dynamic escalation policy. On stop breach, submit the safest valid private exit with bounded escalation, never exceeding configured tip and loss budgets. If no executable route satisfies constraints, enter emergency observation/kill-switch state and record the failure.

## 8. MEV protection and Jito execution

Use private Jito bundles for eligible entry, exit, and emergency transactions. Bundle composition is atomic: all required instructions succeed or the bundle reverts/fails without a partial user-visible trade. Simulate immediately before signing, use exact account state, route constraints, expiration, nonce/idempotency key, and tight slippage bounds. Default private-bundle slippage is 1–3%, selected by liquidity and route; exceeding the configured bound blocks submission. If the bundle cannot land cleanly without exposing the trade to sandwich risk, do not fall back to a public mempool path.

Dynamic tip bidding:

`Tip = min(Tip_max, α × EV_trade + Tip_base)`

where α is calibrated by landing probability, priority block-space scarcity, target-token executable liquidity, bundle competition, expected adverse selection, and urgency. `Tip_max`, `Tip_base`, and α are per-regime limits. A tip can never turn negative EV positive: require `EV_trade − Tip > 0` after stress costs. Bid increments are bounded, logged, and escalated only for an active emergency exit or expiring opportunity. Observe landed/failed bundle rates and recalibrate by validator/slot regime.

A private bundle materially reduces sandwich exposure but is not a mathematical guarantee against all protocol, validator, route, or key compromise. The operational invariant is: no public fallback, exact bounds, simulation, atomicity, and explicit failure if those protections are unavailable.

## 9. EV, paper trading, and validation

Replay only information available at decision time. Include latency distributions, failed transactions, fees, 10%, 15%, 20%, and 25% slippage stress, partial fills, depth limits, Jito tips, gap distributions, adverse selection, and provider outages. Validate out-of-sample and walk-forward across regimes. Report sample sizes, confidence intervals, calibration, leakage checks, survivorship/selection bias, excluded-regime results, and separate rug avoidance from profitability.

Targets remain targets, not promises: ~~85–95% rug avoidance and 40–55% realistic win rate~~, only if supported by sufficient cohorts. Promotion to real capital requires operator approval, reproducible replay, reconciliation tests, kill-switch tests, and demonstrated positive stressed expectancy.

> **MEASURED 2026-08-27 — the win-rate target is out by a factor of four to ten.**
> Buying every launch and holding to the end of the window, the real win rate is
> **11.1% entering at second 0, 9.6% at second 3, and 4.2% at second 50** — it falls
> the later you enter, and it is never near 40%. Expectancy is negative at every
> entry second. Nothing in this section was ever run: no file under `src-tauri/`
> has ever opened a real capture, `OperatingMode::Paper` is never constructed
> outside `#[cfg(test)]`, and `paper_trades` is empty. The validation doctrine
> above is good; it has simply never been exercised.

## 10. Non-custodial execution contract

> **NOT BUILT, as of 27 August 2026.** `execution.rs` could not produce a
> transaction a validator would accept. There is **no ed25519 anywhere in the
> crate** — the mock signer returns "sixty-four bytes over the message, not a
> signature; a receipt shaped like one", and its own header says the shipped
> application has no backend at all — and there is **no PDA derivation**, so the
> sell's account list is built out of label hashes. A real node would reject it. The
> message compiler, the account ordering and the `global:sell` discriminator are
> real and correct; the parts that make it a transaction are absent. There is also
> no entry-side transaction builder at all. `docs/VERDICT-2026-08-27.md`.

The executor is isolated from UI and analytics. Keys stay in OS-protected local storage and are never logged. Each signed decision envelope contains token/pool, side, size, tier, features, gate results, EV, stop, target, slippage, liquidity cap, route, expiry, tip cap, bundle mode, and correlation ID. Simulation is mandatory. Ambiguous confirmation freezes new orders and invokes reconciliation. Retries are idempotent. Configuration changes require authentication and audit logging.

Stops and staged take-profit plans are precomputed and immutable after entry except through an authenticated operator override. Kill-switch cancels eligible orders, blocks new submissions, and switches to observation-only mode through an independent local and UI path.


### 10.1 Partial Fills and Multi-Bundle State Machines

Partial fills are first-class: q_r = max(0, q - q_f), with per-leg receipts, average price, reserve watermark, and state version.

PLANNED -> SIMULATED -> SUBMITTED -> {UNFILLED, PARTIALLY_FILLED, FILLED, EXPIRED, FAILED}; PARTIALLY_FILLED -> RECONCILING -> {PARTIALLY_FILLED, FILLED, EXIT_ONLY}; any exposure -> STOP_PENDING -> EXITING -> {CLOSED, PARTIAL_EXIT, UNEXECUTABLE_EXIT}.

Every transition requires CAS(state_version,v,v+1) plus matching pool-reserve watermark, balances, slot bounds, and idempotency key. A mismatch invalidates the bundle and forces reconciliation/resimulation. Per-pool serialized execution leases use expiry and fencing tokens. After every partial fill, reconcile reserves, balances, fees, and residuals before increasing exposure; residuals are never silently treated as filled.

## 11. Forensic trade record and auditability

For every candidate, block, submission, fill, cancellation, stop, and take-profit event record signal/decision/submission/confirmation/fill timestamps, slots/signatures, route, depth, simulated and realized slippage, impact, fees, latency, tip, partial-fill state, EV, gap scenario, cluster breakdown, gate results, model versions, operator actions, and final outcome. Link the same correlation ID across SQLite, JSONL, RPC/Jito receipts, and UI. Never delete or rewrite raw telemetry.

## 12. 0x100x Command Centre

The UI is a transparent operator console, not a source of truth. Show live, delayed, stale, unknown, blocked, simulated, submitted, confirmed, partial, and failed states distinctly. Never render unknown as zero. Required views:

1. Active positions: fills, mark, P/L, stops, targets, exposure, route, confirmations, and freshness.
2. Signal ledger: tier, confidence vector, EV, stress results, entropy, diversity, cluster probability, absorption, depth, and gate reasons.
3. Forensic inspector: wallet graph, funding tree, CEX/mixer evidence, bundle membership, deployer lineage, and signatures.
4. Liquidity/execution: depth, modeled impact, stress bucket, route, bundle status, tip, and latency.
5. Risk/health: exposure, daily loss, queue depth, event-loop lag, provider p50/p95/p99, stale feeds, sink lag, and blocking gates.
6. Emergency controls: authenticated kill switch, cancel-new-orders, close/override, and immutable audit trail.

Every visualization drills to event IDs and displays source, timestamp, slot, confidence, freshness, and mathematical rationale.

## 13. Capital allocation and treasury

After realized profits are adjusted for losses, fees, taxes, and obligations, retain the default 50/50 split:

- 50% to pure savings/risk-free reserves, segregated from trading and infrastructure risk.
- 50% to measurable system reinvestment: low-latency RPC, redundancy, hardware, observability, forensic storage, replay, backtesting, and tooling.

Never distribute unrealized gains. Every reinvestment records bottleneck, cost, expected improvement, and validation metric. The €25 one-time social-data budget remains binding unless explicitly changed by the operator.

## 14. Testing, operations, and governance

Required tests cover unit, integration, property-based, replay, regression, load, failover, and chaos scenarios: missing/stale/contradictory telemetry; provider switching; p50/p95 breaches; ring-buffer saturation; SQLite contention; JSONL rotation; duplicate events; restart recovery; slot 0–10 and sub-$15k exclusion; ~~$25k–$80k regime~~; 30–75% curve boundaries; exact 50-block history; correlated CEX/mixer funding; entropy at 70%; absorbed/unabsorbed dumps; 1–3% bundle slippage; failed atomic bundles; -30%/-50% gaps; depth at 1.5%; emergency tip escalation; ambiguous confirmations; and kill-switch activation.

> **Note, 27 August 2026.** The "$25k–$80k regime" case is struck: that regime does
> not exist — see Annex F Phase 2 below and the status note at the top of this file.
> Two more things are true of this list as a whole. **Every test in the codebase
> runs on synthetic fixtures**; no file under `src-tauri/` has ever opened a real
> capture, and `fixtures.rs` opens with the words "launches that never happened", so
> "1,654 tests pass" means the engine agrees with itself. And a real test corpus
> cannot yet be built from the recordings we have — not because they are
> incomplete, but because they are **short**. The recorder drops nothing while it
> is connected (137 of 137 launches, checked block by block against the chain); the
> hole is uptime. **08-21 is not a day; it is 48 minutes spread over ten hours**, at
> hours that differ from every other day's, so **no day-level or era-level
> comparison drawn from this corpus should be believed** — that duty cycle is this
> dataset's single biggest source of false findings and it produced two of them in
> one night. Comparing a filter against buy-everything *within the same session* is
> the method that survives. What a real corpus needs is uninterrupted listening, not
> a better listener.

Operational dashboards alert on event-loop lag, queue occupancy, dropped critical events, sink lag, WAL growth, disk pressure, provider disagreement, slot lag, confidence drift, calibration drift, and unexpected zero-trade periods. **Note, 27 August 2026: for this market it is now the measured answer, and this rule as
written forbids reaching it.** Buying a launch loses about 10% a trade in every
hour-matched window from October 2024 to August 2026, and still loses 0.86% with every
cost set to exactly zero. Not trading returns 0.00% against −1.90% for a *perfect*
dead-coin detector, so not trading is the correct output here rather than a fault to be
engineered away. The same shape appears in at least four other clauses in this document
that treat a zero-trade state as an error condition. A rule that cannot conclude "there
was nothing here" cannot discover that there was nothing here.

A zero-trade period must be decomposed into genuine risk rejection, data outage, tier filtering, liquidity limits, and execution failure; it must never be accepted as evidence that the market had no opportunities.

## 15. Final operating doctrine

STS is evidence first, probabilistic where uncertainty is real, and absolute only where safety invariants demand it. It degrades confidence and size for imperfect but recoverable telemetry; it hard-blocks only when execution safety, forensic integrity, or loss containment cannot be established. It enters bases supported by independent accumulation and absorption, sizes against executable liquidity and empirical gaps, protects execution through private atomic bundles, and preserves a complete forensic record. The system may be fast, but it is never allowed to become opaque, custodial, or unmeasured.

# MASTER SPECIFICATION ANNEXES

## Annex A. Notation, units, and numerical conventions

All monetary values are represented internally as integer atomic units and converted to USD only at presentation boundaries. Every conversion stores the oracle source, quote timestamp, confidence, and FX/market pair. Percentages are decimals in equations: 1.5% is 0.015. Probabilities are in [0,1]. Prices are positive real numbers. Empty sets produce an explicit UNKNOWN result rather than zero.

Let:

- m be a token mint.
- p be a pool.
- t be an event time.
- s be a Solana slot.
- q be a candidate order quantity in tokens.
- Q be quote currency quantity in USD.
- L_exec be executable liquidity available within the allowed impact/slippage envelope.
- C_usd be the absolute capital cap for the decision.
- r be account risk budget in USD.
- d_gap be a realized adverse price displacement.
- f be total proportional fees.
- tau be latency in milliseconds.
- sigma be volatility in log-return units.
- U be an unknown-data indicator.

Every feature has: value, unit, source, observed_at, age, slot, confidence, missingness, and calculation version. A feature is not valid merely because its numeric value is present.

## Annex B. Exact expected-value model

### B.1 Trade-path decomposition

A trade is modeled as a set of mutually exclusive exit paths j in J. Paths include normal stop, target exits, partial fills, gap exits, failed exits, and forced emergency exits. Let p_j be the empirical probability of path j conditional on the information set available at entry, with sum_j p_j = 1 after including the no-exit path.

For path j:

`Payoff_j = Proceeds_j - EntryNotional - TradingFees_j - PriorityFees_j - JitoTip_j - SlippageCost_j - AdverseSelectionCost_j - FailureCost_j`

`EV = Σ(j in J) p_j × Payoff_j - DataRiskPenalty - ModelUncertaintyPenalty`

The data-risk and uncertainty penalties are not arbitrary losses. They are separately estimated from calibration error, unresolved missingness, and historical prediction error, and are set to zero only when the relevant validation cohort supports that decision.

### B.2 Empirical gap-down distribution

Let G be a discrete empirical gap variable with buckets:

`G ∈ {0 to 5%, 5 to 10%, 10 to 20%, 20 to 30%, 30 to 50%, greater than 50%, no executable exit}`

For a regime r and liquidity bucket l:

`P(G = g | r,l,x) = (n_g + κ × π_g) / (N + κ)`

where n_g is the observed count, N is the cohort count, π_g is the prior bucket probability, and κ is a predeclared smoothing strength. The model may condition on volatility, holder concentration, insider selling, depth depletion, and latency, but may not use future observations.

For a gap g, use a fill distribution rather than a point fill:

`F_price(y | g,x) = P(realized_exit_return ≤ y | gap=g, x)`

Then:

`EV_gap = Σ_g P(G=g|x) × E_y[Payoff(y,g,x)]`

The no-exit bucket includes failed route, depleted pool, halted execution, and confirmation ambiguity. Its payoff is modeled as the worst supported exposure path until a valid exit occurs, not as a clean stop.

### B.3 Slippage and impact functions

> **MEASURED 2026-08-27.** The maths below is right and the fear behind it was not.
> At 0.1 SOL our own order costs **0.04%**; a buy and a sell on the same
> constant-product curve cancel exactly, so a round trip is the two 1% fees and
> little else. See the note in section 7. `docs/VERDICT-2026-08-27.md`.

Let q be order size, D(a) be executable depth at price-impact level a, and I(q) be the smallest a such that D(a) >= q. A conservative impact estimate is:

`I(q) = max(I_empirical(q), I_curve(q), I_stress(q))`

For constant-product pools with reserves X and Y, ignoring fees for the first approximation:

`y_out(q) = Y - (X×Y)/(X+q)`

`P_mid = Y/X`

`Slippage_pct(q) = 1 - [y_out(q)/q] / P_mid`

The production estimate additionally includes pool fees, route fees, discrete ticks, transfer fees, priority fees, and observed adverse selection. For a multi-route order, optimize only among routes that meet exact slippage, authority, account, and atomicity constraints.

`SlippageCost = Q × (I(q) + route_fee + token_fee)`

### B.4 Adverse selection

Adverse selection measures post-submission price movement against the order after controlling for market movement:

`AS = sign(side) × [P_mid(t_fill + h) - P_mid(t_fill)] / P_mid(t_fill) - BenchmarkReturn(h)`

The production cost uses the conditional expected adverse move:

`AS_cost = Q × E[max(0, AS) | side, venue, latency, regime, liquidity_bucket]`

Use h values such as 1, 5, and 20 slots. Store the distribution, not only its mean.

### B.5 Fees and tips

`TotalCost = network_priority_fee + Jito_tip + aggregator_fee + pool_fee + token_transfer_fee + expected_failure_cost + slippage_cost + adverse_selection_cost`

The EV gate passes only when the lower confidence bound of EV remains positive after the configured stress scenario. A positive point estimate with a negative lower bound is Tier 3 or blocked according to governance.

## Annex C. Dynamic Jito tip pricing

> **MEASURED 2026-08-27 — paying more does not buy an earlier slot.** Paired within
> the same launch, the higher-fee transaction landed earlier **50.3%** of the time
> inside the first 25 slots, on 76,795 such pairs. A coin flip. Over all 303,681
> pairs at any distance it is 52.3% — statistically real, economically nothing. The
> chance of landing in the launch slot by fee paid is a **U-shape, not a ramp**
> (zero fee 18.3%, 10–50k lamports 0.5%, 1–5M 19.0%), and splitting each wallet's
> own trades into cheap and expensive halves moves the odds only 2.1% → 4.5% — so
> the top of that U is *which wallet it is*, not what it paid. This was measured on
> ordinary priority fees, not on Jito tips, so the tip formula below is not
> disproved; what is disproved is the assumption that spending more reliably buys
> position. Separately, **the front of this market is won by bundles and we are not
> in that race at all**: 94.7% of late buyers — 1,169 of 1,234 wallets — never once
> contest a launch slot, and on a free public RPC from a home laptop we are
> permanently in the late group. `docs/VERDICT-2026-08-27.md`.

### C.1 Variables

For candidate c and target slot k:

- `EV_net`: expected profit before the Jito tip but after all other costs.
- `B_k`: priority block-space scarcity score in [0,1].
- `L`: executable target-token liquidity normalized to the strategy liquidity baseline.
- `R`: urgency score based on opportunity decay or emergency exit severity.
- `W`: recent landing probability for the selected block engine/validator path.
- `A`: expected adverse-selection cost if delayed.
- `Comp`: bundle competition score.
- `Tip_base`: minimum operational tip in lamports.
- `Tip_max`: policy maximum in lamports.
- `α`: regime-calibrated EV participation coefficient.

Define:

`α_eff = clamp(α0 × (1 + β_B B_k + β_R R + β_A A + β_C Comp) × W_adjust, α_min, α_max)`

`W_adjust = clamp(1/W, 1, W_max_adjust)`

`LiquidityFactor = clamp(L_ref / max(L, ε), 0, L_max)`

The production bid is:

`Tip_raw = Tip_base + α_eff × max(0, EV_net) × LiquidityFactor`

`Tip_bid = min(Tip_max, max(Tip_base, Tip_raw))`

The simpler policy form remains:

`Tip_bid = min(Tip_max, α × EV_trade + Tip_base)`

but α must be expanded into the observable terms above in the audit record.

### C.2 Edge cases

If EV_net is missing, negative, or stale, no discretionary tip is allowed; only a pre-authorized emergency ceiling may apply. If W is unavailable, the bundle is blocked unless an emergency policy explicitly permits Tip_base. If Tip_bid exceeds EV_net after converting units, block because the tip would consume the trade edge. If Tip_max is missing or malformed, block. If a bundle is retried, the idempotency key remains stable while the tip escalation sequence increments monotonically.

For emergency exits:

`Tip_n = min(Tip_emergency_max, Tip_0 + n × ΔTip + γ × loss_rate × urgency)`

where n is the retry index, ΔTip is a fixed or liquidity-scaled increment, and loss_rate is the current mark-to-market loss rate. Each retry requires fresh simulation and an unchanged route/slippage bound. Escalation cannot relax safety bounds.

## Annex D. Holder entropy, concentration, and Sybil correlation

### D.1 Shannon entropy

Let h_i be the non-excluded token balance of holder i and H_total = Σ_i h_i. Define p_i = h_i/H_total. Shannon entropy is:

`H = -Σ_i p_i × ln(p_i)`

Normalized entropy is:

`H_norm = H / ln(N)`

for N non-zero holders. If N <= 1, H_norm = 0. Effective holder count is:

`N_eff = exp(H)`

Entropy is reported alongside top-holder share because a large number of dust holders can inflate N without providing independence.

### D.2 HHI and concentration

`HHI = Σ_i (100 × p_i)^2`

`TopKShare = Σ_(i=1..K) p_i`

The system separates protocol-owned, locked, liquidity, burn, and known infrastructure accounts before calculating the risk population. Exclusion rules are versioned and reviewable.

### D.3 Buyer diversity index

For a window w, let v_i be the buy volume attributed to independent buyer entity i:

`BDI_w = 1 - Σ_i (v_i / Σ_j v_j)^2`

`BDI_wallet_count = N_independent / max(N_observed,1)`

The reported diversity is a weighted blend with confidence intervals. Multiple wallets linked by funding or behavior are one entity for diversity calculations.

### D.4 CEX clustering probability

For wallet i and candidate cluster C, create features:

`x = [shared_parent, time_proximity, amount_similarity, fanout_similarity, synchronized_entry, instruction_similarity, shared_exit, common_counterparty, known_cex_origin]`

A calibrated logistic score is:

`z = b + Σ_j w_j x_j + Σ_(j<k) w_jk x_j x_k`

`P_cluster = 1/(1+exp(-z))`

Use monotonic constraints where evidence requires them, such as shared parent increasing risk. Store feature provenance and uncertainty. Cluster posterior for a group is not the arithmetic mean; use a noisy-OR for independent evidence:

`P_group = 1 - Π_i (1 - P_cluster_i)`

with correlation correction when features are not independent.

### D.5 Solcat funding-tree graph math

Represent the funding history as directed graph G=(V,E), where vertices are wallets, exchanges, bridges, programs, and token accounts. Edge e=(u,v) has amount a_e, timestamp t_e, asset type, transaction signature, and confidence c_e.

For a wallet v, define discounted path influence from root r:

`I(r→v) = Σ_paths p(r,v) [Π_e∈p c_e × exp(-λ × age(p)) × min(1, flow(p)/threshold)]`

Avoid double-counting by retaining the maximum-confidence path plus additive independent corroboration. The parent posterior is:

`P(parent=r | v) = I(r→v) / Σ_q I(q→v)`

A fanout risk score combines root concentration, time-window density, flow similarity, and synchronized entry. Graph traversal has bounded depth, cycle detection, and an explicit unresolved branch state. It never labels a wallet solely because it transited a regulated exchange.

## Annex E. Microstructure and absorption formulas

### E.1 Volume profile

Partition price into bins b with lower/upper boundaries. For trade j with volume V_j and price P_j:

`VP(b) = Σ_j V_j × 1[P_j ∈ b]`

Separate buys and sells:

`VP_buy(b)=Σ_j V_j×1[buy_j]×1[P_j∈b]`

`VP_sell(b)=Σ_j V_j×1[sell_j]×1[P_j∈b]`

Normalize by window volume. Point of control is argmax_b VP(b). Value area is the smallest contiguous bin range containing the configured fraction, normally 70%, of volume.

### E.2 Volume delta

`Delta_w = BuyVolume_w - SellVolume_w`

`DeltaRatio_w = Delta_w / max(BuyVolume_w + SellVolume_w, ε)`

Use wallet-quality weights q_i only as a secondary view:

`WeightedDelta = Σ_j q(wallet_j) × signed_volume_j`

Never allow a wallet-quality classifier to manufacture volume.

### E.3 Dip-Absorption Ratio

Define the first major selloff window D from the first statistically significant cluster sell event. Let V_sell,D be cluster-adjusted sell volume and V_absorb,D be buy volume from independent entities within the recovery horizon.

`DAR = V_absorb,D / max(V_sell,D, ε)`

Price resilience:

`Resilience = (P_recovery - P_low) / max(P_pre_dump - P_low, ε)`

A robust absorption score is:

`AbsorptionScore = sigmoid(a0 + a1×log(DAR) + a2×Resilience + a3×ΔBDI + a4×HigherLow - a5×correlated_buy_share)`

DAR alone is insufficient. A high ratio driven by correlated wallets or wash volume fails validation.

### E.4 VPIN and orderflow toxicity

Partition volume into equal-volume buckets of size V*. For bucket k:

`OI_k = |BuyVolume_k - SellVolume_k| / V*`

`VPIN = (1/n) × Σ_(k=1..n) OI_k`

Use VPIN as toxicity/imbalance evidence, not as a directional oracle. High VPIN with falling diversity, widening impact, and insider distribution is a block. High VPIN with independent accumulation and stable depth may indicate informed demand but still requires EV validation.

### E.5 Volatility compression

For returns r_t over window w:

`RealizedVol_w = sqrt(annualization × variance(r_t))`

`CompressionRatio = RealizedVol_short / max(RealizedVol_long, ε)`

A base candidate requires compression below the regime percentile threshold, a bounded high-low range, no expanding downside impact, and sufficient observations. Thresholds are learned by regime and frozen for evaluation; they are not adjusted after seeing outcomes.

## Annex F. Lifecycle state machine

### Phase 0: Slot 0–10 sniper death match — excluded

> **MEASURED 2026-08-27 — excluding it is right, but not for the reason given.**
> The launch block is in fact **the only positive entry bucket in the whole market**
> (+9.5%, declining monotonically after it). It pays us nothing anyway: the median
> same-block buyer at 0.05–0.5 SOL returns **exactly −1.90%**, which is the round
> trip and nothing else — because a buy and a sell on a constant-product curve
> return your SOL exactly, so the loss *is* the toll. The median only turns positive
> at 5+ SOL, and 5 SOL is five times the whole bankroll. It is a **size** effect we
> cannot reach, not a speed one. `docs/VERDICT-2026-08-27.md`.

Purpose is telemetry only. Do not auto-enter. Record launches, creator, initial buyers, funding trees, bundled entries, and early liquidity. Invalidation includes missing observations, extreme impact, or any authority contradiction. Phase 0 data can seed later forensic context but cannot become a Tier 1 signal.

### Phase 1: Initial dump and absorption — monitored

Start after the first meaningful insider/sniper distribution or configured volume shock. Measure DAR, resilience, independent wallet growth, higher lows, depth recovery, and toxicity. Entry is prohibited until the dump is classified absorbed, unabsorbed, or unresolved. Unresolved is not equivalent to absorbed.

### Phase 2: $25k–$80k consolidation base — target regime

> **DISPROVED 2026-08-27.** The "consolidation base" does not exist: median **89
> seconds** from launch to $25k, and 3 of 53 coins still above it after twelve
> hours. It is a moment, not a phase, so nothing below it has a base to measure.
> Entering on the band crossing returns −8.43% net against −11.40% for buying
> everything — no edge and still a loss, on n=86 with a wide interval.
> See `docs/VERDICT-2026-08-27.md`.

Eligibility requires market capitalization or pool-value proxy inside the configured band, adequate history, stable authority state, no hard forensic block, executable depth, compression, positive or recovering delta, improving BDI, and positive stressed EV. Entry triggers include base reclaim, confirmed higher low, depth replenishment, independent-wallet expansion, and validated route simulation.

Invalidate on lower-low breakdown with deteriorating depth, renewed correlated distribution, authority change, concentration breach, toxicity spike without absorption, provider disagreement, or EV lower confidence bound crossing zero.

### Phase 3: Migration and price discovery — scaling

Migration requires route and LP verification. Scale only after post-migration price discovery confirms depth, independent demand, and executable exits. Do not assume bonding-curve success implies Raydium safety. Recompute all features after migration and reduce size during route uncertainty.

## Annex G. Adaptive stops and profit ladder

> **DISPROVED 2026-08-27, and never built.** Exits are not where the missing edge
> is. Graded over the full twelve hours on real price paths, **all 150 grid cells
> lose**; the best is take-1.5x-with-a-0.9-stop at **−3.38% net**, against a
> **+2.12%** break-even. The best of 108 rules inside a 60-second window loses
> 2.32%, and the same grid run on *scrambled* seconds loses 2.75–3.21% — so every
> real pattern in how these coins move is worth about **0.4 to 0.9 points**.
> Holding for hours costs a further 0.32 points and ties up capital 500 times
> longer. Coins that are up at 60 seconds do not stay up.
>
> Two traps for anyone re-running this. **There is no resting stop order on
> pump.fun**: you watch the price break, then send a transaction, and you fill at
> whatever the market is doing when it lands — pricing a stop as though it fills
> *at* the stop level is what produced this project's only positive out-of-sample
> number, and it turned negative when priced honestly. And **"a 5% trailing stop"
> is not one rule** — the running peak can track each second's close or its high,
> and the stop can be tested against the close or the low; those four combinations
> spread the answer by six points, which is three times the whole gap to break-even.
> Any future exit number in this project must state both choices or it is not a
> number.
>
> None of this is implemented: there is no stop, no target, no trailing stop and no
> time exit anywhere in `src-tauri/src/`. The daemon's only exit is
> flatten-at-end. `docs/VERDICT-2026-08-27.md`.

### G.1 Dynamic stop

Let ATR_p be realized average true range in price units, V_void be the nearest liquidity void distance, and D_score be executable-depth quality. Define:

`StopDistance = clamp(k_ATR × ATR_p + k_void × V_void, Stop_min, Stop_max(D_score))`

The stop is also bounded by the strategy loss cap and cannot be placed inside ordinary microstructure noise. A stop is invalid if the route cannot be simulated or if the projected impact exceeds policy.

### G.2 Gap emergency protocol

On stop breach, freeze new entries, mark the position emergency, select the precomputed private exit, simulate, and submit with Tip_0. If not landed, retry with bounded Tip_n while preserving exact slippage and route limits. Reconcile every signature. If the pool is depleted, estimate no-exit exposure and escalate to operator emergency controls; never report the stop as filled when it was not.

### G.3 Take-profit ladder

The default ladder is precomputed from entry and risk unit R:

- 2.0R or approximately 2x thesis target: sell 25%.
- 3.5R: sell 35%.
- 5.0R or greater: sell 30%.
- Retain 10% as a runner subject to trailing stop and liquidity constraints.

The percentages are policy defaults and must be stress-tested by liquidity. A ladder may be reduced or blocked if staged exits exceed depth. Each fill recalculates remaining risk, but never increases total position size.

## Annex H. Async systems implementation requirements

### H.1 Ring-buffer memory layout

Each slot contains sequence, event pointer or compact struct, source ID, event type, timestamp, durability class, and checksum. Producers claim sequences atomically. The consumer advances only after complete publication. False sharing is reduced with cache-line padding. A bounded capacity is mandatory; unbounded queues hide outages and create eventual memory death.

Durability classes are critical, important, and observational. Critical events include signatures, orders, fills, stops, authority changes, and gate decisions. Critical events cannot be shed. Observational metrics may be sampled under saturation with a loss counter.

### H.2 Worker separation

Worker 1 performs normalization and deduplication. Worker 2 performs SQLite transactions. Worker 3 performs JSONL serialization and rotation. Worker 4 performs feature aggregation. Worker 5 performs reconciliation. The execution worker has separate scheduling and cannot be starved by analytics. IPC messages carry correlation IDs and deadlines.

### H.3 Node.js SQLite configuration

The production connection applies:

`PRAGMA journal_mode = WAL;`

`PRAGMA synchronous = NORMAL;`

`PRAGMA busy_timeout = 5000;`

`PRAGMA journal_size_limit = 67108864;`

`PRAGMA wal_autocheckpoint = 1000;`

Actual values are configuration-managed and recorded at startup. The writer uses prepared statements and transactions. Batch size is min(500 events, events accumulated for 100 ms). Long-running analytical reads use snapshots or replicas and never share the write transaction.

### H.4 Hot-cache layout

The cache contains LRU maps for mint state, pool state, wallet state, provider health, active positions, and current feature vectors. Each record has last_seen_slot, last_seen_time, TTL, version, source quorum, and invalidation reason. Fast indexes include mint→pool, pool→mint, wallet→cluster, cluster→wallets, signature→event, and position→correlation ID.

LRU eviction is size-aware. Critical active positions are pinned. TTL expiry changes state to STALE before deletion, allowing the UI to show what was known. Authority changes, migration, pool closure, reorg, or explicit reconciliation invalidate dependent keys transitively.

## Annex I. RPC health and quorum matrix

For each provider p:

`Health_p = w_l×LatencyScore_p + w_s×SlotFreshness_p + w_e×(1-ErrorRate_p) + w_c×Consistency_p + w_sim×SimulationSuccess_p`

Provider states are healthy, degraded, unhealthy, quarantined, and recovering. A provider cannot recover on latency alone if its slot or data consistency is bad.

Critical facts require two-source agreement when available. If sources disagree, retain both observations, mark the feature contradictory, and block the dependent execution. Non-critical derived features may use the highest-health source with a degraded confidence penalty.

Fallback matrix:

1. Primary low-latency gRPC.
2. Secondary independent gRPC.
3. WebSocket stream plus RPC backfill.
4. HTTPS RPC with bounded timeout.
5. Observation-only mode.

No fallback changes unknown into safe. Failover decisions, thresholds, and recovery windows are persisted.

## Annex J. Jito bundle pipeline

> **NOT BUILT, as of 27 August 2026.** There is no Jito client: no HTTP, no gRPC,
> no endpoint, and the crate has no HTTP dependency at all. What exists is a
> tip-floor function you must feed by hand and a retention timer over bundle *ID
> strings*; `BundleRecord` contains no transactions and nothing is ever bundled.
> The MEV simulator that appears to justify this pipeline states at the top of its
> own 3,387 lines: *"Nothing in here is a measurement."* Every constant in it
> declares that it was invented, which is good practice and is also the answer to
> anyone who quotes one. `docs/VERDICT-2026-08-27.md`.

1. Freeze decision envelope and assign idempotency key.
2. Fetch fresh accounts and verify slot freshness.
3. Simulate route and every instruction.
4. Validate slippage, depth, token-account ownership, authority state, and compute budget.
5. Calculate EV after priority fee and candidate tip.
6. Sign locally in isolated executor.
7. Submit privately to configured block engine.
8. Track bundle UUID, target slot, tip, status, and receipts.
9. Reconcile landed transaction signatures and actual fills.
10. If not landed before expiry, apply policy-specific retry or cancel.

A sandwich-protection failure is a failed execution, not permission to broaden slippage. Atomicity means the intended instruction set cannot partially complete in a way that creates an unhedged user position; the system must still reconcile protocol-specific semantics.

## Annex K. 0x100x design system

### K.1 Visual language

The Command Centre is a dark, forensic, high-density control surface. The design system uses near-black graphite surfaces, warm off-white primary text, muted slate secondary text, restrained signal colors, and one electric accent reserved for actionable state. Grain is a low-opacity texture layer applied consistently, never to the point of reducing legibility. Radial glow is used behind focal risk and execution modules, not as decoration around every component.

Typography uses a condensed display face for section labels and a highly legible monospace or tabular numeral face for telemetry. Numeric columns align on decimal points. Labels are sentence case. Color is redundant with icons, text, and border state so operators with color-vision deficiency can distinguish statuses.

### K.2 Layout and tokens

Use a 12-column desktop grid, 8-point spacing scale, 16-pixel base radius for cards, and 1-pixel hairline borders. Primary dashboard regions are status rail, signal/market workspace, forensic inspector, and execution/risk rail. Critical actions remain visible without scrolling. Density may be changed by operator preference but not by hiding safety data.

Tokens include surface-0 through surface-4, text-primary/secondary/muted, accent-cyan, positive-green, warning-amber, blocked-red, unknown-violet, and focus-white. Glow intensity, grain opacity, border alpha, and animation duration are tokens. Motion is disabled or reduced under prefers-reduced-motion. Live indicators pulse only when data is genuinely fresh.

### K.3 Forensic Inspector

The Inspector has synchronized panes:

- Graph canvas: wallets as nodes, transfers as directed edges, edge width as normalized flow, edge opacity as confidence, and time scrubber.
- Cluster panel: cluster posterior, shared ancestors, entry window, correlated volume, exit behavior, and unresolved branches.
- Funding provenance: CEX/bridge/mixer classification, transaction signatures, path influence, and confidence intervals.
- Holder panel: entropy, HHI, top-K shares, effective holder count, independent entity count, and concentration history.
- Liquidity panel: depth curve, price-impact curve, current executable participation, simulated exit, and stress scenarios.

Every node and edge opens raw evidence. Graph layout must remain deterministic for the same data/version to support forensic screenshots and replay.

### K.4 Live feed and signal ledger

The live feed is append-only from the operator perspective and shows source, slot, age, event class, severity, and correlation ID. The signal ledger is sortable by tier, EV lower bound, liquidity, DAR, BDI, cluster probability, and freshness. Filters never delete data; they alter the view and display the active filter count.

### K.5 Kill switch and overrides

The kill switch requires deliberate authenticated activation, confirmation, and reason. Emergency mode cancels eligible new orders, blocks new submissions, freezes automated scaling, and exposes close-position actions. Existing risk monitors continue running so the system can report exposure. An override requires operator identity, reason, expiry, affected position, and exact fields changed. Overrides cannot erase forensic findings, bypass private-execution requirements, or remove an audit record.

## Annex L. Paper-trading promotion gates

> **NEVER BUILT, as of 27 August 2026.** There is no paper mode.
> `OperatingMode::Paper` is never constructed outside `#[cfg(test)]` — not gated
> off, never written — which is why the `paper_trades` table is empty and why no
> gate in this annex has ever been run. Two further things would have to be fixed
> before it could mean anything. **The calendar-day split is not an out-of-sample
> test**: the seven calendar files are nine capture sessions, six usable, and 21
> August is the tail of the 20 August run past midnight — one recorder process
> across the boundary, 14.98 hours, no restart. **A genuine holdout does exist and
> nobody used it: 08-16, behind 3.78 days of silence and a different set of
> recorder processes** — a holdout in time rather than in population, since 81.2%
> of its clean coins share a wallet with the fitting days, and with `funding`
> coverage of 4.3% so no wallet-graph feature can be held out there. The method that does work on this corpus is comparing a
> filter against buy-everything **within the same session**, which cancels the
> session effect and resolves a 3-point edge at 150+ coins a cell. And **14% of
> outcomes are silently truncated** when the listener stops.
> `docs/VERDICT-2026-08-27.md`.

Promotion occurs in stages: replay-only, shadow live, paper execution, capped real capital, and governed expansion. Each stage requires deterministic pass/fail criteria.

Minimum statistical report:

- predeclared cohort and inclusion/exclusion rules;
- number of candidates, trades, rugs, wins, losses, and unresolved outcomes;
- median and tail slippage;
- gap distribution and no-exit frequency;
- calibration curve by confidence tier;
- precision/recall for risk findings;
- Brier score and confidence intervals;
- maximum drawdown, CVaR, expected shortfall, and recovery time;
- expectancy net of fees, priority fees, tips, failed transactions, and adverse selection;
- walk-forward and out-of-sample results;
- leakage, survivorship, selection, and look-ahead audits;
- provider outage and latency sensitivity;
- results separated by lifecycle phase and liquidity bucket.

A promotion gate fails if any hard safety invariant fails, if realized execution materially diverges from simulation without a controlled explanation, if confidence is miscalibrated, if the kill switch is unreliable, or if persistence reconciliation is incomplete. Sample size alone never proves safety; uncertainty must be quantified.

## Annex M. Treasury and capital governance

The 50/50 split applies only to realized, settled profit after losses, fees, taxes, and obligations. The reserve half is held outside active trading risk and is not used as margin, collateral, or emergency trading capital without a separately approved governance event.

The reinvestment half is allocated through measured proposals. Each proposal includes problem statement, baseline metric, cost, expected effect, rollback plan, owner, and review date. Eligible categories include RPC redundancy, hardware, storage, observability, backtesting, data quality, security, and operator ergonomics. Cosmetic spending without a measured bottleneck is rejected.

Treasury records are append-only and reconcile to wallet/account statements. Unrealized gains do not qualify. The 50/50 default can be changed only by an authenticated, recorded operator decision with rationale and effective date.

## Annex N. Failure modes and recovery doctrine

A stale feed causes visible stale state and confidence degradation. A contradictory authority read blocks authority-dependent execution. A full ring buffer triggers backpressure and a critical alarm. A locked SQLite database retries within busy_timeout and then enters degraded persistence mode without blocking risk monitoring. A JSONL write failure marks the forensic sink unhealthy and blocks new real-capital execution until reconciliation is complete. A Jito failure never silently becomes public submission. An ambiguous signature freezes new orders until chain state and local state agree. A kill-switch failure is a critical incident requiring immediate local isolation.

Recovery is an explicit state transition: detect, contain, preserve evidence, reconcile, validate health, replay missed events, re-run affected decisions, and obtain operator release. “Recovered” means all critical events are durable, state is consistent, and the relevant test suite passes.

## Annex O. Definitive doctrine

STS does not confuse speed with quality, wallet count with independence, volume with demand, a stop instruction with an executable exit, or a confidence number with certainty. It makes uncertainty measurable, prices execution friction, sizes against executable liquidity, models discontinuous loss, protects private execution, and preserves a complete evidence chain. It is allowed to degrade; it is not allowed to hallucinate safety. It is allowed to miss a trade; it is not allowed to hide why. It compounds only after realized profit, statistical validation, and operational reliability have earned the right to scale.

## Annex P. Production data contracts

Every event envelope includes the following mandatory fields:

- event_id: globally unique deterministic identifier;
- schema_version: immutable schema integer;
- source: provider and stream name;
- observed_at: UTC timestamp with millisecond precision;
- slot: non-negative integer;
- signature: base58 transaction signature where applicable;
- instruction_index: integer or explicit null;
- mint and pool: canonical addresses or explicit null;
- event_type: controlled vocabulary value;
- raw_payload_hash: cryptographic digest of canonical raw bytes;
- derived_version: feature pipeline version;
- durability_class: critical, important, or observational;
- parent_event_ids: causal references;
- integrity_hash: chained digest of prior envelope and current canonical fields.

An event is rejected from the critical path if the address is malformed, timestamp order is impossible beyond clock tolerance, slot regresses without a reorg marker, or the integrity hash cannot be computed. Rejection itself is an event.

### P.1 Decision envelope

A decision envelope includes:

- decision_id and correlation_id;
- mint, pool, lifecycle phase, and decision expiry;
- confidence vector and aggregate calibrated confidence;
- confidence tier;
- every hard gate result;
- every soft degradation and missingness flag;
- feature values, sources, ages, and model versions;
- EV point estimate, lower bound, upper bound, and stress assumptions;
- position-size candidates and binding constraint;
- entry route, exit route, slippage limit, priority-fee cap, and Jito-tip cap;
- stop model, gap model, take-profit ladder, and emergency bundle IDs;
- operator authorization state;
- signing policy and idempotency key.

A decision envelope is immutable after signing. Corrections produce a superseding envelope linked to the prior decision; they do not rewrite history.

## Annex Q. Complete gate taxonomy

### Q.1 Identity gates

G-001: mint address parses and resolves.

G-002: pool address parses and resolves.

G-003: token program and pool program match the supported allowlist.

G-004: slot and timestamp are fresh within policy.

G-005: all critical facts have source quorum or an approved single-source exception.

### Q.2 Authority gates

G-101: mint authority state is known.

G-102: freeze authority state is known.

G-103: authority behavior matches the permitted launch profile.

G-104: no unauthorized authority change occurred during the decision window.

G-105: metadata mutability and upgrade state are known where relevant.

### Q.3 Distribution gates

G-201: supply reconciles to observed balances within tolerance.

G-202: excluded-account policy is applied and versioned.

G-203: concentration is below the configured maximum.

G-204: entropy and effective holder count meet phase requirements.

G-205: non-correlated holder share meets the minimum policy.

G-206: cluster posterior is below the hard-block threshold or is explicitly quarantined.

### Q.4 Liquidity gates

G-301: executable depth is available on the selected route.

G-302: 1.5% liquidity participation cap is calculable.

G-303: modeled impact satisfies the configured bound.

G-304: emergency exit route is simulated.

G-305: depth remains valid after proposed entry.

G-306: partial-fill and no-exit scenarios are included in EV.

### Q.5 Market gates

G-401: lifecycle phase is classified.

G-402: Phase 0 exclusion is enforced.

G-403: accumulation base meets minimum duration and observation count.

G-404: volatility compression is genuine and not caused by missing trades.

G-405: delta, BDI, turnover, and depth show non-circular participation.

G-406: first major dump is absorbed or the candidate remains blocked.

### Q.6 Execution gates

G-501: route simulation succeeds.

G-502: exact slippage bound is enforceable.

G-503: private bundle path is available when required.

G-504: compute and priority budgets fit limits.

G-505: tip is below the EV-preserving maximum.

G-506: key isolation and signing service health are green.

G-507: idempotency key has not been used.

### Q.7 Risk gates

G-601: daily loss and concurrent exposure limits pass.

G-602: stressed loss fits the account risk budget.

G-603: CVaR and expected shortfall fit policy.

G-604: gap-down distribution has sufficient cohort support.

G-605: stop and emergency exit are executable or the trade is blocked.

G-606: take-profit ladder fits depth and route constraints.

Every gate emits PASS, FAIL, DEGRADED, or UNKNOWN. UNKNOWN never becomes PASS through defaulting.

## Annex R. Confidence-vector calibration

Let the confidence vector be:

`c = [c_forensic, c_distribution, c_liquidity, c_microstructure, c_execution, c_EV, c_social]`

Each component is calibrated separately against a declared outcome. For example, c_execution predicts probability that a simulated accepted order lands within the stated bound; c_forensic predicts probability that the token has no policy-defined rug event during the observation horizon.

The aggregate may be a weighted geometric mean to prevent one strong component from masking a weak one:

`C = Π_i (max(c_i, ε))^(w_i)`

with Σ_i w_i=1. Apply a missingness multiplier:

`C_adj = C × Π_k (1 - ρ_k × missing_k)`

where missing_k is 1 only when the feature is materially missing, and ρ_k is validated. Hard invariants remain outside this aggregation. A strong C cannot override a failed hard gate.

Calibration is performed on temporally separated data. Confidence bins must report predicted probability, observed frequency, count, standard error, and calibration error. Drift triggers retraining review, not silent weight changes.

## Annex S. Load, latency, and soak test matrix

Test ID L-001: sustain configured ingress rate for one hour with no persistence backlog.

Test ID L-002: burst at 5x nominal rate for ten seconds; verify bounded memory and backpressure.

Test ID L-003: inject 10% duplicate events; verify idempotent deduplication.

Test ID L-004: delay SQLite writer by 2 seconds; verify ingress remains non-blocking and critical queue alarms fire.

Test ID L-005: make JSONL sink unavailable; verify audit state, degradation, and recovery reconciliation.

Test ID L-006: force WAL growth beyond journal_size_limit; verify checkpoint policy and disk alarm.

Test ID L-007: terminate the persistence worker during a batch; verify atomicity and replay.

Test ID L-008: restart the process while the ring buffer contains unpublished events; verify durability-boundary semantics.

Test ID L-009: inject provider p95 latency of 360 ms; verify tier downgrade without universal blocking.

Test ID L-010: inject provider p95 latency of 550 ms; verify failover and critical-fact quorum.

Test ID L-011: return inconsistent holder balances from two providers; verify contradiction state.

Test ID L-012: delay UI transport; verify stale indicator without altering execution state.

Test ID L-013: flood non-critical telemetry; verify critical events are never shed.

Test ID L-014: fill cache to capacity; verify pinned positions remain and LRU eviction is observable.

Test ID L-015: expire pool TTL; verify dependent features transition to STALE and execution blocks.

Test ID L-016: simulate clock skew; verify freshness uses trusted time and records skew.

Test ID L-017: simulate fork/reorg marker; verify rollback/replay without deleting raw observations.

Test ID L-018: run a 24-hour soak with rolling metrics; verify no memory leak, queue drift, or silent event loss.

## Annex T. Execution and emergency runbooks

### T.1 Normal entry

1. Confirm phase and eligibility.
2. Recompute critical balances and depth.
3. Recompute EV and lower bound.
4. Confirm confidence tier.
5. Construct exact route.
6. Construct emergency exit.
7. Simulate entry and exit.
8. Calculate priority fee and Jito tip.
9. Check account and daily limits.
10. Sign in isolated executor.
11. Submit privately.
12. Reconcile fill.
13. Register stop and take-profit ladder.
14. Publish UI state and audit event.

### T.2 Stop breach

1. Mark position STOP_BREACH_PENDING.
2. Freeze new exposure for the affected mint.
3. Fetch fresh pool and account state.
4. Select the precomputed emergency bundle.
5. Apply tip escalation index zero.
6. Simulate and sign.
7. Submit privately before expiry.
8. Verify landed signature and actual amount received.
9. If not landed, increment escalation within limits.
10. If route becomes invalid, cancel retries and enter NO_EXECUTABLE_EXIT.
11. Notify operator with exact evidence.
12. Recompute account risk and persist final state.

### T.3 Ambiguous transaction

Do not resend automatically. Query independent RPC sources, bundle status, block inclusion, token balances, and signature status. If any source disagrees, freeze the idempotency key and related mint. Resolve with a single authoritative reconciliation record. Only a confirmed absence of execution permits a new idempotent submission.

### T.4 Kill switch

Activation writes a critical event before attempting cancellation. The executor rejects new envelopes, cancels eligible unlanded bundles, freezes automated scaling, and exposes manual close actions. Risk monitors continue. Recovery requires operator authentication, incident review, state reconciliation, and a new release of the execution policy.

## Annex U. Security model

Threats include key exfiltration, malicious UI, compromised RPC, corrupted indexer data, replayed decisions, duplicate submission, bundle spoofing, provider collusion, local privilege escalation, and audit tampering.

Controls include process isolation, least-privilege IPC, OS secret storage, encrypted local configuration, signature verification, nonce/idempotency, canonical serialization, hash-chain audit records, two-source validation for critical facts, dependency pinning, patch review, secret rotation, and offline recovery procedures.

The UI never receives private keys. Analytics never receives signing capability. The persistence worker cannot submit transactions. The execution worker cannot modify raw telemetry. Configuration changes are separate from data events and require authenticated approval.

## Annex V. Observability metrics

Ingress metrics: events per second, accepted, duplicate, rejected, gap count, source lag, ring capacity, producer contention, consumer delay.

Persistence metrics: batch size, batch duration, commit latency, WAL size, checkpoint duration, JSONL flush latency, sink lag, reconciliation mismatch, disk usage.

Market metrics: pool depth, impact curve, volume profile, delta, BDI, entropy, DAR, VPIN, turnover, volatility compression, gap bucket counts.

Execution metrics: simulation latency, signing latency, bundle submission latency, landing probability, tip, priority fee, slippage, adverse selection, partial fill, confirmation time, retry count, no-exit count.

Risk metrics: exposure, daily loss, stressed loss, CVaR, expected shortfall, confidence drift, tier distribution, blocked reason distribution, zero-trade decomposition.

All metrics carry source, period, aggregation method, and reset semantics. Dashboards distinguish unavailable from zero.

## Annex W. Statistical definitions of outcomes

A rug event is defined before evaluation and may include authority abuse, liquidity removal, irreversible exit impairment, coordinated insider distribution beyond policy, or a price/liquidity collapse within the declared horizon. Multiple rug definitions are reported separately.

A win is a fully reconciled position whose realized net P/L exceeds zero after all costs. A loss includes gap loss, failed exit, partial fill, tip, priority fee, and adverse selection. An unresolved outcome remains unresolved until the observation horizon expires or an operator adjudicates it; it is never silently excluded.

Rug avoidance and profitability use different denominators. Confidence intervals use appropriate binomial, bootstrap, or robust time-series methods. Repeated launches from one deployer are clustered in inference to avoid overstating independent sample size.

## Annex X. Acceptance checklist

The system is production-ready only when:

- raw event replay reproduces feature values;
- critical sinks reconcile;
- no synchronous disk operation exists on the hot path;
- SQLite WAL and batch behavior are verified under contention;
- provider failover and quorum are tested;
- confidence calibration is documented;
- every hard gate has a test and reason code;
- entropy, clustering, funding graph, DAR, VPIN, liquidity, EV, CVaR, and gap models have source data;
- private bundles simulate and fail safely;
- public fallback is impossible by policy and code;
- emergency exits are precomputed and exercised;
- dynamic stops and profit ladders respect depth;
- kill switch works from independent paths;
- keys remain isolated;
- 0x100x UI exposes every safety state;
- paper-trading gates pass out of sample;
- operator approval is recorded;
- treasury accounting reconciles;
- incident runbooks are rehearsed;
- limitations and uncertainty are visible to the operator.

This checklist is a release gate, not a documentation suggestion.

# DEBUNKING-RESISTANCE EXTENSION

## Annex Y. Pre-warmed Sybil laundering and wallet-aging resistance

### Y.1 Threat model

A pre-warmed laundering attack creates wallets long before a launch, gives them plausible ages, and delays visible coordination until the target token is ready. Age alone is therefore not an independence signal. The attacker may route funding through CEX withdrawals, ChangeNOW, FixedFloat, Wormhole, bridges, swap aggregators, intermediate wallets, or pre-funded dormant addresses. The system must distinguish calendar age from demonstrated, diverse, economically meaningful history.

The attacker’s objective is to maximize apparent wallet diversity while minimizing observable shared ancestry. STS treats this as a graph inference problem over multiple assets, protocols, time windows, and interaction types. No single heuristic is dispositive; convergence across independent graph views is required.

### Y.2 Temporal interaction graph

For a wallet universe V and ordered event stream E, define a temporal multigraph:

`G_T = (V, E, τ, a, type, source, confidence)`

Each edge e=(u,v) carries amount a_e, timestamp τ_e, asset, protocol type, slot, transaction signature, instruction position, and confidence. Edge types include funding, swap, bridge, withdrawal, deposit, token transfer, fee payment, LP interaction, compute-budget pattern, and common program invocation.

A temporal path p is valid only when edge times are non-decreasing and the elapsed time between edges is below the declared causal horizon. Define path plausibility:

`P_path(p) = Π_(e∈p) c_e × exp(-λ_time × duration(p)) × exp(-λ_hops × (hops(p)-1))`

For wallet v and root r:

`Influence(r,v,T) = max_(p:r→v, p⊂T) P_path(p) × FlowWeight(p)`

where `FlowWeight(p)=min(1, total_flow(p)/F_ref)`. Independent corroborating paths are combined with correlation-adjusted noisy-OR, not simple addition.

### Y.3 Multi-hop graph entropy

Ordinary holder entropy measures balances, not relational independence. Define k-hop neighborhood N_k(v) and edge-type distribution q_(v,e) over observed interactions. The interaction entropy of v is:

`H_interaction(v,k) = -Σ_e q_(v,e) ln(q_(v,e))`

Normalize by the number of observed edge types:

`H_interaction_norm(v,k) = H_interaction(v,k) / ln(max(|E_types(v,k)|,2))`

Define counterparty entropy:

`H_counterparty(v) = -Σ_u q_(v,u) ln(q_(v,u))`

and temporal entropy over inter-event intervals Δt:

`H_time(v) = -Σ_b q_(v,b) ln(q_(v,b))`

A dormant sleeper tends to show high calendar age but low multi-hop interaction entropy, low counterparty entropy, low protocol diversity, and highly concentrated activity around one target launch. Define the aged-history quality score:

`AgeQuality(v)=AgeFactor(v) × (w1 H_interaction_norm + w2 H_counterparty_norm + w3 ProtocolEntropy + w4 TemporalEntropy) × ActivityCoverage`

AgeFactor is capped and cannot rescue a wallet with near-zero diverse history. `ActivityCoverage` is the fraction of historical observation periods with meaningful independent interactions, not the number of days since creation.

### Y.4 Cluster behavioral graph co-occurrence

For a candidate cluster C, define a behavior signature vector per wallet:

`b_v = [entry_delay, size_quantile, sell_delay, route_set, program_set, fee_policy, compute_budget_pattern, funding_interval, counterparties, bridge_usage, swap_protocol_usage]`

Construct a weighted similarity graph where:

`S(u,v)=Σ_j w_j × sim_j(b_u[j],b_v[j])`

For continuous values use robust kernel similarity:

`sim_j(x,y)=exp(-|x-y|/MAD_j)`

For categorical values use exact or ontology-aware similarity. The graph Laplacian is `L=D-W`, and normalized spectral cluster separation is measured using the second eigenvalue and within-cluster conductance. A suspicious cluster has high internal similarity, low external conductance, synchronized phase transitions, and shared funding ancestry.

Behavioral co-occurrence probability is estimated as:

`P_cooccur(C)=P(observed synchronized signature | independent null)^(-1)`

converted to a calibrated posterior using a null distribution generated by time-preserving permutation. The null must preserve launch popularity, wallet age distribution, and market-wide transaction bursts.

### Y.5 Swap-protocol laundering ring detection

Monitor ChangeNOW, FixedFloat, Wormhole, bridge programs, aggregators, and known swap-protocol endpoints as provenance nodes, not automatic guilt labels. For each suspected ring, compute:

- funding amount quantization;
- inter-arrival time distribution;
- source/destination fanout;
- asset conversion sequence;
- shared historical subgraph;
- common fee payer behavior;
- synchronized arrival at target wallets;
- target entry timing.

Amount quantization score:

`Q_amt = 1 - H(amount_mod_resolution) / ln(B_amt)`

where amounts are bucketed at protocol-specific precision and B_amt is the number of occupied buckets. High Q_amt indicates repeated standardized amounts, but it is only evidence when combined with timing and graph structure.

Timing synchronization score:

`Q_time = 1 - H(Δt_interarrival) / ln(B_time)`

also compare the observed interval distribution to a Poisson or renewal-process null using a likelihood ratio:

`LR_time = log P(observed intervals | coordinated renewal) - log P(observed intervals | independent null)`

Shared historical subgraph similarity between wallets u and v:

`J_subgraph(u,v)=|N_h(u)∩N_h(v)| / max(|N_h(u)∪N_h(v)|,1)`

Use weighted edges so a shared CEX node alone has low weight, while shared intermediate wallets, identical bridge paths, common swap sequences, and synchronized target entries have higher weights. A ring posterior is:

`P_ring = calibrate(b0 + b1 Q_amt + b2 Q_time + b3 J_subgraph + b4 fanout_density + b5 entry_sync + b6 path_influence)`

The detector records protocol names and exact evidence. It must not classify a wallet as malicious solely for using a regulated exchange, a bridge, or a swap protocol.

### Y.6 Wallet age and history weighting

Define wallet age in slots and wall-clock time, but cap its direct influence:

`AgeFactor(v)=1-exp(-age_days(v)/τ_age)`

Define diverse-history score:

`DiverseHistory(v)=1-exp(-N_independent_protocols(v)/τ_protocol) × (1 - concentration(history_counterparties))`

Then:

`HistoryWeight(v)=AgeFactor(v) × DiverseHistory(v) × min(1, ActivePeriods(v)/P_ref)`

Aged wallets with zero diverse DeFi activity receive low HistoryWeight. A wallet may be old and still have high sybil probability if its activity is dormant, target-specific, or graph-similar to a coordinated ring. Conversely, a young wallet with genuinely independent, diverse activity is not automatically rejected.

### Y.7 Inter-wallet holding graph similarity

At each observation time, create a holding vector across assets and protocols:

`h_v = [balance_share_asset_1, ..., balance_share_asset_n, LP_share, stablecoin_share, native_asset_share]`

Compute cosine similarity:

`Cos(u,v)=u·v/(||u||×||v||)`

and rank correlation of holding transitions:

`Rho_hold(u,v)=corr(rank(Δh_u),rank(Δh_v))`

Use a temporal sequence similarity for synchronized accumulation/distribution:

`Sim_hold(u,v)=w_cos Cos + w_rho max(0,Rho_hold) + w_seq DTW_similarity`

Discount similarity caused by broad market beta by subtracting the sector/market common factor. High residual similarity across wallets, combined with shared funding and synchronized entries/exits, increases cluster probability. A single common stablecoin holding is not sufficient evidence.

### Y.8 Sybil scoring and actions

The sybil score is a calibrated posterior using graph entropy, age quality, behavioral co-occurrence, ring evidence, holding similarity, and signer diversity:

`P_sybil = calibrate(θ0 + θ1(1-AgeQuality) + θ2 RingScore + θ3 CooccurScore + θ4 HoldingSimilarity + θ5 FundingInfluence - θ6 GenuineHistoryEvidence)`

Actions:

- `P_sybil < 0.25`: no penalty beyond normal uncertainty.
- `0.25 ≤ P_sybil < 0.55`: confidence penalty and reduced independent-holder contribution.
- `0.55 ≤ P_sybil < 0.80`: Tier 3 or paper-only; quarantine cluster.
- `P_sybil ≥ 0.80`: block if corroborated by at least two independent evidence families.

A posterior alone cannot override a hard authority or liquidity block. Every threshold is versioned and evaluated against known false-positive cohorts.

## Annex Z. Jito winner’s-curse and adverse-selection defense

### Z.1 Auction state model

Define auction state at slot k:

`A_k=[B_k, S_k, D_k, W_k, L_k, U_k, T_k]`

where B is block-space scarcity, S is searcher participation density, D is bid dispersion, W is winning probability, L is leader/validator intensity, U is opportunity decay, and T is observed tip distribution.

Searcher density is:

`S_k = unique_searchers_k / max(unique_target_bundles_k,1)`

Bid concentration is:

`HHI_tip_k = Σ_i (tip_i/Σ_j tip_j)^2`

Auction intensity can be estimated from occupied block-space:

`L_k = used_priority_units_k / max(priority_capacity_k,1)`

The system stores uncertainty for every auction variable. A low observed auction intensity can mean no competition or deliberate withdrawal/trap; it is not automatically favorable.

### Z.2 Competitive bid monitoring

Observe block-engine responses, landed bundles, rejected bundles, target slots, tip percentiles, compute-unit price, validator leader schedule, and route contention. Maintain rolling expected values:

`E[land | state]`, `E[tip | percentile,state]`, and `E[adverse_move | delay,state]`.

Detect abrupt competition drop:

`DropComp = (S_expected - S_observed) / max(S_expected,ε)`

and bid-distribution discontinuity:

`DropBid = (T_expected - T_observed) / max(T_expected,ε)`

If both exceed policy thresholds immediately before a high-signal launch, classify as `AUCTION_ANOMALY`. Abort or downgrade unless independent evidence supports a benign explanation such as scheduled leader transition, provider outage, or feed partition. The default response is not to increase the tip into a vacuum.

### Z.3 Calibrated bid sizing

Let `p_land(tip|A_k)` be the calibrated probability of landing at the target slot. Let `EV_delay` be expected EV after delay and `EV_land` expected EV after landing. Choose the smallest tip satisfying:

`p_land(tip|A_k) × EV_land + (1-p_land(tip|A_k)) × EV_delay - tip > 0`

subject to:

`tip ≤ Tip_max`

`slippage ≤ Slippage_max`

`priority_units ≤ PriorityBudget`

and no auction anomaly. If the solution set is empty, abort. Emergency exits may use a separate loss-containment objective but remain bounded.

### Z.4 Post-submission adverse selection feedback

For every submitted bundle, record submission time, target slot, tip, competition state, landing status, actual fill, immediate price movement, and forward returns at 1/5/20 slots. Define realized selection error:

`SelectionError = realized_AS - predicted_AS`

Update the execution model using delayed labels only after the observation horizon. Use exponentially weighted error:

`MAE_t = λ|SelectionError_t| + (1-λ)MAE_(t-1)`

and calibration residual:

`R_land = landed_t - p_land_predicted_t`

A CUSUM or Bayesian drift detector pauses automatic bid model updates when residuals indicate regime change. Feedback updates model parameters in offline/versioned training; it must not mutate live policy weights mid-bundle.

### Z.5 Auction-specific hard blocks

Block when target-slot state is stale, validator/leader identity is unknown where required, the block engine returns inconsistent status, landing probability confidence is below minimum, the bid would consume the EV lower bound, or competition anomaly coincides with unexplained liquidity/price behavior. Never infer safety from a cheap winning bid.

## Annex AA. Wash-trading and synthetic absorption defense

### AA.1 Intra-cluster circular flow

For cluster C and window w, define directed token flow F(u,v). Circular flow ratio is:

`CFR = Σ_cycles min(flow_edges_in_cycle) / max(total_cluster_volume,ε)`

Use bounded cycle enumeration and matrix-based approximations for large graphs. A high CFR indicates volume that returns to related entities rather than distributing to independent holders.

Round-trip retention ratio:

`RTR = volume_that_returns_to_origin_or_cluster / max(volume_sent_out,ε)`

Synthetic absorption is suspected when DAR is high but CFR, RTR, or cluster-adjusted buy share is also high.

### AA.2 Micro-amount loops

Bucket transfers by relative notional:

`r_j = trade_size_j / median_trade_size_regime`

Compute the micro-loop rate:

`MLR = count(trades with r_j < r_micro and returning to a related wallet within Δt_micro) / count(all trades)`

Compare with a time-preserving null. Deterministic repeated amounts, repeated routes, and repeated slot offsets are stronger evidence than small size alone.

### AA.3 Deterministic timing distribution

For inter-trade intervals Δt, calculate coefficient of variation, entropy, serial autocorrelation, and spectral peaks. A deterministic bot loop often has low entropy, low coefficient of variation, and significant periodicity. Define:

`TimingAnomaly = z(autocorrelation_lag1) + z(periodogram_peak) + z(1-H_time_norm)`

The score is corrected for block-time quantization and scheduler effects. It is not applied to normal market bursts without cluster corroboration.

### AA.4 True signer entropy

Wallet addresses are not sufficient. Define signer entities S_fee as unique fee payers and S_cb as unique compute-budget/instruction-pattern families after clustering. True signer entropy is:

`H_signer = -Σ_s p_s ln(p_s)`

and normalized:

`H_signer_norm = H_signer / ln(max(|S_fee|,2))`

Compute-budget signatures are treated as behavioral evidence, not cryptographic identity. Non-correlated compute-budget families receive separate weight from fee payers. High wallet entropy with low signer entropy indicates synthetic address expansion.

### AA.5 Real fee-to-volume ratio

For window w:

`FVR = (base_fees + priority_fees + swap_fees + bridge_fees + rent_changes + estimated protocol costs) / max(real_economic_volume,ε)`

Real economic volume excludes self-reversing circular flow, duplicate wash volume, and transfers that never create independent inventory risk. Compare observed FVR to a regime baseline:

`FVR_z = (FVR - median(FVR_regime)) / MAD(FVR_regime)`

Near-zero participant cost with high reported volume is synthetic-liquidity evidence. FVR must not be interpreted as proof when fee data is incomplete; incomplete fee accounting is UNKNOWN.

### AA.6 Synthetic liquidity depth profile

For each depth level a, calculate:

`D_real(a) = D_observed(a) × (1 - SyntheticShare(a))`

where SyntheticShare is estimated from CFR, signer entropy, loop rate, and fee-to-volume evidence. The production position cap uses the lower confidence bound of D_real, not observed depth. A pool can show deep nominal liquidity while having shallow economic liquidity.

### AA.7 Wash-trading action policy

If synthetic evidence is weak, downgrade confidence. If two independent families—such as circular flow plus signer entropy, or deterministic loops plus FVR—cross the validated threshold, exclude synthetic volume from BDI, DAR, depth, and EV. If adjusted depth or EV fails, block. Never allow synthetic absorption to satisfy the Phase 1 absorption gate.

## Annex AB. Raydium migration state-transition black hole defense

### AB.1 State machine

States are:

`CURVE_ACTIVE → MIGRATION_APPROACHING → MIGRATION_QUARANTINE → MIGRATION_CONFIRMED → RAYDIUM_INITIALIZING → RAYDIUM_VERIFIED → POST_MIGRATION_RESCORING → EXECUTION_ENABLED`

Failure states are `MIGRATION_UNKNOWN`, `MIGRATION_FAILED`, `POOL_INCONSISTENT`, and `EXECUTION_LOCKED`.

Transitions are driven by verified on-chain observations, never by UI estimates. Every transition records slot, signature, source quorum, and state hash.

### AB.2 90–100% quarantine

When bonding-curve progress `p_curve` satisfies:

`0.90 ≤ p_curve < 1.00`

STS enters `MIGRATION_QUARANTINE`. The execution lockout prevents new automated entries, prevents route assumptions, and prevents stop/target plans from being created against a disappearing curve state. Existing positions remain under risk monitoring and may use emergency exits if independently executable.

The lockout remains until all conditions pass:

- migration transaction is confirmed with required finality;
- Raydium pool account exists and matches the expected mint/quote pair;
- reserves and decimals reconcile;
- LP initialization is complete;
- liquidity ownership/lock state is verified from on-chain evidence;
- route simulation succeeds;
- post-migration depth is above minimum;
- no authority or supply contradiction exists;
- a fresh score is computed after the migration slot.

No timer alone unlocks execution.

### AB.3 Dual-AMM route compilation

Before quarantine, precompile two separately simulated transaction envelope families:

`Route_curve = {program, accounts, instruction_data, expected_state_hash, slippage_bound, expiry}`

`Route_raydium = {program, accounts, instruction_data, expected_state_hash, slippage_bound, expiry}`

The envelopes are not interchangeable. Each contains route-specific account ownership, pool state hash, reserve assumptions, compute budget, fee schedule, and emergency exit path. A state-machine transition invalidates the old route automatically.

After migration confirmation, the executor selects Raydium only when the expected state hash and account set match. If the state hash differs, the route is stale and execution remains locked. The bonding-curve route is retained for reconciliation only and is never submitted after confirmed migration unless a separately verified rollback state exists.

### AB.4 Post-migration re-scoring

Recompute authority, supply, LP lock, holder distribution, signer entropy, synthetic depth, price impact, DAR relevance, volatility, and EV. Remove no prior risk findings. A token that was safe on the curve can fail on Raydium. The minimum post-migration observation window is policy-configured by slots and events; it must include at least one valid depth sample and one independent route simulation.

### AB.5 Migration black-hole detection

Detect black-hole states when curve progress reaches the quarantine range but neither migration confirmation nor valid curve trading is observed, when pool initialization events disagree, when liquidity lock proof is missing, or when account ownership changes unexpectedly. These states remain locked and generate high-severity telemetry. The system does not retry against guessed accounts.

## Annex AC. V8 garbage-collection and ingestion-latency defense

### AC.1 Zero-allocation hot path contract

The critical ingestion path uses preallocated static ArrayBuffers, TypedArrays, fixed-size ring slots, interned program/source IDs, and object pools. It does not call JSON.parse, object spread, Array.prototype map/filter/reduce, dynamic string concatenation, or unbounded collection growth on the critical event path. Payload bytes are copied once into a bounded buffer or referenced through a lifetime-managed slab.

Zero-allocation means zero avoidable V8 heap allocations during normal operation, not that the runtime can mathematically guarantee no allocation under every library or exception path. Allocation counters and heap sampling verify the claim. Any fallback allocation path emits a performance-degradation event.

### AC.2 Buffer and pool design

Each ingress slot contains:

- fixed metadata TypedArray fields;
- offset and length into a payload slab;
- source and event-type integer IDs;
- sequence number;
- checksum;
- durability class;
- publication flag.

Payload slabs are pooled by size class. Oversized payloads enter a bounded slow lane and cannot block normal events. Released slabs return to the pool only after all consumers acknowledge completion. Reference counts are atomic or managed by worker ownership; use-after-release is a fatal test failure.

### AC.3 Parser offload

Heavy decoding, protobuf/gRPC parsing, signature extraction, and graph traversal run in dedicated worker threads or native C++/Rust Node-API addons. SharedArrayBuffer carries fixed-layout records and Atomics coordinates ownership. Workers never mutate published records; they write to separate result slots and publish a sequence when complete.

Native modules must expose versioned ABI boundaries, bounds-check all offsets, reject malformed lengths, and include fuzz tests. A native crash is a process-isolation incident, not a reason to bypass parsing. The supervisor restarts the worker, marks dependent features stale, and applies the confidence/degradation policy.

### AC.4 V8 and Node.js tuning

Tuning is benchmark-driven and environment-specific. Candidate flags may include:

`--max-old-space-size=<bounded value>`

`--max-semi-space-size=<bounded value>`

`--trace-gc` in profiling environments only

`--heapsnapshot-near-heap-limit=<bounded count>` for controlled diagnostics

Do not enable arbitrary flags in production without a benchmark, rollback plan, and startup audit record. Increasing heap size is not a latency solution if it increases pause duration. Prefer reducing allocations, limiting object lifetime, isolating workers, and keeping hot structures off the managed heap.

### AC.5 Latency budget

Required measurements include event receipt, slot decode, normalization, publication, worker handoff, feature availability, gate evaluation, signing, submission, and UI propagation. Record p50, p95, p99, and p99.9, plus max, timeout, and GC-pause overlap.

A default latency budget is:

- receipt-to-publication p99.9 ≤ 2 ms;
- publication-to-normalized-record p99.9 ≤ 10 ms;
- critical feature refresh p99.9 ≤ 100 ms;
- gate evaluation p99.9 ≤ 25 ms;
- signing/preflight p99.9 ≤ 100 ms;
- submission handoff p99.9 ≤ 200 ms.

These are targets subject to benchmark validation. Exceedance causes tier degradation or execution lock depending on whether the affected feature is safety-critical. GC pauses are correlated with event IDs and never hidden inside average latency.

### AC.6 GC observability and recovery

Monitor `performance.eventLoopUtilization`, event-loop delay histogram, heap used, heap total, external memory, ArrayBuffer memory, allocation rate, minor GC count/duration, major GC count/duration, worker queue depth, and ring occupancy. Alert on p99.9 delay, not only mean delay.

If p99.9 exceeds threshold for two windows, the system enters `INGESTION_DEGRADED`, stops new real-capital entries, keeps risk exits active through their isolated path, and attempts worker/cache shedding. If critical exit telemetry cannot be processed within policy, enter emergency execution/reconciliation mode. Recovery requires sustained latency, memory, and reconciliation health.

### AC.7 GC and allocation tests

- Run one million representative events and assert bounded heap growth.
- Run a burst at 10x rate and measure p99.9 delay.
- Force minor and major GC under synthetic load.
- Inject oversized malformed payloads.
- Kill and restart parsing workers.
- Verify SharedArrayBuffer sequence integrity.
- Verify no object-pool double release.
- Verify no stale slab is read after release.
- Compare native and reference parser outputs byte-for-byte.
- Confirm allocation-free claims using heap statistics and allocation profiling.
- Confirm the slow lane cannot starve critical events.

## Annex AD. Integrated debunking-failure decision table

Failure: aged wallets with little diverse history.

Evidence: high calendar age, low AgeQuality, low protocol entropy, target-only activity, high graph similarity.

Response: discount age, raise P_sybil, reduce independent-holder count, downgrade or block.

Failure: coordinated swap-protocol laundering.

Evidence: quantized amounts, synchronized intervals, shared historical subgraphs, common bridge/swap paths.

Response: graph posterior, cluster quarantine, remove correlated volume, require independent corroboration.

Failure: Jito winner’s curse.

Evidence: high searcher density, concentrated tips, aggressive leader block-space, adverse post-fill move.

Response: require EV after tip and adverse selection, calibrate landing probability, cap bid, abort when no positive solution.

Failure: competition vacuum trap.

Evidence: abrupt collapse in expected searcher participation and bid distribution before high-signal launch.

Response: AUCTION_ANOMALY, no automatic tip escalation, downgrade or abort pending explanation.

Failure: synthetic absorption.

Evidence: high nominal DAR, circular flow, micro-amount loops, low signer entropy, near-zero true FVR.

Response: subtract synthetic depth and volume, fail absorption gate, block or paper-only.

Failure: migration black hole.

Evidence: curve progress 90–100%, missing or contradictory Raydium initialization/lock evidence.

Response: mandatory quarantine, dual-route state machine, no guessed route, post-migration rescore.

Failure: V8 GC stall.

Evidence: p99.9 event-loop delay, heap allocation spikes, worker queue growth, GC overlap.

Response: stop new entries, preserve exits, shed slow lane, isolate workers, reconcile, and recover only after sustained health.

## Annex AE. Final completeness statement

These extensions are part of the core STS contract, not optional analytics. Sybil resistance is relational and temporal, not merely wallet-age based. MEV defense prices both landing and selection quality, not merely inclusion probability. Absorption is valid only when economically independent participants absorb supply. Migration is a state transition requiring on-chain verification, not a market-cap threshold. Low latency is achieved through allocation discipline, worker isolation, and p99.9 measurement, not through an unsupported claim of zero milliseconds.

Every debunking defense has a feature definition, mathematical representation, evidence provenance, calibrated threshold, degradation action, hard-block condition where necessary, test case, and operator-visible reason code. The system remains non-custodial, auditable, and conservative under uncertainty.

# RECURSIVE ADVERSARIAL REVIEW AND HARDENING ANNEX

## AF. Scope and limits of the adversarial review

This annex subjects the preceding specification to an adversarial review by assuming that every observable feature can be delayed, selectively revealed, strategically manipulated, or made expensive to trade against. It also assumes that paper results can be biased without an obvious coding error, that a valid private bundle can still lose money, that an apparently independent wallet set can be coordinated, and that infrastructure failures can occur exactly during the highest-value opportunity.

The objective is not to claim an ironclad profit guarantee. No specification can eliminate market risk, protocol risk, validator behavior, or unknown unknowns. The objective is to make each known failure mode explicit, price it, test it, fail closed when necessary, and prevent optimistic assumptions from entering EV or sizing.

The adversarial review uses five questions for every feature:

1. Can the observation be forged, delayed, censored, or selectively omitted?
2. Can the measurement be correct while the economic interpretation is wrong?
3. Can an adversary condition behavior on our visible policy?
4. Can execution cost or capital drag erase the apparent edge?
5. Can the validation process reward a strategy that will fail live?

A feature is production-eligible only when its observation path, manipulation surface, confidence, economic consequence, and recovery behavior are documented.

## AG. Hidden economic costs and true net expectancy

### AG.1 Complete cost ledger

The earlier EV formulation must not collapse all friction into one historical average. For position i, define:

`EV_i = E[GrossPayoff_i - C_entry_i - C_exit_i - C_priority_i - C_tip_i - C_route_i - C_token_i - C_impact_i - C_slippage_i - C_adverse_i - C_failure_i - C_capital_i - C_tax_i]`

Entry and exit costs are random variables conditioned on route, slot, pool, size, state, and provider. Capital cost is not only an annualized opportunity cost. It includes capital immobilized while a position cannot exit, expected cost of concurrent exposure, reserve requirement, and the probability that a better opportunity arrives while capital is locked.

`C_capital = r_f × duration × notional + E[opportunity_loss | locked duration] + reserve_drag`

Tax and accounting costs are jurisdiction-specific and may be unknown at execution time. When unknown, the decision stores a tax-excluded EV and applies a governance reserve rather than pretending the cost is zero.

### AG.2 Nonlinear fee and impact composition

Fees must be calculated per instruction and per route, including transfer fees, pool fees, aggregator fees, rent changes, account creation, compute-unit price, base fee, Jito tip, failed transaction fees, and cleanup costs. A failed transaction is a realized cost even when no position is created.

For multi-hop route h with hops j:

`P_out = P_in × Π_j (1 - fee_j) × (1 - impact_j(q_j)) × (1 - token_fee_j)`

The product must be simulated with exact pool states, not approximated by summing hop percentages. Correlated impact is added when hops share a pool, token, route, or adversarial searcher set.

### AG.3 Break-even and margin-of-safety gate

Let `EV_LCB` be the lower confidence bound of net EV and `σ_model` be model uncertainty. Define required margin:

`M_required = z_alpha × σ_model + κ_tail × CVaR_α + Cost_uncertainty`

Accept only when:

`EV_LCB > M_required`

A nominally positive trade with a small edge relative to uncertainty is blocked or paper-only. The margin is larger for low-liquidity pools, novel routes, and high validator/searcher competition.

## AH. Oracle, indexer, clock, and state-consistency attacks

### AH.1 Stale-state exploitation

An adversary can trade between observation and execution, exploit indexer lag, or present inconsistent account data to different providers. Every critical feature therefore carries a maximum age in slots and wall-clock time. The stricter bound applies:

`Fresh = (current_slot - observed_slot ≤ S_max) AND (now - observed_at ≤ T_max) AND (source_health ≥ h_min)`

A fresh timestamp with a stale slot, or a fresh slot with delayed delivery, is not fresh. Feature ages are evaluated immediately before simulation and again immediately before signing.

### AH.2 Quorum disagreement

For critical numeric state x from providers p, calculate robust median and dispersion:

`x_med = median(x_p)`

`Dispersion = MAD(x_p) / max(|x_med|, ε)`

If Dispersion exceeds the field-specific threshold, mark the state contradictory. For discrete state, require quorum; if no quorum exists, block dependent execution. Never average mutually inconsistent authority or reserve states.

### AH.3 Clock and slot attacks

Use a monotonic local clock for durations and a trusted wall clock for external timestamps. Record offset estimates and maximum observed skew. A system clock jump invalidates latency comparisons and freshness calculations until clocks stabilize. Slot time is not assumed constant; use observed slot cadence and confidence intervals.

### AH.4 Reorg and rollback handling

A decision based on a slot that later changes must be marked superseded, not rewritten. The replay engine replays the canonicalized chain view and compares the original decision with the information available at the original time. Any trade executed during a fork or ambiguous finality window receives a separate outcome label and contributes to infrastructure-risk calibration.

## AI. Pool manipulation and reserve-state attacks

### AI.1 Reserve spoofing and just-in-time liquidity

Headline liquidity can be inflated immediately before entry and withdrawn immediately after. Measure depth over time, liquidity persistence, LP ownership concentration, reserve age, and withdrawal rights. Define persistence:

`LP_persistence = time_weighted_min(depth_t / depth_reference)`

Use the lower confidence bound over a rolling window rather than the latest depth. A sudden depth increase with no corresponding independent flow is a liquidity anomaly and cannot be used to increase size.

### AI.2 Price manipulation and oracle contamination

Do not use the candidate pool alone as a price oracle. Compare pool price against independent venues and robust cross-source estimates:

`P_ref = weighted_median(P_pool_1,...,P_pool_n)`

Weights depend on executable depth, freshness, and source independence. A candidate pool whose price deviation exceeds:

`|ln(P_pool/P_ref)| > z_price × σ_cross_source`

is marked manipulated or unresolved. The candidate pool may still be traded only if the route model explicitly prices convergence risk and passes the margin-of-safety gate.

### AI.3 Sandwiching and route contamination

Private submission reduces public exposure but does not remove risks from a compromised route, shared searcher, validator observation, or multi-hop path. For each hop, check whether another instruction in the same slot changes reserves, transfers tokens, changes compute budget, or touches the same accounts. Reject bundles with unmodeled state-changing instructions.

A route is atomic only if the transaction’s protocol semantics guarantee the intended invariant. Bundle-level atomicity does not guarantee economic atomicity when a successful transaction creates an adverse but valid fill. Compare pre- and post-state for every pool and account; flag unexpected reserve changes even when the transaction succeeds.

### AI.4 LP withdrawal and authority races

Immediately before signing, verify LP token ownership, lock program, unlock epoch, pool authority, and reserve accounts. At confirmation, reconcile expected and actual LP state. If any authority, lock, or reserve account changes between simulation and landing, classify the trade as state-raced and block future entries for the token until rescored.

## AJ. Validator, leader, and block-engine collusion

### AJ.1 Threat model

A validator or block leader may observe private order flow, selectively include bundles, delay bundles, favor a searcher, or coordinate with liquidity providers. Jito privacy is a risk reducer, not a trust proof. STS therefore measures landing and adverse-selection outcomes by leader, block engine, route, and slot regime.

### AJ.2 Conditional landing and selection model

Estimate:

`P_land = P(land | leader, engine, tip, compute, slot_state, competition)`

`P_adverse = P(adverse selection | leader, engine, route, tip, latency, state)`

The accepted value is:

`EV_execution = P_land × EV_landed - (1-P_land) × DelayCost - P_adverse × AdverseLoss`

A leader-specific anomaly is triggered when residual adverse selection or landing residual exceeds its calibrated control limit. New submissions to that path are downgraded or halted while independent evidence is collected.

### AJ.3 Validator concentration penalty

Let q_v be the share of relevant opportunity slots controlled by validator/leader group v. Effective leader concentration is:

`HHI_leader = Σ_v q_v^2`

A route or signal whose expected outcome depends on a highly concentrated leader set receives an execution-risk penalty. The system must not infer collusion from concentration alone; the penalty prices dependence, and only abnormal residual behavior triggers investigation.

## AK. Race conditions and transactional correctness

### AK.1 Signal-to-sign race

A signal may become invalid during feature computation. Use a state version and compare-and-swap contract:

`accept iff state_version_at_sign = state_version_at_simulation = current_state_version`

Otherwise invalidate the envelope and recompute. A wall-clock expiry alone is insufficient.

### AK.2 Duplicate and split-brain orders

Every order has an idempotency key derived from decision ID, position ID, side, route version, and attempt policy. The durable order ledger is the authority for submission status. Multiple executor workers must acquire a lease with fencing token before submission. A stale worker cannot submit after lease loss.

### AK.3 Partial fills and accounting races

Position state is a state machine, not a balance guess:

`PLANNED → SUBMITTING → PARTIALLY_FILLED → FILLED → EXIT_PENDING → CLOSED`

Every transition requires a receipt or explicit reconciliation evidence. Mark-to-market P/L uses actual average fill and remaining quantity. Never treat requested quantity as filled quantity.

### AK.4 Concurrent risk updates

Risk budgets are reserved transactionally before submission. A reservation contains amount, expiry, position correlation, and fencing token. On fill, reservation converts to exposure; on failure, it releases only after reconciliation. This prevents concurrent candidates from each believing they have the same capital.

## AL. Statistical overfitting and paper/live divergence

### AL.1 Multiple-testing correction

A research process that tries hundreds of filters will produce attractive backtests by chance. Record every tested hypothesis, parameter set, rejected result, and selection date. Use family-wise or false-discovery controls where appropriate, and reserve a final untouched test set.

A strategy is not promoted because one backtest wins. It must survive nested walk-forward validation, parameter perturbation, alternate data vendors, and negative-control tests.

### AL.2 Purged and embargoed validation

When labels overlap in time, purge training observations whose outcomes overlap the validation interval. Apply an embargo after the training window to prevent information leakage from adjacent launches, shared deployers, or correlated market events.

### AL.3 Live slippage calibration

Paper fills use a distribution conditioned on liquidity bucket, order size, route, latency, leader, competition, and state. A zero-slippage fill is invalid. Maintain a live-versus-paper residual:

`R_slip = Slippage_live - Slippage_paper_expected`

`R_impact = Impact_live - Impact_paper_expected`

Trigger a promotion pause when residuals breach control limits for two consecutive cohorts. Retraining uses only data available at the retraining date, and model versions are frozen for each evaluation period.

### AL.4 Selection and survivorship bias

Include failed launches, disappeared pools, no-exit positions, rejected candidates, and data outages in denominators. Do not condition on tokens that survived long enough to be labeled. Deployer and wallet clusters are grouped in statistical inference. Report results both with and without excluded Phase 0 launches, never mixing them into headline performance.

### AL.5 Economic significance

A statistically significant edge can be economically negative after tips, failed transactions, capital drag, and operational labor. Every report must include net dollars per unit risk, turnover, capital utilization, average lock duration, tail loss, and capacity curve:

`Capacity(Q) = EV(Q) / Q`

A strategy is capacity-constrained when increasing size decreases net EV or increases CVaR faster than the risk budget allows.

## AM. Capital drag, opportunity cost, and portfolio interactions

### AM.1 Lock-duration model

For position i, let T_i be random time to executable closure. Capital drag is:

`Drag_i = Notional_i × (r_f + λ_opportunity) × E[T_i] + ν × Var(T_i)`

where λ_opportunity is estimated from the arrival and quality distribution of competing signals and ν prices uncertainty in lock duration. A pool with attractive point EV but high no-exit duration may fail the capital-efficiency gate.

### AM.2 Correlated positions

Wallet, deployer, quote asset, bridge, protocol, and validator exposure create correlation even across different mints. Build a factor exposure matrix B and covariance estimate Σ:

`PortfolioRisk = sqrt(w^T BΣB^T w)`

Use conservative shrinkage and stress scenarios when sample support is weak. Concurrent positions share daily loss and emergency-exit capacity. The system may block a new signal despite positive standalone EV if portfolio CVaR breaches limits.

### AM.3 Reserve and liquidity runway

Maintain a minimum operational reserve for fees, emergency tips, rent, RPC, and failed transactions. Reserve capital cannot be counted as deployable trade size. If projected emergency costs exceed reserve runway, reduce all tiers or enter observation-only mode.

## AN. Adversarial adaptive behavior and policy randomization

An adversary may learn fixed thresholds, timing windows, tip caps, or route choices. STS must not randomize safety invariants, but can randomize non-safety operational details within pre-approved bounds: observation jitter, equivalent route selection, bundle target slot when EV-preserving, and diagnostic query ordering. Do not expose private policy state through UI or public logs.

Detect policy-conditioned behavior by comparing outcomes across policy versions, routes, and time windows. If a cluster’s activity changes immediately after signal generation or submission, treat it as potential informed adversary behavior and increase adverse-selection estimates. Never retaliate by widening slippage or tip caps.

## AO. Oracle-free and self-referential feature defenses

Features derived from STS’s own prior signals may create feedback loops. Separate exogenous observations from endogenous actions. Social activity after a signal, wallet inflows caused by an alert, or liquidity added in response to visible demand cannot be treated as independent confirmation.

Every feature has lineage labels:

- EXOGENOUS: observed independently of STS actions.
- ENDOGENOUS: potentially caused by STS or its visibility.
- MIXED: causal direction uncertain.

Only EXOGENOUS or properly instrumented MIXED data contributes to causal validation. ENDOGENOUS activity can describe execution impact but cannot validate the original thesis.

## AP. Incident taxonomy and post-mortem requirements

Incidents are classified as data integrity, stale state, route race, execution loss, MEV/adverse selection, sybil misclassification, synthetic volume, migration state error, GC/latency, persistence mismatch, key/security, or governance failure.

Each post-mortem records timeline, last valid state, first detectable signal, missed control, economic loss, counterfactual action, data availability, model version, operator actions, and permanent remediation. The incident is replayed from immutable evidence. Fixes require a regression test and a promotion gate; narrative explanations alone do not close incidents.

## AQ. Recursive review loop

The specification is re-reviewed in cycles:

1. Enumerate assumptions and convert each into a falsifiable statement.
2. Generate adversarial counterexamples using historical and synthetic replay.
3. Measure false positives, false negatives, latency, cost, and capacity impact.
4. Add the smallest defensible control that addresses the failure.
5. Test that the control does not create a larger blind spot or zero-trade state.
6. Freeze the rule, version the model, and record the evidence cohort.
7. Monitor live residuals and reopen the rule when drift is detected.

A refinement is incomplete if it only adds a rejection rule. It must specify observation, calculation, threshold, uncertainty, action, recovery, test, and economic tradeoff.

## AR. Final adversarial conclusions

The most dangerous STS failures are not only obvious rugs. They are profitable-looking trades whose edge disappears after hidden costs; “independent” holders connected through old but dormant graph structure; deep pools whose liquidity is synthetic or transient; private bundles that land precisely when adverse selection is highest; migrations where the route changes between simulation and execution; and latency systems that look fast on averages while p99.9 stalls occur during the decision window.

The hardened doctrine is therefore:

- Treat age as weak evidence until diverse history is demonstrated.
- Treat graph structure, behavior, signer identity, and holding transitions as separate correlated evidence families.
- Price the probability and quality of execution, not just inclusion.
- Abort when auction behavior is inexplicably favorable immediately before a high-signal event.
- Remove synthetic volume and depth before calculating DAR, BDI, position size, or EV.
- Lock the entire 90–100% migration interval and require verified Raydium state plus fresh rescoring.
- Use allocation-free hot-path structures, but verify the claim with allocation and p99.9 tests.
- Treat provider disagreement, state races, and reconciliation ambiguity as first-class risk.
- Include capital drag, portfolio correlation, failed attempts, tax/operational costs, and capacity in net expectancy.
- Preserve an untouched validation path and treat live residuals as evidence against the model.

No control makes Solana markets deterministic. The production standard is stronger: every material uncertainty is visible, every cost is priced, every state transition is verified, every retry is idempotent, and every attractive result must survive adversarial replay before it is trusted.


# AS. IMPLEMENTATION HARDENING ANNEX — EXECUTABLE ENGINEERING DETAIL

## AS.1 Canonical data and event identity

Use UUIDv7 for event_id and correlation_id. Define canonical_bytes(e) as UTF-8 JSON with recursively sorted object keys, no insignificant whitespace, integer quantities encoded as decimal strings, and timestamps normalized to RFC3339 UTC. Let h_0 = SHA256("STS-GENESIS-v1"); h_n = SHA256(h_(n-1) || canonical_bytes(e_n)). Persist prev_hash and hash for every immutable event. Duplicate identity is (source, signature, instruction_index, event_type); conflicting payloads create a contradiction event rather than overwriting.

## AS.2 Tauri/Rust/Native WebKit boundary

The desktop shell is Tauri. Rust owns ingestion, SQLite, key isolation, signing, state machines, IPC authorization, and audit writes. Native WebKit renders views only and cannot access keys, SQLite files, RPC credentials, or arbitrary filesystem paths. Commands are allowlisted and typed; every command carries correlation_id, schema_version, deadline, and operator session. Rust emits immutable event envelopes over a bounded subscription channel. The UI may request projections and commands, never mutate raw events.

Recommended modules: domain (pure types/invariants), ingest, normalize, forensics, risk, execution, persistence, replay, ipc, and audit. Keep calculations deterministic and side-effect free where possible. Use Rust ownership to prevent sharing signing material with WebKit-facing DTOs; redact secrets at serialization boundaries.

SQLite initialization must execute: PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;. One writer owns the connection. Readers use immutable projection queries. Schema migrations are numbered, transactional, checksum-verified, and refused when an unknown future version is detected. WAL checkpointing is scheduled outside the ingress critical path; disk-full or checkpoint failure enters DEGRADED_PERSISTENCE and blocks new execution.

## AS.3 Deterministic sizing and gate pseudocode

~~~text
function evaluate(c, now):
    require c.schema_version == SUPPORTED_SCHEMA
    facts = validate_freshness_and_provenance(c.facts, now)
    hard = run_hard_invariants(facts, c.policy)
    if hard.failed: return Decision(BLOCKED, reasons=hard.reasons)
    confidence = calibrated_vector(facts, c.model)
    stress = simulate_paths(c, gaps=[0.30,0.50], slippage=[.10,.15,.20,.25])
    ev_lcb = lower_confidence_bound(stress.ev, stress.interval)
    if ev_lcb <= 0: return Decision(OBSERVE_ONLY, reason="negative stressed EV LCB")
    tier = tier_for(confidence.scalar, facts.unknown_count, facts.rpc_health)
    base = min(c.risk_budget / max(stress.stressed_loss, epsilon),
               0.015 * facts.executable_pool_liquidity,
               c.max_notional, c.operator_cap)
    size = base * {T1: 1.0, T2: .5, T3: .1}[tier]
    if tier == T3 and not c.operator_confirmed: return Decision(PAPER_ONLY)
    return sign_expiring_envelope(c, size, tier, ev_lcb, hard, stress)
~~~

## AS.4 State machines and recovery

Candidate states are OBSERVED → NORMALIZED → FORENSIC_PENDING → SCORED → GATED → APPROVED or BLOCKED. Approved orders transition to SIMULATING → SIGNING → SUBMITTED → LANDED → RECONCILING → FILLED/PARTIAL/CANCELLED/FAILED. Any contradictory state, expired envelope, stale critical fact, failed simulation, ambiguous confirmation, or persistence integrity error transitions to QUARANTINED; QUARANTINED can recover only through fresh evidence and a new correlation ID. KILL_SWITCHED is an absorbing state for new submissions until authenticated reset and health checks succeed.

~~~text
on timeout: retry same idempotency_key with bounded backoff
on duplicate receipt: reconcile, never submit a new order
on provider disagreement: freeze decision, query quorum, record contradiction
on partial fill: cancel remainder, recompute exposure, apply immutable stop
on unknown confirmation: halt new orders, poll independent providers, escalate operator
on disk/WAL failure: stop execution, preserve memory queue, alert, recover then replay
~~~

## AS.5 Split-pane 0x100x GUI/CLI and pipelines

The command centre uses a fixed semantic split: left pane is the event/signal ledger; center is the selected forensic and mathematical inspector; right is execution, risk, and operator controls. Every color has a text state label and accessibility contrast; UNKNOWN, STALE, BLOCKED, and ZERO are distinct values. CLI commands mirror typed IPC actions: sts observe, sts inspect <correlation_id>, sts replay <range>, sts approve <envelope_id>, sts kill, and sts reconcile <correlation_id>.

Pipeline: source adapters → raw immutable envelope → dedupe/fork detector → normalized event → feature store → forensic graph → calibrated confidence → stress simulator → policy gate → signed envelope → private executor → receipt reconciler → projections/UI/CLI. Backpressure is measured at every edge; each message carries event_id, correlation_id, source watermark, schema version, and provenance. UI projections are disposable and rebuildable from the event log.

## AS.6 Failure-mode acceptance matrix

For every failure, tests must assert detection, safe action, durable evidence, operator-visible explanation, bounded recovery, and replay equivalence. Critical-event loss, key exposure, public-mempool fallback, stale-price execution, unbounded tip escalation, and silent contradiction are release blockers. A successful recovery is not sufficient unless pre-failure and post-replay projections match byte-for-byte for canonical data.

## 16. Bootstrap Economics & Capital Preservation Framework (Max €200 Budget, Zero-Cost Infra Default)

### 16.1 Hard economic invariants

The fixed infrastructure budget is exactly €0. Development, replay backtesting, shadow testing, observability, and forensic computation must run on the existing client machine and free service tiers by default. The €200 total capital allocation is a protected capital buffer, not an infrastructure or experimentation budget. It remains untouched throughout development, replay backtesting, paper trading, and shadow testing.

No subscription, paid RPC tier, hosted database, paid data feed, or infrastructure upgrade may be activated from the €200 buffer. A budget violation is a release-blocking governance failure:

`fixed_infrastructure_spend = 0 EUR`

`protected_capital_balance = 200 EUR` during development ∧ replay ∧ shadow testing.

### 16.2 Zero-cost client-side data pooling

> **CORRECTED 2026-08-27 — history is free, and this rule nearly cost us the most
> important finding in the project.** The prohibition below on ~~"redundant
> historical backfills"~~, on the grounds that they burn credits, is wrong about the
> facts. **The evidence was never out of reach.** The free Helius endpoint serves
> full blocks — transactions and logs — back to at least 6 August 2024 at zero cost.
> Separate research the same day found the same thing by a different route: **the
> official public Solana RPC, `api.mainnet-beta.solana.com` — no account, no API
> key, no card — is a free archive node serving the ledger from genesis**, backed by
> the Solana Foundation's Old Faithful archive. Two routes, found independently,
> both free.
>
> Nobody had ever swept either. When somebody finally did, one afternoon's sweep of
> seven hour-matched windows across two years produced the finding that governs this
> whole document: **this trade has never been profitable, in any market we can
> observe.** That result cost nothing and was available at any point in this
> project's life. The €0 constraint was never what stood between us and it — the
> assumption that history was expensive was.
>
> Keep the live-stream discipline below: targeted subscriptions, no unfiltered log
> streams, no speculative polling, quota counters, shed before the limit. Those
> rules are about protecting a **live** free tier under load and they are sound. **A
> one-off historical sweep is not that, is not "redundant", and should be the first
> response to any claim about how this market used to behave.**
> `docs/VERDICT-2026-08-27.md`; `docs/TRAINING_DATA_FREE.md` and
> `docs/TRAINING_DATA_SOURCES.md` for the routes and their limits.

STS may pool free-tier gRPC/Geyser streams from Helius, QuickNode, and Triton on the client, subject to each provider's current terms, quotas, rate limits, and attribution requirements. The client subscribes only to explicitly targeted bonding-curve accounts, pools, mints, and required program/account filters. Broad-chain subscriptions, unfiltered log streams, redundant historical backfills, and speculative polling are prohibited because they burn credits without improving the decision boundary.

A source adapter must enforce an allowlist before transmission, maintain per-provider quota counters, deduplicate events by stable identity, and stop or shed the source before a free-tier limit is exceeded. Pooling improves availability and coverage; it does not imply identical latency, completeness, or correctness. Critical facts require provider consistency/quorum or remain degraded/blocked for entries. Exits remain live under the liveness invariant.

### 16.3 Capital-preserving promotion ladder

The deployment ladder is:

`development -> deterministic replay -> paper trading -> shadow mode -> operator-approved micro-live -> measured promotion`

Promotion requires out-of-sample positive stressed expectancy, calibrated error within policy, replay equivalence, provider-failover tests, partial-fill reconciliation, stop-loss/kill-switch tests, and a documented rollback. Passing a model gate never authorizes ordinary-size trading automatically.

At first live deployment, risk is limited to micro-positions such as `0.05 SOL` per position, further capped by the smallest of the account risk budget, executable-liquidity cap, loss cap, and operator limit. The 0.05 SOL figure is a maximum example, not a guaranteed or universal safe amount; the protected capital buffer remains the governing constraint. No averaging down and no correlated basket may circumvent the cap.

### 16.4 50/50 flywheel funding rule

Only realized trading profits, after losses, fees, taxes, obligations, and required reserves, may fund future paid infrastructure. The existing treasury rule applies:

`realized_surplus = realized_gains - realized_losses - fees - taxes - obligations`

`system_reinvestment_reserve = max(0, 0.50 × realized_surplus)`

`paid_infrastructure_spend <= system_reinvestment_reserve`

The other 50% goes to segregated savings/risk-free reserves. A paid upgrade requires an operator-approved bottleneck analysis, expected latency/coverage improvement, explicit recurring-cost cap, and measured rollback path. A negative or unverified realized surplus means paid infrastructure remains prohibited. Paid infrastructure can never be justified by projected profits, unrealized P/L, or a desire for convenience.

### 16.5 Feasibility audit: complete answers

Pros: the framework preserves runway, makes the system testable on ordinary client hardware, prevents infrastructure costs from disguising weak expectancy, creates provider redundancy, and forces targeted data collection. It also makes every paid upgrade evidence-based because the upgrade must be financed by realized system output.

Cons: free tiers impose quotas, terms can change, streams may be rate-limited or delayed, providers can disagree, client uptime and thermal limits are weaker than dedicated infrastructure, and narrow filters can miss an unanticipated relevant account. Client-side pooling adds adapter, deduplication, and monitoring complexity. These are accepted risks only when measured, bounded, surfaced, and reflected in operating mode; they are not silently treated as equivalent to paid low-latency infrastructure.

Operating environments: development and replay run offline from immutable event fixtures; paper and shadow modes use pooled free streams without signing or broadcasting; micro-live runs locally with keys isolated, private execution only, strict quotas, and an authenticated kill switch. A MacBook may run ingestion, bounded fast-path features, UI/CLI, and persistence workers, while forensic graph jobs are throttled background work. If CPU, memory, battery, disk, provider, or quota governors breach limits, entries degrade or stop and exits remain available.

Execution-drag mitigation: precompute routes and emergency bundles, keep bounded snapshots warm, use fixed-size queues and slab allocation, filter at the source, maintain provider health scores, select the freshest quorum-consistent snapshot, and use idempotent private bundles with expiry. The fast path must not wait for gRPC fan-out, SQLite WAL, JSON serialization, recursive graphs, or forensic clustering. Measure end-to-end signal-to-submit latency and separate data delay, computation delay, signing delay, and landing delay. If expected execution drag makes stressed EV non-positive, do not enter.

Latency versus forensic balancing: the fast path makes only bounded decisions from timestamped snapshots and applies freshness budgets; the forensic path improves later attribution and model quality without blocking risk-reducing actions. An entry may be restricted or rejected when its forensic watermark is too old, while an exit uses the independent safety path and remains live. The governing rule is not fastest possible entry; it is maximum risk-adjusted expectancy subject to capital preservation:

`permit_entry iff EV_LCB(snapshot, drag, fees, slippage) > 0 ∧ hard_invariants_pass ∧ mode in {NORMAL, RESTRICTED_ENTRY}`

`permit_exit iff executable_route_exists OR emergency_escalation_allowed`

This framework therefore remains feasible as a low-cost, local-first system, but feasibility is not profitability: profitability must be demonstrated out-of-sample and after real execution drag, failures, costs, and partial fills.
