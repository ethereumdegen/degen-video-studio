# The document format

`project.json` is the whole edit. It is the canonical form: the CLI, the MCP server and any
GUI all read and write this and nothing else. The JSON Schema is published — `dvs schema
--project-schema` — and generated from the Rust types in `dvs-core/src/project.rs`, so it
cannot drift from what the engine accepts.

## Directory layout

```text
myproject/
├── project.json      canonical document (small, diffable, never contains media)
├── history.jsonl     append-only journal: {seq, ts, actor, op, args, patch, inverse, target}
├── assets/           content-addressed media: assets/<first2hex>/<blake3hex>.<ext>
├── transcript/       word-timestamped transcripts, one per asset id
├── cache/            proxies, waveforms, thumbnails, rendered segments — all regenerable
└── .lock             writer lock
```

Nothing in `cache/` or `transcript/` is part of the document's identity: delete the
directory and every artifact rebuilds. `assets/` is content-addressed, so importing the same
file twice costs one blob, and an edited original cannot silently change a finished edit.

## Top level

```jsonc
{
  "degenVideo": 1,                  // format version; a reader refuses a higher number
  "id": "prj_01J…",
  "name": "promo",
  "created": "2026-09-21T14:00:00Z",
  "modified": "2026-09-21T14:31:02Z",
  "activeSequence": "seq_01J…",     // what an op edits when --seq is omitted
  "assets":    { "ast_…": { … } },
  "sequences": { "seq_…": { … } },
  "titles":    { "ttl_…": { … } },  // omitted when empty
  "styles":    { "sty_…": { … } },  // caption styles, omitted when empty
  "tools": { "engine": "dvs-0.1.0", "ffmpeg": "ffmpeg version n9.0.1 …", "encoders": ["libx264"] }
}
```

`tools` is provenance, not configuration. Composited frames are deterministic across
machines; *encoded bytes* are not, because they depend on the ffmpeg build — so the build
that produced a render is recorded rather than assumed.

## Time

Every position and duration is an exact rational number of seconds, serialized as
`"num/den"`:

```jsonc
"start": "0/1", "duration": "1001/30", "sourceIn": "12/1"
```

Frame rates are exact too: `"30000/1001"`, never `29.97`. Readers also accept a decimal
(`12.5`), a clock (`00:01:12.500`), a duration (`1m12s`), and — where a frame rate is known —
a frame count (`1800f`) or non-drop timecode (`00:01:12:15`). Writers always emit the
rational.

Why it is not a float: 29.97 fps is `30000/1001`, and an hour of it is exactly 107892
frames. Accumulating `1/29.97` in binary floating point drifts, and the drift becomes a
one-frame error somewhere in the middle of a long timeline — the kind of bug that surfaces
as "the audio is slightly out by the end".

Non-drop timecode is a *label*, not an instant: `00:01:00:00` at 30000/1001 is frame 1800,
which arrives 60.06 real seconds in. `Time::timecode` and the four-part parser are exact
inverses of each other in frame space, which is what an NLE means by the format.

## Sequence

```jsonc
"seq_main": {
  "id": "seq_main", "name": "main",
  "fps": "30000/1001", "size": [1920, 1080],
  "sampleRate": 48000, "channels": 2,
  "background": "#000000",
  "tracks": [ … ],                    // index 0 is the bottom video layer
  "markers": [ { "id": "mk_…", "at": "42/1", "name": "pricing", "color": "#fb8500" } ]
}
```

Track order is compositing order: the first video track is the bottom layer. A sequence's
duration is the end of its last clip on any track — there is no separate length field to
fall out of sync.

## Track

```jsonc
{ "id": "trk_v1", "name": "V1", "kind": "video",
  "muted": false, "solo": false, "locked": false, "hidden": false,
  "gainDb": 0.0, "pan": 0.0,
  "clips": [ … ],
  "cues":  [ … ],            // caption tracks only
  "style": "sty_default" }
```

`kind` is `video`, `audio` or `caption`. A lock guards clip *content*: `clip.*`, `fx.*`,
`kf.*`, `transition.*` and title placement refuse to touch a locked track, while the mixer
strip and the lock flag itself stay editable — otherwise a lock could never be cleared.

**Invariant: clips on a track never overlap and are sorted by `start`.** An overlap is not a
state; it is a transition, expressed as `transitionIn` on the later clip. Ops cannot produce
an overlap, and the engine re-validates every sequence after every op, so a hand-edited
document with overlapping clips is rejected before it can reach a render.

## Clip

```jsonc
{
  "id": "clp_intro", "name": "intro",
  "source": { "kind": "asset", "asset": "ast_talk" },
  "start": "0/1", "duration": "18/1", "sourceIn": "12/1",
  "speed": "1/1", "reverse": false,
  "transform": { "pos": [0, 0], "scale": [1, 1], "rotation": 0, "anchor": [0.5, 0.5] },
  "opacity": 1.0, "blend": "normal", "fit": "contain",
  "crop": { "left": 0, "top": 0, "right": 0, "bottom": 0 },
  "effects": [ { "id": "fx_1", "kind": "color.grade", "enabled": true, "params": { "saturation": 1.2 } } ],
  "keyframes": { "transform.scale": [ { "at": "0/1", "value": 1.0, "easing": "ease-in-out" },
                                      { "at": "2/1", "value": 1.1 } ] },
  "transitionIn": { "kind": "dissolve", "duration": "1/2", "easing": "linear" },
  "gainDb": -6.0, "pan": 0.0, "fadeIn": "1/10", "fadeOut": "1/2",
  "ducking": { "against": "trk_a1", "by": -12.0, "attack": "1/5", "release": "1/2", "threshold": -30.0 },
  "link": "clp_intro_audio",
  "enabled": true
}
```

`duration` is canonical and the source out-point is derived (`sourceIn + duration × speed`),
so retiming cannot desynchronise the two. `source_time(t)` maps a timeline instant to the
source instant to decode, with a branch for `reverse` — a reversed clip's timeline head is
the far end of its source span.

`source` is one of:

| `kind` | Fields | Meaning |
|---|---|---|
| `asset` | `asset`, `stream?` | media from the store |
| `image` | `asset` | a still |
| `title` | `title` | an SVG title document, rasterized per frame |
| `sequence` | `sequence` | nesting; cycles are rejected at render time with the path |
| `color` | `color` | flat color |
| `generator` | `generator`, `params` | `bars`, `tone`, `countdown`, `frame-numbers` |

### Keyframes

Keyed by dotted path — `opacity`, `transform.pos.x`, `transform.pos.y`, `transform.scale`,
`transform.scale.x`, `transform.scale.y`, `transform.rotation`, `fx.<effectId>.<param>` —
with clip-local times, sorted, and easing on the *outgoing* side of each key. Before the
first key and after the last, the value is held: extrapolating an animation past its keys is
never what was meant.

`fx.remove` drops the `fx.<id>.*` curves with the effect, so no orphaned animation survives.

## Titles

```jsonc
"ttl_lower": {
  "id": "ttl_lower", "name": "lower-third", "size": [1920, 1080],
  "svg": "<svg …><text>{{title}}</text><text>{{subtitle}}</text></svg>",
  "fields": { "title": "Andy Mazzola", "subtitle": "degen labs" }
}
```

The markup lives in the document rather than the asset store so a title stays editable, and
`{{field}}` substitution is XML-escaped on the way in. SVG is the interchange format
degen-paint writes, which is how a vector document authored there drops straight in.

## Captions

Cues live on a caption track:

```jsonc
{ "id": "cue_1", "span": { "start": "2/1", "end": "9/2" }, "text": "so today\nwe ship", "style": "sty_default" }
```

and a style says how they render and what counts as too fast to read:

```jsonc
"sty_default": { "id": "sty_default", "name": "default", "font": "sans-serif", "sizePx": 48,
                 "color": "#ffffff", "outline": "#000000", "position": "bottom",
                 "safeArea": 0.9, "maxCps": 20.0, "maxLines": 2 }
```

## Assets

```jsonc
"ast_talk": {
  "id": "ast_talk", "name": "talk.mp4", "hash": "blake3:9f86d0…", "kind": "video",
  "probe": {
    "duration": "5723/30", "container": "mov,mp4",
    "video": { "streamIndex": 0, "size": [1920, 1080], "fps": "30000/1001", "codec": "h264",
               "pixFmt": "yuv420p", "colorRange": "tv", "colorMatrix": "bt709",
               "rotation": 0, "sar": "1/1", "frames": 5723 },
    "audio": { "streamIndex": 1, "rate": 48000, "channels": 2, "codec": "aac" },
    "vfr": false
  },
  "proxy": "cache/proxy/ast_talk.mp4",
  "sourcePath": "/home/andy/footage/talk.mp4",
  "imported": "2026-09-21T14:02:11Z"
}
```

Two probe fields do more work than they look like:

- **`colorRange` / `colorMatrix`** are passed to ffmpeg explicitly on every decode. A
  limited-range BT.709 file decoded as full-range BT.601 looks washed out and slightly
  hue-shifted, and nothing reports it — so the file's claim is recorded and used, and an
  unflagged source falls back by resolution (BT.709 for HD, BT.601 below) rather than to
  whatever the default happens to be.
- **`vfr`** marks variable frame timing. Phone and screen recordings routinely have it, and
  "frame 1274" has no meaning in such a file, so import builds a constant-rate proxy and
  everything frame-exact runs against that. Lint reports the condition either way.

## History

`history.jsonl` is append-only, one entry per line:

```jsonc
{"seq":7,"ts":"…","actor":"agent","op":"clip.split","args":{…},"patch":[…],"inverse":[…]}
{"seq":8,"ts":"…","actor":"human","op":"project.undo","target":7,"patch":[…],"inverse":[…]}
```

Undo does not rewind the file: it applies entry 7's inverse patch and appends a
`project.undo` entry pointing at it. Consequences worth relying on: an agent reading the
journal sees what a human did in a GUI as structured ops, either party can undo the other's
work, and replaying the `patch` column from the start reproduces the current document.

Undo is one mechanism for every op because the engine diffs the document (RFC 6902) instead
of asking each op for an inverse — a per-op `undo()` across 90-plus ops is a permanent bug
farm.

`modified` is deliberately outside the patch chain: it is a write stamp, so an undo does not
rewind the clock and no journal entry exists purely to bump a timestamp.
