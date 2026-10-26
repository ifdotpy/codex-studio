import { test } from "vitest";
import assert from "node:assert/strict";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.ts";
import { createSessionStore } from "./session-store.ts";

test("unclassified API errors persist null and legacy subagent IDs remain distinct", async () => {
  const bridge = await startBridge(
    new URL("./output-compatibility.fixture.mts", import.meta.url),
  );
  try {
    const { turn } = await bridge.call("turn/start", {
      threadId: bridge.thread,
      input: [{ type: "text", text: "1" }],
    });
    await bridge.note(
      "turn/completed",
      (note) => note.params.turn.id === turn.id,
    );
    const read = await createSessionStore(
      path.join(bridge.root, "state"),
    ).readAll(bridge.thread);
    const saved = read.session.turns.find((entry) => entry.id === turn.id);
    assert.ok(saved);
    assert.equal(saved.apiErrorInfo, null);
    assert.deepEqual(
      saved.items.filter((item) => item.nativeSubagent).map((item) => item.id),
      [
        "subagent-first:text:0",
        "subagent-first:text:1",
        "subagent-second:text:0",
        "subagent-second:text:1",
      ],
    );
  } finally {
    await bridge.close();
  }
});
