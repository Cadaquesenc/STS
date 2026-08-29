# Handoff: put the opening window on the chain's clock

Written 2026-08-27. Pick this up in the worktree, not the main checkout.

> **Preserved 2026-08-27, unedited except for this note. It was written a few hours
> before the verdict closed and it was in no commit until now.**
>
> Two things to know before acting on it. **Its stated motivation is gone**: it wants
> slot alignment so a model can train across historical and live captures, and the
> archival sweep has since measured seven hour-matched windows across the whole
> two-year period that history would cover and found expectancy negative in every one.
> There is no model to train. **But its last section is still the right test** — pull
> an hour of blocks and diff the reconstructed openings against the coins captured
> live the same day. That answers whether historical and live data are comparable at
> all, which is worth knowing regardless of what anyone trades.
>
> One number in it is retired: the on-chain failure rate is **11.3%**, not the ~76%
> it quotes. That figure came from a listener counter which overstates by 206 to 274
> times. See the *Do not quote these numbers* table in
> `docs/sprint-2026-08-27/INDEX.md`.

## Why this exists

STS measures a coin's opening as "distinct wallets in the first 3 seconds". Those 3 seconds
are `Date.now()` on Ethan's Mac, taken when the websocket delivered the create. Three separate
research passes all landed on the same conclusion: no historical source can reproduce a
wall-clock window, because block time is whole seconds only. A slot is 0.404 seconds and a
transaction index gives exact order inside a block, so **3 seconds is 7.4 slots**. Until the
window is defined in slots on both sides, historical data cannot be compared to live captures,
and a model trained across both learns the difference between the two capture methods rather
than the market.

## What is already done, and a correction

I first reported that nothing in the capture tool touches `slot`. **That was wrong.** My grep
loop was broken and I read its empty output as fact. `main` already does most of this work:

- `watch.js` line 187 reads `result.context?.slot` off every `logsNotification`
- lines 189 to 193 compute `si`, the observed index within that slot, bounded to ~256 slots
- line 214 threads `{sig, slot, si}` into `onLaunch` and `onTrade` as `ctx`
- lines 308 to 309 put `slot` and `si` on the coin
- lines 532 to 534 put `slot`, `si` and **`slotsAfter`** on every wallet in `who`
- lines 815 to 819 write `slot` and `si` into the record

`slotsAfter` is the landing distance in slots from the launch. It is exactly the ordering key
the research recommended, and it is already being recorded.

## What is still missing

**The window is still cut by wall clock.** `DEFAULTS.seconds = 3`, and `coin.open` is
snapshotted by a timer at the 3 second mark. `openers` at line 671 filters `w.at <= cfg.seconds`.
So the slot data is recorded but nothing selects by it.

## The change to make

Additive, non breaking. Do NOT change `coin.open`, other code and every existing record depend
on its shape.

1. Add to `DEFAULTS`: `slots: 8` with a comment saying 3 seconds is 7.4 slots at 0.404s, rounded
   up so the slot window is never narrower than the wall clock one it sits beside.
2. At record write time, add an `openSlots` block computed from `who[].slotsAfter`:
   `{ slots: cfg.slots, wallets: <count of who entries with slotsAfter != null && slotsAfter <= cfg.slots> }`
3. **Emit only what is actually derivable.** Per wallet the record keeps `in`, `out`, `n` over
   the whole follow window and `in0`, `out0` frozen at the seconds mark. There is no per trade
   slot, so SOL amounts cannot honestly be recut by slot. Wallet count can, because `slotsAfter`
   is the wallet's first trade. Emitting a slot based `solIn` would be a fabricated number.
   Leave it out rather than approximate it.
4. Where `slot` is null (endpoint did not supply context) the count must be null, not zero.
   A missing measurement and a measurement of zero are different facts.

Both figures then sit side by side in every record, which is precisely what the Dune overlap
query in `TRAINING_DATA_DUNE.md` was written to diff against.

## Working rules for this repo

- The main checkout is shared with other windows. **Never `git checkout`.** Use the worktree.
- Worktree already made: `.claude/worktrees/capture-slot-clock`, branch `feat/capture-slot-clock`,
  off `main` at `bbe3c93`.
- Baseline is **285 tests passing**: `cd tools/capture && node --test "test/*.test.js"`. Run it
  before and after. It takes about 47 seconds.
- Do not build Rust in a worktree. Each built worktree keeps a full `target/` and they have
  reached 44 GB before.
- Plain words in comments and commit messages. No em dashes anywhere.

## The seven captured days

`data/coins-*.jsonl` for 10, 11, 12, 15, 16, 20 and 21 August predate slot capture and carry no
slot. They are not lost: every record has the mint, and `getSignaturesForAddress` on a mint
returns its create transaction and therefore its true slot. That backfill is free.

## Free data, settled

`https://api.mainnet-beta.solana.com` is a free full archive node backed by Old Faithful. No
key, no account. Verified live: slot 250,000,000 returned a block from 2024-02-23.

Two measured facts that matter for any backfill:
- **gzip is worth 3.3x.** One real block: 7,624,213 bytes and 15.8s uncompressed, 2,331,263
  bytes and 4.7s with `--compressed`. Always send `Accept-Encoding: gzip`.
- Keep concurrency at 4. Above that the endpoint starts cutting responses mid stream.

Full analysis in `docs/TRAINING_DATA_FREE.md`, `docs/TRAINING_DATA_SOURCES.md` and
`docs/TRAINING_DATA_DUNE.md`. Do not redo that research.

## First task after the change lands

Pull one hour of blocks from 2026-08-20 starting at slot 440,486,139 off the public RPC, filter
to the pump.fun programs, drop the roughly 76 percent that failed on chain, decode the
`Program data:` lines, and diff the reconstructed openings against the 5,392 coins captured live
that same day. That diff is the thing that decides whether history is worth buying.
