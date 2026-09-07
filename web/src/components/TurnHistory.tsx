import { Fragment, useState, type ReactNode } from "react";
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

function messageGroups(items: Message[]) {
  const groups: (Message | Message[])[] = [];
  for (const item of items) {
    if (["tool", "output"].includes(item.role)) {
      const last = groups.at(-1);
      if (Array.isArray(last)) last.push(item);
      else groups.push([item]);
    } else groups.push(item);
  }
  return groups;
}

function messages(items: Message[], render: (message: Message) => ReactNode) {
  return messageGroups(items).map((item) =>
    Array.isArray(item) ? (
      <Activity key={item[0].id} items={item} />
    ) : (
      <Fragment key={item.id}>{render(item)}</Fragment>
    ),
  );
}

function WorkBlock({
  items,
  storageKey,
}: {
  items: Message[];
  storageKey: string;
}) {
  const key = `${storageKey}:tools-v3`;
  const id = items[0].id;
  const [open, setOpen] = useState(
    () => saved<Record<string, boolean>>(key, {})[id] ?? false,
  );
  const summary = activitySummary(items);
  const update = (value: boolean) => {
    setOpen(value);
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
          open ? (
            <ToolCard key={item.id} item={item} />
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
}

function Turn({
  group,
  storageKey,
  render,
  agentId,
  onJump,
}: {
  group: HistoryGroup;
  storageKey: string;
  render: (message: Message) => ReactNode;
  agentId?: string;
  onJump: (id: string) => void;
}) {
  const result = group.result;
  return (
    <section
      className="turn-history"
      data-turn={group.items[0].turnId}
      data-outcome={group.outcome || "active"}
    >
      {messageGroups(group.items).map((item) =>
        Array.isArray(item) ? (
          <WorkBlock key={item[0].id} items={item} storageKey={storageKey} />
        ) : (
          <div
            key={item.id}
            className={item.id === result?.id ? "turn-answer" : undefined}
          >
            {render(item)}
          </div>
        ),
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
}: {
  items: Message[];
  currentTurn?: string;
  enabled: boolean;
  storageKey: string;
  renderMessage: (message: Message) => ReactNode;
  agentId?: string;
  onJump: (id: string) => void;
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
          />
        ),
      )}
    </>
  );
}
