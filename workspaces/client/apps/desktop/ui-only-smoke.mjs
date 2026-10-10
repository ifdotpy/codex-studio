import assert from "node:assert/strict";
import frameOwners from "./frame-owner.cjs";
import { _electron as electron } from "playwright-core";
import { mkdtemp, access, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
const root = path.dirname(fileURLToPath(import.meta.url));
const folder = await mkdtemp(path.join(tmpdir(), "studio-ui-only-smoke-"));
let desktop;
try {
  desktop = await electron.launch({
    ...(process.env.CODEX_UI_EXECUTABLE
      ? { executablePath: process.env.CODEX_UI_EXECUTABLE, args: ["--hidden"] }
      : { args: [root, "--hidden", "--ui-only"] }),
    env: {
      ...process.env,
      CODEX_DESKTOP_PROFILE: path.join(folder, "profile"),
      CODEX_AGENTS_STATE_DIR: path.join(folder, "state"),
      CODEX_UI_PORT: "0",
    },
  });
  const page = await desktop.firstWindow();
  page.setDefaultTimeout(15000);
  const apiRequests = [];
  page.on("request", (request) => {
    if (new URL(request.url()).pathname.startsWith("/api/"))
      apiRequests.push(request.url());
  });
  await page.getByRole("dialog", { name: "Studio settings" }).waitFor();
  assert.match(page.url(), /\?studio-ui-only=1$/);
  assert.equal(
    await desktop.evaluate(({ BrowserWindow }) =>
      BrowserWindow.getAllWindows()[0].isVisible(),
    ),
    false,
  );
  assert.equal(await page.locator("iframe").count(), 0);
  assert.equal(
    (await page.request.get(new URL("/api/session", page.url()).href)).status(),
    404,
  );
  assert.deepEqual(apiRequests, []);
  await assert.rejects(access(path.join(folder, "state")), /ENOENT/);
  await page.evaluate(
    ({ serverHost }) => {
      localStorage.setItem(
        "studio-paired-servers-v1",
        JSON.stringify([
          {
            id: "unpaired",
            label: "Unpaired",
            origin: "https://unpaired.tailnet.ts.net",
            credentialId: "missing",
          },
        ]),
      );
      const frame = document.createElement("iframe");
      const base = new URL(
        window.codexDesktop.serverViewOrigin || location.origin,
      );
      if (window.codexDesktop.serverViewOrigin) base.hostname = serverHost;
      base.search = new URLSearchParams({
        "studio-server": "unpaired",
        ...(window.codexDesktop.serverViewOrigin
          ? {
              "studio-parent": location.origin,
              "studio-origin": "https://unpaired.tailnet.ts.net",
              "studio-credential": "missing",
            }
          : {}),
      }).toString();
      frame.src = base.href;
      frame.title = "unpaired";
      document.body.append(frame);
    },
    { serverHost: frameOwners.serverFrameHost("unpaired") },
  );
  const serverFrame = await page
    .locator('iframe[title="unpaired"]')
    .contentFrame();
  await serverFrame.locator("html").waitFor();
  const attacker = page
    .frames()
    .find((frame) => frame.url().includes("studio-server=unpaired"));
  const parentDenied = await attacker.evaluate(async () => {
    try {
      await parent.codexDesktop.getBackendUpdate();
      return false;
    } catch (error) {
      return error.name === "SecurityError";
    }
  });
  assert.equal(
    parentDenied,
    true,
    "A server frame cannot reach the parent native bridge",
  );
  assert.equal(
    await attacker.evaluate(
      () => typeof require === "undefined" && typeof process === "undefined",
    ),
    true,
  );
  const denied = await attacker.evaluate(async () => {
    const errors = [];
    for (const request of [
      {
        action: "request",
        serverId: "other",
        streamId: "omitted-owner",
        credentialId: "missing",
        url: "https://unpaired.tailnet.ts.net/api/session",
        requestId: "once",
      },
      {
        action: "request",
        serverId: "other",
        frameOwner: "other",
        streamId: "spoofed-owner",
        credentialId: "missing",
        url: "https://unpaired.tailnet.ts.net/api/session",
        requestId: "once",
      },
      {
        action: "request",
        serverId: "other",
        frameOwner: "unpaired",
        streamId: "once",
        credentialId: "missing",
        url: "https://unpaired.tailnet.ts.net/api/session",
        requestId: "once",
      },
      {
        action: "request",
        serverId: "unpaired",
        frameOwner: "unpaired",
        streamId: "once",
        credentialId: "missing",
        url: "https://unpaired.tailnet.ts.net/api/session",
        requestId: "once",
      },
    ]) {
      try {
        await window.codexDesktop.serverCredentialAction(request);
      } catch (error) {
        errors.push(error.message);
      }
    }
    return errors;
  });
  assert.match(denied[0], /Invalid server frame owner/);
  assert.match(denied[1], /Invalid server frame owner/);
  assert.match(denied[2], /Invalid server frame owner/);
  assert.match(denied[3], /server key|Secure key storage/);
  const spoofedNative = await attacker.evaluate(async () => {
    try {
      await window.codexDesktop.serverNativeAction("local", "getBackendUpdate");
      return false;
    } catch {
      return true;
    }
  });
  assert.equal(spoofedNative, true);
  console.log(
    JSON.stringify({
      uiOnly: true,
      hidden: true,
      stateCreated: false,
      localApi: false,
      frameOwnerDenied: true,
      parentBridgeDenied: true,
      spoofedNativeDenied: true,
      executable: process.env.CODEX_UI_EXECUTABLE ? "packaged" : "development",
    }),
  );
} finally {
  if (desktop) {
    const child = desktop.process();
    // Playwright starts Electron in its own process group on Unix. Close its
    // helpers too, because they can retain the test's transport after shutdown.
    const watchdog = setTimeout(() => {
      try {
        if (process.platform === "win32") child.kill("SIGKILL");
        else process.kill(-child.pid, "SIGKILL");
      } catch (error) {
        if (error.code !== "ESRCH") throw error;
      }
    }, 5000);
    watchdog.unref();
    try {
      await desktop.close();
    } finally {
      clearTimeout(watchdog);
    }
  }
  await rm(folder, { recursive: true, force: true });
}
