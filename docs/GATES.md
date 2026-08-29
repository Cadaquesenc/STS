# GATES — what the acceptance ladder should have been

**Companion to [`POSTMORTEM-2026-08-27.md`](POSTMORTEM-2026-08-27.md) — 27 August 2026**

The postmortem says what went wrong: of forty acceptance criteria, three required the
system to be right about the market, and the first of those was the twenty-third in
sequence. This is the other half — the ladder that should have been there. It is not
a rewrite of [`STS_ROADMAP.md`](../STS_ROADMAP.md), which stays as it is, a
historical record with its own stop block and corrections. It is fifteen gates that
can fail, in the order they should be run.

---

## The rule

**A gate must be able to come back FAIL for a reason that is not your fault.** If
every route to a FAIL is "we wrote a bug", the gate is testing the machine, not the
idea. Both are worth testing — determinism, replay equivalence, latency budgets, key
isolation and non-custody are real gates, and STS's versions of them were good ones.
But a ladder made only of those goes green from top to bottom while the thing under
it does not work: 1,654 tests passed here against a fixture file whose opening words
are *"launches that never happened."* The fix is not more rigour in the engineering
gates. It is that **at least one gate near the front must be able to fail because
the world disagrees with you**, and it should be the cheapest such gate, not the
most complete one. A weak test in week one beats a rigorous one in month ten.

The second rule follows, and it is what this document is really for: **a control
that is not attached to a gate is a comment.**
[`STS_CORE_IDEOLOGY.md`](../STS_CORE_IDEOLOGY.md) already held most of the
methodology needed to catch this — AL.1's multiple-testing correction and untouched
test set, AL.3's "a zero-slippage fill is invalid", AL.4 on survivorship, AL.5 on
economic significance, §14's zero-trade decomposition, Annex W's outcome
definitions, "UNKNOWN never becomes PASS through defaulting". All correct, all
written before it was needed, almost none of it wired to anything that returns a
PASS or a FAIL — it sat in a *Deliverables* list. The table near the end wires each
one to a gate and says how you would tell it had been skipped.

---

## Three kinds of gate, and the order

| Kind | How many | Can fail because | Runs on |
|---|---|---|---|
| **D** — data gates | 6 | the corpus does not describe the world | real captures |
| **T** — thesis gates | 6 | the market disagrees with you | real market history |
| **L** — live gates | 3 | reality disagrees with your simulation | a running system |
| **E** — engineering gates | (existing) | you wrote a bug | anything |

The E gates are the forty already in the roadmap. **Keep them — they were not the
problem.** Change one thing: they no longer authorise anything. Passing every
engineering gate means the machine is well made; it does not unlock a capture, a
paper account or a euro. Only D → T → L does that, and in that order, because a
thesis gate on an untrustworthy corpus is worse than no gate — it returns a
confident number.

**Every gate emits PASS, FAIL, DEGRADED or UNKNOWN, and UNKNOWN never becomes PASS
by defaulting.** That sentence is already in the doctrine, at the end of the G-6xx
gate list, and it is exactly right. Two additions:

- **No "or" between a real environment and a simulated one.** "Testnet/devnet **or**
  fixture harness" always resolves to the fixture. If the real thing is unavailable
  the gate is UNRUN, which is a kind of UNKNOWN.
- **No gate passes on data the project invented.** The sprint's first rule, never
  written into the specification.

---

## The fifteen, in one table

| | Gate | FAILs when |
|---|---|---|
| **D1** | Coverage, measured from outside | coverage can only be computed from the recorder's own counters |
| **D2** | Every counter rebuilt from rows | a published counter cannot be recomputed by grouping raw rows |
| **D3** | Time comes from the row, not the file | any split or cohort is assigned by filename |
| **D4** | Cross-tabulate every filter | the rows a filter drops score better than the rows it keeps |
| **D5** | Corrupt the record field by field | a field behind a headline number has no check pointed at it |
| **D6** | Two roads to every number | a load-bearing number has one author and one code path |
| **T1** | Price the toll | the round trip costs more than the move you are chasing |
| **T2** | The floor — buy everything | naive net expectancy is below break-even and the interval excludes it |
| **T3** | The ceiling at zero cost | the best rule loses money with every cost set to exactly zero |
| **T4** | The noise control | the best real rule is inside the noise of the same grid on scrambled data |
| **T5** | The untouched holdout, paired | the paired difference includes zero, or the level is below break-even |
| **T6** | At the size you can trade | it is only positive at a size you do not have |
| **L1** | Shadow, with outside coverage | a zero-trade period is not decomposed, or has no *not running* category |
| **L2** | Paper, and it has to be ahead | the paper account is not ahead at the end |
| **L3** | Micro-capital | realised expectancy is negative, or the real fill misses the modelled one |

---

## The data gates — is the corpus about the world?

Cheap, minutes each, and every one caught a real defect this sprint. Run all six
before any thesis gate is believed.

### D1 — Coverage, measured from outside

**Input:** for the window under test, an independent count of what should have
arrived — the public chain archive is free and goes back to at least August 2024 —
plus the process records showing when the recorder was actually running.

**Output:** one number per hour: *events received ÷ events that existed*. Plus
wall-clock listening time per day.

**FAIL:** coverage cannot be computed without using a counter the recorder wrote
about itself; or any window under a declared floor is used in a headline number
without being labelled with its coverage.

**Skipped if:** every uptime figure traces back to a row the recorder wrote. A launch
never received cannot be counted, and the gap between a stop and the next start is
invisible from inside. STS answered "how much are we missing?" three times — **40%,
then 0–2.5%, then 90%** — all from inside the capture. From the chain: **137 of 137
launches caught, zero drops, 3–72% uptime.** *08-21 is not a day, it is 48 minutes
spread over ten hours.*

### D2 — Every counter rebuilt from rows

**Input:** the raw rows, and every counter that appears in any report.

**Output:** each counter recomputed by grouping the rows, next to the counter as
published, with the difference.

**FAIL:** any published counter cannot be reconstructed from rows; or any field is a
constant written on every row rather than a measurement.

**Skipped if:** pick three counters at random and recompute them; if any differs,
the gate did not run. STS wrote `follow: 60` on every row regardless of how long it
had really watched, counted failed transactions without keeping one, and produced an
on-chain failure rate wrong by **274 times** from its own broken counter.

### D3 — Time comes from the row, not the file

**Input:** every record, its own timestamp, and the file it sits in.

**Output:** the count of rows whose timestamp falls outside the day its filename
names.

**FAIL:** any split, cohort, day bucket or train/test assignment is made by
filename; or the count above is non-zero and unexplained.

**Skipped if:** the count is not reported at all. The recorder rotates files on
write time, not event time, so a session that runs past midnight puts real rows in
the wrong day. Five coins sat on the wrong side of a day boundary for the whole
sprint; one analysis reported one misfiled launch where there were six.

### D4 — Cross-tabulate every filter against the thing you measure

**Input:** every filter in the pipeline, and the outcome variable.

**Output:** one row per filter — rows dropped, rows kept, mean outcome of each.

**FAIL:** the dropped rows score systematically better or worse than the kept rows
and the filter is applied anyway without a stated mechanism for why the difference
is not the effect you are looking for.

**Skipped if:** the drop table does not exist, or the population at the top of the
report does not equal raw rows minus the sum of the drop column. **The cheapest gate
here and the most valuable.** Every pass in the sprint dropped rows whose
price array was full, on the reasonable belief that a full array meant a truncated
record. It was a hard cap, not truncation. The rule deleted **half of every genuine
5x in the corpus** — 69 coins at a median peak of 3.14x — and inflated the measured
value of exiting early by 1.87 points, a quarter of the entire gap to break-even. It looked exactly
like good practice, all night, to everyone. The cross-tab ends it in one line:
**dropped rows had a mean peak of 3.58, kept rows 1.24.**

### D5 — Corrupt a good record, field by field, and count what nothing notices

**Input:** one record known to be sound, and the checker.

**Output:** for each field, whether corrupting it is caught. A count, and the list of
fields with no check pointed at them, by name.

**FAIL:** any field used in a headline number has no check pointed at it.

**Skipped if:** the report does not name both numbers — fields corrupted, fields
caught. Run on STS's checker: **17 of 31 corruptions passed unnoticed**, and the two
fields added that same night — the curve state behind the entry price, and what the
trading fee actually cost — had no check at all. Neither author was careless. Each
checked what they had built and **nobody checked the seam**; two files joined on a
signature used one name, `feeSol`, for two different quantities. Same shape as the
seam between the engine and the market, at a scale of hours instead of months.

### D6 — Two roads to every number

**Input:** any number that a decision rests on.

**Output:** the same quantity computed by a second person, in different code,
ideally by a different route to the same thing — and the two values side by side.

**FAIL:** a load-bearing number appears in a decision document with one author and
one code path.

**Skipped if:** the report says "reviewed" rather than "reproduced". About fifteen
load-bearing numbers were wrong across one night of this sprint. **Every one was
caught by somebody recomputing it. Not one was caught by anybody re-reading the
report.** Reading finds typos; only re-deriving finds a wrong number stated
confidently in a well-written paragraph, and all fifteen were.

---

## The thesis gates — does the trade pay?

Six gates, in this order. **T1 and T2 take an afternoon and cost nothing** — no
capture, no Rust, no recorder, no roadmap. The free public archive serves full
blocks back to at least August 2024.

### T1 — Price the toll

**Input:** a few hundred real landed buys and sells on the venue, at the size you
intend to trade.

**Output:** the round-trip cost as a distribution, not a point — and from it, the
gross return you must clear to break even.

**FAIL:** the toll is larger than the move you are trying to catch. Also FAIL, on
evidence, if the number rests on a quoted default, a document, a front-end's preset,
a percentile reported as a median, or fewer observations than the effect you are
chasing needs.

**Skipped if:** the cost figure has no sample size attached. STS's landing cost was
**1,005,000 lamports — a p90 read as a median off 25 samples, and it was a retail
front-end's default preset** — and it was used to argue the whole project was
infeasible. Measured properly: 95 basis points a side on 4,918 real sells, so
break-even is **+2.12% gross at 0.05 SOL**, not the 2.22% earlier passes used. The
first number in the chain was wrong twice, in both directions.

### T2 — The floor: does the naive trade pay at all?

**Input:** every launch in a real window — no filters, no candidates, no scoring —
with T1's costs applied, and the outcome definitions (win, loss, rug, unresolved)
committed before the run, because this is the first gate that scores an outcome.
Named, so that two people running it get the same number:

- **Source.** The free Helius mainnet RPC, `getBlock` with `transactionDetails:
  full`. It serves complete blocks with logs back to at least **2024-08-06** (slot
  281,993,102) — earlier than pump.fun had meaningful volume — for €0. Sweep
  contiguous runs of blocks. Do not page `getSignaturesForAddress` backwards: one
  `getBlock` returns every pump transaction in that slot and costs about **143x
  fewer calls** — the often-quoted 17x is W25's own estimate, and W48 measured it as
  understating its case by about eightfold. A window covering S seconds gives every launch in its first S−60
  seconds a complete outcome inside the same calls. The seven windows already swept
  are on disk at `~/Code/flux/data/history/hist-YYYY-MM-DD-18-00.jsonl` — one JSON
  record a line, `k` is `launch`, `trade` or `complete`, `src` is `"hist"`, chain
  time in seconds is `ct`.
- **Venue and program.** pump.fun's bonding curve, program
  `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P` (`tools/capture/src/pump.js:19`). A
  launch is a `CreateEvent` in that program's logs, a trade a `TradeEvent`. Price is
  the curve's own spot, `vsol / vtok / 1e3` (`spotPriceSol`, same file), read off the
  reserves a trade left behind — post-trade state, which is what an order landing
  then would actually hit.
- **Windows.** Seven: the third Wednesday of the month at **18:00:00 UTC**, October
  2024 to August 2026. The same hour every time, and that is not decoration —
  expectancy swings **10.4 points between adjacent hours**, so an unmatched sample
  drowns the thing being measured. Check the match held rather than assuming it: the
  seven start between **18:00:02 and 18:01:30 UTC**, all Wednesdays, all inside hour
  18. Read every block in the interval and report how many you missed; all seven
  report `missing: 0`. Each is 5–8 minutes of chain — 305 to 513 seconds, 658 to
  1300 blocks. Floor: at least five windows spread over a year or more, at least 100
  scored launches in each. That floor is a choice, not a measurement. It is there so
  no one window can carry the verdict.
- **Entry and exit.** Buy at the last trade at or before **launch + 2 seconds**, and
  pay the price that trade left. Sell at the last trade at or before **entry + 60
  seconds**. Write both numbers down before you run it. *Entry + 60*, not *second
  60*: a first pass that exited at an absolute second having entered at a later one
  returned a confident **+46.64%** and nothing about the output looked wrong. Block
  times are whole seconds and eight trades routinely share one, so "exactly three
  seconds" is not a runnable instruction — name the second and take the last trade
  inside it. (The two runs of this gate used two seconds and three; the gap between
  them was never measured.)
- **Costs, inherited from T1.** 95 basis points a side, applied as
  `(1 − 0.0095)² × exit ÷ entry − 1`, plus a flat **0.0001 SOL** network cost, at an
  order size of **0.05 SOL** — break-even **+2.12% gross**. Nothing for own-order
  impact, priority fees or tips; all three make the answer worse, none better. In
  the two earliest windows the `TradeEvent` carries **no fee field at all**, so 95
  bps there is assumed rather than measured. Label those windows. Do not quietly
  average them in.

**Output:** mean net return per trade, per window and pooled, each with a 95%
interval and **the method that produced it named**. The mean alone is the fragile
statistic here, because this corpus's defining feature is a long right tail — D4's
dropped rows peak at **3.58** against kept rows at about **1.25** (this document says 1.24, the run says 1.25 and an independent rebuild 1.27 — the ratio is the point, not the third digit) — so print the median
and the share of launches that lose money beside it. (Both were reported: medians
**−1.9% to −14.1%**, win rates **5.8% to 14.2%** — from the run that entered at three
seconds, not the two-second run the rest of this specification describes. Nobody has
measured the gap between them.) Plus the population count and
everything that did not reach it: launches with less than 60 seconds of window left
after them — **203 of 1,283**, 16%, dropped, with their bias checked at a shorter
horizon (kept **−5.74%**, dropped **−4.26%**). Note that this does not close: 1,283
minus 203 is 1,080, and the pooled n is 1,034. **Forty-six launches are unaccounted
for in the only run of this gate**, which is exactly why the ledger is required rather
than a single population count — and coins whose price moved without
the SOL flow to pay for it, which the curve's own arithmetic finds at **0% to 9.4%**
a window, worst in 2026. Removing those cut August 2026 from −10.9% to −14.7%: the
correction runs against you, not for you. Count launches per distinct deployer.
Repeated launches from one creator are not independent draws.

**FAIL:** the **pooled** mean is below break-even and the pooled interval excludes
it. Pooled, decided in advance and not after seeing the windows — six of the seven
exclude break-even on their own and the seventh, 2026-08-19, is wide enough not to
(**[−28.37, +7.87]**), and one wide window is otherwise enough for anyone to argue
the gate did not fail. A FAIL here does not end the project, but it does two things
immediately: it forbids any spend on execution, tips, latency or infrastructure
until T3 and T4 pass, and it converts the work from *build the machine* to *find the
subset*.

**Skipped if:** the result is quoted gross, or from one window, or without the
population count, or with the holding time missing. That last one is not pedantry:
the same run is **−1.38%** at a 5-second hold and **−10.32%** at 60, and the 5-second
number satisfies every other clause while cutting the reported loss by **seven
eighths** — from −10.32% to −1.38%. **This is
the gate that would have failed first.** Buy every launch, hold, sell at 60 seconds:
**−6.5% to −17.1% in every one of seven hour-matched windows from October 2024 to
August 2026, mean −10.1%** — recomputed independently by a second person in
different code as **−10.32% pooled, 95% interval [−13.00, −7.63], n = 1,034** —
against a +2.12% break-even, with no trend over time and none against rival count.
In October 2024, with only 2.7 rival buyers in the first three seconds, the trade
still lost 7%. **No edge was competed away. There was never an edge.** The whole
thing is seven files, **50 MB**, 106,719 records; two python3 scripts of 40 and 45
lines, standard library only; **1.2 seconds of compute for the pair, 0.35 of it T2**,
and about 25 minutes of wall clock, most of it spent working out the record shape. One afternoon, zero
euros, week one.

### T3 — The ceiling, with all costs set to zero

**Input:** a pre-declared grid of realisable rules — entries, exits, stops, holds —
on real price paths, with fees, network costs, slippage and own-order impact **all
set to zero**.

**Output:** the best rule in the grid and its return.

**FAIL:** the best rule is still negative. Nothing on the engineering side can fix
this, and that is the point of running it early: it closes cheaper fees, a faster
machine, private bundles and a bigger bankroll in one line each.

**Skipped if:** the grid was not written down before it was run, or costs were
"low" rather than exactly zero. Run on STS: **−0.86% at a cost of exactly zero**,
and −1.17% on the cleaner corpus. The best rule enters at second 30, so paying for
speed buys nothing.

### T4 — The noise control, and the count of things tried

**Input:** the same grid as T3, run a second time against outcomes whose time order
has been scrambled; and a written count of every rule, filter and parameter set ever
tested, with the date each was chosen.

**Output:** the best real result, the best scrambled result, and the gap between
them, next to the number of rules tried.

**FAIL:** the gap between the best real rule and the best scrambled rule is inside
the noise of the grid; or the best result is not better than what trying that many
rules would produce by chance.

**Skipped if:** the count of rules tried is missing, or is 1. This is AL.1 with a
gate under it. Run on STS: the best of 108 exit rules on real paths loses **2.32%**;
the same grid on scrambled seconds gives **−2.75% to −3.21%**. So every real pattern
in how these coins move — all of it — is worth **0.4 to 0.9 percentage points**,
against a gap to break-even of seven to eight.

### T5 — The untouched holdout, scored as a paired difference

**Input:** a test set frozen and hashed **before** any parameter was chosen, assigned
by row timestamp, with a stated time separation between the last outcome resolving
in training and the first launch in test.

**Output:** the rule's net return and, more importantly, the **paired** difference
between the rule and buying everything inside the same window — which is three to
four times quieter than either level and is the number that says whether the signal
carries information.

**FAIL:** the paired difference includes zero; or the level is below break-even even
when the difference is positive — beating a losing baseline by five points while
still losing is not a pass.

**Skipped if:** the test set's freeze date is later than the selection date of any
parameter; or the exit rule is named without saying **both** what the running peak
tracks and what the stop tests against. Those two choices spread the same "5%
trailing stop" across six points of return — most of the whole seven-to-eight-point gap to break-even
— and three passes each silently picked a different one. **A fill priced at the level
that triggered it is not a fill.** There is no resting stop order here; all four
constructions are positive priced at the stop level and negative priced where the
stop actually fires.

### T6 — At the size you can actually trade

**Input:** the surviving rule, priced at the bankroll's real order size, including
own-order impact, and the capacity curve across sizes.

**Output:** net return per trade at your size, and the size at which it turns
positive if it ever does.

**FAIL:** the result is positive only at a size you do not have.

**Skipped if:** any headline number is quoted without a size attached. This is AL.5,
and it is the gate that changes what STS should have built. The launch block is the
only positive entry window in the market — **+9.5%, declining monotonically after it**
— and the median same-block buyer at 0.05–0.5 SOL returns **−1.90%**, the round-trip
toll and nothing else. It turns positive at 5 SOL, five times the bankroll. The
exclusion of the launch block was right and its stated reason — an unwinnable speed
race — was wrong, and **the wrong reason built the machine**: the dual-speed
pipeline, the ten-millisecond budget, the tip ladder and the garbage-collection
defence all exist to win a race that was never the constraint. Size was.

---

## The live gates

These replace 6B, 6C and 6D. The changes are small and each one is a FAIL condition
the originals did not have.

### L1 — Shadow, with coverage from outside

**Input:** a live run of stated length, plus D1's external count for the same window.

**Output:** coverage, latency bands, and a decomposition of every hour that produced
no trade.

**FAIL:** any zero-trade period is not decomposed; or the decomposition has no
category for *the process was not running*, which only an outside count can supply.

**Skipped if:** the decomposition adds up only to causes the running process can see.
§14 already requires this and Gate 6B already wires it in — the one place the old
ladder did this right. The missing half is that a zero-trade period caused by nobody
starting the recorder produces no alert at all.

### L2 — Paper, and it has to be ahead

**Input:** live snapshots, modelled fills, the full period.

**Output:** the paper account's net position at the end, and the live-versus-paper
slippage residual per cohort.

**FAIL:** the account is not ahead at the end. Also FAIL if any modelled fill has
zero slippage, or if the residual breaches its control limits for two consecutive
cohorts.

**Skipped if:** the acceptance list is all properties — deterministic, reconciled, no
divergence from replay — and no number to beat. As written, Gate 6C passes on
**fourteen consecutive days of losing money**, and it is the last gate before real
euros. It measured fidelity to the model, and the model is the thing on trial.

### L3 — Micro-capital, and it stays the last gate

**Input:** the smallest real position that produces a real fill.

**Output:** realised expectancy after realised costs, with realised compared against
what the simulator predicted for the same fill.

**FAIL:** expectancy is negative, or the realised fill diverges from the modelled one
beyond a tolerance declared in advance.

**Skipped if:** this is the first gate in the ladder that could have returned FAIL.
That was the old ladder's actual shape: 6D was item 39 of 40, and it sits after
mainnet authorisation.

---

## Where the existing methodology attaches

The doctrine's controls are good. Which gate enforces each, and — the part that
matters most, because an unenforceable requirement is the failure being corrected —
how you would tell it had been skipped.

| Control | Enforced by | You can tell it was skipped when |
|---|---|---|
| **AL.1** — multiple-testing correction; *"reserve a final untouched test set"* | **T4** (count of rules tried, scrambled control) and **T5** (the frozen set) | The count of rules tried is absent or is 1. Or the test set's freeze date is later than the selection date of any parameter — both dates are recorded, so this is a comparison, not a judgement. |
| **AL.2** — purge and embargo | **T5** | The split does not state the time separation between the last training outcome resolving and the first test launch. If the number is missing, the split was made by file, not by clock. |
| **AL.3** — *"a zero-slippage fill is invalid"*; live-versus-paper residual | **T5** offline, **L2** live | Any result where the fill price equals the decision price. Any exit rule named without both of its two choices — what the peak tracks, what the stop tests against. Any residual not reported per cohort. |
| **AL.4** — selection and survivorship; results with and without exclusions, never mixed | **D4** and **T2** | The drop table is missing, or the population at the top of the report does not reconcile to raw rows minus drops. Rows left silently is the whole failure mode. |
| **AL.5** — economic significance; capacity curve | **T6** | A headline number with no size attached to it. |
| **§14** — zero-trade decomposition | **L1** | The decomposition's categories are all things the running process can observe. No category for *not running* means D1 was never wired in. |
| **Annex W** — outcomes defined before evaluation; unresolved never silently excluded; deployer clustering | **T2**, the first gate that scores an outcome; its definitions must be committed before it runs | The definitions file's last commit is later than the first result that used it — a date comparison, not a judgement. Unresolved outcomes do not appear in the denominator with a count. Repeated launches from one deployer are counted as independent draws. |
| **Phase 3 criterion 2** — leakage, survivorship, selection bias and time-split violations *fail the run* | **T5** — unchanged, but on real data | It is the best clause in the old ladder and it is vacuous on a fixture, because the generator decides what the future contains. On real data it has teeth. Skipped if the run it guards is a fixture run. |
| **"UNKNOWN never becomes PASS"** | every gate | Any gate reported as PASS whose input was a stand-in for something real. |

---

## What this would have done

Same project, same people, same care, this ladder instead:

- **Week one, one afternoon, zero euros.** T1 prices the toll at +2.12%. T2 buys
  everything across seven windows and returns −10.1%. **T2 FAILS**, and execution,
  tips, latency work and the ten-millisecond budget freeze at that moment.
- **Week two.** T3 sets every cost to zero and the best rule still loses 0.86%. T4
  scrambles the seconds and finds all the real structure is worth 0.4 to 0.9 points.
  **Both FAIL.** The question is settled, and it cost two weeks.
- **Nothing after that gets built** — the entire return on the exercise.

The old ladder reached its first thesis gate in month ten, and by then the calendar
split everyone had been treating as a holdout turned out not to be one — 21 August is
the tail of the 20 August run past midnight, one recorder process across the boundary.
A genuine holdout does exist, 08-16, and nobody used it; it is also narrower than it
looks, valid for the opening features and the outcome label and not for anything built
on `funding` or `tracks`. Either way the count of *runnable* pre-capital thesis gates
on that corpus at the moment it mattered was zero, not one.

**Fifteen gates. Six run on the data, six can fail on the market, three can fail on
reality.** One caution about that first six, by this document's own rule at the top:
a gate must be able to come back FAIL for a reason that is not your fault, and five
of the six D gates can only fail because we wrote a bug. **Only D4 — cross-tabulate
what your filter drops — can fail on a fact about the world.** The D gates are
engineering gates *about the corpus*, and they run first for the reason this document
already gives: a thesis gate on an untrustworthy corpus returns a confident number.
That is a good reason. It is not the same as six world-facing checks.

** T2 fails first, in week one, on a few hours of free data.**

---

## This ladder has been run once

Written after the fact, and then executed the same night, by somebody who had not
written it (`docs/sprint-2026-08-27/W79-gates-run.md`). That matters more than
anything else in this document: an acceptance ladder nobody has run is a proposal.

**Eleven of the fifteen are runnable today. Two pass, nine fail, one is defective,
three need a live system that does not exist.** Total compute for all eleven: under
25 seconds.

T2 failed first and cost **0.35 seconds**. The headline claim reproduces and is
understated. T3 then closed it — **0 of 207 pre-declared rules positive with every
cost set to exactly zero**.

But the run found something the document should be more worried about than its
verdict. **Twenty-four places needed a choice this document should have made, and
several of them decide PASS or FAIL.** The worst is in T2 and it is now named there:
"sell inside the minute" is not an exit rule, and the answer is −1.38% at a five
second hold against −10.32% at sixty. A nine point spread, against a 2.12% bar, on
an unstated word.

Two gates do not survive their own run:

- **T4's gap has the opposite sign.** This document says the real corpus beats a
  scrambled one by 0.4 to 0.9 points. Run on a 207-rule grid, the real corpus scores
  −0.51% and the scrambled ones **+0.61% to +1.34%** — the scramble wins by 1.86
  points. The conclusion is unchanged either way, because neither is exploitable, but
  "0.4 to 0.9 points of real structure" is not a measured quantity. Quote the
  direction.
- **T6 contradicts T3 and needs withdrawing and re-running.** It points at the
  opposite engineering decision from every other gate here, which is the one outcome
  a ladder must never produce quietly.

And the deepest finding, which applies to every gate in the table: **every skip test
checks that a number was reported, never that it was the right one.** Seven gates can
be skipped undetected today — including T2 itself, by reporting the five-second hold.
That is a weaker version of the exact failure this document was written to correct.

D6 — two roads to every number — **has never been run on this document.** Four of its
figures are already known not to reproduce.
