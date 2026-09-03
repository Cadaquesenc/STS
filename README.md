# sts

a solana trading system for pump.fun launches. 106,000 lines of rust, 1,654 tests passing, 12,089 launches recorded, seventeen days.

on day seventeen i measured whether the trade made money.

buying a launch and selling inside the minute loses 7.76% a trade. at zero fees it still loses 0.86%. so it was never the fees. i swept it back to october 2024 and it was never positive in a single window. there was never an edge to lose.

my roadmap had 40 gates. the first one that needed me to be right about the market was number 23.

it never traded. no exit rule, no buy builder, so it cannot.

dead. `docs/VERDICT-2026-08-27.md` is the numbers. `docs/POSTMORTEM-2026-08-27.md` is why it took seventeen days.
