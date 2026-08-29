// Defect 2 and half of defect 4: the tracker's second observation window.
//
// Every test here is named for the thing it stops coming back.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { Tracker, trackRow, checkTrackRow, LADDER } from '../src/track.js';

const LAUNCH = Date.UTC(2026, 7, 20, 10, 0, 0); // 2026-08-20T10:00:00Z

/** A coin record in the shape `watch.js`'s finish() writes it. */
function record(over = {}) {
  return {
    t: LAUNCH,
    mint: 'MintAAA',
    symbol: 'AAA',
    name: 'a test coin',
    creator: 'CreatorAAA',
    supply: 1_000_000_000,
    curve: { virtualSol: 30, virtualTokens: 1_073_000_000, realTokens: 793_100_000 },
    initialBuySol: 0.5,
    initialBuyTokens: 1000,
    social: { kind: 'tweet', handle: 'someone', nth: 1 },
    open: { seconds: 3, wallets: 4, sellers: 1, solIn: 2.5, solOut: 0.3, trades: 9 },
    who: [{ w: 'WalletA', at: 1, in: 1, out: 0 }, { w: 'WalletB', at: 40, in: 5, out: 0 }],
    outcome: {
      follow: 60,
      entry: 0.0001,
      peak: 0.00025,
      last: 0.00012,
      peakMult: 2.5,
      endMult: 1.2,
      // The first minute's peak. This is the value that used to leak.
      peakAtSec: 14,
      trades: 9,
    },
    ...over,
  };
}

const tracked = (over) => {
  const t = new Tracker();
  t.adopt(record(over));
  return t.get('MintAAA');
};

// ---------------------------------------------------------------------------
// Defect 2 — hi, lo, peakAtSec and cross reset together or not at all
// ---------------------------------------------------------------------------

test('adopt starts peakAtSec empty, alongside hi and lo', () => {
  const c = tracked();
  assert.equal(c.hi, 1);
  assert.equal(c.lo, 1);
  assert.equal(c.peakAtSec, null, 'the first minute peak must not be carried into the second window');
});

test('adopt does not carry a first-minute peakAtSec even when the coin record has one', () => {
  const c = tracked({ outcome: { ...record().outcome, peakAtSec: 47 } });
  assert.equal(c.peakAtSec, null);
});

test('adopt starts the cross ladder empty, like hi and lo', () => {
  assert.deepEqual({ ...tracked().cross }, {});
});

test('a row with hi 1 and a peakAtSec is the shape defect 2 produced', () => {
  const bad = { ...trackRow(tracked(), LAUNCH + 1000), peakAtSec: 14 };
  const complaints = checkTrackRow(bad);
  assert.equal(complaints.length, 1);
  assert.match(complaints[0], /never beat entry/);
});

test('a row with a hi above 1 and no peakAtSec is caught too', () => {
  const bad = { ...trackRow(tracked(), LAUNCH + 1000), hi: 1.8, peakAtSec: null };
  assert.match(checkTrackRow(bad)[0], /no peakAtSec says when/);
});

test('a freshly adopted coin that never trades again produces a sound row', () => {
  assert.deepEqual(checkTrackRow(trackRow(tracked(), LAUNCH + 60_000)), []);
});

test('hi and peakAtSec are set by the same trade, so they cannot disagree', () => {
  const t = new Tracker();
  t.adopt(record());
  t.trade('MintAAA', 0.00015, LAUNCH + 120_000); // 1.5x entry at second 120
  const row = trackRow(t.get('MintAAA'), LAUNCH + 130_000);
  assert.equal(row.hi, 1.5);
  assert.equal(row.peakAtSec, 120);
  assert.deepEqual(checkTrackRow(row), []);
});

test('peakAtSec is seconds from launch, not from the follow mark', () => {
  const t = new Tracker();
  t.adopt(record());
  t.trade('MintAAA', 0.0002, LAUNCH + 300_000);
  assert.equal(t.get('MintAAA').peakAtSec, 300);
});

test('a price that only ever falls leaves peakAtSec empty', () => {
  const t = new Tracker();
  t.adopt(record());
  t.trade('MintAAA', 0.00005, LAUNCH + 90_000);
  const c = t.get('MintAAA');
  assert.equal(c.hi, 1);
  assert.equal(c.lo, 0.5);
  assert.equal(c.peakAtSec, null);
});

// ---------------------------------------------------------------------------
// The property worth protecting: entry is never re-struck
// ---------------------------------------------------------------------------

test('adopt takes the three-second entry price straight off the coin record', () => {
  assert.equal(tracked().entry, 0.0001);
});

test('adopt does not re-base entry to the price at the follow mark', () => {
  const c = tracked();
  assert.notEqual(c.entry, record().outcome.last, 'entry must not become the 60-second price');
  assert.equal(c.last, 0.00012);
});

test('every multiple stays measured against what a strategy would have paid', () => {
  const t = new Tracker();
  t.adopt(record());
  t.trade('MintAAA', 0.0002, LAUNCH + 3600_000); // an hour later, 2x the 3s price
  assert.equal(t.get('MintAAA').hi, 2);
});

test('adopt falls back to a bare entry when there is no outcome block', () => {
  const t = new Tracker();
  t.adopt({ mint: 'M2', t: LAUNCH, entry: 0.5, open: {} });
  assert.equal(t.get('M2').entry, 0.5);
});

test('a coin with no measurable entry is watched but never scored', () => {
  const t = new Tracker();
  t.adopt({ mint: 'M3', t: LAUNCH, open: {} });
  assert.equal(t.trade('M3', 0.9, LAUNCH + 1000), true);
  const c = t.get('M3');
  assert.equal(c.entry, null);
  assert.equal(c.last, 0.9);
  assert.equal(c.hi, 1);
});

// ---------------------------------------------------------------------------
// Defect 4 — fields nobody ever filled in
// ---------------------------------------------------------------------------

test('adopt writes no score field', () => {
  assert.equal('score' in tracked(), false);
});

test('adopt writes no eligible field', () => {
  assert.equal('eligible' in tracked(), false);
});

test('a second argument to adopt cannot smuggle a score back in', () => {
  const t = new Tracker();
  t.adopt(record(), { score: 7, eligible: true });
  const c = t.get('MintAAA');
  assert.equal('score' in c, false);
  assert.equal('eligible' in c, false);
});

test('the written row carries neither score nor eligible', () => {
  const row = trackRow(tracked(), LAUNCH + 1000);
  assert.equal('score' in row, false);
  assert.equal('eligible' in row, false);
});

test('checkTrackRow rejects a row that carries them anyway', () => {
  const row = { ...trackRow(tracked(), LAUNCH + 1000), score: null, eligible: false };
  assert.match(checkTrackRow(row).join(' '), /never filled in/);
});

// ---------------------------------------------------------------------------
// The cross ladder
// ---------------------------------------------------------------------------

test('crossing up records the second it first happened', () => {
  const t = new Tracker();
  t.adopt(record());
  t.trade('MintAAA', 0.00016, LAUNCH + 100_000); // 1.6x
  assert.deepEqual({ ...t.get('MintAAA').cross }, { 1.25: 100, 1.5: 100 });
});

test('crossing down records the second it first happened', () => {
  const t = new Tracker();
  t.adopt(record());
  t.trade('MintAAA', 0.00006, LAUNCH + 200_000); // 0.6x
  assert.deepEqual({ ...t.get('MintAAA').cross }, { 0.95: 200, 0.85: 200, 0.7: 200 });
});

test('a rung is stamped once, by the first crossing and not the best', () => {
  const t = new Tracker();
  t.adopt(record());
  t.trade('MintAAA', 0.00016, LAUNCH + 100_000);
  t.trade('MintAAA', 0.00030, LAUNCH + 400_000);
  const cross = t.get('MintAAA').cross;
  assert.equal(cross[1.5], 100, 'the 1.5 rung keeps the time it was first reached');
  assert.equal(cross[2], 400);
});

test('the ladder only holds rungs that were actually reached', () => {
  const t = new Tracker();
  t.adopt(record());
  t.trade('MintAAA', 0.00011, LAUNCH + 100_000); // 1.1x, below the lowest up rung
  assert.deepEqual({ ...t.get('MintAAA').cross }, {});
  assert.ok(LADDER.includes(1.25));
});

test('a trade for a mint nobody adopted is not ours', () => {
  assert.equal(new Tracker().trade('Unknown', 1, LAUNCH), false);
});

// ---------------------------------------------------------------------------
// Writing it down
// ---------------------------------------------------------------------------

function tmpdir() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'capture-track-'));
}

test('a persisted line is JSON with no dead fields in it', () => {
  const dir = tmpdir();
  const t = new Tracker({ dir, save: true });
  t.adopt(record());
  t.close();
  const file = path.join(dir, 'tracks-2026-08-20.jsonl');
  const lines = fs.readFileSync(file, 'utf8').trim().split('\n');
  assert.equal(lines.length, 1);
  const row = JSON.parse(lines[0]);
  assert.equal('score' in row, false);
  assert.equal('eligible' in row, false);
  assert.equal(row.peakAtSec, null);
  assert.deepEqual(checkTrackRow(row), []);
  fs.rmSync(dir, { recursive: true, force: true });
});

test('the file is named for the day the coin launched, not the day it was written', () => {
  const dir = tmpdir();
  const t = new Tracker({ dir, save: true });
  t.adopt(record({ t: Date.UTC(2026, 7, 16, 23, 30, 0) }));
  t.close();
  assert.ok(fs.existsSync(path.join(dir, 'tracks-2026-08-16.jsonl')));
  fs.rmSync(dir, { recursive: true, force: true });
});

test('nothing is written when there is nowhere to write it', () => {
  const t = new Tracker({ dir: null, save: true });
  t.adopt(record());
  assert.doesNotThrow(() => t.close());
  assert.equal(t.save, false);
});

test('sweep drops coins past the horizon and writes what was learned', () => {
  const dir = tmpdir();
  const t = new Tracker({ dir, save: true, maxAgeMs: 1000 });
  t.adopt(record());
  t.trade('MintAAA', 0.0002, LAUNCH + 500);
  t.sweep(LAUNCH + 5000);
  assert.equal(t.size, 0);
  assert.equal(t.evicted, 1);
  const row = JSON.parse(fs.readFileSync(path.join(dir, 'tracks-2026-08-20.jsonl'), 'utf8').trim());
  assert.equal(row.hi, 2);
  assert.equal(row.peakAtSec, 1);
  fs.rmSync(dir, { recursive: true, force: true });
});

test('watchedSec is read off the clock it is handed', () => {
  assert.equal(trackRow(tracked(), LAUNCH + 7_200_000).watchedSec, 7200);
});

test('watchedSec never goes negative on a clock that stepped backwards', () => {
  assert.equal(trackRow(tracked(), LAUNCH - 5000).watchedSec, 0);
});

test('rows, get and size agree about what is being tracked', () => {
  const t = new Tracker();
  t.adopt(record());
  t.adopt(record({ mint: 'MintBBB' }));
  assert.equal(t.size, 2);
  assert.equal(t.rows().length, 2);
  assert.equal(t.get('MintBBB').mint, 'MintBBB');
  assert.equal(t.get('nope'), null);
});

test('the frozen opening travels with the coin', () => {
  const c = tracked();
  assert.deepEqual(c.open, { seconds: 3, solIn: 2.5, solOut: 0.3, sellers: 1 });
  assert.equal(c.who.length, 1, 'only wallets inside the opening cutoff');
  assert.equal(c.who[0].w, 'WalletA');
});
