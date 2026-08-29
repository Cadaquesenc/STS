# STS — Working Log

> **Superseded 2026-08-27, and kept because live code cites it.** Five files under
> `src-tauri/src/strategy/` name this log as their authority, so it is a dependency
> and not just an archive. Read it as a record of what was believed between 10 and 16
> August 2026, not as a source of numbers. Its four load-bearing figures are all
> retired: the wallet-count ladder sorts by **3.7x, not 17x**, and is one binary step
> rather than a ladder; the round trip is **1.90%** and break-even **+2.12% gross at
> 0.05 SOL**, not "about 3%"; own-order price impact does **not** squeeze, because on
> a constant-product curve an instant round trip walks up and back down the same path
> and cancels exactly, **0.04% at 0.1 SOL**; and there is no **+1% a trade** — buying a
> launch returns **−7.76%** after real costs, **−10.41% ±4.18** on the only genuine
> holdout, and **−0.86% with every cost set to zero**.
>
> The log is also right about something the sprint later misread: its monthly launch
> table is explicitly labelled *"from the broken table — shows when it stopped
> working"*, and it says outright that the market had **not** died. A later report
> quoted that table as a market fact and was wrong to. Volume never collapsed.
>
> Check anything here against the *Do not quote these numbers* table in
> `docs/sprint-2026-08-27/INDEX.md` before reusing it.

Newest at the bottom. Plain words only.

---

## 10 Aug 2026, 17:00 — Setup

Repo made (private). "STS" is a placeholder name.

Idea came from two ChatGPT chats. **Kept:** the on-chain timing engine and the wallet graph. Social monitoring is also in scope — see 19:55.

Main idea: don't ask "will this coin die" — they nearly all do. Ask **how long it lasts** and **how high it gets first**.

Data: Dune, plugged straight into Claude Code. On a 14-day free trial that **ends about 20 Aug 2026**.

Setup gotcha: in `claude mcp add`, the `--header` flag eats everything after it. Put it **last** or the key silently doesn't get saved.

---

## 10 Aug 2026, 17:40 — Stage 0: can we get the data?

**Passed.** We can see each wallet, in order, with real amounts, and who funded who. Goes back to at least Feb 2025.

**Where things are:**

| Table | What's in it |
|---|---|
| `pumpdotfun_solana.pump_call_create` | Launches (**broken after 2025 — see 19:20**) |
| `pumpdotfun_solana.pump_call_buy` / `_sell` | Buys and sells |
| `system_program_solana.system_program_call_transfer` | SOL moving between wallets |
| `dex_solana.trades` | **Real prices actually paid** |

**Things that caught us out:**

- `maxSolCost` and `minSolOutput` are **limits, not what was paid**. Real money only in `dex_solana.trades`.
- Every column starts with `call_` — it's `call_block_time`, not `block_time`. Dune's own schema list is wrong. Check with `select * … limit 1`.
- Clock times are whole seconds only. To get the order right use `call_block_slot` (one every ~0.4s) and `call_tx_index`.
- Two wallets buying the same coins means nothing on its own. Some bots buy 5% of *every* launch. Check a wallet's total footprint first. We got this wrong once.
- On a bonding curve the price only moves when someone trades. So "price now" = the **last** trade, not the next one.

**Free signal found:** buys ÷ number of wallets. About 2–3 = a real crowd. Over 40 = a few bots faking activity.

---

## 10 Aug 2026, 18:10 — One launch, followed all the way through

Coin `A6RLPJh8…`, 6 Feb 2025.

19 wallets bought in the first minute. 15 of them in the first 7 seconds. 8 of those had **no history at all** — one buy each, then nothing all week.

**7 of the 8 got exactly 1.45 SOL each, from the same wallet, in the same block, 35 seconds before the coin existed.**

That's a bundle — one person using seven fresh wallets to make a launch look busy. Only the money trail could find them. They had no track record on purpose.

**The bundle lost money:**

| | SOL in | SOL out | Result |
|---|---|---|---|
| The 7 wallets | 7.09 | 5.60 | **−21% in 55 seconds** |

They all sold in the same block and crushed their own price.

**The ones who made money left in seconds:**

| Wallet | In | Out | Result | Held |
|---|---|---|---|---|
| 8mScFWqj… | 3.80 | 4.88 | +28% | 2s |
| 7Y4Fmn2D… | 0.114 | 0.135 | +18% | 2s |
| 9tQ5fzSj… | 0.090 | 0.105 | +17% | 3s |
| EnTMdRQd… | 0.262 | 0.299 | +14% | 3s |
| BENE3RkN… | 3.33 | 3.65 | +9% | 1s |
| GUsdSJZw… | 0.082 | 0.079 | −3% | 3s |

---

## 10 Aug 2026, 18:45 — Stage 1: is there time to make money?

One day (6 Feb 2025), 46,384 coins. The middle one peaks after **10 seconds**, is dead in **8 minutes**, gets **15 trades** total.

**Buying everything loses.** Entering 3 seconds after launch, across 45,685 coins the middle coin peaks *exactly where you bought* (×1.0009) and falls from there.

**But how many wallets buy in the first 3 seconds predicts a lot:**

| Wallets in first 3s | Coins | Reaches +50% | Doubles |
|---|---|---|---|
| 0 | 932 | 2.6% | 1.2% |
| 1 | 5,610 | 5.8% | 3.2% |
| 2–3 | 25,470 | 11.4% | 7.0% |
| 4–7 | 11,155 | 16.9% | 10.3% |
| 8–15 | 1,849 | 31.8% | 13.1% |
| **16+** | **669** | **44.1%** | **21.4%** |

Straight up the table. About 17× difference between worst and best.

---

## 10 Aug 2026, 19:20 — Big correction: the launches table is broken

The launches table shows a 99.8% drop — 57,000 a day in Jan 2025, about 80 a day now. It looked like the market had died.

**It hasn't.** On 6 Aug 2026 there were **44,789 different pump.fun coins traded** in one day (plus 13,265 on PumpSwap). Compare 104,890 in Feb 2025. Smaller, but alive.

pump.fun changed how it creates coins and Dune's table stopped catching it.

**Fix: stop using that table.** Treat a coin's **first ever trade** as its launch. Works on any version, and it's what a live system would see anyway.

Launches per month (from the broken table — shows when it stopped working):

| Month | Launches |
|---|---|
| Jan 2025 | 1,727,508 |
| Jun 2025 | 771,771 |
| Nov 2025 | 248,412 |
| **Dec 2025** | **41,744** ← breaks here |
| Mar 2026 | 11,376 |
| Jul 2026 | 2,851 |

---

## 10 Aug 2026, 19:35 — Stage 1 result: the edge is real but tiny

Today's market (6 Aug 2026), new coins only, fees taken off (1% each way).

**Rule: buy 3s in, sell at +25%, otherwise sell after 60s.**

| Wallets in first 3s | Coins | Average | Middle | Win rate |
|---|---|---|---|---|
| 0–7 | 31,513 | −3.18% | −1.98% | 22.4% |
| 8–15 | 3,286 | −8.66% | −9.64% | 37.3% |
| 16–24 | 1,020 | **−0.85%** | +17.27% | 54.3% |
| 25–49 | 420 | **−1.73%** | +22.32% | 54.3% |

Every average is **negative** even where the middle is strongly positive. The winners win about +30%, the losers lose about −55%. Losing bigger than you win.

**Adding a stop-loss fixes it.** Sell at −15%, or +50%, or after 60s:

| Wallets in first 3s | Coins | Average | Stopped out | Hit target |
|---|---|---|---|---|
| 0–7 | 31,513 | −1.91% | 27.6% | 8.2% |
| 8–15 | 3,286 | −2.27% | 61.4% | 17.0% |
| **16–24** | **1,020** | **+1.34%** | 63.7% | 24.6% |
| **25+** | **439** | **+0.45%** | 69.0% | 24.6% |

So: about **+1% per trade**, on roughly 1,400 chances a day.

**The catch:** that +1% does not include the tip you pay to get your transaction in fast, or the fact that your own buying moves the price. Both are probably around the same size as the whole edge. So this is *not* proven profitable — it's "too close to call without live testing".

---

## 10 Aug 2026, 19:40 — Stage 2 result: the wallet graph didn't help

Two tests, both on 6 Aug 2026, both on the 16+ coins.

**Test 1 — do bundles predict anything?**

| Early buyers sharing a funder | Coins | Average |
|---|---|---|
| None found | 216 | −1.84% |
| 1–2 | 1,155 | +1.86% |
| 3–5 | 71 | −1.22% |
| 6+ | 17 | +2.67% |

No pattern. Goes up, down, up. That's noise, not a signal.

**Test 2 — sell when the early buyers sell?**

| Rule | Average | Win rate |
|---|---|---|
| Fixed stop-loss | **+1.07%** | 29.2% |
| Follow the early sellers | **−1.86%** | 29.8% |

Worse. The reason: on these launches *everyone* sells within seconds anyway, so the signal fires immediately on every coin and tells you nothing.

**So the wallet graph — the thing meant to be the whole advantage — added nothing in either form tested.** One day of data, and only two simple versions, so it's not final. But it's a bad early sign.

---

## 10 Aug 2026, 19:50 — What it costs to get in fast

This was the number that decided whether the +1% was real.

Two costs on top of the platform fee: the network fee, and the **tip** you pay to jump the queue.

**Network fee.** Measured across 135,770 first-3-second buys on 6 Aug 2026:

| | |
|---|---|
| Typical position | 0.679 SOL |
| Typical fee | 0.000143 SOL |
| **Fee as share of position** | **0.055%** |

Tiny. Not a problem.

**Tips.** These are separate payments, so I found them by looking for addresses that got paid over and over across thousands of different launches:

| Tip account | Times paid | Typical tip |
|---|---|---|
| 62qc2CNX… | 62,712 | 0.0046 SOL |
| 5YxQFdt3… | 30,623 | 0.0025 SOL |
| CebN5WGQ… | 18,122 | 0.0028 SOL |
| A7hAgCzF… | 15,028 | 0.0035 SOL |

**A typical tip is about 0.0046 SOL. On a 0.68 SOL position that is 0.68%.**

**Adding it up:**

| | |
|---|---|
| Edge from the signal, after platform fees | +1.34% |
| Network fee, both ways | −0.11% |
| Tip to get in first | −0.68% |
| **Left over** | **about +0.5%** |
| Your own buying moving the price | **not measured** |

That last line is the problem. On a brand new coin the pool is tiny, so a 0.68 SOL buy shifts the price on the way in *and* on the way out. It is very likely more than 0.5%.

**So: after everything we can measure, the edge is about half a percent, and the one thing we can't measure yet is probably bigger than that.**

---

## 10 Aug 2026, 19:55 — Social monitoring is back in scope

Decision: the system does need to watch social media after all. It was dropped early on to save cost and complexity.

The results above make it more important, not less. Every on-chain signal we tested is either weak or already crowded — because every other bot can see the exact same blockchain at the exact same moment. Watching for a coin *before* it launches, or spotting which launch matches a real story, is information that isn't sitting in a public table for everyone.

**Dune cannot test this.** It only holds blockchain data. So this joins the live stages as something that needs building and testing separately.

---

## 10 Aug 2026, 20:00 — The last cost, and the answer

Measured how much a buy pushes the price against itself, on brand new coins:

| Order size | Buys measured | Price moves against you |
|---|---|---|
| under 0.05 SOL | 232,473 | 0.09% |
| 0.05–0.1 | 95,632 | 0.31% |
| 0.1–0.25 | 100,580 | 0.62% |
| 0.25–0.5 | 82,901 | 1.31% |
| 0.5–1.0 | 96,977 | **2.57%** |
| over 1 SOL | 100,504 | **5.84%** |

**This closes the case, because there is a squeeze with no way out.**

The tip is a **fixed** 0.0046 SOL. So the smaller your trade, the bigger a share the tip takes. But the bigger your trade, the more you shove the price against yourself. You cannot escape both.

Total cost by position size (tip + price moving against you, in and out):

| Position | Tip | Price impact both ways | **Total cost** |
|---|---|---|---|
| 0.10 SOL | 4.6% | 0.6% | **5.2%** |
| 0.25 SOL | 1.8% | 1.2% | **3.1%** ← best case |
| 0.50 SOL | 0.9% | 2.6% | **3.5%** |
| 1.00 SOL | 0.5% | 5.2% | **5.7%** |

The cheapest it ever gets is **about 3%**. The edge is **+1.34%**.

**So the on-chain-only strategy loses money at every position size.** Not close. Off by roughly a factor of two, at its best.

---

## 10 Aug 2026, 20:30 — The slow trade doesn't exist

Idea: stop trying to flip in 60 seconds. If you aim for 3x instead of 25%, a 3% trading cost stops mattering. So enter at 60 seconds instead of 3, and hold up to an hour.

**There is nothing to hold.** Across every group of coins, the peak comes a **median of 1 minute after launch** — which is exactly when we would be buying. Stretching the window to an hour finds nothing, because nothing is there.

Real exits over the hour (stop −15%, target +50%, platform fees off, **before** tips and price impact):

| Wallets in first minute | Coins | Result |
|---|---|---|
| under 50 | 34,034 | −2.30% |
| 50–99 | 1,146 | −2.98% |
| 100–249 | 934 | −0.97% |
| 250+ | 144 | −0.77% |

All negative. Adding the costs makes it worse.

**So both ends are now closed.** The fast flip loses to costs. The slow hold loses because these coins have no second act — they spike in the first minute and bleed out.

---

## 10 Aug 2026, 20:45 — The last on-chain idea, and why it failed

**Question asked:** do the coins that actually run big look any different, early on, from the ones that don't? If they look identical, that proves the useful information isn't on the blockchain at all — and social monitoring becomes worth paying for.

**They do look different.** Comparing the first minute of coins that later did 10× against everything else:

| First-minute feature | Normal (20,820) | Later did 10× (170) |
|---|---|---|
| Wallets in first 3s | 3 | **2** |
| Trades per wallet | 1.98 | **9.01** |
| Biggest buyer's share of the money | 0.351 | **0.189** |
| Price move in the first minute | ×0.99 | **×0.54** |

The winners are the opposite of what we'd been buying: fewer wallets, money spread thin, price **down 46%** in the first minute, and wallets trading over and over rather than buying once.

**Tested as a strategy** — buy at 60 seconds, stop at −30%, target 3×, hold up to an hour. On 6 Aug 2026 the best group returned **+5.44%** with a 12.8% chance of hitting 3×, against about −3% for everything else. That looked like a real find.

**Then tested on days we hadn't looked at:**

| Day | Churning wallets? | Coins | Result | Hit 3× |
|---|---|---|---|---|
| 10 Sep 2025 | yes | 309 | **−3.90%** | **0%** |
| 10 Sep 2025 | no | 32,924 | −3.39% | 0.86% |
| 6 Feb 2026 | yes | 2,117 | **−0.42%** | 4.58% |
| 6 Feb 2026 | no | 29,036 | −2.79% | 1.52% |

**Negative on both**, and on one it was *worse* than doing nothing clever. The +5.44% was a single day's noise.

**This is the whole reason for testing on untouched days.** It would have been easy to build a system around that number.

---

## 10 Aug 2026, 20:15 — Where it stands

| Stage | Result |
|---|---|
| 0 — can we get the data | **Pass** |
| 1 — is there time to make money | **Fail.** Fast, slow and dip-buying versions all fail |
| 2 — does the wallet graph help | **No**, in both forms tested |
| Social monitoring | **Untested** — needs a live feed, not Dune |
| 3–5 — live, execution, scaling | **Untested** — cannot be done from history |

**Plain answer: as designed, this does not work.**

The signal is real. Counting wallets in the first three seconds genuinely sorts good coins from bad, by a factor of 17. That part held up on days we never looked at.

It just isn't worth enough. The edge is +1.34% a trade and the cheapest possible cost of trading is about 3%. And the wallet graph, which was supposed to be the whole advantage, added nothing.

**Why, in one line:** every signal we tested is visible to every other bot at the same instant, so the price already reflects it by the time you can act.

**What is left to try:**

1. ~~**A much stronger signal.**~~ **Tested at 20:45 — dead.** Money split, arrival speed and repeat-trading were all tried. The best looked like +5.44% on one day and did not survive on days we hadn't looked at.
2. **Social monitoring.** The one input that isn't a public table everyone reads at the same moment. Back in scope, and now the most promising direction rather than a nice-to-have.
3. ~~**A different exit.**~~ **Tested at 20:30 — dead.** These coins peak within a minute of launch and never come back. There is no slow trade to make.

**Dune has now answered everything it can.** Stages 3 to 5 and social monitoring need a live feed and real trades, so they cannot be settled from history.

No money has been put at risk, and none should be until item 1 or 2 produces something much bigger than what we have.

---

## 10 Aug 2026, 21:05 — Change of plan: start collecting

Decision: stop analysing history, start recording the present.

**A listener already existed.** `~/Code/flux`, written 8 Aug — records every pump.fun launch and trade, with fee payers and outcomes. Zero dependencies, tested, verified live. The instinct was to build one; there was no need.

**But it had only ever run for 190 seconds.** 33 launches, 1,236 trades, on 8 Aug, then nothing. Two days gone that cannot be bought back — free Solana history at this detail isn't for sale.

~~**Now running properly.** Started under launchd with auto-restart, so it survives a closed terminal and comes back after a reboot.~~ **Undone at 21:20 — this wasn't what was asked for.** It was recording about **34 launches a minute** while it ran, which matches the ~36,000 a day measured on Dune — a good sign both are seeing the same world.

**Two holes remain:**

1. **No `FLUX_RPC` key.** The public endpoint works but lags ~9 seconds and rate-limits the fee-payer backfill. A free Helius key fixes both, and the fee payer is the one field that links wallets nobody else links.
2. **The Mac sleeping stops it.** launchd cannot fix a sleeping machine. A cheap VPS is the real home.

Disk: ~250–500MB a day, 102GB free. Not a concern for a long time.

---

## 10 Aug 2026, 21:20 — The program can see

**The always-on collector was the wrong idea and has been removed.** The plist is gone and nothing runs in the background any more. The ask was for the program to see what's happening when you start it, not to hoard files while you sleep. The 8 and 10 August recordings are kept; nothing was deleted.

**What was built instead.** `node src/cli.js`. Start it and it connects to Solana and shows pump.fun live. It has no dependencies, saves nothing, and buys nothing.

The four files that read pump.fun's raw bytes were copied from the earlier listener rather than written again — they were already tested against live mainnet, and one of them checks its own answers against pump's fee formula every single time it decodes a trade.

**What it looks like running:**

```
20:16:00  new   $Fractured  Fractured                65pr…pump
          ↳     $Fractured   28 wallets   19.07 SOL in  36 trades · +7.78 net
20:16:04  new     $BULLSHI  BULLDOG                  FUmE…pump
          ↳       $BULLSHI    2 wallets    0.12 SOL in  3 trades · +0.10 net
20:16:07  new      $Aircon  Air Conditioner          AKBh…pump
          ↳        $Aircon    5 wallets    6.84 SOL in  6 trades · +6.07 net
20:16:23  20 new coins/min · 2418 trades/min · 0 graduated · 0 gaps
```

Every new coin gets a line the moment it exists. Three seconds later it gets a second line: how many separate wallets bought, and how much went in. That count is the only signal measured so far that sorts good coins from bad, so it's the number on screen.

**What it showed on the first run — three things worth knowing:**

| What we saw | What it means |
|---|---|
| The spread is instant and huge | 28 wallets on one coin, 1 on the next, seconds apart. On screen the signal looks as clean as it did in the Dune tables |
| Three "Air Conditioner" coins in 15 minutes | Copies of whatever is working right now. The name is not the idea; the timing is |
| 20 new coins/min, not 35 | The free public endpoint is dropping about 40% of them |

**One number to fix.** Dune measured ~36,000 new coins a day, about 35 a minute. The watcher sees 20. The difference is the free public endpoint quietly dropping messages. A free Helius key in `STS_RPC` fixes it — worth doing before trusting anything counted on screen.

---

## 10 Aug 2026, 21:45 — The story behind a coin

The on-chain work is finished and it failed. Social was the one thread left, and it turned out to be much cheaper to start than expected — because the coins tell you themselves.

**Every launch carries a link to its own description file.** Picture, text, and usually a social link. Free to read, no key, no scraping. Measured on 80 real launches:

| | |
|---|---|
| Description file readable | **80 of 80** |
| Time to fetch one | ~1 second, worst case 6.6 |
| Has a social link | **76%** |
| Of those, points at **one specific tweet** rather than an account | **89%** |

**That last number is the finding.** A pump.fun coin is usually not a project with an X account. It is a bet on a single piece of news, with the receipt attached. So the question isn't "is this team real" — it's "what happened, when, and who noticed".

**And the receipt can be read too, for free.** For any linked tweet we can get the author, their follower count, how old their account is, how many likes and views the tweet has, and when it was posted — so we can work out **how old the tweet was when the coin appeared**.

First example checked: a coin linked to a tweet by a news account with 40,000 followers, posted **30 seconds before the coin existed**.

**Coins race the same story.** Of 46 linked coins, only 28 pointed at different things — **18 were launched on a story someone had already used**.

**What the watcher shows now.** Each coin gets its opening and its story on one line:

```
21:11:31  new      $AKIT47  The AK47 Kitten          8u6Q…pump
21:11:31  new      $AKIT47  The AK47 Kitten          3ZzB…pump
          ↳        $AKIT47    1 wallet · 0.00 SOL in   @Devilantesol · 4k · 1y · tweet 66m old
          ↳        $AKIT47    1 wallet · 0.00 SOL in   @Devilantesol · 4k · 1y · tweet 66m old · #2 on it
21:11:38  new       $Family  Family                  B3iy…pump
          ↳        $Family   19 wallets · 27.29 SOL in  @kuantkid · 2k · 3y · tweet 11s old
21:11:39  new        $DRAKE  DRAKE coin              8Nsj…pump
          ↳         $DRAKE    1 wallet · 0.11 SOL in   no link
```

Three identical AK47 coins in one second, all on the same hour-old tweet, all ignored. A coin on an 11-second-old tweet pulled 27 SOL. A coin with no link pulled 0.11.

**That is one minute of watching, not evidence.** It is the pattern we now have to prove or kill.

**So it writes coins down.** One line each to `data/coins-<date>.jsonl` — coin, story, opening, and what the price did over the next minute. The file is the test; the screen is just how you notice things worth testing.

**Two rules the file obeys, or the test lies to us:**

1. **The opening freezes at 3 seconds.** Wallet counts stop the moment a decision would have been made. Caught this as a live bug: one coin recorded 26 wallets when only 11 had bought by the 3-second mark. Left alone, the "signal" would have secretly contained the answer and every result after it would have been worthless. This is rule 3 from the README, and it nearly got us on the first try.
2. **Nothing is ever rewritten.** No scores, no verdicts. Only what was seen and when.

**What we need before grading:** a few days of coins. Then the question is simply whether a fresh tweet from a real account beats no link at all, by enough to matter — checked on days we deliberately didn't look at.

---

## 10 Aug 2026, 21:50 — How many coins are actually made

**Correction first.** At 21:20 the log said the watcher was seeing 20 coins a minute against a real rate of 35, and blamed the free endpoint for dropping 40%. **That was built on a 25-second sample and it was wrong.**

**The real rate, from Dune, for the whole of 8 Aug 2026:**

| Launchpad | New coins that day | Per minute |
|---|---|---|
| **pump.fun** | **34,076** | **23.7** |
| meteora | 2,494 | 1.7 |
| pumpswap | 1,108 | 0.8 |
| raydium | 861 | 0.6 |
| raydium launchlab | 220 | 0.2 |
| everything else | 84 | — |

About **1,400 an hour, 34,000 a day**. And **pump.fun is 88% of every new coin on Solana** — watching it alone misses very little. ("Launch" here means the first time anyone ever bought the coin, since the launches table has been broken since Dec 2025 — see 19:20.)

**The rate swings enormously.** Four minutes of live watching:

| Time | New coins/min |
|---|---|
| 21:41 | 32 |
| 21:42 | 10 |
| 21:42 | 4 |
| 21:43 | 4 |
| 21:43 | 2 |

An hour earlier the same endpoint was showing 47/min. **A one-minute sample says nothing about the rate.** Any future claim about volume needs at least an hour behind it.

**The watcher itself is sound.** Run side by side with the older listener for four minutes on the same endpoint: 42 coins counted vs 43. Two separate programs, same answer, so ours isn't silently dropping events.

**Still unknown: whether the endpoint drops them.** Both programs were listening to the same free endpoint, so agreeing proves nothing about completeness. That needs a second, paid endpoint to test — run ten minutes on it and compare against the 23.7/min daily average.

**One more thing worth knowing:** of the coins our listener saw created, only **81% ever got a single trade**. A fifth of all launches are born and never touched.

---

## 11 Aug 2026, 00:50 — Watching tweets move

Added the missing input: each linked tweet is now re-read at 30 seconds, 2, 5 and 10 minutes after we first see it. Views, likes, replies, retweets, quotes and the author's follower count, every time.

**Why one reading was never enough.** A snapshot tells you how big a tweet is. It cannot tell you whether it is still alive. From the first 10-minute run:

| Account | Tweet age when the coin appeared | Views over 5 minutes |
|---|---|---|
| @takaichi_sanae | **25 seconds** | 6,909 → **23,628** |
| @freakyfeelingx | 4.6 hours | 350,454 → 396,910 |
| @clownworld | 1 day | 136,760 → 136,947 |
| @brian_armstrong | **3 days** | 116,806 → **116,819** |

The Coinbase tweet has 116,000 views. On a single reading that looks like a huge story. **Thirteen views in five minutes** says it is a corpse someone dug up to name a coin after. Across all tweets followed, views roughly doubled in five minutes at the median, and the best grew 32×.

That distinction is invisible without the curve, and it is exactly the "is it botted, will it get views" question from the original idea.

**Run:** 10 minutes, 480 coins, 33,311 trades, 11 graduations, 0 gaps. 57 tweets followed, 117 samples.

**Design notes worth keeping:**

- Tweets are followed **once each, not once per coin.** Several coins launch on the same tweet, so polling per coin would be both wrong and rude to a free service.
- The launch reading becomes sample zero rather than being fetched twice.
- A tweet that stops answering is recorded as `gone` rather than skipped. Deleted tweets are a real pattern, not a gap in our data.
- Coins and tweets go to **separate files**, joined on the tweet id. The tweet outlives the coin's follow window, so tying them together would have forced one to wait for the other.

**One bug fixed on the way.** After a long run, Ctrl-C finished all its work and printed its summary but the process stayed alive. Could not reproduce it in short runs. Rather than leave it to chance: the ten-minute timers no longer hold the process open, and shutdown now force-exits after 10 seconds if anything stalls. A hang is worse than losing a second of data, because the next thing anyone does is kill it outright and lose everything buffered.

**Next:** collect across different hours for several days, then build the grading table. Nothing to decide until there is data.

---

## 11 Aug 2026, 02:00 — A dashboard, and the wallets behind each coin

Two additions.

**The watcher now records which wallets, not just how many.** Every wallet that touched a coin, what it bought and sold, how many trades, and how many seconds after launch it first appeared. Capped at 200 wallets per coin. This roughly triples the file — **416 → 1,301 bytes per coin**, so about 44 MB a day. Still nothing.

This was pure loss until now: every coin collected before this can never be graphed, because we only ever stored a count.

**And a dashboard.** `node src/cli.js dash` opens a local page. It only reads the files; it never connects to Solana and never writes.

First tab is wallets. Pick a coin, see who traded it. Bubble size is SOL moved. Brightness is how that wallet's *other* coins have done. Lines join wallets that keep turning up together elsewhere.

**Built around the mistake we already made.** On Dune we briefly thought two wallets sharing 100+ coins was a cluster — it turned out one of them bought 5% of every launch on the network. So:

- a link needs at least **two** shared coins, never one
- strength is measured against the **rarer** of the two wallets, so a busy bot cannot manufacture links
- clicking a wallet shows what share of **all** coins it appears in, and above 2% the panel says plainly that it is a bot and shared coins mean nothing

**First real thing it showed.** On `$Grampster` — 174 wallets — the biggest bought 5.93 SOL and sold 8.16, across 9 trades, **first seen 0.01 seconds after launch**. In before any human could act, out ahead of everyone, up 2.2 SOL.

**Honest limit:** this is a tool for looking, not a signal. Both versions of the wallet graph we tested on Dune added nothing. It exists so we can notice things we haven't thought to test.

**On packaging it as a .dmg and .exe.** Deliberately last. Unsigned builds are blocked by macOS Gatekeeper and Windows SmartScreen, so "click and run" needs a paid Apple developer account and a Windows certificate. And wrapping is a day's work whenever we want it, whereas doing it first would mean rebuilding a 200 MB app on every change to a page that is still changing hourly.

---

## 11 Aug 2026, 02:15 — It's an app now, and it's live

**Live.** The page follows the listener rather than the files. Coins appear as they finish, a strip along the bottom shows launches as they happen and fills in each one's opening a few seconds later, and a dot says whether it is really connected. Nothing needs reloading.

Done with server-sent events, so the page holds one connection open and the server pushes. No polling.

**An app.** `npm start` opens a window. It starts listening when it opens and stops when you close it.

**The design decision that matters: the app runs from the repo.** Updating is `git pull` and reopen. No rebuild.

The alternative — a packaged build — bundles a frozen copy of the code into the app, so `git pull` does nothing to it and every change means rebuilding **106 MB**. That is fine for handing to someone else and wrong for daily use while the thing is changing hourly.

`npm run dmg` works and was tested: `dist/STS-0.1.0-arm64.dmg`, 106 MB. Not notarized, so macOS will refuse it on first open until you right-click → Open. Real one-click needs an Apple developer account at $99/year and a Windows certificate.

**Two bugs found by using it:**

- A port clash printed a raw crash stack. For a terminal tool that is merely ugly; for an app it is unacceptable. It now steps to the next free port and says so.
- `type: module` in package.json broke Electron's main process, which wants CommonJS. Renamed to `main.cjs` — the only file in the project that knows Electron exists.

**Kept deliberately separate:** `src/` still has zero dependencies. Electron lives only in `app/`, and the watcher runs without it. If the app idea is ever abandoned, nothing has to be untangled.

---

## 11 Aug 2026, 02:25 — Fixing the live strip

The strip along the bottom was broken in three ways.

**It was 15 pixels tall.** It sat next to the canvas in a flex column, and flex items shrink by default — the canvas took the space and the strip was squeezed to almost nothing. Now `flex: none`, 104px.

**The times ran out of order.** Each line is written when a coin launches, then rewritten a few seconds later when its opening is known — and the rewrite was restamping it with the current time. So a line could show a later time than the lines below it. Now the stamp is set once, at creation, and kept. It reads as the launch time, which is what it should have meant all along.

**The listener's own status lines were written for a terminal** and far too long for a strip this size. Trimmed to the rates.

Reads like this now:

```
02:21:57  $RIAL     5 wallets · 6.91 SOL · no link
02:21:58  $MOONR    7 wallets · 7.72 SOL · @imblankface · 13k · 1y · tweet 24s old
02:22:04  $SeAlon   7 wallets · 6.60 SOL · @SantaCocaine69 · 98 · 1y · tweet 85s old
02:22:06  $OSHI     4 wallets · 6.70 SOL · @cakaldevs · 328 · 118d · tweet 1s old
02:22:06  $Trash   10 wallets · 13.53 SOL · @RFCRUZ · 49 · 17y · tweet 1s old
```

---

## 11 Aug 2026, 10:30 — First grading. The story alone is nothing; the tweet's curve might be something.

2,739 coins collected, 2,100 with both an opening and an outcome. First real cut.

**The story by itself does nothing.**

| bucket | n | 1.5x | 2x | 3x | avg net |
|---|---|---|---|---|---|
| fresh tweet (<5m) | 596 | 13.3% | 6.5% | 2.2% | −8.14% |
| older tweet | 456 | 5.7% | 2.6% | 1.1% | −5.76% |
| profile link | 196 | 6.6% | 4.6% | 1.5% | −4.22% |
| no link | 852 | 20.8% | 11.2% | 4.9% | −5.62% |

Coins with **no link at all** looked best, which should set off alarms — and it was an artefact.

**Why:** the multiple is measured from the price at 3 seconds. Fresh-tweet coins have already taken **5.63 SOL** by then; no-link coins have taken **0.49**. Eleven times more money has already moved the price before the clock starts. That is a ruler problem, not an edge.

Comparing coins with the same opening size:

| wallets at 3s | fresh tweet | no link |
|---|---|---|
| 1 | 1.0% | 5.2% |
| 2–3 | 4.2% | 16.5% |
| 4–7 | **8.2%** | **8.9%** |
| 8+ | **13.2%** | **12.7%** |

At four wallets and above the story makes no difference whatsoever.

**But the tweet's *curve* is a different matter.** Splitting by how much the tweet's views grew while we watched it:

| group | n | views grew | hit 2x | wallets at 3s |
|---|---|---|---|---|
| flat (dead tweet) | 278 | ×1.15 | 4.3% | 2 |
| middle | 278 | ×2.56 | 3.2% | 3 |
| **accelerating** | **280** | **×16.0** | **7.5%** | **1** |

**This one is not confounded the same way** — the accelerating group has *fewer* early buyers and still does better. That is the opposite direction to the artefact above, so it is carrying information the opening does not.

**And it is not established.** Four reasons to hold off:

1. The 3.2-point gap has a **7.8% chance of appearing by luck**. That is not a result.
2. n = 279 in the group that matters.
3. This is the set the rule was found on. Nothing is held out yet.
4. **I tried twelve exit rules and reported the best one.** That is how false positives are manufactured.

For the record, the best of those twelve — accelerating third, −15% stop, let winners run to 5x — came out at **+3.75% average net**. It is not a finding. It is what searching twelve rules on 279 samples produces.

**The number that is real: the median is −3.00% in every single variant.** Most trades simply lose the cost of trading. Any profit lives entirely in a handful of coins.

**What is missing, and it is the third time this pattern has appeared.** The stop-loss above is a guess, because we store entry, peak and end — not the path between them. A coin that dips 20% and recovers looks identical to one that never dipped. Stops cannot be tested honestly without the path.

Twice before, an input was needed and had not been recorded (wallet addresses, tweet curves), and everything collected until then was wasted for that question. Recording a few price points across the follow window fixes it, and every hour collected before that is an hour where no stop-loss can ever be tested.

---

## 11 Aug 2026, 03:10 — First grading of the social question

2,100 coins with a full record. First look, on the build set — **not held out, so none of this is settled.**

**The obvious answer is wrong.** Sorting by story, coins with *no link at all* looked best:

| bucket | n | reach 2x |
|---|---|---|
| fresh tweet (<5m) | 596 | 6.5% |
| older tweet | 456 | 2.6% |
| profile link | 196 | 4.6% |
| **no link** | **852** | **11.2%** |

That is backwards, which is the signal to look for a ruler problem rather than celebrate.

**Found it.** How much had already happened by the three-second mark:

| bucket | wallets @3s | **SOL in @3s** |
|---|---|---|
| fresh tweet | 4 | **5.63** |
| no link | 2 | **0.49** |

Fresh-tweet coins have taken **11× more money** before we even measure. Every multiple is counted from the price at 3s, so those coins start from a base that has already risen. The "advantage" of a no-link coin is that it hasn't moved yet.

**Comparing like with like, the story adds nothing:**

| wallets @3s | fresh tweet | no link |
|---|---|---|
| 4–7 | 8.2% | 8.9% |
| 8+ | 13.2% | 12.7% |

Same coin, same crowd, story makes no difference.

**But one thing did survive.** Using the tweet *curves* — whether attention was actually growing — 836 coins split into thirds:

| tweet | n | views grew | reach 2x | wallets @3s |
|---|---|---|---|---|
| flat (dead) | 278 | ×1.15 | 4.3% | 2 |
| middle | 278 | ×2.56 | 3.2% | 3 |
| **accelerating** | **280** | **×16.02** | **7.5%** | **1** |

**This one is not the ruler problem.** The accelerating group has *fewer* wallets at three seconds, not more — so its better outcome cannot be explained by having already been bought. It is the first result in this project that survives that test.

Being honest about size: 7.5% against 4.3% is roughly 21 successes against 12. That is marginal, on one build set, on a couple of days. It is a reason to keep collecting, not a reason to believe anything.

**Also added: the price path.** Every time a coin sets a new high or low against the entry, that moment is recorded. Peak-and-end could never say whether the dip came before or after the rise, so no stop-loss could be tested honestly. Now it can — `$SC` reached 5.29× and never dropped below 0.989, so a −15% stop would never have fired. Records go 1,301 → 2,329 bytes, about 79 MB a day.

---

## 12 Aug 2026 — Three speed-first UI mockups

Created a separate Figma file named **STS Trading Dashboard Mockups**. No watcher or application UI code changed.

The file compares three desktop directions using the same fake launch data:

1. **Fast List** — every launch in one compact table; nothing hidden.
2. **List + Details** — scan launches on the left and inspect one coin in a stable panel on the right.
3. **Alert Cards** — filtered candidates receive more space, with an explicit warning that filters may hide useful launches.

All three use a restrained dark theme, fixed information positions, keyboard hints, visible connection health, and colour only for meaningful status.

**Working direction for the next iteration: List + Details.** This is not a final selection; it is the version being developed next to keep progress moving while feedback is pending, and it can still be changed. Live launches stay visible on the left while one selected coin uses a stable detail panel on the right. The next design pass will apply the visual language of Crypto Intelligence V0.3.1 before any application UI is implemented.

**Working iteration created.** Added a fourth frame to the Figma file without removing or changing the original three concepts. It applies the Crypto Intelligence V0.3.1 visual language to the List + Details structure: the existing sidebar style, Inter typography, restrained dark panels, compact launch rows, fixed selected-token details, explicit frozen-opening data, social evidence, risk/data-quality notices, keyboard hints, and visible stream health. It uses fake data and has no execution connected. No STS or Crypto Intelligence application code changed.

---

## 12 Aug 2026 — Candidate console test build

Built a separate working candidate interface at `ui/candidates.html` and made the Electron window open it by default. The original wallet dashboard remains available at `/` and was not replaced.

The candidate feed does not show every launch. Its first transparent test rule requires at least eight buyers in the frozen opening and a confidence value of at least 55. The displayed percentage is an experimental heuristic, not AI confidence: early-wallet count carries most of the weight, with smaller adjustments for SOL flow, seller pressure, readable/fresh linked X evidence, failed metadata, and heavily reused social links. The detail panel states exactly why each coin passed and shows any cautions.

This is **not a rug detector or a safety verdict**. Current STS data cannot check every contract, guarantee sellability, or prove that a hidden coin is a rug. The interface says this explicitly.

Added Buy and Sell controls only as an in-memory paper position for testing the workflow. They have no wallet or private key, sign nothing, send no Solana transaction, and disappear when the app closes.

The saved candidate endpoint and live candidate event both use the same rule. The endpoint was tested against the current local log, and the scoring rule was checked with strong and weak fixtures. No commit was created.

**Expanded into a connected test application.** The candidate console now has five working views: filtered Candidates, All Launches, the animated Wallet Map, Paper Trades, and System status. All Launches reads the existing coin log; Candidates reads the shared filter endpoint and live candidate events; Wallet Map reads the original per-coin graph endpoint and restores the moving force-directed wallet constellation as a separate research view; Paper Trades reflects the temporary positions created from candidates; System shows the live connection and local corpus totals.

The main candidate screen remains stable and non-animated. The paper action was reduced to one compact rapid control: `−0.10 | 0.10 SOL | +0.10`. The left side reduces the temporary paper position and the right side increases it. One denomination is used deliberately to avoid clutter. The connected pages, JavaScript syntax, served assets, candidate endpoint, and wallet-graph endpoint were checked locally. Still no real execution and no commit.

**Product polish pass.** Renamed Candidates to Overview, moved All Launches to the bottom of navigation, and removed development/explanatory notes from the product-facing screens. Wallet Map now has a scrollable coin list instead of a dropdown. Its layout is initialized deterministically and fully settled before drawing, so nodes remain stationary under the pointer instead of jumping while someone tries to inspect them.

The paper control now separates amount selection from the action. Available amounts are 0.1, 0.5, 1, 2.5, 5 and 10 SOL; the selected amount stays selected after Buy or Sell. Buy and Sell then apply that amount to the temporary paper position.

Local duplicate protection was strengthened in two places. Electron now permits only one desktop instance/listener per machine, and the coin writer indexes existing mint addresses when it starts and rejects a mint already stored in any coin log. The duplicate rule was tested both within one run and across a simulated restart.

This does not make data team-wide. Each checkout still writes to its own ignored local `data/` directory. Sharing live observations between teammates requires a central ingest service and shared database; Git is not a safe real-time data transport. No commit was created.

**Paper trading foundation.** Removed All Launches from the primary sidebar and placed it inside System under Data & Admin. Rebuilt the quick trade input as a minus button, editable SOL amount, and plus button, followed by explicit Sell and Buy actions.

Paper trades are now stored persistently in the desktop app's local browser storage instead of disappearing whenever the page refreshes. Each purchase records coin, SOL cost, implied token quantity, recorded buy price and time. Sells use first-in-first-out lots and record sell price, proceeds, realised P&L and time. The Paper Trades screen now separates open and closed entries and shows open cost, realised P&L, prices, status and dates. Existing completed STS records provide opening and last-recorded curve prices; this is honest recorded-price paper trading, while truly live price ticks still require the next watcher/API extension. No real wallet or execution was added, and no commit was created.

**Live paper terminal.** Replaced the simple stepper with the requested speed strip: `-10 -5 -2.5 -1 -0.5 -0.1 | editable SOL amount | +0.1 +0.5 +1 +2.5 +5 +10`. Values adjust the editable order amount without placing an order; Buy or Sell remains the explicit final action.

The watcher now forwards every real observed trade to the dashboard with timestamp, mint, side, wallet, SOL size and curve price. The selected candidate turns those events into five-second OHLC candlesticks, displays the live price, and maintains a buyer/seller tape with wallet identifiers and trade sizes. Open paper positions are revalued from that same event price, keeping chart, tape and P&L on one source of truth. The backend retains a bounded in-memory live market window and provides it through `/api/market/:mint` when a screen is opened or revisited.

Old saved records did not contain every trade tick, so their chart is explicitly reconstructed from saved entry/high/low/end observations and labelled `SUMMARY DATA`; buyer/seller tape is available only for newly observed live trades. No fake historical candles or wallets are generated. The watcher must be restarted once for live market events to begin. No real funds, wallet signing or repository commit was added.

**Trading workflow correction.** Moved the full candlestick chart, wallet trade tape and rapid order controls out of Overview and into Paper Trades. Overview is again a fast discovery screen, with only an optional editable quick paper-buy and a button that opens the selected coin in the full trading view. Paper Trades now has its own coin selector, chart, live/recorded price, order entry, positions and history in one scrollable workspace.

Fixed the main content sizing so long views scroll instead of being cut off at the window edge. The paper history has its own horizontal overflow for narrow windows. Wallet Map now includes zoom-out, reset and zoom-in controls, uses a grab cursor, and lets a wallet be clicked to keep its details visible while inspecting it.

The current watcher covers Pump.fun launches and therefore already sees all launches on that source, while Overview intentionally filters them. Established or externally launched assets such as DOGE or TRUMP need a second market-data adapter (for example a DEX/aggregator feed) before their paper prices and candles can be genuine. No placeholder external prices were added. No commit was created.

**Screen-recording correction.** Removed every paper-trading control and trading-view link from Overview; it now contains research evidence only. Kept scrolling functional while hiding visible scrollbars across main views, candidate lists, coin lists, trade tape, paper history and the System launch list.

System no longer sends the user to a separate All Launches page. Connection state, local corpus statistics and the full scrollable All Launches table now remain together on the System screen. A candidate launch in that table can still be opened directly in Paper Trades. No commit was created.

**Chart, wallet map and backtest data repair begun.** Replaced the fragile chart renderer with a stable renderer that supports both reconstructed legacy points and newly saved OHLC candles, includes a price scale, handles empty datasets explicitly and sizes itself from the visible Paper Trades panel. Rebuilt the wallet layout as a deterministic static map with working node selection, persistent wallet detail, fit/reset and spacing controls; it no longer zooms the entire canvas away from its pointer coordinates.

The watcher now builds compact one-second candles for every coin during its observation window. Each candle stores second-after-launch, open, high, low, close, SOL volume, buy count and sell count. New records save those candles under `market`, making charts survive restarts and supplying ordered, non-fabricated price paths for later strategy replay. Displayed prices are now correctly normalized to SOL per whole token using Pump's token/SOL decimal difference.

Added an initial read-only `/api/backtest` engine for candle-equipped records. It replays take-profit, stop-loss and maximum-hold rules, uses the stop first when both levels occur inside the same one-second candle (the conservative assumption), and reports sample size, wins, win rate, average multiple and per-coin exits. Existing records remain excluded because their sparse turning points cannot honestly establish intrabar ordering. The useful sample begins accumulating after the upgraded watcher restarts. No commit was created.

**Paper trade feedback.** Added compact non-blocking trade confirmations inspired by rapid trading interfaces. Successful simulated fills play a short rising ding and display `BOUGHT … SOL` or `SOLD … SOL` with the ticker. Invalid amounts, missing prices and attempts to sell without a position play a low buzz, request a short vibration where supported, and show the exact failure reason. Notifications disappear automatically and never interrupt the trading controls. No real execution was added and no commit was created.

**Live Overview firehose.** Overview now shows every Pump.fun launch as soon as the watcher detects it instead of waiting for the candidate filter. A new row begins in an observing state, fills in opening wallets and SOL after the measurement window, and later receives its saved outcome. The list remains newest-first and retains up to 300 recent launches. Outcome values on the right have returned to multiples such as `1.31x`; coins still being observed show a dash rather than a percentage.

Restored the compact bottom-left operating counters: listening state, launches seen this session, coins in the saved log and unique wallets seen. Trade confirmations now appear in the exact centre of the screen, and successful fills use a three-note cash-register-style chime while failures retain the low buzz. No commit was created.

**Mint-first trading and wallet cleanup.** Added a one-click Copy Mint action beside the selected Overview coin. Paper Trades no longer uses the long coin dropdown: it accepts a pasted mint address and loads on Enter or the Load button. The lookup uses the saved coin endpoint, so any coin recorded by this STS dataset can be opened even when it is no longer present in the recent Overview list; unknown mints produce a clear failure notification.

Simplified Wallet Map controls to one Fit Map action. Reduced the visual link count to stronger recurring relationships, coloured wallets by net buying or selling, added a clear selection ring, and condensed the pinned detail card to bought, sold, net SOL, trade count and corpus appearances. No commit was created.

**TradingView chart upgrade.** Replaced the provisional canvas chart with TradingView Lightweight Charts 5.1, installed as a local application dependency and served by STS rather than relying on a CDN. The Paper Trades chart now supports mouse/touch panning, wheel/pinch zooming, a TradingView-style crosshair, time and price axes, candlesticks, a separate SOL-volume scale, automatic fitting and live redraws from the existing STS market stream.

Added 1-second, 5-second, 15-second and 1-minute timeframe controls. Paper entries appear as a labelled horizontal average-entry line, while paper buys and sells appear as arrow markers on the chart. The chart retains the existing buyer/seller tape and current-price display. A subtle TradingView creator credit is included to comply with the Lightweight Charts license. No market data is supplied by TradingView; all candles remain sourced from STS observations. No commit was created.

**Temporary users and any-mint Solana paper trading.** Added the requested local startup chooser for CL, ethan, vincent and eye. Each temporary account begins with its own 10 SOL balance and keeps separate paper positions and history in desktop browser storage. Buys debit the selected account, sells credit the actual simulated proceeds, insufficient-balance orders fail clearly, and the active user/balance stays visible in the header. This is a local prototype rather than authentication: there are no passwords, server identities or security guarantees yet.

When a pasted mint is absent from the STS launch corpus, Paper Trades now queries DEX Screener's documented Solana token-pair endpoint, selects the highest-liquidity active pool, reads its USD price, converts it to SOL using a live wrapped-SOL reference, and starts polling approximately every two seconds. Those genuine quotes update the TradingView chart, current price and paper P&L; collected ticks persist locally for later candles. This free provider path supplies aggregated pool pricing rather than individual wallet transactions, so the buyer/seller tape remains empty for external coins instead of inventing trades. Native assets on other chains, including native DOGE, still require chain-specific adapters. No commit was created.

**Private-looking username entry and live-market preference.** Replaced the four visible account buttons with one blank username field. Usernames are matched case-insensitively against the temporary local account set, and invalid entries return only `Username not recognised` without listing valid names. This improves normal-screen privacy but remains a local prototype whose usernames can be discovered by inspecting application files; actual privacy still requires server-side authentication.

Changed pasted-mint resolution to prefer the active external Solana market before consulting STS's saved launch record. Previously, any coin already present in the STS corpus loaded its frozen 60-second snapshot and never started polling, which made the TradingView chart appear stuck. Actively traded pasted mints now start the approximately two-second quote loop even when STS scanned them previously; the saved record is used only when no current external pool is available. No commit was created.

**Live chart viewport repair.** Removed the initial `fitContent` behavior that compressed every locally retained tick into a severely zoomed-out chart. A newly loaded market or timeframe now opens on approximately the latest 80 candles with a small right-side margin. The chart detects whether the user is currently at the live edge: incoming bars remain visible and scroll forward automatically, while manually scrolling into older history disables the forced follow until the user returns to the right edge. Timeframe changes reset to the useful recent range. No commit was created.
# 2026-08-12 — Paper terminal functional repair

- Fixed a stale-selection bug where a pasted mint could load on the chart while Buy/Sell still submitted the previously selected coin.
- Recorded OHLC candles and new live quote ticks are now combined, so historical context no longer prevents the current candle from updating.
- Added keyless GeckoTerminal OHLC history for external Solana markets when available.
- Saved STS mints now load locally first; unknown mints fall back to the external live-market provider.
- Added a 10-second total deadline and a clear retry message for stalled market providers.
- Removed a duplicate chart redraw from each external price poll.
- Removed the server-sent event name collision with the browser's built-in connection `open` event.
# 2026-08-12 — Integrated Ethan's ranked-board branch

- Integrated the unmerged `agent/best-to-buy-board` work with the paper-trading terminal rather than replacing either side.
- Added continuous coin tracking, wallet-registry scoring, cost modelling, strategy replay, Dune ingestion, the ranked attention board and wallet-registry screen.
- Preserved the newer TradingView chart, local paper accounts, any-mint Solana lookup, buy/sell feedback and wallet-map repairs.
- Resolved the server-sent-event `open` naming collision while retaining Ethan's fresh-coin tracker handoff.
- Verified JavaScript syntax across the combined backend and UI; `/api/status` and `/api/best` both start and respond successfully.
# 2026-08-12 — Restore merged UI behaviour

- Restored the four compact sidebar readings: listener state, launches seen this session, saved coins and unique wallets.
- Restored centred animated `BOUGHT`/`SOLD` confirmations, the three-note successful-fill chime, and the failed-order shake/buzz feedback.
- Removed the position-size selector from Overview; paper-order sizing remains exclusively in Paper Trades.
- Fixed an empty Overview after startup by temporarily showing recent saved qualifying launches while Ethan's live ranked tracker warms up.
# 2026-08-12 — Overview compatibility repair

- Removed the marked `SCORED BY WHO IS BUYING / Worth a look` panel heading.
- Made Overview fall back to the ordinary saved candidate feed if the running backend does not yet expose `/api/best`.
- The selected time window is respected when recent candidates exist; otherwise the latest 40 qualifying coins are shown so the screen never remains stuck on `Loading`.
# 2026-08-12 — One application UI

- Promoted the interface developed throughout this session to the application root (`/`).
- The Electron desktop window now opens the root route instead of a special test URL.
- Removed the obsolete original `ui/index.html`; there is now one UI to maintain and test.
- Restored the saved-coin `view` payload required by mint-first paper trading.
# 2026-08-12 — Foundation reliability pass

- Removed a duplicate live candidate event that could send `null` and repeatedly crash UI updates.
- Added a client guard so malformed live events cannot stop Overview, Paper Trades or Wallet Map refreshes.
- Made Wallets work without Dune credentials by ranking thousands of repeat wallets from the local STS corpus, including detail, performance, seen coins, watch and export.
- The richer Dune registry remains preferred automatically whenever it is available.

# 2026-08-16 — Hold windows anchored to the entry

- Every second in a replayed path is now counted from the entry rather than from the launch. The recorder stamps candles, ladder rungs and the peak with seconds since launch, and `pathOf` was passing those straight through while seeding the entry at second zero, so the two clocks were mixed in one list.
- `maxHoldSec`, `dumpAtSec`, the `holdSec` a trade reports and `observedSec` are therefore all the same quantity now: seconds a position was held. `creatorDumpSecond` returns a hold rather than a launch second for the same reason.
- The consequence that mattered: a coin followed for 60 seconds and entered at 3 was only ever held for 57, so `observedSec` is 57, not 60. Under the old reading a 60-second hold was answered by 57 seconds of data and every `holdSec` read three seconds high.
- **The default hold moved from 60s to 57s** (`OBSERVED_HOLD_SEC`, which is watch.js `follow` minus `seconds`, held in step by a test). This is not cosmetic. Left at 60 the engine correctly reports that the corpus cannot answer the question — and then drops 1,502 of 2,602 replays as unobserved. The coins that survive are the ones that reached a level, which is to say the ones that moved, so the sample is selected on its own outcome: `buy-everything` comes back as **+163%** instead of the ~95% loss it is. That number is an artefact of the dropped tail, and it is a much larger lie than the three seconds this change set out to fix.
- With the hold at 57s the headline numbers are unchanged from before the clock fix: `buy-everything` -94.9% over 1,526 trades, `basic-momentum` -95.5% over 738, `syndicate-sniper` -27.3% over 36 (34 from the SQLite store, which collapses 116 re-observations). 57-from-entry and 60-from-launch select the same coins, so the P&L baseline did not move — what moved is that the definitions are now true and `holdSec` is a real hold.
- Reported hold times are now honest: the sniper's average hold reads 50.56s against a 57s limit, and the longest trade reads 57s rather than 60s.
- 150 coins in the corpus were recorded with a shorter follow window (`observedSec` of 42 and 37). A 57-second hold genuinely cannot be answered for those, and they are now correctly counted as unobserved rather than resolved at their close.
- Checked on all 2,602 priceable paths: no point at a negative second, no point past what was observed, no exit dated past what was observed, and every path starts at second zero. 23 paths open with a price below 1x at second zero — the entry second's own low sorting ahead of the entry marker, which is the pessimistic within-second ordering working as intended and is inert for every exit rule.
- Six tests updated to the entry clock and three added, including one that pins `OBSERVED_HOLD_SEC` to watch.js's defaults so the two cannot drift apart silently.
# 2026-08-16 — syndicate-sniper v2: the signal has to fire on a group worth following

A tag on a launch says a signal fired *somewhere* in the opening three seconds. It does not say it fired on wallets worth following, and v1 of the entry rule never asked. Four checks were added after the score and primary-signal tests, all asking the same question from different sides.

- **Three wallets before it is a group.** cluster.js already refuses to tag a bundle or a repeated size under three, so on this corpus the check rejects nothing. It is in the gate anyway so the entry rule does not silently inherit a constant from the analyser, and so the funnel can name the case if it ever happens.
- **The bundle's own sizes have to match, within 1%.** This is the one that did the work. The analyser groups sizes at 2%, which is right for *spotting* a scripted amount through a different priority fee; this is a tighter test and a different question — not "did somebody repeat a size somewhere in this launch" but "did the wallets that landed together also take the same position". Those can be, and usually are, disjoint sets of wallets.
- **A deployer buying its own launch is not a syndicate on its own.** When `CREATOR_BOUGHT_OWN` is the only primary tag, the matching group now has to contain three wallets that are not the deployer.
- **The group has to have committed at least 1.5 SOL.** The whole thesis is that the bundle's exit is the trade. A group that put in less than that cannot move the price on the way out either.

Read against the full corpus, `node scripts/run-backtest.js --input data` over 3,324 recorded coins, same balance and same costs on both sides. The old rule is still runnable as `syndicate-sniper-v1` so the comparison is two live replays rather than a number quoted from an old checkout.

| | syndicate-sniper (v2) | syndicate-sniper-v1 | basic-momentum | buy-everything |
|---|---|---|---|---|
| trades | **22** | 36 | 738 | 1,526 |
| won / lost | 0 / 22 | 2 / 34 | 192 / 546 | 325 / 1,201 |
| win rate | **0.00%** | 5.56% | 26.02% | 21.30% |
| profit and loss | **−1.7953 SOL** | −2.7338 SOL | −9.5464 SOL | −9.4912 SOL |
| of the balance | −17.95% | −27.34% | −95.46% | −94.91% |
| max drawdown | **17.95%** | 27.34% | 95.77% | 95.51% |
| expectancy a trade | **−0.0816 SOL** | −0.0759 SOL | −0.0129 SOL | −0.0062 SOL |
| sharpe / sortino (per trade) | −2.85 / −0.92 | −1.31 / −0.85 | — | — |
| average hold | 55.45s | 50.92s | — | — |

**The honest reading of that table is that the filters did not find a better sub-population.** The loss and the drawdown both fell by about a third, and they fell because the rule trades 39% less — the fourteen launches it dropped had a combined −0.9385 SOL, which is exactly the difference. Per trade it got slightly *worse*, −0.0759 to −0.0816 SOL, and both of v1's two winners were among the fourteen dropped: pisscat (+0.2178, hit its target) and a coin whose ticker is a single full stop (+0.0144, ran out of clock). Cutting a losing sample nearly in half and losing both winners in the process is not evidence of an edge; it is a smaller sample of the same thing. At 22 trades it is also under the 30 the engine will report a rate for, so every percentage in that column describes those 22 coins and estimates nothing about the next one.

Friction is unchanged and still comes off both legs of every trade: the run charges itself 5.05% a round trip (150 bps a leg plus 0.005 SOL a leg on a 0.5 SOL position) against the 3.52% cost.js measures at that size, so the P&L above is the pessimistic end of the band, not the optimistic one.

The gate over all 3,324 launches: 801 nobody bought, 1,438 too thin to read, 1,049 read as ordinary, **12 landed together but took unrelated sizes**, **2 were coordinated but committed under 1.5 SOL**, 22 entered. 0.66% of launches, down from 1.08%.

- The sizing test is doing essentially all of the pruning. The launches it caught are the shape it was written for: KAMIKAZE had seven wallets in one bundle holding 10.81 SOL between them, and the largest matching group inside it was **one wallet on 0.0099 SOL**. ONE had nineteen wallets in a bundle worth 20.30 SOL and a matching group of two holding 0.12. Those are queues at a busy launch, not scripts.
- The wallet-count check and the solo-dev check reject nothing here, for different reasons. The first is already implied by the analyser's own minimum. The second is unreachable at a 0.6 score threshold: a launch whose only primary tag is `CREATOR_BOUGHT_OWN` has, by construction, neither an exact repeated size nor a same-instant bundle, and without those two signals it cannot score high enough to be asked the question. Both are tested by hand-built fixtures rather than by the corpus, and the fixture for the solo-dev case says in its own comment why it has to lower the threshold to be reachable at all.
- Cross-checked against the SQLite store, which holds 3,208 of the same coins: 21 trades against v1's 34, −1.6977 SOL against −2.5387, expectancy −0.0808 against −0.0747. Same direction, same sign on the per-trade move.
- The deployer-dump exit now fires zero times on the corpus: the only two gate-clearing coins that carry per-second candles were both pruned, so `syndicate-sniper` and `syndicate-sniper-no-dump` are now identical over this data.
- Eleven tests added to `test/strategy.test.js`, on three new hand-built fixtures — a bundle that is a queue, a group too small to matter, and a deployer buying alone — plus direct tests of the group-finding itself, including the case where scanning from the smallest position finds two wallets and the correct answer is three. Every score in those fixtures was worked out from cluster.js's weights by hand, so a change to those weights fails here with an arithmetic mismatch rather than passing quietly. The whole suite passes — 209 tests at the time of writing.
- `analyzeLaunch` now reports the member addresses and the total SOL of each bundle it found. The gate needs both, and re-deriving the bundling in a second file is how two answers to one question start disagreeing.
# 2026-08-16 — Paper trades are kept in sts.db

The paper terminal held every position in the browser's localStorage. That is one key in one browser on one machine: clearing site data wiped the record, the desktop app and a browser tab kept separate books, and there was nothing to read the trades back from later. The record now lives in the database, and the browser is only a view of it.

- **A new `paper_trades` table.** One row is one position rather than one button press — entry and exit share the row, so an open position is a single thing to update instead of two rows to pair up afterwards. `side` is the direction the position is held in, so BUY is long and SELL is short, and closing a BUY sells. Status is OPEN, CLOSED or CANCELLED, and both it and the side are held to their values by CHECK constraints as well as by the code.
- **Two clocks, and the column names say which.** `entry_sec` and `exit_sec` are seconds, the clock the chart is drawn on, so a fill can be put on a candle without arithmetic. `created_at` and `closed_at` are milliseconds, matching every other table here.
- **P&L is computed on the way out, from the position's own entry, size and direction.** It is never taken from a number the caller sent alongside the exit, because a caller that can send its own P&L can send the wrong one. SOL is kept to the lamport, the percentage to four places. Fees are deliberately not in it — these are paper trades, and `cost.js` still holds the real round-trip cost for anyone who wants to take it off.
- **Three endpoints.** `GET /api/paper/trades` returns the open positions whole, the closed and cancelled record a page at a time, and totals counted across all of it rather than across the page, so the P&L at the top does not change as you scroll. `POST /api/paper/order` fills and records one. `POST /api/paper/close` closes one and works out what it made. An order or a close that names no price fills at the price the board is showing — the live tape, then the opening window, then the tracker, then the last price written down — and is refused outright if nothing has seen a price, rather than filled at nothing.
- **Paging is by trade id, not by offset.** Trades arrive while someone is reading, and an offset would show the same trade on two pages or skip one entirely.
- **A refusal is a sentence.** A bad order comes back 400 with what was wrong with it; a position closed twice comes back 409 carrying the close that already stands, not a second exit written over the first; an unknown id is a 404. The database write is guarded on `status = 'OPEN'`, so a double-clicked button cannot close a position twice even if both requests arrive at once.
- The dashboard now opens its own connection to `sts.db`, so paper trading works with `--browse` and with a watcher told not to save. `busy_timeout` was added to the pragmas because two writers now share the file — without it the loser of that race is refused immediately, and the write being refused would be the trade someone just placed. A database that will not open costs the paper screen and nothing else; everything else on the board is files.
- 47 tests in `test/paper-trades.test.js`: the schema and its constraints, what an order is allowed to be, the P&L in both directions, paging, and the endpoints over real HTTP. The load-bearing one stops a server, starts another on the same directory and finds the position still open — which is the whole reason for the change. One of them found a real fault while being written: a short closed at exactly its entry stored −0, which reads as a loss that did not happen.
- Not done here: the front end still writes to localStorage. Pointing it at these endpoints is the next step, and until then the terminal has a server-side record beside it rather than behind it. 209 tests pass.
# 2026-08-16 — The terminal reads and writes the server record

The screen now draws from `/api/paper/trades` and places its orders through the endpoints. Nothing about paper trading is kept in the browser any more.

- **Buy and sell are requests.** A buy posts the price that is on the screen rather than letting the server quote its own, because what was on the screen is what was agreed to. A sell walks the open positions oldest first and closes each in turn, exactly as it did before.
- **Partial sells needed the server to learn how.** The amount box has always allowed selling part of a position, and the old code split the row in the browser. `POST /api/paper/close` now takes a `sizeSol`: the part sold keeps the id and becomes the closed row, and what is left is reopened at the original entry price and the original opening time. It is the same position, still measured from where it was bought — not re-bought at today's price. Both rows are written in one transaction.
- **The three figures at the top come from the server**, which counts them over the whole record rather than over the hundred rows on screen, so the P&L does not change as you scroll.
- **The account balance is no longer stored anywhere.** It is the starting 10 SOL, less what is tied up in open positions, plus what the closed ones made — both of which the server already counts. It survives a reload for the same reason the trades do.
- **One book, not four.** The login gate still asks for a name and still shows a balance, but the trades behind it are the one record in `sts.db`, so the four usernames now share it. The table has no account column and inventing one was not part of this. If the separation is wanted back it is a column and a filter.
- **"Clear history" is now "Refresh".** There is nothing local left to clear, and no endpoint deletes trades — deliberately. The button re-reads the record instead, which is also how a trade placed in another window turns up in this one. The record is re-read on the same five-second beat as the board.
- The chart, its entry line and its buy/sell markers needed no changes: the rows are mapped into the shape the screen has always drawn, so `terminal-repair.js` never learns where they came from.
- Found while testing this: a request for `//` killed the whole dashboard. `new URL()` was parsing the request path outside the handler's try, so a stray link or a port scanner took the process down instead of getting a 500. Fixed, with a test.
- Nine more tests, 218 in all. The wiring was run end to end against a live server — buy, sell part of it, sell the rest — and the balance came back to exactly 10 SOL. What is not verified is how any of it looks: that needs the app open in front of someone.
# 2026-08-16 — One command starts it, one key stops it

`npm start` now runs `bin/sts.js` and brings up all three parts together: the socket reading the tape, the dashboard serving the board, and the connection to `sts.db` the paper record is kept in. It was `electron .`; the desktop window is still there as `npm run app`.

- **One process, on purpose.** The listener hands coins to the dashboard in memory rather than through a file, and Ctrl-C has one thing to wait for instead of three. There was never a separate paper process to start — a paper order is served by the dashboard and stored in the database — so the banner says where the record is being kept rather than pretending to have launched something.
- **The banner is three lines**: where the board is, what the listener is pointed at, and which file the paper record is in. The endpoint is printed through `redact()`, so an RPC URL with a key in it does not end up pasted into a chat.
- **Ctrl-C stops things in an order that matters.** The socket closes first so nothing new arrives; coins still inside their follow window are written out as they stand, because a short record is a fact and a missing one is a hole; the wallet rollup is rebuilt from what was stored; then every database connection is closed. A second Ctrl-C gives up immediately, and a shutdown that takes more than ten seconds exits anyway — a process that will not quit gets killed, and being killed is the one way to leave a database mid-write.
- **How the tests know the database was really closed.** SQLite in WAL mode keeps `sts.db-wal` and `sts.db-shm` beside the file while a connection is open and folds them back in when the last one closes. So the test asserts they exist while it is running and are gone after the interrupt. That is evidence about both connections, the listener's and the dashboard's, that no amount of reading the shutdown function would give.
- The listener's status lines now go to the terminal as well as to the browser. They only ever went to the window, which was fine when the window was the only way in and useless when the terminal is where you started it.
- Seven tests in `test/startup.test.js`, none of which touch the network: the listener is aimed at a port with nothing behind it, which is a real socket failing and retrying — the state Ctrl-C usually finds it in anyway. 232 tests pass.
- Checked against the real endpoint too, for fourteen seconds: connected, subscribed, 3 launches and 535 trades recorded, and everything written out and closed on the way down.
