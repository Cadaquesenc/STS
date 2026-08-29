// Minimal borsh reader. Only the types Anchor events actually use.
//
// u64/i64 come back as strings, not numbers or BigInt: these records are written
// straight to JSON, and JSON has neither. A string survives the round trip and
// still sorts and compares correctly for equal-width values.
import { encode as b58encode } from './base58.js';

export class Reader {
  constructor(buf) {
    this.buf = Buffer.isBuffer(buf) ? buf : Buffer.from(buf);
    this.off = 0;
  }

  get remaining() {
    return this.buf.length - this.off;
  }

  need(n) {
    if (this.remaining < n) throw new RangeError(`borsh: need ${n}, have ${this.remaining}`);
  }

  u8() {
    this.need(1);
    return this.buf[this.off++];
  }

  bool() {
    return this.u8() !== 0;
  }

  u32() {
    this.need(4);
    const v = this.buf.readUInt32LE(this.off);
    this.off += 4;
    return v;
  }

  u64() {
    this.need(8);
    const v = this.buf.readBigUInt64LE(this.off);
    this.off += 8;
    return v.toString();
  }

  i64() {
    this.need(8);
    const v = this.buf.readBigInt64LE(this.off);
    this.off += 8;
    return v.toString();
  }

  bytes(n) {
    this.need(n);
    const v = this.buf.subarray(this.off, this.off + n);
    this.off += n;
    return v;
  }

  pubkey() {
    return b58encode(this.bytes(32));
  }

  // Borsh strings are u32 length + utf8. A corrupt length is the most likely way
  // a mis-aligned read runs away, so cap it at what is left in the buffer.
  string() {
    const n = this.u32();
    this.need(n);
    return this.bytes(n).toString('utf8');
  }
}
