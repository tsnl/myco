'use strict';
const $ = (id) => document.getElementById(id);
const transcript = $('transcript');
let state = { blocks: [], tasks: [], busy: false };
let connected = false;
let revision = -1;
let follow = true;
let pending = null;
let selectingModel = false;
const nodes = [];
const markdownJobs = new WeakMap();

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}
function error(message = '') { $('error').textContent = message; $('error').hidden = !message; }
async function api(path, body) {
  const response = await fetch(path, body === undefined ? {} : { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) });
  if (!response.ok) throw new Error(await response.text());
  return response;
}
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
  if (!job) { job = { text, version: 0, timer: null }; markdownJobs.set(node, job); }
  job.text = text;
  job.version += 1;
  if (!node.hasChildNodes()) { node.textContent = text; node.classList.add('pending'); }
  if (job.timer) return;
  const render = async () => {
    job.timer = null;
    const version = job.version;
    try {
      const html = await (await api('/api/markdown', { text: job.text })).text();
      if (!node.isConnected) return;
      if (version !== job.version) { job.timer = setTimeout(render, 80); return; }
      node.innerHTML = html;
      node.classList.remove('pending');
      for (const link of node.querySelectorAll('a')) { link.target = '_blank'; link.rel = 'noopener noreferrer'; }
      for (const img of node.querySelectorAll('img')) { img.loading = 'lazy'; img.referrerPolicy = 'no-referrer'; img.addEventListener('load', scrollLatest); }
      scrollLatest();
    } catch (e) { node.textContent = job.text; node.classList.add('pending'); }
  };
  job.timer = setTimeout(render, 80);
}
function jsonArguments(input, preview = false) {
  const pre = element(preview ? 'span' : 'pre', preview ? 'tool-args' : 'arguments');
  const text = JSON.stringify(input, null, preview ? 0 : 2);
  const limit = preview ? Array.from(text).slice(0, 180).join('').length : text.length;
  const strings = /"(?:[^"\\]|\\.)*"\s*:?/g;
  let start = 0;
  for (const match of text.matchAll(strings)) {
    if (match.index >= limit) break;
    if (!match[0].endsWith(':')) continue;
    pre.append(document.createTextNode(text.slice(start, match.index)));
    start = Math.min(limit, match.index + match[0].length);
    pre.append(element('span', 'json-key', text.slice(match.index, start)));
  }
  pre.append(document.createTextNode(text.slice(start, limit) + (limit < text.length ? '…' : '')));
  return pre;
}
function argumentPreview(input) {
  const chars = Array.from(JSON.stringify(input));
  return chars.length > 180 ? `${chars.slice(0, 180).join('')}…` : chars.join('');
}
function toolState(block) {
  if (block.running) return 'running';
  if (block.error) return 'failed';
  return block.status === 'outcome not recorded' ? 'unknown' : 'done';
}
function blockNode(block) {
  if (block.kind === 'notice') return element('div', 'notice', block.text);
  if (block.kind === 'tool') {
    const outcome = toolState(block);
    const details = element('details', `tool ${outcome}`);
    details.dataset.key = JSON.stringify(block.tool);
    const summary = element('summary');
    summary.append(element('span', 'tool-name', block.tool.name), element('span', `tool-status ${outcome}`, block.status), jsonArguments(block.tool.input, true));
    const body = element('div', 'tool-content');
    body.append(element('span', 'tool-label', 'Input'), jsonArguments(block.tool.input));
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
    const heading = element('header', 'message-header');
    heading.append(element('span', 'role', block.role.toUpperCase()), element('time', 'timestamp', block.time || 'unknown'));
    article.append(heading);
  }
  const body = element('div', `body${block.role === 'user' ? '' : ' markdown'}`);
  if (block.role === 'user') body.textContent = block.text;
  else markdown(body, block.text);
  article.append(body);
  addImages(article, block.images);
  return article;
}
function replaceBlock(index, block) {
  const node = blockNode(block);
  if (nodes[index]) {
    if (node.tagName === 'DETAILS' && nodes[index].tagName === 'DETAILS') node.open = nodes[index].open;
    nodes[index].replaceWith(node);
  } else {
    if (!nodes.length) transcript.replaceChildren();
    transcript.append(node);
  }
  nodes[index] = node;
  scrollLatest();
}
function snapshot(next) {
  const open = new Map();
  for (const node of nodes) if (node?.dataset.key) {
    const queue = open.get(node.dataset.key) || []; queue.push(node.open); open.set(node.dataset.key, queue);
  }
  const sameSession = state.session_id === next.session_id && state.thread_id === next.thread_id;
  state = next;
  nodes.length = 0; transcript.replaceChildren();
  for (const [index, block] of state.blocks.entries()) {
    replaceBlock(index, block);
    if (sameSession && nodes[index].dataset.key) nodes[index].open = open.get(nodes[index].dataset.key)?.shift() || false;
  }
  if (!nodes.length) {
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
  const disabled = !connected || state.busy || selectingModel;
  model.disabled = disabled;
  activity();
  $('send').disabled = disabled;
  $('cancel').hidden = !state.busy;
  $('new-session').disabled = disabled;
  $('compact').disabled = disabled || !state.blocks.length;
  $('sessions').disabled = disabled;
  transcript.setAttribute('aria-busy', String(!!state.busy));
  document.title = `${state.title || 'myco'} · myco`;
  if (!state.busy) refreshSessions();
}
function activity() {
  const calls = state.blocks.flatMap((block, index) => block.kind === 'tool' && block.running ? [{ block, index }] : []);
  const tasks = state.tasks || [];
  const count = calls.length + tasks.length;
  $('activity-count').textContent = count;
  $('activity-toggle').classList.toggle('has-activity', !!count);
  $('activity-title').textContent = connected ? 'Activity' : 'Activity · reconnecting';
  $('activity-empty').hidden = !!count;
  $('active-calls').hidden = !calls.length;
  $('background-tasks').hidden = !tasks.length;
  $('activity-divider').hidden = !calls.length || !tasks.length;
  const focusedCall = document.activeElement?.dataset.blockIndex;
  const list = $('activity-list'); list.replaceChildren();
  for (const { block, index } of calls) {
    const item = element('li');
    const button = element('button', 'active-call', `${block.tool.name} ${argumentPreview(block.tool.input)}`);
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
  $('connection').textContent = connected ? state.status === 'Cancelling' ? 'Cancelling' : current : 'Reconnecting…';
}
async function refreshSessions() {
  try {
    const sessions = await (await api('/api/sessions')).json();
    const select = $('sessions'); select.replaceChildren();
    if (!sessions.some((s) => s.id === state.session_id)) sessions.unshift({ id: state.session_id, title: state.title || 'New session' });
    for (const session of sessions) { const option = element('option', '', session.title || session.id); option.value = session.id; select.append(option); }
    select.value = state.session_id;
  } catch (e) { error(e.message); }
}
const stream = new EventSource('/api/events');
stream.onopen = () => { connected = true; metadata(); };
stream.onerror = () => { connected = false; metadata(); };
stream.onmessage = ({ data }) => {
  const update = JSON.parse(data);
  if (update.revision <= revision) return;
  revision = update.revision;
  const change = update.change;
  if (change.kind === 'snapshot') snapshot(change.snapshot);
  else if (change.kind === 'meta') { Object.assign(state, change.meta); metadata(); }
  else if (change.kind === 'block') { state.blocks[change.index] = change.block; replaceBlock(change.index, change.block); activity(); }
  else if (change.kind === 'tasks') { state.tasks = change.tasks; activity(); }
  else if (change.kind === 'append') {
    const block = state.blocks[change.index]; block.text += change.text;
    markdown(nodes[change.index].querySelector('.body'), block.text);
  }
};
async function sendAction(action, requestId = crypto.randomUUID()) {
  error();
  await api('/api/action', { request_id: requestId, session_id: state.session_id, action });
}
$('composer').onsubmit = async (event) => {
  event.preventDefault();
  if (!connected || state.busy || selectingModel) return;
  const text = $('prompt').value.trim(); if (!text) return;
  let action = { kind: 'submit', text };
  if (text.startsWith('/')) {
    if (text === '/new') action = { kind: 'new' };
    else if (text === '/compact') action = { kind: 'compact' };
    else if (text.startsWith('/resume ')) action = { kind: 'open', id: text.slice(8).trim() };
    else { error(text === '/verbose' ? 'Expand an individual tool block to see its full input and output.' : 'Use /new, /compact, /resume <id>, or the session controls.'); return; }
  }
  if (!pending || pending.text !== text || pending.session !== state.session_id) pending = { text, session: state.session_id, id: crypto.randomUUID() };
  $('send').disabled = true;
  try {
    await sendAction(action, pending.id);
    if ($('prompt').value.trim() === text) { $('prompt').value = ''; resizeInput(); }
    pending = null;
  } catch (e) { error(`${e.message} Your draft is still here.`); metadata(); }
};
$('prompt').onkeydown = (event) => { if (event.key === 'Enter' && !event.shiftKey && !event.altKey && !event.isComposing) { event.preventDefault(); $('composer').requestSubmit(); } };
$('new-session').onclick = () => sendAction({ kind: 'new' }).catch((e) => error(e.message));
$('compact').onclick = () => sendAction({ kind: 'compact' }).catch((e) => error(e.message));
$('sessions').onchange = () => sendAction({ kind: 'open', id: $('sessions').value }).catch((e) => error(e.message));
$('cancel').onclick = () => api('/api/cancel', { session_id: state.session_id }).catch((e) => error(e.message));
$('model').onchange = async () => {
  const key = $('model').value;
  if (!key || key === state.model) return;
  selectingModel = true; metadata();
  try { await sendAction({ kind: 'select_model', key }); }
  catch (e) { error(e.message); }
  finally { selectingModel = false; metadata(); }
};
