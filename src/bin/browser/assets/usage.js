// Counts follow the runner's saved usage: latest input/cache, output across a run.
import { $ } from './common.js';

const compact = new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 1 });
const exact = new Intl.NumberFormat();
const percent = new Intl.NumberFormat(undefined, { style: 'percent', maximumFractionDigits: 0 });
const known = (count) => Number.isFinite(count) && count >= 0;
const tokens = (count) => known(count) ? compact.format(count) : '—';

function count(id, label, value, description) {
  const node = $(id);
  node.textContent = `${label} ${tokens(value)}`;
  node.title = known(value) ? `${exact.format(value)} tokens. ${description}` : `${label} tokens have not been reported.`;
}

export function showUsage({ usage, context_window_tokens: capacity }) {
  const used = usage?.input_tokens;
  const context = $('context-usage');
  context.textContent = `Context ${tokens(used)} / ${tokens(capacity)}`;
  if (known(used) && capacity > 0) context.textContent += ` · ${percent.format(used / capacity)}`;
  const limit = known(capacity) ? exact.format(capacity) : 'unknown';
  context.title = known(used)
    ? `${exact.format(used)} / ${limit} tokens. Last measured input, including cached tokens; excludes output and unsent messages.`
    : `Context size is unknown until a model request reports token usage. Capacity: ${limit} tokens.`;
  count('input-tokens', 'Input', used, 'Input to the latest measured model request, including cached tokens.');
  count('output-tokens', 'Output', usage?.output_tokens, 'Generated across the current or most recent run, including tool calls.');
  count('cached-tokens', 'Cached', usage?.cached_input_tokens, 'Cached input to the latest measured request; already included in Input.');
}
