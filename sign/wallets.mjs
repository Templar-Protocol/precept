// The two wallets as one interface: { address, signAuthEntry(preimageB64) -> signatureB64,
// signTransaction(envelopeB64) -> signedEnvelopeB64, close() }. Every flow (sign view and builder) uses
// these; nothing else in the page talks to Freighter.
import { CHAINS, PINNED, Refuse, requireSameAddress, wcStorageKeys } from './decode.mjs';
import { el } from './dom.mjs';

// Public WalletConnect client id (not a secret). Without it the mobile path is disabled with a
// message; the extension path needs nothing.
export const WC_PROJECT_ID = '18de83305a3f84d31fc856839867319f';
// Must be exactly the origin allow-listed in WalletConnect Cloud.
const PAGE_ORIGIN = 'https://templar-protocol.github.io';

// Varial (Freighter Mobile's new name) on the same phone as this page. The WalletConnect registry lists the
// app's link as `freighterwallet://wc-redirect`; this is the URL AppKit builds from it. The app pairs with any
// URL containing that prefix plus a `uri=` parameter, and the bare form just brings the app to the front.
const VARIAL_APP = 'freighterwallet://wc-redirect/';
export const varialPairLink = (wcUri) => `${VARIAL_APP}wc?uri=${encodeURIComponent(wcUri)}`;

const reason = (e) => (e && e.message ? e.message : String(e));

/** The QR for a second device, plus the two ways to pair from this phone: the deep link, and the raw
 *  `wc:` URI to paste into Varial's scanner (the app's documented path for mobile browsers). */
function pairingBox(QR, uri) {
  const canvas = el('canvas');
  QR.toCanvas(canvas, uri, { width: 320 }).catch(() => {});
  const raw = el('input', { readOnly: true, value: uri, hidden: true, size: 40 });
  const copy = el('button', { type: 'button', textContent: 'Copy connection link' });
  copy.addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(uri);
      copy.textContent = 'Copied — paste it in Varial’s QR scanner';
    } catch {
      raw.hidden = false;
      raw.select();
    }
  });
  return el('div', {}, canvas,
    el('p', {}, 'On this phone: ', el('a', { href: varialPairLink(uri), textContent: 'Open in Varial' }), ' ', copy),
    raw);
}

/** Freighter browser extension. `address` is the account the extension currently exposes. */
export async function extensionWallet(network) {
  const api = window.freighterApi;
  if (!api || typeof api.signAuthEntry !== 'function') {
    throw new Refuse('Freighter not detected. Install the Freighter extension, then reload this page.');
  }
  const access = typeof api.requestAccess === 'function' ? await api.requestAccess() : null;
  if (access && access.error) throw new Refuse(`Freighter: ${access.error.message || access.error}`);
  const address = access && access.address;
  if (!address) throw new Refuse('Freighter did not share an account.');
  const networkPassphrase = PINNED[network];
  return {
    kind: 'extension',
    address,
    async signAuthEntry(preimageB64, as = address) {
      const res = await api.signAuthEntry(preimageB64, { networkPassphrase, address: as });
      if (res.error) throw new Refuse(res.error.message || String(res.error));
      const sig = res.signedAuthEntry || res.signature;
      if (!sig) throw new Refuse('the extension returned no signature.');
      requireSameAddress(res.signerAddress || as, as);
      return sig;
    },
    async signTransaction(xdrB64, as = address) {
      const res = await api.signTransaction(xdrB64, { networkPassphrase, address: as });
      if (res.error) throw new Refuse(res.error.message || String(res.error));
      if (!res.signedTxXdr) throw new Refuse('the extension returned no signed transaction.');
      requireSameAddress(res.signerAddress || as, as);
      return res.signedTxXdr;
    },
    async close() { /* nothing to close */ },
  };
}

/** Freighter Mobile over WalletConnect. `address` is null when the session does not expose an account. */
export async function mobileWallet(network, onQr) {
  if (!WC_PROJECT_ID) throw new Refuse('Freighter Mobile is not configured for this page (no WalletConnect project id).');
  let provider = null;
  const close = async () => {
    // The session lives in this origin (shared with every templar-protocol.github.io page): end it and
    // clear what it wrote — localStorage keys plus WalletConnect's IndexedDB database — either way.
    try { if (provider && provider.disconnect) await provider.disconnect(); } catch { /* ignore */ }
    try {
      for (const key of wcStorageKeys(Object.keys(localStorage))) localStorage.removeItem(key);
    } catch { /* ignore */ }
    try { indexedDB.deleteDatabase('WALLET_CONNECT_V2_INDEXED_DB'); } catch { /* ignore */ }
    onQr(null);
  };
  try {
    const { UniversalProvider } = await import('./vendor/universal-provider-2.26.0.mjs');
    const QR = (await import('./vendor/qrcode-1.5.4.mjs')).default;
    provider = await UniversalProvider.init({
      projectId: WC_PROJECT_ID,
      metadata: { name: 'Precept admin signing', description: 'Sign admin actions', url: PAGE_ORIGIN, icons: [] },
    });
    provider.on('display_uri', (uri) => onQr(pairingBox(QR, uri),
      'scan this QR code with Varial (Freighter Mobile), or use the buttons below it on this phone'));
    const chain = CHAINS[network];
    const session = await provider.connect({
      namespaces: {
        stellar: { methods: ['stellar_signAuthEntry', 'stellar_signXDR'], chains: [chain], events: ['accountsChanged'] },
      },
    });
    // Requests now wait in the app; on a phone the browser is in front, so offer the way back.
    onQr(el('p', {}, el('a', { href: VARIAL_APP, textContent: 'Open Varial to approve' }), ' (on this phone)'));
    const account = ((session || provider.session || {}).namespaces || {}).stellar?.accounts?.[0];
    const address = account ? account.split(':')[2] : null;   // "stellar:pubnet:G…"
    return {
      kind: 'mobile',
      address,
      async signAuthEntry(preimageB64, as = address) {
        const res = await provider.request(
          { method: 'stellar_signAuthEntry', params: { entryXdr: preimageB64 } }, chain);
        if (!res || !res.signedAuthEntry) throw new Refuse('Freighter Mobile returned no signature.');
        if (as) requireSameAddress(res.signerAddress || as, as);
        return res.signedAuthEntry;
      },
      async signTransaction(xdrB64) {
        const res = await provider.request({ method: 'stellar_signXDR', params: { xdr: xdrB64 } }, chain);
        if (!res || !res.signedXDR) throw new Refuse('Freighter Mobile returned no signed transaction.');
        return res.signedXDR;
      },
      close,
    };
  } catch (e) {
    await close();
    throw e instanceof Refuse ? e : new Refuse(`WalletConnect reported: ${reason(e)}`);
  }
}
