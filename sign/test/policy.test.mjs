// Page policy that the review asked for: pinned accounts, expiry limits, signer selection, extension
// address equality, WalletConnect storage keys, the decimal payload form Freighter shows, and a real
// "entry already carries a signature" case.
import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import * as d from '../decode.mjs';
import * as xdr from '../xdr-lite.mjs';

const { vectors } = JSON.parse(readFileSync(new URL('./vectors.json', import.meta.url), 'utf8'));
const testnetVec = vectors.find((v) => v.network === 'testnet' && v.credentials_type === 'address_v2');
const mainnetVec = vectors.find((v) => v.network === 'mainnet' && v.credentials_type === 'address_v2');
const atAnchor = (v) => d.LEDGER_ANCHORS[v.network][1];
const copy = (v) => JSON.parse(JSON.stringify(v.bundle));

describe('pinned accounts', () => {
  test('a known testnet account is accepted', async () => {
    const got = await d.derive(testnetVec.bundle, { now: atAnchor(testnetVec) });
    expect(d.KNOWN_ACCOUNTS.testnet).toContain(got.account);
  });

  test('an unknown account is refused with a message naming the pinned list', async () => {
    const b = copy(testnetVec);
    b.account = 'CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4';
    await expect(d.derive(b, { now: atAnchor(testnetVec) })).rejects
      .toThrow(/only signs for the known admin accounts/);
  });

  test('the mainnet account is pinned for mainnet and not for testnet', async () => {
    expect(d.KNOWN_ACCOUNTS.mainnet).toEqual(['CCDZKWLV6JWM4V3WPMJHXTSLWG5IDQ5S3K7BI3AH7MBCRQCPCT3EJ2AI']);
    expect(d.KNOWN_ACCOUNTS.testnet).not.toContain(d.KNOWN_ACCOUNTS.mainnet[0]);
  });
});

describe('expiry policy', () => {
  test('an expired bundle is refused', async () => {
    await expect(d.derive(testnetVec.bundle, { now: atAnchor(testnetVec) + 17280 * 5 + 86400 }))
      .rejects.toThrow(/already expired/);
  });

  test('a bundle valid for more than two days is refused', async () => {
    const validFor = (testnetVec.expiry_ledger - d.LEDGER_ANCHORS.testnet[0]) * d.LEDGER_SECONDS;
    const now = atAnchor(testnetVec) + validFor - (2 * 86400 + 60);   // leaves 2 days + 1 minute
    await expect(d.derive(testnetVec.bundle, { now })).rejects.toThrow(/longer than the 2-day limit/);
  });

  test('the boundary is accepted', async () => {
    // exactly two days of validity: remaining == 2 days is allowed
    const got = await d.derive(testnetVec.bundle, { now: atAnchor(testnetVec) + 17280 * 5 - 2 * 86400 });
    expect(got.expiryRemaining).toBeCloseTo(2 * 86400, 0);
  });

  test('the CLI and the page agree on the limit', () => {
    expect(d.MAX_DAYS).toBe(2);
  });
});

describe('signer selection', () => {
  test('an unknown &s= is refused, never silently swapped', () => {
    expect(() => d.chooseSigner(['G1', 'G2'], 'G3')).toThrow(/not a signer of this bundle/);
  });

  test('a missing &s= picks the first signer', () => {
    expect(d.chooseSigner(['G1', 'G2'])).toBe('G1');
    expect(d.chooseSigner(['G1', 'G2'], '')).toBe('G1');
  });

  test('a listed signer is honoured', () => {
    expect(d.chooseSigner(['G1', 'G2'], 'G2')).toBe('G2');
  });
});

describe('extension address equality', () => {
  test('a different active account is refused', () => {
    expect(() => d.requireSameAddress('GBJFHU35WHKY24GGBY6R4PTEHLMURSQEVTP2TUX5O45XYGG5DI6YS2QV',
      'GAY47MJUC7TKS76Q2X2HUNBIOHX6WO6LCFA2A424KQV723EU5TDQG44M'))
      .toThrow(/Freighter is set to/);
  });

  test('no address is refused rather than guessed', () => {
    expect(() => d.requireSameAddress('', 'G')).toThrow(/did not say which account/);
    expect(() => d.requireSameAddress(undefined, 'G')).toThrow(/did not say which account/);
  });

  test('the same address passes', () => {
    const g = 'GAY47MJUC7TKS76Q2X2HUNBIOHX6WO6LCFA2A424KQV723EU5TDQG44M';
    expect(d.requireSameAddress(g, g)).toBe(g);
  });
});

describe('WalletConnect storage', () => {
  test('only WalletConnect keys are selected for clearing', () => {
    const keys = ['wc@2:core:0.3//keychain', 'WALLETCONNECT_DEEPLINK_CHOICE', '@walletconnect/session',
      'unrelated', 'theme', 'wc_foo'];
    expect(d.wcStorageKeys(keys)).toEqual(
      ['wc@2:core:0.3//keychain', 'WALLETCONNECT_DEEPLINK_CHOICE', '@walletconnect/session', 'wc_foo']);
  });

  test('an empty store is harmless', () => {
    expect(d.wcStorageKeys([])).toEqual([]);
    expect(d.wcStorageKeys(undefined)).toEqual([]);
  });
});

describe('the payload form Freighter shows', () => {
  test('decimal bytes in order, and the exact JSON the released apps render', async () => {
    const got = await d.derive(testnetVec.bundle, { now: atAnchor(testnetVec) });
    expect(got.payloadDec).toHaveLength(32);
    expect(got.payloadDec).toEqual(Array.from(Buffer.from(got.payloadHex, 'hex')));
    const parsed = JSON.parse(got.payloadFreighterJson);
    expect(Object.keys(parsed)).toHaveLength(32);
    expect(Object.keys(parsed)[0]).toBe('0');
    expect(Object.keys(parsed)[31]).toBe('31');
    expect(Object.values(parsed)).toEqual(got.payloadDec);
    expect(got.payloadFreighterJson).toBe(testnetVec.payload_freighter_json);
  });
});

describe('function names and strings', () => {
  test('a function name outside the identifier charset is refused', () => {
    const evil = { function: { contract: 'C…', name: 'tr\u202ansfer', args: [] }, subInvocations: [] };
    expect(() => d.actionLines(evil)).toThrow(/unusual characters/);
  });
});

describe('an entry that already carries a signature', () => {
  // The probe is generated in Python: the same account entry with a real 64-byte signature slot.
  const { signed_probe: probe } = JSON.parse(
    readFileSync(new URL('./vectors.json', import.meta.url), 'utf8'));

  test('the signature arm is parsed, not skipped', () => {
    const acct = xdr.readEntry(probe.account_entry_xdr);
    expect(acct.signature.type).not.toBe(1);            // not SCV_VOID
    expect(acct.expiry).toBe(probe.expiry_ledger);
    expect(acct.address).toBe(probe.account);
  });

  test('a bundle carrying it is refused', async () => {
    const b = copy(mainnetVec);
    b.account_entry_xdr = probe.account_entry_xdr;
    await expect(d.derive(b, { now: atAnchor(mainnetVec) }))
      .rejects.toThrow(/already carries a signature/);
  });
});
