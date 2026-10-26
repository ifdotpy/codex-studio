import { afterEach, expect, it, vi } from "vitest";
import { READ_RECOVERY_MAX_ATTEMPTS } from "../sync/readRetry";
import type {
  ResourceConnectionDetail,
  ResourceConnectionState,
  ResourceStreamState,
} from "../sync/resourceConnection";
import { localTime } from "../local-time";
const h = vi.hoisted(() => ({
  effects: [] as (() => void | (() => void))[],
  session: vi.fn(),
  database: vi.fn(),
  writes: [] as unknown[],
  connection: undefined as
    | undefined
    | ((
        state: ResourceConnectionState,
        activity?: ResourceStreamState,
        detail?: ResourceConnectionDetail,
      ) => void),
}));
vi.mock("react", () => ({
  useEffect: (fn: () => void | (() => void)) => h.effects.push(fn),
  useLayoutEffect: vi.fn(),
  useRef: (current: unknown) => ({ current }),
  useState: (value: unknown) => [
    typeof value === "function" ? value() : value,
    (next: unknown) => h.writes.push(next),
  ],
  useMemo: (fn: () => unknown) => fn(),
  useCallback: (fn: unknown) => fn,
}));
vi.mock("../servers/ServerServicesContext", () => ({
  useServerApi: () => ({
    refreshSession: h.session,
    saved: (_: string, value: unknown) => value,
    save: vi.fn(),
  }),
  useServerSync: () => ({
    syncDatabase: h.database,
    subscribeStateProjection: () => () => {},
  }),
  useServerResources: () => ({
    watchResourceConnection: (fn: typeof h.connection) => {
      h.connection = fn;
      return () => {};
    },
  }),
}));
vi.mock("../sync/client", () => ({}));
vi.mock("../sync/transcriptCache", () => ({}));
vi.mock("../sync/resume", () => ({ onResume: () => () => {} }));
import { useSnapshot } from "../hooks";
let stops: (() => void)[] = [];
afterEach(() => {
  stops.forEach((stop) => stop());
  stops = [];
  h.effects.length = 0;
  h.writes.length = 0;
  vi.clearAllMocks();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});
it.each(["credentials", "database"])(
  "bounds %s startup recovery and displays exhaustion",
  async (boundary) => {
    vi.useFakeTimers();
    vi.stubGlobal("window", new EventTarget());
    vi.stubGlobal("document", { hidden: false });
    vi.stubGlobal("navigator", { onLine: true });
    h.database.mockResolvedValue({ workspaceId: "workspace" });
    h.session.mockResolvedValue({ token: "token" });
    const read = boundary === "credentials" ? h.session : h.database;
    read.mockRejectedValue(new TypeError("unreachable"));
    const hook = useSnapshot();
    stops = h.effects.flatMap((effect) => {
      const stop = effect();
      return stop ? [stop] : [];
    });
    await vi.advanceTimersByTimeAsync(5 * 60_000);
    expect(read).toHaveBeenCalledTimes(READ_RECOVERY_MAX_ATTEMPTS);
    expect(h.writes).toContainEqual(
      expect.objectContaining({
        startupError: expect.stringContaining("exhausted"),
      }),
    );
    window.dispatchEvent(new Event("focus"));
    await vi.advanceTimersByTimeAsync(60_000);
    expect(read).toHaveBeenCalledTimes(READ_RECOVERY_MAX_ATTEMPTS);
    h.connection?.("degraded");
    h.connection?.("live");
    await vi.advanceTimersByTimeAsync(5 * 60_000);
    expect(read).toHaveBeenCalledTimes(READ_RECOVERY_MAX_ATTEMPTS * 2);
    await hook.refresh(true);
    await vi.advanceTimersByTimeAsync(5 * 60_000);
    expect(read).toHaveBeenCalledTimes(READ_RECOVERY_MAX_ATTEMPTS * 3);
  },
);

it.each([true, false])(
  "does not turn the browser offline hint into a startup failure (initiallyOnline=%s)",
  (online) => {
    vi.stubGlobal("window", new EventTarget());
    vi.stubGlobal("document", { hidden: false });
    vi.stubGlobal("navigator", { onLine: online });
    h.database.mockResolvedValue({ workspaceId: "workspace" });
    h.session.mockResolvedValue({ token: "token" });
    useSnapshot();
    stops = h.effects.flatMap((effect) => {
      const stop = effect();
      return stop ? [stop] : [];
    });
    h.connection?.("live");
    window.dispatchEvent(new Event("offline"));
    expect(h.writes).not.toContainEqual(
      expect.objectContaining({
        startupError: expect.stringContaining("Offline"),
      }),
    );
    h.connection?.("degraded");
    expect(h.writes).toContainEqual(
      expect.objectContaining({
        transportError: "Reason unknown. Connection interrupted.",
      }),
    );
    h.connection?.("paused");
    expect(h.writes.at(-1)).toEqual(
      expect.objectContaining({ transportError: "" }),
    );
  },
);

it.each([
  ["live", "offline", {}, "No network."],
  ["degraded", "preparing", {}, "Preparing connection…"],
  ["connecting", "preparing", {}, ""],
  [
    "degraded",
    "connecting",
    { failure: "heartbeat-timeout" },
    "Server updates timed out. Reconnecting…",
  ],
  [
    "degraded",
    "waiting-owner",
    { failure: "stream-error" },
    "Connection failed. Waiting for the shared connection…",
  ],
] as const)(
  "uses actual %s health and %s activity for the snapshot warning",
  (health, activity, detail, transportError) => {
    vi.stubGlobal("window", new EventTarget());
    vi.stubGlobal("document", { hidden: false });
    h.database.mockResolvedValue({ workspaceId: "workspace" });
    h.session.mockResolvedValue({ token: "token" });
    useSnapshot();
    stops = h.effects.flatMap((effect) => {
      const stop = effect();
      return stop ? [stop] : [];
    });
    h.connection?.(health, activity, detail);
    if (transportError)
      expect(h.writes).toContainEqual(
        expect.objectContaining({ transportError }),
      );
    else
      expect(h.writes).not.toContainEqual(
        expect.objectContaining({
          transportError: expect.stringContaining("Preparing"),
        }),
      );
  },
);
it("shows the scheduled retry deadline in the snapshot warning", () => {
  vi.stubGlobal("window", new EventTarget());
  vi.stubGlobal("document", { hidden: false });
  h.database.mockResolvedValue({ workspaceId: "workspace" });
  h.session.mockResolvedValue({ token: "token" });
  useSnapshot();
  stops = h.effects.flatMap((effect) => {
    const stop = effect();
    return stop ? [stop] : [];
  });
  const retryAt = 1_792_965_000_000;
  h.connection?.("degraded", "retry-scheduled", {
    failure: "stream-error",
    retryAt,
  });
  expect(h.writes).toContainEqual(
    expect.objectContaining({
      transportError: `Connection failed. Retry scheduled for ${localTime(new Date(retryAt), { hour: "2-digit", minute: "2-digit", second: "2-digit" })}.`,
    }),
  );
});
