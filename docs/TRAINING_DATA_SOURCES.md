# Training data sources for the on chain half of STS

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

Researched 2026-08-27. Every limit and price below was read off a page that was actually
fetched, and the URL is given. Where something could not be checked, it says "could not
verify" rather than guessing.

## The short version

The best genuinely free source of two years of pump.fun history is **Old Faithful**, the
Solana Foundation funded ledger archive, read with **Jetstreamer**. The data is free and
complete. The thing that costs money is not the data, it is the bandwidth to pull it, and
the fix is to rent a European server for one month for about 40 euros instead of trying to
do it on the Mac.

Separately, and usable today for nothing: several real pump.fun datasets are already published
on Kaggle and HuggingFace, covering about five or six non contiguous months. They do not add
up to the corpus, but they are enough to build and prove the whole feature pipeline before
spending a penny or a day of bandwidth. Start there.

Four findings up front that change the shape of the job.

**1. The volume estimate in the brief is roughly ten to thirty times too low.** Measured
live against pump.fun's own API: 1000 launches came out in 18.4 minutes, which is 54
launches a minute, or about 78,000 a day. The vendor NoLimitNodes states its archive holds
"14M+" mints and "~1.8B" trades from genesis
(https://nolimitnodes.com/products/historic-pump-fun). So the target is more like 14 to 30
million launches, not one million. Plan storage and training set sampling around that.

**2. Flipside Crypto no longer exists as a free option.** Its data business was sold to
SonarX and announced 2026-05-19
(https://www.sonarx.com/blog/sonarx-acquires-flipside-crypto-blockchain-data-business).
Every Flipside domain now redirects to an unrelated product site. Verified by request:
`flipsidecrypto.xyz`, `flipsidecrypto.com` and `docs.flipsidecrypto.xyz` all return HTTP 200
at `https://www.edisyl.com/`, and the `FlipsideCrypto/solana-models` repo returns 404. If
free unlimited Flipside SQL is in the mental model, remove it.

**3. The three second window is not reproducible from history as currently defined, and this
matters more than any data source choice.** See the timestamp section below. It has a clean
fix, but the fix has to happen before any training run, not after.

**4. Check the licence on every published dataset before training on it.** The single largest
published pump.fun corpus, MELT at 218.5 million transactions, is CC BY-NC-SA 4.0, which is
non commercial. For a model that is going to take real positions that is a genuine blocker
rather than a formality. The Kaggle set recommended below is CC0 public domain, which is why
it is the one to start with despite not being the biggest.

---

## The timestamp problem, and how to fix it

This decides whether historical data can train the same model that runs live, so it comes
before the source comparison.

What STS captures live, from `data/coins-2026-08-20.jsonl`:

- `t` is a wall clock millisecond timestamp, for example `1787245956325`.
- `who[].at` is each wallet's first trade offset in seconds to two decimals, for example
  `2.21`, `5.89`, `7.7`.
- `open` counts distinct buying wallets in the first 3 seconds.
- `market.candles` are 1 second buckets.

None of that sub second precision exists on chain. Verified against a real pump.fun
transaction pulled live: `blockTime` is `1787794551`, a whole second, with no fractional
part. Every historical source on this list, including the raw ledger, inherits that limit,
because the ledger simply does not record a finer timestamp.

What history does give, which is arguably better:

- **Slot number.** Measured across two verified anchor points (epoch 660 at slot 285,120,000
  is 2024-08-22, epoch 1000 at slot 432,000,000 is 2026-07-10), a slot is **0.404 seconds**.
- **Transaction index within the block.** This gives exact ordering of trades inside a single
  slot, which wall clock milliseconds cannot do reliably anyway, because the live listener's
  clock reflects when it heard about the trade, not when the trade landed.

So the STS 3 second window is **7.4 slots**.

**Recommendation: redefine the open window in slots, not seconds, and recompute the live
capture the same way.** Use "first 8 slots after the create instruction" as the window and
`(slot, tx_index)` as the ordering key. Then live and historical features are the same
feature, and the model trains on one definition. If the window stays defined in wall clock
seconds, the historical rows and the live rows will be subtly different distributions and the
model will learn the difference between the two capture methods rather than the market.

This is a real change to STS, and it is worth doing before spending a month of bandwidth on a
backfill that will not line up.

Second, smaller note: pump.fun's `TradeEvent` carries its own `timestamp` field, but it is
just the block time in whole seconds. Verified by decoding a live event, where the event
timestamp `1787794551` exactly equals the block time. It adds no precision.

---

## What a pump.fun launch looks like on chain, and why that is good news

This was verified by decoding a real transaction end to end, and it determines which sources
can work at all.

pump.fun is an Anchor program, and it emits its events into the transaction log as
`Program data: <base64>`. Decoding one live gave, from the log line alone:

```
mint      = AHRfdcTkYVfyD8HZMFpBcKKTtdRwaFg6Ut7rX383pump
solAmount = 0.977777777 SOL
tokenAmt  = 3,341,453.80
isBuy     = True
user      = AX4otdfVBUxQKqXtoQohb6WGteyEvp6c4oPyfo3b9WdP
virtualSolReserves   = 97.5440
virtualTokenReserves = 330,004,968
```

That is nearly the whole STS on chain schema out of one field. `who[]`, `open`, `total`,
`curve`, and every 1 second candle in `market.candles` can be rebuilt from a stream of these
events. The bonding curve program is `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P` and the
PumpSwap AMM is `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`, both already referenced in the
STS source.

The funding block is separate and comes from a different place: **native SOL balance deltas**,
that is `preBalances` and `postBalances` per account on every transaction. The same live
transaction showed clean deltas:

```
AX4otdfVBUxQKqXtoQohb6WGteyEvp6c4oPyfo3b9WdP   -1.004079080
AgEXK4fnfWM8jbcQTdZB2sNiKFcAkgvKmbaPDTsrw4KZ   +0.977777777
```

So any source that carries **log messages plus pre and post balances plus slot plus
transaction index** can reproduce the full STS record. That is the test applied to every
source below.

One incidental finding worth knowing: of 300 consecutive recent transactions touching the
pump.fun program, **only 24 succeeded**. **That 92 percent is not the failure rate of pump.fun trades**, and it
is the retired 93.2% in a different costume: `getSignaturesForAddress` indexes on account-key
mention, so it returns the chaff that merely names the program and dies before its code runs.
Counted over whole blocks the real rate is **11.3 percent**, 12,429 of 109,883 pump
transactions across seven windows and 22 months. A whole-block pull that drops nine in ten has
a bug. What follows was written believing 92 percent fail, mostly losing sniper
attempts. A backfill must filter on success or the trade counts will be wildly wrong, and the
failures are themselves probably a useful signal about contention at launch.

---

## Comparison table

Read "Fits STS" as: can it produce launches, trades, the funding graph, and slot level
ordering.

| Source | pump.fun decoded? | History | SOL transfers | Time resolution | Free tier reality | Fits STS |
|---|---|---|---|---|---|---|
| **Old Faithful + Jetstreamer** | No, raw, but logs decode cleanly | Genesis to about 5 days ago | Yes, pre/post balances | Slot + tx index | Archive genuinely free, bandwidth is the cost | **Yes, fully** |
| **Published open datasets** | Yes, already decoded | 5 to 6 months, not contiguous | Mostly no | Varies, best has slot + tx_idx | **Genuinely free**, some non commercial | Partly, best starting point |
| **NoLimitNodes parquet** | Yes, 7 ready tables | Genesis to now | **No** | Slot, tx index unclear | Paid only, 200 USD/month | Partly, no funding graph |
| **BigQuery public Solana** | No, raw only | Unreliable, see below | Yes, `balance_changes` | Slot + `index` | 1 TiB/month free, then 6.25 USD/TiB | Schema yes, freshness and cost no |
| **Dune** | Yes, `pumpdotfun_solana` | Full | Yes, `account_activity` | Slot + `tx_index` | 2,500 credits/month free | Great for aggregates, cannot bulk export |
| **Helius** | No, raw only | Genesis, archival on free tier | Yes | Slot + tx index | 1M credits/month, 10 rps | Validation and targeted lookups only |
| **GCS ledger archives** | No, raw RocksDB | Genesis | Yes | Slot + tx index | Requester pays, about 25,000 USD egress | No |
| **Flipside** | n/a | n/a | n/a | n/a | **Shut down June 2026** | No |
| **Bitquery** | Yes | Self serve is real time only | Partly | Could not verify | 7 day trial only | No |
| **Solscan Pro** | Partly | **DeFi activity only 6 months** | 3 years | Could not verify | Could not verify | No |
| **Allium** | Yes | Full | Yes, `sol_balances` | Could not verify | 20,000 credits | Enterprise sales only |
| **Topledger** | Not named | Genesis, parquet | Could not verify | Could not verify | No free tier, from 750 USD/month | Priced out |
| **Vybe / Shyft / Syndica / others** | Mostly no | Mostly shallow | Mostly no | Could not verify | Small | No |

---

## 1. Old Faithful, the recommended source

**What it actually is.** An open, verified, complete copy of the Solana ledger in CAR files,
built by Triton One under a Solana Foundation grant, covering genesis to the present. The docs
state plainly that the Triton copy, called OF1, "is currently completely free to use"
(https://docs.old-faithful.net/llms-full.txt).

**Verified by direct request, not from docs:**

- Epoch CAR files are served at `https://files.old-faithful.net/<epoch>/epoch-<epoch>.car`.
  Measured sizes: epoch 600 is 595.7 GB, epoch 800 is 824.3 GB, epoch 1000 is 767.4 GB,
  epoch 1015 is 1184.5 GB.
- **HTTP range requests work.** A request for bytes 0-63 of epoch 800 returned HTTP 206 with
  exactly 64 bytes, and the bytes were a valid CAR header. This means partial and random
  access is possible without downloading a whole epoch.
- The archive is current. Epoch 1021 exists, epoch 1022 does not, and the live chain is at
  epoch 1023. So it trails the tip by roughly two epochs, about five days. The docs target
  "each CAR and indexes within 4 epochs of the epoch closing".
- A `gsfa` index, which maps an address to all its signatures, is published per epoch at
  `epoch-<epoch>-gsfa.index.tar.zstd`. Measured: epoch 800 is 39.9 GB, epoch 950 is 33.1 GB,
  epoch 1000 is 39.7 GB, all compressed.

**Coverage of the STS schema.** Full. Jetstreamer's README confirms transaction logs "are
available in `transaction_status_meta.log_messages` for all epochs"
(https://github.com/anza-xyz/jetstreamer). Combined with pre and post balances, slot, and
transaction index, this is everything. It is the only source on this list that is both free
and complete.

**The real cost is bandwidth.** Two years is epochs 660 through 1021, which is 362 epochs.
Across 14 measured epochs in that range the mean CAR size is 724 GB, so the full two year
window is about **262 TB** that has to cross the wire. That is the honest number and it is
what rules out doing this on the Mac:

- at 100 Mbit/s, 243 days
- at 500 Mbit/s, 49 days
- at 1 Gbit/s, 24 days

**Why the Mac cannot do it, plainly.** One single epoch, which is two to three days of chain
history, is 595 to 1184 GB. The Mac has 120 GB free. A single epoch does not fit, let alone
362 of them. The full local archive is documented at "~350TB as of Epoch 827". Downloading
the ledger is not on the table.

**What makes it work anyway: Jetstreamer.** Anza's Jetstreamer streams the archive over the
network and hands transactions to a plugin, with, in the docs' words, "no storage
requirements". It is Rust with a trait based plugin API, which matches the STS Rust and Tauri
stack, so the pump.fun decoder can be written as a Jetstreamer plugin and only pump.fun rows
ever get written to disk. It takes epoch ranges directly, for example
`cargo run --release -- 900-950`. Its buffer defaults to `min(4 GiB, 15% of available RAM)`,
so it adapts to a small machine rather than falling over.

The archive is physically in Amsterdam and the docs say to "run from nearby for best
throughput". The peak figure of 2.7M TPS was on "64 core CPU, 30 Gbps+ network", which is not
the point; the point is that on ordinary hardware it is network bound, not CPU or RAM bound.

**So rent the bandwidth.** A Hetzner dedicated server has "a dedicated 1 GBit uplink by
default and with it unlimited traffic" (https://docs.hetzner.com/robot/general/traffic/), from
about 40 euros a month, in Germany, one short hop from Amsterdam. At 1 Gbit/s the full two
year scan is about 24 days, which fits in a single month's rental. The filtered pump.fun
output is small enough to bring home afterwards.

Honest caveats: 24 days assumes the CDN actually sustains a full gigabit, and Jetstreamer's
own docs mention coping with "CDN throttling", so budget for it to take longer. If it matters,
Hetzner offers 10 Gbit uplinks at extra cost, which would cut the scan to under three days.

**A hosted lookup service exists on paper but is down right now.** The docs advertise
`https://cid.old-faithful.net/api/v1/sig-to-cid/<sig>` and
`https://cid.old-faithful.net/api/v1/slot-to-cid/<slot>`, flagged as "currently in progress".
Tested against a recent slot, an old slot in epoch 800, and a real signature: **all three
returned HTTP 522**, a Cloudflare connection timeout, so the service is not reachable today.

This is worth watching, because if it comes back it changes the economics. A CID lookup plus
the already verified HTTP range support would let a single transaction be fetched from the
archive with two requests and no local storage at all, which would make targeted funding graph
backfill possible directly from the Mac and would avoid renting anything. Retest it before
committing to a full scan, and ask Triton whether a hosted Old Faithful RPC endpoint is
available while you are at it.

---

## 2. BigQuery public Solana dataset

**It exists, and the schema is genuinely well suited.** The dataset id is
`bigquery-public-data.crypto_solana_mainnet_us`, listed in
https://github.com/blockchain-etl/public-datasets, which shows one table, Transactions, with a
2 to 5 minute lag.

The ETL defines seven tables (Accounts, Block Rewards, Blocks, Instructions, Token Transfers,
Tokens, Transactions) but only Transactions is listed as public. Fortunately Transactions is
the one that matters. From
https://github.com/blockchain-etl/solana-etl/blob/main/src/solana_config/schemas/transactions_schema.json:

```
block_slot, block_hash, block_timestamp, signature,
index,                          <- transaction order within the block
accounts[]  {pubkey, signer, writable}
log_messages[]                  <- pump.fun events decode from here
balance_changes[] {account, before, after}   <- the funding graph
pre_token_balances[], post_token_balances[]
```

That is all four things STS needs. On paper this is the nicest source on the list.

**Three problems, and together they are disqualifying for a full backfill.**

*Reliability.* The dataset stalled once already, stopping on 2025-03-31 and resuming around
2025-04-06, and was reported days behind again on 2025-11-25
(https://discuss.google.dev/t/public-solana-bigquery-dataset-crypto-solana-mainnet-us-stopped-updating-on-march-31-2025/185629).
Google never posted an official cause in that thread. **Could not verify** whether it is
current as of today, because that needs an authenticated Google Cloud account and no
`gcloud`, `bq` or `gsutil` is installed on this machine.

*Known gaps.* The maintainers state the initial load is missing 13,602 blocks, about 0.006
percent, plus 18,879 blocks with missing or duplicated transactions, about 0.009 percent
(https://github.com/blockchain-etl/solana-etl/blob/main/docs/bigquery-release-notes.md). Small,
but it means holes in the launch set that will not be obvious.

*Cost.* The free tier is "1 TiB of querying per month" and "10 GiB of storage per month"
(https://cloud.google.com/free/docs/free-cloud-features). Above that it is "$6.25 / 1 tebibyte"
(https://cloud.google.com/bigquery/pricing). Querying a public dataset costs the querier only
bytes scanned, not storage. The catch is that BigQuery bills the whole column across the
scanned partitions, and `log_messages` is the largest column in Solana's largest table, holding
every vote transaction's logs too. Filtering to pump.fun does not reduce what gets scanned. A
two year pull touching `log_messages` and `balance_changes` would very likely run into the
thousands of dollars, and 1 TiB free per month buys well under a day of chain at a time.

**Verdict.** Excellent for spot checks, for pulling one specific day cheaply, and for
validating a decoder against a known slice. Not the vehicle for the corpus. Worth ten minutes
to log in and check freshness and table size before dismissing entirely, since if it is
healthy it is the fastest way to get a first real slice today.

**No decoded pump.fun exists in BigQuery.** The community parsed table project
(https://github.com/nansen-ai/solana-etl-table-definitions) only carries `mango` and
`metaplex`. There is no `solana_pumpfun` dataset.

---

## 3. Flipside Crypto: gone

Not usable. Its blockchain data business went to SonarX, announced 2026-05-19
(https://www.sonarx.com/blog/sonarx-acquires-flipside-crypto-blockchain-data-business), the
Flipspace platform ran only to 2026-06-17, and every Flipside domain now redirects to an
unrelated AI product. SonarX publishes no free tier. Nothing to evaluate.

---

## 4. Solana Foundation and public GCS ledger archives

These exist and are public, and they are still the wrong answer.

Verified by direct request against the Google Cloud Storage API: the bucket
`mainnet-beta-ledger-us-ny5` returns

```
"Bucket is a requester pays bucket but no user project provided."
```

and `mainnet-beta-ledger-europe-fr2` returns the same. So they are real, publicly addressable,
and **requester pays**, meaning the downloader pays egress. Old Faithful's own docs confirm
this bucket is the Anza produced warehouse node archive.

Google's egress price is "$0.12 / 1 gibibyte" up to 10 TiB, "$0.11" to 150 TiB, and "$0.08"
above (https://cloud.google.com/storage/pricing). At roughly 250 TB that is about **25,000 US
dollars** in egress alone.

On top of the cost, the contents are raw RocksDB ledger and snapshots, which have to be
replayed with `solana-ledger-tool` to become queryable. Old Faithful's validation docs put the
requirement at "40 GiB available RAM (total system memory recommended at least 64 GiB)" and
"2 TiB available disk space (ideally NVME)" just to verify a single epoch. The Mac has 8 GiB
and 120 GiB free.

**Verdict: no.** Old Faithful exists precisely so nobody has to do this. It is the same data,
already converted, already validated, and free.

---

## 5. Prepared datasets that already exist

**NoLimitNodes historic pump.fun archive** is the only ready made product found that targets
this exact job (https://nolimitnodes.com/products/historic-pump-fun). Seven Parquet plus CSV
tables: `pumpfun_creates`, `pumpfun_trades`, `pumpfun_graduations`, `pumpfun_creator_aggregates`,
`pumpfun_token_lifecycle`, `pumpfun_priority_fees`, `pumpfun_post_graduation_trades`. Coverage
"Genesis to now", "14M+" mints, "~1.8B" trades, "~38,000" graduations, bundles "6-15 GB /
month" as tar.zst, last verified by the vendor 2026-04-29.

Price is **200 US dollars a month**, 30 percent off at six months, 50 percent at twelve, and
custom slot ranges are "quoted separately" as a one time download.

`pumpfun_creates` carries "mint, creator wallet, virtual SOL/token reserves, metadata URI,
timestamp, slot" and `pumpfun_trades` carries "side, SOL amount, token amount, post-trade
reserves, fee, signer".

**The gap that matters: no SOL transfers and no wallet funding data.** The STS funding block,
which resolves who sent SOL to the deployer and the early buyers, cannot be built from it.
Transaction index within the block is also not stated, so intra slot ordering is unconfirmed.

So it covers the launch and trade half well and the funding half not at all. It is worth
considering as an accelerator rather than a replacement: one month at 200 dollars gets a
decoded corpus in days instead of weeks, and the funding graph gets backfilled separately
against the same mint list.

Treat the vendor as unverified. Buy one month, check a few hundred of its rows against
transactions pulled from a free Helius key, and only then trust it.

### Published open datasets: no two year corpus, but enough to start this week

A full sweep of HuggingFace, Kaggle, Zenodo, GitHub and arXiv supplements was run. **Nothing
published covers two years.** But several real datasets exist, and together they cover roughly
five or six non contiguous months. That is not the training corpus, but it is more than enough
to build and validate the decoder before spending money on bandwidth, which changes the
sensible order of work.

The ones worth having, all verified by reading the actual files rather than the README:

**Kaggle `dremovd/pump-fun-graduation-february-2025`** is the one to start with.
6.70 GB of CSV, **CC0 public domain**, so no licence friction at all. Its parsed transaction
chunks carry `block_time`, `slot`, `tx_idx`, `signing_wallet`, buy or sell direction, token
and SOL amounts, and both virtual balances after each trade. That is `slot` plus `tx_idx`,
exactly the ordering key recommended above, so it can validate the slot based window directly.
Limit: trades cover only the first 100 blocks after each mint, about the first 40 seconds,
which happens to line up almost perfectly with the STS 60 second follow window.

**HuggingFace `Slinky21/Pumpfun_Memecoin_Corpus`** is the cleanest full lifecycle slice.
6.70 GB of Parquet, verified row counts of 798,430 tokens, 33,581,765 trades, 26,934,864
snapshots and 1,016,374 wallet stats, covering 2026-06-05 to 2026-07-14, so 39 days.
Timestamps are genuine microseconds and trades carry `seconds_since_launch` as a float, which
is the closest published match to the STS `at` field anywhere. It also ships a
`KNOWN_ISSUES.md` documenting that 3.38 percent of trade rows have inconsistent SOL amounts,
which is unusually honest and means the bad rows can be filtered rather than silently
absorbed. Licence is ambiguous, CC BY 4.0 in the card body but `mit` in the metadata field, so
resolve that before leaning on it.

**HuggingFace `Zinteck/MELT`** is by far the largest, at 218.5 million transactions verified
by summing every Parquet footer, covering 2024-12 to 2025-03 across 41,470 tokens, with Jito
bundle traces and transfer classification included. **But it is CC BY-NC-SA 4.0, non
commercial.** For a trading engine that is a real blocker, not a technicality. Useful for
research and for checking a decoder against, not for training something that takes positions.

**Kaggle `btclee/memecoins`**, 11.71 GB, MIT licence, has `pumpfun_mints` spanning 2024-11 to
2025-11, which is 13 months and the longest launch coverage found anywhere. The catch is that
its swap data is a single day, 2025-01-01. Worth pulling for the mint list alone, which is a
free 13 month spine to hang a funding graph backfill on. Note its timestamps are
America/Los_Angeles, not UTC, which is an easy mistake to make.

Also real but narrower: **`twainayar/pumpfun-30s-september-2025`** on Kaggle, 3.53 GB, MIT, the
first 30 seconds of every September 2025 launch with 34 columns including holder counts and
top 10 share; and Zenodo **RED-PUMP-2026-v1**, 860,213 launches over 34 days with millisecond
timestamps, though its outcome labels are unreliable by the authors' own published corrigenda
and only the launches file should be trusted.

**Things that look useful and are not.** `solarchive/solarchive` on HuggingFace advertises
complete Solana history but actually holds 477 daily partitions covering 2020, 2021 and about
35 days of late 2025, with nothing at all for 2022 through most of 2025. Its `tokens/` tree
looks like a launch registry but a month sample held 47,578 rows of which only 95 were pump
mints, so it is a partial Metaplex capture. `biznus1/pumpswap-historical-trades` advertises
245.9 million trades and contains one 3.2 MB sample file. `rincel/pumpfun` is 1.18 GB of a
single column of wallet addresses, not a dataset.

**Academic papers with the data you want but no release.** Three were checked and none publish
their corpus. The most painful is https://arxiv.org/html/2602.14860v1, which parsed 655,770
tokens and 2 to 5 million trades a day directly from the pump.fun programs and declined to
release on the grounds that the chain is public. Which is true, and is exactly why the Old
Faithful path below is the answer.

**pump.fun's own API is free but cannot backfill.** Verified: `frontend-api-v3.pump.fun`
answers unauthenticated and returns full launch metadata including `mint`, `creator`,
`created_timestamp`, `bonding_curve`, `virtual_sol_reserves` and `total_supply`. Two hard
limits found by testing: paging works at offset 1000 but returns empty at offset 5000 and
beyond, so only a few thousand records are reachable from either end; and
`created_timestamp` is always a whole second times 1000, so no millisecond precision. Useful
confirmation from it: sorting ascending gives the oldest coin at **2024-01-25**, so pump.fun
history is about 2 years 7 months, comfortably covering the two year target. Good for live
tailing and for cross checking, useless for history.

---

## 6. Everything else

None of these has a free tier that can carry this volume.

**Dune.** Worth calling out because it is already wired into this machine and it has by far the
best data model of anything here. Checked directly against the live catalog, not from docs:
the `pumpdotfun_solana` schema holds **127 decoded tables**, including
`pump_evt_createevent`, `pump_evt_tradeevent`, `pump_call_create`, `pump_call_create_v2`,
`pump_call_buy`, `pump_call_sell`, and the whole `pump_amm_*` family for PumpSwap.

The column lists line up with the STS record almost exactly.
`pump_evt_createevent` carries `mint`, `creator`, `bonding_curve`, `virtual_sol_reserves`,
`virtual_token_reserves`, `real_token_reserves` and `token_total_supply`, which is the STS
`curve` block verbatim. `pump_evt_tradeevent` carries `mint`, `sol_amount`, `token_amount`,
`is_buy`, `user`, `creator_fee` and the reserves, which is everything `who[]` needs. Both
carry `block_slot`, `block_time`, `tx_index`, `outer_instruction_index`,
`inner_instruction_index`, `tx_id` and `tx_signer`, so full intra block ordering is there.
`solana.account_activity` covers pre and post balance changes for the funding graph.

If Dune were exportable in bulk it would be the answer outright. It is not. The limit is
export, not compute: free is 2,500 credits a month, Analyst is 75 USD a month, Plus is 399 USD
a month (https://docs.dune.com/learning/how-tos/credit-system). Dune will happily compute over
a billion rows and will not hand them over.

**So use Dune to design and validate features, not to extract them.** It is the cheapest place
to answer "is this feature worth computing at all" before spending a month of bandwidth
building it. Note also that the connected Dune account here reports
`subscriptionPlanName: visitor_fluid_engine` with a **credits quota of 0**, verified by
calling the usage endpoint, so it cannot run a single query as it stands. That needs a proper
login first.

**Helius.** Raw Solana only, no pump.fun decoding, and the Enhanced Transactions API is
documented as "a legacy product in maintenance mode". But the free tier is 1M credits a month
at 10 requests per second **with archival data included**, which is unusual and useful. Full
genesis history, perfect slot and index resolution, and pre and post lamport balances. At 10
rps it will never extract the corpus, but it is the right tool for validating another source
and for tracing a few thousand specific funding wallets. **Keep it as the reference
implementation.**

**Bitquery.** Genuinely good pump.fun decoding, including creates, curve trades and
graduations. Disqualified on archive depth: self serve plans are real time only with 30 day
trade retention, and history is a separate 70 to 100 USD a month add on layered on top of a
239 USD a month plan. Bulk S3 and Parquet export exists only on unpriced Enterprise.

**Solscan Pro.** Disqualified outright. Its own FAQ caps DeFi Activities, where trades live,
at **6 months**. Two years of trades is not purchasable at any published price.

**Allium.** Best data model of the paid vendors, with pump.fun in `dex_trades` plus
`sol_balances` and `credit_debit` for funding, and proper bulk delivery to S3 and BigQuery.
The free tier is 20,000 credits, which is a sample. No prices published anywhere; enterprise
sales only.

**Topledger.** Sells "Historical raw & decoded data" in Parquet from genesis, which is exactly
the right product shape, but plans start at 750 USD a month with no free tier, and no pump.fun
dataset is named. It is not open data.

**QuickNode** free is 10M credits at 15 rps, raw RPC only, same role as Helius. **Shyft** at
199 USD a month has the right billing shape, unlimited credits with a rate limit, but its
archive depth is undocumented. **Vybe** is real time oriented with 30 day balance history and
no verified native SOL transfers. **Chainbase**, **Birdeye**, **Codex**, **Moralis** and
**Footprint** are priced for dashboards and bots, with free tiers between 10,000 and 30,000
requests. **DEX Screener** and **GeckoTerminal** are free but serve snapshots and candles, not
raw trades. **Syndica** could not be verified at all; its pricing page is client rendered and
returns 404 to every automated fetch, so it needs a human with a browser.

---

## Recommendation

**Primary: Old Faithful, read with Jetstreamer, on a rented European server for one month.**

Total cost about **40 euros**, once. It is the only path that is free at the data layer,
complete back past pump.fun's first day, and carries all four things STS needs: launch events,
trade events, native SOL balance deltas for the funding graph, and slot plus transaction index
ordering. Because the filter runs inside a Jetstreamer plugin, only pump.fun rows are ever
written, so the output that comes home is a few hundred GB at most rather than 262 TB.

It also keeps STS self sufficient. No vendor can deprecate it, reprice it, or sell itself to
someone else, which is exactly what just happened to Flipside and is a live risk with every
paid option here.

**Runner up: NoLimitNodes, 200 US dollars for one month**, ideally as a one time slot range
quote rather than a subscription. It buys decoded launches and trades in Parquet in days
instead of weeks. It does not include the funding graph, so it does not remove the need for a
ledger pass, but it would let model work start on the launch and trade features immediately
while the funding backfill runs. Validate its rows against Helius before trusting it.

**Start with, regardless of which of those two you pick: the free published datasets**, above
all the CC0 Kaggle February 2025 set. They cost nothing, they are already decoded, and the CC0
one already carries `slot` and `tx_idx`. They will not train the final model, but they will
prove the feature pipeline and the new slot based window in days rather than weeks, and that
work has to happen before either paid path is worth starting.

The honest summary of the money question: the data is free, the bandwidth is about 40 euros,
and the 200 dollar option only buys time, not capability. If time matters more than money this
month, buy both and run them in parallel, since NoLimitNodes covers launches and trades while
the ledger pass covers the funding graph that NoLimitNodes lacks.

**Do not** plan around Flipside, the GCS ledger buckets, or Solscan. And do not train
commercially on MELT.

---

## First step, this week

The goal for week one is a proven decoder and a settled feature definition, not a corpus. Do
not rent anything yet, and do not pay anyone yet.

**1. Fix the window definition first, before anything else.** Change the open window from 3
seconds to 8 slots and the ordering key to `(slot, tx_index)`, in the live listener as well as
in whatever reads history. Everything downstream depends on this. Doing it after a month long
backfill means doing the backfill twice.

**2. Download the CC0 Kaggle set today and use it as the bench.** Pull
`dremovd/pump-fun-graduation-february-2025`, 6.70 GB, public domain, no account friction
beyond a Kaggle login. It already carries `slot` and `tx_idx`, so the new slot based window can
be implemented and tested against real pump.fun trades within hours, with no streaming
infrastructure at all. Its 100 block per mint window is close enough to the STS 60 second
follow window to be a fair test. This is the fastest path from here to a working feature
pipeline.

**3. Prove the decoder against the archive on one epoch, locally, for free.** Clone
https://github.com/anza-xyz/jetstreamer and run a single recent epoch in sequential mode with a
small buffer so it behaves on 8 GB:

```
JETSTREAMER_THREADS=4 cargo run --release -- 1020 --sequential --buffer-window 1GiB
```

Write a plugin that keeps only transactions mentioning
`6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P` or
`pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`, decodes the `Program data:` events into
`CreateEvent` and `TradeEvent`, reads the funding edges off the pre and post balances, and
emits the existing STS coin record shape. Remember to filter on transaction success, though expect
to drop about **11 percent**, not 92 — see the note above. This will be slow on a home connection and that is fine, because the
point is correctness, not coverage. Let it run partway and stop it.

**4. Validate against ground truth already on disk.** Epoch 1020 covers roughly 2026-08-20, and
`data/coins-2026-08-20.jsonl` holds 5,392 coins captured live that same day. Reconstruct those
mints from the archive and diff them field by field. Deployer, opening buy, per wallet SOL in
and out, the funding transfers, and the candles should all match closely. Wallet ordering
inside a slot will not match the live `at` offsets, and that mismatch is expected, acceptable,
and precisely the reason for step 1. This diff is the single most valuable thing produced this
week, because it is the only way to know the historical rows and the live rows are the same
thing.

**5. Only then rent the box.** Once the diff is clean, take a Hetzner dedicated server for one
month, run epochs 660 through 1021, and write Parquet. Use `--reverse` so the most recent and
most relevant history lands first and the run stays useful even if it gets cut short.

Two cheap things to do in parallel, both of which could save the rental entirely. Retest
`https://cid.old-faithful.net/api/v1/slot-to-cid/<slot>`, which is down today, and email Triton
One to ask whether a hosted Old Faithful endpoint exists, since targeted lookups would beat a
full scan by a wide margin. And log in to Google Cloud to check whether the BigQuery Solana
table is current and how large it is, since if it is healthy it is the fastest way to pull a
real slice today inside the free 1 TiB.

---

## Sources

- Old Faithful docs: https://docs.old-faithful.net/ and https://docs.old-faithful.net/llms-full.txt
- Old Faithful files, measured directly: https://files.old-faithful.net/
- Jetstreamer: https://github.com/anza-xyz/jetstreamer
- yellowstone-faithful: https://github.com/rpcpool/yellowstone-faithful
- BigQuery public datasets list: https://github.com/blockchain-etl/public-datasets
- Solana ETL schemas: https://github.com/blockchain-etl/solana-etl
- BigQuery known gaps: https://github.com/blockchain-etl/solana-etl/blob/main/docs/bigquery-release-notes.md
- BigQuery dataset staleness reports: https://discuss.google.dev/t/public-solana-bigquery-dataset-crypto-solana-mainnet-us-stopped-updating-on-march-31-2025/185629
- Google Cloud free tier: https://cloud.google.com/free/docs/free-cloud-features
- BigQuery pricing: https://cloud.google.com/bigquery/pricing
- Cloud Storage pricing: https://cloud.google.com/storage/pricing
- Hetzner traffic policy: https://docs.hetzner.com/robot/general/traffic/
- Flipside sale to SonarX: https://www.sonarx.com/blog/sonarx-acquires-flipside-crypto-blockchain-data-business
- NoLimitNodes pump.fun archive: https://nolimitnodes.com/products/historic-pump-fun
- Dune credit system: https://docs.dune.com/learning/how-tos/credit-system
- Nansen parsed Solana tables: https://github.com/nansen-ai/solana-etl-table-definitions

Published datasets:

- Kaggle, CC0, Feb 2025, has slot and tx_idx: https://www.kaggle.com/datasets/dremovd/pump-fun-graduation-february-2025
- HuggingFace, 39 day full lifecycle, microsecond timestamps: https://huggingface.co/datasets/Slinky21/Pumpfun_Memecoin_Corpus
- HuggingFace, MELT, 218.5M transactions, non commercial licence: https://huggingface.co/datasets/Zinteck/MELT
- Kaggle, MIT, 13 months of mints, one day of swaps: https://www.kaggle.com/datasets/btclee/memecoins
- Kaggle, MIT, first 30 seconds of every Sept 2025 launch: https://www.kaggle.com/datasets/twainayar/pumpfun-30s-september-2025
- Zenodo, RED-PUMP-2026-v1, 860,213 launches, launches file only: https://doi.org/10.5281/zenodo.21923106
