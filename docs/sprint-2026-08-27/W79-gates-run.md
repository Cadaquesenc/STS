# W79 — I ran the fifteen gates

`docs/GATES.md` is not on `main`. It is on branch `w74/gates`, commit `c250f5f`. I ran it from there.

## What I found

**Eleven of the fifteen gates are runnable today. Two pass. Eight fail. One is unrunnable
for a reason the document did not anticipate. The three live gates cannot run at all.**

**T2 failed first, and it cost 0.35 seconds of compute.** The headline claim is true and
understated. But four of the six thesis gates could not be applied as written without me
choosing something the document should have chosen — and in two cases the choice decides
the verdict.

| Gate | Verdict | The number it produced | Time |
|---|---|---|---|
| **D1** coverage from outside | **FAIL** (by its own skip test) | every uptime figure traces to a recorder-written row; no external count exists locally for any capture day | 0.3 s |
| **D2** counters rebuilt from rows | **FAIL** | `failRatePct` unrebuildable (no failed tx stored); `pumpTx` wrong in 7/7 windows; `total.solIn` off >0.1 SOL on 96 coins | 0.6 s |
| **D3** time from the row | **PASS** | 5 misfiled coins, all explained | 0.3 s |
| **D4** cross-tabulate filters | **FAIL** | dropped rows peak **3.58** vs kept **1.25** | 0.7 s |
| **D5** corrupt field by field | **FAIL** | **76 of 79** corruptions unnoticed | 0.1 s |
| **D6** two roads to every number | **FAIL** | I was the second road; three of the document's own numbers do not reproduce | — |
| **T1** price the toll | **PASS** | **95.0 bps a side**, round trip **1.90%**, break-even **+2.12%** at 0.05 SOL | 0.9 s |
| **T2** the floor | **FAIL** | **−10.32%** net, 95% CI **[−13.00, −7.63]**, 1034 launches, 7 windows | **0.35 s** |
| **T3** ceiling at zero cost | **FAIL** | **0 of 207** rules positive; best **−0.62%** at exactly zero cost | 4.8 s |
| **T4** noise control | **FAIL** | real **−0.51%**, scrambled **+0.61% to +1.34%** — gap **−1.86 pp** | 15 s |
| **T5** holdout, paired | **FAIL** (both clauses) | level **−2.77%**; paired diff **+0.82 pp**, CI **[−0.07, +1.70]** | (in 15 s) |
| **T6** at the size you can trade | **DEFECTIVE** | does not reproduce under any of 4 conventions; contradicts T3 | 0.4 s |
| **L1** shadow | **UNRUNNABLE** | needs a live run + external count | — |
| **L2** paper | **UNRUNNABLE** | `paper_trades` = 0 rows | — |
| **L3** micro-capital | **UNRUNNABLE** | needs real money on mainnet | — |

Total compute for all eleven: **under 25 seconds.**

---

## The headline claim: verified, and it is cheaper than advertised

T2 is stated as "buy every launch in a real window, net of measured costs", failing at
−6.5% to −17.1% across seven windows against a +2.12% break-even. **It reproduces.**

| window | n | rival buyers in first 3 s | net at 60 s | 95% CI |
|---|---|---|---|---|
| 2024-10-16 | 184 | **2.7** | −7.92% | [−12.61, −3.23] |
| 2025-02-19 | 151 | 3.7 | −5.53% | [−10.26, −0.79] |
| 2025-05-21 | 156 | 2.9 | −18.89% | [−21.48, −16.30] |
| 2025-08-20 | 132 | 6.4 | −10.59% | [−13.97, −7.21] |
| 2025-11-19 | 132 | 4.2 | −8.70% | [−13.14, −4.27] |
| 2026-02-18 | 156 | 4.3 | −10.39% | [−16.04, −4.73] |
| 2026-08-19 | 123 | 6.1 | −10.25% | [−28.37, +7.87] |
| **pooled** | **1034** | | **−10.32%** | **[−13.00, −7.63]** |

Document said mean −10.1%; I get **−10.32%**. Document said 2.7 rivals in October 2024;
I get **2.7**. Every window loses money. There is no trend against time and none against
rival count — in October 2024, with 2.7 competitors, the trade still lost 7.9%.
**No edge was competed away. There was never an edge.**

**The cost.** T1 and T2 are two python3 scripts, 40 and 45 lines, stdlib only. They read
seven files totalling 50 MB from `~/Code/flux/data/history/` — data that came from the free
public block archive. Compute: **0.87 s + 0.35 s = 1.2 seconds.** My wall-clock time from
first opening a history file to the T2 verdict, including working out the record schema
from scratch, was about 25 minutes. No capture, no Rust, no recorder, no roadmap, no
network, no euros. **The claim "one afternoon, week one" is true with an order of
magnitude to spare.** That is the most damning fact about the original roadmap and it
survives being checked.

T3 and T4 then close it: with **every cost set to exactly zero**, not one of 207
pre-declared rules makes money, and a scrambled corpus beats the real one.

---

## Where I had to interpret rather than apply

This is the real test and it is the most valuable thing here. **Twenty-four places.** The
ones marked ★ change a verdict.

### The verdict-changing ones

1. ★ **T3: "the best rule in the grid and its return" — mean or median?** Best-by-mean is
   **−0.51%** (FAIL). Best-by-median is exactly **+0.00%**, which is *not negative*, and
   T3's FAIL clause is "the best rule is still negative". Read literally, T3 **passes** on
   the median. The gate's verdict flips on a statistic it never names.
2. ★ **T3: no minimum-data requirement on a rule.** The winning rule enters at second 30.
   **47% of coins have no candle after second 30**, so for them the rule silently returns a
   flat 1.0 — it never trades — instead of taking a real loss. The grid rewards a rule for
   abstaining. Strip those and the ranking changes.
3. ★ **The two manufacturing-removal rules are not equivalent.** Offered as alternatives
   with an "or": PDA-in-`who[]` removes 1829 coins, the gross SOL-flow bound removes 501,
   and **they disagree on 1344**. T3's zero-cost best moves from −0.38% to −0.66%
   depending which you pick; the dropped rows differ by 30 points of 60-second return
   (−9.19% vs +20.86%).
4. ★ **T2: "sell inside the minute" is not an exit rule.** I had to choose. Net is
   **−1.38%** at a 5-second hold and **−10.32%** at 60 seconds — a nine-point spread on an
   unstated choice, against a 2.12% break-even.
5. ★ **T2: per-window or pooled?** Six of seven windows have intervals excluding zero.
   2026-08-19 does not ([−28.37, +7.87]). T2's FAIL is "the mean is below break-even **and**
   the interval excludes it" — singular. One wide window is enough to argue the gate did
   not fail.
6. ★ **T5: "buying everything inside the same window" is undefined.** Baseline at the
   rule's own entry second is −3.59%; at T2's convention (s=2, hold 60) it is −8.66%. Five
   points of difference, which is larger than the paired difference being measured.

### The rest

7. **D1: no session-gap threshold.** "Wall-clock listening time per day" never says how
   much silence ends a session. 08-21 is **20.6 min** at a 10 s threshold, **48.2 min** at
   60 s, **91.8 min** at 300 s. The document's "48 minutes" is the 60 s choice, unstated —
   a 4.5× range on a free parameter.
8. **D1's inputs contradict its FAIL clause.** It permits "process records showing when the
   recorder was actually running", then FAILs if coverage uses "a counter the recorder
   wrote about itself". Process records *are* recorder-written. `listen.log` — which
   literally prints `dropped 10515` — sits on both sides of the line.
9. **D2: no tolerance for rebuilding a float.** At 1e-9, 3736 of 12205 coins fail to
   reconcile; at 1e-4, 1939; at 0.1 SOL, 96. Three tolerances, three verdicts, no guidance.
10. **D2: "a constant written on every row" has no threshold.** `outcome.follow` is 60 on
    **98.0%** of rows — but 45 on 173 and 40 on 66. Read literally the clause never fires.
11. **D3: "five" and "six" in the same sentence.** I find exactly 5 misfiled coins (a
    four-second burst at 23:53 UTC on 08-20 written into the 08-21 file) and 0 misfiled
    tracks. I cannot tell which number the gate expects me to match.
12. **D3's first FAIL clause is about the analyst, not the corpus.** "Any split is made by
    filename" can always be passed by conducting yourself correctly. It can never fail
    because the world disagrees — which is the document's own opening rule.
13. **D4's skip test is arithmetically impossible.** "The population does not equal raw
    rows minus the **sum** of the drop column" only holds if no two filters overlap. Real
    filters overlap heavily. A correct pipeline fails this test, so it will be waived.
14. **D4 has no category for an unavoidable drop.** 5126 coins (**42%**) have no price path
    at all and *must* be dropped. They score better than the kept rows (peak 1.31 vs 1.25),
    which trips D4's FAIL with no remedy available.
15. **D5: no field-enumeration rule.** I count **79** leaves including array-element fields;
    the document counts **31**. "17 of 31" and "76 of 79" are the same finding with
    different denominators, and nothing says which is right.
16. **D5: "one record known to be sound" does not exist.** Every record in the corpus draws
    10 baseline complaints from the checker. I had to redefine "caught" as *the complaint
    set changed*.
17. **T1's FAIL clause cannot be evaluated at T1 time.** "The toll is larger than the move
    you are trying to catch" requires T2's output, but T1 runs first.
18. **T1: no rule for a window with zero observations.** Two of seven windows (2024-10,
    2025-02) carry **no `fee` field at all**. The toll for the earliest era — the one that
    matters most for "was there ever an edge" — is assumed at 95 bps, not measured. The
    gate demands a sample size and is silent when the sample is zero.
19. **T4: "inside the noise of the grid" is undefined.** Noise across scramble draws, or
    across the 207 rules? I invented one. The number of scramble repetitions is also
    unspecified; I chose 10.
20. **T5: the holdout runs backwards in time.** 08-16 *precedes* the 08-20/08-21 training
    days. The 3.78-day separation clause is satisfied while the split is anti-chronological,
    which is not what AL.2's purge-and-embargo means.
21. **T5's freeze-date skip test has no operands.** There is no frozen, hashed test set and
    no parameter-selection log anywhere in the repo. The one test the document calls "a
    comparison, not a judgement" silently has nothing to compare.
22. **T6 never states the holding period.** "The median same-block buyer at 0.05–0.5 SOL
    returns −1.90%" — over how long? Unstated, and it decides everything.
23. **T6 never says whether the exit charges own-order impact.** Selling into the curve vs
    at spot moves the 1–5 SOL bucket from −8.0% to −14.6%.
24. **D6 is a process gate with no data to run against.** Its skip test is a string search
    for the word "reproduced" in a report.

---

## Can you skip a gate without the skip test noticing? Yes — seven ways

The skip tests are mostly mechanical as advertised. They are also mostly satisfiable
without doing the work.

- **D2.** The skip test is "pick three counters at random and recompute them". Plenty
  reconcile exactly (8469 of 12205 coins reconcile to the lamport). Pick three of those and
  D2 passes while `failRatePct` stays unrebuildable. The checker even ships a
  `DECLARED_CONSTANTS` list with a written excuse for `outcome.follow`, so the one field
  the gate was written about is already pre-legitimised.
- **D4.** Its sum test is impossible with overlapping filters, so it will be waived in
  practice — and then the only surviving requirement is *that a table exists*. A table
  listing eight filters and silently omitting the ninth passes.
- **D5.** Choose a coarse field enumeration — 31 top-level fields instead of 79 leaves —
  and the miss count halves. The skip test only requires naming both numbers; it never
  constrains the denominator.
- **T1.** The skip test is "the cost figure has no sample size attached". Attach the pooled
  **n = 31,253** and nobody notices that two of seven eras contributed **zero**
  observations.
- **T2.** The skip test is "quoted gross, or from one window, or without the population
  count". Report the 5-second hold: **−1.38%**, net, seven windows, n = 1034. Every clause
  satisfied, magnitude cut by a third.
- **T3.** The skip test is "the grid was not written down before it was run, or costs were
  'low' rather than exactly zero". Write the grid down, set costs to exactly zero, report
  the **median: +0.00%** — "not negative" — and T3 passes.
- **T5.** The skip test compares a freeze date to a selection date. Neither artifact
  exists, so the comparison no-ops and the gate reports clean.

**The pattern:** every skip test checks that a *number was reported*, never that the number
was the right one. That is a weaker version of the failure the document is correcting.

---

## Unrunnable for reasons the document did not anticipate

1. **A 42% hole in the price data.** `market.candles` is absent on 3172 coins and empty on
   1849 more. Days 08-10, 08-11 and 08-12 have **no `market` key at all** — the candle
   recorder did not exist yet. Only **7184 of 12205** coins (58.9%) can be priced. Every
   thesis gate on the capture corpus runs on a minority of it, and the excluded rows have a
   *higher* mean peak than the included ones.
2. **`who[]` is capped at 200 entries.** This is the same array-cap trap as
   `outcome.highs`, in a different field, and the document does not mention it. It matters
   because the manufacturing-actor removal rule reads `who[]` — on a coin with more than
   200 wallets the actor can be present and invisible. 99 coins sit at the cap. (I checked:
   no coin is currently mis-classified because of it, but the rule is unsound as written.)
3. **"Exactly t = 3.0 s" is not resolvable in the history corpus.** Block times are whole
   seconds and eight trades routinely share one timestamp — in the first window I opened,
   the price moved +38% inside second zero. The entry rule is defined in candle-units that
   only the capture corpus has.
4. **The windows are 5–8 minutes long.** T2 says "every launch in a real window", but 203
   of 1283 launches (16%) have less than 60 seconds of data after them. I dropped them and
   checked the bias at a 20-second horizon: kept −5.74%, dropped −4.26%. It does not rescue
   T2, but the gate has no category for it.
5. **The checker is from a different era than the corpus.** `check.js` requires `sid`, `v`,
   `whoCapped`, `observedSec`, `highsCapped`, `feeBps` — present on **0 of 12205 rows**.
   Worse, its value checks are guarded by `has(k) => k in outcome && outcome[k] != null`,
   so a missing field makes the check **silently skip** rather than fail. That is
   "UNKNOWN becomes PASS by defaulting", shipped in the code that enforces the rule against it.

---

## Three of the document's own numbers do not reproduce

Running D6 for real means recomputing, and three do not survive it.

- **T6 is wrong and contradicts T3.** T6 says "the launch block is the only positive entry
  window — **+9.5%, declining monotonically after it**". T3 says "the best rule enters at
  second 30, **so paying for speed buys nothing**". These cannot both be true. My run
  supports T3 at **every** hold length:

  | entry second | hold 5 s | hold 10 s | hold 20 s | hold 60 s |
  |---|---|---|---|---|
  | 0 (launch block) | −1.18% | −2.97% | −5.97% | −9.55% |
  | 10 | −1.63% | −2.96% | −4.57% | −5.94% |
  | 30 | **−0.51%** | **−0.85%** | **−1.42%** | **−1.40%** |

  The launch block is the **worst** entry, not the only positive one. And T6's size claim
  fails under all four modelling conventions I tried: the 0.05–0.5 SOL bucket of real
  same-block buyers returns **−8.6% to −9.4%**, not −1.90%, and returns get **worse** with
  size (≥5 SOL: −15.9% to −26.4%), never turning positive. That is what a bonding curve
  requires — a 5 SOL buy moves a 30 SOL virtual reserve 16.7% against you on entry alone.
  **The one gate the document says "changes what STS should have built" is the one that
  does not reproduce.**
- **T4's gap has the opposite sign.** The document reports real −2.32% vs scrambled −2.75%
  to −3.21%, a gap of +0.4 to +0.9 pp. I get real **−0.51%** vs scrambled **+0.61% to
  +1.34%** — a gap of **−1.86 pp**. The scrambled corpus *beats* the real one. Both
  readings support the same conclusion (there is no exploitable time structure), but the
  document's version overstates how much real structure exists. There is none.
- **D5's count.** 17 of 31 becomes 76 of 79 on a field enumeration the document never fixes.

**And one of mine did not survive either.** My first T3/T4/T5 run had `hold` exiting at
absolute second H rather than entry+H — so "enter at second 30, hold 5" was *selling at
second 5 having bought at second 30*, a time machine. It returned a confident **+46.64%**
zero-cost ceiling and a holdout that **passed** at +12.38% with a tight interval excluding
zero. Nothing about the output looked wrong. It was caught only because the number was too
good and I re-derived it. That is D6's entire thesis demonstrated on myself inside one
afternoon, and it is the strongest argument in the document.

---

## What I could not check

- **D1 properly.** No external chain data overlaps any capture day locally, and I have no
  network. The history windows stop at 2026-08-19; the captures run 08-10 to 08-21. The
  document's "137 of 137 launches caught, 3–72% uptime" cannot be verified here. By D1's
  own skip test — "every uptime figure traces back to a row the recorder wrote" — the gate
  is skipped, so I record it as FAIL rather than UNKNOWN.
- **L1, L2, L3.** `positions`, `paper_trades`, `candidates`, `execution_logs` and
  `tick_metrics` are all **0 rows**. L1 needs a live run plus D1's external count; L2 needs
  a paper account that has ever traded; L3 needs real money on mainnet. None can be
  simulated, and the document is right that substituting a fixture would make them
  meaningless.
- **Own-order price impact at the size STS would trade.** I measured it for real
  launch-block buyers from their actual fills, but not for a hypothetical 0.05 SOL order
  placed into a queue it would itself change.
- **08-16's selection bias.** The brief says track coverage there is 4.5%. My T5 holdout
  uses coins with candles (883 of 1822, 48%), which is a different and better-covered
  population, but still not a random sample of the day.

---

## What this means for go / no-go

**No go, and the gates say it four independent ways.**

T2 says buying everything loses 10.3% against a 2.12% break-even, in every one of seven
windows across two years, with no relationship to competition. T3 says that with **every
cost set to exactly zero** — free fees, free network, no slippage, no impact — not one of
207 pre-declared rules makes money. T4 says a scrambled corpus beats the real one, so
there is no time structure to find. T5 says the rule chosen on the training days lands
below break-even on an untouched holdout with a paired difference whose interval includes
zero.

Nothing on the engineering side can move any of those. T3 in particular closes cheaper
fees, faster execution, private bundles and a bigger bankroll in a single line each,
because it already assumed all of them were free.

**On the document itself:** it is a genuine improvement and its central claim survives
being tested — T2 really does fail first, really does cost an afternoon, and really would
have stopped this project in week one. **But it is not yet a specification.** Eight of the
fifteen gates required me to choose something the document should have chosen, and in six
places the choice decides PASS or FAIL. A gate you have to interpret is a gate that gets
interpreted away under deadline, which is the exact failure it was written to fix. The
fixes are small and mostly amount to naming a statistic, a tolerance, a threshold and a
holding period. **T6 needs more than that — it needs withdrawing and re-running, because
it currently contradicts T3 and points at the opposite engineering decision.**

The document's own best line is the one it should apply to itself: a control that is not
attached to a gate is a comment. A gate whose pass condition is not attached to a number
is a comment too.

---

## How I got it

Branch `w74/gates`, `docs/GATES.md`. All scripts in
`.../scratchpad/verdict/w79/` — `lib.py` (loaders, entry rule, both manufacturing filters),
`grid.py` (the 207-rule grid, declared before running), `d1.py`–`d5.mjs`, `t1.py`, `t2.py`,
`t3.py`, `t45.py`, `t6.py`.

- **Capture corpus:** `STS/data/coins-*.jsonl` (12,205 launches), `tracks-*.jsonl` (5,003).
  Coins assigned to days by launch `t` in UTC, never by filename.
- **History corpus:** `~/Code/flux/data/history/hist-*.jsonl`, seven hour-matched windows,
  2024-10-16 to 2026-08-19, 1,283 launches, ~106k trades. Prices from post-trade
  `vsol/vtok`.
- **Entry:** close of the last candle with `s ≤ 2` (capture) / last trade at `ct ≤ T+2`
  (history). **Costs:** 95 bps a side measured in T1, 0.0001 SOL network, 0.05 SOL size,
  break-even +2.12% gross.
- **Not applied:** `len(outcome.highs) >= 60`. **Applied:** manufacturing-actor removal by
  PDA `BwWK17c…de6s` in `who[]`, with the flow-bound variant reported alongside wherever it
  changes a number.
- **Checker for D5:** `tools/capture/src/check.js` from `w74/gates`, copied to scratch and
  run under node 26. The repo was not modified. No network, no cargo, no trading.
