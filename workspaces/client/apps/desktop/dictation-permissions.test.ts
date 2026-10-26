import { present, record } from "./test-support.ts";
import type { DesktopBridge } from "./api.ts";
import type { Session } from "electron";
import type { RecoveryNavigation } from "./renderer-recovery.ts";
import { mainTestSource, mainDependency } from "./main-test-source.ts";
// Execute the real main/preload boundary with native permission prompts mocked.
import assert from "node:assert/strict";
import vm from "node:vm";
import { readFile } from "node:fs/promises";
import { test } from "vitest";

test("main and preload require a trusted local microphone gesture", async () => {
  type PermissionHandler<F> = F extends (
    contents: never,
    ...args: infer Args
  ) => infer Result
    ? (contents: unknown, ...args: Args) => Result
    : never;
  const handlers: {
    request?: PermissionHandler<
      NonNullable<Parameters<Session["setPermissionRequestHandler"]>[0]>
    >;
    check?: PermissionHandler<
      NonNullable<Parameters<Session["setPermissionCheckHandler"]>[0]>
    >;
  } = {};
  const windows: InstanceType<typeof electron.BrowserWindow>[] = [];
  let asked = 0;
  const origin = "http://127.0.0.1:46299";
  const electron = {
    app: {
      setPath() {},
      getPath() {
        return "/tmp";
      },
      setName() {},
      enableSandbox() {},
      getAppPath() {
        return "/tmp/not-installed-studio";
      },
      requestSingleInstanceLock() {
        return true;
      },
      whenReady() {
        return Promise.resolve();
      },
      on() {},
      once() {},
      quit() {},
      isPackaged: false,
    },
    BrowserWindow: class {
      on() {}
      isDestroyed() {
        return false;
      }
      webContents = {
        mainFrame: { url: origin + "/" },
        session: {
          setPermissionRequestHandler(
            fn: PermissionHandler<
              NonNullable<Parameters<Session["setPermissionRequestHandler"]>[0]>
            >,
          ) {
            handlers.request = fn;
          },
          setPermissionCheckHandler(
            fn: PermissionHandler<
              NonNullable<Parameters<Session["setPermissionCheckHandler"]>[0]>
            >,
          ) {
            handlers.check = fn;
          },
        },
        on() {},
        setWindowOpenHandler() {},
      };
      constructor() {
        windows.push(this);
      }
      async loadURL() {}
      showInactive() {}
    },
    ipcMain: { handle() {} },
    dialog: { showErrorBox() {} },
    Menu: {
      setApplicationMenu() {},
      buildFromTemplate() {
        return { getMenuItemById: () => null };
      },
    },
    screen: {
      getPrimaryDisplay: () => ({ id: 1 }),
      getAllDisplays: () => [{ id: 1 }],
    },
    systemPreferences: {
      async askForMediaAccess() {
        asked++;
        return true;
      },
    },
  };
  let ready: () => void = () => {};
  const started = new Promise<void>((resolve) => {
    ready = resolve;
  });
  const context = vm.createContext({
    require: (name: string) =>
      name === "electron"
        ? electron
        : name === "./backend.ts"
          ? { ensureBackend: async () => ({ origin }) }
          : name === "./ui-host.ts"
            ? {
                startUiHost: async () => ({
                  origin: "http://127.0.0.1:46300",
                  close: async () => {},
                }),
              }
            : name === "./recovery.ts"
              ? {
                  configureRecovery: async () => ({ enabled: false }),
                  recoveryPreference: () => false,
                  supervisorPreference: () => false,
                  recoveryStatusLabel: () => "Recovery unavailable",
                  trackDesktopRecovery: () => ({ windowClosing() {} }),
                }
              : name === "./window-state.ts"
                ? {
                    loadWindowState: () => ({
                      bounds: {},
                      minWidth: 800,
                      minHeight: 600,
                    }),
                    trackWindowState: () => ({ restore() {} }),
                  }
                : name === "./renderer-recovery.ts"
                  ? {
                      createRendererRecovery: () => ({
                        handleNavigation(event: RecoveryNavigation) {
                          event.preventDefault();
                        },
                        reload: async () => {},
                      }),
                    }
                  : mainDependency(name),
    module: { exports: {} },
    process: { argv: [], env: {}, platform: "darwin" },
    console: { log: ready, error: console.error },
    __dirname: "/tmp",
    Date,
    Map,
    Error,
    URL,
  });
  vm.runInContext("(function(){" + mainTestSource() + "})()", context);
  await started;
  const window = present(windows.at(-1));
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
  } satisfies Electron.PermissionCheckHandlerHandlerDetails;
  assert.equal(
    present(handlers.check)(window.webContents, "media", origin, check),
    false,
  );
  const allow = (
    details: Parameters<NonNullable<typeof handlers.request>>[3],
  ) =>
    new Promise<boolean>((resolve) =>
      present(handlers.request)(window.webContents, "media", resolve, details),
    );
  assert.equal(await allow(request), false);
  await assert.rejects(
    context.module.exports.nativeAction(
      { ...event, senderFrame: { url: origin + "/" } },
      { method: "requestMicrophone" },
    ),
    /Invalid server frame owner/,
  );
  assert.equal(asked, 0);
  await context.module.exports.nativeAction(event, {
    method: "requestMicrophone",
  });
  assert.equal(asked, 1);
  assert.equal(await allow(request), true);
  assert.equal(
    present(handlers.check)(window.webContents, "media", origin, check),
    true,
  );
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
    present(handlers.check)(window.webContents, "media", origin, {
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
  let bridge: DesktopBridge | undefined;
  let click: ((event: Pick<Event, "isTrusted">) => void) | undefined;
  const calls: unknown[][] = [];
  vm.runInNewContext(
    await readFile(new URL("./preload.cjs", import.meta.url), "utf8"),
    {
      require: () => ({
        contextBridge: {
          exposeInMainWorld(_name: string, value: DesktopBridge) {
            bridge = value;
          },
        },
        ipcRenderer: {
          invoke(...args: unknown[]) {
            calls.push(args);
            return Promise.resolve();
          },
        },
      }),
      process: { isMainFrame: true, platform: "darwin", argv: [] },
      URLSearchParams,
      location: { search: "" },
      performance: { now: () => 100 },
      window: {
        addEventListener(
          name: string,
          handler: (event: Pick<Event, "isTrusted">) => void,
        ) {
          if (name === "click") click = handler;
        },
      },
      Object,
      Promise,
      Error,
    },
  );
  assert(bridge);
  await assert.rejects(bridge.requestMicrophone(), /desktop button/);
  present(click)({ isTrusted: true });
  await bridge.requestMicrophone();
  assert.equal(record(present(calls[0])[1]).method, "requestMicrophone");
  await assert.rejects(bridge.prepareTranscription(), /desktop button/);
  present(click)({ isTrusted: true });
  await bridge.prepareTranscription();
  assert.equal(record(present(calls[1])[1]).method, "prepareTranscription");
  console.log(
    "PASS: real main/preload deny iframe, remote origin, video, missing gesture and invented transcription permits; explicit main-frame audio path accepted with mocked OS permission.",
  );
});
