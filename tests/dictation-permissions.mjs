// Execute the real main/preload boundary with native permission prompts mocked.
import assert from "node:assert/strict";
import vm from "node:vm";
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
const require = createRequire(import.meta.url),
  handlers = {};
let window,
  asked = 0;
const origin = "http://127.0.0.1:46299";
const electron = {
  app: {
    setPath() {},
    getPath() {
      return "/tmp";
    },
    setName() {},
    enableSandbox() {},
    requestSingleInstanceLock() {
      return true;
    },
    whenReady() {
      return Promise.resolve();
    },
    on() {},
    quit() {},
    isPackaged: false,
  },
  BrowserWindow: class {
    constructor() {
      window = this;
      this.webContents = {
        mainFrame: { url: origin + "/" },
        session: {
          setPermissionRequestHandler(fn) {
            handlers.request = fn;
          },
          setPermissionCheckHandler(fn) {
            handlers.check = fn;
          },
        },
        on() {},
        setWindowOpenHandler() {},
      };
    }
    async loadURL() {}
    showInactive() {}
  },
  ipcMain: { handle() {} },
  Menu: { setApplicationMenu() {}, buildFromTemplate() {} },
  systemPreferences: {
    async askForMediaAccess() {
      asked++;
      return true;
    },
  },
};
let ready;
const started = new Promise((resolve) => { ready = resolve; });
const context = vm.createContext({
  require: (name) =>
    name === "electron"
      ? electron
      : name === "./backend.cjs"
        ? { ensureBackend: async () => ({ origin }) }
        : require(name),
  module: { exports: {} },
  process: { argv: [], env: {}, platform: "darwin" },
  console: { log: ready, error: console.error },
  __dirname: "/tmp",
  Date,
  Map,
  Error,
  URL,
});
vm.runInContext(
  "(function(){" +
    (await readFile(new URL("../desktop/main.cjs", import.meta.url), "utf8")) +
    "})()",
  context,
);
await started;
const event = {
  sender: window.webContents,
  senderFrame: window.webContents.mainFrame,
};
const request = {
  isMainFrame: true,
  requestingUrl: origin + "/",
  mediaTypes: ["audio"],
};
const check = {
  isMainFrame: true,
  requestingUrl: origin + "/",
  mediaType: "audio",
};
assert.equal(handlers.check(window.webContents, "media", origin, check), false);
const allow = (details) =>
  new Promise((resolve) =>
    handlers.request(window.webContents, "media", resolve, details),
  );
assert.equal(await allow(request), false);
await assert.rejects(
  context.module.exports.nativeAction(
    { ...event, senderFrame: { url: origin + "/" } },
    { method: "requestMicrophone" },
  ),
  /main frame/,
);
assert.equal(asked, 0);
await context.module.exports.nativeAction(event, {
  method: "requestMicrophone",
});
assert.equal(asked, 1);
assert.equal(await allow(request), true);
assert.equal(handlers.check(window.webContents, "media", origin, check), true);
assert.equal(await allow({ ...request, isMainFrame: false }), false);
assert.equal(
  await allow({ ...request, requestingUrl: "https://evil.invalid/" }),
  false,
);
assert.equal(
  await allow({ ...request, mediaTypes: ["audio", "video"] }),
  false,
);
assert.equal(
  handlers.check(window.webContents, "media", origin, {
    ...check,
    mediaType: "video",
  }),
  false,
);
await assert.rejects(
  context.module.exports.nativeAction(event, {
    method: "transcribeAudio",
    value: { permit: "invented" },
  }),
  /Transcribe/,
);
let bridge, click;
const calls = [];
vm.runInNewContext(
  await readFile(new URL("../desktop/preload.cjs", import.meta.url), "utf8"),
  {
    require: () => ({
      contextBridge: {
        exposeInMainWorld(_name, value) {
          bridge = value;
        },
      },
      ipcRenderer: {
        invoke(...args) {
          calls.push(args);
          return Promise.resolve();
        },
      },
    }),
    process: { isMainFrame: true, platform: "darwin" },
    performance: { now: () => 100 },
    window: {
      addEventListener(name, handler) {
        if (name === "click") click = handler;
      },
    },
    Object,
    Promise,
    Error,
  },
);
await assert.rejects(bridge.requestMicrophone(), /desktop button/);
click({ isTrusted: true });
await bridge.requestMicrophone();
assert.equal(calls[0][1].method, "requestMicrophone");
await assert.rejects(bridge.prepareTranscription(), /desktop button/);
click({ isTrusted: true });
await bridge.prepareTranscription();
assert.equal(calls[1][1].method, "prepareTranscription");
console.log(
  "PASS: real main/preload deny iframe, remote origin, video, missing gesture and invented transcription permits; explicit main-frame audio path accepted with mocked OS permission.",
);
