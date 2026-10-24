import { WORKER_TURN_LIMIT_REASON } from "../../generated/sharedValues";
import { createContext, type ReactNode } from "react";
import { useWorkerModels, type WorkerModelInfo } from "./WorkerModelPicker";

export const TeamModelCatalog = createContext<WorkerModelInfo[]>([]);

export function TeamModels({
  accountKey,
  children,
}: {
  accountKey: string;
  children: ReactNode;
}) {
  const catalog = useWorkerModels(accountKey, true, true);
  return (
    <TeamModelCatalog.Provider value={catalog.models}>
      {children}
    </TeamModelCatalog.Provider>
  );
}

import type { Agent, Snapshot } from "../../types";
import { Users } from "lucide-react";
import { TeamStateIcon } from "./TeamFacts";
import { diskFullHold } from "../../capacityRetry";
import {
  CHAT_INDICATOR_URGENCY,
  type ChatIndicator,
  type ChatIndicatorKind,
} from "../chat-status/chatStatusModel";

type Request = NonNullable<Snapshot["runtime"]>["requests"][number];

export function awaitingAnswerIds(requests: Request[]) {
  return new Set(
    requests
      .filter(
        (request) =>
          !request.deferred &&
          (request.status === "pending" || !request.status),
      )
      .flatMap((request) =>
        typeof request.agent === "string" ? [request.agent] : [],
      ),
  );
}

export function workerState(
  agent: Agent,
  answers: Set<string>,
  deferred?: Set<string>,
) {
  const status = agent.status ?? "";
  if (agent.terminalReason === WORKER_TURN_LIMIT_REASON) return "stopped";
  if (agent.stall) return "stalled";
  if (answers.has(agent.id)) return "answer";
  if (status === "approval")
    return deferred?.has(agent.id) ? "waiting" : "answer";
  if (["failed", "interrupted"].includes(status))
    return diskFullHold(agent) ? "waiting" : "attention";
  if (["running", "starting"].includes(status)) return "working";
  if (status === "parked") return "waiting";
  if (status === "completed") return "completed";
  // A paused worker was stopped. It does not wait for input or delivery.
  if (status === "paused") return "stopped";
  return "waiting";
}

export const teamFilterValue = (state: string) =>
  state === "working" ? "active" : state === "error" ? "attention" : state;
export function TeamGroupLabel({
  kind,
  label,
  count,
}: {
  kind: ChatIndicatorKind | "completed";
  label: string;
  count: number;
}) {
  return (
    <span className="team-group-label">
      <TeamStateIcon kind={kind === "completed" ? "unread" : kind} />
      <span>{label}</span>
      <span className="team-group-count">{count}</span>
    </span>
  );
}
export function TeamSummary({
  workers,
  indicators,
  selected,
  onFilter,
}: {
  workers: Agent[];
  indicators: Map<string, ChatIndicator>;
  selected: string;
  onFilter: (value: string) => void;
}) {
  const { groups, completed } = teamDispatchGroups(workers, indicators);
  const counts = [
    { state: "all", label: "All", count: workers.length },
    ...groups.map((group) => ({
      state: group.kind,
      label: group.name,
      count: group.workers.length,
    })),
    { state: "completed", label: "Finished", count: completed.length },
  ];
  return (
    <div className="team-overview" aria-label="Team status summary">
      <div
        className="team-summary-filters"
        role="group"
        aria-label="Filter subagents"
      >
        {counts
          .filter((row) => row.count > 0 || row.state === "all")
          .map((row) => {
            const value = teamFilterValue(row.state);
            return (
              <button
                key={row.state}
                type="button"
                data-team-count={row.state}
                aria-label={row.state === "working" ? "Active" : row.label}
                aria-pressed={selected === value}
                title={`Show ${row.label.toLowerCase()} agents`}
                onClick={() => onFilter(selected === value ? "all" : value)}
              >
                {row.state === "all" ? (
                  <Users className="team-state-icon" aria-hidden="true" />
                ) : (
                  <TeamStateIcon
                    kind={
                      row.state === "completed"
                        ? "unread"
                        : (row.state as ChatIndicatorKind)
                    }
                  />
                )}
                <span>{row.label}</span>
                <strong>{row.count}</strong>
              </button>
            );
          })}
      </div>
    </div>
  );
}

export function filterTeamWorkers(
  workers: Agent[],
  indicators: Map<string, ChatIndicator>,
  filter: string,
) {
  if (filter === "all") return workers;
  const { groups, completed } = teamDispatchGroups(workers, indicators);
  return filter === "completed"
    ? completed
    : groups.find((group) => teamFilterValue(group.kind) === filter)?.workers ||
        [];
}

/** Indentation follows the server-owned parent relation, including filtered parents. */
export function workerDepth(
  agent: Agent,
  agents: Map<string, Agent>,
  implicitRootId?: string,
) {
  const seen = new Set([agent.id]);
  let parentId = agent.parentId;
  let depth = 0;
  while (parentId && parentId !== implicitRootId && !seen.has(parentId)) {
    seen.add(parentId);
    depth++;
    parentId = agents.get(parentId)?.parentId;
  }
  return depth;
}

const teamIndicatorLabels: Record<ChatIndicatorKind, string> = {
  answer: "Needs you",
  error: "Failed",
  working: "Working",
  unread: "New results",
  paused: "Stopped",
  none: "Waiting",
};
/** Display groups consume the same state owner as each rail. Completed results
 * remain in the native Finished disclosure unless they need operator action. */
export function teamDispatchGroups(
  workers: Agent[],
  indicators: Map<string, ChatIndicator>,
) {
  // A turn-limited worker stopped before finishing; it stays visible as Stopped.
  const turnLimited = (agent: Agent) =>
    agent.terminalReason === WORKER_TURN_LIMIT_REASON;
  const kindOf = (agent: Agent) => {
    const kind = indicators.get(agent.id)?.kind || "none";
    return turnLimited(agent) && !["answer", "error", "working"].includes(kind)
      ? "paused"
      : kind;
  };
  const finished = (agent: Agent) =>
    agent.status === "completed" &&
    !agent.stall &&
    !turnLimited(agent) &&
    !["answer", "error", "working"].includes(kindOf(agent));
  return {
    groups: CHAT_INDICATOR_URGENCY.map((kind) => ({
      kind,
      name: teamIndicatorLabels[kind],
      workers: workers.filter(
        (agent) => !finished(agent) && kindOf(agent) === kind,
      ),
    })),
    completed: workers.filter(finished),
  };
}

/** Keep parents before descendants within each urgency lane. */
export function orderWorkerHierarchy(workers: Agent[]) {
  const ids = new Set(workers.map((agent) => agent.id));
  const children = new Map<string, Agent[]>();
  for (const agent of workers) {
    if (!agent.parentId || !ids.has(agent.parentId)) continue;
    const siblings = children.get(agent.parentId) || [];
    siblings.push(agent);
    children.set(agent.parentId, siblings);
  }
  const ordered: Agent[] = [];
  const seen = new Set<string>();
  const visit = (roots: Agent[]) => {
    const pending = roots.slice().reverse();
    while (pending.length) {
      const agent = pending.pop()!;
      if (seen.has(agent.id)) continue;
      seen.add(agent.id);
      ordered.push(agent);
      pending.push(...(children.get(agent.id) || []).slice().reverse());
    }
  };
  visit(workers.filter((agent) => !agent.parentId || !ids.has(agent.parentId)));
  visit(workers); // Broken parent cycles remain visible and cannot loop the renderer.
  return ordered;
}
