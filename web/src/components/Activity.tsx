import { useEffect, useState } from "react";
import { Badge } from "@mantine/core";
import {
  Check,
  ChevronRight,
  CircleX,
  Code,
  FileDiff,
  Globe,
  LoaderCircle,
  Terminal,
  Wrench,
} from "lucide-react";
import type { Json, Message } from "../types";

const names: Record<string, string> = {
  commandExecution: "Run command",
  dynamicToolCall: "Tool call",
  mcpToolCall: "Connected tool",
  webSearch: "Search the web",
  fileChange: "File changes",
  contextCompaction: "Compact context",
  "turn/plan/updated": "Plan",
  "turn/diff/updated": "Changes",
};
function payload(item: Message): Json {
  try {
    const value = JSON.parse(item.text);
    return value && typeof value === "object" ? value : {};
  } catch {
    return {};
  }
}
const toolNames: Record<string, string> = {
  orchestration_spawn: "Create agents",
  orchestration_send: "Send an instruction",
  orchestration_status: "Check the team",
  orchestration_interrupt: "Stop an agent",
  orchestration_monitor: "Monitor a command",
  orchestration_cancel_monitor: "Cancel a monitor",
  orchestration_message: "Message agents",
  orchestration_peers: "Find agents",
  orchestration_chat_read: "Read agent chat",
  orchestration_complaint: "Complaint book",
  orchestration_title: "Name the conversation",
};
const pretty = (value: unknown) =>
  typeof value === "string" ? value : JSON.stringify(value, null, 2);
const textResult = (value: unknown): string => {
  if (Array.isArray(value))
    return value.map((v) => v.text || pretty(v)).join("\n");
  return value == null ? "" : pretty(value);
};
function status(item: Message, p: Json) {
  return (
    item.toolStatus ||
    (p.status === "inProgress"
      ? "running"
      : p.status === "failed" ||
          p.success === false ||
          (p.exitCode != null && p.exitCode !== 0)
        ? "failed"
        : p.status === "completed"
          ? "completed"
          : "recorded")
  );
}
function ToolCard({ item }: { item: Message }) {
  const p = payload(item),
    state = status(item, p),
    kind = p.type || item.title;
  const Icon =
    kind === "commandExecution"
      ? Terminal
      : kind === "webSearch"
        ? Globe
        : kind === "fileChange"
          ? FileDiff
          : Wrench;
  const label =
    toolNames[p.tool] || p.tool || names[kind] || item.title || "Tool activity";
  const output =
    p.aggregatedOutput ??
    textResult(p.contentItems ?? p.result?.content ?? p.result);
  const args = p.arguments;
  const [open, setOpen] = useState(state === "running");
  return (
    <details
      className="tool-card"
      data-tool-status={state}
      open={open}
      onToggle={(e) => setOpen(e.currentTarget.open)}
    >
      <summary>
        <span className="tool-icon">
          <Icon size={15} />
        </span>
        <span className="tool-title">{label}</span>
        <span className="tool-meta">
          {typeof p.durationMs === "number" && (
            <small>{(p.durationMs / 1000).toFixed(1)}s</small>
          )}
          <Badge
            size="xs"
            variant="light"
            color={
              state === "failed"
                ? "red"
                : state === "running"
                  ? "indigo"
                  : "gray"
            }
            leftSection={
              state === "running" ? (
                <LoaderCircle size={10} className="spin" />
              ) : state === "failed" ? (
                <CircleX size={10} />
              ) : state === "completed" ? (
                <Check size={10} />
              ) : undefined
            }
          >
            {state === "completed" ? "Done" : state}
          </Badge>
          <ChevronRight size={13} className="tool-chevron" />
        </span>
      </summary>
      <div className="tool-body">
        {p.command && (
          <div className="tool-section">
            <span>Command</span>
            <pre className="tool-command">{textResult(p.command)}</pre>
          </div>
        )}
        {p.cwd && <div className="tool-directory">{p.cwd}</div>}
        {p.query && (
          <div className="tool-section">
            <span>Search query</span>
            <p>{p.query}</p>
          </div>
        )}
        {args != null && (
          <div className="tool-section">
            <span>Input</span>
            {typeof args === "object" && !Array.isArray(args) ? (
              <dl className="tool-arguments">
                {Object.entries(args).map(([key, value]) => (
                  <div key={key}>
                    <dt>{key}</dt>
                    <dd>{pretty(value)}</dd>
                  </div>
                ))}
              </dl>
            ) : (
              <pre>{pretty(args)}</pre>
            )}
          </div>
        )}
        {p.changes?.map((change: Json, i: number) => (
          <div className="tool-section" key={i}>
            <span>{change.path || "File change"}</span>
            <pre>{change.diff || pretty(change)}</pre>
          </div>
        ))}
        {output && (
          <div className="tool-section">
            <span>Output</span>
            <pre className="tool-output">{output}</pre>
          </div>
        )}
        {p.exitCode != null && (
          <div className={`tool-exit ${p.exitCode !== 0 ? "danger" : ""}`}>
            Exit code {p.exitCode}
          </div>
        )}
        {p.error && (
          <div className="tool-section danger">
            <span>Error</span>
            <pre>{pretty(p.error)}</pre>
          </div>
        )}
        {!Object.keys(p).length && (
          <pre className="tool-output">{item.text}</pre>
        )}
        {!!Object.keys(p).length && (
          <details className="tool-raw">
            <summary>
              <Code size={12} />
              Raw event
            </summary>
            <pre>{pretty(p)}</pre>
          </details>
        )}
        {item.truncated && <p className="notice">This activity is clipped.</p>}
      </div>
    </details>
  );
}
export default function Activity({ items }: { items: Message[] }) {
  const running = items.filter(
    (item) => status(item, payload(item)) === "running",
  ).length;
  const [open, setOpen] = useState(running > 0);
  useEffect(() => {
    if (running) setOpen(true);
  }, [running]);
  return (
    <details
      className="tool-group"
      open={open}
      onToggle={(e) => setOpen(e.currentTarget.open)}
    >
      <summary>
        <Wrench size={13} />
        <span>
          {items.length === 1
            ? names[items[0].title || ""] || items[0].title || "Tool activity"
            : `${items.length} actions`}
        </span>
        {running > 0 && (
          <span className="activity-running">{running} running</span>
        )}
      </summary>
      <div className="activity-list">
        {items.map((item) => (
          <ToolCard key={item.id} item={item} />
        ))}
      </div>
    </details>
  );
}
