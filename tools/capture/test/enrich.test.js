// What a transaction cost to land.
//
// Nothing here opens a socket or makes a request. `enrich()` takes an object
// with `enabled` and `batched()`, so the whole pass runs against a fake that
// hands back canned `getTransaction` results — which is also the only way to
// test the shapes that matter, because a pruned signature and a dead endpoint
// are exactly the cases a live run cannot be asked to produce on demand.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { encode as b58encode } from '../src/base58.js';
import {
  costOf, computeBudget, keysOf, jitoTip, signaturesIn, alreadyDone, costsFileFor, enrich,
  COMPUTE_BUDGET_PROGRAM, JITO_TIP_ACCOUNTS, LAMPORTS_PER_SIGNATURE,
} from '../src/enrich.js';
import { SCHEMA } from '../src/session.js';

const HERE = path.dirname(fileURLToPath(import.meta.url));

/** SetComputeUnitLimit, as the chain encodes it: one tag byte then a u32. */
const setLimit = (units) => {
  const b = Buffer.alloc(5);
  b[0] = 2;
  b.writeUInt32LE(units, 1);
  return b58encode(b);
};

/** SetComputeUnitPrice: tag 3 then a u64 of micro-lamports per compute unit. */
const setPrice = (microLamports) => {
  const b = Buffer.alloc(9);
  b[0] = 3;
  b.writeBigUInt64LE(BigInt(microLamports), 1);
  return b58encode(b);
};

const tx = ({
  keys = ['payer', COMPUTE_BUDGET_PROGRAM, 'pump'],
  instructions = [],
  fee = 5_000,
  signers = 1,
  cuUsed = 30_000,
  err = null,
  pre = null,
  post = null,
  loadedAddresses = null,
  slot = 42,
  blockTime = 1_700_000_000,
} = {}) => ({
  slot,
  blockTime,
  transaction: { message: { accountKeys: keys, header: { numRequiredSignatures: signers }, instructions } },
  meta: { fee, computeUnitsConsumed: cuUsed, err, preBalances: pre, postBalances: post, loadedAddresses },
});

// ---------------------------------------------------------------------------
// The cost block itself
// ---------------------------------------------------------------------------

test('the base fee and the priority fee are separated, because only one is a choice', () => {
  // 5,000 lamports a signature is the protocol's price and nobody bids it. The
  // rest is what a wallet chose to pay to be earlier in the block, and it is the
  // number the whole go/no-go question turns on — the last cost model was
  // rebuilt from 25 transactions weeks later and was wrong by twenty times.
  const c = costOf(tx({ fee: 105_000, signers: 1 }));
  assert.equal(c.feeBase, LAMPORTS_PER_SIGNATURE);
  assert.equal(c.feePriority, 100_000);
  assert.equal(c.feeTotal, 105_000);
  assert.equal(c.feeTotalSol, 0.000105);
});

test('the network fee is not called feeSol, because the coin record already has one', () => {
  // `outcome.feeSol` on a coin row is the pump *trading* fee its traders paid
  // over the follow window. This row is the Solana network fee for one
  // transaction. The two files are joined on `sig`, so one name across the join
  // would give a reader two different numbers under one label — the same
  // mistake that had six reports disagreeing about what `entry` meant.
  const c = costOf(tx({ fee: 105_000, signers: 1 }));
  assert.ok(!('feeSol' in c), 'the costs row must not carry a field called feeSol');
  assert.equal(c.feeTotalSol, c.feeTotal / 1e9);
});

test('a two-signature transaction pays two base fees', () => {
  assert.equal(costOf(tx({ fee: 10_000, signers: 2 })).feeBase, 10_000);
  assert.equal(costOf(tx({ fee: 10_000, signers: 2 })).feePriority, 0);
});

test('the compute budget is read off the instructions it was set with', () => {
  const c = costOf(tx({
    fee: 55_000,
    instructions: [
      { programIdIndex: 1, accounts: [], data: setLimit(120_000) },
      { programIdIndex: 1, accounts: [], data: setPrice(400_000) },
      { programIdIndex: 2, accounts: [], data: b58encode(Buffer.from([9, 9, 9])) },
    ],
  }));
  assert.equal(c.cuLimit, 120_000);
  assert.equal(c.cuPrice, 400_000);
  assert.equal(c.cuUsed, 30_000);
});

test('a transaction that set no budget says so, rather than guessing a default', () => {
  const c = costOf(tx({}));
  assert.equal(c.cuLimit, null, 'null means "did not ask", not "could not tell"');
  assert.equal(c.cuPrice, null);
  assert.equal(c.feePriority, 0);
});

test('the charged priority fee is what left the wallet, not what was reserved', () => {
  // 120,000 units at 400,000 micro-lamports is 48,000 lamports *asked for*. The
  // transaction used 30,000 units, so the chain charged less. Both are on the
  // row and only one comes out of the bankroll.
  const c = costOf(tx({
    fee: 17_000,
    instructions: [
      { programIdIndex: 1, accounts: [], data: setLimit(120_000) },
      { programIdIndex: 1, accounts: [], data: setPrice(400_000) },
    ],
  }));
  assert.equal(c.feePriority, 12_000, 'total minus base, not cuLimit times cuPrice');
  assert.equal(c.cuLimit * c.cuPrice / 1e6, 48_000, 'and what it reserved is on the row too');
});

test('a transaction that failed still landed and still paid', () => {
  // 93.2% of pump transactions on the wire fail. Dropping them is how the
  // failure cost stayed invisible for a fortnight.
  const c = costOf(tx({ fee: 105_000, err: { InstructionError: [3, { Custom: 6002 }] } }));
  assert.equal(c.err, true);
  assert.equal(c.feeTotal, 105_000);
});

test('a signature the chain no longer has reads as nothing, not as free', () => {
  assert.equal(costOf(null), null);
});

test('balances are indexed across the lookup tables as well as the message keys', () => {
  // A versioned transaction keeps some addresses outside the message, and
  // pre/postBalances run across both. A reader that stops at accountKeys lines
  // the wrong balance up against the wrong address on any modern transaction.
  const t = tx({ keys: ['payer', 'a'], loadedAddresses: { writable: ['w'], readonly: ['r'] } });
  assert.deepEqual(keysOf(t), ['payer', 'a', 'w', 'r']);
});

test('a Jito tip is found by where the money went, not by which instruction sent it', () => {
  const t = tx({
    keys: ['payer', JITO_TIP_ACCOUNTS[3]],
    pre: [1_000_000, 0],
    post: [900_000, 50_000],
  });
  assert.equal(jitoTip(t), 50_000);
  assert.equal(costOf(t).jitoTip, 50_000);
});

test('no tip account touched means no tip, not a tip of zero', () => {
  assert.equal(jitoTip(tx({ keys: ['payer', 'someone'], pre: [10, 0], post: [5, 5] })), null);
});

test('the tip accounts are the same eight the executor would pay', () => {
  // One source of truth, in Rust, where a tip would actually be sent from. This
  // copy exists so an offline pass can recognise one; the assertion is what
  // stops the copy drifting.
  const rust = fs.readFileSync(path.join(HERE, '..', '..', '..', 'src-tauri', 'src', 'execution.rs'), 'utf8');
  const block = rust.match(/pub const JITO_TIP_ACCOUNTS: \[&str; 8\] = \[([\s\S]*?)\];/);
  assert.ok(block, 'execution.rs no longer declares JITO_TIP_ACCOUNTS the way this test reads it');
  const listed = [...block[1].matchAll(/"([1-9A-HJ-NP-Za-km-z]{32,44})"/g)].map((m) => m[1]);
  assert.deepEqual(listed, JITO_TIP_ACCOUNTS);
});

test('an unreadable compute-budget payload costs the other instructions nothing', () => {
  const { cuLimit, cuPrice } = computeBudget(tx({
    instructions: [
      { programIdIndex: 1, accounts: [], data: '!!! not base58 !!!' },
      { programIdIndex: 1, accounts: [], data: setPrice(7) },
    ],
  }));
  assert.equal(cuPrice, 7);
  assert.equal(cuLimit, null);
});

// ---------------------------------------------------------------------------
// Which signatures to look up
// ---------------------------------------------------------------------------

const withDir = async (fn) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'capture-enrich-'));
  try {
    return await fn(dir);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
};

const coinFile = (dir, rows) => {
  const file = path.join(dir, 'coins-abc-20260827-0500.jsonl');
  fs.writeFileSync(file, rows.map((r) => JSON.stringify(r)).join('\n') + '\n');
  return file;
};

test('every signature a coin file names is collected, and each says what it was', async () => {
  await withDir(async (dir) => {
    const file = coinFile(dir, [
      { k: 'start', sid: 'abc', sig: 'not-a-coin' },
      { mint: 'M1', creator: 'C1', sig: 'launch-1', who: [{ w: 'W1', sig: 'buy-1' }, { w: 'W2' }] },
      { mint: 'M2', creator: 'C2', sig: 'launch-2', who: [{ w: 'W3', sig: 'buy-1' }] },
    ]);
    const found = await signaturesIn(file);
    assert.deepEqual(found.map((f) => f.sig), ['launch-1', 'buy-1', 'launch-2']);
    assert.equal(found[0].why, 'launch');
    assert.equal(found[1].why, 'opening-buy');
    assert.equal(found[1].wallet, 'W1', 'the wallet that paid it, so the ladder can be read per wallet');
    assert.equal(found.some((f) => f.sig === 'not-a-coin'), false, 'a session row is not a coin');
  });
});

test('the costs go beside the capture under their own name, never over it', async () => {
  assert.equal(costsFileFor('/d/coins-abc-20260827-0500.jsonl'), '/d/costs-abc-20260827-0500.jsonl');
  assert.equal(costsFileFor('/d/fails-abc.jsonl'), '/d/costs-abc.jsonl');
});

// ---------------------------------------------------------------------------
// The pass
// ---------------------------------------------------------------------------

/** An RPC that answers from a table and counts what it was asked. */
const fakeRpc = (table) => ({
  enabled: true,
  batchSize: 2,
  asked: [],
  async batched(calls) {
    const sigs = calls.map((c) => c.params[0]);
    this.asked.push(...sigs);
    return sigs.map((s) => table[s] ?? null);
  },
});

const read = (file) => fs.readFileSync(file, 'utf8').trim().split('\n').map((l) => JSON.parse(l));

test('every recorded signature comes back with what it cost', async () => {
  await withDir(async (dir) => {
    const file = coinFile(dir, [
      { mint: 'M1', creator: 'C1', sig: 'launch-1', who: [{ w: 'W1', sig: 'buy-1' }] },
    ]);
    const rpc = fakeRpc({
      'launch-1': tx({ fee: 105_000, instructions: [{ programIdIndex: 1, accounts: [], data: setPrice(500) }] }),
      'buy-1': tx({ fee: 15_000 }),
    });
    const totals = await enrich({ files: [file], rpc });

    assert.deepEqual(totals, { asked: 2, resolved: 2, missing: 0, skipped: 0, pending: 0 });
    const rows = read(costsFileFor(file));
    assert.equal(rows.length, 2);
    assert.equal(rows[0].feePriority, 100_000);
    assert.equal(rows[0].cuPrice, 500);
    assert.equal(rows[0].why, 'launch');
    assert.equal(rows[1].why, 'opening-buy');
    assert.equal(rows[1].wallet, 'W1');
    // And the capture itself is untouched — the recorded rows are irreplaceable
    // and no later pass gets to rewrite them.
    assert.equal(read(file).length, 1);
  });
});

test('a second run asks for nothing, so an interrupted pass costs nothing', async () => {
  await withDir(async (dir) => {
    const file = coinFile(dir, [{ mint: 'M1', sig: 'launch-1', who: [] }]);
    const table = { 'launch-1': tx({}) };
    await enrich({ files: [file], rpc: fakeRpc(table) });
    const again = fakeRpc(table);
    const totals = await enrich({ files: [file], rpc: again });
    assert.deepEqual(again.asked, [], 'a signature is resolved once and never again');
    assert.equal(totals.skipped, 1);
    assert.equal(totals.asked, 0);
    assert.equal(read(costsFileFor(file)).length, 1, 'and it is not written twice');
  });
});

test('a signature the chain has pruned is written down as gone, not retried for ever', async () => {
  await withDir(async (dir) => {
    const file = coinFile(dir, [{ mint: 'M1', sig: 'vanished', who: [] }]);
    const totals = await enrich({ files: [file], rpc: fakeRpc({}) });
    assert.equal(totals.missing, 1);
    assert.equal(read(costsFileFor(file))[0].missing, true);
    assert.deepEqual(await [...(await alreadyDone(costsFileFor(file)))], ['vanished'], 'so the queue terminates');
  });
});

test('--limit stops after N lookups and says how many are left', async () => {
  await withDir(async (dir) => {
    const file = coinFile(dir, [
      { mint: 'M1', sig: 's1', who: [{ w: 'W', sig: 's2' }] },
      { mint: 'M2', sig: 's3', who: [] },
    ]);
    const rpc = fakeRpc({ s1: tx({}), s2: tx({}), s3: tx({}) });
    const totals = await enrich({ files: [file], rpc, limit: 2 });
    assert.equal(totals.resolved, 2);
    assert.equal(totals.pending, 1, 'a pass that ran out of budget must not read like a finished one');
  });
});

test('with no endpoint it refuses rather than half-running against nothing', async () => {
  await assert.rejects(
    () => enrich({ files: ['/nowhere'], rpc: { enabled: false } }),
    /no RPC endpoint/,
  );
});

test('it refuses to write its output over the capture it is reading', async () => {
  await withDir(async (dir) => {
    const file = coinFile(dir, [{ mint: 'M1', sig: 's1', who: [] }]);
    await assert.rejects(
      () => enrich({ files: [file], rpc: fakeRpc({}), out: file }),
      /refusing to write costs over the capture/,
    );
  });
});

test('a costs row says which shape it is, like every other record type', async () => {
  // costs-<session>.jsonl is a file of its own with no header in it, and it is
  // joined to the coin records on `sig`. A join between two files where only one
  // of them can say what shape it was written at is a join nobody can date.
  await withDir(async (dir) => {
    const file = coinFile(dir, [{ mint: 'M1', creator: 'C1', sig: 'launch-1', who: [] }]);
    await enrich({ files: [file], rpc: fakeRpc({ 'launch-1': tx({ fee: 25_000 }) }) });
    const row = read(costsFileFor(file))[0];
    assert.equal(row.missing, false);
    assert.equal(row.v, SCHEMA);
  });
  // Including the row for a signature the chain has pruned: "looked up and
  // gone" is a record too, and it has a shape. Its own directory, because the
  // costs file is named after the capture and would otherwise be the same one.
  await withDir(async (dir) => {
    const gone = coinFile(dir, [{ mint: 'M2', creator: 'C2', sig: 'pruned-1', who: [] }]);
    await enrich({ files: [gone], rpc: fakeRpc({}) });
    const row = read(costsFileFor(gone))[0];
    assert.equal(row.missing, true);
    assert.equal(row.v, SCHEMA);
  });
});
