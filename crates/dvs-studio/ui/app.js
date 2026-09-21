// The window: header, viewport, transport, timeline, activity, lint, console, dialogs.
//
// Everything that reaches the engine goes through bridge.js; everything that draws the
// timeline lives in timeline.js. This file owns the state the two share -- which frame the
// playhead is on, which clip is selected, what the status line last said -- and the
// keyboard model.

import { invoke, listen, frameUrl, connection } from "./bridge.js";
import {
  TimelineView,
  parseRational,
  fpsValue,
  timecode,
  secondsToFrame,
  frameToSeconds,
  runGeometryTests,
} from "./timeline.js";

const el = (id) => document.getElementById(id);

const ui = {
  status: el("status"),
  connText: el("conn-text"),
  conn: document.querySelector(".conn"),
  frame: el("frame"),
  scrub: el("scrub"),
  timecodeText: el("timecode-text"),
  frameText: el("frame-text"),
  playButton: el("t-play"),
  playGlyph: el("t-play-glyph"),
  playText: el("t-play-text"),
  activityLive: el("activity-live"),
  activityList: el("activity-list"),
  lintBody: el("lint-body"),
  consoleForm: el("console-form"),
  consoleInput: el("console-input"),
  describeOut: el("describe-out"),
  dlgHelp: el("dlg-help"),
  dlgText: el("dlg-text"),
};

const state = {
  snapshot: null,
  findings: [],
  frame: 0,
  fps: 30,
  frameCount: 1,
  playing: false,
  playStartedAt: 0,
  playFromFrame: 0,
  rafHandle: 0,
  lastAnnounced: "",
  lintStale: false,
};

const timeline = new TimelineView({
  scroller: el("tl-scroller"),
  content: el("tl-content"),
  grid: el("tl-grid"),
  ruler: el("tl-ruler"),
  markers: el("tl-markers"),
  playhead: el("tl-playhead"),
  onSelect: (id, cell) => {
    if (!cell) return;
    say(cell.getAttribute("aria-label"));
  },
  onSeek: (seconds) => setFrame(secondsToFrame(seconds, state.fps)),
  onStatus: say,
});

// ---------------------------------------------------------------------------------------
// status and announcements
// ---------------------------------------------------------------------------------------

function say(message) {
  if (!message) return;
  ui.status.textContent = message;
}

/** One composed sentence into the feed's live region -- never the list itself, which would
 *  make a screen reader re-read every row on every op. */
function announce(sentence) {
  if (!sentence || sentence === state.lastAnnounced) return;
  state.lastAnnounced = sentence;
  ui.activityLive.textContent = sentence;
}

// ---------------------------------------------------------------------------------------
// boot
// ---------------------------------------------------------------------------------------

async function boot() {
  ui.conn.dataset.mode = connection.mode;
  ui.connText.textContent = `${connection.label} — ${connection.detail}`;

  // Wire the controls before the first fetch: if the engine is unreachable, the help
  // dialog and the keyboard map still have to work so a user can find out why.
  wireTransport();
  wireConsole();
  wireDialogs();
  wireShortcuts();

  try {
    await refresh({ initial: true });
    say("Ready. Press ? for the keyboard map.");
  } catch (error) {
    say(`Error: ${error.message || error}`);
    ui.connText.textContent = `no engine — ${error.message || error}`;
    return;
  }

  await listen("document-changed", async (event) => {
    const revision = event?.payload?.revision;
    if (revision !== undefined && state.snapshot && revision === state.snapshot.revision) return;
    await refresh({});
  });

  if (new URLSearchParams(location.search).has("test")) showGeometryTests();
}

async function refresh({ initial = false } = {}) {
  const snapshot = await invoke("snapshot");
  const previous = state.snapshot;
  state.snapshot = snapshot;
  state.fps = fpsValue(snapshot.fps);
  state.frameCount = Math.max(1, snapshot.frameCount);
  if (initial) {
    state.findings = await invoke("lint");
    state.lintStale = false;
  } else if (previous && previous.revision !== snapshot.revision) {
    // Lint is not re-run on every edit on purpose: some rules render frames, and an agent
    // applying thirty ops must not pay for thirty renders. The panel says so instead.
    state.lintStale = true;
  }

  drawHeader(snapshot);
  timeline.render(snapshot, state.findings);
  if (initial) timeline.fitToView();
  drawActivity(snapshot);
  drawLint(state.findings);

  ui.scrub.max = String(state.frameCount - 1);
  setFrame(Math.min(state.frame, state.frameCount - 1), { force: true });

  const newest = snapshot.activity[0];
  if (newest) announce(newest.announce);
}

function drawHeader(snapshot) {
  el("fact-project").textContent = snapshot.projectName;
  el("fact-sequence").textContent = snapshot.sequenceName;
  el("fact-size").textContent = `${snapshot.size[0]}×${snapshot.size[1]}`;
  const fps = fpsValue(snapshot.fps);
  el("fact-fps").textContent =
    String(snapshot.fps) === String(fps) ? `${fps} fps` : `${snapshot.fps} (${fps.toFixed(3)}) fps`;
  el("fact-duration").textContent =
    `${timecode(snapshot.frameCount, fps)} · ${snapshot.frameCount} frames`;
}

// ---------------------------------------------------------------------------------------
// viewport and transport
// ---------------------------------------------------------------------------------------

/** What is on screen at this instant, from the document -- never guessed from the picture. */
function visibleAt(seconds) {
  const model = state.snapshot.timeline;
  return model.clips
    .filter((clip) => parseRational(clip.start) <= seconds && parseRational(clip.end) > seconds)
    .sort((a, b) => a.row - b.row)
    .map((clip) => `${clip.label} on ${model.rows[clip.row]?.name ?? "?"}`);
}

function frameAlt(frame) {
  const seconds = frameToSeconds(frame, state.fps);
  const showing = visibleAt(seconds);
  const what = showing.length ? `showing ${list(showing)}` : "nothing on any track: black";
  return `Frame ${frame} of ${state.frameCount}, ${timecode(frame, state.fps)}, ${what}.`;
}

function list(items) {
  if (items.length === 1) return items[0];
  return `${items.slice(0, -1).join(", ")} and ${items[items.length - 1]}`;
}

function setFrame(frame, { force = false } = {}) {
  const next = Math.max(0, Math.min(state.frameCount - 1, Math.round(frame)));
  if (!force && next === state.frame) return;
  state.frame = next;

  const seconds = frameToSeconds(next, state.fps);
  const code = timecode(next, state.fps);

  ui.frame.src = frameUrl(next, state.snapshot.revision, state.snapshot.scale);
  ui.frame.alt = frameAlt(next);
  ui.timecodeText.textContent = code;
  ui.frameText.textContent = `frame ${next}`;
  if (ui.scrub.value !== String(next)) ui.scrub.value = String(next);
  // The scrubber announces the timecode, not the frame index: "00:00:42:14", not "1274".
  ui.scrub.setAttribute("aria-valuetext", `${code}, frame ${next}`);
  timeline.setPlayhead(seconds);
}

function step(frames) {
  stopPlayback();
  setFrame(state.frame + frames);
  say(`${timecode(state.frame, state.fps)} · frame ${state.frame}`);
}

function startPlayback() {
  if (state.playing) return;
  state.playing = true;
  state.playStartedAt = performance.now();
  state.playFromFrame = state.frame;
  // The label carries the state (Play <-> Pause). aria-pressed on top of that would claim
  // "Pause, pressed", which is not what is happening.
  ui.playGlyph.textContent = "\u25AE\u25AE"; // two bars, text presentation (no emoji font)
  ui.playText.textContent = "Pause";
  const tick = () => {
    if (!state.playing) return;
    const elapsed = (performance.now() - state.playStartedAt) / 1000;
    const frame = state.playFromFrame + elapsed * state.fps;
    if (frame >= state.frameCount - 1) {
      setFrame(state.frameCount - 1);
      stopPlayback();
      say("Reached the end.");
      return;
    }
    setFrame(frame);
    state.rafHandle = requestAnimationFrame(tick);
  };
  state.rafHandle = requestAnimationFrame(tick);
  say("Playing.");
}

function stopPlayback() {
  if (!state.playing) return;
  state.playing = false;
  cancelAnimationFrame(state.rafHandle);
  ui.playGlyph.textContent = "▶";
  ui.playText.textContent = "Play";
}

function togglePlayback() {
  if (state.playing) {
    stopPlayback();
    say(`Paused at ${timecode(state.frame, state.fps)}.`);
  } else {
    startPlayback();
  }
}

function wireTransport() {
  el("t-home").addEventListener("click", () => {
    stopPlayback();
    setFrame(0);
    say("First frame.");
  });
  el("t-end").addEventListener("click", () => {
    stopPlayback();
    setFrame(state.frameCount - 1);
    say("Last frame.");
  });
  el("t-back-frame").addEventListener("click", () => step(-1));
  el("t-fwd-frame").addEventListener("click", () => step(1));
  el("t-back-sec").addEventListener("click", () => step(-Math.round(state.fps)));
  el("t-fwd-sec").addEventListener("click", () => step(Math.round(state.fps)));
  ui.playButton.addEventListener("click", togglePlayback);

  ui.scrub.addEventListener("input", () => {
    stopPlayback();
    setFrame(Number(ui.scrub.value));
  });

  // The buttons zoom around the middle of the visible time area, the way the wheel zooms
  // around the pointer.
  const centre = () => timeline.view.x + timeline.view.width / 2;
  el("zoom-in").addEventListener("click", () => timeline.zoomAt(centre(), 1.5));
  el("zoom-out").addEventListener("click", () => timeline.zoomAt(centre(), 1 / 1.5));
  el("zoom-fit").addEventListener("click", () => {
    timeline.fitToView();
    say("Timeline fitted to the window.");
  });
  el("btn-lint").addEventListener("click", runLint);
}

// ---------------------------------------------------------------------------------------
// activity and lint
// ---------------------------------------------------------------------------------------

const ACTOR_WORD = { agent: "agent", human: "you", ai: "ai" };

function drawActivity(snapshot) {
  const entries = [...snapshot.activity].sort((a, b) => b.seq - a.seq);
  const frag = document.createDocumentFragment();
  for (const entry of entries) {
    const li = document.createElement("li");
    if (entry.fresh) li.classList.add("is-fresh");

    const badge = document.createElement("span");
    badge.className = "badge";
    badge.dataset.actor = entry.actor;
    // The actor is a word, not merely a colour.
    badge.textContent = ACTOR_WORD[entry.actor] ?? entry.actor;

    const op = document.createElement("span");
    op.className = "feed-op";
    op.textContent = entry.op;

    const at = document.createElement("time");
    at.className = "feed-time";
    at.textContent = entry.at;

    const summary = document.createElement("span");
    summary.className = "feed-summary";
    summary.textContent = entry.summary;

    li.append(badge, op, at, summary);
    frag.appendChild(li);
  }
  ui.activityList.replaceChildren(frag);
}

const SEVERITY_ORDER = ["error", "warning", "warn", "info"];
const SEVERITY_GLYPH = { error: "✖", warning: "⚠", warn: "⚠", info: "i" };

function drawLint(findings) {
  const frag = document.createDocumentFragment();
  if (state.lintStale) {
    frag.appendChild(
      nodeFrom("p", "empty", "The document changed since this ran. Press l to run lint again."),
    );
  }
  if (!findings.length) {
    frag.appendChild(nodeFrom("p", "empty", "No findings."));
    ui.lintBody.replaceChildren(frag);
    return;
  }

  const groups = new Map();
  for (const finding of findings) {
    const key = finding.severity.toLowerCase();
    if (!groups.has(key)) groups.set(key, []);
    groups.get(key).push(finding);
  }
  const ordered = [...groups.keys()].sort((a, b) => indexOfSeverity(a) - indexOfSeverity(b));

  for (const severity of ordered) {
    const items = groups.get(severity);
    const heading = document.createElement("h3");
    heading.id = `lint-${severity}`;
    heading.textContent = `${severity} (${items.length})`;
    const ul = document.createElement("ul");
    ul.setAttribute("aria-labelledby", heading.id);

    for (const finding of items) {
      const li = document.createElement("li");
      li.appendChild(findingNode(finding, severity));
      ul.appendChild(li);
    }
    frag.append(heading, ul);
  }
  ui.lintBody.replaceChildren(frag);
}

/** A finding that names a clip is a button that selects it. One that does not is text --
 *  never a disabled button, which is a control that looks operable and is not. */
function findingNode(finding, severity) {
  const sev = document.createElement("span");
  sev.className = "sev";
  sev.dataset.sev = severity;
  // Severity is a glyph and a word as well as a colour.
  sev.textContent = `${SEVERITY_GLYPH[severity] ?? "\u2022"} ${severity}`;

  const target = document.createElement("span");
  target.className = "finding-target";
  target.textContent = `${finding.rule} · ${finding.target}`;

  const detail = document.createElement("span");
  detail.className = "finding-detail";
  detail.textContent = finding.detail;

  if (!finding.clip) {
    const div = document.createElement("div");
    div.className = "finding";
    div.append(sev, target, detail);
    return div;
  }

  const button = document.createElement("button");
  button.type = "button";
  button.className = "finding";
  // The button's own sentence is the accessible name; the three spans repeat it visually,
  // so they are hidden rather than read twice over.
  for (const node of [sev, target, detail]) node.setAttribute("aria-hidden", "true");
  button.setAttribute(
    "aria-label",
    `${severity}, ${finding.rule}, ${finding.target}: ${finding.detail}. Select ${finding.target}.`,
  );
  button.append(sev, target, detail);
  button.addEventListener("click", () => {
    if (timeline.selectClip(finding.clip)) {
      say(`Selected ${finding.target} from the ${finding.rule} finding.`);
    } else {
      say(`${finding.target} is no longer in the timeline.`);
    }
  });
  return button;
}

function indexOfSeverity(severity) {
  const index = SEVERITY_ORDER.indexOf(severity);
  return index < 0 ? SEVERITY_ORDER.length : index;
}

function nodeFrom(tag, className, text) {
  const node = document.createElement(tag);
  node.className = className;
  node.textContent = text;
  return node;
}

async function runLint() {
  try {
    state.findings = await invoke("lint");
    state.lintStale = false;
    drawLint(state.findings);
    timeline.render(state.snapshot, state.findings);
    const errors = state.findings.filter((f) => f.severity === "error").length;
    say(
      state.findings.length
        ? `Lint: ${state.findings.length} findings, ${errors} of them errors.`
        : "Lint: no findings.",
    );
  } catch (error) {
    say(`Error: ${error.message || error}`);
  }
}

// ---------------------------------------------------------------------------------------
// console
// ---------------------------------------------------------------------------------------

function wireConsole() {
  ui.consoleForm.addEventListener("submit", async (event) => {
    event.preventDefault();
    const line = ui.consoleInput.value.trim();
    if (!line) return;
    try {
      const applied = await invoke("run_command", { line });
      ui.consoleInput.value = "";
      const parts = [applied.summary];
      if (applied.snapped?.length) parts.push(applied.snapped.join("; "));
      if (applied.warnings?.length) parts.push(`warnings: ${applied.warnings.join("; ")}`);
      say(parts.filter(Boolean).join(" · "));
    } catch (error) {
      say(`Error: ${error.message || error}`);
    }
  });

  el("btn-undo").addEventListener("click", () => runHistory("undo"));
  el("btn-redo").addEventListener("click", () => runHistory("redo"));
}

async function runHistory(which) {
  try {
    const result = await invoke(which);
    say(result || `Nothing to ${which}.`);
  } catch (error) {
    say(`Error: ${error.message || error}`);
  }
}

// ---------------------------------------------------------------------------------------
// dialogs
// ---------------------------------------------------------------------------------------

let dialogInvoker = null;

function openDialog(dialog, invoker) {
  dialogInvoker = invoker instanceof HTMLElement ? invoker : null;
  dialog.showModal();
}

function wireDialogs() {
  for (const dialog of document.querySelectorAll("dialog")) {
    // Escape and the close button both land here. Returning focus to whatever opened the
    // dialog is the difference between "closed" and "lost".
    dialog.addEventListener("close", () => {
      const invoker = dialogInvoker;
      dialogInvoker = null;
      if (invoker && invoker.isConnected) invoker.focus();
    });
  }

  el("btn-help").addEventListener("click", (event) => openDialog(ui.dlgHelp, event.currentTarget));
  el("btn-text").addEventListener("click", async (event) => {
    const invoker = event.currentTarget;
    ui.describeOut.textContent = "Reading…";
    openDialog(ui.dlgText, invoker);
    try {
      ui.describeOut.textContent = await invoke("describe");
    } catch (error) {
      ui.describeOut.textContent = `error: ${error.message || error}`;
    }
    ui.describeOut.focus();
  });
}

// ---------------------------------------------------------------------------------------
// keyboard
// ---------------------------------------------------------------------------------------

function isTyping(target) {
  return (
    target instanceof HTMLInputElement ||
    target instanceof HTMLTextAreaElement ||
    (target instanceof HTMLElement && target.isContentEditable)
  );
}

function ownsArrows(target) {
  if (!(target instanceof HTMLElement)) return false;
  if (target instanceof HTMLInputElement) return true; // the scrubber and the console
  return Boolean(target.closest('[role="gridcell"], [role="rowheader"]'));
}

function wireShortcuts() {
  document.addEventListener("keydown", (event) => {
    if (event.defaultPrevented || event.altKey || event.metaKey) return;
    const target = event.target;

    if (document.querySelector("dialog[open]")) return; // the dialog owns the keyboard

    if (isTyping(target)) {
      if (event.key === "Escape") {
        // Never drop focus on <body>: that loses a screen reader its place in the page.
        // The timeline's active cell is where the work is, so go back there.
        target.blur();
        timeline.focusFirstCell();
        say("Left the console.");
      }
      return;
    }

    switch (event.key) {
      case " ":
        // Space activates whatever has focus if that thing takes activation.
        if (target instanceof HTMLElement && target.closest("button, [role=\"gridcell\"], [role=\"rowheader\"], input")) return;
        event.preventDefault();
        togglePlayback();
        return;
      case "ArrowLeft":
        if (ownsArrows(target)) return;
        event.preventDefault();
        step(event.shiftKey ? -Math.round(state.fps) : -1);
        return;
      case "ArrowRight":
        if (ownsArrows(target)) return;
        event.preventDefault();
        step(event.shiftKey ? Math.round(state.fps) : 1);
        return;
      case "Home":
        if (ownsArrows(target)) return;
        event.preventDefault();
        stopPlayback();
        setFrame(0);
        say("First frame.");
        return;
      case "End":
        if (ownsArrows(target)) return;
        event.preventDefault();
        stopPlayback();
        setFrame(state.frameCount - 1);
        say("Last frame.");
        return;
      case "u":
        event.preventDefault();
        runHistory("undo");
        return;
      case "r":
        event.preventDefault();
        runHistory("redo");
        return;
      case "l":
        event.preventDefault();
        runLint();
        return;
      case "/":
        event.preventDefault();
        ui.consoleInput.focus();
        ui.consoleInput.select();
        return;
      case "?":
        event.preventDefault();
        openDialog(ui.dlgHelp, target instanceof HTMLElement ? target : el("btn-help"));
        return;
      default:
    }
  });
}

// ---------------------------------------------------------------------------------------
// the geometry suite, on a ?test=1 page
// ---------------------------------------------------------------------------------------

function showGeometryTests() {
  const result = runGeometryTests();
  const section = document.createElement("section");
  section.className = "panel";
  section.id = "geometry-tests";
  section.setAttribute("aria-labelledby", "geometry-tests-h");
  const heading = document.createElement("h2");
  heading.id = "geometry-tests-h";
  heading.textContent = "Geometry tests";
  const pre = document.createElement("pre");
  pre.id = "geometry-tests-out";
  pre.tabIndex = 0;
  pre.setAttribute("aria-label", "Geometry test results");
  pre.textContent =
    `${result.total - result.failed} of ${result.total} assertions passed\n` +
    (result.failures.length ? result.failures.join("\n") : "no failures");
  section.append(heading, pre);
  document.querySelector("main").appendChild(section);
  say(result.failed ? `Geometry tests: ${result.failed} FAILED` : `Geometry tests: all ${result.total} passed`);
}

// A small, deliberate testing surface: the audit script and anyone poking at the window in
// a browser console need a handle on the live objects.
window.dvsStudio = { state, timeline, setFrame, refresh, runLint, say, announce };
window.dvsTimelineTests = runGeometryTests;

boot();
