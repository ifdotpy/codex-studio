import assert from "node:assert/strict";
import {
  checkedMessageReceipts,
  receiptOutgoing,
  receiptTranscript,
  latestMessageReceipt,
} from "./messageReceipts.ts";

import { it } from "vitest";

it("joins outgoing messages to accepted and delivered receipts by identity", () => {
  const delivered = { id: "sso", status: "delivered" };
  const accepted = {
    id: "sso",
    body: { room: "chat", text: "sso done" },
    created: 1,
    status: "accepted",
    receipt: { id: "sso", status: "queued" },
  };
  assert.deepEqual(
    checkedMessageReceipts({ agent: "chat", items: [delivered] }, "chat", [
      "sso",
    ]),
    [delivered],
  );
  for (const value of [
    { agent: "foreign", items: [delivered] },
    { agent: "chat", items: [{ id: "other", status: "delivered" }] },
    { agent: "chat", items: [delivered, delivered] },
    { agent: "chat", items: [{ id: "sso", status: "unknown" }] },
  ])
    assert.throws(() => checkedMessageReceipts(value, "chat", ["sso"]));
  assert.equal(
    receiptOutgoing(accepted, delivered).receipt.status,
    "delivered",
  );
  assert.equal(
    receiptOutgoing({ ...accepted, status: "queued" }, delivered).status,
    "queued",
    "An unacknowledged HTTP outcome is not rewritten",
  );
  assert.equal(
    latestMessageReceipt(delivered, { id: "sso", status: "pending" }),
    delivered,
  );
  const pending = {
    id: "chat:sso",
    clientMessageId: "sso",
    role: "user",
    text: "sso done",
    pending: true,
    materialized: false,
  };
  const shown = receiptTranscript(
    [pending],
    "chat",
    new Map([["sso", delivered]]),
  )[0];
  assert.equal(shown.pending, false);
  assert.equal(
    shown.materialized,
    false,
    "A receipt does not fabricate a transcript item",
  );
  assert.equal(
    receiptTranscript([pending], "chat", new Map())[0],
    pending,
    "An omitted ID does not prove success",
  );
  for (const status of ["uncertain", "failed", "cancelled"])
    assert.equal(
      receiptOutgoing(accepted, { id: "sso", status }).status,
      status,
    );
  console.log("PASS exact identity, ambiguity, and monotonic delivery");
});
