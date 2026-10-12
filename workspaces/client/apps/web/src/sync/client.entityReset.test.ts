import { afterEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  createRxDatabase: vi.fn(),
  syncGet: vi.fn(),
  watchResourceChanges: vi.fn(),
  registerPersister: vi.fn(),
  deleteOtherCaches: vi.fn(),
  holdProjectionDatabaseLock: vi.fn(async () => true),
  isApiSchemaMismatch: vi.fn(() => false),
  cachedWorkspace: "",
  schemaHash: "a".repeat(64),
  matchingSchemaListener: undefined as (() => void) | undefined,
  invalidation: undefined as
    | ((version?: {
        epoch: string;
        entitySequence?: number;
        entitySequenceReset?: boolean;
      }) => void)
    | undefined,
}));
vi.mock("../generated/apiSchema", () => ({
  get API_SCHEMA_HASH() {
    return mocks.schemaHash;
  },
  API_SCHEMA_HASH_HEADER: "X-Studio-API-Schema",
}));

vi.mock("rxdb", () => ({
  addRxPlugin: vi.fn(),
  createRxDatabase: mocks.createRxDatabase,
}));
vi.mock("rxdb/plugins/storage-dexie", () => ({
  getRxStorageDexie: vi.fn(() => ({})),
}));
vi.mock("rxdb/plugins/leader-election", () => ({
  RxDBLeaderElectionPlugin: {},
}));
vi.mock("rxdb/plugins/replication", () => ({
  replicateRxCollection: vi.fn(),
}));
vi.mock("../api", () => ({
  ApiError: class ApiError extends Error {
    constructor(
      message: string,
      public status: number,
    ) {
      super(message);
    }
  },
  registerSyncEntityPersister: mocks.registerPersister,
  isApiSchemaMismatch: mocks.isApiSchemaMismatch,
  onMatchingApiSchemaResponse: vi.fn((listener: () => void) => {
    mocks.matchingSchemaListener = listener;
    return () => {
      if (mocks.matchingSchemaListener === listener)
        mocks.matchingSchemaListener = undefined;
    };
  }),
  save: vi.fn(),
  saved: (key: string, fallback: unknown) =>
    key === "codex-sync-workspace"
      ? mocks.cachedWorkspace || fallback
      : fallback,
  setWorkspace: vi.fn(),
  syncGet: mocks.syncGet,
  syncPost: vi.fn(),
}));
vi.mock("./resourceEvents", () => ({
  acknowledgeEntitySequences: vi.fn(),
  watchResourceChanges: mocks.watchResourceChanges,
  watchResourceConnection: vi.fn(() => () => {}),
}));
vi.mock("./resume", () => ({ onResume: vi.fn(() => () => {}) }));
vi.mock("./entityCacheStorage", () => ({
  deleteOtherEntityProjectionDatabases: mocks.deleteOtherCaches,
  holdEntityProjectionDatabaseLock: mocks.holdProjectionDatabaseLock,
  entityProjectionDatabaseName: (workspace: string, hash: string) =>
    `studio-entity-projection-${workspace}-${hash}`,
}));

describe("entity pull reset wiring", () => {
  afterEach(() => {
    vi.resetModules();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    mocks.createRxDatabase.mockReset();
    mocks.syncGet.mockReset();
    mocks.watchResourceChanges.mockReset();
    mocks.registerPersister.mockReset();
    mocks.deleteOtherCaches.mockReset();
    mocks.holdProjectionDatabaseLock.mockReset();
    mocks.holdProjectionDatabaseLock.mockResolvedValue(true);
    mocks.isApiSchemaMismatch.mockReset();
    mocks.isApiSchemaMismatch.mockReturnValue(false);
    mocks.cachedWorkspace = "";
    mocks.schemaHash = "a".repeat(64);
    mocks.matchingSchemaListener = undefined;
    mocks.invalidation = undefined;
  });

  it("starts stale-cache cleanup only after matching schema confirmation", async () => {
    const workspaceId = "c".repeat(32);
    mocks.syncGet.mockResolvedValue({ workspaceId });
    const projections = {
      database: { name: `studio-entity-projection-${workspaceId}` },
      storageInstance: {},
      $: { subscribe: vi.fn(() => ({ unsubscribe: vi.fn() })) },
    };
    mocks.createRxDatabase
      .mockResolvedValueOnce({ addCollections: vi.fn(async () => ({})) })
      .mockResolvedValueOnce({
        addCollections: vi.fn(async () => ({ projections })),
      });
    vi.stubGlobal("window", { addEventListener: vi.fn() });
    vi.stubGlobal("navigator", { onLine: true });

    const client = await import("./client");
    await client.syncDatabase();
    expect(mocks.deleteOtherCaches).not.toHaveBeenCalled();
    expect(mocks.matchingSchemaListener).toBeTypeOf("function");

    mocks.matchingSchemaListener?.();
    expect(mocks.deleteOtherCaches).toHaveBeenCalledOnce();
    expect(mocks.deleteOtherCaches).toHaveBeenCalledWith(
      workspaceId,
      expect.stringMatching(/^[a-f0-9]{64}$/),
      "",
    );
  });

  it("opens cached state but never schedules cleanup when schema-mismatched", async () => {
    mocks.isApiSchemaMismatch.mockReturnValue(true);
    const workspaceId = "d".repeat(32);
    mocks.cachedWorkspace = workspaceId;
    mocks.syncGet.mockRejectedValue(new Error("schema mismatch"));
    const projections = {
      database: { name: `studio-entity-projection-${workspaceId}` },
      storageInstance: {},
      $: { subscribe: vi.fn(() => ({ unsubscribe: vi.fn() })) },
    };
    mocks.createRxDatabase
      .mockResolvedValueOnce({ addCollections: vi.fn(async () => ({})) })
      .mockResolvedValueOnce({
        addCollections: vi.fn(async () => ({ projections })),
      });
    vi.stubGlobal("window", { addEventListener: vi.fn() });
    vi.stubGlobal("navigator", { onLine: true });
    const client = await import("./client");

    await expect(client.syncDatabase()).resolves.toMatchObject({ workspaceId });
    expect(mocks.deleteOtherCaches).not.toHaveBeenCalled();
    expect(mocks.createRxDatabase).toHaveBeenCalledTimes(2);
  });

  it("refreshes after an epoch change, handles reset and persists the lower cursor", async () => {
    const workspaceId = "b".repeat(32);
    const rows = new Map<string, Record<string, unknown>>([
      [
        "state:entities:checkpoint",
        {
          id: "state:entities:checkpoint",
          payload: JSON.stringify({ initialHigh: 900 }),
          seq: 1000,
          _deleted: false,
        },
      ],
      [
        "state:entities:ready",
        {
          id: "state:entities:ready",
          payload: "ready",
          seq: 1,
          _deleted: false,
        },
      ],
      [
        "state:entities:initial",
        {
          id: "state:entities:initial",
          payload: "{}",
          seq: 900,
          _deleted: false,
        },
      ],
      [
        "state:entities:complete",
        {
          id: "state:entities:complete",
          payload: "complete",
          seq: 1,
          _deleted: false,
        },
      ],
    ]);
    const storageInstance = {
      findDocumentsById: vi.fn(async (ids: string[]) =>
        ids.flatMap((id) => (rows.has(id) ? [rows.get(id)!] : [])),
      ),
      bulkWrite: vi.fn(
        async (writes: Array<{ document: Record<string, unknown> }>) => {
          for (const { document } of writes)
            rows.set(String(document.id), document);
          return { error: [] };
        },
      ),
    };
    const subscription = { unsubscribe: vi.fn() };
    const projections = {
      database: { name: `studio-entity-projection-${workspaceId}` },
      storageInstance,
      $: { subscribe: vi.fn(() => subscription) },
      find: vi.fn(() => ({
        exec: vi.fn(async () => []),
        $: { subscribe: vi.fn(() => subscription) },
      })),
      findOne: vi.fn(() => ({
        $: { subscribe: vi.fn(() => subscription) },
      })),
    };
    const stableDatabase = {
      addCollections: vi.fn(async () => ({})),
    };
    const projectionDatabase = {
      addCollections: vi.fn(async () => ({ projections })),
    };
    mocks.createRxDatabase
      .mockResolvedValueOnce(stableDatabase)
      .mockResolvedValueOnce(projectionDatabase);
    const pulls: number[] = [];
    mocks.syncGet.mockImplementation(
      async (path: string, options?: { query?: { after?: number } }) => {
        if (path === "/api/sync/identity") return { workspaceId };
        const after = options?.query?.after ?? 0;
        pulls.push(after);
        if (after === 1000)
          return {
            workspaceId,
            reset: true,
            floor: 0,
            maxSeq: 900,
          };
        return {
          workspaceId,
          documents: [],
          checkpoint: { seq: 900 },
          maxSeq: 900,
          initialHigh: 900,
        };
      },
    );
    mocks.watchResourceChanges.mockImplementation(
      (resource: unknown, listener: typeof mocks.invalidation) => {
        if ((resource as { kind?: string }).kind === "state")
          mocks.invalidation = listener;
        return () => {};
      },
    );
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      matchMedia: vi.fn(() => ({ matches: false })),
    });
    vi.stubGlobal("document", { hidden: false });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });

    const client = await import("./client");
    const stop = client.subscribeStateProjection(vi.fn(), vi.fn());
    await vi.waitFor(() => expect(mocks.invalidation).toBeTypeOf("function"));
    expect(mocks.invalidation).toBeTypeOf("function");
    mocks.invalidation?.({ epoch: "before-restore", entitySequence: 1000 });
    mocks.invalidation?.({ epoch: "after-restore", entitySequence: 900 });

    await vi.waitFor(() => expect(pulls).toEqual([1000, 0]));
    expect(rows.get("state:entities:checkpoint")?.seq).toBe(900);
    stop();
  });

  it("pulls into the schema-keyed projection acquired after a schema change", async () => {
    const workspaceId = "f".repeat(32);
    const firstHash = "a".repeat(64);
    const nextHash = "b".repeat(64);
    const storedByDatabase = new Map<
      string,
      Map<string, Record<string, unknown>>
    >();
    const pulledAfter: number[] = [];
    let pullCount = 0;
    mocks.schemaHash = firstHash;
    mocks.syncGet.mockImplementation(
      async (path: string, options?: { query?: { after?: number } }) => {
        if (path === "/api/sync/identity") return { workspaceId };
        if (path === "/api/sync/pull") {
          const after = options?.query?.after ?? 0;
          pulledAfter.push(after);
          const seq = pullCount++ === 0 ? 10 : 20;
          const id = seq === 10 ? "old-build-agent" : "new-build-agent";
          return {
            workspaceId,
            documents: [
              {
                id: `entity:agent:${id}`,
                payload: JSON.stringify({
                  collection: "agent",
                  id,
                  value: { id, isLead: true, cwd: "/" },
                }),
                seq,
                _deleted: false,
              },
            ],
            checkpoint: { seq },
            maxSeq: seq,
            initialHigh: after === 0 ? 0 : 10,
          };
        }
        throw new Error(`unexpected syncGet path: ${path}`);
      },
    );
    mocks.createRxDatabase.mockImplementation(
      async ({ name }: { name: string }) => {
        if (!name.startsWith("studio-entity-projection-"))
          return { addCollections: vi.fn(async () => ({})) };
        let rows = storedByDatabase.get(name);
        if (!rows) {
          rows = new Map();
          storedByDatabase.set(name, rows);
        }
        const storageInstance = {
          findDocumentsById: vi.fn(async (ids: string[]) =>
            ids.flatMap((id) => (rows!.has(id) ? [rows!.get(id)!] : [])),
          ),
          bulkWrite: vi.fn(
            async (writes: Array<{ document: Record<string, unknown> }>) => {
              for (const { document } of writes)
                rows!.set(String(document.id), document);
              return { error: [] };
            },
          ),
        };
        const subscription = { unsubscribe: vi.fn() };
        const projections = {
          database: { name },
          storageInstance,
          $: { subscribe: vi.fn(() => subscription) },
          find: vi.fn(() => ({
            exec: vi.fn(async () =>
              [...rows!.values()]
                .filter((row) => String(row.id).startsWith("entity:"))
                .map((row) => ({ toJSON: () => row })),
            ),
          })),
          findOne: vi.fn((id: string) => ({
            exec: vi.fn(async () => rows!.get(id) ?? null),
            $: { subscribe: vi.fn(() => subscription) },
          })),
        };
        return {
          addCollections: vi.fn(async () => ({ projections })),
        };
      },
    );
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      matchMedia: vi.fn(() => ({ matches: false })),
    });
    vi.stubGlobal("document", { hidden: false });
    vi.stubGlobal("navigator", { onLine: true });
    vi.stubGlobal("location", { origin: "http://studio.test" });

    const firstClient = await import("./client");
    await firstClient.refreshProjection();
    const firstDatabase = `studio-entity-projection-${workspaceId}-${firstHash}`;
    expect(
      storedByDatabase.get(firstDatabase)?.has("entity:agent:old-build-agent"),
    ).toBe(true);

    mocks.schemaHash = nextHash;
    vi.resetModules();
    const updatedClient = await import("./client");
    const updatedSnapshot = await updatedClient.refreshProjection();
    const nextDatabase = `studio-entity-projection-${workspaceId}-${nextHash}`;

    expect(pulledAfter).toEqual([0, 0]);
    expect(
      storedByDatabase.get(nextDatabase)?.has("entity:agent:new-build-agent"),
    ).toBe(true);
    expect(updatedSnapshot.runtime.agents.map((agent) => agent.id)).toContain(
      "new-build-agent",
    );
  });
});
