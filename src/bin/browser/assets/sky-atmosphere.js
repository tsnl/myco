import { random } from './sky-noise.js';

//
// Distant stars
//

const namespace = 'http://www.w3.org/2000/svg';

function star(roll) {
  const node = document.createElementNS(namespace, 'circle');
  node.setAttribute('cx', String(roll() * 1600));
  node.setAttribute('cy', String(roll() * 850));
  node.setAttribute('r', String(0.4 + Math.pow(roll(), 3) * 1.4));
  node.style.opacity = String(0.35 + roll() * 0.6);
  return node;
}

function starLayer(index) {
  const layer = document.createElement('div'), stars = document.createElementNS(namespace, 'svg');
  const duration = 6 + index * 1.25;
  layer.className = 'star-layer';
  layer.style.setProperty('--twinkle-duration', `${duration}s`);
  layer.style.setProperty('--twinkle-steps', duration * 4);
  layer.style.setProperty('--twinkle-delay', `${-index * 1.5}s`);
  stars.setAttribute('viewBox', '0 0 1600 1000');
  stars.setAttribute('preserveAspectRatio', 'xMidYMid slice');
  stars.setAttribute('focusable', 'false');
  layer.append(stars);
  return layer;
}

function starfield() {
  const stars = document.createElement('div'), roll = random(2048);
  const layers = Array.from({ length: 4 }, (_, index) => starLayer(index));
  stars.id = 'sky-stars';
  // Animate a few cached planes, rather than repainting individual SVG stars.
  for (let index = 0; index < 110; index++) layers[index % layers.length].firstElementChild.append(star(roll));
  stars.append(...layers);
  return stars;
}

//
// Gradient and diffuse glow
//

function updateGlow(glow, light) {
  glow.style.setProperty('--light-x', `${light.x}%`);
  glow.style.setProperty('--light-y', `${light.y}%`);
  glow.style.setProperty('--light-color', light.color);
  glow.style.opacity = String(light.strength);
}

export function createAtmosphere(sky) {
  const atmosphere = document.createElement('div'), glow = document.createElement('div');
  const stars = starfield();
  atmosphere.className = 'sky-atmosphere'; glow.className = 'sky-glow';
  sky.append(atmosphere, glow, stars);
  return state => {
    document.documentElement.style.setProperty('--glass-tint', state.glassTint);
    document.documentElement.style.setProperty('--sky-light', state.light.color);
    atmosphere.style.backgroundImage = state.gradient;
    atmosphere.style.opacity = String(1 - state.night * 0.92);
    sky.style.setProperty('--overcast', String(state.gloom * 0.55));
    sky.style.setProperty('--sky-clarity', String(state.clarity));
    sky.dataset.phase = state.phase;
    stars.hidden = state.night === 0;
    stars.style.opacity = String(state.night * state.clarity);
    updateGlow(glow, state.light);
  };
}
