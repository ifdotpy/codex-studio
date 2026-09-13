import assert from "node:assert/strict";
import {
  outgoingTranscript,
  deliveryLabel,
  explicitQueue,
  dispatchedMessage,
  mergeQueueOrder,
} from "../web/src/components/messageDelivery.ts";

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
for (const delivery of ["after_tool", "steer"]) {
  const local = { ...receipt, body: { ...receipt.body, delivery } };
  for (const source of [[], [pending]]) {
    const message = outgoingTranscript(source, [local]).items[0];
    assert.equal(message.requestedDelivery, delivery);
    assert.equal(explicitQueue(message), false);
    assert.equal(deliveryLabel(message), "Sending…");
    assert.equal(dispatchedMessage(message), false);
  }
}
assert.equal(explicitQueue({}), true);
assert.equal(explicitQueue({ requestedDelivery: "queue" }), true);
assert.equal(
  dispatchedMessage({ ...pending, pending: false, materialized: true }),
  true,
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
assert.deepEqual(
  mergeQueueOrder(["normal", "a", "hidden", "b"], ["a", "b"], ["b", "a"]),
  ["normal", "b", "hidden", "a"],
);
assert.throws(() =>
  mergeQueueOrder(["normal", "a", "b"], ["a", "b"], ["a", "a"]),
);
assert.throws(() => mergeQueueOrder(["normal", "a", "b"], ["a", "b"], ["a"]));
console.log("Message delivery continuity and queue intent: PASS");
