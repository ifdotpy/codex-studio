import { Fragment, useEffect, useRef, useState, type ReactNode } from "react";
import {
  ChevronRight,
  CircleAlert,
  CircleStop,
  LoaderCircle,
  Wrench,
} from "lucide-react";
import { save, saved } from "../api";
import type { Message } from "../types";
import Activity, { ToolCard, activitySummary } from "./Activity";
import ConversationResults from "./ConversationResults";
import { historyGroups, type HistoryGroup } from "./turnHistoryModel";
import "./turn-history.css";

function messages(items: Message[], render: (message: Message) => ReactNode) {
  const groups: (Message | Message[])[] = [];
  for (const item of items) {
    if (["tool", "output"].includes(item.role)) {
      const last = groups.at(-1);
      if (Array.isArray(last)) last.push(item);
      else groups.push([item]);
    } else groups.push(item);
  }
  return groups.map((item) =>
    Array.isArray(item) ? (
      <Activity key={item[0].id} items={item} />
    ) : (
      <Fragment key={item.id}>{render(item)}</Fragment>
    ),
  );
}

function Turn({
  group,
  storageKey,
  render,
  agentId,
  onJump,
  following,
}: {
  group: HistoryGroup;
  storageKey: string;
  render: (message: Message) => ReactNode;
  agentId?: string;
  onJump: (id: string) => void;
  following: boolean;
}) {
  const key = `${storageKey}:work-v2`;
  const [initialChoice] = useState(
    () => saved<Record<string, boolean>>(key, {})[group.id],
  );
  const choice = useRef(initialChoice);
  const allowLegacyResult = useRef(!!group.outcome);
  const result =
    group.result?.phase === "final_answer" || allowLegacyResult.current
      ? group.result
      : undefined;
  const work = group.items.filter((item) => item.id !== result?.id);
  const tools = work.filter((item) => ["tool", "output"].includes(item.role));
  const summary = activitySummary(tools);
  const problem =
    group.outcome === "failed" ||
    group.outcome === "interrupted" ||
    summary.failed > 0;
  const [open, setOpen] = useState(
    () => choice.current ?? (!result || problem),
  );
  const hadResult = useRef(!!result);
  useEffect(() => {
    const appeared = !!result && !hadResult.current;
    hadResult.current = !!result;
    // Do not remove the paragraph someone is reading, or override their choice.
    if (appeared && following && choice.current === undefined && !problem)
      setOpen(false);
  }, [result, following, problem]);
  const update = (value: boolean) => {
    choice.current = value;
    setOpen(value);
    save(
      key,
      Object.fromEntries([
        ...Object.entries(saved<Record<string, boolean>>(key, {}))
          .filter(([id]) => id !== group.id)
          .slice(-499),
        [group.id, value],
      ]),
    );
  };
  return (
    <section
      className="turn-history"
      data-turn={group.items[0].turnId}
      data-outcome={group.outcome || "active"}
    >
      {!!work.length && (
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
              {summary.label || "Agent updates"}
            </span>
            {!!summary.running && (
              <span className="activity-running">
                {summary.running} running
              </span>
            )}
            {!!summary.failed && (
              <span className="activity-failed">{summary.failed} failed</span>
            )}
            <ChevronRight size={14} className="turn-expand-icon" />
          </summary>
          <div className="turn-work-body">
            {work.map((item) =>
              open ? (
                ["tool", "output"].includes(item.role) ? (
                  <ToolCard key={item.id} item={item} />
                ) : (
                  <Fragment key={item.id}>{render(item)}</Fragment>
                )
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
      )}
      {group.outcome === "failed" && (
        <p className="turn-problem" role="status">
          <CircleAlert size={14} />
          Turn failed
        </p>
      )}
      {group.outcome === "interrupted" && (
        <p className="turn-problem">
          <CircleStop size={14} />
          Turn interrupted
        </p>
      )}
      {result && <div className="turn-answer">{render(result)}</div>}
      <ConversationResults
        messages={group.items}
        agentId={agentId}
        onJump={onJump}
      />
    </section>
  );
}

export default function TurnHistory({
  items,
  currentTurn,
  enabled,
  storageKey,
  renderMessage,
  agentId,
  onJump,
  following = true,
}: {
  items: Message[];
  currentTurn?: string;
  enabled: boolean;
  storageKey: string;
  renderMessage: (message: Message) => ReactNode;
  agentId?: string;
  onJump: (id: string) => void;
  following?: boolean;
}) {
  if (!enabled) return <>{messages(items, renderMessage)}</>;
  return (
    <>
      {historyGroups(items, currentTurn).map((group) =>
        group.items[0].role === "user" || !group.items[0].turnId ? (
          <Fragment key={group.id}>
            {messages(group.items, renderMessage)}
          </Fragment>
        ) : (
          <Turn
            key={group.id}
            group={group}
            storageKey={storageKey}
            render={renderMessage}
            agentId={agentId}
            onJump={onJump}
            following={following}
          />
        ),
      )}
    </>
  );
}
