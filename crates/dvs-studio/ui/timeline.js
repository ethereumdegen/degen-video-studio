// The timeline: time <-> pixel geometry, and the DOM that draws it.
//
// ---------------------------------------------------------------------------------------
// Why this is a grid and not a canvas, and why a grid and not a list
// ---------------------------------------------------------------------------------------
// A <canvas> timeline is a blank wall to assistive technology: one image element with no
// children, no focus, no names. Everything here is a real element with a real role, which
// is the only reason this project runs in a webview at all.
//
// Of the two accessible shapes that fit -- `role="grid"` with a roving tabindex, or one
// `role="list"` per track -- this picks **grid**, deliberately:
//
//   * The content genuinely is two-dimensional. "The clip above this one" is a question an
//     editor asks constantly, and only the grid pattern gives up/down a defined meaning.
//     With per-row lists, up/down is something the author invents and no reading cursor
//     agrees with.
//   * A grid gets a `rowheader` per row, so the track ("V1, video track, 3 clips") is part
//     of the cell's context rather than a floating label the reader has to hunt for. Orca
//     and NVDA both announce the row header when the row changes, which is exactly the
//     behaviour you want when arrowing down from V2 to V1.
//   * Focusable cells put Orca and NVDA into focus mode, so arrow keys reach this code
//     instead of moving the browse-mode caret. A list of focusable items does *not* do that
//     reliably: in browse mode the arrow keys walk the caret through the text and the
//     up/down handler never fires.
//   * Cell counts differ per row, which grid tolerates (the ARIA spec only calls a row's
//     cells "one or more"). `aria-colcount`/`aria-colindex` are deliberately *omitted*:
//     column indices are meaningless when row 1 has three clips and row 3 has one, and a
//     lie in an index is worse than a missing index.
//
// Gaps are cells too. A hole in a timeline is a fact about the edit -- the thing lint
// complains about -- and skipping it would make the reading order disagree with the
// picture.
//
// ---------------------------------------------------------------------------------------
// Coordinate model (ported from the Rust prototype, which was mutation-tested; the
// failure modes in the comments are the ones the tests at the bottom actually catch)
// ---------------------------------------------------------------------------------------
//   zoom   pixels per second
//   scroll the instant, IN SECONDS, at the left edge of the view -- not a pixel offset.
//          This is what makes xToTime(view.x) === scroll and turns the zoom correction
//          into one line. Porting `scroll` as pixels reintroduces the bug.
//   view   { x, width } in the scroller's own coordinate space. The vertical extent never
//          enters the time mapping.

export const RULER_H = 26; // px, the band above the first row
export const ROW_H = 34; // px, one track row
export const ROW_GAP = 2; // px, the gutter between rows -- a click here selects nothing
export const HEAD_W = 92; // px, the sticky track-header lane; matches --head-w in app.css
export const ZOOM_MIN = 0.25; // px/s
export const ZOOM_MAX = 400; // px/s

/** Everything the roving tabindex walks: the row header is column zero of its row, so
 *  arrowing left off the first clip lands on the track and hears its name. */
const NAVIGABLE = '.tl-cell, .tl-head';
export const FLASH_SECONDS = 1.2;

/** Fixed label ladder. A computed 10^round(log10 n) offers a 3.16-second grid, which no
 *  human reads; these are the intervals a person expects to see on a clock. */
const LABEL_LADDER = [
  0.04, 0.1, 0.2, 0.5, 1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 1800, 3600,
];
const LABEL_TARGET = 8; // aim for at most this many labels across the view

// --- geometry ---------------------------------------------------------------------------

export function timeToX(seconds, zoom, scroll, view) {
  return view.x + (seconds - scroll) * zoom;
}

export function xToTime(x, zoom, scroll, view) {
  // A degenerate zoom must not produce an infinity: the caller would get an infinite
  // visible span and an infinite label loop out of it.
  if (!(zoom > 0)) return scroll;
  return scroll + (x - view.x) / zoom;
}

export function visibleSeconds(zoom, view) {
  if (!(zoom > 0)) return 0;
  return view.width / zoom;
}

/**
 * Zoom by `factor` while holding the instant under `cursorX` still.
 *
 * Two things this gets right on purpose:
 *  1. the scroll correction is computed from the **clamped** zoom. Deriving it from the
 *     requested zoom means spinning the wheel at min or max walks the view sideways one
 *     notch per tick instead of parking it;
 *  2. it does **not** clamp the resulting scroll. That needs the duration and the caller's
 *     run-out policy, and folding it in here destroys the invariant the function exists
 *     for. Use clampScroll() next.
 */
export function zoomAround(cursorX, factor, zoom, scroll, view) {
  const next = Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, zoom * factor));
  const anchor = xToTime(cursorX, zoom, scroll, view);
  return { zoom: next, scroll: anchor - (cursorX - view.x) / next };
}

/** Clamp the left-edge instant: never before zero, and about half a viewport of run-out
 *  past the end so the last clip can be pulled away from the right edge. */
export function clampScroll(scroll, zoom, duration, view) {
  const max = Math.max(0, duration - visibleSeconds(zoom, view) / 2);
  return Math.min(max, Math.max(0, scroll));
}

/**
 * Which row a y inside the timeline belongs to, or null.
 * Rejects the ruler band *and* the gutter between rows: a click in the 2 px gap selects
 * nothing rather than snapping to the nearest neighbour.
 */
export function rowAt(y, rowCount) {
  // This guard is the only thing rejecting the ruler band and anything above the widget.
  // A `row < 0` check below would shadow it and make it untestable -- a mutation that
  // deletes this line has to fail a test, or the line is decoration.
  if (y < RULER_H) return null;
  const pitch = ROW_H + ROW_GAP;
  const offset = y - RULER_H;
  const row = Math.floor(offset / pitch);
  if (row >= rowCount) return null;
  if (offset - row * pitch >= ROW_H) return null; // in the gutter below that row
  return row;
}

/** First ladder rung that keeps the label count at or under LABEL_TARGET. */
export function labelInterval(spanSeconds) {
  const wanted = spanSeconds / LABEL_TARGET;
  for (const rung of LABEL_LADDER) if (rung >= wanted) return rung;
  let rung = LABEL_LADDER[LABEL_LADDER.length - 1];
  while (rung < wanted) rung *= 2; // doubling past the top of the ladder
  return rung;
}

/**
 * Emphasis remaining on a clip an op just touched: 1 - t^2 over FLASH_SECONDS.
 * Holds bright for the first fifth of a second, then falls, and is continuous at both
 * ends -- a curve that never actually reaches zero pops instead of fading, which is why
 * the suite asserts flashDecay(1.199) < 0.01 and not merely the endpoints.
 */
export function flashDecay(elapsedSeconds) {
  if (elapsedSeconds <= 0) return 1;
  if (elapsedSeconds >= FLASH_SECONDS) return 0;
  const t = elapsedSeconds / FLASH_SECONDS;
  return 1 - t * t;
}

// --- time formatting --------------------------------------------------------------------

/** "3003/250" | "42.5" | 42.5 -> seconds. Times cross the Rust boundary as exact
 *  rationals; the view only ever needs them as pixels, so a double is enough here. */
export function parseRational(value) {
  if (typeof value === "number") return value;
  if (typeof value !== "string") return 0;
  const slash = value.indexOf("/");
  if (slash < 0) return Number.parseFloat(value) || 0;
  const num = Number.parseFloat(value.slice(0, slash));
  const den = Number.parseFloat(value.slice(slash + 1));
  return den ? num / den : 0;
}

/** Frames per second as a double, from "30000/1001". */
export const fpsValue = parseRational;

/** Non-drop timecode, HH:MM:SS:FF, on the nearest-integer timebase -- 30000/1001 counts in
 *  30 frames, which is why frame 1800 reads 00:01:00:00 at 60.06 seconds. Drop-frame
 *  (semicolon) is not produced: the engine's own clock() does not, and inventing it here
 *  would make the window disagree with `dvs`. */
export function timecode(frame, fps) {
  const base = Math.max(1, Math.round(fps));
  const f = Math.max(0, Math.round(frame));
  const ff = f % base;
  const total = Math.floor(f / base);
  const ss = total % 60;
  const mm = Math.floor(total / 60) % 60;
  const hh = Math.floor(total / 3600);
  const pad = (n) => String(n).padStart(2, "0");
  return `${pad(hh)}:${pad(mm)}:${pad(ss)}:${pad(ff)}`;
}

/** Seconds as a short clock for ruler labels: 1:04 / 12.0 / 0.40 */
export function shortClock(seconds, interval) {
  const s = Math.max(0, seconds);
  if (interval >= 1) {
    const total = Math.round(s);
    return `${Math.floor(total / 60)}:${String(total % 60).padStart(2, "0")}`;
  }
  return s.toFixed(interval >= 0.1 ? 1 : 2);
}

export const secondsToFrame = (seconds, fps) => Math.round(seconds * fps);
export const frameToSeconds = (frame, fps) => frame / fps;

// --- the view -----------------------------------------------------------------------------

const KIND_GLYPH = {
  video: "\u25B6", // play triangle
  audio: "\u266A", // eighth note
  title: "T",
  image: "\u25A3",
  color: "\u25A0",
  generator: "\u2726",
  nested: "\u29C9",
  caption: "\u2261",
};

/**
 * Draws a TimelineModel and owns its keyboard model.
 *
 * Layout rule, and the reason 200 clips stay smooth: nothing is positioned from JavaScript.
 * Every cell carries `--t0` and `--dur` in seconds, written once when it is built, and the
 * scroll container carries `--pps` (pixels per second). A zoom is therefore a single style
 * write on one element; the browser re-resolves 200 transforms from the same custom
 * property instead of this code touching 200 inline styles. Horizontal scrolling is the
 * container's own -- native scrollbars, native keyboard, no synthetic wheel handling.
 */
export class TimelineView {
  /**
   * @param {object} opts
   * @param {HTMLElement} opts.scroller   the overflow-x container
   * @param {HTMLElement} opts.content    the sized content box (owns --pps)
   * @param {HTMLElement} opts.grid       role="grid"
   * @param {HTMLElement} opts.ruler      the tick band
   * @param {HTMLElement} opts.markers    role="group" of marker buttons
   * @param {HTMLElement} opts.playhead   the playhead rule
   * @param {(id: string|null, cell: HTMLElement|null) => void} opts.onSelect
   * @param {(seconds: number) => void} opts.onSeek
   * @param {(message: string) => void} opts.onStatus
   */
  constructor(opts) {
    this.scroller = opts.scroller;
    this.content = opts.content;
    this.grid = opts.grid;
    this.ruler = opts.ruler;
    this.markersHost = opts.markers;
    this.playheadEl = opts.playhead;
    this.onSelect = opts.onSelect || (() => {});
    this.onSeek = opts.onSeek || (() => {});
    this.onStatus = opts.onStatus || (() => {});

    this.zoom = 12; // px/s
    this.duration = 0;
    this.fps = 30;
    this.rowCount = 0;
    this.rows = []; // HTMLElement[][] -- navigable cells per row, header first
    this.active = { row: 0, col: 0 };
    this.selectedId = null;
    this.flashStart = 0;
    this.flashHandle = 0;
    this.reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

    this.grid.addEventListener("keydown", (e) => this.#onKeyDown(e));
    this.grid.addEventListener("mousedown", (e) => this.#onGridPointer(e));
    this.scroller.addEventListener(
      "wheel",
      (e) => {
        if (!e.ctrlKey && !e.altKey) return; // plain wheel scrolls, as it should
        e.preventDefault();
        const rect = this.scroller.getBoundingClientRect();
        this.zoomAt(e.clientX - rect.left, e.deltaY < 0 ? 1.25 : 1 / 1.25);
      },
      { passive: false },
    );
    this.scroller.addEventListener("scroll", () => this.#drawRuler(), { passive: true });
  }

  get view() {
    // Scroller-local coordinates. x is HEAD_W, not 0: the sticky track-header lane occupies
    // the first HEAD_W pixels, so the instant at the left edge of the *time* area sits
    // there. This is precisely what the geometry model's `view.x` is for.
    return { x: HEAD_W, width: Math.max(1, this.scroller.clientWidth - HEAD_W) };
  }

  get scroll() {
    return this.scroller.scrollLeft / this.zoom;
  }

  set scroll(seconds) {
    this.scroller.scrollLeft = Math.max(0, seconds) * this.zoom;
  }

  /** Rebuild from a snapshot. Focus and selection survive by id, because a rebuild that
   *  drops focus back to <body> is a rebuild that loses a screen reader its place. */
  render(snapshot, findings = []) {
    const model = snapshot.timeline;
    this.duration = parseRational(model.duration) || parseRational(snapshot.duration);
    this.fps = fpsValue(model.fps || snapshot.fps);
    this.rowCount = model.rows.length;

    const focusedId = document.activeElement?.closest?.(NAVIGABLE)?.dataset.cellId || null;
    const flagged = new Set(findings.filter((f) => f.clip).map((f) => f.clip));

    this.content.style.setProperty("--duration", String(this.duration));
    this.content.style.setProperty("--pps", String(this.zoom));
    this.grid.setAttribute("aria-rowcount", String(model.rows.length));
    this.grid.setAttribute(
      "aria-label",
      `Timeline, ${model.rows.length} tracks, ${model.clips.length} clips`,
    );

    const byRow = model.rows.map(() => []);
    for (const clip of model.clips) if (byRow[clip.row]) byRow[clip.row].push(clip);
    const gapsByRow = model.rows.map(() => []);
    for (const gap of model.gaps) if (gapsByRow[gap.row]) gapsByRow[gap.row].push(gap);

    const frag = document.createDocumentFragment();
    this.rows = [];
    let touched = 0;

    model.rows.forEach((track, index) => {
      const row = document.createElement("div");
      row.className = "tl-row";
      row.setAttribute("role", "row");
      row.setAttribute("aria-rowindex", String(index + 1));
      row.dataset.kind = track.kind;

      const head = document.createElement("div");
      head.className = "tl-head";
      head.setAttribute("role", "rowheader");
      head.tabIndex = -1;
      head.dataset.cellId = `row:${track.id}`;
      head.setAttribute("aria-label", track.announce);
      head.innerHTML =
        `<span class="tl-head-name">${escapeHtml(track.name)}</span>` +
        `<span class="tl-head-kind">${escapeHtml(track.kind)}</span>`;
      row.appendChild(head);

      const cells = [head];
      const items = [
        ...byRow[index].map((clip) => ({ kind: "clip", at: parseRational(clip.start), clip })),
        ...gapsByRow[index].map((gap) => ({ kind: "gap", at: parseRational(gap.start), gap })),
      ].sort((a, b) => a.at - b.at);

      for (const item of items) {
        const cell =
          item.kind === "clip"
            ? this.#buildClip(item.clip, flagged.has(item.clip.id))
            : this.#buildGap(item.gap);
        if (item.kind === "clip" && item.clip.touched) touched += 1;
        row.appendChild(cell);
        cells.push(cell);
      }

      this.rows.push(cells);
      frag.appendChild(row);
    });

    this.grid.replaceChildren(frag);
    this.#buildMarkers(model.markers);
    this.#drawRuler();

    // Restore the roving tabindex, preferring whatever had focus.
    let restored = null;
    if (focusedId) restored = this.#findCell(focusedId);
    if (!restored && this.selectedId) restored = this.#findCell(this.selectedId);
    if (restored) {
      this.#setActive(restored.row, restored.col, focusedId ? "focus" : "quiet");
    } else {
      this.#setActive(0, Math.min(1, (this.rows[0]?.length || 1) - 1), "quiet");
    }
    if (this.selectedId) this.#paintSelection();
    if (touched) this.#startFlash();
  }

  #buildClip(clip, flagged) {
    const el = document.createElement("div");
    el.className = "tl-cell tl-clip";
    el.setAttribute("role", "gridcell");
    el.tabIndex = -1;
    el.dataset.cellId = clip.id;
    el.dataset.kind = clip.kind;
    el.dataset.type = "clip";
    if (!clip.enabled) el.dataset.disabledClip = "true";
    if (clip.touched) el.classList.add("is-touched");
    if (flagged) el.classList.add("is-flagged");
    el.setAttribute("aria-selected", "false");
    // The accessible name is the sentence Rust composed, never a reassembly of fragments.
    // ", changed" is appended for every touched clip rather than only under reduced
    // motion: a screen reader user never perceives the flash at all, so gating the word on
    // a *motion* preference would hide it from exactly the people who depend on it.
    el.setAttribute("aria-label", clip.announce + (clip.touched ? ", changed" : ""));
    el.style.setProperty("--t0", String(parseRational(clip.start)));
    el.style.setProperty("--dur", String(parseRational(clip.end) - parseRational(clip.start)));

    const glyph = KIND_GLYPH[clip.kind] || "\u25A0";
    const badges =
      (clip.transition ? `<span class="tl-badge">\u25E0 ${escapeHtml(clip.transition.kind)}</span>` : "") +
      (clip.enabled ? "" : `<span class="tl-badge">off</span>`) +
      (flagged ? `<span class="tl-badge tl-badge-flag">lint</span>` : "") +
      (clip.touched ? `<span class="tl-badge tl-badge-changed">changed</span>` : "");
    el.innerHTML =
      `<span class="tl-glyph" aria-hidden="true">${glyph}</span>` +
      `<span class="tl-label">${escapeHtml(clip.label)}</span>` +
      `<span class="tl-kindword">${escapeHtml(clip.kind)}</span>` +
      badges;
    return el;
  }

  #buildGap(gap) {
    const el = document.createElement("div");
    el.className = "tl-cell tl-gap";
    el.setAttribute("role", "gridcell");
    el.tabIndex = -1;
    el.dataset.cellId = `gap:${gap.track}:${gap.start}`;
    el.dataset.type = "gap";
    el.setAttribute("aria-selected", "false");
    el.setAttribute("aria-label", gap.announce);
    el.style.setProperty("--t0", String(parseRational(gap.start)));
    el.style.setProperty("--dur", String(parseRational(gap.end) - parseRational(gap.start)));
    el.innerHTML =
      `<span class="tl-glyph" aria-hidden="true">\u2716</span><span class="tl-label">gap</span>`;
    return el;
  }

  #buildMarkers(markers) {
    const frag = document.createDocumentFragment();
    for (const marker of markers) {
      const at = parseRational(marker.at);
      const button = document.createElement("button");
      button.type = "button";
      button.className = "tl-marker";
      button.style.setProperty("--t0", String(at));
      button.dataset.at = String(at);
      button.setAttribute(
        "aria-label",
        `Marker ${marker.name} at ${timecode(secondsToFrame(at, this.fps), this.fps)}, seek here`,
      );
      button.innerHTML =
        `<span class="tl-marker-pin" aria-hidden="true">\u25BC</span>` +
        `<span class="tl-marker-name">${escapeHtml(marker.name)}</span>`;
      button.addEventListener("click", () => this.onSeek(at));
      frag.appendChild(button);
    }
    this.markersHost.replaceChildren(frag);
  }

  #drawRuler() {
    const span = visibleSeconds(this.zoom, this.view);
    if (!(span > 0)) return;
    const interval = labelInterval(span);
    const first = Math.floor(this.scroll / interval) * interval;
    const last = this.scroll + span;
    const frag = document.createDocumentFragment();
    for (let t = first; t <= last + interval; t += interval) {
      if (t < 0) continue;
      const tick = document.createElement("span");
      tick.className = "tl-tick";
      tick.style.setProperty("--t0", String(t));
      tick.textContent = shortClock(t, interval);
      frag.appendChild(tick);
    }
    this.ruler.replaceChildren(frag);
  }

  // --- interaction -------------------------------------------------------------------

  #onGridPointer(event) {
    const cell = event.target.closest?.(NAVIGABLE);
    if (cell) {
      const found = this.#findCell(cell.dataset.cellId);
      if (found) this.#setActive(found.row, found.col, "focus");
      this.select(cell);
      return;
    }
    // A click on the bed: seek, and reject the gutter between rows the way the prototype
    // did -- landing on the nearest neighbour is a lie about what you clicked.
    const rect = this.scroller.getBoundingClientRect();
    const row = rowAt(event.clientY - rect.top + this.scroller.scrollTop, this.rowCount);
    if (row === null) return;
    const seconds = xToTime(event.clientX - rect.left, this.zoom, this.scroll, this.view);
    this.onSeek(Math.max(0, Math.min(this.duration, seconds)));
  }

  #onKeyDown(event) {
    const cell = event.target.closest?.(NAVIGABLE);
    if (!cell) return;
    const here = this.#findCell(cell.dataset.cellId);
    if (!here) return;
    let { row, col } = here;
    switch (event.key) {
      case "ArrowRight":
        col = Math.min(col + 1, this.rows[row].length - 1);
        break;
      case "ArrowLeft":
        col = Math.max(col - 1, 0);
        break;
      case "ArrowDown":
        row = Math.min(row + 1, this.rows.length - 1);
        col = Math.min(col, this.rows[row].length - 1);
        break;
      case "ArrowUp":
        row = Math.max(row - 1, 0);
        col = Math.min(col, this.rows[row].length - 1);
        break;
      case "Home":
        // The grid pattern wins over the transport's home/end while focus is in a cell:
        // ctrl+Home reaches the first cell of the first row, plain Home the first of this
        // row. Documented in the help dialog so the two meanings are not a surprise.
        if (event.ctrlKey) row = 0;
        col = 0;
        break;
      case "End":
        if (event.ctrlKey) row = this.rows.length - 1;
        col = this.rows[row].length - 1;
        break;
      case "Enter":
      case " ":
        event.preventDefault();
        this.select(cell);
        return;
      default:
        return;
    }
    event.preventDefault();
    this.#setActive(row, col, "focus");
  }

  select(cell) {
    if (!cell) return;
    const isClip = cell.dataset.type === "clip";
    this.selectedId = isClip ? cell.dataset.cellId : null;
    this.#paintSelection();
    if (!isClip && cell.dataset.type === "gap") {
      const start = Number.parseFloat(cell.style.getPropertyValue("--t0")) || 0;
      this.onSeek(start);
    }
    this.onSelect(this.selectedId, cell);
  }

  /** Select by clip id from outside -- the lint panel does this. */
  selectClip(id) {
    const found = this.#findCell(id);
    if (!found) return false;
    this.#setActive(found.row, found.col, "focus");
    this.select(this.rows[found.row][found.col]);
    return true;
  }

  #paintSelection() {
    for (const row of this.rows) {
      for (const cell of row) {
        if (cell.getAttribute("role") === "rowheader") continue;
        const on = cell.dataset.cellId === this.selectedId;
        cell.setAttribute("aria-selected", on ? "true" : "false");
        cell.classList.toggle("is-selected", on);
      }
    }
  }

  #findCell(id) {
    if (!id) return null;
    for (let row = 0; row < this.rows.length; row += 1) {
      const col = this.rows[row].findIndex((cell) => cell.dataset.cellId === id);
      if (col >= 0) return { row, col };
    }
    return null;
  }

  #setActive(row, col, mode) {
    if (!this.rows.length) return;
    row = Math.max(0, Math.min(row, this.rows.length - 1));
    col = Math.max(0, Math.min(col, this.rows[row].length - 1));
    for (const cells of this.rows) for (const cell of cells) cell.tabIndex = -1;
    const cell = this.rows[row][col];
    cell.tabIndex = 0; // roving: exactly one cell in the grid is in the tab order
    this.active = { row, col };
    if (mode === "focus") {
      cell.focus();
      this.#revealCell(cell);
    }
  }

  /** Bring a cell into the horizontal view without scrollIntoView's vertical surprises. */
  #revealCell(cell) {
    // The row header is sticky: it is always visible, and it has no time of its own.
    if (cell.getAttribute("role") === "rowheader") return;
    const t0 = Number.parseFloat(cell.style.getPropertyValue("--t0")) || 0;
    const dur = Number.parseFloat(cell.style.getPropertyValue("--dur")) || 0;
    const view = this.view;
    const left = timeToX(t0, this.zoom, this.scroll, view);
    const right = timeToX(t0 + dur, this.zoom, this.scroll, view);
    const margin = 24;
    if (left < view.x + margin) {
      this.scroll = clampScroll(t0 - margin / this.zoom, this.zoom, this.duration, view);
    } else if (right > view.x + view.width - margin) {
      const wanted = t0 + dur - (view.width - margin) / this.zoom;
      this.scroll = clampScroll(wanted, this.zoom, this.duration, view);
    }
    this.#drawRuler();
  }

  focusFirstCell() {
    this.#setActive(this.active.row, this.active.col, "focus");
  }

  // --- zoom, playhead, flash ------------------------------------------------------------

  zoomAt(cursorX, factor) {
    const view = this.view;
    const next = zoomAround(cursorX, factor, this.zoom, this.scroll, view);
    this.zoom = next.zoom;
    // One style write. 200 clips re-resolve their transform and width from --pps; this code
    // never touches a single clip element to zoom.
    this.content.style.setProperty("--pps", String(this.zoom));
    this.scroll = clampScroll(next.scroll, this.zoom, this.duration, view);
    this.#drawRuler();
    this.onStatus(`zoom ${this.zoom.toFixed(2)} pixels per second`);
  }

  fitToView() {
    const view = this.view;
    if (!(this.duration > 0)) return;
    this.zoom = Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, (view.width - 8) / this.duration));
    this.content.style.setProperty("--pps", String(this.zoom));
    this.scroll = 0;
    this.#drawRuler();
  }

  setPlayhead(seconds) {
    this.playheadEl.style.setProperty("--t0", String(seconds));
    const view = this.view;
    const x = timeToX(seconds, this.zoom, this.scroll, view);
    if (x < view.x || x > view.x + view.width) {
      this.scroll = clampScroll(seconds - visibleSeconds(this.zoom, view) / 2, this.zoom, this.duration, view);
      this.#drawRuler();
    }
  }

  /**
   * Drive the flash from the ported curve.
   *
   * The paint is CSS (an overlay on `.is-touched`, whose opacity is `--flash-decay`), the
   * timing is this loop, and `@media (prefers-reduced-motion: reduce)` in app.css pins
   * `--flash-decay: 1` so the emphasis becomes a persistent outline instead of a fade --
   * which is also why this loop refuses to start under that preference. One custom
   * property on one element animates every touched clip: N clips, one style write a frame.
   */
  #startFlash() {
    if (this.flashHandle) cancelAnimationFrame(this.flashHandle);
    if (this.reduceMotion.matches) {
      this.content.style.setProperty("--flash-decay", "1");
      return;
    }
    this.flashStart = performance.now();
    const step = () => {
      const elapsed = (performance.now() - this.flashStart) / 1000;
      const decay = flashDecay(elapsed);
      this.content.style.setProperty("--flash-decay", decay.toFixed(3));
      this.flashHandle = decay > 0 ? requestAnimationFrame(step) : 0;
    };
    step();
  }
}

function escapeHtml(value) {
  return String(value).replace(/[&<>"']/g, (c) => ({
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#39;",
  })[c]);
}

// --- the ported test suite ----------------------------------------------------------------
//
// These are the prototype's tests, and each one was mutation-verified over there: dropping
// the scroll term in xToTime, skipping the scroll correction in zoomAround, a wrong label
// target, dropping the gutter rejection, dropping the ruler guard, and a decay curve that
// never reaches zero all fail at least one assertion below. Run them with `?test=1`, or
// from the console as `window.dvsTimelineTests()`.

export function runGeometryTests() {
  const failures = [];
  let count = 0;
  const check = (name, condition, detail) => {
    count += 1;
    if (!condition) failures.push(detail ? `${name}: ${detail}` : name);
  };
  const near = (a, b, tol) => Math.abs(a - b) <= tol;
  const view = { x: 12, width: 900 };

  // 1. round trip, within half a pixel
  for (const zoom of [0.25, 1, 12, 97.3, 400]) {
    for (const scroll of [0, 3.5, 120.12]) {
      for (const seconds of [0, 0.04, 42.5091, 120.12]) {
        const x = timeToX(seconds, zoom, scroll, view);
        const back = xToTime(x, zoom, scroll, view);
        check(
          "round trip",
          near(timeToX(back, zoom, scroll, view), x, 0.5),
          `zoom ${zoom} scroll ${scroll} t ${seconds} -> ${back}`,
        );
      }
    }
  }
  check("left edge is the scroll instant", near(xToTime(view.x, 40, 7.25, view), 7.25, 1e-9));
  check("degenerate zoom yields the scroll instant", xToTime(500, 0, 9, view) === 9);
  check("degenerate zoom has no visible span", visibleSeconds(0, view) === 0);

  // 2. zooming holds the instant under the cursor still
  for (const factor of [1.25, 1 / 1.25, 4, 0.1]) {
    const cursorX = view.x + 640;
    const before = xToTime(cursorX, 12, 30, view);
    const after = zoomAround(cursorX, factor, 12, 30, view);
    check(
      "zoom holds the cursor instant",
      near(xToTime(cursorX, after.zoom, after.scroll, view), before, 1e-6),
      `factor ${factor}`,
    );
  }

  // 3. at the clamp, the view parks instead of walking sideways
  const atMax = zoomAround(view.x + 500, 2, ZOOM_MAX, 42, view);
  check("zoom clamps high", atMax.zoom === ZOOM_MAX);
  check("clamped zoom does not walk the scroll", near(atMax.scroll, 42, 1e-9), `got ${atMax.scroll}`);
  const atMin = zoomAround(view.x + 500, 0.5, ZOOM_MIN, 42, view);
  check("zoom clamps low", atMin.zoom === ZOOM_MIN);
  check("clamped zoom does not walk the scroll (low)", near(atMin.scroll, 42, 1e-9));
  const negative = zoomAround(view.x + 500, 0.5, 100, 0.2, view);
  check("zoomAround does not clamp scroll to zero", negative.scroll < 0, `got ${negative.scroll}`);

  // 4. rows, the ruler band and the gutter
  check("ruler band is not a row", rowAt(0, 3) === null);
  check("ruler band is not a row (edge)", rowAt(RULER_H - 1, 3) === null);
  check("first row starts at the ruler", rowAt(RULER_H, 3) === 0);
  check("first row ends before the gutter", rowAt(RULER_H + ROW_H - 1, 3) === 0);
  check("the gutter selects nothing", rowAt(RULER_H + ROW_H, 3) === null);
  check("the gutter selects nothing (2 px)", rowAt(RULER_H + ROW_H + ROW_GAP - 1, 3) === null);
  check("second row is after the gutter", rowAt(RULER_H + ROW_H + ROW_GAP, 3) === 1);
  check("below the last row is nothing", rowAt(RULER_H + 3 * (ROW_H + ROW_GAP), 3) === null);
  check("above the widget is nothing", rowAt(-5, 3) === null);

  // 5. label ladder: on the ladder, monotonic, and never more than eight labels
  let previous = 0;
  for (let span = 0.1; span < 20000; span *= 1.17) {
    const interval = labelInterval(span);
    check("label interval is monotonic", interval >= previous, `span ${span}`);
    check("label interval keeps the count at or under 8", span / interval <= 8 + 1e-9, `span ${span}`);
    check("label interval is a readable rung", isReadableRung(interval), `got ${interval}`);
    previous = interval;
  }
  check("a 40 second view uses the 5 second rung", labelInterval(40) === 5);
  check("a view past the ladder doubles the top rung", labelInterval(57600) === 7200);

  // 6. flash decay: bounds, monotonic, and actually reaching zero
  check("decay starts bright", flashDecay(0) === 1);
  check("decay before zero is bright", flashDecay(-1) === 1);
  check("decay holds for the first fifth of a second", flashDecay(0.2) > 0.9);
  check("decay ends at zero", flashDecay(FLASH_SECONDS) === 0);
  check("decay past the end is zero", flashDecay(99) === 0);
  check("decay is continuous at the end", flashDecay(1.199) < 0.01, `got ${flashDecay(1.199)}`);
  let last = 1.0001;
  for (let t = 0; t <= FLASH_SECONDS; t += 0.02) {
    const d = flashDecay(t);
    check("decay is monotonic", d <= last, `t ${t}`);
    check("decay stays in range", d >= 0 && d <= 1, `t ${t}`);
    last = d;
  }

  // 7. scroll clamping, with run-out
  check("scroll never goes before zero", clampScroll(-5, 10, 120, view) === 0);
  const runout = clampScroll(1e6, 10, 120, view);
  check("run-out is about half a viewport", near(runout, 120 - 45, 1e-9), `got ${runout}`);
  check("a short project cannot scroll", clampScroll(50, 1, 10, view) === 0);

  return { total: count, failed: failures.length, failures };
}

function isReadableRung(interval) {
  if (LABEL_LADDER.includes(interval)) return true;
  let rung = LABEL_LADDER[LABEL_LADDER.length - 1];
  while (rung < interval) rung *= 2;
  return rung === interval;
}
