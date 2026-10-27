import type { Session, Turn } from "./session-state.ts";
import type {
  SessionStoreEntry,
  SDKUserMessage,
  SDKAssistantMessage,
} from "@anthropic-ai/claude-agent-sdk";
import type { NativeHistoryFilesystem } from "./native-history.ts";
type Faults = {
  unreadable?: boolean;
  torn?: boolean;
  changed?: boolean;
  partial?: boolean;
};
import assert from "node:assert/strict";
import { test } from "vitest";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fixtureSessionApi } from "./testing/history-fixture.ts";
import { createSessionStore } from "./session-store.ts";
import { startBridge } from "./testing/bridge-harness.ts";

const sessionId = "11111111-1111-4111-8111-111111111111";
const userId = "22222222-2222-4222-8222-222222222222";
const replyId = "33333333-3333-4333-8333-333333333333";
const nextId = "44444444-4444-4444-8444-444444444444";
const cwd = "/native/project";
const row = (
  type: "user" | "assistant",
  uuid: string,
  parentUuid: string | null,
  content: SDKUserMessage["message"]["content"],
  extra: Partial<SessionStoreEntry> = {},
): SessionStoreEntry & {
  message: SDKUserMessage["message"] &
    Partial<Pick<SDKAssistantMessage["message"], "id" | "stop_reason">>;
} => ({
  type,
  uuid,
  parentUuid,
  sessionId,
  cwd,
  isSidechain: false,
  message: {
    id: "msg-" + uuid,
    role: type,
    content,
  },
  ...extra,
});
const user = () => row("user", userId, null, "Finish the task");
const reply = () =>
  row(
    "assistant",
    replyId,
    userId,
    [{ type: "text", text: "The task is complete." }],
    {
      message: {
        id: "msg-final",
        role: "assistant",
        stop_reason: "end_turn",
        content: [{ type: "text", text: "The task is complete." }],
      },
    },
  );

// The native transcript exists only in memory. File identity and replacement
// checks exercise the production reader without a Claude process or account.
function nativeFilesystem(
  rows: SessionStoreEntry[],
  { unreadable = false, torn = false, changed = false }: Faults = {},
): NativeHistoryFilesystem {
  const raw = Buffer.from(
    rows.map((value) => JSON.stringify(value) + "\n").join("") +
      (torn ? '{"type":' : ""),
  );
  let reads = 0;
  const stat = () => ({
    size: raw.length,
    dev: 1,
    ino: 2,
    mtimeMs: 3,
    isFile: () => true,
    isSymbolicLink: () => false,
  });
  return {
    async realpath(file) {
      return file;
    },
    async lstat() {
      return stat();
    },
    async open(file) {
      assert.equal(file, `/config/projects/-native-project/${sessionId}.jsonl`);
      if (unreadable) throw new Error("EACCES");
      return {
        async stat() {
          return stat();
        },
        async read(buffer, offset, length, position) {
          raw.copy(buffer, offset, position, position + length);
          if (changed && ++reads > 1)
            buffer.writeUInt8(buffer.readUInt8(0) ^ 1, 0);
          return { bytesRead: length };
        },
        async close() {},
      };
    },
  };
}
async function loadStale(
  rows: SessionStoreEntry[],
  options: Faults,
  check: (turn: Turn) => void | Promise<void>,
) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-restart-"));
  try {
    const original: Session & { turns: [Turn] } = {
      id: sessionId,
      cwd,
      turns: [
        {
          id: "turn",
          status: "inProgress",
          items: [
            {
              id: "input",
              type: "userMessage",
              nativeId: userId,
              content: [{ type: "text", text: "Finish the task" }],
            },
          ],
        },
      ],
    };
    if (options.partial)
      original.turns[0].items.push({
        id: "msg-final",
        type: "agentMessage",
        text: "The task",
        phase: "commentary",
      });
    await createSessionStore(root).persist(original);
    // Keep the existing mirror journal repair path active during reconciliation.
    await fs.appendFile(
      path.join(root, "sessions", sessionId + ".jsonl"),
      '{"sequence":',
    );
    const store = createSessionStore(root, {
      nativeHistory: {
        configDir: "/config",
        filesystem: nativeFilesystem(rows, options),
        sessionApi: fixtureSessionApi(rows, { sessionId, cwd }),
      },
    });
    const restored = await store.get(sessionId);
    const [restoredTurn] = restored.turns;
    assert.ok(restoredTurn);
    await check(restoredTurn);
    await store.persist(restored);
    const again = await createSessionStore(root).get(sessionId);
    assert.deepEqual(
      again,
      restored,
      "reconciled result survives another restart",
    );
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
}

test("restart restores the final native assistant reply", async () => {
  await loadStale([user(), reply()], {}, (turn) => {
    assert.equal(turn.status, "completed");
    assert.equal(turn.error, undefined);
    assert.deepEqual(turn.items.at(-1), {
      id: "msg-final",
      nativeId: replyId,
      type: "agentMessage",
      text: "The task is complete.",
      phase: "final_answer",
    });
  });
});
const attachment = (parentUuid: string): SessionStoreEntry => ({
  type: "attachment",
  uuid: nextId,
  parentUuid,
  sessionId,
  cwd,
  isSidechain: false,
  attachment: {
    type: "deferred_tools_delta",
    addedNames: ["mcp__fixture__echo"],
    addedLines: ["mcp__fixture__echo"],
    removedNames: [],
  },
});
const costState: SessionStoreEntry = { type: "cost-state", totalCost: 0 };
for (const trailing of [false, true])
  test(`restart restores the final reply through a known attachment${trailing ? " and trailing metadata" : ""}`, async () => {
    await loadStale(
      [
        user(),
        attachment(userId),
        { ...reply(), parentUuid: nextId },
        ...(trailing ? [costState] : []),
      ],
      {},
      (turn) => {
        assert.equal(turn.status, "completed");
        assert.equal(turn.items.at(-1)?.text, "The task is complete.");
      },
    );
  });

test("restart confirms a native turn with no assistant reply as interrupted", async () => {
  await loadStale([user()], {}, (turn) =>
    assert.equal(turn.status, "interrupted"),
  );
});
test("restart confirms a pending native tool call as interrupted", async () => {
  await loadStale(
    [
      user(),
      row("assistant", replyId, userId, [
        {
          type: "tool_use",
          id: "tool",
          name: "Bash",
          input: { command: "pwd" },
        },
      ]),
    ],
    {},
    (turn) => assert.equal(turn.status, "interrupted"),
  );
});
test("known non-input records cannot turn input coverage into a final answer", async () => {
  await loadStale([user(), attachment(userId), costState], {}, (turn) => {
    assert.equal(turn.status, "interrupted");
    assert.equal(
      turn.items.some((item) => item.phase === "final_answer"),
      false,
    );
  });
});
test("known non-input records preserve the pending-tool outcome", async () => {
  await loadStale(
    [
      user(),
      attachment(userId),
      row("assistant", replyId, nextId, [
        { type: "tool_use", id: "tool", name: "Bash", input: {} },
      ]),
      costState,
    ],
    {},
    (turn) => assert.equal(turn.status, "interrupted"),
  );
});
for (const [name, rows, options] of [
  [
    "API error after a known attachment",
    [
      user(),
      attachment(userId),
      { ...reply(), parentUuid: nextId, isApiErrorMessage: true },
      costState,
    ],
    {},
  ],
  [
    "unknown attachment",
    [
      user(),
      {
        ...attachment(userId),
        attachment: { type: "future-input", content: "instruction" },
      },
      { ...reply(), parentUuid: nextId },
    ],
    {},
  ],
  [
    "broken attachment ancestry",
    [user(), attachment(replyId), { ...reply(), parentUuid: nextId }],
    {},
  ],
  [
    "sibling assistant reply",
    [
      user(),
      row("assistant", nextId, userId, [
        { type: "text", text: "Earlier output" },
      ]),
      reply(),
    ],
    {},
  ],
  [
    "valid compaction boundary after final reply",
    [
      user(),
      reply(),
      {
        type: "system",
        uuid: nextId,
        parentUuid: replyId,
        sessionId,
        cwd,
        isSidechain: false,
        subtype: "compact_boundary",
      },
    ],
    {},
  ],
  [
    "unresolved tool before text reply",
    [
      user(),
      row("assistant", nextId, userId, [
        { type: "tool_use", id: "tool", name: "Bash", input: {} },
      ]),
      { ...reply(), parentUuid: nextId },
      costState,
    ],
    {},
  ],
  ["sidechain input", [{ ...user(), isSidechain: true }, reply()], {}],
  [
    "compacted turn",
    [user(), { type: "system", subtype: "compact_boundary" }, reply()],
    {},
  ],
  [
    "unknown stop reason",
    [
      user(),
      {
        ...reply(),
        message: { ...reply().message, stop_reason: "max_tokens" },
      },
    ],
    {},
  ],
  [
    "malformed content",
    [user(), { ...reply(), message: { ...reply().message, content: [null] } }],
    {},
  ],
  [
    "terminal tool receipt",
    [
      user(),
      row("assistant", replyId, userId, [
        { type: "tool_use", id: "tool", name: "ExitPlanMode", input: {} },
      ]),
      row(
        "user",
        nextId,
        replyId,
        [{ type: "tool_result", tool_use_id: "tool", content: "Stopped" }],
        { toolDenialEndsTurn: true },
      ),
    ],
    {},
  ],

  ["unreadable transcript", [user(), reply()], { unreadable: true }],
  ["torn native tail", [user(), reply()], { torn: true }],
  ["changed native file", [user(), reply()], { changed: true }],
  ["missing input identity", [reply()], {}],
  ["foreign branch reply", [user(), { ...reply(), parentUuid: nextId }], {}],
  [
    "later unowned user input",
    [user(), reply(), row("user", nextId, replyId, "Another task")],
    {},
  ],
] satisfies [string, SessionStoreEntry[], Faults][])
  test(`restart keeps ${name} uncertain`, async () => {
    await loadStale(rows, options, (turn) => {
      assert.equal(turn.status, "uncertain");
      assert.match(turn.error!.message, /could not confirm/);
    });
  });

test("thread/read and resume deliver the recovered final reply without a model query", async () => {
  const bridge = await startBridge(
    new URL("./fixtures/history-readonly-sdk.ts", import.meta.url),
    "claude-restart-wire-",
  );
  try {
    const session: Session = {
      id: sessionId,
      cwd: bridge.root,
      turns: [
        {
          id: "turn",
          status: "inProgress",
          items: [
            {
              id: "input",
              type: "userMessage",
              nativeId: userId,
              content: [{ type: "text", text: "Finish the task" }],
            },
          ],
        },
      ],
    };
    await createSessionStore(path.join(bridge.root, "state")).persist(session);
    const folder = path.join(
      bridge.root,
      "native-config",
      "projects",
      bridge.root.replace(/[^a-zA-Z0-9]/g, "-"),
    );
    await fs.mkdir(folder, { recursive: true });
    await fs.writeFile(
      path.join(folder, sessionId + ".jsonl"),
      [
        user(),
        attachment(userId),
        { ...reply(), parentUuid: nextId },
        costState,
      ]
        .map((entry) => JSON.stringify({ ...entry, cwd: bridge.root }) + "\n")
        .join(""),
    );
    for (const method of ["thread/read", "thread/resume"] satisfies (
      | "thread/read"
      | "thread/resume"
    )[]) {
      const result = await bridge.call(method, {
        threadId: sessionId,
        includeTurns: true,
      });
      assert.equal(result.thread.turns[0]?.status, "completed");
      assert.equal(
        result.thread.turns[0]?.items.at(-1)?.text,
        "The task is complete.",
      );
    }
    assert.equal(
      bridge.notes.some((entry) => entry.method === "turn/started"),
      false,
    );
  } finally {
    await bridge.close();
  }
});

test("restart replaces a partial mirror reply with all native text blocks", async () => {
  const final = reply();
  final.message.stop_reason = null;
  (final.message.content as Exclude<typeof final.message.content, string>).push(
    { type: "text", text: "The checks pass." },
  );
  await loadStale([user(), final], { partial: true }, (turn) => {
    assert.equal(turn.status, "completed");
    const answers = turn.items.filter((item) => item.type === "agentMessage");
    assert.equal(answers.length, 1);
    assert.equal(answers[0]?.text, "The task is complete.\nThe checks pass.");
    assert.equal(answers[0]?.phase, "final_answer");
  });
});

test("bridge reads retain the unconfirmed outcome and reason without a model query", async () => {
  const bridge = await startBridge(
    new URL("./fixtures/history-readonly-sdk.ts", import.meta.url),
    "claude-uncertain-wire-",
  );
  try {
    await createSessionStore(path.join(bridge.root, "state")).persist({
      id: sessionId,
      cwd: bridge.root,
      turns: [
        {
          id: "turn",
          status: "inProgress",
          items: [
            {
              id: "input",
              nativeId: userId,
              type: "userMessage",
              content: [{ type: "text", text: "Run the command" }],
            },
          ],
        },
      ],
    });
    const result = await bridge.call("thread/read", {
      threadId: sessionId,
      includeTurns: true,
    });
    assert.equal(result.thread.turns[0]?.status, "uncertain");
    assert.ok(result.thread.turns[0]?.error);
    assert.match(result.thread.turns[0].error.message, /could not confirm/);
    const state = await bridge.call("claude/state", { threadId: sessionId });
    assert.equal(state.turns[0]?.status, "uncertain");
    assert.deepEqual(state.turns[0]?.error, result.thread.turns[0]?.error);
    assert.equal(
      bridge.notes.some((entry) => entry.method === "turn/started"),
      false,
    );
  } finally {
    await bridge.close();
  }
});
