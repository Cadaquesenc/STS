# The zero cost path to pump.fun history

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

Researched 2026-08-27. This doc answers one question only: what can you get for actually zero
money, not cheap, not 40 euros. Every number below was measured live or read off a page that
was fetched, with the URL. Where something could not be checked it says "could not verify".

This builds on `TRAINING_DATA_SOURCES.md` and `TRAINING_DATA_DUNE.md` and does not repeat the
paid analysis in them.

---

## The short version

**There is a free source that serves the complete Solana ledger, from genesis to today, with
no account, no API key, and no credit card. It is `https://api.mainnet-beta.solana.com`, the
official Solana public RPC endpoint.** It is now backed by Old Faithful. Verified live: it
returned block data from slot 1000, from March 2021, from February 2023, from January 2024,
from November 2024, and from 2026-08-21. Full blocks come back with `logMessages`,
`preBalances`, `postBalances`, slot, and transaction order, which is every single thing STS
needs.

So the data problem is solved for free. What is not free is **time**. The endpoint caps you at
100 MB per 30 seconds per IP, and this Mac's connection is roughly the same speed anyway.
Measured end to end, that works out to roughly **10,000 to 19,000 launches per day of
unattended pulling**. A full two year corpus of 14 to 30 million launches would take about two
and a half years at that rate, so the full corpus is not achievable on this machine.

What IS achievable, for zero money, starting tonight:

- **About 100,000 launches in a week**, sampled across the whole two year window, with complete
  logs, complete SOL balance deltas for the funding graph, and exact slot plus transaction
  index ordering. That is a real training set.
- **The full two years for zero money is possible on Oracle Cloud Always Free**, which gives 2
  ARM cores, 12 GB of memory, free inbound bandwidth and 2 Gbps of network, forever, for
  nothing. The arithmetic says about 22 days of running. It carries real execution risk, laid
  out below.

Three findings that change the picture from the earlier docs:

**1. The 262 TB number is not the obstacle it looked like.** It was never necessary to download
the archive. The public RPC and the Old Faithful server both fetch individual blocks with HTTP
range requests, so you pay for the slots you actually ask for and nothing else. Verified: a
range request into the middle of the epoch 1020 CAR returned HTTP 206 with exactly the bytes
asked for, and the same works on the index files.

**2. The bottleneck is this Mac's internet connection, not any source.** Measured: Old Faithful
served 2.92 MB/s on one stream and 4.44 MB/s across eight. A Hetzner speed test file served
1.59 MB/s at the same time. So the home line is roughly 16 to 35 Mbit/s and it varies. Every
free path collapses to the same wall. Renting a server does not fix a free plan, but Oracle's
free tier does.

**3. Turning on gzip makes the public RPC three times faster and almost nobody does it.**
Verified: one block came back as 6.84 MB uncompressed and 2.20 MB with `Accept-Encoding: gzip`,
a 3.1x saving, and the request took 2.6 seconds instead of 6.4. This is a one line change and
it is the single highest leverage thing in this document.

---

## 1. The find: the official public RPC is a free archive node

This is the part that was missed before, so it gets checked carefully.

**Verified by live request, not by docs.** Every one of these returned a real block:

| What | Slot | Result |
|---|---|---|
| Near genesis | 1,000 | OK, 4 signatures |
| March 2021 | 70,000,000 | OK, 2021-03-20, 1,009 signatures |
| February 2023 | 180,000,000 | OK, 2023-02-28, 4,287 signatures |
| Just after pump.fun launched | 245,000,000 | OK, 2024-01-30, 1,570 signatures |
| November 2024 | 300,000,000 | OK, 2024-11-07, 2,541 transactions |
| STS ground truth day | 440,486,139 | OK, 2026-08-20 13:59:37 UTC, 1,711 transactions |

For comparison, every other free public endpoint tested refused. `solana-rpc.publicnode.com`
said "Block 285120000 cleaned up, does not exist on node. First available block: 441520738",
which is about 3 days of history. `solana.drpc.org` said "chain is not available on free plan".
`rpc.ankr.com/solana` returned 403. `api.blockeden.xyz` said "A paid plan is required". So this
is not a general property of public RPCs. It is specific to the official Solana Foundation
endpoint, and it is because Old Faithful now sits behind it.

**It carries everything STS needs.** Checked on the 2026-08-20 block, the exact day
`data/coins-2026-08-20.jsonl` was captured live:

```
slot 440486139  2026-08-20 13:59:37 UTC  6.84 MB  1,711 transactions
  pump.fun transactions:            35  (33 succeeded)
  with a Create instruction:         1
  with a "Program data:" event log: 33
  meta has logMessages:            yes
  meta has preBalances:            yes
  meta has postBalances:           yes
```

That is a launch, its trades, the decodable `CreateEvent` and `TradeEvent` payloads, and the
native SOL deltas for the funding graph, all in one free call, on the exact day there is a live
capture to diff against.

**Deep history lookups work too.** `getTransaction` on a November 2024 signature returned the
transaction with its logs. `getSignaturesForAddress` on the pump.fun bonding curve program with
a November 2024 cursor returned 1,000 signatures spanning slots 299,999,965 to 299,999,999. So
the address index is live for old epochs as well, not just recent ones.

**The published limits** (https://solana.com/docs/references/clusters):

- Maximum number of requests per 10 seconds per IP: 100
- Maximum number of requests per 10 seconds per IP for a single RPC: 40
- Maximum concurrent connections per IP: 40
- Maximum connection rate per 10 seconds per IP: 40
- **Maximum amount of data per 30 seconds: 100 MB**

That last one is the binding constraint: 100 MB per 30 seconds is 3.33 MB/s.

The docs also warn: "The public RPC endpoints are not intended for production applications.
Please use dedicated/private RPC servers when you launch your application, drop NFTs, etc."
Read that as written. Using it to backfill a research dataset at a polite rate is within the
published limits. Do not point the live trading listener at it.

### The arithmetic

Measured, from this Mac, pulling full blocks with gzip on and 8 requests in flight:

```
recent era (2026, slot 440,487,000 range)
  block size            6.8 MB uncompressed, 2.2 MB on the wire
  sustained rate        0.35 blocks/s   (6 of 16 requests hit the throttle and had to retry)
  per day               30,300 blocks
  chain time per day    3.4 hours of chain per day of pulling

older era (2024, slot 300,000,000 range)
  block size            3.97 MB uncompressed
  sustained rate        0.5 blocks/s without gzip, so roughly 1.0 blocks/s with it
  per day               about 86,000 blocks
  chain time per day    about 9.7 hours of chain per day of pulling
```

Averaging across the two year window, call it **0.6 blocks/s, about 52,000 blocks a day, about
5.8 hours of chain per day of pulling.**

Launch rate is about 78,000 a day today and lower earlier in the window, so somewhere between
40,000 and 78,000 a day across the period. 5.8 chain hours is 24 percent of a chain day, so:

```
launches per day of pulling    10,000 to 19,000
100,000 launches               5 to 10 days
325,000 launches               17 to 33 days
14,000,000 launches            740 to 1,400 days, so 2 to 4 years
```

**So: a genuinely useful sample in a week. The full corpus never.**

The right shape is therefore not a contiguous backfill. It is **sampled windows**: pick, say,
100 separate one hour windows spread evenly across the two years, and pull every block in each.
That gives roughly 325,000 launches with complete detail, spread across every market regime in
the period, instead of one unrepresentative fortnight. Each one hour window is 8,911 slots.

### How to make it faster, in order of payoff

1. **Turn on gzip.** `Accept-Encoding: gzip`. Verified 3.1x. Free.
2. **Keep concurrency at 4, not 8 or 16.** At 8 and above the endpoint starts cutting responses
   mid stream. Measured: at concurrency 8 on recent blocks, 6 of 16 requests failed with
   incomplete reads. At 16 it got no better. Four in flight with a retry loop is the sweet spot.
3. **Do not bother with `transactionDetails: "accounts"`.** It only saves 28 percent (3.27 MB
   against 4.56 MB on the same block) and it drops `logMessages`, which is where the pump.fun
   events live. Not worth it.
4. **Prefer older windows when you want volume per byte.** 2024 blocks are about 4 MB, 2026
   blocks are 7 to 15 MB. You get roughly three times the chain time per byte in 2024.
5. **Use `encoding: "base64"`.** It is the most compact option. `jsonParsed` is much larger.

---

## 2. Old Faithful directly: the same data, cheaper per byte

If you are willing to build a Rust binary, streaming the CAR archive is more byte efficient
than the JSON RPC, because JSON with base64 is roughly 2.5x the size of the raw archive bytes.

**Measured from the epoch 1020 recap file** (`https://files.old-faithful.net/1020/1020.recap.yaml`),
which is a small YAML file the project publishes per epoch and is a much better source of truth
than estimating:

```
epoch 1020
  transactions        653,188,558
  slots                   431,527
  CAR size      1,083,582,470,456 bytes  (1.084 TB)
  transaction bytes  953,057,067,626     (88 percent of the file)

derived
  bytes per slot          2.51 MB
  transactions per slot  1,513.7
  bytes per transaction   1,459
  chain wide rate         3,747 transactions per second
```

**Index sizes, measured by HTTP HEAD on epoch 1020:**

```
epoch-1020.car                                1009.16 GiB
epoch-1020-gsfa.index.tar.zstd                  56.65 GiB
epoch-1020-...-sig-to-cid.index                 23.73 GiB
epoch-1020-...-cid-to-offset-and-size.index     14.51 GiB
epoch-1020-...-sig-exists.index                  4.87 GiB
epoch-1020-...-slot-to-cid.index                 0.016 GiB   (16.8 MB)
epoch-1020-...-slot-to-blocktime.index           0.002 GiB
```

The index filenames contain a content hash. You can look it up: fetch
`https://files.old-faithful.net/<epoch>/epoch-<epoch>.cid`, which returns the hash as plain
text, then build the index filenames from it. Verified working on epoch 1020.

**The important structural fact.** The Old Faithful docs
(https://docs.old-faithful.net/llms-full.txt) show config files where the CAR file and four of
the five indexes are plain `https://files.old-faithful.net/...` URLs. The server reads them
with range requests instead of downloading them. Verified: a range request for bytes 0 to 63 of
the remote `slot-to-cid` index returned HTTP 206 with `accept-ranges: bytes` and
`content-range: bytes 0-63/16830376`. A range request for 128 KB from the middle of the 1.08 TB
CAR file returned exactly 131,072 bytes.

**So nobody has to download 262 TB.** That number only applies if you insist on reading every
slot. Read a thousand slots and you pay for a thousand slots.

The one exception: the `gsfa` index, the address to signature map, is documented as local only.
"provide a local file system path to an unpacked `gsfa` index folder". At 56.65 GB compressed
per epoch, times 362 epochs, that is 20.5 TB, so building your own address lookup is off the
table. Use the public RPC's `getSignaturesForAddress` instead, which is already backed by it.

**Jetstreamer takes arbitrary slot ranges, including across epoch boundaries.** From its README
(https://github.com/anza-xyz/jetstreamer):

```
# slots 358560000 through 367631999, which is epoch 830-850 (slot ranges can be cross-epoch!)
JETSTREAMER_THREADS=8 cargo run --release -- 358560000:367631999
```

That is exactly what a sampling strategy needs. It also has `--sequential`, `--buffer-window`
for low memory machines, `--reverse` to backfill newest first, and resume support that prints
the command to restart from the lowest unprocessed slot.

**Rate on this Mac.** Old Faithful served 2.92 MB/s on one stream, 4.44 MB/s across eight. At
an era average of about 1.7 MB per slot across the two year window, that is roughly 2 blocks/s,
about 180,000 blocks a day, about 20 hours of chain per day of pulling. Call it **three times
faster than the public RPC**, at the cost of building Jetstreamer and writing a decode plugin.

Two honest caveats. Old Faithful sits behind Cloudflare with `cf-cache-status: DYNAMIC`, so
every byte comes from origin and there may be a per IP limit. **Could not verify Old Faithful's
throughput ceiling**, because the home line saturated first in every test. The Bunny CDN mirror
referenced in the Filecoin deal examples (`filecoin-car-storage-cdn.b-cdn.net`) returned 404,
so there is no confirmed alternate mirror to fall back on.

---

## 3. The only free path to the full two years: Oracle Cloud Always Free

This is the answer to "zero money, full scale", and it is real, with real risk.

**What Always Free actually gives you now**
(https://docs.oracle.com/en-us/iaas/Content/FreeTier/freetier_topic-Always_Free_Resources.htm):

```
Ampere A1 compute      1,500 OCPU hours and 9,000 GB hours per month
                       = 2 OCPUs and 12 GB of memory running continuously
Block storage          200 GB total, boot and block combined
Object storage         20 GB, plus 50,000 API requests per month
Outbound transfer      10 TB per month
Inbound transfer       not listed as capped or charged
Duration               for the life of the account
```

Network bandwidth for the A1 shape is **1 Gbps per OCPU**, up to 40 Gbps
(https://docs.oracle.com/en-us/iaas/Content/Compute/References/computeshapes.htm). Two OCPUs
means **2 Gbps**, which is 250 MB/s.

Inbound is free. Oracle also removed outbound charges entirely across all regions in February
2026, so the 10 TB ceiling may no longer even apply, though the Always Free doc still lists it.

### The arithmetic

```
NETWORK
  two years of ledger              262 TB   (from TRAINING_DATA_SOURCES.md, 362 epochs)
  at 2 Gbps = 250 MB/s             262e12 / 250e6 = 1,048,000 seconds
                                   = 12.1 days

CPU
  transaction bytes in the window  262 TB x 0.88 = 231 TB
  at 1,459 bytes per transaction   about 158 billion transactions
  Jetstreamer record               2.7M TPS on a 64 core box with 30 Gbps+
  per core                         about 42,200 TPS
  on 2 cores                       about 84,400 TPS
  time                             158e9 / 84,400 = 1,872,000 seconds
                                   = 21.7 days

ALLOWANCE
  1,500 OCPU hours / 2 OCPU        750 hours = 31.25 days per month
  needed                           21.7 days
  verdict                          fits inside one month of Always Free

STORAGE
  output, STS record shape         about 2 KB per launch
  20 million launches              about 40 GB
  allowance                        200 GB block volume
  verdict                          fits

GETTING IT HOME
  40 GB out, against 10 TB free    fits with room to spare
```

**On paper the full two year backfill costs zero dollars and about 22 days.** That is the
honest headline.

### The risks, stated plainly

1. **Getting an A1 instance at all is the real blocker.** "Out of host capacity" on free tier
   Ampere is a long running, widely reported problem. Reports say EU and APAC regions
   (Frankfurt, Singapore, Tokyo) usually provision within minutes while US regions can fail for
   days. Plan on scripting a retry loop against the create API and picking a European region.
2. **Oracle just halved this allowance and did not announce it.** Always Free ARM went from
   4 OCPU and 24 GB to 2 OCPU and 12 GB, effective 15 June 2026, with instances over the new
   limit terminated from **18 August 2026**, which was nine days ago
   (https://www.infoq.com/news/2026/07/oracle-cloud-free-tier-limits/,
   https://news.ycombinator.com/item?id=49183750). They did it with no blog post and no
   customer email. They can do it again. Do not build anything that has to keep running.
3. **A credit card is required to open the account.** It is not charged unless you upgrade, and
   there is usually a small temporary hold that gets released. If "zero money" means "no card
   touches this", Oracle is out and the public RPC is your answer.
4. **Old Faithful's real ceiling is unverified.** The 12.1 day network figure assumes it will
   serve one client at 250 MB/s. If Cloudflare limits per IP to something like 50 MB/s, that
   becomes 60 days, which no longer fits in one month's allowance. **Test this first** with a
   single epoch before committing.
5. **Jetstreamer's 2.7M TPS is on a 64 core box with a 30 Gbps network.** Assuming it scales
   linearly down to 2 cores is an assumption, not a measurement, and a pump.fun decode plugin
   will cost more per transaction than the bundled counting plugins. Budget for the real number
   being half the estimate, which means two months of Always Free instead of one.
6. **Idle instances get reclaimed**, but only if CPU, network and memory are all under 20
   percent over a 7 day window. A job pinning both cores is safe by that rule.

---

## 4. Google BigQuery: the schema is right, the dataset is probably dead

Investigated hard, as asked. The conclusion is negative but for a different reason than cost.

**The free tier is genuinely free, with no credit card.** The BigQuery sandbox
(https://docs.cloud.google.com/bigquery/docs/sandbox) lets you use BigQuery "without providing
a credit card or creating a billing account for your project", with "the same free usage limits
as the BigQuery free tier, including 10 GB of active storage and 1 TB of processed query data
each month". Restrictions: all tables, views and partitions "automatically expire after 60
days", and there is no streaming, no DML statements, and no Data Transfer Service.

Confirmed independently at https://docs.cloud.google.com/free/docs/free-cloud-features:
"1 TiB of querying per month" and "10 GiB of storage per month", and unused limits do not roll
over. Querying a public dataset costs you bytes scanned only, not storage, so the dataset being
public genuinely does mean the querier pays nothing beyond the free tier.

So the money side is fine. The data side is not.

**The dataset id is `bigquery-public-data.crypto_solana_mainnet_us`, and only the Transactions
table is public.** Its schema is the best fit of anything on any list:
`block_slot`, `block_timestamp`, `signature`, `index` (order within the block), `accounts[]`,
`log_messages[]`, `balance_changes[]` with before and after, plus pre and post token balances.
That is all four things STS needs, in SQL, with no decoder to write.

**Freshness: probably stale, could not verify directly.** Evidence gathered:

- The community thread
  (https://discuss.google.dev/t/public-solana-bigquery-dataset-crypto-solana-mainnet-us-stopped-updating-on-march-31-2025/185629)
  runs: 2 April 2025, dataset stopped updating on 31 March 2025. 3 April 2025, a community
  moderator suggests contacting support. 6 April 2025, reporter says it is "finally more up to
  date now". **25 November 2025, a different user reports it delayed again, with the latest
  `block_timestamp` sitting at 19 November 2025.** No Google staff ever replied in the thread.
- The ETL that feeds it, `blockchain-etl/solana-etl`, has not had a commit since
  **27 September 2024**. Verified through the GitHub API: last push 2024-09-27, not archived,
  40 stars.
- The `blockchain-etl/public-datasets` repo that lists it has not had a commit since
  **26 June 2024**, and the Solana entry itself dates from December 2023.
- The maintainers document known gaps of 13,602 missing blocks and 18,879 blocks with missing
  or duplicated transactions in the initial load.

**Could not verify the current state of the table**, because checking `__TABLES__` or the last
`block_timestamp` requires an authenticated Google Cloud login, and no `gcloud`, `bq` or
`gsutil` is installed on this machine. A dead upstream ETL plus two independent staleness
reports plus zero maintainer activity for two years is strong circumstantial evidence, but it
is circumstantial.

**Even if it were healthy, the arithmetic does not close.** BigQuery bills the whole of every
column you reference across the partitions you scan, and `log_messages` is the largest column
in Solana's largest table, holding every vote transaction's logs alongside the ones you want.
Filtering to pump.fun in the WHERE clause does not reduce what gets scanned. One day of chain
is 537 GB in raw archive form and the uncompressed logical form BigQuery bills on is larger, so
a query touching `log_messages` and `balance_changes` for one day plausibly scans 500 GB to
1.5 TB. **This scan size is an estimate, not verified.** On that estimate:

```
free tier                  1 TiB per month
one day of chain           roughly 0.5 to 1.5 TiB scanned
days you can pull free     roughly 1 to 2 per month
two years needed           730 days
time                       365 to 730 months, so 30 to 60 years
```

**Verdict.** Worth ten minutes of your time to log in and run
`SELECT MAX(block_timestamp) FROM bigquery-public-data.crypto_solana_mainnet_us.Transactions`,
because if it is alive it is the only source here that needs no decoder at all and would be an
excellent way to cross check the public RPC path. It is not the corpus under any circumstances,
and the odds are it is two years stale.

---

## 5. Free compute with free inbound bandwidth

The idea being tested: a free machine streams Old Faithful, filters to pump.fun rows, writes a
small output. Here is what each option actually offers.

| Option | Cores | Memory | Network | Inbound | Storage | Verdict |
|---|---|---|---|---|---|---|
| **Oracle Always Free A1** | 2 OCPU | 12 GB | 2 Gbps | free | 200 GB | **The one that works.** Section 3. |
| Oracle Always Free E2 Micro | 1/8 OCPU | 1 GB | 480 Mbps | free | shares the 200 GB | Too slow to decode. Useful as a controller only. |
| Google Cloud Always Free | e2-micro, 0.25 vCPU | 1 GB | shared | free | 30 GB | Dead. One shared quarter of a core, and outbound is capped at **1 GB per month**. |
| Google Cloud free trial | any | any | any | free | any | 300 dollars of credit over 90 days. Requires a card, expires, and is credit not free. |
| AWS free tier | n/a | n/a | n/a | free | n/a | Now credit based: 100 dollars up front plus up to 100 more, over 6 months, and the account closes at 6 months. Credit, not free. |
| Hetzner | 2 vCPU | 4 GB | 1 Gbps | free | 40 GB | About 4 euros a month. Cheap, not free. Out of scope. |
| GitHub Actions | 4 vCPU | 16 GB | fast | free | 14 GB | Technically the strongest. **Against the rules. Do not.** See below. |

**Google Cloud Always Free**, verified at https://docs.cloud.google.com/free/docs/free-cloud-features:
"1 non-preemptible `e2-micro` VM instance per month" in `us-west1`, `us-central1` or
`us-east1`, "30 GB-months standard persistent disk", and "1 GB of outbound data transfer from
North America to all region destinations (excluding China and Australia) per month". Inbound is
free but a quarter of a shared core cannot decode the ledger. Not viable.

**AWS**, from https://aws.amazon.com/free/: new customers get "100 dollars in credits
immediately" and can "earn up to 100 more", over "6 months", after which "the account
automatically closes". This is a trial with credits, not a free tier in the sense being asked
about. Not viable.

### GitHub Actions: works, and you should not do it

The numbers are genuinely the best on this list. Standard GitHub hosted runners are "free: In
public repositories" with no minute cap
(https://docs.github.com/en/billing/managing-billing-for-your-products/about-billing-for-github-actions),
the Free plan allows **20 concurrent jobs**, and each job can run for **6 hours**
(https://docs.github.com/en/actions/reference/limits). That is 120 job hours per wave on 4 vCPU
machines with datacenter bandwidth. A sharded backfill would chew through this in days.

It is also a clear violation of GitHub's Acceptable Use Policies
(https://docs.github.com/en/site-policy/acceptable-use-policies/github-acceptable-use-policies),
which prohibit "automated excessive bulk activity" and usage "significantly excessive in
relation to other users", and reserve the right to "suspend your Account, throttle your file
hosting, or otherwise limit your activity". The Actions runners are for building and testing
the software in the repository, not as free compute for an unrelated data pipeline.

Reporting it because the brief asked for it. The honest recommendation is no. Oracle Always
Free is designed to be used this way and gets you the same result without putting a GitHub
account at risk.

---

## 6. Free API tiers: the arithmetic that ends the discussion

First, the denominator, measured live rather than estimated. One
`getSignaturesForAddress` call on the pump.fun bonding curve program returned 1,000 signatures
spanning **13 slots and 5 seconds of block time**, of which **760 of 1,000 had failed**.

```
pump.fun transaction rate     1,000 / 5 seconds   = 200 per second
                              200 x 86,400        = 17.3 million per day
over two years                17.3M x 730         = 12.6 billion transactions
of which successful           24 percent          = about 3.0 billion
```

Now the tiers, each verified against its own pricing page:

**Helius free** (https://www.helius.dev/pricing): "1M credits" per month, "10 Requests / sec",
archival data "Yes".

```
on the rate limit alone   12.6e9 / 10 per second = 1.26 billion seconds = 40 years
on credits, at 1 per tx   12,600 months = 1,050 years
```

**QuickNode free** (https://www.quicknode.com/pricing): "10M API credits included",
"15 requests/second".

```
on the rate limit alone   12.6e9 / 15 = 840 million seconds = 26.6 years
```

**Everything else is smaller than these two.** Rather than list them one by one, here is the
bar any of them would have to clear. To finish the job in one month you need:

```
12.6 billion transactions / 2.6 million seconds in a month = 4,850 requests per second
```

No free tier on the market is within a factor of a thousand of 4,850 requests per second, and
none offers 12.6 billion credits a month. Alchemy, Shyft, Syndica, Bitquery, Solscan, Birdeye
and Moralis are all far below Helius and QuickNode on both axes, so they all fail by a wider
margin. The earlier doc's per vendor detail stands and does not need redoing.

**The one that wins is the unmetered one.** The reason `api.mainnet-beta.solana.com` beats
every keyed free tier is that it has no credit budget at all. It has only a bandwidth cap, and
a bandwidth cap is the one limit that a batch method like `getBlock` can amortise: one request
returns 1,500 transactions instead of one. Helius at 10 requests per second fetching one
transaction each does 10 transactions per second. The public RPC at 0.6 blocks per second does
about 900. That is a 90x difference, and it is entirely down to fetching whole blocks instead
of individual transactions.

That is also why `getTransaction` is the wrong verb here even on the public endpoint. The 40
requests per 10 second cap means 4 transactions a second. Whole blocks give you 77 pump.fun
transactions per request. Always pull blocks.

---

## 7. Prepared free datasets: a second sweep found nothing bigger

The earlier doc swept these. This was an independent second pass through the HuggingFace and
Kaggle search APIs rather than the web pages, checking actual file sizes through the tree API
rather than trusting the dataset cards. **Nothing larger than what is already on the list
exists.**

New things found that were not in the earlier doc, with measured sizes:

| Dataset | Size | Licence | Verdict |
|---|---|---|---|
| `Tr4m0ryp/trenches-pumpfun-forward-2026-08` (HF), same as `moussaouallaf/trenches-pumpfun-forward-2026-08` (Kaggle) | 105 MB | "Other" | Ready made `features.parquet` and `labels.parquet` plus curve paths and account state. Small, but it is somebody else's feature set for the same problem and worth reading for ideas. |
| `nexacore/solana-dex-data` | 5.16 GB | not checked | Jupiter v6 swaps, not pump.fun. Wrong program. |
| `Pumpdotstudio/pump-fun-sentiment-100k` | 213 MB | not checked | Social text, not on chain. Possibly relevant to the social half of STS, not this half. |
| `gleb270/pump-fun-api-solana-tokens-info` | 320 MB | CC0 | Token metadata from the pump.fun API, no trades. |
| `muhammetakkurt/pump-fun-meme-token-dataset` | 68 MB | not checked | Small CSV. |
| `blackhawkdragon/pumpfun-real-data` | 70 MB | not checked | Mostly one bot's own log files. Not a dataset. |
| `clr3org/solana-pumpfun-trade-events-10min-sample` | 10 MB | MIT | A 10 minute NoLimitNodes teaser. Useful only to inspect their column layout before deciding whether to buy. |
| `pokecrafterz/pumpfun-memecoins` | 0 GB | Unknown | An empty pointer to the Slinky21 HuggingFace set. |

**Re-verified: `btclee/memecoins` was updated on 2026-08-20, seven days ago, but the coverage
did not change.** Still 11.71 GB, MIT, and the description still reads "pumpfun_mints: pumpfun
mints INF 202411 - 202511" with swaps only for 2025-01-01. So the hope that a recent update
extended it to 21 months does not hold. It is still a 13 month mint list and nothing more.

`dremovd/pump-fun-graduation-february-2025` re-verified: 6.70 GB, **CC0 public domain**, 2,926
downloads. It remains the right one to start with, exactly as the earlier doc said.

**Conclusion for this section: two independent sweeps now agree. There is no bigger free
prepared pump.fun dataset. Stop looking for one.** The published sets are worth about five or
six non contiguous months between them, and the only path past that is pulling the chain
yourself, which section 1 says you can now do for free.

---

## 8. Everything else that was checked and does not work

**Torrents, Internet Archive, Arweave, Filecoin.** No free torrent or Internet Archive mirror
of the Solana ledger exists. The Arweave "Solar Bridge" work is from 2020 and covered ledger
transition data, not the full history. Filecoin does hold Old Faithful CAR files and
yellowstone-faithful supports Filecoin retrieval, but it is the same data as
`files.old-faithful.net` at unknown and probably worse throughput. The Bunny CDN mirror
referenced in the Filecoin deal examples returned 404 when tested.

**Public Clickhouse or Dremio endpoints.** None found. Jetstreamer can write to a ClickHouse
you run yourself (`--clickhouse-dsn`), which is useful, but there is no public one holding this
data.

**The GCS ledger buckets.** Unchanged from the earlier doc: `mainnet-beta-ledger-us-ny5` and
`mainnet-beta-ledger-europe-fr2` are requester pays, so the downloader pays roughly 25,000
dollars of egress. Not free by any reading.

**A hosted Old Faithful endpoint from Triton.** Triton's own docs say Old Faithful "is
currently available for use via a separate, dedicated path" but publish no endpoint URL, no
rate limit and no price for it
(https://docs.triton.one/chains/solana/old-faithful-historical-archive-1). The only price on
that page is for the older Hydrant archive at "10.00 dollars per million queries". **Could not
verify** whether a free or trial Old Faithful endpoint exists. It matters less now, because the
Solana Foundation public RPC is already serving the same archive for free.

**`cid.old-faithful.net` is still down.** Both `slot-to-cid` and `sig-to-cid` returned HTTP 522
after 20 seconds. Same as the earlier doc found. Not needed for the recommended path.

**Making extra Dune accounts.** Off the table, as instructed, and it would violate their terms.

---

## 9. Recommendation

**Best zero cost option: pull sampled windows from `https://api.mainnet-beta.solana.com` with
gzip on and four requests in flight.**

What it gives you:

- The complete pump.fun record. Launch events, trade events, native SOL balance deltas for the
  funding graph, slot, and transaction index. Verified on the exact day you have live capture
  for.
- Any date in the two year window, and in fact any date back to genesis.
- Zero money, zero accounts, zero API keys, zero credit cards. Nothing to sign up for.
- Roughly 10,000 to 19,000 launches per day of unattended pulling.

What it cannot give you:

- The full two year corpus. That is 2 to 4 years of pulling at this rate and it is not going to
  happen on this connection.
- Sub second timing. The `blockTime` is whole seconds, same as every other source. Use
  `(slot, tx_index)` as the ordering key, exactly as `TRAINING_DATA_SOURCES.md` recommends. That
  recommendation is unchanged and still needs doing first.

**If you want the full two years for zero money**, Oracle Cloud Always Free is the only path,
at 2 cores, 12 GB, 2 Gbps and free inbound, running Jetstreamer against Old Faithful for about
22 days inside one month's allowance. Real risk on capacity, on Oracle changing the terms again
without notice, and on unverified throughput. Prove the throughput on one epoch before
committing to it, and note that it needs a credit card on file even though it is never charged.

**The order of work does not change from the earlier doc.** Fix the window definition from 3
seconds to 8 slots first. Then build the decoder. The difference this doc makes is that you no
longer need to rent anything to feed the decoder real data, and you no longer need to spend 40
euros or 200 dollars to find out whether the pipeline works.

---

## First step, tonight

**Write a script that pulls one hour of blocks from 2026-08-20 off the public RPC and rebuilds
the coins in `data/coins-2026-08-20.jsonl`.**

That one step proves the whole path, and everything after it is the same code at a different
scale. Concretely:

```
endpoint     https://api.mainnet-beta.solana.com
method       getBlock
params       {"maxSupportedTransactionVersion": 0,
              "transactionDetails": "full",
              "rewards": false,
              "encoding": "base64"}
headers      content-type: application/json
             Accept-Encoding: gzip          <- do not skip this, it is 3.1x
concurrency  4 in flight, with a retry loop for incomplete reads
slots        440,486,139 upward, which is 2026-08-20 13:59:37 UTC
             one hour is 8,911 slots
```

For each block, keep only transactions whose `meta.logMessages` mention
`6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P` or
`pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`, drop the ones with a non null `meta.err` (about
**11 percent** of them in a whole-block pull, not the 76 percent quoted earlier in this
document, which is the signature-index population), decode the `Program data:` base64 lines into `CreateEvent` and
`TradeEvent`, and read the funding edges off `preBalances` and `postBalances`.

Expect roughly 35 pump.fun transactions per block, of which about 33 succeed and carry a
`Program data:` line, and about 1 block in 30 containing a `Create`. That is what was measured
on slot 440,486,139.

Then diff the result against the 5,392 coins captured live that day. Wallet ordering inside a
slot will not match the live `at` offsets, and that is expected and is the whole reason for
moving the window to slots.

Two things to do in parallel, both cheap:

1. **Ten minutes in the BigQuery sandbox.** No credit card needed. Run
   `SELECT MAX(block_timestamp) FROM bigquery-public-data.crypto_solana_mainnet_us.Transactions`.
   If it comes back with a recent date, you have a free SQL cross check with no decoder, which
   is worth having. If it comes back November 2025 or March 2025, cross it off permanently.
2. **Try to create an Oracle Always Free A1 instance in Frankfurt.** Not to use yet, just to
   find out whether you can get one at all, because that single unknown decides whether the
   full two year corpus is reachable for free or not. If it provisions, run one epoch through
   Jetstreamer on it and measure the real download rate. That number is the last unverified
   thing in this document.

---

## Sources

Measured live in this session, not read off a page:

- `https://api.mainnet-beta.solana.com` archival depth, block contents, gzip behaviour,
  sustained throughput, and rate limit behaviour
- `https://files.old-faithful.net/1020/1020.recap.yaml` for exact epoch statistics
- `https://files.old-faithful.net/1020/epoch-1020.cid` and the index files for sizes and range
  request support
- pump.fun transaction density and failure rate via `getSignaturesForAddress`
- HuggingFace and Kaggle dataset APIs for file sizes and licences

Pages fetched:

- Solana public RPC limits: https://solana.com/docs/references/clusters
- Old Faithful docs: https://docs.old-faithful.net/llms-full.txt and https://docs.old-faithful.net/references/of1-files
- Jetstreamer: https://github.com/anza-xyz/jetstreamer
- BigQuery sandbox: https://docs.cloud.google.com/bigquery/docs/sandbox
- Google Cloud free tier: https://docs.cloud.google.com/free/docs/free-cloud-features
- BigQuery Solana staleness thread: https://discuss.google.dev/t/public-solana-bigquery-dataset-crypto-solana-mainnet-us-stopped-updating-on-march-31-2025/185629
- Oracle Always Free resources: https://docs.oracle.com/en-us/iaas/Content/FreeTier/freetier_topic-Always_Free_Resources.htm
- Oracle compute shapes and network bandwidth: https://docs.oracle.com/en-us/iaas/Content/Compute/References/computeshapes.htm
- Oracle free tier cut: https://www.infoq.com/news/2026/07/oracle-cloud-free-tier-limits/ and https://news.ycombinator.com/item?id=49183750
- AWS free tier: https://aws.amazon.com/free/
- GitHub Actions billing: https://docs.github.com/en/billing/managing-billing-for-your-products/about-billing-for-github-actions
- GitHub Actions limits: https://docs.github.com/en/actions/reference/limits
- GitHub Acceptable Use Policies: https://docs.github.com/en/site-policy/acceptable-use-policies/github-acceptable-use-policies
- Helius pricing: https://www.helius.dev/pricing
- QuickNode pricing: https://www.quicknode.com/pricing
- Triton archival access: https://docs.triton.one/chains/solana/old-faithful-historical-archive-1
- blockchain-etl repos, via the GitHub API, for last commit dates

Could not verify:

- Current freshness of `bigquery-public-data.crypto_solana_mainnet_us`, needs an authenticated
  Google login
- Old Faithful's per client throughput ceiling, the home line saturated first in every test
- BigQuery bytes scanned per day of Solana, estimated from archive size, not measured
- Whether a free or trial hosted Old Faithful RPC exists from Triton
- Oracle Ampere A1 free tier capacity availability in any specific region right now
