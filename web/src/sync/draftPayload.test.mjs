import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { encodeDraftPayload } from "./draftPayload.ts";
import { it } from "vitest";

const original = {
  updated: 1788798612345,
  text: 'Привет 👋\nA quoted "value"',
  session: "lead",
  id: "device:lead",
  device: "device",
  alternatives: ["first", "second"],
  metadata: {
    2: "two",
    10: "ten",
    "😀": false,
    "\uE000": null,
    z: { b: [1, true, null], a: "nested" },
  },
  omitted: undefined,
};

for (const [scenario, value] of [
  ["edit", original],
  ["clear", { ...original, text: "" }],
  ["alternatives", { ...original, alternatives: ["other device"] }],
]) {
  it(`matches Python's canonical encoding for ${scenario} payloads`, () => {
    const encoded = encodeDraftPayload(value);
    assert.deepEqual(JSON.parse(encoded), JSON.parse(JSON.stringify(value)));
    const expected = spawnSync(
      "python3",
      [
        "-c",
        "import json,sys; print(json.dumps(json.load(sys.stdin), sort_keys=True, separators=(',', ':'), ensure_ascii=False), end='')",
      ],
      { input: JSON.stringify(value), encoding: "utf8", timeout: 10_000 },
    );
    assert.equal(expected.status, 0, expected.stderr);
    assert.equal(
      encoded,
      expected.stdout,
      "Draft bytes match the server, including nested keys and Unicode",
    );
  });
}
