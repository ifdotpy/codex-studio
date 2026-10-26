import { createRxDatabase, type RxCollection, type RxDatabase } from "rxdb";
import { getRxStorageMemory } from "rxdb/plugins/storage-memory";
import { SYNC_DOCUMENT_SCHEMA, type SyncDocument } from "./client";
import assert from "node:assert/strict";
import { afterEach, it, vi } from "vitest";
import { ApiError, ApiSchemaMismatchError } from "../api";
const { syncDatabase, syncGet, refreshSession, syncPost } = vi.hoisted(() => ({
  syncDatabase: vi.fn(),
  syncGet: vi.fn(),
  refreshSession: vi.fn(),
  syncPost: vi.fn(),
}));
vi.mock("../api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../api")>()),
  syncGet,
  refreshSession,
  syncPost,
}));
vi.mock("./client", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./client")>()),
  syncDatabase,
}));
import {
  deliver,
  intentionResult,
  matchesMessageDelivery,
  retainQueuedSchemaMismatch,
} from "./send.ts";

let testDatabase:
  | RxDatabase<{ outbox: RxCollection<SyncDocument> }>
  | undefined;

afterEach(async () => {
  await testDatabase?.remove();
  testDatabase = undefined;
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  syncDatabase.mockReset();
  syncGet.mockReset();
  refreshSession.mockReset();
  syncPost.mockReset();
});

it("accepts only receipts matching the immutable message identity", () => {
  for (const status of [
    "queued",
    "pending",
    "reserved",
    "dispatching",
    "delivered",
    "accepted",
    "sent",
    "uncertain",
    "failed",
    "cancelled",
    "stored_only",
  ])
    assert.equal(
      matchesMessageDelivery({ id: "message-1", status }, "message-1"),
      true,
    );

  assert.equal(
    matchesMessageDelivery(
      { id: "message-2", status: "delivered" },
      "message-1",
    ),
    false,
  );
  assert.equal(
    matchesMessageDelivery({ id: "message-1", status: "unknown" }, "message-1"),
    false,
  );
  assert.equal(matchesMessageDelivery(null, "message-1"), false);
});

it("distinguishes durable local queue state from a server delivery receipt", () => {
  assert.deepEqual(
    intentionResult({
      body: { id: "message-1", room: "chat", text: "hello", delivery: "queue" },
      status: "queued",
      created: 1,
    }),
    {
      kind: "local",
      id: "message-1",
      queued: true,
      status: "queued",
      error: undefined,
    },
  );
  assert.deepEqual(
    intentionResult({
      body: { id: "message-1", room: "chat", text: "hello", delivery: "queue" },
      status: "accepted",
      created: 1,
      receipt: {
        id: "message-1",
        status: "delivered",
        deliveries: { peer: "delivered" },
      },
    }),
    {
      kind: "server",
      id: "message-1",
      status: "delivered",
      deliveries: { peer: "delivered" },
      queued: false,
    },
  );
});

it("keeps a schema-blocked outbox request queued with its original identity and body", () => {
  const stored = {
    body: {
      id: "stable-message-id",
      room: "chat",
      text: "hello",
      delivery: "after_tool" as const,
    },
    status: "queued" as const,
    attempted: true,
    created: 1,
  };
  const blocked = retainQueuedSchemaMismatch(
    stored,
    new Error("Reload Studio"),
  );
  assert.equal(blocked.status, "queued");
  assert.equal(blocked.body.id, stored.body.id);
  assert.deepEqual(blocked.body, stored.body);
  assert.equal(blocked.attempted, true);
});

it("deliver keeps a marked-426 message queued without changing its identity", async () => {
  vi.stubGlobal("navigator", { onLine: true });
  vi.stubGlobal("localStorage", { getItem: () => null });
  syncDatabase.mockResolvedValue({ workspaceId: "workspace-one" });
  syncGet.mockResolvedValue({ workspaceId: "workspace-one" });
  refreshSession.mockResolvedValue({ token: "session-token" });
  syncPost.mockRejectedValue(new ApiSchemaMismatchError(true));
  const record = {
    id: "stable-message-id",
    payload: JSON.stringify({
      body: {
        id: "stable-message-id",
        room: "chat",
        text: "hello",
        delivery: "queue",
      },
      status: "queued",
      attempted: false,
      created: 1,
    }),
  };
  testDatabase = await createRxDatabase<{ outbox: RxCollection<SyncDocument> }>(
    {
      name: "send-" + crypto.randomUUID(),
      storage: getRxStorageMemory(),
      multiInstance: false,
    },
  );
  const { outbox } = await testDatabase.addCollections({
    outbox: { schema: SYNC_DOCUMENT_SCHEMA },
  });
  const doc = await outbox.insert({ ...record, seq: 0 });

  const result = await deliver(doc);
  const stored = JSON.parse(doc.getLatest().payload);
  assert.equal(result.queued, true);
  assert.equal(stored.status, "queued");
  assert.equal(stored.body.id, "stable-message-id");
  assert.equal(stored.attempted, true);
  assert.equal(syncPost.mock.calls.length, 1);
});

it("deliver preserves the existing failure behavior for an unmarked 426", async () => {
  vi.stubGlobal("navigator", { onLine: true });
  vi.stubGlobal("localStorage", { getItem: () => null });
  syncDatabase.mockResolvedValue({ workspaceId: "workspace-one" });
  syncGet.mockResolvedValue({ workspaceId: "workspace-one" });
  refreshSession.mockResolvedValue({ token: "session-token" });
  syncPost.mockRejectedValue(new ApiError("Unsupported protocol", 426));
  const record = {
    id: "stable-message-id",
    payload: JSON.stringify({
      body: {
        id: "stable-message-id",
        room: "chat",
        text: "hello",
        delivery: "queue",
      },
      status: "queued",
      attempted: false,
      created: 1,
    }),
  };
  testDatabase = await createRxDatabase<{ outbox: RxCollection<SyncDocument> }>(
    {
      name: "send-" + crypto.randomUUID(),
      storage: getRxStorageMemory(),
      multiInstance: false,
    },
  );
  const { outbox } = await testDatabase.addCollections({
    outbox: { schema: SYNC_DOCUMENT_SCHEMA },
  });
  const doc = await outbox.insert({ ...record, seq: 0 });
  await assert.rejects(deliver(doc), { status: 426 });
  assert.equal(JSON.parse(doc.getLatest().payload).status, "failed");
  assert.equal(
    JSON.parse(doc.getLatest().payload).body.id,
    "stable-message-id",
  );
});
