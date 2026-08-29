# W70 — Both policy calls, decided by measurement

## What I found

**Both items are settled, and one of them goes against the brief's stated view —
with the numbers to say why.**

| item | decision |
|---|---|
| `SCHEMA` stuck at 2 | **bumped to 3**, stamped on every record type that can be read on its own, and `capture check` now **refuses** a version it does not know |
| `curveConservation` gating while failing 5.0% of ordinary coins | **it keeps gating.** The 5% is not good data. Evidence below |
| the SOL-flow test the reviewer declined to implement | **implemented, and it grades.** The reason for leaving it out was checkable and is wrong |
| the 8 silent fields | **5 of the 8 now have an invariant**; the other 3 are printed on screen, not only in the README |

`main` is at **`906e1e7`**, pushed. **285 tests, 0 failures** (257 before; I added
28). Nothing outside `tools/capture/` is touched.

---

## Item 2 first, because it is the one that overrules you

Your view was: *a check that fails 5% of good data must not gate.* That rule is
right. **It does not apply, because the 5% is not good data** — and you said
that if it could be shown to fire only on genuine violations, gating becomes
right. It can.

The lever is not the tolerance. It is the **shape of the miss**, which is the same
test the tree already uses to demote `tokenBalance` ("a smooth tail from 1.0 to
3.7 with no separation"). Applied to `curveConservation`, `impliedOut / bought`
over all 6,072 gradable coins:

| ratio | coins |
|---|---|
| 0.999 – 1.0 | 443 |
| 1.0 – 1.0005 | **1,298** |
| **1.0005 – 1.002** | **0** |
| 1.002 – 1.005 | **0** |
| 1.005 – 1.05 | 10 |
| 1.05 – 1.5 | 55 |
| above 1.5 | 400 |

**There is a hole exactly where the threshold sits.** 1,298 coins — a fifth of the
corpus — land within 0.05% of exactly 1. That is a price read off the trade event
and a token flow summed over `who[]`, two code paths sharing no field, agreeing
to five figures. Then **nothing at all** until 1.005.

A check cutting a smooth continuum is *densest* just past its threshold.
This one is *emptiest* there. That is the signature of two separate populations,
not of a threshold set too tight. Three more facts, each of which independently
rules out "the recorder just missed some buys":

- **Loosening does nothing.** The median failing coin needs **4.3x** more tokens
  than were ever bought. Widening the tolerance from 0.1% to **50%** moves the
  base rate from 5.0% only to 4.3%. There is nothing marginal to forgive.
- **Failures pick favourites.** Of 250 creators with 5+ graded coins, **198 fail
  none of their 2,383 coins**, while 18 fail more than half of theirs and account
  for 195 of the 465 failures. Dropped websocket messages are random.
- **Failing coins have more trades, not fewer.** Median 19 against 9, on *less*
  money (1.07 SOL against 3.94). Missing data would show up as fewer trades.

So the 5% is the rate at which an ordinary pump.fun coin prints a high that the
money in it cannot pay for. **The doctrine is satisfied, not overruled**: the rule
is about a check firing *for a reason nobody can explain*, and the reason is now
explained, measured, and printed on the screen next to the number. That last part
is the actual fix — the contradiction was never the gating, it was gating while
the tool said "base rate unknown".

### The reviewer's reason for not implementing the SOL-flow test is wrong

The stated reason: it needs an era-units rule (price is lamports per base unit on
08-10 through 08-12, whole units from 08-15) that no future row will need, so it
belongs to corpus analysis rather than the recorder.

**Checked against the files: the precondition already enforces that rule.** The
test refuses any row that does not carry its own launch `curve`, and **not one of
the 3,324 rows in the four pre-08-16 files carries a `curve` block at all** — the
curve arrived a day *after* the units changed. No old-era price can reach the
arithmetic. Every future row carries a curve by construction. The era rule has
nothing left to do.

So `solConservation()` is implemented and it grades:

| test | base rate | 5–10x | above 10x | rises with the peak? |
|---|---|---|---|---|
| `tokenBalance`, `tout > tin` | 5.8% | 5.7% | 0.0% | no — still report-only |
| `curveConservation`, tokens | 7.7% | 57.1% | 75.0% | yes — **grades** |
| `solConservation`, gross SOL in | **9.2%** | 51.4% | 75.0% | yes — **grades** |

Of the 5,974 coins both forms can grade: **462 fail both, 3 fail only the token
form, 95 fail only the SOL form.** It also needs no `who[]`, so the 200-wallet cap
does not blind it — 81 coins the token form must refuse, none of which fail.
Tolerance is 1%, the pump trading fee, because `total.solIn` is what buyers *paid*
and about 1% of that never reaches the curve; at 0.1% it fails 597 coins and at 1%
557, and the 40 between are exactly fee-sized.

I kept both. They share no field, and two independent routes to one answer are
worth more than one number.

---

## Item 1 — the version can now show its own failure

`SCHEMA` is **3**, with a changelog in `src/session.js` naming everything v3
promises. Three rules, and they are the contract:

1. **Bump it in the same commit as any change to what is written.**
2. **It is on every record type readable on its own** — the coin row, the tracks
   row, the tweets row, the failure row, the costs row, and the session header.
   Not only the header: `tracks-`, `tweets-`, `fails-` and `costs-` files get no
   header at all, and a coin row is copied out of its file constantly and arrives
   somewhere else with nothing beside it.
3. **`capture check` refuses a version it does not know.** A file stamped newer
   than the build fails the run and says so *before* anything else is printed,
   because every complaint below it came from rules written for a different
   shape. A checker that passes a file it cannot read is reporting its own
   ignorance as a clean bill of health — which is the same defect the version
   number exists to catch.

**Pointing that rule at the recorder's own output immediately found a real
defect.** The coin rows were stamped and the `tick` / `gap` / `failagg` / `stop`
rows were not, so a live session file held "v3" and "no version" at once — the
recorder failing its own rule that a file is one shape. Every line is stamped now,
with a test that pins it.

**The corpus is not renumbered or rewritten, and it still reads.** Verified:

- all 12,204 rows of the seven coin files and all 5,003 tracks rows stream, with
  the **identical** complaint set and conservation table (12,088 · 116 duplicate
  mints · 2 unparseable · 69/29 legacy caps — every figure W60 reported);
- a 2026-08-20 record parses whole, all 17 top-level fields;
- `Records.loadKeys()` recognises all 12,089 mints;
- a legacy row draws **no** schema complaint and **no** complaint from any rule
  added tonight. It is schema 1 by definition and `capture check` says so.

---

## Item 3 — the eight silent fields

They are now named **in the report output** (a `fields nothing holds to anything`
section), not only in the README. And five of the eight turned out to be
checkable:

| field | before | now |
|---|---|---|
| `who[].slotsAfter` | "no invariant available offline" | **exactly `w.slot - record.slot`** — both are on the same row. It was the most checkable of the eight. Also cannot be negative: no wallet buys a coin in a block before the one that created it |
| `whoCapped`, `highsCapped`, `lowsCapped`, `sellsCapped`, `zeroFeeCapped` | absence and `false` read the same | **from v3 all five are written on every row**, so a missing one is a defect. This is what the bump bought — the rule could not be stated at all while every shape shared one number |
| `seq` | unheld | whole number from zero, required on a v3 row, and must **advance within its session** (held across rows) |
| `si` | unheld | whole number from zero, and **no two launches share a slot position** (held across rows) |
| `connectedForSec` | unheld | **still unheld** — nothing offline can contradict it. Named on screen |

---

## How I got it

`.claude/worktrees/main-merge`, on `main`, pulled `--ff-only` before and after.
One commit, `906e1e7`, staged with an explicit `tools/capture/` pathspec — another
pass was rewriting `docs/sprint-2026-08-27/HANDOFF.md` while I worked and their
`8feeab1` is my parent. No `cargo`, no npm, no network, no listener, nothing
written to `data/` or `~/Code/flux`. Zero dependencies added.

The measurements are streaming passes over `data/coins-*.jsonl` (line by line,
aggregates only), re-derivable with `node bin/capture.js check data/coins-*.jsonl`
— the histogram bins and the creator tally were one-off scripts in the scratchpad,
and their conclusions are now in the doc comment on `curveConservation` and on the
screen.

Four fixtures had to change: `fake.js` let a test walk the curve from 30 to 90
virtual SOL while saying 0.5 SOL went in. That is a coin whose peak no money could
have produced, and the new check failed them correctly. They now say what the move
cost. This is the same class the previous pass already fixed on the token axis.

## What I could not check

- **Still nothing recorded off a live socket.** Every end-to-end test drives real
  pump.fun event bytes through a fake `WebSocket`. The standing warning holds:
  **check `slot` is non-null on the first hundred rows of the first real session**
  before letting it run for hours. Add to it: check the `start` row says `v: 3`.
- **`capture enrich` has still never been run.** I stamped the costs row with the
  schema and tested it against the fake RPC; the network path is untried.
- **The failing coins are not diagnosed, only detected.** I established that they
  are not capture loss and not a threshold artefact. I did *not* establish what
  the actor is doing. The zero-fee marker cannot help here — `feeBps` does not
  exist on any recorded row, so `zeroFeeTrades` is absent on all 6,075.
- **I did not re-derive any expectancy** and nothing here changes one.
- **`docs/archive/legacy-node/` still matches only the dated file naming** and
  would skip every session-named file. Still named, still not fixed — `docs/` is
  another pass's.

## What this means for go / no-go

**No go stands. This is instrument work and it rescues nothing.**

One thing here is worth the verdict's attention, and it is not the schema. **On
the three days the recorder can grade, between 7.7% and 9.2% of all coins print a
high that the money in the coin cannot pay for — and above 10x it is 75%.** Both
routes agree on that last figure and they share no field. Anyone sizing a strategy
on the fat tail is sizing it on quotes, not prices. That was already the finding;
what changed tonight is that it is no longer a claim in a report, it is a check
that fails the row and prints why.

The second thing is smaller and more uncomfortable. **The first time the new
schema rule was pointed at the recorder's own live output, it failed.** Not the
corpus — the shipping code, tonight, writing files with two schema versions in
them. A rule nobody had is a rule nothing is breaking; the value of writing it
down is that it starts catching things immediately, including you.
