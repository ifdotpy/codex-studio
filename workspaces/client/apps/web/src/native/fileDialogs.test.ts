import { afterEach, expect, it, vi } from "vitest";

async function setup(remote = false) {
  vi.resetModules();
  vi.stubGlobal("location", {
    search: remote ? "?studio-server=remote" : "",
    protocol: "tauri:",
    origin: "tauri://localhost",
  });
  vi.stubGlobal("window", {});
  const invoke = vi.fn(async (_command, { request }) => ({
    id: request.id,
    result:
      request.method === "desktop.pickDirectory"
        ? null
        : [
            {
              name: "binary.bin",
              mime: "application/octet-stream",
              base64: "AAD/",
            },
          ],
  }));
  vi.stubGlobal("__TAURI__", { core: { invoke } });
  return { api: await import("./fileDialogs"), invoke };
}
afterEach(() => vi.unstubAllGlobals());

it("preserves picker cancellation and exact attachment bytes", async () => {
  const { api, invoke } = await setup();
  expect(await api.pickDirectory()).toBeNull();
  const files = await api.pickFiles();
  expect(files[0].name).toBe("binary.bin");
  expect([...new Uint8Array(await files[0].arrayBuffer())]).toEqual([
    0, 0, 255,
  ]);
  expect(invoke.mock.calls.map(([, value]) => value.request.params)).toEqual([
    {},
    {},
  ]);
});
it("keeps remote views on their browser picker", async () => {
  const { api, invoke } = await setup(true);
  expect(api.canPickDirectory()).toBe(false);
  expect(api.canPickFiles()).toBe(false);
  await expect(api.pickFiles()).rejects.toThrow("unavailable");
  expect(invoke).not.toHaveBeenCalled();
});
it("does not repeat a rejected native dialog request", async () => {
  const { api, invoke } = await setup();
  invoke.mockRejectedValueOnce(new Error("Use a desktop button"));
  await expect(api.pickFiles()).rejects.toThrow("Use a desktop button");
  expect(invoke).toHaveBeenCalledOnce();
});
it("reveals a local path through native IPC and refuses a remote view", async () => {
  const local = await setup();
  await local.api.revealPath("/tmp/folder");
  expect(local.invoke.mock.calls[0][1].request).toMatchObject({
    method: "desktop.revealPath",
    params: { value: "/tmp/folder" },
  });
  const remote = await setup(true);
  await expect(remote.api.revealPath("/tmp/folder")).rejects.toThrow(
    "unavailable",
  );
  expect(remote.invoke).not.toHaveBeenCalled();
});

it("opens vault keys locally and rejects a remote view before IPC", async () => {
  const local = await setup();
  expect(local.api.canShowVaultKeys()).toBe(true);
  await local.api.showVaultKeys("project_1");
  expect(local.invoke.mock.calls[0][1].request).toMatchObject({
    method: "desktop.showVaultKeys",
    params: { value: "project_1" },
  });
  const remote = await setup(true);
  expect(remote.api.canShowVaultKeys()).toBe(false);
  await expect(remote.api.showVaultKeys("project_1")).rejects.toThrow(
    "unavailable",
  );
  expect(remote.invoke).not.toHaveBeenCalled();
});
