#!/usr/bin/env node
// Replay a strategy over recorded coins and print what the account would have
// done. Reads only; nothing is written unless you ask for --out.
//
//   node scripts/run-backtest.js --input data/coins-2026-08-11.jsonl
//   node scripts/run-backtest.js --input data --strategy basic-momentum
//   node scripts/run-backtest.js --db data/sts.db --strategy syndicate-sniper
//   node scripts/run-backtest.js --input data --compare basic-momentum,syndicate-sniper
//   node scripts/run-backtest.js --input data --out /tmp/trades.json
//
// --input takes a .jsonl file or a directory of them. A directory reads every
// coins-*.jsonl inside it, oldest day first. --db reads the same records back
// out of the SQLite store instead.
import fs from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { runBacktest, STRATEGIES, DEFAULTS, GATE_REASONS } from '../src/backtest.js';

const args = process.argv.slice(2);
const flag = (name) => args.includes(name);
const value = (name, fallback) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] !== undefined ? args[i + 1] : fallback;
};
/** Every occurrence of a repeatable flag, with commas splitting each one. */
const list = (name) => args
  .flatMap((a, i) => (a === name && args[i + 1] !== undefined ? args[i + 1].split(',') : []))
  .map((s) => s.trim())
  .filter(Boolean);

const HELP = `run-backtest — replay a strategy over coins that already happened

  --input <path>       a coins-*.jsonl file, or a directory of them
  --db <path>          read the recorded coins out of a SQLite store instead
                       (one of --input or --db is required)
  --strategy <name>    ${Object.keys(STRATEGIES).join(', ')}
                       (default: basic-momentum)
  --compare <names>    replay other strategies over the same coins with the same
                       costs and print them side by side. Repeatable, and takes
                       a comma-separated list.
  --balance <sol>      starting balance            (default: ${DEFAULTS.initialBalanceSol})
  --size <sol>         size of each position       (default: ${DEFAULTS.positionSizeSol})
  --slippage-bps <n>   slippage per leg, in bps    (default: ${DEFAULTS.slippageBps})
  --fee <sol>          priority fee per leg        (default: ${DEFAULTS.feeSol})
  --out <path>         also write every trade to a JSON file
  --quiet              print the summary only

Prices in the recordings are multiples of each coin's entry, so a backtest here
is about the shape of the move, never the size of the market.
`;

if (flag('--help') || flag('-h') || !args.length) {
  process.stdout.write(HELP);
  process.exit(args.length ? 0 : 2);
}

const input = value('--input', null);
const dbPath = value('--db', null);
if (!input && !dbPath) die('one of --input or --db is required\n\n' + HELP);
if (input && dbPath) die('--input and --db name two different corpora; pick one');

const name = value('--strategy', 'basic-momentum');
const strategy = named(name);

const { records, sources, malformed } = input ? readJsonl(input) : readDb(dbPath);

const config = {
  initialBalanceSol: number('--balance', DEFAULTS.initialBalanceSol),
  positionSizeSol: number('--size', DEFAULTS.positionSizeSol),
  slippageBps: number('--slippage-bps', DEFAULTS.slippageBps),
  feeSol: number('--fee', DEFAULTS.feeSol),
};

const result = runBacktest({ records, strategy, ...config });

report(result, { strategy, sources, malformed, quiet: flag('--quiet') });

// Same coins, same balance, same costs. A comparison where the two sides paid
// different prices is not a comparison.
const others = list('--compare').map(named);
if (others.length) {
  compare([result, ...others.map((s) => runBacktest({ records, strategy: s, ...config }))]);
}

const out = value('--out', null);
if (out) {
  const dest = path.resolve(out);
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  fs.writeFileSync(dest, JSON.stringify({
    ranAt: new Date().toISOString(),
    inputs: sources,
    strategy: result.strategy,
    config: result.config,
    summary: result.summary,
    skipped: result.skipped,
    byFidelity: result.byFidelity,
    trades: result.trades,
  }, null, 2));
  console.log(`\nevery trade written to ${dest}`);
}

// ---------------------------------------------------------------------------
// Reading the corpus
// ---------------------------------------------------------------------------

function named(n) {
  const s = STRATEGIES[n];
  if (!s) die(`unknown strategy: ${n}\nknown: ${Object.keys(STRATEGIES).join(', ')}`);
  return s;
}

function readJsonl(where) {
  const target = path.resolve(where);
  if (!fs.existsSync(target)) die(`no such file or directory: ${target}`);

  const files = fs.statSync(target).isDirectory()
    ? fs.readdirSync(target).filter((f) => /^coins-.*\.jsonl$/.test(f)).sort().map((f) => path.join(target, f))
    : [target];
  if (!files.length) die(`no coins-*.jsonl files in ${target}`);

  // The files are small enough to hold, and a backtest has to sort by time
  // anyway, so there is nothing to gain from streaming them.
  const records = [];
  let malformed = 0;
  for (const file of files) {
    for (const line of fs.readFileSync(file, 'utf8').split('\n')) {
      if (!line.trim()) continue;
      try { records.push(JSON.parse(line)); } catch { malformed++; }
    }
  }
  return { records, sources: files, malformed };
}

/**
 * The same records, read back out of the store. `raw` is the whole record as it
 * was written, so a replay from here and a replay from the archive are the same
 * replay — which is a property the e2e suite checks rather than assumes.
 *
 * Opened read-only: a backtest has no business writing to the live database.
 */
function readDb(where) {
  const file = path.resolve(where);
  if (!fs.existsSync(file)) die(`no such database: ${file}`);

  const db = new DatabaseSync(file, { readOnly: true });
  try {
    const rows = db.prepare('SELECT raw FROM tokens ORDER BY created_at, mint').all();
    const records = [];
    let malformed = 0;
    for (const row of rows) {
      try { records.push(JSON.parse(row.raw)); } catch { malformed++; }
    }
    return { records, sources: [file], malformed };
  } finally {
    db.close();
  }
}

// ---------------------------------------------------------------------------

function report(r, { strategy, sources, malformed, quiet }) {
  const s = r.summary;
  const w = 26;
  const rows = [];
  const row = (label, v) => rows.push(`  ${label.padEnd(w)}${v}`);

  console.log(`\n${r.strategy.name} — ${r.strategy.describe ?? ''}`);
  console.log(`${sources.length} source${sources.length === 1 ? '' : 's'}, ${r.recordsConsidered} coins` +
    (malformed ? `, ${malformed} unreadable records skipped` : ''));
  const e = r.config.exit;
  console.log(`exit: take ${x(e.takeProfit)}, stop ${x(e.stopLoss)}` +
    (e.trailingStopPct ? `, trail ${pct(e.trailingStopPct * 100)}` : '') +
    `, give up after ${e.maxHoldSec}s`);
  console.log(`each trade: ${r.config.positionSizeSol} SOL, ${r.config.slippageBps} bps slip a leg, ` +
    `${r.config.feeSol} SOL fee a leg\n`);

  if (!s.trades) {
    console.log('  no trades.\n');
    why(r);
    funnel(strategy, r);
    return;
  }

  row('trades', s.trades + (s.thin ? `   (under ${s.minSample} — too few to read as a rate)` : ''));
  row('won / lost', `${s.wins} / ${s.losses}`);
  row('win rate', mag(s.winRatePct));
  row('profit factor', s.profitFactor === null ? 'n/a — nothing lost' : s.profitFactor.toFixed(2));
  rows.push('');
  row('profit and loss', `${sol(s.pnlSol)}   ${pct(s.pnlPct)} of the balance`);
  row('balance', `${s.initialBalanceSol} SOL  ->  ${s.finalBalanceSol.toFixed(4)} SOL`);
  row('expectancy a trade', sol(s.expectancySol));
  row('average winner', s.avgWinnerSol === null ? '—' : sol(s.avgWinnerSol));
  row('average loser', s.avgLoserSol === null ? '—' : sol(s.avgLoserSol));
  rows.push('');
  row('worst fall', `${(-s.maxDrawdownSol).toFixed(4)} SOL    ${mag(s.maxDrawdownPct)} off the high`);
  row('  took', s.drawdownSec >= 1 ? `${duration(s.drawdownSec)} (trade ${s.drawdownFromTrade} to ${s.drawdownToTrade})` : '—');
  row('sharpe (per trade)', s.sharpe === null ? '—' : s.sharpe.toFixed(2));
  row('sortino (per trade)', s.sortino === null ? '—' : s.sortino.toFixed(2));
  row('average hold', `${s.avgHoldSec}s`);
  console.log(rows.join('\n'));

  console.log(`\n  cost charged here          ${mag(s.simulatedCostPct)} a round trip`);
  console.log(`  cost measured on chain     ${mag(s.measuredCostPct)} at ${r.config.positionSizeSol} SOL   (cost.js)`);
  if (s.simulatedCostPct < s.measuredCostPct - 0.25) {
    console.log('  this run charged itself less than the real thing costs, so read the');
    console.log('  profit and loss above as the optimistic end of the range.');
  }

  console.log('');
  why(r);
  funnel(strategy, r);

  if (!quiet && r.trades.length) {
    const worst = [...r.trades].sort((a, b) => a.pnlSol - b.pnlSol).slice(0, 5);
    const best = [...r.trades].sort((a, b) => b.pnlSol - a.pnlSol).slice(0, 5);
    console.log('\n  best five');
    for (const t of best) console.log(`    ${line(t)}`);
    console.log('\n  worst five');
    for (const t of worst) console.log(`    ${line(t)}`);
    console.log('');
  }
}

/**
 * Every run over the same coins, side by side.
 *
 * A rate computed over no trades is not zero, it is unavailable, so those cells
 * print an em dash rather than a number that would line up with the real ones.
 * The fidelity split is part of the table rather than a footnote because it is
 * the difference between a result and an anecdote: a column whose trades are
 * mostly peak-and-close has not had its stops tested.
 */
function compare(runs) {
  const label = 24;
  const col = (v) => String(v).padStart(22);
  const rule = '—'.repeat(label + 22 * runs.length);

  console.log(`\n${rule}`);
  console.log('head to head, same coins and same costs\n');
  console.log(`  ${''.padEnd(label)}${runs.map((r) => col(r.strategy.name.slice(0, 21))).join('')}`);
  console.log(`  ${'-'.repeat(label - 2).padEnd(label)}${runs.map(() => col('-'.repeat(20))).join('')}`);

  const line = (text, cell) => console.log(`  ${text.padEnd(label)}${runs.map((r) => col(cell(r.summary, r))).join('')}`);
  const rate = (v, s) => (s.trades ? v : '—');

  line('total trades', (s) => s.trades);
  line('win rate', (s) => rate(mag(s.winRatePct), s));
  line('  won / lost', (s) => rate(`${s.wins} / ${s.losses}`, s));
  line('profit factor', (s) => (s.profitFactor === null ? '—' : s.profitFactor.toFixed(2)));
  console.log('');
  line('net profit and loss', (s) => sol(s.pnlSol));
  line('  of the balance', (s) => pct(s.pnlPct));
  line('  final balance', (s) => `${s.finalBalanceSol.toFixed(4)} SOL`);
  line('max drawdown', (s) => mag(s.maxDrawdownPct));
  line('  in SOL', (s) => `${(-s.maxDrawdownSol).toFixed(4)} SOL`);
  line('expectancy a trade', (s) => rate(sol(s.expectancySol), s));
  console.log('');
  // How each trade ended. This is where two rules with the same profit and loss
  // stop looking alike: a run that mostly hit its stop and a run that mostly ran
  // out of clock are not the same rule, however similar the totals.
  console.log('  how the trades ended');
  const reasons = runs.map((r) => tally(r.trades));
  for (const key of exitReasons()) {
    if (!reasons.some((t) => t[key])) continue; // a reason nobody hit is noise
    console.log(`  ${exitWords(key).padEnd(label - 2).padStart(label)}` +
      runs.map((r, i) => col(r.summary.trades ? `${reasons[i][key] ?? 0}   ${mag(((reasons[i][key] ?? 0) / r.summary.trades) * 100)}` : '—')).join(''));
  }

  console.log('');
  console.log('  price detail behind the trades');
  for (const f of ['candles', 'ladder', 'coarse']) {
    line(`  ${fidelityWords(f)}`, (s, r) => (s.trades ? `${r.byFidelity[f]}   ${mag((r.byFidelity[f] / s.trades) * 100)}` : '—'));
  }

  // The caveat has to travel with the numbers, not sit at the bottom of a page
  // somewhere. A rule that fired thirty times has not been measured.
  const thin = runs.filter((r) => r.summary.thin);
  if (thin.length) {
    console.log('');
    for (const r of thin) {
      console.log(`  ${r.strategy.name} took ${r.summary.trades} trade${r.summary.trades === 1 ? '' : 's'} — under ${r.summary.minSample ?? 30}, so its win rate,`);
      console.log('  profit factor and profit and loss describe those coins and estimate nothing.');
    }
  }
  console.log('');
}

// Declarations rather than const maps, for the same reason the formatters below
// are: report() and compare() run at the top of this file, above where these sit.
function fidelityWords(key) {
  return {
    candles: 'per-second candles',
    ladder: 'high/low ladder',
    coarse: 'peak and close only',
  }[key] ?? key;
}

/**
 * Every way simulateExit can end a trade, in the order it checks them. A
 * declaration rather than a const for the reason given above: compare() runs
 * before this point in the file.
 */
function exitReasons() {
  return ['target', 'stop', 'trail', 'dump', 'time', 'end'];
}

function exitWords(key) {
  return {
    target: 'target hit',
    stop: 'stop hit',
    trail: 'trailing stop',
    dump: 'deployer sold',
    time: 'max hold reached',
    end: 'end of recording',
  }[key] ?? key;
}

function tally(trades) {
  const out = {};
  for (const t of trades) out[t.reason] = (out[t.reason] ?? 0) + 1;
  return out;
}

/**
 * Why a selective rule turned down what it turned down.
 *
 * Only printed for a strategy that offers an `explain`, because the engine's own
 * skip counters can only say "the strategy passed" — which, for a rule that
 * passes on 99% of a corpus, is the least interesting sentence available.
 */
function funnel(strategy, r) {
  if (typeof strategy?.explain !== 'function') return;

  const counts = new Map(GATE_REASONS.map((k) => [k, 0]));
  for (const record of records) {
    const reason = strategy.explain(record).reason;
    counts.set(reason, (counts.get(reason) ?? 0) + 1);
  }
  const total = [...counts.values()].reduce((s, n) => s + n, 0);
  if (!total) return;

  console.log(`\n  the entry gate, over all ${total} coins:`);
  for (const [reason, n] of counts) {
    if (!n) continue;
    console.log(`    ${String(n).padStart(6)}  ${gateWords(reason)}`);
  }
  const accepted = counts.get('accepted') ?? 0;
  console.log(`\n  ${accepted} of ${total} launches cleared the gate` +
    ` (${((accepted / total) * 100).toFixed(2)}%); ${r.trades.length} of those became trades.`);
}

function gateWords(reason) {
  return {
    unreadable: 'the analyser could not read the record',
    'no-opening-buys': 'nobody bought in the opening window',
    thin: 'too few opening buyers to tell coordination from coincidence',
    'low-score': 'read as ordinary — cluster score under the threshold',
    'no-primary-signal': 'scored high, but on signals that only mean "unusual"',
    'no-bundle': 'nobody landed together — no bundle to follow',
    'thin-bundle': 'too few wallets landed together to call it a group',
    'mixed-sizing': 'landed together but took unrelated sizes — a queue, not a script',
    'solo-dev': 'only the deployer bought its own launch — no outside wallets with it',
    'small-bundle': 'coordinated, but committed too little to be worth following out',
    accepted: 'looked coordinated — entered',
  }[reason] ?? reason;
}

/** What happened to every coin that did not become a trade. */
function why(r) {
  const k = r.skipped;
  console.log('  of the coins read:');
  console.log(`    ${r.trades.length} traded`);
  if (k.notTaken) console.log(`    ${k.notTaken} the strategy passed on`);
  if (k.noEntry) console.log(`    ${k.noEntry} had no entry price — never buyable`);
  if (k.unobserved) console.log(`    ${k.unobserved} were still being watched when the recording stopped, so the exit is unknown`);
  if (k.insufficientBalance) console.log(`    ${k.insufficientBalance} came after the balance ran too low to trade`);

  const f = r.byFidelity;
  if (r.trades.length) {
    const parts = [];
    if (f.candles) parts.push(`${f.candles} on per-second candles`);
    if (f.ladder) parts.push(`${f.ladder} on the high/low ladder`);
    if (f.coarse) parts.push(`${f.coarse} on peak and close only`);
    console.log(`\n  price detail behind the trades: ${parts.join(', ')}`);
    if (f.coarse) {
      console.log('  the peak-and-close ones cannot see a dip the coin recovered from,');
      console.log('  so their stops fire less often than they really would have.');
    }
  }
}

// Declarations, not const arrows: report() runs at the top of this file, above
// where these sit.
function line(t) {
  return `${(t.symbol ?? t.mint ?? '?').toString().slice(0, 12).padEnd(13)}` +
    `${sol(t.pnlSol).padStart(11)}  ${pct(t.pnlPct).padStart(9)}  ` +
    `${t.reason.padEnd(7)}${String(t.holdSec).padStart(6)}s  peak ${x(t.peakMult)}`;
}

function sol(n) { return `${n >= 0 ? '+' : ''}${n.toFixed(4)} SOL`; }
/** Signed, for anything that can go either way. */
function pct(n) { return n === null ? '—' : `${n >= 0 ? '+' : ''}${n.toFixed(2)}%`; }
/** Unsigned, for magnitudes — a drawdown or a cost has no direction to show. */
function mag(n) { return n === null ? '—' : `${Math.abs(n).toFixed(2)}%`; }
function x(n) { return n == null ? '—' : `${Number(n).toFixed(2)}x`; }

function duration(sec) {
  if (sec < 60) return `${Math.round(sec)}s`;
  if (sec < 3600) return `${Math.round(sec / 60)}m`;
  return `${(sec / 3600).toFixed(1)}h`;
}

function number(name, fallback) {
  const raw = value(name, null);
  if (raw === null) return fallback;
  const n = Number(raw);
  if (!Number.isFinite(n)) die(`${name} must be a number, got: ${raw}`);
  return n;
}

function die(msg) {
  console.error(msg);
  process.exit(2);
}
