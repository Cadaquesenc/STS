# Why nobody found out sooner

**A postmortem on how STS was tested — 27 August 2026**

`VERDICT-2026-08-27.md` answers the market question: STS has no edge, and a
two-year sweep says it never had one. This document answers a different question,
and it is the one worth keeping. **A careful project spent seventeen days building a
trading system and never once tested whether the trade made money.** Not because
anybody skipped a step. Because of how the steps were written.

The engineering here is good. That is what makes it interesting.

---

## The short version

- The plan had **40 acceptance criteria and promotion gates.** Three of them
  require the system to be right about the market — and **two of the three are the
  same test**, because Gate 6A restates Phase 3 criterion 5. So: **37 items and 3,
  or 38 tests and 2.** Both are honest. Mixing them is not.
- One of the two distinct tests happens only **after real money is on the chain.**
- So there was **one gate, in the whole project, that could have told you the strategy
  was wrong before you paid to find out.** It was reached in the last week of
  August 2026. It failed.
- **The other 37 items can all be passed without the system ever being right about
  the market.** Thirty-three need no network at all; four need a live stream but
  measure the machine rather than the thesis.
- The thesis gate is the **23rd item of 40.** The next one is item **39**, after
  mainnet is authorised.
- A separate rule in the doctrine **forbade the one cheap check that would have
  settled the whole question** — a sweep of market history — on the grounds that it
  cost money. It was free. When somebody finally ignored the rule, one afternoon
  produced the finding that governs the entire verdict.

---

## One sentence explains most of it

From the specification's own directive log:

> "09:29 AM UTC — Ethan requested a final recursive self-debunking loop to make the
> STS specification **essentially undebunkable and consistently profitable**."
> — `STS_CORE_IDEOLOGY.md` §00 directive log, item 4

Those two words pull in opposite directions, and only one of them is achievable by
writing.

You cannot make a specification profitable by editing it. You *can* make it
undebunkable by editing it, and there is an easy way to do it: keep rewriting the
tests until they are tests the document passes. Nobody has to delete anything.
Each individual edit looks like rigour — more precise wording, a clearer pass
condition, a defined fixture instead of a vague one. The direction of travel is
always toward a test the spec survives, because that is what was asked for.

Ten months later you have 2,264 lines of doctrine, 446 lines of roadmap,
forty gates, and a machine that has never been pointed at anything real.

**This is the thesis. Everything below is evidence for it.**

---

## Forty items, three of which could tell you anything

Here is every criterion in the roadmap, sorted into three kinds. **The distinction
between the second and third columns is the one that matters**, and it is easy to
blur: plenty of these gates need a live connection to the chain. Needing a live
connection is not the same as needing to be *right*.

| Phase | Items | No network at all | Live, but measures itself | Must be right about the market |
|---|---|---|---|---|
| 0 — foundations | 6 | 6 | 0 | 0 |
| 1 — ingestion | 6 | 4 | 2 | 0 |
| 2 — decision engine | 6 | 6 | 0 | 0 |
| 3 — validation | 6 | 5 | 0 | **1** |
| 4 — execution | 6 | 6 | 0 | 0 |
| 5 — interface | 5 | 5 | 0 | 0 (correctly so) |
| 6 — promotion gates | 5 | 1 | 2 | **2** |
| **total** | **40** | **33** | **4** | **3** |

The four in the middle column — Phase 1's shadow soak and health bands, Gate 6B's
shadow live and Gate 6C's capped paper — are real work against a real stream. Every
one of them asks *is the machine behaving?* None asks *is the machine right?*

The three on the right:

- **Phase 3, criterion 5 of `STS_ROADMAP.md`** — "Holdout stressed EV LCB is
  positive under the approved policy." In plain words: *on data you held back,
  does this make money?* This is the only pre-money contact with reality in the
  entire plan. **It failed on 2026-08-27**, and it did not fail narrowly: the best
  rule anyone found loses about 5% to 6% a trade after costs, against a break-even
  of +2.12%. The gap is 7 to 8 points.
- **Gate 6A, deterministic replay** — "positive out-of-sample stressed EV LCB."
  This is Phase 3 criterion 5 restated, so it is one test appearing twice, not two
  tests. **Three items, two tests.**
- **Gate 6D, micro-capital live** — "positive stressed expectancy after realized
  costs." Real, and it happens *after* mainnet is authorised and euros are at
  risk.

**Now the sequence, which is the part that makes it vivid.** Number the forty in
the order the plan runs them. Phase 3 criterion 5 — the first and only pre-money
test of whether any of this makes money — is **item 23 of 40.** Everything before
it can pass while the thesis is wrong. The next one is **item 39**, and by then
Ethan has authorised mainnet and the euros are on the chain.

Read that table again, because the shape of it is the finding. **The plan had one
load-bearing contact with reality before the money, and thirty-seven items that
could hold without it.** They all held. When the load-bearing member was finally
tested it snapped, and everything above it came down at once.

**And the darkest version of it, which the doctrine says about itself.** Annex L
carried, until it was corrected, an annotation saying there was no valid held-out
day at all. What it says today is *"**The calendar-day split is not an out-of-sample
test**: the seven calendar files are nine capture sessions, six usable, and 21 August
is the tail of the 20 August run past midnight"* — and then, added later, that a
genuine holdout does exist and nobody used it. Taken at
face value, that means Phase 3 criterion 5 could not even have been **run** on this
corpus: **the number of runnable pre-capital tests of the thesis was zero**, and
nobody noticed that either.

**It got better and worse at the same time.** That annotation turns out to be
wrong, and the correction is the disease one more time. The day everyone trusted,
08-21, is indeed not a holdout. But a real one was sitting on disk the whole time —
**08-16, behind 3.78 days of total silence and five separate recorder processes**,
which is a cleaner separation than anyone was asking for. It was never used. So the
gate was runnable after all, on data the project already had, and **the reason it
went unrun was that a belief about the data was wrong and nobody checked it.**
That is finding 1 again, in the last hour of the sprint.

The roadmap's own summary line names the mechanism without noticing:
*"Acceptance uses fresh replay **fixtures** and an out-of-sample holdout."* Both
halves are written down. Only the first was ever built.

---

## Seven ways a test stops touching reality

These are patterns, not accusations. Each one is a normal, sensible-looking piece
of process. Ranked by what it cost.

### 1. The rule that forbids the check that would test it

> "Broad-chain subscriptions, unfiltered log streams, **redundant historical
> backfills**, and speculative polling **are prohibited because they burn credits**
> without improving the decision boundary." — `STS_CORE_IDEOLOGY.md` §16.2

The premise — history costs money — is false, and was false from day one. Two
independent public endpoints serve full blocks back to 2024 for nothing, no
account, no key. The project's own `docs/TRAINING_DATA_FREE.md` says so.

The rule cost almost nothing to write and nearly cost the entire finding. When one pass
finally ignored it, an afternoon of free queries returned this: buying
launches loses **6.5% to 17.1% a trade in every one of seven hour-matched windows
from October 2024 to August 2026**, mean −10.1%, with no trend over time. There was
no edge that got competed away. There was never an edge.

**That claim is the strongest in this document, so it should carry its limits the
way the verdict does.** The sweep is hour 18 UTC only; 110 to 183 launches a window,
which is ±2.7 to ±10.2 points; 7 of 23 planned windows; landed transactions only;
and before own-order impact and priority fees. **It survives all of that
comfortably** — every window is negative, the worst is the middle of the period
rather than the end, and the nearest window to break-even misses by more than twice
its own error bar. But a claim this load-bearing that travels without its caveats
is exactly how this project got into trouble, so it travels with them here.

**That is the whole verdict, and it was one afternoon and zero euros away for ten
months, behind a rule whose stated reason was wrong.**

The general shape: *a rule that forbids an action on a factual premise, where
nobody has checked the premise.* The premise is usually about cost, risk or time —
the three things people accept without asking for a number.

### 2. Graded against its own answer sheet

> "Hard-block, degraded, paper-only, and observe-only decisions **match the
> master-spec pseudocode** across a **golden decision corpus**."
> — `STS_ROADMAP.md`, Phase 2 acceptance criterion 3

This is the purest instance in the document. The code is checked against the spec's
own pseudocode, on a corpus of examples written by the authors of both. It is a
real test — it catches transcription bugs, and those are worth catching. But it
**cannot fail for the reason that matters.** If the pseudocode is wrong about the
market, a perfect score is a perfect score on being wrong.

**And there is a detail here that is almost too neat.** That criterion is the
**only place in the entire roadmap that reaches into the doctrine at all.** Two
documents, 446 lines and 2,264, and exactly one citation between them — and what it
imports is the **pseudocode**. Not the methodology annexes sitting a few hundred
lines away, which are the good part. The plan reached into its own specification
once, and took the answer key rather than the method.

Phase 2 has five more like it: a benchmark "on a representative fixture", features
that must be "reproducible and provenance-linked" (reproducible is not correct),
and a calibration **fixture** reporting how well predictions match outcomes — where
the outcomes were invented. Two lines below that sits an excellent rule: "no model
promotion occurs from in-sample performance alone." The word *fixture* above it
takes the rule apart.

### 3. The stand-in is allowed to be the real thing

> "**Testnet/devnet or fixture harness** demonstrates private bundle lifecycle."
> — `STS_ROADMAP.md`, Phase 4 acceptance criterion 6

One word: *or*. A test network is a machine that says no. A fixture harness is a
machine you wrote, and it says whatever you built it to say. Offering them as
alternatives means the branch that gets taken is the cheap one, and the cheap one
certifies nothing.

What that would have certified here: there is no ed25519 signing anywhere in the
crate and no address derivation, so the transaction builder cannot produce
something a validator would accept. The code says so itself, in a comment —
*"A real node would reject it"* (`src-tauri/src/execution.rs`). A fixture
harness would have passed it.

### 4. A pass that does not require winning

> "**Gate 6C — Capped Paper.** Acceptance: 14 consecutive days, deterministic
> ledger, daily reconciliation, realistic fees/slippage/tips, no unexplained
> divergence from replay beyond predeclared tolerance, kill-switch and exit-only
> drills passed." — `STS_ROADMAP.md`, Phase 6 promotion gates

Every term is the simulator agreeing with itself: deterministic, reconciled, no
divergence *from replay*. **Nothing requires the paper account to be ahead at the
end.** Fourteen straight days of losing money is a PASS as written.

And 6C is the last gate before 6D puts real money on the chain. So the first time
the plan asks "did this make money?" is *after* it has authorised spending.

The shape: *the gate measures fidelity to the model, and the model is the thing on
trial.*

### 5. "Done" defined without a number in it

Annex X — the acceptance checklist, explicitly "a release gate, not a documentation
suggestion" — lists **twenty** things that make the system production-ready.
Nineteen are engineering and safety properties: replay reproduces feature values,
sinks reconcile, no disk writes on the hot path, keys isolated, UI shows every
safety state. All good, all satisfiable by the fixture ladder above.

One points at the market: "paper-trading gates pass out of sample". **The words
"positive expectancy" do not appear in Annex X**, so follow the delegation and see
where it lands. It goes to Annex L, which specifies *how to report* results —
cohorts, calibration, Brier score, walk-forward, and yes, "expectancy net of fees"
— all in a list headed **"minimum statistical report."** A report is a shape, not a
threshold.

Annex L then names the conditions under which a promotion gate **fails**, and there
are exactly five: a hard safety invariant fails, execution diverges from simulation
without explanation, confidence is miscalibrated, the kill switch is unreliable, or
persistence reconciliation is incomplete.

**Not one of the five is "it lost money."**

So the chain is complete and it never touches the ground: the release gate points
at the paper gates, the paper gates point at a reporting annex, and the reporting
annex lists five ways to fail, none of which is losing. You can satisfy nineteen
twentieths of your own definition of "finished" while losing money on every trade,
and the twentieth will not stop you either.

### 6. A premise nobody priced — and it shaped the architecture

> "Phase 0: Slot 0–10 sniper death match — **excluded** … Purpose is telemetry
> only." — `STS_CORE_IDEOLOGY.md` Annex F, enforced as gate G-402 and as a required
> test case.

The launch block was ruled out as an unwinnable race. It was written into three
places and **never measured until 2026-08-27.**

Measured, it is the only positive entry window in the market: **+9.5%, declining
monotonically after it.** Every other entry second is worse.

**The exclusion survives — and for a completely different reason.** At the size
this bankroll can trade, the median same-block buyer returns exactly **−1.90%**,
which is the round-trip fee and nothing else. The median only turns positive at 5
SOL, five times the whole bankroll. It is a **size** effect, not a **speed**
effect.

That distinction is not academic, because *the wrong reason built the machine.* The
dual-speed pipeline, the 10-millisecond budget, the tip-escalation ladder, the
garbage-collection defence — all of it exists to win a speed race the spec had
already declared unwinnable and then spent a document trying to win anyway. Had
G-402's premise been priced when it was written, "speed is not the bottleneck, size
is" was available on day one for free, and a different system gets built.

**Getting the right answer for the wrong reason is not a near-miss. It is a bug
that keeps paying out.**

### 7. The defect that no counter can count

This one is an absence rather than a rule, which is why it survived every audit.

Every loss-detection clause in the doctrine is written from *inside the running
process*:

- "never drops trades … dropped data is **counted and recorded**"
- "zero critical events are **silently dropped**, all drops are **counted**"
- "no memory leak, queue drift, or **silent event loss**"
- ingress metrics: accepted, duplicate, rejected, **gap count**, source lag

Every one of those is a counter incremented by a program that is running. **A
launch the program never received cannot be counted, and the time between a stop
and the next start is structurally invisible**, because gap rows are written by the
running program.

Searching both documents for *uptime*, *downtime*, *duty cycle*, *coverage*: there
is not one requirement, metric, gate or alert that measures how much of the wall
clock the system was actually listening.

**Two places come close, and how they miss is instructive.** §16.5 names "client
uptime" as an accepted risk — but it is prose in a list of pros and cons, with no
metric, no threshold and no gate attached, and the sentence that closes the list
says these risks are accepted "only when measured", which is exactly the thing
nothing then requires. And AL.4 says to include **data outages in denominators**,
which is the right instinct and the correct control — except that it needs the
outage to be *known*, and the only record of an outage is a gap row written by the
running process. **A control that depends on the broken thing to report itself is
not a control.**

The cost was three successive confident, wrong answers to "how much data are we
missing?" — **40%, then 0–2.5%, then 90%** — all derived from inside a capture that
cannot see what it never received. The real answer came from checking against the
chain instead: **137 of 137 launches caught, zero drops.** The recorder was
essentially perfect while connected. It was connected 3–72% of the time. The
sharpest version of it: **"08-21 is not a day, it is 48 minutes spread over ten
hours."** The dashboards would have shown a clean board throughout.

The missing metric is one line and it is in neither document: **events you should
have received, from an independent source, over events you received.**

---

## What that produced

- **Nothing in the Rust crate has ever read a real capture.** No file under
  `src-tauri/` opens `coins-*.jsonl` or `tracks-*.jsonl`. There is not one real
  replay fixture on disk. `fixtures.rs` — a synthetic generator, which the
  project's own first rule forbids — opens with the words *"launches that never
  happened."*
- **"1,654 tests pass" means the engine agrees with itself.** Every number in the
  verdict came from Python written during one night, against the raw files, going
  around the engine entirely.
- **The three things a trading system needs are the three that do not exist:** no
  exit rule anywhere in the source, no paper mode ever constructed outside a test,
  no entry-side transaction builder.
- Nobody noticed any of that for months, and the reason is the same reason: nothing
  ever ran end to end against real data, so nothing surfaced them.

---

## The engineering is not what failed, and this matters

It would be easy and wrong to read this as a story about sloppiness.

**`CurveState` in `replay.rs` is the best code in the project.** Integer-exact
constant-product arithmetic, fee taken outside the curve, rounding always against
the trader. It is the only component ever checked against reality, and it passed:
it predicts the tokens received from **5,927 of 5,959 real launch buys** exactly.
Beside it is a hash-chained record format whose cursor cannot seek backwards, which
turns "this decision could not have seen the future" from something a reviewer has
to notice into something the type system enforces.

**`walkforward.rs` independently implements the leakage controls** — purge, embargo,
group splits — that the sprint's analysis only later discovered it needed. Somebody
knew.

And the doctrine is full of clauses that are the **opposite** of self-sealing.
These deserve naming, because an audit like this creates pressure to throw
everything out:

- **§14's zero-trade decomposition** — a period with no trades "must never be
  accepted as evidence that the market had no opportunities." Exactly right.
- **AL.1** — record every hypothesis, parameter set and rejected result, apply
  false-discovery controls, and **"reserve a final untouched test set."** That is
  precisely the control for the error the sprint spent a night rediscovering.
- **AL.3** — "a zero-slippage fill is invalid." The single assumption that turned
  the sprint's last positive result negative.
- **AL.4** — report results with and without excluded launches, never mixed into a
  headline. That is the antidote to finding #6, written down long before it was
  needed.
- **AL.5** — a statistically significant edge can be economically negative; every
  report must carry net dollars per unit risk, capital drag, tail loss and a
  capacity curve. That is the correct answer to the thing that actually killed this
  project.
- **Annex W** — define a rug before evaluating, never silently drop unresolved
  outcomes.
- **"UNKNOWN never becomes PASS through defaulting."**
- **§3.3** — "'0 ms' is an architectural target, **not an unmeasured claim**."
- **Phase 3, criterion 2** — "leakage, survivorship, selection bias, and time-split
  violations **fail the run**." Note the verb. Hold on to it.
- **Annex AF's five adversarial questions**, of which number five is literally
  *"Can the validation process reward a strategy that will fail live?"*

That last one is the whole postmortem, asked by the document about itself.

### The sharpest thing in this whole file

Look again at what those clauses are. AL.1 through AL.5 are not scattered good
instincts. **They are a correct, complete diagnosis of exactly how this project
would fail**, written years before it failed, by someone who understood the problem
better than most of the sprint that later rediscovered it.

So why did none of it fire?

**Because of where it was filed.** The walk-forward validation, the multiple-testing
controls, the cohort and regime breakdowns, the reserved untouched test set — in the
roadmap these appear under Phase 3's **Deliverables**. Not under "Exact acceptance
criteria." And a deliverable does not have a PASS or a FAIL. It is a thing you are
supposed to produce, and nothing checks that producing it changed any decision.

Trace it and only two threads reach a gate at all: Phase 3 criterion 2 makes
leakage and selection bias **fail the run**, and Gate 6B requires zero-trade periods
to be **decomposed**. Both are enforced, both are good, and both are narrow.
Everything else in Annex AL — the whole methodology — is unattached.

**The author diagnosed the failure mode correctly, wrote the remedy, and filed it
somewhere with no enforcement.** That is a different and more useful finding than
either the failures or the defences on their own, and it generalises past this
project completely:

> **A control that is not attached to a gate is a comment.**

It does not matter how right it is, who wrote it, or how prominently it sits. If
nothing fails when it is ignored, it will be ignored — not through bad faith, but
because a deadline arrives and the things with PASS next to them get done first.

**The problem was never carelessness.** It was a specification optimising for its
own survival, written by someone who knew better and put the part that knew better
in a section that could not stop anything.

---

## The sprint caught the same disease in one night

Eighty-four analysis reports written in one night reproduced the failure in miniature.

**Across the night, fifteen load-bearing findings were retired**, and the number has
since reached **thirty** — the row count of the *Do not quote these numbers*
table in `docs/sprint-2026-08-27/INDEX.md`, which is the register of record and which
kept growing after this was written. Several were
published before anyone noticed. A p90 read as a median from 25 samples, which nearly killed the
project for the wrong reason. Stop-losses priced as though you get the stop price.
A table with the inconvenient row removed. Graduated coins deleted from one arm of
a comparison and not the other, worth 48 points. An on-chain failure rate wrong by
**274 times**, from our own broken counter. A market collapse invented out of our
own listener's output.

**Every single one was caught by a second pass going and recomputing the number.
Not one was caught by anybody re-reading the report.** Reading finds typos. Only
recomputation finds a wrong number that is confidently stated in a well-written
paragraph — and every one of these was.

One instance was a piece of hygiene. Every pass in the sprint dropped
rows where the price array was full, on the reasonable belief that a
full array meant a truncated record. **There was no truncation** — the array is a hard cap, and
the recorded peak matches the raw candles to seven parts in a million on every
affected coin.

What the rule actually did was **delete half of every genuine 5x in the corpus.**
69 coins at a median peak of 3.14x, 53% of the real tail. It made a 5x look twice
as rare as it is, narrowed the confidence interval by 29%, and inflated the
measured value of exiting early — the grids have since been re-run without it, and
the effect came in at **1.87 points** against a forecast 2.15. It was applied by
everyone, for a whole night, and it looked exactly like good practice.

**The diagnostic that would have caught it takes one minute:**

> **Cross-tabulate what your filter drops against the thing you are measuring. If
> the dropped rows score higher than the kept rows, it is a selection rule, not
> hygiene.**

Here: dropped rows had a mean peak of **3.58**, kept rows **1.24**. Three times.
One `GROUP BY` and it is over.

### And then it survived the repair, in the same night

That is the filter error, and it is a wrong instruction propagating. **This one is
better, because it is the failure mode itself reappearing inside the fix — and it
is verifiable in the tree right now.**

Two passes spent the night repairing the recorder. Its defining flaw was that **it
could not detect its own defects**: it wrote `follow: 60` on every row regardless
of how long it had really watched, counted failed transactions without recording
one, and had no way to notice it had been switched off between runs. They fixed all
of it and took the test suite from **15 tests to 243.**

Then a third pass did something different. Instead of testing the recorder, it
tested **the checker** — took one sound record and corrupted it field by field to
see what the checker would notice.

> **17 of 31 corruptions passed `checkRow` unnoticed.**

And the two headline fields added during that very night — `outcome.curveAtEntry`
and `outcome.feeSol`, the state behind the entry price and what the trading fee
actually cost — **had no check pointed at them at all.**

The reason is the entire point. The rule that every number must show the rows
behind it was written by the pass that did **not** add those two fields. Each
author checked what they had built. Neither checked what the other had. **The seam
between two careful people was invisible to both** — which is the same shape as the
seam between the engine and the market, at a scale of hours instead of months.

Three more defects sat on that same seam. The session header recorded that a row
had hit the sells or zero-fee cap without ever saying what the cap was. A candle
could carry three of its four reserve fields with nothing checking its close. And
the sharpest: **the costs file named the Solana network fee `feeSol`, which is the
name the coin record already uses for the pump.fun trading fee — two different
quantities, one name, in two files that are joined on `sig`.**

It is now **285 tests, with 5 of the 8 silent fields given an invariant and the other
three printed on screen.** Both of the things below were still open when this was
written, and both were closed the same night — the record of how is in
`docs/sprint-2026-08-27/W70-capture-policy.md`:

- ~~**`SCHEMA` is still 2 after five commits changed the record's shape.**~~
  **Closed: bumped to 3**, stamped on every record type that can be read on its own,
  and `capture check` now refuses a version it does not know. Pointed at the recorder's
  own live output the new rule immediately failed it, because coin rows were stamped
  and tick/gap rows were not. A version
  number that does not move is one more counter that cannot report its own failure —
  a reader cannot tell two shapes apart, which is finding 7 in miniature.
- **`curveConservation` fails a row while firing on 5.0% of ordinary coins.** Two
  functions below it in the same file, its own authors wrote the rule it breaks:
  *"a check that fires on 6% of ordinary coins for a reason nobody can explain is a
  check that gets ignored, and that is how the last set of defects survived."* It
  was flagged rather than changed, which is right — it is the owner's call, not an
  accident.

The lesson is not about the quality of the repair. It is that **everybody
validated their own work and nobody validated the join** — and that is the shape
most likely to recur in whatever gets built next, because every individual
contribution looks complete.

---

## How to tell if you are doing this

Portable. None of it is about trading.

**1. Count your gates, then find where they sit and what they are wired to.**
How many can fail because the world disagrees with you — and **where in the order do
they come?** A thesis test at item 23 of 40 means twenty-two things can go green
while the idea is wrong. Then walk the other direction: for each control you rely
on, name the gate that enforces it. **Position and attachment are the two numbers
that mattered here**, and neither is visible from a passing test suite. (Resist
setting a target ratio; I have no evidence for one, and item 3 applies to this
checklist too.)

**2. Find your first real contact with reality, and ask when it happens.**
If it is late, everything before it is unvalidated no matter how green it is. Move
one real test to the front, even a bad one. A weak early check beats a rigorous
late one.

For this project that test is written down in [`docs/GATES.md`](GATES.md) as **T2 — the
floor**: *buy every launch in a real window, net of measured costs, over several
windows spread across time.* No filters, no scoring, no capture, no Rust. It takes
an afternoon on free public data, it was available from day one, and run at the end
it returned **−6.5% to −17.1% across seven windows.** That is the whole project
answered before the first line of the roadmap. **Whatever your equivalent of T2 is,
it is cheaper than you think and it goes first.**

**3. Every rule that forbids something: where is the number?**
"Too expensive", "too risky", "too slow", "unwinnable" — each is a factual claim
with a price. If nobody has measured it, the rule is a guess wearing a uniform.
Price the three most load-bearing ones this week. They are usually cheap; that is
the whole trap.

**4. Never grade the code against the spec and call it validation.**
That test is worth having and it is not the test. If the answer key was written by
the same people as the code, a perfect score means they were consistent.

**5. Ban the word "or" between a real environment and a simulated one.**
"Testnet or fixture harness" always resolves to the fixture. If the real thing is
genuinely unavailable, say the check is **unrun** — do not let a stand-in inherit
its authority.

**6. Every gate needs a number to beat, not a list of properties to have.**
"Deterministic, reconciled, no divergence" is a description of a well-behaved
simulation. "Ahead after 14 days" is a gate. If your definition of *done* contains
no threshold, it is not a gate.

**7. For every control you are proud of, ask what fails if it is ignored.**
If the answer is "nothing", it is a comment, however correct it is. Methodology
filed under *deliverables*, *guidelines* or *principles* has no teeth; the same
sentence written as a pass condition does. Walk your best practices one at a time
and find the gate each one is wired to. The ones with no wire are the ones you will
skip under deadline, and you will not notice skipping them.

**8. Ask what your monitoring cannot see because it is part of the thing being
monitored.**
Counters written by a running process cannot count the time it was not running.
Test coverage cannot see the file nobody imported. For anything that matters, get
one number from **outside**: expected versus received, from a source that does not
depend on you being up.

**9. Check whether the right answer has the right reason.**
An exclusion, a threshold, a design choice that is correct on a premise nobody
verified will be correct until the day it silently is not — and in the meantime the
wrong premise shapes everything built around it. Right answer, wrong reason, is a
bug that pays out for months.

**10. Cross-tabulate every filter against your outcome variable.**
If the rows you drop score systematically higher or lower than the rows you keep,
you have a selection rule until you justify it from outside the thing you are
measuring. One minute, every time, no exceptions — and the justification matters,
because STS's own manufacturing-actor filter fails this test and is still correct: the
rows it drops peak at 1.918 against 1.159 for the rows it keeps, and it is justified
anyway, by evidence from outside the outcome variable (none of the 86 real-tail coins
is actor-touched, and none of them fails the SOL-flow test). **The cross-tab is a
trigger for justification, not a verdict.**

**11. A number is not checked until somebody has recomputed it.**
Reading a report checks the prose. Only re-deriving the figure — different code,
different person, ideally a different route to the same quantity — checks the
figure. Budget for this explicitly; here it caught every wrong number, and reading
caught none of them.

**12. Nobody validates the seam, so assign it.**
When two people each build a piece, each will test their own and both will be
right. The field one added and the other's checker never looked at, the name that
means two things across a join, the version number neither bumped — that is where
the defects live, and it is invisible from inside either contribution. Name a
person whose job is the join, or write the test that corrupts a good record field
by field and counts what nothing notices.

**13. Watch which way your corrections run.**
If every correction makes the story *less* flattering, the work was being done by
people who wanted a result — and the corrections are the system working. If they
run the other way, be much more suspicious.

**14. Ask directly: "what result would make us stop?"**
Then find the test that produces it, and check that it is actually scheduled. If
nobody can name that result, no gate in the plan can produce it either.

---

## The one rule

Everything above collapses into a sentence that was the project's own first rule,
written into the sprint brief and never written into the specification:

> **No gate passes on data the project invented.**

STS is a well-built machine that was never pointed at reality. That is not a rare
failure and it is not a dramatic one — no test was skipped, no corner was cut, and
every green checkmark was honestly earned. It is just that **thirty-seven of the
forty items were measuring whether the machine was well made.** The other three
were two tests, and one of the two comes after the money.

It took the entire project to reach the first one. It failed in an afternoon.

*A correction that makes this worse rather than better.* Earlier versions of this
document said ten months. The repository's first commit is 2026-08-10 and this was
written on 2026-08-27: **seventeen days.** Forty acceptance criteria, 106,000 lines of
Rust and 1,654 tests were built in seventeen days, and not one of them asked whether
the trade made money. The speed is not the flaw — it is what makes the shape visible.
A structure that can absorb that much work without touching its own premise does not
need ten months to go wrong. It needs a fortnight.
