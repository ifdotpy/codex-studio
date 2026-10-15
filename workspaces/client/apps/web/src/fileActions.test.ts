import { afterEach, expect, it, vi } from "vitest";

async function setup(remote: boolean, native: boolean) {
  vi.resetModules();
  vi.stubGlobal("location", {
    search: remote ? "?studio-server=remote" : "?studio-server=local",
    origin: "tauri://localhost",
    protocol: "tauri:",
  });
  vi.stubGlobal("window", {});
  const invoke = vi.fn(async (_command, { request }) => ({
    id: request.id,
    result: true,
  }));
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
