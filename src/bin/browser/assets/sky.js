import renderGradient from '/horizon.js';
import { createClouds, colorClouds } from '/clouds.js';
import { skySettings } from '/sky-settings.js';

const sky = document.createElement('div');
sky.id = 'sky';
sky.setAttribute('aria-hidden', 'true');
sky.classList.toggle('paused', document.hidden);
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
const updateClouds = createClouds(sky);
const illustrated = { cloud_cover_high: 32, cloud_cover_mid: 38, cloud_cover_low: 42, wind_speed_10m: 3, wind_direction_10m: 260 };
let forecast = null;

function updateSky() {
  if (document.hidden) return;
  const now = new Date();
  const local = forecast ? new Date(now.getTime() + forecast.utc_offset_seconds * 1000) : now;
  const hour = forecast ? local.getUTCHours() + local.getUTCMinutes() / 60 : now.getHours() + now.getMinutes() / 60;
  const altitude = Math.cos((hour - 12) * Math.PI / 12) * Math.PI / 3;
  const night = Math.max(0, Math.min(1, (-altitude * 180 / Math.PI - 3) / 9));
  const [gradient] = renderGradient(altitude);
  atmosphere.style.backgroundImage = gradient;
  atmosphere.style.opacity = String(1 - night * 0.92);
  stars.style.opacity = String(night);
  sky.dataset.phase = night > 0.5 ? 'night' : altitude < 0.15 ? 'twilight' : 'day';
  colorClouds(sky, altitude, night);
}
document.addEventListener('visibilitychange', () => {
  sky.classList.toggle('paused', document.hidden);
  updateSky();
});
updateSky();
setInterval(updateSky, 60000);
skySettings(next => {
  forecast = next;
  updateClouds(forecast?.current || illustrated);
  updateSky();
});
