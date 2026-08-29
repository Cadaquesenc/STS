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
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

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

  -- ------------------------------------------------------------------------
  -- Written from the storage worker thread, and from nowhere else
  -- ------------------------------------------------------------------------
  --
  -- These three are the only tables here that rows reach from another thread
  -- (src/storage/sqlite_worker.js). They are on that thread because the
  -- ingestion loop cannot afford to do the writing: it is reading a socket that
  -- does not wait, and every millisecond spent in SQLite is a millisecond of
  -- trades queueing behind it. Measured on this Mac, twenty thousand telemetry
  -- rows in batches of five hundred hold the event loop for about 33 ms, and a
  -- timer set for every 5 ms does not fire once in that window. Everything else
  -- in this file is written by whoever is already holding the connection, which
  -- is fine for a handful of rows and is not what these three carry.
  --
  -- One position, one row, and \`key\` is the engine's own name for it —
  -- whatever string it can regenerate for the same position after a restart.
  -- That key is what lets a batch stay pure inserts: opening and closing are two
  -- messages on the wire that collapse onto one row here, so the worker never
  -- has to read before it writes. \`mint\`, \`strategy\`, \`side\` and
  -- \`opened_at\` are the identity of the position and are never updated; a
  -- later event that disagrees with them is describing a different position and
  -- should carry a different key.
  --
  -- P&L is a generated column rather than a stored one, for the same reason
  -- \`closePaperTrade\` computes it instead of storing what it was handed: a
  -- writer that can send its own P&L can send the wrong one, and this one writes
  -- unattended with nobody reading the number as it goes past. SQLite refuses an
  -- INSERT into a generated column outright, which makes that unfalsifiable
  -- rather than merely discouraged. The trailing \`+ 0.0\` folds negative zero
  -- back into zero — a short closed exactly at its entry otherwise reads as
  -- "-0 SOL", which is a worse answer than "0 SOL".
  CREATE TABLE IF NOT EXISTS positions (
    key         TEXT PRIMARY KEY,
    run_id      TEXT,
    mint        TEXT NOT NULL,
    strategy    TEXT NOT NULL,
    side        TEXT NOT NULL CHECK(side IN ('BUY', 'SELL')),
    size_sol    REAL,
    entry_price REAL,
    exit_price  REAL,
    status      TEXT NOT NULL CHECK(status IN ('OPEN', 'CLOSED', 'CANCELLED')),
    opened_at   INTEGER NOT NULL,
    closed_at   INTEGER,
    detail      TEXT,
    pnl_sol REAL GENERATED ALWAYS AS (
      CASE WHEN exit_price IS NULL OR entry_price IS NULL OR size_sol IS NULL THEN NULL
           ELSE ROUND(size_sol * (CASE WHEN side = 'SELL' THEN -1 ELSE 1 END)
                      * (exit_price / entry_price - 1), 9) + 0.0 END) VIRTUAL,
    pnl_pct REAL GENERATED ALWAYS AS (
      CASE WHEN exit_price IS NULL OR entry_price IS NULL THEN NULL
           ELSE ROUND((CASE WHEN side = 'SELL' THEN -1 ELSE 1 END)
                      * (exit_price / entry_price - 1) * 100, 4) + 0.0 END) VIRTUAL,
    -- Table constraints go last because SQLite says so: once one appears, no
    -- more columns may follow it. An open position that does not say what it is
    -- worth is not a position. A closed one that arrives without ever having
    -- been seen open is allowed — it is a fact about something that happened
    -- while we were not looking, and dropping it would not make it less true.
    CHECK (status <> 'OPEN' OR (size_sol IS NOT NULL AND entry_price IS NOT NULL))
  );
  -- Open positions are read on every refresh and are a handful of rows in a
  -- table that only grows, so status leads and the time gives newest-first.
  CREATE INDEX IF NOT EXISTS positions_status ON positions (status, opened_at DESC);
  CREATE INDEX IF NOT EXISTS positions_mint   ON positions (mint, opened_at DESC);
  CREATE INDEX IF NOT EXISTS positions_run    ON positions (run_id, opened_at DESC);

  -- One measurement, one row. This is the high-rate table — a busy minute writes
  -- thousands — which is why nothing in it is derived and nothing in it is ever
  -- updated. Append only, and cheap to drop by date when it gets long.
  --
  -- \`value\` is a plain number so a metric can be averaged in SQL without
  -- unpacking JSON first. Anything that does not fit in one number goes in
  -- \`detail\`, TEXT holding JSON for the same reason \`raw\` is TEXT.
  CREATE TABLE IF NOT EXISTS telemetry_logs (
    id     INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT,
    mint   TEXT,
    metric TEXT NOT NULL,
    value  REAL,
    detail TEXT,
    at     INTEGER NOT NULL
  );
  CREATE INDEX IF NOT EXISTS telemetry_logs_at     ON telemetry_logs (at);
  CREATE INDEX IF NOT EXISTS telemetry_logs_metric ON telemetry_logs (metric, at);
  CREATE INDEX IF NOT EXISTS telemetry_logs_mint   ON telemetry_logs (mint, at);

  -- What was true at the moment something was decided. \`state\` is the whole
  -- picture as JSON, because the point of a snapshot is that the question you
  -- will want to ask of it has not been thought of yet.
  --
  -- \`age_sec\` is seconds since the launch, and it is a separate column from
  -- \`at\` because it is the clock these decisions are actually made on. "At
  -- three seconds it read clean" is the sentence a rug post-mortem gets written
  -- in, and a wall clock alone cannot answer it.
  --
  -- \`digest\` is a hash of \`state\`, and the unique index over
  -- (mint, kind, digest) is what stops a retry filing the same evidence twice.
  -- A row with no digest is always kept, because SQLite counts NULLs as distinct
  -- — so a caller that does not want the dedup simply does not ask for it.
  CREATE TABLE IF NOT EXISTS forensic_snapshots (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    mint    TEXT NOT NULL,
    kind    TEXT NOT NULL,
    run_id  TEXT,
    age_sec REAL,
    state   TEXT NOT NULL,
    digest  TEXT,
    at      INTEGER NOT NULL
  );
  CREATE INDEX IF NOT EXISTS forensic_snapshots_mint ON forensic_snapshots (mint, at);
  CREATE INDEX IF NOT EXISTS forensic_snapshots_kind ON forensic_snapshots (kind, at);
  CREATE UNIQUE INDEX IF NOT EXISTS forensic_snapshots_once
    ON forensic_snapshots (mint, kind, digest);
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

      // The three the storage worker binds a batch to. It runs them a few
      // hundred at a time inside one transaction; preparing them here is what
      // keeps that loop to binding and stepping.
      //
      // Null never overwrites a value that is already stored: a close event
      // carries an exit and nothing else, and COALESCE is what lets it say so
      // without repeating the whole position back. `status` is the exception —
      // it is always the latest word, or a position could never leave OPEN.
      insertPosition: this.sql.prepare(
        `INSERT INTO positions
           (key, run_id, mint, strategy, side, size_sol, entry_price, exit_price,
            status, opened_at, closed_at, detail)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(key) DO UPDATE SET
           run_id      = COALESCE(excluded.run_id, positions.run_id),
           size_sol    = COALESCE(excluded.size_sol, positions.size_sol),
           entry_price = COALESCE(excluded.entry_price, positions.entry_price),
           exit_price  = COALESCE(excluded.exit_price, positions.exit_price),
           status      = excluded.status,
           closed_at   = COALESCE(excluded.closed_at, positions.closed_at),
           detail      = COALESCE(excluded.detail, positions.detail)`,
      ),
      insertTelemetry: this.sql.prepare(
        'INSERT INTO telemetry_logs (run_id, mint, metric, value, detail, at) VALUES (?, ?, ?, ?, ?, ?)',
      ),
      // OR IGNORE, so a snapshot that carries a digest it has filed before is
      // dropped at the door rather than raising. Filing the same evidence twice
      // is what a retry does, and a retry is not an error.
      insertSnapshot: this.sql.prepare(
        `INSERT OR IGNORE INTO forensic_snapshots
           (mint, kind, run_id, age_sec, state, digest, at)
         VALUES (?, ?, ?, ?, ?, ?, ?)`,
      ),
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

  // -------------------------------------------------------------------------
  // Batches from the storage worker
  // -------------------------------------------------------------------------

  /**
   * Commit one batch — positions, telemetry and snapshots together, in a single
   * transaction. Together because a commit is an fsync, and three fsyncs per
   * flush costs three times what one does for no more durability.
   *
   * Rows arrive already checked, in the value arrays `positionRow`,
   * `telemetryRow` and `snapshotRow` return. That is deliberate: the checking
   * happens on the thread that made the mistake, so a bad call throws at the
   * call site instead of one thread away, where nobody is looking.
   *
   * A row the database refuses is counted and skipped rather than taking the
   * other four hundred and ninety-nine down with it — but only if it was
   * refused for being the wrong row. A locked database or a full disk is about
   * the batch, not the row, so it comes back out and the caller decides.
   */
  writeBatch({ positions = [], telemetry = [], snapshots = [] } = {}) {
    const out = { positions: 0, telemetry: 0, snapshots: 0, rejected: 0 };
    if (!positions.length && !telemetry.length && !snapshots.length) return out;
    this.transaction(() => {
      for (const [name, rows, stmt] of [
        ['positions', positions, this.stmt.insertPosition],
        ['telemetry', telemetry, this.stmt.insertTelemetry],
        ['snapshots', snapshots, this.stmt.insertSnapshot],
      ]) {
        for (const row of rows) {
          try {
            out[name] += stmt.run(...row).changes;
          } catch (err) {
            if (!isConstraint(err)) throw err;
            out.rejected++;
          }
        }
      }
    });
    return out;
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

// ---------------------------------------------------------------------------
// Rows for the storage worker
// ---------------------------------------------------------------------------
//
// Each of these turns one object into the exact value array its statement
// binds, and refuses anything it cannot turn. They are exported because they
// run on the *calling* thread — the worker receives arrays, not objects, and
// never has to decide whether a caller meant it.
//
// Both spellings of every field are taken, `sizeSol` and `size_sol`, for the
// reason `paperOrder` gives: the alternative is a caller quietly sending the
// spelling this did not read and having it stored as a default.

/** SQLITE_CONSTRAINT is 19, and every extended form of it is 19 in the low
 *  byte with a reason above. So the low byte answers "was it this row?" —
 *  which is a different question from "is the database in trouble". */
const isConstraint = (err) => (Number(err?.errcode) & 0xff) === 19;

/** A number that is a number. Unlike `positive`, zero and below are fine: a
 *  metric reading of 0, or of -3, is a measurement and not a mistake. */
function finite(v, name) {
  const n = Number(v);
  if (!Number.isFinite(n)) throw invalid(`${name} must be a finite number, not ${JSON.stringify(v)}`);
  return n;
}

/** Same, but absent is allowed and stays absent. */
const maybe = (v, name) => (v == null ? null : finite(v, name));
const maybePositive = (v, name) => (v == null ? null : positive(v, name));
const maybeWhole = (v, name) => (v == null ? null : whole(v, name));

/** Anything JSON-shaped to the TEXT it is stored as. A string is taken as
 *  already-encoded JSON and passed through, because re-encoding it would store
 *  the quotes. */
function json(v) {
  if (v == null) return null;
  if (typeof v === 'string') return v;
  return JSON.stringify(v);
}

/**
 * One position event to the row it upserts onto.
 *
 * `key` is the position's identity and the whole reason a batch can be pure
 * inserts. A caller that does not name one gets `strategy:mint:opened_at`,
 * which is stable across a restart as long as the engine still knows when it
 * opened — and an engine that does not know that cannot close it either.
 *
 * P&L is not a field here. It is computed by the table from what is stored, and
 * passing one is an error rather than something to ignore: a caller that
 * believes it is being recorded should find out immediately.
 */
export function positionRow(input = {}) {
  if (input.pnl_sol != null || input.pnlSol != null || input.pnl_pct != null || input.pnlPct != null)
    throw invalid('pnl is computed from entry, exit and size — do not pass it');

  const mint = text(input.mint ?? input.token_address ?? input.tokenAddress);
  if (!mint) throw invalid('mint is required');

  const side = String(input.side ?? 'BUY').toUpperCase();
  if (!SIDES.has(side)) throw invalid(`side must be BUY or SELL, not ${JSON.stringify(input.side)}`);
  const status = String(input.status ?? 'OPEN').toUpperCase();
  if (!STATUSES.has(status)) throw invalid(`status must be one of OPEN, CLOSED, CANCELLED — not ${JSON.stringify(input.status)}`);

  const strategy = text(input.strategy) ?? 'auto';
  const opened_at = whole(input.opened_at ?? input.openedAt ?? Date.now(), 'opened_at');
  const size_sol = maybePositive(input.size_sol ?? input.sizeSol, 'size_sol');
  const entry_price = maybePositive(input.entry_price ?? input.entryPrice, 'entry_price');
  // The same rule the table holds as a CHECK, said here in a sentence someone
  // can act on. The CHECK is for rows that arrive some other way.
  if (status === 'OPEN' && (size_sol == null || entry_price == null))
    throw invalid('an OPEN position needs size_sol and entry_price');

  return [
    text(input.key) ?? `${strategy}:${mint}:${opened_at}`,
    text(input.run_id ?? input.runId),
    mint,
    strategy,
    side,
    size_sol,
    entry_price,
    maybePositive(input.exit_price ?? input.exitPrice, 'exit_price'),
    status,
    opened_at,
    maybeWhole(input.closed_at ?? input.closedAt, 'closed_at'),
    json(input.detail),
  ];
}

/** One measurement to its row. */
export function telemetryRow(input = {}) {
  const metric = text(input.metric ?? input.name);
  if (!metric) throw invalid('metric is required');
  return [
    text(input.run_id ?? input.runId),
    text(input.mint),
    metric,
    maybe(input.value, 'value'),
    json(input.detail),
    whole(input.at ?? Date.now(), 'at'),
  ];
}

/**
 * One snapshot to its row.
 *
 * `digest: true` asks for the dedup — the hash is taken of the state as stored,
 * so two captures of a byte-identical state file once. It is opt-in because
 * "nothing had changed since last time" is itself a finding, and folding those
 * rows together would erase how long a thing stayed the same. A caller that
 * already has an identity for the evidence passes that string instead.
 */
export function snapshotRow(input = {}) {
  const mint = text(input.mint);
  if (!mint) throw invalid('mint is required');
  const kind = text(input.kind);
  if (!kind) throw invalid('kind is required');
  const state = json(input.state);
  if (state == null) throw invalid('state is required');
  return [
    mint,
    kind,
    text(input.run_id ?? input.runId),
    maybe(input.age_sec ?? input.ageSec, 'age_sec'),
    state,
    input.digest === true ? createHash('sha256').update(state).digest('hex') : text(input.digest),
    whole(input.at ?? Date.now(), 'at'),
  ];
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
