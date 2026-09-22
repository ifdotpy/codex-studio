import assert from "node:assert/strict";
import { registerHooks } from "node:module";
const hooks = registerHooks({
  resolve(specifier, context, next) {
    if (specifier === "../types" || specifier === "./nativeErrors")
      specifier += ".ts";
    return next(specifier, context);
  },
});
const { chatIndicators, unreadResult, chatActivities } = await import(
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
  "unread",
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
  "unread",
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
const inboxAttention = {
  complaints: [
    { leadId: "lead", recipient: "user", needsResponse: true, status: "open" },
  ],
  userTasks: [
    { agent: "lead", status: "open" },
    { agent: "child", rootId: "lead", status: "review" },
  ],
};
for (const [agent, expected] of [
  [{ ...lead, status: "running" }, "working"],
  [lead, "unread"],
  [
    {
      ...lead,
      readState: {
        threadId: "thread",
        turnId: "turn",
        read: true,
        revision: 1,
      },
    },
    "none",
  ],
]) {
  assert.equal(
    state(agent, inboxAttention),
    expected,
    "Inbox items cannot replace the chat status",
  );
  assert.equal(
    state(agent, {
      ...inboxAttention,
      requests: [{ agent: "lead", status: "pending", epoch: 2 }],
    }),
    "answer",
    "An actual pending question still takes priority over inbox items",
  );
}
assert.equal(
  chatIndicators(snapshot([lead, child], inboxAttention)).get("lead").kind,
  "unread",
  "A child user task cannot mark the lead as awaiting an answer",
);
console.log(
  "PASS chat status precedence, work and monitors, unread identity, pending questions without inbox escalation, deferred and stale requests, errors, stopped agents, and team aggregation",
);

const pausedChild = {
  ...child,
  status: "paused",
  autoWake: false,
  inFlight: false,
};
const command = {
  id: "long-command",
  agent: child.id,
  status: "running",
  command: "find /Users/igor",
  created: 10,
};
const activeSnapshot = snapshot([lead, pausedChild], { tasks: [command] });
const reasons = chatActivities(activeSnapshot);
assert.equal(reasons.get("lead")[0].id, command.id);
assert.equal(reasons.get("lead")[0].agentName, child.name);
assert.equal(reasons.get("lead")[0].command, command.command);
assert.equal(reasons.get("child")[0].created, 10);
assert.equal(chatIndicators(activeSnapshot).get("lead").kind, "working");
assert.equal(
  chatActivities(
    snapshot([lead, pausedChild], {
      tasks: [{ ...command, status: "completed" }],
    }),
  ).size,
  0,
);
assert.equal(
  chatActivities(
    snapshot([lead, pausedChild], { tasks: [{ ...command, epoch: 1 }] }),
  ).size,
  0,
);
const activeChild = { ...child, status: "running", inFlight: true };
assert.equal(
  chatActivities(snapshot([lead, activeChild], { tasks: [command] })).get(
    "lead",
  ).length,
  1,
);
assert.equal(
  chatActivities(snapshot([lead, activeChild])).get("lead")[0].kind,
  "agent",
);
const otherRoot = { ...lead, id: "other-root", rootId: "other-root" };
assert.equal(
  chatActivities(
    snapshot([lead, pausedChild, otherRoot], { tasks: [command] }),
  ).has("other-root"),
  false,
);
console.log(
  "PASS visible activity reasons match spinner, including live commands from stopped workers and scope boundaries",
);
