# myco v3 — visual language

The settled design direction for the M3 client ("rev 3 — amethyst",
approved 2026-08-07). This is the contract the client build starts from; the
living mockup it was approved against is the "myco — visual language"
artifact in the design session.

## The story

**A shared studio for two kinds of minds.** myco is a place where work
lives, not an app you visit: instances persist, outlive their viewers, and
are worked by humans and agents as peers. The room is light, airy, and
quietly violet; the work provides the color. The console was the right soul
in the wrong place — it survives as the *terminal's material*, graphite
behind glass, while the chrome around it recedes. The one thing no borrowed
design system can say for us — who is present, and who holds the seat — is
our signature vocabulary, and it appears on every surface.

## Tokens

Named after the world: paper, islands, and hues from mycology — amethyst is
*Laccaria amethystina*, the amethyst deceiver, mycology's own violet.

| token | light | dark | role |
|---|---|---|---|
| `ground` | `#f2f1f6` | `#17151f` | paper — violet-biased, never brown |
| `surface` | `#ffffff` | `#201d2b` | islands: panes, cards, sidebar |
| `sunken` | `#eae8f0` | `#121017` | hover, wells |
| `ink` | `#1e1c26` | `#e7e5ee` | text |
| `dim` | `#6b6779` | `#9d97ac` | secondary text |
| `faint` | `#9b96a8` | `#676177` | tertiary, disabled, empty rings |
| `line` | `#e3e1ea` | `#322d40` | borders |
| `line-soft` | `#edecf3` | `#292534` | island borders, inner rules |
| `accent` | `#5a4e78` | `#a79ac8` | **amethyst** — brand + interactive, worn quietly |
| `accent-soft` | `#5a4e7814` | `#a79ac822` | selection wash |
| `human` | `#c05f38` | `#dd9270` | **clay** — human presence |
| `agent` | `#35836b` | `#7fc4ab` | **verdigris** — agent presence (never violet) |
| `system` | `#7d8894` | `#8b939d` | **slate** — system presence |
| `attn` | `#d84a28` | `#f07a55` | **ember** — attention ONLY (see rules) |
| `ok` | `#3f9142` | `#7fc98a` | success, liveness pulse |

The terminal material is **theme-constant** (the tty pane keeps its own
world in both themes): bg `#161420` (violet-cast graphite), ink `#d8d5e0`,
dim `#7a7488`, ok `#7fc98a`, gold `#e6b357` (ANSI gold lives *only* inside
terminal content — spore amber's retirement home).

Soft chips derive from their hue at ~11–13% alpha (`--*-soft`). One shadow
level: `0 1px 2px ink@3%, 0 10px 32px ink@7%`. Radius: 12px islands, 8px
rows, 99px chips/pills.

## Layout register

Fleet-style **islands on quiet ground**: sidebar and every pane float as
rounded `surface` cards with the single shadow level and 14px gaps on an
8px grid; the ground shows between them. Paddings are generous — pane
headers ~12×16, content wells 20+, sidebar rows ~8×12 — and density comes
from information, never from squeezing chrome. Split-tree panes (not a free
grid); layout belongs to (project, user), client-side.

## Typography

One quiet sans for the room (system stack until the M3 face decision), one
honest mono for everything verbatim — ids, verbs, keys, machine output.
Weight and case make hierarchy; size rarely does. Uppercase labels get
letter-spacing; body stays ≤ ~62ch. Digits in columns use
`tabular-nums`.

## The primitives (our vocabulary, on every surface)

- **Presence dots** — clay = person, verdigris = agent, slate = system;
  identical in tree rows, chips, and transcript bylines.
- **The seat** — every driveable pane wears a chip: `ada driving` (clay
  wash), `agent driving` (verdigris wash), `seat open — take` (dashed
  outline, no fill). An open seat is an *open ring* dot in the tree.
  Transfer animates the chip to its new holder — one of the few things
  that moves.
- **Ember** — a count pill or a left-edge bar. Means *wants you*, nothing
  else, ever.
- **Liveness** — a small `ok`-green dot breathing on watermark advance
  (2.4s ease, disabled under `prefers-reduced-motion`). A dropped stream
  dims the pane and says `reconnecting — showing last state`.

## Rules of the room

1. **Chrome recedes, content leads.** The frame is neutral so unlike kinds
   read as one product; each kind brings its own material (the console
   lives on inside the tty pane, theme-constant).
2. **Air is structure.** Islands, the 8px padding scale, one shadow level.
   Density comes from information, never squeezed chrome.
3. **State is not history.** Panes show *what is* — continuous, breathing,
   no shouting timestamps. Transcripts, logs, toasts show *what happened*
   — timelined, attributed, append-only. Staleness is never dressed as
   freshness.
4. **Motion is causality.** 150–200ms, only to explain a state change
   (pane from tree node, seat chip sliding). Nothing moves to be pretty.
5. **The palette has a reserved word.** Ember = wants you. Spending it on
   decoration debases the one signal that must never be ignorable.
   Corollary: agents are never violet — the brand hue may not impersonate
   a principal.
6. **The voice stays lowercase.** Precise, quiet, a little dry: "seat
   open — take", "reconnecting — showing last state". The UI writes like
   the codebase's comments: constraints, not cheerleading.

## Theme behavior

Light is the default face; dark is violet charcoal, designed — not
inverted. Both themes derive every color from the token table; the client
follows the system with an explicit override. The terminal material does
not participate in theming.
