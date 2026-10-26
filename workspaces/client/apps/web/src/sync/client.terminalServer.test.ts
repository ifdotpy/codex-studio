import assert from "node:assert/strict";
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
import type { ResourceVersion } from "./resourceEvents";
import { createDataObserver } from "./subscriptions/data";
import { SYNC_PROTOCOL_VERSION } from "../generated/api";
import { SYNC_TERMINAL_STREAM_MAX_CHARS } from "../generated/sharedValues";
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
  onMatchingApiSchemaResponse: () => () => {},
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
beforeAll(async () => {
  const binary =
    process.env.STUDIO_DRAFT_SERVER_BINARY ??
    fileURLToPath(
      new URL(
        "../../../../../../target/debug/examples/draft-server",
        import.meta.url,
      ),
    );
  directory = await mkdtemp(join(tmpdir(), "studio-terminal-live-"));
  server = spawn(binary, [directory, "--terminal-live"], {
    stdio: ["pipe", "pipe", "inherit"],
  });
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
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
  const value = await response.json();
  expect(response.ok, JSON.stringify(value)).toBe(true);
  return value;
}

async function* resourceFrames(
  response: Response,
  session?: (id: string) => void,
): AsyncGenerator<ResourceChangeEvent> {
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
        const data = frame
          .split("\n")
          .find((line) => line.startsWith("data: "));
        if (!data) continue;
        if (frame.includes("event: session"))
          session?.(JSON.parse(data.slice(6)).sessionId);
        if (frame.includes("event: resources")) yield JSON.parse(data.slice(6));
      }
    }
  } finally {
    await reader.cancel().catch(() => {});
  }
}

it("streams terminal intervals through RxDB and the screen consumer with zero output GETs", async () => {
  vi.stubGlobal("window", {
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    matchMedia: vi.fn(() => ({ matches: false })),
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
  mocks.syncGet.mockImplementation(
    (path: string, options?: { query?: Record<string, string> }) =>
      request(
        `${path}${options?.query ? `?${new URLSearchParams(options.query)}` : ""}`,
      ),
  );
  const client = await import("./client");
  const { ResourceEntityDelivery } = await import("./resourceEntityDelivery");
  const { workspaceId } = await client.syncDatabase();
  const resource = { kind: "terminal-output", terminalId: "shell" } as const;
  const write = (text: string, offset = 0, status = "running", id = "shell") =>
    request("/fixture/terminal", { id, text, offset, status });
  await write("a🙂");
  await write("other", 0, "running", "other");
  let receive: (version?: ResourceVersion) => void = () => {};
  const accept = vi.fn();
  const failed = vi.fn();
  const recover = vi.fn();
  const stop = createDataObserver(
    () => client,
    {
      watchResourceChanges: (_ref, listener) => {
        receive = listener;
        return () => {
          receive = () => {};
        };
      },
      watchResourceConnection: () => () => {},
    },
    undefined,
    recover,
  )(resource, accept, failed);
  const applied = new ReplaySubject<ResourceChangeEvent>();
  const delivery = new ResourceEntityDelivery(
    client.persistResourceEntityChanges,
    (event) => {
      for (const [index, entry] of event.resourceVersions.entries()) {
        const ref = entry.resource ?? event.resources[index];
        if (
          ref?.kind !== "terminal-output" ||
          ref.terminalId !== resource.terminalId
        )
          continue;
        receive({
          revision: entry.revision,
          cursor: entry.cursor,
          replyKind: entry.replyKind,
          viewSnapshot: entry.viewSnapshot,
          epoch: event.epoch,
          reason: event.reason,
        });
      }
      applied.next(event);
    },
  );
  const reads = mocks.syncGet.mock.calls.length;
  const controller = new AbortController();
  const query = new URLSearchParams({
    protocol: String(SYNC_PROTOCOL_VERSION),
    resources: JSON.stringify([resource]),
    tokenRates: "false",
  });
  const response = await fetch(`${origin}/api/sync/stream?${query}`, {
    headers,
    signal: controller.signal,
  });
  expect(response.ok).toBe(true);
  const raw = new ReplaySubject<ResourceChangeEvent>();
  let sessionId = "";
  const pump = (async () => {
    for await (const event of resourceFrames(response, (id) => {
      sessionId = id;
    })) {
      raw.next(structuredClone(event));
      delivery.receive(event);
    }
  })().catch((error) => {
    if (!controller.signal.aborted) throw error;
  });
  const waitApplied = (seq: number, text: string) =>
    firstValueFrom(
      applied.pipe(
        filter((event) =>
          event.resourceVersions.some(
            (entry) =>
              entry.cursor?.seq === seq &&
              entry.viewSnapshot?.terminalOutput?.text === text,
          ),
        ),
        timeout(5000),
      ),
    );
  const waitRaw = (seq: number) =>
    firstValueFrom(
      raw.pipe(
        filter((event) =>
          event.resourceVersions.some((entry) => entry.cursor?.seq === seq),
        ),
        timeout(5000),
      ),
    );
  try {
    await waitApplied(2, "a🙂");
    await vi.waitFor(() =>
      expect(accept).toHaveBeenLastCalledWith(
        expect.objectContaining({ text: "a🙂" }),
        expect.objectContaining({
          replyKind: "snapshot",
          cursor: { workspaceId, seq: 2 },
        }),
      ),
    );
    await write("a🙂中");
    const delta = await waitRaw(3);
    expect(delta.resourceVersions[0]).toMatchObject({
      replyKind: "delta",
      viewSnapshot: { terminalOutput: { text: "中", offset: 3 } },
    });
    await waitApplied(3, "a🙂中");
    await client.persistResourceEntityChanges(
      structuredClone(delta),
      () => true,
    );
    expect(await client.readSubscriptionCursor(resource)).toEqual({
      workspaceId,
      seq: 3,
    });
    const late = await waitRaw(2);
    assert.ok(late.resourceVersions[0]);
    await client.persistResourceEntityChanges(
      {
        ...structuredClone(late),
        resourceVersions: [{ ...late.resourceVersions[0], replyKind: "delta" }],
      },
      () => true,
    );
    expect(await client.readSubscriptionCursor(resource)).toEqual({
      workspaceId,
      seq: 3,
    });
    await client.persistResourceEntityChanges(
      structuredClone(delta),
      () => false,
    );
    expect(await client.readSubscriptionCursor(resource)).toEqual({
      workspaceId,
      seq: 3,
    });
    await write("tail", 20);
    expect((await waitRaw(24)).resourceVersions[0]?.replyKind).toBe("snapshot");
    await waitApplied(24, "tail");
    await write("new");
    await waitApplied(3, "new");
    await write("new", 0, "exited");
    await firstValueFrom(
      applied.pipe(
        filter((event) => {
          assert.ok(event.resourceVersions[0] != null);
          return (
            event.resourceVersions[0].viewSnapshot?.terminalOutput?.status ===
            "exited"
          );
        }),
        timeout(5000),
      ),
    );
    await vi.waitFor(() =>
      expect(accept).toHaveBeenLastCalledWith(
        expect.objectContaining({ text: "new", status: "exited" }),
        expect.objectContaining({
          replyKind: "delta",
          cursor: { workspaceId, seq: 3 },
        }),
      ),
    );
    await write("new" + "x".repeat(SYNC_TERMINAL_STREAM_MAX_CHARS + 1));
    const bounded = await waitRaw(SYNC_TERMINAL_STREAM_MAX_CHARS + 4);
    assert.ok(bounded.resourceVersions[0] != null);
    expect(bounded.resourceVersions[0].replyKind).toBe("snapshot");
    expect(
      Array.from(
        bounded.resourceVersions[0].viewSnapshot!.terminalOutput!.text,
      ),
    ).toHaveLength(SYNC_TERMINAL_STREAM_MAX_CHARS);
    await waitApplied(
      SYNC_TERMINAL_STREAM_MAX_CHARS + 4,
      "x".repeat(SYNC_TERMINAL_STREAM_MAX_CHARS),
    );
    const cursor = await client.readSubscriptionCursor(resource);
    const reconnectController = new AbortController();
    try {
      await write(
        "new" + "x".repeat(SYNC_TERMINAL_STREAM_MAX_CHARS + 1) + "🙂",
      );
      const resume = new URLSearchParams({
        protocol: String(SYNC_PROTOCOL_VERSION),
        reconnect: "true",
        tokenRates: "false",
        resources: JSON.stringify([resource]),
        since: JSON.stringify([{ resource, since: cursor }]),
      });
      const resumed = await fetch(`${origin}/api/sync/stream?${resume}`, {
        headers,
        signal: reconnectController.signal,
      });
      for await (const frame of resourceFrames(resumed)) {
        expect(frame.reason).toBe("reconnect");
        expect(frame.resourceVersions[0]).toMatchObject({
          replyKind: "delta",
          viewSnapshot: { terminalOutput: { text: "🙂" } },
        });
        break;
      }
    } finally {
      reconnectController.abort();
    }
    const other = { kind: "terminal-output", terminalId: "other" } as const;
    const change = (
      gen: number,
      add: typeof resource | typeof other,
      remove: typeof resource | typeof other,
    ) =>
      request("/api/sync/subscriptions", {
        sessionId,
        gen,
        add: [{ resource: add }],
        remove: [remove],
      });
    await change(1, other, resource);
    const otherInitial = await firstValueFrom(
      raw.pipe(
        filter(
          (event) =>
            event.reason === "initial" &&
            event.resources.some(
              (ref) =>
                ref.kind === "terminal-output" && ref.terminalId === "other",
            ),
        ),
        timeout(5000),
      ),
    );
    expect(otherInitial.resourceVersions[0]).toMatchObject({
      replyKind: "snapshot",
      viewSnapshot: { terminalOutput: { text: "other", offset: 5 } },
    });
    await write("changed while away");
    await change(2, resource, other);
    const reopened = await firstValueFrom(
      raw.pipe(
        filter(
          (event) =>
            event.reason === "initial" &&
            event.resourceVersions.some(
              (entry) =>
                entry.viewSnapshot?.terminalOutput?.text ===
                "changed while away",
            ),
        ),
        timeout(5000),
      ),
    );
    assert.ok(reopened.resourceVersions[0] != null);
    expect(reopened.resourceVersions[0].replyKind).toBe("snapshot");
    expect(recover).not.toHaveBeenCalled();
    expect(mocks.syncGet.mock.calls).toHaveLength(reads);
    expect(failed).not.toHaveBeenCalled();
    stop();
    const calls = accept.mock.calls.length;
    receive({
      epoch: "late",
      revision: 999,
      viewSnapshot: {
        terminalOutput: {
          text: "late",
          offset: 4,
          status: "running",
          truncated: false,
        },
      },
    });
    expect(accept).toHaveBeenCalledTimes(calls);
  } finally {
    stop();
    controller.abort();
    await pump;
  }
}, 20000);
