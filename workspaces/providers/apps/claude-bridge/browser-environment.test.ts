import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { afterEach, test } from "vitest";
import { PATH_SCRIPT, browserEnvironment } from "./browser-environment.ts";

const roots: string[] = [];
afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true });
});

function root(value?: string) {
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
      CLAUDE_ENV_FILE: path.join(folder, PATH_SCRIPT),
    },
  );
});

test("the Bash command script puts the link folder first after the shell startup", () => {
  const links = "/state/it's here/bin";
  const folder = root(
    JSON.stringify({
      BROWSER: "/b",
      STUDIO_BROWSER_SOCKET: "/s",
      STUDIO_BROWSER_PATH: links,
    }),
  );
  const script = browserEnvironment(folder, "s", {
    PATH: "/usr/bin",
  }).CLAUDE_ENV_FILE;
  assert(script);
  const before = readFileSync(script, "utf8");
  // A second session keeps the same script.
  browserEnvironment(folder, "t", { PATH: "/usr/bin" });
  assert.equal(readFileSync(script, "utf8"), before);
  const value = execFileSync(
    "/bin/sh",
    ["-c", '. "$1"; printf %s "$PATH"', "sh", script],
    { env: { PATH: "/usr/bin:/bin" }, encoding: "utf8" },
  );
  assert.equal(value, `${links}:/usr/bin:/bin`);
});

test("a person's own CLAUDE_ENV_FILE stays in place", () => {
  const folder = root(
    JSON.stringify({
      BROWSER: "/b",
      STUDIO_BROWSER_SOCKET: "/s",
      STUDIO_BROWSER_PATH: "/state/bin",
    }),
  );
  const result = browserEnvironment(folder, "s", {
    PATH: "/usr/bin",
    CLAUDE_ENV_FILE: "/home/person/env.sh",
  });
  assert.equal(result.CLAUDE_ENV_FILE, undefined);
  assert.equal(result.PATH, "/state/bin:/usr/bin");
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
