// Byte parity of everything the emergency console builds (bundles, signed auth, envelopes) with the
// Python tool, plus the policy it adds to derive() and the carried-signature check.
import { describe, expect, test } from 'bun:test';
import { readFileSync } from 'node:fs';
import * as b from '../build.mjs';
import * as d from '../decode.mjs';
import * as xdr from '../xdr-lite.mjs';
import { EMERGENCY } from '../actions.mjs';
import { checkSignedEnvelope, explain } from '../rpc.mjs';
import * as ed from '../vendor/noble-ed25519-3.2.0.mjs';

ed.hashes.sha512Async = async (m) => new Uint8Array(await crypto.subtle.digest('SHA-512', m));

const { builder } = JSON.parse(readFileSync(new URL('./vectors.json', import.meta.url), 'utf8'));
const byName = Object.fromEntries(builder.cases.map((c) => [c.name, c]));
const b64 = xdr.bytesToBase64;
const unb64 = xdr.base64ToBytes;
const sig = (k) => new Uint8Array(64).fill(k + 1);
const GOV = EMERGENCY.mainnet.governance;
const ME = 'GBJFHU35WHKY24GGBY6R4PTEHLMURSQEVTP2TUX5O45XYGG5DI6YS2QV';

const bundleCases = builder.cases.filter((c) => c.bundle);
const callFor = (c) => ({
  'solo.on_ice': b.onIceCall(c.inputs.pool),
  'pair.active': b.activeCall(c.inputs.pool),
  'pair.on_ice_rule0': b.onIceCall(c.inputs.pool),
})[c.name];

describe('builder vectors: bundles and signed auth', () => {
  for (const c of bundleCases) {
    test(`${c.name}: buildBundle equals the Python bundle`, async () => {
      const i = c.inputs;
      const got = await b.buildBundle({
        network: 'mainnet', account: i.account, call: callFor(c), signers: i.signers, threshold: i.threshold,
        ruleId: i.rule_id, latestLedger: i.latest, expiryLedgers: i.expiry_ledgers,
        nonces: i.nonces.map(BigInt),
      });
      expect(got).toEqual(c.bundle);
    });

    test(`${c.name}: signedAuth equals the Python entries`, () => {
      const sigs = Object.fromEntries(c.inputs.signers.slice(0, c.inputs.threshold).map((g, k) => [g, sig(k)]));
      const [acct, ...signers] = b.signedAuth(c.bundle, sigs);
      expect(b64(acct)).toBe(c.signed_account_entry_xdr);
      expect(signers.map(b64)).toEqual(
        c.inputs.signers.slice(0, c.inputs.threshold).map((g) => c.signed_signer_entry_xdr[g]));
    });

    test(`${c.name}: envelopes equal the Python envelopes`, () => {
      const host = unb64(c.bundle.host_function_xdr);
      const sigs = Object.fromEntries(c.inputs.signers.slice(0, c.inputs.threshold).map((g, k) => [g, sig(k)]));
      const auth = b.signedAuth(c.bundle, sigs);
      const base = { source: c.tx.source, seq: BigInt(c.tx.seq) + 0n, minTime: c.tx.min_time, maxTime: c.tx.max_time,
        hostFunction: host, auth };
      expect(b64(xdr.envelopeBytes(xdr.transactionBytes({ ...base, fee: 10000 })))).toBe(c.tx.envelope_sim_b64);
      expect(b64(xdr.envelopeBytes(xdr.transactionBytes({
        ...base, fee: 10000 + 1234567, sorobanData: unb64(c.tx.soroban_data_b64) })))).toBe(c.tx.envelope_final_b64);
    });

    test(`${c.name}: the fragment round-trips`, async () => {
      expect(await d.decodeFragment('#b=' + await b.encodeFragment(c.bundle))).toEqual(c.bundle);
    });
  }

  test('signedAuth refuses below the threshold', () => {
    const c = byName['pair.active'];
    expect(() => b.signedAuth(c.bundle, { [c.inputs.signers[0]]: sig(0) })).toThrow(/need 2 of 3/);
  });
});

describe('builder vectors: source-account cases', () => {
  const calls = {
    'solo.trip_create': () => b.tripCreateCall(GOV, ME, 14, EMERGENCY.mainnet.assets[0].id, true),
    'solo.untrip_create': () => b.tripCreateCall(GOV, ME, 14, EMERGENCY.mainnet.assets[0].id, false),
    'solo.execute': () => b.executeCall(GOV, ME, 14),
  };
  for (const [name, mk] of Object.entries(calls)) {
    test(`${name}: entry and envelopes equal the Python bytes`, () => {
      const c = byName[name];
      const call = mk();
      const invoke = xdr.invokeArgsBytes(call.contract, call.fn, call.args);
      const entry = xdr.sourceAccountEntryBytes(xdr.rootInvocationBytes(invoke));
      expect(b64(entry)).toBe(c.auth_entry_xdr);
      const base = { source: ME, seq: BigInt(c.tx.seq), maxTime: c.tx.max_time,
        hostFunction: xdr.hostFunctionBytes(invoke), auth: [entry] };
      expect(b64(xdr.envelopeBytes(xdr.transactionBytes({ ...base, fee: 10000 })))).toBe(c.tx.envelope_sim_b64);
      expect(b64(xdr.envelopeBytes(xdr.transactionBytes({
        ...base, fee: 10000 + 1234567, sorobanData: unb64(c.tx.soroban_data_b64) })))).toBe(c.tx.envelope_final_b64);
    });
  }

  test('the account sequence is read out of an ACCOUNT ledger entry', () => {
    expect(xdr.accountSeqFromEntryData(builder.account_entry_data_b64)).toBe(123456789012n);
  });

  test('a non-ACCOUNT ledger entry is refused', () => {
    const bytes = unb64(builder.account_entry_data_b64);
    bytes[3] = 1;
    expect(() => xdr.accountSeqFromEntryData(b64(bytes))).toThrow(/not an account/);
  });

  test('withSignature refuses an entry that is already signed', () => {
    const c = byName['solo.on_ice'];
    expect(() => xdr.withSignature(unb64(c.signed_account_entry_xdr), new Uint8Array([0, 0, 0, 1])))
      .toThrow(/already carries/);
  });
});

describe('derive: pinned pause rules', () => {
  const base = byName['solo.on_ice'].bundle;
  const now = d.LEDGER_ANCHORS.mainnet[1] + 1000 * 5;
  const copy = () => JSON.parse(JSON.stringify(base));
  const rebuild = async (call, ruleId, ledgers = 720) => b.buildBundle({
    network: 'mainnet', account: EMERGENCY.mainnet.account, call, signers: [ME], threshold: 1, ruleId,
    latestLedger: d.LEDGER_ANCHORS.mainnet[0] + 1000, expiryLedgers: ledgers,
  });

  test('set_status(2) on pause rule 1 is accepted', async () => {
    const got = await d.derive(copy(), { signer: ME, now });
    expect(got.payloadHex).toMatch(/^[0-9a-f]{64}$/);
  });

  test('set_status(0) through rule 1 is refused', async () => {
    const bad = await rebuild(b.activeCall(EMERGENCY.mainnet.pools[0].id), 1);
    await expect(d.derive(bad, { signer: ME, now })).rejects.toThrow(/only signs rule 0, or a pinned pause rule/);
  });

  test("rule 1 aimed at the other pool's address is refused", async () => {
    const bad = await rebuild(b.onIceCall(EMERGENCY.mainnet.pools[1].id), 1);
    await expect(d.derive(bad, { signer: ME, now })).rejects.toThrow(/only signs rule 0/);
  });

  test('an unpinned rule id is refused', async () => {
    const bad = await rebuild(b.onIceCall(EMERGENCY.mainnet.pools[0].id), 7);
    await expect(d.derive(bad, { signer: ME, now })).rejects.toThrow(/only signs rule 0/);
  });

  test('rule 0 still accepts any call (the existing hand-off path)', async () => {
    const pair = byName['pair.on_ice_rule0'].bundle;
    const got = await d.derive(pair, { signer: ME, now: now + 30000 * 5 });
    expect(got.signers).toHaveLength(3);
  });
});

describe('verifyCarriedShare', () => {
  const GA = 'GAY47MJUC7TKS76Q2X2HUNBIOHX6WO6LCFA2A424KQV723EU5TDQG44M';
  const now = d.LEDGER_ANCHORS.mainnet[1] + 1000 * 5 + 3600;
  const seed = new Uint8Array(32).fill(7);
  const strkeyOf = (pub) => xdr.encodeAccount(pub);

  // the carried share is signed by a test key that is spliced in as the bundle's first signer
  async function setup() {
    const pub = await ed.getPublicKeyAsync(seed);
    const g = strkeyOf(pub);
    const c = byName['pair.active'].inputs;
    const bundle = await b.buildBundle({
      network: 'mainnet', account: c.account, call: b.activeCall(c.pool), signers: [g, ME, GA], threshold: 2,
      ruleId: 0, latestLedger: d.LEDGER_ANCHORS.mainnet[0] + 1000, expiryLedgers: 31000,
    });
    const mine = await d.derive(bundle, { signer: g, now });
    const signature = await ed.signAsync(mine.signerDigest, seed);
    const code = d.encodeShare({
      network: mine.network, account: mine.account, signer: g, expiry: mine.expiry,
      payloadHex: mine.payloadHex, signatureB64: b64(signature),
    });
    const other = await d.derive(bundle, { signer: ME, now });
    return { bundle, g, mine, other, code, signature };
  }
  const tweak = (code, f) => {
    const body = JSON.parse(new TextDecoder().decode(d.base64urlToBytes(code.slice(d.SHARE_PREFIX.length))));
    f(body);
    return d.SHARE_PREFIX + d.bytesToBase64url(new TextEncoder().encode(JSON.stringify(body)));
  };

  test('accepts a share signed by another signer of the bundle', async () => {
    const s = await setup();
    const share = await d.verifyCarriedShare(s.bundle, s.other, s.code, ed, now);
    expect(share.signer).toBe(s.g);
  });

  test('refuses a flipped signature byte', async () => {
    const s = await setup();
    const flipped = new Uint8Array(s.signature);
    flipped[0] ^= 1;
    const code = tweak(s.code, (x) => { x.sig = b64(flipped); });
    await expect(d.verifyCarriedShare(s.bundle, s.other, code, ed, now)).rejects.toThrow(/does not verify/);
  });

  test('refuses another payload, expiry, network and account', async () => {
    const s = await setup();
    for (const [f, needle] of [
      [(x) => { x.payload = '0'.repeat(64); }, /different signature payload/],
      [(x) => { x.expiry += 1; }, /different expiry/],
      [(x) => { x.network = 'testnet'; }, /another network/],
      [(x) => { x.account = EMERGENCY.mainnet.pools[0].id; }, /another account/],
    ]) {
      await expect(d.verifyCarriedShare(s.bundle, s.other, tweak(s.code, f), ed, now)).rejects.toThrow(needle);
    }
  });

  test('refuses a share from the account that is signing now', async () => {
    const s = await setup();
    await expect(d.verifyCarriedShare(s.bundle, s.mine, s.code, ed, now)).rejects.toThrow(/already carries a signature from/);
  });

  test('refuses a share from someone who is not a signer of the bundle', async () => {
    const s = await setup();
    const nb = { ...s.bundle, signers: [ME, GA] };
    const other = await d.derive(nb, { signer: ME, now });
    await expect(d.verifyCarriedShare(nb, other, s.code, ed, now)).rejects.toThrow(/not a signer of this bundle/);
  });
});

describe('transaction submit guards', () => {
  const tx = xdr.transactionBytes({
    source: ME, seq: 1n, maxTime: 1, hostFunction: new Uint8Array([0, 0, 0, 0]), auth: [], fee: 10000 });
  const plain = xdr.envelopeBytes(tx);
  const signed = xdr.envelopeBytes(tx, [{ hint: new Uint8Array(4), sig: new Uint8Array(64) }]);

  test('a signed envelope that only adds a signature passes', () => {
    expect(() => checkSignedEnvelope(plain, signed)).not.toThrow();
  });

  test('a signed envelope with a changed fee is refused', () => {
    const tampered = new Uint8Array(signed);
    tampered[4 + 4 + 32 + 3] ^= 0x01;     // the fee word follows type(4), key type(4), key(32)
    expect(() => checkSignedEnvelope(plain, tampered)).toThrow(/wallet changed the transaction/);
  });

  test('an envelope with no signature is refused', () => {
    expect(() => checkSignedEnvelope(plain, plain)).toThrow(/wallet changed the transaction/);
  });

  test('explain maps a contract error code to its hint', () => {
    const text = 'HostError: Error(Contract, #1204)\n  ...';
    expect(explain(text, { '#1204': 'backstop under threshold' })).toStartWith('backstop under threshold\n\nnode said: ');
    expect(explain(text, {})).toContain('node said: ');
  });
});
