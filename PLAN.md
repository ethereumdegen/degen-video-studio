# degen-video-studio — master plan

An agent-native video editor. CLI `dvs` · Rust engine · JSON documents · ffmpeg for codecs ·
Tauri v2 shell · Linux and macOS.

> Sibling of [degen-paint](https://github.com/ethereumdegen/degen-paint): same op-registry spine,
> same journal/undo, same selector grammar, same "digest + lint + diff" feedback channel — applied
> to the time axis. This file holds the thesis, the locked decisions, and the reasoning. Detail
> docs (`docs/`) are produced per phase in the roadmap.

---

## 1. Thesis

[Diffusion Studio](https://diffusion.studio) proved the shape: the edit is a document an agent can
read, diff, re-run and render headlessly, exposed over CLI + MCP. Its engine is TypeScript on
browser WebCodecs; it is macOS/web first.

Kdenlive, Shotcut, Resolve, Premiere all have the *engine* but not the *agent surface*
(verified 2026-09-21: Kdenlive 26.08 exposes no D-Bus/scripting API; its CLI is open+render+exit;
the only drivable layer is the `.kdenlive` MLT XML document — see §9).

degen-video-studio is the missing combination:

| | Diffusion Studio | Kdenlive / Shotcut | degen-video-studio |
|---|---|---|---|
| Document | TSX code | MLT XML, UUID-heavy | **JSON, schema-published, selector-addressable** |
| Engine | TS + WebCodecs | MLT (C) | **Rust compositor, ffmpeg for decode/encode** |
| Agent surface | code + skills | none | **CLI verbs + MCP tools generated from one op registry** |
| Feedback for a blind operator | render | none | **digest, lint, contact sheet, transcript, loudness, diff** |
| Human editor | web app | full GUI | Tauri app on the same document **+ export to `.kdenlive`/FCPXML for the pro tools** |
| Platforms | mac / web | linux / mac / win | **linux + mac** |

The three gaps it closes, in order of importance:

1. **No feedback channel.** An agent that cuts a video cannot see it. It needs to *know*: is there a
   gap on V1, did the title overflow the safe area, is the mix at −14 LUFS, where are the silences,
   what words are said at 01:12. Every render pairs with a machine-readable digest.
2. **No text-native editing.** Most agent video work is talking-head, screencast, and social cuts.
   The lever there is the transcript: "cut every 'um'", "keep the sentence that mentions pricing",
   "caption it". Word-timestamped transcript is a first-class document layer.
3. **No incremental render.** An agent loop makes twenty small edits. Re-encoding ten minutes twenty
   times is not viable. Segment-hashed render cache: change a title, re-encode one segment.

## 2. Locked decisions

**Canonical document is JSON.** `project.json`: assets, sequences, tracks, clips, effects,
keyframes, transcript, captions. Stable ids, no indices. Same reasoning as degen-paint: honest
schemas, real undo via RFC-6902 patches, GUI ↔ agent round-trip. Not code-as-document.

**Engine is Rust; codecs are ffmpeg.** Decode, encode, demux, mux, and audio resampling go
through the `ffmpeg`/`ffprobe` binaries over pipes (`-f rawvideo`, `-f f32le`). Compositing,
transforms, transitions, titles, audio mixing, loudness, and all analysis are Rust. Reasons:

- Linking libav (`ffmpeg-next`) is the single worst cross-platform build experience in the
  ecosystem; the binary is one `brew install ffmpeg` / `pacman -S ffmpeg` away, and
  `ffmpeg-sidecar` can fetch a static build when absent.
- Hardware encoders (VideoToolbox on mac, NVENC/VAAPI on linux) come free via `-c:v`.
- Everything an agent *measures* — pixels, samples, LUFS, SSIM — is computed in-process by Rust
  and is deterministic. ffmpeg only moves bytes in and out.

**One renderer.** The frames the Tauri viewport shows and the frames `dvs render` writes come from
the same Rust compositor. MLT/FCPXML/OTIO are *exports* for interop, never a render path.
(`melt` is used in tests to prove the exported `.kdenlive` matches the native render.)

**Copy degen-paint's spine, not its crates.** `dpaint-core`'s `Op` trait is bound to its own
`Project` type (`op.rs:133`), so `dvs-core` re-implements the same small pattern — `Op`,
`Registry`, `Journal`, `Selector`, `AssetStore`, `Vfs`, JSON-patch undo — against a video
`Project`. The original plan also consumed `dpaint-raster`/`-vector`/`-render`/`-inspect` as git
dependencies; that was dropped during P1. A git dependency on an unpublished sibling workspace
buys nothing here: the parts actually needed are text-and-vector rasterization and a perceptual
diff, which are `resvg`/`usvg`/`fontdb` and, in the end, a hand-written SSIM (dpaint-inspect's
`dssim-core` is AGPL-3.0 and this workspace is MIT) — while the raster compositor this engine needs is
time-aware (keyframes, transitions, decoder-backed layers) rather than a layer stack.

Interop with degen-paint is preserved at the *file* level instead, which is stronger: a title is
an SVG document, degen-paint writes SVG, and `title.add --svg <file>` takes it verbatim.

**The window is a webview, for accessibility.** `dvs-studio` is a Tauri v2 window with a plain
HTML frontend — no npm, no bundler, no framework. The reason is not convenience: HTML semantics
reach the platform accessibility APIs (ATK/Orca on Linux, NSAccessibility/VoiceOver on macOS),
so a clip is a labelled control a screen reader announces. Measured before choosing, in
September 2026: iced 0.14 has no AccessKit integration (issue #552, open since 2020, one draft
PR), GPUI 0.2 has none and 95 direct dependencies, egui's is partial and a custom timeline
canvas would be one opaque rectangle to it, and Slint's is strong but its licensing sits badly
in an MIT repo. The webview is the OS's, not a bundled browser.

There is also a path that needs no window: `dvs-studio --describe` prints the same timeline,
activity and findings as text, and the CLI and MCP surfaces remain the authoritative way to
*drive* the editor — which is, in practice, the most accessible control plane in the project.

**Time is rational, snapped to the frame grid.** Stored positions are `{num, den}` seconds
(`num-rational`). Every op snaps to the sequence's frame rate (`30000/1001` is a first-class
citizen) and reports the snap in its `OpEffect`. Audio is sample-accurate at the sequence sample
rate. Agents speak seconds or timecode (`00:01:12.5`, `1m12s`, `1800f`); the engine never
exposes floats as canonical.

**Determinism at the frame level, tolerance at the byte level.** Same document + same engine
version → byte-identical *composited frames* and *mixed PCM*. Encoded bytes depend on the encoder
build; golden tests decode the output and diff frames with SSIM ≥ 0.99 and PCM with RMS error
bounds. `dvs doctor` records ffmpeg version and enabled encoders into the project for provenance.

**AI is optional and additive.** Whisper transcription (`whisper-rs`, feature-gated, model
downloaded on demand), fal.ai video/image generation, and TTS enter the document as native
structure (transcript words, clips, audio assets) with provenance. The editor is complete with
no models and no keys.

**Linux and macOS.** No Windows in v1. Toolchain paths: `DVS_FFMPEG` env → `PATH` → sidecar
download. HW encode is detected, reported, and opt-in (`--encoder auto|x264|videotoolbox|nvenc|vaapi`);
the default is `libx264 -preset medium -crf 18` because it is everywhere.

## 3. Document model

```
myproject/
├── project.json          # canonical, small, diffable — never contains media
├── history.jsonl         # {op, args, patch, ts, actor} — agent and GUI share it
├── transcript/           # <asset-id>.json word-timestamped transcripts
├── assets/               # content-addressed by blake3: imported media, fonts, generations
├── cache/                # proxies, waveforms, thumbnails, rendered segments (all regenerable)
└── .lock
```

`project.json` (abbreviated; full schema is generated by `dvs schema`):

```jsonc
{
  "degenVideo": 1, "id": "prj_01J…", "activeSequence": "seq_main",
  "assets": {
    "ast_talk": { "hash": "blake3:…", "name": "talk.mp4",
      "probe": { "duration": "5723/30", "video": { "size": [1920,1080], "fps": "30000/1001",
                 "codec": "h264", "pixFmt": "yuv420p", "colorRange": "tv", "colorPrimaries": "bt709" },
                 "audio": { "rate": 48000, "channels": 2, "codec": "aac" }, "vfr": false },
      "proxy": "cache/proxy/ast_talk.mp4" }
  },
  "sequences": {
    "seq_main": { "name": "main", "fps": "30000/1001", "size": [1920,1080], "sampleRate": 48000,
      "tracks": [
        { "id": "trk_v1", "kind": "video", "name": "V1", "clips": [
          { "id": "clp_intro", "name": "intro", "source": { "asset": "ast_talk" },
            "start": "0/1", "sourceIn": "12/1", "sourceOut": "30/1", "speed": "1/1",
            "transform": { "pos": [0,0], "scale": 1, "rot": 0, "anchor": "center" },
            "opacity": 1, "blend": "normal",
            "effects": [ { "id": "fx_1", "kind": "color.lut", "params": { "asset": "ast_lut" } } ],
            "keyframes": { "transform.scale": [ ["0/1", 1.0, "ease-in-out"], ["2/1", 1.1] ] } },
          { "id": "clp_title", "source": { "title": "doc_lowerthird" }, "start": "1/1",
            "duration": "4/1", "transitionIn": { "kind": "dissolve", "duration": "1/2" } }
        ]},
        { "id": "trk_a1", "kind": "audio", "clips": [ … , "gain": -6.0, "pan": 0,
            "fadeIn": "1/10", "fadeOut": "1/2", "ducking": { "against": "trk_a2", "by": -12.0 } ] },
        { "id": "trk_cc", "kind": "caption", "style": "sty_default", "cues": [ … ] }
      ],
      "markers": [ { "id": "mk_1", "at": "42/1", "name": "pricing", "color": "#fb8500" } ]
    }
  },
  "titles": { "doc_lowerthird": { /* embedded degen-paint vector document */ } },
  "styles": { "sty_default": { "font": "Inter", "size": 48, "safeArea": 0.9, "position": "bottom" } }
}
```

Rules:

- **Sources** are one of `asset`, `title` (degen-paint vector doc), `sequence` (nesting),
  `color`, `image`, `generator` (bars, tone, countdown).
- **Clips never overlap on a track.** Overlap is a transition, expressed as `transitionIn` on the
  later clip. Lint reports any state that violates this; ops cannot produce it.
- **Every id is stable and ULID-based**; names are unique per container and addressable via the
  selector grammar (`#intro`, `clip[track=V1]`, `clip[source=ast_talk]`, `track[kind=audio]`,
  time-range terms `@00:10-00:20`, transcript terms `word("pricing")`).
- **Keyframes** are per-param sorted lists with easing; every numeric effect/transform param is
  keyframable. Interpolation is evaluated at frame time by the renderer.
- **Transcript** is an asset-side artifact (`transcript/<asset>.json`, words with `start/end`
  seconds and confidence); clips inherit it through `sourceIn/sourceOut`, so "the words in this
  clip" is a query, not a copy.

## 4. Op catalog (v1, finite)

Namespaced, one JSON Schema each, registered into `dvs-core::Registry`; the CLI, MCP tools,
GUI commands, undo, and docs derive from this list. `--json`, `--dry-run`, `--project`,
`--seq` are global.

| Namespace | Ops |
|---|---|
| `asset` | `import` (probe, hash, copy/link, VFR → CFR proxy), `proxy` (generate/attach), `waveform`, `thumbnails`, `remove`, `relink` |
| `seq` | `new`, `set` (fps/size/rate), `duplicate`, `nest` (range → sequence-as-clip), `trim-silence`, `auto-cut-scenes` |
| `track` | `add`, `remove`, `mute`, `solo`, `lock`, `reorder`, `rename` |
| `clip` | `insert` (ripple), `overwrite`, `append`, `remove` (ripple/lift), `split`, `trim` (in/out/start, ripple or roll), `slip`, `slide`, `move`, `speed` (+ hold, reverse), `link`/`unlink` (a/v), `rename`, `transform`, `opacity`, `blend`, `crop`, `fit` (contain/cover/stretch) |
| `fx` | `add`, `remove`, `set`, `reorder` — v1 kinds: `color.lut`, `color.grade` (lift/gamma/gain/sat/temp), `blur`, `sharpen`, `crop`, `mask.shape`, `chroma-key`, `stabilize` (analysis via ffmpeg `vidstab`, applied natively) |
| `kf` | `set`, `remove`, `clear`, `ease` |
| `transition` | `set` (`cut`, `dissolve`, `dip`, `wipe`, `slide`, `push`), `remove` |
| `audio` | `gain`, `pan`, `fade`, `duck` (sidechain against a track), `normalize` (to LUFS), `mute-range`, `detach` |
| `title` | `add` (from template or degen-paint doc), `set-text`, `edit` (delegates a dpaint op to the embedded doc), `lower-third`, `countdown` |
| `caption` | `import` (SRT/VTT), `generate` (from transcript, line-length + reading-rate aware), `style`, `burn`, `export` |
| `transcript` | `run` (whisper, feature-gated), `import` (word JSON), `find`, `cut-words` (remove filler words / a phrase, ripple), `keep-phrases`, `align` |
| `marker` | `add`, `remove`, `from-transcript`, `from-scenes` |
| `project` | `new`, `gc`, `doctor`, `undo`, `redo`, `history`, `schema`, `lock` |
| `render` | `render`, `frame`, `sheet`, `preview` (proxy-quality), `segments` (cache status) |
| `inspect` | `digest`, `lint`, `diff`, `loudness`, `silence`, `scenes`, `black`, `probe` |
| `export` | `kdenlive`, `mlt`, `fcpxml`, `otio`, `edl`, `srt` |
| `ai` (optional) | `generate.video`, `generate.image`, `tts`, `budget` |

Explicit non-goals for v1: motion tracking, 3D/LUT authoring, multicam sync, color scopes UI,
plugin hosting (VST/OFX/frei0r), Windows, WASM build (WebCodecs decode in the browser is P9+).

## 5. The agent interface

An agent cannot watch the video. Every capability pairs with a channel that answers "what did that
actually do?"

**CLI**

```bash
dvs new promo --fps 30 --size 1920x1080
dvs asset import talk.mp4 music.mp3 logo.svg
dvs transcript run --asset talk.mp4 --model base.en
dvs op transcript.cut-words --target '#talk' --words um,uh,like --min-gap 0.25
dvs op clip.split --at 00:42.5 --track V1
dvs op title.lower-third --text "Andy Mazzola" --sub "degen labs" --at 00:02 --for 4s
dvs op audio.duck --track A2 --against A1 --by -14 --attack 0.2 --release 0.6
dvs op audio.normalize --track A1 --lufs -14
dvs render out.mp4 --digest digest.json
dvs sheet --every 5s --cols 6 sheet.png        # contact sheet with timecodes for a vision model
dvs frame 00:42.5 frame.png --annotate         # one frame, clip bboxes + ids overlaid
dvs lint --json
dvs export kdenlive promo.kdenlive             # hand it to a human in Kdenlive
```

Rules: `--json` everywhere; exit codes `0` ok · `1` op error · `2` bad args · `3` selector matched
nothing · `4` lint failures · `5` tool missing (ffmpeg/whisper) · `6` budget exceeded; errors list
candidates (`selector '#tlk' matched 0 clips; did you mean '#talk'? (V1: #talk, #title-1)`);
`dvs op --list` and `dvs schema --op <id>` for runtime discovery; every op reports the frame-snap
it applied (`"snapped": {"requested": "42.5", "applied": "1274/30"}`).

**MCP** (`dvs mcp`, stdio): one tool per op plus hand-written loop tools:
`dvs_overview`, `dvs_apply` (transactional batch — thirty edits, one round trip, one digest),
`dvs_render` (render + digest + optional contact sheet in one call), `dvs_lint`, `dvs_frame`,
`dvs_transcript` (search + ranges), `dvs_history`.

**Digest** — emitted with every render and on demand:

```jsonc
{
  "sequence": "seq_main", "duration": "185/1", "fps": "30/1", "size": [1920,1080], "renderMs": 42100,
  "tracks": [ { "id": "trk_v1", "clips": [
      { "id": "clp_intro", "range": ["0/1","18/1"], "source": "ast_talk", "sourceRange": ["12/1","30/1"],
        "fitted": "contain", "upscale": 1.0, "words": 41, "firstWords": "so today we're going to" } ] } ],
  "gaps": [ { "track": "trk_v1", "range": ["18/1","18.5/1"] } ],
  "audio": { "integratedLufs": -14.2, "truePeakDb": -1.3, "lra": 6.1,
             "silences": [ ["96/1","99.4/1"] ], "clippedSamples": 0 },
  "video": { "blackRanges": [], "frozenRanges": [], "sceneCuts": ["18/1","61.2/1"] },
  "titles": [ { "id": "clp_title", "text": "Andy Mazzola", "overflow": false, "inSafeArea": true,
                "contrastVsBackdrop": 8.9, "fontFallback": null } ],
  "captions": { "cues": 212, "maxCps": 17.2, "overlaps": 0 },
  "warnings": [ { "code": "gap", "target": "trk_v1", "detail": "0.5s of black on V1 between 18/1 and 37/2" } ]
}
```

Every `target` an agent reads back is a *resolvable* selector: a bare id or a filter that
`resolve` accepts, with the time range in the detail and in the structured block it belongs
to (`gaps[]`, `silences[]`). A target like `trk_v1@18/1` would read well and resolve to
nothing, which is worse than useless in a loop that feeds findings back into ops.

**Lint** — the mistakes a blind editor makes, each finding with a selector:

| Rule | Catches |
|---|---|
| `gap`, `overlap`, `orphan-transition` | holes/collisions on a track, transition longer than either neighbour |
| `past-source-end`, `speed-frame-drop` | clip out-point beyond the asset, retimed clip below 1 source frame per output frame |
| `fps-mismatch`, `vfr-source`, `upscaled`, `letterboxed` | source frame rate/size vs sequence; asset used above native res |
| `loudness-out-of-spec`, `true-peak`, `clipping`, `silence-gap`, `unducked-music` | mix problems against a target profile (YouTube −14, broadcast −23, podcast −16) |
| `black-frames`, `frozen-frames`, `flash` | dead video, stuck decode, single-frame luminance spikes |
| `title-overflow`, `unsafe-area`, `low-contrast`, `font-fallback`, `tiny-type` | inherited from degen-paint's inspect, evaluated against the actual rendered backdrop at that frame |
| `caption-too-fast`, `caption-overlap`, `caption-too-long` | reading rate > 20 cps, overlapping cues, > 2 lines |
| `av-drift` | linked a/v clips whose offsets diverged |
| `unreferenced-asset`, `missing-asset`, `stale-proxy` | store hygiene |

**Contact sheet / frame / annotate** — `dvs sheet` renders a grid of frames with burned-in
timecodes and clip ids; `dvs frame --annotate` overlays clip/title bboxes with stable ids. Vision
models get handles, not guesses.

**Transcript as a control surface** — `dvs transcript find "pricing"` returns ranges;
`transcript.keep-phrases` / `cut-words` edit the timeline with ripple. `caption.generate` builds
cues from words with reading-rate and line-length constraints and reports the resulting `maxCps`.

**Perceptual diff** — `dvs diff a.mp4 b.mp4 --every 1s` samples frames, returns SSIM/ΔE per sample
plus the time ranges that changed; used for "did my edit touch only 00:40–00:45" and for goldens.

**Shared history** — the Tauri app writes through the same registry and `history.jsonl`; either
party undoes the other; the GUI watches the directory; `.lock` serializes writers.

## 6. Architecture

```
  agent ── stdio ─▶ dvs-mcp        (tools from registry)
  agent ── argv ──▶ dvs-cli        (`dvs`, verbs from registry)
  human ── webview ▶ dvs-studio    (Tauri v2 + plain HTML, accessible by construction)
                     │  every surface calls the same ops
                     ▼
        ┌──────────────────────────────────────────────────────┐
        │ dvs-core   Project · Time · Selectors · Op registry   │
        │            Journal/undo · AssetStore · Vfs · Schema   │
        └───┬──────────┬───────────┬───────────┬───────────┬───┘
            ▼          ▼           ▼           ▼           ▼
        dvs-media   dvs-audio   dvs-comp    dvs-text    dvs-interop
        ffprobe/    mix/resample frame        whisper    MLT/.kdenlive
        ffmpeg      loudness     compositor  captions   FCPXML/OTIO/EDL
        pipes       ducking      transforms  SRT/VTT
        proxies                  transitions
        frame cache              titles(dpaint)
            └──────────┴───────────┴───────────┘
                       ▼
             dvs-render   segment scheduler · segment cache · encoder · muxer
             dvs-inspect  digest · lint · scenes · silence · black · sheet · diff
             dvs-ai       fal / TTS / budget (optional)
```

| Crate | Responsibility | Key deps |
|---|---|---|
| `dvs-core` | Project model, `Rational` time + timecode parsing, ids, selectors (incl. time-range and transcript terms), op registry, JSON-patch journal, undo, asset store, `Vfs`, schema export | `serde`, `serde_json`, `schemars`, `json-patch`, `num-rational`, `ulid`, `blake3`, `indexmap` |
| `dvs-media` | Locate ffmpeg/ffprobe (env → PATH → sidecar), probe → typed `Probe`, decoder sessions (seek + sequential rawvideo/f32le pipes), decoder pool + LRU frame cache, proxy + CFR normalization, thumbnails/waveform extraction, encoder session (rawvideo in → container out), HW encoder detection | `ffmpeg-sidecar`, `crossbeam-channel`, `memmap2`, `yuv` |
| `dvs-audio` | Sample-accurate timeline mixing, gain/pan/fades, sidechain ducking, resampling, EBU R128 loudness + true peak, silence detection, normalization | `rubato`, `ebur128`, `dasp`, `hound` |
| `dvs-comp` | Per-frame compositor: clip resolution at time *t*, keyframe evaluation, transforms/crop/fit (SIMD resize), effects, transitions, titles via degen-paint vector render, linear-f32 premultiplied compositing | `dpaint-raster`, `dpaint-vector`, `dpaint-render`, `fast_image_resize`, `rayon`, `kurbo` |
| `dvs-text` | Transcript model, whisper runner (feature `whisper`), caption generation/styling, SRT/VTT import/export, word-level edit helpers | `whisper-rs`, `srtparse` |
| `dvs-interop` | MLT XML / `.kdenlive` writer (+ subset reader), FCPXML 1.11 writer, OTIO JSON writer, CMX3600 EDL | `quick-xml`, `roxmltree` |
| `dvs-render` | Segment scheduler, segment cache (`blake3(segment doc slice ‖ engine ver ‖ ffmpeg ver ‖ encoder params)`), chunk encode + concat, audio bounce, mux, progress events | `dvs-media`, `dvs-comp`, `dvs-audio` |
| `dvs-inspect` | Digest, lint rules, scene/black/frozen detection, contact sheet, annotate, frame diff | `dpaint-inspect`, `dssim-core`, `image` |
| `dvs-ai` | fal.ai video/image, TTS providers, key resolution, cache, budget, provenance | `reqwest`, `tokio`, `keyring`, `secrecy` |
| `dvs-cli` | `dvs` binary | `clap`, `indicatif` |
| `dvs-mcp` | MCP over stdio | `rmcp` |
| `dvs-studio` | Tauri v2 window: viewport, timeline, activity feed, lint, op console; one worker thread owning the `Workspace` and a `Compositor`, frames served over a custom URI scheme | `tauri`, `image`, `notify` |

Dependency direction is strictly downward; `core` knows nothing about media.

### Render pipeline

```
dvs render out.mp4
  1. resolve sequence → flat list of (track, clip, absolute range) + transitions     dvs-core
  2. split timeline into segments at every clip/transition/keyframe-span boundary    dvs-render
  3. for each segment: key = blake3(canonical JSON of everything affecting it)
       cache hit  → reuse cache/segments/<key>.<ext> (closed-GOP chunk, keyframe-aligned)
       cache miss → render:
         a. video: for t in frames: for each video track top→bottom:
              clip at t → decoder pool (seek once, then sequential) → RGBA f32 linear
              → keyframes(t) → transform/crop/fit → effects → transition blend
              → composite (dpaint-raster)                                             dvs-comp
            → ffmpeg encoder session (rawvideo rgb/yuv over stdin)                    dvs-media
         b. audio: mix every audio clip overlapping the segment at sequence rate,
            fades, ducking, gains → f32 PCM                                            dvs-audio
  4. concat segment chunks (`-f concat -c copy`) + bounce audio → mux, loudness pass  dvs-render
  5. digest computed from the same frames/samples that were encoded                   dvs-inspect
```

Frames flow as tiles through `rayon`; decoders are bounded by a pool (default `nproc/2`) and the
frame cache is bounded by bytes, so a 4K project cannot exhaust memory. Color: decode to RGB via
ffmpeg's `scale`/`zscale` with explicit `in_range/in_color_matrix` from the probe (BT.709 vs 601,
tv vs pc range is the #1 silent wrong-colors bug), composite in linear f32, encode tagged BT.709.

### Cross-platform surface

| Concern | Linux | macOS |
|---|---|---|
| ffmpeg | pacman/apt or sidecar static build | `brew install ffmpeg` or sidecar static build |
| HW encode | `h264_nvenc`, `h264_vaapi`, `hevc_*` | `h264_videotoolbox`, `hevc_videotoolbox`, ProRes via `prores_videotoolbox` |
| Viewport | wgpu Vulkan | wgpu Metal |
| Audio monitor | cpal/ALSA/Pipewire | cpal/CoreAudio |
| App shell | Tauri (webkit2gtk) | Tauri (WKWebView) |
| Whisper | CPU, optional CUDA feature | CPU, Metal feature |

`dvs doctor` reports each row as found/missing with the exact fix.

## 7. Risks and mitigations

| Risk | Mitigation |
|---|---|
| ffmpeg version drift changes decode output | `doctor` pins version into `project.json`; goldens are frame-SSIM with tolerance, not byte-exact; CI matrix on ffmpeg 7/8/9 |
| Color range/matrix mistakes (tv/pc, 601/709) | probe drives explicit `scale` args; golden test per matrix/range combo with a synthetic chart; lint `color-untagged` |
| A/V sync drift | audio is sample-positioned from the same `Rational` clock as video; sync test renders a flash+beep pattern and measures offset ≤ 1 frame |
| VFR sources (phone/screen recordings) | detected at import; CFR proxy generated; original kept; lint `vfr-source` |
| Seeking cost on long GOP sources | per-clip decoder session seeks to nearest keyframe once then streams; frame cache; proxies for preview |
| Memory on 4K | bounded decoder pool + byte-capped frame cache + tiled compositing; never hold a whole clip |
| Incremental cache correctness | key includes everything that can change output (doc slice, engine version, ffmpeg version, encoder params); `render --no-cache` for goldens; chunk boundaries are forced keyframes |
| whisper.cpp build pain on mac/linux | feature-gated; `doctor` reports; `transcript.import` accepts external word JSON so any ASR works |
| Scope creep in effects | finite v1 catalog and explicit non-goals; registry makes additions one-op cheap |
| Runaway AI spend | request cache by parameter hash, per-project budget, `--dry-run`, cost in `OpEffect` |
| Undo across 90+ ops | JSON-patch inverse, one mechanism; media in `assets/` is immutable so patches never touch bytes |

## 8. Verified toolchain and dependencies

Build machine (2026-09-21): `rustc 1.98.1`, `cargo 1.98.1`, `ffmpeg n9.0.1` with `libx264`,
`h264_nvenc`, `h264_vaapi`, `libsvtav1`, `libaom-av1`, `aac`, `libopus`; `mlt 7.40.0` (`melt`),
`kdenlive 26.08.0` for interop tests; `node 26.8.1`.

Verified on crates.io while writing this plan:

`ffmpeg-sidecar 2.5.2` · `symphonia 0.6.1` · `rubato 5.0.0` · `ebur128 0.1.10` · `hound 3.5.1` ·
`dasp 0.11.0` · `whisper-rs 0.16.0` · `quick-xml 0.42.0` · `roxmltree 0.21.1` ·
`num-rational 0.4.2` · `yuv 0.8.19` · `image 0.25.10` · `tiny-skia 0.12.0` ·
`fast_image_resize 6.1.0` · `kurbo 0.13.1` · `dssim-core 3.5.1` · `image-compare 0.5.0` ·
`rayon 1.12.0` · `crossbeam-channel 0.5.17` · `memmap2 0.9.11` · `serde_json 1.0.151` ·
`schemars 1.2.2` · `json-patch 4.2.0` · `indexmap 2.14.2` · `ulid 3.0.0` · `blake3 1.8.7` ·
`clap 4.6.7` · `rmcp 3.4.0` · `notify 8.2.0` · `tokio 1.53.1` · `reqwest 0.13.5` ·
`wgpu 30.0.1` · `cpal 0.18.2` · `tauri 2.11.6` · `srtparse 0.3.0`

Rejected: `ffmpeg-next 9.0.0` (libav linking; see §2), `opentimelineio 0.1.0` (placeholder crate —
OTIO JSON is written by hand, the schema is small), `mp4`/`matroska` (ffprobe already answers).

## 9. Interop with the pro editors

`.kdenlive` is MLT XML with `kdenlive:docproperties.*`, `kdenlive:sequenceproperties.*`
(`tracksCount`, `activeTrack`, `documentuuid`, `guides`, `groups`) and per-clip
`kdenlive:control_uuid`/`kdenlive:clipname` (strings verified in the 26.08.0 binary). Verified on
this machine: a hand-written MLT XML with two tracks and a `frei0r.cairoblend` transition renders
with `melt`, and renders identically renamed `.kdenlive`.

`dvs export kdenlive` therefore writes: MLT profile from the sequence, one `<producer>` per asset,
one `<playlist>` per track with `<blank>` padding, `<tractor>` with `mix`/`composite`
transitions, `kdenlive:*` properties for the current document version. Test: `melt` render of the
export vs native render, SSIM ≥ 0.97 on sampled frames (effects that have no MLT equivalent are
listed in the export's `warnings`). Kdenlive normalizes on save, so export is generate-only; a
human's later GUI edits come back via `import mlt` (subset: cuts, positions, gains).

FCPXML gives Final Cut / Resolve on mac; OTIO gives everyone else.

## 10. Build order and acceptance

Each phase ends with a rendered artifact and a passing check, never a claim.

| Phase | Delivers | Acceptance |
|---|---|---|
| **P0 Foundation** | workspace, `dvs-core` (model, rational time, selectors, registry, journal/undo, asset store), `dvs-media` probe + import, `dvs-cli` skeleton, `dvs-interop` `.kdenlive` export | `dvs new` → import two clips → `clip.split`/`clip.move` → `undo` restores byte-identical `project.json`; `dvs export kdenlive` opens in Kdenlive 26.08 and renders with `melt` |
| **P1 Video render** | decoder sessions + pool + cache, compositor (cuts, transform, fit, opacity, dissolve/dip/wipe), encoder session, `dvs render`/`frame` | golden frames for cut/dissolve/scale/601-vs-709 fixtures at SSIM ≥ 0.99; 1080p30 60 s timeline with 3 tracks renders ≤ 1.5× realtime on the 9700X |
| **P2 Audio** | mixing, gain/pan/fades, ducking, resample, loudness, normalize, bounce + mux | flash+beep sync fixture offset ≤ 1 frame; `audio.normalize --lufs -14` measures −14 ± 0.5 by `ebur128` and by `ffmpeg -af ebur128` |
| **P3 Titles & keyframes** | degen-paint vector docs as title sources, lower-third template, keyframes + easing, `fx.color.*`, `blur`, `crop`, `mask.shape` | animated lower-third golden; title digest reports overflow/safe-area/contrast |
| **P4 Inspect** | digest, lint (all rules in §5), scene/black/frozen/silence detection, contact sheet, annotate, frame diff | every lint rule has a fixture that triggers it and one that does not; `dvs sheet` on a 3-min fixture in ≤ 5 s using proxies |
| **P5 Transcript & captions** | `dvs-text`: whisper feature, transcript import, `find`, `cut-words`, `keep-phrases`, caption generate/style/burn, SRT/VTT | `cut-words` on a fixture removes listed fillers with ripple, no gap lint; generated captions have `maxCps ≤ 20` and 0 overlaps |
| **P6 Agent surface** | `dvs-mcp`, `dvs_apply` batching, segment render cache, `doctor`, structured errors with candidates | change one title in a 10-min project → only its segment re-encodes (log proves it); MCP session drives P0–P5 fixtures end to end with no GUI |
| **P7 Studio** | Tauri v2 window: viewport, timeline, activity feed, lint, op console, `--describe` text mode; no bundler and no npm; accessible by construction | agent edit appears in the window within 500 ms; a console edit shows up in `dvs history` as a `human` op; human undo of an agent op works; axe-core reports zero violations and `scripts/a11y-tree.py` shows every clip as a labelled control. **Audio monitoring is not in this phase** — playback is picture-only |
| **P8 Interop & AI** | FCPXML/OTIO/EDL export, MLT import subset, `dvs-ai` (fal video/image, TTS) with budget | FCPXML opens in Final Cut on mac; OTIO round-trips through `otiotool`; `ai.tts` result is a normal audio asset with provenance |
| **P9 Release** | docs from registry, CI (linux + mac, ffmpeg 7/8/9), signed mac build, Arch/Homebrew packaging | `cargo test` green on both OSes; `brew install`/`pacman` recipes verified |

## 11. Immediate next steps

1. `gh repo create ethereumdegen/degen-video-studio --private` and push this plan.
2. P0: scaffold `Cargo.toml` mirroring degen-paint's workspace layout; port `op.rs`, `journal.rs`,
   `selector.rs`, `asset.rs`, `vfs.rs` patterns from `dpaint-core` against the video `Project`.
3. Write the `.kdenlive` exporter first — it is the cheapest end-to-end proof (agent → JSON →
   XML → `melt` → mp4) and the interop guarantee the rest of the plan rests on.
