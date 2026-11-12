// Soroban RPC reads and the simulate -> sign -> send -> confirm pipeline of the emergency console.
// Mirrors tools/multisig/smart_account.py:cmd_submit (INCLUSION_FEE 10000, max fee 5 XLM, 300 s timeout,
// enforcing simulation first). Every transaction is simulated with `authMode: enforce`, so the chain's
// own auth rules (the account's policies included) refuse a bad call before anything is signed or sent.
import { EMERGENCY } from './actions.mjs';
import * as build from './build.mjs';
import { Refuse } from './decode.mjs';
import * as xdr from './xdr-lite.mjs';

export const INCLUSION_FEE = 10000;
export const MAX_FEE = 50_000_000;          // 5 XLM, the CLI default
const TIMEOUT_SECONDS = 300;
const POLL_MS = 2000;
const POLL_LIMIT_MS = 90_000;
const RETRY_MS = 3000;
const CHANGED = 'the wallet changed the transaction — nothing was sent.';

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const unb64 = xdr.base64ToBytes;
const b64 = xdr.bytesToBase64;

/** Hint text for the first hint key found as `Error(Contract, <key>)`, then what the node said. */
export function explain(text, hints = {}) {
  const said = `node said: ${String(text).slice(0, 400)}`;
  for (const [key, hint] of Object.entries(hints)) {
    if (String(text).includes(`Error(Contract, ${key})`)) return `${hint}\n\n${said}`;
  }
  return said;
}

/** The wallet must hand back exactly the envelope it was given, plus signatures. */
export function checkSignedEnvelope(unsigned, signed) {
  const prefix = unsigned.subarray(0, unsigned.length - 4);
  const view = new DataView(signed.buffer, signed.byteOffset, signed.byteLength);
  const ok = unsigned.length >= 4
    && new DataView(unsigned.buffer, unsigned.byteOffset, unsigned.byteLength).getUint32(prefix.length) === 0
    && signed.length >= prefix.length + 4
    && prefix.every((x, i) => signed[i] === x)
    && view.getUint32(prefix.length) >= 1
    && signed.length === prefix.length + 4 + view.getUint32(prefix.length) * (4 + 4 + 64);
  if (!ok) throw new Refuse(CHANGED);
}

export async function rpc(network, method, params) {
  const pinned = EMERGENCY[network];
  if (!pinned) throw new Refuse(`no emergency configuration for ${network}.`);
  let res;
  try {
    res = await fetch(pinned.rpc, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
    });
  } catch (e) {
    throw new Refuse(`RPC ${method}: ${e && e.message ? e.message : e}`);
  }
  if (!res.ok) throw new Refuse(`RPC ${method}: HTTP ${res.status}`);
  const body = await res.json();
  if (body.error) throw new Refuse(`RPC ${method}: ${body.error.message || JSON.stringify(body.error)}`);
  return body.result;
}

export async function latestLedger(network) {
  return (await rpc(network, 'getLatestLedger', {})).sequence;
}

/** The sequence number of a classic account; refuses if it does not exist (it would have to pay the fee). */
export async function loadAccount(network, g) {
  const key = xdr.bytesToBase64(new Uint8Array([0, 0, 0, 0, 0, 0, 0, 0, ...xdr.scAddressFromStrkey(g).subarray(8)]));
  const { entries } = await rpc(network, 'getLedgerEntries', { keys: [key] });
  if (!entries || entries.length === 0) {
    throw new Refuse(`account ${g} does not exist on ${network}; it needs XLM to pay the fee.`);
  }
  return {
    seq: xdr.accountSeqFromEntryData(entries[0].xdr),
  };
}

const maxTime = () => Math.floor(Date.now() / 1000) + TIMEOUT_SECONDS;

function hostOf(call) {
  return xdr.hostFunctionBytes(xdr.invokeArgsBytes(call.contract, call.fn, call.args));
}

// ---- reads (simulations, no auth, sourced from a pinned signer) --------------------------------

const readSeq = {};
export async function readCall(network, call) {
  const source = EMERGENCY[network].signers[0];
  readSeq[network] ??= (await loadAccount(network, source)).seq;
  const tx = xdr.transactionBytes({
    source, fee: INCLUSION_FEE, seq: readSeq[network] + 1n, maxTime: maxTime(), hostFunction: hostOf(call), auth: [],
  });
  const sim = await rpc(network, 'simulateTransaction', { transaction: b64(xdr.envelopeBytes(tx)) });
  if (sim.error) throw new Refuse(`read ${call.fn}: ${String(sim.error).slice(0, 200)}`);
  return xdr.readScValB64(sim.results[0].xdr);
}

const field = (map, name) => {
  const hit = map.value.find(([k]) => k.type === xdr.SCV.SYMBOL && k.value === name);
  if (!hit) throw new Refuse(`unexpected response: no field ${name}`);
  return hit[1];
};

export async function poolStatus(network, pool) {
  return field(await readCall(network, build.getConfigCall(pool)), 'status').value;
}

export async function isTripped(network, assetId) {
  const view = await readCall(network, build.breakerViewCall(EMERGENCY[network].runtime, assetId));
  return field(view, 'is_manually_tripped').value;
}

export async function nextProposalId(network) {
  return (await readCall(network, build.nextIdCall(EMERGENCY[network].governance))).value;
}

export async function hasRole(network, g, role) {
  return (await readCall(network, build.hasRoleCall(EMERGENCY[network].governance, g, role))).value;
}

/** Is `g` a Delegated signer of the account's context rule? */
export async function ruleHasSigner(network, ruleId, g) {
  const rule = await readCall(network, build.contextRuleCall(EMERGENCY[network].account, ruleId));
  return field(rule, 'signers').value.some((s) => s.type === xdr.SCV.VEC && s.value.length === 2
    && s.value[0].value === 'Delegated' && s.value[1].value.strkey === g);
}

// ---- send --------------------------------------------------------------------------------------

/**
 * simulate (enforcing) -> [practice stops here] -> prepare -> wallet signs the envelope -> send -> poll.
 * Give `call`, or `hostFunction` bytes directly (a bundle carries those).
 */
export async function sendHostFunction({
  network, call, hostFunction, auth, submitter, wallet, practice = false, hints = {}, onStatus = () => {},
}) {
  const host = hostFunction ?? hostOf(call);
  const { seq } = await loadAccount(network, submitter);
  const base = { source: submitter, seq: seq + 1n, maxTime: maxTime(), hostFunction: host, auth };
  onStatus('simulating (the chain checks every signature)…');
  const simTx = xdr.envelopeBytes(xdr.transactionBytes({ ...base, fee: INCLUSION_FEE }));
  const sim = await rpc(network, 'simulateTransaction', {
    transaction: b64(simTx), authMode: 'enforce', useUpgradedAuth: true,
  });
  if (sim.error) {
    const err = new Refuse(explain(sim.error, hints));
    err.raw = String(sim.error);
    throw err;
  }
  const resourceFee = Number(sim.minResourceFee);
  if (practice) return { practice: true, resourceFee };

  const fee = INCLUSION_FEE + resourceFee;
  if (fee > MAX_FEE) throw new Refuse(`the fee ${fee} stroops is above the ${MAX_FEE} stroop (5 XLM) limit; nothing was sent.`);
  const unsigned = xdr.envelopeBytes(xdr.transactionBytes({ ...base, fee, sorobanData: unb64(sim.transactionData) }));
  onStatus('waiting for your wallet to sign the transaction…');
  const signed = unb64(await wallet.signTransaction(b64(unsigned)));
  checkSignedEnvelope(unsigned, signed);

  onStatus('sending…');
  let sent;
  for (let attempt = 0; attempt < 3; attempt++) {
    sent = await rpc(network, 'sendTransaction', { transaction: b64(signed) });
    if (sent.status !== 'TRY_AGAIN_LATER') break;
    await sleep(RETRY_MS);
  }
  if (sent.status === 'ERROR') throw new Refuse(`the network rejected the transaction: ${sent.errorResultXdr}`);
  if (sent.status === 'TRY_AGAIN_LATER') throw new Refuse('the network is busy; nothing was confirmed — try again.');
  return confirm(network, sent.hash, onStatus);
}

async function confirm(network, hash, onStatus) {
  const explorer = EMERGENCY[network].explorer;
  for (let waited = 0; waited < POLL_LIMIT_MS; waited += POLL_MS) {
    await sleep(POLL_MS);
    onStatus(`waiting for confirmation… ${Math.round(waited / 1000)} s`);
    const tx = await rpc(network, 'getTransaction', { hash });
    if (tx.status === 'SUCCESS') return { hash };
    if (tx.status === 'FAILED') throw new Refuse(`the transaction failed on chain (${hash}): ${tx.resultXdr}`);
  }
  throw new Refuse(`not confirmed after 90 s — check ${explorer}/tx/${hash} before trying again.`);
}

/** Assemble the signed authorization from a bundle and send it, once the ledger is still inside its expiry. */
export async function submitBundle({ network, bundle, sigsByG, submitter, wallet, practice, hints, onStatus }) {
  if ((await latestLedger(network)) >= bundle.expiry_ledger) {
    throw new Refuse('this authorization has expired; build it again.');
  }
  return sendHostFunction({
    network, hostFunction: unb64(bundle.host_function_xdr), auth: build.signedAuth(bundle, sigsByG),
    submitter, wallet, practice, hints, onStatus,
  });
}
