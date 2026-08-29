# The 27 August 2026 sprint — map of the evidence

This is the register of record for `docs/VERDICT-2026-08-27.md`. The sprint produced
84 analysis reports numbered W00 to W85, a shared brief, three handover documents and
465 files of Python and Node that computed every number in the verdict. **This public
copy carries the index and the two reports the other documents cite by name** —
`W70-capture-policy.md` and `W79-gates-run.md`. The rest is working material and is
kept privately; the table below is the part that matters to a reader, because it says
which numbers were retired and what replaced them.

W81's work is this index, which it rewrote and never got to commit.
**W83 ran and never reported** — 18 files and 809 lines of it, a train/test holdout
re-run with the manufacturing actor removed, last written at 06:33 when the sprint
stopped. Nobody knew it existed until the scripts were swept.

They were written into a temporary scratch directory that could have been wiped at
any moment. They are copied here **verbatim**, including the parts that turned out
to be wrong. Nothing in the reports has been edited. Every correction lives in this
index instead.

---

## What the sprint was

One question: **does STS have a real, tradeable edge after real costs?** On
2026-08-10 the answer had been "not a go" on thin evidence — roughly +0.5% a trade
surviving fees, own-order price impact never measured, the wallet graph adding
nothing. A lot of machinery had been built since and nobody had re-asked.

Roughly 85 passes ran over about twelve hours against the real capture corpus
(no fixtures, no synthetic coins, no network calls). The answer came back **no go,
and not close**: buying a pump.fun launch loses about 10% a trade, loses in every
one of seven hour-matched windows from October 2024 to August 2026, and still loses
0.86% with **every cost set to zero**. The only party making money is the person who
launches the coin.

## The finding that matters most, and it is not about trading

**Thirty load-bearing findings have been retired — the row count of the table
below, which is the register of record for this question. Every single one was
caught by a second pass recomputing the number from the raw files. Not one was
caught by anybody re-reading a report.**

**Fifteen** is the number the verdict, the README, `OVERNIGHT` and the postmortem
quote, and it was correct when they were written: it is the count as the verdict
closed at 06:33. The table has gone on growing since, because the reports that ran
last were verification passes and each one found something. Four of the original
fifteen were the coordinating session's own. Several were published into the verdict
before being caught. Reading a claim carefully, checking that it is
internally consistent, and agreeing that its logic is sound caught **nothing**. What
worked, every time, was someone loading the same files and computing the same figure
with their own code.

Four failure modes recurred and are worth naming, because they will recur again:

1. **A percentile read as a median.** Twice (W12's landing toll; the coordinator's
   same-slot fee figure).
2. **Holding time dressed up as a signal.** Five times (W18's flow lead, the
   entry-time sweep, W26's grid, the selection lead of W56/W62 — where 91% of the
   out-of-sample result turned out to be "this coin will never trade again" — and,
   found last, **inside W26 itself**: the report that named this failure mode ran an
   unmatched comparison of its own, a first-sell rule holding 22.0 seconds against a
   6-second stopwatch, and got the opposite sign to everyone else. If two rules sit
   in the trade for different lengths of time, you are measuring the clock. Match the
   exposure, and put it in the null.
3. **A hygiene filter that quietly deletes the thing being measured.** Twice —
   dropping graduated coins manufactured W30's −62% band result, and the array-cap
   filter below deletes half the real tail.
4. **A null the treatment cannot move.** Once, and it was quoted for hours as the
   reason a lead was closed: W26's flow grid "at the 0th percentile of its own
   null". If the grid contains a rule the shuffle does not touch, the null has a
   floor at the observed value and the percentile is arithmetic rather than
   evidence (W71). **A null can be broken in a way that looks like a finding.**

---

## Do not quote these numbers

If you read one thing in this index, read this table. Several reports carry findings
that were later overturned. The reports still say them, on purpose.

| Number | Where it appears | Status |
|---|---|---|
| **+1.63% gross** "best anywhere" | W9, and verdict revisions 1–2 | **Withdrawn.** A fill artefact — it assumes a trailing stop fills *at* the stop price. Filled at the close of the triggering second the same rule is −1.67%, a 4.32-point swing (W18, W26). A defensible range is −3.06% to −4.29%. |
| **+18.9% perfect-foresight ceiling** | W9 | **Corrected to +12.1%** (W52). W9 struck entry a second late, and one actor was manufacturing half the ceiling. |
| **1,005,000-lamport toll to land** | W12 | **False.** That is the **p90** of the fee distribution read as a median. The median *of trades landing in the same slot as the launch* is **185,000** on the largest sample (n=769) — an earlier, smaller pass got 205,000, and the verdict quotes 185,000 — while the median across **all** trades is 55,000, and the first buyer and the 51st both pay 55,000 (W16, W17, W24). W12 was wrong by about 20x. |
| **"the best coin on the held-out day is 2.71x"** | verdict revision 4, briefly | **Corrected to 3.12x.** The 2.71x was an artefact of the array-cap hygiene filter below, not of any corruption filter. The 3.12x coin has 82 wallets, 156 trades and 36.4 SOL of inflow against the 22.96 its peak required — real by every test in the verdict. |
| **The cleaned tail ladder — "P(5x) 0.21%", "one in 471", "the fall at 5x is 4.3x"** | verdict revision 4, and every report applying the `len(highs) >= 60` filter | **Corrected.** That filter is an array cap, not a truncation guard, and it deletes half of every genuine 5x in the corpus. Cleaned expectancy is **−7.76%, not −9.85%**; P(3x) **1.37%**, P(5x) **0.420%** — a coin 5x's **one time in 238, not one in 471**. It also narrows the confidence interval by 29% and inflates the measured value of exiting early by 2.15 points, so the flow-exit and sell-side grids are provisional. |
| **−62.06% for the $25k–$80k band** | W30 | **Withdrawn.** An artefact of deleting graduated coins, which are 19.8% of the band arm and 1.6% of the baseline (W33). The monotone sweep that was its strongest evidence tracks the graduate share, not the band. |
| **−6.53% gross for the band** (W33's replacement) | W33 | **Superseded.** W33 sold graduates at the graduation cap. W38 measured what a graduate is actually worth 12 hours later: **0.36% of its graduation price**, median giveback −99.64%. Band entry is worse than buying everything after all — W30's direction was right for the wrong reason. |
| **W32's token-balance corroboration** ("100% of coins above 10x show more tokens leaving than were bought") | W32 | **Does not reproduce.** W31 and W43 both get a smooth tail, not a wall; W43 measures 57.1% at 5–10x, not 88.9%. **Cite the SOL-flow version instead** (W40, W46): gross inflow, 10 of the top 10 fail, 22 of 25, 66 of 100, base rate 15.1%. |
| **W32's raw-bytes verification** | W32 | **Withdrawn.** All 30 surviving raw trades have `sol == 0` and `feeBasisPoints == 95` — they are the exact complement of the population under investigation. Zero of the 1,704 zero-fee trades kept a raw (W46). The decoder's innocence rests on `creator` at byte 169 instead. |
| **"1.7M launches/month in Feb 2025 → 2,851 now"** | W3 | **False, off by about 500x.** Actual: 43.9/min in Oct 2024, 37.6/min in Aug 2026 — about 1.6M a month at *both* ends. Volume never collapsed (W25). |
| **"the recorder drops ~90% of launches"** | HANDOFF (~09:00), and briefly the verdict | **False.** 137 of 137 verified launch-for-launch against chain blocks — 62/62 for flux, 75/75 for the STS recorder in its busiest minute (W48). The "90%" was the duty cycle counted twice: 353 events over **10.6 connected minutes** but **100.7 minutes** of wall clock. The captures are essentially complete while connected. They are just **short**. |
| **"the free endpoint drops 0–2.5%"** | W25 | Also false, and struck by the same block-level check (W48). |
| **93.2% on-chain failure rate** | W24 | **Not real — the true rate is 11.3%** (W50: 12,429 of 109,883, counted over whole blocks). The listener's counter overstates by 206–274x, **and the mechanism is still not known.** There is a real and large population of transactions that merely *name* the pump program and die before its code runs — **88.1% of pump-mentioning signatures, about 126 a block** (W48, from a single 1,000-signature call spanning seven slots). It is tempting to make that the explanation, and an earlier draft of this index did. It does not close: 126 a block over 2.46 blocks a second is about **310 failures a second against the listener's 1,426**, and W48 says its 88.1% came from `getSignaturesForAddress`, which indexes on account-key mention — *not* from the `logsSubscribe` filter the listener uses, which W48 describes as an exact server-side match that "either matches or it does not". W48 offered the figure as corroboration of the 93.2%, not as its refutation. W50's own guess is different and it labels it unverified: that the subscription leaks failed transactions **not involving pump.fun at all**, 1,426/s being the right order for chain-wide failures. **Quote the 11.3%. Do not quote a cause.** W24's *conclusion* — the network toll is small and the bankroll can pay it — still stands, but on W16/W17's evidence, not on this. |
| **`tracks` re-bases entry at second 60** | W8 | **False.** `tracks.entry == outcome.entry` on 4,083 of 4,083 joins (W18). This one misread cost the sprint its 12-hour outcome label; **six reports state that anything past 60 seconds is untestable** and it was on disk the whole time. Separately, `tracks.hi` **is** reset at adopt and does not cover the first 60s — `tracks.hi < peakMult` on 1,328 of 4,083 rows (W45). |
| **`open.sellers` reproduces the peak/finish divergence** | HANDOFF (~04:15) | **Withdrawn** by W35 — it does not reproduce, it inverts. The `open.wallets` version of that table stands. |
| **"42% of everything that doubled on the held-out day"** | W44, and the verdict and this index until now | **Superseded — the figure is 86%.** W44 measured the actor's share on the unfiltered 08-21 rows; on the 60-second horizon the sprint actually trades it is far higher (W49). **But the pair that gets quoted does not hold together.** The two shares come from opposite settings of the array-cap filter, and mixing them is what the verdict currently does. With the cap **on**: 86% of doublers and 100% above 3x. With the cap **off**, which is the setting this index says is correct everywhere that does not walk the arrays: **80% of doublers and 94% above 3x** — and the best non-actor coin becomes 3.12x rather than 2.71x, which is the same correction W57 made. The verdict's "86% and 94%" is one from each. **Quote 80% and 94% together, and name the filter.** |
| **"nine of the ten largest peaks fail the money test"** | anywhere the two tests get merged | **A conflation of a retired test with a surviving one.** **9 of the top 10 / 23 of 25 / 79 of 100** is W28's *curve-consistency* check — arithmetic right, diagnosis wrong, and nothing is decoded incorrectly (W32, W46). The money test is the SOL-flow one and it says **10 of the top 10, 22 of 25, 66 of 100, base rate 15.1%** — W46's measurement; W40 supports only the 10 of 10, and the two reports' top-ten lists are not even the same coins. The nine is the dead test wearing the live test's name. If you mean the money test, it is **10 of 10**. |
| **"86% of the doublers and 100% of everything above 3x"** | W49, and the `W44` row and the *42% → 86%* row of this index | **Half of it reproduces. The 86% stands; the 100% does not.** The verdict states the same statistic twice as **94% of everything above 3x — 15 of 16** (L434–435, L512–514); W49 line 31 says 100%. **One missing coin, two documents, and nobody recomputed it.** Quote **86% of the doublers** and quote the 3x share as **94% (15 of 16)** with W49's 100% named beside it. And name the day: this is the **08-21** frame — the verdict enumerates 08-16 separately from "the held-out day" (L585–586) — so it is a statistic about the corpus's worst block, not about the real holdout. |
| **"the SOL-flow test: base rate 15.1%, 61.5% at 5–10x, 88.2% above 10x"** | W40, W46, the *W32 token-balance* row of this table, and `tools/capture/src/check.js:775` | **Two measurements of the same idea, six points apart, and the shipped code now states both.** W70 implemented the test as `solConservation()` (`src/check.js:668`) and measures **9.2% base rate — 557 of 6,055 coins — 51.4% at 5–10x, 75.0% above 10x**, using gross `total.solIn` with a **1% fee slack** on rows carrying their own launch `curve`, which excludes all 3,324 pre-08-16 rows. W46 used gross `total.solIn` **plus `initialBuySol`** on a wider set. W70's commit `906e1e7` left W46's table untouched in the same file, so `check.js` disagrees with itself and with W70's own report. **Nobody has recomputed either.** Name which test and which rows. **The part both agree on is the shape: it rises with the peak, and three-quarters or more of everything above 10x fails it.** |
| **"+4.39 pp out of sample on 08-16" for the sell side** | W64, reproduced exactly by W71 | **Does not reproduce on a rebuild.** W72 gets **+1.57 to +2.13 pp under all four filter combinations** and settles on **+2.04 pp at the 83rd percentile of its own null**, interval crossing zero. W71 reproduced W64's number because it rebuilt W64's construction; W72 rebuilt the question. Quote **+2.04 pp**, and quote it as a direction rather than a size. |
| **"the flow-exit grid sits at the 0th percentile of its own null"** | W26, and the verdict and the handoff for hours | **Withdrawn — this was a broken null, not a data effect.** W26 shuffled exit seconds across coins over a grid that contained a plain 3-second stopwatch, and a stopwatch is *unmoved* by that shuffle: its exit time does not depend on the coin. On W26's own corpus the stopwatch **was** the best rule in the grid, so the shuffled grid's best could never fall below the true best. The null had a floor at the observed value and the 0th percentile is arithmetic, not evidence. Rebuilt like-for-like, the published corpus sits at the **45th percentile** (W71) or the 43rd (W64). **And the gap between 0th and 45th is the grid, not the seed** — W26's null appended eight extra fixed clocks (3, 4, 6, 7, 9, 12, 25 and 40 seconds) before shuffling, and 46% of its null draws sat pinned exactly on the floor. W71's own summary line blames a seed and W71's own working blames the grid; the working is right, and only the 43-versus-45 is implementation. The *reversal* to the 100th percentile is separately real and is a data change rather than a method change (W64, W71). The cap is doing it on its own: with the cap off and the actor still in, the grid is already at the 100th. |
| **+1.76% net, the sell-side cell** | W64 | **Struck.** W64 called it the *first* cell in the sprint above break-even and declined to call it a finding; it is not the only one, because W72 later printed three more out of sample and killed all three under the full standard. At the 2.12% bar rather than the 1.90% one it is +1.54%, not +1.76%. W71 killed it five ways, four of them new: drop the top **two** coins (not three) and it is under the 1.90% round trip; cap any single coin's contribution at a double and it is +1.34% gross, at +50% it is −1.02%; **the median coin in the winning cell loses 1.36%**; its own best-of-240 null on shuffled features clears break-even one time in 27 by W71's count and one time in twenty by W64's — the two reports' nulls for this one cell disagree throughout, W64 giving median/p95/max +0.04/+2.17/+4.82 at the 98th percentile and W71 −0.13/+1.87/+5.30 at the 99th, and nobody reconciled them; and on **112 of its 359 coins no trade printed in the second the exit is marked**, so it is marked to a stale price — those average −4.59% against +7.40% for the ones with a live print. The observation that settles it: **all 359 coins — 100%, not most — also satisfy `open.wallets ≥ 10`.** W71's words are that it is *the many-hands family with a stopwatch bolted on* — the identification with W27's already-dead filter is this index's, not W71's, and the two constructions overlap rather than coincide. And it was then scored on the real holdout, where it does not transfer at all: **−0.14% gross, −2.02% net on 08-16** (n=175), against the +0.43%/−1.47% it published (W72). Every other kill above is in-sample; this one is not. |
| **+5.13 pp for "exit on the first sell"** | W35 | **Corrected twice, and now unresolved rather than established.** Without the array-cap filter it is **+3.64 pp** (W64), reproduced at +3.47 on the training days by W72. On the real holdout (08-16) it lands at **+2.04 pp, 83rd percentile of its own shuffled null, CI [−0.37, +4.20]** — the sign survives in every session, every filter variant and every sell-count rule, it survives latency, and it *grows* when you delete the biggest winners, so it is not a tail artefact. But it stops beating its null and its interval crosses zero. **This is the sprint's strongest surviving effect and one independent capture cannot resolve its size.** Quote the direction, not a number. |
| **"all the real structure in the data is worth 0.4 to 0.9 percentage points"** | W9, the verdict, and `GATES.md` T4 | **The gap does not reproduce and it changes sign.** W9 got a real best of −2.65% against a scrambled null of −2.75% to −3.21%. Run on a 207-rule grid, real scores **−0.51%** and the scrambled corpora **+0.61% to +1.34%** — the scramble beats the real data by **1.86 points** (W79). Different grids, and both chose their own scramble count, which the gate never specifies. The conclusion is identical either way — there is no exploitable time structure — but "0.4 to 0.9 points of real structure" is not a measured quantity. **Quote the direction, not the size.** |
| **"the launch block is the only positive entry window — +9.5%, declining monotonically"** | W36, W39, the verdict, and `GATES.md` T6 | **Contested, and the two sides point at opposite engineering decisions.** W36 measured +9.5% on non-creator rows by entry latency and W39 replicated it at +10.8%. W79's mechanical entry-second sweep makes the launch block the **worst** entry at every hold length — −1.18% at 5s, −9.55% at 60s — against −0.51% to −1.40% entering at second 30, and finds no size at which the trade turns positive. **They are not the same quantity:** W36 and W39 measure realised P&L of real wallets with no fixed exposure; W79 measures a fixed entry-and-hold rule on the history corpus. W79 asserts they cannot both be true without addressing that, and nobody has resolved it. **Quote neither alone.** T6 needs withdrawing and re-running: as written it contradicts T3 three pages above it. |
| **"the selection rule sits at the 99.5–100th percentile of its null"** | W56, reproduced by W62 | **Quote both numbers or you will over-read the first — W62's own instruction.** The 99.5–100th is a correctly built null and a real p-value: a global shuffle genuinely destroys the rule, so it is not vacuous. But give the null one extra piece of information — how many seconds each coin actually traded after entry — and shuffle the returns inside those strata, and in-sample it falls to the **92.5th** (W62) and on the real holdout to the **58th**, a coin flip. **91% of the out-of-sample lift is "this coin will never trade again" and nothing else** (W72). Out of sample it sits at the **86th** percentile of even the plain global shuffle — which is a loss of power rather than a weak null, 293 selected coins inside a 1,098-coin block; the global shuffle still destroys the rule there, median −0.74 pp against an observed +3.34. Two cautions W62 attaches and nobody has repeated: the exposure null conditions on an outcome variable, so it **cannot** be used as a p-value for a tradeable rule — it measures how much of the lift is composition, and that is all;. The mechanism was always the finding; now it is the whole finding. |
| **"+3.31% if you knew the coin would keep trading"** | W56 | **Retired by its own verifier, in terms.** W62 rebuilt it and got **−8.49%** (n=110) under one protocol and **+12.69%** (n=44) under another — a 21-point swing on a protocol choice at n≈50. W62's words: *"it should not be quoted as a number."* W56 still prints it, and still concludes from it that decision-time selection makes money if you knew a coin would trade for ten seconds. It does not survive. |
| **−5.7 points, band entry against buying everything** | W38 | **Not significant** — 95% CI [−24.1, +13.6] on n=86; a mean of these returns is not a measurement. The band conclusion holds, but the statistic to quote is the one a fixed stake actually experiences: **band entry compounds at −58.62% a trade against −29.52% for buying everything, gap −29.1 points, CI [−39.1, −16.5]**, which excludes zero (W59). |
| **"08-21 is the held-out day"** | `BRIEF.md` rule 2, and every report that says "held out" | **False — and there is now a real one.** The much-quoted **611.197 s** is the gap at the *file* boundary, and both of its endpoints launched on 08-20 — the last row of the 08-20 file and the first row of the 08-21 file are ten minutes apart on the same day. The gap at the true **launch-day** boundary is **478.9 s**, about eight minutes. Whichever edge you cut on the buffer is eight to ten minutes, not the hundred that was argued about. And **one process (pid 11679) spans it, 14.98 h, zero restarts**, logging its own 99.9-minute socket outage — W73 found the listener had recorded it itself, as a `gap` of 5,993,856 ms, which is new evidence rather than a confirmation of W68 (W68, and W73 which reproduced its pid span to the millisecond and went further). 08-21 is also the corpus's *worst* block: 46.8% truncated, 48.4% dead, 3 usable coins across hours 01–08. **The real holdout is 08-16** — 1,505 clean coins behind **3.78 days** of total silence and **four** recorder processes on 08-16 itself, five counting 08-15 on the near side and two on the far side — W68's "five and two" is right as it stands, and the version that misleads is the compressed one that makes it sound as though five processes recorded 08-16 — but only for `open` / `social` / `curve` / `initialBuy` and the `outcome` label. On 08-16 the `funding` block is populated on **4.3%** of coins against 77–87% elsewhere, and `tracks` barely exists, so nothing built on those is held out by it (W73). |
| **"38 of 40 gates are satisfiable by an engine agreeing with itself"** | W61, and the postmortem's first draft | **Right in the tables, overstated in the summary line.** The precise split is **33 offline / 4 live-but-self-measured / 3 market-facing**; four of the 38 need a live stream for 24 h, 72 h or 14 days and cannot be passed offline at all. The honest item count is **37 of 40 never test the thesis, 2 are distinct market tests, 1 of those precedes capital** (W69). W61's actual claim — that they pass without the system ever being *right about the market* — is correct, and W69 strengthens the conclusion: the roadmap's own annotation says there was no valid held-out day, so **the count of runnable pre-capital thesis gates on this corpus was zero, not one.** |

## The framing documents also carry errors, preserved

- **`BRIEF.md`** is what every pass was told. Its **rule 2 is false**: it names
  08-21 as a held-out day. W11 showed there is no holdout — 08-21 is the unbroken
  tail of the 08-20 run past midnight, and the real unit of this data is the capture
  session (9 of them, 6 usable). Everything in the sprint that says "held out" is
  reading a UTC midnight file split as a day boundary.
- **`HANDOFF-log.md`** is the running log the night was actually coordinated from,
  and it is where the biased hygiene filter was issued — line 55, in the ground-truth
  block every pass was handed: *"drop `len(outcome.highs) >= 60`"*, as a truncation
  guard. **It is an array cap, not a truncation label.** A coin fills that
  array precisely by moving a lot, so the filter drops 0.9% of all coins but about
  **53% of the real tail**. Expected direction is conservative — deleting real
  winners makes the results look worse, not better — but it was never fully audited.
  At least nine reports apply it: W00, W8, W26, W33, W36, W39, W41, W49, W51, W52.
  It is also the best single narrative of the night, read top to bottom — and it is
  the **stale original**, in the same sense `OVERNIGHT.md` is. W77 replaced it with
  the 276-line document now at `HANDOFF.md`; the log is kept because the instruction
  above is on the record nowhere else. **Read `HANDOFF.md`; cite `HANDOFF-log.md`.**
- **`OVERNIGHT.md`** in this directory is the **stale original**. Five of its headline
  claims were overturned before morning. W54 rewrote it; the current version is
  `docs/OVERNIGHT-2026-08-27.md` in the repo. Read that one, not this.

---

## How to read the status column

| Status | Meaning |
|---|---|
| **Confirmed** | A second pass rebuilt the number from the raw files and got it. |
| **Corrected** | The finding moved. The replacement is named. |
| **Withdrawn** | The headline is not true. Do not quote it. |
| **Single-source** | Nobody ever rechecked it. Not wrong — unverified. |

Numbering was chronological and tells you nothing, so the reports are grouped by
what they were asking.

---

## 1. The base rate — what buying a launch is worth

The spine of the verdict. Everything here points the same way.

| Report | The question | What it found | Status |
|---|---|---|---|
| `W2-baseline-expectancy.md` | Does any simple exit rule make money on the raw outcome distribution? | No. Zero of 60 rules positive after fees; best loses 1.8% a trade. | **Confirmed** in direction by W9, W34, W45, W25, W49. Two of its supports were struck: it prices W12's dead 1,005,000-lamport toll, and its "+1.7% on the held-out day" rests on a holdout that does not exist (W11) and sits at the 68th percentile of noise anyway. |
| `W9-exit-simulation.md` | Simulate 108 exit rules on the real second-by-second path. | All 108 lose; best −2.65% net. Perfect foresight +18.9%. | **Corrected.** The direction stands and was never challenged. Its best-rule figure assumes stops fill at the stop price (withdrawn, see above) and its ceiling is really +12.1% (W52). |
| `W20-raw-events-inventory.md` | Is the 60-second window hiding the upside? | No. Extending to 300s changes nothing: 31.6% hit 1.5x within 60s and 31.6% within 300s, identical to the decimal. Holding longer is worse. | **Confirmed** at a longer horizon still by W34/W45. |
| `W34-twelve-hour-exits.md` | Grade exits over the full 12 hours. | All 150 rules lose. Best is −3.38% net, 3.45 points short of break-even. 66% of 3x events land after second 60, but only 1.7% of coins ever reach 3x. | **Confirmed** by W45 to 0.01 points. |
| `W45-twelve-hour-verify.md` | Rebuild W34 from scratch. | Same numbers to two decimals. Deleting graduated coins is *not* doing the work here (0.08 points). | Verification pass — **confirms W34**, on the same data. **Neither has an out-of-sample test**: `tracks` on the real holdout is 82 rows from a five-minute run, so every 12-hour result in this sprint rests on 08-20 and 08-21 alone. W72 calls that **the single largest untested claim** left standing, and it is right. |
| `W40-early-days.md` | Were 08-10..08-15 really better? | No. The 6-point advantage vanishes on the only hours both eras cover (+0.3pp) and most of it was two coins with arithmetically impossible prices. | **Single-source**, but it also supplied the SOL-flow corruption test that W46 later adopted. |
| `W25-history-backfill.md` | When did the edge die? | **It never lived.** Seven hour-matched windows, Oct 2024 → Aug 2026, expectancy negative in every one: −6.5% to −17.1%, mean −10.1%. Uncorrelated with volume, competition or time. | **The single most important result of the sprint.** Headline stands. Two side claims corrected: the 1.7M-launches figure (false) and the endpoint drop rate (struck by W48). |
| `W49-clean-corpus.md` | Re-run the headline with the manufactured coins removed. | The headline does not move (−9.84% → −9.85%). The tail collapses: P(2x) 6.8% → 3.5%, biggest peak 34.1x → 11.7x, and the confidence interval halves. | **Single-source**, consistent with W44/W46/W52. |
| `W00-zero-fee-ceiling.md` | Where did "at zero fees the best rule still loses 0.86%" come from? | Nowhere — W41 was right that it appeared in no report. This file is the missing source, plus a re-run on cleaned data. | Written to close a gap W41 found. **Single-source by construction.** |
| `W57-tail-verify.md` | Rebuild W49's tail collapse from scratch — own pipeline, own actor set. | Every cell reproduces: 12,205 launches, 7,971 after hygiene, 1,376 the actor's, all five tail probabilities, 34.08x → 11.66x. **The collapse is real and it is large.** But it is measured through a filter that deletes genuine movers from the clean arm only — **half of the real 5x coins in seven days.** | Verification pass — **confirms W49's table, corrects two of its most-quoted numbers.** Traced the array cap to the recorder's source (`watch.js:240`), not by inference. |
| `W55-filter-bias.md` | How far does the `len(highs) >= 60` hygiene rule reach? | Everywhere, and **it never had a hygiene basis at all** — on the 69 capped coins `peakMult` agrees with the candles to 7 parts in a million. Pooled expectancy is **−7.76%, not −9.85%**; P(3x) 1.37%, P(5x) 0.420%; the CI is 29% too narrow; the measured value of exiting early is inflated 2.15 points. Nothing changes sign. | **Confirmed to the digit** by W65. The correction it issued is the largest single number change of the sprint. |
| `W65-filter-verify.md` | Rebuild W55 independently. | All five headline corrections reproduce exactly. Adds the decisive test W55 did not run: on **65 of the 69** capped coins `peakAtSec` is *later* than the last second the array records, and `highs[-1]` disagrees with `peakMult` on 66 of 69. The outcome fields keep tracking past the array's end. **There was never a defect to filter for.** | Verification pass — **confirms W55** and makes its case stronger than W55 made it. |
| `W72-real-holdout.md` | Score every surviving claim on the real holdout, 08-16. | **No go, unchanged — and for the first time on a genuinely independent capture.** Base rate survives at **−10.41% ±4.18** (n=1,505), −7.10% untruncated and actor-removed. Of six claims, two change and **both change toward "less there than we thought"**. No filter beats buying everything under the full standard. | **The sprint's only out-of-sample test that was actually run.** Single-source by construction. Two qualifications it makes about itself and that should travel with it: 08-16 *precedes* the training days, so this is a cross-capture holdout and **not a walk-forward** — W68 names the reverse split as the only genuine walk-forward available, and nobody ran it; and 1,389 clean untruncated coins give a 95% band of about ±4.2 points, so a three-point effect is not resolvable there. One block is one draw. |

## 2. Signals — everything tested for direction

Every signal in this project detects **volatility, not direction**. That is the whole
case, and it survived every attempt to break it.

| Report | The question | What it found | Status |
|---|---|---|---|
| `W3-wallet-count-signal.md` | Re-test the famous 3-second wallet-count ladder. | Real but a fifth the believed size, and it does not make money. The original ladder was computed on Dune history for Feb 2025, never on this project's data. | **Partly false** — its "1.7M → 2,851 launches/month" claim is off by ~500x (W25). The signal result itself was never rechecked. |
| `W4-wallet-graph.md` | Third test of the wallet graph. | "The graph has real signal. It is worth nothing." Known-active early buyers separate 2x from not-2x by about 25x, and it replicates. | **Single-source.** Its P(2x) figures were computed before the manufactured coins were identified; W49 halves that base rate. Nobody re-ran it on the clean corpus. |
| `W5-social-signal.md` | Does the social data predict a pump? | Yes, strongly and monotonically (`nth`, `tweetAgeSec`). Worth negative money. | **Single-source**, same caveat as W4 about the pre-clean base rate. |
| `W10-order-flow.md` | Does the shape of order flow in the first seconds predict what happens next? | No. Best of 1,512 combinations is inside the label-shuffled null band. Add two seconds of realistic delay and everything is negative. | **Confirmed** in spirit by W26. |
| `W26-flow-exits.md` | Are flow-driven exits the missing edge? | No — and it kills W18's 1.8-point lead instructively: that was a **holding-time** effect (3.9s vs 32.7s in the trade). It then closed the door with two sentences, **and both of them are withdrawn** — flow "level with a stopwatch at −0.06pp" and "the whole grid at the 0th percentile of its null". See the two rows for them above; do not quote either from this report. | **Half confirmed, half withdrawn.** The money conclusion — flow exits do not pay — stands and was confirmed out of sample by W72. The two closing sentences were killed by W64 and W71: the percentile was a broken null, and against a properly matched clock flow is **+0.75pp**, not −0.06. W71 found W26's stopwatch sentence wrong in *both* directions on W26's own data. Note W41: W26 published −2.27%, the verdict carried −1.67%, and four reasonable implementations give −3.06% to −4.29%. The sign is solid; the magnitude was flattered. |
| `W27-many-hands.md` | Does the "10+ buyers in 3 seconds, price still near the floor" filter work? | No, killed five separate ways. It was never "many hands" (it counts buys, not buyers); the buyers are a bot fleet (median early buyer appears in 52 other launches); one coin is 52% of P&L; it inverts out of sample; 87th percentile of noise. | **Stands.** W51 later explains the mechanism. |
| `W35-sell-side.md` | Who sells, when, and is it worth anything? | "Exit on the first sell" beats a matched stopwatch by **+5.13pp** — the largest honest effect anyone produced, 100th percentile of 300 shuffles, survives 2s lag. But the best of 248 cells is still −1.47% net. | **Corrected on size, refuted out of sample.** Without the array cap the edge is **+3.64 pp**, not +5.13 (W64), and on the real holdout **+2.04 pp at the 83rd percentile** of its own null, interval crossing zero (W72). Its published best cell does not transfer at all: **−0.14% gross, −2.02% net on 08-16** (n=175). W35 and W64/W71 also disagree on the grid size — 248 cells against 240 — in reports that claim to reproduce each other cell for cell, and nobody resolved it. Self-withdrew `open.sellers` as a corroborator of the peak/finish table, which stands. |
| `W51-real-tail.md` | Is the real tail decidable at second 3? | No. A genuine 3x is a **40-second climb, not a 3-second spike** — median peak at second 39, and **zero of 86 peaked at or before second 3**. Seven early features, every AUC below 0.5 — they are *anti*-predictive. Best of 433 cells: +0.09% net at the 29th percentile of its null. | **Confirms W46's tail finding exactly**, cell for cell. Also reconciles W27's −2.3% contradiction: early hands buy the "market" half and cost you the "3x" half. |

| `W56-selection.md` | The one lead that ever improved a real number — what is it actually selecting? | It is real, it survives every test the sprint uses to kill things — and **it is worth nothing, because the thing it selects is coins that never trade again.** Median kept coin: **1 buyer, 1 trade, 0.01 SOL, zero candles after entry.** 70% have a completely flat price path and **69% return exactly −1.89%: the round trip and nothing else.** The improvement is not finding winners, it is buying things nobody wants so nothing happens while you hold. | **Confirmed** by W62 on both mechanism and conclusion; **demoted** by W72 on the holdout. Its asymptote argument is the durable part: a perfect dead-coin detector returns **−1.90% net**, and not trading returns 0.00%. |
| `W62-selection-verify.md` | Rebuild W56 with different features and different bucket edges. | Reproduces on every split protocol and every fold (1,046 cells against W56's 1,109, nothing shared but the data). The mechanism proof in five rows: **tighten the selection and the improvement and the dead-coin share move together; ask for a positive number and both vanish at once.** Its "confirmed on the one genuinely held-out day" is **struck**: that day was 08-21, which W68 and W73 both showed is not a holdout — and is the *least* suitable one for this particular finding, because its broken half manufactures exactly the dead coins the rule selects. The nearest thing to a real-holdout figure is W72's, on a different population and worth quoting as such: of the 293 coins its rule selects on 08-16, **53.9% have zero candles after entry and 58.0% return exactly the fee**, against a 24.2% base rate in that block. Four denominators are now in play for one statistic — W56's 1,677 in-sample at 69%, W62's own-edges 1,728 at 59.7%, W62's 429 on 08-21 at 69.9%, and W72's 293 on 08-16 at 58.0% — so quote the population every time. The coins are genuinely dead rather than a recording gap (98.9% have `total.trades == open.trades`) — though W62 says plainly that truncation and the dead pool are entangled, and burst-truncated coins do carry a higher dead share. | Verification pass — **confirms W56**, corrects its null (see the do-not-quote table) and two smaller claims. |
| `W64-exits-unfiltered.md` | Re-run W26 and W35 with the array cap turned off **and the manufacturing actor removed** — a third of the move is the actor alone. | **One conclusion survives, one breaks in half.** Flow exits still do not pay — best is −1.50% net, 1.5 points short — but **both sentences W26 used to close the door fail**: the grid is no longer at the 0th percentile and flow is no longer level with a stopwatch. The first-sell edge is +3.64 pp, not +5.13. And the best sell-side cell reaches **+1.76% net**, which W64 itself refuses to call a finding. | **Confirmed on the reversal, corrected on the cell**, by W71 — and then scored out of sample by W72, which confirms the flow reversal on 08-16 (100th percentile again) and kills the +1.76% cell outright. Its one-liner is the right one: *nothing became tradeable, but "closed" was the wrong word for both.* |
| `W71-exit-reversal-verify.md` | Did the two exit conclusions really reverse? | **Yes on the inversion, no on the cell.** Every load-bearing W64 number reproduces; the null percentiles differ by one or two points, so W71's own "reproduces to the second decimal" is a little generous to itself. Diagnoses W26's 0th percentile as arithmetic rather than evidence — like-for-like it is the **45th** — and confirms the reversal is a data change, not a method change. Then kills the +1.76% cell five ways, ending with the one that settles it: **100% of it is also the dead many-hands filter.** | Verification pass — **confirms W64's reversal, strikes its cell**, and is harder on W26 than W64 was. |

**The epitaph, from W51:** perfect foresight on those 86 coins pays +225.6% over 60
seconds. The information arrives around second 20. You must commit at second 3.

## 3. The stated thesis — the "$25k–$80k difficult middle"

The README's own claim, tested last, and the most tangled thread in the sprint.
Read all three in order or you will quote the wrong number.

| Report | The question | What it found | Status |
|---|---|---|---|
| `W30-middle-of-market.md` | Is the README's stated market band testable, and does it work? | Half testable. Band entry loses **−62.06%**, 48 points worse than buying everything. Structurally: the "middle" is a **moment, not a phase** — median 89 seconds from launch to $25k, 3 of 53 coins still above it at 12h. | **Headline withdrawn** (W33). The structural finding is unaffected and is the durable part. |
| `W33-band-verify.md` | Adversarially recheck W30. | Reproduced W30 to the decimal, then found the mechanism: **deleting graduated coins was doing 45 of the 48 points.** Corrected to −6.53% gross / −8.43% net, n=86, p=0.578 — loses, but thin. Named the open question: what is a graduate worth after migration? | **Mechanism confirmed** (the artefact is real). **Its own replacement number is superseded by W38.** |
| `W38-post-migration.md` | What is a graduate actually worth 12 hours later? | **0.36% of its graduation price.** Median giveback −99.64%, n=65. The smallest possible giveback, which happens automatically to a coin with no trading at all, is 20.71% — **median** −20.71%, mean −18.34%, with 2 of the 54 pools read on chain getting a materially better deal. W59's own phrasing is the one to use: *93% of migrations, to the decimal*, not "every one". W33 needed only 9%. | **Decisive, and now verified.** Band entry is worse than buying everything. W30's direction was right; both of its predecessors' numbers were wrong. Its own −5.7-point gap is not significant — see W59. |
| `W59-band-reverify.md` | Verify W38 and close the band question. | **The token side of the 20.71% is exact arithmetic; the SOL side is an on-chain read, and it is a distribution.** 65 of 65 graduates have a PumpSwap pool; **50 of the 54 creation transactions read on chain deposit exactly 206,900,000 tokens and 67.405853768 SOL**, and 206,900,000 is `supply − realTokens`, a number every coin carries in its own record. But a mean of 86 returns is not a measurement (intervals 38 to 450 points wide). What a €200 bankroll actually experiences is compounding: **−58.62% a trade against −29.52%, gap −29.1 points, CI [−39.1, −16.5].** | Verification pass — **confirms W38's constant, replaces its statistic, confirms W33's structural finding.** Closes the most tangled thread in the sprint. |

## 4. Costs — what it costs to get in and out

The only wall of the three that was claimed and then demolished. It was demolished
twice, by different passes, for different reasons.

| Report | The question | What it found | Status |
|---|---|---|---|
| `W1-price-impact.md` | How much does our own order move the price? | **0.04% at 0.1 SOL.** The 2026-08-10 fear was wrong. On a constant-product curve an instant round trip walks up and back down the same path — the impact cancels *exactly*, and only the fees remain. | **Stands**, and the argument is structural rather than statistical. The ~22% landing cost it endorsed from W12 is dead. |
| `W12-landing-cost.md` | What does it cost just to land a transaction? | 1,005,000 lamports to land in the launch slot — 22.1% of a small order. This became "Wall 3": the bankroll cannot pay the toll. | **False.** A p90 read as a median. See the do-not-quote table. Preserved because the whole "Wall 3" argument was built on it and the verdict spent hours removing it. |
| `W16-landing-cost-corrected.md` | Recompute W12. | **Being first does not cost extra.** First buyer 55,000 lamports, 51st-and-later 55,000 — flat. A realistic round trip is 108,000 lamports, 1.08% of a 0.01 SOL order, not 22.1%. Minimum viable order 0.011–0.041 SOL, not 0.25–0.4. | **Confirmed** independently by W17, W24 and W18, then turned from a 25-sample estimate into an 11,085-signature measurement by the overnight backfill. |
| `W17-fee-buys-position.md` | Does paying a bigger priority fee buy you position? | No. Within the first 25 slots — the only slots anyone fights over — the higher fee lands earlier **50.3%** of the time over 76,795 paired comparisons. A coin flip. | **Confirmed**, adopted as ground truth. |
| `W24-failure-rate.md` | What does the 93% on-chain failure rate cost us? | Wall 3 falls. The effective network cost per landed trade is 60,000–320,000 lamports, against pump.fun's flat 2% round trip — the network toll is the small item on the bill. | **Conclusion confirmed, premise false.** The 93.2% is a listener artefact (W50). |
| `W50-worldb-verify.md` | Does the launch-slot argument hold? | Wall 3 stays struck, but **not on W24's argument** — the 93.2% failure rate is not real. Full blocks give **11.3%**. The listener's failure counter overstates by 206–274x, while its success counts are accurate to within 7%. | Verification pass — **corrects W24's premise, keeps its conclusion**. |
| `W6-execution-realism.md` | Could we actually get the fills the backtest assumes? | **`outcome.entry` is not the launch price** — it is the price after the 3-second observation window closes, reproduced from the bonding curve to one part in a million. Every expectancy number in the sprint is therefore already paying for the opening move. | **Confirmed** by W52, which pins entry at exactly 3.0 seconds. Adopted as ground truth. |

## 5. The manufactured tail — who is on the other side

The sprint's strangest thread: a large share of the big multiples in this dataset
were printed by one operator's program, and the first three attempts to explain it
were each partly wrong.

| Report | The question | What it found | Status |
|---|---|---|---|
| `W28-curve-variants.md` | Is the tail a different product? | The `virtualSol: 4.292` variant is genuine but carries no outcome at all. Separately, it reports that 18.4% of coins fail a curve-consistency check — **a count W49 later re-ran with W28's own code on W28's own file and could not reproduce, getting 2,130 coins rather than 1,083, i.e. about 36%** — — and **9 of the top 10, 23 of the top 25 and 79 of the top 100 peaks fail it**. Diagnosed as a broken price field. | **Arithmetic right, diagnosis wrong.** Nothing is decoded wrong (W32, W46). Its warning that W21's check C17 is written wrong is correct. |
| `W32-corruption-root-cause.md` | Why is the SOL reserve wrong, and can the data be saved? | Nothing is decoded wrong. The "impossible" reserve is the live pool state — predicting the next trade's token amount from it matches 99.5%, versus 2.9% from the launch curve. | **Core confirmed** by W46's clean-room rebuild (99.4% vs 2.9%, four further windows). **Two supports withdrawn**: the raw-bytes evidence and the token-balance corroboration. Its "`fee == 0` is a law" is also false. |
| `W46-corruption-verify.md` | Independent check of W32. | The load-bearing claim reproduces; two of three supporting arguments do not. **And the good news:** 86 coins peaked above 3x with 50+ distinct wallets and **not one** fails the SOL test or contains the actor's wallet. There is a real tail, and wallet count separates it better than any curve test. | Verification pass. Its tail finding was **confirmed exactly** by W51. |
| `W44-zero-fee-actor.md` | Who is the zero-fee actor? | **A program, not a wallet.** `BwWK17cb…` is off the ed25519 curve — a PDA. One ordinary wallet signed all 1,704 zero-fee transactions. It rescales the quoted SOL reserve in proportion to real SOL held (median 55x). Ordinary buyers are **not robbed** — about +1.7% across them, market-maker economics. But on the 08-21 file it supplies **86% of everything that doubled and 100% of everything above 3x**, and is in 10 of the 10 biggest peaks. | **Single-source on the mechanism**, heavily consistent with W46, W49 and W52. **Its own 42% figure is superseded:** on the hygiene-filtered corpus the share is 86% (W49). Quote 86%. |
| `W36-who-profits.md` | Does anyone make money here? | **Yes — the person who launches the coin.** Creators staked 13,130 SOL for **+28.8%**; everyone else staked 62,235 for **−8.1%**. Where the creator dumps inside 60s (66.6% of launches): **+39.2% on stake, 60% win rate**, replicating all three days. Per 100 SOL a non-creator stakes, 1.7 goes to pump.fun and **6.5 to the creator**. Winners persist — but only creators. | **Confirmed** by W39 to a rounding difference on every headline. |
| `W39-creator-verify.md` | Rebuild W36 without touching its pipeline. | Same numbers. Creators +29.5% vs W36's +28.8%; the creator-dump figure lands on **+39.23%** against +39.2%; the by-day replication is identical to the decimal. | Verification pass — **confirms W36**. |

The best single line out of this section: **the downward drift measured everywhere
else in this sprint is not friction. It is somebody's income.**

## 6. Data integrity — can this data answer the question at all

| Report | The question | What it found | Status |
|---|---|---|---|
| `W8-data-trust.md` | Can we trust this data? | `outcome` is trustworthy — computed from the complete trade stream, cross-checked against the independent candle series on 6,966 coins with exactly one disagreement. | **Half confirmed, half false.** The `outcome` audit stands and is load-bearing. The claim that `tracks` re-bases entry at second 60 is false and cost the sprint its 12-hour label for most of the night (W18). |
| `W11-holdout-validity.md` | Is there a valid holdout? | **No, and there never was.** Calendar days are not the unit — capture sessions are, and there are 9, of which 6 are usable. Between-session standard deviation is 4.78pp; six sessions give ±3.8pp. You need ~10 sessions for ±3pp and ~20 for a powered test. | **Confirmed** and adopted as ground truth. It invalidates rule 2 of the brief every other pass worked under. |
| `W18-red-team.md` | What else is wrong? | Four things broke, and all four held: the 12-hour label existed all along; W12's toll is wrong; W2's and W9's only positive numbers assume stops fill at the stop price (worth 2.0–4.9 points); W9 mislabels its own best rule. | **Confirmed** on all four. Its own two leads — flow exits and many hands — were both killed within hours (W26, W27). |
| `W41-consistency-audit.md` | Does the verdict agree with itself? | No. It still told readers to "finish the two leads — the only paths to break-even" four sections after both were killed; still carried the withdrawn −62% band figure; still pointed at the wrong producer tag; and its single headline number was **not reproducible from any report**. | **Acted on.** W41 was given write access and fixed the document. This report is the reason the verdict reached revision 4. |
| `W48-capture-method.md` | Should the recorder sweep blocks instead of listening? | No — and the 90% drop claim is struck. **137 of 137 launches captured**, verified block by block on both listeners. Block sweeping was slower than the chain in all seven windows — **1.88x over the seven together**, and 1.06x to 2.98x window by window — and would need ~3 TB/day. Socket latency median 1.63s vs 4–9s by sweep. | **Strikes a claim that had itself replaced a wrong claim.** Also corrected W25's call-count comparison — which was wrong **against** W25, not in its favour: whole-block sweeping costs about **143x** fewer calls than listing signatures and fetching each one, where W25 claimed only 17x. Its 88.1% is widely read as the explanation of the fake 93.2%; it is not, and the row above says why. |
| `W68-holdout-settled.md` | Settle the 08-21 holdout dispute — is the gap 611 seconds or 100 minutes? | **Both numbers are real and they straddle a single coin.** RAMEN, `Cuw4UoDxoPm1`, is the last row of the 08-20 file and the first coin of W62's session 3: the gap before it is 99.92 min, the gap after it is 611.2 s. So 08-21 is not a holdout — and the best available split is a different day entirely, **08-16, behind 3.78 days of silence and five recorder processes, 1,505 clean coins.** It rescues nothing: the result that survives there is still negative. | **Confirmed to the millisecond** by W73. Ends a dispute that had two passes each correctly measuring a different edge of the same coin. |
| `W73-holdout-verify.md` | Check W68 independently. | All six claims reproduce, and the process-identity evidence is **stronger than W68 stated** — the listener logged its own 99.9-minute outage as a `gap` of 5,993,856 ms, and the seven pids in the audit files are each perfectly contiguous in global time order. **New and serious:** on 08-16 the `funding` block is populated on **79 of 1,822 coins (4.3%)** against 77% on 08-20 and 87% on 08-21, all from one 5-minute run. 08-16 is a valid holdout for `open`/`social`/`curve`/`initialBuy` and the `outcome` label — **and not for anything built on `funding` or `tracks`.** Two further caveats it names, carried nowhere else: **81.2% of 08-16's clean coins share at least one wallet with the training days** and 23.3% share a creator, the manufacturing actor sitting on both sides — so a holdout across four days tests **stability over time, not an independent population**; and the two blocks are not the same market, median `open.solIn` being **0.60 SOL on 08-16 against 2.67**, dead-coin rate 19.8% against 30.6%. | Verification pass — **confirms W68 and adds three caveats it did not name.** The scoping one is what anyone using the holdout hits first; the other two change how a *failure* on it should be read, since a rule that dies there may be meeting a different market rather than overfitting. |

## 7. The recorder — the thing that produced the corpus

| Report | The question | What it found | Status |
|---|---|---|---|
| `W21-capture-spec.md` | What must the next capture do differently? | Four things before anyone presses record: every record must say how long it was really watched; must carry a session id and a heartbeat; must carry slot, signature and fee; must never split at UTC midnight — that split is the entire reason a 15-hour run was mistaken for a tuning day plus a holdout. | Largely **implemented** by W23, W29, W31, W43. Its check **C17 is written wrong** (W28) — do not filter on `virtualSol: 4.292`. |
| `W23-capture-fixes.md` | Fix the recorder before the next recording. | All seven reported defects are real; five fixed in flux; tests 15 → 49. The alarming one was not on the list: **`flux stats` reported 100.00% uptime for a listener that had run 0.41% of the span** — it counted in-process reconnect gaps only, never the time between runs. | **Stands.** That bug is why nobody noticed the duty-cycling, and it is the root of the "90% drop" confusion W48 later untangled. |
| `W29-producer-rehome.md` | Give the recorder a home. | Rehomed to `tools/capture/` on `main`, 15 → 97 tests. **And the preservation tag was pointing at the wrong commit** — `capture-producer`/`eaaf1b4` is the stash's *index* commit, 585 lines, and it **does not parse**. The program that wrote the corpus is `373825a` (682 lines). | **Corrects an earlier HANDOFF entry.** The corpus's producer is now correctly preserved. |
| `W31-recorder-finish.md` | Close the remaining defects. | All defects closed plus two nobody reported; 211 tests. The sharpest lesson of the night: **the decoder had been reading `realSolReserves` since it was written — `watch.js` simply never wrote them.** Not missing from the wire, not missing from the parser: in memory, discarded. | **Stands.** Also contradicted W32's token-balance test, correctly. |
| `W43-capture-raw-state.md` | Keep the state, not the reduction of it. | Both fields W32 asked for plus four more that were on the wire and being thrown away; `realSolReserves` verified at byte 105. 243 tests. | **Stands**, and corroborates W31 against W32: the curve test catches 57.1% at 5–10x, not 88.9%. |
| `W60-capture-review.md` | Two hands fixed the recorder in the same window — was anything lost in the interleave? | **Nothing was lost.** 243 tests, every measured number in W31 and W43 reproduces, the old corpus still reads. But the seam left **one mistake in four places, always the same one: a field added by one hand is not covered by a rule written by the other.** Probed rather than eyeballed — corrupt one field at a time and ask the checker: **17 of 31 corruptions passed `capture check` unnoticed**, including both of the night's headline fields. All six defects fixed. **257 tests, 0 failures.** | **Stands**, and is the sharpest instance of the night's own failure mode surviving its own repair: two passes fixed a recorder that could not detect its defects, and a third had to test the *checker*. |
| `W70-capture-policy.md` | Two policy calls left open: bump the schema version, and may a check that fails 5% of ordinary coins be allowed to fail a row? | **Both settled by measurement, and the second overrules the brief.** `SCHEMA` goes to **3**, stamped on every record type readable on its own, and `capture check` now refuses a version it does not know. Pointed at the recorder's own live output the new rule **immediately failed it** — coin rows were stamped and `tick`/`gap`/`failagg`/`stop` rows were not, so a live session file held "v3" and "no version" at once. `curveConservation` keeps gating, because **the 5% is not good data**: across 6,072 gradable coins **1,298 land within 0.05% of exactly 1.0 and then there is nothing at all between 1.0005 and 1.005**. A threshold set too tight is *densest* just past itself; this one is emptiest there. Three independent confirmations: **the median failing coin needs 4.3x more tokens than were ever bought**, and widening the tolerance from 0.1% to 50% moves the base rate only 5.0% → 4.3%; **18 of 250 creators account for 195 of the 465 failures while 198 fail none of their 2,383 coins**, and dropped websocket messages are not creator-selective; failing coins have **more** trades (median 19 vs 9) on less money. The SOL-flow test a reviewer had declined is implemented and grades — **the stated reason for leaving it out was checkable and wrong: not one of the 3,324 pre-08-16 rows carries a `curve` block, so no old-era price can reach the arithmetic.** **285 tests, 0 failures**, commit `906e1e7`. | **Single-source, and it re-derived no expectancy.** Verified in the tree: `SCHEMA = 3` and the refuse-unknown-version logic at `tools/capture/src/session.js:58` and `:69–82`; `solConservation()` at `src/check.js:668` with `FEE_SLACK = 0.01` at `:625`; **285 test declarations counted across the six `test/*.test.js` files** — but `tools/capture/README.md:697` still says "257 tests", left stale by W70's own commit. **Its SOL-flow rates contradict W40/W46 (9.2% vs 15.1%) and W70 left W46's table in the same file at `src/check.js:775`, so `check.js` now states both** — see the do-not-quote table. It also carries two base rates for `curveConservation` and never reconciles them: 5.0% in the tolerance argument, 7.7% in the grading table; **465 of 6,072 is 7.66%**, so 7.7% is the one its own counts support. Nothing here could be recomputed — `data/` is not in the tree. |

## 8. Salvage, repo hygiene, and the documents

| Report | The question | What it found | Status |
|---|---|---|---|
| `W7-build-and-merge.md` | Land the branches; get an expectancy number out of the engine. | Branches landed, tree green. **The engine has never produced a paper trade because there is no paper mode** — `OperatingMode::Paper` is never constructed outside `#[cfg(test)]`. And the backtester has **no exit rule**: it prices exits already in the stream it reads. | **Confirmed and deepened** by W42 and W47. |
| `W13-wip-review.md` | Is the rescued WIP worth landing? | Yes — merged; not a stray file but an extension of a module already on main. 1,605 tests. | **Single-source**, housekeeping. |
| `W14-consolidation.md` | Consolidate ten branches. | Housekeeping only, 1,626 tests, nothing pushed. | **Superseded** by W37 (branches 33 → 12). |
| `W15-walkforward.md` | Rescue `walkforward.rs`. | Ported byte-identical, 2,186 lines, 28 tests. The previous pass's reason for skipping it was wrong and cheaply falsifiable. | **Landed.** W42 reframes its value: like everything else in the crate, it has never been run against a real capture. |
| `W19-design-fork.md` | Dedup versus corroboration — which philosophy wins? | **Neither. There is no design fork.** Two people solved the same problem seventeen minutes apart in worktrees that could not see each other. Three of the brief's own claims about the branch were wrong. | **Single-source**, and the branch was abandoned. |
| `W22-clippy-gate.md` | Close Phase 0 criterion 1. | The gate passes honestly. True starting count was 62 findings, not 52 — `-D warnings` stops at the first failing unit. `cargo fmt` had never been run: 1,576 hunks across all 44 files. Zero new `allow` attributes. | **Confirmed** by W37 and W47 re-running it. **The only gate in this roadmap that has ever closed.** |
| `W37-repo-reconciliation.md` | Make the repository stop contradicting its own findings. | README opened by naming the exact band the sprint had just disproved, and promised an exit rule, a paper mode and an entry-side builder that do not exist. Branches 33 → 12, worktrees 21 → 10, every deleted branch preserved under an annotated `archive/*` tag pushed first. | **Stands.** It also caught that the brief handed to it quoted the already-withdrawn −62% band figure. |
| `W42-salvage.md` | What is worth keeping of 106,000 lines of Rust? | About 9,000 lines, and one file holds most of it. **Nothing in the Rust crate has ever read a real capture** — no file under `src-tauri/` opens a capture file, no replay fixture exists on disk, `fixtures.rs` opens with "launches that never happened". So "1,654 tests pass" means the engine agrees with itself. Keep `replay.rs`. | **Confirmed** by W47 on all three load-bearing technical claims. One claim overstated. |
| `W47-salvage-applied.md` | Check W42 and make it safe. | Confirmed. Tree tagged `pre-salvage-2026-08-27` and pushed; **nothing deleted**. The 40,000-line bin is written up and queued for Ethan, not performed. W42's "no HTTP dependency at all" is **overstated**. | Verification pass — **confirms W42**. |
| `W52-ceiling-verify.md` | Verify the perfect-foresight ceiling. | **+12.1%, not +18.9%.** One actor supplied 53% of the uncleaned figure, and W9 struck entry a second late, worth about 1.7 points. Confirms the sprint's entry second is right and W9 alone is wrong. | Verification pass — **corrects W9**. |
| `W53-doctrine-reconciliation.md` | Reconcile the two large documents nobody had checked. | `STS_CORE_IDEOLOGY.md` and `STS_ROADMAP.md` both carried retired claims as fact. Corrected against verdict revision 4 — struck through, not deleted, so the record of what was believed survives. **Neither document said expectancy was negative in every window since October 2024**; both were written as though something had gone wrong recently. | **Stands.** |
| `W54-overnight-rewrite.md` | Rewrite the document Ethan reads first. | It was badly stale — five headline claims overturned, two of them twice. It had no mention of the archival sweep, the creator finding, the zero-fee ceiling, the fact that nothing in `src-tauri/` has ever read a capture, or the real 86-coin tail. | **Stands.** The stale original is preserved here as `OVERNIGHT.md`. |
| `W58-evidence-preserved.md` | Get the sprint's evidence out of the scratch directory. | 59 files, 978 KB, copied byte-identical, committed and pushed, plus the first version of this index. | **Stands** — and note that the gap it closed **reopened three times afterwards**, because reports kept being written to scratch. See *How this index was kept* at the end. |
| `W63-cold-read.md` | Read the documents cold, in the owner's order. | The argument is sound and well hedged; **the defects are that the two short documents a returning owner reads first had not been swept for numbers the long one retired underneath them.** Six of its items are that same failure. Worst: `OVERNIGHT` sized the project's central claim off the **+18.9%** ceiling it lists as withdrawn 174 lines later, and `README` still called the fat tail a recording defect. | **Acted on** by W66. Single-source by design — a second cold read would not be cold. |
| `W66-overnight-refresh.md` | Bring `OVERNIGHT-2026-08-27.md` back in line with the verdict. | Done, `0cf1a4f`. Worth reading for the process note: **`main` moved twice underneath it and two of the numbers in its own brief were already stale by the time it wrote them.** It rebased and re-verified against the moved verdict rather than the version it had read. | **Stands.** |
| `W77-handoff-rewrite.md` | Turn the appended log into one document. | **736 lines → 276**, six sections, no chronology. Every figure re-verified against verdict revision 4 rather than copied forward. Fourteen stale claims listed and replaced — and in several places the log carried both the old and the corrected version **as if both were true**. | **Stands.** The `HANDOFF.md` in this directory is now the rewritten one, not the log. |
| `W85-handoff-verify.md` | Check W77's 276-line handoff rewrite line by line against verdict revision 4. | **16 findings — 10 defects, 5 load-bearing omissions, and one place where the handoff is right and the verdict is the stale document.** The good news first, and it is the part that matters: **nothing on the do-not-quote table appears as live** in the rewrite, and **§3 (ground truth) and §5 (the two lessons) are clean enough to copy verbatim** — §3 is checked line by line and every figure matches. The damage is concentrated in the summary table a hurried reader copies. It promotes an **08-21** number — the 3.12x coin — onto "the **real** held-out day" four sections after the same document defines that as 08-16: **the sprint's signature error, reintroduced by the rewrite that was meant to remove it.** It states the graduation drop as "**−20.71%, every coin**" and "no sample size", where the verdict measured **median −20.71%, mean −18.34%, 50 of 54** standard deposits with **2 of 54 depositing 82.0 and 85.1 SOL instead**, and called "every single one" *one word too strong*. It quotes **P(5x) = 0.420%** as a point against the verdict's explicit instruction to quote it as a range, and drops P(3x), the figure the verdict says to lean on. **All five omissions are the same subject — what 08-16 can and cannot be used for** — and the biggest is that **−10.41% ±4.18, the base rate on a genuine holdout and the sprint's strongest single validation, is not in the document at all.** | **Single-source by design, and every citation I spot-checked resolves exactly.** `HANDOFF.md` is 276 lines; lines 28–32, 62, 63, 64, 66, 145–150, 191–195, 204–212 and 261 read as quoted; the strings `0.05`, `−10.41`, `−7.10`, `+12.1%` and `−1.89%` appear **zero times** in the file, confirming B1, B4 and A10 by absence. Its report count was right for its target — **74 `W*` files at `8feeab1`** — and the tree has moved since: **82 committed, 84 on disk.** **One defect of its own:** A3 attributes the verdict's "86% of the doublers and 94% above 3x" to an 08-16 frame; the verdict enumerates 08-16 separately from "the held-out day" (L585–586), so both figures are 08-21 — and the real finding underneath is that **W49 says 100% where the verdict says 94%** (see the do-not-quote table). **Its preservation list is this index's own loop closing:** it names W72, W76, W77, W78, W84 as scratchpad-only; all five were committed in `39d269d`, and the two that never were — **W70 and W79** — appear on nobody's list, including this one until now. |
| `W78-data-backup.md` | How exposed is the capture data? | **106 MB in exactly one place on earth, with no backup of any kind.** No Time Machine destination ever configured, no external disk, no copy anywhere in `~`, Trash emptied 08-26, and `~/Code/flux` has no git remote at all. 35 irreplaceable files. The worst case is the 2,411 tweet samples: **nobody stores what a tweet's view count was 30 seconds after posting**, so they cannot be reconstructed by anyone at any price. | **Stands**, and partly acted on — a local archive now exists, **on the same disk**. The open item is "no copy off this machine", not "no backup". |
| `W80-proofread.md` | Proofread every published document mechanically. | Nothing structural is broken — all links resolve, all fences balance, every sha, tag and branch resolves, no heading skips, tables consistent. **26 defects, all in the prose layer**: counts, stale citations, and retired numbers that had leaked back in. Its first finding is the one this index was rewritten to fix — **it was a map to 55 of 74 reports.** | **Acted on**, here and in the documents. The check it ran is the cheap one nobody had run. |
| `W82-backup-verify.md` | Restore the archive W78 built and see whether it works. | **It restores perfectly — 44 of 44 manifest checksums, 170,119 records, 0 unparseable lines, every original byte-identical.** And it is **missing 26.3 MB in four files**, including **2026-08-12: 4,529 launches with 3,974 one-second candle series, the single richest day of launch data on the disk, in no archive at all.** W78's "exactly one copy of every capture file" is not true. | Verification pass — **confirms the archive, corrects its scope.** The most valuable kind of check: it tested the restore, not the backup. |
| `W75-final-state.md` | Audit the final state of everything. | Code green, published documents agree, **evidence not fully preserved** — seventeen finished reports in no commit, on no branch, in no stash, on no tag. The tree moved **seven times** during the audit and the missing-report count rose from 16 to 17 while it was being written. | **Acted on.** Its central warning is the reason this index has a preservation note at the end. |

## 9. The design question — why nobody found out sooner

The sprint answered the market question in its first half. Its second half asked a
different one: this project ran for months and never tested its own thesis. How was
that possible? These reports are the answer, and they are the part that transfers to
whatever gets built next.

| Report | The question | What it found | Status |
|---|---|---|---|
| `W61-self-sealing-rules.md` | Which rules in the doctrine forbid the check that would test them? | **Nine, plus one structural absence.** The template is §16.2 — backfills prohibited "because they burn credits", where the cost was never checked and is zero, and the check it prevented is the one that produced the governing finding. The worst of the rest: of the 40 acceptance criteria and promotion gates, **two require the system to be right about the market**, and one of those happens only after real money is at risk. And **nothing anywhere measures uptime** — every loss metric is written by the running process. | **Audited** by W69: the count is exact and the conclusion survives; the summary line overstates. Names the seven clauses that genuinely resist this pattern, so nobody sweeps them out with the rest. |
| `W69-gate-audit.md` | Audit W61's count. | **It holds.** 40 verified mechanically, phase by phase. Three corrections, none of which move the conclusion: three *items* name a market outcome though only two are distinct tests; "an engine agreeing with itself" overstates four of the 38, which need a live stream of 24 h, 72 h or 14 days; and the roadmap's own 08-27 annotation says there was no valid held-out day — so **the number of runnable pre-capital thesis gates on this corpus was zero, not one.** | Verification pass — **confirms W61 and sharpens it.** The correction makes the finding worse, not better. |
| `W67-postmortem.md` | Write the design-question companion to the verdict. | `docs/POSTMORTEM-2026-08-27.md`. The root cause in one quote — §00 directive 4's *"essentially undebunkable and consistently profitable"*: **you cannot make a spec profitable by editing it, you can make it undebunkable, and every edit toward that looks like rigour.** Seven named, portable shapes. The sequence position is what lands it: **the thesis gate is item 23 of 40, and the next one is item 39, after mainnet authorization.** | **Stands**, cold-read by W76. Fair at length about what the doctrine got right. |
| `W76-postmortem-coldread.md` | Read the postmortem cold. | The argument is sound and the two strong claims most likely to break both held exactly. One defect worth the read: a multiplier grafted onto the wrong quantity, so the document contradicts its own number 350 lines apart — **in the one section whose whole subject is numbers being wrong.** | **Acted on.** Single-source by design. |
| `W74-gates.md` | What should the ladder have been? | `docs/GATES.md` — fifteen gates in D → T → L order, each with input / output / FAIL and **how you would tell it was skipped**. The principle: **a gate must be able to come back FAIL for a reason that is not your fault.** If every route to a FAIL is "we wrote a bug", it tests the machine and not the idea. The existing 40 stay exactly as written and simply **stop authorising anything**. | **Stands.** Cold-read by W84. |
| `W84-gates-coldread.md` | Read `GATES.md` cold — could someone run these? | **Thirteen of fifteen as written.** The structure is right and the fix is correctly aimed. Two things are wrong at the level that matters: **T2, the headline gate — "one afternoon, zero euros, week one" — is not runnable from the document**, which never names the archive, the venue, the launch program, the holding rule or the hour-matching; and the document does not turn its own rules on itself. | **Single-source**, and the more useful of the two cold reads because it is about a document meant to be *executed* rather than read. |
| `W79-gates-run.md` | Run the fifteen gates of `GATES.md` for real — do they actually come back FAIL? | **The only time anyone ran them. Two pass, nine fail, one is defective, three need a live system that does not exist.** T2 fails first and costs **0.35 seconds of compute**: buying every launch across seven hour-matched windows returns **−10.32% net, 95% CI [−13.00, −7.63], n=1,034**, against the **+2.12% break-even** T1 measures independently at 95.0 bps a side. T3 then closes it — **0 of 207 pre-declared rules positive with every cost set to exactly zero**. But **twenty-four places needed a choice the document should have made, and six of them decide PASS or FAIL.** The worst: "sell inside the minute" is not an exit rule, and the answer is **−1.38% at a 5-second hold against −10.32% at 60** — a nine-point spread on an unstated word, against a 2.12% bar. Every skip test checks that *a number was reported*, never that it was the right one, and W79 shows seven gates that pass while being skipped. Its own first run returned a confident **+46.64%** zero-cost ceiling from a hold bug that sold before it bought — caught only because the number was too good. | **Single-source, never rechecked, and it disagrees with the verdict twice on headline numbers** — T4's sign and T6's entry-time gradient; see the do-not-quote table. Its headline tally is off by one: it counts 2+8+1+3 = **14 of 15 gates**, omitting `D6`, which its own table marks FAIL. The table's split is **2 pass / 9 fail / 1 defective / 3 unrunnable**. Its T2 population (1,034) does not reconcile with its own 1,283 − 203; its T3 zero-cost best appears as −0.62%, −0.51%, −0.38% and −0.66% in four places. Ran against `docs/GATES.md` at `c250f5f`, **which is now on `main` unchanged** — its opening line "not on `main`" is stale — and its `D5` used the checker as it stood before W70's `906e1e7`. |

## 10. Verification passes, in one place

Twenty-seven reports existed only to recheck another. This is the column that mattered.

| Verifier | Checked | Outcome |
|---|---|---|
| W16 | W12 (landing cost) | **Corrected** — wrong by ~20x |
| W33 | W30 (band) | **Corrected** — headline was a filter artefact |
| W38 | W33 (band, corrected) | **Corrected again** — W33's replacement was also wrong |
| W39 | W36 (creator) | **Confirmed** to a rounding difference |
| W41 | the verdict against all 35 reports | **Corrected** — the document contradicted itself |
| W45 | W34 (12-hour exits) | **Confirmed** to 0.01 points |
| W46 | W32 (corruption) | **Split** — core confirmed, two supports withdrawn |
| W47 | W42 (salvage) | **Confirmed**, one claim overstated |
| W48 | the "90% drop" claim | **Struck** — 137 of 137 |
| W50 | W24 (failure rate) | **Corrected** — 11.3%, not 93.2% |
| W51 | W46 (the real tail) | **Confirmed** exactly, cell for cell |
| W52 | W9 (ceiling) | **Corrected** — +12.1%, not +18.9% |
| W55 / W65 | the `len(highs) >= 60` hygiene rule | **Corrected** — the filter had no basis; −7.76%, not −9.85% |
| W57 | W49 (tail collapse) | **Split** — every cell reproduces, two quoted numbers wrong |
| W59 | W38 (band) | **Split** — the constant is exact, the statistic is replaced |
| W62 | W56 (selection) | **Split** — mechanism confirmed, null corrected |
| W64 | W26, W35 (exits) | **Split** — one conclusion survives, one reverses |
| W71 | W64 | **Split** — reversal confirmed, its +1.76% cell struck |
| W72 | six surviving claims, on the real holdout | **Corrected** — two demoted, base rate survives |
| W73 | W68 (the holdout) | **Confirmed**, and one serious caveat added |
| W69 | W61 (gate count) | **Confirmed**, summary line sharpened |
| W76 | the postmortem | **Confirmed**, one grafted multiplier |
| W80 | every published document, mechanically | **Corrected** — 26 prose defects, nothing structural |
| W85 | W77's handoff rewrite, against verdict revision 4 | **Split** — §3 and §5 clean, §2's summary table wrong in four places |
| W82 | W78 (the archive) | **Split** — it restores perfectly and is missing 26 MB |
| W84 | `GATES.md` | **Confirmed** — 13 of 15 runnable; T2 is not |
| W79 | `GATES.md`, by running it | **Split** — T2 reproduces and fails first at 0.35 s; T4 and T6 do not |

**Twenty-seven verification passes, and not one left its target exactly as it found
it.** Eight confirmed the load-bearing claim outright; the other nineteen corrected a
number, struck a headline, or split the report into a part that held and a part that
did not. **That hit rate, not the trading answer, is the real lesson of this sprint**
— and it held to the last hour and past it: W82 found a hole in a backup written
forty minutes earlier, W72 demoted a lead two other reports had already confirmed,
and the pass that wrote these last rows found that W79's own count of the gates it
had just run was off by one.

---

## What is still open

Named here because an unmeasured thing named is worth more than one hidden.

- ~~**The array-cap filter was never fully audited.**~~ **Closed.** W55 audited it and
  W65 rebuilt the audit independently; every corrected figure reproduces to the digit.
  The direction is mostly conservative, as expected — but **not everywhere**, and the
  two places it flattered are both exit results, which is why W64 and W71 exist. The
  measured cost is +2.09 points on the headline, a 29% too-narrow confidence interval,
  and half of every genuine 5x deleted.
- **W4 (wallet graph) and W5 (social) were never re-run on the clean corpus.** Their
  base rates predate the identification of the manufactured coins, which halve
  P(2x) across the board. Still true, and now the oldest unclosed item here.
- **Raydium consolidation is untestable with this data.** Seven migration events, no
  post-migration price. Testing it needs a recorder that follows the mint into the
  AMM pool *and* stops seeding its universe from launches.
- **`~/Code/flux/data/` is gitignored, irreplaceable and backed up nowhere.** This
  data is not for sale and every hour not recorded is a permanent hole.
- **Every number here comes from the first 60 seconds of a launch**, because that is
  all the captures hold — with the exception of the 12-hour `tracks` label (W34,
  W45) and the archival sweep (W25).
- **The 12-hour label has never been tested out of sample, and this is the largest
  untested claim in the sprint.** W72's words. On the real holdout `tracks` is 82 rows
  from a five-minute run — not thin, absent — so every 12-hour exit result rests on
  08-20 and 08-21 alone, the same two days everything was fitted on. The direction is
  corroborated by W20 at 300 seconds and by the archival sweep, so this is an untested
  claim rather than a doubted one. It is still untested.
- **One load-bearing argument is weaker than the documents say, and it is an
  interval problem rather than a wrong number.** W35 closed the sell side with a
  principle: when the *ceiling* of an input is below zero, no cleverness inside that
  input reaches break-even. Its ceiling was +1.01% gross, −0.89% net. Without the
  array cap it is **+1.58% ±1.05**, and W71 points out what that means: the upper end
  of the interval is **+2.63% gross, above the +2.12% break-even bar**. The point
  estimate still loses. The interval is no longer entirely below the line. "The
  ceiling settles it" is a statement about a point estimate being read as a statement
  about an interval, and it is used in more than one place.
- **Every flow-grid number in the sprint is fitted on a corpus that contains the
  holdout.** W26 fitted on sessions 0–2, which is 08-15 plus **08-16** plus early
  08-20 — and 08-16 is the day W68 and W72 later designate the only real holdout in
  the corpus. W64 and W71 both rebuild on the same population. So the flow-exit
  results, including the 0th-to-100th percentile reversal, were never out of sample
  and nobody noticed at the time, because the day was not identified as the holdout
  until the last hours of the sprint. It does not change the money conclusion — flow
  exits lose — but no flow number here is a held-out number.
- **The only genuine walk-forward split in the corpus was never run.** W68 names it:
  fit on 08-12, 08-15 and 08-16 (1,623 clean), score on 08-20 and 08-21 (5,461 clean),
  3.78 days of separation, and it runs forwards in time rather than backwards the way
  08-16 does. Cheap, and nobody did it.

---

## How this index was kept

Five passes preserved this evidence. Each of the first four believed it was the last
one needed.

| when | commit | what it saved | what it still missed |
|---|---|---|---|
| 05:41 | `f2141c2` | 59 files — W00–W54, the brief, two handovers, the first version of this index | the reports being written while it ran |
| 06:16 | `4251114` | 22 more | same again |
| 06:33 | `39d269d` | 9 more, finished **by hand** after the pass doing it died with a spend-limit error | W70 and W79 |
| 09:04 | `abccd8d` | W70, and W79 — the only record that anyone ever ran the gates | — |
| this one | `f42fad6` | `HANDOFF-log.md`, which survived only inside git history, and this rewrite of the index, which survived only as an uncommitted file in a temporary worktree | **the scripts** |
| and one more | `cc7933a` | **465 files and 30,673 lines of analysis code — every number in the verdict was computed by something in it, and the whole object graph held zero `.py` files.** Plus `w83/`, an investigation nobody knew had run | — |

The pattern is worth naming, because it is not carelessness. Every pass was thorough
about the files it knew about. What defeated four of them in a row is that finished
work kept arriving *after* the sweep, into a directory nobody was watching, and the
only way to find out was to go and look again. Two and a half hours after a commit
message with the word "final" in it, two finished reports were still sitting in
`/private/tmp`, which a restart clears.

**A preservation pass is not a task you complete. It is a state you have to keep
checking.** The check is cheap — list every file under the scratch directories,
compare against `git ls-tree`, read whatever is left over. It takes a minute and it
has never once come back empty.

**And check by content, not by name, and for every file type and not just the one you
are thinking about.** Four passes swept for `.md` and found the reports. None of them
swept for `.py`, so for nineteen hours the situation was that the argument was fully
preserved and the code behind every number in it was not preserved at all — while a
report titled *"the sprint's evidence is in the repo"* said the job was done. The
sweep that finally caught it hashed 3,146 candidate files against the repository's
2,321 reachable blobs and set-differenced. That is the version of the check that
works.
