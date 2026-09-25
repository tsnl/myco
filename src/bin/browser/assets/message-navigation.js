import { $ } from './common.js';

// Track the rendered message itself: live updates can insert or reorder blocks.
export function messageNavigation({ onNavigate, onLatest }) {
  const prompt = $('prompt'), transcript = $('transcript');
  const status = $('message-navigation-status');
  let selected = null;

  function clear() {
    selected?.removeAttribute('aria-current');
    selected = null;
    status.textContent = '';
  }

  function select(message, index, total) {
    clear();
    selected = message;
    selected.setAttribute('aria-current', 'true');
    status.textContent = `User message ${index + 1} of ${total}. ${message.querySelector('.body').textContent.slice(0, 160) || 'Image attachment.'}`;
    onNavigate();
    selected.scrollIntoView({ block: 'start', inline: 'nearest' });
  }

  function move(step) {
    const messages = Array.from(transcript.querySelectorAll(':scope > .message.user'));
    if (!messages.length || (!selected && step > 0)) return;
    const index = selected ? messages.indexOf(selected) : messages.length;
    const next = Math.max(0, index + step);
    if (next >= messages.length) onLatest();
    else select(messages[next], next, messages.length);
  }

  function refresh() {
    if (selected && (!selected.isConnected || prompt.value !== '')) clear();
  }

  prompt.addEventListener('keydown', event => {
    refresh();
    if (prompt.value !== '' || prompt.readOnly || event.defaultPrevented || event.isComposing || event.keyCode === 229 || event.shiftKey || event.altKey || event.ctrlKey || event.metaKey) return;
    if (event.key === 'Escape' && selected) { event.preventDefault(); onLatest(); }
    else if (event.key === 'ArrowUp' || event.key === 'ArrowDown') {
      event.preventDefault();
      move(event.key === 'ArrowUp' ? -1 : 1);
    }
  });
  prompt.addEventListener('input', clear);
  prompt.form.addEventListener('submit', clear);

  return { clear, refresh, get active() { return !!selected?.isConnected; } };
}
