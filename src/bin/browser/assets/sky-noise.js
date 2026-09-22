// Deterministic fields keep cloud shapes stable when weather or lighting changes.
export function random(seed) {
  return () => { seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0; return seed / 4294967296; };
}

export function noise(seed) {
  const grid = Float32Array.from({ length: 16384 }, random(seed));
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

export function turbulence(sample, x, y) {
  return sample(x, y) * 0.52 + sample(x * 2.03 + 17, y * 2.03 + 9) * 0.27
    + sample(x * 4.07 + 31, y * 4.07 + 21) * 0.14 + sample(x * 8.13, y * 8.13) * 0.07;
}
