// What the storage layer has to get right.
//
// Three things, and the third is the one worth the most care.
//
// Init: the pragmas are not decoration. WAL is what lets the dashboard read
// while the watcher writes, and a database that quietly came up without it
// would only show itself as a lock timeout under load, hours later.
//
// Idempotency: the same mint really does arrive twice — a restart starts the
// in-memory dedup set over, and the recorded files already contain 116 such
// pairs. Re-running the backfill has to be boring.
//
// Dual-write: the JSONL file is the archive and the database is the copy. So
// every test here that writes checks *both*, and the failure tests check that
// a broken database still leaves the line on disk. If that stops being true
// the whole "we can always reprocess from raw" claim goes with it.
//
// Everything runs in a temporary directory. The tests that use the real corpus
// skip rather than fail when data/ is not there, since it is not in git.
//
// Run with: node --test test/

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { Db, dataDir, tokenRow } from '../src/db.js';
import { Records } from '../src/record.js';
import { AuditLogger } from '../src/audit.js';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const DATA = path.join(ROOT, 'data');
const today = () => new Date().toISOString().slice(0, 10);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** A fresh directory per test, removed afterwards. */
function tmp(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'sts-db-test-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

// ---------------------------------------------------------------------------
// Fixtures, in the shape watch.js writes
// ---------------------------------------------------------------------------

const CREATOR = 'CreatorWa11etAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA';
const OTHER = 'OtherWa11etBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB';

const coin = (over = {}) => ({
  t: 1786554449571,
  mint: 'Mint111111111111111111111111111111111111111',
  symbol: 'TEST',
  name: 'A Test Coin',
  creator: CREATOR,
  uri: 'https://example.invalid/meta.json',
  supply: 1_000_000_000,
  initialBuySol: 1.5,
  social: { kind: 'tweet', handle: 'someone', followers: 12 },
  open: { seconds: 3, wallets: 2, sellers: 0, solIn: 3.5, solOut: 0, trades: 4 },
  who: [
    { w: CREATOR, in: 1.5, out: 0, n: 1, at: 0.01 },
    { w: OTHER, in: 2, out: 3, n: 3, at: 1.5 },
  ],
  total: { wallets: 2, sellers: 1, solIn: 3.5, solOut: 3, trades: 4 },
  outcome: { follow: 60, entry: 0.000034, peak: 0.00005, last: 0.00004, peakMult: 1.47, highs: [[1.2, 1.1]], lows: [] },
  market: { candleSeconds: 1, candles: [{ s: 0, o: 1, h: 2, l: 1, c: 2, volume: 3, buys: 2, sells: 0 }] },
  ...over,
});

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------

test('the pragmas that make concurrent reads safe are actually set', (t) => {
  const db = new Db({ dir: tmp(t) });
  const get = (p) => db.sql.prepare(`PRAGMA ${p}`).get();
  assert.equal(get('journal_mode').journal_mode, 'wal');
  assert.equal(get('foreign_keys').foreign_keys, 1);
  assert.equal(get('synchronous').synchronous, 1); // NORMAL
  // 64 MB of page cache, negative because SQLite reads a negative number as
  // KiB and a positive one as a count of pages. The default is -2000.
  assert.equal(get('cache_size').cache_size, -64000);
  db.close();
});

test('every table and index the rest of the code assumes exists', (t) => {
  const db = new Db({ dir: tmp(t) });
  const names = (type) =>
    db.sql
      .prepare(`SELECT name FROM sqlite_master WHERE type='${type}' AND name NOT LIKE 'sqlite_%'`)
      .all()
      .map((r) => r.name);
  for (const table of ['tokens', 'wallets', 'audit_log']) assert.ok(names('table').includes(table), `missing table ${table}`);
  for (const index of ['tokens_created_at', 'tokens_symbol', 'wallets_total_trades', 'audit_log_created_at', 'audit_log_event_type'])
    assert.ok(names('index').includes(index), `missing index ${index}`);
  db.close();
});

test('there is no trades table, on purpose', (t) => {
  // Per-trade capture does not exist yet. An empty table would invite someone
  // to read "no trades" out of it, which is a different claim from "not
  // recorded". This test is here so removing it has to be deliberate.
  const db = new Db({ dir: tmp(t) });
  const found = db.sql.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='trades'").get();
  assert.equal(found, undefined);
  db.close();
});

test('the database lands where $STS_HOME says, and is called sts.db', (t) => {
  const dir = tmp(t);
  const before = process.env.STS_HOME;
  process.env.STS_HOME = dir;
  t.after(() => { if (before === undefined) delete process.env.STS_HOME; else process.env.STS_HOME = before; });

  assert.equal(dataDir(), dir);
  const db = new Db();
  assert.equal(db.file, path.join(dir, 'sts.db'));
  assert.ok(fs.existsSync(db.file));
  db.close();
});

test('opening a database that already exists keeps what was in it', (t) => {
  const dir = tmp(t);
  const first = new Db({ dir });
  first.insertTokens([coin()]);
  first.close();

  const second = new Db({ dir });
  assert.equal(second.count(), 1);
  second.close();
});

// ---------------------------------------------------------------------------
// Idempotency
// ---------------------------------------------------------------------------

test('the same mint twice is one row, and the first one wins', (t) => {
  const db = new Db({ dir: tmp(t) });
  assert.equal(db.insertTokens([coin({ symbol: 'FIRST' })]), 1);
  assert.equal(db.insertTokens([coin({ symbol: 'SECOND' })]), 0);
  assert.equal(db.count(), 1);
  // Not overwritten: a re-observation must not rewrite what we saw first.
  assert.equal(db.sql.prepare('SELECT symbol FROM tokens').get().symbol, 'FIRST');
  db.close();
});

test('a batch carrying its own duplicates still lands once', (t) => {
  const db = new Db({ dir: tmp(t) });
  assert.equal(db.insertTokens([coin(), coin(), coin()]), 1);
  assert.equal(db.count(), 1);
  db.close();
});

test('a record with no mint is skipped rather than stored under null', (t) => {
  const db = new Db({ dir: tmp(t) });
  assert.equal(db.insertTokens([{ t: 1, symbol: 'NOPE' }]), 0);
  assert.equal(db.count(), 0);
  assert.equal(tokenRow({ symbol: 'NOPE' }), null);
  db.close();
});

test('re-running the whole backfill changes nothing', (t) => {
  const db = new Db({ dir: tmp(t) });
  const batch = [coin(), coin({ mint: 'Mint222' }), coin({ mint: 'Mint333' })];
  assert.equal(db.insertTokens(batch), 3);
  const snapshot = db.sql.prepare('SELECT mint,name,symbol,created_at,raw FROM tokens ORDER BY mint').all();

  assert.equal(db.insertTokens(batch), 0);
  assert.equal(db.insertTokens(batch), 0);
  assert.deepEqual(db.sql.prepare('SELECT mint,name,symbol,created_at,raw FROM tokens ORDER BY mint').all(), snapshot);
  db.close();
});

test('the wallet rollup is rebuilt, not accumulated', (t) => {
  const db = new Db({ dir: tmp(t) });
  db.insertTokens([coin()]);
  const first = db.rebuildWallets();
  const after = db.sql.prepare('SELECT address,total_trades FROM wallets ORDER BY address').all();
  // Running it again must give the same numbers, not doubled ones.
  assert.equal(db.rebuildWallets(), first);
  assert.deepEqual(db.sql.prepare('SELECT address,total_trades FROM wallets ORDER BY address').all(), after);
  db.close();
});

// ---------------------------------------------------------------------------
// What the derived columns are allowed to claim
// ---------------------------------------------------------------------------

test('market cap is entry price times supply, and null when either is missing', (t) => {
  const db = new Db({ dir: tmp(t) });
  db.insertTokens([
    coin(),
    coin({ mint: 'NoSupply', supply: null }),
    coin({ mint: 'NoEntry', outcome: { entry: null } }),
  ]);
  const cap = (m) => db.sql.prepare('SELECT market_cap FROM tokens WHERE mint=?').get(m).market_cap;
  assert.equal(cap('Mint111111111111111111111111111111111111111'), 34_000); // 0.000034 × 1e9
  assert.equal(cap('NoSupply'), null);
  assert.equal(cap('NoEntry'), null);
  db.close();
});

test('columns absent from older records are null rather than invented', (t) => {
  const db = new Db({ dir: tmp(t) });
  // A record from before uri/supply/initialBuySol were captured.
  const old = coin({ mint: 'OldStyle' });
  delete old.uri; delete old.supply; delete old.initialBuySol;
  db.insertTokens([old]);
  const row = db.sql.prepare('SELECT uri,initial_buy_sol,market_cap,name FROM tokens WHERE mint=?').get('OldStyle');
  assert.equal(row.uri, null);
  assert.equal(row.initial_buy_sol, null);
  assert.equal(row.market_cap, null);
  assert.equal(row.name, 'A Test Coin'); // what it does have is still there
  db.close();
});

test('the whole original record survives the round trip', (t) => {
  const db = new Db({ dir: tmp(t) });
  const rec = coin();
  db.insertTokens([rec]);
  const back = JSON.parse(db.sql.prepare('SELECT raw FROM tokens').get().raw);
  assert.deepEqual(back, rec);
  // Including the nested parts no column covers.
  assert.deepEqual(back.market.candles, rec.market.candles);
  assert.deepEqual(back.social, rec.social);
  assert.deepEqual(back.outcome.highs, rec.outcome.highs);
  db.close();
});

test('the wallet rollup reads who[] the way it means it', (t) => {
  const db = new Db({ dir: tmp(t) });
  db.insertTokens([coin()]);
  db.rebuildWallets();
  const w = (a) => db.sql.prepare('SELECT * FROM wallets WHERE address=?').get(a);

  const creator = w(CREATOR);
  assert.equal(creator.total_trades, 1);
  assert.deepEqual(JSON.parse(creator.flags), ['creator']);
  assert.equal(creator.win_rate, 0); // put in 1.5, took out nothing
  // The launch itself, not their first trade. A deployer was there at t by
  // definition, and first_seen only ever moves earlier, so the trade at +10ms
  // does not push it later.
  assert.equal(creator.first_seen, 1786554449571);

  const other = w(OTHER);
  assert.equal(other.total_trades, 3);
  assert.deepEqual(JSON.parse(other.flags), []);
  assert.equal(other.win_rate, 1); // out 3 > in 2
  // Everyone else is first seen when they first traded: t + at×1000.
  assert.equal(other.first_seen, 1786554449571 + 1500);
  db.close();
});

// ---------------------------------------------------------------------------
// Dual-write
// ---------------------------------------------------------------------------

test('a coin is written to the file and the database both', async (t) => {
  const dir = tmp(t);
  const db = new Db({ dir });
  const records = new Records({ dir, key: 'mint', db });

  assert.equal(records.write(coin()), true);
  await records.close();

  const lines = fs.readFileSync(path.join(dir, `coins-${today()}.jsonl`), 'utf8').split('\n').filter(Boolean);
  assert.equal(lines.length, 1);
  assert.deepEqual(JSON.parse(lines[0]), coin());
  assert.equal(db.count(), 1);
  assert.equal(records.written, 1);
  assert.equal(records.stored, 1);
  db.close();
});

test('the key dedup still works, and still keeps the second line out of both', async (t) => {
  const dir = tmp(t);
  const db = new Db({ dir });
  const records = new Records({ dir, key: 'mint', db });

  assert.equal(records.write(coin()), true);
  assert.equal(records.write(coin()), false); // same mint
  await records.close();

  const lines = fs.readFileSync(path.join(dir, `coins-${today()}.jsonl`), 'utf8').split('\n').filter(Boolean);
  assert.equal(lines.length, 1);
  assert.equal(db.count(), 1);
  db.close();
});

test('rows wait for a full batch, then go together', async (t) => {
  const dir = tmp(t);
  const db = new Db({ dir });
  const records = new Records({ dir, key: 'mint', db, batchSize: 3, flushMs: 60_000 });

  records.write(coin({ mint: 'a' }));
  records.write(coin({ mint: 'b' }));
  assert.equal(db.count(), 0, 'nothing committed before the batch is full');
  records.write(coin({ mint: 'c' }));
  assert.equal(db.count(), 3, 'the third row closes the batch');

  await records.close();
  db.close();
});

test('a part-full batch still goes, once the timer catches it', async (t) => {
  const dir = tmp(t);
  const db = new Db({ dir });
  const records = new Records({ dir, key: 'mint', db, batchSize: 1000, flushMs: 25 });

  records.write(coin());
  assert.equal(db.count(), 0);
  await sleep(80);
  assert.equal(db.count(), 1, 'the timer flushed it without a full batch');

  await records.close();
  db.close();
});

test('closing commits whatever was still waiting', async (t) => {
  const dir = tmp(t);
  const db = new Db({ dir });
  const records = new Records({ dir, key: 'mint', db, batchSize: 1000, flushMs: 60_000 });

  records.write(coin({ mint: 'x' }));
  records.write(coin({ mint: 'y' }));
  assert.equal(db.count(), 0);
  await records.close();
  assert.equal(db.count(), 2);
  db.close();
});

test('a database that throws costs the row, never the line', async (t) => {
  const dir = tmp(t);
  const broken = {
    dir,
    mints: () => [],
    insertTokens() { throw new Error('disk on fire'); },
  };
  const audit = new AuditLogger({ dir });
  const records = new Records({ dir, key: 'mint', db: broken, audit, batchSize: 1 });

  // The write reports success, because the archive did succeed.
  assert.equal(records.write(coin()), true);
  await records.close();
  await audit.close();

  const lines = fs.readFileSync(path.join(dir, `coins-${today()}.jsonl`), 'utf8').split('\n').filter(Boolean);
  assert.equal(lines.length, 1, 'the JSONL line survived the database failure');
  assert.deepEqual(JSON.parse(lines[0]), coin());

  // And the failure was recorded rather than swallowed.
  const events = fs.readFileSync(path.join(dir, `audit-${today()}.ndjson`), 'utf8')
    .split('\n').filter(Boolean).map((l) => JSON.parse(l));
  const failure = events.find((e) => e.action === 'db_write_failed');
  assert.ok(failure, 'expected a db_write_failed audit event');
  assert.equal(failure.level, 'error');
  assert.equal(failure.data.message, 'disk on fire');
});

test('without a database it is exactly the file writer it used to be', async (t) => {
  const dir = tmp(t);
  const records = new Records({ dir, key: 'mint' });
  assert.equal(records.write(coin()), true);
  assert.equal(records.write(coin()), false);
  await records.close();

  const lines = fs.readFileSync(path.join(dir, `coins-${today()}.jsonl`), 'utf8').split('\n').filter(Boolean);
  assert.equal(lines.length, 1);
  assert.equal(records.stored, 0);
});

test('a restart does not re-append coins the database already knows about', async (t) => {
  const dir = tmp(t);
  const db = new Db({ dir });
  const first = new Records({ dir, key: 'mint', db });
  first.write(coin());
  await first.close();

  // New process, same directory: the mint has to come back as already seen.
  const second = new Records({ dir, key: 'mint', db });
  assert.equal(second.write(coin()), false);
  await second.close();

  const lines = fs.readFileSync(path.join(dir, `coins-${today()}.jsonl`), 'utf8').split('\n').filter(Boolean);
  assert.equal(lines.length, 1);
  db.close();
});

test('audit events are mirrored into the database and still written to the file', async (t) => {
  const dir = tmp(t);
  const db = new Db({ dir });
  const audit = new AuditLogger({ dir, db });

  audit.emit('socket', 'connect', { url: 'wss://example.invalid/?api-key=***' });
  audit.emit('record', 'append', { name: 'coins', bytes: 12, mint: 'abc' });
  await audit.close();

  const rows = db.sql.prepare('SELECT event_type,payload,created_at FROM audit_log ORDER BY id').all();
  assert.equal(rows.length, 2);
  assert.equal(rows[0].event_type, 'socket');
  assert.equal(JSON.parse(rows[0].payload).action, 'connect');
  assert.ok(rows[0].created_at > 0);

  const lines = fs.readFileSync(path.join(dir, `audit-${today()}.ndjson`), 'utf8').split('\n').filter(Boolean);
  assert.equal(lines.length, 2, 'the NDJSON copy is still the primary record');
  assert.equal(audit.mirrored, 2);
  db.close();
});

test('a broken audit mirror does not stop the audit log', async (t) => {
  const dir = tmp(t);
  const audit = new AuditLogger({ dir, db: { insertAudit() { throw new Error('nope'); } } });
  assert.doesNotThrow(() => audit.emit('record', 'append', { name: 'coins' }));
  await audit.close();
  const lines = fs.readFileSync(path.join(dir, `audit-${today()}.ndjson`), 'utf8').split('\n').filter(Boolean);
  assert.equal(lines.length, 1);
  assert.equal(audit.mirrored, 0);
});

// ---------------------------------------------------------------------------
// Against the real recordings, when they are there
// ---------------------------------------------------------------------------

const corpus = () => {
  if (!fs.existsSync(DATA)) return [];
  const files = fs.readdirSync(DATA).filter((f) => /^coins-\d{4}-\d{2}-\d{2}\.jsonl$/.test(f)).sort();
  const out = [];
  for (const f of files)
    for (const line of fs.readFileSync(path.join(DATA, f), 'utf8').split('\n')) {
      if (!line.trim()) continue;
      try { const r = JSON.parse(line); if (r?.mint) out.push(r); } catch {}
    }
  return out;
};

test('every recorded coin stores and comes back unchanged', (t) => {
  const all = corpus();
  if (!all.length) return t.skip('no data/ corpus');

  const db = new Db({ dir: tmp(t) });
  db.insertTokens(all);

  const distinct = new Set(all.map((r) => r.mint));
  assert.equal(db.count(), distinct.size, 'one row per distinct mint');

  // The first sighting of each mint is what should be stored.
  const first = new Map();
  for (const r of all) if (!first.has(r.mint)) first.set(r.mint, r);
  for (const { raw } of db.sql.prepare('SELECT raw FROM tokens').all()) {
    const rec = JSON.parse(raw);
    assert.deepEqual(rec, first.get(rec.mint));
  }
  t.diagnostic(`${all.length} lines, ${distinct.size} distinct mints, ${all.length - distinct.size} re-observations collapsed`);
  db.close();
});

test('backfilling the real corpus twice adds nothing the second time', (t) => {
  const all = corpus();
  if (!all.length) return t.skip('no data/ corpus');

  const db = new Db({ dir: tmp(t) });
  const added = db.insertTokens(all);
  assert.equal(db.insertTokens(all), 0);
  assert.equal(db.count(), added);

  const wallets = db.rebuildWallets();
  assert.equal(db.rebuildWallets(), wallets, 'the rollup is stable across rebuilds');
  t.diagnostic(`${added} coins, ${wallets} wallets`);
  db.close();
});

test('no stored win rate escapes the range it is defined on', (t) => {
  const all = corpus();
  if (!all.length) return t.skip('no data/ corpus');

  const db = new Db({ dir: tmp(t) });
  db.insertTokens(all);
  db.rebuildWallets();
  for (const w of db.sql.prepare('SELECT address,win_rate,total_trades FROM wallets').all()) {
    if (w.win_rate !== null) assert.ok(w.win_rate >= 0 && w.win_rate <= 1, `${w.address} win_rate ${w.win_rate}`);
    assert.ok(w.total_trades >= 0, `${w.address} total_trades ${w.total_trades}`);
  }
  db.close();
});
