'use strict';
// One browser connection serves every profile. A session ID is meaningful only
// together with its profile, including for snapshots buffered during reconnects.
const clients = new Set();
const reconnecting = new Map();
const available = new Map();
let stream = null;
let retry = null;
const ready = client => stream?.readyState === EventSource.OPEN && available.get(client.profile) !== false;
function connection(connected, profile) {
  for (const client of clients) if (!profile || client.profile === profile) {
    client.port.postMessage({ kind: 'connection', connected });
  }
}
async function load(client) {
  if (!clients.has(client) || !client.profile) return;
  if (client.list) {
    client.port.postMessage({ kind: 'sessions_changed' });
    client.port.postMessage({ kind: 'connection', connected: ready(client) });
    return;
  }
  if (!client.requested) return;
  const version = ++client.version;
  client.pending = [];
  client.port.postMessage({ kind: 'connection', connected: false });
  try {
    const response = await fetch(`/profiles/${encodeURIComponent(client.profile)}/api/sessions/${encodeURIComponent(client.requested)}`);
    if (!response.ok) throw new Error(await response.text());
    const initial = await response.json();
    if (!clients.has(client) || version !== client.version) return;
    client.id = initial.session_id;
    client.port.postMessage({ kind: 'snapshot', update: initial });
    for (const update of client.pending) if (update.session_id === client.id) client.port.postMessage({ kind: 'update', update });
    client.pending = null;
    client.port.postMessage({ kind: 'connection', connected: ready(client) });
  } catch (e) {
    if (clients.has(client) && version === client.version) {
      client.pending = null;
      client.port.postMessage({ kind: 'error', message: e.message });
      reloadProfile(client.profile);
    }
  }
}
function profileConnection(profile, connected) {
  available.set(profile, connected);
  if (connected) {
    clearTimeout(reconnecting.get(profile));
    reconnecting.delete(profile);
    for (const client of clients) if (client.profile === profile) load(client);
  } else {
    connection(false, profile);
    reloadProfile(profile);
  }
}
function reloadProfile(profile) {
  if (reconnecting.has(profile)) return;
  reconnecting.set(profile, setTimeout(() => {
    reconnecting.delete(profile);
    for (const client of clients) if (client.profile === profile) load(client);
  }, 2000));
}
function openStream() {
  stream = new EventSource('/api/profile-events');
  stream.onopen = () => { available.clear(); for (const client of clients) load(client); };
  stream.onerror = () => {
    connection(false);
    // Some failures close EventSource instead of scheduling its normal reconnect.
    if (stream.readyState === EventSource.CLOSED) retry = setTimeout(openStream, 2000);
  };
  stream.onmessage = ({ data }) => {
    const event = JSON.parse(data);
    if (event.kind === 'connection') { profileConnection(event.profile, event.connected); return; }
    if (event.kind === 'resync') { for (const client of clients) load(client); return; }
    const update = event.update;
    if (!update) return;
    for (const client of clients) {
      if (client.profile !== event.profile) continue;
      if (client.list) {
        if (['snapshot', 'meta', 'tasks'].includes(update.change.kind)) client.port.postMessage({ kind: 'sessions_changed' });
      } else if (client.pending) client.pending.push(update);
      else if (client.id === update.session_id) client.port.postMessage({ kind: 'update', update });
    }
  };
}
onconnect = ({ ports: [port] }) => {
  const client = { port, profile: null, id: null, requested: null, list: false, pending: [], version: 0 };
  clients.add(client);
  if (!stream) openStream();
  port.onmessage = ({ data }) => {
    if (data.kind === 'unsubscribe') {
      clients.delete(client); port.close();
      if (!clients.size) {
        clearTimeout(retry); stream.close(); stream = null;
        for (const timer of reconnecting.values()) clearTimeout(timer);
        reconnecting.clear();
        available.clear();
      }
      return;
    }
    if (!['subscribe', 'subscribe_list'].includes(data.kind) || !/^[A-Za-z0-9_-]+$/.test(data.profile)) return;
    client.profile = data.profile;
    client.list = data.kind === 'subscribe_list';
    if (client.list) client.pending = null;
    client.requested = data.session_id;
    load(client);
  };
};
