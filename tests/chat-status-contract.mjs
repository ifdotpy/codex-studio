import assert from "node:assert/strict";
import { registerHooks } from "node:module";
const hooks = registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "../types" || specifier === "./nativeErrors")
      specifier += ".ts";
    return next(specifier, context);
  },
});
const { chatIndicators, unreadResult } = await import(
  "../web/src/components/chatStatusModel.ts"
);
hooks.deregister();
const lead = {
  id: "lead",
  rootId: "lead",
  name: "Lead",
  source: "managed",
  isLead: true,
  status: "completed",
  autoWake: true,
  threadId: "thread",
  lastCompletedTurn: "turn",
  lastCompletedTurnStatus: "completed",
  epoch: 2,
};
const child = { ...lead, id: "child", rootId: "lead", isLead: false };
const snapshot = (agents = [lead], runtime = {}) => ({
  threads: agents,
  runtime: {
    requests: [],
    monitors: [],
    tasks: [],
    complaints: [],
    userTasks: [],
    ...runtime,
  },
});
const state = (agent = lead, runtime = {}) =>
  chatIndicators(snapshot([agent], runtime)).get(agent.id).kind;
assert.equal(state(), "unread");
assert.equal(
  state({
    ...lead,
    readState: { threadId: "thread", turnId: "turn", read: true, revision: 1 },
  }),
  "none",
);
for (const readState of [
  { threadId: "old", turnId: "turn", read: true },
  { threadId: "thread", turnId: "old", read: true },
  { threadId: "thread", turnId: "turn", read: false },
])
  assert(unreadResult({ ...lead, readState }));
assert.equal(state({ ...lead, lastCompletedTurnStatus: "failed" }), "none");
assert.equal(state({ ...lead, lastCompletedTurn: null }), "none");
for (const status of ["running", "starting", "queued", "waiting"])
  assert.equal(state({ ...lead, status }), "working", status);
assert.equal(
  state(lead, { monitors: [{ agent: "lead", status: "running" }] }),
  "working",
);
assert.equal(
  state(lead, {
    monitors: [{ agent: "lead", status: "running", panelFeed: true }],
  }),
  "unread",
);
assert.equal(
  state(lead, { monitors: [{ agent: "lead", status: "completed" }] }),
  "unread",
);
assert.equal(
  state(lead, { tasks: [{ agent: "lead", status: "running" }] }),
  "working",
);
assert.equal(
  state(
    { ...lead, status: "running" },
    { requests: [{ agent: "lead", status: "pending", epoch: 2 }] },
  ),
  "answer",
);
assert.equal(
  state(lead, { requests: [{ agent: "lead", status: "pending", epoch: 1 }] }),
  "unread",
);
assert.equal(
  state(lead, { requests: [{ agent: "lead", status: "answered" }] }),
  "unread",
);
assert.equal(
  state(lead, {
    requests: [{ agent: "lead", status: "pending", deferred: true }],
  }),
  "unread",
);
assert.equal(
  state(
    { ...lead, status: "approval" },
    { requests: [{ agent: "lead", status: "pending", deferred: true }] },
  ),
  "paused",
);
assert.equal(state({ ...lead, status: "failed" }), "error");
assert.equal(state({ ...lead, status: "paused", autoWake: false }), "paused");
assert.equal(state({ ...lead, status: "interrupted" }), "paused");
assert.equal(
  state(lead, {
    complaints: [
      {
        leadId: "lead",
        recipient: "lead",
        needsResponse: true,
        status: "open",
      },
    ],
  }),
  "unread",
);
assert.equal(
  state(lead, {
    complaints: [
      {
        leadId: "lead",
        recipient: "user",
        needsResponse: true,
        status: "open",
      },
    ],
  }),
  "answer",
);
assert.equal(
  state(lead, {
    complaints: [
      {
        leadId: "lead",
        recipient: "user",
        needsResponse: false,
        status: "resolved",
      },
    ],
  }),
  "unread",
);
assert.equal(
  state(lead, { userTasks: [{ agent: "lead", status: "open" }] }),
  "answer",
);
assert.equal(
  state(lead, { userTasks: [{ agent: "lead", status: "accepted" }] }),
  "unread",
);
assert.equal(
  chatIndicators(snapshot([lead, { ...child, status: "running" }])).get("lead")
    .kind,
  "working",
);
assert.equal(
  chatIndicators(
    snapshot([lead, child], {
      requests: [{ agent: "child", status: "pending", epoch: 2 }],
    }),
  ).get("lead").kind,
  "answer",
);
assert.equal(state({ ...lead, autoWake: false, inFlight: true }), "working");
for (const type of ["tasks", "monitors"]) {
  assert.equal(
    state(lead, { [type]: [{ agent: "lead", status: "running", epoch: 1 }] }),
    "unread",
  );
}
assert.equal(
  state(
    { ...lead, status: "approval" },
    {
      requests: [{ agent: "lead", status: "pending", epoch: 1 }],
    },
  ),
  "none",
  "An old approval status cannot resurrect a stale question",
);
console.log(
  "PASS chat status precedence, work and monitors, unread identity, pending questions, deferred and stale requests, errors, stopped agents, and team aggregation",
);
