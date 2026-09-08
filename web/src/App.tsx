import { accountLimits } from "./accountUsage";
import { useMobileViewport } from "./hooks/mobileViewport";
import { chatSnapshot, roomLeadIds } from "./chatScope";
import {
  ActionIcon,
  Button,
  Drawer,
  Modal,
  Menu,
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
  MoreHorizontal,
  ShieldCheck,
  Settings,
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
import {
  busy,
  statusLabel,
  complaintNeedsUserResponse,
  type Agent,
  type Json,
} from "./types";
import Sidebar from "./components/Sidebar";
import TeamChats from "./components/TeamChats";
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
import Canvas from "./components/Canvas";
import ComplaintBook from "./components/ComplaintBook";
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
  const [accountChanging, setAccountChanging] = useState(false);
  const selectionScope = useRef("");
  useMobileViewport(mobileClient);
  const narrowTeam = useMediaQuery("(max-width: 1199px)");
  const { data, error, refresh } = useSnapshot(),
    [opened, setOpened] = useState<string | null>(() =>
      window.matchMedia("(max-width: 760px)").matches
        ? saved("codex-mobile-opened", null)
        : null,
    ),
    [roomContext, setRoomContext] = useState<string | null>(null),
    [view, setView] = useState("chat"),
    [sidebar, setSidebar] = useState(false),
    [teamOpen, setTeamOpen] = useState(false),
    [tasksOpen, setTasksOpen] = useState(false),
    [agentChatsOpen, setAgentChatsOpen] = useState(false),
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
    sendingLock = useRef(false),
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
    setView("chat");
    setTeamOpen(false);
    setWorkspaceOpen(false);
    setTasksOpen(false);
    setAgentChatsOpen(false);
    if (opened && (agent || room || legacy) && !agent?.isLead)
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
  const attentionCount =
    (chatData?.runtime.requests.filter((request) => !request.deferred).length ||
      0) +
    (chatData?.runtime.userTasks?.filter((task) => task.status === "open")
      .length || 0) +
    (chatData?.runtime.complaints.filter(complaintNeedsUserResponse).length ||
      0) +
    (chatData?.runtime.work?.filter((w: Json) => w.status === "review")
      .length || 0) +
    (chatData?.threads || []).filter((a) =>
      ["failed", "interrupted"].includes(a.status),
    ).length;
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
  const open = (id: string) => {
    if (mobileClient) {
      const target = agents.find((item) => item.id === id);
      const root = target?.isLead
        ? target
        : agents.find((item) => item.id === target?.rootId && item.isLead);
      if (!root) return;
      id = root.id;
    }
    setRoomContext(lead?.id || null);
    setOpened(id);
    setView("chat");
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
        (data.threads.some(
          (item) => item.id === selected && (!mobileClient || item.isLead),
        ) ||
          (!mobileClient &&
            (data.runtime.rooms.some((item) => item.id === selected) ||
              data.chats.some((item) => item.id === selected))))
      ) {
        setOpened(selected);
        return;
      }
    }
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
  const newChat = async (cwd?: string) => {
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
    if (creation.current && cwd && creation.current.cwd !== cwd) {
      notify(
        "Retry the previous chat request before you start a chat in another project.",
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
        notify("Add a project on your Mac before you start a chat.");
        return null;
      }
    }
    if (
      agent?.isLead &&
      agent.empty &&
      !creation.current &&
      (!cwd || cwd === agent.cwd)
    ) {
      setView("chat");
      return agent.id;
    }
    creationLock.current = true;
    setCreating(true);
    creation.current ||= {
      id: crypto.randomUUID(),
      previous: lead?.id || null,
      model: "gpt-6-astra",
      account_key:
        agent?.isLead && agent.empty
          ? agent.accountKey
          : accounts.data.defaultAccountKey,
      ...(cwd ? { cwd } : {}),
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
      await refresh();
      setOpened(a.id);
      setView("chat");
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
    delivery?: "queue" | "steer";
    attachments?: Json[];
  }) => {
    const draftKey = opened || "new",
      text = (drafts[draftKey] || "").trim();
    if ((!text && !options?.assets?.length) || sendingLock.current) return;
    sendingLock.current = true;
    setSending(true);
    let request: Json | undefined;
    try {
      const id = opened || (await newChat());
      if (!id) return;
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
            delivery: options?.delivery || "queue",
          };
        // Retain the exact request across reloads until acceptance is known.
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
        // Clear together with the optimistic message, not after the network reply.
        setDrafts((old) => {
          const next = { ...old };
          if (next[draftKey]?.trim() === text) delete next[draftKey];
          if (next[id]?.trim() === text) delete next[id];
          save("codex-agent-drafts", next);
          return next;
        });
        const result = await durableSend(request, entry.attachments);
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
          delete sends.current[id];
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
        delete sends.current[id];
        persistSends();
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
          if (old[targetKey]?.trim()) return old;
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
  const folders = (target: Agent, nextAccount?: string) => {
    const account = accounts.data.accounts.find(
      (a) => a.id === (nextAccount || target.accountKey || "default"),
    );
    const suggested = account?.projectRules?.allowedProjects || [];
    if (mobileClient) {
      const paths = [
        ...new Set([
          ...(data?.runtime.projects || []).map((item) => item.path),
          ...leads
            .map((item) => item.cwd)
            .filter((path): path is string => !!path),
        ]),
      ].filter(
        (path) =>
          !nextAccount ||
          target.dangerouslySkipAccountRules ||
          !suggested.length ||
          suggested.some(
            (allowed) =>
              path === allowed ||
              path.startsWith(allowed.replace(/\/$/, "") + "/"),
          ),
      );
      setModal({
        title: "Choose existing project",
        body: (
          <div className="mobile-project-list">
            {paths.map((cwd) => (
              <Button
                key={cwd}
                onClick={() =>
                  void run(async () => {
                    await api(
                      nextAccount ? "/api/agents/account" : "/api/conversation",
                      nextAccount
                        ? { id: target.id, account_key: nextAccount, cwd }
                        : { id: target.id, cwd },
                    );
                    setModal(null);
                  })
                }
              >
                {cwd}
              </Button>
            ))}
            {!paths.length && (
              <p>No available projects. Add a project on your Mac.</p>
            )}
          </div>
        ),
      });
      return;
    }
    setModal({
      title: nextAccount
        ? "Choose project for " +
          (account?.email || account?.label || "account")
        : "Choose project folder",
      body: (
        <ProjectDirectoryPicker
          initialPath={nextAccount ? suggested[0] || target.cwd : target.cwd}
          suggestedPaths={suggested}
          onSelect={async (cwd) => {
            await api(
              nextAccount ? "/api/agents/account" : "/api/conversation",
              nextAccount
                ? { id: target.id, account_key: nextAccount, cwd }
                : { id: target.id, cwd },
            );
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
  const title =
    view === "canvas"
      ? "All agents"
      : view === "complaints"
        ? "Complaint book"
        : agent?.name || room?.name || legacy?.name || "New conversation";
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
        <span>{workers.length} workers</span>
      </div>
      <TeamSummary
        workers={workers}
        answers={answerIds}
        deferred={deferredIds}
      />
      <Button
        id="lead-row"
        aria-current={opened === lead?.id ? "page" : undefined}
        leftSection={<ArrowLeft size={13} />}
        onClick={() => lead && open(lead.id)}
      >
        {lead?.name || "Lead"}
      </Button>
      <TextInput
        leftSection={<Search size={14} />}
        id="worker-search"
        type="search"
        aria-label="Find a worker"
        placeholder="Find a worker"
        value={workerQuery}
        onChange={(e) => setWorkerQuery(e.target.value)}
      />
      {query ? (
        <p className="team-search-count" role="status">
          {shown.length} {shown.length === 1 ? "match" : "matches"} in this team
        </p>
      ) : (
        <div className="team-filters" role="group" aria-label="Filter workers">
          {[
            ["all", "All", workers.length],
            [
              "active",
              "Active",
              workers.filter((a) => busy.has(a.status)).length,
            ],
            ["attention", "Attention", workers.filter(needsAttention).length],
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
      <div id="workers">
        {groups.map((group) =>
          group.workers.length ? (
            <section
              className="team-status-group"
              aria-label={group.name}
              key={group.name}
            >
              <h3>
                {group.name}
                <span>{group.workers.length}</span>
              </h3>
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
              ? "No workers match your search."
              : "No workers in this group."}
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
        view={view}
        open={open}
        newChat={(path) => void newChat(path)}
        addProject={() =>
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
          })
        }
        changeProject={folders}
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
                Back to lead
              </Button>
            )}
            <h1 id="conversation-title" title={title}>
              {title}
            </h1>
            <span id="conversation-status">
              {mobileClient && agent?.cwd
                ? `${agent.cwd.split("/").filter(Boolean).at(-1)} · `
                : ""}
              {view === "canvas"
                ? `${leads.length} leads · ${agents.length} agents`
                : view === "complaints"
                  ? "Your inbox and orchestrator follow-up"
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
          {mobileClient && (
            <ActionIcon
              aria-label="Chat settings"
              onClick={() => setSettingsOpen(true)}
            >
              <Settings size={20} />
            </ActionIcon>
          )}
          {!mobileClient && (
            <div
              className="conversation-settings"
              aria-label="Conversation settings"
            >
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
                    const allowed = accounts.data.accounts.find(
                      (a) => a.id === key,
                    )?.projectRules?.allowedProjects;
                    if (
                      allowed &&
                      !agent.dangerouslySkipAccountRules &&
                      !allowed.some(
                        (path) =>
                          agent.cwd === path ||
                          agent.cwd?.startsWith(path.replace(/\/$/, "") + "/"),
                      )
                    ) {
                      folders(agent, key);
                      return;
                    }
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
              {view === "chat" && agent?.cwd && (
                <Button
                  id="project"
                  className="project-picker"
                  leftSection={<Folder size={15} />}
                  aria-label="Choose project folder"
                  title={agent.cwd}
                  onClick={project}
                >
                  {agent.cwd.split("/").filter(Boolean).at(-1)}
                </Button>
              )}
              {view === "chat" && agent?.source === "managed" && (
                <ExecutionSettings
                  key={"execution:" + agent.id}
                  agent={agent}
                  catalog={workerModels}
                  refresh={refresh}
                />
              )}
              {view === "chat" && lead?.isLead && (
                <ExecutionSettings
                  key={"defaults:" + lead.id}
                  agent={lead}
                  catalog={workerModels}
                  refresh={refresh}
                  teamDefaults
                />
              )}
            </div>
          )}
          {!mobileClient && view === "chat" && !!workers.length && (
            <Button
              leftSection={<Users size={16} />}
              id="team-toggle"
              onClick={() => setTeamOpen(!teamOpen)}
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
          {!mobileClient && view === "chat" && agent?.source === "managed" && (
            <div
              className="conversation-quick-actions"
              aria-label="Agent actions"
            >
              <Menu position="bottom-end" withinPortal>
                <Menu.Target>
                  <ActionIcon
                    aria-label="Chat actions"
                    title="Chat actions"
                    variant="subtle"
                  >
                    <MoreHorizontal size={18} />
                  </ActionIcon>
                </Menu.Target>
                <Menu.Dropdown>
                  {(
                    [
                      ["plan", "Plan", BookOpen],
                      ["rules", "Rules", Clock3],
                    ] as const
                  ).map(([section, label, Icon]) => (
                    <Menu.Item
                      key={section}
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
                      disabled={
                        busy.has(agent.status) ||
                        !!agent.inFlight ||
                        !agent.threadId
                      }
                      leftSection={<Icon size={14} />}
                      onClick={() => {
                        void run(() =>
                          api("/api/action", { id: agent.id, action }),
                        );
                      }}
                    >
                      {label}
                    </Menu.Item>
                  ))}
                </Menu.Dropdown>
              </Menu>
              {(!!agent.inFlight ||
                team.some(
                  (member) =>
                    busy.has(member.status) ||
                    member.status === "queued" ||
                    member.inFlight,
                )) && (
                <Button
                  data-action="stop-team"
                  size="compact-xs"
                  variant="subtle"
                  color="red"
                  leftSection={<Square size={14} />}
                  onClick={() => {
                    void run(() =>
                      api("/api/stop", { id: agent.rootId, descendants: true }),
                    );
                  }}
                >
                  Stop team
                </Button>
              )}
            </div>
          )}
        </header>
        {!mobileClient && (
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
                  !!chatData?.runtime.userTasks?.some(
                    (t) => t.status === "open",
                  ) && (
                    <span className="attention-count">
                      {
                        chatData!.runtime.userTasks!.filter(
                          (t) => t.status === "open",
                        ).length
                      }
                    </span>
                  )}
              </Button>
            ))}
            <Button
              id="agent-chats-toggle"
              leftSection={<MessageSquare size={16} />}
              disabled={!lead}
              onClick={() => setAgentChatsOpen(true)}
            >
              Agent chats
            </Button>
            <Button
              id="tasks-toggle"
              aria-label={`Background tasks${taskCount ? `, ${taskCount} active` : ""}`}
              leftSection={<Activity size={16} />}
              onClick={() => {
                setTasksOpen(true);
              }}
            >
              Background{" "}
              {taskCount > 0 && (
                <span className="tasks-count">{taskCount}</span>
              )}
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
        )}
        {error && (
          <div id="error" role="alert">
            {error}
          </div>
        )}
        <div className="sync-notices">
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
        {view === "chat" && (
          <Conversation
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
            limitsAccountLabel={
              selectedAccount?.email || selectedAccount?.label
            }
            reloadLimits={forceReloadLimits}
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
      {!mobileClient &&
        view === "chat" &&
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
      {!mobileClient && (
        <TerminalDock data={data} agent={agent || lead} notify={notify} />
      )}
      <Workspace
        allRequests={data.runtime.requests}
        key={`workspace:${lead?.id || "none"}`}
        initialSection={workspaceSection}
        opened={workspaceOpen}
        onClose={() => setWorkspaceOpen(false)}
        agent={agent || lead}
        data={chatData!}
        onSelect={open}
        refresh={refresh}
        notify={notify}
      />
      <Drawer
        opened={agentChatsOpen}
        closeButtonProps={{ "aria-label": "Close" }}
        onClose={() => setAgentChatsOpen(false)}
        position="right"
        size={940}
        padding={0}
        title={
          <div className="tasks-title">
            <MessageSquare size={19} />
            <strong>Agent chats</strong>
          </div>
        }
        classNames={{
          content: "tasks-drawer",
          body: "tasks-drawer-body",
          header: "tasks-drawer-header",
        }}
      >
        <p className="activity-team-name">{lead?.name}</p>
        {agentChatsOpen && (
          <TeamChats key={lead?.id} data={chatData!} leadId={lead?.id} />
        )}
      </Drawer>
      <BackgroundTasks
        key={`background:${lead?.id || "none"}`}
        opened={tasksOpen}
        close={() => setTasksOpen(false)}
        data={chatData!}
        leadId={lead?.id}
        openAgent={open}
        refresh={refresh}
        notify={notify}
      />
      <Modal
        opened={mobileClient && settingsOpen}
        onClose={() => setSettingsOpen(false)}
        title="Chat settings"
      >
        <div className="mobile-chat-settings">
          <Button
            leftSection={<MessageSquare size={16} />}
            disabled={!lead}
            onClick={() => {
              setSettingsOpen(false);
              setAgentChatsOpen(true);
            }}
          >
            Agent chats
          </Button>
          {agent?.cwd && <p className="mobile-project-path">{agent.cwd}</p>}
          <NativeSelect
            label="Account"
            aria-label="Chat account"
            value={accountKey}
            disabled={
              accountChanging ||
              (!!agent &&
                (!agent.empty || !!agent.threadId || !!agent.inFlight))
            }
            data={accounts.data.accounts.map((account) => ({
              value: account.id,
              label: account.email || account.label || account.id,
            }))}
            onChange={(event) => {
              const key = event.currentTarget.value;
              setAccountChanging(true);
              void run(async () => {
                if (agent?.isLead) {
                  const allowed = accounts.data.accounts.find(
                    (account) => account.id === key,
                  )?.projectRules?.allowedProjects;
                  if (
                    allowed &&
                    !agent.dangerouslySkipAccountRules &&
                    !allowed.some(
                      (path) =>
                        agent.cwd === path ||
                        agent.cwd?.startsWith(path.replace(/\/$/, "") + "/"),
                    )
                  ) {
                    setSettingsOpen(false);
                    folders(agent, key);
                    return;
                  }
                  await api("/api/agents/account", {
                    id: agent.id,
                    account_key: key,
                  });
                } else
                  accounts.setData(
                    await api("/api/accounts/default", { account_key: key }),
                  );
              }).finally(() => setAccountChanging(false));
            }}
          />
          {agent && (!agent.empty || agent.threadId) && (
            <p className="notice">
              Start a new chat to choose another account.
            </p>
          )}
          {agent?.source === "managed" && (
            <ExecutionSettings
              key={agent.id}
              agent={agent}
              catalog={workerModels}
              refresh={refresh}
            />
          )}
          {lead?.isLead && (
            <ExecutionSettings
              key={"mobile-defaults:" + lead.id}
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
