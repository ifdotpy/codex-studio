import { test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.ts";

async function start(bridge, threadId, text, completed = true) {
  const { turn } = await bridge.call("turn/start", {
    threadId,
    input: [{ type: "text", text }],
  });
  if (completed)
    await bridge.note(
      (n) => n.method === "turn/completed" && n.params.turn.id === turn.id,
    );
  return turn.id;
}
for (const mode of ["running", "background"]) {
  test(`bridge forks an earlier completed turn while source has ${mode} work`, async () => {
    const bridge = await startBridge(
      new URL("./fixtures/fork-active-sdk.ts", import.meta.url),
    );
    try {
      const first = await start(bridge, bridge.thread, "first");
      const latest = await start(
        bridge,
        bridge.thread,
        mode,
        mode !== "running",
      );
      const before = (
        await bridge.call("thread/read", {
          threadId: bridge.thread,
          includeTurns: true,
        })
      ).thread;
      const target = (
        await bridge.call("thread/fork", {
          threadId: bridge.thread,
          lastTurnId: first,
        })
      ).thread;
      assert.notEqual(target.id, bridge.thread);
      assert.equal(target.turns.length, 1);
      assert.equal(target.turns[0].status, "completed");
      assert.deepEqual(
        (
          await bridge.call("thread/read", {
            threadId: bridge.thread,
            includeTurns: true,
          })
        ).thread.turns,
        before.turns,
      );
      assert.deepEqual((await bridge.call("thread/loaded/list", {})).data, [
        bridge.thread,
      ]);
      for (const lastTurnId of [undefined, latest])
        await assert.rejects(
          bridge.call("thread/fork", { threadId: bridge.thread, lastTurnId }),
          /earlier completed Claude turn/,
        );
      await start(bridge, target.id, "fork input");
      assert.equal((await bridge.call("claude/activity", {})).busy, true);
      const events = (
        await fs.readFile(path.join(bridge.root, ".query-log"), "utf8")
      )
        .trim()
        .split("\n")
        .map((line) => JSON.parse(line));
      assert.equal(
        events.filter((e) => e.id === bridge.thread && e.start).length,
        1,
      );
      assert.equal(
        events.some((e) => e.id === bridge.thread && e.close),
        false,
      );
      if (mode === "background")
        assert.equal(
          (
            await bridge.call("thread/backgroundTerminals/list", {
              threadId: bridge.thread,
            })
          ).data[0].task_id,
          "bg",
        );
    } finally {
      await bridge.close();
    }
  });
}
