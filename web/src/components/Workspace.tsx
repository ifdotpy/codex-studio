import ErrorDescription from "./ErrorDescription";
import { useWorkspaceResource as useResource } from "./useWorkspaceResource";
import { messageAttentionCount } from "../chatScope";
import {
  Badge,
  Button,
  Drawer,
  Loader,
  Modal,
  NativeSelect,
  NumberInput,
  Textarea,
  TextInput,
  UnstyledButton,
} from "@mantine/core";
import {
  BookOpen,
  ChevronRight,
  Clock3,
  FileDiff,
  Files,
  GitBranch,
  Inbox,
  Layers3,
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
import { useFormDraft } from "./useFormDraft";
import TeamChats from "./TeamChats";
import FilePreview, { type PreviewTarget } from "./FilePreview";
import "./Workspace.css";

type Props = {
  initialSection?: string;
  initialFocus?: { id: string; requestId: string; roomId?: string };
  opened: boolean;
  onClose: () => void;
  agent?: Agent;
  data: Snapshot;
  allRequests: Json[];
  onSelect: (id: string, messageId?: string) => void;
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
  resourceCache: Map<string, Json>;
};
const sections = [
  ["changes", "Changes", FileDiff],
  ["messages", "Messages", Inbox],
  ["search", "Search", Search],
  ["plan", "Plan", BookOpen],
  ["checkpoints", "Checkpoints", GitBranch],
  ["tools", "Tools", Wrench],
  ["profiles", "Profiles", Users],
  ["rules", "Rules", Clock3],
] as const;
const descriptions: Record<string, string> = {
  changes: "Latest changes by this agent.",
  messages: "Your messages and this team’s conversations.",
  search: "Find messages, work, plans, and agent conversations.",
  plan: "The agent's current plan.",
  checkpoints: "Save and restore points in the work.",
  tools: "Tools the agents can use.",
  profiles: "Reusable instructions and model choices for subagents.",
  rules: "Wake the agent on a time, file change, or event.",
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
  const resourceCache = useRef(new Map<string, Json>()).current;
  const [section, setSection] = useState(props.initialSection || "messages"),
    [agentId, setAgentId] = useState(props.agent?.id || ""),
    [revision, setRevision] = useState(0),
    [preview, setPreview] = useState<PreviewTarget | null>(null);
  const [pending, setPending] = useState(0),
    [focusId, setFocusId] = useState("");
  useEffect(() => {
    if (props.opened && props.initialFocus) setFocusId(props.initialFocus.id);
  }, [props.opened, props.initialFocus?.requestId]);
  const navRef = useRef<HTMLElement>(null);
  useEffect(() => {
    if (props.opened)
      navRef.current
        ?.querySelector('[aria-current="page"]')
        ?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [section, props.opened]);
  const refreshRef = useRef(props.refresh),
    notifyRef = useRef(props.notify);
  refreshRef.current = props.refresh;
  notifyRef.current = props.notify;
  const [previousEntry, setPreviousEntry] = useState({
    opened: props.opened,
    agent: props.agent?.id,
    section: props.initialSection,
  });
  if (
    previousEntry.opened !== props.opened ||
    previousEntry.agent !== props.agent?.id ||
    previousEntry.section !== props.initialSection
  ) {
    setPreviousEntry({
      opened: props.opened,
      agent: props.agent?.id,
      section: props.initialSection,
    });
    if (props.opened) {
      setAgentId(props.agent?.id || "");
      if (
        props.initialSection &&
        (!previousEntry.opened ||
          previousEntry.section !== props.initialSection)
      ) {
        setSection(props.initialSection);
        setFocusId("");
      }
    }
  }
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
    resourceCache,
    navigate: (section, agent, item) => {
      setSection(section);
      if (agent) setAgentId(agent);
      setFocusId(item || "");
    },
  };
  const needAgent = [
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
      closeButtonProps={{ "aria-label": "Close" }}
      onClose={props.onClose}
      position="right"
      size="min(1180px, 100vw)"
      title={
        <span className="workspace-drawer-title">
          {section === "messages" ? <Inbox size={19} /> : <Layers3 size={19} />}{" "}
          {section === "messages" ? "Messages" : "Workspace"}
        </span>
      }
      className={`workspace-drawer ${section === "messages" ? "workspace-messages-drawer" : ""}`}
    >
      <div
        className={`workspace-shell ${section === "messages" ? "workspace-focused-messages" : ""}`}
      >
        {section !== "messages" && (
          <nav
            ref={navRef}
            className="workspace-nav"
            aria-label="Workspace sections"
          >
            {sections.map(([id, label, Icon]) => (
              <UnstyledButton
                key={id}
                className={`workspace-nav-item ${section === id ? "selected" : ""}`}
                onClick={() => setSection(id)}
                aria-current={section === id ? "page" : undefined}
              >
                <Icon size={17} />
                <span>{label}</span>
                {id === "messages" && messageAttentionCount(props.data) > 0 && (
                  <span className="workspace-count">
                    {messageAttentionCount(props.data)}
                  </span>
                )}
              </UnstyledButton>
            ))}
          </nav>
        )}
        <main
          className={`workspace-content ${section === "messages" ? "workspace-messages" : ""}`}
        >
          <header className="workspace-heading">
            <div>
              {section !== "messages" && <h2>{title}</h2>}
              <p>
                {section === "messages"
                  ? props.data.threads.find((agent) => agent.isLead)?.name ||
                    descriptions[section]
                  : descriptions[section]}
              </p>
            </div>
            <Button
              variant="subtle"
              size="compact-sm"
              aria-label={
                section === "messages"
                  ? "Refresh messages"
                  : "Refresh workspace"
              }
              onClick={() => {
                reload();
                if (section === "messages")
                  void refreshRef
                    .current()
                    .catch((error) => notifyRef.current(errorText(error)));
              }}
            >
              <RefreshCw size={16} />
            </Button>
          </header>
          {section !== "messages" && (
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
                      {a.isLead ? "Main agent · " : ""}
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
            <div
              className={
                section === "messages" ? "workspace-message-body" : undefined
              }
              key={`${section}:${section === "messages" ? "team" : selected?.id || "all"}`}
            >
              {section === "changes" && <Changes {...context} />}
              {section === "messages" && (
                <TeamChats
                  refresh={props.refresh}
                  notify={props.notify}
                  data={props.data}
                  focusItemId={props.initialFocus?.id}
                  focusRequestId={props.initialFocus?.requestId}
                  focusRoomId={props.initialFocus?.roomId}
                  leadId={
                    props.agent?.rootId ||
                    (props.agent?.isLead ? props.agent.id : undefined)
                  }
                />
              )}
              {section === "search" && <Find {...context} />}
              {section === "plan" && <Plan {...context} />}
              {section === "checkpoints" && <Checkpoints {...context} />}
              {section === "tools" && <Tools {...context} />}
              {section === "profiles" && <Profiles {...context} />}
              {section === "rules" && <Rules {...context} />}
            </div>
          )}
        </main>
      </div>
      <FilePreview target={preview} onClose={() => setPreview(null)} />
    </Drawer>
  );
}
export default Workspace;

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
        <Empty>
          <ErrorDescription
            value={report.error || "Could not read reported changes."}
          />
        </Empty>
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
            state.data !== null && (
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
      {!search && <Empty>Searches all messages, archived chats too.</Empty>}
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
                if (source.kind === "plan") c.navigate("plan", source.agent);
                else {
                  c.onSelect(source.room || source.agent, source.id);
                  c.onClose();
                }
                setSource(null);
              }}
            >
              Open {source.kind === "plan" ? "plan" : "chat"}
            </Button>
          </>
        )}
      </Modal>
    </>
  );
}

function Plan(c: Context) {
  const state = useResource(endpoint("plan", c.selected), c.revision);
  const native = state.data?.native;
  const steps: Json[] = Array.isArray(native?.plan) ? native.plan : [];
  const explanation =
    typeof native?.explanation === "string" ? native.explanation : "";
  return (
    <>
      <ResourceState state={state} />
      <div className="workspace-actions">
        <Button
          variant="light"
          disabled={!c.selected}
          onClick={() => {
            if (!c.selected) return;
            c.onSelect(c.selected.id);
            c.onClose();
          }}
        >
          Change plan in chat
        </Button>
      </div>
      {steps.length || explanation ? (
        <section className="workspace-result">
          <h3>Agent plan</h3>
          {explanation && <p className="workspace-prose">{explanation}</p>}
          {steps.map((step: Json, i: number) => (
            <div className="workspace-plan-step" key={i}>
              <Status value={step.status || "pending"} />
              <span>{step.step}</span>
            </div>
          ))}
        </section>
      ) : (
        state.data !== null && (
          <Empty>This agent has not reported a plan.</Empty>
        )
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
      {!state.data?.checkpoints?.length && state.data !== null && (
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
            <p>Restores files and chat state. The agent stays stopped.</p>
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
        <ErrorDescription className="workspace-error" key={i} value={error} />
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
    [draft, setDraft] = useFormDraft(
      `studio-profile-draft:${c.data.stateDir}`,
      c.notify,
    ),
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
          Apply profiles when you create subagents.
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
                Start subagent
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
      {!state.data?.profiles?.length && state.data !== null && (
        <Empty>No saved subagent profiles.</Empty>
      )}
      <Modal
        opened={!!draft}
        onClose={() => !busy && setDraft(null)}
        title={
          draft?.id && !draft.isNew
            ? "Edit subagent profile"
            : "New subagent profile"
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
                setDraft((current) => (current === draft ? null : current));
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
        title={`Start ${launch?.name || "subagent"}`}
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
              Main agent: {lead?.name}. The subagent uses this profile's model
              and instructions.
            </p>
            <Textarea
              label="Task for this subagent"
              required
              minRows={5}
              value={launch.prompt}
              onChange={(e) => setLaunch({ ...launch, prompt: e.target.value })}
            />
            <Button variant="filled" type="submit" loading={busy}>
              Start subagent
            </Button>
          </form>
        )}
      </Modal>
    </>
  );
}

function Rules(c: Context) {
  const state = useResource(endpoint("rules", c.selected), c.revision),
    [draft, setDraft] = useFormDraft(
      `studio-rule-draft:${JSON.stringify([c.data.stateDir, c.selected?.id])}`,
      c.notify,
    ),
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
              minimumWorkers: 8,
              durationMinutes: 30,
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
              {rule.kind === "low_workers"
                ? `Fewer than ${rule.minimumWorkers ?? 8} active subagents for more than ${rule.durationMinutes ?? 30} minutes`
                : rule.kind}
              {rule.kind !== "low_workers" && rule.intervalSeconds
                ? ` · every ${rule.intervalSeconds}s`
                : ""}
              {rule.kind !== "low_workers" && rule.nextAt
                ? ` · next ${date(rule.nextAt)}`
                : ""}
            </small>
            {rule.path && (
              <p>
                <code>{rule.path}</code>
              </p>
            )}
            {rule.error && (
              <ErrorDescription
                className="workspace-error"
                value={rule.error}
              />
            )}
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
        state.data !== null && <Empty>No rules for this agent.</Empty>}
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
                  command: draft.kind === "low_workers" ? "" : draft.command,
                  at:
                    draft.kind === "once"
                      ? new Date(draft.at).getTime() / 1000
                      : undefined,
                });
                setDraft((current) => (current === draft ? null : current));
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
              onChange={(e) => {
                const kind = e.target.value;
                setDraft({
                  ...draft,
                  kind,
                  ...(kind === "low_workers"
                    ? {
                        minimumWorkers: draft.minimumWorkers ?? 8,
                        durationMinutes: draft.durationMinutes ?? 30,
                        name: draft.name.trim()
                          ? draft.name
                          : "Too few active subagents",
                        text: draft.text.trim()
                          ? draft.text
                          : "Fewer subagents than the minimum were active.",
                      }
                    : {}),
                });
              }}
              data={[
                { value: "interval", label: "Repeat at an interval" },
                { value: "once", label: "Once at a time" },
                { value: "file", label: "File changes" },
                { value: "event", label: "Runtime event" },
                ...(c.selected?.isLead
                  ? [
                      {
                        value: "low_workers",
                        label: "Too few active subagents",
                      },
                    ]
                  : []),
              ]}
            />
            {draft.kind === "low_workers" && (
              <>
                <NumberInput
                  label="Minimum active subagents"
                  min={1}
                  max={255}
                  allowDecimal={false}
                  allowNegative={false}
                  required
                  value={draft.minimumWorkers ?? 8}
                  onChange={(value) =>
                    setDraft({ ...draft, minimumWorkers: value })
                  }
                />
                <NumberInput
                  label="Duration (minutes)"
                  min={1}
                  max={525600}
                  allowDecimal={false}
                  allowNegative={false}
                  required
                  value={draft.durationMinutes ?? 30}
                  onChange={(value) =>
                    setDraft({ ...draft, durationMinutes: value })
                  }
                />
                <p className="workspace-muted">
                  Alerts the main agent once when fewer subagents than the
                  minimum are active.
                </p>
              </>
            )}
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
                  {
                    value: "worker_completed",
                    label: "Subagent completes a task",
                  },
                  "monitor_exit",
                  "complaint",
                  "work_review",
                ]}
              />
            )}
            {draft.kind !== "low_workers" && (
              <Textarea
                label="Script check (optional)"
                description={
                  'Run under the agent permissions. Exit 0 wakes the agent unless the output contains {"wakeAgent":false}.'
                }
                minRows={3}
                value={draft.command}
                onChange={(e) =>
                  setDraft({ ...draft, command: e.target.value })
                }
              />
            )}
            <Textarea
              label={
                draft.kind === "low_workers"
                  ? "Message to the main agent"
                  : "Message to the agent"
              }
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
