import { $, api, element, error, clearError, setArchived } from './common.js';
import { showActivity } from './activity.js';
import { profileName, profilePath, eventsWorker } from './scope.js';

document.title = `Sessions · ${profileName} · myco`;

let sessions = [];
let refreshing = null;
let refreshAgain = false;
let connected = false;
let streamConnected = false;
let eventPort = null;
const rows = new Map();
let archivedSession = null;

function archiveNotice(id) {
  archivedSession = id;
  const url = new URL(location.href); url.searchParams.delete('archived');
  // Keep Undo in this history entry so a refresh cannot lose it or repeat the archive.
  history.replaceState({ ...history.state, archivedSession: id }, '', url);
  $('archive-notice').hidden = !id;
}
$('undo-archive').onclick = async () => {
  const id = archivedSession;
  if (!id) return;
  $('undo-archive').disabled = true; error();
  try {
    await setArchived(id, false);
    if (archivedSession === id) archiveNotice(null);
    $('archive-filter').value = 'active'; $('search').value = '';
    await refresh();
    rows.get(id)?.querySelector('a').focus();
  } catch (e) { error(e.message); }
  finally { $('undo-archive').disabled = false; }
};

function render() {
  showActivity($('connection'), 'Live', false, connected);
  const query = $('search').value.trim().toLowerCase();
  const list = $('session-list');
  const ids = new Set(sessions.map((session) => session.id));
  for (const [id, row] of rows) if (!ids.has(id)) { row.remove(); rows.delete(id); }
  let visible = 0;
  for (const [index, session] of sessions.entries()) {
    let row = rows.get(session.id);
    if (!row) {
      row = element('li');
      const link = element('a'); link.href = profilePath(`/sessions/${encodeURIComponent(session.id)}`);
      link.append(element('span', 'session-name'), element('span', 'session-meta'), element('span', 'session-id', session.id));
      const archive = element('button'); archive.type = 'button';
      archive.onclick = async () => {
        archive.disabled = true; error();
        try {
          const archived = !row.session.archived;
          await setArchived(session.id, archived);
          if (archived) archiveNotice(session.id);
          else if (archivedSession === session.id) archiveNotice(null);
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
      const status = element('span', 'session-status activity-indicator');
      const time = element('time', '', new Date(session.updated_at).toLocaleString()); time.dateTime = session.updated_at;
      row.querySelector('.session-meta').replaceChildren(status, document.createTextNode(` · ${session.model} · `), time);
      const archive = row.querySelector('button');
      archive.textContent = session.archived ? 'Restore' : 'Archive';
      archive.setAttribute('aria-label', `${archive.textContent} ${session.title}`);
    }
    showActivity(row.querySelector('.session-status'), session.status, session.busy, connected);
    if (list.children[index] !== row) list.insertBefore(row, list.children[index] || null);
    row.hidden = !`${session.title} ${session.model} ${session.id}`.toLowerCase().includes(query);
    if (!row.hidden) visible++;
  }
  const empty = $('archive-filter').value === 'archived' ? 'No archived sessions.' : 'No sessions yet. Start a new session above.';
  const summary = visible ? `${visible} ${visible === 1 ? 'session' : 'sessions'} · Open links in separate tabs to work in parallel.` : query ? 'No matching sessions.' : empty;
  if ($('session-count').textContent !== summary) $('session-count').textContent = summary;
}
async function refresh() {
  if (refreshing) { refreshAgain = true; return refreshing; }
  const archived = $('archive-filter').value === 'archived';
  refreshing = (async () => {
    try {
      sessions = await (await api(`/api/sessions?archived=${archived}`)).json();
      connected = streamConnected;
      render(); clearError('refresh');
    } catch (e) { connected = false; render(); error(e.message, 'refresh'); }
  })();
  await refreshing;
  refreshing = null;
  if (refreshAgain || archived !== ($('archive-filter').value === 'archived')) {
    refreshAgain = false;
    return refresh();
  }
}
function connection(value) {
  streamConnected = value;
  if (value) { clearError('connection'); refresh(); return; }
  connected = false;
  render();
}
function connect() {
  connection(false);
  const worker = eventsWorker();
  eventPort = worker.port;
  worker.onerror = () => { connection(false); error('Could not connect to live activity. Reload this page to reconnect.', 'connection'); };
  eventPort.onmessage = ({ data }) => {
    if (data.kind === 'sessions_changed') refresh();
    else if (data.kind === 'connection') connection(data.connected);
  };
  eventPort.postMessage({ kind: 'subscribe_list', profile: profileName });
}
$('search').oninput = render;
$('archive-filter').onchange = refresh;
const archived = new URLSearchParams(location.search).get('archived') || history.state?.archivedSession;
if (/^[a-f0-9]{32}$/.test(archived || '')) archiveNotice(archived);
window.addEventListener('pagehide', () => { eventPort?.postMessage({ kind: 'unsubscribe' }); eventPort?.close(); });
window.addEventListener('pageshow', (event) => { if (event.persisted) connect(); refresh(); });
document.addEventListener('visibilitychange', () => { if (!document.hidden) refresh(); });
setInterval(() => { if (!document.hidden) refresh(); }, 5000);
connect();
