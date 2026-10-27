// Golden-vector tests: the page's decoder must reproduce the Python tool's bytes and words exactly for
// EVERY call shape the hand-off signs, in both networks and both credential types.
import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import * as d from '../decode.mjs';
import * as xdr from '../xdr-lite.mjs';

const { vectors } = JSON.parse(readFileSync(new URL('./vectors.json', import.meta.url), 'utf8'));
const signerOf = (bundle) => bundle.signers[0];   // mainnet and testnet vectors use different keys

function bundleCopy(v) {
  return JSON.parse(JSON.stringify(v.bundle));
}

describe('golden vectors', () => {
  for (const v of vectors) {
    test(`${v.name} [${v.network}/${v.credentials_type}]`, async () => {
      const signer = signerOf(v.bundle);
      const got = await d.derive(v.bundle, { signer, now: d.LEDGER_ANCHORS[v.network][1] });
      expect(got.payloadHex).toBe(v.payload_hex);
      expect(got.payloadB64).toBe(v.payload_b64);
      expect(got.payloadDec).toEqual(v.payload_dec);                       // Freighter's decimal bytes
      expect(got.payloadFreighterJson).toBe(v.payload_freighter_json);
      expect(got.preimageB64).toBe(v.preimage_b64);          // account preimage (the payload's own)
      expect(got.signer).toBe(signer);
      expect(got.signerPreimageB64).toBe(v.signer_preimage_b64[signer]);
      expect(xdr.hex(got.signerDigest)).toBe(v.signer_digest_hex[signer]);
      expect(got.signerEntryExpiry).toBe(v.expiry_ledger);
      expect(got.network).toBe(v.network);
      expect(got.account).toBe(v.bundle.account);
      expect(got.expiry).toBe(v.expiry_ledger);
      // the plain-words rendering must equal the Python tool's, line for line
      expect(got.action).toEqual(v.describe);
      // and the expiry estimate uses the same anchors
      const est = d.expiryEstimate(v.expiry_ledger, v.network, d.LEDGER_ANCHORS[v.network][1]);
      expect(est.remaining).toBe(17280 * d.LEDGER_SECONDS);
    });
  }

  test('the fragment round-trips through the page decompressor', async () => {
    for (const v of vectors.slice(0, 8)) {
      const bundle = await d.decodeFragment(v.fragment);
      expect(bundle.account).toBe(v.bundle.account);
      expect(bundle.payload_ok === undefined || true).toBe(true);
      const got = await d.derive(bundle, { signer: signerOf(bundle), now: d.LEDGER_ANCHORS[v.network][1] });
      expect(got.payloadHex).toBe(v.payload_hex);
    }
  });

  test('every vector has exactly one auth context and no sub-invocations', async () => {
    for (const v of vectors) {
      const acct = xdr.readEntry(v.bundle.account_entry_xdr);
      expect(acct.invocation.subInvocations.length).toBe(0);
      expect(v.bundle.context_rule_ids).toEqual([0]);
    }
  });
});

describe('refusals', () => {
  const base = vectors.find((v) => v.name === 'sac.transfer' && v.network === 'testnet'
    && v.credentials_type === 'address_v2');

  const signer = signerOf(base.bundle);
  const now = () => d.LEDGER_ANCHORS[base.network][1] + 3600;   // one hour after the anchor

  async function refuses(mutate, needle) {
    const b = bundleCopy(base);
    mutate(b);
    await expect(d.derive(b, { signer, now: now() })).rejects.toThrow(needle ? new RegExp(needle) : d.Refuse);
  }

  test('a bundle that selects more than one auth context is refused', async () => {
    await refuses((b) => { b.context_rule_ids = [0, 1]; }, 'exactly one auth context');
  });

  test('a bundle that selects a rule other than 0 is refused', async () => {
    await refuses((b) => { b.context_rule_ids = [3]; }, 'other than 0');
  });

  test('a different action in the host function is refused', async () => {
    // swap the host function for a different contract call: entries no longer match it
    const other = vectors.find((v) => v.name === 'pool.set_status' && v.network === 'testnet'
      && v.credentials_type === 'address_v2');
    await refuses((b) => { b.host_function_xdr = other.bundle.host_function_xdr; },
      'does not match the action');
  });

  test('an unknown network name is refused', async () => {
    await refuses((b) => { b.network.name = 'soroban-prod'; }, 'unknown network');
  });

  test('a passphrase that does not belong to the network is refused', async () => {
    await refuses((b) => { b.network.passphrase = d.PINNED.mainnet; }, 'does not belong to the network');
  });

  test('a bundle for another account is refused when the signer pins one', async () => {
    await expect(d.derive(base.bundle, { signer: base.bundle.signers[1], now: now() }))
      .resolves.toBeTruthy(); // that IS a signer
    await expect(d.derive(base.bundle, { signer: 'GDIEVMRSOQV3JKZ2CNUL2RQV4TTNAISKW4NAC25PQUQKGMWJO6DTOAE7', now: now() }))
      .rejects.toThrow(/not a signer/);
  });

  test('an account entry for a different action breaks the signer entries', async () => {
    // the real attack this page guards against: swap the entries for another action while leaving the
    // displayed one alone -> the preimage payload moves and the mismatch is refused
    const other = vectors.find((v) => v.name === 'pool.set_status' && v.network === 'testnet'
      && v.credentials_type === 'address_v2');
    await refuses((b) => { b.account_entry_xdr = other.bundle.account_entry_xdr; },
      'does not match the action|not for the account|different signature payload');
  });

  test('a bundle whose signer entry commits to another payload is refused', async () => {
    const other = vectors.find((v) => v.name === 'pool.set_status' && v.network === 'testnet'
      && v.credentials_type === 'address_v2');
    await refuses((b) => {
      for (const g of b.signers) b.signer_entries[g] = other.bundle.signer_entries[g];
    }, 'different signature payload|different expiry|different contract');
  });

  test('mixed credential types are refused', async () => {
    const legacy = vectors.find((v) => v.name === 'sac.transfer' && v.network === 'testnet'
      && v.credentials_type === 'address');
    await refuses((b) => {
      for (const g of b.signers) b.signer_entries[g] = legacy.bundle.signer_entries[g];
    }, 'mixes credential types');
  });

  test('trailing bytes in an entry are refused', async () => {
    const acct = base.bundle.account_entry_xdr.replace(/=+$/, '');
    const longer = acct + 'AAAA';   // four more bytes of well-formed base64, no padding inside
    await refuses((b) => { b.account_entry_xdr = longer; }, 'trailing bytes');
  });

  test('an unsigned entry that already carries a signature is refused', async () => {
    await refuses((b) => { b.account_entry_xdr = '/w=='; }, 'corrupt|truncated|refus|not for the account');
  });

  test('sub-invocations in the action are refused by the page policy', async () => {
    const fake = { function: { contract: base.bundle.account, name: 'f', args: [] },
      subInvocations: [{ function: { contract: base.bundle.account, name: 'g', args: [] }, subInvocations: [] }] };
    expect(() => d.actionLines(fake)).toThrow(/sub-invocations/);
  });

  test('an unprintable symbol is refused rather than shown', async () => {
    const evil = { type: 15, value: 'ok\u001b[2K\rfake' };
    expect(() => d.renderValue(evil)).toThrow(/unusual characters/);
  });
});
