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
| `sky-atmosphere.js` | The open and frosted views of the sky, gradient, diffuse glow, and stars |
| `clouds.js` | Altitude layers, fixed cloud geometry, coverage, wind drift, and painting; owns the texture worker and its fallback |
| `cloud-renderer.js` | Worker scheduling and cached density fields |
| `cloud-field.js` | Deterministic cloud density and soft directional shading |
| `sky-noise.js` | Seeded noise shared by the rendering modules |
| `rain.js`, `aircraft.js` | Independent precipitation and decorative flight layers |
| `sky.css` | Sky layers, weather settings, and motion preferences |

The sky is drawn in two views. The open view holds every detail. The well view
stands in for frosted glass behind the conversation: an opaque copy of the
sky's light with soft clouds, drawn once. A backdrop blur there would be redone
for every frame drawn above it. A window no wider than the well hides the open
view, which stops everything that moves in it.

The renderer keeps 24 density fields (eight per altitude) and sends each
texture to the page as a bitmap. `clouds.js` paints them onto two canvases: a
sharp one in the open view, and a small blurred one in the well. Density
determines alpha and is independent of lighting, so relighting preserves the
outline. Higher coverage fades in additional clouds, including broad veils,
without changing existing shapes. Each altitude is composited before it is
faded, and so is the whole bank, which limits opacity even where clouds overlap.

The worker yields between textures, pauses in hidden tabs, and tags replies with
a generation so superseded lighting cannot overwrite the current scene. Only
changed lighting repaints textures. Worker startup or import failure retains a
lightweight fallback.

Motion asks for no more frames than it can be seen to need, because every
frame recomposites the glass above the sky. Clouds repaint once a second, and
four times a second while cover fades. Stars step twice a second on a grid all
their layers share, and stop in daylight. Aircraft advance a pixel at a time
and rain steps thirty times a second. Nothing that moves carries a filter.
Reduced motion keeps clouds and stars still and hides rain and aircraft.

Light uses an approximate daily clock arc. It does not model seasonal astronomy,
geographic bearings, actual moonlight, or individual real clouds. Weather
interpretation is shared below the settings and rendering modules; worker code
has no DOM or networking dependencies.

Add embedded assets to `../assets.rs`; the HTTP composition applies the same
origin checks and response policy to every entry. All imports stay on this
server, and the browser tests use mocked weather rather than external services.
