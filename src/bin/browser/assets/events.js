'use strict';
// One browser connection serves every profile. A session ID is meaningful only
// together with its profile, including for snapshots buffered during reconnects.
const clients = new Set();
const reconnecting = new Map();
const available = new Map();
let stream = null;
let retry = null;
const ready = client => stream?.readyState === EventSource.OPEN && available.get(client.profile) !== false;
function clientConnection(client, connected) {
  if (client.connected === connected) return;
  client.connected = connected;
  client.port.postMessage({ kind: 'connection', connected });
}
function connection(connected, profile) {
  for (const client of clients) if (!profile || client.profile === profile) {
    clientConnection(client, connected);
  }
}
function listChanged(client) {
  if (client.listTimer) return;
  client.listTimer = setTimeout(() => {
    client.listTimer = null;
    if (clients.has(client)) client.port.postMessage({ kind: 'sessions_changed' });
  }, 100);
}
async function load(client) {
  if (!clients.has(client) || !client.profile) return;
  clearTimeout(client.retry);
  client.retry = null;
  const version = ++client.version;
  if (client.list) {
    listChanged(client);
    clientConnection(client, ready(client));
    return;
  }
  if (!client.requested || client.loading) return;
  client.loading = true;
  client.pending = [];
  clientConnection(client, false);
  try {
    const response = await fetch(`/profiles/${encodeURIComponent(client.profile)}/api/sessions/${encodeURIComponent(client.requested)}`);
    if (!response.ok) throw new Error(await response.text());
    const initial = await response.json();
    if (!clients.has(client) || version !== client.version) return;
    const pending = client.pending.filter(update => update.session_id === initial.session_id && update.revision > initial.revision);
    // A replacement history newer than this response invalidates buffered block
    // indices. Fetch it before applying any more deltas.
    if (pending.some(update => update.change.kind === 'refresh')) { client.version++; return; }
    client.id = initial.session_id;
    client.port.postMessage({ kind: 'snapshot', update: initial });
    for (const update of pending) client.port.postMessage({ kind: 'update', update });
    client.pending = null;
    clientConnection(client, ready(client));
  } catch (e) {
    if (clients.has(client) && version === client.version) {
      // Keep deltas buffered until a successful snapshot restores block indices.
      client.pending = [];
      client.port.postMessage({ kind: 'error', message: e.message });
      client.retry = setTimeout(() => load(client), 2000);
    }
  } finally {
    client.loading = false;
    if (clients.has(client) && version !== client.version) load(client);
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
    if (event.kind === 'resync' && !event.update) {
      for (const client of clients) if (!event.profile || client.profile === event.profile) load(client);
      return;
    }
    const update = event.update;
    if (!update) return;
    for (const client of clients) {
      if (client.profile !== event.profile) continue;
      if (client.list) {
        if (['refresh', 'snapshot', 'meta', 'tasks'].includes(update.change.kind)) listChanged(client);
      } else if (client.pending) {
        if (client.requested && update.session_id.startsWith(client.id || client.requested)) client.pending.push(update);
      } else if (client.id === update.session_id) {
        if (update.change.kind === 'refresh') load(client);
        else client.port.postMessage({ kind: 'update', update });
      }
    }
  };
}
onconnect = ({ ports: [port] }) => {
  const client = { port, profile: null, id: null, requested: null, list: false, pending: [], version: 0, loading: false, connected: null, listTimer: null, retry: null };
  clients.add(client);
  if (!stream) openStream();
  port.onmessage = ({ data }) => {
    if (data.kind === 'unsubscribe') {
      clients.delete(client); clearTimeout(client.listTimer); clearTimeout(client.retry); port.close();
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
    client.id = null;
    client.list = data.kind === 'subscribe_list';
    if (client.list) client.pending = null;
    client.requested = data.session_id;
    load(client);
  };
};
