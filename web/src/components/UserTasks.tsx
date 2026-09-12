import ErrorDescription from "./ErrorDescription";
import { displayError } from "../errorPresentation";
import {
  Avatar,
  Badge,
  Button,
  NativeSelect,
  Textarea,
  TextInput,
  UnstyledButton,
} from "@mantine/core";
import {
  CheckCheck,
  ChevronDown,
  ChevronRight,
  ClipboardCheck,
  Clock3,
  RotateCcw,
  Search,
} from "lucide-react";
import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import { api, errorText, saved } from "../api";
import { writeLocalDraft } from "../sync/localDraft";
import type { Agent, Json, Snapshot, UserTask } from "../types";
import "./user-tasks.css";

// Keep notes across panel changes and reloads within the same workspace.
const noteDrafts = new Map<string, string>();
const noteListeners = new Map<string, Set<() => void>>();
function useTaskNote(key: string, notify: (message: string) => void) {
  const storageKey = `studio-task-note:${key}`;
  if (!noteDrafts.has(key)) noteDrafts.set(key, saved(storageKey, ""));
  const note = useSyncExternalStore(
    (listener) => {
      const listeners = noteListeners.get(key) || new Set();
      noteListeners.set(key, listeners);
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
        if (!listeners.size) noteListeners.delete(key);
      };
    },
    () => noteDrafts.get(key) || "",
  );
  return [
    note,
    (value: string) => {
      noteDrafts.set(key, value);
      const error = writeLocalDraft(storageKey, value);
      if (error) notify(error);
      noteListeners.get(key)?.forEach((listener) => listener());
    },
  ] as const;
}

type Task = UserTask;
type Props = {
  data: Snapshot;
  agent?: Agent;
  compact?: boolean;
  focusId?: string;
  refresh: () => Promise<void>;
  notify: (message: string) => void;
  onSelect?: (id: string) => void;
};
const statusNames = {
  open: "To do",
  review: "Agent reviewing",
  accepted: "Accepted",
  cancelled: "Cancelled",
};
const statusColors = {
  open: "indigo",
  review: "yellow",
  accepted: "teal",
  cancelled: "gray",
};
const when = (at: number) => new Date(at * 1000).toLocaleString();
const isActive = (task: Task) => ["open", "review"].includes(task.status);

export default function UserTasks(p: Props) {
  const [expanded, setExpanded] = useState(false);
  const [query, setQuery] = useState("");
  const [status, setStatus] = useState("active");
  const [owner, setOwner] = useState("");
  const [local, setLocal] = useState<Record<string, Task>>({});
  const tasks: Task[] = (p.data.runtime.userTasks || []).map((task: Task) =>
    local[task.id]?.version > task.version ? local[task.id] : task,
  );
  const root = p.agent?.rootId || p.agent?.id;
  const team = tasks.filter((task) => !p.compact || task.rootId === root);
  const active = team.filter(isActive);
  const openCount = active.filter((task) => task.status === "open").length;
  const reviewCount = active.length - openCount;
  const filtered = team
    .filter((task) => {
      if (p.compact) return isActive(task);
      if (owner && task.agent !== owner) return false;
      if (
        status === "active"
          ? !isActive(task)
          : status !== "all" && task.status !== status
      )
        return false;
      const name =
        p.data.threads.find((agent) => agent.id === task.agent)?.name || "";
      return [
        task.title,
        task.description,
        task.criteria,
        displayError(task.reason),
        name,
      ]
        .join(" ")
        .toLocaleLowerCase()
        .includes(query.toLocaleLowerCase());
    })
    .sort(
      (a, b) =>
        Number(b.status === "open") - Number(a.status === "open") ||
        (b.updated || 0) - (a.updated || 0),
    );
  useEffect(() => {
    if (!p.focusId) return;
    setStatus("all");
    setOwner("");
    setQuery("");
    setExpanded(true);
  }, [p.focusId]);
  if (p.compact && (!p.agent || !active.length)) return null;
  const update = (task: Task) =>
    setLocal((old) => ({ ...old, [task.id]: task }));
  return (
    <section
      className={`user-tasks ${p.compact ? "user-tasks-compact" : "user-tasks-full"}`}
      aria-label={p.compact ? "Tasks for you from this team" : "Your tasks"}
    >
      {p.compact ? (
        <UnstyledButton
          className="user-tasks-summary"
          onClick={() => setExpanded(!expanded)}
          aria-expanded={expanded}
        >
          <ClipboardCheck size={16} />
          <strong>Your tasks</strong>
          <span className="user-tasks-counts">
            {openCount > 0 && `${openCount} to do`}
            {openCount > 0 && reviewCount > 0 && " · "}
            {reviewCount > 0 && `${reviewCount} in review`}
          </span>
          <ChevronDown size={15} className={expanded ? "rotated" : ""} />
        </UnstyledButton>
      ) : (
        <div className="user-tasks-filters">
          <TextInput
            aria-label="Search your tasks"
            placeholder="Search tasks, instructions, or agents"
            leftSection={<Search size={15} />}
            value={query}
            onChange={(e) => setQuery(e.currentTarget.value)}
          />
          <NativeSelect
            aria-label="Task status"
            value={status}
            onChange={(e) => setStatus(e.currentTarget.value)}
          >
            <option value="active">Active tasks</option>
            <option value="open">To do</option>
            <option value="review">Agent reviewing</option>
            <option value="accepted">Accepted</option>
            <option value="cancelled">Cancelled</option>
            <option value="all">All tasks</option>
          </NativeSelect>
          <NativeSelect
            aria-label="Task owner"
            value={owner}
            onChange={(e) => setOwner(e.currentTarget.value)}
          >
            <option value="">All agents</option>
            {p.data.threads
              .filter((agent) => tasks.some((task) => task.agent === agent.id))
              .map((agent) => (
                <option key={agent.id} value={agent.id}>
                  {agent.name}
                </option>
              ))}
          </NativeSelect>
        </div>
      )}
      {(!p.compact || expanded) && (
        <>
          <p className="user-tasks-help">
            Send your result for review. The agent can accept it or request
            changes.
          </p>
          <div className="user-tasks-list">
            {filtered.map((task) => (
              <UserTaskRow
                key={task.id}
                task={task}
                noteKey={JSON.stringify([p.data.stateDir, task.id])}
                owner={p.data.threads.find((agent) => agent.id === task.agent)}
                compact={p.compact}
                focused={p.focusId === task.id}
                onSelect={p.onSelect}
                update={update}
                refresh={p.refresh}
                notify={p.notify}
              />
            ))}
            {!filtered.length && (
              <div className="user-tasks-empty">
                <ClipboardCheck size={25} />
                <strong>
                  {tasks.length
                    ? "No matching tasks"
                    : "Nothing needed from you"}
                </strong>
                <p>
                  {tasks.length
                    ? "Change the search or status filter."
                    : "Agents add tasks here when they need your help."}
                </p>
              </div>
            )}
          </div>
        </>
      )}
    </section>
  );
}

function UserTaskRow(p: {
  task: Task;
  noteKey: string;
  owner?: Agent;
  compact?: boolean;
  focused: boolean;
  onSelect?: (id: string) => void;
  update: (task: Task) => void;
  refresh: () => Promise<void>;
  notify: (message: string) => void;
}) {
  const { task } = p;
  const [details, setDetails] = useState(p.focused);
  const [note, setNote] = useTaskNote(p.noteKey, p.notify);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState("");
  const requestKey = `studio-task-completion:${p.noteKey}`;
  const lock = useRef(false);
  const row = useRef<HTMLElement>(null);
  useEffect(() => {
    if (p.focused) {
      setDetails(true);
      row.current?.scrollIntoView({ block: "nearest" });
    }
  }, [p.focused]);
  const complete = async () => {
    if (lock.current || task.status !== "open") return;
    lock.current = true;
    setPending(true);
    setError("");
    const fingerprint = JSON.stringify({
      task: task.id,
      version: task.version,
      note,
    });
    const previous = saved<{ fingerprint: string; id: string } | null>(
      requestKey,
      null,
    );
    const request =
      previous?.fingerprint === fingerprint
        ? previous
        : { fingerprint, id: crypto.randomUUID() };
    try {
      const storageError = writeLocalDraft(requestKey, request);
      if (storageError) throw new Error(storageError);
      const response = await api("/api/user-tasks/complete", {
        id: request.id,
        task_id: task.id,
        version: task.version,
        note,
      });
      const updated = (response.task || response) as Task;
      p.update(updated);
      if (noteDrafts.get(p.noteKey) === note) setNote("");
      const cleanupError = writeLocalDraft(requestKey, null);
      if (cleanupError) p.notify(cleanupError);
      const stopped = !!updated.agentStopped;
      const message = stopped
        ? "Saved; agent stopped. Your result waits for the agent."
        : "Sent to the agent for review.";
      p.notify(message);
      try {
        await p.refresh();
      } catch {
        /* The completion response already confirms the stored result. */
      }
    } catch (e) {
      setError(errorText(e));
      setDetails(true);
    } finally {
      lock.current = false;
      setPending(false);
    }
  };
  const history: Json[] = task.history || [];
  const returned = task.status === "open" && !!task.reason;
  return (
    <article
      ref={row}
      className={`user-task ${returned ? "user-task-returned" : ""} ${p.focused ? "user-task-focused" : ""}`}
      data-user-task={task.id}
    >
      <div className="user-task-head">
        <UnstyledButton
          className="user-task-toggle"
          onClick={() => setDetails(!details)}
          aria-expanded={details}
        >
          <strong>{task.title}</strong>
          <ChevronRight size={15} className={details ? "rotated" : ""} />
        </UnstyledButton>
        <Badge size="xs" variant="light" color={statusColors[task.status]}>
          {pending ? "Sending" : statusNames[task.status]}
        </Badge>
      </div>
      <div className="user-task-owner">
        <Avatar size={18} radius="xl" color="indigo">
          {(p.owner?.name || "A").slice(0, 1).toUpperCase()}
        </Avatar>
        <span>{p.owner?.name || "Agent"}</span>
        {returned && (
          <span className="user-task-return-label">
            <RotateCcw size={11} /> Needs another pass
          </span>
        )}
      </div>
      {returned && (
        <p className="user-task-reason">
          <ErrorDescription value={task.reason} role="status" />
        </p>
      )}
      {task.status === "review" && task.agentStopped && (
        <p className="user-task-delivery" role="status">
          Saved; agent stopped. Your result waits for the agent.
        </p>
      )}
      {details && (
        <div className="user-task-details">
          {task.description && (
            <p className="user-task-description">{task.description}</p>
          )}
          {["accepted", "cancelled"].includes(task.status) && task.reason && (
            <div className="user-task-criteria">
              <strong>
                {task.status === "accepted"
                  ? "Agent decision"
                  : "Cancellation reason"}
              </strong>
              <p>
                <ErrorDescription value={task.reason} role="status" />
              </p>
            </div>
          )}
          {task.criteria && (
            <div className="user-task-criteria">
              <strong>Done when</strong>
              <p>{task.criteria}</p>
            </div>
          )}
          {task.status === "open" && (
            <div className="user-task-completion">
              <Textarea
                label="Result note (optional)"
                placeholder="What you did, a link, or a result"
                value={note}
                onChange={(e) => setNote(e.currentTarget.value)}
                disabled={pending}
                autosize
                minRows={2}
                maxRows={5}
                maxLength={16000}
              />
              <Button
                variant="light"
                size="xs"
                leftSection={<CheckCheck size={14} />}
                loading={pending}
                onClick={() => void complete()}
              >
                Send for review
              </Button>
            </div>
          )}
          {task.status === "review" && (
            <p className="user-task-review">
              <Clock3 size={14} /> Waiting for the agent to check your result.
            </p>
          )}
          {task.completionNote && (
            <div className="user-task-criteria">
              <strong>Your completion note</strong>
              <p>{task.completionNote}</p>
            </div>
          )}
          {error && (
            <p className="user-task-error" role="alert">
              {error}
            </p>
          )}
          {history.length > 0 && (
            <details className="user-task-history">
              <summary>History · {history.length}</summary>
              <ol>
                {history.map((event, index) => (
                  <li key={index}>
                    <span>
                      <strong>
                        {String(event.action).replaceAll("_", " ")}
                      </strong>
                      <time>{when(event.at)}</time>
                    </span>
                    {event.text && (
                      <p>
                        <ErrorDescription value={event.text} role="status" />
                      </p>
                    )}
                  </li>
                ))}
              </ol>
            </details>
          )}
          {p.onSelect && (
            <Button
              variant="subtle"
              size="compact-xs"
              onClick={() => p.onSelect!(task.agent)}
            >
              Open agent chat <ChevronRight size={13} />
            </Button>
          )}
        </div>
      )}
    </article>
  );
}
