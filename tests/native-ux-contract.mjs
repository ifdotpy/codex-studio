import assert from "node:assert/strict";
import { readFile, mkdtemp, writeFile, rm, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import { EventEmitter } from "node:events";
import vm from "node:vm";
const require = createRequire(import.meta.url);
const folder = await mkdtemp(join(tmpdir(), "studio-native-ux-"));
const main = await readFile(
  new URL("../desktop/main.cjs", import.meta.url),
  "utf8",
);
async function fixture() {
  let window, handler, notification, ready;
  const started = new Promise((resolve) => {
    ready = resolve;
  });
  const events = [];
  class Window {
    constructor() {
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
          }
        : name === "./backend.cjs"
          ? { ensureBackend: async () => ({ origin: "http://localhost:1234" }) }
          : name === "./speech.cjs"
            ? require("../desktop/speech.cjs")
            : require(name),
    process: { argv: ["--hidden"], env: {}, platform: "darwin" },
    __dirname: join(folder, "desktop"),
    module: { exports: {} },
    AbortController,
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
  };
}
try {
  let f = await fixture();
  assert.equal(await f.invoke("getNotifications"), false);
  assert.equal(await f.invoke("setNotifications", true), true);
  f = await fixture();
  assert.equal(await f.invoke("getNotifications"), true);
  await assert.rejects(
    f.handler(
      { sender: {}, senderFrame: f.event.senderFrame },
      { method: "getNotifications" },
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
  await f.invoke("setNotifications", false);
  f = await fixture();
  assert.equal(await f.invoke("getNotifications"), false);
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
  await api.getNotifications();
  await assert.rejects(api.setNotifications(true), /button/);
  listeners.click({ isTrusted: true });
  await api.setNotifications(true);
  await assert.rejects(api.cancelTranscription("a"), /button/);
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
