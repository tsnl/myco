//
// Settings entry and modal
//

function settingsButton() {
  const button = document.createElement('button');
  button.id = 'settings-toggle'; button.type = 'button'; button.title = 'Settings';
  button.setAttribute('aria-label', 'Settings');
  button.setAttribute('aria-haspopup', 'dialog');
  button.setAttribute('aria-controls', 'settings');
  button.setAttribute('aria-expanded', 'false');
  button.innerHTML = `<svg viewBox="0 0 24 24" aria-hidden="true" focusable="false">
    <path d="M4 5h7m4 0h5M4 12h3m4 0h9M4 19h9m4 0h3"/>
    <path d="M11 2h4v6h-4zM7 9h4v6H7zM13 16h4v6h-4z"/></svg>`;
  return button;
}

function inside(dialog, event) {
  const rect = dialog.getBoundingClientRect();
  return event.clientX >= rect.left && event.clientX <= rect.right
    && event.clientY >= rect.top && event.clientY <= rect.bottom;
}

function bindModal(button, dialog) {
  button.onclick = () => { dialog.showModal(); button.setAttribute('aria-expanded', 'true'); };
  dialog.querySelector('#settings-close').onclick = () => dialog.close();
  dialog.addEventListener('close', () => { button.setAttribute('aria-expanded', 'false'); button.focus(); });
  dialog.addEventListener('keydown', event => {
    if (event.key === 'Escape') { event.preventDefault(); dialog.close(); }
  });
  dialog.addEventListener('click', event => {
    if (event.target === dialog && !inside(dialog, event)) dialog.close();
  });
}

// Sections own their controls and persistence; this shell owns modal behavior.
export function createSettings() {
  const button = settingsButton(), dialog = document.createElement('dialog');
  dialog.id = 'settings'; dialog.setAttribute('aria-labelledby', 'settings-title');
  dialog.innerHTML = `
    <header class="settings-heading"><h2 id="settings-title">Settings</h2>
      <button id="settings-close" type="button" aria-label="Close settings" autofocus>×</button></header>
    <div id="settings-content"></div>`;
  document.querySelector('.toolbar-tools').append(button);
  document.body.append(dialog);
  bindModal(button, dialog);
  return dialog.querySelector('#settings-content');
}
