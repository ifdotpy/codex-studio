import type { BridgeHarness, BridgeNoteFor } from "./testing/bridge-harness.ts";
// A long streamed answer through the real bridge with a scripted SDK. No model
// calls. The turn's input text is the number of text deltas per block.
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import path from "node:path";
import { createSessionStore } from "./session-store.ts";
import { startBridge } from "./testing/bridge-harness.ts";

const FAKE_SDK = new URL("./streaming.fixture.mts", import.meta.url);

const bridges: BridgeHarness[] = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

test("a long streamed answer keeps every delta, line and the saved text", async () => {
  const deltasPerBlock = 2_000;
  const bridge = await startBridge(FAKE_SDK, "claude-streaming-");
  bridges.push(bridge);
  const turn = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "long-1",
      input: [{ type: "text", text: String(deltasPerBlock) }],
    })
  ).turn.id;
  const completed = await bridge.note(
    (n): n is BridgeNoteFor<"turn/completed"> => n.method === "turn/completed",
  );
  assert.equal(completed.params.turn.id, turn);
  assert.equal(completed.params.turn.status, "completed");
  const blocks = [0, 1].map((index) =>
    Array.from(
      { length: deltasPerBlock },
      (_, i) => "b" + index + "w" + i + " ",
    ).join(""),
  );
  const expected = blocks.join("\n");
  const deltas = bridge.notes.filter(
    (n): n is BridgeNoteFor<"item/agentMessage/delta"> =>
      n.method === "item/agentMessage/delta" && n.params.itemId === "message-1",
  );
  assert.equal(deltas.length, 2 * deltasPerBlock, "one line per delta");
  assert.equal(deltas.map((n) => n.params.delta).join(""), expected);
  const answer = bridge.notes.find(
    (n): n is BridgeNoteFor<"item/completed"> =>
      n.method === "item/completed" && n.params.item.id === "message-1",
  );
  assert.ok(answer);
  assert.equal(answer.params.item.text, expected);
  const saved = await createSessionStore(
    path.join(bridge.root, "state"),
  ).readAll(bridge.thread);
  const savedTurn = saved.session.turns.find((item) => item.id === turn);
  assert.ok(savedTurn);
  const savedAnswer = savedTurn.items.filter((item) => item.id === "message-1");
  assert.equal(savedAnswer.length, 1);
  assert.ok(savedAnswer[0]);
  assert.equal(savedAnswer[0].text, expected);
});
