import { afterEach, expect, it, vi } from "vitest";
import { READ_RECOVERY_MAX_ATTEMPTS } from "../sync/readRetry";
const h = vi.hoisted(() => ({
  effects: [] as (() => void | (() => void))[],
  session: vi.fn(),
  database: vi.fn(),
  writes: [] as unknown[],
  connection: undefined as undefined | ((state: string) => void),
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
    useSnapshot();
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
  },
);
