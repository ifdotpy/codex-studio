import { localDateTime } from "../../local-time";
import ErrorDescription from "../ErrorDescription";
import { displayError } from "../../errorPresentation";
import {
  ActionIcon,
  Badge,
  Button,
  Drawer,
  NativeSelect,
  NumberInput,
  Text,
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
  Download,
  LoaderCircle,
  Search,
  Square,
  Terminal,
  Wrench,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import {
  get,
  post,
  apiDownload,
  errorText,
  type ApiPostPath,
  type PostBody,
} from "../../api";
import type { paths } from "../../generated/api";
import "./background-controls.css";
import type { Agent, JsonValue, Monitor, Request, Snapshot } from "../../types";
import { watchResourceReads } from "../watchResourceReads";
import { copyText } from "../../clipboard/clipboard";
import {
  activeTask,
  backgroundTasks,
  projectTaskForRenderer,
  type DisplayBackgroundTask,
} from "../backgroundTaskModel";

type BackgroundAction = <Path extends ApiPostPath>(
  path: Path,
  body: PostBody<Path>,
) => Promise<boolean>;
type TaskDetailResponse =
  paths["/api/task"]["get"]["responses"][200]["content"]["application/json"];
type MonitorTask = Monitor;
type PendingRequest = Request;

export function monitorTask(
  monitor: MonitorTask,
): DisplayBackgroundTask | null {
  return projectTaskForRenderer(monitor, "monitor");
}

function isJsonObject(
  value: JsonValue | null | undefined,
): value is Record<string, JsonValue> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

export { activeTask, backgroundTasks } from "../backgroundTaskModel";
const labels: Record<string, string> = {
  running: "Running",
  starting: "Starting",
  pending: "Pending",
  stopping: "Stopping",
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
const taskName = (task: DisplayBackgroundTask) =>
  task.command ||
  ("query" in task ? task.query : undefined) ||
  names[task.name || ""] ||
  task.name?.replaceAll("_", " ") ||
  "Tool call";
const duration = (seconds: number) =>
  seconds < 60
    ? `${Math.floor(seconds)}s`
    : seconds < 3600
      ? `${Math.floor(seconds / 60)}m ${Math.floor(seconds % 60)}s`
      : `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
const elapsed = (task: DisplayBackgroundTask, now: number) => {
  if (task.durationMs != null) return duration(task.durationMs / 1000);
  if (!activeTask(task) && !task.finished) return "";
  return duration(Math.max(0, (task.finished || now) - task.created));
};
function TaskStatus({ task }: { task: DisplayBackgroundTask }) {
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
      {activeTask(task) && task.cancelRequested
        ? "Waiting for exit"
        : labels[task.status] || task.status}
    </Badge>
  );
}
const iconFor = (task: DisplayBackgroundTask) =>
  task.kind === "monitor"
    ? Activity
    : task.kind === "command"
      ? Terminal
      : Wrench;
const taskKindLabel = (task: DisplayBackgroundTask, owner?: Agent) => {
  if (task.kind === "monitor") return "Monitor";
  if (task.kind !== "command") return "Tool";
  const outsideTurn =
    "turnId" in task &&
    task.turnId &&
    owner &&
    (owner.inFlight === false ||
      (owner.inFlight === true &&
        owner.turnId &&
        owner.turnId !== task.turnId));
  return outsideTurn ? "Background command" : "Command";
};
export default function BackgroundTasks({
  opened,
  close,
  afterClose,
  data,
  leadId,
  openAgent,
  refresh,
  notify,
  initialFocus,
}: {
  opened: boolean;
  close: () => void;
  afterClose?: () => void;
  data: Snapshot;
  leadId?: string;
  openAgent: (id: string) => void;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
  initialFocus?: { id: string; leadId: string; requestId: string };
}) {
  const [kind, setKind] = useState("all"),
    [query, setQuery] = useState(""),
    [selection, setSelection] = useState<{
      scope?: string;
      id: string;
      requested?: boolean;
    } | null>(null),
    [mobileDetail, setMobileDetail] = useState(false),
    [now, setNow] = useState(Date.now() / 1000);
  const appliedFocus = useRef<string | undefined>(undefined);
  useEffect(() => {
    if (!opened) return;
    const timer = setInterval(() => setNow(Date.now() / 1000), 1000);
    return () => clearInterval(timer);
  }, [opened]);
  const agents = data.threads,
    tasks = backgroundTasks(data).filter(activeTask),
    owner = (id: string) => agents.find((a) => a.id === id);
  const scoped = tasks.filter(
    (t) =>
      !!leadId &&
      (t.agent === leadId || owner(t.agent)?.rootId === leadId) &&
      (kind === "all" || t.kind === kind),
  );
  const active = scoped.length;
  const filtered = scoped
    .filter((t) =>
      `${taskName(t)} ${owner(t.agent)?.name} ${t.cwd || ""} ${labels[t.status] || t.status}`
        .toLowerCase()
        .includes(query.toLowerCase()),
    )
    .sort((a, b) => b.created - a.created);
  const pendingFocus =
    opened &&
    initialFocus?.leadId === leadId &&
    initialFocus?.requestId !== appliedFocus.current
      ? initialFocus
      : undefined;
  const explicitId =
    pendingFocus?.id ||
    (selection?.scope === leadId && selection?.requested
      ? selection.id
      : undefined);
  const selectedTask = explicitId
    ? scoped.find((task) => task.id === explicitId)
    : (selection && selection.scope === leadId
        ? scoped.find((task) => task.id === selection.id)
        : undefined) || filtered[0];
  const selectedId = selectedTask?.id;
  useEffect(() => {
    if (explicitId && !selectedId) return;
    setSelection((previous) =>
      selectedId
        ? previous && previous.scope === leadId && previous.id === selectedId
          ? previous
          : { scope: leadId, id: selectedId }
        : null,
    );
  }, [leadId, selectedId, selection, explicitId]);
  useEffect(() => {
    setMobileDetail(false);
  }, [leadId]);
  useEffect(() => {
    if (!opened) setMobileDetail(false);
  }, [opened]);
  useEffect(() => {
    if (!opened || !initialFocus || initialFocus.leadId !== leadId) return;
    appliedFocus.current = initialFocus.requestId;
    setSelection({ scope: leadId, id: initialFocus.id, requested: true });
    setKind("all");
    setQuery("");
    setMobileDetail(true);
  }, [opened, initialFocus?.requestId, leadId]);
  return (
    <Drawer
      opened={opened}
      onClose={close}
      onExitTransitionEnd={afterClose}
      returnFocus={!afterClose}
      position="right"
      size={940}
      padding={0}
      title={
        <div className="tasks-title">
          <Activity size={19} />
          <strong>Current activity</strong>
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
        <div className="tasks-filters">
          <NativeSelect
            aria-label="Task type"
            value={kind}
            onChange={(e) => {
              setKind(e.target.value);
              setSelection(null);
              setMobileDetail(false);
            }}
            data={[
              { value: "all", label: "All" },
              { value: "command", label: "Commands" },
              { value: "monitor", label: "Monitors" },
              { value: "tool", label: "Other tools" },
            ]}
            size="xs"
          />
        </div>
        <span className="tasks-filter-help">
          Commands run in the chat. Monitors run in the background. Other tools
          show tool results.
        </span>
      </div>
      <div
        className={`tasks-content ${mobileDetail && (selectedTask || explicitId) ? "show-task-detail" : ""}`}
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
                setSelection(null);
                setMobileDetail(false);
              }}
            />
          </div>
          <div className="tasks-rows">
            {(
              [
                ["command", "Commands"],
                ["monitor", "Monitors"],
                ["tool", "Other tools"],
              ] as const
            ).map(([groupKind, label]) => {
              const groupTasks = filtered.filter(
                (task) => task.kind === groupKind,
              );
              return groupTasks.length ? (
                <section key={groupKind} aria-label={label}>
                  <Text
                    component="h3"
                    size="xs"
                    c="dimmed"
                    fw={600}
                    px="sm"
                    py="xs"
                    m={0}
                  >
                    {label}
                  </Text>
                  {groupTasks.map((task) => {
                    const Icon = iconFor(task);
                    return (
                      <UnstyledButton
                        key={task.id}
                        data-task={task.id}
                        className={`task-row ${selectedTask?.id === task.id ? "selected" : ""}`}
                        aria-pressed={selectedTask?.id === task.id}
                        onClick={() => {
                          setSelection({ scope: leadId, id: task.id });
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
                            {taskKindLabel(task, owner(task.agent))}
                          </span>
                          <span className="task-age">{elapsed(task, now)}</span>
                        </div>
                        <strong
                          className={task.command ? "task-command-title" : ""}
                        >
                          {taskName(task)}
                        </strong>
                        <div className="task-row-footer">
                          <span>{owner(task.agent)?.name || "Agent"}</span>
                          <TaskStatus task={task} />
                        </div>
                      </UnstyledButton>
                    );
                  })}
                </section>
              ) : null;
            })}
            {!filtered.length && (
              <div className="tasks-empty">
                <Check size={24} />
                <strong>
                  {query ? "No matching tasks" : "No active tasks"}
                </strong>
                <p>
                  {query
                    ? "Try another command or agent name."
                    : "Agent commands and tool calls appear here."}
                </p>
              </div>
            )}
          </div>
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
            <p role="status">
              {explicitId
                ? "The selected task is no longer active in this chat."
                : "Select a task to inspect its output."}
            </p>
            {explicitId && (
              <Button variant="subtle" onClick={() => setMobileDetail(false)}>
                Back to tasks
              </Button>
            )}
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
  task: DisplayBackgroundTask;
  opened: boolean;
  owner?: Agent;
  now: number;
  requests: PendingRequest[];
  back: () => void;
  openAgent: () => void;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
}) {
  const [detail, setDetail] = useState<TaskDetailResponse | null>(null),
    [loadError, setLoadError] = useState("");
  useEffect(() => {
    if (!opened || summary.kind === "monitor") return;
    let stopped = false;
    const stop = watchResourceReads(
      { kind: "task", taskId: summary.id },
      async () => {
        const value = await get("/api/task", { query: { id: summary.id } });
        if (!stopped) {
          setDetail(value);
          setLoadError("");
        }
      },
      (error) => {
        if (!stopped) setLoadError(errorText(error));
      },
    );
    return () => {
      stopped = true;
      stop();
    };
  }, [opened, summary.id, summary.kind]);
  const resolvedDetail = detail?.id === summary.id ? detail : null;
  const task = resolvedDetail ? { ...resolvedDetail, ...summary } : summary;
  const monitor = task.kind === "monitor" && "tail" in task ? task : null;
  const outputTail = resolvedDetail?.tail ?? monitor?.tail;
  const outputError = resolvedDetail?.error ?? monitor?.error;
  const [pending, setPending] = useState(false),
    [follow, setFollow] = useState(true),
    [copied, setCopied] = useState(false);
  const output = useRef<HTMLPreElement>(null);
  useEffect(() => {
    if (follow && output.current)
      output.current.scrollTop = output.current.scrollHeight;
  }, [outputTail, follow]);
  const act: BackgroundAction = async <Path extends ApiPostPath>(
    path: Path,
    body: PostBody<Path>,
  ) => {
    setPending(true);
    try {
      const result = await post(path, body);
      await refresh();
      if (
        result &&
        typeof result === "object" &&
        "error" in result &&
        result.error
      )
        throw new Error(displayError(result.error));
      return true;
    } catch (error) {
      notify(errorText(error));
      return false;
    } finally {
      setPending(false);
    }
  };
  const request = requests.find(
    (r) =>
      r.method === "monitor/approve" &&
      isJsonObject(r.params) &&
      r.params.monitorId === task.id,
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
          Activity
        </Button>
        <span>
          <Icon size={15} />
          {taskKindLabel(task, owner)}
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
        {resolvedDetail?.arguments && (
          <details className="task-input">
            <summary>Tool input</summary>
            <pre>{resolvedDetail.arguments}</pre>
          </details>
        )}
        {loadError && (
          <p role="alert" className="task-error">
            Output unavailable: {loadError}
          </p>
        )}
        {outputError && (
          <p role="alert" className="task-error">
            <ErrorDescription value={outputError} />
          </p>
        )}
        {task.stdinError && (
          <p role="alert" className="task-error">
            <ErrorDescription value={task.stdinError} />
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
                aria-label="Download task log"
                disabled={pending}
                onClick={() => void downloadLog(task, resolvedDetail, notify)}
              >
                <Download size={13} />
              </ActionIcon>
              <ActionIcon
                size="sm"
                aria-label="Copy task output"
                disabled={!outputTail}
                onClick={() => {
                  void copyText(outputTail || "")
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
            className={`task-output ${!outputTail ? "empty" : ""}`}
            onScroll={(e) => {
              const el = e.currentTarget;
              if (el.scrollHeight - el.scrollTop - el.clientHeight > 35)
                setFollow(false);
            }}
          >
            {outputTail ||
              (activeTask(task)
                ? "Waiting for output…"
                : "No output recorded.")}
          </pre>
          {(task.outputTruncated ||
            (task.bytes || 0) >
              new TextEncoder().encode(outputTail || "").length) && (
            <p className="task-output-limit">
              Latest output shown
              {task.log ? ". The saved log is available below." : "."}
            </p>
          )}
        </div>
        {task.status === "running" &&
          ((task.kind === "monitor" && task.interactive) ||
            (task.kind === "command" && task.processId)) && (
            <ProcessInput task={task} pending={pending} act={act} />
          )}
        <dl className="task-record">
          <div>
            <dt>Created</dt>
            <dd>{localDateTime(new Date(task.created * 1000))}</dd>
          </div>
          {task.finished && (
            <div>
              <dt>Finished</dt>
              <dd>{localDateTime(new Date(task.finished * 1000))}</dd>
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
            disabled={!!task.cancelRequested}
            onClick={() => void act("/api/monitor/cancel", { id: task.id })}
          >
            {task.cancelRequested ? "Stop requested" : "Cancel monitor"}
          </Button>
        )}
      </div>
    </section>
  );
}

function ProcessInput({
  task,
  pending,
  act,
}: {
  task: DisplayBackgroundTask;
  pending: boolean;
  act: BackgroundAction;
}) {
  const [input, setInput] = useState("");
  const [rows, setRows] = useState<string | number>(24);
  const [cols, setCols] = useState<string | number>(80);
  const [inputClosed, setStdinClosed] = useState(false);
  const stdinClosed = inputClosed || !!task.stdinClosed;
  const stdinUnavailable =
    stdinClosed || !!task.stdinCloseRequested || !!task.cancelRequested;
  const native = task.kind === "command";
  const send = async () => {
    const sent = input;
    const ok = native
      ? await act("/api/native-command", {
          id: task.id,
          action: "input",
          text: sent + "\n",
        })
      : await act("/api/monitor/input", {
          id: task.id,
          text: sent + "\n",
        });
    if (ok) setInput((current) => (current === sent ? "" : current));
  };
  return (
    <section className="process-input" aria-label="Command controls">
      <form
        onSubmit={(e) => {
          e.preventDefault();
          void send();
        }}
        className="process-input-line"
      >
        <TextInput
          aria-label="Terminal input"
          placeholder="Type terminal input"
          value={input}
          onChange={(e) => setInput(e.target.value)}
          disabled={pending || stdinUnavailable}
          maxLength={16000}
        />
        <Button size="sm" type="submit" disabled={pending || stdinUnavailable}>
          {native ? "Send via agent" : "Send line"}
        </Button>
      </form>
      <div className="process-input-actions">
        {native ? (
          <Button
            size="compact-xs"
            color="red"
            disabled={pending}
            onClick={() =>
              void act("/api/native-command", { id: task.id, action: "cancel" })
            }
          >
            Stop command
          </Button>
        ) : (
          <>
            <Button
              size="compact-xs"
              disabled={pending || stdinUnavailable}
              onClick={() =>
                void act("/api/monitor/input", { id: task.id, text: "\u0003" })
              }
            >
              Ctrl+C
            </Button>
            <Button
              size="compact-xs"
              disabled={pending || stdinUnavailable}
              onClick={() => {
                void act("/api/monitor/input", {
                  id: task.id,
                  closeStdin: true,
                }).then((ok) => {
                  if (ok) setStdinClosed(true);
                });
              }}
            >
              {stdinClosed
                ? "Input closed"
                : task.stdinCloseRequested
                  ? "Closing input…"
                  : "Close input (EOF)"}
            </Button>
            <details className="process-resize">
              <summary>Terminal size</summary>
              <div>
                <NumberInput
                  label="Rows"
                  size="xs"
                  value={rows}
                  onChange={setRows}
                  min={1}
                  max={500}
                  allowDecimal={false}
                />
                <NumberInput
                  label="Columns"
                  size="xs"
                  value={cols}
                  onChange={setCols}
                  min={1}
                  max={500}
                  allowDecimal={false}
                />
                <Button
                  size="xs"
                  disabled={
                    pending ||
                    !!task.cancelRequested ||
                    !Number(rows) ||
                    !Number(cols)
                  }
                  onClick={() =>
                    void act("/api/monitor/input", {
                      id: task.id,
                      rows: Number(rows),
                      cols: Number(cols),
                    })
                  }
                >
                  Resize
                </Button>
              </div>
            </details>
          </>
        )}
      </div>
    </section>
  );
}

async function downloadLog(
  task: DisplayBackgroundTask,
  detail: TaskDetailResponse | null,
  notify: (s: string) => void,
) {
  try {
    let blob: Blob, name: string;
    if (task.kind === "monitor") {
      const result = await apiDownload("/api/monitor/log", { id: task.id });
      blob = result.blob;
      name = result.name;
      if (result.truncated)
        notify("The download contains the retained part of the log.");
    } else {
      blob = new Blob([detail?.tail || ""], { type: "text/plain" });
      name = `command-${task.id}.log`;
      if (task.outputTruncated)
        notify("The download contains the retained output.");
    }
    const url = URL.createObjectURL(blob);
    const link = document.createElement("a");
    link.href = url;
    link.download = name;
    link.click();
    setTimeout(() => URL.revokeObjectURL(url), 10000);
  } catch (error) {
    notify(errorText(error));
  }
}
