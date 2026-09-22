import renderGradient from '/horizon.js';

// A local-time day/night cycle; no location permission or external service.
const sky = document.createElement('div');
sky.id = 'sky';
sky.setAttribute('aria-hidden', 'true');
const atmosphere = document.createElement('div');
atmosphere.className = 'sky-atmosphere';
sky.append(atmosphere);

const namespace = 'http://www.w3.org/2000/svg';
const stars = document.createElementNS(namespace, 'svg');
stars.id = 'sky-stars';
stars.setAttribute('viewBox', '0 0 1600 1000');
stars.setAttribute('preserveAspectRatio', 'xMidYMid slice');
stars.setAttribute('focusable', 'false');
let seed = 2048;
function random() { seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0; return seed / 4294967296; }
for (let index = 0; index < 90; index++) {
  const star = document.createElementNS(namespace, 'circle');
  star.setAttribute('cx', String(random() * 1600));
  star.setAttribute('cy', String(random() * 850));
  star.setAttribute('r', String(0.5 + random() * 1.1));
  star.style.setProperty('--twinkle-duration', `${4 + random() * 5}s`);
  star.style.setProperty('--twinkle-delay', `${-random() * 12}s`);
  stars.append(star);
}
sky.append(stars);
document.body.prepend(sky);

function updateSky() {
  if (document.hidden) return;
  const now = new Date();
  const hour = now.getHours() + now.getMinutes() / 60;
  const altitude = Math.cos((hour - 12) * Math.PI / 12) * Math.PI / 3;
  const night = Math.max(0, Math.min(1, (-altitude * 180 / Math.PI - 3) / 9));
  const [gradient] = renderGradient(altitude);
  atmosphere.style.backgroundImage = gradient;
  atmosphere.style.opacity = String(1 - night * 0.92);
  stars.style.opacity = String(night);
  sky.dataset.phase = night > 0.5 ? 'night' : altitude < 0.15 ? 'twilight' : 'day';
}
document.addEventListener('visibilitychange', () => {
  sky.classList.toggle('paused', document.hidden);
  updateSky();
});
updateSky();
setInterval(updateSky, 60000);
