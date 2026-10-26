import { afterEach, expect, it, vi } from "vitest";
import type {
  ResourceConnectionDetail,
  ResourceConnectionState,
  ResourceStreamState,
} from "./sync/resourceConnection";

const h = vi.hoisted(() => ({
  effects: [] as Array<() => void | (() => void)>,
  connection: undefined as
    | undefined
    | ((
        health: ResourceConnectionState,
        activity: ResourceStreamState,
        detail: ResourceConnectionDetail,
      ) => void),
  start: vi.fn(() => () => {}),
  retry: vi.fn(),
  stopConnection: vi.fn(),
  snapshotBoundary: new Error("Snapshot rendering is outside this effect test"),
}));
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useState: (initial: unknown) => [
    typeof initial === "function" ? initial() : initial,
    vi.fn(),
  ],
  useRef: (current: unknown) => ({ current }),
  useMemo: (factory: () => unknown) => factory(),
  useCallback: (callback: unknown) => callback,
  useEffect: (effect: () => void | (() => void)) => h.effects.push(effect),
  useLayoutEffect: () => {},
}));
vi.mock("./hooks", () => ({
  useSnapshot: () => {
    throw h.snapshotBoundary;
  },
}));
vi.mock("./desktop", () => ({}));
vi.mock("./wasm/client", () => ({}));
vi.mock("./sync/uiPreferenceBridge", () => ({
  createUiPreferenceBridge: () => ({
    start: h.start,
    retryPendingWrites: h.retry,
  }),
}));
vi.mock("./servers/ScreenContext", () => ({
  useScreenEnvironment: () => ({}),
}));
vi.mock("./servers/ServerSettingsContext", () => ({
  useServerSettings: () => null,
}));
vi.mock("./components/useSendingMessageRemoval", () => ({
  useSendingMessageRemoval: () => ({}),
}));
vi.mock("./servers/ServerServicesContext", () => ({
  useServerApi: () => ({
    saved: (_key: string, initial: unknown) => initial,
    onApiSchemaMismatch: () => () => {},
  }),
  useServerStorage: () => ({ getItem: () => null }),
  useServerStorageEventKey: () => (key: string) => key,
  useServerChatCreations: () => ({}),
  useServerFileDialogs: () => ({}),
  useServerSync: () => ({}),
  useServerResources: () => ({
    watchResourceConnection: (listener: typeof h.connection) => {
      h.connection = listener;
      return h.stopConnection;
    },
  }),
  useServerSidebarServices: () => ({}),
  useServerAccountLimits: () => ({}),
  useServerOutbox: () => ({ useOutbox: () => ({ entries: [] }) }),
  useServerActivityScope: () => ({}),
}));
vi.mock("@mantine/hooks", async (original) => ({
  ...(await original<typeof import("@mantine/hooks")>()),
  useMediaQuery: () => false,
}));
vi.mock("@mantine/core", async (original) => ({
  ...(await original<typeof import("@mantine/core")>()),
  useMantineColorScheme: () => ({
    colorScheme: "dark",
    setColorScheme: vi.fn(),
  }),
}));

import App from "./App";
const stops: Array<() => void> = [];
afterEach(() => {
  stops.splice(0).forEach((stop) => stop());
  h.effects = [];
  h.connection = undefined;
  vi.clearAllMocks();
  vi.unstubAllGlobals();
});
function mountPreferenceEffect() {
  vi.stubGlobal("window", new EventTarget());
  vi.stubGlobal("localStorage", { getItem: () => null });
  vi.stubGlobal("location", new URL("http://studio.test/"));
  // Execute App's actual effect registration, stopping at the unrelated snapshot
  // hook so this focused test needs no full application or provider fixture.
  expect(() => App({ active: false })).toThrow(h.snapshotBoundary);
  for (const effect of h.effects.splice(0)) {
    const stop = effect();
    if (stop) stops.push(stop);
  }
  expect(h.start).toHaveBeenCalledOnce();
  expect(h.connection).toBeTypeOf("function");
}
it("retries preferences on initial live and each recovery, never on same-health activity or detail changes", () => {
  mountPreferenceEffect();
  h.connection?.("live", "open", {});
  expect(h.retry).toHaveBeenCalledOnce();
  h.connection?.("live", "offline", {});
  h.connection?.("live", "open", {});
  h.connection?.("live", "waiting-owner", { failure: "unknown" });
  expect(h.retry).toHaveBeenCalledOnce();
  h.connection?.("degraded", "retry-scheduled", {
    failure: "stream-error",
    retryAt: 1000,
  });
  h.connection?.("degraded", "connecting", { failure: "stream-error" });
  expect(h.retry).toHaveBeenCalledOnce();
  h.connection?.("live", "open", {});
  expect(h.retry).toHaveBeenCalledTimes(2);
  h.connection?.("live", "open", {});
  expect(h.retry).toHaveBeenCalledTimes(2);
});
