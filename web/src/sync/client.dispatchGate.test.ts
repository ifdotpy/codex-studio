import { afterEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  createRxDatabase: vi.fn(),
  syncGet: vi.fn(),
  watchResourceChanges: vi.fn(),
  invalidate: undefined as
    | ((version?: {
        epoch: string;
        entitySequence?: number;
        entitySequenceReset?: boolean;
      }) => void)
    | undefined,
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
  registerSyncEntityPersister: vi.fn(),
  save: vi.fn(),
  saved: (_key: string, fallback: unknown) => fallback,
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

async function setup() {
  const workspaceId = "e".repeat(32);
  const rows = new Map<string, Record<string, unknown>>([
    [
      "state:entities:checkpoint",
      {
        id: "state:entities:checkpoint",
        payload: "{}",
        seq: 100,
        _deleted: false,
      },
    ],
    [
      "state:entities:ready",
      { id: "state:entities:ready", payload: "ready", seq: 1, _deleted: false },
    ],
    [
      "state:entities:initial",
      {
        id: "state:entities:initial",
        payload: "{}",
        seq: 100,
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
    storageInstance,
    $: { subscribe: vi.fn(() => subscription) },
    find: vi.fn(() => ({
      exec: vi.fn(async () => []),
      $: { subscribe: vi.fn(() => subscription) },
    })),
    findOne: vi.fn((id: string) => ({
      exec: vi.fn(async () => (rows.has(id) ? rows.get(id) : null)),
      $: { subscribe: vi.fn(() => subscription) },
    })),
  };
  mocks.createRxDatabase.mockResolvedValue({
    addCollections: vi.fn(async () => {}),
    projections,
  });
  mocks.syncGet.mockImplementation(async (path: string) => {
    if (path === "/api/sync/identity") return { workspaceId };
    throw new Error("unexpected sync pull");
  });
  mocks.watchResourceChanges.mockImplementation(
    (_resource: unknown, listener: typeof mocks.invalidate) => {
      mocks.invalidate = listener;
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
  const failures: unknown[] = [];
  const stop = client.subscribeStateProjection(vi.fn(), (error) =>
    failures.push(error),
  );
  await vi.waitFor(() => expect(mocks.invalidate).toBeTypeOf("function"));
  return { client, failures, rows, storageInstance, stop, workspaceId };
}

describe("entity pull dispatch gate", () => {
  afterEach(() => {
    vi.resetModules();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    mocks.createRxDatabase.mockReset();
    mocks.syncGet.mockReset();
    mocks.watchResourceChanges.mockReset();
    mocks.invalidate = undefined;
  });

  it("keeps an unversioned invalidation that arrives during a covered pull", async () => {
    let finishFirst!: (value: unknown) => void;
    const firstPull = new Promise((resolve) => {
      finishFirst = resolve;
    });
    const pulls: number[] = [];
    const { failures, stop } = await setup();
    mocks.syncGet.mockImplementation(
      async (path: string, options?: { query?: { after?: number } }) => {
        if (path === "/api/sync/identity")
          return { workspaceId: "e".repeat(32) };
        pulls.push(options?.query?.after ?? -1);
        if (pulls.length === 1) return firstPull;
        return {
          workspaceId: "e".repeat(32),
          documents: [],
          checkpoint: { seq: 100 },
          maxSeq: 100,
          initialHigh: 100,
        };
      },
    );
    mocks.invalidate?.({ epoch: "epoch", entitySequence: 101 });
    await vi.waitFor(() => expect(pulls).toHaveLength(1));
    mocks.invalidate?.();
    finishFirst({
      workspaceId: "e".repeat(32),
      documents: [],
      checkpoint: { seq: 101 },
      maxSeq: 101,
      initialHigh: 100,
    });
    await vi
      .waitFor(() => expect(pulls).toHaveLength(2), { timeout: 3000 })
      .catch(() => {
        throw new Error(
          `pagination pulls=${JSON.stringify(pulls)} failures=${String(failures.at(-1))}`,
        );
      });
    expect(pulls).toEqual([100, 101]);
    stop();
  });

  it("does not gate pagination after a full page", async () => {
    const pulls: number[] = [];
    const { failures, stop } = await setup();
    mocks.syncGet.mockImplementation(
      async (path: string, options?: { query?: { after?: number } }) => {
        if (path === "/api/sync/identity")
          return { workspaceId: "e".repeat(32) };
        const after = options?.query?.after ?? -1;
        pulls.push(after);
        const documents =
          after === 100
            ? Array.from({ length: 500 }, (_, index) => ({
                id: `entity:agent:${index}`,
                payload: "{}",
                seq: 101 + index,
              }))
            : [{ id: "entity:agent:last", payload: "{}", seq: 601 }];
        const checkpoint = after === 100 ? 600 : 601;
        return {
          workspaceId: "e".repeat(32),
          documents,
          checkpoint: { seq: checkpoint },
          maxSeq: 601,
          initialHigh: 100,
        };
      },
    );
    mocks.invalidate?.({ epoch: "epoch", entitySequence: 500 });
    await vi
      .waitFor(() => expect(pulls).toHaveLength(2), { timeout: 3000 })
      .catch(() => {
        throw new Error(
          `pagination pulls=${JSON.stringify(pulls)} failures=${String(failures.at(-1))}`,
        );
      });
    expect(pulls).toEqual([100, 600]);
    stop();
  });

  it("makes a direct refresh pull after a failed refresh even when a retained sequence is covered", async () => {
    const pulls: number[] = [];
    const { client, storageInstance, stop } = await setup();
    mocks.syncGet.mockImplementation(
      async (path: string, options?: { query?: { after?: number } }) => {
        if (path === "/api/sync/identity")
          return { workspaceId: "e".repeat(32) };
        pulls.push(options?.query?.after ?? -1);
        return {
          workspaceId: "e".repeat(32),
          documents: [],
          checkpoint: { seq: 101 },
          maxSeq: 101,
          initialHigh: 100,
        };
      },
    );
    storageInstance.bulkWrite.mockImplementationOnce(async (writes) => {
      if (
        writes.some(
          ({ document }) => document.id === "state:entities:checkpoint",
        )
      )
        throw new TypeError("simulated persistence failure");
      return { error: [] };
    });
    mocks.invalidate?.({ epoch: "epoch", entitySequence: 101 });
    await vi.waitFor(() => expect(pulls).toHaveLength(1));
    await vi.waitFor(() =>
      expect(storageInstance.bulkWrite).toHaveBeenCalled(),
    );
    await client.refreshProjection();
    expect(pulls).toHaveLength(2);
    stop();
  });
});
