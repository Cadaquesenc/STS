// What a transaction cost to land, resolved offline against the signatures the
// recorder wrote down.
//
// The cost model is the crux of the whole go/no-go question and it has never
// been measured. It was rebuilt weeks after the fact out of 25 transactions in
// another project's files and was wrong by a factor of twenty, because the
// recorder kept no `sig` and no `slot`. It keeps both now — and this is the pass
// that turns them into a number.
//
// Three rules, and they are the reason this is a separate command rather than a
// few lines inside `watch.js`:
//
//   • **Nothing here runs on the hot path.** The listener must never wait on a
//     network round trip; a socket that blocks on `getTransaction` drops
//     launches, and dropped launches are the one thing that cannot be recovered.
//   • **It is resumable and idempotent.** A signature is resolved once. The
//     output is read back on the next run and anything already in it is skipped,
//     so an interrupted pass costs nothing and a finished one can be re-run.
//   • **It never writes to the capture.** The costs go in their own file beside
//     it, joined on `sig`. The recorded rows are irreplaceable and no later pass
//     gets to rewrite them.
//
// `flux/src/enrich.js` is the reference and does the same fetch by signature.
// This adds the two things it leaves out: the base fee and the priority fee are
// separated, and the ComputeBudget instructions are decoded, so "what did it
// cost to land" can be answered as "what did I pay for, and what did I get".
import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';
import { decode as b58decode } from './base58.js';
import { Appender } from './record.js';
import { SCHEMA } from './session.js';

/** Solana's base signature fee, per required signature. A protocol constant. */
export const LAMPORTS_PER_SIGNATURE = 5_000;

export const LAMPORTS = 1_000_000_000;

/** The ComputeBudget program, whose instructions are what a priority fee is bought with. */
export const COMPUTE_BUDGET_PROGRAM = 'ComputeBudget111111111111111111111111111111';

/**
 * Jito's mainnet tip accounts, copied from `src-tauri/src/execution.rs`.
 *
 * A tip is an ordinary transfer, so it is indistinguishable from any other
 * transfer except by its destination. The list lives in the Rust executor
 * because that is what would pay one; `enrich.test.js` reads that file and
 * asserts these are the same eight, so the copy cannot drift silently.
 *
 * **Published addresses, not checked against a network by anything in this
 * build.** A stale list under-reports a tip; it never invents one.
 */
export const JITO_TIP_ACCOUNTS = [
  '96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5',
  'HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe',
  'Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY',
  'ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49',
  'DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh',
  'ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt',
  'DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL',
  '3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT',
];

const JITO = new Set(JITO_TIP_ACCOUNTS);

/**
 * Every address a `getTransaction` result indexes balances by, in order.
 *
 * A versioned transaction keeps some of its addresses in a lookup table outside
 * the message, and `preBalances`/`postBalances` are indexed across the message
 * keys **followed by** those. A reader that stops at `accountKeys` lines the
 * wrong balance up against the wrong address on any modern transaction. Same
 * rule, and the same reason, as `rpc.js`'s own `accountKeys`.
 */
export function keysOf(tx) {
  const raw = tx?.transaction?.message?.accountKeys ?? [];
  const keys = raw.map((k) => (typeof k === 'string' ? k : k?.pubkey)).filter(Boolean);
  const loaded = tx?.meta?.loadedAddresses;
  if (loaded) keys.push(...(loaded.writable ?? []), ...(loaded.readonly ?? []));
  return keys;
}

/**
 * The compute budget a transaction asked for, off its own instructions.
 *
 * The layout is one discriminator byte and then the argument, little-endian:
 *
 *   0  RequestUnitsDeprecated        u32 units, u32 additional fee
 *   1  RequestHeapFrame              u32 bytes
 *   2  SetComputeUnitLimit           u32 units
 *   3  SetComputeUnitPrice           u64 micro-lamports per compute unit
 *   4  SetLoadedAccountsDataSizeLimit u32 bytes
 *
 * Only 2 and 3 change what a transaction pays. A transaction that sets neither
 * pays the base fee alone and takes the default limit, which is a real and
 * common answer — `null` on both means "did not ask", not "could not tell".
 */
export function computeBudget(tx) {
  const keys = keysOf(tx);
  const ix = tx?.transaction?.message?.instructions ?? [];
  let cuLimit = null;
  let cuPrice = null;
  for (const i of ix) {
    if (keys[i?.programIdIndex] !== COMPUTE_BUDGET_PROGRAM) continue;
    let data;
    try {
      data = Buffer.from(b58decode(i.data ?? ''));
    } catch {
      continue; // a payload we cannot read is not a reason to lose the rest
    }
    if (data.length < 5) continue;
    if (data[0] === 2) cuLimit = data.readUInt32LE(1);
    else if (data[0] === 3 && data.length >= 9) cuPrice = Number(data.readBigUInt64LE(1));
  }
  return { cuLimit, cuPrice };
}

/**
 * Whatever left the fee payer for a Jito tip account inside this transaction.
 *
 * Read off the balance changes rather than off the instructions, for the same
 * reason `rpc.js` reads a funder that way: a tip can be a plain transfer or can
 * come through a program, and the balances know about both at once.
 */
export function jitoTip(tx) {
  const keys = keysOf(tx);
  const pre = tx?.meta?.preBalances;
  const post = tx?.meta?.postBalances;
  if (!Array.isArray(pre) || !Array.isArray(post)) return null;
  let tip = 0;
  let found = false;
  for (let i = 0; i < keys.length; i++) {
    if (!JITO.has(keys[i])) continue;
    const gained = Number(post[i] ?? 0) - Number(pre[i] ?? 0);
    if (gained > 0) {
      tip += gained;
      found = true;
    }
  }
  return found ? tip : null;
}

/**
 * One `getTransaction` result reduced to what it cost and what bought it.
 *
 * Everything is in lamports, because that is the unit the chain charges in and
 * a conversion is a place for a factor of a thousand to hide — which is exactly
 * what happened to the last cost model. `feeTotalSol` is there for reading and
 * is derived from `feeTotal`, not measured separately.
 *
 * It is **not** called `feeSol`. The coin record already has an
 * `outcome.feeSol`, and that one is the pump *trading* fee the coin's traders
 * paid over the follow window — a different number, in a different file, that
 * these rows are meant to be joined against on `sig`. Two fields with one name
 * across a join is how this project spent a fortnight disagreeing about what
 * `entry` meant.
 *
 * `feePriority` is the **charged** priority fee, `feeTotal - feeBase`, not the
 * `cuLimit x cuPrice` a transaction asked for. Those differ whenever the
 * transaction used less compute than it reserved, and the charged one is the
 * one that comes out of the bankroll.
 */
export function costOf(tx) {
  if (!tx) return null;
  const message = tx?.transaction?.message;
  const signers = Number(message?.header?.numRequiredSignatures ?? tx?.transaction?.signatures?.length ?? 1);
  const feeTotal = tx?.meta?.fee ?? null;
  const feeBase = signers > 0 ? signers * LAMPORTS_PER_SIGNATURE : null;
  const { cuLimit, cuPrice } = computeBudget(tx);
  const keys = keysOf(tx);
  return {
    slot: tx?.slot ?? null,
    blockTime: tx?.blockTime ?? null,
    // A failed transaction still landed and still paid, which is the entire
    // point of measuring it: 93.2% of pump transactions on the wire fail.
    err: tx?.meta?.err ? true : false,
    feePayer: keys[0] ?? null,
    signers: keys.slice(0, Math.max(1, signers)),
    feeTotal,
    feeBase,
    feePriority: feeTotal != null && feeBase != null ? Math.max(0, feeTotal - feeBase) : null,
    feeTotalSol: feeTotal != null ? feeTotal / LAMPORTS : null,
    cuLimit,
    cuPrice,
    cuUsed: tx?.meta?.computeUnitsConsumed ?? null,
    jitoTip: jitoTip(tx),
  };
}

/**
 * Every signature a coin file names, and what each one was.
 *
 * Two kinds, and they are different questions: the launch transaction is what a
 * deployer paid, and an opening buyer's is what a wallet competing for the same
 * position paid. The second is the one a strategy is being priced against, and
 * there are thousands of them per session against the 25 the last cost model was
 * rebuilt from.
 */
export async function signaturesIn(file) {
  const found = new Map();
  const rl = readline.createInterface({ input: fs.createReadStream(file), crlfDelay: Infinity });
  for await (const line of rl) {
    const text = line.trim();
    if (!text) continue;
    let r;
    try {
      r = JSON.parse(text);
    } catch {
      continue; // `capture check` is what complains about unreadable rows
    }
    if (r?.k) continue; // a row about the run, not about a coin
    if (typeof r.sig === 'string' && !found.has(r.sig)) {
      found.set(r.sig, { sig: r.sig, why: 'launch', mint: r.mint ?? null, wallet: r.creator ?? null });
    }
    for (const w of r.who ?? []) {
      if (typeof w?.sig === 'string' && !found.has(w.sig)) {
        found.set(w.sig, { sig: w.sig, why: 'opening-buy', mint: r.mint ?? null, wallet: w.w ?? null });
      }
    }
  }
  return [...found.values()];
}

/** Signatures already resolved, so a second run costs nothing. */
export async function alreadyDone(file) {
  const done = new Set();
  if (!fs.existsSync(file)) return done;
  const rl = readline.createInterface({ input: fs.createReadStream(file), crlfDelay: Infinity });
  for await (const line of rl) {
    const text = line.trim();
    if (!text) continue;
    try {
      const r = JSON.parse(text);
      if (typeof r?.sig === 'string') done.add(r.sig);
    } catch {}
  }
  return done;
}

/** `coins-abc-20260827-0500.jsonl` -> `costs-abc-20260827-0500.jsonl`, beside it. */
export function costsFileFor(file) {
  const dir = path.dirname(file);
  const base = path.basename(file);
  return path.join(dir, base.replace(/^[a-z]+-/, 'costs-'));
}

/**
 * Resolve every signature in `files` and write what each transaction cost.
 *
 * @param {object} o
 * @param {string[]} o.files coin files to read signatures out of.
 * @param {object} o.rpc an `Rpc` — anything with `enabled` and `batched(calls)`.
 * @param {number} [o.limit] stop after this many lookups, for a first taste of a big file.
 * @param {string} [o.out] write everything to this one file instead of one beside each input.
 */
export async function enrich({ files, rpc, limit = Infinity, out = null, onStatus = () => {} }) {
  if (!rpc?.enabled) throw new Error('no RPC endpoint — set STS_RPC. This pass is nothing but network calls.');
  const totals = { asked: 0, resolved: 0, missing: 0, skipped: 0, pending: 0 };
  let budget = limit;

  for (const file of files) {
    const target = out ?? costsFileFor(file);
    if (path.resolve(target) === path.resolve(file)) {
      throw new Error(`refusing to write costs over the capture itself (${file})`);
    }
    const wanted = await signaturesIn(file);
    const done = await alreadyDone(target);
    const todo = wanted.filter((w) => !done.has(w.sig));
    totals.skipped += wanted.length - todo.length;
    onStatus(`${path.basename(file)}: ${wanted.length} signatures, ${todo.length} still to resolve`);
    if (!todo.length) continue;

    const appender = new Appender(target);
    let asked = 0;
    try {
      for (let i = 0; i < todo.length && budget > 0; i += rpc.batchSize ?? 20) {
        const slice = todo.slice(i, i + Math.min(rpc.batchSize ?? 20, budget));
        budget -= slice.length;
        asked += slice.length;
        totals.asked += slice.length;
        const results = await rpc.batched(
          slice.map((w) => ({
            method: 'getTransaction',
            params: [w.sig, { encoding: 'json', maxSupportedTransactionVersion: 0, commitment: 'confirmed' }],
          })),
        );
        for (let j = 0; j < slice.length; j++) {
          const w = slice[j];
          const cost = costOf(results[j]);
          if (!cost) {
            // Written down as looked-up-and-absent rather than left out, so the
            // next run terminates instead of retrying a pruned signature for
            // ever. `rpc.batched` returns null for a dead endpoint too, which is
            // why `missing` is reported separately from `resolved`.
            appender.write({ v: SCHEMA, sig: w.sig, why: w.why, mint: w.mint, wallet: w.wallet, missing: true });
            totals.missing++;
            continue;
          }
          appender.write({ v: SCHEMA, sig: w.sig, why: w.why, mint: w.mint, wallet: w.wallet, missing: false, ...cost });
          totals.resolved++;
        }
        if (totals.resolved && totals.resolved % 500 === 0) onStatus(`  ${totals.resolved} resolved`);
      }
      // Whatever the limit stopped us reaching. A pass that ran out of budget
      // and a pass that finished must not read the same, or the next run has no
      // way to know whether there is anything left to do.
      totals.pending += Math.max(0, todo.length - asked);
    } finally {
      await appender.close();
    }
  }
  return totals;
}
