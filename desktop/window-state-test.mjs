import assert from "node:assert/strict";
import { test } from "node:test";
import { EventEmitter } from "node:events";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { createRequire } from "node:module";
const { windowState, loadWindowState, trackWindowState } = createRequire(
  import.meta.url,
)("./window-state.cjs");
const display = (x = 0, y = 25, width = 1600, height = 1000) => ({
  workArea: { x, y, width, height },
});

test("valid bounds and maximized state survive on the same monitor", () => {
  const saved = {
    version: 1,
    bounds: { x: 80, y: 60, width: 1100, height: 800 },
    maximized: true,
  };
  const result = windowState(saved, [display()]);
  assert.deepEqual(result.bounds, saved.bounds);
  assert.equal(result.maximized, true);
});
test("an absent monitor and a smaller work area cannot leave the window off screen", () => {
  const result = windowState(
    {
      version: 1,
      bounds: { x: 1800, y: -200, width: 1400, height: 1000 },
      maximized: true,
    },
    [display(-1000, 50, 800, 500)],
  );
  assert.deepEqual(result.bounds, { x: -1000, y: 50, width: 800, height: 500 });
  assert.equal(result.minHeight, 500);
  assert.equal(result.maximized, true);
});
test("an attached secondary monitor retains its window", () => {
  const saved = {
    version: 1,
    bounds: { x: -1200, y: 50, width: 900, height: 700 },
  };
  assert.deepEqual(
    windowState(saved, [display(), display(-1400, 25, 1400, 900)]).bounds,
    saved.bounds,
  );
});
test("invalid coordinates and unversioned data use a visible default", () => {
  for (const saved of [
    { bounds: { x: 1, y: 2, width: 900, height: 700 } },
    { version: 1, bounds: { x: Infinity, y: 2, width: 900, height: 700 } },
  ]) {
    const result = windowState(saved, [display(0, 25, 1200, 800)]);
    assert.deepEqual(result.bounds, { x: 0, y: 25, width: 1200, height: 800 });
    assert.equal(result.maximized, false);
  }
});
test("close flushes bounds synchronously; fullscreen is not restored; hidden mode never shows", () => {
  const profile = mkdtempSync(path.join(tmpdir(), "studio-window-state-"));
  try {
    const app = new EventEmitter();
    const win = new EventEmitter();
    let bounds = { x: 30, y: 40, width: 1200, height: 850 };
    win.isDestroyed = () => false;
    win.isFullScreen = () => true;
    win.isMaximized = () => false;
    win.getNormalBounds = () => bounds;
    win.maximize = () => {
      throw new Error("Hidden windows must stay hidden");
    };
    const tracker = trackWindowState(
      win,
      app,
      profile,
      { maximized: true },
      { hidden: true },
    );
    tracker.restore();
    win.emit("resize");
    bounds = { x: 90, y: 70, width: 1000, height: 700 };
    app.emit("before-quit");
    const saved = JSON.parse(
      readFileSync(path.join(profile, "window-state.json")),
    );
    assert.deepEqual(saved.bounds, bounds);
    assert.equal(saved.maximized, true);
    assert.equal(saved.fullscreen, undefined);
    assert.deepEqual(loadWindowState(profile, [display()]).bounds, bounds);
    win.emit("closed");
  } finally {
    rmSync(profile, { recursive: true, force: true });
  }
});
