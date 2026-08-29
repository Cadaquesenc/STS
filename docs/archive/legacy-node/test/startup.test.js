// Starting it all with one command, and stopping it with one key.
//
// The starting half is easy to believe and easy to check: the board answers, so
// the dashboard is up; a paper order is accepted, so the database is open.
//
// The stopping half is the reason this file exists. SQLite in WAL mode keeps two
// files beside the database while a connection is open — sts.db-wal and
// sts.db-shm — and folds them back in when the last connection closes. So their
// absence afterwards is not a tidiness check: it is the evidence that both
// connections, the listener's and the dashboard's, were actually closed rather
// than left to be killed with the process. A run that ends by being killed
// leaves them there, and leaves the last writes only in the log.
//
// Nothing here touches the network. The listener is pointed at a port with
// nothing behind it, which is enough to have a real socket running, failing and
// retrying — which is the state Ctrl-C usually finds it in anyway.
//
// Run with: node --test test/

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

import { Db } from '../src/db.js';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const LAUNCHER = path.join(ROOT, 'bin', 'sts.js');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function tmp(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'sts-start-test-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

/** A port nothing is on, asked for the same way the operating system hands them out. */
async function freePort() {
  const probe = net.createServer();
  await new Promise((resolve) => probe.listen(0, '127.0.0.1', resolve));
  const { port } = probe.address();
  await new Promise((resolve) => probe.close(resolve));
  return port;
}

/**
 * Start it the way `npm start` does, and wait until it says it is ready.
 *
 * The listener is aimed at 127.0.0.1:1, where nothing is listening: a real
 * socket that never connects, so the shutdown path is exercised with the
 * reconnect timer running rather than against a listener that was never up.
 */
async function launch(t, dir, extra = []) {
  const port = await freePort();
  const child = spawn(process.execPath, [LAUNCHER, '--port', String(port), '--no-open', '--ws', 'ws://127.0.0.1:1', ...extra], {
    cwd: ROOT,
    env: { ...process.env, STS_HOME: dir },
    stdio: ['ignore', 'pipe', 'pipe'],
  });

  let out = '';
  child.stdout.on('data', (d) => { out += d; });
  child.stderr.on('data', (d) => { out += d; });

  const exited = new Promise((resolve) => child.on('exit', (code, signal) => resolve({ code, signal })));
  t.after(() => { if (child.exitCode === null) child.kill('SIGKILL'); });

  for (let waited = 0; waited < 15_000 && !/ready\./.test(out); waited += 100) await sleep(100);
  assert.match(out, /ready\./, `it never said it was ready:\n${out}`);
  return { child, port, exited, log: () => out };
}

const json = async (url, init) => {
  const res = await fetch(url, init);
  return { status: res.status, body: await res.json() };
};

test('one command brings up the board, the listener and the paper record', async (t) => {
  const dir = tmp(t);
  const started = await launch(t, dir);
  const at = `http://127.0.0.1:${started.port}`;

  // The dashboard is serving.
  assert.equal((await json(`${at}/api/status`)).status, 200);
  // The page itself, not only the API.
  assert.equal((await fetch(`${at}/`)).status, 200);
  // The database is open, which is the paper record being up.
  const empty = await json(`${at}/api/paper/trades`);
  assert.equal(empty.status, 200);
  assert.deepEqual(empty.body.open, []);

  // And it says where each of the three is, rather than only that it started.
  const log = started.log();
  assert.match(log, /board\s+http:\/\/localhost:\d+/);
  assert.match(log, /listener\s+ws:\/\/127\.0\.0\.1:1/);
  assert.match(log, new RegExp(`paper\\s+${dir.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}`));

  started.child.kill('SIGINT');
  assert.equal((await started.exited).code, 0);
});

test('Ctrl-C closes the database rather than leaving it to be killed', async (t) => {
  const dir = tmp(t);
  const started = await launch(t, dir);
  const at = `http://127.0.0.1:${started.port}`;

  const placed = await json(`${at}/api/paper/order`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ mint: 'Mint111111111111111111111111111111111111111', sizeSol: 0.5, entryPrice: 0.002, strategy: 'manual' }),
  });
  assert.equal(placed.status, 201);

  // Open connection, write-ahead log on disk. This is the state the assertion
  // after the interrupt is measured against.
  assert.ok(fs.existsSync(path.join(dir, 'sts.db-wal')), 'expected a write-ahead log while it is running');

  started.child.kill('SIGINT');
  const { code, signal } = await started.exited;
  assert.equal(signal, null, 'it stopped by itself rather than being killed');
  assert.equal(code, 0);

  // Both connections closed: the listener's and the dashboard's. Either one left
  // open and these are still here.
  assert.equal(fs.existsSync(path.join(dir, 'sts.db-wal')), false, 'the write-ahead log was not folded back in');
  assert.equal(fs.existsSync(path.join(dir, 'sts.db-shm')), false, 'the shared-memory file was left behind');
  assert.match(started.log(), /everything closed/);

  // And the trade placed before the interrupt is still there afterwards.
  const db = new Db({ dir });
  t.after(() => db.close());
  const kept = db.openPaperTrades();
  assert.equal(kept.length, 1);
  assert.equal(kept[0].size_sol, 0.5);
  assert.equal(kept[0].status, 'OPEN');
});

test('the listener writes out what it was following before it goes', async (t) => {
  const dir = tmp(t);
  const started = await launch(t, dir);

  started.child.kill('SIGINT');
  assert.equal((await started.exited).code, 0);

  // The run summary is the listener's own last word. Without it, the socket was
  // never stopped in an orderly way — it was just cut off with the process.
  const log = started.log();
  assert.match(log, /interrupted — stopping/);
  assert.match(log, /watched .* min: \d+ new coins/);
  assert.match(log, /stored \d+ new coins/);
});

test('a second interrupt gives up instead of waiting again', async (t) => {
  const dir = tmp(t);
  const started = await launch(t, dir);

  started.child.kill('SIGINT');
  started.child.kill('SIGINT');
  const { code } = await started.exited;
  // Either it had already finished (0) or the second one cut it short (1). What
  // must not happen is hanging, which the timeout on this promise would catch.
  assert.ok(code === 0 || code === 1, `unexpected exit code ${code}`);
});

test('--browse starts the board without a listener', async (t) => {
  const dir = tmp(t);
  const started = await launch(t, dir, ['--browse']);
  const at = `http://127.0.0.1:${started.port}`;

  assert.equal((await json(`${at}/api/paper/trades`)).status, 200, 'the paper record is up without the listener');
  assert.match(started.log(), /listener\s+off/);

  started.child.kill('SIGINT');
  assert.equal((await started.exited).code, 0);
  assert.equal(fs.existsSync(path.join(dir, 'sts.db-wal')), false, 'still closed cleanly with no listener to stop');
});

test('a port that is not a port is refused before anything starts', async () => {
  const child = spawn(process.execPath, [LAUNCHER, '--port', 'soon'], { cwd: ROOT, stdio: ['ignore', 'pipe', 'pipe'] });
  let out = '';
  child.stdout.on('data', (d) => { out += d; });
  child.stderr.on('data', (d) => { out += d; });
  const code = await new Promise((resolve) => child.on('exit', resolve));
  assert.equal(code, 2);
  assert.match(out, /--port must be a port number/);
});

test('a mistyped option stops it rather than being ignored', async () => {
  const child = spawn(process.execPath, [LAUNCHER, '--prot', '5000'], { cwd: ROOT, stdio: ['ignore', 'pipe', 'pipe'] });
  let out = '';
  child.stdout.on('data', (d) => { out += d; });
  child.stderr.on('data', (d) => { out += d; });
  const code = await new Promise((resolve) => child.on('exit', resolve));
  assert.equal(code, 2, 'starting on the default port with a typo is how you watch the wrong thing');
  assert.match(out, /unknown option: --prot/);
});
