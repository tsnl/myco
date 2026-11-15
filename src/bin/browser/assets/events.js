'use strict';
// One browser connection serves every profile. A session ID is meaningful only
// together with its profile, including for snapshots buffered during reconnects.
const clients = new Set();
const reconnecting = new Map();
const available = new Map();
let stream = null;
let retry = null;
// A hidden session view keeps its own deltas while the stream stays whole, so
// showing it again needs no snapshot. A longer absence costs one snapshot.
const MAX_AWAY = 1000;
const ready = client => stream?.readyState === EventSource.OPEN && available.get(client.profile) !== false;
// A refresh with a splice carries its replacement; only a bare one needs a fetch.
const invalidates = update => update.change.kind === 'refresh' && !update.change.splice;
function forgetAway(profile) {
  for (const client of clients) if (!profile || client.profile === profile) client.away = null;
}
function awayUpdate(client, update) {
  const away = client.away, last = away.at(-1), change = update.change;
  if (invalidates(update) || away.length >= MAX_AWAY) { client.away = null; return; }
  // Streamed text arrives as many small appends to one block; keep one.
  if (change.kind === 'append' && last?.change.kind === 'append' && last.change.index === change.index) {
    away[away.length - 1] = { ...update, change: { ...change, text: last.change.text + change.text } };
  } else away.push(update);
}
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
  if (!client.visible || client.listTimer) return;
  client.listTimer = setTimeout(() => {
    client.listTimer = null;
    if (clients.has(client)) client.port.postMessage({ kind: 'sessions_changed' });
  }, 100);
}
async function load(client) {
  if (!clients.has(client) || !client.profile || !client.visible) return;
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
    // indices unless it carries its own splice. Fetch it before applying more.
    if (pending.some(invalidates)) { client.version++; return; }
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
  forgetAway(profile);
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
  stream.onopen = () => { available.clear(); forgetAway(); for (const client of clients) load(client); };
  stream.onerror = () => {
    forgetAway();
    connection(false);
    // Some failures close EventSource instead of scheduling its normal reconnect.
    if (stream.readyState === EventSource.CLOSED) retry = setTimeout(openStream, 2000);
  };
  stream.onmessage = ({ data }) => {
    const event = JSON.parse(data);
    if (event.kind === 'connection') { profileConnection(event.profile, event.connected); return; }
    if (event.kind === 'resync' && !event.update) {
      // Missing events may include a profile's reconnect notification.
      if (event.profile) available.delete(event.profile);
      else available.clear();
      forgetAway(event.profile);
      for (const client of clients) if (!event.profile || client.profile === event.profile) load(client);
      return;
    }
    const update = event.update;
    if (!update) return;
    for (const client of clients) {
      if (client.profile !== event.profile) continue;
      if (!client.visible) {
        if (client.away && client.id === update.session_id) awayUpdate(client, update);
        continue;
      }
      if (client.list) {
        if (['refresh', 'meta'].includes(update.change.kind)) listChanged(client);
      } else if (client.pending) {
        if (client.requested && update.session_id.startsWith(client.id || client.requested)) client.pending.push(update);
      } else if (client.id === update.session_id) {
        if (invalidates(update)) load(client);
        else client.port.postMessage({ kind: 'update', update });
      }
    }
  };
}
onconnect = ({ ports: [port] }) => {
  const client = { port, profile: null, id: null, requested: null, list: false, visible: true, pending: [], away: null, version: 0, loading: false, connected: null, listTimer: null, retry: null };
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
    if (data.kind === 'visibility') {
      if (typeof data.visible !== 'boolean' || client.visible === data.visible) return;
      client.visible = data.visible;
      clearTimeout(client.listTimer); client.listTimer = null;
      clearTimeout(client.retry); client.retry = null;
      if (!client.visible) {
        // Only a current, connected view can resume from its own deltas. Any
        // other hidden view restores block indices from a fresh snapshot.
        client.away = !client.list && client.pending === null && client.connected ? [] : null;
        client.version++;
        client.pending = client.list ? null : [];
        return;
      }
      const away = client.away;
      client.away = null;
      if (away && ready(client)) {
        client.pending = null;
        for (const update of away) client.port.postMessage({ kind: 'update', update });
        return;
      }
      load(client);
      return;
    }
    if (!['subscribe', 'subscribe_list'].includes(data.kind) || !/^[A-Za-z0-9_-]+$/.test(data.profile)) return;
    client.profile = data.profile;
    client.id = null;
    client.away = null;
    client.list = data.kind === 'subscribe_list';
    client.visible = data.visible !== false;
    if (client.list) client.pending = null;
    client.requested = data.session_id;
    load(client);
  };
};
