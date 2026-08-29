// What the setup path has to get right, run the way a collaborator runs it.
//
// These spawn the real scripts as child processes rather than importing them.
// That is the point: what is being tested is the command in the README, exit
// codes and printed sentences included, because those are the whole interface
// for someone who has just cloned this and has no idea what a `Db` is.
//
// The claim being defended is that running the import twice is boring. The
// recorded files really do contain the same mint more than once — a restart
// starts the watcher's in-memory dedup set over — so "re-running is safe" is
// not a nicety here, it is the normal case. A second run must not raise a
// constraint error, must not overwrite the first version of a coin, and must
// say plainly that it skipped everything.
//
// Every test gets its own directory and its own database. Nothing here reads
// data/, so these run the same on a fresh clone as on a machine with months of
// recordings.
//
// Run with: node --test test/

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

import { Db } from '../src/db.js';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

/** A fresh directory per test, removed afterwards. */
function tmp(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'sts-migrate-test-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

/** Run one of the scripts the way npm does, and hand back everything it said. */
function run(script, args = []) {
  const r = spawnSync(process.execPath, [path.join(ROOT, 'scripts', script), ...args], {
    encoding: 'utf8',
  });
  if (r.error) throw r.error;
  return { status: r.status, stdout: r.stdout, stderr: r.stderr, out: r.stdout + r.stderr };
}

const migrate = (dir, ...rest) => run('migrate-jsonl.js', ['--dir', dir, ...rest]);
const setup = (dir, ...rest) => run('db-setup.js', ['--dir', dir, ...rest]);
const check = (dir, ...rest) => run('check-db.js', ['--dir', dir, ...rest]);

// ---------------------------------------------------------------------------
// Fixtures, in the shape watch.js writes
// ---------------------------------------------------------------------------

const CREATOR = 'CreatorWa11etAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA';
const OTHER = 'OtherWa11etBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB';

const coin = (mint, over = {}) => ({
  t: 1786554449571,
  mint,
  symbol: 'TEST',
  name: 'A Test Coin',
  creator: CREATOR,
  uri: 'https://example.invalid/meta.json',
  supply: 1_000_000_000,
  initialBuySol: 1.5,
  open: { seconds: 3, wallets: 2, sellers: 0, solIn: 3.5, solOut: 0, trades: 4 },
  who: [
    { w: CREATOR, in: 1.5, out: 0, n: 1, at: 0.01 },
    { w: OTHER, in: 2, out: 3, n: 3, at: 1.5 },
  ],
  total: { wallets: 2, sellers: 1, solIn: 3.5, solOut: 3, trades: 4 },
  outcome: { follow: 60, entry: 0.000034, peak: 0.00005, last: 0.00004, peakMult: 1.47 },
  ...over,
});

/** Write one day's file. Lines that are already strings go through untouched. */
function day(dir, date, records) {
  const body = records.map((r) => (typeof r === 'string' ? r : JSON.stringify(r))).join('\n') + '\n';
  fs.writeFileSync(path.join(dir, `coins-${date}.jsonl`), body);
}

const open = (t, dir) => {
  const db = new Db({ dir });
  t.after(() => db.close());
  return db;
};

// ---------------------------------------------------------------------------
// The import itself
// ---------------------------------------------------------------------------

test('a directory of files becomes rows, and the columns say what the record said', (t) => {
  const dir = tmp(t);
  day(dir, '2026-08-10', [coin('Mint1'), coin('Mint2'), coin('Mint3')]);

  const r = migrate(dir);
  assert.equal(r.status, 0, r.out);

  const db = open(t, dir);
  assert.equal(db.count(), 3);
  assert.deepEqual(db.mints().sort(), ['Mint1', 'Mint2', 'Mint3']);

  const row = db.sql.prepare('SELECT * FROM tokens WHERE mint = ?').get('Mint1');
  assert.equal(row.name, 'A Test Coin');
  assert.equal(row.symbol, 'TEST');
  assert.equal(row.uri, 'https://example.invalid/meta.json');
  assert.equal(row.created_at, 1786554449571);
  assert.equal(row.initial_buy_sol, 1.5);
  // entry × supply, and nothing more clever than that.
  assert.equal(row.market_cap, 34000);
  // The whole record survives the trip, which is what makes a column we did not
  // think to add today fillable from `raw` tomorrow.
  assert.deepEqual(JSON.parse(row.raw), coin('Mint1'));
});

test('running it a second time adds nothing and says so', (t) => {
  const dir = tmp(t);
  day(dir, '2026-08-10', [coin('Mint1'), coin('Mint2')]);

  const first = migrate(dir);
  assert.equal(first.status, 0, first.out);
  assert.match(first.stdout, /tokens\s+2 ingested, 0 skipped/);

  const second = migrate(dir);
  assert.equal(second.status, 0, second.out);
  assert.match(second.stdout, /tokens\s+0 ingested, 2 skipped/);
  assert.match(second.stdout, /coins\s+2 → 2/);
  // Not a word about a constraint. A duplicate is expected traffic, not a fault.
  assert.doesNotMatch(second.out, /UNIQUE|constraint|SQLITE_/i);

  assert.equal(open(t, dir).count(), 2);
});

test('a mint recorded twice is stored once, as it was first seen', (t) => {
  const dir = tmp(t);
  // The same coin, re-observed the next day after a restart, with a name that
  // changed in between. The first version is the one that was true then.
  day(dir, '2026-08-10', [coin('Mint1', { name: 'First' }), coin('Mint2')]);
  day(dir, '2026-08-11', [coin('Mint1', { name: 'Second' }), coin('Mint3')]);

  const r = migrate(dir);
  assert.equal(r.status, 0, r.out);

  const db = open(t, dir);
  assert.equal(db.count(), 3);
  const row = db.sql.prepare('SELECT name, raw FROM tokens WHERE mint = ?').get('Mint1');
  assert.equal(row.name, 'First');
  assert.equal(JSON.parse(row.raw).name, 'First');
  assert.match(r.stdout, /tokens\s+3 ingested, 1 skipped/);
});

test('a half-written line does not stop the file it is in', (t) => {
  const dir = tmp(t);
  day(dir, '2026-08-10', [
    coin('Mint1'),
    '{"mint":"Mint2","name":"cut off here',   // killed mid-append
    '',                                        // a blank line is not a record
    JSON.stringify({ t: 1, name: 'no mint' }), // parses, but has no key
    coin('Mint3'),
  ]);

  const r = migrate(dir);
  assert.equal(r.status, 0, r.out);
  assert.match(r.stdout, /records\s+4 read, 2 valid, 2 unreadable/);

  const db = open(t, dir);
  assert.deepEqual(db.mints().sort(), ['Mint1', 'Mint3']);
});

test('the wallet rollup is rebuilt from what was just imported', (t) => {
  const dir = tmp(t);
  day(dir, '2026-08-10', [coin('Mint1'), coin('Mint2')]);

  const r = migrate(dir);
  assert.match(r.stdout, /wallets\s+2 rebuilt/);

  const db = open(t, dir);
  const creator = db.sql.prepare('SELECT * FROM wallets WHERE address = ?').get(CREATOR);
  assert.ok(creator, 'the creator should have a row');
  assert.equal(creator.total_trades, 2); // one trade on each of two coins
  assert.deepEqual(JSON.parse(creator.flags), ['creator']);

  const other = db.sql.prepare('SELECT * FROM wallets WHERE address = ?').get(OTHER);
  assert.equal(other.total_trades, 6);
  assert.equal(other.win_rate, 1); // took out more than it put in, both times
});

test('a dry run counts everything and leaves no database behind', (t) => {
  const dir = tmp(t);
  day(dir, '2026-08-10', [coin('Mint1'), coin('Mint2')]);

  const r = migrate(dir, '--dry-run');
  assert.equal(r.status, 0, r.out);
  assert.match(r.stdout, /records\s+2 read, 2 valid, 0 unreadable/);
  assert.match(r.stdout, /dry run — nothing written/);
  // Creating the file it promised not to write would be the same bug either
  // way: a directory that looks imported when nothing was.
  assert.equal(fs.existsSync(path.join(dir, 'sts.db')), false);
});

test('an empty directory says what is missing instead of importing nothing', (t) => {
  const r = migrate(tmp(t));
  assert.equal(r.status, 1);
  assert.match(r.stderr, /no coins-.*\.jsonl files/);
  assert.match(r.stderr, /npm run watch/);
});

test('a directory that is not there is an error, not a crash', (t) => {
  const r = migrate(path.join(tmp(t), 'nowhere'));
  assert.equal(r.status, 1);
  assert.match(r.stderr, /no such directory/);
});

test('output into a pipe carries no progress bar', (t) => {
  const dir = tmp(t);
  day(dir, '2026-08-10', [coin('Mint1')]);
  const r = migrate(dir);
  // spawnSync is not a terminal, and a bar redrawn into one of these writes
  // hundreds of half-lines into whatever is reading it.
  assert.doesNotMatch(r.out, /\x1b\[/, 'no escape codes into a pipe');
  assert.doesNotMatch(r.out, /[█░]/, 'no bar into a pipe');
  assert.match(r.stdout, /coins-2026-08-10\.jsonl\s+1 lines\s+1 new\s+0 skipped/);
});

// ---------------------------------------------------------------------------
// --verify
// ---------------------------------------------------------------------------

test('verify passes on a whole import, and counts mints rather than lines', (t) => {
  const dir = tmp(t);
  // Four lines, three coins: line count and row count are supposed to differ.
  day(dir, '2026-08-10', [coin('Mint1'), coin('Mint2')]);
  day(dir, '2026-08-11', [coin('Mint1'), coin('Mint3')]);
  migrate(dir);

  const r = migrate(dir, '--verify');
  assert.equal(r.status, 0, r.out);
  assert.match(r.stdout, /files\s+4 lines, 4 valid, 0 unreadable, 3 distinct mints/);
  assert.match(r.stdout, /database\s+3 rows/);
  assert.match(r.stdout, /missing\s+0 mints/);
  assert.match(r.stdout, /ok — every coin in the files has a row/);
});

test('verify fails, loudly and non-zero, when a coin has no row', (t) => {
  const dir = tmp(t);
  day(dir, '2026-08-10', [coin('Mint1'), coin('Mint2'), coin('Mint3')]);
  migrate(dir);

  const db = new Db({ dir });
  db.sql.prepare('DELETE FROM tokens WHERE mint = ?').run('Mint2');
  db.close();

  const r = migrate(dir, '--verify');
  assert.equal(r.status, 1);
  assert.match(r.stdout, /missing\s+1 mint in the files with no row/);
  assert.match(r.stderr, /FAIL — 1 coin in the files never made it/);
  assert.match(r.stderr, /Mint2/);
  assert.match(r.stderr, /npm run db:migrate/);
});

test('verify forgives rows the files do not account for', (t) => {
  const dir = tmp(t);
  day(dir, '2026-08-10', [coin('Mint1')]);
  migrate(dir);

  // What it looks like when the watcher wrote straight to the database and the
  // day's file was archived away afterwards. Not a fault.
  const db = new Db({ dir });
  db.insertTokens([coin('MintFromAnArchivedDay')]);
  db.close();

  const r = migrate(dir, '--verify');
  assert.equal(r.status, 0, r.out);
  assert.match(r.stdout, /extra\s+1 row no file accounts for/);
  assert.match(r.stdout, /ok — every coin in the files has a row/);
});

test('verify on a directory with no database says how to make one', (t) => {
  const dir = tmp(t);
  day(dir, '2026-08-10', [coin('Mint1')]);

  const r = migrate(dir, '--verify');
  assert.equal(r.status, 1);
  assert.match(r.stderr, /no database at/);
  assert.match(r.stderr, /npm run db:setup/);
  assert.equal(fs.existsSync(path.join(dir, 'sts.db')), false);
});

// ---------------------------------------------------------------------------
// db:setup and the pre-run check
// ---------------------------------------------------------------------------

test('db:setup builds the database when there is none', (t) => {
  const dir = tmp(t);
  day(dir, '2026-08-10', [coin('Mint1'), coin('Mint2')]);

  const r = setup(dir);
  assert.equal(r.status, 0, r.out);
  assert.match(r.stdout, /no sts.db yet — building it from 1 file/);
  assert.equal(open(t, dir).count(), 2);
});

test('db:setup leaves a database that is already there alone', (t) => {
  const dir = tmp(t);
  day(dir, '2026-08-10', [coin('Mint1')]);
  setup(dir);

  // A day recorded after the database was built. Setup is not the command that
  // brings it in — re-reading every file on every setup would cost minutes to
  // find nothing — so it points at the one that is.
  day(dir, '2026-08-11', [coin('Mint2')]);
  const r = setup(dir);
  assert.equal(r.status, 0, r.out);
  assert.match(r.stdout, /already at .*sts\.db — 1 coin/);
  assert.match(r.stdout, /npm run db:migrate/);
  assert.equal(open(t, dir).count(), 1);
});

test('db:setup with nothing to import warns instead of failing', (t) => {
  const dir = tmp(t);
  const r = setup(dir);
  // Exit 0 on purpose: a collaborator who has not collected anything yet has
  // not done anything wrong, and a red build tells them the wrong thing.
  assert.equal(r.status, 0, r.out);
  assert.match(r.out, /nothing in .* to build one from/);
  assert.match(r.out, /npm run watch/);
  assert.match(r.out, /STS_HOME/);
  assert.equal(fs.existsSync(path.join(dir, 'sts.db')), false);
});

test('the pre-run check is quiet when the database is there', (t) => {
  const dir = tmp(t);
  day(dir, '2026-08-10', [coin('Mint1')]);
  migrate(dir);

  const r = check(dir, '--for', 'test');
  assert.equal(r.status, 0);
  assert.equal(r.out.trim(), '', 'nothing to say, so it says nothing');
});

test('the pre-run check explains a missing database without failing the run', (t) => {
  const dir = tmp(t);

  const forTests = check(dir, '--for', 'test');
  assert.equal(forTests.status, 0, 'a missing corpus must not fail the suite');
  assert.match(forTests.out, /sts\.db is not there yet/);
  assert.match(forTests.out, /tests that read the recorded corpus will skip/);
  assert.match(forTests.out, /npm run db:setup/);

  const forDash = check(dir, '--for', 'dash');
  assert.equal(forDash.status, 0);
  assert.match(forDash.out, /dashboard will open with no history/);

  // Checking for a file must never be the thing that creates it.
  assert.equal(fs.existsSync(path.join(dir, 'sts.db')), false);
});

test('the pre-run check speaks up for a database that is there but empty', (t) => {
  const dir = tmp(t);
  new Db({ dir }).close(); // opened once, never imported into

  const r = check(dir, '--for', 'test');
  assert.equal(r.status, 0);
  assert.match(r.out, /holds no coins/);
  assert.match(r.out, /npm run db:setup/);
});
