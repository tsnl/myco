export const $ = (id) => document.getElementById(id);

// Request ids deduplicate retries, so they must be unique, not unguessable.
// Keep the fallback for browsers that do not expose randomUUID.
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

export function newSession() {
  window.open('/new', '_blank', 'noopener');
}
