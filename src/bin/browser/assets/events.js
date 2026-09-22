'use strict';
// One stream per browser, so tab groups do not exhaust HTTP/1 connection slots.
const clients = new Set();
let stream = null;
let retry = null;
function connection(connected) {
  for (const client of clients) client.port.postMessage({ kind: 'connection', connected });
}
async function load(client) {
  if (!client.requested) return;
  const version = ++client.version;
  client.pending = [];
  client.port.postMessage({ kind: 'connection', connected: false });
  try {
    const response = await fetch(`/api/sessions/${encodeURIComponent(client.requested)}`);
    if (!response.ok) throw new Error(await response.text());
    const initial = await response.json();
    if (!clients.has(client) || version !== client.version) return;
    client.id = initial.session_id;
    client.port.postMessage({ kind: 'snapshot', update: initial });
    for (const update of client.pending) if (update.session_id === client.id) client.port.postMessage({ kind: 'update', update });
    client.pending = null;
    client.port.postMessage({ kind: 'connection', connected: stream.readyState === EventSource.OPEN });
  } catch (e) {
    if (clients.has(client) && version === client.version) {
      client.pending = null;
      client.port.postMessage({ kind: 'error', message: e.message });
    }
  }
}
function openStream() {
  stream = new EventSource('/api/events');
  stream.onopen = () => { for (const client of clients) load(client); };
  stream.onerror = () => {
    connection(false);
    // Authentication changes after a server restart. Retry once the launch URL signs in again.
    if (stream.readyState === EventSource.CLOSED) retry = setTimeout(openStream, 2000);
  };
  stream.onmessage = ({ data }) => {
    const update = JSON.parse(data);
    for (const client of clients) {
      if (client.pending) client.pending.push(update);
      else if (client.id === update.session_id) client.port.postMessage({ kind: 'update', update });
    }
  };
}
onconnect = ({ ports: [port] }) => {
  const client = { port, id: null, requested: null, pending: [], version: 0 };
  clients.add(client);
  if (!stream) openStream();
  port.onmessage = ({ data }) => {
    if (data.kind === 'unsubscribe') {
      clients.delete(client); port.close();
      if (!clients.size) { clearTimeout(retry); stream.close(); stream = null; }
      return;
    }
    if (data.kind !== 'subscribe') return;
    client.requested = data.session_id;
    load(client);
  };
};
