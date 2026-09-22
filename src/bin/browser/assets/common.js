export const $ = (id) => document.getElementById(id);

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
  createId ||= crypto.randomUUID();
  try {
    const session = await (await api('/api/sessions', { request_id: createId })).json();
    location.assign(`/sessions/${encodeURIComponent(session.id)}`);
  } catch (e) { creating = false; $('new-session').disabled = false; error(e.message); }
}
$('new-session').onclick = newSession;
window.addEventListener('pageshow', () => { creating = false; createId = null; $('new-session').disabled = false; });
