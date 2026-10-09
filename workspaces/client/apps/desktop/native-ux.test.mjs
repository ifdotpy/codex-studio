import assert from "node:assert/strict";
import { readFile, mkdtemp, writeFile, rm, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { EventEmitter } from "node:events";
import vm from "node:vm";
import { test } from "vitest";

test("desktop native settings, notification and speech boundaries work with fixtures", async () => {
  const require = createRequire(import.meta.url);
  const folder = await mkdtemp(join(tmpdir(), "studio-native-ux-"));
  const desktop = fileURLToPath(new URL("./", import.meta.url));
  const main = await readFile(new URL("./main.cjs", import.meta.url), "utf8");
  async function fixture() {
    let window, handler, notification, ready;
    const display = {
      id: 1,
      workArea: { x: 0, y: 0, width: 1440, height: 960 },
    };
    const started = new Promise((resolve) => {
      ready = resolve;
    });
    const events = [];
    let backendBuild = "installed";
    class Window extends EventEmitter {
      constructor() {
        super();
        window = this;
        this.webContents = Object.assign(new EventEmitter(), {
          mainFrame: {
            url: "http://localhost:1234/",
            send: (...args) => window.webContents.send(...args),
          },
          session: {
            setPermissionRequestHandler() {},
            setPermissionCheckHandler() {},
          },
          setWindowOpenHandler() {},
          isDestroyed: () => false,
          getURL: () => "http://localhost:1234/",
          send: (...args) => {
            events.push(args);
            window.webContents.emit("sent", ...args);
          },
        });
      }
      async loadURL() {}
      isDestroyed() {
        return false;
      }
      isMinimized() {
        return false;
      }
      show() {
        events.push(["show"]);
      }
      focus() {
        events.push(["focus"]);
      }
    }
    class Notice extends EventEmitter {
      static isSupported() {
        return true;
      }
      constructor(options) {
        super();
        notification = this;
        this.options = options;
      }
      show() {
        events.push(["notice"]);
        this.emit("show");
      }
    }
    const app = {
      setPath() {},
      getPath: () => folder,
      getAppPath: () => join(folder, "uninstalled-app"),
      setName() {},
      enableSandbox() {},
      isPackaged: false,
      requestSingleInstanceLock: () => true,
      whenReady: () => Promise.resolve(),
      setActivationPolicy() {},
      on() {},
      once() {},
      exit() {
        throw Error("startup failed");
      },
    };
    vm.runInNewContext("(function(){\n" + main + "\n})()", {
      require: (name) =>
        name === "electron"
          ? {
              app,
              BrowserWindow: Window,
              dialog: { showErrorBox() {} },
              ipcMain: {
                handle: (_key, value) => {
                  handler = value;
                },
              },
              Notification: Notice,
              Menu: {
                setApplicationMenu() {},
                buildFromTemplate() {
                  return { getMenuItemById: () => null };
                },
              },
              screen: {
                getPrimaryDisplay: () => display,
                getAllDisplays: () => [display],
              },
            }
          : name === "./backend.cjs"
            ? {
                ensureBackend: async () => ({
                  origin: "http://localhost:1234",
                  stateDir: folder,
                }),
                identity: async (origin, state) => {
                  assert.equal(origin, "http://localhost:1234");
                  assert.equal(state, folder);
                  return { backendBuild };
                },
                updateStatus: (_resources, running) => ({
                  updateRequired: running.backendBuild !== "installed",
                  availableBackendBuild: "installed",
                }),
              }
            : name === "./ui-host.cjs"
              ? {
                  startUiHost: async () => ({
                    origin: "http://127.0.0.1:1235",
                    close: async () => {},
                  }),
                }
              : name === "./speech.cjs"
                ? require("./speech.cjs")
                : name === "./recovery.cjs"
                  ? require("./recovery.cjs")
                  : name.startsWith("./")
                    ? require(join(desktop, name.slice(2)))
                    : require(name),
      process: { argv: ["--hidden"], env: {}, platform: "darwin" },
      __dirname: join(folder, "desktop"),
      module: { exports: {} },
      AbortController,
      fetch: async () => ({
        status: 200,
        statusText: "OK",
        headers: new Map(),
        arrayBuffer: async () => new ArrayBuffer(0),
      }),
      URL,
      setTimeout,
      clearTimeout,
      setInterval,
      clearInterval,
      console: { log: ready, error: console.error },
    });
    await started;
    const event = {
      sender: window.webContents,
      senderFrame: window.webContents.mainFrame,
    };
    return {
      invoke: (method, value) => handler(event, { method, value }),
      handler,
      event,
      events,
      notification: () => notification,
      setBackendBuild: (value) => {
        backendBuild = value;
      },
    };
  }
  try {
    let f = await fixture();
    assert.equal((await f.invoke("getBackendUpdate")).updateRequired, false);
    const serverFrame = {
      url: `http://${require("./frame-owner.cjs").serverFrameHost("local")}:1235/?studio-server=local&studio-parent=http%3A%2F%2Flocalhost%3A1234`,
      parent: f.event.senderFrame,
    };
    f.event.senderFrame.frames = [serverFrame];
    const serverEvent = { sender: f.event.sender, senderFrame: serverFrame };
    assert.equal(
      (
        await f.handler(serverEvent, {
          method: "serverNativeAction",
          value: { serverId: "local", method: "getBackendUpdate" },
        })
      ).updateRequired,
      false,
    );
    await assert.rejects(
      f.handler(serverEvent, {
        method: "serverNativeAction",
        value: { serverId: "other", method: "getBackendUpdate" },
      }),
      /Invalid server frame owner/,
    );
    await assert.rejects(
      f.invoke("serverNativeAction", {
        serverId: "local",
        method: "getBackendUpdate",
      }),
      /Invalid server frame owner/,
    );
    await assert.rejects(
      f.handler(serverEvent, {
        method: "serverNativeAction",
        value: { serverId: "local", method: "notify" },
      }),
      /Unknown server native action/,
    );
    for (const url of [
      "/api/session",
      "/api/sync/pull",
      "/api/ui-summary?server=B",
      "/api/ui-summary#B",
      "/api/ui-summary/",
    ]) {
      await assert.rejects(
        f.invoke("serverCredentialAction", {
          action: "summary",
          serverId: "remote",
          credentialId: "missing",
          url: "https://remote.tailnet.ts.net" + url,
          method: "GET",
        }),
        /Invalid shell summary request/,
      );
    }
    await assert.rejects(
      f.invoke("serverCredentialAction", {
        action: "summary",
        serverId: "remote",
        url: "https://remote.tailnet.ts.net/api/ui-summary",
        method: "POST",
      }),
      /Invalid shell summary request/,
    );
    await assert.rejects(
      f.invoke("serverCredentialAction", {
        action: "summary",
        serverId: "remote",
        credentialId: "missing",
        url: "https://remote.tailnet.ts.net/api/ui-summary",
        method: "GET",
      }),
      /server key is unavailable/,
    );
    await assert.rejects(
      f.handler(serverEvent, {
        method: "serverCredentialAction",
        value: {
          action: "summary",
          serverId: "remote",
          url: "https://remote.tailnet.ts.net/api/ui-summary",
          method: "GET",
        },
      }),
      /Invalid shell summary request/,
    );
    await assert.rejects(
      f.handler(serverEvent, {
        method: "serverCredentialAction",
        value: { action: "request", serverId: "other" },
      }),
      /Invalid server frame owner/,
    );
    await assert.rejects(
      f.handler(
        {
          sender: f.event.sender,
          senderFrame: {
            url: "http://localhost:1234/?studio-server=local",
            parent: f.event.senderFrame,
          },
        },
        { method: "getBackendUpdate" },
      ),
      /Invalid server frame owner/,
    );

    const remoteManagementFrame = {
      url: `http://${require("./frame-owner.cjs").serverFrameHost("remote")}:1235/?studio-server=remote&studio-parent=http%3A%2F%2Flocalhost%3A1234`,
      parent: f.event.senderFrame,
    };
    f.event.senderFrame.frames.push(remoteManagementFrame);
    for (const [owner, origin, frame] of [
      ["local", "http://localhost:1234", serverFrame],
      ["remote", "https://remote.tailnet.ts.net", remoteManagementFrame],
    ]) {
      for (const path of [
        "/api/multi-server",
        "/api/multi-server/",
        "/api/multi%2dserver",
        "/api/multi-server/v1/pair",
        "/api/multi-server/v1/auto-pair",
      ]) {
        await assert.rejects(
          f.handler(
            { sender: f.event.sender, senderFrame: frame },
            {
              method: "serverCredentialAction",
              value: {
                action: "request",
                serverId: owner,
                streamId: "management",
                url: origin + path,
                method: "POST",
              },
            },
          ),
          /Use the workspace shell for server management/,
        );
      }
    }
    assert.equal(
      (
        await f.handler(serverEvent, {
          method: "serverCredentialAction",
          value: {
            action: "request",
            serverId: "local",
            streamId: "ordinary",
            url: "http://localhost:1234/api/messages",
            method: "POST",
          },
        })
      ).status,
      200,
    );

    await assert.rejects(
      f.handler(serverEvent, {
        method: "serverCredentialAction",
        value: {
          action: "pairAttempt",
          serverId: "local",
          requestId: "probe",
          origin: "https://remote.tailnet.ts.net",
        },
      }),
      /Use the server manager/,
    );
    assert.equal(
      await f.invoke("serverCredentialAction", {
        action: "pairAttempt",
        serverId: "remote",
        requestId: "missing",
        origin: "https://remote.tailnet.ts.net",
      }),
      false,
    );
    await assert.rejects(
      f.handler(serverEvent, {
        method: "serverCredentialAction",
        value: {
          action: "pairApproved",
          origin: "https://remote.tailnet.ts.net",
        },
      }),
      /Use the server manager/,
    );
    f.setBackendBuild("old");
    assert.equal((await f.invoke("getBackendUpdate")).updateRequired, true);
    f.setBackendBuild("installed");
    assert.equal((await f.invoke("getBackendUpdate")).updateRequired, false);
    await assert.rejects(
      f.handler(
        { sender: {}, senderFrame: f.event.senderFrame },
        { method: "getBackendUpdate" },
      ),
      /restricted/,
    );
    await assert.rejects(
      f.invoke("notify", {
        title: "a",
        body: "b",
        target: { agentId: "a", section: "evil" },
      }),
      /target/,
    );
    const target = {
      agentId: "agent-123",
      section: "messages",
      itemId: "question-456",
    };
    await f.invoke("notify", {
      title: "Question",
      body: "Open the question",
      target,
    });
    assert.deepEqual(f.events, [["notice"]]);
    f.notification().emit("click");
    assert.deepEqual(JSON.parse(JSON.stringify(f.events.slice(1))), [
      ["show"],
      ["focus"],
      ["codex-desktop-navigate", target],
    ]);
    assert.equal(await f.invoke("cancelTranscription", "unknown"), false);
    await assert.rejects(
      f.invoke("transcribeAudio", { id: "a", permit: "invalid" }),
      /Transcribe/,
    );

    const preload = await readFile(
      new URL("./preload.cjs", import.meta.url),
      "utf8",
    );
    let api;
    const ipc = Object.assign(new EventEmitter(), {
      invoke: async (...args) => args,
    });
    const listeners = {};
    vm.runInNewContext(preload, {
      process: { isMainFrame: true, platform: "darwin", argv: [] },
      URLSearchParams,
      location: { search: "" },
      window: {
        addEventListener: (name, cb) => {
          listeners[name] = cb;
        },
      },
      performance: { now: () => 10 },
      require: () => ({
        contextBridge: {
          exposeInMainWorld: (_name, value) => {
            api = value;
          },
        },
        ipcRenderer: ipc,
      }),
    });
    const updateCall = await api.getBackendUpdate();
    assert.equal(updateCall[0], "codex-desktop");
    assert.equal(updateCall[1].method, "getBackendUpdate");
    assert.equal(api.getNotifications, undefined);
    assert.equal(api.setNotifications, undefined);
    listeners.click({ isTrusted: true });
    const cancelCall = await api.cancelTranscription("a");
    assert.equal(cancelCall[0], "codex-desktop");
    assert.equal(cancelCall[1].method, "cancelTranscription");
    const received = [];
    const unsubscribe = api.onNavigate((value) => received.push(value));
    ipc.emit("codex-desktop-navigate", {}, target);
    unsubscribe();
    ipc.emit("codex-desktop-navigate", {}, target);
    assert.deepEqual(received, [target]);

    const { transcribe } = require("./speech.cjs");
    const data = new ArrayBuffer(46),
      view = new DataView(data),
      bytes = new Uint8Array(data);
    for (const [offset, text] of [
      [0, "RIFF"],
      [8, "WAVE"],
      [12, "fmt "],
      [36, "data"],
    ])
      bytes.set(Buffer.from(text), offset);
    for (const [offset, value] of [
      [4, 38],
      [16, 16],
      [24, 16000],
      [28, 32000],
      [40, 2],
    ])
      view.setUint32(offset, value, true);
    for (const [offset, value] of [
      [20, 1],
      [22, 1],
      [32, 2],
      [34, 16],
    ])
      view.setUint16(offset, value, true);
    const helper = join(folder, "helper");
    await writeFile(
      helper,
      '#!/usr/bin/env node\nprocess.stderr.write(\'{"type":"progress","completed":0,"total":2}\\n\');process.stderr.write(\'{"type":"progress","completed":2,"total":2}\\n\');console.log(\'{"text":"ok"}\');',
      { mode: 0o700 },
    );
    const progress = [];
    assert.equal(
      (
        await transcribe({ audio: data, locale: "en-US" }, helper, {
          onProgress: (value) => progress.push(value),
        })
      ).text,
      "ok",
    );
    assert.deepEqual(progress, [
      { completed: 0, total: 2 },
      { completed: 2, total: 2 },
    ]);
    await writeFile(
      helper,
      '#!/usr/bin/env node\nprocess.stderr.write(\'{"type":"progress","completed":0,"total":1}\\n\');setInterval(()=>{},1000);',
    );
    await mkdir(join(folder, "desktop/native"), { recursive: true });
    await writeFile(
      join(folder, "desktop/native/studio-speech"),
      await readFile(helper),
      { mode: 0o700 },
    );
    const permit = await f.invoke("prepareTranscription");
    const reported = new Promise((resolve) =>
      f.event.sender.once("sent", (channel, value) =>
        resolve({ channel, value }),
      ),
    );
    const active = f.invoke("transcribeAudio", {
      id: "recording-1",
      audio: data,
      locale: "en-US",
      permit,
    });
    const rejected = assert.rejects(active, /canceled/);
    const report = await reported;
    assert.equal(report.channel, "codex-desktop-transcription-progress");
    assert.equal(report.value.id, "recording-1");
    assert.equal(
      await f.invoke("cancelTranscription", "other-recording"),
      false,
    );
    assert.equal(await f.invoke("cancelTranscription", "recording-1"), true);
    await rejected;
    await assert.rejects(
      f.invoke("transcribeAudio", {
        id: "recording-1",
        audio: data,
        locale: "en-US",
        permit,
      }),
      /Transcribe/,
    );
    for (const action of ["release", "navigate", "destroy"]) {
      const remoteFrame = {
        url: `http://${require("./frame-owner.cjs").serverFrameHost("remote")}:1235/?studio-server=remote&studio-parent=http%3A%2F%2Flocalhost%3A1234`,
        parent: f.event.senderFrame,
        processId: 12,
        routingId: 34,
        send: (...args) => f.event.sender.send(...args),
        destroyed: false,
        isDestroyed() {
          return this.destroyed;
        },
      };
      f.event.senderFrame.frames = [remoteFrame];
      const remoteEvent = { sender: f.event.sender, senderFrame: remoteFrame };
      const remoteInvoke = (method, value) =>
        f.handler(remoteEvent, { method, value });
      const remotePermit = await remoteInvoke("prepareTranscription");
      const progressReady = new Promise((resolve) =>
        f.event.sender.once("sent", resolve),
      );
      const listenerBaseline = f.event.sender.listenerCount(
        "did-start-navigation",
      );
      const remoteActive = remoteInvoke("transcribeAudio", {
        id: "remote-recording",
        audio: data,
        locale: "en-US",
        permit: remotePermit,
      });
      const aborted = assert.rejects(remoteActive, /canceled/);
      await progressReady;
      assert.equal(
        f.event.sender.listenerCount("did-start-navigation"),
        listenerBaseline + 1,
      );
      if (action === "release") await f.invoke("releaseServerView", "remote");
      if (action === "navigate")
        f.event.sender.emit("did-start-navigation", {
          isMainFrame: false,
          frame: remoteFrame,
        });
      if (action === "destroy") remoteFrame.destroyed = true;
      let deadline;
      try {
        await Promise.race([
          aborted,
          new Promise((_, reject) => {
            deadline = setTimeout(
              () =>
                reject(
                  new Error(`Frame ${action} did not abort transcription`),
                ),
              2000,
            );
          }),
        ]);
        assert.equal(
          f.event.sender.listenerCount("did-start-navigation"),
          listenerBaseline,
        );
      } finally {
        clearTimeout(deadline);
        remoteFrame.destroyed = false;
        await remoteInvoke("cancelTranscription", "remote-recording");
        await aborted;
      }
      assert.equal(f.event.sender.listenerCount("destroyed"), 0);
    }
    const controller = new AbortController();
    await assert.rejects(
      transcribe({ audio: data, locale: "en-US" }, helper, {
        signal: controller.signal,
        onProgress: () => controller.abort(),
      }),
      /canceled/,
    );
    console.log(
      "PASS: native settings survive restart; notification click preserves target; sender and gesture gates; listener disposal; speech progress and child cancellation. No microphone, model, or OS input.",
    );
  } finally {
    await rm(folder, { recursive: true, force: true });
  }
});
