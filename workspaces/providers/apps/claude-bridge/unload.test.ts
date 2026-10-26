import type { UnloadLog } from "./unload.fixture.mts";
import type { BridgeHarness, BridgeNoteFor } from "./testing/bridge-harness.ts";
// Idle unload through the real bridge with a scripted SDK: a settled turn's
// thread/unsubscribe ends its CLI query, and the next turn starts a new query
// that resumes the same native session with only the new message. The CLI's
// resume_return dialog on such a resume is answered with the full history.
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { startBridge } from "./testing/bridge-harness.ts";

// Every query logs its start options, its inputs and its close to the cwd.
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

async function log(bridge: BridgeHarness) {
  const raw = await fs.readFile(path.join(bridge.root, ".log"), "utf8");
  return raw
    .trim()
    .split("\n")
    .map((line): UnloadLog => JSON.parse(line));
}

test("a settled turn's unload ends its CLI and the next turn resumes the session once", async () => {
  const bridge = await startBridge(FAKE_SDK, "claude-unload-");
  bridges.push(bridge);
  const first = await turn(bridge, "first");
  assert.equal(first.params.turn.status, "completed");
  assert.deepEqual(await bridge.call("thread/loaded/list", {}), {
    data: [bridge.thread],
    nextCursor: null,
  });

  assert.deepEqual(
    await bridge.call("thread/unsubscribe", { threadId: bridge.thread }),
    { status: "unsubscribed" },
  );
  assert.deepEqual(await bridge.call("thread/loaded/list", {}), {
    data: [],
    nextCursor: null,
  });

  await bridge.call("thread/resume", { threadId: bridge.thread });
  const second = await turn(bridge, "second");
  assert.equal(second.params.turn.status, "completed");
  const events = await log(bridge);
  const [started, resumed] = events.filter((event) => event.query);
  assert.ok(started?.query);
  assert.ok(resumed?.query);
  assert.equal(started.query.resume, null);
  assert.ok(started.query.sessionId);
  assert.equal(
    resumed.query.resume,
    started.query.sessionId,
    "the new CLI resumes the same native session",
  );
  assert.deepEqual(
    events.map((event) => event.input ?? (event.close ? "close" : "query")),
    ["query", "first", "close", "query", "second"],
    "the first CLI closes before the next starts; each message is sent once",
  );
});

test("a repeated unload closes nothing more", async () => {
  const bridge = await startBridge(FAKE_SDK, "claude-unload-");
  bridges.push(bridge);
  await turn(bridge, "first");
  await bridge.call("thread/unsubscribe", { threadId: bridge.thread });
  assert.deepEqual(
    await bridge.call("thread/unsubscribe", { threadId: bridge.thread }),
    { status: "notLoaded" },
    "a repeated unload is a no-op",
  );
  assert.equal(
    (await log(bridge)).filter((event) => event.close).length,
    1,
    "only the loaded CLI was closed",
  );
});

// A scripted CLI that raises its resume_return dialog on the first input of a
// resumed query, as the real one does for a long or large session.
const DIALOG_SDK = new URL("./unload-dialog.fixture.mts", import.meta.url);

const askedUser = (bridge: BridgeHarness) =>
  bridge.notes.some(
    (note): note is BridgeNoteFor<"item/tool/requestUserInput"> =>
      note.method === "item/tool/requestUserInput",
  );

test("a resume after Studio's own unload keeps the full history without asking", async () => {
  const bridge = await startBridge(DIALOG_SDK, "claude-unload-");
  bridges.push(bridge);
  await turn(bridge, "first");
  await bridge.call("thread/unsubscribe", { threadId: bridge.thread });
  await bridge.call("thread/resume", { threadId: bridge.thread });
  const second = await turn(bridge, "second");
  assert.equal(second.params.turn.status, "completed");
  const events = await log(bridge);
  assert.deepEqual(
    events.filter((event) => event.dialog).map((event) => event.dialog),
    [{ behavior: "completed", result: "continue" }],
  );
  assert.equal(askedUser(bridge), false, "the dialog never reaches the user");
  assert.deepEqual(
    events.filter((event) => event.input).map((event) => event.input),
    ["first", "second"],
    "the turn is delivered once",
  );
});

test("a resume Studio did not unload still asks through the user question", async () => {
  const bridge = await startBridge(DIALOG_SDK, "claude-unload-");
  bridges.push(bridge);
  await turn(bridge, "first");
  // Studio re-resumes the idle query without unloading it first.
  await bridge.call("thread/resume", { threadId: bridge.thread });
  const id = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "message-second",
      input: [{ type: "text", text: "second" }],
    })
  ).turn.id;
  assert.ok(id);
  await bridge.note(
    (n): n is BridgeNoteFor<"item/tool/requestUserInput"> =>
      n.method === "item/tool/requestUserInput",
  );
});
