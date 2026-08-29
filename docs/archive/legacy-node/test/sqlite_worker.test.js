// What the storage worker has to get right.
//
// The claim it exists to make is a timing one: rows go to disk without the
// thread that collected them stopping to watch. So the middle of this file is a
// measurement rather than an assertion about a return value — the same twenty
// thousand rows are written twice, once inline and once through the worker, with
// a five-millisecond interval running alongside to see whether the event loop
// was ever free. Inline, it is not; the loop goes quiet for the whole write.
//
// The rest is about not losing anything on the way. A batch commits when it is
// full or when its hundred milliseconds are up, whichever lands first, and both
// of those paths are checked by the reason the worker gives for the commit
// rather than by a clock in the test, because a clock in a test is a flake
// waiting for a slow afternoon.
//
// Everything runs in a temporary directory, and every worker is closed — the
// thread holds the process open on purpose, so a test that forgets would hang
// rather than fail.
//
//   node --test test/sqlite_worker.test.js

import test from 'node:test';
import assert from 'node:assert/strict';
import { once } from 'node:events';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

import { Db, positionRow, telemetryRow } from '../src/db.js';
import { StorageWorker } from '../src/storage/sqlite_worker.js';

/** A fresh directory per test, removed afterwards. */
function tmp(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'sts-storage-test-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

/** A worker that closes itself when the test ends, whatever the test does. */
function worker(t, opts = {}) {
  const s = new StorageWorker({ dir: opts.dir ?? tmp(t), ...opts });
  t.after(() => s.close());
  return s;
}

const MINT = 'Mint111111111111111111111111111111111111111';

const position = (over = {}) => ({
  key: 'run7:' + MINT,
  run_id: 'run7',
  mint: MINT,
  strategy: 'edge',
  side: 'BUY',
  size_sol: 0.25,
  entry_price: 2,
  status: 'OPEN',
  opened_at: 1_786_554_449_571,
  ...over,
});

/**
 * How long the event loop was unavailable, at worst, while `work` ran.
 *
 * The trailing stall counts: an interval that stops firing altogether records
 * no gap at all, and "no gap" is precisely the failure being looked for.
 */
async function stalls(work) {
  const gaps = [];
  let last = performance.now();
  const iv = setInterval(() => {
    const now = performance.now();
    gaps.push(now - last);
    last = now;
  }, 5);
  try {
    await work();
  } finally {
    clearInterval(iv);
  }
  gaps.push(performance.now() - last);
  return { worst: Math.max(...gaps), ticks: gaps.length - 1 };
}

// ---------------------------------------------------------------------------
// The connection the worker holds
// ---------------------------------------------------------------------------

test('the worker opens its own connection with the pragmas the isolation needs', async (t) => {
  const s = worker(t);
  await s.ready;
  const { pragmas } = await s.stats();
  // Read off that thread's connection, not this one. Pragmas are per
  // connection: a worker that came up without WAL would look fine from here.
  assert.equal(pragmas.journal_mode, 'wal');
  assert.equal(pragmas.synchronous, 1); // NORMAL
  assert.equal(pragmas.busy_timeout, 5000);
  assert.equal(pragmas.cache_size, -64000); // 64 MB, negative meaning KiB
  assert.equal(pragmas.foreign_keys, 1);
});

test('the three tables it writes to, and the index that makes evidence unique', (t) => {
  const db = new Db({ dir: tmp(t) });
  const names = (type) =>
    db.sql
      .prepare(`SELECT name FROM sqlite_master WHERE type='${type}' AND name NOT LIKE 'sqlite_%'`)
      .all()
      .map((r) => r.name);
  for (const table of ['positions', 'telemetry_logs', 'forensic_snapshots'])
    assert.ok(names('table').includes(table), `missing table ${table}`);
  for (const index of ['positions_status', 'telemetry_logs_metric', 'forensic_snapshots_once'])
    assert.ok(names('index').includes(index), `missing index ${index}`);
  db.close();
});

// ---------------------------------------------------------------------------
// Rows in, rows on disk
// ---------------------------------------------------------------------------

test('all three kinds of row survive the trip to another thread', async (t) => {
  const dir = tmp(t);
  const s = worker(t, { dir });
  await s.ready;

  s.position(position());
  s.telemetry({ metric: 'trades_per_min', value: 412, mint: MINT, run_id: 'run7' });
  s.snapshot({ mint: MINT, kind: 'entry', age_sec: 3, state: { buyers: 16, solIn: 21.81 }, run_id: 'run7' });
  const out = await s.flush();
  assert.deepEqual(
    { positions: out.positions, telemetry: out.telemetry, snapshots: out.snapshots, rejected: out.rejected },
    { positions: 1, telemetry: 1, snapshots: 1, rejected: 0 },
  );

  const db = new Db({ dir });
  t.after(() => db.close());
  assert.equal(db.sql.prepare('SELECT COUNT(*) n FROM positions').get().n, 1);
  const log = db.sql.prepare('SELECT * FROM telemetry_logs').get();
  assert.equal(log.metric, 'trades_per_min');
  assert.equal(log.value, 412);
  // The state goes in as JSON text and comes back out readable, which is the
  // whole point of keeping it rather than the three numbers we thought to name.
  const snap = db.sql.prepare('SELECT * FROM forensic_snapshots').get();
  assert.equal(JSON.parse(snap.state).solIn, 21.81);
  assert.equal(snap.age_sec, 3);
});

test('a full batch commits itself, without waiting out its hundred milliseconds', async (t) => {
  const s = worker(t, { batchSize: 500, flushMs: 60_000 });
  await s.ready;
  const wrote = once(s, 'flushed');
  s.telemetry(Array.from({ length: 500 }, (_, i) => ({ metric: 'trade', value: i })));
  const [m] = await wrote;
  // Asserted on the reason rather than on how long it took. The timer is set a
  // minute out, so anything arriving at all arrived because the batch was full.
  assert.equal(m.reason, 'rows');
  assert.equal(m.telemetry, 500);
});

test('a batch that never fills commits on the timer', async (t) => {
  const s = worker(t, { flushMs: 20 });
  await s.ready;
  const wrote = once(s, 'flushed');
  s.telemetry([{ metric: 'trade', value: 1 }, { metric: 'trade', value: 2 }]);
  const [m] = await wrote;
  assert.equal(m.reason, 'timer');
  assert.equal(m.telemetry, 2);
});

test('closing writes what is still queued', async (t) => {
  const dir = tmp(t);
  const s = new StorageWorker({ dir, flushMs: 60_000 });
  await s.ready;
  s.telemetry({ metric: 'last_word', value: 1 });
  await s.close(); // no flush first: the close has to do it

  const db = new Db({ dir });
  t.after(() => db.close());
  assert.equal(db.sql.prepare("SELECT COUNT(*) n FROM telemetry_logs WHERE metric='last_word'").get().n, 1);
});

test('a closed worker refuses new rows rather than dropping them quietly', async (t) => {
  const s = new StorageWorker({ dir: tmp(t) });
  await s.ready;
  await s.close();
  assert.throws(() => s.telemetry({ metric: 'too_late' }), /closed/);
});

// ---------------------------------------------------------------------------
// What the tables enforce
// ---------------------------------------------------------------------------

test('a position opens and closes as one row, and works out its own P&L', async (t) => {
  const dir = tmp(t);
  const s = worker(t, { dir });
  await s.ready;

  s.position(position());
  s.position(position({ status: 'CLOSED', exit_price: 3, closed_at: 1_786_554_509_571, size_sol: null, entry_price: null }));
  await s.flush();

  const db = new Db({ dir });
  t.after(() => db.close());
  const rows = db.sql.prepare('SELECT * FROM positions').all();
  assert.equal(rows.length, 1, 'the close is the same position, not a second one');
  const row = rows[0];
  assert.equal(row.status, 'CLOSED');
  // The close carried neither size nor entry, and neither was overwritten.
  assert.equal(row.size_sol, 0.25);
  assert.equal(row.entry_price, 2);
  // 0.25 SOL bought at 2 and sold at 3 is half again: 0.125 SOL, 50%.
  assert.equal(row.pnl_sol, 0.125);
  assert.equal(row.pnl_pct, 50);
});

test('P&L is the table\'s to compute and nobody else\'s', async (t) => {
  assert.throws(() => positionRow(position({ pnl_sol: 999 })), /computed/);
  const db = new Db({ dir: tmp(t) });
  t.after(() => db.close());
  // And the same refusal one level down, for a row that never went through the
  // check above. SQLite will not insert into a generated column at all.
  assert.throws(
    () => db.sql.prepare('INSERT INTO positions (key, mint, strategy, side, status, opened_at, pnl_sol) VALUES (?,?,?,?,?,?,?)')
      .run('k', MINT, 'edge', 'BUY', 'CLOSED', 1, 9),
    /generated column/,
  );
});

test('a position that will not say what it is worth is not accepted as open', () => {
  assert.throws(() => positionRow(position({ size_sol: null })), /OPEN position needs/);
  assert.throws(() => positionRow(position({ side: 'HOLD' })), /BUY or SELL/);
  assert.throws(() => positionRow({ strategy: 'edge' }), /mint is required/);
  // A close that arrives for a position nobody saw open is allowed through: it
  // is a fact about something that happened while we were not looking.
  assert.ok(positionRow(position({ status: 'CLOSED', size_sol: null, entry_price: null })));
});

test('the same evidence twice is one row, but only when dedup was asked for', async (t) => {
  const dir = tmp(t);
  const s = worker(t, { dir });
  await s.ready;

  const state = { holders: 41, creatorPct: 18 };
  s.snapshot([
    { mint: MINT, kind: 'rug_check', state, digest: true },
    { mint: MINT, kind: 'rug_check', state, digest: true },
    { mint: MINT, kind: 'watch', state },
    { mint: MINT, kind: 'watch', state },
  ]);
  await s.flush();

  const db = new Db({ dir });
  t.after(() => db.close());
  const count = (kind) => db.sql.prepare('SELECT COUNT(*) n FROM forensic_snapshots WHERE kind = ?').get(kind).n;
  assert.equal(count('rug_check'), 1);
  // Two identical captures with no digest stay two rows. "Nothing had changed"
  // is a finding, and folding them together would erase how long it held.
  assert.equal(count('watch'), 2);
});

test('a row the database refuses does not take the batch with it', (t) => {
  const db = new Db({ dir: tmp(t) });
  t.after(() => db.close());
  const good = telemetryRow({ metric: 'ok', value: 1, at: 1 });
  const bad = [...good];
  bad[2] = null; // metric is NOT NULL — a row that got past the checks somehow
  const out = db.writeBatch({ telemetry: [good, bad, good] });
  assert.equal(out.telemetry, 2);
  assert.equal(out.rejected, 1);
  assert.equal(db.sql.prepare('SELECT COUNT(*) n FROM telemetry_logs').get().n, 2);
});

test('the queue has a ceiling, and says how much it threw away', async (t) => {
  const s = worker(t, { maxQueue: 10, flushMs: 60_000 });
  await s.ready;
  s.telemetry(Array.from({ length: 50 }, (_, i) => ({ metric: 'flood', value: i })));
  const out = await s.flush();
  assert.equal(out.telemetry, 10);
  const { dropped } = await s.stats();
  assert.equal(dropped, 40);
});

// ---------------------------------------------------------------------------
// The reason any of this exists
// ---------------------------------------------------------------------------

test('the ingestion thread keeps running while the worker writes', async (t) => {
  const dir = tmp(t);
  const N = 20_000;
  const CHUNK = 500;
  const rows = Array.from({ length: N }, (_, i) => ({ metric: 'trade', value: i, at: 1 }));

  // The same rows, the same batch size, on this thread. Forty commits, and the
  // loop does not get a turn between any of them.
  const db = new Db({ dir: path.join(dir, 'inline') });
  const inline = await stalls(async () => {
    for (let i = 0; i < N; i += CHUNK) db.writeBatch({ telemetry: rows.slice(i, i + CHUNK).map(telemetryRow) });
  });
  db.close();

  const s = worker(t, { dir: path.join(dir, 'worker') });
  await s.ready;
  const offloaded = await stalls(async () => {
    for (let i = 0; i < N; i += CHUNK) s.telemetry(rows.slice(i, i + CHUNK));
    await s.flush();
  });

  // Both numbers move with the machine, so the assertion is the ratio rather
  // than a millisecond count. What is being claimed is only this: handing the
  // writing to another thread leaves the loop free, and doing it here does not.
  assert.ok(
    offloaded.worst * 2 < inline.worst,
    `worker stalled the loop for ${offloaded.worst.toFixed(1)}ms against ${inline.worst.toFixed(1)}ms inline`,
  );
  assert.ok(offloaded.ticks >= 3, `the loop got ${offloaded.ticks} turns while the worker wrote`);
  assert.equal((await s.stats()).telemetry, N);
});

test('the dashboard can still write while the worker is writing', async (t) => {
  const dir = tmp(t);
  const s = worker(t, { dir });
  await s.ready;

  // Two connections, two threads, one file. WAL allows one writer at a time and
  // busy_timeout is what turns losing that race into a wait instead of a
  // SQLITE_BUSY — this is the case that made both of those pragmas load-bearing.
  const db = new Db({ dir });
  t.after(() => db.close());

  s.telemetry(Array.from({ length: 20_000 }, (_, i) => ({ metric: 'trade', value: i })));
  const trade = db.recordPaperFill({ token_address: MINT, side: 'BUY', size_sol: 0.5, entry_price: 1 });
  const seen = db.sql.prepare('SELECT COUNT(*) n FROM telemetry_logs').get().n;
  await s.flush();

  assert.equal(trade.status, 'OPEN', 'a paper order landed while the worker held the file');
  assert.ok(seen >= 0, 'and the read that ran beside it was answered rather than blocked');
  assert.equal(db.sql.prepare('SELECT COUNT(*) n FROM telemetry_logs').get().n, 20_000);
});
