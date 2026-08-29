// What is this coin worth buying, and is it built like a rug.
//
// Two separate questions, deliberately kept apart. The old score mixed them into
// one confidence number, which is how a coin with 83 SOL from two wallets and no
// social presence scored 15 out of 100 and was thrown away.
//
// The chance figure is not invented. It comes from the measured split: with a
// known-good wallet in within three seconds, 38.4% of coins ran; without one,
// 8.3%; the baseline was 16.4%. A specific wallet's own record moves the number
// from there, shrunk towards the group by how many coins that record rests on,
// so one lucky wallet cannot claim certainty.

import { MIN_COINS } from './wallets.js';
import { analyzeLaunch } from './cluster.js';
import { PUMP_LAUNCH, buyTokens } from './pump.js';
import { entryTiming } from './entry.js';

/** Measured on the local corpus for 11-12 Aug against a Dune-built registry. */
export const RUN_WITH_KNOWN = 0.384;
export const RUN_WITHOUT = 0.083;
export const RUN_BASE = 0.164;

/**
 * The score predicts a spike. It does not predict a profit, and the difference
 * is not academic.
 *
 * Replaying 35 target/stop combinations over 3,556 coins with a known wallet:
 * every one loses after the 3.1% round trip, and every one does WORSE than the
 * coins without a known wallet. Best case -2.61% against -1.99%.
 *
 * The reason is in the timing. The median peak lands at 3 seconds — the entry
 * moment itself — and is given straight back: coins with a known early wallet
 * average a 1.42x peak and a 0.95x close. They are more volatile, not more
 * profitable, so a stop is hit more often and a target rarely survives.
 *
 * This is Log.md's original verdict arriving by a new route. The signal is real
 * and it is not a trade.
 */
export const TRADEABLE = false;
export const TRADEABLE_NOTE =
  'no exit rule tested pays after costs — this ranks attention, not trades';

/** How many coins a wallet's own rate must outweigh before it is trusted. */
const SHRINK = 20;

// ---------------------------------------------------------------------------
// The three refusals
// ---------------------------------------------------------------------------
//
// Everything above this line ranks. Everything below it refuses, and a refusal
// is absolute: the score goes to zero and no amount of good news anywhere else
// brings it back. That asymmetry is deliberate. The ranking questions are
// statistical and a wrong answer costs a percent; these three are structural and
// a wrong answer costs the position.
//
// All three are also allowed to say "I could not tell". A check that treats
// missing data as a failure quietly throws away most of the corpus, and a check
// that treats it as a pass is not a check.

/**
 * The share of the supply that puts a launch out of reach, in percent.
 *
 * On pump.fun's opening curve this is not a large buy. Twenty-five percent of
 * the supply costs the deployer about 10.7 SOL, because the first SOL into a
 * curve buys 3.5% of everything on its own — which is the whole reason this is
 * measured in supply rather than in SOL. A dev holding this much can end the
 * coin in one transaction.
 *
 * It was 15 until the corpus could grade it. The rule had been reading a
 * sixty-second position against a three-second cutoff and reporting most
 * deployers as holding nothing, so until that was fixed there was no honest
 * measurement to set the number from. With it fixed, banding 3,956 launches by
 * what the deployer took says plainly that 15 was in the wrong place:
 *
 *   deployer allocation   launches   rugged (<0.5x)
 *   ------------------------------------------------
 *   the whole corpus         3,956            13.5%
 *   over 15%                   491             9.8%
 *   15% to 25%                 355             7.0%   ← cleanest, and was refused
 *   over 25%                   136            16.9%
 *
 * The band between 15 and 25 rugs at half the base rate. It is the momentum
 * cohort — a deployer with real skin in its own launch — and the old ceiling
 * threw all 355 of them away to catch a group that was cleaner than average.
 * Above 25 the rate finally turns and runs at a quarter again the baseline.
 */
export const MAX_CREATOR_SUPPLY_PCT = 25;

/**
 * How much of the opening's buy depth one seller has to account for before the
 * setup is dead. Not a measured optimum — it is the point at which a single exit
 * is larger than most of what came in, and there is no way to be on the right
 * side of that trade.
 */
export const SELL_DOMINANCE = 0.6;

/**
 * How much of the opening money can sit inside coordinated wallets before the
 * buyer count is fiction rather than a crowd.
 */
export const MAX_CLUSTER_SHARE = 0.7;

/**
 * How much of the supply the wallets that landed with the deployer can hold
 * before the launch is refused for it.
 *
 * This is the rule that catches $Laura, and it is the only one that does. Its
 * deployer took 15.17% on its own, which the ceiling above now allows on
 * purpose; the thirteen wallets that landed with it inside the same hundredth of
 * a second held 45.28% between them, and the coin ended at 0.35x.
 *
 * Thirty-five is where the corpus puts the line. Banding 1,547 launches that
 * have a deployer inside a launch block:
 *
 *   launch block holds   launches   rugged (<0.5x)
 *   -----------------------------------------------
 *   the whole corpus        3,956            13.5%
 *   25% to 35%                308            15.6%
 *   over 35%                  161            28.6%   ← refused
 *   35% or under            1,078             6.0%   ← kept
 *
 * Over 35% the rug rate is more than double the base and what is left behind is
 * less than half of it, which is the separation a refusal needs. Below it the
 * figure means the opposite of what it looks like: a launch block is where every
 * sniper in the world is trying to be, so a big one is usually a crowded launch
 * rather than one operator, and refusing on it at 15% would have thrown out
 * 1,007 launches whose cleanest band was the one nearest the threshold.
 */
export const MAX_LAUNCH_BLOCK_PCT = 35;

/**
 * How much of the opening money can sit inside coordinated wallets before the
 * launch is refused outright, as opposed to merely flagged by `MAX_CLUSTER_SHARE`.
 *
 * Set on request, and the corpus does not support it — this is recorded here
 * rather than argued in a commit message, because the number is one edit away
 * from being turned off and whoever makes that edit should see the measurement.
 *
 *   coordinated share of the opening   launches   rugged (<0.5x)
 *   -------------------------------------------------------------
 *   the whole corpus                      3,956            13.5%
 *   over 30% (3+ clustered wallets)          219             9.1%
 *   over 50%                                 105             6.7%
 *   over 70%                                  50             4.0%
 *
 * The rate falls as the coordination rises, and every band is cleaner than the
 * corpus it is drawn from. On this evidence refusing above 30% costs 219
 * launches that rugged a third less often than average and buys nothing.
 *
 * It also does not catch $Laura, which is what it was added for: $Laura's
 * coordinated wallets are the five dust accounts holding 3.1% of the money, not
 * the four big ones, because the big four took four different sizes and the
 * analyser only joins wallets it has a reason to join. The 45.28% figure is the
 * launch block above, not this.
 *
 * Setting this to 1 (or above) disables it without removing the rule.
 */
export const MAX_CLUSTER_SHARE_REJECT = 0.3;

/** The opening window, in slots. Three seconds is about seven of them. */
export const SLOT_SEC = 0.4;

/** Analysing one coin twice per request is waste; the record is the key. */
const reports = new WeakMap();

/** The cluster read on a coin, computed once and remembered. */
export function clusterReport(coin, cutoffSec = 3) {
  if (!coin || typeof coin !== 'object') return null;
  const hit = reports.get(coin);
  if (hit && hit.cutoff === cutoffSec) return hit.report;
  let report = null;
  try {
    // The funding edges the watcher looked up, when it got any. Handed over only
    // if there is at least one: `analyzeLaunch` treats the presence of a
    // transfer list as "the caller has this data", so an empty array would make
    // it report a funding read it did not do. A record from before the watcher
    // looked funders up has no such field and is analysed exactly as it was.
    const transfers = coin.funding?.transfers;
    report = analyzeLaunch(coin, {
      windowSec: cutoffSec,
      ...(transfers?.length ? { transfers } : {}),
    });
  } catch {
    // A record the analyser cannot read is not a reason to stop scoring the
    // rest. It is also not evidence of anything, so it stays null.
    report = null;
  }
  reports.set(coin, { cutoff: cutoffSec, report });
  return report;
}

/**
 * What share of the supply the deployer is holding right now.
 *
 * Two ways to know, and the difference between them matters enough to report:
 *
 *   • Measured. The watcher records the token side of every trade, so a wallet's
 *     balance is what it bought minus what it sold. Exact.
 *   • Estimated. Older records carry only SOL, so each opening buy is replayed
 *     along the bonding curve in the order the wallets first appeared. That is
 *     the right curve and the right arithmetic on a slightly wrong ordering, and
 *     it errs by at most a few percent of supply on a busy opening.
 *
 * The "deployer bundle" is the deployer plus any wallet the cluster analyser
 * joined to it — same funder, same instant, same size. Those wallets are the
 * deployer with extra steps, and counting them separately is how a 40% position
 * reads as four innocent 10% ones.
 *
 * Returns `known: false` when there is no creator on the record or nothing the
 * creator did can be seen. That is not a pass and not a fail; `rejected` is only
 * ever true on a number that was actually worked out.
 */
export function creatorSupply(coin, { cutoffSec = 3, report = null } = {}) {
  const c = coin || {};
  const creator = c.creator ?? c.dev ?? null;
  const supply = Number(c.supply) > 0 ? Number(c.supply) : PUMP_LAUNCH.totalSupply;
  const assumedSupply = !(Number(c.supply) > 0);
  const who = Array.isArray(c.who) ? c.who : [];
  const early = who
    .filter((w) => w && (w.at ?? 0) <= cutoffSec)
    .sort((a, b) => (a.at ?? 0) - (b.at ?? 0));

  const none = {
    known: false, estimated: false, allocationOnly: false, supply, assumedSupply,
    creator, creatorTokens: null, creatorPct: null,
    bundleTokens: null, bundlePct: null, bundleWallets: creator ? [creator] : [],
    launchBlockWallets: 0, launchBlockPct: null,
    initialPct: null, pct: null, rejected: false, note: null,
  };
  if (!creator) return { ...none, note: 'no deployer on the record' };

  // Measured first. `tin`/`tout` only exist on records written after the token
  // side was added, so their absence means "older record", not "no trades".
  const measured = early.some((w) => w.tin != null || w.tout != null);
  const balances = new Map();
  if (measured) {
    for (const w of early) {
      // `tin`/`tout` are whole-token totals over the entire follow window, not
      // the opening — the watcher adds to them on every trade and never freezes
      // them at the cutoff. So `tin - tout` is the position a minute later, and
      // on a deployer that dumped at second twenty-seven that is zero.
      //
      // `out0` is the one sell figure that was frozen at the cutoff, in SOL. The
      // share of a wallet's selling that had happened by then is `out0 / out`,
      // and the same share of its tokens is what actually left inside the
      // window. On the 144 records carrying a token side, 73 deployers sold
      // nothing until after the cutoff and were being read as holding less than
      // they did.
      //
      // Without `out0` there is no way to date the selling, so the old whole-
      // window subtraction stands — it is the only answer available, and every
      // record written since the token side was added carries `out0` beside it.
      const addr = w.w ?? w.address;
      const boughtTokens = num(w.tin);
      const soldTokens = num(w.tout);
      const outAll = num(w.out);
      const inWindow = w.out0 == null
        ? soldTokens
        : outAll > 0
          ? soldTokens * Math.min(1, num(w.out0) / outAll)
          : 0;
      balances.set(addr, boughtTokens - inWindow);
    }
  } else {
    // Replay the opening along the curve the coin actually opened on.
    let state = c.curve ?? PUMP_LAUNCH;
    for (const w of early) {
      const spent = num(w.in);
      if (!(spent > 0)) continue;
      const step = buyTokens(spent, state);
      state = step.curve;
      const addr = w.w ?? w.address;
      // Only selling that happened *inside* the opening comes off. `out0` is the
      // wallet's position frozen at the cutoff; `out` is a total over the whole
      // sixty-second follow window and belongs to a different question.
      //
      // Subtracting `out` here is what this used to do, and it was wrong in the
      // way that matters most: a deployer that took 15% of the supply and dumped
      // it at second twenty-seven has `out` well above `in`, so the ratio
      // saturates, the whole position is treated as sold, and the check reports
      // a deployer holding nothing. On the recorded corpus that emptied 1,925 of
      // 3,412 deployers and hid 321 of the 433 launches this ceiling exists to
      // catch — the rule was not firing rarely, it was switched off.
      const windowed = w.out0 != null ? num(w.out0) : null;
      const sold = windowed != null && windowed > 0 && spent > 0
        ? step.tokens * Math.min(1, windowed / spent)
        : 0;
      balances.set(addr, (balances.get(addr) ?? 0) + step.tokens - sold);
    }
  }
  // Whether any of the opening's selling could be seen at all. Without it the
  // figures below are what each wallet was allocated rather than what it still
  // held at the cutoff — which is the number this ceiling is about anyway, but
  // the caller is told rather than left to assume.
  const allocationOnly = !measured && !early.some((w) => w.out0 != null);

  let creatorTokens = balances.get(creator) ?? null;
  // A deployer whose opening buy landed before the watcher saw a wallet row for
  // it still has a recorded allocation; use it rather than reporting nothing.
  const initialTokens = num(c.initialBuyTokens) || null;
  if (creatorTokens == null && initialTokens) creatorTokens = initialTokens;
  if (creatorTokens == null) {
    return { ...none, note: 'the deployer did not buy inside the opening' };
  }

  // Everyone the analyser puts in the same operator as the deployer.
  const rep = report ?? clusterReport(coin, cutoffSec);
  const cluster = (rep?.clusters ?? []).find((cl) => (cl.members ?? []).includes(creator));
  const bundleWallets = cluster ? [...cluster.members] : [creator];
  const bundleTokens = bundleWallets.reduce((s, a) => s + (balances.get(a) ?? (a === creator ? creatorTokens : 0)), 0);

  // The whole block of wallets that landed in the same instant as the deployer,
  // reported and deliberately not acted on.
  //
  // It is tempting to add these to the deployer's own position — on $Laura it is
  // 13 wallets holding 45% of the supply against the deployer's 15%. But
  // cluster.js is right that "a bundle is where every sniper in the world is
  // trying to be": the launch block catches the operator's wallets and everyone
  // racing them, and the corpus says so plainly. Refusing on this figure above
  // 15% would throw out 1,007 launches, and the 495 of them between 15% and 25%
  // are the *cleanest* cohort in the whole record at a 2.2% rug rate against a
  // 15.9% base. So the number is published for anyone tuning the rule and the
  // refusal below is left on the wallets the analyser actually joined.
  const block = (rep?.signals?.timing?.bundles ?? []).find((b) => (b.members ?? []).includes(creator));
  const blockWallets = block?.members ?? [];
  const blockTokens = blockWallets.reduce((s, a) => s + (balances.get(a) ?? 0), 0);

  const creatorPct = (creatorTokens / supply) * 100;
  const bundlePct = (bundleTokens / supply) * 100;
  const pct = Math.max(creatorPct, bundlePct);
  return {
    known: true,
    estimated: !measured,
    // True when nothing about the opening's selling was recorded, so these are
    // allocations rather than balances. See the note where it is worked out.
    allocationOnly,
    supply,
    assumedSupply,
    creator,
    creatorTokens: round(creatorTokens, 0),
    creatorPct: round(creatorPct, 2),
    bundleTokens: round(bundleTokens, 0),
    bundlePct: round(bundlePct, 2),
    bundleWallets,
    // Reported, never refused on.
    launchBlockWallets: blockWallets.length,
    launchBlockPct: blockWallets.length ? round((blockTokens / supply) * 100, 2) : null,
    // What the deployer took on its very first buy, before it could have sold
    // anything. The balance can fall below this; the allocation cannot change.
    initialPct: initialTokens ? round((initialTokens / supply) * 100, 2) : null,
    pct: round(pct, 2),
    rejected: pct > MAX_CREATOR_SUPPLY_PCT,
    note: bundleWallets.length > 1
      ? `the deployer and ${bundleWallets.length - 1} wallet${bundleWallets.length === 2 ? '' : 's'} tied to it hold ${pct.toFixed(1)}% of the supply`
      : `the deployer holds ${pct.toFixed(1)}% of the supply`,
  };
}

/**
 * How many of the opening buyers are actually different people.
 *
 * cluster.js does the work of deciding who is joined to whom — shared funding a
 * hop or two back, landing in the same instant, taking the same position to the
 * fourth decimal. This turns that into the one number every threshold in the
 * system was quietly assuming it already had: the count of independent buyers.
 *
 * Each cluster collapses to one buyer. Twelve wallets that are really six people
 * and two scripts is six buyers, not twelve, and every rule that reads "16+
 * buyers is the strongest signal we measured" meant sixteen people.
 */
export function sybilCohort(coin, { cutoffSec = 3, report = null } = {}) {
  const rep = report ?? clusterReport(coin, cutoffSec);
  const who = Array.isArray(coin?.who) ? coin.who : [];
  const earlyRows = who.filter((w) => w && (w.at ?? 0) <= cutoffSec && num(w.in) > 0);
  const early = rep?.window?.participants ?? earlyRows.length;
  const openSol = rep?.window?.sol_in ?? earlyRows.reduce((s, w) => s + num(w.in), 0);

  const clusters = (rep?.clusters ?? []).filter((c) => c.size >= 2);
  const clustered = clusters.reduce((s, c) => s + c.size, 0);
  const clusterSol = clusters.reduce((s, c) => s + num(c.sol_spent), 0);
  const largest = clusters.reduce((m, c) => Math.max(m, c.size), 0);
  const tags = rep?.risk_tags ?? [];

  // Every cluster still gets to be one buyer — the operator is a real bidder,
  // just one of them rather than six.
  const organic = Math.max(0, early - clustered) + clusters.length;
  const share = openSol > 0 ? clusterSol / openSol : 0;

  const flags = [];
  if (largest >= 2) {
    flags.push(
      `${clustered} of ${early} opening buyers are ${clusters.length} operator${clusters.length === 1 ? '' : 's'}, not ${clustered} people`,
    );
  }
  if (tags.includes('SHARED_FUNDER')) flags.push('opening buyers were funded from the same wallet');
  if (tags.includes('SAME_INSTANT_BUNDLE')) flags.push('several buyers landed in the same instant — they were sent together');
  if (share >= MAX_CLUSTER_SHARE && clustered >= 3) {
    flags.push(`coordinated wallets are ${(share * 100).toFixed(0)}% of the opening money`);
  }

  return {
    early,
    organic,
    clustered,
    clusters: clusters.length,
    largest,
    clusterSol: round(clusterSol, 3),
    clusterShare: round(share, 3),
    sharedFunder: tags.includes('SHARED_FUNDER'),
    sameInstant: tags.includes('SAME_INSTANT_BUNDLE'),
    identicalSizing: tags.includes('IDENTICAL_SIZING') || tags.includes('NEAR_IDENTICAL_SIZING'),
    confidence: rep?.confidence_score ?? null,
    // A launch is a bundle rather than a crowd when the coordinated wallets hold
    // most of the money and there is barely anyone left once they collapse to
    // one buyer. Either alone is common; both together is a staged opening.
    bundledLaunch: clustered >= 3 && share >= MAX_CLUSTER_SHARE && organic < 4,
    // The same figure against the lower refusal threshold. Kept separate from
    // `bundledLaunch` because that rule wants a thin crowd as well as a
    // concentrated one, and this is the money on its own. See
    // `MAX_CLUSTER_SHARE_REJECT` for what the corpus says about it.
    overCoordinated: clustered >= 3 && share > MAX_CLUSTER_SHARE_REJECT,
    flags,
  };
}

/**
 * Is the selling in the opening already bigger than everything that bought it.
 *
 * The window is the opening — three seconds, about seven slots — and getting the
 * window right is the whole difficulty here. A wallet row's `in`/`out` are
 * totals over the sixty-second follow window, not the opening, so comparing
 * them against the opening's buy depth compares a minute of selling against
 * three seconds of buying. That is not a rug detector; on the recorded corpus it
 * fires on 44% of every coin ever launched, because "an early buyer took profit
 * in the first minute" describes most coins that went up at all.
 *
 * So this reads only figures that were frozen at the cutoff:
 *
 *   • `out0` on a wallet row — its position at the opening cutoff. This is what
 *     makes the single-seller test possible, and it only exists on records
 *     written since it was added.
 *   • `open.solOut` — the opening's total selling, which the watcher has always
 *     frozen. Present on every record, so the drain test always runs.
 *
 * Where the per-wallet figure is missing the single-seller test reports itself
 * as unknown rather than falling back to the minute-long total. A check that
 * silently answers a different question is worse than one that says it cannot.
 */
export function sellImbalance(coin, { cutoffSec = 3, dominance = SELL_DOMINANCE } = {}) {
  const c = coin || {};
  const who = Array.isArray(c.who) ? c.who : [];
  const early = who.filter((w) => w && (w.at ?? 0) <= cutoffSec);
  // Positions as of the cutoff where they were recorded, falling back to the
  // running totals only for the buy side — a wallet's `in` at three seconds is
  // what it put in, and buying more later cannot make the opening look thinner.
  const perWallet = early.some((w) => w.out0 != null);
  const buyDepth = num(c.open?.solIn) || early.reduce((s, w) => s + num(w.in0 ?? w.in), 0);

  const sellers = perWallet
    ? early
        .filter((w) => num(w.out0) > 0)
        .map((w) => ({ wallet: w.w ?? w.address, sol: num(w.out0), at: w.at ?? 0 }))
        .sort((a, b) => b.sol - a.sol)
    : [];
  const top = sellers[0] ?? null;

  // The opening's own total, from whichever of the two windowed sources exists.
  // Deliberately not run through `num`: that turns a missing figure into a zero,
  // and "nothing sold" is a measurement while "not recorded" is the absence of
  // one. Reporting the second as the first is how a check that cannot see
  // anything reads as a check that saw nothing wrong.
  const openSold = c.open?.solOut != null
    ? Number(c.open.solOut)
    : perWallet
      ? sellers.reduce((s, x) => s + x.sol, 0)
      : c.solOut != null
        ? Number(c.solOut)
        : null;
  const soldKnown = openSold != null && Number.isFinite(openSold);

  const base = {
    known: buyDepth > 0 && soldKnown,
    // Whether the single-seller half of this could be answered at all.
    perWallet,
    blocks: Math.round(cutoffSec / SLOT_SEC),
    buyDepthSol: round(buyDepth, 3),
    soldSol: soldKnown ? round(openSold, 3) : null,
    sellers: perWallet ? sellers.length : (num(c.open?.sellers ?? c.sellers) || 0),
    topSeller: top?.wallet ?? null,
    topSellerSol: top ? round(top.sol, 3) : 0,
    ratio: buyDepth > 0 && top ? round(top.sol / buyDepth, 3) : 0,
    drainRatio: buyDepth > 0 && soldKnown ? round(openSold / buyDepth, 3) : 0,
    invalidated: false,
    note: null,
  };
  if (!(buyDepth > 0)) return base;

  // One address bigger than the book. The exit is larger than everything that
  // could absorb it, so there is no side of this trade to be on.
  if (top && top.sol / buyDepth >= dominance) {
    return {
      ...base,
      invalidated: true,
      note: `one wallet sold ${top.sol.toFixed(2)} SOL into the opening against ${buyDepth.toFixed(2)} SOL of buys — ${(base.ratio * 100).toFixed(0)}% of the depth, from one address`,
    };
  }
  // Or the opening as a whole drained. However many addresses did it, more left
  // than arrived and the depth the score is describing is not there.
  if (soldKnown && openSold >= buyDepth) {
    return {
      ...base,
      invalidated: true,
      note: `${openSold.toFixed(2)} SOL left the opening against ${buyDepth.toFixed(2)} SOL in — more went out than came in`,
    };
  }
  return base;
}

/**
 * Selling as a series rather than a single figure.
 *
 * `sellImbalance` answers "how much left the opening" with one number frozen at
 * the cutoff. That number cannot tell a launch where the selling arrived evenly
 * from one where nothing sold for two seconds and then everything did, and those
 * are different coins. This buckets the selling into slots and reports the
 * change from one slot to the next, so acceleration is visible.
 *
 * What it will not do is invent the resolution. Two facts about the record decide
 * what this can say, and both are worth stating plainly because they are the
 * whole reason the function looks like this:
 *
 *   • A wallet row carries `at` — when that wallet was *first* seen — and `out`,
 *     a total over the follow window. There is no timestamp on a sale. So the
 *     opening's selling cannot be split into slots from `who` at all.
 *   • Candles are the only per-time record of the sell side, and the watcher
 *     buckets them with `Math.floor(age)` — whole seconds. They carry the number
 *     of buys and sells in each second and a `volume` that is both sides added
 *     together, so there is no per-second figure for SOL sold either.
 *
 * So 0.4s is not reachable on any record written so far, and a three-second
 * opening holds at most three buckets. Of the 1,128 launches carrying candles,
 * 638 have two or more inside the opening and can be read as a series; the rest
 * — $Laura among them, where all sixteen buyers and the single sell landed
 * inside second zero — have one bucket, and one point is not a series. Those
 * report `openingKnown: false` and say why rather than returning a delta they
 * cannot support. Past the opening the series is real wherever the candles are,
 * which is how the $Laura dump at second twenty-seven is visible while the same
 * launch's opening is not.
 *
 * `slotSec` is what the caller asked for; `resolutionSec` is what the record
 * could deliver. When they differ the second is the one that was used, and a
 * caller comparing slots across coins should read it rather than assume.
 */
export function sellVelocity(coin, { cutoffSec = 3, slotSec = SLOT_SEC, now = Date.now() } = {}) {
  const c = coin || {};
  const candles = Array.isArray(c.market?.candles) ? c.market.candles : [];
  const bucketSec = num(c.market?.candleSeconds) || 1;
  const ageSec = c.t ? Math.max(0, (now - c.t) / 1000) : null;

  const base = {
    slotSec,
    resolutionSec: candles.length ? bucketSec : null,
    cutoffSec,
    known: false,
    slots: [],
    // The sharpest one-slot rise in selling, and where it happened.
    peakDelta: null,
    peakAtSec: null,
    accelerating: null,
    openingKnown: false,
    note: null,
  };

  if (!candles.length) {
    return { ...base, note: 'no candles on this record, so selling has no time to it' };
  }

  // One bucket per candle that exists. Seconds with no trade have no candle, and
  // a missing second is silence rather than a zero — filling it in would invent
  // a lull the record never saw.
  const slots = candles
    .map((k) => ({
      at: round(num(k?.s) * bucketSec, 2),
      buys: num(k?.buys),
      sells: num(k?.sells),
      // Both sides together. The watcher does not split volume by side, so this
      // is deliberately not called `sold`.
      volume: round(num(k?.volume), 4),
    }))
    .sort((a, b) => a.at - b.at);

  // The change in sell count from each bucket to the next one recorded.
  let peakDelta = null;
  let peakAtSec = null;
  for (let i = 0; i < slots.length; i++) {
    const prev = i > 0 ? slots[i - 1] : null;
    slots[i].delta = prev ? slots[i].sells - prev.sells : null;
    slots[i].net = slots[i].sells - slots[i].buys;
    if (slots[i].delta != null && (peakDelta == null || slots[i].delta > peakDelta)) {
      peakDelta = slots[i].delta;
      peakAtSec = slots[i].at;
    }
  }

  // Inside the opening there is at most one bucket, so there is nothing to take
  // a difference against. Say so rather than returning a one-point "series".
  const inOpening = slots.filter((s) => s.at < cutoffSec);
  const openingKnown = inOpening.length >= 2;

  const last = slots.at(-1);
  return {
    ...base,
    known: slots.length >= 2,
    openingKnown,
    slots,
    peakDelta,
    peakAtSec,
    // Above zero the current bucket is selling harder than the one before it.
    accelerating: last?.delta != null ? last.delta > 0 : null,
    ageSec: ageSec == null ? null : round(ageSec, 1),
    note: openingKnown
      ? null
      : `the opening is one ${bucketSec}s bucket, so selling inside it has no slots to compare — this reads the coin after ${cutoffSec}s`,
  };
}

/**
 * Estimated chance this coin reaches 1.5x, from who is in it.
 * Shrinkage matters more than it looks: a wallet with 10 coins and a 60% rate
 * is not a 60% wallet, and treating it as one is how a board starts promising
 * things it cannot deliver.
 */
export function runChance(known) {
  if (!known.length) return { p: RUN_WITHOUT, basis: 'no known wallet was in early' };
  const best = known[0];
  const p = (best.runRate * best.coins + RUN_WITH_KNOWN * SHRINK) / (best.coins + SHRINK);
  return {
    p,
    basis: `${short(best.wallet)} has ${best.coins} early coins at ${(best.runRate * 100).toFixed(0)}%`,
  };
}

/**
 * How the coin is built, which is a different question from whether it will go
 * up. Reads only what the watcher already records for every wallet.
 */
export function structure(coin, cutoffSec = 3, { report = null } = {}) {
  const who = coin.who || [];
  const early = who.filter((w) => w.at <= cutoffSec && w.in > 0);
  const flags = [];
  const solIn = early.reduce((s, w) => s + (w.in || 0), 0);

  // Several wallets putting in the same amount at the same moment are one
  // person. This is the single clearest structural tell in the data.
  const amounts = new Map();
  for (const w of early) {
    const key = Number(w.in).toFixed(4);
    amounts.set(key, (amounts.get(key) || 0) + 1);
  }
  let bundled = 0;
  for (const [, n] of amounts) if (n >= 3) bundled += n;
  if (bundled >= 3) flags.push(`${bundled} wallets bought the identical amount — one operator, not ${bundled} buyers`);

  // One wallet holding most of the opening money can leave whenever it likes.
  const biggest = early.reduce((m, w) => Math.max(m, w.in || 0), 0);
  const concentration = solIn > 0 ? biggest / solIn : 0;
  if (concentration >= 0.7 && early.length > 1) {
    flags.push(`one wallet is ${(concentration * 100).toFixed(0)}% of the opening money`);
  }

  // The creator trading its own coin inside the first minute.
  const creatorRow = who.find((w) => w.w === coin.creator);
  const creatorSold = !!creatorRow && (creatorRow.out || 0) > 0;
  if (creatorSold) flags.push('the creator sold inside the follow window');

  // Nothing to sell into.
  const sellers = who.filter((w) => (w.out || 0) > 0).length;
  if (solIn > 20 && early.length <= 2) {
    flags.push(`${solIn.toFixed(1)} SOL from only ${early.length} wallet${early.length === 1 ? '' : 's'}`);
  }

  // The structural refusals. They are computed here rather than in scoreCoin so
  // that everything reading `structure` — the board, the console, the candidate
  // filter — sees the same verdict from the same numbers.
  const rep = report ?? clusterReport(coin, cutoffSec);
  const supply = creatorSupply(coin, { cutoffSec, report: rep });
  const sybil = sybilCohort(coin, { cutoffSec, report: rep });
  const imbalance = sellImbalance(coin, { cutoffSec });
  // Reported beside the imbalance, not used to refuse. Inside the opening it
  // has no slots to compare on any record written so far, and a check that
  // cannot run is not a check that passed.
  const velocity = sellVelocity(coin, { cutoffSec });

  const blocking = [];
  if (supply.rejected) {
    blocking.push(
      `${supply.note} — above the ${MAX_CREATOR_SUPPLY_PCT}% ceiling${supply.estimated ? ', estimated from the curve' : ''}`,
    );
  }
  // The wallets that landed with the deployer, holding too much of the supply
  // between them. This is the rule that catches a launch where no single address
  // is over the ceiling and the block as a whole owns the coin.
  if (supply.launchBlockPct != null && supply.launchBlockPct > MAX_LAUNCH_BLOCK_PCT) {
    blocking.push(
      `${supply.launchBlockWallets} wallets landed with the deployer holding ${supply.launchBlockPct.toFixed(1)}% of the supply — above the ${MAX_LAUNCH_BLOCK_PCT}% launch-block ceiling`,
    );
  }
  if (sybil.overCoordinated) {
    blocking.push(
      `coordinated wallets are ${(sybil.clusterShare * 100).toFixed(0)}% of the opening money — above the ${(MAX_CLUSTER_SHARE_REJECT * 100).toFixed(0)}% ceiling`,
    );
  }
  if (imbalance.invalidated) blocking.push(imbalance.note);
  if (sybil.bundledLaunch) {
    blocking.push(
      `a bundled launch: ${sybil.clustered} of ${sybil.early} opening buyers are ${sybil.clusters} operator${sybil.clusters === 1 ? '' : 's'} holding ${(sybil.clusterShare * 100).toFixed(0)}% of the money`,
    );
  }
  flags.push(...blocking, ...sybil.flags.filter((f) => !blocking.some((b) => b.includes(f))));

  return {
    bundled,
    concentration: round(concentration, 3),
    creatorSold,
    sellers,
    earlySol: round(solIn, 3),
    // The count every buyer threshold in the system meant all along: how many
    // separate people are in, once wallets run by one operator collapse to one.
    organicBuyers: sybil.organic,
    supply,
    sybil,
    imbalance,
    sellVelocity: velocity,
    blocking,
    rejected: blocking.length > 0,
    flags,
  };
}

/** Social reach, as far as the record goes. */
function socialNote(coin) {
  const s = coin.social || {};
  if (s.kind === 'tweet' && !s.failed) {
    const bits = ['a readable X post'];
    if (s.followers != null) bits.push(`${s.followers.toLocaleString()} followers`);
    if (s.tweetAgeSec != null && s.tweetAgeSec <= 300) bits.push('posted in the last five minutes');
    if ((s.nth ?? 1) > 2) return { ok: false, text: `the same link was already used by ${s.nth} coins` };
    return { ok: true, text: bits.join(', ') };
  }
  if (s.kind === 'nometa' || s.failed) return { ok: false, text: 'social metadata could not be read' };
  return { ok: null, text: 'no social link' };
}

/**
 * The whole read on one coin. `score` is per coin and continuous, so two coins
 * in the same bucket no longer show the same number — which was the complaint
 * that started this.
 */
export function scoreCoin(coin, book, { cutoffSec = 3, now = Date.now() } = {}) {
  const known = book ? book.earlyKnown(coin, cutoffSec) : [];
  const chance = runChance(known);
  const report = clusterReport(coin, cutoffSec);
  const st = structure(coin, cutoffSec, { report });
  const social = socialNote(coin);
  const reasons = [];
  const cautions = [...st.flags];

  if (known.length) {
    const b = known[0];
    reasons.push(
      `${short(b.wallet)} bought at ${b.at}s — ${b.coins} early coins, ${(b.runRate * 100).toFixed(0)}% ran, mean ${b.meanPeak.toFixed(2)}x`,
    );
    if (known.length > 1) reasons.push(`${known.length} known wallets in within ${cutoffSec}s`);
    if (b.clusterSize) {
      cautions.push(`that wallet is one of ${b.clusterSize} with an identical record — treat as one operator`);
    }
  } else {
    cautions.push('no wallet with a track record bought this early');
  }

  if (st.earlySol >= 5) reasons.push(`${st.earlySol.toFixed(2)} SOL in the opening`);
  if (social.ok) reasons.push(social.text);
  else if (social.ok === false) cautions.push(social.text);

  // Ranking is the estimated chance, nudged by how many known wallets are in
  // and by opening size — both small next to the wallet itself.
  //
  // The sybil penalty replaces the flat one this used to carry. A flat penalty
  // said "some wallets matched" and stopped there; this is proportional to how
  // much of the opening money the coordinated wallets hold, because three
  // matching wallets in a forty-buyer crowd and three matching wallets that
  // *are* the crowd are not the same coin.
  const depth = Math.min(1, known.length / 3) * 0.05;
  const size = Math.min(1, st.earlySol / 25) * 0.03;
  const penalty = st.sybil.clustered >= 3 ? 0.04 + 0.1 * st.sybil.clusterShare : 0;
  // A refusal is not a low score, it is no score. Anything that survives the
  // structural checks is ranked; anything that does not is zero and says why.
  const score = st.rejected ? 0 : clamp(chance.p + depth + size - penalty);

  // When to click, given the coin. Handed the refusals so it never recommends
  // an entry into something the structure already threw out.
  const entry = entryTiming(coin, { blocking: st.blocking, imbalance: st.imbalance, cutoffSec, now });

  return {
    score: Math.round(score * 1000) / 10, // a percentage, one decimal
    runChance: Math.round(chance.p * 1000) / 10,
    basis: chance.basis,
    known: known.slice(0, 5).map((k) => ({
      wallet: k.wallet, coins: k.coins, runRate: k.runRate, meanPeak: k.meanPeak,
      tier: k.tier, at: k.at, solIn: round(k.solIn, 3), clusterSize: k.clusterSize ?? null,
    })),
    structure: st,
    // Hoisted out of `structure` because they are the parts a caller acts on
    // rather than reads: the refusal, the honest buyer count, and the moment.
    rejected: st.rejected,
    blocking: st.blocking,
    organicBuyers: st.organicBuyers,
    supply: st.supply,
    sybil: st.sybil,
    imbalance: st.imbalance,
    sellVelocity: st.sellVelocity,
    entry,
    reasons,
    cautions,
    hasKnown: known.length > 0,
  };
}

function short(w) {
  return w ? `${w.slice(0, 4)}…${w.slice(-4)}` : '?';
}
function clamp(x) {
  return Math.max(0, Math.min(0.99, x));
}
function num(v) {
  const n = Number(v);
  return Number.isFinite(n) ? n : 0;
}
function round(n, dp = 3) {
  const f = 10 ** dp;
  return Math.round(Number(n) * f) / f;
}

export { MIN_COINS };
