import { memo, useState } from "react";
import {
  BookOpen,
  Check,
  ChevronRight,
  CircleX,
  Code,
  FileDiff,
  FileText,
  Globe,
  LoaderCircle,
  Terminal,
  Wrench,
} from "lucide-react";
import type { Json, Message } from "../types";
import "./read-activity.css";
import { toolLimitNotice } from "./toolLimitNotice";
import MarkdownImage from "./MarkdownImage";
import { toolImages, toolImageDisplayPayload } from "./toolImages";
import FileChangeCard from "./FileChangeCard";

const names: Record<string, string> = {
  commandExecution: "Run command",
  dynamicToolCall: "Tool call",
  mcpToolCall: "Connected tool",
  webSearch: "Search the web",
  fileChange: "File changes",
  imageView: "View image",
  contextCompaction: "Compact context",
  "turn/plan/updated": "Plan",
  "turn/diff/updated": "Changes",
};
const payloads = new WeakMap<Message, { text: string; value: Json }>();
function payload(item: Message): Json {
  const prior = payloads.get(item);
  if (prior?.text === item.text) return prior.value;
  let value: Json = {};
  try {
    const parsed = JSON.parse(item.text);
    if (parsed && typeof parsed === "object") value = parsed;
  } catch {
    /* Plain tool output has no structured fields. */
  }
  payloads.set(item, { text: item.text, value });
  return value;
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
  orchestration_complaint: "Message",
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
        : ["declined", "cancelled", "interrupted"].includes(p.status)
          ? p.status
          : p.status === "completed"
            ? "completed"
            : "recorded")
  );
}
export function isFileChange(item: Message) {
  if (!["tool", "output"].includes(item.role)) return false;
  const p = payload(item);
  return p.type === "fileChange";
}
interface ReadTarget {
  name: string;
  path: string;
  skill: boolean;
}
function readActivity(p: Json): { targets: ReadTarget[]; onlyReads: boolean } {
  const target = (path: string, name?: string, skill = false): ReadTarget => {
    const parts = path.split(/[\\/]/).filter(Boolean);
    const skillFile = /(?:^|[\\/])SKILL\.md$/i.test(path);
    return {
      path,
      name: skillFile
        ? parts.slice(-2).join("/")
        : typeof name === "string" && name
          ? name
          : parts.at(-1) || path,
      skill: skill || skillFile,
    };
  };
  if (p.type === "commandExecution") {
    const actions: Json[] = Array.isArray(p.commandActions)
      ? p.commandActions
      : [];
    return {
      targets: actions
        .filter((a) => a?.type === "read" && typeof a.path === "string")
        .map((a) => target(a.path, a.name)),
      onlyReads: actions.length > 0 && actions.every((a) => a?.type === "read"),
    };
  }
  const tool = String(p.tool || p.type || "");
  const skillRead =
    ["skills/read", "skills.read", "read_skill"].includes(tool) ||
    (tool === "read" && (p.namespace === "skills" || p.server === "skills"));
  const fileRead = [
    "read_file",
    "read_text_file",
    "read_multiple_files",
  ].includes(tool);
  if (!skillRead && !fileRead) return { targets: [], onlyReads: false };
  let args = p.arguments;
  if (typeof args === "string") {
    try {
      args = JSON.parse(args);
    } catch {
      args = {};
    }
  }
  const paths = args?.paths || [
    args?.path || args?.file_path || args?.resource || args?.uri || p.path,
  ];
  const targets = (Array.isArray(paths) ? paths : [paths])
    .filter(
      (path): path is string => typeof path === "string" && path.length > 0,
    )
    .map((path) => target(path, undefined, skillRead));
  // An explicit tool remains visible even when its arguments omit a path.
  return {
    targets: targets.length
      ? targets
      : [target("", args?.name || tool, skillRead)],
    onlyReads: true,
  };
}
function readLabel(targets: ReadTarget[]) {
  const skills = targets.filter((t) => t.skill).length;
  const files = targets.length - skills;
  return [
    files ? `${files === 1 ? "file" : `${files} files`}` : "",
    skills ? `${skills === 1 ? "skill" : `${skills} skills`}` : "",
  ]
    .filter(Boolean)
    .join(" and ");
}
function describe(item: Message, p: Json) {
  const read = readActivity(p);
  const kind = p.type || item.title;
  return {
    ...read,
    label:
      read.onlyReads && read.targets.length
        ? `Read ${readLabel(read.targets)}`
        : toolNames[p.tool] ||
          p.tool ||
          names[kind] ||
          item.title ||
          "Tool activity",
  };
}
export const ToolCard = memo(function ToolCard({
  item,
  agentId,
  cwd,
}: {
  item: Message;
  agentId?: string;
  cwd?: string;
}) {
  const p = payload(item),
    state = status(item, p),
    kind = p.type || item.title,
    read = describe(item, p);
  const Icon =
    read.onlyReads && read.targets.length
      ? read.targets.every((t) => t.skill)
        ? BookOpen
        : FileText
      : kind === "commandExecution"
        ? Terminal
        : kind === "webSearch"
          ? Globe
          : kind === "fileChange"
            ? FileDiff
            : Wrench;
  const limit = toolLimitNotice(item);
  const label = limit?.title || read.label;
  const args = p.arguments;
  const [open, setOpen] = useState(false);
  const images = open ? toolImages(p) : [];
  const display = open ? toolImageDisplayPayload(p) : p;
  const output = open
    ? (display.aggregatedOutput ??
      textResult(
        display.contentItems ?? display.result?.content ?? display.result,
      ))
    : "";
  if (kind === "fileChange")
    return <FileChangeCard item={item} payload={p} status={state} cwd={cwd} />;
  return (
    <details
      className="tool-card"
      data-message={item.id}
      data-tool-status={state}
      data-read-count={read.targets.length || undefined}
      open={open}
      onToggle={(e) => {
        if (e.target === e.currentTarget) setOpen(e.currentTarget.open);
      }}
    >
      <summary>
        <span className="tool-icon">
          <Icon size={15} />
        </span>
        <span className={`tool-title${limit ? " tool-title-limit" : ""}`}>
          <span className={limit ? "activity-limit" : undefined}>{label}</span>
          {limit && (
            <small className="tool-limit-message">{limit.message}</small>
          )}
          {!read.targets.length &&
            (typeof p.command === "string" || typeof p.query === "string") && (
              <small className="tool-read-summary" title={p.command || p.query}>
                {p.command || p.query}
              </small>
            )}
          {read.targets.length > 0 && (
            <small
              className="tool-read-summary"
              title={read.targets.map((t) => t.path || t.name).join("\n")}
            >
              {read.targets.map((t) => t.name).join(", ")}
            </small>
          )}
        </span>
        <span className="tool-meta">
          {typeof p.durationMs === "number" && (
            <small>{(p.durationMs / 1000).toFixed(1)}s</small>
          )}
          <span className="tool-state" title={state} aria-label={state}>
            {state === "running" ? (
              <LoaderCircle size={12} className="spin" />
            ) : state === "failed" ? (
              <CircleX size={12} />
            ) : state === "completed" ? (
              <Check size={12} />
            ) : null}
          </span>
          <ChevronRight size={13} className="tool-chevron" />
        </span>
      </summary>
      {open && (
        <div className="tool-body">
          {images.map((image, index) => (
            <div
              className="tool-section"
              key={`${agentId || ""}:${item.id}:${state}:${image.kind}:${image.kind === "path" ? image.source : index}`}
            >
              <MarkdownImage
                src={image.kind === "inline" ? image.source : ""}
                localPath={image.kind === "path" ? image.source : undefined}
                alt={image.name}
                agentId={agentId}
              />
            </div>
          ))}
          {read.targets.length > 0 && (
            <ul className="tool-read-targets" aria-label="Read targets">
              {read.targets.map((target, i) => (
                <li key={`${target.path}:${i}`}>
                  {target.skill ? (
                    <BookOpen size={14} />
                  ) : (
                    <FileText size={14} />
                  )}
                  <span>
                    <small>{target.skill ? "Skill" : "File"}</small>
                    <code>{target.path || target.name}</code>
                  </span>
                </li>
              ))}
            </ul>
          )}
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
          {Array.isArray(p.changes) &&
            p.changes.map((change: Json, i: number) => (
              <div className="tool-section" key={i}>
                <span>{change?.path || "File change"}</span>
                <pre>{change?.diff || pretty(change)}</pre>
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
              <pre>{pretty(display)}</pre>
            </details>
          )}
          {item.truncated && (
            <p className="notice">This activity is clipped.</p>
          )}
        </div>
      )}
    </details>
  );
});

export function activitySummary(items: Message[]) {
  const files = new Set<string>(),
    skills = new Set<string>(),
    changes = new Set<string>();
  let commands = 0,
    searches = 0,
    other = 0,
    running = 0,
    failed = 0;
  for (const item of items) {
    const p = payload(item),
      read = readActivity(p),
      state = status(item, p);
    if (state === "running") running++;
    if (state === "failed") failed++;
    for (const target of read.targets)
      (target.skill ? skills : files).add(target.path || target.name);
    if (read.onlyReads) continue;
    const kind = p.type || item.title;
    if (kind === "commandExecution") commands++;
    else if (kind === "webSearch") searches++;
    else if (kind === "fileChange" && Array.isArray(p.changes))
      for (const change of p.changes) changes.add(change.path || item.id);
    else other++;
  }
  const count = (n: number, singular: string) =>
    `${n} ${singular}${n === 1 ? "" : "s"}`;
  return {
    running,
    failed,
    label: [
      files.size && `Read ${count(files.size, "file")}`,
      skills.size && `Read ${count(skills.size, "skill")}`,
      changes.size && `Changed ${count(changes.size, "file")}`,
      commands && `Ran ${count(commands, "command")}`,
      searches &&
        `${count(searches, "web search").replace("searchs", "searches")}`,
      other && count(other, "tool call"),
    ]
      .filter(Boolean)
      .join(" · "),
  };
}
export default memo(function Activity({
  items,
  agentId,
}: {
  items: Message[];
  agentId?: string;
}) {
  const summary = activitySummary(items);
  const running = summary.running;
  const reads = items.flatMap((item) => readActivity(payload(item)).targets);
  const failed = summary.failed;
  const [open, setOpen] = useState(() => items.length < 3);
  const [visited, setVisited] = useState(open);
  const hasLimit = items.some((item) => toolLimitNotice(item));
  return (
    <details
      className="tool-group"
      data-running={running}
      data-failed={failed}
      open={open}
      onToggle={(e) => {
        if (e.target === e.currentTarget) {
          setOpen(e.currentTarget.open);
          if (e.currentTarget.open) setVisited(true);
        }
      }}
    >
      <summary>
        {running ? (
          <LoaderCircle size={13} className="spin" />
        ) : (
          <Wrench size={13} />
        )}
        <span>
          {hasLimit && (
            <strong className="activity-limit">Account limit reached · </strong>
          )}
          {items.length === 1
            ? describe(items[0], payload(items[0])).label
            : summary.label}
        </span>
        {items.length === 1 && reads.length > 0 && (
          <span
            className="activity-read-summary"
            title={reads.map((t) => t.path || t.name).join("\n")}
          >
            {reads.map((t) => t.name).join(", ")}
          </span>
        )}
        <span className="activity-state">
          {running > 0 && (
            <span className="activity-running">{running} running</span>
          )}
          {failed > 0 && (
            <span className="activity-failed">{failed} failed</span>
          )}
        </span>
      </summary>
      <div className="activity-list">
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
