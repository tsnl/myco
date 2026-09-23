// A run stays active while waiting for the model, streaming, or executing tools.
// Background sessions and disconnected observations must not look like a live run.
export function showActivity(node, status, busy, connected) {
  const label = connected ? status : 'Reconnecting…';
  if (node.textContent !== label) node.textContent = label;
  node.dataset.busy = String(connected && busy);
  node.dataset.state = !connected || ['Stopped', 'Cancelling'].includes(status) ? 'attention'
    : busy ? 'busy' : status === 'Background tasks' ? 'background' : status === 'Saved' ? 'saved' : 'ready';
  node.title = connected ? status : `Connection lost. Last known status: ${status}`;
}
