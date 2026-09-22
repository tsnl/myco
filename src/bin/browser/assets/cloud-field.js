import { random, noise, turbulence } from '/sky-noise.js';

//
// Cirrus, broken banks, and veils
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

function billows(roll, veil) {
  const count = 4 + Math.floor(roll() * 5), span = 0.85 + roll() * 0.4;
  return Array.from({ length: count }, (_, index) => ({
    x: (index / (count - 1) - 0.5) * span + (roll() - 0.5) * 0.18,
    y: (roll() - 0.5) * (veil ? 0.12 : 0.28),
    width: (veil ? 0.4 : 0.19) + roll() * 0.25,
    height: (veil ? 0.07 : 0.09) + roll() * (veil ? 0.08 : 0.15),
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
  const high = spec.form === 'cirrus', veil = spec.form === 'veil';
  const shapes = high ? strands(random(spec.seed)) : billows(random(spec.seed), veil);
  return (x, y) => {
    const warpX = (turbulence(sample, x * 3 + 8, y * 2.5 + 8) - 0.5) * 0.3;
    const warpY = (turbulence(sample, x * 3 + 50, y * 2.5 + 50) - 0.5) * 0.2;
    const detail = turbulence(sample, (x + warpX) * 9 + 20, (y + warpY) * (high ? 48 : veil ? 20 : 10) + 20);
    if (high) return envelope(x + warpX, y + warpY, shapes) * Math.max(0, detail - 0.24);
    const body = cloudBody(x + warpX, y + warpY, shapes);
    // Erosion opens gaps through the body and feathers its outline at several scales.
    return Math.max(0, body - (veil ? 0.3 : 0.38) + (detail - 0.5) * (veil ? 1.1 : 1.25));
  };
}

//
// Density and diffuse lighting
//

export function density(spec) {
  const { width, height, kind, seed, form } = spec;
  const field = cloudField(spec, noise(seed));
  const volumes = new Float32Array(width * height), alpha = new Uint8Array(width * height);
  const thickness = form === 'cirrus' ? 1.4 : form === 'veil' ? 1.8 : kind === 'mid' ? 2.4 : 3;
  for (let y = 0; y < height; y++) {
    for (let x = 0; x < width; x++) {
      const px = x / width * 2 - 1, py = y / height - 0.5, i = y * width + x;
      volumes[i] = field(px, py);
      // The outer fade prevents a texture boundary even on stretched wisps.
      const edge = Math.max(0, 1 - Math.pow(px, 8)) * Math.max(0, 1 - Math.pow(py * 2, 6));
      alpha[i] = (1 - Math.exp(-volumes[i] * thickness)) * edge * 200;
    }
  }
  return { volumes, alpha };
}

function transmittedLight(volumes, spec, x, y, direction) {
  let shadow = 0;
  // Broad extinction toward the light keeps the volume soft. Surface
  // normals over the fine noise would turn every wisp into a hard ridge.
  for (const step of [4, 9, 17, 29, 43]) {
    const sx = Math.round(x + step * direction[0]), sy = Math.round(y + step * direction[1]);
    if (sx < 0 || sx >= spec.width || sy < 0 || sy >= spec.height) break;
    shadow += volumes[sy * spec.width + sx] * 0.2;
  }
  return 0.62 + 0.32 * Math.exp(-shadow * 2.4);
}

function colorPixel(pixels, index, tone, palette) {
  const lower = Math.floor(tone), upper = Math.min(palette.length - 1, lower + 1);
  const fraction = tone - lower;
  for (let channel = 0; channel < 3; channel++) {
    pixels[index * 4 + channel] = palette[lower][channel] * (1 - fraction) + palette[upper][channel] * fraction;
  }
}

// Relighting never changes density or alpha, so weather and clock updates
// preserve the cloud outline. The worker owns and reuses these density maps.
export function paint(spec, field, lighting) {
  const { volumes, alpha } = field, { palette, direction, diffusion } = lighting;
  const pixels = new Uint8ClampedArray(spec.width * spec.height * 4);
  for (let i = 0; i < alpha.length; i++) {
    if (!alpha[i]) continue;
    const direct = spec.form === 'cirrus' ? 0.86 : transmittedLight(volumes, spec, i % spec.width, Math.floor(i / spec.width), direction);
    const light = direct * (1 - diffusion * 0.65) + 0.8 * diffusion * 0.65;
    colorPixel(pixels, i, light * (palette.length - 1), palette);
    pixels[i * 4 + 3] = alpha[i];
  }
  return pixels;
}
