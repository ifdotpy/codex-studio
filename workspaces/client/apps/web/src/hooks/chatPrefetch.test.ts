import { afterEach, expect, it, vi } from "vitest";
import type { Snapshot } from "../types";

const mocks = vi.hoisted(() => ({
  effects: [] as (() => void | (() => void))[],
  resources: new Set<string>(),
  stops: [] as string[],
  callbacks: new Map<string, (version?: unknown) => void>(),
  prefetch: vi.fn(async () => true),
  peek: vi.fn(() => ({ payload: { items: [] }, seq: 1 })),
}));
vi.mock("react", () => ({
  useEffect: (effect: () => void | (() => void)) => mocks.effects.push(effect),
  useRef: (current: unknown) => ({ current }),
  useCallback: (callback: unknown) => callback,
  useMemo: (factory: () => unknown) => factory(),
}));
vi.mock("../sync/client", () => ({}));
vi.mock("../sync/transcriptCache", () => ({}));
vi.mock("../sync/resume", () => ({ onResume: () => () => {} }));
vi.mock("../servers/ServerServicesContext", () => ({
  useServerSync: () => ({
    prefetchTranscript: mocks.prefetch,
    TRANSCRIPT_PREFETCH_LIMIT: 1,
    watchSyncInvalidations: () => () => {},
  }),
  useServerResources: () => ({
    watchResourceChanges: (
      resource: { agentId: string },
      callback: (version?: unknown) => void,
    ) => {
      mocks.callbacks.set(resource.agentId, callback);
      mocks.resources.add(resource.agentId);
      return () => {
        mocks.resources.delete(resource.agentId);
        mocks.stops.push(resource.agentId);
      };
    },
  }),
  useServerTranscripts: () => ({
    peekTranscript: mocks.peek,
    subscribeTranscript: () => () => {},
  }),
}));
import { useChatPrefetch } from "./chatPrefetch";

let cleanup: (() => void) | undefined;
afterEach(() => {
  cleanup?.();
  cleanup = undefined;
  mocks.effects.length = 0;
  mocks.resources.clear();
  mocks.stops.length = 0;
  mocks.callbacks.clear();
  mocks.prefetch.mockClear();
  mocks.peek.mockReturnValue({ payload: { items: [] }, seq: 1 });
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

it("releases prefetch detail when its server screen becomes hidden and restores it on return", () => {
  vi.useFakeTimers();
  vi.stubGlobal("document", {
    hidden: false,
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
  });
  vi.stubGlobal("navigator", { onLine: true });
  vi.stubGlobal("window", {
    matchMedia: () => ({ matches: false }),
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
  });
  const data = {
    stateDir: "workspace",
    threads: [
      { id: "open", source: "managed" },
      { id: "prefetch", source: "managed" },
    ],
  } as Snapshot;
  const render = (active: boolean) => {
    cleanup?.();
    mocks.effects.length = 0;
    useChatPrefetch(data, "open", "workspace", active);
    cleanup = mocks.effects[0]() || undefined;
    vi.advanceTimersByTime(1000);
  };
  render(true);
  expect([...mocks.resources]).toEqual(["prefetch"]);
  render(false);
  expect([...mocks.resources]).toEqual([]);
  expect(mocks.stops).toEqual(["prefetch"]);
  render(true);
  expect([...mocks.resources]).toEqual(["prefetch"]);
});

it("keeps inline prefetch updates warm without pulling and recovers uncovered hints", async () => {
  vi.useFakeTimers();
  vi.stubGlobal("document", {
    hidden: false,
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
  });
  vi.stubGlobal("navigator", { onLine: true });
  vi.stubGlobal("window", {
    matchMedia: () => ({ matches: false }),
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
  });
  const data = {
    stateDir: "workspace",
    threads: [
      { id: "open", source: "managed" },
      { id: "prefetch", source: "managed" },
    ],
  } as Snapshot;
  useChatPrefetch(data, "open", "workspace");
  cleanup = mocks.effects[0]() || undefined;
  await vi.advanceTimersByTimeAsync(1000);
  const changed = mocks.callbacks.get("prefetch")!;
  changed({ epoch: "live", revision: 9 });
  mocks.peek.mockReturnValue({ payload: { items: [] }, seq: 10 });
  changed({ epoch: "live", revision: 10, transcriptSequence: 10 });
  await vi.advanceTimersByTimeAsync(0);
  expect(mocks.prefetch).not.toHaveBeenCalled();
  changed({ epoch: "live", revision: 11, transcriptSequence: 11 });
  await vi.advanceTimersByTimeAsync(0);
  expect(mocks.prefetch).toHaveBeenCalledTimes(1);
  changed({ epoch: "live", revision: 12 });
  await vi.advanceTimersByTimeAsync(0);
  expect(mocks.prefetch).toHaveBeenCalledTimes(2);
});
