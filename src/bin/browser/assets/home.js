import { $, api, element, error } from '/common.js';

let sessions = [];
let refreshing = null;
const rows = new Map();

function render() {
  const query = $('search').value.trim().toLowerCase();
  const list = $('session-list');
  const ids = new Set(sessions.map((session) => session.id));
  for (const [id, row] of rows) if (!ids.has(id)) { row.remove(); rows.delete(id); }
  let visible = 0;
  for (const [index, session] of sessions.entries()) {
    let row = rows.get(session.id);
    if (!row) {
      row = element('li');
      const link = element('a'); link.href = `/sessions/${encodeURIComponent(session.id)}`;
      link.append(element('span', 'session-name'), element('span', 'session-meta'), element('span', 'session-id', session.id));
      const archive = element('button'); archive.type = 'button';
      archive.onclick = async () => {
        archive.disabled = true;
        try {
          await api(`/api/sessions/${encodeURIComponent(session.id)}/archive`, { session_id: session.id, archived: !row.session.archived });
          if (refreshing) await refreshing;
          await refresh();
        } catch (e) { error(e.message); }
        finally { archive.disabled = false; }
      };
      row.append(link, archive); rows.set(session.id, row);
    }
    if (JSON.stringify(row.session) !== JSON.stringify(session)) {
      row.session = session;
      row.querySelector('.session-name').textContent = session.title;
      const status = element('span', session.status === 'Running' || session.status === 'Background tasks' ? 'session-active' : '', session.status);
      const time = element('time', '', new Date(session.updated_at).toLocaleString()); time.dateTime = session.updated_at;
      row.querySelector('.session-meta').replaceChildren(status, document.createTextNode(` · ${session.model} · `), time);
      const archive = row.querySelector('button');
      archive.textContent = session.archived ? 'Restore' : 'Archive';
      archive.setAttribute('aria-label', `${archive.textContent} ${session.title}`);
    }
    if (list.children[index] !== row) list.insertBefore(row, list.children[index] || null);
    row.hidden = !`${session.title} ${session.model} ${session.id}`.toLowerCase().includes(query);
    if (!row.hidden) visible++;
  }
  const empty = $('archive-filter').value === 'archived' ? 'No archived sessions.' : 'No sessions yet. Start a new session above.';
  $('session-count').textContent = visible ? `${visible} ${visible === 1 ? 'session' : 'sessions'} · Open links in separate tabs to work in parallel.` : query ? 'No matching sessions.' : empty;
}
async function refresh() {
  if (refreshing) return refreshing;
  const archived = $('archive-filter').value === 'archived';
  refreshing = (async () => {
    try {
      sessions = await (await api(`/api/sessions?archived=${archived}`)).json();
      render(); error();
    } catch (e) { error(e.message); }
  })();
  await refreshing;
  refreshing = null;
  if (archived !== ($('archive-filter').value === 'archived')) return refresh();
}
$('search').oninput = render;
$('archive-filter').onchange = refresh;
window.addEventListener('pageshow', refresh);
document.addEventListener('visibilitychange', () => { if (!document.hidden) refresh(); });
setInterval(() => { if (!document.hidden) refresh(); }, 5000);
