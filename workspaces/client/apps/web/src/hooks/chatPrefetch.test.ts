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

it.each([false, true])(
  "keeps invisible chats unsubscribed on mobile=%s",
  async (mobile) => {
    vi.useFakeTimers();
    vi.stubGlobal("document", {
      hidden: false,
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("window", {
      matchMedia: () => ({ matches: mobile }),
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    const data = {
      stateDir: "workspace",
      threads: [
        { id: "open", source: "managed" },
        { id: "invisible-one", source: "managed" },
        { id: "invisible-two", source: "managed" },
      ],
    } as Snapshot;
    const prepare = useChatPrefetch(data, "open", "workspace");
    for (const effect of mocks.effects) {
      const stop = effect();
      if (stop) cleanup = stop;
    }
    prepare("invisible-one");
    await vi.advanceTimersByTimeAsync(1000);
    expect([...mocks.resources]).toEqual([]);
    expect(mocks.prefetch).not.toHaveBeenCalled();
  },
);
