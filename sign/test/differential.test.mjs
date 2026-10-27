// Differential test: 120 randomised SCVal trees generated in Python must decode to the same bytes and
// the same words in the page. `scval-encode.mjs` is test-only: the page never writes an SCVal, but
// round-tripping proves the reader advances exactly like the Python encoder.
import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import * as xdr from '../xdr-lite.mjs';
import { encodeScVal } from './scval-encode.mjs';

const { seed, cases } = JSON.parse(readFileSync(new URL('./random_scvals.json', import.meta.url), 'utf8'));

describe(`differential SCVal corpus (seed ${seed})`, () => {
  for (const [i, c] of cases.entries()) {
    test(`case ${i}: ${c.describe.slice(0, 40)}`, () => {
      const bytes = xdr.base64ToBytes(c.xdr);
      const r = new xdr.ReaderForTest(bytes);
      const value = xdr.readScVal(r);
      expect(r.offset).toBe(bytes.length);                 // no trailing bytes
      expect(xdr.renderScVal(value)).toBe(c.describe);      // same words as the Python tool
      expect(xdr.hex(new Uint8Array(encodeScVal(value)))).toBe(xdr.hex(bytes)); // same bytes
    });
  }

  test('a truncated case is refused rather than half-decoded', () => {
    const bytes = xdr.base64ToBytes(cases[0].xdr);
    const r = new xdr.ReaderForTest(bytes.subarray(0, Math.max(4, bytes.length - 4)));
    expect(() => xdr.readScVal(r)).toThrow();
  });

  test('an unimplemented arm is refused, not skipped', () => {
    // SCV_CONTRACT_INSTANCE (19) is not implemented; a reader must refuse instead of guessing a length
    const bytes = xdr.base64ToBytes('AAAAEwAAAAA=');
    const r = new xdr.ReaderForTest(bytes);
    expect(() => xdr.readScVal(r)).toThrow(/unsupported SCVal type 19/);
  });
});
