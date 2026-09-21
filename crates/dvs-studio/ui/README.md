# `dvs-studio` — the window

Plain HTML, CSS and ES modules. No framework, no bundler, no npm at runtime: the webview
loads these files as they are, and `tauri.conf.json` points `frontendDist` here.

| File | What it is |
|---|---|
| `index.html` | the document: landmarks, headings, controls. Semantics first; `app.css` only paints them |
| `app.css` | one palette, both colour schemes, measured contrast ratios in the comment beside it |
| `app.js` | state, transport, activity, lint, console, dialogs, the keyboard model |
| `timeline.js` | time↔pixel geometry (ported, with its test suite) and the grid that draws the timeline |
| `bridge.js` | the only file that knows whether Tauri is there |
| `fixture.json` | a real three-track project, used when it is not |

## Two environments

`bridge.js` exports the same four things either way:

```js
import { invoke, listen, frameUrl, connection } from "./bridge.js";
```

**Inside Tauri** they are `window.__TAURI__.core.invoke`, `window.__TAURI__.event.listen`,
and frames from the `dvsframe` custom scheme
(`dvsframe://localhost/<frame>?rev=&scale=` on macOS, `http://dvsframe.localhost/…` on
Linux and Windows — both are already in the CSP).

**In a plain browser** they are backed by `fixture.json`: a command interpreter that really
mutates that snapshot (`clip.split` splits, reports the frame it snapped to, and creates a
clip you can then select), an undo/redo stack, a lint pass recomputed from the timeline, a
`describe` that composes the same text `dvs_studio::state::describe` does, and frames drawn
as SVG data URLs.

This is not a demo mode. It is how the window gets audited without building Rust, so it
drives the same code paths — including firing a real `document-changed` event 3.5 seconds
after load (an agent trimming `#outro`), which is what makes the live region, the flash and
the activity feed observable.

The header says which one you are in: a teal dot and "live" for Tauri, an amber dot and
"fixture" otherwise.

## How to audit it

```bash
cd crates/dvs-studio/ui
python3 -m http.server 8731
```

Then open <http://127.0.0.1:8731/>. Useful query parameters:

| Parameter | Effect |
|---|---|
| `?test=1` | runs the timeline geometry suite and prints the result into the page |
| `?clips=200` | stretches the fixture to N clips, for measuring the scroll and arrow-navigation floor |

Two globals exist for scripted audits: `window.dvsTimelineTests()` runs the geometry suite
and returns `{ total, failed, failures }`, and `window.dvsStudio` exposes the live state,
the `TimelineView` and `setFrame`.

With axe-core:

```js
await page.addScriptTag({ url: "https://cdn.jsdelivr.net/npm/axe-core@4.10.2/axe.min.js" });
await page.evaluate(() => axe.run(document));
```

Both colour schemes need checking; emulate them rather than trusting one
(`page.emulateMediaFeatures([{ name: "prefers-color-scheme", value: "light" }])`), and do
the same for `prefers-reduced-motion`.

## The decisions worth knowing

**The timeline is a `role="grid"`,** not a canvas and not a list per track. A canvas has no
children for a screen reader to find; a list gives up/down no defined meaning, and in browse
mode the arrow keys never reach the handler. A grid gets a `rowheader` per track (so "V1,
video track, 3 clips" is the row's context rather than a floating label), focusable cells
that put Orca and NVDA into focus mode, and a roving tabindex so the whole timeline is one
tab stop. Gaps are cells too. The full argument is at the top of `timeline.js`.

**Accessible names come from Rust.** `ClipBox::announce`, `TrackRow::announce` and
`GapBox::announce` are composed once, in the engine, and used verbatim as `aria-label`.
JavaScript never reassembles a sentence out of fragments; if a name reads badly, it is fixed
in `state.rs`.

**The live region is one sentence.** `#activity-live` is `aria-live="polite"` and holds the
newest entry's `announce`; the list itself is `aria-live="off"`. A feed that re-reads twenty
rows on every op is worse than silence.

**The scrubber is a native `<input type="range">`** over frame indices with `aria-valuetext`
set to the timecode, so it announces `00:00:42:14` rather than `1274`. The visible timecode
next to it is deliberately not live: one voice per value.

**Alt text is regenerated on every seek** from the snapshot, never from the picture:
"Frame 1274 of 3600, 00:00:42:14, showing talk on V1 and music on A1."

**Nothing is encoded by colour alone.** Clip kind carries a glyph and the kind word, lint
severity carries a glyph and the severity word, actor badges carry the actor's name,
disabled clips are hatched and say "off", gaps are hatched, dashed and say "gap", and a
touched clip has ", changed" appended to its accessible name.

**Zoom is one style write.** Every cell carries `--t0` and `--dur` in seconds; the scroll
container carries `--pps`. Changing the zoom writes one custom property and the browser
re-resolves 200 transforms — this code never touches a clip element to zoom. Measured with
`?clips=200`: ~4 ms for a full style+layout flush, 0.5 ms per arrow key, scrolling
unmeasurable.

**The flash is timed in JS and painted in CSS.** `flashDecay()` in `timeline.js` is
`1 - t²` over 1.2 s; a `requestAnimationFrame` loop writes `--flash-decay` on the grid root
(one write for every touched clip) and `.tl-clip.is-touched::after` uses it as an opacity.
Under `prefers-reduced-motion: reduce` the loop never starts and the stylesheet pins
`--flash-decay: 1`, which turns the fade into an outline that persists until the next edit.

## Keyboard

`space` play/pause · `←`/`→` a frame · `shift` a second · `home`/`end` first/last frame ·
`u` undo · `r` redo · `l` lint · `/` console · `?` help · `escape` closes a dialog and
returns focus to whatever opened it.

Inside the timeline the grid pattern takes over: `←`/`→` move between cells on a track,
`↑`/`↓` between tracks, `home`/`end` reach the ends of *this row* (`ctrl` for the first or
last track), `enter` or `space` selects. The help dialog (`?`) documents both.

## The geometry, and its tests

`timeline.js` is the only home for the time↔pixel mapping, ported from a mutation-tested
Rust prototype. `zoom` is pixels per second; `scroll` is the instant **in seconds** at the
left edge, not a pixel offset; `view` is `{ x, width }`, where `x` is the sticky
track-header lane. `runGeometryTests()` carries the prototype's assertions — round trip
within half a pixel, zoom holding the instant under the cursor, the view parking instead of
walking at the zoom clamps, the 2 px gutter and the ruler band rejecting a click, a
monotonic label ladder that never exceeds eight labels, and a flash curve that is
continuous at both ends (`flashDecay(1.199) < 0.01`, which is the assertion that catches a
curve that never reaches zero and pops).
