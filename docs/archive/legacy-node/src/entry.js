// When to buy, given that you are going to, and what to pay.
//
// Everything else in this project answers "is this coin worth anything". This
// answers the question that comes after it and has always been left to the
// person watching the screen: the coin is on the board, the score is what it is,
// do I click now or in twenty seconds.
//
// That question has a real answer and the record already contains it. A pump.fun
// launch has two distinct moments where a buy is cheap — the opening block,
// before the price has moved at all, and the pullback after the first spike —
// and between them is the one moment where it is expensive, which is exactly
// where a person watching a green number tends to click. score.js has measured
// that directly: the median peak lands at three seconds, the entry moment, and
// is given straight back. Chasing is the single most expensive habit the data
// shows, so the default here is to say no.
//
// Three inputs drive it, and all three come off the record without inference:
//
//   1. Early block velocity. How fast wallets and SOL arrived in the first few
//      slots, and whether that rate is still climbing or has rolled over. A
//      launch filling at four wallets a second is a different thing from the
//      same launch twenty seconds later at zero.
//   2. Bonding curve progression. How far along the curve the coin is, which is
//      recoverable exactly from the price — see curveFromPrice in pump.js. A
//      coin 3% of the way up has all of its move ahead of it; one at 70% is
//      being front-run by everybody watching the same progress bar.
//   3. The current flow. The candles carry a buy and a sell count per second, so
//      whether buyers still outnumber sellers — and by how much — is a measured
//      number rather than an impression of the chart.
//
// The second half of the file answers the question that follows a yes: what
// price, and how much slippage to allow. Both are read off the curve rather than
// guessed, and the arithmetic is exact — see entryPlan.
//
// Nothing here is a claim that these entries are profitable. Log.md's verdict
// stands and the board still says so in words. This says only: given that a buy
// is happening, here is the moment the record says is cheapest, here is the price
// that moment is worth, and here is why.

import { PUMP_LAUNCH, curveFromPrice, curveProgress, solToGraduation, buyTokens } from './pump.js';
import { CHEAPEST_SIZE_SOL } from './cost.js';

/**
 * Every answer this can give, in the order a person would want them: act now,
 * act while it climbs, act on the pullback that is already in, wait for the
 * pullback that is not, do nothing, and the two refusals.
 *
 * There are three ways to be told no and keeping them apart matters, because
 * they expire differently.
 *
 *   `AVOID` is the structural refusal arriving from score.js — the coin is built
 *   wrong and nothing that happens next changes that.
 *
 *   `AVOID_OVEREXTENDED` is a refusal about the moment rather than the coin: the
 *   thing was probably fine, it is simply most of the way up its curve with the
 *   buying dried up, and there is no version of this entry that is early to
 *   anything. The remaining move is not coming.
 *
 *   `NO_ENTRY` is neither. Nothing is happening worth a click right now, and
 *   that can change on the next trade.
 *
 * The two dip states are likewise kept apart. `WAIT_FOR_FIRST_DIP` means the
 * first move has happened and the price is still up there — the thing to do is
 * nothing, at a named level. `POST_FIRST_DIP` means the pullback has arrived,
 * landed in the band that counts as one, and is holding. Collapsing those two
 * into one answer is how a wait gets read as a buy.
 */
export const ENTRY_ACTIONS = [
  'IMMEDIATE_LAUNCH',
  'ON_BONDING_MOMENTUM',
  'POST_FIRST_DIP',
  'WAIT_FOR_FIRST_DIP',
  'NO_ENTRY',
  'AVOID_OVEREXTENDED',
  'AVOID',
];

/** Plain words for each, for a screen that has no room to explain itself. */
export const ENTRY_LABELS = {
  IMMEDIATE_LAUNCH: 'buy now',
  ON_BONDING_MOMENTUM: 'buy while it climbs',
  POST_FIRST_DIP: 'buy the pullback',
  WAIT_FOR_FIRST_DIP: 'wait for the pullback',
  NO_ENTRY: 'nothing here',
  AVOID_OVEREXTENDED: 'too late',
  AVOID: 'do not buy',
};

/** How long each answer is worth anything, in seconds. */
export const ENTRY_TTL_SEC = {
  IMMEDIATE_LAUNCH: 5,
  ON_BONDING_MOMENTUM: 20,
  POST_FIRST_DIP: 60,
  WAIT_FOR_FIRST_DIP: 30,
  NO_ENTRY: 15,
  AVOID_OVEREXTENDED: 0,
  AVOID: 0,
};

/**
 * How hard each answer is pulling at the person reading it. The board sorts on
 * this, so a buy that is live has to outrank a level that is being waited for.
 */
export const ENTRY_URGENCY = {
  IMMEDIATE_LAUNCH: 1,
  ON_BONDING_MOMENTUM: 0.6,
  POST_FIRST_DIP: 0.4,
  WAIT_FOR_FIRST_DIP: 0.2,
  NO_ENTRY: 0,
  AVOID_OVEREXTENDED: 0,
  AVOID: 0,
};

/** The answers that mean buy, as opposed to the ones that name a level or say no. */
export const BUY_ACTIONS = new Set(['IMMEDIATE_LAUNCH', 'ON_BONDING_MOMENTUM', 'POST_FIRST_DIP']);

/** The opening block, in seconds. Slots are 0.4s, so this is about five of them. */
export const LAUNCH_WINDOW_SEC = 2;

/**
 * Wallets a second inside the opening that counts as a launch worth being in.
 * Three is the point where a launch is filling rather than trickling: it is the
 * same "three is a pattern, two is a coin flip" the cluster rules use, per
 * second rather than per launch.
 */
export const HOT_WALLETS_PER_SEC = 3;

/** SOL a second that counts as real money arriving rather than dust. */
export const HOT_SOL_PER_SEC = 1;

/**
 * The fewest distinct wallets in the opening window that can count as a crowd,
 * and the most of the opening money any one of them may hold.
 *
 * This is the "distributed buyers" half of the launch call, and it is a
 * deliberately shallow test: it asks whether the opening money came from several
 * places or one, and nothing more. Whether those places are the same person
 * wearing several addresses is a question about identity, and cluster.js answers
 * it — that answer reaches this file as a `blocking` reason and refuses the coin
 * outright, which is a stronger thing than declining to call it a launch.
 */
export const MIN_LAUNCH_WALLETS = 3;
export const MAX_OPENING_SHARE = 0.6;

/**
 * How far up the curve is still early. Past this the coin is not a launch any
 * more, it is a position other people already hold.
 */
export const EARLY_BONDING = 0.12;

/** Past this the graduation trade is crowded and the easy part is over. */
export const LATE_BONDING = 0.6;

/**
 * Past this, with the buying dried up, the remaining move is not coming and the
 * answer is a refusal rather than a wait. Kept apart from LATE_BONDING because
 * the two say different things: 60% up a curve that is still being eaten is a
 * crowded trade, and 70% up one that has stopped is a finished one.
 */
export const OVEREXTENDED_BONDING = 0.7;

/**
 * How long the SOL still needed to graduate would take to arrive at the volume
 * of the last few seconds, before that volume counts as dried up.
 *
 * This is what "drying" is measured against, rather than a SOL-per-second floor
 * pulled out of the air. The question a coin near the top of its curve poses is
 * only ever "is the rest of this move going to happen", and dividing what is
 * left by what is currently arriving answers it directly. Ten minutes is far
 * longer than any pump.fun launch stays interesting, so a coin that needs more
 * than that is not slow, it is over.
 */
export const DRY_GRADUATION_SEC = 600;

/**
 * How much of the curve has to have been eaten since the entry moment before the
 * climb is a climb rather than noise. One percent of the way to graduation is
 * about 0.85 SOL of real buying — small, but unambiguously more than the tick
 * either side of a flat price.
 */
export const MOMENTUM_STEP = 0.01;

/**
 * How far buyers have to outnumber sellers before a climb is worth buying into.
 *
 * Two to one is the point where the flow is one-sided rather than merely
 * positive. A curve creeping up on 11 buys against 10 sells is being carried by
 * noise and turns over on the next seller; the same curve on 20 buys against 8
 * is being bought. The test is written as a multiplication rather than a
 * division so that a window nobody sold into is not a divide by zero.
 */
export const MOMENTUM_BUY_RATIO = 2;

/**
 * How far above entry counts as "the first move has already happened". A coin
 * 40% up from where it could have been bought is not being entered early, it is
 * being chased.
 */
export const CHASED_MULT = 1.4;

/**
 * The band that counts as the first dip, as a fraction off the high.
 *
 * Under fifteen percent is not a pullback, it is the price wobbling at its high,
 * and buying it is chasing with extra steps. Past thirty percent it is not a
 * pullback either — a coin that has given back a third of its move is not
 * pausing, it is going back where it came from, and the record is full of coins
 * that did exactly that all the way to zero. The buy is the middle.
 */
export const DIP_MIN_RETRACE = 0.15;
export const DIP_MAX_RETRACE = 0.3;

/**
 * How far above the recent low the price has to sit before the dip counts as
 * holding rather than still falling.
 *
 * Without this, every coin on its way down reads as a pullback for exactly as
 * long as it takes to pass through the band. Two percent is about one trade's
 * worth of curve movement at the sizes this project trades, so it separates a
 * price that has turned from one that is merely between sellers.
 */
export const HOLD_MARGIN = 0.02;

/**
 * How long a coin can go without a trade before there is nothing to time.
 *
 * This is what separates a setup from a record. A finished coin still has an
 * entry price, a peak and a curve position, so every rule below will happily
 * produce an answer for it — and every one of those answers is about a moment
 * that has already passed. Two minutes of silence is generous for a pump.fun
 * launch, where the entire interesting window is usually under one.
 */
export const QUIET_SEC = 120;

/**
 * How far behind the coin's own age the last candle can be before "right now"
 * stops meaning right now.
 *
 * The buy/sell split only exists in the candles, and the watcher stops writing
 * them at the end of the follow window. So on a stored record the final candle
 * sits at second 57 of a coin that is now four minutes old, and reading it as
 * the current flow reports a sell-off that finished three minutes ago as though
 * it were happening.
 */
export const FLOW_FRESH_SEC = 15;

/** How much of the recent past "the current flow" covers, in seconds. */
export const FLOW_WINDOW_SEC = 15;

/**
 * How long it takes a decision to become a filled transaction — the wallet, the
 * signature, the slot. Two seconds is deliberately pessimistic; the point of the
 * figure is to price how much of the curve other people eat while you are still
 * clicking, and being wrong about that in the optimistic direction is how a buy
 * comes back rejected in a launch that was worth being in.
 */
export const FILL_LATENCY_SEC = 2;

/**
 * The slippage a buy is sent with, floored and capped.
 *
 * The floor exists because the curve ticks under every single trade: a buy sent
 * with no tolerance at all fails on the person in front of you, every time. The
 * cap exists because past a point you are not tolerating slippage, you are
 * agreeing in advance to buy whatever the price turns out to be — which on a
 * launch that is being spiked is the whole loss in one number.
 */
export const MIN_SLIPPAGE_PCT = 1;
export const MAX_SLIPPAGE_PCT = 25;

/**
 * How fast the opening filled, and whether it is still filling.
 *
 * Wallet arrivals come from `who`, which every shape of record carries — a coin
 * still inside its opening window, one the tracker has adopted, one read back
 * off disk. Candles are used when they exist and only to answer what `who`
 * cannot: whether sellers have started to outnumber buyers, and in which second.
 */
export function blockVelocity(coin, { windowSec = LAUNCH_WINDOW_SEC, cutoffSec = 3, now = Date.now() } = {}) {
  const c = coin || {};
  const who = Array.isArray(c.who) ? c.who : [];
  const early = who.filter((w) => w && num(w.at) <= cutoffSec && num(w.in) > 0);

  const inWindow = early.filter((w) => num(w.at) <= windowSec);
  const wallets = inWindow.length;
  const sol = inWindow.reduce((s, w) => s + num(w.in), 0);

  // The whole opening, so "first two seconds against the rest of the window"
  // can be compared rather than asserted.
  const laterWallets = early.length - wallets;
  const laterSpan = Math.max(0.001, cutoffSec - windowSec);

  const ageSec = c.t ? Math.max(0, (now - c.t) / 1000) : null;

  // Candles are per-second and carry the buy/sell split, which is the only place
  // a turn in the flow is visible.
  const candles = Array.isArray(c.market?.candles) ? c.market.candles : [];
  const seconds = num(c.market?.candleSeconds) || 1;
  let heavySellSec = null;
  let lastBuys = null;
  let lastSells = null;
  let lastCandleSec = null;
  for (const k of candles) {
    const at = num(k?.s) * seconds;
    if (heavySellSec == null && at > 0 && num(k?.sells) > num(k?.buys)) heavySellSec = at;
    lastBuys = num(k?.buys);
    lastSells = num(k?.sells);
    lastCandleSec = at;
  }

  // The biggest single wallet's share of the opening money. One address holding
  // most of what came in is one buyer with a large position, whatever the wallet
  // count says, and that is not the thing "a launch filling" describes.
  const biggest = inWindow.reduce((m, w) => Math.max(m, num(w.in)), 0);
  const topShare = sol > 0 ? biggest / sol : 0;

  const walletsPerSec = windowSec > 0 ? wallets / windowSec : 0;
  const laterPerSec = laterWallets / laterSpan;
  return {
    windowSec,
    wallets,
    sol: round(sol, 3),
    walletsPerSec: round(walletsPerSec, 2),
    solPerSec: round(windowSec > 0 ? sol / windowSec : 0, 3),
    // Above 1 the launch was still gathering pace after the first blocks; below
    // it the crowd arrived at once and left.
    accelerating: walletsPerSec > 0 ? round(laterPerSec / walletsPerSec, 2) : null,
    openingWallets: early.length,
    topShare: round(topShare, 3),
    // Several places rather than one, and enough of them to be a crowd. Says
    // nothing about whether they are the same person — see MIN_LAUNCH_WALLETS.
    distributed: wallets >= MIN_LAUNCH_WALLETS && topShare <= MAX_OPENING_SHARE,
    ageSec: ageSec == null ? null : round(ageSec, 1),
    heavySellSec,
    sellingHard: lastSells != null && lastBuys != null ? lastSells > lastBuys : null,
    // Which second the flow reading above is actually from, and how far behind
    // the coin's own age that is. Without this a caller has no way to tell a
    // sell-off happening now from one recorded a minute before it looked.
    lastCandleSec,
    flowAgeSec: lastCandleSec != null && ageSec != null ? round(Math.max(0, ageSec - lastCandleSec), 1) : null,
  };
}

/**
 * What the last few seconds look like: how one-sided the flow is, how much money
 * is moving, and how low the price has been.
 *
 * `blockVelocity` describes the opening and never stops describing it. This
 * describes now, which is a different question and the one the momentum and dip
 * rules actually ask. Both are reported so a caller can see the difference: a
 * launch that filled at four wallets a second and is currently doing nothing is
 * two true statements, and reading only the first is how a dead coin keeps
 * looking hot.
 *
 * A window with no sellers in it is the reason the ratio is reported alongside
 * `buyShare` rather than on its own — the ratio is undefined there, the share is
 * 1, and the rules below test the share.
 */
export function currentFlow(coin, { windowSec = FLOW_WINDOW_SEC, now = Date.now() } = {}) {
  const c = coin || {};
  const candles = Array.isArray(c.market?.candles) ? c.market.candles : [];
  const seconds = num(c.market?.candleSeconds) || 1;
  const ageSec = c.t ? Math.max(0, (now - c.t) / 1000) : null;
  const blank = {
    known: false, fresh: false, ageSec: ageSec == null ? null : round(ageSec, 1),
    atSec: null, behindSec: null, windowSec, spanSec: null,
    buys: 0, sells: 0, ratio: null, buyShare: null,
    volumeSol: 0, solPerSec: null, low: null, high: null, sellingHard: null,
  };
  if (!candles.length) return blank;

  const atSec = num(candles.at(-1)?.s) * seconds;
  const from = atSec - windowSec + seconds;
  let buys = 0;
  let sells = 0;
  let volumeSol = 0;
  let low = null;
  let high = null;
  for (const k of candles) {
    const at = num(k?.s) * seconds;
    if (at < from) continue;
    buys += num(k?.buys);
    sells += num(k?.sells);
    volumeSol += num(k?.volume);
    const l = num(k?.l);
    const h = num(k?.h);
    if (l > 0) low = low == null ? l : Math.min(low, l);
    if (h > 0) high = high == null ? h : Math.max(high, h);
  }
  // The window cannot cover time the coin has not lived, so a coin four seconds
  // old is measured over four seconds and not fifteen.
  const spanSec = Math.max(seconds, Math.min(windowSec, atSec + seconds));
  const trades = buys + sells;
  const behindSec = ageSec == null ? null : Math.max(0, ageSec - atSec);
  const last = candles.at(-1);
  return {
    known: true,
    fresh: behindSec == null || behindSec <= FLOW_FRESH_SEC,
    ageSec: ageSec == null ? null : round(ageSec, 1),
    atSec: round(atSec, 1),
    behindSec: behindSec == null ? null : round(behindSec, 1),
    windowSec,
    spanSec: round(spanSec, 1),
    buys,
    sells,
    ratio: sells > 0 ? round(buys / sells, 2) : null,
    buyShare: trades > 0 ? round(buys / trades, 3) : null,
    volumeSol: round(volumeSol, 3),
    solPerSec: round(volumeSol / spanSec, 4),
    low,
    high,
    // The current second on its own, which is where a turn shows up first. The
    // window above is deliberately slower — one bad second is not a reversal,
    // but it is enough to stop a buy going in on this tick.
    sellingHard: last ? num(last.sells) > num(last.buys) : null,
  };
}

/**
 * Where the coin sits on its bonding curve, from the only two prices the record
 * is guaranteed to have.
 *
 * `entry` is what a buy at the opening cutoff would have paid and `last` is the
 * live price, so the difference between their two curve positions is how much of
 * the curve has been eaten since the moment the score describes.
 */
export function bondingState(coin) {
  const c = coin || {};
  const open = c.curve ?? PUMP_LAUNCH;
  const entryPrice = num(c.entry ?? c.outcome?.entry);
  const price = num(c.last ?? c.currentPrice ?? c.outcome?.last) || entryPrice;
  if (!(price > 0)) {
    return { known: false, pct: null, entryPct: null, movedPct: null, raisedSol: null, toGraduationSol: null };
  }
  const at = curveFromPrice(price, open);
  const entryAt = entryPrice > 0 ? curveFromPrice(entryPrice, open) : null;
  const pct = curveProgress(at, open);
  const entryPct = entryAt ? curveProgress(entryAt, open) : null;
  const opened = c.curve?.virtualSol ?? PUMP_LAUNCH.virtualSol;
  return {
    known: true,
    pct: round(pct, 4),
    entryPct: entryPct == null ? null : round(entryPct, 4),
    movedPct: entryPct == null ? null : round(pct - entryPct, 4),
    raisedSol: round(Math.max(0, at.virtualSol - opened), 3),
    toGraduationSol: round(solToGraduation(at, open), 3),
  };
}

// ---------------------------------------------------------------------------
// The price
// ---------------------------------------------------------------------------

/**
 * What to pay, and how much slippage to send the buy with.
 *
 * Both numbers are read off the curve rather than fitted, and the arithmetic is
 * short enough to check by hand. A buy of `s` SOL against virtual reserves
 * `vs`/`vt` takes `vt·s/(vs+s)` tokens out, so the average price paid is
 * `s / tokens = (vs+s)/vt`, against a spot of `vs/vt`. Everything cancels: the
 * average fill is exactly `1 + s/vs` times spot. A quarter of a SOL into a
 * freshly launched curve, where vs is 30, fills 0.83% above the screen price.
 *
 * The same identity prices the other half of slippage, which is not yours. If
 * `a` SOL of other people's buying lands in front of you, the spot you were
 * quoted has already moved and your own fill happens against the curve they
 * left. Multiply the two and it collapses the same way: the total premium over
 * the quoted price is `(a + s)/vs`. So the tolerance to send is one division,
 * and the only estimated term in it is how much other people buy while you are
 * clicking — `a`, which is the current rate times FILL_LATENCY_SEC.
 *
 * The zone is the price the answer is about. For the two buy-now answers that is
 * the screen price and everything up to the worst tolerable fill. For the two dip
 * answers it is the retracement band itself, priced off the high — which is the
 * useful part of a wait, because it turns "wait for the pullback" into a number
 * to set an alert at.
 *
 * @param {object} coin the record.
 * @param {string} action one of ENTRY_ACTIONS.
 * @param {object} [options]
 * @param {number} [options.sizeSol] the position, in SOL. Defaults to the size
 *   cost.js worked out as the cheapest a round trip can be.
 * @param {number} [options.flowSolPerSec] the current rate of buying, if already
 *   read. Falls back to the opening rate.
 * @param {number} [options.now] the clock, for tests.
 * @returns {object} EntryPlan
 */
export function entryPlan(coin, action, { sizeSol = CHEAPEST_SIZE_SOL, flowSolPerSec = null, cutoffSec = 3, now = Date.now() } = {}) {
  const c = coin || {};
  const size = num(sizeSol) > 0 ? num(sizeSol) : CHEAPEST_SIZE_SOL;
  const open = c.curve ?? PUMP_LAUNCH;
  const entryPrice = num(c.entry ?? c.outcome?.entry);
  const price = num(c.last ?? c.currentPrice ?? c.outcome?.last) || entryPrice;
  const mult = entryPrice > 0 && price > 0 ? price / entryPrice : 1;
  const peak = Math.max(num(c.hi) || 1, mult);

  const none = (basis) => ({
    actionable: false, buyNow: false, sizeSol: round(size, 3),
    price: price > 0 ? sig(price) : null,
    zone: null, zoneMult: null, refPrice: null,
    ownImpactPct: null, driftPct: null, maxSlippagePct: null, maxFillPrice: null,
    tokens: null, basis,
  });

  const waiting = action === 'WAIT_FOR_FIRST_DIP';
  const buying = BUY_ACTIONS.has(action);
  if (!buying && !waiting) return none('there is no buy to price');
  if (!(price > 0)) return none('no price on the record to work from');

  // The band, for the two answers that are about one. Both are quoted off the
  // high rather than off the current price, because the high is the thing the
  // retracement is a retracement of.
  //
  // `hi` is a multiple of the entry price rather than a price, so a record with
  // no entry price on it has no high either — only a ratio with nothing to
  // multiply. There is no band to name in that case and the honest answer is to
  // say so, because a level quoted off nothing is a number that looks like a
  // price and is not one, and the whole value of being told to wait is the
  // number to set the alert at.
  const dip = action === 'POST_FIRST_DIP' || waiting;
  let zoneMult = null;
  if (dip && peak > 1 && entryPrice > 0) {
    zoneMult = { low: round(peak * (1 - DIP_MAX_RETRACE), 3), high: round(peak * (1 - DIP_MIN_RETRACE), 3) };
  }
  if (waiting && !zoneMult) {
    return none(peak > 1
      ? 'there is no entry price on the record to quote the pullback against'
      : 'nothing has moved yet, so there is no pullback to wait for');
  }

  // What a fill would realistically happen at, and so what the impact below is
  // measured against: the top of the band for a dip, the screen price otherwise.
  const refPrice = zoneMult ? entryPrice * zoneMult.high : price;
  const at = curveFromPrice(refPrice, open);
  const vs = at ? at.virtualSol : 0;

  // What other people take out of the curve while the click becomes a signature.
  const rate = num(flowSolPerSec) > 0 ? num(flowSolPerSec) : blockVelocity(c, { cutoffSec, now }).solPerSec;
  const ahead = Math.max(0, rate) * FILL_LATENCY_SEC;

  const ownImpactPct = vs > 0 ? (size / vs) * 100 : null;
  const driftPct = vs > 0 ? (ahead / vs) * 100 : null;
  const raw = vs > 0 ? ((size + ahead) / vs) * 100 : null;
  const maxSlippagePct = raw == null ? null : round(Math.min(MAX_SLIPPAGE_PCT, Math.max(MIN_SLIPPAGE_PCT, raw)), 2);
  const maxFillPrice = maxSlippagePct == null ? null : refPrice * (1 + maxSlippagePct / 100);
  const tokens = at ? Math.round(buyTokens(size, at).tokens) : null;

  const zone = zoneMult
    ? { low: sig(entryPrice * zoneMult.low), high: sig(entryPrice * zoneMult.high) }
    : { low: sig(price), high: maxFillPrice == null ? sig(price) : sig(maxFillPrice) };

  return {
    // Whether there is something to do with this price right now. A dip that has
    // not happened yet has a zone and is not actionable, and keeping those two
    // apart is the entire reason the zone is worth showing.
    actionable: buying,
    buyNow: buying,
    sizeSol: round(size, 3),
    price: sig(price),
    zone,
    zoneMult,
    refPrice: sig(refPrice),
    ownImpactPct: ownImpactPct == null ? null : round(ownImpactPct, 2),
    driftPct: driftPct == null ? null : round(driftPct, 2),
    maxSlippagePct,
    maxFillPrice: maxFillPrice == null ? null : sig(maxFillPrice),
    tokens,
    basis: waiting
      ? `a fill between ${zoneMult.low}× and ${zoneMult.high}× of the entry price, once it gets there`
      : `${round(size, 3)} SOL fills about ${round(ownImpactPct ?? 0, 2)}% above the screen price, ${round(driftPct ?? 0, 2)}% more for what lands in front of it`,
  };
}

/**
 * The recommendation.
 *
 * Order is the whole design. A refusal beats everything, then the moment that
 * cannot come back, then the dip band, then the flow, then the two entries, then
 * the default — which is that there is nothing here, because most coins most of
 * the time are not worth a click and a signal that always fires is not a signal.
 *
 * @param {object} coin a record, a tracker row, or a live opening row.
 * @param {object} [options]
 * @param {Array}  [options.blocking] reasons the coin has already been refused.
 * @param {object} [options.imbalance] the sellImbalance read, if already taken.
 * @param {number} [options.cutoffSec=3] the opening window.
 * @param {number} [options.sizeSol] the position the price plan is worked for.
 * @param {number} [options.now] the clock, for tests.
 * @returns {object} EntrySignal
 */
export function entryTiming(coin, { blocking = [], imbalance = null, cutoffSec = 3, sizeSol = CHEAPEST_SIZE_SOL, now = Date.now() } = {}) {
  const c = coin || {};
  const velocity = blockVelocity(c, { cutoffSec, now });
  const flow = currentFlow(c, { now });
  const bonding = bondingState(c);
  const entryPrice = num(c.entry ?? c.outcome?.entry);
  const price = num(c.last ?? c.currentPrice ?? c.outcome?.last) || entryPrice;
  const mult = entryPrice > 0 && price > 0 ? price / entryPrice : 1;
  const peak = Math.max(num(c.hi) || 1, mult);
  const ageSec = velocity.ageSec;

  const say = (action, reason, waitFor = null) => ({
    action,
    label: ENTRY_LABELS[action],
    reason,
    waitFor,
    // How long this answer is worth acting on. An entry call with no expiry is
    // how a five-second window gets clicked two minutes late.
    goodForSec: ENTRY_TTL_SEC[action],
    // The clock it was made against, and the instant it stops being worth
    // acting on. A duration on its own cannot be turned into a deadline by
    // anything downstream: it has to be added to something, and a caller
    // reading a stored call has no way to know what. Adding it to the coin's
    // age gets a launch window that expires five seconds after the launch and a
    // climb call that is stale before it is drawn; adding it to the reader's own
    // clock keeps a five-second window alive for as long as nobody refreshes.
    //
    // Nothing that means no gets an expiry. A refusal is not a countdown that
    // ends in permission, so `null` here says there is nothing to wait out.
    at: now,
    expiresAt: ENTRY_TTL_SEC[action] > 0 ? now + ENTRY_TTL_SEC[action] * 1000 : null,
    urgency: ENTRY_URGENCY[action],
    mult: round(mult, 3),
    peak: round(peak, 3),
    ageSec,
    velocity,
    flow,
    bonding,
    // What the answer costs, for the answers that have a price. Worked out from
    // the same numbers the call was made on, so the two can never disagree.
    plan: entryPlan(c, action, { sizeSol, flowSolPerSec: flow.fresh ? flow.solPerSec : null, cutoffSec, now }),
  });

  // 1. Nothing else matters if the coin has already been refused.
  if (blocking.length) return say('AVOID', blocking[0]);
  if (imbalance?.invalidated) return say('AVOID', imbalance.note ?? 'one seller is bigger than the opening book');

  // 2. Nothing has traded in a long time, so there is no moment to time. This
  //    has to come before every price rule below, because a finished record
  //    still has an entry price and a peak and will otherwise be given advice
  //    about a moment that passed several minutes ago.
  const quietSec = c.lastTradeAt ? (now - c.lastTradeAt) / 1000 : null;
  if (quietSec != null && quietSec > QUIET_SEC) {
    return say(
      'NO_ENTRY',
      `nothing has traded for ${quietSec < 120 ? `${Math.round(quietSec)}s` : `${Math.round(quietSec / 60)} minutes`} — this is a record of a launch, not a live one`,
      'trading to resume',
    );
  }

  // 3. Most of the way up the curve with the buying gone. Not a wait: the SOL
  //    that would finish this curve is not arriving, and no amount of patience
  //    changes a number that is already spent.
  //
  //    Both halves are required, and the volume half is only asked of a flow
  //    reading that is current. A stored record has no live volume to speak of,
  //    and calling that "dried up" would refuse every finished coin on the disk
  //    for the crime of having been written down.
  const secToGraduate = bonding.known && flow.fresh && flow.solPerSec > 0
    ? bonding.toGraduationSol / flow.solPerSec
    : flow.fresh && flow.known && bonding.known ? Infinity : null;
  const drying = secToGraduate != null && secToGraduate > DRY_GRADUATION_SEC;
  if (bonding.known && bonding.pct >= OVEREXTENDED_BONDING && drying) {
    return say(
      'AVOID_OVEREXTENDED',
      `${(bonding.pct * 100).toFixed(0)}% of the way to graduation and the buying has dried up — ${bonding.toGraduationSol.toFixed(0)} SOL left at ${flow.solPerSec.toFixed(2)} SOL a second is not arriving`,
    );
  }

  // 4. The first move has happened, so the only question left is where the
  //    pullback lands and whether it holds.
  //
  //    Three answers come out of one measurement. Under the band the price is
  //    still at its high and buying it is chasing; inside the band and holding
  //    is the entry this rule exists for; past the band it is not a pullback at
  //    all. That last case is load-bearing — a peak that has already happened
  //    never stops having happened, so without an upper edge a coin that spiked
  //    and gave every bit of it back reads as a dip worth buying for the rest of
  //    its life, which on the recorded coins is most of them.
  if (peak >= CHASED_MULT) {
    const retrace = 1 - mult / peak;
    const support = supportRead(flow, price);
    if (retrace < DIP_MIN_RETRACE) {
      return say(
        'WAIT_FOR_FIRST_DIP',
        `already ${((mult - 1) * 100).toFixed(0)}% above the entry price and ${(retrace * 100).toFixed(0)}% off the high — this is the part that gets given back`,
        `a retrace into ${(peak * (1 - DIP_MAX_RETRACE)).toFixed(2)}×–${(peak * (1 - DIP_MIN_RETRACE)).toFixed(2)}× of the entry`,
      );
    }
    if (retrace <= DIP_MAX_RETRACE && mult > 1) {
      if (support.holding) {
        return say(
          `POST_FIRST_DIP`,
          `${(retrace * 100).toFixed(0)}% back off a ${peak.toFixed(2)}× high and still above the entry — the pullback is in and holding`,
          'the first higher low after this',
        );
      }
      return say(
        'WAIT_FOR_FIRST_DIP',
        `${(retrace * 100).toFixed(0)}% back off a ${peak.toFixed(2)}× high, but ${support.why}`,
        'the price to stop making new lows',
      );
    }
    return say(
      'NO_ENTRY',
      `${(retrace * 100).toFixed(0)}% off a ${peak.toFixed(2)}× high — that is not a pullback, it is the move coming back`,
      'a base, which this does not have yet',
    );
  }

  // 5. Already sold into, with no first move behind it to be dipping from. Not
  //    a refusal — the setup can come back — but not a moment to be buying
  //    either. Only asked of a flow reading that is actually current: the
  //    candles stop at the end of the follow window, so on a stored record this
  //    would otherwise report a minute-old second as "right now".
  if (flow.fresh && velocity.sellingHard) {
    return say('NO_ENTRY', 'sellers outnumber buyers in the current second', 'the flow to turn back to buys');
  }

  // 6. Too far up the curve to be early to anything. Still moving, or this would
  //    have been refused outright three rules ago.
  if (bonding.known && bonding.pct >= LATE_BONDING) {
    return say(
      'NO_ENTRY',
      `${(bonding.pct * 100).toFixed(0)}% of the way to graduation — everyone watching the same bar is already in`,
      'the graduation itself, or nothing',
    );
  }

  // 7. Still in the opening block, filling, and filling from more than one
  //    place. This is the only moment the price has genuinely not moved yet.
  const inOpening = ageSec == null || ageSec <= cutoffSec + LAUNCH_WINDOW_SEC;
  const hot = velocity.walletsPerSec >= HOT_WALLETS_PER_SEC || velocity.solPerSec >= HOT_SOL_PER_SEC;
  if (inOpening && hot && velocity.distributed && (!bonding.known || bonding.pct < EARLY_BONDING)) {
    const upCurve = bonding.known ? `${(bonding.pct * 100).toFixed(1)}% up the curve` : 'the price has not moved yet';
    return say(
      'IMMEDIATE_LAUNCH',
      `${velocity.wallets} wallets and ${velocity.sol.toFixed(2)} SOL inside the first ${velocity.windowSec}s, none of them more than ${(velocity.topShare * 100).toFixed(0)}% of it, ${upCurve}`,
    );
  }
  if (inOpening && hot && !velocity.distributed) {
    return say(
      'NO_ENTRY',
      velocity.wallets < MIN_LAUNCH_WALLETS
        ? `${velocity.sol.toFixed(2)} SOL of opening buying from ${velocity.wallets} wallet${velocity.wallets === 1 ? '' : 's'} — that is a position, not a launch`
        : `one wallet is ${(velocity.topShare * 100).toFixed(0)}% of the opening money — that is a position, not a launch`,
      'the rest of the opening to fill out',
    );
  }

  // 8. Past the opening, but the curve is still being eaten and buyers still
  //    outnumber sellers. The trade is the climb, and it ends at the graduation
  //    crowd.
  //
  //    The curve test is its own movement since the entry moment, not how fast
  //    the opening filled. Almost every launch is front-loaded — the opening
  //    rate says whether the launch was hot, and says nothing at all about
  //    whether the coin is still being bought a minute later. The flow test is
  //    what says that, when there is a flow to read; on a record with no candles
  //    it is unanswerable, and an unanswerable check is not a failed one, so the
  //    curve carries the call on its own and the words say which evidence it is.
  const flowKnown = flow.known && flow.fresh && flow.buys + flow.sells > 0;
  const oneSided = flow.buys > flow.sells * MOMENTUM_BUY_RATIO;
  const climbing = bonding.known && bonding.movedPct != null && bonding.movedPct >= MOMENTUM_STEP;

  // Whether the price is still at its high, which "climbing" on its own does not
  // say. A curve position is a displacement from the entry moment and nothing
  // else, so it stays positive the whole way down from a peak: a coin that ran
  // to 1.3× and gave back a fifth of it is still well above where it started,
  // and the curve test alone will call that "still climbing" and keep calling it
  // that until the price goes under the entry price. Rule 4 catches the same
  // shape once the move is big enough to be worth chasing; this is that read for
  // the small moves that never got there, and without it the momentum entry
  // fires into a price that has already turned over.
  const offHigh = peak > 1 ? 1 - mult / peak : 0;
  const atItsHigh = offHigh < DIP_MIN_RETRACE;
  if (climbing && atItsHigh && bonding.pct < LATE_BONDING && (!flowKnown || oneSided)) {
    const buyers = flowKnown
      ? ` on ${flow.buys} buys against ${flow.sells} sell${flow.sells === 1 ? '' : 's'}`
      : '';
    return say(
      'ON_BONDING_MOMENTUM',
      `${(bonding.pct * 100).toFixed(0)}% up the curve and still climbing${buyers} — ${bonding.toGraduationSol.toFixed(0)} SOL left to graduation`,
      null,
    );
  }
  if (climbing && !atItsHigh) {
    return say(
      'NO_ENTRY',
      `up on the entry moment but ${(offHigh * 100).toFixed(0)}% under its own high — and a ${peak.toFixed(2)}× high is too small a move to be pulling back from`,
      'a new high, or a move big enough to have a pullback worth buying',
    );
  }
  if (climbing && flowKnown && !oneSided) {
    return say(
      'NO_ENTRY',
      `the curve is moving but only ${flow.buys} buys against ${flow.sells} sells — that is not one-sided enough to be carried`,
      `buyers to outnumber sellers ${MOMENTUM_BUY_RATIO} to 1`,
    );
  }

  // 9. Nothing is happening. Saying so is the point.
  return say(
    'NO_ENTRY',
    velocity.openingWallets < 2
      ? 'almost nobody bought the opening — there is no launch to be early to'
      : 'the opening has passed and the curve is not moving',
    'buying volume to return',
  );
}

/**
 * Whether a price that has come off its high is holding or still going.
 *
 * The lows in the flow window are the evidence. A price sitting on the lowest
 * print of the last few seconds has not turned; one above it by more than a
 * trade's worth of curve has. The current second's buy/sell split is the second
 * half — a bounce that is still being sold into is not support, it is the pause
 * between two sellers.
 *
 * With no candles there is nothing to read, and the answer is that it holds. An
 * unanswerable check is not a failed one, and the alternative is refusing every
 * record written before the candles existed.
 */
function supportRead(flow, price) {
  if (!flow?.known || !flow.fresh || !(flow.low > 0) || !(price > 0)) {
    return { known: false, holding: true, offLowPct: null, why: 'there is no recent low to check it against' };
  }
  const offLow = price / flow.low - 1;
  if (flow.sellingHard) {
    return { known: true, holding: false, offLowPct: round(offLow * 100, 1), why: 'sellers still outnumber buyers in the current second' };
  }
  if (offLow <= HOLD_MARGIN) {
    return { known: true, holding: false, offLowPct: round(offLow * 100, 1), why: 'the price is sitting on the low rather than off it' };
  }
  return { known: true, holding: true, offLowPct: round(offLow * 100, 1), why: `${(offLow * 100).toFixed(0)}% up off the low of the last ${flow.spanSec}s` };
}

function num(v) {
  const n = Number(v);
  return Number.isFinite(n) ? n : 0;
}
function round(n, dp = 3) {
  const f = 10 ** dp;
  return Math.round(Number(n) * f) / f;
}
/**
 * Round to significant figures rather than decimal places. Every price in this
 * file is SOL per whole token, which on a fresh curve is 2.8e-8 — rounding that
 * to three decimals is zero, and a zero price silently turns every ratio below
 * into nonsense.
 */
function sig(n, digits = 6) {
  const x = Number(n);
  if (!Number.isFinite(x) || x === 0) return 0;
  const f = 10 ** (digits - Math.ceil(Math.log10(Math.abs(x))));
  return Math.round(x * f) / f;
}
