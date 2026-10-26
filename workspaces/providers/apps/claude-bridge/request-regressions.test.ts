import { test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import path from "node:path";
import { createInterface } from "node:readline";
import { startBridge } from "./testing/bridge-harness.ts";
import { waitForFile } from "./testing/file-signals.ts";
import type { BridgeHarness } from "./testing/bridge-harness.ts";

function rawReplies(bridge: BridgeHarness, requests: object[]) {
  assert.ok(bridge.process.stdout);
  const lines = createInterface({ input: bridge.process.stdout });
  return new Promise<Record<string, unknown>[]>((resolve, reject) => {
    const replies: Record<string, unknown>[] = [];
    lines.on("line", (line) => {
      try {
        const value: unknown = JSON.parse(line);
        if (!value || typeof value !== "object" || !("id" in value)) return;
        if (typeof value.id !== "string" || !value.id.startsWith("raw-"))
          return;
        replies.push({ ...value });
        if (replies.length === requests.length) {
          lines.close();
          bridge.process.stdout?.resume();
          resolve(replies);
        }
      } catch (error) {
        lines.close();
        bridge.process.stdout?.resume();
        reject(error);
      }
    });
    bridge.process.stdin?.write(
      requests.map((r) => JSON.stringify(r)).join("\n") + "\n",
    );
  });
}

const fixture = new URL("./stop-receipts.fixture.mts", import.meta.url);

// Boundary hardening: these deliberately malformed envelopes are test inputs,
// not evidence that the Rust caller produces them.
for (const malformed of [
  {},
  { result: null, error: { code: -1, message: "ambiguous" } },
  { error: null },
  { error: { code: -1 } },
]) {
  test(`a malformed callback keeps its waiter: ${JSON.stringify(malformed)}`, async () => {
    const bridge = await startBridge(
      new URL("./callback.fixture.mts", import.meta.url),
    );
    try {
      await bridge.call("turn/start", {
        threadId: bridge.thread,
        input: [{ type: "text", text: "callback" }],
      });
      const request = await bridge.note("mcpServer/elicitation/request");
      bridge.process.stdin?.write(
        JSON.stringify({ id: request.id, ...malformed }) + "\n",
      );
      // A subsequent RPC is an ordered stdin barrier, without a timing sleep.
      await bridge.call("thread/loaded/list", {});
      assert.equal(
        bridge.notes.some((note) => note.method === "fixture/callback"),
        false,
      );
      bridge.process.stdin?.write(
        JSON.stringify({
          id: request.id,
          result: null,
          fixtureCallback: true,
        }) + "\n",
      );
      assert.deepEqual((await bridge.note("fixture/callback")).params, {
        value: null,
        decodes: 1,
      });
    } finally {
      await bridge.close();
    }
  });
}

test("callback success and errors decode once and ignore unmatched ids", async () => {
  for (const reply of [
    { result: null },
    { result: { action: "accept", content: {} } },
    { error: { code: -1, message: "fixture refusal" } },
  ]) {
    const bridge = await startBridge(
      new URL("./callback.fixture.mts", import.meta.url),
    );
    try {
      await bridge.call("turn/start", {
        threadId: bridge.thread,
        input: [{ type: "text", text: "callback" }],
      });
      const request = await bridge.note("mcpServer/elicitation/request");
      bridge.process.stdin?.write(
        JSON.stringify({ id: { invalid: true }, ...reply }) + "\n",
      );
      bridge.process.stdin?.write(
        JSON.stringify({ id: "unknown", ...reply }) + "\n",
      );
      await bridge.call("thread/loaded/list", {});
      assert.equal(
        bridge.notes.some((note) => note.method === "fixture/callback"),
        false,
      );
      bridge.process.stdin?.write(
        JSON.stringify({ id: request.id, ...reply, fixtureCallback: true }) +
          "\n",
      );
      assert.deepEqual(
        (await bridge.note("fixture/callback")).params,
        reply.error
          ? { error: reply.error.message, decodes: 1 }
          : { value: reply.result, decodes: 1 },
      );
    } finally {
      await bridge.close();
    }
  }
});

test("requests without params retain the empty parameter default", async () => {
  const bridge = await startBridge(fixture);
  try {
    const [reply] = await rawReplies(bridge, [
      { id: "raw-list", method: "thread/list" },
    ]);
    assert.ok(reply);
    assert.equal(reply.error, undefined);
    assert.ok(reply.result);
  } finally {
    await bridge.close();
  }
});

test("method dispatch preserves initialization and explicit unsupported method refusals", async () => {
  const bridge = await startBridge(fixture);
  try {
    assert.deepEqual(await bridge.call("initialize", {}), {
      userAgent: "studio-claude-bridge",
      platform: process.platform,
      capabilities: {
        claudeVersion: 23,
        passiveAccountRead: true,
        activityRead: true,
      },
    });
    assert.deepEqual(await bridge.call("initialized", {}), {});
    await assert.rejects(
      bridge.call("skills/extraRoots/set", {}),
      /Claude uses its native skills and MCP configuration/,
    );
    const [reply] = await rawReplies(bridge, [
      { id: "raw-unknown", method: "future/method", params: {} },
    ]);
    assert.deepEqual(reply?.error, {
      code: -32000,
      message: "Claude does not support future/method",
    });
  } finally {
    await bridge.close();
  }
});

test("concurrent interrupts revalidate a reserved turn after session lookup", async () => {
  const bridge = await startBridge(fixture, "claude-interrupt-race-", {
    clock: true,
  });
  let pending: Promise<unknown> | undefined;
  try {
    const warm = await bridge.call("turn/start", {
      threadId: bridge.thread,
      input: [{ type: "text", text: "warm" }],
    });
    await bridge.note(
      "turn/completed",
      (note) => note.params.turn.id === warm.turn.id,
    );
    await fs.writeFile(path.join(bridge.root, ".hold-save"), "");
    pending = bridge
      .call("turn/start", {
        threadId: bridge.thread,
        clientUserMessageId: "blocked-save",
        input: [{ type: "text", text: "/clear" }],
      })
      .catch((error: unknown) => error);
    await waitForFile(path.join(bridge.root, ".save-held"));
    const replies = await rawReplies(
      bridge,
      [1, 2].map((id) => ({
        id: `raw-stop-${id}`,
        method: "turn/interrupt",
        params: { threadId: bridge.thread },
      })),
    );
    assert.equal(replies.filter((reply) => "result" in reply).length, 1);
    const failure = replies.find((reply) => "error" in reply)?.error;
    assert.ok(failure && typeof failure === "object" && "message" in failure);
    assert.equal(failure.message, "no active turn to interrupt");
  } finally {
    await fs.writeFile(path.join(bridge.root, ".release-save"), "");
    await pending;
    await bridge.close();
  }
});
