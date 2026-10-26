import { parsedRecord } from "./test-support.ts";
import { nodeFixture } from "./test-support.ts";
import { present, record } from "./test-support.ts";
import type { RawNativeAction } from "./test-support.ts";
import type { DesktopBridge } from "./api.ts";
import type { NativeFrame, NativeEvent } from "./main.ts";
import type { NotificationConstructorOptions } from "electron";
import { transcribe } from "./speech.ts";
import type { SpeechProgress } from "./speech.ts";
import { serverFrameHost } from "./frame-owner.ts";
import { mainTestSource, mainDependency } from "./main-test-source.ts";
import assert from "node:assert/strict";
import { readFile, mkdtemp, writeFile, rm, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { EventEmitter } from "node:events";
import vm from "node:vm";
import { test } from "vitest";

test("desktop native settings, notification and speech boundaries work with fixtures", async () => {
  const folder = await mkdtemp(join(tmpdir(), "studio-native-ux-"));
  const main = mainTestSource();
  async function fixture() {
    const windows: Window[] = [];
    const notices: Notice[] = [];
    let handler: RawNativeAction | undefined;
    let ready: () => void = () => {};
    const display = {
      id: 1,
      workArea: { x: 0, y: 0, width: 1440, height: 960 },
    };
    const started = new Promise<void>((resolve) => {
      ready = resolve;
    });
    const events: unknown[][] = [];
    const intervals = new Set<() => void>();
    let backendBuild = "installed";
    let notificationFailure: string | null = null;
    class Window extends EventEmitter {
      webContents = Object.assign(new EventEmitter(), {
        mainFrame: {
          url: "http://localhost:1234/",
          parent: null,
          frames: [] as NativeFrame[],
          processId: 1,
          routingId: 1,
          isDestroyed: () => false,
          send: (...args: unknown[]) => this.webContents.send(...args),
        },
        session: {
          setPermissionRequestHandler() {},
          setPermissionCheckHandler() {},
        },
        setWindowOpenHandler() {},
        isDestroyed: () => false,
        getURL: () => "http://localhost:1234/",
        send: (...args: unknown[]) => {
          events.push(args);
          this.webContents.emit("sent", ...args);
        },
      });
      constructor() {
        super();
        windows.push(this);
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
      options: NotificationConstructorOptions;
      constructor(options: NotificationConstructorOptions) {
        super();
        notices.push(this);
        this.options = options;
      }
      show() {
        events.push(["notice"]);
        if (notificationFailure) this.emit("failed", {}, notificationFailure);
        else this.emit("show");
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
      require: (name: string) =>
        name === "electron"
          ? {
              app,
              BrowserWindow: Window,
              dialog: { showErrorBox() {} },
              ipcMain: {
                handle: (_key: string, value: RawNativeAction) => {
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
          : name === "./backend.ts"
            ? {
                ensureBackend: async () => ({
                  origin: "http://localhost:1234",
                  stateDir: folder,
                }),
                identity: async (origin: string, state: string) => {
                  assert.equal(origin, "http://localhost:1234");
                  assert.equal(state, folder);
                  return { backendBuild };
                },
                updateStatus: (
                  _resources: string,
                  running: { backendBuild: string },
                ) => ({
                  updateRequired: running.backendBuild !== "installed",
                  availableBackendBuild: "installed",
                }),
              }
            : name === "./ui-host.ts"
              ? {
                  startUiHost: async () => ({
                    origin: "http://127.0.0.1:1235",
                    close: async () => {},
                  }),
                }
              : name === "./speech.ts"
                ? mainDependency("./speech.ts")
                : name === "./recovery.ts"
                  ? mainDependency("./recovery.ts")
                  : name.startsWith("./")
                    ? mainDependency(name)
                    : mainDependency(name),
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
      setInterval: (callback: () => void) => {
        intervals.add(callback);
        return callback;
      },
      clearInterval: (callback: () => void) => intervals.delete(callback),
      console: { log: ready, error: console.error },
    });
    await started;
    const window = present(windows.at(-1));
    const action = present(handler);
    const event = {
      sender: window.webContents,
      senderFrame: window.webContents.mainFrame,
    } satisfies NativeEvent;
    return {
      invoke: (method: string, value?: unknown) =>
        action(event, { method, value }),
      handler: action,
      event,
      events,
      intervals,
      notification: () => present(notices.at(-1)),
      setBackendBuild: (value: string) => {
        backendBuild = value;
      },
      setNotificationFailure: (value: string | null) => {
        notificationFailure = value;
      },
    };
  }
  try {
    let f = await fixture();
    assert.equal(
      record(await f.invoke("getBackendUpdate")).updateRequired,
      false,
    );
    const serverFrame: NativeFrame = {
      processId: 2,
      routingId: 2,
      frames: [],
      isDestroyed: () => false,
      send() {},
      url: `http://${serverFrameHost("local")}:1235/?studio-server=local&studio-parent=http%3A%2F%2Flocalhost%3A1234`,
      parent: f.event.senderFrame,
    };
    f.event.senderFrame.frames = [serverFrame];
    const serverEvent = { sender: f.event.sender, senderFrame: serverFrame };
    assert.equal(
      record(
        await f.handler(serverEvent, {
          method: "serverNativeAction",
          value: { serverId: "local", method: "getBackendUpdate" },
        }),
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

    const remoteManagementFrame: NativeFrame = {
      processId: 3,
      routingId: 3,
      frames: [],
      isDestroyed: () => false,
      send() {},
      url: `http://${serverFrameHost("remote")}:1235/?studio-server=remote&studio-parent=http%3A%2F%2Flocalhost%3A1234`,
      parent: f.event.senderFrame,
    };
    f.event.senderFrame.frames.push(remoteManagementFrame);
    for (const [owner, origin, frame] of [
      ["local", "http://localhost:1234", serverFrame],
      ["remote", "https://remote.tailnet.ts.net", remoteManagementFrame],
    ] as const) {
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
      record(
        await f.handler(serverEvent, {
          method: "serverCredentialAction",
          value: {
            action: "request",
            serverId: "local",
            streamId: "ordinary",
            url: "http://localhost:1234/api/messages",
            method: "POST",
          },
        }),
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
    assert.equal(
      record(await f.invoke("getBackendUpdate")).updateRequired,
      true,
    );
    f.setBackendBuild("installed");
    assert.equal(
      record(await f.invoke("getBackendUpdate")).updateRequired,
      false,
    );
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
    assert.equal(
      f.notification().options.silent,
      true,
      "ordinary alerts are silent",
    );
    f.notification().emit("click");
    assert.deepEqual(parsedRecord(JSON.stringify(f.events.slice(1))), [
      ["show"],
      ["focus"],
      ["codex-desktop-navigate", target],
    ]);
    await f.invoke("notify", {
      title: "Sign-in needed",
      body: "Sign in at id.example.com. Open the card in this chat.",
      sound: true,
      target,
    });
    assert.equal(
      f.notification().options.silent,
      false,
      "an open sign-in card plays the sound",
    );
    f.setNotificationFailure("Notifications are not allowed");
    await assert.rejects(
      f.invoke("notify", {
        title: "Question",
        body: "Open the question",
        target,
      }),
      {
        message:
          "STUDIO_NOTIFICATIONS_DENIED: Allow notifications for LAYR Studio in this computer's notification settings.",
      },
    );
    f.setNotificationFailure(null);
    assert.equal(await f.invoke("cancelTranscription", "unknown"), false);
    await assert.rejects(
      f.invoke("transcribeAudio", { id: "a", permit: "invalid" }),
      /Transcribe/,
    );

    const preload = await readFile(
      new URL("./preload.cjs", import.meta.url),
      "utf8",
    );
    let api: DesktopBridge | undefined;
    const ipc = Object.assign(new EventEmitter(), {
      invoke: async (...args: unknown[]) => args,
    });
    const listeners: Record<string, (event: Pick<Event, "isTrusted">) => void> =
      {};
    vm.runInNewContext(preload, {
      process: { isMainFrame: true, platform: "darwin", argv: [] },
      URLSearchParams,
      location: { search: "" },
      window: {
        addEventListener: (
          name: string,
          cb: (event: Pick<Event, "isTrusted">) => void,
        ) => {
          listeners[name] = cb;
        },
      },
      performance: { now: () => 10 },
      require: () => ({
        contextBridge: {
          exposeInMainWorld: (_name: string, value: DesktopBridge) => {
            api = value;
          },
        },
        ipcRenderer: ipc,
      }),
    });
    assert(api);
    const updateCall = await present(api.getBackendUpdate)();
    assert.equal(record(updateCall)["0"], "codex-desktop");
    assert.equal(record(record(updateCall)["1"]).method, "getBackendUpdate");
    assert.equal(Reflect.get(api, "getNotifications"), undefined);
    assert.equal(Reflect.get(api, "setNotifications"), undefined);
    present(listeners.click)({ isTrusted: true });
    const cancelCall = await api.cancelTranscription("a");
    assert.equal(record(cancelCall)["0"], "codex-desktop");
    assert.equal(record(record(cancelCall)["1"]).method, "cancelTranscription");
    const received: Parameters<
      Parameters<DesktopBridge["onNavigate"]>[0]
    >[0][] = [];
    const unsubscribe = api.onNavigate((value) => received.push(value));
    ipc.emit("codex-desktop-navigate", {}, target);
    unsubscribe();
    ipc.emit("codex-desktop-navigate", {}, target);
    assert.deepEqual(received, [target]);

    const data = new ArrayBuffer(46),
      view = new DataView(data),
      bytes = new Uint8Array(data);
    for (const [offset, text] of [
      [0, "RIFF"],
      [8, "WAVE"],
      [12, "fmt "],
      [36, "data"],
    ] as const)
      bytes.set(Buffer.from(text), offset);
    for (const [offset, value] of [
      [4, 38],
      [16, 16],
      [24, 16000],
      [28, 32000],
      [40, 2],
    ] as const)
      view.setUint32(offset, value, true);
    for (const [offset, value] of [
      [20, 1],
      [22, 1],
      [32, 2],
      [34, 16],
    ] as const)
      view.setUint16(offset, value, true);
    const helper = join(folder, "helper");
    await writeFile(helper, nodeFixture("speech-progress"), { mode: 0o700 });
    const progress: SpeechProgress[] = [];
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
    await writeFile(helper, nodeFixture("speech-pending"));
    await mkdir(join(folder, "desktop/native"), { recursive: true });
    await writeFile(
      join(folder, "desktop/native/studio-speech"),
      await readFile(helper),
      { mode: 0o700 },
    );
    const permit = await f.invoke("prepareTranscription");
    const reported = new Promise<{
      channel: string;
      value: Parameters<
        Parameters<DesktopBridge["onTranscriptionProgress"]>[0]
      >[0];
    }>((resolve) =>
      f.event.sender.once(
        "sent",
        (
          channel: string,
          value: Parameters<
            Parameters<DesktopBridge["onTranscriptionProgress"]>[0]
          >[0],
        ) => resolve({ channel, value }),
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
        url: `http://${serverFrameHost("remote")}:1235/?studio-server=remote&studio-parent=http%3A%2F%2Flocalhost%3A1234`,
        parent: f.event.senderFrame,
        frames: [],
        processId: 12,
        routingId: 34,
        send: (...args: unknown[]) => f.event.sender.send(...args),
        destroyed: false,
        isDestroyed() {
          return this.destroyed;
        },
      };
      f.event.senderFrame.frames = [remoteFrame];
      const remoteEvent = { sender: f.event.sender, senderFrame: remoteFrame };
      const remoteInvoke = (method: string, value?: unknown) =>
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
      assert.equal(f.intervals.size, 1);
      if (action === "release") await f.invoke("releaseServerView", "remote");
      if (action === "navigate")
        f.event.sender.emit("did-start-navigation", {
          isMainFrame: false,
          frame: remoteFrame,
        });
      if (action === "destroy") {
        remoteFrame.destroyed = true;
        for (const callback of f.intervals) callback();
      }
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
      assert.equal(f.intervals.size, 0);
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
