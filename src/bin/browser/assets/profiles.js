import { profileName } from './scope.js';

const picker = document.getElementById('profile-switcher');
async function refresh() {
  try {
    const response = await fetch('/api/profiles');
    if (!response.ok) throw new Error(await response.text());
    const profiles = await response.json();
    picker.replaceChildren(...profiles.map(({ name }) => new Option(name, name, false, name === profileName)));
    picker.title = `Profile: ${profileName}`;
  } catch (error) { picker.title = `Could not refresh profiles: ${error.message}`; }
}
picker.onfocus = refresh;
picker.onchange = () => location.assign(`/profiles/${encodeURIComponent(picker.value)}/`);
refresh();
