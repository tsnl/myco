import { density, paint } from '/cloud-field.js';

// The scene sends a fixed, bounded set of specs. Cache their density in this
// worker; only lighting changes repaint them, and CSS handles all motion.
const fields = new Map();
let pending = [], lighting, generation = 0, paused = false, scheduled = false;

function render(spec) {
  if (!fields.has(spec.id)) fields.set(spec.id, density(spec));
  const pixels = paint(spec, fields.get(spec.id), lighting);
  self.postMessage({ id: spec.id, width: spec.width, height: spec.height, generation, pixels }, [pixels.buffer]);
}

function schedule() {
  if (paused || scheduled || !pending.length) return;
  scheduled = true;
  setTimeout(() => {
    scheduled = false;
    if (paused) return;
    render(pending.shift());
    schedule();
  }, 0);
}

self.onmessage = ({ data }) => {
  if ('paused' in data) paused = data.paused;
  if (data.specs) { pending = data.specs.slice(); lighting = data.lighting; generation = data.generation; }
  schedule();
};
