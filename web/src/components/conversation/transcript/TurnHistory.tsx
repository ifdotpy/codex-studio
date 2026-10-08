import { reportPromptComposerRender } from "../../prompt-composer/renderProbe";
import {
  Fragment,
  memo,
  useMemo,
  useRef,
  useState,
  type ReactNode,
  type RefObject,
} from "react";
import {
  ChevronRight,
  CircleAlert,
  CircleStop,
  LoaderCircle,
  Wrench,
} from "lucide-react";
import { save, saved } from "../../../api";
import type { Agent, Message } from "../../../types";
import { useTurnErrors } from "../../useTurnErrors";
import Activity, {
  ToolCard,
  activitySummary,
  isFileChange,
  isPastCommand,
} from "./Activity";
import { toolLimitNotice } from "../../toolLimitNotice";
import { turnFailureReason } from "../../turnFailureReason";
import ConversationResults from "./ConversationResults";
import {
  historyPresentationGroups,
  incrementalHistoryGroups,
  isEmptyAssistantMessage,
  type HistoryGroup,
} from "../../turnHistoryModel";
import "./turn-history.css";
import { messageRenderKey } from "../../message-delivery/messageDelivery";
import ReasoningDuration from "./ReasoningDuration";
import HistoryWindow from "./HistoryWindow";
import { windowHistoryRows } from "./historyWindowModel";

type CommandVisibility = boolean | ReadonlyMap<string, boolean>;

function messageGroups(
  items: Message[],
  showCompletedCommands: CommandVisibility = false,
  includeReasoning = false,
  revealedMessage?: string,
) {
  const showItem = (item: Message) =>
    item.id === revealedMessage ||
    (typeof showCompletedCommands === "boolean"
      ? showCompletedCommands
      : item.turnId
        ? (showCompletedCommands.get(item.turnId) ?? true)
        : true);
  const groups: (Message | Message[])[] = [];
  for (let index = 0; index < items.length; index++) {
    const item = items[index];
    if (isEmptyAssistantMessage(item)) continue;
    if (!showItem(item) && isPastCommand(item)) continue;
    const isTool =
      ["tool", "output"].includes(item.role) && !isFileChange(item);
    if (isTool || (includeReasoning && item.role === "reasoning")) {
      const run = [item];
      while (index + 1 < items.length) {
        const next = items[index + 1];
        if (
          isEmptyAssistantMessage(next) ||
          (!showItem(next) && isPastCommand(next))
        ) {
          index++;
          continue;
        }
        if (
          !(
            (includeReasoning && next.role === "reasoning") ||
            (["tool", "output"].includes(next.role) && !isFileChange(next))
          )
        )
          break;
        run.push(next);
        index++;
      }
      if (run.some((entry) => ["tool", "output"].includes(entry.role)))
        groups.push(run);
      else groups.push(...run);
    } else groups.push(item);
  }
  return groups;
}

function messages(
  items: Message[],
  render: (message: Message) => ReactNode,
  agentId?: string,
  cwd?: string,
) {
  return messageGroups(items).map((item) =>
    Array.isArray(item) ? (
      <Activity key={item[0].id} items={item} agentId={agentId} />
    ) : (
      <Fragment key={messageRenderKey(item)}>
        {isFileChange(item) ? (
          <ToolCard item={item} agentId={agentId} cwd={cwd} />
        ) : item.role === "reasoning" ? (
          <ReasoningDuration item={item} />
        ) : (
          render(item)
        )}
      </Fragment>
    ),
  );
}

const WorkBlock = memo(function WorkBlock({
  items,
  storageKey,
  storageIds,
  agentId,
  active,
}: {
  items: Message[];
  storageKey: string;
  storageIds: string[];
  agentId?: string;
  active: boolean;
}) {
  const key = `${storageKey}:tools-v3`;
  const tools = items.filter((item) => ["tool", "output"].includes(item.role));
  const id = storageIds[0] || items[0].id;
  const [open, setOpen] = useState(
    () =>
      saved<Record<string, boolean>>(key, {})[id] ??
      (active || tools.length < 3),
  );
  const summary = activitySummary(tools);
  const hasLimit = tools.some((item) => toolLimitNotice(item));
  const update = (value: boolean) => {
    setOpen(value);
    save(
      key,
      Object.fromEntries([
        ...Object.entries(saved<Record<string, boolean>>(key, {}))
          .filter(([entry]) => !storageIds.includes(entry))
          .slice(-499),
        ...storageIds.map((entry) => [entry, value]),
      ]),
    );
  };
  return (
    <details
      className="turn-work"
      data-running={summary.running}
      data-failed={summary.failed}
      open={open}
      onToggle={(event) => {
        if (
          event.target === event.currentTarget &&
          event.currentTarget.open !== open
        )
          update(event.currentTarget.open);
      }}
    >
      <summary
        aria-label={`Work log: ${summary.label || "Agent updates"}`}
        onClick={(event) => {
          event.preventDefault();
          update(!open);
        }}
      >
        <ChevronRight size={14} className="turn-expand-icon" />
        {summary.running ? (
          <LoaderCircle size={14} className="spin" />
        ) : (
          <Wrench size={14} />
        )}
        <span className="turn-work-label">
          {hasLimit && (
            <strong className="activity-limit">Account limit reached · </strong>
          )}
          {summary.label || "Agent updates"}
        </span>
        {!!summary.running && (
          <span className="activity-running">{summary.running} running</span>
        )}
        {!!summary.failed && (
          <span className="activity-failed">{summary.failed} failed</span>
        )}
      </summary>
      <div className="turn-work-body">
        {items.map((item) => {
          return open ? (
            item.role === "reasoning" ? (
              <ReasoningDuration key={item.id} item={item} />
            ) : (
              <ToolCard key={item.id} item={item} agentId={agentId} />
            )
          ) : (
            <span
              key={item.id}
              data-message={item.id}
              data-lazy-message
              hidden
            />
          );
        })}
      </div>
    </details>
  );
});

function Turn({
  group,
  storageKey,
  render,
  agentId,
  cwd,
  onJump,
  failureReason,
  failurePending,
  failureLookupFailed,
  retryFailure,
  revealedMessage,
}: {
  group: HistoryGroup;
  storageKey: string;
  render: (message: Message) => ReactNode;
  agentId?: string;
  cwd?: string;
  onJump: (id: string) => void;
  failureReason: string;
  failurePending: boolean;
  failureLookupFailed: boolean;
  retryFailure: () => void;
  revealedMessage?: string;
}) {
  const result = group.result;
  const turns = group.turns || [group];
  const [savedCommandVisibility, setSavedCommandVisibility] = useState(() => {
    const visibility = new Map<string, boolean>();
    for (const turn of turns) {
      const turnId = turn.items[0].turnId;
      if (turnId) visibility.set(turnId, !turn.outcome);
    }
    return visibility;
  });
  let commandVisibility = savedCommandVisibility;
  for (const turn of turns) {
    const turnId = turn.items[0].turnId;
    if (!turnId || commandVisibility.has(turnId)) continue;
    commandVisibility = new Map(commandVisibility).set(turnId, !turn.outcome);
  }
  if (commandVisibility !== savedCommandVisibility)
    setSavedCommandVisibility(commandVisibility);
  const groupedMessages = useMemo(
    () => messageGroups(group.items, commandVisibility, true, revealedMessage),
    [group.items, commandVisibility, revealedMessage],
  );
  const visibleError = group.items.some(
    (item) =>
      item.nativeNotice === "error" ||
      (item.nativeError && !["tool", "output"].includes(item.role)),
  );
  if (
    !groupedMessages.length &&
    !["failed", "interrupted"].includes(group.outcome || "")
  )
    return null;
  return (
    <section
      className="turn-history"
      data-turn={
        !group.outcome || ["failed", "interrupted"].includes(group.outcome)
          ? turns.at(-1)?.items[0].turnId
          : group.items[0].turnId
      }
      data-turns={turns
        .map((turn) => turn.items[0].turnId)
        .filter(Boolean)
        .join(" ")}
      data-outcome={group.outcome || "active"}
    >
      {groupedMessages.map((item) =>
        Array.isArray(item) ? (
          <WorkBlock
            key={item[0].id}
            items={item}
            storageKey={storageKey}
            storageIds={turns.flatMap((turn) => {
              const turnId = turn.items[0].turnId;
              const contributor = item.find((entry) => entry.turnId === turnId);
              return contributor ? [contributor.id] : [];
            })}
            agentId={agentId}
            active={!group.outcome}
          />
        ) : (
          <div
            key={item.id}
            className={item.id === result?.id ? "turn-answer" : undefined}
          >
            {isFileChange(item) ? (
              <ToolCard item={item} agentId={agentId} cwd={cwd} />
            ) : item.role === "reasoning" ? (
              <ReasoningDuration item={item} />
            ) : (
              render(item)
            )}
          </div>
        ),
      )}
      {group.outcome === "failed" &&
        !visibleError &&
        (failurePending ? (
          <div
            className="turn-error-pending"
            role="status"
            aria-label="Loading error details"
          >
            <span />
            <span />
            <span />
          </div>
        ) : (
          <p className="turn-problem" role="status">
            <CircleAlert size={14} />
            <span>
              {failureLookupFailed
                ? "Could not load error details."
                : failureReason}
            </span>
            {failureLookupFailed && (
              <button className="turn-error-retry" onClick={retryFailure}>
                Retry
              </button>
            )}
          </p>
        ))}
      {group.outcome === "interrupted" && (
        <p className="turn-problem">
          <CircleStop size={14} />
          Turn interrupted
        </p>
      )}
      <ConversationResults
        messages={group.items}
        agentId={agentId}
        onJump={onJump}
      />
    </section>
  );
}

export default function TurnHistory({
  items: sourceItems,
  currentTurn,
  enabled,
  storageKey,
  renderMessage,
  agentId,
  onJump,
  agent,
  scrollContainer,
  rememberScroll,
}: {
  items: Message[];
  agent?: Agent;
  currentTurn?: string;
  enabled: boolean;
  storageKey: string;
  renderMessage: (message: Message) => ReactNode;
  agentId?: string;
  onJump: (id: string) => void;
  scrollContainer?: RefObject<HTMLDivElement | null>;
  rememberScroll?: () => void;
}) {
  reportPromptComposerRender("turn-history", agentId);
  const { items, loading, failed, retry } = useTurnErrors(
    sourceItems,
    enabled ? agent : undefined,
    storageKey,
  );
  const previousGroups = useRef<{
    items: Message[];
    currentTurn?: string;
    groups: HistoryGroup[];
  } | null>(null);
  const groups = useMemo(() => {
    const nativeGroups = incrementalHistoryGroups(
      items,
      currentTurn,
      previousGroups.current,
    );
    previousGroups.current = { items, currentTurn, groups: nativeGroups };
    return historyPresentationGroups(nativeGroups);
  }, [items, currentTurn]);
  const failureReasons = useMemo(() => {
    const failedTurns = new Set(
      groups
        .flatMap((group) => group.turns || [group])
        .filter((turn) => turn.outcome === "failed")
        .flatMap((turn) => {
          const turnId = turn.items[0].turnId;
          return turnId && typeof turnId === "string" ? [turnId] : [];
        }),
    );
    const byTurn = new Map<string, Message[]>();
    for (const item of items) {
      if (typeof item.turnId !== "string") continue;
      if (!failedTurns.has(item.turnId)) continue;
      const turn = byTurn.get(item.turnId) || [];
      turn.push(item);
      byTurn.set(item.turnId, turn);
    }
    return new Map(
      [...byTurn].map(([turn, messages]) => [
        turn,
        turnFailureReason(messages),
      ]),
    );
  }, [items, groups]);
  const rows = useMemo(
    () => (scrollContainer ? windowHistoryRows(groups) : groups),
    [groups, scrollContainer],
  );
  return (
    <HistoryWindow
      rows={rows}
      storageKey={storageKey}
      rememberScroll={rememberScroll}
      scrollContainer={scrollContainer}
      render={(group, revealedMessage) => {
        const outcomeTurn = group.turns?.at(-1) || group;
        const turnId = outcomeTurn.items[0].turnId;
        if (
          !enabled ||
          group.items[0].role === "user" ||
          !group.items[0].turnId ||
          typeof group.items[0].turnId !== "string" ||
          !turnId ||
          typeof turnId !== "string"
        )
          return (
            <Fragment key={messageRenderKey(group.items[0])}>
              {messages(
                group.items,
                renderMessage,
                agentId,
                agent?.cwd ?? undefined,
              )}
            </Fragment>
          );
        return (
          <Turn
            key={group.id}
            group={group}
            revealedMessage={revealedMessage}
            failurePending={loading.has(turnId)}
            failureLookupFailed={failed.has(turnId)}
            retryFailure={() => retry(turnId)}
            failureReason={failureReasons.get(turnId) || ""}
            storageKey={storageKey}
            render={renderMessage}
            agentId={agentId}
            cwd={agent?.cwd ?? undefined}
            onJump={onJump}
          />
        );
      }}
    />
  );
}
