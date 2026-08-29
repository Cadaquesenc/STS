# What transfers

**Written 2026-08-27, after the no-go. The trading conclusion in
[`VERDICT-2026-08-27.md`](VERDICT-2026-08-27.md) is specific to pump.fun. This is the
part that is not, tested rather than asserted — five practices that transfer, one that
transfers only with a boundary attached, one that is textbook wearing new clothes, and
one this project has not earned. Plus the honest counterweight: what STS got right
before any of this, which the postmortem is at risk of burying.**

---

# THE PORTABLE PART

The postmortem's claim is half right. Five of the practices the sprint named do transfer. One transfers only with a boundary attached. One is textbook wearing new clothes and should say so. One is not earned by this project's own record. And one — the shape the postmortem ranks ninth — is fine as a principle and its proof does not currently reproduce.

## 1. A control that is not attached to a gate is a comment

**If nothing fails when a rule is ignored, it is not a rule.**

STS's specification contained a correct and complete diagnosis of how the project would fail, written before it failed. AL.1: record every hypothesis and its selection date, and reserve a final untouched test set. AL.3: a zero-slippage fill is invalid. AL.4: report results with and without exclusions, never mixed into a headline. AL.5: a statistically significant edge can be economically negative. Each one is the exact remedy for something the sprint spent a night rediscovering. None fired.

They were filed in the ideology document. The roadmap — the thing that hands out PASS and FAIL — cites the ideology document exactly once, at `STS_ROADMAP.md:313`, and what it imports is the pseudocode, not the methodology (W69 §3, from a grep for `annex|ideology|master.spec`). The methodology does appear in the roadmap, in Phase 3's *Deliverables* list at `:335`. A deliverable has no FAIL. Only two threads reach a gate: Phase 3 criterion 2, which makes leakage and selection bias fail the run, and Gate 6B, which requires zero-trade periods to be decomposed. Both good. Both narrow.

**Cheapest way to apply it:** take your best practices one at a time and name the check that goes red when each is skipped. Anything under "principles", "guidelines" or "deliverables" has no check. Write the check or drop the principle. The middle option — keeping it as prose — is what happened here.

Nothing about this is about trading. It is about where a sentence lives in a document.

## 2. A number is not checked until someone re-derives it by a different route

The register's retired list carries **28 findings**. Every one was caught by somebody recomputing the figure from the raw files. Not one was caught by anybody re-reading a report. Twenty-seven verification passes ran, and **not one left its target exactly as it found it** — eight confirmed the load-bearing claim outright, nineteen corrected a number, struck a headline, or split the report into a part that held and a part that did not.

The sharp version is that *route* is doing the work, not *person*. W71 reproduced W64's +4.39 pp exactly; W72 got +1.57 to +2.13. W71 rebuilt W64's construction. W72 rebuilt the question.

**Cheapest way to apply it:** for every number a decision rests on, write down who computed it and by what path. Where the answer is one name and one path, the number is unchecked — say so in the document rather than fixing it. Naming it is free; the fixing can be scheduled.

## 3. Cross-tabulate what a filter drops against what you are measuring

Every pass dropped rows whose price array was full, on the reasonable belief that a full array meant a truncated record. It was a hard cap. Dropped rows had a mean peak of **3.58**; kept rows **1.24** (W55; W65 independently gets 3.580 and 1.269; W79 gets 3.58 and 1.25). It deleted 69 coins — 53.6% of the real tail. It moved the headline 2.09 points, narrowed the confidence interval by 29%, and inflated the measured value of exiting early by 1.87 points. It looked exactly like good practice, all night, to everyone. One `GROUP BY` ends it.

**The postmortem states the test one step too strongly, and the sprint's own work proves it.** Item 10 says: if the dropped rows score higher, you have a selection rule, not hygiene. W65 turned the test on its own filters and the manufacturing-actor filter failed it too — 1,376 rows dropped, mean peak 1.918 against 1.159 kept — and that filter is correct. It is justified from outside the outcome variable: 0 of the 86 real-tail coins are actor-touched, and 0 of them fail the SOL-flow test. `GATES.md` D4 gets this right ("without a stated mechanism for why the difference is not the effect you are looking for"); the postmortem does not. **The test is a trigger for justification, not a verdict.**

That the shape recurs is the real evidence it travels. W79 found `who[]` is also capped, at 200, with 99 coins sitting at the cap — and the actor-removal rule reads `who[]`, so above 200 wallets the actor can be present and invisible. Separately, only 7,184 of 12,205 coins can be priced at all, and the excluded rows again score higher.

**Cheapest way to apply it:** one row per filter — dropped, kept, mean outcome of each. Then require that the population at the top of the report equals raw rows minus the sum of the drop column. The second half is what catches rows leaving silently.

## 4. Get one number from outside the thing you are measuring

STS answered "how much data are we missing?" three times — **40%, then 0–2.5%, then 90%** — and every answer was computed from inside a capture that cannot see what it never received. Counters are incremented by a running process, so the time it was not running is structurally invisible. Checked against the chain: **137 of 137** launches, 62 of 62 on one listener and 75 of 75 on the other in its busiest recorded minute, zero drops. The recorder was essentially perfect. It was connected 3–72% of the time. The dashboards showed a clean board throughout.

The durable sentence is the postmortem's own: **a control that depends on the broken thing to report itself is not a control.** It has a code-level twin nobody has drawn out. W79 found that `check.js` guards its value checks with `has = (k) => k in outcome && outcome[k] != null` (`tools/capture/src/check.js:821`), so a missing field makes the check *skip* rather than fail. That is "UNKNOWN never becomes PASS through defaulting" — the doctrine's own rule — being broken inside the code that enforces it.

**Cheapest way to apply it:** one number, expected over received, from a source that does not depend on you being up. Test coverage cannot see the file nobody imported. A queue's own metrics cannot count the messages it never got.

## 5. Nobody validates the seam, so assign it

Two passes fixed the recorder and took it from 15 tests to 243. A third tested the *checker* instead: took one sound record, corrupted it field by field, and asked what nothing noticed. **17 of 31 corruptions passed unnoticed.** Both of that night's headline fields — the curve state behind the entry price, and what the trading fee actually cost — had no check pointed at them. The rule that every number must show its rows was written by the pass who did not add those fields. Each author checked what they had built. The sharpest one: the costs file called the Solana network fee `feeSol`, which is the name the coin record already uses for the pump.fun trading fee — two quantities, one name, two files joined on `sig`.

**Cheapest way to apply it:** corrupt a known-good record one field at a time and report two numbers — fields corrupted, fields caught. Or name one person whose job is the join. The defect is invisible from inside either contribution, because every individual contribution looks complete.

## 6. A gate must be able to fail for a reason that is not your fault — with a boundary

Forty acceptance criteria. **37 never test the thesis.** Three name a market outcome, and two of those are the same test written twice, so there are two distinct tests. One comes before money: item **23 of 40**. The other is item **39**, after mainnet is authorised. The 23rd failed, and by the roadmap's own annotation it could not have been validly run on this corpus at all.

**Now the boundary, because this rule is empty for most software.** For a compiler, a parser, a payroll system, "we wrote a bug" is the only failure mode that matters, and a ladder made entirely of engineering gates is the right ladder. The rule bites exactly where a project rests on a claim about something it does not control — a market, a user, a physical process, another team's system. Where that is true: count how many gates can go red because the world disagrees, and note where in the order they sit. **Position and attachment were the two numbers that mattered here, and neither is visible from a passing test suite.**

## 7. Match the exposure — real, but say what it actually is

STS got this wrong five times. W18's flow lead was worth 1.8 points until someone noticed one rule sat in the trade 3.9 seconds and the other 32.7. The report that *named* the failure mode then committed it: W26 compared a first-sell rule holding 22.0 seconds against a 6-second stopwatch and got the opposite sign to everyone else. And the sprint's best lead selected coins that never trade again — 91% of its out-of-sample improvement was "nothing happened while you held".

Be honest about what this is: confounding, which is textbook, and the reader probably already knows it. What the project actually added is narrower and better. **The confounder is usually the cost dimension of the treatment, not a property of the subject.** Time in the trade. Tokens spent. Retries. Attempts. You do not think of it as a variable because it is the price of the rule rather than part of it. Name that dimension, equalise it, and put it in the null.

**Cheapest way to apply it:** before comparing A and B, list every way they differ that is not the thing you are testing. If one costs more time, money or attempts, that is what you are measuring.

## Two things to cut, and one to hold back

**"Watch which way your corrections run" is not earned.** The postmortem's item 13 says that if every correction makes the story less flattering, the work was being done by people who wanted a result. Here the corrections ran both ways and the work was good. W16 cut the landing cost by about 20x and removed a wall that had nearly killed the project. W50 turned a 93.2% failure rate into 11.3%. W48 struck the claim that the recorder dropped 90% of launches. W55 and W65 moved the headline from −9.85% to −7.76%. Against those, W52 cut the ceiling from +18.9% to +12.1% and W72 demoted two leads. The antecedent was false, and the diagnosis it implies would have been wrong.

**"Right answer, wrong reason" is a real shape whose STS instance does not hold.** Finding #6 says the launch block was excluded as an unwinnable speed race; that measured, it is the only positive entry window — +9.5%, declining monotonically — so the exclusion was right for the wrong reason, and the wrong reason built the machine. W79 is the only run that ever re-ran it. **It does not reproduce under any of four conventions.** In W79's run the launch block is the *worst* entry at every hold length (−1.18% at 5s, −9.55% at 60s, against second 30 at −0.51% and −1.40%), and the size claim inverts: the 0.05–0.5 SOL bucket returns −8.6% to −9.4% rather than −1.90%, and returns get *worse* with size, never turning positive. W79 is single-source and was never rechecked, so this is a live contradiction, not a correction. The irony is exact: the one item offered as proof that a right answer can rest on a wrong reason is the one number in the ladder nobody re-derived. **Do not ship this item with that instance attached.**

**And the postmortem's own headline advice survives, with a second sentence it is missing.** T2 — buy every launch in a real window, net of measured costs — cost **0.35 seconds of compute** when somebody finally ran it. T1 and T2 together are two Python scripts, 40 and 45 lines, standard library only, 1.2 seconds. They return **−10.32% net, 95% CI [−13.00, −7.63], n=1,034 over seven windows**, against a break-even of +2.12%. That is the whole project answered.

But cheap is not the same as ready. The document describing T2 never names the archive, the venue, the launch program, the holding rule or the hour-matching, and twenty-four places needed a choice it should have made — six of which decide PASS or FAIL. The worst: *"sell inside the minute" is not an exit rule.* At a five-second hold the same trade returns **−1.38%**; at sixty seconds, **−10.32%**. Nine points on one unstated word, against a 2.12% bar. So the portable form is two sentences: **your equivalent of T2 is cheaper than you think, and it is not runnable until somebody has written down the five or six choices it silently needs.** The second sentence is the one that gets skipped.

---

# THE HONEST COUNTERWEIGHT

The postmortem has a section headed "The engineering is not what failed," and it means it. But it is a document about a failure, and four things risk being buried under it. Two are stronger than stated. One is weaker. One is a date.

**The specification contained the correct diagnosis, written before the failure.** This is the largest thing the project got right. AL.1 through AL.5 are not scattered good instincts: reserve a final untouched test set; record every hypothesis and its selection date; a zero-slippage fill is invalid; report with and without exclusions, never mixed; a statistically significant edge can be economically negative. Annex W: define a rug before evaluating, never silently exclude an unresolved outcome, cluster repeated launches from one deployer so you do not overstate your sample. §14: a period with no trades must never be accepted as evidence the market had no opportunities. §3.3: "0 ms" is an architectural target, not an unmeasured claim. "UNKNOWN never becomes PASS through defaulting." And Annex AF's fifth adversarial question, which is the entire postmortem asked by the document about itself: *can the validation process reward a strategy that will fail live?* The sprint rediscovered four of those empirically over one night. The author had them written down. The failure was where they were filed, not what they said — and both W61 and W69 stop to list them so nobody sweeps them out with the rest.

**The curve model is exact, and it is the only component ever checked against reality.** `CurveState` in `replay.rs`: integer-exact constant-product arithmetic, fee taken outside the curve, rounding always against the trader. It predicts the tokens received on **5,927 of 5,959 real launch buys** — 99.46%, to the four decimal places the file stores, median error 0.0005% on the remainder. Two independent reports measured it. Beside it sits a hash-chained record format with a forward-only cursor (`replay.rs:1416`), which turns "this decision could not have seen the future" from something a reviewer has to notice into something the type system enforces. That is a good idea and it is not a trading idea.

One consequence the postmortem never draws: because the curve model is exact, W1's price-impact result is **structural rather than statistical**. On a constant-product curve an instant round trip walks the price up and back down the same path, so own-order impact cancels exactly and only fees remain. That answer has no error bar. Almost nothing else in the sprint can say that.

**The recorder was essentially perfect, and it was blamed for months of data it never lost.** 137 of 137 launches, verified block by block on both listeners. Three confident wrong answers preceded that — 40%, 0–2.5%, 90% — and the last reached the verdict before being struck. The capture is not lossy. **It is short.** Those are different defects with different fixes, and real time went into fixing the wrong one.

**`walkforward.rs` — true, and the timeline is wrong.** The file does implement purge, embargo and group splits, and its header says why: a group cut alone leaves a model tested on the same wallet population on both sides (`walkforward.rs:23–27`). 2,201 lines, 28 tests. Somebody did know. But the postmortem's framing — leakage controls "the sprint's analysis only later discovered it needed" — reads as foresight held over months. It was first committed **2026-08-26** (`69adf1c`), one day before the sprint, and landed on main during it (`c626069`). It has never been run against a real capture: no file under `src-tauri/src/` opens one. Credit it as good judgement one day early, not as a control that was in place and ignored.

**A fifth belongs here even though it is not a defence: one gate closed honestly.** Phase 0 criterion 1 — fmt, clippy, tests — is the only gate in the roadmap ever closed by work rather than by a market, and it closed properly. The true starting count was 62 findings, not the 52 believed, because `-D warnings` stops at the first failing unit. `cargo fmt` had never been run and touched 1,576 hunks across all 44 files. It closed with **zero new `allow` attributes**, which is exactly how that gate gets faked. Two later passes re-ran it. The postmortem's line that every green checkmark was honestly earned is true, and this is the one place it is checkable.

**Now the date, because it is load-bearing and it is wrong.** The postmortem says *ten months*, three times; OVERNIGHT repeats it. Nothing supports it. The repository's first commit is **2026-08-10**. The directive log the postmortem quotes for its root cause covers Thursday 20 August to Friday 21 August 2026, and directive 4 — "essentially undebunkable and consistently profitable" — is timestamped **09:29 AM UTC on 21 August 2026**, six days before the sprint. The only calendar dates in either specification document are August 20 and 21, 2026. `replay.rs` first appears on 22 August. **Seventeen days from first commit to verdict.**

This cuts two ways, and the second is the important one. It weakens the most quotable line in the document — *"it took ten months to reach the first one; it failed in an afternoon"* — and it makes the finding underneath considerably worse. Seventeen days is enough to produce forty gates that cannot fail, 106,000 lines of Rust, 1,654 passing tests and a doctrine of over two thousand lines, and to have none of it touch the market. **The shape does not need ten months. It needs a fortnight and a specification that was asked to be undebunkable.** Correct the number and the argument gets stronger, which is the tell that it is the right number.

---

## The line worth keeping

None of these five practices is a discovery. A statistician would recognise every one. What this project paid for is the **recognition rules** — what each failure looks like from the inside, while it is happening, to someone careful. The array cap looked exactly like good practice, all night, to everyone. Nobody was careless. That is the part that outlives the repository.

---

# VERIFICATION LEDGER (for the coordinator)

**Register discrepancy.** The brief says the do-not-quote table carries 25 retired figures. It carries **28** — I counted the rows (`INDEX-merged.md:77–104`), which matches the file's own line 32 ("Twenty-eight load-bearing findings have been retired"). The postmortem's "fifteen" is correct as the count at 06:33 and should stay as written.

**Live defect — `GATES.md:161`, the grafted multiplier.** W76 caught this exact sentence in the postmortem; the postmortem was fixed (`POSTMORTEM:457` now reads "the effect came in at **1.87 points** against a forecast 2.15"). `GATES.md` was not. W80 flagged it by line number (`W80-proofread.md:226`, item 20) and it is still there. 2.15 points is about a quarter of a 7-to-8-point gap, not four times it.
- Exact substitution in `docs/GATES.md`:
  - OLD: `value of exiting early by four times the entire gap to break-even.`
  - NEW: `value of exiting early by 1.87 points, against a gap to break-even of seven to eight.`

**Second instance, same family — `GATES.md:303`.** Also flagged by W80 and unfixed. Six points against a 7-to-8-point gap is most of it, not three times it.
- OLD: `trailing stop" across six points of return — three times the whole gap to break-even`
- NEW: `trailing stop" across six points of return — most of the whole gap to break-even`

**`POSTMORTEM:590–592`, item 10, overstated.** Refuted by W65's own application of the test to the actor filter.
- OLD: `If the rows you drop score systematically higher or lower than the rows you keep,
you have a selection rule, not hygiene. One minute, every time, no exceptions.`
- NEW: `If the rows you drop score systematically higher or lower than the rows you keep,
you have a selection rule until you justify it from outside the thing you are
measuring. One minute, every time, no exceptions. STS's manufacturing-actor filter
fails this test and is still correct — 0 of the 86 real-tail coins are actor-touched.
The cross-tab is a trigger for justification, not a verdict.`

**"Ten months" — unsupported at three sites in `POSTMORTEM` (`:7`, `:22`, `:632`) and one in `OVERNIGHT-2026-08-27.md:15`.** Replace with `seventeen days`. Evidence: repo first commit `b9a05db` 2026-08-10; `STS_CORE_IDEOLOGY.md` directive log header names "Thursday, August 20, 2026 through Friday, August 21, 2026"; the only month-day-year dates in `STS_CORE_IDEOLOGY.md` and `STS_ROADMAP.md` are August 20 and 21, 2026; `replay.rs` first commit `cecaee9` 2026-08-22. `VERDICT:430`'s "over ten months" is a different claim (wallet persistence across the history sweep) and should not be swept up.
- `:632` OLD: `It took ten months to reach the first one. It failed in an afternoon.`
- `:632` NEW: `It took the entire project to reach the first one. It failed in an afternoon.`

**Do not ship postmortem finding #6 or `GATES.md` T6 without a flag.** W79 (the only run of the ladder, single-source, never rechecked) contradicts both the "+9.5%, declining monotonically" gradient and the "−1.90% at 0.05–0.5 SOL, positive at 5 SOL" size claim, under all four conventions it tried, and notes T6 contradicts T3 in the same document. W79 also disagrees with `GATES.md` T4 on sign (real −0.51% vs scrambled +0.61% to +1.34%, gap **−1.86 pp**, against the document's +0.4 to +0.9 pp). `INDEX-merged.md:299` names both disagreements and points to the do-not-quote table — **but no row for either exists in that table**; the cross-reference is dangling and should either gain two rows or lose the pointer.

**Numbers I could not fully reconcile, flagged rather than used.** (a) Kept-row mean peak: 1.24 (W55), 1.269 (W65), 1.25 (W79) — dropped side is 3.580 in W55 and W65 identically; the postmortem's "Three times" is 2.8x on W65's figures. (b) D5 corruption counts: 17 of 31 (W60, twice-cited) versus 76 of 79 (W79, a wider field enumeration on a pre-`906e1e7` checker) — two different tests, never reconciled; I quote 17 of 31 only. (c) `docs/TRAINING_DATA_FREE.md` was first committed **2026-08-27** (`9e09284`, "docs: track the training-data research, which was untracked and at risk") — during the sprint. The postmortem's "The project's own `docs/TRAINING_DATA_FREE.md` says so" is true of the world but is not evidence the project knew earlier. The genuinely pre-sprint instance is `docs/archive/legacy-node/src/social.js`, whose own constructor comment says the tweet services are free, sitting against §13's binding €25 budget.

**Claims I verified in the tree myself:** no file under `src-tauri/src/` references a capture file (grep for `coins-*.jsonl`, `tracks-*.jsonl`, the flux data path — zero hits), confirming W42/W47; `fixtures.rs` opens "Synthetic fixtures: launches that never happened"; `walkforward.rs` is 2,201 lines with 28 `#[test]` declarations and its header names purge, embargo and group split; `replay.rs:1416` is "§9 — the forward-only cursor"; `tools/capture/src/check.js:821` is `const has = (k) => k in outcome && outcome[k] != null`; W48's 137 is 62 + 75 at `W48-capture-method.md:11–12`.