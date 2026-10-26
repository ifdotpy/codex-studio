import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { Agent, Snapshot } from "../types";
import type { UiPreferenceProjection } from "../sync/entityProjection";

type NativeSnapshot = Omit<Snapshot, "token">;
const h = vi.hoisted(() => ({
  cursor: 0,
  dirty: false,
  mounted: true,
  slots: [] as Array<{
    value?: unknown;
    dependencies?: unknown[];
    cleanup?: () => void;
  }>,
  effects: [] as Array<() => void>,
  service: undefined as unknown,
}));

vi.mock("react", () => {
  const same = (left?: unknown[], right?: unknown[]) =>
    !!left &&
    !!right &&
    left.length === right.length &&
    left.every((value, index) => Object.is(value, right[index]));
  function useMemo(factory: () => unknown, dependencies?: unknown[]) {
    const index = h.cursor++;
    const previous = h.slots[index];
    if (!previous || !same(previous.dependencies, dependencies))
      h.slots[index] = { value: factory(), dependencies };
    return h.slots[index].value;
  }
  function useEffect(
    effect: () => void | (() => void),
    dependencies?: unknown[],
  ) {
    const index = h.cursor++;
    const previous = h.slots[index];
    if (!previous || !same(previous.dependencies, dependencies)) {
      const slot = (h.slots[index] = {
        dependencies,
        cleanup: previous?.cleanup,
      });
      h.effects.push(() => {
        slot.cleanup?.();
        slot.cleanup = effect() || undefined;
      });
    }
  }
  return {
    useRef(value: unknown) {
      const index = h.cursor++;
      if (!h.slots[index]) h.slots[index] = { value: { current: value } };
      return h.slots[index].value;
    },
    useState(initial: unknown) {
      const index = h.cursor++;
      if (!h.slots[index])
        h.slots[index] = {
          value: typeof initial === "function" ? initial() : initial,
        };
      return [
        h.slots[index].value,
        (update: unknown) => {
          if (!h.mounted) return;
          const next =
            typeof update === "function"
              ? update(h.slots[index].value)
              : update;
          if (!Object.is(next, h.slots[index].value)) {
            h.slots[index].value = next;
            h.dirty = true;
          }
        },
      ];
    },
    useMemo,
    useCallback: (callback: unknown, dependencies?: unknown[]) =>
      useMemo(() => callback, dependencies),
    useEffect,
    useLayoutEffect: useEffect,
  };
});
vi.mock("./servicesContext", () => ({
  useServerApi: () => (h.service as Services).api,
  useServerSync: () => (h.service as Services).sync,
}));

import { useSnapshot } from "./hooks";

type Accept = (snapshot: NativeSnapshot | null, workspaceId: string) => void;
type Report = (error: unknown | null) => void;
let accept: Accept;
let report: Report;
let acceptPreferences:
  | ((preferences: UiPreferenceProjection) => void)
  | undefined;
const refreshProjection = vi.fn();
const stop = vi.fn();
const subscribeStateProjection = vi.fn(
  (next: Accept, fail: Report, preferences?: typeof acceptPreferences) => {
    accept = next;
    report = fail;
    acceptPreferences = preferences;
    return stop;
  },
);
const savedValues = new Map<string, unknown>();
const api = {
  saved: vi.fn(
    (key: string, fallback: unknown) => savedValues.get(key) ?? fallback,
  ),
  save: vi.fn((key: string, value: unknown) => {
    savedValues.set(key, value);
  }),
};
type Services = {
  api: typeof api;
  sync: {
    workspaceId: Promise<string>;
    subscribeStateProjection: typeof subscribeStateProjection;
    refreshProjection: typeof refreshProjection;
  };
};
let services: Services;
let result: ReturnType<typeof useSnapshot>;
let receivePreferences:
  | ((preferences: UiPreferenceProjection) => void)
  | undefined;

function render() {
  let commits = 0;
  do {
    if (++commits > 20)
      throw new Error("The hook does not reach a stable render.");
    h.cursor = 0;
    h.dirty = false;
    result = useSnapshot(receivePreferences);
    for (const effect of h.effects.splice(0)) effect();
  } while (h.dirty);
  return result;
}
function unmount() {
  h.mounted = false;
  for (const slot of h.slots) slot.cleanup?.();
}
async function settle() {
  await Promise.resolve();
  await Promise.resolve();
  render();
}
function snapshot(stateDir = "/state", threads: Agent[] = []): NativeSnapshot {
  return {
    stateDir,
    threads,
    runtime: { agents: threads },
    chats: [],
    nodes: threads,
    edges: [],
  } as unknown as NativeSnapshot;
}
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

beforeEach(() => {
  h.cursor = 0;
  h.dirty = false;
  h.mounted = true;
  h.slots = [];
  h.effects = [];
  savedValues.clear();
  receivePreferences = undefined;
  acceptPreferences = undefined;
  services = {
    api,
    sync: {
      workspaceId: Promise.resolve("workspace"),
      subscribeStateProjection,
      refreshProjection,
    },
  };
  h.service = services;
  refreshProjection.mockImplementation(
    async ({
      onWorkspaceId,
    }: { onWorkspaceId?: (id: string) => void } = {}) => {
      onWorkspaceId?.("workspace");
      return snapshot();
    },
  );
  // Native hooks must not need browser state, network, or retry timers.
  for (const name of [
    "window",
    "navigator",
    "localStorage",
    "sessionStorage",
    "indexedDB",
    "fetch",
    "setTimeout",
    "setInterval",
  ])
    vi.stubGlobal(
      name,
      new Proxy(() => {}, {
        get() {
          throw new Error(`Unexpected browser owner: ${name}`);
        },
        apply() {
          throw new Error(`Unexpected browser owner: ${name}`);
        },
      }),
    );
});
afterEach(() => {
  unmount();
  vi.unstubAllGlobals();
  vi.clearAllMocks();
});

it("renders the native projection without a session or fake token", async () => {
  render();
  await settle();
  const next = snapshot();
  accept(next, "workspace");
  expect(render().data).toBe(next);
  expect(result.workspaceId).toBe("workspace");
  expect(result.creationScope).toBe("/state");
  expect(result.data).not.toHaveProperty("token");
  expect(refreshProjection).not.toHaveBeenCalled();
});

it("keeps the created chat visible until its exact projection arrives", async () => {
  render();
  accept(snapshot(), "workspace");
  render();
  const agent = { id: "lead" } as Agent;
  result.rememberCreated(agent, "/state");
  expect(render().data?.threads).toEqual([agent]);
  expect(result.data?.runtime.agents).toEqual([agent]);
  const confirmed = snapshot("/state", [agent]);
  accept(confirmed, "workspace");
  expect(render().data).toBe(confirmed);
  expect(api.save).toHaveBeenLastCalledWith("codex-confirmed-chats", {
    scope: "/state",
    agents: [],
  });
});

it("forgets created chats and refuses a changed workspace", () => {
  render();
  accept(snapshot(), "workspace");
  render();
  const agent = { id: "lead" } as Agent;
  result.rememberCreated(agent, "/state");
  render();
  result.forgetCreated(["lead"]);
  expect(render().data?.threads).toEqual([]);
  accept(snapshot("/other"), "other-workspace");
  render();
  expect(() => result.rememberCreated(agent, "/state")).toThrow(
    "workspace changed",
  );
});

it("uses only native preferences for a retained created-chat overlay", () => {
  const agent = { id: "retained" } as Agent;
  savedValues.set("codex-confirmed-chats", {
    scope: "/state",
    agents: [agent],
  });
  render();
  accept(snapshot(), "workspace");
  expect(render().data?.threads).toEqual([agent]);
  accept(snapshot("/other"), "other-workspace");
  expect(render().data?.threads).toEqual([]);
  accept(snapshot(), "workspace");
  expect(render().data?.threads).toEqual([]);
});

it("discards a malformed retained overlay as one invalid value", () => {
  savedValues.set("codex-confirmed-chats", {
    scope: "/state",
    agents: "invalid",
  });
  render();
  expect(api.save).toHaveBeenLastCalledWith("codex-confirmed-chats", {
    scope: "",
    agents: [],
  });
});

it("keeps each explicit readback identity when a newer refresh wins the render", async () => {
  render();
  const older = deferred<NativeSnapshot>();
  refreshProjection.mockImplementationOnce(
    ({ onWorkspaceId }: { onWorkspaceId: (id: string) => void }) => {
      onWorkspaceId("original-workspace");
      return older.promise;
    },
  );
  const readback = vi.fn();
  const pending = result.refresh(false, readback);
  await result.refresh(false);
  const newer = render().data;
  const old = snapshot("/old");
  older.resolve(old);
  await pending;
  expect(readback).toHaveBeenCalledExactlyOnceWith(old, "original-workspace");
  expect(render().data).toBe(newer);
});

it("shows native read errors and recovers only from a new event or explicit refresh", async () => {
  render();
  report(new Error("Native projection unavailable"));
  expect(render().error).toBe("Native projection unavailable");
  expect(refreshProjection).not.toHaveBeenCalled();
  accept(snapshot(), "workspace");
  expect(render().error).toBe("");
  refreshProjection.mockRejectedValueOnce(new Error("Refresh unavailable"));
  await result.refresh(false);
  expect(render().error).toBe("Refresh unavailable");
  await result.refresh(false);
  expect(render().error).toBe("");
});

it("does not let a stale callback publish into a replacement service", async () => {
  render();
  const oldAccept = accept;
  const oldReport = report;
  const oldRead = deferred<NativeSnapshot>();
  refreshProjection.mockReturnValueOnce(oldRead.promise);
  const readback = vi.fn();
  const pending = result.refresh(false, readback);
  services = {
    ...services,
    sync: { ...services.sync, workspaceId: Promise.resolve("new-workspace") },
  };
  h.service = services;
  render();
  await settle();
  const current = snapshot("/new");
  accept(current, "new-workspace");
  render();
  oldAccept(snapshot("/old"), "old-workspace");
  oldReport(new Error("Old error"));
  oldRead.resolve(snapshot("/old"));
  await pending;
  expect(render().data).toBe(current);
  expect(result.error).toBe("");
  expect(result.workspaceId).toBe("new-workspace");
  expect(readback).not.toHaveBeenCalled();
  expect(stop).toHaveBeenCalledTimes(1);
});

it("delivers preferences to the latest committed callback and stops on unmount", () => {
  const first = vi.fn();
  const second = vi.fn();
  receivePreferences = first;
  render();
  receivePreferences = second;
  render();
  const preferences: UiPreferenceProjection = {};
  acceptPreferences?.(preferences);
  expect(first).not.toHaveBeenCalled();
  expect(second).toHaveBeenCalledExactlyOnceWith(preferences);
  expect(subscribeStateProjection).toHaveBeenCalledTimes(1);
  unmount();
  expect(stop).toHaveBeenCalledTimes(1);
  h.slots = [];
});
