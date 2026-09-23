import { random } from './sky-noise.js';
import { rainfall } from './sky-weather.js';

//
// Rain textures and wind
//

function texture(count, depth) {
  const canvas = document.createElement('canvas');
  canvas.width = 768; canvas.height = 512;
  const context = canvas.getContext('2d');
  const roll = random(481 + depth * 379);
  for (let i = 0; i < count; i++) {
    const x = roll() * canvas.width, y = roll() * canvas.height;
    const length = (8 + roll() * 19) * (0.65 + depth * 0.3);
    context.lineWidth = 0.5 + depth * 0.2;
    context.globalAlpha = 0.25 + roll() * 0.5;
    for (const offset of [0, -canvas.height]) {
      const fade = context.createLinearGradient(x, y + offset, x, y + offset + length);
      fade.addColorStop(0, '#d4e4f000'); fade.addColorStop(1, '#d4e4f0');
      context.strokeStyle = fade; context.beginPath();
      context.moveTo(x, y + offset); context.lineTo(x, y + offset + length); context.stroke();
    }
  }
  return `url(${canvas.toDataURL()})`;
}

export function createRain(sky) {
  const rain = document.createElement('div');
  rain.id = 'sky-rain'; rain.hidden = true;
  const sheets = Array.from({ length: 3 }, () => {
    const layer = document.createElement('div'), sheet = document.createElement('div');
    layer.className = 'rain-layer'; sheet.className = 'rain-sheet';
    layer.append(sheet); rain.append(layer); return sheet;
  });
  sky.append(rain);
  return conditions => {
    const rate = rainfall(conditions), strength = Math.min(1, Math.log1p(rate) / Math.log(13));
    rain.hidden = rate === 0; sky.dataset.rain = rate > 0 ? 'rain' : 'none';
    if (!rate) return;
    const slant = -Math.sin(conditions.wind_direction_10m * Math.PI / 180) * Math.min(conditions.wind_speed_10m, 16) * 1.5;
    rain.style.setProperty('--rain-slant', `${slant}deg`);
    sheets.forEach((sheet, depth) => {
      const count = Math.round(8 + strength * (48 - depth * 10));
      if (Number(sheet.dataset.drops) !== count) {
        sheet.style.backgroundImage = texture(count, depth); sheet.dataset.drops = count;
      }
      sheet.style.opacity = String((0.2 + strength * 0.4) * (1 - depth * 0.12));
      sheet.style.animationDuration = `${(2.4 - depth * 0.55) / (0.85 + strength * 0.35)}s`;
    });
  };
}
