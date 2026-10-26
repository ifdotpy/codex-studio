import type { IdentityQueryRecord } from "./fixtures/session-identity-sdk.ts";
import type { Session } from "./session-state.ts";
import { expect, test } from "vitest";
import { startBridge } from "./testing/bridge-harness.ts";
import fs from "node:fs/promises";
import path from "node:path";
import os from "node:os";

// The scripted child writes this local query log using IdentityQueryRecord.
function parseIdentityQueryRecord(text: string): IdentityQueryRecord {
  return JSON.parse(text);
}
const NEW = "22222222-2222-4222-8222-222222222222";
const CONVERSATION = "33333333-3333-4333-8333-333333333333";

for (const source of ["init", "hook", "fork", "both"])
  test(`clear/fork ${source} persists the authoritative native id and resumes it after restart`, async () => {
    const base = path.join(
      os.homedir(),
      ".cache/codex-studio/pivot1-v5-history",
    );
    await fs.mkdir(base, { recursive: true });
    const bridge = await startBridge(
      new URL("./fixtures/session-identity-sdk.ts", import.meta.url),
      "clear-",
      {
        temporaryBase: base,
        env: {
          IDENTITY_SOURCE: source,
          IDENTITY_NEW_SESSION_ID: NEW,
          IDENTITY_CONVERSATION_ID: CONVERSATION,
        },
      },
    );
    try {
      const first = await bridge.call("turn/start", {
        threadId: bridge.thread,
        input: [{ type: "text", text: "/clear" }],
      });
      await bridge.note(
        (n) =>
          n.method === "turn/completed" && n.params.turn.id === first.turn.id,
      );
      expect(bridge.thread).not.toBe(NEW);
      expect(
        (await bridge.call("claude/state", { threadId: bridge.thread }))
          .nativeId,
      ).toBe(NEW);
      await bridge.restart();
      expect(
        (await bridge.call("claude/state", { threadId: bridge.thread }))
          .nativeId,
      ).toBe(NEW);
      await bridge.call("turn/start", {
        threadId: bridge.thread,
        input: [{ type: "text", text: "after clear" }],
      });
      await bridge.note(
        (note) =>
          note.method === "fixture/query" &&
          (note.params.resume || note.params.sessionId),
      );
      const queries = (
        await fs.readFile(path.join(bridge.root, ".queries"), "utf8")
      )
        .trim()
        .split("\n")
        .map(parseIdentityQueryRecord);
      expect(queries[1]?.resume).toBe(NEW);
      expect(queries[1]?.resume).not.toBe(CONVERSATION);
    } finally {
      await bridge.close();
    }
  });

for (const phase of [
  "before",
  "after",
  "committed",
  "missing target",
  "unsolicited",
])
  test(`clear crash ${phase} transition save never resumes the old native session`, async () => {
    const base = path.join(
      os.homedir(),
      ".cache/codex-studio/pivot1-v5-history",
    );
    await fs.mkdir(base, { recursive: true });
    const bridge = await startBridge(
      new URL("./fixtures/session-identity-sdk.ts", import.meta.url),
      "clear-crash-",
      {
        temporaryBase: base,
        env: {
          IDENTITY_SOURCE: phase === "unsolicited" ? "fork" : "both",
          IDENTITY_PHASE: phase,
          IDENTITY_NEW_SESSION_ID: NEW,
          IDENTITY_CONVERSATION_ID: CONVERSATION,
        },
      },
    );
    try {
      const first = bridge
        .call("turn/start", {
          threadId: bridge.thread,
          input: [
            {
              type: "text",
              text: phase === "unsolicited" ? "continue normally" : "/clear",
            },
          ],
        })
        .catch(() => null);
      await bridge.note((note) => note.method === "fixture/crash");
      await first;
      await bridge.restart();
      let rejected;
      try {
        await bridge.call("turn/start", {
          threadId: bridge.thread,
          input: [{ type: "text", text: "after crash" }],
        });
      } catch (error) {
        rejected = error;
      }
      if (rejected instanceof Error)
        expect(rejected.message).toMatch(
          /native session.*unknown|transition.*unknown/i,
        );
      if (!rejected)
        await bridge.note(
          (note) =>
            note.method === "fixture/query" &&
            (note.params.resume || note.params.sessionId),
        );
      const queries = (
        await fs.readFile(path.join(bridge.root, ".queries"), "utf8")
      )
        .trim()
        .split("\n")
        .map(parseIdentityQueryRecord);
      expect(
        queries.slice(1).some((query) => query.resume === bridge.thread),
      ).toBe(false);
      if (phase === "after" || phase === "committed" || phase === "unsolicited")
        expect(
          (await bridge.call("claude/state", { threadId: bridge.thread }))
            .nativeId,
        ).toBe(NEW);
      else expect(rejected).toBeDefined();
    } finally {
      await bridge.close();
    }
  });

test("a rejected write-ahead save blocks live resume with an unknown transition", async () => {
  const { sessionStartHook, assertNativeSessionReady } =
    await import("./session-identity.ts");
  const session: Session = {
    id: "11111111-1111-4111-8111-111111111111",
    cwd: "/fixture",
    turns: [],
    pendingNativeSession: {
      from: "11111111-1111-4111-8111-111111111111",
      source: "input",
      status: "pending",
    },
  };
  const hook = sessionStartHook(
    session,
    () => undefined,
    async () => {
      throw new Error("save denied");
    },
  );
  await expect(
    hook(
      {
        cwd: session.cwd,
        transcript_path: "fixture.jsonl",
        hook_event_name: "SessionStart",
        source: "clear",
        session_id: NEW,
      },
      undefined,
      { signal: new AbortController().signal },
    ),
  ).rejects.toThrow("save denied");
  expect(() => assertNativeSessionReady(session)).toThrow(
    /transition.*unknown/i,
  );
  expect(session.nativeId).toBeUndefined();
});

test("another new-session source durably saves target and identity before hook return", async () => {
  const { sessionStartHook } = await import("./session-identity.ts");
  const OLD = "11111111-1111-4111-8111-111111111111";
  const session: Session = {
    id: OLD,
    nativeId: OLD,
    cwd: "/fixture",
    turns: [],
  };
  const snapshots: Session[] = [];
  let completedWrites = 0;
  const hook = sessionStartHook(
    session,
    () => undefined,
    async (state) => {
      snapshots.push(structuredClone(state));
      await Promise.resolve();
      completedWrites++;
    },
  );
  await hook(
    {
      cwd: session.cwd,
      transcript_path: "fixture.jsonl",
      hook_event_name: "SessionStart",
      source: "compact",
      session_id: NEW,
    },
    undefined,
    { signal: new AbortController().signal },
  );
  expect(snapshots).toHaveLength(2);
  expect(snapshots[0]?.nativeId).toBe(OLD);
  expect(snapshots[0]?.pendingNativeSession?.target).toBe(NEW);
  expect(snapshots[1]?.nativeId).toBe(NEW);
  expect(snapshots[1]?.pendingNativeSession).toBeUndefined();
  expect(completedWrites).toBe(2);
});

for (const deliveryStatus of ["not_applied", "cancelled"])
  for (const match of ["exact", "input", "turn", "attempt"])
    test(`saved ${deliveryStatus} receipt retires only its matching transition guard: ${match}`, async () => {
      const { randomUUID } = await import("node:crypto");
      const { createSessionStore } = await import("./session-store.ts");
      const { assertNativeSessionReady } =
        await import("./session-identity.ts");
      const base = path.join(
        os.homedir(),
        ".cache/codex-studio/pivot1-v5-history",
      );
      await fs.mkdir(base, { recursive: true });
      const root = await fs.mkdtemp(path.join(base, "saved-guard-"));
      try {
        const id = randomUUID();
        const attemptId = randomUUID();
        const session: Session = {
          id,
          cwd: root,
          started: true,
          pendingNativeSession: {
            from: id,
            source: "input",
            status: "pending",
            inputId: match === "input" ? "other-steer" : "clear-steer",
            turnId: match === "turn" ? "other-turn" : "turn",
            attemptId: match === "attempt" ? randomUUID() : attemptId,
          },
          turns: [
            {
              id: "turn",
              status: "completed",
              items: [
                {
                  id: "clear-steer",
                  type: "userMessage",
                  delivery: "steer",
                  deliveryStatus,
                  nativeSubmissionAttemptId: attemptId,
                },
              ],
            },
          ],
        };
        await createSessionStore(root).persist(session);
        const restored = await createSessionStore(root).get(id);
        if (match === "exact") {
          expect(restored.pendingNativeSession).toBeUndefined();
          expect(() => assertNativeSessionReady(restored)).not.toThrow();
          expect(
            (await createSessionStore(root).get(id)).pendingNativeSession,
          ).toBeUndefined();
        } else
          expect(() => assertNativeSessionReady(restored)).toThrow(/unknown/);
      } finally {
        await fs.rm(root, { recursive: true, force: true });
      }
    });
