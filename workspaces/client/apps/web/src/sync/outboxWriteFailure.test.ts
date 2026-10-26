import assert from "node:assert/strict";
import { afterEach, expect, it, vi } from "vitest";
import { createRxDatabase, type RxDatabase, type RxCollection } from "rxdb";
import { getRxStorageMemory } from "rxdb/plugins/storage-memory";
import { createOutbox } from "./send";
import {
  createApiScope,
  type PostBody,
  type GetResult,
  type PostResult,
} from "../api";
import { SYNC_IDENTITY_PROTOCOL_VERSION } from "../generated/api";
import { createMutationJournal } from "../servers/mutationIdentity";
import {
  API_SCHEMA_HASH,
  API_SCHEMA_HASH_HEADER,
} from "../generated/apiSchema";
import { SYNC_DOCUMENT_SCHEMA, type SyncDocument } from "./client";

// A full disk rejects one IndexedDB write. Later sends, removals and
// cancellations on the same page must still settle with their exact request.
const WORKSPACE_ID = "0123456789abcdef0123456789abcdef";
const SETTLE_DEADLINE_MS = 2_000;

class MemoryStorage implements Storage {
  private values = new Map<string, string>();
  get length() {
    return this.values.size;
  }
  clear() {
    this.values.clear();
  }
  getItem(key: string) {
    return this.values.get(key) ?? null;
  }
  key(index: number) {
    return [...this.values.keys()][index] ?? null;
  }
  removeItem(key: string) {
    this.values.delete(key);
  }
  setItem(key: string, value: string) {
    this.values.set(key, value);
  }
}

// The memory storage with one rejected incremental write, as from a full disk.
function storageWithOneFailedWrite() {
  const memory = getRxStorageMemory();
  const state = { failNextIncrementalWrite: false };
  const storage: typeof memory = {
    ...memory,
    async createStorageInstance(params) {
      const instance = await memory.createStorageInstance(params);
      const bulkWrite = instance.bulkWrite.bind(instance);
      instance.bulkWrite = (rows, context) => {
        if (context === "incremental-write" && state.failNextIncrementalWrite) {
          state.failNextIncrementalWrite = false;
          return Promise.reject(
            new DOMException("The disk is full.", "QuotaExceededError"),
          );
        }
        return bulkWrite(rows, context);
      };
      return instance;
    },
  };
  return { storage, state };
}

function settles<T>(work: Promise<T>, label: string): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  return Promise.race([
    work,
    new Promise<never>((_, reject) => {
      timer = setTimeout(
        () => reject(new Error(`${label} did not settle`)),
        SETTLE_DEADLINE_MS,
      );
    }),
  ]).finally(() => clearTimeout(timer));
}

const message = (id: string) => ({
  id,
  room: "agent-1",
  text: `message ${id}`,
  assets: [],
  delivery: "current_turn" as const,
});

let database:
  | RxDatabase<{
      outbox: RxCollection<SyncDocument>;
      projections: RxCollection<SyncDocument>;
    }>
  | undefined;

afterEach(async () => {
  vi.unstubAllGlobals();
  await database?.close();
  database = undefined;
});

it("sends, removes and cancels after one outbox write failed", async () => {
  vi.stubGlobal("navigator", { onLine: true });
  const { storage: rxStorage, state } = storageWithOneFailedWrite();
  database = await createRxDatabase<{
    outbox: RxCollection<SyncDocument>;
    projections: RxCollection<SyncDocument>;
  }>({
    name: `outbox-write-failure-${crypto.randomUUID()}`,
    storage: rxStorage,
    multiInstance: false,
  });
  await database.addCollections({
    outbox: { schema: SYNC_DOCUMENT_SCHEMA },
    projections: { schema: SYNC_DOCUMENT_SCHEMA },
  });
  const db = database;
  const storage = new MemoryStorage();
  const syncPost =
    vi.fn<(path: string, body: PostBody<"/api/messages">) => void>();
  const api = createApiScope({
    origin: "http://studio.test",
    localStorage: storage,
    sessionStorage: storage,
    beginMutation: createMutationJournal("test", storage),
    beginActivity: () => () => {},
    fetch: async (request) => {
      const path = new URL(request.url).pathname;
      let value: unknown;
      if (path === "/api/sync/identity")
        value = {
          workspaceId: WORKSPACE_ID,
          syncProtocol: SYNC_IDENTITY_PROTOCOL_VERSION,
        } satisfies GetResult<"/api/sync/identity">;
      else if (path === "/api/session")
        value = { token: "session" } satisfies GetResult<"/api/session">;
      else if (path === "/api/messages") {
        // This API instance wrote the request using the generated message contract.
        const body: PostBody<"/api/messages"> = await request.json();
        syncPost(path, body);
        value = {
          id: body.id,
          status: "delivered",
        } satisfies PostResult<"/api/messages">;
      } else throw new Error("Unexpected route: " + path);
      return Response.json(value, {
        headers: { [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH },
      });
    },
  });
  const outbox = createOutbox({
    api,
    syncDatabase: async () => ({
      db,
      workspaceId: WORKSPACE_ID,
      verifyWorkspace: async () => {},
    }),
    storage,
  });

  // An earlier delivered message is acknowledged while the disk is full.
  await outbox.durableSend(message("earlier"));
  state.failNextIncrementalWrite = true;
  await expect(
    settles(outbox.acknowledgeOutbox(["earlier"]), "acknowledgement"),
  ).rejects.toThrow("The disk is full.");

  // Space is back. A new message reaches the server once, with its own id.
  const sent = await settles(
    outbox.durableSend(message("after-disk-full")),
    "send",
  );
  expect(sent).toMatchObject({ id: "after-disk-full", status: "delivered" });
  expect(
    syncPost.mock.calls.filter(([, body]) => body.id === "after-disk-full"),
  ).toHaveLength(1);

  // A message that never left the device can still be removed or cancelled.
  assert.ok(db.outbox != null);
  for (const id of ["removed", "cancelled"])
    await db.outbox.insert({
      id,
      seq: 0,
      payload: JSON.stringify({
        body: message(id),
        status: "queued",
        attempted: false,
        created: Date.now(),
      }),
    });
  await expect(
    settles(outbox.stopRemovedMessage("removed"), "removal"),
  ).resolves.toBe(true);
  await settles(outbox.changeOutbox("cancelled", "cancel"), "cancellation");
  const cancelled = await db.outbox.findOne("cancelled").exec();
  expect(JSON.parse(cancelled!.payload).status).toBe("cancelled");
  expect(
    syncPost.mock.calls.some(
      ([, body]) =>
        body !== null &&
        typeof body === "object" &&
        "id" in body &&
        ["removed", "cancelled"].includes(body.id),
    ),
  ).toBe(false);
});
