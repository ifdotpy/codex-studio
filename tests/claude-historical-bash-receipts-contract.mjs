#!/usr/bin/env node
import assert from "node:assert/strict";
import { test } from "node:test";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import {
  HISTORICAL_BASH_TAIL_BYTES,
  reconcileHistoricalBash,
} from "../scripts/claude_bridge/historical-bash-receipts.mjs";
import { createSessionStore } from "../scripts/claude_bridge/session-store.mjs";

const bridgeDirectory = fileURLToPath(
  new URL("../scripts/claude_bridge/", import.meta.url),
);
const threadId = "11111111-1111-4111-8111-111111111111";
const assistantId = "22222222-2222-4222-8222-222222222222";
const resultId = "33333333-3333-4333-8333-333333333333";
const nativeId = "44444444-4444-4444-8444-444444444444";

async function fixture(t) {
  const root = await fs.realpath(
    await fs.mkdtemp(path.join(os.tmpdir(), "studio-bash-receipt-")),
  );
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  const configDir = path.join(root, "profile");
  const item = {
    id: "toolu_exact",
    type: "commandExecution",
    tool: "Bash",
    status: "inProgress",
    arguments: { command: "printf fixture", timeout: 1000 },
    command: "printf fixture",
    cwd: root,
    aggregatedOutput: "",
  };
  const session = {
    id: threadId,
    nativeId,
    cwd: root,
    started: true,
    model: "default",
    createdAt: 1,
    turns: [
      {
        id: "historical-turn",
        status: "failed",
        items: [item],
        error: { message: "Bridge disconnected" },
      },
      {
        id: "newer-turn",
        status: "completed",
        items: [{ id: "newer-input", type: "userMessage", text: "new input" }],
      },
    ],
  };
  const rows = [
    {
      type: "assistant",
      uuid: assistantId,
      sessionId: nativeId,
      cwd: root,
      isSidechain: false,
      timestamp: "2026-10-07T06:00:00.000Z",
      message: {
        content: [
          {
            type: "tool_use",
            id: item.id,
            name: "Bash",
            input: item.arguments,
          },
        ],
      },
    },
    {
      type: "user",
      uuid: resultId,
      parentUuid: assistantId,
      sourceToolAssistantUUID: assistantId,
      sessionId: nativeId,
      cwd: root,
      isSidechain: false,
      timestamp: "2026-10-07T06:00:01.000Z",
      message: {
        content: [
          {
            type: "tool_result",
            tool_use_id: item.id,
            is_error: false,
            content: "fixture",
          },
        ],
      },
      toolUseResult: {
        stdout: "fixture",
        stderr: "",
        interrupted: false,
        isImage: false,
      },
    },
  ];
  const file = path.join(
    configDir,
    "projects",
    root.replace(/[^a-zA-Z0-9]/g, "-"),
    nativeId + ".jsonl",
  );
  await fs.mkdir(path.dirname(file), { recursive: true });
  const write = (value = rows) =>
    fs.writeFile(
      file,
      value.map((row) => JSON.stringify(row)).join("\n") + "\n",
    );
  await write();
  const events = [];
  let persisted = 0;
  const run = (extra = {}) =>
    reconcileHistoricalBash(session, {
      configDir,
      isIdle: () => true,
      complete: (turn, completed, times) => {
        turn.items[turn.items.indexOf(item)] = completed;
        events.push({ turnId: turn.id, item: completed, ...times });
      },
      persist: async () => {
        persisted++;
      },
      ...extra,
    });
  return {
    root,
    configDir,
    session,
    item,
    rows,
    file,
    write,
    events,
    run,
    persisted: () => persisted,
  };
}

test("an exact foreground result settles only its historical item once", async (t) => {
  const f = await fixture(t);
  const before = structuredClone(f.session);
  assert.equal(await f.run(), 1);
  assert.equal(f.events.length, 1);
  assert.equal(f.events[0].turnId, "historical-turn");
  assert.equal(f.events[0].item.exitCode, 0);
  assert.equal(f.events[0].completedAtMs - f.events[0].startedAtMs, 1000);
  assert.equal(f.session.turns[0].status, before.turns[0].status);
  assert.deepEqual(f.session.turns[0].error, before.turns[0].error);
  assert.deepEqual(f.session.turns[1], before.turns[1]);
  assert.equal(await f.run(), 0);
  assert.equal(f.events.length, 1);
  assert.equal(f.persisted(), 1);
});

test("known invalid native proofs stay unknown", async (t) => {
  const cases = {
    account: async (f) => {
      await fs.rename(f.configDir, f.configDir + "-other");
    },
    session: (f) => {
      f.rows[1].sessionId = threadId;
    },
    cwd: (f) => {
      f.rows[1].cwd = "/another-project";
    },
    input: (f) => {
      f.rows[0].message.content[0].input = { command: "different" };
    },
    tool: (f) => {
      f.rows[0].message.content[0].name = "Read";
    },
    assistant: (f) => {
      f.rows[1].sourceToolAssistantUUID = resultId;
    },
    parent: (f) => {
      f.rows[1].parentUuid = resultId;
    },
    sidechain: (f) => {
      f.rows[1].isSidechain = true;
    },
    error: (f) => {
      f.rows[1].message.content[0].is_error = true;
    },
    interrupted: (f) => {
      f.rows[1].toolUseResult.interrupted = true;
    },
    background: (f) => {
      f.item.arguments.run_in_background = true;
    },
    backgroundReceipt: (f) => {
      f.rows[1].toolUseResult.backgroundTaskId = "task";
    },
    output: (f) => {
      f.rows[1].toolUseResult.stdout = "different";
    },
    image: (f) => {
      f.rows[1].toolUseResult.isImage = true;
    },
    time: (f) => {
      f.rows[1].timestamp = "2026-10-07T05:00:00.000Z";
    },
    compact: (f) => {
      f.rows.push({ type: "system", subtype: "compact_boundary" });
    },
    compactSummary: (f) => {
      f.rows[1].isCompactSummary = true;
    },
    duplicateUse: (f) => {
      f.rows.push(structuredClone(f.rows[0]));
    },
    duplicateResult: (f) => {
      f.rows.push(structuredClone(f.rows[1]));
    },
    duplicateStored: (f) => {
      f.session.turns[1].items.push(structuredClone(f.item));
    },
    reversedRecords: (f) => {
      f.rows.reverse();
    },
  };
  for (const [name, change] of Object.entries(cases)) {
    await t.test(name, async (sub) => {
      const f = await fixture(sub);
      await change(f);
      if (name !== "account") await f.write();
      assert.equal(await f.run(), 0);
      assert.equal(f.events.length, 0);
      assert.equal(f.item.status, "inProgress");
      assert.equal(f.persisted(), 0);
    });
  }
});

test("file and session mutation during a read reject the proof", async (t) => {
  for (const name of [
    "inode",
    "content",
    "item",
    "session",
    "newTurn",
    "busy",
  ]) {
    await t.test(name, async (sub) => {
      const f = await fixture(sub);
      let readCount = 0,
        idle = true;
      const filesystem = {
        ...fs,
        open: async (...args) => {
          const handle = await fs.open(...args);
          return {
            stat: () => handle.stat(),
            close: () => handle.close(),
            read: async (...values) => {
              const result = await handle.read(...values);
              if (++readCount === 1) {
                if (name === "inode") {
                  await fs.rename(f.file, f.file + ".previous");
                  await f.write();
                } else if (name === "content") {
                  f.rows[1].message.content[0].content = "changed";
                  await f.write();
                } else if (name === "item") f.item.command = "changed";
                else if (name === "session") f.session.nativeId = threadId;
                else if (name === "newTurn")
                  f.session.turns.push({ id: "racing-turn", items: [] });
                else idle = false;
              }
              return result;
            },
          };
        },
      };
      assert.equal(await f.run({ filesystem, isIdle: () => idle }), 0);
      assert.equal(f.events.length, 0);
    });
  }
});

test("old, torn, oversized and symbolic-link transcripts stay unknown", async (t) => {
  for (const name of ["outsideBudget", "torn", "oversized", "symlink"]) {
    await t.test(name, async (sub) => {
      const f = await fixture(sub);
      if (name === "outsideBudget") {
        const line =
          JSON.stringify({ type: "progress", padding: "x".repeat(1000) }) +
          "\n";
        await fs.appendFile(
          f.file,
          line.repeat(Math.ceil(HISTORICAL_BASH_TAIL_BYTES / line.length) + 1),
        );
      } else if (name === "torn") await fs.appendFile(f.file, '{"type":');
      else if (name === "oversized")
        await fs.appendFile(
          f.file,
          JSON.stringify({ padding: "x".repeat(65537) }) + "\n",
        );
      else {
        await fs.rename(f.file, f.file + ".original");
        await fs.symlink(f.file + ".original", f.file);
      }
      let bytes = 0;
      const filesystem = {
        ...fs,
        open: async (...args) => {
          const handle = await fs.open(...args);
          return {
            stat: () => handle.stat(),
            close: () => handle.close(),
            read: async (...values) => {
              bytes += values[2];
              return handle.read(...values);
            },
          };
        },
      };
      assert.equal(await f.run({ filesystem }), 0);
      assert.ok(
        bytes <= HISTORICAL_BASH_TAIL_BYTES,
        "unknown tail has one bounded read",
      );
      assert.equal(f.events.length, 0);
    });
  }
});

test("an active query does not open native history", async (t) => {
  const f = await fixture(t);
  let reads = 0;
  assert.equal(
    await f.run({
      isIdle: () => false,
      filesystem: {
        realpath: () => {
          reads++;
          return f.file;
        },
      },
    }),
    0,
  );
  assert.equal(reads, 0);
  assert.equal(f.events.length, 0);
});

async function bridgeFixture(t, f, { holdRead = false } = {}) {
  const directory = path.join(f.root, "bridge"),
    state = path.join(f.root, "state");
  await fs.mkdir(directory);
  for (const name of await fs.readdir(bridgeDirectory)) {
    if (!name.endsWith(".mjs")) continue;
    const source =
      name === "bridge.mjs" && process.env.STUDIO_BASH_RECEIPT_BRIDGE
        ? await fs.readFile(process.env.STUDIO_BASH_RECEIPT_BRIDGE, "utf8")
        : await fs.readFile(path.join(bridgeDirectory, name), "utf8");
    await fs.writeFile(
      path.join(directory, name),
      source.replaceAll("@anthropic-ai/claude-agent-sdk", "./fake.mjs"),
    );
  }
  await fs.symlink(
    path.join(bridgeDirectory, "node_modules"),
    path.join(directory, "node_modules"),
    "dir",
  );
  await fs.writeFile(
    path.join(directory, "fake.mjs"),
    `
    import fs from 'node:fs/promises';
    const open = fs.open.bind(fs);
    fs.open = async (file, ...args) => {
      const handle = await open(file, ...args);
      if (String(file) !== ${JSON.stringify(f.file)}) return handle;
      await fs.appendFile(${JSON.stringify(path.join(f.root, "native-reads"))}, 'read\\n');
      if (!${JSON.stringify(holdRead)}) return handle;
      return {
        stat: () => handle.stat(),
        close: () => handle.close(),
        read: async (...values) => {
          await fs.writeFile(${JSON.stringify(path.join(f.root, "native-read-held"))}, 'held');
          for (;;) {
            try { await fs.stat(${JSON.stringify(path.join(f.root, "native-read-release"))}); break; }
            catch (error) { if (error.code !== 'ENOENT') throw error; }
            await new Promise(resolve => setTimeout(resolve, 5));
          }
          return handle.read(...values);
        },
      };
    };
    export function query() { throw new Error('No native query is allowed'); }
    export const createSdkMcpServer = value => value;
    export const tool = () => ({});
    export const forkSession = () => { throw new Error('No fork is allowed'); };
    export const getSessionMessages = () => { throw new Error('No full history read is allowed'); };
  `,
  );
  await createSessionStore(state).persist(f.session);
  const child = spawn(
    process.execPath,
    [path.join(directory, "bridge.mjs"), state],
    {
      env: {
        ...process.env,
        CLAUDE_CONFIG_DIR: f.configDir,
        STUDIO_CLAUDE_ACCOUNT: "fixture@example.test",
      },
      stdio: ["pipe", "pipe", "pipe"],
    },
  );
  let stderr = "",
    sequence = 0;
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });
  const events = [],
    waiting = new Map();
  child.once("exit", (code) => {
    for (const request of waiting.values())
      request.reject(new Error("Owned bridge exited: " + code));
    waiting.clear();
  });
  createInterface({ input: child.stdout }).on("line", (line) => {
    const row = JSON.parse(line);
    if (row.id !== undefined) {
      const request = waiting.get(row.id);
      waiting.delete(row.id);
      if (row.error) request?.reject(new Error(row.error.message));
      else request?.resolve(row.result);
    } else events.push(row);
  });
  t.after(async () => {
    child.stdin.end();
    if (child.exitCode === null)
      await new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
          child.kill();
          reject(new Error("Owned fixture did not exit"));
        }, 5000);
        child.once("exit", () => {
          clearTimeout(timer);
          resolve();
        });
      });
    assert.equal(stderr, "");
  });
  const call = (method, params) =>
    new Promise((resolve, reject) => {
      const id = ++sequence;
      const timer = setTimeout(() => {
        waiting.delete(id);
        reject(new Error("Fixture response timeout"));
      }, 5000);
      waiting.set(id, {
        resolve: (value) => {
          clearTimeout(timer);
          resolve(value);
        },
        reject: (error) => {
          clearTimeout(timer);
          reject(error);
        },
      });
      child.stdin.write(JSON.stringify({ id, method, params }) + "\n");
    });
  return { call, events, state };
}

test("the real bridge resume emits the original turn receipt and preserves both turn statuses", async (t) => {
  const f = await fixture(t),
    b = await bridgeFixture(t, f);
  const result = await b.call("thread/resume", {
    threadId,
    excludeTurns: false,
  });
  const completed = b.events.filter((row) => row.method === "item/completed");
  assert.equal(
    completed.length,
    1,
    "resume must consume the lost historical receipt",
  );
  assert.equal(completed[0].params.threadId, threadId);
  assert.equal(completed[0].params.turnId, "historical-turn");
  assert.equal(completed[0].params.item.id, f.item.id);
  assert.equal(
    completed[0].params.completedAtMs - completed[0].params.startedAtMs,
    1000,
  );
  assert.equal(result.thread.turns[0].items[0].status, "completed");
  assert.equal(result.thread.turns[0].status, "failed");
  assert.deepEqual(result.thread.turns[1], f.session.turns[1]);
  const saved = await createSessionStore(b.state).get(threadId);
  assert.equal(saved.turns[0].items[0].status, "completed");
  assert.equal(saved.turns[0].status, "failed");
  await b.call("thread/resume", { threadId });
  assert.equal(
    b.events.filter((row) => row.method === "item/completed").length,
    1,
  );
});

test("a held native tail read leaves cheap bridge metadata available", async (t) => {
  const f = await fixture(t),
    b = await bridgeFixture(t, f, { holdRead: true });
  const resume = b.call("thread/resume", { threadId });
  const deadline = Date.now() + 3000;
  for (;;) {
    try {
      await fs.stat(path.join(f.root, "native-read-held"));
      break;
    } catch (error) {
      if (error.code !== "ENOENT") throw error;
    }
    assert.ok(Date.now() < deadline, "the fixture must enter the native read");
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  try {
    const metadata = await b.call("thread/read", {
      threadId,
      includeTurns: false,
    });
    assert.equal(metadata.thread.id, threadId);
    const diagnostics = await b.call("claude/diagnostics", {});
    assert.equal(diagnostics.liveQueries, 0);
    assert.equal(b.events.length, 0);
  } finally {
    await fs.writeFile(path.join(f.root, "native-read-release"), "release");
    await resume;
  }
  assert.equal(
    b.events.filter((row) => row.method === "item/completed").length,
    1,
  );
});

test("state polls and read requests do not consume native receipts", async (t) => {
  const f = await fixture(t),
    b = await bridgeFixture(t, f);
  await b.call("claude/state", { threadId });
  const result = await b.call("thread/read", { threadId, includeTurns: true });
  assert.equal(result.thread.turns[0].items[0].status, "inProgress");
  assert.equal(b.events.length, 0);
  await assert.rejects(fs.stat(path.join(f.root, "native-reads")), {
    code: "ENOENT",
  });
});
