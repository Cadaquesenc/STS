// Who paid for the wallet.
//
// cluster.js has known how to read a funding graph since it was written, and has
// never been given one. `findSharedFunders` walks back from the opening buyers
// looking for the address that paid for them, and where those paths meet is the
// strongest signal in the file — the whole `funding: 0.8` weight. Every call to
// `analyzeLaunch` in the codebase passed no graph, so that branch was skipped,
// the weight contributed nothing, and `SHARED_FUNDER` had never once fired on
// real data. This file is the missing half.
//
// The idea it is built on: a sniper uses a fresh wallet for every launch, so
// that wallet's *first ever transaction* is somebody sending it the SOL it is
// about to spend. Find that transaction and you have the wallet's parent. Do it
// for every opening buyer and the ones that share a parent are the same person.
//
// Two things this file refuses to do, both of which would be easier:
//
// 1. **It never blocks the watcher.** Nothing here is on the path a trade takes.
//    The watcher freezes a coin's opening at three seconds and writes the record
//    at sixty, so there is a fifty-seven second hole in the middle that already
//    exists and is already doing nothing. The lookup goes there. If it has not
//    finished by the time the record is written, the record says so and goes out
//    without it — a short record is a fact, a late one is a hole.
//
// 2. **It never guesses.** A wallet we could not read is stored as unread, not
//    as unfunded. `findSharedFunders` treats a missing edge as "these two are
//    not related", so a failed lookup silently becomes evidence of innocence.
//    The status column is there to keep those two apart.
const LAMPORTS = 1_000_000_000;

/** Solana's own cap on one `getSignaturesForAddress` page. */
const MAX_PAGE = 1000;

export const RPC_DEFAULTS = {
  // Measured, not guessed. Against Helius on 16 Aug, a batch of twelve wallets
  // took between 3.3 and 14.5 seconds depending on how much history they had,
  // and the 8 seconds this started at was timing real lookups out. A depth-two
  // graph is four of these batches end to end, so the ceiling here has to leave
  // room for four of them inside the watcher's fifty-seven second window.
  timeoutMs: 15_000,
  // Round trips, not requests: the calls are sent as JSON-RPC batches, so one
  // coin's worth of buyers is two requests rather than two per wallet.
  batchSize: 100,
  // Signatures asked for per wallet, and with it the line between a wallet we
  // call fresh and one we give up on. A full page means we never reached the
  // wallet's first transaction, which is recorded as `truncated`.
  //
  // Set to the protocol maximum after measuring both ends against Helius on the
  // 16 Aug corpus, four launches and 48 opening buyers:
  //
  //     250  -> 15 resolved, 33 truncated, 5.4s
  //     1000 -> 19 resolved, 29 truncated, 7.9s
  //
  // A quarter more of the buyers read, for two and a half seconds. That would be
  // a poor trade on the hot path and it is not on the hot path: the whole lookup
  // happens inside a fifty-seven second window that is otherwise doing nothing,
  // so the only currency that buys anything here is coverage.
  pageLimit: MAX_PAGE,
  // How many pages back we are willing to walk looking for a wallet's first
  // transaction. One is the right answer for what this is for: a wallet with
  // more than a thousand transactions is not a wallet somebody opened this
  // morning to snipe with, and paging back through a busy address costs far
  // more than the answer is worth. Such a wallet is recorded as `truncated`,
  // which is honest — we looked as far as we were willing to pay for.
  maxPages: 1,
  depth: 2,
};

/**
 * A JSON-RPC client that answers one question: who funded this wallet.
 *
 * Deliberately plain JSON-RPC rather than Helius's enriched-transaction API.
 * Every method used here exists on any Solana node, so pointing `STS_RPC` at a
 * different provider changes nothing, and the tests can hand it a function
 * instead of a network.
 */
export class Rpc {
  constructor({
    url = process.env.STS_RPC || null,
    fetch: fetchImpl = null,
    timeoutMs = RPC_DEFAULTS.timeoutMs,
    batchSize = RPC_DEFAULTS.batchSize,
    maxPages = RPC_DEFAULTS.maxPages,
    pageLimit = RPC_DEFAULTS.pageLimit,
    store = null,
    audit = null,
  } = {}) {
    this.url = url;
    this.fetch = fetchImpl ?? ((...a) => globalThis.fetch(...a));
    this.timeoutMs = timeoutMs;
    this.batchSize = batchSize;
    this.maxPages = maxPages;
    this.pageLimit = Math.min(Math.max(1, pageLimit), MAX_PAGE);
    // Where answers are kept between coins. A wallet's funder is a fact about
    // the past and cannot change, so a wallet is looked up once and never again
    // — which is most of the reason this is affordable at all.
    this.store = store;
    this.audit = audit;
    this.cache = new Map();
    this.stats = { requests: 0, calls: 0, errors: 0, cached: 0 };
    this.stopped = false;
    if (store?.knownFunding) {
      // Warm from the database so a restart does not re-buy answers already
      // paid for.
      try {
        for (const row of store.knownFunding()) this.cache.set(row.address, row);
      } catch {}
    }
  }

  get enabled() {
    return Boolean(this.url);
  }

  /** Stop starting new work. In-flight requests are left to time out on their own. */
  stop() {
    this.stopped = true;
  }

  // -------------------------------------------------------------------------
  // Transport
  // -------------------------------------------------------------------------

  /**
   * Send one JSON-RPC batch and return the results in the order asked for.
   *
   * Never throws. A dead endpoint, a timeout, a 500 or a body that is not JSON
   * all come back as an array of nulls, because every caller here treats "no
   * answer" the same way and none of them should be able to take the watcher
   * down with them.
   */
  async batch(calls) {
    if (!calls.length) return [];
    if (!this.url || this.stopped) return calls.map(() => null);
    const body = calls.map((c, i) => ({ jsonrpc: '2.0', id: i, method: c.method, params: c.params }));
    this.stats.requests++;
    this.stats.calls += calls.length;
    try {
      const res = await this.fetch(this.url, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify(body),
        signal: AbortSignal.timeout(this.timeoutMs),
      });
      if (!res.ok) throw new Error(`http ${res.status}`);
      const json = await res.json();
      const rows = Array.isArray(json) ? json : [json];
      // A batch response is explicitly allowed to come back in any order, so
      // the ids are what line answers up with questions, not the position.
      const out = new Array(calls.length).fill(null);
      for (const r of rows) {
        const i = Number(r?.id);
        if (Number.isInteger(i) && i >= 0 && i < out.length) out[i] = r?.error ? null : (r?.result ?? null);
      }
      return out;
    } catch (err) {
      this.stats.errors++;
      this.audit?.emit('error', 'rpc_failed', { calls: calls.length, message: err.message }, { level: 'warn' });
      return calls.map(() => null);
    }
  }

  /** `batch`, split so one enormous list does not become one enormous request. */
  async batched(calls) {
    const out = [];
    for (let i = 0; i < calls.length; i += this.batchSize) {
      out.push(...(await this.batch(calls.slice(i, i + this.batchSize))));
    }
    return out;
  }

  // -------------------------------------------------------------------------
  // The lookup
  // -------------------------------------------------------------------------

  /**
   * Resolve the funder of every address given, in as few round trips as the
   * shape of the problem allows: one to ask every wallet for its history, one
   * to fetch every wallet's oldest transaction.
   *
   * @param {string[]} addresses wallets to trace back from.
   * @returns {Promise<Array>} one row per address that was actually looked up.
   */
  async funders(addresses) {
    const wanted = [...new Set((addresses || []).filter(Boolean))];
    const todo = [];
    for (const a of wanted) {
      if (this.cache.has(a)) this.stats.cached++;
      else todo.push(a);
    }
    if (!todo.length || !this.enabled || this.stopped) return wanted.map((a) => this.cache.get(a)).filter(Boolean);

    // --- Round one: the oldest signature each wallet has ---------------------
    const oldest = new Map();
    let page = todo.map((address) => ({ address, before: null }));
    for (let hop = 0; hop < this.maxPages && page.length; hop++) {
      const results = await this.batched(
        page.map((p) => ({
          method: 'getSignaturesForAddress',
          params: [p.address, p.before ? { limit: this.pageLimit, before: p.before } : { limit: this.pageLimit }],
        })),
      );
      const next = [];
      for (let i = 0; i < page.length; i++) {
        const { address } = page[i];
        const sigs = results[i];
        if (!Array.isArray(sigs)) {
          oldest.set(address, { status: 'error' });
          continue;
        }
        if (!sigs.length) {
          // Either the wallet has no history at all, or the previous page ended
          // exactly on a boundary. Either way the last thing we saw is its first.
          if (!oldest.has(address)) oldest.set(address, { status: 'none' });
          continue;
        }
        const last = sigs[sigs.length - 1];
        oldest.set(address, { status: 'ok', sig: last?.signature ?? null, blockTime: last?.blockTime ?? null });
        // A full page means there is older history we have not seen, so what we
        // are holding is not the wallet's first transaction yet.
        if (sigs.length >= this.pageLimit) next.push({ address, before: last?.signature ?? null });
      }
      page = next.filter((p) => p.before);
      if (page.length && hop === this.maxPages - 1) {
        for (const p of page) oldest.set(p.address, { status: 'truncated' });
      }
    }

    // --- Round two: what that transaction did -------------------------------
    const fetchable = todo.filter((a) => oldest.get(a)?.status === 'ok' && oldest.get(a).sig);
    const txs = await this.batched(
      fetchable.map((a) => ({
        method: 'getTransaction',
        params: [oldest.get(a).sig, { maxSupportedTransactionVersion: 0, encoding: 'jsonParsed' }],
      })),
    );

    const now = Date.now();
    const rows = [];
    for (let i = 0; i < fetchable.length; i++) {
      const address = fetchable[i];
      const meta = oldest.get(address);
      const edge = funderFromTransaction(txs[i], address);
      rows.push({
        address,
        funder: edge?.from ?? null,
        sol: edge?.sol ?? null,
        sig: meta.sig,
        blockTime: meta.blockTime ?? null,
        // `ok` means we read the transaction and it funded this wallet. `none`
        // means we read it and it did not — a wallet whose first transaction was
        // something else. The difference matters to anyone reading the table.
        status: edge ? 'ok' : 'none',
        checkedAt: now,
      });
    }
    for (const address of todo) {
      if (fetchable.includes(address)) continue;
      const meta = oldest.get(address) ?? { status: 'error' };
      rows.push({ address, funder: null, sol: null, sig: meta.sig ?? null, blockTime: null, status: meta.status, checkedAt: now });
    }

    for (const row of rows) this.cache.set(row.address, row);
    // Everything looked up is written down, including the failures — the point
    // of storing a failure is to stop it being looked up again on every coin.
    //
    // Unless we are shutting down. A lookup started before Ctrl-C can resolve
    // after the connection it would write through has been closed, and the
    // shutdown order deliberately closes the database last: a write arriving
    // after that is the one way to be mid-write when the process ends. The
    // answer is in the cache either way, and losing it costs one lookup.
    if (this.store?.insertFunding && !this.stopped) {
      try {
        this.store.insertFunding(rows);
      } catch (err) {
        this.audit?.emit('error', 'funding_write_failed', { rows: rows.length, message: err.message }, { level: 'error' });
      }
    }
    return wanted.map((a) => this.cache.get(a)).filter(Boolean);
  }

  /**
   * The graph `analyzeLaunch` wants, for one launch.
   *
   * Walks back `depth` hops. Hop one is the opening buyers; hop two is whoever
   * paid *their* funders, which is where a syndicate that launders through one
   * fresh intermediate per wallet shows up. Hop two is nearly free because
   * funders repeat — that is the entire point of looking for them.
   *
   * What it reports about *itself* is as much the point as the graph. `depth`
   * used to be written on every row, and it was the configured cap echoed back
   * — the literal `2` on all 5,659 records ever produced, while the code
   * advertised a 24-hop tracer. It said nothing about this launch. It is gone.
   * In its place:
   *
   *   `hopsWalked`  how many hops this call actually made. Less than the cap
   *                 whenever the frontier ran dry, which is most launches,
   *                 because most opening buyers resolve to no funder at all.
   *   `perHop`      what each hop asked and what came back, so "we walked two
   *                 hops and found nothing" can be told from "hop one found
   *                 nothing so there was no hop two".
   *   `status`      why each requested wallet has no edge. `unresolved` alone
   *                 flattens "asked, answered, not related" together with "the
   *                 endpoint errored" and "the wallet has more history than we
   *                 were willing to page through" — and cluster.js reads a
   *                 missing edge as proof two wallets are unrelated.
   *
   * @returns {Promise<object>} `{ available, transfers, requested, resolved, ... }`
   */
  async fundingGraph(addresses, { depth = RPC_DEFAULTS.depth } = {}) {
    const wanted = [...new Set((addresses || []).filter(Boolean))];
    const empty = {
      available: false,
      hopsWalked: 0,
      perHop: [],
      requested: wanted.length,
      resolved: 0,
      transfers: [],
      unresolved: wanted.length,
      status: { ok: 0, none: 0, truncated: 0, error: 0, notAsked: wanted.length },
    };
    if (!wanted.length || !this.enabled || this.stopped) return empty;

    const transfers = [];
    const seen = new Set(wanted);
    const perHop = [];
    // Only the first hop's statuses describe the wallets we were asked about;
    // later hops describe funders we went looking for on our own.
    let firstHopRows = null;
    let frontier = wanted;
    let hopsWalked = 0;
    for (let hop = 0; hop < Math.max(1, depth) && frontier.length; hop++) {
      const asked = frontier.length;
      const rows = await this.funders(frontier);
      if (hop === 0) firstHopRows = rows;
      const next = [];
      let found = 0;
      for (const row of rows) {
        if (row.status !== 'ok' || !row.funder) continue;
        found++;
        transfers.push({ from: row.funder, to: row.address, sol: row.sol ?? 0 });
        if (!seen.has(row.funder)) {
          seen.add(row.funder);
          next.push(row.funder);
        }
      }
      hopsWalked = hop + 1;
      perHop.push({ hop: hopsWalked, asked, resolved: found });
      frontier = next;
    }

    const resolved = new Set(transfers.map((t) => t.to).filter((a) => wanted.includes(a))).size;
    return {
      available: transfers.length > 0,
      hopsWalked,
      perHop,
      requested: wanted.length,
      resolved,
      unresolved: wanted.length - resolved,
      status: censusOf(wanted, firstHopRows),
      transfers,
    };
  }
}

/**
 * Why each wallet we were asked about has the answer it has.
 *
 * `ok`        — read, and a funding transfer was found
 * `none`      — read, and nothing funded it that we could see
 * `truncated` — too much history to page through; we stopped looking
 * `error`     — the endpoint did not answer for this wallet
 * `notAsked`  — never reached, because the call was cut short
 *
 * Only `none` is a statement about the wallet. The other three are statements
 * about us, and collapsing them into one `unresolved` count is how a syndicate
 * goes unnoticed: a wallet nobody could read looks exactly like a wallet with
 * nothing to hide.
 */
export function censusOf(wanted, rows) {
  const census = { ok: 0, none: 0, truncated: 0, error: 0, notAsked: 0 };
  const byAddress = new Map((rows ?? []).map((row) => [row.address, row]));
  for (const address of wanted) {
    const row = byAddress.get(address);
    if (!row) census.notAsked++;
    else if (row.status === 'ok' && row.funder) census.ok++;
    else if (row.status === 'ok' || row.status === 'none') census.none++;
    else if (row.status === 'truncated') census.truncated++;
    else census.error++;
  }
  return census;
}

// ---------------------------------------------------------------------------
// Reading one transaction
// ---------------------------------------------------------------------------

/**
 * Every address a transaction touched, in the order the balance arrays are
 * indexed by.
 *
 * Versioned transactions keep some of their addresses in a lookup table outside
 * the message, and `preBalances`/`postBalances` are indexed across the message
 * keys *followed by* those — so a reader that only looks at `accountKeys` lines
 * the wrong balance up against the wrong address on any modern transaction.
 */
export function accountKeys(tx) {
  const raw = tx?.transaction?.message?.accountKeys ?? [];
  const keys = raw.map((k) => (typeof k === 'string' ? k : k?.pubkey)).filter(Boolean);
  const loaded = tx?.meta?.loadedAddresses;
  if (loaded) keys.push(...(loaded.writable ?? []), ...(loaded.readonly ?? []));
  return keys;
}

/**
 * The edge, if this transaction is the one that funded `address`.
 *
 * Read off the balance changes rather than off the instructions, deliberately.
 * SOL reaches a wallet by a plain system transfer, through a program, or as the
 * closing balance of an account somebody shut — and an instruction parser has to
 * know about each of those separately. The balances know about all of them at
 * once: whatever the route, the wallet ends up with more than it started with
 * and somebody else ends up with less.
 *
 * The funder is taken as whoever lost the most, which is right even when the
 * transaction moved money to several places, and is why the fee payer is not
 * simply assumed — on a transaction a program signed, the fee payer is the
 * program's own payer and not the person behind it.
 *
 * @returns {{from: string, to: string, sol: number}|null}
 */
export function funderFromTransaction(tx, address) {
  if (!tx || !address) return null;
  if (tx?.meta?.err) return null;
  const keys = accountKeys(tx);
  const i = keys.indexOf(address);
  if (i < 0) return null;
  const pre = tx?.meta?.preBalances;
  const post = tx?.meta?.postBalances;
  if (!Array.isArray(pre) || !Array.isArray(post)) return null;
  if (pre.length !== post.length || i >= pre.length) return null;

  const gain = (Number(post[i]) - Number(pre[i])) / LAMPORTS;
  // Not a funding transaction. The wallet's first transaction can be it sending
  // rather than receiving — a wallet funded by an inner transfer the balances
  // still show, or one whose history starts with something else entirely.
  if (!(gain > 0)) return null;

  let payer = null;
  for (let j = 0; j < keys.length; j++) {
    if (j === i || keys[j] === address) continue;
    const delta = (Number(post[j]) - Number(pre[j])) / LAMPORTS;
    if (!(delta < 0)) continue;
    if (!payer || delta < payer.delta) payer = { address: keys[j], delta };
  }
  if (!payer) return null;
  return { from: payer.address, to: address, sol: round(gain, 9) };
}

/** Lamport-exact. Anything finer is floating-point noise from the division. */
function round(n, dp = 9) {
  const f = 10 ** dp;
  return Math.round(Number(n) * f) / f;
}
