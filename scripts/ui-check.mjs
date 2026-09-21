// End-to-end checks for the studio window's frontend, run against a real headless Chromium.
//
// The window's JavaScript has no unit-test harness on purpose: almost every interesting
// behaviour here is a browser behaviour — a pointer drag that must not drift, a key that
// must not reach two handlers, a size that must survive a reload. Asserting those against a
// mocked DOM would test the mock. So this drives the actual page, served statically with
// its development bridge, which is the same code path the axe audit uses.
//
//   node scripts/ui-check.mjs [http://127.0.0.1:8173]
//
// Requires puppeteer (`npm install --no-save puppeteer`). Exits non-zero on the first
// failed assertion, with the measured numbers.

import puppeteer from "puppeteer";

const base = process.argv[2] || "http://127.0.0.1:8173";
const failures = [];
let checks = 0;

function check(name, condition, detail) {
  checks += 1;
  if (!condition) failures.push(detail ? `${name} — ${detail}` : name);
}

/** Resolved grid tracks, the numbers the separators actually write. */
const LAYOUT = `(() => {
  const main = getComputedStyle(document.querySelector('main'));
  const side = getComputedStyle(document.querySelector('.side'));
  const px = (template, i) => Math.round(Number.parseFloat(template.split(' ')[i]));
  return {
    sideWidth: px(main.gridTemplateColumns, 2),
    stageHeight: px(main.gridTemplateRows, 0),
    activityHeight: px(side.gridTemplateRows, 0),
    frame: document.getElementById('frame-text').textContent.trim(),
    status: document.getElementById('status').textContent.trim(),
    stored: localStorage.getItem('dvs-studio:layout'),
    separators: [...document.querySelectorAll('[role=separator]')].map((s) => ({
      id: s.id,
      now: Number(s.getAttribute('aria-valuenow')),
      min: Number(s.getAttribute('aria-valuemin')),
      max: Number(s.getAttribute('aria-valuemax')),
      text: s.getAttribute('aria-valuetext'),
      focusable: s.tabIndex >= 0,
    })),
  };
})()`;

const browser = await puppeteer.launch({
  args: ["--no-sandbox", "--disable-dev-shm-usage"],
});

try {
  const page = await browser.newPage();
  await page.setViewport({ width: 1600, height: 1000 });
  await page.goto(`${base}/index.html`, { waitUntil: "networkidle0" });
  await page.evaluate(() => window.localStorage.removeItem("dvs-studio:layout"));
  await page.reload({ waitUntil: "networkidle0" });
  // The fixture bridge fetches its snapshot; the timeline is drawn from it.
  await page.waitForSelector("[role=gridcell]", { timeout: 10_000 });

  const start = await page.evaluate(LAYOUT);
  check(
    "three separators, all reachable by keyboard",
    start.separators.length === 3 && start.separators.every((s) => s.focusable),
    JSON.stringify(start.separators.map((s) => [s.id, s.focusable])),
  );
  check(
    "each separator publishes its size and its room",
    start.separators.every((s) => s.now >= s.min && s.now <= s.max && /pixels$/.test(s.text || "")),
    JSON.stringify(start.separators),
  );

  // 1. A pointer drag moves the pane by exactly the distance dragged.
  const handle = await page.evaluate(`(() => {
    const r = document.getElementById('split-side').getBoundingClientRect();
    return { x: Math.round(r.x + r.width / 2), y: Math.round(r.y + r.height / 2) };
  })()`);
  await page.mouse.move(handle.x, handle.y);
  await page.mouse.down();
  await page.mouse.move(handle.x - 60, handle.y, { steps: 6 });
  await page.mouse.move(handle.x - 140, handle.y, { steps: 6 });
  await page.mouse.up();
  const dragged = await page.evaluate(LAYOUT);
  check(
    "dragging the side separator 140px widens the side column by 140px",
    dragged.sideWidth === start.sideWidth + 140,
    `${start.sideWidth} -> ${dragged.sideWidth}`,
  );

  // 2. Dragging and releasing repeatedly must not drift: the pane measures its own grid
  //    track, not its border box, and a pane with a margin would shrink a little each time.
  for (let i = 0; i < 3; i += 1) {
    const h = await page.evaluate(`(() => {
      const r = document.getElementById('split-side').getBoundingClientRect();
      return { x: Math.round(r.x + r.width / 2), y: Math.round(r.y + r.height / 2) };
    })()`);
    await page.mouse.move(h.x, h.y);
    await page.mouse.down();
    await page.mouse.move(h.x, h.y, { steps: 2 });
    await page.mouse.up();
  }
  const settled = await page.evaluate(LAYOUT);
  check(
    "three zero-distance drags leave the pane where it was",
    settled.sideWidth === dragged.sideWidth,
    `${dragged.sideWidth} -> ${settled.sideWidth}`,
  );

  // 3. Keyboard: arrows move the separator and must not reach the transport, which reads
  //    the same keys as "step one frame" and "first/last frame".
  await page.evaluate(`document.getElementById('split-side').focus()`);
  await page.keyboard.press("ArrowLeft");
  await page.keyboard.press("ArrowLeft");
  await page.keyboard.press("PageDown");
  const typed = await page.evaluate(LAYOUT);
  check(
    "two arrows and a page move the separator by 16, 16 and 64 pixels",
    typed.sideWidth === settled.sideWidth + 16 + 16 - 64,
    `${settled.sideWidth} -> ${typed.sideWidth}`,
  );
  check("separator keys do not seek the playhead", typed.frame === start.frame, typed.frame);
  check(
    "the separator says what it did",
    /Side panel width \d+ pixels/.test(typed.status),
    typed.status,
  );

  await page.keyboard.press("End");
  const maxed = await page.evaluate(LAYOUT);
  const sideSep = maxed.separators.find((s) => s.id === "split-side");
  check("End goes to the maximum the window allows", maxed.sideWidth === sideSep.max, `${maxed.sideWidth} vs ${sideSep.max}`);
  check("the playhead still has not moved", maxed.frame === start.frame, maxed.frame);

  // 4. Sizes survive a reload; Home puts them back and forgets them.
  await page.reload({ waitUntil: "networkidle0" });
  await page.waitForSelector("[role=gridcell]", { timeout: 10_000 });
  const reloaded = await page.evaluate(LAYOUT);
  check(
    "pane sizes survive a reload",
    reloaded.sideWidth === maxed.sideWidth,
    `${maxed.sideWidth} -> ${reloaded.sideWidth}`,
  );

  await page.evaluate(`document.getElementById('split-side').focus()`);
  await page.keyboard.press("Home");
  const reset = await page.evaluate(LAYOUT);
  check("Home restores the default width", reset.sideWidth === start.sideWidth, `${reset.sideWidth} vs ${start.sideWidth}`);
  check("and forgets the stored size", !JSON.parse(reset.stored || "{}").side, reset.stored);

  // 5. The other two separators move their own panes, and only their own.
  await page.evaluate(`document.getElementById('split-stage').focus()`);
  await page.keyboard.press("ArrowUp");
  await page.keyboard.press("PageUp");
  const stage = await page.evaluate(LAYOUT);
  check(
    "the stage separator shrinks the viewport by 16 + 64 pixels",
    stage.stageHeight === reset.stageHeight - 80,
    `${reset.stageHeight} -> ${stage.stageHeight}`,
  );
  check("and leaves the side column alone", stage.sideWidth === reset.sideWidth, `${stage.sideWidth}`);

  await page.evaluate(`document.getElementById('split-activity').focus()`);
  await page.keyboard.press("ArrowDown");
  const activity = await page.evaluate(LAYOUT);
  check(
    "the activity separator grows the feed by 16 pixels",
    activity.activityHeight === stage.activityHeight + 16,
    `${stage.activityHeight} -> ${activity.activityHeight}`,
  );

  // 6. Below the stacking breakpoint there is nothing to divide, and a separator that
  //    cannot move must not be in the tab order or the accessibility tree.
  await page.setViewport({ width: 820, height: 900 });
  await new Promise((resolve) => setTimeout(resolve, 200));
  const stacked = await page.evaluate(`(() => {
    const seps = [...document.querySelectorAll('[role=separator]')];
    return seps.map((s) => getComputedStyle(s).display);
  })()`);
  check("stacked layout hides the separators", stacked.every((d) => d === "none"), stacked.join(","));
} finally {
  await browser.close();
}

if (failures.length) {
  console.error(`ui-check: ${failures.length} of ${checks} checks failed`);
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log(`ui-check: ${checks} checks passed`);
