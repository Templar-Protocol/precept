// Minimal, self-contained XDR reader/writer for exactly what this page needs: a Soroban
// authorization entry, and the HashIdPreimage that signs it. No dependency, no network.
//
// Why not a full Stellar SDK: @stellar/stellar-base 15.0.0 predates CAP-71 (no address-V2
// credentials, no with-address preimage), and @stellar/stellar-sdk 17.4.0 ships a brand-new union API
// (plain camel/snake-case objects, no switch()/arm()) that this page would be pinned to forever. The
// decoder below is ~300 auditable lines, and every number it produces is checked against the Python
// tool's own output in sign/test/.
//
// The payload binding deliberately re-uses the ORIGINAL BYTES of the entry's root invocation: nothing
// here re-serialises an SCVal tree, so a decoding bug can never change what gets hashed.

const ENVELOPE_SOROBAN_AUTHORIZATION = 9;
const ENVELOPE_SOROBAN_AUTHORIZATION_WITH_ADDRESS = 10;
const CRED_SOURCE_ACCOUNT = 0;
const CRED_ADDRESS = 1;
const CRED_ADDRESS_V2 = 2;

export const SCV = {
  BOOL: 0, VOID: 1, ERROR: 2, U32: 3, I32: 4, U64: 5, I64: 6, TIMEPOINT: 7, DURATION: 8,
  U128: 9, I128: 10, U256: 11, I256: 12, BYTES: 13, STRING: 14, SYMBOL: 15, VEC: 16, MAP: 17,
  ADDRESS: 18, CONTRACT_INSTANCE: 19, LEDGER_KEY_CONTRACT_INSTANCE: 20,
  LEDGER_KEY_NONCE: 21,
};

class Reader {
  constructor(bytes) {
    this.b = bytes;
    this.i = 0;
  }
  need(n) {
    if (this.i + n > this.b.length) throw new Error('XDR truncated');
  }
  u32() {
    this.need(4);
    const v = (this.b[this.i] << 24 | this.b[this.i + 1] << 16 | this.b[this.i + 2] << 8 | this.b[this.i + 3]) >>> 0;
    this.i += 4;
    return v;
  }
  i32() { return this.u32() | 0; }
  u64() {
    const hi = BigInt(this.u32());
    const lo = BigInt(this.u32());
    return (hi << 32n) | lo;
  }
  i64() {
    const v = this.u64();
    return v >= 1n << 63n ? v - (1n << 64n) : v;
  }
  /** `n` bytes plus XDR padding. Padding must be zero: canonical encodings only (anything else is
   *  refused rather than skipped, so two different byte strings can never decode to the same value). */
  bytes(n) {
    const pad = (4 - (n % 4)) % 4;
    this.need(n + pad);
    const out = this.b.subarray(this.i, this.i + n);
    for (let k = 0; k < pad; k++) {
      if (this.b[this.i + n + k] !== 0) throw new Error('non-zero XDR padding');
    }
    this.i += n + pad;
    return out;
  }
  opaque() { return this.bytes(this.u32()); }
  /** Strict UTF-8: invalid input throws instead of being replaced with U+FFFD. The bytes are read
   *  OUTSIDE the try, so a padding error is reported as a padding error, not as bad text. */
  string() {
    const raw = this.opaque();
    try {
      return new TextDecoder('utf-8', { fatal: true }).decode(raw);
    } catch (e) {
      throw new Error('invalid UTF-8 in a name or symbol');
    }
  }
  symbol() { return this.string(); }
}

/** Test seam: the same reader, with an `offset` getter (the page never uses this). */
export class ReaderForTest extends Reader {
  get offset() { return this.i; }
}

function scAddress(r) {
  const type = r.u32();
  if (type === 0) {
    const keyType = r.u32();
    if (keyType !== 0) throw new Error('unsupported public key type');
    return { kind: 'account', strkey: encodeAccount(r.bytes(32)) };
  }
  if (type === 1) return { kind: 'contract', strkey: encodeContract(r.bytes(32)) };
  throw new Error('unsupported SCAddress type ' + type);
}

// ---- strkey (only the two forms we must print) ------------------------------------------------
const B32 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
function base32(buf) {
  let bits = 0, value = 0, out = '';
  for (const byte of buf) {
    value = (value << 8) | byte; bits += 8;
    while (bits >= 5) { out += B32[(value >>> (bits - 5)) & 31]; bits -= 5; }
  }
  if (bits > 0) out += B32[(value << (5 - bits)) & 31];
  return out;
}

export function crc16xmodem(bytes) {
  let crc = 0;
  for (const byte of bytes) {
    crc ^= byte << 8;
    for (let i = 0; i < 8; i++) crc = crc & 0x8000 ? ((crc << 1) ^ 0x1021) & 0xffff : (crc << 1) & 0xffff;
  }
  return crc;
}

function strkey(versionByte, payload) {
  const body = new Uint8Array(1 + payload.length);
  body[0] = versionByte;
  body.set(payload, 1);
  const crc = crc16xmodem(body);
  return base32(new Uint8Array([...body, crc & 0xff, (crc >> 8) & 0xff]));
}

export function encodeAccount(raw32) { return strkey(6 << 3, raw32); }
export function encodeContract(raw32) { return strkey(2 << 3, raw32); }

export function decodeStrkeyAccount(text) {
  // inverse of encodeAccount, used only to confirm what we print round-trips
  if (!/^G[A-Z2-7]{55}$/.test(text)) throw new Error('not an account address');
  return base32Decode(text);
}

export function base32Decode(text) {
  let bits = 0, value = 0;
  const out = [];
  for (const ch of text) {
    const idx = B32.indexOf(ch);
    if (idx < 0) throw new Error('bad base32');
    value = (value << 5) | idx; bits += 5;
    if (bits >= 8) { out.push((value >>> (bits - 8)) & 0xff); bits -= 8; }
  }
  return new Uint8Array(out);
}

// ---- SCVal (decode for display; the payload never depends on this) -----------------------------
export function readScVal(r) {
  const type = r.u32();
  switch (type) {
    case SCV.BOOL: {
      const w = r.u32();
      if (w > 1) throw new Error('invalid XDR bool');
      return { type, value: w === 1 };
    }
    case SCV.VOID: return { type, value: null };
    case SCV.U32: return { type, value: r.u32() };
    case SCV.I32: return { type, value: r.i32() };
    case SCV.U64: return { type, value: r.u64() };
    case SCV.I64: return { type, value: r.i64() };
    case SCV.U128: { const hi = r.u64(); return { type, value: (hi << 64n) | r.u64() }; }
    case SCV.I128: {
      const hi = r.u64();
      const lo = r.u64();
      let v = (hi << 64n) | lo;
      if (v >= 1n << 127n) v -= 1n << 128n;
      return { type, value: v };
    }
    case SCV.BYTES: return { type, value: r.opaque() };
    case SCV.STRING: {
      const raw = r.opaque();
      try {
        return { type, value: new TextDecoder('utf-8', { fatal: true }).decode(raw), raw, valid: true };
      } catch (e) {
        return { type, value: null, raw, valid: false };   // shown as hex, never as replacement text
      }
    }
    case SCV.SYMBOL: return { type, value: r.symbol() };
    case SCV.ADDRESS: return { type, value: scAddress(r) };
    // SCVal's vec/map arms are OPTIONAL POINTERS on the wire: a presence word precedes the element
    // count (stellar-xdr `SCVal *vec<>` / `SCMap *map`). Getting this wrong shifts every following
    // read, so the presence flag is required to be exactly 1.
    case SCV.VEC: {
      const present = r.u32();
      if (present !== 1) throw new Error('unsupported SCVal vec presence word ' + present);
      const n = r.u32();
      const items = [];
      for (let i = 0; i < n; i++) items.push(readScVal(r));
      return { type, value: items };
    }
    case SCV.MAP: {
      const present = r.u32();
      if (present !== 1) throw new Error('unsupported SCVal map presence word ' + present);
      const n = r.u32();
      const entries = [];
      for (let i = 0; i < n; i++) entries.push([readScVal(r), readScVal(r)]);
      return { type, value: entries };
    }
    default:
      throw new Error('unsupported SCVal type ' + type + ' (refusing rather than guessing)');
  }
}

const SYMBOL_RE = /^[A-Za-z0-9_]{1,32}$/;

// Characters that must never reach a screen unescaped: C1 controls and DEL, soft hyphen, the Arabic
// letter mark, bidi controls/isolates, zero-width and invisible formatting, the BOM, interlinear
// annotation marks and the tag characters. C0 controls are already escaped by JSON quoting.
const UNSAFE = /[\u007f-\u009f\u00ad\u061c\u180e\u200b-\u200f\u2028-\u202e\u2060-\u206f\ufeff\ufff9-\ufffb\u{e0000}-\u{e007f}]/gu;

/** JSON-quote `text`, then escape every UNSAFE code point (`\uXXXX`, or `\u{X…}` outside the BMP).
 *  tools/multisig/smart_account.py:quote_string produces the identical text. */
export function quoteString(text) {
  return JSON.stringify(text).replace(UNSAFE, (c) => {
    const cp = c.codePointAt(0);
    return cp <= 0xffff ? '\\u' + cp.toString(16).padStart(4, '0') : '\\u{' + cp.toString(16) + '}';
  });
}

export function hex(bytes) {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
}

export function renderScVal(v) {
  switch (v.type) {
    case SCV.VOID: return 'void';
    case SCV.BOOL: return String(v.value);
    case SCV.ADDRESS: return v.value.strkey;
    case SCV.BYTES: return '0x' + hex(v.value);
    case SCV.SYMBOL:
      if (!SYMBOL_RE.test(v.value)) throw new Error('symbol with unusual characters refused');
      return v.value;
    case SCV.STRING: return v.valid ? quoteString(v.value) : `bytes(0x${hex(v.raw)})`;
    case SCV.VEC: return '[' + v.value.map(renderScVal).join(', ') + ']';
    case SCV.MAP: return '{' + v.value.map(([k, val]) => `${renderScVal(k)}: ${renderScVal(val)}`).join(', ') + '}';
    case SCV.U32: return `u32(${v.value})`;
    case SCV.I32: return `i32(${v.value})`;
    case SCV.U64: return `u64(${v.value})`;
    case SCV.I64: return `i64(${v.value})`;
    case SCV.U128: return `u128(${v.value})`;
    case SCV.I128: return `i128(${v.value})`;
    default: throw new Error('refusing to display SCVal type ' + v.type);
  }
}

// ---- authorization entry ----------------------------------------------------------------------
function readInvocation(r) {
  const start = r.i;
  const fnType = r.u32();
  if (fnType !== 0) throw new Error('refusing a non-contract-function authorization');
  const fnStart = r.i;              // InvokeContractArgs begin right after the function-type word
  const contract = scAddress(r);
  const name = r.symbol();
  const nargs = r.u32();
  const args = [];
  for (let i = 0; i < nargs; i++) args.push(readScVal(r));
  const fnEnd = r.i;                // …and end before the sub-invocation vector
  const nsub = r.u32();
  const subs = [];
  for (let i = 0; i < nsub; i++) subs.push(readInvocation(r));
  const end = r.i;
  return {
    function: { contract: contract.strkey, name, args },
    subInvocations: subs,
    raw: r.b.subarray(start, end),
    functionRaw: r.b.subarray(fnStart, fnEnd),   // host-function-comparable bytes
  };
}

export function readEntry(b64) {
  const bytes = base64ToBytes(b64);
  const r = new Reader(bytes);
  const credType = r.u32();
  if (credType === CRED_SOURCE_ACCOUNT) throw new Error('source-account credentials: not a delegated signer entry');
  if (credType !== CRED_ADDRESS && credType !== CRED_ADDRESS_V2) {
    throw new Error(credType === 3 ? 'address-with-delegates credentials are not supported here'
      : 'unsupported credentials type ' + credType);
  }
  const address = scAddress(r);
  const nonce = r.i64();
  const expiry = r.u32();
  const signature = readScVal(r);
  const invocation = readInvocation(r);
  if (r.i !== bytes.length) throw new Error('trailing bytes in the authorization entry');
  return {
    credentialsType: credType === CRED_ADDRESS_V2 ? 'address_v2' : 'address',
    address: address.strkey,
    nonce,
    expiry,
    signature,
    invocation,
    invocationRaw: invocation.raw,
  };
}

// ---- preimage ---------------------------------------------------------------------------------
function writeU32(out, v) {
  out.push((v >>> 24) & 0xff, (v >>> 16) & 0xff, (v >>> 8) & 0xff, v & 0xff);
}
function writeI64(out, v) {
  const u = BigInt.asUintN(64, v);
  writeU32(out, Number(u >> 32n));
  writeU32(out, Number(u & 0xffffffffn));
}

/** Serialise the SCAddress of a contract/account strkey back to XDR (needed only for ADDRESS_V2). */
export function scAddressFromStrkey(text) {
  const raw = base32Decode(text);
  const payload = raw.subarray(1, 33);
  const out = [];
  writeU32(out, text.startsWith('G') ? 0 : 1);
  if (text.startsWith('G')) {
    writeU32(out, 0);
    out.push(...payload);
  } else {
    out.push(...payload);
  }
  return new Uint8Array(out);
}

/** HashIdPreimage bytes: exactly what the wallet signs (sha256 of this is `signature_payload`). */
export function preimageBytes(entry, networkId) {
  const out = [];
  const withAddress = entry.credentialsType === 'address_v2';
  writeU32(out, withAddress ? ENVELOPE_SOROBAN_AUTHORIZATION_WITH_ADDRESS : ENVELOPE_SOROBAN_AUTHORIZATION);
  out.push(...networkId);
  writeI64(out, entry.nonce);
  writeU32(out, entry.expiry);
  if (withAddress) out.push(...scAddressFromStrkey(entry.address));
  out.push(...entry.invocationRaw);
  return new Uint8Array(out);
}

// ---- base64 ------------------------------------------------------------------------------------
export function base64ToBytes(b64) {
  const clean = String(b64).replace(/\s+/g, '');
  if (!/^[A-Za-z0-9+/]+={0,2}$/.test(clean)) throw new Error('not base64');
  const padded = clean + '='.repeat((4 - (clean.length % 4)) % 4);
  let bin;
  try {
    bin = atob(padded);
  } catch {
    throw new Error('not base64');
  }
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export function bytesToBase64(bytes) {
  let bin = '';
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin);
}
