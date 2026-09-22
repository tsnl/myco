'use strict';
const $ = (id) => document.getElementById(id);
let sessions = [];
let refreshing = false;
let createId = null;

function render() {
  const query = $('search').value.trim().toLowerCase();
  const visible = sessions.filter((s) => `${s.title} ${s.model} ${s.id}`.toLowerCase().includes(query));
  const focused = document.activeElement?.getAttribute('href');
  const rows = visible.map((session) => {
    const row = document.createElement('li');
    const link = document.createElement('a'); link.href = `/sessions/${encodeURIComponent(session.id)}`;
    const title = document.createElement('span'); title.className = 'session-name'; title.textContent = session.title;
    const meta = document.createElement('span'); meta.className = 'session-meta';
    const status = document.createElement('span'); status.className = session.status === 'Running' || session.status === 'Background tasks' ? 'session-active' : '';
    status.textContent = session.status;
    const time = document.createElement('time'); time.dateTime = session.updated_at; time.textContent = new Date(session.updated_at).toLocaleString();
    meta.append(status, document.createTextNode(` · ${session.model} · `), time);
    const id = document.createElement('span'); id.className = 'session-id'; id.textContent = session.id;
    link.append(title, meta, id); row.append(link); return row;
  });
  $('session-list').replaceChildren(...rows);
  if (focused) Array.from($('session-list').querySelectorAll('a')).find((link) => link.getAttribute('href') === focused)?.focus({ preventScroll: true });
  $('session-count').textContent = visible.length ? `${visible.length} ${visible.length === 1 ? 'session' : 'sessions'} · Open links in separate tabs to work in parallel.` : query ? 'No matching sessions.' : 'No sessions yet. Start a new session above.';
}
async function refresh() {
  if (refreshing) return;
  refreshing = true;
  try {
    const response = await fetch('/api/sessions');
    if (!response.ok) throw new Error(await response.text());
    sessions = await response.json(); render(); $('error').hidden = true;
  } catch (e) { $('error').textContent = e.message; $('error').hidden = false; }
  finally { refreshing = false; }
}
$('search').oninput = render;
$('new-session').onclick = async () => {
  $('new-session').disabled = true;
  createId ||= crypto.randomUUID();
  try {
    const response = await fetch('/api/sessions', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ request_id: createId }) });
    if (!response.ok) throw new Error(await response.text());
    const session = await response.json();
    location.assign(`/sessions/${encodeURIComponent(session.id)}`);
  } catch (e) { $('error').textContent = e.message; $('error').hidden = false; $('new-session').disabled = false; }
};
window.addEventListener('pageshow', () => { $('new-session').disabled = false; createId = null; refresh(); });
document.addEventListener('visibilitychange', () => { if (!document.hidden) refresh(); });
setInterval(() => { if (!document.hidden) refresh(); }, 5000);
