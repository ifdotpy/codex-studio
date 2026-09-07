// New clients must also work with the old server's byte comparison.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { encodeDraftPayload } from "../web/src/sync/draftPayload.ts";
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
for (const value of [
  original,
  { ...original, text: "" },
  { ...original, alternatives: ["other device"] },
]) {
  const encoded = encodeDraftPayload(value);
  assert.deepEqual(JSON.parse(encoded), JSON.parse(JSON.stringify(value)));
  const expected = spawnSync(
    "python3",
    [
      "-c",
      "import json,sys; print(json.dumps(json.load(sys.stdin), sort_keys=True, separators=(',', ':'), ensure_ascii=False), end='')",
    ],
    { input: JSON.stringify(value), encoding: "utf8" },
  );
  assert.equal(expected.status, 0, expected.stderr);
  assert.equal(
    encoded,
    expected.stdout,
    "Draft bytes match the server, including nested keys and Unicode",
  );
}
console.log(
  "Draft payload compatibility: PASS (edit, clear, alternatives, nested values, Unicode, old server encoding).",
);
