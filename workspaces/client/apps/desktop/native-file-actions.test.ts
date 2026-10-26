import type { RawNativeAction } from "./test-support.ts";
import { present, record } from "./test-support.ts";
import type { DesktopBridge } from "./api.ts";
import type { SaveDialogOptions, SaveDialogReturnValue } from "electron";
import type { components } from "../web/src/generated/api.ts";
import { mainTestSource, mainDependency } from "./main-test-source.ts";
import assert from "node:assert/strict";
import * as fs from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import vm from "node:vm";
import { EventEmitter } from "node:events";

import { test } from "vitest";

test("main and preload enforce safe native file action boundaries", async () => {
  const root = await fs.mkdtemp(path.join(tmpdir(), "studio-native-files-"));
  const calls: [string, ...unknown[]][] = [];
  let selected: Partial<SaveDialogReturnValue> | undefined;
  let metadata: Partial<components["schemas"]["FileInfo"]> & {
    error?: string;
  } = {};
  let responseError: string | null = null;
  let fetchGate: Promise<void> | null = null;
  let writeFails = false,
    navigateOnWrite = false;
  const origin = "http://127.0.0.1:4620";
  const mainFrame = { url: origin + "/" };
  const contents = { mainFrame, getURL: () => mainFrame.url };
  const win = {
    webContents: contents,
    destroyed: false,
    isDestroyed() {
      return this.destroyed;
    },
    previewFile: (...args: Parameters<Electron.BrowserWindow["previewFile"]>) =>
      calls.push(["preview", ...args]),
  };
  const dialog = {
    showSaveDialog: async (owner: typeof win, options: SaveDialogOptions) => {
      assert.equal(owner, win);
      calls.push(["save-dialog", options]);
      return selected || { canceled: true };
    },
    showErrorBox: (...args: Parameters<Electron.Dialog["showErrorBox"]>) =>
      calls.push(["error", ...args]),
  };
  const shell = {
    showItemInFolder: (file: string) => calls.push(["reveal", file]),
    openPath: async (file: string) => {
      calls.push(["open", file]);
      return responseError || "";
    },
  };
  const source = mainTestSource();
  const sandbox: {
    actions?: {
      setup: (window: typeof win, connection: { origin: string }) => void;
      nativeAction: RawNativeAction;
      nativeDownload: (
        event: Pick<Electron.Event, "preventDefault">,
        item: Pick<
          Electron.DownloadItem,
          "getURL" | "getFilename" | "setSaveDialogOptions"
        > &
          Pick<NodeJS.EventEmitter, "once">,
        sender: unknown,
      ) => void;
    };
  } & Record<string, unknown> = {
    require: (name: string) =>
      name === "electron"
        ? {
            app: {
              setPath() {},
              setName() {},
              enableSandbox() {},
              getPath: () => root,
            },
            dialog,
            shell,
          }
        : name === "node:fs/promises"
          ? {
              ...fs,
              writeFile: async (...args: Parameters<typeof fs.writeFile>) => {
                if (writeFails) throw new Error("fixture disk full");
                const result = await fs.writeFile(...args);
                if (navigateOnWrite) mainFrame.url = "https://elsewhere.test/";
                return result;
              },
            }
          : mainDependency(name),
    process: { argv: [], env: {}, platform: "darwin" },
    ArrayBuffer,
    Buffer,
    URLSearchParams,
    AbortSignal,
    console,
    fetch: async (url: string, options: RequestInit) => {
      const requested = new URL(url);
      assert.equal(requested.origin, origin);
      assert.equal(requested.pathname, "/api/file-info");
      assert.equal(options.redirect, "error");
      assert(options.signal instanceof AbortSignal);
      calls.push(["metadata", requested.search]);
      if (fetchGate) await fetchGate;
      return { ok: !metadata.error, json: async () => metadata };
    },
  };
  const context = vm.createContext(sandbox);
  vm.runInContext(
    "(function(){\n" +
      source.slice(0, source.indexOf("async function start()")) +
      `
    globalThis.actions = { nativeAction, nativeDownload, setup(window, connection) { win = window; backend = connection; } };
  })()`,
    context,
  );
  const actions = present(sandbox.actions);
  actions.setup(win, { origin });
  const event = { sender: contents, senderFrame: mainFrame };
  const invoke = (
    method: string,
    value?: unknown,
    caller: { sender: unknown; senderFrame: unknown } = event,
  ) => actions.nativeAction(caller, { method, value });
  const data = new TextEncoder().encode("saved content").buffer;
  try {
    await assert.rejects(
      invoke(
        "saveFile",
        { name: "x", data },
        { sender: {}, senderFrame: mainFrame },
      ),
      /restricted|Invalid server frame owner/,
    );
    await assert.rejects(
      invoke(
        "saveFile",
        { name: "x", data },
        { sender: contents, senderFrame: { url: origin + "/" } },
      ),
      /restricted|Invalid server frame owner/,
    );
    await assert.rejects(
      invoke("saveFile", { name: "x", data: { byteLength: 3 } }),
      /64 MiB/,
    );
    await assert.rejects(
      invoke("saveFile", {
        name: "x",
        data: new ArrayBuffer(64 * 1024 * 1024 + 1),
      }),
      /64 MiB/,
    );
    await assert.rejects(invoke("saveFile", { name: "..", data }), /file name/);
    assert.equal(
      await invoke("saveFile", { name: "../../report.txt", data }),
      false,
    );
    assert.equal(
      record(present(calls.at(-1))[1]).defaultPath,
      path.join(root, "report.txt"),
    );
    const output = path.join(root, "report.txt");
    await fs.writeFile(output, "old");
    selected = { canceled: false, filePath: output };
    writeFails = true;
    await assert.rejects(
      invoke("saveFile", { name: "report.txt", data }),
      /disk full/,
    );
    assert.equal(await fs.readFile(output, "utf8"), "old");
    assert.deepEqual(
      (await fs.readdir(root)).filter((name) =>
        name.startsWith(".studio-save-"),
      ),
      [],
    );
    writeFails = false;
    navigateOnWrite = true;
    await assert.rejects(
      invoke("saveFile", { name: "report.txt", data }),
      /restricted|Invalid server frame owner/,
    );
    assert.equal(await fs.readFile(output, "utf8"), "old");
    assert.deepEqual(
      (await fs.readdir(root)).filter((name) =>
        name.startsWith(".studio-save-"),
      ),
      [],
    );
    mainFrame.url = origin + "/";
    navigateOnWrite = false;
    assert.equal(await invoke("saveFile", { name: "report.txt", data }), true);
    assert.equal(await fs.readFile(output, "utf8"), "saved content");

    metadata = {
      path: await fs.realpath(output),
      name: "report.txt",
      mime: "text/plain",
      size: 13,
    };
    const target = { agent: "agent", path: "report.txt" };
    for (const action of ["open", "reveal", "preview"])
      assert.equal(await invoke("fileAction", { action, target }), true);
    assert(
      calls.some((row) => row[0] === "preview" && row[1] === metadata.path),
    );
    responseError = "fixture default application unavailable";
    await assert.rejects(
      invoke("fileAction", { action: "open", target }),
      /application unavailable/,
    );
    responseError = null;
    for (const invalid of [
      { path: "/etc/passwd" },
      { asset: "a", path: "/etc/passwd" },
      { agent: "a", path: "x", url: "https://bad.test/" },
    ])
      await assert.rejects(
        invoke("fileAction", { action: "open", target: invalid }),
        /workspace file/,
      );
    metadata = { error: "outside authorized workspace" };
    await assert.rejects(
      invoke("fileAction", { action: "open", target }),
      /authorized workspace/,
    );
    metadata = { path: "/not-a-real-file", name: "missing.txt" };
    await assert.rejects(
      invoke("fileAction", { action: "open", target }),
      /ENOENT/,
    );

    const executable = path.join(root, "run.command");
    await fs.writeFile(executable, "#!/bin/sh\nexit 0");
    metadata = { path: await fs.realpath(executable), name: "run.command" };
    await assert.rejects(
      invoke("fileAction", { action: "open", target }),
      /Preview/,
    );
    const openCount = calls.filter((row) => row[0] === "open").length;
    assert.equal(
      await invoke("fileAction", { action: "preview", target }),
      true,
    );
    assert.equal(calls.filter((row) => row[0] === "open").length, openCount);
    await fs.chmod(output, 0o700);
    metadata = { path: await fs.realpath(output), name: "report.txt" };
    await assert.rejects(
      invoke("fileAction", { action: "open", target }),
      /Preview/,
    );
    await fs.chmod(output, 0o600);
    let resume: () => void = () => {};
    fetchGate = new Promise<void>((resolve) => {
      resume = resolve;
    });
    const pending = invoke("fileAction", { action: "open", target });
    mainFrame.url = "https://elsewhere.test/";
    resume();
    await assert.rejects(pending, /restricted|Invalid server frame owner/);
    mainFrame.url = origin + "/";
    fetchGate = null;

    function download(url: string, sender: unknown = contents) {
      let prevented = false,
        options: SaveDialogOptions | undefined;
      const item = Object.assign(new EventEmitter(), {
        getURL: () => url,
        getFilename: () => "export.csv",
        setSaveDialogOptions: (value: SaveDialogOptions) => {
          options = value;
        },
      });
      actions.nativeDownload(
        {
          preventDefault: () => {
            prevented = true;
          },
        },
        item,
        sender,
      );
      return { item, prevented, options };
    }
    for (const url of [
      "https://remote.test/file",
      "file:///etc/passwd",
      "data:text/plain,bad",
    ])
      assert.equal(download(url).prevented, true);
    assert.equal(download("blob:" + origin + "/id", {}).prevented, true);
    win.destroyed = true;
    assert.equal(download("blob:" + origin + "/id").prevented, true);
    win.destroyed = false;
    const transfer = download("blob:" + origin + "/export");
    assert.equal(transfer.prevented, false);
    assert.equal(present(transfer.options).title, "Save As");
    assert.equal(
      present(transfer.options).defaultPath,
      path.join(root, "export.csv"),
    );
    let errors = calls.filter((row) => row[0] === "error").length;
    transfer.item.emit("done", {}, "cancelled");
    assert.equal(calls.filter((row) => row[0] === "error").length, errors);
    download(origin + "/export").item.emit("done", {}, "interrupted");
    assert.equal(calls.filter((row) => row[0] === "error").length, errors + 1);

    let bridge: DesktopBridge | undefined;
    const listeners: Record<string, (event: Pick<Event, "isTrusted">) => void> =
      {};
    const ipc = {
      invoke: async (_channel: string, request: unknown) => request,
    };
    const preload = await fs.readFile(
      new URL("./preload.cjs", import.meta.url),
      "utf8",
    );
    vm.runInNewContext(preload, {
      require: () => ({
        contextBridge: {
          exposeInMainWorld: (_name: string, value: DesktopBridge) => {
            bridge = value;
          },
        },
        ipcRenderer: ipc,
      }),
      process: { isMainFrame: true, platform: "darwin", argv: [] },
      URLSearchParams,
      location: { search: "" },
      performance: { now: () => 20 },
      window: {
        addEventListener: (
          name: string,
          listener: (event: Pick<Event, "isTrusted">) => void,
        ) => {
          listeners[name] = listener;
        },
      },
    });
    assert(bridge);
    assert(bridge.saveFile && bridge.fileAction);
    await assert.rejects(bridge.saveFile({ name: "x", data }), /button/);
    present(listeners.click)({ isTrusted: false });
    await assert.rejects(
      bridge.fileAction({ action: "open", target }),
      /button/,
    );
    present(listeners.click)({ isTrusted: true });
    assert.equal(
      record(await bridge.saveFile({ name: "x", data })).method,
      "saveFile",
    );
    await assert.rejects(
      bridge.fileAction({ action: "open", target }),
      /button/,
    );
    present(listeners.keydown)({ isTrusted: true });
    assert.equal(
      record(await bridge.fileAction({ action: "preview", target })).method,
      "fileAction",
    );
    console.log(
      "Native file actions PASS: save/cancel/failure, sender and backend boundaries, no executable launch, Quick Look, download dialog, gesture consumption",
    );
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});
