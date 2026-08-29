// Coins the watcher has stopped writing down but has not stopped watching.
//
// The watcher forgets a coin 60 seconds after it launches, which means the
// system has never known any coin's current price. Everything past that first
// minute — whether a target was ever reached, whether a stop would have fired,
// what the thing is worth right now — was simply unavailable.
//
// Continuing to watch is free. The subscription is one firehose over the whole
// pump.fun program, so trades for every coin arrive whether we look at them or
// not; until now they were parsed and thrown away.
//
// What is not free is memory. At the observed rate of about 30 launches a minute,
// a twelve-hour window holds over twenty thousand coins, so keeping a price
// series for each one is out of the question. It is also unnecessary. To score a
// rule like "take 50%, stop at -15%, give up after 15 minutes" the only thing
// that matters is *when each threshold was first crossed*. That is a handful of
// numbers per coin no matter how long the window is, and because it is recorded
// as the trades arrive it knows the true order of events — which a candle can
// only guess at.

import fs from 'node:fs';
import path from 'node:path';
import { jsonLine } from './record.js';
import { SCHEMA } from './session.js';

/** Multiples of the entry price whose first crossing is worth knowing. */
export const LADDER = [0.3, 0.5, 0.7, 0.85, 0.95, 1.25, 1.5, 2, 3, 5, 10];

const UP = LADDER.filter((x) => x >= 1);
const DOWN = LADDER.filter((x) => x < 1);

export const MAX_AGE_MS = 12 * 60 * 60 * 1000;

export class Tracker {
  /**
   * `session` is the filename infix the whole run shares, and it replaces the
   * launch date. A tracked coin is followed for twelve hours, so under the old
   * naming one run's tracks landed in two or three files split at UTC midnight
   * — the same split that turned a fifteen-hour capture into a tuning day plus
   * a fictional holdout. Without a session it keeps the dated naming, so the
   * existing corpus still reads exactly as it did.
   */
  constructor({ dir = null, save = false, maxAgeMs = MAX_AGE_MS, session = null, sid = null } = {}) {
    this.dir = dir;
    this.save = save && !!dir;
    this.maxAgeMs = maxAgeMs;
    this.session = session;
    this.sid = sid;
    this.coins = new Map(); // mint -> entry
    this.evicted = 0;
    this.pending = new Map(); // file infix -> lines waiting to be appended
  }

  /**
   * Take over a coin at the moment the watcher writes it down and lets it go.
   * Coins with no measurable entry price cannot be scored against anything, so
   * they are watched for their current price but never enter the model.
   *
   * `entry` is deliberately *not* re-struck here. It is `coin.outcome.entry` —
   * the price at the three-second mark, which is what a strategy would actually
   * have paid — so `hi`, `lo`, `last` and every rung of `cross` stay measured
   * against that one price all the way out to twelve hours. Verified on all
   * 4,083 joins in the recorded corpus: not one row re-bases. Six sprint reports
   * assumed otherwise and wrote off the long horizons because of it.
   *
   * What *is* re-struck is the observation window. `hi`, `lo` and `cross` start
   * empty because this window begins where the coin record ends, at the follow
   * mark. `peakAtSec` has to start empty with them or the row contradicts
   * itself: carrying the first minute's peak time next to a freshly reset `hi`
   * produced 929 rows on one day reading `hi: 1, peakAtSec: 14` — "the price
   * never beat entry" and "the best price was at second 14" in the same record.
   * Nothing is lost by dropping it. The first minute's peak time is already
   * written in coins-*.jsonl as `outcome.peakAtSec`; a reader who wants both
   * joins on `mint`.
   */
  adopt(coin) {
    const entry = coin.outcome?.entry ?? coin.entry ?? null;
    const open = coin.open || {};
    this.coins.set(coin.mint, {
      mint: coin.mint,
      // Which run watched this coin. The tracker outlives the coin record by
      // twelve hours, so without it a tracks row cannot be joined back to the
      // session whose uptime says whether it was being watched at all.
      sid: coin.sid ?? this.sid ?? null,
      symbol: coin.symbol,
      name: coin.name ?? null,
      t: coin.t,
      entry,
      last: coin.outcome?.last ?? entry,
      lastTradeAt: coin.t + (coin.outcome?.follow ?? 60) * 1000,
      // Reset together, always. See the note above.
      hi: 1,
      lo: 1,
      peakAtSec: null,
      cross: Object.create(null),
      wallets: Number(open.wallets || 0),
      sellers: Number(open.sellers || 0),
      solIn: Number(open.solIn || 0),
      trades: Number(open.trades || 0),
      // The frozen opening, carried whole. The seller check needs both sides of
      // it and the coin has stopped being written down by the time it gets here.
      open: {
        seconds: open.seconds ?? 3,
        solIn: Number(open.solIn || 0),
        solOut: Number(open.solOut || 0),
        sellers: Number(open.sellers || 0),
      },
      // Early wallets travel with the coin: the board scores by who bought, so
      // dropping them here would leave a tracked coin unscoreable.
      who: (coin.who || []).filter((w) => w.at <= (coin.open?.seconds ?? 3)),
      creator: coin.creator ?? null,
      // The supply check and the bonding-curve position are asked of tracked
      // coins too, and neither can be worked out from a price alone.
      supply: coin.supply ?? null,
      curve: coin.curve ?? null,
      initialBuySol: coin.initialBuySol ?? null,
      initialBuyTokens: coin.initialBuyTokens ?? null,
      kind: coin.social?.kind ?? null,
      handle: coin.social?.handle ?? null,
      nth: coin.social?.nth ?? null,
      // `score` and `eligible` used to be here. They were filled from a second
      // `assessment` argument that the one caller — watch.js's finish() — never
      // passed, so they were `null` and `false` on all 5,003 rows ever written.
      // A field that is always null is worse than no field, because a reader
      // trusts it. If a score is wanted later, pass one and write it then.
      graduated: false,
    });
  }

  /**
   * A trade arrived for a coin past its follow window. Update the current price
   * and note any threshold this is the first to cross.
   */
  trade(mint, price, at = Date.now()) {
    const c = this.coins.get(mint);
    if (!c) return false;
    c.last = price;
    c.lastTradeAt = at;
    if (!c.entry) return true;

    const mult = price / c.entry;
    const sec = Math.round((at - c.t) / 1000);

    // Only a new extreme can cross anything, so the common case costs one compare.
    if (mult > c.hi) {
      c.hi = mult;
      c.peakAtSec = sec;
      for (const level of UP) {
        if (mult >= level && c.cross[level] === undefined) c.cross[level] = sec;
      }
    } else if (mult < c.lo) {
      c.lo = mult;
      for (const level of DOWN) {
        if (mult <= level && c.cross[level] === undefined) c.cross[level] = sec;
      }
    }
    return true;
  }

  /**
   * Drop coins past the longest window we answer questions about, and write what
   * was learned so a restart does not begin from nothing.
   */
  sweep(now = Date.now()) {
    for (const [mint, c] of this.coins) {
      if (now - c.t < this.maxAgeMs) continue;
      this.coins.delete(mint);
      this.evicted++;
      this.persist(c);
    }
    this.flush();
  }

  /** Coins whose last trade is long enough ago to be suspicious. */
  static isStale(c, now = Date.now(), quietMs = 10 * 60 * 1000) {
    return now - c.lastTradeAt > quietMs;
  }

  rows() {
    return [...this.coins.values()];
  }

  /**
   * One tracked coin, by mint. The board needs a single row on every tick to
   * recompute that row's live figures, and scanning `rows()` for it turned a
   * per-trade lookup into a walk of everything being tracked.
   */
  get(mint) {
    return this.coins.get(mint) ?? null;
  }

  get size() {
    return this.coins.size;
  }

  /**
   * Queue a coin for writing. Nothing is streamed: a write stream flushes on a
   * later tick, and the one moment this data matters most is shutdown, when
   * Electron quits the process immediately after asking us to stop. A stream
   * loses the entire run there, silently. Lines are buffered and written with
   * a synchronous append instead, so returning means it is on disk.
   */
  persist(c, now = Date.now()) {
    if (!this.save) return;
    try {
      const infix = this.session ?? new Date(c.t).toISOString().slice(0, 10);
      if (!this.pending.has(infix)) this.pending.set(infix, []);
      this.pending.get(infix).push(jsonLine(trackRow(c, now)) + '\n');
    } catch {
      // Losing a tracked coin is not worth taking the watcher down for.
    }
  }

  /** Write buffered lines to disk. Returning means they are durable. */
  flush() {
    if (!this.save || !this.pending.size) return;
    try {
      fs.mkdirSync(this.dir, { recursive: true });
      for (const [infix, lines] of this.pending) {
        if (lines.length) fs.appendFileSync(path.join(this.dir, `tracks-${infix}.jsonl`), lines.join(''));
      }
      this.pending.clear();
    } catch {
      // Keep the lines buffered rather than dropping them; the next flush retries.
    }
  }

  /** Flush everything still held — used on shutdown so a run is not wasted. */
  close() {
    if (this.save) for (const c of this.coins.values()) this.persist(c);
    this.flush();
  }
}

/**
 * One line of tracks-*.jsonl, built from one tracked coin.
 *
 * Pure and exported so the shape can be tested without a socket, a clock or a
 * disk — which is what the two defects this file used to carry needed and never
 * had. `checkTrackRow` states the invariant the row must satisfy.
 */
export function trackRow(c, now = Date.now()) {
  return {
    // `tracks-<session>.jsonl` is its own file with no `start` header in it, so
    // the row is the only place its shape can be written down.
    v: SCHEMA,
    mint: c.mint, symbol: c.symbol, t: c.t, sid: c.sid ?? null, entry: c.entry, last: c.last,
    // How long this coin was actually watched. Without it a coin dropped
    // at shutdown looks the same as one that ran the full window, and
    // every unresolved rule would silently score as a time exit.
    watchedSec: Math.max(0, Math.round((now - c.t) / 1000)),
    // Measured against `entry` — the three-second price on the coin record,
    // never re-struck. This window starts at the follow mark, so `hi`, `lo`,
    // `peakAtSec` and `cross` all begin empty together.
    hi: r(c.hi), lo: r(c.lo), peakAtSec: c.peakAtSec, cross: c.cross,
    wallets: c.wallets, sellers: c.sellers, solIn: r(c.solIn), trades: c.trades,
    kind: c.kind, nth: c.nth,
  };
}

/**
 * The one thing a tracks row can be internally wrong about. Returns a list of
 * complaints, empty when the row is sound.
 *
 * `hi` is the best price this window has seen as a multiple of entry, and
 * `peakAtSec` is when that happened. They are written by the same branch of
 * `trade()`, so either both are set or neither is. A row saying the price never
 * beat entry *and* naming the second it peaked is the shape 929 rows had on
 * 2026-08-20, and it is not recoverable after the fact — it is two windows
 * mixed into one row.
 */
export function checkTrackRow(row) {
  const bad = [];
  const untouched = row.hi === 1;
  if (untouched && row.peakAtSec != null) {
    bad.push(`hi is 1 (price never beat entry) but peakAtSec is ${row.peakAtSec}`);
  }
  if (!untouched && row.hi > 1 && row.peakAtSec == null) {
    bad.push(`hi is ${row.hi} but no peakAtSec says when`);
  }
  if ('score' in row || 'eligible' in row) {
    bad.push('score/eligible were never filled in and must not be written');
  }
  return bad;
}

function r(n, dp = 4) {
  const f = 10 ** dp;
  return Math.round(Number(n) * f) / f;
}
