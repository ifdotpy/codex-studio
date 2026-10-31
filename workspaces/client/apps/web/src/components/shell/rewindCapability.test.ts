import assert from "node:assert/strict";
import { test } from "vitest";
import {
  PROVIDER_CAPABILITIES,
  providerCapabilities,
  providerSupports,
} from "../../generated/providerKind.ts";

test("each provider reports its capability list; unknown providers report none", () => {
  assert.deepEqual(providerCapabilities("claude"), PROVIDER_CAPABILITIES);
  assert.deepEqual(providerCapabilities("codex"), PROVIDER_CAPABILITIES);
  for (const unknown of [undefined, null, "", "future", "CLAUDE"])
    assert.deepEqual(providerCapabilities(unknown), []);
});

test("edit(turn) gives every provider the rewind control; an unknown provider has none", () => {
  assert.equal(providerSupports("codex", "rewind"), true);
  assert.equal(providerSupports("claude", "rewind"), true);
  assert.equal(providerSupports("future", "rewind"), false);
  assert.equal(providerSupports(undefined, "rewind"), false);
});
