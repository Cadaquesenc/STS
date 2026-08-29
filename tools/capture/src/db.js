// The local database, and the line between what was seen and what was worked out.
//
// JSONL stays the archive: every coin is still appended to a file that is never
// edited, because that is the thing we can reprocess from when a heuristic turns
// out to be wrong. This database is the queryable copy of the same facts, plus a
// rollup table that is derived from them and can be thrown away and rebuilt.
//
// `tokens` holds the columns worth querying by, and the entire original record
// alongside them in `raw`. Nothing is dropped on the way in, so a column we did
// not think to add today can be filled from `raw` tomorrow without re-collecting
// anything.
//
// `wallets` is a rollup, not a log. It is computed from `tokens.raw` and rebuilt
// whole. Do not write to it expecting the write to survive.
//
// There is deliberately no `trades` table yet. STS does not record individual
// trades — it adds them up in memory and writes one summary per coin — so a
// trades table would either sit empty or get filled with per-wallet rollups
// pretending to be transactions. It arrives when per-trade capture does.
//
// `positions`, `telemetry_logs` and `forensic_snapshots` are the three tables
// written from another thread, by src/storage/sqlite_worker.js, in batches. They
// do not weaken the paragraph above: none of them holds an observed trade.
// `positions` holds positions a strategy took, which is authored rather than
// observed, the same way `paper_trades` is; `telemetry_logs` holds measurements
// of a run; `forensic_snapshots` holds what was true at the moment a decision
// got made, kept so the decision can be argued with afterwards.
//
// `paper_trades` is the exception to the archive-first rule, and the reason is
// worth stating: a paper order is not observed, it is authored. Nothing on
// chain and nothing in a JSONL file can be reprocessed into it, because it only
// ever existed where it was typed. Keeping it in the browser meant one cleared
// site setting erased the record; here it is primary, and a reload costs
// nothing.
import fs from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { fileURLToPath } from 'node:url';

// Three levels up: this file lives at tools/capture/src/, where it used to sit
// at src/. See the same note in record.js.
const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..', '..');

/** Where the data lives. Honours $STS_HOME, like the JSONL writer does. */
export function dataDir() {
  return process.env.STS_HOME || path.join(ROOT, 'data');
}

// WAL so a reader (the dashboard, a sqlite3 shell) never blocks the writer, and
// synchronous=NORMAL because losing the last few milliseconds of writes to a
// power cut is survivable when the JSONL archive is still on disk beside it.
//
// busy_timeout matters now that two connections in the same process write here:
// the watcher committing a batch of coins and the dashboard recording a paper
// order. WAL allows one writer at a time, so without this the loser of that race
// gets SQLITE_BUSY immediately — five seconds of waiting instead is the whole
// difference between a slow click and a lost trade.
//
// cache_size is negative on purpose. SQLite reads a positive number as a count
// of pages and a negative one as an amount of memory in KiB, so -64000 asks for
// 64 MB and keeps asking for 64 MB if the page size ever changes. It is per
// connection rather than per file — the storage worker opens its own and gets
// its own 64 MB — and the default is -2000, which is 2 MB: small enough that a
// batch of writes pushes the indexes it is about to need straight back out to
// disk. On a machine with 8 GB, 64 MB per connection is a rounding error next
// to the read amplification it removes.
const PRAGMAS = `
  PRAGMA journal_mode = WAL;
  PRAGMA foreign_keys = ON;
  PRAGMA synchronous = NORMAL;
  PRAGMA busy_timeout = 5000;
  PRAGMA cache_size = -64000;
`;

// `payload` and `raw` are declared TEXT rather than JSON: SQLite has no JSON
// type, and a column typed JSON silently gets NUMERIC affinity, which is the
// wrong home for a JSON string. The json_*() functions work on TEXT regardless.
const SCHEMA = `
  CREATE TABLE IF NOT EXISTS tokens (
    mint            TEXT PRIMARY KEY,
    name            TEXT,
    symbol          TEXT,
    uri             TEXT,
    created_at      INTEGER,
    initial_buy_sol REAL,
    market_cap      REAL,
    raw             TEXT NOT NULL
  );
  CREATE INDEX IF NOT EXISTS tokens_created_at ON tokens (created_at);
  CREATE INDEX IF NOT EXISTS tokens_symbol     ON tokens (symbol);

  CREATE TABLE IF NOT EXISTS wallets (
    address      TEXT PRIMARY KEY,
    first_seen   INTEGER,
    total_trades INTEGER NOT NULL DEFAULT 0,
    win_rate     REAL,
    flags        TEXT NOT NULL DEFAULT '[]'
  );
  CREATE INDEX IF NOT EXISTS wallets_total_trades ON wallets (total_trades DESC);

  -- Who paid for a wallet, and how we know it. One row per address ever looked
  -- up, kept forever: a wallet's first transaction is a fact about the past and
  -- cannot change, so this table is what stops the same answer being bought from
  -- the RPC twice.
  --
  -- \`status\` is the load-bearing column, and the reason this is its own table
  -- rather than two more fields on \`wallets\`. It keeps "we looked, and nobody
  -- funded this" apart from "we have not looked". cluster.js reads a missing
  -- edge as proof that two wallets are unrelated, so an unread wallet recorded
  -- as unfunded is not a gap in the data — it is a wrong answer, and it is a
  -- wrong answer in the direction that hides syndicates.
  --   ok        - read the wallet's first transaction, and it was funded in it
  --   none      - read it, and it was not a funding transaction
  --   truncated - too much history to reach the beginning at a price worth paying
  --   error     - the endpoint did not answer
  CREATE TABLE IF NOT EXISTS wallet_funding (
    address    TEXT PRIMARY KEY,
    funder     TEXT,
    sol        REAL,
    sig        TEXT,
    block_time INTEGER,
    status     TEXT NOT NULL,
    checked_at INTEGER NOT NULL
  );
  -- "Who else did this address pay for" is the question the whole table exists
  -- to answer, and it is the one that is not the primary key.
  CREATE INDEX IF NOT EXISTS wallet_funding_funder ON wallet_funding (funder);

  CREATE TABLE IF NOT EXISTS audit_log (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type TEXT NOT NULL,
    payload    TEXT,
    created_at INTEGER NOT NULL
  );
  CREATE INDEX IF NOT EXISTS audit_log_created_at ON audit_log (created_at);
  CREATE INDEX IF NOT EXISTS audit_log_event_type ON audit_log (event_type);

  -- One row is one position, not one button press. \`side\` is the direction the
  -- position is held in — BUY is long, SELL is short — so closing a BUY sells
  -- and closing a SELL buys back. Entry and exit share the row, which is what
  -- makes an open position one thing to update rather than two rows to pair up
  -- afterwards.
  --
  -- Two clocks, and the names say which: \`entry_sec\`/\`exit_sec\` are epoch
  -- seconds, the clock the chart is drawn on, so a fill can be put on a candle
  -- without arithmetic. \`created_at\`/\`closed_at\` are epoch milliseconds, the
  -- wall clock every other table here is stamped with.
  --
  -- The CHECK constraints are the last line rather than the first: the writers
  -- reject a bad side or status with a sentence a person can read. What these
  -- catch is anything that reaches the file without going through them.
  CREATE TABLE IF NOT EXISTS paper_trades (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    token_address TEXT NOT NULL,
    strategy      TEXT NOT NULL,
    side          TEXT NOT NULL CHECK(side IN ('BUY', 'SELL')),
    size_sol      REAL NOT NULL,
    entry_price   REAL NOT NULL,
    exit_price    REAL,
    entry_sec     INTEGER NOT NULL,
    exit_sec      INTEGER,
    pnl_sol       REAL,
    pnl_pct       REAL,
    status        TEXT NOT NULL CHECK(status IN ('OPEN', 'CLOSED', 'CANCELLED')),
    created_at    INTEGER NOT NULL,
    closed_at     INTEGER
  );
  -- Open positions are read on every refresh and are a handful of rows in a
  -- table that only grows, so status leads the index and the id after it gives
  -- the newest-first paging order for free.
  CREATE INDEX IF NOT EXISTS paper_trades_status     ON paper_trades (status, id DESC);
  CREATE INDEX IF NOT EXISTS paper_trades_token      ON paper_trades (token_address, id DESC);
  CREATE INDEX IF NOT EXISTS paper_trades_strategy   ON paper_trades (strategy, id DESC);
  CREATE INDEX IF NOT EXISTS paper_trades_created_at ON paper_trades (created_at);
`;

export class Db {
  constructor({ dir = dataDir(), file = null } = {}) {
    this.dir = dir;
    fs.mkdirSync(dir, { recursive: true });
    this.file = file ?? path.join(dir, 'sts.db');
    this.sql = new DatabaseSync(this.file);
    this.sql.exec(PRAGMAS);
    this.sql.exec(SCHEMA);

    // Prepared once and reused. This is most of the reason the write path can
    // keep up with a spike: the query planner runs at startup, not per row.
    this.stmt = {
      insertToken: this.sql.prepare(
        `INSERT OR IGNORE INTO tokens
           (mint, name, symbol, uri, created_at, initial_buy_sol, market_cap, raw)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)`,
      ),
      insertAudit: this.sql.prepare(
        'INSERT INTO audit_log (event_type, payload, created_at) VALUES (?, ?, ?)',
      ),
      insertWallet: this.sql.prepare(
        `INSERT INTO wallets (address, first_seen, total_trades, win_rate, flags)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(address) DO UPDATE SET
           first_seen   = MIN(wallets.first_seen, excluded.first_seen),
           total_trades = excluded.total_trades,
           win_rate     = excluded.win_rate,
           flags        = excluded.flags`,
      ),
      // An `ok` answer is permanent, so a later failure must never overwrite
      // one. Without the WHERE, one bad afternoon on the endpoint would erase
      // funding edges that were read correctly weeks ago.
      insertFunding: this.sql.prepare(
        `INSERT INTO wallet_funding (address, funder, sol, sig, block_time, status, checked_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(address) DO UPDATE SET
           funder     = excluded.funder,
           sol        = excluded.sol,
           sig        = excluded.sig,
           block_time = excluded.block_time,
           status     = excluded.status,
           checked_at = excluded.checked_at
         WHERE excluded.status = 'ok' OR wallet_funding.status <> 'ok'`,
      ),
      allFunding: this.sql.prepare(
        'SELECT address, funder, sol, sig, block_time, status, checked_at FROM wallet_funding',
      ),
      fundedBy: this.sql.prepare('SELECT address, sol FROM wallet_funding WHERE funder = ?'),
      allMints: this.sql.prepare('SELECT mint FROM tokens'),
      allRaw: this.sql.prepare('SELECT raw FROM tokens'),
      countTokens: this.sql.prepare('SELECT COUNT(*) AS n FROM tokens'),
      insertPaperTrade: this.sql.prepare(
        `INSERT INTO paper_trades
           (token_address, strategy, side, size_sol, entry_price, entry_sec, status, created_at)
         VALUES (?, ?, ?, ?, ?, ?, 'OPEN', ?)`,
      ),
      // `AND status = 'OPEN'` is the lock. Two closes of the same position race
      // otherwise — a double-clicked button is enough — and the second one would
      // overwrite the first exit with a later price.
      finishPaperTrade: this.sql.prepare(
        `UPDATE paper_trades
            SET exit_price = ?, exit_sec = ?, pnl_sol = ?, pnl_pct = ?, status = ?, closed_at = ?
          WHERE id = ? AND status = 'OPEN'`,
      ),
      resizePaperTrade: this.sql.prepare(
        "UPDATE paper_trades SET size_sol = ? WHERE id = ? AND status = 'OPEN'",
      ),
      paperTradeById: this.sql.prepare('SELECT * FROM paper_trades WHERE id = ?'),
    };
  }

  /** Run `fn` inside one transaction. Batching writes this way is worth ~an
   *  order of magnitude on throughput, because each commit is one fsync. */
  transaction(fn) {
    this.sql.exec('BEGIN');
    try {
      const out = fn();
      this.sql.exec('COMMIT');
      return out;
    } catch (err) {
      try {
        this.sql.exec('ROLLBACK');
      } catch {}
      throw err;
    }
  }

  /**
   * Write coin records. Returns how many were new — a mint already present is
   * ignored rather than overwritten, so re-observing a coin after a restart
   * cannot rewrite what we saw the first time.
   */
  insertTokens(records) {
    if (!records.length) return 0;
    return this.transaction(() => {
      let added = 0;
      for (const rec of records) {
        const row = tokenRow(rec);
        if (!row) continue;
        added += this.stmt.insertToken.run(...row).changes;
      }
      return added;
    });
  }

  /**
   * Write what was learned about who funded which wallet.
   *
   * One transaction for the batch, same as the coins: a commit is an fsync, and
   * a launch's worth of buyers resolving one at a time is what would fall behind
   * at a spike.
   */
  insertFunding(rows) {
    const list = (Array.isArray(rows) ? rows : [rows]).filter((r) => r?.address && r?.status);
    if (!list.length) return 0;
    return this.transaction(() => {
      let written = 0;
      for (const r of list) {
        written += this.stmt.insertFunding.run(
          r.address,
          r.funder ?? null,
          r.sol ?? null,
          r.sig ?? null,
          r.blockTime ?? r.block_time ?? null,
          r.status,
          r.checkedAt ?? r.checked_at ?? Date.now(),
        ).changes;
      }
      return written;
    });
  }

  /** Every funding answer already paid for, in the shape rpc.js caches. */
  knownFunding() {
    return this.stmt.allFunding.all().map((r) => ({
      address: r.address,
      funder: r.funder,
      sol: r.sol,
      sig: r.sig,
      blockTime: r.block_time,
      status: r.status,
      checkedAt: r.checked_at,
    }));
  }

  /** Every wallet a given address paid for. The reverse of the lookup. */
  fundedBy(funder) {
    if (!funder) return [];
    return this.stmt.fundedBy.all(funder);
  }

  /** Mirror of one audit event. The NDJSON file remains the primary record. */
  insertAudit(type, event, at) {
    this.stmt.insertAudit.run(type, JSON.stringify(event), at);
  }

  mints() {
    return this.stmt.allMints.all().map((r) => r.mint);
  }

  count() {
    return this.stmt.countTokens.get().n;
  }

  /**
   * Recompute `wallets` from every stored coin record.
   *
   * Cheap enough to do whole rather than incrementally (a few thousand coins is
   * milliseconds), and recomputing beats accumulating because a rollup that
   * drifts is worse than no rollup at all.
   *
   * `total_trades` is the wallet's trade count summed across coins. `win_rate`
   * is the share of the coins it touched where it took out more SOL than it put
   * in — a rough measure, and only over the follow window, not a P&L.
   */
  rebuildWallets() {
    const acc = new Map();
    for (const { raw } of this.stmt.allRaw.all()) {
      let rec;
      try {
        rec = JSON.parse(raw);
      } catch {
        continue;
      }
      if (rec?.creator) flag(acc, rec.creator, 'creator', rec.t);
      for (const w of rec?.who ?? []) {
        if (!w?.w) continue;
        const seen = rec.t != null && w.at != null ? rec.t + Math.round(w.at * 1000) : rec.t ?? null;
        const e = entry(acc, w.w, seen);
        e.trades += w.n ?? 0;
        e.coins += 1;
        if ((w.out ?? 0) > (w.in ?? 0)) e.wins += 1;
      }
    }
    return this.transaction(() => {
      this.sql.exec('DELETE FROM wallets');
      for (const [address, e] of acc) {
        this.stmt.insertWallet.run(
          address,
          e.first_seen,
          e.trades,
          e.coins ? Number((e.wins / e.coins).toFixed(4)) : null,
          JSON.stringify([...e.flags]),
        );
      }
      return acc.size;
    });
  }

  // -------------------------------------------------------------------------
  // Paper trades
  // -------------------------------------------------------------------------

  /**
   * Record a fill and hand back the row it became.
   *
   * Returning the stored row rather than an id is the point: the caller gets
   * the trade exactly as the next reader will see it, including the defaults it
   * did not set, so nothing downstream has to guess what was written.
   */
  recordPaperFill(order) {
    const o = paperOrder(order);
    const { lastInsertRowid } = this.stmt.insertPaperTrade.run(
      o.token_address, o.strategy, o.side, o.size_sol, o.entry_price, o.entry_sec, o.created_at,
    );
    return this.paperTrade(Number(lastInsertRowid));
  }

  /**
   * Close an open position at `exitPrice`, or cancel it.
   *
   * A close is where the money is decided, so it is the one write here that
   * computes rather than stores: P&L comes from the row's own entry and size,
   * never from a number the caller passed alongside the exit. A caller that
   * could send its own P&L could send the wrong one.
   *
   * Returns null when there is no such trade — that is a 404, not a failure.
   * Throws when the trade is not open, because closing a closed position is a
   * mistake worth hearing about rather than a no-op to swallow.
   */
  closePaperTrade(id, { exitPrice = null, exitSec = null, closedAt = null, status = 'CLOSED' } = {}) {
    const row = this.paperTrade(id);
    if (!row) return null;
    if (row.status !== 'OPEN') throw notOpen(row);

    const finish = String(status).toUpperCase();
    if (finish !== 'CLOSED' && finish !== 'CANCELLED') throw invalid(`status must be CLOSED or CANCELLED, not ${JSON.stringify(status)}`);

    const closed_at = whole(closedAt ?? Date.now(), 'closed_at');
    // A cancelled position never had an exit, so it gets neither an exit price,
    // an exit second, nor a P&L. Only `closed_at` — when it stopped being open.
    const exit_price = finish === 'CANCELLED' ? null : positive(exitPrice, 'exit_price');
    const exit_sec = finish === 'CANCELLED' ? null : whole(exitSec ?? Math.floor(closed_at / 1000), 'exit_sec');
    const { pnl_sol, pnl_pct } = exit_price == null
      ? { pnl_sol: null, pnl_pct: null }
      : paperPnl({ ...row, exit_price });

    const { changes } = this.stmt.finishPaperTrade.run(
      exit_price, exit_sec, pnl_sol, pnl_pct, finish, closed_at, row.id,
    );
    // Read as OPEN a moment ago, refused by the guard now: another writer got
    // there in between. Same answer as if it had been closed all along.
    if (!changes) throw notOpen(this.paperTrade(id) ?? row);
    return this.paperTrade(id);
  }

  /**
   * Sell part of a position and keep the rest.
   *
   * The part sold keeps the id, so a link to the trade still points at the exit
   * that happened. What is left becomes a new open row carrying the original
   * entry price and the original opening time — it is the same position, still
   * measured from where it was bought, not re-bought at today's price.
   *
   * Returns `{ trade, remainder }`, with a null remainder when the whole
   * position went. Both rows are written in one transaction: a crash between
   * them would otherwise leave the sold part closed and the rest nowhere.
   */
  reducePaperTrade(id, { sizeSol, exitPrice = null, exitSec = null, closedAt = null } = {}) {
    const row = this.paperTrade(id);
    if (!row) return null;
    if (row.status !== 'OPEN') throw notOpen(row);

    const portion = positive(sizeSol, 'size_sol');
    if (portion > row.size_sol + LAMPORT) throw invalid(`cannot sell ${portion} SOL of a ${row.size_sol} SOL position`);
    // Within a lamport of the whole thing is the whole thing. Leaving a dust
    // position open is how a list fills with rows nobody can close.
    if (portion >= row.size_sol - LAMPORT) {
      return { trade: this.closePaperTrade(id, { exitPrice, exitSec, closedAt }), remainder: null };
    }

    return this.transaction(() => {
      const rest = round(row.size_sol - portion, 9);
      this.stmt.resizePaperTrade.run(portion, row.id);
      return {
        trade: this.closePaperTrade(id, { exitPrice, exitSec, closedAt }),
        remainder: this.recordPaperFill({
          token_address: row.token_address,
          strategy: row.strategy,
          side: row.side,
          size_sol: rest,
          entry_price: row.entry_price,
          entry_sec: row.entry_sec,
          created_at: row.created_at,
        }),
      };
    });
  }

  /** One trade, or null. */
  paperTrade(id) {
    const n = Number(id);
    if (!Number.isInteger(n) || n <= 0) throw invalid('id must be a positive integer');
    return this.stmt.paperTradeById.get(n) ?? null;
  }

  /**
   * A page of trades, newest first.
   *
   * Paged by id rather than by offset: rows arrive while someone is reading, and
   * an offset would show the same trade on two pages or skip one entirely. The
   * cursor is the id of the last row of the previous page — `nextCursor` is null
   * when there is nothing after it.
   */
  paperTrades({ status = null, token = null, strategy = null, limit = 100, cursor = null } = {}) {
    const where = [];
    const args = [];

    const wanted = (Array.isArray(status) ? status : [status]).filter((s) => s != null).map((s) => String(s).toUpperCase());
    for (const s of wanted) if (!STATUSES.has(s)) throw invalid(`status must be one of OPEN, CLOSED, CANCELLED — not ${JSON.stringify(s)}`);
    if (wanted.length) {
      where.push(`status IN (${wanted.map(() => '?').join(', ')})`);
      args.push(...wanted);
    }
    if (token != null) { where.push('token_address = ?'); args.push(String(token)); }
    if (strategy != null) { where.push('strategy = ?'); args.push(String(strategy)); }
    if (cursor != null) {
      const c = whole(cursor, 'cursor');
      if (c <= 0) throw invalid('cursor must be a positive id');
      where.push('id < ?');
      args.push(c);
    }

    const take = pageSize(limit);
    // One more than asked for, so "is there another page" is answered by the
    // read itself rather than by a second COUNT over the same rows.
    const rows = this.sql
      .prepare(`SELECT * FROM paper_trades ${where.length ? `WHERE ${where.join(' AND ')} ` : ''}ORDER BY id DESC LIMIT ?`)
      .all(...args, take + 1);
    const page = rows.slice(0, take);
    return { rows: page, nextCursor: rows.length > take ? page.at(-1).id : null };
  }

  /** Positions still open, newest first. Plain array: there are never many. */
  openPaperTrades({ token = null, strategy = null, limit = 200 } = {}) {
    return this.paperTrades({ status: 'OPEN', token, strategy, limit }).rows;
  }

  /**
   * The three numbers the terminal puts at the top — what is at risk, what has
   * been made, and how often it was right — counted in SQL rather than over a
   * page of rows, so they describe the whole record and not the first hundred.
   *
   * Cancelled positions are counted but never scored: they have no exit, so
   * they belong in neither the wins nor the losses.
   *
   * Nor does a trade that came out exactly level. It is not a win and it is not a
   * loss — it is a trade that did nothing, and calling it a loss was both wrong
   * and out of step with `summarise` in backtest.js, which has always counted a
   * win as `> 0` and a loss as `< 0` over the same kind of series. A test pins the
   * two together now.
   *
   * So `wins + losses` need not equal `closed`, and the gap is the level ones. The
   * win rate is deliberately over every closed trade rather than over the scored
   * ones: a flat trade is a trade you were right about nothing on, and it should
   * dilute the rate rather than vanish from it. Dropping it from the denominator
   * instead would let a run of break-evens quietly flatter the number.
   */
  paperSummary({ token = null, strategy = null } = {}) {
    const where = [];
    const args = [];
    if (token != null) { where.push('token_address = ?'); args.push(String(token)); }
    if (strategy != null) { where.push('strategy = ?'); args.push(String(strategy)); }
    const rows = this.sql
      .prepare(
        `SELECT status,
                COUNT(*)                                  AS n,
                COALESCE(SUM(size_sol), 0)                AS size_sol,
                COALESCE(SUM(pnl_sol), 0)                 AS pnl_sol,
                SUM(CASE WHEN pnl_sol > 0 THEN 1 ELSE 0 END) AS wins,
                SUM(CASE WHEN pnl_sol < 0 THEN 1 ELSE 0 END) AS losses
           FROM paper_trades ${where.length ? `WHERE ${where.join(' AND ')} ` : ''}GROUP BY status`,
      )
      .all(...args);

    const by = new Map(rows.map((r) => [r.status, r]));
    const open = by.get('OPEN');
    const closed = by.get('CLOSED');
    const cancelled = by.get('CANCELLED');
    const wins = closed?.wins ?? 0;
    const losses = closed?.losses ?? 0;
    return {
      open: open?.n ?? 0,
      closed: closed?.n ?? 0,
      cancelled: cancelled?.n ?? 0,
      openCostSol: round(open?.size_sol ?? 0, 9),
      realisedSol: round(closed?.pnl_sol ?? 0, 9),
      wins,
      losses,
      winRate: closed?.n ? round(wins / closed.n, 4) : null,
    };
  }

  close() {
    try {
      this.sql.close();
    } catch {}
  }
}

// ---------------------------------------------------------------------------
// Paper trade input, and the one calculation this file is allowed to make
// ---------------------------------------------------------------------------

const SIDES = new Set(['BUY', 'SELL']);
const STATUSES = new Set(['OPEN', 'CLOSED', 'CANCELLED']);

/** The smallest amount of SOL there is, and so the width of "the same size". */
const LAMPORT = 1e-9;

/** Bad input from a person, not a broken database. Callers turn this into a 400. */
function invalid(message) {
  const err = new Error(message);
  err.code = 'INVALID';
  return err;
}

function notOpen(row) {
  const err = new Error(`trade ${row.id} is ${row.status.toLowerCase()}, not open`);
  err.code = 'NOT_OPEN';
  err.trade = row;
  return err;
}

const text = (v) => (typeof v === 'string' && v.trim() ? v.trim() : null);

function positive(v, name) {
  const n = Number(v);
  if (!Number.isFinite(n) || n <= 0) throw invalid(`${name} must be a positive number, not ${JSON.stringify(v)}`);
  return n;
}

function whole(v, name) {
  const n = Number(v);
  if (!Number.isFinite(n)) throw invalid(`${name} must be a number, not ${JSON.stringify(v)}`);
  return Math.floor(n);
}

const pageSize = (limit) => Math.min(500, Math.max(1, Math.floor(Number(limit) || 100)));

// Rounding, with negative zero folded back into zero. A short closed at exactly
// its entry makes -0, which stores as -0.0 and reads as a loss that did not
// happen: "-0 SOL" on a screen is a worse answer than "0 SOL".
const round = (n, dp) => {
  const f = 10 ** dp;
  const out = Math.round(Number(n) * f) / f;
  return out === 0 ? 0 : out;
};

/**
 * One order to the columns it is stored in, or an error saying what is wrong
 * with it.
 *
 * Both spellings of every field are accepted — `sizeSol` from JavaScript,
 * `size_sol` from the row it will become — because the alternative is the
 * caller silently sending the one this did not take and having it stored as a
 * default.
 *
 * `strategy` defaults to 'manual', which is the honest name for an order typed
 * into the terminal by hand. A bot names itself.
 */
export function paperOrder(input = {}) {
  const token = text(input.token_address ?? input.tokenAddress ?? input.mint);
  if (!token) throw invalid('token_address is required');
  const side = String(input.side ?? 'BUY').toUpperCase();
  if (!SIDES.has(side)) throw invalid(`side must be BUY or SELL, not ${JSON.stringify(input.side)}`);
  const created_at = whole(input.created_at ?? input.createdAt ?? Date.now(), 'created_at');
  return {
    token_address: token,
    strategy: text(input.strategy) ?? 'manual',
    side,
    size_sol: positive(input.size_sol ?? input.sizeSol, 'size_sol'),
    entry_price: positive(input.entry_price ?? input.entryPrice, 'entry_price'),
    entry_sec: whole(input.entry_sec ?? input.entrySec ?? Math.floor(created_at / 1000), 'entry_sec'),
    created_at,
  };
}

/**
 * What a position made, from its own entry, size and direction.
 *
 * A BUY earns the move up; a SELL earns the move down, by exactly as much. P&L
 * is in SOL to the lamport — nine decimals, the smallest amount that exists —
 * and the percentage is a percentage, so 50 means half again, not fifty times.
 *
 * Fees are not in here. These are paper trades, and pretending to model the
 * spread would make the number look more real than it is; `cost.js` holds the
 * round-trip cost for anyone who wants to take it off.
 */
export function paperPnl({ side, size_sol, entry_price, exit_price }) {
  const move = exit_price / entry_price - 1;
  const direction = side === 'SELL' ? -1 : 1;
  return {
    pnl_sol: round(size_sol * direction * move, 9),
    pnl_pct: round(direction * move * 100, 4),
  };
}

function entry(acc, address, seen) {
  let e = acc.get(address);
  if (!e) {
    e = { first_seen: seen, trades: 0, coins: 0, wins: 0, flags: new Set() };
    acc.set(address, e);
  } else if (seen != null && (e.first_seen == null || seen < e.first_seen)) {
    e.first_seen = seen;
  }
  return e;
}

function flag(acc, address, name, seen) {
  entry(acc, address, seen).flags.add(name);
}

/**
 * One coin record to one `tokens` row.
 *
 * The derived columns are only as honest as their inputs, so each is null unless
 * the record actually carries what it needs:
 *   uri, initial_buy_sol   captured at launch — absent from anything written
 *                          before this column existed
 *   market_cap             entry price × supply, both of which have to be there
 */
export function tokenRow(rec) {
  if (!rec?.mint) return null;
  const entryPrice = rec.outcome?.entry ?? null;
  const supply = rec.supply ?? null;
  const marketCap = entryPrice != null && supply != null ? Number((entryPrice * supply).toFixed(6)) : null;
  return [
    rec.mint,
    rec.name ?? null,
    rec.symbol ?? null,
    rec.uri ?? null,
    rec.t ?? null,
    rec.initialBuySol ?? null,
    marketCap,
    JSON.stringify(rec),
  ];
}
