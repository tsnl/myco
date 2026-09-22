# Myco reviewing Myco

Visuals for [PR #251](https://github.com/tsnl/myco/pull/251), captured from the
actual HTTPS browser UI at `d30830c87ea50092655ed7ed1a4bd6430b6ce259`.

- `myco-review.png`: 3200 × 2000 hero screenshot, captured directly by Chromium.
- `myco-review-rain.gif`: 1600 × 1000, eight-second looping rain animation at 12 fps.

The local scripted provider issues a real `bash` read of Myco's `auth.rs` and
`files.rs`, then displays a review based on the two verified HTTP findings.
The inline SVG is loaded through the authenticated `/files/` route. The app's
styling is unchanged. Weather is simulated at dusk; rain uses the native renderer
with complete animation cycles for a seamless loop. Other sky layers are held
still for the recording.

These assets live on the `feat/pr251-hero` media branch and are embedded in the
PR description using immutable commit URLs.
