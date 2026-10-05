import assert from "node:assert/strict";
import { afterEach, it, vi } from "vitest";
import { ApiSchemaMismatchError } from "../api";
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

afterEach(() => {
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
  let record = {
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
  const doc = {
    id: record.id,
    getLatest: () => record,
    incrementalModify: async (
      modify: (value: typeof record) => typeof record,
    ) => (record = modify(record)),
  };

  const result = await deliver(doc);
  const stored = JSON.parse(record.payload);
  assert.equal(result.queued, true);
  assert.equal(stored.status, "queued");
  assert.equal(stored.body.id, "stable-message-id");
  assert.equal(stored.attempted, true);
  assert.equal(syncPost.mock.calls.length, 1);
});
