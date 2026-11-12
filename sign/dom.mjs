// DOM helpers shared by the sign view (app.mjs) and the builder (builder.mjs). Everything that reaches the
// page goes through textContent / createElement: no markup is ever built from data.
export const $ = (id) => document.getElementById(id);

export const setText = (id, msg, cls = '') => { const el = $(id); el.textContent = msg; el.className = cls; };

export function fatal(msg) {
  $('fatal').hidden = false;
  $('fatal').innerHTML = '';
  const box = document.createElement('div');
  box.className = 'guard';
  box.textContent = msg;
  $('fatal').appendChild(box);
}

export function el(tag, props = {}, ...children) {
  const node = document.createElement(tag);
  Object.assign(node, props);
  node.append(...children);
  return node;
}

/** An explorer link to a transaction, opened in a new tab without a referrer or opener. */
export function txLink(explorer, hash) {
  return el('a', { href: `${explorer}/tx/${hash}`, target: '_blank', rel: 'noopener noreferrer', textContent: hash });
}

export async function copyFrom(textareaId, statusId) {
  try {
    await navigator.clipboard.writeText($(textareaId).value);
    $(statusId).textContent = 'copied';
  } catch {
    $(textareaId).select();
    $(statusId).textContent = 'press ⌘/Ctrl-C to copy';
  }
}

let edModule = null;   // @noble/ed25519, loaded lazily
export async function loadEd() {
  if (edModule) return edModule;
  edModule = await import('./vendor/noble-ed25519-3.2.0.mjs');
  edModule.hashes.sha512Async = async (m) => new Uint8Array(await crypto.subtle.digest('SHA-512', m));
  return edModule;
}
