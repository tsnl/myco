import { $, api, element } from './common.js';

const active = event => ['pending', 'delivering', 'running'].includes(event.state);
export const activeEvents = events => events.filter(active);

export function renderInbox(events, timers, sessionId, connected) {
  const scheduled = new Set(timers.map(timer => timer.id));
  const visible = events.filter(event => !scheduled.has(event.request.event_id));
  $('event-inbox').hidden = !visible.length;
  const list = $('inbox-list');
  const key = JSON.stringify([visible, sessionId, connected]);
  if (list.dataset.events === key) return;
  list.dataset.events = key;
  const focused = document.activeElement?.dataset.eventId;
  list.replaceChildren(...visible.slice().reverse().map(event => {
    const item = element('li', 'inbox-event');
    item.dataset.eventId = event.request.event_id;
    item.dataset.state = event.state;
    const priority = event.request.priority.replaceAll('_', ' ');
    const status = event.state === 'pending' ? event.held ? 'Paused for editing'
      : new Date(event.due_at) > new Date() ? 'Scheduled' : 'Queued' : event.state;
    item.append(element('p', 'inbox-message', event.message), element('span', 'inbox-state', `${status} · ${priority}`));
    const when = element('time', 'inbox-due', new Date(event.due_at).toLocaleString());
    when.dateTime = event.due_at; item.append(when);
    if (event.request.expires_at) item.append(element('span', 'inbox-expiry', `Expires ${new Date(event.request.expires_at).toLocaleString()}`));
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
  }));
  if (focused && $('activity').open) (list.querySelector(`button[data-event-id="${focused}"]`) || $('activity-close')).focus({ preventScroll: true });
}
