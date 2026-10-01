import { ActionIcon, Button, Menu, UnstyledButton } from "@mantine/core";
import { ChevronRight, MoreHorizontal, Trash2 } from "lucide-react";
import { useState } from "react";
import { save, saved } from "../api";
import { nativeErrorView } from "../nativeErrors";
import { shortModel } from "./ExecutionSettings";
import { agentErrorLabel, nativeReleaseLabel, statusLabel, type Agent, type Json } from "../types";
import type { WorktreeDiskSnapshot } from "../hooks/useWorktreeDisk";
import ChatStatus from "./ChatStatus";
import type { ChatIndicator } from "./chatStatusModel";
import { TeamDiskTotal, WorkerDiskLabel } from "./WorktreeDisk";

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
  disk,
  selected,
  awaitingAnswer,
  deferred,
  open,
  indicator,
  remove,
}: {
  agent: Agent;
  disk?: WorktreeDiskSnapshot["workers"][string];
  selected: boolean;
  awaitingAnswer: boolean;
  deferred: boolean;
  open: () => void;
  remove?: () => void;
  indicator?: ChatIndicator;
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
      <div className="worker-heading">
        <UnstyledButton
          className="worker"
          data-worker={agent.id}
          aria-current={selected ? "page" : undefined}
          onClick={open}
        >
          <ChatStatus
            status={indicator}
            provider={agent.provider}
            model={agent.model}
          />
          <span className="worker-text">
            <strong>{agent.name}</strong>
            <span className="worker-meta">
              <small>
                {indicator?.kind === "answer" ||
                ["waiting", "parked"].includes(agent.status) ||
                (indicator?.kind === "working" && !agent.inFlight)
                  ? indicator?.label || statusLabel(agent.status)
                  : awaitingAnswer
                    ? "Needs your answer"
                    : deferred && agent.status === "approval"
                      ? "Question deferred"
                      : agent.status === "starting" &&
                          (agent.startAttempt?.prepareError ||
                            agent.startAttempt?.responseError)
                        ? "Waiting for Codex"
                        : [statusLabel(agent.status, undefined, agent.parkedEvent), nativeReleaseLabel(agent)]
                            .filter(Boolean).join(" · ")}
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
            <WorkerDiskLabel agent={agent} disk={disk} />
            {Boolean(agent.error) && (
              <span className="worker-error">{errorSummary}</span>
            )}
          </span>
        </UnstyledButton>
        {remove && (
          <Menu withinPortal position="bottom-end">
            <Menu.Target>
              <ActionIcon
                variant="subtle"
                color="gray"
                size="sm"
                aria-label={`Options for subagent ${agent.name}`}
              >
                <MoreHorizontal size={16} />
              </ActionIcon>
            </Menu.Target>
            <Menu.Dropdown>
              <Menu.Item
                color="red"
                leftSection={<Trash2 size={14} />}
                onClick={remove}
              >
                Delete
              </Menu.Item>
            </Menu.Dropdown>
          </Menu>
        )}
      </div>
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
