import { afterEach, expect, test, vi } from "vitest";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { rollbackBoundary, forkAtTurn, rollbackSession } from "./controls.mjs";
import { nativeInputIds } from "./input-messages.mjs";
import { recordedInputIds, inputHistoryOutcome } from "./stall-watch.mjs";
import { readNativeHistory } from "./native-history.mjs";
import { waitForFile } from "./testing/file-signals.mjs";
import { startBridge } from "./testing/bridge-harness.mjs";
import { createSessionStore } from "./session-store.mjs";

const sessionId = "11111111-1111-4111-8111-111111111111";
const inputId = "22222222-2222-4222-8222-222222222222";
const replyId = "33333333-3333-4333-8333-333333333333";
const steerId = "44444444-4444-4444-8444-444444444444";
let root;
afterEach(async () => {
  vi.useRealTimers();
  if (root) await fs.rm(root, { recursive: true, force: true });
  root = undefined;
});

const message = (type, uuid) => ({
  type,
  uuid,
  message: { content: uuid },
  parent_tool_use_id: null,
});
const turns = () => [
  { id: "u1", status: "completed" },
  {
    id: "final2",
    status: "completed",
    nativeMessageId: "final2",
    items: [
      {
        type: "userMessage",
        nativeId: "final2",
        nativeIds: ["prefix2", "final2"],
      },
    ],
  },
];
const messages = () => [
  message("user", "u1"),
  message("assistant", "a1"),
  message("user", "prefix2"),
  message("user", "final2"),
  message("assistant", "a2"),
];

test("rollback removes the whole composite input", () => {
  expect(rollbackBoundary(turns(), messages(), "final2")).toEqual({
    index: 1,
    end: 2,
    upToMessageId: "a1",
  });
});

test.each([
  [
    "missing prefix",
    [
      message("user", "u1"),
      message("assistant", "a1"),
      message("user", "final2"),
    ],
  ],
  [
    "reversed parts",
    [
      message("user", "u1"),
      message("user", "final2"),
      message("user", "prefix2"),
    ],
  ],
  ["duplicate part", [...messages(), message("user", "prefix2")]],
  [
    "foreign subagent part",
    messages().map((row) =>
      row.uuid === "prefix2" ? { ...row, parent_agent_id: "agent" } : row,
    ),
  ],
])("rollback refuses %s", (_name, rows) => {
  expect(() => rollbackBoundary(turns(), rows, "final2")).toThrow(/boundary/);
});

test("fork stops before the next composite prefix and remaps retained parts", async () => {
  const source = {
    id: "source",
    cwd: "/project",
    started: true,
    turns: turns(),
  };
  const fork = vi.fn(async () => ({ sessionId: "target" }));
  const deps = {
    persist: vi.fn(),
    forkSession: fork,
    getSessionMessages: vi.fn(async (id) =>
      id === "source"
        ? messages()
        : messages()
            .slice(0, 2)
            .map((row, i) => ({ ...row, uuid: `fork${i}` })),
    ),
  };
  const target = await forkAtTurn(source, { lastTurnId: "u1" }, deps);
  expect(fork).toHaveBeenCalledWith("source", {
    dir: "/project",
    upToMessageId: "a1",
  });
  expect(target.turns).toHaveLength(1);
});

test.each([
  "malformed",
  "torn",
  "absent anchor",
  "outside tail",
  "replaced",
  "partial composite",
  "reversed composite",
  "foreign steer",
  "valid absent",
  "valid delivered",
])("closed steer history: %s", async (kind) => {
  root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-lifecycle-"));
  const session = { id: sessionId, cwd: root };
  const initial = { id: "initial", type: "userMessage", nativeId: inputId };
  const steer = {
    id: "steer",
    type: "userMessage",
    nativeId: steerId,
    delivery: "steer",
    deliveryStatus: "submitted",
  };
  const row = (uuid) => ({
    type: "user",
    uuid,
    sessionId,
    cwd: root,
    isSidechain: false,
  });
  let raw =
    JSON.stringify(row(kind === "absent anchor" ? replyId : inputId)) + "\n";
  if (kind === "malformed") raw += "broken\n";
  if (kind === "torn") raw += '{"type":';
  if (kind === "outside tail")
    raw += (
      JSON.stringify({ type: "progress", padding: "x".repeat(60_000) }) + "\n"
    ).repeat(141);
  if (["partial composite", "reversed composite"].includes(kind))
    steer.nativeIds = [replyId, steerId];
  if (
    [
      "valid delivered",
      "partial composite",
      "reversed composite",
      "foreign steer",
    ].includes(kind)
  )
    raw +=
      JSON.stringify({
        ...row(steerId),
        parentUuid: inputId,
        ...(kind === "foreign steer" ? { sessionId: replyId } : {}),
      }) + "\n";
  if (kind === "reversed composite")
    raw += JSON.stringify({ ...row(replyId), parentUuid: steerId }) + "\n";
  const configDir = path.join(root, "config");
  const file = path.join(
    configDir,
    "projects",
    root.replace(/[^a-zA-Z0-9]/g, "-"),
    sessionId + ".jsonl",
  );
  await fs.mkdir(path.dirname(file), { recursive: true });
  await fs.writeFile(file, raw);
  const records = await readNativeHistory(session, {
    configDir,
    ...(kind === "replaced"
      ? {
          filesystem: {
            ...fs,
            lstat: async (file) => {
              const stat = await fs.lstat(file);
              stat.ino++;
              return stat;
            },
          },
        }
      : {}),
  });
  expect(
    inputHistoryOutcome(
      records?.map(({ row }) => row),
      {
        sessionId,
        cwd: root,
        anchorIds: nativeInputIds(initial),
        inputIds: nativeInputIds(steer),
      },
    ),
  ).toBe(
    kind === "valid absent"
      ? "not_applied"
      : kind === "valid delivered"
        ? "delivered"
        : "unknown",
  );
});

test.each(["complete", "partial initial", "partial steer", "reversed steer"])(
  "restart promotes chain-proven receipts: %s",
  async (kind) => {
    root = await fs.mkdtemp(path.join(os.tmpdir(), "claude-receipts-"));
    const session = {
      id: sessionId,
      cwd: root,
      turns: [
        {
          id: "turn",
          status: "inProgress",
          startOutcome: "preparing",
          items: [
            { id: "initial", type: "userMessage", nativeId: inputId },
            {
              id: "steer",
              type: "userMessage",
              nativeId: steerId,
              delivery: "steer",
              deliveryStatus: "submitted",
            },
            {
              id: "unproven",
              type: "userMessage",
              nativeId: "55555555-5555-4555-8555-555555555555",
              delivery: "steer",
              deliveryStatus: "submitted",
            },
          ],
        },
      ],
    };
    const row = (type, uuid, parentUuid, content) => ({
      type,
      uuid,
      parentUuid,
      sessionId,
      cwd: root,
      isSidechain: false,
      message: { id: uuid, content, stop_reason: "end_turn" },
    });
    const rows = [
      row("user", inputId, null, "initial"),
      row("user", steerId, inputId, [{ type: "text", text: "steer" }]),
      row("assistant", replyId, steerId, [{ type: "text", text: "done" }]),
    ];
    const configDir = path.join(root, "config");
    const file = path.join(
      configDir,
      "projects",
      root.replace(/[^a-zA-Z0-9]/g, "-"),
      sessionId + ".jsonl",
    );
    await fs.mkdir(path.dirname(file), { recursive: true });
    await fs.writeFile(
      file,
      rows.map((row) => JSON.stringify(row) + "\n").join(""),
    );
    if (kind === "partial initial")
      session.turns[0].items[0].nativeIds = [
        "66666666-6666-4666-8666-666666666666",
        inputId,
      ];
    if (["partial steer", "reversed steer"].includes(kind))
      session.turns[0].items[1].nativeIds = [
        "66666666-6666-4666-8666-666666666666",
        steerId,
      ];
    if (kind === "reversed steer") {
      const prefix = row(
        "user",
        "66666666-6666-4666-8666-666666666666",
        steerId,
        [{ type: "text", text: "prefix" }],
      );
      rows.splice(2, 0, prefix);
      rows.at(-1).parentUuid = prefix.uuid;
      await fs.writeFile(
        file,
        rows.map((row) => JSON.stringify(row) + "\n").join(""),
      );
    }
    const stateRoot = path.join(root, "state");
    await createSessionStore(stateRoot).persist(session);
    const store = createSessionStore(stateRoot, {
      nativeHistory: { configDir },
    });
    const restored = await store.get(sessionId);
    expect(restored.turns[0].startOutcome).toBe(
      kind === "partial initial" ? "preparing" : "accepted",
    );
    expect(restored.turns[0].items[1].deliveryStatus).toBe(
      kind === "complete" ? "delivered" : "submitted",
    );
    expect(restored.turns[0].items[2].deliveryStatus).toBe("submitted");
    await store.persist(restored);
    expect(await createSessionStore(stateRoot).get(sessionId)).toEqual(
      restored,
    );
  },
);

test("a malformed native row cannot prove absent input", () => {
  expect(
    recordedInputIds([
      JSON.stringify({ type: "user", uuid: inputId }),
      "broken",
    ]),
  ).toBeNull();
});

test("rollback remaps every retained composite part and preserves checkpoint identity", async () => {
  const sourceTurns = turns();
  sourceTurns[0] = {
    id: "u1",
    status: "completed",
    nativeMessageId: "u1",
    fileCheckpointMessageId: "u1",
    items: [
      { type: "userMessage", nativeId: "u1", nativeIds: ["prefix1", "u1"] },
    ],
  };
  const sourceMessages = [message("user", "prefix1"), ...messages()];
  const session = {
    id: "source",
    cwd: "/project",
    nativeId: "native",
    started: true,
    turns: sourceTurns,
  };
  const fork = vi.fn(async () => ({ sessionId: "target" }));
  await rollbackSession(
    session,
    { requestId: "rollback", turnId: "final2" },
    {
      persist: vi.fn(),
      forkSession: fork,
      getSessionMessages: async (id) =>
        id === "native"
          ? sourceMessages
          : sourceMessages
              .slice(0, 3)
              .map((row, i) => ({ ...row, uuid: `fork${i}` })),
    },
  );
  expect(fork).toHaveBeenCalledWith("native", {
    dir: "/project",
    upToMessageId: "a1",
  });
  expect(session.turns[0].nativeMessageId).toBe("fork0");
  expect(session.turns[0].items[0].nativeIds).toEqual(["fork0", "fork1"]);
  expect(session.turns[0].items[0].nativeId).toBe("fork1");
  expect(session.turns[0].fileCheckpointMessageId).toBeUndefined();
  expect(session.historyBranches[0].turns[0].fileCheckpointMessageId).toBe(
    "u1",
  );
});

test.each(["direct", "timer", "success"])(
  "real bridge finalization has one outcome: %s",
  async (mode) => {
    const bridge = await startBridge(
      `const scenario = ${JSON.stringify(mode)};\n` +
        String.raw`
    import fs from "node:fs/promises";
    let cwd;
    const append = fs.appendFile;
    fs.appendFile = async (file, data, ...args) => {
      if (scenario !== "success" && String(file).includes("/sessions/") && String(data).includes('"value":"completed"')) {
        await append(cwd + "/save-attempts", "x");
        throw Object.assign(new Error("final save denied"), { code: "EACCES" });
      }
      return append(file, data, ...args);
    };
    export const tool = () => ({});
    export const createSdkMcpServer = value => value;
    export const forkSession = async () => { throw new Error("No fork expected"); };
    export const getSessionMessages = async () => [];
    export function query({ prompt, options }) {
      cwd = options.cwd;
      let closed = false;
      return {
        supportedModels: async () => [{ value: "default", displayName: "Default", resolvedModel: "claude-opus-5-5" }],
        accountInfo: async () => ({ email: "test@example.test", subscriptionType: "Claude Max", apiProvider: "firstParty" }),
        initializationResult: async () => ({ commands: [] }),
        close() { closed = true; },
        async *[Symbol.asyncIterator]() {
          const first = await prompt.next();
          if (first.done) return;
          await fs.appendFile(options.cwd + "/inputs", "once\n");
          yield { type: "user", isReplay: true, uuid: first.value.uuid };
          yield { type: "system", subtype: "init" };
          if (scenario === "timer") {
            const steer = await prompt.next();
            await fs.appendFile(cwd + "/inputs", steer.value.message.content[0].text + "\n");
          }
          yield { type: "result", subtype: "success", result: "done", usage: { input_tokens: 1, output_tokens: 1 }, modelUsage: { "claude-opus-5-5": { inputTokens: 1, outputTokens: 1, cacheReadInputTokens: 0, cacheCreationInputTokens: 0, webSearchRequests: 0, costUSD: 0, contextWindow: 200000, maxOutputTokens: 64000 } } };
          if (!closed) await prompt.next();
        },
      };
    }
  `,
      "claude-final-save-",
      {
        clock: true,
        env: {
          STUDIO_CLAUDE_STEER_ECHO_GRACE_MS: "3",
          STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "3",
        },
      },
    );
    try {
      const input = [{ type: "text", text: "run once" }];
      const started = await bridge.call("turn/start", {
        threadId: bridge.thread,
        clientUserMessageId: "once",
        input,
      });
      if (mode === "timer")
        await bridge.call("turn/steer", {
          threadId: bridge.thread,
          expectedTurnId: started.turn.id,
          clientUserMessageId: "steer-once",
          input: [{ type: "text", text: "steer once" }],
        });
      const done = await bridge.advanceUntil(
        (note) => note.method === "turn/completed",
      );
      expect(done.params.turn.status).toBe(
        mode === "success" ? "completed" : "failed",
      );
      if (mode !== "success") {
        expect(done.params.turn.error.message).toContain("final save denied");
        expect(
          await fs.readFile(path.join(bridge.root, "save-attempts"), "utf8"),
        ).toBe("x");
        expect((await bridge.call("thread/loaded/list", {})).data).toEqual([]);
      }
      const retry = await bridge.call("turn/start", {
        threadId: bridge.thread,
        clientUserMessageId: "once",
        input,
      });
      expect(retry.turn.id).toBe(started.turn.id);
      expect(await fs.readFile(path.join(bridge.root, "inputs"), "utf8")).toBe(
        mode === "timer" ? "once\nsteer once\n" : "once\n",
      );
      const usage = bridge.notes.filter(
        (note) => note.method === "thread/tokenUsage/updated",
      );
      expect(usage).toHaveLength(1);
      expect(usage[0].params.studioUsage).toMatchObject({
        kind: "cumulative",
        scope: "claudeQuery",
        total: {
          uncachedInput: 1,
          cacheReadInput: 0,
          cacheWriteInput: 0,
          output: 1,
        },
      });
      expect(usage[0].params.tokenUsage).toBeUndefined();
      expect(
        bridge.notes.filter((note) => note.method === "turn/started"),
      ).toHaveLength(1);
      expect(
        bridge.notes.filter((note) => note.method === "turn/completed"),
      ).toHaveLength(1);
    } finally {
      await bridge.close();
    }
  },
);

test.each(["not_applied", "cancelled"])(
  "rollback preserves an unapplied steer receipt: %s",
  async (status) => {
    const session = {
      id: "source",
      cwd: "/project",
      started: true,
      turns: turns(),
    };
    session.turns[0].items = [
      { type: "userMessage", nativeId: "u1" },
      {
        type: "userMessage",
        nativeId: "unread",
        delivery: "steer",
        deliveryStatus: status,
      },
    ];
    const forkSession = vi.fn(async () => ({ sessionId: "target" }));
    await rollbackSession(
      session,
      { requestId: "rollback", turnId: "final2" },
      {
        persist: vi.fn(),
        forkSession,
        getSessionMessages: async (id) =>
          id === "source"
            ? messages()
            : messages()
                .slice(0, 2)
                .map((row, i) => ({ ...row, uuid: `fork${i}` })),
      },
    );
    expect(forkSession).toHaveBeenCalledTimes(1);
    expect(session.controlRequests.rollback.status).toBe("completed");
    expect(session.turns[0].items[1].deliveryStatus).toBe(status);
  },
);

test("fork accepts the authoritative boundary of a legacy remapped turn", async () => {
  const source = {
    id: "source",
    cwd: "/project",
    started: true,
    turns: [
      {
        id: "original",
        status: "completed",
        nativeMessageId: "u1",
        items: [{ type: "userMessage", nativeId: "original" }],
      },
    ],
  };
  const target = await forkAtTurn(
    source,
    {},
    {
      persist: vi.fn(),
      forkSession: async () => ({ sessionId: "target" }),
      getSessionMessages: async (id) =>
        id === "source"
          ? messages().slice(0, 2)
          : messages()
              .slice(0, 2)
              .map((row, i) => ({ ...row, uuid: `fork${i}` })),
    },
  );
  expect(target.turns[0].nativeMessageId).toBe("fork0");
  expect(target.turns[0].items[0].nativeId).toBe("fork0");
});

test("an unresolved middle parent keeps absent steer history unknown", () => {
  const row = (type, uuid, parentUuid) => ({
    type,
    uuid,
    parentUuid,
    sessionId,
    cwd: "/project",
    isSidechain: false,
  });
  expect(
    inputHistoryOutcome(
      [row("user", inputId, null), row("assistant", replyId, steerId)],
      { sessionId, cwd: "/project", anchorIds: [inputId], inputIds: [steerId] },
    ),
  ).toBe("unknown");
});

test("a legacy delivered steer without provenance fails before native fork submission", async () => {
  const source = {
    id: "legacy-fork",
    cwd: "/project",
    started: true,
    turns: [
      {
        id: "original-turn",
        status: "completed",
        nativeMessageId: "remapped-initial",
        items: [
          { type: "userMessage", nativeId: "original-initial" },
          {
            type: "userMessage",
            nativeId: "original-steer",
            delivery: "steer",
            deliveryStatus: "delivered",
          },
        ],
      },
    ],
  };
  const rows = [
    message("user", "remapped-initial"),
    message("assistant", "reply1"),
    message("user", "remapped-steer"),
    message("assistant", "reply2"),
  ];
  const forkSession = vi.fn(async () => ({ sessionId: "target" }));
  await expect(
    forkAtTurn(
      source,
      {},
      {
        persist: vi.fn(),
        forkSession,
        getSessionMessages: async (id) =>
          id === "legacy-fork"
            ? rows
            : rows.map((row, i) => ({ ...row, uuid: `fork${i}` })),
      },
    ),
  ).rejects.toThrow(/mapping.*unavailable|boundary/i);
  expect(forkSession).not.toHaveBeenCalled();
});

test("Stop finalizes a stuck SDK whose close throws without redelivering unknown input", async () => {
  const bridge = await startBridge(
    String.raw`
    import fs from "node:fs/promises";
    export const tool = () => ({});
    export const createSdkMcpServer = value => value;
    export const forkSession = async () => { throw new Error("No fork expected"); };
    export const getSessionMessages = async () => [];
    export function query({ prompt, options }) {
      let consumed = false, held;
      return {
        supportedModels: async () => [{ value: "default", displayName: "Default", resolvedModel: "claude-opus-5-5" }],
        accountInfo: async () => ({ email: "test@example.test", subscriptionType: "Claude Max", apiProvider: "firstParty" }),
        initializationResult: async () => ({ commands: [] }),
        interrupt: async () => ({ cancelled: [], still_queued: held ? [held.uuid] : [] }),
        close() { if (consumed) throw new Error("SDK close failed"); },
        async *[Symbol.asyncIterator]() {
          const first = await prompt.next();
          if (first.done) return;
          consumed = true;
          yield { type: "user", isReplay: true, uuid: first.value.uuid };
          yield { type: "system", subtype: "init" };
          held = (await prompt.next()).value;
          await fs.writeFile(options.cwd + "/steer-read", "read");
          await new Promise(() => {});
        },
      };
    }
  `,
    "claude-close-failure-",
    {
      clock: true,
      env: {
        STUDIO_CLAUDE_STALL_INTERRUPT_WAIT_MS: "3",
        STUDIO_CLAUDE_STALL_EXIT_WAIT_MS: "3",
      },
    },
  );
  try {
    const started = await bridge.call("turn/start", {
      threadId: bridge.thread,
      clientUserMessageId: "initial",
      input: [{ type: "text", text: "initial" }],
    });
    await bridge.call("turn/steer", {
      threadId: bridge.thread,
      expectedTurnId: started.turn.id,
      clientUserMessageId: "held",
      input: [{ type: "text", text: "held" }],
    });
    await waitForFile(path.join(bridge.root, "steer-read"));
    await bridge.call("turn/interrupt", {
      threadId: bridge.thread,
      turnId: started.turn.id,
    });
    const done = await bridge.advanceUntil(
      (note) => note.method === "turn/completed",
    );
    expect(["failed", "interrupted"]).toContain(done.params.turn.status);
    expect(done.params.turn.error.message).toMatch(
      /close failed|did not end|did not stop/,
    );
    expect(done.params.turn.unappliedSteers).toBeUndefined();
    expect(
      bridge.notes.filter((note) => note.method === "turn/completed"),
    ).toHaveLength(1);
    expect((await bridge.call("thread/loaded/list", {})).data).toEqual([]);
    const saved = (
      await bridge.call("thread/read", {
        threadId: bridge.thread,
        includeTurns: true,
      })
    ).thread.turns[0];
    expect(saved.items.find((item) => item.id === "held").deliveryStatus).toBe(
      "submitted",
    );
  } finally {
    await bridge.close();
  }
});
