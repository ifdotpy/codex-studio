import { AgentStatus } from "../../generated/api";
import { RetainedWorktree } from "./RetainedWorktrees";
import { providerLabel } from "../../generated/providerKind.ts";
import { useServerApi } from "../../servers/ServerServicesContext";
import { useSyncedVisualState } from "../../sync/useSyncedVisualState";
import { ActionIcon, Button, Menu, UnstyledButton } from "@mantine/core";
import {
  ChevronRight,
  CircleAlert,
  FileCheck,
  FileText,
  MoreHorizontal,
  Trash2,
  Terminal,
} from "lucide-react";
import { memo, useContext, useMemo, type CSSProperties } from "react";
import { useCommittedCallback } from "../../hooks";
import { reportPromptComposerRender } from "../prompt-composer/renderProbe";
import "../../api";
import { nativeErrorView } from "../../nativeErrors";
import { DISK_FULL_WAIT_LABEL, diskFullHold } from "../../capacityRetry";
import { startWaitLabel } from "../../startWait";
import { modelDisplayName } from "../models/modelOption";
import { TeamModelCatalog } from "./WorkerOverview";
import Disclosure from "../ui/Disclosure";
import type { Account } from "../Accounts";
import { ProviderMark } from "../AccountTiles";
import { TeamRoleIcon, TeamStateIcon, activityPresentation } from "./TeamFacts";
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
  truncated?: boolean | undefined;
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
    <Disclosure
      className="worker-excerpt"
      open={expanded}
      onChange={(value) => {
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
      summary={
        <span className="worker-excerpt-label">
          <FileText size={11} aria-hidden="true" />
          {label}
          <ChevronRight size={11} aria-hidden="true" />
        </span>
      }
    >
      <div className="worker-excerpt-full">
        <p>{text}</p>
        {truncated && (
          <Button variant="subtle" size="compact-xs" onClick={open}>
            Continue in chat
          </Button>
        )}
      </div>
    </Disclosure>
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
  remove?: (() => void) | undefined;
  indicator?: ChatIndicator | undefined;
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
  const provider = agent.provider || "";
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
    (account) => !account.disconnected && account.provider === provider,
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
          ? indicator?.label || statusLabel(agent.status)
          : awaitingAnswer
            ? "Needs your answer"
            : agent.status === AgentStatus.starting && agent.worktreePreparation
              ? agent.worktreePreparation === "waiting"
                ? "Waiting to prepare folder"
                : "Preparing folder"
              : deferred && agent.status === AgentStatus.approval
                ? "Question deferred"
                : agent.status === AgentStatus.starting &&
                    (agent.startAttempt?.prepareError ||
                      agent.startAttempt?.responseError)
                  ? "Waiting for Codex"
                  : [
                      agent.status === AgentStatus.completed
                        ? "Finished"
                        : agent.status === AgentStatus.failed
                          ? diskHeld
                            ? DISK_FULL_WAIT_LABEL
                            : "Failed"
                          : agent.status === AgentStatus.running &&
                              agent.inFlight
                            ? statusLabel(
                                agent.status,
                                agent.activity?.phase || undefined,
                              )
                            : statusLabel(
                                agent.status,
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
  const activity = activityPresentation(agent);
  const ActivityIcon =
    error && !diskHeld && !stopReason ? CircleAlert : activity.Icon;
  const activityText =
    error && !diskHeld
      ? stopReason || errorSummary === "Could not prepare the project folder."
        ? stopReason || errorSummary
        : error.replace(/\s+/g, " ")
      : agent.commandProgress?.[0]?.stage || statusText;
  return (
    <div
      className={`worker-entry ${selected ? "selected" : ""}`}
      data-studio-state={indicator?.kind || "none"}
      data-has-parent={depth > 0 || undefined}
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
              <span className="worker-indicator">
                <ChatStatus
                  status={indicator}
                  provider={agent.provider ?? undefined}
                  model={agent.model ?? undefined}
                />
                {(!indicator ||
                  ["none", "paused"].includes(indicator.kind)) && (
                  <TeamStateIcon kind={indicator?.kind || "none"} />
                )}
              </span>
              <span
                className="worker-provider"
                role="img"
                aria-label={providerLabel(provider)}
                title={providerLabel(provider)}
              >
                <ProviderMark provider={provider} />
              </span>
              <strong title={agent.name || undefined}>{agent.name}</strong>
              <TeamRoleIcon agent={agent} lead={lead} />
              <AgentElapsed agent={agent} />
            </span>
            <span className="worker-live-line">
              <ActivityIcon
                className="worker-activity-icon"
                aria-label={
                  error && !diskHeld ? "Error reason" : activity.label
                }
                role="img"
              />
              <span
                className={
                  error && !diskHeld && !stopReason
                    ? "worker-activity worker-error"
                    : "worker-activity"
                }
                title={error || statusDetail}
              >
                {activityText}
              </span>
            </span>
            {agent.overview?.task && !(error && !diskHeld && !stopReason) && (
              <span className="worker-task" title={agent.overview.task}>
                {agent.overview.task}
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
      <RetainedWorktree agent={agent} />
      <div className="worker-bottom">
        <span className="worker-meta" title={meta}>
          <span className="worker-model-name">{modelName}</span>
          <span className="worker-effort">
            {effort.charAt(0).toUpperCase() + effort.slice(1)}
          </span>
          {connected.length > 1 && account && (
            <span className="worker-account">
              {accountDisplayName(account)}
            </span>
          )}
          {agent.environment === "linux" && (
            <span className="worker-environment">VM</span>
          )}
          <TokenRate agent={agent} variant="worker" />
        </span>
        <div className="worker-disclosures">
          {Boolean(agent.error) && (
            <Disclosure
              className={
                stopReason ? "worker-stop-details" : "worker-error-details"
              }
              ariaLabel={stopReason ? "Stop details" : "Error details"}
              summary={
                <>
                  <CircleAlert aria-hidden="true" />
                  <span className="sr-only">
                    {stopReason ? "Stop details" : "Error details"}
                  </span>
                </>
              }
            >
              <pre>{errorView.details || errorView.message}</pre>
            </Disclosure>
          )}
          {overview?.result ? (
            <WorkerExcerpt
              agentId={agent.id}
              label="Last report"
              text={overview.result}
              truncated={overview.resultTruncated ?? undefined}
              open={open}
            />
          ) : null}
          {!!agent.commandProgress?.length && (
            <Disclosure
              className="worker-command-details"
              ariaLabel="Command progress"
              summary={
                <>
                  <Terminal aria-hidden="true" />
                  <span className="sr-only">Command progress</span>
                </>
              }
            >
              <AgentProgress agent={agent} />
            </Disclosure>
          )}
          {agent.stall && (
            <span className="agent-stall" title={statusDetail}>
              Waiting for lead
            </span>
          )}
        </div>
      </div>
      {overview?.resultFile && (
        <div className="worker-actions">
          <Button
            component="a"
            className="worker-result"
            href={overview.resultFile}
            size="compact-xs"
            variant="subtle"
            color="gray"
            leftSection={<FileCheck size={12} aria-hidden="true" />}
            onClick={(event) => {
              event.preventDefault();
              previewResult();
            }}
          >
            Open submitted result
          </Button>
        </div>
      )}
    </div>
  );
});
