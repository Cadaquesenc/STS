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
import { Rpc } from './rpc.js';
import { AuditLogger } from './audit.js';
import { LinkHealth } from './link.js';
import { SCHEMA, newSessionId, sessionFile, closeFacts, classifyErr, sampled } from './session.js';

const LAMPORTS = 1_000_000_000;

export const DEFAULTS = {
  seconds: 3, // how long to watch a new coin before summarising its opening
  follow: 60, // ...and how long to keep following its price for the record
  all: false, // print every single trade, not just the summaries
  save: true, // append one line per coin to data/coins-<session>.jsonl
  tweets: true, // ...and follow each linked tweet's engagement for ten minutes
  statusMs: 30_000,
  socialWaitMs: 6_000, // how long a summary will wait on a slow social lookup
  // Written whether or not anything happened, so uptime is measured rather than
  // inferred from how far apart launches happen to fall. W21 §3 asks for 10s.
  heartbeatMs: 10_000,
  // Turning points kept per coin. The old cap was 60 and it froze the running
  // extreme with it, which bit 0.2–1.2% of priced coins — and it bit the
  // winners, because a coin that keeps making new highs is the one that runs
  // out of room. See `onTrade`.
  highsCap: 1000,
  whoCap: 200, // distinct wallets kept per coin
  // Every sell inside the follow window, with the wallet that made it. The
  // candles counted sells without naming anybody, so "is the creator still
  // holding at second N" was only ever answerable as "has anybody sold by
  // second N" — and the two are not the same question. The address is already
  // on the trade event; it was being thrown away, not missing. Measured on
  // 2026-08-20: median 2 sells a coin, mean 15.5, p99 209, max 570.
  sellsCap: 1000,
  // Trades whose fee basis points are zero — the exact signature of the 2026
  // reserve anomaly. They are kept in full because they are the ones that need
  // reconstructing, and there are few of them per coin. See `onTrade`.
  zeroFeeCap: 1000,
  failLog: true, // write a row for sampled failed transactions
  // On-chain failures outrun successes about 14 to 1, so keeping every one is a
  // real storage decision: ~190 bytes each is 0.5–4 GB for a four-hour session
  // against ~170 MB for everything else. A deterministic 1-in-N sample keyed on
  // the signature goes to its own file with the rate on every row, and the
  // per-minute rollup below keeps the totals exact. Set 1 to keep them all.
  failSample: 50,
  gitCommit: null, // stamped into the session header by the entry point
};

export function watch({ wsUrl, opts = {}, out = console.log, status = console.error, on = {} }) {
  const cfg = { ...DEFAULTS, ...opts };
  const startedAt = Date.now();
  // Every record carries this. Seven calendar days in the corpus were really
  // nine sessions, and the day everybody treated as an independent holdout was
  // the tail of the previous run past midnight — a fact that took a forensic
  // exercise to establish and should have been a lookup.
  const sid = cfg.sid ?? newSessionId(startedAt);
  const session = sessionFile(sid, startedAt);
  const social = new Social();
  // Coins go to both the file and the database. Tweets stay on file only for
  // now — their shape is nothing like a coin's and they have no table yet.
  const db = cfg.save ? new Db(cfg.dir ? { dir: cfg.dir } : {}) : null;
  // What the run did to itself: sockets dropping, gaps, lines written, decode
  // failures. Rotating NDJSON is the primary copy; the database gets the same
  // events so they can be joined against the coins they happened around.
  const audit = db ? new AuditLogger({ dir: db.dir, db }) : null;
  // `dir` reaches all four writers or none of them. It used to reach the
  // database and the tracker and not the coin log, so a run pointed somewhere
  // else wrote its coins to the default directory anyway and said nothing —
  // which is the same silence as a field that is quietly constant.
  const where = cfg.dir ? { dir: cfg.dir } : {};
  const records = cfg.save ? new Records({ key: 'mint', db, audit, session, ...where }) : null;
  // Failed transactions get their own file. They outnumber the coins about
  // fourteen to one, and putting them in with the coins would make every pass
  // that streams the coin log walk fifteen times as many lines forever.
  const fails = cfg.save && cfg.failLog ? new Records({ name: 'fails', session, ...where }) : null;
  // Who paid for the opening buyers. Answers are cached in the database, so this
  // is handed the same connection the coins go to and warms itself from it. With
  // no endpoint configured it reports `enabled: false` and every launch is
  // recorded exactly as it was before — the funding field is simply absent.
  const rpc = cfg.rpc === false ? null : new Rpc({
    url: cfg.rpcUrl ?? process.env.STS_RPC ?? null,
    store: db,
    audit,
    ...(cfg.rpcOptions ?? {}),
  });
  const tracker = new Tracker({ dir: cfg.dir ?? null, save: cfg.save, session, sid });
  const tweets = cfg.tweets ? new TweetTracker({ social, save: cfg.save, session, ...where }) : null;

  const live = new Map(); // coins still being followed
  const names = new Map(); // mint -> ticker, so a graduation later has a name

  const totals = {
    launches: 0, trades: 0, grads: 0, gaps: 0, since: startedAt,
    // Downtime, in ms, accumulated over the run. Frozen per coin when it
    // launches and diffed when it is written, which is how a record can say how
    // much of *its own* window the feed was missing for. The follow timer fires
    // whether or not the socket was alive; nothing used to notice.
    gapMs: 0,
    failed: 0, failLogged: 0, refused: 0, truncated: 0, beats: 0, connectedBeats: 0,
  };
  let seq = 0; // record index within this session
  // `since` so a rate can be divided by the time that has actually elapsed. The
  // counters reset every statusMs, and dividing by the nominal window instead
  // reports a third of the real rate to anyone who asks ten seconds after a reset.
  const window = { launches: 0, trades: 0, since: Date.now() };

  const request = {
    jsonrpc: '2.0',
    id: 1,
    method: 'logsSubscribe',
    params: [{ mentions: [PUMP_PROGRAM] }, { commitment: 'confirmed' }],
  };

  const seen = new Set(); // signatures, so a reconnect cannot double-count
  // Failures get their own dedup set rather than sharing that one. Sharing it
  // would let a redelivered burst of failures evict every successful signature
  // inside a few minutes, and the next reconnect would then double-count real
  // trades. The dedup happens *before* the `err` branch either way, which is
  // what the old counter got wrong: it incremented first, so redelivered
  // failures were counted twice and every published failure rate was an upper
  // bound by an amount nobody could measure.
  const seenFail = new Set();
  // A minute of failures at a time, so the totals stay exact even though only a
  // sample of the rows is kept. A counter with no rows behind it is what hid
  // this defect for weeks; a rollup that can be re-derived from the file is not.
  let failWindow = null;
  let draining = false; // shutting down: finish what is live, admit nothing new
  let drainCancelled = false; // ...and a second Ctrl-C means now, not in a minute

  // The state of the link itself, kept apart from the state of the browser's
  // connection to us — the two were being reported as one thing, and the one
  // that can actually fail was the one not being watched. See link.js.
  const link = new LinkHealth({ endpoint: redact(wsUrl) });

  const sock = new Socket({
    url: wsUrl,
    label: redact(wsUrl),
    request,
    audit,
    onStatus: (m) => { status(m); on.link?.(link.report(sock)); },
    onGap: (g) => {
      totals.gaps++;
      totals.gapMs += g.ms || 0;
      link.gap(g);
      // A silent reconnect makes a hole in the picture look like a quiet market.
      // It used to reach a counter and the terminal and nothing else, so the
      // hole was in the file with nothing in the file to say so. `from` is the
      // last message actually received, not when the close was noticed.
      records?.writeMeta({ k: 'gap', v: SCHEMA, sid, t: Date.now(), ...g });
      status(`missed ${(g.ms / 1000).toFixed(1)}s while disconnected (${g.reason})`);
    },
    onMessage: (msg) => {
      if (msg.id === request.id) return status('subscribed');
      if (msg.method !== 'logsNotification') return;
      handle(msg.params?.result);
    },
  });

  // Where in a slot we saw this transaction. Not the block index —
  // `logsSubscribe` does not carry one, and recovering the real one costs a
  // `getBlock` per slot, offline. It does order the pump transactions we
  // observed inside a slot, which is what a contention question needs, and it
  // costs nothing. Counted per slot rather than reset when the slot changes,
  // because notifications interleave across slots and a reset would hand out
  // the same index twice. Bounded to the last few hundred slots.
  const slotN = new Map();

  function handle(result) {
    const value = result?.value;
    if (!value) return;

    const sig = value.signature;
    const slot = result.context?.slot ?? null;

    let si = null;
    if (slot !== null) {
      si = slotN.get(slot) ?? 0;
      slotN.set(slot, si + 1);
      if (slotN.size > 256) for (const k of [...slotN.keys()].slice(0, 128)) slotN.delete(k);
    }

    // A transaction that mentioned the pump program and failed on chain. It
    // still landed in a block and it still paid its fee, so it is part of what
    // trading here costs. This used to be `stats.failed++; return` — counted
    // and thrown away, which cannot tell a market where everyone fails fourteen
    // times per fill from one where a few bots spam and everyone else lands
    // first try. Those are different markets and different verdicts.
    if (value.err) {
      if (!sig || seenFail.has(sig)) return;
      seenFail.add(sig);
      if (seenFail.size > 100_000) for (const s of [...seenFail].slice(0, 20_000)) seenFail.delete(s);
      onFail(sig, slot, si, value.err);
      return;
    }

    if (!sig || seen.has(sig)) return;
    seen.add(sig);
    if (seen.size > 50_000) for (const s of [...seen].slice(0, 10_000)) seen.delete(s);

    const ctx = { sig, slot, si };
    for (const b64 of programData(value.logs)) {
      const ev = decodeEvent(b64);
      if (!ev) continue;
      // The block's own clock, which every create and trade event carries and
      // which nothing read until now. It is the only end-to-end latency figure
      // available without asking the chain a second question.
      link.sample(ev.ts);
      if (ev.kind === 'create') onLaunch(ev, ctx);
      else if (ev.kind === 'trade') onTrade(ev, ctx);
      else if (ev.kind === 'complete') onGraduate(ev);
    }
  }

  /**
   * One failed transaction: rolled into the minute's totals always, written out
   * if the deterministic sample keeps it.
   *
   * The fee payer is **not** on the `logsSubscribe` payload — it carries
   * `{signature, err, logs}` and no account keys — so answering "whose failures
   * are these" costs one `getTransaction` per signature, off the hot path,
   * against a signature this row records. Nothing here asks the network
   * anything.
   */
  function onFail(sig, slot, si, err) {
    totals.failed++;
    const minute = Math.floor(Date.now() / 60_000);
    if (!failWindow || failWindow.minute !== minute) {
      flushFailAgg();
      failWindow = { minute, n: 0, kept: 0, byErr: Object.create(null) };
    }
    const { e, keepRaw } = classifyErr(err);
    failWindow.n++;
    failWindow.byErr[e] = (failWindow.byErr[e] ?? 0) + 1;

    if (!fails || !sampled(sig, cfg.failSample)) return;
    failWindow.kept++;
    totals.failLogged++;
    fails.writeMeta({
      // `fails-<session>.jsonl` is its own file and gets no `start` header, so
      // without this the failure rows are the one shape in the capture with no
      // version anywhere to read it off.
      k: 'fail', v: SCHEMA, t: Date.now(), sid, sig, slot, si, e,
      // The rate this row survived at, on the row. Scaling a sample back up
      // needs it, and a sample whose rate is not written down is not a sample,
      // it is a hole — the same defect as `follow: 60`.
      rate: cfg.failSample,
      // The raw error only when the shape was not recognised, so an unfamiliar
      // failure mode stays recoverable instead of being flattened into "other".
      ...(keepRaw ? { err } : {}),
    });
  }

  /** Close off the minute of failures and write its rollup. */
  function flushFailAgg() {
    if (!failWindow || !failWindow.n) return;
    records?.writeMeta({
      // `t` is when the rollup was written; `minute` is the bucket it covers.
      // Stamping it with the bucket instead made a two-second session read as
      // forty-four, because the span of a session is the span of its rows.
      k: 'failagg', v: SCHEMA, sid, t: Date.now(),
      minute: failWindow.minute, minuteStart: failWindow.minute * 60_000,
      n: failWindow.n, kept: failWindow.kept,
      rate: cfg.failSample, byErr: failWindow.byErr,
    });
    failWindow = null;
  }

  function onLaunch(ev, ctx = {}) {
    const t = Date.now();
    // A shutdown that keeps admitting launches is a shutdown that manufactures
    // truncated records. During the drain the socket stays up so coins already
    // being followed can reach their own follow mark, and new ones are refused
    // and counted rather than half-watched.
    if (draining) {
      totals.refused++;
      return;
    }
    totals.launches++;
    window.launches++;
    names.set(ev.mint, ev.symbol);
    if (names.size > 20_000) for (const k of [...names.keys()].slice(0, 5_000)) names.delete(k);

    const coin = {
      t,
      seq: seq++,
      // The join key to the cost of landing this transaction, and the clock it
      // is measured against. Neither was recorded at all, which is why the cost
      // model had to be rebuilt weeks later out of 25 transactions in another
      // project's files — and was wrong by a factor of twenty for weeks. Fee,
      // priority fee and compute units are not on the `logsSubscribe` payload;
      // they come from `getTransaction` against this signature, offline. No RPC
      // call goes on this path.
      sig: ctx.sig ?? null,
      slot: ctx.slot ?? null,
      si: ctx.si ?? null,
      // How long the feed had been continuously up when this arrived.
      connectedForSec: sock.connectedForSec(t),
      // The run's downtime total at this instant, so the record written a
      // minute from now can say how much of *this coin's* window was missing.
      down0: totals.gapMs + sock.openDownMs(t),
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
      // The curve this coin opened on, in whole tokens and SOL. Every launch
      // starts on the same constants, but reading them off the event rather than
      // assuming them means a coin launched under different parameters gives a
      // different answer instead of a confidently wrong one.
      curve: ev.virtualSolReserves != null && ev.virtualTokenReserves != null
        ? {
            virtualSol: Number(ev.virtualSolReserves) / 1e9,
            virtualTokens: Number(ev.virtualTokenReserves) / 1e6,
            realTokens: ev.realTokenReserves != null ? Number(ev.realTokenReserves) / 1e6 : null,
          }
        : null,
      // The deployer's own first buy, filled in by the first trade that matches.
      // In SOL because that is what a person reads, and in tokens because what
      // matters for a rug is the share of the supply that SOL bought — and early
      // on the curve, a small number of SOL buys a very large share of it.
      initialBuySol: null,
      initialBuyTokens: null,
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
      // Whether the turning-point lists ran out of room. The old cap was 60 and
      // it stopped the running extreme dead with it, so a coin that kept making
      // new highs was written down as having stopped making them — and said
      // nothing about it. Now the extreme always moves, only the list stops,
      // and the row says which happened.
      highsCapped: false,
      lowsCapped: false,
      // Who sold, and when. One entry per sell: [at, wallet, sol, tokens].
      // Positional like `highs` and `lows`, for the same reason — the shape
      // repeats thousands of times and the key names would be most of it.
      sells: [],
      sellsCapped: false,
      // What fee rate each trade actually paid, counted. A normal pump trade is
      // 95 basis points. A trade at 0 is not a cheap trade, it is the marker of
      // the actor whose trades left the curve in a state the launch curve
      // cannot produce — and the prices printed after it are priced off that
      // impossible state. 18.4% of coins in the corpus are affected and it
      // concentrates in 9 of the 10 largest peaks.
      feeBps: Object.create(null),
      zeroFee: [],
      zeroFeeCapped: false,
      // The pump trading fee actually paid over the window, in SOL. It is on
      // every trade event and has never been written down, so every cost model
      // in this project has used a remembered 1% against a chain that charges
      // 95 basis points. Now it is measured rather than assumed.
      feeSol: 0,
      whoCapped: false,
      // The curve state at the instant the entry price was struck, straight off
      // the event: [vsol, vtok, rsol, rtok]. `entry` is a price and a price is a
      // ratio; this is the absolute state behind it, so a later reader can put
      // an order size on the curve instead of inferring one from a multiple.
      reserves: null,
      curveAtEntry: null,
      // The creator selling their own coin is the one sell worth naming on its
      // own, the way `initialBuySol` names their first buy. Derived from
      // `sells` and checkable against it, so it is a number with the rows still
      // behind it rather than a counter to be taken on trust.
      creatorSellAtSec: null,
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

    // Held so they can be cancelled. Without this, a coin written out early —
    // which is what shutdown does to everything still inside its window — has
    // its two timers still pending, and they fire into a closed recorder
    // afterwards: `summarise` re-freezes an opening nobody will read and
    // `finish` re-adopts the coin into a tracker that has already flushed. The
    // writes are all dropped further down, so nothing was corrupted, but a stop
    // that leaves work scheduled is a stop that has not stopped.
    coin.timers = [
      setTimeout(() => summarise(coin), cfg.seconds * 1000),
      setTimeout(() => finish(coin), cfg.follow * 1000),
    ];
  }

  function onTrade(ev, ctx = {}) {
    totals.trades++;
    window.trades++;
    const coin = live.get(ev.mint);
    // Past its follow window a coin is no longer written down, but it is still
    // watched: this is where every price after the first minute comes from, and
    // it costs nothing because the trade already arrived.
    if (!coin) {
      const p = price(ev);
      if (p && tracker.trade(ev.mint, p)) {
        // The size and side travel with the tick as well as the price. Without
        // them the board's volume column freezes the moment a coin leaves its
        // follow window, which is when most of its volume happens.
        on.tick?.({
          mint: ev.mint, price: p, t: Date.now(),
          sol: round(Number(ev.sol) / LAMPORTS), side: ev.isBuy ? 'buy' : 'sell',
        });
      }
      return;
    }

    const sol = Number(ev.sol) / LAMPORTS;
    // pump mints carry six decimals, so this is whole tokens. It is the field
    // that makes "what share of the supply does this wallet hold" answerable
    // without inferring anything from the price.
    const tokens = ev.tokens != null ? Number(ev.tokens) / 1e6 : 0;
    const age = (Date.now() - coin.t) / 1000;
    coin.trades++;
    // The deployer buying their own launch is the one trade worth naming on its
    // own, so it is caught here rather than inferred from the wallet totals —
    // those are sums over the whole window and would say something different.
    if (coin.initialBuySol === null && ev.isBuy && coin.creator && ev.user === coin.creator) {
      coin.initialBuySol = round(sol);
      coin.initialBuyTokens = round(tokens, 2);
    }
    if (ev.isBuy) {
      coin.buyers.add(ev.user);
      coin.solIn += sol;
    } else {
      coin.sellers.add(ev.user);
      coin.solOut += sol;
      // Recorded before anything else can drop it: a sell with no price, a sell
      // in the first three seconds before entry is struck, and a sell by a
      // wallet past the 200-wallet cap all still count. 71.3% of creators sell
      // their own coin and a third of those do it inside three seconds, so the
      // early ones are the whole point.
      if (coin.sells.length < cfg.sellsCap) {
        coin.sells.push([round(age, 1), ev.user, round(sol), round(tokens, 2)]);
      } else {
        coin.sellsCapped = true;
      }
      if (coin.creatorSellAtSec === null && coin.creator && ev.user === coin.creator) {
        coin.creatorSellAtSec = round(age, 1);
      }
    }

    // The curve state this trade left behind, kept so the entry price can be
    // reported as a state and not only as a ratio. Four numbers, overwritten
    // every trade — it costs nothing and it is the raw form of the number the
    // candles below reduce to a price.
    coin.reserves = reserves(ev);

    // The fee rate this trade paid, and the whole trade kept when that rate is
    // zero. `feeBasisPoints` and `fee` have been decoded on every trade since
    // the decoder was written and neither has ever been written down.
    const bps = ev.feeBasisPoints != null ? Number(ev.feeBasisPoints) : null;
    if (bps !== null) coin.feeBps[bps] = (coin.feeBps[bps] ?? 0) + 1;
    coin.feeSol += Number(ev.fee ?? 0) / LAMPORTS;
    if (bps === 0) {
      if (coin.zeroFee.length < cfg.zeroFeeCap) {
        coin.zeroFee.push([
          round(age, 1), ev.user, round(sol), round(tokens, 2), ev.isBuy ? 1 : 0,
          ...reserves(ev),
          round(Number(ev.fee ?? 0) / LAMPORTS, 9),
        ]);
      } else {
        coin.zeroFeeCapped = true;
      }
    }

    // A wallet is capped per coin so one bot trading a hundred times cannot
    // dominate the file, but the cap is on distinct wallets rather than trades
    // so the picture of who was there stays complete for normal coins.
    let w = coin.who.get(ev.user);
    if (!w && coin.who.size >= cfg.whoCap) coin.whoCapped = true;
    if (!w && coin.who.size < cfg.whoCap) {
      // `tin`/`tout` are the same two sides as `in`/`out`, counted in whole
      // tokens instead of SOL.
      w = { w: ev.user, in: 0, out: 0, tin: 0, tout: 0, n: 0, at: round(age, 2) };
      // The fee ladder: what it cost the wallets that got in at the open, on
      // thousands of transactions a session instead of the 25 the cost model
      // was rebuilt from. Only for the opening buyers — those are the positions
      // a strategy would be competing for, and putting four more fields on all
      // 200 wallets of every coin would cost more bytes than the rest of the
      // record. `slotsAfter` is the landing distance from the launch itself.
      if (age <= cfg.seconds) {
        w.sig = ctx.sig ?? null;
        w.slot = ctx.slot ?? null;
        w.si = ctx.si ?? null;
        w.slotsAfter = ctx.slot != null && coin.slot != null ? ctx.slot - coin.slot : null;
      }
      coin.who.set(ev.user, w);
    }
    if (w) {
      w.n++;
      if (ev.isBuy) {
        w.in += sol;
        w.tin += tokens;
      } else {
        w.out += sol;
        w.tout += tokens;
      }
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
      // The curve state this second closed on, straight off the event.
      //
      // This is the field the corpus most wishes it had. `watch.js` stored only
      // derived open/high/low/close prices, and no per-trade reserve figure was
      // ever written by anything — so when 18.4% of coins turned out to have
      // been priced off an impossible reserve value, **zero of them were
      // recoverable.** A quarter of the dataset's tail is permanently unusable
      // for want of two numbers that were already on the wire, sitting in the
      // decoded event at every single trade. Record raw state, not state you
      // have already reduced: the reduction is always the part you needed back.
      [candle.vsol, candle.vtok, candle.rsol, candle.rtok] = reserves(ev);
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
        //
        // The running extreme moves whatever the list does. It used to be inside
        // the same condition as the push, so once 60 entries were down `hi`
        // froze — and because the low branch was an `else if` behind a test that
        // now always passed, new lows stopped being recorded too. Two separate
        // tests: a multiple cannot be both a new high and a new low.
        if (m > coin.hi) {
          coin.hi = m;
          if (coin.highs.length < cfg.highsCap) coin.highs.push([round(age, 1), round(m, 4)]);
          else coin.highsCapped = true;
        }
        if (m < coin.lo) {
          coin.lo = m;
          if (coin.lows.length < cfg.highsCap) coin.lows.push([round(age, 1), round(m, 4)]);
          else coin.lowsCapped = true;
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
    // The state the entry price was read off, frozen at the same instant. Every
    // multiple on the record is measured against `entry`, and `entry` on its own
    // cannot say how much SOL was in the curve when it was struck — which is the
    // difference between a peak somebody could have sold into and a quote.
    coin.curveAtEntry = coin.reserves;

    // Freeze the opening here. The coin keeps trading for another minute, and if
    // the record counted wallets up to *then* the signal being graded would
    // quietly include information from after the decision it is meant to inform.
    //
    // Each wallet's own position is frozen for the same reason and it is a
    // sharper one: `in` and `out` keep running for another minute, so a record
    // written at the end has no per-wallet figure for the opening at all. A
    // check asking "did one wallet sell into the opening" that reads a minute of
    // selling is not asking that question — it fires on every coin whose price
    // rose and whose early buyers took the profit, which is most of them.
    for (const w of coin.who.values()) {
      w.in0 = round(w.in);
      w.out0 = round(w.out);
    }

    coin.open = {
      seconds: cfg.seconds,
      wallets: coin.buyers.size,
      sellers: coin.sellers.size,
      solIn: round(coin.solIn),
      solOut: round(coin.solOut),
      trades: coin.trades,
    };

    // Ask who paid for the wallets that just opened this coin.
    //
    // Started here and read at `finish`, which is the whole trick: the opening
    // is frozen at three seconds and the record is not written until sixty, so
    // there is a fifty-seven second gap in the middle that already exists and is
    // already doing nothing. A network round trip fits in it with room to spare.
    //
    // Deliberately not awaited. Nothing below waits on it, `finish` does not
    // wait on it, and if it has not come back by then the record goes out saying
    // it is still pending rather than being held up for it.
    //
    // The deployer goes in the list too. Its own funder is the single most
    // useful edge on the graph: an opening buyer that traces back to whoever
    // paid for the deployer is not an early buyer, it is the same person.
    const openers = [...coin.who.values()].filter((w) => w.at <= cfg.seconds).map((w) => w.w);
    if (rpc?.enabled && openers.length) {
      const ask = coin.creator ? [...new Set([coin.creator, ...openers])] : openers;
      // Every branch below writes the same field names, so a reader never has to
      // tell "this launch had no hops" from "this field did not exist yet".
      coin.funding = unanswered(ask.length, { pending: true });
      rpc
        .fundingGraph(ask)
        .then((g) => {
          coin.funding = { ...g, pending: false };
        })
        .catch((err) => {
          // A failed lookup is recorded as failed. Left as `pending` it would
          // read as "still coming" forever; dropped to `available: false` with
          // no explanation it would read as "asked, and nobody shares a funder",
          // which is a claim about the coin rather than about the endpoint.
          coin.funding = unanswered(ask.length, { pending: false, failed: true });
          audit?.emit('error', 'funding_lookup_failed', { mint: coin.mint, message: err?.message ?? String(err) }, { level: 'warn' });
        });
    }

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
      // The cutoff every figure below was frozen at. A reader that has to guess
      // it cannot tell an opening window from a follow window.
      seconds: cfg.seconds,
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
        .map((w) => ({
          w: w.w, in: round(w.in), out: round(w.out),
          // The same two frozen at this instant. Identical here by definition,
          // written anyway so one field name means "in the opening" everywhere.
          in0: w.in0, out0: w.out0,
          tin: round(w.tin, 2), tout: round(w.tout, 2), n: w.n, at: w.at,
        })),
      creator: coin.creator,
      // What the supply check needs and cannot work out for itself: how many
      // tokens exist, what the curve opened at, and what the deployer took.
      supply: coin.supply,
      curve: coin.curve,
      initialBuySol: coin.initialBuySol,
      initialBuyTokens: coin.initialBuyTokens,
    });
    out(
      `${head}  ${pad(coin.buyers.size, 3)} ${plural(coin.buyers.size, 'wallet')} · ` +
        `${coin.solIn.toFixed(2)} SOL in`.padEnd(13) +
        `  ${story}`,
    );
  }

  /**
   * The `follow` mark: write the coin down and forget it.
   *
   * `reason` is 'window' when the timer fired and 'shutdown' when the run ended
   * first — two call sites that were always separate and never said which. The
   * record used to carry `follow: 60` from the configuration on every row,
   * including the ~14% of coins the listener was still watching when it
   * stopped, whose median last candle is at second 3 against 26 for the rest.
   * Nothing on those rows said so, so every expectancy figure downstream mixed
   * cut-off observations in with whole ones and nobody could have known.
   */
  function finish(coin, reason = 'window') {
    live.delete(coin.mint);
    for (const timer of coin.timers ?? []) clearTimeout(timer);
    coin.timers = null;
    const s = coin.social;
    // Whatever the funding lookup managed in the follow window. Read, never
    // awaited — `finish` is called on the way out of the process as well as on a
    // timer, and a shutdown that waits on the network is a shutdown that gets
    // killed instead of finishing.
    const funding = coin.funding
      ? {
          ...coin.funding,
          // Pulled out by name because it is the one edge that gets asked for on
          // its own, and digging it back out of the transfer list every time is
          // how two readers end up disagreeing about which end is which.
          deployerFunder: coin.creator
            ? (coin.funding.transfers.find((t) => t.to === coin.creator)?.from ?? null)
            : null,
        }
      : null;
    const now = Date.now();
    const close = closeFacts({
      t: coin.t,
      now,
      follow: cfg.follow,
      down0: coin.down0 ?? 0,
      // Including an outage still in progress. `onGap` only fires once service
      // resumes, so a coin whose window ended mid-outage would otherwise be
      // written as a clean observation of a market that was never being watched.
      downNow: totals.gapMs + sock.openDownMs(now),
      reason,
    });
    if (!close.complete) totals.truncated++;
    const rec = {
      t: coin.t,
      // The shape this row was written at. It is in the session header too, but
      // a row is copied out of its file constantly — into a database, a jq
      // pipeline, another file — and it arrives at the far end with no header
      // beside it. Six bytes so that a record can always answer "what am I"
      // without the reader inferring the answer from which fields happen to be
      // present, which is precisely the guess `v` exists to remove.
      v: SCHEMA,
      // Which run wrote this, and where in it. Seven calendar days in the
      // corpus were nine sessions; without this that took a night to work out.
      sid,
      seq: coin.seq,
      // The launch transaction, and the block it landed in.
      sig: coin.sig ?? null,
      slot: coin.slot ?? null,
      // Our observed position among the pump transactions in that slot — not
      // the block index, which `logsSubscribe` does not carry and which costs a
      // `getBlock` per slot to recover offline.
      si: coin.si ?? null,
      connectedForSec: coin.connectedForSec ?? null,
      mint: coin.mint,
      symbol: coin.symbol,
      name: coin.name,
      creator: coin.creator,
      uri: coin.uri,
      supply: coin.supply,
      curve: coin.curve,
      initialBuySol: coin.initialBuySol,
      initialBuyTokens: coin.initialBuyTokens,
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
      // Who paid for the wallets in that opening. Three different things this
      // field can say and they must never be flattened into one:
      //   null              — never asked, because no endpoint is configured
      //   pending: true     — asked, and the answer did not arrive in time
      //   failed: true      — asked, and the endpoint did not answer
      //   available: false  — asked, answered, and nothing traced back
      // Only the last of those is a statement about the coin. cluster.js reads a
      // missing edge as proof two wallets are unrelated, so the first three
      // being mistaken for the fourth is how a syndicate goes unnoticed.
      funding,
      // Every wallet that touched this coin, with what it did and when it first
      // appeared. `at` is seconds after launch, so a reader can rebuild who was
      // there at any cutoff rather than trusting ours. `in`/`out` are totals
      // over the whole follow window; `in0`/`out0` are the same wallet's
      // position at the opening cutoff, and mixing the two up is how a check on
      // the first three seconds ends up reading the first sixty.
      who: [...coin.who.values()].map((w) => ({
        ...w, in: round(w.in), out: round(w.out), tin: round(w.tin, 2), tout: round(w.tout, 2),
      })),
      // Whether the 200-wallet cap turned any wallet away. It is binding on the
      // busy coins — the longest `who` in the corpus is exactly 200 on both
      // recorded days — and every sum taken over `who` is a floor when it is
      // true, including the token-conservation check that catches the anomaly
      // above by a completely independent route.
      whoCapped: coin.whoCapped,
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
        // The window we said we would watch for — configuration, kept so a
        // reader knows what was promised...
        follow: cfg.follow,
        // ...and the four facts about what actually happened, which is what was
        // missing. `complete` is the flag to branch on: watched for the whole
        // window AND the feed was up throughout. The analysis rule that follows
        // is "default to complete && gapSec == 0, and state the count you
        // dropped".
        observedSec: close.observedSec,
        complete: close.complete,
        stopReason: close.stopReason,
        gapSec: close.gapSec,
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
        // Every sell in the window, in the order it happened, each naming the
        // wallet that made it. `market.candles[].sells` is the count of these;
        // this is the evidence behind that count, and `capture check` holds the
        // two to each other.
        sells: coin.sells,
        sellsCapped: coin.sellsCapped,
        // The curve state the entry price was struck on: [vsol, vtok, rsol,
        // rtok] in whole SOL and whole tokens. Null when no trade landed before
        // the cutoff, which is also when `entry` is null.
        curveAtEntry: coin.curveAtEntry,
        // How many trades paid each fee rate, and every zero-fee trade in full:
        // [at, wallet, sol, tokens, buy, vsol, vtok, rsol, rtok, fee]. The
        // census is the counter; the ledger is the rows behind it.
        feeBps: { ...coin.feeBps },
        // The same census reduced to the one number and the one flag an analyst
        // needs on sight. W32 found this out a fortnight after the fact by
        // decoding raw bytes in another project; nobody should have to again.
        // `curveSuspect` means at least one trade on this coin paid no fee, and
        // every such trade in the corpus left the curve in a state the launch
        // curve cannot produce. Prices printed after it are priced off that
        // state — real prints, but not prices anyone could have sold into.
        zeroFeeTrades: Number(coin.feeBps[0] ?? 0),
        curveSuspect: Number(coin.feeBps[0] ?? 0) > 0,
        zeroFee: coin.zeroFee,
        zeroFeeCapped: coin.zeroFeeCapped,
        // The pump trading fee actually paid over the window, in SOL. On the
        // wire at every trade, never once written down — which is why the cost
        // model has been carrying a remembered 1% against a chain charging 95
        // basis points.
        feeSol: round(coin.feeSol, 9),
        // When the creator first sold, in seconds, or null if they never did.
        creatorSellAtSec: coin.creatorSellAtSec,
        // Whether either list ran out of room. False on both is the goal; true
        // means the extremes on this row are a floor, not the truth.
        highsCapped: coin.highsCapped,
        lowsCapped: coin.lowsCapped,
      },
      market: {
        candleSeconds: 1,
        candles: coin.candles.map((x) => ({
          s: x.s, o: x.o, h: x.h, l: x.l, c: x.c,
          volume: round(x.volume), buys: x.buys, sells: x.sells,
          // The reserves this second closed on: virtual SOL, virtual tokens,
          // real SOL, real tokens. The price above is derived from the first
          // two; these are what it was derived from.
          vsol: x.vsol ?? null, vtok: x.vtok ?? null, rsol: x.rsol ?? null, rtok: x.rtok ?? null,
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

  // The session header, one per file. Every bound that shapes what lands in the
  // file, in the file — including the failure sample rate and the git commit of
  // the program that wrote it. A capture whose producer cannot be identified
  // from the capture is one whose fields have to be guessed at later, which is
  // exactly what happened to this corpus.
  records?.writeMeta({
    k: 'start',
    v: SCHEMA,
    sid,
    t: startedAt,
    pid: process.pid,
    gitCommit: cfg.gitCommit ?? null,
    endpoint: redact(wsUrl),
    policy: {
      seconds: cfg.seconds,
      follow: cfg.follow,
      heartbeatMs: cfg.heartbeatMs,
      // Every bound a row can hit, in the file the rows are in. A row that says
      // `sellsCapped: true` and a file that never says what the cap was is the
      // same defect as a sample whose rate is not written down: the number is
      // there and the thing needed to read it is not. `highsCap` bounds `lows`
      // as well — one cap for both turning-point lists, by design.
      highsCap: cfg.highsCap,
      sellsCap: cfg.sellsCap,
      zeroFeeCap: cfg.zeroFeeCap,
      whoCap: cfg.whoCap,
      failLog: cfg.failLog && !!fails,
      failSample: cfg.failSample,
      statusMs: cfg.statusMs,
      tweets: !!tweets,
      rpc: !!rpc?.enabled,
    },
  });

  // The heartbeat. Written whether or not anything happened, which is the whole
  // point: uptime becomes `connected ticks ÷ ticks`, a measured number, instead
  // of something reconstructed from a 0.8-second median gap between launches.
  // A hard kill leaves no `stop` row, so this is also what bounds the end of a
  // session — to within one interval.
  const beating = setInterval(() => {
    const t = Date.now();
    totals.beats++;
    const connected = sock.connectedAt !== null;
    if (connected) totals.connectedBeats++;
    records?.writeMeta({
      k: 'tick', v: SCHEMA, sid, t, connected,
      socketAgeSec: sock.connectedForSec(t),
      launches: totals.launches, trades: totals.trades, grads: totals.grads,
      gaps: totals.gaps, gapMs: totals.gapMs, failed: totals.failed,
      liveCoins: live.size, tracked: tracker.size, written: records?.written ?? 0,
      draining,
    });
  }, cfg.heartbeatMs);

  const ticking = setInterval(() => {
    tracker.sweep();
    flushFailAgg();
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
    window.since = Date.now();
  }, cfg.statusMs);

  sock.start();

  return {
    totals,
    tracker,
    sid,
    /** The filename infix every file of this run shares. */
    session,
    startedAt,
    /** The state of the Solana socket, on demand. See link.js. */
    link: (now = Date.now()) => {
      // Over the time that has actually passed since the counters were reset,
      // and only once there is enough of it for the figure to mean anything.
      const mins = (now - window.since) / 60_000;
      const rate = (n) => (mins >= 0.05 ? round(n / mins, 1) : null);
      return {
        ...link.report(sock, now),
        // Throughput belongs next to latency: a link that is up, current, and
        // delivering nothing is a different failure from one that is behind.
        coinsPerMin: rate(window.launches),
        tradesPerMin: rate(window.trades),
        totals: { ...totals },
      };
    },
    /** Abandon a drain in progress and write everything still live as truncated. */
    finishNow() {
      drainCancelled = true;
    },
    /**
     * Stop recording.
     *
     * `drainMs` buys the cheapest possible fix for the truncation problem: stop
     * admitting new launches, keep the socket open until the coins already
     * inside their follow window reach the end of it, and only then close. That
     * turns most of the ~14% of records that used to be cut off into complete
     * ones rather than into well-labelled incomplete ones. Whatever is still
     * live when the budget runs out is written with `complete: false` and
     * `stopReason: 'shutdown'`, because a short record is a fact and a missing
     * one is a hole.
     */
    async stop({ drainMs = 0 } = {}) {
      draining = true;
      if (drainMs > 0 && live.size) {
        const until = Date.now() + drainMs;
        status(`draining: ${live.size} coins still inside their window (up to ${Math.round(drainMs / 1000)}s)`);
        while (live.size && !drainCancelled && Date.now() < until) await sleep(100);
      }
      clearInterval(ticking);
      clearInterval(beating);
      await sock.stop();
      // Stop the funding lookups before anything is written or closed. This does
      // not wait for the ones already in the air — waiting on a network call is
      // how a shutdown runs past its ten-second limit and gets killed, and being
      // killed is the one way to end up mid-write. It stops new ones starting,
      // and it stops the ones still running from writing into a database that is
      // about to close underneath them. Their answers are lost; they cost one
      // lookup each to get back.
      rpc?.stop();
      // Coins still inside their follow window are written as they stand rather
      // than dropped; a short record is a fact, a missing one is a hole. They
      // are labelled as cut off, which is the part that was missing.
      for (const coin of [...live.values()]) finish(coin, 'shutdown');
      flushFailAgg();
      // The session footer. Every counter in it is backed by rows in the file:
      // `launches` by the coin rows, `truncated` by their `complete` flags,
      // `beats` and `connectedBeats` by the ticks, `failed` by the per-minute
      // rollups, `gaps` by the gap rows. A counter that cannot be re-derived
      // from the file is decoration, and this recorder had three of them.
      records?.writeMeta({
        k: 'stop', v: SCHEMA, sid, t: Date.now(), startedAt,
        spanSec: Math.round((Date.now() - startedAt) / 1000),
        beats: totals.beats, connectedBeats: totals.connectedBeats,
        uptime: totals.beats ? round(totals.connectedBeats / totals.beats, 4) : null,
        launches: totals.launches, trades: totals.trades, grads: totals.grads,
        gaps: totals.gaps, gapMs: totals.gapMs,
        failed: totals.failed, failLogged: totals.failLogged, failSample: cfg.failSample,
        truncated: totals.truncated, refusedWhileDraining: totals.refused,
        written: records?.written ?? 0,
      });
      await tweets?.close();
      await fails?.close();
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
      const up = totals.beats ? `${((totals.connectedBeats / totals.beats) * 100).toFixed(1)}%` : 'unmeasured';
      status(
        `\nwatched ${mins.toFixed(1)} min: ${totals.launches} new coins, ` +
          `${totals.trades} trades, ${totals.grads} graduated, ${totals.gaps} gaps` +
          `\nsession ${sid} · uptime ${up} of ${totals.beats} heartbeats · ` +
          `${totals.truncated} records cut short · ` +
          `${totals.failed} failed transactions (${totals.failLogged} kept, 1 in ${cfg.failSample})` +
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

/**
 * A funding block for a launch whose lookup has not come back — because it is
 * still in the air, or because the endpoint failed. Same field names as a real
 * answer, so the difference between the three is in the values and never in
 * which keys are present.
 */
export function unanswered(requested, extra = {}) {
  return {
    available: false,
    hopsWalked: 0,
    perHop: [],
    requested,
    resolved: 0,
    unresolved: requested,
    status: { ok: 0, none: 0, truncated: 0, error: 0, notAsked: requested },
    transfers: [],
    ...extra,
  };
}

/**
 * The four reserve figures a trade event carries, in whole SOL and whole tokens.
 *
 * All four have been decoded since the decoder was written and none of them has
 * ever reached a file. Byte offsets into the trade event payload, counted after
 * the 8-byte event discriminator and confirmed against 30 stored raw events:
 *
 *   virtualSolReserves    89     realSolReserves   105
 *   virtualTokenReserves  97     realTokenReserves 113
 *
 * `realSolReserves` — the one that says whether an inflated quote is backed by
 * SOL the curve could actually pay out — is at **byte 105** and has never been
 * written to a file by this project or by flux.
 *
 * Whole units rather than base units, to match `curve` on the same record, and
 * at full precision rather than a display rounding: SOL carries 9 decimals and
 * pump tokens carry 6, so these divisions are exactly invertible and nothing is
 * discarded. Rounding them for looks is the same mistake as storing the price
 * instead of the reserves, one decimal place further down.
 */
export function reserves(ev) {
  const n = (v, d, dp) => (v == null ? null : round(Number(v) / d, dp));
  return [
    n(ev.virtualSolReserves, 1e9, 9),
    n(ev.virtualTokenReserves, 1e6, 6),
    n(ev.realSolReserves, 1e9, 9),
    n(ev.realTokenReserves, 1e6, 6),
  ];
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
