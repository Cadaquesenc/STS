# What is it?
A Solana trading system I built to trade pump.fun launches, it records every launch as it happens, rebuilds the bonding curve from the coin's own trades and replays the recording through a decision and execution pipeline so the pipeline can be tested against something real. I worked on it for seventeen days and then measured whether the trade it was built for actually makes money. It doesn't, and it never did, and that is what this repository is for now.

## What I wanted it to be
Something that reads a launch better than the people buying it. Wallet clustering to see who is really behind a coin, funding traced back through the graph, social signals folded in, an EV engine that only takes trades it can explain, forensics good enough that every buy has a reason you could read back a month later. Running local on my own machine, no hosted anything, off a €200 stake, in the difficult middle of the market where I thought the lottery snipers weren't looking.

## What I built
106,000 lines of Rust behind a Tauri window. A recorder that captured 12,089 real launches. Exact integer pump.fun curve arithmetic, checked trade by trade against the chain. Wallet clustering with funding traced to 24 hops, a hash-chained audit log, Jito bundle construction, sandwich and re-org cost models, walk-forward splits, 1,654 passing tests, clippy and fmt clean. All of it real and all of it works. None of it ever asked whether the trade made money.

## When I found out it doesn't work
27 August 2026. Buying launches and selling inside the minute loses 7.76% a trade, and 0.86% at literally zero fees, so it isn't the fees, it isn't the machine, it isn't the stake and it isn't the bankroll, the loss is there before anything is charged. Every signal I had is real and none of them is worth anything, tweet reuse and repeat early buyers and order flow all sort outcomes at ten to fifteen standard deviations and all of them predict the finish equally hard in the opposite direction, they detect which coins move and not which way. Then I swept seven hour-matched windows back to October 2024 and expectancy was never positive in a single one, minus 6.5% to minus 17.1%, and in October 2024 with only 2.7 rival buyers in the first three seconds this trade still lost 7%. The edge was not competed away, there was never an edge to lose. The money in this market is real and it goes to the person who launched the coin, creators staked 13,130 SOL and took out 28.8% while everyone else staked 62,235 for minus 8.1%, the drift everyone else is fighting is somebody's income.

## Why I didn't find out sooner
Of the 40 gates on the roadmap only two required being right about the market and the first of those is 23rd in the sequence, so twenty-two of them could go green, honestly earned, before anything at all checked the only thing that mattered. The spec asked for a system that was consistently profitable and essentially undebunkable, and you cannot edit a spec into being profitable but you can absolutely edit it into being undebunkable, and every one of those edits looks like rigour while you are making it.

## Run it
`cd src-tauri && cargo test`

Rust stable and the Tauri 2.0 prerequisites for your platform, `cargo tauri dev` opens the window, `cargo tauri build` if you want a binary. There is no exit rule, no paper mode and no entry-side transaction builder anywhere in here, those are the three things a trading system needs and they are the three that were never written, so it cannot trade and never has.

## The writeup
`docs/VERDICT-2026-08-27.md` is the numbers and the method, `docs/POSTMORTEM-2026-08-27.md` is why a careful seventeen days never once tested the premise. The trading answer is specific to pump.fun. The other one isn't, and it is the reason I put this up.

Nothing here is advice, the measured result for this strategy is a loss, and if you take a number out of this repository check it against the verdict first.
