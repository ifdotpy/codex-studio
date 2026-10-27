import type { BridgeNoteFor } from "./testing/bridge-harness.ts";
import { test } from "vitest";
import assert from "node:assert/strict";
import { startBridge } from "./testing/bridge-harness.ts";

const FAKE_SDK = new URL("./mcp-status.fixture.mts", import.meta.url);

test("MCP status and reconnect use the session query and preserve discovery pagination", async () => {
  const bridge = await startBridge(FAKE_SDK);
  try {
    await assert.rejects(
      bridge.call("mcpServerStatus/list", { threadId: bridge.thread }),
      /query is not loaded/,
    );
    const started = await bridge.call("turn/start", {
      threadId: bridge.thread,
      input: [{ type: "text", text: "start" }],
    });
    await bridge.note(
      (n): n is BridgeNoteFor<"turn/completed"> =>
        n.method === "turn/completed" && n.params.turn.id === started.turn.id,
    );
    const first = await bridge.call("mcpServerStatus/list", {
      threadId: bridge.thread,
      limit: 1,
    });
    assert.deepEqual(first, {
      data: [
        {
          name: "one",
          status: "connected",
          scope: "project",
          tools: [{ name: "read" }],
        },
      ],
      nextCursor: "1",
    });
    const second = await bridge.call("mcpServerStatus/list", {
      threadId: bridge.thread,
      cursor: first.nextCursor,
      limit: 1,
    });
    assert.ok(second.data[0]);
    assert.equal(second.data[0].status, "failed");
    assert.equal(second.nextCursor, null);
    await bridge.call("mcpServer/reconnect", {
      threadId: bridge.thread,
      name: "two",
    });
    assert.equal(
      (await bridge.call("mcpServerStatus/list", { threadId: bridge.thread }))
        .data[1]?.status,
      "connected",
    );
    await assert.rejects(
      bridge.call("mcpServer/reconnect", {
        threadId: bridge.thread,
        name: "missing",
      }),
      /unknown server/,
    );
    await assert.rejects(
      bridge.call("mcpServerStatus/list", {
        threadId: bridge.thread,
        cursor: "wrong",
      }),
      /cursor/,
    );
    await bridge.call("thread/unsubscribe", { threadId: bridge.thread });
    await assert.rejects(
      bridge.call("mcpServerStatus/list", { threadId: bridge.thread }),
      /query is not loaded/,
    );
  } finally {
    await bridge.close();
  }
});
