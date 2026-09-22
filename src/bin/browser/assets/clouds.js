// Density and lighting textures are generated once in a worker. CSS handles
// drifting; only changes to the sky palette repaint the finished clouds.
const LAYERS = [
  { name: 'high', altitude: '8+', count: 8, top: 9, spread: 25, width: 850, height: 220, resolution: 192, duration: 6800 },
  { name: 'mid', altitude: '3–8', count: 8, top: 32, spread: 26, width: 900, height: 440, resolution: 256, duration: 5100 },
  { name: 'low', altitude: '0–3', count: 8, top: 60, spread: 26, width: 1050, height: 580, resolution: 288, duration: 3700 },
];

//
// Textures and fallback
//

function random(seed) {
  return () => { seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0; return seed / 4294967296; };
}

const textures = new Map();
let worker, currentPalette, generation = 0;

function fallback(canvas, seed) {
  const context = canvas.getContext('2d'), roll = random(seed);
  context.clearRect(0, 0, canvas.width, canvas.height);
  context.save(); context.scale(1, 0.4);
  for (let i = 0; i < 9; i++) {
    const x = canvas.width * (0.12 + i * 0.095), y = canvas.height * (0.9 + roll() * 0.65);
    const radius = canvas.height * (0.23 + roll() * 0.12);
    const glow = context.createRadialGradient(x, y, radius * 0.12, x, y, radius);
    const color = currentPalette[3 + i % 3].join(' ');
    glow.addColorStop(0, `rgb(${color} / .35)`);
    glow.addColorStop(0.55, `rgb(${color} / .12)`);
    glow.addColorStop(1, `rgb(${color} / 0)`);
    context.fillStyle = glow;
    context.fillRect(x - radius, y - radius, radius * 2, radius * 2);
  }
  context.restore();
}

function renderTextures(sky) {
  renderFallback();
  try {
    worker = new Worker('/cloud-renderer.js', { type: 'module' });
    worker.onmessage = ({ data }) => {
      if (data.generation !== generation) return;
      const texture = textures.get(data.id);
      const frame = new ImageData(data.pixels, data.width, data.height);
      for (const canvas of texture.canvases) canvas.getContext('2d').putImageData(frame, 0, 0);
      texture.ready = true;
      if ([...textures.values()].every(item => item.ready)) sky.dataset.clouds = 'ready';
    };
    worker.onerror = event => { event.preventDefault(); worker?.terminate(); worker = null; sky.dataset.clouds = 'fallback'; };
    repaint(sky);
    const pause = () => worker?.postMessage({ paused: document.hidden });
    document.addEventListener('visibilitychange', pause);
    pause();
  } catch { sky.dataset.clouds = 'fallback'; }
}

function renderFallback() {
  const key = JSON.stringify(currentPalette);
  for (const texture of textures.values()) {
    if (texture.fallbackPalette === key) continue;
    const [first, ...copies] = texture.canvases;
    fallback(first, texture.spec.seed);
    for (const canvas of copies) {
      const context = canvas.getContext('2d');
      context.clearRect(0, 0, canvas.width, canvas.height);
      context.drawImage(first, 0, 0);
    }
    texture.fallbackPalette = key;
  }
}

function repaint(sky) {
  generation++;
  if (worker) {
    sky.dataset.clouds = 'painting';
    for (const texture of textures.values()) texture.ready = false;
    worker.postMessage({ specs: [...textures.values()].map(item => item.spec), palette: currentPalette, generation });
  } else {
    renderFallback();
  }
}

//
// Altitude layers and wind
//

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
    cloud.className = 'cloud-sprite';
    cloud.dataset.index = String(i);
    const scale = 0.75 + roll() * 0.5;
    cloud.style.width = `${spec.width * scale}px`;
    cloud.style.height = `${spec.height * scale}px`;
    cloud.style.top = `${spec.top + roll() * spec.spread}%`;
    cloud.style.setProperty('--cloud-tilt', `${(roll() - 0.5) * (24 - index * 8)}deg`);
    const id = `${spec.name}-${i}`;
    const texture = { spec: { id, kind: spec.name, seed: 781 + index * 100 + i * 7, width: 512, height: spec.resolution }, canvases: [] };
    textures.set(id, texture);
    // Three copies cover both viewport edges throughout either wind direction.
    const position = (i * 61.803 + index * 23) % 100;
    for (const offset of [-100, 0, 100]) {
      const copy = cloud.cloneNode(true);
      copy.style.left = `${position + offset}%`;
      const canvas = document.createElement('canvas');
      canvas.width = texture.spec.width; canvas.height = texture.spec.height;
      copy.append(canvas); texture.canvases.push(canvas);
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
  requestAnimationFrame(() => renderTextures(sky));
  return conditions => {
    LAYERS.forEach((spec, index) => {
      const cover = conditions[`cloud_cover_${spec.name}`];
      const layer = layers[index];
      layer.dataset.cover = String(cover);
      drift(layer, spec, conditions);
      for (const cloud of layer.querySelectorAll('.cloud-sprite')) {
        const threshold = Number(cloud.dataset.index) * 100 / spec.count;
        cloud.style.opacity = String(Math.max(0, Math.min(1, (cover - threshold) / 16)));
      }
    });
  };
}

//
// Daylight and overcast lighting
//

const DAY = ['#587491', '#718baa', '#9db2c5', '#cad7e1', '#e9eff1', '#fffcf3'];
const DUSK = ['#655d91', '#8b709e', '#b98ba8', '#e1aaa9', '#f4c6ad', '#ffdfb9'];
const NIGHT = ['#172238', '#263850', '#3a4f6a', '#5b7490', '#849bb0', '#b4c6d5'];
const OVERCAST = ['#34475b', '#50657a', '#728597', '#98a7b4', '#b5c3cc', '#d9e1e6'];

const channels = hex => [1, 3, 5].map(offset => parseInt(hex.slice(offset, offset + 2), 16));
const blend = (a, b, weight) => a.map((value, index) => value * (1 - weight) + b[index] * weight);
currentPalette = DAY.map(channels);

export function colorClouds(sky, altitude, night, gloom = 0) {
  const twilight = Math.max(0, 1 - Math.abs(altitude) / 0.35);
  const next = DAY.map((color, index) => {
    const warm = blend(channels(color), channels(DUSK[index]), twilight);
    const shaded = blend(warm, channels(OVERCAST[index]), gloom * (1 - night));
    const tones = blend(shaded, channels(NIGHT[index]), night).map(Math.round);
    return tones;
  });
  if (JSON.stringify(next) === JSON.stringify(currentPalette)) return;
  currentPalette = next;
  repaint(sky);
}
