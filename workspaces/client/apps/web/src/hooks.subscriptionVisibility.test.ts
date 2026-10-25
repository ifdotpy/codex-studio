import { afterEach, expect, it, vi } from "vitest";
import type { SubscriptionState } from "./sync/subscriptions/contracts";
import type { TranscriptPageData } from "./types";
const h = vi.hoisted(() => ({
  effects: [] as (() => void | (() => void))[],
  writes: [] as unknown[],
  connection: undefined as undefined | ((status: string) => void),
  get: vi.fn(),
}));
const subscription = vi.hoisted(() =>
  vi.fn((): SubscriptionState<TranscriptPageData> => ({
    data: undefined,
    status: "stale",
    error: null,
  })),
);
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useEffect: (effect: () => void | (() => void)) => h.effects.push(effect),
  useLayoutEffect: () => {},
  useRef: (current: unknown) => ({ current }),
  useState: (initial: unknown) => {
    let value = typeof initial === "function" ? initial() : initial;
    return [
      value,
      (next: unknown) => {
        value = typeof next === "function" ? next(value) : next;
        h.writes.push(value);
      },
    ];
  },
  useMemo: (factory: () => unknown) => factory(),
  useCallback: (callback: unknown) => callback,
}));
vi.mock("./sync/subscriptions", () => ({ useSubscription: subscription }));
vi.mock("./sync/client", () => ({}));
vi.mock("./sync/transcriptCache", () => ({}));
vi.mock("./servers/ServerServicesContext", () => ({
  useServerApi: () => ({ syncGet: h.get }),
  useServerSync: () => ({}),
  useServerResources: () => ({
    watchResourceChanges: vi.fn(),
    watchResourceConnection: (accept: typeof h.connection) => {
      h.connection = accept;
      return () => {
        h.connection = undefined;
      };
    },
  }),
  useServerTranscripts: () => ({ peekTranscript: () => undefined }),
}));
import { useMessages } from "./hooks";
let stops: (() => void)[] = [];
afterEach(() => {
  stops.forEach((stop) => stop());
  stops = [];
  h.effects.length = 0;
  h.writes.length = 0;
  h.connection = undefined;
  subscription.mockReturnValue({
    data: undefined,
    status: "stale",
    error: null,
  });
  vi.clearAllMocks();
});
function runEffects() {
  stops = h.effects.flatMap((effect) => {
    const stop = effect();
    return stop ? [stop] : [];
  });
}

it("keeps a mounted hidden conversation out of subscription interest", () => {
  useMessages("hidden-chat", "agent", true, "workspace", "workspace", false);
  expect(subscription).toHaveBeenLastCalledWith(null);
});
it("subscribes the visible managed conversation", () => {
  useMessages("visible-chat", "agent", true, "workspace", "workspace");
  expect(subscription).toHaveBeenCalledWith(
    expect.objectContaining({ kind: "transcript", agentId: "visible-chat" }),
  );
});

it.each([
  { visible: false, status: "stale" },
  { visible: true, status: "loading" },
  { visible: true, status: "stale" },
  { visible: true, status: "error" },
] as const)(
  "does not call $status transcript data a lost connection (visible=$visible)",
  ({ visible, status }) => {
    subscription.mockReturnValue({
      data: undefined,
      status,
      error: status === "error" ? new Error("Projection unavailable") : null,
    });
    useMessages("chat", "agent", true, "workspace", "workspace", visible);
    runEffects();
    expect(h.writes).not.toContain("reconnecting");
    expect(h.get).not.toHaveBeenCalled();
  },
);

it("tracks actual transport loss and recovery independently of transcript freshness", () => {
  useMessages("chat", "agent", true, "workspace", "workspace");
  runEffects();
  expect(h.connection).toBeTypeOf("function");
  h.connection?.("connecting");
  expect(h.writes).not.toContain("reconnecting");
  h.connection?.("live");
  expect(h.writes.at(-1)).toBe("live");
  h.connection?.("degraded");
  expect(h.writes.at(-1)).toBe("reconnecting");
  h.connection?.("connecting");
  expect(h.writes.at(-1)).toBe("reconnecting");
  h.connection?.("live");
  expect(h.writes.at(-1)).toBe("live");
  h.connection?.("offline");
  expect(h.writes.at(-1)).toBe("reconnecting");
  h.connection?.("live");
  expect(h.writes.at(-1)).toBe("live");
  const updates = h.writes.length;
  h.connection?.("live");
  h.connection?.("live");
  expect(h.writes).toHaveLength(updates);
  expect(h.get).not.toHaveBeenCalled();
});
