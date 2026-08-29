// Base58 (Bitcoin alphabet) — Solana addresses.
// Same implementation as weld/src/base58.js; copied rather than imported so flux
// stands alone and neither tool's layout can break the other.
const ALPHABET = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz';
const MAP = new Map([...ALPHABET].map((c, i) => [c, i]));

export function encode(bytes) {
  const b = Uint8Array.from(bytes);
  let zeros = 0;
  while (zeros < b.length && b[zeros] === 0) zeros++;
  // All-zero input carries no significant digits; the loop below would emit a
  // spurious leading digit for it.
  if (zeros === b.length) return '1'.repeat(zeros);

  const digits = [0];
  for (let i = zeros; i < b.length; i++) {
    let carry = b[i];
    for (let j = 0; j < digits.length; j++) {
      carry += digits[j] << 8;
      digits[j] = carry % 58;
      carry = (carry / 58) | 0;
    }
    while (carry > 0) {
      digits.push(carry % 58);
      carry = (carry / 58) | 0;
    }
  }

  let out = '1'.repeat(zeros);
  for (let i = digits.length - 1; i >= 0; i--) out += ALPHABET[digits[i]];
  return out;
}

export function decode(str) {
  if (typeof str !== 'string' || str.length === 0) throw new Error('base58: empty input');

  let zeros = 0;
  while (zeros < str.length && str[zeros] === '1') zeros++;
  if (zeros === str.length) return new Uint8Array(zeros);

  const bytes = [0];
  for (let i = zeros; i < str.length; i++) {
    const val = MAP.get(str[i]);
    if (val === undefined) throw new Error('base58: invalid character ' + JSON.stringify(str[i]));
    let carry = val;
    for (let j = 0; j < bytes.length; j++) {
      carry += bytes[j] * 58;
      bytes[j] = carry & 0xff;
      carry >>= 8;
    }
    while (carry > 0) {
      bytes.push(carry & 0xff);
      carry >>= 8;
    }
  }

  const out = new Uint8Array(zeros + bytes.length);
  for (let i = 0; i < bytes.length; i++) out[zeros + bytes.length - 1 - i] = bytes[i];
  return out;
}

export const isAddress = (str) => {
  try {
    return decode(str).length === 32;
  } catch {
    return false;
  }
};
