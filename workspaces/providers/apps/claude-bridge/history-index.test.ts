import type { ControlDependencies } from "./controls.ts";
import type { Session, Turn } from "./session-state.ts";
import { afterEach, expect, test } from "vitest";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { randomUUID } from "node:crypto";
import * as sdk from "@anthropic-ai/claude-agent-sdk";
import { readHistoryIndex, verifiedHistoryIndex } from "./history-index.ts";
import { forkAtTurn, rollbackSession } from "./controls.ts";

const noRestore = async () => {
  throw new Error("No file restore expected");
};
function controls(): ControlDependencies {
  return {
    ...sdk,
    persist: async () => {},
    getHistoryIndex: (id, options) =>
      verifiedHistoryIndex({ id, cwd: options!.dir! }),
    rewindFiles: noRestore,
    prepareFileRestore: noRestore,
    verifyFileRestore: noRestore,
  };
}
const roots: string[] = [];
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
  const ids: [string, string, string, string, string, string] = [
    randomUUID(),
    randomUUID(),
    randomUUID(),
    randomUUID(),
    randomUUID(),
    randomUUID(),
  ];
  const row = (uuid: string, index: number): sdk.SessionStoreEntry => ({
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
  });
  const rows: [
    sdk.SessionStoreEntry,
    sdk.SessionStoreEntry,
    sdk.SessionStoreEntry,
    sdk.SessionStoreEntry,
    sdk.SessionStoreEntry,
    sdk.SessionStoreEntry,
  ] = [
    row(ids[0], 0),
    row(ids[1], 1),
    row(ids[2], 2),
    row(ids[3], 3),
    row(ids[4], 4),
    row(ids[5], 5),
  ];
  const file = path.join(
    configDir,
    "projects",
    cwd.replace(/[^a-zA-Z0-9]/g, "-"),
    `${nativeId}.jsonl`,
  );
  await fs.mkdir(path.dirname(file), { recursive: true });
  await fs.writeFile(
    file,
    rows.map(JSON.stringify as (value: unknown) => string).join("\n") + "\n",
  );
  const session: Session & { nativeId: string } = {
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
      ...f.rows[2],
      uuid: damage === "malformed uuid" ? "bad-uuid" : randomUUID(),
      parentUuid: f.rows[2].uuid,
      ...(damage === "unknown record" ? { type: "future-input" } : {}),
    });
    await fs.writeFile(
      f.file,
      rows.map(JSON.stringify as (value: unknown) => string).join("\n") + "\n",
    );
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
    const deps = controls();
    const fork = await forkAtTurn(f.session, { lastTurnId: "turn0" }, deps);
    expect(fork.turns).toHaveLength(1);
    expect(fork.turns[0]?.items[0]?.nativeIds).toHaveLength(2);
    await rollbackSession(
      f.session,
      { requestId: randomUUID(), turnId: "turn3" },
      deps,
    );
    expect(f.session.turns).toHaveLength(1);
    expect(f.session.turns[0]?.items[0]?.nativeIds).toHaveLength(2);
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
    await fs.writeFile(
      f.file,
      rows.map(JSON.stringify as (value: unknown) => string).join("\n") + "\n",
    );
    const { createSessionStore } = await import("./session-store.ts");
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
    expect(restored.turns[0]?.status).toBe("uncertain");
    expect(restored.turns[0]?.startOutcome).toBe("preparing");
    expect(restored.turns[0]?.items[1]?.deliveryStatus).toBe("submitted");
    expect(restored.turns[0]).not.toHaveProperty("unappliedSteers");
  });

test("post-clear controls use current native turns while preserving earlier mirror history", async () => {
  const f = await fixture();
  for (const turn of f.session.turns) turn.nativeSessionId = f.session.nativeId;
  const earlier: Turn = {
    id: "before-clear",
    nativeSessionId: randomUUID(),
    status: "completed",
    items: [{ type: "userMessage", nativeId: randomUUID() }],
  };
  const clear: Turn = {
    id: "clear",
    nativeSessionId: f.session.nativeId,
    nativeHistoryDetached: true,
    status: "completed",
    items: [{ type: "userMessage", nativeId: randomUUID() }],
  };
  f.session.turns.unshift(earlier, clear);
  const deps = controls();
  const fork = await forkAtTurn(f.session, { lastTurnId: "turn0" }, deps);
  expect(fork.turns.map((turn: Turn) => turn.id)).toEqual([
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
] satisfies [string, Record<string, unknown>][])
  test(`unsupported structure cannot prove absence: ${kind}`, async () => {
    const f = await fixture();
    const rows = f.rows.slice(0, 3);
    rows.push({
      ...f.rows[2],
      uuid: randomUUID(),
      parentUuid: f.ids[2],
      ...change,
    });
    await fs.writeFile(
      f.file,
      rows.map(JSON.stringify as (value: unknown) => string).join("\n") + "\n",
    );
    const index = await readHistoryIndex(f.session);
    expect(index?.outcome([randomUUID()], [f.ids[1]]) ?? "unknown").toBe(
      "unknown",
    );
  });

test("installed alpha import retains exact ordered meta provenance", async () => {
  const f = await fixture();
  const entries: sdk.SessionStoreEntry[] = [];
  const keys: sdk.SessionKey[] = [];
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
      importSessionToStore: async (
        id: string,
        store: sdk.SessionStore,
        options: Parameters<typeof sdk.importSessionToStore>[2],
      ) => {
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
    // @ts-expect-error importSessionToStore intentionally returns an unsupported result shape.
    expect(await readHistoryIndex(f.session, { sessionApi })).toBeNull();
  });

test("bridge sources have no absolute host import dependencies", async () => {
  const directory = new URL(".", import.meta.url);
  const sources = (await fs.readdir(directory)).filter((name) =>
    /\.(?:mjs|mts|ts|cts)$/.test(name),
  );
  expect(sources).toEqual(
    expect.arrayContaining([
      "bridge.ts",
      "history-index.ts",
      "session-store.ts",
    ]),
  );
  for (const name of sources) {
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
] satisfies [string, unknown][])
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
    await fs.writeFile(
      f.file,
      rows.map(JSON.stringify as (value: unknown) => string).join("\n") + "\n",
    );
    const index = await readHistoryIndex(f.session);
    expect(index?.outcome([randomUUID()], [f.ids[1]]) ?? "unknown").toBe(
      "unknown",
    );
  });

for (const kind of ["system retry", "tool references", "non-input attachment"])
  test(`non-input ${kind} preserves complete input coverage`, async () => {
    const f = await fixture();
    const extra = {
      ...f.rows[5],
      uuid: randomUUID(),
      parentUuid: f.ids[5],
    };
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
      [...f.rows, extra]
        .map(JSON.stringify as (value: unknown) => string)
        .join("\n") + "\n",
    );
    const index = await readHistoryIndex(f.session);
    expect(index?.outcome([randomUUID()], [f.ids[1]]) ?? "unknown").toBe(
      "not_applied",
    );
    expect(index?.outcome([f.ids[1]], [f.ids[1]])).toBe("delivered");
  });

// Claude 2.1.310 writes CLI context attachments such as skill_listing.
for (const attachment of [
  {
    type: "future-input",
    content: [{ type: "text", text: "Studio instruction" }],
  },
  { type: "future-input", content: 42 },
  { type: "skill_listing", content: "- skill: description", skillCount: 1 },
])
  test(`an unclassified attachment leaves absence unknown and delivery provable: ${attachment.type}`, async () => {
    const f = await fixture();
    const row = {
      ...f.rows[5],
      type: "attachment",
      uuid: randomUUID(),
      parentUuid: f.ids[5],
      message: undefined,
      attachment,
    };
    await fs.writeFile(
      f.file,
      [...f.rows, row]
        .map(JSON.stringify as (value: unknown) => string)
        .join("\n") + "\n",
    );
    const index = await readHistoryIndex(f.session);
    expect(index?.outcome([randomUUID()], [f.ids[1]]) ?? "unknown").toBe(
      "unknown",
    );
    expect(index?.outcome([f.ids[1]], [f.ids[1]])).toBe("delivered");
  });

test("a Claude 2.1.310 steer receipt after opaque rows in another directory is delivered", async () => {
  const f = await fixture();
  const opaqueId = randomUUID();
  const steerId = randomUUID();
  const moved = path.join(f.cwd, "src");
  const rows = [
    ...f.rows.slice(0, 3),
    {
      ...f.rows[2],
      type: "attachment",
      uuid: opaqueId,
      parentUuid: f.ids[2],
      cwd: moved,
      message: undefined,
      attachment: { type: "skill_listing", content: "- skill: description" },
    },
    {
      ...f.rows[2],
      type: "attachment",
      uuid: randomUUID(),
      parentUuid: opaqueId,
      cwd: moved,
      message: undefined,
      attachment: {
        type: "queued_command",
        prompt: [{ type: "text", text: "steer" }],
        source_uuid: steerId,
        delivery_id: randomUUID(),
        commandMode: "prompt",
        origin: { kind: "unclassified" },
        timestamp: new Date().toISOString(),
        isMeta: true,
      },
    },
  ];
  await fs.writeFile(
    f.file,
    rows.map(JSON.stringify as (value: unknown) => string).join("\n") + "\n",
  );
  const index = await readHistoryIndex(f.session);
  expect(index?.outcome([steerId], [f.ids[1]])).toBe("delivered");
  expect(index?.outcome([randomUUID()], [f.ids[1]])).toBe("unknown");
});

test("native history controls refuse opaque records", async () => {
  const f = await fixture();
  const row = {
    ...f.rows[5],
    type: "attachment",
    uuid: randomUUID(),
    parentUuid: f.ids[5],
    message: undefined,
    attachment: { type: "skill_listing", content: "- skill: description" },
  };
  await fs.writeFile(
    f.file,
    [...f.rows, row]
      .map(JSON.stringify as (value: unknown) => string)
      .join("\n") + "\n",
  );
  await expect(verifiedHistoryIndex(f.session)).rejects.toThrow(
    /incomplete or uncertain/,
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
    [...f.rows, row]
      .map(JSON.stringify as (value: unknown) => string)
      .join("\n") + "\n",
  );
  const index = await readHistoryIndex(f.session);
  expect(index?.outcome([randomUUID()], [f.ids[1]]) ?? "unknown").toBe(
    "unknown",
  );
  expect(index?.outcome([f.ids[1]], [f.ids[1]])).toBe("delivered");
});

// Claude records the shell's current directory on each row after a `cd`.
test("rows recorded in another working directory keep input coverage", async () => {
  const f = await fixture();
  const rows = f.rows.map((row, index) =>
    index >= 3 ? { ...row, cwd: path.join(f.cwd, "src") } : row,
  );
  await fs.writeFile(
    f.file,
    rows.map(JSON.stringify as (value: unknown) => string).join("\n") + "\n",
  );
  const index = await readHistoryIndex(f.session);
  expect(index?.outcome([f.ids[4]], [f.ids[1]])).toBe("delivered");
  expect(index?.outcome([randomUUID()], [f.ids[1]])).toBe("not_applied");
});

for (const trailing of [false, true])
  test(`verified reader preserves input coverage through an attachment before a final reply${trailing ? " with trailing metadata" : ""}`, async () => {
    const f = await fixture();
    const attachmentId = randomUUID();
    const rows: sdk.SessionStoreEntry[] = [
      ...f.rows.slice(0, 2),
      {
        type: "attachment",
        uuid: attachmentId,
        parentUuid: f.ids[1],
        sessionId: f.session.nativeId,
        cwd: f.cwd,
        isSidechain: false,
        attachment: {
          type: "deferred_tools_delta",
          addedNames: [],
          addedLines: [],
          removedNames: [],
        },
      },
      { ...f.rows[2], parentUuid: attachmentId },
      ...(trailing ? [{ type: "cost-state", totalCost: 0 }] : []),
    ];
    await fs.writeFile(
      f.file,
      rows.map((row) => JSON.stringify(row) + "\n").join(""),
    );
    const index = await readHistoryIndex(f.session);
    expect(index?.outcome([f.ids[1]], [f.ids[0]])).toBe("delivered");
    expect(index?.messages.at(-1)?.uuid).toBe(f.ids[2]);
  });
