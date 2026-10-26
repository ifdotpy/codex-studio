import assert from "node:assert/strict";
import fixtures from "./draftPayload.fixtures.json";
import { encodeDraftPayload } from "./draftPayload.ts";
import { it } from "vitest";

it("keeps JSON booleans out of object encoding", () => {
  assert.equal(encodeDraftPayload(false), "false");
});

for (const { scenario, input, expected } of fixtures) {
  it(`matches canonical encoding for ${scenario} payloads`, () => {
    const encoded = encodeDraftPayload({ ...input, omitted: undefined });
    assert.deepEqual(JSON.parse(encoded), input);
    assert.equal(
      encoded,
      expected,
      "Draft bytes preserve nested keys and Unicode",
    );
  });
}

it("orders keys that share a prefix by code point and then by length", () => {
  assert.equal(
    encodeDraftPayload({ aa: 2, a: 1, "😀": 3, "😀a": 4 }),
    '{"a":1,"aa":2,"😀":3,"😀a":4}',
  );
});

it("preserves JSON scalar and null payloads", () => {
  for (const value of [null, false, 0, "draft"]) {
    assert.equal(encodeDraftPayload(value), JSON.stringify(value));
  }
});
