// What a coin looks like right now — and the one rule about arithmetic that the
// board kept breaking.
//
// Nearly every figure on the board is a ratio: a price against the price you
// could have entered at, one wallet's SOL against the opening's, the curve
// position against graduation. And a fifth of what the watcher records has a
// zero or a missing number underneath one of those. In this corpus 961 of 4,318
// coins opened with no trade at all inside the cutoff, so `outcome.entry` is
// null for them; 28 of those went on to trade afterwards, which is the exact
// shape that made the board divide a real price by a missing one and print
// `Infinity%` — or `NaN%`, when both sides were missing — at somebody who was
// about to spend money on the answer.
//
// So there is one rule here and everything else follows from it:
//
//   a ratio whose denominator is missing or zero is null, never a number.
//
// Null travels all the way to the screen, where it renders as an em dash. That
// distinction is the whole point: a figure nobody could work out has to look
// different from one that came out as zero. "This coin is flat" and "this coin
// never had a price to be flat against" are not the same sentence, and a board
// that says the first when it means the second is lying in the direction of
// making a coin look tradeable.
//
// None of this rounds, clamps, or substitutes a default. A guard that quietly
// returns 0 for an unknown ratio is the same bug wearing a better disguise.
import { bondingState } from './entry.js';

/** The number, or null. Strings, nulls, NaN and both infinities all fail. */
export function finite(value) {
  const n = Number(value);
  return Number.isFinite(n) ? n : null;
}

/**
 * `top / bottom`, or null if that division cannot be trusted.
 *
 * Zero on the bottom is the case this exists for, but a missing top is refused
 * just as hard: 0 / 5 is a measurement and null / 5 is the absence of one.
 */
export function ratio(top, bottom) {
  const a = finite(top);
  const b = finite(bottom);
  if (a === null || b === null || b === 0) return null;
  const out = a / b;
  return Number.isFinite(out) ? out : null;
}

/** How far `now` is above or below `then`, in percent. Null if `then` is no price. */
export function changePct(now, then) {
  const r = ratio(now, then);
  return r === null ? null : (r - 1) * 100;
}

/** `now` as a multiple of `then`. Null on the same terms. */
export function multiple(now, then) {
  return ratio(now, then);
}

/** `part` as a percentage of `whole`. Null when there is no whole to be part of. */
export function share(part, whole) {
  const r = ratio(part, whole);
  return r === null ? null : r * 100;
}

/** Round, but keep null as null rather than turning it into 0. */
export function round(value, dp = 4) {
  const n = finite(value);
  if (n === null) return null;
  const f = 10 ** dp;
  return Math.round(n * f) / f;
}

/**
 * Everything about a coin that changes between one trade and the next, in the
 * shape the board's live columns read.
 *
 * This is what makes a column updatable in place: the browser is sent the
 * figures rather than the inputs, so a row can be repainted cell by cell
 * without the page recomputing anything, and without a table re-render.
 *
 * The bonding position comes back through entry.js's `bondingState`, handed the
 * new price — the curve arithmetic stays in pump.js where it is tested, and
 * this does not grow a second copy of it that can drift.
 */
export function liveFigures(coin, price, { now = Date.now() } = {}) {
  const c = coin || {};
  const last = finite(price) ?? finite(c.last) ?? finite(c.outcome?.last);
  const entry = finite(c.entry) ?? finite(c.outcome?.entry);
  // Handing `last` in rather than reading it off the record: the whole reason
  // to call this is that a price just arrived which the record does not have.
  const bonding = last === null ? null : bondingState({ ...c, last });

  return {
    price: last,
    entryPrice: entry,
    changePct: round(changePct(last, entry), 2),
    mult: round(multiple(last, entry), 4),
    // Cumulative SOL through this coin since the watcher first saw it, which is
    // a different figure from `open.solIn` — that one is frozen at the cutoff
    // on purpose and must not be allowed to drift.
    volSol: round(finite(c.volSol), 3),
    openSol: round(finite(c.solIn) ?? finite(c.open?.solIn), 3),
    bondingPct: bonding?.known ? round(share(bonding.pct, 1), 2) : null, // pct is 0..1
    toGraduationSol: bonding?.known ? round(bonding.toGraduationSol, 2) : null,
    ageSec: c.t ? Math.max(0, Math.round((now - c.t) / 1000)) : null,
  };
}
