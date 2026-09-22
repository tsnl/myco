export const $ = (id) => document.getElementById(id);

// Request ids deduplicate retries, so they must be unique, not unguessable.
// `crypto.randomUUID` exists only in a secure context, which plain HTTP to
// anything but localhost is not — and that is how `--web-bind` is reached.
export const requestId = () =>
  crypto.randomUUID?.() ??
  '10000000-1000-4000-8000-100000000000'.replace(/[018]/g, (c) =>
    (c ^ (crypto.getRandomValues(new Uint8Array(1))[0] & (15 >> (c / 4)))).toString(16),
  );

export function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}
export function error(message = '') { $('error').textContent = message; $('error').hidden = !message; }
export async function api(path, body) {
  const response = await fetch(path, body === undefined ? {} : { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
  if (!response.ok) throw new Error(await response.text());
  return response;
}

let createId = null;
let creating = false;
export async function newSession() {
  if (creating) return;
  creating = true; $('new-session').disabled = true;
  createId ||= requestId();
  try {
    const session = await (await api('/api/sessions', { request_id: createId })).json();
    location.assign(`/sessions/${encodeURIComponent(session.id)}`);
  } catch (e) { creating = false; $('new-session').disabled = false; error(e.message); }
}
$('new-session').onclick = newSession;
window.addEventListener('pageshow', () => { creating = false; createId = null; $('new-session').disabled = false; });
