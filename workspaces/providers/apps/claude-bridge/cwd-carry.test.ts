import type { UnloadLog } from "./unload.fixture.mts";
import type { BridgeHarness, BridgeNoteFor } from "./testing/bridge-harness.ts";
// A chat whose workspace moves (for example from a Git worktree to a Layr line)
// resumes with a new cwd. Claude keys native history by cwd, so the bridge
// carries the session file to the new project folder and resumes the same session.
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.ts";
import { nativeDestination } from "./move.ts";

const FAKE_SDK = new URL("./unload.fixture.mts", import.meta.url);
const bridges: BridgeHarness[] = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

async function turn(bridge: BridgeHarness, text: string) {
  const id = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "message-" + text,
      input: [{ type: "text", text }],
    })
  ).turn.id;
  return bridge.note(
    (n): n is BridgeNoteFor<"turn/completed"> =>
      n.method === "turn/completed" && n.params.turn.id === id,
  );
}

async function log(cwd: string) {
  const raw = await fs.readFile(path.join(cwd, ".log"), "utf8");
  return raw
    .trim()
    .split("\n")
    .map((line): UnloadLog => JSON.parse(line));
}

test("a resume in a new cwd carries the native session and continues it", async () => {
  const bridge = await startBridge(FAKE_SDK, "claude-cwd-carry-");
  bridges.push(bridge);
  await turn(bridge, "first");
  const sessionId = (await log(bridge.root)).find((e) => e.query)!.query!
    .sessionId!;
  const config = path.join(bridge.root, "native-config");
  const before = await nativeDestination(sessionId, bridge.root, config);
  const history = '{"type":"user","sessionId":"' + sessionId + '"}\n';
  await fs.mkdir(path.dirname(before), { recursive: true });
  await fs.writeFile(before, history);
  const line = path.join(bridge.root, "layr-line");
  await fs.mkdir(line);

  await bridge.call("thread/unsubscribe", { threadId: bridge.thread });
  await bridge.call("thread/resume", { threadId: bridge.thread, cwd: line });
  const second = await turn(bridge, "second");
  assert.equal(second.params.turn.status, "completed");

  const after = await nativeDestination(sessionId, line, config);
  assert.equal(await fs.readFile(after, "utf8"), history);
  assert.equal(
    await fs.readFile(before, "utf8"),
    history,
    "the old history stays in place",
  );
  const resumed = (await log(line)).find((event) => event.query);
  assert.equal(resumed?.query?.resume, sessionId, "the same session resumes");
  // A repeated resume in the same cwd is a no-op.
  await bridge.call("thread/unsubscribe", { threadId: bridge.thread });
  await bridge.call("thread/resume", { threadId: bridge.thread, cwd: line });
});
