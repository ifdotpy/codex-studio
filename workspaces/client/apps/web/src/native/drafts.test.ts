import { afterEach, beforeEach, expect, it, vi } from "vitest";
const hooks = vi.hoisted(() => ({
  effects: [] as (() => void | (() => void))[],
  cleanups: [] as (() => void)[],
}));
vi.mock("react", () => ({
  useRef: (current: unknown) => ({ current }),
  useState: (value: unknown) => [value, vi.fn()],
  useCallback: (callback: unknown) => callback,
  useEffect: (effect: () => void | (() => void)) => hooks.effects.push(effect),
}));
vi.mock("./closeState", () => ({ guardNativeClose: () => () => {} }));
import { createNativeDrafts } from "./drafts";
import type { NativeServerContext, NativeSubscriptionEvent } from "./client";

beforeEach(() => vi.useFakeTimers());
afterEach(() => {
  for (const cleanup of hooks.cleanups.splice(0)) cleanup();
  hooks.effects = [];
  vi.useRealTimers();
});
async function settle() {
  await vi.advanceTimersByTimeAsync(0);
}
function fixture() {
  let view = {
    workspaceId: "a".repeat(32),
    drafts: {} as Record<string, string>,
    conflicts: [],
    revision: 0,
  };
  let lost = false;
  let receive!: (event: NativeSubscriptionEvent<any>) => void;
  let ids = 0;
  const request = vi.fn(async (method: string, params: any) => {
    if (method === "system.uuid") return `native:${++ids}`;
    if (method === "drafts.view") return structuredClone(view);
    if (method === "drafts.edit") {
      view = {
        ...view,
        drafts: { ...view.drafts, ...params.changes },
        revision: view.revision + 1,
      };
      if (lost) {
        lost = false;
        throw new Error("lost commit reply");
      }
      return structuredClone(view);
    }
    if (method === "drafts.push") return structuredClone(view);
    throw new Error(`Unexpected native command ${method}`);
  });
  const stop = vi.fn(async () => {});
  const subscribe = vi.fn(
    async (topic: string, params: unknown, callback: typeof receive) => {
      expect(topic).toBe("subscriptions");
      expect(params).toEqual({ resources: [{ kind: "drafts" }] });
      receive = callback;
      return stop;
    },
  );
  const context = {
    handle: "remote-context",
    serverId: "remote",
    workspaceId: view.workspaceId,
    request,
    subscribe,
  } as unknown as NativeServerContext;
  const useDrafts = createNativeDrafts(context);
  const result = useDrafts();
  for (const effect of hooks.effects.splice(0)) {
    const cleanup = effect();
    if (cleanup) hooks.cleanups.push(cleanup);
  }
  return {
    result,
    request,
    stop,
    notify: () =>
      receive({
        value: { type: "resource", snapshot: { resource: { kind: "drafts" } } },
      }),
    live: () => receive({ value: { type: "connection", status: "live" } }),
    lose: () => {
      lost = true;
    },
    remote: (text: string) => {
      view = { ...view, drafts: { room: text }, revision: view.revision + 1 };
    },
  };
}
it("uses committed native draft views and typed SSE without browser journals or snapshot polling", async () => {
  const f = fixture();
  await settle();
  const initial = f.request.mock.calls.length;
  await vi.advanceTimersByTimeAsync(60_000);
  expect(f.request).toHaveBeenCalledTimes(initial);
  f.remote("remote text");
  f.notify();
  await settle();
  expect(f.result.getDraft("room")).toBe("remote text");
  f.result.setDrafts({ room: "local text" });
  await settle();
  expect(f.result.getDraft("room")).toBe("local text");
  expect(f.request.mock.calls.map(([method]) => method)).toEqual([
    "drafts.view",
    "drafts.view",
    "system.uuid",
    "drafts.edit",
    "drafts.push",
  ]);
});
it("retries only the exact pending SQLite draft identity after a lost reply", async () => {
  const f = fixture();
  await settle();
  f.lose();
  f.result.setDrafts({ room: "keep" });
  await settle();
  f.live();
  await settle();
  const writes = f.request.mock.calls.filter(
    ([method]) => method === "drafts.edit",
  );
  expect(writes).toHaveLength(2);
  expect(writes[0]).toEqual(writes[1]);
  expect(
    f.request.mock.calls.filter(([method]) => method === "system.uuid"),
  ).toHaveLength(1);
  expect(
    f.request.mock.calls.filter(([method]) => method === "drafts.push"),
  ).toHaveLength(1);
  for (const cleanup of hooks.cleanups.splice(0)) cleanup();
  const count = f.request.mock.calls.length;
  f.notify();
  await settle();
  expect(f.request).toHaveBeenCalledTimes(count);
  expect(f.stop).toHaveBeenCalledOnce();
});
