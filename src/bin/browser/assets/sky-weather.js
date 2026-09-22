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

export const illustrated = {
  cloud_cover_high: 32, cloud_cover_mid: 38, cloud_cover_low: 42,
  wind_speed_10m: 3, wind_direction_10m: 260,
};
