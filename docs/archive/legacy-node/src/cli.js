#!/usr/bin/env node
// The way in. Start it and it watches; stop it and it stops.
import { watch, DEFAULTS, redact } from './watch.js';
import { serve } from './dash.js';

// Node's built-in WebSocket announces itself as experimental on every start. It
// is the only warning we expect, so hide that one and let anything else through.
// Node's own printer is a listener, so it has to go before ours can decide.
process.removeAllListeners('warning');
process.on('warning', (w) => {
  if (w.name === 'ExperimentalWarning' && /WebSocket/.test(w.message)) return;
  console.error(w.stack || String(w));
});

const HELP = `sts — watch pump.fun as it happens

  sts                  watch new coins, their story, and how they open
  sts dash             open the dashboard, listening live
  sts dash --browse    open it without starting a listener
  sts --all            also print every single trade (very fast)
  sts --seconds 5      judge each coin's opening at 5 seconds instead of 3
  sts --follow 300     keep watching each coin's price for 5 minutes, not 1
  sts --no-save        don't write anything down
  sts --no-tweets      don't follow linked tweets' engagement over time

  --ws <url>           websocket to listen on
                       (default: $STS_RPC_WS, or derived from $STS_RPC,
                        or the free public endpoint)

One line per coin goes to data/coins-YYYY-MM-DD.jsonl, and each linked
tweet is re-checked over the following ten minutes into data/tweets-*.jsonl
so real attention can be told apart from bought. Ctrl-C to stop.
Nothing is bought.
`;

function parse(argv) {
  const opts = { ...DEFAULTS };
  let ws = null, cmd = 'watch', port = 4747, listen = true;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '--help' || a === '-h') return { help: true };
    else if (a === '--all') opts.all = true;
    else if (a === '--no-save') opts.save = false;
    else if (a === '--no-tweets') opts.tweets = false;
    else if (a === '--seconds') opts.seconds = Number(argv[++i]);
    else if (a === '--follow') opts.follow = Number(argv[++i]);
    else if (a === '--ws') ws = argv[++i];
    else if (a === 'dash') cmd = 'dash';
    else if (a === '--browse') listen = false;
    else if (a === '--port') port = Number(argv[++i]);
    else if (a === 'watch') cmd = 'watch';
    else return { error: `unknown option: ${a}` };
  }
  if (!Number.isFinite(opts.seconds) || opts.seconds <= 0) return { error: '--seconds must be a positive number' };
  if (!Number.isFinite(opts.follow) || opts.follow <= 0) return { error: '--follow must be a positive number' };
  if (opts.follow < opts.seconds) return { error: '--follow must be at least --seconds' };
  return { opts, ws, cmd, port, listen };
}

const { help, error, opts, ws, cmd, port, listen } = parse(process.argv.slice(2));
if (help) {
  process.stdout.write(HELP);
  process.exit(0);
}
if (error) {
  console.error(error + '\n\n' + HELP);
  process.exit(2);
}

// The dashboard only reads files. It never connects to Solana, so it needs no
// endpoint and cannot interfere with a watcher running beside it.
if (cmd === 'dash') {
  const url = ws || process.env.STS_RPC_WS ||
    (process.env.STS_RPC ? process.env.STS_RPC.replace(/^http/, 'ws') : null) ||
    'wss://api.mainnet-beta.solana.com';
  const server = serve({ port, listen, wsUrl: url, opts });
  for (const sig of ['SIGINT', 'SIGTERM']) {
    process.on(sig, async () => {
      await server.stop();
      process.exit(0);
    });
  }
} else {

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
console.error(`sts — watching pump.fun via ${redact(wsUrl)}`);

const w = watch({ wsUrl, opts });

let stopping = false;
for (const sig of ['SIGINT', 'SIGTERM']) {
  process.on(sig, async () => {
    if (stopping) process.exit(1); // a second Ctrl-C means now
    stopping = true;
    // A shutdown that hangs is worse than one that loses a few seconds of data:
    // the next thing the user does is kill -9, which loses everything buffered.
    // Ten seconds is far longer than flushing has ever taken.
    const bail = setTimeout(() => {
      console.error('shutdown took too long; exiting anyway');
      process.exit(0);
    }, 10_000);
    bail.unref();
    await w.stop();
    clearTimeout(bail);
    process.exit(0);
  });
}
}
