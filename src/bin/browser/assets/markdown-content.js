function fragmentId(href) {
  try { return decodeURIComponent(href.slice(1)); }
  catch { return href.slice(1); }
}

// HTML comes from the server's Markdown renderer. Scope its generated IDs
// before insertion so footnotes cannot collide with other messages or controls.
export function markdownContent(html, scope) {
  const template = document.createElement('template');
  template.innerHTML = html;
  const ids = new Map();
  let index = 0;
  for (const node of template.content.querySelectorAll('[id]')) {
    const original = node.id;
    node.id = `markdown-${scope}-${index++}`;
    if (!ids.has(original)) ids.set(original, node.id);
  }
  for (const link of template.content.querySelectorAll('a[href]')) {
    const href = link.getAttribute('href');
    if (href.startsWith('#')) {
      const target = ids.get(href.slice(1)) || ids.get(fragmentId(href));
      if (target) link.setAttribute('href', `#${target}`);
    } else {
      link.target = '_blank';
      link.rel = 'noopener noreferrer';
    }
  }
  return template.content;
}
