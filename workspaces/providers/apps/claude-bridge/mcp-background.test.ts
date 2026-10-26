import type { BridgeHarness, BridgeNoteFor } from "./testing/bridge-harness.ts";
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.ts";

const FAKE_SDK = new URL("./mcp-background.fixture.mts", import.meta.url);

const bridges: BridgeHarness[] = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

async function start(options: Parameters<typeof startBridge>[2] = {}) {
  const bridge = await startBridge(FAKE_SDK, "claude-mcp-background-", {
    clock: true,
    ...options,
  });
  bridges.push(bridge);
  return bridge;
}

async function begin(bridge: BridgeHarness) {
  const { turn } = (await bridge.call("turn/start", {
    threadId: bridge.thread,
    input: [{ type: "text", text: "Start the slow tool" }],
  })) as { turn: { id: string } };
  await bridge.note("item/started", (n) => n.params.item.id === "mcp-call");
  return turn.id;
}

test("MCP timer releases the turn, retains the task, avoids a false stall, and wakes once", async () => {
  const bridge = await start({
    env: { STUDIO_CLAUDE_TOOL_SILENCE_MS: "180000" },
  });
  const turn = await begin(bridge);
  const options = JSON.parse(
    await fs.readFile(path.join(bridge.root, "options.json"), "utf8"),
  );
  assert.equal(options.env.CLAUDE_AUTO_BACKGROUND_TASKS, "true");
  assert.equal(options.env.CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS, "120000");
  await bridge.advanceTime(119999);
  assert.equal(
    bridge.notes.some((n) => n.method === "turn/completed"),
    false,
  );
  await bridge.advanceTime(1);
  await bridge.advanceUntil("turn/completed", (n) => n.params.turn.id === turn);
  const diagnostics = await bridge.call("claude/diagnostics", {});
  assert.equal(diagnostics.activeTurns, 0);
  assert.equal(diagnostics.backgroundQueries, 1);
  assert.equal(
    (await bridge.call("claude/state", { threadId: bridge.thread })).tasks[0]
      .task_id,
    "mcp-task",
  );
  await bridge.advanceTime(180001);
  assert.equal(
    bridge.notes.some(
      (n) => n.method === "item/completed" && n.params.item.stalled === true,
    ),
    false,
  );
  await assert.rejects(
    fs.stat(path.join(bridge.root, ".interrupted")),
    /ENOENT/,
  );
  await bridge.writeSignal(".finish");
  await bridge.advanceUntil("turn/completed", (n) => n.params.turn.id !== turn);
  assert.equal(
    bridge.notes.filter((n) => n.method === "turn/started").length,
    2,
  );
  const tasks = bridge.notes.filter(
    (n): n is BridgeNoteFor<"item/started" | "item/completed"> =>
      (n.method === "item/started" || n.method === "item/completed") &&
      n.params.item.id === "task:mcp-task",
  );
  assert.equal(tasks.at(-1)?.params.item.status, "completed");
  assert.equal(tasks.at(-1)?.params.turnId, turn);
  assert.equal(
    (await bridge.call("claude/state", { threadId: bridge.thread })).tasks
      .length,
    0,
  );
});

test("the explicit opt-out survives restart and the foreground stall watch stays active", async () => {
  const bridge = await start({
    env: {
      STUDIO_CLAUDE_TOOL_SILENCE_MS: "180000",
      STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "10",
    },
  });
  await bridge.call("claude/settings", {
    threadId: bridge.thread,
    settings: { mcpAutoBackground: false },
  });
  await bridge.restart();
  const turn = await begin(bridge);
  const options = JSON.parse(
    await fs.readFile(path.join(bridge.root, "options.json"), "utf8"),
  );
  assert.equal(options.env.CLAUDE_AUTO_BACKGROUND_TASKS, "false");
  assert.equal(options.env.CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS, "0");
  await bridge.advanceTime(180001);
  const completed = await bridge.advanceUntil(
    "turn/completed",
    (n) => n.params.turn.id === turn,
  );
  assert.equal(completed.params.turn.status, "interrupted");
  assert.equal(
    bridge.notes.some(
      (n) => n.method === "item/completed" && n.params.item.stalled === true,
    ),
    true,
  );
});
