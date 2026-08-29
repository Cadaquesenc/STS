# What to keep, what to bin

**Written 2026-08-27, after the "no go" verdict. Nothing has been deleted.
Nothing in this file has been acted on beyond adding three header comments.
Deleting anything is Ethan's call and should be one reviewed commit.**

**Recounted 2026-08-27 against the tree. The three modules that gained a header
comment gained lines with it: `attribution.rs` 3,990, `mev_sim.rs` 3,405,
`loadgen.rs` 1,390. Every other count below was recomputed and holds.**

**A companion dossier now exists.** [`SALVAGE-DOSSIER.md`](SALVAGE-DOSSIER.md) has the
complete file-by-file inventory — lines, tests, and who imports whom — with every
number recomputed from the tree rather than carried from here, so a proposed split can
be checked for whether it would compile before anyone acts on it. It disagrees with
this document in three places and says so.

Everything at the moment this was written is recoverable by name:

```
git checkout pre-salvage-2026-08-27
```

That is an annotated tag at `2926046`, pushed to `origin`. It holds the complete
tree — all ~106,000 lines, the Tauri shell, `ui/`, every test. Whatever gets
deleted later, this brings it back.

*(`main` moved on by one commit while this was being written — `80d2bab`, a
`tools/capture` fix from another hand. No salvage action happened in between, so
the tag still marks the complete pre-salvage tree. The tag has deliberately not
been moved: a pushed tag that moves is worth less than one that does not.)*

---

## The one fact that reorders everything

**Nothing in the Rust crate has ever read a real capture.**

I checked this myself rather than taking it on report, and it holds:

- No file under `src-tauri/` mentions `coins-`, `tracks-` or `tweets-`. The only
  `.jsonl` names in the whole crate are its own numbered fixture segments —
  `000.jsonl`, `001.jsonl`.
- The crate's fixture format, `sts.replay.v1`, appears in exactly three places in
  code: the spec that defines it, `replay.rs`, and `backtest.rs`. (Grep the whole
  tree and you get 13 files; the other ten are documents written *about* it, this
  one included.) **There is not
  one file of it anywhere.** No `000.jsonl` exists in the repository.
- `fixtures.rs` opens with the words "launches that never happened", and its own
  header adds that every fixture the harness has been tested against so far was
  hand-written inside a test function.

So the engine can read a corpus, in a format that has never been written, and it
cannot read the corpus we actually have because it does not speak that format.
The 43 MB of real captures in `data/` and the Rust have never met.

**"1,654 tests pass" therefore means the engine agrees with itself.** The count
reconciles without running anything: 1,678 `#[test]` and `#[tokio::test]` declarations
in the crate, 24 of them inside `geyser.rs`'s `geyser-grpc` module which is off by
default, leaving 1,422 unit and 232 integration. And there is a sharper way to put the
point than this document uses: **pointed at the real corpus, the engine would refuse it
on the first line.** `replay.rs:1103` rejects any record whose `schema` field is not
its own, and a real capture row has no `schema` key at all. It is not
evidence that any of it is right about the world. Every number in the verdict
came from Python and Node written during the sprint, against the raw files
directly.

There is one exception, and it is the thing most worth keeping.

---

## Keep — about 9,000 lines

### `replay.rs` — 6,357 lines, 123 tests, no `use crate::` anywhere

**Keep whole. This is the crown jewel, and the argument for it is the sharpest
thing anyone wrote during the sprint:**

> Every *arithmetic* self-deception this sprint suffered — a p90 read as a median, stops
> priced at the stop, deleted graduates, frozen migrated prices — was committed
> **outside** that file, in scripts that reimplemented ad-hoc what it already
> had right.

**And the limit of that argument, because it is the load-bearing one.** It holds for
the arithmetic mistakes and it does not extend to the design ones, which are the
majority of the thirty retired findings. `replay.rs` has no machinery that would
have caught the `len(highs) >= 60` array cap — that originates in the recorder, not in
any analysis — nor the null containing a rule the shuffle cannot move, nor an
unmatched holding time. Routing the analysis through this file would have prevented a
real class of error. It would not have prevented the class that did the most damage.

Two separate assets share the file.

**The curve model (`CurveState`, ~550 lines).** Integer-exact constant-product
arithmetic with the fee taken *outside* the curve, so `k` survives every swap and
rounding always goes against the trader. Plus market cap, graduation progress,
largest position for a given pool share, binary-search sell-to-target, and
`displaced()` for pricing a fill after somebody else has moved the curve.

**It is the only component in this project that has ever been checked against
reality.** W1 re-derived the formula independently in Python and tested it three
ways against the real captures: it predicts `initialBuyTokens` from
`initialBuySol` exactly for 5,927 of 5,959 real trades; token conservation holds
on 98.9% of buy-only coins; and inverting it on 35,619 real round-trip wallets
puts 97.0% at a curve position that is physically possible. The sprint went
hunting for bugs in it twice and found none.

It transfers to any `x·y=k` pool on any chain — Raydium, Uniswap v2, anything
constant-product. The pump.fun specifics are five named constants. Lift-out cost:
copy one struct and inline one `u16`. There is no `use crate::` line anywhere in the
file, but `replay.rs:1792` aliases `crate::types::MAX_POOL_SHARE_BPS` — which is 150 —
in a const initialiser twenty lines above `CurveState` itself. It is the only line in
6,357 that names the rest of the crate, and "it imports nothing" was one word too
strong.

**The record layer (~1,400 lines).** Hash-chained append-only JSONL, a mockable
clock, deterministic seeded draws keyed by a correlation id, and a cursor **with
no seek**. That last one is the part worth noticing: it makes "the decision could
not have seen a later record" a property of the type rather than something a
reviewer has to stay alert to. Vendored SHA-256 and base64 keep it dependency-free.

*The honest caveat:* nobody has ever run this Rust against the captures. W1's
verification was a Python re-derivation compared against the Rust by reading.
Strong, but indirect.

### `walkforward.rs` — the split core, ~600 of 2,201 lines

`group_launches`, `cut_blocks`, `assert_split`, `wallet_overlap`,
`one_sided_z_micros`, `lower_confidence_bound`, `cvar_bps`, and the Bonferroni
`MultipleTesting` record.

Quietly the best-reasoned code here. It implements the exact leakage controls the
sprint later discovered it needed — purge, embargo, connected-component group
split by funder — and it was written before anyone knew they were needed. Three
details better than most published work: whole funder *components* go to one side
rather than "the largest funder", because picking one satisfies the assertion by
narrowing it; wallet overlap is reported beside every metric and **never asserted
away**, because the wallet that appears in 1,829 launches is in every fold however
you cut it; and the normal quantiles are a lookup table rather than an `erf_inv`
call, because two machines that disagree in the last bit disagree about whether a
rule cleared.

Useful anywhere entities recur across observations and leakage matters — fraud,
churn, clinical follow-up, ad conversion. The type names carry crypto nouns; the
shapes underneath are `id`, `start`, `end`, `group key`, `member id`. That is a
rename, not a rewrite.

*Same caveat: it has never run on real data either. A correct harness that never
got a corpus.* The other 1,600 lines — `evaluate`, `read_corpus`, the stress grid
— are welded to `backtest.rs` and go wherever it goes.

### `fixed.rs` — 1,880 lines, 79 tests

Deterministic fixed-point at 10⁻¹⁸: `mul`/`div`/`pow`, `exp_neg`, `ln`, entropy, a
`Q18` type, and no floating point anywhere — enforced by a test that reads its own
source. Worth keeping wherever two machines have to agree on a number to the last
bit.

### `subslot.rs` — 1,456 lines, 31 tests, zero crate imports

A slot ledger tracking processed/confirmed/finalized heads with fork detection,
and a generic `TickRing<T>` that reorders a racing stream into strict chain order
and emits rollbacks on a reorg. **This is Solana infrastructure, not trading
code**, it is generic in `T`, and it is the fiddly part every Geyser consumer gets
wrong. One file, copies clean.

### `tracer.rs` — 2,331 lines, 51 tests, one crate import

The verdict says freeze the forensics, and for trading purposes that is right — but
it does not mention this file, and this file is different. It produces a *scored*
funding trace: confidence multiplied along the path, 24-hour decay from the hop
that actually delivered, a **bottleneck rule** (you cannot attribute 4 SOL down a
path whose narrowest hop moved 0.01), and hub suppression so one exchange hot
wallet does not collapse the graph into a blob.

Tracing where a wallet's money came from is a job people are paid for, and this
gives a scored answer where the free tools give a reachability answer. It takes
plain `{from, to, amount, at_ms, slot, sig}` edges — no database, no pump.fun, no
mint — so it would run on EVM transfers after a find-and-replace.

**One claim to stop repeating: "24-hop tracing" is misleading.** The depth counter
goes to 24 and the test proving it walks a single-file chain with one edge per
node. On real branching data with the default node budget it truncates at about
two or three hops. The module is honest about this; the marketing number is not.

### `ui/test/{cdp,assert,server}.mjs` — ~450 lines

A dependency-free Chrome DevTools Protocol client that spawns headless Chrome and
talks CDP over Node's global WebSocket, plus a static server and a recorder that
logs *what* was checked rather than counting silence. Replaces about 90% of
Puppeteer with nothing to install.

### Not code, but worth more than all of it

**The 43 MB of captures in `data/`, plus flux's 67 MB.** You cannot re-record
August 2026. Whatever happens to the Rust, this is the asset.

---

## Bin — about 42,600 lines of Rust, plus roughly 12,000 of Tauri shell and `ui/`

Said plainly, because a project being wound down is better served by that than by
a flattering inventory. **None of this has been deleted.**

| What | Lines | Why it goes |
|---|---|---|
| `jito.rs` + `bundle.rs` | 3,235 | There is no Jito client and never was — see below |
| `execution.rs` | 6,822 | It could not produce a transaction a validator would accept |
| `mev_sim.rs` | 3,387 | Its own header says "Nothing in here is a measurement" |
| `attribution.rs` | 3,976 | Nothing in the shipped app references it at all |
| `forensics.rs` | 4,124 | A SQLite audit log for a schema being deleted, and misnamed |
| `fixtures.rs` | 2,483 | A synthetic-fixture generator — precisely what rule 1 now forbids |
| `chainproof.rs` | 1,526 | Has never checked a real transaction; keep one page of the idea |
| `loadgen.rs` | 1,376 | Proves the throughput of a Geyser consumer that never dialled a Geyser |
| `alerting` + `daemon` + `prometheus` + `telemetry` | 8,752 | Alerting and a headless runner for an engine that never traded |
| `clustering.rs` + `strategy/syndicate.rs` | 6,906 | Freeze, per the verdict — syndicate's own header records "22 trades, no winners, −17.95%" |
| Tauri shell + `ui/` | ~12,000 | Template-grade config; keep only the 450-line test harness |

### The three claims I checked line by line

The audit rested on three specific technical claims. Each is a big call, so I read
the code rather than taking them on report. **All three hold.**

**There is no Jito client.** Exactly three files in the entire crate touch the
network: `ingestion.rs` (the websocket price feed), `metrics.rs` (an inbound
Prometheus scrape server) and `alerting.rs` (an outbound webhook). Neither
`jito.rs` nor `execution.rs` is one of them. `jito.rs` says so itself at line 36
under the heading "Nothing here observes anything" — a port with nothing behind it,
because answering it "needs a block engine's bundle stream and this crate has no
HTTP client in its dependencies". `BundleRecord` holds an id string and a set of
timers; **it contains no transactions**. Nothing is ever bundled. `jito.rs` is
reached only by `bundle.rs`, and `bundle.rs` only by a UI panel.

*One correction to the audit's wording:* it says the crate has "no HTTP dependency
at all". There is no HTTP *client library*, which is the load-bearing part and is
true — but `metrics.rs` hand-writes an HTTP server and `alerting.rs` hand-writes an
outbound webhook over TLS. The Jito conclusion is unaffected.

**`execution.rs` could not produce a valid transaction.** There is no ed25519
implementation anywhere in the crate — no `dalek`, no signing library, nothing. (The
string itself appears twice, at `execution.rs:4113` and `loadgen.rs:460`, in comments
that say so.) The only signer is
`MockSolanaSigner`, whose own doc says "**The signature is not ed25519.** It is a
digest of the message bytes and the exit's intent id. A real node would reject it."
And there is **no PDA derivation**: `find_program_address` and
`create_program_address` return zero hits, so the sell's account list is built from
label hashes rather than derived. The file does contain a genuine legacy-message
compiler, correct account ordering, compact-u16 encoding, and the real pump.fun
`sell` discriminator verified against `sha256("global:sell")` — all of which
`solana-sdk` gives away for free.

Its own header carries the sharper version of the finding: *"Nothing in `run()`
installs one, so the shipped application has no backend at all."*

And the shape of what is missing is sharper than "no entry-side builder": **the
project built the entire machinery for getting out of a position and never a rule
for deciding when.** Stop, target, trailing stop and time exit return zero hits
across `src/` and `tests/`. Positions leave the book only via the operator's panic
button, or `flatten_at_end` when a fixture runs out.

---

## What is genuinely dead, and what is merely judged

This distinction matters, because one half is a fact and the other is a decision.

**Dead by the compiler's reckoning — 8,739 lines.** Three modules are declared
`pub mod` in `lib.rs` and referenced by **no shipped code path anywhere**. Only the
integration tests reach them:

| Module | Lines | Reached by |
|---|---|---|
| `attribution.rs` | 3,976 | nothing in `src/` — only `tests/replay_tests.rs` |
| `mev_sim.rs` | 3,387 | only `attribution.rs`, which itself is unreachable |
| `loadgen.rs` | 1,376 | nothing in `src/` — only `tests/geyser_tests.rs` |

These three now carry a header comment naming this audit and its date, so the counts above are pre-header — `wc -l` today gives `attribution.rs` 3,990, `mev_sim.rs` 3,405 and `loadgen.rs` 1,390, 8,785 together. **That is
the only change made to any Rust file.** They still compile, their tests still run,
nothing is removed.

**Everything else on the bin list is reachable code.** `fixtures.rs` is reached
from the `sts backtest` CLI. `daemon.rs` is reached from `main.rs`. `forensics.rs`
and `chainproof.rs` are wired into `lib.rs`. Binning them is a judgement about what
the project is for, not a mechanical fact about dead code — which is exactly why it
should be a reviewed decision rather than a sweep.

---

## The habit worth keeping, whatever happens to the code

The lasting worth of this crate is not the trading system, which was never
connected to a market. It is a discipline that is genuinely rare: integer money,
no floats, deterministic by construction, mocks that announce loudly that they are
mocks, and doc comments that volunteer a limitation before the reader finds it.

`mev_sim.rs:33` says "Nothing in here is a measurement" at the top of 3,405 lines
somebody had every incentive to oversell. `jito.rs:36` says the port has nothing
behind it. `execution.rs:4113` says the signature would be rejected by a real node.
Every one of those was written by the author of the code, unprompted, against their
own interest.

That habit is worth more than the code it is attached to. It is also the reason
this audit took one night rather than one month.

---

## What a reviewer should decide

1. **Extract the keep pile first, delete nothing yet.** `replay.rs`, `subslot.rs`
   and `types.rs` have no crate imports at all; `replay.rs`, `tracer.rs` and
   `fixed.rs` have one each. The audit prices the whole extraction at under two days. Do it while the
   tree still builds.
2. **Then, and separately, decide the bin.** One reviewed commit, not a sweep.
3. **Do not confuse the two.** The keep pile is portable value. The bin is a
   high-quality model of a market it never touched.

---

## The build, after these changes

Run on the tree described here, with the three header comments in place:

| | result |
|---|---|
| `cargo check -j 2` | clean |
| `cargo test -j 2` | **1,654 passed, 0 failed, 0 ignored** — the baseline exactly |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |

Phase 0's gate is still closed. Nothing here broke anything.

And the number is worth one last look, because it is the point of this whole
document: **1,654 tests pass, and not one of them has ever seen a real capture.**
