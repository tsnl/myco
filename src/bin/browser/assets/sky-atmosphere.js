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

// Every layer steps on the same half-second grid, so a night sky asks for two
// frames a second however many layers twinkle.
function starLayer(index) {
  const layer = document.createElement('div'), stars = document.createElementNS(namespace, 'svg');
  const duration = [6, 7.5, 8.5, 10][index];
  layer.className = 'star-layer';
  layer.style.setProperty('--twinkle-duration', `${duration}s`);
  layer.style.setProperty('--twinkle-steps', duration * 2);
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

// The sky is drawn twice, and both views read their light from properties on
// the sky. The open view holds every detail. The well shows the sky as
// frosted glass would: its light and soft clouds, without stars, aircraft, or rain.
function view(id) {
  const node = document.createElement('div'), atmosphere = document.createElement('div'), glow = document.createElement('div');
  node.id = id; atmosphere.className = 'sky-atmosphere'; glow.className = 'sky-glow';
  node.append(atmosphere, glow);
  return node;
}

export function createAtmosphere(sky) {
  const stars = starfield(), open = view('sky-open'), well = view('sky-well');
  open.append(stars);
  sky.append(open, well);
  // A window no wider than the well shows nothing of the open sky, so none of it is drawn.
  const sides = new ResizeObserver(() => { open.hidden = well.offsetWidth >= sky.offsetWidth; });
  sides.observe(sky); sides.observe(well);
  return {
    open, well,
    update(state) {
      document.documentElement.style.setProperty('--glass-tint', state.glassTint);
      document.documentElement.style.setProperty('--sky-light', state.light.color);
      const properties = {
        '--sky-gradient': state.gradient, '--sky-day': 1 - state.night * 0.92,
        '--overcast': state.gloom * 0.55, '--sky-clarity': state.clarity,
        '--light-x': `${state.light.x}%`, '--light-y': `${state.light.y}%`,
        '--light-color': state.light.color, '--light-strength': state.light.strength,
      };
      for (const [name, value] of Object.entries(properties)) sky.style.setProperty(name, String(value));
      sky.dataset.phase = state.phase;
      stars.hidden = state.night === 0;
      stars.style.opacity = String(state.night * state.clarity);
    },
  };
}
