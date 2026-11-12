// Builder view (no `#b=` in the URL): the emergency console. A short pinned list of actions, each shown
// in plain words with the chain's current state next to it. What gets signed is decided by flows.mjs
// and decode.mjs; this file only renders rows, runs one flow at a time and reports the outcome.
import { EMERGENCY } from './actions.mjs';
import { Refuse } from './decode.mjs';
import { $, copyFrom, el, loadEd, setText, txLink } from './dom.mjs';
import * as flows from './flows.mjs';
import * as build from './build.mjs';
import * as rpc from './rpc.mjs';
import { WC_PROJECT_ID, extensionWallet, mobileWallet } from './wallets.mjs';

const STATUS_WORDS = ['Active (admin)', 'Active', 'On-Ice (admin)', 'On-Ice', 'Frozen (admin)', 'Frozen', 'Setup'];
const status = (msg, cls = '') => setText('b-status', msg, cls);

let net = 'mainnet';
let wallet = null;
let me = null;
let busy = false;
let live = { pools: {}, assets: {} };   // pool name -> status | '?', asset id -> tripped | '?'
let generation = 0;                     // drops reads that finished after a network switch
let lastTrip = null;                    // { asset, tripped }: what the "Execute proposal" retry needs

const pin = () => EMERGENCY[net];
const practice = () => $('practice').checked;
const known = (v) => v !== undefined && v !== '?';

// ---- wallet -----------------------------------------------------------------------------------

async function closeWallet() {
  const w = wallet;
  wallet = null;
  me = null;
  if (w) await w.close();
  await refreshMe();
}

const showQr = (canvas) => {
  $('b-qr').replaceChildren(...(canvas ? [canvas] : []));
  if (canvas) status('scan this QR code with Freighter Mobile');
};

async function connect(open) {
  if (busy) return;
  busy = true;
  try {
    await closeWallet();
    const w = await open();
    if (!w.address) {
      await w.close();
      throw new Refuse('Freighter Mobile did not share your account — use the Freighter extension instead.');
    }
    if (!pin().signers.includes(w.address)) {
      await w.close();
      throw new Refuse(`This wallet account (${w.address}) is not one of the admin signers — switch account in Freighter.`);
    }
    wallet = w;
    me = w.address;
    status('Connected.', 'ok');
    await refreshMe();
  } catch (e) {
    status(e instanceof Refuse ? e.message : `could not connect: ${e && e.message ? e.message : e}`, 'err');
  } finally {
    busy = false;
  }
}

async function refreshMe() {
  const box = $('me');
  if (!me) {
    box.textContent = 'Not connected.';
    return;
  }
  const who = me;
  const yesNo = (p) => p.then((v) => (v ? 'yes' : 'no')).catch(() => 'unknown');
  const lines = await Promise.all([
    ...pin().pools.map(async (pool) => `${pool.name} — on pause rule: ${await yesNo(rpc.ruleHasSigner(net, pool.pauseRule, who))}`),
    (async () => `ManualTripper: ${await yesNo(rpc.hasRole(net, who, 'ManualTripper'))}`)(),
  ]);
  if (who !== me) return;
  box.replaceChildren(el('div', {}, `connected: ${who}`), ...lines.map((l) => el('div', {}, l)));
}

// ---- state and rows ---------------------------------------------------------------------------

async function loadState() {
  const mine = ++generation;
  live = { pools: {}, assets: {} };
  render();
  const p = pin();
  await Promise.all([
    ...p.pools.map(async (pool) => { live.pools[pool.name] = await rpc.poolStatus(net, pool.id).catch(() => '?'); }),
    ...p.assets.map(async (a) => { live.assets[a.id] = await rpc.isTripped(net, a.id).catch(() => '?'); }),
  ]);
  if (mine === generation) render();
}

function rowSpecs() {
  const p = pin();
  const specs = [];
  for (const pool of p.pools) {
    const st = live.pools[pool.name];
    const state = known(st) ? (STATUS_WORDS[st] ?? `status ${st}`) : 'state unknown';
    const onIce = {
      state,
      effect: `New borrowing on ${pool.name} stops. Supplying, withdrawing, repaying and liquidations keep working.`,
      disabled: st === 2 || st === 4 || st === 6,
    };
    specs.push({
      ...onIce, title: `On-Ice ${pool.name}`, who: 'you alone',
      run: () => flows.soloOnIce({ network: net, pool, me, wallet, ed: ED, ui, practice: practice() }).then(reportSubmit),
    });
    specs.push({
      ...onIce, title: `On-Ice ${pool.name} via 2-of-3`, who: 'needs a second signer', pair: true,
      run: () => pair(`On-Ice ${pool.name}`, build.onIceCall(pool.id)),
    });
    specs.push({
      state, title: `Back to Active ${pool.name}`, who: 'needs a second signer', pair: true,
      effect: `Borrowing on ${pool.name} is allowed again. The pool refuses this if the backstop is under its `
        + 'threshold or half of it is queued for withdrawal.',
      disabled: st === 0 || st === 4 || st === 6,
      run: () => pair(`Back to Active ${pool.name}`, build.activeCall(pool.id)),
    });
  }
  for (const asset of p.assets) {
    const t = live.assets[asset.id];
    const state = known(t) ? (t ? 'tripped' : 'normal') : 'state unknown';
    specs.push({
      state, title: `Trip ${asset.name}`, who: 'you alone', disabled: t === true,
      effect: `The oracle stops serving ${asset.name}. Borrowing, collateral withdrawals and liquidations that need it fail in: ${asset.pools.join(', ')}.`,
      run: () => trip(asset, true),
    });
    specs.push({
      state, title: `Untrip ${asset.name}`, who: 'you alone', disabled: t === false,
      effect: `${asset.name} prices come back at the next price refresh.`,
      run: () => trip(asset, false),
    });
  }
  return specs;
}

function render() {
  $('practice-note').textContent = practice() ? 'Practice covers the one-person actions.' : '';
  $('rows').replaceChildren(...rowSpecs().map((s) => {
    const button = el('button', { type: 'button', textContent: s.title, disabled: s.disabled || (practice() && s.pair) });
    button.addEventListener('click', () => run(s.run));
    return el('div', { className: 'row' },
      el('h3', {}, `${s.title} — ${s.who}`),
      el('p', {}, s.effect),
      el('p', { className: 'muted' }, `now: ${s.state}`),
      button);
  }));
}

// ---- flows ------------------------------------------------------------------------------------

let ED = null;

const ui = {
  status: (m) => status(m),
  confirm(derived, title) {
    $('confirm-title').textContent = title;
    $('confirm-action').textContent = derived.action.join('\n');
    $('confirm-expiry').textContent = `${derived.expiry} · ${derived.expiryHuman}`;
    $('confirm-payload').textContent = derived.payloadHex;
    $('confirm').hidden = false;
    return new Promise((resolve) => {
      const done = (v) => {
        $('confirm').hidden = true;
        $('confirm-sign').onclick = null;
        $('confirm-cancel').onclick = null;
        resolve(v);
      };
      $('confirm-sign').onclick = () => done(true);
      $('confirm-cancel').onclick = () => done(false);
    });
  },
};

function reportSubmit(out) {
  if (out.practice) {
    status(`Practice: simulation OK (resource fee ${out.resourceFee} stroops). Nothing was sent.`, 'ok');
    return;
  }
  status(`Done. Pool status is now ${STATUS_WORDS[out.status] ?? out.status}.`, out.status === 2 ? 'ok' : 'err');
  $('result').replaceChildren('transaction ', txLink(pin().explorer, out.hash));
}

async function pair(title, call) {
  const out = await flows.twoSignerBuild({
    network: net, call, title, me, wallet, ed: ED, ui, baseUrl: location.origin + location.pathname,
  });
  $('handoff').value = out.message;
  $('own-share').value = out.share;
  $('handoff-box').hidden = false;
  status('Signed. Send the message below to the second signer.', 'ok');
}

async function trip(asset, tripped) {
  lastTrip = { asset, tripped };
  status('Freighter will ask you twice (create the proposal, then execute it).');
  const out = await flows.tripFlow({ network: net, asset, tripped, me, wallet, ui, practice: practice() });
  if (out.practice) {
    status(`Practice: ${out.note}`, 'ok');
    return;
  }
  const ok = out.tripped === tripped;
  status(`Done. ${asset.name} is now ${out.tripped ? 'tripped' : 'normal'}.`, ok ? 'ok' : 'err');
  $('result').replaceChildren('transaction ', txLink(pin().explorer, out.hash));
}

function offerRetry(id) {
  const button = $('retry-exec');
  button.textContent = `Execute proposal ${id}`;
  button.hidden = false;
  button.onclick = () => run(async () => {
    button.hidden = true;
    const { asset, tripped } = lastTrip;
    const out = await flows.executeProposal({ network: net, asset, id, me, wallet, ui });
    status(`Done. ${asset.name} is now ${out.tripped ? 'tripped' : 'normal'}.`, out.tripped === tripped ? 'ok' : 'err');
    $('result').replaceChildren('transaction ', txLink(pin().explorer, out.hash));
  });
}

/** One flow at a time; a mobile session ends with the flow (it reconnects per flow). */
async function run(fn) {
  if (busy) return;
  busy = true;
  $('result').replaceChildren();
  $('retry-exec').hidden = true;
  $('handoff-box').hidden = true;
  status('');
  try {
    if (!wallet) throw new Refuse('Connect Freighter first.');
    ED = await loadEd();
    await fn();
  } catch (e) {
    if (e instanceof flows.Cancelled) status('Cancelled.');
    else if (e instanceof Refuse) {
      status(e.message, 'err');
      if (e.proposalId !== undefined && lastTrip) offerRetry(e.proposalId);
    } else status(`failed: ${e && e.message ? e.message : e}`, 'err');
  } finally {
    busy = false;
    if (wallet && wallet.kind === 'mobile') await closeWallet();
    loadState();
  }
}

// ---- wiring -----------------------------------------------------------------------------------

export function startBuilder() {
  $('builder').hidden = false;
  if (EMERGENCY.testnet) $('net-testnet').hidden = false;
  if (!WC_PROJECT_ID) $('b-mob').disabled = true;
  $('b-ext').addEventListener('click', () => connect(() => {
    status('asking Freighter for access to this site…');
    return extensionWallet(net);
  }));
  $('b-mob').addEventListener('click', () => connect(() => {
    status('opening WalletConnect…');
    return mobileWallet(net, showQr);
  }));
  $('refresh').addEventListener('click', () => { loadState(); refreshMe(); });
  $('practice').addEventListener('change', render);
  $('copy-msg').addEventListener('click', () => copyFrom('handoff', 'copy-msg-status'));
  for (const radio of document.querySelectorAll('input[name=net]')) {
    radio.addEventListener('change', async () => {
      if (!radio.checked) return;
      await closeWallet();
      net = radio.value;
      status('');
      loadState();
    });
  }
  addEventListener('pagehide', () => { closeWallet().catch(() => {}); });
  refreshMe();
  loadState();
}
