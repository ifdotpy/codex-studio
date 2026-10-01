import assert from "node:assert/strict";
import {
  applyEntityRows,
  emptyEntityProjection,
} from "../web/src/sync/entityProjection.ts";
import {
  boundTranscriptItems,
  trimTranscriptPageCache,
} from "../web/src/transcriptPageBounds.ts";
import { incrementalHistoryGroups } from "../web/src/components/turnHistoryModel.ts";

const row = (collection, id, value, seq, deleted = false) => ({
  id: `entity:${collection}:${id}`,
  payload: JSON.stringify({ collection, id, value }),
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
  assert.equal(parsed, 3, "the initial entity rows parse once");
  const agentA = first.threads[0];
  const projectList = first.runtime.projects;

  const changed = applyEntityRows(
    state,
    [initial[0], row("agent", "b", { id: "b", name: "B2" }, 4), initial[2]],
    true,
  );
  assert.equal(parsed, 4, "one entity update parses one payload");
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
    [initial[0], row("agent", "b", { id: "b", name: "stale" }, 3), initial[2]],
    true,
  );
  assert.equal(
    stale.threads[1].name,
    "B2",
    "older sequences cannot replace newer values",
  );
  assert.equal(parsed, 4, "stale sequences are not parsed");

  const deleted = applyEntityRows(
    state,
    [initial[0], row("agent", "b", {}, 5, true), initial[2]],
    true,
  );
  assert.deepEqual(
    deleted.threads.map((agent) => agent.id),
    ["a"],
  );
  assert.equal(parsed, 4, "tombstones do not parse payloads");
  const afterStale = applyEntityRows(
    state,
    [initial[0], row("agent", "b", { id: "b", name: "stale" }, 4), initial[2]],
    true,
  );
  assert.deepEqual(
    afterStale.threads.map((agent) => agent.id),
    ["a"],
  );
  assert.equal(parsed, 4, "an older row cannot resurrect a newer delete");
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
assert.equal(nextGroups[0], firstGroups[0], "unchanged groups keep references");
assert.notEqual(nextGroups[3], firstGroups[3], "the changed group is rebuilt");
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
const bigBound = boundTranscriptItems(
  Array.from({ length: 10 }, (_, index) => ({
    id: `big${index}`,
    role: "assistant",
    text: "x".repeat(1_100_000),
  })),
  (item) => item.text.length * 2,
  "newest",
);
assert.ok(bigBound.bytes <= 4_000_000, "the byte limit bounds large messages");
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
