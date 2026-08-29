// What a run knows about itself, tested without a socket, a disk or a wait.
//
// Every test here is named for the mistake it prevents. The four facts a
// finished record states about its own observation window are the reason this
// file exists: `outcome.follow` was the configured 60 on all 8,881 recorded
// rows, so a coin the listener was still watching when it stopped was written
// identically to one that ran the full minute — and the two were averaged
// together in every expectancy number the project has ever produced.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  SCHEMA, KNOWN_SCHEMAS, schemaStatus,
  newSessionId, sessionStamp, sessionFile, closeFacts, classifyErr, sampled, sessions,
} from '../src/session.js';

// ---------------------------------------------------------------------------
// Session identity, and the filename that stopped splitting at midnight
// ---------------------------------------------------------------------------

test('two runs on the same machine get different session ids', () => {
  assert.notEqual(newSessionId(1_700_000_000_000, 111), newSessionId(1_700_000_000_001, 111));
  assert.notEqual(newSessionId(1_700_000_000_000, 111), newSessionId(1_700_000_000_000, 222));
});

test('a session id sorts by when the run started', () => {
  const early = newSessionId(1_700_000_000_000, 7);
  const late = newSessionId(1_700_000_600_000, 7);
  assert.ok(early < late, `${early} should sort before ${late}`);
});

test('the session stamp is UTC, so two machines name the same run the same way', () => {
  // 2026-08-20T23:59:00Z — one minute before the midnight that split a
  // fifteen-hour capture into a tuning day and a fictional holdout.
  assert.equal(sessionStamp(Date.parse('2026-08-20T23:59:00Z')), '20260820-2359');
  assert.equal(sessionStamp(Date.parse('2026-08-21T00:01:00Z')), '20260821-0001');
});

test('a run that crosses midnight keeps one filename the whole way through', () => {
  const started = Date.parse('2026-08-20T21:00:00Z');
  const sid = newSessionId(started, 9);
  const name = sessionFile(sid, started);
  // The name is fixed at the start of the run; nothing about it depends on the
  // clock afterwards, which is the entire fix. The old naming took the date at
  // the moment of the write, so a coin recorded at 00:01 landed in a different
  // file from one recorded at 23:59 of the same continuous session.
  assert.equal(name, sessionFile(sid, started));
  assert.match(name, /^[0-9a-z]+-[0-9a-z]+-20260820-2100$/);
});

// ---------------------------------------------------------------------------
// Defect 1 — how long was this actually watched
// ---------------------------------------------------------------------------

const facts = (over = {}) => closeFacts({ t: 0, now: 60_000, follow: 60, ...over });

test('a window that ran its course is complete and says so', () => {
  assert.deepEqual(facts(), {
    observedSec: 60, gapSec: 0, stopReason: 'window', complete: true, follow: 60,
  });
});

test('a coin cut off by shutdown is not complete, and says how long it really got', () => {
  const f = facts({ now: 12_400, reason: 'shutdown' });
  assert.equal(f.observedSec, 12, 'twelve seconds, not the sixty the configuration promised');
  assert.equal(f.complete, false);
  assert.equal(f.stopReason, 'shutdown');
});

test('the truncated coin and the whole one are no longer the same row', () => {
  const whole = facts();
  const cut = facts({ now: 3_000, reason: 'shutdown' });
  // This is the defect in one assertion. Under the old shape both rows carried
  // `follow: 60` and nothing else about the window, so they were identical.
  assert.notDeepEqual(whole, cut);
  assert.equal(whole.follow, cut.follow, 'the promise was the same...');
  assert.notEqual(whole.observedSec, cut.observedSec, '...what was delivered was not');
});

test('downtime inside a window is counted even though the timer fired on time', () => {
  // The follow timer fires whether or not the feed was alive. A coin that
  // launched cleanly and lost eight seconds mid-window used to read as a
  // complete observation, and nobody had noticed.
  const f = facts({ down0: 1_000, downNow: 9_000 });
  assert.equal(f.gapSec, 8);
  assert.equal(f.complete, false, 'the window ran its length but the feed did not');
  assert.equal(f.stopReason, 'window', 'eight seconds of sixty is under the ratio');
});

test('downtime from before this coin launched is not charged to it', () => {
  // `down0` is the run's downtime total frozen when the coin opened. Only what
  // accrues after that belongs to this window.
  assert.equal(facts({ down0: 40_000, downNow: 40_000 }).gapSec, 0);
  assert.equal(facts({ down0: 40_000, downNow: 43_000 }).gapSec, 3);
});

test('losing most of the window to an outage is called socket-down, not window', () => {
  const f = facts({ down0: 0, downNow: 30_000 });
  assert.equal(f.stopReason, 'socket-down');
  assert.equal(f.gapSec, 30);
  assert.equal(f.complete, false);
});

test('half a second of downtime rounds up, so gapSec zero means the feed never dropped', () => {
  // Rounding a 400 ms outage away would put the row back where it started:
  // looking complete while not being complete.
  const f = facts({ down0: 0, downNow: 400 });
  assert.equal(f.gapSec, 1);
  assert.equal(f.complete, false);
});

test('gap time can never exceed the window it happened in', () => {
  assert.equal(facts({ now: 5_000, downNow: 99_000, reason: 'shutdown' }).gapSec, 5);
});

test('a clock that stepped backwards produces zero, not a negative window', () => {
  const f = closeFacts({ t: 10_000, now: 0, follow: 60, reason: 'shutdown' });
  assert.equal(f.observedSec, 0);
  assert.equal(f.gapSec, 0);
});

test('observedSec is floored, so a complete window reads exactly the window', () => {
  // A timer never fires early, so nothing complete can fall under the window by
  // rounding — and a coin cut off at 59.6s reads 59, not 60.
  assert.equal(facts({ now: 60_004 }).observedSec, 60);
  assert.equal(facts({ now: 59_600, reason: 'shutdown' }).observedSec, 59);
});

// ---------------------------------------------------------------------------
// Failed transactions
// ---------------------------------------------------------------------------

test('pump slippage is told apart from contention, because they mean opposite things', () => {
  // 6002 is pump's slippage error: somebody was outbid. AccountInUse is
  // contention. One says a strategy is uncompetitive, the other says it is slow.
  assert.deepEqual(classifyErr({ InstructionError: [3, { Custom: 6002 }] }), { e: 'ix3:custom:6002', keepRaw: false });
  assert.deepEqual(classifyErr({ InstructionError: [0, 'AccountInUse'] }), { e: 'ix0:AccountInUse', keepRaw: false });
});

test('an unfamiliar failure keeps its raw error instead of being flattened into other', () => {
  const weird = classifyErr({ InstructionError: [1, { SomethingNew: 4 }] });
  assert.equal(weird.e, 'ix1:SomethingNew');
  assert.equal(weird.keepRaw, true, 'the shape was not recognised, so the original has to survive');
  assert.equal(classifyErr({ InsufficientFundsForRent: { account_index: 3 } }).keepRaw, true);
});

test('a string error and a missing error are both handled without throwing', () => {
  assert.equal(classifyErr('BlockhashNotFound').e, 'BlockhashNotFound');
  assert.equal(classifyErr(null).e, 'none');
});

test('the failure sample is keyed on the signature, so arrival order cannot bias it', () => {
  const sigs = Array.from({ length: 20_000 }, (_, i) => `sig-${i}-${i * 7919}`);
  const kept = sigs.filter((s) => sampled(s, 50)).length;
  // 1 in 50 of 20,000 is 400. Anything near it is the sample behaving; a
  // counter-based sample would be exactly 400 and would also be correlated with
  // whichever burst happened to be arriving.
  assert.ok(kept > 250 && kept < 550, `kept ${kept} of 20000, wanted roughly 400`);
});

test('the same signature is always kept or always dropped, however often it is seen', () => {
  const sig = 'a-signature-redelivered-after-a-reconnect';
  assert.equal(sampled(sig, 50), sampled(sig, 50));
});

test('a rate of one keeps every failure', () => {
  assert.equal(sampled('anything', 1), true);
  assert.equal(sampled('', 1), false, 'except one with no signature to key on');
});

// ---------------------------------------------------------------------------
// Uptime, measured rather than inferred
// ---------------------------------------------------------------------------

const tick = (sid, t, connected) => ({ k: 'tick', sid, t, connected });

test('uptime is connected heartbeats over heartbeats, not a guess from launch timing', () => {
  const rows = [
    { k: 'start', sid: 'A', t: 0, policy: { heartbeatMs: 10_000 } },
    tick('A', 10_000, true), tick('A', 20_000, true), tick('A', 30_000, false), tick('A', 40_000, true),
    { k: 'stop', sid: 'A', t: 50_000 },
  ];
  const [s] = sessions(rows);
  assert.equal(s.ticks, 4);
  assert.equal(s.connected, 3);
  assert.equal(s.uptime, 0.75);
  assert.equal(s.spanSec, 50);
  assert.equal(s.ended, 'stop');
});

test('a session with no heartbeats reports unmeasured rather than a confident number', () => {
  // The other recorder in this house printed "coverage 100.00%" for a listener
  // that had been running for 0.41% of the span it covered. Saying nothing is
  // better than saying that.
  const [s] = sessions([{ k: 'start', sid: 'A', t: 0 }, { k: 'stop', sid: 'A', t: 1000 }]);
  assert.equal(s.uptime, null);
});

test('a run that was killed is left open, because the log cannot tell that from still running', () => {
  const [s] = sessions([{ k: 'start', sid: 'A', t: 0 }, tick('A', 10_000, true)]);
  assert.equal(s.ended, 'open');
  assert.equal(s.to, 10_000, 'the last heartbeat bounds the end to within one interval');
});

test('two sessions in one file are two sessions, in the order they ran', () => {
  const rows = [
    { k: 'start', sid: 'B', t: 100_000 }, tick('B', 110_000, true),
    { k: 'start', sid: 'A', t: 0 }, tick('A', 10_000, true),
  ];
  assert.deepEqual(sessions(rows).map((s) => s.sid), ['A', 'B']);
});

test('gaps and failure rollups are attributed to the session that recorded them', () => {
  const rows = [
    { k: 'start', sid: 'A', t: 0 },
    { k: 'gap', sid: 'A', t: 5_000, ms: 4_000, reason: 'close' },
    { k: 'failagg', sid: 'A', t: 60_000, n: 1_400 },
    { k: 'failagg', sid: 'A', t: 120_000, n: 1_600 },
  ];
  const [s] = sessions(rows);
  assert.equal(s.gaps, 1);
  assert.equal(s.gapMs, 4_000);
  assert.equal(s.failed, 3_000);
});

test('coin rows are not session rows and are ignored here', () => {
  assert.deepEqual(sessions([{ mint: 'M', t: 1, outcome: {} }]), []);
});

test('the schema version is written down so a reader never has to guess from field presence', () => {
  assert.ok(Number.isInteger(SCHEMA) && SCHEMA >= 1);
});

test('a version this build was not written for is refused rather than assumed readable', () => {
  // The failure mode being closed: a checker that meets a shape it has never
  // seen and prints "ok". That is the checker reporting its own ignorance as a
  // clean bill of health, and it is the same defect as a counter with no rows
  // behind it.
  assert.equal(schemaStatus(SCHEMA), 'known');
  assert.equal(schemaStatus(SCHEMA + 1), 'ahead');
  assert.equal(schemaStatus(0), 'unknown');
  assert.equal(schemaStatus(-1), 'unknown');
  assert.equal(schemaStatus(1.5), 'unknown');
  assert.equal(schemaStatus('2'), 'unknown');
});

test('the versionless corpus is legacy, not unknown — those files cannot be re-recorded', () => {
  assert.equal(schemaStatus(null), 'legacy');
  assert.equal(schemaStatus(undefined), 'legacy');
});

test('every version this build claims to know is one it can name', () => {
  for (const v of KNOWN_SCHEMAS) assert.equal(schemaStatus(v), 'known');
  assert.ok(KNOWN_SCHEMAS.has(SCHEMA), 'including the one it writes');
});
