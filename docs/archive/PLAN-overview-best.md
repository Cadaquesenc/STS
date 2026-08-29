# Overview panel: best coins to actually buy

Replaces the left-hand "Candidate coins" list. Instead of a recency feed, the panel
ranks coins by **how much money you would expect to make buying them right now**, and
states a recommended entry and exit for each.

Status: plan only. No code written.

---

## Read this first

STS has already tested this question and the recorded answer is no. From `Log.md`,
10 Aug 2026:

| Finding | Number |
|---|---|
| Edge from the best on-chain signal, after platform fees | **+1.34%** per trade |
| Cheapest possible cost of a round trip | **~3.1%** (at 0.25 SOL) |
| Net result | **negative at every position size** |

The costs squeeze from both ends and there is no gap between them. The tip is a fixed
0.0046 SOL, so a small trade pays it as a large percentage; a large trade moves the
price against itself on the way in and again on the way out. Best case is about 3%
against an edge of 1.34%. The log's words: "off by roughly a factor of two, at its
best."

A recommended exit was tested directly and marked dead. These coins peak a **median of
one minute after launch**, which is exactly when a buy would land. Holding for an hour
returned between −0.77% and −2.98% across every group of coins, before costs.

A stronger entry signal was tested and looked like +5.44% on one day, then returned
−3.90% and −0.42% on days that had not been looked at.

The summary line in the log is "as designed, this does not work," and the standing rule
is that no money goes at risk until something much larger than the current edge shows
up.

**This plan therefore does not build a panel that claims profit. It builds the
instrument that measures whether profit exists, honestly, and reports the answer
whatever it is.** Today that answer will be roughly −1.7%. If the social work produces
a real edge, the same panel will show it without any rewiring.

---

## What the panel looks like

The timeframe chips become the **hold horizon under test** — "if I bought and held for
this long, what happens?" That makes them do real work rather than just filter by age.

```
BEST TO BUY        hold: [5m] [15m] [30m] [1h] [5h] [12h]

$GROKENING    net −1.6%   after 3.1% costs
  enter now (47s old) · exit +50% or −15% or 15m
  edge +1.5%  ·  hit rate 7.5%  ·  n=214  ·  out-of-sample

$WiggaButt    net −2.4%   after 3.1% costs
  enter now (22s old) · exit +50% or −15% or 15m
  edge +0.7%  ·  hit rate 5.1%  ·  n=214  ·  out-of-sample

nothing here is currently profitable — best net is −1.6%
```

The net figure is the headline, not the gross edge. A row is only ever green if the
number is positive after costs. If none are, the panel says so in words at the bottom
rather than presenting the least-bad loss as a pick.

---

## Where entry and exit come from

Not from a guess. From the backtest engine that already exists at
`/api/backtest` in `src/dash.js:208`, which replays a take-profit, a stop-loss and a
maximum hold over recorded one-second candles, and takes the stop first when both
levels fall inside the same bar.

**Entry.** The earliest moment STS can act is the end of the opening window, currently
3 seconds. The log's later tests used 60 seconds. Both should be offered, because the
difference matters: at 3 seconds the price already reflects the opening rush, and at 60
seconds the median coin has already peaked. The panel states which one the number
assumes.

**Exit.** A sweep across the backtest's three parameters — take-profit, stop-loss and
maximum hold — picking the combination with the best net result for the selected
horizon. That combination is the "recommended exit" shown on the row.

**Costs, subtracted always.** Position size, the fixed 0.0046 SOL tip, and price impact
in and out, using the measured table in the log:

| Position | Tip | Price impact both ways | Total |
|---|---|---|---|
| 0.10 SOL | 4.6% | 0.6% | 5.2% |
| 0.25 SOL | 1.8% | 1.2% | **3.1%** |
| 0.50 SOL | 0.9% | 2.6% | 3.5% |
| 1.00 SOL | 0.5% | 5.2% | 5.7% |

The panel needs a position-size control, because the answer genuinely changes with it
and a number quoted without a size is meaningless.

---

## The three rules that keep it honest

These are what separate this from the thing that showed +5.44% and was noise.

**1. The strategy may not be chosen and graded on the same coins.** Pick the exit rule
on one set of days, report the result on days that were not used to pick it. Every
number on the panel is the out-of-sample one, labelled as such. This single rule is why
the earlier find was caught instead of shipped.

**2. Costs are never optional.** No gross figure is ever displayed as the headline. The
gross edge can sit beside it as context, but the ranking sorts on net.

**3. Sample size is shown, and small samples are not ranked.** The most recent social
result was 21 successes against 12 — the log calls that "a reason to keep collecting,
not a reason to believe anything." Rows below a minimum sample show "not enough data"
instead of a percentage.

There is also a measurement trap specific to this data, recorded at `Log.md:604`: the
multiple is measured from the price at 3 seconds, and coins with a fresh tweet have
already taken 5.63 SOL by then versus 0.49 for coins with no link. Eleven times more
money has already moved the price before the clock starts. Comparing their multiples
directly measures the ruler, not the edge. Any social comparison in this panel has to
start from the same baseline or it will invent an edge that isn't there.

---

## What is genuinely new since those verdicts

Worth saying, because the verdicts above are not the end of the story:

1. **Those tests ran on Dune, on historical data.** Dune could not establish what
   happened inside a single second, so intrabar ordering — did the stop or the target
   hit first — was guesswork. The watcher now records one-second candles, which settles
   that question honestly for the first time. The log flags this exact gap.
2. **Social monitoring is the one untested input**, and the log calls it the most
   promising remaining direction precisely because it is not a public table that every
   bot reads at the same instant. The branch already tracks tweets and their engagement.
3. The panel is the natural place to find out. It is the measuring instrument for the
   remaining open question, not a trading recommendation.

The honest expectation is that it reads negative for a while. That is the correct
result to display, and displaying it is the point.

---

## Build order

1. Cost model as a shared function — size in, total percentage out. Nothing gets built
   on top of an uncosted number.
2. Extend `/api/backtest` to sweep parameters and split days into a set used for
   choosing and a set used for reporting.
3. `GET /api/best?hold=15m&size=0.25` returning ranked rows with net, gross, hit rate,
   sample size and the chosen exit rule.
4. The panel, with the horizon chips, the size control, and the empty-state sentence
   for when nothing clears costs — which is the state it will be in most of the time.
