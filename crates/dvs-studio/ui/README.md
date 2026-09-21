# `dvs-studio` — the window

Plain HTML, CSS and ES modules. No framework, no bundler, no npm at runtime: the webview
loads these files as they are, and `tauri.conf.json` points `frontendDist` here.

| File | What it is |
|---|---|
| `index.html` | the document: landmarks, headings, controls. Semantics first; `app.css` only paints them |
| `app.css` | one palette, both colour schemes, measured contrast ratios in the comment beside it |
| `app.js` | state, transport, activity, lint, console, dialogs, the keyboard model |
| `timeline.js` | time↔pixel geometry (ported, with its test suite) and the grid that draws the timeline |
| `waveform.js` | the peaks, drawn per clip into an `aria-hidden` canvas, with the number in the name |
| `bridge.js` | the only file that knows whether Tauri is there |
| `fixture.json` | a real three-track project, used when it is not |
| `frame-fixture.svg` | the picture `?frames=http` serves, so frame loading can be delayed by a harness |

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
| `?device=none` | `monitor_play` fails naming audio, as the engine does with no output device |
| `?device=stalled` | the stream opens and never reports a position: a device that failed to start |
| `?dropouts=3` | the device reports N audio dropouts, to watch them reach the readout |
| `?frames=http` | frames are fetched over HTTP instead of inlined as data URLs |

`?frames=http` exists because a `data:` URL decodes instantly, so nothing in the fixture can
ever be late and the skipped-frame path cannot be exercised against one. Over HTTP every
frame is a real request a harness can hold, throttle or fail. Measured with Puppeteer
request interception, one frame served every 62 ms: the playhead holds 29.99 fps, the
picture arrives at 16.1 fps, and the readout says
`playing · audio clock · 16.1 / 29.97 fps · 50 skipped · 11 frames behind`.

Three globals exist for scripted audits: `window.dvsTimelineTests()` runs the geometry suite
and returns `{ total, failed, failures }`; `window.dvsStudio` exposes the live state, the
`playback` and `frames` objects, the `TimelineView`, `setFrame`, `userSeek`, `startPlayback`
and `endPlayback`; and in the fixture `window.dvsFixture` exposes the knobs, the simulated
device and every `prefetch` the window has asked for.

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

**Audio is the master clock.** The engine mixes the sequence, streams it to a device and
reports where that device has got to as `monitor-position`, twenty times a second. The
playhead follows that, and between events it interpolates from the last one with wall time,
clamped to 120 ms — enough to keep a 30 fps picture smooth on a 20 Hz clock, not enough to
outlive a device that has stopped reporting. Running the picture off `performance.now()`
while a device plays at its own rate drifts audibly within seconds.

With no device the command fails and a wall clock takes over; if a stream opens and no
position arrives within 600 ms, the same. Which clock is running is never hidden: the
status line announces the handover once, and the readout under the transport says
`playing · wall clock (no audio device) · 29.9 / 29.97 fps · 0 skipped` for as long as it
lasts. Playback is forward only — the engine mixes forward, and a reverse transport would
be a picture with no sound to keep it honest.

**The picture never gates the clock.** Frames are `<img>` objects loaded ahead of the
playhead (`prefetch` for the engine, a decode queue for the browser). The playhead moves
when the clock says so and shows whatever has arrived; the readout carries three numbers,
each meaning one thing: pictures per second, frames the viewer never saw, and how far the
picture is behind the sound. Two constants hold the queue together and they are a pair —
at most six requests in flight, and a frame is worth waiting for up to fifteen frames past
its moment. The queue must be shorter, in time, than that tolerance: longer and every
request is abandoned a moment before it would have landed, which is the pathological case
where the renderer is busy all day and the viewport never updates once.

**Waveforms are decoration with a textual equivalent.** One `aria-hidden` canvas per audio
clip — the only canvas in the window — because a couple of thousand `<div>`s per clip would
make arrowing through the timeline unusable, and because a waveform tells a screen reader
nothing a number cannot tell it better. The number is `, peak −1.3 dBFS`, appended to the
clip's accessible name and to the track's row header. The canvas lives in a 13 px strip
reserved at the bottom of the cell, out from behind the label: the ink is one colour over
eight clip fills, and a wash dark enough to read as a waveform drops the label's 4.5:1 to
about 3:1 on the darker ones. Peaks are per track over the whole sequence, so a zoom is a
redraw (driven by a `ResizeObserver`), never an IPC round trip.

## Keyboard

`space` or `k` play/pause · `shift`+`space` play from the start · `←`/`→` or `,`/`.` a
frame · `shift`+`←`/`→` a second · `home`/`end` first/last frame · `u` undo · `r` redo ·
`l` lint · `/` console · `?` help · `escape` closes a dialog and returns focus to whatever
opened it.

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
