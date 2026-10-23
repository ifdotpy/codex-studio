import { useServerApi } from "../../servers/ServerServicesContext";
import { useSyncedVisualState } from "../../sync/useSyncedVisualState";
import { ActionIcon, Button, Menu, UnstyledButton } from "@mantine/core";
import { ChevronRight, MoreHorizontal, Trash2 } from "lucide-react";
import { memo, useContext, useMemo, type CSSProperties } from "react";
import { useCommittedCallback } from "../../hooks";
import { reportPromptComposerRender } from "../prompt-composer/renderProbe";
import "../../api";
import { nativeErrorView } from "../../nativeErrors";
import { DISK_FULL_WAIT_LABEL, diskFullHold } from "../../capacityRetry";
import { startWaitLabel } from "../../startWait";
import { modelDisplayName } from "../models/modelOption";
import { TeamModelCatalog } from "./WorkerOverview";
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
import { AgentElapsed, AgentProgress, commandWaitLabel } from "./AgentProgress";
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
  const { save, saved } = useServerApi();

  const storageKey = "codex-worker-disclosures";
  const key = `${agentId}:${label}`;
  const [disclosures] = useSyncedVisualState<Record<string, boolean>>(
    storageKey,
    {},
  );
  const expanded = disclosures[key] || false;
  return (
    <details
      className="worker-excerpt"
      open={expanded}
      onToggle={(event) => {
        if (event.target !== event.currentTarget) return;
        const value = event.currentTarget.open;
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

type WorkerCardProps = {
  agent: Agent;
  depth?: number;
  lead?: boolean;
  accounts?: Account[];
  selected: boolean;
  awaitingAnswer: boolean;
  deferred: boolean;
  open: () => void;
  previewResult: () => void;
  remove?: () => void;
  indicator?: ChatIndicator;
};

export default function WorkerCard(props: WorkerCardProps) {
  const open = useCommittedCallback(props.open);
  const previewResult = useCommittedCallback(props.previewResult);
  const remove = useCommittedCallback(() => props.remove?.());
  const indicator = useMemo(
    () => props.indicator,
    [props.indicator?.kind, props.indicator?.label],
  );
  return (
    <WorkerCardView
      {...props}
      open={open}
      previewResult={previewResult}
      remove={props.remove ? remove : undefined}
      indicator={indicator}
    />
  );
}

const WorkerCardView = memo(function WorkerCardView({
  agent,
  depth = 0,
  lead = false,
  accounts = [],
  selected,
  awaitingAnswer,
  deferred,
  open,
  previewResult,
  indicator,
  remove,
}: WorkerCardProps) {
  reportPromptComposerRender("team-row", agent.id);
  const provider = agent.provider || "codex";
  const models = useContext(TeamModelCatalog).filter(
    (row) => !row.provider || row.provider === provider,
  );
  const info = models.find(
    (row) => row.model === agent.model || row.resolvedModel === agent.model,
  );
  const resolved =
    info?.isDefault && info.resolvedModel
      ? models.find(
          (row) =>
            row.model !== info.model &&
            (row.model === info.resolvedModel ||
              row.resolvedModel === info.resolvedModel),
        )
      : undefined;
  const modelName =
    !info && agent.model === "default"
      ? "Auto"
      : modelDisplayName(
          resolved ||
            (info?.isDefault && info.resolvedModel
              ? { ...info, model: info.resolvedModel }
              : info),
          agent.model || "",
        );
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
    .join(", ");
  const overview = agent.overview;
  const errorView = nativeErrorView(agent.error);
  const diskHeld = Boolean(diskFullHold(agent));
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
  const statusDetail = agent.stall
    ? "Waiting for lead: task not submitted"
    : ["waiting_on_monitor"].includes(agent.status ?? "")
      ? commandWaitLabel(agent)
      : (startWaitLabel(agent) ??
        (indicator?.kind === "answer" ||
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
                          ? diskHeld
                            ? DISK_FULL_WAIT_LABEL
                            : "Failed"
                          : agent.status === "running" && agent.inFlight
                            ? statusLabel(
                                agent.status,
                                agent.activity?.phase || undefined,
                              )
                            : statusLabel(
                                agent.status ?? "",
                                undefined,
                                agent.parkedEvent ?? undefined,
                              ),
                      nativeReleaseLabel(agent),
                    ]
                      .filter(Boolean)
                      .join(" · ")));
  const statusText =
    !awaitingAnswer &&
    ["waiting", "parked", "queued"].includes(agent.status || "")
      ? "Waiting"
      : statusDetail;
  return (
    <div
      className={`worker-entry ${selected ? "selected" : ""}`}
      data-studio-state={indicator?.kind || "none"}
      style={{ "--worker-depth": depth } as CSSProperties}
    >
      <div className="worker-heading">
        <UnstyledButton
          className="worker"
          data-worker={lead ? undefined : agent.id}
          data-lead={lead ? agent.id : undefined}
          aria-current={selected ? "page" : undefined}
          onClick={open}
        >
          <span className="worker-text">
            <span className="worker-name-line">
              <strong title={agent.name || undefined}>{agent.name}</strong>
              <span className="worker-meta">
                <span className="worker-model-summary" title={meta}>
                  <span className="worker-role">
                    {lead
                      ? "Lead"
                      : agent.role === "reviewer"
                        ? "Reviewer"
                        : "Worker"}
                  </span>

                  <span className="worker-model-name">{modelName}</span>
                  <span className="worker-effort">
                    {effort.charAt(0).toUpperCase() + effort.slice(1)}
                  </span>
                  {connected.length > 1 && account && (
                    <span>{accountDisplayName(account)}</span>
                  )}
                </span>
              </span>
              <AgentElapsed agent={agent} />
            </span>
            <span className="worker-status-accessible">
              <ChatStatus
                status={indicator}
                provider={agent.provider ?? undefined}
                model={agent.model ?? undefined}
              />
              <small>{statusText}</small>
            </span>
            <span className="worker-live-line">
              <span
                className="worker-activity"
                title={errorSummary || statusDetail}
              >
                {statusText}
              </span>
              {agent.overview?.task && (
                <span className="worker-task" title={agent.overview.task}>
                  {agent.overview.task}
                </span>
              )}
              <TokenRate agent={agent} variant="worker" />
            </span>
            <AgentProgress agent={agent} />
            {agent.environment === "linux" && <small>Linux VM</small>}
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
      ) : !lead && agent.status === "completed" ? (
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
});
