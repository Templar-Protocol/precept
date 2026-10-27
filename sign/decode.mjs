// ============================================================================================
// Decoder for a sign link. NOTHING here trusts the bundle: every field is re-derived from XDR and
// compared, exactly like `decode_bundle` in tools/multisig/smart_account.py. If anything disagrees
// the page refuses with a specific message instead of asking a wallet to sign.
//
// Loss model: the bundle travels in the URL fragment, so a hostile page could hand the page any
// bundle. What that cannot change is the message the signer received from the proposer: the
// `signature_payload` printed there must equal what Freighter shows. This module computes that value
// from the entries themselves, so a bundle whose entries commit to a different action shows up as a
// different payload.
// ============================================================================================
import * as xdr from './xdr-lite.mjs';

export const PINNED = {
  testnet: 'Test SDF Network ; September 2015',
  mainnet: 'Public Global Stellar Network ; September 2015',
};
export const CHAINS = { testnet: 'stellar:testnet', mainnet: 'stellar:pubnet' };
// Only the admin accounts this page is allowed to sign for. A link naming anything else is refused:
// Freighter shows `__check_auth` on whatever account the entry says, so an unknown account is exactly
// how a hostile link would try to make a signer authorize a different account's rule.
export const KNOWN_ACCOUNTS = {
  mainnet: ['CCDZKWLV6JWM4V3WPMJHXTSLWG5IDQ5S3K7BI3AH7MBCRQCPCT3EJ2AI'],
  testnet: ['CAUO3A7E4W6KVOJ3FHQVUSRRDZQCVW4TKP7D7M2PP3PO2EY734SJE73P',
            'CCSLQYY74HV7OOER5ENLTP7CO6M7QJU6LWURLXRPFYBNZBTKNGMQQLRH'],
};
// The page signs nothing that stays usable for longer than this (the CLI default is the same).
export const MAX_DAYS = 2;
// Same anchors as the Python tool: (ledger, unix close time), 5 s per ledger.
export const LEDGER_ANCHORS = {
  mainnet: [65165790, 1793115600],
  testnet: [5418022, 1793113734],
};
export const LEDGER_SECONDS = 5;
export const BUNDLE_VERSION = 1;
const ENVELOPE_LEGACY = 9;
const ENVELOPE_WITH_ADDRESS = 10;
const HOST_FN_INVOKE_CONTRACT = 0;

export class Refuse extends Error {}

function need(cond, msg) {
  if (!cond) throw new Refuse(msg);
}

export async function sha256(bytes) {
  return new Uint8Array(await crypto.subtle.digest('SHA-256', bytes));
}

// ---------------------------------------------------------------- fragment
export function base64urlToBytes(text) {
  const s = String(text).replace(/-/g, '+').replace(/_/g, '/');
  return xdr.base64ToBytes(s + '='.repeat((4 - (s.length % 4)) % 4));
}

export function bytesToBase64url(bytes) {
  return xdr.bytesToBase64(bytes).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

export async function inflateJson(bytes) {
  if (typeof DecompressionStream === 'function') {
    const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream('deflate'));
    const buf = new Uint8Array(await new Response(stream).arrayBuffer());
    return JSON.parse(new TextDecoder().decode(buf));
  }
  const zlib = await import('node:zlib'); // bun/node only, for the tests
  return JSON.parse(zlib.inflateSync(Buffer.from(bytes)).toString('utf8'));
}

/** `#b=<base64url(deflate(json))>` -> bundle object. */
export async function decodeFragment(fragmentOrUrl) {
  const text = String(fragmentOrUrl);
  const hash = text.includes('#') ? text.slice(text.indexOf('#') + 1) : text;
  const params = new URLSearchParams(hash);
  const b = params.get('b');
  need(b, 'this link has no bundle in it (expected “#b=…”). Ask the proposer to resend it.');
  let bundle;
  try {
    bundle = await inflateJson(base64urlToBytes(b));
  } catch (e) {
    throw new Refuse('the bundle in this link is corrupt or truncated — ask the proposer to resend the whole link.');
  }
  need(bundle && typeof bundle === 'object', 'the bundle is not an object');
  return bundle;
}

// ---------------------------------------------------------------- rendering
const SYMBOL_RE = /^[A-Za-z0-9_]{1,32}$/;

export function renderValue(v) {
  const hex = xdr.hex;
  switch (v.type) {
    case 1: return 'void';
    case 0: return String(v.value);
    case 18: return v.value.strkey;
    case 13: return '0x' + hex(v.value);
    case 15:
      need(SYMBOL_RE.test(v.value), 'refusing to display a symbol with unusual characters');
      return v.value;
    // one implementation of string rendering: the same quoteString() the vectors assert against
    // Python's describe output, so a bidi or invalid-UTF-8 string can never be shown raw here
    case 14: return xdr.renderScVal(v);
    case 16: return '[' + v.value.map(renderValue).join(', ') + ']';
    case 17: return '{' + v.value.map(([k, val]) => `${renderValue(k)}: ${renderValue(val)}`).join(', ') + '}';
    case 3: return `u32(${v.value})`;
    case 4: return `i32(${v.value})`;
    case 5: return `u64(${v.value})`;
    case 6: return `i64(${v.value})`;
    case 9: return `u128(${v.value})`;
    case 10: return `i128(${v.value})`;
    default: throw new Refuse('refusing to display an argument the page cannot render faithfully');
  }
}

export function actionLines(invocation) {
  need(invocation.subInvocations.length === 0,
    'this action authorizes other contract calls inside it (sub-invocations), which this page does not '
    + 'sign. Ask the proposer to use the command-line tool for this one.');
  const f = invocation.function;
  need(SYMBOL_RE.test(f.name), 'refusing to display a function name with unusual characters');
  const lines = [`contract  ${f.contract}`, `function  ${f.name}`];
  f.args.forEach((a, i) => lines.push(`arg[${i}]    ${renderValue(a)}`));
  return lines;
}

export function expiryEstimate(expiryLedger, network, now = Date.now() / 1000) {
  const [anchorLedger, anchorTime] = LEDGER_ANCHORS[network];
  const eta = anchorTime + (Number(expiryLedger) - anchorLedger) * LEDGER_SECONDS;
  const remaining = eta - now;
  const stamp = new Date(eta * 1000).toISOString().replace('T', ' ').slice(0, 16) + 'Z';
  const hours = remaining / 3600;
  const human = remaining > 0
    ? `${stamp} (~${hours.toFixed(1)} h / ${(hours / 24).toFixed(1)} d from now)`
    : `${stamp} (in the past)`;
  return { remaining, human };
}

// ---------------------------------------------------------------- derivation
function compareInvocationRaw(invocationRaw, hostFnInvokeRaw) {
  if (invocationRaw.length !== hostFnInvokeRaw.length) return false;
  for (let i = 0; i < invocationRaw.length; i++) if (invocationRaw[i] !== hostFnInvokeRaw[i]) return false;
  return true;
}

/** Read a HostFunction that must be INVOKE_CONTRACT and return the raw InvokeContractArgs bytes. */
export function hostInvokeRaw(hostFnB64) {
  const bytes = xdr.base64ToBytes(hostFnB64);
  need(bytes.length >= 8, 'the bundle’s host function is truncated.');
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const type = view.getUint32(0);
  need(type === HOST_FN_INVOKE_CONTRACT,
    'the bundle authorizes something other than a single contract call (create/upload), which this page refuses.');
  return bytes.subarray(4);
}

/**
 * Re-derive everything a signer needs, or throw Refuse.
 * Mirrors tools/multisig/smart_account.py:decode_bundle plus the page's own policy (no sub-invocations).
 */
export async function derive(bundle, { signer, network: expectNetwork, now } = {}) {
  need(bundle.version === BUNDLE_VERSION, 'unsupported bundle version');
  const netName = bundle?.network?.name;
  need(Object.hasOwn(PINNED, netName), `the bundle names an unknown network (${netName}).`);
  need(bundle.network.passphrase === PINNED[netName],
    'the bundle’s network passphrase does not belong to the network it names — refusing.');
  if (expectNetwork) need(netName === expectNetwork, `this link is for ${netName}, not ${expectNetwork}.`);

  const account = bundle.account;
  need(/^C[A-Z2-7]{55}$/.test(account), 'the bundle’s account is not a contract address.');
  need((KNOWN_ACCOUNTS[netName] || []).includes(account),
    `this page only signs for the known admin accounts (${(KNOWN_ACCOUNTS[netName] || []).join(', ') || 'none on this network'}). `
    + `The link names ${account}; if that is genuinely your admin account, the pinned list has to be updated in a reviewed change.`);
  const expiry = Number(bundle.expiry_ledger);
  need(Number.isInteger(expiry) && expiry > 0, 'the bundle has no valid expiry ledger.');

  const acct = xdr.readEntry(bundle.account_entry_xdr);
  need(acct.address === account, 'the account entry is not for the account the bundle names.');
  need(acct.expiry === expiry, 'the account entry’s expiry disagrees with the bundle.');
  need(acct.signature.type === 1, 'the account entry already carries a signature (it should be unsigned).');

  const hostRaw = hostInvokeRaw(bundle.host_function_xdr);
  need(compareInvocationRaw(acct.invocation.functionRaw, hostRaw),
    'the action in the bundle does not match the action the entries authorize — refusing.');

  const ruleIds = (bundle.context_rule_ids || []).map(Number);
  need(Array.isArray(ruleIds) && ruleIds.length === 1,
    'this page only signs authorizations with exactly one auth context (the account’s Default rule).');
  need(ruleIds[0] === 0, 'the bundle selects a context rule other than 0 — refusing.');

  const networkId = await sha256(new TextEncoder().encode(bundle.network.passphrase));
  // The ACCOUNT entry's preimage digest is the `signature_payload` that ends up inside __check_auth —
  // the 32 bytes a signer compares against the message they received. It is NOT what the wallet signs:
  // each delegated signer signs its own entry's preimage (address-bound to that G, its own nonce).
  const preimage = xdr.preimageBytes(acct, networkId);
  const digest = await sha256(preimage);
  const payloadHex = xdr.hex(digest);

  const signers = Array.from(new Set(bundle.signers || []));
  need(signers.length === (bundle.signers || []).length, 'the bundle lists a signer twice.');
  need(signers.length > 0, 'the bundle lists no signers.');
  for (const g of signers) need(/^G[A-Z2-7]{55}$/.test(g), `bundle signer ${g} is not an account address.`);
  if (signer) need(signers.includes(signer), `${signer} is not a signer of this bundle.`);

  // every per-signer entry must be this account's __check_auth with the preimage built above
  const expectedPreimageKeys = { account, ruleIds, payloadHex };
  for (const g of signers) {
    const raw = bundle.signer_entries && bundle.signer_entries[g];
    need(raw, `the bundle has no entry for signer ${g}.`);
    const se = xdr.readEntry(raw);
    need(se.address === g, `the entry stored for ${g} is addressed to someone else.`);
    need(se.credentialsType === acct.credentialsType,
      'the bundle mixes credential types between the account and its signers — refusing.');
    need(se.expiry === expiry, `the entry for ${g} has a different expiry.`);
    need(se.invocation.subInvocations.length === 0, `the entry for ${g} carries sub-invocations.`);
    need(se.invocation.function.name === '__check_auth', `the entry for ${g} does not authorize __check_auth.`);
    need(se.invocation.function.contract === account, `the entry for ${g} authorizes a different contract.`);
    need(se.invocation.function.args.length === 1, `the entry for ${g} has an unexpected argument list.`);
    const pre = se.invocation.function.args[0];
    need(pre.type === 17, `the entry for ${g} does not embed an AuthDigestPreimage map.`);
    const keys = pre.value.map(([k]) => (k.type === 15 ? k.value : '?'));
    need(keys.join(',') === 'account,context_rule_ids,signature_payload',
      `the entry for ${g} commits to an unexpected preimage shape (${keys.join(',')}).`);
    const value = Object.fromEntries(pre.value.map(([k, v]) => [k.value, v]));
    need(value.account.type === 18 && value.account.value.strkey === expectedPreimageKeys.account,
      `the entry for ${g} commits to a different account.`);
    need(value.context_rule_ids.type === 16
      && value.context_rule_ids.value.map((v) => v.value).join(',') === ruleIds.join(','),
      `the entry for ${g} commits to different context rule ids.`);
    need(value.signature_payload.type === 13
      && xdr.hex(value.signature_payload.value) === expectedPreimageKeys.payloadHex,
      `the entry for ${g} commits to a different signature payload.`);
  }

  // per-signer preimage (what the wallet signs, and what the returned signature must verify over)
  const chosen = signer || signers[0];
  const ownEntry = xdr.readEntry(bundle.signer_entries[chosen]);
  const signerPreimage = xdr.preimageBytes(ownEntry, networkId);
  const signerDigest = await sha256(signerPreimage);

  const { remaining, human } = expiryEstimate(expiry, netName, now);
  need(remaining > 0,
    `this authorization already expired (${human}). Ask the proposer for a new link.`);
  need(remaining <= MAX_DAYS * 86400,
    `this authorization would stay usable for ${(remaining / 86400).toFixed(1)} days (${human}), longer than the `
    + `${MAX_DAYS}-day limit this page signs. Ask the proposer for a shorter expiry.`);
  return {
    signer: chosen,
    signerPreimageB64: xdr.bytesToBase64(signerPreimage),
    signerDigest,
    signerEntryExpiry: ownEntry.expiry,
    network: netName, chain: CHAINS[netName], passphrase: bundle.network.passphrase,
    account, signers, expiry, expiryRemaining: remaining, expiryHuman: human,
    action: actionLines(acct.invocation), invocation: acct.invocation,
    payloadHex, payloadB64: xdr.bytesToBase64(digest),
    // Freighter 5.49 (extension) and 1.26 (mobile) render the payload as decimal bytes — the released
    // apps show `{"0":124,…,"31":238}` — so the page shows the same values in that form too.
    payloadDec: Array.from(digest),
    payloadFreighterJson: freighterJson(digest),
    preimageB64: xdr.bytesToBase64(preimage), digest,
    credentialsType: acct.credentialsType,
  };
}

/** The exact string the released Freighter apps show for `signature_payload`
 *  (`JSON.stringify(scValToNative(bytes), null, 2)`). */
export function freighterJson(bytes) {
  return JSON.stringify(Object.fromEntries(Array.from(bytes, (b, i) => [String(i), b])), null, 2);
}

/** `&s=` handling: an unknown signer is refused outright (never silently swapped for another). */
export function chooseSigner(signers, wanted) {
  need(Array.isArray(signers) && signers.length > 0, 'this bundle lists no signers.');
  if (wanted === undefined || wanted === null || wanted === '') return signers[0];
  need(signers.includes(wanted),
    `this link is for ${wanted}, who is not a signer of this bundle (signers: ${signers.join(', ')}).`);
  return wanted;
}

/** The extension must be signing with the claimed account, not whichever one happens to be active. */
export function requireSameAddress(returned, signer) {
  need(typeof returned === 'string' && returned.length > 0,
    'Freighter did not say which account would sign; refusing rather than guessing.');
  need(returned === signer,
    `Freighter is set to ${returned}, but this link is for ${signer}. Switch accounts in Freighter and retry.`);
  return returned;
}

/** WalletConnect keeps its session keys in this origin's localStorage (plus an IndexedDB database,
 *  which app.mjs deletes); these are the localStorage keys to clear. */
export function wcStorageKeys(keys) {
  return Array.from(keys || []).filter((k) => /^(wc@|WALLETCONNECT|@walletconnect|wc_)/i.test(String(k)));
}

// ---------------------------------------------------------------- share
export const SHARE_PREFIX = 'pss1.';

export function encodeShare({ network, account, signer, expiry, payloadHex, signatureB64 }) {
  const body = {
    v: 1, network, account, signer, expiry: Number(expiry), payload: payloadHex, sig: signatureB64,
  };
  return SHARE_PREFIX + bytesToBase64url(new TextEncoder().encode(JSON.stringify(body)));
}

export function decodeShare(code) {
  const text = String(code).trim();
  need(text.startsWith(SHARE_PREFIX), `a signed share starts with ${SHARE_PREFIX}`);
  let body;
  try {
    body = JSON.parse(new TextDecoder().decode(base64urlToBytes(text.slice(SHARE_PREFIX.length))));
  } catch {
    throw new Refuse('that signed share is not readable — ask for it again.');
  }
  need(body && body.v === 1, 'unsupported share version');
  need(/^[A-Z2-7]{56}$/.test(String(body.account || '').slice(1)) && String(body.account).startsWith('C'),
    'the share names no contract account');
  need(/^G[A-Z2-7]{55}$/.test(String(body.signer || '')), 'the share names no account signer');
  need(/^[0-9a-f]{64}$/.test(String(body.payload || '')), 'the share has no payload digest');
  need(Number.isInteger(body.expiry) && body.expiry > 0, 'the share has no expiry');
  need(typeof body.sig === 'string' && body.sig.length > 0, 'the share has no signature');
  return body;
}

/** Verify a base64 ed25519 signature over the digest, against the signer's G address. */
export async function verifySignature(signerG, digest, signatureB64, ed) {
  let sig;
  try {
    sig = xdr.base64ToBytes(signatureB64);
  } catch {
    throw new Refuse('the signature is not base64.');
  }
  need(sig.length === 64, `the signature is ${sig.length} bytes, expected 64.`);
  const pub = accountPublicKey(signerG);
  let ok = false;
  try {
    ok = await ed.verifyAsync(sig, digest, pub);
  } catch {
    ok = false;
  }
  need(ok, `the signature does not verify against ${signerG} for this payload — nothing was signed.`);
  return sig;
}

/** G… strkey -> 32 raw bytes (version byte and CRC checked). */
export function accountPublicKey(text) {
  need(/^G[A-Z2-7]{55}$/.test(text), 'not an account address');
  const raw = xdr.base32Decode(text);
  need(raw.length === 35 && raw[0] === (6 << 3), 'bad account address');
  const crc = raw[33] | (raw[34] << 8);
  need(xdr.crc16xmodem(raw.subarray(0, 33)) === crc, 'account address checksum does not match');
  return raw.subarray(1, 33);
}
