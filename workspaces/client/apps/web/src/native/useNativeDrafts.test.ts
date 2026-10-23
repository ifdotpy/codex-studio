import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type {
  ResourceConnectionState,
  ResourceStreamState,
  ResourceVersion,
} from "../sync/resourceEvents";
import type { DraftView } from "./draftState";

const mocks = vi.hoisted(() => ({
  effects: [] as Array<() => void | (() => void)>,
  cleanups: [] as Array<() => void>,
  changes: new Set<(version?: ResourceVersion) => void>(),
  connections: new Set<
    (status: ResourceConnectionState, stream: ResourceStreamState) => void
  >(),
  resumes: new Set<() => void>(),
  request: vi.fn(),
}));
vi.mock("react", () => ({
  useRef: (current: unknown) => ({ current }),
  useState: (value: unknown) => [value, vi.fn()],
  useCallback: (callback: unknown) => callback,
  useEffect: (effect: () => void | (() => void)) => mocks.effects.push(effect),
}));
vi.mock("./client", () => ({ nativeRequest: mocks.request }));
vi.mock("./legacyDrafts", () => ({ importLegacyDrafts: async () => {} }));
vi.mock("./draftJournal", () => ({ nativeDraftJournal: () => undefined }));
vi.mock("./closeState", () => ({ guardNativeClose: () => () => {} }));
vi.mock("../sync/resume", () => ({
  onResume: (callback: () => void) => {
    mocks.resumes.add(callback);
    return () => mocks.resumes.delete(callback);
  },
}));
vi.mock("../sync/resourceEvents", () => ({
  watchResourceChanges: (
    resource: unknown,
    callback: (version?: ResourceVersion) => void,
  ) => {
    expect(resource).toEqual({ kind: "drafts" });
    mocks.changes.add(callback);
    return () => mocks.changes.delete(callback);
  },
  watchResourceConnection: (
    callback: (
      status: ResourceConnectionState,
      stream: ResourceStreamState,
    ) => void,
  ) => {
    mocks.connections.add(callback);
    callback("connecting", "connecting");
    return () => mocks.connections.delete(callback);
  },
}));
import { useNativeDrafts } from "./useNativeDrafts";
const view: DraftView = {
  drafts: {},
  conflicts: [],
  workspaceId: "workspace",
  revision: 0,
};
async function settle() {
  await vi.advanceTimersByTimeAsync(0);
}
function mount() {
  const hook = useNativeDrafts();
  for (const effect of mocks.effects.splice(0)) {
    const cleanup = effect();
    if (cleanup) mocks.cleanups.push(cleanup);
  }
  return hook;
}
function notify(revision: number) {
  for (const callback of mocks.changes) callback({ epoch: "epoch", revision });
}
function connection(status: ResourceConnectionState) {
  for (const callback of mocks.connections)
    callback(status, status === "live" ? "open" : "connecting");
}
function refreshCount() {
  return mocks.request.mock.calls.filter(
    ([method]) => method === "drafts.refresh",
  ).length;
}
beforeEach(() => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
  vi.stubGlobal("window", new EventTarget());
  vi.stubGlobal(
    "document",
    Object.assign(new EventTarget(), { hidden: false }),
  );
  mocks.request.mockImplementation(async (method: string) =>
    method === "identity.get" ? { workspaceId: "workspace" } : view,
  );
});
afterEach(() => {
  for (const cleanup of mocks.cleanups.splice(0)) cleanup();
  mocks.effects.length = 0;
  mocks.request.mockReset();
  mocks.changes.clear();
  mocks.connections.clear();
  mocks.resumes.clear();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});
it("makes no native requests while idle, including a hidden window", async () => {
  mount();
  await settle();
  expect(refreshCount()).toBe(1);
  const initial = mocks.request.mock.calls.length;
  await vi.advanceTimersByTimeAsync(60_000);
  expect(mocks.request).toHaveBeenCalledTimes(initial);
  Object.defineProperty(document, "hidden", { value: true });
  await vi.advanceTimersByTimeAsync(60_000);
  expect(mocks.request).toHaveBeenCalledTimes(initial);
});
it("refreshes once for a notification, reconnect, or resume and removes listeners", async () => {
  mount();
  await settle();
  notify(1);
  await settle();
  expect(refreshCount()).toBe(2);
  connection("degraded");
  connection("live");
  await settle();
  expect(refreshCount()).toBe(3);
  for (const callback of mocks.resumes) callback();
  await settle();
  expect(refreshCount()).toBe(4);
  for (const cleanup of mocks.cleanups.splice(0)) cleanup();
  expect(mocks.changes.size).toBe(0);
  expect(mocks.connections.size).toBe(0);
  expect(mocks.resumes.size).toBe(0);
});
it("retains notifications during a request and coalesces them into one follow-up", async () => {
  const hook = mount();
  await settle();
  let finish!: (view: DraftView) => void;
  mocks.request.mockImplementationOnce(
    () =>
      new Promise<DraftView>((resolve) => {
        finish = resolve;
      }),
  );
  notify(1);
  await settle();
  expect(refreshCount()).toBe(2);
  notify(2);
  notify(3);
  await settle();
  expect(refreshCount()).toBe(2);
  finish({ ...view, drafts: { room: "remote" }, revision: 1 });
  await settle();
  expect(refreshCount()).toBe(3);
  expect(hook.getDraft("room")).toBe("remote");
});
it("refreshes after a committed local edit without refreshing for unchanged text", async () => {
  const hook = mount();
  await settle();
  mocks.request.mockResolvedValue({
    ...view,
    drafts: { room: "local" },
    revision: 1,
  });
  hook.setDrafts({ room: "local" });
  await settle();
  expect(refreshCount()).toBe(2);
  expect(hook.getDraft("room")).toBe("local");
  hook.setDrafts({ room: "local" });
  await settle();
  expect(refreshCount()).toBe(2);
});

it("coalesces a reconnect and draft baseline delivered together", async () => {
  mount();
  await settle();
  connection("live");
  notify(1);
  await settle();
  expect(refreshCount()).toBe(2);
});
it("refreshes after a local conflict dismissal", async () => {
  const hook = mount();
  await settle();
  hook.dismissDraft({
    id: "conflict",
    session: "room",
    text: "old",
    device: "device",
    updated: 1,
  });
  await settle();
  expect(
    mocks.request.mock.calls.filter(([method]) => method === "drafts.dismiss"),
  ).toHaveLength(1);
  expect(refreshCount()).toBe(2);
});
it("recovers local drafts offline even in a hidden window and retries on resume", async () => {
  Object.defineProperty(document, "hidden", { value: true });
  vi.stubGlobal("navigator", { onLine: false });
  mocks.request.mockImplementation(async (method: string) => {
    if (method === "identity.get") return { workspaceId: "workspace" };
    if (method === "drafts.refresh") throw new Error("offline");
    return { ...view, drafts: { room: "saved" } };
  });
  const hook = mount();
  await settle();
  expect(hook.getDraft("room")).toBe("saved");
  const initial = mocks.request.mock.calls.length;
  await vi.advanceTimersByTimeAsync(60_000);
  expect(mocks.request).toHaveBeenCalledTimes(initial);
  mocks.request.mockResolvedValue({ ...view, drafts: { room: "saved" } });
  for (const callback of mocks.resumes) callback();
  await settle();
  expect(refreshCount()).toBe(2);
  expect(hook.getDraft("room")).toBe("saved");
});
it("does not run a queued follow-up after unmount", async () => {
  mount();
  await settle();
  let finish!: (view: DraftView) => void;
  mocks.request.mockImplementationOnce(
    () =>
      new Promise<DraftView>((resolve) => {
        finish = resolve;
      }),
  );
  notify(1);
  await settle();
  notify(2);
  for (const cleanup of mocks.cleanups.splice(0)) cleanup();
  finish(view);
  await settle();
  expect(refreshCount()).toBe(2);
});
