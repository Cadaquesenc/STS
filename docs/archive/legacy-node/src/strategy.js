// Does buying this kind of coin make money, and if so how should it be exited.
//
// The honest answer today is no, and this file is built so that it says no when
// that is true. Log.md records the reason: the best on-chain signal is worth
// about +1.34% a trade and the cheapest possible round trip costs about 3%. It
// also records what happens when you skip the discipline below — an exit rule
// picked and graded on the same days looked like +5.44%, and returned -3.90% on
// days nobody had looked at.
//
// So three rules are structural here rather than optional:
//   1. A rule is chosen on one set of days and reported on a different set.
//   2. Costs come off before anything is ranked or coloured.
//   3. A sample too small to mean anything reports as too small, not as a number.

import { roundTripCostPct, netReturnPct } from './cost.js';

/** Exit rules worth testing. Both legs must exist on the tracker's ladder. */
export const TARGETS = [1.25, 1.5, 2, 3];
export const STOPS = [0.95, 0.85, 0.7];

export const HORIZONS = [
  { key: '1m', label: '1m', seconds: 60 },
  { key: '5m', label: '5m', seconds: 300 },
  { key: '15m', label: '15m', seconds: 900 },
  { key: '30m', label: '30m', seconds: 1800 },
  { key: '1h', label: '1h', seconds: 3600 },
  { key: '5h', label: '5h', seconds: 18000 },
  { key: '12h', label: '12h', seconds: 43200 },
];

/** Below this a cohort reports "not enough data" instead of a percentage. */
export const MIN_SAMPLE = 30;

/**
 * What a single coin would have returned under one rule, using the recorded
 * first-crossing times. Returns null when the coin has not been watched long
 * enough for the answer to be known — counting those as anything would quietly
 * bias every number towards the coins that resolved early.
 */
export function outcome(c, { target, stop, holdSec }, now = Date.now()) {
  if (!c.entry) return null;
  // How long this coin has actually been watched. A coin still being watched is
  // observed up to now; one loaded from disk only as far as it got.
  const observedSec = c.watchedSec ?? (now - c.t) / 1000;
  const hitAt = c.cross?.[target];
  const stopAt = c.cross?.[stop];
  const hit = hitAt !== undefined && hitAt <= holdSec;
  const stopped = stopAt !== undefined && stopAt <= holdSec;

  if (hit && stopped) {
    // Same instant is treated as the stop: the pessimistic reading, and the one
    // that stops a rule looking better than it was.
    return stopAt <= hitAt
      ? { multiple: stop, reason: 'stop' }
      : { multiple: target, reason: 'target' };
  }
  if (hit) return { multiple: target, reason: 'target' };
  if (stopped) return { multiple: stop, reason: 'stop' };

  // Neither level was touched, so the exit is the clock — which is only a fact
  // once the clock has actually run out.
  if (observedSec < holdSec) return null;
  return { multiple: (c.last ?? c.entry) / c.entry, reason: 'time' };
}

/** Coins are grouped by what was visible before any money moved. */
export function cohortKey(c) {
  const w = Number(c.wallets || 0);
  const band = w >= 16 ? '16+' : w >= 8 ? '8-15' : w >= 4 ? '4-7' : '0-3';
  const social = c.kind === 'tweet' ? 'fresh tweet' : c.kind === 'nometa' ? 'no metadata' : 'other';
  return `${band} buyers · ${social}`;
}

function summarise(rows, sizeSol) {
  if (!rows.length) return null;
  const mean = rows.reduce((s, r) => s + r.multiple, 0) / rows.length;
  const wins = rows.filter((r) => r.multiple > 1).length;
  const cost = roundTripCostPct(sizeSol);
  return {
    sample: rows.length,
    grossPct: round((mean - 1) * 100),
    netPct: netReturnPct(mean, sizeSol),
    costPct: cost.totalPct,
    hitRate: round((wins / rows.length) * 100, 2),
    targetRate: round((rows.filter((r) => r.reason === 'target').length / rows.length) * 100, 2),
  };
}

/**
 * Split by day so a rule is never graded on the days that picked it. The most
 * recent day reports; everything before it chooses. One day of data cannot do
 * this, and says so rather than pretending.
 */
export function splitByDay(coins) {
  const days = new Map();
  for (const c of coins) {
    const d = new Date(c.t).toISOString().slice(0, 10);
    if (!days.has(d)) days.set(d, []);
    days.get(d).push(c);
  }
  const keys = [...days.keys()].sort();
  if (keys.length < 2) return { choose: coins, report: coins, validated: false, days: keys.length };
  const reportDay = keys.at(-1);
  return {
    choose: keys.slice(0, -1).flatMap((k) => days.get(k)),
    report: days.get(reportDay),
    validated: true,
    days: keys.length,
  };
}

/**
 * Build the model: for every cohort and horizon, the exit rule that did best on
 * the choosing days, scored on the reporting days.
 */
export function buildModel(coins, { sizeSol = 0.25, now = Date.now() } = {}) {
  const usable = coins.filter((c) => c.entry);
  const { choose, report, validated, days } = splitByDay(usable);
  const cohorts = new Map();

  for (const horizon of HORIZONS) {
    const holdSec = horizon.seconds;
    const byCohortChoose = groupBy(choose, cohortKey);
    const byCohortReport = groupBy(report, cohortKey);

    for (const [key, chooseRows] of byCohortChoose) {
      let best = null;
      for (const target of TARGETS) {
        for (const stop of STOPS) {
          const outcomes = chooseRows
            .map((c) => outcome(c, { target, stop, holdSec }, now))
            .filter(Boolean);
          const stats = summarise(outcomes, sizeSol);
          if (!stats || stats.sample < MIN_SAMPLE) continue;
          if (!best || stats.netPct > best.stats.netPct) best = { target, stop, stats };
        }
      }
      if (!best) continue;

      const reportRows = (byCohortReport.get(key) || [])
        .map((c) => outcome(c, { target: best.target, stop: best.stop, holdSec }, now))
        .filter(Boolean);
      const scored = summarise(reportRows, sizeSol);

      if (!cohorts.has(key)) cohorts.set(key, {});
      cohorts.get(key)[horizon.key] = {
        rule: { target: best.target, stop: best.stop, holdSec },
        chosenOn: best.stats,
        result: scored && scored.sample >= MIN_SAMPLE ? scored : null,
        thin: !scored || scored.sample < MIN_SAMPLE,
        sample: scored?.sample ?? 0,
      };
    }
  }

  return {
    cohorts: Object.fromEntries(cohorts),
    validated,
    days,
    sizeSol,
    cost: roundTripCostPct(sizeSol),
    coinsConsidered: usable.length,
    builtAt: now,
  };
}

/** What the model expects from one live coin, at one horizon. */
export function expectation(model, coin, horizonKey) {
  const cell = model.cohorts?.[cohortKey(coin)]?.[horizonKey];
  if (!cell) return { known: false, reason: 'no model for this kind of coin yet' };
  if (!cell.result) {
    return { known: false, reason: `only ${cell.sample} resolved examples so far`, rule: cell.rule };
  }
  return {
    known: true,
    netPct: cell.result.netPct,
    grossPct: cell.result.grossPct,
    hitRate: cell.result.hitRate,
    targetRate: cell.result.targetRate,
    sample: cell.result.sample,
    rule: cell.rule,
    validated: model.validated,
  };
}

function groupBy(rows, keyOf) {
  const m = new Map();
  for (const r of rows) {
    const k = keyOf(r);
    if (!m.has(k)) m.set(k, []);
    m.get(k).push(r);
  }
  return m;
}

function round(n, dp = 2) {
  const f = 10 ** dp;
  return Math.round(Number(n) * f) / f;
}
