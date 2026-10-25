import { afterAll, beforeAll, expect, it, vi } from "vitest";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createInterface } from "node:readline";
import type { RxDatabase } from "rxdb";
import { ReplaySubject, filter, firstValueFrom, timeout } from "rxjs";
import type { Snapshot } from "../types";
import { SYNC_PROTOCOL_VERSION } from "../generated/api";
import { SYNC_ENTITY_STREAM_MAX_DOCUMENTS } from "../generated/sharedValues";
import type { ResourceChangeEvent } from "./subscriptions/events";

const mocks = vi.hoisted(() => ({
  databases: [] as RxDatabase[],
  syncGet: vi.fn(),
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
  ApiError: class extends Error {},
  syncGet: mocks.syncGet,
  syncPost: vi.fn(),
  saved: (_key: string, fallback: unknown) => fallback,
  save: vi.fn(),
  setWorkspace: vi.fn(),
  isApiSchemaMismatch: () => false,
  onMatchingApiSchemaResponse: () => () => false,
}));
vi.mock("./subscriptions/events", () => ({
  acknowledgeEntitySequences: vi.fn(),
  watchResourceConnection: () => () => {},
  watchResourceChanges: () => () => {},
}));
vi.mock("./resume", () => ({ onResume: () => () => {} }));
vi.mock("./entityCacheStorage", async (original) => ({
  ...(await original<typeof import("./entityCacheStorage")>()),
  holdEntityProjectionDatabaseLock: async () => true,
  deleteOtherEntityProjectionDatabases: async () => {},
}));

let origin: string;
let headers: Record<string, string>;
let server: ReturnType<typeof spawn>;
let directory: string;
let stopProjection: (() => void) | undefined;
beforeAll(async () => {
  const binary =
    process.env.STUDIO_DRAFT_SERVER_BINARY ??
    fileURLToPath(
      new URL(
        "../../../../../../target/debug/examples/draft-server",
        import.meta.url,
      ),
    );
  directory = await mkdtemp(join(tmpdir(), "studio-entity-live-"));
  server = spawn(
    binary,
    [directory, "--entity-live", "--entity-transfer-work"],
    {
      stdio: ["pipe", "pipe", "inherit"],
    },
  );
  const lines = createInterface({ input: server.stdout! });
  try {
    const [line] = await Promise.race([
      once(lines, "line", { signal: AbortSignal.timeout(30_000) }),
      once(server, "exit").then(([code]) => {
        throw new Error(`Entity fixture exited: ${code}`);
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
});
afterAll(async () => {
  stopProjection?.();
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
    await Promise.all(mocks.databases.map((db) => db.remove()));
    if (directory) await rm(directory, { recursive: true, force: true });
    vi.unstubAllGlobals();
  }
});
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

class FixtureTransferStorage {
  private rows = new Map<string, string>();
  async put(key: string, value: string) {
    this.rows.set(key, value);
  }
  async putMany(values: [string, string][]) {
    for (const [key, value] of values) this.rows.set(key, value);
  }
  async get(key: string) {
    return this.rows.get(key);
  }
  async remove(prefix: string) {
    for (const key of this.rows.keys())
      if (key.startsWith(prefix)) this.rows.delete(key);
  }
  async close() {
    this.rows.clear();
  }
}
async function* wireFrames(
  response: Response,
): AsyncGenerator<{ name: string; value: unknown }> {
  const reader = response.body!.getReader();
  let buffer = "";
  const decoder = new TextDecoder();
  try {
    for (;;) {
      const chunk = await reader.read();
      if (chunk.done) return;
      buffer += decoder.decode(chunk.value, { stream: true });
      let end: number;
      while ((end = buffer.indexOf("\n\n")) >= 0) {
        const frame = buffer.slice(0, end);
        buffer = buffer.slice(end + 2);
        const name = frame
          .split("\n")
          .find((line) => line.startsWith("event: "))
          ?.slice(7);
        const data = frame
          .split("\n")
          .find((line) => line.startsWith("data: "));
        if (name && data) yield { name, value: JSON.parse(data.slice(6)) };
      }
    }
  } finally {
    await reader.cancel().catch(() => {});
  }
}
async function* resourceFrames(
  response: Response,
): AsyncGenerator<ResourceChangeEvent> {
  const { EntityTransferAssembler } = await import("./entityTransfer");
  const client = await import("./client");
  const assembler = new EntityTransferAssembler(new FixtureTransferStorage());
  try {
    for await (const frame of wireFrames(response)) {
      if (frame.name === "resources") yield frame.value as ResourceChangeEvent;
      if (frame.name !== "entity-transfer") continue;
      const interval = await assembler.receive(frame.value);
      if (!interval) continue;
      await client.persistStagedEntityTransfer(interval, () => true);
      yield {
        protocol: interval.protocol,
        workspaceId: interval.workspaceId,
        epoch: interval.epoch,
        revision: interval.revision,
        reason: interval.reason,
        resources: [interval.resource],
        resourceVersions: [
          {
            resource: interval.resource,
            revision: interval.cursor.seq,
            cursor: interval.cursor,
            replyKind: "delta",
          },
        ],
      };
      await interval.dispose();
    }
  } finally {
    await assembler.invalidate();
  }
}

it("stages a cold >512 entity SSE interval before projection deletion or cursor publication, then replays after a failed write", async () => {
  vi.stubGlobal("window", {
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    matchMedia: vi.fn(() => ({ matches: false })),
  });
  vi.stubGlobal("document", { hidden: true });
  vi.stubGlobal("navigator", {
    onLine: true,
    locks: {
      request: (_key: string, write: () => Promise<unknown>) => write(),
    },
  });
  mocks.syncGet.mockImplementation((path: string) => request(path));
  const module = await import("./client");
  const { createEntitySequenceScope } = await import("./entitySequence");
  const { createTranscriptCache } = await import("./transcriptCache");
  const { EntityTransferAssembler } = await import("./entityTransfer");
  const client = module.createSyncClient({
    api: {
      syncGet: mocks.syncGet,
      syncPost: vi.fn(),
      saved: (_key, fallback) => fallback,
      save: vi.fn(),
      setWorkspace: vi.fn(),
      isApiSchemaMismatch: () => false,
      onMatchingApiSchemaResponse: () => () => false,
    },
    resources: {
      watchResourceChanges: () => () => {},
      watchResourceConnection: () => () => {},
    },
    sequences: createEntitySequenceScope(),
    transcripts: createTranscriptCache(),
    databaseSuffix: "coldtransfer",
  });
  const { db } = await client.syncDatabase();
  await client.persistProjection(db.projections, {
    id: "entity:agentSummary:stale",
    payload: "",
    seq: 0,
    entity: {
      collection: "agentSummary",
      id: "stale",
      value: { id: "stale", source: "managed", status: "idle" },
    },
  });
  const assembler = new EntityTransferAssembler(new FixtureTransferStorage());
  const controller = new AbortController();
  const query = new URLSearchParams({
    protocol: String(SYNC_PROTOCOL_VERSION),
    resources: JSON.stringify([{ kind: "state" }]),
    tokenRates: "false",
  });
  const response = await fetch(`${origin}/api/sync/stream?${query}`, {
    headers,
    signal: controller.signal,
  });
  let complete: import("./entityTransfer").StagedEntityInterval | undefined;
  let stagedFrames = 0;
  try {
    for await (const frame of wireFrames(response)) {
      if (frame.name !== "entity-transfer") continue;
      stagedFrames++;
      complete = await assembler.receive(frame.value);
      if (!complete) {
        expect(
          await client.readSubscriptionCursor({ kind: "state" }),
        ).toBeUndefined();
        expect(
          await db.projections.findOne("entity:agentSummary:stale").exec(),
        ).not.toBeNull();
      } else break;
    }
    expect(stagedFrames).toBeGreaterThan(1);
    expect(complete).toBeDefined();
    const interval = complete!;
    const lockNavigator = navigator;
    vi.stubGlobal("navigator", { onLine: true });
    await expect(
      client.persistStagedEntityTransfer(interval, () => true),
    ).rejects.toThrow("projection write lock");
    expect(
      await client.readSubscriptionCursor({ kind: "state" }),
    ).toBeUndefined();
    vi.stubGlobal("navigator", lockNavigator);
    const outsideScope = {
      ...interval,
      async *pages() {
        for await (const page of interval.pages())
          yield {
            ...page,
            documents: [
              {
                op: "upsert" as const,
                collection: "agentDetail" as const,
                id: "wrong-scope",
                seq: page.through,
                value: { id: "wrong-scope" },
              },
            ],
          };
      },
    };
    await expect(
      client.persistStagedEntityTransfer(outsideScope, () => true),
    ).rejects.toThrow("outside staged subscription scope");
    expect(
      await client.readSubscriptionCursor({ kind: "state" }),
    ).toBeUndefined();
    expect(
      await db.projections.findOne("entity:agentSummary:stale").exec(),
    ).not.toBeNull();
    await client.persistStagedEntityTransfer(interval, () => false);
    expect(
      await client.readSubscriptionCursor({ kind: "state" }),
    ).toBeUndefined();
    const write = vi.spyOn(db.projections.storageInstance, "bulkWrite");
    write.mockRejectedValueOnce(new Error("fixture staging write failure"));
    await expect(
      client.persistStagedEntityTransfer(interval, () => true),
    ).rejects.toThrow("fixture staging write failure");
    expect(
      await client.readSubscriptionCursor({ kind: "state" }),
    ).toBeUndefined();
    expect(
      await db.projections.findOne("entity:agentSummary:stale").exec(),
    ).not.toBeNull();
    await client.persistStagedEntityTransfer(interval, () => true);
    expect((await client.readSubscriptionCursor({ kind: "state" }))?.seq).toBe(
      interval.cursor.seq,
    );
    expect(
      await db.projections.findOne("entity:agentSummary:stale").exec(),
    ).toBeNull();
    expect(
      (
        await db.projections
          .find({
            selector: {
              id: { $gte: "entity:agentSummary:", $lt: "entity:agentSummary;" },
            },
          })
          .exec()
      ).length,
    ).toBeGreaterThan(SYNC_ENTITY_STREAM_MAX_DOCUMENTS);
    const calls = write.mock.calls.length;
    await client.persistStagedEntityTransfer(interval, () => true);
    expect(write.mock.calls.length).toBe(calls);
    write.mockRestore();
    expect(
      mocks.syncGet.mock.calls.every(([path]) => path === "/api/sync/identity"),
    ).toBe(true);
    await interval.dispose();
  } finally {
    controller.abort();
    await assembler.invalidate();
  }
});

it("preserves an existing oversized Work result and >512 history rows, then streams only changed task metadata", async () => {
  vi.stubGlobal("window", {
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    matchMedia: vi.fn(() => ({ matches: false })),
  });
  vi.stubGlobal("document", { hidden: true });
  vi.stubGlobal("navigator", {
    onLine: true,
    locks: {
      request: (_key: string, write: () => Promise<unknown>) => write(),
    },
  });
  mocks.syncGet.mockImplementation((path: string) => request(path));
  const module = await import("./client");
  const { createEntitySequenceScope } = await import("./entitySequence");
  const { createTranscriptCache } = await import("./transcriptCache");
  const { EntityTransferAssembler } = await import("./entityTransfer");
  const { createTaskAssignmentSelector } =
    await import("./subscriptions/workData");
  const { SYNC_STREAM_EVENT_MAX_BYTES } =
    await import("../generated/sharedValues");
  const client = module.createSyncClient({
    api: {
      syncGet: mocks.syncGet,
      syncPost: vi.fn(),
      saved: (_key, fallback) => fallback,
      save: vi.fn(),
      setWorkspace: vi.fn(),
      isApiSchemaMismatch: () => false,
      onMatchingApiSchemaResponse: () => () => false,
    },
    resources: {
      watchResourceChanges: () => () => {},
      watchResourceConnection: () => () => {},
    },
    sequences: createEntitySequenceScope(),
    transcripts: createTranscriptCache(),
    databaseSuffix: "worktransfer",
  });
  const { db } = await client.syncDatabase();
  const resource = { kind: "work", agentId: "lead" } as const;
  const query = new URLSearchParams({
    protocol: String(SYNC_PROTOCOL_VERSION),
    resources: JSON.stringify([resource]),
    tokenRates: "false",
  });
  const controller = new AbortController();
  const response = await fetch(`${origin}/api/sync/stream?${query}`, {
    headers,
    signal: AbortSignal.any([controller.signal, AbortSignal.timeout(10000)]),
  });
  const frames = wireFrames(response)[Symbol.asyncIterator]();
  const assembler = new EntityTransferAssembler(new FixtureTransferStorage());
  let interval: import("./entityTransfer").StagedEntityInterval | undefined;
  let transfers = 0;
  try {
    while (!interval) {
      const frame = await frames.next();
      if (frame.done)
        throw new Error("Work source closed before its complete interval");
      if (frame.value.name !== "entity-transfer") continue;
      transfers++;
      interval = await assembler.receive(frame.value.value);
      if (!interval)
        expect(await client.readSubscriptionCursor(resource)).toBeUndefined();
    }
    expect(transfers).toBeGreaterThan(2);
    await client.persistStagedEntityTransfer(interval, () => true);
    const rows = await db.projections
      .find({ selector: { id: { $gte: "entity:work", $lt: "entity:workz" } } })
      .exec();
    const entities = rows.flatMap((row) => {
      const value = row.toJSON();
      return value.entity ? [value.entity] : [];
    });
    const tasks = createTaskAssignmentSelector()(entities, "lead");
    expect(tasks).toHaveLength(1);
    expect(tasks[0]?.results).toHaveLength(515);
    expect(tasks[0]?.results?.[0]?.text).toBe(
      "界".repeat(SYNC_STREAM_EVENT_MAX_BYTES / 2),
    );
    const resultSequence = rows
      .find((row) => row.id === "entity:workResult:transfer-assignment:0")!
      .toJSON().seq;
    const reads = mocks.syncGet.mock.calls.length;
    await request("/fixture/entity", { workStatus: "review" });
    let delta: ResourceChangeEvent | undefined;
    while (!delta) {
      const frame = await frames.next();
      if (frame.done)
        throw new Error("Work source closed before its metadata delta");
      expect(frame.value.name).not.toBe("entity-transfer");
      if (frame.value.name !== "resources") continue;
      const candidate = frame.value.value as ResourceChangeEvent;
      if (candidate.reason === "change") delta = candidate;
    }
    const changes = delta.resourceVersions.flatMap(
      (entry) => entry.entityChanges?.documents ?? [],
    );
    expect(changes.map((row) => row.collection)).toEqual(["workDetail"]);
    expect(new TextEncoder().encode(JSON.stringify(delta)).length).toBeLessThan(
      SYNC_STREAM_EVENT_MAX_BYTES,
    );
    await client.persistResourceEntityChanges(delta, () => true);
    expect(
      (
        await db.projections
          .findOne("entity:workResult:transfer-assignment:0")
          .exec()
      )?.toJSON().seq,
    ).toBe(resultSequence);
    expect(
      (
        await db.projections
          .findOne("entity:workDetail:transfer-assignment")
          .exec()
      )?.toJSON().entity?.value,
    ).toMatchObject({ status: "review" });
    expect(mocks.syncGet.mock.calls.length).toBe(reads);
    await request("/fixture/entity", { appendWorkResult: true });
    let append: ResourceChangeEvent | undefined;
    while (!append) {
      const frame = await frames.next();
      if (frame.done)
        throw new Error("Work source closed before its append delta");
      expect(frame.value.name).not.toBe("entity-transfer");
      if (
        frame.value.name === "resources" &&
        (frame.value.value as ResourceChangeEvent).reason === "change"
      )
        append = frame.value.value as ResourceChangeEvent;
    }
    const appended = append.resourceVersions.flatMap(
      (entry) => entry.entityChanges?.documents ?? [],
    );
    expect(
      appended
        .filter((row) => row.collection === "workResult")
        .map((row) => row.id),
    ).toEqual(["transfer-assignment:515"]);
    expect(appended).toHaveLength(2);
    expect(
      new TextEncoder().encode(JSON.stringify(append)).length,
    ).toBeLessThan(SYNC_STREAM_EVENT_MAX_BYTES);
    await client.persistResourceEntityChanges(append, () => true);
    expect(
      (
        await db.projections
          .findOne("entity:workResult:transfer-assignment:0")
          .exec()
      )?.toJSON().seq,
    ).toBe(resultSequence);
    expect(mocks.syncGet.mock.calls.length).toBe(reads);
    await interval.dispose();
  } finally {
    controller.abort();
    await frames.return(undefined);
    await assembler.invalidate();
  }
}, 15000);

it("applies source commits beyond a full entity page through real SSE and RxDB without refresh reads", async () => {
  vi.stubGlobal("window", {
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    matchMedia: vi.fn(() => ({ matches: false })),
  });
  vi.stubGlobal("document", { hidden: true });
  vi.stubGlobal("navigator", {
    onLine: true,
    locks: {
      request: (_key: string, write: () => Promise<unknown>) => write(),
    },
  });
  mocks.syncGet.mockImplementation(
    (path: string, options?: { query?: Record<string, string> }) =>
      request(
        `${path}${options?.query ? `?${new URLSearchParams(options.query)}` : ""}`,
      ),
  );
  const client = await import("./client");
  const { ResourceEntityDelivery } = await import("./resourceEntityDelivery");
  const snapshots = new ReplaySubject<Snapshot>(1);
  const failed = vi.fn();
  let latest: Snapshot | undefined;
  const stop = client.subscribeStateProjection((value) => {
    if (value) {
      latest = value;
      snapshots.next(value);
    }
  }, failed);
  stopProjection = stop;
  const waitSnapshot = (accept: (snapshot: Snapshot) => boolean) =>
    firstValueFrom(snapshots.pipe(filter(accept), timeout(5000)));
  const baseline = await waitSnapshot(
    (value) => value.entitiesComplete === true,
  ).catch((error) => {
    throw new Error(
      `Initial projection did not settle: ${JSON.stringify({ reads: mocks.syncGet.mock.calls, failures: failed.mock.calls.map(([value]) => String(value)), rows: latest?.threads.length, complete: latest?.entitiesComplete })}`,
      { cause: error },
    );
  });
  expect(baseline.threads.length).toBeGreaterThan(
    SYNC_ENTITY_STREAM_MAX_DOCUMENTS,
  );
  const { db, workspaceId } = await client.syncDatabase();
  const queryProjection = vi.spyOn(db.projections, "find");
  const controller = new AbortController();
  const frames = new ReplaySubject<ResourceChangeEvent>();
  const dispatched = new ReplaySubject<ResourceChangeEvent>();
  const delivery = new ResourceEntityDelivery(
    client.persistResourceEntityChanges,
    (event) => dispatched.next(event),
  );
  const query = new URLSearchParams({
    protocol: String(SYNC_PROTOCOL_VERSION),
    resources: JSON.stringify([
      { kind: "state" },
      { kind: "tasks", agentId: "lead" },
    ]),
  });
  const response = await fetch(`${origin}/api/sync/stream?${query}`, {
    headers,
    signal: controller.signal,
  });
  expect(response.ok).toBe(true);
  let forwardFrames = true;
  const pump = (async () => {
    for await (const event of resourceFrames(response)) {
      frames.next(event);
      if (forwardFrames) delivery.receive(event);
    }
  })().catch((error) => {
    if (!controller.signal.aborted) throw error;
  });
  const waitFrame = (accept: (event: ResourceChangeEvent) => boolean) =>
    firstValueFrom(frames.pipe(filter(accept), timeout(5000)));
  try {
    await firstValueFrom(dispatched.pipe(timeout(5000)));
    const reads = mocks.syncGet.mock.calls.length;
    const worker = {
      id: "worker",
      rootId: "lead",
      parentId: "lead",
      epoch: 1,
      status: "idle",
      name: "New worker",
      model: "model-before",
    };
    const created = await request("/fixture/entity", { agent: worker });
    const createdFrame = await waitFrame((event) =>
      event.resourceVersions.some((entry) =>
        entry.entityChanges?.documents.some(
          (row) => row.collection === "agentSummary" && row.id === "worker",
        ),
      ),
    );
    expect(createdFrame.reason).toBe("change");
    const shown = await waitSnapshot((value) =>
      value.threads.some((agent) => agent.id === "worker"),
    );
    expect(
      shown.threads.filter((agent) => agent.rootId === "lead"),
    ).toHaveLength(2);
    expect(shown.threads.find((agent) => agent.id === "older-0")).toBe(
      baseline.threads.find((agent) => agent.id === "older-0"),
    );
    expect(await client.readSubscriptionCursor({ kind: "state" })).toEqual({
      workspaceId,
      seq: created.seq,
    });
    const changed = await request("/fixture/entity", {
      agent: { ...worker, status: "running", model: "model-after" },
      task: {
        id: "command",
        agent: "worker",
        kind: "command",
        status: "running",
        created: 1,
      },
    });
    const running = await waitSnapshot(
      (value) =>
        value.threads.some(
          (agent) =>
            agent.id === "worker" &&
            agent.status === "running" &&
            agent.model === "model-after",
        ) &&
        value.runtime.tasks.some(
          (task) => task.id === "command" && task.status === "running",
        ),
    );
    expect(running.threads).toHaveLength(shown.threads.length);
    for (const status of ["interrupted", "completed"] as const) {
      await request("/fixture/entity", {
        agent: { ...worker, status, model: "model-after" },
      });
      const next = await waitSnapshot((value) =>
        value.threads.some(
          (agent) => agent.id === "worker" && agent.status === status,
        ),
      );
      expect(next.threads.find((agent) => agent.id === "lead")).toBe(
        shown.threads.find((agent) => agent.id === "lead"),
      );
    }
    const leadChanged = await request("/fixture/entity", {
      agent: {
        id: "lead",
        rootId: "lead",
        epoch: 1,
        status: "running",
        name: "Lead",
      },
    });
    await waitSnapshot((value) =>
      value.threads.some(
        (agent) => agent.id === "lead" && agent.status === "running",
      ),
    );
    const beforeProject = latest!.threads;
    const project = {
      id: "new-project",
      path: directory,
      name: "Live project",
      created: 1,
    };
    const projectAdded = await request("/fixture/entity", { project });
    const withProject = await waitSnapshot((value) =>
      value.runtime.projects.some(
        (row) => row.id === project.id && row.name === project.name,
      ),
    );
    expect(withProject.threads).toBe(beforeProject);
    const cursor = await client.readSubscriptionCursor({ kind: "state" });
    expect(cursor?.seq).toBe(projectAdded.seq);
    expect(cursor!.seq).toBeGreaterThan(leadChanged.seq);
    expect(cursor!.seq).toBeGreaterThan(changed.seq);
    await client.persistResourceEntityChanges(createdFrame, () => true);
    expect(await client.readSubscriptionCursor({ kind: "state" })).toEqual(
      cursor,
    );
    const [stored] = await db.projections.storageInstance.findDocumentsById(
      ["entity:agentSummary:worker"],
      false,
    );
    expect(stored.entity?.collection).toBe("agentSummary");
    forwardFrames = false;
    const titled = await request("/fixture/entity", {
      agent: {
        ...worker,
        name: "Automatic title",
        status: "completed",
        model: "model-after",
      },
    });
    const titleFrame = await waitFrame((event) =>
      event.resourceVersions.some((entry) =>
        entry.entityChanges?.documents.some(
          (row) =>
            row.op === "upsert" &&
            row.collection === "agentSummary" &&
            row.id === "worker" &&
            row.value.name === "Automatic title",
        ),
      ),
    );
    const writes = vi.spyOn(db.projections.storageInstance, "bulkWrite");
    writes.mockRejectedValueOnce(new Error("local storage unavailable"));
    await expect(
      client.persistResourceEntityChanges(titleFrame, () => true),
    ).rejects.toThrow("local storage unavailable");
    expect(await client.readSubscriptionCursor({ kind: "state" })).toEqual(
      cursor,
    );
    await client.persistResourceEntityChanges(titleFrame, () => false);
    expect(await client.readSubscriptionCursor({ kind: "state" })).toEqual(
      cursor,
    );
    writes.mockRestore();
    controller.abort();
    await pump;
    const reconnect = new AbortController();
    try {
      const resumeQuery = new URLSearchParams({
        protocol: String(SYNC_PROTOCOL_VERSION),
        reconnect: "true",
        resources: JSON.stringify([{ kind: "state" }]),
        since: JSON.stringify([{ resource: { kind: "state" }, since: cursor }]),
      });
      const resumed = await fetch(`${origin}/api/sync/stream?${resumeQuery}`, {
        headers,
        signal: reconnect.signal,
      });
      expect(resumed.ok).toBe(true);
      for await (const frame of resourceFrames(resumed)) {
        expect(frame.reason).toBe("reconnect");
        expect(frame.resourceVersions[0].replyKind).toBe("delta");
        expect(frame.resourceVersions[0].entityChanges?.after).toBe(
          cursor!.seq,
        );
        await client.persistResourceEntityChanges(frame, () => true);
        break;
      }
      await waitSnapshot((value) =>
        value.threads.some(
          (agent) => agent.id === "worker" && agent.name === "Automatic title",
        ),
      );
      expect(await client.readSubscriptionCursor({ kind: "state" })).toEqual({
        workspaceId,
        seq: titled.seq,
      });
    } finally {
      reconnect.abort();
    }
    expect(mocks.syncGet.mock.calls).toHaveLength(reads);
    expect(
      queryProjection.mock.calls.filter(
        ([query]) =>
          (query?.selector?.id as { $gte?: string } | undefined)?.$gte ===
          "entity:",
      ),
    ).toEqual([]);
    expect(failed.mock.calls.filter(([error]) => error !== null)).toEqual([]);
  } finally {
    stop();
    stopProjection = undefined;
    controller.abort();
    await pump;
  }
}, 20000);
