import { afterEach, expect, test } from "vitest";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { randomUUID } from "node:crypto";
import * as sdk from "@anthropic-ai/claude-agent-sdk";
import { readHistoryIndex, verifiedHistoryMessages } from "./history-index.mjs";
import { forkAtTurn, rollbackSession } from "./controls.mjs";

const roots = [];
const previous = process.env.CLAUDE_CONFIG_DIR;
afterEach(async () => {
  if (previous === undefined) delete process.env.CLAUDE_CONFIG_DIR;
  else process.env.CLAUDE_CONFIG_DIR = previous;
  await Promise.all(
    roots
      .splice(0)
      .map((root) => fs.rm(root, { recursive: true, force: true })),
  );
});
async function fixture(serviceFirst = true) {
  const base = path.join(os.homedir(), ".cache/codex-studio/pivot1-v5-history");
  await fs.mkdir(base, { recursive: true });
  const cwd = await fs.mkdtemp(path.join(base, "sdk-"));
  roots.push(cwd);
  const configDir = path.join(cwd, "config");
  process.env.CLAUDE_CONFIG_DIR = configDir;
  const nativeId = randomUUID();
  const ids = Array.from({ length: 6 }, () => randomUUID());
  const rows = ids.map((uuid, index) => ({
    type: index === 2 || index === 5 ? "assistant" : "user",
    uuid,
    parentUuid: index ? ids[index - 1] : null,
    sessionId: nativeId,
    cwd,
    isSidechain: false,
    timestamp: new Date().toISOString(),
    ...(index % 3 === (serviceFirst ? 0 : 1)
      ? { isMeta: true, origin: { kind: "unclassified" } }
      : {}),
    message: {
      role: index === 2 || index === 5 ? "assistant" : "user",
      content: [
        {
          type: "text",
          text:
            index === 2 || index === 5
              ? "reply"
              : index % 3 === (serviceFirst ? 0 : 1)
                ? "service"
                : "human",
        },
      ],
      ...(index === 2 || index === 5
        ? { id: `msg_${index}`, stop_reason: "end_turn" }
        : {}),
    },
  }));
  const file = path.join(
    configDir,
    "projects",
    cwd.replace(/[^a-zA-Z0-9]/g, "-"),
    `${nativeId}.jsonl`,
  );
  await fs.mkdir(path.dirname(file), { recursive: true });
  await fs.writeFile(file, rows.map(JSON.stringify).join("\n") + "\n");
  const session = {
    id: randomUUID(),
    nativeId,
    cwd,
    started: true,
    turns: [0, 3].map((index) => ({
      id: `turn${index}`,
      status: "completed",
      nativeMessageId: ids[index],
      items: [
        {
          type: "userMessage",
          nativeId: ids[index + 1],
          nativeIds: ids.slice(index, index + 2),
        },
      ],
    })),
  };
  return { cwd, configDir, file, rows, session, ids };
}

for (const damage of ["malformed uuid", "unknown record"])
  test(`SDK history rejects ${damage} before authorizing redelivery`, async () => {
    const f = await fixture();
    const rows = f.rows.slice(0, 3);
    rows.push({
      ...rows[2],
      uuid: damage === "malformed uuid" ? "bad-uuid" : randomUUID(),
      parentUuid: rows[2].uuid,
      ...(damage === "unknown record" ? { type: "future-input" } : {}),
    });
    await fs.writeFile(f.file, rows.map(JSON.stringify).join("\n") + "\n");
    // The installed SDK silently skips unknown rows and accepts malformed UUIDs.
    expect(
      (await sdk.getSessionMessages(f.session.nativeId, { dir: f.cwd })).length,
    ).toBeGreaterThan(0);
    expect(await readHistoryIndex(f.session)).toBeNull();
  });
for (const serviceFirst of [true, false])
  test(`SDK composite ${serviceFirst ? "service/human" : "human/service"} survives fork and rollback`, async () => {
    const f = await fixture(serviceFirst);
    const visible = await sdk.getSessionMessages(f.session.nativeId, {
      dir: f.cwd,
    });
    expect(visible).toHaveLength(4);
    const deps = {
      ...sdk,
      getSessionMessages: (id, options) =>
        verifiedHistoryMessages({ id, cwd: options.dir }),
      persist: async () => {},
    };
    const fork = await forkAtTurn(f.session, { lastTurnId: "turn0" }, deps);
    expect(fork.turns).toHaveLength(1);
    expect(fork.turns[0].items[0].nativeIds).toHaveLength(2);
    await rollbackSession(
      f.session,
      { requestId: randomUUID(), turnId: "turn3" },
      deps,
    );
    expect(f.session.turns).toHaveLength(1);
    expect(f.session.turns[0].items[0].nativeIds).toHaveLength(2);
  });

for (const damage of ["malformed uuid", "unknown record"])
  test(`restart preserves uncertain input receipts after ${damage}`, async () => {
    const f = await fixture(false);
    const rows = [
      f.rows[0],
      {
        ...f.rows[2],
        parentUuid: f.ids[0],
        uuid: damage === "malformed uuid" ? "bad-uuid" : randomUUID(),
        ...(damage === "unknown record" ? { type: "future-input" } : {}),
      },
    ];
    await fs.writeFile(f.file, rows.map(JSON.stringify).join("\n") + "\n");
    const { createSessionStore } = await import("./session-store.mjs");
    f.session.turns = [
      {
        id: "stale",
        status: "inProgress",
        startOutcome: "preparing",
        items: [
          { id: "initial", type: "userMessage", nativeId: f.ids[0] },
          {
            id: "steer",
            type: "userMessage",
            nativeId: f.ids[3],
            delivery: "steer",
            deliveryStatus: "submitted",
          },
        ],
      },
    ];
    const root = path.join(f.cwd, "state");
    await createSessionStore(root).persist(f.session);
    const restored = await createSessionStore(root).get(f.session.id);
    expect(restored.turns[0].status).toBe("uncertain");
    expect(restored.turns[0].startOutcome).toBe("preparing");
    expect(restored.turns[0].items[1].deliveryStatus).toBe("submitted");
    expect(restored.turns[0].unappliedSteers).toBeUndefined();
  });

test("post-clear controls use current native turns while preserving earlier mirror history", async () => {
  const f = await fixture();
  for (const turn of f.session.turns) turn.nativeSessionId = f.session.nativeId;
  const earlier = {
    id: "before-clear",
    nativeSessionId: randomUUID(),
    status: "completed",
    items: [{ type: "userMessage", nativeId: randomUUID() }],
  };
  const clear = {
    id: "clear",
    nativeSessionId: f.session.nativeId,
    nativeHistoryDetached: true,
    status: "completed",
    items: [{ type: "userMessage", nativeId: randomUUID() }],
  };
  f.session.turns.unshift(earlier, clear);
  const deps = {
    ...sdk,
    getSessionMessages: (id, options) =>
      verifiedHistoryMessages({ id, cwd: options.dir }),
    persist: async () => {},
  };
  const fork = await forkAtTurn(f.session, { lastTurnId: "turn0" }, deps);
  expect(fork.turns.map((turn) => turn.id)).toEqual([
    "before-clear",
    "clear",
    "turn0",
  ]);
  expect(fork.turns[0]).toEqual(earlier);
  await rollbackSession(
    f.session,
    { requestId: randomUUID(), turnId: "turn0" },
    deps,
  );
  expect(f.session.turns).toEqual([earlier, clear]);
  expect(f.session.started).toBe(false);
});

for (const [kind, change] of [
  ["malformed auxiliary", { type: "queue-operation", operation: 42 }],
  [
    "malformed content block",
    {
      type: "user",
      message: { role: "user", content: [{ type: "text", text: 42 }] },
    },
  ],
  [
    "unknown content block",
    {
      type: "user",
      message: {
        role: "user",
        content: [{ type: "future-input", text: "instruction" }],
      },
    },
  ],
])
  test(`unsupported structure cannot prove absence: ${kind}`, async () => {
    const f = await fixture();
    const rows = f.rows.slice(0, 3);
    rows.push({
      ...f.rows[2],
      uuid: randomUUID(),
      parentUuid: f.ids[2],
      ...change,
    });
    await fs.writeFile(f.file, rows.map(JSON.stringify).join("\n") + "\n");
    const index = await readHistoryIndex(f.session);
    expect(index?.outcome([randomUUID()], [f.ids[1]]) ?? "unknown").toBe(
      "unknown",
    );
  });

test("installed alpha import retains exact ordered meta provenance", async () => {
  const f = await fixture();
  const entries = [];
  const keys = [];
  const result = await sdk.importSessionToStore(
    f.session.nativeId,
    {
      append: async (key, batch) => {
        keys.push(key);
        entries.push(...batch);
      },
      load: async () => null,
    },
    { dir: f.cwd, includeSubagents: false },
  );
  expect(result).toBeUndefined();
  expect(keys.length).toBeGreaterThan(0);
  expect(
    keys.every(
      (key) =>
        key.sessionId === f.session.nativeId &&
        key.projectKey === f.cwd.replace(/[^a-zA-Z0-9]/g, "-") &&
        key.subpath === undefined,
    ),
  ).toBe(true);
  expect(entries).toEqual(f.rows);
  expect(entries.filter((row) => row.isMeta).map((row) => row.uuid)).toEqual([
    f.ids[0],
    f.ids[3],
  ]);
});
for (const behavior of [
  "throws",
  "unexpected result",
  "unexpected project key",
  "missing batch",
])
  test(`alpha import fails closed: ${behavior}`, async () => {
    const f = await fixture();
    const sessionApi = {
      ...sdk,
      importSessionToStore: async (id, store, options) => {
        if (behavior === "throws") throw new Error("alpha API changed");
        if (behavior === "missing batch") return;
        if (behavior === "unexpected project key")
          return store.append(
            { sessionId: id, projectKey: "unexpected" },
            f.rows,
          );
        await sdk.importSessionToStore(id, store, options);
        return { newShape: true };
      },
    };
    expect(await readHistoryIndex(f.session, { sessionApi })).toBeNull();
  });

test("bridge sources have no absolute host import dependencies", async () => {
  const directory = new URL(".", import.meta.url);
  for (const name of await fs.readdir(directory)) {
    if (!/\.(?:mjs|mts)$/.test(name)) continue;
    const source = await fs.readFile(new URL(name, directory), "utf8");
    expect(source, name).not.toMatch(
      /(?:from\s*|import\s*\()['"]\/(?:work|home|Users|tmp)\//,
    );
  }
});

for (const [field, value] of [
  ["content", 42],
  ["commandUuid", "not-a-uuid"],
  ["reason", []],
  ["deliveryId", {}],
])
  test(`queue input payload rejects malformed ${field}`, async () => {
    const f = await fixture();
    const rows = [
      ...f.rows,
      {
        type: "queue-operation",
        operation: "enqueue",
        timestamp: new Date().toISOString(),
        sessionId: f.session.nativeId,
        [field]: value,
      },
    ];
    await fs.writeFile(f.file, rows.map(JSON.stringify).join("\n") + "\n");
    const index = await readHistoryIndex(f.session);
    expect(index?.outcome([randomUUID()], [f.ids[1]]) ?? "unknown").toBe(
      "unknown",
    );
  });

for (const kind of ["system retry", "tool references", "non-input attachment"])
  test(`non-input ${kind} preserves complete input coverage`, async () => {
    const f = await fixture();
    const extra = { ...f.rows[5], uuid: randomUUID(), parentUuid: f.ids[5] };
    if (kind === "future system subtype")
      Object.assign(extra, {
        type: "system",
        subtype: "future-output",
        message: undefined,
      });
    if (kind === "system retry")
      Object.assign(extra, {
        type: "system",
        subtype: "api_error",
        source: "request_retry",
        message: undefined,
        error: { status: 503 },
      });
    if (kind === "tool references")
      Object.assign(extra, {
        type: "user",
        message: {
          role: "user",
          content: [
            {
              type: "tool_result",
              tool_use_id: "fixture",
              content: [
                { type: "tool_reference", tool_name: "mcp__fixture__echo" },
              ],
            },
          ],
        },
      });
    if (kind === "non-input attachment")
      Object.assign(extra, {
        type: "attachment",
        message: undefined,
        attachment: {
          type: "deferred_tools_delta",
          addedNames: ["mcp__fixture__echo"],
          addedLines: ["mcp__fixture__echo"],
          removedNames: [],
        },
      });
    await fs.writeFile(
      f.file,
      [...f.rows, extra].map(JSON.stringify).join("\n") + "\n",
    );
    const index = await readHistoryIndex(f.session);
    expect(index?.outcome([randomUUID()], [f.ids[1]]) ?? "unknown").toBe(
      "not_applied",
    );
    expect(index?.outcome([f.ids[1]], [f.ids[1]])).toBe("delivered");
  });

for (const content of [[{ type: "text", text: "Studio instruction" }], 42])
  test(`an unknown input-shaped attachment remains unknown: ${JSON.stringify(content)}`, async () => {
    const f = await fixture();
    const row = {
      ...f.rows[5],
      type: "attachment",
      uuid: randomUUID(),
      parentUuid: f.ids[5],
      message: undefined,
      attachment: { type: "future-input", content },
    };
    await fs.writeFile(
      f.file,
      [...f.rows, row].map(JSON.stringify).join("\n") + "\n",
    );
    const index = await readHistoryIndex(f.session);
    expect(index?.outcome([randomUUID()], [f.ids[1]]) ?? "unknown").toBe(
      "unknown",
    );
  });

test("a known output attachment with an input-shaped payload stays unknown", async () => {
  const f = await fixture();
  const row = {
    ...f.rows[5],
    type: "attachment",
    uuid: randomUUID(),
    parentUuid: f.ids[5],
    message: undefined,
    attachment: {
      type: "deferred_tools_record",
      entries: [],
      content: [{ type: "text", text: "Studio instruction" }],
    },
  };
  await fs.writeFile(
    f.file,
    [...f.rows, row].map(JSON.stringify).join("\n") + "\n",
  );
  const index = await readHistoryIndex(f.session);
  expect(index?.outcome([randomUUID()], [f.ids[1]]) ?? "unknown").toBe(
    "unknown",
  );
});
