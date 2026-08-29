// Pulling history in from Dune.
//
// The live watcher only knows what it saw while it was open, and on the public
// RPC that is a small fraction of what happens. Measured on 11 Aug 2026: pump.fun
// had 40,372 launches that day and the local log holds 2,965 of them, so roughly
// nine launches in ten never reached us. Every wallet record built from local
// data alone is therefore built on a sample of about 7%.
//
// Dune has all of it. This reads results of a Dune execution and writes them
// beside the coin logs, so the wallet registry can be built from months of
// launches instead of a few days of whatever the socket happened to catch.
//
// The key is read from the environment and never stored in the repository.

import fs from 'node:fs';
import path from 'node:path';

const API = 'https://api.dune.com/api/v1';

export function apiKey() {
  const k = process.env.DUNE_API_KEY;
  if (!k) throw new Error('DUNE_API_KEY is not set — export it before running the ingest');
  return k;
}

async function get(url) {
  const res = await fetch(url, { headers: { 'X-Dune-API-Key': apiKey() } });
  if (!res.ok) throw new Error(`dune ${res.status}: ${(await res.text()).slice(0, 200)}`);
  return res.json();
}

/**
 * Page through a finished execution's rows. Dune caps a single response, so
 * this walks offsets until the rows run out.
 */
export async function* rowsOf(executionId, { pageSize = 5000 } = {}) {
  let offset = 0;
  for (;;) {
    const body = await get(`${API}/execution/${executionId}/results?limit=${pageSize}&offset=${offset}`);
    const rows = body?.result?.rows ?? [];
    if (!rows.length) return;
    for (const r of rows) yield r;
    offset += rows.length;
    const total = body?.result?.metadata?.total_row_count;
    if (total != null && offset >= total) return;
    if (rows.length < pageSize) return;
  }
}

/** Write an execution's rows to a JSONL file, returning how many landed. */
export async function saveExecution(executionId, file, { pageSize = 5000 } = {}) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const out = fs.openSync(file, 'w');
  let n = 0;
  let buf = '';
  try {
    for await (const row of rowsOf(executionId, { pageSize })) {
      buf += JSON.stringify(row) + '\n';
      n++;
      if (buf.length > 1 << 20) {
        fs.writeSync(out, buf);
        buf = '';
      }
    }
    if (buf) fs.writeSync(out, buf);
  } finally {
    fs.closeSync(out);
  }
  return n;
}

/**
 * Wallet records as the registry wants them. Dune returns strings for decimals,
 * so everything is coerced here rather than at every use site.
 */
export function readWalletRecords(file) {
  if (!fs.existsSync(file)) return [];
  const out = [];
  for (const line of fs.readFileSync(file, 'utf8').split('\n')) {
    if (!line) continue;
    try {
      const r = JSON.parse(line);
      out.push({
        wallet: r.wallet,
        coins: Number(r.coins),
        meanPeak: Number(r.mean_peak),
        runRate: Number(r.run_rate),
        rate2x: Number(r.rate_2x),
        rate5x: Number(r.rate_5x ?? 0),
        avgEarlyBuyers: Number(r.avg_early_buyers),
        avgSolIn: Number(r.avg_sol_in ?? 0),
        firstDay: r.first_day ?? null,
        lastDay: r.last_day ?? null,
      });
    } catch {
      // A truncated final line is not worth failing the whole load for.
    }
  }
  return out;
}
