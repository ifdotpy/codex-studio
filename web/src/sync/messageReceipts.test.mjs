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
    assert.deepEqual(
      checkedMessageReceipts(
        { agent: "chat", items: [{ id: "sso", status }] },
        "chat",
        ["sso"],
      ),
      [{ id: "sso", status }],
    );
  assert.throws(
    () =>
      checkedMessageReceipts({ agent: "foreign", items: [delivered] }, "chat", [
        "sso",
      ]),
    { message: "The delivery receipts belong to another chat." },
  );
  for (const value of [
    { agent: "chat", items: [{ id: "other", status: "delivered" }] },
    { agent: "chat", items: [delivered, delivered] },
  ])
    assert.throws(() => checkedMessageReceipts(value, "chat", ["sso"]), {
      message: "The delivery receipts do not match the requested messages.",
    });
  assert.throws(
    () =>
      checkedMessageReceipts(
        {
          agent: "chat",
          items: [
            { id: "first", status: "pending" },
            { id: "unexpected", status: "delivered" },
          ],
        },
        "chat",
        ["first"],
      ),
    { message: "The delivery receipts do not match the requested messages." },
  );
  assert.deepEqual(
    checkedMessageReceipts(
      {
        agent: "chat",
        items: [
          { id: "first", status: "pending" },
          { id: "second", status: "delivered" },
        ],
      },
      "chat",
      ["first", "second"],
    ),
    [
      { id: "first", status: "pending" },
      { id: "second", status: "delivered" },
    ],
  );
  assert.equal(
    receiptOutgoing(accepted, delivered).receipt.status,
    "delivered",
  );
  assert.deepEqual(receiptOutgoing(accepted, delivered), {
    ...accepted,
    status: "accepted",
    receipt: delivered,
    error: undefined,
  });
  assert.equal(
    receiptOutgoing({ ...accepted, status: "uncertain" }, delivered).status,
    "accepted",
    "A receipt resolves an uncertain send using the same message identity",
  );
  assert.equal(
    receiptOutgoing(accepted, {
      id: "sso",
      status: "failed",
      error: "server rejected the message",
    }).error,
    "server rejected the message",
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
  const newerDelivered = { id: "sso", status: "delivered" };
  assert.equal(
    latestMessageReceipt(delivered, newerDelivered),
    newerDelivered,
    "A later delivery receipt replaces the earlier confirmation",
  );
  const firstPending = { id: "sso", status: "pending" };
  assert.equal(
    latestMessageReceipt(undefined, firstPending),
    firstPending,
    "The first receipt is retained",
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
  const fallbackId = {
    ...pending,
    id: "chat:fallback",
    clientMessageId: undefined,
  };
  assert.deepEqual(
    receiptTranscript(
      [fallbackId],
      "chat",
      new Map([
        ["fallback", { id: "fallback", status: "failed", error: "no" }],
      ]),
    )[0],
    {
      ...fallbackId,
      deliveryStatus: "failed",
      pending: false,
      deliveryError: "no",
    },
  );
  const nonPrefixedId = {
    ...pending,
    id: "legacy-id",
    clientMessageId: undefined,
  };
  assert.deepEqual(
    receiptTranscript(
      [nonPrefixedId],
      "chat",
      new Map([["legacy-id", { id: "legacy-id", status: "pending" }]]),
    )[0],
    {
      ...nonPrefixedId,
      deliveryStatus: "pending",
      pending: true,
      deliveryError: undefined,
    },
  );
  const roomPrefixWithoutDelimiter = {
    ...pending,
    id: "chatty",
    clientMessageId: undefined,
  };
  assert.equal(
    receiptTranscript(
      [roomPrefixWithoutDelimiter],
      "chat",
      new Map([["chatty", { id: "chatty", status: "delivered" }]]),
    )[0].deliveryStatus,
    "delivered",
    "Only the exact room prefix followed by a colon is stripped",
  );
  const preferredClientId = {
    ...pending,
    id: "chat:other",
    clientMessageId: "sso",
  };
  assert.equal(
    receiptTranscript(
      [preferredClientId],
      "chat",
      new Map([
        ["sso", { id: "sso", status: "failed" }],
        ["other", { id: "other", status: "cancelled" }],
      ]),
    )[0].deliveryStatus,
    "failed",
    "The stable client message ID takes precedence over the transcript ID",
  );
  const assistant = { ...pending, role: "assistant" };
  assert.equal(
    receiptTranscript([assistant], "chat", new Map([["sso", delivered]]))[0],
    assistant,
    "Receipts update user messages only",
  );
  const alreadyDelivered = {
    ...pending,
    deliveryStatus: "delivered",
    pending: false,
    deliveryError: "stale",
  };
  assert.deepEqual(
    receiptTranscript(
      [alreadyDelivered],
      "chat",
      new Map([["sso", { id: "sso", status: "pending", error: "old" }]]),
    )[0],
    {
      ...alreadyDelivered,
      deliveryStatus: "delivered",
      pending: false,
      deliveryError: undefined,
    },
  );
  for (const status of ["uncertain", "failed", "cancelled"])
    assert.equal(
      receiptOutgoing(accepted, { id: "sso", status }).status,
      status,
    );
  console.log("PASS exact identity, ambiguity, and monotonic delivery");
});
