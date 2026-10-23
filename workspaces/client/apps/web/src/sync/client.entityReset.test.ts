import { afterEach, describe, expect, it, vi } from "vitest";
import type { TranscriptPageData } from "../types";
import type { ResourceChangeEvent } from "./resourceEvents";
import { SYNC_TRANSCRIPT_STREAM_MAX_BYTES } from "../generated/sharedValues";
import { readRetryDelay } from "./readRetry";

const mocks = vi.hoisted(() => ({
  ApiError: class extends Error {
    constructor(
      message: string,
      public status: number,
    ) {
      super(message);
    }
  },
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
  transcriptInvalidation: undefined as (() => void) | undefined,
  connectionListener: undefined as ((status: string) => void) | undefined,
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
  ApiError: mocks.ApiError,
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
  watchResourceConnection: vi.fn((listener: (status: string) => void) => {
    mocks.connectionListener = listener;
    return () => {
      if (mocks.connectionListener === listener)
        mocks.connectionListener = undefined;
    };
  }),
}));
vi.mock("./resume", () => ({ onResume: vi.fn(() => () => {}) }));
vi.mock("./entityCacheStorage", () => ({
  deleteOtherEntityProjectionDatabases: mocks.deleteOtherCaches,
  holdEntityProjectionDatabaseLock: mocks.holdProjectionDatabaseLock,
  entityProjectionDatabaseName: (workspace: string, hash: string) =>
    `studio-entity-projection-${workspace}-${hash}`,
  workspaceDatabaseName: (workspace: string, suffix = "") =>
    `studio${workspace}${suffix}`,
}));

describe("entity pull reset wiring", () => {
  afterEach(() => {
    vi.useRealTimers();
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
    mocks.transcriptInvalidation = undefined;
    mocks.connectionListener = undefined;
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

  it.each(["delta", "retry", "gap", "oversize", "epoch", "storage"])(
    "applies or rejects a transcript stream %s through the pulled delta applier",
    async (scenario) => {
      const workspaceId = "8".repeat(32);
      const rows = new Map<string, Record<string, unknown>>();
      rows.set("transcript:agent", {
        id: "transcript:agent",
        seq: 1,
        payload: JSON.stringify({
          format: 2,
          epoch: scenario === "epoch" ? "old" : "epoch",
          metadata: { truncated: false },
          order: ["answer"],
          itemRevisions: {},
        }),
      });
      rows.set("transcript:item:agent:answer", {
        id: "transcript:item:agent:answer",
        seq: 1,
        payload: JSON.stringify({ id: "answer", text: "before" }),
      });
      const storageInstance = {
        findDocumentsById: vi.fn(async (ids: string[]) =>
          ids.flatMap((id) => (rows.has(id) ? [rows.get(id)!] : [])),
        ),
        bulkWrite: vi.fn(
          async (writes: { document: Record<string, unknown> }[]) => {
            for (const { document } of writes)
              rows.set(String(document.id), document);
            return { error: [] };
          },
        ),
      };
      const projections = {
        database: { name: "transcript-test" },
        storageInstance,
        $: { subscribe: vi.fn(() => ({ unsubscribe: vi.fn() })) },
      };
      mocks.createRxDatabase
        .mockResolvedValueOnce({ addCollections: vi.fn(async () => ({})) })
        .mockResolvedValueOnce({
          addCollections: vi.fn(async () => ({ projections })),
        });
      mocks.syncGet.mockResolvedValue({ workspaceId });
      vi.stubGlobal("window", { addEventListener: vi.fn() });
      vi.stubGlobal("navigator", {
        onLine: true,
        locks: {
          request: (_key: string, callback: () => Promise<unknown>) =>
            callback(),
        },
      });
      const client = await import("./client");
      const event: ResourceChangeEvent = {
        protocol: 3,
        workspaceId,
        epoch: "epoch",
        revision: 2,
        reason: "change",
        resources: [{ kind: "transcript", agentId: "agent" }],
        resourceVersions: [
          {
            revision: 2,
            transcriptChanges: {
              after: scenario === "gap" ? 0 : 1,
              through: 2,
              delta: {
                delta: true,
                items: [
                  {
                    id: "answer",
                    role: "assistant",
                    text:
                      scenario === "oversize"
                        ? "x".repeat(SYNC_TRANSCRIPT_STREAM_MAX_BYTES)
                        : "streamed",
                  },
                ],
                removed: [],
                itemRevisions: { answer: "0".repeat(64) },
                truncated: false,
              },
            },
          },
        ],
      };
      if (scenario === "storage")
        storageInstance.bulkWrite.mockRejectedValueOnce(
          new Error("storage unavailable"),
        );
      if (["gap", "oversize", "epoch", "storage"].includes(scenario)) {
        await expect(
          client.persistResourceEntityChanges(event, () => true),
        ).rejects.toThrow();
        expect(rows.get("transcript:agent")?.seq).toBe(1);
        return;
      }
      await client.persistResourceEntityChanges(event, () => true);
      const writes = storageInstance.bulkWrite.mock.calls.length;
      if (scenario === "retry") {
        await client.persistResourceEntityChanges(event, () => true);
        expect(storageInstance.bulkWrite).toHaveBeenCalledTimes(writes);
      }
      expect(rows.get("transcript:agent")?.seq).toBe(2);
      expect(
        JSON.parse(String(rows.get("transcript:item:agent:answer")?.payload))
          .text,
      ).toBe("streamed");
      expect(
        mocks.syncGet.mock.calls.filter(([path]) => path === "/api/sync/pull"),
      ).toHaveLength(0);
    },
  );

  it.each([
    { liveBeforeConflict: false, stopDuringBackoff: false },
    { liveBeforeConflict: true, stopDuringBackoff: false },
    { liveBeforeConflict: true, stopDuringBackoff: true },
  ])(
    "recovers transcript conflicts with live=$liveBeforeConflict stop=$stopDuringBackoff",
    async ({ liveBeforeConflict, stopDuringBackoff }) => {
      const workspaceId = "9".repeat(32);
      const rows = new Map<string, Record<string, unknown>>();
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
        findOne: vi.fn(() => ({
          exec: vi.fn(async () => null),
          $: { subscribe: vi.fn(() => subscription) },
        })),
      };
      mocks.createRxDatabase
        .mockResolvedValueOnce({ addCollections: vi.fn(async () => ({})) })
        .mockResolvedValueOnce({
          addCollections: vi.fn(async () => ({ projections })),
        });
      let releaseFirstPull!: () => void;
      let firstPullStarted!: () => void;
      const firstPullGate = new Promise<void>((resolve) => {
        releaseFirstPull = resolve;
      });
      const firstPull = new Promise<void>((resolve) => {
        firstPullStarted = resolve;
      });
      let releaseLivePull!: () => void;
      let livePullStarted!: () => void;
      const livePullGate = new Promise<void>((resolve) => {
        releaseLivePull = resolve;
      });
      const livePull = new Promise<void>((resolve) => {
        livePullStarted = resolve;
      });
      let retryTimerScheduled!: () => void;
      const retryTimer = new Promise<void>((resolve) => {
        retryTimerScheduled = resolve;
      });
      let secondPullStarted!: () => void;
      const secondPull = new Promise<void>((resolve) => {
        secondPullStarted = resolve;
      });
      let thirdPullStarted!: () => void;
      const thirdPull = new Promise<void>((resolve) => {
        thirdPullStarted = resolve;
      });
      let pullCount = 0;
      const assistant = {
        id: "agent:assistant-turn",
        role: "assistant",
        title: "Agent",
        text: "STUDIO_RUST_LUNA_OK",
        at: 10,
      };
      mocks.syncGet.mockImplementation(async (path: string) => {
        if (path === "/api/sync/identity") return { workspaceId };
        if (path !== "/api/sync/pull")
          throw new Error(`unexpected syncGet path: ${path}`);
        pullCount++;
        if (pullCount === 1) {
          firstPullStarted();
          await firstPullGate;
          throw new mocks.ApiError("Conflict", 409);
        }
        if (pullCount === 2 && !stopDuringBackoff) {
          livePullStarted();
          await livePullGate;
          throw new mocks.ApiError("Conflict", 409);
        }
        if (pullCount === 2) secondPullStarted();
        else thirdPullStarted();
        return {
          workspaceId,
          documents: [
            {
              id: "transcript:agent",
              payload: JSON.stringify({
                items: [assistant],
                truncated: false,
                nextCursor: null,
                nextAfterCursor: null,
                historyVersion: "thread:checkpoint",
                unavailable: null,
                agent: { id: "agent", status: "completed" },
              }),
              seq: 1,
              _deleted: false,
            },
          ],
          checkpoint: { seq: 1 },
          maxSeq: 1,
        };
      });
      mocks.watchResourceChanges.mockImplementation(
        (resource: unknown, listener: typeof mocks.invalidation) => {
          if ((resource as { kind?: string }).kind === "transcript")
            mocks.transcriptInvalidation = listener;
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
      const accepted: Array<TranscriptPageData | null> = [];
      const failures: unknown[] = [];
      let resolveConflictReported!: () => void;
      const conflictReported = new Promise<void>((resolve) => {
        resolveConflictReported = resolve;
      });
      const stop = client.subscribeTranscriptProjection(
        "transcript:agent",
        (payload) => {
          accepted.push(payload);
        },
        (error) => {
          failures.push(error);
          if (error instanceof mocks.ApiError && error.status === 409)
            resolveConflictReported();
        },
      );
      await vi.waitFor(() => {
        expect(mocks.transcriptInvalidation).toBeTypeOf("function");
        expect(mocks.connectionListener).toBeTypeOf("function");
        expect(projections.findOne).toHaveBeenCalledWith("transcript:agent");
      });
      mocks.transcriptInvalidation?.();
      await vi.waitFor(() => expect(pullCount).toBe(1), {
        interval: 1,
        timeout: 100,
      });
      await firstPull;
      if (liveBeforeConflict) mocks.connectionListener?.("live");
      vi.useFakeTimers();
      if (stopDuringBackoff) {
        const originalSetTimeout = globalThis.setTimeout;
        vi.spyOn(globalThis, "setTimeout").mockImplementation(
          (handler, timeout, ...args) => {
            if (timeout === readRetryDelay(0)) retryTimerScheduled();
            return originalSetTimeout(handler, timeout, ...args);
          },
        );
      }
      releaseFirstPull();
      if (stopDuringBackoff) {
        await retryTimer;
        stop();
        await vi.advanceTimersByTimeAsync(readRetryDelay(0));
        expect(pullCount).toBe(1);

        const stopReopened = client.subscribeTranscriptProjection(
          "transcript:agent",
          (payload) => accepted.push(payload),
          (error) => failures.push(error),
        );
        await vi.waitFor(() =>
          expect(projections.findOne).toHaveBeenCalledTimes(2),
        );
        mocks.transcriptInvalidation?.();
        await secondPull;
        await vi.waitFor(() =>
          expect(
            accepted.some((payload) =>
              payload?.items.some(
                (item) => item.text === "STUDIO_RUST_LUNA_OK",
              ),
            ),
          ).toBe(true),
        );
        expect(pullCount).toBe(2);
        stopReopened();
        return;
      }
      if (liveBeforeConflict) {
        await vi.advanceTimersByTimeAsync(readRetryDelay(0));
      } else {
        await conflictReported;
        mocks.connectionListener?.("live");
      }
      await livePull;
      releaseLivePull();
      await vi.advanceTimersByTimeAsync(
        readRetryDelay(liveBeforeConflict ? 1 : 0),
      );
      await thirdPull;
      await vi.waitFor(
        () =>
          expect(
            accepted.some((payload) =>
              payload?.items.some(
                (item) => item.text === "STUDIO_RUST_LUNA_OK",
              ),
            ),
          ).toBe(true),
        { interval: 1, timeout: 100 },
      );

      expect(pullCount).toBe(3);
      expect(
        accepted.some((payload) =>
          payload?.items.some((item) => item.text === "STUDIO_RUST_LUNA_OK"),
        ),
      ).toBe(true);
      expect(
        failures.some(
          (error) => error instanceof mocks.ApiError && error.status === 409,
        ),
      ).toBe(!liveBeforeConflict);
      expect(failures.at(-1)).toBeNull();
      stop();
    },
  );

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
    let projectedWorkspaceId = "";
    const updatedSnapshot = await updatedClient.refreshProjection({
      onWorkspaceId: (id) => {
        projectedWorkspaceId = id;
      },
    });
    const nextDatabase = `studio-entity-projection-${workspaceId}-${nextHash}`;

    expect(pulledAfter).toEqual([0, 0]);
    expect(
      storedByDatabase.get(nextDatabase)?.has("entity:agent:new-build-agent"),
    ).toBe(true);
    expect(updatedSnapshot.runtime.agents.map((agent) => agent.id)).toContain(
      "new-build-agent",
    );
    expect(projectedWorkspaceId).toBe(workspaceId);
  });
});
