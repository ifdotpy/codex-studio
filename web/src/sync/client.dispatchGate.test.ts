import { afterEach, describe, expect, it, vi } from "vitest";
import {
  API_SCHEMA_HASH,
  API_SCHEMA_HASH_HEADER,
} from "../generated/apiSchema";

const mocks = vi.hoisted(() => ({
  createRxDatabase: vi.fn(),
  watchResourceChanges: vi.fn(),
  pulls: undefined as number[] | undefined,
  pullResponse: undefined as
    | ((
        after: number,
      ) => Record<string, unknown> | Promise<Record<string, unknown>>)
    | undefined,
  mutationResponse: null as Record<string, unknown> | null,
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
vi.mock("./resourceEvents", () => ({
  acknowledgeEntitySequences: vi.fn(),
  watchResourceChanges: mocks.watchResourceChanges,
  watchResourceConnection: vi.fn(() => () => {}),
}));
vi.mock("./resume", () => ({ onResume: vi.fn(() => () => {}) }));

async function setup() {
  const workspaceId = "e".repeat(32);
  mocks.pulls = undefined;
  mocks.pullResponse = undefined;
  mocks.mutationResponse = null;
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
  mocks.createRxDatabase
    .mockResolvedValueOnce({ addCollections: vi.fn(async () => ({})) })
    .mockResolvedValueOnce({
      addCollections: vi.fn(async () => ({ projections })),
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
  vi.stubGlobal(
    "fetch",
    vi.fn(async (request: Request) => {
      const url = new URL(request.url);
      if (url.pathname === "/api/sync/identity")
        return Response.json(
          { workspaceId },
          { headers: { [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH } },
        );
      if (url.pathname === "/api/sync/pull") {
        const after = Number(url.searchParams.get("after") || 0);
        mocks.pulls?.push(after);
        const body = await mocks.pullResponse?.(after);
        return Response.json(
          body ?? {
            workspaceId,
            documents: [],
            checkpoint: { seq: 100 },
            maxSeq: 100,
            initialHigh: 100,
          },
          { headers: { [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH } },
        );
      }
      if (request.method === "POST" && url.pathname === "/api/sync/drafts")
        return Response.json(mocks.mutationResponse ?? {}, {
          headers: { [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH },
        });
      throw new Error(
        `unexpected API request ${request.method} ${url.pathname}`,
      );
    }),
  );
  const client = await import("./client");
  const api = await import("../api");
  const entitySequence = await import("./entitySequence");
  const failures: unknown[] = [];
  const stop = client.subscribeStateProjection(vi.fn(), (error) =>
    failures.push(error),
  );
  await vi
    .waitFor(() => expect(mocks.invalidate).toBeTypeOf("function"))
    .catch(() => {
      throw new Error(
        `watch not installed: ${failures.map(String).join("; ")}`,
      );
    });
  return {
    api,
    client,
    entitySequence,
    failures,
    rows,
    storageInstance,
    stop,
    workspaceId,
  };
}

describe("entity pull dispatch gate", () => {
  afterEach(() => {
    vi.resetModules();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
    mocks.createRxDatabase.mockReset();
    mocks.watchResourceChanges.mockReset();
    mocks.pulls = undefined;
    mocks.pullResponse = undefined;
    mocks.mutationResponse = null;
    mocks.invalidate = undefined;
  });

  const installMutationResponse = () => {
    const documents = [
      {
        id: "entity:agent:created",
        payload: JSON.stringify({ id: "created", value: { name: "new" } }),
        seq: 101,
        _deleted: false,
      },
    ];
    mocks.mutationResponse = {
      _syncEntities: documents,
      _syncEntitiesAfter: 100,
    };
    return documents;
  };

  const configurePulls = (
    workspaceId: string,
    pulls: number[],
    documents: Array<Record<string, unknown>> = [],
  ) => {
    mocks.pulls = pulls;
    mocks.pullResponse = (after) => ({
      workspaceId,
      documents,
      checkpoint: { seq: 101 },
      maxSeq: 101,
      initialHigh: 100,
      after,
    });
  };

  it("suppresses a same-epoch frame below the durable checkpoint", async () => {
    const { entitySequence, rows, stop } = await setup();
    const checkpoint = entitySequence.getEntitySequenceCheckpoint(
      "state:entities:v1",
      API_SCHEMA_HASH,
    );
    checkpoint.observeEpoch("server-epoch-a");
    checkpoint.assign(2123);
    rows.set("state:entities:checkpoint", {
      id: "state:entities:checkpoint",
      payload: "{}",
      seq: 2123,
      _deleted: false,
    });
    const pulls: number[] = [];
    mocks.pulls = pulls;
    mocks.invalidate?.({ epoch: "server-epoch-a", entitySequence: 2001 });

    await Promise.resolve();
    expect(checkpoint.value).toBe(2123);
    expect(rows.get("state:entities:checkpoint")?.seq).toBe(2123);
    expect(pulls).toEqual([]);
    stop();
  });

  it("pulls from a reset baseline when the server epoch changes", async () => {
    const { entitySequence, rows, stop, workspaceId } = await setup();
    const checkpoint = entitySequence.getEntitySequenceCheckpoint(
      "state:entities:v1",
      API_SCHEMA_HASH,
    );
    checkpoint.observeEpoch("server-epoch-a");
    checkpoint.assign(2123);
    rows.set("state:entities:checkpoint", {
      id: "state:entities:checkpoint",
      payload: "{}",
      seq: 2123,
      _deleted: false,
    });
    const pulls: number[] = [];
    mocks.pulls = pulls;
    mocks.pullResponse = (after) =>
      after > 5
        ? { workspaceId, reset: true, floor: 0, maxSeq: 5 }
        : {
            workspaceId,
            documents: [],
            checkpoint: { seq: 5 },
            maxSeq: 5,
            initialHigh: 5,
          };
    mocks.invalidate?.({ epoch: "server-epoch-b", entitySequence: 5 });

    await vi.waitFor(() => expect(pulls).toHaveLength(2));
    expect(pulls).toEqual([2123, 0]);
    expect(checkpoint.value).toBe(5);
    expect(rows.get("state:entities:checkpoint")?.seq).toBe(5);
    stop();
  });

  it("runs a real post through the held persister and gates the real pull loop", async () => {
    const { api, entitySequence, rows, storageInstance, stop, workspaceId } =
      await setup();
    const documents = installMutationResponse();
    const pulls: number[] = [];
    configurePulls(workspaceId, pulls, documents);
    const checkpoint = entitySequence.getEntitySequenceCheckpoint(
      "state:entities:v1",
      API_SCHEMA_HASH,
    );
    let release!: () => void;
    const hold = new Promise<void>((resolve) => {
      release = resolve;
    });
    let entered!: () => void;
    const persisterEntered = new Promise<void>((resolve) => {
      entered = resolve;
    });
    storageInstance.bulkWrite.mockImplementation(async (writes) => {
      if (writes.some(({ document }) => document.id === documents[0]?.id)) {
        entered();
        await hold;
      }
      for (const { document } of writes)
        rows.set(String(document.id), document);
      return { error: [] };
    });

    const post = api.post("/api/sync/drafts", { rows: [] });
    await persisterEntered;
    const readsBeforeFrame =
      storageInstance.findDocumentsById.mock.calls.length;
    mocks.invalidate?.({ epoch: "held", entitySequence: 101 });
    await vi.waitFor(() =>
      expect(
        storageInstance.findDocumentsById.mock.calls.length,
      ).toBeGreaterThan(readsBeforeFrame),
    );
    expect(pulls).toEqual([]);
    release();
    await post;
    expect(pulls).toEqual([]);
    expect(rows.get(documents[0]!.id)).toMatchObject({ seq: 101 });
    expect(checkpoint.value).toBe(101);
    stop();
  });

  it.each(["reject", "250ms timeout"] as const)(
    "dispatches exactly one fallback pull when a real held persister %s",
    async (mode) => {
      const { api, rows, storageInstance, stop, workspaceId } = await setup();
      const documents = installMutationResponse();
      const pulls: number[] = [];
      configurePulls(workspaceId, pulls, documents);
      let release!: () => void;
      let rejectWrite!: (error: Error) => void;
      const hold = new Promise<void>((resolve, reject) => {
        release = resolve;
        rejectWrite = reject;
      });
      let entered!: () => void;
      const persisterEntered = new Promise<void>((resolve) => {
        entered = resolve;
      });
      storageInstance.bulkWrite.mockImplementation(async (writes) => {
        if (writes.some(({ document }) => document.id === documents[0]?.id)) {
          entered();
          await hold;
        }
        for (const { document } of writes)
          rows.set(String(document.id), document);
        return { error: [] };
      });
      const post = api.post("/api/sync/drafts", { rows: [] });
      await persisterEntered;
      const readsBeforeFrame =
        storageInstance.findDocumentsById.mock.calls.length;
      mocks.invalidate?.({ epoch: "held", entitySequence: 101 });
      await vi.waitFor(() =>
        expect(
          storageInstance.findDocumentsById.mock.calls.length,
        ).toBeGreaterThan(readsBeforeFrame),
      );
      expect(pulls).toEqual([]);
      if (mode === "reject") rejectWrite(new Error("test persistence failure"));
      else await new Promise((resolve) => setTimeout(resolve, 300));
      await post;
      await vi.waitFor(() => expect(pulls).toHaveLength(1));
      expect(pulls).toHaveLength(1);
      release();
      await Promise.resolve();
      stop();
    },
  );

  it("pulls once when a reset arrives during the real held persister", async () => {
    const { api, entitySequence, rows, storageInstance, stop, workspaceId } =
      await setup();
    const documents = installMutationResponse();
    const pulls: number[] = [];
    configurePulls(workspaceId, pulls, documents);
    let release!: () => void;
    const hold = new Promise<void>((resolve) => {
      release = resolve;
    });
    let entered!: () => void;
    const persisterEntered = new Promise<void>((resolve) => {
      entered = resolve;
    });
    storageInstance.bulkWrite.mockImplementation(async (writes) => {
      if (writes.some(({ document }) => document.id === documents[0]?.id)) {
        entered();
        await hold;
      }
      for (const { document } of writes)
        rows.set(String(document.id), document);
      return { error: [] };
    });
    const post = api.post("/api/sync/drafts", { rows: [] });
    await persisterEntered;
    mocks.invalidate?.({ epoch: "reset", entitySequenceReset: true });
    await vi.waitFor(() => expect(pulls).toHaveLength(1));
    release();
    await post;
    expect(pulls).toHaveLength(1);
    expect(
      entitySequence
        .getEntitySequenceCheckpoint("state:entities:v1", API_SCHEMA_HASH)
        .effectiveCoverage(),
    ).toBe(101);
    stop();
  });

  it("does not suppress a frame while the checkpoint is not durably valid", async () => {
    const { entitySequence, stop, workspaceId } = await setup();
    const checkpoint = entitySequence.getEntitySequenceCheckpoint(
      "state:entities:v1",
      API_SCHEMA_HASH,
    );
    checkpoint.reset();
    checkpoint.assignWithinEpoch(100);
    expect(checkpoint.canUseDurableCheckpoint).toBe(false);
    const pulls: number[] = [];
    configurePulls(workspaceId, pulls);
    mocks.invalidate?.({ epoch: "validity", entitySequence: 100 });
    await vi.waitFor(() => expect(pulls).toHaveLength(1));
    expect(pulls).toHaveLength(1);
    stop();
  });

  it("keeps an unversioned invalidation that arrives during a covered pull", async () => {
    let finishFirst!: (value: unknown) => void;
    const firstPull = new Promise((resolve) => {
      finishFirst = resolve;
    });
    const pulls: number[] = [];
    const { failures, stop } = await setup();
    mocks.pulls = pulls;
    mocks.pullResponse = async (after) => {
      if (pulls.length === 1)
        return firstPull as Promise<Record<string, unknown>>;
      return {
        workspaceId: "e".repeat(32),
        documents: [],
        checkpoint: { seq: 100 },
        maxSeq: 100,
        initialHigh: 100,
        after,
      };
    };
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
    mocks.pulls = pulls;
    mocks.pullResponse = async (after) => {
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
        after,
      };
    };
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
    mocks.pulls = pulls;
    mocks.pullResponse = async (after) => {
      if (pulls.length === 1) throw new TypeError("simulated read failure");
      return {
        workspaceId: "e".repeat(32),
        documents: [],
        checkpoint: { seq: 101 },
        maxSeq: 101,
        initialHigh: 100,
        after,
      };
    };
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
    const beforeDirectRefresh = pulls.length;
    await client.refreshProjection();
    expect(pulls).toHaveLength(beforeDirectRefresh + 1);
    stop();
  });
});
