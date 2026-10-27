import assert from "node:assert/strict";
import path from "node:path";
import { test } from "vitest";
import { startBridge } from "./testing/bridge-harness.ts";
import { createSessionStore } from "./session-store.ts";
import { assistant, sdkResult } from "./usage-fixtures.ts";
import type { SDKAssistantMessage } from "@anthropic-ai/claude-agent-sdk";

const pendingTool = {
  ...assistant,
  message: {
    ...assistant.message,
    content: [
      {
        type: "tool_use",
        id: "malformed-tool",
        name: "Bash",
        input: { command: "fixture", timeout: 100 },
      },
    ],
  },
} satisfies SDKAssistantMessage;

const { is_error: _isError, ...missingError } = sdkResult;
const malformedResults: Record<string, unknown>[] = [
  missingError,
  { ...sdkResult, is_error: null },
  { ...sdkResult, is_error: 0 },
  { ...sdkResult, subtype: "future_result" },
];
for (const [index, result] of malformedResults.entries()) {
  test(`malformed finite SDK result ${index} preserves an unresolved turn`, async () => {
    const bridge = await startBridge(
      new URL("./lifecycle-result-boundary.fixture.mts", import.meta.url),
      "claude-malformed-result-",
      {
        env: {
          STUDIO_FIXTURE_EVENTS: JSON.stringify([result]),
          STUDIO_FIXTURE_FINITE_RESULT: "1",
        },
      },
    );
    try {
      const { turn } = await bridge.call("turn/start", {
        threadId: bridge.thread,
        input: [{ type: "text", text: "Continue" }],
      });
      await bridge.note(
        "fixture/signal",
        (note) =>
          note.params.name === "result-processed-1" ||
          note.params.name === "result-closed",
      );
      assert.equal(
        bridge.notes.some((note) => note.method === "turn/completed"),
        false,
      );
      const { session } = await createSessionStore(
        path.join(bridge.root, "state"),
      ).readAll(bridge.thread);
      assert.equal(
        session.turns.find((saved) => saved.id === turn.id)?.status,
        "inProgress",
      );
      await bridge.restart();
      const resumed = await bridge.call("thread/read", {
        threadId: bridge.thread,
        includeTurns: true,
      });
      assert.equal(
        resumed.thread.turns.find((saved) => saved.id === turn.id)?.status,
        "uncertain",
      );
    } finally {
      await bridge.close();
    }
  });
}

test("a malformed result disarms deferred completion before delayed iterator close", async () => {
  const bridge = await startBridge(
    new URL("./lifecycle-result-boundary.fixture.mts", import.meta.url),
    "claude-malformed-deferred-",
    {
      clock: true,
      env: {
        STUDIO_FIXTURE_EVENTS: JSON.stringify([
          pendingTool,
          sdkResult,
          malformedResults[0],
        ]),
        STUDIO_FIXTURE_DEFER_RESULT: "1",
        STUDIO_FIXTURE_DELAY_RESULT_CLOSE: "1",
        STUDIO_CLAUDE_STEER_ECHO_GRACE_MS: "1000",
        STUDIO_CLAUDE_TOOL_STALL_GRACE_MS: "100",
        STUDIO_CLAUDE_STALL_INTERRUPT_WAIT_MS: "20",
        STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "1",
      },
    },
  );
  try {
    const { turn } = await bridge.call("turn/start", {
      threadId: bridge.thread,
      input: [{ type: "text", text: "Continue" }],
    });
    await bridge.call("turn/steer", {
      threadId: bridge.thread,
      expectedTurnId: turn.id,
      clientUserMessageId: "steer",
      input: [{ type: "text", text: "Next" }],
    });
    await bridge.note(
      "fixture/signal",
      (note) => note.params.name === "close-pending",
    );
    const outcomesBeforeClose = bridge.notes.filter(
      (note) => note.method === "item/completed",
    ).length;
    await bridge.advanceTime(1001);
    await bridge.advanceTime(21);
    await bridge.advanceTime(10);
    assert.equal(
      bridge.notes.filter((note) => note.method === "item/completed").length,
      outcomesBeforeClose,
    );
    const observed = await bridge.call("thread/read", {
      threadId: bridge.thread,
      includeTurns: true,
    });
    assert.ok(
      ["inProgress", "uncertain"].includes(
        observed.thread.turns.find((saved) => saved.id === turn.id)?.status ??
          "missing",
      ),
    );
    assert.equal(
      bridge.notes.some((note) => note.method === "turn/completed"),
      false,
    );
    await bridge.writeSignal("release-close");
    await bridge.note(
      "fixture/signal",
      (note) => note.params.name === "result-closed",
    );
    await bridge.restart();
    const resumed = await bridge.call("thread/read", {
      threadId: bridge.thread,
      includeTurns: true,
    });
    assert.equal(
      resumed.thread.turns.find((saved) => saved.id === turn.id)?.status,
      "uncertain",
    );
  } finally {
    await bridge.writeSignal("release-close");
    await bridge.close();
  }
});

for (const is_error of [false, true]) {
  test(`Stop still overrides a validated success result with is_error=${is_error}`, async () => {
    const bridge = await startBridge(
      new URL("./lifecycle-result-boundary.fixture.mts", import.meta.url),
      "claude-validated-stop-",
      {
        clock: true,
        env: {
          STUDIO_FIXTURE_EVENTS: JSON.stringify([{ ...sdkResult, is_error }]),
          STUDIO_FIXTURE_HOLD_RESULT: "1",
          STUDIO_FIXTURE_FINITE_RESULT: "1",
        },
      },
    );
    try {
      const { turn } = await bridge.call("turn/start", {
        threadId: bridge.thread,
        input: [{ type: "text", text: "Continue" }],
      });
      await bridge.note(
        "fixture/signal",
        (note) => note.params.name === "ready",
      );
      await bridge.call("turn/interrupt", {
        threadId: bridge.thread,
        turnId: turn.id,
      });
      await bridge.writeSignal("release-result");
      const done = await bridge.advanceUntil("turn/completed");
      assert.equal(done.params.turn.status, "interrupted");
    } finally {
      await bridge.close();
    }
  });
}

test("Stop does not invent an outcome from a malformed result", async () => {
  const bridge = await startBridge(
    new URL("./lifecycle-result-boundary.fixture.mts", import.meta.url),
    "claude-malformed-stop-",
    {
      clock: true,
      env: {
        STUDIO_FIXTURE_EVENTS: JSON.stringify([malformedResults[0]]),
        STUDIO_FIXTURE_HOLD_RESULT: "1",
        STUDIO_FIXTURE_FINITE_RESULT: "1",
      },
    },
  );
  try {
    const { turn } = await bridge.call("turn/start", {
      threadId: bridge.thread,
      input: [{ type: "text", text: "Continue" }],
    });
    await bridge.note("fixture/signal", (note) => note.params.name === "ready");
    await bridge.call("turn/interrupt", {
      threadId: bridge.thread,
      turnId: turn.id,
    });
    await bridge.writeSignal("release-result");
    await bridge.note(
      "fixture/signal",
      (note) => note.params.name === "result-closed",
    );
    await bridge.advanceTime(1001);
    const observed = await bridge.call("thread/read", {
      threadId: bridge.thread,
      includeTurns: true,
    });
    assert.ok(
      ["inProgress", "uncertain"].includes(
        observed.thread.turns.find((saved) => saved.id === turn.id)?.status ??
          "missing",
      ),
    );
    assert.equal(
      bridge.notes.some((note) => note.method === "turn/completed"),
      false,
    );
  } finally {
    await bridge.close();
  }
});

test("a malformed result retires recovery already waiting for process exit", async () => {
  const bridge = await startBridge(
    new URL("./lifecycle-result-boundary.fixture.mts", import.meta.url),
    "claude-malformed-recovery-",
    {
      clock: true,
      env: {
        STUDIO_FIXTURE_EVENTS: JSON.stringify([
          pendingTool,
          malformedResults[0],
        ]),
        STUDIO_FIXTURE_HOLD_TOOL_RESULT: "1",
        STUDIO_FIXTURE_DELAY_RESULT_CLOSE: "1",
        STUDIO_CLAUDE_TOOL_STALL_GRACE_MS: "100",
        STUDIO_CLAUDE_STALL_INTERRUPT_WAIT_MS: "20",
        STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "200",
      },
    },
  );
  try {
    const { turn } = await bridge.call("turn/start", {
      threadId: bridge.thread,
      input: [{ type: "text", text: "Continue" }],
    });
    await bridge.note(
      "fixture/signal",
      (note) => note.params.name === "tool-ready",
    );
    await bridge.advanceTime(201);
    await bridge.note(
      "fixture/signal",
      (note) => note.params.name === "interrupt",
    );
    await bridge.advanceTime(21);
    await bridge.note(
      "fixture/signal",
      (note) => note.params.name === "query-close",
    );
    await bridge.writeSignal("release-result");
    await bridge.note(
      "fixture/signal",
      (note) => note.params.name === "close-pending",
    );
    const outcomesBeforeClose = bridge.notes.filter(
      (note) => note.method === "item/completed",
    ).length;
    await bridge.advanceTime(201);
    // The exit deadline resumes recovery through one setImmediate before finalization.
    await bridge.advanceTime(1);
    const observed = await bridge.call("thread/read", {
      threadId: bridge.thread,
      includeTurns: true,
    });
    assert.equal(
      observed.thread.turns.find((saved) => saved.id === turn.id)?.status,
      "inProgress",
    );
    assert.equal(
      bridge.notes.some((note) => note.method === "turn/completed"),
      false,
    );
    assert.equal(
      bridge.notes.filter((note) => note.method === "item/completed").length,
      outcomesBeforeClose,
    );
    assert.deepEqual((await bridge.call("thread/loaded/list", {})).data, []);
    await bridge.writeSignal("release-close");
    await bridge.note(
      "fixture/signal",
      (note) => note.params.name === "result-closed",
    );
    await bridge.restart();
    const resumed = await bridge.call("thread/read", {
      threadId: bridge.thread,
      includeTurns: true,
    });
    assert.equal(
      resumed.thread.turns.find((saved) => saved.id === turn.id)?.status,
      "uncertain",
    );
  } finally {
    await bridge.writeSignal("release-result");
    await bridge.writeSignal("release-close");
    await bridge.close();
  }
});
