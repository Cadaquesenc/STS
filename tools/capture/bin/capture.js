#!/usr/bin/env node
// The way in. Start it and it records; stop it and it stops. Nothing is bought.
//
// This is the watch path of the old `src/cli.js`, with the dashboard command
// removed — that command pulled in the scoring, backtest, clustering and HTTP
// server halves of an application that no longer exists, and none of it ever
// touched the recording.
//
// One difference from that CLI, and it matters. The old `sts` command called
// `watch()` without a `dir`, which left the Tracker with `dir: null` and
// therefore `save: false` — so running it wrote coins-*.jsonl and no
// tracks-*.jsonl at all. Every tracks file in the corpus came out of `sts dash`,
// which did pass one. This always passes one, and prints what it resolved to,
// because a recorder that silently writes three of its four files is the same
// class of bug as a field that is silently constant.
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { watch, DEFAULTS, redact } from '../src/watch.js';
import { dataDir } from '../src/db.js';
import { checkFiles } from '../src/check.js';
import { SCHEMA } from '../src/session.js';
import { enrich, costsFileFor } from '../src/enrich.js';
import { Rpc } from '../src/rpc.js';

// Node's built-in WebSocket announces itself as experimental on every start. It
// is the only warning we expect, so hide that one and let anything else through.
// Node's own printer is a listener, so it has to go before ours can decide.
process.removeAllListeners('warning');
process.on('warning', (w) => {
  if (w.name === 'ExperimentalWarning' && /WebSocket/.test(w.message)) return;
  console.error(w.stack || String(w));
});

const HELP = `capture — record every pump.fun launch and trade as it happens

  capture                    record until you stop it
  capture --all              also print every single trade (very fast)
  capture --seconds 5        freeze each coin's opening at 5 seconds, not 3
  capture --follow 300       keep writing each coin's price for 5 minutes, not 1
  capture --dir <path>       where the files go (default: $STS_HOME, else <repo>/data)
  capture --no-save          don't write anything down
  capture --no-tweets        don't follow linked tweets' engagement over time
  capture --heartbeat 10     seconds between liveness records (0 turns them off)
  capture --fail-sample 50   keep 1 failed transaction in this many (1 = all)
  capture --no-fail-log      don't write failed transactions at all
  capture --no-drain         on Ctrl-C, stop at once instead of finishing
                             the coins already inside their window

  capture check <files...>   grade finished files: dead fields, impossible rows,
                             truncation, session identity and uptime

  capture enrich <files...>  look up what each recorded transaction cost to land
                             — base fee, priority fee, compute units, Jito tip —
                             and write costs-<session>.jsonl beside each input.
                             Needs $STS_RPC. Offline, resumable, idempotent: it
                             never touches the capture and never runs while
                             recording. --limit N stops after N lookups.

  --ws <url>                 websocket to listen on
                             (default: $STS_RPC_WS, or derived from $STS_RPC,
                              or the free public endpoint)

Files come out named for the session, never for the calendar day — a run that
crosses UTC midnight used to be split into two files, and the second half was
later mistaken for an independent day. All in --dir:
  coins-<session>.jsonl      one line per launch, written at the follow mark,
                             interleaved with the run's own start/tick/gap/stop
  tracks-<session>.jsonl     what happened to each coin after that, out to 12h
  tweets-<session>.jsonl     each linked tweet's engagement, sampled for 10 min
  fails-<session>.jsonl      a sample of the transactions that failed on chain
  audit-YYYY-MM-DD.ndjson    what the run did to itself: drops, gaps, writes

Ctrl-C to stop; it finishes the coins already inside their follow window first,
so they are recorded whole rather than cut off. Ctrl-C again to stop at once.
It runs in the foreground and only when you start it: there is no timer, no
launch agent and no daemon, on purpose.
`;

function parse(argv) {
  const opts = { ...DEFAULTS };
  let ws = null;
  let dir = null;
  let cmd = 'run';
  let drain = true;
  const rest = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--help' || a === '-h') return { help: true };
    else if (a === 'check') cmd = 'check';
    else if (a === 'enrich') cmd = 'enrich';
    else if (cmd === 'enrich' && a === '--limit') opts.limit = Number(argv[++i]);
    else if (cmd === 'check' || cmd === 'enrich') rest.push(a);
    else if (a === '--all') opts.all = true;
    else if (a === '--no-save') opts.save = false;
    else if (a === '--no-tweets') opts.tweets = false;
    else if (a === '--no-fail-log') opts.failLog = false;
    else if (a === '--no-drain') drain = false;
    else if (a === '--seconds') opts.seconds = Number(argv[++i]);
    else if (a === '--follow') opts.follow = Number(argv[++i]);
    else if (a === '--heartbeat') opts.heartbeatMs = Number(argv[++i]) * 1000;
    else if (a === '--fail-sample') opts.failSample = Number(argv[++i]);
    else if (a === '--dir') dir = argv[++i];
    else if (a === '--ws') ws = argv[++i];
    else return { error: `unknown option: ${a}` };
  }
  if (!Number.isFinite(opts.seconds) || opts.seconds <= 0) return { error: '--seconds must be a positive number' };
  if (!Number.isFinite(opts.follow) || opts.follow <= 0) return { error: '--follow must be a positive number' };
  if (opts.follow < opts.seconds) return { error: '--follow must be at least --seconds' };
  if (!Number.isFinite(opts.heartbeatMs) || opts.heartbeatMs < 0) return { error: '--heartbeat must be zero or more seconds' };
  // A heartbeat that never fires is a session whose uptime cannot be measured,
  // which is the defect it exists to close. Allowed, because a short test run
  // does not want one, but it has to be asked for.
  if (opts.heartbeatMs === 0) opts.heartbeatMs = Number.MAX_SAFE_INTEGER;
  if (!Number.isInteger(opts.failSample) || opts.failSample < 1) {
    return { error: '--fail-sample must be a whole number of 1 or more (1 keeps every failure)' };
  }
  return { opts, ws, dir, cmd, drain, files: rest };
}

/**
 * The commit of the program doing the recording, stamped into the session
 * header. A capture whose producer cannot be identified from the capture is one
 * whose fields have to be guessed at later — which is what happened to this
 * corpus, whose producer was found in a git stash on no branch.
 *
 * One subprocess, at startup, never on the hot path.
 */
function gitCommit(cwd) {
  try {
    return execFileSync('git', ['rev-parse', 'HEAD'], { cwd, encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim() || null;
  } catch {
    return null;
  }
}

const { help, error, opts, ws, dir, cmd, drain, files } = parse(process.argv.slice(2));
if (help) {
  process.stdout.write(HELP);
  process.exit(0);
}
if (error) {
  console.error(error + '\n\n' + HELP);
  process.exit(2);
}

if (cmd === 'enrich') {
  if (!files.length) {
    console.error('capture enrich needs at least one coin file\n\n' + HELP);
    process.exit(2);
  }
  // Every call this makes is a network call, so it says so and stops rather
  // than half-running against nothing. `watch()` builds its own Rpc for the
  // funding graph; this one is separate and is never handed to the listener.
  const rpc = new Rpc({});
  if (!rpc.enabled) {
    console.error('capture enrich needs an RPC endpoint: set STS_RPC.');
    process.exit(2);
  }
  console.log(`enriching against ${redact(rpc.url)}`);
  for (const f of files) console.log(`  ${path.basename(f)} -> ${path.basename(costsFileFor(f))}`);
  const totals = await enrich({
    files,
    rpc,
    limit: Number.isFinite(opts.limit) ? opts.limit : Infinity,
    onStatus: (m) => console.log(m),
  });
  console.log(
    `\nasked ${totals.asked}, resolved ${totals.resolved}, ` +
      `gone from the chain ${totals.missing}, already done ${totals.skipped}, still to do ${totals.pending}`,
  );
  process.exit(totals.resolved || !totals.asked ? 0 : 1);
}

if (cmd === 'check') {
  if (!files.length) {
    console.error('capture check needs at least one file\n\n' + HELP);
    process.exit(2);
  }
  const result = await checkFiles(files);
  console.log(`${result.rows} rows, ${result.fields} fields, ${result.metaRows} session rows\n`);

  // The shape, before anything that depends on knowing it. A version this build
  // has never met means every complaint below was produced by rules written for
  // a different file, so it is said first and it fails the run — a checker that
  // passes a shape it cannot read is reporting its own ignorance as a clean
  // bill of health, which is the defect the version number exists to catch.
  console.log(`schema (this build reads up to v${SCHEMA}):`);
  for (const s of result.schemas) {
    if (s.status === 'legacy') {
      console.log(`  ${s.rows} rows carry no version — the recorded corpus, schema 1 by definition. Still readable.`);
    } else if (s.status === 'known') {
      console.log(`  v${s.v}  ${s.rows} rows`);
    } else {
      console.log(`  v${s.v}  ${s.rows} rows  *** ${s.status === 'ahead'
        ? 'WRITTEN BY A NEWER RECORDER THAN THIS ONE'
        : 'NOT A VERSION THIS BUILD KNOWS'} ***`);
      console.log('      Nothing below can be trusted about these rows: the rules that graded');
      console.log('      them were written for a shape this file does not claim to be.');
    }
  }
  for (const f of result.filesWithSeveralSchemas) {
    console.log(`  ${path.basename(f)} holds more than one schema version`);
  }
  const unknownSchema = result.schemas.filter((s) => s.status === 'ahead' || s.status === 'unknown');
  console.log('');

  // What the run said about itself. No session rows at all is not a clean
  // result: it means uptime is unmeasurable and the calendar is standing in for
  // the run, which is how a fifteen-hour capture became a tuning day plus a
  // holdout that was really its own tail.
  console.log('sessions:');
  if (!result.sessions.length) {
    console.log('  (none — this file predates session records: no sid, no heartbeat,');
    console.log('   so uptime cannot be measured, only guessed at from launch timing)');
  }
  for (const s of result.sessions) {
    const up = s.uptime === null ? 'unmeasured' : `${(s.uptime * 100).toFixed(1)}%`;
    console.log(`  ${s.sid}  ${fmtSpan(s.spanSec)}  uptime ${up} of ${s.ticks} heartbeats  ` +
      `${s.gaps} gaps (${(s.gapMs / 1000).toFixed(1)}s)  ended: ${s.ended}`);
  }
  if (result.rowsWithoutSid) {
    console.log(`  ${result.rowsWithoutSid} of ${result.rows} rows carry no session id`);
  }
  for (const sid of result.sessionsSplitAcrossFiles) {
    console.log(`  session ${sid} is spread across more than one file`);
  }
  for (const f of result.filesWithSeveralSessions) {
    console.log(`  ${path.basename(f)} holds more than one session`);
  }
  if (result.failRows) {
    console.log(`  ${result.failRows} failed transactions kept` +
      (result.failRowsWithoutRate ? `, ${result.failRowsWithoutRate} of them with no sample rate recorded` : ''));
  }
  if (result.outOfOrder) console.log(`  ${result.outOfOrder} rows written out of time order`);
  if (result.soldMoreThanBought) {
    // Two checks ask nearly the same question and only one of them grades. Which
    // is which has to be on the screen, not only in the source: a reader who
    // cannot tell them apart will either ignore both or act on both.
    console.log(`  ${result.soldMoreThanBought} of ${result.balanceChecked} coins sold more tokens than were bought` +
      ' out of the curve');
    console.log('      REPORTED ONLY, never fails a row: the excess has no structure and does not rise');
    console.log('      with the peak, so it is an accounting difference, not an anomaly');
  }

  // Curve conservation, broken out by how big the peak was. The gradient is the
  // finding: a small rise is usually real, a large one usually is not, and the
  // rate climbs all the way up. One number for the file would hide that.
  const table = (buckets, title, note) => {
    const graded = buckets.filter((b) => b.coins);
    if (!graded.length) return;
    const total = graded.reduce((a, b) => a + b.coins, 0);
    const failed = graded.reduce((a, b) => a + b.impossible, 0);
    console.log(`\n${title} (${failed} of ${total} coins the rule can grade):`);
    for (const b of graded) {
      const pct = ((b.impossible / b.coins) * 100).toFixed(1);
      console.log(`  ${b.label.padEnd(10)} ${String(b.coins).padStart(6)} coins  ${String(b.impossible).padStart(5)} impossible  ${pct.padStart(5)}%`);
    }
    for (const line of note) console.log(`      ${line}`);
  };

  table(result.solConservationByPeak, 'peaks no money paid for — SOL into the curve', [
    'THIS ONE FAILS THE ROW. It is the stronger of the two: it needs no who[], so',
    'the 200-wallet cap does not blind it, and of the 5,974 coins both can grade it',
    'catches all but 3 of what the token form catches plus 95 it does not.',
  ]);
  table(result.conservationByPeak, 'peaks no buying paid for — tokens out of the curve', [
    'THIS ONE FAILS THE ROW TOO, by a route sharing no field with the one above.',
    'The gradient is the finding, not the base rate: a small rise is usually real',
    'and a large one usually is not. It fires on 5.0% of coins under 1.5x, and',
    'that 5% is not noise — the miss is bimodal with an empty band at the',
    'threshold (1,298 coins within 0.05% of exactly 1, then none at all until',
    '1.005), it concentrates by creator (198 of 250 repeat creators fail none of',
    'their 2,383 coins), and failing coins carry more trades on less money. A',
    'dropped message would look like the opposite of all three.',
  ]);

  // W21 C21, and the lesson underneath every defect this recorder has had: a
  // number is only worth having if the rows behind it are still there.
  console.log('\ncounters with nothing behind them (W21 C21):');
  for (const u of result.unbacked) {
    console.log(u.found === null
      ? `  ${u.counter} = ${u.said}  — nothing in the file can rebuild it (${u.from})`
      : `  ${u.counter} = ${u.said}  — but the ${u.from} add up to ${u.found}`);
  }
  if (!result.unbacked.length) {
    console.log(result.sessions.length ? '  (none — every counter checks out against its rows)' : '  (no session footer to check)');
  }

  console.log('\nconstants somebody decided on:');
  for (const d of result.declared) console.log(`  ${d.path} = ${d.value}\n      ${d.why}`);
  if (!result.declared.length) console.log('  (none)');

  console.log('\nfields carrying no information (W21 C7):');
  for (const d of result.dead) {
    console.log(`  ${d.path} = ${d.value}  — the same on all ${d.rows} rows that have it`);
  }
  if (!result.dead.length) console.log('  (none)');

  // Named rather than hidden. A limit somebody decided on reads exactly like a
  // limit nobody noticed unless the tool says which it is, and the whole reason
  // this recorder had defects for a fortnight is that nothing said out loud
  // which of its fields had nothing behind them.
  console.log('\nfields nothing holds to anything:');
  console.log('  connectedForSec  — how long the socket had been up when the launch arrived.');
  console.log('                     Nothing offline can contradict it.');
  console.log('  seq, si          — held only to their range and their order across rows');
  console.log('                     (seq advances within a session; no two launches share a');
  console.log('                     slot position). Their actual values are unverifiable.');
  console.log('  Five of the eight that used to be here now have an invariant:');
  console.log('    who[].slotsAfter  = the wallet\'s own slot minus the launch slot, exactly');
  console.log('    the five *Capped flags — written on every row from v3, so a missing one');
  console.log('    is a defect instead of a shorter way of writing false');
  if (result.seqOutOfOrder) console.log(`  ${result.seqOutOfOrder} rows whose seq does not advance within its session`);
  if (result.duplicateSlotPosition) console.log(`  ${result.duplicateSlotPosition} rows sharing a slot position with another row`);

  console.log('\nrows that contradict themselves:');
  for (const c of result.complaints) console.log(`  ${String(c.rows).padStart(7)}  ${c.kind}`);
  if (!result.complaints.length) console.log('  (none)');
  for (const e of result.examples) {
    console.log(`      e.g. ${path.basename(e.file)}:${e.lineNo}  ${e.bad.join('; ')}`);
  }

  // No session rows is itself a finding: it is the difference between a capture
  // that can prove it was running and one that merely says so.
  const noSessions = result.rows > 0 && !result.sessions.length;
  // A counter that disagrees with its rows is a failure. A counter with no rows
  // behind it at all is reported and not failed on, because this recorder has
  // one of those by design and it is named rather than hidden.
  const contradicted = result.unbacked.filter((u) => u.found !== null).length;
  const failed = result.dead.length || result.badRows || noSessions || contradicted ||
    result.sessionsSplitAcrossFiles.length || result.filesWithSeveralSessions.length ||
    result.failRowsWithoutRate || unknownSchema.length || result.filesWithSeveralSchemas.length ||
    result.seqOutOfOrder || result.duplicateSlotPosition;
  console.log(failed
    ? `\nFAIL — ${result.dead.length} dead fields, ${result.badRows} of ${result.lines} lines unsound` +
      (noSessions ? ', no session records at all' : '') +
      (unknownSchema.length ? `, ${unknownSchema.length} schema version(s) this build cannot read` : '')
    : '\nok — no undeclared constants, no impossible rows, every row inside a session');
  process.exit(failed ? 1 : 0);
}

function fmtSpan(sec) {
  if (sec < 90) return `${sec}s`;
  if (sec < 5400) return `${(sec / 60).toFixed(1)}m`;
  return `${(sec / 3600).toFixed(1)}h`;
}

// A public endpoint works and costs nothing, but it lags a few seconds and drops
// messages under load. Set STS_RPC to your own to fix both.
const wsUrl =
  ws ||
  process.env.STS_RPC_WS ||
  (process.env.STS_RPC ? process.env.STS_RPC.replace(/^http/, 'ws') : null) ||
  'wss://api.mainnet-beta.solana.com';

if (wsUrl.includes('api.mainnet-beta.solana.com')) {
  console.error('using the free public endpoint — expect a few seconds of lag. set STS_RPC for your own.');
}

const where = path.resolve(dir || dataDir());
const here = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const w = watch({ wsUrl, opts: { ...opts, dir: where, gitCommit: gitCommit(here) } });

console.error(`capture — watching pump.fun via ${redact(wsUrl)}`);
console.error(`session ${w.sid} · writing to ${where}`);
console.error(`files are named for the session, not the day: coins-${w.session}.jsonl`);

// Finishing the coins already inside their window is the cheapest fix there is
// for the truncation problem: roughly one record in seven used to be cut off by
// shutdown and there was nothing on the row to say so. Draining turns most of
// those into whole observations rather than into well-labelled partial ones.
const drainMs = drain ? Math.ceil(opts.follow * 1000) + 2_000 : 0;

let stopping = false;
for (const sig of ['SIGINT', 'SIGTERM']) {
  process.on(sig, async () => {
    if (stopping) {
      // A second Ctrl-C abandons the drain — but still goes through the normal
      // shutdown, so what is live is written down as cut off rather than lost.
      console.error('\nnot waiting: what is still live will be written as truncated');
      w.finishNow();
      return;
    }
    stopping = true;
    // A shutdown that hangs is worse than one that loses a few seconds of data:
    // the next thing the user does is kill -9, which loses everything buffered.
    // The budget is the drain plus the ten seconds flushing has never needed.
    const bail = setTimeout(() => {
      console.error('shutdown took too long; exiting anyway');
      process.exit(0);
    }, drainMs + 10_000);
    bail.unref();
    await w.stop({ drainMs });
    clearTimeout(bail);
    process.exit(0);
  });
}
