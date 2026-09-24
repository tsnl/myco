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
  node.style.setProperty('--star-brightness', String(0.35 + roll() * 0.6));
  node.style.setProperty('--twinkle-duration', `${4 + roll() * 5}s`);
  node.style.setProperty('--twinkle-delay', `${-roll() * 12}s`);
  return node;
}

function starfield() {
  const stars = document.createElementNS(namespace, 'svg'), roll = random(2048);
  stars.id = 'sky-stars';
  stars.setAttribute('viewBox', '0 0 1600 1000');
  stars.setAttribute('preserveAspectRatio', 'xMidYMid slice');
  stars.setAttribute('focusable', 'false');
  stars.append(...Array.from({ length: 110 }, () => star(roll)));
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
    atmosphere.style.backgroundImage = state.gradient;
    atmosphere.style.opacity = String(1 - state.night * 0.92);
    sky.style.setProperty('--overcast', String(state.gloom * 0.55));
    sky.style.setProperty('--sky-clarity', String(state.clarity));
    sky.dataset.phase = state.phase;
    stars.style.opacity = String(state.night * state.clarity);
    updateGlow(glow, state.light);
  };
}
