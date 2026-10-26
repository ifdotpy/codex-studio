import type { RawNativeAction } from "./test-support.ts";
import { record } from "./test-support.ts";
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
  let window!: InstanceType<typeof electron.BrowserWindow>,
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
      declare on: () => void;
      declare isDestroyed: () => boolean;
      declare webContents: {
        mainFrame: Pick<Electron.WebFrameMain, "url">;
        session: {
          setPermissionRequestHandler(
            fn: NonNullable<typeof handlers.request>,
          ): void;
          setPermissionCheckHandler(
            fn: NonNullable<typeof handlers.check>,
          ): void;
        };
        on(): void;
        setWindowOpenHandler(): void;
      };
      constructor() {
        window = Object.assign(this);
        this.on = () => {};
        this.isDestroyed = () => false;
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
  let ready!: () => void;
  const started = new Promise<void>((resolve) => {
    ready = resolve;
  });
  const module: { exports: { nativeAction?: RawNativeAction } } = {
    exports: {},
  };
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
    module,
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
  const nativeAction = module.exports.nativeAction!;
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
    handlers.check!(window.webContents, "media", origin, check),
    false,
  );
  const allow = (
    details: Parameters<NonNullable<typeof handlers.request>>[3],
  ) =>
    new Promise<boolean>((resolve) =>
      handlers.request!(window.webContents, "media", resolve, details),
    );
  assert.equal(await allow(request), false);
  await assert.rejects(
    nativeAction(
      { ...event, senderFrame: { url: origin + "/" } },
      { method: "requestMicrophone" },
    ),
    /Invalid server frame owner/,
  );
  assert.equal(asked, 0);
  await nativeAction(event, {
    method: "requestMicrophone",
  });
  assert.equal(asked, 1);
  assert.equal(await allow(request), true);
  assert.equal(
    handlers.check!(window.webContents, "media", origin, check),
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
    handlers.check!(window.webContents, "media", origin, {
      ...check,
      mediaType: "video",
    }),
    false,
  );
  await assert.rejects(
    nativeAction(event, {
      method: "transcribeAudio",
      value: { permit: "invented" },
    }),
    /Transcribe/,
  );
  let bridge: DesktopBridge | undefined,
    click: ((event: Pick<Event, "isTrusted">) => void) | undefined;
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
  await assert.rejects(bridge!.requestMicrophone(), /desktop button/);
  click!({ isTrusted: true });
  await bridge!.requestMicrophone();
  assert.equal(record(calls[0]![1]).method, "requestMicrophone");
  await assert.rejects(bridge!.prepareTranscription(), /desktop button/);
  click!({ isTrusted: true });
  await bridge!.prepareTranscription();
  assert.equal(record(calls[1]![1]).method, "prepareTranscription");
  console.log(
    "PASS: real main/preload deny iframe, remote origin, video, missing gesture and invented transcription permits; explicit main-frame audio path accepted with mocked OS permission.",
  );
});
