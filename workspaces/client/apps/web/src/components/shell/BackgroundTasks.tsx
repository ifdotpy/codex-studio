import { useSubscription } from "../../sync/subscriptions";
import {
  useServerApi,
  useServerServices,
} from "../../servers/ServerServicesContext";
import { localDateTime } from "../../local-time";
import ErrorDescription from "../ErrorDescription";
import { displayError } from "../../errorPresentation";
import {
  ActionIcon,
  Badge,
  Button,
  Checkbox,
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
  apiDownload,
  ApiError,
  errorText,
  type ApiPostPath,
  type PostBody,
  type PostResult,
} from "../../api";
import type { paths } from "../../generated/api";
import "./background-controls.css";
import MonitorRestart from "./MonitorRestart";
import {
  CHAT_INDICATOR_URGENCY,
  type ChatIndicatorKind,
} from "../chat-status/chatStatusModel";
import type { Agent, JsonValue, Monitor, Request, Snapshot } from "../../types";
import { copyText } from "../../clipboard/clipboard";
import {
  activeTask,
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
type ManagedProcess =
  paths["/api/processes"]["get"]["responses"][200]["content"]["application/json"]["processes"][number];
type ProcessResponseTarget = NonNullable<
  paths["/api/processes"]["get"]["responses"][200]["content"]["application/json"]["responseTarget"]
>;
export const backgroundProcessGroups = [
  ["command", "Commands"],
  ["monitor", "Monitors"],
  ["tool", "Other tools"],
  ["backgroundTask", "Background tasks"],
] as const;
export const responseStopIdentity = (
  target: ProcessResponseTarget | null | undefined,
) => (target ? { responseTarget: target } : {});
export const responseOwnerIdentityKey = (
  owner:
    | {
        id?: string;
        accountKey?: string | null;
        epoch?: number | null;
        inFlight?: boolean | null;
        threadId?: string | null;
        turnId?: string | null;
      }
    | undefined,
) =>
  JSON.stringify(
    owner
      ? [
          owner.id,
          owner.accountKey,
          owner.epoch,
          owner.inFlight,
          owner.threadId,
          owner.turnId,
        ]
      : null,
  );

function displayProcess(process: ManagedProcess): DisplayBackgroundTask {
  return {
    ...process,
    kind:
      process.source === "monitor"
        ? "monitor"
        : process.source === "codex_task"
          ? "command"
          : "backgroundTask",
    tail: process.output || "",
    interactive: process.capabilities.sendInput,
    processKey: process.key,
  };
}

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
export function backgroundTaskState(
  task: DisplayBackgroundTask,
): ChatIndicatorKind {
  if (task.status === "approval") return "answer";
  if (["failed", "lost"].includes(task.status)) return "error";
  if (activeTask(task)) return "working";
  if (task.status === "completed") return "unread";
  return "paused";
}
function TaskStatus({ task }: { task: DisplayBackgroundTask }) {
  return (
    <span className="task-status">
      {activeTask(task) && task.cancelRequested
        ? "Waiting for exit"
        : labels[task.status] || task.status}
    </span>
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
  const outsideTurn = Boolean(
    "turnId" in task &&
    task.turnId &&
    owner &&
    (owner.inFlight === false ||
      (owner.inFlight === true &&
        owner.turnId &&
        owner.turnId !== task.turnId)),
  );
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
  const { get, post } = useServerApi();
  const services = useServerServices();
  const taskRecords = useSubscription(
    opened && leadId ? { kind: "tasks", agentId: leadId } : null,
  );
  const controlStorageKey = `process-control:${services?.owner.id || "local"}:${leadId}`;
  const controlScope = useRef(controlStorageKey);
  controlScope.current = controlStorageKey;
  const [kind, setKind] = useState("all"),
    [query, setQuery] = useState(""),
    [selection, setSelection] = useState<{
      scope?: string;
      id: string;
      requested?: boolean;
    } | null>(null),
    [mobileDetail, setMobileDetail] = useState(false),
    [now, setNow] = useState(Date.now() / 1000);
  const processes = useSubscription(
    opened && leadId ? { kind: "processes", agentId: leadId } : null,
  );
  const processRecords = processes.data?.processes ?? [];
  const processLoadError = processes.error ? errorText(processes.error) : "";
  const processLoading = processes.status === "loading";
  const [controlPending, setControlPending] = useState(false);
  const [includeWorkers, setIncludeWorkers] = useState(false);
  const [pendingControl, setPendingControl] =
    useState<PostBody<"/api/processes/action"> | null>(null);
  const [controlResult, setControlResult] =
    useState<PostResult<"/api/processes/action"> | null>(null);
  const appliedFocus = useRef<string | undefined>(undefined);
  const agents = data.threads,
    owner = (id: string) => agents.find((a) => a.id === id),
    scopeOwner = owner(leadId || ""),
    streamedTarget = processes.data?.responseTarget,
    responseTarget =
      scopeOwner?.inFlight &&
      streamedTarget &&
      streamedTarget.agent === scopeOwner.id &&
      streamedTarget.epoch === scopeOwner.epoch &&
      streamedTarget.threadId === scopeOwner.threadId &&
      streamedTarget.turnId === scopeOwner.turnId &&
      streamedTarget.accountKey === (scopeOwner.accountKey || "default")
        ? streamedTarget
        : null;
  useEffect(() => {
    if (!opened) return;
    const timer = setInterval(() => setNow(Date.now() / 1000), 1000);
    return () => clearInterval(timer);
  }, [opened]);
  useEffect(() => {
    if (!leadId) return;
    setControlResult(null);
    setControlPending(false);
    setIncludeWorkers(false);
    try {
      const raw = sessionStorage.getItem(controlStorageKey);
      setPendingControl(
        raw ? (JSON.parse(raw) as PostBody<"/api/processes/action">) : null,
      );
    } catch {
      setPendingControl(null);
    }
  }, [leadId, controlStorageKey]);
  const processIds = new Set(processRecords.map((process) => process.id));
  const tasks = [
    ...processRecords.map(displayProcess),
    // The task stream also carries tools and commands without a process handle.
    // Process records own the output and controls for overlapping task ids.
    ...(taskRecords.data ?? [])
      .map((task) => projectTaskForRenderer(task))
      .filter(
        (task): task is DisplayBackgroundTask =>
          task != null && !processIds.has(task.id),
      ),
  ].filter(activeTask);
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
    .sort(
      (a, b) =>
        CHAT_INDICATOR_URGENCY.indexOf(backgroundTaskState(a)) -
          CHAT_INDICATOR_URGENCY.indexOf(backgroundTaskState(b)) ||
        b.created - a.created,
    );
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
  const submitControl = async (body: PostBody<"/api/processes/action">) => {
    setControlPending(true);
    try {
      const result = await post("/api/processes/action", body, {
        timeoutMs: 30000,
      });
      if (controlScope.current !== controlStorageKey) return;
      setControlResult(result);
      if (result.status === "uncertain") {
        setPendingControl(body);
        sessionStorage.setItem(controlStorageKey, JSON.stringify(body));
        notify(
          "The stop result is unknown. Check its receipt before another stop.",
        );
      } else {
        sessionStorage.removeItem(controlStorageKey);
        setPendingControl(null);
        const outcomes = (result.processes || [])
          .map((item) => item.status)
          .join(", ");
        notify(
          result.status === "partial"
            ? `Some processes did not stop: ${outcomes || result.error || "see each process result"}`
            : `Stop result: ${outcomes || result.response?.status || "unknown"}`,
        );
      }
      await refresh();
    } catch (error) {
      if (controlScope.current !== controlStorageKey) return;
      if (
        error instanceof ApiError &&
        error.status >= 400 &&
        error.status < 500
      ) {
        sessionStorage.removeItem(controlStorageKey);
        setPendingControl(null);
        notify(errorText(error));
        return;
      }
      setPendingControl(body);
      sessionStorage.setItem(controlStorageKey, JSON.stringify(body));
      notify(
        "The stop reply is unknown. Check its receipt before another stop.",
      );
      void error;
    } finally {
      if (controlScope.current === controlStorageKey) setControlPending(false);
    }
  };
  const checkControlReceipt = async (
    body: PostBody<"/api/processes/action">,
  ) => {
    setControlPending(true);
    try {
      const receipt = await get("/api/processes/action", {
        query: {
          requestId: body.requestId,
          agent: body.agent,
          accountKey: body.accountKey,
          epoch: body.epoch,
        },
      });
      if (controlScope.current !== controlStorageKey) return;
      if (
        !receipt.found ||
        !receipt.result ||
        receipt.result.status === "uncertain"
      ) {
        notify(
          "The stop result is still unknown. Check its receipt again later.",
        );
        return;
      }
      setControlResult(receipt.result);
      sessionStorage.removeItem(controlStorageKey);
      setPendingControl(null);
      const outcomes = (receipt.result.processes || [])
        .map((item) => item.status)
        .join(", ");
      notify(
        receipt.result.status === "partial"
          ? `Some processes did not stop: ${outcomes || receipt.result.error || "see each process result"}`
          : `Stop result: ${outcomes || receipt.result.response?.status || "unknown"}`,
      );
      await refresh();
    } catch {
      if (controlScope.current !== controlStorageKey) return;
      notify(
        "The stop result is still unknown. Check its receipt again later.",
      );
    } finally {
      if (controlScope.current === controlStorageKey) setControlPending(false);
    }
  };
  const runProcessControl = (
    action: "stop_selected" | "stop_owned" | "stop_response",
    process?: DisplayBackgroundTask,
  ) => {
    if (
      !leadId ||
      !scopeOwner ||
      scopeOwner.epoch == null ||
      controlPending ||
      processLoading ||
      processLoadError
    )
      return;
    if (pendingControl) {
      notify("Check the earlier stop result before starting another stop.");
      return;
    }
    const processIdentity = (task: DisplayBackgroundTask) => ({
      key: task.processKey || "",
      generation: task.generation || "",
      agent: task.agent,
      accountKey: task.accountKey || "",
      epoch: task.epoch ?? -1,
    });
    const body: PostBody<"/api/processes/action"> = {
      action,
      requestId: crypto.randomUUID(),
      agent: leadId,
      accountKey: scopeOwner.accountKey || "default",
      epoch: scopeOwner.epoch,
      ...(action === "stop_response"
        ? responseStopIdentity(responseTarget)
        : {}),
      ...(action === "stop_selected" && process
        ? { process: processIdentity(process) }
        : {}),
      ...(action === "stop_owned"
        ? {
            processes: tasks
              .filter(
                (task) =>
                  task.capabilities?.stop &&
                  (task.agent === leadId ||
                    (includeWorkers && owner(task.agent)?.rootId === leadId)),
              )
              .map(processIdentity),
            includeDescendants: includeWorkers,
          }
        : {}),
      ...(action === "stop_selected" &&
      process?.agent !== leadId &&
      includeWorkers
        ? { includeDescendants: true }
        : {}),
    };
    if (action === "stop_response" && !responseTarget) {
      notify("Refresh the process list before stopping this response.");
      return;
    }
    sessionStorage.setItem(controlStorageKey, JSON.stringify(body));
    setPendingControl(body);
    void submitControl(body);
  };
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
          <Badge variant="light" color="gray" size="sm">
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
            ]}
            size="xs"
          />
        </div>
        <span className="tasks-filter-help">
          Commands run in the chat. Monitors run commands in the background.
        </span>
        <div className="tasks-process-actions" aria-label="Process controls">
          <Checkbox
            size="xs"
            label="Include worker processes"
            checked={includeWorkers}
            onChange={(event) => setIncludeWorkers(event.currentTarget.checked)}
            disabled={controlPending || !!pendingControl}
          />
          <Button
            size="xs"
            color="red"
            leftSection={<Square size={12} />}
            loading={controlPending}
            disabled={
              !scopeOwner?.inFlight || !responseTarget || !!pendingControl
            }
            onClick={() => runProcessControl("stop_response")}
          >
            Stop response
          </Button>
          <Button
            size="xs"
            color="red"
            variant="light"
            leftSection={<Square size={12} />}
            loading={controlPending}
            disabled={
              !processRecords.some(
                (process) =>
                  process.capabilities.stop &&
                  (includeWorkers || process.agent === leadId),
              ) ||
              !!pendingControl ||
              processLoading ||
              !!processLoadError
            }
            onClick={() => runProcessControl("stop_owned")}
          >
            Stop owned processes
          </Button>
          {pendingControl && (
            <Button
              size="xs"
              variant="light"
              loading={controlPending}
              onClick={() => void checkControlReceipt(pendingControl)}
            >
              Check stop result
            </Button>
          )}
        </div>
      </div>
      {processLoadError && (
        <p role="alert" className="task-error">
          Process list unavailable: {processLoadError}
        </p>
      )}
      {processLoading && <p role="status">Loading processes…</p>}
      {taskRecords.error && (
        <p role="alert" className="task-error">
          Task list unavailable: {errorText(taskRecords.error)}
        </p>
      )}
      {taskRecords.status === "loading" && <p role="status">Loading tasks…</p>}
      {controlResult?.response && (
        <p role={controlResult.status === "uncertain" ? "alert" : "status"}>
          Response stop: {String(controlResult.response.status)}
          {typeof controlResult.response.error === "string"
            ? `, ${controlResult.response.error}`
            : ""}
        </p>
      )}
      {!!controlResult?.processes?.length && (
        <ul
          aria-label="Process stop results"
          className="process-control-results"
        >
          {controlResult.processes.map((item, index) => {
            const key = typeof item.key === "string" ? item.key : "";
            const process = processRecords.find(
              (candidate) => candidate.key === key,
            );
            return (
              <li key={`${key}:${index}`}>
                {process?.name ||
                  (typeof item.agent === "string"
                    ? owner(item.agent)?.name
                    : null) ||
                  "Process"}
                : {String(item.status || "unknown")}
                {typeof item.error === "string" ? `, ${item.error}` : ""}
              </li>
            );
          })}
        </ul>
      )}
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
            {backgroundProcessGroups.map(([groupKind, label]) => {
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
                  {groupTasks.map((task) => (
                    <BackgroundTaskStrip
                      key={task.id}
                      task={task}
                      owner={owner(task.agent)}
                      now={now}
                      selected={selectedTask?.id === task.id}
                      select={() => {
                        setSelection({ scope: leadId, id: task.id });
                        setMobileDetail(true);
                      }}
                    />
                  ))}
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
            canStop={
              !processLoading &&
              !processLoadError &&
              !pendingControl &&
              !!selectedTask.capabilities?.stop &&
              (includeWorkers || selectedTask.agent === leadId)
            }
            onStop={() => runProcessControl("stop_selected", selectedTask)}
            controlPending={controlPending}
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

/** Same dispatch-strip geometry as an agent; only actual commands use code type. */
export function BackgroundTaskStrip({
  task,
  owner,
  now,
  selected,
  select,
}: {
  task: DisplayBackgroundTask;
  owner?: Agent;
  now: number;
  selected: boolean;
  select: () => void;
}) {
  return (
    <UnstyledButton
      data-task={task.id}
      data-studio-state={backgroundTaskState(task)}
      className={`task-row ${selected ? "selected" : ""}`}
      aria-pressed={selected}
      onClick={select}
    >
      <div className="task-row-heading">
        <span className="task-row-kind">{taskKindLabel(task, owner)}</span>
        <span className="task-age">{elapsed(task, now)}</span>
      </div>
      <strong
        className={task.command ? "task-command-title" : ""}
        title={taskName(task)}
      >
        {taskName(task)}
      </strong>
      <div className="task-row-footer">
        <span>{owner?.name || "Agent"}</span>
        <TaskStatus task={task} />
      </div>
    </UnstyledButton>
  );
}

function TaskDetail({
  task: summary,
  owner,
  now,
  requests,
  back,
  openAgent,
  refresh,
  notify,
  canStop,
  onStop,
  controlPending,
}: {
  task: DisplayBackgroundTask;
  owner?: Agent;
  now: number;
  requests: PendingRequest[];
  back: () => void;
  openAgent: () => void;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
  canStop: boolean;
  onStop: () => void;
  controlPending: boolean;
}) {
  const { get, post } = useServerApi();

  const [fileOutput, setFileOutput] = useState<{
    text: string;
    truncated: boolean;
  } | null>(null);
  const [filePending, setFilePending] = useState(false);
  const [fileError, setFileError] = useState("");
  const [detail, setDetail] = useState<TaskDetailResponse | null>(null),
    [loadError, setLoadError] = useState("");
  const loadDetail = async () => {
    try {
      const value = await get("/api/task", { query: { id: summary.id } });
      setDetail(value);
      setLoadError("");
    } catch (error) {
      setLoadError(errorText(error));
    }
  };
  const resolvedDetail = detail?.id === summary.id ? detail : null;
  const task = resolvedDetail ? { ...resolvedDetail, ...summary } : summary;
  const monitor = task.kind === "monitor" && "tail" in task ? task : null;
  const outputSupported = summary.capabilities?.readOutput !== false;
  const outputTail = outputSupported
    ? (fileOutput?.text ??
      resolvedDetail?.tail ??
      ("tail" in task ? task.tail : undefined))
    : undefined;
  const outputError = task.error ?? resolvedDetail?.error ?? monitor?.error;
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
      if (
        result &&
        typeof result === "object" &&
        "error" in result &&
        result.error
      )
        throw new Error(displayError(result.error));
      void refresh().catch((error) => notify(errorText(error)));
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
        {task.kind === "monitor" && task.outputWakeBudget && (
          <div className="task-wake-budget" role="status">
            <Text size="sm">
              {task.outputWakeBudget.totalSuppressedMatches} matches suppressed
            </Text>
            {task.outputWakeBudget.paused && (
              <>
                <Text fw={600} size="sm">
                  Output wakes paused
                </Text>
                <Text size="sm">
                  {task.status === "running"
                    ? "The command is still running. Its exit will still wake the agent."
                    : "The command exit will still wake the agent."}
                </Text>
              </>
            )}
            {activeTask(task) && (
              <MonitorRestart task={task} notify={notify} refresh={refresh} />
            )}
          </div>
        )}
        {task.outputFile && (
          <div className="task-file-output">
            <p className="task-cwd">{task.outputFile}</p>
            <Button
              size="xs"
              loading={filePending}
              onClick={async () => {
                setFilePending(true);
                setFileError("");
                try {
                  setFileOutput(
                    await get("/api/task/output", {
                      query: { id: task.id, agent: task.agent },
                    }),
                  );
                } catch (error) {
                  setFileError(errorText(error));
                } finally {
                  setFilePending(false);
                }
              }}
            >
              View output
            </Button>
            {fileError && (
              <p className="task-error" role="alert">
                {fileError}
              </p>
            )}
            {fileOutput?.truncated && (
              <p className="task-note">
                The output file exceeds the preview limit. Only its first part
                appears below.
              </p>
            )}
          </div>
        )}
        {task.kind !== "monitor" && !resolvedDetail && (
          <Button
            size="compact-xs"
            variant="subtle"
            onClick={() => void loadDetail()}
          >
            Read tool input
          </Button>
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
              {outputSupported ? "Output" : "Output unavailable"}
            </span>
            {outputSupported && (
              <div>
                <Button
                  size="compact-xs"
                  aria-pressed={follow}
                  onClick={() => setFollow(!follow)}
                  leftSection={<ChevronDown size={12} />}
                  color={follow ? "signal" : "gray"}
                >
                  Follow
                </Button>
                <ActionIcon
                  size="sm"
                  aria-label="Download task log"
                  disabled={pending || (!!task.outputFile && !fileOutput)}
                  onClick={() =>
                    void downloadLog(
                      task,
                      resolvedDetail,
                      notify,
                      fileOutput?.text,
                    )
                  }
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
            )}
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
            {!outputSupported
              ? "This provider does not expose process output."
              : outputTail ||
                (task.outputFile && !fileOutput
                  ? "Select View output to read this file."
                  : activeTask(task)
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
        {task.status === "running" && task.capabilities?.sendInput && (
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
        {canStop && activeTask(task) && (
          <Button
            size="xs"
            color="red"
            leftSection={<Square size={12} />}
            loading={controlPending}
            disabled={!!task.cancelRequested || controlPending}
            onClick={onStop}
          >
            {task.cancelRequested ? "Stop requested" : "Stop selected process"}
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
        {!native && (
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
  fileText?: string,
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
      blob = new Blob([fileText ?? detail?.tail ?? task.tail ?? ""], {
        type: "text/plain",
      });
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
