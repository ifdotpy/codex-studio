import { afterEach, expect, it, vi } from "vitest";
import type { RxDatabase } from "rxdb";
import type { ResourceChangeEvent, ResourceVersion } from "./resourceEvents";

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
vi.mock("../api", () => ({
  syncGet: mocks.syncGet,
  syncPost: mocks.syncPost,
  saved: (_key: string, fallback: unknown) => fallback,
  save: vi.fn(),
  setWorkspace: vi.fn(),
  isApiSchemaMismatch: () => false,
  onMatchingApiSchemaResponse: () => () => {},
}));
vi.mock("./resourceEvents", () => ({
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
afterEach(async () => {
  await stop?.();
  stop = undefined;
  await Promise.all(mocks.databases.map((db) => db.close()));
  mocks.databases = [];
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
  await vi.waitFor(() =>
    expect(
      mocks.syncGet.mock.calls.filter(([path]) => path === "/api/sync/pull"),
    ).toHaveLength(1),
  );
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
  const deliver = async (frame: ResourceChangeEvent) => {
    mocks.invalidate?.({
      epoch: frame.epoch,
      revision: frame.resourceVersions[0].revision,
      reason: frame.reason,
      draftChanges: frame.resourceVersions[0].draftChanges ?? undefined,
    });
  };
  const pulls = () =>
    mocks.syncGet.mock.calls.filter(([path]) => path === "/api/sync/pull");
  return { client, db, event, deliver, pulls, workspaceId };
}

it("applies real RxDB draft stream edits and tombstones without pulls, then recovers gaps and reconnects", async () => {
  const { db, event, deliver, pulls } = await setup();
  await deliver(event(0, 1));
  await vi.waitFor(async () =>
    expect((await db.drafts.findOne("phone:chat").exec())?.payload).toContain(
      '"updated":1.5',
    ),
  );
  await deliver(event(1, 2, true));
  await vi.waitFor(async () =>
    expect(!!(await db.drafts.findOne("phone:chat").exec())).toBe(false),
  );
  expect(pulls()).toHaveLength(1);
  await deliver(event(3, 4));
  await vi.waitFor(() => expect(pulls()).toHaveLength(2));
  await deliver({ ...event(0, 2), reason: "reconnect" });
  await vi.waitFor(() => expect(pulls()).toHaveLength(3));
  await deliver({ ...event(0, 2), epoch: "two" });
  await vi.waitFor(() => expect(pulls()).toHaveLength(4));
});

it("recovers an accepted draft from the durable RxDB checkpoint after persistence fails", async () => {
  const { db, event, deliver, pulls, workspaceId } = await setup();
  await deliver(event(0, 1));
  await vi.waitFor(async () =>
    expect(!!(await db.drafts.findOne("phone:chat").exec())).toBe(true),
  );
  const changed = event(1, 2);
  const documents = changed.resourceVersions[0].draftChanges!.documents;
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
  await deliver(changed);
  await vi.waitFor(() => expect(pulls()).toHaveLength(2));
  await vi.waitFor(async () =>
    expect((await db.drafts.findOne("phone:chat").exec())?.payload).toContain(
      "recovered",
    ),
  );
  expect(pulls()[1][1]).toMatchObject({ query: { scope: "drafts", after: 1 } });
  write.mockRestore();
});

it("admits streamed changes during a pending push and keeps both conflicting texts without pulling", async () => {
  const { db, event, deliver, pulls } = await setup();
  await deliver(event(0, 1));
  await vi.waitFor(async () =>
    expect(!!(await db.drafts.findOne("phone:chat").exec())).toBe(true),
  );
  const remote = event(1, 2);
  const documents = remote.resourceVersions[0].draftChanges!.documents;
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
  mocks.syncPost.mockReturnValueOnce(pendingPush);
  const local = await db.drafts.findOne("phone:chat").exec();
  await local!.incrementalPatch({
    payload: JSON.stringify({
      device: "phone",
      session: "chat",
      text: "local-unsent",
      updated: 2,
    }),
  });
  await vi.waitFor(() => expect(mocks.syncPost).toHaveBeenCalledOnce());
  await deliver(remote);
  expect(pulls()).toHaveLength(1);
  finish(documents);
  await vi.waitFor(() => expect(mocks.syncPost).toHaveBeenCalledTimes(2));
  const pushed = mocks.syncPost.mock.calls[1][1];
  const merged = pushed.rows[0].newDocumentState;
  expect(merged.payload).toContain("remote-new");
  expect(merged.payload).toContain("local-unsent");
  // A successful merged push publishes its own newer draft sequence.
  const confirmed = event(2, 3);
  confirmed.resourceVersions[0].draftChanges!.documents[0].payload =
    merged.payload;
  await deliver(confirmed);
  await vi.waitFor(async () => {
    const payload = (await db.drafts.findOne("phone:chat").exec())?.payload;
    expect(payload).toContain("remote-new");
    expect(payload).toContain("local-unsent");
  });
  expect(pulls()).toHaveLength(1);
});
