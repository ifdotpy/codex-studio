import { expect, test } from "vitest";
import * as sdk from "@anthropic-ai/claude-agent-sdk";
import { applyIdleSummary, prepareIdleSummary } from "./idle-summary.ts";
import { promptPrefix } from "./idle-compaction.ts";
import type { Session } from "./session-state.ts";
import type { SessionMessage } from "@anthropic-ai/claude-agent-sdk";

const metadata = {
  trigger: "manual" as const,
  pre_tokens: 150000,
  post_tokens: 1000,
};

// Test doubles carry only the fields these tests read.
const typed = <T>(value: object) => value as T;

function fixture() {
  const source = typed<SessionMessage[]>([
    { type: "user", uuid: "u", message: { content: "hello" } },
    { type: "assistant", uuid: "a", message: { content: "answer" } },
  ]);
  const s = typed<Session>({
    id: "studio",
    nativeId: "main",
    cwd: "/repo",
    started: true,
    turns: [{ id: "turn", status: "completed", items: [] }],
    idleCache: { requestId: "response" },
  });
  let forks = 0;
  const writes: Session[] = [];
  const deps = {
    persist: async (value: Session) => {
      writes.push(structuredClone(value));
    },
    getSessionMessages: async (id: string) =>
      id === "main"
        ? source
        : source.map((m) => ({ ...m, uuid: m.uuid + "-fork" })),
    forkSession: async () => {
      forks++;
      return { sessionId: "fork" };
    },
    compact: async () => metadata,
    isCurrent: () => true,
  };
  const expected = { requestId: "response", prefix: promptPrefix(s) };
  return { s, source, deps, expected, writes, forks: () => forks };
}

test("preparation keeps the main history; return commits the fork exactly once", async () => {
  const f = fixture();
  await prepareIdleSummary(f.s, "idle:one", f.expected, {}, f.deps);
  expect(f.s.nativeId).toBe("main");
  expect(f.s.idleSummary?.status).toBe("ready");
  expect(f.writes[0].idleSummary?.status).toBe("preparing");
  const restarted = structuredClone(f.s);
  expect(await applyIdleSummary(restarted, {}, f.deps)).toMatchObject({
    requestId: "idle:one",
  });
  expect(restarted.nativeId).toBe("fork");
  expect(restarted.historyBranches?.[0].nativeId).toBe("main");
  expect(await applyIdleSummary(restarted, {}, f.deps)).toBe(null);
  expect(f.forks()).toBe(1);
});

test("changed history, prefix, and native identity prevent replacement", async () => {
  const changes: ((f: ReturnType<typeof fixture>) => void)[] = [
    (f) =>
      f.source.push(
        typed<SessionMessage>({
          type: "user",
          uuid: "new",
          message: { content: "new input" },
        }),
      ),
    (f) => {
      f.s.model = "different";
    },
    (f) => {
      f.s.nativeId = "other";
    },
  ];
  for (const change of changes) {
    const f = fixture();
    await prepareIdleSummary(f.s, "idle:one", f.expected, {}, f.deps);
    change(f);
    const before = f.s.nativeId;
    expect(await applyIdleSummary(f.s, {}, f.deps)).toBe(null);
    expect(f.s.nativeId).toBe(before);
  }
});

test("failed fork or summary and uncertain preparation never replace or retry", async () => {
  for (const stage of ["forkSession", "compact", "persist"] as const) {
    const f = fixture();
    (f.deps as Record<string, unknown>)[stage] = async () => {
      throw new Error(stage);
    };
    await expect(
      prepareIdleSummary(f.s, "idle:one", f.expected, {}, f.deps),
    ).rejects.toThrow(stage);
    expect(f.s.nativeId).toBe("main");
    await expect(
      prepareIdleSummary(f.s, "idle:one", f.expected, {}, f.deps),
    ).rejects.toThrow();
    expect(f.forks()).toBeLessThanOrEqual(1);
  }
});

test("the fork must preserve each message, and a busy source cannot become ready", async () => {
  const f = fixture();
  f.deps.getSessionMessages = async (id: string) =>
    id === "main" ? f.source : [];
  await expect(
    prepareIdleSummary(f.s, "idle:one", f.expected, {}, f.deps),
  ).rejects.toThrow("history");
  expect(f.s.nativeId).toBe("main");
  const g = fixture();
  g.deps.compact = async () => {
    g.deps.isCurrent = () => false;
    return metadata;
  };
  await expect(
    prepareIdleSummary(g.s, "idle:one", g.expected, {}, g.deps),
  ).rejects.toThrow("changed");
  expect(g.s.idleSummary?.status).not.toBe("ready");
});

test("a lost target or changed activity during validation keeps the original", async () => {
  for (const kind of ["missing", "busy"]) {
    const f = fixture();
    await prepareIdleSummary(f.s, "idle:one", f.expected, {}, f.deps);
    const read = f.deps.getSessionMessages;
    f.deps.getSessionMessages = async (id: string) => {
      if (id === "fork") {
        if (kind === "missing") throw new Error("target missing");
        f.deps.isCurrent = () => false;
      }
      return read(id);
    };
    expect(await applyIdleSummary(f.s, {}, f.deps)).toBe(null);
    expect(f.s.nativeId).toBe("main");
  }
});

test("a failed switch write restores all memory fields before another input", async () => {
  const f = fixture();
  await prepareIdleSummary(f.s, "idle:one", f.expected, {}, f.deps);
  f.deps.persist = async () => {
    throw new Error("disk");
  };
  await expect(applyIdleSummary(f.s, {}, f.deps)).rejects.toThrow("disk");
  expect(f.s.nativeId).toBe("main");
  expect(f.s.historyBranches).toBeUndefined();
  expect(f.s.idleSummary?.status).toBe("ready");
});

test("the installed SDK fork preserves the proof for native message and system rows", async () => {
  const main = "11111111-1111-4111-8111-111111111111";
  const input = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
  const answer = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
  const rows = [
    {
      type: "user",
      uuid: input,
      parentUuid: null,
      sessionId: main,
      cwd: "/repo",
      timestamp: "2026-10-25T00:00:00.000Z",
      message: { role: "user", content: "hello" },
    },
    {
      type: "system",
      uuid: "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
      parentUuid: input,
      sessionId: main,
      cwd: "/repo",
      timestamp: "2026-10-25T00:00:01.000Z",
      subtype: "local_command",
      message: { content: "native notice" },
    },
    {
      type: "assistant",
      uuid: answer,
      parentUuid: "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
      sessionId: main,
      cwd: "/repo",
      timestamp: "2026-10-25T00:00:02.000Z",
      message: {
        role: "assistant",
        content: [{ type: "text", text: "answer" }],
      },
    },
  ];
  const saved = new Map<string, unknown[]>([[main, rows]]);
  const sessionStore = {
    load: async (key: { sessionId: string }) =>
      structuredClone(saved.get(key.sessionId) || null),
    append: async (key: { sessionId: string }, entries: unknown[]) =>
      saved.set(key.sessionId, [
        ...(saved.get(key.sessionId) || []),
        ...structuredClone(entries),
      ]),
  };
  // The SDK store options are not in the public type; the call shape is checked at runtime.
  const withStore = (options: object) =>
    typed<Parameters<typeof sdk.getSessionMessages>[1]>({
      ...options,
      sessionStore,
    });
  const source = await sdk.getSessionMessages(
    main,
    withStore({
      dir: "/repo",
      includeSystemMessages: true,
    }),
  );
  expect(source.map((message) => message.type)).toEqual([
    "user",
    "system",
    "assistant",
  ]);
  const s = typed<Session>({
    id: "studio",
    nativeId: main,
    started: true,
    cwd: "/repo",
    turns: [],
    idleCache: { requestId: "response" },
  });
  const receipt = await prepareIdleSummary(
    s,
    "idle:sdk",
    { requestId: "response", prefix: promptPrefix(s) },
    {},
    {
      persist: async () => {},
      isCurrent: () => true,
      getSessionMessages: (id, options) =>
        sdk.getSessionMessages(id, withStore(options)),
      forkSession: (id, options) => sdk.forkSession(id, withStore(options)),
      compact: async () => metadata,
    },
  );
  expect(receipt.status).toBe("ready");
  expect(receipt.targetNativeId).not.toBe(main);
  expect(s.nativeId).toBe(main);
});
