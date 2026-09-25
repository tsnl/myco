import { $, element, error, newSession, requestId } from './common.js';
import { imageAttachments } from './attachments.js';
import { setLinkedText } from './links.js';
import { profilePath } from './scope.js';

// The server owns submitted input, including held edits. The composer holds
// unsaved text locally; switching into an edit preserves the previous draft.
export function messageComposer({ sendAction, imageSource, addImages, resizeInput }) {
  let state = {}, connected = false, selectingModel = false;
  let working = false, pending = null, editing = null, savedDraft = null;
  const prompt = $('prompt');
  const attachments = imageAttachments(() => state.attachment_limits, render, imageSource);
  const unavailable = () => !connected || !state.session_id || selectingModel || working;
  const changedElsewhere = () => editing && !(state.queued || []).some(message => message.request_id === editing.request_id && message.revision === editing.revision && message.state === 'editing');
  const retrySave = (text, images) => pending?.action.kind === 'update_queued' && pending.action.update.kind === 'save' && pending.action.message_id === editing?.request_id && pending.action.update.text === text && JSON.stringify(pending.action.update.images) === JSON.stringify(images);

  function render() {
    attachments.lock(working);
    prompt.readOnly = working;
    $('send').disabled = unavailable() || !attachments.ready || state.status === 'Cancelling';
    $('send').textContent = editing ? retrySave(prompt.value.trim(), attachments.sources) ? 'Retry save ↵' : changedElsewhere() ? 'Send as new' : 'Save & send ↵' : state.busy ? 'Queue ↵' : 'Send ↵';
    $('input-hint').textContent = editing ? 'Keeps its queue position · Shift+Enter for a new line' : state.busy ? 'Enter to queue · Sent after the current tools finish' : 'Enter to send · Shift+Enter for a new line';
    $('queue-edit').hidden = !editing;
    $('queue-edit-label').textContent = changedElsewhere() ? 'Message changed elsewhere. Your edits are still here.' : 'Editing queued message · delivery paused here';
    $('queue-edit-cancel').disabled = unavailable();
    const queued = state.queued || [];
    $('cancel').hidden = !state.busy;
    $('cancel').textContent = queued[0]?.state === 'ready' ? 'Cancel & send queued' : 'Cancel run';
    $('queued').hidden = !queued.length;
    $('queued-count').textContent = `${queued.length} queued`;
    renderQueue(queued);
  }

  function renderQueue(messages) {
    const list = $('queued-list');
    const signature = JSON.stringify([messages, unavailable(), attachments.ready, editing?.request_id]);
    if (list.dataset.messages === signature) return;
    const focused = document.activeElement?.dataset.queueControl;
    list.dataset.messages = signature;
    list.replaceChildren(...messages.map(message => {
      const item = element('li'); item.dataset.messageId = message.request_id;
      const content = element('span', 'queued-content');
      setLinkedText(content, message.text); addImages(content, message.images);
      if (message.timer) content.prepend(element('span', 'queue-state', 'Timer · '));
      const controls = element('div', 'queue-controls');
      if (message.state !== 'ready') controls.append(element('span', 'queue-state', message.state === 'sending' ? 'Sending…' : 'Paused for editing'));
      const button = (label, kind, action, disabled = false) => {
        const control = element('button', '', label); control.type = 'button';
        control.dataset.queueControl = `${message.request_id}-${kind}`;
        control.disabled = unavailable() || message.state === 'sending' || disabled;
        control.onclick = action; controls.append(control);
      };
      button('Edit', 'edit', () => edit(message), !!editing || !attachments.ready);
      if (message.state === 'editing' && message.request_id !== editing?.request_id) button('Resume', 'resume', () => mutate(message, 'resume'));
      button('Unqueue', 'remove', () => mutate(message, 'remove'));
      item.append(content, controls);
      return item;
    }));
    if (focused) list.querySelector(`[data-queue-control="${focused}"]`)?.focus({ preventScroll: true });
  }

  const queueAction = (message, update) => ({ kind: 'update_queued', message_id: message.request_id, revision: message.revision, update });

  async function request(action) {
    const signature = JSON.stringify([state.session_id, action]);
    if (pending?.signature !== signature) pending = { signature, action, id: requestId() };
    await sendAction(action, pending.id);
    pending = null;
  }

  // HTTP acceptance can arrive before the live event. Apply the acknowledged
  // revision locally so the editor never mistakes that gap for a conflicting tab.
  function acknowledge(message, update) {
    const next = state.queued?.find(item => item.request_id === message.request_id);
    if (!next || next.revision !== message.revision) return;
    if (update.kind === 'remove') state.queued = state.queued.filter(item => item !== next);
    else Object.assign(next, { revision: next.revision + 1, state: update.kind === 'edit' ? 'editing' : 'ready' }, update.kind === 'save' ? { text: update.text, images: update.images } : {});
  }

  async function mutate(message, kind) {
    if (unavailable()) return;
    working = true; render();
    try {
      await request(queueAction(message, { kind }));
      acknowledge(message, { kind });
    } catch (e) { error(e.message); }
    finally { working = false; render(); }
  }

  async function edit(message) {
    if (unavailable() || editing || !attachments.ready) return;
    // Capture the revision before live events can update the displayed row.
    const target = { ...message };
    working = true; render();
    try {
      await request(queueAction(target, { kind: 'edit' }));
      acknowledge(target, { kind: 'edit' });
      savedDraft = { text: prompt.value, images: attachments.capture() };
      editing = { ...target, revision: target.revision + 1 };
      prompt.value = target.text; attachments.load(target.images); resizeInput();
    } catch (e) { error(e.message); }
    finally { working = false; render(); prompt.focus(); }
  }

  function restoreDraft() {
    editing = null;
    prompt.value = savedDraft.text;
    attachments.restore(savedDraft.images);
    savedDraft = null; resizeInput();
  }

  $('queue-edit-cancel').onclick = async () => {
    if (unavailable() || !editing) return;
    working = true; render();
    try {
      if (!changedElsewhere()) {
        await request(queueAction(editing, { kind: 'resume' }));
        acknowledge(editing, { kind: 'resume' });
      }
      restoreDraft();
    } catch (e) { error(`${e.message} Your draft is still here.`); }
    finally { working = false; render(); prompt.focus(); }
  };

  function submission(text, images) {
    if (retrySave(text, images)) return pending.action;
    if (editing && !changedElsewhere()) return queueAction(editing, { kind: 'save', text, images });
    if (editing || !text.startsWith('/')) return { kind: 'submit', text, images };
    if (images.length) throw new Error('Send or remove your attached images before using a slash command.');
    if (text === '/compact') return { kind: 'compact' };
    if (text === '/new') { newSession(); prompt.value = ''; resizeInput(); return null; }
    if (text.startsWith('/resume ')) { location.assign(profilePath(`/sessions/${encodeURIComponent(text.slice(8).trim())}`)); return null; }
    throw new Error(text === '/verbose' ? 'Expand an individual tool block to see its full input and output.' : 'Use /new, /compact, /resume <id>, or the session controls.');
  }

  $('composer').onsubmit = async event => {
    event.preventDefault();
    if ($('send').disabled) return;
    const text = prompt.value.trim(), imageIds = attachments.ids;
    if (!text && !imageIds.length) return;
    if (changedElsewhere() && !retrySave(text, attachments.sources) && event.submitter !== $('send')) {
      error('This queued message changed elsewhere. Click Send as new to submit your edits as a new message.'); return;
    }
    let action;
    try { action = submission(text, attachments.sources); }
    catch (e) { error(e.message); return; }
    if (!action) return;
    working = true; render();
    try {
      await request(action);
      if (action.kind === 'update_queued') acknowledge(editing, action.update);
      if (editing) restoreDraft();
      else {
        prompt.value = ''; resizeInput(); attachments.discard(imageIds);
      }
    } catch (e) { error(`${e.message} Your draft is still here.`); }
    finally { working = false; render(); }
  };
  prompt.onkeydown = event => {
    if (event.key === 'Enter' && !event.shiftKey && !event.altKey && !event.isComposing) { event.preventDefault(); $('composer').requestSubmit(); }
  };
  prompt.addEventListener('input', render);

  return {
    update(snapshot, online, selecting) { state = snapshot; connected = online; selectingModel = selecting; render(); },
  };
}
