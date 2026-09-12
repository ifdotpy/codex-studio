import {
  Fragment,
  memo,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import {
  ChevronRight,
  CircleAlert,
  CircleStop,
  LoaderCircle,
  Wrench,
} from "lucide-react";
import { save, saved } from "../api";
import type { Agent, Message } from "../types";
import { useTurnErrors } from "./useTurnErrors";
import Activity, { ToolCard, activitySummary, isFileChange } from "./Activity";
import { toolLimitNotice } from "./toolLimitNotice";
import { turnFailureReason } from "./turnFailureReason";
import ConversationResults from "./ConversationResults";
import { historyGroups, type HistoryGroup } from "./turnHistoryModel";
import "./turn-history.css";
import { messageRenderKey } from "./messageDelivery";
import ReasoningDuration from "./ReasoningDuration";

function messageGroups(items: Message[]) {
  const groups: (Message | Message[])[] = [];
  for (const item of items) {
    if (isFileChange(item)) groups.push(item);
    else if (["tool", "output"].includes(item.role)) {
      const last = groups.at(-1);
      if (Array.isArray(last)) last.push(item);
      else groups.push([item]);
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
  agentId,
}: {
  items: Message[];
  storageKey: string;
  agentId?: string;
}) {
  const key = `${storageKey}:tools-v3`;
  const id = items[0].id;
  const [open, setOpen] = useState(
    () => saved<Record<string, boolean>>(key, {})[id] ?? items.length < 3,
  );
  const [visited, setVisited] = useState(open);
  const summary = activitySummary(items);
  const hasLimit = items.some((item) => toolLimitNotice(item));
  const update = (value: boolean) => {
    setOpen(value);
    if (value) setVisited(true);
    save(
      key,
      Object.fromEntries([
        ...Object.entries(saved<Record<string, boolean>>(key, {}))
          .filter(([entry]) => entry !== id)
          .slice(-499),
        [id, value],
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
        <ChevronRight size={14} className="turn-expand-icon" />
      </summary>
      <div className="turn-work-body">
        {items.map((item) =>
          visited ? (
            <ToolCard key={item.id} item={item} agentId={agentId} />
          ) : (
            <span
              key={item.id}
              data-message={item.id}
              data-lazy-message
              hidden
            />
          ),
        )}
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
}) {
  const result = group.result;
  const groupedMessages = useMemo(
    () => messageGroups(group.items),
    [group.items],
  );
  const visibleError = group.items.some(
    (item) =>
      item.nativeNotice === "error" ||
      (item.nativeError && !["tool", "output"].includes(item.role)),
  );
  return (
    <section
      className="turn-history"
      data-turn={group.items[0].turnId}
      data-outcome={group.outcome || "active"}
    >
      {groupedMessages.map((item) =>
        Array.isArray(item) ? (
          <WorkBlock
            key={item[0].id}
            items={item}
            storageKey={storageKey}
            agentId={agentId}
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
}: {
  items: Message[];
  agent?: Agent;
  currentTurn?: string;
  enabled: boolean;
  storageKey: string;
  renderMessage: (message: Message) => ReactNode;
  agentId?: string;
  onJump: (id: string) => void;
}) {
  const { items, loading, failed, retry } = useTurnErrors(
    sourceItems,
    enabled ? agent : undefined,
    storageKey,
  );
  const previousGroups = useRef<HistoryGroup[]>([]);
  const groups = useMemo(() => {
    const previous = new Map(
      previousGroups.current.map((group) => [group.id, group]),
    );
    const next = historyGroups(items, currentTurn).map((group) => {
      const prior = previous.get(group.id);
      return prior &&
        prior.outcome === group.outcome &&
        prior.result === group.result &&
        prior.items.length === group.items.length &&
        prior.items.every((item, index) => item === group.items[index])
        ? prior
        : group;
    });
    previousGroups.current = next;
    return next;
  }, [items, currentTurn]);
  const failureReasons = useMemo(() => {
    const failedTurns = new Set(
      groups
        .filter((group) => group.outcome === "failed")
        .map((group) => group.items[0].turnId),
    );
    const byTurn = new Map<string, Message[]>();
    for (const item of items) {
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
  if (!enabled)
    return <>{messages(items, renderMessage, agentId, agent?.cwd)}</>;
  return (
    <>
      {groups.map((group) =>
        group.items[0].role === "user" || !group.items[0].turnId ? (
          <Fragment key={messageRenderKey(group.items[0])}>
            {messages(group.items, renderMessage, agentId, agent?.cwd)}
          </Fragment>
        ) : (
          <Turn
            key={group.id}
            group={group}
            failurePending={loading.has(group.items[0].turnId)}
            failureLookupFailed={failed.has(group.items[0].turnId)}
            retryFailure={() => retry(group.items[0].turnId)}
            failureReason={failureReasons.get(group.items[0].turnId) || ""}
            storageKey={storageKey}
            render={renderMessage}
            agentId={agentId}
            cwd={agent?.cwd}
            onJump={onJump}
          />
        ),
      )}
    </>
  );
}
