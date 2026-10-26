import type { Agent, Snapshot } from "../../types";
import type { ChatActivity } from "./chatStatusModel";
import { componentSnapshot, required } from "../componentTestFixtures";
import assert from "node:assert/strict";
import {
  CHAT_INDICATOR_URGENCY,
  chatIndicators,
  unreadResult,
  chatActivities,
  backgroundActivities,
  chatWaitState,
  endedWaitLabel,
} from "./chatStatusModel.ts";

import { it } from "vitest";

it("keeps unread and attention indicators correct with summaries and empty panels", () => {
  const summary: Agent = {
    id: "chat",
    name: "Chat",
    source: "managed",
    isLead: true,
    status: "completed",
    threadId: "thread",
    lastCompletedTurn: "turn",
    lastCompletedTurnStatus: "completed",
  };
  const snapshot = (agent: Agent) => componentSnapshot([agent]);
  assert.equal(
    required(chatIndicators(snapshot(summary)).get("chat")).kind,
    "unread",
  );
  assert.equal(
    required(
      chatIndicators(snapshot({ ...summary, hasQuestion: true })).get("chat"),
    ).kind,
    "answer",
  );
  assert.equal(
    required(
      chatIndicators(
        snapshot({ ...summary, status: "waiting", hasBackgroundWork: true }),
      ).get("chat"),
    ).kind,
    "working",
  );
  assert.equal(
    required(
      chatIndicators(
        snapshot({
          ...summary,
          readState: {
            threadId: "thread",
            turnId: "turn",
            read: true,
            revision: 1,
          },
        }),
      ).get("chat"),
    ).kind,
    "none",
  );
});

it("classifies unread, work, answer, error and paused states from current runtime data", () => {
  const lead: Agent = {
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
  const child: Agent = {
    ...lead,
    id: "child",
    rootId: "lead",
    parentId: "lead",
    isLead: false,
  };
  const snapshot = (
    agents: Agent[] = [lead],
    runtime: Partial<Snapshot["runtime"]> = {},
  ) => componentSnapshot(agents, runtime);
  const state = (
    agent: Agent = lead,
    runtime: Partial<Snapshot["runtime"]> = {},
  ) => required(chatIndicators(snapshot([agent], runtime)).get(agent.id)).kind;
  assert.equal(state(), "unread");
  assert.equal(
    state({
      ...lead,
      readState: {
        threadId: "thread",
        turnId: "turn",
        read: true,
        revision: 1,
      },
    }),
    "none",
  );
  for (const readState of [
    { revision: 1, threadId: "old", turnId: "turn", read: true },
    { revision: 1, threadId: "thread", turnId: "old", read: true },
    { revision: 1, threadId: "thread", turnId: "turn", read: false },
  ] as const)
    assert(unreadResult({ ...lead, readState }));
  assert.equal(state({ ...lead, lastCompletedTurnStatus: "failed" }), "none");
  assert.equal(state({ ...lead, lastCompletedTurn: null }), "none");
  for (const status of ["running", "starting", "queued"] as const)
    assert.equal(state({ ...lead, status }), "working", status);
  for (const status of ["waiting", "waiting_on_monitor"] as const) {
    assert.equal(
      state({ ...lead, status }),
      "none",
      "A stale waiting state without an active command does not show busy",
    );
    assert.equal(
      state(
        { ...lead, status },
        {
          monitors: [
            { id: "fixture-record", agent: "lead", status: "running" },
          ],
        },
      ),
      "working",
    );
    assert.equal(
      chatWaitState(
        snapshot([lead, { ...child, status }], {
          monitors: [
            { id: "fixture-record", agent: "child", status: "running" },
          ],
        }),
        lead,
      ).live,
      true,
    );
  }
  assert.equal(
    state(lead, {
      monitors: [{ id: "fixture-record", agent: "lead", status: "running" }],
    }),
    "working",
  );
  assert.equal(
    state(lead, {
      monitors: [{ id: "fixture-record", agent: "lead", status: "completed" }],
    }),
    "unread",
  );
  assert.equal(
    state(lead, {
      tasks: [{ id: "fixture-record", agent: "lead", status: "running" }],
    }),
    "working",
  );
  assert.equal(
    state(
      { ...lead, status: "running" },
      {
        requests: [
          { id: "fixture-record", agent: "lead", status: "pending", epoch: 2 },
        ],
      },
    ),
    "answer",
  );
  assert.equal(
    state(lead, {
      requests: [
        { id: "fixture-record", agent: "lead", status: "pending", epoch: 1 },
      ],
    }),
    "unread",
  );
  assert.equal(
    state(lead, {
      requests: [{ id: "fixture-record", agent: "lead", status: "answered" }],
    }),
    "unread",
  );
  assert.equal(
    state(lead, {
      requests: [
        {
          id: "fixture-record",
          agent: "lead",
          status: "pending",
          deferred: true,
        },
      ],
    }),
    "unread",
  );
  assert.equal(
    state(
      { ...lead, status: "approval" },
      {
        requests: [
          {
            id: "fixture-record",
            agent: "lead",
            status: "pending",
            deferred: true,
          },
        ],
      },
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
          id: "fixture-record",
          leadId: "lead",
          recipient: "lead",
          needsUserResponse: true,
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
          id: "fixture-record",
          leadId: "lead",
          recipient: "user",
          needsUserResponse: true,
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
          id: "fixture-record",
          leadId: "lead",
          recipient: "user",
          needsUserResponse: false,
          status: "resolved",
        },
      ],
    }),
    "unread",
  );
  assert.equal(
    state(lead, {
      // @ts-expect-error Legacy userTasks is not part of Snapshot.runtime; it must not create chat attention.
      userTasks: [{ id: "fixture-record", agent: "lead", status: "open" }],
    }),
    "unread",
  );
  assert.equal(
    state(lead, {
      // @ts-expect-error Legacy userTasks is not part of Snapshot.runtime; it must not create chat attention.
      userTasks: [{ id: "fixture-record", agent: "lead", status: "accepted" }],
    }),
    "unread",
  );
  assert.equal(
    required(
      chatIndicators(snapshot([lead, { ...child, status: "running" }])).get(
        "lead",
      ),
    ).kind,
    "working",
  );
  assert.equal(
    required(
      chatIndicators(
        snapshot([
          { ...lead, status: "waiting" },
          { ...child, status: "running" },
        ]),
      ).get("lead"),
    ).kind,
    "working",
    "A lead waiting for an active child remains busy",
  );
  assert.equal(
    required(
      chatIndicators(
        snapshot([lead, child], {
          requests: [
            {
              id: "fixture-record",
              agent: "child",
              status: "pending",
              epoch: 2,
            },
          ],
        }),
      ).get("lead"),
    ).kind,
    "answer",
  );
  assert.equal(state({ ...lead, autoWake: false, inFlight: true }), "working");
  for (const type of ["tasks", "monitors"] as const) {
    assert.equal(
      state(lead, {
        [type]: [{ id: "fixture-record", agent: "lead", status: "running" }],
      }),
      "working",
      "task and monitor entities have no epoch to mark this work stale",
    );
  }
  assert.equal(
    state(
      { ...lead, status: "approval" },
      {
        requests: [
          { id: "fixture-record", agent: "lead", status: "pending", epoch: 1 },
        ],
      },
    ),
    "none",
    "An old approval status cannot resurrect a stale question",
  );
  const inboxAttention: Partial<Snapshot["runtime"]> = {
    complaints: [
      {
        id: "fixture-record",
        leadId: "lead",
        recipient: "user",
        needsUserResponse: true,
        status: "open",
      },
    ],
    // @ts-expect-error Legacy userTasks is not part of Snapshot.runtime; it must not create chat attention.
    userTasks: [
      { id: "fixture-record", agent: "lead", status: "open" },
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
  ] as const) {
    assert.equal(
      state(agent, inboxAttention),
      expected,
      "Inbox items cannot replace the chat status",
    );
    assert.equal(
      state(agent, {
        ...inboxAttention,
        requests: [
          { id: "fixture-record", agent: "lead", status: "pending", epoch: 2 },
        ],
      }),
      "answer",
      "An actual pending question still takes priority over inbox items",
    );
  }
  assert.equal(
    required(
      chatIndicators(snapshot([lead, child], inboxAttention)).get("lead"),
    ).kind,
    "unread",
    "A child user task cannot mark the lead as awaiting an answer",
  );
  console.log(
    "PASS chat status precedence, work and monitors, unread identity, pending questions without inbox escalation, deferred and stale requests, errors, stopped agents, and team aggregation",
  );

  const pausedChild: Agent = {
    ...child,
    status: "paused",
    autoWake: false,
    inFlight: false,
  };
  const command: Snapshot["runtime"]["tasks"][number] = {
    id: "long-command",
    agent: child.id,
    status: "running",
    command: "find /Users/igor",
    created: 10,
  };
  const activeSnapshot = snapshot([lead, pausedChild], { tasks: [command] });
  const reasons = chatActivities(activeSnapshot);
  assert.equal(required(required(reasons.get("lead"))[0]).id, command.id);
  assert.equal(
    required(required(reasons.get("lead"))[0]).agentName,
    child.name,
  );
  assert.equal(
    required(required(reasons.get("lead"))[0]).command,
    command.command,
  );
  assert.equal(required(required(reasons.get("child"))[0]).created, 10);
  assert.equal(
    required(chatIndicators(activeSnapshot).get("lead")).kind,
    "working",
  );
  assert.equal(
    chatActivities(
      snapshot([lead, pausedChild], {
        tasks: [{ ...command, status: "completed" }],
      }),
    ).size,
    0,
  );
  const activeChild: Agent = { ...child, status: "running", inFlight: true };
  assert.equal(
    required(
      chatActivities(snapshot([lead, activeChild], { tasks: [command] })).get(
        "lead",
      ),
    ).length,
    1,
  );
  assert.equal(
    required(
      required(chatActivities(snapshot([lead, activeChild])).get("lead"))[0],
    ).kind,
    "agent",
  );
  const otherRoot: Agent = { ...lead, id: "other-root", rootId: "other-root" };
  assert.equal(
    chatActivities(
      snapshot([lead, pausedChild, otherRoot], { tasks: [command] }),
    ).has("other-root"),
    false,
  );
  // The bar above the chat shows only monitors and commands outside their turn.
  const turnChild: Agent = {
    ...child,
    status: "running",
    inFlight: true,
    turnId: "t2",
  };
  const strip = (
    tasks: Snapshot["runtime"]["tasks"],
    monitors: Snapshot["runtime"]["monitors"] = [],
    agents: Agent[] = [lead, turnChild],
  ) =>
    backgroundActivities(
      required(
        chatActivities(snapshot(agents, { tasks, monitors })).get("lead"),
      ) || [],
    ).map((activity) => activity.id);
  assert.deepEqual(strip([{ ...command, turnId: "t2" }]), []);
  assert.deepEqual(strip([{ ...command, turnId: "t1" }]), [command.id]);
  assert.deepEqual(strip([command], [], [lead, pausedChild]), [command.id]);
  assert.deepEqual(
    strip([], [{ id: "watch", agent: child.id, status: "running" }]),
    ["watch"],
  );
  assert.deepEqual(strip([]), []);
  assert.equal(
    required(
      required(chatActivities(snapshot([lead, turnChild])).get("lead"))[0],
    ).kind,
    "agent",
  );
  console.log(
    "PASS visible activity reasons match spinner, including live commands from stopped workers and scope boundaries",
  );

  const waitingLead: Agent = { ...lead, status: "waiting", inFlight: false };
  const waitState = (
    agents: Agent[] = [waitingLead],
    runtime: Partial<Snapshot["runtime"]> = {},
    agent = required(agents[0]),
  ) => chatWaitState(snapshot(agents, runtime), agent);
  assert.equal(waitState().live, false);
  assert.equal(waitState().label, "Turn ended. Send a message to continue.");
  assert.deepEqual(waitState().commands, []);
  assert.deepEqual(waitState().monitors, []);
  assert.deepEqual(
    required(chatIndicators(snapshot([waitingLead])).get("lead")),
    {
      kind: "none",
      label: endedWaitLabel,
    },
  );
  for (const status of ["queued", "starting", "running", "approval"] as const) {
    const agents = [waitingLead, { ...child, status }];
    assert.equal(waitState(agents).label, "Waiting for 1 agent", status);
    assert.deepEqual(required(chatIndicators(snapshot(agents)).get("lead")), {
      kind: "working",
      label: "Waiting for 1 agent",
    });
  }
  assert.equal(
    waitState([waitingLead, { ...child, status: "completed", inFlight: true }])
      .live,
    true,
  );
  for (const status of [
    "idle",
    "completed",
    "failed",
    "paused",
    "interrupted",
  ] as const)
    assert.equal(
      waitState([waitingLead, { ...child, status, inFlight: false }]).live,
      false,
      status,
    );
  assert.equal(
    waitState([
      waitingLead,
      { ...child, source: "registered", status: "running" },
    ]).live,
    false,
    "only managed children can keep a conversation waiting",
  );
  assert.equal(
    waitState([waitingLead, { ...child, status: "running", parentId: "other" }])
      .live,
    false,
  );
  const ownCommand = {
    ...command,
    agent: "lead",
    kind: "command",
  } satisfies Snapshot["runtime"]["tasks"][number];
  const ownMonitor: Snapshot["runtime"]["monitors"][number] = {
    id: "monitor",
    agent: "lead",
    command: "watch build",
    status: "running",
  };
  for (const status of ["starting", "running", "approval"] as const) {
    assert.equal(
      waitState(undefined, { tasks: [{ ...ownCommand, status }] }).label,
      "Waiting for 1 command",
    );
    assert.equal(
      waitState(undefined, { monitors: [{ ...ownMonitor, status }] }).label,
      "Waiting for 1 monitor",
    );
  }
  for (const record of [
    { status: "completed" },
    { status: "failed" },
    { id: "fixture-record", agent: "other" },
  ] as const) {
    assert.equal(
      waitState(undefined, { tasks: [{ ...ownCommand, ...record }] }).live,
      false,
    );
    assert.equal(
      waitState(undefined, { monitors: [{ ...ownMonitor, ...record }] }).live,
      false,
    );
  }
  assert.equal(
    waitState(undefined, {
      tasks: [
        { ...ownCommand, command: undefined, kind: "tool", query: "search" },
      ],
    }).live,
    false,
  );
  assert.equal(
    chatWaitState(
      snapshot([{ ...waitingLead, inFlight: false }]),
      { ...waitingLead, inFlight: false },
      new Map<string, ChatActivity[]>([
        [
          "lead",
          [
            {
              id: ownCommand.id,
              agentName: "Lead",
              label: "Command",
              status: "running",
              agentId: "lead",
              command: undefined,
              kind: "task",
              commandTask: true,
              background: true,
            },
          ],
        ],
      ]),
    ).commands.length,
    1,
    "a command task remains visible when its command text is unavailable",
  );
  const mismatchedRecordIsNotOwned = chatWaitState(
    snapshot([{ ...waitingLead, inFlight: false }]),
    { ...waitingLead, inFlight: false },
    new Map<string, ChatActivity[]>([
      [
        "lead",
        [
          {
            id: ownCommand.id,
            agentId: "lead",
            agentName: "Lead",
            label: "Command",
            status: "running",
            kind: "agent",
            commandTask: true,
            background: true,
          },
        ],
      ],
    ]),
  );
  assert.deepEqual(mismatchedRecordIsNotOwned.commands, []);
  assert.equal(
    chatWaitState(
      snapshot([{ ...waitingLead, inFlight: false }]),
      { ...waitingLead, inFlight: false },
      new Map<string, ChatActivity[]>([
        [
          "lead",
          [
            {
              id: ownCommand.id,
              agentName: "Lead",
              label: "Command",
              status: "running",
              agentId: "lead",
              kind: "task",
              commandTask: true,
              background: false,
            },
          ],
        ],
      ]),
    ).commands.length,
    1,
    "a stopped turn makes even a foreground task a live wait trigger",
  );
  assert.equal(
    waitState([{ ...waitingLead, inFlight: true, turnId: "current" }], {
      tasks: [{ ...ownCommand, turnId: "current" }],
    }).live,
    false,
    "A foreground tool is not a wake trigger after a turn",
  );
  const parked: Agent = {
    ...waitingLead,
    status: "parked",
    parkedEvent: "build-ready",
    autoWake: false,
  };
  assert.equal(waitState([parked]).label, "Waiting for event build-ready");
  assert.deepEqual(required(chatIndicators(snapshot([parked])).get("lead")), {
    kind: "working",
    label: "Waiting for event build-ready",
  });
  const input: Snapshot["runtime"]["requests"][number] = {
    id: "input",
    agent: "lead",
    status: "pending",
    epoch: 2,
  };
  assert.equal(
    waitState(undefined, { requests: [input] }).label,
    "Waiting for 1 input",
  );
  assert.equal(
    // @ts-expect-error A malformed snapshot can omit the required requests collection at runtime.
    chatWaitState(snapshot([waitingLead], { requests: undefined }), waitingLead)
      .inputs,
    0,
    "snapshots without a requests collection contain no input triggers",
  );
  assert.equal(
    chatWaitState(
      snapshot([waitingLead], {
        requests: [{ id: "fixture-record", agent: "lead" }],
      }),
      waitingLead,
    ).inputs,
    1,
    "an unscoped request epoch remains current",
  );
  assert.deepEqual(
    required(
      chatIndicators(snapshot([waitingLead], { requests: [input] })).get(
        "lead",
      ),
    ),
    {
      kind: "answer",
      label: "Waiting for 1 input",
    },
  );
  for (const record of [
    { status: "answered" },
    { deferred: true },
    { epoch: 1 },
    { id: "fixture-record", agent: "other" },
  ] as const)
    assert.equal(
      waitState(undefined, { requests: [{ ...input, ...record }] }).live,
      false,
    );
  const twoWorkers: Agent[] = [
    waitingLead,
    { ...child, status: "running" },
    { ...child, id: "child-2", status: "queued" },
  ];
  assert.equal(waitState(twoWorkers).label, "Waiting for 2 agents");
  assert.equal(
    waitState(twoWorkers, { monitors: [ownMonitor] }).label,
    "Waiting for 2 agents and 1 monitor",
  );
  assert.equal(
    waitState(twoWorkers, { tasks: [ownCommand], monitors: [ownMonitor] })
      .label,
    "Waiting for 2 agents, 1 command and 1 monitor",
  );
  assert.equal(
    waitState([{ ...waitingLead, parkedEvent: "ready" }], {
      tasks: [ownCommand, { ...ownCommand, id: "cmd-2" }],
      monitors: [ownMonitor, { ...ownMonitor, id: "monitor-2" }],
      requests: [input, { ...input, id: "input-2" }],
    }).label,
    "Waiting for 2 commands, 2 monitors, event ready and 2 inputs",
  );
  console.log(
    "PASS live wait triggers: workers, commands, monitors, event, input, exact counts, stale records, ownership, and static turn end",
  );
});

const makeAgent = (overrides: Partial<Agent> = {}): Agent => ({
  id: "lead",
  rootId: "lead",
  name: "Lead",
  source: "managed",
  isLead: true,
  status: "completed",
  autoWake: true,
  epoch: 2,
  threadId: "thread",
  lastCompletedTurn: "turn",
  lastCompletedTurnStatus: "completed",
  ...overrides,
});

const makeSnapshot = componentSnapshot;

it("keeps a parent waiting for a child with current command or monitor work", () => {
  const lead = makeAgent({ status: "waiting", inFlight: false });
  const child = makeAgent({
    id: "child",
    parentId: lead.id,
    rootId: lead.id,
    isLead: false,
    status: "waiting",
    inFlight: false,
  });
  for (const kind of ["monitors", "tasks"] as const) {
    const work: Snapshot["runtime"]["tasks"][number] = {
      id: "work",
      agent: child.id,
      status: "running",
      command: "watch build",
    };
    for (const status of ["starting", "running", "approval"] as const) {
      const snapshot = makeSnapshot([lead, child], {
        [kind]: [{ ...work, status }],
      });
      assert.deepEqual(required(chatIndicators(snapshot).get(lead.id)), {
        kind: "working",
        label: "Waiting for 1 agent",
      });
      const wait = chatWaitState(snapshot, lead);
      assert.deepEqual(
        wait.agents.map(({ id }) => id),
        [child.id],
      );
      assert.deepEqual(wait.monitors, []);
      assert.deepEqual(wait.commands, []);
      assert.equal(chatWaitState(snapshot, child).live, true);
    }
    for (const change of [
      { status: "completed" },
      { status: "failed" },
      { status: "cancelled" },
      { status: "lost" },
      { id: "fixture-record", agent: "unknown" },
    ] as const) {
      const snapshot = makeSnapshot([lead, child], {
        [kind]: [{ ...work, ...change }],
      });
      assert.deepEqual(required(chatIndicators(snapshot).get(lead.id)), {
        kind: "none",
        label: endedWaitLabel,
      });
    }
    for (const change of [
      { status: "paused", autoWake: false },
      { status: "failed" },
      { status: "interrupted" },
      { status: "completed" },
      { status: "idle" },
      { archived: true },
      { autoWake: false },
      { source: "registered" },
    ] as const) {
      const snapshot = makeSnapshot([lead, { ...child, ...change }], {
        [kind]: [work],
      });
      assert.equal(chatWaitState(snapshot, lead).live, false);
    }
    const snapshot = makeSnapshot([lead, child], {
      [kind]: [work],
      requests: [
        {
          id: "fixture-record",
          agent: child.id,
          epoch: child.epoch,
          status: "pending",
        },
      ],
    });
    assert.equal(
      required(chatIndicators(snapshot).get(lead.id)).kind,
      "answer",
    );
  }
  assert.equal(
    chatWaitState(
      makeSnapshot([lead, child], {
        tasks: [{ id: "tool", agent: child.id, status: "running" }],
      }),
      lead,
    ).live,
    false,
    "an unfinished generic tool record does not promise another child turn",
  );
});

it("counts active work through waiting descendants without crossing stopped branches", () => {
  const lead = makeAgent({ status: "waiting", inFlight: false });
  const middle = makeAgent({
    id: "middle",
    parentId: lead.id,
    rootId: lead.id,
    isLead: false,
    status: "waiting",
    inFlight: false,
  });
  const child: Agent = { ...middle, id: "child", parentId: middle.id };
  const monitor: Snapshot["runtime"]["monitors"][number] = {
    id: "watch",
    agent: child.id,
    status: "running",
    // @ts-expect-error Monitor/task DTOs have no epoch; legacy metadata must not create an extra trigger.
    epoch: child.epoch,
  };
  const snapshot = makeSnapshot([lead, middle, child], { monitors: [monitor] });
  const indicators = chatIndicators(snapshot);
  for (const agent of [lead, middle] as const) {
    assert.deepEqual(required(indicators.get(agent.id)), {
      kind: "working",
      label: "Waiting for 1 agent",
    });
    assert.equal(
      required(chatWaitState(snapshot, agent).agents[0]).parentId,
      agent.id,
    );
  }
  assert.deepEqual(required(indicators.get(child.id)), {
    kind: "working",
    label: "Waiting for 1 monitor",
  });
  for (const change of [
    { status: "paused", autoWake: false },
    { status: "failed" },
    { archived: true },
    { autoWake: false },
    { source: "registered" },
    { parentId: "other-root" },
  ] as const) {
    const snapshot = makeSnapshot([lead, { ...middle, ...change }, child], {
      monitors: [monitor],
    });
    assert.equal(chatWaitState(snapshot, lead).live, false);
  }
  for (const runtime of [
    {},
    { monitors: [{ ...monitor, status: "completed" }] },
  ] as const) {
    const snapshot = makeSnapshot(
      [lead, middle, child],
      runtime.monitors ? { monitors: [...runtime.monitors] } : {},
    );
    for (const agent of [lead, middle, child] as const)
      assert.equal(chatWaitState(snapshot, agent).live, false);
  }
  const sibling: Agent = { ...child, id: "sibling", parentId: lead.id };
  assert.equal(
    chatWaitState(
      makeSnapshot([lead, middle, child, sibling], {
        monitors: [{ ...monitor, agent: sibling.id }],
      }),
      middle,
    ).live,
    false,
    "a root activity does not become work owned by a sibling branch",
  );
  assert.equal(
    chatWaitState(
      makeSnapshot([lead, { ...middle, parentId: child.id }, child], {
        monitors: [monitor],
      }),
      middle,
    ).agents.length,
    1,
    "a parent cycle cannot include the requesting agent again",
  );
});

it("keeps the parent waiting for a parked child's saved event", () => {
  const lead = makeAgent({ status: "waiting", inFlight: false });
  const child = makeAgent({
    id: "child",
    parentId: lead.id,
    rootId: lead.id,
    status: "parked",
    inFlight: false,
    autoWake: false,
    parkedEvent: "build-ready",
  });
  const monitor: Snapshot["runtime"]["tasks"][number] = {
    id: "watch",
    agent: child.id,
    status: "running",
    // @ts-expect-error Monitor/task DTOs have no epoch; legacy metadata must not create an extra trigger.
    epoch: 2,
  };
  for (const monitors of [[], [monitor]] as const) {
    const snapshot = makeSnapshot([lead, child], { monitors: [...monitors] });
    assert.deepEqual(required(chatIndicators(snapshot).get(lead.id)), {
      kind: "working",
      label: "Waiting for 1 agent",
    });
    assert.equal(chatWaitState(snapshot, child).event, "build-ready");
  }
  for (const change of [
    { parkedEvent: null },
    { parkedEvent: "" },
    { status: "waiting" },
    { status: "paused" },
    { status: "failed" },
    { status: "interrupted" },
    { archived: true },
  ] as const)
    assert.equal(
      chatWaitState(
        makeSnapshot([lead, { ...child, ...change }], { monitors: [monitor] }),
        lead,
      ).live,
      false,
    );
});

it("indexes one wide team's children once for all chat indicators", () => {
  const lead = makeAgent({ status: "waiting", inFlight: false });
  const children = Array.from({ length: 1024 }, (_, index) =>
    makeAgent({
      id: `child-${index}`,
      parentId: lead.id,
      rootId: lead.id,
      status: "waiting",
      inFlight: false,
    }),
  );
  const snapshot = makeSnapshot([lead, ...children], {
    monitors: [
      {
        id: "watch",
        agent: required(children[0]).id,
        status: "running",
        // @ts-expect-error Monitor/task DTOs have no epoch; legacy metadata must not create an extra trigger.
        epoch: 2,
      },
    ],
  });
  const threads = snapshot.threads;
  let reads = 0;
  Object.defineProperty(snapshot, "threads", {
    get() {
      reads++;
      return threads;
    },
  });
  const indicators = chatIndicators(snapshot);
  assert.equal(indicators.size, 1025);
  assert.equal(required(indicators.get(lead.id)).label, "Waiting for 1 agent");
  assert(reads <= 3, `the team roster was read ${reads} times`);
});

it("rejects malformed request epochs while preserving absent, null and numeric rules", () => {
  const failed = makeAgent({ status: "failed", epoch: 4 });
  const waiting = makeAgent({ status: "waiting", epoch: 4 });
  const requestEpochs = [
    [{}, true],
    [{ epoch: null }, true],
    [{ epoch: 4 }, true],
    [{ epoch: 3 }, false],
    [{ epoch: "3" }, false],
  ] as const;

  for (const [record, current] of requestEpochs) {
    // @ts-expect-error A string request epoch is invalid; the runtime must reject it.
    const request: Snapshot["runtime"]["requests"][number] = {
      id: "fixture-record",
      agent: "lead",
      status: "pending",
      ...record,
    };
    assert.equal(
      chatWaitState(makeSnapshot([waiting], { requests: [request] }), waiting)
        .inputs,
      current ? 1 : 0,
      `wait input epoch ${JSON.stringify(record)}`,
    );
    assert.equal(
      required(
        chatIndicators(makeSnapshot([failed], { requests: [request] })).get(
          "lead",
        ),
      ).kind,
      current ? "answer" : "error",
      `indicator request epoch ${JSON.stringify(record)}`,
    );
  }
});

it("maps live runtime records to sorted caller-visible activities", () => {
  const lead = makeAgent();
  const child = makeAgent({
    id: "child",
    rootId: "lead",
    parentId: "lead",
    isLead: false,
    name: "Worker",
    status: "running",
    inFlight: true,
    turnId: "current",
  });
  const snapshot = makeSnapshot([lead, child], {
    tasks: [
      { id: "z", agent: "child", status: "running", created: 10 },
      {
        id: "command",
        agent: "child",
        kind: "command",
        command: "git status",
        status: "approval",
        created: 2,
        turnId: "old",
      },
      {
        id: "tool",
        agent: "child",
        query: "search query",
        // @ts-expect-error Task DTOs have no type field; legacy metadata must not determine command activity.
        type: "search",
        status: "starting",
        created: 2,
        turnId: "current",
      },
      {
        id: "ignored-status",
        agent: "child",
        status: "completed",
        command: "done",
      },
      { id: "unknown-agent", agent: "missing", status: "running" },
    ],
    monitors: [
      {
        id: "monitor",
        agent: "child",
        command: "watch",
        name: "watcher",
        status: "running",
        created: 2,
      },
      {
        id: "monitor-no-command",
        agent: "child",
        name: "watcher without a command",
        status: "running",
        created: 2,
      },
    ],
  });

  const activities = chatActivities(snapshot);
  assert.deepEqual(
    required(activities.get("lead")).map(({ id }) => id),
    ["command", "monitor", "monitor-no-command", "tool", "z"],
    "activities are shared with the root chat and sort by time, then ID",
  );
  assert.deepEqual(required(required(activities.get("lead"))[0]), {
    id: "command",
    kind: "task",
    agentId: "child",
    agentName: "Worker",
    label: "Background command",
    command: "git status",
    created: 2,
    status: "approval",
    background: true,
    commandTask: true,
  });
  assert.deepEqual(required(required(activities.get("lead"))[1]), {
    id: "monitor",
    kind: "monitor",
    agentId: "child",
    agentName: "Worker",
    label: "Background command",
    command: "watch",
    created: 2,
    status: "running",
    background: true,
    commandTask: false,
  });
  assert.equal(
    required(required(activities.get("lead"))[2]).label,
    "Background command",
  );
  assert.equal(
    required(required(activities.get("lead"))[2]).command,
    "watcher without a command",
  );
  assert.equal(
    required(required(activities.get("lead"))[3]).label,
    "Tool call",
  );
  assert.equal(
    required(required(activities.get("lead"))[3]).command,
    "search query",
  );
  assert.equal(required(required(activities.get("lead"))[3]).background, false);
  assert.equal(
    required(required(activities.get("lead"))[3]).commandTask,
    false,
  );
  assert.equal(required(activities.get("child")).length, 5);
  assert.deepEqual(
    backgroundActivities(required(activities.get("lead"))).map(({ id }) => id),
    ["command", "monitor", "monitor-no-command"],
  );

  const selfActivity = chatActivities(
    makeSnapshot([lead], {
      tasks: [{ id: "self", agent: "lead", status: "running", command: "pwd" }],
    }),
  );
  assert.deepEqual(
    required(selfActivity.get("lead")).map(({ id }) => id),
    ["self"],
  );
  const unknownRootActivity = chatActivities(
    makeSnapshot(
      [makeAgent({ id: "orphan", rootId: "missing-root", inFlight: true })],
      { tasks: [{ id: "orphan-task", agent: "orphan", status: "running" }] },
    ),
  );
  assert.equal(unknownRootActivity.has("missing-root"), false);
  assert.deepEqual(
    required(
      chatIndicators(
        makeSnapshot([makeAgent({ id: "orphan", rootId: "missing-root" })], {
          requests: [{ id: "fixture-record", agent: "orphan" }],
        }),
      ).get("orphan"),
    ),
    { kind: "answer", label: "Needs your answer" },
    "an unknown team root does not affect the worker's own answer indicator",
  );
  assert.deepEqual(
    required(
      chatIndicators(
        makeSnapshot(
          [
            makeAgent({ id: "regular", rootId: "" }),
            makeAgent({ id: "", rootId: "", status: "idle" }),
          ],
          { requests: [{ id: "fixture-record", agent: "regular" }] },
        ),
      ).get(""),
    ),
    { kind: "none", label: "Ready" },
    "an empty root identifier does not propagate an answer to another chat",
  );
});

it("uses the runtime command, turn, epoch and fallback rules for activities", () => {
  const lead = makeAgent({ inFlight: true, turnId: "turn-3" });
  const noEpoch = makeAgent({
    id: "no-epoch",
    rootId: "lead",
    epoch: null,
    name: "No epoch",
    status: "paused",
    inFlight: false,
  });
  const child = makeAgent({
    id: "child",
    rootId: "lead",
    parentId: "lead",
    name: "Worker",
    status: "running",
    inFlight: true,
    turnId: "turn-3",
  });
  const activities = chatActivities(
    makeSnapshot([lead, noEpoch, child], {
      tasks: [
        {
          id: "command-kind-only",
          agent: "lead",
          kind: "command",
          status: "running",
          // @ts-expect-error Task DTOs have no type field; legacy metadata must not determine command activity.
          type: "exec",
        },
        {
          id: "command-without-kind",
          agent: "lead",
          status: "running",
          command: "git diff",
        },
        {
          id: "same-turn",
          agent: "lead",
          status: "running",
          command: "pwd",
          turnId: "turn-3",
        },
        {
          id: "different-turn",
          agent: "lead",
          status: "running",
          command: "ls",
          turnId: "turn-2",
        },
        {
          id: "unscoped-turn",
          agent: "lead",
          status: "running",
          command: "cat",
        },
        {
          id: "agent-without-epoch",
          agent: "no-epoch",
          status: "running",
          name: "named task",
        },
        {
          id: "tool-by-kind",
          agent: "child",
          kind: "tool",
          status: "running",
          // @ts-expect-error Task DTOs have no type field; legacy metadata must not determine command activity.
          type: "exec",
        },
      ],
    }),
  );
  assert.deepEqual(
    required(activities.get("lead")).map(({ id }) => id),
    [
      "agent-without-epoch",
      "command-kind-only",
      "command-without-kind",
      "different-turn",
      "same-turn",
      "tool-by-kind",
      "unscoped-turn",
    ],
  );
  assert.deepEqual(
    required(activities.get("lead"))
      .map(({ id }) => id)
      .filter((id) =>
        backgroundActivities(required(activities.get("lead"))).some(
          (entry) => entry.id === id,
        ),
      ),
    ["different-turn"],
  );
  assert.equal(
    required(activities.get("lead")).find(({ id }) => id === "same-turn")
      ?.label,
    "Command",
  );
  assert.equal(
    required(activities.get("lead")).find(
      ({ id }) => id === "command-without-kind",
    )?.commandTask,
    true,
  );
  assert.equal(
    required(activities.get("lead")).find(
      ({ id }) => id === "command-kind-only",
    )?.commandTask,
    true,
  );
  assert.equal(
    required(activities.get("lead")).find(({ id }) => id === "tool-by-kind")
      ?.command,
    "tool",
  );
  assert.equal(
    required(required(activities.get("no-epoch"))[0]).command,
    "named task",
  );
  assert.equal(
    required(required(activities.get("no-epoch"))[0]).label,
    "Tool call",
  );

  const fallback = (agent: Agent) =>
    chatActivities(makeSnapshot([agent])).get(agent.id)?.[0];
  for (const [status, label] of [
    ["queued", "Waiting to start"],
    ["starting", "Starting"],
    ["running", "Working"],
  ] as const) {
    assert.equal(
      fallback(makeAgent({ id: status, status, inFlight: false }))?.label,
      label,
    );
  }
  assert.equal(
    fallback(makeAgent({ id: "in-flight", status: "paused", inFlight: true }))
      ?.label,
    "Working",
  );
  for (const agent of [
    makeAgent({ id: "auto-wake-off", status: "running", autoWake: false }),
    makeAgent({ id: "completed", status: "completed", inFlight: false }),
    makeAgent({ id: "idle", status: "idle", inFlight: false }),
    makeAgent({
      id: "not-managed",
      source: "registered",
      status: "running",
      inFlight: true,
    }),
  ] as const)
    assert.equal(fallback(agent), undefined);
  const concreteWins = chatActivities(
    makeSnapshot(
      [makeAgent({ id: "concrete", status: "running", inFlight: true })],
      {
        tasks: [
          {
            id: "actual",
            agent: "concrete",
            status: "running",
            command: "make",
          },
        ],
      },
    ),
  );
  assert.deepEqual(
    required(concreteWins.get("concrete")).map(({ id }) => id),
    ["actual"],
  );
});

it("builds live wait labels only from current owned triggers", () => {
  const lead = makeAgent({
    status: "parked",
    inFlight: false,
    parkedEvent: "release-ready",
  });
  const child = makeAgent({
    id: "child",
    rootId: "lead",
    parentId: "lead",
    status: "approval",
    inFlight: true,
  });
  const task: ChatActivity = {
    agentName: "Lead",
    label: "Command",
    id: "command",
    kind: "task",
    commandTask: true,
    agentId: "lead",
    background: true,
    status: "running",
  };
  const monitor: ChatActivity = {
    agentName: "Lead",
    label: "Background command",
    id: "watch",
    kind: "monitor",
    agentId: "lead",
    status: "approval",
  };
  const snapshot = makeSnapshot([lead, child], {
    requests: [
      { id: "pending", agent: "lead", epoch: 2 },
      { id: "deferred", agent: "lead", status: "pending", deferred: true },
      { id: "answered", agent: "lead", status: "answered" },
      { id: "stale", agent: "lead", epoch: 1 },
      { id: "child", agent: "child", status: "pending" },
    ],
  });
  const state = chatWaitState(
    snapshot,
    lead,
    new Map<string, ChatActivity[]>([
      [
        "lead",
        [task, monitor, { ...task, id: "other-agent", agentId: "child" }],
      ],
    ]),
  );
  assert.equal(state.live, true);
  assert.equal(
    state.label,
    "Waiting for 1 agent, 1 command, 1 monitor, event release-ready and 1 input",
  );
  assert.deepEqual(
    state.agents.map(({ id }) => id),
    ["child"],
  );
  assert.deepEqual(
    state.commands.map(({ id }) => id),
    ["command"],
  );
  assert.deepEqual(
    state.monitors.map(({ id }) => id),
    ["watch"],
  );
  assert.equal(state.event, "release-ready");
  assert.equal(state.inputs, 1);

  const foreground = { ...task, id: "foreground", background: false };
  assert.equal(
    chatWaitState(
      makeSnapshot([makeAgent({ inFlight: true })]),
      makeAgent({ inFlight: true }),
      new Map<string, ChatActivity[]>([["lead", [foreground]]]),
    ).live,
    false,
  );
  const noEpochAgent = makeAgent({ epoch: null, status: "waiting" });
  assert.equal(
    chatWaitState(
      makeSnapshot([noEpochAgent], {
        requests: [{ id: "fixture-record", agent: "lead", epoch: 7 }],
      }),
      noEpochAgent,
    ).label,
    "Waiting for 1 input",
  );
  const scopedAgent = makeAgent({ status: "waiting", epoch: 2 });
  assert.equal(
    chatWaitState(
      makeSnapshot([scopedAgent], {
        requests: [{ id: "fixture-record", agent: "lead" }],
      }),
      scopedAgent,
    ).inputs,
    1,
  );
  const waitingChild = makeAgent({
    id: "wait-child",
    parentId: "lead",
    status: "completed",
    inFlight: true,
  });
  assert.equal(
    chatWaitState(makeSnapshot([lead, waitingChild]), lead).label,
    "Waiting for 1 agent and event release-ready",
  );
});

it("keeps completion receipts and operational indicator priority aligned", () => {
  const agent = makeAgent();
  assert.equal(unreadResult(agent), true);
  assert.equal(unreadResult({ ...agent, threadId: null }), false);
  assert.equal(unreadResult({ ...agent, lastCompletedTurn: null }), false);
  assert.equal(
    unreadResult({ ...agent, lastCompletedTurnStatus: "failed" }),
    false,
  );
  assert.equal(
    unreadResult(agent, {
      threadId: "thread",
      turnId: "turn",
      read: true,
      revision: 2,
    }),
    false,
  );
  assert.equal(
    unreadResult(agent, {
      threadId: "thread",
      turnId: "other",
      read: true,
      revision: 2,
    }),
    true,
  );
  assert.equal(unreadResult(agent, null), true);
  assert.equal(
    unreadResult({
      ...agent,
      readState: {
        threadId: "thread",
        turnId: "turn",
        read: true,
        revision: 2,
      },
    }),
    false,
    "omitting the read-state argument uses the agent's current receipt",
  );

  const child = makeAgent({
    id: "child",
    rootId: "lead",
    parentId: "lead",
    isLead: false,
    status: "paused",
    autoWake: false,
  });
  const snapshot = makeSnapshot([agent, child], {
    requests: [
      { id: "fixture-record", agent: "child", status: "pending", epoch: 2 },
    ],
    tasks: [{ id: "task", agent: "lead", command: "build", status: "running" }],
  });
  const indicators = chatIndicators(snapshot, () => null);
  assert.deepEqual(required(indicators.get("lead")), {
    kind: "answer",
    label: "Needs your answer",
  });
  assert.deepEqual(required(indicators.get("child")), {
    kind: "answer",
    label: "Needs your answer",
  });
  assert.equal(indicators.has("missing"), false);
  assert.deepEqual(
    required(
      chatIndicators(makeSnapshot([makeAgent({ status: "failed" })])).get(
        "lead",
      ),
    ),
    { kind: "error", label: "Failed" },
  );
  assert.deepEqual(
    required(
      chatIndicators(makeSnapshot([makeAgent({ status: "interrupted" })])).get(
        "lead",
      ),
    ),
    { kind: "paused", label: "Interrupted" },
  );
  assert.deepEqual(
    required(
      chatIndicators(
        makeSnapshot([makeAgent({ status: "approval" })], {
          requests: [{ id: "fixture-record", agent: "lead", deferred: true }],
        }),
      ).get("lead"),
    ),
    { kind: "paused", label: "Question deferred" },
  );
  assert.deepEqual(
    required(
      chatIndicators(
        makeSnapshot([
          makeAgent({ status: "idle", autoWake: false, empty: false }),
        ]),
      ).get("lead"),
    ),
    { kind: "paused", label: "Stopped" },
  );
  assert.deepEqual(
    required(
      chatIndicators(
        makeSnapshot([
          makeAgent({ status: "idle", autoWake: false, empty: true }),
        ]),
      ).get("lead"),
    ),
    { kind: "none", label: "Ready" },
  );
  assert.deepEqual(
    required(
      chatIndicators(makeSnapshot([makeAgent({ status: "paused" })])).get(
        "lead",
      ),
    ),
    { kind: "paused", label: "Stopped" },
  );
  assert.deepEqual(
    required(
      chatIndicators(
        makeSnapshot([
          makeAgent({
            status: "completed",
            readState: {
              threadId: "thread",
              turnId: "turn",
              read: true,
              revision: 1,
            },
          }),
        ]),
      ).get("lead"),
    ),
    { kind: "none", label: "Read" },
  );
  assert.deepEqual(
    required(
      chatIndicators(makeSnapshot([makeAgent({ status: "idle" })])).get("lead"),
    ),
    { kind: "none", label: "Ready" },
  );
  const withSavedReceipt = makeAgent({
    status: "completed",
    readState: { threadId: "thread", turnId: "turn", read: true, revision: 3 },
  });
  assert.deepEqual(
    required(chatIndicators(makeSnapshot([withSavedReceipt])).get("lead")),
    { kind: "none", label: "Read" },
    "the default read-state lookup uses the agent's saved receipt",
  );
  assert.equal(
    chatIndicators(
      makeSnapshot([
        agent,
        makeAgent({ id: "local", source: "registered", status: "failed" }),
      ]),
    ).has("local"),
    false,
    "only managed chats receive sidebar indicators",
  );
  assert.deepEqual(
    required(
      // @ts-expect-error A malformed snapshot without its runtime collections must remain readable.
      chatIndicators({ threads: [makeAgent()], runtime: {} }).get("lead"),
    ),
    { kind: "unread", label: "Unread result" },
    "snapshots without a requests collection still derive the completed result",
  );
  assert.deepEqual(
    required(
      chatIndicators(
        makeSnapshot([makeAgent({ status: "waiting", epoch: 2 })], {
          requests: [{ id: "fixture-record", agent: "lead", epoch: 2 }],
        }),
      ).get("lead"),
    ),
    { kind: "answer", label: "Waiting for 1 input" },
  );
  assert.deepEqual(
    required(
      chatIndicators(
        makeSnapshot([makeAgent({ status: "waiting", epoch: null })], {
          requests: [{ id: "fixture-record", agent: "lead", epoch: 9 }],
        }),
      ).get("lead"),
    ),
    { kind: "answer", label: "Waiting for 1 input" },
    "a request remains current when the agent has no epoch value",
  );
  assert.deepEqual(
    required(
      chatIndicators(
        makeSnapshot(
          [
            makeAgent({ status: "waiting", inFlight: false }),
            makeAgent({
              id: "child",
              rootId: "lead",
              parentId: "lead",
              status: "paused",
              autoWake: false,
              inFlight: false,
            }),
          ],
          {
            requests: [
              {
                id: "fixture-record",
                agent: "child",
                status: "pending",
                epoch: 2,
              },
            ],
          },
        ),
      ).get("lead"),
    ),
    { kind: "answer", label: "Needs your answer" },
    "a child's question reaches its waiting lead without claiming a lead-local wake trigger",
  );

  const currentWork = new Map<string, ChatActivity[]>([
    [
      "lead",
      [
        {
          id: "task",
          kind: "task",
          label: "Command",
          agentId: "lead",
          agentName: "Lead",
          commandTask: true,
          background: false,
          status: "running",
        },
        {
          id: "monitor",
          kind: "monitor",
          label: "Background command",
          agentId: "lead",
          agentName: "Lead",
          status: "running",
        },
      ],
    ],
  ]);
  assert.deepEqual(
    required(
      chatIndicators(
        makeSnapshot([makeAgent({ inFlight: true })]),
        undefined,
        currentWork,
      ).get("lead"),
    ),
    { kind: "working", label: "Working" },
    "a mix of task and monitor activity keeps both operational kinds visible",
  );
  const stoppedTask = new Map<string, ChatActivity[]>([
    [
      "lead",
      [
        {
          id: "task",
          kind: "task",
          label: "Command",
          agentId: "lead",
          agentName: "Lead",
          commandTask: true,
          background: false,
          status: "running",
          command: "make",
        },
      ],
    ],
  ]);
  assert.deepEqual(
    required(
      chatIndicators(
        makeSnapshot([makeAgent({ status: "completed", inFlight: false })]),
        undefined,
        stoppedTask,
      ).get("lead"),
    ),
    { kind: "working", label: "Waiting for 1 command" },
    "a stopped turn includes the wait label for its remaining command",
  );
  assert.deepEqual(
    required(
      chatIndicators(
        makeSnapshot([makeAgent({ status: "completed", inFlight: false })]),
        undefined,
        new Map<string, ChatActivity[]>([
          [
            "lead",
            [
              {
                ...required(required(stoppedTask.get("lead"))[0]),
                kind: "monitor",
                commandTask: false,
              },
            ],
          ],
        ]),
      ).get("lead"),
    ),
    { kind: "working", label: "Waiting for 1 monitor" },
    "a stopped turn includes the wait label for its remaining monitor",
  );
  assert.deepEqual(
    required(
      chatIndicators(
        makeSnapshot([makeAgent({ status: "completed", inFlight: false })]),
        undefined,
        new Map<string, ChatActivity[]>([
          [
            "lead",
            [
              {
                ...required(required(stoppedTask.get("lead"))[0]),
                commandTask: false,
                command: undefined,
              },
            ],
          ],
        ]),
      ).get("lead"),
    ),
    { kind: "working", label: "Working" },
    "a non-command task without another trigger does not create a wait label",
  );
  assert.deepEqual(
    required(
      chatIndicators(
        makeSnapshot([makeAgent({ status: "completed", inFlight: true })]),
        undefined,
        new Map<string, ChatActivity[]>([
          ["lead", required(stoppedTask.get("lead"))],
        ]),
      ).get("lead"),
    ),
    { kind: "working", label: "Working" },
  );
  assert.deepEqual(
    required(
      chatIndicators(
        makeSnapshot([agent]),
        undefined,
        new Map<string, ChatActivity[]>([["lead", []]]),
      ).get("lead"),
    ),
    { kind: "unread", label: "Unread result" },
    "an empty activity group is not active work",
  );
  assert.deepEqual(
    required(
      chatIndicators(
        makeSnapshot([makeAgent({ status: "waiting", inFlight: false })]),
      ).get("lead"),
    ),
    { kind: "none", label: "Turn ended. Send a message to continue." },
  );
});

it("orders every indicator kind once by operator urgency", () => {
  assert.deepEqual(CHAT_INDICATOR_URGENCY, [
    "answer",
    "error",
    "working",
    "unread",
    "paused",
    "none",
  ]);
});
