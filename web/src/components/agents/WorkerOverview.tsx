import type { Agent, Json } from "../../types";
import type { WorktreeDiskSnapshot } from "../../hooks/useWorktreeDisk";
import { TeamDiskTotal } from "../WorktreeDisk";

export function awaitingAnswerIds(requests: Json[]) {
  return new Set(
    requests
      .filter(
        (request) =>
          !request.deferred &&
          (request.status === "pending" || !request.status),
      )
      .map((request) => request.agent as string),
  );
}

export function workerState(
  agent: Agent,
  answers: Set<string>,
  deferred?: Set<string>,
) {
  if (answers.has(agent.id)) return "answer";
  if (agent.status === "approval")
    return deferred?.has(agent.id) ? "waiting" : "answer";
  if (["failed", "interrupted"].includes(agent.status)) return "attention";
  if (["running", "starting"].includes(agent.status)) return "working";
  if (agent.status === "parked") return "waiting";
  if (agent.status === "completed") return "completed";
  // A paused worker was stopped. It does not wait for input or delivery.
  if (agent.status === "paused") return "stopped";
  return "waiting";
}

export const TEAM_STATES = [
  ["answer", "Need you"],
  ["attention", "Failed"],
  ["working", "Working"],
  ["waiting", "Waiting"],
  ["stopped", "Stopped"],
  ["completed", "Finished"],
] as const;

// Keep active work at the top of the panel in compact and grouped layouts.
export const TEAM_PANEL_STATES = [
  ...TEAM_STATES.filter(([state]) => state === "working"),
  ...TEAM_STATES.filter(([state]) => state !== "working"),
];

export function TeamSummary({
  workers,
  answers,
  deferred,
  disk,
}: {
  workers: Agent[];
  answers: Set<string>;
  deferred: Set<string>;
  disk?: WorktreeDiskSnapshot;
}) {
  const count = (state: string) =>
    workers.filter((agent) => workerState(agent, answers, deferred) === state)
      .length;
  const working = count("working");
  const answer = count("answer");
  // One sentence for the current activity; each state then appears once.
  const headline = [
    working
      ? `${working} ${working === 1 ? "subagent is" : "subagents are"} working.`
      : "No subagent is working.",
    answer > 0 && `${answer} ${answer === 1 ? "needs" : "need"} your answer.`,
  ]
    .filter(Boolean)
    .join(" ");
  return (
    <div className="team-overview" aria-label="Team status summary">
      <p className="team-headline">{headline}</p>
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
      <TeamDiskTotal workers={workers} disk={disk} />
    </div>
  );
}
