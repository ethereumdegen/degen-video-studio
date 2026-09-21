// Resizable panes.
//
// Three separators: the side column against the stage/timeline/console column, the stage
// against the timeline, and the activity feed against lint. Each one is a real
// `role="separator"` with `aria-valuenow`, not a bare `<div>` with a mousedown handler, for
// the same reason everything else in this window is: a separator a keyboard cannot move is
// a control half the users of this application do not have.
//
// Sizes live in CSS custom properties on `<main>` and are read back from the *rendered*
// geometry, never from the stored number. A fraction (`1fr`) is the honest default — it is
// what makes the window look right at any size — so the first drag converts the current
// rendered size into pixels rather than guessing one. The clamping is done here, not with
// `minmax()` alone, because `aria-valuenow` has to agree with what the pane actually did.
//
// Stored in `localStorage`: pane sizes are a habit of the person, not a property of the
// document, so they belong to the window and not to `project.json` — an agent reading the
// document should never see a human's furniture in it.

/** Minimums in CSS pixels. Below these a pane stops being a pane and starts being a sliver. */
const LIMITS = {
  side: { min: 240, max: 720, fallback: 340 },
  stage: { min: 140, max: Infinity, fallback: null },
  activity: { min: 96, max: Infinity, fallback: null },
};

/** Keyboard steps. An arrow is a nudge, a page is a decision. */
const STEP = 16;
const PAGE = 64;

export class Splitters {
  /**
   * @param {object} options
   * @param {HTMLElement} options.main        the grid that owns the custom properties
   * @param {string} options.storageKey       localStorage key; distinct per project
   * @param {(name: string) => void} options.onResize  called after a pane settles
   * @param {(message: string) => void} options.announce  status-line sentence
   */
  constructor({ main, storageKey, onResize, announce }) {
    this.main = main;
    this.storageKey = storageKey;
    this.onResize = onResize || (() => {});
    this.announce = announce || (() => {});
    this.panes = new Map();
    this.dragging = null;
  }

  /**
   * Register one separator.
   *
   * @param {object} pane
   * @param {string} pane.name      key in storage and in the announcement
   * @param {HTMLElement} pane.handle   the `role="separator"` element
   * @param {HTMLElement} pane.owner    the grid whose track is being resized
   * @param {number} pane.track     index of that track in the resolved template
   * @param {HTMLElement} pane.target   the pane itself, used only as a fallback measure
   * @param {string} pane.property  the custom property to write, e.g. `--side-w`
   * @param {"x"|"y"} pane.axis     which way the pointer moves
   * @param {-1|1} pane.sign        +1 when dragging towards larger coordinates grows the
   *                                target (the stage), -1 when it shrinks it (the side)
   * @param {string} pane.label     human name, used in the status line
   */
  add(pane) {
    const entry = { ...pane, saved: null };
    this.panes.set(pane.name, entry);

    pane.handle.addEventListener("pointerdown", (event) => this.onPointerDown(entry, event));
    pane.handle.addEventListener("pointermove", (event) => this.onPointerMove(entry, event));
    pane.handle.addEventListener("pointerup", (event) => this.onPointerUp(entry, event));
    pane.handle.addEventListener("pointercancel", (event) => this.onPointerUp(entry, event));
    pane.handle.addEventListener("keydown", (event) => this.onKeyDown(entry, event));
    pane.handle.addEventListener("dblclick", () => {
      this.reset(entry);
      this.announce(`${entry.label} reset.`);
    });
    return this;
  }

  /** Apply stored sizes, then publish the starting `aria-valuenow` for every separator. */
  restore() {
    const stored = this.read();
    for (const entry of this.panes.values()) {
      const size = stored[entry.name];
      if (typeof size === "number" && Number.isFinite(size)) {
        this.main.style.setProperty(entry.property, `${this.clamp(entry, size)}px`);
      }
    }
    this.publishAll();
    return this;
  }

  /** Re-read geometry into `aria-valuenow`; call after a window resize or a re-render. */
  publishAll() {
    for (const entry of this.panes.values()) this.publish(entry);
  }

  // -- pointer ---------------------------------------------------------------------------

  onPointerDown(entry, event) {
    if (event.button !== 0 || this.isStacked()) return;
    // Capture on the handle so a fast drag that outruns the pointer keeps getting events,
    // and so releasing outside the window still ends the drag.
    entry.handle.setPointerCapture(event.pointerId);
    this.dragging = {
      entry,
      pointerId: event.pointerId,
      origin: entry.axis === "x" ? event.clientX : event.clientY,
      start: this.measure(entry),
    };
    entry.handle.dataset.dragging = "true";
    event.preventDefault();
  }

  onPointerMove(entry, event) {
    const drag = this.dragging;
    if (!drag || drag.entry !== entry || drag.pointerId !== event.pointerId) return;
    const now = entry.axis === "x" ? event.clientX : event.clientY;
    this.resize(entry, drag.start + (now - drag.origin) * entry.sign, { quiet: true });
  }

  onPointerUp(entry, event) {
    const drag = this.dragging;
    if (!drag || drag.entry !== entry) return;
    if (entry.handle.hasPointerCapture(event.pointerId)) {
      entry.handle.releasePointerCapture(event.pointerId);
    }
    delete entry.handle.dataset.dragging;
    this.dragging = null;
    this.save();
    this.onResize(entry.name);
  }

  // -- keyboard --------------------------------------------------------------------------

  onKeyDown(entry, event) {
    if (this.isStacked()) return;
    const horizontal = entry.axis === "x";
    const size = this.measure(entry);
    let next = null;
    switch (event.key) {
      case horizontal ? "ArrowLeft" : "ArrowUp":
        next = size - STEP * entry.sign;
        break;
      case horizontal ? "ArrowRight" : "ArrowDown":
        next = size + STEP * entry.sign;
        break;
      case "PageUp":
        next = size - PAGE * entry.sign;
        break;
      case "PageDown":
        next = size + PAGE * entry.sign;
        break;
      case "Home":
        // The default, not the minimum: a separator you can only slam to one end is a
        // control that cannot undo itself.
        this.reset(entry);
        this.announce(`${entry.label} reset.`);
        event.preventDefault();
        return;
      case "End":
        next = this.bounds(entry).max;
        break;
      default:
        return;
    }
    // `preventDefault` is also what keeps the window's global key handler off this event:
    // it reads the same arrows as "step one frame" and Home/End as "first/last frame", and
    // it skips anything already handled. Moving a separator must not seek the playhead.
    event.preventDefault();
    this.resize(entry, next);
    this.save();
    this.onResize(entry.name);
  }

  // -- geometry --------------------------------------------------------------------------

  /** The pane's current size in pixels, measured rather than remembered.
   *
   * This reads the *resolved grid track*, not the pane's border box, and the difference is
   * not cosmetic: a pane with a margin measures smaller than its track, so writing that
   * number back as the track size shrinks the pane a little on every drag — a slow, mystifying
   * drift. `getComputedStyle().gridTemplateColumns` returns used pixel values, which is
   * exactly the number the custom property is about to be set to. */
  measure(entry) {
    const style = window.getComputedStyle(entry.owner);
    const template = entry.axis === "x" ? style.gridTemplateColumns : style.gridTemplateRows;
    const track = template.split(" ")[entry.track];
    const size = Number.parseFloat(track);
    if (Number.isFinite(size)) return Math.round(size);
    // `none` (a display:none grid, or a browser that reports the author value) — fall back
    // to the box, which is right whenever there are no margins in play.
    const box = entry.target.getBoundingClientRect();
    return Math.round(entry.axis === "x" ? box.width : box.height);
  }

  /** Hard limits for this pane, given how much room the window has right now. */
  bounds(entry) {
    const limit = LIMITS[entry.name] || { min: 100, max: Infinity };
    const box = entry.owner.getBoundingClientRect();
    const available = entry.axis === "x" ? box.width : box.height;
    // Leave the other side of the separator at least as much room as this one needs; the
    // alternative is a drag that pushes the timeline or the viewport out of existence.
    const ceiling = Math.max(limit.min, Math.min(limit.max, available - limit.min * 2));
    return { min: limit.min, max: ceiling };
  }

  clamp(entry, size) {
    const { min, max } = this.bounds(entry);
    return Math.round(Math.min(max, Math.max(min, size)));
  }

  resize(entry, size, { quiet = false } = {}) {
    const next = this.clamp(entry, size);
    this.main.style.setProperty(entry.property, `${next}px`);
    this.publish(entry);
    if (!quiet) this.announce(`${entry.label} ${next} pixels.`);
    return next;
  }

  reset(entry) {
    this.main.style.removeProperty(entry.property);
    const stored = this.read();
    delete stored[entry.name];
    this.write(stored);
    this.publish(entry);
    this.onResize(entry.name);
  }

  /** Tell assistive technology where the separator now sits.
   *
   * The scale is CSS pixels, not a percentage of travel. A percentage would have to be
   * relative to `bounds()`, which moves with the window, so the same pane would announce
   * two different numbers at two window sizes without anything having been dragged. Pixels
   * are stable, and `aria-valuemin`/`aria-valuemax` carry the room that is left. */
  publish(entry) {
    const { min, max } = this.bounds(entry);
    const size = this.measure(entry);
    entry.handle.setAttribute("aria-valuenow", String(size));
    entry.handle.setAttribute("aria-valuemin", String(Math.round(min)));
    entry.handle.setAttribute("aria-valuemax", String(Math.round(max)));
    entry.handle.setAttribute("aria-valuetext", `${entry.label} ${size} pixels`);
  }

  /** Below the stacking breakpoint the grid is one column and separators do nothing. */
  isStacked() {
    return window.matchMedia("(max-width: 900px)").matches;
  }

  // -- persistence -----------------------------------------------------------------------

  read() {
    try {
      const raw = window.localStorage.getItem(this.storageKey);
      const parsed = raw ? JSON.parse(raw) : {};
      return parsed && typeof parsed === "object" ? parsed : {};
    } catch {
      // A private-mode webview, a corrupt value, a quota error: layout is a convenience and
      // must never be the reason the window fails to start.
      return {};
    }
  }

  write(value) {
    try {
      window.localStorage.setItem(this.storageKey, JSON.stringify(value));
    } catch {
      /* see read() */
    }
  }

  save() {
    const value = this.read();
    for (const entry of this.panes.values()) {
      const explicit = this.main.style.getPropertyValue(entry.property);
      if (explicit) value[entry.name] = Math.round(Number.parseFloat(explicit));
    }
    this.write(value);
  }
}

/**
 * Wire the three separators this window has.
 *
 * @param {object} options
 * @param {(name: string) => void} options.onResize
 * @param {(message: string) => void} options.announce
 */
export function installSplitters({ onResize, announce }) {
  const main = document.querySelector("main");
  if (!main) return null;
  const handle = (id) => document.getElementById(id);
  const target = (selector) => main.querySelector(selector);
  const side = handle("split-side");
  const stage = handle("split-stage");
  const activity = handle("split-activity");
  if (!side || !stage || !activity) return null;

  const splitters = new Splitters({
    main,
    storageKey: "dvs-studio:layout",
    onResize,
    announce,
  });
  const sidePane = target(".side");
  // Track indices follow grid-template-* in app.css:
  //   main   columns: [0] stage/timeline/console, [1] gutter, [2] side
  //   main   rows:    [0] stage, [1] gutter, [2] timeline, [3] console
  //   .side  rows:    [0] activity, [1] gutter, [2] lint
  splitters
    .add({
      name: "side",
      handle: side,
      owner: main,
      track: 2,
      target: sidePane,
      property: "--side-w",
      axis: "x",
      // Dragging right shrinks the side column, which lives on the right.
      sign: -1,
      label: "Side panel width",
    })
    .add({
      name: "stage",
      handle: stage,
      owner: main,
      track: 0,
      target: target(".stage"),
      property: "--stage-h",
      axis: "y",
      sign: 1,
      label: "Viewport height",
    })
    .add({
      name: "activity",
      handle: activity,
      owner: sidePane,
      track: 0,
      target: target(".activity"),
      property: "--activity-h",
      axis: "y",
      sign: 1,
      label: "Activity feed height",
    })
    .restore();

  // `aria-valuemax` is "how much room is left", which changes whenever anything reflows —
  // a window resize, a longer lint list, the timeline growing a track. Observing the grid
  // itself catches all of those; a `resize` listener catches only the first.
  if (typeof ResizeObserver === "function") {
    let queued = false;
    const observer = new ResizeObserver(() => {
      // Coalesce to one publish per frame: an observer that writes attributes can otherwise
      // feed itself.
      if (queued) return;
      queued = true;
      requestAnimationFrame(() => {
        queued = false;
        splitters.publishAll();
      });
    });
    observer.observe(main);
  } else {
    window.addEventListener("resize", () => splitters.publishAll());
  }
  return splitters;
}
