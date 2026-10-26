import assert from "node:assert/strict";
import { DRAFT_SERVER_READY_TIMEOUT_MS } from "./draftServerFixture";
import { afterAll, afterEach, beforeAll, expect, it, vi } from "vitest";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import fixture from "../../../../contracts/draft-replication.json";
import type { RxDatabase } from "rxdb";
import { firstValueFrom, ReplaySubject, filter, skip } from "rxjs";
import { SYNC_PROTOCOL_VERSION } from "../generated/api";
import type {
  ResourceChangeEvent,
  ResourceVersion,
} from "./subscriptions/events";

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

// The gate builds this fixture once. Vitest owns the real HTTP/RxDB exchange;
// Rust nextest never launches Node or a second test runner.
let origin: string;
let headers: Record<string, string>;
let server: ReturnType<typeof spawn>;
let directory: string;
beforeAll(async () => {
  const binary =
    process.env.STUDIO_DRAFT_SERVER_BINARY ??
    fileURLToPath(
      new URL(
        "../../../../../../target/debug/examples/draft-server",
        import.meta.url,
      ),
    );
  directory = await mkdtemp(join(tmpdir(), "studio-draft-"));
  server = spawn(binary, [directory], { stdio: ["pipe", "pipe", "inherit"] });
  const lines = createInterface({ input: server.stdout! });
  try {
    const [line] = await Promise.race([
      once(lines, "line", {
        signal: AbortSignal.timeout(DRAFT_SERVER_READY_TIMEOUT_MS),
      }),
      once(server, "exit").then(([code]) => {
        throw new Error(`Draft fixture exited: ${code}`);
      }),
    ]);
    const ready = JSON.parse(line);
    origin = `http://127.0.0.1:${ready.port}`;
    headers = {
      "content-type": "application/json",
      "x-canvas-token": "draft-witness",
      "x-canvas-workspace": ready.workspaceId,
    };
  } finally {
    lines.close();
  }
}, DRAFT_SERVER_READY_TIMEOUT_MS);
afterAll(async () => {
  try {
    if (server?.pid && server.exitCode === null && server.signalCode === null) {
      const exited = once(server, "exit", {
        signal: AbortSignal.timeout(10_000),
      });
      server.stdin?.end();
      try {
        await exited;
      } catch (error) {
        if (server.exitCode === null && server.signalCode === null) {
          const killed = once(server, "exit");
          server.kill("SIGKILL");
          await killed;
        }
        throw error;
      }
    }
  } finally {
    if (directory) await rm(directory, { recursive: true, force: true });
  }
});
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
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
  const value = await response.json();
  expect(response.ok, JSON.stringify(value)).toBe(true);
  return value;
}

it.each(fixture.orders)(
  "single writer avoids false conflicts with %s",
  async (order) => {
    vi.stubGlobal("window", {
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
    });
    vi.stubGlobal("navigator", { onLine: true });
    const pulled = barrier<void>();
    mocks.syncGet.mockImplementation(
      async (path: string, options?: { query?: Record<string, string> }) => {
        const value = await request(
          `${path}${options?.query ? `?${new URLSearchParams(options.query)}` : ""}`,
        );
        if (path === "/api/sync/pull") pulled.resolve();
        return value;
      },
    );
    const client = await import("./client");
    const { encodeDraftPayload } = await import("./draftPayload");
    const accepted = barrier<void>();
    const secondPost = barrier<void>();
    const release = barrier<void>();
    const posts: { body: PushRequest; conflicts: SyncDocument[] }[] = [];
    mocks.syncPost.mockImplementation(
      async (_path: string, body: PushRequest) => {
        const conflicts = await request("/api/sync/drafts", body);
        posts.push({ body: structuredClone(body), conflicts });
        if (posts.length === 2) secondPost.resolve();
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
    const received = new ReplaySubject<ResourceChangeEvent>();
    let forwardFrames = false;
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
          if (data) {
            const value: ResourceChangeEvent = JSON.parse(data.slice(6));
            frames.push(value);
            if (forwardFrames) deliver(value);
            received.next(value);
          }
        }
      }
    })().catch((error) => {
      if (!controller.signal.aborted) throw error;
    });
    function deliver(frame: ResourceChangeEvent) {
      const version = frame.resourceVersions[0];
      assert.ok(version != null);
      mocks.invalidate?.({
        epoch: frame.epoch,
        reason: frame.reason,
        revision: version.revision,
        draftChanges: version.draftChanges ?? undefined,
      });
    }
    try {
      deliver(await firstValueFrom(received));
      await pulled.promise;
      assert.ok(mocks.replications[0] != null);
      await mocks.replications[0].awaitInSync();
      expect(
        mocks.syncGet.mock.calls.some(([path]) => path === "/api/sync/pull"),
      ).toBe(true);
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
      await edit(1);
      await accepted.promise;
      await edit(2); // A local edit arrives while the first HTTP result is held.
      const echo = async () =>
        deliver(
          await firstValueFrom(
            received.pipe(
              filter((frame) => {
                assert.ok(frame.resourceVersions[0] != null);
                return !!frame.resourceVersions[0].draftChanges?.documents.some(
                  (doc) => doc.id === id,
                );
              }),
            ),
          ),
        );
      await firstValueFrom(received.pipe(skip(1)));
      const acknowledged = barrier<void>();
      const subscription = mocks.replications[0].sent$.subscribe(() =>
        acknowledged.resolve(),
      );
      if (order === "echo-first" || order === "lost-response") await echo();
      if (order === "second-writer") {
        assert.ok(posts[0] != null);
        assert.ok(posts[0].body.rows[0] != null);
        const row = posts[0].body.rows[0]?.newDocumentState;
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
          await secondPost.promise;
          expect(posts).toHaveLength(2);
          assert.ok(mocks.replications[0] != null);
          await mocks.replications[0].awaitInSync();
        }
        await echo();
        if (order === "late-echo") {
          assert.ok(mocks.replications[0] != null);
          await mocks.replications[0].awaitInSync();
        }
      }
      await edit(3);
      subscription.unsubscribe();
      // Forward later master echoes as they arrive, after the controlled ordering.
      forwardFrames = true;
      for (const frame of frames.slice(1)) deliver(frame);
      const expectedText =
        order === "second-writer" ? "other writer" : "edit-3";
      await firstValueFrom(
        received.pipe(
          filter((frame) => {
            assert.ok(frame.resourceVersions[0] != null);
            return !!frame.resourceVersions[0].draftChanges?.documents.some(
              (doc) => {
                if (doc.id !== id) return false;
                const payload = JSON.parse(doc.payload);
                return (
                  payload.text === expectedText &&
                  (order !== "second-writer" ||
                    payload.alternatives?.includes("edit-3"))
                );
              },
            );
          }),
        ),
      );
      await mocks.replications[0].awaitInSync();
      const page = await request("/api/sync/pull?scope=drafts");
      const current = page.documents.find((doc: SyncDocument) => doc.id === id);
      expect(JSON.parse(current.payload).text).toBe(expectedText);
      if (order === "second-writer")
        expect(JSON.parse(current.payload).alternatives).toContain("edit-3");
      const local = await firstValueFrom(
        db.drafts
          .findOne(id)
          .$.pipe(filter((doc) => !!doc?.payload.includes("edit-3"))),
      );
      expect(local?.payload).toContain("edit-3");
      expect(posts.at(-1)?.body.rows[0]?.newDocumentState.payload).toContain(
        "edit-3",
      );
      console.log(
        JSON.stringify({
          order,
          posts: posts.length,
          conflicts: posts.filter((post) => post.conflicts.length).length,
          trace: posts.map(({ body, conflicts }) => {
            assert.ok(body.rows[0] != null);
            return {
              sent: JSON.parse(body.rows[0]?.newDocumentState.payload).updated,
              assumed: body.rows[0].assumedMasterState
                ? JSON.parse(body.rows[0].assumedMasterState.payload).updated
                : null,
              conflicts: conflicts.map(
                (row) => JSON.parse(row.payload).updated,
              ),
            };
          }),
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

it.each([false, true])(
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
    assert.ok(mocks.replications[0] != null);
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
    const local = await firstValueFrom(
      db.drafts.findOne(id).$.pipe(filter((doc) => doc?.seq === master.seq)),
    );
    expect(local?.payload).toBe(master.payload);
    expect(local?.seq).toBe(master.seq);
  },
);

it.each(fixture.legacy.map(({ session }) => session))(
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
    assert.ok(mocks.replications[0] != null);
    await mocks.replications[0].awaitInSync();
    expect(posts).toHaveLength(session === "legacy-conflict" ? 2 : 1);
    const latest = (
      await request("/api/sync/pull?scope=drafts")
    ).documents.find((doc: SyncDocument) => doc.id === id);
    const payload = JSON.parse(latest.payload);
    expect(payload.metadata).toEqual({ kept: true });
    if (session === "legacy-conflict") {
      assert.ok(posts[0] != null);
      expect(posts[0].conflicts).toHaveLength(1);
      assert.ok(posts[1] != null);
      expect(posts[1].conflicts).toEqual([]);
      assert.ok(posts[1].body.rows[0] != null);
      expect(posts[1].body.rows[0].assumedMasterState?.payload).toBe(
        master.payload,
      );
      expect(payload.alternatives).toContain("pending local");
    } else {
      expect(latest.seq).toBe(master.seq);
      assert.ok(posts[0] != null);
      assert.ok(posts[0].body.rows[0] != null);
      expect(posts[0].body.rows[0]?.newDocumentState.payload).toBe(
        master.payload,
      );
    }
    expect((await db.drafts.findOne(id).exec())?.payload).toBe(latest.payload);
  },
);
