import { element } from '/common.js';

const localTime = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'medium' });
const fullTime = new Intl.DateTimeFormat(undefined, { dateStyle: 'full', timeStyle: 'long' });
const relativeTime = new Intl.RelativeTimeFormat(undefined, { numeric: 'always' });
const clocks = new WeakMap();

function updateTimestamp(node, now) {
  const clock = clocks.get(node);
  if (!clock) return;
  const minutes = Math.floor(Math.max(0, now - clock.instant) / 60000);
  if (minutes === clock.minutes) return;
  clock.minutes = minutes;
  clock.age.textContent = `(${relativeTime.format(-minutes, 'minute')})`;
}

export function messageTimestamp(value) {
  const node = element('time', 'timestamp', 'unknown');
  const instant = value ? Date.parse(value) : NaN;
  if (!Number.isFinite(instant)) return node;
  node.dateTime = value;
  node.title = fullTime.format(instant);
  const age = element('span', 'timestamp-age');
  node.replaceChildren(`${localTime.format(instant)} `, age);
  clocks.set(node, { instant, age });
  updateTimestamp(node, Date.now());
  return node;
}

function refreshTimestamps() {
  if (document.hidden) return;
  const now = Date.now();
  for (const node of document.querySelectorAll('time.timestamp[datetime]')) updateTimestamp(node, now);
}

setInterval(refreshTimestamps, 1000);
document.addEventListener('visibilitychange', refreshTimestamps);
window.addEventListener('pageshow', refreshTimestamps);
