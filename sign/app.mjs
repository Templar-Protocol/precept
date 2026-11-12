// Sign view (a link with #b=…) and routing. All logic that decides *what* gets signed lives in decode.mjs
// (tested against the Python tool); this file wires the DOM and the wallets. No `#b=` opens the builder.
import { EMERGENCY } from './actions.mjs';
import { startBuilder } from './builder.mjs';
import {
  Refuse, KNOWN_ACCOUNTS, chooseSigner, decodeFragment, derive, encodeShare, requireSameAddress,
  verifyCarriedShare, verifySignature,
} from './decode.mjs';
import { $, copyFrom, fatal, loadEd, setText, txLink } from './dom.mjs';
import { PAIR_HINTS } from './flows.mjs';
import { submitBundle, poolStatus } from './rpc.mjs';
import { WC_PROJECT_ID, extensionWallet, mobileWallet } from './wallets.mjs';
import { base64ToBytes } from './xdr-lite.mjs';

const status = (msg, cls = '') => setText('status', msg, cls);

// { bundle, signer, derived, carried, sigs: {G: Uint8Array}, wallet }
let state = null;

function fillSigners(signers, chosen) {
  const sel = $('signer');
  sel.replaceChildren(...signers.map((g) => {
    const o = document.createElement('option');
    o.value = g;
    o.textContent = g;
    if (g === chosen) o.selected = true;
    return o;
  }));
}

async function closeWallet() {
  const wallet = state && state.wallet;
  if (state) state.wallet = null;
  if (wallet) await wallet.close();
}

async function show(signers, preferred) {
  await closeWallet();
  const signer = chooseSigner(signers, preferred);
  const derived = await derive(state.bundle, { signer });
  // a signature carried in the link (&p=) is another signer's: verified before anything is shown
  const p = new URLSearchParams(location.hash.slice(1)).get('p');
  const carried = p ? await verifyCarriedShare(state.bundle, derived, p, await loadEd()) : null;
  state.signer = signer;
  state.derived = derived;
  state.carried = carried;
  state.sigs = carried ? { [carried.signer]: base64ToBytes(carried.sig) } : {};
  $('main').hidden = false;
  $('sign-intro').hidden = false;
  $('carried').hidden = !carried;
  if (carried) $('carried').textContent = `already signed by ${carried.signer}`;
  $('action').textContent = derived.action.join('\n');
  $('network').textContent = derived.network;
  $('account').textContent = derived.account;
  $('expiry').textContent = `${derived.expiry} · ${derived.expiryHuman}`;
  $('payload-hex').textContent = derived.payloadHex;
  $('payload-b64').textContent = derived.payloadB64;
  $('payload-dec').textContent = derived.payloadDec.join(' ');
  $('payload-freighter').textContent = derived.payloadFreighterJson;
  $('known-account').textContent = (KNOWN_ACCOUNTS[derived.network] || []).includes(derived.account)
    ? 'a known admin account' : 'NOT a pinned account (the page refuses these)';
  fillSigners(derived.signers, state.signer);
  $('share-box').hidden = true;
  $('submit-box').hidden = true;
  $('submit-status').textContent = '';
  $('submit-result').replaceChildren();
  $('qr').replaceChildren();
  status('');
}

function showShare(signatureB64) {
  const share = encodeShare({
    network: state.derived.network, account: state.derived.account, signer: state.signer,
    expiry: state.derived.expiry, payloadHex: state.derived.payloadHex, signatureB64,
  });
  $('share').value = share;
  $('share-box').hidden = false;
  status('Signed. Copy the code below and send it back to the proposer.', 'ok');
}

/** Both signatures present (or the threshold met by this one)? Then the page can submit. */
function canSubmit() {
  const have = state.derived.signers.filter((g) => Object.hasOwn(state.sigs, g)).length;
  return EMERGENCY[state.derived.network] && have >= (state.bundle.threshold ?? state.derived.signers.length);
}

async function acceptSignature(signatureB64, reportedSigner) {
  if (reportedSigner && reportedSigner !== state.signer) {
    throw new Refuse(`the wallet signed with ${reportedSigner}, but this link is for ${state.signer}.`);
  }
  const sig = await verifySignature(state.signer, state.derived.signerDigest, signatureB64, await loadEd());
  state.sigs[state.signer] = sig;
  showShare(signatureB64);
  if (canSubmit()) {
    $('submit-fee').textContent = `You pay the network fee (well under 1 XLM) from ${state.signer}.`;
    $('submit-box').hidden = false;
  }
}

/** Open a wallet, sign this signer's entry. The wallet stays open only when a submit can follow. */
async function signWith(openWallet) {
  try {
    await closeWallet();
    const wallet = await openWallet();
    state.wallet = wallet;
    if (wallet.address) requireSameAddress(wallet.address, state.signer);
    status('waiting for your wallet…');
    const sig = await wallet.signAuthEntry(state.derived.signerPreimageB64, state.signer);
    await acceptSignature(sig, state.signer);
  } catch (e) {
    status(e instanceof Refuse ? e.message : `the wallet reported: ${e && e.message ? e.message : e}`, 'err');
  } finally {
    if ($('submit-box').hidden) await closeWallet();
  }
}

const showQr = (canvas) => {
  $('qr').replaceChildren(...(canvas ? [canvas] : []));
  if (canvas) status('scan this QR code with Freighter Mobile');
};

$('ext').addEventListener('click', () => signWith(() => {
  status('asking Freighter for access to this site…');
  return extensionWallet(state.derived.network);
}));

$('mob').addEventListener('click', () => signWith(() => {
  status('opening WalletConnect…');
  return mobileWallet(state.derived.network, showQr);
}));

$('submit').addEventListener('click', async () => {
  const { derived } = state;
  $('submit').disabled = true;
  $('submit-result').replaceChildren();
  try {
    const wallet = state.wallet;
    if (!wallet) throw new Refuse('the wallet session ended — sign again, then submit.');
    const res = await submitBundle({
      network: derived.network, bundle: state.bundle, sigsByG: state.sigs, submitter: state.signer, wallet,
      practice: false, hints: PAIR_HINTS, onStatus: (m) => setText('submit-status', m),
    });
    setText('submit-status', 'Submitted and confirmed.', 'ok');
    const out = $('submit-result');
    out.append('transaction ', txLink(EMERGENCY[derived.network].explorer, res.hash));
    const pool = EMERGENCY[derived.network].pools.find((p) => p.id === derived.invocation.function.contract);
    if (pool) {
      try { out.append(` — pool ${pool.name} status now ${await poolStatus(derived.network, pool.id)}`); } catch { /* read-back is best effort */ }
    }
  } catch (e) {
    setText('submit-status', e instanceof Refuse ? e.message : `submit failed: ${e && e.message ? e.message : e}`, 'err');
  } finally {
    $('submit').disabled = false;
    await closeWallet();
  }
});

$('copy').addEventListener('click', () => copyFrom('share', 'copy-status'));

$('signer').addEventListener('change', async () => {
  try {
    await show(state.bundle.signers, $('signer').value);
  } catch (e) {
    fatal(e instanceof Refuse ? e.message : `this link cannot be used: ${e && e.message ? e.message : e}`);
    $('main').hidden = true;
  }
});

addEventListener('pagehide', () => { closeWallet().catch(() => {}); });

(async function start() {
  // GitHub Pages cannot send frame-ancestors, so refuse to run framed at all.
  if (top !== self) {
    fatal('This page refuses to run inside a frame. Open the link directly in your browser.');
    return;
  }
  const hash = new URLSearchParams(location.hash.slice(1));
  if (!hash.has('b')) {
    if (location.hash.length > 1) {
      fatal('This page needs a complete sign link (the part after “#b=…”). Ask the proposer to resend '
        + 'the whole link — chat apps sometimes cut it off at the first “#”.');
      return;
    }
    startBuilder();
    return;
  }
  try {
    state = { bundle: await decodeFragment(location.hash), wallet: null };
    await show(state.bundle.signers, hash.get('s') || undefined);
    if (!WC_PROJECT_ID) {
      $('mob').disabled = true;
      $('mob-note').textContent = 'Freighter Mobile needs a WalletConnect project id.';
    }
  } catch (e) {
    $('main').hidden = true;
    if (e instanceof Refuse) fatal(e.message);
    else fatal(`this link cannot be used: ${e && e.message ? e.message : e}`);
  }
}());
