# Changelog

## 0.1.0 — unreleased

First working engine: P0–P6 and P8 of [PLAN.md](PLAN.md). 593 tests, run against real
ffmpeg, real encodes and real `melt`.

### Document and engine

- `project.json` as the canonical document: sequences, tracks, clips, effects, keyframes,
  transitions, markers, titles, caption cues and styles, with a published JSON Schema.
- Exact rational time throughout (`Time`, `Rat`, `Fps`, `Span`). `30000/1001` is a value, not
  an approximation; non-drop timecode parses and prints as the frame count it labels.
- Content-addressed asset store (blake3), atomic writes through a `Vfs` boundary, and an
  append-only `history.jsonl` where undo and redo are themselves entries.
- Transactional op engine: validate, apply to a clone, re-validate every sequence, journal,
  then write. A failure anywhere — including inside a batch — leaves the project untouched.
- Selector grammar with attribute filters, time windows, positional narrowing and unions;
  misses list the real candidates.

### Media, compositing, audio

- ffmpeg over pipes for decode, encode, probe, concat and mux; no libav linking.
- Premultiplied linear-light f32 frames; probed color range and matrix passed explicitly on
  every decode and tagged on every encode.
- Compositor with fit/transform/crop/rotation, six blend modes, six transitions, keyframed
  parameters, SVG titles and captions, synthetic generators, and nested sequences with cycle
  detection.
- Effect chain: `color.grade`, `color.lut` (Adobe `.cube`), `blur`, `sharpen`, `crop`,
  `mask.shape`, `chroma-key`, and two-pass `stabilize` via `fx.analyze`.
- Sample-exact mixing with constant-power pan, fades, sidechain ducking with lookahead, EBU
  R128 loudness, silence detection and `seq.trim-silence`.

### Agent surface

- `dvs` CLI: 98 ops plus `new`, `render`, `frame`, `sheet`, `digest`, `lint`, `diff`,
  `transcript`, `caption`, `export`, `undo`, `redo`, `history`, `gc`, `doctor`, `mcp`.
  `--json` everywhere, exit codes 0–6, errors that list candidates.
- MCP server with 105 tools: one per op plus `dvs_overview`, `dvs_apply` (transactional
  batch), `dvs_render`, `dvs_lint`, `dvs_frame`, `dvs_transcript`, `dvs_history`.
- Digest, 29 lint rules, contact sheets, annotated frames and SSIM diff.
- Transcript-driven editing and caption generation; whisper behind an off-by-default feature,
  with `transcript.import` as the keyless path.
- Incremental rendering: segment plan, `blake3` cache key over everything that can change a
  segment's frames. Measured on a 7 s two-segment project: 7.3 s cold, 0.2 s warm.
- Interop: `.kdenlive`/MLT (rendered by `melt` at the same frame count as the native
  renderer), FCPXML 1.11, OTIO, CMX3600 EDL, SRT/VTT.
- Optional AI providers (fal.ai, TTS) with a request cache, a per-project budget ceiling and
  provenance recorded on generated assets.

### Decisions changed during implementation

- **degen-paint crates are not consumed as git dependencies.** `dvs-core` copies the op/
  journal/selector *pattern*; rasterization uses `resvg`/`usvg`/`fontdb` directly. Interop
  with degen-paint is at the file level: a title is an SVG document.
- **No `dssim-core`.** It is AGPL-3.0 and this workspace is MIT, so SSIM is implemented
  directly (8×8 window, L = 1.0, C1 = 0.01², C2 = 0.03²).
- **`--target` is the selector argument on every op**, including `clip.*`, which briefly used
  `--clip`. One spelling across 98 ops beats a per-namespace rule.
- **An imported asset wins over a generator keyword.** `--source bars` after importing
  `bars.mp4` places the footage; `gen:bars` always means the generator.
- **`gap` only fires where nothing covers the hole.** An overlay track is empty by design,
  and a lint that cries wolf on every project stops being read.
- **Ducking attack and release are not frame-snapped.** They are sidechain time constants
  measured in samples; the frame grid has no meaning there.
- **P9 (packaging and signing) is not implemented**, and the studio window has no audio
  monitoring — playback is picture-only.

### The studio window (P7)

- `dvs-studio`: a native window on a live project — composited viewport, timeline with
  tracks/clips/transitions/gaps/markers/playhead, the op journal streaming past as edits land,
  lint findings, and a console running the same ops as the CLI. A console edit is journalled as
  `human`, so agent and human share one undo stack.
- Tauri v2 with a plain HTML/CSS/ES-module frontend: no npm, no bundler, no framework.
- Frames are served over a custom `dvsframe://` URI scheme and PNG-encoded in process, rather
  than returned from a command: a 1280×720 frame is ~3.5 MB of base64 per scrub tick through
  the IPC, and as an image the webview caches and decodes it off the main thread.
- One worker thread owns the `Workspace` and the `Compositor`; frames are cached in an LRU
  keyed by `(index, revision)`, and the compositor is rebuilt when the document changes so no
  decoder outlives the edit it was opened against.
- The project *directory* is watched, debounced at 120 ms: atomic writes replace the inode, so
  a watch on `project.json` itself goes deaf after the first edit.
- `dvs-studio --describe` prints the whole window as text.

### Accessibility

Chosen as a requirement, and the reason the view is a webview rather than a Rust-drawn
toolkit (iced 0.14 has no AccessKit integration, GPUI none, egui partial with an opaque
canvas, Slint strong but GPL/commercial):

- The timeline is a `role="grid"`: a `rowheader` per track, a cell per clip whose accessible
  name is composed in Rust ("intro, video clip on V1, 0 to 12.012 seconds, plays talk.mp4"),
  with gaps and markers labelled the same way. No `<canvas>`.
- Keyboard-complete, with a visible focus indicator, focus restored after dialogs, and no
  positive `tabindex`.
- Nothing encoded in colour alone; text contrast ≥ 4.5:1 and controls ≥ 3:1, ratios recorded
  beside the palette; dark and light both audited.
- `prefers-reduced-motion` replaces the change flash with a persistent outline plus "changed"
  in the clip's accessible name.
- Verified, not asserted: axe-core 4.10.2 reports 0 violations across six page states, and
  `scripts/a11y-tree.py` dumps the real window's AT-SPI tree — which is how the live
  agent-edit loop was proven while the screen was locked.

### Fixed while integrating

- `Toolchain::shared()` printed its error prefix twice on a machine without ffmpeg.
- The decoder returned a black frame instead of holding the last one when a seek past the end
  of a source forced a respawn.
- The renderer muxed no audio track when the only sound came from a clip on a *video* track —
  the commonest timeline there is.
- `text_reports` could not report a font substitution, because usvg keeps the declared family
  on the span; substitution is now detected against the source declarations.
- A missing track resolved through the generic name lookup and answered with asset names; it
  now names tracks, and on an empty sequence it names `track.add`.
- `dvs … --json | head` panicked on the closed pipe instead of exiting.
- `sheet --width` was documented as cell width and implemented as sheet width.
- The studio window silently ran on its development fixture instead of the real project: the
  CSP had no `connect-src ipc: http://ipc.localhost`, so every `invoke` was blocked and the
  frontend fell back. Caught by reading the window's accessibility tree and noticing the clip
  names belonged to `ui/fixture.json`. `withGlobalTauri` was missing too.
- `image`'s PNG encoder panics on a short buffer rather than erroring, which would have taken
  the window's IPC thread down; the frame path now checks the length itself.
