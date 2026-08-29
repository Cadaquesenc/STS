// Unified append-only NDJSON audit logger with daily and size-based rotation.
import fs from 'node:fs';
import path from 'node:path';

export const AUDIT_VERSION = 1;
export const EVENT_TYPES = Object.freeze({
  socket: 'socket',
  record: 'record',
  decode: 'decode',
  dashboard: 'dashboard',
  error: 'error',
});

export class AuditLogger {
  constructor({ dir, name = 'audit', maxBytes = 50 * 1024 * 1024, clock = Date, db = null } = {}) {
    if (!dir) throw new TypeError('AuditLogger requires dir');
    this.dir = dir;
    this.name = name;
    this.maxBytes = maxBytes;
    this.clock = clock;
    // The NDJSON file stays the primary record — it rotates, and it can be
    // tailed while a run is in progress. The database copy is for joining audit
    // events against the coins they refer to.
    this.db = db;
    this.mirrored = 0;
    this.stream = null;
    this.file = null;
    this.bytes = 0;
    fs.mkdirSync(dir, { recursive: true });
  }

  emit(type, action, data = {}, { level = 'info', actor = 'sts' } = {}) {
    if (!Object.values(EVENT_TYPES).includes(type)) throw new TypeError(`unknown audit type: ${type}`);
    const now = new this.clock();
    const event = {
      schema: 'sts.audit', version: AUDIT_VERSION,
      id: `${now.getTime()}-${process.pid}-${this.bytes}`,
      ts: now.toISOString(), level, type, action, actor, data,
    };
    const line = JSON.stringify(event) + '\n';
    this.#ensure(now, Buffer.byteLength(line));
    // No drain handler here on purpose. A buffered write still reaches the file
    // on its own, and the empty listener this used to attach never did anything
    // except accumulate one per line once the stream backed up.
    this.stream.write(line);
    this.bytes += Buffer.byteLength(line);
    // Never let the mirror break the thing it mirrors: the file write above has
    // already happened, and an audit log that throws is worse than one that is
    // missing a row.
    if (this.db) {
      try {
        this.db.insertAudit(type, event, now.getTime());
        this.mirrored++;
      } catch {}
    }
    return event;
  }

  #ensure(now, incoming) {
    const day = now.toISOString().slice(0, 10);
    if (this.stream && this.file && this.file.day === day && this.bytes + incoming <= this.maxBytes) return;
    this.stream?.end();
    let index = 0;
    const prefix = `${this.name}-${day}`;
    do {
      const suffix = index ? `-${index}` : '';
      this.file = { day, path: path.join(this.dir, `${prefix}${suffix}.ndjson`) };
      index++;
    } while (fs.existsSync(this.file.path) && fs.statSync(this.file.path).size + incoming > this.maxBytes);
    this.bytes = fs.existsSync(this.file.path) ? fs.statSync(this.file.path).size : 0;
    this.stream = fs.createWriteStream(this.file.path, { flags: 'a' });
  }

  async close() {
    const stream = this.stream;
    this.stream = null;
    if (stream) await new Promise((resolve) => stream.end(resolve));
  }
}
