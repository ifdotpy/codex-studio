import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.ts";

const SDK = new URL("./file-checkpoints.sdk.ts", import.meta.url);
const bridges: Awaited<ReturnType<typeof startBridge>>[] = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

test("bridge enables SDK checkpoints and marks the turn restorable", async () => {
  const bridge = await startBridge(SDK, "claude-file-checkpoints-");
  bridges.push(bridge);
  const started = await bridge.call("turn/start", {
    threadId: bridge.thread,
    clientUserMessageId: "human-message",
    input: [{ type: "text", text: "edit" }],
  });
  await bridge.note(
    (n) =>
      n.method === "turn/completed" && n.params.turn.id === started.turn.id,
  );
  const state = await bridge.call("claude/state", { threadId: bridge.thread });
  assert.ok(state.turns[0]);
  assert.equal(state.turns[0].canRestoreFiles, true);
  const calls = (
    await fs.readFile(path.join(bridge.root, "sdk-calls.jsonl"), "utf8")
  )
    .trim()
    .split("\n")
    .map((line): Record<string, unknown> => JSON.parse(line));
  for (const q of calls.filter((c) => c.kind === "query")) {
    assert.equal(q.enabled, true);
    assert.equal(q.env, "true");
  }
});
