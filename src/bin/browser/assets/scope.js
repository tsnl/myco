// Modules are served from the profile root, including on nested session URLs.
export const basePath = new URL('.', import.meta.url).pathname.replace(/\/$/, '');
export const profileName = decodeURIComponent(basePath.split('/').at(-1) || 'default');
export const profilePath = (path) => `${basePath}${path}`;
// A reloaded tab must not join the pre-upgrade worker still used by older tabs.
export const eventsWorker = () => new SharedWorker('/profile-events.js', { name: 'myco-profile-events-v3' });
