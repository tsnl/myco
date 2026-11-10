import { density, paint } from './cloud-field.js';

// The scene sends a fixed, bounded set of specs. Cache their density in this
// worker; only lighting changes repaint them, and CSS handles all motion.
const fields = new Map();
let pending = [], lighting, generation = 0, paused = false, scheduled = false;

// A bitmap is ready to draw: the page neither encodes nor decodes an image.
async function render(spec) {
  if (!fields.has(spec.id)) fields.set(spec.id, density(spec));
  const reply = { id: spec.id, generation };
  const pixels = paint(spec, fields.get(spec.id), lighting);
  const bitmap = await createImageBitmap(new ImageData(pixels, spec.width, spec.height));
  self.postMessage({ ...reply, bitmap }, [bitmap]);
}

function schedule() {
  if (paused || scheduled || !pending.length) return;
  scheduled = true;
  setTimeout(() => {
    scheduled = false;
    if (paused) return;
    // An error inside a promise would vanish; rethrow it for the scene's handler.
    render(pending.shift()).catch(error => setTimeout(() => { throw error; }));
    schedule();
  }, 0);
}

self.onmessage = ({ data }) => {
  if ('paused' in data) paused = data.paused;
  if (data.specs) { pending = data.specs.slice(); lighting = data.lighting; generation = data.generation; }
  schedule();
};
