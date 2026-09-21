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
import { Waveforms } from "./waveform.js";
import { installSplitters } from "./splitters.js";

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
  playStat: el("play-stat"),
  playStatText: el("play-stat-text"),
};

const state = {
  snapshot: null,
  findings: [],
  frame: 0,
  fps: 30,
  frameCount: 1,
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
  onSeek: (seconds) => userSeek(secondsToFrame(seconds, state.fps)),
  onStatus: say,
});

/** The waveforms hang inside the cells the timeline just built, so this needs the grid
 *  and nothing else the view owns. A failure to fetch peaks is said out loud rather than
 *  swallowed: silent clips and clips whose sound could not be measured look identical. */
const waveforms = new Waveforms({ grid: el("tl-grid"), invoke, onError: say });

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
  wireSplitters();

  try {
    await refresh({ initial: true });
    // The readout answers its three questions from the first paint, not from the first
    // play: "stopped, no clock running, target 29.97 fps" is an answer.
    updateReadout(performance.now(), true);
    say("Ready. Press ? for the keyboard map.");
  } catch (error) {
    say(`Error: ${error.message || error}`);
    ui.connText.textContent = `no engine — ${error.message || error}`;
    return;
  }

  await listen("document-changed", async (event) => {
    const revision = event?.payload?.revision;
    if (revision !== undefined && state.snapshot && revision === state.snapshot.revision) return;
    // The engine retires playback on an edit -- the mix it was streaming belongs to a
    // document that no longer exists -- and emits monitor-ended a moment before this.
    // Saying so here is what keeps the transport from sitting there claiming to play.
    if (playback.playing || performance.now() - playback.endedAt < 1000) {
      endPlayback({ message: "Playback stopped: the document changed.", reason: "the document changed" });
    }
    await refresh({});
  });

  // The audio clock. Every event re-anchors the playhead; see clockFrame().
  await listen("monitor-position", (event) => onPosition(event?.payload));

  await listen("monitor-ended", () => {
    if (!playback.playing) return;
    // Either the sequence ran out or an edit retired the stream. Where the playhead is
    // decides which, and document-changed says the rest a moment later.
    const atEnd = state.frame >= state.frameCount - 2;
    endPlayback({
      message: atEnd ? "Reached the end." : "",
      reason: atEnd ? "reached the end" : "the engine stopped the stream",
      quiet: !atEnd,
    });
  });

  // The mixer failed mid-stream. The picture can still run, but not against a clock that
  // has stopped reporting, and the window must not pretend the sound is still there.
  await listen("monitor-failed", (event) => {
    if (playback.playing) useWallClock(String(event?.payload || "the engine stopped mixing"));
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
  // Not awaited: peaks are one IPC round trip over the whole sequence, and the cells are
  // already on screen and already named. The waveform is the last thing to arrive and the
  // only thing that can be late without costing the window information.
  drawWaveforms(snapshot);
  if (initial) timeline.fitToView();
  drawActivity(snapshot);
  drawLint(state.findings);

  ui.scrub.max = String(state.frameCount - 1);
  // Before the seek below: an edit changes every picture, so the decode queue is keyed by
  // revision and a stale entry would show the previous cut of the frame.
  frames.reset(snapshot.revision, snapshot.scale);
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

/** One attach at a time, and never one for a document that has already been superseded.
 *  Two overlapping attaches would each hang a canvas in the same cell -- both invisible,
 *  both wrong -- and the peaks of the older one would be the ones left drawn. */
let waveformQueue = Promise.resolve();
function drawWaveforms(snapshot) {
  waveformQueue = waveformQueue
    .then(() => (snapshot.revision === state.snapshot?.revision ? waveforms.attach(snapshot) : null))
    .catch((error) => say(`No waveforms: ${error.message || error}`));
}

// ---------------------------------------------------------------------------------------
// viewport and transport
// ---------------------------------------------------------------------------------------
//
// Playback is a picture chasing a clock, and which clock it is matters more than anything
// else in this file.
//
// The engine mixes the sequence, streams it to an audio device, and reports where that
// device has got to twenty times a second as `monitor-position`. The playhead follows
// *that*. The alternative -- running the picture off performance.now() while a device
// plays sound at its own rate -- drifts audibly within seconds, because the two clocks do
// not agree and the one the ear believes is the device's.
//
// With no device, or with no audio in the range, there is nothing to follow and a wall
// clock takes over. That is a real difference in what the window is doing, so the readout
// names the clock that is running and the status line announces the handover once.
//
// Frames never gate the clock. The playhead moves when the clock says so; the picture is
// whatever has finished decoding by then, and a frame that misses its moment is counted
// and reported. An honest "18 / 29.97 fps" beats a playhead that waits for a slideshow.

/** Frames the engine is asked to render ahead of the playhead. The same window app.rs
 *  warms on its own when a stream opens; asking again as the playhead moves is what keeps
 *  the picture ahead of it rather than one lucky second ahead of the start. */
const PREFETCH_AHEAD = 48;
/** Re-arm the prefetch every this many frames, so a second of playback costs two or three
 *  calls instead of thirty. */
const PREFETCH_EVERY = 12;
/** How far ahead of the playhead frames are asked for: a little under a second, which is
 *  the lead a renderer needs to hide a cold segment without holding work nobody wants. */
const WARM_AHEAD = 24;
/** How many of those may be in flight at once. Back-pressure, not a fetch limit: it is
 *  what stops a renderer slower than real time from being buried in stale requests.
 *
 *  This and LATE_TOLERANCE are a pair, and the invariant between them is the whole trick:
 *  the queue must be shorter, in time, than the tolerance. A renderer managing a frame
 *  every 60 ms works through six requests in 360 ms, inside the half second a late frame
 *  is still worth having, so everything asked for arrives in time to be shown. Raise the
 *  queue past that and every request is abandoned a moment before it would have landed --
 *  the pathological case, where the renderer is busy all day and the viewport never
 *  updates once. */
const IN_FLIGHT = 6;
/** How far behind the playhead a frame may still be worth waiting for. A picture two
 *  hundredths of a second late is the best picture available; half a second late it is
 *  work the renderer should be spending on the frames ahead instead. */
const LATE_TOLERANCE = 15;
/** How long the audio anchor may be extrapolated from. Positions arrive every 50 ms; past
 *  a couple of those the device has stopped reporting, and a picture that keeps going is
 *  a picture that has left the sound behind. It stops instead, visibly. */
const ANCHOR_HOLD_MS = 120;
/** Grace for the first position event before the wall clock is declared the winner. */
const CLOCK_WATCHDOG_MS = 600;
/** A drag is a hundred seeks; restarting the stream on each one is a stutter, not a
 *  scrub. The stream reopens this long after the last of them. */
const RESTART_AFTER_MS = 140;
/** Decoded frames held for one revision. About four seconds at 30 fps in each direction. */
const FRAME_CACHE = 192;

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

/**
 * The decode queue: <img> objects the browser loads in the background, keyed by frame
 * index for one revision. Nothing here is ever awaited -- a caller asks whether a frame
 * has arrived and moves on if it has not, which is the whole difference between playback
 * and a slideshow.
 */
const frames = {
  revision: -1,
  scale: 1,
  shown: -1,
  images: new Map(),
  /** Frames whose load has not finished. Its size is the only back-pressure signal this
   *  needs: a renderer slower than real time cannot be helped by a longer queue. */
  pending: new Set(),

  reset(revision, scale) {
    if (revision === this.revision && scale === this.scale) return;
    this.revision = revision;
    this.scale = scale;
    for (const frame of [...this.images.keys()]) this.drop(frame);
    this.images.clear();
    this.pending.clear();
    this.shown = -1;
  },

  /** Start (or find) a frame's load. */
  warm(frame) {
    if (this.revision < 0 || frame < 0 || frame >= state.frameCount) return null;
    const hit = this.images.get(frame);
    if (hit) return hit;
    const img = new Image();
    img.decoding = "async";
    const settled = () => this.pending.delete(frame);
    img.addEventListener("load", settled, { once: true });
    img.addEventListener("error", settled, { once: true });
    this.pending.add(frame);
    img.src = frameUrl(frame, this.revision, this.scale);
    this.images.set(frame, img);
    // Insertion order is play order, so the oldest entry is the one furthest behind.
    while (this.images.size > FRAME_CACHE) {
      const oldest = this.images.keys().next().value;
      if (oldest === frame) break;
      this.drop(oldest);
    }
    return img;
  },

  /** Release a frame, aborting its fetch if it is still in flight. */
  drop(frame) {
    const img = this.images.get(frame);
    if (!img) return;
    if (!img.complete) img.removeAttribute("src");
    this.pending.delete(frame);
    this.images.delete(frame);
  },

  /**
   * Abandon every load the playhead has already gone past.
   *
   * This is the difference between a player that drops frames and one that falls a second
   * behind and stays there. A frame that has not arrived by the time it is behind the
   * playhead will never be shown; leaving it in the queue means the renderer spends its
   * next second making pictures nobody can see, while the ones that could be seen wait
   * behind them. Dropping the request is what lets a renderer that can manage 16 fps show
   * 16 of the right frames a second instead of 16 of the wrong ones.
   */
  dropBefore(frame) {
    for (const [index, img] of this.images) {
      if (index >= frame || img.complete) continue;
      this.drop(index);
    }
  },

  ready(frame) {
    const img = this.images.get(frame);
    return Boolean(img && img.complete && img.naturalWidth > 0);
  },

  /** The newest frame that has arrived and is not in the future, searching back over the
   *  window the loader was ever asked for. Used when the frame that was due has not
   *  landed: a late picture is worth more than a frozen one, and the alt text names
   *  whichever frame is really up, so the two never disagree. */
  newestReadyBefore(frame) {
    const floor = Math.max(0, frame - WARM_AHEAD * 2);
    for (let f = frame - 1; f >= floor; f -= 1) if (this.ready(f)) return f;
    return -1;
  },
};

const playback = {
  playing: false,
  clock: "none", // "audio" | "wall" | "none"
  note: "", // why this clock and not the other one
  device: null,
  fromFrame: 0,
  /** Retires in-flight replies, watchdogs and position events from a stream we left. */
  epoch: 0,
  anchorFrame: 0,
  anchorAt: 0,
  pendingRestart: false,
  restartTimer: 0,
  watchdog: 0,
  stopping: null,
  raf: 0,
  startedAt: 0,
  endedAt: -1e9,
  wanted: -1,
  /** The last frame the skip accounting saw, so a seek is not read as a hundred drops. */
  accounted: -1,
  skipped: 0,
  underruns: 0,
  achieved: 0,
  shownAt: [], // when each frame was presented, for the rolling rate
  lastPrefetch: -1e9,
  prefetchFailed: false,
  readoutAt: 0,
  ranOnce: false,
  /** Why the last run ended, kept for the readout after the status line has moved on. */
  stopReason: "",
};

// --- the playhead, and the picture that follows it ---------------------------------------

/** Everything about a frame except the picture. This never waits for anything. */
function placePlayhead(frame) {
  state.frame = frame;
  const code = timecode(frame, state.fps);
  ui.timecodeText.textContent = code;
  ui.frameText.textContent = `frame ${frame}`;
  if (ui.scrub.value !== String(frame)) ui.scrub.value = String(frame);
  // The scrubber announces the timecode, not the frame index: "00:00:42:14", not "1274".
  ui.scrub.setAttribute("aria-valuetext", `${code}, frame ${frame}`);
  timeline.setPlayhead(frameToSeconds(frame, state.fps));
}

/** Show a frame if it has arrived. The alt text describes the picture that is up, not the
 *  frame the playhead is on: they differ exactly when a frame was skipped, and saying
 *  otherwise would make the one textual description of the viewport a lie. */
function showPicture(frame) {
  if (frames.shown === frame) return true;
  if (!frames.ready(frame)) return false;
  frames.shown = frame;
  ui.frame.src = frames.images.get(frame).src;
  ui.frame.alt = frameAlt(frame);
  return true;
}

let awaitedFrame = -1;

/** Show the frame now if it is here, and otherwise the moment it arrives. This is the seek
 *  path: a seek has no rate to keep up with, so nothing it waits for counts as skipped. */
function presentWhenReady(frame) {
  if (showPicture(frame)) return;
  const img = frames.warm(frame);
  if (!img) return;
  awaitedFrame = frame;
  img.addEventListener(
    "load",
    () => {
      if (awaitedFrame === frame && state.frame === frame) showPicture(frame);
    },
    { once: true },
  );
}

function setFrame(frame, { force = false } = {}) {
  const next = Math.max(0, Math.min(state.frameCount - 1, Math.round(frame)));
  if (!force && next === state.frame) return;
  placePlayhead(next);
  presentWhenReady(next);
}

/**
 * Keep the browser's decode queue and the engine's render queue ahead of the playhead.
 *
 * Two rules, and between them they are the whole difference between dropping frames and
 * falling behind for good:
 *
 *  1. Abandon what the playhead has passed. A frame that has not arrived by the time it
 *     is behind us will never be shown, and leaving it queued makes the renderer spend
 *     its next second on pictures nobody can see.
 *  2. Never queue more than IN_FLIGHT at once. Asking a renderer that manages sixteen
 *     frames a second for thirty does not get you sixteen of them -- it gets a queue of
 *     stale requests, each cancelled a moment before it would have been served, and a
 *     picture that never moves. Bounded, the requests are always for the frames nearest
 *     the playhead, so a slow renderer delivers its sixteen from the right part of the
 *     sequence and the skip counter accounts for the other fourteen.
 */
function warmAhead(frame) {
  frames.dropBefore(frame - LATE_TOLERANCE);
  for (let f = frame; f <= frame + WARM_AHEAD; f += 1) {
    if (frames.pending.size >= IN_FLIGHT) break;
    frames.warm(f);
  }
  if (frame - playback.lastPrefetch < PREFETCH_EVERY) return;
  playback.lastPrefetch = frame;
  invoke("prefetch", { fromFrame: frame, count: PREFETCH_AHEAD }).catch((error) => {
    // Once per run: a failing prefetch shows up as skipped frames anyway, and a status
    // line that repeats the same failure thirty times a second says nothing.
    if (playback.prefetchFailed) return;
    playback.prefetchFailed = true;
    say(`Prefetch failed: ${error.message || error}`);
  });
}

// --- the clock ---------------------------------------------------------------------------

const CLOCK_NAME = { audio: "audio clock", wall: "wall clock", none: "no clock running" };

function clockPhrase() {
  const name = CLOCK_NAME[playback.clock];
  return playback.note ? `${name} (${playback.note})` : name;
}

/**
 * The frame the clock says we are on.
 *
 * Both clocks are the same arithmetic from an anchor; what differs is who moves it. The
 * audio clock re-anchors on every position event, and the wall time since that anchor is
 * interpolation, not drift: positions arrive at 20 Hz, and presenting only on them would
 * cap a 30 fps sequence at 20 fps. The clamp is what makes it safe -- a device that stops
 * reporting freezes the picture within ANCHOR_HOLD_MS instead of running away from sound
 * that is no longer playing.
 */
function clockFrame(now) {
  const since = now - playback.anchorAt;
  const held = playback.clock === "audio" ? Math.min(since, ANCHOR_HOLD_MS) : since;
  return playback.anchorFrame + (held / 1000) * state.fps;
}

/** Move the clock's origin. Re-anchoring never moves the picture: the playhead is already
 *  at `frame`, and the next tick carries on from there. */
function anchor(clock, note, frame) {
  playback.clock = clock;
  playback.note = note;
  playback.anchorFrame = frame;
  playback.anchorAt = performance.now();
}

function useWallClock(why) {
  const repeat = playback.clock === "wall" && playback.note === why;
  anchor("wall", why, state.frame);
  if (repeat) return;
  updateReadout(performance.now(), true);
  say(`Playing on the wall clock: ${why}. The picture is not following any sound.`);
}

function onPosition(payload) {
  if (!playback.playing || playback.pendingRestart) return;
  const frame = Number(payload && payload.frame);
  if (!Number.isFinite(frame)) return;
  // A straggler from a stream the engine has already retired. After a restart the device
  // begins again at the new frame, and an event from before it would drag the picture back.
  if (frame + 2 < playback.fromFrame) return;
  playback.underruns = Number(payload.underruns) || 0;
  playback.anchorFrame = frame;
  playback.anchorAt = performance.now();
  if (playback.clock === "audio") return;
  clearTimeout(playback.watchdog);
  playback.watchdog = 0;
  playback.clock = "audio";
  playback.note = "";
  updateReadout(performance.now(), true);
  say(`Playing on the audio clock${playback.device ? `: ${playback.device}` : ""}.`);
}

// --- the readout --------------------------------------------------------------------------

const formatRate = (fps) => String(Math.round(fps * 100) / 100);

/**
 * Playing or not, on which clock, at what achieved rate -- in that order, always.
 *
 * Three numbers, each meaning exactly one thing. The rate is pictures per second, which
 * is what the eye is getting. "Skipped" is frames the viewer never saw at all, counted
 * from the gaps between the pictures that did appear -- not frames that merely arrived
 * late, which would report a uniformly delayed picture as a hundred drops a second and
 * make the number useless for the case it exists to catch. A picture that is complete but
 * behind the sound is the third number, and it says so in frames.
 */
function readoutText() {
  const target = formatRate(state.fps);
  const rate = `${playback.achieved.toFixed(1)} / ${target} fps`;
  const skipped = `${playback.skipped} skipped`;
  const count = playback.underruns;
  // Only while playing. The engine counts one break per starvation stretch, and tearing a
  // stream down at the end of a sequence normally costs one in the moment between the mix
  // running out and the feeder stopping -- so "1 dropout" after a clean run would be an
  // accusation about nothing. During playback it means what it says.
  const dropouts = count ? ` · ${count} audio dropout${count === 1 ? "" : "s"}` : "";
  const lag = playback.playing ? playback.wanted - frames.shown : 0;
  const behind = lag > 1 ? ` · ${lag} frames behind` : "";
  if (playback.playing) {
    return `playing · ${clockPhrase()} · ${rate} · ${skipped}${behind}${dropouts}`;
  }
  if (playback.ranOnce) {
    const why = playback.stopReason ? ` · ${playback.stopReason}` : "";
    return `stopped${why} · last run on the ${clockPhrase()} · ${rate} · ${skipped}`;
  }
  return `stopped · no clock running · target ${target} fps`;
}

function updateReadout(now, immediate = false) {
  const oldest = Math.max(playback.startedAt, now - 1000);
  while (playback.shownAt.length && playback.shownAt[0] < oldest) playback.shownAt.shift();
  // Intervals, not samples: counting the frames inside a one-second window and calling it
  // a rate reports 31 fps on a 29.97 fps sequence, because both edges of the window land
  // between frames. The span between the first and last frame shown has no such edge.
  const shown = playback.shownAt;
  const span = shown.length > 1 ? (shown[shown.length - 1] - shown[0]) / 1000 : 0;
  if (playback.playing && span >= 0.2) playback.achieved = (shown.length - 1) / span;
  if (!immediate && now - playback.readoutAt < 200) return;
  playback.readoutAt = now;
  ui.playStatText.textContent = readoutText();
  ui.playStat.dataset.playing = playback.playing ? "true" : "false";
}

// --- the loop -------------------------------------------------------------------------------

/** Put a frame on screen and account for what putting it there passed over. */
function present(frame, now) {
  if (!showPicture(frame)) return false;
  const previous = playback.accounted;
  if (previous >= 0 && frame > previous + 1) playback.skipped += frame - previous - 1;
  playback.accounted = frame;
  playback.shownAt.push(now);
  return true;
}

function tick() {
  if (!playback.playing) return;
  playback.raf = requestAnimationFrame(tick);
  const now = performance.now();

  if (!playback.pendingRestart) {
    const last = state.frameCount - 1;
    // Never backwards: re-anchoring after interpolation can land a millisecond behind, and
    // a picture that steps back reads as a fault in the edit rather than in the player.
    const want = Math.min(last, Math.max(playback.wanted, Math.floor(clockFrame(now))));
    if (want !== playback.wanted) {
      playback.wanted = want;
      placePlayhead(want);
      warmAhead(want);
    }
    // Skips are counted where they happen: between the picture that was up and the one
    // that replaces it. Every frame in that gap went by unseen, which is the only sense
    // in which a frame is "skipped" that a person can act on.
    if (frames.shown !== want && !present(want, now)) {
      // Behind is not the same as frozen. When the renderer cannot keep up, the newest
      // frame that did arrive is still the best picture there is, and showing it is what
      // makes a struggling playback look slow rather than broken.
      const late = frames.newestReadyBefore(want);
      if (late > frames.shown) present(late, now);
    }
    if (want >= last) {
      endPlayback({ message: "Reached the end.", reason: "reached the end" });
      return;
    }
  }
  updateReadout(now);
}

// --- transport commands ---------------------------------------------------------------------

function setPlayButton(on) {
  // The label carries the state (Play <-> Pause). aria-pressed on top of that would claim
  // "Pause, pressed", which is not what is happening.
  ui.playGlyph.textContent = on ? "\u25AE\u25AE" : "▶"; // two bars, text presentation
  ui.playText.textContent = on ? "Pause" : "Play";
}

async function startPlayback({ from = state.frame } = {}) {
  if (playback.playing) return;
  const last = state.frameCount - 1;
  // Parked on the last frame: play means play, so start over rather than refuse. The
  // engine rejects a stream that begins at the end of the sequence, and it is right to.
  const fromFrame = from >= last ? 0 : Math.max(0, Math.round(from));

  const epoch = (playback.epoch += 1);
  playback.playing = true;
  playback.ranOnce = true;
  playback.fromFrame = fromFrame;
  playback.device = null;
  playback.skipped = 0;
  playback.underruns = 0;
  playback.achieved = 0;
  playback.shownAt.length = 0;
  playback.wanted = -1;
  playback.accounted = fromFrame;
  playback.pendingRestart = false;
  playback.lastPrefetch = -1e9;
  playback.prefetchFailed = false;
  playback.startedAt = performance.now();
  // The wall clock carries the picture from the instant the key goes down. Waiting for the
  // first position would put an IPC round trip and a mix between the key and any movement.
  anchor("wall", "starting the audio device", fromFrame);
  setPlayButton(true);
  setFrame(fromFrame, { force: true });
  warmAhead(fromFrame);
  playback.raf = requestAnimationFrame(tick);
  updateReadout(performance.now(), true);
  say(`Playing from ${timecode(fromFrame, state.fps)} on the ${clockPhrase()}.`);

  await openStream(epoch, fromFrame);
}

/**
 * Ask the engine for sound, and decide from its answer which clock is in charge.
 *
 * There is no "no device" reply to read: with no device the command fails, naming audio,
 * so that is the error path below rather than a branch of the success path. A reply that
 * succeeded is a stream, and a stream that has not reported inside CLOCK_WATCHDOG_MS is a
 * device that never started -- silence still reports, because silence is samples the
 * device consumes like any other.
 */
async function openStream(epoch, fromFrame) {
  try {
    // Never let a stop overtake the play that follows it: both are one-way calls, and the
    // engine would kill the new stream with the old one's stop.
    await playback.stopping;
    if (epoch !== playback.epoch) return;
    const reply = await invoke("monitor_play", { fromFrame });
    if (epoch !== playback.epoch) return; // stopped or seeked while we waited
    // A name when the engine has one; the status line drops the colon when it does not.
    playback.device = (reply && reply.device) ? String(reply.device) : null;
    playback.watchdog = setTimeout(() => {
      if (epoch !== playback.epoch || playback.clock === "audio") return;
      useWallClock("the audio device is not reporting");
    }, CLOCK_WATCHDOG_MS);
  } catch (error) {
    if (epoch !== playback.epoch) return;
    useWallClock(error && error.message ? error.message : String(error));
  }
}

/**
 * End playback and say so.
 *
 * `reason` outlives the message. The status line is one line shared with everything else
 * the window says, and the op that ended playback usually wants it a moment later -- so
 * the reason also goes into the readout, where it stays until the next run.
 */
function endPlayback({ message = "", reason = "", quiet = false } = {}) {
  if (playback.playing) playback.stopReason = reason;
  clearTimeout(playback.restartTimer);
  clearTimeout(playback.watchdog);
  playback.restartTimer = 0;
  playback.watchdog = 0;
  playback.pendingRestart = false;
  if (playback.playing) {
    // Freeze the achieved rate at the truth before the state says "stopped".
    updateReadout(performance.now(), true);
    playback.playing = false;
    playback.endedAt = performance.now();
    playback.epoch += 1;
    cancelAnimationFrame(playback.raf);
    playback.raf = 0;
    setPlayButton(false);
    // The last frame of a run is often one that missed its moment; show it now that there
    // is no moment left to miss.
    presentWhenReady(state.frame);
    // Idempotent on the engine side, and the one call that releases the device whatever
    // ended playback -- the button, a key, an edit, or the end of the sequence.
    playback.stopping = invoke("monitor_stop").catch((error) => {
      say(`The audio device would not stop: ${error.message || error}`);
    });
  }
  updateReadout(performance.now(), true);
  if (message && !quiet) say(message);
}

function togglePlayback() {
  if (playback.playing) endPlayback({ message: `Paused at ${timecode(state.frame, state.fps)}.` });
  else void startPlayback({});
}

function playFromStart() {
  endPlayback({ quiet: true });
  setFrame(0, { force: true });
  void startPlayback({ from: 0 });
}

/** A seek the user made. The playhead goes where they put it; if sound is running, the
 *  stream reopens from there once they stop moving. */
function userSeek(frame, message) {
  setFrame(frame);
  if (message) say(message);
  if (!playback.playing) return;
  if (state.frame >= state.frameCount - 1) {
    endPlayback({ message: "Reached the end.", reason: "reached the end" });
    return;
  }
  playback.pendingRestart = true;
  playback.wanted = -1;
  // A seek is not a drop: accounting starts again from where the user put the playhead.
  playback.accounted = state.frame;
  anchor(playback.clock, "seeking", state.frame);
  clearTimeout(playback.restartTimer);
  playback.restartTimer = setTimeout(restartStream, RESTART_AFTER_MS);
}

function restartStream() {
  if (!playback.playing) return;
  const fromFrame = state.frame;
  const epoch = (playback.epoch += 1);
  playback.fromFrame = fromFrame;
  playback.pendingRestart = false;
  playback.lastPrefetch = -1e9;
  playback.device = null;
  anchor("wall", "restarting the audio device", fromFrame);
  warmAhead(fromFrame);
  playback.stopping = invoke("monitor_stop").catch((error) => {
    say(`The audio device would not stop: ${error.message || error}`);
  });
  void openStream(epoch, fromFrame);
}

/** Stepping is inspection, not transport: it stops the sound rather than reopening the
 *  stream a frame later, which is what every editor's arrow keys do. */
function step(count) {
  endPlayback({ quiet: true });
  setFrame(state.frame + count);
  say(`${timecode(state.frame, state.fps)} · frame ${state.frame}`);
}

function wireTransport() {
  el("t-home").addEventListener("click", () => userSeek(0, "First frame."));
  el("t-end").addEventListener("click", () => userSeek(state.frameCount - 1, "Last frame."));
  el("t-back-frame").addEventListener("click", () => step(-1));
  el("t-fwd-frame").addEventListener("click", () => step(1));
  el("t-back-sec").addEventListener("click", () => step(-Math.round(state.fps)));
  el("t-fwd-sec").addEventListener("click", () => step(Math.round(state.fps)));
  ui.playButton.addEventListener("click", togglePlayback);

  ui.scrub.addEventListener("input", () => userSeek(Number(ui.scrub.value)));

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

/** Pane separators. The timeline is laid out from the scroller's width and the waveforms
 *  are rasterised at the cell's size, so both have to be redrawn once a pane settles —
 *  during the drag the CSS grid alone keeps up, and re-rendering per pointer event would
 *  make a drag feel like mud. */
function wireSplitters() {
  installSplitters({
    announce: (message) => say(message),
    onResize: () => {
      if (!state.snapshot) return;
      timeline.render(state.snapshot, state.findings);
      drawWaveforms(state.snapshot);
      timeline.setPlayhead(frameToSeconds(state.frame, state.fps));
    },
  });
}

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
        if (event.shiftKey) playFromStart();
        else togglePlayback();
        return;
      // The editor's habit: k is play/pause everywhere focus happens to be, including
      // inside the timeline grid, where space belongs to the cell under the cursor.
      case "k":
      case "K":
        event.preventDefault();
        togglePlayback();
        return;
      // The other editor's habit, and the reason the transport has no reverse: the pair
      // that steps a frame without leaving the home row.
      case ",":
        event.preventDefault();
        step(-1);
        return;
      case ".":
        event.preventDefault();
        step(1);
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
        userSeek(0, "First frame.");
        return;
      case "End":
        if (ownsArrows(target)) return;
        event.preventDefault();
        userSeek(state.frameCount - 1, "Last frame.");
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
window.dvsStudio = {
  state,
  playback,
  frames,
  waveforms,
  timeline,
  setFrame,
  userSeek,
  startPlayback,
  endPlayback,
  playFromStart,
  togglePlayback,
  refresh,
  runLint,
  say,
  announce,
};
window.dvsTimelineTests = runGeometryTests;

boot();
