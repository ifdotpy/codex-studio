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

export const TEAM_STATES = [
  ["answer", "Need you"],
  ["attention", "Failed"],
  ["stalled", "Waiting for lead"],
  ["working", "Working"],
  ["waiting", "Waiting"],
  ["stopped", "Stopped"],
  ["completed", "Finished"],
] as const;

// Operator questions and faults precede running work.
export const TEAM_PANEL_STATES = TEAM_STATES;

export function TeamSummary({
  workers,
  answers,
  deferred,
}: {
  workers: Agent[];
  answers: Set<string>;
  deferred: Set<string>;
}) {
  const count = (state: string) =>
    workers.filter((agent) => workerState(agent, answers, deferred) === state)
      .length;
  return (
    <div className="team-overview" aria-label="Team status summary">
      <dl>
        {TEAM_STATES.filter(([state]) => count(state) > 0).map(
          ([state, label]) => (
            <div key={state} data-team-count={state}>
              <dt>{label}</dt>
              <dd>{count(state)}</dd>
            </div>
          ),
        )}
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
