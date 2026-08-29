// The paper record, and why it is held to a higher standard than the rest.
//
// Everything else in this database is a copy. Lose the `tokens` table and it can
// be rebuilt line by line from the JSONL archive; lose the `wallets` rollup and
// it is one function call away. A paper trade has nothing behind it. It happened
// when someone pressed a button, it was never on chain, and no file anywhere can
// be reprocessed into it. That is what it used to share with localStorage, which
// is exactly how it kept getting lost.
//
// So the tests here are about three claims, in order of how much they matter:
//
// It is still there. A trade written by one server is read by the next one,
// because that — not the schema, not the shape of the JSON — is the whole point
// of moving it off the browser.
//
// The number is right. P&L is computed from the position's own entry, size and
// direction, and never from a figure the caller sent alongside the exit. A
// caller that can send its own P&L can send the wrong one, and a paper record
// that flatters itself is worse than no record at all.
//
// It refuses clearly. A bad order is a sentence, not a constraint violation; a
// position closed twice is a conflict rather than a silent second exit. These
// are the paths a person actually meets, so they are tested as answers, not as
// exceptions.
//
// Every test gets its own directory and its own port. Nothing here reaches the
// network, and nothing here needs the real corpus.
//
// Run with: node --test test/

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { once } from 'node:events';

import { Db, paperOrder, paperPnl } from '../src/db.js';
import { summarise } from '../src/backtest.js';
import { serve } from '../src/dash.js';

/** A fresh directory per test, removed afterwards. */
function tmp(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'sts-paper-test-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

/** A database in one, closed afterwards. */
function db(t, dir = tmp(t)) {
  const open = new Db({ dir });
  t.after(() => open.close());
  return open;
}

const MINT = 'Mint111111111111111111111111111111111111111';
const OTHER = 'Mint222222222222222222222222222222222222222';

/** A filled order, with only what has to be there. */
const order = (over = {}) => ({ tokenAddress: MINT, sizeSol: 0.25, entryPrice: 0.001, ...over });

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

test('paper_trades is created with the columns the rest of the code writes', (t) => {
  const columns = new Map(
    db(t).sql.prepare('PRAGMA table_info(paper_trades)').all().map((c) => [c.name, c]),
  );

  const expected = {
    id: ['INTEGER', 0], // nullable in the pragma's eyes: it is the rowid alias
    token_address: ['TEXT', 1],
    strategy: ['TEXT', 1],
    side: ['TEXT', 1],
    size_sol: ['REAL', 1],
    entry_price: ['REAL', 1],
    exit_price: ['REAL', 0],
    entry_sec: ['INTEGER', 1],
    exit_sec: ['INTEGER', 0],
    pnl_sol: ['REAL', 0],
    pnl_pct: ['REAL', 0],
    status: ['TEXT', 1],
    created_at: ['INTEGER', 1],
    closed_at: ['INTEGER', 0],
  };
  assert.deepEqual([...columns.keys()], Object.keys(expected), 'columns, in order');
  for (const [name, [type, notnull]] of Object.entries(expected)) {
    assert.equal(columns.get(name).type, type, `${name} type`);
    assert.equal(columns.get(name).notnull, notnull, `${name} nullability`);
  }
  assert.equal(columns.get('id').pk, 1, 'id is the primary key');
});

test('a second writer waits its turn instead of being turned away', (t) => {
  // Two connections write to this file now — the watcher committing a batch of
  // coins and the dashboard recording an order. WAL takes one writer at a time,
  // so without a busy timeout the second one is refused outright, and the trade
  // that is refused is the one someone just placed.
  assert.equal(db(t).sql.prepare('PRAGMA busy_timeout').get().timeout, 5000);
});

test('the indexes an open-positions read depends on are there', (t) => {
  const indexes = db(t).sql
    .prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='paper_trades'")
    .all()
    .map((r) => r.name);
  for (const name of ['paper_trades_status', 'paper_trades_token', 'paper_trades_strategy', 'paper_trades_created_at'])
    assert.ok(indexes.includes(name), `missing index ${name}`);
});

test('the id really does auto-increment, so a reused id cannot exist', (t) => {
  const open = db(t);
  const first = open.recordPaperFill(order());
  const second = open.recordPaperFill(order());
  assert.equal(second.id, first.id + 1);
  // Even after the first is gone: an id that came back would point an old link
  // at a different trade.
  open.sql.prepare('DELETE FROM paper_trades WHERE id = ?').run(second.id);
  assert.equal(open.recordPaperFill(order()).id, second.id + 1);
});

test('the CHECK constraints refuse a side or a status the code would never write', (t) => {
  const open = db(t);
  const insert = (side, status) =>
    open.sql
      .prepare(
        `INSERT INTO paper_trades (token_address, strategy, side, size_sol, entry_price, entry_sec, status, created_at)
         VALUES (?, 'manual', ?, 1, 1, 1, ?, 1)`,
      )
      .run(MINT, side, status);

  assert.throws(() => insert('HOLD', 'OPEN'), /CHECK/i, 'side is BUY or SELL');
  assert.throws(() => insert('BUY', 'PENDING'), /CHECK/i, 'status is OPEN, CLOSED or CANCELLED');
  assert.throws(() => insert('buy', 'OPEN'), /CHECK/i, 'and they are upper case');
  // What the code does write is accepted.
  for (const side of ['BUY', 'SELL']) for (const status of ['OPEN', 'CLOSED', 'CANCELLED']) insert(side, status);
  assert.equal(open.sql.prepare('SELECT COUNT(*) AS n FROM paper_trades').get().n, 6);
});

test('a trade cannot be stored without the parts that make it a trade', (t) => {
  const open = db(t);
  assert.throws(
    () => open.sql.prepare('INSERT INTO paper_trades (strategy, side, size_sol, entry_price, entry_sec, status, created_at) VALUES (?,?,?,?,?,?,?)')
      .run('manual', 'BUY', 1, 1, 1, 'OPEN', 1),
    /NOT NULL/i,
    'no token address',
  );
  assert.throws(
    () => open.sql.prepare('INSERT INTO paper_trades (token_address, strategy, side, size_sol, entry_sec, status, created_at) VALUES (?,?,?,?,?,?,?)')
      .run(MINT, 'manual', 'BUY', 1, 1, 'OPEN', 1),
    /NOT NULL/i,
    'no entry price',
  );
});

// ---------------------------------------------------------------------------
// What an order is allowed to be
// ---------------------------------------------------------------------------

test('an order can be spelled either way, and fills in what it did not say', () => {
  const camel = paperOrder({ tokenAddress: MINT, sizeSol: 2, entryPrice: 0.5, createdAt: 1786554449571 });
  const snake = paperOrder({ token_address: MINT, size_sol: 2, entry_price: 0.5, created_at: 1786554449571 });
  assert.deepEqual(camel, snake);

  assert.equal(camel.side, 'BUY', 'an order with no side is a long');
  assert.equal(camel.strategy, 'manual', 'and one with no strategy was placed by hand');
  assert.equal(camel.created_at, 1786554449571, 'the wall clock is milliseconds');
  assert.equal(camel.entry_sec, 1786554449, 'the chart clock is the same instant in seconds');
});

test('a mint is accepted by that name too, since that is what the terminal calls it', () => {
  assert.equal(paperOrder({ mint: MINT, sizeSol: 1, entryPrice: 1 }).token_address, MINT);
});

test('an order that cannot be filled is refused in words', () => {
  const refused = (input, pattern) => {
    assert.throws(() => paperOrder(input), (e) => {
      assert.equal(e.code, 'INVALID', 'refusals are the caller to fix');
      assert.match(e.message, pattern);
      return true;
    });
  };

  refused({ sizeSol: 1, entryPrice: 1 }, /token_address is required/);
  refused({ tokenAddress: '   ', sizeSol: 1, entryPrice: 1 }, /token_address is required/);
  refused(order({ side: 'HODL' }), /side must be BUY or SELL/);
  refused(order({ sizeSol: 0 }), /size_sol must be a positive number/);
  refused(order({ sizeSol: -1 }), /size_sol must be a positive number/);
  refused(order({ sizeSol: 'lots' }), /size_sol must be a positive number/);
  refused({ tokenAddress: MINT, entryPrice: 1 }, /size_sol must be a positive number/);
  refused(order({ entryPrice: 0 }), /entry_price must be a positive number/);
  refused(order({ entryPrice: Infinity }), /entry_price must be a positive number/);
});

test('a lower-case side is a spelling, not a different side', () => {
  assert.equal(paperOrder(order({ side: 'sell' })).side, 'SELL');
});

// ---------------------------------------------------------------------------
// The number
// ---------------------------------------------------------------------------

test('a long earns the move up and a short earns the same move down', () => {
  const long = paperPnl({ side: 'BUY', size_sol: 1, entry_price: 100, exit_price: 150 });
  assert.deepEqual(long, { pnl_sol: 0.5, pnl_pct: 50 });

  const short = paperPnl({ side: 'SELL', size_sol: 1, entry_price: 150, exit_price: 100 });
  assert.equal(short.pnl_pct, 33.3333, 'a third off, to four places');
  assert.equal(short.pnl_sol, 0.333333333, 'and a third of a SOL, to the lamport');

  // And the mirror: the same move against each of them costs the same.
  assert.deepEqual(paperPnl({ side: 'SELL', size_sol: 1, entry_price: 100, exit_price: 150 }), { pnl_sol: -0.5, pnl_pct: -50 });
});

test('the percentage is a percentage and the SOL is scaled by the size', () => {
  assert.equal(paperPnl({ side: 'BUY', size_sol: 4, entry_price: 1, exit_price: 3 }).pnl_pct, 200, '3x is +200%, not 3');
  assert.equal(paperPnl({ side: 'BUY', size_sol: 4, entry_price: 1, exit_price: 3 }).pnl_sol, 8, 'and 4 SOL in made 8');
  assert.equal(paperPnl({ side: 'BUY', size_sol: 0.25, entry_price: 1, exit_price: 3 }).pnl_sol, 0.5, 'a quarter of the size, a quarter of the P&L');
});

test('an exit at the entry is exactly nothing, on both sides', () => {
  for (const side of ['BUY', 'SELL']) {
    const flat = paperPnl({ side, size_sol: 3, entry_price: 0.000123, exit_price: 0.000123 });
    assert.equal(flat.pnl_sol, 0);
    assert.equal(flat.pnl_pct, 0);
    assert.ok(!Object.is(flat.pnl_sol, -0), 'and not negative zero, which prints as -0');
  }
});

test('P&L is kept to the lamport, which is as fine as SOL goes', () => {
  const tiny = paperPnl({ side: 'BUY', size_sol: 0.000000001, entry_price: 1, exit_price: 2 });
  assert.equal(tiny.pnl_sol, 0.000000001, 'one lamport survives');
  const finer = paperPnl({ side: 'BUY', size_sol: 0.0000000001, entry_price: 1, exit_price: 2 });
  assert.equal(finer.pnl_sol, 0, 'a tenth of one does not, and is not pretended to');
});

// ---------------------------------------------------------------------------
// Writing and closing
// ---------------------------------------------------------------------------

test('a fill comes back as the row it became', (t) => {
  const open = db(t);
  const trade = open.recordPaperFill(order({ strategy: 'syndicate-sniper', side: 'BUY' }));

  assert.equal(trade.token_address, MINT);
  assert.equal(trade.strategy, 'syndicate-sniper');
  assert.equal(trade.size_sol, 0.25);
  assert.equal(trade.entry_price, 0.001);
  assert.equal(trade.status, 'OPEN');
  assert.equal(trade.exit_price, null);
  assert.equal(trade.exit_sec, null);
  assert.equal(trade.pnl_sol, null);
  assert.equal(trade.closed_at, null);
  assert.ok(trade.created_at > 1_700_000_000_000, 'stamped in milliseconds');
  assert.equal(trade.entry_sec, Math.floor(trade.created_at / 1000));

  // The same thing the next reader will see, not a hopeful copy of it.
  assert.deepEqual({ ...open.paperTrade(trade.id) }, { ...trade });
});

test('closing computes the P&L from the position, not from what it was told', (t) => {
  const open = db(t);
  const trade = open.recordPaperFill(order({ sizeSol: 2, entryPrice: 0.001 }));
  // A caller sending its own numbers alongside the exit: they are not columns
  // this write reads, and they must not become the record.
  const closed = open.closePaperTrade(trade.id, { exitPrice: 0.0015, pnl_sol: 999, pnlPct: 999 });

  assert.equal(closed.status, 'CLOSED');
  assert.equal(closed.exit_price, 0.0015);
  assert.equal(closed.pnl_sol, 1, '2 SOL at +50%');
  assert.equal(closed.pnl_pct, 50);
  assert.ok(closed.closed_at >= trade.created_at);
  assert.equal(closed.exit_sec, Math.floor(closed.closed_at / 1000));
  assert.deepEqual({ ...open.paperTrade(trade.id) }, { ...closed }, 'and it is what was stored');
});

test('a losing close is stored as a loss rather than dropped', (t) => {
  const open = db(t);
  const trade = open.recordPaperFill(order({ sizeSol: 1, entryPrice: 0.001 }));
  const closed = open.closePaperTrade(trade.id, { exitPrice: 0.0002 });
  assert.equal(closed.pnl_sol, -0.8);
  assert.equal(closed.pnl_pct, -80);
});

test('a stop-out is an ordinary close at the price it stopped at', (t) => {
  const open = db(t);
  const trade = open.recordPaperFill(order({ sizeSol: 0.5, entryPrice: 0.001, strategy: 'syndicate-sniper' }));
  const stopped = open.closePaperTrade(trade.id, { exitPrice: 0.0008, exitSec: 1786554460, closedAt: 1786554460500 });

  assert.equal(stopped.exit_sec, 1786554460, 'the second it stopped, for the chart');
  assert.equal(stopped.closed_at, 1786554460500);
  assert.equal(stopped.pnl_pct, -20);
  assert.equal(stopped.status, 'CLOSED', 'a stop is a close; there is no third state for it');
});

test('a cancelled position keeps no exit and no P&L, because it had neither', (t) => {
  const open = db(t);
  const trade = open.recordPaperFill(order());
  const cancelled = open.closePaperTrade(trade.id, { status: 'CANCELLED' });

  assert.equal(cancelled.status, 'CANCELLED');
  assert.equal(cancelled.exit_price, null);
  assert.equal(cancelled.exit_sec, null);
  assert.equal(cancelled.pnl_sol, null);
  assert.equal(cancelled.pnl_pct, null);
  assert.ok(cancelled.closed_at > 0, 'but it does record when it stopped being open');
});

test('closing something that was never open says so, each in its own way', (t) => {
  const open = db(t);
  assert.equal(open.closePaperTrade(4242, { exitPrice: 1 }), null, 'no such trade is not an error');

  const trade = open.recordPaperFill(order());
  open.closePaperTrade(trade.id, { exitPrice: 0.002 });
  assert.throws(
    () => open.closePaperTrade(trade.id, { exitPrice: 0.009 }),
    (e) => {
      assert.equal(e.code, 'NOT_OPEN');
      assert.match(e.message, /is closed, not open/);
      assert.equal(e.trade.exit_price, 0.002, 'and it carries the close that already happened');
      return true;
    },
  );
  assert.equal(open.paperTrade(trade.id).exit_price, 0.002, 'the first exit stands');
});

test('a close needs a price it could have been closed at', (t) => {
  const open = db(t);
  const trade = open.recordPaperFill(order());
  for (const bad of [undefined, null, 0, -1, 'later']) {
    assert.throws(() => open.closePaperTrade(trade.id, { exitPrice: bad }), (e) => e.code === 'INVALID');
  }
  assert.throws(() => open.closePaperTrade(trade.id, { exitPrice: 1, status: 'PENDING' }), (e) => e.code === 'INVALID');
  assert.equal(open.paperTrade(trade.id).status, 'OPEN', 'and none of that closed it');
});

test('selling part of a position closes that much and keeps the rest', (t) => {
  const open = db(t);
  const held = open.recordPaperFill(order({ sizeSol: 1, entryPrice: 0.001, strategy: 'syndicate-sniper' }));
  const { trade, remainder } = open.reducePaperTrade(held.id, { sizeSol: 0.25, exitPrice: 0.002 });

  assert.equal(trade.id, held.id, 'the part sold keeps the id');
  assert.equal(trade.size_sol, 0.25, 'and is the size that was sold');
  assert.equal(trade.pnl_sol, 0.25, 'P&L is on the quarter, not on the whole SOL');
  assert.equal(trade.status, 'CLOSED');

  assert.equal(remainder.size_sol, 0.75);
  assert.equal(remainder.status, 'OPEN');
  assert.equal(remainder.entry_price, held.entry_price, 'still measured from where it was bought');
  assert.equal(remainder.created_at, held.created_at, 'and still opened when it was opened');
  assert.equal(remainder.entry_sec, held.entry_sec);
  assert.equal(remainder.strategy, 'syndicate-sniper');

  const totals = open.paperSummary();
  assert.equal(totals.openCostSol, 0.75);
  assert.equal(totals.realisedSol, 0.25, 'the part still held has not made anything yet');
});

test('selling all of it in one go leaves nothing behind', (t) => {
  const open = db(t);
  const held = open.recordPaperFill(order({ sizeSol: 0.5 }));
  const { trade, remainder } = open.reducePaperTrade(held.id, { sizeSol: 0.5, exitPrice: 0.002 });
  assert.equal(remainder, null);
  assert.equal(trade.size_sol, 0.5);
  assert.equal(open.openPaperTrades().length, 0);
});

test('a remainder under a lamport is not left open to be tidied up later', (t) => {
  const open = db(t);
  const held = open.recordPaperFill(order({ sizeSol: 1 }));
  const { remainder } = open.reducePaperTrade(held.id, { sizeSol: 1 - 1e-11, exitPrice: 0.002 });
  assert.equal(remainder, null, 'close enough to all of it is all of it');
  assert.equal(open.openPaperTrades().length, 0);
});

test('you cannot sell more of a position than you have', (t) => {
  const open = db(t);
  const held = open.recordPaperFill(order({ sizeSol: 0.5 }));
  assert.throws(
    () => open.reducePaperTrade(held.id, { sizeSol: 2, exitPrice: 0.002 }),
    (e) => {
      assert.equal(e.code, 'INVALID');
      assert.match(e.message, /cannot sell 2 SOL of a 0.5 SOL position/);
      return true;
    },
  );
  assert.equal(open.paperTrade(held.id).status, 'OPEN', 'and the attempt changed nothing');
  assert.equal(open.paperTrade(held.id).size_sol, 0.5);
});

test('a part sale that cannot be completed writes neither half', (t) => {
  const open = db(t);
  const held = open.recordPaperFill(order({ sizeSol: 1 }));
  // No exit price: the close inside the transaction refuses, and the resize
  // that ran before it has to go with it.
  assert.throws(() => open.reducePaperTrade(held.id, { sizeSol: 0.25 }), (e) => e.code === 'INVALID');
  assert.equal(open.paperTrade(held.id).size_sol, 1, 'the position is whole');
  assert.equal(open.paperTrade(held.id).status, 'OPEN');
  assert.equal(open.paperTrades({}).rows.length, 1, 'and no remainder row was left behind');
});

test('a position sold down in pieces adds up to what it was', (t) => {
  const open = db(t);
  let id = open.recordPaperFill(order({ sizeSol: 1, entryPrice: 0.001 })).id;
  for (const piece of [0.25, 0.25, 0.25]) {
    id = open.reducePaperTrade(id, { sizeSol: piece, exitPrice: 0.002 }).remainder.id;
  }
  const last = open.closePaperTrade(id, { exitPrice: 0.002 });

  assert.equal(last.size_sol, 0.25);
  const totals = open.paperSummary();
  assert.equal(totals.open, 0);
  assert.equal(totals.closed, 4, 'four exits, one position');
  assert.equal(totals.realisedSol, 1, 'and 1 SOL doubled is 1 SOL made, however it was sold');
});

test('closing one position leaves the others exactly as they were', (t) => {
  const open = db(t);
  const a = open.recordPaperFill(order());
  const b = open.recordPaperFill(order({ tokenAddress: OTHER, sizeSol: 1 }));
  const before = { ...open.paperTrade(b.id) };

  open.closePaperTrade(a.id, { exitPrice: 0.002 });
  assert.deepEqual({ ...open.paperTrade(b.id) }, before);
});

test('an id has to be an id', (t) => {
  const open = db(t);
  for (const bad of [0, -1, 1.5, 'one', null, undefined]) {
    assert.throws(() => open.paperTrade(bad), (e) => e.code === 'INVALID');
  }
});

// ---------------------------------------------------------------------------
// Reading it back
// ---------------------------------------------------------------------------

test('open positions are the open ones, newest first', (t) => {
  const open = db(t);
  const first = open.recordPaperFill(order());
  const second = open.recordPaperFill(order({ tokenAddress: OTHER }));
  const third = open.recordPaperFill(order());
  open.closePaperTrade(second.id, { exitPrice: 0.002 });

  assert.deepEqual(open.openPaperTrades().map((r) => r.id), [third.id, first.id]);
  assert.deepEqual(open.openPaperTrades({ token: OTHER }).map((r) => r.id), [], 'the one on that coin is closed');
  assert.deepEqual(open.openPaperTrades({ token: MINT }).map((r) => r.id), [third.id, first.id]);
});

test('a strategy can be asked about on its own', (t) => {
  const open = db(t);
  open.recordPaperFill(order({ strategy: 'syndicate-sniper' }));
  open.recordPaperFill(order({ strategy: 'manual' }));
  open.recordPaperFill(order({ strategy: 'syndicate-sniper' }));

  assert.equal(open.paperTrades({ strategy: 'syndicate-sniper' }).rows.length, 2);
  assert.equal(open.paperTrades({ strategy: 'manual' }).rows.length, 1);
  assert.equal(open.paperTrades({ strategy: 'nobody' }).rows.length, 0);
});

test('paging by cursor covers every trade once, in order', (t) => {
  const open = db(t);
  const ids = [];
  for (let i = 0; i < 25; i++) ids.push(open.recordPaperFill(order({ sizeSol: i + 1 })).id);
  const newestFirst = [...ids].reverse();

  const seen = [];
  let cursor = null;
  let pages = 0;
  do {
    const page = open.paperTrades({ limit: 7, cursor });
    seen.push(...page.rows.map((r) => r.id));
    cursor = page.nextCursor;
    pages++;
    assert.ok(pages < 10, 'paging terminates');
  } while (cursor);

  assert.equal(pages, 4, '25 rows at 7 a page');
  assert.deepEqual(seen, newestFirst, 'every row once, newest first');
});

test('the last page says there is nothing after it', (t) => {
  const open = db(t);
  open.recordPaperFill(order());
  open.recordPaperFill(order());
  const page = open.paperTrades({ limit: 2 });
  assert.equal(page.rows.length, 2);
  assert.equal(page.nextCursor, null, 'exactly a full page is still the last one');
});

test('a trade arriving mid-read does not push another off the next page', (t) => {
  const open = db(t);
  const ids = [];
  for (let i = 0; i < 6; i++) ids.push(open.recordPaperFill(order()).id);

  const first = open.paperTrades({ limit: 3 });
  // Someone buys while the reader is on page one. With an offset this would
  // shift everything down and repeat a row; the cursor is an id, so it cannot.
  open.recordPaperFill(order());
  const second = open.paperTrades({ limit: 3, cursor: first.nextCursor });

  assert.deepEqual(first.rows.map((r) => r.id), [ids[5], ids[4], ids[3]]);
  assert.deepEqual(second.rows.map((r) => r.id), [ids[2], ids[1], ids[0]]);
});

test('the page size is bounded at both ends', (t) => {
  const open = db(t);
  for (let i = 0; i < 3; i++) open.recordPaperFill(order());
  assert.equal(open.paperTrades({ limit: 1 }).rows.length, 1);
  assert.equal(open.paperTrades({ limit: 0 }).rows.length, 3, 'nought means the default, not nothing');
  assert.equal(open.paperTrades({ limit: -5 }).rows.length, 1, 'and a negative is clamped to one');
  assert.equal(open.paperTrades({ limit: 100_000 }).rows.length, 3, 'a huge limit is capped, not honoured');
});

test('a status that does not exist is refused rather than quietly matching nothing', (t) => {
  const open = db(t);
  assert.throws(() => open.paperTrades({ status: 'PENDING' }), (e) => e.code === 'INVALID');
  assert.throws(() => open.paperTrades({ cursor: 'soon' }), (e) => e.code === 'INVALID');
  assert.throws(() => open.paperTrades({ cursor: 0 }), (e) => e.code === 'INVALID');
});

test('history can be asked for as closed and cancelled together', (t) => {
  const open = db(t);
  const a = open.recordPaperFill(order());
  const b = open.recordPaperFill(order());
  open.recordPaperFill(order()); // stays open
  open.closePaperTrade(a.id, { exitPrice: 0.002 });
  open.closePaperTrade(b.id, { status: 'CANCELLED' });

  const history = open.paperTrades({ status: ['CLOSED', 'CANCELLED'] });
  assert.deepEqual(history.rows.map((r) => r.id), [b.id, a.id]);
});

test('the totals are counted over everything, not over a page of it', (t) => {
  const open = db(t);
  const won = open.recordPaperFill(order({ sizeSol: 1, entryPrice: 0.001 }));
  const lost = open.recordPaperFill(order({ sizeSol: 1, entryPrice: 0.001 }));
  const gone = open.recordPaperFill(order({ sizeSol: 1, entryPrice: 0.001 }));
  open.recordPaperFill(order({ sizeSol: 0.75, entryPrice: 0.001 })); // still open
  open.closePaperTrade(won.id, { exitPrice: 0.002 }); // +1
  open.closePaperTrade(lost.id, { exitPrice: 0.0005 }); // -0.5
  open.closePaperTrade(gone.id, { status: 'CANCELLED' });

  const summary = open.paperSummary();
  assert.equal(summary.open, 1);
  assert.equal(summary.closed, 2);
  assert.equal(summary.cancelled, 1);
  assert.equal(summary.openCostSol, 0.75, 'what is still at risk');
  assert.equal(summary.realisedSol, 0.5, '+1 and -0.5');
  assert.equal(summary.wins, 1);
  assert.equal(summary.losses, 1);
  assert.equal(summary.winRate, 0.5);
  // The cancelled one is counted but never scored: it has no exit to judge.
  assert.equal(summary.wins + summary.losses, 2);
});

test('the totals can be narrowed to one coin', (t) => {
  const open = db(t);
  const mine = open.recordPaperFill(order({ sizeSol: 1, entryPrice: 0.001 }));
  open.recordPaperFill(order({ tokenAddress: OTHER, sizeSol: 5, entryPrice: 0.001 }));
  open.closePaperTrade(mine.id, { exitPrice: 0.002 });

  assert.equal(open.paperSummary({ token: MINT }).realisedSol, 1);
  assert.equal(open.paperSummary({ token: MINT }).openCostSol, 0);
  assert.equal(open.paperSummary({ token: OTHER }).openCostSol, 5);
  assert.equal(open.paperSummary({ token: OTHER }).realisedSol, 0);
});

test('an empty record reads as empty rather than as nothing', (t) => {
  const summary = db(t).paperSummary();
  assert.deepEqual(summary, {
    open: 0, closed: 0, cancelled: 0, openCostSol: 0, realisedSol: 0, wins: 0, losses: 0, winRate: null,
  });
});

// ---------------------------------------------------------------------------
// Still there afterwards — the reason any of this moved off the browser
// ---------------------------------------------------------------------------

test('a trade outlives the connection that wrote it', (t) => {
  const dir = tmp(t);
  const first = new Db({ dir });
  const trade = first.recordPaperFill(order({ sizeSol: 3, strategy: 'syndicate-sniper' }));
  first.closePaperTrade(trade.id, { exitPrice: 0.002 });
  first.close();

  const second = new Db({ dir });
  t.after(() => second.close());
  const back = second.paperTrade(trade.id);
  assert.equal(back.strategy, 'syndicate-sniper');
  assert.equal(back.size_sol, 3);
  assert.equal(back.pnl_sol, 3);
  assert.equal(back.status, 'CLOSED');
});

test('opening the table on a database that predates it adds it and keeps everything else', (t) => {
  const dir = tmp(t);
  const before = new Db({ dir });
  // A database from before this table existed.
  before.sql.exec('DROP TABLE paper_trades');
  before.insertTokens([{ mint: MINT, symbol: 'TEST', t: 1786554449571 }]);
  before.close();

  const after = new Db({ dir });
  t.after(() => after.close());
  assert.equal(after.count(), 1, 'the coins are still there');
  assert.ok(after.recordPaperFill(order()).id > 0, 'and the table came back');
});

// ---------------------------------------------------------------------------
// Over HTTP
// ---------------------------------------------------------------------------

/** A dashboard on its own port and its own directory, stopped afterwards. */
async function boot(t, dir = tmp(t)) {
  const server = serve({ port: 0, dir, open: false, listen: false, status: () => {} });
  await once(server, 'listening');
  t.after(() => server.stop());
  const base = `http://127.0.0.1:${server.address().port}`;
  const call = async (p, init) => {
    const res = await fetch(base + p, init);
    return { status: res.status, body: await res.json() };
  };
  return {
    dir,
    server,
    get: (p) => call(p),
    post: (p, body) => call(p, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(body),
    }),
    raw: call,
  };
}

test('the board starts with an empty record rather than a missing one', async (t) => {
  const api = await boot(t);
  const { status, body } = await api.get('/api/paper/trades');
  assert.equal(status, 200);
  assert.deepEqual(body.open, []);
  assert.deepEqual(body.closed, []);
  assert.equal(body.nextCursor, null);
  assert.equal(body.summary.open, 0);
  assert.equal(body.summary.realisedSol, 0);
});

test('an order is placed, read back open, closed, and read back closed', async (t) => {
  const api = await boot(t);

  const placed = await api.post('/api/paper/order', {
    tokenAddress: MINT, strategy: 'syndicate-sniper', side: 'BUY', sizeSol: 0.25, entryPrice: 0.001,
  });
  assert.equal(placed.status, 201);
  assert.equal(placed.body.trade.status, 'OPEN');
  assert.equal(placed.body.trade.strategy, 'syndicate-sniper');
  assert.equal(placed.body.filledAt, 0.001);
  const id = placed.body.trade.id;

  const holding = await api.get('/api/paper/trades');
  assert.deepEqual(holding.body.open.map((r) => r.id), [id]);
  assert.deepEqual(holding.body.closed, []);
  assert.equal(holding.body.summary.openCostSol, 0.25);

  const closed = await api.post('/api/paper/close', { id, exitPrice: 0.002 });
  assert.equal(closed.status, 200);
  assert.equal(closed.body.trade.status, 'CLOSED');
  assert.equal(closed.body.trade.pnl_sol, 0.25, '0.25 SOL doubled');
  assert.equal(closed.body.trade.pnl_pct, 100);

  const after = await api.get('/api/paper/trades');
  assert.deepEqual(after.body.open, []);
  assert.deepEqual(after.body.closed.map((r) => r.id), [id]);
  assert.equal(after.body.summary.openCostSol, 0);
  assert.equal(after.body.summary.realisedSol, 0.25);
  assert.equal(after.body.summary.winRate, 1);
});

test('a short is placed and closed the same way, and earns the fall', async (t) => {
  const api = await boot(t);
  const { body } = await api.post('/api/paper/order', { tokenAddress: MINT, side: 'SELL', sizeSol: 1, entryPrice: 0.002 });
  const closed = await api.post('/api/paper/close', { id: body.trade.id, exitPrice: 0.001 });
  assert.equal(closed.body.trade.side, 'SELL');
  assert.equal(closed.body.trade.pnl_sol, 0.5);
  assert.equal(closed.body.trade.pnl_pct, 50);
});

test('an order with no price fills at the price the board is showing', async (t) => {
  const dir = tmp(t);
  // A coin the dashboard has already read out of the log, with a last price.
  fs.writeFileSync(
    path.join(dir, 'coins-2026-08-16.jsonl'),
    `${JSON.stringify({ mint: MINT, symbol: 'TEST', t: 1786554449571, who: [], outcome: { entry: 0.001, last: 0.0016 } })}\n`,
  );
  const api = await boot(t, dir);

  const placed = await api.post('/api/paper/order', { tokenAddress: MINT, sizeSol: 1 });
  assert.equal(placed.status, 201);
  assert.equal(placed.body.trade.entry_price, 0.0016, 'the last price seen, not a guess');
  assert.equal(placed.body.quoted, true, 'and it says the price came from here');

  const closed = await api.post('/api/paper/close', { id: placed.body.trade.id });
  assert.equal(closed.status, 200);
  assert.equal(closed.body.trade.exit_price, 0.0016, 'the close quotes the same way');
  assert.equal(closed.body.trade.pnl_sol, 0);
});

test('an order on a coin with no price anywhere is refused, not filled at nothing', async (t) => {
  const api = await boot(t);
  const { status, body } = await api.post('/api/paper/order', { tokenAddress: 'NeverSeen', sizeSol: 1 });
  assert.equal(status, 400);
  assert.match(body.error, /no live price/);
  assert.deepEqual((await api.get('/api/paper/trades')).body.open, [], 'and nothing was written');
});

test('the refusals a person actually meets, each with its own status', async (t) => {
  const api = await boot(t);
  const open = await api.post('/api/paper/order', { tokenAddress: MINT, sizeSol: 1, entryPrice: 0.001 });
  const id = open.body.trade.id;

  const bad = await api.post('/api/paper/order', { tokenAddress: MINT, sizeSol: 1, entryPrice: 0.001, side: 'HODL' });
  assert.equal(bad.status, 400);
  assert.match(bad.body.error, /side must be BUY or SELL/);

  const sizeless = await api.post('/api/paper/order', { tokenAddress: MINT, entryPrice: 0.001 });
  assert.equal(sizeless.status, 400);
  assert.match(sizeless.body.error, /size_sol/);

  const nameless = await api.post('/api/paper/order', { sizeSol: 1, entryPrice: 0.001 });
  assert.equal(nameless.status, 400);

  const missing = await api.post('/api/paper/close', { id: 9999, exitPrice: 0.002 });
  assert.equal(missing.status, 404);
  assert.match(missing.body.error, /no paper trade with id 9999/);

  const noId = await api.post('/api/paper/close', { exitPrice: 0.002 });
  assert.equal(noId.status, 400);

  await api.post('/api/paper/close', { id, exitPrice: 0.002 });
  const twice = await api.post('/api/paper/close', { id, exitPrice: 0.009 });
  assert.equal(twice.status, 409, 'closing a closed position is a conflict, not a bad request');
  assert.equal(twice.body.trade.exit_price, 0.002, 'and the answer carries the close that stands');

  const junk = await api.raw('/api/paper/order', { method: 'POST', body: '{not json' });
  assert.equal(junk.status, 400);
  assert.match(junk.body.error, /not valid JSON/);

  const listed = await api.raw('/api/paper/trades', { method: 'POST', body: '{}' });
  assert.equal(listed.status, 405);
  const posted = await api.get('/api/paper/order');
  assert.equal(posted.status, 405);
});

test('a part sale over HTTP answers with both halves of it', async (t) => {
  const api = await boot(t);
  const placed = await api.post('/api/paper/order', { tokenAddress: MINT, sizeSol: 1, entryPrice: 0.001 });

  const sold = await api.post('/api/paper/close', { id: placed.body.trade.id, sizeSol: 0.4, exitPrice: 0.002 });
  assert.equal(sold.status, 200);
  assert.equal(sold.body.trade.size_sol, 0.4);
  assert.equal(sold.body.trade.pnl_sol, 0.4);
  assert.equal(sold.body.remainder.size_sol, 0.6);
  assert.equal(sold.body.remainder.status, 'OPEN');

  const state = await api.get('/api/paper/trades');
  assert.deepEqual(state.body.open.map((r) => r.size_sol), [0.6]);
  assert.equal(state.body.summary.openCostSol, 0.6);
  assert.equal(state.body.summary.realisedSol, 0.4);

  const tooMuch = await api.post('/api/paper/close', { id: state.body.open[0].id, sizeSol: 9, exitPrice: 0.002 });
  assert.equal(tooMuch.status, 400);
  assert.match(tooMuch.body.error, /cannot sell/);
});

test('a full close over HTTP still reports no remainder', async (t) => {
  const api = await boot(t);
  const placed = await api.post('/api/paper/order', { tokenAddress: MINT, sizeSol: 1, entryPrice: 0.001 });
  const closed = await api.post('/api/paper/close', { id: placed.body.trade.id, exitPrice: 0.002 });
  assert.equal(closed.body.remainder, null);
});

test('a cancel over HTTP records no exit and no P&L', async (t) => {
  const api = await boot(t);
  const { body } = await api.post('/api/paper/order', { tokenAddress: MINT, sizeSol: 1, entryPrice: 0.001 });
  const cancelled = await api.post('/api/paper/close', { id: body.trade.id, status: 'CANCELLED' });

  assert.equal(cancelled.status, 200);
  assert.equal(cancelled.body.trade.status, 'CANCELLED');
  assert.equal(cancelled.body.trade.exit_price, null);
  assert.equal(cancelled.body.trade.pnl_sol, null);

  const after = await api.get('/api/paper/trades');
  assert.deepEqual(after.body.open, []);
  assert.deepEqual(after.body.closed.map((r) => r.status), ['CANCELLED'], 'the record shows what happened to it');
  assert.equal(after.body.summary.winRate, null, 'and it is not scored');
});

test('the history pages and filters over HTTP the way it does underneath', async (t) => {
  const api = await boot(t);
  const ids = [];
  for (let i = 0; i < 5; i++) {
    const placed = await api.post('/api/paper/order', { tokenAddress: i % 2 ? OTHER : MINT, sizeSol: 1, entryPrice: 0.001 });
    ids.push(placed.body.trade.id);
    await api.post('/api/paper/close', { id: placed.body.trade.id, exitPrice: 0.002 });
  }

  const first = await api.get('/api/paper/trades?limit=2');
  assert.deepEqual(first.body.closed.map((r) => r.id), [ids[4], ids[3]]);
  assert.equal(first.body.nextCursor, ids[3]);

  const second = await api.get(`/api/paper/trades?limit=2&cursor=${first.body.nextCursor}`);
  assert.deepEqual(second.body.closed.map((r) => r.id), [ids[2], ids[1]]);

  const byCoin = await api.get(`/api/paper/trades?token=${OTHER}`);
  assert.deepEqual(byCoin.body.closed.map((r) => r.id), [ids[3], ids[1]]);
  assert.equal(byCoin.body.summary.closed, 2, 'and the totals follow the filter');

  const openOnly = await api.get('/api/paper/trades?status=OPEN');
  assert.deepEqual(openOnly.body.open, []);
  assert.deepEqual(openOnly.body.closed, []);

  const nonsense = await api.get('/api/paper/trades?status=SOON');
  assert.equal(nonsense.status, 400);
});

test('a position survives the dashboard being closed and reopened', async (t) => {
  const dir = tmp(t);
  const before = await boot(t, dir);
  const placed = await before.post('/api/paper/order', {
    tokenAddress: MINT, strategy: 'syndicate-sniper', sizeSol: 2, entryPrice: 0.001,
  });
  await before.server.stop();

  // A different server, a different connection, the same directory. This is the
  // whole claim: state that used to live in one browser tab now outlives it.
  const after = await boot(t, dir);
  const { body } = await after.get('/api/paper/trades');
  assert.deepEqual(body.open.map((r) => r.id), [placed.body.trade.id]);
  assert.equal(body.open[0].strategy, 'syndicate-sniper');
  assert.equal(body.summary.openCostSol, 2);

  // And it can still be closed from the new one.
  const closed = await after.post('/api/paper/close', { id: placed.body.trade.id, exitPrice: 0.0015 });
  assert.equal(closed.status, 200);
  assert.equal(closed.body.trade.pnl_sol, 1);
});

test('two dashboards on one database see each other, which is why it is not in the browser', async (t) => {
  const dir = tmp(t);
  const one = await boot(t, dir);
  const two = await boot(t, dir);

  const placed = await one.post('/api/paper/order', { tokenAddress: MINT, sizeSol: 1, entryPrice: 0.001 });
  assert.deepEqual((await two.get('/api/paper/trades')).body.open.map((r) => r.id), [placed.body.trade.id]);

  await two.post('/api/paper/close', { id: placed.body.trade.id, exitPrice: 0.002 });
  const seenByOne = await one.get('/api/paper/trades');
  assert.deepEqual(seenByOne.body.open, []);
  assert.equal(seenByOne.body.closed[0].pnl_sol, 1);
});

test('a request the URL parser cannot read is answered, not fatal', async (t) => {
  // "//" is not a path new URL() will take, and parsing it outside the request
  // handler's try took the whole dashboard down with it — a stray link or a
  // port scanner was enough. Found by asking curl for it while testing this.
  const api = await boot(t);
  const res = await fetch(`http://127.0.0.1:${api.server.address().port}//`);
  assert.equal(res.status, 500);
  assert.match((await res.json()).error, /Invalid URL/);
  // And the server is still up afterwards, which is the whole point.
  assert.equal((await api.get('/api/paper/trades')).status, 200);
});

test('the endpoints that only read are still there and unchanged', async (t) => {
  // The paper routes write, and they were added to a file whose whole promise is
  // that it does not. This is the check that the promise still holds elsewhere.
  const api = await boot(t);
  const status = await api.get('/api/status');
  assert.equal(status.status, 200);
  assert.equal(status.body.coins, 0);
  assert.equal((await api.get('/api/coins')).status, 200);
  assert.equal((await api.get('/api/candidates')).status, 200);
  assert.equal((await api.get('/api/nope')).status, 404);
});

// ---------------------------------------------------------------------------
// The trade that did nothing
// ---------------------------------------------------------------------------
//
// A position closed at the price it opened at is not a win and it is not a loss.
// It used to be counted as a loss, which made the record read worse than it was
// and disagreed with `summarise` in backtest.js over the same question. Both
// halves of the fix are pinned here: which bucket a level trade lands in, and
// what the win rate is divided by once it lands in neither.

test('a trade that came out level is neither a win nor a loss', (t) => {
  const open = db(t);
  const flat = open.recordPaperFill(order({ sizeSol: 1, entryPrice: 0.001 }));
  open.closePaperTrade(flat.id, { exitPrice: 0.001 });

  const summary = open.paperSummary();
  assert.equal(summary.closed, 1, 'it closed');
  assert.equal(summary.realisedSol, 0, 'and made nothing');
  assert.equal(summary.wins, 0);
  assert.equal(summary.losses, 0, 'making nothing is not losing');
  assert.equal(summary.winRate, 0, 'nor is it being right');
});

test('a level trade is level on both sides of the book', (t) => {
  // The same claim for a short, because a short closed at its entry is the case
  // where a sign error would have hidden: -(0) is still 0.
  const open = db(t);
  const short = open.recordPaperFill(order({ side: 'SELL', sizeSol: 2, entryPrice: 0.001 }));
  open.closePaperTrade(short.id, { exitPrice: 0.001 });

  const summary = open.paperSummary();
  assert.equal(summary.losses, 0);
  assert.equal(summary.wins, 0);
  assert.equal(summary.realisedSol, 0);
});

test('the win rate is over every closed trade, so a level one dilutes it', (t) => {
  // One won, one lost, one did nothing. Two of the three are scored, and the rate
  // is one in three rather than one in two: a trade you were right about nothing
  // on should pull the number down, not disappear from it.
  const open = db(t);
  const won = open.recordPaperFill(order({ sizeSol: 1, entryPrice: 0.001 }));
  const lost = open.recordPaperFill(order({ sizeSol: 1, entryPrice: 0.001 }));
  const flat = open.recordPaperFill(order({ sizeSol: 1, entryPrice: 0.001 }));
  open.closePaperTrade(won.id, { exitPrice: 0.002 });   // +1
  open.closePaperTrade(lost.id, { exitPrice: 0.0005 }); // -0.5
  open.closePaperTrade(flat.id, { exitPrice: 0.001 });  //  0

  const summary = open.paperSummary();
  assert.equal(summary.closed, 3);
  assert.equal(summary.wins, 1);
  assert.equal(summary.losses, 1);
  assert.equal(summary.winRate, round(1 / 3, 4), 'one of three, not one of two');
  assert.equal(summary.realisedSol, 0.5);
  // The gap between the two is the level trade, and it is meant to be there.
  assert.equal(summary.closed - (summary.wins + summary.losses), 1);
});

test('nothing but level trades is a rate of zero, not a rate of nothing', (t) => {
  // Zero and null are different answers: zero says "scored, and never right",
  // null says "nothing has closed yet". A record of flat trades is the first.
  const open = db(t);
  for (const _ of [1, 2]) {
    const flat = open.recordPaperFill(order({ sizeSol: 1, entryPrice: 0.001 }));
    open.closePaperTrade(flat.id, { exitPrice: 0.001 });
  }
  const summary = open.paperSummary();
  assert.equal(summary.closed, 2);
  assert.equal(summary.winRate, 0);
  assert.notEqual(summary.winRate, null);
});

test('cancelled positions stay out of the rate entirely', (t) => {
  // A cancel has no exit, so it is not a level trade — it is an unscored one, and
  // it must not appear in the denominator the way a level trade does.
  const open = db(t);
  const won = open.recordPaperFill(order({ sizeSol: 1, entryPrice: 0.001 }));
  const gone = open.recordPaperFill(order({ sizeSol: 1, entryPrice: 0.001 }));
  open.closePaperTrade(won.id, { exitPrice: 0.002 });
  open.closePaperTrade(gone.id, { status: 'CANCELLED' });

  const summary = open.paperSummary();
  assert.equal(summary.cancelled, 1);
  assert.equal(summary.closed, 1);
  assert.equal(summary.winRate, 1, 'one closed trade, and it won');
});

test('the paper record scores a trade the way the backtester does', (t) => {
  // The two live in different files and answer the same question, and they drifted
  // once already. This is the pin: the same set of outcomes, counted by both, has
  // to come back with the same wins, the same losses and the same rate.
  const open = db(t);
  const outcomes = [
    { exit: 0.002, pnl: 1 },       // won
    { exit: 0.0005, pnl: -0.5 },   // lost
    { exit: 0.001, pnl: 0 },       // level
    { exit: 0.0015, pnl: 0.5 },    // won
  ];
  for (const o of outcomes) {
    const held = open.recordPaperFill(order({ sizeSol: 1, entryPrice: 0.001 }));
    open.closePaperTrade(held.id, { exitPrice: o.exit });
  }
  const paper = open.paperSummary();

  // summarise() takes trades in its own shape; only the P&L and the size matter
  // to the counting, which is the part being compared.
  const engine = summarise({
    trades: outcomes.map((o, i) => ({ pnlSol: o.pnl, sizeSol: 1, holdSec: 1, balanceSol: 10 + i })),
    equity: [{ t: 0, balance: 10, trade: 0 }],
    initialBalanceSol: 10,
    positionSizeSol: 1,
    slippageBps: 0,
    feeSol: 0,
  });

  assert.equal(paper.wins, engine.wins, 'wins');
  assert.equal(paper.losses, engine.losses, 'losses');
  assert.equal(paper.closed, engine.trades, 'and both counted the same trades');
  assert.equal(paper.winRate, round(engine.winRatePct / 100, 4), 'same rate, different units');
});

test('a level trade reads as level over HTTP too', async (t) => {
  const api = await boot(t);
  const placed = await api.post('/api/paper/order', {
    tokenAddress: MINT, strategy: 'manual', side: 'BUY', sizeSol: 1, entryPrice: 0.001,
  });
  assert.equal(placed.status, 201);

  const closed = await api.post('/api/paper/close', { id: placed.body.trade.id, exitPrice: 0.001 });
  assert.equal(closed.status, 200);
  assert.equal(closed.body.trade.pnl_sol, 0);

  const { body } = await api.get('/api/paper/trades');
  assert.equal(body.summary.closed, 1);
  assert.equal(body.summary.wins, 0);
  assert.equal(body.summary.losses, 0, 'the screen must not call it a loss either');
  assert.equal(body.summary.winRate, 0);
});

/** The same rounding paperSummary uses, so the expectations are written the same way. */
function round(n, dp = 4) {
  const f = 10 ** dp;
  return Math.round(Number(n) * f) / f;
}
