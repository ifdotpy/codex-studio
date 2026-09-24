// Run with: node --experimental-strip-types tests/native-commands-contract.mjs
import assert from "node:assert/strict";
import test from "node:test";
import {
  menuActions,
  studioCommand,
} from "../web/src/nativeCommands.ts";

test("Codex chats run compact and review as Studio actions", () => {
  for (const text of ["/compact", "/review", "/stop", "/stop-team", "/review extra"])
    assert.equal(studioCommand(text, "codex"), true, text);
  assert.equal(studioCommand("/compacting", "codex"), false);
  assert.equal(studioCommand("please /review", undefined), false);
  assert.deepEqual(menuActions("codex"), ["compact", "review"]);
  assert.deepEqual(menuActions(undefined), ["compact", "review"]);
});

test("Claude chats send compact and review to Claude", () => {
  assert.equal(studioCommand("/compact", "claude"), false);
  assert.equal(studioCommand("/review", "claude"), false);
  assert.equal(studioCommand("/stop", "claude"), true);
  assert.equal(studioCommand("/stop-team", "claude"), true);
  assert.deepEqual(menuActions("claude"), ["compact"]);
});
