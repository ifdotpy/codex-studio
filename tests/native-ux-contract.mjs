import assert from "node:assert/strict";
import { readFile, mkdtemp, writeFile, rm, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { EventEmitter } from "node:events";
import vm from "node:vm";
const require = createRequire(import.meta.url);
const folder = await mkdtemp(join(tmpdir(), "studio-native-ux-"));
const desktop = fileURLToPath(new URL("../desktop/", import.meta.url));
const main = await readFile(
  new URL("../desktop/main.cjs", import.meta.url),
  "utf8",
);
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
        mainFrame: { url: "http://localhost:1234/" },
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
    setName() {},
    enableSandbox() {},
    requestSingleInstanceLock: () => true,
    whenReady: () => Promise.resolve(),
    setActivationPolicy() {},
    on() {},
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
            ipcMain: {
              handle: (_key, value) => {
                handler = value;
              },
            },
            Notification: Notice,
            Menu: { setApplicationMenu() {}, buildFromTemplate() {} },
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
          : name === "./speech.cjs"
            ? require("../desktop/speech.cjs")
            : name === "./recovery.cjs"
              ? require("../desktop/recovery.cjs")
              : name.startsWith("./")
                ? require(join(desktop, name.slice(2)))
                : require(name),
    process: { argv: ["--hidden"], env: {}, platform: "darwin" },
    __dirname: join(folder, "desktop"),
    module: { exports: {} },
    AbortController,
    setTimeout,
    clearTimeout,
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
    new URL("../desktop/preload.cjs", import.meta.url),
    "utf8",
  );
  let api;
  const ipc = Object.assign(new EventEmitter(), {
    invoke: async (...args) => args,
  });
  const listeners = {};
  vm.runInNewContext(preload, {
    process: { isMainFrame: true, platform: "darwin" },
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

  const { transcribe } = require("../desktop/speech.cjs");
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
  assert.equal(await f.invoke("cancelTranscription", "other-recording"), false);
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
