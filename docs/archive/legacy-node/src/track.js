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

/** Multiples of the entry price whose first crossing is worth knowing. */
export const LADDER = [0.3, 0.5, 0.7, 0.85, 0.95, 1.25, 1.5, 2, 3, 5, 10];

const UP = LADDER.filter((x) => x >= 1);
const DOWN = LADDER.filter((x) => x < 1);

export const MAX_AGE_MS = 12 * 60 * 60 * 1000;

export class Tracker {
  constructor({ dir = null, save = false, maxAgeMs = MAX_AGE_MS } = {}) {
    this.dir = dir;
    this.save = save && !!dir;
    this.maxAgeMs = maxAgeMs;
    this.coins = new Map(); // mint -> entry
    this.evicted = 0;
    this.pending = new Map(); // day -> lines waiting to be appended
  }

  /**
   * Take over a coin at the moment the watcher writes it down and lets it go.
   * Coins with no measurable entry price cannot be scored against anything, so
   * they are watched for their current price but never enter the model.
   */
  adopt(coin, assessment = null) {
    const entry = coin.outcome?.entry ?? coin.entry ?? null;
    const open = coin.open || {};
    this.coins.set(coin.mint, {
      mint: coin.mint,
      symbol: coin.symbol,
      name: coin.name ?? null,
      t: coin.t,
      entry,
      last: coin.outcome?.last ?? entry,
      lastTradeAt: coin.t + (coin.outcome?.follow ?? 60) * 1000,
      hi: 1,
      lo: 1,
      peakAtSec: coin.outcome?.peakAtSec ?? null,
      cross: Object.create(null),
      wallets: Number(open.wallets || 0),
      sellers: Number(open.sellers || 0),
      solIn: Number(open.solIn || 0),
      trades: Number(open.trades || 0),
      // Early wallets travel with the coin: the board scores by who bought, so
      // dropping them here would leave a tracked coin unscoreable.
      who: (coin.who || []).filter((w) => w.at <= (coin.open?.seconds ?? 3)),
      creator: coin.creator ?? null,
      kind: coin.social?.kind ?? null,
      handle: coin.social?.handle ?? null,
      nth: coin.social?.nth ?? null,
      score: assessment?.score ?? null,
      eligible: assessment?.eligible ?? false,
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
  persist(c) {
    if (!this.save) return;
    try {
      const day = new Date(c.t).toISOString().slice(0, 10);
      if (!this.pending.has(day)) this.pending.set(day, []);
      this.pending.get(day).push(
        JSON.stringify({
          mint: c.mint, symbol: c.symbol, t: c.t, entry: c.entry, last: c.last,
          // How long this coin was actually watched. Without it a coin dropped
          // at shutdown looks the same as one that ran the full window, and
          // every unresolved rule would silently score as a time exit.
          watchedSec: Math.max(0, Math.round((Date.now() - c.t) / 1000)),
          hi: r(c.hi), lo: r(c.lo), peakAtSec: c.peakAtSec, cross: c.cross,
          wallets: c.wallets, sellers: c.sellers, solIn: r(c.solIn), trades: c.trades,
          kind: c.kind, nth: c.nth, score: c.score, eligible: c.eligible,
        }) + '\n',
      );
    } catch {
      // Losing a tracked coin is not worth taking the watcher down for.
    }
  }

  /** Write buffered lines to disk. Returning means they are durable. */
  flush() {
    if (!this.save || !this.pending.size) return;
    try {
      fs.mkdirSync(this.dir, { recursive: true });
      for (const [day, lines] of this.pending) {
        if (lines.length) fs.appendFileSync(path.join(this.dir, `tracks-${day}.jsonl`), lines.join(''));
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

function r(n, dp = 4) {
  const f = 10 ** dp;
  return Math.round(Number(n) * f) / f;
}
