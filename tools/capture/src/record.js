// One line per coin, appended, never edited; optionally mirrored into SQLite and
// into the audit NDJSON.
//
// The file is still the archive. The database is a second destination, not a
// replacement: if a schema turns out wrong we rebuild it from these lines, so
// they keep being written exactly as before.
//
// Rows go to SQLite in micro-batches rather than one at a time. A commit is an
// fsync, and at a spike of launches one fsync per coin is what would eventually
// fall behind — so rows queue until there are `batchSize` of them or `flushMs`
// has passed, whichever comes first, and the whole batch commits together.
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

// The repository root. Three levels up because this file lives at
// tools/capture/src/ — it used to sit at src/, where one level was enough.
// The entry point passes `dir` explicitly and prints what it resolved to, so
// this is only ever the fallback; it is written down so the fallback is not a
// silent one.
const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..', '..');

/**
 * One record as one line, and never accidentally as two.
 *
 * `JSON.stringify` leaves U+2028 (LINE SEPARATOR) and U+2029 raw, because both
 * are legal inside a JSON string. Node's `readline` — and every other streaming
 * reader that splits on line terminators — treats them as the end of a line, so
 * a record containing one arrives as two fragments, neither of which parses.
 *
 * This is not hypothetical. `coins-2026-08-20.jsonl` line 1934 is a coin whose
 * on-chain name is "Power Belongs⁠<U+2028>in Human Hands", and it is the only
 * unreadable record in the entire corpus. Coin names, symbols and metadata URIs
 * are written by whoever launched the coin, so this is attacker-controlled text
 * going straight into the archive: one character in a ticker was enough to
 * destroy a row and silently corrupt the one after it.
 */
export function jsonLine(rec) {
  return JSON.stringify(rec).replace(/[\u2028\u2029]/g, (c) => (c === '\u2028' ? '\\u2028' : '\\u2029'));
}

/**
 * An append-only line writer for one fixed file.
 *
 * `Records` is about a recording session: it rotates, dedups on a key, mirrors
 * into SQLite and knows what a coin is. An offline pass wants none of that — it
 * wants to add lines to a named file and be sure they went through `jsonLine`,
 * so a name out of a metadata URI cannot split a row here either.
 */
export class Appender {
  constructor(file) {
    this.file = file;
    fs.mkdirSync(path.dirname(file), { recursive: true });
    this.stream = fs.createWriteStream(file, { flags: 'a' });
    this.written = 0;
  }
  write(rec) {
    this.stream.write(jsonLine(rec) + '\n');
    this.written++;
  }
  async close() {
    const s = this.stream;
    if (!s) return;
    this.stream = null;
    await new Promise((r) => s.end(r));
  }
}

export class Records {
  /**
   * `session` is the whole reason this class changed. Given one, the file is
   * named `<name>-<session>.jsonl` and stays that file for the life of the run.
   * Without one it falls back to the old `<name>-YYYY-MM-DD.jsonl`, which
   * rotates at UTC midnight — and that rotation is the mechanical fact that
   * turned a single fifteen-hour run into "2026-08-20" plus a "held-out"
   * 2026-08-21 that six later analyses treated as an independent day.
   */
  constructor({
    dir = process.env.STS_HOME || path.join(ROOT, 'data'),
    name = 'coins',
    key = null,
    audit = null,
    db = null,
    session = null,
    batchSize = 64,
    flushMs = 250,
  } = {}) {
    this.dir = dir; this.name = name; this.key = key; this.audit = audit;
    this.db = db; this.session = session; this.batchSize = batchSize; this.flushMs = flushMs;
    this.batch = []; this.flushTimer = null; this.stored = 0;
    this.keys = new Set(); this.stream = null; this.day = null; this.written = 0; this.pending = new Set();
    fs.mkdirSync(dir, { recursive: true });
    if (key) this.loadKeys();
  }
  get file() {
    const infix = this.session ?? new Date().toISOString().slice(0, 10);
    return path.join(this.dir, `${this.name}-${infix}.jsonl`);
  }
  /**
   * A row about the run rather than about a coin — the session header, the
   * heartbeat, a socket gap, the failure rollup, the footer.
   *
   * It goes into the same file as the coins on purpose: uptime, truncation and
   * the coins they apply to have to be readable from one file or they will
   * again be pieced together months later from timestamps. Session rows carry a
   * `k` and coin rows do not, so a reader separates them with one test. They
   * skip the mint dedup and the database mirror, neither of which they fit.
   */
  writeMeta(rec) {
    this.append(rec);
    return true;
  }
  write(rec) {
    if (this.key) { const value = rec?.[this.key]; if (value == null || this.keys.has(value)) return false; this.keys.add(value); }
    this.append(rec);
    if (this.db) this.queue(rec);
    this.written++; return true;
  }
  /** Put one line on disk, opening or rotating the stream if it has to. */
  append(rec) {
    // With a session there is one file for the whole run and nothing rotates.
    const day = this.session ?? new Date().toISOString().slice(0, 10);
    if (this.day !== day) { if (this.stream) this.stream.end(); this.stream = fs.createWriteStream(this.file, { flags: 'a' }); this.day = day; }
    const line = jsonLine(rec) + '\n';
    // Only listen for drain when the write actually went into the buffer, and
    // only once per backed-up stream. Attaching on every line — which is what
    // this did — piles up listeners that never fire, and a spike is exactly
    // when that starts costing memory.
    if (!this.stream.write(line) && !this.pending.has(this.stream)) {
      const s = this.stream;
      this.pending.add(s);
      s.once('drain', () => this.pending.delete(s));
    }
    this.audit?.emit('record', 'append', { name: this.name, bytes: Buffer.byteLength(line), mint: rec?.mint ?? null });
  }
  /** Hold a row for the next batch, and make sure a batch is coming. */
  queue(rec) {
    this.batch.push(rec);
    if (this.batch.length >= this.batchSize) return void this.flush();
    if (this.flushTimer) return;
    this.flushTimer = setTimeout(() => { this.flushTimer = null; this.flush(); }, this.flushMs);
    // A pending flush must never be the reason the process stays alive.
    this.flushTimer.unref?.();
  }
  /**
   * Commit whatever is queued. A database failure must not take the watcher down
   * or lose the coin: the JSONL line is already on disk by the time we get here,
   * so the batch is reported and dropped rather than retried forever.
   */
  flush() {
    if (this.flushTimer) { clearTimeout(this.flushTimer); this.flushTimer = null; }
    if (!this.batch.length || !this.db) return 0;
    const batch = this.batch;
    this.batch = [];
    try {
      const added = this.db.insertTokens(batch);
      this.stored += added;
      return added;
    } catch (err) {
      this.audit?.emit('error', 'db_write_failed', { name: this.name, rows: batch.length, message: err.message }, { level: 'error' });
      return 0;
    }
  }
  loadKeys() {
    // The union of both destinations. The files are authoritative, but a
    // backfilled database can know about days whose files have been archived
    // away, so neither one alone is the full picture.
    for (const file of fs.readdirSync(this.dir)) {
      // Both namings: the dated files the corpus was recorded under, and the
      // session-named files written from now on. A dedup that only knew one of
      // them would re-record every mint the other had already seen.
      if (!new RegExp(`^${this.name}-[^/]+\\.jsonl$`).test(file)) continue;
      for (const line of fs.readFileSync(path.join(this.dir, file), 'utf8').split('\n')) { if (!line) continue; try { const value = JSON.parse(line)?.[this.key]; if (value != null) this.keys.add(value); } catch {} }
    }
    if (this.db && this.key === 'mint') { try { for (const mint of this.db.mints()) this.keys.add(mint); } catch {} }
  }
  async close() {
    this.flush();
    if (!this.stream) return;
    const s = this.stream; this.stream = null; this.day = null;
    await new Promise((r) => s.end(r));
    this.pending.clear();
  }
}
