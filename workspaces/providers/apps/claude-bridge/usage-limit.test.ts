import fs from "node:fs/promises";
// A Claude subscription limit through the real bridge with a scripted SDK. No
// model calls. The messages follow what Claude Code writes to its stream when a
// request is refused at the limit: a rate_limit_event, then a synthetic
// assistant message with `error: "rate_limit"`, then a failed result.
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import { startBridge } from "./testing/bridge-harness.ts";

const RESET = 1_900_000_000;
const LIMIT_TEXT = "You've hit your session limit · resets 3:40am (UTC)";

// Input "work": one completed turn, then Claude starts a turn of its own (as for
// a background task notification) and the limit refuses its first request. The
// rate_limit_event comes before that turn exists. Input "plain": the turn the
// input started is refused, and the message has no rate_limit_info.
const FAKE_SDK = await fs.readFile(
  new URL("./usage-limit.fixture.mts", import.meta.url),
  "utf8",
);

const bridges: Awaited<ReturnType<typeof startBridge>>[] = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});

async function start(text: string) {
  const bridge = await startBridge(FAKE_SDK, "claude-usage-limit-");
  bridges.push(bridge);
  const turn = (
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "limit-" + text,
      input: [{ type: "text", text }],
    })
  ).turn.id;
  return { bridge, turn };
}

test("a limit that refuses a turn Claude started itself reports the provider reset", async () => {
  const { bridge, turn } = await start("work");
  const first = await bridge.note(
    (n) => n.method === "turn/completed" && n.params.turn.id === turn,
  );
  assert.equal(first.params.turn.status, "completed");
  const own = await bridge.note(
    (n) => n.method === "turn/completed" && n.params.turn.id !== turn,
  );
  assert.equal(own.params.turn.status, "failed");
  assert.deepEqual(own.params.turn.error, {
    message: LIMIT_TEXT,
    codexErrorInfo: "rateLimitExceeded",
    resetsAt: RESET,
  });
  const shown = await bridge.note(
    (n) =>
      n.method === "item/completed" &&
      n.params.turnId === own.params.turn.id &&
      n.params.item.id ===
        "limit:" + own.params.turn.id + ":five_hour:" + RESET,
  );
  assert.match(
    shown.params.item.text,
    new RegExp(
      "^Claude usage limit reached \\(five_hour\\)\\. Resets at " +
        new Date(RESET * 1000).toISOString().replaceAll(".", "\\."),
    ),
  );
});

test("a limit without rate-limit details still reports a usage wait", async () => {
  const { bridge, turn } = await start("plain");
  const done = await bridge.note(
    (n) => n.method === "turn/completed" && n.params.turn.id === turn,
  );
  assert.equal(done.params.turn.status, "failed");
  assert.deepEqual(done.params.turn.error, {
    message: LIMIT_TEXT,
    codexErrorInfo: "rateLimitExceeded",
  });
});
