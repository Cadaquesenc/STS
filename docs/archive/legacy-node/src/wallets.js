// Who is buying, rather than how many are buying.
//
// STS has always scored a coin by the size of its opening crowd. Measured over
// 12 Jul - 10 Aug on Dune and scored on local days it never covered, that is the
// wrong question: coins with a known-good wallet in within three seconds ran
// 38.4% of the time against 8.3% without, on a 16.4% baseline. Crowd size adds
// nothing once a crowd exists — at 8-15 early buyers the wallet identity made no
// difference at all.
//
// So this holds the registry: every wallet worth recognising, what its record
// is, and how much confidence that record deserves.

import fs from 'node:fs';
import path from 'node:path';
import { readWalletRecords } from './dune.js';

/** A record needs this many coins behind it before it means anything. */
export const MIN_COINS = 10;

/** Fraction of ranked wallets treated as the top tier. */
export const TOP_FRACTION = 0.25;

export class WalletBook {
  constructor() {
    this.byWallet = new Map();
    this.ranked = [];
    this.builtFrom = null;
    this.clusters = new Map(); // signature -> wallets that share it exactly
  }

  get size() {
    return this.byWallet.size;
  }

  /**
   * Load the registry written by the Dune ingest. Local observations can be
   * folded in later, but they must never be the base: the watcher sees roughly
   * one launch in fourteen on the public RPC, so a locally-built record is a
   * 7% sample pretending to be a track record.
   */
  loadDune(file) {
    const rows = readWalletRecords(file);
    if (!rows.length) return 0;
    for (const r of rows) {
      if (!r.wallet || !Number.isFinite(r.meanPeak)) continue;
      this.byWallet.set(r.wallet, {
        wallet: r.wallet,
        coins: r.coins,
        meanPeak: r.meanPeak,
        runRate: r.runRate,
        rate2x: r.rate2x,
        rate5x: r.rate5x,
        avgEarlyBuyers: r.avgEarlyBuyers,
        avgSolIn: r.avgSolIn,
        firstDay: r.firstDay,
        lastDay: r.lastDay,
        source: 'dune',
      });
    }
    this.builtFrom = file;
    this.rank();
    return this.byWallet.size;
  }

  /**
   * Sort by record and mark the tiers. Only wallets with enough coins behind
   * them are ranked at all — a wallet that got lucky once outranks everything
   * on a mean, and means nothing.
   */
  rank() {
    this.ranked = [...this.byWallet.values()]
      .filter((w) => w.coins >= MIN_COINS)
      .sort((a, b) => b.meanPeak - a.meanPeak);
    const cut = Math.max(1, Math.floor(this.ranked.length * TOP_FRACTION));
    this.ranked.forEach((w, i) => {
      w.rank = i + 1;
      w.tier = i < cut ? 'top' : i < cut * 2 ? 'good' : 'known';
    });
    this.findClusters();
  }

  /**
   * Wallets whose records match to the digit are not independent buyers. They
   * are one operator running several addresses through the same coins, which is
   * worth seeing in its own right — both as an insider group to follow and as a
   * warning that their "record" is really one record counted many times.
   */
  findClusters() {
    this.clusters.clear();
    const groups = new Map();
    for (const w of this.ranked) {
      // Same number of coins and the same mean peak to four decimals means the
      // same coin set. The amount each address put in differs and must not be
      // part of the signature — that is exactly how one operator hides as
      // several buyers.
      const sig = `${w.coins}|${w.meanPeak}|${w.runRate}`;
      if (!groups.has(sig)) groups.set(sig, []);
      groups.get(sig).push(w.wallet);
    }
    for (const [sig, members] of groups) {
      if (members.length < 2) continue;
      this.clusters.set(sig, members);
      for (const m of members) {
        const w = this.byWallet.get(m);
        if (w) { w.cluster = sig; w.clusterSize = members.length; }
      }
    }
  }

  get(wallet) {
    return this.byWallet.get(wallet) ?? null;
  }

  /** The known wallets that were in a coin before the cutoff, best first. */
  earlyKnown(coin, cutoffSec = 3) {
    const out = [];
    for (const w of coin.who || []) {
      if (w.at > cutoffSec || !(w.in > 0)) continue;
      const rec = this.byWallet.get(w.w);
      if (rec && rec.coins >= MIN_COINS) out.push({ ...rec, solIn: w.in, at: w.at });
    }
    return out.sort((a, b) => b.meanPeak - a.meanPeak);
  }

  /** Top of the book, for the interface. */
  top(limit = 100) {
    return this.ranked.slice(0, limit);
  }
}

/** Where the ingest drops its file. */
export function walletFile(dir) {
  return path.join(dir, 'wallets-dune.jsonl');
}

export function loadBook(dir) {
  const book = new WalletBook();
  const file = walletFile(dir);
  try {
    if (fs.existsSync(file)) book.loadDune(file);
  } catch {
    // An unreadable registry is a degraded interface, not a dead one.
  }
  return book;
}
