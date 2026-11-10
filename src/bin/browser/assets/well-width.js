'use strict';
// A classic script in <head>: a saved width applies before first paint, so no
// page flashes the default well. The preference belongs to this browser.
(() => {
  const KEY = 'myco.well-width.v1';
  const DEFAULT = 100, PRESETS = [80, 100, 120, 150, 200], MIN = 40, MAX = 1000;
  const root = document.documentElement;

  const valid = columns => Number.isInteger(columns) && columns >= MIN && columns <= MAX;

  function saved() {
    let value;
    try { value = localStorage.getItem(KEY); } catch { return DEFAULT; }
    if (value === 'unbounded') return value;
    return valid(Number(value)) ? Number(value) : DEFAULT;
  }

  // The stylesheet derives the well from a column count; only an unbounded
  // well replaces that width outright.
  function apply(width) {
    root.style.removeProperty('--well-columns'); root.style.removeProperty('--well-width');
    if (width === 'unbounded') root.style.setProperty('--well-width', '100%');
    else if (width !== DEFAULT) root.style.setProperty('--well-columns', width);
  }

  apply(saved());

  //
  // Settings section
  //

  function controls(container) {
    const section = document.createElement('section');
    section.id = 'layout-settings'; section.setAttribute('aria-labelledby', 'layout-heading');
    section.innerHTML = `
      <h3 id="layout-heading">Layout</h3>
      <label for="well-width">Conversation width</label>
      <div class="well-width-row">
        <select id="well-width">
          ${PRESETS.map(columns => `<option value="${columns}">${columns} columns${columns === DEFAULT ? ' (default)' : ''}</option>`).join('')}
          <option value="unbounded">Unbounded</option>
          <option value="custom">Custom…</option>
        </select>
        <input id="well-width-custom" type="number" min="${MIN}" max="${MAX}" step="1" required aria-label="Custom width in columns" hidden>
      </div>
      <p id="well-width-note" role="status">Columns of conversation text. Narrower windows use their full width.</p>`;
    container.prepend(section);
    const select = section.querySelector('select'), custom = section.querySelector('input');
    const note = section.querySelector('#well-width-note'), hint = note.textContent;

    function show(width) {
      const preset = width === 'unbounded' || PRESETS.includes(width);
      select.value = preset ? String(width) : 'custom';
      custom.hidden = preset;
      if (!preset) custom.value = width;
    }

    function choose(width) {
      apply(width);
      try {
        if (width === DEFAULT) localStorage.removeItem(KEY);
        else localStorage.setItem(KEY, String(width));
        note.textContent = hint;
      } catch { note.textContent = 'This browser cannot save your width preference.'; }
    }

    select.onchange = () => {
      custom.hidden = select.value !== 'custom';
      if (custom.hidden) { choose(select.value === 'unbounded' ? select.value : Number(select.value)); return; }
      // A custom width starts from the columns currently in effect.
      const current = saved();
      custom.value = current === 'unbounded' ? DEFAULT : current;
      custom.focus(); custom.select();
    };
    custom.oninput = () => { if (valid(custom.valueAsNumber)) choose(custom.valueAsNumber); };
    window.addEventListener('storage', event => {
      if (event.key === KEY || event.key === null) { apply(saved()); show(saved()); }
    });
    show(saved());
  }

  // Module scripts build the Settings dialog before this event.
  document.addEventListener('DOMContentLoaded', () => {
    const container = document.getElementById('settings-content');
    if (container) controls(container);
  });
})();
