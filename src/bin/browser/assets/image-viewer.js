let viewer = null;

function createViewer() {
  const dialog = document.createElement('dialog');
  dialog.id = 'image-viewer'; dialog.setAttribute('aria-label', 'Image viewer');
  dialog.innerHTML = `
    <button type="button" class="image-viewer-close" aria-label="Close image viewer" title="Close (Escape)">×</button>
    <img class="image-viewer-image" alt="" referrerpolicy="no-referrer" hidden>
    <p role="status">Loading image…</p>`;
  document.body.append(dialog);
  const image = dialog.querySelector('img'), status = dialog.querySelector('[role="status"]');
  const close = dialog.querySelector('button');
  let opener = null;
  image.onload = () => { if (dialog.open) { image.hidden = false; status.hidden = true; } };
  image.onerror = () => { if (dialog.open) { image.hidden = true; status.hidden = false; status.textContent = 'Image unavailable.'; } };
  close.onclick = () => dialog.close();
  dialog.onclick = event => { if (event.target === dialog) dialog.close(); };
  dialog.onclose = () => {
    if (dialog.open) return;
    document.documentElement.classList.remove('viewing-image');
    image.removeAttribute('src');
    (opener?.isConnected ? opener : document.getElementById('prompt'))?.focus({ preventScroll: true });
    opener = null;
  };
  return source => {
    opener = source;
    image.hidden = true; status.hidden = false; status.textContent = 'Loading image…';
    image.alt = source.alt || 'Image from the conversation';
    document.documentElement.classList.add('viewing-image');
    dialog.showModal();
    image.src = source.currentSrc || source.src;
    close.focus({ preventScroll: true });
  };
}

// Bind at image creation so streamed Markdown, tool output, and composer previews
// all share one viewer without scanning the conversation on every update.
export function imagePreview(image) {
  if (image.classList.contains('image-preview')) return;
  viewer ||= createViewer();
  image.classList.add('image-preview'); image.tabIndex = 0;
  image.setAttribute('role', 'button');
  image.setAttribute('aria-label', `View image: ${image.alt || 'Image'}`);
  image.setAttribute('aria-haspopup', 'dialog');
  image.setAttribute('aria-controls', 'image-viewer');
  const open = event => {
    if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    event.preventDefault(); event.stopPropagation();
    viewer(image);
  };
  image.addEventListener('click', open);
  image.addEventListener('keydown', event => {
    if (!event.isComposing && (event.key === 'Enter' || event.key === ' ')) open(event);
  });
}
