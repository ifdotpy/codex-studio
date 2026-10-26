import type { Rectangle, Display, BrowserWindow, App } from "electron";
export type SavedWindowState = {
  version?: number;
  bounds?: Rectangle;
  maximized?: boolean;
};
import * as fs from "node:fs";
import * as path from "node:path";
import { atomicJSON } from "./recovery.ts";

function rectangle(value: unknown): value is Rectangle {
  if (!value || typeof value !== "object") return false;
  const fields = value as Record<string, unknown>;
  return (
    value &&
    ["x", "y", "width", "height"].every((key) =>
      Number.isSafeInteger(fields[key]),
    ) &&
    typeof fields.width === "number" &&
    fields.width > 0 &&
    typeof fields.height === "number" &&
    fields.height > 0
  );
}
function windowState(
  saved: SavedWindowState | undefined,
  displays: Pick<Display, "workArea">[],
) {
  const areas = displays.map((display) => display.workArea).filter(rectangle);
  const first = areas[0];
  if (!first) throw new Error("No display work area is available.");
  const valid = saved?.version === 1 && rectangle(saved.bounds);
  const bounds =
    valid && saved?.bounds
      ? saved.bounds
      : { x: first.x, y: first.y, width: 1440, height: 960 };
  const overlap = (area: Rectangle) =>
    Math.max(
      0,
      Math.min(area.x + area.width, bounds.x + bounds.width) -
        Math.max(area.x, bounds.x),
    ) *
    Math.max(
      0,
      Math.min(area.y + area.height, bounds.y + bounds.height) -
        Math.max(area.y, bounds.y),
    );
  const distance = (area: Rectangle) => {
    const x = bounds.x + bounds.width / 2,
      y = bounds.y + bounds.height / 2;
    return (
      (x - Math.max(area.x, Math.min(x, area.x + area.width))) ** 2 +
      (y - Math.max(area.y, Math.min(y, area.y + area.height))) ** 2
    );
  };
  const area = valid
    ? ([...areas].sort(
        (a, b) => overlap(b) - overlap(a) || distance(a) - distance(b),
      )[0] ?? first)
    : first;
  const minWidth = Math.min(420, area.width),
    minHeight = Math.min(600, area.height);
  const width = Math.max(minWidth, Math.min(bounds.width, area.width));
  const height = Math.max(minHeight, Math.min(bounds.height, area.height));
  return {
    bounds: {
      x: valid
        ? Math.max(area.x, Math.min(bounds.x, area.x + area.width - width))
        : area.x + Math.floor((area.width - width) / 2),
      y: valid
        ? Math.max(area.y, Math.min(bounds.y, area.y + area.height - height))
        : area.y + Math.floor((area.height - height) / 2),
      width,
      height,
    },
    minWidth,
    minHeight,
    maximized: valid && saved.maximized === true,
  };
}
function loadWindowState(
  profile: string,
  displays: Pick<Display, "workArea">[],
) {
  let saved: SavedWindowState | undefined;
  try {
    saved = JSON.parse(
      fs.readFileSync(path.join(profile, "window-state.json"), "utf8"),
    );
  } catch (caughterror) {
    if (!(caughterror instanceof Error)) throw caughterror;
    const error: NodeJS.ErrnoException = caughterror;
    if (error.code !== "ENOENT")
      console.error("Cannot read saved window bounds:", error.message);
  }
  return windowState(saved, displays);
}
function trackWindowState(
  win: Pick<
    BrowserWindow,
    | "isDestroyed"
    | "isFullScreen"
    | "isMaximized"
    | "getNormalBounds"
    | "on"
    | "maximize"
  >,
  app: Pick<App, "on" | "removeListener">,
  profile: string,
  initial: ReturnType<typeof windowState>,
  { hidden = false } = {},
) {
  const filename = path.join(profile, "window-state.json");
  let timer: ReturnType<typeof setTimeout> | undefined;
  let maximized = initial.maximized;
  const save = () => {
    clearTimeout(timer);
    timer = undefined;
    if (win.isDestroyed()) return;
    if (!win.isFullScreen() && !hidden) maximized = win.isMaximized();
    const bounds = win.getNormalBounds();
    if (!rectangle(bounds)) return;
    try {
      atomicJSON(filename, { version: 1, bounds, maximized });
    } catch (caughterror) {
      if (!(caughterror instanceof Error)) throw caughterror;
      const error: NodeJS.ErrnoException = caughterror;
      console.error("Cannot save window bounds:", error.message);
    }
  };
  const schedule = () => {
    if (!timer) timer = setTimeout(save, 100);
  };
  win.on("move", schedule);
  win.on("resize", schedule);
  win.on("maximize", schedule);
  win.on("unmaximize", schedule);
  win.on("leave-full-screen", schedule);
  win.on("close", save);
  app.on("before-quit", save);
  win.on("closed", () => {
    clearTimeout(timer);
    app.removeListener("before-quit", save);
  });
  return {
    save,
    restore: () => {
      // maximize() shows an existing window without focus. Never call it in hidden tests.
      if (initial.maximized && !hidden) win.maximize();
    },
  };
}
export { windowState, loadWindowState, trackWindowState };
