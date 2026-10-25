import { afterEach, expect, it, vi } from "vitest";
import type { RxDatabase } from "rxdb";
import type { Snapshot } from "../types";
import { SYNC_PROTOCOL_VERSION } from "../generated/api";
import type {
  ResourceChangeEvent,
  ResourceConnectionState,
  ResourceStreamState,
  ResourceVersion,
} from "./subscriptions/events";
import { ResourceEntityDelivery } from "./resourceEntityDelivery";
import { createSubscriptionStore } from "./subscriptions/store";
import { API_SCHEMA_HASH } from "../generated/apiSchema";

const mocks = vi.hoisted(() => ({
  databases: [] as RxDatabase[],
  syncGet: vi.fn(),
  changes: new Set<(version?: ResourceVersion) => void>(),
  connections: new Set<
    (state: ResourceConnectionState, stream: ResourceStreamState) => void
  >(),
  watch: vi.fn(),
  watchConnection: vi.fn(),
  stagingWrite: vi.fn(),
}));
vi.mock("rxdb", async (original) => {
  const actual = await original<typeof import("rxdb")>();
  return {
    ...actual,
    createRxDatabase: async (
      options: Parameters<typeof actual.createRxDatabase>[0],
    ) => {
      const db = await actual.createRxDatabase({
        ...options,
        multiInstance: false,
      });
      mocks.databases.push(db);
      return db;
    },
  };
});
vi.mock("rxdb/plugins/storage-dexie", async () => ({
  getRxStorageDexie: (await import("rxdb/plugins/storage-memory"))
    .getRxStorageMemory,
}));
vi.mock("../api", () => ({
  ApiError: class extends Error {},
  syncGet: mocks.syncGet,
  get: mocks.syncGet,
  post: vi.fn(),
  clearSchemaUpdateAttemptAfterMatch: vi.fn(),
  matchingApiSchemaResponseGeneration: () => 0,
  markApiSchemaMismatch: vi.fn(),
  onApiSchemaMismatch: () => () => {},
  syncPost: vi.fn(),
  saved: (_key: string, fallback: unknown) => fallback,
  save: vi.fn(),
  setWorkspace: vi.fn(),
  isApiSchemaMismatch: () => false,
  isApiSchemaMismatchHandshake: () => false,
  onMatchingApiSchemaResponse: () => () => {},
}));
vi.mock("./subscriptions/events", () => ({
  acknowledgeEntitySequences: vi.fn(),
  watchResourceChanges: mocks.watch,
  watchResourceConnection: mocks.watchConnection,
}));
vi.mock("../usage/tokenRate", () => ({
  configureTokenRateStream: vi.fn(),
  receiveResourceTokenRates: vi.fn(),
  getWorkspaceTokenRates: vi.fn(() => ({ records: [] })),
}));
vi.mock("./resume", () => ({ onResume: () => () => {} }));
vi.mock("./entityTransfer", async (original) => {
  const actual = await original<typeof import("./entityTransfer")>();
  return {
    ...actual,
    EntityTransferAssembler: class extends actual.EntityTransferAssembler {
      constructor() {
        const rows = new Map<string, string>();
        super({
          async put(key, value) {
            rows.set(key, value);
            mocks.stagingWrite();
          },
          async putMany(values) {
            for (const [key, value] of values) rows.set(key, value);
          },
          async get(key) {
            return rows.get(key);
          },
          async remove(prefix) {
            for (const key of rows.keys())
              if (key.startsWith(prefix)) rows.delete(key);
          },
          async close() {
            rows.clear();
          },
        });
      }
    },
  };
});
vi.mock("./entityCacheStorage", async (original) => ({
  ...(await original<typeof import("./entityCacheStorage")>()),
  holdEntityProjectionDatabaseLock: async () => true,
  deleteOtherEntityProjectionDatabases: async () => {},
}));

const stops: (() => void)[] = [];
afterEach(async () => {
  for (const stop of stops.splice(0)) stop();
  await Promise.all(mocks.databases.splice(0).map((db) => db.close()));
  mocks.syncGet.mockReset();
  mocks.watch.mockReset();
  mocks.watchConnection.mockReset();
  mocks.stagingWrite.mockReset();
  mocks.changes.clear();
  mocks.connections.clear();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  vi.resetModules();
});

async function setup() {
  const workspaceId = crypto.randomUUID().replaceAll("-", "");
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
    locks: {
      request: (_key: string, write: () => Promise<unknown>) => write(),
    },
  });
  mocks.syncGet.mockImplementation(async (path: string) =>
    path === "/api/sync/identity"
      ? { workspaceId }
      : {
          workspaceId,
          documents: [],
          checkpoint: { seq: 0 },
          initialHigh: 0,
          maxSeq: 0,
        },
  );
  mocks.watch.mockImplementation((_ref, callback) => {
    mocks.changes.add(callback);
    return () => mocks.changes.delete(callback);
  });
  mocks.watchConnection.mockImplementation((listener) => {
    mocks.connections.add(listener);
    return () => mocks.connections.delete(listener);
  });
  const client = await import("./client");
  const { db } = await client.syncDatabase();
  const snapshots: Snapshot[] = [];
  const failed = vi.fn();
  const subscribe = () => {
    const stop = client.subscribeStateProjection((value) => {
      if (value) snapshots.push(value);
    }, failed);
    stops.push(stop);
    return stop;
  };
  const pulls = () =>
    mocks.syncGet.mock.calls.filter(([path]) => path === "/api/sync/pull");
  const notify = (version?: ResourceVersion) => {
    for (const callback of mocks.changes) callback(version);
  };
  const dispatch = vi.fn((event: ResourceChangeEvent) => {
    const entry = event.resourceVersions[0];
    notify({
      epoch: event.epoch,
      revision: entry.revision,
      reason: event.reason,
      cursor: entry.cursor ?? undefined,
      replyKind: entry.replyKind ?? undefined,
      entitySequence: entry.revision,
    });
  });
  const delivery = new ResourceEntityDelivery(
    client.persistResourceEntityChanges,
    dispatch,
  );
  const frame = (
    seq: number,
    epoch = "first",
    name = "SSE baseline",
  ): ResourceChangeEvent => ({
    protocol: SYNC_PROTOCOL_VERSION,
    workspaceId,
    epoch,
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
  const connection = (
    state: ResourceConnectionState,
    stream: ResourceStreamState = "open",
  ) => {
    for (const callback of mocks.connections) callback(state, stream);
  };
  return {
    client,
    db,
    workspaceId,
    snapshots,
    failed,
    subscribe,
    pulls,
    notify,
    dispatch,
    delivery,
    frame,
    connection,
  };
}

it("waits for the applied cold SSE baseline and publishes it without a speculative pull", async () => {
  const f = await setup();
  f.subscribe();
  await vi.waitFor(() => expect(mocks.watch).toHaveBeenCalledOnce());
  expect(mocks.watch).toHaveBeenCalledWith(
    { kind: "state" },
    expect.any(Function),
    { waitForBaseline: true },
  );
  f.delivery.receive(f.frame(5));
  await vi.waitFor(() =>
    expect(f.snapshots.at(-1)?.threads[0]?.name).toBe("SSE baseline"),
  );
  expect(f.snapshots.at(-1)?.entitiesComplete).toBe(true);
  expect(f.dispatch).toHaveBeenCalledOnce();
  expect(f.pulls()).toEqual([]);
  expect(
    mocks.syncGet.mock.calls.filter(([path]) => path === "/api/sync/identity"),
  ).toHaveLength(1);
});

it("accepts applied reconnect and process-restart snapshots without recovery reads", async () => {
  const f = await setup();
  f.subscribe();
  await vi.waitFor(() => expect(mocks.watch).toHaveBeenCalledOnce());
  f.delivery.receive(f.frame(8));
  await vi.waitFor(() =>
    expect(f.snapshots.at(-1)?.threads[0]?.name).toBe("SSE baseline"),
  );
  f.connection("degraded");
  f.delivery.receive({ ...f.frame(8), reason: "reconnect" });
  await vi.waitFor(() => expect(f.dispatch).toHaveBeenCalledTimes(2));
  f.delivery.receive({
    ...f.frame(9, "replacement", "Restarted"),
    reason: "reconnect",
  });
  await vi.waitFor(() =>
    expect(f.snapshots.at(-1)?.threads[0]?.name).toBe("Restarted"),
  );
  expect(f.pulls()).toEqual([]);
  expect(await f.client.readSubscriptionCursor({ kind: "state" })).toEqual({
    workspaceId: f.workspaceId,
    seq: 9,
  });
});

it("keeps a partial scoped cache independent from canonical state readiness", async () => {
  const f = await setup();
  const frame = f.frame(3);
  frame.resources = [{ kind: "agentDetail", agentId: "lead" }];
  frame.resourceVersions[0].resource = frame.resources[0];
  frame.resourceVersions[0].entityChanges!.documents = [
    {
      op: "upsert",
      collection: "agentDetail",
      id: "lead",
      seq: 3,
      value: { id: "lead", canSend: true },
    },
  ];
  await f.client.persistResourceEntityChanges(frame, () => true);
  expect(await f.client.readSubscriptionCursor(frame.resources[0])).toEqual({
    workspaceId: f.workspaceId,
    seq: 3,
  });
  expect(
    await f.client.readSubscriptionCursor({ kind: "state" }),
  ).toBeUndefined();
  f.subscribe();
  await vi.waitFor(() => expect(mocks.watch).toHaveBeenCalledOnce());
  f.delivery.receive(f.frame(5));
  await vi.waitFor(() =>
    expect(f.snapshots.at(-1)?.entitiesComplete).toBe(true),
  );
  expect(f.pulls()).toEqual([]);
});

it.each(["revision-only", "overflow", "persistence failure"] as const)(
  "%s explicitly starts one existing bounded recovery read",
  async (reason) => {
    const f = await setup();
    f.subscribe();
    await vi.waitFor(() => expect(mocks.watch).toHaveBeenCalledOnce());
    mocks.syncGet.mockClear();
    if (reason === "persistence failure") {
      vi.spyOn(console, "error").mockImplementation(() => {});
      vi.spyOn(
        f.db.projections.storageInstance,
        "bulkWrite",
      ).mockRejectedValueOnce(new Error("storage unavailable"));
      f.delivery.receive(f.frame(5));
    } else {
      const frame = f.frame(5);
      frame.reason = reason === "overflow" ? "overflow" : "initial";
      frame.resourceVersions = [{ revision: 5 }];
      f.delivery.receive(frame);
    }
    await vi.waitFor(() => expect(f.pulls()).toHaveLength(1));
    await vi.waitFor(() =>
      expect(f.snapshots.at(-1)?.entitiesComplete).toBe(true),
    );
    expect(f.pulls()).toHaveLength(1);
    expect(f.failed.mock.calls.filter(([error]) => error !== null)).toEqual([]);
  },
);

it.each(["degraded", "offline", "stopped"] as const)(
  "cold %s transport starts recovery instead of leaving the watcher loading",
  async (state) => {
    const f = await setup();
    f.subscribe();
    await vi.waitFor(() => expect(mocks.watch).toHaveBeenCalledOnce());
    f.connection(
      state === "stopped" ? "connecting" : state,
      state === "stopped" ? "stopped" : "open",
    );
    await vi.waitFor(() =>
      expect(f.snapshots.at(-1)?.entitiesComplete).toBe(true),
    );
    expect(f.pulls()).toHaveLength(1);
    // Repeated transport state is not a new recovery notification.
    f.connection(
      state === "stopped" ? "connecting" : state,
      state === "stopped" ? "stopped" : "open",
    );
    expect(f.pulls()).toHaveLength(1);
  },
);

it("does not use HTTP recovery to bypass a schema-mismatched stream", async () => {
  const f = await setup();
  f.subscribe();
  await vi.waitFor(() => expect(mocks.watch).toHaveBeenCalledOnce());
  f.connection("schema-mismatch", "stopped");
  expect(f.pulls()).toEqual([]);
  expect(f.snapshots).toEqual([]);
});

it.each([
  { follower: false, staged: false, cancelled: false },
  { follower: true, staged: false, cancelled: false },
  { follower: false, staged: true, cancelled: false },
  { follower: false, staged: true, cancelled: true },
])(
  "transport follower=$follower staged=$staged cancelled=$cancelled publishes readiness only after the actual RxDB receipt",
  async ({ follower, staged, cancelled }) => {
    const f = await setup();
    vi.stubGlobal("location", { origin: "http://studio.test" });
    let peer!: (value: unknown) => void;
    vi.stubGlobal(
      "BroadcastChannel",
      follower
        ? class {
            onmessage: ((event: MessageEvent) => void) | null = null;
            constructor() {
              peer = (value) =>
                this.onmessage?.({ data: value } as MessageEvent);
            }
            postMessage() {}
            close() {}
          }
        : undefined,
    );
    if (follower)
      vi.stubGlobal("navigator", {
        onLine: true,
        locks: {
          request: (
            _key: string,
            options: unknown,
            callback?: (lock: null) => unknown,
          ) => (callback ? callback(null) : (options as () => unknown)()),
        },
      });
    class Source {
      listeners = new Map<string, (event: Event) => void>();
      onerror = null;
      onopen = null;
      addEventListener(name: string, accept: (event: Event) => void) {
        this.listeners.set(name, accept);
      }
      close() {}
      emit(name: string, value: unknown) {
        this.listeners.get(name)?.({
          data: JSON.stringify(value),
        } as MessageEvent);
      }
    }
    const source = new Source();
    let finish!: () => void;
    const gate = new Promise<void>((resolve) => {
      finish = resolve;
    });
    const persist = vi.fn(
      async (
        ...args: Parameters<typeof f.client.persistResourceEntityChanges>
      ) => {
        await gate;
        await f.client.persistResourceEntityChanges(...args);
      },
    );
    const persistStaged = vi.fn(
      async (
        ...args: Parameters<typeof f.client.persistStagedEntityTransfer>
      ) => {
        await gate;
        await f.client.persistStagedEntityTransfer(...args);
      },
    );
    const { createResourceEvents } = await import("./resourceEvents");
    const transport = createResourceEvents({
      api: {
        get: mocks.syncGet,
        post: vi.fn().mockResolvedValue({ gen: 1 }),
        clearSchemaUpdateAttemptAfterMatch: vi.fn(),
        isApiSchemaMismatch: () => false,
        matchingApiSchemaResponseGeneration: () => 0,
        markApiSchemaMismatch: vi.fn(),
        onApiSchemaMismatch: () => () => {},
      },
      sync: {
        syncDatabase: f.client.syncDatabase,
        persistResourceEntityChanges: persist,
        readSubscriptionCursor: f.client.readSubscriptionCursor,
        persistStagedEntityTransfer: persistStaged,
      },
      tokenRates: {
        configureTokenRateStream: vi.fn(),
        receiveResourceTokenRates: vi.fn(() => true),
        getWorkspaceTokenRates: vi.fn(() => ({ records: [] })),
      },
      openEvents: () => source,
      storageName: (name) => name,
    });
    mocks.watch.mockImplementation(transport.watchResourceChanges);
    mocks.watchConnection.mockImplementation(transport.watchResourceConnection);
    const stopProjection = f.subscribe();
    const store = createSubscriptionStore(transport);
    store.watchResourceChanges({ kind: "state" }, () => {});
    stops.push(finish, store.dispose, transport.dispose);
    if (follower) {
      await vi.waitFor(() => expect(peer).toBeTypeOf("function"));
      peer({
        kind: "resource-event",
        apiSchemaHash: API_SCHEMA_HASH,
        workspaceId: f.workspaceId,
        tabId: "owner",
        event: f.frame(5),
      });
    } else {
      await vi.waitFor(() =>
        expect(source.listeners.has("resources")).toBe(true),
      );
      source.emit("api-schema", { hash: API_SCHEMA_HASH });
      if (staged) {
        const frame = f.frame(5);
        const encoded = JSON.stringify(frame.resourceVersions[0].entityChanges);
        const split = Math.floor(encoded.length / 2);
        const metadata = {
          protocol: frame.protocol,
          workspaceId: f.workspaceId,
          epoch: frame.epoch,
          revision: frame.revision,
          reason: frame.reason,
          resource: frame.resources[0],
          cursor: { workspaceId: f.workspaceId, seq: 5 },
          replyKind: "snapshot",
          transferId: "cold-resource",
          pageIndex: 0,
          partCount: 2,
        };
        source.emit("entity-transfer", {
          ...metadata,
          partIndex: 0,
          complete: false,
          data: encoded.slice(0, split),
        });
        await vi.waitFor(() =>
          expect(mocks.stagingWrite).toHaveBeenCalledOnce(),
        );
        expect(persistStaged).not.toHaveBeenCalled();
        expect(
          await f.client.readSubscriptionCursor({ kind: "state" }),
        ).toBeUndefined();
        expect(f.snapshots).toEqual([]);
        expect(f.pulls()).toEqual([]);
        source.emit("entity-transfer", {
          ...metadata,
          partIndex: 1,
          complete: true,
          data: encoded.slice(split),
        });
      } else source.emit("resources", f.frame(5));
    }
    await vi.waitFor(() =>
      expect(staged ? persistStaged : persist).toHaveBeenCalledOnce(),
    );
    expect(store.getSnapshot({ kind: "state" }).status).not.toBe("live");
    expect(f.snapshots).toEqual([]);
    expect(f.pulls()).toEqual([]);
    if (staged) expect(persist).not.toHaveBeenCalled();
    if (cancelled) {
      // The screen releases its watcher before the server module closes.
      // A still-active cold watcher intentionally recovers a stopped stream.
      stopProjection();
      transport.dispose();
      finish();
      await persistStaged.mock.results[0].value;
      expect(
        await f.client.readSubscriptionCursor({ kind: "state" }),
      ).toBeUndefined();
      expect(store.getSnapshot({ kind: "state" }).status).not.toBe("live");
      expect(f.snapshots).toEqual([]);
      expect(f.pulls()).toEqual([]);
      return;
    }
    finish();
    await vi.waitFor(() =>
      expect(store.getSnapshot({ kind: "state" }).status).toBe("live"),
    );
    await vi.waitFor(() =>
      expect(f.snapshots.at(-1)?.threads[0]?.name).toBe("SSE baseline"),
    );
    expect(await f.client.readSubscriptionCursor({ kind: "state" })).toEqual({
      workspaceId: f.workspaceId,
      seq: 5,
    });
    expect(f.pulls()).toEqual([]);
  },
);
