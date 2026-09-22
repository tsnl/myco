// Pixel silhouettes are generated once. CSS moves the finished layers; there
// is no animation loop, canvas repaint, or image/service dependency.
const NS = 'http://www.w3.org/2000/svg';
const WIDTH = 176, HEIGHT = 88;
const LAYERS = [
  { name: 'high', altitude: '8+', count: 8, top: 9, spread: 22, width: 470, height: 110, duration: 6800 },
  { name: 'mid', altitude: '3–8', count: 8, top: 33, spread: 24, width: 470, height: 205, duration: 5100 },
  { name: 'low', altitude: '0–3', count: 8, top: 62, spread: 23, width: 620, height: 310, duration: 3700 },
];

function random(seed) {
  return () => { seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0; return seed / 4294967296; };
}

function lobes(kind, roll) {
  if (kind === 'high') {
    return Array.from({ length: 7 }, (_, i) => ({
      x: 28 + i * 19, y: 55 - i * 4 + roll() * 12,
      rx: 21 + roll() * 22, ry: 4 + roll() * 8, depth: 0.5,
    }));
  }
  const puffs = [{ x: 87, y: 67, rx: 76, ry: 12, depth: 0.55 }];
  for (let i = 0; i < 6; i++) {
    const x = 27 + i * 24 + roll() * 8;
    const rise = Math.sin((i + 1) / 7 * Math.PI);
    puffs.push({ x, y: 62 - rise * (kind === 'low' ? 31 : 16) + roll() * 12,
      rx: 18 + roll() * 13, ry: 11 + rise * (kind === 'low' ? 22 : 12) + roll() * 7, depth: 0.7 + roll() * 0.4 });
  }
  return puffs;
}

function pixel(x, y, puffs, kind, roll) {
  let weight = 0, illumination = 0, depth = 0;
  for (const puff of puffs) {
    const dx = (x - puff.x) / puff.rx, dy = (y - puff.y) / puff.ry;
    const z = Math.sqrt(Math.max(0, 1 - dx * dx - dy * dy));
    const contribution = z ** 4 * puff.depth;
    illumination += contribution * (-dx * 0.15 - dy * 0.3 + z * 0.3);
    weight += contribution;
    depth = Math.max(depth, z * puff.depth);
  }
  if (!weight || depth < 0.1) return -1;
  // A few broken edge pixels and horizontal wisps soften the block silhouette.
  const grain = roll();
  if (depth < 0.23 && grain > depth * 3) return -1;
  if (kind === 'high' && (grain > 0.96 || (y % 4 === 0 && grain > 0.65))) return -1;
  const light = illumination / weight + (1 - y / HEIGHT) * 0.45;
  const underside = Math.max(0, y - 55) / 65;
  const dither = ((x + y) % 2 ? 1 : -1) * 0.025;
  return Math.max(0, Math.min(5, Math.floor((light - underside + 0.25 + dither) * 5.5)));
}

function sprite(kind, seed) {
  const roll = random(seed), puffs = lobes(kind, roll);
  const paths = Array.from({ length: 6 }, () => '');
  // Run-length encoded pixel rows keep each cloud to six SVG paths.
  for (let y = 0; y < HEIGHT; y++) {
    let previous = -1, start = 0;
    for (let x = 0; x <= WIDTH; x++) {
      const tone = x === WIDTH ? -1 : pixel(x, y, puffs, kind, roll);
      if (tone === previous) continue;
      if (previous >= 0) paths[previous] += `M${start} ${y}h${x - start}v1H${start}z`;
      previous = tone; start = x;
    }
  }
  const svg = document.createElementNS(NS, 'svg');
  svg.setAttribute('viewBox', `0 0 ${WIDTH} ${HEIGHT}`);
  svg.setAttribute('preserveAspectRatio', 'none');
  svg.setAttribute('focusable', 'false');
  for (let tone = 0; tone < paths.length; tone++) {
    const path = document.createElementNS(NS, 'path');
    path.setAttribute('d', paths[tone]);
    path.style.fill = `var(--cloud-${tone})`;
    svg.append(path);
  }
  return svg;
}

function makeLayer(spec, index) {
  const layer = document.createElement('div');
  layer.className = `cloud-layer cloud-${spec.name}`;
  layer.dataset.layer = spec.name;
  layer.dataset.altitude = spec.altitude;
  const track = document.createElement('div');
  track.className = 'cloud-track';
  const roll = random(943 + index * 379);
  for (let i = 0; i < spec.count; i++) {
    const cloud = document.createElement('div');
    cloud.className = 'pixel-cloud';
    cloud.dataset.index = String(i);
    const scale = 0.75 + roll() * 0.5;
    cloud.style.width = `${spec.width * scale}px`;
    cloud.style.height = `${spec.height * scale}px`;
    cloud.style.top = `${spec.top + roll() * spec.spread}%`;
    cloud.append(sprite(spec.name, 781 + index * 100 + i * 7));
    // Three copies cover both viewport edges throughout either wind direction.
    const position = (i * 61.803 + index * 23) % 100;
    for (const offset of [-100, 0, 100]) {
      const copy = cloud.cloneNode(true);
      copy.style.left = `${position + offset}%`;
      track.append(copy);
    }
  }
  layer.append(track);
  return layer;
}

function drift(layer, spec, conditions) {
  const duration = spec.duration / (0.7 + Math.min(conditions.wind_speed_10m, 20) / 30);
  // Meteorological direction names where the wind comes from.
  const reverse = conditions.wind_direction_10m >= 180;
  const track = layer.firstElementChild;
  const progress = track.getAnimations()[0]?.effect.getComputedTiming().progress || 0;
  layer.style.setProperty('--drift-duration', `${duration}s`);
  layer.style.setProperty('--drift-direction', reverse ? 'reverse' : 'normal');
  // Changing CSS duration or direction otherwise jumps the clouds across the
  // screen. Preserve their position when a new weather report changes the wind.
  const animation = track.getAnimations()[0];
  if (animation) animation.currentTime = (reverse ? 1 - progress : progress) * duration * 1000;
}

export function createClouds(sky) {
  const clouds = document.createElement('div');
  clouds.id = 'sky-clouds';
  const layers = LAYERS.map(makeLayer);
  clouds.append(...layers);
  sky.append(clouds);
  return conditions => {
    LAYERS.forEach((spec, index) => {
      const cover = conditions[`cloud_cover_${spec.name}`];
      const layer = layers[index];
      layer.dataset.cover = String(cover);
      drift(layer, spec, conditions);
      for (const cloud of layer.querySelectorAll('.pixel-cloud')) {
        const threshold = Number(cloud.dataset.index) * 100 / spec.count;
        cloud.style.opacity = String(Math.max(0, Math.min(1, (cover - threshold) / 16)));
      }
    });
    const overcast = Math.max(conditions.cloud_cover_low, conditions.cloud_cover_mid) / 100;
    sky.style.setProperty('--overcast', String(overcast * 0.28));
  };
}

const DAY = ['#708aaf', '#92acc5', '#b5ccda', '#d8e3df', '#eef0df', '#fff7e3'];
const DUSK = ['#655d91', '#8b709e', '#b98ba8', '#e1aaa9', '#f4c6ad', '#ffdfb9'];
const NIGHT = ['#1a243e', '#263351', '#354663', '#485b77', '#617693', '#8297ad'];

const channels = hex => [1, 3, 5].map(offset => parseInt(hex.slice(offset, offset + 2), 16));
const blend = (a, b, weight) => a.map((value, index) => value * (1 - weight) + b[index] * weight);

export function colorClouds(sky, altitude, night) {
  const twilight = Math.max(0, 1 - Math.abs(altitude) / 0.35);
  DAY.forEach((color, index) => {
    const warm = blend(channels(color), channels(DUSK[index]), twilight);
    const tones = blend(warm, channels(NIGHT[index]), night).map(Math.round);
    sky.style.setProperty(`--cloud-${index}`, `rgb(${tones.join(' ')})`);
  });
}
