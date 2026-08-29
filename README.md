# what is it?
a solana trading system i built for pump.fun launches, it records every launch as it happens, rebuilds the bonding curve out of the coin's own trades and replays the whole thing back through a decision pipeline. i worked on it for seventeen days and then actually measured whether the trade makes money, it doesn't, and it turns out it never did

## what i wanted it to be
something that reads a launch better than the people buying it, wallet clustering to see who is really behind a coin, funding traced back through the graph, an ev engine that only takes trades it can explain. all local on my own machine, off a €200 stake, in the middle of the market where i thought nobody was looking

## what i made
106k lines of rust behind a tauri window, a recorder that caught 12,089 real launches, exact pump.fun curve arithmetic checked trade by trade against the chain, wallet clustering, a hash-chained audit log, 1,654 passing tests. all of it works. none of it ever asked whether the trade made money

## when i found out it doesn't work
27 august 2026. buying launches and selling inside the minute loses 7.76% a trade, and 0.86% at literally zero fees, so it isn't the fees and it isn't the machine and it isn't the stake, the loss is already there before anything gets charged. every signal i had is real and none of them is worth anything, they tell you which coins are going to move and never which way. then i swept it back to october 2024 and it was never positive in a single window, so the edge wasn't competed away, there was never an edge. the money here is real though, it just goes to whoever launched the coin

## why it took me so long to notice
of the 40 gates on my roadmap only two needed me to be right about the market and the first of those is 23rd, so twenty-two things could go green, properly earned, before anything checked the only thing that mattered

## run it
`cd src-tauri && cargo test`

rust stable and the tauri 2.0 bits for your platform, `cargo tauri dev` opens the window. there's no exit rule, no paper mode and no buy builder anywhere in here, the three things a trading system needs are the three i never got round to, so it can't trade and never has

## the writeup
`docs/VERDICT-2026-08-27.md` is the numbers and how i got them, `docs/POSTMORTEM-2026-08-27.md` is why i didn't find out sooner. the trading answer is just about pump.fun, the other one isn't, that's the reason i put this up

nothing here is advice, the measured result is a loss
