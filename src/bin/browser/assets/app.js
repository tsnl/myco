import { $, api, element, error, newSession, requestId } from '/common.js';
import { showActivity } from '/activity.js';
const transcript = $('transcript');
let state = { blocks: [], tasks: [], busy: false };
let connected = false;
let revision = -1;
let follow = true;
let pending = null;
let selectingModel = false;
let sending = false;
let eventPort = null;
const sessionId = decodeURIComponent(location.pathname.slice('/sessions/'.length));
const nodes = [];
const markdownJobs = new WeakMap();
const toolClocks = new WeakMap();

function updateToolDuration(node, now = performance.now()) {
  const elapsed = Number(node.dataset.elapsed) + (node.dataset.running === 'true' ? now - Number(node.dataset.observed) : 0);
  const text = `${(Math.floor(Math.max(0, elapsed) / 100) / 10).toFixed(1)}s`;
  if (node.textContent !== text) node.textContent = text;
}
function toolDuration(block) {
  const node = element('span', 'tool-duration');
  node.hidden = !Number.isFinite(block.elapsed_ms);
  if (node.hidden) return node;
  let clock = toolClocks.get(block);
  if (!clock) { clock = { elapsed: block.elapsed_ms, observed: performance.now() }; toolClocks.set(block, clock); }
  node.dataset.elapsed = clock.elapsed;
  node.dataset.observed = clock.observed;
  node.dataset.running = String(block.running);
  node.title = block.running ? 'Tool execution time' : 'Total tool execution time';
  updateToolDuration(node);
  return node;
}
function updateToolDurations() {
  const now = performance.now();
  for (const node of document.querySelectorAll('.tool-duration[data-running="true"]')) updateToolDuration(node, now);
}
setInterval(updateToolDurations, 100);
document.addEventListener('visibilitychange', updateToolDurations);

function scrollLatest() { requestAnimationFrame(() => { if (follow) window.scrollTo({ top: document.documentElement.scrollHeight }); }); }
window.addEventListener('scroll', () => {
  follow = document.documentElement.scrollHeight - window.scrollY - window.innerHeight < 100;
  $('jump').hidden = follow;
}, { passive: true });
$('jump').onclick = () => { follow = true; scrollLatest(); };
new ResizeObserver(() => {
  document.documentElement.style.setProperty('--composer-height', `${$('composer').offsetHeight}px`);
  scrollLatest();
}).observe($('composer'));
function resizeInput() { $('prompt').style.height = 'auto'; $('prompt').style.height = `${$('prompt').scrollHeight}px`; }
$('prompt').addEventListener('input', resizeInput);
$('activity-toggle').onclick = () => {
  $('activity').showModal();
  $('activity-toggle').setAttribute('aria-expanded', 'true');
};
$('activity-close').onclick = () => $('activity').close();
$('activity').addEventListener('close', () => $('activity-toggle').setAttribute('aria-expanded', 'false'));
$('activity').addEventListener('click', (event) => {
  if (event.target !== $('activity')) return;
  const rect = $('activity').getBoundingClientRect();
  if (event.clientX < rect.left || event.clientX > rect.right || event.clientY < rect.top || event.clientY > rect.bottom) $('activity').close();
});

function imageSource(source) {
  if (/^https?:\/\//.test(source) || /^data:image\/(png|jpeg|gif|webp);base64,/.test(source)) return source;
  if (!source.startsWith('myco-image:') && /^[A-Za-z0-9+/]+=*$/.test(source)) return `data:image/png;base64,${source}`;
  return `/api/image?source=${encodeURIComponent(source)}`;
}
function addImages(parent, sources) {
  for (const source of sources || []) {
    const img = element('img'); img.src = imageSource(source); img.alt = 'Image from the conversation'; img.loading = 'lazy'; img.referrerPolicy = 'no-referrer';
    img.addEventListener('load', scrollLatest);
    img.addEventListener('error', () => { img.alt = 'Image unavailable'; });
    parent.append(img);
  }
}
function markdown(node, text) {
  let job = markdownJobs.get(node);
  if (!job) { job = { text: null, pending: false, failed: false }; markdownJobs.set(node, job); }
  if (job.text === text && !job.failed) return;
  job.text = text;
  if (!node.hasChildNodes()) { node.textContent = text; node.classList.add('pending'); }
  if (job.pending) return;
  job.pending = true;
  const render = async () => {
    if (!node.isConnected) { job.pending = false; return; }
    const text = job.text;
    job.failed = false;
    try {
      const html = await (await api('/api/markdown', { text })).text();
      if (!node.isConnected || text !== job.text) return;
      node.innerHTML = html;
      node.classList.remove('pending');
      for (const link of node.querySelectorAll('a')) { link.target = '_blank'; link.rel = 'noopener noreferrer'; }
      for (const img of node.querySelectorAll('img')) { img.loading = 'lazy'; img.referrerPolicy = 'no-referrer'; img.addEventListener('load', scrollLatest); }
      scrollLatest();
    } catch (e) { job.failed = true; node.textContent = job.text; node.classList.add('pending'); }
    finally {
      if (node.isConnected && text !== job.text) setTimeout(render, 80);
      else job.pending = false;
    }
  };
  setTimeout(render, 80);
}
function argumentValue(value) {
  return typeof value === 'string' ? value : JSON.stringify(value, null, 2);
}
function argumentEntries(input) {
  return input !== null && typeof input === 'object' && !Array.isArray(input) ? Object.entries(input) : [['value', input]];
}
function toolArguments(input) {
  const fields = element('dl', 'arguments');
  for (const [key, value] of argumentEntries(input)) {
    const label = element('dt'); label.append(element('strong', '', key));
    const body = element('dd'); body.append(element('pre', '', argumentValue(value)));
    fields.append(label, body);
  }
  return fields;
}
function argumentPreview(input) {
  const text = argumentEntries(input).map(([key, value]) => `${key}: ${argumentValue(value)}`).join(' · ').replace(/\s+/g, ' ');
  const chars = Array.from(text);
  return chars.length > 180 ? `${chars.slice(0, 180).join('')}…` : chars.join('');
}
function toolState(block) {
  if (block.running) return 'running';
  if (block.error) return 'failed';
  return block.status === 'outcome not recorded' ? 'unknown' : 'done';
}
function messageHeading(role, time) {
  const heading = element('header', 'message-header');
  heading.append(element('span', 'role', role.toUpperCase()), element('time', 'timestamp', time || 'unknown'));
  return heading;
}
function blockNode(block) {
  if (block.kind === 'notice') return element('div', 'notice', block.text);
  if (block.kind === 'assistant_heading') return messageHeading('assistant', block.time);
  if (block.kind === 'tool') {
    const outcome = toolState(block);
    const details = element('details', `tool ${outcome}`);
    const summary = element('summary');
    summary.append(element('span', 'tool-name', block.tool.name), element('span', `tool-status ${outcome}`, block.status), toolDuration(block), element('span', 'tool-args', argumentPreview(block.tool.input)));
    const body = element('div', 'tool-content');
    body.append(element('span', 'tool-label', 'Input'), toolArguments(block.tool.input));
    if (block.text || block.images?.length) body.append(element('span', 'tool-label', 'Output'));
    if (block.text) body.append(element('pre', 'output', block.text));
    addImages(body, block.images);
    details.append(summary, body);
    return details;
  }
  const thinking = block.role === 'thinking';
  const article = element(thinking ? 'details' : 'article', `message ${block.role}${thinking ? ' thinking' : ''}`);
  if (thinking) article.append(element('summary', '', 'Thinking'));
  else {
    article.append(messageHeading(block.role, block.time));
  }
  const body = element('div', `body${block.role === 'user' ? '' : ' markdown'}`);
  if (block.role === 'user') body.textContent = block.text;
  else markdown(body, block.text);
  article.append(body);
  addImages(article, block.images);
  return article;
}
function replaceBlock(index, block, previous) {
  if (nodes[index] && previous) {
    if (JSON.stringify(previous) === JSON.stringify(block)) {
      if (block.kind === 'message' && block.role !== 'user') markdown(nodes[index].querySelector('.body'), block.text);
      return;
    }
    if (block.kind === 'tool' && previous.kind === 'tool') {
      const { elapsed_ms: elapsed, ...content } = block;
      const { elapsed_ms: previousElapsed, ...previousContent } = previous;
      if (JSON.stringify(content) === JSON.stringify(previousContent)) {
        nodes[index].querySelector('.tool-duration').replaceWith(toolDuration(block));
        return;
      }
    }
    if (block.kind === 'message' && previous.kind === 'message' && block.role === previous.role && block.time === previous.time && JSON.stringify(block.images) === JSON.stringify(previous.images)) {
      const body = nodes[index].querySelector('.body');
      if (block.role === 'user') body.textContent = block.text;
      else markdown(body, block.text);
      scrollLatest();
      return;
    }
  }
  const node = blockNode(block);
  if (nodes[index]) {
    if (node.tagName === 'DETAILS' && nodes[index].tagName === 'DETAILS') node.open = nodes[index].open;
    nodes[index].replaceWith(node);
  } else {
    if (!nodes.length) transcript.querySelector('.welcome')?.remove();
    transcript.append(node);
  }
  nodes[index] = node;
  scrollLatest();
}
function blockKey(block) {
  if (block.kind === 'tool') return JSON.stringify(['tool', block.tool]);
  if (block.kind === 'message') return JSON.stringify(['message', block.role, block.time, block.images]);
  if (block.kind === 'assistant_heading') return JSON.stringify(['assistant_heading', block.time]);
  return JSON.stringify(['notice', block.text]);
}
function snapshot(next) {
  const sameSession = state.session_id === next.session_id && state.thread_id === next.thread_id;
  const reusable = new Map();
  if (sameSession) for (const [index, block] of state.blocks.entries()) {
    const key = blockKey(block);
    const queue = reusable.get(key) || [];
    queue.push({ block, node: nodes[index] }); reusable.set(key, queue);
  }
  state = next;
  history.replaceState(null, '', `/sessions/${encodeURIComponent(state.session_id)}`);
  nodes.length = 0;
  for (const [index, block] of state.blocks.entries()) {
    const previous = reusable.get(blockKey(block))?.shift();
    if (previous) nodes[index] = previous.node;
    replaceBlock(index, block, previous?.block);
    if (transcript.children[index] !== nodes[index]) transcript.insertBefore(nodes[index], transcript.children[index] || null);
  }
  const retained = new Set(nodes);
  for (const node of Array.from(transcript.children)) if (!retained.has(node)) node.remove();
  if (!nodes.length && !transcript.querySelector('.welcome')) {
    const welcome = element('section', 'welcome');
    welcome.append(element('h1', '', 'MYCO'), element('p', '', 'Write a prompt to begin. Tool inputs and output expand in place.'));
    transcript.append(welcome);
  }
  metadata();
}
function metadata() {
  const model = $('model');
  const keys = state.models || [];
  if (keys.length !== model.options.length || keys.some((key, index) => model.options[index]?.value !== key)) {
    model.replaceChildren(...keys.map((key) => { const option = element('option', '', key); option.value = key; return option; }));
  }
  model.value = state.model || '';
  const disabled = !connected || !state.session_id || state.busy || selectingModel;
  model.disabled = disabled;
  activity();
  $('send').disabled = !connected || !state.session_id || selectingModel || sending || state.status === 'Cancelling';
  $('send').textContent = state.busy ? 'Queue ↵' : 'Send ↵';
  $('input-hint').textContent = state.busy ? 'Enter to queue · Sent after the current tools finish' : 'Enter to send · Shift+Enter for a new line';
  $('cancel').hidden = !state.busy;
  const queued = state.queued || [];
  $('cancel').textContent = queued.length ? 'Cancel & send queued' : 'Cancel run';
  $('queued').hidden = !queued.length;
  $('queued-count').textContent = `${queued.length} queued`;
  const list = $('queued-list');
  const signature = JSON.stringify(queued);
  if (list.dataset.messages !== signature) {
    list.dataset.messages = signature;
    list.replaceChildren(...queued.map((message) => element('li', '', message.text)));
  }
  $('compact').disabled = disabled || !state.blocks.length;
  $('session-title').textContent = state.title || 'Session';
  $('session-title').title = state.title || 'Session';
  transcript.setAttribute('aria-busy', String(!!state.busy));
  document.title = `${state.title || 'myco'} · myco`;
}
function activity() {
  const calls = state.blocks.flatMap((block, index) => block.kind === 'tool' && block.running ? [{ block, index }] : []);
  const tasks = state.tasks || [];
  const count = calls.length + tasks.length;
  $('activity-count').textContent = count;
  $('activity-count').hidden = !count;
  $('activity-toggle').classList.toggle('has-activity', connected && (state.busy || !!count));
  $('activity-title').textContent = connected ? 'Activity' : 'Activity · reconnecting';
  $('activity-empty').hidden = !!count;
  $('activity-empty').textContent = !connected ? 'Reconnecting to confirm activity.' : state.busy ? 'Run in progress. No active tool calls.' : 'No running activities.';
  $('active-calls').hidden = !calls.length;
  $('background-tasks').hidden = !tasks.length;
  $('activity-divider').hidden = !calls.length || !tasks.length;
  const focusedCall = document.activeElement?.dataset.blockIndex;
  const list = $('activity-list'); list.replaceChildren();
  for (const { block, index } of calls) {
    const item = element('li');
    const button = element('button', 'active-call');
    button.append(element('span', '', block.tool.name), toolDuration(block), document.createTextNode(` ${argumentPreview(block.tool.input)}`));
    button.type = 'button';
    button.dataset.blockIndex = index;
    button.onclick = () => {
      $('activity').close();
      nodes[index].open = true;
      follow = false;
      nodes[index].scrollIntoView({ block: 'center' });
    };
    item.append(button); list.append(item);
  }
  const background = $('background-list'); background.replaceChildren();
  for (const task of tasks) background.append(element('li', 'background-task', task));
  if ($('activity').open && focusedCall !== undefined) (list.querySelector(`[data-block-index="${focusedCall}"]`) || $('activity-close')).focus({ preventScroll: true });
  const current = calls.length ? `${calls.map(({ block }) => block.tool.name).join(', ')} · running` : tasks.length ? `${tasks.length} background ${tasks.length === 1 ? 'task' : 'tasks'}` : state.status || 'Ready';
  const status = $('connection');
  showActivity(status, state.busy ? state.status : count ? 'Background tasks' : state.status || 'Ready', state.busy, connected);
  if (connected) status.title = current;
}
function updateSession(update) {
  if (update.revision <= revision) return;
  revision = update.revision;
  const change = update.change;
  if (change.kind === 'snapshot') snapshot(change.snapshot);
  else if (change.kind === 'meta') { Object.assign(state, change.meta); metadata(); }
  else if (change.kind === 'block') { replaceBlock(change.index, change.block, state.blocks[change.index]); state.blocks[change.index] = change.block; activity(); }
  else if (change.kind === 'tasks') { state.tasks = change.tasks; activity(); }
  else if (change.kind === 'append') {
    const block = state.blocks[change.index]; block.text += change.text;
    markdown(nodes[change.index].querySelector('.body'), block.text);
  }
}
function connect() {
  connected = false; metadata();
  const worker = new SharedWorker('/events.js', { name: 'myco-events' });
  eventPort = worker.port;
  worker.onerror = () => { connected = false; metadata(); error('Could not connect to live output. Reload this page to reconnect.'); };
  eventPort.onmessage = ({ data }) => {
    if (data.kind === 'snapshot') { revision = -1; error(); updateSession(data.update); }
    else if (data.kind === 'update') updateSession(data.update);
    else if (data.kind === 'connection') { connected = data.connected; metadata(); }
    else if (data.kind === 'error') { connected = false; metadata(); error(data.message); }
  };
  eventPort.postMessage({ kind: 'subscribe', session_id: sessionId });
}
window.addEventListener('pagehide', () => { eventPort?.postMessage({ kind: 'unsubscribe' }); eventPort?.close(); });
window.addEventListener('pageshow', (event) => { if (event.persisted) connect(); });
connect();
async function sendAction(action, id = requestId()) {
  error();
  await api(`/api/sessions/${encodeURIComponent(state.session_id)}/action`, { request_id: id, session_id: state.session_id, action });
}
$('composer').onsubmit = async (event) => {
  event.preventDefault();
  if (!connected || !state.session_id || selectingModel || sending || state.status === 'Cancelling') return;
  const text = $('prompt').value.trim(); if (!text) return;
  let action = { kind: 'submit', text };
  if (text.startsWith('/')) {
    if (text === '/new') { newSession(); $('prompt').value = ''; resizeInput(); return; }
    else if (text === '/compact') action = { kind: 'compact' };
    else if (text.startsWith('/resume ')) { location.assign(`/sessions/${encodeURIComponent(text.slice(8).trim())}`); return; }
    else { error(text === '/verbose' ? 'Expand an individual tool block to see its full input and output.' : 'Use /new, /compact, /resume <id>, or the session controls.'); return; }
  }
  if (!pending || pending.text !== text || pending.session !== state.session_id) pending = { text, session: state.session_id, id: requestId() };
  sending = true; metadata();
  try {
    await sendAction(action, pending.id);
    if ($('prompt').value.trim() === text) { $('prompt').value = ''; resizeInput(); }
    pending = null;
  } catch (e) { error(`${e.message} Your draft is still here.`); }
  finally { sending = false; metadata(); }
};
$('prompt').onkeydown = (event) => { if (event.key === 'Enter' && !event.shiftKey && !event.altKey && !event.isComposing) { event.preventDefault(); $('composer').requestSubmit(); } };
$('compact').onclick = () => sendAction({ kind: 'compact' }).catch((e) => error(e.message));
$('cancel').onclick = () => api(`/api/sessions/${encodeURIComponent(state.session_id)}/cancel`, { session_id: state.session_id }).catch((e) => error(e.message));
$('model').onchange = async () => {
  const key = $('model').value;
  if (!key || key === state.model) return;
  selectingModel = true; metadata();
  try { await sendAction({ kind: 'select_model', key }); }
  catch (e) { error(e.message); }
  finally { selectingModel = false; metadata(); }
};
