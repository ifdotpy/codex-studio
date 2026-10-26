import type { BridgeParams } from "./bridge-protocol.ts";
import type { BridgeHarness } from "./testing/bridge-harness.ts";
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import path from "node:path";
import { createSessionStore } from "./session-store.ts";
import { startBridge } from "./testing/bridge-harness.ts";

const SDK = new URL("./live-permissions.fixture.mts", import.meta.url);
const bridges: BridgeHarness[] = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});
async function running() {
  const bridge = await startBridge(SDK, "claude-live-permissions-", {
    thread: { claude: { permissionMode: "default", thinking: true } },
  });
  bridges.push(bridge);
  await bridge.call("turn/start", {
    threadId: bridge.thread,
    input: [{ type: "text", text: "wait for permission" }],
  });
  await bridge.note("item/commandExecution/requestApproval");
  return bridge;
}
test("a live permission switch reaches the query and Full releases its pending approval", async () => {
  const bridge = await running();
  const options = await bridge.note("fixture/options");
  assert.deepEqual(options.params, { allowed: true, mode: "default" });
  const settings = {
    permissionMode: "bypassPermissions",
    thinking: true,
  } satisfies BridgeParams<"claude/settings">["settings"];
  assert.deepEqual(
    await bridge.call("claude/settings", { threadId: bridge.thread, settings }),
    { settings },
  );
  assert.equal(
    (await bridge.note("fixture/mode")).params.mode,
    "bypassPermissions",
  );
  assert.equal(
    (await bridge.note("fixture/permission")).params.behavior,
    "allow",
  );
  assert.equal(
    (await bridge.call("claude/state", { threadId: bridge.thread })).settings
      .permissionMode,
    "bypassPermissions",
  );
  await bridge.call("claude/settings", {
    threadId: bridge.thread,
    settings: { ...settings, permissionMode: "default" },
  });
  assert.equal(
    (await bridge.call("claude/state", { threadId: bridge.thread })).settings
      .permissionMode,
    "default",
  );
  await bridge.writeSignal(".ask");
  const pending = await bridge.note(
    "item/commandExecution/requestApproval",
    (n) => n.params.itemId === "permission-two",
  );
  assert.equal(
    bridge.notes.some((n) => n.method === "fixture/second-permission"),
    false,
  );
  bridge.process.stdin!.write(
    JSON.stringify({ id: pending.id, result: { decision: "decline" } }) + "\n",
  );
  assert.equal(
    (await bridge.note("fixture/second-permission")).params.behavior,
    "deny",
  );
});
test("failed live apply keeps persisted settings and pending permission unchanged", async () => {
  const bridge = await running();
  await assert.rejects(
    bridge.call("claude/settings", {
      threadId: bridge.thread,
      settings: { permissionMode: "plan", thinking: true },
    }),
    (error: Error & { data?: { settingsOutcome?: string } }) =>
      error.message.includes("fixture live apply failed") &&
      error.data?.settingsOutcome === "not_applied",
  );
  assert.equal(
    (await bridge.call("claude/state", { threadId: bridge.thread })).settings
      .permissionMode,
    "default",
  );
  assert.equal(
    bridge.notes.some((n) => n.method === "fixture/permission"),
    false,
  );
});

test("a metadata failure after the journal append restores persisted and live permission mode", async () => {
  const bridge = await running();
  await bridge.writeSignal(".fail-save");
  await assert.rejects(
    bridge.call("claude/settings", {
      threadId: bridge.thread,
      settings: { permissionMode: "bypassPermissions", thinking: true },
    }),
    /fixture metadata save failed/,
  );
  const stored = await createSessionStore(path.join(bridge.root, "state")).get(
    bridge.thread,
  );
  assert.equal(stored.claude?.permissionMode, "default");
  assert.equal(
    bridge.notes.some((n) => n.method === "fixture/permission"),
    false,
  );
});
