// Sign-page UI. All logic that decides *what* gets signed lives in decode.mjs (tested against the
// Python tool); this file only wires the DOM, the two wallets, and the display.
import {
  Refuse, KNOWN_ACCOUNTS, chooseSigner, decodeFragment, derive, encodeShare, requireSameAddress,
  verifySignature, wcStorageKeys,
} from './decode.mjs';
import { bytesToBase64 } from './xdr-lite.mjs';

// Public WalletConnect client id (not a secret). Without it the mobile path is disabled with a
// message; the extension path needs nothing.
const WC_PROJECT_ID = '18de83305a3f84d31fc856839867319f';
// Must be exactly the origin allow-listed in WalletConnect Cloud.
const PAGE_ORIGIN = 'https://templar-protocol.github.io';

const $ = (id) => document.getElementById(id);
const status = (msg, cls = '') => { const el = $('status'); el.textContent = msg; el.className = cls; };
const fatal = (msg) => {
  $('fatal').hidden = false;
  $('fatal').innerHTML = '';
  const box = document.createElement('div');
  box.className = 'guard';
  box.textContent = msg;
  $('fatal').appendChild(box);
};

let ed = null;       // @noble/ed25519, loaded lazily
let state = null;    // { bundle, signer, derived }

async function loadEd() {
  if (ed) return ed;
  ed = await import('./vendor/noble-ed25519-3.2.0.mjs');
  ed.hashes.sha512Async = async (m) => new Uint8Array(await crypto.subtle.digest('SHA-512', m));
  return ed;
}

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

async function show(signers, preferred) {
  const signer = chooseSigner(signers, preferred);
  const derived = await derive(state.bundle, { signer });
  state.signer = signer;
  state.derived = derived;
  $('main').hidden = false;
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

async function acceptSignature(signatureB64, reportedSigner) {
  if (reportedSigner && reportedSigner !== state.signer) {
    throw new Refuse(`the wallet signed with ${reportedSigner}, but this link is for ${state.signer}.`);
  }
  const noble = await loadEd();
  await verifySignature(state.signer, state.derived.signerDigest, signatureB64, noble);
  showShare(signatureB64);
}

$('ext').addEventListener('click', async () => {
  const api = window.freighterApi;
  if (!api || typeof api.signAuthEntry !== 'function') {
    status('Freighter not detected. Install the Freighter extension, then reload this page.', 'err');
    return;
  }
  try {
    status('asking Freighter for access to this site…');
    const access = typeof api.requestAccess === 'function' ? await api.requestAccess() : null;
    if (access && access.error) throw new Refuse(`Freighter: ${access.error.message || access.error}`);
    if (access && access.address) requireSameAddress(access.address, state.signer);
    status('waiting for the Freighter extension…');
    const res = await api.signAuthEntry(state.derived.signerPreimageB64, {
      networkPassphrase: state.derived.passphrase, address: state.signer,
    });
    if (res.error) throw new Refuse(res.error.message || String(res.error));
    const sig = res.signedAuthEntry || res.signature;
    if (!sig) throw new Refuse('the extension returned no signature.');
    await acceptSignature(sig, res.signerAddress || state.signer);
  } catch (e) {
    status(e instanceof Refuse ? e.message : `the wallet reported: ${e && e.message ? e.message : e}`, 'err');
  }
});

$('mob').addEventListener('click', async () => {
  if (!WC_PROJECT_ID) {
    status('Freighter Mobile is not configured for this page (no WalletConnect project id).', 'err');
    return;
  }
  let provider = null;
  try {
    status('opening WalletConnect…');
    const { UniversalProvider } = await import('./vendor/universal-provider-2.26.0.mjs');
    const QR = (await import('./vendor/qrcode-1.5.4.mjs')).default;
    provider = await UniversalProvider.init({
      projectId: WC_PROJECT_ID,
      metadata: {
        name: 'Precept admin signing', description: 'Sign one admin authorization',
        url: PAGE_ORIGIN, icons: [],
      },
    });
    provider.on('display_uri', (uri) => {
      status('scan this QR code with Freighter Mobile');
      const canvas = document.createElement('canvas');
      $('qr').replaceChildren(canvas);
      QR.toCanvas(canvas, uri, { width: 320 }).catch(() => {});
    });
    await provider.connect({
      namespaces: {
        stellar: {
          methods: ['stellar_signAuthEntry'], chains: [state.derived.chain], events: ['accountsChanged'],
        },
      },
    });
    status('connected — approve the request in Freighter Mobile…');
    const res = await provider.request(
      { method: 'stellar_signAuthEntry', params: { entryXdr: state.derived.signerPreimageB64 } },
      state.derived.chain,
    );
    await acceptSignature(res.signedAuthEntry, res.signerAddress || state.signer);
  } catch (e) {
    status(e instanceof Refuse ? e.message
      : `WalletConnect reported: ${e && e.message ? e.message : e}`, 'err');
  } finally {
    // The session lives in this origin (shared with every templar-protocol.github.io page): end it and
    // clear what it wrote — localStorage keys plus WalletConnect's IndexedDB database — either way.
    try { if (provider && provider.disconnect) await provider.disconnect(); } catch { /* ignore */ }
    try {
      for (const key of wcStorageKeys(Object.keys(localStorage))) localStorage.removeItem(key);
    } catch { /* ignore */ }
    try {
      indexedDB.deleteDatabase('WALLET_CONNECT_V2_INDEXED_DB');
    } catch { /* ignore */ }
    $('qr').replaceChildren();
  }
});

$('copy').addEventListener('click', async () => {
  try {
    await navigator.clipboard.writeText($('share').value);
    $('copy-status').textContent = 'copied';
  } catch {
    $('share').select();
    $('copy-status').textContent = 'press ⌘/Ctrl-C to copy';
  }
});

$('signer').addEventListener('change', async () => {
  try {
    await show(state.bundle.signers, $('signer').value);
  } catch (e) {
    fatal(e instanceof Refuse ? e.message : `this link cannot be used: ${e && e.message ? e.message : e}`);
  }
});

(async function start() {
  // GitHub Pages cannot send frame-ancestors, so refuse to run framed at all.
  if (top !== self) {
    fatal('This page refuses to run inside a frame. Open the link directly in your browser.');
    return;
  }
  try {
    const hash = new URLSearchParams(location.hash.slice(1));
    const wanted = hash.get('s');
    const bundle = await decodeFragment(location.hash);
    state = { bundle };
    await show(bundle.signers, wanted || undefined);
    if (!WC_PROJECT_ID) {
      $('mob').disabled = true;
      $('mob-note').textContent = 'Freighter Mobile needs a WalletConnect project id.';
    }
  } catch (e) {
    if (e instanceof Refuse) fatal(e.message);
    else if (/no bundle/i.test(String(e && e.message))) {
      fatal('This page needs a complete sign link (the part after “#b=…”). Ask the proposer to resend '
        + 'the whole link — chat apps sometimes cut it off at the first “#”.');
    } else fatal(`this link cannot be used: ${e && e.message ? e.message : e}`);
  }
}());
