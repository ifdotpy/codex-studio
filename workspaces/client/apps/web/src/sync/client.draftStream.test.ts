import assert from "node:assert/strict";
import { afterEach, expect, it, vi } from "vitest";
import type { RxDatabase } from "rxdb";
import { firstValueFrom, filter } from "rxjs";
import { DRAFT_SYNC_TIMING_MS } from "./draftSyncTiming.ts";
import type {
  ResourceChangeEvent,
  ResourceVersion,
} from "./subscriptions/events";

const mocks = vi.hoisted(() => ({
  databases: [] as RxDatabase[],
  syncGet: vi.fn(),
  syncPost: vi.fn(
    async (
      _path: string,
      _body: import("../generated/api").components["schemas"]["DraftPushRequest"],
      _options: unknown,
    ): Promise<import("./client").SyncDocument[]> => [],
  ),
  pushWait: undefined as (() => void) | undefined,
  replication: undefined as { awaitInSync: () => Promise<unknown> } | undefined,
  invalidate: undefined as ((version?: ResourceVersion) => void) | undefined,
}));
vi.mock("rxdb", async (original) => {
  const rxdb = await original<typeof import("rxdb")>();
  return {
    ...rxdb,
    createRxDatabase: async (
      options: Parameters<typeof rxdb.createRxDatabase>[0],
    ) => {
      const db = await rxdb.createRxDatabase({
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
vi.mock("rxdb/plugins/replication", async (original) => {
  const actual = await original<typeof import("rxdb/plugins/replication")>();
  return {
    ...actual,
    replicateRxCollection: (
      ...args: Parameters<typeof actual.replicateRxCollection>
    ) => {
      const options = args[0];
      const waitBeforePersist = options.push?.waitBeforePersist;
      if (options.push && waitBeforePersist)
        options.push.waitBeforePersist = () => {
          const pending = waitBeforePersist();
          mocks.pushWait?.();
          return pending;
        };
      const replication = actual.replicateRxCollection(...args);
      mocks.replication = replication;
      return replication;
    },
  };
});

vi.mock("../api", () => ({
  syncGet: mocks.syncGet,
  syncPost: mocks.syncPost,
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
    _resource: unknown,
    listener: (version?: ResourceVersion) => void,
  ) => {
    mocks.invalidate = listener;
    return () => {};
  },
}));
vi.mock("./resume", () => ({ onResume: () => () => {} }));

let stop: (() => Promise<void>) | undefined;
let releasePendingPush: (() => void) | undefined;
afterEach(async () => {
  releasePendingPush?.();
  releasePendingPush = undefined;
  const closing = stop?.();
  if (vi.isFakeTimers()) await vi.runOnlyPendingTimersAsync();
  await closing;
  vi.useRealTimers();
  stop = undefined;
  await Promise.all(mocks.databases.map((db) => db.close()));
  mocks.databases = [];
  mocks.replication = undefined;
  mocks.pushWait = undefined;
  mocks.syncGet.mockReset();
  mocks.syncPost.mockClear();
  vi.unstubAllGlobals();
  vi.resetModules();
});

async function setup() {
  const workspaceId = "d".repeat(32);
  vi.stubGlobal("window", {
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
  });
  vi.stubGlobal("navigator", { onLine: true });
  mocks.syncGet.mockImplementation(async (path: string) =>
    path === "/api/sync/identity"
      ? { workspaceId }
      : { workspaceId, documents: [], checkpoint: { seq: 0 } },
  );
  const client = await import("./client");
  stop = await client.startDraftReplication(() => {});
  const { db } = await client.syncDatabase();
  mocks.invalidate?.({ epoch: "one", revision: 0, reason: "initial" });
  await mocks.replication!.awaitInSync();
  expect(
    mocks.syncGet.mock.calls.filter(([path]) => path === "/api/sync/pull"),
  ).toHaveLength(1);
  const event = (
    after: number,
    through: number,
    deleted = false,
  ): ResourceChangeEvent => ({
    protocol: 4,
    workspaceId,
    epoch: "one",
    revision: through,
    reason: "change",
    resources: [{ kind: "drafts" }],
    resourceVersions: [
      {
        revision: through,
        draftChanges: {
          after,
          through,
          documents: [
            {
              id: "phone:chat",
              seq: through,
              _deleted: deleted,
              payload:
                '{"device":"phone","session":"chat","text":"remote","updated":1.5}',
            },
          ],
        },
      },
    ],
  });
  const deliver = async (frame: ResourceChangeEvent, wait = true) => {
    assert.ok(frame.resourceVersions[0] != null);
    mocks.invalidate?.({
      epoch: frame.epoch,
      revision: frame.resourceVersions[0].revision,
      reason: frame.reason,
      draftChanges: frame.resourceVersions[0].draftChanges ?? undefined,
    });
    if (wait) await mocks.replication!.awaitInSync();
  };
  const pulls = () =>
    mocks.syncGet.mock.calls.filter(([path]) => path === "/api/sync/pull");
  return { client, db, event, deliver, pulls, workspaceId };
}

it("applies real RxDB draft stream edits and tombstones without pulls, then recovers gaps and reconnects", async () => {
  const { db, event, deliver, pulls } = await setup();
  await deliver(event(0, 1));
  expect((await db.drafts.findOne("phone:chat").exec())?.payload).toContain(
    '"updated":1.5',
  );
  const deleted = firstValueFrom(
    db.drafts.findOne("phone:chat").$.pipe(filter((doc) => doc === null)),
  );
  await deliver(event(1, 2, true));
  await deleted;
  expect(!!(await db.drafts.findOne("phone:chat").exec())).toBe(false);
  expect(pulls()).toHaveLength(1);
  await deliver(event(3, 4));
  expect(pulls()).toHaveLength(2);
  await deliver({ ...event(0, 2), reason: "reconnect" });
  expect(pulls()).toHaveLength(3);
  await deliver({ ...event(0, 2), epoch: "two" });
  expect(pulls()).toHaveLength(4);
});

it("recovers an accepted draft from the durable RxDB checkpoint after persistence fails", async () => {
  const { db, event, deliver, pulls, workspaceId } = await setup();
  await deliver(event(0, 1));
  expect(!!(await db.drafts.findOne("phone:chat").exec())).toBe(true);
  const changed = event(1, 2);
  assert.ok(changed.resourceVersions[0] != null);
  const documents = changed.resourceVersions[0].draftChanges!.documents;
  assert.ok(documents[0] != null);
  documents[0].payload = documents[0].payload.replace("remote", "recovered");
  mocks.syncGet.mockImplementation(
    async (path: string, options?: { query: { after: number } }) =>
      path === "/api/sync/identity"
        ? { workspaceId }
        : {
            workspaceId,
            documents: options?.query.after === 1 ? documents : [],
            checkpoint: { seq: 2 },
          },
  );
  const write = vi
    .spyOn(db.drafts.storageInstance, "bulkWrite")
    .mockRejectedValueOnce(new Error("injected stream persistence failure"));
  const recovered = firstValueFrom(
    db.drafts
      .findOne("phone:chat")
      .$.pipe(filter((doc) => !!doc?.payload.includes("recovered"))),
  );
  await deliver(changed);
  await recovered;
  expect(pulls()).toHaveLength(2);
  expect((await db.drafts.findOne("phone:chat").exec())?.payload).toContain(
    "recovered",
  );
  expect(pulls()[1]?.[1]).toMatchObject({
    query: { scope: "drafts", after: 1 },
  });
  write.mockRestore();
});

it("admits streamed changes during a pending push and keeps both conflicting texts without pulling", async () => {
  const { db, event, deliver, pulls } = await setup();
  await deliver(event(0, 1));
  expect(!!(await db.drafts.findOne("phone:chat").exec())).toBe(true);
  const remote = event(1, 2);
  assert.ok(remote.resourceVersions[0] != null);
  const documents = remote.resourceVersions[0].draftChanges!.documents;
  assert.ok(documents[0] != null);
  documents[0].payload = JSON.stringify({
    device: "phone",
    session: "chat",
    text: "remote-new",
    updated: 3,
  });
  let finish!: (rows: typeof documents) => void;
  const pendingPush = new Promise<typeof documents>((resolve) => {
    finish = resolve;
  });
  releasePendingPush = () => finish([]);
  const pushStarted = Promise.withResolvers<void>();
  const mergePushed = Promise.withResolvers<void>();
  mocks.syncPost.mockImplementationOnce(() => {
    pushStarted.resolve();
    return pendingPush;
  });
  mocks.syncPost.mockImplementationOnce(async () => {
    mergePushed.resolve();
    return [];
  });
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
  const firstDebounce = Promise.withResolvers<void>();
  mocks.pushWait = firstDebounce.resolve;
  const local = await db.drafts.findOne("phone:chat").exec();
  await local!.incrementalPatch({
    payload: JSON.stringify({
      device: "phone",
      session: "chat",
      text: "local-unsent",
      updated: 2,
    }),
  });

  await firstDebounce.promise;
  await vi.advanceTimersByTimeAsync(DRAFT_SYNC_TIMING_MS.pushMaxWait);

  await pushStarted.promise;
  expect(mocks.syncPost).toHaveBeenCalledOnce();
  await deliver(remote, false);
  expect(pulls()).toHaveLength(1);
  const nextDebounce = Promise.withResolvers<void>();
  mocks.pushWait = nextDebounce.resolve;
  finish(documents);

  await nextDebounce.promise;
  await vi.advanceTimersByTimeAsync(DRAFT_SYNC_TIMING_MS.pushMaxWait);

  await mergePushed.promise;
  vi.useRealTimers();

  await mocks.replication!.awaitInSync();
  expect(mocks.syncPost).toHaveBeenCalledTimes(2);
  assert.ok(mocks.syncPost.mock.calls[1] != null);
  const pushed = mocks.syncPost.mock.calls[1][1];
  assert.ok(pushed.rows[0] != null);
  const merged = pushed.rows[0].newDocumentState;
  expect(merged.payload).toContain("remote-new");
  expect(merged.payload).toContain("local-unsent");
  // A successful merged push publishes its own newer draft sequence.
  const confirmed = event(2, 3);
  const confirmedDocument =
    confirmed.resourceVersions[0]?.draftChanges?.documents[0];
  assert.ok(confirmedDocument);
  confirmedDocument.payload = merged.payload;
  const confirmedWrite = firstValueFrom(
    db.drafts
      .findOne("phone:chat")
      .$.pipe(
        filter(
          (doc) =>
            !!doc?.payload.includes("remote-new") &&
            doc.payload.includes("local-unsent"),
        ),
      ),
  );
  await deliver(confirmed);
  const payload = (await confirmedWrite)?.payload;
  expect(payload).toContain("remote-new");
  expect(payload).toContain("local-unsent");
  expect(pulls()).toHaveLength(1);
});
