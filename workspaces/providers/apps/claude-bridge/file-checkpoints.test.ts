import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.mjs";

const SDK = new URL("./file-checkpoints.sdk.ts", import.meta.url);
const bridges: Awaited<ReturnType<typeof startBridge>>[] = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

test("bridge enables SDK checkpoints and rewinds the exact echoed message with context", async () => {
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
  assert.equal(state.turns[0].canRestoreFiles, true);
  const request = {
    threadId: bridge.thread,
    turnId: started.turn.id,
    requestId: "rewind",
    restoreFiles: true,
  };
  const result = await bridge.call("thread/rollback", request);
  assert.equal(result.filesRestored, true);
  assert.equal(
    await fs.readFile(path.join(bridge.root, "file.txt"), "utf8"),
    "checkpoint",
  );
  assert.equal(
    (await bridge.call("claude/state", { threadId: bridge.thread })).turns
      .length,
    0,
  );
  assert.deepEqual(await bridge.call("thread/rollback", request), result);
  const usage = await bridge.call("claude/fileBackups", {
    threadId: bridge.thread,
  });
  assert.equal(usage.count, 1);
  assert.equal(
    (
      await bridge.call("claude/fileBackups", {
        threadId: bridge.thread,
        settledRequestIds: [],
      })
    ).released,
    0,
  );
  const released = await bridge.call("claude/fileBackups", {
    threadId: bridge.thread,
    settledRequestIds: ["rewind"],
  });
  assert.equal(released.released, 1);
  assert.equal(released.count, 0);
  assert.deepEqual(await bridge.call("thread/rollback", request), result);
  assert.equal(
    await fs.readFile(path.join(bridge.root, "file.txt"), "utf8"),
    "checkpoint",
  );
  const calls = (
    await fs.readFile(path.join(bridge.root, "sdk-calls.jsonl"), "utf8")
  )
    .trim()
    .split("\n")
    .map((line): Record<string, unknown> => JSON.parse(line));
  const input = calls.find((c) => c.kind === "input");
  for (const q of calls.filter((c) => c.kind === "query")) {
    assert.equal(q.enabled, true);
    assert.equal(q.env, "true");
  }
  const rewinds = calls.filter((c) => c.kind === "rewind");
  assert.equal(rewinds.length, 2);
  assert.deepEqual(
    rewinds.map((c) => c.rewind),
    [{ dryRun: true }, { dryRun: false }],
  );
  assert.ok(
    rewinds.every(
      (c) => c.messageId === input?.messageId && c.resume === bridge.thread,
    ),
  );
});
