import { random } from './sky-noise.js';

// Clouds are painted onto canvases, not composited as elements. The browser
// then draws one static texture per frame, and a frame is only needed when a
// cloud has moved: about once a second. The second canvas is a small blurred
// copy that stands in for frosted glass behind the conversation.
const LAYERS = [
  { name: 'high', count: 8, top: 9, spread: 25, width: 850, height: 220, resolution: 192, duration: 6800, opacity: 0.64 },
  { name: 'mid', count: 8, top: 32, spread: 26, width: 900, height: 440, resolution: 256, duration: 5100, opacity: 0.87 },
  { name: 'low', count: 8, top: 60, spread: 26, width: 1050, height: 580, resolution: 288, duration: 3700, opacity: 1 },
];
const TRACK = 1440, FADE = 12000, DRIFT_TICK = 1000, FADE_TICK = 250;
// The glass blurred its backdrop by 18px. The soft canvas works at an eighth
// of that scale, where the same blur is cheap.
const SOFT_SCALE = 1 / 8, SOFT_BLUR = 18;

const wrap = value => value - Math.floor(value);

//
// Fixed cloud geometry
//

function sprites(spec, layer) {
  const roll = random(943 + layer * 379);
  return Array.from({ length: spec.count }, (_, index) => {
    const form = layer === 0 ? 'cirrus' : index >= 4 && index % 2 === 0 ? 'veil' : 'bank';
    const scale = 0.75 + roll() * 0.5;
    const width = spec.width * scale * (form === 'veil' ? 1.25 : 1);
    const height = spec.height * scale * (0.8 + roll() * 0.4);
    return {
      width, height, reach: Math.hypot(width, height) / 2,
      top: (spec.top + roll() * spec.spread) / 100,
      tilt: (roll() - 0.5) * (24 - layer * 8) * Math.PI / 180,
      position: (index * 61.803 + layer * 23) % 100 / 100,
      threshold: index * 100 / spec.count,
      alpha: 0, target: 0, bitmap: null, incoming: null,
      texture: { id: `${spec.name}-${index}`, kind: spec.name, form, seed: 781 + layer * 100 + index * 7,
        width: 512, height: spec.resolution },
    };
  });
}

// A stand-in when the texture worker cannot run.
function fallback(texture, palette) {
  const canvas = document.createElement('canvas');
  canvas.width = texture.width; canvas.height = texture.height;
  const context = canvas.getContext('2d'), roll = random(texture.seed);
  context.scale(1, 0.4);
  for (let i = 0; i < 9; i++) {
    const x = canvas.width * (0.12 + i * 0.095), y = canvas.height * (0.9 + roll() * 0.65);
    const radius = canvas.height * (0.23 + roll() * 0.12);
    const glow = context.createRadialGradient(x, y, radius * 0.12, x, y, radius);
    const color = palette[3 + i % 3].join(' ');
    glow.addColorStop(0, `rgb(${color} / .35)`);
    glow.addColorStop(0.55, `rgb(${color} / .12)`);
    glow.addColorStop(1, `rgb(${color} / 0)`);
    context.fillStyle = glow;
    context.fillRect(x - radius, y - radius, radius * 2, radius * 2);
  }
  return canvas;
}

//
// Painting
//

function surface(id, scale, blur) {
  const canvas = document.createElement('canvas'), scratch = document.createElement('canvas');
  canvas.id = id;
  return { canvas, scratch, scale, blur, ratio: scale, margin: 0, width: 0, height: 0,
    context: canvas.getContext('2d'), group: scratch.getContext('2d') };
}

// A canvas follows its own box. One that is not displayed has no size.
function resize(target, { width, height }) {
  // Clouds are soft and their textures are stretched, so device pixels add nothing.
  target.ratio = Math.min(devicePixelRatio, 1) * target.scale;
  // A blur reads past the edge it is drawn to. Without clouds there, it would fade them.
  target.margin = Math.ceil(3 * target.blur * target.ratio);
  target.width = width; target.height = height;
  target.canvas.width = Math.max(1, Math.round(width * target.ratio));
  target.canvas.height = Math.max(1, Math.round(height * target.ratio));
  target.scratch.width = target.canvas.width + 2 * target.margin;
  target.scratch.height = target.canvas.height + 2 * target.margin;
}

// Each altitude is composited before it is faded, so overlapping clouds in
// one layer never add up to more than that layer's opacity.
function paint(target, layers) {
  const { context, group, ratio, margin, width, height } = target;
  if (!width) return;
  const track = Math.max(width, TRACK), left = (width - track) / 2, beyond = margin / ratio;
  context.clearRect(0, 0, target.canvas.width, target.canvas.height);
  if (target.blur && 'filter' in context) context.filter = `blur(${target.blur * ratio}px)`;
  for (const layer of layers) {
    group.setTransform(1, 0, 0, 1, 0, 0);
    group.clearRect(0, 0, target.scratch.width, target.scratch.height);
    for (const sprite of layer.sprites) {
      if (!sprite.alpha || !sprite.bitmap) continue;
      const x = left + wrap(sprite.position - layer.offset) * track, y = sprite.top * height;
      const cos = Math.cos(sprite.tilt) * ratio, sin = Math.sin(sprite.tilt) * ratio;
      group.globalAlpha = sprite.alpha;
      // Neighbouring copies cover both edges while a cloud wraps around.
      for (const copy of [x - track, x, x + track]) {
        if (copy + sprite.reach < -beyond || copy - sprite.reach > width + beyond) continue;
        group.setTransform(cos, sin, -sin, cos, copy * ratio + margin, y * ratio + margin);
        group.drawImage(sprite.bitmap, -sprite.width / 2, -sprite.height / 2, sprite.width, sprite.height);
      }
    }
    context.globalAlpha = layer.spec.opacity;
    context.drawImage(target.scratch, -margin, -margin);
  }
}

//
// Scene
//

export function createClouds(sky, open, well) {
  const layers = LAYERS.map((spec, index) => ({ spec, sprites: sprites(spec, index), offset: 0, speed: 0 }));
  const all = layers.flatMap(layer => layer.sprites);
  const sharp = surface('sky-clouds', 1, 0), soft = surface('sky-clouds-soft', SOFT_SCALE, SOFT_BLUR);
  const motion = matchMedia('(prefers-reduced-motion: reduce)');
  let moved = performance.now(), timer = 0, queued = false;
  let worker = null, broken = false, generation = 0, lighting = null, key = '';
  open.append(sharp.canvas); well.append(soft.canvas);

  // Drift and fades follow elapsed time, so they stay continuous across a
  // change of wind and a hidden tab's missed ticks.
  function advance() {
    const now = performance.now(), elapsed = now - moved;
    moved = now;
    let fading = false;
    for (const layer of layers) {
      if (!motion.matches) layer.offset = wrap(layer.offset + layer.speed * elapsed);
      for (const sprite of layer.sprites) {
        const step = motion.matches ? 1 : elapsed / FADE, gap = sprite.target - sprite.alpha;
        sprite.alpha = Math.abs(gap) <= step ? sprite.target : sprite.alpha + Math.sign(gap) * step;
        fading ||= sprite.alpha !== sprite.target;
      }
    }
    return fading;
  }

  function render() {
    queued = false;
    clearTimeout(timer);
    if (document.hidden) return;
    const fading = advance();
    for (const target of [sharp, soft]) paint(target, layers);
    sharp.canvas.dataset.drift = layers.map(layer => layer.offset.toFixed(5)).join(' ');
    if (fading || !motion.matches) timer = setTimeout(render, fading ? FADE_TICK : DRIFT_TICK);
  }

  // Several changes in one task repaint once.
  function invalidate() {
    if (queued) return;
    queued = true;
    queueMicrotask(render);
  }

  // Bitmaps hold memory outside the JavaScript heap until they are closed.
  function replace(sprite, field, bitmap) {
    sprite[field]?.close?.();
    sprite[field] = bitmap;
  }

  function relight() {
    generation++;
    if (!lighting) return;
    for (const sprite of all) replace(sprite, 'incoming', null);
    if (broken) {
      for (const sprite of all) replace(sprite, 'bitmap', fallback(sprite.texture, lighting.palette));
      invalidate();
    } else if (worker) {
      sky.dataset.clouds = 'painting';
      worker.postMessage({ specs: all.map(sprite => sprite.texture), lighting, generation });
    }
  }

  function fail() {
    worker?.terminate(); worker = null; broken = true;
    sky.dataset.clouds = 'fallback';
    relight();
  }

  function accept(data) {
    // A changed city or clock can supersede textures still being painted.
    if (data.generation !== generation) { data.bitmap.close(); return; }
    replace(all.find(sprite => sprite.texture.id === data.id), 'incoming', data.bitmap);
    if (!all.every(sprite => sprite.incoming)) return;
    // Every cloud takes the new light in the same frame.
    for (const sprite of all) { replace(sprite, 'bitmap', sprite.incoming); sprite.incoming = null; }
    sky.dataset.clouds = 'ready';
    invalidate();
  }

  function start() {
    try {
      worker = new Worker(new URL('./cloud-renderer.js', import.meta.url), { type: 'module' });
      worker.onmessage = ({ data }) => accept(data);
      worker.onerror = event => { event.preventDefault(); fail(); };
      const pause = () => worker?.postMessage({ paused: document.hidden });
      document.addEventListener('visibilitychange', pause);
      pause();
      relight();
    } catch { fail(); }
  }

  const sized = new ResizeObserver(entries => {
    for (const entry of entries) resize(entry.target === sharp.canvas ? sharp : soft, entry.contentRect);
    invalidate();
  });
  sized.observe(sharp.canvas); sized.observe(soft.canvas);
  document.addEventListener('visibilitychange', invalidate);
  motion.addEventListener('change', invalidate);
  requestAnimationFrame(start);

  return {
    setWeather(conditions) {
      advance();
      for (const layer of layers) {
        const cover = conditions[`cloud_cover_${layer.spec.name}`];
        sharp.canvas.dataset[layer.spec.name] = String(cover);
        // Meteorological direction names where the wind comes from.
        const direction = conditions.wind_direction_10m >= 180 ? -1 : 1;
        layer.speed = direction * (0.7 + Math.min(conditions.wind_speed_10m, 20) / 30) / (layer.spec.duration * 1000);
        for (const sprite of layer.sprites) sprite.target = Math.max(0, Math.min(1, (cover - sprite.threshold) / 16));
      }
      invalidate();
    },
    setLighting(next) {
      const nextKey = JSON.stringify(next);
      if (nextKey === key) return;
      key = nextKey; lighting = next;
      relight();
    },
  };
}
