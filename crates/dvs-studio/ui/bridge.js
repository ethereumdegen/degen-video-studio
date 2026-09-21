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

const tauri = typeof window !== "undefined" ? window.__TAURI__ : undefined;
export const isTauri = Boolean(tauri && tauri.core && typeof tauri.core.invoke === "function");

/** Tauri v2 serves custom schemes as `scheme://localhost` on macOS and iOS, and as
 *  `http://scheme.localhost` on Linux and Windows. Both are in the CSP in tauri.conf.json. */
function frameOrigin() {
  const ua = navigator.userAgent || "";
  const appleLike = /Mac OS X|iPhone|iPad/.test(ua) && !/Windows|Linux|Android/.test(ua);
  return appleLike ? "dvsframe://localhost" : "http://dvsframe.localhost";
}

export const connection = isTauri
  ? { mode: "tauri", label: "live", detail: "connected to the engine" }
  : { mode: "fixture", label: "fixture", detail: "no engine: showing ui/fixture.json" };

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
  };
  devState = state;

  const wanted = Number.parseInt(new URLSearchParams(location.search).get("clips") || "", 10);
  if (Number.isFinite(wanted) && wanted > state.snapshot.timeline.clips.length) {
    synthesiseClips(state.snapshot, wanted);
  }

  const emit = (event, payload) => {
    for (const handler of state.listeners.get(event) || []) handler({ event, payload });
  };

  const bumped = (op) => {
    state.snapshot.revision += 1;
    emit("document-changed", { revision: state.snapshot.revision });
    return op;
  };

  // The scripted agent edit. This is the whole point of the fallback: something that is
  // not the user changes the document while the window is open.
  const scripted = fixture.scriptedChange;
  if (scripted) {
    setTimeout(() => {
      applyScripted(state, scripted);
      emit("document-changed", { revision: state.snapshot.revision });
    }, scripted.afterMs ?? 3500);
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
        case "undo": {
          const previous = state.undoStack.pop();
          if (!previous) return null;
          state.redoStack.push(snapshotOf(state));
          restore(state, previous);
          state.snapshot.revision += 1;
          emit("document-changed", { revision: state.snapshot.revision });
          return `undid ${previous.label}`;
        }
        case "redo": {
          const next = state.redoStack.pop();
          if (!next) return null;
          state.undoStack.push(snapshotOf(state));
          restore(state, next);
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
    const covered = snapshot.timeline.clips.some(
      (clip) =>
        clip.row !== gap.row &&
        ratio(clip.start) <= ratio(gap.start) + 1e-9 &&
        ratio(clip.end) >= ratio(gap.end) - 1e-9,
    );
    if (covered) continue;
    const row = snapshot.timeline.rows[gap.row];
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

// --- fake frames ---------------------------------------------------------------------------

/** A generated picture, so the viewport is exercised (letterboxing, alt text, cache keys)
 *  without an engine. It draws what the frame *would* contain: the clips live at that
 *  instant, on a slow gradient so scrubbing visibly moves. */
function devFrame(frameIndex, revision, scale) {
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
