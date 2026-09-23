const urls = /(?:https?:\/\/|www\.)[^\s<>"'`“”‘’]+/giu;
const closing = { ')': '(', ']': '[', '}': '{' };

function trimUrl(text) {
  const counts = {};
  for (const char of text) counts[char] = (counts[char] || 0) + 1;
  let end = text.length;
  while (end) {
    const char = text[end - 1], open = closing[char];
    if (!/[.,;:!?]/.test(char) && !(open && counts[char] > (counts[open] || 0))) break;
    counts[char]--; end--;
  }
  return text.slice(0, end);
}

function linkDestination(text) {
  try {
    const url = new URL(/^www\./i.test(text) ? `https://${text}` : text);
    if (['http:', 'https:'].includes(url.protocol) && url.hostname) return url.href;
  } catch { /* Incomplete streamed URLs remain ordinary text. */ }
  return null;
}

function linkedText(text) {
  const fragment = document.createDocumentFragment();
  let offset = 0;
  for (const match of text.matchAll(urls)) {
    // Avoid turning the tail of an email, path, or identifier into a link.
    if (match.index && /[\p{L}\p{N}_@/]/u.test(text[match.index - 1])) continue;
    const label = trimUrl(match[0]), destination = linkDestination(label);
    if (!destination) continue;
    fragment.append(text.slice(offset, match.index));
    const link = document.createElement('a');
    link.href = destination; link.textContent = label;
    link.target = '_blank'; link.rel = 'noopener noreferrer';
    fragment.append(link);
    offset = match.index + label.length;
  }
  fragment.append(text.slice(offset));
  return fragment;
}

export function setLinkedText(node, text) {
  node.replaceChildren(linkedText(text));
}

export function linkify(node) {
  const walker = document.createTreeWalker(node, NodeFilter.SHOW_TEXT, {
    acceptNode: text => text.parentElement.closest('a, code, pre') ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT,
  });
  const texts = [];
  while (walker.nextNode()) texts.push(walker.currentNode);
  for (const text of texts) text.replaceWith(linkedText(text.data));
}
