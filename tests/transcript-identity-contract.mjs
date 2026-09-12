import assert from "node:assert/strict";
import { retainTranscriptItems } from "../web/src/transcriptIdentity.ts";

const previous = [
  {
    id: "user",
    role: "user",
    text: "Saved input",
    inputs: [{ id: "request", pending: true }],
    assets: [{ id: "image" }],
  },
  {
    id: "answer",
    role: "assistant",
    text: "First answer",
    streaming: true,
    turnStatus: "running",
  },
  {
    id: "tool",
    role: "tool",
    text: "{}",
    nativeError: { message: "Failed", metadata: { retry: false } },
  },
];
assert.equal(
  retainTranscriptItems(previous, JSON.parse(JSON.stringify(previous))),
  previous,
);
for (const mutate of [
  (rows) => {
    rows[0].inputs[0].pending = false;
  },
  (rows) => {
    rows[0].assets[0].id = "new-image";
  },
  (rows) => {
    rows[1].text = "New answer";
  },
  (rows) => {
    rows[1].streaming = false;
    rows[1].turnStatus = "completed";
  },
  (rows) => {
    delete rows[1].streaming;
  },
  (rows) => {
    rows[2].nativeError.metadata.retry = true;
  },
  (rows) => {
    rows[2].nativeError.metadata = [false];
  },
]) {
  const incoming = structuredClone(previous);
  mutate(incoming);
  const retained = retainTranscriptItems(previous, incoming);
  assert.deepEqual(retained, incoming);
  assert.equal(
    retained.filter((row, index) => row === previous[index]).length,
    2,
  );
}
const reordered = retainTranscriptItems(
  previous,
  structuredClone(previous).reverse(),
);
assert.equal(reordered[0], previous[2]);
assert.equal(reordered[2], previous[0]);
assert.deepEqual(retainTranscriptItems(previous, []), []);
const extended = [
  ...structuredClone(previous),
  { id: "new", role: "system", text: "New notice" },
];
assert.deepEqual(retainTranscriptItems(previous, extended), extended);
assert.equal(retainTranscriptItems(previous, extended)[0], previous[0]);
console.log(
  "PASS: full JSON snapshots retain unchanged rows; nested edits, removed fields, completion, reordering and deletion remain authoritative",
);
