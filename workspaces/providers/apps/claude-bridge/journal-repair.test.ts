import type { Session } from "./session-state.ts";
import type { StoreOptions } from "./session-store.ts";
import { object } from "./history-records.ts";
import assert from "node:assert/strict";
import { test } from "vitest";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createSessionStore } from "./session-store.ts";
import { startBridge } from "./testing/bridge-harness.ts";

const id = "11111111-1111-4111-8111-111111111111";
const userId = "22222222-2222-4222-8222-222222222222";
const answerId = "33333333-3333-4333-8333-333333333333";
async function fixture(
  run: (value: {
    root: string;
    native: string;
    journal: string;
    session: Session;
    options: StoreOptions;
    valid: string;
  }) => Promise<void>,
) {
  const root = await fs.realpath(
    await fs.mkdtemp(path.join(os.tmpdir(), "claude-journal-repair-")),
  );
  const configDir = path.join(root, "native");
  const session: Session = {
    id,
    cwd: root,
    turns: [
      {
        id: userId,
        status: "completed",
        items: [
          {
            id: userId,
            nativeId: userId,
            type: "userMessage",
            content: [{ type: "text", text: "Hello" }],
          },
          {
            id: "msg-answer",
            nativeId: answerId,
            type: "agentMessage",
            text: "Hello back",
            phase: "final_answer",
          },
        ],
      },
    ],
  };
  const native = path.join(
    configDir,
    "projects",
    root.replace(/[^a-zA-Z0-9]/g, "-"),
    id + ".jsonl",
  );
  const rows = [
    {
      type: "user",
      uuid: userId,
      parentUuid: null,
      message: { role: "user", content: "Hello" },
    },
    {
      type: "assistant",
      uuid: answerId,
      parentUuid: userId,
      message: {
        id: "msg-answer",
        role: "assistant",
        stop_reason: "end_turn",
        content: [{ type: "text", text: "Hello back" }],
      },
    },
  ].map((row) => ({ ...row, sessionId: id, cwd: root, isSidechain: false }));
  const store = createSessionStore(root);
  await store.persist(session);
  session.name = "Saved name";
  await store.persist(session);
  const journal = path.join(root, "sessions", id + ".jsonl");
  const valid = await fs.readFile(journal, "utf8");
  await fs.writeFile(journal, "{broken complete row}\n" + valid);
  await fs.mkdir(path.dirname(native), { recursive: true });
  await fs.writeFile(
    native,
    rows.map((row) => JSON.stringify(row) + "\n").join(""),
  );
  try {
    await run({
      root,
      native,
      journal,
      session,
      options: { nativeHistory: { configDir } },
      valid,
    });
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
}

test("malformed complete journal row repairs only with exact native history and reports skipped count", async () =>
  fixture(async (f) => {
    const restored = await createSessionStore(f.root, f.options).get(id);
    assert.deepEqual(restored.turns, f.session.turns);
    assert.equal(restored.name, "Saved name");
    assert.equal(restored.journalRepair?.status, "repaired");
    assert.equal(restored.journalRepair?.skippedRows, 1);
    assert.equal(restored.journalRepair?.scopeConfirmed, true);
    assert.equal(restored.journalRepair?.nativeHistoryConfirmed, true);
    assert.match(
      await fs.readFile(f.journal + ".corrupt", "utf8"),
      /broken complete row/,
    );
    assert.deepEqual(
      await createSessionStore(f.root, f.options).get(id),
      restored,
    );
  }));

for (const kind of [
  "missing",
  "mismatch",
  "torn",
  "sequence gap",
  "complete corrupt tail",
  "unsafe patch",
]) {
  test(`journal corruption needs repair when native proof is ${kind}`, async () =>
    fixture(async (f) => {
      if (kind === "missing") await fs.rm(f.native);
      if (kind === "mismatch")
        await fs.writeFile(
          f.native,
          (await fs.readFile(f.native, "utf8")).replace(
            "Hello back",
            "Different answer",
          ),
        );
      if (kind === "torn") await fs.appendFile(f.native, '{"type":');
      if (kind === "sequence gap")
        await fs.writeFile(
          f.journal,
          "{broken}\n" + f.valid.replace('"sequence":1', '"sequence":2'),
        );
      if (kind === "complete corrupt tail")
        await fs.writeFile(f.journal, f.valid + "{broken}\n");
      if (kind === "unsafe patch")
        await fs.appendFile(
          f.journal,
          JSON.stringify({
            sequence: 2,
            changes: [
              { op: "set", path: ["__proto__", "polluted"], value: true },
            ],
          }) + "\n",
        );
      const before = await fs.readFile(f.journal, "utf8");
      await assert.rejects(
        createSessionStore(f.root, f.options).get(id),
        (error: unknown) => {
          assert.ok(object(error));
          assert.equal(error.code, "CLAUDE_SESSION_REPAIR_REQUIRED");
          assert.ok(typeof error.message === "string");
          assert.match(error.message, /needs repair/);
          assert.ok(object(error.data));
          assert.ok(
            typeof error.data.skippedRows === "number" &&
              error.data.skippedRows >= 1,
          );
          return true;
        },
      );
      assert.equal(await fs.readFile(f.journal, "utf8"), before);
      assert.equal(Reflect.get({}, "polluted"), undefined);
      const marker = JSON.parse(
        await fs.readFile(
          path.join(f.root, "sessions", id + ".repair.json"),
          "utf8",
        ),
      );
      assert.equal(marker.status, "needs_repair");
      assert.equal(marker.nativeHistoryConfirmed, false);
      const read = await createSessionStore(f.root, f.options).repairReport(id);
      assert.deepEqual(read, marker);
      assert.equal(await fs.readFile(f.journal, "utf8"), before);
    }));
}

for (const confirmed of [true, false]) {
  test(`bridge history and resume ${confirmed ? "report confirmed repair" : "reject unconfirmed repair without crashing"}`, async () =>
    fixture(async (f) => {
      const bridge = await startBridge(
        new URL("./fixtures/history-readonly-sdk.ts", import.meta.url),
      );
      try {
        for (const suffix of [".json", ".jsonl", ".meta.json"])
          await fs.copyFile(
            path.join(f.root, "sessions", id + suffix),
            path.join(bridge.root, "state/sessions", id + suffix),
          );
        if (confirmed) {
          const target = path.join(
            bridge.root,
            "native-config/projects",
            f.root.replace(/[^a-zA-Z0-9]/g, "-"),
            id + ".jsonl",
          );
          await fs.mkdir(path.dirname(target), { recursive: true });
          await fs.copyFile(f.native, target);
        }
        for (const method of ["thread/read", "thread/resume"]) {
          const call = bridge.call(method, {
            threadId: id,
            includeTurns: true,
          });
          if (confirmed) {
            const result = await call;
            assert.equal(result.thread.journalRepair.skippedRows, 1);
            assert.deepEqual(result.thread.turns, f.session.turns);
          } else
            await assert.rejects(
              call,
              /needs repair: 1 malformed journal rows/,
            );
        }
        const report = await bridge.call("claude/repairReport", {
          threadId: id,
        });
        assert.equal(report.status, confirmed ? "repaired" : "needs_repair");
        assert.equal(report.skippedRows, 1);
        assert.equal(report.nativeHistoryConfirmed, confirmed);
        assert.equal(report.scopeConfirmed, true);
        assert.deepEqual(await bridge.call("thread/loaded/list", {}), {
          data: [],
          nextCursor: null,
        });
      } finally {
        await bridge.close();
      }
    }));
}

test("repair report is bounded, excludes private fields, and never repairs the journal", async () =>
  fixture(async (f) => {
    const store = createSessionStore(f.root, f.options);
    assert.equal((await store.repairReport(id)).status, "none");
    const marker = path.join(f.root, "sessions", id + ".repair.json");
    const before = await fs.readFile(f.journal, "utf8");
    await fs.writeFile(
      marker,
      JSON.stringify({
        status: "needs_repair",
        skippedRows: 1,
        invalidSequence: false,
        secret: "never return",
        path: "/private",
        nativeHistoryConfirmed: false,
      }),
    );
    assert.deepEqual(await store.repairReport(id), {
      status: "needs_repair",
      skippedRows: 1,
      invalidSequence: false,
      scopeConfirmed: null,
      nativeHistoryConfirmed: false,
    });
    assert.equal(await fs.readFile(f.journal, "utf8"), before);
    await fs.writeFile(marker, " ".repeat(20000));
    await assert.rejects(store.repairReport(id), /exceeds the read limit/);
    await assert.rejects(store.repairReport("../foreign"), /identity/);
  }));
