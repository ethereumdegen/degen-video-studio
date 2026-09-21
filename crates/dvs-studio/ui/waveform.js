// Waveforms: the shape of the sound, inside the clip that carries it.
//
// ---------------------------------------------------------------------------------------
// Why a <canvas> in a window whose whole argument is that it is not a canvas
// ---------------------------------------------------------------------------------------
// Because a waveform is *decorative*. It tells a sighted editor where the dialogue is at a
// glance, and it tells a screen reader nothing that a number cannot tell it better. The
// two honest options were a canvas or a couple of thousand <div>s per clip; the second
// would put thousands of meaningless nodes in the accessibility tree and make arrowing
// through the timeline unusable. So the drawing is one `aria-hidden` canvas per clip, and
// the *textual equivalent* -- the clip's peak in dBFS -- is appended to the clip's
// accessible name, where a reader already looks. A track's own peak is appended to its
// row header for the same reason. Nothing here is the only carrier of anything.
//
// The canvas is never focusable, never hit-testable and never the source of a name: if
// every canvas in this file failed to draw, the window would lose no information.
//
// ---------------------------------------------------------------------------------------
// Caching, and why zoom must not touch the engine
// ---------------------------------------------------------------------------------------
// Peaks are per track over the *whole sequence*, so they do not depend on the zoom: a
// zoom changes which buckets land in which pixel column, not the buckets. They are cached
// here per revision (and in Rust per revision and bucket count), because a wheel handler
// that does an IPC round trip per notch is a wheel handler that stutters. A redraw is
// driven by a ResizeObserver on the clip cells instead -- a zoom is one `--pps` write in
// timeline.js and every cell changes width, which is precisely the signal we need and the
// reason this file does not have to know that zooming exists.
//
// Buckets are min/max pairs over all channels, and they ignore mute and solo: the waveform
// is the shape of the material, not of the monitor path, so soloing A2 does not blank A1.

import { parseRational } from "./timeline.js";

/** Matches dvs_audio::SILENCE_FLOOR_DB. Peaks are always finite -- JSON has no -inf, and a
 *  null in a numeric comparison is a silent wrong answer -- so silence arrives as this. */
const SILENCE_FLOOR_DB = -100;

/** Backing-store ceiling. A 120-second clip at the 400 px/s zoom limit is 48000 CSS pixels
 *  wide; a canvas that size is ~6 MB of pixels per clip for detail the peaks do not even
 *  contain. Past this the drawing is upscaled, which is honest about its resolution. */
const MAX_CANVAS_PX = 2048;

/** 2048 buckets over the sequence: ~17 per second on a two-minute edit, which is finer
 *  than a pixel column at any zoom a person uses, and one small array per track. */
export const DEFAULT_BUCKETS = 2048;

export class Waveforms {
  #invoke;
  #onError;
  #grid;
  #buckets;
  #cache = new Map(); // revision -> Promise<TrackPeaks[]>
  #items = []; // { cell, canvas, entry, start, end }
  #dirty = new Set();
  #frame = 0;
  #duration = 1;
  #ink = "rgba(7, 8, 10, 0.66)";
  #observer;

  /**
   * @param {object} opts
   * @param {HTMLElement} opts.grid   the timeline's role="grid", whose cells own the canvases
   * @param {(command: string, args?: object) => Promise<any>} opts.invoke
   * @param {(message: string) => void} [opts.onError]  said out loud, not swallowed
   * @param {number} [opts.buckets]
   */
  constructor(opts) {
    this.#grid = opts.grid;
    this.#invoke = opts.invoke;
    this.#onError = opts.onError || (() => {});
    this.#buckets = opts.buckets || DEFAULT_BUCKETS;

    this.#observer = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const item = this.#items.find((candidate) => candidate.cell === entry.target);
        if (item) this.#dirty.add(item);
      }
      this.#schedule();
    });

    // A theme flip changes the ink the waveform is drawn in, and a canvas does not
    // re-resolve a custom property the way a stylesheet does.
    window.matchMedia("(prefers-color-scheme: light)").addEventListener("change", () => this.redraw());
  }

  /**
   * Fetch (or reuse) the peaks for this revision and hang a canvas inside every clip whose
   * track carries audio. Call it after every `TimelineView.render`, which rebuilds the
   * cells from scratch: the canvases and the name suffixes go with them.
   */
  async attach(snapshot) {
    this.#items = [];
    this.#dirty.clear();
    this.#observer.disconnect();
    this.#duration = parseRational(snapshot.timeline.duration || snapshot.duration) || 1;

    let peaks;
    try {
      peaks = await this.#peaksFor(snapshot.revision);
    } catch (error) {
      // No waveforms and no peak in any name, and the window says so rather than looking
      // like a sequence of silent clips.
      this.#onError(`No waveforms: ${error.message || error}`);
      return;
    }

    const byTrack = new Map();
    for (const entry of peaks) byTrack.set(entry.track, entry);

    for (const row of snapshot.timeline.rows) {
      const entry = byTrack.get(row.id);
      if (!entry || !entry.buckets.length) continue;
      const head = this.#cell(`row:${row.id}`);
      if (head) this.#appendSummary(head, dbSuffix(entry.peakDb));
    }

    for (const clip of snapshot.timeline.clips) {
      const entry = byTrack.get(clip.track);
      if (!entry || !entry.buckets.length) continue;
      const cell = this.#cell(clip.id);
      if (!cell) continue;

      const start = parseRational(clip.start);
      const end = parseRational(clip.end);
      this.#appendSummary(cell, dbSuffix(peakDbBetween(entry, start, end, this.#duration)));

      const canvas = document.createElement("canvas");
      canvas.className = "tl-wave";
      // Decoration, and the accessible name already carries the number it encodes.
      canvas.setAttribute("aria-hidden", "true");
      cell.prepend(canvas);

      const item = { cell, canvas, entry, start, end };
      this.#items.push(item);
      this.#dirty.add(item);
      // Fires once on observe, which is also the first draw.
      this.#observer.observe(cell);
    }
    this.#schedule();
  }

  /** Redraw every canvas: a theme change, or a caller that moved the cells. */
  redraw() {
    for (const item of this.#items) this.#dirty.add(item);
    this.#schedule();
  }

  /** Peak of the whole track, in dBFS, or null if the track carries no audio. */
  async trackPeak(revision, trackId) {
    const peaks = await this.#peaksFor(revision);
    const entry = peaks.find((candidate) => candidate.track === trackId);
    return entry && entry.buckets.length ? entry.peakDb : null;
  }

  #cell(id) {
    return this.#grid.querySelector(`[data-cell-id="${CSS.escape(id)}"]`);
  }

  /** Append to the sentence Rust composed rather than recomposing it, and only once: the
   *  cells are new after every render, so the marker on the cell is the guard. */
  #appendSummary(cell, suffix) {
    if (cell.dataset.wavePeak === suffix) return;
    cell.dataset.wavePeak = suffix;
    cell.setAttribute("aria-label", `${cell.getAttribute("aria-label") || ""}${suffix}`);
  }

  #peaksFor(revision) {
    const hit = this.#cache.get(revision);
    if (hit) return hit;
    const pending = this.#invoke("peaks", { buckets: this.#buckets }).then((rows) =>
      (Array.isArray(rows) ? rows : []).map((row) => ({
        track: row.track,
        name: row.name,
        buckets: Array.isArray(row.buckets) ? row.buckets : [],
        peakDb: Number.isFinite(row.peakDb) ? row.peakDb : SILENCE_FLOOR_DB,
      })),
    );
    this.#cache.set(revision, pending);
    // A failure is not a cacheable answer: the next render asks again.
    pending.catch(() => this.#cache.delete(revision));
    while (this.#cache.size > 4) this.#cache.delete(this.#cache.keys().next().value);
    return pending;
  }

  #schedule() {
    if (this.#frame || !this.#dirty.size) return;
    this.#frame = requestAnimationFrame(() => {
      this.#frame = 0;
      this.#ink =
        getComputedStyle(document.documentElement).getPropertyValue("--wave-ink").trim() || this.#ink;
      const batch = [...this.#dirty];
      this.#dirty.clear();
      for (const item of batch) this.#draw(item);
    });
  }

  /**
   * One column of pixels per column of pixels, and each column is the min/max of *every*
   * bucket it spans. Sampling one bucket per column instead would alias the peaks away at
   * low zoom -- the waveform would grow and shrink as you zoomed, which is the one thing a
   * level display must not do.
   */
  #draw(item) {
    const { canvas, entry, start, end } = item;
    const cssW = canvas.clientWidth;
    const cssH = canvas.clientHeight;
    if (!(cssW >= 2 && cssH >= 4)) {
      // Narrower than a line has nothing to say; the name still carries the peak.
      if (canvas.width) canvas.width = 0;
      return;
    }
    const dpr = Math.min(3, window.devicePixelRatio || 1);
    const w = Math.max(1, Math.min(MAX_CANVAS_PX, Math.round(cssW * dpr)));
    const h = Math.max(1, Math.round(cssH * dpr));
    if (canvas.width !== w) canvas.width = w;
    if (canvas.height !== h) canvas.height = h;

    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.clearRect(0, 0, w, h);
    ctx.fillStyle = this.#ink;

    const buckets = entry.buckets;
    const count = buckets.length;
    const span = Math.max(1e-9, end - start);
    const mid = h / 2;
    const reach = mid * 0.92; // leave a hair of margin so a full-scale peak is not clipped

    for (let x = 0; x < w; x += 1) {
      const t0 = start + (x / w) * span;
      const t1 = start + ((x + 1) / w) * span;
      let first = Math.floor((t0 / this.#duration) * count);
      let last = Math.ceil((t1 / this.#duration) * count);
      first = Math.max(0, Math.min(count - 1, first));
      last = Math.max(first + 1, Math.min(count, last));
      let lo = 0;
      let hi = 0;
      for (let b = first; b < last; b += 1) {
        const pair = buckets[b];
        if (pair[0] < lo) lo = pair[0];
        if (pair[1] > hi) hi = pair[1];
      }
      const top = mid - hi * reach;
      const bottom = mid - lo * reach;
      // Minimum one pixel: silence draws a centre line, not a hole.
      ctx.fillRect(x, top, 1, Math.max(1, bottom - top));
    }
  }
}

/** Loudest sample in a clip's own span, in dBFS -- the track's peak would say "this album
 *  is loud somewhere", which is not what the clip's name is for. */
export function peakDbBetween(entry, start, end, duration) {
  const count = entry.buckets.length;
  if (!count || !(duration > 0)) return SILENCE_FLOOR_DB;
  let first = Math.floor((start / duration) * count);
  let last = Math.ceil((end / duration) * count);
  first = Math.max(0, Math.min(count - 1, first));
  last = Math.max(first + 1, Math.min(count, last));
  let loudest = 0;
  for (let b = first; b < last; b += 1) {
    const pair = entry.buckets[b];
    const level = Math.max(Math.abs(pair[0]), Math.abs(pair[1]));
    if (level > loudest) loudest = level;
  }
  return loudest > 0 ? Math.max(SILENCE_FLOOR_DB, 20 * Math.log10(loudest)) : SILENCE_FLOOR_DB;
}

/** ", peak −3.2 dBFS". U+2212 MINUS SIGN, not a hyphen: a reader says "minus three point
 *  two" for the first and "dash" or nothing at all for the second. */
export function dbSuffix(db) {
  if (!Number.isFinite(db) || db <= SILENCE_FLOOR_DB) return ", silent";
  const rounded = Math.round(db * 10) / 10;
  const text = rounded < 0 ? `\u2212${Math.abs(rounded).toFixed(1)}` : rounded.toFixed(1);
  return `, peak ${text} dBFS`;
}
