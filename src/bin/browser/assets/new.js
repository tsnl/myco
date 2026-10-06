import { $, api, error, requestId } from './common.js';
import { profileName, profilePath } from './scope.js';

document.title = `New session · ${profileName} · myco`;

// Browser restores may preserve the URL without restoring history.state.
const url = new URL(location.href);
const createId = url.searchParams.get('id') || history.state?.createId || requestId();
url.searchParams.set('id', createId);
history.replaceState({ ...history.state, createId }, '', url);
async function create() {
  $('retry').hidden = true;
  $('creation-status').textContent = 'Creating your session…';
  error();
  try {
    const session = await (await api('/api/sessions', { request_id: createId })).json();
    location.replace(profilePath(`/sessions/${encodeURIComponent(session.id)}`));
  } catch (e) {
    $('creation-status').textContent = 'Could not create the session.';
    error(e.message);
    $('retry').hidden = false;
  }
}
$('retry').onclick = create;
create();
