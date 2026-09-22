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

//
// Cirrus and billows
//

function strands(roll) {
  return Array.from({ length: 11 }, () => ({
    x: (roll() - 0.5) * 0.5, y: (roll() - 0.5) * 0.3,
    width: 0.4 + roll() * 0.5, thickness: 0.018 + roll() * 0.035,
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

function billows(roll) {
  return Array.from({ length: 7 }, (_, index) => ({
    x: (index - 3) * 0.19 + (roll() - 0.5) * 0.16,
    y: (roll() - 0.5) * 0.2,
    width: 0.24 + roll() * 0.22,
    height: 0.1 + roll() * 0.12,
  }));
}

function cloudBody(x, y, shapes) {
  let distance = Infinity;
  for (const shape of shapes) {
    const dx = (x - shape.x) / shape.width, dy = (y - shape.y) / shape.height;
    distance = Math.min(distance, dx * dx + dy * dy);
  }
  return Math.exp(-distance * 1.6);
}

function cloudField(spec, sample) {
  const high = spec.kind === 'high';
  const shapes = high ? strands(random(spec.seed)) : billows(random(spec.seed));
  return (x, y) => {
    const warpX = (turbulence(sample, x * 3 + 8, y * 2.5 + 8) - 0.5) * 0.3;
    const warpY = (turbulence(sample, x * 3 + 50, y * 2.5 + 50) - 0.5) * 0.2;
    const detail = turbulence(sample, (x + warpX) * 9 + 20, (y + warpY) * (high ? 48 : 10) + 20);
    if (high) return envelope(x + warpX, y + warpY, shapes) * Math.max(0, detail - 0.24);
    const body = cloudBody(x + warpX, y + warpY, shapes);
    // Erosion opens gaps through the body and feathers its outline at several scales.
    return Math.max(0, body - 0.38 + (detail - 0.5) * 1.25);
  };
}

//
// Density and diffuse lighting
//

function density(spec) {
  const { width, height, kind, seed } = spec;
  const field = cloudField(spec, noise(seed));
  const volumes = new Float32Array(width * height);
  const samples = new Uint8Array(width * height * 2);
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      const px = x / width * 2 - 1, py = y / height - 0.5, i = y * width + x;
      const volume = field(px, py);
      // The outer fade prevents a texture boundary even on stretched wisps.
      const edge = Math.max(0, 1 - Math.pow(px, 8)) * Math.max(0, 1 - Math.pow(py * 2, 6));
      const thickness = kind === 'high' ? 1.4 : kind === 'mid' ? 2.4 : 3;
      samples[i * 2] = (1 - Math.exp(-volume * thickness)) * edge * 200;
      samples[i * 2 + 1] = 220;
      volumes[i] = volume;
    }
  }
  if (kind !== 'high') illuminate(samples, volumes, width, height);
  return samples;
}

function transmittedLight(volumes, width, x, y) {
  let shadow = 0;
  // Broad extinction toward the light keeps the volume soft. Surface
  // normals over the fine noise would turn every wisp into a hard ridge.
  for (const step of [4, 9, 17, 29, 43]) {
    if (x < step || y < step) break;
    shadow += volumes[(y - step) * width + x - step] * 0.2;
  }
  return 0.62 + 0.32 * Math.exp(-shadow * 2.4);
}

function illuminate(samples, volumes, width, height) {
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      const i = y * width + x;
      if (!samples[i * 2]) continue;
      samples[i * 2 + 1] = transmittedLight(volumes, width, x, y) * 255;
    }
  }
}

//
// Texture delivery
//

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
