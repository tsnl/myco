import { random } from '/sky-noise.js';
import { CloudTextures } from '/cloud-textures.js';

const LAYERS = [
  { name: 'high', altitude: '8+', count: 8, top: 9, spread: 25, width: 850, height: 220, resolution: 192, duration: 6800 },
  { name: 'mid', altitude: '3–8', count: 8, top: 32, spread: 26, width: 900, height: 440, resolution: 256, duration: 5100 },
  { name: 'low', altitude: '0–3', count: 8, top: 60, spread: 26, width: 1050, height: 580, resolution: 288, duration: 3700 },
];

//
// Altitude layers and wind
//

function sprite(spec, layer, index, form, roll) {
  const cloud = document.createElement('div');
  cloud.className = 'cloud-sprite';
  cloud.dataset.index = String(index);
  cloud.dataset.form = form;
  const scale = 0.75 + roll() * 0.5;
  cloud.style.width = `${spec.width * scale * (form === 'veil' ? 1.25 : 1)}px`;
  cloud.style.height = `${spec.height * scale * (0.8 + roll() * 0.4)}px`;
  cloud.style.top = `${spec.top + roll() * spec.spread}%`;
  cloud.style.setProperty('--cloud-tilt', `${(roll() - 0.5) * (24 - layer * 8)}deg`);
  return cloud;
}

function wrapCloud(track, cloud, texture, position) {
  // Three copies cover both viewport edges throughout either wind direction.
  for (const offset of [-100, 0, 100]) {
    const copy = cloud.cloneNode(true), canvas = document.createElement('canvas');
    copy.style.left = `${position + offset}%`;
    canvas.width = texture.spec.width; canvas.height = texture.spec.height;
    copy.append(canvas); texture.canvases.push(canvas);
    track.append(copy);
  }
}

function makeLayer(spec, index, textures) {
  const layer = document.createElement('div');
  layer.className = `cloud-layer cloud-${spec.name}`;
  layer.dataset.layer = spec.name;
  layer.dataset.altitude = spec.altitude;
  const track = document.createElement('div');
  track.className = 'cloud-track';
  const roll = random(943 + index * 379);
  for (let i = 0; i < spec.count; i++) {
    const form = index === 0 ? 'cirrus' : i >= 4 && i % 2 === 0 ? 'veil' : 'bank';
    const cloud = sprite(spec, index, i, form, roll);
    const id = `${spec.name}-${i}`;
    const texture = { spec: { id, kind: spec.name, form, seed: 781 + index * 100 + i * 7,
      width: 512, height: spec.resolution }, canvases: [] };
    textures.set(id, texture);
    wrapCloud(track, cloud, texture, (i * 61.803 + index * 23) % 100);
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

function coverage(layer, spec, conditions) {
  const cover = conditions[`cloud_cover_${spec.name}`];
  layer.dataset.cover = String(cover);
  drift(layer, spec, conditions);
  for (const cloud of layer.querySelectorAll('.cloud-sprite')) {
    const threshold = Number(cloud.dataset.index) * 100 / spec.count;
    cloud.style.opacity = String(Math.max(0, Math.min(1, (cover - threshold) / 16)));
  }
}

export function createClouds(sky) {
  const clouds = document.createElement('div'), textures = new Map();
  clouds.id = 'sky-clouds';
  const layers = LAYERS.map((spec, index) => makeLayer(spec, index, textures));
  const renderer = new CloudTextures(sky, textures);
  clouds.append(...layers); sky.append(clouds);
  requestAnimationFrame(() => renderer.start());
  return {
    setWeather: conditions => LAYERS.forEach((spec, index) => coverage(layers[index], spec, conditions)),
    setLighting: lighting => renderer.setLighting(lighting),
  };
}
