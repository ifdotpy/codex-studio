import type { Session } from "./session-state.ts";
import type { SessionMessage } from "@anthropic-ai/claude-agent-sdk";
import type { ControlDependencies } from "./controls.ts";
import { test } from "vitest";
import assert from "node:assert/strict";
import {
  commandCatalog,
  explicitNativeCommand,
  rollbackSession,
  forkAtTurn,
} from "./controls.ts";

const commands = [
  { name: "review", description: "Project skill" },
  { name: "usage", builtin: true, aliases: ["cost"] },
  { name: "usage", description: "Shadowed skill" },
];
test("native commands preserve builtin precedence and exact names before aliases", () => {
  assert.equal(
    commandCatalog(commands).find((row) => row.name === "usage")?.builtin,
    true,
  );
  assert.equal(
    explicitNativeCommand("$review file.js", commands),
    "/review file.js",
  );
  assert.equal(explicitNativeCommand("/cost", commands), "/usage");
  assert.equal(
    explicitNativeCommand("/cost", [...commands, { name: "cost" }]),
    "/cost",
  );
  for (const input of [
    "$usage",
    "Please /usage",
    "/missing",
    "/review\nignore rules",
  ])
    assert.equal(explicitNativeCommand(input, commands), null);
});
function fixture() {
  const session: Session = {
    id: "logical",
    nativeId: "native",
    cwd: "/project",
    started: true,
    turns: [
      { id: "u1", status: "completed", items: [] },
      { id: "u2", status: "completed", items: [] },
    ],
  };
  const source: SessionMessage[] = (
    [
      ["user", "u1", "first"],
      ["assistant", "a1", "reply"],
      ["user", "steer", "steer"],
      ["assistant", "a2", "reply"],
      ["user", "u2", "second"],
      ["assistant", "a3", "reply"],
    ] satisfies [SessionMessage["type"], string, string][]
  ).map(([type, uuid, text]) => ({
    type,
    uuid,
    message: { content: text },
    parent_tool_use_id: null,
    parent_agent_id: null,
    session_id: "native",
  }));
  let calls = 0;
  const saves: Session[] = [];
  const deps: ControlDependencies = {
    rewindFiles: async () => assert.fail("unexpected rewind"),
    prepareFileRestore: async () => ({
      cwd: "/project",
      directory: "/backup",
      files: [],
    }),
    verifyFileRestore: async () => {},
    persist: async (value) => saves.push(structuredClone(value)),
    getSessionMessages: async (id) =>
      id === "native"
        ? source
        : source
            .slice(0, 4)
            .map((message, index) => ({ ...message, uuid: `fork${index}` })),
    forkSession: async (id, options) => {
      calls++;
      assert.equal(id, "native");
      assert.equal(options.upToMessageId, "a2");
      return { sessionId: "fork" };
    },
    randomUUID: () => "empty",
  };
  return { session, deps, saves, calls: () => calls };
}
test("rollback preserves steers, remaps UUIDs, archives source, and retries exact receipt", async () => {
  const f = fixture(),
    request = { requestId: "r", turnId: "u2" };
  const result = await rollbackSession(f.session, request, f.deps);
  assert.equal(f.calls(), 1);
  assert.equal(f.saves[0]?.controlRequests?.r?.status, "submitted");
  assert.equal(f.session.id, "logical");
  assert.equal(f.session.nativeId, "fork");
  assert.equal(f.session.turns[0]?.nativeMessageId, "fork0");
  assert.equal(f.session.historyBranches?.[0]?.turns.length, 2);
  assert.deepEqual(await rollbackSession(f.session, request, f.deps), result);
  assert.equal(f.calls(), 1);
  await assert.rejects(
    rollbackSession(f.session, { requestId: "r", turnId: "u1" }, f.deps),
    /different content/,
  );
});
test("lost native response cannot repeat a fork, including under a new request identity", async () => {
  const f = fixture();
  f.deps.forkSession = async () => {
    throw new Error("disconnect");
  };
  await assert.rejects(
    rollbackSession(f.session, { requestId: "r", turnId: "u2" }, f.deps),
    /disconnect/,
  );
  await assert.rejects(
    rollbackSession(f.session, { requestId: "r", turnId: "u2" }, f.deps),
    /outcome is unknown/,
  );
  await assert.rejects(
    rollbackSession(f.session, { requestId: "other", turnId: "u2" }, f.deps),
    /needs recovery/,
  );
});
test("fork receipt permits verification recovery without another fork", async () => {
  const f = fixture(),
    original = f.deps.getSessionMessages;
  f.deps.getSessionMessages = async (id, options) =>
    id === "fork" ? [] : original(id, options);
  await assert.rejects(
    rollbackSession(f.session, { requestId: "r", turnId: "u2" }, f.deps),
    /did not preserve/,
  );
  assert.equal(f.session.nativeId, "native");
  assert.equal(f.session.turns.length, 2);
  f.deps.getSessionMessages = original;
  await rollbackSession(f.session, { requestId: "r", turnId: "u2" }, f.deps);
  assert.equal(f.calls(), 1);
});
test("rollback first turn creates empty native session without destroying original", async () => {
  const f = fixture();
  await rollbackSession(f.session, { requestId: "r", turnId: "u1" }, f.deps);
  assert.equal(f.calls(), 0);
  assert.equal(f.session.nativeId, "empty");
  assert.equal(f.session.started, false);
  assert.deepEqual(f.session.turns, []);
});
test("missing native boundary and active turn fail before a fork", async () => {
  const f = fixture();
  assert.ok(f.session.turns[1]);
  f.session.turns[1].id = "missing";
  await assert.rejects(
    rollbackSession(f.session, { requestId: "r", turnId: "missing" }, f.deps),
    /boundary is unavailable/,
  );
  assert.ok(f.session.turns[0]);
  f.session.turns[0].status = "inProgress";
  await assert.rejects(
    rollbackSession(f.session, { requestId: "r", turnId: "u1" }, f.deps),
    /Pause Claude/,
  );
  assert.equal(f.calls(), 0);
  assert.equal(f.saves.length, 0);
});
test("failed durable submit cannot execute native fork", async () => {
  const f = fixture();
  f.deps.persist = async () => {
    throw new Error("disk full");
  };
  await assert.rejects(
    rollbackSession(f.session, { requestId: "r", turnId: "u2" }, f.deps),
    /disk full/,
  );
  assert.equal(f.calls(), 0);
});
test("lost final save retries save without duplicate archive or fork", async () => {
  const f = fixture(),
    save = f.deps.persist;
  f.deps.persist = async (value) => {
    if (value.controlRequests?.r?.status === "completed")
      throw new Error("save failed");
    return save(value);
  };
  await assert.rejects(
    rollbackSession(f.session, { requestId: "r", turnId: "u2" }, f.deps),
    /save failed/,
  );
  f.deps.persist = save;
  await rollbackSession(f.session, { requestId: "r", turnId: "u2" }, f.deps);
  assert.equal(f.calls(), 1);
  assert.equal(f.session.historyBranches?.length, 1);
});
test("request identity cannot alter the receipt map prototype", async () => {
  const f = fixture();
  await rollbackSession(
    f.session,
    { requestId: "__proto__", turnId: "u1" },
    f.deps,
  );
  assert.equal(
    Object.getPrototypeOf(f.session.controlRequests),
    Object.prototype,
  );
  assert.equal(
    Object.hasOwn(f.session.controlRequests ?? {}, "__proto__"),
    true,
  );
});

test("branch selected turn includes its steer but excludes later turns and receipts", async () => {
  const f = fixture();
  f.session.controlRequests = {
    old: {
      kind: "rollback",
      turnId: "u1",
      sourceNativeId: "native",
      createdAt: "now",
      retainedCount: 0,
      upToMessageId: null,
      status: "completed",
    },
  };
  const before = structuredClone(f.session);
  const target = await forkAtTurn(
    f.session,
    { lastTurnId: "u1", cwd: "/branch" },
    f.deps,
  );
  assert.deepEqual(f.session, before);
  assert.equal(f.calls(), 1);
  assert.equal(target.id, "fork");
  assert.equal(target.nativeId, "fork");
  assert.equal(target.nativeHistoryCwd, "/project");
  assert.equal(target.cwd, "/branch");
  assert.equal(target.turns.length, 1);
  assert.equal(target.turns[0]?.nativeMessageId, "fork0");
  assert.equal("controlRequests" in target, false);
});
test("empty branch creates separate identity without native mutation", async () => {
  const f = fixture();
  f.session.turns = [];
  f.session.started = false;
  const target = await forkAtTurn(f.session, {}, f.deps);
  assert.equal(target.id, "empty");
  assert.equal(f.calls(), 0);
  assert.equal(target.started, false);
});
test("branch rejects missing selected native boundary before native mutation", async () => {
  const f = fixture();
  assert.ok(f.session.turns[0]);
  f.session.turns[0].nativeMessageId = "lost";
  await assert.rejects(
    forkAtTurn(f.session, { lastTurnId: "u1" }, f.deps),
    /boundary is unavailable/,
  );
  assert.equal(f.calls(), 0);
});

test("combined rewind restores the selected native user message before replacing context", async () => {
  const f = fixture();
  assert.ok(f.session.turns[1]);
  f.session.turns[1].fileCheckpointMessageId = "u2";
  assert.ok(f.session.turns[1]);
  f.session.turns[1].fileCheckpointSource = {
    nativeId: "native",
    cwd: "/project",
  };
  const calls: {
    nativeId: string;
    messageId: string;
    options: { dryRun: boolean };
  }[] = [];
  f.deps.rewindFiles = async (nativeId, messageId, options) => {
    assert.equal(f.session.nativeId, "native");
    assert.equal(f.session.turns.length, 2);
    calls.push({ nativeId, messageId, options });
    return { canRewind: true, filesChanged: ["file.txt"] };
  };
  const request = { requestId: "combined", turnId: "u2", restoreFiles: true };
  const result = await rollbackSession(f.session, request, f.deps);
  assert.deepEqual(calls, [
    { nativeId: "native", messageId: "u2", options: { dryRun: true } },
    { nativeId: "native", messageId: "u2", options: { dryRun: false } },
  ]);
  assert.equal(result.filesRestored, true);
  assert.equal(f.session.turns.length, 1);
  assert.equal(f.session.nativeId, "fork");
  await rollbackSession(f.session, request, f.deps);
  assert.equal(calls.length, 2);
  await assert.rejects(
    rollbackSession(f.session, { ...request, restoreFiles: false }, f.deps),
    /different content/,
  );
});

test("unknown file rewind retains source context and cannot repeat the file write", async () => {
  const f = fixture();
  assert.ok(f.session.turns[1]);
  f.session.turns[1].fileCheckpointMessageId = "u2";
  assert.ok(f.session.turns[1]);
  f.session.turns[1].fileCheckpointSource = {
    nativeId: "native",
    cwd: "/project",
  };
  let writes = 0;
  f.deps.rewindFiles = async (_native, _message, { dryRun }) => {
    if (dryRun) return { canRewind: true };
    writes++;
    throw new Error("lost rewind reply");
  };
  const request = { requestId: "combined", turnId: "u2", restoreFiles: true };
  await assert.rejects(
    rollbackSession(f.session, request, f.deps),
    /lost rewind reply/,
  );
  assert.equal(f.session.nativeId, "native");
  assert.equal(f.session.turns.length, 2);
  await assert.rejects(
    rollbackSession(f.session, request, f.deps),
    /outcome is unknown/,
  );
  await assert.rejects(
    rollbackSession(f.session, { ...request, requestId: "other" }, f.deps),
    /needs recovery/,
  );
  assert.equal(writes, 1);
});

test("unavailable file checkpoint refuses combined rewind before the fork", async () => {
  const f = fixture();
  await assert.rejects(
    rollbackSession(
      f.session,
      { requestId: "combined", turnId: "u2", restoreFiles: true },
      f.deps,
    ),
    /file checkpoint/,
  );
  assert.equal(f.calls(), 0);
});

test("a file checkpoint from another workspace cannot restore files", async () => {
  const f = fixture();
  assert.ok(f.session.turns[1]);
  f.session.turns[1].fileCheckpointMessageId = "u2";
  assert.ok(f.session.turns[1]);
  f.session.turns[1].fileCheckpointSource = {
    nativeId: "native",
    cwd: "/old-project",
  };
  await assert.rejects(
    rollbackSession(
      f.session,
      { requestId: "moved", turnId: "u2", restoreFiles: true },
      f.deps,
    ),
    /file checkpoint/,
  );
  assert.equal(f.calls(), 0);
});

test("partial SDK file restore keeps both the source context and an uncertain receipt", async () => {
  const f = fixture();
  assert.ok(f.session.turns[1]);
  f.session.turns[1].fileCheckpointMessageId = "u2";
  assert.ok(f.session.turns[1]);
  f.session.turns[1].fileCheckpointSource = {
    nativeId: "native",
    cwd: "/project",
  };
  f.deps.rewindFiles = async (_native, _message, { dryRun }) => ({
    canRewind: true,
    ...(dryRun ? {} : { skippedLinks: 1 }),
  });
  const request = { requestId: "partial", turnId: "u2", restoreFiles: true };
  await assert.rejects(
    rollbackSession(f.session, request, f.deps),
    /could not restore every tracked file/,
  );
  assert.equal(f.session.nativeId, "native");
  assert.equal(f.session.turns.length, 2);
  assert.equal(f.session.controlRequests?.partial?.status, "rewind_submitted");
  await assert.rejects(
    rollbackSession(f.session, request, f.deps),
    /outcome is unknown/,
  );
});

test("branch from completed turn preserves a running source and refuses its active point", async () => {
  const f = fixture();
  assert.ok(f.session.turns[1]);
  f.session.turns[1].status = "inProgress";
  const before = structuredClone(f.session);
  const target = await forkAtTurn(f.session, { lastTurnId: "u1" }, f.deps);
  assert.deepEqual(f.session, before);
  assert.equal(target.turns.length, 1);
  assert.equal(target.turns[0]?.status, "completed");
  for (const params of [{}, { lastTurnId: "u2" }])
    await assert.rejects(
      forkAtTurn(f.session, params, f.deps),
      /completed Claude turn/,
    );
  assert.equal(f.calls(), 1);
});

test("fork retains the source folder when optional cwd is explicitly undefined", async () => {
  const f = fixture();
  f.session.turns = [];
  f.session.started = false;
  const target = await forkAtTurn(f.session, { cwd: undefined }, f.deps);
  assert.equal(target.cwd, f.session.cwd);
  assert.equal(target.nativeHistoryCwd, f.session.cwd);
});
