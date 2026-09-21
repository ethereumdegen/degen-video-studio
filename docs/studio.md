# The studio window

![the studio, dark](img/studio-dark.png)

`dvs-studio` opens a project in a native window: the composited viewport, the timeline, the
op journal streaming past as edits land, the lint findings, and a console that runs the same
ops the CLI does.

It exists because the primary operator here is a machine, and that leaves a human with
nothing to look at. An agent quietly getting a timeline wrong looks exactly like an agent
getting it right — until somebody renders and watches. This window is the instrument for the
human half of that loop.

```bash
dvs-studio                       # nearest project at or above the working directory
dvs-studio --project ~/promo --scale 0.5
dvs-studio --describe            # the same information as text, no window
```

## What it shows

| Region | Contents |
|---|---|
| Header | project · sequence · size · frame rate · duration · frame count, a status line, and whether it is connected to the engine |
| Viewport | the composited frame at `--scale`, from the same `dvs-comp::Compositor` that `dvs render` uses |
| Transport | play/pause, ±1 frame, ±1 second, start/end, a scrubber, timecode and frame index |
| Timeline | one row per track, clips coloured *and labelled* by kind, transitions badged, uncovered gaps hatched, markers pinned on the ruler, playhead |
| Activity | the journal, newest first, with an actor badge (`AGENT` / `YOU` / `AI`), the op, its arguments, the frame a time snapped to, and the local time |
| Lint | findings grouped by severity; clicking one selects the clip it names |
| Console | `clip.split --target '#intro' --at 42.5` — the CLI's grammar, the CLI's schema coercion, the same registry |

Human and agent share one document. A console edit is journalled as `human` in the same
`history.jsonl`, so either party can undo the other's work, and the window follows an
agent's edits within about 200 ms of them hitting disk.

## Why a webview

The view layer is HTML in a [Tauri](https://tauri.app) window rather than a Rust-drawn
widget set, and that is an **accessibility** decision before it is a rendering one. HTML
semantics map to the platform accessibility APIs — ATK/Orca on Linux, NSAccessibility and
VoiceOver on macOS — so a clip is a labelled control that a screen reader announces rather
than a coloured rectangle in a canvas.

The Rust GUI options were measured, not assumed, before choosing (versions and dependency
counts checked on crates.io in September 2026):

| Toolkit | Accessibility |
|---|---|
| iced 0.14 | none — no AccessKit dependency; [issue #552](https://github.com/iced-rs/iced/issues/552) open since 2020, only a draft PR |
| GPUI 0.2 | none, and 95 direct dependencies |
| egui 0.36 | AccessKit, partial; a custom canvas timeline would still be one opaque rectangle |
| Slint 1.18 | strong (`accessible-role`, `accessible-label`, testable a11y tree), but GPL/royalty-free/commercial licensing inside an MIT repo |
| Tauri + HTML | the platform's own accessibility stack, and no drawn-widget gap to close |

The webview is the OS's (WebKitGTK here, WKWebView on macOS), not a bundled browser, and
the frontend is plain HTML/CSS/ES modules — **no npm, no bundler, no build step**.

## Accessibility

Built in, not bolted on:

- **Semantics, not pixels.** The timeline is a `role="grid"`: a row per track with a
  `rowheader` ("V1, video track, 3 clips"), a cell per clip whose accessible name is a full
  sentence composed in Rust ("intro, video clip on V1, 0 to 12.012 seconds, plays talk.mp4"),
  and gaps and markers labelled the same way. No `<canvas>` anywhere.
- **Keyboard-complete.** Every action has a key: `space` play/pause, `←`/`→` a frame,
  `shift+←/→` a second, `home`/`end`, arrows to move between clips and tracks, `enter` to
  select, `u` undo, `r` redo, `l` lint, `/` console, `?` help, `escape` to close a dialog and
  return focus to whatever opened it.
- **Nothing encoded in colour alone.** Clip kind, lint severity and actor each carry a word
  or glyph as well as a hue.
- **Contrast** at or above 4.5:1 for text and 3:1 for controls, with the measured ratio
  written beside every colour in `app.css`. Dark by default, light as a real alternative.
- **`prefers-reduced-motion`** replaces the "this just changed" flash with a persistent
  outline and the word *changed* in the clip's accessible name.
- **`--describe`** prints the whole window as text — timeline, activity, findings — so the
  studio is usable from a terminal, over ssh, and by a screen reader with no window at all.
  The CLI and MCP surfaces remain the authoritative way to *drive* the editor.

### Verifying it

Two checks, both repeatable:

```bash
# 1. the frontend, in a browser, against the fixture — axe-core, keyboard, live regions
python3 -m http.server -d crates/dvs-studio/ui 8000

# 2. the real window's accessibility tree, which is what a screen reader reads
WEBKIT_DISABLE_DMABUF_RENDERER=1 dvs-studio --project ~/promo &
python3 scripts/a11y-tree.py --grep "table cell"
```

The second is the stronger of the two: it proves the window is *readable*, not merely drawn,
and it works while the screen is locked or over ssh. Sample output while an agent was editing
in another terminal:

```text
table: Timeline, 2 tracks, 3 clips
row header: V2, video track, 1 clip
table cell: Andy Mazzola, title clip on V2, 1.001 to 4.004 seconds
row header: V1, video track, 2 clips
table cell: main, video clip on V1, 0 to 2.502 seconds, changed
table cell: main, video clip on V1, 2.502 to 5.005 seconds, changed
list item: AGENT clip.split 13:53:03 --target #main --at 2.5 (frame 75)
```

That is one `dvs op clip.split --target '#main' --at 2.5` in a terminal, appearing in the
window — the split, both halves marked *changed*, and the journal entry naming the frame the
request snapped to.

axe-core 4.10.2 reports **0 violations** across six page states (dark, light, after an agent
edit, both dialogs open, and a 200-clip timeline). The remaining `incomplete` results are
symbol-only decorations and gradient backdrops that axe declines to judge; each one's
computed ratio is recorded in `crates/dvs-studio/ui/README.md`.

## Known environment note

On wlroots compositors (Hyprland, Sway) WebKitGTK's DMABUF renderer can fail with
`Error 71 (Protocol error) dispatching to Wayland display`. Run with:

```bash
WEBKIT_DISABLE_DMABUF_RENDERER=1 dvs-studio
```

This is a WebKitGTK/compositor interaction, not something this application can fix from the
inside; it is the standard workaround and costs nothing but a slower compositing path.

## How it is wired

```text
 webview (ui/)                      Rust (crates/dvs-studio/src/)
 ────────────                       ─────────────────────────────
 bridge.js  ── invoke ──────────▶   app.rs        commands: snapshot, run_command,
            ◀── document-changed ─                apply_op, undo, redo, lint, describe_text
 <img src="dvsframe://…/1274">  ─▶  app.rs        custom scheme → PNG, in-process encode
                                    engine.rs     one worker thread: Workspace + Compositor,
                                                  frame LRU keyed by (index, revision)
                                    watch.rs      the project *directory*, debounced 120 ms
```

Two details worth keeping:

- Frames travel as a URL rather than a command reply. A 1280×720 frame is ~3.5 MB of base64
  per scrub tick through the IPC; as an image the webview fetches, caches and decodes it off
  the main thread. The revision is in the query string, so an edit busts the cache.
- The watcher watches the *directory*. `FsVfs` writes a `.tmp` sibling and renames it over
  the target, so every save replaces the inode and a watch registered on the file itself goes
  deaf after the first edit — the window would then update exactly once and look correct
  while being permanently stale.

## Install

`cargo install --path crates/dvs-studio` puts `dvs-studio` in `~/.cargo/bin`; the runtime
needs `ffmpeg` plus a system webview (`webkit2gtk-4.1` on Linux, nothing extra on macOS).
A freedesktop entry and icons are in [`packaging/`](../packaging/README.md), along with the
exact install commands and a note on what `tauri build` would and would not produce —
bundling is `active: false` today, so `cargo install` is the supported path.
