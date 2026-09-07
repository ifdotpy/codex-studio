import { complaintNeedsUserResponse } from "../types";
import {
  Badge,
  Button,
  Drawer,
  Loader,
  Modal,
  MultiSelect,
  NativeSelect,
  NumberInput,
  Textarea,
  TextInput,
  UnstyledButton,
} from "@mantine/core";
import {
  BookOpen,
  CheckCheck,
  ChevronRight,
  Clock3,
  FileDiff,
  Files,
  GitBranch,
  Inbox,
  Layers3,
  ListTodo,
  Plus,
  RefreshCw,
  Search,
  SlidersHorizontal,
  Users,
  Wrench,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { api, errorText, save, saved } from "../api";
import type { Agent, Json, Snapshot } from "../types";
import Requests from "./Requests";
import UserTasks from "./UserTasks";
import ComplaintBook from "./ComplaintBook";
import FilePreview, { type PreviewTarget } from "./FilePreview";
import "./Workspace.css";

type Props = {
  initialSection?: string;
  opened: boolean;
  onClose: () => void;
  agent?: Agent;
  data: Snapshot;
  onSelect: (id: string) => void;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
};
type Context = Props & {
  selected: Agent | undefined;
  run: (path: string, body: Json) => Promise<any>;
  reload: () => void;
  revision: number;
  preview: (target: PreviewTarget) => void;
  navigate: (section: string, agent?: string, item?: string) => void;
  focusId: string;
  notifications: boolean;
  toggleNotifications: () => Promise<void>;
};
const sections = [
  ["work", "Work", ListTodo],
  ["user-tasks", "Your tasks", CheckCheck],
  ["changes", "Changes", FileDiff],
  ["inbox", "Inbox", Inbox],
  ["search", "Search", Search],
  ["plan", "Plan", BookOpen],
  ["checkpoints", "Checkpoints", GitBranch],
  ["tools", "Tools", Wrench],
  ["profiles", "Profiles", Users],
  ["rules", "Rules", Clock3],
  ["resources", "Resources", Layers3],
] as const;
const descriptions: Record<string, string> = {
  work: "Assign work, track dependencies, and accept results.",
  "user-tasks":
    "Tasks agents need you to complete. Each result goes back to its agent for review.",
  changes:
    "Latest changes reported by this agent. File previews show the current files.",
  inbox: "Questions, approvals, and problems in this chat.",
  search: "Find messages, work, plans, and agent conversations.",
  plan: "A shared plan for this agent. Saved edits remain separate from its reported plan.",
  checkpoints:
    "Save a point in the work. Preview file changes before a restore.",
  tools:
    "Tools available through orchestration, connected services, and observed native calls.",
  profiles: "Reusable instructions and model choices for workers.",
  rules:
    "Wait for a time, file change, or event. Script checks can prevent unnecessary agent turns.",
  resources: "Shared resource leases from the existing resource board.",
};
const date = (value: number | string | undefined) =>
  value
    ? new Date(
        typeof value === "number" ? value * 1000 : value,
      ).toLocaleString()
    : "";
const endpoint = (name: string, agent?: Agent) =>
  `/api/${name}${agent ? `?agent=${encodeURIComponent(agent.id)}` : ""}`;
function Empty({ children }: { children: React.ReactNode }) {
  return <div className="workspace-empty">{children}</div>;
}
function useResource(path: string | null, revision: number) {
  const [state, setState] = useState<{
    data: Json | null;
    error: string;
    loading: boolean;
  }>({ data: null, error: "", loading: false });
  useEffect(() => {
    if (!path) {
      setState({ data: null, error: "", loading: false });
      return;
    }
    let active = true;
    setState((old) => ({ ...old, error: "", loading: true }));
    api(path)
      .then((data) => {
        if (active) setState({ data, error: "", loading: false });
      })
      .catch((e) => {
        if (active)
          setState({ data: null, error: errorText(e), loading: false });
      });
    return () => {
      active = false;
    };
  }, [path, revision]);
  return state;
}
function ResourceState({
  state,
}: {
  state: { error: string; loading: boolean; data: Json | null };
}) {
  return state.error ? (
    <p role="alert" className="workspace-error">
      {state.error}
    </p>
  ) : state.loading && !state.data ? (
    <div className="workspace-load">
      <Loader size="sm" />
    </div>
  ) : null;
}
function Status({ value }: { value: string }) {
  return (
    <Badge
      size="xs"
      variant="light"
      color={
        value === "accepted"
          ? "teal"
          : value === "review"
            ? "orange"
            : ["failed", "blocked"].includes(value)
              ? "red"
              : "gray"
      }
    >
      {value.replaceAll("_", " ")}
    </Badge>
  );
}
function ownerName(data: Snapshot, id?: string) {
  return data.threads.find((a) => a.id === id)?.name || id || "Unassigned";
}

export function Workspace(props: Props) {
  const [section, setSection] = useState("work"),
    [agentId, setAgentId] = useState(props.agent?.id || ""),
    [revision, setRevision] = useState(0),
    [preview, setPreview] = useState<PreviewTarget | null>(null);
  const [pending, setPending] = useState(0),
    [focusId, setFocusId] = useState("");
  const [notifications, setNotifications] = useState(() =>
    window.codexDesktop ? false : saved("workspace-notifications", false),
  );
  const dataRef = useRef(props.data);
  dataRef.current = props.data;
  const refreshRef = useRef(props.refresh),
    notifyRef = useRef(props.notify);
  refreshRef.current = props.refresh;
  notifyRef.current = props.notify;
  useEffect(() => {
    if (props.opened) setAgentId(props.agent?.id || "");
  }, [props.opened, props.agent?.id]);
  useEffect(() => {
    if (props.opened && props.initialSection) {
      setSection(props.initialSection);
      setFocusId("");
    }
  }, [props.opened, props.initialSection]);
  const reload = useCallback(() => setRevision((value) => value + 1), []);
  useEffect(() => {
    if (!props.opened) return;
    const timer = setInterval(reload, 5000);
    return () => clearInterval(timer);
  }, [props.opened, reload]);
  const run = useCallback(
    async (path: string, body: Json) => {
      setPending((n) => n + 1);
      try {
        const result = await api(path, body);
        reload();
        await refreshRef.current();
        return result;
      } catch (e) {
        notifyRef.current(errorText(e));
        throw e;
      } finally {
        setPending((n) => n - 1);
      }
    },
    [reload],
  );
  const toggleNotifications = async () => {
    if (notifications) {
      try {
        if (window.codexDesktop)
          await window.codexDesktop.setNotifications(false);
        setNotifications(false);
        save("workspace-notifications", false);
      } catch (error) {
        props.notify(errorText(error));
      }
      return;
    }
    if (window.codexDesktop) {
      try {
        const allowed = await window.codexDesktop.setNotifications(true);
        setNotifications(allowed);
        save("workspace-notifications", allowed);
      } catch (error) {
        props.notify(errorText(error));
      }
      return;
    }
    if (!("Notification" in window)) {
      props.notify("This browser does not support desktop notifications.");
      return;
    }
    const permission = await Notification.requestPermission();
    if (permission !== "granted") {
      props.notify(
        "Notifications are blocked. Change the browser permission to enable them.",
      );
      return;
    }
    setNotifications(true);
    save("workspace-notifications", true);
  };
  useEffect(() => {
    if (
      !notifications ||
      (!window.codexDesktop &&
        (!("Notification" in window) || Notification.permission !== "granted"))
    )
      return;
    let active = true,
      seen: Set<string> | null = null;
    const check = async () => {
      try {
        if (!props.agent) return;
        const result = await api(endpoint("workspace", props.agent));
        if (!active) return;
        const ids = new Set(dataRef.current.threads.map((agent) => agent.id));
        const items: Json[] = (result.inbox || []).filter((item: Json) =>
          ids.has(item.agent),
        );
        const next = new Set(items.map((item) => `${item.kind}:${item.id}`));
        const added = seen
          ? items.filter((item) => !seen!.has(`${item.kind}:${item.id}`))
          : [];
        seen = next;
        if (added.length && document.visibilityState !== "visible") {
          const groups = new Map<string, number>();
          for (const item of added)
            groups.set(item.kind, (groups.get(item.kind) || 0) + 1);
          const body = [...groups]
            .map(([kind, count]) => `${count} ${kind}${count === 1 ? "" : "s"}`)
            .join(", ");
          if (window.codexDesktop) {
            await window.codexDesktop.notify({
              title: "Codex Studio needs attention",
              body,
            });
            return;
          }
          const notification = new Notification(
            "Codex Studio needs attention",
            { body, tag: "codex-workspace-attention" },
          );
          notification.onclick = () => {
            notification.close();
            window.focus();
            setSection("inbox");
            document.getElementById("workspace-toggle")?.click();
          };
        }
      } catch {
        /* Retry on the next interval. This does not change the inbox. */
      }
    };
    void check();
    const timer = setInterval(() => void check(), 10000);
    return () => {
      active = false;
      clearInterval(timer);
    };
  }, [notifications, props.agent?.rootId, props.agent?.id]);
  const selected = props.data.threads.find(
    (a) => a.id === agentId && a.source === "managed",
  );
  const context: Context = {
    ...props,
    selected,
    run,
    reload,
    revision,
    preview: setPreview,
    focusId,
    notifications,
    toggleNotifications,
    navigate: (section, agent, item) => {
      setSection(section);
      if (agent) setAgentId(agent);
      setFocusId(item || "");
    },
  };
  const needAgent = [
    "work",
    "changes",
    "plan",
    "checkpoints",
    "tools",
    "rules",
  ].includes(section);
  const title = sections.find(([id]) => id === section)?.[1];
  return (
    <Drawer
      opened={props.opened}
      onClose={props.onClose}
      position="right"
      size="min(1180px, 100vw)"
      title={
        <span className="workspace-drawer-title">
          <Layers3 size={19} /> Workspace
        </span>
      }
      className="workspace-drawer"
    >
      <div className="workspace-shell">
        <nav className="workspace-nav" aria-label="Workspace sections">
          {sections.map(([id, label, Icon]) => (
            <UnstyledButton
              key={id}
              className={`workspace-nav-item ${section === id ? "selected" : ""}`}
              onClick={() => setSection(id)}
              aria-current={section === id ? "page" : undefined}
            >
              <Icon size={17} />
              <span>{label}</span>
              {id === "inbox" &&
                !!props.data.runtime.requests.filter(
                  (request) => !request.deferred,
                ).length && (
                  <span className="workspace-count">
                    {
                      props.data.runtime.requests.filter(
                        (request) => !request.deferred,
                      ).length
                    }
                  </span>
                )}
            </UnstyledButton>
          ))}
        </nav>
        <main className="workspace-content">
          <header className="workspace-heading">
            <div>
              <h2>{title}</h2>
              <p>{descriptions[section]}</p>
            </div>
            <Button
              variant="subtle"
              size="compact-sm"
              aria-label="Refresh workspace"
              onClick={reload}
            >
              <RefreshCw size={16} />
            </Button>
          </header>
          {section !== "user-tasks" && (
            <div className="workspace-scope">
              <NativeSelect
                label="Agent"
                value={agentId}
                onChange={(e) => setAgentId(e.target.value)}
              >
                <option value="">Select an agent</option>
                {props.data.threads
                  .filter((a) => a.source === "managed")
                  .map((a) => (
                    <option key={a.id} value={a.id}>
                      {a.isLead ? "Lead · " : ""}
                      {a.name}
                    </option>
                  ))}
              </NativeSelect>
              {selected && (
                <Button
                  variant="subtle"
                  size="xs"
                  onClick={() => {
                    props.onSelect(selected.id);
                    props.onClose();
                  }}
                >
                  Open chat <ChevronRight size={14} />
                </Button>
              )}
              {pending > 0 && <Loader size={16} />}
            </div>
          )}
          {needAgent && !selected ? (
            <Empty>Select an agent to view its {title?.toLowerCase()}.</Empty>
          ) : (
            <div key={`${section}:${selected?.id || "all"}`}>
              {section === "work" && <Work {...context} />}
              {section === "user-tasks" && (
                <UserTasks
                  data={props.data}
                  focusId={focusId}
                  refresh={props.refresh}
                  notify={props.notify}
                  onSelect={(id) => {
                    props.onSelect(id);
                    props.onClose();
                  }}
                />
              )}

              {section === "changes" && <Changes {...context} />}
              {section === "inbox" && <Attention {...context} />}
              {section === "search" && <Find {...context} />}
              {section === "plan" && <Plan {...context} />}
              {section === "checkpoints" && <Checkpoints {...context} />}
              {section === "tools" && <Tools {...context} />}
              {section === "profiles" && <Profiles {...context} />}
              {section === "rules" && <Rules {...context} />}
              {section === "resources" && <Resources {...context} />}
            </div>
          )}
        </main>
      </div>
      <FilePreview target={preview} onClose={() => setPreview(null)} />
    </Drawer>
  );
}
export default Workspace;

function Work(c: Context) {
  const state = useResource(endpoint("work", c.selected), c.revision),
    tasks: Json[] = state.data?.tasks || [];
  const [filter, setFilter] = useState("all"),
    [id, setId] = useState(c.focusId),
    [editor, setEditor] = useState<Json | null>(null),
    [submission, setSubmission] = useState<Json | null>(null),
    [decision, setDecision] = useState<Json | null>(null),
    [saving, setSaving] = useState(false);
  const selected = tasks.find((task) => task.id === id),
    root = c.selected?.rootId || c.selected?.id;
  const team = c.data.threads.filter(
    (a) => (a.rootId || a.id) === root && a.source === "managed",
  );
  const request = useRef<{ signature: string; id: string } | null>(null);
  const submit = async (body: Json, done: () => void) => {
    const signature = JSON.stringify(body);
    if (request.current?.signature !== signature)
      request.current = { signature, id: crypto.randomUUID() };
    setSaving(true);
    try {
      await c.run("/api/work", {
        agent: c.selected!.id,
        ...body,
        id: request.current.id,
      });
      request.current = null;
      done();
    } catch {
      /* Retain the draft after a failed request. */
    } finally {
      setSaving(false);
    }
  };
  return (
    <>
      <ResourceState state={state} />
      <div className="workspace-toolbar">
        <NativeSelect
          aria-label="Work status"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          data={[
            { value: "all", label: "All work" },
            { value: "ready", label: "Ready" },
            { value: "running", label: "In progress" },
            { value: "blocked", label: "Blocked" },
            { value: "review", label: "Needs acceptance" },
            { value: "accepted", label: "Accepted" },
          ]}
        />
        <Button
          size="xs"
          variant="filled"
          color="indigo"
          leftSection={<Plus size={14} />}
          onClick={() =>
            setEditor({
              title: "",
              description: "",
              owner: "",
              dependencies: [],
              status: "ready",
            })
          }
        >
          New work
        </Button>
      </div>
      <div className="workspace-master-detail">
        <div className="workspace-list">
          {tasks
            .filter(
              (t) =>
                filter === "all" || (t.displayStatus || t.status) === filter,
            )
            .map((task) => (
              <UnstyledButton
                key={task.id}
                className={`workspace-row ${id === task.id ? "selected" : ""}`}
                onClick={() => setId(task.id)}
              >
                <div className="workspace-row-head">
                  <strong>{task.title}</strong>
                  <Status value={task.displayStatus || task.status} />
                </div>
                <small>
                  {ownerName(c.data, task.owner)}
                  {task.blockedBy?.length
                    ? ` · ${task.blockedBy.length} dependencies`
                    : ""}
                </small>
              </UnstyledButton>
            ))}
          {!tasks.length && !state.loading && !state.error && (
            <Empty>No work yet. Create the first task for this team.</Empty>
          )}
        </div>
        {selected ? (
          <article className="workspace-detail">
            <div className="workspace-toolbar">
              <Status value={selected.displayStatus || selected.status} />
              <Button
                size="compact-xs"
                variant="subtle"
                disabled={selected.status === "accepted"}
                onClick={() =>
                  setEditor({ ...selected, owner: selected.owner || "" })
                }
              >
                Edit
              </Button>
            </div>
            <h3>{selected.title}</h3>
            <p className="workspace-prose">
              {selected.description || "No description."}
            </p>
            <dl className="workspace-facts">
              <dt>Owner</dt>
              <dd>{ownerName(c.data, selected.owner)}</dd>
              <dt>Dependencies</dt>
              <dd>
                {(selected.dependencies || [])
                  .map(
                    (dep: string) =>
                      tasks.find((t) => t.id === dep)?.title || dep,
                  )
                  .join(", ") || "None"}
              </dd>
            </dl>
            {selected.status !== "accepted" && (
              <div className="workspace-actions">
                <Button
                  size="xs"
                  variant="light"
                  disabled={
                    !selected.owner ||
                    !!selected.blockedBy?.length ||
                    !["ready", "running"].includes(selected.status)
                  }
                  onClick={() =>
                    void submit(
                      {
                        action: "claim",
                        task_id: selected.id,
                        owner: selected.owner,
                        version: selected.version,
                      },
                      () => {},
                    )
                  }
                >
                  Claim for owner
                </Button>
                <Button
                  size="xs"
                  disabled={!selected.owner || !!selected.blockedBy?.length}
                  onClick={() =>
                    setSubmission({
                      task_id: selected.id,
                      result: "",
                      checks: "",
                      revision: "",
                      files: "",
                      version: selected.version,
                    })
                  }
                >
                  Submit result
                </Button>
              </div>
            )}
            {(selected.results || [])
              .slice()
              .reverse()
              .map((result: Json, index: number) => (
                <section className="workspace-result" key={result.id || index}>
                  <div className="workspace-row-head">
                    <strong>Submitted result</strong>
                    <small>{date(result.created)}</small>
                  </div>
                  <p className="workspace-prose">
                    {result.text || result.result || result.summary}
                  </p>
                  <dl className="workspace-facts">
                    <dt>Checks reported</dt>
                    <dd className="workspace-prose">
                      {result.checks || "None"}
                    </dd>
                    <dt>Revision reported</dt>
                    <dd>
                      <code>{result.revision || "None"}</code>
                    </dd>
                  </dl>
                  <div className="workspace-actions">
                    {(result.files || []).map((path: string) => (
                      <Button
                        key={path}
                        variant="light"
                        size="xs"
                        leftSection={<Files size={13} />}
                        onClick={() =>
                          c.preview({
                            agent: selected.owner || c.selected!.id,
                            path,
                          })
                        }
                      >
                        {path}
                      </Button>
                    ))}
                  </div>
                  {index === 0 && selected.status === "review" && (
                    <div className="workspace-actions">
                      <Button
                        color="teal"
                        size="xs"
                        leftSection={<CheckCheck size={14} />}
                        onClick={() =>
                          setDecision({
                            action: "accept",
                            task_id: selected.id,
                            version: selected.version,
                            result: "",
                          })
                        }
                      >
                        Accept
                      </Button>
                      <Button
                        variant="light"
                        color="orange"
                        size="xs"
                        onClick={() =>
                          setDecision({
                            action: "reject",
                            task_id: selected.id,
                            version: selected.version,
                            result: "",
                          })
                        }
                      >
                        Request changes
                      </Button>
                    </div>
                  )}
                </section>
              ))}
            {!!selected.decisions?.length && (
              <section className="workspace-result">
                <h4>Acceptance history</h4>
                {selected.decisions.map((entry: Json, index: number) => (
                  <div key={entry.id || index}>
                    <Status
                      value={
                        entry.decision ||
                        entry.action ||
                        entry.status ||
                        "decision"
                      }
                    />
                    <p className="workspace-prose">
                      {entry.reason || entry.result}
                    </p>
                    <small>{date(entry.created)}</small>
                  </div>
                ))}
              </section>
            )}
          </article>
        ) : (
          !!tasks.length && (
            <Empty>Select work to view its dependencies and results.</Empty>
          )
        )}
      </div>
      <Modal
        opened={!!editor}
        onClose={() => !saving && setEditor(null)}
        title={editor?.id ? "Edit work" : "New work"}
        size="lg"
      >
        {editor && (
          <form
            className="workspace-form"
            onSubmit={(e) => {
              e.preventDefault();
              void submit(
                {
                  title: editor.title,
                  description: editor.description,
                  dependencies: editor.dependencies,
                  status: ["ready", "blocked"].includes(editor.status)
                    ? editor.status
                    : undefined,
                  version: editor.version,
                  action: editor.id ? "update" : "create",
                  task_id: editor.id,
                  owner: editor.owner || null,
                },
                () => setEditor(null),
              );
            }}
          >
            <TextInput
              label="Title"
              required
              maxLength={160}
              value={editor.title}
              onChange={(e) => setEditor({ ...editor, title: e.target.value })}
            />
            <Textarea
              label="Description"
              autosize
              minRows={4}
              value={editor.description}
              onChange={(e) =>
                setEditor({ ...editor, description: e.target.value })
              }
            />
            <NativeSelect
              label="Owner"
              value={editor.owner}
              onChange={(e) => setEditor({ ...editor, owner: e.target.value })}
            >
              <option value="">Unassigned</option>
              {team.map((a) => (
                <option key={a.id} value={a.id}>
                  {a.name}
                </option>
              ))}
            </NativeSelect>
            <MultiSelect
              label="Dependencies"
              searchable
              data={tasks
                .filter((t) => t.id !== editor.id)
                .map((t) => ({ value: t.id, label: t.title }))}
              value={editor.dependencies || []}
              onChange={(dependencies) =>
                setEditor({ ...editor, dependencies })
              }
            />
            {editor.id && (
              <NativeSelect
                label="Availability"
                value={
                  ["ready", "blocked"].includes(editor.status)
                    ? editor.status
                    : "unchanged"
                }
                onChange={(e) =>
                  setEditor({ ...editor, status: e.target.value })
                }
                data={[
                  { value: "unchanged", label: "Keep current status" },
                  { value: "ready", label: "Ready" },
                  { value: "blocked", label: "Blocked" },
                ]}
              />
            )}
            <Button variant="filled" type="submit" loading={saving}>
              Save work
            </Button>
          </form>
        )}
      </Modal>
      <Modal
        opened={!!submission}
        onClose={() => !saving && setSubmission(null)}
        title="Submit a result"
        size="lg"
      >
        {submission && (
          <form
            className="workspace-form"
            onSubmit={(e) => {
              e.preventDefault();
              void submit(
                {
                  ...submission,
                  action: "submit",
                  files: submission.files
                    .split("\n")
                    .map((s: string) => s.trim())
                    .filter(Boolean),
                },
                () => setSubmission(null),
              );
            }}
          >
            <p className="workspace-muted">
              Submission requires a separate acceptance decision.
            </p>
            <Textarea
              label="Result"
              required
              minRows={3}
              value={submission.result}
              onChange={(e) =>
                setSubmission({ ...submission, result: e.target.value })
              }
            />
            <Textarea
              label="Checks and evidence"
              required
              minRows={3}
              value={submission.checks}
              onChange={(e) =>
                setSubmission({ ...submission, checks: e.target.value })
              }
            />
            <TextInput
              label="Revision or artifact identity"
              required
              value={submission.revision}
              onChange={(e) =>
                setSubmission({ ...submission, revision: e.target.value })
              }
            />
            <Textarea
              label="Report files"
              description="One path per line, relative to the owner's workspace."
              value={submission.files}
              onChange={(e) =>
                setSubmission({ ...submission, files: e.target.value })
              }
            />
            <Button variant="filled" type="submit" loading={saving}>
              Submit for acceptance
            </Button>
          </form>
        )}
      </Modal>
      <Modal
        opened={!!decision}
        onClose={() => !saving && setDecision(null)}
        title={
          decision?.action === "accept"
            ? "Accept this result"
            : "Request changes"
        }
      >
        {decision && (
          <form
            className="workspace-form"
            onSubmit={(e) => {
              e.preventDefault();
              void submit(decision, () => setDecision(null));
            }}
          >
            <Textarea
              label="Decision and evidence"
              required
              minRows={3}
              value={decision.result}
              onChange={(e) =>
                setDecision({ ...decision, result: e.target.value })
              }
            />
            <Button
              variant="filled"
              type="submit"
              color={decision.action === "accept" ? "teal" : "orange"}
              loading={saving}
            >
              {decision.action === "accept"
                ? "Accept result"
                : "Request changes"}
            </Button>
          </form>
        )}
      </Modal>
    </>
  );
}

function Changes(c: Context) {
  const state = useResource(
      `${endpoint("changes", c.selected)}&scope=chat`,
      c.revision,
    ),
    comments = useResource(endpoint("workspace", c.selected), c.revision);
  const [path, setPath] = useState(""),
    [comment, setComment] = useState<Json | null>(null),
    [saving, setSaving] = useState(false);
  const report = state.data?.scope === "chat" ? state.data : null;
  const files: Json[] = report?.files || [];
  const annotations: Json[] = (comments.data?.annotations || []).filter(
    (entry: Json) => entry.agent === c.selected?.id,
  );
  let line = 0,
    current = "",
    inHunk = false;
  const rows = String(report?.diff || "")
    .split("\n")
    .map((text) => {
      if (text.startsWith("diff --git ")) {
        current = "";
        inHunk = false;
      }
      if (!inHunk && text.startsWith("+++ b/")) current = text.slice(6);
      const hunk = text.match(/^@@ -\d+(?:,\d+)? \+(\d+)/);
      if (hunk) {
        line = Number(hunk[1]) - 1;
        inHunk = true;
      }
      const newLine =
        inHunk && !hunk && (text.startsWith("+") || text.startsWith(" "));
      if (newLine) line++;
      return { text, line, path: current, commentable: !!current && newLine };
    });
  return (
    <>
      <ResourceState state={state} />
      {state.data && !report ? (
        <Empty>Chat changes require the updated server.</Empty>
      ) : report?.git === false ? (
        <Empty>{report.error || "Could not read reported changes."}</Empty>
      ) : (
        <>
          <div className="workspace-toolbar">
            <span className="workspace-muted">
              {files.length} reported files
              {report?.reportedAt && <> · {date(report.reportedAt)}</>}
            </span>
            <TextInput
              aria-label="Open a file"
              placeholder="File path"
              value={path}
              onChange={(e) => setPath(e.target.value)}
              rightSection={
                <UnstyledButton
                  aria-label="Preview file"
                  disabled={!path.trim()}
                  onClick={() => c.preview({ agent: c.selected!.id, path })}
                >
                  <ChevronRight size={16} />
                </UnstyledButton>
              }
              onKeyDown={(e) => {
                if (e.key === "Enter" && path.trim())
                  c.preview({ agent: c.selected!.id, path });
              }}
            />
          </div>
          {report?.truncated && (
            <p className="workspace-muted">
              Showing the first 300,000 characters of the reported diff.
            </p>
          )}
          <div className="workspace-file-list">
            {files.map((file) => (
              <Button
                key={file.path}
                size="xs"
                variant="subtle"
                onClick={() =>
                  c.preview({ agent: c.selected!.id, path: file.path })
                }
              >
                <code>{file.status}</code>&nbsp; {file.path}
              </Button>
            ))}
          </div>
          {report?.diff ? (
            <div
              className="workspace-diff"
              role="region"
              aria-label="Changes diff"
            >
              {rows.map((row, index) => (
                <div
                  className={`workspace-diff-line ${row.text.startsWith("+") ? "addition" : row.text.startsWith("-") ? "deletion" : row.text.startsWith("@@") ? "hunk" : ""}`}
                  key={index}
                >
                  <button
                    type="button"
                    disabled={!row.commentable}
                    aria-label={
                      row.commentable
                        ? `Comment on ${row.path} line ${row.line}`
                        : undefined
                    }
                    title={
                      row.commentable
                        ? `Comment on line ${row.line}`
                        : undefined
                    }
                    onClick={() =>
                      setComment({
                        path: row.path,
                        line: row.line,
                        turnId: report?.turnId,
                        text: "",
                        id: crypto.randomUUID(),
                      })
                    }
                  >
                    {row.commentable ? row.line : ""}
                  </button>
                  <code>{row.text || " "}</code>
                </div>
              ))}
            </div>
          ) : (
            !state.loading &&
            !state.error && (
              <Empty>
                {files.length
                  ? "Select a file to inspect its contents. No tracked text diff is available."
                  : "No file changes."}
              </Empty>
            )
          )}
        </>
      )}
      {!!annotations.length && (
        <section className="workspace-result">
          <h3>Comments sent to this agent</h3>
          {annotations.map((entry: Json) => (
            <article key={entry.id}>
              <small>
                {entry.path}:{entry.line}
              </small>
              <p className="workspace-prose">{entry.text}</p>
            </article>
          ))}
        </section>
      )}
      <Modal
        opened={!!comment}
        onClose={() => !saving && setComment(null)}
        title={comment ? `${comment.path}:${comment.line}` : "Line comment"}
      >
        {comment && (
          <form
            className="workspace-form"
            onSubmit={async (e) => {
              e.preventDefault();
              setSaving(true);
              try {
                await c.run("/api/annotation", {
                  agent: c.selected!.id,
                  ...comment,
                });
                setComment(null);
              } catch {
              } finally {
                setSaving(false);
              }
            }}
          >
            <Textarea
              label="Comment to the agent"
              required
              minRows={4}
              value={comment.text}
              onChange={(e) => setComment({ ...comment, text: e.target.value })}
            />
            <Button variant="filled" type="submit" loading={saving}>
              Send comment
            </Button>
          </form>
        )}
      </Modal>
    </>
  );
}

function Attention(c: Context) {
  const state = useResource(
    c.agent ? endpoint("workspace", c.agent) : null,
    c.revision,
  );
  const ids = new Set(c.data.threads.map((agent) => agent.id));
  const items: Json[] = (state.data?.inbox || []).filter((item: Json) =>
    ids.has(item.agent),
  );
  const groups = [
    ...new Set(
      items
        .filter((item) => !["request", "complaint"].includes(item.kind))
        .map((item) => item.kind),
    ),
  ];
  return (
    <>
      <ResourceState state={state} />
      <div className="workspace-toolbar">
        <span className="workspace-muted">
          Group new alerts when this browser tab is in the background.
        </span>
        <Button
          size="xs"
          variant="light"
          onClick={() => void c.toggleNotifications()}
        >
          {c.notifications ? "Disable desktop alerts" : "Enable desktop alerts"}
        </Button>
      </div>
      <Requests
        requests={c.data.runtime.requests}
        scopeAgentId={c.agent?.id}
        agents={c.data.threads}
        refresh={c.refresh}
        notify={c.notify}
      />
      {groups.map((kind) => (
        <section className="workspace-inbox-group" key={kind}>
          <h3>
            {String(kind).replaceAll("_", " ")}{" "}
            <span>{items.filter((item) => item.kind === kind).length}</span>
          </h3>
          {items
            .filter((item) => item.kind === kind)
            .map((item) => (
              <UnstyledButton
                className="workspace-row"
                key={`${kind}:${item.id}`}
                onClick={() => {
                  if (item.kind === "work")
                    c.navigate("work", item.agent, item.id);
                  else if (item.kind === "user_task")
                    c.navigate("user-tasks", item.agent, item.id);
                  else if (item.kind === "rule")
                    c.navigate("rules", item.agent, item.id);
                  else if (item.agent) {
                    c.onSelect(item.agent);
                    c.onClose();
                  }
                }}
              >
                <div className="workspace-row-head">
                  <strong>{item.title || item.kind}</strong>
                  <ChevronRight size={15} />
                </div>
                <p>{item.text}</p>
                <small>{ownerName(c.data, item.agent)}</small>
              </UnstyledButton>
            ))}
        </section>
      ))}
      <ComplaintBook data={c.data} refresh={c.refresh} notify={c.notify} />
      {!items.length &&
        !c.data.runtime.requests.length &&
        !c.data.runtime.complaints.some(complaintNeedsUserResponse) &&
        !state.loading && <Empty>No questions or unresolved problems.</Empty>}
    </>
  );
}

function Find(c: Context) {
  const sourceRequest = useRef(0);
  useEffect(
    () => () => {
      sourceRequest.current++;
    },
    [],
  );
  const [source, setSource] = useState<Json | null>(null),
    [sourceError, setSourceError] = useState("");
  const [query, setQuery] = useState(""),
    [search, setSearch] = useState("");
  const state = useResource(
    search ? `/api/search?q=${encodeURIComponent(search)}` : null,
    c.revision,
  );
  return (
    <>
      <form
        className="workspace-toolbar"
        onSubmit={(e) => {
          e.preventDefault();
          setSearch(query.trim());
        }}
      >
        <TextInput
          className="workspace-grow"
          autoFocus
          aria-label="Search all conversations"
          placeholder="Search messages, work, and agent chats"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          leftSection={<Search size={16} />}
        />
        <Button
          variant="filled"
          type="submit"
          disabled={!query.trim()}
          loading={state.loading}
        >
          Search
        </Button>
      </form>
      <ResourceState state={state} />
      {(state.data?.results || []).map((result: Json, index: number) => (
        <UnstyledButton
          className="workspace-row"
          key={`${result.kind}:${result.id}:${index}`}
          onClick={async () => {
            const request = ++sourceRequest.current;
            setSource({ ...result, loading: true });
            setSourceError("");
            try {
              const record = await api(
                `/api/search/item?id=${encodeURIComponent(result.id)}`,
              );
              if (request === sourceRequest.current)
                setSource({ ...result, ...record, loading: false });
            } catch (e) {
              if (request === sourceRequest.current) {
                setSourceError(errorText(e));
                setSource({ ...result, loading: false });
              }
            }
          }}
        >
          <div className="workspace-row-head">
            <Badge size="xs" variant="light" color="gray">
              {result.kind}
            </Badge>
            <small>{ownerName(c.data, result.agent)}</small>
          </div>
          <p className="workspace-prose">{result.text}</p>
        </UnstyledButton>
      ))}
      {search && state.data && !state.data.results?.length && (
        <Empty>No results for “{search}”.</Empty>
      )}
      {!search && (
        <Empty>
          Search the full message history, including archived conversations.
        </Empty>
      )}
      <Modal
        opened={!!source}
        onClose={() => {
          sourceRequest.current++;
          setSource(null);
        }}
        title="Search source"
        size="lg"
      >
        {source && (
          <>
            <div className="workspace-toolbar">
              <Badge variant="light" color="gray">
                {source.kind || source.type}
              </Badge>
              <small className="workspace-muted">{source.id}</small>
            </div>
            {sourceError && (
              <p role="alert" className="workspace-error">
                {sourceError}
              </p>
            )}
            {source.loading ? (
              <Loader size="sm" />
            ) : (
              <p className="workspace-prose">
                {typeof source.text === "string"
                  ? source.text
                  : JSON.stringify(source, null, 2)}
              </p>
            )}
            <Button
              variant="light"
              onClick={() => {
                if (source.kind === "work")
                  c.navigate("work", source.agent, source.id);
                else if (source.kind === "plan")
                  c.navigate("plan", source.agent);
                else {
                  c.onSelect(source.room || source.agent);
                  c.onClose();
                }
                setSource(null);
              }}
            >
              Open{" "}
              {source.kind === "work"
                ? "work"
                : source.kind === "plan"
                  ? "plan"
                  : "chat"}
            </Button>
          </>
        )}
      </Modal>
    </>
  );
}

function Plan(c: Context) {
  const state = useResource(endpoint("plan", c.selected), c.revision),
    [draft, setDraft] = useState(""),
    [version, setVersion] = useState<number | null>(null),
    [dirty, setDirty] = useState(false),
    [saving, setSaving] = useState(false);
  useEffect(() => {
    if (state.data && !dirty) {
      setDraft(state.data.text || "");
      setVersion(state.data.version || 0);
    }
  }, [state.data, dirty]);
  return (
    <>
      <ResourceState state={state} />
      {state.data && (
        <form
          className="workspace-form"
          onSubmit={async (e) => {
            e.preventDefault();
            setSaving(true);
            try {
              const result = await c.run("/api/plan", {
                agent: c.selected!.id,
                text: draft,
                version,
              });
              setVersion(result.version);
              setDirty(false);
            } catch {
            } finally {
              setSaving(false);
            }
          }}
        >
          <Textarea
            label="Shared plan"
            description="Edits use a version check so concurrent changes are not overwritten."
            autosize
            minRows={12}
            maxRows={28}
            value={draft}
            onChange={(e) => {
              setDraft(e.target.value);
              setDirty(true);
            }}
          />
          <div className="workspace-actions">
            <Button
              variant="filled"
              type="submit"
              loading={saving}
              disabled={!dirty}
            >
              Save plan
            </Button>
            {dirty && (
              <Button
                variant="subtle"
                onClick={() => {
                  setDirty(false);
                  c.reload();
                }}
              >
                Discard edits
              </Button>
            )}
            <span className="workspace-muted">Version {version}</span>
          </div>
        </form>
      )}
      {state.data?.native && (
        <section className="workspace-result">
          <h3>Agent's reported plan</h3>
          {Array.isArray(state.data.native.plan) ? (
            state.data.native.plan.map((step: Json, i: number) => (
              <div className="workspace-plan-step" key={i}>
                <Status value={step.status || "pending"} />
                <span>{step.step}</span>
              </div>
            ))
          ) : (
            <pre className="workspace-code">
              {JSON.stringify(state.data.native, null, 2)}
            </pre>
          )}
        </section>
      )}
    </>
  );
}

function Checkpoints(c: Context) {
  const state = useResource(endpoint("checkpoints", c.selected), c.revision),
    [label, setLabel] = useState(""),
    [preview, setPreview] = useState<Json | null>(null),
    [busy, setBusy] = useState(false);
  return (
    <>
      <ResourceState state={state} />
      <form
        className="workspace-toolbar"
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          try {
            await c.run("/api/checkpoint", {
              agent: c.selected!.id,
              label: label.trim() || "Manual checkpoint",
            });
            setLabel("");
          } catch {
          } finally {
            setBusy(false);
          }
        }}
      >
        <TextInput
          className="workspace-grow"
          aria-label="Checkpoint name"
          placeholder="Checkpoint name (optional)"
          value={label}
          onChange={(e) => setLabel(e.target.value)}
        />
        <Button
          variant="filled"
          type="submit"
          loading={busy}
          disabled={c.selected?.inFlight}
        >
          Save checkpoint
        </Button>
      </form>
      {(state.data?.checkpoints || []).map((checkpoint: Json) => (
        <div className="workspace-row" key={checkpoint.id}>
          <div className="workspace-row-head">
            <strong>{checkpoint.label}</strong>
            <Button
              size="compact-xs"
              variant="light"
              disabled={busy}
              onClick={async () => {
                setBusy(true);
                try {
                  const value = await c.run("/api/checkpoint/preview", {
                    agent: c.selected!.id,
                    checkpoint: checkpoint.id,
                  });
                  setPreview({
                    ...value,
                    checkpoint: checkpoint.id,
                    label: checkpoint.label,
                  });
                } catch {
                } finally {
                  setBusy(false);
                }
              }}
            >
              Preview restore
            </Button>
          </div>
          <small>
            {date(checkpoint.created)} · {checkpoint.commit?.slice(0, 12)}
          </small>
        </div>
      ))}
      {!state.data?.checkpoints?.length && !state.loading && !state.error && (
        <Empty>No checkpoints yet.</Empty>
      )}
      <Modal
        opened={!!preview}
        onClose={() => !busy && setPreview(null)}
        title={`Restore: ${preview?.label || "checkpoint"}`}
        size="xl"
      >
        {preview && (
          <>
            <p>
              Restore changes files and conversation state. The agent remains
              stopped after the restore.
            </p>
            <pre className="workspace-code workspace-diff-preview">
              {preview.diff || "No file changes."}
            </pre>
            {!preview.canRestore && (
              <p className="workspace-muted">
                A restore requires an idle agent with an isolated worktree.
              </p>
            )}
            <Button
              color="orange"
              disabled={!preview.canRestore}
              loading={busy}
              onClick={async () => {
                setBusy(true);
                try {
                  await c.run("/api/checkpoint/restore", {
                    agent: c.selected!.id,
                    checkpoint: preview.checkpoint,
                    expectedTree: preview.expectedTree,
                  });
                  setPreview(null);
                } catch {
                } finally {
                  setBusy(false);
                }
              }}
            >
              Restore this checkpoint
            </Button>
          </>
        )}
      </Modal>
    </>
  );
}

function Tools(c: Context) {
  const state = useResource(endpoint("capabilities", c.selected), c.revision),
    [query, setQuery] = useState("");
  return (
    <>
      <ResourceState state={state} />
      <TextInput
        aria-label="Filter tools"
        placeholder="Filter tools"
        leftSection={<Search size={15} />}
        value={query}
        onChange={(e) => setQuery(e.target.value)}
      />
      {(state.data?.errors || []).map((error: unknown, i: number) => (
        <p className="workspace-error" key={i}>
          {typeof error === "string" ? error : JSON.stringify(error)}
        </p>
      ))}
      {state.data && (
        <>
          <h3 className="workspace-section-title">Orchestration tools</h3>
          <div className="workspace-tools">
            {(state.data.managed || [])
              .filter((tool: Json) =>
                JSON.stringify(tool)
                  .toLowerCase()
                  .includes(query.toLowerCase()),
              )
              .map((tool: Json) => (
                <details key={tool.name} className="workspace-tool">
                  <summary>
                    <Wrench size={14} />
                    <strong>{tool.name}</strong>
                  </summary>
                  <p>{tool.description}</p>
                  <pre className="workspace-code">
                    {JSON.stringify(
                      tool.inputSchema || tool.parameters,
                      null,
                      2,
                    )}
                  </pre>
                </details>
              ))}
          </div>
          <h3 className="workspace-section-title">Observed native tools</h3>
          <p className="workspace-muted">{state.data.nativeInventory}</p>
          <div className="workspace-actions">
            {(state.data.observedNative || [])
              .filter((name: string) =>
                name.toLowerCase().includes(query.toLowerCase()),
              )
              .map((name: string) => (
                <Badge color="gray" variant="light" key={name}>
                  {name}
                </Badge>
              ))}
          </div>
          {!(state.data.observedNative || []).length && (
            <p className="workspace-muted">
              No native tool calls recorded for this agent.
            </p>
          )}
          <h3 className="workspace-section-title">Skills</h3>
          <Inventory value={state.data.skills} query={query} />
          <h3 className="workspace-section-title">Connected tools (MCP)</h3>
          <Inventory value={state.data.mcp} query={query} />
        </>
      )}
    </>
  );
}
function Inventory({ value, query }: { value: any; query: string }) {
  if (!value || (Array.isArray(value) && !value.length))
    return <p className="workspace-muted">No entries returned by Codex.</p>;
  const entries = Array.isArray(value)
    ? value
    : value.data || value.servers || [value];
  return (
    <div className="workspace-tools">
      {entries
        .filter((entry: any) =>
          JSON.stringify(entry).toLowerCase().includes(query.toLowerCase()),
        )
        .map((entry: any, index: number) => (
          <details className="workspace-tool" key={index}>
            <summary>
              <SlidersHorizontal size={14} />
              <strong>
                {entry.name ||
                  entry.cwd ||
                  entry.serverName ||
                  `Entry ${index + 1}`}
              </strong>
            </summary>
            <pre className="workspace-code">
              {JSON.stringify(entry, null, 2)}
            </pre>
          </details>
        ))}
    </div>
  );
}

function Profiles(c: Context) {
  const state = useResource("/api/profiles", c.revision),
    [draft, setDraft] = useState<Json | null>(null),
    [busy, setBusy] = useState(false),
    [remove, setRemove] = useState<Json | null>(null),
    [launch, setLaunch] = useState<Json | null>(null);
  const lead = c.data.threads.find(
    (a) => a.id === (c.selected?.rootId || c.selected?.id) && a.isLead,
  );
  return (
    <>
      <ResourceState state={state} />
      <div className="workspace-toolbar">
        <span className="workspace-muted">
          Apply profiles when you create workers.
        </span>
        <Button
          size="xs"
          variant="filled"
          color="indigo"
          leftSection={<Plus size={14} />}
          onClick={() =>
            setDraft({
              id: crypto.randomUUID(),
              isNew: true,
              name: "",
              role: "implementer",
              model: "gpt-5.6-sol",
              effort: "high",
              instructions: "",
            })
          }
        >
          New profile
        </Button>
      </div>
      {(state.data?.profiles || []).map((profile: Json) => (
        <div className="workspace-row" key={profile.id}>
          <div className="workspace-row-head">
            <strong>{profile.name}</strong>
            <div className="workspace-actions">
              <Button
                size="compact-xs"
                variant="light"
                disabled={!lead || lead.autoWake === false}
                onClick={() =>
                  setLaunch({
                    id: crypto.randomUUID(),
                    profile_id: profile.id,
                    name: profile.name,
                    prompt: "",
                  })
                }
              >
                Start worker
              </Button>
              <Button
                size="compact-xs"
                variant="subtle"
                onClick={() => setDraft({ ...profile })}
              >
                Edit
              </Button>
              <Button
                size="compact-xs"
                color="red"
                variant="subtle"
                onClick={() => setRemove(profile)}
              >
                Delete
              </Button>
            </div>
          </div>
          <small>
            {profile.role} · {profile.model} · {profile.effort}
          </small>
          <p className="workspace-clamp">{profile.instructions}</p>
        </div>
      ))}
      {!state.data?.profiles?.length && !state.loading && !state.error && (
        <Empty>No saved worker profiles.</Empty>
      )}
      <Modal
        opened={!!draft}
        onClose={() => !busy && setDraft(null)}
        title={
          draft?.id && !draft.isNew
            ? "Edit worker profile"
            : "New worker profile"
        }
        size="lg"
      >
        {draft && (
          <form
            className="workspace-form"
            onSubmit={async (e) => {
              e.preventDefault();
              setBusy(true);
              try {
                await c.run("/api/profiles", { ...draft, action: "save" });
                setDraft(null);
              } catch {
              } finally {
                setBusy(false);
              }
            }}
          >
            <TextInput
              label="Name"
              required
              value={draft.name}
              onChange={(e) => setDraft({ ...draft, name: e.target.value })}
            />
            <NativeSelect
              label="Role"
              value={draft.role}
              onChange={(e) => setDraft({ ...draft, role: e.target.value })}
              data={["implementer", "reviewer"]}
            />
            <TextInput
              label="Model"
              required
              value={draft.model}
              onChange={(e) => setDraft({ ...draft, model: e.target.value })}
            />
            <NativeSelect
              label="Reasoning effort"
              value={draft.effort}
              onChange={(e) => setDraft({ ...draft, effort: e.target.value })}
              data={["low", "medium", "high", "xhigh", "max", "ultra"]}
            />
            <Textarea
              label="Instructions"
              minRows={7}
              autosize
              value={draft.instructions}
              onChange={(e) =>
                setDraft({ ...draft, instructions: e.target.value })
              }
            />
            <Button variant="filled" type="submit" loading={busy}>
              Save profile
            </Button>
          </form>
        )}
      </Modal>
      <Modal
        opened={!!remove}
        onClose={() => !busy && setRemove(null)}
        title="Delete profile"
      >
        <p>
          Delete “{remove?.name}”? Existing agents keep their current
          instructions.
        </p>
        <Button
          color="red"
          loading={busy}
          onClick={async () => {
            setBusy(true);
            try {
              await c.run("/api/profiles", {
                id: remove?.id,
                action: "delete",
              });
              setRemove(null);
            } catch {
            } finally {
              setBusy(false);
            }
          }}
        >
          Delete profile
        </Button>
      </Modal>
      <Modal
        opened={!!launch}
        onClose={() => !busy && setLaunch(null)}
        title={`Start ${launch?.name || "worker"}`}
        size="lg"
      >
        {launch && (
          <form
            className="workspace-form"
            onSubmit={async (e) => {
              e.preventDefault();
              setBusy(true);
              try {
                const worker = await c.run("/api/agents", {
                  ...launch,
                  parent: lead!.id,
                  cwd: lead!.cwd,
                });
                setLaunch(null);
                c.notify(`Started ${worker.name}`);
              } catch {
              } finally {
                setBusy(false);
              }
            }}
          >
            <p className="workspace-muted">
              Lead: {lead?.name}. The worker uses this profile's model and
              instructions.
            </p>
            <Textarea
              label="Task for this worker"
              required
              minRows={5}
              value={launch.prompt}
              onChange={(e) => setLaunch({ ...launch, prompt: e.target.value })}
            />
            <Button variant="filled" type="submit" loading={busy}>
              Start worker
            </Button>
          </form>
        )}
      </Modal>
    </>
  );
}

function Rules(c: Context) {
  const state = useResource(endpoint("rules", c.selected), c.revision),
    [draft, setDraft] = useState<Json | null>(null),
    [busy, setBusy] = useState(false),
    [remove, setRemove] = useState<Json | null>(null);
  const act = async (rule: Json, action: string) => {
    setBusy(true);
    try {
      await c.run("/api/rules", { agent: c.selected!.id, id: rule.id, action });
      if (action === "delete") setRemove(null);
    } catch {
    } finally {
      setBusy(false);
    }
  };
  return (
    <>
      <ResourceState state={state} />
      <div className="workspace-toolbar">
        <span className="workspace-muted">
          The model does not run while a rule waits.
        </span>
        <Button
          size="xs"
          variant="filled"
          color="indigo"
          leftSection={<Plus size={14} />}
          onClick={() =>
            setDraft({
              id: crypto.randomUUID(),
              isNew: true,
              name: "",
              kind: "interval",
              intervalSeconds: 300,
              at: "",
              path: "",
              event: "worker_completed",
              command: "",
              text: "",
            })
          }
        >
          New rule
        </Button>
      </div>
      {(state.data?.rules || [])
        .filter((rule: Json) => rule.agent === c.selected?.id)
        .map((rule: Json) => (
          <div className="workspace-row" key={rule.id}>
            <div className="workspace-row-head">
              <strong>{rule.name}</strong>
              <Status
                value={
                  rule.status || (rule.enabled === false ? "paused" : "active")
                }
              />
            </div>
            <small>
              {rule.kind}
              {rule.intervalSeconds ? ` · every ${rule.intervalSeconds}s` : ""}
              {rule.nextAt ? ` · next ${date(rule.nextAt)}` : ""}
            </small>
            {rule.path && (
              <p>
                <code>{rule.path}</code>
              </p>
            )}
            {rule.error && <p className="workspace-error">{rule.error}</p>}
            <p className="workspace-clamp">{rule.text}</p>
            <p className="workspace-muted">
              {rule.checks || 0} checks · {rule.wakes || 0} agent turns
              {rule.lastExitCode != null
                ? ` · last exit ${rule.lastExitCode}`
                : ""}
            </p>
            {rule.lastOutput && (
              <details className="workspace-tool">
                <summary>Last check output</summary>
                <pre className="workspace-code">{rule.lastOutput}</pre>
              </details>
            )}
            <div className="workspace-actions">
              <Button
                size="compact-xs"
                variant="subtle"
                onClick={() =>
                  setDraft({
                    ...rule,
                    at: rule.at
                      ? new Date(
                          typeof rule.at === "number"
                            ? rule.at * 1000
                            : rule.at,
                        )
                          .toLocaleString("sv-SE")
                          .slice(0, 16)
                          .replace(" ", "T")
                      : "",
                  })
                }
              >
                Edit
              </Button>
              <Button
                size="compact-xs"
                variant="light"
                disabled={busy}
                onClick={() =>
                  void act(
                    rule,
                    rule.status === "paused" || rule.enabled === false
                      ? "resume"
                      : "pause",
                  )
                }
              >
                {rule.status === "paused" || rule.enabled === false
                  ? "Resume"
                  : "Pause"}
              </Button>
              <Button
                size="compact-xs"
                variant="subtle"
                color="red"
                onClick={() => setRemove(rule)}
              >
                Delete
              </Button>
            </div>
          </div>
        ))}
      {!(state.data?.rules || []).some(
        (rule: Json) => rule.agent === c.selected?.id,
      ) &&
        !state.loading &&
        !state.error && <Empty>No rules for this agent.</Empty>}
      <Modal
        opened={!!draft}
        onClose={() => !busy && setDraft(null)}
        title={draft?.id && !draft.isNew ? "Edit rule" : "New rule"}
        size="lg"
      >
        {draft && (
          <form
            className="workspace-form"
            onSubmit={async (e) => {
              e.preventDefault();
              setBusy(true);
              try {
                await c.run("/api/rules", {
                  ...draft,
                  agent: c.selected!.id,
                  action: "save",
                  at:
                    draft.kind === "once"
                      ? new Date(draft.at).getTime() / 1000
                      : undefined,
                });
                setDraft(null);
              } catch {
              } finally {
                setBusy(false);
              }
            }}
          >
            <TextInput
              label="Name"
              required
              value={draft.name}
              onChange={(e) => setDraft({ ...draft, name: e.target.value })}
            />
            <NativeSelect
              label="Trigger"
              value={draft.kind}
              onChange={(e) => setDraft({ ...draft, kind: e.target.value })}
              data={[
                { value: "interval", label: "Repeat at an interval" },
                { value: "once", label: "Once at a time" },
                { value: "file", label: "File changes" },
                { value: "event", label: "Runtime event" },
              ]}
            />
            {draft.kind === "interval" && (
              <NumberInput
                label="Interval (seconds)"
                min={10}
                required
                value={draft.intervalSeconds}
                onChange={(value) =>
                  setDraft({ ...draft, intervalSeconds: Number(value) })
                }
              />
            )}
            {draft.kind === "once" && (
              <TextInput
                label="Run at (local time)"
                type="datetime-local"
                required
                value={draft.at}
                onChange={(e) => setDraft({ ...draft, at: e.target.value })}
              />
            )}
            {draft.kind === "file" && (
              <TextInput
                label="File path"
                description="Relative to this agent's workspace."
                required
                value={draft.path}
                onChange={(e) => setDraft({ ...draft, path: e.target.value })}
              />
            )}
            {draft.kind === "event" && (
              <NativeSelect
                label="Runtime event"
                required
                value={draft.event}
                onChange={(e) => setDraft({ ...draft, event: e.target.value })}
                data={[
                  { value: "", label: "Select an event" },
                  "worker_completed",
                  "monitor_exit",
                  "complaint",
                  "work_review",
                ]}
              />
            )}
            <Textarea
              label="Script check (optional)"
              description={
                'Run under the agent permissions. Exit 0 wakes the agent unless the output contains {"wakeAgent":false}.'
              }
              minRows={3}
              value={draft.command}
              onChange={(e) => setDraft({ ...draft, command: e.target.value })}
            />
            <Textarea
              label="Message to the agent"
              required
              minRows={3}
              value={draft.text}
              onChange={(e) => setDraft({ ...draft, text: e.target.value })}
            />
            <Button variant="filled" type="submit" loading={busy}>
              Save rule
            </Button>
          </form>
        )}
      </Modal>
      <Modal
        opened={!!remove}
        onClose={() => !busy && setRemove(null)}
        title="Delete rule"
      >
        <p>Delete “{remove?.name}”?</p>
        <Button
          color="red"
          loading={busy}
          onClick={() => remove && void act(remove, "delete")}
        >
          Delete rule
        </Button>
      </Modal>
    </>
  );
}

function Resources(c: Context) {
  const state = useResource("/api/resources", c.revision),
    [resource, setResource] = useState(""),
    [note, setNote] = useState(""),
    [busy, setBusy] = useState(false);
  const board = state.data?.state,
    claims: Json[] = Array.isArray(board?.claims)
      ? board.claims
      : Object.entries(board?.claims || {}).map(([resource, value]) => ({
          resource,
          ...(value as Json),
        }));
  const act = async (action: string, name: string) => {
    if (!c.selected) return;
    setBusy(true);
    try {
      const response = await c.run("/api/resources", {
        agent: c.selected.id,
        action,
        resource: name,
        note,
      });
      if (response.ok === false) {
        c.notify(response.message || "The resource operation failed.");
        return;
      }
      c.notify(
        action === "claim"
          ? "Resource claimed."
          : action === "renew"
            ? "Resource renewed."
            : "Resource released.",
      );
      if (action === "claim") {
        setResource("");
        setNote("");
      }
    } catch {
    } finally {
      setBusy(false);
    }
  };
  return (
    <>
      <ResourceState state={state} />
      {state.data?.path && (
        <p className="workspace-muted workspace-wrap">
          Board: <code>{state.data.path}</code>
        </p>
      )}
      {!c.selected && (
        <p className="workspace-muted">
          Select an agent to claim or manage its resources.
        </p>
      )}
      <form
        className="workspace-form"
        onSubmit={(e) => {
          e.preventDefault();
          void act("claim", resource.trim());
        }}
      >
        <div className="workspace-toolbar">
          <TextInput
            className="workspace-grow"
            label="Resource"
            placeholder="Existing resource name"
            required
            value={resource}
            onChange={(e) => setResource(e.target.value)}
          />
          <Button
            variant="filled"
            type="submit"
            loading={busy}
            disabled={!c.selected || !resource.trim()}
          >
            Claim
          </Button>
        </div>
        <TextInput
          label="Purpose"
          value={note}
          onChange={(e) => setNote(e.target.value)}
        />
      </form>
      <h3 className="workspace-section-title">Active leases</h3>
      {claims.map((claim, index) => (
        <div
          className="workspace-row"
          key={claim.id || claim.resource || index}
        >
          <div className="workspace-row-head">
            <strong>{claim.resource || claim.name || claim.slot}</strong>
            <small>
              {ownerName(c.data, claim.worker || claim.owner || claim.agent)}
            </small>
          </div>
          <p>{claim.note || claim.purpose}</p>
          <small>{claim.expires ? `Expires ${date(claim.expires)}` : ""}</small>
          <div className="workspace-actions">
            <Button
              size="compact-xs"
              variant="light"
              disabled={
                !c.selected ||
                busy ||
                (claim.worker || claim.owner || claim.agent) !== c.selected.id
              }
              onClick={() => void act("renew", claim.resource || claim.name)}
            >
              Renew
            </Button>
            <Button
              size="compact-xs"
              variant="subtle"
              disabled={
                !c.selected ||
                busy ||
                (claim.worker || claim.owner || claim.agent) !== c.selected.id
              }
              onClick={() => void act("release", claim.resource || claim.name)}
            >
              Release
            </Button>
          </div>
        </div>
      ))}
      {!claims.length && board && <Empty>No active resource leases.</Empty>}
      {board?.queue && (
        <details className="workspace-tool">
          <summary>Wait queue</summary>
          <pre className="workspace-code">
            {JSON.stringify(board.queue, null, 2)}
          </pre>
        </details>
      )}
      {board?.notes && (
        <details className="workspace-tool">
          <summary>Board notes</summary>
          <pre className="workspace-code">
            {JSON.stringify(board.notes, null, 2)}
          </pre>
        </details>
      )}
    </>
  );
}
