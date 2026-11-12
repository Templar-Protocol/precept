// Builds the bundles and signed authorization entries of the emergency console. Every function mirrors
// one in tools/multisig/smart_account.py (named in its JSDoc) and is byte-checked against that tool in
// test/builder.test.mjs. Nothing here decides what is allowed: decode.mjs:derive does, on the bundle
// this module produces, exactly as it does for a bundle that arrived in a link.
import * as xdr from './xdr-lite.mjs';
import { SCV } from './xdr-lite.mjs';
import { PINNED, actionLines, bytesToBase64url, sha256 } from './decode.mjs';

const enc = new TextEncoder();
const sym = (value) => ({ type: SCV.SYMBOL, value });
const u32 = (value) => ({ type: SCV.U32, value });
const u64 = (value) => ({ type: SCV.U64, value: BigInt(value) });
const addr = (strkey) => ({ type: SCV.ADDRESS, value: { strkey } });
const bytes = (value) => ({ type: SCV.BYTES, value });
const vec = (value) => ({ type: SCV.VEC, value });
const map = (value) => ({ type: SCV.MAP, value });
const bool = (value) => ({ type: SCV.BOOL, value });
const VOID = { type: SCV.VOID, value: null };

// ---- the calls the console makes: { contract, fn, args } ---------------------------------------

/** Pool `set_status(2)`: On-Ice. */
export const onIceCall = (pool) => ({ contract: pool, fn: 'set_status', args: [u32(2)] });
/** Pool `set_status(0)`: back to Active (admin). */
export const activeCall = (pool) => ({ contract: pool, fn: 'set_status', args: [u32(0)] });

/** Governance `create_proposal(caller, id, SetManualTrip(Stellar(asset), tripped, None), 0)`. */
export function tripCreateCall(gov, caller, id, assetId, tripped) {
  const op = vec([sym('SetManualTrip'), vec([sym('Stellar'), addr(assetId)]), bool(tripped), VOID]);
  return { contract: gov, fn: 'create_proposal', args: [addr(caller), u64(id), op, u64(0)] };
}

/** Governance `execute_proposal(caller, id)`. */
export const executeCall = (gov, caller, id) =>
  ({ contract: gov, fn: 'execute_proposal', args: [addr(caller), u64(id)] });

/** The auth list of a call the tx source authorizes itself (governance create/execute): one SOURCE_ACCOUNT entry. */
export const sourceAccountAuth = (call) => [xdr.sourceAccountEntryBytes(
  xdr.rootInvocationBytes(xdr.invokeArgsBytes(call.contract, call.fn, call.args)))];

// read-only calls (run as simulations)
export const getConfigCall = (pool) => ({ contract: pool, fn: 'get_config', args: [] });
export const breakerViewCall = (runtime, assetId) =>
  ({ contract: runtime, fn: 'get_breaker_set_view', args: [vec([sym('Stellar'), addr(assetId)])] });
export const nextIdCall = (gov) => ({ contract: gov, fn: 'next_proposal_id', args: [] });
export const hasRoleCall = (gov, g, role) =>
  ({ contract: gov, fn: 'has_role', args: [addr(g), vec([sym(role)])] });
export const contextRuleCall = (account, id) =>
  ({ contract: account, fn: 'get_context_rule', args: [u32(id)] });

// ---- signatures --------------------------------------------------------------------------------

const rawKey = (g) => xdr.scAddressFromStrkey(g).subarray(8);

function compareBytes(a, b) {
  for (let i = 0; i < Math.min(a.length, b.length); i++) if (a[i] !== b[i]) return a[i] - b[i];
  return a.length - b.length;
}

/** smart_account.py:auth_payload. Delegated signers map to empty Bytes, ordered by raw public key
 *  (the Soroban order of `Vec[Delegated, Address]` keys). */
export function authPayloadScVal(ruleIds, signers) {
  const sorted = [...signers].sort((a, b) => compareBytes(rawKey(a), rawKey(b)));
  return map([
    [sym('context_rule_ids'), vec(ruleIds.map(u32))],
    [sym('signers'), map(sorted.map((g) => [vec([sym('Delegated'), addr(g)]), bytes(new Uint8Array(0))]))],
  ]);
}

/** smart_account.py:default_account_signature: `Vec[Map{public_key, signature}]`. */
export function defaultAccountSignature(g, sig64) {
  if (sig64.length !== 64) throw new Error('a signature is 64 bytes');
  return vec([map([[sym('public_key'), bytes(rawKey(g))], [sym('signature'), bytes(sig64)]])]);
}

/** smart_account.py:cmd_submit, auth assembly: the account entry carrying the AuthPayload, then each
 *  signing signer's entry carrying its signature. Refuses below the bundle's threshold. */
export function signedAuth(bundle, sigsByG) {
  const signerList = bundle.signers.filter((g) => Object.hasOwn(sigsByG, g));
  const need = bundle.threshold ?? bundle.signers.length;
  if (signerList.length < need) {
    throw new Error(`need ${need} of ${bundle.signers.length} signatures, have ${signerList.length}`);
  }
  const account = xdr.withSignature(
    xdr.base64ToBytes(bundle.account_entry_xdr),
    new Uint8Array(xdr.encodeScVal(authPayloadScVal(bundle.context_rule_ids, signerList))));
  const signers = signerList.map((g) => xdr.withSignature(
    xdr.base64ToBytes(bundle.signer_entries[g]),
    new Uint8Array(xdr.encodeScVal(defaultAccountSignature(g, sigsByG[g])))));
  return [account, ...signers];
}

// ---- bundles -----------------------------------------------------------------------------------

function randomNonce() {
  const b = crypto.getRandomValues(new Uint8Array(8));
  return BigInt.asIntN(64, b.reduce((acc, x) => (acc << 8n) | BigInt(x), 0n));
}

/**
 * smart_account.py:new_bundle + the `propose` command (address_v2, root invocation only). Nonces are
 * consumed in order: the account entry first, then one per signer in list order.
 */
export async function buildBundle({
  network, account, call, signers, threshold, ruleId, latestLedger, expiryLedgers, nonces,
}) {
  const passphrase = PINNED[network];
  const n = nonces ?? Array.from({ length: 1 + signers.length }, randomNonce);
  const expiry = latestLedger + expiryLedgers;
  const invoke = xdr.invokeArgsBytes(call.contract, call.fn, call.args);
  const accountEntry = xdr.addressEntryBytes({
    credType: 'address_v2', address: account, nonce: n[0], expiry, invocation: xdr.rootInvocationBytes(invoke),
  });
  const parsed = xdr.readEntry(xdr.bytesToBase64(accountEntry));
  // the same call `derive` makes: sha256(preimage) is the `signature_payload` inside __check_auth
  const networkId = await sha256(enc.encode(passphrase));
  const payload = await sha256(xdr.preimageBytes(parsed, networkId));
  const preimage = map([
    [sym('account'), addr(account)],
    [sym('context_rule_ids'), vec([u32(ruleId)])],
    [sym('signature_payload'), bytes(payload)],
  ]);
  const delegate = xdr.rootInvocationBytes(xdr.invokeArgsBytes(account, '__check_auth', [preimage]));
  const signerEntries = {};
  signers.forEach((g, k) => {
    signerEntries[g] = xdr.bytesToBase64(xdr.addressEntryBytes({
      credType: 'address_v2', address: g, nonce: n[1 + k], expiry, invocation: delegate,
    }));
  });
  return {
    version: 1,
    network: { name: network, passphrase },
    account,
    expiry_ledger: expiry,
    context_rule_ids: [ruleId],
    signers: [...signers],
    threshold,
    credentials_type: 'address_v2',
    host_function_xdr: xdr.bytesToBase64(xdr.hostFunctionBytes(invoke)),
    account_entry_xdr: xdr.bytesToBase64(accountEntry),
    signer_entries: signerEntries,
    signed: {},
    summary: actionLines(parsed.invocation).join('\n'),
  };
}

const sortKeys = (v) => (Array.isArray(v) ? v.map(sortKeys)
  : v && typeof v === 'object'
    ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, sortKeys(v[k])])) : v);

/** `#b=` payload: key-sorted JSON, zlib-deflated, base64url (what decode.mjs:decodeFragment reads). */
export async function encodeFragment(bundle) {
  const json = enc.encode(JSON.stringify(sortKeys(bundle)));
  let out;
  if (typeof CompressionStream === 'function') {
    const stream = new Blob([json]).stream().pipeThrough(new CompressionStream('deflate'));
    out = new Uint8Array(await new Response(stream).arrayBuffer());
  } else {
    const zlib = await import('node:zlib'); // bun/node only, for the tests
    out = new Uint8Array(zlib.deflateSync(Buffer.from(json)));
  }
  return bytesToBase64url(out);
}
