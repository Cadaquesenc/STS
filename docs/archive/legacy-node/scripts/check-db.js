#!/usr/bin/env node
// Say something useful when sts.db is missing, and nothing at all when it is
// not. Wired up as `pretest` and `predash`.
//
//   node scripts/check-db.js --for test
//   node scripts/check-db.js --for dash
//
// This never fails. The tests skip the corpus-backed cases when there is no
// corpus, and the dashboard opens on an empty board rather than refusing to
// start, so turning a missing file into a non-zero exit would break both of the
// things it is meant to help. What was actually wrong before is that neither
// said why it was empty. This says why.
import fs from 'node:fs';
import path from 'node:path';
import { Db, dataDir } from '../src/db.js';

const args = process.argv.slice(2);
const value = (name, fallback) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] ? args[i + 1] : fallback;
};

const who = value('--for', 'test');
const dir = path.resolve(value('--dir', dataDir()));
const dbFile = path.join(dir, 'sts.db');

const CONSEQUENCE = {
  test: 'the tests that read the recorded corpus will skip; the rest still run.',
  dash: 'the dashboard will open with no history, and paper trading starts an empty ledger.',
};

function notice(headline) {
  console.warn(`\n${headline}`);
  console.warn(`  ${CONSEQUENCE[who] ?? CONSEQUENCE.test}`);
  console.warn('  `npm run db:setup` builds it from the coins-*.jsonl files in data/.');
  console.warn('  no data/ either? `npm run watch` records some, or set STS_HOME.\n');
}

try {
  // Checked as a file first: opening a database that is not there creates one,
  // and a check that creates what it is checking for is worse than no check.
  if (!fs.existsSync(dbFile)) {
    notice(`sts.db is not there yet (looked in ${dir}).`);
  } else {
    const db = new Db({ dir });
    const coins = db.count();
    db.close();
    if (!coins) notice(`sts.db is at ${dbFile} but holds no coins.`);
  }
} catch (err) {
  // A check standing between someone and their test run does not get to be the
  // reason it did not happen.
  console.warn(`could not check sts.db: ${err.message}`);
}
