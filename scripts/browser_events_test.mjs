// Exercise the actual SharedWorker protocol with controlled HTTP responses and
// clocks. No browser scheduling or external model determines these assertions.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../src/bin/browser/assets/events.js', import.meta.url), 'utf8');
const settle = () => new Promise(resolve => setImmediate(resolve));
const snapshot = (session_id, revision) => ({ session_id, revision, change: { kind: 'snapshot', snapshot: {} } });

function fixture() {
  const requests = [], timers = new Map();
  let stream, clock = 0, timerId = 0;
  class EventSource {
    static OPEN = 1;
    static CLOSED = 2;
    constructor() { stream = this; this.readyState = 0; }
    close() { this.readyState = EventSource.CLOSED; }
  }
  const context = vm.createContext({
    onconnect: null, EventSource,
    setTimeout(callback, delay) { const id = ++timerId; timers.set(id, { at: clock + delay, callback }); return id; },
    clearTimeout(id) { timers.delete(id); },
    fetch(url) {
      return new Promise((resolve, reject) => requests.push({ url,
        respond: initial => resolve({ ok: true, json: async () => initial }),
        fail: () => reject(new Error('Snapshot unavailable')),
      }));
    },
  });
  vm.runInContext(source, context);
  const send = event => stream.onmessage({ data: JSON.stringify(event) });
  return {
    requests, send,
    update(profile, session_id, revision, kind = 'append') {
      send({ kind: kind === 'refresh' ? 'resync' : 'update', profile,
        update: { session_id, revision, change: { kind, text: `delta ${revision}` } } });
    },
    subscribe(profile, session_id) {
      const port = { messages: [], postMessage(message) { this.messages.push(structuredClone(message)); }, close() {} };
      context.onconnect({ ports: [port] });
      if (stream.readyState === 0) { stream.readyState = EventSource.OPEN; stream.onopen(); }
      port.send = data => port.onmessage({ data });
      port.send({ kind: session_id ? 'subscribe' : 'subscribe_list', profile, session_id });
      return port;
    },
    advance(milliseconds) {
      clock += milliseconds;
      for (const [id, timer] of [...timers]) if (timer.at <= clock) { timers.delete(id); timer.callback(); }
    },
  };
}

test('bursts across 26 sessions coalesce catalog notifications and stop after unsubscribe', () => {
  const f = fixture(), port = f.subscribe('work');
  const changed = () => port.messages.filter(message => message.kind === 'sessions_changed').length;
  for (let index = 0; index < 26; index++) {
    for (const kind of ['meta', 'tasks', 'refresh']) f.update('work', `session-${index}`, 1, kind);
  }
  assert.equal(changed(), 0);
  f.advance(100);
  assert.equal(changed(), 1);
  for (let index = 0; index < 26; index++) f.send({ kind: 'resync', profile: 'work' });
  f.advance(100);
  assert.equal(changed(), 2);
  assert.equal(port.messages.filter(message => message.kind === 'connection').length, 1);
  f.update('work', 'session-0', 2, 'meta');
  port.send({ kind: 'unsubscribe' });
  f.advance(100);
  assert.equal(changed(), 2);
});

test('repeated resyncs allow one fetch in flight and discard stale responses', async () => {
  const f = fixture(), port = f.subscribe('work', 'session');
  for (let index = 0; index < 26; index++) f.send({ kind: 'resync', profile: 'work' });
  assert.equal(f.requests.length, 1);
  f.requests[0].respond(snapshot('session', 1));
  await settle();
  assert.equal(f.requests.length, 2);
  assert.equal(port.messages.filter(message => message.kind === 'snapshot').length, 0);
  f.update('work', 'session', 3);
  f.update('work', 'unobserved-session', 4);
  f.update('personal', 'session', 4);
  f.requests[1].respond(snapshot('session', 2));
  await settle();
  assert.deepEqual(port.messages.filter(message => message.update).map(message => message.update.revision), [2, 3]);
  assert.equal(f.requests.length, 2);
});

test('history invalidation during a fetch is skipped only when the snapshot covers it', async () => {
  const f = fixture(), port = f.subscribe('work', 'session');
  f.update('work', 'session', 5, 'refresh');
  f.requests[0].respond(snapshot('session', 5));
  await settle();
  assert.equal(f.requests.length, 1);
  f.update('work', 'session', 6, 'refresh');
  assert.equal(f.requests.length, 2);
  f.update('work', 'session', 7, 'refresh');
  f.update('work', 'session', 8);
  f.requests[1].respond(snapshot('session', 6));
  await settle();
  assert.equal(f.requests.length, 3);
  f.update('work', 'session', 9);
  f.requests[2].respond(snapshot('session', 8));
  await settle();
  assert.deepEqual(port.messages.filter(message => message.update).map(message => message.update.revision), [5, 8, 9]);
});

test('profile recovery and unseen sessions do not refetch unrelated histories', async () => {
  const f = fixture();
  f.subscribe('work', 'same-id');
  f.subscribe('personal', 'same-id');
  for (const request of f.requests) request.respond(snapshot('same-id', 1));
  await settle();
  f.update('work', 'unseen-id', 2, 'refresh');
  assert.equal(f.requests.length, 2);
  f.send({ kind: 'resync', profile: 'work' });
  assert.equal(f.requests.length, 3);
  assert.equal(f.requests[2].url, '/profiles/work/api/sessions/same-id');
  f.requests[2].respond(snapshot('same-id', 2));
  await settle();
  f.send({ kind: 'resync' });
  assert.equal(f.requests.length, 5);
});

test('unsubscribing during recovery never restarts a fetch or posts the old result', async () => {
  const f = fixture(), port = f.subscribe('work', 'session');
  f.send({ kind: 'resync' });
  port.send({ kind: 'unsubscribe' });
  f.requests[0].respond(snapshot('session', 1));
  await settle();
  assert.equal(f.requests.length, 1);
  assert.equal(port.messages.filter(message => message.update).length, 0);
});

test('failed snapshot recovery buffers deltas and retries only the affected view', async () => {
  const f = fixture(), port = f.subscribe('work', 'session');
  f.subscribe('work', 'other-session');
  f.requests[0].respond(snapshot('session', 1));
  f.requests[1].respond(snapshot('other-session', 1));
  await settle();
  f.update('work', 'session', 2, 'refresh');
  f.requests[2].fail();
  await settle();
  f.update('work', 'session', 3);
  assert.deepEqual(port.messages.filter(message => message.update).map(message => message.update.revision), [1]);
  f.advance(2000);
  assert.equal(f.requests.length, 4);
  assert.equal(f.requests[3].url, '/profiles/work/api/sessions/session');
  f.update('work', 'session', 5);
  f.requests[3].respond(snapshot('session', 4));
  await settle();
  assert.deepEqual(port.messages.filter(message => message.update).map(message => message.update.revision), [1, 4, 5]);
  assert.equal(port.messages.at(-1).connected, true);
});

test('changing a subscription during recovery fetches the new profile and session', async () => {
  const f = fixture(), port = f.subscribe('work', 'old');
  port.send({ kind: 'subscribe', profile: 'personal', session_id: 'new' });
  f.requests[0].respond(snapshot('old', 1));
  await settle();
  assert.equal(f.requests.length, 2);
  assert.equal(f.requests[1].url, '/profiles/personal/api/sessions/new');
  f.requests[1].respond(snapshot('new', 2));
  await settle();
  assert.deepEqual(port.messages.filter(message => message.update).map(message => message.update.session_id), ['new']);
});
