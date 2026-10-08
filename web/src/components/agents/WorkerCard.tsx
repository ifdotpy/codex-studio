import {
  ActionIcon,
  Button,
  Loader,
  Menu,
  UnstyledButton,
} from "@mantine/core";
import {
  ChevronRight,
  CircleAlert,
  MoreHorizontal,
  Trash2,
} from "lucide-react";
import { useState } from "react";
import { save, saved } from "../../api";
import { nativeErrorView } from "../../nativeErrors";
import { setupModelName } from "./AgentSetupPicker";
import { useWorkerModels } from "./WorkerModelPicker";
import { ProviderMark } from "../AccountTiles";
import type { Account } from "../Accounts";
import { accountDisplayName } from "../../accountName";
import {
  agentErrorLabel,
  agentStopReason,
  nativeReleaseLabel,
  statusLabel,
  type Agent,
} from "../../types";
import ChatStatus from "./ChatStatus";
import TokenRate from "../TokenRate";
import type { ChatIndicator } from "../chat-status/chatStatusModel";

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
  accounts = [],
  selected,
  awaitingAnswer,
  deferred,
  open,
  previewResult,
  indicator,
  remove,
}: {
  agent: Agent;
  accounts?: Account[];
  selected: boolean;
  awaitingAnswer: boolean;
  deferred: boolean;
  open: () => void;
  previewResult: () => void;
  remove?: () => void;
  indicator?: ChatIndicator;
}) {
  const catalog = useWorkerModels(
    agent.accountKey || "default",
    agent.source === "managed",
  );
  const info = catalog.models.find(
    (row) => row.model === agent.model || row.resolvedModel === agent.model,
  );
  const resolved =
    info?.isDefault && info.resolvedModel
      ? catalog.models.find(
          (row) =>
            row.model !== info.model &&
            (row.model === info.resolvedModel ||
              row.resolvedModel === info.resolvedModel),
        )
      : undefined;
  const modelName =
    !info && agent.model === "default"
      ? "Auto"
      : setupModelName(
          resolved ||
            (info?.isDefault && info.resolvedModel
              ? { ...info, model: info.resolvedModel }
              : info),
          agent.model || "",
        );
  const provider = agent.provider || "codex";
  const connected = accounts.filter(
    (account) =>
      !account.disconnected && (account.provider || "codex") === provider,
  );
  const account = connected.find(
    (account) => account.id === (agent.accountKey || "default"),
  );
  const effort = agent.effort || info?.defaultReasoningEffort || "default";
  const meta = [
    modelName,
    effort.charAt(0).toUpperCase() + effort.slice(1),
    connected.length > 1 && account ? accountDisplayName(account) : "",
  ]
    .filter(Boolean)
    .join(" · ");
  const overview = agent.overview;
  const errorView = nativeErrorView(agent.error);
  const error = agent.error ? agentErrorLabel(agent) : "";
  const stopReason = agentStopReason(agent);
  const errorSummary =
    /worktree[\s\S]*add[\s\S]*(?:exit status|exit code|failed)/i.test(error)
      ? "Could not prepare the project folder."
      : error.length > 160 ||
          /[\r\n]/.test(error) ||
          /^Command [["']/.test(error)
        ? "The agent stopped with an error."
        : error;
  const statusDetail =
    indicator?.kind === "answer" ||
    ["waiting", "parked"].includes(agent.status ?? "") ||
    (indicator?.kind === "working" && !agent.inFlight)
      ? indicator?.label || statusLabel(agent.status ?? "")
      : awaitingAnswer
        ? "Needs your answer"
        : agent.status === "starting" && agent.worktreePreparation
          ? agent.worktreePreparation === "waiting"
            ? "Waiting to prepare folder"
            : "Preparing folder"
          : deferred && agent.status === "approval"
            ? "Question deferred"
            : agent.status === "starting" &&
                (agent.startAttempt?.prepareError ||
                  agent.startAttempt?.responseError)
              ? "Waiting for Codex"
              : [
                  agent.status === "completed"
                    ? "Finished"
                    : agent.status === "failed"
                      ? "Failed"
                      : agent.status === "running" && agent.inFlight
                        ? "Working"
                        : statusLabel(
                            agent.status ?? "",
                            undefined,
                            agent.parkedEvent ?? undefined,
                          ),
                  nativeReleaseLabel(agent),
                ]
                  .filter(Boolean)
                  .join(" · ");
  const statusText =
    !awaitingAnswer &&
    ["waiting", "parked", "queued"].includes(agent.status || "")
      ? "Waiting"
      : statusDetail;
  return (
    <div className={`worker-entry ${selected ? "selected" : ""}`}>
      <div className="worker-heading">
        <UnstyledButton
          className="worker"
          data-worker={agent.id}
          aria-current={selected ? "page" : undefined}
          onClick={open}
        >
          <span className="worker-provider">
            <ProviderMark provider={provider} />
          </span>
          <span className="worker-text">
            <strong>{agent.name}</strong>
            {agent.environment === "linux" && <small>Linux VM</small>}
            <span className="worker-model-summary" title={meta}>
              {meta}
            </span>
            <span
              className={`worker-meta worker-state-${agent.status || "waiting"}`}
            >
              <ChatStatus
                status={indicator}
                provider={agent.provider ?? undefined}
                model={agent.model ?? undefined}
              />
              {!["working", "answer", "error"].includes(
                indicator?.kind || "",
              ) &&
                ["running", "starting"].includes(agent.status || "") && (
                  <Loader size={12} color="gray" aria-hidden="true" />
                )}
              {agent.status === "failed" && indicator?.kind !== "error" && (
                <CircleAlert size={13} aria-hidden="true" />
              )}
              <small title={statusDetail}>{statusText}</small>
              <TokenRate agent={agent} variant="worker" />
            </span>
            {Boolean(agent.error) && (
              <span
                className={stopReason ? "worker-stop-reason" : "worker-error"}
              >
                {stopReason || errorSummary}
              </span>
            )}
          </span>
        </UnstyledButton>
        {remove && (
          <Menu withinPortal position="bottom-end">
            <Menu.Target>
              <ActionIcon
                className="worker-options"
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
        <details
          className={
            stopReason ? "worker-stop-details" : "worker-error-details"
          }
        >
          <summary>{stopReason ? "Stop details" : "Error details"}</summary>
          <pre>{errorView.details || errorView.message}</pre>
        </details>
      )}
      {overview?.result ? (
        <WorkerExcerpt
          agentId={agent.id}
          label="Last report"
          text={overview.result}
          truncated={overview.resultTruncated ?? undefined}
          open={open}
        />
      ) : agent.status === "completed" ? (
        <p className="worker-missing">No final report available</p>
      ) : null}
      {overview?.resultFile && (
        <Button
          component="a"
          href={overview.resultFile}
          size="compact-xs"
          variant="subtle"
          onClick={(event) => {
            event.preventDefault();
            previewResult();
          }}
        >
          Open submitted result
        </Button>
      )}
    </div>
  );
}
