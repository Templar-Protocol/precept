// The emergency console's flows, with no DOM: the page (builder.mjs) and the testnet rehearsal driver
// (test/testnet-rehearsal.mjs) run exactly this code, the driver with a key-backed wallet.
//
//   wallet = { address, signAuthEntry(preimageB64), signTransaction(envelopeB64), close() }   (wallets.mjs)
//   ui     = { status(msg), confirm(derived, title) -> Promise<boolean> }     (confirm shows what is signed)
//
// Everything a wallet is asked to sign is first run through decode.mjs:derive, the same policy a link goes through.
import { EMERGENCY } from './actions.mjs';
import * as build from './build.mjs';
import { Refuse, derive, encodeShare, verifySignature } from './decode.mjs';
import * as rpc from './rpc.mjs';

export class Cancelled extends Error {}

export const POOL_HINTS = {
  '#1204': 'The pool refused the status change: the backstop is under its threshold or too much of it is '
    + 'queued for withdrawal, or the pool is frozen.',
  '#1': 'The pause policy refused this call.',
};
export const PAIR_HINTS = {
  '#1204': POOL_HINTS['#1204'],
  '#3202': 'Not enough signatures for the 2-of-3 rule.',
};
export const TRIP_HINTS = { '#2': 'This account is not allowed to trip.', '#8': 'proposal id moved' };

const SOLO_EXPIRY_LEDGERS = 720;
const PAIR_EXPIRY_LEDGERS = 31000;

/** derive -> show -> wallet signs the signer entry -> verified signature. Returns { derived, sigB64, sig }. */
async function signOwnEntry({ bundle, me, wallet, ed, ui, title }) {
  const derived = await derive(bundle, { signer: me });
  if (!(await ui.confirm(derived, title))) throw new Cancelled('cancelled');
  ui.status('waiting for your wallet to sign…');
  const sigB64 = await wallet.signAuthEntry(derived.signerPreimageB64, me);
  const sig = await verifySignature(me, derived.signerDigest, sigB64, ed);
  return { derived, sigB64, sig };
}

/** On-Ice `pool` with the signer's own pause rule: one signature, one transaction. */
export async function soloOnIce({ network, pool, me, wallet, ed, ui, practice = false }) {
  const pin = EMERGENCY[network];
  if (!(await rpc.ruleHasSigner(network, pool.pauseRule, me))) {
    throw new Refuse(`You are not on the pause rule for ${pool.name} yet — use On-Ice via 2-of-3.`);
  }
  const bundle = await build.buildBundle({
    network, account: pin.account, call: build.onIceCall(pool.id), signers: [me], threshold: 1,
    ruleId: pool.pauseRule, latestLedger: await rpc.latestLedger(network), expiryLedgers: SOLO_EXPIRY_LEDGERS,
  });
  const { sig } = await signOwnEntry({ bundle, me, wallet, ed, ui, title: `On-Ice ${pool.name}` });
  const res = await rpc.submitBundle({
    network, bundle, sigsByG: { [me]: sig }, submitter: me, wallet, practice, hints: POOL_HINTS, onStatus: ui.status,
  });
  if (res.practice) return res;
  return { ...res, status: await rpc.poolStatus(network, pool.id) };
}

/** Governance create + execute of `SetManualTrip` by a ManualTripper (or Admin), two transactions. */
export async function tripFlow({ network, asset, tripped, me, wallet, ui, practice = false }) {
  const gov = EMERGENCY[network].governance;
  let allowed = await rpc.hasRole(network, me, 'ManualTripper');
  if (!allowed) allowed = await rpc.hasRole(network, me, 'Admin').catch(() => false);
  if (!allowed) throw new Refuse('This account cannot trip (no ManualTripper role)');

  const create = (id) => {
    const call = build.tripCreateCall(gov, me, id, asset.id, tripped);
    return rpc.sendHostFunction({
      network, call, auth: build.sourceAccountAuth(call), submitter: me, wallet, practice,
      hints: TRIP_HINTS, onStatus: ui.status,
    });
  };
  let id = await rpc.nextProposalId(network);
  let created;
  try {
    created = await create(id);
  } catch (e) {
    if (!(e && e.raw && e.raw.includes('Error(Contract, #8)'))) throw e;
    id = await rpc.nextProposalId(network);      // somebody else's proposal moved the counter: once more
    created = await create(id);
  }
  if (created.practice) {
    return { practice: true, id, note: 'create would succeed; a real run executes it right after.' };
  }
  ui.status(`proposal ${id} created; executing it…`);
  return executeProposal({ network, asset, id, me, wallet, ui, afterCreate: true });
}

/** Send `execute_proposal(me, id)` and read the breaker back. Also the retry after a failed execute. */
export async function executeProposal({ network, asset, id, me, wallet, ui, afterCreate = false }) {
  const gov = EMERGENCY[network].governance;
  const call = build.executeCall(gov, me, id);
  let res;
  try {
    res = await rpc.sendHostFunction({
      network, call, auth: build.sourceAccountAuth(call), submitter: me, wallet, hints: TRIP_HINTS, onStatus: ui.status,
    });
  } catch (e) {
    if (afterCreate && e instanceof Refuse) {
      const err = new Refuse(`Proposal ${id} was created but not executed: ${e.message}`);
      err.proposalId = id;
      throw err;
    }
    throw e;
  }
  return { ...res, id, tripped: await rpc.isTripped(network, asset.id) };
}

/** Two-signer action: this signer builds and signs, and the others get a link that carries the signature. */
export async function twoSignerBuild({ network, call, title, me, wallet, ed, ui, baseUrl }) {
  const pin = EMERGENCY[network];
  const bundle = await build.buildBundle({
    network, account: pin.account, call, signers: pin.signers, threshold: pin.threshold, ruleId: 0,
    latestLedger: await rpc.latestLedger(network), expiryLedgers: PAIR_EXPIRY_LEDGERS,
  });
  const { derived, sigB64 } = await signOwnEntry({ bundle, me, wallet, ed, ui, title });
  const share = encodeShare({
    network, account: pin.account, signer: me, expiry: derived.expiry, payloadHex: derived.payloadHex,
    signatureB64: sigB64,
  });
  const frag = await build.encodeFragment(bundle);
  const links = pin.signers.filter((g) => g !== me).map((g) => [g, `${baseUrl}#b=${frag}&s=${g}&p=${share}`]);
  return { bundle, derived, share, links, message: handoffMessage({ title, derived, me, links }) };
}

/** The chat message for the second signer (same fields the CLI's `link` prints). */
export function handoffMessage({ title, derived, me, links }) {
  return [
    `Precept: ${title} needs a second signature.`,
    `network: ${derived.network}`,
    `smart account: ${derived.account}`,
    'action:',
    ...derived.action.map((l) => `  ${l}`),
    `valid until: ${derived.expiry} (${derived.expiryHuman})`,
    `expected signature_payload: ${derived.payloadHex}`,
    `already signed by: ${me}`,
    ...links.map(([g, url]) => `${g}: ${url}`),
    'Open your link, check it, sign, then press "Submit now".',
  ].join('\n');
}
