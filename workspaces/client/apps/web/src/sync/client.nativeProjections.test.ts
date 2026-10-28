import assert from "node:assert/strict";
import { afterEach, expect, it, vi } from "vitest";
import type { Snapshot } from "../types";
import { SYNC_PROTOCOL_VERSION } from "../generated/api";
import type {
  ResourceChangeEvent,
  ResourceVersion,
} from "./subscriptions/events";
import { ResourceEntityDelivery } from "./resourceEntityDelivery";
import { createNativeCoreFixture } from "./nativeCoreFixture";

// The native path must never open IndexedDB, RxDB, Web Locks or a
// BroadcastChannel for projections. Each of them throws if it is touched.
const mocks = vi.hoisted(() => ({
  syncGet: vi.fn(),
  changes: new Set<(version?: ResourceVersion) => void>(),
  core: undefined as undefined | ReturnType<typeof createNativeCoreFixture>,
}));
vi.mock("rxdb", async (original) => ({
  ...(await original<typeof import("rxdb")>()),
  createRxDatabase: () => {
    throw new Error("RxDB must not open in the native path.");
  },
}));
vi.mock("../native/client", () => ({
  hasNativeClient: () => true,
  nativeRequest: (method: string, params: unknown) =>
    mocks.core!.request(method, params),
}));
vi.mock("../api", () => ({
  ApiError: class extends Error {},
  syncGet: mocks.syncGet,
  syncPost: vi.fn(),
  saved: (_key: string, fallback: unknown) => fallback,
  save: vi.fn(),
  setWorkspace: vi.fn(),
  isApiSchemaMismatch: () => false,
  onMatchingApiSchemaResponse: () => () => {},
}));
vi.mock("./subscriptions/events", () => ({
  acknowledgeEntitySequences: vi.fn(),
  watchResourceConnection: () => () => {},
  watchResourceChanges: (
    _ref: unknown,
    accept: (version?: ResourceVersion) => void,
  ) => {
    mocks.changes.add(accept);
    return () => mocks.changes.delete(accept);
  },
}));
vi.mock("./resume", () => ({ onResume: () => () => {} }));

const workspaceId = "b".repeat(32);
const stops: (() => void)[] = [];
afterEach(() => {
  for (const stop of stops.splice(0)) stop();
  mocks.syncGet.mockReset();
  mocks.changes.clear();
  vi.unstubAllGlobals();
  vi.resetModules();
});

async function page() {
  const forbidden = () => {
    throw new Error("A browser storage API was used in the native path.");
  };
  vi.stubGlobal("window", {
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    matchMedia: () => ({ matches: false }),
  });
  vi.stubGlobal("document", {
    hidden: false,
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
  });
  vi.stubGlobal("navigator", {
    onLine: true,
    get locks() {
      return forbidden();
    },
  });
  vi.stubGlobal("indexedDB", {
    open: forbidden,
    databases: forbidden,
    deleteDatabase: forbidden,
  });
  vi.stubGlobal("BroadcastChannel", forbidden);
  mocks.syncGet.mockImplementation(async (path: string) => {
    if (path === "/api/sync/identity") return { workspaceId };
    throw new Error(`No HTTP read is expected: ${path}`);
  });
  const client = await import("./client");
  const snapshots: Snapshot[] = [];
  const failed = vi.fn();
  stops.push(
    client.subscribeStateProjection((value) => {
      if (value) snapshots.push(value);
    }, failed),
  );
  const frame = (seq: number, name: string): ResourceChangeEvent => ({
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId,
    epoch: "first",
    revision: seq,
    reason: "initial",
    resources: [{ kind: "state" }],
    resourceVersions: [
      {
        revision: seq,
        resource: { kind: "state" },
        cursor: { workspaceId, seq },
        replyKind: "snapshot",
        entityChanges: {
          after: 0,
          through: seq,
          documents: [
            {
              op: "upsert",
              collection: "agentSummary",
              id: "lead",
              seq,
              value: {
                id: "lead",
                rootId: "lead",
                epoch: 1,
                name,
                status: "idle",
              },
            },
          ],
        },
      },
    ],
  });
  const delivery = new ResourceEntityDelivery(
    client.persistResourceEntityChanges,
    (event) => {
      const entry = event.resourceVersions[0];
      assert.ok(entry != null);
      for (const callback of mocks.changes)
        callback({
          epoch: event.epoch,
          revision: entry.revision,
          reason: event.reason,
          cursor: entry.cursor ?? undefined,
          replyKind: entry.replyKind ?? undefined,
          entitySequence: entry.revision,
        });
    },
  );
  return { client, snapshots, failed, frame, delivery };
}

it("renders a state frame from the core's projections without opening browser storage", async () => {
  mocks.core = createNativeCoreFixture();
  const p = await page();
  const { workspaceId: opened } = await p.client.syncDatabase();
  expect(opened).toBe(workspaceId);
  p.delivery.receive(p.frame(5, "From the core"));
  await vi.waitFor(() =>
    expect(p.snapshots.at(-1)?.threads[0]?.name).toBe("From the core"),
  );
  expect(p.snapshots.at(-1)?.entitiesComplete).toBe(true);
  expect(p.failed).not.toHaveBeenCalledWith(expect.any(Error));
  // Rows, markers and the cursor reached the core.
  const methods = mocks.core.calls.map((call) => call.method);
  expect(methods).toContain("projection.putRows");
  expect(await p.client.readSubscriptionCursor({ kind: "state" })).toEqual({
    workspaceId,
    seq: 5,
  });
  expect(
    mocks.syncGet.mock.calls.filter(([path]) => path !== "/api/sync/identity"),
  ).toEqual([]);
});

it("resumes from the core after a page restart: cursor and snapshot without a server read", async () => {
  mocks.core = createNativeCoreFixture();
  const first = await page();
  first.delivery.receive(first.frame(7, "Before restart"));
  await vi.waitFor(() =>
    expect(first.snapshots.at(-1)?.threads[0]?.name).toBe("Before restart"),
  );
  for (const stop of stops.splice(0)) stop();
  vi.resetModules();
  mocks.syncGet.mockReset();
  const restarted = await page();
  await vi.waitFor(() =>
    expect(restarted.snapshots.at(-1)?.threads[0]?.name).toBe("Before restart"),
  );
  expect(
    await restarted.client.readSubscriptionCursor({ kind: "state" }),
  ).toEqual({
    workspaceId,
    seq: 7,
  });
  expect(
    mocks.syncGet.mock.calls.filter(([path]) => path !== "/api/sync/identity"),
  ).toEqual([]);
});

it("applies a newer delta over the stored cursor and keeps the older snapshot rows", async () => {
  mocks.core = createNativeCoreFixture();
  const p = await page();
  p.delivery.receive(p.frame(3, "One"));
  await vi.waitFor(() =>
    expect(p.snapshots.at(-1)?.threads[0]?.name).toBe("One"),
  );
  const delta = p.frame(4, "Two");
  const entry = delta.resourceVersions[0]!;
  entry.replyKind = "delta";
  entry.entityChanges!.after = 3;
  delta.reason = "change";
  p.delivery.receive(delta);
  await vi.waitFor(() =>
    expect(p.snapshots.at(-1)?.threads[0]?.name).toBe("Two"),
  );
  expect(await p.client.readSubscriptionCursor({ kind: "state" })).toEqual({
    workspaceId,
    seq: 4,
  });
});
