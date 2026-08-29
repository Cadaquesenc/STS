// The thread that does the writing, so the thread that does the reading never
// has to wait for a disk.
//
// STS already batches: `Records` holds coins until there are sixty-four of them
// or a quarter-second has passed, then commits the lot. What it does not do is
// get out of the way. That commit runs on the same thread as the socket, and
// while it runs nothing else on that thread does — not the parser, not the
// timers, not the next message off the wire.
//
// It is worth being exact about the size of that, because the usual line — "a
// commit is an fsync" — is not true here. WAL with synchronous=NORMAL does not
// fsync per commit; it appends to the log and leaves the fsync for a
// checkpoint. What costs is the work itself, and it is still enough to matter:
// twenty thousand telemetry rows written in batches of five hundred hold the
// loop for about 33 ms on this Mac, and a timer set for every 5 ms does not fire
// once in that window. Through the worker the same rows cost this thread 3 ms of
// posting, and the timer keeps its turn. Every trade that lands while the event
// loop is busy is a trade timestamped late, and at a spike that is the whole
// problem.
//
// So the writing moves to its own thread and its own connection. WAL is what
// makes that legal — one writer at a time, readers never blocked — and
// busy_timeout is what makes it survive the moment the dashboard writes a paper
// order into the same file. Neither is new here; see the pragmas in db.js.
//
// Two channels, on purpose. Rows go down a MessageChannel that carries nothing
// else, and asking-and-answering (flush, stats, close, and the report of a batch
// that failed) happens on the worker's own parentPort. The data path is
// one-way and never blocks on a reply; the control path stays answerable even
// when tens of thousands of rows are queued ahead of it. It also leaves the row
// end of the channel transferable: a future decoder thread can be handed it and
// write telemetry without routing through the main thread at all.
//
// The cost of two channels is that they have no order between them — a row
// posted just before `flush()` could arrive after the flush request. Every
// request therefore carries `upto`, the number of rows posted before it was
// made, and the worker holds the request until it has actually received that
// many. Without it, "flush and tell me what you wrote" would occasionally miss
// the last row, which is exactly the kind of failure that shows up as a flaky
// test and gets blamed on the test.
//
//   const storage = new StorageWorker({ dir });
//   await storage.ready;
//   storage.telemetry({ metric: 'trades_per_min', value: 412 });
//   await storage.close();            // flushes what is queued, then joins
import { EventEmitter } from 'node:events';
import { MessageChannel, Worker, isMainThread, parentPort, workerData } from 'node:worker_threads';

import { Db, dataDir, positionRow, snapshotRow, telemetryRow } from '../db.js';

/** Flush at five hundred rows or a tenth of a second, whichever comes first. */
export const DEFAULTS = Object.freeze({ flushMs: 100, batchSize: 500, maxQueue: 50_000 });

// This file is both halves — the handle and the thread — so it is also its own
// worker entry point. The marker is what tells the two apart: `isMainThread`
// alone would start a writer inside any *other* worker that imported this
// module, which is a second connection nobody asked for.
const MARK = 'sts.storage.worker';

const KINDS = { p: 'positions', t: 'telemetry', s: 'snapshots' };

// ---------------------------------------------------------------------------
// The handle, on the thread that has the rows
// ---------------------------------------------------------------------------

export class StorageWorker extends EventEmitter {
  /**
   * Start the writer.
   *
   * `ready` resolves once its connection is open; rows sent before then are not
   * lost, they queue in the channel like any others. Await it when you want to
   * know the database opened at all.
   */
  constructor({ dir = dataDir(), file = null, name = 'sts-storage', ...limits } = {}) {
    super();
    const cfg = { ...DEFAULTS, ...limits };
    const { port1, port2 } = new MessageChannel();

    this.rows = port1;
    this.posted = 0;
    this.written = { positions: 0, telemetry: 0, snapshots: 0, rejected: 0 };
    this.dropped = 0;
    this.lost = 0;
    this.stopped = false;
    this.asked = new Map();
    this.nextId = 1;

    this.worker = new Worker(new URL(import.meta.url), {
      name,
      workerData: { mark: MARK, dir, file, port: port2, ...cfg },
      transferList: [port2],
    });

    this.ready = new Promise((resolve, reject) => {
      this.worker.once('message', (m) => (m?.t === 'ready' ? resolve(m) : reject(new Error(`storage worker said ${m?.t} before it was ready`))));
      this.worker.once('error', reject);
    });
    // Nothing is meant to observe this rejection except a caller that awaited
    // `ready`. An unhandled one would take the process down over a database
    // that failed to open, which the caller may well be able to live without.
    this.ready.catch(() => {});

    this.exited = new Promise((resolve) => this.worker.once('exit', resolve));
    this.worker.on('message', (m) => this.#heard(m));
    // A thread that throws is not a batch that failed. It is not called `error`
    // because EventEmitter treats an unlistened-to `error` as fatal, and losing
    // the telemetry writer should not be how the watcher dies.
    this.worker.on('error', (err) => this.emit('worker-error', err));
    this.worker.on('exit', () => {
      this.stopped = true;
      for (const { reject } of this.asked.values()) reject(new Error('storage worker exited before answering'));
      this.asked.clear();
    });
  }

  /** One position event, or many. Opening and closing are both events here. */
  position(input) {
    return this.#send('p', input, positionRow);
  }

  /** One measurement, or many. */
  telemetry(input) {
    return this.#send('t', input, telemetryRow);
  }

  /** One snapshot of what was true, or many. */
  snapshot(input) {
    return this.#send('s', input, snapshotRow);
  }

  /** Commit everything posted so far, and say what it came to. */
  flush() {
    return this.#ask('flush');
  }

  /** The worker's own counters, and the pragmas its connection actually has. */
  stats() {
    return this.#ask('stats');
  }

  /**
   * Flush what is queued, close the database, and wait for the thread to end.
   *
   * Required, not optional: the worker holds the process open on purpose, so
   * that unwritten rows are a reason to stay alive rather than something a fast
   * exit silently drops. Call it from wherever `db.close()` is called.
   */
  async close() {
    if (this.closing) return this.closing;
    this.closing = (async () => {
      // A worker that never opened its database still has a thread to join.
      const done = this.stopped ? null : this.#ask('close');
      this.stopped = true;
      try {
        await done;
      } catch {}
      await this.exited;
      this.rows.close();
      return this.written;
    })();
    return this.closing;
  }

  /** Let the process exit with rows still queued. Rarely what you want. */
  unref() {
    this.worker.unref();
    return this;
  }

  #send(kind, input, toRow) {
    if (this.stopped) throw new Error('storage worker is closed');
    const list = Array.isArray(input) ? input : [input];
    if (!list.length) return 0;
    // Checked here rather than in the worker, deliberately. This is the thread
    // that made the mistake, so this is where the stack trace is worth reading;
    // a row rejected a thread away arrives as a message nobody is awaiting.
    const rows = list.map(toRow);
    this.posted += rows.length;
    this.rows.postMessage({ kind, rows });
    return rows.length;
  }

  #ask(t) {
    if (this.stopped && t !== 'close') return Promise.reject(new Error('storage worker is closed'));
    return new Promise((resolve, reject) => {
      const id = this.nextId++;
      this.asked.set(id, { resolve, reject });
      // `upto` is the promise that this answer will account for every row sent
      // before the question was asked. See the note at the top of the file.
      this.worker.postMessage({ t, id, upto: this.posted });
    });
  }

  #heard(m) {
    if (m?.t === 'flushed' || m?.t === 'closed') {
      for (const k of ['positions', 'telemetry', 'snapshots', 'rejected']) this.written[k] += m[k] ?? 0;
      this.dropped = m.dropped ?? this.dropped;
      this.lost = m.lost ?? this.lost;
      if (m.t === 'flushed' && m.id == null) this.emit('flushed', m);
    }
    if (m?.t === 'write-error') {
      this.lost = m.lost ?? this.lost;
      this.emit('write-error', m);
    }
    if (m?.id == null) return;
    const waiting = this.asked.get(m.id);
    if (!waiting) return;
    this.asked.delete(m.id);
    waiting.resolve(m);
  }
}

// ---------------------------------------------------------------------------
// The thread that holds the connection
// ---------------------------------------------------------------------------

function run(cfg) {
  const { port, flushMs, batchSize, maxQueue } = cfg;
  // Throwing here is the right failure: it reaches the main thread as the
  // worker's `error` event and rejects `ready`, which is where a caller is
  // already looking to find out whether the database opened.
  const db = new Db({ dir: cfg.dir, file: cfg.file });

  let batch = { positions: [], telemetry: [], snapshots: [] };
  let queued = 0;
  let received = 0;
  let timer = null;
  const waiting = [];
  const total = { positions: 0, telemetry: 0, snapshots: 0, rejected: 0, flushes: 0, dropped: 0, lost: 0 };

  function arm() {
    if (timer) return;
    timer = setTimeout(() => {
      timer = null;
      report(flush('timer'));
    }, flushMs);
    // A flush that is merely due must not be the reason the thread stays alive.
    timer.unref?.();
  }

  /**
   * Commit what is held, in one transaction, and hand back what it came to.
   *
   * A batch that cannot be committed is reported and dropped. That is the same
   * answer `Records.flush` gives for coins, but the reason is weaker here and
   * worth being honest about: coins have the JSONL archive behind them and can
   * be rebuilt, while telemetry that is dropped is simply gone. It is still the
   * right answer — the alternative is retrying into a full disk forever while
   * the queue grows — but the loss is counted and said out loud rather than
   * swallowed, so a run can be told apart from a run that wrote everything.
   */
  function flush(reason) {
    if (timer) {
      clearTimeout(timer);
      timer = null;
    }
    if (!queued) return { reason, positions: 0, telemetry: 0, snapshots: 0, rejected: 0, rows: 0, ms: 0 };

    const held = batch;
    const rows = queued;
    batch = { positions: [], telemetry: [], snapshots: [] };
    queued = 0;

    const started = performance.now();
    try {
      const out = db.writeBatch(held);
      total.flushes++;
      for (const k of ['positions', 'telemetry', 'snapshots', 'rejected']) total[k] += out[k];
      return { reason, ...out, rows, ms: Math.round((performance.now() - started) * 100) / 100 };
    } catch (err) {
      total.lost += rows;
      parentPort.postMessage({ t: 'write-error', reason, rows, lost: total.lost, message: err.message, code: err.code ?? null });
      return { reason, positions: 0, telemetry: 0, snapshots: 0, rejected: 0, rows, ms: 0, failed: true };
    }
  }

  /** Tell the main thread about a flush it did not ask for. */
  function report(out) {
    if (!out.rows) return;
    parentPort.postMessage({ t: 'flushed', ...out, dropped: total.dropped, lost: total.lost });
  }

  port.on('message', (m) => {
    const into = batch[KINDS[m?.kind]];
    if (!into) return;
    for (const row of m.rows) {
      // The queue has a ceiling because a writer that cannot keep up must not
      // be the thing that runs the machine out of memory. Newest rows are the
      // ones dropped: the older ones are already half-committed to a batch, and
      // dropping the front of the queue to make room for the back would lose
      // the same number of rows and scramble their order as well.
      // Counted as received even when dropped, and that is not a detail: `upto`
      // counts rows *posted*, so a request waiting on a row that was thrown
      // away would wait for a row that is never coming.
      received++;
      if (queued >= maxQueue) {
        total.dropped++;
        continue;
      }
      into.push(row);
      queued++;
    }
    if (queued >= batchSize) report(flush('rows'));
    else if (queued) arm();
    serve();
  });

  /**
   * Answer the requests whose rows have all arrived.
   *
   * In order, and never past one that is still waiting: requests are made in
   * order and each `upto` is at least the last, so serving them out of order
   * would answer a later question with an earlier state.
   */
  function serve() {
    while (waiting.length && received >= (waiting[0].upto ?? 0)) {
      const req = waiting.shift();
      if (req.t === 'stats') {
        parentPort.postMessage({
          t: 'stats',
          id: req.id,
          ...total,
          queued,
          received,
          file: db.file,
          // Read back off this thread's own connection, because that is the one
          // the claim is about. Pragmas are per connection, and a worker that
          // came up without WAL would be invisible from the main thread.
          // Read by position rather than by name: most pragmas answer in a
          // column called after themselves, but busy_timeout answers in one
          // called `timeout`, and a lookup by name quietly reads null there.
          pragmas: Object.fromEntries(
            ['journal_mode', 'synchronous', 'busy_timeout', 'cache_size', 'foreign_keys'].map((p) => [
              p,
              Object.values(db.sql.prepare(`PRAGMA ${p}`).get() ?? {})[0] ?? null,
            ]),
          ),
        });
        continue;
      }

      const out = flush(req.t === 'close' ? 'close' : 'asked');
      if (req.t === 'flush') {
        parentPort.postMessage({ t: 'flushed', id: req.id, ...out, dropped: total.dropped, lost: total.lost });
        continue;
      }

      db.close();
      port.close();
      // `out` is this last batch; the running totals travel under their own key
      // so they cannot be added to the handle's tally a second time.
      parentPort.postMessage({ t: 'closed', id: req.id, ...out, dropped: total.dropped, lost: total.lost, total: { ...total }, queued, received });
      // Nothing is listening on either port now, so the thread has nothing left
      // holding it open and ends on its own. The main thread is waiting on
      // `exit` rather than on this message, so a close is only over when the
      // thread is actually gone.
      parentPort.close();
      return;
    }
  }

  parentPort.on('message', (m) => {
    if (!m?.t) return;
    waiting.push(m);
    serve();
  });

  parentPort.postMessage({ t: 'ready', file: db.file, flushMs, batchSize, maxQueue });
}

if (!isMainThread && workerData?.mark === MARK) run(workerData);
