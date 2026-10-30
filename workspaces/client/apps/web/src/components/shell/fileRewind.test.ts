import assert from "node:assert/strict";
import { test } from "vitest";
import { showsFileRewind } from "./fileRewind.ts";
import {
  PROVIDER_CAPABILITIES,
  providerCapabilities,
  providerSupports,
} from "../../generated/providerKind.ts";

test("each provider reports its capability list; unknown providers report none", () => {
  assert.deepEqual(providerCapabilities("claude"), PROVIDER_CAPABILITIES);
  assert.deepEqual(providerCapabilities("codex"), [
    "steer",
    "fork",
    "model_change",
    "effort",
    "compaction",
    "approvals",
  ]);
  for (const unknown of [undefined, null, "", "future", "CLAUDE"])
    assert.deepEqual(providerCapabilities(unknown), []);
  assert.equal(providerSupports("codex", "rewind"), false);
});

test("the file rewind control is hidden for an adapter without rewind", () => {
  assert.equal(
    showsFileRewind({ provider: "codex", workspaceMode: "shared" }),
    false,
  );
  assert.equal(showsFileRewind({ provider: "future" }), false);
  assert.equal(showsFileRewind({}), false);
});

test("the file rewind control shows for a rewind adapter in a shared folder", () => {
  assert.equal(
    showsFileRewind({ provider: "claude", workspaceMode: "shared" }),
    true,
  );
  assert.equal(showsFileRewind({ provider: "claude" }), true);
  assert.equal(
    showsFileRewind({ provider: "claude", workspaceMode: "worktree" }),
    false,
  );
  assert.equal(showsFileRewind({ provider: "claude", worktree: "/w" }), false);
});
