import type { Agent, Snapshot } from "../../types";

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
  if (answers.has(agent.id)) return "answer";
  if (status === "approval")
    return deferred?.has(agent.id) ? "waiting" : "answer";
  if (["failed", "interrupted"].includes(status)) return "attention";
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
