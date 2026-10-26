import assert from "node:assert/strict";
import { afterEach, expect, it, vi } from "vitest";
import type { RxDatabase } from "rxdb";
import type { Snapshot } from "../types";
import { SYNC_PROTOCOL_VERSION, type components } from "../generated/api";
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
  sources: [] as {
    url: string;
    closed: boolean;
    emit(name: string, value: unknown): void;
  }[],
}));
vi.mock("../servers/eventSource", () => ({
  openServerEvents: vi.fn(),
  SignedEventSource: class {
    listeners = new Map<string, (event: Event) => void>();
    onopen = null;
    onerror = null;
    closed = false;
    readonly url: string;
    constructor(url: string) {
      this.url = url;

      mocks.sources.push(this);
    }
    addEventListener(name: string, accept: (event: Event) => void) {
      this.listeners.set(name, accept);
    }
    close() {
      this.closed = true;
    }
    emit(name: string, value: unknown) {
      if (!this.closed)
        this.listeners.get(name)?.({
          data: JSON.stringify(value),
        } as MessageEvent);
    }
  },
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
  mocks.sources = [];
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
    assert.ok(entry != null);
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
  expect(f.snapshots.at(-1)?.entityWorkspaceId).toBe(f.workspaceId);
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
  assert.ok(frame.resourceVersions[0] != null);
  assert.ok(frame.resources[0]);
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

it.each([false, true])(
  "snapshot staged=%s preserves a newer independently subscribed row and evicts only covered versions",
  async (staged) => {
    const f = await setup();
    const tasks = { kind: "tasks", agentId: "lead" } as const;
    const newer = { kind: "task", taskId: "newer" } as const;
    const snapshot = (
      resource: ResourceChangeEvent["resources"][number],
      through: number,
      documents: components["schemas"]["EntityChangeBatch"]["documents"],
    ): ResourceChangeEvent => ({
      ...f.frame(through),
      resources: [resource],
      resourceVersions: [
        {
          resource,
          revision: through,
          cursor: { workspaceId: f.workspaceId, seq: through },
          replyKind: "snapshot",
          entityChanges: { after: 0, through, documents },
        },
      ],
    });
    const row = (
      id: string,
      seq: number,
    ): components["schemas"]["EntityChangeBatch"]["documents"][number] => ({
      collection: "task",
      op: "upsert",
      id,
      seq,
      value: {
        id,
        agent: "lead",
        kind: "command",
        status: "running",
        created: 1,
      },
    });
    await f.client.persistResourceEntityChanges(
      snapshot(tasks, 9, [row("older", 9)]),
      () => true,
    );
    await f.client.persistResourceEntityChanges(
      snapshot({ kind: "task", taskId: "equal" }, 10, [row("equal", 10)]),
      () => true,
    );
    // This replacement was captured before the separate task scope committed 11.
    // Its own applied cursor is still 9, so the same-ref stale guard cannot hide the race.
    const captured = snapshot(tasks, 10, []);
    const { EntityTransferAssembler } = await import("./entityTransfer");
    const assembler = new EntityTransferAssembler();
    let interval: Awaited<ReturnType<typeof assembler.receive>>;
    try {
      assert.ok(captured.resourceVersions[0] != null);
      if (staged)
        interval = await assembler.receive({
          protocol: captured.protocol,
          workspaceId: f.workspaceId,
          epoch: captured.epoch,
          revision: captured.revision,
          reason: captured.reason,
          resource: tasks,
          cursor: { workspaceId: f.workspaceId, seq: 10 },
          replyKind: "snapshot",
          transferId: crypto.randomUUID(),
          pageIndex: 0,
          partIndex: 0,
          partCount: 1,
          complete: true,
          data: JSON.stringify(captured.resourceVersions[0].entityChanges),
        });
      await f.client.persistResourceEntityChanges(
        snapshot(newer, 11, [row("newer", 11)]),
        () => true,
      );
      if (staged) {
        expect(interval).toBeDefined();
        await f.client.persistStagedEntityTransfer(interval!, () => true);
      } else await f.client.persistResourceEntityChanges(captured, () => true);
      expect(
        (await f.db.projections.findOne("entity:task:newer").exec())?.toJSON(),
      ).toMatchObject({ seq: 11, entity: { id: "newer" } });
      expect(await f.client.readSubscriptionCursor(newer)).toEqual({
        workspaceId: f.workspaceId,
        seq: 11,
      });
      expect(await f.client.readSubscriptionCursor(tasks)).toEqual({
        workspaceId: f.workspaceId,
        seq: 10,
      });
      for (const [id, seq] of [
        ["older", 9],
        ["equal", 10],
      ] as const) {
        const [removed] =
          await f.db.projections.storageInstance.findDocumentsById(
            [`entity:task:${id}`],
            true,
          );
        expect(removed).toMatchObject({ seq, _deleted: true, evicted: true });
      }
      expect(f.pulls()).toEqual([]);
    } finally {
      await interval?.dispose();
      await assembler.invalidate();
    }
  },
);

it.each([false, true])(
  "the actual Work adapter recovers a failed inline baseline and queued delta through SSE (durable base=%s)",
  async (durableBase) => {
    const f = await setup();
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("BroadcastChannel", undefined);
    const { createServerSubscriptions } =
      await import("./subscriptions/server");
    const api = await import("../api");
    const subscriptions = createServerSubscriptions({
      owner: { id: "local", origin: "http://studio.test" },
      api: { ...api, fetchApiRequest: vi.fn() },
      tokenRates: {
        configureTokenRateStream: vi.fn(),
        receiveResourceTokenRates: vi.fn(() => true),
        getWorkspaceTokenRates: vi.fn(() => ({ records: [] })),
      },
      sync: () => f.client,
    } as unknown as Parameters<typeof createServerSubscriptions>[0]);
    const ref = { kind: "work", agentId: "lead" } as const;
    const stop = subscriptions.subscribe(ref, () => {}, { graceMs: 0 });
    stops.push(stop, subscriptions.dispose);
    await vi.waitFor(() => expect(mocks.sources).toHaveLength(1));
    const source = mocks.sources[0];
    assert.ok(source != null);
    source.emit("api-schema", { hash: API_SCHEMA_HASH });
    source.emit("session", { sessionId: "quiet-work-session" });
    const frame = (seq: number, snapshot: boolean): ResourceChangeEvent => ({
      protocol: SYNC_PROTOCOL_VERSION,
      workspaceId: f.workspaceId,
      epoch: "server",
      revision: seq,
      reason: snapshot ? "initial" : "change",
      resources: [ref],
      resourceVersions: [
        {
          resource: ref,
          revision: seq,
          cursor: { workspaceId: f.workspaceId, seq },
          replyKind: snapshot ? "snapshot" : "delta",
          entityChanges: {
            after: snapshot ? 0 : 5,
            through: seq,
            documents: [
              {
                op: "upsert",
                collection: "workDetail",
                id: "assignment",
                seq,
                value: {
                  id: "assignment",
                  rootId: "lead",
                  title: "Assignment",
                  created: 1,
                  updated: 1,
                  version: 1,
                  status: snapshot && seq === 5 ? "ready" : "review",
                },
              },
            ],
          },
        },
      ],
    });
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    if (durableBase)
      await f.client.persistResourceEntityChanges(frame(4, true), () => true);
    vi.spyOn(
      f.db.projections.storageInstance,
      "bulkWrite",
    ).mockRejectedValueOnce(new Error("inline storage unavailable"));
    source.emit("resources", frame(5, true));
    source.emit("resources", frame(6, false));
    await vi.waitFor(() => expect(mocks.sources).toHaveLength(2), {
      timeout: 2500,
    });
    expect(source.closed).toBe(true);
    expect(await f.client.readSubscriptionCursor(ref)).toEqual(
      durableBase ? { workspaceId: f.workspaceId, seq: 4 } : undefined,
    );
    const replacement = mocks.sources[1];
    assert.ok(replacement != null);
    const query = new URL(replacement.url).searchParams;
    if (durableBase)
      expect(JSON.parse(query.get("since")!)).toContainEqual({
        resource: ref,
        since: { workspaceId: f.workspaceId, seq: 4 },
      });
    else
      expect(JSON.parse(query.get("since")!)).not.toContainEqual(
        expect.objectContaining({ resource: ref }),
      );
    replacement.emit("api-schema", { hash: API_SCHEMA_HASH });
    replacement.emit("resources", frame(6, true));
    await vi.waitFor(() =>
      expect(subscriptions.getSnapshot(ref)).toMatchObject({
        status: "live",
        data: { items: [expect.objectContaining({ status: "review" })] },
      }),
    );
    expect(subscriptions.getSnapshot(ref).status).toBe("live");
    expect(await f.client.readSubscriptionCursor(ref)).toEqual({
      workspaceId: f.workspaceId,
      seq: 6,
    });
    expect(f.pulls()).toEqual([]);
    expect(
      mocks.syncGet.mock.calls.every(([path]) => path === "/api/sync/identity"),
    ).toBe(true);
    error.mockRestore();
  },
);

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
          ) =>
            callback
              ? new Promise<void>((_resolve, reject) => {
                  (options as { signal: AbortSignal }).signal.addEventListener(
                    "abort",
                    () => reject(new DOMException("Aborted", "AbortError")),
                    { once: true },
                  );
                })
              : (options as () => unknown)(),
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
      const validatedAt = Date.now();
      peer({
        kind: "resource-event",
        apiSchemaHash: API_SCHEMA_HASH,
        workspaceId: f.workspaceId,
        tabId: "owner",
        health: {
          status: "live",
          generation: 1,
          sequence: 1,
          sentAt: validatedAt,
          validatedAt,
        },
        event: f.frame(5),
      });
    } else {
      await vi.waitFor(() =>
        expect(source.listeners.has("resources")).toBe(true),
      );
      source.emit("api-schema", { hash: API_SCHEMA_HASH });
      if (staged) {
        const frame = f.frame(5);
        assert.ok(frame.resourceVersions[0] != null);
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
      assert.ok(persistStaged.mock.results[0] != null);
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

it.each(["overflow", "inline removal", "staged removal"])(
  "the actual Work adapter recovers a still-active interval discarded by %s",
  async (cause) => {
    const f = await setup();
    vi.stubGlobal("location", { origin: "http://studio.test" });
    vi.stubGlobal("BroadcastChannel", undefined);
    const { createServerSubscriptions } =
      await import("./subscriptions/server");
    const api = await import("../api");
    const subscriptions = createServerSubscriptions({
      owner: { id: "local", origin: "http://studio.test" },
      api: { ...api, fetchApiRequest: vi.fn() },
      tokenRates: {
        configureTokenRateStream: vi.fn(),
        receiveResourceTokenRates: vi.fn(() => true),
        getWorkspaceTokenRates: vi.fn(() => ({ records: [] })),
      },
      sync: () => f.client,
    } as unknown as Parameters<typeof createServerSubscriptions>[0]);
    const ref = { kind: "work", agentId: "lead" } as const;
    const stop = subscriptions.subscribe(ref, () => {}, { graceMs: 0 });
    const others = Array.from(
      { length: 10 },
      (_, index) =>
        ({ kind: "agentDetail", agentId: `other-${index}` }) as const,
    );
    const stopOthers = others.map((other) =>
      subscriptions.watchResourceChanges(other, () => {}, { graceMs: 0 }),
    );
    stops.push(stop, ...stopOthers, subscriptions.dispose);
    await vi.waitFor(() => expect(mocks.sources).toHaveLength(1));
    const source = mocks.sources[0];
    assert.ok(source != null);
    source.emit("api-schema", { hash: API_SCHEMA_HASH });
    let finish!: () => void;
    source.emit("session", { sessionId: "quiet-work-discard-session" });
    const gate = new Promise<void>((resolve) => {
      finish = resolve;
    });
    stops.push(finish);
    // Keep the held storage write independent of wall-clock scheduling.
    // The test controls cancellation and reconnect, not the persistence timeout.
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const started = Promise.withResolvers<void>();
    const staged = cause === "staged removal";
    const originalInline = f.client.persistResourceEntityChanges;
    const originalStaged = f.client.persistStagedEntityTransfer;
    const held = staged
      ? vi
          .spyOn(f.client, "persistStagedEntityTransfer")
          .mockImplementationOnce(async (...args) => {
            started.resolve();
            await gate;
            return originalStaged(...args);
          })
      : vi
          .spyOn(f.client, "persistResourceEntityChanges")
          .mockImplementationOnce(async (...args) => {
            started.resolve();
            await gate;
            return originalInline(...args);
          });
    const frame: ResourceChangeEvent = {
      protocol: SYNC_PROTOCOL_VERSION,
      workspaceId: f.workspaceId,
      epoch: "server",
      revision: 5,
      reason: "initial",
      resources: [ref],
      resourceVersions: [
        {
          resource: ref,
          revision: 5,
          cursor: { workspaceId: f.workspaceId, seq: 5 },
          replyKind: "snapshot",
          entityChanges: {
            after: 0,
            through: 5,
            documents: [
              {
                op: "upsert",
                collection: "workDetail",
                id: "assignment",
                seq: 5,
                value: {
                  id: "assignment",
                  rootId: "lead",
                  title: "Quiet assignment",
                  created: 1,
                  updated: 1,
                  version: 1,
                  status: "ready",
                },
              },
            ],
          },
        },
      ],
    };
    try {
      assert.ok(source != null);
      assert.ok(frame.resourceVersions[0] != null);
      if (staged)
        source.emit("entity-transfer", {
          protocol: frame.protocol,
          workspaceId: f.workspaceId,
          epoch: frame.epoch,
          revision: 5,
          reason: "initial",
          resource: ref,
          cursor: { workspaceId: f.workspaceId, seq: 5 },
          replyKind: "snapshot",
          transferId: "quiet-work",
          pageIndex: 0,
          partIndex: 0,
          partCount: 1,
          complete: true,
          data: JSON.stringify(frame.resourceVersions[0].entityChanges),
        });
      else source.emit("resources", frame);
      await started.promise;
      expect(held).toHaveBeenCalledOnce();
      assert.ok(stopOthers[0] != null);
      if (cause === "overflow") {
        assert.ok(source != null);
        for (const [index, other] of others.entries())
          source.emit("resources", {
            protocol: frame.protocol,
            workspaceId: f.workspaceId,
            epoch: frame.epoch,
            revision: 6 + index,
            reason: "initial",
            resources: [other],
            resourceVersions: [
              {
                resource: other,
                revision: 6 + index,
                cursor: { workspaceId: f.workspaceId, seq: 6 + index },
                replyKind: "snapshot",
                entityChanges: { after: 0, through: 6 + index, documents: [] },
              },
            ],
          });
      } else stopOthers[0]();
      expect(source.closed).toBe(true);
      finish();
      // A replacement stream must not race the canceled write still owning RxDB.
      assert.ok(held.mock.results[0] != null);
      await held.mock.results[0].value;
      await vi.advanceTimersByTimeAsync(0);
      await vi.waitFor(() => expect(mocks.sources).toHaveLength(2), {
        timeout: 2500,
      });
      expect(await f.client.readSubscriptionCursor(ref)).toBeUndefined();
      const replacement = mocks.sources[1];
      assert.ok(replacement != null);
      replacement.emit("api-schema", { hash: API_SCHEMA_HASH });
      replacement.emit("resources", structuredClone(frame));
      await vi.waitFor(() =>
        expect(subscriptions.getSnapshot(ref)).toMatchObject({
          status: "live",
          data: {
            items: [expect.objectContaining({ title: "Quiet assignment" })],
          },
        }),
      );
      expect(f.pulls()).toEqual([]);
      expect(
        mocks.syncGet.mock.calls.every(
          ([path]) => path === "/api/sync/identity",
        ),
      ).toBe(true);
    } finally {
      subscriptions.dispose();
      finish();
      await Promise.allSettled(held.mock.results.map((result) => result.value));
      await vi.advanceTimersByTimeAsync(0);
      vi.clearAllTimers();
      vi.useRealTimers();
    }
  },
);
