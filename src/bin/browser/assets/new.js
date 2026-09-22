import { $, api, error, requestId } from '/common.js';

// Reloads and retries must keep the same durable identity after a lost response.
const createId = history.state?.createId || requestId();
history.replaceState({ createId }, '');
async function create() {
  $('retry').hidden = true;
  $('creation-status').textContent = 'Creating your session…';
  error();
  try {
    const session = await (await api('/api/sessions', { request_id: createId })).json();
    location.replace(`/sessions/${encodeURIComponent(session.id)}`);
  } catch (e) {
    $('creation-status').textContent = 'Could not create the session.';
    error(e.message);
    $('retry').hidden = false;
  }
}
$('retry').onclick = create;
create();
