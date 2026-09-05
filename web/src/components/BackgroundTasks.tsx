import {
  ActionIcon,
  Badge,
  Button,
  Drawer,
  NativeSelect,
  SegmentedControl,
  TextInput,
  UnstyledButton,
} from "@mantine/core";
import {
  Activity,
  ArrowLeft,
  ArrowUpRight,
  Check,
  ChevronDown,
  Clock,
  Copy,
  LoaderCircle,
  Search,
  Square,
  Terminal,
  Wrench,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api, errorText } from "../api";
import type { Agent, BackgroundTask, Json, Snapshot } from "../types";

export const activeTask = (task: BackgroundTask) =>
  ["running", "starting", "approval"].includes(task.status);
const labels: Record<string, string> = {
  running: "Running",
  starting: "Starting",
  approval: "Needs approval",
  completed: "Completed",
  failed: "Failed",
  cancelled: "Cancelled",
  interrupted: "Interrupted",
  lost: "Outcome unknown",
};
const names: Record<string, string> = {
  commandExecution: "Command",
  webSearch: "Web search",
  mcpToolCall: "Connected tool",
  dynamicToolCall: "Tool call",
  contextCompaction: "Context compaction",
  fileChange: "File changes",
};
const taskName = (task: BackgroundTask) =>
  task.command ||
  task.query ||
  names[task.name || ""] ||
  task.name?.replaceAll("_", " ") ||
  "Tool call";
const duration = (seconds: number) =>
  seconds < 60
    ? `${Math.floor(seconds)}s`
    : seconds < 3600
      ? `${Math.floor(seconds / 60)}m ${Math.floor(seconds % 60)}s`
      : `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
const elapsed = (task: BackgroundTask, now: number) => {
  if (task.durationMs != null) return duration(task.durationMs / 1000);
  if (!activeTask(task) && !task.finished) return "";
  return duration(Math.max(0, (task.finished || now) - task.created));
};
function TaskStatus({ task }: { task: BackgroundTask }) {
  return (
    <Badge
      size="xs"
      variant="light"
      color={
        task.status === "failed"
          ? "red"
          : ["approval", "lost"].includes(task.status)
            ? "yellow"
            : activeTask(task)
              ? "indigo"
              : "gray"
      }
    >
      {labels[task.status] || task.status}
    </Badge>
  );
}
const iconFor = (task: BackgroundTask) =>
  task.kind === "monitor"
    ? Activity
    : task.kind === "command"
      ? Terminal
      : Wrench;
export function backgroundTasks(data: Snapshot | null): BackgroundTask[] {
  return [
    ...(data?.runtime.monitors || []).map(
      (m) => ({ ...m, kind: "monitor" as const }) as BackgroundTask,
    ),
    ...(data?.runtime.tasks || []),
  ];
}

export default function BackgroundTasks({
  opened,
  close,
  data,
  leadId,
  openAgent,
  refresh,
  notify,
}: {
  opened: boolean;
  close: () => void;
  data: Snapshot;
  leadId?: string;
  openAgent: (id: string) => void;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
}) {
  const [tab, setTab] = useState("active"),
    [scope, setScope] = useState("all"),
    [kind, setKind] = useState("all"),
    [query, setQuery] = useState(""),
    [selected, setSelected] = useState<string | null>(null),
    [mobileDetail, setMobileDetail] = useState(false),
    [now, setNow] = useState(Date.now() / 1000);
  useEffect(() => {
    if (!opened) return;
    const timer = setInterval(() => setNow(Date.now() / 1000), 1000);
    return () => clearInterval(timer);
  }, [opened]);
  const agents = data.threads,
    tasks = backgroundTasks(data),
    owner = (id: string) => agents.find((a) => a.id === id);
  const scoped = tasks.filter(
    (t) =>
      (scope !== "team" || !leadId || owner(t.agent)?.rootId === leadId) &&
      (kind === "all" || t.kind === kind),
  );
  const active = scoped.filter(activeTask).length;
  const filtered = scoped
    .filter(
      (t) =>
        (tab === "all" ||
          (tab === "active" ? activeTask(t) : !activeTask(t))) &&
        `${taskName(t)} ${owner(t.agent)?.name} ${t.cwd || ""} ${labels[t.status] || t.status}`
          .toLowerCase()
          .includes(query.toLowerCase()),
    )
    .sort(
      (a, b) =>
        Number(activeTask(b)) - Number(activeTask(a)) || b.created - a.created,
    );
  const selectedTask = scoped.find((t) => t.id === selected) || filtered[0];
  useEffect(() => {
    if (!opened) setMobileDetail(false);
  }, [opened]);
  return (
    <Drawer
      opened={opened}
      onClose={close}
      position="right"
      size={940}
      padding={0}
      title={
        <div className="tasks-title">
          <Activity size={19} />
          <strong>Background tasks</strong>
          <Badge variant="light" color={active ? "indigo" : "gray"} size="sm">
            {active} active
          </Badge>
        </div>
      }
      classNames={{
        content: "tasks-drawer",
        body: "tasks-drawer-body",
        header: "tasks-drawer-header",
      }}
    >
      <div className="tasks-toolbar">
        <SegmentedControl
          aria-label="Task status"
          value={tab}
          onChange={(value) => {
            setTab(value);
            setSelected(null);
            setMobileDetail(false);
          }}
          data={[
            { value: "active", label: "Active" },
            { value: "history", label: "History" },
            { value: "all", label: "All" },
          ]}
          size="xs"
        />
        <div className="tasks-filters">
          <NativeSelect
            aria-label="Task team"
            value={scope}
            onChange={(e) => {
              setScope(e.target.value);
              setSelected(null);
              setMobileDetail(false);
            }}
            data={[
              { value: "all", label: "All teams" },
              ...(leadId ? [{ value: "team", label: "This team" }] : []),
            ]}
            size="xs"
          />
          <NativeSelect
            aria-label="Task type"
            value={kind}
            onChange={(e) => {
              setKind(e.target.value);
              setSelected(null);
              setMobileDetail(false);
            }}
            data={[
              { value: "all", label: "All tools" },
              { value: "monitor", label: "Monitors" },
              { value: "command", label: "Commands" },
              { value: "tool", label: "Other tools" },
            ]}
            size="xs"
          />
        </div>
      </div>
      <div
        className={`tasks-content ${mobileDetail && selectedTask ? "show-task-detail" : ""}`}
      >
        <section className="tasks-list" aria-label="Task list">
          <div className="tasks-search">
            <TextInput
              size="sm"
              aria-label="Find a task"
              placeholder="Search commands or agents"
              leftSection={<Search size={14} />}
              value={query}
              onChange={(e) => {
                setQuery(e.target.value);
                setSelected(null);
                setMobileDetail(false);
              }}
            />
          </div>
          <div className="tasks-rows">
            {filtered.map((task) => {
              const Icon = iconFor(task);
              return (
                <UnstyledButton
                  key={task.id}
                  data-task={task.id}
                  className={`task-row ${selectedTask?.id === task.id ? "selected" : ""}`}
                  aria-pressed={selectedTask?.id === task.id}
                  onClick={() => {
                    setSelected(task.id);
                    setMobileDetail(true);
                  }}
                >
                  <div className="task-row-heading">
                    <span
                      className={`task-kind-icon ${activeTask(task) ? "active" : ""}`}
                    >
                      <Icon size={16} />
                    </span>
                    <span className="task-row-kind">
                      {task.kind === "monitor"
                        ? "Monitor"
                        : task.kind === "command"
                          ? "Command"
                          : "Tool"}
                    </span>
                    <span className="task-age">{elapsed(task, now)}</span>
                  </div>
                  <strong className={task.command ? "task-command-title" : ""}>
                    {taskName(task)}
                  </strong>
                  <div className="task-row-footer">
                    <span>{owner(task.agent)?.name || "Agent"}</span>
                    <TaskStatus task={task} />
                  </div>
                </UnstyledButton>
              );
            })}
            {!filtered.length && (
              <div className="tasks-empty">
                <Check size={24} />
                <strong>
                  {query
                    ? "No matching tasks"
                    : tab === "active"
                      ? "No active tasks"
                      : "No task history"}
                </strong>
                <p>
                  {query
                    ? "Try another command or agent name."
                    : "Commands, monitors and tool calls appear here as agents use them."}
                </p>
              </div>
            )}
          </div>
          <p className="tasks-footnote">
            All monitors · Latest {data.runtime.tasksHistoryLimit || 100}{" "}
            finished tool calls
          </p>
        </section>
        {selectedTask ? (
          <TaskDetail
            key={selectedTask.id}
            task={selectedTask}
            opened={opened}
            owner={owner(selectedTask.agent)}
            now={now}
            requests={data.runtime.requests}
            back={() => setMobileDetail(false)}
            openAgent={() => {
              close();
              openAgent(selectedTask.agent);
            }}
            refresh={refresh}
            notify={notify}
          />
        ) : (
          <div className="task-detail-empty">
            <Terminal size={32} />
            <p>Select a task to inspect its output.</p>
          </div>
        )}
      </div>
    </Drawer>
  );
}

function TaskDetail({
  task: summary,
  opened,
  owner,
  now,
  requests,
  back,
  openAgent,
  refresh,
  notify,
}: {
  task: BackgroundTask;
  opened: boolean;
  owner?: Agent;
  now: number;
  requests: Json[];
  back: () => void;
  openAgent: () => void;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
}) {
  const [detail, setDetail] = useState<BackgroundTask | null>(null),
    [loadError, setLoadError] = useState("");
  useEffect(() => {
    if (!opened || summary.kind === "monitor") return;
    let stopped = false;
    let timer: ReturnType<typeof setTimeout>;
    const load = async () => {
      try {
        const value = await api<BackgroundTask>(
          "/api/task?id=" + encodeURIComponent(summary.id),
        );
        if (!stopped) {
          setDetail(value);
          setLoadError("");
        }
      } catch (error) {
        if (!stopped) setLoadError(errorText(error));
      }
      if (!stopped) timer = setTimeout(load, 1600);
    };
    void load();
    return () => {
      stopped = true;
      clearTimeout(timer);
    };
  }, [opened, summary.id, summary.kind]);
  const task =
    detail && detail.id === summary.id ? { ...detail, ...summary } : summary;
  const [pending, setPending] = useState(false),
    [follow, setFollow] = useState(true),
    [copied, setCopied] = useState(false);
  const output = useRef<HTMLPreElement>(null);
  useEffect(() => {
    if (follow && output.current)
      output.current.scrollTop = output.current.scrollHeight;
  }, [task.tail, follow]);
  const act = async (path: string, body: Json) => {
    setPending(true);
    try {
      const result = await api(path, body);
      if (result?.error) notify(result.error);
      await refresh();
    } catch (error) {
      notify(errorText(error));
    } finally {
      setPending(false);
    }
  };
  const request = requests.find(
    (r) => r.method === "monitor/approve" && r.params?.monitorId === task.id,
  );
  const Icon = iconFor(task),
    age = elapsed(task, now);
  return (
    <section
      className="task-detail"
      aria-label="Task details"
      data-task-detail={task.id}
    >
      <div className="task-detail-top">
        <Button
          className="task-back"
          size="compact-xs"
          leftSection={<ArrowLeft size={14} />}
          onClick={back}
        >
          Tasks
        </Button>
        <span>
          <Icon size={15} />
          {task.kind === "monitor"
            ? "Command monitor"
            : task.kind === "command"
              ? "Command"
              : "Tool call"}
        </span>
        <TaskStatus task={task} />
      </div>
      <div className="task-detail-scroll">
        <h2 className={task.command ? "task-command-title" : ""}>
          {taskName(task)}
        </h2>
        <Button
          size="compact-xs"
          className="task-owner"
          rightSection={<ArrowUpRight size={13} />}
          onClick={openAgent}
        >
          {owner?.name || "Open agent"}
        </Button>
        <div className="task-facts">
          {age && (
            <span>
              <Clock size={12} />
              {age}
            </span>
          )}
          {task.exitCode != null && (
            <span className={task.exitCode !== 0 ? "task-failed" : ""}>
              Exit {task.exitCode}
            </span>
          )}
          {task.timeout_ms != null && (
            <span>Timeout {duration(task.timeout_ms / 1000)}</span>
          )}
          {task.processId && <span>Process {task.processId}</span>}
        </div>
        {task.cwd && <p className="task-cwd">{task.cwd}</p>}
        {task.kind === "monitor" && activeTask(task) && (
          <p className="task-note">
            {task.status === "approval"
              ? "The command waits for your approval."
              : "The agent receives the result when this command exits."}
          </p>
        )}
        {task.arguments && (
          <details className="task-input">
            <summary>Tool input</summary>
            <pre>{task.arguments}</pre>
          </details>
        )}
        {loadError && (
          <p role="alert" className="task-error">
            Output unavailable: {loadError}
          </p>
        )}
        {task.error && (
          <p role="alert" className="task-error">
            {task.error}
          </p>
        )}
        <div className="task-terminal">
          <div className="task-terminal-header">
            <span>
              {activeTask(task) ? (
                <LoaderCircle size={12} className="spin" />
              ) : (
                <Terminal size={12} />
              )}
              Output
            </span>
            <div>
              <Button
                size="compact-xs"
                aria-pressed={follow}
                onClick={() => setFollow(!follow)}
                leftSection={<ChevronDown size={12} />}
                color={follow ? "indigo" : "gray"}
              >
                Follow
              </Button>
              <ActionIcon
                size="sm"
                aria-label="Copy task output"
                disabled={!task.tail}
                onClick={() => {
                  void navigator.clipboard
                    .writeText(task.tail || "")
                    .then(() => setCopied(true))
                    .catch(() => notify("Could not copy output"));
                }}
              >
                {copied ? <Check size={13} /> : <Copy size={13} />}
              </ActionIcon>
            </div>
          </div>
          <pre
            ref={output}
            className={`task-output ${!task.tail ? "empty" : ""}`}
            onScroll={(e) => {
              const el = e.currentTarget;
              if (el.scrollHeight - el.scrollTop - el.clientHeight > 35)
                setFollow(false);
            }}
          >
            {task.tail ||
              (activeTask(task)
                ? "Waiting for output…"
                : "No output recorded.")}
          </pre>
          {(task.outputTruncated ||
            (task.bytes || 0) >
              new TextEncoder().encode(task.tail || "").length) && (
            <p className="task-output-limit">
              Latest output shown
              {task.log ? ". The saved log is available below." : "."}
            </p>
          )}
        </div>
        <dl className="task-record">
          <div>
            <dt>Created</dt>
            <dd>{new Date(task.created * 1000).toLocaleString()}</dd>
          </div>
          {task.finished && (
            <div>
              <dt>Finished</dt>
              <dd>{new Date(task.finished * 1000).toLocaleString()}</dd>
            </div>
          )}
          {task.log && (
            <div>
              <dt>Saved log</dt>
              <dd>{task.log}</dd>
            </div>
          )}
        </dl>
      </div>
      <div className="task-actions">
        {request && task.status === "approval" && (
          <Button
            size="xs"
            variant="light"
            loading={pending}
            onClick={() =>
              void act("/api/answer", { id: request.id, decision: "accept" })
            }
          >
            Approve command
          </Button>
        )}
        {task.kind === "monitor" && activeTask(task) && (
          <Button
            size="xs"
            color="red"
            leftSection={<Square size={12} />}
            loading={pending}
            onClick={() => void act("/api/monitor/cancel", { id: task.id })}
          >
            Cancel monitor
          </Button>
        )}
        <Button
          size="xs"
          rightSection={<ArrowUpRight size={13} />}
          onClick={openAgent}
        >
          Open agent
        </Button>
      </div>
    </section>
  );
}
