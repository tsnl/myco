// A soft glow follows the highlight around the composer. Blurring a moving
// highlight means blurring again for every frame, so the blur is drawn once
// here, as a mask, and the highlight turns beneath it.
const REACH = 96;
// Deviation, strength, and drop of each layer of glow, tightest first.
const GLOW = [[2, 0.85, 0], [10, 0.6, 0], [28, 0.35, 10]];

export function composerLight(frame) {
  const glow = frame.querySelector('.composer-light-glow');
  let drawn = '';
  // Call when the frame may have changed size or state. Idle frames show no light.
  return () => {
    if (frame.dataset.state === 'ready') return;
    const width = frame.offsetWidth, height = frame.offsetHeight, size = `${width}x${height}`;
    if (size === drawn) return;
    drawn = size;
    const canvas = document.createElement('canvas');
    canvas.width = width + 2 * REACH; canvas.height = height + 2 * REACH;
    const context = canvas.getContext('2d');
    context.lineWidth = 2;
    context.strokeRect(REACH + 1, REACH + 1, width - 2, height - 2);
    // Each layer is the shadow of everything drawn so far, laid beneath it.
    // Drawing the source a canvas-width away leaves only its shadow in view.
    context.globalCompositeOperation = 'destination-over';
    context.shadowOffsetX = canvas.width;
    for (const [deviation, strength, drop] of GLOW) {
      context.shadowColor = `rgb(0 0 0 / ${strength})`;
      context.shadowBlur = 2 * deviation; context.shadowOffsetY = drop;
      context.drawImage(canvas, -canvas.width, 0);
    }
    glow.style.maskImage = `url(${canvas.toDataURL()})`;
    glow.hidden = false;
  };
}
