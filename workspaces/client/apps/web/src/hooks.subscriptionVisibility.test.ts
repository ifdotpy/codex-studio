import { afterEach, expect, it, vi } from "vitest";
import type { SubscriptionState } from "./sync/subscriptions/contracts";
import type { TranscriptPageData } from "./types";
import type {
  ResourceConnectionDetail,
  ResourceConnectionState,
  ResourceStreamState,
} from "./sync/resourceConnection";
const h = vi.hoisted(() => ({
  effects: [] as (() => void | (() => void))[],
  writes: [] as unknown[],
  connection: undefined as
    | undefined
    | ((
        status: ResourceConnectionState,
        stream?: ResourceStreamState,
        detail?: ResourceConnectionDetail,
      ) => void),
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
    expect(h.writes).not.toContainEqual(
      expect.objectContaining({
        message: expect.stringContaining("Reconnecting"),
      }),
    );
    expect(h.get).not.toHaveBeenCalled();
  },
);

it("tracks actual transport activity independently of transcript freshness", () => {
  useMessages("chat", "agent", true, "workspace", "workspace");
  runEffects();
  expect(h.connection).toBeTypeOf("function");
  h.connection?.("connecting", "connecting");
  expect(h.writes).not.toContainEqual(
    expect.objectContaining({
      message: expect.stringContaining("Reconnecting"),
    }),
  );
  h.connection?.("live", "open");
  expect(h.writes.at(-1)).toMatchObject({ health: "live", message: "" });
  h.connection?.("degraded", "open");
  expect(h.writes.at(-1)).toMatchObject({
    message: "Reason unknown. Waiting for server updates…",
  });
  h.connection?.("degraded", "connecting");
  expect(h.writes.at(-1)).toMatchObject({
    message: "Reason unknown. Reconnecting…",
  });
  h.connection?.("live", "open");
  expect(h.writes.at(-1)).toMatchObject({ health: "live", message: "" });
  h.connection?.("offline", "stopped");
  expect(h.writes.at(-1)).toMatchObject({
    message: "Reason unknown. Connection interrupted.",
  });
  h.connection?.("live", "open");
  expect(h.writes.at(-1)).toMatchObject({ health: "live", message: "" });
  const updates = h.writes.length;
  h.connection?.("live", "open");
  h.connection?.("live", "open");
  expect(h.writes).toHaveLength(updates);
  expect(h.get).not.toHaveBeenCalled();
});

it.each([
  ["degraded", "preparing", "Preparing connection…"],
  ["degraded", "waiting-owner", "Waiting for the shared connection…"],
  ["degraded", "retry-scheduled", "Reason unknown. Retry scheduled."],
  ["degraded", "connecting", "Reason unknown. Reconnecting…"],
  ["connecting", "connecting", "Connecting…"],
  ["connecting", "open", "Waiting for server updates…"],
  ["offline", "offline", "No network."],
  ["live", "offline", "No network."],
  ["degraded", "stopped", "Reason unknown. Connection interrupted."],
  ["degraded", undefined, "Reason unknown. Connection interrupted."],
  ["offline", undefined, "Reason unknown. Connection interrupted."],
  ["degraded", "paused", ""],
  ["paused", "stopped", ""],
  ["live", "waiting-owner", ""],
  ["schema-mismatch", "stopped", ""],
] as const)(
  "selects %s health with %s activity through useMessages",
  (health, activity, message) => {
    useMessages("chat", "agent", true, "workspace", "workspace");
    runEffects();
    h.connection?.(health, activity);
    expect(h.writes.at(-1)).toMatchObject({ health, activity, message });
    if (activity !== "connecting")
      expect(h.writes.at(-1)).not.toMatchObject({
        message: expect.stringContaining("Reconnecting"),
      });
    expect(h.get).not.toHaveBeenCalled();
  },
);

it("updates the actual retry deadline and known cause without changing health", () => {
  useMessages("chat", "agent", true, "workspace", "workspace");
  runEffects();
  h.connection?.("degraded", "retry-scheduled", {
    failure: "heartbeat-timeout",
    retryAt: 1_792_965_000_000,
  });
  expect(h.writes.at(-1)).toMatchObject({
    health: "degraded",
    activity: "retry-scheduled",
    message: expect.stringContaining(
      "Server updates timed out. Retry scheduled for ",
    ),
  });
  const first = h.writes.at(-1);
  h.connection?.("degraded", "retry-scheduled", {
    failure: "heartbeat-timeout",
    retryAt: 1_792_965_010_000,
  });
  expect(h.writes.at(-1)).not.toEqual(first);
  h.connection?.("live", "open", {});
  expect(h.writes.at(-1)).toMatchObject({ health: "live", message: "" });
  expect(h.get).not.toHaveBeenCalled();
});

it("keeps an intentionally paused stream separate from a lost connection", () => {
  useMessages("chat", "agent", true, "workspace", "workspace");
  runEffects();
  h.connection?.("live");
  h.connection?.("paused");
  expect(h.writes.at(-1)).toMatchObject({ health: "paused", message: "" });
  expect(h.writes).not.toContainEqual(
    expect.objectContaining({
      message: expect.stringContaining("Reconnecting"),
    }),
  );
  h.connection?.("connecting");
  h.connection?.("live");
  expect(h.writes.at(-1)).toMatchObject({ health: "live", message: "" });
  expect(h.get).not.toHaveBeenCalled();
});
