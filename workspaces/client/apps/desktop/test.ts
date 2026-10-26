import type { Notification, WebPreferences } from "electron";
import type { DesktopBridge } from "./api.ts";
import assert from "node:assert/strict";
import type { ElectronApplication } from "playwright-core";
import { _electron as electron } from "playwright-core";
import { mkdtemp, mkdir, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { createServer } from "node:net";
import { readTestState } from "../web/tests/playwright.mjs";
import { ensureBackend, identity } from "./backend.ts";
declare module "electron" {
  interface App {
    __testNotifications: Pick<Notification, "title" | "body">[];
    __testNotificationShow: Notification["show"];
    __testExternal: string;
    __testExternalActivate: boolean | undefined;
    __testReveal: string;
    __testOpen: string;
    __testPreview: string;
  }
}
declare global {
  var __testOriginalFetch: typeof fetch;
}
const root = import.meta.dirname;
const temp = await mkdtemp(path.join(tmpdir(), "codex-desktop-test-"));
const state = path.join(temp, "state");
const port = await new Promise<number>((resolve) => {
  const server = createServer();
  server.listen(0, "127.0.0.1", () => {
    const address = server.address();
    assert.ok(address && typeof address !== "string");
    const port = address.port;
    server.close(() => resolve(port));
  });
});
const env = {
  ...process.env,
  CODEX_AGENTS_STATE_DIR: state,
  CODEX_DESKTOP_PORT: String(port),
  CODEX_DESKTOP_PROFILE: path.join(temp, "profile"),
};
const resources = path.resolve(root, "..");
let desktop: ElectronApplication | null = null;
let backend: Awaited<ReturnType<typeof ensureBackend>> | undefined;
const poll = async (fn: () => Promise<boolean>) => {
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
  assert.equal(race[0]?.pid, race[1]?.pid);
  backend = race[0];
  assert.ok(backend);
  await assert.rejects(
    identity(backend.origin, path.join(temp, "different-state")),
    /different state/,
  );
  assert.match(backend.backendBuild, /^[a-f0-9]{64}$/);
  const attached = await ensureBackend({ resources, port, env });
  assert.equal(attached.owned, false);
  await mkdir(env.CODEX_DESKTOP_PROFILE, { recursive: true });
  await writeFile(
    path.join(env.CODEX_DESKTOP_PROFILE, "notifications.json"),
    JSON.stringify({ enabled: false }),
  );
  desktop = await electron.launch({ args: [root, "--hidden"], env });
  const page = await desktop.firstWindow();
  await page.waitForFunction(() => !!window.codexDesktop);
  assert.equal(
    (
      await page.evaluate(() => {
        const bridge = window.codexDesktop;
        if (!bridge) throw new Error("The desktop bridge is unavailable.");
        if (!bridge.getBackendUpdate)
          throw new Error(
            "The desktop getBackendUpdate method is unavailable.",
          );
        return bridge.getBackendUpdate();
      })
    )?.updateRequired,
    false,
  );
  await desktop.evaluate(() => {
    const original = globalThis.fetch;
    globalThis.__testOriginalFetch = original;
    globalThis.fetch = async (...args) => {
      const response = await original(...args);
      if (String(args[0]).endsWith("/api/desktop")) {
        const body = await response.json();
        return new Response(
          JSON.stringify({ ...body, backendBuild: "a".repeat(64) }),
        );
      }
      return response;
    };
  });
  assert.equal(
    (
      await page.evaluate(() => {
        const bridge = window.codexDesktop;
        if (!bridge) throw new Error("The desktop bridge is unavailable.");
        if (!bridge.getBackendUpdate)
          throw new Error(
            "The desktop getBackendUpdate method is unavailable.",
          );
        return bridge.getBackendUpdate();
      })
    )?.updateRequired,
    true,
  );
  await page.evaluate(() => window.dispatchEvent(new Event("focus")));
  await page.getByRole("button", { name: "New chat", exact: true }).waitFor();
  assert.equal(await page.locator("[data-backend-update-pending]").count(), 0);
  await desktop.evaluate(() => {
    globalThis.fetch = globalThis.__testOriginalFetch;
  });
  const preferences = await desktop.evaluate(({ BrowserWindow }) => {
    const window = BrowserWindow.getAllWindows()[0];
    if (!window) throw new Error("Hidden window is unavailable.");
    const method: unknown = Reflect.get(
      window.webContents,
      "getLastWebPreferences",
    );
    if (typeof method !== "function")
      throw new Error("Web preferences are unavailable.");
    // Electron 44 exposes this method at runtime but omits it from its declarations.
    const preferences: WebPreferences = method.call(window.webContents);
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
    await page.evaluate(() => {
      const bridge = window.codexDesktop;
      if (!bridge) throw new Error("The desktop bridge is unavailable.");
      return bridge.pickDirectory().catch((e) => e.message);
    }),
    /desktop button/,
  );
  await desktop.evaluate(({ Notification, app }) => {
    app.__testNotifications = [];
    Notification.isSupported = () => true;
    Notification.prototype.show = function () {
      app.__testNotifications.push({ title: this.title, body: this.body });
      this.emit("show");
    };
  });
  await desktop.evaluate(({ Notification, app }) => {
    app.__testNotificationShow = Notification.prototype.show;
    Notification.prototype.show = function () {
      this.emit(
        "failed",
        {},
        "Notifications are not allowed for this application",
      );
    };
  });
  assert.match(
    await page.evaluate(() => {
      const bridge = window.codexDesktop;
      if (!bridge) throw new Error("The desktop bridge is unavailable.");
      return bridge
        .notify({
          title: "Permission test",
          body: "Test",
          target: { agentId: "fixture", section: "messages" },
        })
        .catch((e) => e.message);
    }),
    /STUDIO_NOTIFICATIONS_DENIED/,
  );
  await desktop.evaluate(({ Notification, app }) => {
    Notification.prototype.show = app.__testNotificationShow;
  });
  assert.match(
    await page.evaluate(() => {
      const bridge = window.codexDesktop;
      if (!bridge) throw new Error("The desktop bridge is unavailable.");
      // @ts-expect-error target is deliberately missing to exercise runtime validation.
      return bridge
        .notify({ title: "Test", body: "Invalid target" })
        .catch((e) => e.message);
    }),
    /Invalid notification target/,
  );
  assert.equal(
    await page.evaluate(() => {
      const bridge = window.codexDesktop;
      if (!bridge) throw new Error("The desktop bridge is unavailable.");
      return bridge.notify({
        title: "Global alert",
        body: "No app opt-in",
        target: { agentId: "test-chat", section: "messages" },
      });
    }),
    true,
  );
  assert.deepEqual(
    await desktop.evaluate(({ app }) => app.__testNotifications),
    [{ title: "Global alert", body: "No app opt-in" }],
  );
  await desktop.evaluate(({ Notification }) => {
    Notification.prototype.show = function () {
      this.emit("failed", {}, "Notification fixture denied");
    };
  });
  assert.match(
    await page.evaluate(() => {
      const bridge = window.codexDesktop;
      if (!bridge) throw new Error("The desktop bridge is unavailable.");
      return bridge
        .notify({
          title: "Failed alert",
          body: "Failure is visible",
          target: { agentId: "test-chat", section: "messages" },
        })
        .catch((error) => error.message);
    }),
    /Notification fixture denied/,
  );
  assert.equal(
    await page.evaluate(() => {
      const bridge = window.codexDesktop;
      if (!bridge) throw new Error("The desktop bridge is unavailable.");
      return typeof Reflect.get(bridge ?? {}, "setNotifications");
    }),
    "undefined",
  );
  const file = path.join(temp, "native-picker.txt");
  await writeFile(file, "Native attachment");
  await desktop.evaluate(
    ({ dialog, shell, app }, value) => {
      dialog.showOpenDialog = async (windowOrOptions, suppliedOptions?) => ({
        canceled: false,
        filePaths: (
          suppliedOptions ??
          ("properties" in windowOrOptions ? windowOrOptions : {})
        ).properties?.includes("openDirectory")
          ? [value.directory]
          : [value.file],
      });
      shell.openExternal = async (url, options) => {
        app.__testExternal = url;
        app.__testExternalActivate = options?.activate;
      };
      shell.showItemInFolder = (path) => {
        app.__testReveal = path;
      };
    },
    { file, directory: temp },
  );
  // Real React callers use the isolated bridge and the real asset/database API.
  await page.getByRole("button", { name: "New chat", exact: true }).click();
  await page
    .getByRole("button", { name: "Chat settings", exact: true })
    .click();
  await page.locator("#project:not([disabled])").waitFor();
  const projectResponse = page.waitForResponse(
    (response) =>
      response.url().endsWith("/api/conversation") &&
      response.request().method() === "POST",
  );
  await page.locator("#project").click();
  await page
    .getByRole("button", { name: "Browse folders…", exact: true })
    .click();
  await page.locator(".directory-location").filter({ hasText: temp }).waitFor();
  await page
    .getByRole("button", { name: "Use this folder", exact: true })
    .click();
  assert.equal((await projectResponse).status(), 200);
  const uiState = await readTestState(backend.origin);
  const lead = uiState.runtime.agents.find((agent) => agent.isLead);
  assert.ok(lead);
  assert.ok(lead.cwd.endsWith(path.basename(temp)));
  assert.equal(lead.threadId, null);
  const uploadResponse = page.waitForResponse(
    (response) =>
      response.url().endsWith("/api/assets") &&
      response.request().method() === "POST",
  );
  await page.getByRole("button", { name: "Attach files", exact: true }).click();
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
    .getByRole("button", { name: /^Messages/ })
    .first()
    .click();
  assert.equal(
    await page.getByRole("button", { name: /desktop alerts/ }).count(),
    0,
  );
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
  const activeDesktop = desktop;
  await poll(
    async () =>
      (await activeDesktop.evaluate(({ app }) => app.__testExternal)) ===
      "https://example.com/native-ui",
  );
  await page.locator("#native-external-link").evaluate((link) => link.remove());
  assert.equal(page.url(), `${backend.origin}/`);
  const finalState = await readTestState(backend.origin);
  assert.ok(
    finalState.runtime.agents.every(
      (agent) => !agent.threadId && !agent.inFlight,
    ),
  );
  await page.screenshot({ path: path.join(temp, "native-react-callers.png") });
  await page.evaluate(() => {
    const bridge = window.codexDesktop;
    if (!bridge) throw new Error("The desktop bridge is unavailable.");
    const button = document.createElement("button");
    button.id = "native-test";
    button.style.cssText = "position:fixed;top:0;right:0;z-index:2147483647";
    button.textContent = "Native test";
    button.onclick = () => {
      const methodName = button.dataset.method;
      if (!methodName) throw new Error("Native fixture method is unavailable.");
      let value: unknown = button.dataset.value;
      if (["saveFile", "fileAction"].includes(methodName)) {
        if (typeof value !== "string")
          throw new Error("Native fixture value is unavailable.");
        // The fixture serializes this value immediately before the click.
        const parsed: { name: string; data: number[] } = JSON.parse(value);
        value =
          methodName === "saveFile"
            ? { ...parsed, data: Uint8Array.from(parsed.data).buffer }
            : parsed;
      }
      const method: unknown = Reflect.get(bridge, methodName);
      if (typeof method !== "function")
        throw new Error("Native fixture method is unavailable.");
      const result: Promise<unknown> = Reflect.apply(method, bridge, [value]);
      return result
        .then((value) => {
          document.body.dataset.nativeResult = JSON.stringify({ value });
        })
        .catch((e) => {
          document.body.dataset.nativeResult = JSON.stringify({
            error: e.message,
          });
        });
    };
    document.body.append(button);
  });
  type InvokedMethod =
    | "pickDirectory"
    | "pickFiles"
    | "saveFile"
    | "fileAction"
    | "openExternal"
    | "openSignIn"
    | "revealPath";
  const invoke = async <K extends InvokedMethod>(
    method: K,
    value?: string,
  ): Promise<{
    value?: Awaited<ReturnType<NonNullable<DesktopBridge[K]>>>;
    error?: string;
  }> => {
    await page.locator("#native-test").evaluate(
      (button, request) => {
        button.dataset.method = request.method;
        if (request.value === undefined) delete button.dataset.value;
        else button.dataset.value = request.value;
        delete document.body.dataset.nativeResult;
      },
      { method, value },
    );
    await page.locator("#native-test").click();
    await page.waitForFunction(() => document.body.dataset.nativeResult);
    return JSON.parse(
      await page.evaluate(() => document.body.dataset.nativeResult ?? "null"),
    );
  };
  assert.equal((await invoke("pickDirectory")).value, temp);
  const files = (await invoke("pickFiles")).value;
  assert.ok(files?.[0]);
  assert.equal(
    Buffer.from(files[0].data, "base64").toString(),
    "Native attachment",
  );
  assert.equal(files[0].mime, "text/plain");
  const savedCopy = path.join(temp, "saved-copy.txt");
  await desktop.evaluate(
    ({ dialog, shell, BrowserWindow, app }, destination) => {
      dialog.showSaveDialog = async () => ({
        canceled: false,
        filePath: destination,
      });
      shell.openPath = async (file) => {
        app.__testOpen = file;
        return "";
      };
      const main = BrowserWindow.getAllWindows()[0];
      if (!main) throw new Error("Hidden window is unavailable.");
      main.previewFile = (file) => {
        app.__testPreview = file;
      };
    },
    savedCopy,
  );
  const nativeBytes = [0, 1, 2, 127, 255];
  assert.equal(
    (
      await invoke(
        "saveFile",
        JSON.stringify({ name: "copy.txt", data: nativeBytes }),
      )
    ).value,
    true,
  );
  assert.deepEqual([...(await readFile(savedCopy))], nativeBytes);
  await desktop.evaluate(({ dialog }) => {
    dialog.showSaveDialog = async () => ({ canceled: true, filePath: "" });
  });
  assert.equal(
    (await invoke("saveFile", JSON.stringify({ name: "copy.txt", data: [42] })))
      .value,
    false,
  );
  assert.deepEqual([...(await readFile(savedCopy))], nativeBytes);
  const resolvedAsset = await (
    await fetch(`${backend.origin}/api/file-info?asset=${asset.id}`)
  ).json();
  for (const action of ["open", "reveal", "preview"])
    assert.equal(
      (
        await invoke(
          "fileAction",
          JSON.stringify({ action, target: { asset: asset.id } }),
        )
      ).value,
      true,
    );
  assert.deepEqual(
    await desktop.evaluate(({ app }) => [
      app.__testOpen,
      app.__testReveal,
      app.__testPreview,
    ]),
    [resolvedAsset.path, resolvedAsset.path, resolvedAsset.path],
  );
  assert.match(
    (await invoke("openExternal", "file:///etc/passwd")).error ?? "",
    /HTTP/,
  );
  await invoke("openExternal", "https://example.com/");
  assert.deepEqual(
    await desktop.evaluate(({ app }) => [
      app.__testExternal,
      app.__testExternalActivate,
    ]),
    ["https://example.com/", false],
  );
  // An approved sign-in link brings the browser to the front; the scheme rule stays.
  assert.match(
    (await invoke("openSignIn", "javascript:alert(1)")).error,
    /HTTP/,
  );
  await invoke("openSignIn", "https://accounts.example.com/authorize?x=1");
  assert.deepEqual(
    await desktop.evaluate(({ app }) => [
      app.__testExternal,
      app.__testExternalActivate,
    ]),
    ["https://accounts.example.com/authorize?x=1", true],
  );
  await invoke("revealPath", file);
  assert.equal(await desktop.evaluate(({ app }) => app.__testReveal), file);
  await page.evaluate(() => {
    const frame = document.createElement("iframe");
    frame.srcdoc = "<p>Untrusted preview</p>";
    frame.setAttribute("sandbox", "allow-scripts");
    document.body.append(frame);
  });
  await poll(async () => page.frames().length === 2);
  assert.equal(
    await page
      .frames()
      .at(1)
      ?.evaluate(() => {
        const bridge = window.codexDesktop;
        if (!bridge) throw new Error("The desktop bridge is unavailable.");
        return typeof bridge;
      }),
    "undefined",
  );
  assert.equal(
    await page.frames().at(1)?.locator("p").textContent(),
    "Untrusted preview",
  );
  await desktop.evaluate(async ({ BrowserWindow, app }) => {
    const main = BrowserWindow.getAllWindows()[0];
    if (!main) throw new Error("Hidden window is unavailable.");
    const { nativeAction } = await import(`${app.getAppPath()}/main.ts`);
    const frame = main.webContents.mainFrame.frames[0];
    try {
      await nativeAction(
        { sender: main.webContents, senderFrame: frame },
        { method: "pickFiles" },
      );
      throw Error("Subframe gained native access");
    } catch (error) {
      if (!(error instanceof Error) || !error.message.includes("main frame"))
        throw error;
    }
  });
  // Exercise the actual IPC channel from a separate renderer with the same preload.
  const rogueId = await desktop.evaluate(async ({ BrowserWindow, app }) => {
    const main = BrowserWindow.getAllWindows()[0];
    if (!main) throw new Error("Hidden window is unavailable.");
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
  assert.ok(rogue);
  await rogue.evaluate(() => {
    const bridge = window.codexDesktop;
    if (!bridge) throw new Error("The desktop bridge is unavailable.");
    const button = document.createElement("button");
    button.id = "ipc-test";
    button.style.cssText = "position:fixed;top:0;right:0;z-index:2147483647";
    button.textContent = "IPC test";
    button.onclick = () =>
      bridge.pickFiles().catch((e) => {
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
    ({ BrowserWindow }, id) => BrowserWindow.fromId(id)?.destroy(),
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
  const savedBounds = await desktop.evaluate(({ BrowserWindow }) => {
    const window = BrowserWindow.getAllWindows()[0];
    if (!window) throw new Error("Hidden window is unavailable.");
    const bounds = window.getBounds();
    window.setBounds(
      {
        ...bounds,
        width: Math.max(420, bounds.width - 100),
        height: Math.max(600, bounds.height - 80),
      },
      false,
    );
    return window.getNormalBounds();
  });
  await desktop.close();
  desktop = null;
  const savedWindow = JSON.parse(
    await readFile(
      path.join(env.CODEX_DESKTOP_PROFILE, "window-state.json"),
      "utf8",
    ),
  );
  assert.deepEqual(savedWindow.bounds, savedBounds);
  desktop = await electron.launch({ args: [root, "--hidden"], env });
  const reopened = await desktop.firstWindow();
  await reopened.waitForFunction(() => !!window.codexDesktop);
  assert.deepEqual(
    await desktop.evaluate(({ BrowserWindow }) =>
      BrowserWindow.getAllWindows().at(0)?.getNormalBounds(),
    ),
    savedBounds,
  );
  assert.equal(
    await desktop.evaluate(({ BrowserWindow }) =>
      BrowserWindow.getAllWindows().at(0)?.isVisible(),
    ),
    false,
  );
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
        "backend source identity",
        "backend mismatch without a chat notice",
        "state mismatch",
        "sandbox",
        "gesture",
        "global native notifications ignore legacy opt-out",
        "native pickers",
        "native file save bytes and cancel through isolated IPC",
        "native file metadata and actions through isolated IPC",
        "React project picker",
        "React attachment upload",
        "React has no notification toggle",
        "production external-link handler",
        "external URL validation",
        "local reveal",
        "preview bridge isolation",
        "subframe rejection",
        "foreign renderer IPC rejection",
        "navigation denial",
        "popup denial",
        "window bounds survive hidden app restart",
        "backend survives quit",
        "duplicate state owner rejection",
      ],
      artifact: temp,
    }),
  );
} finally {
  if (desktop) await desktop.close();
  if (backend) {
    const stoppedBackend = backend;
    process.kill(backend.pid, "SIGTERM");
    await poll(async () => {
      try {
        return !(await identity(
          stoppedBackend.origin,
          stoppedBackend.stateDir,
        ));
      } catch {
        return false;
      }
    });
  }
}
