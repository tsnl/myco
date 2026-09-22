import { createAtmosphere } from '/sky-atmosphere.js';
import { createClouds } from '/clouds.js';
import { createRain } from '/rain.js';
import { createAircraft } from '/aircraft.js';
import { sampleSky } from '/sky-light.js';
import { skySettings } from '/sky-settings.js';

// One scene per document. Weather changes coverage and wind; the minute clock
// only updates lighting. Hidden tabs defer painting until they become visible.
const sky = document.createElement('div');
sky.id = 'sky';
sky.setAttribute('aria-hidden', 'true');
document.body.prepend(sky);
const updateAtmosphere = createAtmosphere(sky);
createAircraft(sky);
const clouds = createClouds(sky);
const updateRain = createRain(sky);
let forecast = null;

function updateSky(weatherChanged = false) {
  if (document.hidden) return;
  const state = sampleSky(new Date(), forecast);
  updateAtmosphere(state);
  clouds.setLighting(state.lighting);
  if (weatherChanged) {
    clouds.setWeather(state.conditions);
    updateRain(state.conditions);
  }
}

function visibility() {
  sky.classList.toggle('paused', document.hidden);
  updateSky(true);
}

document.addEventListener('visibilitychange', visibility);
visibility();
setInterval(updateSky, 60000);
skySettings(next => { forecast = next; updateSky(true); });
