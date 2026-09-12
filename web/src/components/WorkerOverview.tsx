import { Button, UnstyledButton } from "@mantine/core";
import { ChevronRight } from "lucide-react";
import { useState } from "react";
import { save, saved } from "../api";
import { nativeErrorView } from "../nativeErrors";
import { shortModel } from "./ExecutionSettings";
import { agentErrorLabel, statusLabel, type Agent, type Json } from "../types";

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

function WorkerExcerpt({
  agentId,
  label,
  text,
  truncated,
  open,
}: {
  agentId: string;
  label: string;
  text: string;
  truncated?: boolean;
  open: () => void;
}) {
  const storageKey = "codex-worker-disclosures";
  const key = `${agentId}:${label}`;
  const [expanded, setExpanded] = useState(
    () => saved<Record<string, boolean>>(storageKey, {})[key] || false,
  );
  return (
    <details
      className="worker-excerpt"
      open={expanded}
      onToggle={(event) => {
        if (event.target !== event.currentTarget) return;
        const value = event.currentTarget.open;
        setExpanded(value);
        const prior = saved<Record<string, boolean>>(storageKey, {});
        save(
          storageKey,
          Object.fromEntries([
            ...Object.entries(prior)
              .filter(([id]) => id !== key)
              .slice(-499),
            [key, value],
          ]),
        );
      }}
    >
      <summary>
        <span className="worker-excerpt-label">
          {label}
          <ChevronRight size={11} aria-hidden="true" />
        </span>
        <span className="worker-excerpt-preview">{text}</span>
      </summary>
      <div className="worker-excerpt-full">
        <p>{text}</p>
        {truncated && (
          <Button variant="subtle" size="compact-xs" onClick={open}>
            Continue in chat
          </Button>
        )}
      </div>
    </details>
  );
}

export default function WorkerCard({
  agent,
  selected,
  awaitingAnswer,
  deferred,
  open,
}: {
  agent: Agent;
  selected: boolean;
  awaitingAnswer: boolean;
  deferred: boolean;
  open: () => void;
}) {
  const overview = agent.overview;
  const errorView = nativeErrorView(agent.error);
  const error = agent.error ? agentErrorLabel(agent) : "";
  const errorSummary =
    /worktree[\s\S]*add[\s\S]*(?:exit status|exit code|failed)/i.test(error)
      ? "Could not prepare the project folder."
      : error.length > 160 ||
          /[\r\n]/.test(error) ||
          /^Command [\["']/.test(error)
        ? "The agent stopped with an error."
        : error;
  return (
    <div className={`worker-entry ${selected ? "selected" : ""}`}>
      <UnstyledButton
        className="worker"
        data-worker={agent.id}
        aria-current={selected ? "page" : undefined}
        onClick={open}
      >
        <span
          className={`dot ${awaitingAnswer ? "approval" : deferred && agent.status === "approval" ? "waiting" : agent.status}`}
        />
        <span className="worker-text">
          <strong>{agent.name}</strong>
          <span className="worker-meta">
            <small>
              {awaitingAnswer
                ? "Needs your answer"
                : deferred && agent.status === "approval"
                  ? "Question deferred"
                  : agent.status === "starting" &&
                      (agent.startAttempt?.prepareError ||
                        agent.startAttempt?.responseError)
                    ? "Waiting for Codex"
                    : statusLabel(agent.status)}
            </small>
            <span
              className="worker-model-summary"
              title={[
                agent.model,
                agent.effort || "default reasoning",
                agent.fastMode ? "Fast" : "Standard",
              ].join(" · ")}
            >
              {shortModel(agent.model)}
              {agent.fastMode ? " · Fast" : ""}
            </span>
          </span>
          {Boolean(agent.error) && (
            <span className="worker-error">{errorSummary}</span>
          )}
        </span>
      </UnstyledButton>
      {Boolean(agent.error) && (
        <details className="worker-error-details">
          <summary>Error details</summary>
          <pre>{errorView.details || errorView.message}</pre>
        </details>
      )}
      {overview?.task ? (
        <WorkerExcerpt
          agentId={agent.id}
          label="Task"
          text={overview.task}
          truncated={overview.taskTruncated}
          open={open}
        />
      ) : (
        <p className="worker-missing">Task details unavailable</p>
      )}
      {overview?.result ? (
        <WorkerExcerpt
          agentId={agent.id}
          label="Last report"
          text={overview.result}
          truncated={overview.resultTruncated}
          open={open}
        />
      ) : agent.status === "completed" ? (
        <p className="worker-missing">No final report available</p>
      ) : null}
    </div>
  );
}
