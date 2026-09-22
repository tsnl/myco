// Decorative flybys use one timer and CSS motion. Only visible time counts
// towards the next arrival, so returning to a tab cannot unleash a backlog.
const between = (low, high) => low + Math.random() * (high - low);

function airplane() {
  const plane = document.createElement('div');
  plane.className = 'sky-aircraft';
  plane.classList.toggle('has-contrail', Math.random() < 0.7);
  plane.innerHTML = `<div class="aircraft-bearing">
    <div class="aircraft-contrail"></div>
    <svg viewBox="0 0 28 20" focusable="false" aria-hidden="true">
      <path class="aircraft-silhouette" d="M26 10C26 9 24 8.5 22 8.5H17L9 1H6L10 8.5H4L1 5.5H0L1.5 10L0 14.5H1L4 11.5H10L6 19H9L17 11.5H22C24 11.5 26 11 26 10Z"/>
      <g class="aircraft-lights"><circle class="aircraft-port" cx="7.5" cy="1.5" r="1.25"/>
        <circle class="aircraft-starboard" cx="7.5" cy="18.5" r="1.25"/>
        <circle class="aircraft-beacon" cx="3" cy="10" r="1.15"/></g>
    </svg></div>`;
  return plane;
}

function flight() {
  const plane = airplane(), eastbound = Math.random() < 0.5;
  const start = between(12, 38), end = Math.max(8, Math.min(46, start + between(-9, 9)));
  const heading = () => Math.atan2((end - start) * innerHeight / 100, (innerWidth + 600) * (eastbound ? 1 : -1)) * 180 / Math.PI;
  plane.style.setProperty('--flight-from-x', eastbound ? '-300px' : 'calc(100vw + 300px)');
  plane.style.setProperty('--flight-to-x', eastbound ? 'calc(100vw + 300px)' : '-300px');
  plane.style.setProperty('--flight-from-y', `${start}vh`);
  plane.style.setProperty('--flight-to-y', `${end}vh`);
  plane.style.setProperty('--flight-duration', `${between(135, 210)}s`);
  plane.style.setProperty('--aircraft-scale', String(between(0.6, 0.85)));
  plane.style.setProperty('--contrail-length', `${between(160, 260)}px`);
  plane.style.setProperty('--beacon-delay', `${between(-4, 0)}s`);
  const resize = () => plane.style.setProperty('--flight-heading', `${heading()}deg`);
  resize();
  window.addEventListener('resize', resize);
  const remove = () => { window.removeEventListener('resize', resize); plane.remove(); };
  plane.addEventListener('animationend', event => { if (event.target === plane) remove(); });
  // Reduced motion removes a flight before its natural animationend.
  plane.addEventListener('animationcancel', event => { if (event.target === plane) remove(); });
  return { plane, remove };
}

export function createAircraft(sky) {
  const layer = document.createElement('div');
  layer.id = 'sky-aircraft';
  sky.append(layer);
  const motion = matchMedia('(prefers-reduced-motion: reduce)');
  const flights = new Set();
  let timer = null, started = 0, remaining = between(45000, 110000);

  function arrive() {
    timer = null;
    if (document.hidden || motion.matches) { remaining = 0; return; }
    for (const item of flights) if (!item.plane.isConnected) flights.delete(item);
    if (flights.size < 2) {
      const item = flight();
      flights.add(item); layer.append(item.plane);
    }
    // A small chance of a nearby second flight; otherwise leave a quiet gap.
    remaining = flights.size === 1 && Math.random() < 0.16 ? between(25000, 50000) : between(180000, 360000);
    schedule();
  }

  function schedule() {
    if (timer !== null || document.hidden || motion.matches) return;
    started = performance.now();
    timer = setTimeout(arrive, remaining);
  }

  function suspend() {
    if (timer === null) return;
    remaining = Math.max(0, remaining - (performance.now() - started));
    clearTimeout(timer); timer = null;
  }

  document.addEventListener('visibilitychange', () => document.hidden ? suspend() : schedule());
  motion.addEventListener('change', () => {
    suspend();
    for (const item of flights) item.remove();
    flights.clear(); remaining = between(45000, 110000);
    schedule();
  });
  window.addEventListener('pagehide', suspend);
  window.addEventListener('pageshow', schedule);
  schedule();
}
