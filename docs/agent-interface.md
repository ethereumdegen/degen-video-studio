# The agent interface

The constraint that shapes everything here: **an agent cannot watch the video.** A human
editor gets continuous feedback at zero cost; an agent gets a return value. So every
capability is paired with a machine-readable channel that answers "what did that actually
do?", and every failure is paired with the information needed to fix it.

## 1. One registry, two front ends

98 ops are registered into one `dvs_core::Registry` — by `dvs-core` (document ops),
`dvs-media` (`asset.*`), `dvs-comp` (`fx.analyze`), `dvs-audio`, `dvs-text`, `dvs-interop`,
`dvs-inspect` and `dvs-ai`. The CLI derives its `op` verb and flags from that registry; the
MCP server derives one tool per op from the same place. Neither has a hand-written list, so
they cannot disagree.

```
asset 6 · seq 8 · track 10 · clip 19 · fx 6 · kf 4 · transition 2 · marker 5 · title 4
audio 7 · transcript 5 · caption 5 · inspect 6 · export 6 · import 1 · ai 4
```

## 2. Argument conventions

Uniform across every op, because an agent should never have to remember per-op spellings:

| Flag | Meaning |
|---|---|
| `--target <selector>` | what the op acts on, in selector grammar. Every op that takes a selector uses this name — `clip.split --target '#intro'`, `fx.add --target 'clip[track=V1]'`, `marker.remove --target '*'` |
| `--track <name\|id>` | a single track, where the op is about the track itself |
| `--seq <id\|name>` (global) | the sequence being edited; defaults to the project's active one |
| `--json`, `--dry-run`, `-q`, `--project` (global) | machine output, validate-without-writing, silence, explicit project directory |

Argument keys are kebab-case and match the CLI flag exactly (`--min-gap` is `"min-gap"` in
an MCP call). Document fields are camelCase. The two namespaces are deliberately separate:
arguments mirror the command line, the document mirrors JSON conventions.

Every time argument accepts every spelling an agent might write — `42.5`, `1m12.5s`,
`00:01:12.500`, `1274f`, `00:00:42:15`, `85/2` — and is snapped to the sequence frame grid.
A typo'd argument is an error naming the accepted set, never a silent no-op.

## 3. Selectors, not indices

"Clip 3" stops being true the moment a ripple insert happens. Everything addressable has a
stable ULID and a name, and selectors compose:

```
#intro                       by name or id
V1                           a track by name
*                            every clip in the sequence
clip[track=V1]               attribute filter: =, !=, ^= (prefix), *= (contains)
clip[source=ast_talk]        by what it plays
clip[kind=title]             by source variant
clip[track=V1]@00:10-00:20   plus a half-open time window
track[kind=audio]            tracks
clip[track=V1]:last          :first · :last · :nth(2), applied after filtering
#intro #outro                space-separated union
```

A miss is `exit 3` and lists what actually exists:

```
$ dvs op clip.split --target '#tlk' --at 2
error: clip '#tlk' matched nothing; candidates: #talk, #title-1, #outro
```

An op that can only act on one clip refuses to guess: `clip.split` on a multi-match selector
names the count and the matches rather than taking the first.

## 4. Exit codes and JSON

`--json` puts exactly one object on stdout. Notes go to stderr, so a pipe into `jq` never
trips over them, and a closed pipe ends the process quietly instead of panicking.

| Code | Meaning |
|---|---|
| 0 | success |
| 1 | op error — valid arguments, impossible on this document |
| 2 | bad arguments — unknown flag, unparseable value |
| 3 | selector or op id matched nothing |
| 4 | lint findings of severity `error` are present |
| 5 | an external tool is missing (ffmpeg, a whisper model, piper) |
| 6 | AI budget exceeded |

Errors are `{"error":{"kind","code","message","candidates"}}` on stderr with the same code.

Every mutation prints what it did:

```json
{ "op": "clip.split", "seq": 12,
  "changed": ["clp_01J…"], "created": ["clp_01J…"],
  "warnings": [{ "code": "past-source-end", "target": "clp_01J…", "detail": "…" }],
  "snapped": [{ "field": "at", "requested": "85/2", "applied": "637637/15000", "frame": 1274 }] }
```

The `snapped` block is not decoration. An agent asked for 42.5 s on a 30000/1001 timeline;
the edit landed on frame 1274 at 42.5425 s. Reporting it is the difference between a known
quantity and a surprise three ops later.

## 5. Runtime discovery

```bash
dvs op --list                    # every op: id, about, query/network flags, schema
dvs schema --op clip.split       # one op's JSON Schema
dvs schema --project-schema      # the document schema
dvs doctor --json                # ffmpeg, encoders, whisper, missing assets, stale proxies
```

Nothing here requires a memorized manual, which matters because the manual is not in the
context window.

## 6. The digest

`dvs digest`, or `dvs render --digest digest.json`, measures what is actually there:

- per track and clip: timeline range, source and source range, fit, upscale ratio, effects,
  transition;
- gaps, with their ranges;
- titles: the text, whether it overflowed, whether it sits inside the safe area, its
  contrast against the **rendered backdrop at that frame**, and which font was used if the
  requested one was missing;
- captions: cue count, maximum characters per second, overlaps;
- audio: integrated LUFS, true peak, LRA, silences, clipped samples;
- video: black ranges, frozen ranges, scene cuts.

Frames are sampled (1 s by default) rather than every frame: a full-rate digest of a
ten-minute 1080p timeline is a full render, and the questions it answers do not need one.

## 7. Lint

29 rules, each finding carrying a selector-shaped `target` that resolves:

| Group | Rules |
|---|---|
| Timeline | `gap` `overlap` `orphan-transition` `past-source-end` `speed-frame-drop` `av-drift` |
| Media | `fps-mismatch` `vfr-source` `upscaled` `letterboxed` `missing-asset` `stale-proxy` `unreferenced-asset` |
| Audio | `loudness-out-of-spec` `true-peak` `clipping` `silence-gap` `unducked-music` |
| Picture | `black-frames` `frozen-frames` `flash` |
| Text | `title-overflow` `unsafe-area` `low-contrast` `font-fallback` `tiny-type` |
| Captions | `caption-too-fast` `caption-overlap` `caption-too-long` |

`--profile youtube|podcast|broadcast` sets the loudness target (−14, −16, −23 LUFS).
`--no-render` runs only the rules that read the document, so a structural check costs no
ffmpeg time at all. Findings of severity `error` make the command exit 4.

A rule that fires when nothing is wrong is worse than no rule: `gap` only reports a hole no
other track covers, so a lower third on V2 or a music bed on A2 is silent by design rather
than a finding.

## 8. Seeing, indirectly

```bash
dvs frame 00:00:42.500 frame.png --annotate   # one frame, numbered bboxes + clip ids
dvs sheet sheet.png --every 5s --cols 6       # grid with burned-in timecodes and ids
dvs diff a.mp4 b.mp4 --every 1s               # SSIM per sample + the ranges that changed
```

`--annotate` returns a legend as JSON alongside the PNG, so "box 3 overlaps box 7" maps to
clip ids a subsequent op can act on. `diff` answers both "did my edit change only the part I
meant" and "does this still match the golden".

## 9. Transcripts as a control surface

```bash
dvs op transcript.import --asset talk.mp4 --path words.json   # whisper.cpp, WhisperX, or [{text,start,end}]
dvs op transcript.find --asset talk.mp4 --phrase pricing      # spans in source and timeline time
dvs op transcript.cut-words --target '#talk' --words um,uh,like --min-gap 0.25
dvs op transcript.keep-phrases --target '#talk' --phrases "pricing,roadmap"
dvs op caption.generate --target '#talk' --max-cps 17
```

Transcripts are stored per *asset*, so one transcription survives every trim; a clip's words
are derived through its `sourceIn`, `speed` and `reverse`. `cut-words` ripple-deletes and
keeps linked a/v in sync; `caption.generate` respects the style's reading-rate and line
limits and reports the maximum it produced.

## 10. MCP

`dvs mcp` serves 105 tools over stdio: one per op (`clip_split`, `audio_normalize`, …) plus
seven written for the loop:

| Tool | Why it exists |
|---|---|
| `dvs_overview` | project, sequences, assets and recent history in one call |
| `dvs_apply` | a batch of ops applied transactionally — thirty edits in one round trip, all or nothing |
| `dvs_render` | render, digest and optional contact sheet in one call |
| `dvs_lint` | findings with resolvable targets |
| `dvs_frame` | a rendered frame as an image block plus its annotations |
| `dvs_transcript` | search and ranges without loading the whole transcript |
| `dvs_history` | the journal, including what a human did in a GUI |

Batching is the point: a promo is thirty ops, and thirty MCP round trips is thirty model
turns. A failure anywhere in a batch rolls the whole thing back — verified: a batch whose
third op has an unparseable time leaves the document and the journal untouched.

## 11. Determinism

Same document and same engine version gives byte-identical composited frames and mixed PCM:
no wall clock, locale or system font resolution reaches the render path, generic font
families resolve to one deterministic face, and random parameters are explicit op arguments.

Encoded bytes are *not* reproducible across ffmpeg builds, so goldens decode and compare with
SSIM and bounded sample error, and `project.json` records which ffmpeg produced a render.

## 12. Sharing a document with a human

The journal is append-only and undo is an entry in it, so `dvs history` shows a human's GUI
edits and an agent's ops in one stream, either party can undo the other's work, and replaying
the `patch` column reproduces the document. `.lock` serializes writers. `dvs export kdenlive`
hands the timeline to Kdenlive when a human wants to finish by hand.
