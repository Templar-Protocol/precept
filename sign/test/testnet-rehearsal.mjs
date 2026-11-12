// MANUAL driver, not part of `bun test` (no `.test.` in the name; Pages does not publish test/).
// Runs the console's real flows (flows.mjs) against the permanent testnet rehearsal stack with key-backed
// wallets, and prints PASS per step:
//
//   RH_A=$(stellar keys show emerg-rh-a) RH_B=$(stellar keys show emerg-rh-b) bun sign/test/testnet-rehearsal.mjs
//
// The key wallet signs what Freighter would: auth entries as ed25519 over sha256(preimage), envelopes over
// sha256(networkId || u32 2 || tx) with a hint signature.
import { EMERGENCY } from '../actions.mjs';
import * as build from '../build.mjs';
import { PINNED, Refuse, decodeFragment, derive, decodeShare, sha256, verifyCarriedShare, verifySignature } from '../decode.mjs';
import * as flows from '../flows.mjs';
import * as rpc from '../rpc.mjs';
import * as xdr from '../xdr-lite.mjs';
import * as ed from '../vendor/noble-ed25519-3.2.0.mjs';

ed.hashes.sha512Async = async (m) => new Uint8Array(await crypto.subtle.digest('SHA-512', m));

const NET = 'testnet';
const pin = EMERGENCY[NET];
if (!pin) throw new Error('EMERGENCY.testnet is not configured');
const BASE_URL = 'https://example.invalid/precept/sign/';
let failures = 0;

const check = (name, ok, detail = '') => {
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? ` — ${detail}` : ''}`);
  if (!ok) failures += 1;
};

/** `S…` secret strkey -> 32-byte seed (version byte and checksum are not re-checked: the CLI printed it). */
const seedOf = (secret) => xdr.base32Decode(secret).subarray(1, 33);

async function keyWallet(seed) {
  const pub = await ed.getPublicKeyAsync(seed);
  const address = xdr.encodeAccount(pub);
  const networkId = await sha256(new TextEncoder().encode(PINNED[NET]));
  return {
    kind: 'key',
    address,
    async signAuthEntry(preimageB64) {
      const digest = await sha256(xdr.base64ToBytes(preimageB64));
      return xdr.bytesToBase64(await ed.signAsync(digest, seed));
    },
    async signTransaction(envelopeB64) {
      const env = xdr.base64ToBytes(envelopeB64);
      const tx = env.subarray(4, env.length - 4);          // type word … signature count
      const payload = new Uint8Array([...networkId, 0, 0, 0, 2, ...tx]);
      const sig = await ed.signAsync(await sha256(payload), seed);
      return xdr.bytesToBase64(xdr.envelopeBytes(tx, [{ hint: pub.subarray(28), sig }]));
    },
    async close() {},
  };
}

const ui = { status: (m) => console.log(`      ${m}`), confirm: async () => true };
const refusal = async (promise) => { try { await promise; return null; } catch (e) { return e; } };

const A = await keyWallet(seedOf(process.env.RH_A));
const B = await keyWallet(seedOf(process.env.RH_B));
const pool = pin.pools[0];
const usdc = pin.assets.find((a) => a.name === 'USDC') ?? pin.assets[0];
console.log(`rehearsal on ${NET}: account ${pin.account}\n  pool ${pool.name} ${pool.id} (pause rule ${pool.pauseRule}); signers rh-a ${A.address}, rh-b ${B.address}`);

// (a) practice On-Ice by rh-a: simulation OK, status unchanged
const before = await rpc.poolStatus(NET, pool.id);
const practice = await flows.soloOnIce({ network: NET, pool, me: A.address, wallet: A, ed, ui, practice: true });
check('a. practice On-Ice simulates and sends nothing',
  practice.practice === true && (await rpc.poolStatus(NET, pool.id)) === before, `status ${before}`);

// (b) solo On-Ice by rh-a: status 2
if (before === 2 || before === 4 || before === 6) {
  console.log(`      pool status is ${before}; bringing it back to Active first is outside this step`);
}
const solo = await flows.soloOnIce({ network: NET, pool, me: A.address, wallet: A, ed, ui });
check('b. solo On-Ice by rh-a puts the pool On-Ice', solo.status === 2, `tx ${solo.hash}`);

// (c) two-signer Back to Active: rh-a builds + signs, rh-b checks the carried share, signs and submits
const built = await flows.twoSignerBuild({
  network: NET, call: build.activeCall(pool.id), title: `Back to Active ${pool.name}`, me: A.address, wallet: A,
  ed, ui, baseUrl: BASE_URL,
});
const [, linkForB] = built.links.find(([g]) => g === B.address);
const params = new URLSearchParams(linkForB.slice(linkForB.indexOf('#') + 1));
const bundle = await decodeFragment(linkForB);
const derivedB = await derive(bundle, { signer: params.get('s') });
const carried = await verifyCarriedShare(bundle, derivedB, params.get('p'), ed);
const mine = await derive(bundle, { signer: B.address });
const sigB = await verifySignature(B.address, mine.signerDigest,
  await B.signAuthEntry(mine.signerPreimageB64), ed);
const submitted = await rpc.submitBundle({
  network: NET, bundle, submitter: B.address, wallet: B, hints: flows.PAIR_HINTS, onStatus: ui.status,
  sigsByG: { [carried.signer]: xdr.base64ToBytes(carried.sig), [B.address]: sigB },
});
const afterPair = await rpc.poolStatus(NET, pool.id);
check('c. two-signer Back to Active (build+sign by rh-a, check+sign+submit by rh-b)',
  decodeShare(built.share).signer === A.address && afterPair === 0, `tx ${submitted.hash}, status ${afterPair}`);
check('c2. the hand-off message names the other signers and the payload',
  built.message.includes(`expected signature_payload: ${built.derived.payloadHex}`)
  && built.message.includes(`${B.address}: ${BASE_URL}#b=`));

// (d) trip USDC by rh-a, (e) untrip by rh-b
const tripped = await flows.tripFlow({ network: NET, asset: usdc, tripped: true, me: A.address, wallet: A, ui });
check('d. trip by rh-a', tripped.tripped === true, `proposal ${tripped.id}, tx ${tripped.hash}`);
const untripped = await flows.tripFlow({ network: NET, asset: usdc, tripped: false, me: B.address, wallet: B, ui });
check('e. untrip by rh-b', untripped.tripped === false, `proposal ${untripped.id}, tx ${untripped.hash}`);

// (f) negative: a funded account without the role. The page refuses before simulating; and the chain itself
// refuses the same create (enforcing simulation), with the #2 hint.
const stranger = await keyWallet(crypto.getRandomValues(new Uint8Array(32)));
const funded = await fetch(`https://friendbot.stellar.org/?addr=${stranger.address}`);
if (!funded.ok) throw new Error(`friendbot ${funded.status}`);
const byRole = await refusal(flows.tripFlow({ network: NET, asset: usdc, tripped: true, me: stranger.address, wallet: stranger, ui, practice: true }));
check('f1. a stranger is refused by the role check', byRole instanceof Refuse && /no ManualTripper role/.test(byRole.message), byRole && byRole.message);
const id = await rpc.nextProposalId(NET);
const call = build.tripCreateCall(pin.governance, stranger.address, id, usdc.id, true);
const byChain = await refusal(rpc.sendHostFunction({
  network: NET, call, auth: build.sourceAccountAuth(call), submitter: stranger.address, wallet: stranger, practice: true,
  hints: flows.TRIP_HINTS, onStatus: ui.status,
}));
check('f2. the chain refuses the same trip with the #2 hint',
  byChain instanceof Refuse && byChain.message.includes('This account is not allowed to trip.'), byChain && byChain.message.split('\n')[0]);

console.log(failures ? `\n${failures} step(s) FAILED` : '\nall steps PASSED');
process.exit(failures ? 1 : 0);
