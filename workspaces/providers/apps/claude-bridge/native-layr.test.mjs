import assert from "node:assert/strict";
import { test } from "vitest";
import { nativeLayrEnvironment } from "./native-layr.mjs";

test("native Layr values remain local to their session", () => {
  const session = { studioNativeLayrEnvironment: { LAYR_ROOT: "/store", LAYR_LINE: "worker" } };
  const result = nativeLayrEnvironment(session);
  assert.deepEqual(result, session.studioNativeLayrEnvironment);
  result.LAYR_LINE = "different";
  assert.equal(session.studioNativeLayrEnvironment.LAYR_LINE, "worker");
  assert.deepEqual(nativeLayrEnvironment({}), {});
});

test("native Layr metadata cannot replace unrelated process variables", () => {
  for (const value of [{ HOME: "/other" }, [], { LAYR_ROOT: 1 }, { LAYR_ROOT: "bad\0path" }])
    assert.throws(() => nativeLayrEnvironment({ studioNativeLayrEnvironment: value }));
});
