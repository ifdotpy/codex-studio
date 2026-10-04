import assert from "node:assert/strict";
import { it } from "vitest";
import { matchesMessageDelivery } from "./send.ts";

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
