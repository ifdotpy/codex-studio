import {
  complaintNeedsUserResponse,
  type Agent,
  type Snapshot,
} from "../types";

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
  for (const complaint of data.runtime.complaints || []) {
    if (
      complaint.needsResponse &&
      complaintNeedsUserResponse(complaint) &&
      !["resolved", "declined"].includes(complaint.status)
    )
      answers.add(complaint.leadId);
  }
  for (const task of data.runtime.userTasks || []) {
    if (["open", "review"].includes(task.status))
      includeLead(answers, task.agent || task.rootId);
  }
  const currentEpoch = (entry: { agent?: string; epoch?: number }) => {
    const agent = byId.get(entry.agent || "");
    return (
      !!agent &&
      (entry.epoch == null ||
        agent.epoch == null ||
        entry.epoch === agent.epoch)
    );
  };
  for (const monitor of data.runtime.monitors || []) {
    if (
      currentEpoch(monitor) &&
      !monitor.panelFeed &&
      ["starting", "running", "approval"].includes(monitor.status)
    )
      includeLead(monitoring, monitor.agent);
  }
  for (const task of data.runtime.tasks || []) {
    if (
      currentEpoch(task) &&
      ["starting", "running", "approval"].includes(task.status)
    )
      includeLead(tasks, task.agent);
  }
  for (const agent of agents) {
    if (
      agent.inFlight ||
      (agent.autoWake !== false &&
        ["queued", "starting", "running", "waiting"].includes(agent.status))
    )
      includeLead(tasks, agent.id);
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
