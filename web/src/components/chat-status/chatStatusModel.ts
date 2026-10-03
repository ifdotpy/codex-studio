import type { Agent, Snapshot } from "../../types";

export type ChatIndicatorKind =
  | "working"
  | "unread"
  | "answer"
  | "error"
  | "paused"
  | "none";
export interface ChatIndicator {
  kind: ChatIndicatorKind;
  label: string;
}
export interface ReadState {
  threadId: string;
  turnId: string;
  read: boolean;
  revision: number;
}

export interface ChatActivity {
  id: string;
  kind: "task" | "monitor" | "agent";
  agentId: string;
  agentName: string;
  label: string;
  command?: string;
  created?: number;
  status: string;
  // Monitors and commands that outlive their turn. Only these appear above the chat.
  background?: boolean;
  commandTask?: boolean;
}

export const endedWaitLabel = "Turn ended. Send a message to continue.";

export interface ChatWaitState {
  live: boolean;
  label: string;
  agents: Agent[];
  commands: ChatActivity[];
  monitors: ChatActivity[];
  event?: string;
  inputs: number;
}

// A saved waiting status alone cannot promise another turn.
export function chatWaitState(
  data: Snapshot,
  agent: Agent,
  activities = chatActivities(data),
): ChatWaitState {
  const agents = data.threads.filter(
    (child) =>
      child.source === "managed" &&
      child.parentId === agent.id &&
      (child.inFlight ||
        ["queued", "starting", "running", "approval"].includes(child.status)),
  );
  const owned = (activities.get(agent.id) || []).filter(
    (activity) => activity.agentId === agent.id,
  );
  const commands = owned.filter(
    (activity) =>
      activity.kind === "task" &&
      activity.commandTask &&
      (activity.background || !agent.inFlight),
  );
  const monitors = owned.filter((activity) => activity.kind === "monitor");
  const inputs = (data.runtime.requests || []).filter(
    (request) =>
      request.agent === agent.id &&
      (!request.status || request.status === "pending") &&
      !request.deferred &&
      (request.epoch == null ||
        agent.epoch == null ||
        request.epoch === agent.epoch),
  ).length;
  const event = agent.parkedEvent || undefined;
  const count = (value: number, name: string) =>
    value ? `${value} ${name}${value === 1 ? "" : "s"}` : "";
  const reasons = [
    count(agents.length, "agent"),
    count(commands.length, "command"),
    count(monitors.length, "monitor"),
    event ? `event ${event}` : "",
    count(inputs, "input"),
  ].filter(Boolean);
  const label = reasons.length
    ? `Waiting for ${reasons.slice(0, -1).join(", ")}${reasons.length > 1 ? " and " : ""}${reasons.at(-1)}`
    : endedWaitLabel;
  return {
    live: reasons.length > 0,
    label,
    agents,
    commands,
    monitors,
    event,
    inputs,
  };
}

export function backgroundActivities(activities: ChatActivity[]) {
  return activities.filter((activity) => activity.background);
}

// The visible reasons and the spinner use the same activity records.
export function chatActivities(data: Snapshot): Map<string, ChatActivity[]> {
  const agents = data.threads.filter((agent) => agent.source === "managed");
  const byId = new Map(agents.map((agent) => [agent.id, agent]));
  const result = new Map<string, ChatActivity[]>();
  const concrete = new Set<string>();
  const add = (agent: Agent, activity: ChatActivity) => {
    for (const id of new Set([agent.id, agent.rootId].filter(Boolean))) {
      if (!byId.has(id!)) continue;
      result.set(id!, [...(result.get(id!) || []), activity]);
    }
  };
  for (const [kind, entries] of [
    ["task", data.runtime.tasks || []],
    ["monitor", data.runtime.monitors || []],
  ] as const) {
    for (const record of entries) {
      const entry = record as typeof record & {
        epoch?: number;
        type?: string;
        turnId?: string;
      };
      const agent = byId.get(entry.agent);
      if (
        !agent ||
        (entry.epoch != null &&
          agent.epoch != null &&
          entry.epoch !== agent.epoch) ||
        !["starting", "running", "approval"].includes(entry.status)
      )
        continue;
      concrete.add(agent.id);
      const background =
        kind === "monitor" ||
        agent.inFlight === false ||
        !!(entry.turnId && agent.turnId && agent.turnId !== entry.turnId);
      add(agent, {
        id: entry.id,
        kind,
        agentId: agent.id,
        agentName: agent.name,
        label:
          kind === "monitor" || (background && entry.command)
            ? "Background command"
            : entry.command
              ? "Command"
              : "Tool call",
        command: entry.command || entry.query || entry.name || entry.type,
        created: entry.created,
        status: entry.status,
        background,
        commandTask:
          kind === "task" && (!!entry.command || entry.kind === "command"),
      });
    }
  }
  for (const agent of agents) {
    if (concrete.has(agent.id)) continue;
    if (
      agent.inFlight ||
      (agent.autoWake !== false &&
        ["queued", "starting", "running"].includes(agent.status))
    )
      add(agent, {
        id: agent.id,
        kind: "agent",
        agentId: agent.id,
        agentName: agent.name,
        label:
          agent.status === "queued"
            ? "Waiting to start"
            : agent.status === "starting"
              ? "Starting"
              : "Working",
        status: agent.status,
      });
  }
  for (const entries of result.values())
    entries.sort(
      (a, b) =>
        (a.created ?? Infinity) - (b.created ?? Infinity) ||
        a.id.localeCompare(b.id),
    );
  return result;
}
export function hasCompletedResult(agent: Agent) {
  return !!(
    agent.threadId &&
    agent.lastCompletedTurn &&
    agent.lastCompletedTurnStatus === "completed"
  );
}
export function unreadResult(
  agent: Agent,
  readState: ReadState | null = agent.readState || null,
) {
  return (
    hasCompletedResult(agent) &&
    !(
      readState?.read === true &&
      readState.threadId === agent.threadId &&
      readState.turnId === agent.lastCompletedTurn
    )
  );
}

// Operational state and read receipts are separate: a question cannot disappear
// merely because its conversation was opened or a background command is active.
export function chatIndicators(
  data: Snapshot,
  readStateFor: (agent: Agent) => ReadState | null = (agent) =>
    agent.readState || null,
  activities = chatActivities(data),
): Map<string, ChatIndicator> {
  const agents = data.threads.filter((agent) => agent.source === "managed");
  const byId = new Map(agents.map((agent) => [agent.id, agent]));
  const answers = new Set<string>(),
    deferred = new Set<string>(),
    monitoring = new Set<string>(),
    tasks = new Set<string>();
  const includeLead = (set: Set<string>, id: string) => {
    set.add(id);
    const agent = byId.get(id);
    if (agent?.rootId && byId.has(agent.rootId)) set.add(agent.rootId);
  };
  for (const request of data.runtime.requests || []) {
    if (request.status && request.status !== "pending") continue;
    const agent = byId.get(request.agent);
    if (
      !agent ||
      (request.epoch != null &&
        agent.epoch != null &&
        request.epoch !== agent.epoch)
    )
      continue;
    if (request.deferred) deferred.add(agent.id);
    else includeLead(answers, agent.id);
  }
  for (const [id, entries] of activities) {
    if (entries.some((entry) => entry.kind === "monitor")) monitoring.add(id);
    if (entries.some((entry) => entry.kind !== "monitor")) tasks.add(id);
  }
  return new Map(
    agents.map((agent) => {
      let indicator: ChatIndicator;
      const waiting = ["waiting", "parked"].includes(agent.status);
      const wait =
        waiting ||
        (!agent.inFlight && (tasks.has(agent.id) || monitoring.has(agent.id)))
          ? chatWaitState(data, agent, activities)
          : undefined;
      if (answers.has(agent.id))
        indicator = {
          kind: "answer",
          label: waiting && wait?.live ? wait.label : "Needs your answer",
        };
      else if (waiting)
        indicator = {
          kind: wait?.live ? "working" : "none",
          label: wait?.label || endedWaitLabel,
        };
      else if (tasks.has(agent.id) || monitoring.has(agent.id))
        indicator = {
          kind: "working",
          label: !agent.inFlight && wait?.live ? wait.label : "Working",
        };
      else if (agent.status === "failed")
        indicator = { kind: "error", label: "Failed" };
      else if (
        ["paused", "interrupted"].includes(agent.status) ||
        (agent.autoWake === false && !agent.empty)
      )
        indicator = {
          kind: "paused",
          label: agent.status === "interrupted" ? "Interrupted" : "Stopped",
        };
      else if (deferred.has(agent.id) && agent.status === "approval")
        indicator = { kind: "paused", label: "Question deferred" };
      else if (
        agent.status === "completed" &&
        unreadResult(agent, readStateFor(agent))
      )
        indicator = { kind: "unread", label: "Unread result" };
      else
        indicator = {
          kind: "none",
          label: agent.status === "completed" ? "Read" : "Ready",
        };
      return [agent.id, indicator];
    }),
  );
}
