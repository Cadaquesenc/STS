// Pull a finished Dune execution into the local data directory.
//   DUNE_API_KEY=... node pull-dune.mjs <executionId> [outfile]
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { saveExecution, readWalletRecords } from './src/dune.js';

// Local convenience only: if the key is not exported, borrow the one already
// configured for the Dune MCP server. The key itself is never written into this
// repository — src/dune.js reads the environment and nothing else.
if (!process.env.DUNE_API_KEY) {
  try {
    const cfg = fs.readFileSync(path.join(os.homedir(), '.claude.json'), 'utf8');
    const m = cfg.match(/dune-api-key"\s*:\s*"([^"]+)"/);
    if (m) process.env.DUNE_API_KEY = m[1];
  } catch {}
}

const execId = process.argv[2];
const out = process.argv[3] || './scratch-data/wallets-dune.jsonl';
if (!execId) {
  console.error('usage: DUNE_API_KEY=... node pull-dune.mjs <executionId> [outfile]');
  process.exit(1);
}

console.log('pulling', execId, '->', out);
const n = await saveExecution(execId, out);
console.log('rows written:', n);

const recs = readWalletRecords(out);
console.log('parsed records:', recs.length);
const withMin = recs.filter((r) => r.coins >= 10);
console.log('wallets with >=10 early coins:', withMin.length);
const sorted = [...recs].sort((a, b) => b.meanPeak - a.meanPeak);
console.log('\ntop 5 by mean peak:');
for (const r of sorted.slice(0, 5)) {
  console.log(' ', r.wallet.slice(0, 8) + '…', `coins=${r.coins}`, `mean=${r.meanPeak.toFixed(2)}x`, `run=${(r.runRate * 100).toFixed(0)}%`, `${r.firstDay}→${r.lastDay}`);
}
