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

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

export class Records {
  constructor({
    dir = process.env.STS_HOME || path.join(ROOT, 'data'),
    name = 'coins',
    key = null,
    audit = null,
    db = null,
    batchSize = 64,
    flushMs = 250,
  } = {}) {
    this.dir = dir; this.name = name; this.key = key; this.audit = audit;
    this.db = db; this.batchSize = batchSize; this.flushMs = flushMs;
    this.batch = []; this.flushTimer = null; this.stored = 0;
    this.keys = new Set(); this.stream = null; this.day = null; this.written = 0; this.pending = new Set();
    fs.mkdirSync(dir, { recursive: true });
    if (key) this.loadKeys();
  }
  get file() { return path.join(this.dir, `${this.name}-${new Date().toISOString().slice(0, 10)}.jsonl`); }
  write(rec) {
    if (this.key) { const value = rec?.[this.key]; if (value == null || this.keys.has(value)) return false; this.keys.add(value); }
    const day = new Date().toISOString().slice(0, 10);
    if (this.day !== day) { if (this.stream) this.stream.end(); this.stream = fs.createWriteStream(this.file, { flags: 'a' }); this.day = day; }
    const line = JSON.stringify(rec) + '\n';
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
    if (this.db) this.queue(rec);
    this.written++; return true;
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
      if (!new RegExp(`^${this.name}-\\d{4}-\\d{2}-\\d{2}\\.jsonl$`).test(file)) continue;
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
