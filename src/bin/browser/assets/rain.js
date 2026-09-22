//
// Current weather interpretation
//

export function rainfall(conditions) {
  if (!conditions || !Number.isFinite(conditions.interval) || conditions.interval <= 0) return 0;
  const amount = conditions.rain + conditions.showers;
  if (!Number.isFinite(amount) || amount < 0) return 0;
  // Current accumulations cover the returned interval, not necessarily an hour.
  if (amount > 0) return amount * 3600 / conditions.interval;
  // Very light precipitation can round to zero while the current code is wet.
  return [51, 53, 55, 56, 57, 61, 63, 65, 66, 67, 80, 81, 82].includes(conditions.weather_code) ? 0.1 : 0;
}

const CONDITIONS = {
  0: 'Clear sky', 1: 'Mostly clear', 2: 'Partly cloudy', 3: 'Overcast',
  45: 'Fog', 48: 'Freezing fog', 51: 'Light drizzle', 53: 'Drizzle', 55: 'Dense drizzle',
  56: 'Freezing drizzle', 57: 'Freezing drizzle', 61: 'Light rain', 63: 'Rain', 65: 'Heavy rain',
  66: 'Freezing rain', 67: 'Heavy freezing rain', 71: 'Light snow', 73: 'Snow', 75: 'Heavy snow',
  77: 'Snow grains', 80: 'Light showers', 81: 'Rain showers', 82: 'Heavy showers',
  85: 'Snow showers', 86: 'Heavy snow showers', 95: 'Thunderstorm', 96: 'Thunderstorm with hail', 99: 'Thunderstorm with hail',
};

export function weatherDescription(conditions) {
  return CONDITIONS[conditions?.weather_code] || 'Current cloud cover';
}

//
// Rain textures and wind
//

function texture(count, depth) {
  const canvas = document.createElement('canvas');
  canvas.width = 768; canvas.height = 512;
  const context = canvas.getContext('2d');
  let seed = 481 + depth * 379;
  const roll = () => { seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0; return seed / 4294967296; };
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
