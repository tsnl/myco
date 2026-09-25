import { random } from './sky-noise.js';

//
// Cached image textures
//

// Cloud pixels change only with lighting. Share a lossless image across the
// wrapping copies so their canvases do not become separate compositor surfaces.
function encodeTexture(spec, draw) {
  const canvas = document.createElement('canvas');
  canvas.width = spec.width; canvas.height = spec.height;
  canvas.getContext('2d', { willReadFrequently: true });
  draw(canvas);
  return canvas.toDataURL();
}

async function updateTexture(texture, data) {
  const source = encodeTexture(data, canvas => {
    canvas.getContext('2d').putImageData(new ImageData(data.pixels, data.width, data.height), 0, 0);
  });
  for (const image of texture.images) image.src = source;
  await Promise.all(texture.images.map(image => image.decode()));
}

//
// Lightweight fallback
//

function fallback(canvas, seed, palette) {
  const context = canvas.getContext('2d'), roll = random(seed);
  context.clearRect(0, 0, canvas.width, canvas.height);
  context.save(); context.scale(1, 0.4);
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
  context.restore();
}

//
// Scene-owned worker and texture delivery
//

export class CloudTextures {
  constructor(sky, textures) {
    this.sky = sky;
    this.textures = textures;
    this.generation = 0;
    this.worker = null;
  }

  start() {
    try {
      this.worker = new Worker(new URL('./cloud-renderer.js', import.meta.url), { type: 'module' });
      this.worker.onmessage = ({ data }) => this.accept(data);
      this.worker.onerror = event => { event.preventDefault(); this.fail(); };
      const pause = () => this.worker?.postMessage({ paused: document.hidden });
      document.addEventListener('visibilitychange', pause);
      pause();
      this.repaint();
    } catch { this.fail(); }
  }

  setLighting(lighting) {
    const key = JSON.stringify(lighting);
    if (key === this.key) return;
    this.key = key;
    this.lighting = lighting;
    this.repaint();
  }

  repaint() {
    this.generation++;
    if (!this.worker) { this.renderFallback(); return; }
    this.sky.dataset.clouds = 'painting';
    for (const texture of this.textures.values()) texture.ready = false;
    this.worker.postMessage({ specs: [...this.textures.values()].map(item => item.spec),
      lighting: this.lighting, generation: this.generation });
  }

  async accept(data) {
    // A changed city or clock can supersede textures still being painted.
    if (data.generation !== this.generation) return;
    const texture = this.textures.get(data.id);
    try { await updateTexture(texture, data); }
    catch {
      if (data.generation === this.generation) this.fail();
      return;
    }
    if (data.generation !== this.generation) return;
    texture.ready = true;
    if ([...this.textures.values()].every(item => item.ready)) this.sky.dataset.clouds = 'ready';
  }

  fail() {
    this.worker?.terminate(); this.worker = null;
    // Image decoding may still finish after the worker stops.
    this.generation++;
    this.sky.dataset.clouds = 'fallback';
    this.renderFallback();
  }

  renderFallback() {
    if (!this.lighting) return;
    for (const texture of this.textures.values()) {
      const source = encodeTexture(texture.spec, canvas => fallback(canvas, texture.spec.seed, this.lighting.palette));
      for (const image of texture.images) image.src = source;
    }
  }
}
