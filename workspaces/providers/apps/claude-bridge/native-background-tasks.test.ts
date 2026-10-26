import type { BridgeHarness, BridgeNoteFor } from "./testing/bridge-harness.ts";
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.ts";

// Exercise the real bridge with task events and a native wake after turn end.
const FAKE_SDK = new URL(
  "./native-background-tasks.fixture.mts",
  import.meta.url,
);

const bridges: BridgeHarness[] = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

test.each([
  ["completed", "completed"],
  ["failed", "failed"],
  ["killed", "failed"],
  ["stopped", "failed"],
])(
  "a native task stays running until its %s notification and preserves the wake turn",
  async (status, expectedStatus) => {
    const bridge = await startBridge(FAKE_SDK, "claude-native-task-");
    bridges.push(bridge);
    const turn = (
      await bridge.call("turn/start", {
        threadId: bridge.thread,
        input: [{ type: "text", text: "Start a background task" }],
      })
    ).turn.id;
    await bridge.note(
      (n): n is BridgeNoteFor<"turn/completed"> =>
        n.method === "turn/completed" && n.params.turn.id === turn,
    );
    const taskEvents = () =>
      bridge.notes.filter(
        (n): n is BridgeNoteFor<"item/started" | "item/completed"> =>
          (n.method === "item/started" || n.method === "item/completed") &&
          n.params.item.id === "task:native-1",
      );
    assert.deepEqual(
      taskEvents().map((n) => [n.method, n.params.item.status]),
      Array(3).fill(["item/started", "inProgress"]),
    );
    assert.deepEqual(taskEvents()[1]?.params.item.result, {
      content: [{ type: "text", text: "Build still running" }],
    });
    assert.equal(
      taskEvents()[2]?.params.item.arguments?.description,
      "Build almost done",
    );
    // Publish the marker only after its status is complete.
    const pending = path.join(bridge.root, ".finish-pending");
    await fs.writeFile(pending, status);
    await fs.rename(pending, path.join(bridge.root, ".finish"));
    const wake = await bridge.note(
      (n): n is BridgeNoteFor<"turn/completed"> =>
        n.method === "turn/completed" && n.params.turn.id !== turn,
    );
    assert.equal(wake.params.turn.status, "completed");
    const events = taskEvents();
    assert.deepEqual(
      events.map((n) => [n.method, n.params.item.status]),
      [
        ...Array(4).fill(["item/started", "inProgress"]),
        ["item/completed", expectedStatus],
      ],
    );
    const terminal = events.at(-1);
    assert.ok(terminal);
    assert.equal(terminal.params.turnId, turn);
    assert.equal(terminal.params.item.tool, "local_bash");
    assert.equal(
      terminal.params.item.outputFile,
      path.join(bridge.root, "native-1.output"),
    );
    assert.deepEqual(terminal.params.item.result, {
      content: [{ type: "text", text: "Task " + status }],
    });
    assert.equal(
      bridge.notes.filter(
        (n): n is BridgeNoteFor<"turn/started"> => n.method === "turn/started",
      ).length,
      2,
    );
  },
);
