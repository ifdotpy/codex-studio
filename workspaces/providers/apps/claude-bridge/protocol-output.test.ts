import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import fs from "node:fs/promises";
import path from "node:path";
import type {
  SDKAssistantMessage,
  SDKMessage,
} from "@anthropic-ai/claude-agent-sdk";
import { test } from "vitest";
import { nativeItem } from "./features.ts";
import { assistant, sdkResult, zeroResult } from "./usage-fixtures.ts";
import { startBridge } from "./testing/bridge-harness.ts";
import { canonicalJson, nativeDestination } from "./move.ts";
import { createSessionStore } from "./session-store.ts";
import type { MoveTurnOptions, Turn } from "./session-state.ts";

test("native tool payloads stay opaque without invented command or file strings", () => {
  const arguments_ = { command: { future: true } };
  const command = nativeItem(
    { type: "tool_use", id: "opaque", name: "Bash", input: arguments_ },
    "/work",
  );
  assert.ok(command.type === "commandExecution");
  assert.equal(command.command, arguments_.command);
  assert.equal(command.arguments, arguments_);
  const file = nativeItem(
    { type: "tool_use", id: "file", name: "Write", input: {} },
    "/work",
  );
  assert.ok(file.type === "fileChange");
  assert.deepEqual(JSON.parse(JSON.stringify(file.changes)), [
    { kind: { type: "add" }, diff: "-\n+" },
  ]);
  const nullCommand = nativeItem(
    {
      type: "tool_use",
      id: "null-command",
      name: "Bash",
      input: { command: null },
    },
    "/work",
  );
  assert.ok(nullCommand.type === "commandExecution");
  assert.equal(JSON.parse(JSON.stringify(nullCommand)).command, null);
  const nullPath = nativeItem(
    {
      type: "tool_use",
      id: "null-path",
      name: "Write",
      input: { file_path: null, notebook_path: null },
    },
    "/work",
  );
  assert.ok(nullPath.type === "fileChange");
  assert.deepEqual(JSON.parse(JSON.stringify(nullPath.changes)), [
    { path: null, kind: { type: "add" }, diff: "-\n+" },
  ]);
});

test("a native Bash result without content preserves omitted wire fields", async () => {
  const events = [
    {
      ...assistant,
      message: {
        ...assistant.message,
        content: [
          { type: "tool_use", id: "empty-bash", name: "Bash", input: {} },
        ],
      },
    },
    {
      type: "user",
      uuid: randomUUID(),
      session_id: "fixture",
      parent_tool_use_id: null,
      message: {
        role: "user",
        content: [{ type: "tool_result", tool_use_id: "empty-bash" }],
      },
    },
    sdkResult,
  ] satisfies SDKMessage[];
  const bridge = await startBridge(
    new URL("./usage-events.fixture.mts", import.meta.url),
    "claude-protocol-empty-result-",
    { env: { STUDIO_FIXTURE_EVENTS: JSON.stringify(events) } },
  );
  try {
    const { turn } = await bridge.call("turn/start", {
      threadId: bridge.thread,
      input: [{ type: "text", text: "Continue" }],
    });
    const started = await bridge.note(
      "item/started",
      (note) => note.params.item.id === "empty-bash",
    );
    const completed = await bridge.note(
      "item/completed",
      (note) => note.params.item.id === "empty-bash",
    );
    await bridge.note(
      "turn/completed",
      (note) => note.params.turn.id === turn.id,
    );
    assert.equal(Object.hasOwn(started.params.item, "command"), false);
    assert.equal(started.params.item.aggregatedOutput, "");
    assert.equal(Object.hasOwn(completed.params.item, "command"), false);
    assert.equal(
      Object.hasOwn(completed.params.item, "aggregatedOutput"),
      false,
    );
    assert.deepEqual(completed.params.item.result, {});
    assert.equal(completed.params.item.exitCode, 0);
  } finally {
    await bridge.close();
  }
});

test("SDK native blocks reach fresh item producers without changing output order", async () => {
  const metadata = {
    trigger: "auto",
    pre_tokens: 120,
    post_tokens: 50,
  } as const;
  const events = [
    {
      ...assistant,
      message: {
        ...assistant.message,
        content: [
          { type: "text", text: "Working", citations: null },
          {
            type: "tool_use",
            id: "bash",
            name: "Bash",
            input: { command: "pwd" },
          },
          {
            type: "tool_use",
            id: "write",
            name: "Write",
            input: { file_path: "/work/file", content: "body" },
          },
          {
            type: "tool_use",
            id: "other",
            name: "ExternalTool",
            input: { future: [1, { key: true }] },
          },
        ],
      },
    },
    {
      type: "user",
      uuid: randomUUID(),
      session_id: "fixture",
      parent_tool_use_id: null,
      message: {
        role: "user",
        content: [
          { type: "tool_result", tool_use_id: "bash", content: "done" },
          { type: "tool_result", tool_use_id: "write", content: "saved" },
          { type: "tool_result", tool_use_id: "other", content: "returned" },
        ],
      },
    },
    {
      type: "system",
      subtype: "compact_boundary",
      uuid: randomUUID(),
      session_id: "fixture",
      compact_metadata: metadata,
    },
    sdkResult,
  ] satisfies SDKMessage[];
  const bridge = await startBridge(
    new URL("./usage-events.fixture.mts", import.meta.url),
    "claude-protocol-items-",
    { env: { STUDIO_FIXTURE_EVENTS: JSON.stringify(events) } },
  );
  try {
    const { turn } = await bridge.call("turn/start", {
      threadId: bridge.thread,
      input: [{ type: "text", text: "Continue" }],
    });
    await bridge.note(
      "turn/completed",
      (note) => note.params.turn.id === turn.id,
    );
    const started = bridge.notes.filter(
      (note) => note.method === "item/started",
    );
    assert.deepEqual(
      started.map((note) => note.params.item.type),
      ["commandExecution", "fileChange", "mcpToolCall"],
    );
    assert.deepEqual(started[2]?.params.item.arguments, {
      future: [1, { key: true }],
    });
    const completed = bridge.notes.filter(
      (note) => note.method === "item/completed",
    );
    assert.deepEqual(
      completed.map((note) => note.params.item.type),
      [
        "agentMessage",
        "commandExecution",
        "fileChange",
        "mcpToolCall",
        "contextCompaction",
        "agentMessage",
      ],
    );
    const command = completed.find((note) => note.params.item.id === "bash")
      ?.params.item;
    assert.equal(command?.aggregatedOutput, "done");
    assert.equal(command?.exitCode, 0);
    const compact = completed.find(
      (note) => note.params.item.type === "contextCompaction",
    )?.params.item;
    assert.deepEqual(compact?.compactionMetadata, metadata);
    assert.equal(completed.at(-1)?.params.item.phase, "final_answer");
  } finally {
    await bridge.close();
  }
});

test("native plan updates preserve raw todo fields at the wire boundary", async () => {
  const message = {
    ...assistant,
    uuid: randomUUID(),
    message: {
      ...assistant.message,
      content: [
        {
          type: "tool_use",
          id: "todo-call",
          name: "TodoWrite",
          input: { todos: [{ content: 42, status: { future: true } }, {}] },
        },
      ],
    },
  } satisfies SDKAssistantMessage;
  const bridge = await startBridge(
    new URL("./usage-events.fixture.mts", import.meta.url),
    "claude-protocol-plan-",
    {
      env: { STUDIO_FIXTURE_EVENTS: JSON.stringify([message, zeroResult]) },
    },
  );
  try {
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      input: [{ type: "text", text: "Continue" }],
    });
    const note = await bridge.note("turn/plan/updated");
    assert.deepEqual(note.params.plan, [
      { step: 42, status: { future: true } },
      {},
    ]);
  } finally {
    await bridge.close();
  }
});

test("producer null model and userMcp preserve fresh output and move override hashes", async () => {
  const cases: MoveTurnOptions[] = [
    {},
    { effort: "high", serviceTier: "priority" },
  ];
  for (const overrides of cases) {
    const bridge = await startBridge(
      new URL("./protocol-output.fixture.mts", import.meta.url),
      "claude-protocol-move-",
      {
        thread: { userMcp: false },
        env: { STUDIO_FIXTURE_EVENTS: JSON.stringify([assistant, sdkResult]) },
      },
    );
    try {
      const { turn } = await bridge.call("turn/start", {
        threadId: bridge.thread,
        model: null,
        input: [{ type: "text", text: "Continue" }],
        ...overrides,
      });
      const completed = await bridge.note(
        "turn/completed",
        (note) => note.params.turn.id === turn.id,
      );
      assert.equal(completed.params.turn.status, "completed");
      const items = bridge.notes.filter(
        (note) => note.method === "item/completed",
      );
      assert.ok(items.length > 0);
      for (const note of items) {
        assert.equal(typeof note.params.item.id, "string");
        assert.equal(typeof note.params.item.type, "string");
      }
      const { session } = await createSessionStore(
        path.join(bridge.root, "state"),
      ).readAll(bridge.thread);
      assert.equal(session.userMcp, false);
      assert.deepEqual(session.moveTurnOptions, overrides);
      const file = await nativeDestination(
        session.nativeId || session.id,
        session.cwd,
        path.join(bridge.root, "native-config"),
      );
      await fs.mkdir(path.dirname(file), { recursive: true });
      await fs.writeFile(
        file,
        JSON.stringify({
          type: "attachment",
          sessionId: session.nativeId || session.id,
          attachment: {
            type: "prompt_snapshot",
            systemPrompt: ["Fixture"],
            tools: [],
            inlineTools: false,
          },
        }) + "\n",
      );
      const proof = await bridge.call("claude/moveProof", {
        threadId: bridge.thread,
      });
      assert.deepEqual(proof.session.moveTurnOptions, overrides);
      assert.equal(
        proof.options.settings.fastMode,
        overrides.serviceTier === "priority",
      );
      assert.equal(proof.options.effort, overrides.effort || null);
      assert.equal(
        Object.hasOwn(proof.options.settings, "effortLevel"),
        overrides.effort !== undefined,
      );
      assert.equal(proof.options.settings.effortLevel, overrides.effort);
      assert.equal(
        proof.optionsHash,
        createHash("sha256").update(canonicalJson(proof.options)).digest("hex"),
      );
    } finally {
      await bridge.close();
    }
  }
});

test.each([undefined, "legacy-input"])(
  "legacy receipts preserve identity %s without fresh item requirements",
  async (legacyInputId) => {
    const legacyNativeId = randomUUID();
    const bridge = await startBridge(
      new URL("./usage-events.fixture.mts", import.meta.url),
      "claude-protocol-legacy-",
      {
        env: {
          STUDIO_FIXTURE_EVENTS: JSON.stringify([
            { type: "user", isReplay: true, uuid: legacyNativeId },
            sdkResult,
          ]),
        },
      },
    );
    try {
      await bridge.call("thread/unsubscribe", { threadId: bridge.thread });
      const store = createSessionStore(path.join(bridge.root, "state"));
      const { session } = await store.readAll(bridge.thread);
      const legacy = {
        id: "legacy-turn",
        status: "completed",
        items: [
          {
            id: legacyInputId,
            type: "userMessage",
            nativeId: legacyNativeId,
            delivery: "steer",
            deliveryStatus: "submitted",
          },
        ],
      } satisfies Turn;
      session.turns.push(legacy);
      await store.persist(session);
      await store.drain();
      await bridge.restart();
      const { turn } = await bridge.call("turn/start", {
        threadId: bridge.thread,
        input: [{ type: "text", text: "Continue" }],
      });
      await bridge.note(
        "turn/completed",
        (note) => note.params.turn.id === turn.id,
      );
      assert.equal(
        bridge.notes.some(
          (note) =>
            note.method === "item/completed" &&
            note.params.turnId === legacy.id,
        ),
        legacyInputId !== undefined,
      );
      if (legacyInputId !== undefined) {
        const emitted = await bridge.note(
          "item/completed",
          (note) => note.params.turnId === legacy.id,
        );
        assert.deepEqual(emitted.params.item, {
          id: legacyInputId,
          type: "userMessage",
          nativeId: legacyNativeId,
          delivery: "steer",
          deliveryStatus: "delivered",
          clientId: legacyInputId,
        });
        assert.equal(Object.hasOwn(emitted.params.item, "content"), false);
      }
      const page = await bridge.call("thread/turns/items/list", {
        threadId: bridge.thread,
        turnId: legacy.id,
      });
      assert.equal(page.data[0]?.id, legacyInputId);
      assert.equal(page.data[0]?.deliveryStatus, "delivered");
    } finally {
      await bridge.close();
    }
  },
);

test("SDK success with is_error settles the actual bridge turn as failed", async () => {
  const result = {
    ...sdkResult,
    is_error: true,
  } satisfies import("@anthropic-ai/claude-agent-sdk").SDKResultMessage;
  const bridge = await startBridge(
    new URL("./usage-events.fixture.mts", import.meta.url),
    "claude-result-error-",
    {
      env: { STUDIO_FIXTURE_EVENTS: JSON.stringify([assistant, result]) },
    },
  );
  try {
    const { turn } = await bridge.call("turn/start", {
      threadId: bridge.thread,
      input: [{ type: "text", text: "Continue" }],
    });
    const done = await bridge.note(
      "turn/completed",
      (note) => note.params.turn.id === turn.id,
    );
    assert.equal(done.params.threadId, bridge.thread);
    assert.notEqual(done.params.turn.id, result.uuid);
    assert.equal(done.params.turn.status, "failed");
    const { session } = await createSessionStore(
      path.join(bridge.root, "state"),
    ).readAll(bridge.thread);
    assert.equal(
      session.turns.find((saved) => saved.id === turn.id)?.status,
      "failed",
    );
  } finally {
    await bridge.close();
  }
});
