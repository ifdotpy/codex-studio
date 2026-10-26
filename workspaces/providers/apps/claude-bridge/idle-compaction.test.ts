import type { Session } from "./session-state.ts";
import { test, expect } from "vitest";
import {
  cacheReport,
  promptPrefix,
  idleRefusal,
  compactionEnabled,
} from "./idle-compaction.ts";

test("reports the last request TTL and context, not cumulative result usage", () => {
  const s: Partial<Session> = {
    cwd: "/repo",
    model: "opus",
    developerInstructions: "rules",
  };
  const report = cacheReport(
    s,
    {
      input_tokens: 20,
      cache_read_input_tokens: 200000,
      cache_creation_input_tokens: 5000,
      output_tokens: 30,
      cache_creation: {
        ephemeral_1h_input_tokens: 5000,
        ephemeral_5m_input_tokens: 0,
      },
    },
    "response",
    1000,
  );
  expect(report).toMatchObject({
    ttl: "1h",
    contextTokens: 205050,
    lastActivityAt: 1000,
    requestId: "response",
    warm: true,
  });
  s.idleCache = report;
  expect(
    cacheReport(
      s,
      {
        input_tokens: 20,
        cache_read_input_tokens: 205000,
        cache_creation_input_tokens: 0,
        output_tokens: 5,
      },
      "next",
      2000,
    ).ttl,
  ).toBe("1h");
  expect(
    cacheReport(
      s,
      {
        input_tokens: 20,
        cache_creation_input_tokens: 50,
        cache_creation: { ephemeral_5m_input_tokens: 50 },
      },
      "five",
      3000,
    ).ttl,
  ).toBe("5m");
  expect(
    cacheReport(
      s,
      {
        cache_creation_input_tokens: 100,
        cache_creation: {
          ephemeral_5m_input_tokens: 50,
          ephemeral_1h_input_tokens: 50,
        },
      },
      "mixed",
      3000,
    ).ttl,
  ).toBe("5m");
  expect(cacheReport({}, { input_tokens: 200000 }, "uncached", 3000).warm).toBe(
    false,
  );
  expect(
    cacheReport({}, { cache_read_input_tokens: 200000 }, "unknown", 3000).ttl,
  ).toBe("unknown");
});

test("refuses a changed prefix, newer request, active query, and disabled auto compaction", () => {
  const s: Partial<Session> = { model: "opus" };
  s.idleCache = cacheReport(
    s,
    {
      cache_creation_input_tokens: 200000,
      cache_creation: { ephemeral_1h_input_tokens: 200000 },
    },
    "a",
    1000,
  );
  const expected = { requestId: "a", prefix: promptPrefix(s) };
  expect(idleRefusal(s, null, expected)).toBe(null);
  expect(idleRefusal({ ...s, model: "sonnet" }, null, expected)).toBe(
    "prefix_changed",
  );
  expect(idleRefusal(s, { turn: {} }, expected)).toBe("active");
  expect(idleRefusal(s, { tasks: new Set(["task"]) }, expected)).toBe("active");
  expect(idleRefusal(s, null, { ...expected, requestId: "old" })).toBe(
    "newer_request",
  );
});

test("the real bridge emits request-local cache telemetry and keeps it across a bridge restart", async () => {
  const { startBridge } = await import("./testing/bridge-harness.ts");

  const bridge = await startBridge(
    new URL("./fixtures/idle-compaction-sdk.ts", import.meta.url),
    "claude-idle-compaction-",
  );
  try {
    const turn = await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "input",
      input: [{ type: "text", text: "hello" }],
    });
    const notification = await bridge.note(
      (note) => note.method === "thread/idleCompaction/updated",
    );
    const report = notification.params.report;
    expect(report).toMatchObject({
      ttl: "1h",
      contextTokens: 210030,
      requestId: "response",
      warm: true,
    });
    await bridge.note(
      (note) =>
        note.method === "turn/completed" &&
        note.params.turn.id === turn.turn.id,
    );
    await bridge.restart();
    await expect(
      bridge.call("thread/compact/start", {
        threadId: bridge.thread,
        requestId: "idle",
        idleCompaction: { ...report, prefix: "changed" },
      }),
    ).rejects.toThrow("prefix_changed");
    const compact = await bridge.call("thread/compact/start", {
      threadId: bridge.thread,
      requestId: "idle",
      idleCompaction: report,
    });
    await bridge.note(
      (note) =>
        note.method === "turn/completed" &&
        note.params.turn.id === compact.turn.id,
    );
    expect(
      bridge.notes.filter(
        (note) => note.method === "thread/idleCompaction/updated",
      ),
    ).toHaveLength(0);
    expect(
      bridge.notes.filter(
        (note) =>
          note.method === "item/completed" &&
          note.params.item.type === "contextCompaction",
      ),
    ).toHaveLength(1);
  } finally {
    await bridge.close();
  }
});

test("compaction disable values follow Claude's environment rules", () => {
  expect(compactionEnabled({}, { DISABLE_COMPACT: "false" })).toBe(true);
  expect(compactionEnabled({}, { DISABLE_COMPACT: "0" })).toBe(true);
  expect(compactionEnabled({}, { DISABLE_COMPACT: "ON" })).toBe(false);
  expect(compactionEnabled({ autoCompactEnabled: false }, {})).toBe(false);
});
