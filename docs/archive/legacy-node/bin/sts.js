#!/usr/bin/env node
// One command, and everything that has to be running is running.
//
// Three things make up STS while it is up: the listener reading the pump.fun
// tape, the dashboard serving the board, and sts.db holding the paper record.
// They were started separately, and separately they drift — a dashboard with no
// listener shows a board that never moves, a listener with no dashboard is a
// file nobody reads, and a second copy of either fights the first for the port
// and the database.
//
// So this starts one of each, in one process, and — the part that actually
// matters — stops them in the right order. The socket closes first, so nothing
// new arrives; then coins still inside their follow window are written out as
// they stand, because a short record is a fact and a missing one is a hole; then
// the wallet rollup is rebuilt from what was stored; then every database
// connection is closed, which is what checkpoints the write-ahead log and
// leaves one file behind instead of three.
//
// There is no separate paper process to start. A paper order is served by the
// dashboard and stored in sts.db, so it is up when they are, and the banner
// below says where it is being kept rather than pretending to have launched it.
import { serve } from '../src/dash.js';
import { redact } from '../src/watch.js';

// Node calls its WebSocket experimental on every start. It is the only warning
// expected here, so this hides that one and lets everything else through. Node's
// own printer is a listener, so it has to go before ours can decide.
process.removeAllListeners('warning');
process.on('warning', (w) => {
  if (w.name === 'ExperimentalWarning' && /WebSocket/.test(w.message)) return;
  console.error(w.stack || String(w));
});

const HELP = `sts — the listener, the dashboard and the paper record, together

  npm start                    start all of it and open the board
  npm start -- --port 5000     ...on another port
  npm start -- --no-open       ...without opening a browser
  npm start -- --browse        ...without the listener, to read what is stored

  --ws <url>   websocket to listen on
               (default: $STS_RPC_WS, or derived from $STS_RPC, or the free
                public endpoint, which lags a few seconds and drops messages)

Data goes where $STS_HOME says, or to data/ beside this repo.
Ctrl-C stops everything and closes the database. Nothing is bought.
`;

const KNOWN = ['--help', '-h', '--port', '--ws', '--no-open', '--browse'];
const argv = process.argv.slice(2);
const has = (name) => argv.includes(name);
const value = (name, fallback = null) => {
  const at = argv.indexOf(name);
  return at >= 0 && argv[at + 1] && !argv[at + 1].startsWith('-') ? argv[at + 1] : fallback;
};

if (has('--help') || has('-h')) {
  process.stdout.write(HELP);
  process.exit(0);
}

// A typo has to stop it rather than be ignored: --prot 5000 silently starting on
// the default port is how you end up looking at yesterday's window.
const unknown = argv.filter((a) => a.startsWith('-') && !KNOWN.includes(a));
if (unknown.length) {
  console.error(`unknown option: ${unknown.join(' ')}\n\n${HELP}`);
  process.exit(2);
}

const port = Number(value('--port', process.env.STS_PORT || 4747));
if (!Number.isInteger(port) || port < 0 || port > 65535) {
  console.error(`--port must be a port number, not ${JSON.stringify(value('--port'))}`);
  process.exit(2);
}

// A public endpoint works and costs nothing, but it lags and drops messages
// under load. Set STS_RPC to your own to fix both.
const wsUrl =
  value('--ws') ||
  process.env.STS_RPC_WS ||
  (process.env.STS_RPC ? process.env.STS_RPC.replace(/^http/, 'ws') : null) ||
  'wss://api.mainnet-beta.solana.com';

const listen = !has('--browse');

// One line, one thing that happened, stamped. Everything the dashboard and the
// listener have to say comes through here, so the log reads as one program
// rather than three talking over each other.
const stamp = () => new Date().toTimeString().slice(0, 8);
const log = (message) => {
  for (const line of String(message).split('\n')) if (line.trim()) console.error(`${stamp()}  ${line.trim()}`);
};

const server = serve({
  port,
  listen,
  wsUrl,
  open: !has('--no-open'),
  status: log,
});

server.once('listening', () => {
  const at = `http://localhost:${server.address()?.port ?? port}`;
  log(`board      ${at}`);
  log(`listener   ${listen ? redact(wsUrl) : 'off — started with --browse'}`);
  log(`paper      ${server.db ? server.db.file : 'unavailable, sts.db did not open'}`);
  log('ready. Ctrl-C to stop.');
});

let stopping = false;

/**
 * Stop everything, once.
 *
 * A shutdown that hangs is worse than one that gives up a few seconds of data,
 * because the next thing anyone does to a process that will not quit is kill it
 * outright — and that is the one way to leave a database mid-write. Ten seconds
 * is far longer than the flush has ever taken, and a second Ctrl-C means now.
 */
async function shutdown(reason, code = 0) {
  if (stopping) {
    log('still stopping — leaving now, which may lose the last few seconds');
    process.exit(1);
  }
  stopping = true;
  log(`${reason} — stopping`);

  const bail = setTimeout(() => {
    log('shutdown took too long; exiting anyway');
    process.exit(code || 1);
  }, 10_000);
  bail.unref();

  try {
    await server.stop();
    log('everything closed. nothing left running.');
  } catch (e) {
    log(`could not stop cleanly: ${e.message}`);
    code ||= 1;
  }
  clearTimeout(bail);
  process.exit(code);
}

process.on('SIGINT', () => shutdown('interrupted'));
process.on('SIGTERM', () => shutdown('terminated'));

// A crash has to go out through the same door. Exiting on the spot is what
// leaves a write-ahead log beside the database and a follow window unwritten.
process.on('uncaughtException', (e) => {
  log(`crashed: ${e?.stack || e}`);
  shutdown('after a crash', 1);
});
process.on('unhandledRejection', (e) => {
  log(`unhandled rejection: ${e?.stack || e}`);
  shutdown('after an unhandled rejection', 1);
});
