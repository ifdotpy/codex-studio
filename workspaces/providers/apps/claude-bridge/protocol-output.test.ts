import assert from "node:assert/strict";
import { createHash, randomUUID } from "node:crypto";
import fs from "node:fs/promises";
import path from "node:path";
import type { SDKAssistantMessage } from "@anthropic-ai/claude-agent-sdk";
import { test } from "vitest";
import { assistant, sdkResult, zeroResult } from "./usage-fixtures.ts";
import { startBridge } from "./testing/bridge-harness.ts";
import { canonicalJson, nativeDestination } from "./move.ts";
import { createSessionStore } from "./session-store.ts";
import type { MoveTurnOptions, Turn } from "./session-state.ts";

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
      const file = nativeDestination(
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

test("legacy receipts keep missing identity without emitting an incomplete item", async () => {
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
          note.method === "item/completed" && note.params.turnId === legacy.id,
      ),
      false,
    );
    const page = await bridge.call("thread/turns/items/list", {
      threadId: bridge.thread,
      turnId: legacy.id,
    });
    assert.equal(page.data[0]?.id, undefined);
    assert.equal(page.data[0]?.deliveryStatus, "delivered");
  } finally {
    await bridge.close();
  }
});
