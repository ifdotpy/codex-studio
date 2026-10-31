import type { Session } from "./session-state.ts";
import type { SessionMessage } from "@anthropic-ai/claude-agent-sdk";
import type { ControlDependencies } from "./controls.ts";
import { messageIndex } from "./history-index.ts";
import { test } from "vitest";
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import {
  commandCatalog,
  explicitNativeCommand,
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
    persist: async (value) => saves.push(structuredClone(value)),
    getHistoryIndex: async (id) =>
      messageIndex(
        id === "native"
          ? source
          : source
              .slice(0, 4)
              .map((message, index) => ({ ...message, uuid: `fork${index}` })),
      ),
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
test.each(["symlink", "letter case"])(
  "branch gives the SDK the real project path: %s",
  async (kind) => {
    const root = await fs.realpath(
      await fs.mkdtemp(path.join(os.tmpdir(), "claude-controls-dir-")),
    );
    try {
      const project = path.join(root, "Project");
      await fs.mkdir(project);
      let cwd = path.join(root, "link");
      if (kind === "symlink") await fs.symlink(project, cwd);
      else {
        cwd = path.join(root, "PROJECT");
        // Only a case-insensitive volume, such as default APFS, has this alias.
        if (!(await fs.stat(cwd).catch(() => null))) return;
      }
      const f = fixture();
      f.session.cwd = cwd;
      const dirs: unknown[] = [];
      const { getHistoryIndex, forkSession } = f.deps;
      f.deps.getHistoryIndex = (id, options) => {
        dirs.push(options?.dir);
        return getHistoryIndex(id, options);
      };
      f.deps.forkSession = (id, options) => {
        dirs.push(options.dir);
        return forkSession(id, options);
      };
      await forkAtTurn(f.session, { lastTurnId: "u1" }, f.deps);
      assert.ok(dirs.length >= 3);
      assert.deepEqual(new Set(dirs), new Set([project]));
    } finally {
      await fs.rm(root, { recursive: true, force: true });
    }
  },
);
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
      targetNativeId: "old-target",
      result: { threadId: "logical", nativeId: "old-target", removedTurns: 1 },
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
