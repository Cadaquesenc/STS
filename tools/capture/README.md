# capture — the recorder

This is the program that wrote `data/coins-*.jsonl`, `data/tracks-*.jsonl` and
`data/tweets-*.jsonl`: every launch and trade the 2026-08 verdict sprint graded.
It listens to pump.fun over one websocket, writes a line per launch, and buys
nothing.

> **Status, 2026-08-27: the corpus this produced returned a verified no go.** Buying a
> pump.fun launch loses about 7.8% a trade after costs, **−10.41% ±4.18** on the real
> holdout, loses in every one of seven hour-matched windows from October 2024 to August
> 2026, and still loses **0.86% with every cost set to zero** — independently, 0 of 207
> pre-declared rules are positive at exactly zero cost. See
> [`../../docs/VERDICT-2026-08-27.md`](../../docs/VERDICT-2026-08-27.md). **The recorder
> is in good shape and it is not the reason.** It caught 137 of 137 launches verified
> block-for-block against the chain while connected; what it was, was rarely connected.
> **Nothing below is a case for recording more.** It is a record of how the existing
> corpus was made and what can be trusted in it.

It is here because until 2026-08-27 it existed **only inside a git stash**. A
`git stash drop` would have destroyed the producer of the only irreplaceable
thing this project owns. A capture whose producer can be lost that way is not a
capture.

---

## Where it came from, and a correction

The stash had three commits, as stashes do. The one that was tagged first —
`capture-producer` / `eaaf1b4` — is the **index** commit, and it is not the
program that ran:

| | `eaaf1b4` (tag `capture-producer`) | `373825a` (tag `capture-producer-runnable`) |
|---|---|---|
| what it is | the stash's staged half | the stash's working tree |
| `src/watch.js` | 585 lines | 682 lines |
| does it parse? | **no** — statements land inside an object literal at line 365 | yes |
| writes `curve{}`? | no | yes |
| writes `initialBuyTokens`? | no | yes |

Every recorded row carries `curve` and `initialBuyTokens`, so `eaaf1b4` cannot
have written them, and a file that does not parse cannot have written anything.
It is a half-staged snapshot. **`373825a` is the producer**, and this directory
is a copy of it.

Both are preserved and pushed: tags `capture-producer` and
`capture-producer-runnable`, branches `capture-producer-eaaf1b4` and
`capture-producer-wip-373825a`. The second of those has all three stash commits
behind it, including `0f6f0c6`, which is where `src/link.js` lived — it was an
untracked file and would have gone with the rest.

`docs/archive/legacy-node/src/watch.js` is a third, older program. It has no
`curve`, no `initialBuyTokens` and no funding lookup. It did not write this data
either. Do not read it for this.

### What is here

`src/` is the dependency closure of `watch.js`, copied verbatim from `373825a`
apart from the changes listed below — 13 files, no dependencies, nothing from
the Electron app or the dashboard. `bin/capture.js` is the old `src/cli.js` with
its `dash` command removed; that command pulled in scoring, backtesting,
clustering and an HTTP server, none of which ever touched the recording.

---

## Running it

```
node bin/capture.js                    # record until Ctrl-C
node bin/capture.js --dir /some/path   # write somewhere other than <repo>/data
node bin/capture.js --follow 300       # keep writing each coin's price for 5 min
node bin/capture.js --help
```

Set `STS_RPC` to a real endpoint. Without one it falls back to the free public
node, which lags a few seconds and drops messages under load — the free endpoint
is how a session ends up with holes in it.

It runs in the foreground and only when you start it. **There is no timer, no
launch agent, no cron entry and no daemon, deliberately.** Do not add one.

Ctrl-C finishes the coins that are already inside their follow window before it
closes, so they are recorded whole instead of cut off; Ctrl-C again stops at
once and whatever is still live is written down as truncated. `--no-drain`
turns the wait off.

### The files a run produces

One file per session, named for the run and never for the calendar day:

```
coins-<sid>-<YYYYMMDD-HHMM>.jsonl    one line per launch, plus the run's own
                                     start / tick / gap / failagg / stop rows
tracks-<sid>-<YYYYMMDD-HHMM>.jsonl   what happened after the follow mark, to 12h
tweets-<sid>-<YYYYMMDD-HHMM>.jsonl   each linked tweet's engagement, 10 minutes
fails-<sid>-<YYYYMMDD-HHMM>.jsonl    a sample of transactions that failed on chain
audit-YYYY-MM-DD.ndjson              what the run did to itself
```

Rows about the run carry a `k` and rows about a coin do not, so a reader
separates them with one test:

```js
for (const line of file) { const r = JSON.parse(line); if (r.k) continue; /* a coin */ }
```

`sid` is on every row of every file, so a tracks row twelve hours later still
knows which session watched it.

### Grading what came out

```
node bin/capture.js check data/coins-*.jsonl
```

This is W21's check C7 — "no scalar field has exactly one distinct value across
the corpus" — plus the row invariants a census cannot see. Run it before
analysing anything. On the existing corpus it reproduces the defects that were
found by hand:

```
$ node bin/capture.js check data/tracks-2026-08-20.jsonl
3259 rows, 28 fields, 0 session rows

sessions:
  (none — this file predates session records: no sid, no heartbeat,
   so uptime cannot be measured, only guessed at from launch timing)
  3259 of 3259 rows carry no session id

fields carrying no information (W21 C7):
  eligible = false  — the same on all 3259 rows that have it
  score = null      — the same on all 3259 rows that have it

rows that contradict themselves:
     2165  hi is N (price never beat entry) but peakAtSec is N
```

Every defect this recorder was found to have was a field that read perfectly and
never varied. That is why the check ships next to the code that writes the file
instead of living in an analyst's notebook.

**What it holds, and what it does not.** Every field the recorder writes should
have something holding it to something else, and until this was audited two did
not: `outcome.curveAtEntry` and `outcome.feeSol` were added and no check was
ever pointed at them. Both are held now — the entry price has to be the ratio of
the reserves that are supposed to be behind it, a candle's close has to be the
ratio of the reserves it says it closed on, a candle carries all four reserve
fields or none, and a fee cannot exceed the SOL it was charged on. **Five of the eight fields that used to be held to nothing now have an
invariant.** `who[].slotsAfter` was called uncheckable and is the most checkable
of them: it is exactly `w.slot - record.slot`, and both are on the same row (it
also cannot be negative — no wallet buys a coin in a block before the one that
created it). The five `*Capped` flags got theirs from the schema bump: from v3
the recorder writes all five on every row, so a missing one is a defect rather
than a shorter way of writing `false` — a rule that could not be stated at all
while every shape shared one version number. `seq` and `si` are held to their
range and to their order across rows (`seq` advances within a session; no two
launches share a slot position), and `connectedForSec` is held to nothing at all.
`capture check` prints that list under **fields nothing holds to anything**, so
it is on screen and not only here.

### What a transaction cost to land

```
STS_RPC=<endpoint> node bin/capture.js enrich data/coins-*.jsonl
```

The fee, the priority fee and the compute units are **not on the socket payload**
— they need a `getTransaction` per signature — so this is a separate command and
never a line inside `watch.js`. A listener that waits on a network round trip
drops launches, and a dropped launch is the one thing that cannot be recovered.

It reads the `sig` on every coin and on every opening buyer, resolves each one
once, and writes `costs-<session>.jsonl` **beside** the capture, joined on `sig`.
The recorded rows are never touched. A signature already in the output is
skipped, so an interrupted pass costs nothing and a finished one can be re-run;
a signature the chain has pruned is written down as gone rather than retried for
ever. `--limit N` stops after N lookups, and the summary distinguishes "finished"
from "ran out of budget".

Each row carries:

| field | means |
|---|---|
| `feeTotal` | lamports the transaction paid, all in |
| `feeTotalSol` | the same number in SOL, for reading. **Not `feeSol`** — the coin record already has an `outcome.feeSol` and it is a different thing (the pump trading fee its traders paid over the follow window). These two files are joined on `sig`, so one name across the join would hand a reader two numbers under one label |
| `feeBase` | 5,000 x required signatures — the protocol's price, which nobody bids |
| `feePriority` | `feeTotal - feeBase`: **what was charged**, not what was reserved |
| `cuLimit` / `cuPrice` | what it asked for, decoded off its own ComputeBudget instructions. `null` means it set none, not that we could not tell |
| `cuUsed` | what it actually burned. `cuLimit x cuPrice` and `feePriority` differ whenever these two do |
| `jitoTip` | lamports that reached a Jito tip account, read off the balance changes |
| `err` | true when it failed. **A failed transaction still landed and still paid** — 93.2% of pump transactions on the wire fail, and dropping them is how the failure cost stayed invisible |
| `feePayer`, `signers`, `slot`, `blockTime` | who paid and when |

`flux/src/enrich.js` is the reference and fetches the same way. This adds the two
things it leaves out: base and priority fee separated, and the ComputeBudget
instructions decoded, so "what did it cost to land" can be answered as "what did
I pay for, and what did I get".

**It does nothing for the recorded corpus.** `signaturesIn()` over
`coins-2026-08-20.jsonl` and `coins-2026-08-21.jsonl` finds **0 signatures** —
those rows predate `sig`, and no amount of later work puts one back. The cost
model stays the reconstructed one until a session recorded by this program has
been through this pass.

---

## What was fixed here

### The tracker's second window contradicted itself

`Tracker.adopt()` reset `hi` and `lo` to 1 at the follow mark but carried
`peakAtSec` over from the first minute. 2,165 of 3,259 rows on 2026-08-20 said
"the price never beat entry" and "the best price was at second 14" at once.

`hi`, `lo`, `peakAtSec` and the `cross` ladder now reset together. Nothing is
lost: the first minute's peak time is already in `coins-*.jsonl` as
`outcome.peakAtSec`, and a reader who wants both joins on `mint`.

**What was *not* changed, and matters:** `adopt()` does not re-base the entry
price. It reads `coin.outcome.entry` — the price at the three-second mark, what
a strategy would actually have paid — and every multiple in a tracks row is
measured against that, out to twelve hours. Verified on all 4,083 joins in the
corpus, no exceptions. Six sprint reports wrote off the long horizons believing
otherwise. It is a good property; there are tests holding it in place.

### Fields nobody ever filled in

`score` and `eligible` came from a second argument to `adopt()` that the one
caller never passed, so they were `null` and `false` on all 5,003 rows. Deleted.

`funding.depth` was the configured cap echoed back — the literal `2` on all
5,659 records, while the comments advertised a 24-hop tracer. It said nothing
about any launch. Replaced with three things that do:

- `hopsWalked` — how many hops this call actually made. Fewer than the cap
  whenever the frontier ran dry, which is most launches.
- `perHop[{hop, asked, resolved}]` — what each hop cost and what it bought.
- `status{ok, none, truncated, error, notAsked}` — why each wallet has no edge.
  Only `none` is a claim about the wallet. The rest are claims about us, and
  `cluster.js` reads a missing edge as proof two wallets are unrelated.

A field that is always null is worse than no field, because a reader trusts it.

### A record now says how long it was really watched

`outcome.follow` was the configured `60` on all 8,881 rows of the corpus,
including the ~14% of coins the listener was still watching when it stopped.
Those coins have a median last candle at second 3 against 26 for the rest, and
nothing on the row said so — so every expectancy number the project has produced
quietly averaged cut-off observations in with whole ones.

Four additive fields replace the guess. `follow` stays, as the window that was
promised:

| field | means |
|---|---|
| `outcome.observedSec` | whole seconds actually watched. Equal to `follow` when the window ran its course, strictly less when it did not |
| `outcome.complete` | **the flag to branch on**: the whole window *and* the feed up throughout |
| `outcome.stopReason` | `window` \| `shutdown` \| `socket-down` |
| `outcome.gapSec` | seconds inside *this coin's own* window when the socket was down |

`gapSec` closes a hole nobody had flagged: the follow timer fires whether or not
the feed is alive, so a coin that launched cleanly and lost twenty seconds
mid-window used to read as a complete observation. Any non-zero downtime rounds
**up** to one second, so `gapSec > 0` means exactly "the feed dropped during this
window" and never "it dropped for less than half a second".

The analysis rule that follows: **default to `complete && gapSec === 0`, and
state the count you dropped.**

### Every row says which shape it is

`SCHEMA` in `src/session.js` is **3**. It sat at 2 through five commits that each
changed what a record carries — `observedSec`, `complete`, `stopReason`,
`gapSec`, `sid`/`seq`, heartbeat rows, `slot`/`sig`/`si`, four reserve fields a
candle, `curveAtEntry`, `feeBps`, `zeroFee[]`, `feeSol`, `sells[]`, `whoCapped`
and more — so two files both stamped `v: 2` could hold entirely different shapes
and nothing on either said so. A version that does not move is a field that reads
perfectly and cannot be wrong, which is the one defect every check in this
directory exists to catch.

Three rules follow, and they are the whole contract:

1. **Bump `SCHEMA` in the same commit as any change to what is written**, and add
   a line to the changelog in `session.js`. That list is what lets a reader say
   what is on a row without inferring it from which fields happen to be present.
2. **The version is on every record type that can be read on its own** — the coin
   row, the tracks row, the tweets row, the failure row, and the `start` header.
   Not only the header: `tracks-`, `tweets-` and `fails-` files get no header at
   all, and a coin row is copied out of its file constantly and arrives at the
   far end with nothing beside it.
3. **`capture check` refuses a version it does not know.** A file stamped newer
   than this build fails the run and says so before anything else is printed,
   because every complaint below it was produced by rules written for a different
   shape — a checker that passes a file it cannot read is reporting its own
   ignorance as a clean bill of health.

**The recorded corpus carries no `v` at all and is not renumbered or rewritten.**
It is schema 1 by definition, `capture check` says so and reads it, and every
rule added since is asked only of rows that claim to carry the fields it is
about. Verified: all 12,204 rows of the seven coin files and all 5,003 tracks
rows still stream, a 2026-08-20 record still parses whole with all 17 of its
top-level fields, and `Records.loadKeys()` still recognises all 12,089 mints.

### Every row says which run recorded it, and the run says whether it was up

- `sid` and `seq` on every coin, tracks and tweets row.
- A `start` header per file: schema version, pid, git commit, redacted endpoint,
  and **every bound the run recorded under** — `highsCap` (which bounds `lows`
  too), `sellsCap`, `zeroFeeCap`, `whoCap`, and the failure sample rate. A row
  that says `sellsCapped: true` in a file that never says what the cap was is
  the same defect as a sample whose rate is not written down: the number is
  there and the thing needed to read it is not. Two of the five were missing
  until this was audited.
- A `tick` heartbeat every 10 seconds (`--heartbeat`), **written whether or not
  anything happened**. Uptime is then `connected ticks ÷ ticks`, a measured
  number rather than something inferred from how far apart launches happened to
  fall. An outage and a quiet market are now different files.
- A `gap` row on every reconnect, `from` set to the last message actually
  received. It used to reach a counter and the terminal and nothing else.
- A `stop` footer whose every counter is backed by rows in the same file.
- **One file per session.** The dated naming split a fifteen-hour run at UTC
  midnight, and the second half was then treated as an independent holdout day
  by six analyses. The filename is fixed when the run starts and nothing about
  it depends on the clock afterwards.

### `slot` and `sig`, so a transaction can be costed later

Neither existed. That is why the cost model had to be rebuilt weeks later out of
25 transactions in another project's files. Now on every coin record: `sig`,
`slot`, `si` (our observed position among the pump transactions in that slot —
*not* the block index, which `logsSubscribe` does not carry) and
`connectedForSec`. Each wallet that got in before the opening cutoff carries its
own `sig`, `slot`, `si` and `slotsAfter`: that is the fee ladder, on thousands
of transactions a session instead of 25.

Fee, priority fee and compute units are **not** on the `logsSubscribe` payload.
They come from `getTransaction` against these signatures, offline — that is
`capture enrich`, above, and it writes its own file rather than touching the
capture. **No RPC call was added to the hot path** and none should be.

### Failed transactions are recorded, not thrown away

`watch.js` line 102 used to read `if (!value || value.err) return;` — failures
were dropped without even a count, and they outnumber successes about 14 to 1 on
the wire. A failure that reached a block still paid its fee, so the real cost of
landing a trade may be the fee times the attempts it took.

- Every failure is rolled into a per-minute `failagg` row: total, kept, and a
  census by error kind. The headline rate is therefore reproducible from the
  file rather than resting on a counter.
- A deterministic 1-in-50 sample (`--fail-sample`, 1 keeps everything) goes to
  its own `fails-*.jsonl` with `sig`, `slot`, `si` and a normalised error code.
  Keeping all of them would be 0.5–4 GB a session against ~170 MB for everything
  else; the rollup keeps the totals exact anyway.
- **The rate is on every kept row and in the `start` header.** A sample whose
  rate is not written down is not a sample, it is a hole — the same defect as
  `follow: 60`.
- The error kind is the valuable half: `ix3:custom:6002` is pump's slippage
  error, meaning somebody was outbid; `ix0:AccountInUse` is contention. They say
  opposite things about whether a strategy is uncompetitive or merely slow.
  Unrecognised shapes keep their raw error rather than being flattened.
- Failures dedup **before** they are counted, and in their own set. Counting
  first made every redelivered failure count twice; sharing the successes' set
  would let one burst evict the signatures that stop launches double-counting.

### The turning-point lists no longer freeze the extreme

`highs`/`lows` were capped at 60 entries, and the running extreme was inside the
same condition as the push — so once the list was full `hi` froze, and because
the low branch sat behind an `else if` whose test now always passed, new lows
stopped being recorded too. It bit 0.2–1.2% of priced coins and it bit the
winners, because a coin that keeps making new highs is the one that runs out of
room. 51 rows of the corpus are on the cap.

The cap is now 1,000 (`highsCap`), the extreme always moves, and the row carries
`highsCapped` / `lowsCapped` so a truncated list can never pass for a complete
one.

### A sell now names the wallet that made it

`market.candles[].sells` counted sells and named nobody, and `who[]` carries a
wallet's totals over the whole window with no per-sell timing. So "is the
creator still holding at second N" was only ever answerable as "has anybody sold
by second N", and those are different questions — the difference is the whole
sell-side signal. The seller's address is on every trade event; it was being
thrown away, not missing.

`outcome.sells[]` is one entry per sell inside the follow window, positional
like `highs` and `lows`:

```
[at, wallet, sol, tokens]      at = seconds since launch, one decimal
```

`outcome.creatorSellAtSec` names the second the creator first sold, or null —
the same treatment `initialBuySol` already gives their first buy. It is derived
from the ledger and `capture check` holds the two to each other, so it is a
number with its rows still behind it rather than one to be taken on trust.

Sells before the entry price is struck are recorded like any other: a third of
creator dumps happen inside the first three seconds, so anything that waited for
a price would have lost exactly the ones that matter.

**Size.** Measured on `coins-2026-08-20.jsonl`: median 2 sells a coin, mean 15.5,
p99 209, max 570 — about +1.1 KB on a mean record of 4.35 KB, so roughly a
quarter more. The cap is 1,000 (`sellsCap`) with a `sellsCapped` flag, and it
bites nothing in the recorded corpus.

### Every counter has to be reconstructable from rows

W21's C21, and the lesson underneath every defect this program has ever had.
`stats.failed` counted 645,741 failed transactions and kept none of them, so a
possibly verdict-changing fact sat behind a number nobody could check for weeks;
`funding.depth` and `outcome.follow` hid theirs by being constants nobody looked
at.

`capture check` now holds every counter in a session footer to the rows in the
same file — launches and `written` to the coin rows, `truncated` to their
`complete` flags, `beats` and `connectedBeats` to the ticks, `gaps` and `gapMs`
to the gap rows, `failed` and `failLogged` to the per-minute rollups. A counter
that disagrees with its rows fails the check.

One counter has no rows behind it at all: `stop.trades`. This recorder writes one
row per coin, not one per trade, so a run's trade total genuinely cannot be
rebuilt from the file. It is **named** in the report rather than tolerated
silently, which is the difference between a known limit and a defect.

The same rule applies inside a row: `market.candles[].sells` and
`total.sellers` are now checked against `outcome.sells[]`, and
`outcome.creatorSellAtSec` against the same ledger.

### The curve state a price came from is now kept, not just the price

This is the field the corpus most wishes it had, and it was **already on the
wire at every single trade**. `realSolReserves` sits at byte 105 of every trade
event; the decoder has read it since it was written; nothing ever logged it.

W32 traced the "impossible price" problem that affects 18.4% of coins and
concentrates in nine of the ten largest peaks. It is **not a decoder bug and not
a capture bug** — those prices really printed on chain. The chain priced every
following trade off the impossible reserve value: predicting the next trade from
the previous event's reserves matches 99.5% of bad trades, predicting from the
launch curve matches 2.9%. The pool state genuinely was that. One actor is
responsible — 1,396 of 1,398 zero-fee trades on 2026-08-10 come from a single
wallet across 43 coins — and it is new: none in Oct 2024, four in Aug 2025, then
25% / 14% / 8% across the 2026 capture windows.

**No repair path exists for the existing corpus.** `watch.js` stored only derived
open/high/low/close prices, and no per-trade reserve figure was ever written by
anything, so **zero coins are recoverable**. A quarter of the dataset's tail is
permanently unusable for want of two numbers that were sitting in the decoded
event the whole time. That is the strongest argument this project has for
recording raw state rather than state you have already reduced: the reduction is
always the part you needed back.

**Where the fields are, verified rather than taken on trust.** Byte offsets into
the trade event payload, counted after the 8-byte event discriminator, checked
by slicing the stored raw base64 of 30 real events and comparing every field
against the decoder's answer — all 30 agree on all nine:

| field | byte | field | byte |
|---|---|---|---|
| `sol` | 32 | `realSolReserves` | **105** |
| `tokens` | 40 | `realTokenReserves` | 113 |
| `virtualSolReserves` | 89 | `feeBasisPoints` | **153** |
| `virtualTokenReserves` | 97 | `fee` | 161 |
| | | `creator` | 169 |

They also fall straight out of `readTrade` in `src/pump.js` if you add up the
widths — 32 for a pubkey, 8 for a u64, 1 for a bool — which is the cheapest way
to check them and the one that cannot go stale. One source report had
`virtualTokenReserves` at 105 in one section and `realSolReserves` at 105 in
another; the arithmetic and 30 real events both say `realSolReserves`.

Six fields close it:

- **Every candle carries the reserves it closed on** — `vsol`, `vtok`, `rsol`,
  `rtok`, in whole SOL and whole tokens, at full precision (9 decimals for SOL,
  6 for tokens) so the conversion out of base units is exactly invertible and
  nothing is discarded. Rounding them for looks would be the same mistake as
  storing the price instead of the reserves, one decimal place further down.
  Bounded by the 60-candle window: median 3 candles a coin, mean 8.4.
- **`outcome.curveAtEntry`** — `[vsol, vtok, rsol, rtok]` at the instant the
  entry price was struck. `entry` is a price and a price is a ratio; this is the
  absolute state behind it, so a reader can put an order size on the curve
  instead of inferring one from a multiple.
- **`outcome.feeBps`** counts what fee rate each trade actually paid, and
  **`outcome.zeroFee[]`** keeps every zero-fee trade in full —
  `[at, wallet, sol, tokens, buy, vsol, vtok, rsol, rtok, fee]`. A normal pump
  trade pays 95 basis points; zero percent of zero-fee trades obey the launch
  curve while 92–95% of 95-bps trades do.
- **`outcome.zeroFeeTrades`** and **`outcome.curveSuspect`** — the count and the
  flag, so an analyst sees it on the row rather than inferring it a fortnight
  later from raw bytes in another project. Both are held to the census and the
  ledger by `capture check`, the same counter rule as everywhere else.
- **`outcome.feeSol`** — the pump trading fee actually paid over the window. It
  is on every trade event and has never been written down, which is why every
  cost model here has carried a remembered 1% against a chain charging 95 bps.

#### The conservation rule, and what it actually catches

The curve is a constant product, so a price pins both reserves exactly:
`vtok = sqrt(k / price)`. Reaching the printed peak therefore *requires* the
curve to have handed over `virtualTokens - sqrt(k / peak)` tokens — and it can
never have handed over more than were bought out of it, which `who[]` counts
from a completely different code path. When the first number exceeds the second
the peak on that row is arithmetically impossible: a quote, not a price anyone
could have sold into.

It needs `curve`, `outcome.peak` and `who[].tin` and nothing this recorder just
added, so **it grades the existing corpus today**. `capture check` prints the
split, because the gradient is the finding and one number for a file hides it —
`capture check data/coins-*.jsonl`, 6,075 coins with a launch curve, a peak and
an uncapped `who[]`:

| peak | coins | impossible |
|---|---|---|
| 1–1.5x | 5,423 | 5.0% |
| 1.5–2x | 331 | 24.2% |
| 2–3x | 204 | 24.5% |
| 3–5x | 81 | 51.9% |
| 5–10x | 28 | 57.1% |
| **above 10x** | **8** | **75.0%** |

Overall 465 of 6,075, 7.7%, against the 7.3% W32 reports for the same test. **The
shape reproduces and the exact percentages do not**: W32 quotes 100% above 10x
and 88.9% at 5–10x, and on this slightly larger population it is 6 of the 8 coins
above 10x rather than 6 of 6. Reported as measured, and re-derivable by running
the command.

Two smaller assertions run beside it, on the new fields: a candle cannot close
below the curve its own coin opened at — **the row's own `curve.virtualSol`, not
a hardcoded 30, because 216 of the 7,926 recorded coins that carry a curve open
at 4.292** — and a curve cannot hold more tokens than it ever issued.

**Three checks, and two of them grade.** This has to be unambiguous or a reader
will either act on all of them or ignore all of them, so `capture check` says it
on screen as well as here:

| check | what it asks | in the code | in the report |
|---|---|---|---|
| `solConservation()` | does the printed peak need more SOL into the curve than anyone ever put in? | **fails the row** | its own peak table, and one line per failing coin |
| `curveConservation()` | does the printed peak need more tokens out of the curve than were ever bought out of it? | **fails the row** | its own peak table, and one line per failing coin |
| `tokenBalance()` | were more tokens sold than were bought? | **never fails a row** | one counted line, labelled `REPORTED ONLY` |

The second is the blunter reading of the same file — `tout > tin`, nobody can
sell a token they never bought — and it fires on 5.8% of coins with no structure
at all: a smooth tail from 1.0 to 3.7, flat across every peak bucket (5.4% under
1.5x, 0% of the eight above 10x). That is the profile of a systematic accounting
difference between the buy and sell legs, not of an anomaly. A check that fires
on 6% of ordinary coins for a reason nobody can explain is a check that gets
ignored, which is how the last set of defects survived.

**The third form is the strongest, and it is now implemented and grading.** It
asks whether the peak needs more SOL than ever entered the coin, and the ceiling
has to be **gross** `total.solIn` rather than net — the peak is transient, so
money that came in and left again still paid for it. On net inflow it fires on
73% of everything and means nothing.

| test | base rate | 5–10x | above 10x | rises with the peak? |
|---|---|---|---|---|
| `tokenBalance`, `tout > tin` | 5.8% | 5.7% | **0.0%** | no |
| `curveConservation`, tokens | 7.7% | 57.1% | 75.0% | yes |
| `solConservation`, gross SOL in | **9.2%** | **51.4%** | **75.0%** | yes |

It was previously left out on the grounds that it needed an era-units rule — the
stored price is lamports per base unit on 08-10 through 08-12 and whole units
from 08-15 — that no future row would need. **That reason was checked against the
files and does not hold.** The test refuses any row that does not carry its own
launch `curve`, and **not one of the 3,324 rows in the four pre-08-16 files
carries a `curve` block at all**: the curve arrived after the units did. So no
old-era price can reach the arithmetic, every future row carries a curve by
construction, and the era rule was already being enforced by a precondition that
was there for another reason.

Of the 5,974 coins both forms can grade, 462 fail both, **3** fail only the token
form and **95** fail only this one. It also needs no `who[]`, so the 200-wallet
cap does not blind it — 81 coins the token form has to refuse, none of which turn
out to fail.

**Why `curveConservation` grades while firing on 5.0% of ordinary coins.** The
rule about ignorable checks turns on the *reason*, not the rate, and the reason
is now measured. Bin `impliedOut / bought` finely around the threshold and there
is a hole at it: **1,298 coins land within 0.05% of exactly 1** — two unrelated
code paths agreeing to five figures — and then **nothing at all until 1.005**.
A check cutting a smooth continuum is densest just past its threshold; this one
is emptiest there. The median failing coin needs **4.3x** more tokens than were
bought, and widening the tolerance from 0.1% to 50% moves the base rate only from
5.0% to 4.3%. Nor is it the recorder missing buys: dropped messages are random
and would pile up just above 1, failures instead concentrate by creator (**198 of
250 repeat creators fail none of their 2,383 coins** while 18 fail more than half
of theirs), and failing coins carry *more* trades (median 19 against 9) on *less*
money (1.07 SOL against 3.94). So the 5% is the rate at which an ordinary coin
prints a high the money in it cannot pay for — a fact about pump.fun, not a false
alarm — and the reason is printed next to the number on screen.

**The fixtures were the first thing the conservation rule caught.** `fake.js`
moved `virtualSol` while leaving `virtualTokens` at a fixed 1,073,000,000 — a
curve state the chain cannot produce, where the price moves and no tokens leave.
The fake now derives one side from the other and hands over the tokens the move
implies, which is also why a test that triples the SOL side now expects a 9x
price rather than a 3x: the product is what is fixed.

### A coin name could split its own record

`coins-2026-08-20.jsonl` line 1934 is a coin named `Power Belongs⟨U+2028⟩in
Human Hands`. `JSON.stringify` leaves U+2028 raw — it is legal inside a JSON
string — and Node's `readline`, like most streaming readers, treats it as the
end of a line. That record reaches every streaming analysis as two fragments,
neither of which parses, and it is the only unreadable row in the whole corpus.

Coin names, symbols and metadata URIs are written by whoever launched the coin,
so this is untrusted text going straight into the archive: one character in a
ticker was enough to destroy a row. `record.js` now escapes U+2028 and U+2029 on
the way out. Nothing else about the line changes and the value round-trips
unaltered.

### Two things found while moving it

**`--dir` did not reach the coin log.** `watch()` passed the directory to the
database and to the tracker but not to `Records`, so `coins-*.jsonl` and
`tweets-*.jsonl` went to `$STS_HOME` or `<repo>/data` regardless of what you
asked for, and said nothing about it. All four writers now get it, and
`bin/capture.js` prints the directory it resolved.

**The old `sts` command never wrote a tracks file at all.** It called `watch()`
with no `dir`, which left the tracker with `save: false`. Every `tracks-*.jsonl`
in the corpus came out of `sts dash`, which did pass one. `bin/capture.js`
always passes one.

**Stopping did not stop.** The per-coin `summarise` and `finish` timers were
never cancelled, so after shutdown they kept firing into closed writers. Their
writes were all dropped further down, so nothing was corrupted — but a stop that
leaves work scheduled has not stopped.

### Two authors in one directory, and what the seam left behind

Almost everything above was written on the night of 2026-08-26 across two passes
working in the same checkout at the same time, neither able to see the other.
Their commits interleave: `80d2bab` carries some of the other's working tree
mixed into its own. Nothing was lost — the test count and every measured number
reproduce — but the review that followed found four things at the seam, and they
are all one thing:

**A field added by one hand is not covered by a rule written by the other.** The
counter rule — every number must have its rows still behind it — was written in
one commit; `outcome.curveAtEntry` and `outcome.feeSol` were added in another,
and the rule was never pointed at them. Either could have been deleted, negated
or made to contradict the price beside it and `capture check` would have passed
the file. The same seam left `sellsCap` and `zeroFeeCap` out of the session
header while `highsCap` and `whoCap` were in it, left the check accepting a
candle with three of its four reserve fields, and left the costs file's Solana
network fee sharing the name `feeSol` with the coin record's pump trading fee —
two different numbers under one label in two files designed to be joined.

All four are closed above. The rule for whatever is added next is the one that
was already written down and simply not applied twice: **a field arrives with
the thing that stops it drifting, or it does not arrive.**

---

## What W21 asks for and this does not do

W21 (`W21-capture-spec.md`) is the specification for the next capture. This
producer meets some of it and not the rest. **Nothing below is fixed here.** The
gaps are listed so nobody has to rediscover them.

| W21 | What it asks for | State |
|---|---|---|
| §1 `observedSec`, `complete`, `stopReason`, `gapSec` | say how long each coin was really watched | **done.** Additive: `follow` stays as the promised window |
| §1 session id and 10-second heartbeat | uptime as a measured fact | **done.** `sid` and `seq` on every row; `start`/`tick`/`gap`/`failagg`/`stop` rows |
| §1 one file per session, never split at UTC midnight | stop a 15-hour run reading as two days | **done.** `coins-<sid>-<YYYYMMDD-HHMM>.jsonl`, fixed when the run starts |
| §1 `slot`, `sig`, `tx{}` cost block | the launch's fee, priority fee, CU price, position in block | **done, in two halves.** `slot`, `sig`, `si` and `connectedForSec` are on the record and on every opening buyer with `slotsAfter`; `capture enrich` resolves the fee, priority fee, compute units and Jito tip against those signatures offline. **It recovers nothing for the existing corpus** — those rows have no `sig` at all |
| §4.1 record failures, do not tally them | 93.2% of pump transactions on the wire fail and still pay a fee | **done.** Per-minute rollup always, a deterministic 1-in-50 sample to `fails-*.jsonl` with the rate recorded, dedup before the count. **Not done:** `slotroster-*.jsonl` — that needs `getBlock` per launch slot, which is a network call |
| §1 `outcome.highs[]`/`lows[]` cap | cap 1000 with a `highsCapped` flag | **done** |
| §1 `entryAtSec`, `entrySource` | say when entry was struck | **not done.** Entry is the price at the three-second mark. Two sprint reports disagreed about what it meant for a whole sprint |
| §1 `curveAtEntry` | per-coin reserves instead of the launch constant | **done.** `outcome.curveAtEntry` is `[vsol, vtok, rsol, rtok]` frozen at the same instant `entry` is, and every candle carries the live reserves besides |
| §1 `social.text` | keep the tweet text | **not done** |
| §2 tiers B/C/D, `horizons-*.jsonl` | 5-minute candles and cheap curve snapshots to 24h | **not done.** The tracker still holds coins in memory for 12 hours and loses everything a crash interrupts |
| §1 drop `tracks-*.jsonl` entirely | W21 would delete the file and `adopt()` | **not taken.** The second window survives here, made self-consistent instead. It is the only long-horizon data that exists, and it is measured against the entry price a strategy pays — see above |
| §5 C7 dead fields | no scalar field with one distinct value | **done** — `capture check`, and it is what caught the two defects above on the real corpus |
| §5 C13 rows that contradict themselves | zero self-contradictory rows | **partly.** Checked: `hi`/`peakAtSec`, the funding block, the truncation facts, the sell ledger against the counts that summarise it, the fee census against the zero-fee ledger, `entry` against `curveAtEntry`, every candle's close against the reserves it closed on, and `feeSol` against the SOL it was charged on. Not checked: `endMult == last/entry`, and the candle-versus-peak check |
| §5 C21 counters with nothing behind them | every counter reproducible from rows | **done, and checked.** `capture check` holds every footer counter to the rows in the same file and fails on a disagreement. `stop.trades` is the one counter with no rows behind it — one row per coin, not one per trade — and it is named in the report rather than tolerated |
| — sell attribution (W35) | which wallet sold, and when | **done.** `outcome.sells[]` and `creatorSellAtSec`. Not asked for by W21; it was the wall W35 hit on the only input in the sprint that beat a matched-exposure baseline |
| — raw curve state (W32) | reserves per trade, and the zero-fee marker | **done.** Reserves on every candle and at entry, `feeBps` census, `zeroFeeTrades`/`curveSuspect`, `zeroFee[]` ledger, `feeSol`. Every one of them was already decoded and never written; the corpus has no repair path because of it |
| — curve conservation (W32) | flag a peak more tokens left the curve for than were ever bought | **done twice over, and both grade the existing corpus.** `curveConservation()` on the token side and `solConservation()` on the SOL side, per row, each with its own split by peak size because the gradient is the finding: 5.0% of coins under 1.5x against 75% of those above 10x |
| §1 `whoCapped` | flag the 200-wallet cap | **done.** It is what makes any sum over `who` sound or unsound |
| §5 C17 impossible curve state | zero rows below the 30 SOL floor | **done for new captures** — checked per candle, which needs the reserves that were never recorded before |
| §5 C1/C2/C3 uptime, gaps, session identity | run them at all | **done** — `capture check` reports uptime per session from the heartbeats, and flags a file with two sessions or a session across two files |
| §5 C9/C10 duplicate mints, out-of-order writes | catch both | **done** — `capture check` |
| §0 the producer lands on a branch | before the next session | **done** — this directory, plus the two tags and two branches above |

The short version: **a row can no longer be quietly wrong about how it was
made.** What is left is either a network call this recorder deliberately does
not make on the hot path (the fee block, the slot rosters, the curve snapshots)
or a field nobody has needed badly enough yet (`entryAtSec`, `social.text`).

The `check` command is the place to add the next one. Every defect this program
has ever been found to have was a field that read perfectly and never varied, so
the rule for anything added later is the one W21 states as C21: **can I get back
to the underlying rows from this number?** If not, it is decoration.

---

## Tests

```
node --test "test/*.test.js"      # 285 tests, no network, nothing started
npm run test:capture              # the same, from the repo root
```

`test/capture.test.js` drives the real `watch()` end to end: real pump.fun event
bytes go in at the websocket and real files come out on disk, with nothing
stubbed in between. `globalThis.WebSocket` is replaced for the duration of each
test and put back afterwards, so no socket is ever opened to anything. It also
drops and reconnects that socket, so `gapSec` is exercised through the real
reconnect path rather than by setting a field.

| file | tests | what it holds |
|---|---:|---|
| `test/capture.test.js` | 64 | the whole recorder end to end: truncation, drain, sessions, heartbeats, slot/sig, failures, the caps, the sell ledger, the reserves on every candle and at entry |
| `test/check.test.js` | 116 | the C7 census, every row invariant, the C21 counter rule, curve conservation, the price-against-its-own-reserves invariants, and the session/uptime report over files |
| `test/track.test.js` | 32 | the second observation window, and the entry price that is never re-struck |
| `test/session.test.js` | 30 | the pure logic: `closeFacts`, the failure sample, uptime from heartbeats |
| `test/enrich.test.js` | 22 | the cost block: base against priority fee, the compute budget, a Jito tip, a pass that is resumable against a fake RPC, and the field name that must not collide with the coin record's |
| `test/funding.test.js` | 21 | `hopsWalked` / `perHop` / `status` |

Every test is named for the mistake it prevents.
