// W21's check C7, and the row invariants a census cannot see.
//
// The point of these living next to the recorder: every defect this producer was
// found to have was a field that read perfectly and never varied. A check that
// ships with the code that writes the file is the only kind that cannot quietly
// fall out of date with it.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { census, walk, checkRow, checkOutcome, checkSells, checkFiles, kindOfFile, unbackedCounters, tokenBalance, curveConservation, solConservation, checkUnheld, curveFloor, CURVE_FLOOR_SOL, DECLARED_CONSTANTS } from '../src/check.js';
import { SCHEMA, KNOWN_SCHEMAS, schemaStatus } from '../src/session.js';

const report = (rows) => {
  const c = census();
  for (const r of rows) c.add(r);
  return c.report(rows.length);
};

const deadPaths = (rows) => report(rows).dead.map((d) => d.path);

// ---------------------------------------------------------------------------
// C7 — a field that never varies is carrying no information
// ---------------------------------------------------------------------------

test('a field with one value across the corpus is reported dead', () => {
  assert.deepEqual(deadPaths([{ follow: 60, hi: 1.2 }, { follow: 60, hi: 0.8 }]), ['follow']);
});

test('a field that varies is left alone', () => {
  assert.deepEqual(deadPaths([{ hi: 1.2 }, { hi: 0.8 }]), []);
});

test('the false-and-null pair that hid on 5,003 rows is caught', () => {
  const rows = Array.from({ length: 50 }, (_, i) => ({ mint: `m${i}`, score: null, eligible: false }));
  assert.deepEqual(deadPaths(rows).sort(), ['eligible', 'score']);
});

test('a constant two levels down is caught too', () => {
  assert.deepEqual(deadPaths([{ funding: { depth: 2 } }, { funding: { depth: 2 } }]), ['funding.depth']);
});

test('arrays are skipped whole, because their contents vary by construction', () => {
  assert.deepEqual(deadPaths([{ who: [1, 2] }, { who: [1, 2] }]), []);
});

test('null is a value like any other, and a field that is always null is dead', () => {
  assert.deepEqual(deadPaths([{ score: null }, { score: null }]), ['score']);
});

test('a field that is null on some rows and set on others is alive', () => {
  assert.deepEqual(deadPaths([{ score: null }, { score: 7 }]), []);
});

test('a block that is absent on some rows is not a dead field', () => {
  // `funding` is null on 213 of the 1,667 rows of 2026-08-21 because those
  // launches were never asked about. That is not a constant field, it is a
  // missing block, and its own leaves are censused on their own.
  const dead = deadPaths([{ funding: null }, { funding: { hopsWalked: 1 } }, { funding: null }]);
  assert.equal(dead.includes('funding'), false);
});

test('but a leaf inside such a block is still caught', () => {
  const dead = deadPaths([{ funding: null }, { funding: { depth: 2 } }, { funding: { depth: 2 } }]);
  assert.deepEqual(dead, ['funding.depth']);
});

test('numbers and strings that look alike are not confused for each other', () => {
  assert.deepEqual(deadPaths([{ nth: 1 }, { nth: '1' }]), []);
});

test('the report says how many rows each dead field appeared on', () => {
  const r = report([{ follow: 60 }, { follow: 60 }, {}]);
  assert.equal(r.dead[0].rows, 2);
  assert.equal(r.rows, 3);
});

test('walk visits every scalar leaf by its full path', () => {
  const seen = [];
  walk({ a: 1, b: { c: 2, d: { e: 3 } }, f: [9] }, (p, v) => seen.push(`${p}=${v}`));
  assert.deepEqual(seen, ['a=1', 'b.c=2', 'b.d.e=3']);
});

// ---------------------------------------------------------------------------
// A constant somebody decided on is not a defect
// ---------------------------------------------------------------------------

test('a declared constant is reported separately, with the reason', () => {
  const r = report([{ supply: 1e9 }, { supply: 1e9 }]);
  assert.deepEqual(r.dead, []);
  assert.equal(r.declared.length, 1);
  assert.match(r.declared[0].why, /protocol/);
});

test('outcome.follow is only allowed to be constant because observedSec sits beside it', () => {
  // It used to be declared "STILL A DEFECT", and it was: the configured window
  // written on every row as if it were the observed one. It is now a policy
  // constant, and the row check below is what keeps it honest.
  assert.match(DECLARED_CONSTANTS['outcome.follow'], /observedSec/);
  const legacy = { mint: 'M', t: 1, sid: 's', slot: 1, sig: 'x', outcome: { follow: 60, entry: 1 } };
  assert.match(checkRow(legacy).join(' '), /no observedSec/);
});

test('the opening cutoff is a constant a reader needs, not one to delete', () => {
  assert.ok('open.seconds' in DECLARED_CONSTANTS);
  assert.deepEqual(deadPaths([{ open: { seconds: 3 } }, { open: { seconds: 3 } }]), []);
});

// ---------------------------------------------------------------------------
// Rows that contradict themselves — invisible to a census
// ---------------------------------------------------------------------------

const tracksRow = (over = {}) => ({
  mint: 'M', t: 1, entry: 1, last: 1, watchedSec: 100,
  hi: 1, lo: 1, peakAtSec: null, cross: {}, ...over,
});

test('the defect 2 shape is caught even though hi and peakAtSec both vary', () => {
  assert.match(checkRow(tracksRow({ peakAtSec: 14 })).join(' '), /never beat entry/);
  // A census cannot see this one: both fields take more than one value across
  // the corpus and are therefore perfectly alive. Only the row says it is wrong.
  const dead = deadPaths([tracksRow({ peakAtSec: 14 }), tracksRow({ hi: 2, peakAtSec: 90 })]);
  assert.equal(dead.includes('hi'), false);
  assert.equal(dead.includes('peakAtSec'), false);
});

test('a sound tracks row has nothing to complain about', () => {
  assert.deepEqual(checkRow(tracksRow()), []);
  assert.deepEqual(checkRow(tracksRow({ hi: 2, peakAtSec: 300 })), []);
});

const coinsRow = (funding) => ({ mint: 'M', t: 1, funding });

test('a funding block that echoes the configured cap is rejected', () => {
  const bad = checkRow(coinsRow({ depth: 2, requested: 1, resolved: 0 }));
  assert.match(bad.join(' '), /configured cap echoed back/);
});

test('a funding block that cannot say how far it looked is rejected', () => {
  assert.match(checkRow(coinsRow({ requested: 1 })).join(' '), /no hopsWalked/);
});

test('perHop and hopsWalked have to agree', () => {
  const bad = checkRow(coinsRow({ hopsWalked: 2, perHop: [{ hop: 1, asked: 1, resolved: 0 }], requested: 1 }));
  assert.match(bad.join(' '), /perHop has 1 entries but hopsWalked is 2/);
});

test('the status census has to account for every wallet asked about', () => {
  const bad = checkRow(coinsRow({
    hopsWalked: 1, perHop: [{ hop: 1, asked: 3, resolved: 0 }], requested: 3,
    status: { ok: 0, none: 1, truncated: 0, error: 0, notAsked: 0 },
  }));
  assert.match(bad.join(' '), /sums to 1 but 3 wallets/);
});

test('a sound coin row passes', () => {
  assert.deepEqual(checkRow(coinsRow({
    available: false, hopsWalked: 1, perHop: [{ hop: 1, asked: 2, resolved: 0 }],
    requested: 2, resolved: 0, unresolved: 2,
    status: { ok: 0, none: 2, truncated: 0, error: 0, notAsked: 0 }, transfers: [],
  })), []);
});

test('a coin row with no funding block at all is not complained about', () => {
  assert.deepEqual(checkRow({ mint: 'M', funding: null }), []);
});

// ---------------------------------------------------------------------------
// Over real files
// ---------------------------------------------------------------------------

function withFile(lines, fn) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'capture-check-'));
  const file = path.join(dir, 'rows.jsonl');
  fs.writeFileSync(file, lines.map((l) => (typeof l === 'string' ? l : JSON.stringify(l))).join('\n') + '\n');
  return Promise.resolve(fn(file)).finally(() => fs.rmSync(dir, { recursive: true, force: true }));
}

test('a file of good rows passes clean', async () => {
  await withFile([tracksRow({ mint: 'A', hi: 2, peakAtSec: 90 }), tracksRow({ mint: 'B' })], async (file) => {
    const r = await checkFiles([file]);
    assert.equal(r.rows, 2);
    assert.equal(r.badRows, 0);
    const dead = r.dead.map((d) => d.path);
    assert.equal(dead.includes('hi'), false);
    assert.equal(dead.includes('peakAtSec'), false);
  });
});

test('a file with the defect 2 shape in it fails, and says which line', async () => {
  await withFile([tracksRow({ mint: 'A' }), tracksRow({ mint: 'B', peakAtSec: 14 })], async (file) => {
    const r = await checkFiles([file]);
    assert.equal(r.badRows, 1);
    assert.equal(r.examples[0].lineNo, 2);
    assert.match(r.examples[0].bad.join(' '), /never beat entry/);
  });
});

test('a line that is not JSON is a bad row, not a crash', async () => {
  await withFile([tracksRow(), 'not json at all'], async (file) => {
    const r = await checkFiles([file]);
    assert.equal(r.rows, 1);
    assert.equal(r.badRows, 1);
    assert.match(r.examples[0].bad[0], /not valid JSON/);
  });
});

test('blank lines are skipped rather than counted', async () => {
  await withFile([tracksRow(), '', tracksRow({ mint: 'B' })], async (file) => {
    const r = await checkFiles([file]);
    assert.equal(r.rows, 2);
    assert.equal(r.badRows, 0);
  });
});

test('the census spans every file it is given, not each one alone', async () => {
  await withFile([{ mint: 'A', kind: 'tweet' }], async (a) => {
    await withFile([{ mint: 'B', kind: 'none' }], async (b) => {
      assert.deepEqual((await checkFiles([a, b])).dead.map((d) => d.path), []);
      assert.deepEqual((await checkFiles([a])).dead.map((d) => d.path), ['kind', 'mint']);
    });
  });
});

// ---------------------------------------------------------------------------
// Defect 1 — the row has to say how long it was really watched
// ---------------------------------------------------------------------------

/**
 * A finished coin row of the shape the recorder writes now.
 *
 * `entry` and `curveAtEntry` are a coherent pair on purpose — the price is
 * `vsol / vtok` and nothing else, and a fixture that has them disagree is a
 * fixture asserting a row the recorder cannot produce. It carried `entry: 1`
 * with no state behind it and no `feeSol` until this was noticed: two fields the
 * recorder always writes that no check was ever pointed at.
 */
const coin = (outcome = {}, over = {}) => ({
  mint: 'M', t: 1, sid: 'abc-1', seq: 0, slot: 42, sig: 'S',
  outcome: {
    follow: 60, observedSec: 60, complete: true, stopReason: 'window', gapSec: 0,
    entry: 30 / 1.073e9, curveAtEntry: [30, 1.073e9, 0, 793_100_000],
    highs: [], lows: [], highsCapped: false, lowsCapped: false,
    sells: [], sellsCapped: false, creatorSellAtSec: null,
    feeBps: { 95: 1 }, zeroFee: [], zeroFeeCapped: false, feeSol: 0, ...outcome,
  },
  ...over,
});

test('a sound coin row passes every new check', () => {
  assert.deepEqual(checkRow(coin()), []);
  assert.deepEqual(checkRow(coin({ complete: false, stopReason: 'shutdown', observedSec: 12 })), []);
  assert.deepEqual(checkRow(coin({ complete: false, stopReason: 'socket-down', gapSec: 41 })), []);
});

test('the whole recorded corpus fails this check, which is the point', () => {
  // Every row of coins-2026-08-{16,20,21}.jsonl has exactly this shape: the
  // configured window and nothing about the observed one.
  const legacy = { mint: 'M', t: 1, outcome: { follow: 60, entry: 1, peak: 1, last: 1 } };
  const bad = checkRow(legacy).join(' | ');
  assert.match(bad, /no observedSec/);
  assert.match(bad, /no complete flag/);
  assert.match(bad, /no gapSec/);
  assert.match(bad, /no sid/);
  assert.match(bad, /no slot\/sig/);
});

test('a row claiming to be complete while the feed was down is a contradiction', () => {
  assert.match(checkOutcome(coin(), coin({ gapSec: 7 }).outcome).join(' '), /complete is true but gapSec is 7/);
});

test('a row claiming to be complete on half a window is a contradiction', () => {
  assert.match(checkRow(coin({ observedSec: 31 })).join(' '), /observedSec 31 is under the 60s window/);
});

test('a row cut off with no gap that still covers the whole window is a contradiction', () => {
  const bad = checkRow(coin({ complete: false, stopReason: 'shutdown', observedSec: 60 }));
  assert.match(bad.join(' '), /covers the whole 60s window/);
});

test('a truncated row has to say what truncated it', () => {
  const row = coin({ complete: false, observedSec: 4 });
  delete row.outcome.stopReason;
  assert.match(checkRow(row).join(' '), /no stopReason/);
});

test('a coin row with no session id says so — a calendar day is not a run', () => {
  assert.match(checkRow(coin({}, { sid: null })).join(' '), /which run recorded it/);
});

test('a coin row with no slot or signature cannot ever be costed', () => {
  assert.match(checkRow(coin({}, { slot: null })).join(' '), /what this transaction cost to land/);
  assert.match(checkRow(coin({}, { sig: null })).join(' '), /what this transaction cost to land/);
});

test('a capped turning-point list is reported, and the old silent cap is caught too', () => {
  assert.match(checkRow(coin({ highsCapped: true })).join(' '), /outcome.highs ran out of room/);
  const legacy = coin({ highs: Array.from({ length: 60 }, (_, i) => [i, 1 + i / 100]) });
  delete legacy.outcome.highsCapped;
  assert.match(checkRow(legacy).join(' '), /legacy cap with no highsCapped/);
});

test('a tracks row is not judged as if it were a coin row', () => {
  // Different file, different shape. Complaining about a missing `outcome` on
  // every tracks row is how a check gets ignored.
  assert.deepEqual(checkRow(tracksRow()), []);
});

// ---------------------------------------------------------------------------
// Sessions, heartbeats and uptime, over whole files
// ---------------------------------------------------------------------------

const sessionRows = (sid, { beats = 3, connected = beats, stop = true } = {}) => [
  { k: 'start', v: 2, sid, t: 0, policy: { heartbeatMs: 10_000, failSample: 50 } },
  ...Array.from({ length: beats }, (_, i) => ({ k: 'tick', sid, t: (i + 1) * 10_000, connected: i < connected })),
  ...(stop ? [{ k: 'stop', sid, t: (beats + 1) * 10_000 }] : []),
];

test('session rows are counted apart from coin rows and never censused', async () => {
  await withFile([...sessionRows('run-1'), coin({}, { mint: 'A', sid: 'run-1' })], async (file) => {
    const r = await checkFiles([file]);
    assert.equal(r.rows, 1, 'one coin');
    assert.equal(r.metaRows, 5, 'and the run talking about itself');
    // `k` is constant across the session rows by construction; if they were
    // censused it would drown the one signal C7 exists to give.
    assert.equal(r.dead.some((d) => d.path === 'k'), false);
  });
});

test('uptime comes out of the heartbeats, as a measured number', async () => {
  await withFile(sessionRows('run-2', { beats: 4, connected: 3 }), async (file) => {
    const [s] = (await checkFiles([file])).sessions;
    assert.equal(s.uptime, 0.75);
    assert.equal(s.ended, 'stop');
  });
});

test('a file with no session records at all reports none, rather than a clean bill', async () => {
  await withFile([{ mint: 'A', t: 1 }, { mint: 'B', t: 2 }], async (file) => {
    const r = await checkFiles([file]);
    assert.deepEqual(r.sessions, []);
    assert.equal(r.rowsWithoutSid, 2, 'uptime here is not bad, it is unmeasurable');
  });
});

test('one file holding two sessions is reported — that is the midnight split', async () => {
  await withFile([...sessionRows('run-a'), ...sessionRows('run-b')], async (file) => {
    const r = await checkFiles([file]);
    assert.equal(r.filesWithSeveralSessions.length, 1);
    assert.equal(r.sessions.length, 2);
  });
});

test('one session spread across two coin files is reported too', async () => {
  await withFile(sessionRows('run-c', { stop: false }), async (a) => {
    await withFile([{ k: 'stop', sid: 'run-c', t: 99_000 }], async (b) => {
      const r = await checkFiles([a, b]);
      assert.deepEqual(r.sessionsSplitAcrossFiles, ['run-c']);
    });
  });
});

test('a session writing its coins and its failures is not a session that was split', () => {
  // Every run writes four files. Complaining about that would make the check
  // cry wolf on every clean capture, which is how a check gets ignored.
  assert.equal(kindOfFile('/d/coins-abc-20260827-0238.jsonl'), 'coins');
  assert.notEqual(kindOfFile('/d/fails-abc-20260827-0238.jsonl'), 'coins');
  assert.equal(kindOfFile('/d/coins-2026-08-20.jsonl'), 'coins', 'the old dated naming too');
});

test('a failure sample with no rate recorded is a hole, and is called one', async () => {
  await withFile([
    { k: 'fail', sid: 'run-d', t: 1, sig: 'x', e: 'ix3:custom:6002', rate: 50 },
    { k: 'fail', sid: 'run-d', t: 2, sig: 'y', e: 'ix0:AccountInUse' },
  ], async (file) => {
    const r = await checkFiles([file]);
    assert.equal(r.failRows, 2);
    assert.equal(r.failRowsWithoutRate, 1);
  });
});

test('the same mint recorded twice is caught, within a file and across files', async () => {
  // 2026-08-11 has 116 duplicated mints and nothing ever said so.
  await withFile([coin({}, { mint: 'A' }), coin({}, { mint: 'A' })], async (file) => {
    const r = await checkFiles([file]);
    assert.equal(r.badRows, 1);
    assert.match(r.examples[0].bad.join(' '), /duplicate mint/);
  });
  await withFile([coin({}, { mint: 'Z' })], async (a) => {
    await withFile([coin({}, { mint: 'Z' })], async (b) => {
      assert.equal((await checkFiles([a, b])).badRows, 1);
    });
  });
});

test('rows written out of time order are counted', async () => {
  await withFile([coin({}, { mint: 'A', t: 100 }), coin({}, { mint: 'B', t: 50 })], async (file) => {
    assert.equal((await checkFiles([file])).outOfOrder, 1);
  });
});

test('a clean session file passes with nothing to say', async () => {
  await withFile([
    ...sessionRows('run-e'),
    coin({}, { mint: 'A', sid: 'run-e', t: 1, seq: 0, slot: 1, sig: 's1' }),
    coin({ complete: false, stopReason: 'shutdown', observedSec: 9 },
      { mint: 'B', sid: 'run-e', t: 2, seq: 1, slot: 2, sig: 's2' }),
  ], async (file) => {
    const r = await checkFiles([file]);
    assert.equal(r.badRows, 0);
    assert.equal(r.rowsWithoutSid, 0);
    assert.equal(r.outOfOrder, 0);
    assert.deepEqual(r.sessionsSplitAcrossFiles, []);
    assert.deepEqual(r.filesWithSeveralSessions, []);
  });
});

// ---------------------------------------------------------------------------
// The sell ledger, and the counters that have to fall out of it
// ---------------------------------------------------------------------------

const withSells = (sells, over = {}) => coin(
  { sells, creatorSellAtSec: null },
  { creator: null, total: { sellers: new Set(sells.map((s) => s[1])).size }, ...over },
);

test('a coin row with no sell ledger says a sell can be counted but not attributed', () => {
  const legacy = coin();
  delete legacy.outcome.sells;
  assert.match(checkRow(legacy).join(' '), /never attributed to a wallet/);
});

test('the candle count and the ledger have to agree', () => {
  const row = withSells([[1, 'A', 0.2, 10]], {
    market: { candleSeconds: 1, candles: [{ s: 1, sells: 3, buys: 0 }] },
  });
  assert.match(checkRow(row).join(' '), /candles count 3 sells but the ledger names 1/);
});

test('total.sellers has to be the distinct wallets in the ledger', () => {
  const row = withSells([[1, 'A', 0.2, 10], [2, 'A', 0.1, 5]]);
  row.total.sellers = 2; // two sells, but only one wallet
  assert.match(checkRow(row).join(' '), /says 2 but 1 distinct wallets sold/);
});

test('creatorSellAtSec has to be the first time the creator appears in the ledger', () => {
  const row = withSells([[4, 'DEV', 0.2, 10], [9, 'DEV', 0.1, 5]], { creator: 'DEV' });
  row.outcome.creatorSellAtSec = 9; // the second one, not the first
  assert.match(checkRow(row).join(' '), /creatorSellAtSec says 9 but the ledger says 4/);
  row.outcome.creatorSellAtSec = 4;
  assert.deepEqual(checkRow(row), []);
});

test('a creator who never sold reads as null on both sides', () => {
  const row = withSells([[4, 'SOMEBODY', 0.2, 10]], { creator: 'DEV' });
  assert.deepEqual(checkRow(row), []);
  row.outcome.creatorSellAtSec = 4;
  assert.match(checkRow(row).join(' '), /but the ledger says null/);
});

test('a truncated ledger stops the counts being read as exact', () => {
  const row = withSells([[1, 'A', 0.2, 10]], {
    total: { sellers: 40 },
    market: { candleSeconds: 1, candles: [{ s: 1, sells: 40, buys: 0 }] },
  });
  row.outcome.sellsCapped = true;
  // One complaint, not three. Once the ledger is a prefix every count over it
  // is a floor, and saying so three times would bury the one that matters.
  assert.deepEqual(checkSells(row, row.outcome).length, 1);
  assert.match(checkSells(row, row.outcome).join(' '), /prefix, not the window/);
});

test('a sell ledger out of order is caught', () => {
  assert.match(checkRow(withSells([[9, 'A', 0.2, 10], [4, 'B', 0.1, 5]])).join(' '), /out of order/);
});

// ---------------------------------------------------------------------------
// C21 in its general form
// ---------------------------------------------------------------------------

const footer = (over = {}) => ({
  k: 'stop', sid: 'S', t: 100, launches: 2, written: 2, truncated: 1,
  beats: 2, connectedBeats: 1, gaps: 1, gapMs: 500, failed: 10, failLogged: 2, trades: 77, ...over,
});

const backingRows = [
  { k: 'tick', sid: 'S', t: 10, connected: true },
  { k: 'tick', sid: 'S', t: 20, connected: false },
  { k: 'gap', sid: 'S', t: 30, ms: 500 },
  { k: 'failagg', sid: 'S', t: 40, n: 10, kept: 2 },
];
const coinTally = new Map([['S', { rows: 2, truncated: 1 }]]);

test('a footer whose every counter is backed by rows reports only the one that cannot be', () => {
  const out = unbackedCounters([...backingRows, footer()], coinTally);
  assert.deepEqual(out.map((u) => u.counter), ['trades']);
  assert.equal(out[0].found, null, 'one row per coin, not one per trade — so it cannot be rebuilt');
  assert.match(out[0].from, /one row per coin/);
});

test('a counter that disagrees with its rows is reported with both numbers', () => {
  // This is the shape of the defect the rule exists for: `stats.failed` counted
  // 645,741 failures and kept none of them, and nobody could check it.
  const out = unbackedCounters([...backingRows, footer({ failed: 645741 })], coinTally);
  const failed = out.find((u) => u.counter === 'failed');
  assert.equal(failed.said, 645741);
  assert.equal(failed.found, 10);
  assert.match(failed.from, /failagg/);
});

test('a footer that overstates its uptime is caught by the heartbeats', () => {
  const out = unbackedCounters([...backingRows, footer({ connectedBeats: 2 })], coinTally);
  assert.equal(out.find((u) => u.counter === 'connectedBeats').found, 1);
});

test('a footer claiming coins the file does not contain is caught', () => {
  const out = unbackedCounters([...backingRows, footer({ launches: 9 })], coinTally);
  assert.equal(out.find((u) => u.counter === 'launches').found, 2);
});

test('a file with no footer has no counters to check and says nothing', () => {
  assert.deepEqual(unbackedCounters(backingRows, coinTally), []);
});

test('counters from one session are never checked against another session rows', () => {
  const other = [{ k: 'tick', sid: 'OTHER', t: 10, connected: true }];
  const out = unbackedCounters([...backingRows, ...other, footer()], coinTally);
  assert.equal(out.some((u) => u.counter === 'beats'), false);
});

// ---------------------------------------------------------------------------
// The on-chain reserve anomaly — two independent routes to the same coins
// ---------------------------------------------------------------------------

test('a row that cannot see the fee rate cannot see the anomaly at all', () => {
  const legacy = coin();
  delete legacy.outcome.feeBps;
  assert.match(checkRow(legacy).join(' '), /zero-fee signature/);
});

test('a zero-fee trade is called out by name', () => {
  const row = coin({ feeBps: { 0: 3, 95: 40 }, zeroFee: [[1, 'A', 0.1, 5, 1, 44, 1e9, 14, 7e8, 0], [2, 'A', 0.1, 5, 1, 45, 1e9, 15, 7e8, 0], [3, 'A', 0.1, 5, 1, 46, 1e9, 16, 7e8, 0]] });
  assert.match(checkRow(row).join(' '), /3 trades paid zero fee/);
});

test('the zero-fee census must be backed by the zero-fee ledger', () => {
  const row = coin({ feeBps: { 0: 9 }, zeroFee: [[1, 'A', 0.1, 5, 1, 44, 1e9, 14, 7e8, 0]] });
  assert.match(checkRow(row).join(' '), /says 9 zero-fee trades but the ledger holds 1/);
});

test('the count and the flag an analyst reads have to fall out of the census', () => {
  // The same counter rule as everywhere else, at its smallest: `zeroFeeTrades`
  // and `curveSuspect` exist so nobody has to decode raw bytes a fortnight
  // later to learn this, and a number nobody can re-derive is how that happened.
  const lying = coin({ feeBps: { 0: 2, 95: 5 }, zeroFee: [[1, 'A', 0.1, 5, 1, 44, 1e9, 14, 7e8, 0], [2, 'A', 0.1, 5, 1, 45, 1e9, 15, 7e8, 0]], zeroFeeTrades: 7, curveSuspect: false });
  const bad = checkRow(lying).join(' ');
  assert.match(bad, /zeroFeeTrades says 7 but the fee census counts 2/);
  assert.match(bad, /curveSuspect is false with 2 zero-fee trades/);
});

test('a clean coin says so on both: no zero-fee trades, and not suspect', () => {
  const row = coin({ feeBps: { 95: 12 }, zeroFeeTrades: 0, curveSuspect: false });
  assert.deepEqual(checkRow(row), []);
});

test('a candle closing below the curve this coin opened at is a state the chain cannot reach', () => {
  // Nothing could ever check this, because the reserves were never on the candle.
  const row = coin({}, {
    curve: { virtualSol: 30, virtualTokens: 1.073e9, realTokens: 793_100_000 },
    market: { candleSeconds: 1, candles: [{ s: 0, sells: 0, vsol: 4.292, vtok: 1e9, rsol: 0, rtok: 7e8 }] },
  });
  assert.match(checkRow(row).join(' '), /below the 30 SOL this coin opened at/);
});

test('a coin that opened at 4.292 is not judged against somebody else\'s 30', () => {
  // 216 of the 7,926 recorded coins that carry a curve open at 4.292 virtual
  // SOL, not 30. A hardcoded floor calls every candle of every one of them
  // impossible — the row's own launch state is the floor, which is the same
  // rule `watch.js` follows in reading the opening state off the event.
  const row = coin({}, {
    curve: { virtualSol: 4.292, virtualTokens: 1.073e9, realTokens: 793_100_000 },
    market: { candleSeconds: 1, candles: [{ s: 0, sells: 0, vsol: 4.5, vtok: 1e9, rsol: 0.2, rtok: 7e8 }] },
  });
  assert.equal(checkRow(row).some((b) => /opened at/.test(b)), false);
  assert.equal(curveFloor(row), 4.292);
  assert.equal(curveFloor({}), CURVE_FLOOR_SOL, 'and 30 only when the row cannot say');
});

test('candles with no reserves at all are the corpus defect, and are named as such', () => {
  const row = coin({}, { market: { candleSeconds: 1, candles: [{ s: 0, o: 1, h: 1, l: 1, c: 1, sells: 0 }] } });
  assert.match(checkRow(row).join(' '), /only derived prices/);
});

test('a curve holding more tokens than it ever issued is impossible', () => {
  // The exact form of "more tokens left the curve than were bought out of it":
  // the curve issued 793.1M at launch, so `rtok` can only fall inside that.
  const row = coin({}, {
    curve: { virtualSol: 30, virtualTokens: 1.073e9, realTokens: 793_100_000 },
    market: { candleSeconds: 1, candles: [{ s: 0, sells: 0, vsol: 31, vtok: 1e9, rsol: 1, rtok: 900_000_000 }] },
  });
  assert.match(checkRow(row).join(' '), /more tokens in the curve than it ever issued/);
});

test('a curve inside its own issuance passes', () => {
  const row = coin({}, {
    curve: { virtualSol: 30, virtualTokens: 1.073e9, realTokens: 793_100_000 },
    market: { candleSeconds: 1, candles: [{ s: 0, sells: 0, vsol: 31, vtok: 1e9, rsol: 1, rtok: 700_000_000 }] },
  });
  assert.deepEqual(checkRow(row), []);
});

// ---------------------------------------------------------------------------
// The two fields that arrived in the same night as the counter rule and were
// never held to it: the state behind the entry price, and the fee that was paid
// ---------------------------------------------------------------------------

test('a curve state that does not imply the entry price above it is a contradiction', () => {
  // `entry` is a ratio and `curveAtEntry` is the absolute state behind it —
  // which is only a true sentence if `vsol / vtok` is `entry`. The recorder
  // reads both off the same event, so a row where they disagree has been
  // rewritten and a reader has no way to tell which half to believe.
  const row = coin({ curveAtEntry: [60, 1.073e9, 30, 700_000_000] });
  assert.match(checkRow(row).join(' '), /the price and the state behind it disagree/);
});

test('a coin nobody traded has no entry and no state behind it, and that is sound', () => {
  assert.deepEqual(checkRow(coin({ entry: null, curveAtEntry: null, peak: null })), []);
});

test('an entry price with no state behind it is a contradiction', () => {
  assert.match(checkRow(coin({ curveAtEntry: null })).join(' '), /curveAtEntry is null/);
});

test('a curve state with no entry price behind it is a contradiction', () => {
  assert.match(checkRow(coin({ entry: null })).join(' '), /no entry price was ever struck/);
});

test('a row that kept the reserves but not the state at entry is named', () => {
  // Only asked of rows that carry the candle reserves. The recorded corpus has
  // neither and is already told, once, that its candles kept only prices.
  const row = coin({}, { market: { candleSeconds: 1, candles: [{ s: 0, c: 3e-8, sells: 0, vsol: 30, vtok: 1e9, rsol: 0, rtok: 7e8 }] } });
  delete row.outcome.curveAtEntry;
  assert.match(checkRow(row).join(' '), /no curveAtEntry/);
  const legacy = { mint: 'M', t: 1, sid: 's', slot: 1, sig: 'x', outcome: { follow: 60, entry: 1 } };
  assert.equal(checkRow(legacy).some((b) => /curveAtEntry/.test(b)), false, 'a legacy row is not told twice');
});

test('a candle closing at a price its own reserves do not imply is a contradiction', () => {
  // The price is the reduction and the reserves are the state it was reduced
  // from. The whole reason for storing the second is that the first can be
  // rebuilt from it; if it cannot, one of the two is not what it says it is.
  const row = coin({}, {
    curve: { virtualSol: 30, virtualTokens: 1.073e9, realTokens: 793_100_000 },
    market: { candleSeconds: 1, candles: [{ s: 0, o: 3e-8, h: 3e-8, l: 3e-8, c: 9e-8, sells: 0, vsol: 31, vtok: 1e9, rsol: 1, rtok: 7e8 }] },
  });
  assert.match(checkRow(row).join(' '), /close at a price their own reserves do not imply/);
});

test('a candle whose close is exactly its own reserves passes', () => {
  const row = coin({}, {
    curve: { virtualSol: 30, virtualTokens: 1.073e9, realTokens: 793_100_000 },
    market: { candleSeconds: 1, candles: [{ s: 0, o: 3.1e-8, h: 3.1e-8, l: 3.1e-8, c: 31 / 1e9, sells: 0, vsol: 31, vtok: 1e9, rsol: 1, rtok: 7e8 }] },
  });
  assert.deepEqual(checkRow(row), []);
});

test('a candle carrying three of the four reserve fields is a half-applied change', () => {
  // The recorder writes all four keys and uses null when the extended block
  // would not decode, so three keys is never a layout the chain produced.
  const row = coin({}, {
    curve: { virtualSol: 30, virtualTokens: 1.073e9, realTokens: 793_100_000 },
    market: { candleSeconds: 1, candles: [{ s: 0, c: 31 / 1e9, sells: 0, vsol: 31, vtok: 1e9, rsol: 1 }] },
  });
  assert.match(checkRow(row).join(' '), /some of the reserve fields and not the others/);
});

test('a null reserve field is a decode that failed, not a missing field', () => {
  const row = coin({}, {
    curve: { virtualSol: 30, virtualTokens: 1.073e9, realTokens: 793_100_000 },
    market: { candleSeconds: 1, candles: [{ s: 0, c: 31 / 1e9, sells: 0, vsol: 31, vtok: 1e9, rsol: null, rtok: null }] },
  });
  assert.deepEqual(checkRow(row), []);
});

test('a fee larger than the SOL it was charged on is impossible at any rate', () => {
  // Loose on purpose: a tight bound would fire on the live anomaly the decoder
  // already knows about — a zero-SOL trade carrying a real fee. This one cannot
  // be crossed by any fee rate, and it is the line a lamports-for-SOL mix-up
  // lands a thousand times the wrong side of.
  const row = coin({ feeSol: 4 }, { total: { wallets: 2, sellers: 0, solIn: 1, solOut: 0.5, trades: 3 } });
  assert.match(checkRow(row).join(' '), /no fee rate can charge more than it was charged on/);
});

test('a fee inside the SOL it was charged on passes', () => {
  const row = coin({ feeSol: 0.014 }, { total: { wallets: 2, sellers: 0, solIn: 1, solOut: 0.5, trades: 3 } });
  assert.deepEqual(checkRow(row), []);
});

test('a negative fee is not a fee', () => {
  assert.match(checkRow(coin({ feeSol: -1 })).join(' '), /a fee is a non-negative number of SOL/);
});

test('a row that counts fee rates but never says what they cost is named', () => {
  const row = coin();
  delete row.outcome.feeSol;
  assert.match(checkRow(row).join(' '), /no feeSol/);
  const legacy = { mint: 'M', t: 1, sid: 's', slot: 1, sig: 'x', outcome: { follow: 60, entry: 1 } };
  assert.equal(checkRow(legacy).some((b) => /feeSol/.test(b)), false, 'a legacy row is not told twice');
});

test('the who-based token balance is reported, never used to fail a row', () => {
  // W32 reports this catches every coin peaking above 10x. Measured over
  // `who[]` on coins-2026-08-20 it does not: 7.0% of coins exceed 1.0 on a
  // smooth tail with no separation, and none of the four coins above 10x is
  // among them. A check that fires on 7% of ordinary coins for a reason nobody
  // can explain is a check that gets ignored.
  const row = coin({}, { whoCapped: false, who: [{ w: 'A', tin: 1000, tout: 9000 }] });
  assert.equal(tokenBalance(row).sound, false);
  assert.equal(checkRow(row).some((b) => /left the curve/.test(b)), false, 'counted, not failed on');
});

test('a wallet selling no more than it bought balances', () => {
  assert.equal(tokenBalance(coin({}, { who: [{ w: 'A', tin: 1000, tout: 900 }] })).sound, true);
});

test('past the wallet cap every sum is a floor, so the balance is not computed', () => {
  assert.equal(tokenBalance(coin({}, { whoCapped: true, who: [{ w: 'A', tin: 10, tout: 9000 }] })), null);
  const atCap = coin({}, { who: Array.from({ length: 200 }, (_, i) => ({ w: `w${i}`, tin: 1, tout: 9 })) });
  assert.equal(tokenBalance(atCap), null, 'a legacy row at exactly 200 says nothing about whether it was capped');
});

test('the file report counts the coins whose token balance does not hold', async () => {
  const bad = coin({}, { mint: 'A', who: [{ w: 'A', tin: 10, tout: 900 }] });
  const good = coin({}, { mint: 'B', who: [{ w: 'B', tin: 10, tout: 9 }] });
  await withFile([bad, good], async (file) => {
    const r = await checkFiles([file]);
    assert.equal(r.balanceChecked, 2);
    assert.equal(r.soldMoreThanBought, 1);
    assert.equal(r.badRows, 0, 'reported, not failed on');
  });
});

// ---------------------------------------------------------------------------
// Curve conservation — the route that works on rows written before any of this
// ---------------------------------------------------------------------------

/** A launch on pump's opening constants, with a peak and the wallets behind it. */
const curved = (peak, tin) => coin({ peak }, {
  curve: { virtualSol: 30, virtualTokens: 1.073e9, realTokens: 793_100_000 },
  who: [{ w: 'A', tin, tout: 0 }],
});

test('a peak that needs more tokens than anyone bought is arithmetically impossible', () => {
  // 4x the launch price halves the token side of the curve, so 536.5M tokens
  // have to have left it. 1,000 did. That peak is a quote, not a price anybody
  // could have sold into — and it is the whole reason the tail looked fat.
  const row = curved(4 * (30 / 1.073e9), 1_000);
  const c = curveConservation(row);
  assert.equal(c.sound, false);
  assert.ok(Math.abs(c.impliedOut - 536_500_000) < 1_000, `implied ${c.impliedOut}`);
  assert.match(checkRow(row).join(' '), /tokens out of the curve/);
});

test('a peak the buying actually paid for passes', () => {
  const row = curved(4 * (30 / 1.073e9), 600_000_000);
  assert.equal(curveConservation(row).sound, true);
  assert.deepEqual(checkRow(row), []);
});

test('the rule credits every buy in the window against the peak, so it only under-reports', () => {
  // Exactly enough, to the token. The peak may have happened in the first
  // second and the buying an hour later; this counts it anyway.
  const row = curved(4 * (30 / 1.073e9), 536_500_000);
  assert.equal(curveConservation(row).sound, true);
});

test('past the wallet cap the buy total is a floor, so conservation says nothing', () => {
  const capped = curved(9 * (30 / 1.073e9), 10);
  capped.whoCapped = true;
  assert.equal(curveConservation(capped), null);
  const atCap = coin({ peak: 9 * (30 / 1.073e9) }, {
    curve: { virtualSol: 30, virtualTokens: 1.073e9, realTokens: 793_100_000 },
    who: Array.from({ length: 200 }, (_, i) => ({ w: `w${i}`, tin: 1, tout: 0 })),
  });
  assert.equal(curveConservation(atCap), null);
});

test('a row with no launch curve, no peak or no wallets cannot answer', () => {
  assert.equal(curveConservation(coin({ peak: 1 })), null, 'no curve');
  assert.equal(curveConservation(curved(null, 10)), null, 'no peak');
  assert.equal(curveConservation(coin({ peak: 1 }, {
    curve: { virtualSol: 30, virtualTokens: 1.073e9 }, who: [],
  })), null, 'no wallets');
});

test('the file report splits conservation by how big the peak was', async () => {
  // A single count reads as background noise. The finding is the gradient, so
  // the report has to be able to show one.
  const at = (mult, tin) => ({
    mint: `m${mult}-${tin}`, t: 1, sid: 'abc-1', seq: 0, slot: 1, sig: 'S',
    curve: { virtualSol: 30, virtualTokens: 1.073e9, realTokens: 793_100_000 },
    who: [{ w: 'A', tin, tout: 0 }],
    outcome: {
      follow: 60, observedSec: 60, complete: true, stopReason: 'window', gapSec: 0,
      entry: 30 / 1.073e9, peak: mult * (30 / 1.073e9), peakMult: mult,
      highs: [], lows: [], highsCapped: false, lowsCapped: false,
      sells: [], sellsCapped: false, creatorSellAtSec: null,
      feeBps: { 95: 1 }, zeroFee: [], zeroFeeCapped: false,
    },
    market: { candleSeconds: 1, candles: [] },
  });
  await withFile([at(1.2, 1e9), at(1.2, 1), at(20, 1)], async (file) => {
    const r = await checkFiles([file]);
    const small = r.conservationByPeak.find((b) => b.label === '1–1.5x');
    const huge = r.conservationByPeak.find((b) => b.label === 'above 10x');
    assert.deepEqual([small.coins, small.impossible], [2, 1]);
    assert.deepEqual([huge.coins, huge.impossible], [1, 1]);
  });
});

test('conservation reads a legacy row — it needs nothing this recorder added', () => {
  // The point of keeping this route: `curve`, `outcome.peak` and `who[].tin`
  // have been on every row since long before `vsol` or `feeBps` existed, so the
  // recorded corpus can be graded by it today.
  const legacy = {
    mint: 'M', t: 1,
    curve: { virtualSol: 30, virtualTokens: 1.073e9, realTokens: 793_100_000 },
    outcome: { follow: 60, entry: 30 / 1.073e9, peak: 9 * (30 / 1.073e9) },
    who: [{ w: 'A', tin: 500, tout: 0 }],
  };
  assert.equal(curveConservation(legacy).sound, false);
});

// ---------------------------------------------------------------------------
// The schema version — the counter that could not show its own failure
// ---------------------------------------------------------------------------

test('the version moved when the shape did, and it is past the one five commits left behind', () => {
  // It sat at 2 while the record gained observedSec, complete, stopReason,
  // gapSec, sid, seq, slot, sig, si, four reserve fields a candle, curveAtEntry,
  // feeBps, zeroFee, feeSol, sells and five cap flags. A version that does not
  // move is a field that reads perfectly and cannot be wrong, which is the one
  // defect every other check here exists to catch.
  assert.ok(SCHEMA > 2, `SCHEMA is ${SCHEMA} and the shape has changed since 2`);
  assert.ok(KNOWN_SCHEMAS.has(SCHEMA));
});

test('a version this build has never met is refused, not graded', () => {
  const ahead = checkRow(coin({}, { v: SCHEMA + 1 })).join(' | ');
  assert.match(ahead, /newer recorder/);
  assert.match(checkRow(coin({}, { v: 0 })).join(' | '), /not a version this build knows/);
  assert.match(checkRow(coin({}, { v: 'three' })).join(' | '), /not a version this build knows/);
});

test('a row with no version at all is the recorded corpus, and still reads', () => {
  // The whole existing capture carries no `v`. Refusing it would be renumbering
  // the corpus by another name, and those files cannot be re-recorded.
  assert.equal(schemaStatus(undefined), 'legacy');
  assert.equal(schemaStatus(null), 'legacy');
  assert.deepEqual(checkRow(coin()), [], 'a versionless row is graded, not rejected');
});

test('a row at the current version passes, and one at an older known one too', () => {
  const modern = { v: SCHEMA, whoCapped: false };
  assert.deepEqual(checkRow(coin({ zeroFeeTrades: 0, curveSuspect: false }, modern)), []);
  assert.deepEqual(checkRow(coin({}, { v: 2 })), []);
});

test('the file report says which shapes it met and whether it knows them', async () => {
  await withFile([coin({}, { mint: 'A', v: SCHEMA, whoCapped: false }), coin({}, { mint: 'B' })], async (file) => {
    const r = await checkFiles([file]);
    const seen = Object.fromEntries(r.schemas.map((s) => [String(s.v), s.status]));
    assert.equal(seen[String(SCHEMA)], 'known');
    assert.equal(seen.null, 'legacy');
    assert.deepEqual(r.filesWithSeveralSchemas, [file], 'two shapes in one file is a defect');
  });
});

test('one file, one shape — a single version is not reported as a mixture', async () => {
  await withFile([coin({}, { mint: 'A', v: SCHEMA, whoCapped: false }),
    coin({}, { mint: 'B', v: SCHEMA, whoCapped: false })], async (file) => {
    const r = await checkFiles([file]);
    assert.deepEqual(r.filesWithSeveralSchemas, []);
    assert.equal(r.badRows, 0);
  });
});

// ---------------------------------------------------------------------------
// The fields nothing used to hold to anything
// ---------------------------------------------------------------------------

test('slotsAfter has to be the wallet own slot minus the launch slot', () => {
  // Called uncheckable. Both numbers are on the same row, so it is the most
  // checkable of the eight.
  const row = coin({}, { slot: 100, who: [{ w: 'A', slot: 103, slotsAfter: 3 }] });
  assert.deepEqual(checkUnheld(row), []);
  const adrift = coin({}, { slot: 100, who: [{ w: 'A', slot: 103, slotsAfter: 9 }] });
  assert.match(checkUnheld(adrift).join(' '), /slotsAfter is not their own slot/);
});

test('a wallet cannot buy a coin in a block before the one that created it', () => {
  const row = coin({}, { slot: 100, who: [{ w: 'A', slot: 98, slotsAfter: -2 }] });
  assert.match(checkUnheld(row).join(' '), /landed before the block that created/);
});

test('a wallet with no slotsAfter is left alone — only the openers carry one', () => {
  assert.deepEqual(checkUnheld(coin({}, { slot: 100, who: [{ w: 'A', in: 1 }] })), []);
});

test('from v3 a missing cap flag is a defect, not a shorter way of writing false', () => {
  // This is what bumping the version bought. While every shape shared one
  // number the presence rule could not be stated at all: absence and `false`
  // meant the same thing and no check could tell them apart.
  const full = coin({}, { v: SCHEMA, whoCapped: false });
  assert.deepEqual(checkUnheld(full), []);
  const { whoCapped, ...missing } = full;
  assert.match(checkUnheld(missing).join(' '), /whoCapped.*absent or not a boolean/);
  const noSells = coin({ sellsCapped: undefined }, { v: SCHEMA, whoCapped: false });
  delete noSells.outcome.sellsCapped;
  assert.match(checkUnheld(noSells).join(' '), /outcome\.sellsCapped/);
});

test('a legacy row is not told it is missing a flag that did not exist yet', () => {
  const legacy = { mint: 'M', t: 1, outcome: { follow: 60 } };
  assert.deepEqual(checkUnheld(legacy), []);
});

test('seq is a whole number from zero, and a v3 row has to carry one', () => {
  assert.match(checkUnheld(coin({}, { seq: -1 })).join(' '), /seq is -1/);
  assert.match(checkUnheld(coin({}, { seq: 1.5 })).join(' '), /seq is 1.5/);
  const { seq, ...noSeq } = coin({}, { v: SCHEMA, whoCapped: false });
  assert.match(checkUnheld(noSeq).join(' '), /no seq/);
});

test('si is a whole number from zero', () => {
  assert.deepEqual(checkUnheld(coin({}, { si: 0 })), []);
  assert.match(checkUnheld(coin({}, { si: -3 })).join(' '), /si is -3/);
});

test('seq has to advance within a session, and no two launches share a slot position', async () => {
  const at = (mint, seq, si) => coin({}, { mint, seq, si, slot: 500, whoCapped: false, v: SCHEMA });
  await withFile([at('A', 0, 0), at('B', 0, 1), at('C', 2, 1)], async (file) => {
    const r = await checkFiles([file]);
    assert.equal(r.seqOutOfOrder, 1, 'B repeats A seq');
    assert.equal(r.duplicateSlotPosition, 1, 'C sits where B already is');
  });
});

// ---------------------------------------------------------------------------
// Conservation on the SOL side — the form an independent check settled on
// ---------------------------------------------------------------------------

/** A launch on pump's opening constants, with a peak and the money behind it. */
const flowed = (peak, solIn) => coin({ peak }, {
  curve: { virtualSol: 30, virtualTokens: 1.073e9, realTokens: 793_100_000 },
  total: { wallets: 1, sellers: 0, solIn, solOut: 0, trades: 1 },
});

test('a peak that needs more SOL than ever entered the coin is impossible', () => {
  // 4x the launch price doubles the SOL side of the curve, so 30 SOL has to
  // have gone in. One did.
  const row = flowed(4 * (30 / 1.073e9), 1);
  const c = solConservation(row);
  assert.equal(c.sound, false);
  assert.ok(Math.abs(c.impliedSol - 30) < 0.01, `implied ${c.impliedSol}`);
  assert.match(checkRow(row).join(' '), /SOL into the curve/);
});

test('a peak the money actually paid for passes', () => {
  assert.equal(solConservation(flowed(4 * (30 / 1.073e9), 31)).sound, true);
});

test('the ceiling is gross inflow, so money that came in and left again still counts', () => {
  // The peak is transient. On net inflow the same test fires on 73% of
  // everything and means nothing.
  const row = flowed(4 * (30 / 1.073e9), 31);
  row.total.solOut = 30;
  assert.equal(solConservation(row).sound, true);
});

test('a fee-sized overshoot is forgiven, because solIn is what buyers paid, not what reached the curve', () => {
  const row = flowed(4 * (30 / 1.073e9), 30 / 1.005);
  assert.equal(solConservation(row).sound, true, 'inside the 1% trading fee');
  assert.equal(solConservation(flowed(4 * (30 / 1.073e9), 30 / 1.2)).sound, false, 'well past it');
});

test('the SOL form needs no who[], so the 200-wallet cap does not blind it', () => {
  // The token form has to refuse a capped row because every sum over `who[]` is
  // then a floor. `total.solIn` is summed over every buy regardless.
  const row = flowed(4 * (30 / 1.073e9), 1);
  row.who = Array.from({ length: 200 }, (_, i) => ({ w: `w${i}`, tin: 1e9, tout: 0 }));
  row.whoCapped = true;
  assert.equal(curveConservation(row), null, 'the token form bows out');
  assert.equal(solConservation(row).sound, false, 'this one still grades it');
});

test('a row from the lamports-per-base-unit era cannot reach the arithmetic', () => {
  // The reason this test was previously left out of the recorder was an
  // era-units rule: stored price is lamports per base unit on 2026-08-10
  // through 08-12 and whole units from 08-15. The precondition already enforces
  // it — not one row in those four files carries a `curve` block, so no
  // old-era price is ever divided by a modern reserve.
  const oldEra = { mint: 'M', t: 1, outcome: { follow: 60, peak: 1e3 }, total: { solIn: 1 } };
  assert.equal(solConservation(oldEra), null, 'no launch curve, no answer');
});

test('the file report splits the SOL form by peak too', async () => {
  const named = (mint, peak, solIn) => ({ ...flowed(peak, solIn), mint });
  await withFile([named('A', 1.2 * (30 / 1.073e9), 100), named('B', 1.2 * (30 / 1.073e9), 0.001),
    named('C', 20 * (30 / 1.073e9), 0.001)], async (file) => {
    const r = await checkFiles([file]);
    const small = r.solConservationByPeak.find((b) => b.label === '1–1.5x');
    const huge = r.solConservationByPeak.find((b) => b.label === 'above 10x');
    assert.deepEqual([small.coins, small.impossible], [2, 1]);
    assert.deepEqual([huge.coins, huge.impossible], [1, 1]);
  });
});
