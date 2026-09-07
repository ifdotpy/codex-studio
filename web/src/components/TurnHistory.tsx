import { Fragment, useEffect, useState, type ReactNode } from "react";
import {
  Check,
  ChevronRight,
  CircleAlert,
  CircleStop,
  Layers,
} from "lucide-react";
import { save, saved } from "../api";
import type { Message } from "../types";
import Activity from "./Activity";
import ConversationResults from "./ConversationResults";
import {
  historyGroups,
  resultExcerpt,
  type HistoryGroup,
} from "./turnHistoryModel";
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
  latest,
  storageKey,
  render,
  agentId,
  onJump,
}: {
  group: HistoryGroup;
  latest: boolean;
  storageKey: string;
  render: (message: Message) => ReactNode;
  agentId?: string;
  onJump: (id: string) => void;
}) {
  const [open, setOpen] = useState(
    () => saved<Record<string, boolean>>(storageKey, {})[group.id] ?? latest,
  );
  const updateOpen = (value: boolean) => {
    setOpen(value);
    const prior = saved<Record<string, boolean>>(storageKey, {});
    save(
      storageKey,
      Object.fromEntries([
        ...Object.entries(prior)
          .filter(([id]) => id !== group.id)
          .slice(-499),
        [group.id, value],
      ]),
    );
  };
  const result = group.result;
  const earlier = group.items.filter((item) => item.id !== result?.id);
  const tools = group.items.filter((item) =>
    ["tool", "output"].includes(item.role),
  ).length;
  const label =
    group.outcome === "failed"
      ? "Turn failed"
      : group.outcome === "interrupted"
        ? "Turn interrupted"
        : group.outcome === "completed"
          ? "Turn complete"
          : "Turn ended";
  const Icon =
    group.outcome === "failed"
      ? CircleAlert
      : group.outcome === "interrupted"
        ? CircleStop
        : group.outcome === "completed"
          ? Check
          : Layers;
  return (
    <section
      className="turn-history"
      data-turn={group.items[0].turnId}
      data-outcome={group.outcome}
    >
      <details
        className="turn-result"
        open={open}
        onToggle={(event) => {
          if (event.target !== event.currentTarget) return;
          updateOpen(event.currentTarget.open);
        }}
      >
        <summary
          onClick={(event) => {
            event.preventDefault();
            updateOpen(!open);
          }}
          aria-label={`${label}: ${result ? resultExcerpt(result.text) : "No final text"}`}
        >
          <Icon size={15} className="turn-outcome-icon" />
          <span className="turn-result-copy">
            <span className="turn-result-label">
              {label}
              {tools > 0 && (
                <span>
                  {tools} tool {tools === 1 ? "call" : "calls"}
                </span>
              )}
            </span>
            {!open && (
              <span className="turn-result-excerpt">
                {result
                  ? resultExcerpt(result.text)
                  : "No final text was recorded."}
              </span>
            )}
          </span>
          <ChevronRight size={15} className="turn-expand-icon" />
        </summary>
        <div className="turn-result-body">
          {result ? (
            render(result)
          ) : (
            <p className="notice">No final text was recorded.</p>
          )}
          {earlier.length > 0 && (
            <details className="turn-work">
              <summary>
                Work before this result <span>{earlier.length} records</span>
              </summary>
              {messages(earlier, render)}
            </details>
          )}
        </div>
      </details>
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
  const groups = historyGroups(items, currentTurn);
  const latest = groups.filter((group) => group.outcome).at(-1)?.id;
  return (
    <>
      {groups.map((group) => (
        <StableTurn
          key={group.id}
          group={group}
          latest={group.id === latest}
          storageKey={storageKey}
          render={renderMessage}
          agentId={agentId}
          onJump={onJump}
        />
      ))}
    </>
  );
}

function StableTurn(props: Parameters<typeof Turn>[0]) {
  // Historical turns start compact. A turn seen live keeps its message nodes
  // and chronological order until the reader explicitly asks to collapse it.
  const layoutKey = `${props.storageKey}:layout`;
  const [compact, setCompact] = useState(
    () =>
      !!props.group.outcome &&
      saved<Record<string, boolean>>(layoutKey, {})[props.group.id] !== false,
  );
  useEffect(() => {
    if (!props.group.items[0].turnId || props.group.items[0].role === "user")
      return;
    const prior = saved<Record<string, boolean>>(layoutKey, {});
    save(
      layoutKey,
      Object.fromEntries([
        ...Object.entries(prior)
          .filter(([id]) => id !== props.group.id)
          .slice(-499),
        [props.group.id, compact],
      ]),
    );
  }, [layoutKey, props.group.id, compact]);
  if (compact && props.group.outcome) return <Turn {...props} />;
  return (
    <>
      {messages(props.group.items, props.render)}
      {props.group.items[0].role !== "user" && (
        <ConversationResults
          messages={props.group.items}
          agentId={props.agentId}
          onJump={props.onJump}
        />
      )}
      {props.group.outcome && (
        <button
          className="turn-collapse"
          onClick={() => {
            save(props.storageKey, {
              ...saved(props.storageKey, {}),
              [props.group.id]: false,
            });
            setCompact(true);
          }}
        >
          <Layers size={14} /> Collapse this turn
        </button>
      )}
    </>
  );
}
