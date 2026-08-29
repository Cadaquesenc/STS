// Replay a trading rule over coins that already happened.
//
// This is the deterministic cousin of strategy.js. That file asks "which exit
// rule looked best across a cohort"; this one asks "if I had actually run this
// rule, with a real balance, paying real costs, in the order the coins arrived
// — what would the account look like now". Same data, different question, and
// the second one is the only one that produces a drawdown.
//
// Three rules are carried over from strategy.js because they are what stops a
// backtest from lying:
//
//   1. Costs come off before anything is reported. A gross number is not a
//      result, it is an advertisement.
//   2. A trade whose exit was never observed is not a trade. It is counted and
//      set aside, never resolved as a time exit, because "the window ran out"
//      and "the clock exit fired" are different facts and only one of them is
//      known.
//   3. A sample too small to mean anything says so instead of printing a
//      percentage.
//
// The one thing this file must never do is invent price action. What each coin
// did between its launch and the end of its follow window is known to different
// depths depending on when it was recorded, so every replay carries the fidelity
// of the data it ran on and the summary reports the mix. See `pathOf` below.

import { roundTripCostPct } from './cost.js';
import { MIN_SAMPLE } from './strategy.js';
import { analyzeLaunch } from './cluster.js';

export const DEFAULTS = {
  initialBalanceSol: 10,
  positionSizeSol: 0.5,
  slippageBps: 150,
  feeSol: 0.005,
};

/**
 * The longest hold the recorder's defaults can actually answer for.
 *
 * watch.js follows a coin for `follow` seconds from its launch and does not fix
 * an entry price until `seconds`, so a position is observed for the difference:
 * 57 seconds, not the round minute the exits here used to ask for. The two only
 * looked like the same number while paths were stamped from the launch instead of
 * from the entry.
 *
 * Asking for longer than was observed is not the cautious choice it looks like.
 * Every coin that did not reach a level comes back unresolved and is dropped, and
 * the coins that did reach one are the coins that moved — so what survives is not
 * a smaller sample, it is a sample selected on its own outcome. On the corpus in
 * this checkout, a 60-second hold drops 1,502 of 2,602 replays and turns
 * buy-everything from a 95% loss into a 163% gain. That number is the artefact,
 * not the finding.
 *
 * Held in step with watch.js by a test rather than by an import, so running a
 * backtest does not drag in the websocket and the database to learn a constant.
 */
export const OBSERVED_HOLD_SEC = 57; // watch.js DEFAULTS.follow - DEFAULTS.seconds

/** Exit rules used when a strategy does not name its own. */
export const DEFAULT_EXIT = {
  takeProfit: 1.5,
  stopLoss: 0.85,
  trailingStopPct: null,
  maxHoldSec: OBSERVED_HOLD_SEC,
};

/**
 * How well a coin's price path is known, best first. A replay is only as
 * trustworthy as the worst fidelity in it, which is why the summary breaks the
 * trades down by this rather than averaging over it.
 *
 *   candles  every second that traded, with its high and low
 *   ladder   every new best and new worst, in the order they happened
 *   coarse   only the peak and the close; the shape between them is assumed
 */
export const FIDELITY = ['candles', 'ladder', 'coarse'];

// Where an observation sits inside its own second. Several prices can share a
// timestamp, and which one a rule sees first decides the answer, so the order is
// named rather than left to the sort.
const LOW = 0;   // the worst price of that instant, examined first
const MID = 1;   // the best price of that instant
const CLOSE = 2; // where the instant ended, and so what a clock exit gets

// ---------------------------------------------------------------------------
// Price paths
// ---------------------------------------------------------------------------

/**
 * Turn one recorded coin into a time-ordered list of `{ sec, mult }` points,
 * where `mult` is a multiple of the entry price.
 *
 * Within any one instant the pessimistic order is used — the low before the
 * high — so that a rule with both a stop and a target is scored as if the stop
 * came first. This is the same tie-break strategy.js makes, and for the same
 * reason: it is the reading that stops a rule looking better than it was.
 *
 * Every `sec` is seconds **held** — counted from the entry, not from the launch.
 * The recorder stamps candles, ladder rungs and the peak with seconds since the
 * launch, and the first `entrySec` of those happened before there was a position
 * to measure, so they are subtracted off here. That makes `sec` the same quantity
 * an exit rule is written in: `maxHoldSec` is a hold, and so is the `holdSec` a
 * trade reports.
 *
 * Returns null when the coin has no entry price, which means it was never
 * buyable and cannot be scored against anything.
 */
export function pathOf(record) {
  const o = record?.outcome ?? {};
  const entry = o.entry ?? record?.entry ?? null;
  if (!entry || !(entry > 0)) return null;

  // The second the position was opened, on the recorder's clock. watch.js does
  // not fix an entry price until it reaches its `seconds` mark, so the coin
  // trades for a few seconds before there is anything to measure against.
  //
  // A record with no opening summary gets 0, which is the reading such a record
  // has always had: assume the path starts where the file starts.
  const entrySec = Number(record?.open?.seconds ?? 0) || 0;

  // How long the position could be watched. `follow` is the whole window from
  // the launch, and its first `entrySec` were not time anyone held anything, so
  // an exit rule asking for more than this difference is asking about seconds
  // that were never recorded.
  const followSec = Number(o.follow ?? record?.watchedSec ?? 0) || 0;
  const observedSec = Math.max(0, followSec - entrySec);

  const candles = record?.market?.candles;
  if (Array.isArray(candles) && candles.length) {
    const seconds = Number(record.market.candleSeconds ?? 1) || 1;
    const points = [];
    for (const c of candles) {
      const at = Number(c.s) * seconds - entrySec;
      // The seconds before the entry are a price that existed and was never
      // enterable. Reading them as price action is how a coin that ran up into
      // its entry used to break the stop at second zero, closing a position
      // that had not been opened — on this corpus, 27 of the 86 coins with
      // candles. A bucket that merely straddles the entry instant goes too:
      // part of it is pre-entry, and a candle never says which part.
      if (!(at >= 0)) continue;
      // A candle says what its extremes were, never which came first. The low
      // is read first (rank 0) and the close last (rank 2), so a stop inside
      // the second is seen before the high that would have saved it, and a
      // clock exit lands on the closing price rather than the second's best.
      if (c.l > 0) points.push({ sec: at, mult: c.l / entry, rank: LOW });
      if (c.h > 0) points.push({ sec: at, mult: c.h / entry, rank: MID });
      if (c.c > 0) points.push({ sec: at, mult: c.c / entry, rank: CLOSE });
    }
    // Nothing survived, so every candle this coin has predates its entry: it
    // traded in its first seconds and then went quiet. The file looks detailed
    // and says nothing about the seconds actually held, so this is not a
    // candle-fidelity replay and must not be counted as one. Falling through
    // keeps `fidelity` a claim about the held period rather than about the file.
    if (points.length) return finish({ points, fidelity: 'candles', entry, entrySec, observedSec, record });
  }

  const highs = Array.isArray(o.highs) ? o.highs : [];
  const lows = Array.isArray(o.lows) ? o.lows : [];
  if (highs.length || lows.length) {
    // Both arrays are already multiples of entry, each recorded the moment it
    // became a new extreme. Merging them by time rebuilds the order of events.
    const points = [
      ...lows.map(([sec, mult]) => ({ sec: Number(sec) - entrySec, mult: Number(mult), rank: LOW })),
      ...highs.map(([sec, mult]) => ({ sec: Number(sec) - entrySec, mult: Number(mult), rank: MID })),
    ];
    return finish({ points, fidelity: 'ladder', entry, entrySec, observedSec, record });
  }

  // Nothing but the summary. The path is assumed to be entry, then up to the
  // peak, then down to the close — which is true as far as it goes, and says
  // nothing about any dip the coin took and recovered from. Rules replayed at
  // this fidelity are reported separately for exactly that reason.
  // The peak and the close are put back by `finish` from the same summary
  // fields, so the only thing to add here is the shape between them: nothing.
  return finish({ points: [], fidelity: 'coarse', entry, entrySec, observedSec, record });
}

/**
 * Common tail of every path: prepend the entry, reconcile against the summary
 * figures, and sort.
 *
 * The reconciliation matters. watch.js stops appending to `highs`/`lows` after
 * sixty turning points, and freezes its running extremes with them, but keeps
 * tracking the peak separately — so on a busy coin the ladder can end below the
 * peak that actually happened. Trusting the ladder alone would quietly hide the
 * best part of the move, so the recorded peak is put back.
 *
 * Second zero is the entry, and the points arrive already counted from it. The
 * only thing still on the recorder's clock by the time it gets here is the peak,
 * which is read straight off the summary, so that is rebased here. The bound is
 * the same statement from the other side: a path holds the seconds the position
 * was held and no others, whatever fidelity fed it.
 */
function finish({ points, fidelity, entry, entrySec = 0, observedSec, record }) {
  const o = record?.outcome ?? {};
  const all = [
    { sec: 0, mult: 1, rank: LOW },
    ...points.filter((p) => Number.isFinite(p.sec) && p.sec >= 0 && p.mult > 0),
  ];

  const peakMult = Number(o.peakMult ?? 0);
  // A record carrying a peak multiple but no second for it keeps the reading it
  // has always had — the entry instant — rather than losing the peak altogether.
  const peakAtSec = o.peakAtSec == null ? 0 : Math.max(0, Number(o.peakAtSec) - entrySec);
  const seen = all.reduce((m, p) => Math.max(m, p.mult), 1);
  if (peakMult > seen + 1e-9 && Number.isFinite(peakAtSec)) {
    all.push({ sec: peakAtSec, mult: peakMult, rank: MID, restored: true });
  }

  // The close is the last thing that is known to have happened.
  const endMult = Number(o.endMult ?? 0);
  if (endMult > 0 && observedSec > 0) all.push({ sec: observedSec, mult: endMult, rank: CLOSE, close: true });

  // Time, then rank, then price. Rank is what keeps the low-before-high reading
  // from being sorted away, and what keeps the close at the end of its second.
  all.sort((a, b) => a.sec - b.sec || a.rank - b.rank || a.mult - b.mult);
  // `entrySec` is carried so a caller can put a hold back on the recorder's clock
  // if it needs to — plotting the path against the candles, say. Nothing in the
  // replay reads it; everything in the replay is already counted from the entry.
  return { points: all, fidelity, entry, entrySec, observedSec };
}

// ---------------------------------------------------------------------------
// Exit simulation
// ---------------------------------------------------------------------------

/**
 * Walk a path under one exit rule and report where the position came off.
 *
 * Order of checks inside a single observation is deliberate and pessimistic:
 * a hard stop beats a trailing stop beats a target. If two of them are true at
 * the same instant, the one that pays least is the one that fired.
 *
 * Every second in here is a hold: `maxHoldSec`, `dumpAtSec`, the `sec` on each
 * point and the `sec` reported back all count from the entry. `observedSec` is
 * therefore how long the position was watched, not how long the coin was, which
 * is what makes the unresolved test below mean what it says.
 *
 * Returns `{ resolved: false }` when the window ran out before any exit
 * condition was met and before the hold limit was reached — the caller must not
 * turn that into a trade.
 */
export function simulateExit(path, exit = DEFAULT_EXIT) {
  const takeProfit = num(exit.takeProfit);
  const stopLoss = num(exit.stopLoss);
  const trail = num(exit.trailingStopPct);
  const maxHoldSec = num(exit.maxHoldSec);
  // An exit at a moment the caller names, rather than at a price. Used for
  // "get out when something happened", where the something is known from the
  // record but is not a level — see creatorDumpSecond.
  const dumpAtSec = num(exit.dumpAtSec);

  let peak = 1;
  let last = { sec: 0, mult: 1 };

  for (const p of path.points) {
    if (maxHoldSec != null && p.sec > maxHoldSec) break;
    last = p;
    if (p.mult > peak) peak = p.mult;

    if (stopLoss != null && p.mult <= stopLoss) {
      return { resolved: true, reason: 'stop', mult: stopLoss, sec: p.sec, peak };
    }
    if (trail != null && peak > 1 && p.mult <= peak * (1 - trail)) {
      // The trailing level is where the order rests, so that is the fill —
      // never the lower price that happened to trip it.
      return { resolved: true, reason: 'trail', mult: peak * (1 - trail), sec: p.sec, peak };
    }
    // Checked after the resting orders and before the target, keeping the same
    // pessimistic bias as the rest of this loop: a stop that was already hit
    // wins, and a target that had not been reached does not get to claim the
    // trade. This fills at whatever the price was, because leaving at market on
    // news is not a level you chose.
    if (dumpAtSec != null && p.sec >= dumpAtSec) {
      return { resolved: true, reason: 'dump', mult: p.mult, sec: p.sec, peak };
    }
    if (takeProfit != null && p.mult >= takeProfit) {
      return { resolved: true, reason: 'target', mult: takeProfit, sec: p.sec, peak };
    }
  }

  // Nothing was touched. The exit is the clock — but only if the clock actually
  // ran out inside the window we watched. Otherwise the answer is not known.
  if (maxHoldSec == null) {
    return { resolved: true, reason: 'end', mult: last.mult, sec: last.sec, peak };
  }
  if (path.observedSec + 1e-9 < maxHoldSec) {
    return { resolved: false, reason: 'unobserved', heldSec: path.observedSec, wanted: maxHoldSec };
  }
  return { resolved: true, reason: 'time', mult: last.mult, sec: Math.min(maxHoldSec, last.sec), peak };
}

// ---------------------------------------------------------------------------
// The replay
// ---------------------------------------------------------------------------

/**
 * Run a strategy over historical coins, in the order they launched.
 *
 * `strategy.shouldEnter(record, context)` decides. Returning anything truthy
 * enters; returning an object may override the exit rule or the size for that
 * one coin. The context carries the running balance, so a strategy can size off
 * the account rather than a constant.
 */
export function runBacktest({
  records,
  strategy,
  initialBalanceSol = DEFAULTS.initialBalanceSol,
  positionSizeSol = DEFAULTS.positionSizeSol,
  slippageBps = DEFAULTS.slippageBps,
  feeSol = DEFAULTS.feeSol,
} = {}) {
  if (!Array.isArray(records)) throw new TypeError('runBacktest needs an array of records');
  if (!strategy || typeof strategy.shouldEnter !== 'function') {
    throw new TypeError('runBacktest needs a strategy with a shouldEnter(record, context) method');
  }
  if (!(initialBalanceSol > 0)) throw new RangeError('initialBalanceSol must be positive');
  if (!(positionSizeSol > 0)) throw new RangeError('positionSizeSol must be positive');

  const slip = Number(slippageBps) / 10_000;
  const fee = Number(feeSol) || 0;

  // Chronological, because a drawdown is a statement about order. Ties keep the
  // order the file gave them, so the same file always replays identically.
  const ordered = records
    .map((record, i) => ({ record, i }))
    .sort((a, b) => (Number(a.record?.t ?? 0) - Number(b.record?.t ?? 0)) || (a.i - b.i))
    .map((x) => x.record);

  let balance = initialBalanceSol;
  const trades = [];
  const equity = [{ t: ordered.length ? Number(ordered[0].t ?? 0) : 0, balance, trade: 0 }];
  const skipped = { noEntry: 0, notTaken: 0, unobserved: 0, insufficientBalance: 0 };
  const byFidelity = { candles: 0, ladder: 0, coarse: 0 };

  for (const record of ordered) {
    // Positions are taken and closed one at a time. Coins overlap in reality,
    // so a live account running this rule would hold several at once and would
    // hit its balance limit sooner than this replay does — which makes the
    // number of trades here an upper bound, not a forecast.
    const context = {
      balanceSol: balance,
      initialBalanceSol,
      positionSizeSol,
      trades: trades.length,
    };

    let decision;
    try {
      decision = strategy.shouldEnter(record, context);
    } catch {
      // A strategy that throws on one odd coin should not lose the whole run.
      decision = false;
    }
    if (!decision) { skipped.notTaken++; continue; }

    const path = pathOf(record);
    if (!path) { skipped.noEntry++; continue; }

    const exit = { ...DEFAULT_EXIT, ...(strategy.exit ?? {}), ...(decision?.exit ?? {}) };
    const size = Number(decision?.sizeSol ?? positionSizeSol);

    // Every trade costs its fee twice whether it wins or not, so the account has
    // to be able to pay for the exit at the moment it pays for the entry.
    if (balance < size + fee * 2) { skipped.insufficientBalance++; continue; }

    const result = simulateExit(path, exit);
    if (!result.resolved) { skipped.unobserved++; continue; }

    // Slippage works against you on both legs: you buy above the quote and sell
    // below it.
    const entryFill = 1 * (1 + slip);
    const exitFill = result.mult * (1 - slip);
    const proceeds = size * (exitFill / entryFill);
    const pnlSol = proceeds - size - fee * 2;

    balance += pnlSol;
    byFidelity[path.fidelity]++;

    const trade = {
      mint: record.mint ?? null,
      symbol: record.symbol ?? null,
      t: Number(record.t ?? 0),
      sizeSol: round(size, 6),
      entryMult: round(entryFill, 6),
      exitMult: round(exitFill, 6),
      grossMult: round(result.mult, 6),
      reason: result.reason,
      holdSec: round(result.sec, 2),
      peakMult: round(result.peak ?? 1, 4),
      fidelity: path.fidelity,
      feeSol: round(fee * 2, 6),
      pnlSol: round(pnlSol, 6),
      pnlPct: round((pnlSol / size) * 100, 4),
      balanceSol: round(balance, 6),
    };
    trades.push(trade);
    equity.push({ t: trade.t, balance, trade: trades.length });
  }

  return {
    summary: summarise({ trades, equity, initialBalanceSol, positionSizeSol, slippageBps, feeSol }),
    trades,
    equity,
    skipped,
    byFidelity,
    config: { initialBalanceSol, positionSizeSol, slippageBps, feeSol, exit: { ...DEFAULT_EXIT, ...(strategy.exit ?? {}) } },
    strategy: { name: strategy.name ?? 'anonymous', describe: strategy.describe ?? null },
    recordsConsidered: ordered.length,
  };
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/**
 * Everything the report prints, computed once from the trade list and the
 * equity curve.
 *
 * Sharpe and Sortino here are per-trade, not annualised. Annualising would mean
 * asserting a trade frequency the data does not contain, and the number would
 * look authoritative while resting on an assumption nobody checked. They are
 * comparable between two runs over the same coins and mean nothing outside that.
 */
export function summarise({ trades, equity = [], initialBalanceSol, positionSizeSol, slippageBps, feeSol }) {
  const n = trades.length;
  const finalBalance = n ? trades.at(-1).balanceSol : initialBalanceSol;
  const drawdown = maxDrawdown(equity.length ? equity : [{ t: 0, balance: initialBalanceSol, trade: 0 }]);
  const cost = roundTripCostPct(positionSizeSol);
  // What this run charged itself, in the same terms cost.js states its measured
  // figure, so the two can be read side by side.
  const simulatedCostPct = round(
    ((1 + slippageBps / 10_000) / (1 - slippageBps / 10_000) - 1) * 100 + ((feeSol * 2) / positionSizeSol) * 100,
  );

  if (!n) {
    return {
      trades: 0, thin: true, wins: 0, losses: 0, winRatePct: null, profitFactor: null,
      pnlSol: 0, pnlPct: 0, initialBalanceSol, finalBalanceSol: initialBalanceSol,
      maxDrawdownSol: 0, maxDrawdownPct: 0, drawdownSec: 0,
      sharpe: null, sortino: null, avgHoldSec: null,
      avgWinnerSol: null, avgLoserSol: null, expectancySol: null,
      simulatedCostPct, measuredCostPct: cost.totalPct,
    };
  }

  const wins = trades.filter((t) => t.pnlSol > 0);
  const losses = trades.filter((t) => t.pnlSol < 0);
  const grossProfit = wins.reduce((s, t) => s + t.pnlSol, 0);
  const grossLoss = Math.abs(losses.reduce((s, t) => s + t.pnlSol, 0));
  const pnlSol = trades.reduce((s, t) => s + t.pnlSol, 0);

  // Per-trade return on the money actually at risk, which is the series both
  // ratios are computed over.
  const returns = trades.map((t) => t.pnlSol / t.sizeSol);
  const mean = avg(returns);
  const sd = stdev(returns, mean);
  const downside = downsideDeviation(returns);

  return {
    trades: n,
    // Below this a percentage is noise wearing a number's clothes.
    thin: n < MIN_SAMPLE,
    minSample: MIN_SAMPLE,
    wins: wins.length,
    losses: losses.length,
    winRatePct: round((wins.length / n) * 100, 2),
    // No losers at all is not an infinite profit factor, it is an untested one.
    profitFactor: grossLoss > 0 ? round(grossProfit / grossLoss, 3) : null,
    pnlSol: round(pnlSol, 6),
    pnlPct: round((pnlSol / initialBalanceSol) * 100, 3),
    initialBalanceSol,
    finalBalanceSol: round(finalBalance, 6),
    maxDrawdownSol: round(drawdown.sol, 6),
    maxDrawdownPct: round(drawdown.pct, 3),
    drawdownSec: round(drawdown.durationMs / 1000, 1),
    drawdownFromTrade: drawdown.fromTrade,
    drawdownToTrade: drawdown.toTrade,
    sharpe: sd > 0 ? round(mean / sd, 3) : null,
    sortino: downside > 0 ? round(mean / downside, 3) : null,
    avgHoldSec: round(avg(trades.map((t) => t.holdSec)), 2),
    avgWinnerSol: wins.length ? round(avg(wins.map((t) => t.pnlSol)), 6) : null,
    avgLoserSol: losses.length ? round(avg(losses.map((t) => t.pnlSol)), 6) : null,
    expectancySol: round(pnlSol / n, 6),
    simulatedCostPct,
    measuredCostPct: cost.totalPct,
  };
}

/**
 * The worst peak-to-trough fall in the equity curve, and how long the account
 * spent under water getting there.
 */
export function maxDrawdown(equity) {
  let peak = equity[0].balance;
  let peakAt = equity[0].t;
  let peakTrade = equity[0].trade ?? 0;
  let worst = { sol: 0, pct: 0, durationMs: 0, fromTrade: null, toTrade: null };

  for (const point of equity) {
    if (point.balance > peak) {
      peak = point.balance;
      peakAt = point.t;
      peakTrade = point.trade ?? 0;
      continue;
    }
    const sol = peak - point.balance;
    if (sol > worst.sol) {
      worst = {
        sol,
        pct: peak > 0 ? (sol / peak) * 100 : 0,
        durationMs: Math.max(0, point.t - peakAt),
        fromTrade: peakTrade,
        toTrade: point.trade ?? null,
      };
    }
  }
  return worst;
}

// ---------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------

/**
 * A worked example, not a recommendation.
 *
 * It buys coins whose first seconds showed more buyers than sellers and real
 * SOL going in, which is the plainest possible reading of "momentum". Log.md
 * records that the best single on-chain signal found so far is worth about
 * +1.34% a trade against a round trip that costs about 3%, so the expected
 * result of running this is a loss. It is here so the harness has something
 * real to replay, and so that a negative number is what an honest engine
 * prints.
 */
export const basicMomentum = {
  name: 'basic-momentum',
  describe: 'early buyers outnumber early sellers, with real SOL going in',
  exit: { takeProfit: 1.5, stopLoss: 0.85, trailingStopPct: null, maxHoldSec: OBSERVED_HOLD_SEC },
  shouldEnter(record) {
    const open = record?.open ?? {};
    const wallets = Number(open.wallets || 0);
    const sellers = Number(open.sellers || 0);
    const solIn = Number(open.solIn || 0);
    return wallets >= 4 && sellers < wallets && solIn >= 1;
  },
};

/** Buys everything it can price. The baseline every other rule has to beat. */
export const buyEverything = {
  name: 'buy-everything',
  describe: 'enters every coin with a measurable entry price',
  exit: { ...DEFAULT_EXIT },
  shouldEnter: () => true,
};

/** Buys nothing. Proves the report survives a run with no trades in it. */
export const buyNothing = {
  name: 'buy-nothing',
  describe: 'never enters',
  shouldEnter: () => false,
};

// ---------------------------------------------------------------------------
// Following the syndicate
// ---------------------------------------------------------------------------

/**
 * The tags that mean "these buyers are one person", as opposed to the tags that
 * only mean "this launch is unusual". A score on its own is a blend; requiring
 * one of these means the score has to be coming from coordination rather than
 * from, say, a crowded first slot.
 */
export const PRIMARY_SIGNALS = ['IDENTICAL_SIZING', 'SAME_INSTANT_BUNDLE', 'CREATOR_BOUGHT_OWN'];

/** The score a launch has to reach before a primary signal is even consulted. */
export const MIN_CLUSTER_SCORE = 0.6;

/**
 * How many wallets have to be in the coordinated group before it is a group.
 *
 * Two addresses doing the same thing is a coincidence with a 50% base rate; the
 * third is what makes it a pattern. cluster.js already refuses to tag a bundle
 * or a repeated size below three, so on the recorded corpus this rejects
 * nothing — it is here because the gate should not depend on the analyser's
 * constant staying where it is, and because a caller widening `minGroup` should
 * not silently widen the entry rule with it.
 */
export const MIN_BUNDLE_WALLETS = 3;

/**
 * How close two positions in the same bundle have to be to count as one script.
 *
 * cluster.js groups sizes at 2%, which is the right width for *detecting* a
 * scripted amount through a different priority fee. This is tighter on purpose
 * and it is asking a different question: not "did somebody repeat a size
 * somewhere in this launch", but "did the wallets that landed together also
 * take the same position". A launch can pass the first and fail the second —
 * the identical-size group and the bundle can be disjoint sets of wallets — and
 * v1 of this rule could not tell those two apart.
 */
export const BUNDLE_SIZE_TOLERANCE = 0.01;

/**
 * The least a coordinated group can commit and still be worth following, in
 * SOL, summed over the wallets that both landed together and took the same size.
 *
 * The thesis of the whole rule is that the bundle's exit is the trade. A group
 * that put in less than this cannot move the price on the way out either, so
 * there is nothing to be early to.
 */
export const MIN_BUNDLE_SOL = 1.5;

/**
 * Every answer the gate can give, worst first, so a caller printing a funnel
 * gets the same order every time. `accepted` is the only one that trades.
 */
export const GATE_REASONS = [
  'unreadable',
  'no-opening-buys',
  'thin',
  'low-score',
  'no-primary-signal',
  'no-bundle',
  'thin-bundle',
  'mixed-sizing',
  'solo-dev',
  'small-bundle',
  'accepted',
];

/** Analysing the same coin twice per run is pure waste; the record is the key. */
const reportCache = new WeakMap();

function reportFor(record) {
  if (!record || typeof record !== 'object') return null;
  if (reportCache.has(record)) return reportCache.get(record);
  let report = null;
  try {
    report = analyzeLaunch(record);
  } catch {
    // A record the analyser cannot read is not a buy signal. It is also not a
    // reason to stop the run.
    report = null;
  }
  reportCache.set(record, report);
  return report;
}

/**
 * When the deployer sold, as far as the recording can say — which is less far
 * than it sounds.
 *
 * What is actually known is that the creator's wallet took SOL out at some
 * point inside the follow window. `who` carries the totals and the moment the
 * wallet was first seen, not the moment it sold, so the exact second is not in
 * the data.
 *
 * The stand-in is the first second after entry where the candle shows more
 * sells than buys. That is a guess, and it is worth being clear about which
 * way it is wrong: it fires on the first heavy-selling second whether or not
 * the creator was in it, so it will sometimes leave a coin that had not been
 * dumped yet. Whether that helps or hurts depends on the coin, which is why
 * `creatorDumpExit` can be turned off and the run compared both ways.
 *
 * Returned as a hold, counted from the entry, because that is the clock
 * `simulateExit` compares it against. The candle it comes from is stamped from
 * the launch, so the entry second comes off it on the way out.
 *
 * Returns null when the creator did not sell, or when the recording has no
 * candles and so cannot place the moment at all.
 */
export function creatorDumpSecond(record) {
  const creator = record?.creator;
  if (!creator) return null;

  const who = Array.isArray(record?.who) ? record.who : [];
  const dev = who.find((w) => w?.w === creator);
  if (!dev || !(num(dev.out) > 0)) return null;

  const candles = record?.market?.candles;
  if (!Array.isArray(candles) || !candles.length) return null;

  const seconds = num(record.market.candleSeconds) ?? 1;
  const entrySec = num(record?.open?.seconds) ?? 0;
  for (const c of candles) {
    const at = num(c?.s);
    if (at == null || at * seconds <= entrySec) continue; // you cannot react before you are in
    if (num(c?.sells) > num(c?.buys)) return at * seconds - entrySec;
  }
  return null;
}

/**
 * The two figures a human reads off a launch before deciding anything: what the
 * deployer put in, and what the thing was worth.
 *
 * Both are routinely absent. In this corpus `initialBuySol` is missing on 97%
 * of records and `supply` on 95%, and the SQLite store keeps the same two under
 * `initial_buy_sol` and `market_cap` — which are also null on most rows. So both
 * spellings are accepted, a market cap is worked out from supply and entry price
 * only when neither column has one, and every one of those steps is allowed to
 * come back null.
 *
 * Nothing here can reject a launch. A missing number is not a bad number, and a
 * gate that treats "not recorded" as "failed" quietly throws away the coins the
 * watcher happened to catch early — which is most of them.
 */
export function launchSize(record) {
  const initialBuySol = num(record?.initialBuySol ?? record?.initial_buy_sol);

  const stated = num(record?.marketCap ?? record?.market_cap);
  const supply = num(record?.supply);
  const entry = num(record?.outcome?.entry ?? record?.entry);
  const derived = supply > 0 && entry > 0 ? round(supply * entry, 6) : null;

  return { initialBuySol, marketCapSol: stated ?? derived };
}

/**
 * The wallets that both landed together and took the same position — the
 * syndicate itself, as opposed to everyone who happened to be in the same
 * seconds as it.
 *
 * Read off the largest bundle cluster.js found, then narrowed to the biggest
 * run of positions inside it that sit within `sizeTolerance` of each other.
 * Both halves are load-bearing. A bundle on a busy launch is mostly a queue —
 * on this corpus the widest one spans 26 wallets whose positions differ by a
 * factor of 125 — and a repeated size somewhere in the opening says nothing
 * about whether those particular wallets arrived together. The intersection is
 * the only part that means one operator.
 *
 * The run is bounded from its own smallest member rather than chained neighbour
 * to neighbour, for the reason cluster.js gives where it does the same: a ladder
 * of amounts 1% apart would otherwise collapse into one group and every launch
 * would look scripted.
 *
 * Always returns an object. `bundle` is null when no bundle cleared the
 * analyser's own minimum, and the counts are zero — a caller deciding what to do
 * about that is the gate's job, not this function's.
 */
export function coordinatedCohort(report, {
  sizeTolerance = BUNDLE_SIZE_TOLERANCE,
} = {}) {
  const none = { bundle: null, wallets: 0, sol: 0, external: 0, sizeSol: null, deltaPct: null };
  const bundles = report?.signals?.timing?.bundles;
  if (!Array.isArray(bundles) || !bundles.length) return none;

  // The largest bundle, earliest one winning a tie — the same one cluster.js
  // reports as `largest_bundle` and tags SAME_INSTANT_BUNDLE on.
  const bundle = bundles.reduce((m, b) => (b.wallets > m.wallets || (b.wallets === m.wallets && b.at < m.at) ? b : m));
  const members = Array.isArray(bundle.members) ? bundle.members : [];
  if (!members.length) return { ...none, bundle };

  const inBundle = new Set(members);
  const rows = (report.participants ?? [])
    .filter((p) => inBundle.has(p.address) && num(p.sol_spent) > 0)
    .sort((a, b) => a.sol_spent - b.sol_spent);
  if (!rows.length) return { ...none, bundle };

  // Widest window over the sorted positions whose ends are within tolerance.
  // A window rather than a left-to-right sweep because the question is how big
  // the matching group is, not how the launch partitions: a sweep anchored on
  // the smallest position can be broken by one wallet and miss a larger group
  // sitting just above it.
  let best = [];
  for (let lo = 0, hi = 0; hi < rows.length; hi++) {
    while ((rows[hi].sol_spent - rows[lo].sol_spent) / rows[lo].sol_spent > sizeTolerance) lo++;
    if (hi - lo + 1 > best.length) best = rows.slice(lo, hi + 1);
  }

  const creator = report.creator ?? null;
  const sol = best.reduce((s, r) => s + r.sol_spent, 0);
  const low = best[0].sol_spent;
  const high = best.at(-1).sol_spent;
  return {
    bundle,
    wallets: best.length,
    sol: round(sol, 6),
    // Wallets in the group that are not the deployer. A deployer buying its own
    // launch alongside two of its own wallets is a different thing from three
    // strangers, and only the count can tell them apart.
    external: best.filter((r) => r.address !== creator).length,
    sizeSol: round(low, 6),
    deltaPct: low > 0 ? round(((high - low) / low) * 100, 3) : null,
  };
}

/**
 * Does this launch look organised enough to follow, and if not, why not.
 *
 * Split out of the strategy and exported because the reason is worth as much as
 * the verdict: a run that took four trades out of three thousand coins is only
 * interpretable next to the count of what each rejection rule threw out. It is
 * pure — same record, same answer — so a caller can build that funnel without
 * running a backtest.
 *
 * The order of the checks is the order of confidence in them. The two data
 * rejections come first and are absolute: a launch nobody bought, or one bought
 * by fewer than three wallets, cannot be told apart from noise no matter what
 * score falls out of it. cluster.js already caps a thin launch's confidence at
 * 0.25, so the score test would catch these anyway — they are named separately
 * because "we cannot see this" and "we looked and it was ordinary" are different
 * facts, and a funnel that merges them hides how much of the corpus is simply
 * too quiet to read.
 *
 * The last four checks are v2, and they all ask the same question from
 * different sides: the tags say a signal fired *somewhere* in the opening, and
 * these ask whether it fired on a group large enough, uniform enough and rich
 * enough to be the thing the trade is following. A launch can carry
 * IDENTICAL_SIZING because three wallets in the twentieth second matched, and
 * SAME_INSTANT_BUNDLE because a different sixteen wallets raced the block, and
 * v1 read that as one syndicate. Turning a threshold off (0, or Infinity for
 * the tolerance) skips its check, which is how the v1 rule is still runnable
 * for comparison.
 */
export function syndicateGate(record, {
  minScore = MIN_CLUSTER_SCORE,
  primarySignals = PRIMARY_SIGNALS,
  minBundleWallets = MIN_BUNDLE_WALLETS,
  bundleSizeTolerance = BUNDLE_SIZE_TOLERANCE,
  minBundleSol = MIN_BUNDLE_SOL,
  requireExternalBundle = true,
} = {}) {
  const report = reportFor(record);
  if (!report) {
    return {
      enter: false, reason: 'unreadable', score: null, tags: [], thin: true,
      initialBuySol: null, marketCapSol: null,
      bundleWallets: 0, bundleSol: 0, cohortWallets: 0, cohortSol: 0, cohortDeltaPct: null, cohortExternal: 0,
    };
  }

  const tags = Array.isArray(report.risk_tags) ? report.risk_tags : [];
  const cohort = coordinatedCohort(report, { sizeTolerance: bundleSizeTolerance });
  const facts = {
    score: num(report.confidence_score) ?? 0,
    tags,
    thin: !!report.thin,
    ...launchSize(record),
    // Carried on every verdict, including the refusals, so a funnel can show
    // what the group actually looked like next to the reason it was turned down.
    bundleWallets: cohort.bundle?.wallets ?? 0,
    bundleSol: cohort.bundle?.sol ?? 0,
    cohortWallets: cohort.wallets,
    cohortSol: cohort.sol,
    cohortDeltaPct: cohort.deltaPct,
    cohortExternal: cohort.external,
  };
  const no = (reason) => ({ enter: false, reason, ...facts });

  if (tags.includes('NO_OPENING_BUYS')) return no('no-opening-buys');
  if (facts.thin) return no('thin');
  if (facts.score < minScore) return no('low-score');

  const primaries = tags.filter((tag) => primarySignals.includes(tag));
  if (!primaries.length) return no('no-primary-signal');

  if (minBundleWallets > 0) {
    // Nobody landed together in a group the analyser would call a bundle, so
    // whatever the tags are describing, it is not several wallets acting at once.
    if (!cohort.bundle) return no('no-bundle');
    // A bundle, but not enough addresses in it to be a pattern. Two wallets
    // doing the same thing is a coin flip.
    if (cohort.bundle.wallets < minBundleWallets) return no('thin-bundle');
    // Enough of them landed together, but they took unrelated positions. That is
    // a queue at a popular launch, which is what this rule is most often fooled
    // by — and the reason the two rejections are named separately.
    if (cohort.wallets < minBundleWallets) return no('mixed-sizing');
  }

  // A deployer buying its own coin is the weakest of the three primary signals —
  // cluster.js weights it lowest for the same reason — and on its own it says
  // "rug risk", not "syndicate". It only becomes an entry when somebody other
  // than the deployer bought in with it.
  if (requireExternalBundle && primaries.length === 1 && primaries[0] === 'CREATOR_BOUGHT_OWN') {
    if (cohort.external < Math.max(1, minBundleWallets)) return no('solo-dev');
  }

  if (minBundleSol > 0 && !(cohort.sol >= minBundleSol)) return no('small-bundle');

  return { enter: true, reason: 'accepted', ...facts };
}

/**
 * Buy the launches that look organised, and leave when the organiser does.
 *
 * The thesis is not that coordinated launches are good. It is that they are
 * *predictable*: a bundle that bought together tends to sell together, and the
 * window between those two things is the whole trade. So the entry is narrow on
 * purpose — a confidence score of its own is not enough, because the score can
 * come from signals that only mean "unusual".
 *
 * A caution that belongs next to any number this produces: the gate is narrow
 * enough that it fires on roughly one launch in a hundred. Over a corpus this
 * size that is a few dozen trades, which is below the point where a win rate
 * means much. Treat the output as a description of what these coins did, not as
 * an estimate of what the rule will do next.
 */
export function syndicateSniperStrategy({
  name = null,
  minScore = MIN_CLUSTER_SCORE,
  primarySignals = PRIMARY_SIGNALS,
  minBundleWallets = MIN_BUNDLE_WALLETS,
  bundleSizeTolerance = BUNDLE_SIZE_TOLERANCE,
  minBundleSol = MIN_BUNDLE_SOL,
  requireExternalBundle = true,
  creatorDumpExit = true,
  exit = { takeProfit: 1.5, stopLoss: 0.85, trailingStopPct: null, maxHoldSec: OBSERVED_HOLD_SEC },
} = {}) {
  const gateOptions = {
    minScore, primarySignals, minBundleWallets, bundleSizeTolerance, minBundleSol, requireExternalBundle,
  };
  const bundleWords = minBundleWallets > 0
    ? `, on ${minBundleWallets}+ wallets inside ${(bundleSizeTolerance * 100).toFixed(0)}% of one size`
    : '';
  const solWords = minBundleSol > 0 ? ` committing ${minBundleSol}+ SOL` : '';
  return {
    name: name ?? (creatorDumpExit ? 'syndicate-sniper' : 'syndicate-sniper-no-dump'),
    describe: `cluster score >= ${minScore} with one of ${primarySignals.join('/')}` +
      bundleWords + solWords +
      (creatorDumpExit ? ', exiting when the deployer sells' : ', ignoring the deployer'),
    exit,
    /** The funnel, for a caller that wants to know what was turned away. */
    explain: (record) => syndicateGate(record, gateOptions),
    shouldEnter(record) {
      const gate = syndicateGate(record, gateOptions);
      if (!gate.enter) return false;
      if (!creatorDumpExit) return gate;

      const dumpAtSec = creatorDumpSecond(record);
      return dumpAtSec == null ? gate : { ...gate, exit: { ...exit, dumpAtSec } };
    },
  };
}

export const syndicateSniper = syndicateSniperStrategy();
export const syndicateSniperNoDump = syndicateSniperStrategy({ creatorDumpExit: false });

/**
 * The rule as it stood before the group checks: score and a primary tag, and
 * nothing about who the tag fired on.
 *
 * Kept runnable rather than deleted because the only honest way to state what
 * the new checks did is to replay both over the same coins at the same costs,
 * and a number quoted from a previous checkout is not that.
 */
export const syndicateSniperV1 = syndicateSniperStrategy({
  name: 'syndicate-sniper-v1',
  minBundleWallets: 0,
  bundleSizeTolerance: Infinity,
  minBundleSol: 0,
  requireExternalBundle: false,
});

export const STRATEGIES = {
  'basic-momentum': basicMomentum,
  'buy-everything': buyEverything,
  'buy-nothing': buyNothing,
  'syndicate-sniper': syndicateSniper,
  'syndicate-sniper-no-dump': syndicateSniperNoDump,
  'syndicate-sniper-v1': syndicateSniperV1,
};

// ---------------------------------------------------------------------------

/**
 * A number, or null for "this rule is not set".
 *
 * The explicit null check earns its keep: `Number(null)` is 0, so without it an
 * unset trailing stop reads as a 0% trail that fires on the first tick and an
 * unset target reads as a target of zero. Both turn every trade into an instant
 * exit, and both look like a working backtest while doing it.
 */
function num(v) {
  if (v === null || v === undefined || v === '') return null;
  const n = Number(v);
  return Number.isFinite(n) ? n : null;
}

function avg(xs) {
  return xs.length ? xs.reduce((s, x) => s + x, 0) / xs.length : 0;
}

function stdev(xs, mean) {
  if (xs.length < 2) return 0;
  const v = xs.reduce((s, x) => s + (x - mean) ** 2, 0) / (xs.length - 1);
  return Math.sqrt(v);
}

/**
 * Root-mean-square of the losing side only, over the whole series — the
 * denominator Sortino uses so that upside volatility stops counting as risk.
 * Winners contribute zero rather than being dropped, which is what keeps a run
 * of large wins from shrinking the denominator and flattering the ratio.
 */
function downsideDeviation(xs) {
  if (xs.length < 2) return 0;
  const v = xs.reduce((s, x) => s + Math.min(0, x) ** 2, 0) / (xs.length - 1);
  return Math.sqrt(v);
}

function round(n, dp = 4) {
  const f = 10 ** dp;
  return Math.round(Number(n) * f) / f;
}
