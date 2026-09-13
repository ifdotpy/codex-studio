import type { Agent, Snapshot } from "../types";

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
        panelFeed?: boolean;
        type?: string;
      };
      const agent = byId.get(entry.agent);
      if (
        !agent ||
        (entry.epoch != null &&
          agent.epoch != null &&
          entry.epoch !== agent.epoch) ||
        (kind === "monitor" && entry.panelFeed) ||
        !["starting", "running", "approval"].includes(entry.status)
      )
        continue;
      concrete.add(agent.id);
      add(agent, {
        id: entry.id,
        kind,
        agentId: agent.id,
        agentName: agent.name,
        label:
          kind === "monitor"
            ? "Background command"
            : entry.command
              ? "Command"
              : "Tool call",
        command: entry.command || entry.query || entry.name || entry.type,
        created: entry.created,
        status: entry.status,
      });
    }
  }
  for (const agent of agents) {
    if (concrete.has(agent.id)) continue;
    if (
      agent.inFlight ||
      (agent.autoWake !== false &&
        ["queued", "starting", "running", "waiting"].includes(agent.status))
    )
      add(agent, {
        id: agent.id,
        kind: "agent",
        agentId: agent.id,
        agentName: agent.name,
        label:
          agent.status === "waiting"
            ? "Waiting for results"
            : agent.status === "queued"
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
      if (answers.has(agent.id))
        indicator = { kind: "answer", label: "Needs your answer" };
      else if (tasks.has(agent.id) || monitoring.has(agent.id))
        indicator = {
          kind: "working",
          label:
            monitoring.has(agent.id) && !agent.inFlight
              ? "Waiting for a monitor"
              : agent.status === "waiting"
                ? "Waiting for results"
                : "Working",
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
