// The whole chain, on coins built to have known answers.
//
// Each piece is tested on its own elsewhere. What this file asks is whether they
// still agree once they are joined up: a coin written by the watcher, stored,
// read back out of SQLite, read by the analyser, and replayed by the backtest
// has to mean the same thing at the end as it did at the start.
//
// The fixtures are built backwards from the answers. One launch is a syndicate
// and a winning trade, one is organic and a losing trade, one is unbuyable and
// must be refused by both the analyser and the replay. A test that only checked
// the first would pass just as well against something that flagged everything.
//
// The load-bearing assertion is the last one: the replay run over records read
// out of the database is compared trade-for-trade against the same replay run
// over the original objects. Round-tripping "looks fine" is not the claim —
// the claim is that storage changes no number downstream of it.
//
// Not covered here: the socket and the borsh decoder. Raw program logs are not
// retained anywhere, so there is nothing to replay them from, and a synthetic
// byte fixture would only test that the decoder agrees with whatever encoded
// it. Ingest here starts where watch.js hands a finished record to Records.
//
// Run with: node --test test/

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { Db } from '../src/db.js';
import { Records } from '../src/record.js';
import { analyzeLaunch, isSyndicate, getSyndicateExposure } from '../src/cluster.js';
import { runBacktest, pathOf, buyEverything, basicMomentum, buyNothing } from '../src/backtest.js';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const DATA = path.join(ROOT, 'data');
const today = () => new Date().toISOString().slice(0, 10);

function tmp(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'sts-e2e-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

// ---------------------------------------------------------------------------
// Fixtures, built backwards from the answers they should produce
// ---------------------------------------------------------------------------

const T0 = 1786554000000;
const w = (n) => `Wa11et${String(n).padStart(2, '0')}${'x'.repeat(30)}`;
const DEV_SYNDI = `DevSynd1cate${'a'.repeat(32)}`;
const DEV_CLEAN = `DevOrgan1c${'b'.repeat(34)}`;

/** Prices as the watcher records them: absolute, with entry taken at 3s. */
const ENTRY = 0.001;

/**
 * Five wallets, one odd amount each, all inside the same instant, with the
 * deployer among them. Every size and timing signal should fire at once.
 * Its price path clears the 1.5× target in the second after entry.
 */
const syndicate = () => ({
  t: T0,
  mint: 'SyndicateM1nt1111111111111111111111111111111',
  symbol: 'SYNDI',
  name: 'Five Wallets One Wallet',
  creator: DEV_SYNDI,
  uri: 'https://example.invalid/syndi.json',
  supply: 1_000_000_000,
  initialBuySol: 0.777,
  social: { kind: 'tweet', handle: 'promoter', followers: 40 },
  open: { seconds: 3, wallets: 5, sellers: 0, solIn: 3.885, solOut: 0, trades: 5 },
  who: [
    { w: DEV_SYNDI, in: 0.777, out: 0, n: 1, at: 0.01 },
    { w: w(1), in: 0.777, out: 0, n: 1, at: 0.01 },
    { w: w(2), in: 0.777, out: 0, n: 1, at: 0.01 },
    { w: w(3), in: 0.777, out: 0, n: 1, at: 0.02 },
    { w: w(4), in: 0.777, out: 0, n: 1, at: 0.02 },
  ],
  total: { wallets: 5, sellers: 0, solIn: 3.885, solOut: 0, trades: 5 },
  outcome: {
    follow: 60,
    entry: ENTRY,
    peak: 0.0016,
    last: 0.0014,
    peakMult: 1.6,
    endMult: 1.4,
    peakAtSec: 4,
    trades: 5,
    highs: [[4, 1.6]],
    lows: [],
  },
  market: {
    candleSeconds: 1,
    candles: [
      // Entry second: flat around the entry price.
      { s: 3, o: 0.001, h: 0.00101, l: 0.00099, c: 0.001, volume: 3.885, buys: 5, sells: 0 },
      // Next second: low stays above the 0.85 stop, high clears the 1.5 target.
      { s: 4, o: 0.001, h: 0.0016, l: 0.00098, c: 0.0014, volume: 2, buys: 3, sells: 1 },
    ],
  },
});

/**
 * Six wallets, six different amounts, spread across the window, deployer not
 * buying. Nothing here should read as coordination. Its price path breaks the
 * 0.85 stop before it ever goes up.
 */
const organic = () => ({
  t: T0 + 60_000,
  mint: 'Organ1cM1nt22222222222222222222222222222222',
  symbol: 'CLEAN',
  name: 'Just A Coin',
  creator: DEV_CLEAN,
  uri: 'https://example.invalid/clean.json',
  supply: 1_000_000_000,
  initialBuySol: null,
  social: { kind: 'none' },
  open: { seconds: 3, wallets: 6, sellers: 1, solIn: 5.48, solOut: 0.2, trades: 8 },
  who: [
    { w: w(10), in: 0.31, out: 0, n: 1, at: 0.4 },
    { w: w(11), in: 1.7, out: 0, n: 2, at: 0.9 },
    { w: w(12), in: 0.05, out: 0, n: 1, at: 1.4 },
    { w: w(13), in: 2.4, out: 0.2, n: 2, at: 1.9 },
    { w: w(14), in: 0.9, out: 0, n: 1, at: 2.4 },
    { w: w(15), in: 0.12, out: 0, n: 1, at: 2.9 },
  ],
  total: { wallets: 6, sellers: 1, solIn: 5.48, solOut: 0.2, trades: 8 },
  outcome: {
    follow: 60,
    entry: ENTRY,
    peak: 0.00102,
    last: 0.0007,
    peakMult: 1.02,
    endMult: 0.7,
    peakAtSec: 3,
    trades: 8,
    highs: [],
    lows: [[4, 0.8]],
  },
  market: {
    candleSeconds: 1,
    candles: [
      { s: 3, o: 0.001, h: 0.00102, l: 0.001, c: 0.001, volume: 5.48, buys: 6, sells: 1 },
      // Low of 0.0008 is 0.8× entry, under the 0.85 stop.
      { s: 4, o: 0.001, h: 0.001, l: 0.0008, c: 0.0007, volume: 1, buys: 0, sells: 3 },
    ],
  },
});

/** Nobody bought, so there is no entry price and nothing to say. */
const unbuyable = () => ({
  t: T0 + 120_000,
  mint: 'NobodyBought3333333333333333333333333333333',
  symbol: 'THIN',
  name: 'Nobody Came',
  creator: `DevQu1et${'c'.repeat(36)}`,
  uri: null,
  supply: 1_000_000_000,
  initialBuySol: null,
  social: { kind: 'nometa' },
  open: { seconds: 3, wallets: 0, sellers: 0, solIn: 0, solOut: 0, trades: 0 },
  who: [],
  total: { wallets: 0, sellers: 0, solIn: 0, solOut: 0, trades: 0 },
  outcome: { follow: 60, entry: null, peak: null, last: null, peakMult: null, endMult: null, peakAtSec: null, trades: 0, highs: [], lows: [] },
  market: { candleSeconds: 1, candles: [] },
});

const fixtures = () => [syndicate(), organic(), unbuyable()];

/** Stage 1: hand the records to the writer, exactly as watch.js does. */
async function ingest(dir, records) {
  const db = new Db({ dir });
  const writer = new Records({ dir, key: 'mint', db });
  const accepted = records.map((r) => writer.write(r));
  await writer.close();
  return { db, writer, accepted };
}

/** Stage 3 read-back: the records as the database gives them back. */
const fromDb = (db) =>
  db.sql.prepare('SELECT raw FROM tokens ORDER BY created_at').all().map((r) => JSON.parse(r.raw));

// ---------------------------------------------------------------------------
// Stage 1 — ingest
// ---------------------------------------------------------------------------

test('e2e: every fixture is accepted once, into both destinations', async (t) => {
  const dir = tmp(t);
  const { db, writer, accepted } = await ingest(dir, fixtures());

  assert.deepEqual(accepted, [true, true, true]);
  assert.equal(writer.written, 3);
  assert.equal(writer.stored, 3);

  const lines = fs.readFileSync(path.join(dir, `coins-${today()}.jsonl`), 'utf8').split('\n').filter(Boolean);
  assert.equal(lines.length, 3, 'the archive has all three');
  assert.equal(db.count(), 3, 'the database has all three');
  db.close();
});

test('e2e: re-ingesting the same batch is a no-op end to end', async (t) => {
  const dir = tmp(t);
  const { db } = await ingest(dir, fixtures());
  const first = fromDb(db);

  const again = new Records({ dir, key: 'mint', db });
  assert.deepEqual(fixtures().map((r) => again.write(r)), [false, false, false]);
  await again.close();

  assert.equal(db.count(), 3);
  assert.deepEqual(fromDb(db), first);
  const lines = fs.readFileSync(path.join(dir, `coins-${today()}.jsonl`), 'utf8').split('\n').filter(Boolean);
  assert.equal(lines.length, 3, 'and nothing was appended a second time');
  db.close();
});

// ---------------------------------------------------------------------------
// Stage 2 — the analyser, on what came back out of storage
// ---------------------------------------------------------------------------

test('e2e: the syndicate fixture is called a syndicate', async (t) => {
  const dir = tmp(t);
  const { db } = await ingest(dir, fixtures());
  const stored = fromDb(db);
  const report = analyzeLaunch(stored.find((r) => r.symbol === 'SYNDI'));

  assert.equal(report.mint, syndicate().mint);
  assert.equal(report.window.participants, 5);
  assert.ok(report.risk_tags.includes('IDENTICAL_SIZING'), `tags were ${report.risk_tags}`);
  assert.ok(report.risk_tags.includes('CREATOR_BOUGHT_OWN'), `tags were ${report.risk_tags}`);
  assert.ok(report.confidence_score > 0.5, `confidence was ${report.confidence_score}`);
  assert.ok(isSyndicate(report), 'should clear the default threshold');
  assert.equal(report.thin, false);

  const exposure = getSyndicateExposure(report);
  assert.ok(exposure.wallets >= 3, `only ${exposure.wallets} wallets clustered`);
  assert.ok(exposure.pct > 0 && exposure.pct <= 100, `exposure ${exposure.pct}%`);
  t.diagnostic(`SYNDI ${report.confidence_score} — ${report.risk_tags.join(', ')}`);
  db.close();
});

test('e2e: the organic fixture is not called a syndicate', async (t) => {
  const dir = tmp(t);
  const { db } = await ingest(dir, fixtures());
  const stored = fromDb(db);
  const report = analyzeLaunch(stored.find((r) => r.symbol === 'CLEAN'));

  assert.equal(report.window.participants, 6);
  assert.equal(isSyndicate(report), false, `confidence was ${report.confidence_score}`);
  assert.ok(!report.risk_tags.includes('IDENTICAL_SIZING'), `tags were ${report.risk_tags}`);
  assert.ok(!report.risk_tags.includes('CREATOR_BOUGHT_OWN'), 'the deployer did not buy');
  t.diagnostic(`CLEAN ${report.confidence_score} — ${report.risk_tags.join(', ') || 'no tags'}`);
  db.close();
});

test('e2e: a launch nobody bought is reported as thin, not as innocent', async (t) => {
  const dir = tmp(t);
  const { db } = await ingest(dir, fixtures());
  const report = analyzeLaunch(fromDb(db).find((r) => r.symbol === 'THIN'));

  assert.equal(report.confidence_score, 0);
  assert.deepEqual(report.risk_tags, ['NO_OPENING_BUYS']);
  assert.equal(report.thin, true);
  assert.equal(isSyndicate(report), false);
  db.close();
});

test('e2e: the analyser separates the two launches by a wide margin', async (t) => {
  const dir = tmp(t);
  const { db } = await ingest(dir, fixtures());
  const stored = fromDb(db);
  const syndi = analyzeLaunch(stored.find((r) => r.symbol === 'SYNDI'));
  const clean = analyzeLaunch(stored.find((r) => r.symbol === 'CLEAN'));
  // A detector that ranks them the right way round but only just is not much
  // use on a corpus where most launches are ordinary.
  assert.ok(
    syndi.confidence_score - clean.confidence_score > 0.3,
    `${syndi.confidence_score} vs ${clean.confidence_score} is too close to call`,
  );
  db.close();
});

// ---------------------------------------------------------------------------
// Stage 3 — storage fidelity
// ---------------------------------------------------------------------------

test('e2e: what comes out of SQLite is what went in', async (t) => {
  const dir = tmp(t);
  const { db } = await ingest(dir, fixtures());
  assert.deepEqual(fromDb(db), fixtures(), 'including candles, highs, lows and social');
  db.close();
});

test('e2e: the indexed columns describe the fixtures correctly', async (t) => {
  const dir = tmp(t);
  const { db } = await ingest(dir, fixtures());
  const row = (m) => db.sql.prepare('SELECT * FROM tokens WHERE mint=?').get(m);

  const s = row(syndicate().mint);
  assert.equal(s.symbol, 'SYNDI');
  assert.equal(s.created_at, T0);
  assert.equal(s.initial_buy_sol, 0.777);
  assert.equal(s.market_cap, ENTRY * 1_000_000_000); // entry × supply
  assert.equal(s.uri, 'https://example.invalid/syndi.json');

  // No entry price means no market cap, rather than a zero that reads as one.
  assert.equal(row(unbuyable().mint).market_cap, null);
  assert.equal(row(organic().mint).initial_buy_sol, null);
  db.close();
});

test('e2e: the wallet rollup counts the syndicate wallets it should', async (t) => {
  const dir = tmp(t);
  const { db } = await ingest(dir, fixtures());
  db.rebuildWallets();

  const dev = db.sql.prepare('SELECT * FROM wallets WHERE address=?').get(DEV_SYNDI);
  assert.deepEqual(JSON.parse(dev.flags), ['creator']);
  assert.equal(dev.first_seen, T0, 'a deployer is present from the launch itself');
  assert.equal(dev.win_rate, 0, 'put SOL in, took none out');

  // Every distinct address across all three fixtures, creators included.
  const expected = new Set(fixtures().flatMap((r) => [r.creator, ...r.who.map((x) => x.w)]));
  assert.equal(db.sql.prepare('SELECT COUNT(*) n FROM wallets').get().n, expected.size);
  db.close();
});

// ---------------------------------------------------------------------------
// Stage 4 — the replay
// ---------------------------------------------------------------------------

test('e2e: the replay reaches the outcomes the fixtures were built to have', async (t) => {
  const dir = tmp(t);
  const { db } = await ingest(dir, fixtures());
  const run = runBacktest({ records: fromDb(db), strategy: buyEverything });

  assert.equal(run.recordsConsidered, 3);
  assert.equal(run.trades.length, 2, 'the unbuyable coin cannot be traded');
  assert.equal(run.skipped.noEntry, 1);

  const [win, loss] = run.trades;
  assert.equal(win.symbol, 'SYNDI');
  assert.equal(win.reason, 'target');
  assert.equal(win.fidelity, 'candles', 'candles are the best fidelity and should be preferred');
  assert.ok(win.pnlSol > 0, `expected a win, got ${win.pnlSol}`);

  assert.equal(loss.symbol, 'CLEAN');
  assert.equal(loss.reason, 'stop');
  assert.ok(loss.pnlSol < 0, `expected a loss, got ${loss.pnlSol}`);

  // Chronological: SYNDI launched a minute before CLEAN.
  assert.ok(win.t < loss.t);
  assert.equal(run.byFidelity.candles, 2);
  t.diagnostic(`${win.symbol} ${win.pnlSol} SOL, ${loss.symbol} ${loss.pnlSol} SOL`);
  db.close();
});

test('e2e: storing a coin changes nothing about how it replays', async (t) => {
  const dir = tmp(t);
  const { db } = await ingest(dir, fixtures());

  // The whole point of the chain: same rule, same coins, one set straight from
  // the watcher and one that has been through the database.
  const direct = runBacktest({ records: fixtures(), strategy: buyEverything });
  const stored = runBacktest({ records: fromDb(db), strategy: buyEverything });

  assert.deepEqual(stored.trades, direct.trades);
  assert.deepEqual(stored.summary, direct.summary);
  assert.deepEqual(stored.equity, direct.equity);
  assert.deepEqual(stored.skipped, direct.skipped);
  assert.deepEqual(stored.byFidelity, direct.byFidelity);
  db.close();
});

test('e2e: the price path itself survives the round trip', async (t) => {
  const dir = tmp(t);
  const { db } = await ingest(dir, fixtures());
  const stored = fromDb(db);
  for (const original of fixtures()) {
    const back = stored.find((r) => r.mint === original.mint);
    assert.deepEqual(pathOf(back), pathOf(original), `${original.symbol} path differs`);
  }
  db.close();
});

test('e2e: a selective strategy passes on the coins it should', async (t) => {
  const dir = tmp(t);
  const { db } = await ingest(dir, fixtures());
  const run = runBacktest({ records: fromDb(db), strategy: basicMomentum });

  // basic-momentum wants 4+ buyers, fewer sellers than buyers, and 1+ SOL in.
  // SYNDI and CLEAN both qualify; THIN has nobody.
  assert.equal(run.trades.length, 2);
  assert.equal(run.skipped.notTaken, 1);
  assert.equal(run.skipped.noEntry, 0, 'it never got as far as pricing the thin one');
  db.close();
});

test('e2e: a run that takes nothing still reports cleanly', async (t) => {
  const dir = tmp(t);
  const { db } = await ingest(dir, fixtures());
  const run = runBacktest({ records: fromDb(db), strategy: buyNothing });
  assert.equal(run.trades.length, 0);
  assert.equal(run.skipped.notTaken, 3);
  assert.equal(run.summary.trades, 0);
  assert.equal(run.summary.finalBalanceSol, 10, 'an untouched account is the starting balance');
  assert.equal(run.summary.pnlSol, 0);
  assert.equal(run.summary.maxDrawdownSol, 0);
  db.close();
});

// ---------------------------------------------------------------------------
// The chain, twice
// ---------------------------------------------------------------------------

test('e2e: the whole pipeline is deterministic', async (t) => {
  const once = async () => {
    const dir = tmp(t);
    const { db } = await ingest(dir, fixtures());
    const stored = fromDb(db);
    const out = {
      count: db.count(),
      wallets: db.rebuildWallets(),
      reports: stored.map((r) => {
        const rep = analyzeLaunch(r);
        return { mint: rep.mint, score: rep.confidence_score, tags: rep.risk_tags };
      }),
      run: runBacktest({ records: stored, strategy: buyEverything }).trades,
    };
    db.close();
    return out;
  };
  assert.deepEqual(await once(), await once());
});

// ---------------------------------------------------------------------------
// The same chain over the real recordings, when they are there
// ---------------------------------------------------------------------------

test('e2e: real coins survive the chain and replay identically from storage', async (t) => {
  if (!fs.existsSync(DATA)) return t.skip('no data/ corpus');
  const files = fs.readdirSync(DATA).filter((f) => /^coins-\d{4}-\d{2}-\d{2}\.jsonl$/.test(f)).sort();
  if (!files.length) return t.skip('no data/ corpus');

  const seen = new Set();
  const real = [];
  for (const f of files) {
    for (const line of fs.readFileSync(path.join(DATA, f), 'utf8').split('\n')) {
      if (!line.trim() || real.length >= 400) continue;
      try {
        const r = JSON.parse(line);
        if (r?.mint && !seen.has(r.mint)) { seen.add(r.mint); real.push(r); }
      } catch {}
    }
  }
  if (!real.length) return t.skip('no readable records');

  const dir = tmp(t);
  const { db } = await ingest(dir, real);
  const stored = fromDb(db);
  assert.equal(stored.length, real.length);

  // Every real coin analyses without throwing, and stays inside its range.
  let flagged = 0;
  for (const r of stored) {
    const rep = analyzeLaunch(r);
    assert.ok(rep.confidence_score >= 0 && rep.confidence_score <= 1, `${r.mint} scored ${rep.confidence_score}`);
    for (const tag of rep.risk_tags) assert.ok(typeof tag === 'string' && tag.length, `${r.mint} had an empty tag`);
    if (isSyndicate(rep)) flagged++;
  }

  const direct = runBacktest({ records: real, strategy: basicMomentum });
  const replayed = runBacktest({ records: stored, strategy: basicMomentum });
  assert.deepEqual(replayed.trades, direct.trades, 'the database changed a real backtest');
  assert.deepEqual(replayed.summary, direct.summary);

  t.diagnostic(`${real.length} real coins, ${flagged} flagged, ${direct.trades.length} trades taken`);
  db.close();
});
