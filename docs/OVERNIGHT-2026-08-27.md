# What happened while you slept — 27 Aug 2026

**The answer is no. It is no for a simpler and much older reason than anyone
thought, and the reason is now measured rather than argued.**

When you went to bed the verdict said the trade was maybe half a point from
working. That was wrong. Overnight it went to five points short, then to seven or
eight — and then somebody went and looked at two years of history and found the
question was the wrong one. It was never nearly working. There was never an edge
to lose.

Everything is committed and pushed to `origin/main`. The full argument, with all
its methods and caveats, is `docs/VERDICT-2026-08-27.md` (revision 4), and a
companion, `docs/POSTMORTEM-2026-08-27.md`, asks the separate and more useful
question of how seventeen days of careful testing never once tested whether the trade
made money. This page is just the catch-up.

---

## The one sentence

**Buy a coin at launch, sell it inside the minute, and you lose about a tenth of
your money — and that was as true in October 2024 as it is today.**

Seven test windows, hour-matched so they are comparable — the third Wednesday of
each month at 18:00 UTC, October 2024 through August 2026 — each read straight
out of a run of whole blocks with nothing missing:

| | |
|---|---|
| every one of the seven windows | **−6.5% to −17.1%** per trade |
| average | **−10.1%** |
| worst window | **May 2025** — the middle of the period, not the end |

The loss does not track how busy the market was (correlation −0.20), or how many
rivals were buying in the first three seconds (−0.11), or the date (−0.33).

The line worth keeping: **in October 2024, when a launch drew only 2.7 other
buyers in the first three seconds, this trade lost 7%.**

So the story we had been telling ourselves — that there was an edge once and
faster bots competed it away — is false. Two other beliefs died with it. The
market did not collapse (43.9 launches a minute then, 37.6 now, about 1.6 million
a month at both ends), and the evidence was never out of reach: the free endpoint
serves full blocks back to August 2024 at no cost. Nobody had swept it.

## It loses before it costs anything

This is the finding that closes off every "what if we just…".

Take a grid of rules you could actually run — five entry times, three trailing
stops, three take-profit-and-stop pairs, plain holding — price the exits at what
you would really get rather than at the level you hoped for, and run it on 4,195
real coins. **Set every cost in the system to zero and the best rule still loses
0.86% a trade.** On the cleaned data it loses 1.17%.

Not after fees. *Before* them. Before pump.fun's cut, before the network, before
slippage.

Which rules out, in one line each, the four things anyone would reach for next:

- cheaper fees — the loss exists at a fee of exactly zero
- a faster machine — entering at second 30 beats entering at second 3, and
  paying more does not buy you an earlier slot (a paired test on 76,795
  same-launch pairs says the higher bid lands first 50.3% of the time — a coin
  flip)
- Jito bundles — they buy position in a race whose winners return less than the
  wallets that never enter it
- a bigger bankroll — the cost per trade is already negligible at 0.05 SOL

With perfect hindsight the same coins are worth **+12.1%** — and on the *median*
coin perfect hindsight pays **−1.89%**, the fee and nothing else, because the
typical launch never trades above its three-second price again. Where the
information exists it is real. Nothing you can decide at second three gets at it.
**That gap is the project.**

(The +18.9% this page used to quote was wrong twice over — struck a second late,
and inflated by the chart-manufacturing operator two sections down, whose 16% of
coins supplied 53% of it. The honest ceiling is +12.1%.)

## The middle of the market is worse, not better

Everything above is the launch window, and the README used to say STS was not for
that — it was for the *"difficult middle of the market: approximately $25k–$80k
market-cap bonding curves and Raydium consolidation"*. That is the obvious
defence, and it has been tested twice tonight, overturned each time, and then
settled for good by measuring the one thing both earlier attempts had to guess
at.

**Start with the part that needs no statistics at all.** Every pump.fun
graduation puts **exactly 206,900,000 tokens and 67.4059 SOL** into the new
pool — the same two numbers every time, read to the decimal out of 29 clean
pool-creation transactions. Divide them out and the new pool opens at about $55k
where the bonding curve's last price said $69k.

**Every coin loses 20.71% at the instant it graduates. Every single one, with no
trading involved** — pump.fun keeps about 17.6 of the 85 SOL. There is no sample
size here and no modelling choice: it is two constants divided by each other.

The number that decided this question was **9%**. A graduate needed to give back
only that much after migration for band entry to be worse than buying everything.
It gives back 20.71% before a single trade — and by twelve hours the **median
giveback is −99.64%** (n=65). **The median graduate is worth 0.36% of its
graduation price half a day later.**

So the earlier −6.53% gross / −8.43% net for band entry was not a thin loss, it
was an optimistic one: it assumed you sell at the graduation price, and that
price ceases to exist at the moment of migration.

**Re-run with an exit you could actually take** — hold through migration and sell
into the new pool — the band returns **−17.94% gross, −19.84% net** against
**−12.20% / −14.10%** for buying everything. **5.7 points worse**, which is
exactly where a 20.71% giveback says it should land. The arithmetic and the
measurement agree. **Buying the band is worse than buying everything, not
better.**

Three things finish it off:

- **Only 2.48% of launches ever graduate** — 99 out of a random 4,000. That is
  the base rate any "middle of the market" plan is working against, and it
  appears nowhere else in this project.
- **Winners do not keep.** Of 65 graduates, 5 were still above their graduation
  price at twelve hours, 3 at twenty-four, and **one today.** One of them touched
  a $29M market cap; its pool now holds 0.4 SOL.
- **You could not have sold anyway.** The median graduate's pool holds **1.37
  SOL** twelve hours in, so even the losing numbers above are flattering.

**The venue in the old thesis was wrong too.** Graduates do not go to Raydium.
They go to **PumpSwap** — 65 of 65 confirmed. So the "consolidation" half of that
sentence is no longer untestable and no longer untested. It has now been
measured, on the right venue, and it loses.

**One honest hole:** about **45% of graduations happen after our twelve-hour
observation window closes**, so the giveback measurements describe the fast half
of the population. That does not touch the conclusion — the 20.71% at migration
is arithmetic and applies to every graduate whenever it happens — but the
twelve-hour figures are a subset, not the whole.

The structural finding underneath is the durable part and it is worth one line:
**the "middle" is not a phase, it is a moment.** Median time from launch to $25k
is 89 seconds, and only 3 of 53 coins are still above $25k twelve hours later.
There is no consolidation base sitting there to be traded. What survives
regardless of the answer is the instrument problem: a real band strategy scans
the coins *already* sitting at $25k–$80k, and a launch listener cannot see those
at all.

**This is the cleanest example of the night's recurring mistake.** Two careful
passes argued about how to model an unmeasured quantity. Neither went and
measured it. It took one afternoon and it was exact.

## The fat tail was largely manufactured

This is the biggest thing that changed while you slept, and it took most of the
night to get right.

One operator, running one program, supplies **58% of every coin in our data that
doubled, 75% of the 3x, 81% of the 5x and 93% of the 10x.** On the held-out day
it supplies **86% of the doublers and 94% of everything above 3x** — 15 of 16.

Take its coins out — 17.3% of the corpus — and:

| | full corpus | cleaned |
|---|---|---|
| average per trade | −9.84% | **−7.76%** |
| chance of a 5x | 1 in 109 | **1 in 238** |
| biggest peak anywhere | 34.1x | **11.7x** |

Cleaned, the best *genuine* coin on the held-out day peaks at **3.12x**. It has 82
wallets, 156 trades and 36.4 SOL of real money in it against the 22.96 SOL its
peak required, so it is real by every test in the file.

**Those three cleaned figures moved after this page was first written, and they
moved by a lot.** Every pass that night threw away coins
whose recorded high-price list was full, on the belief that a full list meant a
truncated record. There was never anything to throw away — the list is simply
capped at sixty entries, nothing was ever frozen, and the check that would have
shown it takes a minute. What the rule actually deleted was **half of every
genuine 5x coin in the corpus.** Put them back and the cleaned average goes from
−9.85% to −7.76%, a 5x becomes **one coin in 238 rather than one in 471**, and
the best real coin on the held-out day goes from 2.71x to 3.12x. **The collapse
of the tail is real and survives. Its most vivid number was about twice as steep
as the truth.**

**What that counterparty was carrying was the variation, not the returns.** It is
17.3% of the coins and **82.2% of the whole sum of squares** — twenty-two times
the variance per coin — and it sits on both ends at once: 73.8% of the
worst-finishing tenth, 41.6% of the best, and only 1–8% of the middle.

**Do not read much into the two-point move in the average, in either direction.**
Day by day the shift runs from −1.5 points to +1.4 and happens to cancel, and on
the held-out day removing the operator makes the result *worse*. The claim that
holds is the one about variance.

So: the average trade was losing for reasons that have nothing to do with this
operator. **What dies when you remove it is the fat tail the whole case rested
on, not the verdict.** The reason this was always negative is now separable from
the reason it looked exciting.

How it works, briefly: the token side of the price is correct, but the quoted SOL
side is rescaled rather than moved by the trade, so the chart shows a curve
holding 95 SOL when it is holding 2.51. The prices really printed — this is not a
decoding bug, and we checked that four ways after first getting it wrong. They
are real prints on coins nobody's money could pay for and nobody could have sold
into. The corpus's single biggest winner, a 34x, took in 17.7766 SOL and paid out
17.7766 SOL. Exactly zero net money.

**And it deserves a fair description, because it is easy to write this
unfairly.** Over seven days it moved about 3,700 SOL in and 3,778 out — **+1.7%,
which is market-maker economics, not a robbery.** Buyers on its coins do slightly
*worse* at the median — 0.69 of their money back against 0.75 elsewhere — and
clearly *better* in the tail: about a third more get their money back and about a
third fewer are wiped out completely, because it is a two-sided counterparty that
will buy your bag when the coin is one second old. It sells a chart. It does not
appear to rob the audience.

Detecting it costs nothing and is now done at capture time. Two checks.

## Who does make money: the creator, and essentially nobody else

The most useful thing the night produced, and it is not about our strategy at
all.

On 5,241 launches with verified curve data, **creators put in 13,130 SOL and took
out 28.8% more; everyone else put in 62,235 SOL and lost 8.1%.** On the two
thirds of launches where the creator dumps everything inside sixty seconds, the
creator makes **+39.2% on the money staked**, with a 60% win rate, and it repeats
on every day in the data including the held-out one.

**Per 100 SOL that an ordinary buyer stakes, 1.7 goes to pump.fun and 6.5 goes to
the creator.** The platform is not the main drain. The steady bleed measured
everywhere else in this project is not friction — it is somebody's income.

**And the edge is the seat, not the person.** When those same creator wallets buy
somebody else's coin, they lose 4.03%. Skill is visible in this market and it is
worth nothing: past non-creator winners keep winning more often than chance, at
exactly zero expectancy. A persistent style, not a persistent profit.

## The thing about the engine

**Nothing in the Rust crate has ever read a real capture.** No file under
`src-tauri/` opens `coins-*.jsonl` or `tracks-*.jsonl`. The fixture format
`sts.replay.v1` has zero files on disk anywhere. `fixtures.rs` opens with the
words "launches that never happened".

So "1,654 tests pass" means the engine agrees with itself. Every number in the
verdict came from Python written during the night, straight against the raw
files, going around the engine entirely. That is also why three missing pieces
went unnoticed for months: there is no exit rule anywhere in the crate, paper
mode is never constructed outside a test, and there is no entry-side transaction
builder.

`docs/SALVAGE.md` says what is worth keeping and what is not. Nothing has been
deleted.

## There is a real tail. It is just not decidable in advance.

The section above is the strongest negative in the whole document and it should
not be read as more than it is, because the same check turned up something nobody
was looking for:

> **86 coins peaked above 3x with fifty or more different wallets in them. Not
> one of those 86 is manufactured.**

There is a real tail. It is small — 0.7% of everything captured — and the thing
that separates it from the fake one is not any clever price test. It is **how
many different wallets were there**, which is free and available at capture time
and cleaner than anything else tried all night.

It does not rescue the strategy, and the reason is worth one line: **a genuine 3x
is a forty-second climb, not a three-second spike.** The typical one has 175
wallets and 394 trades in it and peaks at second 39, and **not one of the 86
peaked at or before second three.** The information arrives around second 20 and
you have to commit at second 3.

A follow-up went looking for them at second three anyway, across a 433-cell grid:
the best selector makes +1.99% before costs and +0.09% after, sits at the 29th
percentile of its own random baseline, and loses on the held-out day. Half its
profit is one coin. Inside any bucket you can define
at second three, the coins that go on to run had *fewer* early buyers, *less*
early money and *fewer* early trades than the ones that do not. Every early
signal points the wrong way.

But you should not come away thinking every big move in this market is fake. They
are not.

## The last lead closed, and it closed in an interesting way

One rule — built only out of things you can see at the moment you have to
decide — survived every test the night could throw at it. It is worth saying how
hard it was tested before saying what it turned out to be:

| test | result | rebuilt from scratch |
|---|---|---|
| leave out one session at a time | −8.35% → **−3.43%**, 4 sessions out of 4 | −8.41% → −3.65%, 4 of 4 |
| fit on the past, score the future | −6.64% → **−2.60%** | −6.64% → −1.84% |
| against shuffled labels | **99.5th percentile** | reproduced |

It was then rebuilt from nothing by a second hand, on a separately constructed
grid, and came out in the same place. Nothing else all night survived that much
checking. **It was not a validation failure. It passed everything.**

**And it is a dead-coin detector.** What it picks has a median of **one buyer,
one trade and 0.01 SOL**; the median pick records no candles at all after entry;
and **69% of them return exactly −1.89% — the round-trip fee and nothing else.**
It is not finding winners. It is buying silence: on a bonding curve, a coin
nobody trades hands your money back minus the fee, and that beats the average
because the average coin falls.

The mechanism was then confirmed by turning the dial both ways. Tighten it and
the share of dead coins and the return improve together. **Ask it instead to find
coins whose fitted return is positive — actual winners — and the dead share drops
to 5% and the result collapses to −8.83%.** Every point of improvement is bought
with a dead coin.

Which puts a hard ceiling on it. **Its asymptote is minus the fee.** The best of
thirteen exit rules on top of it makes +0.09% before costs and **−1.81% after.
Not trading at all beats it by 1.8 points.**

**And the epitaph needs no number at all.** The thing that would make this work —
knowing whether a coin is still alive a few seconds in — is not available at the
moment you have to commit. Compare each coin only against others that traded
about as much as it did, and roughly **seventy percent of the whole effect turns
out to be the features predicting whether the coin trades again at all.** That is
not a flaw in the test. That is the finding.

With that one closed, **nothing under test reaches break-even** — though "closed"
turned out to be too strong for the exit work. Re-run without a bad hygiene filter,
flow and sell-side timing both carry real information about when to leave: about a
point better than a stopwatch at short holds, three and a half at long ones. Not
enough to trade, and not nothing.

---

## What got fixed

This is the good news and it is real work.

- **Your recorder was living in a git stash.** The program that wrote every
  capture we have been analysing existed on no branch, one garbage collection
  from gone. It is now maintained at `tools/capture/` on `main`, preserved under
  tag `capture-producer`, and has gone from **15 tests to 285**. (The first
  attempt to preserve it saved the wrong commit — a half-staged snapshot that
  does not even parse — and another pass caught it.)
- **It now records the state behind the price, not the price.** The old recorder
  stored only derived open/high/low/close. The four numbers that would have let
  us repair a fifth of this dataset were being read off the wire correctly on
  every single trade and simply never written out — they were in a variable at
  the moment the row was saved. Cost of the fix: **385 extra bytes on a 4.35 KB
  record.** That was always the entire price. Record raw state, not derived
  state.
- **It also logs its failures, proves its own uptime, and carries a `capture
  check` command** that finds this class of defect by itself. Pointed at the
  existing data it immediately flagged 2,165 impossible rows without being told
  what to look for.
- **`flux stats` had been claiming 100% uptime for a listener that ran 0.41% of
  the span.** That is why nobody knew the captures were full of holes.
- **And the recorder itself was exonerated.** Checked launch for launch against
  real chain blocks rather than inferred from inside the capture, it caught **137
  of 137** — zero drops. **The captures are complete while they are connected.
  They are just short.** The sharpest way to say what that costs: **08-21 is not
  a day. It is 48 minutes spread over ten hours.** Every sentence in the verdict
  about a "day" is really about a handful of connected minutes scattered through
  it, at different hours from every other day — which is this dataset's single
  biggest source of false findings, and produced two of them tonight.
- **One thing about the data looked like it might go our way. It did not.** Every
  pass worked from a brief saying 08-21 is not a real held-out day but the tail of
  the 08-20 run running past midnight. A rebuild reported a 100-minute break before
  it starts, which would have made it a genuine holdout. The whole discrepancy was
  **one coin** — RAMEN is both the last row of the 08-20 file and the first coin of
  the session that runs into 08-21, so one pass measured the gap before it and the
  other the gap after it. On launch time the day-line gap is **eight minutes**, and
  the listener's audit log shows **one process running 14.98 hours straight across
  it.** 08-21 is not independent, and it is the worst day in the corpus to validate
  on: **46.8% of its clean coins are burst-truncated** against 7.7% on 08-16, and
  hours 01–08 hold three usable coins.

  What is true is narrower. **"There is no valid holdout" was wrong** — 08-16
  qualifies, behind 3.78 days of total silence and five distinct recorder
  processes — and that test has already been run: the one selection result that
  survives every null scores **−7.09% → −3.77%** on it. **So no finding became more
  favourable. One negative finding became more credible.** That is the honest shape
  of the only correction all night that could have gone the other way.
- **flux is under version control for the first time** — with real history, not a
  snapshot. Tests 15 → 49.
- **The fee backfill you had abandoned finished**: 11,085 signatures resolved, 0
  missing. It replaced a 25-sample guess that had wrongly convinced us your €200
  was too small to trade. It is not — the median fee across the whole
  distribution is 55,000 lamports and being first in the launch slot costs a
  median 185,000, against the 1,005,000 we had been quoting, which turned out to
  be a sniper front-end's "high priority" preset.
- **Phase 0's gate passes** — clippy clean and `cargo fmt` run for the first time
  ever, 1,654 tests green, no new lint suppressions. It is the only gate in the
  roadmap that has ever closed.
- **The repo went from 33 branches to 12 and 21 worktrees to 10.** Everything
  deleted was tagged first, and **nothing of substance was removed** — the whole
  tree is at tag `pre-salvage-2026-08-27`.

Also deliberate: the listener was **not** started, no money was spent, and the
supervisor is stopped on purpose — it dispatches roadmap work and would have
driven straight past the closed gate.

---

## The one thing I would want you to take from the night

**Fifteen findings that other things rested on turned out to be wrong, and three
of them had already been written up as settled in documents that were published.**
(That was the count at the time. The table has since grown to **thirty** —
count it, do not trust this sentence.)
(That is the row count of the *Do not quote these numbers* table in
`docs/sprint-2026-08-27/INDEX.md`, which is the register of record — count it
rather than trusting this sentence. Most are numbers; a few are claims that were
wrong without being numbers.)

The ones that mattered most. The half-point-from-break-even headline. The claim
that the market had collapsed from 1.7 million launches a month to 2,851 — off
by about 500x, and it was our
own listener's output mistaken for the market. The drop rate, which was stated as
40%, then 0–2.5%, then 90%, and is really **nothing**: checked block-by-block
against the chain the recorder caught 137 of 137 launches, and the 90% was the
duty-cycle hole counted a second time under a different name. A 93.2% on-chain
failure rate that is really 11.3% and was our own broken counter. A +18.9%
ceiling that is really +12.1%, struck a second late and contaminated. A p90 read
as a median. A table published with the one row removed that broke its claim. The
$25k–$80k band, wrong twice: −62% first, then −6.5% once graduated coins were
handled the same way on both sides, and now worse than buying everything once
somebody finally measured what a graduation does to the price. And this sprint's
own hygiene rule, which was quietly a selection rule and cost us half the real
tail.

**Every single one of them was caught by a second pass going and recomputing the
number. Not one was caught by anybody re-reading the document.** Careful reading
found nothing at all. That is the most transferable thing this sprint produced,
and it is worth more than the loss-per-trade number.

**And the filter mistake produced the single most reusable thing of the night**,
which is a one-minute check anybody can run: **cross-tabulate what a filter drops
against the thing you are measuring. If the rows it drops score higher than the
rows it keeps — 3.58 against 1.24 here — it is a selection rule, not hygiene.**
That test would have caught in sixty seconds a rule that at least ten reports applied all
night without re-deriving it.

(Every retired number and what replaced it is now tabulated in
`docs/sprint-2026-08-27/INDEX.md`, under *Do not quote these numbers*. If you find
a figure in this repository that looks wrong, check it there first.)

The other half of it: **every one of those errors ran toward a tidier story.**
None of them changed the sign of anything, and correcting all of them made the
"no" stronger rather than weaker. Reassuring, and also a warning — a document
that keeps needing to be made *less* flattering is being written by people who
want a result.

**And the engineering was not what failed.** The curve maths in `replay.rs` is
exact — it predicts the real token amount on 5,927 of 5,959 real trades — and it
is the only component in this project that has ever been checked against reality.
The walk-forward harness had independently implemented the exact leakage controls
the analysis only later discovered it needed. The catalogue of mistakes above was
made *outside* those files, in scripts that hurriedly reimplemented what the
engine already had right. The analysis would have been better if it had gone
through the engine instead of around it.

---

## Three things that need you

1. **The captures are irreplaceable and backed up nowhere.** They live in
   `STS/data/` and `~/Code/flux/data/`, both gitignored, about **170 MB
   together** — small enough that this is not a storage problem, only a decision
   nobody has made. This data is not for sale and every hour not recorded is a
   permanent hole. **It is the only irreversible item on this list**, so rather
   than leave you a blank page: my default would be one dated `tar` of both
   directories, copied to a second physical place, today — a one-command job.
   Nothing more elaborate is warranted at this size, and nothing at all is what
   we have now. Say the word and it is done; where it goes is your call, not
   mine.
2. **`docs/SALVAGE.md` is a proposal, not something that has happened.** Nothing
   has been deleted and nothing will be without you. It sorts the crate into a
   **keep pile of about 9,000 lines** — `replay.rs` above all, the one component
   ever checked against reality — and **about 42,600 lines of Rust to bin, plus roughly 12,000 of Tauri shell and `ui/`**, and it
   asks you to do those in that order: extract the keep pile first while the tree
   still builds (priced at under two days), then decide the bin separately, as
   one reviewed commit. The whole pre-salvage tree is recoverable by name at tag
   `pre-salvage-2026-08-27`.
3. **Whether to capture fresh sessions at all.** The recorder is fixed and ready.
   Your standing position is that the listener does not run continuously, and
   changing that is yours to decide. What it would buy: the recorder does not
   drop launches, so the only defect left in the data is that it was rarely
   running — and **150 coins per comparison cell needs about four hours of
   connected time**, which is the difference between a real held-out day and
   another 48 minutes smeared over ten hours.
