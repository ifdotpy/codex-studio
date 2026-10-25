import { expect, it, vi } from "vitest";
const subscription = vi.hoisted(() =>
  vi.fn(() => ({ data: undefined, status: "stale", error: null })),
);
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useEffect: () => {},
  useLayoutEffect: () => {},
  useRef: (current: unknown) => ({ current }),
  useState: (initial: unknown) => [
    typeof initial === "function" ? initial() : initial,
    () => {},
  ],
  useMemo: (factory: () => unknown) => factory(),
  useCallback: (callback: unknown) => callback,
}));
vi.mock("./sync/subscriptions", () => ({ useSubscription: subscription }));
vi.mock("./sync/client", () => ({}));
vi.mock("./sync/transcriptCache", () => ({}));
vi.mock("./servers/ServerServicesContext", () => ({
  useServerApi: () => ({ syncGet: vi.fn() }),
  useServerSync: () => ({}),
  useServerResources: () => ({ watchResourceChanges: vi.fn() }),
  useServerTranscripts: () => ({ peekTranscript: () => undefined }),
}));
import { useMessages } from "./hooks";

it("keeps a mounted hidden conversation out of subscription interest", () => {
  useMessages("hidden-chat", "agent", true, "workspace", "workspace", false);
  expect(subscription).toHaveBeenLastCalledWith(null);
});
it("subscribes the visible managed conversation", () => {
  useMessages("visible-chat", "agent", true, "workspace", "workspace");
  expect(subscription).toHaveBeenLastCalledWith(
    expect.objectContaining({ kind: "transcript", agentId: "visible-chat" }),
  );
});
