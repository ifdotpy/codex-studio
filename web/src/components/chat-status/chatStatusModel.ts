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
  const runtime = data.runtime;
  const agents = data.threads.filter(
    (child) =>
      child.source === "managed" &&
      child.parentId === agent.id &&
      (child.inFlight ||
        ["queued", "starting", "running", "approval"].includes(
          child.status || "",
        )),
  );
  const owned = activities
    .get(agent.id)
    ?.filter((activity) => activity.agentId === agent.id);
  const commands =
    owned?.filter(
      (activity) =>
        activity.kind === "task" &&
        activity.commandTask &&
        (activity.background || !agent.inFlight),
    ) ?? [];
  const monitors =
    owned?.filter((activity) => activity.kind === "monitor") ?? [];
  const inputs =
    runtime?.requests?.filter(
      (request) =>
        request.agent === agent.id &&
        (!request.status || request.status === "pending") &&
        !request.deferred &&
        (!("epoch" in request && typeof request.epoch === "number") ||
          agent.epoch == null ||
          request.epoch === agent.epoch),
    ).length ?? 0;
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
  const runtime = data.runtime;
  const agents = data.threads.filter((agent) => agent.source === "managed");
  const byId = new Map(agents.map((agent) => [agent.id, agent]));
  const result = new Map<string, ChatActivity[]>();
  const concrete = new Set<string>();
  const add = (agent: Agent, activity: ChatActivity) => {
    const ids = new Set([agent.id]);
    if (agent.rootId && byId.has(agent.rootId)) ids.add(agent.rootId);
    for (const id of ids) {
      result.set(id, [...(result.get(id) || []), activity]);
    }
  };
  const entries = runtime
    ? [
        ...(runtime.tasks || []).map((record) => ({
          kind: "task" as const,
          record,
        })),
        ...(runtime.monitors || []).map((record) => ({
          kind: "monitor" as const,
          record,
        })),
      ]
    : [];
  for (const { kind, record } of entries) {
    const agent = byId.get(record.agent || "");
    if (
      !agent ||
      ("epoch" in record &&
        typeof record.epoch === "number" &&
        agent.epoch != null &&
        record.epoch !== agent.epoch) ||
      !["starting", "running", "approval"].includes(record.status || "")
    )
      continue;
    concrete.add(agent.id);
    const background =
      kind === "monitor" ||
      agent.inFlight === false ||
      !!(
        "turnId" in record &&
        record.turnId &&
        agent.turnId &&
        agent.turnId !== record.turnId
      );
    const type =
      "type" in record && typeof record.type === "string"
        ? record.type
        : undefined;
    const command =
      record.command ||
      (kind === "task" ? record.query : undefined) ||
      record.name ||
      type ||
      (kind === "task" ? record.kind : undefined) ||
      undefined;
    add(agent, {
      id: record.id,
      kind,
      agentId: agent.id,
      agentName: agent.name || "Agent",
      label:
        kind === "monitor" || (background && record.command)
          ? "Background command"
          : record.command
            ? "Command"
            : "Tool call",
      command,
      created: record.created ?? undefined,
      status: record.status || "",
      background,
      commandTask:
        kind === "task" && (!!record.command || record.kind === "command"),
    });
  }
  for (const agent of agents) {
    if (concrete.has(agent.id)) continue;
    if (
      agent.inFlight ||
      (agent.autoWake !== false &&
        ["queued", "starting", "running"].includes(agent.status || ""))
    )
      add(agent, {
        id: agent.id,
        kind: "agent",
        agentId: agent.id,
        agentName: agent.name || "Agent",
        label:
          agent.status === "queued"
            ? "Waiting to start"
            : agent.status === "starting"
              ? "Starting"
              : "Working",
        status: agent.status || "",
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
  readState: ReadState | null = savedReadState(agent),
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

function savedReadState(agent: Agent): ReadState | null {
  if (!("readState" in agent)) return null;
  const value = agent.readState;
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const threadId = "threadId" in value ? value.threadId : undefined;
  const turnId = "turnId" in value ? value.turnId : undefined;
  const read = "read" in value ? value.read : undefined;
  const revision = "revision" in value ? value.revision : undefined;
  return typeof threadId === "string" &&
    typeof turnId === "string" &&
    typeof read === "boolean" &&
    typeof revision === "number"
    ? { threadId, turnId, read, revision }
    : null;
}

// Operational state and read receipts are separate: a question cannot disappear
// merely because its conversation was opened or a background command is active.
export function chatIndicators(
  data: Snapshot,
  readStateFor: (agent: Agent) => ReadState | null = savedReadState,
  activities = chatActivities(data),
): Map<string, ChatIndicator> {
  const agents = data.threads.filter((agent) => agent.source === "managed");
  const byId = new Map(agents.map((agent) => [agent.id, agent]));
  const answers = new Set<string>(),
    deferred = new Set<string>(),
    active = new Set<string>();
  const includeLead = (set: Set<string>, id: string) => {
    set.add(id);
    const agent = byId.get(id)!;
    if (agent.rootId) {
      const root = byId.get(agent.rootId);
      if (root) set.add(root.id);
    }
  };
  const requests = data.runtime?.requests;
  if (requests) {
    for (const request of requests) {
      if (request.status && request.status !== "pending") continue;
      const agent = byId.get(request.agent || "");
      if (
        !agent ||
        ("epoch" in request &&
          typeof request.epoch === "number" &&
          agent.epoch != null &&
          request.epoch !== agent.epoch)
      )
        continue;
      if (request.deferred) deferred.add(agent.id);
      else includeLead(answers, agent.id);
    }
  }
  for (const [id, entries] of activities) {
    if (entries.length) active.add(id);
  }
  return new Map(
    agents.map((agent) => {
      let indicator: ChatIndicator;
      const waiting = ["waiting", "parked"].includes(agent.status || "");
      const getWait = () => chatWaitState(data, agent, activities);
      if (answers.has(agent.id)) {
        let label = "Needs your answer";
        if (waiting) {
          const wait = getWait();
          if (wait.live) label = wait.label;
        }
        indicator = {
          kind: "answer",
          label,
        };
      } else if (waiting) {
        const wait = getWait();
        indicator = {
          kind: wait.live ? "working" : "none",
          label: wait.label || endedWaitLabel,
        };
      } else if (active.has(agent.id)) {
        let label = "Working";
        if (!agent.inFlight) {
          const wait = getWait();
          if (wait.live) label = wait.label;
        }
        indicator = {
          kind: "working",
          label,
        };
      } else if (agent.status === "failed")
        indicator = { kind: "error", label: "Failed" };
      else if (
        ["paused", "interrupted"].includes(agent.status || "") ||
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
        unreadResult(agent, readStateFor(agent) ?? null)
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
