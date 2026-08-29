# Known wallets beat crowd size — and it is still not a trade

> **Correction, 12 Aug, after the numbers below were written.** Everything below
> holds: a known-good early wallet really does predict a far higher peak. It is
> not tradeable. Replaying 35 target/stop combinations over 3,556 coins with a
> known wallet, **every rule loses after the 3.1% round trip, and every rule
> loses more than the coins without one** — best case −2.61% against −1.99%.
>
> The reason is timing. The median peak lands at **3 seconds**, which is the
> entry moment itself, and is handed straight back: these coins average a
> **1.42× peak and a 0.95× close**, against 1.05× and 1.00× for coins with no
> known wallet. They are more volatile, not more profitable — the stop is hit
> more often and the target rarely survives.
>
> The strongest evidence needs no replay at all and no assumption about ordering
> inside a second: **coins with a known early wallet close below their entry on
> average.** A predictor of spikes is not a predictor of profit, and this is
> Log.md's original verdict arriving by a new route.
>
> What the signal is still good for: ranking attention, finding operators, and
> telling you who is early. Not deciding what to buy.


Tested 12 Aug 2026 on the local corpus: 3,878 coins with wallet records across
10–12 Aug, 15,875 distinct wallets.

**Not yet established.** One test day, and every outcome is measured inside the
60-second follow window. It needs replication before anyone acts on it.

---

## The question

STS scores a coin by how many wallets bought it in the first three seconds. The
question here is different: forget the coin, score the **wallet**. Does a wallet
with a good past record keep picking well, and is that knowable early enough to
act on?

## Method

Wallet records were built from **10–11 Aug only**, and scored on **12 Aug**, a
day that had no part in ranking them. Only wallets that entered a coin **within
the first 3 seconds** counted, on both sides — a wallet arriving at second 30 is
reacting to a move, not predicting it, and could never be acted on. Each test
coin is counted once, not once per wallet.

## What came back

Test day baseline: mean peak **1.452×**, **18.2%** of coins reached 1.5×.

Taking the top quarter of wallets by their earlier record:

| Wallets with ≥N early coins on train days | Coin has one as an early buyer | Coin does not |
|---|---|---|
| ≥2 | 2.071× · 38.6% ran | 1.122× · 7.3% ran |
| ≥3 | 2.153× · 39.2% ran | 1.142× · 8.9% ran |
| ≥5 | 2.229× · 41.9% ran | 1.145× · 8.8% ran |

Roughly **five times the run rate**, decidable at the three-second mark.

## The control that matters

The obvious objection: these wallets might simply buy coins that attract buyers,
and coins with buyers run. So the same test, holding the number of early buyers
fixed:

| Early buyers | With a known wallet | Without |
|---|---|---|
| 1 | 1.000× · 0% (n=2) | 1.051× · 2.2% (n=366) |
| **2–3** | **2.835× · 52.7% (n=186)** | **1.126× · 8.0% (n=199)** |
| 4–7 | 1.503× · 25.6% (n=39) | 1.167× · 9.4% (n=203) |
| 8–15 | 1.526× · 24.1% (n=79) | 1.413× · 28.6% (n=105) |
| 16+ | 1.625× · 45.7% (n=46) | 1.341× · 21.1% (n=19) |

The signal is not spread evenly. It is concentrated almost entirely in the
**2–3 early buyer** band: a quiet launch that one known-good wallet got into.
There, a known wallet lifts the run rate from 8% to 52.7%.

At 8–15 buyers the wallet identity adds **nothing** — 24.1% against 28.6%
without. Once a crowd has arrived, who is in it stops mattering.

## Why this matters more than the number

**The current candidate filter requires at least 8 early buyers.** It therefore
rejects, by construction, every coin in the one band where this works. The
filter and the signal are looking in opposite places.

`$JIM` (`8Kos93i…`, 12 Aug 16:45) is the case in point: 2 wallets, 82.97 SOL in
three seconds, no social presence at all. It scored **15 out of 100** and was
marked ineligible, on the grounds of "only 2 early buyers."

This also reframes an old verdict. Log.md records that the wallet graph "added
nothing", but what was tested there was co-occurrence — whether wallets
appearing together predicts returns. This is a different measurement: a single
wallet's own track record. The earlier finding does not cover it.

---

## Confirmed against a second source (12 Aug, Dune)

The test above ranks wallets and scores them inside the same local corpus. Dune
allows a cleaner split: build the registry from history the local watcher never
saw, then score it on local days that history does not cover.

Registry built from **pump.fun via Dune, 12 Jul – 10 Aug** (45,067 wallets with
5 or more early coins). Scored on the **local corpus, 11–12 Aug**. Different
source, no overlapping dates.

| Registry pool | Local coins with one in early | Mean peak | Ran |
|---|---|---|---|
| ≥5 dune coins | 1,168 | 1.882× | 35.5% |
| ≥10 dune coins | 1,065 | 1.931× | 36.8% |
| ≥25 dune coins | 996 | 1.983× | **38.4%** |
| *(coins without one)* | 2,697 | 1.132× | **8.3%** |

Local baseline for those days: 1.361×, 16.4% ran.

**Coverage is the part that makes this usable.** Of every early-buyer slot in the
local test coins, the Dune registry recognises **79.4%**. This is not a thin
overlap that happens to work — it knows most of the wallets that turn up.

### And the sampling problem it exposed

On 11 Aug pump.fun had **40,372 launches**. The local log holds **2,965** of
them. The public Solana RPC is dropping roughly **93%** of launches, so every
wallet record built from local data alone rests on a 7% sample. Anything built
on wallet history should be built from Dune and merely *kept current* by the
live watcher.

---

## Past sixty seconds, on 200,000 coins (12 Aug, Dune)

Everything above stops at the sixty-second mark, because that is where the local
watcher stops looking. That was the open question behind `$JIM`, which ran to
30k after the window closed. Dune can see the whole hour.

Registry built on **12–31 Jul**, tested on coins launched **1–7 Aug**. 199,022
coins, none of which had any part in ranking the wallets.

| | With a top wallet (62,007) | Without (137,015) |
|---|---|---|
| mean max at 60s | **2.161×** | 1.234× |
| mean max at 5m | **2.514×** | 1.303× |
| mean max at 15m | 2.592× | 1.332× |
| mean max at 1h | **2.617×** | 1.349× |
| mean close at 60s | 0.964× | 0.952× |
| mean close at 5m | 0.893× | 0.923× |
| mean close at 15m | 0.872× | 0.913× |
| mean close at 1h | **0.857×** | 0.909× |
| still above 1.2× at 1h | ~10% | ~0% |

Three things follow, and they do not all point the same way.

**The signal is large and it is real.** A 2.6× average peak against 1.35× is not
a marginal effect, and it held on a week the registry never saw.

**Peaks keep forming after the first minute.** The mean max rises from 2.161× at
60s to 2.514× by 5m. A third of the eventual move happens after the local
watcher has stopped looking — which is exactly why the 60-second window had to
go, and why `$JIM` was invisible.

**And holding is still wrong.** The average close falls at every step: 0.964 →
0.893 → 0.872 → 0.857. Worse, coins with a top wallet close *lower* than coins
without at every horizon past a minute. They spike harder and bleed harder, so
buy-and-hold loses more the longer it is held, and loses most on exactly the
coins the signal likes.

What remains is whether an exit rule can capture the spike rather than ride it
down. That is the last open question.

---

## A rule that pays, twice, out of sample

The last open question was whether an exit rule could capture the spike rather
than ride it down. It can, narrowly.

**Buy at the 3-second mark when a top-quartile wallet is already in. Sell at 2×.
Stop at −15%. Give up after an hour.**

Registry built on **12–31 Jul** in both tests. Entry and exit replayed from
first-crossing times, with a tie inside the same second counted as the stop.

| Test period | Coins | Gross | Net at 3.09% | Hit 2× | Stopped |
|---|---|---|---|---|---|
| 1–7 Aug (rule chosen here, from 5) | 60,260 | 1.0389 | **+0.80%** | — | — |
| 8–11 Aug (rule fixed in advance) | 32,478 | 1.0465 | **+1.56%** | 20% | 70% |
| 8–11 Aug, coins *without* a top wallet | 74,555 | 0.9703 | −6.06% | 10% | 50% |

The second week is the one that counts: the rule was specified before looking,
on days neither the registry nor the first test touched. It replicated, and
slightly better.

**Why this works when the 60-second version did not.** The 2× target is often
reached *between one and five minutes*, and the local watcher stops at sixty
seconds. Every earlier negative verdict — the log's and this file's — was
measuring a window that closes before the trade finishes.

### Why this is still not a green light

- **It is thin, and only at one size.** +0.8% to +1.6% is measured at the
  cheapest possible position, 0.297 SOL. At 0.5 SOL it falls to +0.4%/+1.2%; at
  1 SOL the round trip is 5.7% and both weeks go **negative**. This is a
  small-size edge or nothing.
- **The payoff is a lottery.** 20% of trades hit the target, 70% stop out. The
  mean is positive; almost every individual trade is a loss.
- **It assumes fills that STS cannot currently get.** The entry is the 3-second
  price and the exit is exactly 2×. Real execution slips, and the cost model
  covers tip and curve impact only — not failed transactions or being front-run
  into the same spike.
- **The live pipeline cannot see what this backtest saw.** On the public RPC the
  watcher misses about 93% of launches outright and 10–20% of early buyers on
  the coins it does catch. A wallet missed at second 3 is a trade not taken. The
  binding constraint is now the feed, not the signal.

## What it does not say

- It is one test day. The 2–3 band rests on 186 coins.
- Outcomes are peak multiples inside 60 seconds, so "ran" means "ran early".
  With the tracker now holding coins for 12 hours this can finally be measured
  over a real horizon.
- It says nothing about whether the trade is profitable after costs. A 52.7% run
  rate has to clear the ~3.1% round trip before it is worth anything.
- Frequency is not quality: wallets appearing on 50+ coins have *worse* mean
  outcomes (1.787×) than wallets appearing on one (1.947×). Bots that buy every
  launch are noise, and the ranking has to be by record, not by activity.
