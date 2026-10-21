import { afterEach, expect, it, vi } from "vitest";
import { createRxDatabase, type RxDatabase, type RxStorage } from "rxdb";
import { getRxStorageMemory } from "rxdb/plugins/storage-memory";
import { createOutbox } from "./send";

// A full disk rejects one IndexedDB write. Later sends, removals and
// cancellations on the same page must still settle with their exact request.
const WORKSPACE_ID = "0123456789abcdef0123456789abcdef";
const SETTLE_DEADLINE_MS = 2_000;

const schema = {
  version: 0,
  primaryKey: "id",
  type: "object",
  properties: {
    id: { type: "string", maxLength: 300 },
    payload: { type: "string" },
    seq: { type: "integer", minimum: 0, maximum: 1e15, multipleOf: 1 },
  },
  required: ["id", "payload", "seq"],
} as const;

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
  const storage: RxStorage<any, any> = {
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

let database: RxDatabase | undefined;

afterEach(async () => {
  vi.unstubAllGlobals();
  await database?.close();
  database = undefined;
});

it("sends, removes and cancels after one outbox write failed", async () => {
  vi.stubGlobal("navigator", { onLine: true });
  const { storage, state } = storageWithOneFailedWrite();
  database = await createRxDatabase({
    name: `outbox-write-failure-${crypto.randomUUID()}`,
    storage,
    multiInstance: false,
  });
  await database.addCollections({ outbox: { schema } });
  const db = database;
  const syncPost = vi.fn(async (_path: string, body: { id: string }) => ({
    id: body.id,
    status: "delivered",
  }));
  const outbox = createOutbox({
    api: {
      isApiSchemaMismatch: () => false,
      onApiSchemaMismatch: () => () => {},
      syncGet: (async () => ({ workspaceId: WORKSPACE_ID })) as any,
      syncPost: syncPost as any,
      refreshSession: (async () => ({ token: "session" })) as any,
    },
    syncDatabase: (async () => ({
      db,
      workspaceId: WORKSPACE_ID,
      verifyWorkspace: async () => {},
    })) as any,
    storage: new MemoryStorage(),
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
    syncPost.mock.calls.some(([, body]) =>
      ["removed", "cancelled"].includes(body.id),
    ),
  ).toBe(false);
});
