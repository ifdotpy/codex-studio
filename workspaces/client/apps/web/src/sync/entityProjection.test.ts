import type { EntityRow } from "./entityProjection.ts";
import type { Agent, Message, Snapshot } from "../types";
import type { SyncEntityPayload } from "../generated/api";
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

it("revives a locally evicted source version without reviving authoritative tombstones", () => {
  const state = emptyEntityProjection();
  const row = {
    id: "entity:agentSummary:moved",
    seq: 3,
    payload: "",
    entity: {
      collection: "agentSummary",
      id: "moved",
      value: { id: "moved", status: "idle" },
    },
  };
  applyEntityChanges(state, [row], true);
  const removed = applyEntityChanges(
    state,
    [{ id: row.id, seq: 3, _deleted: true, evicted: true }],
    true,
  );

  assert.equal(removed.threads.length, 0);
  const revived = applyEntityChanges(state, [row], true);

  assert.ok(revived.threads[0] !== null && revived.threads[0] !== undefined);
  assert.equal(revived.threads[0].id, "moved");
  applyEntityChanges(state, [{ id: row.id, seq: 4, _deleted: true }], true);
  const stale = applyEntityChanges(state, [row], true);

  assert.equal(stale.threads.length, 0);
});

it("retains other agents when one streamed summary or detail changes", () => {
  const state = emptyEntityProjection();

  const first = applyEntityChanges(
    state,
    [
      entityRow(
        {
          collection: "agentSummary",
          id: "a",
          value: { id: "a", status: "idle" },
        },
        1,
      ),
      entityRow(
        {
          collection: "agentSummary",
          id: "b",
          value: { id: "b", status: "running" },
        },
        2,
      ),
    ],
    true,
  );

  const untouched = first.runtime.agents[1];
  const updated = applyEntityChanges(
    state,
    [
      entityRow(
        {
          collection: "agentSummary",
          id: "a",
          value: { id: "a", status: "running" },
        },
        3,
      ),
    ],
    true,
  );

  assert.notEqual(updated.runtime.agents[0], first.runtime.agents[0]);

  assert.ok(
    updated.runtime.agents[0] !== null &&
      updated.runtime.agents[0] !== undefined,
  );
  assert.equal(updated.runtime.agents[0].status, "running");

  assert.equal(updated.runtime.agents[1], untouched);
  const detailed = applyEntityChanges(
    state,
    [
      entityRow(
        {
          collection: "agentDetail",
          id: "a",
          value: { id: "a", lastAnswer: "Progress" },
        },
        4,
      ),
    ],
    true,
  );

  assert.ok(
    detailed.runtime.agents[0] !== null &&
      detailed.runtime.agents[0] !== undefined,
  );
  assert.equal(detailed.runtime.agents[0].lastAnswer, "Progress");

  assert.equal(detailed.runtime.agents[1], untouched);
  const closed = applyEntityChanges(
    state,
    [{ id: "entity:agentDetail:a", seq: 5, _deleted: true }],
    true,
  );

  assert.ok(
    closed.runtime.agents[0] !== null && closed.runtime.agents[0] !== undefined,
  );
  assert.equal(closed.runtime.agents[0].lastAnswer, undefined);

  assert.equal(closed.runtime.agents[1], untouched);
  const removed = applyEntityChanges(
    state,
    [{ id: "entity:agentSummary:a", seq: 6, _deleted: true }],
    true,
  );

  assert.deepEqual(removed.runtime.agents, [untouched]);

  assert.equal(removed.runtime.agents[0], untouched);
});

it("renders the list from summaries and adds detail without losing unread state", () => {
  const state = emptyEntityProjection();
  const summary = {
    id: "a",
    name: "Current",
    kind: "agent",
    source: "managed",
    status: "completed",
    threadId: "thread",
    lastCompletedTurn: "turn",
    lastCompletedTurnStatus: "completed",
    readState: { threadId: "thread", turnId: "turn", read: false, revision: 1 },
  } satisfies Agent;

  const first = applyEntityRows(
    state,
    [entityRow({ collection: "agentSummary", id: "a", value: summary }, 1)],
    true,
  );

  assert.ok(first.threads[0] !== null && first.threads[0] !== undefined);
  assert.equal(first.threads[0].name, "Current");

  assert.ok(first.threads[0] !== null && first.threads[0] !== undefined);
  assert.equal(first.threads[0].lastAnswer, undefined);

  assert.ok(
    first.threads[0].readState !== null &&
      first.threads[0].readState !== undefined,
  );
  assert.equal(first.threads[0].readState.read, false);
  const open = applyEntityChanges(
    state,
    [
      entityRow(
        {
          collection: "agentDetail",
          id: "a",
          value: { id: "a", lastAnswer: "Answer" },
        },
        2,
      ),
    ],
    true,
  );

  assert.ok(open.threads[0] !== null && open.threads[0] !== undefined);
  assert.equal(open.threads[0].lastAnswer, "Answer");

  assert.ok(
    open.threads[0].readState !== null &&
      open.threads[0].readState !== undefined,
  );
  assert.equal(open.threads[0].readState.read, false);
  const updated = applyEntityChanges(
    state,
    [
      entityRow(
        {
          collection: "agentSummary",
          id: "a",
          value: { ...summary, name: "Renamed" },
        },
        3,
      ),
    ],
    true,
  );

  assert.ok(updated.threads[0] !== null && updated.threads[0] !== undefined);
  assert.equal(updated.threads[0].name, "Renamed");

  assert.ok(updated.threads[0] !== null && updated.threads[0] !== undefined);
  assert.equal(updated.threads[0].lastAnswer, "Answer");
  const closed = applyEntityChanges(
    state,
    [{ id: "entity:agentDetail:a", seq: 4, _deleted: true }],
    true,
  );

  assert.ok(closed.threads[0] !== null && closed.threads[0] !== undefined);
  assert.equal(closed.threads[0].lastAnswer, undefined);

  assert.ok(closed.threads[0] !== null && closed.threads[0] !== undefined);
  assert.equal(closed.threads[0].name, "Renamed");
});

it("projects bounded workspace summaries and receipts with deletes", () => {
  const state = emptyEntityProjection();

  const first = applyEntityRows(
    state,
    [
      entityRow(
        {
          collection: "messageReceipt",
          id: "m",
          value: {
            id: "m",
            agent: "a",
            status: "pending",
            materialized: false,
          },
        },
        1,
      ),
      entityRow(
        {
          collection: "plan",
          id: "a",
          value: { id: "a", rootId: "a", text: "Plan", version: 1, updated: 1 },
        },
        2,
      ),
      entityRow(
        {
          collection: "annotation",
          id: "n",
          value: {
            id: "n",
            agent: "a",
            rootId: "a",
            path: "a.rs",
            line: 1,
            text: "Comment",
            created: 1,
          },
        },
        3,
      ),
    ],
    true,
  );

  assert.ok(
    first.runtime.messageReceipts !== null &&
      first.runtime.messageReceipts !== undefined,
  );
  assert.ok(
    first.runtime.messageReceipts[0] !== null &&
      first.runtime.messageReceipts[0] !== undefined,
  );
  assert.equal(first.runtime.messageReceipts[0].status, "pending");

  assert.ok(first.runtime.plans !== null && first.runtime.plans !== undefined);
  assert.ok(
    first.runtime.plans[0] !== null && first.runtime.plans[0] !== undefined,
  );
  assert.equal(first.runtime.plans[0].text, "Plan");

  assert.ok(
    first.runtime.annotations !== null &&
      first.runtime.annotations !== undefined,
  );
  assert.ok(
    first.runtime.annotations[0] !== null &&
      first.runtime.annotations[0] !== undefined,
  );
  assert.equal(first.runtime.annotations[0].text, "Comment");
  const next = applyEntityChanges(
    state,
    [{ id: "entity:annotation:n", seq: 4, _deleted: true }],
    true,
  );

  assert.ok(
    next.runtime.annotations !== null && next.runtime.annotations !== undefined,
  );
  assert.equal(next.runtime.annotations.length, 0);

  assert.equal(next.runtime.messageReceipts, first.runtime.messageReceipts);
});

it("reuses unchanged entity data and bounds transcript and history caches", () => {
  const state = emptyEntityProjection();
  let parsed = 0;
  const parse = JSON.parse;
  JSON.parse = (...args) => {
    parsed++;
    return parse(...args);
  };
  try {
    const initial: [EntityRow, EntityRow, EntityRow] = [
      entityRow(
        { collection: "agent", id: "a", value: { id: "a", name: "A" } },
        1,
      ),
      entityRow(
        { collection: "agent", id: "b", value: { id: "b", name: "B" } },
        2,
      ),
      entityRow(
        { collection: "project", id: "p", value: { id: "p", name: "P" } },
        3,
      ),
    ];
    const first = applyEntityRows(state, initial, true);
    assert.equal(parsed, 0, "structured entity rows need no JSON parsing");

    const agentA = first.threads[0];

    const projectList = first.runtime.projects;

    const changed = applyEntityRows(
      state,
      [
        initial[0],
        entityRow(
          { collection: "agent", id: "b", value: { id: "b", name: "B2" } },
          4,
        ),
        initial[2],
      ],
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

    assert.ok(changed.threads[1] !== null && changed.threads[1] !== undefined);
    assert.equal(changed.threads[1].name, "B2");

    const stale = applyEntityRows(
      state,
      [
        initial[0],
        entityRow(
          { collection: "agent", id: "b", value: { id: "b", name: "stale" } },
          3,
        ),
        initial[2],
      ],
      true,
    );

    assert.ok(stale.threads[1] !== null && stale.threads[1] !== undefined);
    assert.equal(
      stale.threads[1].name,
      "B2",
      "older sequences cannot replace newer values",
    );
    assert.equal(parsed, 0, "stale sequences are not parsed");

    const deleted = applyEntityRows(
      state,
      [
        initial[0],
        entityRow({ collection: "agent", id: "b", value: { id: "b" } }, 5, {
          _deleted: true,
        }),
        initial[2],
      ],
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
        entityRow(
          { collection: "agent", id: "b", value: { id: "b", name: "stale" } },
          4,
        ),
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
        entityRow(
          {
            collection: "agent",
            id: "b",
            value: { id: "b", name: "restored" },
          },
          6,
        ),
        initial[2],
      ],
      true,
    );

    assert.deepEqual(
      restored.threads.map((agent) => agent.id),
      ["a", "b"],
    );

    assert.ok(
      restored.threads[1] !== null && restored.threads[1] !== undefined,
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

  assert.ok(bounded.items[0] !== null && bounded.items[0] !== undefined);
  assert.equal(bounded.items[0].id, "m0");

  assert.equal(bounded.items.at(-1)?.id, "m239");
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

  const message = (
    id: string,
    role: Message["role"],
    turnId: string,
    extra: Partial<Message> = {},
  ): Message => ({
    id,
    role,
    turnId,
    text: id,
    ...extra,
  });
  const transcript: [Message, Message, Message, Message] = [
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
    appendGroups.at(-1)?.items.map((item) => item.id),
    ["a2", "tool2"],
  );
  const runGroups: [
    Parameters<typeof historyPresentationGroups>[0][number],
    Parameters<typeof historyPresentationGroups>[0][number],
  ] = [
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
    presentation[0]?.items.map((item) => item.id),
    ["tool-a", "reasoning-a", "tool-b"],
    "reasoning remains chronological between tools",
  );

  assert.ok(presentation[0]);
  assert.ok(presentation[0].turns);
  assert.ok(
    presentation[0]?.turns !== null && presentation[0]?.turns !== undefined,
  );
  assert.equal(
    presentation[0]?.turns.length,
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

  assert.ok(
    activePresentation[0] !== null && activePresentation[0] !== undefined,
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

  assert.ok(
    activePresentation[0] !== null && activePresentation[0] !== undefined,
  );
  assert.ok(
    completedPresentation[0] !== null && completedPresentation[0] !== undefined,
  );
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
    ])[0]?.outcome,
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

const entityRow = <T extends SyncEntityPayload>(
  entity: T,
  seq: number,
  extra: Partial<EntityRow> = {},
): EntityRow => ({
  id: `entity:${entity.collection}:${entity.id}`,
  payload: "",
  entity,
  seq,
  ...extra,
});

it("projects entity rows and preserves server sequence ordering", () => {
  const state = emptyEntityProjection();
  const initial = applyEntityRows(
    state,
    [
      { id: "state:ready", seq: 1 } satisfies Omit<EntityRow, "payload">,
      { id: "other:agent", seq: 1 } satisfies Omit<EntityRow, "payload">,
      entityRow(
        { collection: "agent", id: "a", value: { id: "a", name: "A" } },
        2,
      ),
    ],
    true,
  );

  assert.deepEqual(initial.threads, [{ id: "a", name: "A" }]);

  const unchanged = applyEntityRows(
    state,
    [
      entityRow(
        { collection: "agent", id: "a", value: { id: "a", name: "equal" } },
        2,
      ),
      entityRow(
        { collection: "agent", id: "a", value: { id: "a", name: "older" } },
        1,
      ),
    ],
    true,
  );
  assert.equal(unchanged, initial, "equal and older sequences are ignored");

  assert.deepEqual(unchanged.threads, [{ id: "a", name: "A" }]);
  const unchangedForIrrelevantRows = applyEntityRows(
    state,
    [
      entityRow(
        { collection: "agent", id: "a", value: { id: "a", name: "A" } },
        2,
      ),
      { id: "other:agent", seq: 3 } satisfies Omit<EntityRow, "payload">,
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
  const fixtureEntities = [
    {
      collection: "agent",
      id: "agent",
      value: { id: "agent", name: "agent" },
    },
    {
      collection: "room",
      id: "room",
      value: { id: "room", name: "room" },
    },
    {
      collection: "task",
      id: "task",
      value: { id: "task", name: "task" },
    },
    {
      collection: "monitor",
      id: "monitor",
      value: { id: "monitor", name: "monitor" },
    },
    {
      collection: "complaint",
      id: "complaint",
      value: { id: "complaint" },
    },
    {
      collection: "request",
      id: "request",
      value: { id: "request" },
    },
    {
      collection: "rule",
      id: "rule",
      value: { id: "rule", name: "rule" },
    },
    {
      collection: "project",
      id: "project",
      value: { id: "project", name: "project" },
    },
    {
      collection: "peerTeam",
      id: "peerTeam",
      value: { id: "peerTeam", name: "peerTeam" },
    },
    {
      collection: "chat",
      id: "chat",
      value: { id: "chat", name: "chat" },
    },
    {
      collection: "edge",
      id: "edge",
      value: { id: "edge" },
    },
    {
      collection: "event",
      id: "event",
      value: { id: "event" },
    },
    {
      collection: "work",
      id: "work",
      value: { id: "work" },
    },

    {
      collection: "workspace",
      id: "current",
      value: { stateDir: "" },
    },
  ] satisfies SyncEntityPayload[];
  const collections = fixtureEntities.map((entity) => entity.collection);
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
    chat: null,
    edge: null,
    event: "events",
    work: "work",
    workspace: null,
  } as const;
  const state = emptyEntityProjection();
  const first = applyEntityRows(
    state,
    fixtureEntities.map((entity, index) => entityRow(entity, index + 1)),
    true,
  );
  for (const [collection, runtimeName] of collections.map(
    (collection) => [collection, runtimeNames[collection]] as const,
  )) {
    if (collection === "chat") assert.equal(first.chats.length, 1);
    else if (collection === "edge") assert.equal(first.edges.length, 1);
    else if (collection === "agent") assert.equal(first.threads.length, 1);
    else if (runtimeName) assert.equal(first.runtime[runtimeName].length, 1);
  }

  const priorArrays = new Map(
    collections.map((collection): [(typeof collections)[number], unknown] => {
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
      ...fixtureEntities
        .slice(0, -1)
        .map((entity, index) => entityRow(entity, index + 1)),
      entityRow(
        {
          collection: "project",
          id: "project",
          value: { id: "project", name: "changed" },
        },
        20,
      ),
      entityRow(
        {
          collection: "workspace",
          id: "current",
          value: { stateDir: "/tmp/state" },
        },
        14,
      ),
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
      entityRow(
        {
          collection: "chat",
          id: "c",
          value: { id: "c", title: "Shared chat" },
        },
        1,
      ),
      entityRow(
        { collection: "agent", id: "a", value: { id: "a", name: "A" } },
        2,
      ),
    ],
    true,
  );

  const chats = initial.chats;
  const updated = applyEntityRows(
    state,
    [
      entityRow(
        {
          collection: "chat",
          id: "c",
          value: { id: "c", title: "Shared chat" },
        },
        1,
      ),
      entityRow(
        { collection: "agent", id: "a", value: { id: "a", name: "Updated A" } },
        3,
      ),
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
      entityRow(
        { collection: "agent", id: "a", value: { id: "a", name: "A1" } },
        1,
      ),
      entityRow(
        { collection: "project", id: "p", value: { id: "p", name: "P1" } },
        2,
      ),
    ],
    true,
  );
  const malformedRows: EntityRow[] = [
    { id: "entity:broken:json", payload: "{", seq: 4 },
    { id: "entity:broken:empty", payload: "", seq: 5 },
    { id: "entity:broken:null", payload: "null", seq: 6 },
    { id: "entity:broken:number", payload: "42", seq: 7 },
    { id: "entity:broken:array", payload: "[]", seq: 8 },
    {
      payload: "",
      id: "entity:agent:missing-id",
      // @ts-expect-error the malformed envelope deliberately has no entity id or value id.
      entity: { collection: "agent", value: { name: "A" } },
      seq: 9,
    },
    {
      payload: "",
      id: "entity:agent:null-value",
      // @ts-expect-error the malformed entity deliberately has a null value.
      entity: { collection: "agent", id: "a", value: null },
      seq: 10,
    },
    {
      payload: "",
      id: "entity:agent:missing-value",
      // @ts-expect-error the malformed entity deliberately has no value.
      entity: { collection: "agent", id: "a" },
      seq: 11,
    },
    {
      payload: "",
      id: "entity:future:x",
      // @ts-expect-error the malformed entity deliberately uses an unknown collection.
      entity: { collection: "future", id: "x", value: {} },
      seq: 12,
    },
  ];
  const reports: unknown[][] = [];
  const report = console.error;
  console.error = (...args) => reports.push(args);
  try {
    const changed = applyEntityRows(
      state,
      [
        entityRow(
          { collection: "agent", id: "a", value: { id: "a", name: "A2" } },
          3,
        ),
        ...malformedRows,
        entityRow(
          { collection: "project", id: "p", value: { id: "p", name: "P2" } },
          6,
        ),
      ],
      true,
    );

    assert.ok(changed.threads[0] !== null && changed.threads[0] !== undefined);
    assert.equal(changed.threads[0].name, "A2");

    assert.ok(
      changed.runtime.projects[0] !== null &&
        changed.runtime.projects[0] !== undefined,
    );
    assert.equal(changed.runtime.projects[0].name, "P2");

    assert.equal(changed.chats, initial.chats);
    assert.equal(reports.length, malformedRows.length);
    assert.ok(
      reports.every(
        ([message, fields]) =>
          typeof message === "string" &&
          typeof fields === "object" &&
          fields !== null &&
          "id" in fields &&
          typeof fields.id === "string" &&
          message.includes("invalid payload") &&
          message.includes(fields.id),
      ),
    );
    assert.ok(
      reports.every(
        ([, fields]) =>
          typeof fields === "object" &&
          fields !== null &&
          !Object.hasOwn(fields, "payload"),
      ),
    );

    const repeated = applyEntityRows(
      state,
      [
        entityRow(
          { collection: "agent", id: "a", value: { id: "a", name: "A2" } },
          3,
        ),
        ...malformedRows,
        entityRow(
          { collection: "project", id: "p", value: { id: "p", name: "P2" } },
          6,
        ),
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
    [
      entityRow(
        { collection: "agent", id: "a", value: { id: "a", name: "A" } },
        1,
      ),
    ],
    true,
  );
  const moved = applyEntityRows(
    state,
    [
      entityRow(
        { collection: "project", id: "p", value: { id: "p", name: "P" } },
        2,
      ),
    ],
    true,
  );

  assert.deepEqual(first.threads, [{ id: "a", name: "A" }]);

  assert.deepEqual(moved.threads, []);

  assert.deepEqual(moved.runtime.projects, [{ id: "p", name: "P" }]);

  assert.equal(state.rows.get("entity:agent:a")?.deleted, true);
  assert.deepEqual(state.values.agent.size, 0);

  const removed = applyEntityRows(state, [], true);

  assert.deepEqual(removed.runtime.projects, []);

  assert.equal(state.rows.get("entity:project:p")?.deleted, true);
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
      entityRow({ collection: "agent", id: "a", value: { id: "a" } }, 1),
      entityRow({ collection: "chat", id: "c", value: { id: "c" } }, 2, {
        _deleted: true,
      }),
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
      entityRow({ collection: "agent", id: "a", value: { id: "a" } }, 1),
      entityRow(
        { collection: "chat", id: "unknown", value: { id: "unknown" } },
        3,
        { _deleted: true },
      ),
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
    [
      entityRow({ collection: "agent", id: "a", value: { id: "a" } }, 3, {
        _deleted: true,
      }),
    ],
    true,
  );

  assert.deepEqual(snapshot.threads, []);

  assert.equal(state.rows.get("entity:agent:a")?.deleted, true);

  snapshot = applyEntityRows(
    state,
    [
      entityRow(
        { collection: "agent", id: "a", value: { id: "a", name: "back" } },
        4,
      ),
    ],
    true,
  );

  assert.deepEqual(snapshot.threads, [{ id: "a", name: "back" }]);

  assert.equal(state.rows.get("entity:agent:a")?.deleted, undefined);
});

it("waits for readiness while retaining the latest entity rows", () => {
  const state = emptyEntityProjection();
  assert.equal(
    applyEntityRows(
      state,
      [entityRow({ collection: "agent", id: "a", value: { id: "a" } }, 1)],
      false,
    ),
    null,
  );
  const ready = applyEntityRows(
    state,
    [entityRow({ collection: "agent", id: "a", value: { id: "a" } }, 1)],
    true,
  );

  assert.deepEqual(ready.threads, [{ id: "a" }]);
});

it("updates stable entities in place and relocates changed entity IDs", () => {
  const state = emptyEntityProjection();
  applyEntityRows(
    state,
    [
      entityRow(
        { collection: "agent", id: "a", value: { id: "a", name: "A" } },
        1,
        { id: `entity:agent:${"row-a"}` },
      ),
      entityRow(
        { collection: "agent", id: "b", value: { id: "b", name: "B" } },
        2,
        { id: `entity:agent:${"row-b"}` },
      ),
    ],
    true,
  );
  const stable = applyEntityRows(
    state,
    [
      entityRow(
        { collection: "agent", id: "a", value: { id: "a", name: "A2" } },
        3,
        { id: `entity:agent:${"row-a"}` },
      ),
      entityRow(
        { collection: "agent", id: "b", value: { id: "b", name: "B" } },
        2,
        { id: `entity:agent:${"row-b"}` },
      ),
    ],
    true,
  );

  assert.deepEqual(
    stable.threads.map(({ id }) => id),
    ["a", "b"],
  );

  assert.ok(stable.threads[0] !== null && stable.threads[0] !== undefined);
  assert.equal(stable.threads[0].name, "A2");

  const relocated = applyEntityRows(
    state,
    [
      entityRow(
        {
          collection: "agent",
          id: "renamed",
          value: { id: "renamed", name: "A3" },
        },
        4,
        { id: `entity:agent:${"row-a"}` },
      ),
      entityRow(
        { collection: "agent", id: "b", value: { id: "b", name: "B" } },
        2,
        { id: `entity:agent:${"row-b"}` },
      ),
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
    [
      entityRow({ collection: "agent", id: "a", value: { id: "a" } }, 1, {
        id: `entity:source:${"row"}`,
      }),
    ],
    true,
  );
  const moved = applyEntityRows(
    state,
    [
      entityRow({ collection: "project", id: "a", value: { id: "a" } }, 2, {
        id: `entity:source:${"row"}`,
      }),
    ],
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
      entityRow({ collection: "agent", id: "a", value: { id: "a" } }, 1),
      entityRow({ collection: "agent", id: "b", value: { id: "b" } }, 2),
    ],
    false,
  );
  applyEntityRows(
    state,
    [entityRow({ collection: "agent", id: "b", value: { id: "b" } }, 2)],
    false,
  );
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
  ] as const)
    assert.deepEqual(empty.runtime[key], [], `${key} starts as an empty list`);

  const state = emptyEntityProjection();
  const first = applyEntityRows(
    state,
    [
      entityRow({ collection: "agent", id: "a", value: { id: "a" } }, 1),
      entityRow({ collection: "edge", id: "e", value: { id: "e" } }, 2),
    ],
    true,
  );
  const edgeChange = applyEntityRows(
    state,
    [
      entityRow({ collection: "agent", id: "a", value: { id: "a" } }, 1),
      entityRow({ collection: "edge", id: "e", value: { id: "new-edge" } }, 3),
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
    [entityRow({ collection: "agent", id: "a", value: { id: "a" } }, 1)],
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
      entityRow({ collection: "work", id: "w1", value: { id: "w1" } }, 1),
      entityRow({ collection: "request", id: "r1", value: { id: "r1" } }, 2),
      entityRow({ collection: "peerTeam", id: "pt1", value: { id: "pt1" } }, 3),
      entityRow(
        { collection: "agent", id: "a1", value: { id: "a1", name: "A" } },
        4,
      ),
    ],
    true,
  );
  const updated = applyEntityRows(
    state,
    [
      entityRow({ collection: "work", id: "w1", value: { id: "w1" } }, 1),
      entityRow({ collection: "request", id: "r1", value: { id: "r1" } }, 2),
      entityRow({ collection: "peerTeam", id: "pt1", value: { id: "pt1" } }, 3),
      entityRow(
        { collection: "agent", id: "a1", value: { id: "a1", name: "A2" } },
        5,
      ),
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
      entityRow(
        { collection: "agent", id: "a1", value: { id: "a1", name: "A" } },
        1,
      ),
      entityRow(
        {
          collection: "workspace",
          id: "current",
          value: { stateDir: "/workspace" },
        },
        2,
      ),
    ],
    true,
  );

  assert.deepEqual(initial.runtime.agents, [{ id: "a1", name: "A" }]);
  const updated = applyEntityRows(
    state,
    [
      entityRow(
        { collection: "agent", id: "a1", value: { id: "a1", name: "A2" } },
        3,
      ),
      entityRow(
        {
          collection: "workspace",
          id: "current",
          value: { stateDir: "/workspace" },
        },
        2,
      ),
    ],
    true,
  );

  assert.deepEqual(updated.runtime.agents, [{ id: "a1", name: "A2" }]);

  assert.equal(updated.runtime.stateDir, "/workspace");
});

it("maps all runtime collections and carries workspace and edge data into snapshots", () => {
  const state = emptyEntityProjection();
  const workspace = { stateDir: "/workspace", custom: "value" };
  const entries = [
    { collection: "agent", id: "a", value: { id: "a" } },
    { collection: "chat", id: "c", value: { id: "c" } },
    { collection: "room", id: "r", value: { id: "r" } },
    { collection: "task", id: "t", value: { id: "t" } },
    { collection: "monitor", id: "m", value: { id: "m" } },
    { collection: "complaint", id: "q", value: { id: "q" } },
    { collection: "request", id: "u", value: { id: "u" } },
    { collection: "rule", id: "l", value: { id: "l" } },
    { collection: "project", id: "p", value: { id: "p" } },
    { collection: "peerTeam", id: "pt", value: { id: "pt" } },
    { collection: "event", id: "e", value: { id: "e" } },
    { collection: "work", id: "w", value: { id: "w" } },
    { collection: "edge", id: "e1", value: { id: "e1" } },
    {
      collection: "workspace",
      id: "current",
      value: workspace,
    },
  ] satisfies SyncEntityPayload[];
  const snapshot = applyEntityRows(
    state,
    entries.map((entity, index) => entityRow(entity, index + 1)),
    true,
  );

  assert.deepEqual(snapshot.threads, [{ id: "a" }]);

  assert.deepEqual(snapshot.chats, [{ id: "c" }]);

  assert.deepEqual(snapshot.nodes, [{ id: "a" }, { id: "c" }]);

  assert.deepEqual(snapshot.edges, [{ id: "e1" }]);

  assert.equal(snapshot.stateDir, "/workspace");

  assert.equal(
    "custom" in snapshot.runtime ? snapshot.runtime.custom : undefined,
    "value",
  );

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
  ] as const)
    assert.deepEqual(snapshot.runtime[key], [
      { id: entries.find((entity) => entity.collection === collection)?.id },
    ]);
});

it("reuses unchanged runtime lists and edges, but replaces changed collections", () => {
  const state = emptyEntityProjection();
  const first = applyEntityRows(
    state,
    [
      entityRow(
        { collection: "agent", id: "a", value: { id: "a", name: "A" } },
        1,
      ),
      entityRow({ collection: "project", id: "p", value: { id: "p" } }, 2),
      entityRow({ collection: "edge", id: "e", value: { id: "e" } }, 3),
      entityRow(
        { collection: "workspace", id: "current", value: { stateDir: "/one" } },
        4,
      ),
    ],
    true,
  );

  const projects = first.runtime.projects;

  const edges = first.edges;

  const workspace = first.runtime;

  const changedAgent = applyEntityRows(
    state,
    [
      entityRow(
        { collection: "agent", id: "a", value: { id: "a", name: "A2" } },
        5,
      ),
      entityRow({ collection: "project", id: "p", value: { id: "p" } }, 2),
      entityRow({ collection: "edge", id: "e", value: { id: "e" } }, 3),
      entityRow(
        { collection: "workspace", id: "current", value: { stateDir: "/one" } },
        4,
      ),
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
      entityRow(
        { collection: "agent", id: "a", value: { id: "a", name: "A2" } },
        5,
      ),
      entityRow({ collection: "project", id: "p", value: { id: "p" } }, 2),
      entityRow({ collection: "edge", id: "e", value: { id: "e" } }, 3),
      entityRow(
        { collection: "workspace", id: "current", value: { stateDir: "/one" } },
        4,
      ),
    ],
    true,
  );
  assert.equal(unchanged, changedAgent);

  const changedEdge = applyEntityRows(
    state,
    [
      entityRow(
        { collection: "agent", id: "a", value: { id: "a", name: "A2" } },
        5,
      ),
      entityRow({ collection: "project", id: "p", value: { id: "p" } }, 2),
      entityRow({ collection: "edge", id: "e", value: { id: "e2" } }, 6),
      entityRow(
        { collection: "workspace", id: "current", value: { stateDir: "/one" } },
        4,
      ),
    ],
    true,
  );

  assert.deepEqual(changedEdge.edges, [{ id: "e2" }]);

  assert.equal(changedEdge.runtime.projects, projects);

  const changedWorkspace = applyEntityRows(
    state,
    [
      entityRow(
        { collection: "agent", id: "a", value: { id: "a", name: "A2" } },
        5,
      ),
      entityRow({ collection: "project", id: "p", value: { id: "p" } }, 2),
      entityRow({ collection: "edge", id: "e", value: { id: "e2" } }, 6),
      entityRow(
        {
          collection: "workspace",
          id: "current",
          value: { stateDir: "/two", custom: "new" },
        },
        7,
      ),
    ],
    true,
  );

  assert.equal(changedWorkspace.stateDir, "/two");

  assert.equal(
    "custom" in changedWorkspace.runtime
      ? changedWorkspace.runtime.custom
      : undefined,
    "new",
  );
});

it("retains graph nodes while another collection changes", () => {
  const state = emptyEntityProjection();
  const rows: [EntityRow, EntityRow, EntityRow] = [
    entityRow(
      { collection: "agent", id: "a", value: { id: "a", name: "A" } },
      1,
    ),
    entityRow(
      { collection: "chat", id: "c", value: { id: "c", name: "C" } },
      2,
    ),
    entityRow(
      { collection: "rule", id: "r", value: { id: "r", checks: 0 } },
      3,
    ),
  ];
  const initial = applyEntityRows(state, rows, true);
  const changed = applyEntityRows(
    state,
    [
      rows[0],
      rows[1],
      entityRow(
        { collection: "rule", id: "r", value: { id: "r", checks: 1 } },
        4,
      ),
    ],
    true,
  );

  assert.equal(changed.nodes, initial.nodes);

  assert.equal(changed.threads, initial.threads);

  assert.equal(changed.chats, initial.chats);

  assert.ok(
    changed.runtime.rules[0] !== null && changed.runtime.rules[0] !== undefined,
  );
  assert.equal(
    "checks" in changed.runtime.rules[0]
      ? changed.runtime.rules[0].checks
      : undefined,
    1,
  );
  const renamed = applyEntityRows(
    state,
    [
      rows[0],
      entityRow(
        { collection: "chat", id: "c", value: { id: "c", name: "Renamed" } },
        5,
      ),
      entityRow(
        { collection: "rule", id: "r", value: { id: "r", checks: 1 } },
        4,
      ),
    ],
    true,
  );

  assert.notEqual(renamed.nodes, changed.nodes);

  assert.ok(renamed.nodes[1] !== null && renamed.nodes[1] !== undefined);
  assert.equal(renamed.nodes[1].name, "Renamed");
});

it("bounds retained tombstones after deletes and missing rows", () => {
  const explicit = emptyEntityProjection();
  const rows = Array.from({ length: 4097 }, (_, index) =>
    entityRow(
      { collection: "agent", id: `a${index}`, value: { id: `a${index}` } },
      index + 1,
    ),
  );
  applyEntityRows(explicit, rows, false);
  applyEntityRows(
    explicit,
    [
      entityRow(
        { collection: "agent", id: "a0", value: { id: "a0", name: "kept" } },
        5000,
      ),
    ],
    false,
  );
  applyEntityRows(
    explicit,
    [
      entityRow({ collection: "agent", id: "a0", value: { id: "a0" } }, 5001, {
        _deleted: true,
      }),
    ],
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
    [
      entityRow(
        { collection: "agent", id: "a4096", value: { id: "a4096" } },
        5000,
      ),
    ],
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
        {
          collection: "agent",
          id: "extra",
          value: {
            id: "extra",
            kind: "agent",
            name: "Worker",
            unknownPrivateLookingField: "retained",
          },
        },
        1,
      ),
      entityRow(
        // @ts-expect-error name is deliberately numeric to witness envelope-only validation.
        {
          collection: "agent",
          id: "wrong-type",
          value: {
            id: "wrong-type",
            kind: "agent",
            name: 42,
          },
        },
        2,
      ),
    ],
    true,
  );

  assert.equal(snapshot.runtime.agents.length, 2);

  assert.ok(
    snapshot.runtime.agents[0] !== null &&
      snapshot.runtime.agents[0] !== undefined,
  );
  assert.equal(
    "unknownPrivateLookingField" in snapshot.runtime.agents[0]
      ? snapshot.runtime.agents[0].unknownPrivateLookingField
      : undefined,
    "retained",
  );

  assert.ok(
    snapshot.runtime.agents[1] !== null &&
      snapshot.runtime.agents[1] !== undefined,
  );
  assert.equal(snapshot.runtime.agents[1].name, 42);

  assert.equal(state.rows.get("entity:agent:extra")?.seq, 1);

  assert.equal(state.rows.get("entity:agent:wrong-type")?.seq, 2);
});

it("applies v4 structured entity rows without parsing JSON strings", () => {
  const state = emptyEntityProjection();
  const upsert: EntityRow = {
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
        seq: 2,
        _deleted: true,
      } satisfies Omit<EntityRow, "payload">,
    ],
    true,
  );

  assert.deepEqual(deleted.threads, []);
  const stale = applyEntityRows(state, [upsert], true);

  assert.deepEqual(stale.threads, []);
});

it("projects independent account limits and preserves them across workspace updates", () => {
  const state = emptyEntityProjection();
  const alpha = { accountKey: "alpha", at: 100, data: { accountId: "A" } };
  const initial = applyEntityRows(
    state,
    [
      entityRow(
        {
          collection: "workspace",
          id: "current",
          value: { stateDir: "fixture" },
        },
        1,
      ),
      entityRow({ collection: "rateLimit", id: "alpha", value: alpha }, 2),
      entityRow(
        {
          collection: "rateLimit",
          id: "default",
          value: { accountKey: "default", at: 101 },
        },
        3,
      ),
    ],
    true,
  );

  assert.ok(
    initial.runtime.rateLimitsByAccount !== null &&
      initial.runtime.rateLimitsByAccount !== undefined,
  );
  assert.deepEqual(initial.runtime.rateLimitsByAccount.alpha, alpha);

  assert.ok(
    initial.runtime.rateLimitsByAccount !== null &&
      initial.runtime.rateLimitsByAccount !== undefined,
  );
  assert.equal(
    initial.runtime.rateLimits,
    initial.runtime.rateLimitsByAccount.default,
  );
  const next = applyEntityRows(
    state,
    [
      entityRow(
        {
          collection: "workspace",
          id: "current",
          value: { stateDir: "updated" },
        },
        4,
      ),
      entityRow({ collection: "rateLimit", id: "alpha", value: alpha }, 2),
      entityRow(
        {
          collection: "rateLimit",
          id: "default",
          value: { accountKey: "default", at: 101 },
        },
        3,
      ),
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
      entityRow(
        {
          collection: "workspace",
          id: "current",
          value: { stateDir: "updated" },
        },
        4,
      ),
      entityRow(
        {
          collection: "rateLimit",
          id: "default",
          value: { accountKey: "default", at: 101 },
        },
        3,
      ),
      {
        id: "entity:rateLimit:alpha",
        seq: 5,
        _deleted: true,
      } satisfies Omit<EntityRow, "payload">,
    ],
    true,
  );

  assert.ok(
    withoutAlpha.runtime.rateLimitsByAccount !== null &&
      withoutAlpha.runtime.rateLimitsByAccount !== undefined,
  );
  assert.equal(withoutAlpha.runtime.rateLimitsByAccount.alpha, undefined);

  assert.equal(withoutAlpha.runtime.rateLimits?.accountKey, "default");
});

it("projects Studio error entities and removes dismissed rows by tombstone", () => {
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
  } satisfies Extract<
    SyncEntityPayload,
    { collection: "studioError" }
  >["value"];
  const initial = applyEntityRows(
    state,
    [
      {
        id: "entity:workspace:current",
        entity: {
          collection: "workspace",
          id: "current",
          value: { stateDir: "fixture" },
        },
        seq: 1,
      } satisfies Omit<EntityRow, "payload">,
      entityRow({ collection: "studioError", id: item.id, value: item }, 2),
    ],
    true,
  );

  assert.deepEqual(initial.runtime.studioErrors, [item]);
  const removed = applyEntityRows(
    state,
    [
      {
        id: "entity:workspace:current",
        entity: {
          collection: "workspace",
          id: "current",
          value: { stateDir: "fixture" },
        },
        seq: 1,
      } satisfies Omit<EntityRow, "payload">,
      {
        id: `entity:studioError:${item.id}`,
        seq: 3,
        _deleted: true,
      } satisfies Omit<EntityRow, "payload">,
    ],
    true,
  );

  assert.deepEqual(removed.runtime.studioErrors, []);
});
it("projects queue stream upserts and deletion without endpoint reads", () => {
  const state = emptyEntityProjection();
  const value = {
    items: [
      { id: "q", text: "Queued", created: 0, kind: "user", status: "pending" },
    ],
    revision: "one",
    capabilities: { reorder: true, receipts: true },
  } satisfies Extract<SyncEntityPayload, { collection: "queue" }>["value"];
  const row: EntityRow = {
    id: "entity:queue:chat",
    seq: 1,
    payload: "",
    entity: { collection: "queue", id: "chat", value },
  };
  const initial = applyEntityRows(state, [row], true);

  assert.ok(
    initial.runtime.queuesByAgent !== null &&
      initial.runtime.queuesByAgent !== undefined,
  );
  assert.deepEqual(initial.runtime.queuesByAgent.chat, value);
  const next = applyEntityRows(
    state,
    [
      row,
      {
        id: "entity:queue:chat",
        seq: 2,
        _deleted: true,
      } satisfies Omit<EntityRow, "payload">,
    ],
    true,
  );

  assert.ok(
    next.runtime.queuesByAgent !== null &&
      next.runtime.queuesByAgent !== undefined,
  );
  assert.equal(next.runtime.queuesByAgent.chat, undefined);
});

const sourceCases: {
  entity: SyncEntityPayload;
  select: (snapshot: Snapshot) => unknown;
}[] = [
  {
    entity: {
      collection: "accounts",
      id: "current",
      value: {
        accounts: [],
        archivedAccounts: [],
        defaultAccountGroups: [],
        defaultAccountKey: "default",
        supportsDelete: false,
        supportsDisconnect: false,
      },
    },
    select: (snapshot) => snapshot.runtime.accountsSummary,
  },
  {
    entity: {
      collection: "accountCost",
      id: "record",
      value: {
        accountKey: "record",
        at: null,
        data: null,
        error: null,
        refreshing: false,
        stale: false,
      },
    },
    select: (snapshot) => snapshot.runtime.accountCostsByAccount?.record,
  },
  {
    entity: {
      collection: "sessionCost",
      id: "record",
      value: {
        breakdown: { models: {}, providers: {} },
        cacheAgeSeconds: 0,
        estimated: false,
        pricedSamples: 0,
        pricingState: "ready",
        refreshing: false,
        rootId: "record",
        totalUSD: null,
        unknownModels: [],
      },
    },
    select: (snapshot) => snapshot.runtime.sessionCostsByAgent?.record,
  },
  {
    entity: {
      collection: "desktop",
      id: "current",
      value: {
        application: "fixture",
        backendBuild: "fixture",
        browser: null,
        liveUpdate: null,
        mobileProtocol: 4,
        nativeRuntime: null,
        pid: 0,
        protocol: 4,
        providerVersions: { checkedAt: null, providers: [], warnings: [] },
        publicOrigin: null,
        restartEnvironment: {},
        stateDir: "/fixture",
        supervisor: null,
        supervisorError: null,
        supervisorFallback: false,
        supervisorMode: false,
        supervisorNotice: null,
        supervisorRequired: false,
      },
    },
    select: (snapshot) => snapshot.runtime.desktop,
  },
  {
    entity: {
      collection: "terminal",
      id: "record",
      value: {
        id: "record",
        agent: "agent",
        created: 0,
        cwd: "/fixture",
        status: "running",
        title: "fixture",
      },
    },
    select: (snapshot) => snapshot.runtime.terminals?.[0],
  },
];
for (const { entity, select } of sourceCases)
  it(`projects ${entity.collection} source entities and deletion`, () => {
    const state = emptyEntityProjection();
    const row = entityRow(entity, 1);
    const initial = applyEntityRows(state, [row], true);

    assert.deepEqual(select(initial), entity.value);
    const next = applyEntityRows(
      state,
      [
        row,
        { id: row.id, seq: 2, _deleted: true } satisfies Omit<
          EntityRow,
          "payload"
        >,
      ],
      true,
    );

    assert.equal(select(next), undefined);
  });
