// The dashboard's back end.
//
// It reads the same append-only files the watcher writes and serves them to a
// local page. It computes nothing it cannot recompute from those files, holds no
// state of its own, and — for everything the watcher produced — never writes.
// Close it and nothing observed is lost.
//
// Paper trades are the one exception, and they are an exception on purpose. An
// order is not something the watcher saw; it is something a person did, and it
// existed nowhere but the browser's localStorage until now, where clearing site
// data or opening the app somewhere else erased it. Those three endpoints write,
// and what they write to is sts.db.
//
// The one real piece of work here is the wallet graph, and the trap in it is the
// one we already walked into on Dune: two wallets appearing on the same coins
// means nothing by itself, because a handful of bots buy a large share of every
// launch on the network. So every link is measured against how often each wallet
// shows up at all, and the raw footprint is shown next to it.
import fs from 'node:fs';
import { watch as startWatcher, DEFAULTS } from './watch.js';
import { Tracker, LADDER } from './track.js';
import { buildModel } from './strategy.js';
import { runBacktest, STRATEGIES, DEFAULT_EXIT } from './backtest.js';
import { roundTripCostPct } from './cost.js';
import { loadBook, MIN_COINS as WALLET_MIN_COINS } from './wallets.js';
import { scoreCoin, structure, TRADEABLE, TRADEABLE_NOTE } from './score.js';
import { Db } from './db.js';

/** How recently a coin launched — the windows the board offers. */
export const WINDOWS = [
  { key: '1m', seconds: 60 },
  { key: '5m', seconds: 300 },
  { key: '15m', seconds: 900 },
  { key: '30m', seconds: 1800 },
  { key: '1h', seconds: 3600 },
  { key: '5h', seconds: 18000 },
  { key: '12h', seconds: 43200 },
];
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const UI = path.join(ROOT, 'ui');

const TYPES = { '.html': 'text/html', '.css': 'text/css', '.js': 'text/javascript', '.svg': 'image/svg+xml' };
const WRAPPED_SOL = 'So11111111111111111111111111111111111111112';
const externalCache = new Map();

export function serve({
  port = 4747,
  dir = process.env.STS_HOME || path.join(ROOT, 'data'),
  open = true,
  status = console.error,
  listen = false, // run the watcher in here, so opening the app is all it takes
  wsUrl = null,
  opts = {},
  db: injectedDb = null, // tests hand in their own; nothing else should need to
} = {}) {
  const store = new Store(dir);
  const clients = new Set();
  const markets = new Map();

  // Its own connection, not the watcher's. The dashboard runs without a watcher
  // (`sts dash --browse`) and with one that was told not to save, and in both
  // cases the paper record still has to work. WAL and busy_timeout are what make
  // two writers on one file safe; see db.js.
  //
  // A database that will not open must not take the whole board down with it.
  // Everything else here is files, and reading them is still worth doing.
  let db = injectedDb;
  const ownsDb = !db;
  if (ownsDb) {
    try {
      db = new Db({ dir });
    } catch (e) {
      db = null;
      status(`paper trading is unavailable: ${e.message}`);
    }
  }

  // Coins still inside their follow window. The tracker only adopts a coin at
  // the 60-second mark, so without these the shortest holds would have nothing
  // to show: the window would close at the exact moment tracking began.
  const opening = new Map();

  const market = (mint) => {
    if (!markets.has(mint)) markets.set(mint, { mint, ticks: [], trades: [] });
    return markets.get(mint);
  };

  const send = (kind, data) => {
    const frame = `event: ${kind}\ndata: ${JSON.stringify(data)}\n\n`;
    for (const res of clients) {
      try {
        res.write(frame);
      } catch {
        clients.delete(res);
      }
    }
  };

  // Link state for the feed's connection indicator.
  const linkState = { state: 'connecting', endpoint: wsUrl, lag: null, upSince: null };

  let watcher = null;
  if (listen) {
    watcher = startWatcher({
      wsUrl,
      // The tracker writes its own tracks-*.jsonl beside the coin logs, so a
      // restart does not start the model from nothing.
      opts: { ...opts, dir },
      // The terminal is not the audience for every trade any more, the window
      // is. Status is different: when this is started from one command, the
      // terminal is the only place anyone can see whether the socket is up, so
      // those lines go to both.
      out: () => {},
      status: (m) => {
        const msg = String(m);
        send('status', { text: msg });
        status(m);
        if (/^connected/i.test(msg)) {
          linkState.state = 'up';
          linkState.upSince = Date.now();
          send('link', { ...linkState });
        } else if (/^subscribed/i.test(msg)) {
          linkState.state = 'up';
          send('link', { ...linkState });
        } else if (/disconnect|reconnect|error|idle|close/i.test(msg)) {
          linkState.state = 'down';
          linkState.upSince = null;
          send('link', { ...linkState });
        }
      },
      on: {
        launch: (l) => send('launch', l),
        trade: (t) => {
          const o = opening.get(t.mint);
          if (o) { o.last = t.price; o.lastTradeAt = t.t; if (o.entry) o.hi = Math.max(o.hi, t.price / o.entry); }
          const m = market(t.mint);
          m.ticks.push({ t: t.t, price: t.price });
          m.trades.unshift(t);
          if (m.ticks.length > 2000) m.ticks.splice(0, 500);
          if (m.trades.length > 120) m.trades.length = 120;
          send('trade', t);
        },
        open: (o) => {
          // "open" is reserved by EventSource for the connection lifecycle.
          send('opening', o);
          // Worked out once. It is the same answer twice over — the score the
          // tracker carries and the feed's verdict — and the structural read
          // behind it is the expensive part.
          const assessment = candidateAssessment(o);
          if (o.entry) {
            opening.set(o.mint, {
              mint: o.mint, symbol: o.symbol, name: o.name, t: o.t,
              entry: o.entry, last: o.last ?? o.entry, hi: 1, lo: 1, cross: Object.create(null),
              wallets: o.wallets ?? 0, sellers: o.sellers ?? 0, solIn: o.solIn ?? 0,
              trades: o.trades ?? 0, kind: o.kind ?? null, handle: o.handle ?? null,
              nth: o.nth ?? null, score: assessment.score, lastTradeAt: Date.now(),
              // Without these the score has nothing to work with, and a coin in
              // its first minute — the whole point of the short windows — falls
              // back to the no-known-wallet floor and never surfaces.
              who: o.who ?? [], creator: o.creator ?? null,
            });
          }
          const candidate = candidateView(o);
          if (candidate) send('candidate', candidate);
          // The feed's line. Unlike `candidate`, this is sent for every launch
          // including the refused ones: a live feed that quietly drops what the
          // filters threw out cannot be checked against the chain.
          send('verdict', feedRow(o, assessment));
        },
        // A price from a coin past its follow window. This is what makes the
        // board live rather than a snapshot of the last minute of each launch.
        tick: (t) => send('tick', t),
        coin: (c) => {
          // Handed over to the tracker at this point, so it stops being an
          // "opening" coin and is counted once, not twice.
          opening.delete(c.mint);
          store.add(c);
          send('coin', {
            mint: c.mint, symbol: c.symbol, name: c.name, t: c.t,
            wallets: c.open?.wallets ?? null, solIn: c.open?.solIn ?? null,
            peakMult: c.outcome?.peakMult ?? null,
            handle: c.social?.handle ?? null, kind: c.social?.kind ?? null,
          });
        },
      },
    });
  }

  const models = new Models({ dir, store, tracker: () => watcher?.tracker ?? null });
  // Who is worth recognising. Built from Dune history rather than from what the
  // socket happened to catch, because the socket catches about one launch in
  // fourteen.
  const book = loadBook(dir);
  if (book.size) status(`wallet registry: ${book.ranked.length} ranked of ${book.size}, ${book.clusters.size} clusters`);
  else status('wallet registry: none loaded — run the Dune ingest for the wallet signal');

  const server = http.createServer(async (req, res) => {
    try {
      // Inside the try, because it throws. A request for "//" is enough to make
      // this fail, and outside the try that killed the process rather than the
      // request — the dashboard went down to a stray link or a port scanner.
      const url = new URL(req.url, 'http://localhost');
      if (url.pathname === '/api/live') {
        res.writeHead(200, {
          'content-type': 'text/event-stream',
          'cache-control': 'no-cache',
          connection: 'keep-alive',
        });
        res.write('retry: 2000\n\n');
        clients.add(res);
        req.on('close', () => clients.delete(res));
        return;
      }
      if (url.pathname.startsWith('/api/')) {
        // Awaited, not returned. A returned promise settles outside this try,
        // so a route that throws asynchronously — which any route reading a
        // request body can — used to escape as an unhandled rejection and hang
        // the request instead of answering 500.
        return await api(url, res, { store, markets, tracker: watcher?.tracker ?? null, models, opening, book, db, req, linkState });
      }
      return statics(url, res);
    } catch (e) {
      json(res, 500, { error: String(e.message || e) });
    }
  });

  // A port clash must not greet the user with a stack trace. Step up and retry:
  // in app form there may well be another copy already open.
  server.on('error', (e) => {
    if (e.code === 'EADDRINUSE' && port < 4757) {
      status(`port ${port} busy, trying ${port + 1}`);
      port++;
      setTimeout(() => server.listen(port, '127.0.0.1'), 60);
      return;
    }
    status(`could not start: ${e.message}`);
    process.exitCode = 1;
  });

  server.listen(port, '127.0.0.1', () => {
    // Whatever it actually bound to, which is not `port` when the retry above
    // stepped past a busy one, and not when a caller asked for 0 and let the
    // operating system choose.
    const at = `http://localhost:${server.address()?.port ?? port}`;
    status(`dashboard on ${at}`);
    store.load();
    status(`${store.coins.length} coins loaded from ${dir}`);
    if (open) openBrowser(at);
  });

  /**
   * Write tracked coins out right now, synchronously.
   *
   * Shutdown cannot rely on the async stop path: Electron does not await an
   * async window-all-closed handler when the application is quitting, so the
   * flush never ran and every run's crossing times were lost. This is safe to
   * call from before-quit because it returns only once the data is on disk.
   */
  server.flushNow = () => {
    try {
      const t = watcher?.tracker;
      const held = t?.size ?? 0;
      t?.close();
      status(`flush on quit: ${held} tracked coins written (save=${t?.save}, dir=${t?.dir})`);
    } catch (e) {
      status(`flush on quit failed: ${e?.message}`);
    }
  };

  server.stop = async () => {
    for (const res of clients) res.end();
    await watcher?.stop();
    server.close();
    // Only the connection this opened. A caller that handed one in keeps it —
    // it may well still be reading from it after the server has gone.
    if (ownsDb) db?.close();
  };
  server.db = db;
  return server;
}

// One object rather than nine positional arguments: the paper endpoints need the
// request and the database as well, and by then the call site had stopped saying
// which `null` was which.
function api(url, res, ctx = {}) {
  const { store, markets, tracker = null, models = null, opening = new Map(), book = null, db = null, req = null, linkState = { state: 'connecting', endpoint: null, lag: null, upSince: null } } = ctx;
  const p = url.pathname;

  if (p === '/api/status') {
    store.refresh();
    return json(res, 200, {
      coins: store.coins.length,
      wallets: store.byWallet.size,
      withWallets: store.coins.filter((c) => c.who?.length).length,
      days: [...new Set(store.coins.map((c) => new Date(c.t).toISOString().slice(0, 10)))].sort(),
      newest: store.coins.at(-1)?.t ?? null,
    });
  }

  if (p === '/api/link') {
    return json(res, 200, linkState);
  }

  if (p === '/api/coins') {
    store.refresh();
    const q = (url.searchParams.get('q') || '').toLowerCase();
    const limit = Math.min(500, Number(url.searchParams.get('limit')) || 200);
    const rows = store.coins
      .filter((c) => c.who?.length)
      .filter((c) => !q || (c.symbol || '').toLowerCase().includes(q) || (c.name || '').toLowerCase().includes(q))
      .slice(-limit)
      .reverse()
      .map((c) => ({
        mint: c.mint,
        symbol: c.symbol,
        name: c.name,
        t: c.t,
        wallets: c.open?.wallets ?? null,
        solIn: c.open?.solIn ?? null,
        peakMult: c.outcome?.peakMult ?? null,
        handle: c.social?.handle ?? null,
        kind: c.social?.kind ?? null,
      }));
    return json(res, 200, rows);
  }

  if (p === '/api/candidates') {
    store.refresh();
    const limit = Math.min(200, Number(url.searchParams.get('limit')) || 80);
    const rows = store.coins
      .map(candidateView)
      .filter(Boolean)
      .slice(-limit)
      .reverse();
    return json(res, 200, rows);
  }

  if (p === '/api/feed') {
    // What the page shows before the first live launch arrives. Straight off
    // the log, newest first, refused launches included — the stream picks up
    // from here and the two are the same shape.
    store.refresh();
    const limit = Math.min(200, Number(url.searchParams.get('limit')) || 40);
    const rows = store.coins.slice(-limit).reverse().map((c) => feedRow(c));
    return json(res, 200, { rows, logged: store.coins.length });
  }

  if (p.startsWith('/api/market/')) {
    store.refresh();
    const mint = decodeURIComponent(p.slice('/api/market/'.length));
    const live = markets.get(mint);
    if (live) return json(res, 200, live);
    const c = store.byMint.get(mint);
    if (!c) return json(res, 404, { error: 'no market data for this coin' });
    const entry = c.outcome?.entry;
    const savedCandles = (c.market?.candles || []).map((x) => ({
      t: c.t + x.s * 1000, o: x.o, h: x.h, l: x.l, c: x.c,
      volume: x.volume, buys: x.buys, sells: x.sells,
    }));
    if (savedCandles.length) return json(res, 200, { mint, candles: savedCandles, ticks: [], trades: [], historicalSummary: false });
    const points = entry ? [
      { t: c.t + (c.open?.seconds || 3) * 1000, price: entry },
      ...(c.outcome?.highs || []).map(([s, mult]) => ({ t: c.t + s * 1000, price: entry * mult })),
      ...(c.outcome?.lows || []).map(([s, mult]) => ({ t: c.t + s * 1000, price: entry * mult })),
      { t: c.t + (c.outcome?.follow || 60) * 1000, price: c.outcome?.last || entry },
    ].sort((a, b) => a.t - b.t) : [];
    return json(res, 200, { mint, ticks: points, trades: [], historicalSummary: true });
  }

  if (p === '/api/backtest') {
    store.refresh();
    return backtestEndpoint(url, res, store);
  }

  if (p === '/api/best') {
    // The window is how recently a coin launched, not how long you would hold
    // it. "Show me what has appeared in the last five minutes and is worth a
    // look" is the question the interface is actually being asked.
    const windowKey = url.searchParams.get('window') || url.searchParams.get('hold') || '5m';
    const win = WINDOWS.find((w) => w.key === windowKey) || WINDOWS[1];
    const sizeSol = Number(url.searchParams.get('size')) || 0.25;
    const minScore = Number(url.searchParams.get('min') ?? 25);
    const now = Date.now();

    const live = [...opening.values(), ...(tracker ? tracker.rows() : [])];
    const inWindow = live.filter((c) => c.entry && now - c.t <= win.seconds * 1000);

    const scored = inWindow.map((c) => {
      const s = scoreCoin(c, book);
      return {
        mint: c.mint, symbol: c.symbol, name: c.name, t: c.t,
        ageSec: Math.round((now - c.t) / 1000),
        entry: c.entry, last: c.last,
        changePct: round(((c.last / c.entry) - 1) * 100, 2),
        peakMult: round(c.hi ?? 1, 3),
        wallets: c.wallets, solIn: round(c.solIn ?? 0, 3), sellers: c.sellers ?? 0,
        handle: c.handle, kind: c.kind,
        quiet: c.lastTradeAt ? Tracker.isStale(c, now) : false,
        score: s.score,
        runChance: s.runChance,
        basis: s.basis,
        known: s.known,
        structure: s.structure,
        reasons: s.reasons,
        cautions: s.cautions,
        hasKnown: s.hasKnown,
      };
    });

    // Only what it thinks is worth looking at. Everything seen is still on the
    // System screen; this list is the opinion, not the feed.
    const rows = scored
      .filter((r) => r.score >= minScore)
      .sort((a, b) => b.score - a.score || b.changePct - a.changePct)
      .slice(0, 40);

    return json(res, 200, {
      window: win.key,
      windowSeconds: win.seconds,
      sizeSol,
      minScore,
      cost: roundTripCostPct(sizeSol),
      registry: {
        wallets: book?.size ?? 0,
        ranked: book?.ranked.length ?? 0,
        clusters: book?.clusters.size ?? 0,
        loaded: !!book?.builtFrom,
      },
      seenInWindow: inWindow.length,
      // Two different things that were previously added together, which made an
      // empty tracker look busy: coins still inside their opening window, and
      // coins the tracker has taken over and will keep pricing for 12 hours.
      opening: opening.size,
      tracked: tracker ? tracker.size : 0,
      tracking: live.length,
      rows,
      tradeable: TRADEABLE,
      tradeableNote: TRADEABLE_NOTE,
      verdict: !book?.size
        ? 'no wallet registry loaded — run the Dune ingest, or this is only counting crowds'
        : rows.length
          ? `${rows.length} of ${inWindow.length} launches in the last ${win.key} are worth a look`
          : `nothing in the last ${win.key} scores above ${minScore}`,
    });
  }

  if (p === '/api/wallets') {
    const limit = Math.min(500, Number(url.searchParams.get('limit')) || 100);
    const q = (url.searchParams.get('q') || '').trim().toLowerCase();
    const only = url.searchParams.get('only'); // 'clusters' | null
    const minCoins = Number(url.searchParams.get('minCoins')) || 0;
    const sort = url.searchParams.get('sort') || 'mean';

    store.refresh();
    const localWallets = () => [...store.byWallet.entries()].map(([wallet, indexes]) => {
      const coins = indexes.map((i) => store.coins[i]).filter(Boolean);
      const peaks = coins.map((c) => Number(c.outcome?.peakMult || 1));
      const meanPeak = peaks.reduce((sum, value) => sum + value, 0) / Math.max(1, peaks.length);
      return { wallet, coins: coins.length, meanPeak, runRate: peaks.filter((v) => v >= 1.5).length / Math.max(1, peaks.length), rate2x: peaks.filter((v) => v >= 2).length / Math.max(1, peaks.length), rate5x: peaks.filter((v) => v >= 5).length / Math.max(1, peaks.length), avgSolIn: coins.reduce((sum, c) => sum + Number(c.open?.solIn || 0), 0) / Math.max(1, coins.length), avgEarlyBuyers: coins.reduce((sum, c) => sum + Number(c.open?.wallets || 0), 0) / Math.max(1, coins.length), firstDay: coins.length ? new Date(Math.min(...coins.map((c) => c.t))).toISOString().slice(0, 10) : null, lastDay: coins.length ? new Date(Math.max(...coins.map((c) => c.t))).toISOString().slice(0, 10) : null, tier: coins.length >= 10 ? 'local repeat buyer' : 'locally observed', rank: 0, clusterSize: null, source: 'local' };
    }).filter((w) => w.coins >= 2).sort((a, b) => b.meanPeak - a.meanPeak || b.coins - a.coins).map((w, index) => ({ ...w, rank: index + 1 }));
    let pool = book?.ranked?.length ? book.ranked : localWallets();
    if (q) pool = pool.filter((w) => w.wallet.toLowerCase().includes(q));
    if (only === 'clusters') pool = pool.filter((w) => w.clusterSize > 1);
    if (minCoins) pool = pool.filter((w) => w.coins >= minCoins);
    if (sort === 'run') pool = [...pool].sort((a, b) => b.runRate - a.runRate || b.coins - a.coins);
    else if (sort === 'coins') pool = [...pool].sort((a, b) => b.coins - a.coins);

    const rows = pool.slice(0, limit).map((w) => ({
      wallet: w.wallet, coins: w.coins, meanPeak: w.meanPeak, runRate: w.runRate,
      rate2x: w.rate2x, rate5x: w.rate5x, avgSolIn: w.avgSolIn, avgEarlyBuyers: w.avgEarlyBuyers,
      firstDay: w.firstDay, lastDay: w.lastDay, tier: w.tier, rank: w.rank,
      clusterSize: w.clusterSize ?? null,
    }));
    return json(res, 200, {
      total: book?.size || pool.length,
      ranked: book?.ranked.length || pool.length,
      clusters: book?.clusters.size ?? 0,
      source: book?.builtFrom || 'local STS observations',
      minCoins: book?.ranked?.length ? WALLET_MIN_COINS : 2,
      matched: pool.length,
      rows,
    });
  }

  if (p.startsWith('/api/wallet/')) {
    const id = decodeURIComponent(p.slice('/api/wallet/'.length));
    store.refresh();
    const seen = (store.byWallet.get(id) || []).map((i) => store.coins[i]).filter(Boolean);
    let rec = book?.get(id);
    if (!rec && seen.length) {
      const peaks = seen.map((c) => Number(c.outcome?.peakMult || 1));
      rec = { wallet: id, coins: seen.length, meanPeak: peaks.reduce((s, v) => s + v, 0) / peaks.length, runRate: peaks.filter((v) => v >= 1.5).length / peaks.length, rate2x: peaks.filter((v) => v >= 2).length / peaks.length, rate5x: peaks.filter((v) => v >= 5).length / peaks.length, avgSolIn: seen.reduce((s, c) => s + Number(c.open?.solIn || 0), 0) / seen.length, avgEarlyBuyers: seen.reduce((s, c) => s + Number(c.open?.wallets || 0), 0) / seen.length, firstDay: new Date(Math.min(...seen.map((c) => c.t))).toISOString().slice(0, 10), lastDay: new Date(Math.max(...seen.map((c) => c.t))).toISOString().slice(0, 10), tier: 'local STS record', rank: null };
    }
    if (!rec) return json(res, 404, { error: 'wallet has not been observed' });
    return json(res, 200, {
      wallet: rec,
      cluster: rec.cluster ? book?.clusters.get(rec.cluster) : null,
      seenLocally: seen.slice(-50).map((c) => ({
        mint: c.mint, symbol: c.symbol, t: c.t,
        peakMult: c.outcome?.peakMult ?? null,
        wallets: c.open?.wallets ?? null,
      })),
    });
  }

  if (p.startsWith('/api/paper/')) {
    return paperApi(p, url, req, res, { db, store, markets, opening, tracker });
  }

  if (p.startsWith('/api/external/solana/')) {
    const mint = decodeURIComponent(p.slice('/api/external/solana/'.length)).trim();
    if (!/^[1-9A-HJ-NP-Za-km-z]{32,50}$/.test(mint)) return json(res, 400, { error: 'invalid Solana mint address' });
    const deadline = new Promise((_, reject) => setTimeout(() => reject(new Error('live market provider timed out; try again')), 10000));
    return Promise.race([externalSolana(mint), deadline]).then((body) => json(res, 200, body), (error) => json(res, 404, { error: error.message }));
  }

  if (p.startsWith('/api/coin/')) {
    const mint = decodeURIComponent(p.slice('/api/coin/'.length));
    const coin = store.byMint.get(mint);
    if (!coin) return json(res, 404, { error: 'no such coin in the log' });
    return json(res, 200, { coin, view: candidateView(coin, false), graph: store.graph(coin) });
  }

  return json(res, 404, { error: 'no such endpoint' });
}

// ---------------------------------------------------------------------------
// Paper trading
// ---------------------------------------------------------------------------

/**
 * The paper terminal's three endpoints.
 *
 * They are the only writing part of this file, and they are deliberately thin:
 * every rule about what a trade may contain, and the whole P&L calculation,
 * lives in db.js next to the table it is stored in. What is decided here is what
 * belongs to a request rather than to a trade — which price a bare order fills
 * at, and which HTTP status a refusal deserves.
 */
async function paperApi(p, url, req, res, { db, store, markets, opening, tracker }) {
  if (!db) return json(res, 503, { error: 'paper trading needs sts.db, which did not open' });
  const method = req?.method || 'GET';

  try {
    if (p === '/api/paper/trades') {
      if (method !== 'GET') return json(res, 405, { error: `${p} is GET only` });
      return json(res, 200, paperState(db, url));
    }

    if (p === '/api/paper/order') {
      if (method !== 'POST') return json(res, 405, { error: `${p} is POST only` });
      const body = await readJsonBody(req);
      const token = body.tokenAddress ?? body.token_address ?? body.mint ?? null;
      // An order may name its price or leave it to the market. Leaving it out is
      // the ordinary case from a button: the terminal knows which coin it is
      // looking at, and the price it fills at should be the one on the screen,
      // not one the browser did arithmetic on and posted back.
      const entryPrice = body.entryPrice ?? body.entry_price ?? livePrice(token, { store, markets, opening, tracker });
      if (!(Number(entryPrice) > 0)) {
        return json(res, 400, { error: 'no live price for this coin — pass entryPrice with the order' });
      }
      const trade = db.recordPaperFill({ ...body, token_address: token, entry_price: entryPrice });
      return json(res, 201, { trade, filledAt: Number(entryPrice), quoted: body.entryPrice == null && body.entry_price == null });
    }

    if (p === '/api/paper/close') {
      if (method !== 'POST') return json(res, 405, { error: `${p} is POST only` });
      const body = await readJsonBody(req);
      const id = Number(body.id);
      if (!Number.isInteger(id) || id <= 0) return json(res, 400, { error: 'id must be the positive integer id of an open trade' });

      const held = db.paperTrade(id);
      if (!held) return json(res, 404, { error: `no paper trade with id ${id}` });

      // Cancelling records no exit and no P&L: it is the answer for a position
      // that should never have been opened, which is a different claim from one
      // that was closed at a price.
      const cancelling = String(body.status ?? 'CLOSED').toUpperCase() === 'CANCELLED';
      const exitPrice = cancelling
        ? null
        : body.exitPrice ?? body.exit_price ?? livePrice(held.token_address, { store, markets, opening, tracker });
      if (!cancelling && !(Number(exitPrice) > 0)) {
        return json(res, 400, { error: 'no live price for this coin — pass exitPrice with the close' });
      }

      const when = {
        exitPrice,
        exitSec: body.exitSec ?? body.exit_sec ?? null,
        closedAt: body.closedAt ?? body.closed_at ?? null,
      };
      // A size means sell that much of it and keep the rest, which is what the
      // terminal's amount box has always done. No size means all of it.
      const part = body.sizeSol ?? body.size_sol ?? null;
      const done = cancelling || part == null
        ? { trade: db.closePaperTrade(id, { ...when, status: cancelling ? 'CANCELLED' : 'CLOSED' }), remainder: null }
        : db.reducePaperTrade(id, { ...when, sizeSol: part });

      if (!done?.trade) return json(res, 404, { error: `no paper trade with id ${id}` });
      return json(res, 200, { ...done, filledAt: cancelling ? null : Number(exitPrice) });
    }

    return json(res, 404, { error: 'no such endpoint' });
  } catch (e) {
    // Told wrong versus went wrong. Everything db.js rejects on the way in is
    // the caller's to fix and says so in words; a closed position asked to close
    // again is a conflict, not a bad request; anything else is ours and belongs
    // in the 500 the caller above turns it into.
    if (e.code === 'INVALID') return json(res, 400, { error: e.message });
    if (e.code === 'NOT_OPEN') return json(res, 409, { error: e.message, trade: e.trade ?? null });
    throw e;
  }
}

/**
 * Everything the paper screen draws itself from, in one read.
 *
 * Open positions come back whole — there are never many, and a position missing
 * from a page is a position nobody closes. The record behind them is paged,
 * newest first, and the totals are counted across all of it rather than across
 * the page, so the P&L at the top does not change as you scroll.
 */
function paperState(db, url) {
  const q = url.searchParams;
  const token = q.get('token') || q.get('mint') || null;
  const strategy = q.get('strategy') || null;
  const status = q.get('status') ? q.get('status').toUpperCase() : null;
  const limit = Number(q.get('limit')) || 100;
  const cursor = q.get('cursor') || null;

  const open = status && status !== 'OPEN' ? [] : db.openPaperTrades({ token, strategy });
  // 'closed' is the record: closed and cancelled both, since the screen shows
  // what happened to a position rather than only the ones that made money.
  const page = status === 'OPEN'
    ? { rows: [], nextCursor: null }
    : db.paperTrades({ status: status ?? ['CLOSED', 'CANCELLED'], token, strategy, limit, cursor });

  return {
    open,
    closed: page.rows,
    nextCursor: page.nextCursor,
    limit,
    filters: { token, strategy, status },
    summary: db.paperSummary({ token, strategy }),
  };
}

/**
 * What a coin is trading at right now, from whichever source has actually seen a
 * price: the live tape first, then the coins still inside their opening window,
 * then the tracker, then the last price ever written down for it.
 *
 * Null when nothing has — which is an answer, and the endpoints turn it into a
 * refusal rather than filling an order at a made-up price.
 */
function livePrice(mint, { store, markets, opening, tracker }) {
  if (!mint) return null;
  const tape = markets?.get(mint)?.ticks?.at(-1)?.price;
  if (tape > 0) return tape;

  const fresh = opening?.get(mint);
  if (fresh?.last > 0) return fresh.last;
  if (fresh?.entry > 0) return fresh.entry;

  const tracked = tracker?.rows?.().find((r) => r.mint === mint);
  if (tracked?.last > 0) return tracked.last;
  if (tracked?.entry > 0) return tracked.entry;

  // Last resort, so it is worth catching up on what the watcher has appended
  // since the last read. The refresh throttles itself to once every two seconds.
  store?.refresh?.();
  const saved = store?.byMint?.get(mint);
  if (saved?.outcome?.last > 0) return saved.outcome.last;
  if (saved?.outcome?.entry > 0) return saved.outcome.entry;
  return null;
}

/** A JSON request body, with a cap. Anything unparseable is the caller's fault. */
async function readJsonBody(req, limit = 64 * 1024) {
  if (!req) throw badRequest('no request body');
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > limit) throw badRequest('request body is too large');
    chunks.push(chunk);
  }
  if (!size) return {};
  let body;
  try {
    body = JSON.parse(Buffer.concat(chunks).toString('utf8'));
  } catch {
    throw badRequest('body is not valid JSON');
  }
  if (!body || typeof body !== 'object' || Array.isArray(body)) throw badRequest('body must be a JSON object');
  return body;
}

function badRequest(message) {
  const err = new Error(message);
  err.code = 'INVALID'; // handled the same way db.js's own refusals are
  return err;
}

async function externalSolana(mint) {
  const cached = externalCache.get(mint);
  if (cached && Date.now() - cached.at < 1800) return cached.body;
  const fetchPairs = async (address) => {
    const response = await fetch(`https://api.dexscreener.com/token-pairs/v1/solana/${encodeURIComponent(address)}`, {
      headers: { accept: 'application/json', 'user-agent': 'STS/0.1' }, signal: AbortSignal.timeout(7000),
    });
    if (!response.ok) throw new Error(`market provider returned ${response.status}`);
    return response.json();
  };
  const [pairs, solPairs] = await Promise.all([fetchPairs(mint), fetchPairs(WRAPPED_SOL)]);
  const liquid = (rows) => (Array.isArray(rows) ? rows : []).filter((x) => Number(x.priceUsd) > 0).sort((a, b) => Number(b.liquidity?.usd || 0) - Number(a.liquidity?.usd || 0))[0];
  const pair = liquid(pairs), solPair = liquid(solPairs);
  if (!pair) throw new Error('no active Solana market found for this mint');
  if (!solPair) throw new Error('SOL reference price unavailable');
  const token = pair.baseToken?.address === mint ? pair.baseToken : pair.quoteToken?.address === mint ? pair.quoteToken : pair.baseToken;
  const priceUsd = Number(pair.priceUsd), solUsd = Number(solPair.priceUsd), priceSol = priceUsd / solUsd;
  if (!(priceSol > 0)) throw new Error('market has no usable live price');
  const now = Date.now();
  let historicalCandles = [];
  try {
    const side = pair.baseToken?.address === mint ? 'base' : 'quote';
    const historyResponse = await fetch(`https://api.geckoterminal.com/api/v2/networks/solana/pools/${encodeURIComponent(pair.pairAddress)}/ohlcv/minute?aggregate=1&limit=120&currency=usd&token=${side}`, {
      headers: { accept: 'application/json', 'user-agent': 'STS/0.1' }, signal: AbortSignal.timeout(4000),
    });
    if (historyResponse.ok) {
      const history = await historyResponse.json();
      historicalCandles = (history?.data?.attributes?.ohlcv_list || []).map(([t, o, h, l, c, volume]) => ({
        t: Number(t) * 1000, o: Number(o) / solUsd, h: Number(h) / solUsd,
        l: Number(l) / solUsd, c: Number(c) / solUsd, volume: Number(volume || 0),
      })).filter((bar) => [bar.t, bar.o, bar.h, bar.l, bar.c].every(Number.isFinite)).reverse();
    }
  } catch {}
  const body = {
    source: 'dexscreener', external: true, pairAddress: pair.pairAddress, dex: pair.dexId,
    liquidityUsd: Number(pair.liquidity?.usd || 0), priceUsd, solUsd, priceSol,
    view: { mint, symbol: token?.symbol || '?', name: token?.name || 'Unknown token', t: Number(pair.pairCreatedAt || now), wallets: null, sellers: null, solIn: null, solOut: null, trades: null, currentPrice: priceSol, entryPrice: priceSol, score: null, reasons: [], cautions: [], eligible: false, external: true, peakMult: null },
    market: { mint, external: true, candles: historicalCandles, ticks: [{ t: now, price: priceSol }], trades: [] },
  };
  externalCache.set(mint, { at: now, body });
  return body;
}

/**
 * Experimental candidate filter for the test UI. This is deliberately a
 * transparent heuristic, not a safety verdict and not a trading model.
 */
export function candidateAssessment(input) {
  const open = input?.open || input || {};
  const social = input?.social || input || {};
  const cutoffSec = Number(open.seconds || 3);
  const st = structure(input || {}, cutoffSec);
  const addresses = Number(open.wallets || 0);
  const wallets = st.sybil.early > 0 ? st.organicBuyers : addresses;
  const sybilised = st.sybil.early > 0 && st.organicBuyers < addresses;
  const sellers = Number(open.sellers || 0);
  const solIn = Number(open.solIn || 0);
  const solOut = Number(open.solOut || 0);
  const reasons = [];
  const cautions = [];
  let score = 0;

  if (st.rejected) {
    return {
      eligible: false, score: 0, reasons: [], cautions: [...st.blocking],
      rejected: true, blocking: st.blocking, structure: st,
      organicBuyers: wallets, addresses,
    };
  }

  const counted = sybilised ? `${wallets} independent buyers (${addresses} addresses)` : `${wallets} buyers`;

  if (wallets >= 16) {
    score += 55;
    reasons.push(`${counted} inside ${cutoffSec}s — the strongest measured STS signal`);
  } else if (wallets >= 10) {
    score += 42;
    reasons.push(`${counted} inside ${cutoffSec}s — above the candidate floor`);
  } else if (wallets >= 6) {
    score += 20;
    cautions.push(`only ${counted} — thin crowd, high risk`);
  } else {
    cautions.push(`only ${wallets} independent early buyer${wallets === 1 ? '' : 's'}`);
  }

  if (sybilised) {
    cautions.push(`${addresses - wallets} of the ${addresses} opening addresses are other wallets of a buyer already counted`);
    score -= 5;
  }

  if (solIn >= 10) {
    score += 18;
    reasons.push(`${solIn.toFixed(2)} SOL entered — strong opening flow`);
  } else if (solIn >= 5) {
    score += 12;
    reasons.push(`${solIn.toFixed(2)} SOL entered during the opening`);
  } else if (solIn >= 2) {
    score += 5;
    reasons.push(`${solIn.toFixed(2)} SOL entered during the opening`);
  } else {
    score -= 15;
    cautions.push('low opening SOL flow — no real interest');
  }

  const sellerRatio = wallets ? sellers / wallets : 1;
  if (sellerRatio <= 0.2 && solOut <= solIn * 0.3) {
    score += 12;
    reasons.push('very limited seller pressure');
  } else if (sellerRatio <= 0.35 && solOut <= solIn * 0.5) {
    score += 6;
    reasons.push('limited seller pressure at the cutoff');
  } else if (sellerRatio > 0.5 || solOut > solIn * 0.7) {
    score -= 18;
    cautions.push('heavy early seller pressure');
  }

  if (st.creatorSold) {
    score -= 25;
    cautions.push('the creator dumped inside the follow window');
  }

  if (st.concentration >= 0.7) {
    score -= 20;
    cautions.push('whale concentration — one wallet dominates the opening');
  } else if (st.concentration >= 0.5) {
    score -= 8;
    cautions.push('high concentration in one wallet');
  }

  if (social.kind === 'tweet' && social.failed !== true) {
    score += 6;
    reasons.push('linked X post was readable');
    if (social.tweetAgeSec != null && social.tweetAgeSec >= 0 && social.tweetAgeSec <= 300) {
      score += 4;
      reasons.push('linked post was less than five minutes old');
    }
    if (social.followers != null && social.followers >= 1000) {
      score += 4;
      reasons.push(`${social.followers.toLocaleString()} followers — real account`);
    }
  } else if (social.kind === 'nometa' || social.failed) {
    score -= 10;
    cautions.push('social metadata could not be verified');
  } else {
    score -= 6;
    cautions.push('no social link — unverifiable project');
  }

  if (Number(social.nth || 1) > 3) {
    score -= 15;
    cautions.push(`the same social link was already used by ${social.nth} coins — likely a farm`);
  } else if (Number(social.nth || 1) > 2) {
    score -= 10;
    cautions.push(`the same social link was already used by ${social.nth} coins`);
  }

  score = Math.max(0, Math.min(99, Math.round(score)));
  const eligible = wallets >= 10 && score >= 50;
  return {
    eligible, score, reasons, cautions,
    rejected: false, blocking: [],
    structure: st,
    organicBuyers: wallets, addresses,
  };
}

/** The launch-block ceiling, as a share of supply. */
const MAX_LAUNCH_BLOCK_PCT = 35;

/**
 * Which rule refused a launch, in two words.
 *
 * The full sentence is already on the row as a tooltip; this is the version that
 * fits in a column. Order matters — it is the order the checks are written in,
 * so the name shown is the first rule that fired rather than the worst-sounding
 * one. A launch that trips three rules is still refused once.
 */
function refusedOn(st) {
  if (st.supply?.rejected) return 'deployer supply';
  if (st.supply?.launchBlockPct != null && st.supply.launchBlockPct > MAX_LAUNCH_BLOCK_PCT) return 'launch block';
  if (st.sybil?.overCoordinated) return 'coordinated money';
  if (st.imbalance?.invalidated) return 'seller imbalance';
  if (st.sybil?.bundledLaunch) return 'bundled launch';
  return 'structure';
}

/**
 * One launch as the home page shows it.
 *
 * The same object the stream sends and the backfill returns, so a row on the
 * page cannot tell which one it came from and does not have to.
 *
 * Refused launches come through here with everything they were refused on
 * attached. That is the point: the numbers the filters acted on are the numbers
 * on the screen, and a rule that is wrong is visible rather than silent.
 *
 * The structural fields are null until the deployer-supply and funding-graph
 * reads land on this branch. The page already draws that as "unknown" rather
 * than as a zero, which is the honest thing for a number nobody worked out.
 */
export function feedRow(coin, assessment = candidateAssessment(coin)) {
  const open = coin?.open || coin || {};
  const st = assessment.structure || {};
  return {
    mint: coin?.mint ?? null,
    symbol: coin?.symbol ?? null,
    name: coin?.name ?? null,
    t: coin?.t ?? null,
    // The read has landed. Rows created from a bare launch event carry false
    // until it does, which is what the ANALYSING state on the page means.
    resolved: true,
    rejected: !!assessment.rejected,
    // Refused and worth acting on are different questions, and a launch can
    // fail the second having passed the first — which is most of them.
    eligible: !!assessment.eligible,
    blocking: assessment.blocking || [],
    score: assessment.score ?? null,
    solIn: Number(open.solIn || 0),
    wallets: Number(open.wallets || 0),
    organicBuyers: assessment.organicBuyers ?? null,
    supply: st.supply ?? null,
    sybil: st.sybil ?? null,
    imbalance: st.imbalance ?? null,
    refusedOn: assessment.rejected ? refusedOn(st) : null,
  };
}

function candidateView(c) {
  const assessment = candidateAssessment(c);
  if (!assessment.eligible) return null;
  const open = c.open || c;
  const social = c.social || c;
  return {
    mint: c.mint,
    symbol: c.symbol,
    name: c.name,
    t: c.t,
    wallets: Number(open.wallets || 0),
    sellers: Number(open.sellers || 0),
    solIn: Number(open.solIn || 0),
    solOut: Number(open.solOut || 0),
    trades: Number(open.trades || 0),
    handle: social.handle ?? null,
    kind: social.kind ?? null,
    tweetAgeSec: social.tweetAgeSec ?? null,
    followers: social.followers ?? null,
    nth: social.nth ?? null,
    peakMult: c.outcome?.peakMult ?? null,
    endMult: c.outcome?.endMult ?? null,
    entryPrice: c.outcome?.entry ?? c.entry ?? null,
    currentPrice: c.outcome?.last ?? c.last ?? c.entry ?? null,
    score: assessment.score,
    reasons: assessment.reasons,
    cautions: assessment.cautions,
  };
}

/**
 * Builds and caches the expected-value model.
 *
 * Three sources feed it, in order of preference: coins being watched right now,
 * coins written out by earlier runs, and — so the board is not blank on a cold
 * start — coins reconstructed from the one-second candles already on disk.
 * Reconstructed coins only ever reach 60 seconds, which is exactly why the
 * longer horizons start empty and fill in as the tracker runs.
 */
class Models {
  constructor({ dir, store, tracker }) {
    this.dir = dir;
    this.store = store;
    this.tracker = tracker;
    this.cache = new Map(); // size -> { model, at }
    this.ttlMs = 60_000;
  }

  get(sizeSol, now = Date.now()) {
    const hit = this.cache.get(sizeSol);
    if (hit && now - hit.at < this.ttlMs) return hit.model;
    const model = buildModel(this.inputs(now), { sizeSol, now });
    this.cache.set(sizeSol, { model, at: now });
    return model;
  }

  inputs(now) {
    const byMint = new Map();

    for (const c of this.fromCandles()) byMint.set(c.mint, c);
    for (const c of this.fromTracks()) byMint.set(c.mint, c);
    const live = this.tracker?.();
    if (live) {
      for (const c of live.rows()) {
        if (!c.entry) continue;
        byMint.set(c.mint, { ...c, watchedSec: Math.round((now - c.t) / 1000) });
      }
    }
    return [...byMint.values()];
  }

  /** Tracked coins written out by this or an earlier run. */
  fromTracks() {
    const out = [];
    let files = [];
    try {
      files = fs.readdirSync(this.dir).filter((f) => /^tracks-\d{4}-\d{2}-\d{2}\.jsonl$/.test(f));
    } catch {
      return out;
    }
    for (const f of files) {
      let text = '';
      try {
        text = fs.readFileSync(path.join(this.dir, f), 'utf8');
      } catch {
        continue;
      }
      for (const line of text.split('\n')) {
        if (!line) continue;
        try {
          const c = JSON.parse(line);
          if (c.entry) out.push(c);
        } catch {
          // A half-written last line is normal on an append-only file.
        }
      }
    }
    return out;
  }

  /**
   * Crossing times rebuilt from saved candles. A candle only says what happened
   * within a second, so where a bar both rose and fell the order is unknowable —
   * the low is taken first, which is the reading that never flatters a rule.
   */
  fromCandles() {
    const out = [];
    for (const c of this.store.coins) {
      const entry = c.outcome?.entry;
      const bars = c.market?.candles;
      if (!entry || !bars?.length) continue;
      const cross = Object.create(null);
      let hi = 1;
      let lo = 1;
      for (const b of bars) {
        const l = b.l / entry;
        const h = b.h / entry;
        if (l < lo) {
          lo = l;
          for (const L of LADDER) if (L < 1 && l <= L && cross[L] === undefined) cross[L] = b.s;
        }
        if (h > hi) {
          hi = h;
          for (const L of LADDER) if (L >= 1 && h >= L && cross[L] === undefined) cross[L] = b.s;
        }
      }
      out.push({
        mint: c.mint, symbol: c.symbol, name: c.name, t: c.t, entry,
        last: c.outcome?.last ?? entry, hi, lo, cross,
        watchedSec: c.outcome?.follow ?? 60,
        wallets: c.open?.wallets ?? 0, sellers: c.open?.sellers ?? 0,
        solIn: c.open?.solIn ?? 0, trades: c.open?.trades ?? 0,
        kind: c.social?.kind ?? null, nth: c.social?.nth ?? null,
      });
    }
    return out;
  }
}

// ---------------------------------------------------------------------------
// Backtesting
// ---------------------------------------------------------------------------

/**
 * Replay a strategy over the stored coins and report it.
 *
 * This used to be its own replay, written inline: walk each coin's candles, take
 * the first bar that crossed a level, and average the gross multiples. It agreed
 * with backtest.js about nothing, and every way it disagreed flattered the
 * result —
 *
 *   - no costs at all, so a "win" was any exit above the entry rather than any
 *     exit above the entry plus two legs of slippage and two fees. backtest.js
 *     opens by saying a gross number is not a result, it is an advertisement;
 *   - a coin whose window ran out before the hold did was still reported, as a
 *     time exit at its last bar, which is the one thing the engine refuses to do
 *     because "the recording stopped" and "the clock fired" are different facts;
 *   - it only looked at coins with candles — 65 of the 2,602 priceable coins in
 *     this corpus — and called that a win rate, unlabelled;
 *   - it printed a percentage on any sample size, with no thin-sample guard;
 *   - and it compared a bar's launch-second against `maxHold` as if that were a
 *     hold, which is the same clock confusion pathOf carried until it was fixed.
 *
 * So this is a deletion, not a port. The engine already answers this question,
 * with costs, an equity curve, a real drawdown, the fidelity mix behind the
 * trades and a count of what it refused to resolve. The endpoint's job is to
 * parse the query and hand it over.
 *
 * Two shapes are deliberate. `trades` is capped, because buy-everything over this
 * corpus is 1,526 of them and half a megabyte of JSON, and the cap is reported
 * next to the total so nobody mistakes the page for the sample. And `summary`
 * goes back whole, including `thin`, so a caller cannot read a win rate without
 * also being told the engine thinks it means nothing.
 */
function backtestEndpoint(url, res, store) {
  const q = url.searchParams;

  const name = q.get('strategy') || 'buy-everything';
  const strategy = STRATEGIES[name];
  if (!strategy) {
    return json(res, 400, {
      error: `unknown strategy: ${name}`,
      known: Object.keys(STRATEGIES),
    });
  }

  // Every number is optional and every one that is given has to be a number. The
  // old endpoint clamped silently, which meant a typo came back as a plausible
  // result computed from something other than what was asked for.
  const nums = {};
  for (const [key, label] of [
    ['takeProfit', 'takeProfit'], ['stopLoss', 'stopLoss'], ['trailingStopPct', 'trailingStopPct'],
    ['maxHold', 'maxHold'], ['balance', 'balance'], ['size', 'size'],
    ['slippageBps', 'slippageBps'], ['fee', 'fee'],
  ]) {
    const raw = q.get(key);
    if (raw === null || raw === '') continue;
    const value = Number(raw);
    if (!Number.isFinite(value)) return json(res, 400, { error: `${label} must be a number, got ${JSON.stringify(raw)}` });
    nums[key] = value;
  }

  // An exit rule the caller only partly names inherits the rest from the
  // strategy, then from the engine's defaults — the same order runBacktest uses,
  // so naming nothing here is exactly the strategy as the CLI would run it.
  // Keyed the way the engine keys them, because that is where they end up.
  const override = {};
  if ('takeProfit' in nums) override.takeProfit = nums.takeProfit;
  if ('stopLoss' in nums) override.stopLoss = nums.stopLoss;
  if ('trailingStopPct' in nums) override.trailingStopPct = nums.trailingStopPct;
  if ('maxHold' in nums) override.maxHoldSec = nums.maxHold;

  const limit = Math.max(0, Math.min(1000, Number(q.get('limit')) || 200));

  let result;
  try {
    result = runBacktest({
      records: store.coins,
      strategy: withExitOverride(strategy, override),
      ...('balance' in nums ? { initialBalanceSol: nums.balance } : {}),
      ...('size' in nums ? { positionSizeSol: nums.size } : {}),
      ...('slippageBps' in nums ? { slippageBps: nums.slippageBps } : {}),
      ...('fee' in nums ? { feeSol: nums.fee } : {}),
    });
  } catch (e) {
    // RangeError and TypeError out of the engine are the caller's fault — a
    // negative balance, a zero position — so they are a 400, not a 500.
    if (e instanceof RangeError || e instanceof TypeError) return json(res, 400, { error: e.message });
    throw e;
  }

  return json(res, 200, {
    strategy: { name: result.strategy.name, describe: result.strategy.describe },
    known: Object.keys(STRATEGIES),
    config: result.config,
    summary: result.summary,
    // What the replay would not answer, and why. A backtest read without this is
    // a backtest read without its denominator.
    skipped: result.skipped,
    byFidelity: result.byFidelity,
    recordsConsidered: result.recordsConsidered,
    trades: result.trades.slice(-limit).reverse(),
    tradesReturned: Math.min(limit, result.trades.length),
    tradesTotal: result.trades.length,
    equity: result.equity.length > 2000 ? sample(result.equity, 2000) : result.equity,
    equityPoints: result.equity.length,
  });
}

/**
 * The same strategy, with the exit fields the caller named forced on top.
 *
 * Setting `strategy.exit` alone is not enough, and the reason is easy to miss. A
 * strategy may hand back an exit for one particular coin — the sniper does, to
 * leave when the deployer sells — and runBacktest merges that *after* the
 * strategy's own rule, which is right: a per-coin decision should beat a blanket
 * one. It also means an override parked in `strategy.exit` loses to it. So a
 * request for a different target would have applied to most coins and quietly not
 * to the ones that dumped, which is worse than not offering the parameter.
 *
 * It does not fire on the corpus in this checkout — the sniper's coins have no
 * candles, so the dump second can never be placed — which is exactly why it is
 * worth writing down rather than waiting to notice.
 */
function withExitOverride(strategy, override) {
  if (!Object.keys(override).length) return strategy;
  return {
    ...strategy,
    exit: { ...DEFAULT_EXIT, ...(strategy.exit ?? {}), ...override },
    shouldEnter(record, context) {
      const decision = strategy.shouldEnter(record, context);
      if (!decision || typeof decision !== 'object' || !decision.exit) return decision;
      return { ...decision, exit: { ...decision.exit, ...override } };
    },
  };
}

/**
 * Thin a series down to `n` points, keeping the first and the last.
 *
 * Evenly spaced rather than smoothed, because the curve is read for its shape and
 * a mean would round off the drawdown that is the whole reason to look at it.
 */
function sample(rows, n) {
  if (rows.length <= n) return rows;
  const step = (rows.length - 1) / (n - 1);
  const out = [];
  for (let i = 0; i < n; i++) out.push(rows[Math.round(i * step)]);
  return out;
}

function round(n, dp = 4) {
  const f = 10 ** dp;
  return Math.round(Number(n) * f) / f;
}

/** Everything the watcher has ever written, indexed in memory. */
class Store {
  constructor(dir) {
    this.dir = dir;
    this.coins = [];
    this.byMint = new Map();
    this.byWallet = new Map(); // wallet -> array of coin indexes
    this.sizes = new Map(); // file -> bytes read, so a growing log is only re-read at the tail
  }

  files() {
    if (!fs.existsSync(this.dir)) return [];
    return fs
      .readdirSync(this.dir)
      .filter((f) => /^coins-\d{4}-\d{2}-\d{2}\.jsonl$/.test(f))
      .sort()
      .map((f) => path.join(this.dir, f));
  }

  load() {
    for (const file of this.files()) this.readFrom(file);
  }

  /** Index a record the moment the watcher produces it, rather than re-reading the file for it. */
  add(c) {
    if (!c?.mint || this.byMint.has(c.mint)) return;
    const i = this.coins.push(c) - 1;
    this.byMint.set(c.mint, c);
    for (const w of c.who || []) {
      let list = this.byWallet.get(w.w);
      if (!list) this.byWallet.set(w.w, (list = []));
      list.push(i);
    }
  }

  /** Pick up whatever the watcher appended since last time. Cheap enough to call per request. */
  refresh() {
    const now = Date.now();
    if (this._last && now - this._last < 2000) return;
    this._last = now;
    for (const file of this.files()) this.readFrom(file);
  }

  readFrom(file) {
    const size = fs.statSync(file).size;
    const from = this.sizes.get(file) || 0;
    if (size <= from) return;

    const fd = fs.openSync(file, 'r');
    const buf = Buffer.alloc(size - from);
    fs.readSync(fd, buf, 0, buf.length, from);
    fs.closeSync(fd);

    const text = buf.toString('utf8');
    // A partial final line means the watcher is mid-write; leave it for next time.
    const cut = text.lastIndexOf('\n');
    if (cut < 0) return;
    this.sizes.set(file, from + Buffer.byteLength(text.slice(0, cut + 1)));

    for (const line of text.slice(0, cut).split('\n')) {
      if (!line) continue;
      let c;
      try {
        c = JSON.parse(line);
      } catch {
        continue;
      }
      if (this.byMint.has(c.mint)) continue;
      const i = this.coins.push(c) - 1;
      this.byMint.set(c.mint, c);
      for (const w of c.who || []) {
        let list = this.byWallet.get(w.w);
        if (!list) this.byWallet.set(w.w, (list = []));
        list.push(i);
      }
    }
  }

  /**
   * The picture for one coin: its wallets, how big each is here, how practised
   * each is across everything we've seen, and which of them keep turning up
   * together elsewhere.
   */
  graph(coin) {
    const who = coin.who || [];
    const nodes = who.map((w) => {
      const seen = this.byWallet.get(w.w) || [];
      const mults = [];
      for (const i of seen) {
        const m = this.coins[i]?.outcome?.peakMult;
        if (m) mults.push(m);
      }
      return {
        id: w.w,
        sol: Number((w.in + w.out).toFixed(4)),
        bought: w.in,
        sold: w.out,
        trades: w.n,
        at: w.at,
        // How many coins in our whole log this wallet has touched. The number
        // that stops a busy bot from looking like a conspiracy.
        seen: seen.length,
        share: this.coins.length ? Number((seen.length / this.coins.length).toFixed(4)) : 0,
        // Average best move of every coin this wallet appeared on. Only means
        // anything once `seen` is more than a handful — the UI says so.
        track: mults.length ? Number((mults.reduce((a, b) => a + b, 0) / mults.length).toFixed(3)) : null,
        trackN: mults.length,
      };
    });

    // Which other coins each wallet was on, so pairs can be intersected.
    const sets = new Map(nodes.map((n) => [n.id, new Set(this.byWallet.get(n.id) || [])]));
    const links = [];
    for (let a = 0; a < nodes.length; a++) {
      for (let b = a + 1; b < nodes.length; b++) {
        const A = sets.get(nodes[a].id);
        const B = sets.get(nodes[b].id);
        let shared = 0;
        const [small, big] = A.size < B.size ? [A, B] : [B, A];
        for (const i of small) if (big.has(i)) shared++;
        if (shared < 2) continue; // this coin alone is not evidence of anything
        // Measured against the rarer of the two. Two wallets on ten coins each,
        // eight of them shared, is a pair. A bot on 5,000 coins sharing eight
        // with you is a coincidence.
        const strength = shared / Math.min(A.size, B.size);
        links.push({ a: nodes[a].id, b: nodes[b].id, shared, strength: Number(strength.toFixed(3)) });
      }
    }
    links.sort((x, y) => y.strength - x.strength);
    return { nodes, links: links.slice(0, 400), corpus: this.coins.length };
  }
}

function statics(url, res) {
  if (url.pathname === '/vendor/lightweight-charts.js') {
    const vendor = path.join(ROOT, 'node_modules', 'lightweight-charts', 'dist', 'lightweight-charts.standalone.production.js');
    if (!fs.existsSync(vendor)) { res.writeHead(404); return res.end('run npm install'); }
    res.writeHead(200, { 'content-type': 'text/javascript', 'cache-control': 'public, max-age=86400' });
    return fs.createReadStream(vendor).pipe(res);
  }
  const rel = url.pathname === '/' ? 'index.html' : url.pathname.slice(1);
  const file = path.join(UI, rel);
  // Never serve outside ui/, whatever the path claims to be.
  if (!file.startsWith(UI) || !fs.existsSync(file)) {
    res.writeHead(404, { 'content-type': 'text/plain' });
    return res.end('not found');
  }
  res.writeHead(200, { 'content-type': TYPES[path.extname(file)] || 'application/octet-stream' });
  fs.createReadStream(file).pipe(res);
}

const json = (res, code, body) => {
  res.writeHead(code, { 'content-type': 'application/json' });
  res.end(JSON.stringify(body));
};

function openBrowser(url) {
  const cmd = process.platform === 'darwin' ? 'open' : process.platform === 'win32' ? 'start' : 'xdg-open';
  import('node:child_process').then(({ spawn }) => {
    try {
      spawn(cmd, [url], { stdio: 'ignore', detached: true, shell: process.platform === 'win32' }).unref();
    } catch {
      /* the URL is printed either way */
    }
  });
}
