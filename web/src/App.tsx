import {
  ActionIcon,
  Button,
  Drawer,
  Modal,
  NativeSelect,
  TextInput,
  UnstyledButton,
} from "@mantine/core";
import { useMediaQuery } from "@mantine/hooks";
import {
  Activity,
  BookOpen,
  CheckCheck,
  Clock3,
  FileDiff,
  Inbox,
  Minimize2,
  ShieldCheck,
  Square,
  ListTodo,
  ArrowLeft,
  Folder,
  Maximize2,
  MessageSquare,
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
import WorkerModelPicker, {
  useWorkerModels,
} from "./components/WorkerModelPicker";
import {
  ExecutionControls,
  WorkerDefaults,
  shortModel,
} from "./components/ExecutionSettings";
import Accounts, { useAccounts } from "./components/Accounts";
import Conversation from "./components/Conversation";
import ProjectDirectoryPicker from "./components/ProjectDirectoryPicker";
import TerminalDock from "./components/TerminalDock";
import "./desktop";
import Workspace from "./components/Workspace";
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
    [workspaceOpen, setWorkspaceOpen] = useState(false),
    [workspaceSection, setWorkspaceSection] = useState("work"),
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
  const accounts = useAccounts(data?.stateDir);
  const accountKey =
    agent?.accountKey ||
    lead?.accountKey ||
    (agent ? "default" : accounts.data.defaultAccountKey);
  const workerModels = useWorkerModels(
    accountKey,
    !!agent && agent.source === "managed",
  );
  const changeWorkerModel = async (id: string, model: string) => {
    await api("/api/conversation", { id, model });
    await refresh();
  };
  const limitsRequest = useRef(0);
  const currentAccountKey = useRef(accountKey);
  currentAccountKey.current = accountKey;
  const visibleLimits = limits?.accountKey === accountKey ? limits : null;
  const attentionCount =
    (data?.runtime.requests.length || 0) +
    (data?.runtime.userTasks?.filter((task) => task.status === "open").length ||
      0) +
    (data?.runtime.complaints.filter((c) => c.needsResponse).length || 0) +
    (data?.runtime.work?.filter((w: Json) => w.status === "review").length ||
      0) +
    agents.filter((a) => ["failed", "interrupted"].includes(a.status)).length;
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
  useEffect(() => {
    const onError = (event: Event) =>
      notify((event as CustomEvent<string>).detail);
    window.addEventListener("desktop-error", onError);
    return () => window.removeEventListener("desktop-error", onError);
  }, [notify]);
  const reloadLimits = useCallback(() => {
    const request = ++limitsRequest.current;
    const query =
      accountKey === "default"
        ? ""
        : `?account_key=${encodeURIComponent(accountKey)}`;
    void api("/api/limits" + query)
      .then((result) => {
        if (result.accountKey && result.accountKey !== accountKey)
          throw new Error("Codex returned limits for another account.");
        if (
          request === limitsRequest.current &&
          currentAccountKey.current === accountKey
        )
          setLimits({ ...result, accountKey });
      })
      .catch((e) => {
        if (
          request === limitsRequest.current &&
          currentAccountKey.current === accountKey
        )
          setLimits({ data: null, error: errorText(e), accountKey });
      });
  }, [accountKey]);
  useEffect(() => {
    if (data?.stateDir) reloadLimits();
  }, [data?.stateDir, reloadLimits]);
  useEffect(() => {
    const result =
      data?.runtime.rateLimitsByAccount?.[accountKey] ||
      (accountKey === "default" ? data?.runtime.rateLimits : null);
    if (result?.at && (result.accountKey || "default") === accountKey)
      setLimits((old) =>
        !old || old.accountKey !== accountKey || result.at >= (old.at || 0)
          ? { ...result, accountKey }
          : old,
      );
  }, [data?.runtime.rateLimits, data?.runtime.rateLimitsByAccount, accountKey]);
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
      account_key: accounts.data.defaultAccountKey,
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
  const send = async (options?: {
    assets?: string[];
    delivery?: "queue" | "steer";
  }) => {
    const draftKey = opened || "new",
      text = (drafts[draftKey] || "").trim();
    if ((!text && !options?.assets?.length) || sendingLock.current) return;
    sendingLock.current = true;
    setSending(true);
    try {
      const id = opened || (await newChat());
      if (!id) return;
      if (
        agent?.source === "managed" &&
        /^\/(compact|review|stop|stop-team)(\s|$)/.test(text)
      ) {
        const [command] = text.split(/\s+/);
        if (command === "/stop" || command === "/stop-team")
          await api("/api/stop", {
            id: command === "/stop-team" ? agent.rootId : id,
            descendants: command === "/stop-team",
          });
        else await api("/api/action", { id, action: command.slice(1) });
      } else {
        if (
          sends.current[id]?.text !== text ||
          JSON.stringify(sends.current[id]?.assets || []) !==
            JSON.stringify(options?.assets || []) ||
          sends.current[id]?.delivery !== (options?.delivery || "queue")
        )
          sends.current[id] = {
            id: crypto.randomUUID(),
            room: id,
            text,
            assets: options?.assets || [],
            delivery: options?.delivery || "queue",
          };
        const result = await api("/api/messages", sends.current[id]);
        if (result.status === "cancelled") {
          delete sends.current[id];
          throw new Error(
            "This message was cancelled. Send it again to resume.",
          );
        }
        if (result.status === "uncertain")
          throw new Error(
            result.error ||
              "Delivery is uncertain. Inspect the conversation before sending again.",
          );
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
      throw e;
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
  const folders = (target: Agent) => {
    setModal({
      title: "Project directory",
      body: (
        <ProjectDirectoryPicker
          initialPath={target.cwd}
          onSelect={async (cwd) => {
            await api("/api/conversation", { id: target.id, cwd });
            setModal(null);
            await refresh();
          }}
        />
      ),
    });
  };
  const project = () => {
    if (window.codexDesktop && agent?.isLead && !agent.threadId) {
      const id = agent.id;
      void window.codexDesktop
        .pickDirectory()
        .then(async (cwd) => {
          if (!cwd) return;
          await api("/api/conversation", { id, cwd });
          await refresh();
        })
        .catch((error) => notify(errorText(error)));
    } else if (window.codexDesktop && agent?.cwd) {
      void window.codexDesktop
        .revealPath(agent.cwd)
        .catch((error) => notify(errorText(error)));
    } else if (agent?.isLead && !agent.threadId) folders(agent);
    else if (agent?.cwd)
      setModal({ title: "Project directory", body: <p>{agent.cwd}</p> });
  };
  const importChat = async (cursor?: string) => {
    try {
      const d = await api(
        "/api/import?account_key=" +
          encodeURIComponent(accountKey) +
          (cursor ? "&cursor=" + encodeURIComponent(cursor) : ""),
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
                        account_key: accountKey,
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
      <div className="startup">{error || "Connecting to Codex Studio…"}</div>
    );
  const title =
    view === "canvas"
      ? "All agents"
      : view === "complaints"
        ? "Complaint book"
        : agent?.name || room?.name || legacy?.name || "New conversation";
  const worker = (a: Agent) => (
    <div
      className={`worker-entry ${opened === a.id ? "selected" : ""}`}
      key={a.id}
    >
      <UnstyledButton
        className="worker"
        data-worker={a.id}
        onClick={() => open(a.id)}
      >
        <span className={`dot ${a.status}`} />
        <span className="worker-text">
          <strong>{a.name}</strong>
          <small>{statusLabel(a.status)}</small>
        </span>
      </UnstyledButton>
      <span
        className="worker-model-summary"
        title={[
          a.model,
          a.effort || "default reasoning",
          a.fastMode ? "Fast" : "Standard",
        ].join(" · ")}
      >
        {shortModel(a.model)}
        {a.fastMode ? " · Fast" : ""}
      </span>
    </div>
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
        refresh={refresh}
        notify={notify}
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
                      : room
                        ? "Broadcast"
                        : ""}
            </span>
          </div>
          <Accounts
            state={accounts}
            agent={agent || lead}
            accountKey={accountKey}
            onError={notify}
            lead={lead || (agent?.isLead ? agent : undefined)}
            teamBusy={
              team.some(
                (member) =>
                  !!member.inFlight ||
                  busy.has(member.status) ||
                  member.status === "queued",
              ) ||
              !!agent?.inFlight ||
              (!!agent && busy.has(agent.status))
            }
            changeRuleOverride={async (enabled) => {
              const root = lead || (agent?.isLead ? agent : undefined);
              if (!root) return;
              await api("/api/conversation", {
                id: root.id,
                dangerously_skip_rules: enabled,
              });
              await refresh();
            }}
            changeAccount={async (key) => {
              if (agent?.isLead) {
                await api("/api/agents/account", {
                  id: agent.id,
                  account_key: key,
                });
                await refresh();
              } else {
                accounts.setData(
                  await api("/api/accounts/default", { account_key: key }),
                );
              }
            }}
          />
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
          {view === "chat" && agent?.source === "managed" && !agent.isLead && (
            <WorkerModelPicker
              key={agent.id}
              id="model"
              agent={agent}
              catalog={workerModels}
              change={changeWorkerModel}
              onError={notify}
            />
          )}
          {view === "chat" && agent?.source === "managed" && (
            <ExecutionControls
              key={"execution:" + agent.id}
              agent={agent}
              catalog={workerModels}
              refresh={refresh}
              onError={notify}
            />
          )}
          {view === "chat" && lead?.isLead && (
            <WorkerDefaults
              key={"defaults:" + lead.id}
              lead={lead}
              catalog={workerModels}
              refresh={refresh}
            />
          )}
          {view === "chat" && !!workers.length && (
            <Button
              leftSection={<Users size={16} />}
              id="team-toggle"
              onClick={() => setTeamOpen(!teamOpen)}
            >
              Team
            </Button>
          )}
          {view === "chat" && agent?.source === "managed" && !agent.empty && (
            <div
              className="conversation-quick-actions"
              aria-label="Agent actions"
            >
              {(
                [
                  ["compact", "Compact", Minimize2],
                  ["review", "Review", ShieldCheck],
                  ["stop-team", "Stop team", Square],
                ] as const
              ).map(([action, label, Icon]) => (
                <Button
                  key={String(action)}
                  data-action={String(action)}
                  disabled={
                    action !== "stop-team" &&
                    (busy.has(agent.status) ||
                      !!agent.inFlight ||
                      !agent.threadId)
                  }
                  size="compact-xs"
                  variant="subtle"
                  color={action === "stop-team" ? "red" : undefined}
                  leftSection={<Icon size={14} />}
                  onClick={() => {
                    void run(() =>
                      api(
                        action === "stop-team" ? "/api/stop" : "/api/action",
                        action === "stop-team"
                          ? { id: agent.rootId, descendants: true }
                          : { id: agent.id, action },
                      ),
                    );
                  }}
                >
                  {String(label)}
                </Button>
              ))}
            </div>
          )}
        </header>
        <nav className="workspace-shortcuts" aria-label="Workspace shortcuts">
          <Button
            id="workspace-toggle"
            leftSection={<ListTodo size={16} />}
            onClick={() => {
              setWorkspaceSection("work");
              setWorkspaceOpen(true);
            }}
            aria-label={`Work workspace${attentionCount ? `, ${attentionCount} need attention` : ""}`}
          >
            <span className="workspace-button-label">Work</span>
            {attentionCount > 0 && (
              <span className="attention-count">{attentionCount}</span>
            )}
          </Button>
          {(
            [
              ["user-tasks", "Your tasks", CheckCheck],
              ["inbox", "Inbox", Inbox],
              ["changes", "Changes", FileDiff],
              ["search", "Search", Search],
              ["plan", "Plan", BookOpen],
              ["rules", "Rules", Clock3],
            ] as const
          ).map(([section, label, Icon]) => (
            <Button
              key={section}
              data-workspace-section={section}
              leftSection={<Icon size={14} />}
              onClick={() => {
                setWorkspaceSection(section);
                setWorkspaceOpen(true);
              }}
            >
              {label}
              {section === "user-tasks" &&
                !!data.runtime.userTasks?.some((t) => t.status === "open") && (
                  <span className="attention-count">
                    {
                      data.runtime.userTasks.filter((t) => t.status === "open")
                        .length
                    }
                  </span>
                )}
            </Button>
          ))}
          <Button
            id="tasks-toggle"
            aria-label={`Background tasks${taskCount ? `, ${taskCount} active` : ""}`}
            leftSection={<Activity size={16} />}
            onClick={() => {
              setTasksOpen(true);
            }}
          >
            Background{" "}
            {taskCount > 0 && <span className="tasks-count">{taskCount}</span>}
          </Button>
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
        </nav>
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
            limits={visibleLimits}
            reloadLimits={reloadLimits}
            onPhase={onPhase}
            onSelect={open}
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
      <TerminalDock
        data={data}
        agent={agent || lead}
        onOpenBackground={() => setTasksOpen(true)}
        onSelectAgent={open}
        notify={notify}
      />
      <Workspace
        initialSection={workspaceSection}
        opened={workspaceOpen}
        onClose={() => setWorkspaceOpen(false)}
        agent={agent || lead}
        data={data}
        onSelect={open}
        refresh={refresh}
        notify={notify}
      />
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
