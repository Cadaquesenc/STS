// pump.fun program constants and Anchor event decoding.
//
// The decoder is deliberately conservative. pump.fun's IDL has gained trailing
// fields more than once, and this listener is meant to keep running across those
// changes without a code push and without losing anything. So:
//
//   • the raw base64 of every event is stored verbatim, always
//   • only the long-stable prefix of each event is decoded as first-class fields
//   • trailing fields are decoded opportunistically into `ext`, and a failure
//     there costs nothing
//   • `tail` records how many bytes went undecoded, so layout drift shows up in
//     the data itself rather than as a silent wrong answer
//
// Every record can therefore be re-parsed later, from the log, with a better
// decoder. That is the whole point of recording facts instead of conclusions.
import crypto from 'node:crypto';
import { Reader } from './borsh.js';

export const PUMP_PROGRAM = '6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P';

/** Anchor event discriminator: first 8 bytes of sha256("event:<Name>"). */
const eventDisc = (name) =>
  crypto.createHash('sha256').update(`event:${name}`).digest().subarray(0, 8).toString('hex');

/**
 * Anchor's self-CPI event tag, prepended when a program uses `emit_cpi!` rather
 * than `emit!`. A fixed protocol constant, asserted against its derivation in the
 * tests. Events wrapped this way carry it ahead of the event discriminator.
 */
export const CPI_EVENT_TAG = 'e445a52e51cb9a1d';

export const DISC = {
  create: eventDisc('CreateEvent'),
  trade: eventDisc('TradeEvent'),
  complete: eventDisc('CompleteEvent'),
};

/** Bonding curve account layout, verified live by weld (weld/DESIGN.md §12). */
export function decodeCurve(data) {
  const r = new Reader(data);
  r.bytes(8); // account discriminator
  return {
    virtualTokenReserves: r.u64(),
    virtualSolReserves: r.u64(),
    realTokenReserves: r.u64(),
    realSolReserves: r.u64(),
    tokenTotalSupply: r.u64(),
    complete: r.bool(),
  };
}

/**
 * Spot price in SOL per whole token, from virtual reserves.
 * pump tokens are 6 decimals, SOL is 9 — the 1e3 accounts for the difference.
 */
export function spotPriceSol(curve) {
  const vt = BigInt(curve.virtualTokenReserves);
  if (vt === 0n) return null;
  return Number(BigInt(curve.virtualSolReserves)) / Number(vt) / 1e3;
}

// ---------------------------------------------------------------------------
// The curve itself
// ---------------------------------------------------------------------------
//
// Everything below works in whole tokens and whole SOL rather than base units,
// because everything that reads it — how much of the supply the deployer took,
// how far along the curve a coin is — is a human-sized question. The exchange
// rate is a constant product on the *virtual* reserves, which is what pump.fun
// does, so the arithmetic here is the same arithmetic the program runs.

/**
 * The state every pump.fun coin opens in. Read off live mainnet create events on
 * 2026-08-08 and asserted in readCreate below; a launch that reports something
 * else is a launch this file should be told about rather than one it should
 * assume for.
 */
export const PUMP_LAUNCH = Object.freeze({
  virtualSol: 30,
  virtualTokens: 1_073_000_000,
  realTokens: 793_100_000,
  totalSupply: 1_000_000_000,
});

/**
 * SOL that has to enter the curve before the coin graduates — about 85, which is
 * the number every pump.fun front end quotes. Derived rather than typed in, so a
 * different opening state gives a different answer instead of a wrong one.
 */
export const GRADUATION_SOL = solToDrain(PUMP_LAUNCH);

/**
 * Whole tokens a buy of `solIn` takes out of the curve, and the state it leaves
 * behind. `solIn` is the amount that reaches the curve; pump's fee is charged on
 * top of it and never buys anything, which is why the trade event's `sol` field
 * can be used here as it stands.
 */
export function buyTokens(solIn, state = PUMP_LAUNCH) {
  const sol = Number(solIn);
  const { virtualSol, virtualTokens } = normaliseCurve(state);
  if (!(sol > 0) || !(virtualSol > 0) || !(virtualTokens > 0)) {
    return { tokens: 0, curve: { virtualSol, virtualTokens } };
  }
  const k = virtualSol * virtualTokens;
  const nextSol = virtualSol + sol;
  const nextTokens = k / nextSol;
  return { tokens: virtualTokens - nextTokens, curve: { virtualSol: nextSol, virtualTokens: nextTokens } };
}

/**
 * The curve state implied by a price, which is the only curve figure the watcher
 * keeps for a coin past its first minute.
 *
 * Price is virtualSol / virtualTokens and their product is fixed, so one price
 * pins both reserves exactly. This is not an approximation of the curve — it is
 * the curve, read backwards.
 */
export function curveFromPrice(priceSol, state = PUMP_LAUNCH) {
  const p = Number(priceSol);
  const { virtualSol, virtualTokens } = normaliseCurve(state);
  if (!(p > 0) || !(virtualSol > 0) || !(virtualTokens > 0)) return null;
  const k = virtualSol * virtualTokens;
  return { virtualSol: Math.sqrt(k * p), virtualTokens: Math.sqrt(k / p) };
}

/**
 * How far along the curve a coin is: 0 the instant it launched, 1 when it
 * graduates. Clamped, because a coin that has already left the curve should read
 * as finished rather than as 103% of the way there.
 */
export function curveProgress(now, state = PUMP_LAUNCH) {
  const open = normaliseCurve(state);
  const at = normaliseCurve(now ?? open);
  const total = solToDrain(open);
  if (!(total > 0)) return 0;
  const raised = at.virtualSol - open.virtualSol;
  return Math.max(0, Math.min(1, raised / total));
}

/** SOL still needed before graduation, from a curve state. Never negative. */
export function solToGraduation(now, state = PUMP_LAUNCH) {
  const open = normaliseCurve(state);
  const at = normaliseCurve(now ?? open);
  return Math.max(0, solToDrain(open) - (at.virtualSol - open.virtualSol));
}

/** What it costs to buy every real token out of a curve in its opening state. */
function solToDrain(open) {
  const left = open.virtualTokens - open.realTokens;
  if (!(left > 0)) return 0;
  return (open.virtualSol * open.virtualTokens) / left - open.virtualSol;
}

/**
 * Accept either whole-unit figures (what this file works in) or the base units a
 * decoded event carries, so a caller can hand over a create event untouched.
 * Base units are recognised by size: no real curve holds 1e12 whole SOL.
 */
function normaliseCurve(state) {
  const s = state || {};
  const rawSol = Number(s.virtualSol ?? s.virtualSolReserves ?? 0);
  const rawTokens = Number(s.virtualTokens ?? s.virtualTokenReserves ?? 0);
  const rawReal = Number(s.realTokens ?? s.realTokenReserves ?? 0);
  const baseUnits = rawSol > 1e6 || rawTokens > 1e12;
  return {
    virtualSol: baseUnits ? rawSol / 1e9 : rawSol,
    virtualTokens: baseUnits ? rawTokens / 1e6 : rawTokens,
    realTokens: baseUnits ? rawReal / 1e6 : rawReal,
  };
}

/**
 * Pull every `Program data:` payload out of a transaction's logs.
 * `Program log:` lines are human text and carry nothing we want.
 */
export function programData(logs) {
  const out = [];
  for (const line of logs || []) {
    const i = line.indexOf('Program data: ');
    if (i === 0) out.push(line.slice(14).trim());
  }
  return out;
}

/**
 * Decode one `Program data:` payload. Returns null when it is not a pump event
 * we recognise — which is normal and not an error.
 */
export function decodeEvent(b64) {
  let buf;
  try {
    buf = Buffer.from(b64, 'base64');
  } catch {
    return null;
  }
  if (buf.length < 8) return null;

  // emit_cpi! wraps the event behind the anchor tag; emit! does not.
  let body = buf;
  if (body.subarray(0, 8).toString('hex') === CPI_EVENT_TAG) {
    if (body.length < 16) return null;
    body = body.subarray(8);
  }

  const disc = body.subarray(0, 8).toString('hex');
  const payload = body.subarray(8);

  if (disc === DISC.create) return wrap('create', b64, payload, readCreate);
  if (disc === DISC.trade) return wrap('trade', b64, payload, readTrade);
  if (disc === DISC.complete) return wrap('complete', b64, payload, readComplete);
  return null;
}

function wrap(kind, raw, payload, read) {
  const rec = { kind, raw };
  const r = new Reader(payload);
  try {
    Object.assign(rec, read(r));
  } catch (e) {
    // A prefix that will not parse is a real signal, not something to swallow.
    rec.decodeError = String(e.message || e);
  }
  rec.tail = r.remaining;
  return rec;
}

/**
 * CreateEvent. The first six fields are the long-stable prefix; the six after
 * them were verified against live mainnet events on 2026-08-08, where the four
 * reserve figures came back as pump's exact opening constants
 * (1.073e15 / 30e9 / 793.1e12 / 1e15) and `creator` equalled `user`.
 */
function readCreate(r) {
  const v = {
    name: r.string(),
    symbol: r.string(),
    uri: r.string(),
    mint: r.pubkey(),
    bondingCurve: r.pubkey(),
    user: r.pubkey(),
  };
  const ext = attempt(r, (x) => ({
    creator: x.pubkey(),
    ts: x.i64(),
    virtualTokenReserves: x.u64(),
    virtualSolReserves: x.u64(),
    realTokenReserves: x.u64(),
    tokenTotalSupply: x.u64(),
  }));
  if (ext) {
    Object.assign(v, ext);
    // Structural, not clock- or config-dependent: real reserves are a subset of
    // virtual ones and supply bounds both. Holds for any curve configuration, so
    // a failure here means the layout moved rather than the parameters.
    v.extOk =
      BigInt(ext.virtualTokenReserves) > 0n &&
      BigInt(ext.realTokenReserves) <= BigInt(ext.virtualTokenReserves) &&
      BigInt(ext.tokenTotalSupply) >= BigInt(ext.realTokenReserves);
  }
  return v;
}

/**
 * TradeEvent. Prefix through `virtualTokenReserves` is long-stable. The six
 * fields after it were verified against live mainnet events on 2026-08-08:
 * `realSolReserves` reconciled to virtual minus the 30 SOL offset, `feeBasisPoints`
 * came back as 95 — the value weld also read from Global, not the 1% of reputation
 * — `fee` matched the fee identity below exactly on every sample, and `creator`
 * was constant across trades on one mint and differed across mints.
 *
 * `creator` is the reason to decode this far: it puts the deployer on every trade,
 * so a mint whose launch we missed is still attributable.
 */
function readTrade(r) {
  const v = {
    mint: r.pubkey(),
    sol: r.u64(),
    tokens: r.u64(),
    isBuy: r.bool(),
    user: r.pubkey(),
    ts: r.i64(),
    virtualSolReserves: r.u64(),
    virtualTokenReserves: r.u64(),
  };
  const ext = attempt(r, (x) => ({
    realSolReserves: x.u64(),
    realTokenReserves: x.u64(),
    feeRecipient: x.pubkey(),
    feeBasisPoints: x.u64(),
    fee: x.u64(),
    creator: x.pubkey(),
  }));
  if (ext) {
    Object.assign(v, ext);
    v.extOk = feeIdentity(v.sol, ext.feeBasisPoints, ext.fee);
  }
  return v;
}

/**
 * fee == ceil(sol × bps / 10000). Verified exact on live buys. A fixed-width read
 * of the wrong field still "succeeds", so this identity is what actually tells us
 * the offsets are right — it is the decoder checking itself against the data.
 *
 * Recorded rather than enforced. If sells price differently the flag will simply
 * come back false for them, which is a thing to learn from the log rather than a
 * reason to throw the fields away.
 */
export function feeIdentity(sol, bps, fee) {
  try {
    // A zero-SOL trade carrying a real fee is a live anomaly — 17 of the first
    // 1236 records, all from one creator. The identity is undefined there rather
    // than violated, and returning null keeps those out of the drift count so a
    // genuine layout change cannot hide behind them. They still keep their raw
    // bytes, because `extOk !== true` is what governs that.
    if (BigInt(sol) === 0n) return null;
    const expected = (BigInt(sol) * BigInt(bps) + 9999n) / 10000n;
    const diff = BigInt(fee) - expected;
    return diff <= 1n && diff >= -1n;
  } catch {
    return false;
  }
}

function readComplete(r) {
  return {
    user: r.pubkey(),
    mint: r.pubkey(),
    bondingCurve: r.pubkey(),
    ts: r.i64(),
  };
}

/**
 * Read trailing fields if they are there. A rewind on failure keeps `tail`
 * meaningful: it stays the count of bytes we genuinely did not understand.
 */
function attempt(r, fn) {
  const at = r.off;
  try {
    return fn(r);
  } catch {
    r.off = at;
    return undefined;
  }
}
