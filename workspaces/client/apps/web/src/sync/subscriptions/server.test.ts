import { expect, it, vi } from "vitest";
import type { ResourceEventsOptions } from "../resourceEvents";
const mocks = vi.hoisted(() => ({
  options: undefined as ResourceEventsOptions | undefined,
  source: vi.fn(),
}));
vi.mock("../resourceEvents", () => ({
  createResourceEvents: (options: ResourceEventsOptions) => {
    mocks.options = options;
    return {
      watchResourceChanges: () => () => {},
      watchResourceConnection: () => () => {},
      dispose: () => {},
    };
  },
}));
vi.mock("../../servers/eventSource", () => ({
  SignedEventSource: class {
    constructor(...args: unknown[]) {
      mocks.source(...args);
    }
  },
}));
vi.mock("./data", () => ({ createDataObserver: () => () => () => {} }));
vi.mock("./entityData", () => ({
  createEntityDataObserver: () => () => () => {},
}));
import { createServerSubscriptions } from "./server";

it("forwards resumed refs to the same server's fully applied cursor reader", async () => {
  const ref = { kind: "transcript", agentId: "chat" } as const;
  const cursor = { workspaceId: "server-workspace", seq: 7 };
  const read = vi.fn(async () => cursor);
  const database = vi.fn();
  const persist = vi.fn();
  const request = vi.fn();
  const options = {
    owner: { id: "remote", origin: "https://server.example" },
    api: { fetchApiRequest: request },
    tokenRates: {},
    sync: () => ({
      readSubscriptionCursor: read,
      syncDatabase: database,
      persistResourceEntityChanges: persist,
    }),
  } as unknown as Parameters<typeof createServerSubscriptions>[0];
  const subscriptions = createServerSubscriptions(options);
  expect(mocks.options?.sync.readSubscriptionCursor).toBeTypeOf("function");
  await expect(
    mocks.options?.sync.readSubscriptionCursor?.(ref),
  ).resolves.toEqual(cursor);
  expect(read).toHaveBeenCalledExactlyOnceWith(ref);
  expect(mocks.options?.storageName?.("cache")).toBe("cache:server:remote");
  mocks.options?.openEvents?.("/api/sync/stream");
  expect(mocks.source).toHaveBeenCalledWith(
    "https://server.example/api/sync/stream",
    request,
  );
  subscriptions.dispose();
});
