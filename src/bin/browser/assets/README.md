# Browser sky

`sky.js` composes one decorative scene per document. It feeds weather changes
to cloud coverage, wind, and rain; a minute timer updates lighting. The scene
never participates in session state or conversation requests.

| Module | Responsibility |
| --- | --- |
| `settings.js` | Shared Settings modal, keyboard dismissal, and focus restoration |
| `sky-settings.js` | Sky section controls, location persistence, refresh, and stale-weather fallback |
| `sky-weather.js` | Weather descriptions, precipitation units, and illustrated defaults |
| `sky-light.js` | Pure sampling of clock and weather into colors, light direction, and visibility |
| `sky-atmosphere.js` | Gradient, diffuse glow, and stars |
| `clouds.js` | Altitude layers, fixed cloud geometry, coverage, and continuous wind drift |
| `cloud-textures.js` | Scene-owned worker, stale-result rejection, and lightweight fallback |
| `cloud-renderer.js` | Worker scheduling and cached density fields |
| `cloud-field.js` | Deterministic cloud density and soft directional shading |
| `sky-noise.js` | Seeded noise shared by the rendering modules |
| `rain.js`, `aircraft.js` | Independent precipitation and decorative flight layers |
| `sky.css` | Sky layers, weather settings, and motion preferences |

The renderer keeps 24 density fields (eight per altitude). Each texture becomes
one lossless image shared by three copies to wrap in either wind direction.
Only offscreen scratch canvases encode pixels, so the browser can combine static
cloud images instead of maintaining 72 independent canvas surfaces. Density
determines alpha and is independent of lighting, so relighting preserves the
outline. Higher coverage fades in additional clouds, including broad veils,
without changing existing shapes. The whole cloud bank is composited before
fading, which limits opacity even where layers overlap.

The worker yields between textures, pauses in hidden tabs, and tags replies with
a generation so superseded lighting cannot overwrite the current scene. Only
changed lighting repaints textures; CSS handles drifting, twinkling, and rain.
Slow drift and twinkling advance four times a second so tiny changes do not
continuously recomposite the glass. Stars use four cached planes and stop
animating entirely in daylight. Rain and aircraft keep smooth motion.
Worker startup or import failure retains a lightweight fallback. Reduced motion
keeps clouds and stars still and hides rain and aircraft.

Light uses an approximate daily clock arc. It does not model seasonal astronomy,
geographic bearings, actual moonlight, or individual real clouds. Weather
interpretation is shared below the settings and rendering modules; worker code
has no DOM or networking dependencies.

Add embedded assets to `../assets.rs`; the HTTP composition applies the same
origin checks and response policy to every entry. All imports stay on this
server, and the browser tests use mocked weather rather than external services.
