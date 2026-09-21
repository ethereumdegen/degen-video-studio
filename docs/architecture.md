# Architecture

## 1. Shape of the system

```text
  agent ── stdio ──▶ dvs-mcp     one MCP tool per op + loop tools
  agent ── argv ───▶ dvs-cli     `dvs`, verbs generated from the registry
                        │  both surfaces call the same ops
                        ▼
        ┌──────────────────────────────────────────────────────────┐
        │ dvs-core   Project · Time · Selectors · Op registry      │
        │            Journal / undo · AssetStore · Vfs · Schema    │
        └──┬───────────┬───────────┬───────────┬───────────┬───────┘
           ▼           ▼           ▼           ▼           ▼
       dvs-media   dvs-audio   dvs-comp    dvs-text   dvs-interop
       ffprobe /   mix ·       frames ·    whisper ·  MLT/.kdenlive
       ffmpeg      loudness ·  transforms  captions · FCPXML · OTIO
       pipes ·     ducking ·   transitions SRT/VTT    EDL
       proxies     silence     titles · fx
           └───────────┴───────────┴───────────┴───────────┘
                                   ▼
                     dvs-render    segment plan · segment cache · encode · mux
                     dvs-inspect   digest · lint · scenes · sheet · annotate · diff
                     dvs-ai        fal · TTS · budget · request cache (optional)
```

Dependency direction is strictly downward. `dvs-core` knows nothing about ffmpeg, pixels or
samples; every other crate reaches the document only through ops registered into
`dvs_core::Registry`, which is why `dvs op --list` is the entire surface no matter which
crate implements what.

**The invariant that matters:** there is exactly one renderer. `dvs render`, `dvs frame`,
`dvs sheet`, the digest and any future viewport all go through `dvs_comp::Compositor`. No
second preview path exists that could disagree with the export.

## 2. Crates

| Crate | Responsibility |
|---|---|
| `dvs-core` | Document model, exact rational time, typed ids, selector grammar, op registry, JSON-patch journal with undo/redo, content-addressed asset store, `Vfs`, schema export, and the document ops (`seq.* track.* clip.* fx.* kf.* transition.* marker.* title.*`) |
| `dvs-media` | The only crate that spawns ffmpeg: toolchain discovery, `ffprobe` → typed `Probe`, sequential `VideoDecoder`, `decode_audio`, `EncoderSession`, concat/mux, proxies, thumbnails, waveforms, PNG I/O, and `asset.*` |
| `dvs-audio` | Sample-exact timeline mixing, gain/pan/fades, sidechain ducking, EBU R128 loudness, silence detection, `audio.*` and `seq.trim-silence` |
| `dvs-comp` | Per-frame compositor: clip resolution, keyframe evaluation, fit/transform/crop/rotation, blend modes, transitions, SVG titles and captions, the effect chain, and `fx.analyze` |
| `dvs-text` | Transcript model and whisper runner, caption generation and styling, `transcript.*` and `caption.*` |
| `dvs-interop` | `.kdenlive`/MLT writer and reader subset, FCPXML, OTIO, EDL, and `export.*` / `import.mlt` |
| `dvs-render` | Segment planning, the incremental segment cache, encode/concat/mux, progress reporting |
| `dvs-inspect` | Digest, lint rules, scene/black/frozen detection, contact sheet, annotated frames, perceptual diff, `inspect.*` |
| `dvs-ai` | Optional providers (fal.ai, TTS) with a request cache, budget ceiling and provenance |
| `dvs-cli` | The `dvs` binary; verbs and flags derived from the registry |
| `dvs-mcp` | MCP over stdio; one tool per op plus the batching/feedback tools |

## 3. Data flow of one edit

```text
dvs op clip.split --at 42.5 --track V1
  1. discover the project directory, open the Workspace     dvs-core::engine
  2. resolve the op id in the Registry (unknown → exit 3 with real candidates)
  3. parse args against the op's JSON Schema; reject unknown keys outright
  4. clone the Project (validate-then-mutate)
  5. resolve '--track V1' / selectors to typed ids          dvs-core::selector
  6. snap 42.5 to the sequence grid → frame 1274 at 30000/1001, record the snap
  7. apply(&mut Project, args)                              the owning crate's op
  8. validate every sequence (no overlap, no zero-length clip)
  9. diff snapshot → RFC-6902 patch, append to history.jsonl
 10. write project.json atomically (tmp sibling + rename)
 11. emit Applied as JSON: {op, seq, changed, created, removed, warnings, snapped}
```

A failure at any step — including the 17th op of an `dvs_apply` batch — leaves
`project.json` and `history.jsonl` byte-identical to before the call.

## 4. Render pipeline

```text
dvs render out.mp4
  1. frame-align the range; split it into segments at every discontinuity        dvs-render::plan
       clip starts/ends, transition edges, caption cue edges, keyframe spans
  2. per segment: key = blake3(document slice ‖ asset hashes ‖ engine version
                               ‖ ffmpeg version ‖ encoder/quality/scale/proxy)   dvs-render::cache
       hit  → reuse cache/segments/<key>.mp4 (closed GOP, concat-safe)
       miss → for each frame index:
                bottom track upward: clip at t → decode/rasterize/generate at
                the destination size → effect chain → place (fit/transform/
                crop/rotate) → transition mix → composite with blend+opacity    dvs-comp
              → EncoderSession (rawvideo rgb24 over stdin)                      dvs-media
  3. mix the whole range once: clip fades, gains, pans, ducking, track strip     dvs-audio
  4. concat segments with -c copy, mux the PCM, tag color metadata               dvs-media
  5. digest from the same frames and samples that were encoded                   dvs-inspect
```

Frames are premultiplied **linear-light f32 RGBA** end to end. Two consequences that are the
whole reason for the choice: a dissolve between black and white passes through 50% *light*
rather than 50% encoded value, and a thirty-layer stack does not band.

Decoding is one ffmpeg process per (source, decode size), read sequentially. Reading forward
a few frames is cheaper than a seek — a seek decodes from the previous keyframe anyway — so
the decoder only respawns when the request goes backwards or jumps far ahead. `Compositor::
seek_count()` exposes the respawn count, because a number that grows with frame count means
the access pattern is thrashing.

Color is stated, never defaulted: the decode filter chain carries the probed range and
matrix (`in_range`, `in_color_matrix`) and outputs full-range RGBA; the encoder converts back
to limited-range BT.709 and tags the file to match. A file that is untagged or decoded with
the wrong matrix looks subtly wrong in one player and fine in another, and nothing reports
it — so both ends are explicit.

## 5. Determinism, and where it stops

Same document plus same engine version gives byte-identical *composited frames* and
byte-identical *mixed PCM*: no wall clock, locale or system font resolution reaches the
render path, generic font families resolve to one deterministic face, and every random
parameter is an explicit op argument.

Encoded bytes are **not** reproducible across ffmpeg builds, and pretending otherwise would
make golden tests lie. So goldens decode the output and compare frames with SSIM plus
bounded per-pixel difference, audio with RMS error bounds, and `project.json` records which
ffmpeg produced a render.

## 6. Storage boundary

Everything in `dvs-core` reaches storage through the `Vfs` trait — `FsVfs` natively, `MemVfs`
in tests and for a future browser build. A `std::fs` call anywhere else in that crate would
compile and then fail at run time in a non-native target, which is the worst kind of bug, so
a unit test greps the crate's own sources and fails the build on a reintroduced call. The
single exemption is the streaming asset hasher, marked in place, because hashing a 4 GB
import through an in-memory buffer is not an option.

`assets/` is content-addressed by blake3. `project.json` therefore never contains media,
undo snapshots copy JSON rather than pixels, re-importing a file costs nothing, and `dvs gc`
can prune blobs no document references.

## 7. Cross-platform surface

| Concern | Linux | macOS |
|---|---|---|
| ffmpeg | `pacman`/`apt` | `brew install ffmpeg` |
| Hardware encode | `h264_nvenc`, `h264_vaapi`, `hevc_*` | `h264_videotoolbox`, `hevc_videotoolbox`, ProRes |
| Fonts | fontconfig via `fontdb` system scan | Core Text directories via `fontdb` system scan |
| Whisper | CPU, optional CUDA | CPU, optional Metal |

`Encoder::Auto` resolves to `libx264` on both, because reproducibility across machines beats
speed as a default; hardware encoders are opt-in per render. `dvs doctor` reports each row as
found or missing with the exact fix, and a requested encoder the local ffmpeg lacks is an
error listing what it does have — never a silent fallback that changes quality and file size
without saying so.
