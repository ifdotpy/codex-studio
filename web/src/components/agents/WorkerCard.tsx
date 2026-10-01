import { ActionIcon, Button, Menu, UnstyledButton } from "@mantine/core";
import { ChevronRight, MoreHorizontal, Trash2 } from "lucide-react";
import { useState } from "react";
import { save, saved } from "../../api";
import { nativeErrorView } from "../../nativeErrors";
import { shortModel } from "./ExecutionSettings";
import {
  agentErrorLabel,
  nativeReleaseLabel,
  statusLabel,
  type Agent,
} from "../../types";
import type { WorktreeDiskSnapshot } from "../../hooks/useWorktreeDisk";
import ChatStatus from "./ChatStatus";
import type { ChatIndicator } from "../chatStatusModel";
import { WorkerDiskLabel } from "../WorktreeDisk";

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
          /^Command [["']/.test(error)
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
                        : [
                            statusLabel(
                              agent.status,
                              undefined,
                              agent.parkedEvent,
                            ),
                            nativeReleaseLabel(agent),
                          ]
                            .filter(Boolean)
                            .join(" · ")}
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
