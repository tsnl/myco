import { $, element, requestId } from './common.js';

// Remaining time comes from the server's monotonic clock, not the browser's
// wall clock. Only visible countdowns need work between server updates.
export function sessionTimers(sendAction) {
  const clocks = new Map();
  const observations = new WeakMap();
  const list = $('timer-list');
  let connected = false;
  function tick() {
    if (document.hidden || !$('activity').open) return;
    const now = performance.now();
    for (const { node, deadline } of clocks.values()) {
      const seconds = Math.max(0, (deadline - now) / 1000);
      const text = !connected ? 'Reconnecting…' : seconds === 0 ? 'Due · waiting for delivery'
        : seconds >= 60 ? `In ${Math.ceil(seconds / 60)} min` : `In ${seconds.toFixed(1)}s`;
      if (node.textContent !== text) node.textContent = text;
    }
  }
  setInterval(tick, 100);
  $('activity').addEventListener('toggle', tick);
  document.addEventListener('visibilitychange', tick);
  return function render(timers, online) {
    connected = online;
    $('active-timers').hidden = !timers.length;
    const key = JSON.stringify([timers.map(({ id, message, due_at }) => [id, message, due_at]), online]);
    if (list.dataset.timers !== key) {
      const focused = document.activeElement?.dataset.timerId;
      list.dataset.timers = key;
      list.replaceChildren(); clocks.clear();
      for (const timer of timers) {
        const item = element('li', 'active-timer');
        const remaining = element('span', 'timer-countdown');
        const when = element('time', 'timer-due', new Date(timer.due_at).toLocaleString());
        when.dateTime = timer.due_at;
        const cancel = element('button', '', 'Cancel timer');
        const cancelId = requestId();
        cancel.type = 'button'; cancel.dataset.timerId = timer.id; cancel.disabled = !online;
        cancel.onclick = async () => {
          cancel.disabled = true;
          $('timer-error').hidden = true;
          try {
            await sendAction({ kind: 'cancel_timer', timer_id: timer.id }, cancelId);
            if ($('activity').open && [document.body, cancel].includes(document.activeElement)) $('activity-close').focus({ preventScroll: true });
          } catch (e) {
            $('timer-error').textContent = e.message; $('timer-error').hidden = false;
            cancel.disabled = !connected;
          }
        };
        item.append(element('p', 'timer-message', timer.message), remaining, when, cancel);
        clocks.set(timer.id, { node: remaining, deadline: 0 }); list.append(item);
      }
      if (focused && $('activity').open) (list.querySelector(`[data-timer-id="${focused}"]`) || $('activity-close')).focus({ preventScroll: true });
    }
    for (const timer of timers) {
      if (!observations.has(timer)) observations.set(timer, performance.now() + timer.remaining_ms);
      clocks.get(timer.id).deadline = observations.get(timer);
    }
    tick();
  };
}
