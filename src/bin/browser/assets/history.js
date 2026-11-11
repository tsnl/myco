import { $, api, element } from './common.js';

const clock = new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' });

// Threads that compaction replaced never change. Each is fetched once, when
// the reader reaches the top of what is shown, and placed above its successor.
export function earlierHistory(blockNode) {
  const container = $('earlier'), control = $('earlier-load'), blocks = $('blocks');
  const shown = new Map(), pending = new Set();
  let session = null, order = [], failed = false;

  // Content arrives above or below what is being read. Hold the element at
  // mid-screen still ourselves, so browsers with and without scroll anchoring
  // behave alike.
  function keepingPlace(change) {
    const root = document.documentElement;
    const at = document.elementFromPoint(blocks.getBoundingClientRect().left + 1, innerHeight / 2);
    const anchor = at && ![container, control].includes(at) && (container.contains(at) || blocks.contains(at)) ? at : blocks;
    const top = anchor.getBoundingClientRect().top;
    root.style.overflowAnchor = 'none';
    change();
    window.scrollBy(0, anchor.getBoundingClientRect().top - top);
    root.style.overflowAnchor = '';
  }

  function show(id, history) {
    const section = element('section', 'earlier-thread');
    section.append(...history.blocks.map(blockNode));
    if (history.blocks.length) {
      const boundary = element('div', 'thread-boundary', `Compacted · ${clock.format(new Date(history.replaced_at))}`);
      boundary.setAttribute('role', 'separator');
      boundary.title = 'The assistant continued from a summary of the conversation above.';
      section.append(boundary);
    }
    // Threads may arrive out of order; each goes above its nearest shown successor.
    const successor = order.slice(order.indexOf(id) + 1).find(later => shown.has(later));
    container.insertBefore(section, shown.get(successor) || null);
    shown.set(id, section);
  }

  // The next thread above everything shown.
  function older() {
    const oldest = order.findIndex(id => shown.has(id));
    return oldest < 0 ? order.at(-1) : order[oldest - 1];
  }

  function render() {
    const next = older();
    control.hidden = !next;
    control.disabled = pending.has(next);
    control.textContent = pending.has(next) ? 'Loading earlier messages…'
      : failed ? 'Could not load earlier messages. Retry' : 'Show earlier messages';
  }

  async function fetchThread(id) {
    if (pending.has(id) || shown.has(id)) return;
    pending.add(id); render();
    const requested = session;
    try {
      const response = await api(`/api/sessions/${encodeURIComponent(session)}/threads/${encodeURIComponent(id)}`);
      const history = await response.json();
      if (requested === session && order.includes(id) && !shown.has(id)) keepingPlace(() => show(id, history));
      failed = false;
    } catch { failed = true; }
    finally { pending.delete(id); render(); }
  }

  const near = () => !control.hidden && control.getBoundingClientRect().bottom > -600;

  // Short threads may not fill the space above the reader; keep going.
  async function load() {
    const next = older();
    if (!next || pending.has(next)) return;
    await fetchThread(next);
    if (!failed && near()) load();
  }

  control.onclick = load;
  let offset = window.scrollY;
  window.addEventListener('scroll', () => {
    const up = window.scrollY < offset;
    offset = window.scrollY;
    if (up && near()) load();
  }, { passive: true });

  return {
    sync(id, threads) {
      if (id !== session) { session = id; failed = false; }
      order = threads;
      for (const [shownId, section] of shown) if (!order.includes(shownId)) { section.remove(); shown.delete(shownId); }
      // A thread replaced while older ones are shown belongs between them
      // and the conversation.
      const oldest = order.findIndex(shownId => shown.has(shownId));
      if (oldest >= 0) for (const missing of order.slice(oldest)) fetchThread(missing);
      render();
    },
  };
}
