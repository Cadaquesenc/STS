#!/usr/bin/env node
// Load the coin files into SQLite. Safe to run as many times as you like.
//
// Idempotent because the mint is the primary key and every insert is
// INSERT OR IGNORE: a coin already in the database is left exactly as it was
// first written, not overwritten by a later re-observation. That matters here —
// the same mint really does appear twice across these files, because a restart
// starts the in-memory dedup set over. So a second run does not fail on a
// constraint and does not rewrite history; it reports every record it already
// had as skipped and stops there.
//
// Nothing is deleted and no file is touched. The JSONL stays the archive.
//
//   node scripts/migrate-jsonl.js              # read $STS_HOME or ./data
//   node scripts/migrate-jsonl.js --dir path   # read somewhere else
//   node scripts/migrate-jsonl.js --dry-run    # count, change nothing
//   node scripts/migrate-jsonl.js --verify     # compare the files to the rows
//   node scripts/migrate-jsonl.js --quiet      # totals only, no per-file lines
import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';
import { Db, dataDir } from '../src/db.js';

const args = process.argv.slice(2);
const flag = (name) => args.includes(name);
const value = (name, fallback) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] ? args[i + 1] : fallback;
};

const dir = path.resolve(value('--dir', dataDir()));
const dbFile = path.join(dir, 'sts.db');
const dryRun = flag('--dry-run');
const quiet = flag('--quiet');
const BATCH = 500;

// A bar is for a person watching it happen. Redrawing a line with \r into a
// pipe writes hundreds of half-lines into whatever is reading, so anything that
// is not a terminal gets one summary line per file and nothing in between.
const DRAW = Boolean(process.stdout.isTTY) && !quiet;

const fmt = (n) => n.toLocaleString('en-US');
const plural = (n, word) => `${fmt(n)} ${word}${n === 1 ? '' : 's'}`;

if (flag('--help') || flag('-h')) {
  console.log(`usage: node scripts/migrate-jsonl.js [options]

  --dir <path>   read coins-*.jsonl from here (default: $STS_HOME, or ./data)
  --dry-run      count everything, write nothing
  --verify       compare the files against the database and exit non-zero if a
                 coin in the files has no row. Changes nothing.
  --quiet        print the totals only`);
  process.exit(0);
}

if (!fs.existsSync(dir)) {
  console.error(`no such directory: ${dir}`);
  process.exit(1);
}

const files = fs
  .readdirSync(dir)
  .filter((f) => /^coins-\d{4}-\d{2}-\d{2}\.jsonl$/.test(f))
  .sort();

if (!files.length) {
  console.error(`no coins-*.jsonl files in ${dir}`);
  console.error('collect some with `npm run watch`, or copy a day of them in.');
  process.exit(1);
}

/**
 * Walk one JSONL file line by line.
 *
 * A file is read as a stream rather than into memory because the busy days are
 * already megabytes and there is no reason for that to be the thing that breaks
 * first. `onBytes` gets every line's length including the blank ones, so the
 * progress bar tracks the file rather than the records.
 */
async function readRecords(full, onRecord, onBytes) {
  const rl = readline.createInterface({
    input: fs.createReadStream(full),
    crlfDelay: Infinity,
  });
  let lines = 0;
  let valid = 0;
  let broken = 0;
  for await (const line of rl) {
    onBytes?.(Buffer.byteLength(line, 'utf8') + 1);
    if (!line.trim()) continue;
    lines++;
    let rec;
    try {
      rec = JSON.parse(line);
    } catch {
      // A half-written last line is normal if a run was killed mid-append.
      broken++;
      continue;
    }
    if (!rec?.mint) {
      broken++;
      continue;
    }
    valid++;
    onRecord(rec);
  }
  return { lines, valid, broken };
}

/** Per-file progress. Counters are public; the drawing is throttled. */
class Bar {
  constructor(label, totalBytes) {
    this.label = label;
    this.total = Math.max(1, totalBytes);
    this.bytes = 0;
    this.rows = 0;
    this.added = 0;
    // Started as if it had just drawn, so the first frame is 80 ms in. A small
    // file finishes before that and prints its one summary line without ever
    // flashing a bar at nought per cent on the way past.
    this.drawnAt = Date.now();
  }

  tick() {
    if (!DRAW) return;
    // Sixty redraws a second is sixty writes to a terminal that cannot show
    // them. Twelve looks continuous and costs nothing.
    const now = Date.now();
    if (now - this.drawnAt < 80) return;
    this.drawnAt = now;
    const width = 18;
    const share = Math.min(1, this.bytes / this.total);
    const full = Math.round(share * width);
    const bar = '█'.repeat(full) + '░'.repeat(width - full);
    const pct = String(Math.round(share * 100)).padStart(3);
    process.stdout.write(
      `\x1b[2K\r  ${this.label.padEnd(26)} [${bar}] ${pct}%  ${fmt(this.rows).padStart(8)} lines`,
    );
  }

  /** Erase the bar and leave one line behind that survives the scrollback. */
  done(text) {
    if (DRAW) process.stdout.write('\x1b[2K\r');
    if (!quiet) console.log(text);
  }
}

if (flag('--verify')) await verify();
else await ingest();

/**
 * Read the files into the database.
 *
 * Rows go in batches inside one transaction each, because a commit is an fsync
 * and one fsync per coin is the difference between seconds and minutes.
 */
async function ingest() {
  // A dry run that leaves a database behind has changed the directory, which is
  // the one thing it promised not to do. So the file is only opened if it is
  // already there.
  const db = !dryRun || fs.existsSync(dbFile) ? new Db({ dir }) : null;
  const before = db ? db.count() : 0;

  console.log(`files      ${files.length} in ${dir}`);
  console.log(
    db
      ? `database   ${db.file} — ${fmt(before)} coins`
      : `database   ${dbFile} — not there, and a dry run will not make one`,
  );
  if (!quiet) console.log('');

  const started = process.hrtime.bigint();
  let lines = 0;
  let valid = 0;
  let broken = 0;
  let added = 0;

  for (const file of files) {
    const full = path.join(dir, file);
    const bar = new Bar(file, fs.statSync(full).size);
    let batch = [];

    const commit = () => {
      if (!batch.length) return;
      // INSERT OR IGNORE, so a mint already stored costs a lookup and nothing
      // else. `changes` is how many were actually new; the rest were skipped.
      if (db && !dryRun) bar.added += db.insertTokens(batch);
      batch = [];
    };

    const stats = await readRecords(
      full,
      (rec) => {
        batch.push(rec);
        bar.rows++;
        if (batch.length >= BATCH) commit();
        bar.tick();
      },
      (bytes) => {
        bar.bytes += bytes;
      },
    );
    commit();

    lines += stats.lines;
    valid += stats.valid;
    broken += stats.broken;
    added += bar.added;

    const skipped = stats.valid - bar.added;
    const tail = dryRun
      ? '        (dry run)'
      : `${fmt(bar.added).padStart(8)} new ${fmt(skipped).padStart(8)} skipped`;
    const unreadable = stats.broken ? `  ${fmt(stats.broken)} unreadable` : '';
    bar.done(`  ${file.padEnd(26)} ${fmt(stats.lines).padStart(8)} lines ${tail}${unreadable}`);
  }

  let wallets = 0;
  if (db && !dryRun) wallets = db.rebuildWallets();

  const ms = Number(process.hrtime.bigint() - started) / 1e6;
  const after = db ? db.count() : 0;
  const skipped = valid - added;

  console.log('');
  console.log(`records    ${fmt(lines)} read, ${fmt(valid)} valid, ${fmt(broken)} unreadable`);
  if (dryRun) {
    console.log('dry run — nothing written');
  } else {
    console.log(`tokens     ${fmt(added)} ingested, ${fmt(skipped)} skipped (already in the database)`);
    console.log(`coins      ${fmt(before)} → ${fmt(after)}`);
    console.log(`wallets    ${fmt(wallets)} rebuilt`);
  }
  console.log(`took       ${ms.toFixed(0)} ms`);

  db?.close();
}

/**
 * Compare the files against the rows and say whether the import is whole.
 *
 * Counting lines against rows on its own would report a mismatch on a healthy
 * database, because the same mint legitimately appears in more than one file.
 * The number that has to match is the count of *distinct* mints, and the check
 * worth having is stronger than a count anyway: every mint in the files must
 * have a row. Rows the files do not account for are reported and forgiven —
 * the watcher writes to the database directly, so a day whose file was archived
 * away still has its coins here.
 */
async function verify() {
  if (!fs.existsSync(dbFile)) {
    console.error(`no database at ${dbFile}`);
    console.error('run `npm run db:setup` to build one from the files beside it.');
    process.exit(1);
  }

  const mints = new Set();
  let lines = 0;
  let valid = 0;
  let broken = 0;

  for (const file of files) {
    const full = path.join(dir, file);
    const bar = new Bar(file, fs.statSync(full).size);
    const stats = await readRecords(
      full,
      (rec) => {
        mints.add(rec.mint);
        bar.rows++;
        bar.tick();
      },
      (bytes) => {
        bar.bytes += bytes;
      },
    );
    lines += stats.lines;
    valid += stats.valid;
    broken += stats.broken;
    bar.done(`  ${file.padEnd(26)} ${fmt(stats.lines).padStart(8)} lines ${fmt(stats.valid).padStart(8)} valid`);
  }

  const db = new Db({ dir });
  const rows = db.count();
  const stored = new Set(db.mints());
  db.close();

  let missing = 0;
  const examples = [];
  for (const mint of mints) {
    if (stored.has(mint)) continue;
    missing++;
    if (examples.length < 5) examples.push(mint);
  }
  let extra = 0;
  for (const mint of stored) if (!mints.has(mint)) extra++;

  if (!quiet) console.log('');
  console.log(`files      ${fmt(lines)} lines, ${fmt(valid)} valid, ${fmt(broken)} unreadable, ${fmt(mints.size)} distinct mints`);
  console.log(`database   ${fmt(rows)} rows in ${dbFile}`);
  console.log(`missing    ${plural(missing, 'mint')} in the files with no row`);
  console.log(`extra      ${plural(extra, 'row')} no file accounts for`);

  if (missing) {
    console.error('');
    console.error(`FAIL — ${plural(missing, 'coin')} in the files never made it into the database.`);
    for (const mint of examples) console.error(`  ${mint}`);
    console.error('run `npm run db:migrate` to bring them in.');
    process.exit(1);
  }

  console.log('');
  console.log('ok — every coin in the files has a row.');
  if (extra) console.log('extra rows are expected: the watcher writes here directly, and old files get archived away.');
}
