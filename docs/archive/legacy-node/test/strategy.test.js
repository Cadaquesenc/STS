// The strategy definitions in backtest.js — chiefly syndicate-sniper, its entry
// gate, and the exits it hands the replay engine.
//
// (backtest.test.js covers the engine's arithmetic; cluster.test.js covers the
// analyser. This file only asks whether the rule accepts what it should, refuses
// what it should, and leaves where it said it would.)
//
// The fixtures are built backwards from the answers they are supposed to
// produce, the same way the e2e suite builds them, and every expected number
// below was worked out by hand rather than copied off a run.
//
// The deployer-dump exit in particular has to be tested this way rather than
// against the corpus. Placing the moment of a dump needs per-second candles;
// only 96 of the 3,324 recorded coins carry them, and of the 36 that clear the
// entry gate just two do — both of which stop out before the dump second is
// reached. Replaying the corpus therefore exercises the rule zero times and
// comes out identical with it on and off, so every dump test below builds the
// situation by hand and says what it built.
//
//   node --test test/
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  runBacktest, simulateExit, pathOf,
  syndicateGate, syndicateSniper, syndicateSniperNoDump, syndicateSniperStrategy,
  syndicateSniperV1, coordinatedCohort,
  creatorDumpSecond, launchSize, basicMomentum,
  PRIMARY_SIGNALS, MIN_CLUSTER_SCORE, GATE_REASONS, STRATEGIES,
  MIN_BUNDLE_WALLETS, BUNDLE_SIZE_TOLERANCE, MIN_BUNDLE_SOL,
  DEFAULT_EXIT, OBSERVED_HOLD_SEC,
} from '../src/backtest.js';
import { analyzeLaunch } from '../src/cluster.js';
import { DEFAULTS as WATCH_DEFAULTS } from '../src/watch.js';

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const T0 = 1786554000000;
const w = (n) => `Wa11et${String(n).padStart(2, '0')}${'x'.repeat(30)}`;
const DEV_SYNDI = `DevSynd1cate${'a'.repeat(32)}`;
const DEV_CLEAN = `DevOrgan1c${'b'.repeat(34)}`;
const DEV_QUEUE = `DevQueue${'e'.repeat(36)}`;
const DEV_TINY = `DevT1ny${'f'.repeat(37)}`;
const DEV_SOLO = `DevSo1o${'g'.repeat(37)}`;
const ENTRY = 0.001;

/**
 * The SYNDI fixture from the e2e suite: five wallets, one odd amount each, all
 * inside the same instant, deployer among them. Every size and timing signal
 * fires at once, and the price clears the 1.5x target in the second after entry.
 */
const syndicate = () => ({
  t: T0,
  mint: 'SyndicateM1nt1111111111111111111111111111111',
  symbol: 'SYNDI',
  creator: DEV_SYNDI,
  supply: 1_000_000_000,
  initialBuySol: 0.777,
  open: { seconds: 3, wallets: 5, sellers: 0, solIn: 3.885, solOut: 0, trades: 5 },
  who: [
    { w: DEV_SYNDI, in: 0.777, out: 0, n: 1, at: 0.01 },
    { w: w(1), in: 0.777, out: 0, n: 1, at: 0.01 },
    { w: w(2), in: 0.777, out: 0, n: 1, at: 0.01 },
    { w: w(3), in: 0.777, out: 0, n: 1, at: 0.02 },
    { w: w(4), in: 0.777, out: 0, n: 1, at: 0.02 },
  ],
  outcome: {
    follow: 60, entry: ENTRY, peakMult: 1.6, endMult: 1.4, peakAtSec: 4,
    highs: [[4, 1.6]], lows: [],
  },
  market: {
    candleSeconds: 1,
    candles: [
      { s: 3, o: 0.001, h: 0.00101, l: 0.00099, c: 0.001, volume: 3.885, buys: 5, sells: 0 },
      // Low stays above the 0.85 stop; high clears the 1.5 target.
      { s: 4, o: 0.001, h: 0.0016, l: 0.00098, c: 0.0014, volume: 2, buys: 3, sells: 1 },
    ],
  },
});

/**
 * The CLEAN fixture: six wallets, six different amounts, spread across the
 * window, deployer not buying. Nothing here reads as coordination, and the price
 * breaks the 0.85 stop before it ever goes up.
 */
const organic = () => ({
  t: T0 + 60_000,
  mint: 'Organ1cM1nt22222222222222222222222222222222',
  symbol: 'CLEAN',
  creator: DEV_CLEAN,
  supply: 1_000_000_000,
  initialBuySol: null,
  open: { seconds: 3, wallets: 6, sellers: 1, solIn: 5.48, solOut: 0.2, trades: 8 },
  who: [
    { w: w(10), in: 0.31, out: 0, n: 1, at: 0.4 },
    { w: w(11), in: 1.7, out: 0, n: 2, at: 0.9 },
    { w: w(12), in: 0.05, out: 0, n: 1, at: 1.4 },
    { w: w(13), in: 2.4, out: 0.2, n: 2, at: 1.9 },
    { w: w(14), in: 0.9, out: 0, n: 1, at: 2.4 },
    { w: w(15), in: 0.12, out: 0, n: 1, at: 2.9 },
  ],
  outcome: {
    follow: 60, entry: ENTRY, peakMult: 1.02, endMult: 0.7, peakAtSec: 3,
    highs: [], lows: [[4, 0.8]],
  },
  market: {
    candleSeconds: 1,
    candles: [
      { s: 3, o: 0.001, h: 0.00102, l: 0.001, c: 0.001, volume: 5.48, buys: 6, sells: 1 },
      { s: 4, o: 0.001, h: 0.001, l: 0.0008, c: 0.0007, volume: 1, buys: 0, sells: 3 },
    ],
  },
});

/** Nobody bought, so there is nothing to read and no price to read it at. */
const unbuyable = () => ({
  t: T0 + 120_000,
  mint: 'NobodyBought3333333333333333333333333333333',
  symbol: 'THIN',
  creator: `DevQu1et${'c'.repeat(36)}`,
  supply: 1_000_000_000,
  initialBuySol: null,
  open: { seconds: 3, wallets: 0, sellers: 0, solIn: 0, solOut: 0, trades: 0 },
  who: [],
  outcome: { follow: 60, entry: null, peakMult: null, endMult: null, peakAtSec: null, highs: [], lows: [] },
  market: { candleSeconds: 1, candles: [] },
});

/**
 * Two wallets on the identical amount, with the deployer one of them. Every
 * primary signal a two-wallet launch can fire, fires — which is the point: it is
 * still under MIN_PARTICIPANTS, so it must be turned away for being unreadable
 * rather than for looking ordinary.
 */
const tooFewBuyers = () => ({
  t: T0 + 180_000,
  mint: 'TwoWa11ets44444444444444444444444444444444',
  symbol: 'DUO',
  creator: `DevSma11${'d'.repeat(36)}`,
  supply: 1_000_000_000,
  initialBuySol: 0.5,
  open: { seconds: 3, wallets: 2, sellers: 0, solIn: 1, solOut: 0, trades: 2 },
  who: [
    { w: `DevSma11${'d'.repeat(36)}`, in: 0.5, out: 0, n: 1, at: 0.01 },
    { w: w(20), in: 0.5, out: 0, n: 1, at: 0.01 },
  ],
  outcome: { follow: 60, entry: ENTRY, peakMult: 1.6, endMult: 1.5, peakAtSec: 4, highs: [[4, 1.6]], lows: [] },
});

const fixtures = () => [syndicate(), organic(), unbuyable(), tooFewBuyers()];

// ---------------------------------------------------------------------------
// The entry gate: what it takes
// ---------------------------------------------------------------------------

test('the gate accepts a launch that is plainly one operator', () => {
  const gate = syndicateGate(syndicate());

  assert.equal(gate.enter, true);
  assert.equal(gate.reason, 'accepted');
  assert.ok(gate.score >= MIN_CLUSTER_SCORE, `score ${gate.score} should clear ${MIN_CLUSTER_SCORE}`);
  assert.ok(
    gate.tags.some((t) => PRIMARY_SIGNALS.includes(t)),
    'acceptance must rest on a primary signal, not on the score alone',
  );
  assert.equal(gate.thin, false);
});

test('a high score with no primary signal behind it is not enough', () => {
  // The same launch, judged by a gate that recognises none of the signals it
  // fired. The score is untouched and still clears the threshold, so the only
  // thing that can turn this away is the primary-signal requirement itself.
  const gate = syndicateGate(syndicate(), { primarySignals: ['SHARED_FUNDER'] });

  assert.equal(gate.enter, false);
  assert.equal(gate.reason, 'no-primary-signal');
  assert.ok(gate.score >= MIN_CLUSTER_SCORE, 'the score was never the problem');
});

test('a primary signal under the score threshold is not enough either', () => {
  const gate = syndicateGate(syndicate(), { minScore: 0.95 });

  assert.equal(gate.enter, false);
  assert.equal(gate.reason, 'low-score');
  assert.ok(gate.tags.includes('CREATOR_BOUGHT_OWN'), 'the signal was there; the conviction was not');
});

// ---------------------------------------------------------------------------
// The entry gate: what it refuses
// ---------------------------------------------------------------------------

test('an organic launch is turned away for being ordinary', () => {
  const gate = syndicateGate(organic());

  assert.equal(gate.enter, false);
  assert.equal(gate.reason, 'low-score');
  assert.equal(gate.thin, false, 'six buyers is plenty to read — it simply read as nothing');
  assert.deepEqual(
    gate.tags.filter((t) => PRIMARY_SIGNALS.includes(t)),
    [],
    'no primary signal should fire on six wallets buying six different amounts',
  );
});

test('a launch nobody bought is refused for that, not for its score', () => {
  const gate = syndicateGate(unbuyable());

  assert.equal(gate.enter, false);
  assert.equal(gate.reason, 'no-opening-buys');
  assert.ok(gate.tags.includes('NO_OPENING_BUYS'));
});

test('a launch too thin to read is refused for that, not for its score', () => {
  const gate = syndicateGate(tooFewBuyers());

  assert.equal(gate.enter, false);
  assert.equal(gate.reason, 'thin');
  assert.equal(gate.thin, true);
  // The distinction this test exists for: the signals did fire. Had there been
  // a third wallet the launch would have been a candidate. It is refused for
  // being unreadable, and "we cannot see this" must not be filed under "we
  // looked and it was ordinary".
  assert.ok(gate.tags.some((t) => PRIMARY_SIGNALS.includes(t)));
});

test('every refusal names a reason the funnel knows about', () => {
  for (const record of fixtures()) {
    assert.ok(GATE_REASONS.includes(syndicateGate(record).reason));
  }
  assert.equal(syndicateGate(null).reason, 'unreadable');
  assert.equal(syndicateGate(undefined).reason, 'unreadable');
});

// ---------------------------------------------------------------------------
// v2: who the signal actually fired on
//
// A tag says a signal fired somewhere in the opening window. It does not say it
// fired on a group worth following, and the three fixtures below are the three
// ways v1 could be fooled: a bundle that is a queue, a group too small to move
// anything, and a deployer buying its own launch with nobody else in it.
//
// Every score below was worked out from cluster.js's weights by hand, the same
// way the rest of this file does it, so a change to those weights fails here
// with an arithmetic mismatch rather than passing quietly.
// ---------------------------------------------------------------------------

/**
 * Ten buyers. Six of them bought the identical odd amount, spread across the
 * window well apart; four different wallets raced into the same instant with
 * completely unrelated sizes. IDENTICAL_SIZING and SAME_INSTANT_BUNDLE both
 * fire, and they fire on disjoint sets of wallets — which is the case v1 read
 * as one syndicate.
 *
 * Scores 0.78: sizing 1.0 (a group of six, exact, not a round number), timing
 * 0.67 (four in a bundle, in the same instant, not the launch block), dev 0.41.
 */
const bundleQueue = () => ({
  t: T0 + 240_000,
  mint: 'QueueNotABund1e555555555555555555555555555',
  symbol: 'QUEUE',
  creator: DEV_QUEUE,
  supply: 1_000_000_000,
  open: { seconds: 3, wallets: 10, sellers: 0, solIn: 10.7026, solOut: 0, trades: 10 },
  who: [
    // The race: same instant, sizes nobody scripted.
    { w: DEV_QUEUE, in: 0.11, out: 0, n: 1, at: 0.5 },
    { w: w(40), in: 0.7, out: 0, n: 1, at: 0.505 },
    { w: w(41), in: 2.3, out: 0, n: 1, at: 0.51 },
    { w: w(42), in: 5, out: 0, n: 1, at: 0.515 },
    // The repeat: identical amounts, but seconds apart and never together.
    { w: w(43), in: 0.4321, out: 0, n: 1, at: 1 },
    { w: w(44), in: 0.4321, out: 0, n: 1, at: 1.4 },
    { w: w(45), in: 0.4321, out: 0, n: 1, at: 1.8 },
    { w: w(46), in: 0.4321, out: 0, n: 1, at: 2.2 },
    { w: w(47), in: 0.4321, out: 0, n: 1, at: 2.6 },
    { w: w(48), in: 0.4321, out: 0, n: 1, at: 3 },
  ],
  // Priced as a winner, so the test below is about the entry and not the exit.
  outcome: { follow: 60, entry: ENTRY, peakMult: 1.6, endMult: 1.5, peakAtSec: 4, highs: [[4, 1.6]], lows: [] },
});

/**
 * Six wallets, one script, one instant, and 0.26 SOL between them. Every signal
 * v1 asks for fires and the group is real — it is simply too small to be the
 * thing the price does next.
 */
const smallBundle = () => ({
  t: T0 + 300_000,
  mint: 'T1nyBund1e6666666666666666666666666666666',
  symbol: 'TINY',
  creator: DEV_TINY,
  supply: 1_000_000_000,
  open: { seconds: 3, wallets: 6, sellers: 0, solIn: 0.2592, solOut: 0, trades: 6 },
  who: [
    { w: DEV_TINY, in: 0.0432, out: 0, n: 1, at: 0.5 },
    { w: w(50), in: 0.0432, out: 0, n: 1, at: 0.503 },
    { w: w(51), in: 0.0432, out: 0, n: 1, at: 0.506 },
    { w: w(52), in: 0.0432, out: 0, n: 1, at: 0.509 },
    { w: w(53), in: 0.0432, out: 0, n: 1, at: 0.512 },
    { w: w(54), in: 0.0432, out: 0, n: 1, at: 0.515 },
  ],
  outcome: { follow: 60, entry: ENTRY, peakMult: 1.6, endMult: 1.5, peakAtSec: 4, highs: [[4, 1.6]], lows: [] },
});

/**
 * The deployer and two of its own wallets, near-identical sizes, landing inside
 * a tenth of a second — plus two ordinary buyers who are not part of it.
 *
 * Deliberately built so CREATOR_BOUGHT_OWN is the *only* primary tag: the sizes
 * differ enough to read as NEAR_IDENTICAL_SIZING rather than IDENTICAL_SIZING,
 * and the bundle is wide enough to read as SUB_SECOND_BUNDLE rather than
 * SAME_INSTANT_BUNDLE. Neither of those is a primary signal.
 *
 * The cost of building it that way is that it scores 0.30, well under the
 * default threshold, so the tests below lower `minScore` to zero. That is the
 * only way to put the solo-dev rule under test on its own — and it is also the
 * reason the rule rejects nothing on the recorded corpus, where the score test
 * gets to these launches first.
 */
const soloDevBundle = () => ({
  t: T0 + 360_000,
  mint: 'So1oDevOn1y777777777777777777777777777777',
  symbol: 'SOLO',
  creator: DEV_SOLO,
  supply: 1_000_000_000,
  open: { seconds: 3, wallets: 5, sellers: 0, solIn: 10.212, solOut: 0, trades: 5 },
  who: [
    { w: DEV_SOLO, in: 1, out: 0, n: 1, at: 0.5 },
    { w: w(60), in: 1.004, out: 0, n: 1, at: 0.55 },
    { w: w(61), in: 1.008, out: 0, n: 1, at: 0.6 },
    { w: w(62), in: 0.2, out: 0, n: 1, at: 1.5 },
    { w: w(63), in: 7, out: 0, n: 1, at: 2.5 },
  ],
  outcome: { follow: 60, entry: ENTRY, peakMult: 1.6, endMult: 1.5, peakAtSec: 4, highs: [[4, 1.6]], lows: [] },
});

test('the wallets that landed together have to be the wallets that matched sizes', () => {
  const record = bundleQueue();

  // Both tags are there, and the score is not the problem.
  const v1 = syndicateGate(record, { minBundleWallets: 0, bundleSizeTolerance: Infinity, minBundleSol: 0 });
  assert.equal(v1.enter, true, 'v1 read this as a syndicate');
  assert.ok(v1.tags.includes('IDENTICAL_SIZING'));
  assert.ok(v1.tags.includes('SAME_INSTANT_BUNDLE'));
  assert.ok(v1.score >= MIN_CLUSTER_SCORE, `score ${v1.score} clears ${MIN_CLUSTER_SCORE}`);

  // The bundle is four wallets on 0.11, 0.7, 2.3 and 5 SOL. No three of them
  // are within 1% of each other, so there is no group to follow.
  const gate = syndicateGate(record);
  assert.equal(gate.enter, false);
  assert.equal(gate.reason, 'mixed-sizing');
  assert.equal(gate.bundleWallets, 4, 'four did land together');
  assert.equal(gate.cohortWallets, 1, 'but no two of them took the same position');
});

test('a group has to be at least three wallets before it is a group', () => {
  // SYNDI is five wallets on one amount, so it clears the default. Asked for six,
  // the same launch is refused — and refused for the size of the group rather
  // than for the sizes in it, which the funnel has to be able to tell apart.
  assert.equal(syndicateGate(syndicate()).enter, true);

  const strict = syndicateGate(syndicate(), { minBundleWallets: 6 });
  assert.equal(strict.enter, false);
  assert.equal(strict.reason, 'thin-bundle');
  assert.equal(strict.bundleWallets, 5);

  // And the default is the number the analyser needs to see a pattern at all.
  assert.equal(MIN_BUNDLE_WALLETS, 3);
});

test('the sizing tolerance is enforced across the bundle, not somewhere else in the launch', () => {
  const gate = syndicateGate(syndicate());

  // All five of SYNDI's wallets bought 0.777 and landed in the same instant, so
  // the whole bundle is the group and the spread across it is nothing.
  assert.equal(gate.cohortWallets, 5);
  assert.equal(gate.cohortSol, 3.885);
  assert.equal(gate.cohortDeltaPct, 0);

  // Widening the tolerance cannot rescue QUEUE below 1%, and narrowing it below
  // what SYNDI's wallets actually did would drop SYNDI too.
  assert.equal(BUNDLE_SIZE_TOLERANCE, 0.01);
  assert.equal(syndicateGate(bundleQueue(), { bundleSizeTolerance: 0.5 }).reason, 'mixed-sizing');
  assert.equal(syndicateGate(bundleQueue(), { bundleSizeTolerance: 100 }).enter, true, 'with no sizing test at all it is v1 again');
});

test('a coordinated group that committed almost nothing is not worth following out', () => {
  const record = smallBundle();

  const gate = syndicateGate(record);
  assert.equal(gate.enter, false);
  assert.equal(gate.reason, 'small-bundle');
  assert.equal(gate.cohortWallets, 6, 'the group is real');
  assert.equal(gate.cohortSol, 0.2592, 'it is just 0.26 SOL of it');

  // The threshold is the only thing rejecting it: drop the bar under what they
  // committed and the same launch is taken.
  assert.equal(syndicateGate(record, { minBundleSol: 0.2 }).enter, true);
  assert.equal(MIN_BUNDLE_SOL, 1.5);
});

test('the deployer buying its own launch alone is not a syndicate', () => {
  // Scored at zero so the solo-dev rule is the only thing that can speak — see
  // the fixture's note for why it cannot be reached at the default threshold.
  const open = { minScore: 0 };
  const record = soloDevBundle();

  const primaries = syndicateGate(record, open).tags.filter((t) => PRIMARY_SIGNALS.includes(t));
  assert.deepEqual(primaries, ['CREATOR_BOUGHT_OWN'], 'the fixture only fires the weakest primary');

  const gate = syndicateGate(record, open);
  assert.equal(gate.enter, false);
  assert.equal(gate.reason, 'solo-dev');
  assert.equal(gate.cohortWallets, 3, 'three wallets did land together on one size');
  assert.equal(gate.cohortExternal, 2, 'but only two of them were somebody else');

  // Two ways the same launch becomes a trade: stop asking for outside wallets,
  // or have three of them.
  assert.equal(syndicateGate(record, { ...open, requireExternalBundle: false }).enter, true);

  const joined = soloDevBundle();
  joined.who.push({ w: w(64), in: 1.006, out: 0, n: 1, at: 0.58 });
  const withOutsiders = syndicateGate(joined, open);
  assert.equal(withOutsiders.cohortExternal, 3);
  assert.equal(withOutsiders.enter, true);
});

test('a launch with another primary signal is not judged as a solo dev buy', () => {
  // SYNDI's deployer also bought its own coin. It is not a solo dev buy, because
  // CREATOR_BOUGHT_OWN is not the only thing that fired.
  const gate = syndicateGate(syndicate());
  assert.ok(gate.tags.includes('CREATOR_BOUGHT_OWN'));
  assert.ok(gate.tags.filter((t) => PRIMARY_SIGNALS.includes(t)).length > 1);
  assert.equal(gate.enter, true);
});

// ---------------------------------------------------------------------------
// The group itself
// ---------------------------------------------------------------------------

test('the cohort is the widest matching group in the bundle, not the first one', () => {
  // Four wallets, sorted: 1.000, 1.005, 1.0105, 1.0115. Anchored on the smallest
  // the run breaks at 1.0105 and finds two; the answer is the three at the top,
  // which span 0.65%. This is why it is a window and not a sweep.
  const report = {
    creator: 'DEV',
    signals: { timing: { bundles: [{
      wallets: 4, at: 0.5, span: 0.01, same_instant: true, sol: 4.027,
      members: ['DEV', 'A', 'B', 'C'],
    }] } },
    participants: [
      { address: 'DEV', sol_spent: 1, at: 0.5 },
      { address: 'A', sol_spent: 1.005, at: 0.5 },
      { address: 'B', sol_spent: 1.0105, at: 0.5 },
      { address: 'C', sol_spent: 1.0115, at: 0.5 },
    ],
  };

  const cohort = coordinatedCohort(report);
  assert.equal(cohort.wallets, 3);
  assert.equal(cohort.sol, 3.027);
  assert.equal(cohort.external, 3, 'the deployer is not in the matching group');
  assert.equal(cohort.deltaPct, 0.647);
});

test('the cohort is read off the largest bundle', () => {
  const report = {
    creator: 'DEV',
    signals: { timing: { bundles: [
      { wallets: 3, at: 0.1, span: 0.01, sol: 3, members: ['A', 'B', 'C'] },
      { wallets: 4, at: 2, span: 0.01, sol: 8, members: ['D', 'E', 'F', 'G'] },
    ] } },
    participants: [
      { address: 'A', sol_spent: 1 }, { address: 'B', sol_spent: 1 }, { address: 'C', sol_spent: 1 },
      { address: 'D', sol_spent: 2 }, { address: 'E', sol_spent: 2 }, { address: 'F', sol_spent: 2 }, { address: 'G', sol_spent: 2 },
    ],
  };

  const cohort = coordinatedCohort(report);
  assert.equal(cohort.bundle.wallets, 4);
  assert.equal(cohort.wallets, 4);
  assert.equal(cohort.sol, 8);
});

test('a launch with no bundle at all has no cohort, and says so rather than throwing', () => {
  const empty = { bundle: null, wallets: 0, sol: 0, external: 0, sizeSol: null, deltaPct: null };
  assert.deepEqual(coordinatedCohort(analyzeLaunch(organic())), empty);
  assert.deepEqual(coordinatedCohort(analyzeLaunch(unbuyable())), empty);
  assert.deepEqual(coordinatedCohort(null), empty);
  assert.deepEqual(coordinatedCohort({}), empty);
  assert.deepEqual(coordinatedCohort({ signals: { timing: { bundles: [{ wallets: 3, at: 0, members: [] }] } } }).wallets, 0);
});

// ---------------------------------------------------------------------------
// v1 and v2, side by side
// ---------------------------------------------------------------------------

test('the v1 rule is still runnable, and is not the v2 rule', () => {
  assert.equal(STRATEGIES['syndicate-sniper-v1'], syndicateSniperV1);
  assert.notEqual(syndicateSniperV1, syndicateSniper);

  // Over the three launches v2 was tightened against, v1 takes all three and v2
  // takes none. Same coins, same engine, same costs.
  const records = [bundleQueue(), smallBundle(), soloDevBundle()];
  const v1 = runBacktest({ records, strategy: syndicateSniperV1 });
  const v2 = runBacktest({ records, strategy: syndicateSniper });

  assert.equal(v1.trades.length, 2, 'SOLO is under the score threshold even for v1');
  assert.equal(v2.trades.length, 0);
  assert.deepEqual(v2.trades.map((t) => t.symbol), []);

  // And the launch both rules agree on is still taken by both.
  assert.equal(runBacktest({ records: [syndicate()], strategy: syndicateSniperV1 }).trades.length, 1);
  assert.equal(runBacktest({ records: [syndicate()], strategy: syndicateSniper }).trades.length, 1);
});

test('every v2 refusal names a reason the funnel knows about', () => {
  for (const record of [...fixtures(), bundleQueue(), smallBundle(), soloDevBundle()]) {
    assert.ok(GATE_REASONS.includes(syndicateGate(record).reason));
    assert.ok(GATE_REASONS.includes(syndicateGate(record, { minScore: 0 }).reason));
  }
  // The reasons are ordered worst-first, and the group checks sit after the ones
  // that can be answered without looking at the group.
  assert.ok(GATE_REASONS.indexOf('no-primary-signal') < GATE_REASONS.indexOf('no-bundle'));
  assert.equal(GATE_REASONS.at(-1), 'accepted');
});

// ---------------------------------------------------------------------------
// Nullable fields
// ---------------------------------------------------------------------------

test('a missing initial buy or market cap is reported, never used to reject', () => {
  // CLEAN carries an explicit null initial buy, which is the common case: it is
  // absent on 97% of the recorded corpus.
  assert.equal(launchSize(organic()).initialBuySol, null);

  // The same launch that was accepted above, with both fields stripped, must
  // still be accepted. A number nobody wrote down is not evidence.
  const stripped = syndicate();
  stripped.initialBuySol = null;
  delete stripped.supply;

  const gate = syndicateGate(stripped);
  assert.equal(gate.enter, true, 'a coordinated launch stays coordinated when a field is missing');
  assert.equal(gate.initialBuySol, null);
  assert.equal(gate.marketCapSol, null);
});

test('both spellings of the nullable fields are read, and the store wins', () => {
  // The archive writes initialBuySol; the SQLite store writes initial_buy_sol
  // and market_cap. Both have to arrive at the same answer.
  assert.deepEqual(launchSize({ initialBuySol: 0.5 }).initialBuySol, 0.5);
  assert.deepEqual(launchSize({ initial_buy_sol: 0.5 }).initialBuySol, 0.5);

  // With no stated cap, supply times entry price stands in for one.
  assert.equal(launchSize({ supply: 1_000_000_000, outcome: { entry: 3e-8 } }).marketCapSol, 30);
  // With one stated, it is preferred over the arithmetic.
  assert.equal(launchSize({ market_cap: 41, supply: 1_000_000_000, outcome: { entry: 3e-8 } }).marketCapSol, 41);
});

test('junk in the nullable fields comes back as null rather than NaN', () => {
  for (const bad of [{}, { initialBuySol: 'n/a', supply: 'lots' }, { initialBuySol: undefined }, { supply: 0, outcome: { entry: 0 } }]) {
    const size = launchSize(bad);
    assert.equal(size.initialBuySol === null || Number.isFinite(size.initialBuySol), true);
    assert.equal(size.marketCapSol === null || Number.isFinite(size.marketCapSol), true);
  }
  assert.deepEqual(launchSize(null), { initialBuySol: null, marketCapSol: null });
});

test('a record that cannot be analysed is skipped, not thrown', () => {
  // A `who` that is not a list of anything the analyser understands.
  const nonsense = { mint: 'X', creator: 'C', who: [null, 42, 'wallet'], outcome: { entry: ENTRY } };
  assert.doesNotThrow(() => syndicateGate(nonsense));
  assert.equal(syndicateGate(nonsense).enter, false);

  const r = runBacktest({ records: [nonsense, syndicate()], strategy: syndicateSniper });
  assert.equal(r.trades.length, 1, 'the good record still trades');
});

// ---------------------------------------------------------------------------
// Exits
// ---------------------------------------------------------------------------

test('a coordinated launch that runs hits the target, and is paid at the target', () => {
  const r = runBacktest({ records: [syndicate()], strategy: syndicateSniper });

  assert.equal(r.trades.length, 1);
  const [trade] = r.trades;
  assert.equal(trade.reason, 'target');
  assert.equal(trade.grossMult, 1.5, 'a target fills at the target, not at the 1.6x high behind it');
  assert.ok(trade.pnlSol > 0);
});

test('a coordinated launch that breaks hits the stop, and is paid at the stop', () => {
  // SYNDI's wallets on CLEAN's price path: accepted on the same evidence as
  // above, so the only thing under test here is which exit fires.
  const breaks = syndicate();
  breaks.outcome = organic().outcome;
  breaks.market = organic().market;

  const r = runBacktest({ records: [breaks], strategy: syndicateSniper });

  assert.equal(r.trades.length, 1);
  const [trade] = r.trades;
  assert.equal(trade.reason, 'stop');
  assert.equal(trade.grossMult, 0.85);
  assert.ok(trade.pnlSol < 0);
});

test('the stop is read before the target when one second holds both', () => {
  // A second whose low is under the stop and whose high is over the target. The
  // pessimistic reading is the stop, and a strategy must not be able to opt out
  // of it by naming its own exits.
  const both = syndicate();
  both.market = {
    candleSeconds: 1,
    candles: [{ s: 3, o: 0.001, h: 0.002, l: 0.0008, c: 0.001, volume: 1, buys: 1, sells: 1 }],
  };
  both.outcome = { ...both.outcome, highs: [], lows: [], peakMult: 2, peakAtSec: 3, endMult: 1 };

  const r = runBacktest({ records: [both], strategy: syndicateSniper });
  assert.equal(r.trades[0].reason, 'stop');
});

test('a position still open when the recording stopped is not a trade', () => {
  // Watched for ten seconds under a sixty-second hold, and it neither ran nor
  // broke. What it did next is not in the data, so there is no trade to report.
  const cut = syndicate();
  cut.outcome = {
    follow: 10, entry: ENTRY, peakMult: 1.1, endMult: 1.05, peakAtSec: 5,
    highs: [[5, 1.1]], lows: [[8, 0.95]],
  };
  delete cut.market;

  const r = runBacktest({ records: [cut], strategy: syndicateSniper });
  assert.equal(r.trades.length, 0);
  assert.equal(r.skipped.unobserved, 1);
});

test('the clock exit fires once the hold has been watched all the way out', () => {
  const dawdle = syndicate();
  dawdle.outcome = {
    follow: 60, entry: ENTRY, peakMult: 1.1, endMult: 1.05, peakAtSec: 5,
    highs: [[5, 1.1]], lows: [[8, 0.95]],
  };
  delete dawdle.market;

  const r = runBacktest({ records: [dawdle], strategy: syndicateSniper });
  assert.equal(r.trades.length, 1);
  assert.equal(r.trades[0].reason, 'time');
});

// ---------------------------------------------------------------------------
// Leaving when the deployer leaves
// ---------------------------------------------------------------------------

test('the deployer dump exit needs both a sale and candles to place it', () => {
  // SYNDI's deployer never sold.
  assert.equal(creatorDumpSecond(syndicate()), null);

  // Now it did, but the record has no candles, so the moment cannot be placed.
  const sold = syndicate();
  sold.who[0].out = 0.9;
  delete sold.market;
  assert.equal(creatorDumpSecond(sold), null, 'a sale with no candles is not a timestamp');

  // With candles, the first heavy-selling second after entry is the stand-in.
  const placeable = syndicate();
  placeable.who[0].out = 0.9;
  placeable.market.candles = [
    { s: 3, o: 0.001, h: 0.00101, l: 0.00099, c: 0.001, volume: 3.885, buys: 5, sells: 0 },
    { s: 4, o: 0.001, h: 0.00105, l: 0.00099, c: 0.001, volume: 2, buys: 1, sells: 6 },
  ];
  // The candle is the launch's fourth second; the entry was fixed at the third.
  // What comes back is the hold — one second in — because that is the clock the
  // exit is compared against.
  assert.equal(creatorDumpSecond(placeable), 1);
});

test('the dump exit leaves at the market price, and only where the sniper uses it', () => {
  // Priced so that nothing else can claim the trade: the whole path sits between
  // the 0.85 stop and the 1.5 target, and the 60s hold is never reached.
  const dumped = syndicate();
  dumped.who[0].out = 0.9;
  dumped.market.candles = [
    { s: 3, o: 0.001, h: 0.00101, l: 0.00099, c: 0.001, volume: 3.885, buys: 5, sells: 0 },
    { s: 4, o: 0.001, h: 0.0011, l: 0.0009, c: 0.00095, volume: 2, buys: 1, sells: 6 },
  ];
  dumped.outcome = { ...dumped.outcome, highs: [], lows: [], peakMult: 1.1, peakAtSec: 4, endMult: 0.95 };

  const r = runBacktest({ records: [dumped], strategy: syndicateSniper });
  assert.equal(r.trades.length, 1);
  assert.equal(r.trades[0].reason, 'dump');
  // The launch's fourth second, one second after the entry was fixed — and
  // `holdSec` is a hold, so it is the one, not the four.
  assert.equal(r.trades[0].holdSec, 1);
  // The fill is the low of that second — 0.0009 / 0.001 — because leaving at
  // market on news is not a level anybody chose.
  assert.equal(r.trades[0].grossMult, 0.9);

  // The same coin, for the variant that ignores the deployer, runs to the clock.
  const without = runBacktest({ records: [dumped], strategy: syndicateSniperNoDump });
  assert.equal(without.trades[0].reason, 'time');
});

test('the entry second itself cannot be the dump — you are not in yet', () => {
  // The launch second is heavy selling, but the position is opened at the end
  // of the opening window, so that second is not a moment this rule can act on.
  const rec = syndicate();
  rec.who[0].out = 5;
  rec.market.candles = [
    { s: 3, o: 0.001, h: 0.001, l: 0.001, c: 0.001, volume: 3.9, buys: 0, sells: 9 },
    { s: 6, o: 0.001, h: 0.001, l: 0.00097, c: 0.00098, volume: 1, buys: 0, sells: 2 },
  ];
  // Second three is skipped for being the entry's own second, so the answer is
  // second six — three seconds into the hold, which is how it is reported.
  assert.equal(creatorDumpSecond(rec), 3);
});

test('a dump beats a target that had not been reached yet', () => {
  // Up to 1.4x and selling hard, with the target at 1.5x. The trade leaves on
  // the news at the price of the moment, and does not get to claim a target the
  // coin never touched.
  const rec = syndicate();
  rec.market.candles = [
    { s: 3, o: 0.001, h: 0.00101, l: 0.00099, c: 0.001, volume: 3.9, buys: 5, sells: 0 },
    { s: 5, o: 0.0012, h: 0.0014, l: 0.0012, c: 0.0013, volume: 4, buys: 0, sells: 5 },
  ];
  // The summary has to agree with the candles: pathOf puts back a recorded peak
  // the candles never saw, so leaving the fixture's 1.6x here would inject a
  // point above the target and the trade would exit there instead.
  rec.outcome = { follow: 60, entry: ENTRY, peakMult: 1.4, endMult: 1.3, peakAtSec: 5, highs: [], lows: [] };

  // The selling second is the launch's fifth and the entry was fixed at the
  // third, so as a hold the dump is at second two.
  const e = simulateExit(pathOf(rec), { takeProfit: 1.5, stopLoss: 0.85, maxHoldSec: 57, dumpAtSec: 2 });
  assert.equal(e.reason, 'dump');
  assert.equal(Math.round(e.mult * 1e4) / 1e4, 1.2, 'the low of that second, examined first');
});

test('an exit with no dump set behaves exactly as it did before', () => {
  // The guard on the whole feature: adding dumpAtSec must be inert for every
  // strategy that does not set it, including the two already merged.
  const path = pathOf(syndicate());
  const exit = { takeProfit: 1.5, stopLoss: 0.85, trailingStopPct: null, maxHoldSec: 60 };
  assert.deepEqual(simulateExit(path, { ...exit, dumpAtSec: null }), simulateExit(path, exit));
  assert.deepEqual(simulateExit(path, { ...exit, dumpAtSec: undefined }), simulateExit(path, exit));
});

test('an exit at a named second cannot rescue a position the stop already took', () => {
  // The dump is at second 4; the stop is broken at second 4 too. The stop wins,
  // which keeps the pessimistic reading the rest of the engine uses.
  const path = pathOf({
    outcome: {
      follow: 60, entry: 1, peakMult: 1, endMult: 0.5, peakAtSec: 0,
      highs: [], lows: [[4, 0.5]],
    },
  });
  const e = simulateExit(path, { takeProfit: 1.5, stopLoss: 0.85, maxHoldSec: 60, dumpAtSec: 4 });
  assert.equal(e.reason, 'stop');
});

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

test('the same coins replay to the same answer every time', () => {
  const runs = Array.from({ length: 5 }, () =>
    runBacktest({ records: fixtures(), strategy: syndicateSniper }));

  for (const r of runs.slice(1)) {
    assert.deepEqual(r.trades, runs[0].trades);
    assert.deepEqual(r.summary, runs[0].summary);
    assert.deepEqual(r.skipped, runs[0].skipped);
    assert.deepEqual(r.byFidelity, runs[0].byFidelity);
  }
});

test('the answer does not depend on the order the coins were handed over', () => {
  // The replay sorts by launch time, so a shuffled file is the same account.
  const forwards = runBacktest({ records: fixtures(), strategy: syndicateSniper });
  const backwards = runBacktest({ records: fixtures().reverse(), strategy: syndicateSniper });

  assert.deepEqual(backwards.trades, forwards.trades);
  assert.deepEqual(backwards.summary, forwards.summary);
});

test('the gate is pure — reading a record does not change what it says', () => {
  const record = syndicate();
  const before = JSON.stringify(record);
  const first = syndicateGate(record);
  const second = syndicateGate(record);

  assert.deepEqual(second, first, 'a cached report must not drift from a fresh one');
  assert.equal(JSON.stringify(record), before, 'the record itself is untouched');
});

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

test('the documented defaults are the defaults', () => {
  assert.equal(MIN_CLUSTER_SCORE, 0.6);
  assert.deepEqual(PRIMARY_SIGNALS, ['IDENTICAL_SIZING', 'SAME_INSTANT_BUNDLE', 'CREATOR_BOUGHT_OWN']);
  assert.equal(syndicateSniper.exit.takeProfit, 1.5);
  assert.equal(syndicateSniper.exit.stopLoss, 0.85);
  assert.equal(syndicateSniper.exit.maxHoldSec, OBSERVED_HOLD_SEC);
});

test('the default hold is exactly what the recorder observes of a position', () => {
  // The reason this is asserted rather than trusted: the two numbers live in
  // different files, and if watch.js ever follows a coin for longer or judges its
  // opening later, a hold of 57 silently becomes either short or unanswerable.
  // Unanswerable is the dangerous one — it drops every coin that did not reach a
  // level and leaves a sample selected on its own outcome.
  assert.equal(OBSERVED_HOLD_SEC, WATCH_DEFAULTS.follow - WATCH_DEFAULTS.seconds);
  assert.equal(DEFAULT_EXIT.maxHoldSec, OBSERVED_HOLD_SEC);

  // And the arithmetic it protects: a coin followed for `follow` seconds, entered
  // at `seconds`, has exactly this much of it left to observe.
  const rec = syndicate();
  rec.outcome = { ...rec.outcome, follow: WATCH_DEFAULTS.follow };
  assert.equal(pathOf(rec).observedSec, OBSERVED_HOLD_SEC);
});

test('the strategy explains itself with the same verdict it trades on', () => {
  // The funnel the runner prints has to be the rule that actually traded, not a
  // second opinion that happens to sit next to it.
  for (const record of fixtures()) {
    assert.equal(!!syndicateSniper.shouldEnter(record), syndicateSniper.explain(record).enter);
  }
});

test('both syndicate rules are reachable by name, and are not the same rule', () => {
  assert.equal(STRATEGIES['syndicate-sniper'], syndicateSniper);
  assert.equal(STRATEGIES['syndicate-sniper-no-dump'], syndicateSniperNoDump);
  assert.equal(STRATEGIES['basic-momentum'], basicMomentum);
  assert.notEqual(syndicateSniper.exit.dumpAtSec, 0);
  assert.equal(typeof syndicateSniper.explain, 'function');
});

test('the sniper is stricter than the momentum baseline it has to beat', () => {
  // Over the same four coins: momentum buys anything with buyers, the sniper
  // buys the one launch that was demonstrably run by one person.
  const records = fixtures();
  const sniper = runBacktest({ records, strategy: syndicateSniper });
  const momentum = runBacktest({ records, strategy: basicMomentum });

  assert.equal(sniper.trades.length, 1);
  assert.equal(sniper.trades[0].symbol, 'SYNDI');
  assert.ok(momentum.trades.length > sniper.trades.length);
});

test('a hand-tuned sniper keeps its name and applies its own threshold', () => {
  const strict = syndicateSniperStrategy({ name: 'sniper-strict', minScore: 0.95 });
  assert.equal(strict.name, 'sniper-strict');

  // SYNDI scores 0.77: enough for the default gate, not enough for this one.
  assert.ok(syndicateSniper.shouldEnter(syndicate()));
  assert.equal(strict.shouldEnter(syndicate()), false);
  assert.equal(strict.explain(syndicate()).reason, 'low-score');
});

test('friction comes off every trade, so a flat exit still loses money', () => {
  // A coin that goes nowhere and closes exactly where it opened. Gross, that is
  // a break-even trade; net, it is the round trip, and the report must show the
  // second number.
  const flat = syndicate();
  flat.outcome = { follow: 60, entry: ENTRY, peakMult: 1, endMult: 1, peakAtSec: 0, highs: [], lows: [] };
  delete flat.market;

  const r = runBacktest({ records: [flat], strategy: syndicateSniper });
  assert.equal(r.trades[0].grossMult, 1, 'it closed where it opened');
  assert.ok(r.trades[0].pnlSol < 0, 'and still lost, because a round trip is not free');

  // 150 bps a leg plus 0.005 SOL a leg on a 0.5 SOL position: the engine charges
  // roughly 5% a round trip, which is the top of the realistic band.
  assert.ok(r.summary.simulatedCostPct > 3 && r.summary.simulatedCostPct < 5.5);
});
