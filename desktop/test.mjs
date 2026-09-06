import assert from "node:assert/strict";
import { _electron as electron } from "playwright-core";
import { createRequire } from "node:module";
import { mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { createServer } from "node:net";
const require = createRequire(import.meta.url);
const { ensureBackend, identity } = require("./backend.cjs");
const root = path.dirname(fileURLToPath(import.meta.url));
const temp = await mkdtemp(path.join(tmpdir(), "codex-desktop-test-"));
const state = path.join(temp, "state");
const port = await new Promise((resolve) => {
  const server = createServer();
  server.listen(0, "127.0.0.1", () => {
    const port = server.address().port;
    server.close(() => resolve(port));
  });
});
const env = {
  ...process.env,
  CODEX_AGENTS_STATE_DIR: state,
  CODEX_DESKTOP_PORT: String(port),
  CODEX_DESKTOP_PROFILE: path.join(temp, "profile"),
  CODEX_BOARD_STATE_DIR: path.join(temp, "board"),
};
const resources = path.resolve(root, "..");
let desktop;
let backend;
const poll = async (fn) => {
  for (let i = 0; i < 100; i++) {
    if (await fn()) return;
    await new Promise((r) => setTimeout(r, 100));
  }
  throw Error("Timed out");
};
try {
  // Independent launchers race the real Python listener and the real state flock.
  const race = await Promise.all([
    ensureBackend({ resources, port, env }),
    ensureBackend({ resources, port, env }),
  ]);
  assert.equal(race[0].pid, race[1].pid);
  backend = race[0];
  await assert.rejects(
    identity(backend.origin, path.join(temp, "different-state")),
    /different state/,
  );
  const attached = await ensureBackend({ resources, port, env });
  assert.equal(attached.owned, false);
  desktop = await electron.launch({ args: [root, "--hidden"], env });
  const page = await desktop.firstWindow();
  await page.waitForFunction(() => !!window.codexDesktop);
  const preferences = await desktop.evaluate(({ BrowserWindow }) => {
    const window = BrowserWindow.getAllWindows()[0];
    const preferences = window.webContents.getLastWebPreferences();
    return {
      sandbox: preferences.sandbox,
      contextIsolation: preferences.contextIsolation,
      nodeIntegration: preferences.nodeIntegration,
      visible: window.isVisible(),
    };
  });
  assert.deepEqual(preferences, {
    sandbox: true,
    contextIsolation: true,
    nodeIntegration: false,
    visible: false,
  });
  assert.equal(await page.evaluate(() => typeof window.require), "undefined");
  assert.match(
    await page.evaluate(() =>
      window.codexDesktop.pickDirectory().catch((e) => e.message),
    ),
    /desktop button/,
  );
  assert.equal(
    await page.evaluate(() =>
      window.codexDesktop.notify({ title: "Test", body: "Must not display" }),
    ),
    false,
  );
  const file = path.join(temp, "native-picker.txt");
  await writeFile(file, "Native attachment");
  await desktop.evaluate(
    ({ dialog, shell, app }, value) => {
      dialog.showOpenDialog = async (_window, options) => ({
        canceled: false,
        filePaths: options.properties.includes("openDirectory")
          ? [value.directory]
          : [value.file],
      });
      shell.openExternal = async (url) => {
        app.__testExternal = url;
      };
      shell.showItemInFolder = (path) => {
        app.__testReveal = path;
      };
    },
    { file, directory: temp },
  );
  // Real React callers use the isolated bridge and the real asset/database API.
  await page.getByRole("button", { name: "New chat", exact: true }).click();
  await page.locator("#project:not([disabled])").waitFor();
  const projectResponse = page.waitForResponse(
    (response) =>
      response.url().endsWith("/api/conversation") &&
      response.request().method() === "POST",
  );
  await page.locator("#project").click();
  assert.equal((await projectResponse).status(), 200);
  const uiState = await (await fetch(`${backend.origin}/api/state`)).json();
  const lead = uiState.runtime.agents.find((agent) => agent.isLead);
  assert.ok(lead.cwd.endsWith(path.basename(temp)));
  assert.equal(lead.threadId, null);
  const uploadResponse = page.waitForResponse(
    (response) =>
      response.url().endsWith("/api/assets") &&
      response.request().method() === "POST",
  );
  await page.getByRole("button", { name: "Attach", exact: true }).click();
  const uploaded = await uploadResponse;
  assert.equal(uploaded.status(), 200);
  const asset = await uploaded.json();
  assert.equal(asset.name, "native-picker.txt");
  await page
    .locator(".attachment-chip")
    .filter({ hasText: "native-picker.txt" })
    .waitFor();
  assert.equal(
    await readFile(path.join(state, "uploads", asset.id, asset.name), "utf8"),
    "Native attachment",
  );
  await page
    .getByRole("button", { name: "Inbox", exact: true })
    .first()
    .click();
  await page
    .getByRole("button", { name: "Enable desktop alerts", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Disable desktop alerts", exact: true })
    .click();
  await page
    .getByRole("button", { name: "Enable desktop alerts", exact: true })
    .waitFor();
  await page.keyboard.press("Escape");
  await page.getByRole("dialog").waitFor({ state: "hidden" });
  await page.evaluate(() => {
    const link = document.createElement("a");
    link.id = "native-external-link";
    link.href = "https://example.com/native-ui";
    link.textContent = "Native external link";
    link.style.cssText = "position:fixed;top:0;right:0;z-index:2147483647";
    document.body.append(link);
  });
  await page.locator("#native-external-link").click();
  await poll(
    async () =>
      (await desktop.evaluate(({ app }) => app.__testExternal)) ===
      "https://example.com/native-ui",
  );
  await page.locator("#native-external-link").evaluate((link) => link.remove());
  assert.equal(page.url(), `${backend.origin}/`);
  const finalState = await (await fetch(`${backend.origin}/api/state`)).json();
  assert.ok(
    finalState.runtime.agents.every(
      (agent) => !agent.threadId && !agent.inFlight,
    ),
  );
  await page.screenshot({ path: path.join(temp, "native-react-callers.png") });
  await page.evaluate(() => {
    const button = document.createElement("button");
    button.id = "native-test";
    button.style.cssText = "position:fixed;top:0;right:0;z-index:2147483647";
    button.textContent = "Native test";
    button.onclick = () =>
      window.codexDesktop[button.dataset.method](button.dataset.value)
        .then((value) => {
          document.body.dataset.nativeResult = JSON.stringify({ value });
        })
        .catch((e) => {
          document.body.dataset.nativeResult = JSON.stringify({
            error: e.message,
          });
        });
    document.body.append(button);
  });
  const invoke = async (method, value) => {
    await page.locator("#native-test").evaluate(
      (button, request) => {
        button.dataset.method = request.method;
        button.dataset.value = request.value;
        delete document.body.dataset.nativeResult;
      },
      { method, value },
    );
    await page.locator("#native-test").click();
    await page.waitForFunction(() => document.body.dataset.nativeResult);
    return JSON.parse(
      await page.evaluate(() => document.body.dataset.nativeResult),
    );
  };
  assert.equal((await invoke("pickDirectory")).value, temp);
  const files = (await invoke("pickFiles")).value;
  assert.equal(
    Buffer.from(files[0].data, "base64").toString(),
    "Native attachment",
  );
  assert.equal(files[0].mime, "text/plain");
  assert.match(
    (await invoke("openExternal", "file:///etc/passwd")).error,
    /HTTP/,
  );
  await invoke("openExternal", "https://example.com/");
  assert.equal(
    await desktop.evaluate(({ app }) => app.__testExternal),
    "https://example.com/",
  );
  await invoke("revealPath", file);
  assert.equal(await desktop.evaluate(({ app }) => app.__testReveal), file);
  await page.evaluate(() => {
    const frame = document.createElement("iframe");
    frame.srcdoc = "<p>Untrusted preview</p>";
    frame.sandbox = "allow-scripts";
    document.body.append(frame);
  });
  await poll(async () => page.frames().length === 2);
  assert.equal(
    await page.frames()[1].evaluate(() => typeof window.codexDesktop),
    "undefined",
  );
  assert.equal(
    await page.frames()[1].locator("p").textContent(),
    "Untrusted preview",
  );
  await desktop.evaluate(async ({ BrowserWindow, app }) => {
    const main = BrowserWindow.getAllWindows()[0];
    const { nativeAction } = process.mainModule.require(
      `${app.getAppPath()}/main.cjs`,
    );
    const frame = main.webContents.mainFrame.frames[0];
    try {
      await nativeAction(
        { sender: main.webContents, senderFrame: frame },
        { method: "pickFiles" },
      );
      throw Error("Subframe gained native access");
    } catch (error) {
      if (!error.message.includes("main frame")) throw error;
    }
  });
  // Exercise the actual IPC channel from a separate renderer with the same preload.
  const rogueId = await desktop.evaluate(async ({ BrowserWindow, app }) => {
    const main = BrowserWindow.getAllWindows()[0];
    const rogue = new BrowserWindow({
      show: false,
      webPreferences: {
        preload: `${app.getAppPath()}/preload.cjs`,
        sandbox: true,
        contextIsolation: true,
        nodeIntegration: false,
      },
    });
    await rogue.loadURL(main.webContents.getURL());
    return rogue.id;
  });
  const rogue = desktop.windows().find((candidate) => candidate !== page);
  await rogue.evaluate(() => {
    const button = document.createElement("button");
    button.id = "ipc-test";
    button.style.cssText = "position:fixed;top:0;right:0;z-index:2147483647";
    button.textContent = "IPC test";
    button.onclick = () =>
      window.codexDesktop.pickFiles().catch((e) => {
        document.body.dataset.ipcError = e.message;
      });
    document.body.append(button);
  });
  // Playwright dispatches a real renderer input event without OS input or window activation.
  await rogue.locator("#ipc-test").click();
  await rogue.waitForFunction(() =>
    document.body.dataset.ipcError?.includes("main frame"),
  );
  await desktop.evaluate(
    ({ BrowserWindow }, id) => BrowserWindow.fromId(id).destroy(),
    rogueId,
  );
  await page.evaluate(() => {
    window.open("https://example.com");
    location.href = "https://example.com";
  });
  await new Promise((r) => setTimeout(r, 100));
  assert.equal(page.url(), `${backend.origin}/`);
  assert.equal(desktop.windows().length, 1);
  await page.screenshot({ path: path.join(temp, "desktop-hidden.png") });
  await desktop.close();
  desktop = null;
  assert.equal(
    (await identity(backend.origin, backend.stateDir)).pid,
    backend.pid,
    "Closing Electron must preserve the backend",
  );
  // Confirm a second port cannot acquire the same state owner or damage the first.
  await assert.rejects(
    ensureBackend({ resources, port: port + 1, env }),
    /Backend exited/,
  );
  assert.equal(
    (await identity(backend.origin, backend.stateDir)).pid,
    backend.pid,
  );
  console.log(
    JSON.stringify({
      result: "PASS",
      cases: [
        "startup race",
        "attach",
        "state mismatch",
        "sandbox",
        "gesture",
        "notifications opt-in",
        "native pickers",
        "React project picker",
        "React attachment upload",
        "React notification toggle",
        "production external-link handler",
        "external URL validation",
        "local reveal",
        "preview bridge isolation",
        "subframe rejection",
        "foreign renderer IPC rejection",
        "navigation denial",
        "popup denial",
        "backend survives quit",
        "duplicate state owner rejection",
      ],
      artifact: temp,
    }),
  );
} finally {
  if (desktop) await desktop.close();
  if (backend) {
    process.kill(backend.pid, "SIGTERM");
    await poll(async () => {
      try {
        return !(await identity(backend.origin, backend.stateDir));
      } catch {
        return false;
      }
    });
  }
}
