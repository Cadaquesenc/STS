// The backtester's own arithmetic, checked by hand.
//
// Every expected number in this file was worked out on paper from the inputs
// above it, not copied from a run. A test that records whatever the code did is
// a test that will happily record the day it starts being wrong.
//
//   node --test test/
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  runBacktest, simulateExit, pathOf, maxDrawdown, summarise,
  buyEverything, buyNothing, DEFAULT_EXIT,
} from '../src/backtest.js';

// A coin with a ladder: rose to 1.6x by 20s, fell to 0.6x by 40s, closed at 0.7x.
function coin({
  mint = 'M1', t = 1_000_000, entry = 1e-6, follow = 60,
  highs = [[10, 1.2], [20, 1.6]], lows = [[40, 0.6]],
  peakMult = 1.6, endMult = 0.7, peakAtSec = 20,
  open = { wallets: 10, sellers: 2, solIn: 5 },
} = {}) {
  return { mint, symbol: mint, t, open, outcome: { follow, entry, highs, lows, peakMult, endMult, peakAtSec } };
}

/** Enters everything, with an exit rule the test names. */
const withExit = (exit) => ({ name: 'test', exit, shouldEnter: () => true });

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

test('a coin with no entry price has no path and is never traded', () => {
  assert.equal(pathOf({ outcome: { entry: null } }), null);
  assert.equal(pathOf({}), null);

  const r = runBacktest({ records: [coin({ entry: null })], strategy: buyEverything });
  assert.equal(r.trades.length, 0);
  assert.equal(r.skipped.noEntry, 1);
});

test('the ladder is replayed in the order it happened', () => {
  const p = pathOf(coin());
  assert.equal(p.fidelity, 'ladder');
  const secs = p.points.map((x) => x.sec);
  assert.deepEqual(secs, [...secs].sort((a, b) => a - b), 'points must be time-ordered');
  assert.equal(p.points[0].mult, 1, 'a path starts at the entry');
  assert.equal(p.points.at(-1).mult, 0.7, 'and ends at the close');
});

test('per-second candles beat the ladder, and read the low before the high', () => {
  const c = coin();
  c.market = { candleSeconds: 1, candles: [{ s: 5, o: 1e-6, h: 2e-6, l: 5e-7, c: 1e-6 }] };
  const p = pathOf(c);
  assert.equal(p.fidelity, 'candles');
  const at5 = p.points.filter((x) => x.sec === 5).map((x) => x.mult);
  assert.deepEqual(at5, [0.5, 2, 1], 'low, high, close — the low is read before the high');
});

test('a clock exit inside a candle gets the close, not the second\'s best price', () => {
  // The high of the last second is 2x and the close is 1x. Sorting the points
  // by price would hand a time exit the 2x, which never happened at the end.
  const c = coin({ highs: [], lows: [], endMult: 1, peakMult: 2, peakAtSec: 5, follow: 5 });
  c.market = { candleSeconds: 1, candles: [{ s: 5, o: 1e-6, h: 2e-6, l: 9e-7, c: 1e-6 }] };
  const e = simulateExit(pathOf(c), { takeProfit: 9, stopLoss: 0.1, maxHoldSec: 5 });
  assert.equal(e.reason, 'time');
  assert.equal(e.mult, 1);
});

test('with only a peak and a close the path is coarse, and says so', () => {
  const p = pathOf(coin({ highs: [], lows: [] }));
  assert.equal(p.fidelity, 'coarse');
  assert.deepEqual(p.points.map((x) => x.mult), [1, 1.6, 0.7]);
});

test('a peak above everything the ladder kept is put back', () => {
  // watch.js stops appending after sixty turning points and freezes its running
  // extremes with them, so the ladder can end below the peak that happened.
  const p = pathOf(coin({ highs: [[10, 1.2]], peakMult: 4, peakAtSec: 15 }));
  assert.equal(Math.max(...p.points.map((x) => x.mult)), 4);
  const peak = p.points.find((x) => x.mult === 4);
  assert.equal(peak.sec, 15, 'restored at the second it really happened');
});

// The entry second. watch.js watches a coin for three seconds before it fixes an
// entry price, so a recording opens with seconds of trading that no position
// lived through. Reading them as price action is how a coin that ran up into its
// entry broke its stop before it was ever bought.
//
// The recorder stamps its seconds from the launch. Every second in a path, and in
// every exit rule read against one, is a hold counted from the entry — so these
// also check the subtraction between the two.

const opened = (seconds) => ({ seconds, wallets: 10, sellers: 2, solIn: 5 });

test('the seconds before the entry are not price action the position lived through', () => {
  // This coin cost 1e-6 to enter and traded at 0.3e-6 in its launch second — a
  // third of the entry price, under any stop worth setting. The stop must not
  // fire there, because there is nothing to stop out of until second three.
  const c = coin({
    highs: [], lows: [], peakMult: 1.2, endMult: 1.1, peakAtSec: 5, open: opened(3),
  });
  c.market = {
    candleSeconds: 1,
    candles: [
      { s: 0, o: 3e-7, h: 4e-7, l: 3e-7, c: 4e-7 },
      { s: 3, o: 1e-6, h: 1.1e-6, l: 1e-6, c: 1.05e-6 },
      { s: 5, o: 1.05e-6, h: 1.2e-6, l: 1.05e-6, c: 1.1e-6 },
    ],
  };
  const p = pathOf(c);
  assert.equal(p.points.every((x) => x.sec >= 0), true, 'no point predates the entry');
  assert.equal(Math.min(...p.points.map((x) => x.mult)), 1, 'and the 0.3x is not in it');

  const e = simulateExit(p, { takeProfit: 9, stopLoss: 0.85, maxHoldSec: 57 });
  assert.equal(e.reason, 'time', 'this coin never broke its stop after entry');
});

test('a path is counted from the entry, not from the launch', () => {
  // Entry fixed at the launch's third second, so the candle at its fifth is two
  // seconds into the hold — and the entry itself is second zero, because zero
  // seconds held is exactly when you have paid one times the entry price.
  const c = coin({
    highs: [], lows: [], peakMult: 1.2, endMult: 1.1, peakAtSec: 5, follow: 60, open: opened(3),
  });
  c.market = {
    candleSeconds: 1,
    candles: [{ s: 5, o: 1e-6, h: 1.2e-6, l: 1e-6, c: 1.1e-6 }],
  };
  const p = pathOf(c);
  assert.equal(p.points[0].sec, 0);
  assert.equal(p.points[0].mult, 1);
  assert.deepEqual(p.points.filter((x) => x.mult === 1.2).map((x) => x.sec), [2], 'the fifth second, two into the hold');
  assert.equal(p.entrySec, 3, 'and the path says where it was cut from');

  // A minute of following, three seconds of it before the entry: 57 observable.
  assert.equal(p.observedSec, 57);
  assert.equal(p.points.at(-1).sec, 57, 'the close lands at the end of what was watched');
});

test('a hold longer than the position was watched is unresolved, not a clock exit', () => {
  // The three seconds before the entry are the whole point: a coin followed for
  // sixty seconds and entered at three was only ever held for fifty-seven, so a
  // sixty-second hold is a question this recording cannot answer. It used to be
  // answered anyway, because the hold was being measured from the launch.
  const c = coin({ highs: [], lows: [], peakMult: 1.02, endMult: 1.01, peakAtSec: 5, follow: 60, open: opened(3) });
  const flat = { takeProfit: 9, stopLoss: 0.1, trailingStopPct: null };

  const asked57 = simulateExit(pathOf(c), { ...flat, maxHoldSec: 57 });
  assert.equal(asked57.reason, 'time');
  assert.equal(asked57.sec, 57);

  const asked60 = simulateExit(pathOf(c), { ...flat, maxHoldSec: 60 });
  assert.equal(asked60.resolved, false);
  assert.equal(asked60.reason, 'unobserved');
  assert.equal(asked60.heldSec, 57, 'and it says how much it did have');
  assert.equal(asked60.wanted, 60);
});

test('a coin whose every candle predates its entry is not a candle-fidelity replay', () => {
  // It traded hard for two seconds and then went quiet: the file is detailed
  // about seconds nobody could trade and silent about every second held. Calling
  // that candle fidelity would report a precision the replay does not have.
  const candles = {
    candleSeconds: 1,
    candles: [
      { s: 0, o: 3e-7, h: 4e-7, l: 3e-7, c: 4e-7 },
      { s: 1, o: 4e-7, h: 9e-7, l: 4e-7, c: 9e-7 },
    ],
  };

  const bare = coin({ highs: [], lows: [], open: opened(3) });
  bare.market = candles;
  assert.equal(pathOf(bare).fidelity, 'coarse', 'the summary is all it really has');

  const withLadder = coin({ open: opened(3) });
  withLadder.market = candles;
  assert.equal(pathOf(withLadder).fidelity, 'ladder', 'falling back to what it does have');
});

test('a record with no opening summary keeps the reading it always had', () => {
  // Coins written before `open.seconds` existed cannot say when their entry was
  // fixed, so the path starts where the file starts. Every other test in this
  // file leans on this, which is the reason it is asserted rather than assumed.
  const p = pathOf(coin({ open: { wallets: 10, sellers: 2, solIn: 5 } }));
  assert.equal(p.points[0].sec, 0);
  assert.equal(p.fidelity, 'ladder');
});

// ---------------------------------------------------------------------------
// Exits
// ---------------------------------------------------------------------------

test('a target that is already passed fills at the target, not the peak', () => {
  const e = simulateExit(pathOf(coin()), { takeProfit: 1.5, stopLoss: 0.5, maxHoldSec: 60 });
  assert.equal(e.reason, 'target');
  assert.equal(e.mult, 1.5, 'you get your limit price, never the high that filled it');
  assert.equal(e.sec, 20);
});

test('a stop fills at the stop', () => {
  const e = simulateExit(pathOf(coin()), { takeProfit: 5, stopLoss: 0.85, maxHoldSec: 60 });
  assert.equal(e.reason, 'stop');
  assert.equal(e.mult, 0.85);
  assert.equal(e.sec, 40);
});

test('when a stop and a target are both live in one instant, the stop wins', () => {
  // A single candle whose low is under the stop and whose high is over the
  // target. Nothing in the recording says which came first, so it is the stop.
  const c = coin({ highs: [], lows: [] });
  c.market = { candleSeconds: 1, candles: [{ s: 3, o: 1e-6, h: 3e-6, l: 5e-7, c: 1e-6 }] };
  const e = simulateExit(pathOf(c), { takeProfit: 2, stopLoss: 0.8, maxHoldSec: 60 });
  assert.equal(e.reason, 'stop');
});

test('a trailing stop rides the peak down and fills at the trailing level', () => {
  // Peak 1.6x, trail 25% => the order rests at 1.2x. The coin reaches 0.6x, so
  // it is tripped, but the fill is 1.2x.
  const e = simulateExit(pathOf(coin()), { takeProfit: null, stopLoss: null, trailingStopPct: 0.25, maxHoldSec: 60 });
  assert.equal(e.reason, 'trail');
  assert.equal(round(e.mult), 1.2);
});

test('a time exit uses the last price actually seen', () => {
  const e = simulateExit(pathOf(coin()), { takeProfit: 9, stopLoss: 0.1, maxHoldSec: 60 });
  assert.equal(e.reason, 'time');
  assert.equal(e.mult, 0.7);
});

test('an exit past the end of the recording is unresolved, never a time exit', () => {
  // Watched for 60 seconds, asked to hold for 900. Calling that a time exit at
  // the 60-second price is the single easiest way to fake a backtest.
  const far = { takeProfit: 9, stopLoss: 0.1, trailingStopPct: null, maxHoldSec: 900 };
  const e = simulateExit(pathOf(coin()), far);
  assert.equal(e.resolved, false);
  assert.equal(e.reason, 'unobserved');

  const r = runBacktest({ records: [coin()], strategy: withExit(far) });
  assert.equal(r.trades.length, 0);
  assert.equal(r.skipped.unobserved, 1);
});

test('a level the coin did reach still resolves even past the recording', () => {
  // The other half of the rule above: 15 minutes is longer than this coin was
  // watched, but it touched 1.5x at twenty seconds, so the exit is known.
  const r = runBacktest({ records: [coin()], strategy: withExit({ ...DEFAULT_EXIT, maxHoldSec: 900 }) });
  assert.equal(r.trades.length, 1);
  assert.equal(r.trades[0].reason, 'target');
  assert.equal(r.skipped.unobserved, 0);
});

// ---------------------------------------------------------------------------
// Money
// ---------------------------------------------------------------------------

test('an instant take-profit pays the target less both legs of cost', () => {
  // 0.5 SOL, target 1.5x, 150 bps a leg, 0.005 SOL a leg.
  //   entry fill 1.015, exit fill 1.5 * 0.985 = 1.4775
  //   proceeds   0.5 * 1.4775 / 1.015 = 0.727832512315...
  //   pnl        0.727832512315 - 0.5 - 0.01 = 0.217832512315
  const r = runBacktest({
    records: [coin({ highs: [[1, 1.5]], peakMult: 1.5, peakAtSec: 1 })],
    strategy: withExit({ takeProfit: 1.5, stopLoss: 0.5, maxHoldSec: 60 }),
    initialBalanceSol: 10, positionSizeSol: 0.5, slippageBps: 150, feeSol: 0.005,
  });
  assert.equal(r.trades.length, 1);
  const t = r.trades[0];
  assert.equal(t.reason, 'target');
  assert.ok(Math.abs(t.pnlSol - 0.217833) < 1e-6, `pnl was ${t.pnlSol}`);
  assert.ok(Math.abs(r.summary.finalBalanceSol - 10.217833) < 1e-6);
  assert.equal(r.summary.wins, 1);
  assert.equal(r.summary.losses, 0);
  assert.equal(r.summary.profitFactor, null, 'no losses is not an infinite profit factor');
});

test('a rug to zero loses the position and both fees, and no more', () => {
  // A coin that goes to nothing with no stop in the way: the exit multiple is
  // the close, which is 0. You lose the 0.5 and the 0.01 of fees.
  const rug = coin({ highs: [], lows: [[5, 0.001]], peakMult: 1, peakAtSec: 0, endMult: 0.0001 });
  const r = runBacktest({
    records: [rug],
    strategy: withExit({ takeProfit: 99, stopLoss: null, maxHoldSec: 60 }),
    initialBalanceSol: 10, positionSizeSol: 0.5, slippageBps: 150, feeSol: 0.005,
  });
  assert.equal(r.trades.length, 1);
  const t = r.trades[0];
  // Position 0.5 plus 0.01 of fees, less the ~0.00005 the dust was worth.
  assert.ok(t.pnlSol < -0.5, 'a total loss costs more than the position: the fees too');
  assert.ok(t.pnlSol > -0.51, `and never more than the position plus its fees, got ${t.pnlSol}`);
  assert.ok(t.pnlPct < -100 && t.pnlPct > -102.5, `just past -100% once fees are in, got ${t.pnlPct}%`);
  assert.equal(r.summary.winRatePct, 0);
  assert.equal(r.summary.profitFactor, 0, 'all loss, no profit');
});

test('a run with no trades reports zeros rather than dividing by none', () => {
  const r = runBacktest({ records: [coin(), coin({ mint: 'M2' })], strategy: buyNothing });
  assert.equal(r.trades.length, 0);
  assert.equal(r.skipped.notTaken, 2);
  const s = r.summary;
  assert.equal(s.trades, 0);
  assert.equal(s.pnlSol, 0);
  assert.equal(s.finalBalanceSol, 10);
  assert.equal(s.maxDrawdownSol, 0);
  for (const k of ['winRatePct', 'profitFactor', 'sharpe', 'sortino', 'expectancySol', 'avgHoldSec']) {
    assert.equal(s[k], null, `${k} must be unknown, not zero`);
  }
  assert.equal(s.thin, true);
});

test('the account stops trading when it can no longer fund a position', () => {
  // Ten total losses of 0.5 from a balance of 2 can fund four, not ten.
  const rugs = Array.from({ length: 10 }, (_, i) =>
    coin({ mint: `R${i}`, t: 1_000_000 + i * 1000, highs: [], lows: [[5, 0.01]], peakMult: 1, endMult: 0.01 }));
  const r = runBacktest({
    records: rugs,
    strategy: withExit({ takeProfit: 99, stopLoss: null, maxHoldSec: 60 }),
    initialBalanceSol: 2, positionSizeSol: 0.5, slippageBps: 150, feeSol: 0.005,
  });
  assert.equal(r.trades.length, 3, 'three trades fit before the balance is too thin for a fourth');
  assert.equal(r.skipped.insufficientBalance, 7);
  assert.ok(r.summary.finalBalanceSol >= 0, 'the account never goes negative');
});

// ---------------------------------------------------------------------------
// Drawdown
// ---------------------------------------------------------------------------

test('drawdown is measured from the high water mark, not the start', () => {
  // 10 -> 12 -> 9 -> 11.  The fall is 3 from the peak of 12, which is 25%.
  const equity = [
    { t: 0, balance: 10, trade: 0 },
    { t: 1000, balance: 12, trade: 1 },
    { t: 5000, balance: 9, trade: 2 },
    { t: 9000, balance: 11, trade: 3 },
  ];
  const d = maxDrawdown(equity);
  assert.equal(d.sol, 3);
  assert.equal(d.pct, 25);
  assert.equal(d.durationMs, 4000, 'peak at 1000ms, trough at 5000ms');
  assert.equal(d.fromTrade, 1);
  assert.equal(d.toTrade, 2);
});

test('a curve that only rises has no drawdown', () => {
  const d = maxDrawdown([{ t: 0, balance: 10, trade: 0 }, { t: 1, balance: 11, trade: 1 }]);
  assert.equal(d.sol, 0);
  assert.equal(d.pct, 0);
});

test('the deepest fall wins, not the most recent', () => {
  // 10 -> 6 (a fall of 4) -> 20 -> 18 (a fall of 2). The answer is the first.
  const d = maxDrawdown([
    { t: 0, balance: 10, trade: 0 },
    { t: 1, balance: 6, trade: 1 },
    { t: 2, balance: 20, trade: 2 },
    { t: 3, balance: 18, trade: 3 },
  ]);
  assert.equal(d.sol, 4);
  assert.equal(d.pct, 40);
});

// ---------------------------------------------------------------------------
// Summary arithmetic
// ---------------------------------------------------------------------------

test('profit factor, expectancy and averages are what they say they are', () => {
  const trades = [
    { pnlSol: 2, sizeSol: 1, holdSec: 10, balanceSol: 12 },
    { pnlSol: -1, sizeSol: 1, holdSec: 20, balanceSol: 11 },
    { pnlSol: 4, sizeSol: 1, holdSec: 30, balanceSol: 15 },
    { pnlSol: -2, sizeSol: 1, holdSec: 40, balanceSol: 13 },
  ];
  const s = summarise({
    trades,
    equity: trades.map((t, i) => ({ t: i * 1000, balance: t.balanceSol, trade: i + 1 })),
    initialBalanceSol: 10, positionSizeSol: 1, slippageBps: 150, feeSol: 0.005,
  });
  assert.equal(s.trades, 4);
  assert.equal(s.wins, 2);
  assert.equal(s.losses, 2);
  assert.equal(s.winRatePct, 50);
  assert.equal(s.profitFactor, 2, 'won 6, lost 3');
  assert.equal(s.pnlSol, 3);
  assert.equal(s.pnlPct, 30, '3 SOL on a 10 SOL balance');
  assert.equal(s.expectancySol, 0.75, '3 over 4 trades');
  assert.equal(s.avgWinnerSol, 3);
  assert.equal(s.avgLoserSol, -1.5);
  assert.equal(s.avgHoldSec, 25);
  assert.equal(s.thin, true, 'four trades is far under the minimum sample');
});

test('sharpe is positive when the average trade wins and undefined for one trade', () => {
  const one = summarise({
    trades: [{ pnlSol: 1, sizeSol: 1, holdSec: 5, balanceSol: 11 }],
    equity: [{ t: 0, balance: 11, trade: 1 }],
    initialBalanceSol: 10, positionSizeSol: 1, slippageBps: 150, feeSol: 0.005,
  });
  assert.equal(one.sharpe, null, 'one trade has no spread to measure');

  const many = summarise({
    trades: [1, -0.5, 1, -0.5, 2].map((p, i) => ({ pnlSol: p, sizeSol: 1, holdSec: 5, balanceSol: 10 + p })),
    equity: [{ t: 0, balance: 10, trade: 0 }],
    initialBalanceSol: 10, positionSizeSol: 1, slippageBps: 150, feeSol: 0.005,
  });
  assert.ok(many.sharpe > 0, `expected a positive sharpe, got ${many.sharpe}`);
  assert.ok(many.sortino > many.sharpe, 'sortino ignores upside spread, so it reads higher here');
});

// ---------------------------------------------------------------------------
// The replay as a whole
// ---------------------------------------------------------------------------

test('coins are replayed oldest first however the file was ordered', () => {
  const records = [coin({ mint: 'C', t: 3000 }), coin({ mint: 'A', t: 1000 }), coin({ mint: 'B', t: 2000 })];
  const r = runBacktest({ records, strategy: withExit({ takeProfit: 1.5, stopLoss: 0.85, maxHoldSec: 60 }) });
  assert.deepEqual(r.trades.map((t) => t.mint), ['A', 'B', 'C']);
});

test('the same input always gives the same result', () => {
  const records = Array.from({ length: 20 }, (_, i) => coin({ mint: `M${i}`, t: 1000 + i }));
  const run = () => runBacktest({ records, strategy: buyEverything });
  assert.deepEqual(run().summary, run().summary);
});

test('a strategy that throws costs one coin, not the run', () => {
  const angry = {
    name: 'angry',
    shouldEnter: (rec) => { if (rec.mint === 'M1') throw new Error('nope'); return true; },
  };
  const r = runBacktest({ records: [coin({ mint: 'M1' }), coin({ mint: 'M2', t: 2000 })], strategy: angry });
  assert.equal(r.trades.length, 1);
  assert.equal(r.trades[0].mint, 'M2');
});

test('bad arguments are refused rather than quietly guessed at', () => {
  assert.throws(() => runBacktest({ records: null, strategy: buyEverything }), TypeError);
  assert.throws(() => runBacktest({ records: [], strategy: {} }), TypeError);
  assert.throws(() => runBacktest({ records: [], strategy: buyEverything, initialBalanceSol: 0 }), RangeError);
  assert.throws(() => runBacktest({ records: [], strategy: buyEverything, positionSizeSol: -1 }), RangeError);
});

function round(n, dp = 6) {
  const f = 10 ** dp;
  return Math.round(n * f) / f;
}
