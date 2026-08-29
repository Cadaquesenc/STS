// The recorder grading its own output.
//
// Every defect this producer was found to have was invisible in exactly the same
// way: a field that was written on every row and said the same thing on every
// row. `outcome.follow` was the literal 60 on all 8,881 records. `funding.depth`
// was the literal 2 on all 5,659. `eligible` was false and `score` was null on
// all 5,003. None of them looked wrong. A row carrying them reads perfectly.
//
// W21's check C7 is the general form — "no scalar field has exactly one distinct
// value across the corpus" — and it would have caught all four on day one. It
// lives here rather than in an analyst's notebook because the producer is the
// thing that should be embarrassed by its own dead fields, and because a check
// that ships next to the code that writes the file cannot fall out of date with
// it quietly.
//
// Nothing here runs on the hot path. It reads finished files.

import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';
import { checkTrackRow } from './track.js';
import { sessions, SCHEMA, schemaStatus } from './session.js';

/**
 * Fields that are genuinely constant and are supposed to be.
 *
 * A constant is not automatically a defect. `supply` is a pump.fun protocol
 * constant; `market.candleSeconds` and `open.seconds` are the configuration the
 * row was produced under, and a reader that has to guess the cutoff cannot tell
 * an opening window from a follow window. What makes a constant a defect is
 * nobody having decided it should be one — so each of these carries a reason,
 * and anything not on this list is reported.
 *
 * W21 §1 wants the protocol constants moved to a session header instead. Until
 * there is a session header they stay on the row and stay declared here.
 */
export const DECLARED_CONSTANTS = {
  supply: 'pump.fun mints a fixed supply; a different value means the protocol moved',
  'curve.virtualSol': 'the launch curve constant, read off the event rather than assumed',
  'curve.virtualTokens': 'the launch curve constant, read off the event rather than assumed',
  'curve.realTokens': 'the launch curve constant, read off the event rather than assumed',
  'open.seconds': 'the opening cutoff this row was frozen at — configuration, but a reader needs it',
  'market.candleSeconds': 'the candle width this row was written at',
  // `follow` on its own was the defect: the configured window written on every
  // row as if it were the observed one, including the ~14% of coins cut off at
  // shutdown. It is allowed to stay a constant now only because `observedSec`
  // sits beside it saying what really happened — and a row that has one without
  // the other is complained about per-row, below.
  'outcome.follow': 'the window this row was promised — paired with outcome.observedSec, which says what it got',
  sid: 'one file per session; constant by construction, and that is the point',
  v: 'the schema version this file was written at',
  // Four fields whose whole job is to be boring. A session where every row is
  // complete, unbroken and uncapped is the goal, not a dead field — but the
  // moment one of them varies, the census stops reporting it and the row-level
  // checks below pick it up instead.
  'outcome.complete': 'true on every row is the target, not a defect — see stopReason and gapSec',
  'outcome.stopReason': "'window' on every row is the target: nothing was cut off",
  'outcome.gapSec': '0 on every row is the target: the feed was up for every coin window',
  'outcome.highsCapped': 'false on every row is the target: no coin ran out of turning points',
  'outcome.lowsCapped': 'false on every row is the target: no coin ran out of turning points',
  'outcome.sellsCapped': 'false on every row is the target: no coin ran out of room for sells',
  'outcome.zeroFeeCapped': 'false on every row is the target: no coin ran out of room for zero-fee trades',
  'outcome.zeroFeeTrades': '0 on every row is the target: no trade on any coin skipped the fee',
  'outcome.curveSuspect': 'false on every row is the target: no coin was touched by a zero-fee trade',
  whoCapped: 'false on every row is the target: no coin turned a wallet away at the 200 cap',
};

/**
 * The virtual SOL a pump curve opens at, used only when the row does not carry
 * its own launch curve.
 *
 * It is a fallback and not a constant. 30 is what almost every launch opens at,
 * but **216 of the 7,926 recorded coins that carry a curve open at 4.292** — so
 * a hardcoded floor of 30 would call every candle of those coins impossible.
 * The row's own `curve.virtualSol` is the floor whenever it has one, which is
 * the same rule `watch.js` follows in reading the opening state off the event
 * rather than assuming it.
 */
export const CURVE_FLOOR_SOL = 30;

/** The floor below which this particular coin's curve cannot go. */
export function curveFloor(record) {
  const open = Number(record?.curve?.virtualSol);
  return open > 0 ? open : CURVE_FLOOR_SOL;
}

/**
 * How big the printed peak was, for reporting curve conservation against it.
 *
 * The whole finding is a gradient — the rate at which a peak turns out to be
 * unbacked climbs steeply with the size of the peak — so a single number for the
 * whole file hides it. Same brackets W32 used, so the two can be compared.
 */
export const PEAK_BUCKETS = [
  { label: '1–1.5x', from: 1, to: 1.5 },
  { label: '1.5–2x', from: 1.5, to: 2 },
  { label: '2–3x', from: 2, to: 3 },
  { label: '3–5x', from: 3, to: 5 },
  { label: '5–10x', from: 5, to: 10 },
  { label: 'above 10x', from: 10, to: Infinity },
];

/**
 * Walk a record and hand every scalar leaf to `visit(path, value)`.
 *
 * Arrays are skipped whole, as W21's own version does: an array's contents vary
 * by construction and a per-element census says nothing about whether the field
 * carries information.
 */
export function walk(value, visit, prefix = '') {
  if (value && typeof value === 'object' && !Array.isArray(value)) {
    for (const [key, child] of Object.entries(value)) {
      walk(child, visit, prefix ? `${prefix}.${key}` : key);
    }
  } else if (!Array.isArray(value)) {
    visit(prefix, value);
  }
}

/**
 * A running census of how many distinct values each field has taken.
 *
 * Bounded: it stops collecting distinct values for a field once it has seen
 * more than one, because one is the only count the question turns on. A day of
 * coins is 23 MB and 3,000 fields; holding every value would be the expensive
 * way to answer a yes/no.
 */
export function census() {
  const seen = new Map(); // path -> { rows, values:Set (capped), overflowed }

  return {
    add(record) {
      walk(record, (path, value) => {
        let entry = seen.get(path);
        if (!entry) {
          entry = { rows: 0, values: new Set(), overflowed: false };
          seen.set(path, entry);
        }
        entry.rows++;
        if (entry.overflowed) return;
        entry.values.add(typeof value === 'object' ? 'null' : `${typeof value}:${String(value)}`);
        if (entry.values.size > 1) entry.overflowed = true;
      });
    },

    /**
     * @returns {{rows:number, fields:number, dead:Array, declared:Array}}
     *   `dead` is every field with one distinct value that nobody declared.
     */
    report(rows) {
      const dead = [];
      const declared = [];
      const paths = [...seen.keys()];
      // A whole block that is null on the rows that lack it — `funding` on a
      // launch nobody asked about, `social` on a coin with no metadata — reads
      // as a constant-null scalar on those rows only. It is not a dead field:
      // the block is populated elsewhere, and its own leaves are censused
      // separately. Crying wolf about it is how a check gets ignored.
      const isNullableBlock = (p) => paths.some((other) => other.startsWith(`${p}.`));
      for (const [path, entry] of [...seen].sort()) {
        if (entry.overflowed || isNullableBlock(path)) continue;
        const only = [...entry.values][0] ?? 'null';
        const row = { path, value: only.replace(/^[a-z]+:/, ''), rows: entry.rows };
        if (path in DECLARED_CONSTANTS) declared.push({ ...row, why: DECLARED_CONSTANTS[path] });
        else dead.push(row);
      }
      return { rows, fields: seen.size, dead, declared };
    },
  };
}

/**
 * Per-row invariants, for the two defects this producer used to carry.
 *
 * A census catches a field that never varies. It cannot catch a row that
 * contradicts itself — `hi: 1` beside `peakAtSec: 14` varies just fine across
 * the corpus and is impossible on every single row it appears on. That needs
 * the row read as a whole, which is what this does.
 *
 * @returns {string[]} complaints, empty when the row is sound
 */
export function checkRow(record) {
  const bad = [];

  // tracks-*.jsonl: one observation window, all of it reset together.
  if ('cross' in record && 'hi' in record) bad.push(...checkTrackRow(record));

  // coins-*.jsonl: the funding block has to say how far it actually walked.
  const funding = record.funding;
  if (funding && typeof funding === 'object') {
    if ('depth' in funding) {
      bad.push('funding.depth is the configured cap echoed back; write hopsWalked instead');
    }
    if (!('hopsWalked' in funding)) {
      bad.push('funding block with no hopsWalked — the row cannot say how far it looked');
    }
    if (Array.isArray(funding.perHop) && funding.perHop.length !== funding.hopsWalked) {
      bad.push(`funding.perHop has ${funding.perHop.length} entries but hopsWalked is ${funding.hopsWalked}`);
    }
    const census_ = funding.status;
    if (census_ && funding.requested != null) {
      const total = Object.values(census_).reduce((a, b) => a + b, 0);
      if (total !== funding.requested) {
        bad.push(`funding.status sums to ${total} but ${funding.requested} wallets were asked about`);
      }
    }
  }

  if ('score' in record || 'eligible' in record) {
    bad.push('score/eligible were never filled in and must not be written');
  }

  // The shape the row says it is. Asked of every row, coin or track, because a
  // version this build has never heard of means every other complaint below was
  // produced by rules written for a different file.
  const status = schemaStatus(record.v);
  if (status === 'ahead') {
    bad.push(`schema v${record.v} was written by a newer recorder than this one, which knows up to v${SCHEMA}`);
  } else if (status === 'unknown') {
    bad.push(`schema v${record.v} is not a version this build knows how to read`);
  }

  // coins-*.jsonl: a launch record. Everything below is a defect this corpus
  // actually carries, so pointed at the recorded days every one of them fires.
  const outcome = record.outcome;
  if (outcome && typeof outcome === 'object') {
    bad.push(...checkOutcome(record, outcome));
  }

  // A tracks row trips both the tracker check and the one above; say it once.
  return [...new Set(bad)];
}

/**
 * The sell ledger, and the counters it has to agree with.
 *
 * `market.candles[].sells` counted sells and named nobody, so "is the creator
 * still holding at second N" could only ever be asked as "has anybody sold by
 * second N" — different questions, and the difference is the whole sell-side
 * signal. The seller's address was on the trade event all along; it was thrown
 * away rather than missing.
 *
 * Every check below is the same rule in a different place: a number is only
 * worth having if the rows behind it are still there. `sells[]` is the rows;
 * `total.sellers`, the candle counts and `creatorSellAtSec` are numbers that
 * must fall out of it.
 */
export function checkSells(record, outcome) {
  const bad = [];
  const sells = outcome.sells;
  if (!Array.isArray(sells)) {
    bad.push('outcome has no sells — a sell can be counted but never attributed to a wallet');
    return bad;
  }
  if (outcome.sellsCapped === true) {
    bad.push('outcome.sells ran out of room — the sells on this row are a prefix, not the window');
    return bad; // every count below is a floor once the ledger is truncated
  }

  const candleSells = Array.isArray(record.market?.candles)
    ? record.market.candles.reduce((a, c) => a + (c.sells || 0), 0)
    : null;
  if (candleSells !== null && candleSells !== sells.length) {
    bad.push(`the candles count ${candleSells} sells but the ledger names ${sells.length}`);
  }

  const wallets = new Set(sells.map((x) => (Array.isArray(x) ? x[1] : x?.w)));
  if (record.total?.sellers != null && wallets.size !== record.total.sellers) {
    bad.push(`total.sellers says ${record.total.sellers} but ${wallets.size} distinct wallets sold`);
  }

  // A sell cannot happen before the coin exists, and the ledger is in order.
  let last = -Infinity;
  for (const x of sells) {
    const at = Array.isArray(x) ? x[0] : x?.at;
    if (!(at >= 0)) { bad.push('a sell with no time on it'); break; }
    if (at < last) { bad.push('the sell ledger is out of order'); break; }
    last = at;
  }

  if ('creatorSellAtSec' in outcome && record.creator) {
    const mine = sells.filter((x) => (Array.isArray(x) ? x[1] : x?.w) === record.creator);
    const first = mine.length ? Math.min(...mine.map((x) => (Array.isArray(x) ? x[0] : x.at))) : null;
    if ((outcome.creatorSellAtSec ?? null) !== first) {
      bad.push(`creatorSellAtSec says ${outcome.creatorSellAtSec} but the ledger says ${first}`);
    }
  }
  return bad;
}

/**
 * The curve state a row kept, and the two ways it can be shown to be impossible.
 *
 * W32 traced the "impossible price" problem that affects 18.4% of coins and
 * concentrates in nine of the ten largest peaks. It is **not** a decoder bug and
 * **not** a capture bug: those prices really printed on chain, and the chain
 * priced every following trade off the impossible reserve value. One actor is
 * responsible — 1,396 of 1,398 zero-fee trades on 2026-08-10 came from a single
 * wallet across 43 coins — and it is new: none in Oct 2024, four in Aug 2025,
 * then a quarter of the 2026 capture windows.
 *
 * Two independent routes catch it, which is why both are here:
 *
 *   1. **The fee rate.** Every corrupt trade paid `feeBasisPoints == 0`. Zero
 *      percent of those obey the launch curve; 92–95% of normal 95-bps trades
 *      do. It is an exact marker and it costs nothing to store.
 *   2. **Curve conservation.** To print a given price the curve has to have
 *      given up a given number of tokens, and it can never have given up more
 *      than were bought out of it. That is `curveConservation` below, it shares
 *      no code with the first, and it works on rows written before either field
 *      existed.
 *
 * Both run at capture time now, so an affected coin is quarantined when it is
 * recorded rather than found months later at the bottom of an expectancy table.
 */
export function checkCurve(record, outcome) {
  const bad = [];

  const bps = outcome.feeBps;
  if (bps == null) {
    bad.push('no feeBps — the zero-fee signature of the on-chain reserve anomaly cannot be seen');
  } else {
    const zero = Number(bps['0'] ?? 0);
    if (zero > 0) {
      bad.push(`${zero} trades paid zero fee — the exact marker of the actor whose trades leave the curve in an impossible state`);
      const ledger = Array.isArray(outcome.zeroFee) ? outcome.zeroFee.length : null;
      if (ledger !== null && outcome.zeroFeeCapped !== true && ledger !== zero) {
        bad.push(`feeBps says ${zero} zero-fee trades but the ledger holds ${ledger}`);
      }
    }
    // The counter rule, in the smallest place it applies: the count and the flag
    // an analyst reads at a glance have to fall out of the census and the ledger
    // behind them, or they are two more numbers to be taken on trust.
    if ('zeroFeeTrades' in outcome && outcome.zeroFeeTrades !== zero) {
      bad.push(`zeroFeeTrades says ${outcome.zeroFeeTrades} but the fee census counts ${zero}`);
    }
    if ('curveSuspect' in outcome && outcome.curveSuspect !== zero > 0) {
      bad.push(`curveSuspect is ${outcome.curveSuspect} with ${zero} zero-fee trades on the row`);
    }
    if (outcome.zeroFeeCapped === true) {
      bad.push('outcome.zeroFee ran out of room — the anomalous trades on this row are a prefix');
    }
  }

  // The candles kept only derived prices for the whole recorded corpus, so when
  // the reserves turned out to be the thing in question not one coin could be
  // repaired. This is the check that would have said so on day one.
  const candles = record.market?.candles;
  const floor = curveFloor(record);
  if (Array.isArray(candles) && candles.length) {
    if (!candles.some((c) => c && 'vsol' in c)) {
      bad.push('candles carry only derived prices — the curve state they came from was never written and cannot be recovered');
    } else {
      // All four reserve fields or none of them. The recorder always writes the
      // four keys, using null when the trade event's extended block would not
      // decode — so a candle carrying `vsol` and missing `rtok` is a half-applied
      // change rather than a layout the chain produced, and a reader that finds
      // three of four has no way to tell those apart.
      const partial = candles.filter(
        (c) => c && 'vsol' in c && !(('vtok' in c) && ('rsol' in c) && ('rtok' in c)),
      ).length;
      if (partial) {
        bad.push(`${partial} candles carry some of the reserve fields and not the others — all four or none`);
      }
      // Against this coin's own opening state, not a hardcoded 30 — 216 recorded
      // coins open at 4.292 and a fixed floor calls every one of them impossible.
      const impossible = candles.filter((c) => c?.vsol != null && c.vsol < floor * 0.999).length;
      if (impossible) {
        bad.push(`${impossible} candles close on a curve below the ${floor} SOL this coin opened at — no such state exists`);
      }
      const negative = candles.filter((c) => c?.rtok != null && c.rtok < 0).length;
      if (negative) bad.push(`${negative} candles close with negative real token reserves`);
      // The curve issued `curve.realTokens` and cannot hold more than it issued.
      const opened = Number(record.curve?.realTokens ?? 0);
      const over = opened > 0 ? candles.filter((c) => c?.rtok != null && c.rtok > opened * 1.001).length : 0;
      if (over) bad.push(`${over} candles hold more tokens in the curve than it ever issued`);
      // The whole reason the reserves are stored: the close above them is
      // *derived* from them, `vsol / vtok` and nothing else. If the two ever
      // disagree, one of them is not what it says it is — and since the price is
      // the reduction and the reserves are the state, a reader has no way to know
      // which. The recorder writes both off the same event, so this is exact by
      // construction and only ever fires on a row that has been rewritten.
      const adrift = candles.filter((c) => {
        if (!c || c.c == null || c.vsol == null || c.vtok == null || !(c.vtok > 0)) return false;
        const implied = c.vsol / c.vtok;
        return Math.abs(c.c - implied) > Math.abs(implied) * 1e-6;
      }).length;
      if (adrift) {
        bad.push(`${adrift} candles close at a price their own reserves do not imply — the price and the state behind it disagree`);
      }
    }
  }

  bad.push(...checkCurveAtEntry(record, outcome));
  bad.push(...checkFeeSol(record, outcome));

  const conserved = curveConservation(record);
  if (conserved && !conserved.sound) {
    bad.push(
      `the peak needs ${Math.round(conserved.impliedOut).toLocaleString('en-US')} tokens out of the curve ` +
        `but only ${Math.round(conserved.bought).toLocaleString('en-US')} were ever bought out of it — ` +
        'the printed high is a quote nobody could have sold into',
    );
  }
  const flow = solConservation(record);
  if (flow && !flow.sound) {
    bad.push(
      `the peak needs ${flow.impliedSol.toFixed(3)} SOL into the curve ` +
        `but only ${flow.grossIn.toFixed(3)} SOL was ever put in — ` +
        'the printed high is a quote no money in the coin could have produced',
    );
  }
  return bad;
}

/**
 * The state the entry price was struck on, held to the entry price.
 *
 * `outcome.entry` is a ratio and `outcome.curveAtEntry` is the absolute state
 * behind it — which is only true if the two agree. The price *is* `vsol / vtok`;
 * the recorder reads both off the same event, so a row where they disagree has
 * been rewritten by something and a reader cannot tell which half to believe.
 *
 * This is the counter rule applied to the field: `entry` is the number, the
 * reserves are the rows behind it. It arrived in the same night as the rule and
 * was the one field the rule was never pointed at.
 *
 * Silent on the recorded corpus, which has no `curveAtEntry` at all — the
 * presence half is asked only of rows that carry the candle reserves, so a
 * legacy row is not told twice that it predates a field.
 */
export function checkCurveAtEntry(record, outcome) {
  const bad = [];
  const has = 'curveAtEntry' in outcome;
  const state = outcome.curveAtEntry;
  const modern = Array.isArray(record.market?.candles) && record.market.candles.some((c) => c && 'vsol' in c);

  if (!has) {
    if (modern && outcome.entry != null) {
      bad.push('outcome has no curveAtEntry — the entry price is a ratio with no state behind it');
    }
    return bad;
  }
  if (state === null) {
    // Null is the right answer when no trade landed before the cutoff, which is
    // exactly when `entry` is null too. One without the other is a contradiction.
    if (outcome.entry != null) {
      bad.push('outcome.entry was struck but curveAtEntry is null — the state it was read off was not kept');
    }
    return bad;
  }
  if (!Array.isArray(state) || state.length !== 4) {
    bad.push('outcome.curveAtEntry is not [vsol, vtok, rsol, rtok]');
    return bad;
  }
  if (outcome.entry == null) {
    bad.push('outcome.curveAtEntry holds a curve state but no entry price was ever struck');
    return bad;
  }
  const [vsol, vtok] = state;
  if (!(Number(vsol) > 0) || !(Number(vtok) > 0)) {
    bad.push('outcome.curveAtEntry has no virtual reserves — nothing can be priced off it');
    return bad;
  }
  const implied = Number(vsol) / Number(vtok);
  if (Math.abs(outcome.entry - implied) > Math.abs(implied) * 1e-6) {
    bad.push(`outcome.entry is ${outcome.entry} but curveAtEntry implies ${implied} — the price and the state behind it disagree`);
  }
  return bad;
}

/**
 * The pump trading fee actually paid, held to the SOL it was charged on.
 *
 * Deliberately loose. A tight bound — the census rate applied to the volume —
 * would be arithmetically right and would still fire on the live anomaly the
 * decoder already knows about: a zero-SOL trade carrying a real fee, 17 of the
 * first 1,236 records. So the bound here is the one no fee rate can cross,
 * charged at 95 basis points or at any other rate: **the fee cannot exceed the
 * SOL it was charged on.**
 *
 * That is still the check worth having, because the failure this field exists to
 * prevent is a unit error. The last cost model in this project was wrong by a
 * factor of twenty and the one before it confused lamports with SOL; either
 * mistake puts `feeSol` a hundred to a thousand times over this line, and
 * nothing else about the row would look wrong.
 */
export function checkFeeSol(record, outcome) {
  const bad = [];
  const modern = outcome.feeBps != null;
  if (!('feeSol' in outcome)) {
    if (modern) {
      bad.push('outcome has no feeSol — what the trading fee actually cost is left to a remembered 1%');
    }
    return bad;
  }
  const fee = Number(outcome.feeSol);
  if (!Number.isFinite(fee) || fee < 0) {
    bad.push(`outcome.feeSol is ${outcome.feeSol} — a fee is a non-negative number of SOL`);
    return bad;
  }
  const traded = Number(record.total?.solIn ?? 0) + Number(record.total?.solOut ?? 0);
  if (traded > 0 && fee > traded) {
    bad.push(`outcome.feeSol is ${fee} SOL against ${traded} SOL traded — no fee rate can charge more than it was charged on`);
  }
  return bad;
}

/**
 * Curve conservation: does the printed peak need more tokens out of the curve
 * than anyone ever bought out of it?
 *
 * The bonding curve is a constant product on the virtual reserves, so a price
 * pins both of them exactly: `vtok = sqrt(k / price)`. Reaching the peak price
 * therefore *requires* the curve to have handed over `virtualTokens - sqrt(k /
 * peak)` tokens. `who[]` records tokens in and out per wallet from a completely
 * different code path, and the most that can ever have left the curve is the sum
 * of everything bought out of it. When the first number exceeds the second the
 * peak on that row is arithmetically impossible.
 *
 * Deliberately generous, exactly as W32 built it: it credits *every* buy in the
 * whole window against a peak that may have happened in the first second, so it
 * only ever under-reports.
 *
 * **Measured by `capture check data/coins-*.jsonl` over all 7 recorded coin
 * files — 6,075 coins carrying a launch curve, a peak and an uncapped `who[]`:**
 *
 * | peak | coins | impossible |
 * |---|---|---|
 * | 1–1.5x | 5,423 | 5.0% |
 * | 1.5–2x | 331 | 24.2% |
 * | 2–3x | 204 | 24.5% |
 * | 3–5x | 81 | 51.9% |
 * | 5–10x | 28 | 57.1% |
 * | **above 10x** | **8** | **75.0%** |
 *
 * Overall 465 of 6,075, 7.7%, against the 7.3% W32 reports for the same test.
 * The gradient is the finding and it is steep and monotonic: the bigger the
 * printed peak, the likelier it is that no money backs it. W32's per-bucket
 * figures (100% above 10x, 88.9% at 5–10x) come out lower here on a slightly
 * larger population — 6 of the 8 coins above 10x rather than 6 of 6 — so **the
 * shape reproduces and the exact percentages do not.** Reported as measured
 * rather than as quoted, and re-derivable by running the command above.
 *
 * ## Why this one grades and `tokenBalance` does not
 *
 * The house rule is that a check firing on a meaningful share of ordinary coins
 * *for a reason nobody can explain* is a check people learn to ignore, and this
 * one fires on 5.0% of coins under 1.5x. That looked like a contradiction. It is
 * not, and the thing that settles it is the shape of the miss, not its rate.
 *
 * Take `impliedOut / bought` over all 6,072 coins the rule can grade and bin it
 * finely around the threshold:
 *
 * | ratio | coins |
 * |---|---|
 * | 0.999 – 1.0 | 443 |
 * | 1.0 – 1.0005 | **1,298** |
 * | 1.0005 – 1.002 | **0** |
 * | 1.002 – 1.005 | 0 |
 * | 1.005 – 1.05 | 10 |
 * | 1.05 – 1.5 | 55 |
 * | above 1.5 | 400 |
 *
 * **There is a hole at the threshold.** 1,298 coins — a fifth of the corpus —
 * land within 0.05% of exactly 1, which is two entirely separate code paths (a
 * price read off the trade event, a token flow summed over `who[]`) agreeing to
 * five figures. Then nothing at all until 1.005. A check cutting a smooth
 * continuum would be densest just past its threshold; this one is emptiest
 * there. The median failing coin needs **4.3x** more tokens than were ever
 * bought, and loosening the tolerance from 0.1% all the way to 50% only moves
 * the base rate from 5.0% to 4.3%. There is nothing marginal to forgive.
 *
 * Two more facts rule out the alternative — that the recorder simply missed
 * buys, which would make `bought` a floor and the coin innocent:
 *
 *   • Dropped messages are random, so they would pile up just above 1. That
 *     band is empty.
 *   • Failures concentrate by creator. Of 250 creators with 5 or more graded
 *     coins, **198 fail none of their 2,383 coins** while 18 fail more than half
 *     of theirs and account for 195 of the 465 failures. Capture loss does not
 *     pick favourites.
 *   • Failing coins carry *more* trades (median 19 against 9) on *less* money
 *     (1.07 SOL against 3.94). Missing data would show up as fewer trades, not
 *     more.
 *
 * So the 5% is not a false-alarm rate. It is the rate at which an ordinary
 * pump.fun coin prints a high that the money in it cannot pay for. The rule
 * about ignorable checks is satisfied rather than overruled: the reason is now
 * explained, and it is written on the screen where the number is. **This check
 * grades.** `tokenBalance` does not, and the difference is exactly the shape
 * test above — its excess is a smooth tail through 1.0 with no hole in it.
 *
 * Returns null when the row cannot answer: no launch curve, no peak, no `who[]`,
 * or the 200-wallet cap bit — past the cap `bought` is a floor and a floor would
 * make this fire on coins that are fine.
 */
export function curveConservation(record) {
  const open = record?.curve;
  const peak = Number(record?.outcome?.peak);
  const who = record?.who;
  if (!open || !(Number(open.virtualSol) > 0) || !(Number(open.virtualTokens) > 0)) return null;
  if (!(peak > 0) || !Array.isArray(who) || !who.length) return null;
  if (record.whoCapped === true || who.length >= 200) return null;

  const k = Number(open.virtualSol) * Number(open.virtualTokens);
  const impliedOut = Number(open.virtualTokens) - Math.sqrt(k / peak);
  let bought = 0;
  for (const w of who) bought += Number(w?.tin ?? 0);
  // A tenth of a percent, plus one whole token, for the two-decimal rounding
  // `who[]` is written at summed over as many as 200 rows.
  return { impliedOut, bought, sound: impliedOut <= bought * 1.001 + 1 };
}

/**
 * The pump trading fee, as a fraction, used as the slack on the SOL-flow test.
 *
 * `total.solIn` is what buyers *paid*, and about 1% of that is the trading fee,
 * which never reaches the curve. So the ceiling is a touch generous already and
 * the tolerance only has to cover that one point. Measured: at 0.1% the test
 * fails 597 of 6,055 coins and at 1% it fails 557, and the 40 coins between the
 * two sit exactly where a fee-sized overshoot would put them. Everything past
 * 1% is over by 10% or more.
 */
export const FEE_SLACK = 0.01;

/**
 * The same conservation question asked of SOL instead of tokens — and this is
 * the form an independent check settled on as the sound one.
 *
 * To print price `p` the curve's virtual SOL has to have reached `sqrt(k·p)`, so
 * reaching the peak *required* `sqrt(k·peak) - virtualSol` SOL to have entered
 * the curve. The most that can ever have entered is everything buyers put in,
 * which is `total.solIn`. When the first number exceeds the second, the peak on
 * that row is a price no money in the coin could have produced.
 *
 * **Gross inflow, never net.** The peak is transient: SOL that came in and left
 * again still paid for it while it was there. On net inflow the same test fires
 * on 73% of everything and means nothing.
 *
 * Three properties make this the better of the two, all measured over the 7
 * recorded coin files:
 *
 *   1. **It finds nearly everything the token form does, and much more.** Of
 *      the 5,974 coins both can grade, 462 fail both, **3** fail only the token
 *      form and **95** fail only this one.
 *   2. **It does not need `who[]`,** so the 200-wallet cap does not blind it:
 *      `total.solIn` is summed over every buy on the coin whether or not the
 *      wallet made it into the ledger. That is 81 coins the token form has to
 *      refuse and this one can grade — none of which turn out to fail, which is
 *      worth knowing rather than assuming.
 *   3. **It fires more, and at the same gradient.** 557 of 6,055 coins, 9.2%,
 *      against 7.7%, and the two agree at 75% above 10x.
 *
 * **The reason it was previously left out does not hold, and this is the
 * measurement that settles it.** It was said to need an era-units rule — stored
 * price is lamports per base unit on 2026-08-10 through 08-12 and whole units
 * from 08-15 — which no future row will need. But the rule is already enforced
 * by the precondition: this test refuses any row without its own launch curve,
 * and **not one of the 3,324 rows in the four pre-08-16 files carries a `curve`
 * block at all.** The curve arrived after the units did. So no old-era row can
 * reach the arithmetic, every future row carries a curve by construction, and
 * the era rule has nothing left to do here.
 *
 * Returns null when the row cannot answer: no launch curve, no peak, or no
 * gross inflow to compare against.
 */
export function solConservation(record) {
  const open = record?.curve;
  const peak = Number(record?.outcome?.peak);
  if (!open || !(Number(open.virtualSol) > 0) || !(Number(open.virtualTokens) > 0)) return null;
  if (!(peak > 0)) return null;
  const grossIn = Number(record?.total?.solIn);
  if (!Number.isFinite(grossIn) || !(grossIn > 0)) return null;
  const vsol = Number(open.virtualSol);
  const k = vsol * Number(open.virtualTokens);
  const impliedSol = Math.sqrt(k * peak) - vsol;
  return { impliedSol, grossIn, sound: impliedSol <= grossIn * (1 + FEE_SLACK) };
}

/**
 * The fields nothing used to hold to anything.
 *
 * Eight were named as unheld: `seq`, `si`, `who[].slotsAfter`, and the presence
 * of the five `*Capped` flags, where absence and `false` read the same. Five of
 * the eight turn out to have an invariant after all, and they are here:
 *
 *   • `who[].slotsAfter` is **exactly** `w.slot - record.slot`, and both of
 *     those are on the same row. It was called uncheckable; it is the most
 *     checkable field of the eight. It also cannot be negative — a wallet
 *     cannot buy a coin in a block before the block that created it.
 *   • `seq` is a record's index within its session: a whole number from zero,
 *     and it has to advance (`checkFiles` holds it to that across rows).
 *   • `si` is a position among the pump transactions in a slot: a whole number
 *     from zero, and two launches cannot occupy the same one (again across
 *     rows).
 *   • The five cap flags now have their invariant supplied by the schema
 *     itself. From v3 the recorder writes all five on every row, so a row that
 *     is missing one is a defect rather than a row saying `false` the short
 *     way. **This is what bumping the version bought**: the presence rule could
 *     not be stated at all while every shape shared one number.
 *
 * The three that remain genuinely unheld are named in `capture check`'s output
 * rather than left to the source: `si` and `seq` beyond their range and
 * ordering, and `connectedForSec`.
 */
export function checkUnheld(record) {
  const bad = [];
  // Only of rows that say they are v3 or later. A legacy row is not told it is
  // missing a field that did not exist when it was written.
  const modern = Number(record.v) >= 3;

  if ('seq' in record && !(Number.isInteger(record.seq) && record.seq >= 0)) {
    bad.push(`seq is ${record.seq} — a record's index within its session is a whole number from zero`);
  } else if (modern && !('seq' in record)) {
    bad.push('no seq — the row cannot say where in its session it was written');
  }
  if (record.si != null && !(Number.isInteger(record.si) && record.si >= 0)) {
    bad.push(`si is ${record.si} — a position among the pump transactions in a slot is a whole number from zero`);
  }

  const who = record.who;
  if (Array.isArray(who) && record.slot != null) {
    let adrift = 0;
    let before = 0;
    for (const w of who) {
      if (!w || w.slotsAfter == null) continue;
      if (w.slot != null && w.slotsAfter !== w.slot - record.slot) adrift++;
      if (w.slotsAfter < 0) before++;
    }
    if (adrift) {
      bad.push(`${adrift} wallets whose slotsAfter is not their own slot minus the launch slot — the landing distance and the slots behind it disagree`);
    }
    if (before) {
      bad.push(`${before} wallets that landed before the block that created the coin they bought`);
    }
  }

  if (modern) {
    const absent = [];
    if (typeof record.whoCapped !== 'boolean') absent.push('whoCapped');
    for (const flag of ['highsCapped', 'lowsCapped', 'sellsCapped', 'zeroFeeCapped']) {
      if (typeof record.outcome?.[flag] !== 'boolean') absent.push(`outcome.${flag}`);
    }
    if (absent.length) {
      bad.push(`${absent.join(', ')} absent or not a boolean — from v3 every cap flag is written, so absence must not read as false`);
    }
  }
  return bad;
}

/**
 * Tokens bought out of the curve against tokens sold back into it, from `who[]`.
 *
 * A second, blunter reading of the same file: nobody can sell a token they never
 * bought, so `tout` should never exceed `tin` inside one coin's window.
 *
 * **Measured, it does, on 5.8% of coins, and the excess has no structure**: a
 * smooth tail from 1.0 to 3.7 with no separation, flat across every peak bucket
 * (5.4% of coins under 1.5x, 0% of the eight above 10x). That is the profile of
 * a systematic accounting difference between the buy and sell legs, not of an
 * anomaly, so it is **reported and counted, never used to fail a row.** A check
 * that fires on 6% of ordinary coins for a reason nobody can explain is a check
 * that gets ignored, and that is how the last set of defects survived.
 *
 * `curveConservation` above is the version of this question that does separate
 * the good coins from the bad ones, and it is the one that **fails a row**.
 *
 * **Settled by a third measurement, and neither of the two here is the best
 * detector.** Asked of every 2026 coin file, three tests of the same idea:
 *
 * | test | base rate | 5–10x | above 10x | rises with the peak? |
 * |---|---|---|---|---|
 * | this one, `tout > tin` | 5.8% | 5.7% | **0.0%** | no |
 * | `curveConservation`, tokens | 7.3% | 45.7% | 75.0% | yes |
 * | SOL flow on **gross** inflow | **15.1%** | **61.5%** | **88.2%** | yes |
 *
 * The third asks whether the peak needs more SOL than ever entered the coin, and
 * the ceiling has to be gross `total.solIn` and not net — the peak is transient,
 * so money that came in and left again still paid for it. On net inflow it fires
 * on 73% of everything and means nothing.
 *
 * **It is `solConservation` above, and it now grades.** It was left out on the
 * grounds that it needed an era-units rule (stored price is in lamports per base
 * unit on 08-10 through 08-12 and in whole units from 08-15) that no future row
 * would need. That reason was checked and does not hold: the test refuses any
 * row that does not carry its own launch `curve`, and none of the 3,324 rows in
 * the four pre-08-16 files carries one, so no old-era row ever reaches the
 * arithmetic. The units rule was already being enforced by a precondition that
 * was there for another purpose.
 *
 * Returns null when the row cannot answer: no `who`, or the 200-wallet cap bit
 * and every sum over it is a floor.
 */
export function tokenBalance(record) {
  const who = record?.who;
  if (!Array.isArray(who) || !who.length) return null;
  if (record.whoCapped === true || who.length >= 200) return null;
  let tin = 0;
  let tout = 0;
  for (const w of who) {
    tin += Number(w?.tin ?? 0);
    tout += Number(w?.tout ?? 0);
  }
  if (!(tin > 0)) return null;
  return { tin, tout, ratio: tout / tin, sound: tout <= tin * 1.001 + 1 };
}

/**
 * The four facts a finished coin record has to be able to state about itself,
 * and the two ways it can contradict them.
 *
 * This is defect 1 written as a test. `outcome.follow` was the configured 60 on
 * all 8,881 recorded rows, so a coin the listener was still watching when it
 * stopped is written identically to one that ran the full minute. Those coins
 * have a median last candle at second 3 against 26 for the rest, and every
 * expectancy number computed off the corpus silently averaged the two together.
 */
export function checkOutcome(record, outcome) {
  const bad = [];
  const has = (k) => k in outcome && outcome[k] != null;

  if (!has('observedSec')) {
    bad.push('outcome has no observedSec — the row cannot say how long it was really watched');
  }
  if (!('complete' in outcome)) {
    bad.push('outcome has no complete flag — a truncated observation reads as a whole one');
  }
  if (!has('gapSec')) {
    bad.push('outcome has no gapSec — the follow timer fires whether or not the feed was alive');
  }
  if (outcome.complete === false && !has('stopReason')) {
    bad.push('an incomplete outcome with no stopReason — it says it was cut off but not by what');
  }
  if (outcome.complete === true && outcome.gapSec > 0) {
    bad.push(`complete is true but gapSec is ${outcome.gapSec} — the feed was down inside this window`);
  }
  // `observedSec` is whole seconds — floored, because a timer never fires early,
  // so a window that ran its course reads exactly `follow` and one cut short
  // reads strictly less. The window itself is compared floored too, so a
  // sub-second `--follow` (which only a test ever uses) does not make every row
  // look short.
  const window = has('follow') ? Math.floor(outcome.follow) : null;
  if (outcome.complete === true && has('observedSec') && window !== null && outcome.observedSec < window) {
    bad.push(`complete is true but observedSec ${outcome.observedSec} is under the ${outcome.follow}s window`);
  }
  if (outcome.complete === false && outcome.gapSec === 0 && has('observedSec') && window !== null
      && outcome.observedSec >= Math.max(1, window)) {
    bad.push(`incomplete with no gap, yet observedSec ${outcome.observedSec} covers the whole ${outcome.follow}s window`);
  }

  // The turning-point lists. The old cap was 60 entries and it froze the
  // running extreme with it, so a coin that kept setting new highs was recorded
  // as having stopped — and it bit the winners, because they are the coins that
  // run out of room.
  for (const side of ['highs', 'lows']) {
    const list = outcome[side];
    const flag = `${side}Capped`;
    if (outcome[flag] === true) {
      bad.push(`outcome.${side} ran out of room — the extremes on this row are a floor, not the truth`);
    } else if (Array.isArray(list) && list.length >= 60 && !(flag in outcome)) {
      bad.push(`outcome.${side} is at the legacy cap with no ${flag} saying so, so the running extreme froze with it`);
    }
  }

  bad.push(...checkSells(record, outcome));
  bad.push(...checkCurve(record, outcome));
  bad.push(...checkUnheld(record));

  if (record.sid == null) {
    bad.push('no sid — the row cannot say which run recorded it, so calendar days stand in for sessions');
  }
  if (record.slot == null || record.sig == null) {
    bad.push('no slot/sig — what this transaction cost to land can never be looked up');
  }
  return bad;
}

/**
 * Run both checks over a set of JSONL files, streaming.
 *
 * Streaming rather than reading whole: the largest recorded day is 23 MB and the
 * machine this runs on has 8 GB, so the file is read a line at a time and only
 * the census is held.
 */
export async function checkFiles(files, { onBadRow = null } = {}) {
  const kindOf = (text) => text.replace(/\b\d+(\.\d+)?\b/g, 'N');
  const c = census();
  let rows = 0;
  let lineCount = 0;
  let badRows = 0;
  // At most one example per distinct set of complaints. Five near-identical
  // paragraphs saying the same six things is not five examples, it is one.
  const examples = [];
  const exampleKinds = new Set();
  const keepExample = (file, lineNo, bad) => {
    const key = bad.map(kindOf).sort().join('|');
    if (exampleKinds.has(key) || examples.length >= 5) return;
    exampleKinds.add(key);
    examples.push({ file, lineNo, bad });
  };
  // Rows about the run rather than about a coin — `start`, `tick`, `gap`,
  // `failagg`, `stop`, and the `fail` rows in their own file. They carry a `k`
  // and coin rows do not, so one test separates them. They are kept out of the
  // census on purpose: their fields are constant by design and mixing them in
  // would drown the one signal C7 exists to give.
  const meta = [];
  // Which sessions each file holds, and which files each session is spread
  // across. One file with two sessions, or one session across two files, is the
  // shape that produced a holdout day that was really the tail of the run
  // before it.
  const sidsByFile = new Map();
  const filesBySid = new Map();
  let rowsWithoutSid = 0;
  let failRows = 0;
  let failRowsWithoutRate = 0;
  // Coin rows tallied per session, so the session footer can be held to them.
  const coinsBySid = new Map();
  const mints = new Map(); // mint -> first file:line, for duplicate detection
  let outOfOrder = 0;
  // Counted, not failed on. See `tokenBalance`.
  let soldMoreThanBought = 0;
  let balanceChecked = 0;
  // Curve conservation by how big the peak was. The per-row complaint says
  // which coins; this says the thing that matters about them, which is that the
  // rate climbs steeply with the size of the printed peak. A flat count would
  // read as background noise and get ignored.
  const byPeak = PEAK_BUCKETS.map((b) => ({ ...b, coins: 0, impossible: 0 }));
  // The same, for the SOL-flow form. Kept side by side rather than merged: two
  // independent routes to one answer are worth more than one number, and the
  // pair is how the token form was shown to be the weaker of them.
  const byPeakSol = PEAK_BUCKETS.map((b) => ({ ...b, coins: 0, impossible: 0 }));
  // Which shapes the files claim to be. A version this build does not know
  // means every complaint below was produced by rules for a different file, so
  // it is reported first and it fails the run.
  const schemasSeen = new Map(); // v (or null for legacy) -> rows
  const schemasByFile = new Map(); // file -> Set of v
  // `seq` is a record's index within its session and `si` its position among
  // the pump transactions in a slot. Both were named as fields nothing held to
  // anything; both have an invariant that only shows up across rows.
  const seqBySid = new Map(); // sid -> highest seq seen
  const slotPositions = new Set(); // `sid|slot|si`, which no two launches share
  let seqOutOfOrder = 0;
  let duplicateSlotPosition = 0;
  // How many rows each kind of complaint applies to. A list of 3,000 lines
  // saying the same thing is not a finding; "929 rows say the price never beat
  // entry and also name the second it peaked" is.
  const byComplaint = new Map();

  for (const file of files) {
    const lines = readline.createInterface({
      input: fs.createReadStream(file),
      crlfDelay: Infinity,
    });
    let lineNo = 0;
    let lastT = null;
    for await (const line of lines) {
      lineNo++;
      if (!line.trim()) continue;
      lineCount++;
      let record;
      try {
        record = JSON.parse(line);
      } catch {
        badRows++;
        byComplaint.set('not valid JSON', (byComplaint.get('not valid JSON') ?? 0) + 1);
        keepExample(file, lineNo, ['not valid JSON']);
        continue;
      }

      // Asked of every row, session rows included: they carry the version too
      // and a file that holds two shapes is a file nothing can be said about.
      if (record && typeof record === 'object') {
        const v = record.v ?? null;
        schemasSeen.set(v, (schemasSeen.get(v) ?? 0) + 1);
        if (!schemasByFile.has(file)) schemasByFile.set(file, new Set());
        schemasByFile.get(file).add(v);
      }

      if (record && typeof record.sid === 'string') {
        if (!sidsByFile.has(file)) sidsByFile.set(file, new Set());
        sidsByFile.get(file).add(record.sid);
        // Keyed by kind of file, not by file. One session legitimately writes a
        // coins file, a tracks file, a tweets file and a fails file; what is
        // never legitimate is one session's coins landing in two coins files,
        // which is exactly what the UTC-midnight split used to do.
        const key = `${kindOfFile(file)}|${record.sid}`;
        if (!filesBySid.has(key)) filesBySid.set(key, new Set());
        filesBySid.get(key).add(file);
      }

      // A row about the run. Counted and kept for the session report, never
      // censused and never row-checked as if it were a coin.
      if (record && typeof record.k === 'string') {
        meta.push(record);
        if (record.k === 'fail') {
          failRows++;
          // A sample whose rate is not written down is not a sample, it is a
          // hole — the same defect as a hardcoded window.
          if (record.rate == null) failRowsWithoutRate++;
        }
        continue;
      }

      rows++;
      if (record?.sid == null) rowsWithoutSid++;
      if (record?.outcome && typeof record.outcome === 'object') {
        const tally = coinsBySid.get(record.sid ?? null) ?? { rows: 0, truncated: 0 };
        tally.rows++;
        if (record.outcome.complete === false) tally.truncated++;
        coinsBySid.set(record.sid ?? null, tally);
      }
      // Out-of-order writes make every "what did the feed do between these two
      // launches" question unanswerable.
      if (typeof record?.t === 'number') {
        if (lastT !== null && record.t < lastT) outOfOrder++;
        lastT = record.t;
      }
      if (typeof record?.mint === 'string') {
        const where = `${file}:${lineNo}`;
        if (mints.has(record.mint)) {
          badRows++;
          const complaint = `duplicate mint — already recorded at ${mints.get(record.mint)}`;
          byComplaint.set('duplicate mint', (byComplaint.get('duplicate mint') ?? 0) + 1);
          keepExample(file, lineNo, [complaint]);
          c.add(record);
          continue;
        }
        mints.set(record.mint, where);
      }
      const balance = tokenBalance(record);
      if (balance) {
        balanceChecked++;
        if (!balance.sound) soldMoreThanBought++;
      }
      const mult = Number(record?.outcome?.peakMult
        ?? (record?.outcome?.entry ? record.outcome.peak / record.outcome.entry : 1));
      const conserved = curveConservation(record);
      if (conserved) {
        const b = byPeak.find((x) => mult >= x.from && mult < x.to);
        if (b) {
          b.coins++;
          if (!conserved.sound) b.impossible++;
        }
      }
      const flow = solConservation(record);
      if (flow) {
        const b = byPeakSol.find((x) => mult >= x.from && mult < x.to);
        if (b) {
          b.coins++;
          if (!flow.sound) b.impossible++;
        }
      }
      // `seq` has to advance within a session, and no two launches can sit at
      // the same position in the same slot. Neither can be seen from one row.
      if (Number.isInteger(record?.seq) && record?.sid != null) {
        const last = seqBySid.get(record.sid);
        if (last !== undefined && record.seq <= last) seqOutOfOrder++;
        else seqBySid.set(record.sid, record.seq);
      }
      if (record?.slot != null && record?.si != null) {
        const at = `${record.sid ?? ''}|${record.slot}|${record.si}`;
        if (slotPositions.has(at)) duplicateSlotPosition++;
        else slotPositions.add(at);
      }
      c.add(record);
      const bad = checkRow(record);
      if (bad.length) {
        badRows++;
        onBadRow?.(file, lineNo, bad);
        for (const complaint of bad) {
          const kind = kindOf(complaint);
          byComplaint.set(kind, (byComplaint.get(kind) ?? 0) + 1);
        }
        keepExample(file, lineNo, bad);
      }
    }
  }

  // C3: a session is one file and a file is one session. Anything else and the
  // calendar is standing in for the run again.
  const split = [...filesBySid]
    .filter(([, seenIn]) => seenIn.size > 1)
    .map(([key]) => key.slice(key.indexOf('|') + 1));
  const mixed = [...sidsByFile].filter(([, sids]) => sids.size > 1).map(([f]) => f);

  return {
    ...c.report(rows),
    unbacked: unbackedCounters(meta, coinsBySid),
    // Rows the census could read, and lines the file actually held — they are
    // not the same number when a line does not parse, and a ratio taken over
    // the wrong one reads as "5,393 of 5,391".
    lines: lineCount,
    badRows,
    examples,
    complaints: [...byComplaint].sort((a, b) => b[1] - a[1]).map(([kind, n]) => ({ kind, rows: n })),
    // What the run said about itself. Empty means the capture predates session
    // records, in which case uptime is not merely bad — it is unmeasurable, and
    // saying so is the whole point.
    sessions: sessions(meta),
    metaRows: meta.length,
    rowsWithoutSid,
    sessionsSplitAcrossFiles: split,
    filesWithSeveralSessions: mixed,
    outOfOrder,
    soldMoreThanBought,
    balanceChecked,
    conservationByPeak: byPeak,
    solConservationByPeak: byPeakSol,
    failRows,
    failRowsWithoutRate,
    // What shapes were met, in the order 1, 2, 3 …, with `null` standing for
    // the recorded corpus, which carries no version at all.
    schemas: [...schemasSeen]
      .map(([v, rowsAt]) => ({ v, rows: rowsAt, status: schemaStatus(v) }))
      .sort((a, b) => (a.v ?? 0) - (b.v ?? 0)),
    // A file is one shape. Two versions in one file and nothing can be said
    // about either half — the same defect as one session across two files.
    filesWithSeveralSchemas: [...schemasByFile].filter(([, vs]) => vs.size > 1).map(([f]) => f),
    seqOutOfOrder,
    duplicateSlotPosition,
  };
}

/**
 * Counters in a session footer that the rows in the file do not account for.
 *
 * This is W21's C21 in its general form, and it is the lesson underneath every
 * defect this recorder has ever had. `stats.failed` counted 645,741 failed
 * transactions and kept none of them, so a possibly verdict-changing fact sat
 * behind a number nobody could check for weeks. `funding.depth` and
 * `outcome.follow` hid theirs the same way, by being constants nobody looked
 * at. The rule for every number a recorder writes: **can I get back to the
 * underlying rows from this?** If not, it is decoration.
 *
 * Each entry says what the footer claimed, what the rows add up to, and — when
 * a counter has no rows behind it at all — that no evidence exists either way.
 *
 * @param meta      the run's own rows
 * @param coinsBySid  per-session tallies taken from the coin rows themselves
 */
export function unbackedCounters(meta, coinsBySid = new Map()) {
  const out = [];
  const stops = meta.filter((r) => r.k === 'stop');
  for (const stop of stops) {
    const sid = stop.sid ?? null;
    const mine = meta.filter((r) => (r.sid ?? null) === sid);
    const coins = coinsBySid.get(sid) ?? { rows: 0, truncated: 0 };
    const agg = mine.filter((r) => r.k === 'failagg');
    const sum = (rows, field) => rows.reduce((a, r) => a + (r[field] || 0), 0);

    const backed = [
      ['launches', stop.launches, coins.rows, 'coin rows'],
      ['written', stop.written, coins.rows, 'coin rows'],
      ['truncated', stop.truncated, coins.truncated, 'coin rows with complete:false'],
      ['beats', stop.beats, mine.filter((r) => r.k === 'tick').length, 'tick rows'],
      ['connectedBeats', stop.connectedBeats, mine.filter((r) => r.k === 'tick' && r.connected).length, 'connected tick rows'],
      ['gaps', stop.gaps, mine.filter((r) => r.k === 'gap').length, 'gap rows'],
      ['gapMs', stop.gapMs, sum(mine.filter((r) => r.k === 'gap'), 'ms'), 'gap rows'],
      ['failed', stop.failed, sum(agg, 'n'), 'failagg rows'],
      ['failLogged', stop.failLogged, sum(agg, 'kept'), 'failagg rows'],
    ];
    for (const [counter, said, found, from] of backed) {
      if (said == null) continue;
      if (said !== found) out.push({ sid, counter, said, found, from });
    }
    // Named rather than silently tolerated. This producer writes one row per
    // coin, not one per trade, so the run's trade total genuinely cannot be
    // rebuilt from the file — and a reader deserves to be told that rather
    // than to assume it can.
    if (stop.trades != null) {
      out.push({ sid, counter: 'trades', said: stop.trades, found: null, from: 'this recorder writes one row per coin, not one per trade' });
    }
  }
  return out;
}

/**
 * Which family a file belongs to — `coins`, `tracks`, `tweets`, `fails`.
 *
 * The session infix follows the name, so the first segment is the kind.
 */
export function kindOfFile(file) {
  return path.basename(file).split('-')[0];
}
