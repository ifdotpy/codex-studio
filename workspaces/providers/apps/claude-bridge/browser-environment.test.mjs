import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { afterEach, test } from "vitest";
import { browserEnvironment } from "./browser-environment.mjs";

const roots = [];
afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true });
});

function root(value) {
  const folder = mkdtempSync(path.join(tmpdir(), "studio-browser-env-"));
  roots.push(folder);
  if (value !== undefined)
    writeFileSync(path.join(folder, "studio-browser.json"), value);
  return folder;
}

test("a session gets the launcher, its socket, its own session and the links first on PATH", () => {
  const folder = root(
    JSON.stringify({
      BROWSER: "/state/browser-host/studio-browser",
      STUDIO_BROWSER_SOCKET: "/state/browser-host/open.sock",
      STUDIO_BROWSER_PATH: "/state/browser-host/bin",
    }),
  );
  assert.deepEqual(
    browserEnvironment(folder, "session-1", {
      PATH: "/usr/bin:/state/browser-host/bin:/bin",
    }),
    {
      BROWSER: "/state/browser-host/studio-browser",
      STUDIO_BROWSER_SOCKET: "/state/browser-host/open.sock",
      STUDIO_BROWSER_SESSION: "session-1",
      PATH: "/state/browser-host/bin:/usr/bin:/bin",
    },
  );
});

test("an older server file without the link folder keeps PATH unchanged", () => {
  const folder = root(
    JSON.stringify({ BROWSER: "/b", STUDIO_BROWSER_SOCKET: "/s" }),
  );
  assert.deepEqual(browserEnvironment(folder, "s", { PATH: "/usr/bin" }), {
    BROWSER: "/b",
    STUDIO_BROWSER_SOCKET: "/s",
    STUDIO_BROWSER_SESSION: "s",
  });
});

test("a missing, invalid or relative file gives no browser environment", () => {
  assert.deepEqual(browserEnvironment(root(), "s"), {});
  assert.deepEqual(browserEnvironment(root("{"), "s"), {});
  assert.deepEqual(
    browserEnvironment(
      root(
        JSON.stringify({
          BROWSER: "/b",
          STUDIO_BROWSER_SOCKET: "/s",
          STUDIO_BROWSER_PATH: "bin",
        }),
      ),
      "s",
      { PATH: "/usr/bin" },
    ).PATH,
    undefined,
  );
});
