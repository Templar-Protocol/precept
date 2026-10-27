// TEST-ONLY SCVal encoder. The page never needs to write an SCVal (it re-uses the entry's original
// invocation bytes for the payload), but the differential test must be able to re-encode what the
// decoder produced and compare it with Python's bytes. Kept out of the page's trust path on purpose.
import * as xdr from '../xdr-lite.mjs';

const words = (n) => [(n >>> 24) & 0xff, (n >>> 16) & 0xff, (n >>> 8) & 0xff, n & 0xff];

function padded(bytes) {
  const out = [...words(bytes.length), ...bytes];
  while (out.length % 4) out.push(0);
  return out;
}

function u64Bytes(v) {
  const u = BigInt.asUintN(64, BigInt(v));
  return [...words(Number(u >> 32n)), ...words(Number(u & 0xffffffffn))];
}

function u128Bytes(v) {
  const u = BigInt.asUintN(128, BigInt(v));
  return [...u64Bytes(u >> 64n), ...u64Bytes(u & ((1n << 64n) - 1n))];
}

export function encodeScVal(v) {
  switch (v.type) {
    case 0: return [...words(0), ...words(v.value ? 1 : 0)];
    case 1: return [...words(1)];
    case 3: return [...words(3), ...words(v.value >>> 0)];
    case 4: return [...words(4), ...words(v.value >>> 0)];
    case 5: case 6: return [...words(v.type), ...u64Bytes(v.value)];
    case 9: case 10: return [...words(v.type), ...u128Bytes(v.value)];
    case 13: return [...words(13), ...padded([...v.value])];
    case 14: return [...words(14), ...padded([...new TextEncoder().encode(v.value)])];
    case 15: return [...words(15), ...padded([...new TextEncoder().encode(v.value)])];
    // vec/map arms are optional pointers: the presence word is ALWAYS 1 in what the tool produces,
    // even for an empty vector, followed by the element count.
    case 16:
      return [...words(16), ...words(1), ...words(v.value.length), ...v.value.flatMap(encodeScVal)];
    case 17:
      return [...words(17), ...words(1), ...words(v.value.length),
        ...v.value.flatMap(([k, val]) => [...encodeScVal(k), ...encodeScVal(val)])];
    case 18: {
      const sc = xdr.scAddressFromStrkey(v.value.strkey);
      return [...words(18), ...sc];
    }
    default: throw new Error('encoder does not implement SCVal type ' + v.type);
  }
}
