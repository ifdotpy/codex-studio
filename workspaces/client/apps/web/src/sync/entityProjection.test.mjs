import assert from "node:assert/strict";
import {
  applyEntityChanges,
  applyEntityRows,
  emptyEntityProjection,
} from "./entityProjection.ts";
import {
  boundTranscriptItems,
  trimTranscriptPageCache,
} from "../transcriptPageBounds.ts";
import {
  historyPresentationGroups,
  incrementalHistoryGroups,
} from "../components/turnHistoryModel.ts";

import { it } from "vitest";

it("projects bounded workspace summaries and receipts with deletes", () => {
  const state = emptyEntityProjection();
  const row = (collection, id, value, seq) => ({
    id: `entity:${collection}:${id}`,
    seq,
    payload: "",
    entity: { collection, id, value },
  });
  const first = applyEntityRows(
    state,
    [
      row(
        "messageReceipt",
        "m",
        { id: "m", agent: "a", status: "pending", materialized: false },
        1,
      ),
      row(
        "plan",
        "a",
        { id: "a", rootId: "a", text: "Plan", version: 1, updated: 1 },
        2,
      ),
      row(
        "annotation",
        "n",
        {
          id: "n",
          agent: "a",
          rootId: "a",
          path: "a.rs",
          line: 1,
          text: "Comment",
          created: 1,
        },
        3,
      ),
    ],
    true,
  );
  assert.equal(first.runtime.messageReceipts[0].status, "pending");
  assert.equal(first.runtime.plans[0].text, "Plan");
  assert.equal(first.runtime.annotations[0].text, "Comment");
  const next = applyEntityChanges(
    state,
    [{ id: "entity:annotation:n", seq: 4, payload: "", _deleted: true }],
    true,
  );
  assert.equal(next.runtime.annotations.length, 0);
  assert.equal(next.runtime.messageReceipts, first.runtime.messageReceipts);
});

it("reuses unchanged entity data and bounds transcript and history caches", () => {
  const row = (collection, id, value, seq, deleted = false) => ({
    id: `entity:${collection}:${id}`,
    payload: "",
    entity: { collection, id, value },
    seq,
    ...(deleted ? { _deleted: true } : {}),
  });
  const state = emptyEntityProjection();
  let parsed = 0;
  const parse = JSON.parse;
  JSON.parse = (...args) => {
    parsed++;
    return parse(...args);
  };
  try {
    const initial = [
      row("agent", "a", { id: "a", name: "A" }, 1),
      row("agent", "b", { id: "b", name: "B" }, 2),
      row("project", "p", { id: "p", name: "P" }, 3),
    ];
    const first = applyEntityRows(state, initial, true);
    assert.equal(parsed, 0, "structured entity rows need no JSON parsing");
    const agentA = first.threads[0];
    const projectList = first.runtime.projects;

    const changed = applyEntityRows(
      state,
      [initial[0], row("agent", "b", { id: "b", name: "B2" }, 4), initial[2]],
      true,
    );
    assert.equal(parsed, 0, "structured updates need no JSON parsing");
    assert.equal(
      changed.threads[0],
      agentA,
      "unchanged entity references remain stable",
    );
    assert.equal(
      changed.runtime.projects,
      projectList,
      "unchanged collection arrays remain stable",
    );
    assert.equal(changed.threads[1].name, "B2");

    const stale = applyEntityRows(
      state,
      [
        initial[0],
        row("agent", "b", { id: "b", name: "stale" }, 3),
        initial[2],
      ],
      true,
    );
    assert.equal(
      stale.threads[1].name,
      "B2",
      "older sequences cannot replace newer values",
    );
    assert.equal(parsed, 0, "stale sequences are not parsed");

    const deleted = applyEntityRows(
      state,
      [initial[0], row("agent", "b", {}, 5, true), initial[2]],
      true,
    );
    assert.deepEqual(
      deleted.threads.map((agent) => agent.id),
      ["a"],
    );
    assert.equal(parsed, 0, "tombstones do not parse payloads");
    const afterStale = applyEntityRows(
      state,
      [
        initial[0],
        row("agent", "b", { id: "b", name: "stale" }, 4),
        initial[2],
      ],
      true,
    );
    assert.deepEqual(
      afterStale.threads.map((agent) => agent.id),
      ["a"],
    );
    assert.equal(parsed, 0, "an older row cannot resurrect a newer delete");
    const restored = applyEntityRows(
      state,
      [
        initial[0],
        row("agent", "b", { id: "b", name: "restored" }, 6),
        initial[2],
      ],
      true,
    );
    assert.deepEqual(
      restored.threads.map((agent) => agent.id),
      ["a", "b"],
    );
    assert.equal(restored.threads[1].name, "restored");
  } finally {
    JSON.parse = parse;
  }

  const items = Array.from({ length: 300 }, (_, index) => ({
    id: `m${index}`,
    role: "assistant",
    text: "x".repeat(100),
  }));
  const bounded = boundTranscriptItems(
    items,
    (item) => item.text.length * 2,
    "oldest",
  );
  assert.equal(bounded.items.length, 240);
  assert.ok(bounded.bytes <= 4_000_000);
  assert.equal(bounded.items[0].id, "m0");
  assert.equal(bounded.items.at(-1).id, "m239");
  assert.equal(bounded.droppedNewest, 60);
  const anchorItems = Array.from({ length: 500 }, (_, index) => ({
    id: `anchor${index}`,
    role: "assistant",
    text: "x".repeat(100),
  }));
  const anchored = boundTranscriptItems(
    anchorItems,
    (item) => item.text.length * 2,
    "oldest",
    "anchor250",
  );
  assert.equal(anchored.items.length, 240);
  assert.ok(anchored.items.some((item) => item.id === "anchor250"));
  assert.ok(anchored.droppedOldest > 0 && anchored.droppedNewest > 0);
  const byBytes = boundTranscriptItems(
    items.slice(0, 20),
    (item) => item.text.length * 2,
    "newest",
  );
  assert.ok(byBytes.bytes <= 4_000_000);

  const message = (id, role, turnId, extra = {}) => ({
    id,
    role,
    turnId,
    text: id,
    ...extra,
  });
  const transcript = [
    message("u1", "user", "t1"),
    message("a1", "assistant", "t1", { turnStatus: "completed" }),
    message("u2", "user", "t2"),
    message("a2", "assistant", "t2", { turnStatus: "completed" }),
  ];
  const firstGroups = incrementalHistoryGroups(transcript, undefined, null);
  const edited = [
    ...transcript.slice(0, 3),
    { ...transcript[3], text: "updated" },
  ];
  const nextGroups = incrementalHistoryGroups(edited, undefined, {
    items: transcript,
    groups: firstGroups,
  });
  assert.equal(
    nextGroups[0],
    firstGroups[0],
    "unchanged groups keep references",
  );
  assert.notEqual(
    nextGroups[3],
    firstGroups[3],
    "the changed group is rebuilt",
  );
  const editedMiddle = [
    transcript[0],
    { ...transcript[1], text: "changed" },
    ...transcript.slice(2),
  ];
  const middleGroups = incrementalHistoryGroups(editedMiddle, undefined, {
    items: transcript,
    groups: firstGroups,
  });
  assert.notEqual(middleGroups[1], firstGroups[1]);
  assert.equal(
    middleGroups[2],
    firstGroups[2],
    "later unchanged groups keep references",
  );
  const appended = [...transcript, message("tool2", "tool", "t2")];
  const appendGroups = incrementalHistoryGroups(appended, undefined, {
    items: transcript,
    groups: firstGroups,
  });
  assert.equal(appendGroups.length, firstGroups.length);
  assert.deepEqual(
    appendGroups.at(-1).items.map((item) => item.id),
    ["a2", "tool2"],
  );
  const runGroups = [
    {
      id: "tool-a",
      items: [
        message("tool-a", "tool", "a", { turnStatus: "completed" }),
        message("reasoning-a", "reasoning", "a", { turnStatus: "completed" }),
      ],
      outcome: "completed",
    },
    {
      id: "tool-b",
      items: [message("tool-b", "output", "b", { turnStatus: "completed" })],
      outcome: "completed",
    },
  ];
  const presentation = historyPresentationGroups(runGroups);
  assert.equal(
    presentation.length,
    1,
    "adjacent completed tool turns share a disclosure",
  );
  assert.deepEqual(
    presentation[0].items.map((item) => item.id),
    ["tool-a", "reasoning-a", "tool-b"],
    "reasoning remains chronological between tools",
  );
  assert.equal(
    presentation[0].turns.length,
    2,
    "native turn records remain attributable",
  );
  const activePresentation = historyPresentationGroups([
    runGroups[0],
    {
      ...runGroups[1],
      outcome: undefined,
      items: [message("tool-b-live", "output", "b", { toolStatus: "running" })],
    },
  ]);
  assert.equal(
    activePresentation.length,
    1,
    "a live adjacent turn stays in the run",
  );
  assert.equal(activePresentation[0].outcome, undefined);
  const completedPresentation = historyPresentationGroups([
    runGroups[0],
    {
      ...runGroups[1],
      items: [
        message("tool-b-live", "output", "b", {
          toolStatus: "completed",
          turnStatus: "completed",
        }),
      ],
    },
  ]);
  assert.equal(
    completedPresentation[0].id,
    activePresentation[0].id,
    "terminal status keeps the shared presentation identity",
  );
  assert.equal(
    historyPresentationGroups([
      ...runGroups.slice(0, 1),
      {
        ...runGroups[1],
        items: [message("commentary", "assistant", "b", { text: "visible" })],
      },
    ]).length,
    2,
    "visible commentary splits activity",
  );
  assert.equal(
    historyPresentationGroups([
      ...runGroups.slice(0, 1),
      { ...runGroups[1], outcome: "failed" },
    ]).length,
    1,
    "failed turns keep their native outcome within the shared presentation",
  );
  assert.equal(
    historyPresentationGroups([
      ...runGroups.slice(0, 1),
      { ...runGroups[1], outcome: "interrupted" },
    ])[0].outcome,
    "interrupted",
    "interrupted status stays attributable to the last native turn",
  );
  assert.equal(
    historyPresentationGroups([
      ...runGroups.slice(0, 1),
      {
        ...runGroups[1],
        items: [
          message("file", "output", "b", { text: '{"type":"fileChange"}' }),
        ],
      },
    ]).length,
    2,
    "file changes split activity",
  );
  const bigBound = boundTranscriptItems(
    Array.from({ length: 10 }, (_, index) => ({
      id: `big${index}`,
      role: "assistant",
      text: "x".repeat(1_100_000),
    })),
    (item) => item.text.length * 2,
    "newest",
  );
  assert.ok(
    bigBound.bytes <= 4_000_000,
    "the byte limit bounds large messages",
  );
  assert.ok(bigBound.items.length < 10);
  const cache = new Map(
    Array.from({ length: 4 }, (_, index) => [
      `chat${index}`,
      { items: items.slice(index, index + 1), size: 4_000_000 },
    ]),
  );
  trimTranscriptPageCache(cache, "chat3");
  assert.deepEqual([...cache.keys()], ["chat1", "chat2", "chat3"]);
  console.log(
    "PASS: renderer projection deltas, transcript bounds, incremental groups",
  );
});

const entityRow = (collection, id, value, seq, extra = {}) => ({
  id: `entity:${collection}:${id}`,
  payload: "",
  entity: { collection, id, value },
  seq,
  ...extra,
});
const keyedEntityRow = (rowId, entityId, value, seq) => ({
  id: `entity:agent:${rowId}`,
  payload: "",
  entity: { collection: "agent", id: entityId, value },
  seq,
});
const payloadRow = (rowId, collection, entityId, value, seq) => ({
  id: `entity:source:${rowId}`,
  payload: "",
  entity: { collection, id: entityId, value },
  seq,
});

it("projects entity rows and preserves server sequence ordering", () => {
  const state = emptyEntityProjection();
  const initial = applyEntityRows(
    state,
    [
      { id: "state:ready", payload: "ignored", seq: 1 },
      { id: "other:agent", payload: "ignored", seq: 1 },
      entityRow("agent", "a", { id: "a", name: "A" }, 2),
    ],
    true,
  );
  assert.deepEqual(initial.threads, [{ id: "a", name: "A" }]);

  const unchanged = applyEntityRows(
    state,
    [
      entityRow("agent", "a", { id: "a", name: "equal" }, 2),
      entityRow("agent", "a", { id: "a", name: "older" }, 1),
    ],
    true,
  );
  assert.equal(unchanged, initial, "equal and older sequences are ignored");
  assert.deepEqual(unchanged.threads, [{ id: "a", name: "A" }]);
  const unchangedForIrrelevantRows = applyEntityRows(
    state,
    [
      entityRow("agent", "a", { id: "a", name: "A" }, 2),
      { id: "other:agent", payload: "ignored", seq: 3 },
    ],
    true,
  );
  assert.equal(
    unchangedForIrrelevantRows,
    initial,
    "rows outside the entity range do not change the snapshot",
  );
});

it("projects every collection and preserves other collection identities", () => {
  const collections = [
    "agent",
    "room",
    "task",
    "monitor",
    "complaint",
    "request",
    "rule",
    "project",
    "peerTeam",
    "chat",
    "edge",
    "event",
    "work",
    "workspace",
  ];
  const runtimeNames = {
    agent: "agents",
    room: "rooms",
    task: "tasks",
    monitor: "monitors",
    complaint: "complaints",
    request: "requests",
    rule: "rules",
    project: "projects",
    peerTeam: "peerTeams",
    chat: "chats",
    edge: "edges",
    event: "events",
    work: "work",
    workspace: null,
  };
  const state = emptyEntityProjection();
  const first = applyEntityRows(
    state,
    collections.map((collection, index) =>
      entityRow(
        collection,
        collection === "workspace" ? "current" : collection,
        { id: collection, name: collection },
        index + 1,
      ),
    ),
    true,
  );
  for (const [collection, runtimeName] of Object.entries(runtimeNames)) {
    if (collection === "chat") assert.equal(first.chats.length, 1);
    else if (collection === "edge") assert.equal(first.edges.length, 1);
    else if (collection === "agent") assert.equal(first.threads.length, 1);
    else if (runtimeName) assert.equal(first.runtime[runtimeName].length, 1);
  }

  const priorArrays = new Map(
    collections.map((collection) => {
      if (collection === "chat") return [collection, first.chats];
      if (collection === "edge") return [collection, first.edges];
      if (collection === "agent") return [collection, first.threads];
      const name = runtimeNames[collection];
      return [collection, name ? first.runtime[name] : null];
    }),
  );
  const changed = applyEntityRows(
    state,
    [
      ...collections
        .slice(0, -1)
        .map((collection, index) =>
          entityRow(
            collection,
            collection,
            { id: collection, name: collection },
            index + 1,
          ),
        ),
      entityRow("project", "project", { id: "project", name: "changed" }, 20),
      entityRow("workspace", "current", { stateDir: "/tmp/state" }, 14),
    ],
    true,
  );
  for (const [collection, priorArray] of priorArrays) {
    if (collection === "workspace") continue;
    const nextArray =
      collection === "chat"
        ? changed.chats
        : collection === "edge"
          ? changed.edges
          : collection === "agent"
            ? changed.threads
            : changed.runtime[runtimeNames[collection]];
    if (collection === "project") assert.notEqual(nextArray, priorArray);
    else
      assert.equal(nextArray, priorArray, `${collection} identity is stable`);
  }
});

it("keeps chats after an unrelated entity update", () => {
  const state = emptyEntityProjection();
  const initial = applyEntityRows(
    state,
    [
      entityRow("chat", "c", { id: "c", title: "Shared chat" }, 1),
      entityRow("agent", "a", { id: "a", name: "A" }, 2),
    ],
    true,
  );
  const chats = initial.chats;
  const updated = applyEntityRows(
    state,
    [
      entityRow("chat", "c", { id: "c", title: "Shared chat" }, 1),
      entityRow("agent", "a", { id: "a", name: "Updated A" }, 3),
    ],
    true,
  );
  assert.equal(updated.chats, chats);
  assert.deepEqual(updated.chats, [{ id: "c", title: "Shared chat" }]);
});

it("skips malformed entity rows once and still applies the rest of the batch", () => {
  const state = emptyEntityProjection();
  const initial = applyEntityRows(
    state,
    [
      entityRow("agent", "a", { id: "a", name: "A1" }, 1),
      entityRow("project", "p", { id: "p", name: "P1" }, 2),
    ],
    true,
  );
  const malformedRows = [
    { id: "entity:broken:json", payload: "{", seq: 4 },
    { id: "entity:broken:empty", payload: "", seq: 5 },
    { id: "entity:broken:null", payload: "null", seq: 6 },
    { id: "entity:broken:number", payload: "42", seq: 7 },
    { id: "entity:broken:array", payload: "[]", seq: 8 },
    {
      id: "entity:agent:missing-id",
      entity: { collection: "agent", value: { name: "A" } },
      seq: 9,
    },
    {
      id: "entity:agent:null-value",
      entity: { collection: "agent", id: "a", value: null },
      seq: 10,
    },
    {
      id: "entity:agent:missing-value",
      entity: { collection: "agent", id: "a" },
      seq: 11,
    },
    {
      id: "entity:future:x",
      entity: { collection: "future", id: "x", value: {} },
      seq: 12,
    },
  ];
  const reports = [];
  const report = console.error;
  console.error = (...args) => reports.push(args);
  try {
    const changed = applyEntityRows(
      state,
      [
        entityRow("agent", "a", { id: "a", name: "A2" }, 3),
        ...malformedRows,
        entityRow("project", "p", { id: "p", name: "P2" }, 6),
      ],
      true,
    );
    assert.equal(changed.threads[0].name, "A2");
    assert.equal(changed.runtime.projects[0].name, "P2");
    assert.equal(changed.chats, initial.chats);
    assert.equal(reports.length, malformedRows.length);
    assert.ok(
      reports.every(
        ([message, fields]) =>
          message.includes("invalid payload") && message.includes(fields.id),
      ),
    );
    assert.ok(reports.every(([, fields]) => !Object.hasOwn(fields, "payload")));

    const repeated = applyEntityRows(
      state,
      [
        entityRow("agent", "a", { id: "a", name: "A2" }, 3),
        ...malformedRows,
        entityRow("project", "p", { id: "p", name: "P2" }, 6),
      ],
      true,
    );
    assert.equal(repeated, changed);
    assert.equal(
      reports.length,
      malformedRows.length,
      "the same invalid row versions report once",
    );
  } finally {
    console.error = report;
  }
});

it("moves replaced entities between collections and removes rows omitted by RxDB", () => {
  const state = emptyEntityProjection();
  const first = applyEntityRows(
    state,
    [entityRow("agent", "a", { id: "a", name: "A" }, 1)],
    true,
  );
  const moved = applyEntityRows(
    state,
    [entityRow("project", "p", { id: "p", name: "P" }, 2)],
    true,
  );
  assert.deepEqual(first.threads, [{ id: "a", name: "A" }]);
  assert.deepEqual(moved.threads, []);
  assert.deepEqual(moved.runtime.projects, [{ id: "p", name: "P" }]);
  assert.equal(state.rows.get("entity:agent:a").deleted, true);
  assert.deepEqual(state.values.agent.size, 0);

  const removed = applyEntityRows(state, [], true);
  assert.deepEqual(removed.runtime.projects, []);
  assert.equal(state.rows.get("entity:project:p").deleted, true);
  assert.equal(
    applyEntityRows(state, [], true),
    removed,
    "already deleted rows stay unchanged when omitted again",
  );
});

it("handles explicit deletes, unseen tombstones, and re-created rows", () => {
  const state = emptyEntityProjection();
  let snapshot = applyEntityRows(
    state,
    [
      entityRow("agent", "a", { id: "a" }, 1),
      entityRow("chat", "c", { id: "c" }, 2, { _deleted: true }),
    ],
    true,
  );
  assert.deepEqual(snapshot.threads, [{ id: "a" }]);
  assert.deepEqual(state.rows.get("entity:chat:c"), {
    seq: 2,
    deleted: true,
  });

  const unchanged = applyEntityRows(
    state,
    [
      entityRow("agent", "a", { id: "a" }, 1),
      entityRow("chat", "unknown", {}, 3, { _deleted: true }),
    ],
    true,
  );
  assert.equal(
    unchanged,
    snapshot,
    "deleting an unseen row does not change projected data",
  );

  snapshot = applyEntityRows(
    state,
    [entityRow("agent", "a", {}, 3, { _deleted: true })],
    true,
  );
  assert.deepEqual(snapshot.threads, []);
  assert.equal(state.rows.get("entity:agent:a").deleted, true);

  snapshot = applyEntityRows(
    state,
    [entityRow("agent", "a", { id: "a", name: "back" }, 4)],
    true,
  );
  assert.deepEqual(snapshot.threads, [{ id: "a", name: "back" }]);
  assert.equal(state.rows.get("entity:agent:a").deleted, undefined);
});

it("waits for readiness while retaining the latest entity rows", () => {
  const state = emptyEntityProjection();
  assert.equal(
    applyEntityRows(state, [entityRow("agent", "a", { id: "a" }, 1)], false),
    null,
  );
  const ready = applyEntityRows(
    state,
    [entityRow("agent", "a", { id: "a" }, 1)],
    true,
  );
  assert.deepEqual(ready.threads, [{ id: "a" }]);
});

it("updates stable entities in place and relocates changed entity IDs", () => {
  const state = emptyEntityProjection();
  applyEntityRows(
    state,
    [
      keyedEntityRow("row-a", "a", { id: "a", name: "A" }, 1),
      keyedEntityRow("row-b", "b", { id: "b", name: "B" }, 2),
    ],
    true,
  );
  const stable = applyEntityRows(
    state,
    [
      keyedEntityRow("row-a", "a", { id: "a", name: "A2" }, 3),
      keyedEntityRow("row-b", "b", { id: "b", name: "B" }, 2),
    ],
    true,
  );
  assert.deepEqual(
    stable.threads.map(({ id }) => id),
    ["a", "b"],
  );
  assert.equal(stable.threads[0].name, "A2");

  const relocated = applyEntityRows(
    state,
    [
      keyedEntityRow("row-a", "renamed", { id: "renamed", name: "A3" }, 4),
      keyedEntityRow("row-b", "b", { id: "b", name: "B" }, 2),
    ],
    true,
  );
  assert.deepEqual(relocated.threads, [
    { id: "b", name: "B" },
    { id: "renamed", name: "A3" },
  ]);
  assert.equal(state.values.agent.has("a"), false);
});

it("moves a stable server row between entity collections", () => {
  const state = emptyEntityProjection();
  applyEntityRows(
    state,
    [payloadRow("row", "agent", "a", { id: "a" }, 1)],
    true,
  );
  const moved = applyEntityRows(
    state,
    [payloadRow("row", "project", "a", { id: "a" }, 2)],
    true,
  );
  assert.deepEqual(moved.threads, []);
  assert.deepEqual(moved.runtime.projects, [{ id: "a" }]);
});

it("moves newly omitted tombstones to the end of retention order", () => {
  const state = emptyEntityProjection();
  applyEntityRows(
    state,
    [
      entityRow("agent", "a", { id: "a" }, 1),
      entityRow("agent", "b", { id: "b" }, 2),
    ],
    false,
  );
  applyEntityRows(state, [entityRow("agent", "b", { id: "b" }, 2)], false);
  assert.deepEqual(
    [...state.rows.keys()],
    ["entity:agent:b", "entity:agent:a"],
  );
});

it("initializes empty runtime collections and reuses agents on unrelated updates", () => {
  const empty = applyEntityRows(emptyEntityProjection(), [], true);
  for (const key of [
    "agents",
    "rooms",
    "tasks",
    "monitors",
    "complaints",
    "requests",
    "rules",
    "projects",
    "peerTeams",
    "events",
    "work",
  ])
    assert.deepEqual(empty.runtime[key], [], `${key} starts as an empty list`);

  const state = emptyEntityProjection();
  const first = applyEntityRows(
    state,
    [
      entityRow("agent", "a", { id: "a" }, 1),
      entityRow("edge", "e", { id: "e" }, 2),
    ],
    true,
  );
  const edgeChange = applyEntityRows(
    state,
    [
      entityRow("agent", "a", { id: "a" }, 1),
      entityRow("edge", "e", { id: "new-edge" }, 3),
    ],
    true,
  );
  assert.equal(edgeChange.threads, first.threads);
  assert.equal(edgeChange.runtime.agents, first.runtime.agents);
  assert.deepEqual(edgeChange.runtime.agents, [{ id: "a" }]);
});

it("keeps an empty chat list empty while unrelated entities update", () => {
  const state = emptyEntityProjection();
  const first = applyEntityRows(state, [], true);
  const updated = applyEntityRows(
    state,
    [entityRow("agent", "a", { id: "a" }, 1)],
    true,
  );
  assert.deepEqual(first.chats, []);
  assert.deepEqual(updated.chats, []);
});

it("reuses unchanged work, request, and peer team arrays when agents change", () => {
  const state = emptyEntityProjection();
  const initial = applyEntityRows(
    state,
    [
      entityRow("work", "w1", { id: "w1" }, 1),
      entityRow("request", "r1", { id: "r1" }, 2),
      entityRow("peerTeam", "pt1", { id: "pt1" }, 3),
      entityRow("agent", "a1", { id: "a1", name: "A" }, 4),
    ],
    true,
  );
  const updated = applyEntityRows(
    state,
    [
      entityRow("work", "w1", { id: "w1" }, 1),
      entityRow("request", "r1", { id: "r1" }, 2),
      entityRow("peerTeam", "pt1", { id: "pt1" }, 3),
      entityRow("agent", "a1", { id: "a1", name: "A2" }, 5),
    ],
    true,
  );
  assert.deepEqual(updated.runtime.work, [{ id: "w1" }]);
  assert.equal(updated.runtime.work, initial.runtime.work);
  assert.equal(updated.runtime.requests, initial.runtime.requests);
  assert.equal(updated.runtime.peerTeams, initial.runtime.peerTeams);
});

it("does not reapply unchanged workspace fields over a changed collection", () => {
  const state = emptyEntityProjection();
  const initial = applyEntityRows(
    state,
    [
      entityRow("agent", "a1", { id: "a1", name: "A" }, 1),
      entityRow("workspace", "current", { stateDir: "/workspace" }, 2),
    ],
    true,
  );
  assert.deepEqual(initial.runtime.agents, [{ id: "a1", name: "A" }]);
  const updated = applyEntityRows(
    state,
    [
      entityRow("agent", "a1", { id: "a1", name: "A2" }, 3),
      entityRow("workspace", "current", { stateDir: "/workspace" }, 2),
    ],
    true,
  );

  assert.deepEqual(updated.runtime.agents, [{ id: "a1", name: "A2" }]);
  assert.equal(updated.runtime.stateDir, "/workspace");
});

it("maps all runtime collections and carries workspace and edge data into snapshots", () => {
  const state = emptyEntityProjection();
  const entries = [
    ["agent", "a", { id: "a" }],
    ["chat", "c", { id: "c" }],
    ["room", "r", { id: "r" }],
    ["task", "t", { id: "t" }],
    ["monitor", "m", { id: "m" }],
    ["complaint", "q", { id: "q" }],
    ["request", "u", { id: "u" }],
    ["rule", "l", { id: "l" }],
    ["project", "p", { id: "p" }],
    ["peerTeam", "pt", { id: "pt" }],
    ["event", "e", { id: "e" }],
    ["work", "w", { id: "w" }],
    ["edge", "e1", { id: "e1" }],
    ["workspace", "current", { stateDir: "/workspace", custom: "value" }],
  ];
  const snapshot = applyEntityRows(
    state,
    entries.map(([collection, id, value], index) =>
      entityRow(collection, id, value, index + 1),
    ),
    true,
  );
  assert.deepEqual(snapshot.threads, [{ id: "a" }]);
  assert.deepEqual(snapshot.chats, [{ id: "c" }]);
  assert.deepEqual(snapshot.nodes, [{ id: "a" }, { id: "c" }]);
  assert.deepEqual(snapshot.edges, [{ id: "e1" }]);
  assert.equal(snapshot.stateDir, "/workspace");
  assert.equal(snapshot.runtime.custom, "value");
  for (const [collection, key] of [
    ["room", "rooms"],
    ["task", "tasks"],
    ["monitor", "monitors"],
    ["complaint", "complaints"],
    ["request", "requests"],
    ["rule", "rules"],
    ["project", "projects"],
    ["peerTeam", "peerTeams"],
    ["event", "events"],
    ["work", "work"],
  ])
    assert.deepEqual(snapshot.runtime[key], [
      { id: entries.find(([name]) => name === collection)[2].id },
    ]);
});

it("reuses unchanged runtime lists and edges, but replaces changed collections", () => {
  const state = emptyEntityProjection();
  const first = applyEntityRows(
    state,
    [
      entityRow("agent", "a", { id: "a", name: "A" }, 1),
      entityRow("project", "p", { id: "p" }, 2),
      entityRow("edge", "e", { id: "e" }, 3),
      entityRow("workspace", "current", { stateDir: "/one" }, 4),
    ],
    true,
  );
  const projects = first.runtime.projects;
  const edges = first.edges;
  const workspace = first.runtime;

  const changedAgent = applyEntityRows(
    state,
    [
      entityRow("agent", "a", { id: "a", name: "A2" }, 5),
      entityRow("project", "p", { id: "p" }, 2),
      entityRow("edge", "e", { id: "e" }, 3),
      entityRow("workspace", "current", { stateDir: "/one" }, 4),
    ],
    true,
  );
  assert.notEqual(changedAgent, first);
  assert.notEqual(changedAgent.threads, first.threads);
  assert.equal(changedAgent.runtime.projects, projects);
  assert.equal(changedAgent.edges, edges);
  assert.notEqual(changedAgent.runtime, workspace);

  const unchanged = applyEntityRows(
    state,
    [
      entityRow("agent", "a", { id: "a", name: "A2" }, 5),
      entityRow("project", "p", { id: "p" }, 2),
      entityRow("edge", "e", { id: "e" }, 3),
      entityRow("workspace", "current", { stateDir: "/one" }, 4),
    ],
    true,
  );
  assert.equal(unchanged, changedAgent);

  const changedEdge = applyEntityRows(
    state,
    [
      entityRow("agent", "a", { id: "a", name: "A2" }, 5),
      entityRow("project", "p", { id: "p" }, 2),
      entityRow("edge", "e", { id: "e2" }, 6),
      entityRow("workspace", "current", { stateDir: "/one" }, 4),
    ],
    true,
  );
  assert.deepEqual(changedEdge.edges, [{ id: "e2" }]);
  assert.equal(changedEdge.runtime.projects, projects);

  const changedWorkspace = applyEntityRows(
    state,
    [
      entityRow("agent", "a", { id: "a", name: "A2" }, 5),
      entityRow("project", "p", { id: "p" }, 2),
      entityRow("edge", "e", { id: "e2" }, 6),
      entityRow("workspace", "current", { stateDir: "/two", custom: "new" }, 7),
    ],
    true,
  );
  assert.equal(changedWorkspace.stateDir, "/two");
  assert.equal(changedWorkspace.runtime.custom, "new");
});

it("retains graph nodes while another collection changes", () => {
  const state = emptyEntityProjection();
  const rows = [
    entityRow("agent", "a", { id: "a", name: "A" }, 1),
    entityRow("chat", "c", { id: "c", name: "C" }, 2),
    entityRow("rule", "r", { id: "r", checks: 0 }, 3),
  ];
  const initial = applyEntityRows(state, rows, true);
  const changed = applyEntityRows(
    state,
    [rows[0], rows[1], entityRow("rule", "r", { id: "r", checks: 1 }, 4)],
    true,
  );
  assert.equal(changed.nodes, initial.nodes);
  assert.equal(changed.threads, initial.threads);
  assert.equal(changed.chats, initial.chats);
  assert.equal(changed.runtime.rules[0].checks, 1);
  const renamed = applyEntityRows(
    state,
    [
      rows[0],
      entityRow("chat", "c", { id: "c", name: "Renamed" }, 5),
      entityRow("rule", "r", { id: "r", checks: 1 }, 4),
    ],
    true,
  );
  assert.notEqual(renamed.nodes, changed.nodes);
  assert.equal(renamed.nodes[1].name, "Renamed");
});

it("bounds retained tombstones after deletes and missing rows", () => {
  const explicit = emptyEntityProjection();
  const rows = Array.from({ length: 4097 }, (_, index) =>
    entityRow("agent", `a${index}`, { id: `a${index}` }, index + 1),
  );
  applyEntityRows(explicit, rows, false);
  applyEntityRows(
    explicit,
    [entityRow("agent", "a0", { id: "a0", name: "kept" }, 5000)],
    false,
  );
  applyEntityRows(
    explicit,
    [entityRow("agent", "a0", {}, 5001, { _deleted: true })],
    false,
  );
  assert.equal(explicit.rows.size, 4096);
  assert.equal(explicit.rows.has("entity:agent:a0"), false);
  assert.equal(explicit.rows.has("entity:agent:a1"), true);
  assert.equal(explicit.rows.has("entity:agent:a4096"), true);

  const omitted = emptyEntityProjection();
  applyEntityRows(omitted, rows, false);
  applyEntityRows(
    omitted,
    [entityRow("agent", "a4096", { id: "a4096" }, 5000)],
    false,
  );
  applyEntityRows(omitted, [], false);
  assert.equal(omitted.rows.size, 4096);
  assert.equal(omitted.rows.has("entity:agent:a0"), true);
  assert.equal(omitted.rows.has("entity:agent:a4096"), false);
});

it("keeps extra and wrong-typed known DTO fields after envelope validation", () => {
  const state = emptyEntityProjection();
  const snapshot = applyEntityRows(
    state,
    [
      entityRow(
        "agent",
        "extra",
        {
          id: "extra",
          kind: "agent",
          name: "Worker",
          unknownPrivateLookingField: "retained",
        },
        1,
      ),
      entityRow(
        "agent",
        "wrong-type",
        {
          id: "wrong-type",
          kind: "agent",
          name: 42,
        },
        2,
      ),
    ],
    true,
  );
  assert.equal(snapshot.runtime.agents.length, 2);
  assert.equal(
    snapshot.runtime.agents[0].unknownPrivateLookingField,
    "retained",
  );
  assert.equal(snapshot.runtime.agents[1].name, 42);
  assert.equal(state.rows.get("entity:agent:extra").seq, 1);
  assert.equal(state.rows.get("entity:agent:wrong-type").seq, 2);
});

it("applies v4 structured entity rows without parsing JSON strings", () => {
  const state = emptyEntityProjection();
  const upsert = {
    id: "entity:agent:a",
    payload: "",
    entity: { collection: "agent", id: "a", value: { id: "a", name: "A" } },
    seq: 1,
  };
  const first = applyEntityRows(state, [upsert], true);
  assert.equal(first.threads[0]?.name, "A");
  const deleted = applyEntityRows(
    state,
    [
      {
        id: upsert.id,
        collection: "agent",
        entityId: "a",
        seq: 2,
        _deleted: true,
      },
    ],
    true,
  );
  assert.deepEqual(deleted.threads, []);
  const stale = applyEntityRows(state, [upsert], true);
  assert.deepEqual(stale.threads, []);
});

it("projects independent account limits and preserves them across workspace updates", () => {
  const row = (collection, id, value, seq) => ({
    id: `entity:${collection}:${id}`,
    payload: "",
    entity: { collection, id, value },
    seq,
  });
  const state = emptyEntityProjection();
  const alpha = { accountKey: "alpha", at: 100, data: { accountId: "A" } };
  const initial = applyEntityRows(
    state,
    [
      row("workspace", "current", { stateDir: "fixture" }, 1),
      row("rateLimit", "alpha", alpha, 2),
      row("rateLimit", "default", { accountKey: "default", at: 101 }, 3),
    ],
    true,
  );
  assert.deepEqual(initial.runtime.rateLimitsByAccount.alpha, alpha);
  assert.equal(
    initial.runtime.rateLimits,
    initial.runtime.rateLimitsByAccount.default,
  );
  const next = applyEntityRows(
    state,
    [
      row("workspace", "current", { stateDir: "updated" }, 4),
      row("rateLimit", "alpha", alpha, 2),
      row("rateLimit", "default", { accountKey: "default", at: 101 }, 3),
    ],
    true,
  );
  assert.equal(
    next.runtime.rateLimitsByAccount,
    initial.runtime.rateLimitsByAccount,
  );
  const withoutAlpha = applyEntityRows(
    state,
    [
      row("workspace", "current", { stateDir: "updated" }, 4),
      row("rateLimit", "default", { accountKey: "default", at: 101 }, 3),
      { id: "entity:rateLimit:alpha", seq: 5, payload: "{}", _deleted: true },
    ],
    true,
  );
  assert.equal(withoutAlpha.runtime.rateLimitsByAccount.alpha, undefined);
  assert.equal(withoutAlpha.runtime.rateLimits.accountKey, "default");
});

it("projects Studio error entities and removes dismissed rows by tombstone", () => {
  const row = (id, value, seq) => ({
    id: `entity:studioError:${id}`,
    payload: "",
    entity: { collection: "studioError", id, value },
    seq,
  });
  const state = emptyEntityProjection();
  const item = {
    id: "agent:worker:turn-1",
    kind: "turn_disconnected",
    reason: "The provider connection ended during the turn.",
    action: "resume",
    agentId: "worker",
    agentName: "Worker",
    detail: "disconnected",
    subject: "",
    at: 100,
  };
  const initial = applyEntityRows(
    state,
    [
      {
        id: "entity:workspace:current",
        payload: "",
        entity: {
          collection: "workspace",
          id: "current",
          value: { stateDir: "fixture" },
        },
        seq: 1,
      },
      row(item.id, item, 2),
    ],
    true,
  );
  assert.deepEqual(initial.runtime.studioErrors, [item]);
  const removed = applyEntityRows(
    state,
    [
      {
        id: "entity:workspace:current",
        payload: "",
        entity: {
          collection: "workspace",
          id: "current",
          value: { stateDir: "fixture" },
        },
        seq: 1,
      },
      { id: `entity:studioError:${item.id}`, seq: 3, payload: "{}", _deleted: true },
    ],
    true,
  );
  assert.deepEqual(removed.runtime.studioErrors, []);
});
it("projects queue stream upserts and deletion without endpoint reads", () => {
  const state = emptyEntityProjection();
  const value = {
    items: [{ id: "q", text: "Queued" }],
    revision: "one",
    capabilities: { reorder: true, receipts: true },
  };
  const row = {
    id: "entity:queue:chat",
    seq: 1,
    payload: "",
    entity: { collection: "queue", id: "chat", value },
  };
  const initial = applyEntityRows(state, [row], true);
  assert.deepEqual(initial.runtime.queuesByAgent.chat, value);
  const next = applyEntityRows(
    state,
    [row, { id: "entity:queue:chat", seq: 2, payload: "{}", _deleted: true }],
    true,
  );
  assert.equal(next.runtime.queuesByAgent.chat, undefined);
});

for (const [collection, field, singleton] of [
  ["accounts", "accountsSummary", true],
  ["accountCost", "accountCostsByAccount", false],
  ["sessionCost", "sessionCostsByAgent", false],
  ["desktop", "desktop", true],
  ["terminal", "terminals", false],
])
  it(`projects ${collection} source entities and deletion`, () => {
    const state = emptyEntityProjection();
    const value = { id: "record", marker: collection };
    const id = singleton ? "current" : "record";
    const row = {
      id: `entity:${collection}:${id}`,
      seq: 1,
      payload: "",
      entity: { collection, id, value },
    };
    const initial = applyEntityRows(state, [row], true);
    assert.deepEqual(
      singleton
        ? initial.runtime[field]
        : collection === "terminal"
          ? initial.runtime[field][0]
          : initial.runtime[field][id],
      value,
    );
    const next = applyEntityRows(
      state,
      [row, { id: row.id, seq: 2, payload: "{}", _deleted: true }],
      true,
    );
    assert.equal(
      singleton
        ? next.runtime[field]
        : collection === "terminal"
          ? next.runtime[field][0]
          : next.runtime[field][id],
      undefined,
    );
  });
