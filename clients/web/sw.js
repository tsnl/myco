// The service worker: platform glue in the same category as index.html —
// browsers insist push handlers live in a real worker file. It shows the
// sealed payload and hands focus back to the workspace; everything it
// displays is re-readable in the notifier, which is the record.
self.addEventListener('push', (event) => {
  let data = {};
  try { data = event.data ? event.data.json() : {}; } catch (_) {}
  event.waitUntil(self.registration.showNotification(data.title || 'myco', {
    body: data.body || '',
  }));
});
self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  event.waitUntil(clients.matchAll({ type: 'window' }).then((open) =>
    open.length ? open[0].focus() : clients.openWindow('/')));
});
