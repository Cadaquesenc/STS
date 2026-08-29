// The whole recorder, end to end, with a socket that goes nowhere.
//
// Real pump.fun event bytes go in at the websocket and real files come out on
// disk. Nothing is stubbed between those two points: the borsh decode, the
// three-second freeze, the follow mark, the handover to the tracker, the
// twelve-hour window and the shutdown flush are all the shipping code.
//
// No network. No listener is started against anything real: `globalThis.
// WebSocket` is replaced for the duration of each test and put back after.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { watch, redact } from '../src/watch.js';
import { checkRow, checkFiles } from '../src/check.js';
import { jsonLine } from '../src/record.js';
import { checkTrackRow } from '../src/track.js';
import { SCHEMA } from '../src/session.js';
import { FakeSocket, install, address, createEvent, tradeEvent, sleep } from './fake.js';

const SECONDS = 0.2; // the opening freeze
const FOLLOW = 0.6; // the follow mark

/**
 * Start a recorder against a fake socket in a fresh directory.
 *
 * `save` is on because the files are the thing being tested. Tweets and the
 * funding RPC are off: both reach the network, and neither is what these tests
 * are about.
 */
function recorder(t, extra = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'capture-e2e-'));
  const restore = install();
  const w = watch({
    wsUrl: 'wss://endpoint.invalid/?api-key=secret',
    opts: {
      dir, save: true, tweets: false, rpc: false,
      seconds: SECONDS, follow: FOLLOW, socialWaitMs: 0, statusMs: 600_000,
      // Far longer than any test runs, so a test that wants heartbeats asks for
      // them and no other test has its coin rows diluted by them.
      heartbeatMs: 600_000,
      ...extra,
    },
    out: () => {},
    status: () => {},
  });
  FakeSocket.last.open();
  let stopped = false;
  // Registered up front, so a test that throws — an assertion, or a fake event
  // the chain could not have produced — still stops its recorder. Without this
  // one failing test leaves a live status interval and a live socket timer
  // behind, the runner never sees the process exit, and a plain failure
  // presents as the whole suite hanging with no output at all.
  t?.after(async () => {
    if (!stopped) await w.stop();
    restore();
    fs.rmSync(dir, { recursive: true, force: true });
  });
  return {
    dir,
    w,
    // A getter, not the socket we opened: a reconnect replaces it, and a test
    // that held the old one would be talking to a socket the recorder has
    // already let go of.
    get sock() { return FakeSocket.last; },
    read(name) {
      const file = fs.readdirSync(dir).find((f) => f.startsWith(name));
      if (!file) return [];
      return fs.readFileSync(path.join(dir, file), 'utf8')
        .split('\n').filter(Boolean).map((l) => JSON.parse(l));
    },
    /**
     * Start the shutdown without waiting for it, so a test can do something
     * while the drain is running.
     */
    begin(stopOpts) {
      const done = w.stop(stopOpts);
      return done.then(() => { stopped = true; });
    },
    async finish(stopOpts) {
      await this.begin(stopOpts);
      // The coin log now also carries the run's own start / tick / gap /
      // failagg / stop rows. They carry a `k` and coin rows do not, which is
      // the one test every reader needs.
      const all = this.read('coins-');
      return {
        coins: all.filter((r) => !r.k),
        meta: all.filter((r) => r.k),
        tracks: this.read('tracks-'),
        fails: this.read('fails-'),
      };
    },
  };
}

/** A launch, a first trade that sets the entry price, and nothing else yet. */
/**
 * `sol` is what the opening buy paid. It defaults to the 0.5 SOL most tests
 * here want and is passed explicitly by the ones that walk the curve a long way
 * up: reaching 45 virtual SOL means 15 SOL went in, and saying it went in on
 * half a SOL is a coin whose peak no money could have produced — which is what
 * `solConservation` fails a row for, correctly.
 */
function launch(r, seed, { virtualSol = 30, slot = 1, tradeSlot = null, sol = 0.5 } = {}) {
  const mint = address(`mint:${seed}`);
  const user = address(`user:${seed}`);
  r.sock.notify(`sig-create-${seed}`, [createEvent({ mint, user, symbol: seed.toUpperCase() })], { slot });
  r.sock.notify(`sig-open-${seed}`, [tradeEvent({ mint, user, sol, virtualSol })], { slot: tradeSlot ?? slot });
  return { mint, user };
}

/**
 * Drop the socket and bring it back, the way a real one does.
 *
 * The reconnect is on a randomised backoff inside `ws.js`, so this waits for
 * the replacement socket rather than assuming when it appears.
 */
async function drop(r, ms = 4000) {
  const before = FakeSocket.last;
  before.fire('close');
  const until = Date.now() + ms;
  while (FakeSocket.last === before && Date.now() < until) await sleep(20);
  assert.notEqual(FakeSocket.last, before, 'the socket never reconnected');
  FakeSocket.last.open();
}

const trade = (r, seed, coin, virtualSol, n, sol = 0.1) =>
  r.sock.notify(`sig-${seed}-${n}`, [tradeEvent({ ...coin, sol, virtualSol })]);

// ---------------------------------------------------------------------------

test('a launch, a minute of trades and a shutdown produce both files', async (t) => {
  const r = recorder(t);
  const coin = launch(r, 'aaa');
  await sleep(FOLLOW * 1000 + 250);
  const { coins, tracks } = await r.finish();

  assert.equal(coins.length, 1);
  assert.equal(coins[0].mint, coin.mint);
  assert.equal(coins[0].symbol, 'AAA');
  assert.equal(tracks.length, 1, 'the tracker writes its own file — the old CLI silently did not');
  assert.equal(tracks[0].mint, coin.mint);
});

test('a coin that peaked in its first minute and never again writes no peakAtSec', async (t) => {
  const r = recorder(t);
  // Entry is struck at 0.2s. Push the price up before the follow mark, then stop
  // trading entirely. This is the exact shape that produced 929 impossible rows.
  const coin = launch(r, 'bbb');
  await sleep(SECONDS * 1000 + 60);
  trade(r, 'bbb', coin, 45, 1); // 1.5x, inside the first window
  await sleep(FOLLOW * 1000);
  const { coins, tracks } = await r.finish();

  assert.notEqual(coins[0].outcome.peakAtSec, null, 'the coin record does keep the first-minute peak');
  assert.ok(coins[0].outcome.peakMult > 1);
  assert.equal(tracks[0].hi, 1, 'nothing beat entry after the follow mark');
  assert.equal(tracks[0].peakAtSec, null, 'so nothing may claim to say when it did');
  assert.deepEqual(checkTrackRow(tracks[0]), []);
});

test('a coin that runs after the follow mark gets a peak time from that window', async (t) => {
  const r = recorder(t);
  const coin = launch(r, 'ccc');
  await sleep(FOLLOW * 1000 + 150);
  trade(r, 'ccc', coin, 60, 1); // 2x, after the handover
  await sleep(60);
  const { tracks } = await r.finish();

  assert.ok(tracks[0].hi > 1.9, `hi was ${tracks[0].hi}`);
  assert.notEqual(tracks[0].peakAtSec, null);
  assert.deepEqual(checkTrackRow(tracks[0]), []);
});

test('the tracks entry price is the coin record entry price, never re-struck', async (t) => {
  const r = recorder(t);
  const coin = launch(r, 'ddd');
  await sleep(SECONDS * 1000 + 60);
  trade(r, 'ddd', coin, 45, 1); // the price moves before the follow mark
  await sleep(FOLLOW * 1000);
  const { coins, tracks } = await r.finish();

  assert.equal(tracks[0].entry, coins[0].outcome.entry);
  assert.notEqual(tracks[0].entry, coins[0].outcome.last, 'not the 60-second price');
});

test('a multiple an hour later is still measured against what a buyer would have paid', async (t) => {
  const r = recorder(t);
  const coin = launch(r, 'eee');
  await sleep(FOLLOW * 1000 + 150);
  trade(r, 'eee', coin, 90, 1); // the curve's SOL side triples
  await sleep(60);
  const { coins, tracks } = await r.finish();

  // Price is virtualSol / virtualTokens and their product is fixed, so tripling
  // the SOL side thirds the token side and the price goes up nine times, not
  // three. The fixture used to move one side alone, which is a curve state the
  // chain cannot produce — and this is the arithmetic that says so.
  const expected = (90 / 30) ** 2;
  assert.ok(Math.abs(tracks[0].hi - expected) < 0.01, `hi was ${tracks[0].hi}, wanted ${expected}`);
  assert.ok(Math.abs(tracks[0].last / coins[0].outcome.entry - expected) < 0.01);
});

test('the cross ladder stamps the second each multiple was first reached', async (t) => {
  const r = recorder(t);
  const coin = launch(r, 'fff');
  await sleep(FOLLOW * 1000 + 150);
  // 36.7 SOL on the curve is (36.7/30)² = 1.50x the launch price. The SOL side
  // and the price are not the same number: the product is what is fixed.
  trade(r, 'fff', coin, 36.75, 1);
  await sleep(60);
  const { tracks } = await r.finish();

  assert.ok('1.5' in tracks[0].cross, `cross was ${JSON.stringify(tracks[0].cross)}`);
  assert.ok('1.25' in tracks[0].cross);
  assert.equal('2' in tracks[0].cross, false, 'a rung that was never reached is absent, not zero');
});

test('no written row carries score or eligible', async (t) => {
  const r = recorder(t);
  launch(r, 'ggg');
  await sleep(FOLLOW * 1000 + 250);
  const { coins, tracks } = await r.finish();

  for (const row of [...coins, ...tracks]) {
    assert.equal('score' in row, false);
    assert.equal('eligible' in row, false);
  }
});

test('every written row passes the producer own checks', async (t) => {
  const r = recorder(t);
  // Two coins, one busy and one that never trades again. Entry is struck part
  // way up the curve so the busy one can fall below it later without asking for
  // a curve state below the 30 SOL floor.
  // The money matches the move on both: 45 virtual SOL is 15 SOL in, 60 is 30.
  const a = launch(r, 'hhh', { virtualSol: 45, sol: 15 });
  launch(r, 'iii');
  await sleep(SECONDS * 1000 + 60);
  trade(r, 'hhh', a, 60, 1, 15); // up, inside the first window
  await sleep(FOLLOW * 1000 + 150);
  trade(r, 'hhh', a, 33, 2); // and down, after the handover
  await sleep(60);
  const { coins, tracks } = await r.finish();

  assert.equal(coins.length, 2);
  assert.equal(tracks.length, 2);
  const busy = tracks.find((row) => row.mint === a.mint);
  assert.ok(busy.lo < 1, `the fall after the follow mark should show, lo was ${busy.lo}`);
  assert.equal(busy.hi, 1, 'and nothing beat entry in that window');
  assert.equal(busy.peakAtSec, null);
  for (const row of [...coins, ...tracks]) assert.deepEqual(checkRow(row), []);
});

test('a coin still inside its window at shutdown is written, not dropped', async (t) => {
  const r = recorder(t);
  launch(r, 'jjj');
  await sleep(50); // nowhere near the follow mark
  const { coins, tracks } = await r.finish();

  assert.equal(coins.length, 1, 'a short record is a fact, a missing one is a hole');
  assert.equal(tracks.length, 1);
});

test('the opening is frozen at the seconds mark and nothing later leaks into it', async (t) => {
  const r = recorder(t);
  const coin = launch(r, 'kkk');
  await sleep(SECONDS * 1000 + 80);
  // A second wallet arrives after the freeze. It belongs in `total` and in
  // `who`, and must not appear in `open`.
  const latecomer = address('user:latecomer');
  r.sock.notify('sig-kkk-late', [tradeEvent({ mint: coin.mint, user: latecomer, sol: 2, virtualSol: 40 })]);
  await sleep(FOLLOW * 1000);
  const { coins } = await r.finish();

  assert.equal(coins[0].open.wallets, 1);
  assert.equal(coins[0].total.wallets, 2);
  assert.equal(coins[0].who.length, 2);
  assert.equal(coins[0].who.filter((w) => w.at <= coins[0].open.seconds).length, 1);
});

test('a trade for a mint we never saw launch is not invented', async (t) => {
  const r = recorder(t);
  const stranger = address('mint:stranger');
  r.sock.notify('sig-stranger', [tradeEvent({ mint: stranger, user: address('user:x'), virtualSol: 31 })]);
  await sleep(80);
  const { coins, tracks } = await r.finish();
  assert.equal(coins.length, 0);
  assert.equal(tracks.length, 0);
});

test('a redelivered signature after a reconnect is not counted twice', async (t) => {
  const r = recorder(t);
  const mint = address('mint:lll');
  const user = address('user:lll');
  const payload = [createEvent({ mint, user, symbol: 'LLL' })];
  r.sock.notify('sig-lll', payload);
  r.sock.notify('sig-lll', payload); // the same signature again
  await sleep(FOLLOW * 1000 + 250);
  const { coins } = await r.finish();
  assert.equal(coins.length, 1);
});

test('a failed transaction is not recorded as a launch', async (t) => {
  const r = recorder(t);
  const mint = address('mint:mmm');
  r.sock.notify('sig-mmm', [createEvent({ mint, user: address('user:mmm') })], { err: { InstructionError: [3, 'x'] } });
  await sleep(FOLLOW * 1000 + 150);
  const { coins } = await r.finish();
  assert.equal(coins.length, 0);
  // Still true, and still defect 7: it is dropped without a trace. See the
  // README's list of what W21 asks for and this producer does not do.
});

test('a log line that is not a pump event is ignored without complaint', async (t) => {
  const r = recorder(t);
  r.sock.deliver({
    jsonrpc: '2.0', method: 'logsNotification',
    params: { result: { context: { slot: 1 }, value: { signature: 'sig-noise', err: null, logs: ['Program log: hello', 'Program data: bm90IGFuIGV2ZW50'] } } },
  });
  await sleep(80);
  const { coins } = await r.finish();
  assert.equal(coins.length, 0);
});

test('the endpoint key never reaches anything that gets written down', () => {
  assert.equal(redact('wss://x.example/?api-key=secret'), 'wss://x.example/?api-key=***');
  assert.equal(redact('not a url'), 'invalid-url');
});

test('the subscribe request asks for the pump program and nothing else', async (t) => {
  const r = recorder(t);
  const sent = JSON.parse(r.sock.sent[0]);
  assert.equal(sent.method, 'logsSubscribe');
  assert.deepEqual(sent.params[0].mentions, ['6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P']);
  await r.finish();
});

// ---------------------------------------------------------------------------
// Defect 1 — a record that says how long it was really watched
// ---------------------------------------------------------------------------

test('a coin that ran its whole window is marked complete, with the seconds to prove it', async (t) => {
  const r = recorder(t);
  launch(r, 'w1');
  await sleep(FOLLOW * 1000 + 250);
  const { coins } = await r.finish();

  const o = coins[0].outcome;
  assert.equal(o.complete, true);
  assert.equal(o.stopReason, 'window');
  assert.equal(o.gapSec, 0);
  assert.equal(o.follow, FOLLOW, 'the window it was promised is still on the row...');
  assert.ok('observedSec' in o, '...beside what it actually got');
});

test('a coin cut off by shutdown says so, instead of claiming the full window', async (t) => {
  const r = recorder(t);
  launch(r, 'w2');
  await sleep(50); // nowhere near the follow mark
  const { coins } = await r.finish();

  const o = coins[0].outcome;
  assert.equal(o.complete, false, 'this is the ~14% of the corpus that was indistinguishable');
  assert.equal(o.stopReason, 'shutdown');
  assert.ok(o.observedSec < o.follow, `observed ${o.observedSec}s of a ${o.follow}s window`);
});

test('the cut-off record and the whole one are no longer the same row', async (t) => {
  const r = recorder(t);
  launch(r, 'w3a');
  await sleep(FOLLOW * 1000 + 200);
  launch(r, 'w3b'); // still inside its window when the recorder stops
  await sleep(50);
  const { coins } = await r.finish();

  const done = coins.find((c) => c.symbol === 'W3A');
  const cut = coins.find((c) => c.symbol === 'W3B');
  assert.equal(done.outcome.complete, true);
  assert.equal(cut.outcome.complete, false);
  // Under the old shape these two rows carried an identical `follow: 60` and
  // nothing else about the window, so every downstream average mixed them.
  assert.equal(done.outcome.follow, cut.outcome.follow);
  assert.notEqual(done.outcome.stopReason, cut.outcome.stopReason);
});

test('a socket that dropped inside a coin window leaves gapSec on that coin', async (t) => {
  const r = recorder(t, { follow: 3 });
  launch(r, 'w4');
  await drop(r);
  await sleep(3_200);
  const { coins, meta } = await r.finish();

  const o = coins[0].outcome;
  assert.ok(o.gapSec >= 1, `the feed was down inside this window; gapSec was ${o.gapSec}`);
  assert.equal(o.complete, false, 'the timer fired on time, but nobody was watching for part of it');
  assert.ok(['window', 'socket-down'].includes(o.stopReason));
  const gaps = meta.filter((m) => m.k === 'gap');
  assert.equal(gaps.length, 1, 'and the outage itself is a row, not just a counter');
  assert.ok(gaps[0].ms > 0);
});

test('a coin that launched after the feed came back is not charged for the outage', async (t) => {
  const r = recorder(t, { follow: 2 });
  await drop(r);
  launch(r, 'w5');
  await sleep(2_200);
  const { coins } = await r.finish();

  assert.equal(coins[0].outcome.gapSec, 0, 'downtime before it existed is not its downtime');
  assert.equal(coins[0].outcome.complete, true);
});

test('draining lets the coins already inside their window finish, whole', async (t) => {
  const r = recorder(t);
  launch(r, 'w6');
  await sleep(50);
  // Without the drain this coin is the shutdown case above: recorded, labelled,
  // and 200 milliseconds long. With it, it is a whole observation.
  const { coins } = await r.finish({ drainMs: FOLLOW * 1000 + 500 });

  assert.equal(coins.length, 1);
  assert.equal(coins[0].outcome.complete, true, 'the drain turned a truncated record into a real one');
  assert.equal(coins[0].outcome.stopReason, 'window');
});

test('a launch arriving during the drain is refused rather than half-watched', async (t) => {
  const r = recorder(t);
  const stopping = r.begin({ drainMs: 300 });
  launch(r, 'w7');
  await stopping;
  const all = r.read('coins-');
  assert.equal(all.filter((x) => !x.k).length, 0, 'a coin nobody could finish is not recorded');
  assert.equal(r.w.totals.refused, 1, 'it is counted, though — a refusal is not a silence');
});

// ---------------------------------------------------------------------------
// Session identity, heartbeats, and one file per run
// ---------------------------------------------------------------------------

test('every coin row carries the session that recorded it, and its place in the run', async (t) => {
  const r = recorder(t);
  launch(r, 's1a');
  launch(r, 's1b');
  await sleep(FOLLOW * 1000 + 250);
  const { coins, tracks } = await r.finish();

  for (const c of coins) assert.equal(c.sid, r.w.sid);
  assert.deepEqual(coins.map((c) => c.seq).sort(), [0, 1]);
  for (const row of tracks) assert.equal(row.sid, r.w.sid, 'the tracks row outlives the coin by 12h and still knows');
});

test('the file is named for the session, so a run that crosses midnight stays one file', async (t) => {
  const r = recorder(t);
  launch(r, 's2');
  await sleep(FOLLOW * 1000 + 250);
  await r.finish();

  const files = fs.readdirSync(r.dir);
  assert.equal(files.filter((f) => f.startsWith('coins-')).length, 1);
  assert.ok(files.includes(`coins-${r.w.session}.jsonl`), files.join(', '));
  assert.ok(files.includes(`tracks-${r.w.session}.jsonl`), files.join(', '));
  // Nothing in the name comes from the clock at the time of the write. The old
  // naming took the date then, which is why one fifteen-hour capture became
  // "2026-08-20" plus a "held-out" 2026-08-21 that six analyses believed in.
  assert.equal(/\d{4}-\d{2}-\d{2}\.jsonl$/.test(`coins-${r.w.session}.jsonl`), false);
});

test('a session opens with a header naming every bound it will record under', async (t) => {
  const r = recorder(t);
  await sleep(20);
  const { meta } = await r.finish();

  const start = meta.find((m) => m.k === 'start');
  assert.equal(start.sid, r.w.sid);
  assert.ok(start.v >= 1, 'the schema version, so a reader never guesses from field presence');
  assert.equal(start.policy.follow, FOLLOW);
  assert.equal(start.policy.seconds, SECONDS);
  assert.equal(start.policy.failSample, 50, 'a sample rate that is not written down is a hole');
  assert.ok('highsCap' in start.policy && 'whoCap' in start.policy);
  assert.equal(start.endpoint.includes('secret'), false, 'and never the api key');
});

test('the heartbeat is written whether or not anything happened', async (t) => {
  const r = recorder(t, { heartbeatMs: 60 });
  await sleep(320); // no launches at all, just a quiet market
  const { meta } = await r.finish();

  const beats = meta.filter((m) => m.k === 'tick');
  assert.ok(beats.length >= 3, `a quiet run still proves it was running; got ${beats.length} beats`);
  assert.equal(beats[0].connected, true);
  assert.equal(beats[0].sid, r.w.sid);
});

test('a heartbeat during an outage says the feed was down, so uptime is measurable', async (t) => {
  const r = recorder(t, { heartbeatMs: 60 });
  const before = FakeSocket.last;
  before.fire('close');
  await sleep(200); // beats keep coming while the socket is away
  const { meta } = await r.finish();

  const beats = meta.filter((m) => m.k === 'tick');
  assert.ok(beats.some((b) => b.connected === false), 'an outage and a quiet market are now different files');
});

test('a session closes with a footer whose every counter is backed by rows', async (t) => {
  const r = recorder(t, { heartbeatMs: 60 });
  launch(r, 's3');
  await sleep(FOLLOW * 1000 + 250);
  const { meta, coins } = await r.finish();

  const stop = meta.find((m) => m.k === 'stop');
  assert.equal(stop.sid, r.w.sid);
  assert.equal(stop.launches, coins.length, 'launches are countable from the coin rows');
  assert.equal(stop.beats, meta.filter((m) => m.k === 'tick').length);
  assert.equal(stop.gaps, meta.filter((m) => m.k === 'gap').length);
  assert.equal(stop.truncated, coins.filter((c) => !c.outcome.complete).length);
  assert.equal(stop.uptime, 1);
});

// ---------------------------------------------------------------------------
// The cost block — slot and signature, so the fee can be looked up later
// ---------------------------------------------------------------------------

test('a coin record carries the launch signature and the slot it landed in', async (t) => {
  const r = recorder(t);
  launch(r, 'c1', { slot: 4242 });
  await sleep(FOLLOW * 1000 + 250);
  const { coins } = await r.finish();

  assert.equal(coins[0].sig, 'sig-create-c1');
  assert.equal(coins[0].slot, 4242);
  assert.equal(coins[0].si, 0, 'first pump transaction we saw in that slot');
  assert.equal(typeof coins[0].connectedForSec, 'number');
});

test('the wallets that got in at the open carry their own signature and landing distance', async (t) => {
  const r = recorder(t);
  launch(r, 'c2', { slot: 100, tradeSlot: 103 });
  await sleep(FOLLOW * 1000 + 250);
  const { coins } = await r.finish();

  const opener = coins[0].who[0];
  assert.equal(opener.sig, 'sig-open-c2');
  assert.equal(opener.slot, 103);
  assert.equal(opener.slotsAfter, 3, 'three slots behind the launch — this is the fee ladder');
});

test('a wallet arriving after the opening cutoff is not given cost fields it cannot use', async (t) => {
  const r = recorder(t);
  const coin = launch(r, 'c3');
  await sleep(SECONDS * 1000 + 80);
  r.sock.notify('sig-late-c3', [tradeEvent({ mint: coin.mint, user: address('user:late-c3'), virtualSol: 40 })], { slot: 9 });
  await sleep(FOLLOW * 1000);
  const { coins } = await r.finish();

  const late = coins[0].who.find((w) => w.at > coins[0].open.seconds);
  assert.equal('sig' in late, false, 'the ladder is about the positions a strategy competes for');
  assert.equal('slotsAfter' in late, false);
});

test('two transactions in one slot are ordered, and a new slot starts again at zero', async (t) => {
  const r = recorder(t);
  launch(r, 'c4', { slot: 7 }); // create then open trade, both in slot 7
  launch(r, 'c5', { slot: 8 });
  await sleep(FOLLOW * 1000 + 250);
  const { coins } = await r.finish();

  assert.equal(coins.find((c) => c.symbol === 'C4').si, 0);
  assert.equal(coins.find((c) => c.symbol === 'C4').who[0].si, 1, 'second pump transaction in slot 7');
  assert.equal(coins.find((c) => c.symbol === 'C5').si, 0);
});

// ---------------------------------------------------------------------------
// Failed transactions — recorded, not tallied
// ---------------------------------------------------------------------------

test('a failed transaction is written down, with the kind of failure it was', async (t) => {
  const r = recorder(t, { failSample: 1 });
  r.sock.notify('sig-fail-1', [], { err: { InstructionError: [3, { Custom: 6002 }] } });
  await sleep(60);
  const { fails, coins } = await r.finish();

  assert.equal(coins.length, 0, 'still not a launch');
  assert.equal(fails.length, 1, 'but no longer thrown away either');
  assert.equal(fails[0].sig, 'sig-fail-1');
  assert.equal(fails[0].e, 'ix3:custom:6002', 'pump slippage: somebody was outbid');
  assert.equal(fails[0].rate, 1);
});

test('failures go in their own file, so every pass over the coins is not fifteen times longer', async (t) => {
  const r = recorder(t, { failSample: 1 });
  r.sock.notify('sig-fail-2', [], { err: 'BlockhashNotFound' });
  launch(r, 'f1');
  await sleep(FOLLOW * 1000 + 250);
  await r.finish();

  const files = fs.readdirSync(r.dir);
  assert.ok(files.some((f) => f.startsWith('fails-')), files.join(', '));
  const coinFile = fs.readFileSync(path.join(r.dir, `coins-${r.w.session}.jsonl`), 'utf8');
  assert.equal(coinFile.includes('sig-fail-2'), false);
});

test('the sample rate is on every kept row, because a sample without one is a hole', async (t) => {
  const r = recorder(t, { failSample: 3 });
  for (let i = 0; i < 200; i++) r.sock.notify(`sig-s-${i}`, [], { err: 'BlockhashNotFound' });
  await sleep(60);
  const { fails } = await r.finish();

  assert.ok(fails.length > 0 && fails.length < 200, `kept ${fails.length} of 200 at 1 in 3`);
  for (const f of fails) assert.equal(f.rate, 3);
});

test('the totals stay exact even though only a sample of the rows is kept', async (t) => {
  const r = recorder(t, { failSample: 10 });
  for (let i = 0; i < 120; i++) r.sock.notify(`sig-agg-${i}`, [], { err: { InstructionError: [3, { Custom: 6002 }] } });
  await sleep(60);
  const { meta, fails } = await r.finish();

  const agg = meta.filter((m) => m.k === 'failagg');
  const counted = agg.reduce((a, m) => a + m.n, 0);
  assert.equal(counted, 120, 'the headline rate is reproducible from the file, not from a counter');
  assert.equal(agg.reduce((a, m) => a + m.kept, 0), fails.length);
  assert.equal(agg[0].byErr['ix3:custom:6002'], 120, 'and the error census is in there too');
});

test('a failure redelivered after a reconnect is counted once, not twice', async (t) => {
  // The old code incremented the counter before the signature dedup, so every
  // published failure rate was an upper bound by an amount nobody could measure.
  const r = recorder(t, { failSample: 1 });
  r.sock.notify('sig-dup', [], { err: 'BlockhashNotFound' });
  r.sock.notify('sig-dup', [], { err: 'BlockhashNotFound' });
  await sleep(60);
  const { fails, meta } = await r.finish();

  assert.equal(fails.length, 1);
  assert.equal(meta.filter((m) => m.k === 'failagg').reduce((a, m) => a + m.n, 0), 1);
  assert.equal(r.w.totals.failed, 1);
});

test('a burst of failures cannot evict the successful signatures the dedup needs', async (t) => {
  const r = recorder(t, { failSample: 50 });
  const mint = address('mint:f2');
  const user = address('user:f2');
  const payload = [createEvent({ mint, user, symbol: 'F2' })];
  r.sock.notify('sig-f2', payload);
  for (let i = 0; i < 500; i++) r.sock.notify(`sig-burst-${i}`, [], { err: 'BlockhashNotFound' });
  r.sock.notify('sig-f2', payload); // the same launch, redelivered
  await sleep(FOLLOW * 1000 + 250);
  const { coins } = await r.finish();

  assert.equal(coins.length, 1, 'failures have their own dedup set for exactly this reason');
});

test('turning the failure log off still leaves the rate in the file', async (t) => {
  const r = recorder(t, { failLog: false });
  r.sock.notify('sig-off', [], { err: 'BlockhashNotFound' });
  await sleep(60);
  const { meta } = await r.finish();

  assert.equal(fs.readdirSync(r.dir).some((f) => f.startsWith('fails-')), false);
  assert.equal(meta.filter((m) => m.k === 'failagg').reduce((a, m) => a + m.n, 0), 1);
});

// ---------------------------------------------------------------------------
// The turning-point cap
// ---------------------------------------------------------------------------

test('the running extreme keeps moving after the turning-point list is full', async (t) => {
  const r = recorder(t, { highsCap: 3 });
  const coin = launch(r, 'h1');
  await sleep(SECONDS * 1000 + 60);
  // Six new highs, one after another, against a list with room for three.
  for (let i = 1; i <= 6; i++) trade(r, 'h1', coin, 30 + i * 5, i);
  await sleep(FOLLOW * 1000);
  const { coins } = await r.finish();

  const o = coins[0].outcome;
  assert.equal(o.highs.length, 3, 'the list stopped');
  assert.equal(o.highsCapped, true, 'and the row says so, which is the part that was missing');
  // The old code froze `hi` with the list, so a coin that kept running was
  // recorded as having stopped — and it bit the winners hardest.
  assert.ok(o.peakMult > o.highs.at(-1)[1], `peak ${o.peakMult} should be past the last kept high`);
});

test('a coin that never fills the list says nothing was capped', async (t) => {
  const r = recorder(t);
  const coin = launch(r, 'h2');
  await sleep(SECONDS * 1000 + 60);
  trade(r, 'h2', coin, 45, 1);
  await sleep(FOLLOW * 1000);
  const { coins } = await r.finish();

  assert.equal(coins[0].outcome.highsCapped, false);
  assert.equal(coins[0].outcome.lowsCapped, false);
});

test('new lows keep being recorded even while new highs are being refused', async (t) => {
  const r = recorder(t, { highsCap: 2 });
  // Entry part way up the curve, so the price has room to fall without asking
  // for a curve state below pump's 30 SOL floor.
  const coin = launch(r, 'h3', { virtualSol: 60 });
  await sleep(SECONDS * 1000 + 60);
  for (let i = 1; i <= 4; i++) trade(r, 'h3', coin, 60 + i * 5, i); // fill the highs
  trade(r, 'h3', coin, 40, 9); // and then a new low
  await sleep(FOLLOW * 1000);
  const { coins } = await r.finish();

  const o = coins[0].outcome;
  assert.equal(o.highsCapped, true);
  assert.ok(o.lows.length >= 1, 'the old `else if` meant a full highs list silenced the lows too');
  assert.equal(o.lowsCapped, false);
});

// ---------------------------------------------------------------------------
// One record, one line — whatever the launcher called their coin
// ---------------------------------------------------------------------------

// Written as an escape, never as the character. U+2028 is a line terminator in
// JavaScript source as well as in readline, so pasting the real thing into this
// file stops the file from parsing — which is the defect, demonstrated on the
// test that tests it.
const LINE_SEP = '\u2028';
const PARA_SEP = '\u2029';

test('a coin name containing a line separator does not split its own record', async (t) => {
  // coins-2026-08-20.jsonl line 1934 is exactly this: a coin named
  // "Power Belongs<U+2028>in Human Hands". JSON.stringify leaves U+2028 raw
  // because it is legal inside a JSON string, and readline treats it as the end
  // of a line — so that record reaches every streaming reader as two fragments,
  // neither of which parses. It is the only unreadable row in the corpus, and
  // the text came from whoever launched the coin.
  const r = recorder(t);
  const mint = address('mint:sep');
  const user = address('user:sep');
  const name = `Power Belongs${LINE_SEP}in Human Hands`;
  r.sock.notify('sig-sep', [createEvent({ mint, user, symbol: 'SEP', name })]);
  await sleep(FOLLOW * 1000 + 250);
  const { coins } = await r.finish();

  assert.equal(coins.length, 1);
  assert.equal(coins[0].name, name, 'the name survives intact...');

  // ...and the file is still one line per record when read the way every
  // analysis in this project reads it.
  const streamed = await checkFiles([path.join(r.dir, `coins-${r.w.session}.jsonl`)]);
  assert.equal(streamed.rows, 1);
  assert.equal(streamed.badRows, 0, 'no fragment, no unparseable half-row');
});

test('the paragraph separator is escaped too, and both survive a round trip', () => {
  for (const ch of [LINE_SEP, PARA_SEP]) {
    const line = jsonLine({ name: `a${ch}b` });
    assert.equal(line.includes(ch), false, 'the raw character never reaches the file');
    assert.equal(JSON.parse(line).name, `a${ch}b`, 'but the value is unchanged');
    assert.equal(line.split(new RegExp(`\\r\\n|[\\n\\r${LINE_SEP}${PARA_SEP}]`)).length, 1, 'and it is still one line');
  }
});

test('the failure rollup is timestamped when it was written, not at its minute boundary', async (t) => {
  // Stamping it with the start of the minute put a row up to sixty seconds
  // before the session that wrote it, and the span of a session is the span of
  // its rows — so a two-second run reported forty-four.
  const r = recorder(t, { failSample: 1 });
  r.sock.notify('sig-when', [], { err: 'BlockhashNotFound' });
  await sleep(60);
  const { meta } = await r.finish();

  const agg = meta.find((m) => m.k === 'failagg');
  const start = meta.find((m) => m.k === 'start');
  const stop = meta.find((m) => m.k === 'stop');
  assert.ok(agg.t >= start.t && agg.t <= stop.t, 'inside the session that wrote it');
  assert.equal(agg.minuteStart, agg.minute * 60_000, 'the bucket is still on the row');
});

// ---------------------------------------------------------------------------
// Who sold, and when
// ---------------------------------------------------------------------------

/**
 * A sell by `seller`, at whatever the price is now — preceded by the buy that
 * put those tokens in their hands.
 *
 * A wallet can only sell what the curve issued to it. Selling tokens nobody
 * ever bought is a state the chain cannot produce, and `checkCurve`'s
 * conservation rule catches it — including when a test invents it, which is how
 * these helpers were caught doing it.
 */
const sell = (r, coin, seller, virtualSol, n) => {
  r.sock.notify(`sig-buy-${n}`, [tradeEvent({ mint: coin.mint, user: seller, sol: 0.2, tokens: 1000, isBuy: true, virtualSol })]);
  r.sock.notify(`sig-sell-${n}`, [tradeEvent({ mint: coin.mint, user: seller, sol: 0.2, tokens: 1000, isBuy: false, virtualSol })]);
};

test('a sell names the wallet that made it, not just the fact that one happened', async (t) => {
  const r = recorder(t);
  const coin = launch(r, 'sl1', { virtualSol: 60 });
  await sleep(SECONDS * 1000 + 60);
  const bot = address('user:bot');
  sell(r, coin, bot, 55, 1);
  await sleep(FOLLOW * 1000);
  const { coins } = await r.finish();

  const sells = coins[0].outcome.sells;
  assert.equal(sells.length, 1);
  const [at, who, sol] = sells[0];
  assert.equal(who, bot, 'the address was on the trade event all along; it was being thrown away');
  assert.ok(at > 0);
  assert.equal(sol, 0.2);
});

test('the creator selling their own coin is named, and the second is on the row', async (t) => {
  // 71.3% of creators sell their own coin, a third of them inside three
  // seconds. "Has the creator sold by second N" used to be answerable only as
  // "has anybody sold by second N", and those are different questions.
  const r = recorder(t);
  const coin = launch(r, 'sl2', { virtualSol: 60, sol: 30 });
  await sleep(SECONDS * 1000 + 60);
  sell(r, coin, coin.user, 50, 1); // the creator dumps
  sell(r, coin, address('user:other'), 45, 2); // and so does somebody else
  await sleep(FOLLOW * 1000);
  const { coins } = await r.finish();

  const o = coins[0].outcome;
  assert.notEqual(o.creatorSellAtSec, null);
  assert.equal(o.sells.length, 2);
  assert.equal(o.sells[0][1], coin.user);
  // The named field is derived from the ledger, so the ledger can always
  // contradict it — and the check is what holds them together.
  assert.deepEqual(checkRow(coins[0]), []);
});

test('a coin whose creator never sold says so, rather than saying nothing', async (t) => {
  const r = recorder(t);
  const coin = launch(r, 'sl3', { virtualSol: 60 });
  await sleep(SECONDS * 1000 + 60);
  sell(r, coin, address('user:not-creator'), 55, 1);
  await sleep(FOLLOW * 1000);
  const { coins } = await r.finish();

  assert.equal(coins[0].outcome.creatorSellAtSec, null);
  assert.equal(coins[0].outcome.sells.length, 1, 'somebody sold — just not the creator');
});

test('a sell before the entry price is struck is still recorded', async (t) => {
  // A third of creator dumps happen inside the first three seconds, which is
  // before `entry` exists. Anything that waited for a price would lose them.
  const r = recorder(t);
  const coin = launch(r, 'sl4', { virtualSol: 60 });
  sell(r, coin, coin.user, 55, 1); // immediately, well before the freeze
  await sleep(FOLLOW * 1000 + 250);
  const { coins } = await r.finish();

  assert.equal(coins[0].outcome.sells.length, 1);
  assert.ok(coins[0].outcome.creatorSellAtSec < coins[0].open.seconds);
});

test('the candles count exactly the sells the ledger names', async (t) => {
  const r = recorder(t);
  const coin = launch(r, 'sl5', { virtualSol: 90, sol: 60 });
  await sleep(SECONDS * 1000 + 60);
  for (let i = 1; i <= 4; i++) sell(r, coin, address(`user:s${i}`), 90 - i * 5, i);
  await sleep(FOLLOW * 1000);
  const { coins } = await r.finish();

  const counted = coins[0].market.candles.reduce((a, c) => a + c.sells, 0);
  assert.equal(counted, 4);
  assert.equal(coins[0].outcome.sells.length, counted, 'the count and its evidence agree');
  assert.equal(coins[0].total.sellers, 4);
  assert.deepEqual(checkRow(coins[0]), []);
});

test('a ledger that ran out of room says so instead of reading as a quiet coin', async (t) => {
  const r = recorder(t, { sellsCap: 2 });
  const coin = launch(r, 'sl6', { virtualSol: 90 });
  await sleep(SECONDS * 1000 + 60);
  for (let i = 1; i <= 5; i++) sell(r, coin, address(`user:c${i}`), 90 - i * 5, i);
  await sleep(FOLLOW * 1000);
  const { coins } = await r.finish();

  assert.equal(coins[0].outcome.sells.length, 2);
  assert.equal(coins[0].outcome.sellsCapped, true);
  assert.match(checkRow(coins[0]).join(' '), /ran out of room/);
});

test('a buy is not written into the sell ledger', async (t) => {
  const r = recorder(t);
  const coin = launch(r, 'sl7');
  await sleep(SECONDS * 1000 + 60);
  trade(r, 'sl7', coin, 45, 1); // a buy
  await sleep(FOLLOW * 1000);
  const { coins } = await r.finish();

  assert.deepEqual(coins[0].outcome.sells, []);
  assert.equal(coins[0].outcome.creatorSellAtSec, null);
});

// ---------------------------------------------------------------------------
// C21 — a counter is only worth having if the rows behind it are still there
// ---------------------------------------------------------------------------

test('every counter in the session footer checks out against the rows', async (t) => {
  const r = recorder(t, { heartbeatMs: 60, failSample: 1 });
  launch(r, 'c21a');
  r.sock.notify('sig-c21-fail', [], { err: 'BlockhashNotFound' });
  await sleep(FOLLOW * 1000 + 250);
  launch(r, 'c21b'); // still live at shutdown, so `truncated` is not zero
  await sleep(40);
  await r.finish();

  const unbacked = (await checkFiles([path.join(r.dir, `coins-${r.w.session}.jsonl`)])).unbacked;
  // `trades` is the one that genuinely cannot be rebuilt: this recorder writes
  // one row per coin, not one per trade. It is named rather than tolerated.
  assert.deepEqual(unbacked.map((u) => u.counter), ['trades']);
  assert.equal(unbacked[0].found, null);
});

// ---------------------------------------------------------------------------
// The raw curve state, and the zero-fee marker
// ---------------------------------------------------------------------------

test('every candle keeps the reserves it closed on, not just the price it implied', async (t) => {
  // The corpus kept only derived open/high/low/close, and no per-trade reserve
  // figure was ever written by anything. So when 18.4% of coins turned out to
  // have been priced off an impossible reserve value, zero of them could be
  // repaired. Both numbers were on the wire at every trade the whole time.
  const r = recorder(t);
  const coin = launch(r, 'cv1');
  await sleep(SECONDS * 1000 + 60);
  trade(r, 'cv1', coin, 45, 1);
  await sleep(FOLLOW * 1000);
  const { coins } = await r.finish();

  const last = coins[0].market.candles.at(-1);
  assert.equal(last.vsol, 45, 'virtual SOL, straight off the event');
  assert.ok(last.vtok > 0);
  assert.equal(last.rsol, 15, 'real SOL — byte 105 of every trade event, never logged until now');
  assert.ok(last.rtok > 0);
  // And the derived price is recoverable from the raw state now sitting beside
  // it — which is the whole point of keeping the state rather than only the
  // reduction of it. Both are in whole units, so the decimals cancel.
  assert.ok(Math.abs(last.c - last.vsol / last.vtok) < last.c * 1e-9,
    `candle close ${last.c} should equal ${last.vsol} / ${last.vtok}`);
});

test('a normal trade records the 95 basis points it paid', async (t) => {
  const r = recorder(t);
  const coin = launch(r, 'cv2');
  await sleep(SECONDS * 1000 + 60);
  // A small move, because this test is about the fee and the 0.5 + 0.1 SOL it
  // was charged on. Walking to 45 virtual SOL on 0.6 SOL is a peak no money
  // paid for, and the row would rightly fail for it.
  trade(r, 'cv2', coin, 30.5, 1);
  await sleep(FOLLOW * 1000);
  const { coins } = await r.finish();

  const o = coins[0].outcome;
  assert.equal(o.feeBps['95'], 2, 'the opening trade and this one');
  assert.deepEqual(o.zeroFee, []);
  // The count and the flag, so nobody has to read the census to see it.
  assert.equal(o.zeroFeeTrades, 0);
  assert.equal(o.curveSuspect, false);
  // 95 basis points of 0.5 SOL and of 0.1 SOL, in SOL. Every cost model in this
  // project has used a remembered 1%; this is what the chain actually charged.
  assert.equal(o.feeSol, 0.0057);
  assert.deepEqual(checkRow(coins[0]), []);
});

test('the entry price says what curve it was struck on, not only what ratio it was', async (t) => {
  // `entry` is a price and a price is a ratio. Without the state behind it a
  // reader cannot tell a peak somebody could have sold into from a quote — and
  // that distinction is the whole of W32.
  const r = recorder(t);
  const coin = launch(r, 'cv6', { virtualSol: 48 });
  await sleep(SECONDS * 1000 + 60);
  trade(r, 'cv6', coin, 60, 1);
  await sleep(FOLLOW * 1000);
  const { coins } = await r.finish();

  const [vsol, vtok, rsol, rtok] = coins[0].outcome.curveAtEntry;
  assert.equal(vsol, 48, 'the curve as it stood when the entry price was read off it');
  assert.equal(rsol, 18, 'realSolReserves — byte 105, and never logged before now');
  assert.ok(vtok > 0 && rtok > 0);
  // And it is the state that price came from, not the state a minute later.
  assert.ok(Math.abs(coins[0].outcome.entry - vsol / vtok) < coins[0].outcome.entry * 1e-9);
  assert.notDeepEqual(coins[0].market.candles.at(-1).vsol, vsol, 'the coin kept trading after the freeze');
});

test('a coin with no trade before the cutoff has no entry and no curve behind it', async (t) => {
  const r = recorder(t);
  const mint = address('mint:cv7');
  r.sock.notify('sig-create-cv7', [createEvent({ mint, user: address('user:cv7'), symbol: 'CV7' })]);
  await sleep(FOLLOW * 1000 + 250);
  const { coins } = await r.finish();

  assert.equal(coins[0].outcome.entry, null);
  assert.equal(coins[0].outcome.curveAtEntry, null, 'absent, rather than a curve nobody traded on');
  assert.equal(coins[0].outcome.feeSol, 0);
});

test('a zero-fee trade is kept in full, and the row is quarantined', async (t) => {
  const r = recorder(t);
  const mint = address('mint:cv3');
  const user = address('user:cv3');
  const actor = address('user:the-one-wallet');
  r.sock.notify('sig-create-cv3', [createEvent({ mint, user, symbol: 'CV3' })]);
  r.sock.notify('sig-open-cv3', [tradeEvent({ mint, user, sol: 0.5, virtualSol: 30 })]);
  await sleep(SECONDS * 1000 + 60);
  // The trade that leaves the curve where the launch curve cannot reach.
  r.sock.notify('sig-zero-cv3', [tradeEvent({
    mint, user: actor, sol: 0.4, tokens: 5_000, virtualSol: 44, feeBasisPoints: 0,
  })]);
  await sleep(FOLLOW * 1000);
  const { coins } = await r.finish();

  const o = coins[0].outcome;
  assert.equal(o.feeBps['0'], 1);
  assert.equal(o.zeroFee.length, 1, 'the whole trade, because it is the one that needs rebuilding');
  const [at, who, sol, tokens, buy, vsol, vtok, rsol] = o.zeroFee[0];
  assert.equal(who, actor);
  assert.equal(buy, 1);
  assert.equal(vsol, 44);
  assert.equal(rsol, 14, 'virtual minus the 30 SOL the curve opens at');
  assert.ok(at > 0 && sol === 0.4 && vtok > 0 && tokens === 5000);

  // And the check refuses to let it pass unremarked.
  assert.match(checkRow(coins[0]).join(' '), /trades paid zero fee/);
});

test('the zero-fee census and the zero-fee ledger are held to each other', async (t) => {
  const r = recorder(t);
  const mint = address('mint:cv4');
  const user = address('user:cv4');
  r.sock.notify('sig-create-cv4', [createEvent({ mint, user, symbol: 'CV4' })]);
  for (let i = 0; i < 3; i++) {
    r.sock.notify(`sig-z-cv4-${i}`, [tradeEvent({ mint, user, sol: 0.1, virtualSol: 31 + i, feeBasisPoints: 0 })]);
  }
  await sleep(FOLLOW * 1000 + 250);
  const { coins } = await r.finish();

  assert.equal(coins[0].outcome.feeBps['0'], 3);
  assert.equal(coins[0].outcome.zeroFee.length, 3, 'the counter and its rows agree');
});

test('the 200-wallet cap is written down, because every sum over who is a floor past it', async (t) => {
  const r = recorder(t, { whoCap: 2 });
  const coin = launch(r, 'cv5');
  await sleep(SECONDS * 1000 + 60);
  for (let i = 0; i < 4; i++) {
    r.sock.notify(`sig-w-${i}`, [tradeEvent({ mint: coin.mint, user: address(`user:w${i}`), sol: 0.1, virtualSol: 35 + i })]);
  }
  await sleep(FOLLOW * 1000);
  const { coins } = await r.finish();

  assert.equal(coins[0].who.length, 2);
  assert.equal(coins[0].whoCapped, true);
});

test('every record type that can be read on its own says which shape it is', async (t) => {
  // The session header alone is not enough. A coin row is copied out of its
  // file constantly — into a database, a jq pipeline, another file — and
  // arrives at the far end with no header beside it. Worse, tracks, tweets and
  // fails each get their own file and no header is written into any of them, so
  // without this the only versioned file in a capture is the coins file.
  const r = recorder(t, { failLog: true, failSample: 1 });
  const coin = launch(r, 'ver');
  r.sock.notify('sig-ver-fail', [tradeEvent({ mint: coin.mint, user: address('user:f'), sol: 0.1 })],
    { err: { InstructionError: [3, { Custom: 6002 }] } });
  await sleep(FOLLOW * 1000 + 250);
  const { coins, tracks, meta } = await r.finish();

  assert.equal(coins[0].v, SCHEMA, 'the coin record');
  assert.equal(tracks[0].v, SCHEMA, 'the tracks row, whose file has no header at all');
  assert.equal(meta.find((m) => m.k === 'start').v, SCHEMA, 'the session header');

  const fails = fs.readFileSync(path.join(r.dir, `fails-${r.w.session}.jsonl`), 'utf8')
    .trim().split('\n').map((l) => JSON.parse(l));
  assert.equal(fails[0].v, SCHEMA, 'the failure row, whose file has no header either');
});

test('a version that moved is a version a reader can act on', async (t) => {
  // The whole point of the bump. A file written today and a file written before
  // the night that added observedSec, curveAtEntry, feeSol, sells and the cap
  // flags now answer "what am I" differently, which they could not do while
  // both said 2.
  const r = recorder(t);
  launch(r, 'ver2');
  await sleep(FOLLOW * 1000 + 250);
  const { coins } = await r.finish();

  assert.ok(coins[0].v > 2, `written at v${coins[0].v}, and the old corpus is versionless`);
  // And the row backs the claim up: everything schema 3 promises is on it.
  for (const field of ['observedSec', 'complete', 'stopReason', 'gapSec', 'curveAtEntry',
    'feeSol', 'feeBps', 'sells', 'highsCapped', 'lowsCapped', 'sellsCapped', 'zeroFeeCapped']) {
    assert.ok(field in coins[0].outcome, `outcome.${field} is promised by v${SCHEMA}`);
  }
  assert.equal(typeof coins[0].whoCapped, 'boolean');
  assert.deepEqual(checkRow(coins[0]), []);
});

test('a capture file holds exactly one shape, session rows included', async (t) => {
  // Found by pointing the new rule at the recorder's own output: the coin rows
  // were stamped and the heartbeat, gap, failagg and stop rows were not, so a
  // real session file held "v3" and "no version" at once — the recorder failing
  // its own rule about a file being one shape. Every line is stamped now.
  const r = recorder(t, { heartbeatMs: 60 });
  launch(r, 'one');
  await sleep(FOLLOW * 1000 + 250);
  await r.finish();

  const file = path.join(r.dir, `coins-${r.w.session}.jsonl`);
  const res = await checkFiles([file]);
  assert.deepEqual(res.filesWithSeveralSchemas, [], 'one file, one version');
  assert.deepEqual(res.schemas.map((s) => s.v), [SCHEMA], 'and it is the current one');
  assert.equal(res.schemas[0].status, 'known');
  assert.ok(res.metaRows >= 3, 'start, at least one tick, and stop were all stamped');
  assert.equal(res.badRows, 0);
});
