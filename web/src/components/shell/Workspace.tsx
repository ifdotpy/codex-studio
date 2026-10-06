import { EmptyState, PanelHeader } from "../ui/primitives";
import { localDateTime } from "../../local-time";
import ErrorDescription from "../ErrorDescription";
import { useWorkspaceResource as useResource } from "../useWorkspaceResource";
import { messageAttentionCount } from "../../chatScope";
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
import {
  get,
  post,
  errorText,
  type ApiPostPath,
  type GetResult,
  type PostBody,
  type PostResult,
} from "../../api";
import type { Agent, Json, JsonValue, Snapshot } from "../../types";
import { useFormDraft } from "../useFormDraft";
import { ModelPicker, type ModelOption } from "../ModelPicker";
import { useWorkerModels } from "../agents/WorkerModelPicker";
import { watchResourceReads } from "../watchResourceReads";
import TeamChats from "./messages/TeamChats";
import FilePreview, { type PreviewTarget } from "../FilePreview";
import "./Workspace.css";

type Props = {
  initialSection?: string;
  initialFocus?: { id: string; requestId: string; roomId?: string };
  opened: boolean;
  onClose: () => void;
  agent?: Agent;
  data: Snapshot;
  allRequests: NonNullable<Snapshot["runtime"]>["requests"];
  onSelect: (id: string, messageId?: string) => void;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
};
type WorkspaceAction = <Path extends ApiPostPath>(
  path: Path,
  body: PostBody<Path>,
) => Promise<PostResult<Path>>;
type Context = Props & {
  selected: Agent | undefined;
  run: WorkspaceAction;
  reload: () => void;
  revision: number;
  preview: (target: PreviewTarget) => void;
  navigate: (section: string, agent?: string, item?: string) => void;
  focusId: string;
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
  profiles: "Reusable instructions and model choices for workers.",
  rules: "When the agent resumes",
};
const date = (value: number | string | undefined) =>
  value
    ? localDateTime(new Date(typeof value === "number" ? value * 1000 : value))
    : "";
function Empty({ children }: { children: React.ReactNode }) {
  return <EmptyState title={children} />;
}
function ResourceState({
  state,
}: {
  state: { error: string; loading: boolean; data: unknown };
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
  const [section, setSection] = useState(props.initialSection || "messages"),
    [agentId, setAgentId] = useState(props.agent?.id || ""),
    [revision, setRevision] = useState(0),
    [preview, setPreview] = useState<PreviewTarget | null>(null);
  const [pending, setPending] = useState(0),
    [focusId, setFocusId] = useState(""),
    [filePath, setFilePath] = useState("");
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
    if (!props.opened || !agentId) return;
    return watchResourceReads(
      { kind: "workspace", agentId },
      async () => reload(),
      () => {
        // The current panel remains visible until its next workspace change.
      },
    );
  }, [props.opened, agentId, reload]);
  const run = useCallback(
    async <Path extends ApiPostPath>(
      path: Path,
      body: PostBody<Path>,
    ): Promise<PostResult<Path>> => {
      setPending((n) => n + 1);
      try {
        const result = await post(path, body);
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
    <Drawer.Root
      opened={props.opened}
      onClose={props.onClose}
      position="right"
      size={
        section === "changes" || section === "messages"
          ? "min(1040px, 100vw)"
          : "min(840px, 100vw)"
      }
      className={`workspace-drawer ${section === "messages" ? "workspace-messages-drawer" : ""}`}
    >
      <Drawer.Overlay />
      <Drawer.Content>
        <Drawer.Header>
          <Drawer.Title>
            {" "}
            <span className="workspace-drawer-title">
              {section === "messages" ? (
                <Inbox size={19} />
              ) : (
                <Layers3 size={19} />
              )}{" "}
              {section === "messages" ? "Messages" : "Workspace"}
            </span>
          </Drawer.Title>
          {section === "messages" && (
            <Button
              variant="subtle"
              size="compact-sm"
              className="workspace-messages-refresh"
              aria-label="Refresh messages"
              onClick={() => {
                reload();
                void refreshRef
                  .current()
                  .catch((error) => notifyRef.current(errorText(error)));
              }}
            >
              <RefreshCw size={16} />
            </Button>
          )}
          <Drawer.CloseButton aria-label="Close" />
        </Drawer.Header>
        <Drawer.Body>
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
                    {id === "messages" &&
                      messageAttentionCount(props.data) > 0 && (
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
              {section !== "messages" && (
                <div className="workspace-heading">
                  <div>
                    <PanelHeader title={title} help={descriptions[section]} />
                    {section !== "messages" && section !== "profiles" && (
                      <div className="workspace-scope">
                        <NativeSelect
                          aria-label="Agent"
                          size="xs"
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
                            size="compact-xs"
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
                  </div>
                  {section === "plan" && (
                    <Button
                      variant="subtle"
                      size="compact-sm"
                      disabled={!selected}
                      onClick={() => {
                        if (selected) {
                          props.onSelect(selected.id);
                          props.onClose();
                        }
                      }}
                    >
                      Change plan in chat
                    </Button>
                  )}
                  {section === "changes" && selected && (
                    <TextInput
                      aria-label="Open a file"
                      placeholder="File path"
                      value={filePath}
                      onChange={(e) => setFilePath(e.target.value)}
                      rightSection={
                        <UnstyledButton
                          aria-label="Preview file"
                          disabled={!filePath.trim()}
                          onClick={() =>
                            setPreview({ agent: selected.id, path: filePath })
                          }
                        >
                          <ChevronRight size={16} />
                        </UnstyledButton>
                      }
                      onKeyDown={(e) => {
                        if (e.key === "Enter" && filePath.trim())
                          setPreview({ agent: selected.id, path: filePath });
                      }}
                    />
                  )}
                  <Button
                    variant="subtle"
                    size="compact-sm"
                    aria-label="Refresh workspace"
                    onClick={reload}
                  >
                    <RefreshCw size={16} />
                  </Button>
                </div>
              )}
              {needAgent && !selected ? (
                <Empty>
                  Select an agent to view its {title?.toLowerCase()}.
                </Empty>
              ) : (
                <div
                  className={
                    section === "messages"
                      ? "workspace-message-body"
                      : undefined
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
        </Drawer.Body>
      </Drawer.Content>
    </Drawer.Root>
  );
}
export default Workspace;

function Changes(c: Context) {
  const state = useResource("/api/changes", c.revision, {
      query: {
        ...(c.selected ? { agent: c.selected.id } : {}),
        scope: "chat",
      },
    }),
    comments = useResource("/api/workspace", c.revision, {
      query: {
        ...(c.selected ? { agent: c.selected.id } : {}),
        view: "annotations",
      },
    });
  const [comment, setComment] = useState<{
      path: string;
      line: number;
      turnId?: string;
      text: string;
      id: string;
    } | null>(null),
    [saving, setSaving] = useState(false);
  const report = state.data?.scope === "chat" ? state.data : null;
  const files = (report?.files || []).filter(
    (file) => typeof file.path === "string" && typeof file.status === "string",
  );
  const annotations = (comments.data?.annotations || []).filter(
    (entry) =>
      entry.agent === c.selected?.id &&
      typeof entry.id === "string" &&
      typeof entry.path === "string" &&
      typeof entry.line === "number" &&
      typeof entry.text === "string",
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
      const headerPath = text.startsWith("diff --git ")
        ? text.match(/ b\/(.*)$/)?.[1] || text.slice(11)
        : undefined;
      return {
        text,
        line,
        path: current,
        headerPath,
        commentable: !!current && newLine,
      };
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
              {files.length} reported {files.length === 1 ? "file" : "files"}
              {report?.reportedAt && <> · {date(report.reportedAt)}</>}
            </span>
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
                  className={`workspace-diff-line ${row.headerPath ? "file-header" : !row.commentable && !row.text.startsWith("@@") ? "patch-metadata" : ""} ${row.text.startsWith("+") ? "addition" : row.text.startsWith("-") ? "deletion" : row.text.startsWith("@@") ? "hunk" : ""}`}
                  key={index}
                >
                  {row.headerPath ? (
                    <>
                      <code className="workspace-diff-status">
                        {files.find((file) => file.path === row.headerPath)
                          ?.status || "M"}
                      </code>
                      <span>{row.headerPath}</span>
                    </>
                  ) : (
                    <>
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
                            ...(typeof report?.turnId === "string"
                              ? { turnId: report.turnId }
                              : {}),
                            text: "",
                            id: crypto.randomUUID(),
                          })
                        }
                      >
                        {row.commentable ? row.line : ""}
                      </button>
                      <code>{row.text || " "}</code>
                    </>
                  )}
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
          {annotations.map((entry) => (
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
  type SearchResult = GetResult<"/api/search">["results"][number];
  type SearchItem = GetResult<"/api/search/item">;
  type SearchSource =
    | (SearchResult & { loading: boolean })
    | (SearchItem & { loading: boolean });
  const [source, setSource] = useState<SearchSource | null>(null),
    [sourceError, setSourceError] = useState("");
  const [query, setQuery] = useState(""),
    [search, setSearch] = useState("");
  const state = useResource(search ? "/api/search" : null, c.revision, {
    query: { q: search },
  });
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
      {(state.data?.results || []).map((result, index) => (
        <UnstyledButton
          className="workspace-row"
          key={`${result.kind}:${result.id}:${index}`}
          onClick={async () => {
            const request = ++sourceRequest.current;
            setSource({ ...result, loading: true });
            setSourceError("");
            try {
              const record = await get("/api/search/item", {
                query: { id: result.id },
              });
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
                {source.kind || ("type" in source ? source.type : "")}
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
  const state = useResource("/api/plan", c.revision, {
    query: c.selected ? { agent: c.selected.id } : {},
  });
  const native = state.data?.native;
  const steps = native?.plan ?? [];
  const explanation = native?.explanation ?? "";
  return (
    <>
      <ResourceState state={state} />
      {steps.length || explanation ? (
        <section className="workspace-result">
          <h3>Agent plan</h3>
          {explanation && <p className="workspace-prose">{explanation}</p>}
          {steps.map((step, i) => {
            const detail = isRecord(step) ? step : null;
            const status =
              typeof detail?.status === "string" ? detail.status : "pending";
            const text =
              typeof detail?.step === "string"
                ? detail.step
                : JSON.stringify(step);
            return (
              <div className="workspace-plan-step" key={i}>
                <Status value={status} />
                <span>{text}</span>
              </div>
            );
          })}
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
  const state = useResource("/api/checkpoints", c.revision, {
      query: c.selected ? { agent: c.selected.id } : {},
    }),
    [label, setLabel] = useState(""),
    [preview, setPreview] = useState<CheckpointPreview | null>(null),
    [busy, setBusy] = useState(false);
  return (
    <>
      <ResourceState state={state} />
      <p className="workspace-muted">
        Save the project files and chat history. Preview changes before you
        restore a checkpoint.
      </p>
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
          color="indigo"
          type="submit"
          loading={busy}
          disabled={!!c.selected?.inFlight}
        >
          Save checkpoint
        </Button>
      </form>
      {(state.data?.checkpoints || []).map((checkpoint) => (
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

type CheckpointPreview = Omit<
  PostResult<"/api/checkpoint/preview">,
  "checkpoint"
> & { checkpoint: string; label: string };

function Tools(c: Context) {
  const state = useResource("/api/capabilities", c.revision, {
      query: c.selected ? { agent: c.selected.id } : {},
    }),
    [query, setQuery] = useState("");
  const managedTools = (state.data?.managed ?? []).filter(
    (value): value is Record<string, JsonValue> => isJsonObject(value),
  );
  const observedNative = (state.data?.observedNative ?? []).filter(
    (name): name is string => typeof name === "string",
  );
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
      {!!state.data?.errors?.length && (
        <details className="workspace-error">
          <summary>Some tools could not load. View details.</summary>
          {state.data.errors.map((error: unknown, i: number) => (
            <ErrorDescription key={i} value={error} />
          ))}
        </details>
      )}
      {state.data && (
        <>
          <h3 className="workspace-section-title">Orchestration tools</h3>
          <div className="workspace-tools">
            {managedTools
              .filter((tool) =>
                JSON.stringify(tool)
                  .toLowerCase()
                  .includes(query.toLowerCase()),
              )
              .map((tool, index) => (
                <details
                  key={`${inventoryName(tool) || "tool"}:${index}`}
                  className="workspace-tool"
                >
                  <summary>
                    <Wrench size={14} />
                    <strong>
                      {inventoryName(tool) || `Tool ${index + 1}`}
                    </strong>
                    <span className="workspace-tool-description">
                      {typeof tool.description === "string"
                        ? tool.description
                        : ""}
                    </span>
                  </summary>
                  <pre className="workspace-code">
                    {JSON.stringify(
                      tool.inputSchema ?? tool.parameters,
                      null,
                      2,
                    )}
                  </pre>
                </details>
              ))}
          </div>
          <h3 className="workspace-section-title">Observed native tools</h3>
          <p className="workspace-muted">
            {typeof state.data.nativeInventory === "string"
              ? state.data.nativeInventory
              : JSON.stringify(state.data.nativeInventory)}
          </p>
          <div className="workspace-actions">
            {observedNative
              .filter((name) =>
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
function Inventory({ value, query }: { value: unknown; query: string }) {
  if (!value || (Array.isArray(value) && !value.length))
    return <p className="workspace-muted">No entries returned by Codex.</p>;
  const entries: unknown[] = Array.isArray(value)
    ? value
    : isRecord(value)
      ? Array.isArray(value.data)
        ? value.data
        : Array.isArray(value.servers)
          ? value.servers
          : [value]
      : [value];
  return (
    <div className="workspace-tools">
      {entries
        .filter((entry: unknown) =>
          JSON.stringify(entry)?.toLowerCase().includes(query.toLowerCase()),
        )
        .map((entry: unknown, index: number) => (
          <details className="workspace-tool" key={index}>
            <summary>
              <SlidersHorizontal size={14} />
              <strong>{inventoryName(entry) || `Entry ${index + 1}`}</strong>
            </summary>
            <pre className="workspace-code">
              {JSON.stringify(entry, null, 2) ?? String(entry)}
            </pre>
          </details>
        ))}
    </div>
  );
}

function isJsonObject(
  value: JsonValue | null | undefined,
): value is Record<string, JsonValue> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function inventoryName(value: unknown): string | undefined {
  if (!isRecord(value)) return;
  for (const key of ["name", "cwd", "serverName"] as const) {
    const candidate = value[key];
    if (typeof candidate === "string" && candidate) return candidate;
  }
}

function Profiles(c: Context) {
  const state = useResource("/api/profiles", c.revision),
    [draft, setDraft] = useFormDraft(
      `studio-profile-draft:${c.data.stateDir}`,
      c.notify,
    ),
    [busy, setBusy] = useState(false),
    [remove, setRemove] = useState<
      GetResult<"/api/profiles">["profiles"][number] | null
    >(null),
    [launch, setLaunch] = useState<{
      id: string;
      profile_id: string;
      name: string;
      prompt: string;
    } | null>(null);
  const lead = c.data.threads.find(
    (a) => a.id === (c.selected?.rootId || c.selected?.id) && a.isLead,
  );
  // The profile model list comes from the selected lead's subagent catalog.
  const catalog = useWorkerModels(lead?.accountKey || "default", !!draft, true);
  const profileModels: ModelOption[] = catalog.models.map((row) => ({
    value: row.model,
    label:
      typeof row.displayName === "string" && row.displayName
        ? row.displayName
        : row.model,
    description:
      typeof row.description === "string" && row.description
        ? row.description
        : undefined,
    isDefault: !!row.isDefault,
  }));
  const draftModel = typeof draft?.model === "string" ? draft.model : "";
  if (draftModel && !profileModels.some((row) => row.value === draftModel))
    profileModels.unshift({ value: draftModel, label: draftModel });
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
      {(state.data?.profiles || []).map((profile) => (
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
        <Empty>No saved worker profiles.</Empty>
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
                await c.run("/api/profiles", {
                  action: "save",
                  id: typeof draft.id === "string" ? draft.id : undefined,
                  isNew: draft.isNew === true,
                  name: typeof draft.name === "string" ? draft.name : "",
                  role: draft.role === "reviewer" ? "reviewer" : "implementer",
                  model: draftModel,
                  effort:
                    draft.effort === "low" ||
                    draft.effort === "medium" ||
                    draft.effort === "high" ||
                    draft.effort === "xhigh" ||
                    draft.effort === "max" ||
                    draft.effort === "ultra"
                      ? draft.effort
                      : null,
                  instructions:
                    typeof draft.instructions === "string"
                      ? draft.instructions
                      : "",
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
              value={typeof draft.name === "string" ? draft.name : ""}
              onChange={(e) => setDraft({ ...draft, name: e.target.value })}
            />
            <NativeSelect
              label="Role"
              value={
                typeof draft.role === "string" ? draft.role : "implementer"
              }
              onChange={(e) => setDraft({ ...draft, role: e.target.value })}
              data={["implementer", "reviewer"]}
            />
            {catalog.models.length ? (
              <ModelPicker
                label="Model"
                value={draftModel}
                options={profileModels}
                onChange={(model) => setDraft({ ...draft, model })}
              />
            ) : (
              <TextInput
                label="Model"
                required
                value={draftModel}
                onChange={(e) => setDraft({ ...draft, model: e.target.value })}
              />
            )}
            <NativeSelect
              label="Reasoning effort"
              value={typeof draft.effort === "string" ? draft.effort : "high"}
              onChange={(e) => setDraft({ ...draft, effort: e.target.value })}
              data={["low", "medium", "high", "xhigh", "max", "ultra"]}
            />
            <Textarea
              label="Instructions"
              minRows={7}
              autosize
              value={
                typeof draft.instructions === "string" ? draft.instructions : ""
              }
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
                role: remove?.role ?? "reviewer",
                instructions: remove?.instructions ?? "",
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
                  id: launch.id,
                  name: launch.name,
                  profile_id: launch.profile_id,
                  prompt: launch.prompt,
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
  const state = useResource("/api/rules", c.revision, {
      query: c.selected ? { agent: c.selected.id } : {},
    }),
    [draftValue, setDraftValue] = useFormDraft(
      `studio-rule-draft:${JSON.stringify([c.data.stateDir, c.selected?.id])}`,
      c.notify,
    ),
    [busy, setBusy] = useState(false),
    [remove, setRemove] = useState<RuleRecord | null>(null);
  const draft = parseRuleDraft(draftValue);
  const setDraft = (value: RuleDraft | null) =>
    setDraftValue(value ? { ...value } : null);
  const act = async (rule: RuleRecord, action: RuleAction) => {
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
              stallTimeoutSeconds: 1800,
              at: "",
              path: "",
              event: "worker_completed",
              command: "",
              livenessCommand: "",
              text: "",
            })
          }
        >
          New rule
        </Button>
      </div>
      {(state.data?.rules || [])
        .filter((rule) => rule.agent === c.selected?.id)
        .map((rule) => (
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
                onClick={() => setDraft(ruleDraftFromRecord(rule))}
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
        (rule) => rule.agent === c.selected?.id,
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
              const submittedDraft = draftValue;
              try {
                await c.run(
                  "/api/rules",
                  ruleBodyFromDraft(draft, c.selected!.id),
                );
                setDraftValue((current) =>
                  current === submittedDraft ? null : current,
                );
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
                if (!isRuleKind(kind)) return;
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
                onChange={(e) => {
                  const event = e.target.value;
                  if (isRuleEvent(event)) setDraft({ ...draft, event });
                }}
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

type RuleRecord = GetResult<"/api/rules">["rules"][number];
type RuleAction = PostBody<"/api/rules">["action"];
type RuleDraft = {
  id: string;
  isNew?: boolean;
  name: string;
  kind: NonNullable<RuleRecord["kind"]>;
  intervalSeconds: number | string;
  minimumWorkers: number | string;
  durationMinutes: number | string;
  stallTimeoutSeconds: number | string;
  at: string;
  path: string;
  event: NonNullable<PostBody<"/api/rules">["event"]>;
  command: string;
  livenessCommand: string;
  text: string;
};

const ruleKinds = ["interval", "once", "file", "event", "low_workers"] as const;
const ruleEvents = [
  "worker_completed",
  "monitor_exit",
  "complaint",
  "work_review",
] as const;

function isRuleKind(value: string): value is RuleDraft["kind"] {
  return ruleKinds.some((kind) => kind === value);
}

function isRuleEvent(value: string): value is RuleDraft["event"] {
  return ruleEvents.some((event) => event === value);
}

export function parseRuleDraft(value: Json | null): RuleDraft | null {
  if (
    !value ||
    typeof value.id !== "string" ||
    typeof value.name !== "string" ||
    typeof value.kind !== "string" ||
    !isRuleKind(value.kind)
  )
    return null;
  const asNumberOrString = (candidate: JsonValue, fallback: number) =>
    typeof candidate === "number" || typeof candidate === "string"
      ? candidate
      : fallback;
  return {
    id: value.id,
    isNew: value.isNew === true,
    name: value.name,
    kind: value.kind,
    intervalSeconds: asNumberOrString(value.intervalSeconds, 60),
    minimumWorkers: asNumberOrString(value.minimumWorkers, 8),
    durationMinutes: asNumberOrString(value.durationMinutes, 30),
    stallTimeoutSeconds: asNumberOrString(value.stallTimeoutSeconds, 1800),
    at: typeof value.at === "string" ? value.at : "",
    path: typeof value.path === "string" ? value.path : "",
    event:
      typeof value.event === "string" && isRuleEvent(value.event)
        ? value.event
        : "worker_completed",
    command: typeof value.command === "string" ? value.command : "",
    livenessCommand:
      typeof value.livenessCommand === "string" ? value.livenessCommand : "",
    text: typeof value.text === "string" ? value.text : "",
  };
}

export function ruleDraftFromRecord(rule: RuleRecord): RuleDraft {
  return {
    id: rule.id,
    name: rule.name || "",
    kind: rule.kind || "interval",
    intervalSeconds: rule.intervalSeconds ?? 60,
    minimumWorkers: rule.minimumWorkers ?? 8,
    durationMinutes: rule.durationMinutes ?? 30,
    stallTimeoutSeconds: rule.stallTimeoutSeconds ?? 1800,
    at: rule.at
      ? new Date(typeof rule.at === "number" ? rule.at * 1000 : rule.at)
          .toLocaleString("sv-SE")
          .slice(0, 16)
          .replace(" ", "T")
      : "",
    path: rule.path || "",
    event:
      typeof rule.event === "string" && isRuleEvent(rule.event)
        ? rule.event
        : "worker_completed",
    command: rule.command || "",
    livenessCommand: rule.livenessCommand || "",
    text: rule.text || "",
  };
}

export function ruleBodyFromDraft(
  draft: RuleDraft,
  agent: string,
): PostBody<"/api/rules"> {
  return {
    id: draft.id,
    agent,
    action: "save",
    name: draft.name,
    kind: draft.kind,
    intervalSeconds:
      draft.kind === "interval" ? Number(draft.intervalSeconds) : undefined,
    minimumWorkers:
      draft.kind === "low_workers" ? Number(draft.minimumWorkers) : undefined,
    durationMinutes:
      draft.kind === "low_workers" ? Number(draft.durationMinutes) : undefined,
    stallTimeoutSeconds: Number(draft.stallTimeoutSeconds),
    at: draft.kind === "once" ? new Date(draft.at).getTime() / 1000 : undefined,
    path: draft.kind === "file" ? draft.path : undefined,
    event: draft.kind === "event" ? draft.event : undefined,
    command: draft.kind === "low_workers" ? "" : draft.command,
    livenessCommand: draft.kind === "file" ? draft.livenessCommand : undefined,
    text: draft.text,
  };
}
