import {
  ActionIcon,
  Button,
  Drawer,
  Menu,
  Modal,
  NativeSelect,
  TextInput,
  UnstyledButton,
} from "@mantine/core";
import { useMediaQuery } from "@mantine/hooks";
import {
  Activity,
  ArrowLeft,
  Folder,
  Maximize2,
  MessageSquare,
  MoreHorizontal,
  PanelLeft,
  Search,
  Users,
  X,
} from "lucide-react";
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { api, errorText, save, saved } from "./api";
import { useSnapshot } from "./hooks";
import { busy, statusLabel, type Agent, type Json } from "./types";
import Sidebar from "./components/Sidebar";
import Conversation from "./components/Conversation";
import Canvas from "./components/Canvas";
import ComplaintBook from "./components/ComplaintBook";
import BackgroundTasks, {
  activeTask,
  backgroundTasks,
} from "./components/BackgroundTasks";
export default function App() {
  const [livePhase, setLivePhase] = useState<{
    id: string | null;
    label: string;
  } | null>(null);
  const onPhase = useCallback(
    (id: string | null, label: string) =>
      setLivePhase((old) =>
        old?.id === id && old.label === label ? old : { id, label },
      ),
    [],
  );
  const narrowTeam = useMediaQuery("(max-width: 1199px)");
  const { data, error, refresh } = useSnapshot(),
    [opened, setOpened] = useState<string | null>(null),
    [view, setView] = useState("chat"),
    [sidebar, setSidebar] = useState(false),
    [teamOpen, setTeamOpen] = useState(false),
    [tasksOpen, setTasksOpen] = useState(false),
    [workerQuery, setWorkerQuery] = useState(""),
    [creating, setCreating] = useState(false),
    [sending, setSending] = useState(false),
    [toast, setToast] = useState(""),
    [modal, setModal] = useState<{ title: string; body: ReactNode } | null>(
      null,
    ),
    [limits, setLimits] = useState<Json | null>(null);
  const [drafts, setDrafts] = useState<Record<string, string>>(() =>
    saved("codex-agent-drafts", {}),
  );
  const creation = useRef<Json | null>(null),
    sends = useRef<Record<string, Json>>({}),
    sendingLock = useRef(false),
    creationLock = useRef(false),
    toastTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const notify = useCallback((s: string) => {
    setToast(s);
    if (toastTimer.current) clearTimeout(toastTimer.current);
    toastTimer.current = setTimeout(() => setToast(""), 5000);
  }, []);
  const agents = data?.threads || [],
    leads = agents.filter((a) => a.source === "managed" && a.isLead),
    agent = agents.find((a) => a.id === opened),
    room = data?.runtime.rooms?.find((r) => r.id === opened),
    legacy = data?.chats.find((c) => c.id === opened),
    lead = agents.find((a) => a.id === (agent?.rootId || room?.rootId)),
    team = agents.filter((a) => a.rootId === lead?.id),
    workers = team.filter((a) => !a.isLead);
  const taskCount = backgroundTasks(data).filter(activeTask).length;
  const setDraft = (text: string, id = opened || "new") =>
    setDrafts((old) => {
      const next = { ...old, [id]: text };
      save("codex-agent-drafts", next);
      return next;
    });
  const open = (id: string) => {
    setOpened(id);
    setView("chat");
    setSidebar(false);
    setTeamOpen(false);
  };
  useEffect(() => {
    if (data && (!opened || (!agent && !room && !legacy)))
      setOpened(leads.at(-1)?.id || null);
  }, [data, opened, agent, room, legacy]);
  const reloadLimits = useCallback(() => {
    void api("/api/limits")
      .then(setLimits)
      .catch((e) => setLimits({ data: null, error: errorText(e) }));
  }, []);
  useEffect(() => {
    if (data?.stateDir) reloadLimits();
  }, [data?.stateDir, reloadLimits]);
  useEffect(() => {
    if (data?.runtime.rateLimits?.at) setLimits(data.runtime.rateLimits);
  }, [data?.runtime.rateLimits]);
  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "k") {
        e.preventDefault();
        setSidebar(true);
        requestAnimationFrame(() =>
          document.querySelector<HTMLInputElement>("#chat-search")?.focus(),
        );
      }
      if (e.key === "Escape") {
        setModal(null);
        setSidebar(false);
        setTeamOpen(false);
      }
    };
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }, []);
  const run = async (fn: () => Promise<unknown>) => {
    try {
      await fn();
      await refresh();
    } catch (e) {
      notify(errorText(e));
    }
  };
  const newChat = async () => {
    if (creationLock.current) return null;
    if (agent?.isLead && agent.empty && !creation.current) {
      setView("chat");
      return agent.id;
    }
    creationLock.current = true;
    setCreating(true);
    creation.current ||= {
      id: crypto.randomUUID(),
      previous: lead?.id || null,
      model: lead?.model || "gpt-6-astra",
    };
    if (!opened && drafts.new) setDraft(drafts.new, creation.current.id);
    try {
      const a = await api("/api/leads", creation.current);
      if (!opened && drafts.new) setDraft(drafts.new, a.id);
      setToast("");
      creation.current = null;
      await refresh();
      open(a.id);
      return a.id as string;
    } catch (e) {
      notify(errorText(e));
      return null;
    } finally {
      creationLock.current = false;
      setCreating(false);
    }
  };
  const send = async () => {
    const draftKey = opened || "new",
      text = (drafts[draftKey] || "").trim();
    if (!text || sendingLock.current) return;
    sendingLock.current = true;
    setSending(true);
    try {
      const id = opened || (await newChat());
      if (!id) return;
      if (
        agent?.source === "managed" &&
        /^\/(monitor|compact|review|stop|stop-team)(\s|$)/.test(text)
      ) {
        const [command, ...rest] = text.split(" ");
        if (command === "/monitor")
          await api("/api/monitor", {
            id: crypto.randomUUID(),
            agent: id,
            command: rest.join(" "),
          });
        else if (command.startsWith("/stop"))
          await api("/api/stop", {
            id: command === "/stop-team" ? agent.rootId : id,
            descendants: command === "/stop-team",
          });
        else await api("/api/action", { id, action: command.slice(1) });
      } else {
        if (sends.current[id]?.text !== text)
          sends.current[id] = { id: crypto.randomUUID(), room: id, text };
        const result = await api("/api/messages", sends.current[id]);
        delete sends.current[id];
        if (Object.values(result.deliveries || {}).some((v) => v !== "queued"))
          notify("Message saved. Some deliveries are not confirmed.");
      }
      setDrafts((old) => {
        const next = { ...old };
        if (next[draftKey]?.trim() === text) delete next[draftKey];
        if (next[id]?.trim() === text) delete next[id];
        save("codex-agent-drafts", next);
        return next;
      });
      await refresh();
    } catch (e) {
      notify(errorText(e));
    } finally {
      sendingLock.current = false;
      setSending(false);
    }
  };
  const rename = async (id: string, name: string) => {
    try {
      await api("/api/rename", { id, name });
      await refresh();
    } catch (e) {
      notify(errorText(e));
      throw e;
    }
  };
  const remove = (id: string, isRoom: boolean) =>
    setModal({
      title: "Delete chat?",
      body: (
        <>
          <p>
            {isRoom
              ? "Remove this agent chat from your list. A new agent message makes it appear again."
              : "Stop this agent and its workers, and remove their conversations. Files and stored history remain on disk."}
          </p>
          <Button
            color="red"
            variant="filled"
            data-delete-chat={id}
            onClick={() =>
              void run(async () => {
                const r = await api(
                  isRoom ? "/api/room/delete" : "/api/conversation/delete",
                  { id },
                );
                setModal(null);
                if (r.deleted.includes(opened)) setOpened(null);
                setDrafts((old) => {
                  const next = { ...old };
                  for (const key of r.deleted) delete next[key];
                  save("codex-agent-drafts", next);
                  return next;
                });
              })
            }
          >
            Delete chat
          </Button>
        </>
      ),
    });
  const folders = async (path?: string) => {
    try {
      const d = await api(
        "/api/directories" + (path ? "?path=" + encodeURIComponent(path) : ""),
      );
      setModal({
        title: "Project directory",
        body: (
          <>
            <p className="notice">{d.path}</p>
            <Button
              onClick={() =>
                void run(async () => {
                  await api("/api/conversation", { id: opened, cwd: d.path });
                  setModal(null);
                })
              }
            >
              Use this folder
            </Button>
            {d.parent && (
              <Button onClick={() => void folders(d.parent)}>
                Parent folder
              </Button>
            )}
            {d.directories.map((r: Json) => (
              <Button key={r.path} onClick={() => void folders(r.path)}>
                <Folder size={15} /> {r.name}
              </Button>
            ))}
          </>
        ),
      });
    } catch (e) {
      notify(errorText(e));
    }
  };
  const project = () => {
    if (agent?.isLead && !agent.threadId) void folders(agent.cwd);
    else if (agent?.cwd)
      setModal({ title: "Project directory", body: <p>{agent.cwd}</p> });
  };
  const importChat = async (cursor?: string) => {
    try {
      const d = await api(
        "/api/import" + (cursor ? "?cursor=" + encodeURIComponent(cursor) : ""),
      );
      setModal({
        title: "Import from Codex",
        body: (
          <>
            <p className="notice">Copies the last 20 turns into a new lead.</p>
            {d.data.map((t: Json) => {
              const id = crypto.randomUUID();
              return (
                <Button
                  key={t.id}
                  onClick={() =>
                    void run(async () => {
                      const a = await api("/api/import", {
                        id,
                        threadId: t.id,
                        name: "Imported chat",
                        model: lead?.model || "gpt-6-astra",
                      });
                      setModal(null);
                      await refresh();
                      open(a.id);
                    })
                  }
                >
                  {t.name || t.preview || "Untitled"}
                  <small>{t.cwd}</small>
                </Button>
              );
            })}
            {d.nextCursor && (
              <Button onClick={() => void importChat(d.nextCursor)}>
                More conversations
              </Button>
            )}
          </>
        ),
      });
    } catch (e) {
      notify(errorText(e));
    }
  };
  const other = () =>
    setModal({
      title: "Other sessions",
      body: (
        <>
          {agents
            .filter((a) => !a.isLead)
            .map((a) => (
              <Button
                key={a.id}
                onClick={() => {
                  setModal(null);
                  open(a.id);
                }}
              >
                {a.name}
                <small>{statusLabel(a.status)}</small>
              </Button>
            ))}
          {data?.chats.map((c) => (
            <Button
              key={c.id}
              onClick={() => {
                setModal(null);
                open(c.id);
              }}
            >
              {c.name}
              <small>Shared chat</small>
            </Button>
          ))}
        </>
      ),
    });
  if (!data)
    return (
      <div className="startup">{error || "Connecting to Codex agents…"}</div>
    );
  const title =
    view === "canvas"
      ? "All agents"
      : view === "complaints"
        ? "Complaint book"
        : agent?.name || room?.name || legacy?.name || "New conversation";
  const worker = (a: Agent) => (
    <UnstyledButton
      className={`worker ${opened === a.id ? "selected" : ""}`}
      data-worker={a.id}
      key={a.id}
      onClick={() => open(a.id)}
    >
      <span className={`dot ${a.status}`} />
      <span className="worker-text">
        <strong>{a.name}</strong>
        <small>{statusLabel(a.status)}</small>
      </span>
    </UnstyledButton>
  );
  const shown = workers.filter((a) =>
    `${a.name} ${a.status} ${a.role}`
      .toLowerCase()
      .includes(workerQuery.toLowerCase()),
  );
  const teamPanel = (
    <aside id="team" aria-label="Team">
      <div className="team-heading">
        <h2>Team</h2>
        <ActionIcon
          aria-label="Close team"
          id="team-close"
          onClick={() => setTeamOpen(false)}
        >
          <X size={16} />
        </ActionIcon>
        <span>
          {workers.filter((a) => busy.has(a.status)).length} active /{" "}
          {workers.length}
        </span>
      </div>
      <Button id="lead-row" onClick={() => lead && open(lead.id)}>
        {lead?.name || "Lead"}
      </Button>
      {workers.length >= 8 && (
        <TextInput
          leftSection={<Search size={14} />}
          id="worker-search"
          type="search"
          aria-label="Find a worker"
          placeholder="Find a worker"
          value={workerQuery}
          onChange={(e) => setWorkerQuery(e.target.value)}
        />
      )}
      <div id="workers">
        {shown.filter((a) => a.status !== "completed").map(worker)}
        {shown.some((a) => a.status === "completed") && (
          <details className="worker-group">
            <summary>
              Completed · {shown.filter((a) => a.status === "completed").length}
            </summary>
            {shown.filter((a) => a.status === "completed").map(worker)}
          </details>
        )}
      </div>
    </aside>
  );
  return (
    <>
      <Sidebar
        data={data}
        opened={opened}
        view={view}
        open={open}
        newChat={() => void newChat()}
        creating={creating}
        complaints={() => {
          setView("complaints");
          setSidebar(false);
          setTeamOpen(false);
        }}
        rename={rename}
        remove={remove}
        importChat={() => void importChat()}
        other={other}
        mobile={sidebar}
        close={() => setSidebar(false)}
      />
      <main className="workspace">
        <header className="workspace-header">
          <ActionIcon
            id="sidebar-toggle"
            aria-label="Toggle conversations"
            onClick={() => setSidebar(!sidebar)}
          >
            <PanelLeft size={18} />
          </ActionIcon>
          <div className="conversation-heading">
            {view === "chat" && lead && agent?.id !== lead.id && (
              <Button
                size="compact-xs"
                leftSection={<ArrowLeft size={13} />}
                id="back-lead"
                onClick={() => open(lead.id)}
              >
                Lead
              </Button>
            )}
            <h1 id="conversation-title">{title}</h1>
            <span id="conversation-status">
              {view === "canvas"
                ? `${leads.length} leads · ${agents.length} agents`
                : view === "complaints"
                  ? "Lead review required"
                  : agent
                    ? livePhase?.id === agent.id
                      ? livePhase.label
                      : statusLabel(agent.status, agent.activity?.phase)
                    : room?.kind === "private"
                      ? "Private between agents · Visible to you"
                      : "Broadcast"}
            </span>
          </div>
          {view === "chat" && agent?.isLead && (
            <NativeSelect
              id="model"
              aria-label="Lead model"
              value={agent.model}
              disabled={busy.has(agent.status) || agent.inFlight}
              onChange={(e) =>
                void run(() =>
                  api("/api/conversation", {
                    id: agent.id,
                    model: e.target.value,
                  }),
                )
              }
            >
              <option value="gpt-6-astra">Astra</option>
              <option value="gpt-5.6-sol">Sol</option>
            </NativeSelect>
          )}
          <Button
            id="view-toggle"
            leftSection={
              view === "canvas" ? (
                <MessageSquare size={15} />
              ) : (
                <Maximize2 size={15} />
              )
            }
            aria-pressed={view === "canvas"}
            onClick={() => setView(view === "canvas" ? "chat" : "canvas")}
          >
            {view === "canvas" ? "Chat" : "Canvas"}
          </Button>
          <Button
            id="tasks-toggle"
            aria-label={`Background tasks${taskCount ? `, ${taskCount} active` : ""}`}
            leftSection={<Activity size={16} />}
            onClick={() => setTasksOpen(true)}
          >
            Tasks{" "}
            {taskCount > 0 && <span className="tasks-count">{taskCount}</span>}
          </Button>
          {view === "chat" && !!workers.length && (
            <Button
              leftSection={<Users size={16} />}
              id="team-toggle"
              onClick={() => setTeamOpen(!teamOpen)}
            >
              Team
            </Button>
          )}
          {view === "chat" && agent?.source === "managed" && (
            <Menu position="bottom-end" withinPortal width={200} shadow="lg">
              <Menu.Target>
                <ActionIcon
                  id="conversation-menu"
                  aria-label="Conversation actions"
                >
                  <MoreHorizontal size={18} />
                </ActionIcon>
              </Menu.Target>
              <Menu.Dropdown>
                {["monitor", "compact", "review", "stop-team"].map((action) => (
                  <Menu.Item
                    key={action}
                    data-action={action}
                    color={action === "stop-team" ? "red" : undefined}
                    onClick={() => {
                      if (action === "monitor") setDraft("/monitor ");
                      else
                        void run(() =>
                          api(
                            action === "stop-team"
                              ? "/api/stop"
                              : "/api/action",
                            action === "stop-team"
                              ? { id: agent.rootId, descendants: true }
                              : { id: agent.id, action },
                          ),
                        );
                    }}
                  >
                    {
                      {
                        monitor: "Monitor a command",
                        compact: "Compact context",
                        review: "Review changes",
                        "stop-team": "Stop team",
                      }[action]
                    }
                  </Menu.Item>
                ))}
              </Menu.Dropdown>
            </Menu>
          )}
        </header>
        {error && (
          <div id="error" role="alert">
            {error}
          </div>
        )}
        {view === "chat" && (
          <Conversation
            id={opened}
            agent={agent}
            room={room}
            legacy={legacy}
            data={data}
            draft={drafts[opened || "new"] || ""}
            setDraft={setDraft}
            send={send}
            sending={sending}
            refresh={refresh}
            notify={notify}
            project={project}
            limits={limits}
            reloadLimits={reloadLimits}
            onPhase={onPhase}
          />
        )}
        {view === "canvas" && (
          <Canvas
            agents={agents}
            stateDir={data.stateDir}
            opened={opened}
            open={open}
          />
        )}
        {view === "complaints" && (
          <ComplaintBook
            data={data}
            leadId={lead?.id}
            refresh={refresh}
            notify={notify}
          />
        )}
      </main>
      {view === "chat" &&
        !!workers.length &&
        (narrowTeam ? (
          <Drawer
            opened={teamOpen}
            onClose={() => setTeamOpen(false)}
            position="right"
            size={300}
            padding={0}
            withCloseButton={false}
            title="Team"
            classNames={{ header: "sr-only" }}
          >
            {teamPanel}
          </Drawer>
        ) : (
          teamPanel
        ))}
      <BackgroundTasks
        opened={tasksOpen}
        close={() => setTasksOpen(false)}
        data={data}
        leadId={lead?.id}
        openAgent={open}
        refresh={refresh}
        notify={notify}
      />
      <Modal
        opened={!!modal}
        onClose={() => setModal(null)}
        title={modal?.title}
      >
        <div className="picker">{modal?.body}</div>
      </Modal>
      {toast && (
        <div id="toast" role="status">
          {toast}
        </div>
      )}
    </>
  );
}
