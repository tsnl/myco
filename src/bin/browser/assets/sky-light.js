import renderGradient from './horizon.js';
import { illustrated, rainfall } from './sky-weather.js';

//
// Clock and atmospheric conditions
//

function localHour(now, forecast) {
  if (!forecast) return now.getHours() + now.getMinutes() / 60;
  const local = new Date(now.getTime() + forecast.utc_offset_seconds * 1000);
  return local.getUTCHours() + local.getUTCMinutes() / 60;
}

function weatherLight(conditions) {
  const cover = Math.max(conditions.cloud_cover_low, conditions.cloud_cover_mid) / 100;
  const wet = Math.min(1, rainfall(conditions) / 6);
  return { gloom: cover * cover * (0.55 + wet * 0.45), clarity: 1 - cover * cover * 0.96 };
}

export function sampleSky(now, forecast) {
  const hour = localHour(now, forecast), conditions = forecast?.current || illustrated;
  const altitude = Math.cos((hour - 12) * Math.PI / 12) * Math.PI / 3;
  const night = Math.max(0, Math.min(1, (-altitude * 180 / Math.PI - 3) / 9));
  const twilight = Math.max(0, 1 - Math.abs(altitude) / 0.35);
  const { gloom, clarity } = weatherLight(conditions);
  const light = lightSource(hour, night, twilight, gloom);
  return { conditions, night, gloom, clarity, light, gradient: renderGradient(altitude)[0],
    glassTint: glassTint(twilight, night, gloom),
    phase: night > 0.5 ? 'night' : altitude < 0.15 ? 'twilight' : 'day',
    lighting: { direction: light.direction, diffusion: gloom, palette: cloudPalette(twilight, night, gloom) } };
}

//
// Directional glow, glass, and cloud colors
//

function lightSource(hour, night, twilight, gloom) {
  // This arc follows the selected city's clock, not seasonal solar astronomy.
  // At night a broad, cool glow supplies ambient light without depicting a moon.
  const progress = Math.max(0, Math.min(1, (hour - 6) / 12));
  const x = night > 0.5 ? 68 : 10 + progress * 80;
  const y = night > 0.5 ? 20 : 78 - Math.sin(progress * Math.PI) * 70;
  return { x, y, direction: [Math.round((x - 50) / 40 * 10) / 10, -0.6],
    color: night > 0.5 ? '129 164 227' : twilight > 0.3 ? '255 187 140' : '222 240 255',
    strength: (1 - gloom * 0.85) * (night > 0.5 ? 0.1 : 0.22 + twilight * 0.12) };
}

const DAY = ['#587491', '#718baa', '#9db2c5', '#cad7e1', '#e9eff1', '#fffcf3'];
const DUSK = ['#655d91', '#8b709e', '#b98ba8', '#e1aaa9', '#f4c6ad', '#ffdfb9'];
const NIGHT = ['#172238', '#263850', '#3a4f6a', '#5b7490', '#849bb0', '#b4c6d5'];
const OVERCAST = ['#34475b', '#50657a', '#728597', '#98a7b4', '#b5c3cc', '#d9e1e6'];
const channels = hex => [1, 3, 5].map(offset => parseInt(hex.slice(offset, offset + 2), 16));
const blend = (a, b, weight) => a.map((value, index) => value * (1 - weight) + b[index] * weight);

function glassTint(twilight, night, gloom) {
  // Grade the glass with the same light as the sky, retaining dark enough
  // colors for readable text without increasing the surface opacity.
  const warm = blend([16, 30, 50], [48, 29, 27], twilight);
  const overcast = blend(warm, [24, 28, 34], gloom * 0.65);
  return blend(overcast, [12, 20, 35], night).map(Math.round).join(' ');
}

function cloudPalette(twilight, night, gloom) {
  return DAY.map((color, index) => {
    const warm = blend(channels(color), channels(DUSK[index]), twilight);
    const shaded = blend(warm, channels(OVERCAST[index]), gloom * (1 - night));
    return blend(shaded, channels(NIGHT[index]), night).map(Math.round);
  });
}
