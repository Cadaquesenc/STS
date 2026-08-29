# STS risk governor and Sybil forensics

> **STATUS NOTE — 27 August 2026.** This document reads as a description of live
> behaviour. Two of its mechanisms are specified here and do not exist in
> `src-tauri/src/`. See [`../VERDICT-2026-08-27.md`](../VERDICT-2026-08-27.md).
>
> - **The two-provider quorum is not in force.** §3.6.2 describes the roadmap's
>   Phase 1 rule — critical facts need two consistent providers or they are
>   UNKNOWN — in the past tense, as something already applied. It was never
>   implemented, and no second provider is configured, so it could not run if it
>   were. Every critical fact in the running system is single-source.
> - **The stop in §12.5 does not exist.** There is no stop, no target, no trailing
>   stop and no time exit anywhere in `src-tauri/src/`; the daemon's only exit is
>   to flatten at the end of the run. Separately, when the equivalent rules were
>   actually tested, the best of 108 exit rules on real second-by-second price
>   paths returned **−2.32%**, and at a fee of zero the best realizable rule still
>   loses 0.86% a trade — so this is not simply an unwritten piece of a working
>   design.
>
> The forensics themselves (clustering, funding graph, entropy, the governor's
> structure, and the rule that nothing may ever stop an exit) are intact and worth
> keeping. What they cannot do is pick winners: every signal in this system sorts
> coins by how much they will *move*, not by which way.

This is the canonical description of two things: how the engine decides that a
group of wallets is one hand rather than a crowd, and how it decides whether it
is allowed to open a position at all. They live in one document because they are
one decision — the forensics produce numbers, the governor turns numbers into a
yes or a no, and the same rule binds both: nothing here may ever stop an exit.

Everything below is either a formula with its exact arithmetic, a parameter with
its default and where the default lives, or an invariant with the test that
proves it. Where a number is a policy choice it says so, and policy choices are
versioned and belong in configuration, not in the code that computes the metric.

## A note on phase numbering

`STS_ROADMAP.md` numbers Phase 2 "Dual-Speed Risk & Feature Pipeline" and Phase 3
"Deterministic Replay & Out-of-Sample Backtesting". The work this document
specifies — the forensic cluster metrics and the risk governor — is the
roadmap's Phase 2 deliverable set, and it is currently being tracked as Phase 3
in the build sequence. This document is written against the roadmap's Phase 2
acceptance criteria. It is worth reconciling the two numbering schemes before
the gate dossier is written, because a gate record that cites the wrong phase is
a gate record nobody can check.

## Conventions

**Money is lamports in unsigned 64-bit integers.** Every intermediate product in
a concentration or sizing calculation widens to `u128` before multiplying and
narrows afterwards. This is not defensive habit: `high_water × 10_000` overflows
`u64` at balances the engine will genuinely see, and an overflow in a drawdown
calculation reports a ruined account as a healthy one.

**Fractions are basis points in integers, or unit floats in `f32`, and never
both.** A ratio that is a policy limit or a concentration is an integer in basis
points — 10 000 is 100%. A ratio that came out of an eigen-solver or an entropy
sum is an `f32` in `[0, 1]`. `SybilClusterMetrics` follows exactly this split:
`holding_hhi_bps` is a `u16`, the other three are `f32`.

**Time is epoch milliseconds in signed 64-bit integers**, one clock, from
`telemetry::now_ms()`. Windows and half-lives in this document are written in
human units and converted once, at configuration load.

**UNKNOWN is a value, not a zero.** Every metric here has at least one input
state where the honest answer is "no measurement". In all of those cases the
function returns `None` and the caller treats it as UNKNOWN. It never returns a
neutral-looking number. A zero HHI means "perfectly spread out", which is the
single most dangerous thing an unmeasured token could be mistaken for.

**Partial evidence may raise risk and may never lower it.** Every bounded
traversal in Part I can run out of budget. A truncated result is allowed to
block an entry. It is never allowed to clear one. This asymmetry is what makes
it safe to put hard budgets on forensic work.

**Determinism is a requirement, not a quality.** The roadmap's replay gate needs
byte-identical decisions across two runs of the same fixture. Anything in this
document that iterates, converges, or sums floating point does so with a fixed
starting vector, a fixed iteration cap, a fixed summation order, and a rounding
step before the value is stored. Section 7 gives the rules.

---

# Part I — Forensic Sybil cluster metrics

The output of this part is one row in `clusters` per cluster per version, with
four measurements and one flag. The measurements are evidence. The flag is a
threshold applied to the evidence, it belongs to the EV engine, and nothing
downstream may treat it as evidence on its own.

## 1. The population being measured

Two different populations get measured with the same arithmetic, and confusing
them is the most likely source of a wrong number in this system.

**The token holder population** is every account holding the mint, after
exclusions. Its concentration is an input to the entry gate: a token where one
non-protocol wallet holds a third of the supply is a rejection regardless of what
any cluster looks like.

**The cluster wallet population** is the wallets inside one identified cluster.
Its concentration says how the cluster's own holdings are split between its
members — whether it is one funder with forty empty puppets, or forty wallets
that genuinely each hold something. This is what `clusters.hhi` stores, per the
schema.

### Exclusions

Before the token holder population is counted, these are removed:

- The bonding curve account and, after migration, the pool's token vaults.
- The burn address and any account provably burned to.
- Program-owned accounts and PDAs belonging to the protocol.
- Locked, vesting, and escrow accounts where the lock can be read on chain.
- Known centralised-exchange hot wallets, from the versioned address list.

The exclusion list carries a version number, and that version is recorded next
to any metric computed with it. A metric computed under list v7 and a threshold
tuned under list v9 is not a comparison, and without the version stamped there is
no way to notice.

**An account that cannot be classified stays in the population.** This is the
conservative direction and it is worth being explicit about why: including an
unclassified account can only make concentration look worse, which can only
block an entry. Excluding it could make a controlled supply look distributed. The
count of unclassified accounts is reported alongside the metric so that a number
resting on a lot of guesswork can be seen for what it is.

Dust is not excluded. A thousand accounts holding a lamport each barely move the
HHI, and excluding them would change the denominator, which moves every other
share. They do inflate the raw holder count, which is exactly why effective
holder count is reported next to it (section 2.3).

## 2. Concentration

### 2.1 The Herfindahl-Hirschman index

Let `h_i` be the balance of holder `i` in the population, and `H = Σ h_i`. The
share of holder `i` is `p_i = h_i / H`. The index is:

```text
HHI = Σ_i (100 × p_i)^2  =  10_000 × Σ_i p_i^2
```

Which is the same number in two forms, and the second form is why the column is
in basis points. `Σ p_i²` runs from `1/N` (everyone equal) to `1` (one holder
has everything), so `HHI_bps` runs from `10_000/N` to `10_000` and fits a `u16`
exactly, with no scaling decision left to the caller.

Read it as: **the share the average lamport's owner holds.** At 10 000 one wallet
owns the token. At 2 500 the supply behaves like four equal holders. At 100 it
behaves like a hundred. That reading is exact — `10_000 / HHI_bps` is the
effective number of equal-sized holders, which is the reciprocal-HHI form of
`N_eff`.

### 2.2 Exact implementation

The index is computed in integers end to end. Floating point here would make two
machines disagree in the last digit, and the last digit is stored, compared and
replayed.

```rust
/// Concentration of `balances` in basis points, or `None` when there is
/// nothing to measure.
///
/// Integer throughout: the balances are scaled to parts-per-trillion shares
/// first, which bounds every square by 10^24 and keeps the whole sum inside a
/// u128 with fourteen orders of magnitude to spare.
fn hhi_bps(balances: &[u64]) -> Option<u16> {
    const SCALE: u128 = 1_000_000_000_000; // parts per trillion

    let total: u128 = balances.iter().map(|&b| b as u128).sum();
    if total == 0 || balances.is_empty() {
        return None; // UNKNOWN. Never Some(0).
    }

    let mut sum_sq: u128 = 0;
    for &b in balances {
        let q = (b as u128) * SCALE / total; // share, parts per trillion
        sum_sq += q * q;                     // <= 10^24
    }

    // sum_sq / SCALE^2 is the sum of squared shares; times 10_000 is bps.
    // The half-denominator addend rounds to nearest rather than truncating.
    let bps = (sum_sq * 10_000 + (SCALE * SCALE) / 2) / (SCALE * SCALE);
    Some(bps.min(10_000) as u16)
}
```

Three things this arithmetic is doing deliberately:

`b as u128 * SCALE` is at most `1.8 × 10^19 × 10^12 ≈ 1.8 × 10^31`, comfortably
inside a `u128`. Doing the multiply before the divide keeps the full precision of
the share; doing it the other way round would turn every small holder into zero.

`q * q` is at most `10^24` and `Σ q²` is bounded by `(Σ q)² = 10^24` as well,
because the sum of shares is one. So the accumulator cannot overflow no matter
how many holders there are.

**Rounding is to nearest, not down.** Truncation biases the index downwards,
which is the direction that makes a concentrated token look safer. The addend of
half the denominator costs one instruction and removes a bias that always points
the wrong way. The residual error from truncating each `q` is bounded by
`N × 10⁻¹²` relative, which is under a millionth of a basis point for any holder
count this system will see.

The summation order is the slice order, and **the slice must be sorted before it
is passed in** — by balance descending, ties broken by the account address
ascending. Integer addition is associative so the order does not change the
result here, but the same slice is used for top-K share and for entropy, where
order does matter, and one sort at the boundary is cheaper than three
conventions.

### 2.3 The numbers reported next to it

The HHI alone is a bad summary and is never the only number in front of a
decision. Three more are computed from the same sorted slice:

**Top-K share**, for K in 1, 5, 10:

```text
TopK_bps = 10_000 × Σ_(i=1..K) p_i
```

This is what carries the hard rejection. HHI is dominated by the largest holder,
so a token with one 30% wallet and a genuine crowd behind it and a token with
three 10% wallets can land at similar indices while being different problems.

**Shannon entropy** over the same shares, and its normalised form:

```text
H      = -Σ_i p_i × ln(p_i)          (0 × ln 0 is defined as 0)
H_norm = H / ln(N)                   for N >= 2, else 0
N_eff  = exp(H)
```

`N_eff` is the number of equally-sized holders that would produce the same
entropy. Compared against the raw holder count `N`, it is the dust detector: a
token with 4 000 holders and an `N_eff` of 6 has 4 000 accounts and six owners.

Entropy is computed in `f64` and rounded before storage (section 7.2). The
`0 × ln 0` case is not a limit to be evaluated at runtime — shares of exactly
zero are skipped in the loop, because `ln(0)` is `-inf` and `0 × -inf` is NaN.

**Buyer diversity** over a window `w`, where `v_i` is buy volume attributed to
independent buyer entity `i`:

```text
BDI_w = 1 - Σ_i (v_i / Σ_j v_j)^2
```

This is one minus the HHI of buy flow, on a 0-to-1 scale. The word doing the work
is *entity*: wallets linked by funding or by the metrics in sections 3 to 5 count
once. Computing BDI over raw wallets measures how many keypairs someone
generated, which is free.

### 2.4 Degenerate inputs

| Input | Result | Why not something else |
| --- | --- | --- |
| Empty population | `None` | There is no supply to be concentrated |
| `H = 0` | `None` | Every share is undefined, not equal |
| One holder | `Some(10_000)` | Correct, and the maximum |
| One holder, rest dust | Near 10 000 | Correct; dust does not dilute control |
| Every holder equal, N large | `10_000 / N`, floored at 1 | A `u16` bottoms out at 1 bp, i.e. 10 000 equal holders |
| Unclassified accounts present | Computed, count reported | Included is the safe direction (section 1) |

### 2.5 Thresholds, and where they live

These are the current policy defaults. They are configuration, versioned, and the
metric code does not know them.

| Rule | Default | Effect |
| --- | --- | --- |
| `top1_hard_block_bps` | 2 500 | Hard reject: one non-protocol, non-locked wallet over 25% |
| `top10_hard_block_bps` | 6 000 | Hard reject |
| `hhi_tier_demote_bps` | 2 000 | Tier 1 becomes Tier 2 |
| `hhi_restrict_bps` | 3 500 | Entry restricted to Tier 3 sizing |
| `n_eff_min` | 25 | Below this, no Tier 1 regardless of HHI |
| `unclassified_max` | 3 | More unclassified accounts than this makes the metric UNKNOWN |

The 25% figure is the one inherited directly from doctrine ("a default hard
rejection applies when non-protocol, non-locked concentration exceeds 25%"). The
rest are tuning surface and must be re-derived from the calibration fixture, not
argued about.

## 3. The temporal influence graph

This is the part that answers "who paid for these wallets, and did they all move
at once". It produces `temporal_influence`, and it is the most expensive thing in
Part I, which is why it runs on the async worker and why every loop in it has a
hard budget.

### 3.1 The graph

Vertices are wallets, exchange hot wallets, bridges, programs and token accounts.
A directed edge `u → v` exists when `u` sent SOL or an SPL token to `v`. Each
edge carries:

| Field | Meaning |
| --- | --- |
| `a_e` | Amount, lamports (or token base units) |
| `t_e` | Block time, epoch ms |
| `slot_e` | Slot, for ordering within a millisecond |
| `sig_e` | Transaction signature — the edge's identity and its evidence |
| `asset_e` | Mint, or native SOL |
| `c_e` | Confidence in `[0, 1]` that this edge is a real funding relationship |

`c_e` is below 1 when the transfer is ambiguous: a program-mediated transfer, an
edge reconstructed from a single provider, an edge whose amount is small enough
to be a rent-exemption top-up rather than funding. Its exact assignment is
policy, versioned with the rest.

**Exchange hot wallets, bridges and mixers are absorbing.** A path may end at
one; it may never pass through one. This is not a performance shortcut — an
exchange hot wallet pays out to hundreds of thousands of unrelated people, so
traversing through it links every one of them to every other. The doctrine line
is that transiting a regulated exchange never labels a wallet on its own, and
making these nodes absorbing is how that is enforced structurally rather than
remembered.

### 3.2 The delta-t windows

Three time constraints, all versioned policy:

| Parameter | Default | What it bounds |
| --- | --- | --- |
| `W_lookback` | 72 h | How far before the launch funding edges are considered at all |
| `dt_hop` | 6 h | The largest gap allowed between two consecutive hops on one path |
| `tau_sync` | 5 s | The bandwidth of the buy-synchrony kernel |

A path is only valid if its edges are non-decreasing in `(t_e, slot_e)` — money
moves forward in time — and if each consecutive pair is within `dt_hop`. The
causality rule sounds obvious and is worth enforcing explicitly, because the
graph is built from two providers whose block times can disagree by a few
hundred milliseconds and an unordered path through that disagreement is a path
through nothing.

### 3.3 Path influence

For a root `r` and a wallet `v`, the influence of `r` over `v` along one path `p`
is the product of three factors:

```text
influence(p) = [ Π_(e in p) c_e ] × exp(-lambda × age(p)) × min(1, flow(p) / theta)
                                  × exp(-lambda_hops × (hops(p) - 1))
```

- `Π c_e` — a path is only as good as its weakest link, multiplicatively.
- `age(p) = t_ref - t_last_edge`, in seconds, where `t_ref` is the launch time.
  `lambda = ln(2) / half_life`, default half-life 24 h. Funding from three days
  ago is real but weaker evidence than funding from twenty minutes ago.
- `flow(p) = min_(e in p) a_e` — the bottleneck. You cannot attribute more money
  down a path than its narrowest edge carried. `theta` is the flow at which the
  path counts fully, default 0.1 SOL.
- `lambda_hops = ln(2) / hop_half_life`, default half-life **4 hops**. This is
  the master spec's §Y.2 term, and version 1 of this section omitted it.

**Why the hop term is not optional at the current depth.** Under a four-hop
budget the difference between a one-hop claim and a four-hop one is small enough
that leaving the term out changed little, and version 1 left it out. §3.4's
budget is now twenty-four hops, and at that depth the omission is an exploit:
nothing in the first three factors counts hops, so a twenty-four-hop chain of
unambiguous SOL transfers scores exactly what one direct transfer scores. An
attacker would buy a clean origin for the price of twenty-four keypairs — which
is precisely the shape §Y.1 says the attacker is already building.

With the term, the first hop is undiscounted and every four hops of distance
halve what a path can claim: 1.00 at one hop, 0.59 at four, 0.35 at eight, 0.12
at sixteen, 0.019 at twenty-four. The evidence is found, reported, visible to an
operator, and unable to carry a launch on its own.

Note that the posterior of §3.3 is a **ratio**, so a decay applied to every path
of the same length cancels out of it. The hop term moves short paths relative to
long ones and nothing else, which is the single thing it is for. Distance
discounts the influence; it never changes the identification.

Summing over all paths double-counts, because two paths that share an edge share
the same money. The rule:

```text
I(r -> v) = influence(p*) + kappa × Σ_(p in D) influence(p)
```

where `p*` is the highest-influence path and `D` is the set of paths that are
**edge-disjoint from `p*`**, with `kappa = 0.25` by default. One strong path is
the claim; independent corroborating paths add to it, at a discount, because they
are usually not as independent as they look.

The parent posterior is the normalised influence:

```text
P(parent = r | v) = I(r -> v) / Σ_q I(q -> v)
```

If the denominator is zero — no root reaches `v` inside the windows — the parent
is UNKNOWN. It is not "self-funded" and it is not "clean".

### 3.4 The traversal, and its budgets

The search is a bounded breadth-first expansion backwards from each cluster
wallet toward roots. It is iterative, not recursive: recursion here means an
unbounded stack on data an adversary controls the shape of.

```text
budget := { depth: 24, fanout: 64, nodes: 4096, edges: 32768 }

frontier := { v }, depth 0
visited  := { v }
paths    := []

while frontier is not empty and budget is not exhausted:
    next := empty
    for each node n in frontier, in address order:
        in_edges := edges into n within W_lookback and dt_hop of n's edge
        if in_edges is larger than budget.fanout:
            keep the budget.fanout largest by amount;
            ties broken by signature ascending;
            mark the result truncated
        for each edge e in in_edges, in (amount desc, signature asc) order:
            if e.source is on the current path: skip        # cycle
            if e.source is absorbing: record path, do not expand
            else if depth + 1 <= budget.depth: add to next
            else: mark the result truncated
            decrement budget.edges; decrement budget.nodes on first visit
    frontier := next
```

Every ordering in there is total and deterministic. Address order, then amount
descending, then signature ascending: no two edges can tie on all three, because
a signature is unique. This matters because the fanout cap makes the *order*
decide which edges survive, and a non-deterministic order would make the metric
non-replayable.

**The depth was four and is now twenty-four.** Four was never the depth at which
the money stops moving; it was the depth at which the version 1 influence formula
stopped being safe. §Y.1's attack is built out of fresh keypairs precisely
because each one costs nothing, so the described shape — a CEX withdrawal, an
instant-swap service, three dormant hops, a bridge, the wallet that buys — is
eight or ten hops long before it arrives, and a four-hop traversal answers
`Truncation::Depth` and UNKNOWN for every wallet in it. That is the honest answer
under a four-hop budget and it is not a useful one: the chain is *designed* to sit
one hop past the cap, wherever the cap is.

Two things make the deeper walk safe, and neither is optional:

1. **The depth cap was never what bounded the work.** `nodes` and `edges` are,
   and they are unchanged at 4096 and 32768. Twenty-four spends the same edge
   budget on a longer trail rather than refusing to spend it, so the worst case
   is the same worst case. What moves is which budget binds first, and therefore
   which `Truncation` a bound walk reports.
2. **§3.3's hop term.** Without it, deepening the walk is a gift to the attacker
   rather than a defence against one. See §3.3.

**Budget exhaustion sets `truncated` and does not extend the budget.** A
truncated traversal produces an influence number that is a lower bound — more
search could only find more funding. Per the conventions, a lower bound may block
an entry and may not clear one, so a truncated result is usable when it is above
a threshold and is UNKNOWN when it is below one.

### 3.5 The score

`temporal_influence` combines two things that must both be true for a cluster to
be one hand: the wallets share a funder, and they moved together.

**Funding concentration** — the largest share of the cluster, weighted by buy
volume, that points at a single root:

```text
fund(C) = max_r [ Σ_(v in C) w_v × P(parent = r | v) ] / Σ_(v in C) w_v
```

with `w_v` the buy volume of wallet `v` in the launch window. Weighting by volume
rather than by wallet count is deliberate: forty dust wallets funded by one root
matter less than two large ones, and an unweighted mean is trivially gamed by
generating empty keypairs.

**Buy synchrony** — a kernel over the pairwise gaps between first-buy times:

```text
sync(C) = ( 1 / (|C| × (|C| - 1)) ) × Σ_(i != j) exp( -|t_i - t_j| / tau_sync )
```

One when every wallet bought in the same instant, decaying smoothly to zero as
the buys spread out. It is a mean over ordered pairs, so it is bounded by one
without further normalisation, and it needs no binning — binning creates a
boundary an adversary can straddle.

The stored score is the geometric mean:

```text
temporal_influence = sqrt( sync(C) × fund(C) )
```

Geometric rather than arithmetic because both halves are necessary. Fifty wallets
buying in the same slot with fifty different funders is a bot service with fifty
customers, not one hand. One funder whose wallets bought over four hours is a
person managing positions. Only both together is the thing this metric exists to
find, and an arithmetic mean would score each of the first two at 0.5.

**If `fund(C)` is UNKNOWN or the traversal was truncated below threshold, no row
is written.** The geometric mean would return zero, and zero in this column reads
as "these wallets are unrelated", which is the opposite of what was learned. The
cluster goes to the unresolved queue and the candidate is UNKNOWN to the gate —
which blocks entry and, as always, blocks nothing else.

## 3.6 Verifying the graph against the chain

Sections 3.1 to 3.5 take the edge set as given, and that is the correct split —
it leaves exactly one hole, one section wide. An edge is an *assertion* that some
signature moved some amount between two addresses at some slot. Every number
downstream — the posterior, the cluster, `temporal_influence`, the flag an
operator acts on — inherits the truth of that assertion, and none of them can
test it. Without this section a forensic report is a rigorous derivation from
whatever the message said.

### 3.6.1 Three answers, not two

`verified: bool` is the wrong shape, for the reason UNKNOWN is never a zero
anywhere else here. Three states, licensing different actions:

| Verdict | Meaning | Effect on the edge |
| --- | --- | --- |
| `CONFIRMED` | The quorum served the transaction and it carries this transfer | Stands at the confidence it was asserted with |
| `SINGLE_SOURCE` | Fewer providers found it than the quorum wants | Kept, confidence scaled by `unverified_confidence` |
| `UNVERIFIED` | Nobody could answer | Kept, same discount |
| `ABSENT` | The chain has no transaction under this signature | Dropped |
| `FAILED` | The transaction landed and reverted, so it moved nothing | Dropped |
| `MISMATCHED` | The transaction is on chain and does not carry this transfer | Dropped |
| `SPLIT` | The providers that answered disagree with one another | Dropped |

An unverifiable edge is **kept and discounted, never dropped**. It is still the
best evidence available, and discarding it would clear a wallet by declining to
look at it — the one direction the conventions forbid. A contradicted edge is
dropped *before the graph is assembled*, so that no vertex, no router degree
count and no fan-out cut ever rests on a transfer the chain does not have.

`unverified_confidence` defaults to 0.5. The number is a dial; the shape is the
doctrine. Not 1, because an unchecked assertion is not a checked one. Not 0,
because zero is UNKNOWN rendered as "did not happen".

### 3.6.2 Two providers or UNKNOWN

`quorum` defaults to 2, which is the roadmap's Phase 1 rule — critical facts
require two consistent providers or they are UNKNOWN — applied to the one class
of fact that had escaped it. One provider confirming is `SINGLE_SOURCE`: better
than nothing, not a confirmation, never a clearance. Providers that disagree with
each other are `SPLIT` rather than a tie broken by whoever answered first, and a
split is a **contradiction event**, published separately from the finding and
never folded into it.

Comparison tolerances, all versioned policy:

| Parameter | Default | What it bounds |
| --- | --- | --- |
| `slot_tolerance` | 0 | A transaction lands in exactly one slot; the knob exists for a re-org window |
| `time_tolerance` | 1 s | §3.2's few hundred milliseconds of provider block-time disagreement |
| `amount_tolerance` | 0 | An amount is an integer |

### 3.6.3 What a proof licenses

The same asymmetry §3.4 applies to truncation, for the same reason: **an
unverified lineage may block an entry and may never clear one.** A report may be
used to clear a launch only when nothing was truncated *and* a witness was
supplied *and* every edge came back confirmed. All three are independent and all
three must hold; a perfect proof over a budget-bound traversal still clears
nothing.

A report with no witness at all reports "nothing was checked", which is not the
same as nothing being wrong and is emphatically not a pass.

### 3.6.4 Where the evidence lives

Attestations travel **in the request**, beside the edges they are about, exactly
as the graph does. A verified report is therefore reproducible from the message
that produced it, replayable from a fixture, and provable after the fact. A
verifier that dialled out mid-analysis would make the same report depend on when
it was asked for, which is the property this whole system is built to keep.

Acquiring the attestations is a provider concern and belongs beside the adapters
of Phase 1, where the quota counters and the circuit breakers already are.

## 4. Spectral cluster separation

This answers "do these wallets talk mostly to each other, or are they part of the
wider market". It produces `spectral_separation`.

### 4.1 The graph it runs on

Undirected and weighted, over the cluster `C` and its one-hop neighbourhood
`N(C)`, restricted to the connected component containing `C`. Edge weight is
normalised interaction strength between two wallets:

```text
w_uv = ln(1 + volume_uv / unit) × (1 + count_uv)^(1/2)
```

with `unit` = 0.01 SOL. The log on volume stops one large transfer dominating the
whole spectrum; the square root on count keeps a chatty pair from doing the same.
Both are policy defaults.

Restricting to one component is not cosmetic. The smallest eigenvalue of a
Laplacian is zero once per component, so a graph that happens to contain an
unrelated island reports a zero gap and the cluster scores as perfectly separated
for reasons that have nothing to do with it.

### 4.2 The spectrum

With `W` the weight matrix and `D` the diagonal of weighted degrees:

```text
L_sym = I - D^(-1/2) W D^(-1/2)
```

Its eigenvalues satisfy `0 = mu_1 <= mu_2 <= ... <= mu_n <= 2`. A graph that
splits cleanly into two communities has `mu_2` near zero and a visible jump to
`mu_3`. The score is that jump, normalised:

```text
spectral_separation = clamp( (mu_3 - mu_2) / mu_3 , 0, 1 )   for mu_3 > 0
                    = 0                                       otherwise
```

One when the cluster is a component of its own, falling toward zero as the
cluster dissolves into the surrounding graph. Normalising by `mu_3` makes it
scale-free, so a dense cluster and a sparse one are compared on shape rather than
on volume.

### 4.3 Computing it deterministically

`mu_2` and `mu_3` come from Lanczos iteration with explicit reorthogonalisation
against the known eigenvector of `mu_1`, which for `L_sym` is `D^(1/2) 1` and
does not need to be solved for.

Every source of run-to-run variation is pinned:

| Source | Rule |
| --- | --- |
| Starting vector | Derived from a hash of `cluster_id`, never from an RNG or the clock |
| Iteration count | Fixed cap of 128; convergence checked but never extends it |
| Convergence | Fixed tolerance 1e-9 on the Ritz residual |
| Matrix assembly | Rows in wallet-address order, entries in counterparty-address order |
| Summation | Fixed order; no parallel reduction inside the solver |
| Result | Rounded to four decimal places before storage (section 7.2) |

Hitting the iteration cap without converging marks the result truncated and it is
treated as any other truncated result.

### 4.4 The cheap check that must agree

Conductance of the cut between the cluster and everything else needs no solver
and is exact in one pass over the edges:

```text
phi(C) = cut(C, V \ C) / min( vol(C), vol(V \ C) )
```

Cheeger's inequality bounds the two against each other:

```text
mu_2 / 2  <=  phi  <=  sqrt(2 × mu_2)
```

That relationship is asserted in tests on every fixture. It is the check that
catches an eigen-solver bug, a mis-assembled Laplacian, or a component that was
not actually restricted — all of which are silent failures that produce a
plausible number.

## 5. Interaction entropy and wash trading

### 5.1 The stored score

Let `E_C` be the edges inside the cluster and `w_e` their volumes, with
`p_e = w_e / Σ w`:

```text
H_int      = -Σ_(e in E_C) p_e × ln(p_e)
interaction_entropy = H_int / ln(|E_C|)      for |E_C| >= 2
```

Low means the volume runs through a small number of relationships — the star
shape you get when one funder pays everyone and nobody else interacts. High means
the internal flow is spread across many pairs.

`|E_C| < 2` is not a low-entropy cluster, it is an unmeasurable one. It combines
with `SybilClusterMetrics::is_measurable`, which is the `wallet_count >= 2`
check: **both must hold, and any query feeding a decision must apply both.** A
cluster of two wallets with one edge between them has a defined entropy of zero
that means nothing at all.

### 5.2 What entropy does not catch

Entropy is a shape metric and wash trading has a perfectly ordinary shape. Two
wallets passing the same tokens back and forth produce two well-balanced edges
and a normalised entropy near one. Three metrics cover it, and they are computed
alongside:

**Round-trip ratio.** For each wallet, the volume that came back to it from
inside the cluster:

```text
RT(C) = Σ_(v in C) min( inflow_internal(v), outflow_internal(v) ) / Σ_(v in C) volume(v)
```

Bounded in `[0, 1]` by construction. High means the cluster's flow is
circulating rather than going anywhere.

**Cycle volume share.** Cycles of length up to four are enumerated inside the
cluster only — a cluster is small and bounded, so this is cheap — and each
contributes its bottleneck flow:

```text
CYC(C) = Σ_(cycles) min_(e in cycle) w_e / total_volume(C)
```

Length four covers the shapes that matter: `A→B→A`, `A→B→C→A`, and the
four-wallet ring. Longer rings exist and are left to `RT`, which catches them
without enumeration.

**Self-dealing share of the token's volume.** The number that actually matters
to sizing, because it says how much of the tape is fictional:

```text
wash_share(C, token, window) =
    volume where buyer_cluster == seller_cluster == C  /  total volume in window
```

And the correction it feeds:

```text
effective_volume = observed_volume × (1 - Σ_C wash_share(C, ...))
```

Every downstream number computed from volume — absorption, buyer diversity,
depth quality, the volume half of the liquidity check — uses effective volume.
Using observed volume is how a coordinated cluster manufactures the appearance of
the exact conditions the entry regime is looking for.

## 6. From four numbers to one judgement

The four metrics stay separate in the row. Combining happens in the EV engine,
where the thresholds live and where they can change without rewriting history.

Per-wallet cluster membership is a calibrated logistic over the feature vector
from doctrine:

```text
x = [shared_parent, time_proximity, amount_similarity, fanout_similarity,
     synchronized_entry, instruction_similarity, shared_exit,
     common_counterparty, known_cex_origin]

z = b + Σ_j w_j x_j + Σ_(j<k) w_jk x_j x_k

P_cluster = 1 / (1 + exp(-z))
```

with monotonic constraints where evidence demands them — `shared_parent` may only
increase the score, never decrease it, whatever the fit prefers. Group posterior
is a noisy-OR over independent evidence, corrected where features correlate:

```text
P_group = 1 - Π_i (1 - P_cluster_i)
```

Noisy-OR rather than an average because the evidence is disjunctive: a cluster is
suspicious if *any* strong link holds, and averaging lets a pile of weak
non-evidence dilute one strong link.

`flag_sybil` is `P_group >= flag_threshold`, default 0.80. It exists so the UI
has something to filter on and the audit trail has something to point at. The
policy consequences are graded, not binary:

| Condition | Consequence |
| --- | --- |
| `P_group >= 0.95` with corroborated funding evidence | Hard block |
| `P_group` in `[0.80, 0.95)` | Tier 3 at most; quarantine, no automatic real capital |
| `P_group` in `[0.55, 0.80)` | Tier demotion by one, size reduced |
| `P_group < 0.55` | No cluster-derived restriction |
| Any input UNKNOWN or truncated below threshold | Entry blocked as UNKNOWN, exits unaffected |

Each stored row also carries the model version, the input hash, `produced_at`,
the validity watermark and the count of unknown features — without them a score
cannot be replayed and cannot be audited, and a score that cannot be audited is
an opinion.

## 7. Determinism and degenerate inputs

### 7.1 Degenerate graphs

Every one of these has produced a NaN or an infinity in some implementation of
these formulas, and every one of them is a fixture:

| Input | Required behaviour |
| --- | --- |
| Empty cluster | Not measurable; no row |
| One wallet | `is_measurable()` false; no row |
| Two wallets, no edges | Not measurable; no row |
| Fully disconnected neighbourhood | Component restriction applies; separation from the cluster's own component only |
| Self-loop (wallet sends to itself) | Edge dropped before assembly; it is not an interaction |
| Complete graph, equal weights | `separation` near 0, `entropy` exactly 1 |
| Star: one funder, N leaves | `entropy` near 0, `separation` high, `temporal_influence` high |
| All balances zero | HHI `None` |
| Buy times identical | `sync = 1` exactly, no division by zero |

### 7.2 Float discipline

Computation is `f64`. Storage is `f32`. The step between them is a rounding to
four decimal places:

```rust
fn store(x: f64) -> f32 {
    if !x.is_finite() { return 0.0; }          // and the row is not written
    ((x.clamp(0.0, 1.0) * 10_000.0).round() / 10_000.0) as f32
}
```

Four places is more resolution than any threshold in this document uses, and it
is coarse enough to absorb the last-bit differences that come from a compiler
choosing a fused multiply-add on one build and not another. Two runs of the same
fixture must produce identical rows, and identical means byte-identical.

`SybilClusterMetrics::new` clamps on the way in and `unit()` turns NaN into zero.
That is the last line of defence, not the plan: **a NaN reaching `new` is a bug
that has already happened**, and the row should never have been built. The
`CHECK (x BETWEEN 0.0 AND 1.0)` constraints in `clusters` are the line after
that, and they work on NaN because every comparison against NaN is false, so the
insert fails loudly rather than storing a value that makes every gate answer no.

---

# Part II — The dual-speed risk governor

## 8. Two speeds, and why

The engine has to answer two questions that cannot be answered on the same
clock. "Is this token controlled by one person" takes a graph traversal and
hundreds of milliseconds. "May I open this position, right now, at this size"
has to be answered before the price it is answering about has moved.

The resolution is that the fast path never computes anything. It reads a
snapshot that the slow path already finished, checks a fixed list of conditions
against it, does integer arithmetic, and returns. The slow path publishes new
snapshots atomically. Neither waits for the other, ever.

| | Fast path | Forensic path |
| --- | --- | --- |
| Runs on | The ingest thread, in the event's own turn | Background workers |
| Reads | One immutable snapshot, by atomic pointer load | Chain history, SQLite, the graph |
| Writes | One decision into a preallocated slot | New snapshots, `clusters` rows |
| Budget | p99 under 10 ms end to end | Best effort, with a validity watermark |
| On overrun | Returns UNKNOWN, refuses the entry | Publishes truncated, or does not publish |
| May block an entry | Yes | Yes |
| May block an exit | **No** | **No** |

The last row is the whole design. Everything else is negotiable under load.

## 9. The fast-path invariant

### 9.1 What is being measured

The p99 target is over a precisely defined section, because a latency number
without a defined section is not a measurement. **The measured section runs from
the moment the ingest worker dequeues the event to the moment the decision is
written into the publication ring.** It includes snapshot acquisition, every gate
check, sizing, the emergency-route lookup and the audit record build. It excludes
the network, the queue wait before the dequeue, and everything downstream of the
ring.

Sub-budgets, all p99, measured on the Phase 2 fixture on the target MacBook:

| Step | Budget | What it is |
| --- | --- | --- |
| Snapshot acquire | 1 µs | One atomic load of a pointer; no lock |
| Freshness and provenance | 2 µs | Integer comparisons against watermarks |
| Hard invariants | 5 µs | The ordered predicate list, short-circuiting |
| Sizing | 10 µs | `u128` arithmetic, a fixed chain of minimums |
| Emergency route lookup | 2 µs | Index into a preallocated table |
| Audit record | 30 µs | Fill a preallocated buffer; no allocation, no serialisation |
| **Compute total** | **50 µs** | |
| **End-to-end p99 target** | **10 ms** | The roadmap gate |

The two-hundred-fold gap between the compute budget and the gate is headroom, and
it is stated rather than quietly consumed. A change that moves compute from 50 µs
to 500 µs still passes the gate and is still a regression; the sub-budgets are
what make that visible.

### 9.2 The allowlist

The fast path may do exactly these things:

- Load one immutable snapshot through an atomic pointer.
- Integer arithmetic, including `u128` widening.
- Comparisons on `f32` scores already in the snapshot.
- Read from preallocated, fixed-size tables by index.
- Write into a preallocated ring slot.
- Loops whose bound is a compile-time constant.

### 9.3 The forbidden list

These must be **structurally impossible**, not merely absent from the current
code. Absence is a property of today's code; impossibility is a property that
survives the next person editing it.

| Forbidden | How it is prevented |
| --- | --- |
| Heap allocation | Allocator guard (below) counts and, in debug, panics |
| SQLite access | The fast path holds no connection and no handle to get one |
| JSON or any serialisation | The audit record is a fixed struct written into a fixed buffer |
| Graph traversal or recursion | No graph is reachable from the snapshot type |
| Waiting on the forensic worker | The snapshot is a value; there is no channel to await |
| Blocking locks | Lock-free reads only; a `try_lock` that fails is UNKNOWN, not a wait |
| Syscalls, file logging, DNS | No such handles in scope |
| Floating-point money | Lamports are `u64`; `f32` appears only in scores |
| Unbounded loops | Every loop bound is a constant or a `u16` field with a hard cap |

The allocator guard is a thread-local flag set on entry to the measured section
and a global allocator wrapper that, when the flag is set, increments a counter
and — in debug and test builds — panics with a backtrace. The counter is asserted
zero by the `fast_path` benchmark. This catches the allocation that arrives three
refactors from now inside a helper nobody thought about, which is the only kind
that ever gets in.

### 9.4 The gate, in order

The order is not stylistic. Cheapest and most fatal first, short-circuiting, so
that a halted engine does no work at all and a healthy one pays for the checks it
actually needs.

```text
1.  mode.allows_new_entries()                        # Halted stops here
2.  !circuit_breaker.blocks_entries_at(now_ms)       # a trip stops here
3.  open_positions < max_open_positions
4.  drawdown_bps < max_drawdown_bps
5.  snapshot age <= freshness budget, slot lag <= tolerance
6.  provider quorum holds for every critical fact
7.  liquidity.admits_entry(pool_lamports)
8.  forensic verdict is not a hard block, and not UNKNOWN
9.  size := sizing chain (section 10); size >= min_notional
10. an emergency exit route exists, is simulated, and is inside its validity
11. stressed EV lower confidence bound > 0
```

Steps 1 to 4 are `RiskSnapshot::entries_allowed()` today, and its shape is the
shape of the whole gate: every clause is a reason to say no, and there is
deliberately no clause that can say yes on its own.

Steps 5 to 11 are the remainder and they sit in the same function, against the
same snapshot value, passed by value so that a decision cannot be made against
numbers that changed halfway through making it.

**Step 10 comes before step 11 on purpose.** Checking whether a trade is
profitable before checking whether it can be got out of gets the priority exactly
backwards. A position with no exit route is not a trade with a bad expected
value, it is not a trade.

### 9.5 Freshness

| Fact | Budget | On breach |
| --- | --- | --- |
| Price, liquidity, curve progress | 400 ms | UNKNOWN, entry blocked |
| Slot lag | 2 slots | UNKNOWN, entry blocked |
| Cluster metrics | 15 min validity | UNKNOWN, entry blocked, Tier capped |
| Provider health window | 60 s | Degrade a tier |
| Emergency route simulation | 5 s | Re-simulate before use; never entry |

Every budget here blocks entries. **None of them blocks an exit.** A stale price
means the engine does not know what a position is worth, which is a reason to
leave, not a reason to stay.

### 9.6 Budget overflow

If any step would exceed its budget — the snapshot is being swapped, a table
lookup misses, a `try_lock` fails — the fast path returns bounded UNKNOWN
immediately and refuses the entry. **It never expands work inline** and never
extends its own deadline. Overruns are counted, and a sustained overrun rate is
itself a degradation trigger into `RESTRICTED_ENTRY`.

The fast path may only narrow what the slow path permits. `FastPathGate` is a
further restriction on top of everything else, which is exactly what
`RiskSnapshot::fast_path_allowed` encodes: `entries_allowed() &&
fast_path.admits(notional)`. There is no path by which taking the fast route
allows something the slow route would have refused. `FastPathGate::CLOSED` is the
value it starts from and the value it falls back to.

## 10. Sizing

Size is the minimum of a chain of caps, computed in integers, in this order:

```text
risk_budget_size = risk_budget_lamports × 10_000 / stressed_loss_bps
pool_cap         = liquidity.max_position_lamports(pool_lamports)
gate_cap         = fast_path.max_notional_lamports          (fast route only)
operator_cap     = operator_max_notional_lamports
equity_cap       = free_equity_lamports

base = min(risk_budget_size, pool_cap, gate_cap, operator_cap, equity_cap)
size = base × tier_multiplier_bps / 10_000
```

`stressed_loss_bps` is the **worst** modelled loss across the stress set, not the
expected one: the −30% and −50% gap buckets and the 10/15/20/25% slippage
buckets, each simulated against the current depth. Sizing off an expected loss
sizes for the day that does not need risk control.

`max_pool_share_bps` defaults to 150 — the 1.5% executable-liquidity cap from
doctrine. `LiquidityThresholds::max_position_lamports` already computes it in
`u128` and saturates, so a pool of zero yields a size of zero rather than a
panic.

Tier multipliers, from the confidence tiers:

| Tier | Confidence | Multiplier | Notes |
| --- | --- | --- | --- |
| 1 | >= 0.85 | 10 000 | All hard invariants pass, no material unknowns |
| 2 | 0.70 – 0.849 | 5 000 | Limited soft uncertainty or latency degradation |
| 3 | 0.55 – 0.699 | 1 000 | Operator-confirmed or paper only; never automatic real capital |
| — | < 0.55 | 0 | Observe only |

A tier can never override a hard block. Tiering reduces size; it does not grant
permission.

Two floors after the chain: `size >= min_notional_lamports` (default 0.01 SOL —
below it the round-trip cost is most of the trade) and, at Gate 6D,
`size <= 0.05 SOL` regardless of what anything above computed.

No averaging down. A second entry into a position already open is not a size
calculation, it is a different decision, and this gate does not make it.

## 11. Circuit breakers

`CircuitBreaker` is `Clear` or `Tripped { reason, at_ms, clears_at_ms }`.
`clears_at_ms: None` means it does not lift on its own — a person has to look at
what happened first. That is the correct default for anything that tripped
because the engine was losing money, and it is why `trip_hard` and `trip_until`
are separate constructors rather than one with an optional argument.

`blocks_entries_at(now_ms)` is the only question anything asks. A cool-off that
has run out stops blocking by itself; a hard trip never does, however long ago it
was.

### 11.1 Parameters

| Breaker | `BreakerReason` | Trips when | Clears |
| --- | --- | --- | --- |
| Drawdown | `Drawdown` | `drawdown_bps >= max_drawdown_bps`, default 1 500 | Hard — operator only |
| Daily loss | `Drawdown` | Realised loss in a rolling 24 h >= 800 bps of the window's opening equity | Hard |
| Losing streak | `LosingStreak` | 4 consecutive closed losers | Hard |
| Slippage spike | `SlippageSpike` | Median realised slippage over the last 5 fills >= 2× quoted, or any single fill over `max_slippage_bps` | Cool-off 15 min |
| Volatility | *(no variant yet — see 16)* | Realised vol over 5 min of 15 s returns >= 3× the session baseline, **and** effective depth has fallen by 30%+ | Cool-off 10 min |
| RPC degraded | `RpcDegraded` | p95 > 500 ms for two consecutive 60 s windows, or quorum disagreement on a critical fact, or slot lag beyond tolerance | Auto after two healthy windows |
| Kill switch | `KillSwitch` | Pulled, or the process panicked and restarted | Hard |
| Operator | `Operator` | Somebody stopped it | Hard |

`max_open_positions` is not a breaker but sits with them because it is the same
kind of limit:

| Mode | Default | Why |
| --- | --- | --- |
| Gate 6D micro-live | 1 | Doctrine: one position at a time initially |
| Paper | 3 | Enough to exercise concurrent management |
| Replay | Per fixture | It is part of the fixture, not the config |

With two correlation rules on top: at most one open position per creator cluster,
and at most one per correlated cohort, where cohorts come from the same forensic
pass. Two positions in two tokens launched by one hand is one position with extra
steps.

### 11.2 Drawdown, exactly

```rust
pub fn drawdown_bps(equity_lamports: u64, high_water_lamports: u64) -> u16
```

Measured from the high-water mark, never from the session open — an engine that
resets its reference every morning never registers a drawdown. The multiply is in
`u128` because `high_water × 10_000` overflows `u64` at balances that will
occur, and an overflow here reports a ruined account as a healthy one. Equity
above the high-water mark is zero drawdown, not a negative one, and zero over
zero is zero rather than a division fault.

`RiskSnapshot::with_recomputed_drawdown` exists so that a snapshot cannot be
built claiming a drawdown its own two balances disagree with. **Every snapshot
that reaches the gate must have been through it.** A hand-assembled snapshot with
a stale `drawdown_bps` is a gate that passes on a number nobody computed.

### 11.3 Hysteresis

A breaker that flaps is worse than one that stays tripped, because it produces
entries at exactly the moments conditions are unstable. Three rules:

- An auto-clearing breaker needs **two consecutive healthy windows**, not one
  good sample. One fast response after a bad minute is noise.
- For one window after clearing, the trip threshold is tightened by 25%. If it
  trips again inside that window it converts to a hard trip.
- Degradation is stepwise — `NORMAL → RESTRICTED_ENTRY → EXIT_ONLY → HALTED` —
  and recovery is stepwise too, one level per healthy window. Nothing jumps from
  `EXIT_ONLY` back to `NORMAL`.

### 11.4 What a trip does and does not do

A tripped breaker refuses new entries. That is its entire effect. It does not
close positions, it does not cancel the exit ladder, it does not stop
reconciliation, it does not silence telemetry, and it does not stop the audit
writer. `CircuitBreaker::blocks_entries_at` is named for the only thing it is
allowed to block, and no code may call `is_tripped()` to decide whether to
permit an exit.

## 12. The liveness invariant

### 12.1 Statement

**For every reachable state of the system, every open position has at least one
available risk-reducing action, and the path to executing it depends on nothing
that can be unavailable.**

"Every reachable state" means the full cross product: every operating mode, every
breaker reason and clearing state, every drawdown from zero to total, position
counts at and over the cap, every provider health combination, every storage
state including a full disk and a failed WAL checkpoint, and a frozen UI.

Three obligations follow, and each has a test that is not allowed to be skipped.

### 12.2 L1 — Permission

`RiskSnapshot::exits_allowed()` is a `const fn` that returns `true`. It takes
`&self` and ignores it.

This is a function rather than an omission on purpose. Closing a position goes
through the same call as opening one, and this is the call that says the answer
is not up for discussion — not when halted, not when the breaker is tripped, not
at full drawdown. **A limit that can trap the engine in a position is not a risk
control, it is the risk.**

The proof obligation is a property test over the full cross product of
`OperatingMode`, `CircuitBreaker` (both variants, every reason, cleared and
uncleared, before and after `clears_at_ms`), `drawdown_bps` in `0..=10_000`,
`open_positions` at zero, at the cap, and above it. `exits_allowed()` is true in
every cell. The test enumerates rather than samples, because the cell that fails
will be the one a sampler skipped.

The corresponding structural fact in the state machine is that
`(from, Aborted) if from.is_active()` is the only unconditional edge in
`can_transition_to`. Every other edge is a specific pair. Abort is available from
every running state, always, and it is the only thing that is.

### 12.3 L2 — Route

Permission without a route is a promise nobody can keep.

Every position in `Sent` or `Confirmed` carries a precomputed exit route: exact
accounts, an exact path, a slippage bound, and a simulation timestamp. It is
refreshed on a timer and carries a validity watermark. When the watermark
expires, the position is re-routed. It is never left with permission and no
route.

When no executable route exists — the pool is depleted, every path fails
simulation — that is an alarm, not a quiet hold. The engine emits
`no_executable_exit` with the estimated no-exit exposure in lamports, escalates
to the operator emergency controls, and keeps retrying with bounded tips while
preserving the exact slippage and route limits. **It never reports a stop as
filled when it was not.**

`LiquidityThresholds` carries two floors rather than one for this reason:
`min_pool_lamports` is "too thin to enter" and `exit_only_below_lamports` is "too
thin to still be here", and the second is set strictly below the first so the
engine is never entering and exiting the same pool on the same tick.

### 12.4 L3 — Reachability

The exit path may not share a required dependency with the entry path beyond the
signer and one working RPC endpoint. Specifically it must not require:

| Dependency | Why exits cannot need it |
| --- | --- |
| The forensic worker | It is best-effort by design and can be minutes behind |
| Cluster metrics or any Part I output | They can legitimately be UNKNOWN |
| Provider quorum | Quorum is for critical facts about *new* exposure |
| The UI | It is a projection and it can freeze |
| A SQLite write | The audit falls back to buffered NDJSON; the exit does not wait for a commit |
| A healthy disk | Same |
| A fresh price | The exit uses the last valid bounded route and conservative limits |

Degraded data widens the exit's limits conservatively rather than blocking it.
The doctrine sentence this implements: stale, missing, contradictory or UNKNOWN
data never blocks exits, stop-losses, reductions, reconciliation, or kill-switch
actions. **Only new exposure may be blocked or reduced.**

### 12.5 The stop itself

The per-position stop, from doctrine:

```text
StopDistance = clamp( k_ATR × ATR_p + k_void × V_void, Stop_min, Stop_max(D_score) )
```

with defaults `k_ATR = 2.5`, `k_void = 0.5`, `Stop_min = 800 bps`, and
`Stop_max` scaling from 3 500 bps down as executable-depth quality falls. The
stop is additionally bounded by the strategy loss cap and may not be placed
inside ordinary microstructure noise — a stop that sits inside the spread is a
guaranteed exit at the worst available price.

A stop is invalid if its route cannot be simulated or if projected impact exceeds
policy. An invalid stop does not mean no stop: it means the position is marked
emergency and escalated under L2.

## 13. Unwind obligations on aborted fills

This is where the liveness invariant meets the fact that a transaction cannot be
recalled, and it is the most careful part of the execution path.

### 13.1 What abort actually does

`ExecutionState::abort` succeeds from every active state and returns an
`AbortOutcome` whose `needs_unwind` is `self.has_money_at_risk()` — true exactly
in `Sent` and `Confirmed`.

Aborting does **not** sell anything. There is no transaction that un-sends
another one. It stops the engine managing the position, and something is left on
chain that still has to be flattened. `needs_unwind` is how that gets noticed
instead of being discovered later in a balance.

| Aborted from | `needs_unwind` | What is actually out there |
| --- | --- | --- |
| `IntentCreated` | false | A plan. Nothing. |
| `Validated` | false | A plan the gate approved. Still nothing. |
| `Sent` | **true** | A transaction on the network with an unknown outcome |
| `Confirmed` | **true** | A position |
| `Completed` / `Aborted` | — | `AlreadyTerminal`; aborting a finished execution would rewrite history |

The `Sent` case is the subtle one. `needs_unwind` is true, but the obligation is
**conditional and must be reconciled before it is acted on**: the signature is
followed until it lands or its blockhash expires. If it never landed, the
obligation resolves to nothing and is closed with an audit event. If it landed,
there is a position, and it is flattened. Selling a position that does not exist
because an abort assumed the worst is its own incident.

Partial fills follow the same rule against the residual. A bundle that filled `q`
of an intended `Q` leaves an obligation over `q`. Never `Q`.

### 13.2 The rules

**U1 — One source.** `execution_logs.needs_unwind` is written from
`AbortOutcome::needs_unwind` and from nothing else. It is not recomputed at the
call site, not inferred from the abort reason, and not defaulted.

**U2 — Never edited.** The row is history. A resolved obligation is a new intent
and new rows, never an update to the old one. `execution_logs` is append-only and
this is the case that most tempts somebody to make an exception.

**U3 — The unwind goes through the exit gate.** An unwind intent must be
creatable while the breaker is tripped, while the drawdown is at 100%, and while
the mode is `Halted`. It calls `exits_allowed()`, which is always true. **A bug
where the unwind path calls `entries_allowed()` is the single most dangerous bug
in this system** — it produces an engine that refuses to close the positions that
tripped its own breaker, and it looks like correct risk management right up until
it does not. This deserves its own named regression test.

**U4 — Obligations count as exposure.** `open_positions` counts rows in `Sent`
or `Confirmed` **plus** unresolved `needs_unwind` obligations. Counting only
managed positions lets the engine open new ones while it has orphans it has
forgotten about, which is how a one-position limit becomes a three-position
limit.

**U5 — No recovery while obligations are open.** The mode may not step back up
to `NORMAL` while any unresolved obligation exists. Something is on chain that
the engine is not managing; that is not a normal state and should not be labelled
as one.

**U6 — Reconciliation is idempotent.** Keyed on the transaction signature, which
the schema already enforces with a unique partial index. A duplicate receipt, an
unknown confirmation, a restart mid-reconcile, and a provider replaying a
confirmation all converge to the same row set, and none of them increases
exposure.

### 13.3 Restart

On startup, open obligations are rebuilt from `execution_logs` before anything
else runs:

```sql
-- The newest row per intent, restricted to intents that still have money out.
WITH latest AS (
  SELECT intent_id, MAX(seq) AS seq
    FROM execution_logs
   WHERE mode = :mode
   GROUP BY intent_id
)
SELECT e.intent_id, e.seq, e.state, e.mint, e.side, e.size_lamports,
       e.signature, e.needs_unwind
  FROM execution_logs e
  JOIN latest l ON l.intent_id = e.intent_id AND l.seq = e.seq
 WHERE e.state IN ('sent', 'confirmed')
    OR e.needs_unwind = 1;
```

The two arms of that `WHERE` are different obligations and both are needed. An
intent whose newest state is still `sent` or `confirmed` was never finished — the
process died mid-flight. An intent whose newest state is `aborted` with
`needs_unwind = 1` was finished by the engine and left something behind. Neither
is visible from the other's query.

The engine enters `EXIT_ONLY` and stays there until every one of them is
reconciled. It does not open a position before it knows what it already owns.
`execution_logs_unwind` and `execution_logs_open` are the partial indexes that
make this a lookup into a tiny B-tree rather than a scan of everything that has
ever executed, which matters because this query runs on the startup path.

---

# Part III — Acceptance

## 14. Test vectors

These are exact and must be reproduced by any implementation. HHI first, in basis
points, with the rounding rule from section 2.2:

| Population | `Σ p_i²` | `HHI_bps` |
| --- | --- | --- |
| `[100]` | 1.0 | 10 000 |
| `[50, 50]` | 0.5 | 5 000 |
| `[25, 25, 25, 25]` | 0.25 | 2 500 |
| ten equal | 0.1 | 1 000 |
| one hundred equal | 0.01 | 100 |
| `[90, 10]` | 0.82 | 8 200 |
| `[50]` + fifty of `[1]` | 0.255 | 2 550 |
| `[0, 0, 0]` | — | `None` |
| `[]` | — | `None` |

Entropy, to four places:

| Shares | `H` | `H_norm` | `N_eff` |
| --- | --- | --- | --- |
| `[0.5, 0.5]` | 0.6931 | 1.0000 | 2.0000 |
| four equal | 1.3863 | 1.0000 | 4.0000 |
| `[0.9, 0.1]` | 0.3251 | 0.4690 | 1.3841 |
| `[1.0]` | 0.0000 | 0.0000 | 1.0000 |

Synchrony, `tau_sync = 5 s`, buys at 0 s, 0.5 s, 1.0 s:

```text
pairs: exp(-0.1) = 0.9048, exp(-0.2) = 0.8187, exp(-0.1) = 0.9048
sync  = 2 × (0.9048 + 0.8187 + 0.9048) / (3 × 2) = 0.8761
```

Conductance, cluster of five wallets with twenty internal unit-weight edges and
one unit-weight edge leaving:

```text
vol(C) = 2 × 20 + 1 = 41
cut    = 1
phi    = 1 / 41 = 0.0244
```

and Cheeger then requires `mu_2 <= 2 × 0.0244 = 0.0488`, which the solver's
output must satisfy.

Drawdown, from `types.rs` and restated here because it is a gate input:

```text
drawdown_bps(100, 100) = 0
drawdown_bps( 75, 100) = 2 500
drawdown_bps(  0, 100) = 10 000
drawdown_bps(120, 100) = 0        # up on the day, and no underflow
drawdown_bps(  0,   0) = 0
```

## 15. Property obligations

Each of these is a test that must exist and must not be skipped. They are
properties, not examples, and they enumerate where the space is small enough.

| # | Property | Method |
| --- | --- | --- |
| P1 | `exits_allowed()` is true in every reachable state | Full cross product of mode × breaker × drawdown × position count |
| P2 | Abort succeeds from every active state | Enumerate `ExecutionState::ALL` |
| P3 | `needs_unwind` is true from `Sent` and `Confirmed`, false otherwise | Enumerate |
| P4 | Abort from a terminal state returns `AlreadyTerminal` | Enumerate |
| P5 | The unwind path never calls `entries_allowed()` | Named regression test plus a call-graph assertion |
| P6 | `entries_allowed()` is false whenever any single clause is false | Enumerate each clause |
| P7 | No `SybilClusterMetrics` field is ever NaN or outside `[0, 1]` | Fuzz over degenerate graphs (7.1) |
| P8 | Every metric is `None` rather than a neutral number on empty input | Enumerate the table in 2.4 |
| P9 | Two runs of one fixture produce byte-identical `clusters` rows | Replay equivalence |
| P10 | Fast path allocates zero bytes in the measured section | Allocator guard counter, asserted in the bench |
| P11 | Fast path p99 is under 10 ms end to end | `cargo bench --bench fast_path` |
| P12 | Cheeger's inequality holds on every spectral fixture | Assert `mu_2/2 <= phi <= sqrt(2 mu_2)` |
| P13 | `hhi_bps` never overflows or panics on any `u64` input | Property test over random balance vectors including `u64::MAX` |
| P14 | Truncated forensic results never clear a candidate | Assert the asymmetry directly |
| P15 | A tripped breaker changes no exit-side behaviour | Diff exit decisions with the breaker clear and tripped |

## 16. What the schema does not carry yet

Stated plainly so that the gap is a decision rather than a discovery, in the same
spirit as the closing section of `SCHEMA.md`.

**`clusters` has no confidence, truncation, or model-version columns.** Sections
3.4, 4.3 and 6 all produce results that are qualified — truncated, converged or
not, computed under model version *n*. The row as it stands cannot say so. Until
columns exist, the rule in this document is that a result that cannot be stood
behind is not written at all, and the cluster goes to the unresolved queue. That
is correct but lossy: "we looked and could not tell" and "we never looked" become
the same absent row.

**Token-level holder concentration has nowhere to live.** `clusters.hhi` is the
cluster-internal index per the schema, and `candidates` has no concentration
column. The top-1/5/10 shares, token HHI, normalised entropy and effective holder
count from section 2 are gate inputs with no table. They currently exist only in
the snapshot and vanish when it is replaced, which means a decision made on them
cannot be explained afterwards.

**Wash-trading metrics have nowhere to live.** `RT`, `CYC` and `wash_share` from
section 5.2 feed sizing through effective volume, and effective volume is not
stored either. A backtest cannot currently reproduce why a size was what it was.

**`BreakerReason` has no volatility variant.** The volatility breaker in section
11.1 is specified and has no honest reason code. Mapping it onto `SlippageSpike`
would make the audit trail say something that is not true. It needs a variant, or
the breaker needs to not exist.

**The exclusion-list version is not recorded anywhere.** Section 1 requires it
next to every concentration metric and there is no column for it.

None of these block the metrics being implemented. All of them block a Phase 3
replay dossier being able to explain a decision, which is the thing the phase
exists to produce.

## 17. What this document does not decide

- **Model weights and calibration.** The logistic in section 6 has a shape, not a
  fit. Its coefficients come from the calibration fixture with a Brier score, a
  reliability diagram and a leakage check, and no model is promoted on in-sample
  performance.
- **Threshold values.** Every number in a policy table here is a starting point
  to be re-derived from held-out data. They are written down so they can be
  argued with against evidence, not so they can be treated as settled.
- **Execution mechanics.** Bundle construction, tip pricing, simulation and the
  private-relay contract are Phase 4 and live in their own document. This one
  stops at "an emergency route exists and is inside its validity".
- **The EV model.** Section 10 consumes `stressed_loss_bps` and a stressed EV
  lower bound. Where those come from is Annex B, not here.
- **Provider health scoring.** Section 9.5 consumes a health band. How the band
  is computed is Phase 1.

The line this document draws is between measurement and judgement. Part I
measures and refuses to conclude. Part II concludes and refuses to measure. The
seam between them is the versioned snapshot, and the reason the seam is drawn
there is that it is the only place where a slow, honest answer and a fast,
bounded one can be made to coexist without either one corrupting the other.
