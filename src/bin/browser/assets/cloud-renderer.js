// Cloud density and lighting are prepared off the UI thread. Finished textures
// only move in CSS; a minute-by-minute palette change reuses the density maps.
const cache = new Map();
let pending = [], palette, generation = 0, paused = false, scheduled = false;

function random(seed) {
  return () => { seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0; return seed / 4294967296; };
}

function noise(seed) {
  const roll = random(seed), grid = Float32Array.from({ length: 16384 }, roll);
  return (x, y) => {
    const ix = Math.floor(x), iy = Math.floor(y);
    const dx = x - ix, dy = y - iy;
    const u = dx * dx * (3 - 2 * dx), v = dy * dy * (3 - 2 * dy);
    const a = grid[(iy & 127) * 128 + (ix & 127)];
    const b = grid[(iy & 127) * 128 + ((ix + 1) & 127)];
    const c = grid[((iy + 1) & 127) * 128 + (ix & 127)];
    const d = grid[((iy + 1) & 127) * 128 + ((ix + 1) & 127)];
    return (a + (b - a) * u) * (1 - v) + (c + (d - c) * u) * v;
  };
}

function turbulence(sample, x, y) {
  return sample(x, y) * 0.52 + sample(x * 2.03 + 17, y * 2.03 + 9) * 0.27
    + sample(x * 4.07 + 31, y * 4.07 + 21) * 0.14 + sample(x * 8.13, y * 8.13) * 0.07;
}

function billows(kind, roll) {
  const shapes = [{ x: 0, y: 0.21, rx: 0.83, ry: 0.16, depth: 0.38 }];
  for (let i = 0; i < 7; i++) {
    const crown = Math.sin((i + 1) / 8 * Math.PI);
    const x = -0.77 + i * 0.245 + roll() * 0.08;
    const y = 0.16 - crown * (kind === 'low' ? 0.33 : 0.14) + roll() * 0.13;
    shapes.push({ x, y, rx: Math.min(0.96 - Math.abs(x), 0.18 + roll() * 0.16),
      ry: Math.min(0.46 - Math.abs(y), 0.12 + crown * 0.18 + roll() * 0.07), depth: 0.55 + crown * 0.45 });
  }
  return shapes;
}

function envelope(x, y, shapes) {
  let value = -1, depth = 0;
  for (const shape of shapes) {
    const dx = (x - shape.x) / shape.rx, dy = (y - shape.y) / shape.ry;
    const next = 1 - dx * dx - dy * dy;
    const join = Math.max(0, 0.22 - Math.abs(value - next));
    value = Math.max(value, next) + join * join / 0.88;
    if (next > 0) {
      const surface = Math.sqrt(next) * shape.depth;
      const merge = Math.max(0, 0.12 - Math.abs(depth - surface));
      depth = Math.max(depth, surface) + merge * merge / 0.48;
    }
  }
  return [value, depth];
}

function cirrus(x, y, sample, seed) {
  const bend = 0.11 * Math.sin(x * 2.6 + seed) - x * 0.16;
  const wisps = turbulence(sample, x * 3 + 40, y * 27 + 40);
  const distance = Math.abs(y - bend + (sample(x * 5 + 9, y * 3 + 7) - 0.5) * 0.15);
  const taper = Math.max(0, 1 - x * x);
  const density = Math.max(0, (0.15 * taper - distance) * 6 + (wisps - 0.5) * 0.8);
  return [(1 - Math.exp(-density * 2.8)) * Math.min(1, taper * 8), 0.66 + wisps * 0.22];
}

function density(spec) {
  const { width, height, kind, seed } = spec;
  const sample = noise(seed), shapes = billows(kind, random(seed));
  const surface = new Float32Array(width * height);
  const samples = new Uint8Array(width * height * 2);
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      const px = x / width * 2 - 1, py = y / height - 0.5, i = y * width + x;
      if (kind === 'high') {
        const [alpha, light] = cirrus(px, py, sample, seed);
        samples[i * 2] = alpha * 255; samples[i * 2 + 1] = light * 255;
        continue;
      }
      const warpX = (sample(px * 5 + 8, py * 9 + 8) - 0.5) * 0.075;
      const warpY = (sample(px * 7 + 50, py * 8 + 50) - 0.5) * 0.075;
      const [shape, depth] = envelope(px + warpX, py + warpY, shapes);
      const detail = turbulence(sample, px * 12 + 20, py * 15 + 20);
      const volume = Math.max(0, shape - 0.15 + (detail - 0.5) * 0.55);
      samples[i * 2] = (1 - Math.exp(-volume * 6)) * 255;
      const billow = turbulence(sample, px * 5 + 20, py * 6 + 20);
      surface[i] = depth * 0.30 + billow * 0.035 + detail * 0.008;
    }
  }
  if (kind !== 'high') illuminate(samples, surface, width, height);
  return samples;
}

function illuminate(samples, surface, width, height) {
  for (let y = 3; y < height - 3; y++) {
    for (let x = 3; x < width - 3; x++) {
      const i = y * width + x;
      if (!samples[i * 2]) continue;
      const nx = (surface[i - 3] - surface[i + 3]) * width / 18;
      const ny = (surface[i - width * 3] - surface[i + width * 3]) * height / 10;
      const sunlight = Math.max(0, (-nx * 0.48 - ny * 0.64 + 0.60) / Math.hypot(nx, ny, 1));
      let shadow = 0;
      for (const step of [6, 14, 28, 44]) {
        if (x < step || y < step) break;
        shadow += Math.max(0, surface[i - step * (width + 1)] - surface[i] - step * 0.0014);
      }
      const ambient = 0.34 + (1 - y / height) * 0.18;
      const light = ambient + sunlight * 0.54 * Math.exp(-shadow * 1.4);
      samples[i * 2 + 1] = Math.min(1, light) * 255;
    }
  }
}

function paint(spec) {
  let samples = cache.get(spec.id);
  if (!samples) { samples = density(spec); cache.set(spec.id, samples); }
  const pixels = new Uint8ClampedArray(spec.width * spec.height * 4);
  for (let i = 0; i < samples.length / 2; i++) {
    const tone = samples[i * 2 + 1] / 255 * (palette.length - 1);
    const lower = Math.floor(tone), upper = Math.min(palette.length - 1, lower + 1);
    const fraction = tone - lower;
    for (let channel = 0; channel < 3; channel++) {
      pixels[i * 4 + channel] = palette[lower][channel] * (1 - fraction) + palette[upper][channel] * fraction;
    }
    pixels[i * 4 + 3] = samples[i * 2];
  }
  self.postMessage({ id: spec.id, width: spec.width, height: spec.height, generation, pixels }, [pixels.buffer]);
}

function schedule() {
  if (paused || scheduled || !pending.length) return;
  scheduled = true;
  setTimeout(() => {
    scheduled = false;
    if (paused) return;
    paint(pending.shift());
    schedule();
  }, 0);
}

self.onmessage = ({ data }) => {
  if ('paused' in data) paused = data.paused;
  if (data.specs) { pending = data.specs.slice(); palette = data.palette; generation = data.generation; }
  schedule();
};
