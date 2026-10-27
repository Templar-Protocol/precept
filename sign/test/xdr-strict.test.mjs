// Strictness of the XDR reader and the renderer, from hand-crafted (non-canonical) bytes. The golden
// vectors prove canonical encodings pass; these prove everything else is refused rather than guessed.
import { describe, expect, test } from 'bun:test';
import * as xdr from '../xdr-lite.mjs';

const hexToBytes = (h) => new Uint8Array(
  h.replace(/\s+/g, '').match(/../g).map((b) => parseInt(b, 16)));
const readAll = (h) => {
  const r = new xdr.ReaderForTest(hexToBytes(h));
  const v = xdr.readScVal(r);
  return { v, offset: r.offset, total: hexToBytes(h).length };
};

describe('refuses non-canonical encodings', () => {
  test('vec presence word other than 1', () => {
    expect(() => readAll('00000010 00000000')).toThrow(/vec presence word 0/);
    expect(() => readAll('00000010 00000002')).toThrow(/vec presence word 2/);
  });

  test('map presence word other than 1', () => {
    expect(() => readAll('00000011 00000000')).toThrow(/map presence word 0/);
  });

  test('bool values other than 0/1', () => {
    expect(() => readAll('00000000 00000002')).toThrow(/invalid XDR bool/);
    expect(() => readAll('00000000 ffffffff')).toThrow(/invalid XDR bool/);
    expect(readAll('00000000 00000001').v.value).toBe(true);
    expect(readAll('00000000 00000000').v.value).toBe(false);
  });

  test('non-zero padding after a symbol', () => {
    // SCV_SYMBOL, length 2, "ab", then padding 00 01 instead of 00 00
    expect(() => readAll('0000000f 00000002 6162 0001')).toThrow(/non-zero XDR padding/);
  });

  test('non-zero padding after a string', () => {
    // length 5 needs three padding bytes; make the last of them non-zero
    expect(() => readAll('0000000e 00000005 6162636465 000001')).toThrow(/non-zero XDR padding/);
    expect(readAll('0000000e 00000005 6162636465 000000').v.value).toBe('abcde');
  });

  test('non-zero padding after bytes', () => {
    expect(() => readAll('0000000d 00000001 ff 00ff00')).toThrow(/non-zero XDR padding/);
    expect(readAll('0000000d 00000001 ff 000000').v.value[0]).toBe(0xff);
  });

  test('truncated input', () => {
    expect(() => readAll('0000000d 00000010 00')).toThrow(/truncated/);
    expect(() => readAll('00000011 00000001 00000001')).toThrow(/truncated/);
  });

  test('an unimplemented arm is refused, not skipped', () => {
    expect(() => readAll('00000013 00000000')).toThrow(/unsupported SCVal type 19/);
  });
});

describe('strings', () => {
  const strBytes = (utf8) => {
    const bytes = new TextEncoder().encode(utf8);
    const len = bytes.length.toString(16).padStart(8, '0');
    const pad = '00'.repeat((4 - (bytes.length % 4)) % 4);
    return '0000000e' + len + [...bytes].map((b) => b.toString(16).padStart(2, '0')).join('') + pad;
  };

  test('bidi, zero-width and control characters are escaped, never printed raw', () => {
    const { v } = readAll(strBytes('a\u202eb\u200bc\u0007d\u2028e'));
    const out = xdr.renderScVal(v);
    for (const raw of ['\u202e', '\u200b', '\u0007', '\u2028']) expect(out).not.toContain(raw);
    expect(out).toContain('\\u202e');
    expect(out).toContain('\\u200b');
    expect(out).toContain('\\u0007');
    expect(out).toContain('\\u2028');
  });

  test('the tag-character range and soft hyphens are escaped too', () => {
    const { v } = readAll(strBytes('x\u{E0041}y\u00adz'));
    const out = xdr.renderScVal(v);
    expect(out).not.toContain('\u{E0041}');
    expect(out).not.toContain('\u00ad');
    expect(out).toContain('\\u{e0041}');
    expect(out).toContain('\\u00ad');
  });

  test('ordinary text is quoted unchanged', () => {
    expect(xdr.renderScVal(readAll(strBytes('transfer 12.5 XLM')).v)).toBe('"transfer 12.5 XLM"');
    expect(xdr.renderScVal(readAll(strBytes('é')).v)).toBe('"é"');
  });

  test('invalid UTF-8 is shown as hex, not as replacement characters', () => {
    // length 2, bytes ff fe, padding 00 00
    const { v } = readAll('0000000e 00000002 fffe 0000');
    expect(v.valid).toBe(false);
    expect(xdr.renderScVal(v)).toBe('bytes(0xfffe)');
    expect(v.raw).toEqual(new Uint8Array([0xff, 0xfe]));
  });

  test('a symbol with invalid UTF-8 is refused outright', () => {
    expect(() => readAll('0000000f 00000002 fffe 0000')).toThrow(/invalid UTF-8/);
  });

  test('quoteString escapes the same set the Python side does', () => {
    expect(xdr.quoteString('a"b\\c\u0000')).toBe('"a\\"b\\\\c\\u0000"');
    expect(xdr.quoteString('\u009f')).toBe('"\\u009f"');
    expect(xdr.quoteString('\ufeff')).toBe('"\\ufeff"');
  });
});
