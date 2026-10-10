import assert from "node:assert/strict";
import { retainTranscriptItems } from "./transcriptIdentity.ts";

import { it } from "vitest";

it("retains identity for unchanged transcript items and replaces changed data", () => {
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
});

it("keeps changed JSON shapes and deeply nested projections authoritative", () => {
  const row = (value) => [{ id: "message", role: "assistant", value }];
  const changed = [
    [null, {}],
    [{}, null],
    [false, {}],
    [42, {}],
    [{}, 42],
    [[], {}],
    [{}, []],
    [["first"], ["second"]],
    [{ key: "same" }, {}],
    [{}, { added: "new projection field" }],
    [{ first: "same" }, { other: "same" }],
  ];

  for (const [priorValue, incomingValue] of changed) {
    const previous = row(priorValue);
    const incoming = row(incomingValue);
    const retained = retainTranscriptItems(previous, incoming);
    assert.equal(retained[0], incoming[0]);
    assert.notEqual(retained[0], previous[0]);
    assert.deepEqual(retained, incoming);
  }

  // A JSON-like record may have inherited properties. Matching an inherited
  // key must not hide the changed own keys in an incoming projection.
  const previousWithOwnKey = row({ key: "same" });
  const incomingWithInheritedKey = row(
    Object.assign(Object.create({ key: "same" }), { other: "same" }),
  );
  const inheritedResult = retainTranscriptItems(
    previousWithOwnKey,
    incomingWithInheritedKey,
  );
  assert.equal(inheritedResult[0], incomingWithInheritedKey[0]);

  const nest = (value, levels) => {
    for (let level = 0; level < levels; level++) value = { child: value };
    return value;
  };
  const unchangedAtLimit = row(nest({ leaf: "same" }, 31));
  assert.equal(
    retainTranscriptItems(unchangedAtLimit, structuredClone(unchangedAtLimit)),
    unchangedAtLimit,
  );

  const deeperPrevious = row(nest({ leaf: "same" }, 34));
  const deeperIncoming = structuredClone(deeperPrevious);
  const deeperResult = retainTranscriptItems(deeperPrevious, deeperIncoming);
  assert.equal(deeperResult[0], deeperIncoming[0]);
  assert.notEqual(deeperResult[0], deeperPrevious[0]);
  console.log(
    "PASS: changed shapes and keys stay authoritative; reference retention respects the JSON comparison depth limit",
  );
});
