// The transport, and the only file that knows whether Tauri is there.
//
// Two environments, one API:
//
//   * inside the Tauri window   -> window.__TAURI__.core.invoke / .event.listen, and frames
//                                  from the dvsframe: custom scheme;
//   * in a plain browser        -> fixture.json, a command interpreter that really mutates
//                                  that snapshot, and frames drawn as SVG data URLs.
//
// The fallback is not a demo mode. It is how this UI gets audited -- axe-core, a keyboard
// pass, a screen reader -- without building Rust, so it drives the same code paths the real
// bridge does, including emitting a genuine `document-changed` event a few seconds after
// load so the live region can be observed doing its job.
//
// It also carries a simulated audio device: real time, real 20 Hz `monitor-position`
// events, the same reply shapes, and the failures worth rehearsing (no device, a device
// that never starts). The clock logic is the one part of this window that can be wrong in
// a way nobody notices until playback drifts, so it is the part that most needs to be
// testable in a browser.

const tauri = typeof window !== "undefined" ? window.__TAURI__ : undefined;
export const isTauri = Boolean(tauri && tauri.core && typeof tauri.core.invoke === "function");

/** The origin the `dvsframe` scheme is served from, asked of Tauri rather than guessed.
 *
 *  The mapping is per-platform and not guessable from the user agent: WebKitGTK and WKWebView
 *  get a real `dvsframe://localhost`, while Windows (WebView2) and Android are served
 *  `http://dvsframe.localhost`. Sniffing the UA got Linux wrong, and the failure was silent
 *  — every frame request 404'd inside the webview, the viewport sat on "no frame yet", and
 *  the browser fixture (which never uses this path) stayed green. `convertFileSrc` is the
 *  API whose whole job is this mapping; it is also what the CSP in tauri.conf.json is
 *  written against, which is why both spellings appear there. */
function frameOrigin() {
  const convert = tauri?.core?.convertFileSrc;
  if (typeof convert === "function") {
    // Any path works as a probe: we want the origin it produces, not the path.
    const probe = convert("0", "dvsframe");
    const cut = probe.lastIndexOf("/0");
    if (cut > 0) return probe.slice(0, cut);
  }
  // convertFileSrc is part of @tauri-apps/api's core module and is always present in a v2
  // window. If Tauri ever drops it, WebKitGTK's spelling is the one this app ships on.
  return "dvsframe://localhost";
}

export const connection = isTauri
  ? { mode: "tauri", label: "live", detail: "connected to the engine" }
  : { mode: "fixture", label: "fixture", detail: "no engine: showing ui/fixture.json" };

/**
 * Fixture knobs, read from the query string so an audit is a URL rather than a code edit.
 * They exist because the interesting half of playback is what happens when something is
 * missing, and none of those states can be reached in a browser by asking nicely.
 *
 *   ?device=none     monitor_play fails, naming audio. This is what the engine really
 *                    does when there is no output device -- it does not succeed with a
 *                    null device -- so it is the window's fallback path, not a branch of
 *                    the happy one.
 *   ?device=stalled  the stream opens and the device never reports a position. Silence
 *                    does not do this (silence is samples, and the device consumes them
 *                    and keeps counting); a device that failed to start does.
 *   ?frames=http     frame pictures are fetched over HTTP instead of inlined as data
 *                    URLs. A data: URL is decoded instantly, which is precisely the case
 *                    the skipped-frame path does not cover; an HTTP request is something
 *                    a harness can delay, throttle or fail.
 *   ?dropouts=N      report N audio dropouts, to watch them reach the readout.
 */
function fixtureKnobs() {
  const search = typeof location === "undefined" ? "" : location.search;
  const params = new URLSearchParams(search);
  const device = params.get("device") || "";
  return {
    device: device === "none" || device === "stalled" ? device : device || "fixture: synthetic tone",
    frames: params.get("frames") || "data",
    dropouts: Math.max(0, Number.parseInt(params.get("dropouts") || "0", 10) || 0),
  };
}

/** Module scope because devFrame() is synchronous and runs before the bridge is built. */
const knobs = fixtureKnobs();

// ---------------------------------------------------------------------------------------
// public API
// ---------------------------------------------------------------------------------------

export async function invoke(command, args = {}) {
  if (isTauri) return tauri.core.invoke(command, args);
  const dev = await devBridge();
  return dev.invoke(command, args);
}

export async function listen(event, handler) {
  if (isTauri) return tauri.event.listen(event, handler);
  const dev = await devBridge();
  return dev.listen(event, handler);
}

/** The composited frame as a URL the webview can cache. */
export function frameUrl(frameIndex, revision, scale) {
  if (isTauri) {
    return `${frameOrigin()}/${frameIndex}?rev=${revision}&scale=${scale}`;
  }
  return devFrame(frameIndex, revision, scale);
}

// ---------------------------------------------------------------------------------------
// the development bridge
// ---------------------------------------------------------------------------------------

let devPromise = null;
function devBridge() {
  if (!devPromise) devPromise = buildDevBridge();
  return devPromise;
}

let devState = null; // shared with devFrame(), which is synchronous

async function buildDevBridge() {
  const response = await fetch(new URL("./fixture.json", import.meta.url));
  if (!response.ok) throw new Error(`fixture.json: ${response.status} ${response.statusText}`);
  const fixture = await response.json();

  const state = {
    snapshot: structuredClone(fixture.snapshot),
    findings: structuredClone(fixture.findings),
    undoStack: [],
    redoStack: [],
    listeners: new Map(),
    monitor: { playing: false, timer: 0, startedAt: 0, fromFrame: 0, underruns: 0 },
    prefetches: [],
  };
  devState = state;

  const wanted = Number.parseInt(new URLSearchParams(location.search).get("clips") || "", 10);
  if (Number.isFinite(wanted) && wanted > state.snapshot.timeline.clips.length) {
    synthesiseClips(state.snapshot, wanted);
  }

  const emit = (event, payload) => {
    for (const handler of state.listeners.get(event) || []) handler({ event, payload });
  };

  /** A revision bump retires playback, exactly as app.rs does when the watcher fires: the
   *  mix that was streaming belongs to a document that no longer exists. Emitting
   *  monitor-ended before document-changed matches the order the window sees them in. */
  const retire = () => {
    if (!state.monitor.playing) return;
    stopMonitor(state);
    emit("monitor-ended", null);
  };

  const bumped = (op) => {
    retire();
    state.snapshot.revision += 1;
    emit("document-changed", { revision: state.snapshot.revision });
    return op;
  };

  // The scripted agent edit. This is the whole point of the fallback: something that is
  // not the user changes the document while the window is open.
  const scripted = fixture.scriptedChange;
  if (scripted) {
    setTimeout(() => {
      retire();
      applyScripted(state, scripted);
      emit("document-changed", { revision: state.snapshot.revision });
    }, scripted.afterMs ?? 3500);
  }

  // The live objects an audit needs: which prefetches were asked for, and what the
  // simulated device thinks it is doing.
  if (typeof window !== "undefined") {
    window.dvsFixture = { knobs, prefetches: state.prefetches, monitor: state.monitor };
  }

  return {
    listen(event, handler) {
      const list = state.listeners.get(event) || [];
      list.push(handler);
      state.listeners.set(event, list);
      return Promise.resolve(() => {
        state.listeners.set(
          event,
          (state.listeners.get(event) || []).filter((h) => h !== handler),
        );
      });
    },
    async invoke(command, args) {
      switch (command) {
        case "snapshot":
          return structuredClone(state.snapshot);
        case "lint":
          state.findings = recomputeFindings(state);
          return structuredClone(state.findings);
        case "describe":
          return describe(state.snapshot, recomputeFindings(state));
        case "run_command": {
          const parsed = parseCommandLine(String(args.line || ""));
          if (!parsed.op) throw new Error("no op: type something like clip.split --target '#intro' --at 42.5");
          return bumped(applyOp(state, parsed.op, parsed.args));
        }
        case "apply_op":
          return bumped(applyOp(state, args.op, args.args || {}));
        case "monitor_play": {
          const fromFrame = Math.max(0, Math.round(Number(args.fromFrame) || 0));
          if (fromFrame >= state.snapshot.frameCount) {
            throw new Error("the playhead is at the end of the sequence");
          }
          if (knobs.device === "none") {
            throw new Error("no audio device: cpal found no default output");
          }
          state.monitor.fromFrame = fromFrame;
          // "stalled" opens the stream and never reports, which is what a device that
          // failed to start looks like from here.
          if (knobs.device !== "stalled") startMonitor(state, emit, fromFrame);
          return { playing: true, rate: 48000, channels: 2, device: deviceName(), fromFrame };
        }
        case "monitor_stop":
          stopMonitor(state);
          return null;
        case "monitor_state":
          return {
            playing: state.monitor.playing,
            frame: monitorFrame(state),
            device: deviceName(),
            underruns: state.monitor.underruns,
          };
        case "peaks":
          return synthPeaks(state.snapshot, Math.min(4096, Math.max(16, Number(args.buckets) || 2048)));
        case "prefetch": {
          state.prefetches.push({
            fromFrame: Math.round(Number(args.fromFrame) || 0),
            count: Math.round(Number(args.count) || 0),
            at: performance.now(),
          });
          while (state.prefetches.length > 256) state.prefetches.shift();
          return null;
        }
        case "undo": {
          const previous = state.undoStack.pop();
          if (!previous) return null;
          state.redoStack.push(snapshotOf(state));
          restore(state, previous);
          retire();
          state.snapshot.revision += 1;
          emit("document-changed", { revision: state.snapshot.revision });
          return `undid ${previous.label}`;
        }
        case "redo": {
          const next = state.redoStack.pop();
          if (!next) return null;
          state.undoStack.push(snapshotOf(state));
          restore(state, next);
          retire();
          state.snapshot.revision += 1;
          emit("document-changed", { revision: state.snapshot.revision });
          return `redid ${next.label}`;
        }
        default:
          throw new Error(`unknown command '${command}'`);
      }
    },
  };
}

// --- rational time, kept exact the way the engine keeps it --------------------------------

function ratio(value) {
  if (typeof value === "number") return value;
  const text = String(value ?? "0");
  const slash = text.indexOf("/");
  if (slash < 0) return Number.parseFloat(text) || 0;
  return (Number.parseFloat(text.slice(0, slash)) || 0) / (Number.parseFloat(text.slice(slash + 1)) || 1);
}

function fpsParts(fps) {
  const text = String(fps);
  const slash = text.indexOf("/");
  if (slash < 0) return { num: Number.parseFloat(text) || 30, den: 1 };
  return { num: Number.parseFloat(text.slice(0, slash)), den: Number.parseFloat(text.slice(slash + 1)) };
}

/** Frame index -> the exact rational string the engine would have written. */
function frameTime(frame, fps) {
  const { num, den } = fpsParts(fps);
  return `${Math.round(frame) * den}/${num}`;
}

function toFrame(seconds, fps) {
  return Math.round(seconds * ratio(fps));
}

function clock(seconds) {
  const s = Math.max(0, seconds);
  const hh = Math.floor(s / 3600);
  const mm = Math.floor((s % 3600) / 60);
  const ss = s % 60;
  return `${String(hh).padStart(2, "0")}:${String(mm).padStart(2, "0")}:${ss.toFixed(3).padStart(6, "0")}`;
}

/** Accepts every spelling the engine accepts: 42.5, 1m12.5s, 00:01:12.500, 1274f, 85/2. */
function parseTimeArg(value, fps) {
  const text = String(value).trim();
  if (/^-?\d+f$/.test(text)) return Number.parseInt(text, 10) / ratio(fps);
  if (/^-?\d+(\.\d+)?\/\d+(\.\d+)?$/.test(text)) return ratio(text);
  // 00:01:12.500 is a clock; 00:00:42:15 is a timecode, whose last field counts frames.
  const stamp = /^(\d{1,2}):(\d{2}):(\d{2})([:.])(\d+)$/.exec(text);
  if (stamp) {
    const sub =
      stamp[4] === ":" ? Number(stamp[5]) / Math.max(1, Math.round(ratio(fps))) : Number(`0.${stamp[5]}`);
    return Number(stamp[1]) * 3600 + Number(stamp[2]) * 60 + Number(stamp[3]) + sub;
  }
  const span = /^(?:(\d+(?:\.\d+)?)m)?(?:(\d+(?:\.\d+)?)s)?$/.exec(text);
  if (span && (span[1] || span[2])) return Number(span[1] || 0) * 60 + Number(span[2] || 0);
  const plain = Number.parseFloat(text);
  return Number.isFinite(plain) ? plain : 0;
}

// --- the command line ---------------------------------------------------------------------

/** `clip.split --target '#intro' --at 42.5` -> { op, args }. Quoting matches the shell's
 *  single/double quotes, because that is what an agent will paste in here. */
export function parseCommandLine(line) {
  const tokens = [];
  const pattern = /'([^']*)'|"([^"]*)"|(\S+)/g;
  let match;
  while ((match = pattern.exec(line)) !== null) {
    tokens.push(match[1] ?? match[2] ?? match[3]);
  }
  const op = tokens.shift() || "";
  const args = {};
  for (let i = 0; i < tokens.length; i += 1) {
    const token = tokens[i];
    if (!token.startsWith("--")) continue;
    const key = token.slice(2);
    const next = tokens[i + 1];
    if (next === undefined || next.startsWith("--")) {
      args[key] = true;
    } else {
      args[key] = next;
      i += 1;
    }
  }
  return { op, args };
}

const KNOWN_OPS = [
  "clip.split",
  "clip.trim",
  "clip.remove",
  "clip.enable",
  "clip.disable",
  "marker.add",
  "transition.add",
  "audio.normalize",
  "title.lower-third",
];

function resolveClip(snapshot, selector) {
  if (!selector || selector === true) return null;
  const want = String(selector).replace(/^#/, "");
  return (
    snapshot.timeline.clips.find((clip) => clip.id === want || clip.label === want) || null
  );
}

function candidates(snapshot) {
  return snapshot.timeline.clips.map((clip) => `#${clip.label}`).join(", ");
}

function applyOp(state, op, args) {
  const snapshot = state.snapshot;
  const fps = snapshot.fps;
  if (!KNOWN_OPS.includes(op)) {
    throw new Error(
      `unknown op '${op}'; the development fixture implements ${KNOWN_OPS.join(", ")}`,
    );
  }
  state.undoStack.push(snapshotOf(state, op));
  state.redoStack.length = 0;

  for (const clip of snapshot.timeline.clips) clip.touched = false;
  const applied = { op, summary: "", changed: [], created: [], removed: [], warnings: [], snapped: [] };
  const target = resolveClip(snapshot, args.target);

  switch (op) {
    case "clip.split": {
      if (!target) throw new Error(`clip '${args.target}' matched nothing; candidates: ${candidates(snapshot)}`);
      const requested = parseTimeArg(args.at ?? 0, fps);
      const frame = toFrame(requested, fps);
      const at = frame / ratio(fps);
      if (at <= ratio(target.start) || at >= ratio(target.end)) {
        state.undoStack.pop();
        throw new Error(
          `--at ${args.at} is outside #${target.label} (${clock(ratio(target.start))}-${clock(ratio(target.end))})`,
        );
      }
      const tail = structuredClone(target);
      tail.id = `${target.id}_b`;
      tail.label = `${target.label}-b`;
      tail.start = frameTime(frame, fps);
      tail.touched = true;
      target.end = frameTime(frame, fps);
      target.touched = true;
      target.announce = clipAnnounce(target, snapshot);
      tail.announce = clipAnnounce(tail, snapshot);
      snapshot.timeline.clips.push(tail);
      applied.changed.push(target.id);
      applied.created.push(tail.id);
      applied.snapped.push(`at ${requested} s snapped to frame ${frame}`);
      applied.summary = `split #${target.label} at frame ${frame}, created #${tail.label}`;
      pushActivity(state, op, `--target #${target.label} --at ${args.at}`, `agent ran ${op} on #${target.label}, ${requested} seconds snapped to frame ${frame}`);
      break;
    }
    case "clip.trim": {
      if (!target) throw new Error(`clip '${args.target}' matched nothing; candidates: ${candidates(snapshot)}`);
      const requested = parseTimeArg(args.end ?? args.at ?? 0, fps);
      const frame = toFrame(requested, fps);
      target.end = frameTime(frame, fps);
      target.touched = true;
      target.announce = clipAnnounce(target, snapshot);
      applied.changed.push(target.id);
      applied.snapped.push(`end ${requested} s snapped to frame ${frame}`);
      applied.summary = `trimmed #${target.label} to frame ${frame}`;
      pushActivity(state, op, `--target #${target.label} --end ${args.end ?? args.at}`, `agent ran ${op} on #${target.label}, ${requested} seconds snapped to frame ${frame}`);
      break;
    }
    case "clip.remove": {
      if (!target) throw new Error(`clip '${args.target}' matched nothing; candidates: ${candidates(snapshot)}`);
      snapshot.timeline.clips = snapshot.timeline.clips.filter((clip) => clip.id !== target.id);
      applied.removed.push(target.id);
      applied.summary = `removed #${target.label}`;
      pushActivity(state, op, `--target #${target.label}`, `agent ran ${op} and removed #${target.label}`);
      break;
    }
    case "clip.enable":
    case "clip.disable": {
      if (!target) throw new Error(`clip '${args.target}' matched nothing; candidates: ${candidates(snapshot)}`);
      target.enabled = op === "clip.enable";
      target.touched = true;
      applied.changed.push(target.id);
      applied.summary = `${target.enabled ? "enabled" : "disabled"} #${target.label}`;
      pushActivity(state, op, `--target #${target.label}`, `agent ${target.enabled ? "enabled" : "disabled"} #${target.label}`);
      break;
    }
    case "marker.add": {
      const requested = parseTimeArg(args.at ?? 0, fps);
      const frame = toFrame(requested, fps);
      const name = args.name === true || args.name === undefined ? `marker ${snapshot.timeline.markers.length + 1}` : String(args.name);
      snapshot.timeline.markers.push({ at: frameTime(frame, fps), name });
      applied.snapped.push(`at ${requested} s snapped to frame ${frame}`);
      applied.summary = `added marker ${name} at frame ${frame}`;
      pushActivity(state, op, `--at ${args.at} --name ${name}`, `agent added marker ${name} at frame ${frame}`);
      break;
    }
    case "transition.add": {
      if (!target) throw new Error(`clip '${args.target}' matched nothing; candidates: ${candidates(snapshot)}`);
      const seconds = parseTimeArg(args.for ?? 0.5, fps);
      const frames = toFrame(seconds, fps);
      target.transition = { kind: args.kind === true || !args.kind ? "dissolve" : String(args.kind), duration: frameTime(frames, fps) };
      target.touched = true;
      target.announce = clipAnnounce(target, snapshot);
      applied.changed.push(target.id);
      applied.snapped.push(`for ${seconds} s snapped to ${frames} frames`);
      applied.summary = `${target.transition.kind} into #${target.label} over ${frames} frames`;
      pushActivity(state, op, `--target #${target.label} --kind ${target.transition.kind}`, `agent ran ${op} on #${target.label}, ${target.transition.kind} in over ${frames} frames`);
      break;
    }
    default: {
      applied.summary = `${op} applied`;
      applied.warnings.push(`the development fixture does not simulate ${op}; the timeline is unchanged`);
      pushActivity(state, op, Object.entries(args).map(([k, v]) => `--${k} ${v}`).join(" "), `agent ran ${op}`);
    }
  }

  snapshot.timeline.rows.forEach((row, index) => {
    row.clipCount = snapshot.timeline.clips.filter((clip) => clip.row === index).length;
    row.announce = `${row.name}, ${row.kind} track, ${row.clipCount} ${row.clipCount === 1 ? "clip" : "clips"}`;
  });
  recomputeGaps(snapshot);
  state.findings = recomputeFindings(state);
  return applied;
}

function clipAnnounce(clip, snapshot) {
  const row = snapshot.timeline.rows[clip.row];
  const start = ratio(clip.start);
  const end = ratio(clip.end);
  const transition = clip.transition
    ? `, ${clip.transition.kind} in over ${ratio(clip.transition.duration).toFixed(4)} seconds`
    : "";
  const off = clip.enabled ? "" : ", disabled";
  return `${clip.label}, ${clip.kind} clip on ${row ? row.name : "?"}, ${trim(start)} to ${trim(end)} seconds${transition}${off}, plays ${clip.source}`;
}

const trim = (n) => Number(n.toFixed(4)).toString();

function pushActivity(state, op, summary, announce) {
  const now = new Date();
  state.snapshot.activity.unshift({
    seq: (state.snapshot.activity[0]?.seq ?? 0) + 1,
    actor: "human",
    op,
    summary,
    at: now.toTimeString().slice(0, 8),
    announce: announce.replace(/^agent /, "you ran ").replace(/^you ran ran /, "you ran "),
    fresh: true,
  });
}

function recomputeGaps(snapshot) {
  const gaps = [];
  snapshot.timeline.rows.forEach((row, index) => {
    const clips = snapshot.timeline.clips
      .filter((clip) => clip.row === index)
      .sort((a, b) => ratio(a.start) - ratio(b.start));
    let cursor = 0;
    for (const clip of clips) {
      const start = ratio(clip.start);
      if (start - cursor > 1e-9) {
        gaps.push(makeGap(row, index, cursor, start));
      }
      cursor = Math.max(cursor, ratio(clip.end));
    }
    const duration = ratio(snapshot.duration);
    if (duration - cursor > 1e-9 && clips.length) {
      gaps.push(makeGap(row, index, cursor, duration));
    }
  });
  snapshot.timeline.gaps = gaps;
}

function makeGap(row, index, start, end) {
  const kindWord = row.kind === "audio" ? "silence" : "black";
  return {
    row: index,
    track: row.id,
    start: `${start}/1`,
    end: `${end}/1`,
    announce: `${trim(end - start)} seconds of ${kindWord} on ${row.name} from ${trim(start)} to ${trim(end)} seconds`,
  };
}

function recomputeFindings(state) {
  const snapshot = state.snapshot;
  const findings = [];
  for (const clip of snapshot.timeline.clips) {
    if (clip.id === "clp_lower_third") {
      findings.push({
        rule: "low-contrast",
        severity: "error",
        target: `#${clip.label}`,
        detail: "title text measures 2.8:1 against the rendered backdrop at 00:00:04:00; the floor is 4.5:1",
        clip: clip.id,
      });
    }
  }
  for (const gap of snapshot.timeline.gaps) {
    const row = snapshot.timeline.rows[gap.row];
    // A gap only counts when nothing of the *same kind* covers it: a music bed on A1 does
    // not fill a hole in the picture, and a lower third on V2 does. This mirrors the
    // engine's `gap` rule, whose whole point is not firing when nothing is wrong.
    const covered = snapshot.timeline.clips.some((clip) => {
      const other = snapshot.timeline.rows[clip.row];
      return (
        clip.row !== gap.row &&
        other &&
        other.kind === row.kind &&
        ratio(clip.start) <= ratio(gap.start) + 1e-9 &&
        ratio(clip.end) >= ratio(gap.end) - 1e-9
      );
    });
    if (covered) continue;
    findings.push({
      rule: "gap",
      severity: "warning",
      target: `${row.name}@${clock(ratio(gap.start))}-${clock(ratio(gap.end))}`,
      detail: gap.announce,
      clip: null,
    });
  }
  return findings;
}

function snapshotOf(state, label) {
  return {
    label: label || "the last edit",
    snapshot: structuredClone(state.snapshot),
    findings: structuredClone(state.findings),
  };
}

function restore(state, entry) {
  const revision = state.snapshot.revision;
  state.snapshot = structuredClone(entry.snapshot);
  state.findings = structuredClone(entry.findings);
  state.snapshot.revision = revision;
}

function applyScripted(state, scripted) {
  const snapshot = state.snapshot;
  for (const clip of snapshot.timeline.clips) clip.touched = false;
  for (const [id, patch] of Object.entries(scripted.clipPatch || {})) {
    const clip = snapshot.timeline.clips.find((c) => c.id === id);
    if (clip) Object.assign(clip, patch);
  }
  for (const id of scripted.touched || []) {
    const clip = snapshot.timeline.clips.find((c) => c.id === id);
    if (clip) clip.touched = true;
  }
  for (const gap of scripted.addGaps || []) snapshot.timeline.gaps.push(structuredClone(gap));
  if (scripted.activity) snapshot.activity.unshift(structuredClone(scripted.activity));
  snapshot.revision = scripted.revision ?? snapshot.revision + 1;
  state.findings = recomputeFindings(state);
}

/** `?clips=200`: stretch the fixture to N clips so the scroll and arrow-navigation floor
 *  can be measured against something the size of a real edit. */
function synthesiseClips(snapshot, wanted) {
  const template = snapshot.timeline.clips.find((clip) => clip.kind === "video");
  const rows = snapshot.timeline.rows.length;
  const duration = ratio(snapshot.duration);
  const existing = snapshot.timeline.clips.length;
  const extra = wanted - existing;
  const perRow = Math.ceil(extra / rows);
  const slot = duration / (perRow + 1);
  const fps = snapshot.fps;
  let made = 0;
  for (let row = 0; row < rows && made < extra; row += 1) {
    for (let i = 0; i < perRow && made < extra; i += 1) {
      const start = duration + slot * i;
      const end = start + slot * 0.9;
      const clip = structuredClone(template);
      clip.id = `clp_bulk_${row}_${i}`;
      clip.label = `bulk-${row}-${i}`;
      clip.row = row;
      clip.track = snapshot.timeline.rows[row].id;
      clip.kind = ["video", "audio", "title", "image", "caption"][(row + i) % 5];
      clip.start = frameTime(toFrame(start, fps), fps);
      clip.end = frameTime(toFrame(end, fps), fps);
      clip.transition = null;
      clip.touched = false;
      clip.announce = clipAnnounce(clip, snapshot);
      snapshot.timeline.clips.push(clip);
      made += 1;
    }
  }
  const total = duration + slot * perRow + slot;
  snapshot.duration = `${total}/1`;
  snapshot.timeline.duration = snapshot.duration;
  snapshot.frameCount = toFrame(total, fps);
  snapshot.timeline.rows.forEach((row, index) => {
    row.clipCount = snapshot.timeline.clips.filter((clip) => clip.row === index).length;
    row.announce = `${row.name}, ${row.kind} track, ${row.clipCount} clips`;
  });
}

/** The text view, composed the way dvs_studio::state::describe composes it, so the
 *  "read as text" dialog says the same thing in both environments. */
function describe(snapshot, findings) {
  const lines = [];
  lines.push(
    `${snapshot.projectName} · sequence ${snapshot.sequenceName} · ${snapshot.size[0]}×${snapshot.size[1]} · ${snapshot.fps} fps · ${clock(ratio(snapshot.duration))} (${snapshot.frameCount} frames)`,
  );
  lines.push(snapshot.root, "", "timeline");
  snapshot.timeline.rows.forEach((row, index) => {
    lines.push(`  ${row.announce}`);
    for (const clip of snapshot.timeline.clips.filter((c) => c.row === index)) {
      lines.push(`    ${clip.announce}`);
    }
    for (const gap of snapshot.timeline.gaps.filter((g) => g.row === index)) {
      lines.push(`    ${gap.announce}`);
    }
  });
  if (snapshot.timeline.markers.length) {
    lines.push("markers");
    for (const marker of snapshot.timeline.markers) {
      lines.push(`  ${clock(ratio(marker.at))} ${marker.name}`);
    }
  }
  lines.push("", "activity");
  if (!snapshot.activity.length) lines.push("  (nothing yet)");
  for (const entry of snapshot.activity.slice(0, 20)) lines.push(`  ${entry.at} ${entry.announce}`);
  lines.push("", "lint");
  if (!findings.length) lines.push("  no findings");
  for (const finding of findings) {
    lines.push(`  ${finding.severity} [${finding.rule}] ${finding.target} — ${finding.detail}`);
  }
  return lines.join("\n") + "\n";
}

// --- the simulated device -------------------------------------------------------------------
//
// Real time, real 20 Hz reporting, and the same event names and payloads app.rs emits.
// The window cannot tell the difference, which is the point: the clock logic -- the one
// piece of this UI that can be wrong in a way nobody notices until playback drifts -- gets
// audited in a browser rather than only in a built window.

/** app.rs ticks its reporter every 50 ms. Matching it matters: the window interpolates
 *  between positions, and the interpolation is only honest if the gap is the real one. */
const POSITION_MS = 50;

/** The name the engine would report. "stalled" is a fixture state, not a device name. */
function deviceName() {
  return knobs.device === "stalled" ? "fixture: a device that will not start" : knobs.device;
}

function startMonitor(state, emit, fromFrame) {
  stopMonitor(state);
  const monitor = state.monitor;
  const fps = ratio(state.snapshot.fps);
  const frameCount = state.snapshot.frameCount;
  monitor.playing = true;
  monitor.fromFrame = fromFrame;
  monitor.startedAt = performance.now();
  monitor.underruns = knobs.dropouts;
  monitor.timer = setInterval(() => {
    const frame = monitorFrame(state);
    emit("monitor-position", { frame, underruns: monitor.underruns });
    // app.rs stops when the device has played past the end of the sequence, not at the
    // last frame: the last frame has a duration too.
    if (frame >= frameCount) {
      stopMonitor(state);
      emit("monitor-ended", null);
    }
  }, POSITION_MS);
}

function stopMonitor(state) {
  clearInterval(state.monitor.timer);
  state.monitor.timer = 0;
  state.monitor.playing = false;
}

/** Where the device has got to: the fixture's whole audio clock, in one line. */
function monitorFrame(state) {
  const monitor = state.monitor;
  if (!monitor.playing) return monitor.fromFrame;
  const played = (performance.now() - monitor.startedAt) / 1000;
  return Math.floor(monitor.fromFrame + played * ratio(state.snapshot.fps));
}

// --- synthetic peaks --------------------------------------------------------------------------

/**
 * A tone with a shape, bucketed the way dvs_audio buckets a real mix: min and max over
 * every sample in the bucket, across channels, ignoring mute and solo. A real carrier is
 * far finer than a bucket, so what a bucket holds is the envelope -- which is what these
 * hold. The buckets span the whole sequence, not the clip, because that is the contract
 * waveform.js reads them under: peaks do not depend on the zoom or on the cut.
 */
function synthPeaks(snapshot, buckets) {
  const duration = ratio(snapshot.timeline.duration || snapshot.duration) || 1;
  return snapshot.timeline.rows
    .filter((row) => row.kind === "audio")
    .map((row) => {
      const clips = snapshot.timeline.clips.filter((clip) => clip.track === row.id && clip.enabled);
      const values = new Array(buckets);
      let loudest = 0;
      for (let b = 0; b < buckets; b += 1) {
        const t = ((b + 0.5) / buckets) * duration;
        const clip = clips.find((c) => ratio(c.start) <= t && ratio(c.end) > t);
        if (!clip) {
          values[b] = [0, 0];
          continue;
        }
        // Edges fade rather than cliff, so the drawing shows a shape a person recognises.
        const fade = Math.min(1, (t - ratio(clip.start)) / 0.75, (ratio(clip.end) - t) / 0.75);
        const beat = 0.55 + 0.45 * Math.abs(Math.sin((t * Math.PI) / 1.5));
        const phrase = 0.5 + 0.5 * Math.sin((t * Math.PI) / 21 + 1.1);
        const level = 0.86 * fade * beat * (0.45 + 0.55 * phrase);
        // Asymmetric, the way recorded material is: a waveform drawn from |sample| is a
        // waveform that cannot show a DC offset or an inverted transient.
        values[b] = [-level, level * 0.94];
        if (level > loudest) loudest = level;
      }
      return {
        track: row.id,
        name: row.name,
        buckets: values,
        // Matches dvs_audio::SILENCE_FLOOR_DB, which is what waveform.js expects to read.
        peakDb: loudest > 0 ? 20 * Math.log10(loudest) : -100,
      };
    });
}

// --- fake frames ---------------------------------------------------------------------------

/** A generated picture, so the viewport is exercised (letterboxing, alt text, cache keys)
 *  without an engine. It draws what the frame *would* contain: the clips live at that
 *  instant, on a slow gradient so scrubbing visibly moves. */
function devFrame(frameIndex, revision, scale) {
  if (knobs.frames === "http") {
    // One real request per frame, the way dvsframe:// works in the window: the same URL
    // for the same frame, so a re-seek is answered by the browser's cache, and a harness
    // that wants frames to be slow has something it can actually hold on to.
    return `frame-fixture.svg?frame=${frameIndex}&rev=${revision}&scale=${scale}`;
  }
  const snapshot = devState?.snapshot;
  const width = snapshot ? snapshot.size[0] : 1920;
  const height = snapshot ? snapshot.size[1] : 1080;
  const fps = snapshot ? ratio(snapshot.fps) : 30;
  const seconds = frameIndex / fps;
  const visible = snapshot
    ? snapshot.timeline.clips
        .filter((clip) => ratio(clip.start) <= seconds && ratio(clip.end) > seconds)
        .map((clip) => `${clip.label} (${snapshot.timeline.rows[clip.row]?.name || "?"})`)
    : [];
  const hue = (frameIndex * 0.1) % 360;
  const label = visible.length ? visible.join(" · ") : "black";
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" role="img">
<defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
<stop offset="0" stop-color="hsl(${hue} 45% 18%)"/><stop offset="1" stop-color="hsl(${(hue + 40) % 360} 55% 8%)"/>
</linearGradient></defs>
<rect width="100%" height="100%" fill="url(#g)"/>
<g fill="#f2f5fa" font-family="monospace" text-anchor="middle">
<text x="${width / 2}" y="${height / 2 - 40}" font-size="${Math.round(height / 9)}">frame ${frameIndex}</text>
<text x="${width / 2}" y="${height / 2 + 60}" font-size="${Math.round(height / 18)}">${escapeXml(label)}</text>
<text x="${width / 2}" y="${height - 60}" font-size="${Math.round(height / 28)}" fill="#aab4c4">fixture render · rev ${revision} · scale ${scale}</text>
</g></svg>`;
  return `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}`;
}

function escapeXml(value) {
  return String(value).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&apos;" })[c]);
}
