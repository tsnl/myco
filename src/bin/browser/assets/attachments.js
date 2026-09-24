import { $, element, error, requestId } from './common.js';
import { imagePreview } from './image-viewer.js';

const supported = file => /^image\/(png|jpeg|gif|webp)$/.test(file.type) || /\.(png|jpe?g|gif|webp)$/i.test(file.name);
const encodedSize = file => Math.ceil(file.size / 3) * 4;
const mib = bytes => `${(bytes / 1024 / 1024).toFixed(1)} MiB`;

export function imageAttachments(limits, changed, imageSource = source => source) {
  let items = [], locked = false;
  const composer = $('composer'), picker = $('attachment-picker');

  function render() {
    const focused = document.activeElement?.dataset.attachment;
    $('attachments').hidden = !items.length;
    $('attachment-count').textContent = `${items.length} ${items.length === 1 ? 'image' : 'images'} attached`;
    $('attachment-list').replaceChildren(...items.map(item => {
      const card = element('li', `attachment${item.error ? ' invalid' : ''}`);
      const preview = item.source ? element('img') : element('span', 'attachment-placeholder', item.error ? 'Unavailable' : 'Reading…');
      if (item.source) { preview.src = imageSource(item.source); preview.alt = item.file.name; imagePreview(preview); }
      const name = element('span', 'attachment-name', item.file.name); name.title = item.file.name;
      const remove = element('button', 'remove-attachment', '×');
      remove.type = 'button'; remove.disabled = locked; remove.dataset.attachment = item.id;
      remove.setAttribute('aria-label', `Remove ${item.file.name}`);
      remove.onclick = () => { if (!locked) { discard([item.id]); $('prompt').focus(); } };
      card.append(preview, name, remove);
      return card;
    }));
    if (focused) document.querySelector(`[data-attachment="${focused}"]`)?.focus({ preventScroll: true });
    changed();
  }

  function fail(item, message) {
    if (!items.includes(item)) return;
    item.error = true; error(`${item.file.name}: ${message}`); render();
  }
  function read(item) {
    const reader = new FileReader(); item.reader = reader;
    reader.onerror = () => fail(item, 'Could not read the image. Remove it and try again.');
    reader.onload = () => {
      if (!items.includes(item)) return;
      const preview = new Image();
      preview.onerror = () => fail(item, 'Could not open this image. Choose a PNG, JPEG, GIF, or WebP image.');
      preview.onload = () => {
        if (!items.includes(item)) return;
        item.source = reader.result; render();
      };
      preview.src = reader.result;
    };
    reader.readAsDataURL(item.file);
  }
  function add(files) {
    if (!files.length) return;
    if (locked) { error('Wait for this message to send before attaching images.'); return; }
    const cap = limits();
    if (!cap) { error('Wait for the session to connect before attaching images.'); return; }
    const invalid = files.find(file => !supported(file));
    if (invalid) { error(`${invalid.name}: choose a PNG, JPEG, GIF, or WebP image.`); return; }
    if (items.length + files.length > cap.max_images) { error(`Attach at most ${cap.max_images} images per message.`); return; }
    const large = files.find(file => encodedSize(file) > cap.max_image_base64_bytes);
    if (large) { error(`${large.name} exceeds this model's image limit of ${mib(Math.floor(cap.max_image_base64_bytes / 4) * 3)} per file. Resize or re-compress it.`); return; }
    const total = [...items.map(item => item.file), ...files].reduce((sum, file) => sum + encodedSize(file) + 64, 0);
    if (total > cap.max_message_attachment_bytes) { error('Images exceed this message’s total size limit. Choose fewer images.'); return; }
    error();
    const added = files.map(file => ({ id: requestId(), file, source: null, error: false }));
    items.push(...added); render(); added.forEach(read);
  }
  function discard(ids) {
    for (const item of items) if (ids.includes(item.id) && item.reader?.readyState === FileReader.LOADING) item.reader.abort();
    items = items.filter(item => !ids.includes(item.id)); error(); render();
  }

  $('attach').onclick = () => picker.click();
  picker.onchange = () => { add(Array.from(picker.files)); picker.value = ''; };
  composer.addEventListener('paste', event => {
    const files = Array.from(event.clipboardData?.files || []);
    if (!files.length) return;
    event.preventDefault();
    if (locked) { error('Wait for this message to send before attaching images.'); return; }
    const text = event.clipboardData.getData('text/plain');
    if (text) {
      const prompt = $('prompt');
      prompt.setRangeText(text, prompt.selectionStart, prompt.selectionEnd, 'end');
      prompt.dispatchEvent(new Event('input', { bubbles: true }));
    }
    add(files);
  });
  composer.addEventListener('dragover', event => {
    if (!Array.from(event.dataTransfer.types).includes('Files')) return;
    event.preventDefault(); event.dataTransfer.dropEffect = 'copy'; composer.classList.add('dragging-images');
  });
  composer.addEventListener('dragleave', event => { if (!composer.contains(event.relatedTarget)) composer.classList.remove('dragging-images'); });
  composer.addEventListener('drop', event => {
    composer.classList.remove('dragging-images');
    if (!event.dataTransfer.files.length) return;
    event.preventDefault(); add(Array.from(event.dataTransfer.files));
  });

  return {
    get ready() { return items.every(item => item.source && !item.error); },
    get ids() { return items.map(item => item.id); },
    get sources() { return items.map(item => item.source); },
    capture() { return items.slice(); },
    restore(saved) {
      for (const item of items) if (!saved.includes(item) && item.reader?.readyState === FileReader.LOADING) item.reader.abort();
      items = saved.slice(); render();
    },
    load(sources) {
      this.restore(sources.map((source, index) => ({ id: requestId(), file: { name: `Image ${index + 1}`, size: 0 }, source, error: false })));
    },
    lock(value) {
      locked = value; $('attach').disabled = value; picker.disabled = value;
      for (const button of $('attachment-list').querySelectorAll('button')) button.disabled = value;
    },
    discard,
  };
}
