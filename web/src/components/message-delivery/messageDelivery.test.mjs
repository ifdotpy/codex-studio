import assert from "node:assert/strict";
import {
  outgoingTranscript,
  deliveryLabel,
  explicitQueue,
  dispatchedMessage,
  mergeQueueOrder,
  messageRenderKey,
  receiptMessage,
  applySendingOverlay,
} from "./messageDelivery.ts";
import { it } from "vitest";

const receipt = {
  id: "one",
  body: { room: "lead", text: "Same text" },
  created: 1,
  status: "accepted",
  receipt: { status: "queued" },
};
const pending = {
  id: "lead:one",
  role: "user",
  text: "Same text",
  pending: true,
};

it("does not restore a sending action over a materialized removed copy", () => {
  const restored = {
    id: "lead:removed",
    role: "user",
    text: "Removed while sending",
    materialized: true,
    pending: false,
    deliveryStatus: "uncertain",
  };
  assert.deepEqual(applySendingOverlay(restored, true), restored);
  assert.deepEqual(
    applySendingOverlay({ ...restored, materialized: false }, true),
    {
      ...restored,
      materialized: false,
      pending: true,
      deliveryStatus: "sending",
    },
  );
  assert.equal(applySendingOverlay(restored, true).deliveryStatus, "uncertain");
});

it("matches transcript rows by receipt identity and retains edited text and attachments", () => {
  assert.equal(
    outgoingTranscript(
      [pending],
      [{ ...receipt, attachments: [{ id: "asset-one" }] }],
    ).items[0].assets[0].id,
    "asset-one",
  );
  assert.equal(outgoingTranscript([], [receipt]).items[0].text, "Same text");
  assert.deepEqual(outgoingTranscript([pending], [receipt]).observed, []);
  assert.equal(outgoingTranscript([pending], [receipt]).items.length, 1);
  // An older server omits a reserved queue row before it materializes the input.
  assert.equal(outgoingTranscript([], [receipt]).items.length, 1);
  assert.deepEqual(
    outgoingTranscript(
      [{ ...pending, pending: false, materialized: false }],
      [receipt],
    ).observed,
    [],
  );
  assert.deepEqual(
    outgoingTranscript(
      [{ ...pending, pending: false, materialized: true }],
      [receipt],
    ).observed,
    ["one"],
  );
  // Equal text does not identify a message. Both independent sends remain visible.
  assert.equal(
    outgoingTranscript([pending], [receipt, { ...receipt, id: "two" }]).items
      .length,
    2,
  );
  const batch = [
    { id: "lead:one", clientMessageId: "one", role: "user", text: "Same text" },
    { id: "lead:two", clientMessageId: "two", role: "user", text: "Same text" },
  ];
  assert.deepEqual(
    outgoingTranscript(batch, [receipt, { ...receipt, id: "two" }]).observed,
    ["one", "two"],
  );
  const edited = { ...receipt, displayText: "Edited queue text" };
  assert.equal(
    outgoingTranscript([], [edited]).items[0].text,
    "Edited queue text",
  );
  assert.equal(edited.body.text, "Same text");
  assert.deepEqual(
    outgoingTranscript(
      [{ id: "lead:one:0", role: "user", text: "Same text" }],
      [receipt],
    ).observed,
    ["one"],
  );
  assert.deepEqual(
    outgoingTranscript(
      [{ id: "lead:one:1", role: "user", text: "Same text" }],
      [receipt],
    ).observed,
    [],
  );
});

it("retires delivered local cards outside the page only with exact saved input proof", () => {
  const latest = [{ id: "lead:new", role: "assistant", text: "Latest reply" }];
  const delivered = {
    ...receipt,
    displayPending: true,
    receipt: { id: "one", status: "delivered", materialized: true },
  };
  const result = outgoingTranscript(latest, [delivered]);
  assert.deepEqual(result.items, latest);
  assert.deepEqual(result.observed, ["one"]);
  for (const materialized of [false, undefined]) {
    const unproven = {
      ...delivered,
      receipt: { ...delivered.receipt, materialized },
    };
    assert.equal(outgoingTranscript(latest, [unproven]).items.length, 2);
    assert.deepEqual(outgoingTranscript(latest, [unproven]).observed, []);
  }
  for (const status of ["queued", "sending", "uncertain", "failed", "paused"]) {
    assert.equal(
      outgoingTranscript(latest, [{ ...delivered, status }]).items.length,
      2,
    );
  }
  for (const status of ["pending", "reserved", "dispatching", "uncertain"]) {
    assert.equal(
      outgoingTranscript(latest, [
        { ...delivered, receipt: { ...delivered.receipt, status } },
      ]).items.length,
      2,
    );
  }
  const original = {
    id: "lead:batch-second",
    clientMessageId: "one",
    role: "user",
    text: "Same text",
    materialized: true,
  };
  assert.deepEqual(outgoingTranscript([original], [delivered]).items, [
    original,
  ]);
  assert.deepEqual(outgoingTranscript([original], [delivered]).observed, [
    "one",
  ]);
});

it("keeps explicit delivery modes queued until a message is dispatched", () => {
  for (const delivery of ["after_tool", "steer"]) {
    const local = { ...receipt, body: { ...receipt.body, delivery } };
    for (const source of [[], [pending]]) {
      const message = outgoingTranscript(source, [local]).items[0];
      assert.equal(message.requestedDelivery, delivery);
      assert.equal(explicitQueue(message), true);
      assert.equal(deliveryLabel(message), "Queued");
      assert.equal(dispatchedMessage(message), false);
    }
  }
  assert.equal(explicitQueue({}), false);
  assert.equal(explicitQueue({ requestedDelivery: "queue" }), false);
  assert.equal(explicitQueue({ deliveryStatus: "queued" }), true);
  assert.equal(explicitQueue({ status: "pending" }), true);
  assert.equal(explicitQueue({ pending: true }), true);
  assert.equal(
    explicitQueue({
      localDelivery: true,
      deliveryStatus: "queued",
      pending: true,
    }),
    false,
  );
  assert.equal(
    explicitQueue({ localDelivery: true, deliveryStatus: "pending" }),
    true,
  );
  assert.equal(explicitQueue({ deliveryStatus: "", status: "queued" }), true);
  assert.equal(
    dispatchedMessage({ ...pending, pending: false, materialized: true }),
    true,
  );
  for (const deliveryStatus of ["reserved", "dispatching", "delivered", "sent"])
    assert.equal(dispatchedMessage({ ...pending, deliveryStatus }), true);
  assert.equal(dispatchedMessage({ ...pending, materialized: true }), true);
  assert.equal(dispatchedMessage({ ...pending, pending: true }), false);
  assert.equal(dispatchedMessage({ ...pending, materialized: false }), false);
  assert.equal(
    dispatchedMessage({
      ...pending,
      localDelivery: true,
      materialized: undefined,
    }),
    false,
  );
  assert.equal(
    dispatchedMessage({
      ...pending,
      localDelivery: true,
      deliveryStatus: "sent",
    }),
    true,
  );
  assert.equal(
    dispatchedMessage({ ...pending, pending: false, materialized: undefined }),
    true,
  );
  assert.equal(
    dispatchedMessage({ ...pending, pending: false, materialized: true }),
    true,
  );
  assert.equal(
    dispatchedMessage({ ...pending, pending: false, materialized: false }),
    false,
  );
  assert.equal(
    dispatchedMessage({
      ...pending,
      pending: false,
      localDelivery: true,
      deliveryStatus: "accepted",
    }),
    false,
  );
  assert.equal(
    outgoingTranscript([], [{ ...receipt, receipt: { status: "pending" } }])
      .items[0].deliveryStatus,
    "pending",
  );
});

it("applies queue permutations and rejects duplicate or incomplete orders", () => {
  assert.deepEqual(
    mergeQueueOrder(["normal", "a", "hidden", "b"], ["a", "b"], ["b", "a"]),
    ["normal", "b", "hidden", "a"],
  );
  assert.throws(() =>
    mergeQueueOrder(["normal", "a", "b"], ["a", "b"], ["a", "a"]),
  );
  assert.throws(() => mergeQueueOrder(["normal", "a", "b"], ["a", "b"], ["a"]));
  assert.throws(() =>
    mergeQueueOrder(["normal", "a", "b"], ["a", "b"], ["a", "missing"]),
  );
  assert.throws(() =>
    mergeQueueOrder(["normal", "a", "b"], ["a", "missing"], ["a", "missing"]),
  );
  assert.throws(() => mergeQueueOrder(["normal", "a"], ["a", "a"], ["a"]), {
    message: "The queue changed. Reload before reordering.",
  });
  assert.throws(
    () => mergeQueueOrder(["normal", "a", "b"], ["a", "b"], ["a", "missing"]),
    { message: "The queue changed. Reload before reordering." },
  );
  assert.deepEqual(
    mergeQueueOrder(["normal", "a", "b"], ["a", "b"], ["b", "a"]),
    ["normal", "b", "a"],
  );
});

it("uses stable render and receipt identities rather than message text", () => {
  assert.equal(
    messageRenderKey({
      id: "server-row",
      role: "user",
      clientMessageId: "send-1",
    }),
    "client:send-1",
  );
  assert.equal(
    messageRenderKey({
      id: "assistant-row",
      role: "assistant",
      clientMessageId: "send-1",
    }),
    "assistant-row",
  );
  assert.equal(messageRenderKey({ id: "user-row", role: "user" }), "user-row");

  assert.equal(
    receiptMessage({ id: "server-row", clientMessageId: "one" }, receipt),
    true,
  );
  assert.equal(receiptMessage({ id: "one" }, receipt), true);
  assert.equal(receiptMessage({ id: "lead:one" }, receipt), true);
  assert.equal(receiptMessage({ id: "lead:one:0" }, receipt), true);
  assert.equal(receiptMessage({ id: "lead:one:1" }, receipt), false);
  assert.equal(
    receiptMessage(
      { id: "other:one:0", clientMessageId: "different" },
      receipt,
    ),
    false,
  );
});

it("updates a matching receipt without replacing existing transcript content", () => {
  const durable = {
    id: "lead:one",
    clientMessageId: "one",
    role: "user",
    text: "server text",
    assets: [{ id: "server-asset" }],
    pending: false,
    materialized: true,
    requestedDelivery: "after_turn",
    deliveryStatus: "sent",
  };
  const entry = {
    ...receipt,
    body: { ...receipt.body, delivery: "after_tool" },
    attachments: [{ id: "outbox-asset" }],
    status: "failed",
    error: "late receipt error",
  };
  const result = outgoingTranscript([durable], [entry]);
  assert.deepEqual(result.observed, ["one"]);
  assert.equal(result.items.length, 1);
  assert.equal(result.items[0].text, "server text");
  assert.deepEqual(result.items[0].assets, [{ id: "server-asset" }]);
  assert.equal(result.items[0].requestedDelivery, "after_turn");
  assert.equal(result.items[0].deliveryStatus, "sent");
  assert.equal(result.items[0].deliveryError, undefined);
  assert.equal(result.items[0], durable);

  const emptyDurable = {
    ...durable,
    id: "lead:two",
    clientMessageId: "two",
    assets: undefined,
  };
  const noAttachmentChange = outgoingTranscript(
    [emptyDurable],
    [{ ...receipt, id: "two", attachments: [{ id: "outbox-only" }] }],
  );
  assert.equal(noAttachmentChange.items[0].assets, undefined);
  assert.equal(noAttachmentChange.items[0], emptyDurable);
});

it("restores attachments and requested delivery to a local transcript row", () => {
  const materializing = {
    id: "lead:one",
    clientMessageId: "one",
    role: "user",
    text: "Same text",
    pending: false,
    materialized: false,
  };
  const result = outgoingTranscript(
    [materializing],
    [
      {
        ...receipt,
        body: { ...receipt.body, delivery: "after_tool" },
        attachments: [{ id: "asset-one" }],
      },
    ],
  );
  assert.deepEqual(result.items[0].assets, [{ id: "asset-one" }]);
  assert.equal(result.items[0].requestedDelivery, "after_tool");
  assert.deepEqual(result.observed, []);

  const existingDelivery = {
    ...materializing,
    requestedDelivery: "after_turn",
  };
  const unchangedDelivery = outgoingTranscript(
    [existingDelivery],
    [{ ...receipt, body: { ...receipt.body, delivery: "after_tool" } }],
  );
  assert.equal(unchangedDelivery.items[0].requestedDelivery, "after_turn");
  assert.equal(unchangedDelivery.items[0], existingDelivery);
});

it("shows failed and uncertain receipts until the transcript confirms their outcome", () => {
  for (const status of ["failed", "uncertain"]) {
    const present = {
      id: "lead:one",
      clientMessageId: "one",
      role: "user",
      text: "Same text",
      pending: true,
    };
    const result = outgoingTranscript(
      [present],
      [{ ...receipt, status, error: `${status} detail` }],
    );
    assert.equal(result.items[0].deliveryStatus, status);
    assert.equal(result.items[0].deliveryError, `${status} detail`);
    assert.deepEqual(result.observed, []);

    const alreadyLabeled = { ...present, deliveryStatus: "sending" };
    const preserved = outgoingTranscript(
      [alreadyLabeled],
      [{ ...receipt, status, error: "must not overwrite" }],
    );
    assert.equal(preserved.items[0].deliveryStatus, "sending");
    assert.equal(preserved.items[0].deliveryError, undefined);

    const materializedWithoutLabel = {
      ...present,
      pending: false,
      materialized: true,
      deliveryStatus: undefined,
    };
    const notYetObserved = outgoingTranscript(
      [materializedWithoutLabel],
      [{ ...receipt, status, error: `${status} detail` }],
    );
    assert.deepEqual(notYetObserved.observed, []);
    assert.equal(notYetObserved.items[0].deliveryStatus, status);
  }
});

it("creates local receipt rows with display content and accepted queue status", () => {
  const missingReceipt = {
    ...receipt,
    status: "accepted",
    receipt: undefined,
    displayText: "Edited draft",
    attachments: undefined,
    created: 1234,
    error: "receipt unavailable",
    body: { room: "lead", text: "Original text", delivery: "after_turn" },
  };
  const row = outgoingTranscript([], [missingReceipt]).items[0];
  assert.equal(row.id, "lead:one");
  assert.equal(row.clientMessageId, "one");
  assert.equal(row.role, "user");
  assert.equal(row.title, "You");
  assert.equal(row.text, "Edited draft");
  assert.deepEqual(row.assets, []);
  assert.equal(row.at, 1.234);
  assert.equal(row.deliveryStatus, "accepted");
  assert.equal(row.deliveryError, "receipt unavailable");
  assert.equal(row.localDelivery, true);
  assert.equal(row.requestedDelivery, "after_turn");

  const queued = outgoingTranscript(
    [],
    [{ ...receipt, receipt: { status: "reserved" } }],
  ).items[0];
  assert.equal(queued.deliveryStatus, "accepted");
  const waiting = outgoingTranscript(
    [],
    [{ ...receipt, receipt: { status: "pending" } }],
  ).items[0];
  assert.equal(waiting.deliveryStatus, "pending");
  const missing = outgoingTranscript([], [{ ...receipt, receipt: undefined }])
    .items[0];
  assert.equal(missing.deliveryStatus, "accepted");
  const nonAccepted = outgoingTranscript(
    [],
    [{ ...receipt, status: "queued", receipt: { status: "pending" } }],
  ).items[0];
  assert.equal(nonAccepted.deliveryStatus, "queued");
});

it("labels every delivery state used by the chat and hides unknown states", () => {
  const labels = {
    sending: "Sending…",
    reserved: "Starting…",
    dispatching: "Sending…",
    pending: "Queued",
    queued: "Waiting to send",
    paused: "Retries paused",
    uncertain: "Delivery unconfirmed",
    failed: "Not sent",
    accepted: "Sent",
    cancelled: "Cancelled",
  };
  for (const [status, label] of Object.entries(labels))
    assert.equal(deliveryLabel({ deliveryStatus: status }), label);
  assert.equal(
    deliveryLabel({ deliveryStatus: "sending", pending: true }),
    "Waiting for agent",
  );
  assert.equal(deliveryLabel({ pending: true }), "Queued");
  assert.equal(deliveryLabel({}), "");
  assert.equal(deliveryLabel({ deliveryStatus: "future-status" }), "");
});
