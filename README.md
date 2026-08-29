# STS

> i built a solana trading system for seventeen days.
> on day seventeen i measured whether the trade made money.
> it does not, and it turns out it never did.

## what i wanted it to be

something that reads a launch better than the people buying it. wallet clustering to see who is really behind a coin, funding traced back through the graph, an ev engine that only takes trades it can explain. all local on my own machine, off a €200 stake, in the middle of the market where i thought nobody was looking.

## what i made

106k lines of rust behind a tauri window. a recorder that caught 12,089 real launches. exact pump.fun curve arithmetic checked trade by trade against the chain. wallet clustering. a hash-chained audit log. 1,654 passing tests.

all of it works. none of it ever asked whether the trade made money.

## what happened

27 august 2026.

buying launches and selling inside the minute loses **7.76% a trade**. at literally zero fees it still loses **0.86%**.

so it is not the fees, it is not the machine and it is not the size of the stake. the loss is already there before anything gets charged.

then i swept it back to october 2024 and it was never positive in a single window. the edge was not competed away. there was never an edge.

every signal i had is real, and none of them is worth anything. they tell you which coins are going to move and never which way.

the money in this market is real. it just goes to whoever launched the coin.

## why it took me seventeen days to notice

this is the part i actually put the repo up for.

my roadmap had 40 gates. of those 40, exactly **two** required me to be right about the market, and the first of those two is number 23.

so twenty-two things could go green, properly earned, real work, passing tests, before anything checked the only assumption the entire project rested on.

that is not a discipline failure. i was extremely disciplined. i was disciplined about the wrong ordering. the gate that can kill the project belongs at position one, and it is always the least fun one to build, which is exactly why it ends up at 23.

## what i learned

- order your milestones by what can kill the project, not by what can be built next. the cheapest test of the core assumption goes first, always
- "works" and "makes money" are unrelated properties and it is very easy to spend seventeen days getting good at the first one
- zero fees is the cleanest test there is. if it loses at zero fees, stop optimising execution
- a signal that predicts magnitude and not direction feels like a signal, backtests like a signal, and pays nothing. check the sign separately
- if the trade is negative in every window across two years, you were not late. there was nothing there

## run it

```
cd src-tauri && cargo test
```

rust stable and the tauri 2.0 bits for your platform. `cargo tauri dev` opens the window.

there is no exit rule, no paper mode and no buy builder anywhere in here. the three things a trading system actually needs are the three i never got round to, so it cannot trade and never has.

## the writeup

`docs/VERDICT-2026-08-27.md` is the numbers and how i got them.
`docs/POSTMORTEM-2026-08-27.md` is why i did not find out sooner.

the trading answer is only about pump.fun. the other one is not, and that is the reason this is public.

## status

- ✅ 1,654 tests passing
- ✅ 12,089 launches recorded
- ❌ -7.76% a trade
- ❌ -0.86% at zero fees, so it was never the fees
- 🪦 seventeen days
- 📄 the postmortem is the useful part

nothing here is advice. the measured result is a loss.
