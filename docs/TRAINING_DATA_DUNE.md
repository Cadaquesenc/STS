# Training the on-chain half from Dune

> **Read this first, added 2026-08-27.** This document was written at 05:33, one hour
> before the verdict closed, and it does not know about it. **It proposes acquiring the
> launch history of October 2024 to August 2026 in order to train a model to pick
> pump.fun launches — and that trade, over exactly that period, had already been
> measured as unprofitable.** Seven hour-matched windows across the same span return
> −6.5% to −17.1% a trade, negative in every one, uncorrelated with volume, competition
> or time; an independent re-run gets −10.32% net, CI [−13.00, −7.63], n=1,034, in 0.35
> seconds of compute, against a +2.12% break-even. The sweep's own closing sentence is
> addressed to documents like this one: *"Anyone proposing to backtest STS's ranking on
> 2025 data should know they would be searching for alpha inside a population that
> returned −6.5%."* A hundred windows buy precision on that number, not a different
> sign.
>
> **The research itself is sound and worth keeping** — the free-data findings, the
> bandwidth arithmetic and the slot mathematics were checked and stand. What does not
> stand is the reason for spending any of it. See `docs/VERDICT-2026-08-27.md` and the
> *Do not quote these numbers* table in `docs/sprint-2026-08-27/INDEX.md`.

Assessed 27 Aug 2026. Question: can Dune supply two years of Solana pump.fun history
in the shape of `data/coins-2026-08-20.jsonl`, so the on-chain half of the model can
be trained on years instead of days, and what does that cost.

Short answer: the data is there and it is better than expected, but the account
cannot run a single query today, and the one feature the whole opening signal rests
on (how many wallets bought in the first 3 seconds) cannot be reproduced from Dune
as it is currently defined. That feature has to be redefined in slots on both sides
before any history is worth pulling.

## 0. What could and could not be verified

Everything below about table names and column names was read from live
`searchTables` schema results and is real. Nothing below about row counts, earliest
date, or actual data content was verified, because:

```
createAndExecuteQuery -> "Your account is read-only. Upgrade to create content or run queries."
getUsage              -> plan visitor_fluid_engine, creditsUsed 0, creditsQuota 0
```

The account is view-only. It cannot execute a query at any engine tier, including
the free tier. So the history-depth check, the row counts, and the two schema
ambiguities flagged below are all open items, not findings.

## 1. The tables that matter, and one big surprise

The brief assumed `pump_call_create` / `pump_call_buy` / `pump_call_sell`. Those
exist, but they are the wrong tables to build this from.

Dune also decodes pump.fun's **Anchor events**, and those are a near-exact match for
what the live listener already decodes in `~/Code/flux/src/pump.js`:

| Table | What it is |
| --- | --- |
| `pumpdotfun_solana.pump_evt_createevent` | The `CreateEvent` the listener parses |
| `pumpdotfun_solana.pump_evt_tradeevent` | The `TradeEvent` the listener parses |

Why this matters more than the naming:

1. **The call tables do not carry the money.** `pump_call_buy` carries `amount`
   (tokens requested) and `max_sol_cost` (the slippage ceiling). Neither is what the
   buyer actually paid. `pump_evt_tradeevent` carries `sol_amount`, `token_amount`,
   `is_buy`, and both virtual reserves, which is exactly the set the listener uses.
2. **The call tables miss router traffic.** A pump buy placed through Photon, BullX,
   Trojan, Jupiter, or any sniper bot arrives as an inner instruction, and Dune's own
   docs say of decoded IDL tables: "These only decode from instructions, and not
   inner instructions." The Anchor event is emitted by the program itself on every
   path, so the event table should not have that hole. Given that most pump.fun
   volume is routed, this is the difference between a representative dataset and a
   badly skewed one.

The `_v2` variants (`pump_call_create_v2`, `pump_call_buy_v2`, `pump_call_sell_v2`,
`pump_call_migrate_v2`) exist because the program was upgraded. The event tables
absorb the versions into one table with nullable columns, which is another reason to
prefer them.

### Field by field against the JSONL

Numeric event fields come back as `varbinary` holding little-endian u64. In DuneSQL
that is `bytearray_to_bigint(bytearray_reverse(col))`.

| JSONL field | Dune source | Clean? |
| --- | --- | --- |
| `mint` | `pump_evt_createevent.mint` | yes |
| `symbol`, `name`, `uri` | `pump_evt_createevent.symbol` / `.name` / `.uri` | yes |
| `creator` | `pump_evt_createevent.creator` (fall back to `.user`) | yes |
| `supply` | `token_total_supply`, divided by 1e6 | yes |
| `curve.virtualSol` | `virtual_sol_reserves` / 1e9 | yes |
| `curve.virtualTokens` | `virtual_token_reserves` / 1e6 | yes |
| `curve.realTokens` | `real_token_reserves` / 1e6 | yes |
| `initialBuySol` | first `pump_evt_tradeevent` on the mint where `user = creator` | yes |
| `initialBuyTokens` | same row, `token_amount` / 1e6 | yes |
| `who[].w` | `pump_evt_tradeevent.user` | yes |
| `who[].in` / `.out` | sum of `sol_amount` / 1e9 split on `is_buy` | yes |
| `who[].tin` / `.tout` | sum of `token_amount` / 1e6 split on `is_buy` | yes |
| `who[].n` | count of trade events for that wallet | yes |
| `total.*`, `open.solIn/solOut/trades/sellers` | aggregates of the above | yes |
| `market.candles[].o/h/l/c` | `virtual_sol_reserves / virtual_token_reserves / 1e3`, which is the listener's own price formula | yes, **but see the bucket problem** |
| `market.candles[].volume/buys/sells` | aggregates | yes, same bucket problem |
| `outcome.entry/peak/last/peakMult/endMult` | derived from the candle series | yes, same bucket problem |
| `t` (launch time, ms) | `block_time` (whole seconds) or the event's own `timestamp` (unix seconds) | **no, second resolution only** |
| `who[].at` (first-trade offset, seconds, 2 dp) | not available | **no** |
| `open.seconds` = 3, `open.wallets` | not available as defined | **no** |
| `market.candleSeconds` = 1 | not available as defined | **no** |
| `outcome.peakAtSec`, `outcome.highs/lows` | not available as defined | **no** |
| `funding.*` | reconstructible, expensively, see section 3 | partly |
| `social.*` | out of scope by design, stays on live captures | n/a |

Two schema ambiguities to settle with a one-row probe before writing the extract:

- A working public query (Dune query 7615392) references `call_block_time` and
  `call_block_date` on `pump_call_create`, while `searchTables` reports plain
  `block_time` and `block_date`. Both namings appear in Dune's docs. Confirm which
  the event tables use.
- `pump_evt_tradeevent` carries both camelCase (`solAmount`, `virtualSolReserves`)
  and snake_case (`sol_amount`, `virtual_sol_reserves`) columns, presumably one set
  per program version. Confirm which is populated on recent dates and whether older
  rows use the other.

## 2. Time resolution: this is the thing that breaks

This is the most important finding in the document.

**What STS measures now.** In `docs/archive/legacy-node/src/watch.js`, the launch
timestamp is `const t = Date.now()` at the moment the websocket delivered the create
event, and every offset is `age = (Date.now() - coin.t) / 1000`. The wallet offset
`at` is that age rounded to 2 decimal places. The candle bucket is
`Math.floor(age)`. So the entire time axis of the record is **the listener's own
wall clock, relative to the listener's own receipt of the create**. It has
sub-second precision, and it embeds RPC propagation delay, websocket delivery jitter,
and whatever the listener's event loop was doing at the time.

**What Dune has.** `block_time` is a `TIMESTAMP` at slot level. Solana reports block
time as a unix timestamp in **whole seconds**, and every transaction and every
instruction inside one block shares the single block time of that block. Slots are
roughly 400 ms, so several consecutive slots commonly carry the same block time
second. There is no sub-second timestamp anywhere in these tables. The event's own
`timestamp` column is also unix seconds, written by the program from the same clock.

What Dune does have is a **total order**, and it is exact:
`(block_slot, tx_index, outer_instruction_index, inner_instruction_index)`.
`block_slot` advances about 2.5 times per second.

**Verdict on "28 wallets in the first 3 seconds".** Not reproducible. Two ways to
approximate it, both wrong in different ways:

- *By block_time seconds.* `trade.block_time <= create.block_time + interval '3'
  second`. Both endpoints are quantized to whole seconds, so the true window is
  anywhere from 2 to 4 seconds wide. That is up to 33% error on the width of a
  3-second window, and the error is not random: it correlates with where in the
  second the launch landed, which correlates with block position, which correlates
  with whether snipers were contending for the block. It would inject a bias
  straight into the feature the model leans on hardest.
- *By slots.* `trade.block_slot - create.block_slot < 8`, which is about 3.2 seconds
  at nominal slot time. Deterministic, chain-native, identical for every launch, no
  observer jitter at all. But it is not the same measurement as the live one, so a
  model trained on it cannot be scored against live records that use wall-clock
  `at`.

**The fix, and it is cheap.** Redefine the opening window in **slots on both sides**,
then the two datasets agree by construction. This is already possible live with no
new capture work: `~/Code/flux/src/listen.js` line 129 already reads
`result.context.slot` off every notification and writes it into the events log. The
listener has the slot; the coins record just does not carry it yet.

So the sequence is: put `slot` on the create record and on each trade, redefine
`open` as "distinct buyers within N slots of the create slot" and the candles as
slot-offset buckets, recompute the existing seven days of captures under the new
definition, and only then pull history. Doing it in the other order means pulling
tens of millions of rows against a feature definition that is about to be thrown
away.

One caveat on the intra-slot order. `listen.js` (lines 117 to 124) assigns `si` as
the order the listener *observed* transactions inside a slot, and its own comment
says this is "not the true block index". Dune's `tx_index` **is** the true block
index. Slot numbers are directly comparable between the two sources; positions
within a slot are not. Use slot only.

The 1-second candles have the same problem and the same fix. Slot buckets are
actually finer than the current 1-second candles (400 ms), so they downsample to
whatever bucket width is wanted, deterministically, from the create slot.

## 3. The funding graph

Reconstructible. There are two routes, with different failure modes.

**Route A, the direct edge list.**
`system_program_solana.system_program_call_transfer` carries exactly the shape of
`FundingEdge` in `src-tauri/src/strategy/syndicate.rs`:

| Column | Type |
| --- | --- |
| `account_from` | varchar |
| `account_to` | varchar |
| `lamports` | varbinary (little-endian u64) |
| `block_time`, `block_slot`, `block_date` | timestamp / bigint / date |
| `tx_id`, `tx_signer` | varchar |
| `is_inner`, `inner_instruction_index`, `outer_instruction_index` | bool / int / int |

The risk is the inner-instruction rule again. A plain wallet-to-wallet funding is a
top-level System Program transfer and will be present. A funding that arrives via a
program (a CEX withdrawal contract, a swap payout, a bridge release) is a CPI and may
be missing. Since laundering through a program is precisely the behaviour the
syndicate detector is looking for, that gap matters. Verify it before trusting this
table alone.

**Route B, the balance-delta route.** `solana.account_activity` has one row per
account per transaction, with `pre_balance`, `post_balance`, `balance_change` in
lamports, plus `tx_id`, `block_slot`, `tx_index`, and `token_mint_address` which is
null for native SOL. It is derived from balances, not from instructions, so it has no
CPI blind spot at all. To get an edge: find rows where the deployer's
`balance_change > 0` and `token_mint_address IS NULL`, then look up the negative-delta
signer in the same `tx_id`.

Route B is also cheaper than its 227 TB size suggests, because it is **partitioned by
`address`**. Looking up a known list of deployers prunes hard. Route A sits inside a
267 TB shared table with no address partition.

Recommendation: Route B as the source of truth, Route A as a cross-check on a sample.

**What is genuinely expensive.** `FUNDING_DEPTH = 2` means every deployer and every
early buyer needs a two-hop walk, and `HUB_DEGREE = 25` means each candidate funder
needs its distinct-payee count over some lookback so exchange hot wallets can be
excluded. That degree count is a global aggregate over a very large table, and it has
to be done once per funder. On the current capture only 41% of launches resolved a
funder at all (2,209 of 5,392 on 20 Aug), so this is not a small side quest: it is
the single most expensive part of the whole job, comfortably more than the launches
and trades combined.

Pull the launches and trades first. Treat funding as a separate, later, budgeted job
scoped to the deployers that actually matter.

## 4. History depth

**Not verified.** The min/max aggregate could not be run.

What is known: pump.fun went live on Solana mainnet in January 2024, so two years
back from today (Aug 2024) is comfortably inside the program's life. Two years of
*chain* history certainly exists.

What is not known, and needs one cheap query each:

1. `SELECT min(block_date), max(block_date) FROM pumpdotfun_solana.pump_evt_createevent`
2. Same for `pump_evt_tradeevent`.
3. A count by month, to see whether the event tables were backfilled to the start or
   only decoded from whenever the IDL was submitted to Dune. This is the real risk:
   Dune decodes from the point an IDL is registered, and backfill is not guaranteed.
   If `pump_evt_*` only starts in, say, mid-2025, the fallback is `pump_call_*` for
   the earlier period, which reintroduces the router blind spot and loses the actual
   SOL amounts. That would be a materially worse dataset and worth knowing before
   paying for anything.

## 5. Volume and cost, and what this account can actually do

**Table sizes** (from `getTableSize`, real numbers):

| Logical table | Underlying physical table | Size |
| --- | --- | --- |
| `pumpdotfun_solana.pump_call_create` | `solana.instruction_calls_decoded_0021` | 267,876 GB |
| `pumpdotfun_solana.pump_call_buy` | same physical table | 267,877 GB |
| `system_program_solana.system_program_call_transfer` | same physical table | 267,877 GB |
| `solana.account_activity` | `solana.account_activity_0015` | 227,014 GB |

Every decoded Solana table on Dune, for every protocol, is a view over one shared
268 TB table partitioned by `instruction_identifier`. `getTableSize` reports the
whole physical table, not the slice. So these numbers say nothing about how much
pump.fun data there is; they say that an unpartitioned scan is catastrophic and every
query must filter on `block_date`.

**Row estimates.** From `data/coins-2026-08-20.jsonl`, 5,392 captured launches:

| Per launch | Mean |
| --- | --- |
| trade events in the 60 s window | 34.5 |
| distinct wallets | 13.4 |
| candles | 8.4 |
| funding edges | 1.2 (and only 41% of launches resolved any) |

For the denominator, the comment in `docs/archive/legacy-node/src/dune.js` records a
measured figure: on 11 Aug 2026 pump.fun had 40,372 launches and the local log held
2,965. **That is a duty cycle, not a capture rate** — the recorder caught 137 of 137 launches
verified block-for-block against the chain while connected, and on 11 August it was connected
for 161 of the day's 1,440 minutes. It is essentially complete while running, and it is almost
never running. Taking a long-run average somewhere between 20k and 40k
launches per day over 730 days gives roughly **15M to 30M launches**, and at 34.5
trade events each, roughly **500M to 1B trade rows** in the 60-second windows. These
are estimates from one day's shape, not counts.

**What the account can do today: nothing.** Plan `visitor_fluid_engine`, quota 0,
read-only. This matches Dune's documented trial behaviour exactly: a new account gets
14 days on Free-tier economics, "the trial ends after 14 days or once you use up the
credits, whichever comes first, and the account then moves to view-only access until
you upgrade to a paid plan." Schema search and doc search still work, which is why
this document exists. Query execution does not.

**Plans and export economics** (from Dune docs):

| Plan | Price | Credits/mo | Extra per 100 credits | Export | CSV export |
| --- | --- | --- | --- | --- | --- |
| Free | $0 | 2,500 | $5.00 | 20 credits/MB | no |
| Analyst | $75/mo | 4,000 | $1.875 | 10 credits/MB | no |
| Plus | $399/mo ($349 annual) | 25,000 | $1.596 | 2 credits/MB | yes |
| Enterprise | custom | custom | custom | custom | yes |

Credits pay for two separate things: the compute the query burns, and the bytes
exported. Included credits do not roll over. There is a 32 GB cap on a single query
result, and results beyond it are silently truncated unless `allow_partial_results`
is passed.

**Two very different cost outcomes, and the choice matters enormously.**

*Naive: export the raw trade rows and aggregate locally.* 500M to 1B rows at roughly
175 bytes each is 90 to 175 GB. On Plus at 2 credits/MB, 100 GB is about 205,000
credits. Netting the 25,000 included, that is roughly **$2,900 to $5,700 in export
credits alone**, plus $399/month, plus compute credits for scans of a 268 TB table,
plus the 32 GB result cap means chunking into dozens of executions. Do not do this.

*Sensible: aggregate inside DuneSQL and export one row per launch.* The
`who[]`/`open`/`total`/`candles` rollups are all group-bys that Trino does happily.
One row per launch with nested arrays is roughly 300 bytes to 3 KB depending on how
much of `who[]` is kept. At 15M to 30M launches that is **5 to 20 GB**, which on Plus
is about 10,000 to 40,000 export credits, i.e. **roughly $150 to $600 on top of the
$399/month subscription**, spread over two or three months of billing to stay near
the included quota. Compute is extra and needs measuring on a single day first.

So: **Plus, at $399/month, is the minimum viable plan**, both because Analyst's
10 credits/MB is five times the export cost and because CSV export is Plus-only.
Budget two to three months. Expect $800 to $1,800 all in for the launches-and-trades
half, and treat the funding graph as a separate budget line that has not been sized.

If the raw trade-level detail turns out to be genuinely necessary, the honest answer
is not more credits but Enterprise Datashare: Dune's Feb 2026 changelog notes all raw
Solana data is now synced to BigQuery US, "over 600TB across 9 tables including
`solana.account_activity`, `solana.instruction_calls`, `solana.transactions`", at no
extra cost for existing US multi-region customers. That is a different conversation
and a different price bracket.

## 6. The overlap test: reconstruct 20 Aug 2026 and diff it

Run this against `data/coins-2026-08-20.jsonl` (5,392 launches, all of which should
appear inside Dune's full ~40k for that day). The point is not to check that Dune has
the data. It is to measure, per field, how far the Dune reconstruction sits from what
the listener actually recorded, and specifically to put a number on the time-axis
disagreement before committing money.

Note the two window definitions carried side by side: `open_wallets_slot` is the
proposed slot definition, `open_wallets_bt` is the naive block_time one. Diffing both
against the JSONL's `open.wallets` measures exactly how much damage the quantisation
does.

```sql
-- STS overlap test: reconstruct 20 Aug 2026 pump.fun launches in the coins JSONL shape.
-- Cost control: both tables filtered on block_date. The trade side spans two days so
-- that launches near midnight keep a full 60 s window.
-- Verify column naming (block_time vs call_block_time, snake vs camel) with a
-- LIMIT 1 probe before running this.

WITH launch AS (
    SELECT
        mint,
        block_slot                                                          AS launch_slot,
        block_time                                                          AS launch_time,
        tx_index                                                            AS launch_tx_index,
        COALESCE(creator, "user")                                           AS creator,
        symbol,
        name,
        uri,
        bytearray_to_bigint(bytearray_reverse(token_total_supply))    / 1e6 AS supply,
        bytearray_to_bigint(bytearray_reverse(virtual_sol_reserves))  / 1e9 AS virtual_sol,
        bytearray_to_bigint(bytearray_reverse(virtual_token_reserves))/ 1e6 AS virtual_tokens,
        bytearray_to_bigint(bytearray_reverse(real_token_reserves))   / 1e6 AS real_tokens,
        tx_id                                                               AS launch_tx
    FROM pumpdotfun_solana.pump_evt_createevent
    WHERE block_date = DATE '2026-08-20'
),

trade AS (
    SELECT
        mint,
        "user"                                                              AS wallet,
        is_buy,
        block_slot,
        block_time,
        tx_index,
        outer_instruction_index,
        inner_instruction_index,
        tx_id,
        bytearray_to_bigint(bytearray_reverse(sol_amount))            / 1e9 AS sol,
        bytearray_to_bigint(bytearray_reverse(token_amount))          / 1e6 AS tokens,
        -- The listener's own price formula: vsol/vtok/1e3, from the 9 and 6 decimals.
        CAST(bytearray_to_bigint(bytearray_reverse(virtual_sol_reserves))   AS DOUBLE)
          / CAST(bytearray_to_bigint(bytearray_reverse(virtual_token_reserves)) AS DOUBLE)
          / 1e3                                                             AS price
    FROM pumpdotfun_solana.pump_evt_tradeevent
    WHERE block_date BETWEEN DATE '2026-08-20' AND DATE '2026-08-21'
      AND bytearray_to_bigint(bytearray_reverse(virtual_token_reserves)) > 0
),

-- 150 slots is about 60 s at nominal 400 ms slot time, matching the follow window.
win AS (
    SELECT
        l.mint,
        l.creator,
        l.launch_slot,
        l.launch_time,
        t.wallet,
        t.is_buy,
        t.sol,
        t.tokens,
        t.price,
        t.block_slot,
        t.tx_index,
        t.outer_instruction_index,
        t.inner_instruction_index,
        t.block_slot - l.launch_slot                                        AS slot_offset,
        date_diff('second', l.launch_time, t.block_time)                    AS bt_offset_sec
    FROM launch l
    JOIN trade t
      ON t.mint = l.mint
     AND t.block_slot >= l.launch_slot
     AND t.block_slot <  l.launch_slot + 150
),

-- Per wallet, over the whole 60 s window. Mirrors who[].
per_wallet AS (
    SELECT
        mint,
        wallet,
        SUM(IF(is_buy, sol,    0))                                          AS sol_in,
        SUM(IF(is_buy, 0,      sol))                                        AS sol_out,
        SUM(IF(is_buy, tokens, 0))                                          AS tok_in,
        SUM(IF(is_buy, 0,      tokens))                                     AS tok_out,
        COUNT(*)                                                            AS n,
        -- Replaces who[].at. Slots, not wall-clock seconds; see section 2.
        MIN(slot_offset)                                                    AS first_slot_offset,
        MIN(bt_offset_sec)                                                  AS first_bt_offset_sec
    FROM win
    GROUP BY mint, wallet
),

-- One-second candles, bucketed off the create slot at nominal 400 ms per slot.
candles AS (
    SELECT
        mint,
        CAST(FLOOR(slot_offset * 0.404) AS INTEGER)                           AS s,
        MIN_BY(price, (block_slot, tx_index, outer_instruction_index))      AS o,
        MAX(price)                                                          AS h,
        MIN(price)                                                          AS l,
        MAX_BY(price, (block_slot, tx_index, outer_instruction_index))      AS c,
        SUM(sol)                                                            AS volume,
        SUM(IF(is_buy, 1, 0))                                               AS buys,
        SUM(IF(is_buy, 0, 1))                                               AS sells
    FROM win
    GROUP BY mint, CAST(FLOOR(slot_offset * 0.404) AS INTEGER)
),

opening AS (
    SELECT
        mint,
        -- Proposed definition: 8 slots is about 3.2 s. This is the one to keep.
        COUNT(DISTINCT IF(slot_offset < 8  AND is_buy, wallet))             AS open_wallets_slot,
        -- Naive definition, kept only to measure the quantisation damage.
        COUNT(DISTINCT IF(bt_offset_sec <= 3 AND is_buy, wallet))           AS open_wallets_bt,
        COUNT(DISTINCT IF(slot_offset < 8 AND NOT is_buy, wallet))          AS open_sellers_slot,
        SUM(IF(slot_offset < 8 AND is_buy,     sol, 0))                     AS open_sol_in,
        SUM(IF(slot_offset < 8 AND NOT is_buy, sol, 0))                     AS open_sol_out,
        SUM(IF(slot_offset < 8, 1, 0))                                      AS open_trades
    FROM win
    GROUP BY mint
),

totals AS (
    SELECT
        mint,
        COUNT(DISTINCT IF(is_buy,     wallet))                              AS total_wallets,
        COUNT(DISTINCT IF(NOT is_buy, wallet))                              AS total_sellers,
        SUM(IF(is_buy,     sol, 0))                                         AS total_sol_in,
        SUM(IF(is_buy, 0,  sol))                                            AS total_sol_out,
        COUNT(*)                                                            AS total_trades
    FROM win
    GROUP BY mint
),

-- initialBuySol / initialBuyTokens: the deployer's own first buy.
deployer_open AS (
    SELECT
        w.mint,
        MIN_BY(w.sol,    (w.block_slot, w.tx_index, w.outer_instruction_index)) AS initial_buy_sol,
        MIN_BY(w.tokens, (w.block_slot, w.tx_index, w.outer_instruction_index)) AS initial_buy_tokens
    FROM win w
    WHERE w.is_buy AND w.wallet = w.creator
    GROUP BY w.mint
)

SELECT
    l.mint,
    l.symbol,
    l.name,
    l.creator,
    l.uri,
    l.supply,
    l.launch_slot,
    l.launch_time,
    l.virtual_sol,
    l.virtual_tokens,
    l.real_tokens,
    d.initial_buy_sol,
    d.initial_buy_tokens,
    o.open_wallets_slot,
    o.open_wallets_bt,
    o.open_sellers_slot,
    o.open_sol_in,
    o.open_sol_out,
    o.open_trades,
    t.total_wallets,
    t.total_sellers,
    t.total_sol_in,
    t.total_sol_out,
    t.total_trades,
    ARRAY_AGG(
        CAST(ROW(pw.wallet, pw.sol_in, pw.sol_out, pw.tok_in, pw.tok_out,
                 pw.n, pw.first_slot_offset, pw.first_bt_offset_sec)
        AS ROW(w VARCHAR, sol_in DOUBLE, sol_out DOUBLE, tok_in DOUBLE,
               tok_out DOUBLE, n BIGINT, first_slot INTEGER, first_bt_sec BIGINT))
        ORDER BY pw.first_slot_offset
    )                                                                       AS who,
    (SELECT ARRAY_AGG(
        CAST(ROW(cd.s, cd.o, cd.h, cd.l, cd.c, cd.volume, cd.buys, cd.sells)
        AS ROW(s INTEGER, o DOUBLE, h DOUBLE, l DOUBLE, c DOUBLE,
               volume DOUBLE, buys BIGINT, sells BIGINT))
        ORDER BY cd.s)
     FROM candles cd WHERE cd.mint = l.mint)                                AS market_candles
FROM launch l
LEFT JOIN opening       o  ON o.mint  = l.mint
LEFT JOIN totals        t  ON t.mint  = l.mint
LEFT JOIN deployer_open d  ON d.mint  = l.mint
LEFT JOIN per_wallet    pw ON pw.mint = l.mint
GROUP BY
    l.mint, l.symbol, l.name, l.creator, l.uri, l.supply, l.launch_slot,
    l.launch_time, l.virtual_sol, l.virtual_tokens, l.real_tokens,
    d.initial_buy_sol, d.initial_buy_tokens,
    o.open_wallets_slot, o.open_wallets_bt, o.open_sellers_slot,
    o.open_sol_in, o.open_sol_out, o.open_trades,
    t.total_wallets, t.total_sellers, t.total_sol_in, t.total_sol_out, t.total_trades
```

**Before running the full day, run this instead.** It is one slot of data and settles
both schema ambiguities and the router-coverage question for a few credits:

```sql
SELECT *
FROM pumpdotfun_solana.pump_evt_createevent
WHERE block_date = DATE '2026-08-20'
LIMIT 5
```

**What to diff, and what a pass looks like.** Join Dune output to the JSONL on
`mint`, restricted to the 5,392 mints the listener actually saw.

| Field | Expectation |
| --- | --- |
| `creator`, `symbol`, `supply`, `curve.*` | exact match, every row. Any mismatch means the wrong column set was read. |
| `initialBuySol` | exact to rounding. |
| `total.*` over the 60 s window | Dune should be **greater than or equal to** the JSONL. Dune sees every trade; the listener drops what the public RPC did not deliver. A Dune total that is *lower* means the window arithmetic is wrong. |
| `who[]` membership | Dune superset of JSONL. Measure the extra fraction: that is the listener's real miss rate on trades, which is worth knowing on its own. |
| `open_wallets_slot` vs JSONL `open.wallets` | the number to watch. Expect close but not equal. |
| `open_wallets_bt` vs JSONL `open.wallets` | expect materially worse than the slot version. If it is not, the whole slot argument is weaker than it looks and section 2 should be revisited. |
| candle `o/h/l/c` at matching `s` | price should agree to floating-point noise, since both use the same formula on the same reserves. Bucket *membership* will disagree at the edges. |

Budget: one day, both tables filtered to two `block_date` partitions. Run it once on
Free-tier economics after upgrading, look at the reported
`executionCostCredits`, and multiply by 730 before committing to anything.

## 7. Verdict

- **Can Dune supply it?** Yes, and via `pump_evt_createevent` / `pump_evt_tradeevent`
  it supplies a cleaner and more complete feed than the call tables the brief
  assumed, including router-routed buys and the real SOL amounts.
- **What breaks?** The time axis. Everything measured in wall-clock seconds from the
  listener's receipt of the create (`open.seconds`, `who[].at`, 1-second candles,
  `peakAtSec`) has no Dune equivalent and must be redefined in slots on both sides,
  or dropped.
- **What it costs?** Nothing is possible today; the account is view-only. Plus at
  $399/month is the floor. Aggregating in SQL and exporting one row per launch puts
  the whole launches-and-trades extract at roughly $800 to $1,800 over two or three
  months. The funding graph is a separate, larger, unsized job.
- **Unverified and material:** whether `pump_evt_*` is backfilled to pump.fun's 2024
  start or only decoded from a later IDL registration. If it is not backfilled, the
  two-year dataset is not what it appears to be.

## 8. Next step

One thing, in this order:

1. Redefine the opening window and the candle clock in **slots**, and recompute the
   seven existing days of captures under the new definition. This costs nothing, uses
   data already on disk (`listen.js` already records `result.context.slot`), and it
   is the precondition for history being worth anything. Doing it after paying Dune
   means paying twice.

Then, and only then: upgrade to Plus, run the LIMIT 5 probe, run the min/max and
count-by-month checks on `pump_evt_createevent`, run the 20 Aug overlap query, read
the reported credit cost, and decide.
