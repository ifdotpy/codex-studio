import fs from "node:fs/promises";
import { afterEach, test } from "vitest";
import assert from "node:assert/strict";
import { startBridge } from "./testing/bridge-harness.ts";

const bridges: Awaited<ReturnType<typeof startBridge>>[] = [];
afterEach(async () => {
  for (const bridge of bridges.splice(0)) await bridge.close();
});
const sdk = await fs.readFile(
  new URL("./usage-events.fixture.mts", import.meta.url),
  "utf8",
);
async function run(events: readonly unknown[], model = "sonnet") {
  const bridge = await startBridge(sdk, "claude-usage-", {
    env: { STUDIO_FIXTURE_EVENTS: JSON.stringify(events) },
  });
  bridges.push(bridge);
  await bridge.call("turn/start", {
    threadId: bridge.thread,
    model,
    input: [{ type: "text", text: "Continue" }],
  });
  const done = await bridge.note((n) => n.method === "turn/completed");
  return { bridge, turn: done.params.turn };
}
const failed = {
  type: "result",
  subtype: "error_during_execution",
  is_error: true,
  errors: ["429 Too Many Requests"],
};

test("plain 429 without a rate limit event has a typed terminal cause", async () => {
  const { turn } = await run([
    {
      type: "assistant",
      error: "rate_limit",
      uuid: "error",
      message: {
        id: "error",
        model: "claude-sonnet-4-6",
        content: [{ type: "text", text: "429 Too Many Requests" }],
      },
    },
    failed,
  ]);
  assert.equal(turn.status, "failed");
  assert.equal(turn.error.codexErrorInfo, "rateLimitExceeded");
});

test("terminal result with only a plain HTTP 429 has a typed cause", async () => {
  const { turn } = await run([failed]);
  assert.equal(turn.error.codexErrorInfo, "rateLimitExceeded");
});

test("retry and fallback details survive as readable thread items", async () => {
  const { bridge } = await run([
    {
      type: "system",
      subtype: "api_retry",
      uuid: "retry-1",
      attempt: 2,
      max_retries: 5,
      retry_delay_ms: 1500,
      error_status: 429,
      error: "rate_limit",
    },
    {
      type: "system",
      subtype: "model_fallback",
      uuid: "fallback-1",
      original_model: "claude-opus-4-6",
      fallback_model: "claude-sonnet-4-6",
      content: "Changed model after the usage limit.",
    },
    {
      type: "system",
      subtype: "model_consent_fallback",
      uuid: "consent-1",
      original_model: "claude-opus-4-6",
      fallback_model: "claude-sonnet-4-6",
      choice: "session",
      content: "Changed model for this session.",
    },
    { type: "result", subtype: "success", result: "Done" },
  ]);
  const read = await bridge.call("thread/read", {
    threadId: bridge.thread,
    includeTurns: true,
  });
  const items = read.thread.turns.flatMap((turn) => turn.items);
  const retry = items.find((item) => item.id === "retry-1");
  assert.ok(retry, "retry item persisted");
  assert.match(retry.text, /2.*5/);
  assert.match(retry.text, /1500 ms/);
  assert.match(retry.text, /429/);
  for (const id of ["fallback-1", "consent-1"]) {
    const item = items.find((item) => item.id === id);
    assert.ok(item, id + " persisted");
    assert.match(item.text, /claude-opus-4-6/);
    assert.match(item.text, /claude-sonnet-4-6/);
    assert.ok(
      bridge.notes.some(
        (n) => n.method === "item/completed" && n.params.item.id === id,
      ),
    );
  }
});

test("an Opus rejection does not attach its reset to a Sonnet failure", async () => {
  const { turn } = await run([
    {
      type: "rate_limit_event",
      rate_limit_info: {
        rateLimitType: "seven_day_opus",
        status: "rejected",
        utilization: 1,
        resetsAt: 9000,
      },
    },
    {
      type: "result",
      subtype: "error_during_execution",
      is_error: true,
      errors: ["Unrelated failure"],
    },
  ]);
  assert.equal(turn.error.codexErrorInfo, undefined);
  assert.equal(turn.error.resetsAt, undefined);
});
