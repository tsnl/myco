import { api } from './common.js';

// One dialog per page; failed saves keep the user's proposed name available.
export function sessionRenamer(onSaved = () => {}) {
  const dialog = document.createElement('dialog');
  dialog.id = 'rename-session'; dialog.setAttribute('aria-labelledby', 'rename-heading');
  dialog.innerHTML = `
    <form id="rename-form">
      <h2 id="rename-heading">Rename session</h2>
      <label for="rename-name">Session name</label>
      <input id="rename-name" name="title" type="text" required maxlength="120" autocomplete="off" aria-describedby="rename-error">
      <p id="rename-error" role="alert" hidden></p>
      <div class="rename-actions"><button id="rename-cancel" type="button">Cancel</button><button id="rename-save" type="submit">Save</button></div>
    </form>`;
  document.body.append(dialog);
  const input = dialog.querySelector('input'), form = dialog.querySelector('form');
  const save = dialog.querySelector('#rename-save'), cancel = dialog.querySelector('#rename-cancel');
  const error = dialog.querySelector('#rename-error');
  let sessionId, trigger, saving = false;
  function controls() {
    input.disabled = cancel.disabled = saving;
    save.disabled = saving || !input.value.trim();
    save.textContent = saving ? 'Saving…' : 'Save';
    form.setAttribute('aria-busy', String(saving));
  }
  input.oninput = controls;
  cancel.onclick = () => dialog.close();
  dialog.oncancel = event => { if (saving) event.preventDefault(); };
  dialog.onclose = () => { if (trigger?.isConnected) trigger.focus({ preventScroll: true }); };
  form.onsubmit = async event => {
    event.preventDefault();
    if (saving || !input.value.trim()) return;
    saving = true; error.hidden = true; controls();
    try {
      await api(`/api/sessions/${encodeURIComponent(sessionId)}/rename`, { session_id: sessionId, title: input.value });
      await onSaved();
      dialog.close();
    } catch (failure) {
      error.textContent = failure.message; error.hidden = false;
    } finally {
      saving = false; controls();
      if (dialog.open) input.focus();
    }
  };
  return (session, button) => {
    sessionId = session.id; trigger = button;
    input.value = session.title; error.hidden = true; controls();
    dialog.showModal(); input.focus(); input.select();
  };
}
