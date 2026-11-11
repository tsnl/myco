import { $, api, element } from './common.js';

// Events awaiting delivery need attention. Delivered events are also in the
// transcript, and dropped ones never will be, so both rest in collapsed lists.
const ACTIVE = ['pending', 'delivering', 'running'], DROPPED = ['cancelled', 'expired'];
export const activeEvents = events => events.filter(event => ACTIVE.includes(event.state));

const clock = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' });

function moment(value, label = '') {
  const node = element('time', '', `${label}${clock.format(new Date(value))}`);
  node.dateTime = value; node.title = new Date(value).toLocaleString();
  return node;
}

function status(event) {
  if (event.state !== 'pending') return event.state[0].toUpperCase() + event.state.slice(1);
  return event.held ? 'Paused for editing' : new Date(event.due_at) > new Date() ? 'Scheduled' : 'Queued';
}

function eventItem(event, sessionId, connected) {
  const item = element('li', 'inbox-event');
  item.dataset.eventId = event.request.event_id;
  item.dataset.state = event.state;
  const labels = [status(event)];
  if (event.source === 'timer') labels.unshift('Timer');
  if (event.request.priority !== 'normal') labels.push(event.request.priority.replaceAll('_', ' '));
  const times = element('span', 'inbox-times');
  if (!ACTIVE.includes(event.state)) times.append(moment(event.finished_at || event.due_at));
  else {
    times.append(moment(event.due_at, 'Due '));
    if (event.request.expires_at) times.append(' · ', moment(event.request.expires_at, 'Expires '));
  }
  item.append(element('span', 'inbox-state', labels.join(' · ')), element('p', 'inbox-message', event.message), times);
  if (event.state === 'pending') {
    const button = element('button', '', 'Cancel event');
    button.type = 'button'; button.disabled = !connected;
    button.dataset.eventId = event.request.event_id;
    button.onclick = async () => {
      button.disabled = true; $('inbox-error').hidden = true;
      try { await api(`/api/sessions/${encodeURIComponent(sessionId)}/inbox/${event.request.event_id}/cancel`, {}); }
      catch (error) { $('inbox-error').textContent = error.message; $('inbox-error').hidden = false; button.disabled = !connected; }
    };
    item.append(button);
  }
  return item;
}

// The groups are static elements, so an update keeps whichever are open.
function group(id, events, sessionId, connected) {
  const node = $(id);
  node.hidden = !events.length;
  node.querySelector('summary span').textContent = events.length;
  node.querySelector('ul').replaceChildren(...events.map(event => eventItem(event, sessionId, connected)));
}

export function renderInbox(events, timers, sessionId, connected) {
  // Pending timers have their own section, with countdowns.
  const scheduled = new Set(timers.map(timer => timer.id));
  const visible = events.filter(event => !scheduled.has(event.request.event_id));
  $('event-inbox').hidden = !visible.length;
  const list = $('inbox-list');
  const key = JSON.stringify([visible, sessionId, connected]);
  if (list.dataset.events === key) return;
  list.dataset.events = key;
  const focused = document.activeElement?.dataset.eventId;
  const ended = event => event.finished_at || event.due_at;
  const finished = visible.filter(event => !ACTIVE.includes(event.state))
    .sort((a, b) => ended(b).localeCompare(ended(a)));
  const waiting = activeEvents(visible).sort((a, b) => a.due_at.localeCompare(b.due_at));
  list.replaceChildren(...waiting.map(event => eventItem(event, sessionId, connected)));
  group('inbox-read', finished.filter(event => !DROPPED.includes(event.state)), sessionId, connected);
  group('inbox-dropped', finished.filter(event => DROPPED.includes(event.state)), sessionId, connected);
  if (focused && $('activity').open) (list.querySelector(`button[data-event-id="${focused}"]`) || $('activity-close')).focus({ preventScroll: true });
}
