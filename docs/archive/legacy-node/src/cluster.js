// Is this launch one person wearing several wallets.
//
// score.js already counts the single clearest tell — several wallets putting in
// the identical amount — and stops there. This file is the longer answer: it
// takes the opening window of a coin and asks whether the buyers in it behave
// like separate people or like one operator with a script.
//
// Three things give a script away, and they are independent of each other:
//
//   1. Size. People pick amounts that mean something to them; a script picks one
//      amount and repeats it. Identical or near-identical positions across
//      several addresses is the loudest signal in the recorded data.
//   2. Timing. Separate people cannot land in the same slot. Addresses that all
//      execute inside the same fraction of a second were sent together.
//   3. Money in. Addresses funded from the same place a hop or two back are the
//      same wallet with extra steps. This is the only one of the three that is
//      hard evidence rather than inference — and it is the one we usually do not
//      have, because it needs transfer history the watcher does not collect.
//
// The confidence number adds up what each of those is worth, and the weights
// deliberately sum to more than one. That is not sloppiness: it means a missing
// signal costs nothing. Almost every live coin arrives without a funding graph,
// and a denominator that included it would cap every one of them below the
// syndicate threshold — a detector that can never fire on the data it actually
// gets. Two strong signals are enough to clear the bar; proof of shared funding
// is enough on its own.
//
// Everything here is pure: same input, same output, nothing written, nothing
// read from disk. It can be called from strategy.js, from the candidate console,
// or from a test with a hand-written record, and it behaves the same way.

/** The opening window, in seconds. Matches the cutoff score.js uses. */
export const WINDOW_SEC = 3;

/** How many opening buyers to consider at most, oldest first. */
export const MAX_WALLETS = 50;

/** A Solana slot, near enough. Used to turn "first N slots" into seconds. */
export const SLOT_SEC = 0.4;

/**
 * Two positions count as near-identical within this relative distance. 2% is
 * wide enough to survive a different priority fee and slippage on the same
 * scripted amount, and tight enough that 1.0 and 1.05 stay separate.
 */
export const SIZE_TOLERANCE = 0.02;

/** Timing gap under which consecutive buys are treated as one bundle. */
export const BUNDLE_SEC = 0.25;

/** Buys this close together are the same transaction or the same slot. */
export const INSTANT_SEC = 0.02;

/** A group of repeated positions has to be at least this big to mean anything. */
export const MIN_GROUP = 3;

/**
 * How much each kind of link between two wallets is worth, and how much is
 * needed before they are called the same operator.
 *
 * Shared funding alone is enough. An identical position alone is enough, unless
 * the amount is a round number a human might also have picked — two people both
 * buying exactly 1 SOL is a coincidence that happens all day, so that link needs
 * corroboration from timing. Being in the same bundle is never enough on its
 * own: a bundle is where every sniper in the world is trying to be.
 */
export const LINK_WEIGHTS = {
  shared_funder: 1,
  identical_size: 0.6,
  identical_round_size: 0.45,
  near_size: 0.35,
  same_instant: 0.5,
  bundle: 0.35,
};
export const LINK_THRESHOLD = 0.6;

/**
 * The most each signal can add to the confidence number. They sum to 1.8 on
 * purpose — see the note at the top. Size is worth most of what can be read off
 * a launch alone; the dev's behaviour is worth least, because a dev buying its
 * own coin is a rug risk rather than evidence of a syndicate, and the two get
 * confused constantly.
 */
export const SIGNAL_WEIGHTS = {
  sizing: 0.5,
  timing: 0.3,
  dev: 0.2,
  funding: 0.8,
};

/** Every tag this file can put on a launch. */
export const RISK_TAGS = [
  'IDENTICAL_SIZING',
  'NEAR_IDENTICAL_SIZING',
  'LOW_SIZING_ENTROPY',
  'SAME_INSTANT_BUNDLE',
  'SUB_SECOND_BUNDLE',
  'FIRST_SLOT_CROWD',
  'SOLO_DEV_DOMINANCE',
  'CREATOR_BOUGHT_OWN',
  'CREATOR_EXIT',
  'WHALE_CONCENTRATION',
  'SHARED_FUNDER',
  'NO_OPENING_BUYS',
  'INSUFFICIENT_DATA',
];

/** Below this many opening buyers, nothing here can be told apart from noise. */
export const MIN_PARTICIPANTS = 3;

/** The most confidence a launch too thin to read is allowed to report. */
const THIN_CEILING = 0.25;

// ---------------------------------------------------------------------------
// The analyser
// ---------------------------------------------------------------------------

/**
 * The whole read on one launch.
 *
 * @param {object} coinRecord a record as written by the watcher — `who`,
 *   `creator`, `open` — or anything with the same shape. Field names are
 *   normalised, so a row parsed from an RPC or Helius response can use
 *   `address`/`sol`/`tx_count` instead of `w`/`in`/`n`.
 * @param {object} [options]
 * @param {number} [options.windowSec=3] opening window in seconds.
 * @param {number} [options.maxWallets=50] cap on buyers considered.
 * @param {number} [options.sizeTolerance=0.02] relative width of a size group.
 * @param {number} [options.bundleSec=0.25] gap that ends a timing bundle.
 * @param {number} [options.devSlots=4] how many slots count as "with the dev".
 * @param {number} [options.minGroup=3] smallest repeated-size group that counts.
 * @param {Array}  [options.transfers] raw SOL transfers, if the caller has them.
 * @param {Map}    [options.adjacency] a graph from buildAdjacencyList, instead.
 * @param {number} [options.fundingDepth=2] hops to walk back for funders.
 * @param {Array}  [options.participants] opening buyers, if not on the record.
 * @returns {object} ClusterReport
 */
export function analyzeLaunch(coinRecord, options = {}) {
  const {
    windowSec = WINDOW_SEC,
    maxWallets = MAX_WALLETS,
    sizeTolerance = SIZE_TOLERANCE,
    bundleSec = BUNDLE_SEC,
    devSlots = 4,
    minGroup = MIN_GROUP,
    transfers = null,
    adjacency = null,
    fundingDepth = 2,
    participants: given = null,
  } = options;

  const coin = coinRecord || {};
  const creator = coin.creator ?? coin.dev ?? null;
  const rows = normaliseParticipants(given ?? coin.who ?? coin.participants ?? []);

  // The opening window, oldest first, capped. `at` is seconds since the launch
  // transaction, which is what the watcher records; a row without one is treated
  // as having arrived at the launch itself rather than being thrown away.
  const early = rows
    .filter((r) => r.at <= windowSec && r.sol > 0)
    .sort((a, b) => a.at - b.at)
    .slice(0, maxWallets);

  const openSol = early.reduce((s, r) => s + r.sol, 0);
  const base = {
    mint: coin.mint ?? null,
    creator,
    window: {
      seconds: windowSec,
      slots: devSlots,
      participants: early.length,
      considered: rows.length,
      sol_in: round(openSol, 4),
    },
  };

  if (!early.length) {
    return {
      ...base,
      confidence_score: 0,
      syndicate_size: 0,
      largest_cluster: 0,
      clustered_wallets: [],
      clusters: [],
      participants: [],
      risk_tags: ['NO_OPENING_BUYS'],
      signals: emptySignals(),
      reasons: ['nobody bought inside the opening window'],
      thin: true,
    };
  }

  // --- 1. Size -------------------------------------------------------------
  const sizeGroups = groupBySize(early, sizeTolerance);
  const repeated = sizeGroups.filter((g) => g.members.length >= minGroup);
  const inRepeated = repeated.reduce((s, g) => s + g.members.length, 0);
  const entropy = sizingEntropy(early.map((r) => r.sol), sizeTolerance);
  const biggestGroup = repeated.reduce((m, g) => Math.max(m, g.members.length), 0);
  const anyExact = repeated.some((g) => g.exact);

  const sizing = {
    entropy: round(entropy, 4),
    groups: repeated.map((g) => ({
      sol: round(g.value, 4),
      wallets: g.members.length,
      exact: g.exact,
      round_number: g.roundNumber,
    })),
    repeated_wallets: inRepeated,
    largest_group: biggestGroup,
    // Driven by the biggest repeated group rather than by what share of the
    // launch it is. Three addresses on one odd amount among forty buyers is
    // still three addresses run by one person — the share of the launch they
    // hold is a separate question, and getSyndicateExposure answers it. The
    // entropy term is a small top-up for a launch with no repeats big enough to
    // count but hardly any variety either.
    score: clamp01(groupStrength(biggestGroup, minGroup) * sizeQuality(repeated) + 0.2 * (1 - entropy)),
  };

  // --- 2. Timing -----------------------------------------------------------
  const bundles = findBundles(early, bundleSec);
  const topBundle = bundles.reduce(
    (m, b) => (b.members.length > m.members.length ? b : m),
    { members: [], span: 0 },
  );
  const realBundle = topBundle.members.length >= minGroup ? topBundle : null;
  const instant = realBundle && realBundle.span <= INSTANT_SEC;
  // A bundle sitting on the launch itself is the block every sniper on the
  // network is trying to be in, and the watcher records that whole block at the
  // same hundredth of a second whether or not the buyers know each other. So it
  // counts for half. A bundle that forms a second and a half later, when there
  // is no race left to explain it, counts for all of it.
  const launchBlock = !!realBundle && realBundle.members[0].at <= INSTANT_SEC;

  const timing = {
    // `members` and `sol` are here so a caller can ask a question about the
    // buyers inside one bundle — how much they committed between them, whether
    // they all took the same size — without re-deriving the grouping and
    // drifting from the rule above. backtest.js's entry gate is the caller that
    // needs both.
    bundles: bundles
      .filter((b) => b.members.length >= minGroup)
      .map((b) => ({
        wallets: b.members.length,
        at: round(b.members[0].at, 3),
        span: round(b.span, 3),
        same_instant: b.span <= INSTANT_SEC,
        sol: round(sumSol(b.members), 4),
        members: b.members.map((r) => r.address),
      })),
    largest_bundle: realBundle ? realBundle.members.length : 0,
    span: realBundle ? round(realBundle.span, 3) : null,
    same_instant: !!instant,
    launch_block: launchBlock,
    // A bundle with no measurable span is one transaction; a bundle spread over
    // a fifth of a second is a crowded slot. Both are worth seeing and they are
    // not worth the same.
    score: realBundle
      ? clamp01(
          groupStrength(realBundle.members.length, minGroup) *
            (instant ? 1 : 0.6) *
            (launchBlock ? 0.5 : 1),
        )
      : 0,
  };

  // --- 3. The dev ----------------------------------------------------------
  const devWindow = devSlots * SLOT_SEC;
  const creatorRow = creator ? rows.find((r) => r.address === creator) : null;
  const creatorSol = creatorRow && creatorRow.at <= windowSec ? creatorRow.sol : 0;
  const creatorShare = openSol > 0 ? creatorSol / openSol : 0;
  const withDev = early.filter((r) => r.at <= devWindow);
  const biggest = early.reduce((m, r) => Math.max(m, r.sol), 0);
  const concentration = openSol > 0 ? biggest / openSol : 0;

  const dev = {
    creator_bought: creatorSol > 0,
    creator_sol: round(creatorSol, 4),
    creator_share: round(creatorShare, 4),
    creator_sold: !!creatorRow && creatorRow.solOut > 0,
    with_dev: withDev.length,
    with_dev_share: round(withDev.length / early.length, 4),
    concentration: round(concentration, 4),
    // Buying your own launch is common and only mildly interesting. Owning half
    // the opening money is the thing that lets one address leave whenever it
    // likes, and selling inside the window is that promise being kept.
    score: clamp01(
      (creatorSol > 0 ? 0.4 : 0) +
        Math.min(1, creatorShare / 0.5) * 0.5 +
        (creatorRow && creatorRow.solOut > 0 ? 0.3 : 0),
    ),
  };

  // --- 4. Money in ---------------------------------------------------------
  // Only evaluated when the caller supplied transfers or a graph. Absent, it is
  // left out of the average rather than scored as zero — a missing test is not
  // a passed test.
  const graph = adjacency ?? (transfers ? buildAdjacencyList(transfers) : null);
  let funding = null;
  if (graph) {
    const shared = findSharedFunders(early.map((r) => r.address), fundingDepth, graph);
    funding = {
      available: true,
      overlap_pct: shared.overlapPct,
      linked_wallets: shared.linkedWallets.length,
      funders: shared.funders
        .filter((f) => !f.hub)
        .map((f) => ({ funder: f.funder, hops: f.hops, wallets: f.wallets.length })),
      hubs_ignored: shared.funders.filter((f) => f.hub).length,
      // Any shared funder at all is most of the way to proof, so this starts at
      // a half rather than at nothing and reaches the top once it accounts for
      // half the opening. The difference between two linked wallets and ten is
      // the size of the syndicate, not whether there is one.
      score: shared.overlapPct > 0 ? clamp01(0.5 + 0.5 * Math.min(1, shared.overlapPct / 50)) : 0,
      pairs: shared.pairs,
    };
  }

  // --- Clusters ------------------------------------------------------------
  const { clusters, byWallet, relations } = clusterWallets(early, {
    sizeGroups,
    bundles,
    minGroup,
    fundingPairs: funding ? funding.pairs : [],
  });

  const clusteredSol = clusters.reduce((s, c) => s + c.sol, 0);
  const largest = clusters.reduce((m, c) => Math.max(m, c.size), 0);
  const clusteredCount = clusters.reduce((s, c) => s + c.size, 0);

  // --- Confidence ----------------------------------------------------------
  let confidence = clamp01(
    SIGNAL_WEIGHTS.sizing * sizing.score +
      SIGNAL_WEIGHTS.timing * timing.score +
      SIGNAL_WEIGHTS.dev * dev.score +
      (funding ? SIGNAL_WEIGHTS.funding * funding.score : 0),
  );

  const thin = early.length < MIN_PARTICIPANTS;
  if (thin) confidence = Math.min(confidence, THIN_CEILING);

  // --- Tags and plain words ------------------------------------------------
  const risk_tags = [];
  const reasons = [];

  if (repeated.length) {
    if (anyExact) risk_tags.push('IDENTICAL_SIZING');
    else risk_tags.push('NEAR_IDENTICAL_SIZING');
    const g = repeated.reduce((m, x) => (x.members.length > m.members.length ? x : m));
    reasons.push(
      `${g.members.length} wallets bought ${g.exact ? 'the identical' : 'within 2% of the same'} amount (${round(g.value, 4)} SOL) — one operator, not ${g.members.length} buyers`,
    );
  }
  if (early.length >= 4 && entropy <= 0.6) {
    risk_tags.push('LOW_SIZING_ENTROPY');
    reasons.push(`${early.length} buyers between them used very few distinct sizes`);
  }
  if (realBundle) {
    risk_tags.push(instant ? 'SAME_INSTANT_BUNDLE' : 'SUB_SECOND_BUNDLE');
    reasons.push(
      instant
        ? `${realBundle.members.length} wallets landed in the same instant — they were sent together`
        : `${realBundle.members.length} wallets landed inside ${round(realBundle.span, 2)}s of each other`,
    );
  }
  if (withDev.length >= minGroup && withDev.length / early.length >= 0.5) {
    risk_tags.push('FIRST_SLOT_CROWD');
    reasons.push(`${withDev.length} of ${early.length} opening buyers were in within ${devSlots} slots of the launch`);
  }
  if (dev.creator_bought) {
    risk_tags.push('CREATOR_BOUGHT_OWN');
    reasons.push(`the creator bought ${dev.creator_sol} SOL of its own launch`);
  }
  if (creatorShare >= 0.5) {
    risk_tags.push('SOLO_DEV_DOMINANCE');
    reasons.push(`the creator is ${(creatorShare * 100).toFixed(0)}% of the opening money`);
  }
  if (dev.creator_sold) {
    risk_tags.push('CREATOR_EXIT');
    reasons.push('the creator sold inside the follow window');
  }
  if (concentration >= 0.7 && early.length > 1 && creatorShare < 0.5) {
    risk_tags.push('WHALE_CONCENTRATION');
    reasons.push(`one wallet is ${(concentration * 100).toFixed(0)}% of the opening money`);
  }
  if (funding && funding.overlap_pct > 0) {
    risk_tags.push('SHARED_FUNDER');
    reasons.push(
      `${funding.linked_wallets} of ${early.length} opening buyers trace back to the same funding wallet within ${fundingDepth} hops`,
    );
  }
  if (thin) {
    risk_tags.push('INSUFFICIENT_DATA');
    reasons.push(`only ${early.length} buyer${early.length === 1 ? '' : 's'} in the opening — too few to tell coordination from coincidence`);
  }

  return {
    ...base,
    confidence_score: round(confidence, 4),
    syndicate_size: clusteredCount,
    largest_cluster: largest,
    clustered_wallets: clusters.flatMap((c) =>
      c.members.map((r) => ({
        address: r.address,
        sol_spent: round(r.sol, 4),
        tx_count: r.txCount,
        cluster_id: c.id,
        flags: byWallet.get(r.address) ?? [],
        at: r.at,
      })),
    ),
    clusters: clusters.map((c) => ({
      id: c.id,
      size: c.size,
      sol_spent: round(c.sol, 4),
      share_of_open: openSol > 0 ? round(c.sol / openSol, 4) : 0,
      first_at: round(c.firstAt, 3),
      reasons: c.reasons,
      members: c.members.map((r) => r.address),
    })),
    participants: early.map((r) => ({
      address: r.address,
      sol_spent: round(r.sol, 4),
      sol_out: round(r.solOut, 4),
      tx_count: r.txCount,
      at: r.at,
      cluster_id: clusterIdOf(clusters, r.address),
      flags: byWallet.get(r.address) ?? [],
    })),
    risk_tags,
    signals: { sizing, timing, dev, funding: funding ?? { available: false, score: null } },
    reasons,
    exposure_sol: round(clusteredSol, 4),
    relations,
    thin,
  };
}

// ---------------------------------------------------------------------------
// Helpers a caller is meant to use
// ---------------------------------------------------------------------------

/**
 * Is this launch a syndicate. The threshold is the caller's, because what counts
 * as enough proof depends on what happens next — refusing to buy wants a lower
 * bar than accusing somebody.
 */
export function isSyndicate(report, threshold = 0.75) {
  if (!report || typeof report.confidence_score !== 'number') return false;
  return report.confidence_score >= threshold;
}

/**
 * How much of the opening the correlated wallets control. This is the number
 * that matters for trading: a syndicate holding 8% of a launch can be ignored,
 * the same syndicate holding 80% is the entire order book and can leave in one
 * transaction.
 */
export function getSyndicateExposure(report) {
  if (!report) return { sol: 0, pct: 0, wallets: 0, largest_cluster: 0, clusters: 0 };
  const open = report.window?.sol_in ?? 0;
  const sol = report.exposure_sol ?? 0;
  return {
    sol: round(sol, 4),
    pct: open > 0 ? round((sol / open) * 100, 2) : 0,
    wallets: report.syndicate_size ?? 0,
    largest_cluster: report.largest_cluster ?? 0,
    clusters: report.clusters?.length ?? 0,
  };
}

// ---------------------------------------------------------------------------
// Graph and funding traversal
// ---------------------------------------------------------------------------

/**
 * Turn a flat list of SOL transfers into a graph that can be walked in either
 * direction. Field names are normalised so the same function takes rows from an
 * RPC parser, a Helius enriched transaction, or a test fixture.
 *
 * @param {Array} transfers rows with a source and a destination, e.g.
 *   `{ from, to, sol }`, `{ source, destination, lamports }`.
 * @returns {Map<string, {out: Map, in: Map}>} address -> who it paid, who paid it.
 */
export function buildAdjacencyList(transfers) {
  const graph = new Map();
  if (!Array.isArray(transfers)) return graph;

  const node = (addr) => {
    let n = graph.get(addr);
    if (!n) graph.set(addr, (n = { out: new Map(), in: new Map() }));
    return n;
  };
  const bump = (map, key, sol) => {
    const e = map.get(key) ?? { count: 0, sol: 0 };
    e.count += 1;
    e.sol += sol;
    map.set(key, e);
  };

  for (const t of transfers) {
    if (!t) continue;
    const from = t.from ?? t.source ?? t.src ?? t.sender ?? null;
    const to = t.to ?? t.destination ?? t.dest ?? t.receiver ?? null;
    if (!from || !to || from === to) continue;
    const sol = Number.isFinite(t.sol)
      ? t.sol
      : Number.isFinite(t.lamports)
        ? t.lamports / 1e9
        : Number.isFinite(t.amount)
          ? t.amount
          : 0;
    bump(node(from).out, to, sol);
    bump(node(to).in, from, sol);
  }
  return graph;
}

/**
 * Walk backwards from each wallet looking for the address that paid for it, and
 * report where those paths meet.
 *
 * The trap here is the exchange. A Kraken hot wallet funds thousands of
 * unrelated people, and treating it as a shared funder makes every launch look
 * like a syndicate. So any address that has paid out to more than `hubDegree`
 * distinct addresses in the graph it was given is marked as a hub and left out
 * of the overlap — reported, not silently dropped, because "everyone here came
 * from an exchange" is itself worth knowing.
 *
 * @param {string[]} walletList addresses to trace back from.
 * @param {number} [depth=2] hops to walk. 1 is the direct funder, 2 catches the
 *   usual one-hop laundering through a fresh intermediate wallet.
 * @param {Map|Array} [source] a graph from buildAdjacencyList, or a raw transfer
 *   array to build one from.
 * @param {object} [options]
 * @param {number} [options.hubDegree=25] out-degree above which a funder is a hub.
 * @param {Set|Array} [options.exclude] addresses never counted as funders.
 */
export function findSharedFunders(walletList, depth = 2, source = null, options = {}) {
  const { hubDegree = 25, exclude = [] } = options;
  const graph = source instanceof Map ? source : buildAdjacencyList(source);
  const wallets = [...new Set((walletList || []).filter(Boolean))];
  const skip = new Set([...exclude, ...wallets]);

  const empty = {
    funders: [],
    byWallet: new Map(),
    pairs: [],
    linkedWallets: [],
    overlapPct: 0,
    depth,
  };
  if (!graph.size || wallets.length < 2) return empty;

  // funder -> wallet -> fewest hops from that wallet back to the funder.
  const reach = new Map();
  const byWallet = new Map();

  for (const wallet of wallets) {
    byWallet.set(wallet, new Set());
    let frontier = [wallet];
    const seen = new Set([wallet]);
    for (let hop = 1; hop <= depth && frontier.length; hop++) {
      const next = [];
      for (const addr of frontier) {
        const node = graph.get(addr);
        if (!node) continue;
        for (const funder of node.in.keys()) {
          if (seen.has(funder)) continue;
          seen.add(funder);
          next.push(funder);
          if (skip.has(funder)) continue;
          if (!reach.has(funder)) reach.set(funder, new Map());
          const m = reach.get(funder);
          if (!m.has(wallet)) m.set(wallet, hop);
          byWallet.get(wallet).add(funder);
        }
      }
      frontier = next;
    }
  }

  const funders = [];
  const linked = new Set();
  const pairs = [];

  for (const [funder, m] of reach) {
    if (m.size < 2) continue;
    const hub = (graph.get(funder)?.out.size ?? 0) > hubDegree;
    const members = [...m.keys()];
    funders.push({
      funder,
      hub,
      hops: Math.min(...m.values()),
      wallets: members,
    });
    if (hub) continue;
    for (const w of members) linked.add(w);
    for (let i = 0; i < members.length; i++) {
      for (let j = i + 1; j < members.length; j++) {
        pairs.push([members[i], members[j], funder, Math.max(m.get(members[i]), m.get(members[j]))]);
      }
    }
  }

  funders.sort((a, b) => b.wallets.length - a.wallets.length || a.hops - b.hops);

  return {
    funders,
    byWallet,
    pairs,
    linkedWallets: [...linked],
    // The share of the buyers we were asked about that share a funder with at
    // least one other buyer. Not the share of pairs — one funder behind five of
    // twenty wallets is 25%, which is the sentence a person would say.
    overlapPct: round((linked.size / wallets.length) * 100, 2),
    depth,
  };
}

/**
 * How much variety there is in the opening positions, from 0 (every wallet took
 * the same size) to 1 (every wallet took a different one). Sizes are grouped by
 * the same tolerance the clustering uses, so a script with jittered amounts does
 * not read as variety.
 */
export function sizingEntropy(amounts, tolerance = SIZE_TOLERANCE) {
  const values = (amounts || []).filter((n) => Number.isFinite(n) && n > 0);
  const n = values.length;
  if (n < 2) return 1;
  const groups = groupBySize(values.map((sol) => ({ sol })), tolerance);
  let h = 0;
  for (const g of groups) {
    const p = g.members.length / n;
    h -= p * Math.log(p);
  }
  return clamp01(h / Math.log(n));
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/** Accept the watcher's field names or an RPC parser's, and nothing else. */
function normaliseParticipants(rows) {
  if (!Array.isArray(rows)) return [];
  const out = [];
  for (const r of rows) {
    if (!r) continue;
    const address = r.w ?? r.address ?? r.wallet ?? r.owner ?? null;
    if (!address) continue;
    const sol = num(r.in ?? r.sol ?? r.sol_in ?? r.solIn ?? r.sol_spent);
    const solOut = num(r.out ?? r.sol_out ?? r.solOut);
    const txCount = Math.max(1, Math.round(num(r.n ?? r.tx_count ?? r.txCount ?? r.trades) || 1));
    const at = num(r.at ?? r.tSec ?? r.seconds ?? r.offset);
    out.push({ address, sol, solOut, txCount, at });
  }
  return out;
}

/**
 * Positions grouped by size. Each group is bounded by the tolerance from its own
 * smallest member rather than chained neighbour to neighbour — otherwise a long
 * ladder of amounts 1% apart collapses into one group and the whole launch looks
 * coordinated.
 */
function groupBySize(rows, tolerance) {
  const sorted = [...rows].filter((r) => r.sol > 0).sort((a, b) => a.sol - b.sol);
  const groups = [];
  for (const r of sorted) {
    const g = groups.at(-1);
    if (g && (r.sol - g.min) / g.min <= tolerance) {
      g.members.push(r);
      g.max = r.sol;
    } else {
      groups.push({ min: r.sol, max: r.sol, members: [r] });
    }
  }
  return groups.map((g) => {
    // "Exact" is to four decimals, which is the precision the record keeps.
    const exact = g.members.every((m) => m.sol.toFixed(4) === g.members[0].sol.toFixed(4));
    return {
      value: g.members[0].sol,
      members: g.members,
      exact,
      roundNumber: isRoundAmount(g.members[0].sol),
    };
  });
}

/**
 * Buys that all landed inside one `gap`-wide window.
 *
 * The window is measured from the first buy in the run, not from the previous
 * one. Measuring gap to gap looks equivalent and is not: on a busy launch every
 * consecutive pair is a tenth of a second apart, the run never breaks, and
 * twenty-six wallets spread over a second and a half get reported as one
 * bundle. That reads as a conspiracy and is just a queue.
 */
function findBundles(early, gap) {
  const sorted = [...early].sort((a, b) => a.at - b.at);
  const bundles = [];
  let cur = [];
  for (const r of sorted) {
    if (!cur.length || r.at - cur[0].at <= gap) cur.push(r);
    else {
      bundles.push(cur);
      cur = [r];
    }
  }
  if (cur.length) bundles.push(cur);
  return bundles.map((members) => ({
    members,
    span: members.at(-1).at - members[0].at,
  }));
}

/**
 * Join opening buyers into operators. Every pair accumulates whatever evidence
 * links it and is joined once that evidence passes the threshold, so no single
 * weak signal can merge a launch into one imaginary syndicate.
 */
function clusterWallets(early, { sizeGroups, bundles, minGroup, fundingPairs }) {
  const index = new Map(early.map((r, i) => [r.address, i]));
  const edges = new Map(); // "i|j" -> { weight, kinds: Set }

  const link = (a, b, kind) => {
    const i = index.get(a);
    const j = index.get(b);
    if (i === undefined || j === undefined || i === j) return;
    const key = i < j ? `${i}|${j}` : `${j}|${i}`;
    const e = edges.get(key) ?? { weight: 0, kinds: new Set() };
    if (e.kinds.has(kind)) return;
    e.kinds.add(kind);
    e.weight += LINK_WEIGHTS[kind] ?? 0;
    edges.set(key, e);
  };

  for (const g of sizeGroups) {
    if (g.members.length < 2) continue;
    const kind = g.exact ? (g.roundNumber ? 'identical_round_size' : 'identical_size') : 'near_size';
    for (let i = 0; i < g.members.length; i++) {
      for (let j = i + 1; j < g.members.length; j++) link(g.members[i].address, g.members[j].address, kind);
    }
  }

  for (const b of bundles) {
    if (b.members.length < 2) continue;
    const kind = b.span <= INSTANT_SEC ? 'same_instant' : 'bundle';
    for (let i = 0; i < b.members.length; i++) {
      for (let j = i + 1; j < b.members.length; j++) link(b.members[i].address, b.members[j].address, kind);
    }
  }

  for (const [a, b] of fundingPairs || []) link(a, b, 'shared_funder');

  // Union-find over the edges that cleared the bar.
  const parent = early.map((_, i) => i);
  const find = (i) => {
    while (parent[i] !== i) {
      parent[i] = parent[parent[i]];
      i = parent[i];
    }
    return i;
  };
  const kept = [];
  for (const [key, e] of edges) {
    if (e.weight < LINK_THRESHOLD) continue;
    const [i, j] = key.split('|').map(Number);
    kept.push({ a: early[i].address, b: early[j].address, weight: round(e.weight, 3), kinds: [...e.kinds] });
    const ri = find(i);
    const rj = find(j);
    if (ri !== rj) parent[ri] = rj;
  }

  const byRoot = new Map();
  early.forEach((r, i) => {
    const root = find(i);
    if (!byRoot.has(root)) byRoot.set(root, []);
    byRoot.get(root).push(r);
  });

  // Which kinds of evidence ended up inside each cluster, for the report.
  const kindsByRoot = new Map();
  for (const edge of kept) {
    const root = find(index.get(edge.a));
    if (!kindsByRoot.has(root)) kindsByRoot.set(root, new Set());
    for (const k of edge.kinds) kindsByRoot.get(root).add(k);
  }

  const clusters = [...byRoot.entries()]
    .filter(([, members]) => members.length >= 2)
    .map(([root, members]) => ({ root, members }))
    .sort((a, b) => b.members.length - a.members.length || sumSol(b.members) - sumSol(a.members))
    .map((c, i) => ({
      id: `c${i + 1}`,
      size: c.members.length,
      sol: sumSol(c.members),
      firstAt: Math.min(...c.members.map((m) => m.at)),
      members: c.members,
      reasons: [...(kindsByRoot.get(c.root) ?? [])].sort(),
    }));

  // Per-wallet flags, including ones that do not depend on being clustered.
  const flags = new Map(early.map((r) => [r.address, []]));
  for (const g of sizeGroups) {
    if (g.members.length < minGroup) continue;
    for (const m of g.members) flags.get(m.address)?.push(g.exact ? 'IDENTICAL_SIZE' : 'NEAR_IDENTICAL_SIZE');
  }
  for (const b of bundles) {
    if (b.members.length < minGroup) continue;
    for (const m of b.members) flags.get(m.address)?.push(b.span <= INSTANT_SEC ? 'SAME_INSTANT' : 'BUNDLED');
  }
  for (const [a, b] of fundingPairs || []) {
    flags.get(a)?.push('SHARED_FUNDER');
    flags.get(b)?.push('SHARED_FUNDER');
  }
  for (const r of early) if (r.solOut > 0) flags.get(r.address)?.push('SOLD_IN_WINDOW');
  for (const [addr, list] of flags) flags.set(addr, [...new Set(list)]);

  return { clusters, byWallet: flags, relations: kept };
}

function clusterIdOf(clusters, address) {
  for (const c of clusters) if (c.members.some((m) => m.address === address)) return c.id;
  return null;
}

function emptySignals() {
  return {
    sizing: { entropy: 1, groups: [], repeated_wallets: 0, largest_group: 0, score: 0 },
    timing: { bundles: [], largest_bundle: 0, span: null, same_instant: false, launch_block: false, score: 0 },
    dev: {
      creator_bought: false, creator_sol: 0, creator_share: 0, creator_sold: false,
      with_dev: 0, with_dev_share: 0, concentration: 0, score: 0,
    },
    funding: { available: false, score: null },
  };
}

/**
 * How much a repeated group of k wallets is worth, from nothing below the
 * minimum to everything three past it. Three matching wallets could be an
 * accident; six could not.
 */
function groupStrength(k, minGroup) {
  if (!(k >= minGroup)) return 0;
  return Math.min(1, (k - minGroup + 1) / 3);
}

/**
 * How trustworthy a repeat is. Matching to the fourth decimal is a script.
 * Matching on a number a person would type — 1 SOL, 0.5 SOL — is the one repeat
 * that happens honestly, so it is discounted and has to be corroborated.
 */
function sizeQuality(groups) {
  if (!groups.length) return 0;
  const g = groups.reduce((m, x) => (x.members.length > m.members.length ? x : m));
  if (!g.exact) return 0.6;
  return g.roundNumber ? 0.75 : 1;
}

/** An amount a person might have typed, rather than one a script produced. */
function isRoundAmount(x) {
  if (!(x > 0)) return false;
  const step = x >= 1 ? 0.1 : 0.05;
  return Math.abs(x / step - Math.round(x / step)) < 1e-6;
}

function sumSol(rows) {
  return rows.reduce((s, r) => s + r.sol, 0);
}
function num(x) {
  const n = Number(x);
  return Number.isFinite(n) ? n : 0;
}
function clamp01(x) {
  return Math.max(0, Math.min(1, Number.isFinite(x) ? x : 0));
}
function round(n, dp = 2) {
  const f = 10 ** dp;
  return Math.round(Number(n) * f) / f;
}
