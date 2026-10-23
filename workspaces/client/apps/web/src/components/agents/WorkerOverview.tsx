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

export function TeamSummary({
  workers,
  indicators,
}: {
  workers: Agent[];
  indicators: Map<string, ChatIndicator>;
}) {
  const { groups, completed } = teamDispatchGroups(workers, indicators);
  const counts = [
    ...groups.map((group) => ({
      state: group.kind,
      label: group.name,
      count: group.workers.length,
    })),
    { state: "completed", label: "Finished", count: completed.length },
  ];
  return (
    <div className="team-overview" aria-label="Team status summary">
      <dl>
        {counts
          .filter((row) => row.count > 0)
          .map((row) => (
            <div key={row.state} data-team-count={row.state}>
              <dt>{row.label}</dt>
              <dd>{row.count}</dd>
            </div>
          ))}
      </dl>
    </div>
  );
}

/** Indentation follows the server-owned parent relation, including filtered parents. */
export function workerDepth(agent: Agent, agents: Map<string, Agent>) {
  const seen = new Set([agent.id]);
  let parentId = agent.parentId;
  let depth = 0;
  while (parentId && !seen.has(parentId)) {
    seen.add(parentId);
    depth++;
    parentId = agents.get(parentId)?.parentId;
  }
  return depth;
}

const teamIndicatorLabels: Record<ChatIndicatorKind, string> = {
  answer: "Need you",
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
  const finished = (agent: Agent) =>
    agent.status === "completed" &&
    !agent.stall &&
    !["answer", "error", "working"].includes(
      indicators.get(agent.id)?.kind || "none",
    );
  return {
    groups: CHAT_INDICATOR_URGENCY.map((kind) => ({
      kind,
      name: teamIndicatorLabels[kind],
      workers: workers.filter(
        (agent) =>
          !finished(agent) &&
          (indicators.get(agent.id)?.kind || "none") === kind,
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
