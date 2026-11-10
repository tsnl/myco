//
// Toolbar menu
//

// The menu is a popover in the top layer, outside the toolbar's layout. Hang
// it below its toggle, aligned to the toggle's right edge.
function anchorMenu(toggle, menu) {
  const place = () => {
    const rect = toggle.getBoundingClientRect();
    menu.style.top = `${rect.bottom + 8}px`;
    menu.style.right = `${document.documentElement.clientWidth - rect.right}px`;
  };
  menu.addEventListener('beforetoggle', event => { if (event.newState === 'open') place(); });
  window.addEventListener('resize', () => { if (menu.matches(':popover-open')) place(); });
  // An entry's own handler runs first; the menu then closes behind it.
  menu.addEventListener('click', event => {
    if (event.target.closest('button') && menu.matches(':popover-open')) menu.hidePopover();
  });
}

//
// Settings entry and modal
//

function settingsEntry() {
  const button = document.createElement('button');
  button.id = 'settings-toggle'; button.type = 'button'; button.textContent = 'Settings';
  button.setAttribute('aria-haspopup', 'dialog');
  button.setAttribute('aria-controls', 'settings');
  return button;
}

function inside(dialog, event) {
  const rect = dialog.getBoundingClientRect();
  return event.clientX >= rect.left && event.clientX <= rect.right
    && event.clientY >= rect.top && event.clientY <= rect.bottom;
}

// Opening a modal closes the menu, so focus returns to the menu's toggle.
function bindModal(button, dialog, toggle) {
  button.onclick = () => dialog.showModal();
  dialog.querySelector('#settings-close').onclick = () => dialog.close();
  dialog.addEventListener('close', () => toggle.focus());
  dialog.addEventListener('keydown', event => {
    if (event.key === 'Escape') { event.preventDefault(); dialog.close(); }
  });
  dialog.addEventListener('click', event => {
    if (event.target === dialog && !inside(dialog, event)) dialog.close();
  });
}

// Sections own their controls and persistence; this shell owns modal behavior.
export function createSettings() {
  const toggle = document.getElementById('menu-toggle'), menu = document.getElementById('menu');
  const button = settingsEntry(), dialog = document.createElement('dialog');
  dialog.id = 'settings'; dialog.setAttribute('aria-labelledby', 'settings-title');
  dialog.innerHTML = `
    <header class="settings-heading"><h2 id="settings-title">Settings</h2>
      <button id="settings-close" type="button" aria-label="Close settings" autofocus>×</button></header>
    <div id="settings-content"></div>`;
  anchorMenu(toggle, menu);
  menu.append(button);
  document.body.append(dialog);
  bindModal(button, dialog, toggle);
  return dialog.querySelector('#settings-content');
}
