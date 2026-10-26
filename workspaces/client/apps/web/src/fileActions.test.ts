import assert from "node:assert/strict";
import type { DesktopBridge } from "./desktop";
import type { FileTarget } from "./fileActions";
import { afterEach, expect, it, test, vi } from "vitest";

async function setup(remote: boolean, native: boolean) {
  vi.resetModules();
  vi.stubGlobal("location", {
    search: remote ? "?studio-server=remote" : "?studio-server=local",
    origin: "tauri://localhost",
    protocol: "tauri:",
  });
  vi.stubGlobal("window", {});
  const invoke = vi.fn(
    async (_command: string, { request }: { request: { id: string } }) => ({
      id: request.id,
      result: true,
    }),
  );
  vi.stubGlobal("__TAURI__", native ? { core: { invoke } } : undefined);
  return { api: await import("./fileActions"), invoke };
}

afterEach(() => vi.unstubAllGlobals());

it("sends only the local file identity and never creates a renderer permit", async () => {
  const { api, invoke } = await setup(false, true);
  expect(api.canUseNativeFiles()).toBe(true);
  expect(await api.openFile({ agent: "agent", path: "/tmp/test.txt" })).toBe(
    true,
  );
  expect(invoke).toHaveBeenCalledWith("client_request", {
    request: {
      id: expect.any(String),
      method: "desktop.fileAction",
      params: {
        value: {
          action: "open",
          target: { agent: "agent", path: "/tmp/test.txt" },
        },
      },
    },
  });
});

it.each([
  [true, true],
  [false, false],
])(
  "does not resolve remote or browser files through the local native backend",
  async (remote, native) => {
    const { api, invoke } = await setup(remote, native);
    expect(api.canUseNativeFiles()).toBe(false);
    expect(await api.previewFile({ asset: "asset" })).toBe(false);
    expect(invoke).not.toHaveBeenCalled();
  },
);

it("does not let same-window remote targets use local native or Electron file actions", async () => {
  const { api, invoke } = await setup(false, true);
  const local = api.createFileActions("local");
  const remote = api.createFileActions("remote");
  const delayedRemoteOpen = () =>
    remote.openFile({ agent: "same-id", path: "/tmp/same-path" });
  expect(local.canUseNativeFiles()).toBe(true);
  expect(remote.canUseNativeFiles()).toBe(false);
  await local.openFile({ agent: "same-id", path: "/tmp/same-path" });
  expect(await delayedRemoteOpen()).toBe(false);
  expect(invoke).toHaveBeenCalledTimes(1);
  const fileAction = vi.fn();
  vi.stubGlobal("window", { codexDesktop: { fileAction } });
  expect(remote.canUseNativeFiles()).toBe(false);
  expect(await remote.revealFile({ asset: "same-id" })).toBe(false);
  expect(fileAction).not.toHaveBeenCalled();
});

const originalWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
const originalNavigator = Object.getOwnPropertyDescriptor(
  globalThis,
  "navigator",
);
const originalDocument = Object.getOwnPropertyDescriptor(
  globalThis,
  "document",
);

test("file actions preserve the native gesture and browser save paths", async () => {
  try {
    const { saveFile, openFile, revealFile, previewFile, canUseNativeFiles } =
      await import("./fileActions.ts");
    const calls: unknown[] = [];
    const saves: Parameters<NonNullable<DesktopBridge["saveFile"]>>[0][] = [];
    const desktop = {
      codexDesktop: {
        saveFile: (
          value: Parameters<NonNullable<DesktopBridge["saveFile"]>>[0],
        ) => {
          saves.push(value);
          calls.push(["save", value]);
          return Promise.resolve(false);
        },
        fileAction: (
          value: Parameters<NonNullable<DesktopBridge["fileAction"]>>[0],
        ) => {
          calls.push(["file", value]);
          return Promise.resolve(true);
        },
      },
    };
    vi.stubGlobal("window", desktop);
    vi.stubGlobal("__TAURI__", undefined);
    assert.equal(canUseNativeFiles(), true);
    const bytes = Uint8Array.of(0, 1, 2, 3).subarray(1, 3);
    const saving = saveFile({
      name: "../report.md",
      mime: "text/markdown",
      data: bytes,
    });
    assert.equal(
      calls.length,
      1,
      "Native gesture must be consumed before the first await",
    );
    assert.equal(saves[0]?.name, "report.md");
    assert.deepEqual(
      [...new Uint8Array(saves[0]?.data ?? new ArrayBuffer(0))],
      [1, 2],
    );
    assert.equal(await saving, false, "Cancellation must not become success");
    for (const action of [openFile, revealFile, previewFile])
      await action({
        agent: "a",
        path: "report.md",
        line: 30,
      } satisfies FileTarget & { line: number });
    assert.deepEqual(
      calls.slice(1).map((row) => {
        assert.ok(Array.isArray(row));
        return row[1];
      }),
      ["open", "reveal", "preview"].map((action) => ({
        action,
        target: { agent: "a", path: "report.md" },
      })),
    );
    await assert.rejects(saveFile({ name: "..", data: "x" }), /file name/);
    desktop.codexDesktop.saveFile = async () => {
      throw new Error("fixture write failure");
    };
    await assert.rejects(
      saveFile({ name: "report.md", data: "x" }),
      /write failure/,
    );
    vi.stubGlobal("window", {});
    assert.equal(canUseNativeFiles(), false);
    assert.equal(await openFile({ agent: "a", path: "report.md" }), false);
    let shared: File | undefined;
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      value: {
        canShare: ({ files }: ShareData) => files?.length === 1,
        share: async ({ files }: ShareData) => {
          shared = files?.[0];
        },
      },
    });
    assert.equal(
      await saveFile({
        name: "report.md",
        mime: "text/markdown",
        data: "# Progress",
      }),
      true,
    );
    assert.ok(shared);
    assert.equal(shared.name, "report.md");
    assert.equal(shared.type, "text/markdown");
    assert.equal(await shared.text(), "# Progress");
    navigator.share = async () => {
      throw new DOMException("User cancelled", "AbortError");
    };
    assert.equal(await saveFile({ name: "report.md", data: "x" }), false);
    navigator.share = async () => {
      throw new Error("fixture sharing failure");
    };
    await assert.rejects(
      saveFile({ name: "report.md", data: "x" }),
      /sharing failure/,
    );
    const link = {
      href: "",
      download: "",
      click() {
        calls.push(["browser-save", this.download]);
      },
      remove() {},
    };
    vi.stubGlobal("document", {
      createElement: () => link,
      body: { append() {} },
    });
    navigator.canShare = () => false;
    const originalTimeout = globalThis.setTimeout;
    let release: (() => void) | undefined;
    vi.stubGlobal("setTimeout", (callback: () => void, delay: number) => {
      assert.equal(delay, 60000);
      release = callback;
      return 0;
    });
    try {
      assert.equal(await saveFile({ name: "report.md", data: "x" }), true);
      assert.deepEqual(calls.at(-1), ["browser-save", "report.md"]);
      assert.match(link.href, /^blob:/);
      assert.ok(release);
      release();
    } finally {
      globalThis.setTimeout = originalTimeout;
    }
    console.log(
      "Shared file actions PASS: native synchronous gesture, exact bytes, cancellation/errors, target identity, PWA share, explicit browser save fallback",
    );
  } finally {
    for (const [name, descriptor] of [
      ["window", originalWindow],
      ["navigator", originalNavigator],
      ["document", originalDocument],
    ] as const) {
      if (descriptor) Object.defineProperty(globalThis, name, descriptor);
      else Reflect.deleteProperty(globalThis, name);
    }
  }
});
