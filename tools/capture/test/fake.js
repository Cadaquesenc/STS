// Test scaffolding: a websocket that is not a network, and a borsh writer that
// builds the exact bytes pump.fun puts on the wire.
//
// Real event bytes rather than a stubbed decoder, because the two defects being
// tested here live at the far end of the pipeline — the tracker row that comes
// out after a launch, its trades, the follow mark and a shutdown — and a test
// that skips the decode is not testing that path, it is testing itself.
import crypto from 'node:crypto';
import { encode as b58encode, decode as b58decode } from '../src/base58.js';

// ---------------------------------------------------------------------------
// A websocket that goes nowhere
// ---------------------------------------------------------------------------

/**
 * Stands in for the global `WebSocket` that `src/ws.js` constructs.
 *
 * `install()` swaps it in and hands back the restore function, so a test can
 * drive `watch()` end to end without a socket, a server or a port.
 */
export class FakeSocket {
  static last = null;

  constructor(url) {
    this.url = url;
    this.sent = [];
    this.closed = false;
    this.listeners = new Map();
    FakeSocket.last = this;
  }

  addEventListener(kind, fn) {
    if (!this.listeners.has(kind)) this.listeners.set(kind, []);
    this.listeners.get(kind).push(fn);
  }

  send(data) {
    this.sent.push(data);
  }

  close() {
    this.closed = true;
  }

  fire(kind, ev = {}) {
    for (const fn of this.listeners.get(kind) ?? []) fn(ev);
  }

  /** Connect, and answer the subscribe request the way a node does. */
  open(subscriptionId = 42) {
    this.fire('open');
    this.deliver({ jsonrpc: '2.0', id: 1, result: subscriptionId });
  }

  deliver(msg) {
    this.fire('message', { data: JSON.stringify(msg) });
  }

  /** One `logsNotification` carrying these already-encoded event payloads. */
  notify(signature, b64s, { err = null, slot = 1 } = {}) {
    this.deliver({
      jsonrpc: '2.0',
      method: 'logsNotification',
      params: {
        result: {
          context: { slot },
          value: {
            signature,
            err,
            logs: b64s.map((b) => `Program data: ${b}`),
          },
        },
      },
    });
  }
}

export function install() {
  const previous = globalThis.WebSocket;
  FakeSocket.last = null;
  globalThis.WebSocket = FakeSocket;
  return () => {
    globalThis.WebSocket = previous;
  };
}

// ---------------------------------------------------------------------------
// Borsh, going the other way
// ---------------------------------------------------------------------------

class Writer {
  constructor() {
    this.parts = [];
  }
  u8(v) {
    this.parts.push(Buffer.from([v]));
    return this;
  }
  bool(v) {
    return this.u8(v ? 1 : 0);
  }
  u32(v) {
    const b = Buffer.alloc(4);
    b.writeUInt32LE(v);
    this.parts.push(b);
    return this;
  }
  u64(v) {
    const b = Buffer.alloc(8);
    b.writeBigUInt64LE(BigInt(v));
    this.parts.push(b);
    return this;
  }
  i64(v) {
    const b = Buffer.alloc(8);
    b.writeBigInt64LE(BigInt(v));
    this.parts.push(b);
    return this;
  }
  string(s) {
    const b = Buffer.from(s, 'utf8');
    return this.u32(b.length).raw(b);
  }
  pubkey(address) {
    return this.raw(Buffer.from(b58decode(address)));
  }
  raw(b) {
    this.parts.push(Buffer.from(b));
    return this;
  }
  done() {
    return Buffer.concat(this.parts);
  }
}

const disc = (name) =>
  crypto.createHash('sha256').update(`event:${name}`).digest().subarray(0, 8);

const CPI_TAG = Buffer.from('e445a52e51cb9a1d', 'hex');

const wrap = (name, body) =>
  Buffer.concat([CPI_TAG, disc(name), body]).toString('base64');

/** A deterministic, valid-looking address from a seed string. */
export function address(seed) {
  return b58encode(crypto.createHash('sha256').update(seed).digest());
}

const LAMPORTS = 1_000_000_000n;

/** The state pump opens a curve in. Both sides, because the product is what is fixed. */
const LAUNCH_SOL = 30;
const LAUNCH_TOKENS = 1_073_000_000;

/**
 * A CreateEvent on the opening curve. `virtualSol` and `virtualTokens` set the
 * launch price; every trade below is what moves it.
 */
export function createEvent({
  mint,
  symbol = 'TEST',
  name = 'a test coin',
  uri = '',
  user,
  ts = 1_700_000_000,
  virtualSol = 30,
  virtualTokens = 1_073_000_000,
} = {}) {
  const body = new Writer()
    .string(name)
    .string(symbol)
    .string(uri)
    .pubkey(mint)
    .pubkey(address(`curve:${mint}`))
    .pubkey(user)
    .pubkey(user) // creator
    .i64(ts)
    .u64(BigInt(Math.round(virtualTokens * 1e6)))
    .u64(BigInt(Math.round(virtualSol * 1e9)))
    .u64(BigInt(Math.round(793_100_000 * 1e6))) // realTokenReserves
    .u64(BigInt(1_000_000_000 * 1e6)) // tokenTotalSupply
    .done();
  return wrap('CreateEvent', body);
}

/**
 * A TradeEvent. `virtualSol` / `virtualTokens` are the curve *after* the trade,
 * which is what `watch.js` reads the price off — so they are how a test says
 * "and then the price was this".
 */
export function tradeEvent({
  mint,
  user,
  sol = 0.1,
  tokens = null,
  isBuy = true,
  ts = 1_700_000_000,
  virtualSol = 30,
  // The curve is a constant product, so naming one side names the other. This
  // used to be a fixed 1,073,000,000 whatever `virtualSol` was told to do,
  // which is a curve state the chain cannot produce: the price moved with no
  // tokens leaving. `checkCurve`'s conservation rule catches exactly that, and
  // the first thing it caught was these fixtures.
  virtualTokens = (LAUNCH_SOL * LAUNCH_TOKENS) / virtualSol,
  // A normal pump trade pays 95 basis points. Zero is the marker of the actor
  // whose trades leave the curve in a state the launch curve cannot produce —
  // real, on chain, and the signature `checkCurve` looks for. A test asks for
  // it explicitly, because nothing should produce one by accident.
  feeBasisPoints = 95,
} = {}) {
  // pump's curve opens at 30 virtual SOL and only ever goes up from there, so
  // real reserves are virtual minus 30 and can never be negative. Asking for
  // less is asking for a state the chain cannot produce — W21's check C17 is
  // about exactly this, and 199 recorded rows fail it. Said out loud here
  // rather than left to overflow a u64 further down.
  if (virtualSol < 30) {
    throw new RangeError(`virtualSol ${virtualSol} is below pump's 30 SOL floor — no such curve state exists`);
  }
  // Tokens the curve has given up to reach this state. A test that says "and
  // then the price was this" is also saying how many tokens had to leave to get
  // there, and a fixture that hands over fewer is inventing a coin whose peak
  // nobody could have sold into. Over-credits on a sequence of trades, which is
  // the safe direction: a test that wants the rule to fire says so explicitly.
  const moved = tokens ?? LAUNCH_TOKENS - virtualTokens;
  const body = new Writer()
    .pubkey(mint)
    .u64(BigInt(Math.round(sol * 1e9)))
    .u64(BigInt(Math.round(moved * 1e6)))
    .bool(isBuy)
    .pubkey(user)
    .i64(ts)
    .u64(BigInt(Math.round(virtualSol * 1e9)))
    .u64(BigInt(Math.round(virtualTokens * 1e6)))
    .u64(BigInt(Math.round(virtualSol * 1e9)) - 30n * LAMPORTS) // realSolReserves
    .u64(BigInt(Math.round(700_000_000 * 1e6))) // realTokenReserves
    .pubkey(address('fee-recipient'))
    .u64(BigInt(feeBasisPoints))
    .u64(BigInt(Math.ceil((sol * 1e9 * feeBasisPoints) / 10_000)))
    .pubkey(user) // creator
    .done();
  return wrap('TradeEvent', body);
}

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
