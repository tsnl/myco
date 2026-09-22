const KEY = 'myco.sky.location.v1';
const REFRESH = 15 * 60 * 1000;

function savedLocation() {
  try {
    const value = JSON.parse(localStorage.getItem(KEY));
    return validLocation(value) ? value : null;
  } catch { return null; }
}

function validLocation(value) {
  return value && typeof value.name === 'string' && value.name.length <= 200
    && Number.isFinite(value.latitude) && Math.abs(value.latitude) <= 90
    && Number.isFinite(value.longitude) && Math.abs(value.longitude) <= 180;
}

async function request(path, signal) {
  const response = await fetch(path, { signal });
  if (!response.ok) throw new Error(await response.text());
  return response.json();
}

function controls() {
  const button = document.createElement('button');
  button.id = 'sky-toggle'; button.type = 'button'; button.textContent = 'Sky';
  button.setAttribute('aria-haspopup', 'dialog');
  button.setAttribute('aria-controls', 'sky-settings');
  button.setAttribute('aria-expanded', 'false');
  document.querySelector('.toolbar-tools').append(button);
  const dialog = document.createElement('dialog');
  dialog.id = 'sky-settings'; dialog.setAttribute('aria-labelledby', 'sky-heading');
  dialog.innerHTML = `
    <div class="sky-heading"><h2 id="sky-heading">Your sky</h2><button id="sky-close" aria-label="Close sky settings">×</button></div>
    <p id="sky-status" role="status"></p>
    <dl id="sky-coverage" hidden><div><dt>High clouds</dt><dd id="sky-high"></dd></div><div><dt>Middle clouds</dt><dd id="sky-mid"></dd></div><div><dt>Low clouds</dt><dd id="sky-low"></dd></div></dl>
    <form id="sky-search"><label for="sky-city">Follow the weather in a city</label><div class="sky-search-row"><input id="sky-city" type="search" placeholder="City or postal code" minlength="2" maxlength="80" required autocomplete="off"><button>Search</button></div></form>
    <ul id="sky-results" aria-label="Matching cities"></ul>
    <div class="sky-actions"><button id="sky-locate">Use my location</button><button id="sky-reset">Illustrated sky</button></div>
    <p id="sky-error" role="status"></p>
    <div class="sky-attribution">Weather from <a href="https://open-meteo.com/" target="_blank" rel="noopener noreferrer">Open-Meteo</a> · Locations from <a href="https://www.geonames.org/" target="_blank" rel="noopener noreferrer">GeoNames</a>.<p>City searches and approximate coordinates go to Open-Meteo. Your choice is saved in this browser.</p></div>`;
  document.body.append(dialog);
  button.onclick = () => { dialog.showModal(); button.setAttribute('aria-expanded', 'true'); };
  dialog.addEventListener('close', () => button.setAttribute('aria-expanded', 'false'));
  dialog.querySelector('#sky-close').onclick = () => dialog.close();
  dialog.addEventListener('keydown', event => {
    if (event.key === 'Escape') { event.preventDefault(); dialog.close(); }
  });
  dialog.addEventListener('click', event => { if (event.target === dialog && !inside(dialog, event)) dialog.close(); });
  return id => dialog.querySelector(`#sky-${id}`);
}

function inside(dialog, event) {
  const rect = dialog.getBoundingClientRect();
  return event.clientX >= rect.left && event.clientX <= rect.right && event.clientY >= rect.top && event.clientY <= rect.bottom;
}

export function skySettings(onChange) {
  const $ = controls();
  let location = savedLocation(), forecast = null, controller, searchController;
  let refreshed = 0, selection = 0;

  function display(state) {
    const active = location && forecast;
    const time = active ? new Date(forecast.current.time * 1000).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' }) : '';
    $('status').textContent = active ? `${location.name} · ${state === 'stale' ? 'last available' : 'updated'} ${time}`
      : location ? `${location.name} · illustrated sky until weather is available` : 'An illustrated sky, following your clock.';
    $('coverage').hidden = !active;
    if (active) for (const layer of ['high', 'mid', 'low']) $(layer).textContent = `${Math.round(forecast.current[`cloud_cover_${layer}`])}%`;
    document.querySelector('#sky').dataset.weather = state;
    document.querySelector('#sky-toggle').title = active ? `Sky over ${location.name}` : 'Choose your sky';
    onChange(active ? forecast : null);
  }

  async function refresh() {
    controller?.abort();
    if (!location || document.hidden) return;
    controller = new AbortController();
    const signal = controller.signal;
    refreshed = Date.now();
    try {
      const query = new URLSearchParams({ latitude: location.latitude, longitude: location.longitude });
      const next = await request(`/api/sky/weather?${query}`, signal);
      if (signal.aborted) return;
      forecast = next; $('error').textContent = ''; display('live');
    } catch (error) {
      if (signal.aborted) return;
      if (forecast && Date.now() / 1000 - forecast.current.time > 2 * 60 * 60) forecast = null;
      $('error').textContent = `Weather unavailable. ${forecast ? 'Showing the last available conditions.' : 'Showing an illustrated sky.'}`;
      $('error').title = error.message;
      display(forecast ? 'stale' : 'illustrated');
    }
  }

  function choose(next, save = true) {
    selection++;
    controller?.abort(); searchController?.abort();
    location = next; forecast = null; refreshed = 0;
    $('results').replaceChildren(); $('error').textContent = '';
    if (save) {
      try { next ? localStorage.setItem(KEY, JSON.stringify(next)) : localStorage.removeItem(KEY); }
      catch { $('error').textContent = 'This browser cannot save your sky preference.'; }
    }
    display('illustrated');
    refresh();
  }

  $('search').onsubmit = async event => {
    event.preventDefault();
    searchController?.abort(); searchController = new AbortController();
    const signal = searchController.signal;
    $('results').replaceChildren(); $('error').textContent = 'Finding cities…';
    try {
      const data = await request(`/api/sky/locations?query=${encodeURIComponent($('city').value.trim())}`, signal);
      if (signal.aborted) return;
      $('error').textContent = data.results.length ? '' : 'No matching cities. Try a nearby city or add a country.';
      for (const city of data.results) {
        const name = [...new Set([city.name, city.admin1, city.country].filter(Boolean))].join(', ');
        const item = document.createElement('li'), button = document.createElement('button');
        button.type = 'button'; button.textContent = name;
        button.onclick = () => choose({ name, latitude: Math.round(city.latitude * 100) / 100, longitude: Math.round(city.longitude * 100) / 100 });
        item.append(button); $('results').append(item);
      }
    } catch (error) { if (!signal.aborted) $('error').textContent = `City search unavailable. ${error.message}`; }
  };

  $('locate').onclick = () => {
    if (!navigator.geolocation) { $('error').textContent = 'Device location needs HTTPS or localhost. You can choose a city above.'; return; }
    const version = ++selection;
    $('error').textContent = 'Finding your location…';
    navigator.geolocation.getCurrentPosition(position => {
      if (version !== selection) return;
      choose({ name: 'Your location', latitude: Math.round(position.coords.latitude * 100) / 100, longitude: Math.round(position.coords.longitude * 100) / 100 });
    }, () => {
      if (version === selection) $('error').textContent = 'Location unavailable. You can choose a city above.';
    }, { enableHighAccuracy: false, maximumAge: 600000, timeout: 8000 });
  };
  $('reset').onclick = () => choose(null);
  window.addEventListener('storage', event => { if (event.key === KEY || event.key === null) choose(savedLocation(), false); });
  const resume = () => { if (!document.hidden && Date.now() - refreshed >= REFRESH) refresh(); };
  document.addEventListener('visibilitychange', resume);
  setInterval(resume, REFRESH);
  display('illustrated'); refresh();
}
