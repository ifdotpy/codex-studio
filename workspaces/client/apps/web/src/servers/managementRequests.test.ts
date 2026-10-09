import { expect, it, vi } from "vitest";
import { ServerManagementRequests } from "./managementRequests";
function fixture() {
  const records = new Map<string, string>();
  const storage = {
    get length() {
      return records.size;
    },
    key: (index: number) => [...records.keys()][index] ?? null,
    getItem: (key: string) => records.get(key) ?? null,
    setItem: (key: string, value: string) => {
      records.set(key, value);
    },
    removeItem: (key: string) => {
      records.delete(key);
    },
  } as Storage;
  let queue = Promise.resolve();
  const lock = vi.fn((_name: string, run: () => Promise<void>) => {
    const work = queue.then(run);
    queue = work.catch(() => {});
    return work;
  });
  return { records, storage, lock };
}
it("tab B cannot clear tab A's uncertain discover request", async () => {
  const f = fixture();
  const sendA = vi.fn(async (): Promise<void> => {
    throw new TypeError("Lost discover response");
  });
  const sendB = vi.fn(async () => {});
  const a = new ServerManagementRequests("home", f.storage, f.lock, sendA);
  const b = new ServerManagementRequests("home", f.storage, f.lock, sendB);
  await expect(
    a.run({ action: "discover", requestId: "discover-A" }),
  ).rejects.toThrow("Lost discover");
  await expect(
    b.run({ action: "settings", autoPair: false, requestId: "settings-B" }),
  ).rejects.toThrow("Retry the saved");
  expect(a.pending()).toEqual([
    { action: "discover", requestId: "discover-A" },
  ]);
  expect(sendB).not.toHaveBeenCalled();
  sendA.mockImplementationOnce(async () => {});
  await a.run({ action: "discover", requestId: "new-click" });
  expect(sendA.mock.calls[0]).toEqual(sendA.mock.calls[1]);
  expect(f.records.size).toBe(0);
});
it("serializes management mutations across tabs and gives each its own saved key", async () => {
  const f = fixture();
  let finish!: () => void;
  const calls: string[] = [];
  const a = new ServerManagementRequests(
    "home",
    f.storage,
    f.lock,
    async (body) => {
      calls.push(body.requestId);
      await new Promise<void>((resolve) => {
        finish = resolve;
      });
    },
  );
  const b = new ServerManagementRequests(
    "home",
    f.storage,
    f.lock,
    async (body) => {
      calls.push(body.requestId);
    },
  );
  const one = a.run({ action: "discover", requestId: "one" });
  const two = b.run({ action: "settings", autoPair: false, requestId: "two" });
  await vi.waitFor(() => expect(calls).toEqual(["one"]));
  expect([...f.records.keys()]).toEqual([
    "studio-server-management-v1:home:one",
  ]);
  finish();
  await Promise.all([one, two]);
  expect(calls).toEqual(["one", "two"]);
  expect(f.records.size).toBe(0);
});
it("does not remove another operation when its own response succeeds", async () => {
  const f = fixture();
  const manager = new ServerManagementRequests(
    "home",
    f.storage,
    f.lock,
    async () => {
      f.storage.setItem(
        "studio-server-management-v1:home:other",
        JSON.stringify({ action: "discover", requestId: "other" }),
      );
    },
  );
  await manager.run({ action: "settings", autoPair: true, requestId: "own" });
  expect(manager.pending()).toEqual([
    { action: "discover", requestId: "other" },
  ]);
});

it("shows and migrates a legacy uncertain request without changing its identity", async () => {
  const f = fixture();
  const request = { action: "discover" as const, requestId: "legacy-discover" };
  f.storage.setItem(
    "studio-server-discovery-pending-v1",
    JSON.stringify(request),
  );
  const send = vi.fn(async () => {});
  const manager = new ServerManagementRequests("home", f.storage, f.lock, send);
  expect(manager.pending()).toEqual([request]);
  await manager.run(request);
  expect(send).toHaveBeenCalledWith(request);
  expect(f.records.size).toBe(0);
});
