import type { BridgeHarness, BridgeNoteFor } from "./testing/bridge-harness.ts";
// Exercise thread start/resume through the real bridge with an SDK fixture.
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.ts";

const FAKE_SDK = new URL("./workspace-fence.fixture.mts", import.meta.url);
const bridges: BridgeHarness[] = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

test("the SDK receives each thread's host worktree fence, including resume", async () => {
  const bridge = await startBridge(FAKE_SDK, "claude-workspace-fence-");
  bridges.push(bridge);
  const tree = bridge.root;
  const fence = JSON.stringify(["/source checkout", '/sibling:quoted"path']);
  const options: import("./bridge-protocol.ts").BridgeParams<"thread/start"> = {
    cwd: tree,
    dynamicTools: [],
    claude: {
      permissionMode: "bypassPermissions",
      env: {
        CLAUDE_CODE_HOST_WORKTREE: tree,
        CLAUDE_CODE_HOST_WORKTREE_FENCE: fence,
      },
    },
  };
  const isolated = (await bridge.call("thread/start", options)).thread.id;
  let input = 0;
  async function run(threadId: string) {
    const turn = (
      await bridge.call("turn/start", {
        threadId,
        clientUserMessageId: `input-${++input}`,
        input: [{ type: "text", text: "check environment" }],
      })
    ).turn.id;
    const note = await bridge.note(
      (n): n is BridgeNoteFor<"turn/completed"> =>
        n.method === "turn/completed" && n.params.turn.id === turn,
    );
    assert.equal(note.params.turn.status, "completed");
    return JSON.parse(
      await fs.readFile(path.join(tree, "sdk-env.json"), "utf8"),
    );
  }
  const expected = {
    tree,
    fence,
    permissionMode: "bypassPermissions",
    account: "test@example.test",
  };
  assert.deepEqual(await run(isolated), expected);
  const shared = await run(bridge.thread);
  assert.equal(shared.tree, undefined);
  assert.equal(shared.fence, undefined);
  assert.equal(shared.account, "test@example.test");
  await bridge.call("thread/resume", { threadId: isolated, ...options });
  assert.deepEqual(await run(isolated), expected);
});
