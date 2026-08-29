#!/usr/bin/env node
// From a fresh clone to a database you can open, in one command.
//
//   npm run db:setup
//
// It does the least it can get away with. A database that is already there is
// left alone — the migration is safe to re-run, but running it on every setup
// would re-read every file for nothing, and `npm run db:migrate` is right there
// when new days need bringing in.
//
// Nothing here fails the build. A collaborator with no data yet has not done
// anything wrong; they just have not collected any, and the useful thing to
// hand them is the sentence that says how.
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { Db, dataDir } from '../src/db.js';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const value = (name, fallback) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] ? args[i + 1] : fallback;
};

const dir = path.resolve(value('--dir', dataDir()));
const dbFile = path.join(dir, 'sts.db');

/** What to tell someone whose data directory has nothing to import. */
function explainEmpty() {
  console.warn(`no database, and nothing in ${dir} to build one from.`);
  console.warn('');
  console.warn('  data/ is where STS keeps everything: one coins-YYYY-MM-DD.jsonl per');
  console.warn('  day, and sts.db beside them. Neither is in git, so a clone starts empty.');
  console.warn('');
  console.warn('  collect your own       npm run watch      (writes coins-<today>.jsonl)');
  console.warn('  use a shared corpus    copy their coins-*.jsonl into this directory,');
  console.warn('                         then run npm run db:setup again');
  console.warn('  keep data elsewhere    set STS_HOME to that directory');
}

// The file has to be checked before anything opens it: opening a database that
// is not there creates it, which would turn this check into a yes every time.
if (fs.existsSync(dbFile)) {
  let coins = null;
  try {
    const db = new Db({ dir });
    coins = db.count();
    db.close();
  } catch (err) {
    console.warn(`sts.db is at ${dbFile} but would not open: ${err.message}`);
    console.warn('move it aside and run this again to rebuild from the files.');
    process.exit(0);
  }
  console.log(`sts.db is already at ${dbFile} — ${coins.toLocaleString('en-US')} coins.`);
  console.log('run `npm run db:migrate` to bring in any days recorded since, or');
  console.log('`npm run db:verify` to check it against the files.');
  process.exit(0);
}

if (!fs.existsSync(dir)) {
  explainEmpty();
  process.exit(0);
}

const files = fs.readdirSync(dir).filter((f) => /^coins-\d{4}-\d{2}-\d{2}\.jsonl$/.test(f));
if (!files.length) {
  explainEmpty();
  process.exit(0);
}

console.log(`no sts.db yet — building it from ${files.length} file${files.length === 1 ? '' : 's'} in ${dir}\n`);

// One implementation of the import, not two. This is the same command the
// README tells a collaborator to run by hand.
const run = spawnSync(process.execPath, [path.join(HERE, 'migrate-jsonl.js'), '--dir', dir], {
  stdio: 'inherit',
});

if (run.error) {
  console.error(`could not run the migration: ${run.error.message}`);
  process.exit(1);
}
if (run.status !== 0) {
  console.error('\nthe migration did not finish. nothing was lost — the JSONL files are untouched.');
  process.exit(run.status ?? 1);
}

console.log('\nready. `npm run dash` opens the dashboard.');
