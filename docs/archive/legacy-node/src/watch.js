// Seeing, and writing down what was seen.
//
// This turns the firehose into something a person can read: every new coin as it
// appears, the story it claims to be about, who opened it, and what happened to
// its price. It scores nothing and buys nothing.
//
// The one question it is built to answer: does a coin whose story is real — a
// fresh tweet, from an account with a following — behave differently from one
// with no link at all? That cannot be answered from the chain, and it cannot be
// answered from a screen either, so every coin is also written to a file.
import { Socket } from './ws.js';
import { PUMP_PROGRAM, programData, decodeEvent } from './pump.js';
import { Social, describe } from './social.js';
import { TweetTracker } from './tweets.js';
import { Records } from './record.js';
import { Tracker } from './track.js';
import { Db } from './db.js';
import { AuditLogger } from './audit.js';

const LAMPORTS = 1_000_000_000;

export const DEFAULTS = {
  seconds: 3, // how long to watch a new coin before summarising its opening
  follow: 60, // ...and how long to keep following its price for the record
  all: false, // print every single trade, not just the summaries
  save: true, // append one line per coin to data/coins-YYYY-MM-DD.jsonl
  tweets: true, // ...and follow each linked tweet's engagement for ten minutes
  statusMs: 30_000,
  socialWaitMs: 6_000, // how long a summary will wait on a slow social lookup
};

export function watch({ wsUrl, opts = {}, out = console.log, status = console.error, on = {} }) {
  const cfg = { ...DEFAULTS, ...opts };
  const social = new Social();
  // Coins go to both the file and the database. Tweets stay on file only for
  // now — their shape is nothing like a coin's and they have no table yet.
  const db = cfg.save ? new Db(cfg.dir ? { dir: cfg.dir } : {}) : null;
  // What the run did to itself: sockets dropping, gaps, lines written, decode
  // failures. Rotating NDJSON is the primary copy; the database gets the same
  // events so they can be joined against the coins they happened around.
  const audit = db ? new AuditLogger({ dir: db.dir, db }) : null;
  const records = cfg.save ? new Records({ key: 'mint', db, audit }) : null;
  const tracker = new Tracker({ dir: cfg.dir ?? null, save: cfg.save });
  const tweets = cfg.tweets ? new TweetTracker({ social, save: cfg.save }) : null;

  const live = new Map(); // coins still being followed
  const names = new Map(); // mint -> ticker, so a graduation later has a name

  const totals = { launches: 0, trades: 0, grads: 0, gaps: 0, since: Date.now() };
  const window = { launches: 0, trades: 0 };

  const request = {
    jsonrpc: '2.0',
    id: 1,
    method: 'logsSubscribe',
    params: [{ mentions: [PUMP_PROGRAM] }, { commitment: 'confirmed' }],
  };

  const seen = new Set(); // signatures, so a reconnect cannot double-count

  const sock = new Socket({
    url: wsUrl,
    label: redact(wsUrl),
    request,
    audit,
    onStatus: status,
    onGap: (g) => {
      totals.gaps++;
      // A silent reconnect makes a hole in the picture look like a quiet market.
      status(`missed ${(g.ms / 1000).toFixed(1)}s while disconnected (${g.reason})`);
    },
    onMessage: (msg) => {
      if (msg.id === request.id) return status('subscribed');
      if (msg.method !== 'logsNotification') return;
      handle(msg.params?.result);
    },
  });

  function handle(result) {
    const value = result?.value;
    if (!value || value.err) return;

    const sig = value.signature;
    if (!sig || seen.has(sig)) return;
    seen.add(sig);
    if (seen.size > 50_000) for (const s of [...seen].slice(0, 10_000)) seen.delete(s);

    for (const b64 of programData(value.logs)) {
      const ev = decodeEvent(b64);
      if (!ev) continue;
      if (ev.kind === 'create') onLaunch(ev);
      else if (ev.kind === 'trade') onTrade(ev);
      else if (ev.kind === 'complete') onGraduate(ev);
    }
  }

  function onLaunch(ev) {
    const t = Date.now();
    totals.launches++;
    window.launches++;
    names.set(ev.mint, ev.symbol);
    if (names.size > 20_000) for (const k of [...names.keys()].slice(0, 5_000)) names.delete(k);

    const coin = {
      t,
      mint: ev.mint,
      symbol: ev.symbol,
      name: ev.name,
      creator: ev.creator ?? ev.user ?? null,
      // The metadata link was always fetched for the story lookup but never
      // written down; keeping it means a record can be re-read later without
      // going back to the chain for it.
      uri: ev.uri ?? null,
      // Whole tokens, not base units — pump mints carry six decimals. Only the
      // extended part of the create event has it, so it can be missing.
      supply: ev.tokenTotalSupply != null ? Number(ev.tokenTotalSupply) / 1e6 : null,
      // The deployer's own first buy, filled in by the first trade that matches.
      initialBuySol: null,
      buyers: new Set(),
      sellers: new Set(),
      // Who, not just how many. Counts are enough to score a coin but useless
      // for asking whether the same people keep showing up together.
      who: new Map(),
      solIn: 0,
      solOut: 0,
      trades: 0,
      entry: null, // price at the `seconds` mark — what you'd have paid
      peak: 0,
      peakAt: null,
      last: 0,
      // The shape of the move, not just its extremes. Every time the price sets
      // a new high or a new low against the entry, that moment is kept. A stop
      // loss cannot be tested without knowing whether the dip came before or
      // after the rise, and peak-and-end alone cannot tell those apart.
      hi: 1,
      lo: 1,
      highs: [],
      lows: [],
      candles: [],
      social: null,
    };
    live.set(ev.mint, coin);
    on.launch?.({ t, mint: ev.mint, symbol: ev.symbol, name: ev.name });
    out(`${clock()}  ${tag('new ', 'green')} ${ticker(ev.symbol)}  ${dim(trim(ev.name, 24))}  ${dim(short(ev.mint))}`);

    // The story lookup runs on its own clock; a slow IPFS gateway must never
    // hold up reading the socket.
    social
      .lookup(ev.uri, t)
      .then((s) => {
        coin.social = s;
        // The tweet outlives the coin's follow window, so it is followed
        // separately and joined back on the tweet id at analysis time.
        if (s?.link?.statusId) tweets?.track(s.link, ev.mint, s.x);
      })
      .catch(() => {
        coin.social = { kind: 'nometa' };
      });

    setTimeout(() => summarise(coin), cfg.seconds * 1000);
    setTimeout(() => finish(coin), cfg.follow * 1000);
  }

  function onTrade(ev) {
    totals.trades++;
    window.trades++;
    const coin = live.get(ev.mint);
    // Past its follow window a coin is no longer written down, but it is still
    // watched: this is where every price after the first minute comes from, and
    // it costs nothing because the trade already arrived.
    if (!coin) {
      const p = price(ev);
      if (p && tracker.trade(ev.mint, p)) {
        on.tick?.({ mint: ev.mint, price: p, t: Date.now() });
      }
      return;
    }

    const sol = Number(ev.sol) / LAMPORTS;
    const age = (Date.now() - coin.t) / 1000;
    coin.trades++;
    // The deployer buying their own launch is the one trade worth naming on its
    // own, so it is caught here rather than inferred from the wallet totals —
    // those are sums over the whole window and would say something different.
    if (coin.initialBuySol === null && ev.isBuy && coin.creator && ev.user === coin.creator) {
      coin.initialBuySol = round(sol);
    }
    if (ev.isBuy) {
      coin.buyers.add(ev.user);
      coin.solIn += sol;
    } else {
      coin.sellers.add(ev.user);
      coin.solOut += sol;
    }

    // A wallet is capped per coin so one bot trading a hundred times cannot
    // dominate the file, but the cap is on distinct wallets rather than trades
    // so the picture of who was there stays complete for normal coins.
    let w = coin.who.get(ev.user);
    if (!w && coin.who.size < 200) {
      w = { w: ev.user, in: 0, out: 0, n: 0, at: round(age, 2) };
      coin.who.set(ev.user, w);
    }
    if (w) {
      w.n++;
      if (ev.isBuy) w.in += sol;
      else w.out += sol;
    }

    // Price is what the curve says after this trade, not what anyone paid in fees.
    const p = price(ev);
    if (p) {
      coin.last = p;
      const bucket = Math.floor(age);
      let candle = coin.candles.at(-1);
      if (!candle || candle.s !== bucket) {
        candle = { s: bucket, o: p, h: p, l: p, c: p, volume: 0, buys: 0, sells: 0 };
        coin.candles.push(candle);
      }
      candle.h = Math.max(candle.h, p);
      candle.l = Math.min(candle.l, p);
      candle.c = p;
      candle.volume = round(candle.volume + sol);
      if (ev.isBuy) candle.buys++;
      else candle.sells++;
      on.trade?.({
        t: Date.now(), mint: coin.mint, symbol: coin.symbol, side: ev.isBuy ? 'buy' : 'sell',
        wallet: ev.user, sol: round(sol), price: p, age: round(age, 2),
      });
      // Only count a peak once there is an entry to measure it against, so the
      // number always means "from where you could have bought".
      if (coin.entry) {
        const m = p / coin.entry;
        if (p > coin.peak) {
          coin.peak = p;
          coin.peakAt = Math.round(age);
        }
        // Only the turning points are kept, so the record stays small while
        // still saying exactly when any given level was first crossed.
        if (m > coin.hi && coin.highs.length < 60) {
          coin.hi = m;
          coin.highs.push([round(age, 1), round(m, 4)]);
        } else if (m < coin.lo && coin.lows.length < 60) {
          coin.lo = m;
          coin.lows.push([round(age, 1), round(m, 4)]);
        }
      }
    }

    if (cfg.all) {
      const side = ev.isBuy ? tag('buy ', 'green') : tag('sell', 'red');
      out(`${clock()}  ${side} ${ticker(coin.symbol)}  ${sol.toFixed(3)} SOL  ${dim(short(ev.user))}`);
    }
  }

  function onGraduate(ev) {
    totals.grads++;
    const sym = names.get(ev.mint);
    // Roughly one coin in a hundred gets this far. Always worth a line.
    out(`${clock()}  ${tag('grad', 'cyan')} ${ticker(sym ?? '?')}  ${dim('left pump.fun for a real exchange')}  ${dim(short(ev.mint))}`);
  }

  /** The `seconds` mark: fix the entry price, then print one line about the open. */
  async function summarise(coin) {
    coin.entry = coin.last || null;
    coin.peak = coin.entry || 0;
    coin.peakAt = coin.entry ? cfg.seconds : null;

    // Freeze the opening here. The coin keeps trading for another minute, and if
    // the record counted wallets up to *then* the signal being graded would
    // quietly include information from after the decision it is meant to inform.
    coin.open = {
      seconds: cfg.seconds,
      wallets: coin.buyers.size,
      sellers: coin.sellers.size,
      solIn: round(coin.solIn),
      solOut: round(coin.solOut),
      trades: coin.trades,
    };

    // Give a slow lookup a moment rather than printing "looking…" forever, but
    // never let one hang the line.
    const until = Date.now() + cfg.socialWaitMs;
    while (!coin.social && Date.now() < until) await sleep(250);

    // Summaries land seconds after the launch line, by which time other coins have
    // scrolled past — so each repeats its own ticker rather than relying on the
    // reader to remember which arrow belongs to which coin.
    const head = `${blank()}  ${dim('↳')}    ${ticker(coin.symbol)}`;
    const story = story_(coin.social);
    if (coin.trades === 0) {
      out(`${head}  ${dim(`${cfg.seconds}s: nobody`)}   ${story}`);
      return;
    }
    on.open?.({
      mint: coin.mint,
      symbol: coin.symbol,
      name: coin.name,
      t: coin.t,
      wallets: coin.buyers.size,
      sellers: coin.sellers.size,
      solIn: round(coin.solIn),
      solOut: round(coin.solOut),
      trades: coin.trades,
      story: coin.social ? describe(coin.social) : null,
      handle: coin.social?.handle ?? null,
      kind: coin.social?.kind ?? null,
      followers: coin.social?.followers ?? null,
      tweetAgeSec: coin.social?.tweetAgeSec ?? null,
      nth: coin.social?.nth ?? null,
      failed: coin.social?.failed ?? false,
      telegram: coin.social?.telegram ?? null,
      website: coin.social?.website ?? null,
      entry: coin.entry,
      last: coin.last,
      // The wallets that were in before the cutoff. Who they are is the whole
      // signal now, so the opening summary has to carry them.
      who: [...coin.who.values()]
        .filter((w) => w.at <= cfg.seconds)
        .map((w) => ({ w: w.w, in: round(w.in), out: round(w.out), n: w.n, at: w.at })),
      creator: coin.creator,
    });
    out(
      `${head}  ${pad(coin.buyers.size, 3)} ${plural(coin.buyers.size, 'wallet')} · ` +
        `${coin.solIn.toFixed(2)} SOL in`.padEnd(13) +
        `  ${story}`,
    );
  }

  /** The `follow` mark: write the coin down and forget it. */
  function finish(coin) {
    live.delete(coin.mint);
    const s = coin.social;
    const rec = {
      t: coin.t,
      mint: coin.mint,
      symbol: coin.symbol,
      name: coin.name,
      creator: coin.creator,
      uri: coin.uri,
      supply: coin.supply,
      initialBuySol: coin.initialBuySol,
      social: s
        ? {
            kind: s.kind,
            handle: s.handle ?? null,
            // Joins this coin to its tweet's engagement series in tweets-*.jsonl
            statusId: s.statusId ?? null,
            followers: s.followers ?? null,
            accountDays: s.accountDays ?? null,
            tweetAgeSec: s.tweetAgeSec ?? null,
            likes: s.likes ?? null,
            retweets: s.retweets ?? null,
            views: s.views ?? null,
            nth: s.nth ?? null,
            telegram: s.telegram ?? null,
            website: s.website ?? null,
            words: s.words ?? null,
            failed: s.failed ?? false,
          }
        : null,
      // What was known at the `seconds` mark, and nothing later.
      open: coin.open ?? null,
      // Every wallet that touched this coin, with what it did and when it first
      // appeared. `at` is seconds after launch, so a reader can rebuild who was
      // there at any cutoff rather than trusting ours.
      who: [...coin.who.values()].map((w) => ({ ...w, in: round(w.in), out: round(w.out) })),
      // The whole follow window, for context only — never a decision input.
      total: {
        wallets: coin.buyers.size,
        sellers: coin.sellers.size,
        solIn: round(coin.solIn),
        solOut: round(coin.solOut),
        trades: coin.trades,
      },
      // Prices are curve ratios, not SOL — only the multiples between them mean
      // anything, and that is all the grading needs.
      outcome: {
        follow: cfg.follow,
        entry: coin.entry,
        peak: coin.peak || null,
        last: coin.last || null,
        peakMult: coin.entry ? round(coin.peak / coin.entry, 4) : null,
        endMult: coin.entry ? round(coin.last / coin.entry, 4) : null,
        peakAtSec: coin.peakAt,
        trades: coin.trades,
        // Multiples of the entry price, in the order they happened. `highs` is
        // every new best, `lows` every new worst. Between them any exit rule —
        // stop, target, or both — can be replayed exactly rather than guessed.
        highs: coin.highs,
        lows: coin.lows,
      },
      market: {
        candleSeconds: 1,
        candles: coin.candles.map((x) => ({
          s: x.s, o: x.o, h: x.h, l: x.l, c: x.c,
          volume: round(x.volume), buys: x.buys, sells: x.sells,
        })),
      },
    };
    records?.write(rec);
    on.coin?.(rec);
    // Written down, but no longer forgotten: the tracker keeps its price current
    // so questions about anything past the first minute have an answer.
    tracker.adopt(rec);
    on.tracked?.(tracker.size);
  }

  const ticking = setInterval(() => {
    tracker.sweep();
    const mins = cfg.statusMs / 60_000;
    const st = social.stats;
    status(
      `${clock()}  ${(window.launches / mins).toFixed(0)} new coins/min · ` +
        `${(window.trades / mins).toFixed(0)} trades/min · ` +
        `${totals.grads} graduated · ${totals.gaps} gaps · ` +
        `stories ${st.metaOk}/${st.metaOk + st.metaFail}, X ${st.xOk}/${st.xOk + st.xFail} (${st.cached} shared)` +
        (tweets ? ` · following ${tweets.watching.size} tweets, ${tweets.stats.samples} samples` : '') +
        (records ? ` · ${records.written} coins written` : ''),
    );
    window.launches = 0;
    window.trades = 0;
  }, cfg.statusMs);

  sock.start();

  return {
    totals,
    tracker,
    async stop() {
      clearInterval(ticking);
      await sock.stop();
      // Coins still inside their follow window are written as they stand rather
      // than dropped; a short record is a fact, a missing one is a hole.
      for (const coin of [...live.values()]) finish(coin);
      await tweets?.close();
      await records?.close();
      // The wallet rollup is derived, so it is recomputed from the stored coins
      // rather than kept up to date on the hot path.
      if (db) {
        try {
          db.rebuildWallets();
        } catch (err) {
          status(`wallet rollup failed: ${err.message}`);
        }
      }
      await audit?.close();
      // Tracked coins are written out too — a run's worth of price history is
      // the only thing that makes the longer horizons answerable later.
      tracker.close();
      const mins = (Date.now() - totals.since) / 60_000;
      status(
        `\nwatched ${mins.toFixed(1)} min: ${totals.launches} new coins, ` +
          `${totals.trades} trades, ${totals.grads} graduated, ${totals.gaps} gaps` +
          (records ? `\nwrote ${records.written} coins to ${records.file}` : '') +
          (db ? `\nstored ${records?.stored ?? 0} new coins in ${db.file} (${db.count()} total)` : '') +
          (tweets ? `\nwrote ${tweets.written} tweets, ${tweets.stats.samples} engagement samples` : ''),
      );
      db?.close();
    },
  };
}

/** Endpoints carry API keys, and this text gets pasted. */
export function redact(url) {
  try {
    const u = new URL(url);
    if (u.searchParams.has('api-key')) u.searchParams.set('api-key', '***');
    return u.origin + u.pathname + (u.search ? '?' + u.searchParams.toString() : '');
  } catch {
    return 'invalid-url';
  }
}

/** Price implied by the curve after a trade. A ratio, not SOL. */
function price(ev) {
  const s = Number(ev.virtualSolReserves);
  const t = Number(ev.virtualTokenReserves);
  // Pump tokens use 6 decimals while SOL uses 9. This returns SOL per whole
  // token; ratios are unchanged, but displayed and paper-trade prices are real.
  return s > 0 && t > 0 ? s / t / 1e3 : null;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const round = (n, d = 4) => (Number.isFinite(n) ? Number(n.toFixed(d)) : null);

const colour = process.stdout.isTTY && !process.env.NO_COLOR;
const CODES = { green: 32, red: 31, cyan: 36, yellow: 33 };

const paint = (s, c) => (colour ? `\x1b[${CODES[c]}m${s}\x1b[0m` : s);
const dim = (s) => (colour ? `\x1b[2m${s}\x1b[0m` : s);
const tag = (s, c) => paint(s, c);
const ticker = (s) => pad('$' + trim(s || '?', 10), 11);
const clock = () => new Date().toTimeString().slice(0, 8);
const blank = () => ' '.repeat(8);
const short = (a) => (a ? a.slice(0, 4) + '…' + a.slice(-4) : '?');
const trim = (s, n) => ((s || '').length > n ? s.slice(0, n - 1) + '…' : s || '');
const pad = (v, n) => String(v).padStart(n);
const plural = (n, w) => (n === 1 ? w : w + 's');

// A real, readable story is worth making visible; a missing one is worth making
// invisible, because most coins have none and the eye should skip them.
function story_(s) {
  const text = describe(s);
  if (!s || s.kind === 'none' || s.kind === 'nometa' || s.kind === 'other' || s.failed) return dim(text);
  const loud = (s.followers ?? 0) >= 10_000 || (s.nth ?? 1) > 1;
  return loud ? paint(text, 'yellow') : text;
}
