import type { Agent, Json } from "../../types";

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
  if (agent.status === "completed") return "completed";
  return "waiting";
}

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
        {[
          ["working", "Working"],
          ["answer", "Need you"],
          ["completed", "Finished"],
        ].map(([state, label]) => (
          <div
            key={state}
            data-team-count={state}
            data-active={count(state) > 0}
          >
            <dt>{label}</dt>
            <dd>{count(state)}</dd>
          </div>
        ))}
      </dl>
      <p aria-hidden={count("waiting") === 0 && count("attention") === 0}>
        {[
          count("waiting") > 0 && `${count("waiting")} waiting`,
          count("attention") > 0 && `${count("attention")} need attention`,
        ]
          .filter(Boolean)
          .join(" · ")}
      </p>
    </div>
  );
}
