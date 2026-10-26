import { test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.ts";
import type { Options, Query } from "@anthropic-ai/claude-agent-sdk";

type CapturedSettings = {
  initial: Options["settings"];
  live?: Parameters<Query["applyFlagSettings"]>[0];
};
async function capturedSettings(root: string): Promise<CapturedSettings> {
  // This fixture writes the document with the same SDK-owned types it reads.
  return JSON.parse(
    await fs.readFile(path.join(root, ".flag-settings.json"), "utf8"),
  );
}

test("initial settings serialize the unchanged SDK flag object, including max and null", async () => {
  const bridge = await startBridge(
    new URL("./settings.fixture.mts", import.meta.url),
    "claude-settings-",
    {
      thread: { model: "claude-sonnet-4-5", claude: { thinking: null } },
    },
  );
  try {
    const flags = {
      idleCompaction: false,
      fastMode: false,
      effortLevel: "max",
      alwaysThinkingEnabled: null,
    } satisfies Parameters<Query["applyFlagSettings"]>[0];
    for (const id of ["initial", "live"]) {
      const result = await bridge.call("turn/start", {
        threadId: bridge.thread,
        clientUserMessageId: id,
        effort: "max",
        input: [{ type: "text", text: id }],
      });
      await bridge.note(
        "turn/completed",
        (note) => note.params.turn.id === result.turn.id,
      );
    }
    const settings = await capturedSettings(bridge.root);
    assert.equal(typeof settings.initial, "string");
    assert.equal(settings.initial, JSON.stringify(flags));
    assert.deepEqual(settings.live, flags);
    assert.deepEqual(JSON.parse(String(settings.initial)), settings.live);
  } finally {
    await bridge.close();
  }
});
