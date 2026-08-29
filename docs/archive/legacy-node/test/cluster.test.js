// What the clustering has to get right, and what it has to refuse to say.
//
// Two kinds of test here. The synthetic ones pin the behaviour: a launch built
// to be a syndicate has to read as one, and a launch built to be ordinary has to
// read as ordinary — the second half being the one that matters, because a
// detector that flags everything is the same as no detector.
//
// The rest run over the real recorded launches in data/. Those have no labels,
// so they check invariants rather than answers: nothing throws, no score leaves
// its range, no launch is reported as more syndicate than it has wallets, and
// the rate at which the whole corpus trips the threshold stays low enough that
// the number still means something. data/ is not in git, so those tests skip
// rather than fail when it is not there.
//
// Run with: node --test test/

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import {
  analyzeLaunch,
  buildAdjacencyList,
  findSharedFunders,
  sizingEntropy,
  isSyndicate,
  getSyndicateExposure,
  RISK_TAGS,
} from '../src/cluster.js';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const DATA = path.join(ROOT, 'data');

// ---------------------------------------------------------------------------
// Fixtures, in the shape the watcher writes
// ---------------------------------------------------------------------------

const CREATOR = 'CreatorWa11etAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA';
const addr = (i) => `Wa11et${String(i).padStart(2, '0')}xxxxxxxxxxxxxxxxxxxxxxxxxxxx`;

/** Nobody bought. The launch that exists and then does not. */
const zeroBuy = {
  mint: 'zero',
  creator: CREATOR,
  who: [],
  open: { seconds: 3, wallets: 0, solIn: 0 },
};

/** Buys recorded, but all of them are sells — nothing entered the window. */
const sellersOnly = {
  mint: 'sellers',
  creator: CREATOR,
  who: [
    { w: addr(1), in: 0, out: 0.4, n: 1, at: 0.9 },
    { w: addr(2), in: 0, out: 1.1, n: 2, at: 1.4 },
  ],
};

/** Eight people who do not know each other, buying what they felt like. */
const organic = {
  mint: 'organic',
  creator: CREATOR,
  who: [
    { w: addr(1), in: 0.3172, out: 0, n: 1, at: 0.31 },
    { w: addr(2), in: 1.4041, out: 0, n: 2, at: 0.62 },
    { w: addr(3), in: 0.0488, out: 0.0121, n: 3, at: 0.98 },
    { w: addr(4), in: 2.7713, out: 0, n: 1, at: 1.33 },
    { w: addr(5), in: 0.6109, out: 0, n: 1, at: 1.71 },
    { w: addr(6), in: 0.1902, out: 0, n: 2, at: 2.09 },
    { w: addr(7), in: 3.8874, out: 1.2, n: 4, at: 2.44 },
    { w: addr(8), in: 0.8365, out: 0, n: 1, at: 2.88 },
  ],
};

/** Five addresses, one odd amount, one instant, a second and a half in. */
const snipe = {
  mint: 'snipe',
  creator: CREATOR,
  who: [
    { w: CREATOR, in: 0.7331, out: 0, n: 1, at: 1.52 },
    { w: addr(1), in: 0.7331, out: 0, n: 1, at: 1.52 },
    { w: addr(2), in: 0.7331, out: 0, n: 1, at: 1.52 },
    { w: addr(3), in: 0.7331, out: 0, n: 1, at: 1.53 },
    { w: addr(4), in: 0.7331, out: 0, n: 1, at: 1.53 },
    { w: addr(5), in: 0.211, out: 0, n: 1, at: 2.7 },
  ],
};

/** The dev holding almost the whole opening, with two passers-by. */
const soloDev = {
  mint: 'solodev',
  creator: CREATOR,
  who: [
    { w: CREATOR, in: 8, out: 0, n: 1, at: 0.02 },
    { w: addr(1), in: 0.2143, out: 0, n: 1, at: 1.2 },
    { w: addr(2), in: 0.1077, out: 0, n: 2, at: 2.4 },
  ],
};

/** Sizes jittered by a percent or so, spread out in time to hide the bundle. */
const jittered = {
  mint: 'jittered',
  creator: CREATOR,
  who: [
    { w: addr(1), in: 0.99, out: 0, n: 1, at: 0.4 },
    { w: addr(2), in: 1.0, out: 0, n: 1, at: 1.1 },
    { w: addr(3), in: 1.008, out: 0, n: 1, at: 1.9 },
    { w: addr(4), in: 0.4127, out: 0, n: 1, at: 2.5 },
  ],
};

/** The same jitter, this time sent together. */
const jitteredBundle = {
  ...jittered,
  mint: 'jittered-bundle',
  who: [
    { w: addr(1), in: 0.99, out: 0, n: 1, at: 1.4 },
    { w: addr(2), in: 1.0, out: 0, n: 1, at: 1.4 },
    { w: addr(3), in: 1.008, out: 0, n: 1, at: 1.41 },
    { w: addr(4), in: 0.4127, out: 0, n: 1, at: 2.5 },
  ],
};

// ---------------------------------------------------------------------------
// The launches that must not be called syndicates
// ---------------------------------------------------------------------------

test('a launch nobody bought scores zero and says why', () => {
  for (const coin of [zeroBuy, sellersOnly]) {
    const r = analyzeLaunch(coin);
    assert.equal(r.confidence_score, 0);
    assert.equal(r.syndicate_size, 0);
    assert.deepEqual(r.clustered_wallets, []);
    assert.deepEqual(r.risk_tags, ['NO_OPENING_BUYS']);
    assert.equal(r.thin, true);
    assert.equal(isSyndicate(r), false);
  }
});

test('eight independent buyers do not look coordinated', () => {
  const r = analyzeLaunch(organic);
  assert.equal(r.window.participants, 8);
  assert.equal(r.syndicate_size, 0, 'nothing should cluster');
  assert.ok(r.confidence_score < 0.3, `expected a low score, got ${r.confidence_score}`);
  assert.equal(isSyndicate(r), false);
  assert.ok(r.signals.sizing.entropy > 0.9, 'eight distinct sizes is full variety');
  assert.equal(r.signals.timing.largest_bundle, 0);
  assert.ok(!r.risk_tags.includes('IDENTICAL_SIZING'));
});

test('a dev owning the opening is flagged, but is not a syndicate', () => {
  const r = analyzeLaunch(soloDev);
  assert.ok(r.risk_tags.includes('SOLO_DEV_DOMINANCE'));
  assert.ok(r.risk_tags.includes('CREATOR_BOUGHT_OWN'));
  assert.ok(r.signals.dev.creator_share > 0.9);
  assert.equal(isSyndicate(r), false, 'one wallet is a rug risk, not a group');
  assert.equal(r.syndicate_size, 0);
});

test('a fat non-creator wallet reads as concentration, not as a group', () => {
  const r = analyzeLaunch({
    mint: 'whale',
    creator: CREATOR,
    who: [
      { w: addr(1), in: 9, out: 0, n: 1, at: 0.5 },
      { w: addr(2), in: 1.3, out: 0, n: 1, at: 1.4 },
      { w: addr(3), in: 0.9, out: 0, n: 1, at: 2.2 },
    ],
  });
  assert.ok(r.risk_tags.includes('WHALE_CONCENTRATION'));
  assert.ok(!r.risk_tags.includes('SOLO_DEV_DOMINANCE'));
  assert.equal(isSyndicate(r), false);
});

test('a crowded launch block with varied sizes is a race, not a bundle', () => {
  // Five wallets in the opening instant, every one a different size. This is
  // what most real launches look like and it must not clear the threshold.
  const r = analyzeLaunch({
    mint: 'race',
    creator: CREATOR,
    who: [
      { w: addr(1), in: 3, out: 0, n: 3, at: 0.01 },
      { w: addr(2), in: 2.1, out: 0, n: 3, at: 0.01 },
      { w: addr(3), in: 1.8, out: 0, n: 3, at: 0.01 },
      { w: addr(4), in: 0.4866, out: 0, n: 2, at: 0.01 },
      { w: addr(5), in: 0.0099, out: 0, n: 2, at: 0.01 },
    ],
  });
  assert.equal(r.signals.timing.same_instant, true);
  assert.equal(r.signals.timing.launch_block, true, 'the discount has to apply');
  assert.ok(r.confidence_score < 0.4, `got ${r.confidence_score}`);
  assert.equal(r.syndicate_size, 0, 'timing alone must never merge wallets');
});

test('a steady queue of buyers is not one long bundle', () => {
  // Twelve wallets a tenth of a second apart. Every consecutive gap is inside
  // the bundle window, so measuring gap to gap would chain the whole lot into a
  // single "bundle" spanning 1.2 seconds — and then pair them off on near-
  // matching sizes. A bundle is a window, not a chain.
  const who = [];
  for (let i = 0; i < 12; i++) {
    who.push({ w: addr(i + 1), in: round(0.5 + i * 0.006, 4), out: 0, n: 1, at: round(0.4 + i * 0.1, 2) });
  }
  const r = analyzeLaunch({ mint: 'queue', creator: CREATOR, who });
  assert.ok(r.signals.timing.largest_bundle <= 4, `bundle of ${r.signals.timing.largest_bundle}`);
  assert.ok((r.signals.timing.span ?? 0) <= 0.25);
  assert.ok(r.confidence_score < 0.5, `got ${r.confidence_score}`);
});

test('two wallets on a round number are a coincidence until something confirms it', () => {
  const r = analyzeLaunch({
    mint: 'round',
    creator: CREATOR,
    who: [
      { w: addr(1), in: 1, out: 0, n: 1, at: 0.6 },
      { w: addr(2), in: 1, out: 0, n: 1, at: 2.1 },
      { w: addr(3), in: 0.34, out: 0, n: 1, at: 2.6 },
    ],
  });
  assert.equal(r.syndicate_size, 0, '1 SOL twice, far apart, is not proof');
  assert.equal(isSyndicate(r), false);
});

// ---------------------------------------------------------------------------
// The launches that must be called syndicates
// ---------------------------------------------------------------------------

test('five wallets on one odd amount in one instant is a syndicate', () => {
  const r = analyzeLaunch(snipe);
  assert.ok(r.confidence_score >= 0.75, `got ${r.confidence_score}`);
  assert.equal(isSyndicate(r), true);
  assert.equal(r.syndicate_size, 5);
  assert.equal(r.largest_cluster, 5);
  assert.equal(r.clusters.length, 1);
  assert.ok(r.risk_tags.includes('IDENTICAL_SIZING'));
  assert.ok(r.risk_tags.includes('SAME_INSTANT_BUNDLE'));
  assert.equal(r.signals.timing.launch_block, false, 'this bundle is not the opening block');

  // The sixth wallet bought a different amount later and must stay out of it.
  const members = r.clusters[0].members;
  assert.equal(members.length, 5);
  assert.ok(!members.includes(addr(5)));

  // Every clustered row carries the fields the console reads.
  for (const w of r.clustered_wallets) {
    assert.equal(typeof w.address, 'string');
    assert.equal(w.sol_spent, 0.7331);
    assert.equal(w.tx_count, 1);
    assert.equal(w.cluster_id, 'c1');
    assert.ok(w.flags.includes('IDENTICAL_SIZE'));
    assert.ok(w.flags.includes('SAME_INSTANT'));
  }
});

test('jittered sizes are named but do not cluster on their own', () => {
  const spread = analyzeLaunch(jittered);
  assert.ok(spread.risk_tags.includes('NEAR_IDENTICAL_SIZING'));
  assert.ok(!spread.risk_tags.includes('IDENTICAL_SIZING'));
  assert.equal(spread.syndicate_size, 0, 'near-matching alone is under the bar');

  // Same amounts, sent together: now there are two reasons and they combine.
  const together = analyzeLaunch(jitteredBundle);
  assert.equal(together.syndicate_size, 3);
  assert.ok(together.clusters[0].reasons.includes('near_size'));
  assert.ok(together.clusters[0].reasons.includes('same_instant'));
  assert.ok(together.confidence_score > spread.confidence_score);
});

test('a creator selling inside the window is called out', () => {
  const r = analyzeLaunch({
    mint: 'exit',
    creator: CREATOR,
    who: [
      { w: CREATOR, in: 3, out: 4.4871, n: 3, at: 0.01 },
      { w: addr(1), in: 2.1, out: 0, n: 1, at: 0.9 },
      { w: addr(2), in: 1.8, out: 0, n: 1, at: 1.6 },
    ],
  });
  assert.ok(r.risk_tags.includes('CREATOR_EXIT'));
  assert.ok(r.signals.dev.creator_sold);
});

// ---------------------------------------------------------------------------
// The funding graph
// ---------------------------------------------------------------------------

test('buildAdjacencyList takes whatever the parser calls its fields', () => {
  const g = buildAdjacencyList([
    { from: 'A', to: 'B', sol: 1.5 },
    { source: 'A', destination: 'C', lamports: 2_000_000_000 },
    { src: 'B', dest: 'D', amount: 0.25 },
    { from: 'A', to: 'B', sol: 0.5 },
    null,
    { from: 'X' },
    { from: 'Z', to: 'Z', sol: 1 },
  ]);

  assert.equal(g.get('A').out.get('B').count, 2);
  assert.equal(g.get('A').out.get('B').sol, 2);
  assert.equal(g.get('A').out.get('C').sol, 2);
  assert.equal(g.get('B').in.get('A').count, 2);
  assert.equal(g.get('D').in.get('B').sol, 0.25);
  assert.equal(g.has('X'), false, 'a transfer with no destination is not an edge');
  assert.equal(g.has('Z'), false, 'a self-transfer is not an edge');
  assert.deepEqual(buildAdjacencyList(null), new Map());
});

test('findSharedFunders walks back the depth it was given', () => {
  // F paid M, M paid A and B. F is two hops behind both wallets.
  const transfers = [
    { from: 'F', to: 'M', sol: 5 },
    { from: 'M', to: 'A', sol: 1 },
    { from: 'M', to: 'B', sol: 1 },
    { from: 'Q', to: 'C', sol: 1 },
  ];

  const one = findSharedFunders(['A', 'B', 'C'], 1, transfers);
  assert.equal(one.funders.length, 1, 'at one hop only M is shared');
  assert.equal(one.funders[0].funder, 'M');
  assert.equal(one.overlapPct, 66.67);

  const two = findSharedFunders(['A', 'B', 'C'], 2, transfers);
  const names = two.funders.map((f) => f.funder).sort();
  assert.deepEqual(names, ['F', 'M']);
  assert.equal(two.funders.find((f) => f.funder === 'F').hops, 2);
  assert.deepEqual(two.linkedWallets.sort(), ['A', 'B']);
  assert.equal(two.overlapPct, 66.67);
  assert.equal(two.pairs.length, 2, 'one pair per shared funder');
});

test('an exchange that funds everybody is reported, not counted', () => {
  const transfers = [];
  for (let i = 0; i < 40; i++) transfers.push({ from: 'EXCHANGE', to: `U${i}`, sol: 1 });
  const r = findSharedFunders(['U1', 'U2', 'U3'], 1, transfers);
  assert.equal(r.funders.length, 1);
  assert.equal(r.funders[0].hub, true);
  assert.equal(r.overlapPct, 0, 'a hub proves nothing about who knows whom');
  assert.deepEqual(r.pairs, []);

  // Raise the bar and the same graph reads as a syndicate again.
  const strict = findSharedFunders(['U1', 'U2', 'U3'], 1, transfers, { hubDegree: 100 });
  assert.equal(strict.funders[0].hub, false);
  assert.equal(strict.overlapPct, 100);
});

test('findSharedFunders returns nothing rather than throwing on empty input', () => {
  assert.equal(findSharedFunders([], 2, []).overlapPct, 0);
  assert.equal(findSharedFunders(['A'], 2, [{ from: 'F', to: 'A', sol: 1 }]).overlapPct, 0);
  assert.equal(findSharedFunders(null, 2, null).funders.length, 0);
});

test('shared funding turns an unremarkable launch into a syndicate', () => {
  const coin = {
    mint: 'funded',
    creator: CREATOR,
    who: [
      { w: addr(1), in: 0.4137, out: 0, n: 1, at: 0.4 },
      { w: addr(2), in: 1.2201, out: 0, n: 1, at: 0.9 },
      { w: addr(3), in: 0.7734, out: 0, n: 1, at: 1.5 },
      { w: addr(4), in: 2.0512, out: 0, n: 1, at: 2.1 },
      { w: addr(5), in: 0.3311, out: 0, n: 1, at: 2.6 },
      { w: addr(6), in: 1.9004, out: 0, n: 1, at: 2.9 },
    ],
  };

  const blind = analyzeLaunch(coin);
  assert.equal(blind.signals.funding.available, false);
  assert.equal(blind.syndicate_size, 0);
  assert.ok(blind.confidence_score < 0.2, 'nothing visible on size or timing');

  const transfers = [
    { from: 'FUNDER', to: addr(1), sol: 1 },
    { from: 'FUNDER', to: addr(2), sol: 1 },
    { from: 'FUNDER', to: addr(3), sol: 1 },
    { from: 'FUNDER', to: addr(4), sol: 1 },
    { from: 'ELSEWHERE', to: addr(5), sol: 1 },
  ];
  const seen = analyzeLaunch(coin, { transfers });
  assert.equal(seen.signals.funding.available, true);
  assert.equal(seen.signals.funding.overlap_pct, 66.67);
  assert.equal(seen.syndicate_size, 4);
  assert.equal(seen.largest_cluster, 4);
  assert.ok(seen.risk_tags.includes('SHARED_FUNDER'));
  assert.equal(isSyndicate(seen), true);

  // Passing a pre-built graph has to give the same answer as raw transfers.
  const viaGraph = analyzeLaunch(coin, { adjacency: buildAdjacencyList(transfers) });
  assert.equal(viaGraph.confidence_score, seen.confidence_score);
  assert.equal(viaGraph.syndicate_size, seen.syndicate_size);
});

// ---------------------------------------------------------------------------
// The exported helpers
// ---------------------------------------------------------------------------

test('sizingEntropy runs from all-the-same to all-different', () => {
  assert.equal(sizingEntropy([1, 1, 1, 1]), 0);
  assert.equal(sizingEntropy([0.1, 0.7, 2.4, 9]), 1);
  assert.equal(sizingEntropy([1]), 1, 'one buyer tells you nothing');
  assert.equal(sizingEntropy([]), 1);
  assert.ok(sizingEntropy([1, 1, 1, 5]) > 0);
  assert.ok(sizingEntropy([1, 1, 1, 5]) < sizingEntropy([1, 1, 5, 9]));
  // Jitter inside the tolerance is not variety.
  assert.equal(sizingEntropy([1, 1.005, 1.01]), 0);
});

test('isSyndicate takes the caller threshold and survives junk', () => {
  const r = analyzeLaunch(snipe);
  assert.equal(isSyndicate(r, 0.99), false);
  assert.equal(isSyndicate(r, 0.5), true);
  assert.equal(isSyndicate(null), false);
  assert.equal(isSyndicate({}), false);
});

test('getSyndicateExposure reports SOL and its share of the opening', () => {
  const r = analyzeLaunch(snipe);
  const e = getSyndicateExposure(r);
  assert.equal(e.wallets, 5);
  assert.equal(e.largest_cluster, 5);
  assert.equal(e.clusters, 1);
  assert.equal(e.sol, round(0.7331 * 5, 4));
  // Five of the 3.8765 SOL opening, the sixth wallet being the rest.
  assert.equal(e.pct, round((0.7331 * 5) / (0.7331 * 5 + 0.211) * 100));

  const none = getSyndicateExposure(analyzeLaunch(organic));
  assert.deepEqual(none, { sol: 0, pct: 0, wallets: 0, largest_cluster: 0, clusters: 0 });
  assert.deepEqual(getSyndicateExposure(null), {
    sol: 0, pct: 0, wallets: 0, largest_cluster: 0, clusters: 0,
  });
});

// ---------------------------------------------------------------------------
// Shape, purity and bad input
// ---------------------------------------------------------------------------

test('the analyser never touches what it was given', () => {
  const before = JSON.stringify(snipe);
  const transfers = [{ from: 'FUNDER', to: addr(1), sol: 1 }];
  const copy = JSON.stringify(transfers);
  analyzeLaunch(snipe, { transfers });
  assert.equal(JSON.stringify(snipe), before);
  assert.equal(JSON.stringify(transfers), copy);
});

test('the same launch analysed twice gives the same report', () => {
  assert.deepEqual(analyzeLaunch(snipe), analyzeLaunch(snipe));
});

test('rows from an RPC parser are read as well as the watcher rows', () => {
  const rpc = {
    mint: 'rpc',
    creator: CREATOR,
    participants: [
      { address: addr(1), sol: 0.7331, tx_count: 1, at: 1.5 },
      { address: addr(2), sol: 0.7331, tx_count: 1, at: 1.5 },
      { address: addr(3), sol: 0.7331, tx_count: 1, at: 1.5 },
    ],
  };
  const r = analyzeLaunch(rpc);
  assert.equal(r.syndicate_size, 3);
  assert.ok(r.risk_tags.includes('IDENTICAL_SIZING'));
});

test('nonsense in gives an empty report out, not an exception', () => {
  for (const bad of [null, undefined, {}, { who: null }, { who: 'nope' }, { who: [null, {}, 7] }]) {
    const r = analyzeLaunch(bad);
    assert.equal(r.confidence_score, 0);
    assert.equal(r.syndicate_size, 0);
    assert.ok(Array.isArray(r.risk_tags));
  }
});

test('too few buyers is reported as too few, not as innocence', () => {
  const r = analyzeLaunch({
    mint: 'thin',
    creator: CREATOR,
    who: [
      { w: addr(1), in: 4.4444, out: 0, n: 1, at: 0.5 },
      { w: addr(2), in: 4.4444, out: 0, n: 1, at: 0.5 },
    ],
  });
  assert.equal(r.thin, true);
  assert.ok(r.risk_tags.includes('INSUFFICIENT_DATA'));
  assert.ok(r.confidence_score <= 0.25, 'a thin launch cannot claim certainty');
  assert.equal(isSyndicate(r), false);
});

test('options move the window and the cap', () => {
  const late = {
    mint: 'late',
    creator: CREATOR,
    who: [
      { w: addr(1), in: 0.8123, out: 0, n: 1, at: 7.2 },
      { w: addr(2), in: 0.8123, out: 0, n: 1, at: 7.2 },
      { w: addr(3), in: 0.8123, out: 0, n: 1, at: 7.21 },
    ],
  };
  assert.equal(analyzeLaunch(late).window.participants, 0, 'all of it is past 3s');
  const wide = analyzeLaunch(late, { windowSec: 10 });
  assert.equal(wide.window.participants, 3);
  assert.equal(wide.syndicate_size, 3);
  assert.equal(analyzeLaunch(late, { windowSec: 10, maxWallets: 2 }).window.participants, 2);
});

// ---------------------------------------------------------------------------
// The real corpus
// ---------------------------------------------------------------------------

function loadCoins(limit = 4000) {
  if (!fs.existsSync(DATA)) return [];
  const files = fs.readdirSync(DATA).filter((f) => /^coins-\d{4}-\d{2}-\d{2}\.jsonl$/.test(f)).sort();
  const out = [];
  for (const f of files) {
    for (const line of fs.readFileSync(path.join(DATA, f), 'utf8').split('\n')) {
      if (!line || out.length >= limit) break;
      try {
        out.push(JSON.parse(line));
      } catch {
        // A half-written final line is the watcher mid-append, not a failure.
      }
    }
  }
  return out;
}

test('every recorded launch analyses without breaking an invariant', (t) => {
  const coins = loadCoins();
  if (!coins.length) return t.skip('no data/coins-*.jsonl on this machine');

  const tags = new Set(RISK_TAGS);
  let flagged = 0;
  let withBuyers = 0;

  for (const coin of coins) {
    const r = analyzeLaunch(coin);
    const n = r.window.participants;

    assert.ok(r.confidence_score >= 0 && r.confidence_score <= 1, `${coin.mint}: ${r.confidence_score}`);
    assert.ok(r.syndicate_size <= n, `${coin.mint}: more clustered than present`);
    assert.ok(r.largest_cluster <= r.syndicate_size);
    assert.equal(r.clustered_wallets.length, r.syndicate_size);
    for (const tag of r.risk_tags) assert.ok(tags.has(tag), `unknown tag ${tag}`);

    const exposure = getSyndicateExposure(r);
    assert.ok(exposure.pct >= 0 && exposure.pct <= 100.001, `${coin.mint}: ${exposure.pct}%`);
    assert.ok(exposure.sol <= r.window.sol_in + 1e-6, `${coin.mint}: exposure over the opening`);

    // Every clustered wallet must be one of the wallets actually in the window.
    const present = new Set(r.participants.map((p) => p.address));
    for (const w of r.clustered_wallets) assert.ok(present.has(w.address));

    if (n > 0) withBuyers++;
    if (isSyndicate(r)) flagged++;
  }

  // The point of the threshold is that it separates. If the whole corpus trips
  // it, it is measuring the market rather than the launch.
  const rate = flagged / Math.max(1, withBuyers);
  assert.ok(rate < 0.2, `${(rate * 100).toFixed(1)}% of launches flagged — the bar is too low`);
  t.diagnostic(`${coins.length} launches, ${withBuyers} with opening buyers, ${flagged} flagged (${(rate * 100).toFixed(1)}%)`);
});

test('the loudest launches in the corpus are ones a person would also call', (t) => {
  const coins = loadCoins();
  if (!coins.length) return t.skip('no data/coins-*.jsonl on this machine');

  const ranked = coins
    .map((c) => ({ coin: c, r: analyzeLaunch(c) }))
    .filter((x) => x.r.window.participants >= 3)
    .sort((a, b) => b.r.confidence_score - a.r.confidence_score)
    .slice(0, 10);

  if (!ranked.length) return t.skip('no launch in the corpus had three opening buyers');

  for (const { coin, r } of ranked) {
    // Anything at the top has to have a stated reason and a named cluster —
    // a high number with nothing behind it is the failure mode worth catching.
    assert.ok(r.reasons.length > 0, `${coin.mint} scored ${r.confidence_score} with no reason given`);
    if (r.confidence_score >= 0.75) {
      assert.ok(r.syndicate_size >= 2, `${coin.mint} is a syndicate of ${r.syndicate_size}`);
      assert.ok(
        r.risk_tags.some((tg) => tg === 'IDENTICAL_SIZING' || tg === 'NEAR_IDENTICAL_SIZING' || tg === 'SHARED_FUNDER'),
        `${coin.mint} cleared the bar on timing alone`,
      );
    }
  }
  t.diagnostic(`top score ${ranked[0].r.confidence_score} on ${ranked[0].coin.mint} — ${ranked[0].r.risk_tags.join(', ')}`);
});

function round(n, dp = 2) {
  const f = 10 ** dp;
  return Math.round(n * f) / f;
}
