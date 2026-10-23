import { afterEach, expect, it, vi } from "vitest";
import type { RxDatabase } from "rxdb";
import { SYNC_PROTOCOL_VERSION } from "../generated/api";
import type { ResourceChangeEvent, ResourceVersion } from "./resourceEvents";

const mocks = vi.hoisted(() => ({
  databases: [] as RxDatabase[],
  replications: [] as ReturnType<
    typeof import("rxdb/plugins/replication").replicateRxCollection
  >[],
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
vi.mock("rxdb/plugins/replication", async (original) => {
  const module = await original<typeof import("rxdb/plugins/replication")>();
  return {
    ...module,
    replicateRxCollection: (
      ...args: Parameters<typeof module.replicateRxCollection>
    ) => {
      const current = module.replicateRxCollection(...args);
      mocks.replications.push(current);
      return current;
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
  registerSyncEntityPersister: vi.fn(),
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
  await Promise.all(mocks.databases.map((db) => db.remove()));
  mocks.databases = [];
  mocks.replications = [];
  mocks.syncGet.mockReset();
  mocks.syncPost.mockReset();
  vi.unstubAllGlobals();
  vi.resetModules();
});

const origin = process.env.STUDIO_DRAFT_WITNESS_URL;
const workspaceId = process.env.STUDIO_DRAFT_WITNESS_WORKSPACE!;
const headers = {
  "content-type": "application/json",
  "x-canvas-token": "draft-witness",
  "x-canvas-workspace": workspaceId,
};
type PushRequest =
  import("../generated/api").components["schemas"]["DraftPushRequest"];
type SyncDocument = import("./client").SyncDocument;

function barrier<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

async function request(path: string, body?: unknown) {
  const response = await fetch(`${origin}${path}`, {
    headers,
    method: body === undefined ? "GET" : "POST",
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const value = await response.json();
  expect(response.ok, JSON.stringify(value)).toBe(true);
  return value;
}

it
  .skipIf(!origin)
  .each([
    "ack-first",
    "late-echo",
    "echo-first",
    "lost-response",
    "second-writer",
  ] as const)(
  "single writer avoids false conflicts with %s",
  async (order) => {
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    mocks.syncGet.mockImplementation(
      (path: string, options?: { query?: Record<string, string> }) =>
        request(
          `${path}${options?.query ? `?${new URLSearchParams(options.query)}` : ""}`,
        ),
    );
    const client = await import("./client");
    const { encodeDraftPayload } = await import("./draftPayload");
    const accepted = barrier<void>();
    const release = barrier<void>();
    const posts: { body: PushRequest; conflicts: SyncDocument[] }[] = [];
    mocks.syncPost.mockImplementation(
      async (_path: string, body: PushRequest) => {
        const conflicts = await request("/api/sync/drafts", body);
        posts.push({ body: structuredClone(body), conflicts });
        if (posts.length === 1) {
          accepted.resolve();
          await release.promise;
          if (order === "lost-response")
            throw new Error("accepted response lost");
        }
        return conflicts;
      },
    );
    const errors: unknown[] = [];
    stop = await client.startDraftReplication((error) => {
      if (error) errors.push(error);
    });
    const { db } = await client.syncDatabase();
    const controller = new AbortController();
    const query = new URLSearchParams({
      protocol: String(SYNC_PROTOCOL_VERSION),
      resources: JSON.stringify([{ kind: "drafts" }]),
    });
    const response = await fetch(`${origin}/api/sync/stream?${query}`, {
      headers,
      signal: controller.signal,
    });
    expect(response.ok).toBe(true);
    const reader = response.body!.getReader();
    const frames: ResourceChangeEvent[] = [];
    const pump = (async () => {
      let buffer = "";
      const decoder = new TextDecoder();
      while (true) {
        const chunk = await reader.read();
        if (chunk.done) return;
        buffer += decoder.decode(chunk.value, { stream: true });
        let end: number;
        while ((end = buffer.indexOf("\n\n")) >= 0) {
          const frame = buffer.slice(0, end);
          buffer = buffer.slice(end + 2);
          if (!frame.includes("event: resources")) continue;
          const data = frame
            .split("\n")
            .find((line) => line.startsWith("data: "));
          if (data) frames.push(JSON.parse(data.slice(6)));
        }
      }
    })().catch((error) => {
      if (!controller.signal.aborted) throw error;
    });
    const deliver = (frame: ResourceChangeEvent) => {
      const version = frame.resourceVersions[0];
      mocks.invalidate?.({
        epoch: frame.epoch,
        reason: frame.reason,
        revision: version.revision,
        draftChanges: version.draftChanges ?? undefined,
      });
    };
    try {
      await vi.waitFor(() => expect(frames.length).toBeGreaterThan(0));
      deliver(frames[0]);
      await vi.waitFor(() =>
        expect(
          mocks.syncGet.mock.calls.some(([path]) => path === "/api/sync/pull"),
        ).toBe(true),
      );
      const id = `phone:writer:${order}`;
      const edit = async (updated: number) => {
        const payload = encodeDraftPayload({
          id,
          device: "phone",
          session: order,
          text: `edit-${updated}`,
          updated,
          seen: { [id]: updated - 1 },
          alternatives: [],
        });
        const current = await db.drafts.findOne(id).exec();
        if (current) await current.incrementalPatch({ payload });
        else await db.drafts.insert({ id, seq: 0, payload, _deleted: false });
      };
      await mocks.replications[0].awaitInitialReplication();
      await edit(1);
      await accepted.promise;
      await edit(2); // A local edit arrives while the first HTTP result is held.
      const echo = async () => {
        await vi.waitFor(() =>
          expect(
            frames.some((frame) =>
              frame.resourceVersions[0].draftChanges?.documents.some(
                (doc) => doc.id === id,
              ),
            ),
          ).toBe(true),
        );
        deliver(
          frames.find((frame) =>
            frame.resourceVersions[0].draftChanges?.documents.some(
              (doc) => doc.id === id,
            ),
          )!,
        );
      };
      await vi.waitFor(() => expect(frames.length).toBeGreaterThan(1));
      const acknowledged = barrier<void>();
      const subscription = mocks.replications[0].sent$.subscribe(() =>
        acknowledged.resolve(),
      );
      if (order === "echo-first" || order === "lost-response") await echo();
      if (order === "second-writer") {
        const row = posts[0].body.rows[0].newDocumentState;
        const payload = encodeDraftPayload({
          ...JSON.parse(row.payload),
          text: "other writer",
          updated: 4,
          seen: {},
        });
        expect(
          await request("/api/sync/drafts", {
            rows: [
              {
                newDocumentState: { ...row, payload },
                assumedMasterState: row,
              },
            ],
          }),
        ).toEqual([]);
      }
      release.resolve();
      if (order === "ack-first" || order === "late-echo") {
        await acknowledged.promise;
        if (order === "late-echo") {
          await vi.waitFor(() => expect(posts.length).toBe(2));
          await mocks.replications[0].awaitInSync();
        }
        await echo();
        if (order === "late-echo") {
          await mocks.replications[0].awaitInSync();
        }
      }
      await edit(3);
      subscription.unsubscribe();
      // Release every complete master echo, including those from later writes.
      let delivered = 1;
      await vi.waitFor(
        async () => {
          while (delivered < frames.length) deliver(frames[delivered++]);
          const page = await request("/api/sync/pull?scope=drafts");
          const current = page.documents.find(
            (doc: SyncDocument) => doc.id === id,
          );
          expect(current && JSON.parse(current.payload).text).toBe(
            order === "second-writer" ? "other writer" : "edit-3",
          );
          if (order === "second-writer")
            expect(JSON.parse(current.payload).alternatives).toContain(
              "edit-3",
            );
        },
        { timeout: 10000 },
      );
      await vi.waitFor(async () => {
        while (delivered < frames.length) deliver(frames[delivered++]);
        expect((await db.drafts.findOne(id).exec())?.payload).toContain(
          "edit-3",
        );
        expect(posts.at(-1)?.body.rows[0].newDocumentState.payload).toContain(
          "edit-3",
        );
      });
      await mocks.replications[0].awaitInSync();
      console.log(
        JSON.stringify({
          order,
          posts: posts.length,
          conflicts: posts.filter((post) => post.conflicts.length).length,
          trace: posts.map(({ body, conflicts }) => ({
            sent: JSON.parse(body.rows[0].newDocumentState.payload).updated,
            assumed: body.rows[0].assumedMasterState
              ? JSON.parse(body.rows[0].assumedMasterState.payload).updated
              : null,
            conflicts: conflicts.map((row) => JSON.parse(row.payload).updated),
          })),
          replications: mocks.replications.length,
          reportedErrors: errors.length,
          pulls: mocks.syncGet.mock.calls,
        }),
      );
      expect(posts.filter((post) => post.conflicts.length)).toHaveLength(
        order === "second-writer" ? 1 : 0,
      );
      expect(posts.length).toBeLessThanOrEqual(
        order === "ack-first" || order === "echo-first" ? 2 : 3,
      );
    } finally {
      release.resolve();
      controller.abort();
      await pump;
    }
  },
  20000,
);

async function prepareClient() {
  vi.stubGlobal("window", {
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
  });
  vi.stubGlobal("navigator", { onLine: true });
  mocks.syncGet.mockImplementation(
    (path: string, options?: { query?: Record<string, string> }) =>
      request(
        `${path}${options?.query ? `?${new URLSearchParams(options.query)}` : ""}`,
      ),
  );
  return import("./client");
}

it.skipIf(!origin).each([false, true])(
  "applies a later accepted lower-timestamp master (unseen alternatives: %s)",
  async (withAlternatives) => {
    const client = await prepareClient();
    const { encodeDraftPayload } = await import("./draftPayload");
    const session = `lower-clock-${withAlternatives}`;
    const id = `phone:writer:${session}`;
    const accepted = {
      id,
      seq: 0,
      deleted: false,
      payload: encodeDraftPayload({
        id,
        device: "phone",
        session,
        text: "accepted local",
        updated: 2001,
        seen: { [id]: 2000 },
      }),
    };
    expect(
      await request("/api/sync/drafts", {
        rows: [{ newDocumentState: accepted }],
      }),
    ).toEqual([]);
    mocks.syncPost.mockImplementation((_path, body) =>
      request("/api/sync/drafts", body),
    );
    stop = await client.startDraftReplication(() => {});
    mocks.invalidate?.({ epoch: "witness", reason: "initial", revision: 0 });
    await mocks.replications[0].awaitInitialReplication();
    const { db } = await client.syncDatabase();
    const before = (
      await request("/api/sync/pull?scope=drafts")
    ).documents.find((doc: SyncDocument) => doc.id === id);
    expect((await db.drafts.findOne(id).exec())?.payload).toBe(before.payload);
    const payload = encodeDraftPayload({
      id,
      device: "phone",
      session,
      text: "later accepted remote",
      updated: 1999,
      seen: {},
      ...(withAlternatives ? { alternatives: ["unseen text"] } : {}),
    });
    expect(
      await request("/api/sync/drafts", {
        rows: [
          {
            newDocumentState: { ...accepted, payload },
            assumedMasterState: {
              id: before.id,
              payload: before.payload,
              seq: before.seq,
              deleted: before._deleted,
            },
          },
        ],
      }),
    ).toEqual([]);
    const page = await request("/api/sync/pull?scope=drafts");
    const master = page.documents.find((doc: SyncDocument) => doc.id === id);
    expect(master.seq).toBeGreaterThan(before.seq);
    mocks.invalidate?.({
      epoch: "witness",
      reason: "change",
      revision: master.seq,
      draftChanges: {
        after: before.seq,
        through: master.seq,
        documents: [master],
      },
    });
    await mocks.replications[0].awaitInSync();
    await vi.waitFor(async () => {
      const local = await db.drafts.findOne(id).exec();
      expect(local?.payload).toBe(master.payload);
      expect(local?.seq).toBe(master.seq);
    });
  },
);

it.skipIf(!origin).each(["legacy-conflict", "legacy-lost-response"] as const)(
  "acknowledges persisted pending payload through %s",
  async (session) => {
    const client = await prepareClient();
    const { encodeDraftPayload } = await import("./draftPayload");
    const id = `phone:writer:${session}`;
    const master = (
      await request("/api/sync/pull?scope=drafts")
    ).documents.find((doc: SyncDocument) => doc.id === id);
    const { db } = await client.syncDatabase();
    await db.drafts.insert({
      id,
      seq: 0,
      _deleted: false,
      payload:
        session === "legacy-lost-response"
          ? master.payload
          : encodeDraftPayload({
              id,
              device: "phone",
              session,
              text: "pending local",
              updated: 4,
            }),
    });
    const posts: { body: PushRequest; conflicts: SyncDocument[] }[] = [];
    mocks.syncPost.mockImplementation(async (_path, body) => {
      const conflicts = await request("/api/sync/drafts", body);
      posts.push({ body: structuredClone(body), conflicts });
      return conflicts;
    });
    stop = await client.startDraftReplication(() => {});
    await vi.waitFor(() =>
      expect(posts).toHaveLength(session === "legacy-conflict" ? 2 : 1),
    );
    await mocks.replications[0].awaitInSync();
    const latest = (
      await request("/api/sync/pull?scope=drafts")
    ).documents.find((doc: SyncDocument) => doc.id === id);
    const payload = JSON.parse(latest.payload);
    expect(payload.metadata).toEqual({ kept: true });
    if (session === "legacy-conflict") {
      expect(posts[0].conflicts).toHaveLength(1);
      expect(posts[1].conflicts).toEqual([]);
      expect(posts[1].body.rows[0].assumedMasterState?.payload).toBe(
        master.payload,
      );
      expect(payload.alternatives).toContain("pending local");
    } else {
      expect(latest.seq).toBe(master.seq);
      expect(posts[0].body.rows[0].newDocumentState.payload).toBe(
        master.payload,
      );
    }
    expect((await db.drafts.findOne(id).exec())?.payload).toBe(latest.payload);
  },
);
