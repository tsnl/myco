// Cloud density and lighting are prepared off the UI thread. Finished textures
// only move in CSS; a minute-by-minute palette change reuses the density maps.
const cache = new Map();
let pending = [], palette, generation = 0, paused = false, scheduled = false;

//
// Seeded cloud fields
//

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

function strands(kind, roll) {
  return Array.from({ length: kind === 'high' ? 11 : 9 }, () => ({
    x: (roll() - 0.5) * 0.5, y: (roll() - 0.5) * 0.3,
    width: 0.4 + roll() * 0.5, thickness: (kind === 'high' ? 0.018 : 0.038) + roll() * 0.035,
    bend: (roll() - 0.5) * 0.25, slope: (roll() - 0.5) * 0.22,
    phase: roll() * Math.PI * 2, strength: 0.35 + roll() * 0.45,
  }));
}

function envelope(x, y, shapes) {
  let density = 0;
  for (const s of shapes) {
    const along = (x - s.x) / s.width, taper = Math.exp(-Math.pow(along, 4) * 2);
    const center = s.y + x * s.slope + x * x * s.bend + Math.sin(x * 3 + s.phase) * 0.035;
    const across = (y - center) / (s.thickness * (0.35 + taper * 0.65));
    density += Math.exp(-across * across) * taper * s.strength;
  }
  return density;
}

function density(spec) {
  const { width, height, kind, seed } = spec;
  const sample = noise(seed), shapes = strands(kind, random(seed));
  const surface = new Float32Array(width * height);
  const samples = new Uint8Array(width * height * 2);
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      const px = x / width * 2 - 1, py = y / height - 0.5, i = y * width + x;
      const warpX = (sample(px * 3 + 8, py * 8 + 8) - 0.5) * 0.12;
      const warpY = (turbulence(sample, px * 2.2 + 50, py * 6 + 50) - 0.5) * 0.19;
      const shape = envelope(px + warpX, py + warpY, shapes);
      const fibers = turbulence(sample, px * 4 + 20, (py + warpY) * 48 + 20);
      const detail = Math.max(0, (fibers - 0.22) / 0.78);
      const volume = shape * (0.2 + detail * 1.5);
      // The outer fade prevents a texture boundary even on stretched wisps.
      const edge = Math.max(0, 1 - Math.pow(px, 8)) * Math.max(0, 1 - Math.pow(py * 2, 6));
      const thickness = kind === 'high' ? 0.9 : kind === 'mid' ? 1.3 : 1.7;
      samples[i * 2] = (1 - Math.exp(-volume * thickness)) * edge * 220;
      samples[i * 2 + 1] = (0.74 + detail * 0.18) * 255;
      surface[i] = shape * 0.065 + detail * 0.012;
    }
  }
  if (kind !== 'high') illuminate(samples, surface, width, height);
  return samples;
}

//
// Lighting and texture delivery
//

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
