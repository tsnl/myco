// A run stays active while waiting for the model, streaming, or executing tools.
// Process activity can outlive generation; `busy` still controls submission and cancellation.
export function showActivity(node, status, busy, connected) {
  const label = connected ? status : 'Reconnecting…';
  if (node.textContent !== label) node.textContent = label;
  node.dataset.busy = String(connected && busy);
  node.dataset.active = String(connected && (busy || status === 'Running'));
  node.dataset.state = !connected || ['Stopped', 'Cancelling'].includes(status) ? 'attention'
    : busy || status === 'Running' ? 'busy' : status === 'Saved' ? 'saved' : 'ready';
  node.title = connected ? status : `Connection lost. Last known status: ${status}`;
}
