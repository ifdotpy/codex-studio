import { useChatPrefetch } from "./hooks/chatPrefetch";
import { accountLimits } from "./accountUsage";
import { useMobileViewport } from "./hooks/mobileViewport";
import { chatSnapshot, roomLeadIds, messageAttentionCount } from "./chatScope";
import { nativeThreadError } from "./nativeErrors";
import {
  ActionIcon,
  Button,
  Drawer,
  Modal,
  Menu,
  NativeSelect,
  useMantineColorScheme,
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
  Minimize2,
  MoreHorizontal,
  ShieldCheck,
  Settings,
  Square,
  ListTodo,
  ArrowLeft,
  Folder,
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
import { api, ApiError, errorText, save, saved } from "./api";
import { useSnapshot } from "./hooks";
import {
  acknowledgeOutbox,
  editOutboxDisplay,
  durableSend,
  useOutbox,
  type OutgoingMessage,
} from "./sync/send";
import { useSyncedDrafts } from "./sync/drafts";
import { busy, statusLabel, type Agent, type Json } from "./types";
import Sidebar from "./components/Sidebar";
import ProjectAccount from "./components/ProjectAccount";
import { useWorkerModels } from "./components/WorkerModelPicker";
import { ExecutionSettings } from "./components/ExecutionSettings";
import Accounts, { useAccounts } from "./components/Accounts";
import Conversation from "./components/Conversation";
import ProjectDirectoryPicker from "./components/ProjectDirectoryPicker";
import TerminalDock from "./components/TerminalDock";
import "./desktop";
import "./components/team-navigation.css";
import WorkerCard, {
  awaitingAnswerIds,
  TeamSummary,
  workerState,
} from "./components/WorkerOverview";
import Workspace from "./components/Workspace";
import BackgroundTasks, {
  activeTask,
  backgroundTasks,
} from "./components/BackgroundTasks";
export default function App() {
  const outbox = useOutbox();
  const [outgoing, setOutgoing] = useState<Record<string, OutgoingMessage>>({});
  const observedSends = useRef(new Set<string>());
  const observeSends = useCallback((ids: string[]) => {
    ids.forEach((id) => observedSends.current.add(id));
    setOutgoing((old) =>
      Object.fromEntries(
        Object.entries(old).filter(([id]) => !observedSends.current.has(id)),
      ),
    );
    void acknowledgeOutbox(ids).catch(() => {});
  }, []);
  const editSend = useCallback((id: string, text: string) => {
    setOutgoing((old) =>
      old[id] ? { ...old, [id]: { ...old[id], displayText: text } } : old,
    );
    void editOutboxDisplay(id, text).catch(() => {});
  }, []);
  const visibleSends = new Map(
    outbox.entries
      .filter(
        (entry) =>
          entry.displayPending === true ||
          (entry.displayPending !== false && entry.status !== "accepted"),
      )
      .map((entry) => [entry.id, entry as OutgoingMessage]),
  );
  for (const entry of Object.values(outgoing))
    if (entry.status === "sending" || !visibleSends.has(entry.id))
      visibleSends.set(entry.id, entry);
  const outgoingMessages = [...visibleSends.values()].filter(
    (entry) => !observedSends.current.has(entry.id),
  );
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
  const mobileClient = useMediaQuery("(max-width: 760px)");
  const [settingsOpen, setSettingsOpen] = useState(false);
  const chatActionsButton = useRef<HTMLButtonElement>(null);
  const [accountModalOpen, setAccountModalOpen] = useState(false);
  const [mainSettingsOpen, setMainSettingsOpen] = useState(false);
  const [subagentSettingsOpen, setSubagentSettingsOpen] = useState(false);
  const { colorScheme, setColorScheme } = useMantineColorScheme();
  const [sidebarCollapsed, setSidebarCollapsed] = useState(() =>
    saved("codex-sidebar-collapsed", false),
  );
  const [wideTeamOpen, setWideTeamOpen] = useState(() =>
    saved("codex-team-open", false),
  );
  const [jumpTarget, setJumpTarget] = useState<{
    chatId: string;
    messageId: string;
    requestId: string;
  }>();
  const [workspaceFocus, setWorkspaceFocus] = useState<{
    id: string;
    requestId: string;
  }>();
  const [limitsLoading, setLimitsLoading] = useState<Record<string, boolean>>(
    {},
  );
  const selectionScope = useRef("");
  const createdSelection = useRef<string | null>(null);
  useMobileViewport(mobileClient);
  const narrowTeam = useMediaQuery("(max-width: 1199px)");
  const { data, error, refresh, workspaceId } = useSnapshot(),
    [opened, setOpened] = useState<string | null>(() =>
      window.matchMedia("(max-width: 760px)").matches
        ? saved("codex-mobile-opened", null)
        : null,
    ),
    [roomContext, setRoomContext] = useState<string | null>(null),
    [sidebar, setSidebar] = useState(false),
    [teamOpen, setTeamOpen] = useState(false),
    [tasksOpen, setTasksOpen] = useState(false),
    [workspaceOpen, setWorkspaceOpen] = useState(false),
    [workspaceSection, setWorkspaceSection] = useState("work"),
    [workerQuery, setWorkerQuery] = useState(""),
    [workerFilter, setWorkerFilter] = useState("all"),
    [completedOpen, setCompletedOpen] = useState(false),
    [creating, setCreating] = useState(false),
    [sending, setSending] = useState(false),
    [toast, setToast] = useState(""),
    [modal, setModal] = useState<{ title: string; body: ReactNode } | null>(
      null,
    ),
    [limitsByAccount, setLimitsByAccount] = useState<Record<string, Json>>({});
  const receipts = new Map(
    (data?.runtime.events || []).map((event) => [event.id, event]),
  );
  useChatPrefetch(data, opened, workspaceId);
  const cancelledSends = outgoingMessages
    .filter((entry) => receipts.get(entry.id)?.status === "cancelled")
    .map((entry) => entry.id)
    .join(",");
  useEffect(() => {
    if (cancelledSends) observeSends(cancelledSends.split(","));
  }, [cancelledSends, observeSends]);
  const visibleOutgoing = outgoingMessages
    .filter((entry) => receipts.get(entry.id)?.status !== "cancelled")
    .map((entry) => {
      const receipt = receipts.get(entry.id);
      return receipt && ["failed", "uncertain"].includes(receipt.status)
        ? { ...entry, status: receipt.status, error: receipt.error }
        : entry;
    });
  const {
    drafts,
    setDrafts,
    conflicts: draftConflicts,
    dismissDraft,
    error: draftError,
  } = useSyncedDrafts();
  const [pendingCreation, setPendingCreation] = useState<Json | null>(null);
  const creationKey = `codex-pending-creation:${data?.stateDir || ""}`;
  const creation = useRef<Json | null>(null),
    sends = useRef<Record<string, Json>>({}),
    sendingLock = useRef<symbol | null>(null),
    latestSend = useRef<Record<string, symbol>>({}),
    creationLock = useRef(false),
    toastTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const pendingSendKey = `studio-pending-sends:${data?.stateDir || ""}`;
  useEffect(() => {
    if (data?.stateDir) sends.current = saved(pendingSendKey, {});
  }, [pendingSendKey, data?.stateDir]);
  const persistSends = () => save(pendingSendKey, sends.current);
  useEffect(() => {
    if (!data?.stateDir) return;
    const pending = saved<Json | null>(creationKey, null);
    creation.current = pending;
    setPendingCreation(pending);
  }, [creationKey, data?.stateDir]);
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
    roomRoots = room ? roomLeadIds(room, agents) : [],
    lead = agents.find(
      (a) =>
        a.id ===
        (agent?.rootId ||
          (agent?.isLead ? agent.id : undefined) ||
          (roomContext && roomRoots.includes(roomContext)
            ? roomContext
            : roomRoots[0])),
    ),
    team = agents.filter((a) => a.rootId === lead?.id),
    workers = team.filter((a) => !a.isLead);
  useEffect(() => {
    setWorkerQuery("");
    setWorkerFilter("all");
    setCompletedOpen(false);
  }, [lead?.id]);
  useEffect(() => {
    if (agent && !agent.isLead && agent.status === "completed")
      setCompletedOpen(true);
  }, [agent?.id, agent?.status]);
  useEffect(() => {
    if (!mobileClient) return;
    if (opened && (room || legacy) && !agent)
      setOpened(lead?.id || leads.at(-1)?.id || null);
  }, [mobileClient, opened, agent?.isLead, lead?.id]);
  const accounts = useAccounts(data?.stateDir);
  const accountKey =
    agent?.accountKey ||
    lead?.accountKey ||
    (agent ? "default" : accounts.data.defaultAccountKey);
  const workerModels = useWorkerModels(
    accountKey,
    !!agent && agent.source === "managed",
  );
  const limitsRequests = useRef(new Map<string, Promise<void>>());
  const selectedAccount = accounts.data.accounts.find(
    (a) => a.id === accountKey,
  );
  const accountId = selectedAccount?.accountId;
  const cachedLimits = accountLimits(
    limitsByAccount[accountKey],
    accountKey,
    accountId,
  );
  const snapshotLimits =
    data?.runtime.rateLimitsByAccount?.[accountKey] ||
    (accountKey === "default" ? data?.runtime.rateLimits : null);
  const matchingSnapshot = accountLimits(snapshotLimits, accountKey, accountId);
  const visibleLimits =
    matchingSnapshot &&
    (!cachedLimits || (matchingSnapshot.at || 0) > (cachedLimits.at || 0))
      ? matchingSnapshot
      : cachedLimits || null;
  const chatData = chatSnapshot(data, lead?.id);
  const attentionCount = messageAttentionCount(chatData);
  const taskCount = backgroundTasks(chatData).filter(activeTask).length;
  const setDraft = (
    text: string | ((current: string) => string),
    id = opened || "new",
  ) =>
    setDrafts((old) => {
      const next = {
        ...old,
        [id]: typeof text === "function" ? text(old[id] || "") : text,
      };
      save("codex-agent-drafts", next);
      return next;
    });
  const open = (id: string, messageId?: string) => {
    setJumpTarget(
      messageId
        ? { chatId: id, messageId, requestId: crypto.randomUUID() }
        : undefined,
    );
    if (mobileClient) {
      const target = agents.find((item) => item.id === id);
      if (!target) return;
    }
    setRoomContext(lead?.id || null);
    setOpened(id);
    setSidebar(false);
    setTeamOpen(false);
  };
  useEffect(() => {
    if (!data) return;
    const scope = `${data.stateDir}:${mobileClient ? "mobile" : "desktop"}`;
    const key = mobileClient
      ? "codex-mobile-opened"
      : `codex-desktop-opened:${data.stateDir}`;
    if (selectionScope.current !== scope) {
      selectionScope.current = scope;
      const selected = saved<string | null>(key, null);
      if (
        selected &&
        (data.threads.some((item) => item.id === selected) ||
          (!mobileClient &&
            (data.runtime.rooms.some((item) => item.id === selected) ||
              data.chats.some((item) => item.id === selected))))
      ) {
        setOpened(selected);
        return;
      }
    }
    // The create response can arrive before the replicated chat projection.
    if (
      createdSelection.current &&
      opened === createdSelection.current &&
      !agent
    )
      return;
    if (agent?.id === createdSelection.current) createdSelection.current = null;
    if (!opened || (!agent && !room && !legacy))
      setOpened(leads.at(-1)?.id || null);
    else save(key, opened);
  }, [data, opened, agent, room, legacy, mobileClient]);
  useEffect(() => {
    const onError = (event: Event) =>
      notify((event as CustomEvent<string>).detail);
    window.addEventListener("desktop-error", onError);
    return () => window.removeEventListener("desktop-error", onError);
  }, [notify]);
  const [backendUpdatePending, setBackendUpdatePending] = useState(false);
  useEffect(() => {
    const check = window.codexDesktop?.getBackendUpdate;
    if (!check) return;
    let active = true;
    let checking = false;
    const refreshUpdate = () => {
      if (checking) return;
      checking = true;
      void check()
        .then((status) => {
          if (active) setBackendUpdatePending(status.updateRequired === true);
        })
        .catch(() => {
          // Connection notices already report an unavailable backend.
        })
        .finally(() => {
          checking = false;
        });
    };
    refreshUpdate();
    window.addEventListener("focus", refreshUpdate);
    document.addEventListener("visibilitychange", refreshUpdate);
    return () => {
      active = false;
      window.removeEventListener("focus", refreshUpdate);
      document.removeEventListener("visibilitychange", refreshUpdate);
    };
  }, []);
  const [navigationTarget, setNavigationTarget] = useState<{
    agentId: string;
    section: "messages";
    itemId?: string;
    requestId: string;
  }>();
  useEffect(() => {
    const navigate = (target: {
      agentId: string;
      section: "messages";
      itemId?: string;
    }) => {
      setNavigationTarget({ ...target, requestId: crypto.randomUUID() });
    };
    const unsubscribe = window.codexDesktop?.onNavigate?.(navigate);
    const onNavigate = (event: Event) =>
      navigate((event as CustomEvent<Parameters<typeof navigate>[0]>).detail);
    window.addEventListener("studio-navigate", onNavigate);
    return () => {
      unsubscribe?.();
      window.removeEventListener("studio-navigate", onNavigate);
    };
  }, []);
  useEffect(() => {
    if (!navigationTarget || !data) return;
    if (!agents.some((item) => item.id === navigationTarget.agentId)) {
      notify("This chat is no longer available.");
    } else {
      setOpened(navigationTarget.agentId);
      setWorkspaceSection("messages");
      setWorkspaceFocus(
        navigationTarget.itemId
          ? {
              id: navigationTarget.itemId,
              requestId: navigationTarget.requestId,
            }
          : undefined,
      );
      setWorkspaceOpen(true);
    }
    setNavigationTarget(undefined);
  }, [navigationTarget, data, agents, notify]);
  const limitsCache = useRef(limitsByAccount);
  limitsCache.current = limitsByAccount;
  const reloadLimits = useCallback(
    (force = false) => {
      const pending = limitsRequests.current.get(accountKey);
      if (pending) return pending;
      const cached = accountLimits(
        limitsCache.current[accountKey],
        accountKey,
        accountId,
      );
      if (
        !force &&
        cached?.data &&
        !cached.error &&
        Date.now() / 1000 - (cached.at || 0) < 60
      )
        return Promise.resolve();
      const query =
        accountKey === "default"
          ? ""
          : `?account_key=${encodeURIComponent(accountKey)}`;
      setLimitsLoading((old) => ({ ...old, [accountKey]: true }));
      const request = api("/api/limits" + query, undefined, {
        timeoutMs: 25000,
      })
        .then((result) => {
          if (!accountLimits(result, accountKey, accountId))
            throw new Error("Codex returned limits for another account.");
          setLimitsByAccount((old) => {
            const previous = accountLimits(
              old[accountKey],
              accountKey,
              accountId,
            );
            if (previous && (previous.at || 0) > (result.at || 0)) return old;
            return { ...old, [accountKey]: { ...result, accountKey } };
          });
        })
        .catch((error) => {
          setLimitsByAccount((old) => ({
            ...old,
            [accountKey]: {
              ...(old[accountKey] || { data: null }),
              error: errorText(error),
              accountKey,
            },
          }));
        })
        .finally(() => {
          limitsRequests.current.delete(accountKey);
          setLimitsLoading((old) => ({ ...old, [accountKey]: false }));
        });
      limitsRequests.current.set(accountKey, request);
      return request;
    },
    [accountKey, accountId],
  );
  const forceReloadLimits = useCallback(
    () => reloadLimits(true),
    [reloadLimits],
  );
  useEffect(() => {
    if (!data?.stateDir) return;
    void reloadLimits();
    const timer = setInterval(() => {
      if (document.visibilityState === "visible") void reloadLimits();
    }, 60000);
    const refreshVisible = () => {
      if (document.visibilityState === "visible") void reloadLimits();
    };
    window.addEventListener("focus", refreshVisible);
    document.addEventListener("visibilitychange", refreshVisible);
    return () => {
      clearInterval(timer);
      window.removeEventListener("focus", refreshVisible);
      document.removeEventListener("visibilitychange", refreshVisible);
    };
  }, [data?.stateDir, opened, reloadLimits]);
  useEffect(() => {
    if (!visibleLimits?.error) return;
    const timer = setTimeout(() => {
      void reloadLimits();
    }, 30000);
    return () => clearTimeout(timer);
  }, [accountKey, visibleLimits?.error, reloadLimits]);
  useEffect(() => {
    // Keep each account's latest snapshot for immediate return navigation.
    const incoming = { ...data?.runtime.rateLimitsByAccount };
    if (data?.runtime.rateLimits && !incoming.default)
      incoming.default = data.runtime.rateLimits;
    setLimitsByAccount((old) => {
      let next = old;
      for (const [key, value] of Object.entries(incoming)) {
        if (!value?.at || (value.accountKey || "default") !== key) continue;
        if (!old[key] || value.at > (old[key].at || 0)) {
          next = { ...next, [key]: { ...value, accountKey: key } };
        }
      }
      return next;
    });
  }, [data?.runtime.rateLimits, data?.runtime.rateLimitsByAccount]);
  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "k") {
        e.preventDefault();
        setWorkspaceSection("search");
        setWorkspaceOpen(true);
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
  const newChat = async (cwd?: string, projectFolder?: string) => {
    if (creationLock.current) return null;
    try {
      const pending = JSON.parse(localStorage.getItem(creationKey) || "null");
      if (pending && (typeof pending.id !== "string" || !pending.id))
        throw new Error("The saved chat request is invalid.");
      creation.current = pending;
    } catch (error) {
      notify("Cannot read the saved chat request: " + errorText(error));
      return null;
    }
    if (
      creation.current &&
      cwd &&
      (creation.current.cwd !== cwd ||
        (creation.current.project_folder || undefined) !== projectFolder)
    ) {
      notify(
        "Retry the previous chat request before you start a chat in another project or folder.",
      );
      return null;
    }
    if (creation.current) cwd = creation.current.cwd;
    if (mobileClient && !cwd) {
      cwd =
        lead?.cwd ||
        data?.runtime.projects?.[0]?.path ||
        leads.find((item) => item.cwd)?.cwd;
      if (!cwd) {
        setSidebar(false);
        setModal({
          title: "Choose project folder",
          body: (
            <ProjectDirectoryPicker
              onSelect={async (path) => {
                await api("/api/projects", { path });
                setModal(null);
                await refresh();
                await newChat(path);
              }}
            />
          ),
        });
        return null;
      }
    }
    creationLock.current = true;
    setCreating(true);
    creation.current ||= {
      id: crypto.randomUUID(),
      previous: lead?.id || null,
      reuse_empty: false,
      model: "gpt-6-astra",
      ...(cwd ? { cwd } : {}),
      ...(projectFolder ? { project_folder: projectFolder } : {}),
      ...(cwd &&
      data?.runtime.projects?.find((project) => project.path === cwd)
        ?.accountKey
        ? {
            account_key: data.runtime.projects.find(
              (project) => project.path === cwd,
            )!.accountKey,
          }
        : {}),
    };
    if (!opened && drafts.new) setDraft(drafts.new, creation.current.id);
    try {
      // Save the exact request before sending it. A lost response must retain this identity.
      localStorage.setItem(creationKey, JSON.stringify(creation.current));
      setPendingCreation(creation.current);
      const a = await api("/api/leads", creation.current);
      if (a.id !== creation.current.id)
        throw new Error("The server returned another chat identity.");
      localStorage.removeItem(creationKey);
      setPendingCreation(null);
      if (!opened && drafts.new) setDraft(drafts.new, a.id);
      setToast("");
      creation.current = null;
      createdSelection.current = a.id;
      await refresh();
      setOpened(a.id);
      setSidebar(false);
      setTeamOpen(false);
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
    delivery?: "queue" | "steer" | "after_tool";
    attachments?: Json[];
    onPersist?: () => void | Promise<void>;
  }) => {
    const draftKey = opened || "new",
      text = (drafts[draftKey] || "").trim();
    if ((!text && !options?.assets?.length) || sendingLock.current) return;
    const attempt = Symbol();
    sendingLock.current = attempt;
    latestSend.current[draftKey] = attempt;
    setSending(true);
    const release = () => {
      if (sendingLock.current !== attempt) return;
      sendingLock.current = null;
      setSending(false);
    };
    let request: Json | undefined;
    try {
      const id = opened || (await newChat());
      if (!id) return;
      latestSend.current[id] = attempt;
      if (
        agent?.source === "managed" &&
        /^\/(compact|review|stop|stop-team)(\s|$)/.test(text)
      ) {
        const [command] = text.split(/\s+/);
        if (options?.assets?.length)
          throw new Error(
            "Commands cannot include files. Remove the attachments or send a normal message.",
          );
        if (text !== command)
          throw new Error(
            "Use the command without additional text, or send a normal message.",
          );
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
            JSON.stringify(options?.assets || [])
        )
          sends.current[id] = {
            id: crypto.randomUUID(),
            room: id,
            text,
            assets: options?.assets || [],
            delivery: options?.delivery || "after_tool",
          };
        // Retain the exact request until the durable outbox owns its retry.
        // A turn ending can change the default delivery mode, not this receipt.
        persistSends();
        request = sends.current[id];
        const entry: OutgoingMessage = {
          id: request.id,
          body: request,
          status: "sending",
          created: Date.now(),
          attachments: options?.attachments || [],
          displayPending: true,
        };
        setOutgoing((old) => ({ ...old, [entry.id]: entry }));
        const result = await durableSend(
          request,
          entry.attachments,
          async () => {
            // Keep the draft until a durable store owns the immutable message.
            setDrafts((old) => {
              const next = { ...old };
              if (next[draftKey]?.trim() === text) delete next[draftKey];
              if (next[id]?.trim() === text) delete next[id];
              save("codex-agent-drafts", next);
              return next;
            });
            // The outbox now owns this immutable request. A new user submission,
            // including identical text, gets its own identity.
            if (sends.current[id]?.id === entry.id) {
              delete sends.current[id];
              persistSends();
            }
            await options?.onPersist?.();
            release();
          },
        );
        setOutgoing((old) => ({
          ...old,
          [entry.id]: {
            ...entry,
            status: result.queued
              ? "queued"
              : result.status === "uncertain"
                ? "uncertain"
                : "accepted",
            receipt: result,
            error: result.error,
          },
        }));
        if (result.status === "cancelled") {
          if (sends.current[id]?.id === entry.id) {
            delete sends.current[id];
            persistSends();
          }
          throw new ApiError(
            "This message was cancelled. Send it again to resume.",
            400,
          );
        }
        if (result.status === "failed")
          throw new ApiError(result.error || "The message was not sent.", 400);
        if (result.status === "uncertain")
          throw new Error(
            result.error ||
              "Delivery is uncertain. Inspect the conversation before sending again.",
          );
        if (sends.current[id]?.id === entry.id) {
          delete sends.current[id];
          persistSends();
        }
        if (Object.values(result.deliveries || {}).some((v) => v !== "queued"))
          notify("Message saved. Some deliveries are not confirmed.");
      }
      if (!request)
        setDrafts((old) => {
          const next = { ...old };
          if (next[draftKey]?.trim() === text) delete next[draftKey];
          if (next[id]?.trim() === text) delete next[id];
          save("codex-agent-drafts", next);
          return next;
        });
      void refresh().catch((error) => notify(errorText(error)));
    } catch (e) {
      if (request) {
        // Never replace text the user typed while this request was in flight.
        const targetKey = request.room || draftKey;
        setDrafts((old) => {
          if (
            latestSend.current[targetKey] !== attempt ||
            old[targetKey]?.trim()
          )
            return old;
          const next = { ...old, [targetKey]: text };
          save("codex-agent-drafts", next);
          return next;
        });
        const key = request.id;
        const rejected =
          e instanceof ApiError &&
          e.status < 500 &&
          ![408, 429].includes(e.status);
        if (rejected && sends.current[request.room]?.id === key)
          delete sends.current[request.room];
        if (
          !rejected &&
          latestSend.current[targetKey] === attempt &&
          !sends.current[request.room]
        )
          sends.current[request.room] = request;
        persistSends();
        setOutgoing((old) =>
          old[key]
            ? {
                ...old,
                [key]: {
                  ...old[key],
                  status: rejected ? "failed" : "uncertain",
                  error: errorText(e),
                },
              }
            : old,
        );
      }
      if (!request) notify(errorText(e));
      throw e;
    } finally {
      release();
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
    setSidebar(false);
    setModal({
      title: "Choose project folder",
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
    if (agent?.isLead && !agent.threadId) folders(agent);
    else if (agent?.cwd)
      setModal({
        title: "Project folder",
        body: (
          <>
            <p>{agent.cwd}</p>
            <p>
              This chat keeps its project. Start a new chat to choose another
              folder.
            </p>
            {window.codexDesktop && (
              <Button
                onClick={() =>
                  void window.codexDesktop
                    ?.revealPath(agent.cwd!)
                    .catch((error) => notify(errorText(error)))
                }
              >
                Show in Finder
              </Button>
            )}
          </>
        ),
      });
  };
  if (!data)
    return (
      <div className="startup">{error || "Connecting to Codex Studio…"}</div>
    );
  const title = agent?.name || room?.name || legacy?.name || "New conversation";
  const projectName =
    data.runtime.projects?.find((item) => item.path === agent?.cwd)?.name ||
    agent?.cwd?.split("/").filter(Boolean).at(-1);
  const answerIds = awaitingAnswerIds(chatData?.runtime.requests || []);
  const deferredIds = new Set<string>(
    (chatData?.runtime.requests || [])
      .filter((request) => request.deferred && request.status === "pending")
      .map((request) => request.agent),
  );
  const worker = (a: Agent) => (
    <WorkerCard
      key={a.id}
      agent={a}
      selected={opened === a.id}
      awaitingAnswer={workerState(a, answerIds, deferredIds) === "answer"}
      deferred={deferredIds.has(a.id)}
      open={() => open(a.id)}
    />
  );
  const needsAttention = (a: Agent) =>
    ["attention", "answer"].includes(workerState(a, answerIds, deferredIds));
  const query = workerQuery.trim().toLowerCase();
  const shown = workers.filter((a) =>
    query
      ? `${a.name} ${a.status} ${a.role || ""} ${a.overview?.task || ""} ${a.overview?.result || ""}`
          .toLowerCase()
          .includes(query)
      : workerFilter === "attention"
        ? needsAttention(a)
        : workerFilter === "active"
          ? busy.has(a.status)
          : true,
  );
  const groups = [
    { name: "Attention", workers: shown.filter(needsAttention) },
    {
      name: "Working",
      workers: shown.filter(
        (a) => workerState(a, answerIds, deferredIds) === "working",
      ),
    },
    {
      name: "Waiting",
      workers: shown.filter(
        (a) => workerState(a, answerIds, deferredIds) === "waiting",
      ),
    },
  ];
  const completed = shown.filter(
    (a) => workerState(a, answerIds, deferredIds) === "completed",
  );
  const smallTeam = workers.length <= 3;
  const showTeamFilters = !smallTeam || !!workerQuery || workerFilter !== "all";
  const teamPanel = (
    <aside
      id="team"
      aria-label="Team"
      className={smallTeam ? "team-compact" : undefined}
    >
      <div className="team-heading">
        <h2>Team</h2>
        <ActionIcon
          aria-label="Close team"
          id="team-close"
          onClick={() => {
            setTeamOpen(false);
            setWideTeamOpen(false);
            save("codex-team-open", false);
          }}
        >
          <X size={16} />
        </ActionIcon>
        <span>
          {workers.length} {workers.length === 1 ? "subagent" : "subagents"}
        </span>
      </div>
      {!smallTeam && (
        <TeamSummary
          workers={workers}
          answers={answerIds}
          deferred={deferredIds}
        />
      )}
      <Button
        id="lead-row"
        aria-current={opened === lead?.id ? "page" : undefined}
        leftSection={<ArrowLeft size={13} />}
        onClick={() => lead && open(lead.id)}
      >
        {lead?.name || "Main agent"}
      </Button>
      {showTeamFilters && (
        <>
          <TextInput
            leftSection={<Search size={14} />}
            id="worker-search"
            type="search"
            aria-label="Find a subagent"
            placeholder="Find a subagent"
            value={workerQuery}
            onChange={(e) => setWorkerQuery(e.target.value)}
          />
          {query ? (
            <p className="team-search-count" role="status">
              {shown.length} {shown.length === 1 ? "match" : "matches"} in this
              team
            </p>
          ) : (
            <div
              className="team-filters"
              role="group"
              aria-label="Filter subagents"
            >
              {[
                ["all", "All", workers.length],
                [
                  "active",
                  "Active",
                  workers.filter((a) => busy.has(a.status)).length,
                ],
                [
                  "attention",
                  "Attention",
                  workers.filter(needsAttention).length,
                ],
              ].map(([value, label, count]) => (
                <UnstyledButton
                  key={value}
                  aria-pressed={workerFilter === value}
                  onClick={() => setWorkerFilter(String(value))}
                >
                  {label} <span>{count}</span>
                </UnstyledButton>
              ))}
            </div>
          )}
        </>
      )}
      <div id="workers">
        {groups.map((group) =>
          group.workers.length ? (
            <section
              className="team-status-group"
              aria-label={group.name}
              key={group.name}
            >
              {!smallTeam && (
                <h3>
                  {group.name}
                  <span>{group.workers.length}</span>
                </h3>
              )}
              {group.workers.map(worker)}
            </section>
          ) : null,
        )}
        {!!completed.length && (
          <details
            className="worker-group"
            open={!!query || completedOpen}
            onToggle={(event) => {
              if (!query) setCompletedOpen(event.currentTarget.open);
            }}
          >
            <summary>Completed · {completed.length}</summary>
            {completed.map(worker)}
          </details>
        )}
        {!shown.length && (
          <p className="team-empty" role="status">
            {query
              ? "No subagents match your search."
              : "No subagents in this group."}
          </p>
        )}
      </div>
    </aside>
  );
  return (
    <>
      <Sidebar
        data={data}
        opened={opened}
        lead={lead}
        open={open}
        newChat={(path, folder) => void newChat(path, folder)}
        addProject={() => {
          setSidebar(false);
          setModal({
            title: "Add project",
            body: (
              <ProjectDirectoryPicker
                onSelect={async (path) => {
                  await api("/api/projects", { path });
                  setModal(null);
                  await refresh();
                }}
              />
            ),
          });
        }}
        changeProject={folders}
        projectAccount={(path) => {
          setSidebar(false);
          setModal({
            title: "Project account",
            body: (
              <ProjectAccount
                path={path}
                project={data.runtime.projects?.find(
                  (item) => item.path === path,
                )}
                accounts={accounts.data}
                defaultAccountKey={
                  data.runtime.projects
                    ?.filter(
                      (item) =>
                        path === item.path ||
                        path.startsWith(item.path.replace(/\/$/, "") + "/"),
                    )
                    .sort((a, b) => b.path.length - a.path.length)[0]
                    ?.accountKey || accounts.data.defaultAccountKey
                }
                saved={async () => {
                  await refresh();
                  setModal(null);
                }}
              />
            ),
          });
        }}
        creating={creating}
        refresh={refresh}
        notify={notify}
        rename={rename}
        remove={remove}
        mobile={sidebar}
        collapsed={sidebarCollapsed}
        onSearch={() => {
          setSidebar(false);
          setWorkspaceSection("search");
          setWorkspaceOpen(true);
        }}
        close={() => {
          if (mobileClient) setSidebar(false);
          else {
            setSidebarCollapsed(true);
            save("codex-sidebar-collapsed", true);
          }
        }}
      />
      <main className="workspace">
        <header className="workspace-header simple-workspace-header">
          <ActionIcon
            id="sidebar-toggle"
            aria-label="Toggle conversations"
            aria-expanded={mobileClient ? sidebar : !sidebarCollapsed}
            onClick={() => {
              if (mobileClient) setSidebar(!sidebar);
              else {
                setSidebarCollapsed(!sidebarCollapsed);
                save("codex-sidebar-collapsed", !sidebarCollapsed);
              }
            }}
          >
            <PanelLeft size={18} />
          </ActionIcon>
          <div className="conversation-heading">
            {lead && agent?.id !== lead.id && (
              <Button
                size="compact-xs"
                leftSection={<ArrowLeft size={13} />}
                id="back-lead"
                onClick={() => open(lead.id)}
              >
                Back to main agent
              </Button>
            )}
            <h1 id="conversation-title" title={title}>
              {title}
            </h1>
            <span id="conversation-status">
              {mobileClient && agent?.cwd ? `${projectName} · ` : ""}
              {agent
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
          {
            <ActionIcon
              aria-label="Chat settings"
              onClick={() => setSettingsOpen(true)}
            >
              <Settings size={20} />
            </ActionIcon>
          }
          {!!workers.length && (
            <Button
              leftSection={<Users size={16} />}
              id="team-toggle"
              aria-label="Team"
              aria-expanded={narrowTeam ? teamOpen : wideTeamOpen}
              onClick={() => {
                if (narrowTeam) setTeamOpen(!teamOpen);
                else {
                  setWideTeamOpen(!wideTeamOpen);
                  save("codex-team-open", !wideTeamOpen);
                }
              }}
            >
              Team
              {workers.length > 0 && (
                <span className="team-button-count">
                  {workers.filter((worker) => busy.has(worker.status)).length}/
                  {workers.length}
                </span>
              )}
            </Button>
          )}
          <Button
            id="messages-toggle"
            leftSection={<MessageSquare size={16} />}
            disabled={!lead}
            onClick={() => {
              setWorkspaceSection("messages");
              setWorkspaceFocus(undefined);
              setWorkspaceOpen(true);
            }}
          >
            Messages{" "}
            {attentionCount > 0 && (
              <span className="attention-count">{attentionCount}</span>
            )}
          </Button>
          <Menu position="bottom-end" withinPortal>
            <Menu.Target>
              <ActionIcon
                ref={chatActionsButton}
                aria-label="Chat actions"
                title="Chat actions"
              >
                <MoreHorizontal size={18} />
              </ActionIcon>
            </Menu.Target>
            <Menu.Dropdown>
              {(
                [
                  ["work", "Agent tasks", ListTodo],
                  ["user-tasks", "Your tasks", CheckCheck],
                  ["changes", "Changes", FileDiff],
                  ["plan", "Plan", BookOpen],
                  ["rules", "Rules", Clock3],
                  ["search", "Search", Search],
                ] as const
              ).map(([section, label, Icon]) => (
                <Menu.Item
                  key={section}
                  id={section === "work" ? "workspace-toggle" : undefined}
                  aria-label={section === "work" ? "Agent tasks" : label}
                  data-workspace-section={section}
                  leftSection={<Icon size={14} />}
                  onClick={() => {
                    setWorkspaceSection(section);
                    setWorkspaceOpen(true);
                  }}
                >
                  {label}
                </Menu.Item>
              ))}
              <Menu.Item
                id="tasks-toggle"
                aria-label={`Background tasks${taskCount ? `, ${taskCount} active` : ""}`}
                leftSection={<Activity size={14} />}
                onClick={() => setTasksOpen(true)}
              >
                Background {taskCount || ""}
              </Menu.Item>
              {agent?.source === "managed" && (
                <>
                  <Menu.Divider />
                  {(
                    [
                      ["compact", "Compact", Minimize2],
                      ["review", "Review", ShieldCheck],
                    ] as const
                  ).map(([action, label, Icon]) => (
                    <Menu.Item
                      key={action}
                      data-action={action}
                      leftSection={<Icon size={14} />}
                      disabled={
                        busy.has(agent.status) ||
                        !!agent.inFlight ||
                        !!nativeThreadError(agent) ||
                        !agent.threadId
                      }
                      onClick={() => {
                        void run(() =>
                          api("/api/action", { id: agent.id, action }),
                        );
                      }}
                    >
                      {label}
                    </Menu.Item>
                  ))}
                  {(!!agent.inFlight ||
                    team.some(
                      (member) =>
                        busy.has(member.status) ||
                        member.status === "queued" ||
                        member.inFlight,
                    )) && (
                    <Menu.Item
                      data-action="stop-team"
                      color="red"
                      leftSection={<Square size={14} />}
                      onClick={() => {
                        void run(() =>
                          api("/api/stop", {
                            id: agent.rootId,
                            descendants: true,
                          }),
                        );
                      }}
                    >
                      Stop team
                    </Menu.Item>
                  )}
                </>
              )}
            </Menu.Dropdown>
          </Menu>
        </header>
        {error && (
          <div id="error" role="alert">
            {error}
          </div>
        )}
        <div className="sync-notices">
          {backendUpdatePending && (
            <p
              className="sync-status"
              role="status"
              data-backend-update-pending
            >
              Server update pending. Active work continues.
            </p>
          )}
          {pendingCreation && !creating && (
            <div className="sync-status">
              <p>
                The previous chat request needs confirmation:{" "}
                {pendingCreation.cwd || "default project"}.
              </p>
              <Button onClick={() => void newChat()}>Retry chat request</Button>
            </div>
          )}
          {draftError && (
            <p className="sync-status" role="status" data-draft-sync-status>
              {draftError}
            </p>
          )}
          {outbox.error && (
            <p className="sync-status" role="alert">
              {outbox.error}
            </p>
          )}
        </div>
        <Conversation
          syncWorkspaceId={workspaceId}
          id={opened}
          agent={agent}
          room={room}
          legacy={legacy}
          data={data}
          draft={drafts[opened || "new"] || ""}
          setDraft={setDraft}
          draftConflicts={draftConflicts.filter(
            (version) => version.session === (opened || "new"),
          )}
          dismissDraft={dismissDraft}
          send={send}
          sending={sending}
          outgoing={visibleOutgoing}
          onObserved={observeSends}
          onOutgoingEdit={editSend}
          refresh={refresh}
          notify={notify}
          limits={visibleLimits}
          limitsLoading={!!limitsLoading[accountKey]}
          jumpTarget={jumpTarget?.chatId === opened ? jumpTarget : undefined}
          limitsAccountLabel={selectedAccount?.email || selectedAccount?.label}
          reloadLimits={forceReloadLimits}
          onPhase={onPhase}
          onSelect={open}
          onBranchCreated={(id) => {
            createdSelection.current = id;
            setOpened(id);
            setSidebar(false);
            setTeamOpen(false);
          }}
          onNewChat={() => void newChat(agent?.cwd || lead?.cwd)}
          onChooseChat={() => {
            setSidebar(true);
            setSidebarCollapsed(false);
            save("codex-sidebar-collapsed", false);
            requestAnimationFrame(() =>
              document
                .querySelector<HTMLInputElement>(
                  '#sidebar [aria-label="Filter projects and chats"]',
                )
                ?.focus(),
            );
          }}
        />
      </main>
      {!!workers.length &&
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
        ) : wideTeamOpen ? (
          teamPanel
        ) : null)}
      {!mobileClient && (
        <TerminalDock data={data} agent={agent || lead} notify={notify} />
      )}
      <Workspace
        allRequests={data.runtime.requests}
        key={`workspace:${lead?.id || "none"}`}
        initialSection={workspaceSection}
        initialFocus={workspaceFocus}
        opened={workspaceOpen}
        onClose={() => setWorkspaceOpen(false)}
        agent={agent || lead}
        data={chatData!}
        onSelect={open}
        refresh={refresh}
        notify={notify}
      />
      <BackgroundTasks
        key={`background:${lead?.id || "none"}`}
        opened={tasksOpen}
        close={() => setTasksOpen(false)}
        afterClose={() =>
          requestAnimationFrame(() => chatActionsButton.current?.focus())
        }
        data={chatData!}
        leadId={lead?.id}
        openAgent={open}
        refresh={refresh}
        notify={notify}
      />
      <Modal
        opened={settingsOpen}
        closeOnEscape={
          !accountModalOpen && !mainSettingsOpen && !subagentSettingsOpen
        }
        closeOnClickOutside={
          !accountModalOpen && !mainSettingsOpen && !subagentSettingsOpen
        }
        onClose={() => setSettingsOpen(false)}
        title="Chat settings"
      >
        <div className="chat-settings-panel">
          <NativeSelect
            label="Appearance"
            aria-label="Appearance"
            value={colorScheme}
            data={[
              { value: "auto", label: "System" },
              { value: "light", label: "Light" },
              { value: "dark", label: "Dark" },
            ]}
            onChange={(event) =>
              setColorScheme(
                event.currentTarget.value as "auto" | "light" | "dark",
              )
            }
          />
          <Accounts
            onModalOpenChange={setAccountModalOpen}
            projectAccountKeys={
              data.runtime.projects
                ?.filter(
                  (project) =>
                    (agent || lead)?.cwd === project.path ||
                    (agent || lead)?.cwd?.startsWith(project.path + "/"),
                )
                .sort((a, b) => b.path.length - a.path.length)[0]?.accountKeys
            }
            state={accounts}
            agent={agent || lead}
            accountKey={accountKey}
            onError={notify}
            changeAccount={async (key) => {
              if (agent?.isLead) {
                await api("/api/agents/account", {
                  id: agent.id,
                  account_key: key,
                });
                await refresh();
              } else {
                accounts.setData(
                  await api("/api/accounts/default", {
                    account_key: key,
                  }),
                );
              }
            }}
          />
          {agent?.cwd && (
            <Button
              id="project"
              className="project-picker"
              leftSection={<Folder size={15} />}
              aria-label="Choose project folder"
              title={agent.cwd}
              onClick={() => {
                setSettingsOpen(false);
                project();
              }}
            >
              {projectName}
            </Button>
          )}
          {agent?.source === "managed" && (
            <ExecutionSettings
              key={"execution:" + agent.id}
              onOpenChange={setMainSettingsOpen}
              agent={agent}
              catalog={workerModels}
              refresh={refresh}
            />
          )}
          {lead?.isLead && (
            <ExecutionSettings
              key={"defaults:" + lead.id}
              onOpenChange={setSubagentSettingsOpen}
              agent={lead}
              catalog={workerModels}
              refresh={refresh}
              teamDefaults
            />
          )}
          {agent?.cwd && (
            <Button
              disabled={creating}
              onClick={() => {
                setSettingsOpen(false);
                void newChat(agent.cwd);
              }}
            >
              New chat in this project
            </Button>
          )}
        </div>
      </Modal>
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
