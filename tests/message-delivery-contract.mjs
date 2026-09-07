import assert from "node:assert/strict";
import { outgoingTranscript } from "../web/src/components/messageDelivery.ts";

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
console.log("Message delivery continuity: PASS");
