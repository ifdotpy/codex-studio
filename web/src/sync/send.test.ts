import assert from "node:assert/strict";
import { it } from "vitest";
import {
  intentionResult,
  matchesMessageDelivery,
  retainQueuedSchemaMismatch,
} from "./send.ts";

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
