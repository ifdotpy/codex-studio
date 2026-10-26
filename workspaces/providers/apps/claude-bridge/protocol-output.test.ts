import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import type { SDKAssistantMessage } from "@anthropic-ai/claude-agent-sdk";
import { test } from "vitest";
import { assistant, zeroResult } from "./usage-fixtures.ts";
import { startBridge } from "./testing/bridge-harness.ts";

test("native plan updates preserve raw todo fields at the wire boundary", async () => {
  const message = {
    ...assistant,
    uuid: randomUUID(),
    message: {
      ...assistant.message,
      content: [
        {
          type: "tool_use",
          id: "todo-call",
          name: "TodoWrite",
          input: { todos: [{ content: 42, status: { future: true } }, {}] },
        },
      ],
    },
  } satisfies SDKAssistantMessage;
  const bridge = await startBridge(
    new URL("./usage-events.fixture.mts", import.meta.url),
    "claude-protocol-plan-",
    {
      env: { STUDIO_FIXTURE_EVENTS: JSON.stringify([message, zeroResult]) },
    },
  );
  try {
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      input: [{ type: "text", text: "Continue" }],
    });
    const note = await bridge.note("turn/plan/updated");
    assert.deepEqual(note.params.plan, [
      { step: 42, status: { future: true } },
      {},
    ]);
  } finally {
    await bridge.close();
  }
});
