import { Tooltip } from "@mantine/core";
import StudioSettingsTabs, {
  isStudioSettingsTab,
  type StudioSettingsTab,
} from "./components/StudioSettingsTabs";
import { useServerSettings } from "./servers/ServerSettingsContext";
import { useServerActivity } from "./servers/activity";
import ServerAccessSettings from "./servers/ServerAccessSettings";
import { useServerFrame } from "./servers/frameBridge";
import {
  serverViewId,
  isServerView,
  isRemoteServerView,
  serverParentOrigin,
} from "./servers/environment";
import { serverStorageEventKey } from "./servers/storage";
import { serverLocalStorage as localStorage } from "./servers/storage";
import SearchOverlay from "./components/shell/SearchOverlay";
import { SettingsRow } from "./components/ui/primitives";
import { modalSizes } from "./theme";
import { menuActions, renameCommand, studioCommand } from "./nativeCommands";
import { useDesktopNotifications } from "./hooks/desktopNotifications";
import { useNativeAction } from "./useNativeAction";
import { useChatPrefetch } from "./hooks/chatPrefetch";
import { useTeamTokenRateStream } from "./hooks/useTeamTokenRateStream";
import {
  accountLimits,
  limitsReadSucceeded,
  limitsSnapshotIsFresh,
  shouldReplaceLimitsSnapshot,
} from "./usage/accountUsage";
import { usageAccountConnectionKey as usageConnectionKey } from "./usage/usageAccountRefresh";
import type { AccountLimitsSnapshot } from "./usage/accountUsage";
import { useMobileViewport } from "./hooks/mobileViewport";
import { roomLeadIds, messageAttentionCount } from "./chatScope";
import { nativeThreadError } from "./nativeErrors";
import {
  pendingChatCreations,
  saveChatCreation,
  confirmChatCreation,
} from "./chatCreation";
import {
  ActionIcon,
  Button,
  Drawer,
  Modal,
  Menu,
  NativeSelect,
  Slider,
  Tabs,
  useMantineColorScheme,
  TextInput,
  UnstyledButton,
} from "@mantine/core";
import { useMediaQuery } from "@mantine/hooks";
import {
  Activity,
  BookOpen,
  Clock3,
  FileDiff,
  Minimize2,
  MoreHorizontal,
  ShieldCheck,
  Settings,
  Square,
  ArrowLeft,
  Folder,
  MessageSquare,
  Mail,
  PanelLeft,
  Plus,
  Search,
  Settings2,
  Users,
  X,
} from "lucide-react";
import {
  useCallback,
  useMemo,
  useEffect,
  lazy,
  useRef,
  useState,
  memo,
  Suspense,
  type ReactNode,
} from "react";
import {
  get,
  post,
  ApiError,
  errorText,
  save,
  saved,
  type PostBody,
  isApiSchemaMismatch,
  onApiSchemaMismatch,
  schemaUpdateFailedAfterReload,
  updateRendererAndReload,
} from "./api";
import {
  useSnapshot,
  useChatSnapshot,
  useCommittedCallback,
  useRetainedArray,
} from "./hooks";
import { removeAllSendingMessages } from "./components/removeSendingMessages";
import type { Attachment } from "./components/ComposerAttachments";
import {
  defaultStudioPreferences,
  fontFamilies,
  formatSidebarShortcut,
  parseSidebarShortcut,
  parseStudioPreferences,
  studioPreferencesStorageKey,
  type StudioPreferences,
} from "./studioPreferences";
import {
  acknowledgeOutbox,
  editOutboxDisplay,
  durableSend,
  useOutbox,
  type OutgoingMessage,
} from "./sync/send";
import { useSyncedDrafts } from "./sync/drafts";
import { reportPromptComposerRender } from "./components/prompt-composer/renderProbe";
import {
  busy,
  nativeReleaseLabel,
  statusLabel,
  type Agent,
  type Json,
} from "./types";

type LeadCreateRequest = Omit<
  PostBody<"/api/leads">,
  "id" | "previous" | "reuse_empty" | "cwd" | "project_folder" | "account_key"
> & {
  id: string;
  previous: string | null;
  reuse_empty: boolean;
  cwd?: string;
  project_folder?: string;
  account_key?: string;
};
function isLeadCreateRequest(value: Json): value is LeadCreateRequest {
  return (
    typeof value.id === "string" &&
    !!value.id &&
    (value.previous === null || typeof value.previous === "string") &&
    typeof value.reuse_empty === "boolean" &&
    (value.cwd === undefined || typeof value.cwd === "string") &&
    (value.project_folder === undefined ||
      typeof value.project_folder === "string") &&
    (value.account_key === undefined || typeof value.account_key === "string")
  );
}
import Sidebar from "./components/Sidebar";
import { createAppCatalogSelector } from "./components/sidebar/catalog";
const emptyAgents: Agent[] = [];
import {
  unreadResult,
  chatIndicators,
  backgroundActivities,
  chatActivities,
  hasCompletedResult,
} from "./components/chat-status/chatStatusModel";
import { useChatReadState } from "./components/useChatReadState";
import UIErrorBoundary from "./components/UIErrorBoundary";
import ProjectAccount from "./components/ProjectAccount";
import SessionActivity from "./components/agents/SessionActivity";
import { useWorkerModels } from "./components/agents/WorkerModelPicker";
import { UnifiedAgentSettings } from "./components/agents/UnifiedAgentSettings";
import { FederationSettings } from "./components/FederationSettings";
import { LinuxVMSettings } from "./components/LinuxVMSettings";
import BrowserAccessNotice from "./components/BrowserAccessNotice";
import SupervisorRecoveryNotice from "./components/SupervisorRecoveryNotice";
import Accounts, { useAccounts } from "./components/Accounts";
import ClaudeSignIn from "./components/ClaudeSignIn";
import AccountSignInNotice from "./components/AccountSignInNotice";
import CodexSignIn from "./components/CodexSignIn";
import ConversationTitle from "./components/shell/ConversationTitle";
import SubagentConcurrencyControl from "./components/agents/SubagentConcurrencyControl";
import Conversation from "./components/Conversation";
const SelectedConversation = memo(Conversation);
import type { UsageAccount } from "./components/Usage";
import { watchResourceReads } from "./components/watchResourceReads";
import {
  limitsBaselineReader,
  type LimitsBaselineHydration,
} from "./usage/limitsBaseline";
import RadioChat from "./components/RadioChat";
import SharedChatCreate, {
  sharedCreationKey,
} from "./components/SharedChatCreate";
import ProjectDirectoryPicker from "./components/ProjectDirectoryPicker";
import "./desktop";
import "./components/team-navigation.css";
import WorkerCard from "./components/agents/WorkerCard";
import FilePreview, { type PreviewTarget } from "./components/FilePreview";
import {
  awaitingAnswerIds,
  TeamSummary,
  workerState,
  TEAM_PANEL_STATES,
  TeamModels,
} from "./components/agents/WorkerOverview";
import { activeTask, backgroundTasks } from "./components/backgroundTaskModel";
import { watchResourceChanges } from "./sync/resourceEvents";
const ClaudeSettings = lazy(() =>
  import("./components/ClaudeSettings").then((module) => ({
    default: module.ClaudeSettings,
  })),
);
const TerminalDock = lazy(() => import("./components/TerminalDock"));
const Workspace = lazy(() => import("./components/shell/Workspace"));
const BackgroundTasks = lazy(
  () => import("./components/shell/BackgroundTasks"),
);
export default function App() {
  reportPromptComposerRender("app");
  const [schemaMismatch, setSchemaMismatch] = useState(false);
  const [schemaMismatchNeedsRebuild, setSchemaMismatchNeedsRebuild] =
    useState(false);
  const [schemaUpdateError, setSchemaUpdateError] = useState("");
  useEffect(() => {
    let shown = false;
    const showMismatch = () => {
      if (shown) return;
      shown = true;
      setSchemaMismatchNeedsRebuild(schemaUpdateFailedAfterReload());
      setSchemaMismatch(true);
    };
    return onApiSchemaMismatch(showMismatch);
  }, []);
  const outbox = useOutbox();
  const [removingAllSending, setRemovingAllSending] = useState(false);
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
  const outgoingMessages = useMemo(() => {
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
    return [...visibleSends.values()].filter(
      (entry) => !observedSends.current.has(entry.id),
    );
  }, [outbox.entries, outgoing]);
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
  const [studioSettingsOpen, setStudioSettingsOpen] = useState(false);
  const [studioSettingsTab, setStudioSettingsTab] =
    useState<StudioSettingsTab>("accounts");
  const serverSettings = useServerSettings();
  useEffect(() => {
    if (studioSettingsOpen && studioSettingsTab === "servers")
      serverSettings?.activate();
  }, [studioSettingsOpen, studioSettingsTab]);
  const activateSettingsTab = (value: string | null) => {
    if (!isStudioSettingsTab(value)) return;
    setStudioSettingsTab(value);
    if (value === "servers") {
      if (serverSettings) serverSettings.activate();
      else if (serverViewId && window.parent !== window) {
        setStudioSettingsOpen(false);
        window.parent.postMessage(
          { kind: "studio-server-open-settings", serverId: serverViewId },
          serverParentOrigin,
        );
      }
    }
  };
  const chatActionsButton = useRef<HTMLButtonElement>(null);
  const [accountModalOpen, setAccountModalOpen] = useState(false);
  const [claudeLoginKey, setClaudeLoginKey] = useState("");
  const [codexLoginKey, setCodexLoginKey] = useState("");
  const [mainSettingsOpen, setMainSettingsOpen] = useState(false);
  const [workerSettingsOpen, setWorkerSettingsOpen] = useState(false);
  const [reviewSettingsOpen, setReviewSettingsOpen] = useState(false);
  const [filePreview, setFilePreview] = useState<PreviewTarget | null>(null);
  const { colorScheme, setColorScheme } = useMantineColorScheme();
  const preferenceLoad = useMemo(() => {
    try {
      const stored = localStorage.getItem(studioPreferencesStorageKey);
      if (!stored)
        return {
          value: { ...defaultStudioPreferences, theme: colorScheme },
          error: "",
        };
      try {
        return { value: parseStudioPreferences(stored), error: "" };
      } catch {
        return {
          value: { ...defaultStudioPreferences, theme: colorScheme },
          error:
            "Saved Studio preferences were invalid; safe defaults are active.",
        };
      }
    } catch {
      return {
        value: { ...defaultStudioPreferences, theme: colorScheme },
        error:
          "Studio preferences could not be read. Changes may not persist in this browser.",
      };
    }
  }, []);
  const [studioPreferences, setStudioPreferences] = useState<StudioPreferences>(
    preferenceLoad.value,
  );
  const [studioPreferencesError, setStudioPreferencesError] = useState(
    preferenceLoad.error,
  );
  const [sidebarShortcutError, setSidebarShortcutError] = useState("");
  const updateStudioPreferences = useCallback(
    (next: StudioPreferences) => {
      setStudioPreferences(next);
      if (next.theme !== colorScheme) setColorScheme(next.theme);
      try {
        localStorage.setItem(studioPreferencesStorageKey, JSON.stringify(next));
        if (isServerView)
          window.parent.postMessage(
            { kind: "studio-server-preferences", preferences: next },
            serverParentOrigin,
          );
        setStudioPreferencesError("");
      } catch {
        setStudioPreferencesError(
          "Studio preferences could not be saved in this browser.",
        );
      }
    },
    [colorScheme, setColorScheme],
  );
  const [sidebarCollapsed, setSidebarCollapsed] = useState(() =>
    saved("codex-sidebar-collapsed", false),
  );
  useEffect(() => {
    if (studioPreferences.theme !== colorScheme)
      setColorScheme(studioPreferences.theme);
  }, [studioPreferences.theme, colorScheme, setColorScheme]);
  useEffect(() => {
    const root = document.documentElement;
    root.dataset.studioTypography = studioPreferences.typography;
    root.dataset.studioContentLayout = studioPreferences.contentLayout;
    root.style.setProperty(
      "--studio-font-family",
      fontFamilies[studioPreferences.fontFamily].css,
    );
    root.style.setProperty(
      "--studio-sidebar-font-size",
      `${studioPreferences.sidebarFontSize}px`,
    );
    root.style.setProperty(
      "--studio-main-font-size",
      `${studioPreferences.mainFontSize}px`,
    );
    if (studioPreferences.typography === "original") {
      root.style.removeProperty("--studio-font-family");
      root.style.removeProperty("--studio-sidebar-font-size");
      root.style.removeProperty("--studio-main-font-size");
    }
    root.style.setProperty(
      "--studio-content-width-ratio",
      String(studioPreferences.contentWidth / 100),
    );
  }, [studioPreferences]);
  useEffect(() => {
    const preferences = (event: StorageEvent) => {
      if (event.key !== studioPreferencesStorageKey || !event.newValue) return;
      try {
        setStudioPreferences(parseStudioPreferences(event.newValue));
      } catch {}
    };
    window.addEventListener("storage", preferences);
    return () => window.removeEventListener("storage", preferences);
  }, []);
  const toggleSidebar = useCallback(() => {
    if (isServerView) {
      window.parent.postMessage(
        { kind: "studio-server-toggle-sidebar" },
        serverParentOrigin,
      );
      return;
    }
    if (mobileClient) {
      setSidebar((visible) => !visible);
      return;
    }
    setSidebarCollapsed((collapsed) => {
      save("codex-sidebar-collapsed", !collapsed);
      return !collapsed;
    });
  }, [mobileClient]);
  useEffect(() => {
    const shortcut = parseSidebarShortcut(studioPreferences.sidebarShortcut);
    if (!shortcut) return;
    const onKeyDown = (event: KeyboardEvent) => {
      const target = event.target;
      if (
        target instanceof HTMLElement &&
        (target.isContentEditable ||
          target.closest("input, textarea, select, [role='textbox']"))
      )
        return;
      if (
        event.key.toLowerCase() !== shortcut.key ||
        event.metaKey !== shortcut.meta ||
        event.ctrlKey !== shortcut.ctrl ||
        event.altKey !== shortcut.alt ||
        event.shiftKey !== shortcut.shift
      )
        return;
      event.preventDefault();
      toggleSidebar();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [studioPreferences.sidebarShortcut, toggleSidebar]);
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
    roomId?: string;
  }>();
  const [taskFocus, setTaskFocus] = useState<{
    id: string;
    leadId: string;
    requestId: string;
  }>();
  const [limitsLoading, setLimitsLoading] = useState<Record<string, boolean>>(
    {},
  );
  const selectionScope = useRef("");
  const navigationIntent = useRef(0);
  const createdSelection = useRef<string | null>(null);
  useMobileViewport(mobileClient);
  const narrowTeam = useMediaQuery("(max-width: 1199px)");
  const {
      data,
      error,
      refresh,
      workspaceId,
      rememberCreated,
      forgetCreated,
      creationScope,
    } = useSnapshot(),
    [opened, setOpened] = useState<string | null>(() =>
      window.matchMedia("(max-width: 760px)").matches
        ? saved("codex-mobile-opened", null)
        : null,
    ),
    [roomContext, setRoomContext] = useState<string | null>(null),
    [sidebar, setSidebar] = useState(false),
    [teamOpen, setTeamOpen] = useState(false),
    [tasksOpen, setTasksOpen] = useState(false),
    [searchOpen, setSearchOpen] = useState(false),
    [workspaceOpen, setWorkspaceOpen] = useState(false),
    [tasksRendered, setTasksRendered] = useState(false),
    [workspaceRendered, setWorkspaceRendered] = useState(false),
    [workspaceSection, setWorkspaceSection] = useState("messages"),
    [workerQuery, setWorkerQuery] = useState(""),
    [workerFilter, setWorkerFilter] = useState("all"),
    [completedOpen, setCompletedOpen] = useState(false),
    [sharedCreate, setSharedCreate] = useState<{ path?: string } | null>(null),
    [creating, setCreating] = useState(false),
    [sending, setSending] = useState(false),
    [toast, setToast] = useState(""),
    [modal, setModal] = useState<{ title: string; body: ReactNode } | null>(
      null,
    ),
    [limitsByAccount, setLimitsByAccount] = useState<
      Record<string, AccountLimitsSnapshot>
    >({}),
    [limitsCacheScope, setLimitsCacheScope] = useState<string | null>(null);
  useEffect(() => {
    if (mobileClient || !data?.stateDir) return;
    // Include terminals in the first typed subscription set; TerminalDock can
    // mount later, and its watcher then shares this resource without a reopen.
    return watchResourceChanges({ kind: "terminals" }, () => {});
  }, [mobileClient, data?.stateDir]);
  useEffect(() => {
    if (!data?.stateDir) return;
    setLimitsByAccount(saved(`codex-limits:${data.stateDir}`, {}));
    setLimitsCacheScope(data.stateDir);
  }, [data?.stateDir]);
  useEffect(() => {
    if (!limitsCacheScope || limitsCacheScope !== data?.stateDir) return;
    const good = Object.fromEntries(
      Object.entries(limitsByAccount).filter(
        ([, value]) => value?.data && value?.at,
      ),
    );
    save(`codex-limits:${limitsCacheScope}`, good);
  }, [limitsByAccount, limitsCacheScope, data?.stateDir]);
  useEffect(() => {
    if (data?.stateDir && saved(sharedCreationKey(data.stateDir), null))
      setSharedCreate({});
  }, [data?.stateDir]);
  const receipts = useMemo(
    () =>
      new Map((data?.runtime?.events || []).map((event) => [event.id, event])),
    [data?.runtime?.events],
  );
  const prepareChat = useChatPrefetch(data, opened, workspaceId);
  useEffect(() => {
    if (tasksOpen) setTasksRendered(true);
    if (workspaceOpen) setWorkspaceRendered(true);
  }, [tasksOpen, workspaceOpen]);
  const cancelledSends = outgoingMessages
    .filter((entry) => receipts.get(entry.id)?.status === "cancelled")
    .map((entry) => entry.id)
    .join(",");
  useEffect(() => {
    if (cancelledSends) observeSends(cancelledSends.split(","));
  }, [cancelledSends, observeSends]);
  const visibleOutgoing = useMemo(
    () =>
      outgoingMessages
        .filter((entry) => receipts.get(entry.id)?.status !== "cancelled")
        .map((entry) => {
          const receipt = receipts.get(entry.id);
          if (receipt?.status === "failed")
            return {
              ...entry,
              status: "failed",
              error: receipt.error || undefined,
            } satisfies OutgoingMessage;
          if (receipt?.status === "uncertain")
            return {
              ...entry,
              status: "uncertain",
              error: receipt.error || undefined,
            } satisfies OutgoingMessage;
          return entry;
        }),
    [outgoingMessages, receipts],
  );
  const conversationOutgoing = useRetainedArray(
    visibleOutgoing.filter((entry) => entry.body.room === opened),
  );
  const {
    setDrafts,
    getDraft,
    subscribeDraft,
    conflicts: draftConflicts,
    dismissDraft,
    error: draftError,
    localPersistenceFailed,
  } = useSyncedDrafts();
  const conversationDraftConflicts = useRetainedArray(
    draftConflicts.filter((version) => version.session === (opened || "new")),
  );
  useServerActivity(localPersistenceFailed || sending);
  const [pendingCreations, setPendingCreations] = useState<Json[]>([]);
  const creationKey = `codex-pending-creation:${data?.stateDir || ""}`;
  const creation = useRef<LeadCreateRequest | null>(null),
    sends = useRef<Record<string, PostBody<"/api/messages">>>({}),
    sendingLock = useRef<symbol | null>(null),
    latestSend = useRef<Record<string, symbol>>({}),
    creationLock = useRef(false),
    renameRequests = useRef<Record<string, { text: string; id: string }>>({}),
    toastTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const pendingSendKey = `studio-pending-sends:${data?.stateDir || ""}`;
  const pendingRenameKey = `studio-pending-renames:${data?.stateDir || ""}`;
  useEffect(() => {
    if (data?.stateDir) sends.current = saved(pendingSendKey, {});
  }, [pendingSendKey, data?.stateDir]);
  useEffect(() => {
    if (data?.stateDir) renameRequests.current = saved(pendingRenameKey, {});
  }, [pendingRenameKey, data?.stateDir]);
  const persistSends = () => save(pendingSendKey, sends.current);
  useEffect(() => {
    if (!data?.stateDir) return;
    const load = () => {
      try {
        setPendingCreations(pendingChatCreations(creationKey));
      } catch (error) {
        setToast("Cannot read the saved chat requests: " + errorText(error));
      }
    };
    load();
    const stored = (event: StorageEvent) => {
      if (
        serverStorageEventKey(event) === null ||
        serverStorageEventKey(event) === creationKey ||
        serverStorageEventKey(event)?.startsWith(`${creationKey}:request:`)
      ) {
        load();
      }
    };
    window.addEventListener("storage", stored);
    return () => window.removeEventListener("storage", stored);
  }, [creationKey, data?.stateDir]);
  const notify = useCallback((s: string) => {
    setToast(s);
    if (toastTimer.current) clearTimeout(toastTimer.current);
    toastTimer.current = setTimeout(() => setToast(""), 5000);
  }, []);
  const nativeActions = useNativeAction(data?.stateDir, workspaceId, notify);
  useDesktopNotifications(data, opened);
  const submitNativeAction = async (
    target: Agent,
    action: "compact" | "review",
  ) => {
    const response = await nativeActions.submit(target, action);
    setDrafts((old) => {
      if (old[target.id]?.trim() !== `/${action}`) return old;
      const next = { ...old };
      delete next[target.id];
      return next;
    });
    return response;
  };
  const appCatalogSelector = useMemo(createAppCatalogSelector, []);
  const agents = data?.threads || emptyAgents;
  const catalog = appCatalogSelector(agents);
  const leads = catalog.leads;
  const agent = catalog.byId.get(opened || "");
  const room = data?.runtime?.rooms?.find((r) => r.id === opened);
  const legacy = data?.chats.find((c) => c.id === opened);
  const roomRoots = room ? roomLeadIds(room, agents) : [];
  const lead = room?.radio
    ? undefined
    : catalog.byId.get(
        agent?.rootId ||
          (agent?.isLead ? agent.id : undefined) ||
          (roomContext && roomRoots.includes(roomContext)
            ? roomContext
            : roomRoots[0]) ||
          "",
      );
  const team = (lead && catalog.team(lead.id)) || emptyAgents;
  const workers = (lead && catalog.workers(lead.id)) || emptyAgents;
  useTeamTokenRateStream(
    lead?.id,
    Boolean(workers.length && (narrowTeam ? teamOpen : wideTeamOpen)),
  );
  const readState = useChatReadState(
    data,
    opened,
    notify,
    refresh,
    workspaceId,
  );
  const activities = useMemo(
    () => (data ? chatActivities(data) : new Map()),
    [data?.threads, data?.runtime?.tasks, data?.runtime?.monitors],
  );
  const indicators = useMemo(
    () =>
      data
        ? chatIndicators(data, readState.readStateFor, activities)
        : new Map(),
    [
      data?.threads,
      data?.runtime?.requests,
      readState.readStateFor,
      activities,
    ],
  );
  useEffect(() => {
    setWorkerQuery("");
    setWorkerFilter("all");
    setCompletedOpen(false);
  }, [lead?.id]);
  useEffect(() => {
    if (agent && !agent.isLead && agent.status === "completed")
      setCompletedOpen(true);
  }, [agent?.id, agent?.status]);
  const accounts = useAccounts(data?.stateDir);
  const accountKey =
    agent?.accountKey ||
    lead?.accountKey ||
    (agent ? "default" : accounts.data.defaultAccountKey);
  const agentModels = useWorkerModels(
    accountKey,
    !!agent && agent.source === "managed",
  );
  const limitsRequests = useRef(new Map<string, Promise<boolean>>());
  const selectedAccount =
    accounts.data.accounts.find((a) => a.id === accountKey) ||
    accounts.data.archivedAccounts?.find((a) => a.id === accountKey);
  const accountId = selectedAccount?.accountId;
  const cachedLimits = accountLimits(
    limitsByAccount[accountKey],
    accountKey,
    accountId,
  );
  const snapshotLimits =
    data?.runtime?.rateLimitsByAccount?.[accountKey] ||
    (accountKey === "default" ? data?.runtime?.rateLimits : null);
  const matchingSnapshot = accountLimits(snapshotLimits, accountKey, accountId);
  const visibleLimits =
    matchingSnapshot &&
    (!cachedLimits || (matchingSnapshot.at || 0) > (cachedLimits.at || 0))
      ? matchingSnapshot
      : cachedLimits || null;
  const chatData = useChatSnapshot(data, lead?.id);
  const scopedConversationData = useChatSnapshot(data, lead?.id, true);
  const attentionCount = messageAttentionCount(chatData);
  const taskCount = backgroundTasks(chatData).filter(activeTask).length;
  const setDraft = useCallback(
    (text: string | ((current: string) => string), id = opened || "new") =>
      setDrafts((old) => {
        const next = {
          ...old,
          [id]: typeof text === "function" ? text(old[id] || "") : text,
        };
        return next;
      }),
    [opened, setDrafts],
  );
  const open = useCommittedCallback((id: string, messageId?: string) => {
    prepareChat(id);
    navigationIntent.current++;
    setJumpTarget(
      messageId
        ? { chatId: id, messageId, requestId: crypto.randomUUID() }
        : undefined,
    );
    setRoomContext(lead?.id || null);
    setOpened(id);
    setSidebar(false);
    setTeamOpen(false);
  });
  useEffect(() => {
    if (!data) return;
    if (
      agent?.sharedRoomId &&
      data.runtime?.rooms.some(
        (room) => room.id === agent.sharedRoomId && !room.userHidden,
      )
    ) {
      setOpened(agent.sharedRoomId);
      return;
    }
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
          data.runtime?.rooms.some((item) => item.id === selected) ||
          data.chats.some((item) => item.id === selected))
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
      setOpened(
        leads.at(-1)?.id ||
          data.runtime?.rooms
            .filter((room) => room.radio?.direct && !room.userHidden)
            .at(-1)?.id ||
          null,
      );
    else save(key, opened);
  }, [data, opened, agent, room, legacy, mobileClient]);
  useEffect(() => {
    const onError = (event: Event) =>
      notify((event as CustomEvent<string>).detail);
    window.addEventListener("desktop-error", onError);
    return () => window.removeEventListener("desktop-error", onError);
  }, [notify]);
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
      navigationIntent.current++;
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
      setWorkspaceOpen(!!navigationTarget.itemId);
    }
    setNavigationTarget(undefined);
  }, [navigationTarget, data, agents, notify]);
  const limitsCache = useRef(limitsByAccount);
  limitsCache.current = limitsByAccount;
  const limitsWatchers = useRef(new Map<string, () => void>());
  const limitsCacheHydrations = useRef(
    new Map<
      string,
      { token: object; promise: Promise<LimitsBaselineHydration> }
    >(),
  );
  const accountsForLimits = useRef(accounts.data.accounts);
  accountsForLimits.current = accounts.data.accounts;
  const reloadLimitsFor = useCallback(
    (key: string, force = false) => {
      const selected = accounts.data.accounts.find((item) => item.id === key);
      const selectedId = selected?.accountId;
      const pending = limitsRequests.current.get(key);
      if (pending) return pending;
      const cached = accountLimits(limitsCache.current[key], key, selectedId);
      if (
        !force &&
        cached?.data &&
        !cached.error &&
        Date.now() / 1000 - (cached.at || 0) < 60
      )
        return Promise.resolve(true);
      const requestStartedAt = Date.now() / 1000;
      setLimitsLoading((old) => ({ ...old, [key]: true }));
      const request = get("/api/limits", {
        query: key === "default" ? undefined : { account_key: key },
        timeoutMs: 25000,
      })
        .then((result) => {
          const responseSnapshot = accountLimits(result, key, selectedId);
          if (!responseSnapshot)
            throw new Error("Codex returned limits for another account.");
          const snapshot =
            responseSnapshot.data === null && limitsReadSucceeded(result)
              ? { ...responseSnapshot, at: requestStartedAt }
              : responseSnapshot;
          const currentSnapshot = accountLimits(
            limitsCache.current[key],
            key,
            selectedId,
          );
          if (shouldReplaceLimitsSnapshot(currentSnapshot, snapshot))
            limitsCache.current = {
              ...limitsCache.current,
              [key]: { ...snapshot, accountKey: key },
            };
          setLimitsByAccount((old) => {
            const previous = accountLimits(old[key], key, selectedId);
            if (!shouldReplaceLimitsSnapshot(previous, snapshot)) return old;
            return { ...old, [key]: { ...snapshot, accountKey: key } };
          });
          return limitsReadSucceeded(result);
        })
        .catch((error) => {
          limitsCache.current = {
            ...limitsCache.current,
            [key]: {
              ...(limitsCache.current[key] || { data: null }),
              error: errorText(error),
              accountKey: key,
            },
          };
          setLimitsByAccount((old) => ({
            ...old,
            [key]: {
              ...(old[key] || { data: null }),
              error: errorText(error),
              accountKey: key,
            },
          }));
          return false;
        })
        .finally(() => {
          limitsRequests.current.delete(key);
          setLimitsLoading((old) => ({ ...old, [key]: false }));
        });
      limitsRequests.current.set(key, request);
      return request;
    },
    [accounts.data.accounts],
  );
  const reloadLimits = useCallback(
    async (force = false) => {
      await reloadLimitsFor(accountKey, force);
    },
    [accountKey, reloadLimitsFor],
  );
  const forceReloadLimits = useCallback(
    () => reloadLimits(true),
    [reloadLimits],
  );
  const reloadLimitsForRef = useRef(reloadLimitsFor);
  reloadLimitsForRef.current = reloadLimitsFor;
  const usageTeamIndicators = team
    .map((item) => indicators.get(item.id)?.kind || "")
    .join(",");
  const usageAccounts = useMemo<UsageAccount[]>(() => {
    if (!agent) return [];
    const rootId = agent.rootId || agent.id;
    // The lead's current account and the accounts of working subagents take
    // part in this chat. Finished workers keep an account the chat left.
    const teamAgents = team.filter(
      (item) =>
        !item.deletedAt &&
        (item.id === rootId ||
          (item.rootId === rootId &&
            !item.archived &&
            (item.inFlight ||
              ["running", "queued", "approval", "starting"].includes(
                item.status ?? "",
              ) ||
              (["waiting", "parked"].includes(item.status ?? "") &&
                ["answer", "working"].includes(
                  indicators.get(item.id)?.kind || "",
                ))))),
    );
    const keys = new Set(
      teamAgents.map((item) => item.accountKey || "default"),
    );
    keys.add(agent.accountKey || "default");
    const labelFor = (key: string) => {
      const account = accountsForLimits.current.find((item) => item.id === key);
      return account?.email || account?.label || key;
    };
    return [...keys]
      .sort((a, b) =>
        a === (agent.accountKey || "default")
          ? -1
          : b === (agent.accountKey || "default")
            ? 1
            : labelFor(a).localeCompare(labelFor(b)),
      )
      .map((key) => {
        const account = accounts.data.accounts.find((item) => item.id === key);
        const cached = accountLimits(
          limitsByAccount[key],
          key,
          account?.accountId,
        );
        const snapshot =
          data?.runtime?.rateLimitsByAccount?.[key] ||
          (key === "default" ? data?.runtime?.rateLimits : null);
        const matching = accountLimits(snapshot, key, account?.accountId);
        const limits =
          matching && (!cached || (matching.at || 0) > (cached.at || 0))
            ? matching
            : cached || null;
        return {
          key,
          label: account?.label || key,
          email: account?.email,
          provider: account?.provider ?? undefined,
          accountId: account?.accountId,
          signedOut: !!account?.disconnected,
          limits,
          loading: !!limitsLoading[key],
          reload: async (force = false) => {
            await reloadLimitsFor(key, force);
          },
        };
      });
  }, [
    agent,
    team,
    usageTeamIndicators,
    accounts.data.accounts,
    limitsByAccount,
    data?.runtime?.rateLimitsByAccount,
    data?.runtime?.rateLimits,
    limitsLoading,
    reloadLimitsFor,
  ]);
  // Only accounts that take part in this chat team get a dot and a cache read.
  const usageAccountKeys = usageAccounts.map((item) => item.key).join("\n");
  const usageAccountConnectionKey = usageConnectionKey(
    usageAccountKeys,
    accounts.data.accounts,
  );
  useEffect(() => {
    if (!data?.stateDir || !usageAccountKeys) return;
    for (const key of usageAccountKeys.split("\n")) {
      const account = accountsForLimits.current.find((item) => item.id === key);
      if (account?.disconnected) {
        limitsCacheHydrations.current.delete(key);
        continue;
      }
      if (limitsCacheHydrations.current.has(key)) continue;
      const token = {};
      const hydration = get("/api/limits", {
        query: { account_key: key, cached: "1" },
      })
        .then((result) => {
          if (limitsCacheHydrations.current.get(key)?.token !== token)
            return false;
          const currentAccount = accountsForLimits.current.find(
            (item) => item.id === key,
          );
          const snapshot = accountLimits(
            result,
            key,
            currentAccount?.accountId,
          );
          if (
            currentAccount?.disconnected ||
            !snapshot ||
            !limitsSnapshotIsFresh(snapshot, key, currentAccount?.accountId)
          )
            return false;
          const previousCurrent = accountLimits(
            limitsCache.current[key],
            key,
            currentAccount?.accountId,
          );
          if (
            !previousCurrent ||
            (previousCurrent.at || 0) < (snapshot.at || 0)
          )
            limitsCache.current = {
              ...limitsCache.current,
              [key]: { ...snapshot, accountKey: key },
            };
          setLimitsByAccount((old) => {
            const previous = accountLimits(
              old[key],
              key,
              currentAccount?.accountId,
            );
            if (previous && (previous.at || 0) >= (snapshot.at || 0))
              return old;
            return { ...old, [key]: snapshot };
          });
          return { snapshot };
        })
        .catch(() => false);
      limitsCacheHydrations.current.set(key, { token, promise: hydration });
    }
    // Account metadata validates this key through the ref above; the request
    // identity depends only on workspace + usageAccountKeys. Replacing the
    // accounts array during its initial load must not repeat the same read.
  }, [data?.stateDir, usageAccountKeys, usageAccountConnectionKey]);
  useEffect(() => {
    if (!data?.stateDir) return;
    return () => {
      for (const stop of limitsWatchers.current.values()) stop();
      limitsWatchers.current.clear();
      limitsCacheHydrations.current.clear();
    };
  }, [data?.stateDir]);
  useEffect(() => {
    if (!data?.stateDir) return;
    const keys = new Set([
      accountKey,
      ...usageAccountKeys.split("\n").filter(Boolean),
    ]);
    for (const [key, stop] of limitsWatchers.current) {
      if (keys.has(key)) continue;
      stop();
      limitsWatchers.current.delete(key);
    }
    for (const key of keys) {
      if (limitsWatchers.current.has(key)) continue;
      let active = true;
      const stopWatching = watchResourceReads(
        { kind: "limits", accountKey: key },
        limitsBaselineReader(
          () =>
            limitsCacheHydrations.current.get(key)?.promise ??
            Promise.resolve(false),
          // A same-version reconnect baseline is covered by hydration; a newer
          // resource version still forces a read despite a fresh cache entry.
          () => reloadLimitsForRef.current(key, true),
          () => {
            const account = accountsForLimits.current.find(
              (item) => item.id === key,
            );
            return active && !account?.disconnected;
          },
          (snapshot) => {
            const account = accountsForLimits.current.find(
              (item) => item.id === key,
            );
            return limitsSnapshotIsFresh(
              snapshot ?? limitsCache.current[key],
              key,
              account?.accountId,
            );
          },
        ),
        () => {
          // reloadLimitsFor stores errors in visible account state.
        },
      );
      const stop = () => {
        active = false;
        stopWatching();
      };
      limitsWatchers.current.set(key, stop);
    }
  }, [data?.stateDir, accountKey, usageAccountKeys]);
  useEffect(() => {
    // Keep each account's latest snapshot for immediate return navigation.
    const incoming = { ...data?.runtime?.rateLimitsByAccount };
    if (data?.runtime?.rateLimits && !incoming.default)
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
  }, [data?.runtime?.rateLimits, data?.runtime?.rateLimitsByAccount]);
  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key === "k") {
        e.preventDefault();
        if (isServerView) {
          window.parent.postMessage(
            { kind: "studio-server-search" },
            serverParentOrigin,
          );
          return;
        }
        setSidebar(false);
        setSearchOpen(true);
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
  const newChat = async (
    cwd?: string,
    projectFolder?: string,
    retryId?: string,
  ) => {
    if (creationLock.current) return null;
    if (!retryId && !cwd) {
      cwd =
        (lead?.cwd ?? undefined) ||
        (mobileClient
          ? (data?.runtime?.projects?.[0]?.path ?? undefined) ||
            leads.find((item) => item.cwd)?.cwd ||
            undefined
          : undefined);
      projectFolder = lead?.projectFolder || undefined;
    }
    try {
      const pending = pendingChatCreations(creationKey);
      creation.current =
        (retryId
          ? pending.find(
              (request): request is LeadCreateRequest =>
                isLeadCreateRequest(request) && request.id === retryId,
            )
          : pending.find(
              (request): request is LeadCreateRequest =>
                isLeadCreateRequest(request) &&
                (request.cwd ||
                  leads.find((item) => item.id === request.previous)?.cwd) ===
                  cwd &&
                (request.project_folder || undefined) === projectFolder,
            )) || null;
      setPendingCreations(pending);
      if (retryId && !creation.current)
        throw new Error("The saved chat request is no longer available.");
    } catch (error) {
      notify("Cannot read the saved chat request: " + errorText(error));
      return null;
    }
    if (creation.current) cwd = creation.current.cwd;
    if (mobileClient && !cwd) {
      cwd =
        (lead?.cwd ?? undefined) ||
        (data?.runtime?.projects?.[0]?.path ?? undefined) ||
        (leads.find((item) => item.cwd)?.cwd ?? undefined);
      if (!cwd) {
        setSidebar(false);
        setModal({
          title: "Choose project folder",
          body: (
            <ProjectDirectoryPicker
              onSelect={async (path) => {
                await post("/api/projects", { path });
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
    const selectionIntent = navigationIntent.current;
    const selectedScope = selectionScope.current;
    setCreating(true);
    creation.current ||= {
      id: crypto.randomUUID(),
      previous: lead?.id || null,
      reuse_empty: false,
      ...(cwd ? { cwd } : {}),
      ...(projectFolder ? { project_folder: projectFolder } : {}),
      ...(cwd &&
      data?.runtime?.projects?.find((project) => project.path === cwd)
        ?.accountKey
        ? {
            account_key:
              data.runtime?.projects?.find((project) => project.path === cwd)
                ?.accountKey ?? undefined,
          }
        : {}),
    };
    const creationRequest = creation.current;
    if (!creationRequest) {
      creationLock.current = false;
      setCreating(false);
      return null;
    }
    if (!opened && getDraft("new"))
      setDraft(getDraft("new"), creationRequest.id);
    try {
      // Save the exact request before sending it. A lost response must retain this identity.
      saveChatCreation(creationKey, creationRequest);
      setPendingCreations(pendingChatCreations(creationKey));
      const a = await post("/api/leads", creationRequest, {
        timeoutMs: 15000,
      });
      if (a.id !== creationRequest.id)
        throw new Error("The server returned another chat identity.");
      rememberCreated(a, creationScope);
      confirmChatCreation(creationKey, creationRequest.id);
      setPendingCreations(pendingChatCreations(creationKey));
      if (!opened && getDraft("new")) setDraft(getDraft("new"), a.id);
      setToast("");
      creation.current = null;
      if (
        navigationIntent.current === selectionIntent &&
        selectionScope.current === selectedScope
      ) {
        createdSelection.current = a.id;
        setOpened(a.id);
        setSidebar(false);
        setTeamOpen(false);
      }
      void refresh();
      return a.id;
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
    attachments?: Attachment[];
    onPersist?: () => void | Promise<void>;
    delivery?: "after_tool" | "after_turn";
  }) => {
    if (schemaMismatch || isApiSchemaMismatch()) return;
    const draftKey = opened || "new",
      text = getDraft(draftKey).trim();
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
    let request: PostBody<"/api/messages"> | undefined;
    try {
      if (renameCommand(text) !== undefined && !opened)
        throw new Error("Open a chat before using /rename.");
      const id = opened || (await newChat());
      if (!id) return;
      latestSend.current[id] = attempt;
      if (
        agent?.source === "managed" &&
        studioCommand(text, agent.provider ?? undefined)
      ) {
        const [command] = text.split(/\s+/);
        if (options?.assets?.length)
          throw new Error(
            "Commands cannot include files. Remove the attachments or send a normal message.",
          );
        if (command !== "/rename" && text !== command)
          throw new Error(
            "Use the command without additional text, or send a normal message.",
          );
        if (command === "/rename") {
          const name = renameCommand(text);
          if (name && name.length > 80)
            throw new Error("A name must have 1 to 80 characters.");
          if (renameRequests.current[id]?.text !== text) {
            renameRequests.current[id] = { text, id: crypto.randomUUID() };
            save(pendingRenameKey, renameRequests.current);
          }
          const request_id = renameRequests.current[id].id;
          const body = { id, name, request_id };
          const result = await post("/api/rename", body);
          const finishRename = async (receipt: typeof result, attempts = 0) => {
            if (receipt.status === "pending") {
              if (attempts >= 90) {
                notify(
                  "The rename outcome is unknown. Use /rename again to check this request.",
                );
                return;
              }
              window.setTimeout(async () => {
                try {
                  await finishRename(
                    await post("/api/rename", body),
                    attempts + 1,
                  );
                } catch (error) {
                  notify(errorText(error));
                }
              }, 1000);
              return;
            }
            if (receipt.status === "failed") {
              delete renameRequests.current[id];
              save(pendingRenameKey, renameRequests.current);
              notify(receipt.error || "The title could not be generated.");
              return;
            }
            delete renameRequests.current[id];
            save(pendingRenameKey, renameRequests.current);
            await refresh();
          };
          void finishRename(result);
        } else if (command === "/stop" || command === "/stop-team")
          await post("/api/stop", {
            id: command === "/stop-team" ? (agent.rootId ?? id) : id,
            descendants: command === "/stop-team",
          });
        else
          await submitNativeAction(
            agent,
            command.slice(1) as "compact" | "review",
          );
      } else {
        if (
          sends.current[id]?.text !== text ||
          JSON.stringify(sends.current[id]?.assets || []) !==
            JSON.stringify(options?.assets || []) ||
          sends.current[id]?.delivery !== (options?.delivery || "after_tool")
        )
          sends.current[id] = {
            id: crypto.randomUUID(),
            room: id,
            text,
            assets: options?.assets || [],
            // Enter delivers after the current tool call; Tab after the turn.
            delivery: options?.delivery || "after_tool",
          };
        // Retain the exact request until the durable outbox owns its retry.
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
            ...(result.kind === "server" ? { receipt: result } : {}),
            error: result.error || undefined,
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
        if (
          result.kind === "server" &&
          Object.values(result.deliveries || {}).some((v) => v !== "queued")
        )
          notify("Message saved. Some deliveries are not confirmed.");
      }
      if (!request)
        setDrafts((old) => {
          const next = { ...old };
          if (next[draftKey]?.trim() === text) delete next[draftKey];
          if (next[id]?.trim() === text) delete next[id];
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
  const sendToConversation = useCommittedCallback(send);
  const branchCreated = useCommittedCallback((id: string) => {
    navigationIntent.current++;
    createdSelection.current = id;
    setOpened(id);
    setSidebar(false);
    setTeamOpen(false);
  });
  const newConversation = useCommittedCallback(
    () => void newChat(agent?.cwd || lead?.cwd || undefined),
  );
  const chooseConversation = useCallback(() => {
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
  }, []);
  const rename = async (id: string, name: string) => {
    try {
      await post("/api/rename", { id, name, request_id: crypto.randomUUID() });
      await refresh();
    } catch (e) {
      notify(errorText(e));
      throw e;
    }
  };
  const remove = (id: string, isRoom: boolean, isWorker = false) =>
    setModal({
      title: isWorker ? "Delete?" : "Delete chat?",
      body: (
        <>
          <p>
            {isRoom
              ? "Hide this chat. It returns on a new message."
              : "Stop this agent and its workers, and remove their chats. Files and history stay on disk."}
          </p>
          <Button
            color="red"
            variant="filled"
            data-delete-chat={id}
            onClick={() =>
              void run(async () => {
                const r = await post(
                  isRoom ? "/api/room/delete" : "/api/conversation/delete",
                  { id },
                );
                const deleted =
                  typeof r.deleted === "string"
                    ? [r.deleted]
                    : (r.deleted || []).filter(
                        (value): value is string => typeof value === "string",
                      );
                setModal(null);
                if (!isRoom) forgetCreated(deleted);
                if (opened && deleted.includes(opened)) {
                  navigationIntent.current++;
                  setOpened(null);
                }
                setDrafts((old) => {
                  const next = { ...old };
                  for (const key of deleted) delete next[key];
                  return next;
                });
              })
            }
          >
            {isWorker ? "Delete" : "Delete chat"}
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
          initialPath={target.cwd ?? undefined}
          onSelect={async (cwd) => {
            await post("/api/conversation", { id: target.id, cwd });
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
            <p>To use another folder, start a new chat.</p>
            {window.codexDesktop && !isRemoteServerView && (
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
  const schemaMismatchDialog = (
    <Modal
      opened={schemaMismatch}
      onClose={() => {}}
      title="Studio update required"
      closeOnEscape={false}
      closeOnClickOutside={false}
      withCloseButton={false}
      role="alertdialog"
      aria-label="Studio update required"
    >
      <p>
        {schemaMismatchNeedsRebuild
          ? "The installed renderer build does not match the server. Rebuild Studio before continuing."
          : "Studio has been updated. Update this tab to continue syncing and sending."}
      </p>
      {!schemaMismatchNeedsRebuild && (
        <>
          <Button
            onClick={() => {
              setSchemaUpdateError("");
              void updateRendererAndReload().catch((error: unknown) =>
                setSchemaUpdateError(
                  error instanceof Error
                    ? error.message
                    : "Studio update failed. Try again.",
                ),
              );
            }}
          >
            Update
          </Button>
          {schemaUpdateError && <p role="alert">{schemaUpdateError}</p>}
        </>
      )}
    </Modal>
  );
  useServerFrame(
    data,
    opened,
    error,
    (command) => {
      if (command.action === "open") {
        open(command.id, command.messageId);
        document.documentElement.removeAttribute("data-server-projects");
      } else if (command.action === "new-chat") void newChat(command.path);
      else if (command.action === "settings") {
        setStudioSettingsTab(command.tab || "accounts");
        setStudioSettingsOpen(true);
      } else if (command.action === "focus") {
        window.focus();
        document.getElementById("message")?.focus();
        window.dispatchEvent(new Event("focus"));
      } else if (command.action === "notifications")
        window.dispatchEvent(
          new CustomEvent("studio-navigate", { detail: command }),
        );
      else if (command.action === "projects") {
        document.documentElement.dataset.serverProjects = "true";
        setSidebarCollapsed(false);
        setSidebar(true);
      }
    },
    new Set(
      agents
        .filter((agent) => unreadResult(agent, readState.readStateFor(agent)))
        .map((agent) => agent.id),
    ),
  );
  if (!data)
    return schemaMismatch ? (
      schemaMismatchDialog
    ) : (
      <main className="startup" aria-label="Studio startup">
        <p
          role={error ? "alert" : "status"}
          className={error ? "studio-recovery-banner" : undefined}
        >
          {error || "Connecting to Codex Studio…"}
        </p>
        {error && <a href="/">Reload Studio</a>}
      </main>
    );
  const title = agent?.name || room?.name || legacy?.name || "New conversation";
  const projectName =
    data.runtime?.projects?.find((item) => item.path === agent?.cwd)?.name ||
    agent?.cwd?.split("/").filter(Boolean).at(-1);
  const answerIds = awaitingAnswerIds(chatData?.runtime?.requests || []);
  const deferredIds = new Set<string>(
    (chatData?.runtime?.requests || [])
      .filter((request) => request.deferred && request.status === "pending")
      .map((request) => request.agent)
      .filter((agentId): agentId is string => typeof agentId === "string"),
  );
  const worker = (a: Agent) => (
    <UIErrorBoundary key={a.id} label="this subagent" resetKey={a.id}>
      <WorkerCard
        agent={a}
        accounts={accounts.data.accounts}
        selected={opened === a.id}
        awaitingAnswer={workerState(a, answerIds, deferredIds) === "answer"}
        deferred={deferredIds.has(a.id)}
        indicator={indicators.get(a.id)}
        open={() => open(a.id)}
        previewResult={() =>
          setFilePreview({
            agent: a.id,
            path: a.overview?.resultFile ?? undefined,
          })
        }
        remove={() => {
          setTeamOpen(false);
          remove(a.id, false, true);
        }}
      />
    </UIErrorBoundary>
  );
  // Failed workers are the lead's work; the filter only finds them.
  const failed = (a: Agent) =>
    workerState(a, answerIds, deferredIds) === "attention";
  const query = workerQuery.trim().toLowerCase();
  const shown = workers.filter((a) =>
    query
      ? `${a.name} ${a.status} ${a.role || ""} ${a.overview?.task || ""} ${a.overview?.result || ""}`
          .toLowerCase()
          .includes(query)
      : workerFilter === "attention"
        ? failed(a)
        : workerFilter === "active"
          ? busy.has(a.status ?? "")
          : true,
  );
  // The same states and names as the team summary; Finished stays collapsed below.
  const panelState = (a: Agent) =>
    a.inFlight && ["running", "starting"].includes(a.status ?? "")
      ? "working"
      : workerState(a, answerIds, deferredIds);
  const groups = TEAM_PANEL_STATES.filter(
    ([state]) => state !== "completed",
  ).map(([state, name]) => ({
    name,
    workers: shown.filter((a) => panelState(a) === state),
  }));
  const completed = shown.filter(
    (a) => workerState(a, answerIds, deferredIds) === "completed",
  );
  const smallTeam = workers.length <= 3;
  const showTeamFilters = !smallTeam || !!workerQuery || workerFilter !== "all";
  const teamPanel = (
    <TeamModels accountKey={lead?.accountKey || "default"}>
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
        </div>
        <TeamSummary
          workers={workers}
          answers={answerIds}
          deferred={deferredIds}
        />
        {/* The lead link is only useful from a worker chat. */}
        {lead && opened !== lead.id && (
          <Button
            id="lead-row"
            variant="subtle"
            size="compact-sm"
            title={lead.name || "Main agent"}
            leftSection={<ArrowLeft size={13} />}
            onClick={() => open(lead.id)}
          >
            Back to main agent
          </Button>
        )}
        {showTeamFilters && (
          <>
            <TextInput
              leftSection={<Search size={14} />}
              id="worker-search"
              type="search"
              aria-label="Find a subagent"
              placeholder="Find a worker"
              value={workerQuery}
              onChange={(e) => setWorkerQuery(e.target.value)}
            />
            {query ? (
              <p className="team-search-count" role="status">
                {shown.length} {shown.length === 1 ? "match" : "matches"} in
                this team
              </p>
            ) : (
              <div
                className="team-filters"
                role="group"
                aria-label="Filter subagents"
              >
                {[
                  ["all", "All"],
                  ["active", "Active"],
                  ["attention", "Failed"],
                ].map(([value, label]) => (
                  <UnstyledButton
                    key={value}
                    aria-pressed={workerFilter === value}
                    onClick={() => setWorkerFilter(String(value))}
                  >
                    {label}
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
                {!smallTeam && <h3>{group.name}</h3>}
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
              <summary>Finished</summary>
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
    </TeamModels>
  );
  const toggleTeam = () => {
    if (narrowTeam) setTeamOpen(!teamOpen);
    else {
      setWideTeamOpen(!wideTeamOpen);
      save("codex-team-open", !wideTeamOpen);
    }
  };
  return (
    <>
      <Sidebar
        data={data}
        opened={opened}
        lead={lead}
        open={open}
        prepareChat={prepareChat}
        newChat={(path, folder) => void newChat(path, folder)}
        newSharedChat={(path) => {
          setSidebar(false);
          setSharedCreate({ path });
        }}
        addProject={() => {
          setSidebar(false);
          setModal({
            title: "Add project",
            body: (
              <ProjectDirectoryPicker
                onSelect={async (path) => {
                  await post("/api/projects", { path });
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
                project={data.runtime?.projects?.find(
                  (item) => item.path === path,
                )}
                accounts={accounts.data}
                defaultAccountKey={
                  data.runtime?.projects
                    ?.filter(
                      (item) =>
                        typeof item.path === "string" &&
                        (path === item.path ||
                          path.startsWith(item.path.replace(/\/$/, "") + "/")),
                    )
                    .sort(
                      (a, b) => (b.path?.length || 0) - (a.path?.length || 0),
                    )[0]?.accountKey || accounts.data.defaultAccountKey
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
        indicators={indicators}
        markUnread={(a) => void readState.markUnread(a)}
        markingRead={readState.marking}
        rename={rename}
        remove={remove}
        mobile={sidebar}
        collapsed={sidebarCollapsed}
        onSearch={() => {
          if (isServerView) {
            window.parent.postMessage(
              { kind: "studio-server-search" },
              serverParentOrigin,
            );
            return;
          }
          setSidebar(false);
          setSearchOpen(true);
        }}
        close={() => {
          if (isServerView)
            delete document.documentElement.dataset.serverProjects;
          if (mobileClient) setSidebar(false);
          else {
            setSidebarCollapsed(true);
            save("codex-sidebar-collapsed", true);
          }
        }}
      />
      <main
        className="workspace"
        data-show-message-avatars={studioPreferences.showMessageAvatars}
      >
        <SupervisorRecoveryNotice />
        <header className="workspace-header simple-workspace-header">
          <ActionIcon
            id="sidebar-toggle"
            aria-label="Toggle conversations"
            aria-expanded={mobileClient ? sidebar : !sidebarCollapsed}
            onClick={toggleSidebar}
          >
            <PanelLeft size={18} />
          </ActionIcon>
          <ConversationTitle
            title={title}
            agent={agent}
            indicator={agent ? indicators.get(agent.id) : undefined}
            projectPrefix={
              mobileClient && agent?.cwd ? `${projectName} · ` : ""
            }
            onBack={
              // Only a worker chat links back; the lead and team rooms do not.
              lead && agent && !agent.isLead && agent.id !== lead.id
                ? () => open(lead.id)
                : undefined
            }
            modeControl={
              lead?.source === "managed" && !mobileClient ? (
                <span className="header-mode-summary">
                  {lead.concurrency === 0 ||
                  (lead.concurrency == null &&
                    lead.agentModeSupported &&
                    lead.agentMode === "single")
                    ? "Single agent"
                    : lead.concurrency != null ||
                        (lead.agentModeSupported && lead.agentMode === "multi")
                      ? "Multi agent"
                      : "Mode unavailable"}
                </span>
              ) : undefined
            }
            statusText={
              agent
                ? indicators.get(agent.id)?.kind === "answer" ||
                  (indicators.get(agent.id)?.kind === "working" &&
                    !agent.inFlight)
                  ? indicators.get(agent.id)?.label || ""
                  : livePhase?.id === agent.id
                    ? livePhase.label
                    : [
                        statusLabel(
                          agent.status ?? "unknown",
                          agent.activity?.phase ?? undefined,
                          agent.parkedEvent ?? undefined,
                        ),
                        nativeReleaseLabel(agent),
                      ]
                        .filter(Boolean)
                        .join(" · ")
                : room?.radio
                  ? "Shared chat · One agent speaks at a time"
                  : room?.kind === "private"
                    ? "Private agent chat"
                    : room
                      ? "Broadcast"
                      : ""
            }
          />
          <ActionIcon
            id="studio-settings-toggle"
            aria-label="Studio settings"
            title="Studio settings"
            onClick={() => setStudioSettingsOpen(true)}
          >
            <Settings size={18} />
          </ActionIcon>
          <div id="conversation-header-tools" />
          {!room?.radio && (
            <ActionIcon
              id="chat-settings-toggle"
              aria-label="Chat settings"
              title="Chat settings"
              onClick={() => setSettingsOpen(true)}
            >
              <Settings2 size={18} />
            </ActionIcon>
          )}
          {!!workers.length && (
            <Button
              leftSection={<Users size={16} />}
              id="team-toggle"
              aria-label="Team"
              aria-expanded={narrowTeam ? teamOpen : wideTeamOpen}
              onClick={toggleTeam}
            >
              Team
              {workers.length > 0 && (
                <span className="team-button-count">
                  {
                    workers.filter((worker) => busy.has(worker.status ?? ""))
                      .length
                  }
                  /{workers.length}
                </span>
              )}
            </Button>
          )}
          {!room?.radio && (
            <Button
              id="messages-toggle"
              aria-label="Messages"
              title={
                attentionCount > 0
                  ? `Messages, ${attentionCount} need you`
                  : "Messages"
              }
              leftSection={<MessageSquare size={16} />}
              disabled={!lead}
              onClick={() => {
                setWorkspaceSection("messages");
                setWorkspaceFocus(undefined);
                setWorkspaceOpen(true);
              }}
            >
              <span className="messages-toggle-label">Messages</span>{" "}
              {attentionCount > 0 && (
                <span className="attention-count">{attentionCount}</span>
              )}
            </Button>
          )}
          {!room?.radio && (
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
                <Menu.Label>Views</Menu.Label>
                {(
                  [
                    ["changes", "Changes", FileDiff],
                    ["plan", "Plan", BookOpen],
                    ["rules", "Wake rules", Clock3],
                    ["search", "Search", Search],
                  ] as const
                ).map(([section, label, Icon]) => (
                  <Menu.Item
                    key={section}
                    aria-label={section === "rules" ? "Rules" : label}
                    data-workspace-section={section}
                    leftSection={<Icon size={14} />}
                    onClick={() => {
                      if (section === "search") {
                        if (isServerView)
                          window.parent.postMessage(
                            { kind: "studio-server-search" },
                            serverParentOrigin,
                          );
                        else setSearchOpen(true);
                      } else {
                        setWorkspaceSection(section);
                        setWorkspaceOpen(true);
                      }
                    }}
                  >
                    {label}
                  </Menu.Item>
                ))}
                <Menu.Item
                  id="tasks-toggle"
                  aria-label={`Current activity${taskCount ? `, ${taskCount} active` : ""}`}
                  leftSection={<Activity size={14} />}
                  onClick={() => {
                    setTaskFocus(undefined);
                    setTasksOpen(true);
                  }}
                >
                  Background activity {taskCount || ""}
                </Menu.Item>
                <Menu.Divider />
                <Menu.Label>Chat</Menu.Label>
                <Menu.Item
                  id="chat-actions-settings"
                  aria-label="Chat settings"
                  leftSection={<Settings size={14} />}
                  onClick={() => setSettingsOpen(true)}
                >
                  Chat settings
                </Menu.Item>
                {!!workers.length && (
                  <Menu.Item
                    id="chat-actions-team"
                    aria-label="Team"
                    leftSection={<Users size={14} />}
                    onClick={toggleTeam}
                  >
                    Team{" "}
                    {
                      workers.filter((worker) => busy.has(worker.status ?? ""))
                        .length
                    }
                    /{workers.length}
                  </Menu.Item>
                )}
                {agent?.source === "managed" && (
                  <Menu.Item
                    id="mark-unread"
                    aria-label="Mark chat unread"
                    leftSection={<Mail size={14} />}
                    disabled={
                      !agent.readStateSupported ||
                      !hasCompletedResult(agent) ||
                      readState.marking.has(agent.id)
                    }
                    onClick={() => void readState.markUnread(agent)}
                  >
                    Mark as unread
                  </Menu.Item>
                )}
                {agent?.source === "managed" && (
                  <>
                    {(!!agent.inFlight ||
                      team.some(
                        (member) =>
                          (member.status != null && busy.has(member.status)) ||
                          member.status === "queued" ||
                          member.inFlight,
                      )) && (
                      <Menu.Item
                        data-action="stop-team"
                        color="red"
                        leftSection={<Square size={14} />}
                        onClick={() => {
                          void run(() =>
                            post("/api/stop", {
                              id: agent.rootId ?? agent.id,
                              descendants: true,
                            }),
                          );
                        }}
                      >
                        Stop team
                      </Menu.Item>
                    )}
                    <Menu.Divider />
                    <Menu.Label>Model actions</Menu.Label>
                    {(
                      [
                        ["compact", "Compact", Minimize2],
                        ["review", "Review", ShieldCheck],
                      ] as const
                    )
                      .filter(([action]) =>
                        menuActions(agent.provider ?? undefined).includes(
                          action,
                        ),
                      )
                      .map(([action, label, Icon]) => (
                        <Menu.Item
                          key={action}
                          aria-label={label}
                          data-action={action}
                          leftSection={<Icon size={14} />}
                          disabled={
                            busy.has(agent.status ?? "") ||
                            !!agent.inFlight ||
                            !!nativeThreadError(agent) ||
                            !agent.threadId
                          }
                          onClick={() => {
                            void run(() => submitNativeAction(agent, action));
                          }}
                        >
                          {label}
                          {(!agent.threadId ||
                            agent.inFlight ||
                            busy.has(agent.status ?? "") ||
                            !!nativeThreadError(agent)) && (
                            <small className="menu-action-help">
                              {!agent.threadId
                                ? "Available after the chat starts."
                                : "Wait until the chat is ready."}
                            </small>
                          )}
                        </Menu.Item>
                      ))}
                  </>
                )}
              </Menu.Dropdown>
            </Menu>
          )}
        </header>
        {agent && nativeActions.pending(agent.id) && (
          <div
            role="status"
            className="native-action-pending"
            style={{ padding: "8px 16px" }}
          >
            <span>
              The {nativeActions.pending(agent.id)!.action} request has no
              confirmed reply.{" "}
            </span>
            <Button
              size="compact-sm"
              loading={nativeActions.active}
              onClick={() =>
                void run(() =>
                  submitNativeAction(
                    agent,
                    nativeActions.pending(agent.id)!.action,
                  ),
                )
              }
            >
              Check action request
            </Button>
          </div>
        )}
        {agent && (
          <SessionActivity
            key={`${data.stateDir}:${agent.id}`}
            activities={backgroundActivities(activities.get(agent.id) || [])}
            onOpen={(activity) => {
              if (activity.kind === "agent") open(activity.agentId);
              else if (lead) {
                setTaskFocus({
                  id: activity.id,
                  leadId: lead.id,
                  requestId: crypto.randomUUID(),
                });
                setTasksOpen(true);
              }
            }}
          />
        )}
        {claudeLoginKey &&
          (accounts.data.accounts.some((item) => item.id === claudeLoginKey) ||
            accounts.data.archivedAccounts?.some(
              (item) => item.id === claudeLoginKey,
            )) && (
            <ClaudeSignIn
              key={claudeLoginKey}
              account={
                accounts.data.accounts.find(
                  (item) => item.id === claudeLoginKey,
                ) ||
                accounts.data.archivedAccounts!.find(
                  (item) => item.id === claudeLoginKey,
                )!
              }
              scope={data.stateDir}
              onClose={() => setClaudeLoginKey("")}
              onReady={accounts.refresh}
            />
          )}
        {codexLoginKey &&
          accounts.data.accounts.some((item) => item.id === codexLoginKey) && (
            <CodexSignIn
              account={accounts.data.accounts.find(
                (item) => item.id === codexLoginKey,
              )!}
              state={accounts}
              onClose={() => setCodexLoginKey("")}
            />
          )}
        {[...accounts.data.accounts, ...(accounts.data.archivedAccounts || [])]
          .filter(
            (account) =>
              account.id === accountKey ||
              agents.some(
                (item) =>
                  item.rootId === lead?.id &&
                  !item.deletedAt &&
                  (item.accountKey || "default") === account.id &&
                  item.status === "failed",
              ),
          )
          .map((account) => (
            <AccountSignInNotice
              key={account.id}
              account={account}
              errors={agents
                .filter(
                  (item) =>
                    (item.id === agent?.id || item.rootId === lead?.id) &&
                    (item.accountKey || "default") === account.id,
                )
                .flatMap((item) => {
                  const nativeStatus = item.nativeStatus;
                  const nativeError =
                    nativeStatus &&
                    typeof nativeStatus === "object" &&
                    "error" in nativeStatus
                      ? nativeStatus.error
                      : undefined;
                  return [item.error, nativeError];
                })
                .filter((value): value is string => typeof value === "string")}
              onSignIn={
                account.provider === "claude"
                  ? setClaudeLoginKey
                  : setCodexLoginKey
              }
            />
          ))}
        {error && (
          <div id="error" role="alert">
            {error}
          </div>
        )}
        <div className="sync-notices">
          {!creating &&
            pendingCreations.filter(isLeadCreateRequest).map((request) => (
              <div className="sync-status" key={request.id}>
                <p>
                  The previous chat request needs confirmation:{" "}
                  {request.cwd || "default project"}
                  {request.project_folder ? ` (${request.project_folder})` : ""}
                  .
                </p>
                <Button
                  onClick={() =>
                    void newChat(
                      request.cwd,
                      request.project_folder,
                      request.id,
                    )
                  }
                >
                  Retry chat request
                </Button>
              </div>
            ))}
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
        <UIErrorBoundary label="this conversation" resetKey={opened}>
          {room?.radio ? (
            <RadioChat
              key={room.id}
              room={room}
              data={data}
              getDraft={getDraft}
              subscribeDraft={subscribeDraft}
              setDraft={setDraft}
              refresh={refresh}
              notify={notify}
            />
          ) : (
            <SelectedConversation
              accountsState={accounts}
              syncWorkspaceId={workspaceId}
              id={opened}
              agent={agent}
              room={room}
              legacy={legacy}
              data={
                agent?.source === "managed" && !room
                  ? scopedConversationData!
                  : data
              }
              getDraft={getDraft}
              subscribeDraft={subscribeDraft}
              setDraft={setDraft}
              draftConflicts={conversationDraftConflicts}
              dismissDraft={dismissDraft}
              send={sendToConversation}
              sending={sending}
              schemaMismatch={schemaMismatch}
              outgoing={conversationOutgoing}
              onObserved={observeSends}
              onReadResult={readState.observeRead}
              onOutgoingEdit={editSend}
              refresh={refresh}
              notify={notify}
              limits={visibleLimits}
              limitsAccounts={usageAccounts}
              limitsLoading={!!limitsLoading[accountKey]}
              jumpTarget={
                jumpTarget?.chatId === opened ? jumpTarget : undefined
              }
              limitsAccountLabel={
                selectedAccount?.email || selectedAccount?.label
              }
              reloadLimits={forceReloadLimits}
              onPhase={onPhase}
              onSelect={open}
              onBranchCreated={branchCreated}
              onNewChat={newConversation}
              onChooseChat={chooseConversation}
            />
          )}
        </UIErrorBoundary>
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
        <Suspense fallback={null}>
          <TerminalDock data={data} agent={agent || lead} notify={notify} />
        </Suspense>
      )}
      <SearchOverlay
        opened={searchOpen}
        onClose={() => setSearchOpen(false)}
        data={chatData!}
        onSelect={open}
        navigate={(section, id) => {
          if (id) open(id);
          setSearchOpen(false);
          setWorkspaceSection(section);
          setWorkspaceOpen(true);
        }}
      />
      {(workspaceRendered || workspaceOpen) && (
        <Suspense fallback={null}>
          <Workspace
            allRequests={data.runtime?.requests || []}
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
        </Suspense>
      )}
      {(tasksRendered || tasksOpen) && (
        <Suspense fallback={null}>
          <BackgroundTasks
            key={`background:${lead?.id || "none"}`}
            opened={tasksOpen}
            initialFocus={taskFocus}
            close={() => setTasksOpen(false)}
            afterClose={() =>
              requestAnimationFrame(() => {
                const source =
                  taskFocus && taskFocus.leadId === lead?.id
                    ? document.querySelector<HTMLButtonElement>(
                        `[data-activity-id="${CSS.escape(taskFocus.id)}"]`,
                      )
                    : null;
                (source || chatActionsButton.current)?.focus();
              })
            }
            data={chatData!}
            leadId={lead?.id}
            openAgent={open}
            refresh={refresh}
            notify={notify}
          />
        </Suspense>
      )}
      <Modal
        opened={studioSettingsOpen}
        transitionProps={{ duration: 0 }}
        closeOnEscape={!accountModalOpen}
        closeOnClickOutside={!accountModalOpen}
        onClose={() => {
          setStudioSettingsOpen(false);
          serverSettings?.close();
        }}
        size={modalSizes.settings}
        title="Studio settings"
        classNames={{ body: "studio-settings-body" }}
      >
        <div className="studio-settings-panel" data-testid="studio-settings">
          <Tabs
            value={studioSettingsTab}
            onChange={activateSettingsTab}
            className="studio-settings-tabs"
          >
            <StudioSettingsTabs />
            <Tabs.Panel value="accounts" pt="md">
              <section className="settings-group" aria-label="Studio accounts">
                <Accounts
                  managerOnly
                  onModalOpenChange={setAccountModalOpen}
                  state={accounts}
                  onError={notify}
                />
              </section>
            </Tabs.Panel>
            <Tabs.Panel value="servers" pt="md" keepMounted={false}>
              {serverSettings?.panel || (
                <ServerAccessSettings active={studioSettingsOpen} />
              )}
            </Tabs.Panel>
            <Tabs.Panel value="appearance" pt="md">
              <div className="studio-appearance-groups">
                <section className="settings-group" aria-label="Theme">
                  <h2>Theme</h2>
                  <SettingsRow label="Color scheme">
                    <NativeSelect
                      aria-label="Studio theme"
                      value={studioPreferences.theme}
                      data={[
                        { value: "auto", label: "System" },
                        { value: "light", label: "Light" },
                        { value: "dark", label: "Dark" },
                      ]}
                      onChange={(event) =>
                        updateStudioPreferences({
                          ...studioPreferences,
                          theme: event.currentTarget
                            .value as StudioPreferences["theme"],
                        })
                      }
                    />
                  </SettingsRow>
                </section>
                <section className="settings-group" aria-label="Fonts">
                  <h2>Fonts</h2>
                  <SettingsRow label="Text style">
                    <NativeSelect
                      aria-label="Studio text style"
                      value={studioPreferences.typography}
                      data={[
                        {
                          value: "original",
                          label: "Default text",
                        },
                        { value: "custom", label: "Custom text" },
                      ]}
                      onChange={(event) =>
                        updateStudioPreferences({
                          ...studioPreferences,
                          typography: event.currentTarget
                            .value as StudioPreferences["typography"],
                        })
                      }
                    />
                    <small>
                      Select Custom text to change the font and text sizes.
                    </small>
                  </SettingsRow>
                  <SettingsRow label="Font family">
                    <NativeSelect
                      aria-label="Studio font family"
                      disabled={studioPreferences.typography === "original"}
                      value={studioPreferences.fontFamily}
                      data={Object.entries(fontFamilies).map(
                        ([value, font]) => ({ value, label: font.label }),
                      )}
                      onChange={(event) =>
                        updateStudioPreferences({
                          ...studioPreferences,
                          fontFamily: event.currentTarget
                            .value as StudioPreferences["fontFamily"],
                        })
                      }
                    />
                  </SettingsRow>
                  <SettingsRow
                    label={
                      <span className="studio-range-label">
                        Sidebar text{" "}
                        <output>{studioPreferences.sidebarFontSize}px</output>
                      </span>
                    }
                  >
                    <div className="studio-range-field">
                      <Slider
                        thumbLabel="Sidebar font size"
                        disabled={studioPreferences.typography === "original"}
                        min={12}
                        max={24}
                        step={1}
                        value={studioPreferences.sidebarFontSize}
                        onChange={(value) =>
                          updateStudioPreferences({
                            ...studioPreferences,
                            sidebarFontSize: value,
                          })
                        }
                      />
                    </div>
                  </SettingsRow>
                  <SettingsRow
                    label={
                      <span className="studio-range-label">
                        Main text{" "}
                        <output>{studioPreferences.mainFontSize}px</output>
                      </span>
                    }
                  >
                    <div className="studio-range-field">
                      <Slider
                        thumbLabel="Main font size"
                        disabled={studioPreferences.typography === "original"}
                        min={12}
                        max={24}
                        step={1}
                        value={studioPreferences.mainFontSize}
                        onChange={(value) =>
                          updateStudioPreferences({
                            ...studioPreferences,
                            mainFontSize: value,
                          })
                        }
                      />
                    </div>
                  </SettingsRow>
                </section>
                <section className="settings-group" aria-label="Chat layout">
                  <h2>Chat layout</h2>
                  <SettingsRow label="Width">
                    <NativeSelect
                      aria-label="Chat width layout"
                      value={studioPreferences.contentLayout}
                      data={[
                        { value: "original", label: "Default width" },
                        { value: "custom", label: "Custom width" },
                      ]}
                      onChange={(event) =>
                        updateStudioPreferences({
                          ...studioPreferences,
                          contentLayout: event.currentTarget
                            .value as StudioPreferences["contentLayout"],
                        })
                      }
                    />
                    <small>Select Custom width to change the chat width.</small>
                  </SettingsRow>
                  <SettingsRow
                    label={
                      <span className="studio-range-label">
                        Custom width{" "}
                        <output>{studioPreferences.contentWidth}%</output>
                      </span>
                    }
                  >
                    <div className="studio-range-field">
                      <Slider
                        thumbLabel="Transcript width"
                        disabled={
                          studioPreferences.contentLayout === "original"
                        }
                        min={60}
                        max={100}
                        step={1}
                        value={studioPreferences.contentWidth}
                        onChange={(value) =>
                          updateStudioPreferences({
                            ...studioPreferences,
                            contentWidth: value,
                          })
                        }
                      />
                      <small>
                        Applies to messages, progress, and composer. Narrow
                        screens use the full available width.
                      </small>
                    </div>
                  </SettingsRow>
                </section>
                <section className="settings-group" aria-label="Messages">
                  <h2>Messages</h2>
                  <label className="settings-field studio-preference-toggle">
                    <span className="settings-label">Show message avatars</span>
                    <input
                      aria-label="Show message avatars"
                      type="checkbox"
                      checked={studioPreferences.showMessageAvatars}
                      onChange={(event) =>
                        updateStudioPreferences({
                          ...studioPreferences,
                          showMessageAvatars: event.currentTarget.checked,
                        })
                      }
                    />
                  </label>
                </section>
                <section className="settings-group" aria-label="Maintenance">
                  <h2>Maintenance</h2>
                  <Button
                    loading={removingAllSending}
                    onClick={async () => {
                      if (removingAllSending) return;
                      setRemovingAllSending(true);
                      try {
                        const count = await removeAllSendingMessages(
                          { stateDir: data.stateDir, workspaceId },
                          data,
                          outgoingMessages,
                        );
                        notify(
                          `Removed ${count} sending messages from this device. Work already sent continues.`,
                        );
                      } catch (error) {
                        notify(errorText(error));
                      } finally {
                        setRemovingAllSending(false);
                      }
                    }}
                  >
                    Remove all sending messages
                  </Button>
                </section>
              </div>
            </Tabs.Panel>
            <Tabs.Panel value="federation" pt="md">
              {lead?.isLead && (
                <FederationSettings
                  active={studioSettingsOpen}
                  leadId={lead.id}
                  agents={data?.runtime?.agents || []}
                  refresh={refresh}
                  notify={notify}
                />
              )}
            </Tabs.Panel>
            <Tabs.Panel value="linux-vm" pt="md">
              <LinuxVMSettings active={studioSettingsOpen} />
            </Tabs.Panel>
            <Tabs.Panel value="hotkeys" pt="md">
              <section className="settings-group" aria-label="Sidebar shortcut">
                <h2>Keyboard shortcut</h2>
                <SettingsRow label="Toggle sidebar">
                  <div className="studio-shortcut-control">
                    <TextInput
                      className="studio-shortcut-input"
                      aria-describedby="studio-shortcut-hint"
                      aria-label="Toggle sidebar shortcut"
                      readOnly
                      value={formatSidebarShortcut(
                        studioPreferences.sidebarShortcut,
                      )}
                      onKeyDown={(event) => {
                        if (event.key === "Tab" || event.key === "Escape") {
                          setSidebarShortcutError("");
                          return;
                        }
                        if (
                          ["Control", "Meta", "Alt", "Shift"].includes(
                            event.key,
                          )
                        )
                          return;
                        if (event.ctrlKey || event.metaKey)
                          event.stopPropagation();
                        event.preventDefault();
                        const mods = [
                          event.metaKey ? "Meta" : "",
                          event.ctrlKey ? "Control" : "",
                          event.altKey ? "Alt" : "",
                          event.shiftKey ? "Shift" : "",
                        ].filter(Boolean);
                        const candidate = [...mods, event.key].join("+");
                        if (!parseSidebarShortcut(candidate)) {
                          setSidebarShortcutError(
                            "Choose a letter or number with Ctrl or ⌘. Browser-reserved shortcuts cannot be used.",
                          );
                          return;
                        }
                        setSidebarShortcutError("");
                        updateStudioPreferences({
                          ...studioPreferences,
                          sidebarShortcut: candidate,
                        });
                      }}
                      onFocus={() =>
                        setSidebarShortcutError(
                          "Press a modifier and a letter or number to set the shortcut.",
                        )
                      }
                      onBlur={() => setSidebarShortcutError("")}
                    />
                    <div className="studio-shortcut-tokens" aria-hidden="true">
                      {formatSidebarShortcut(studioPreferences.sidebarShortcut)
                        .split("+")
                        .map((key, index) => (
                          <kbd key={`${key}:${index}`}>{key}</kbd>
                        ))}
                    </div>
                  </div>
                  <small id="studio-shortcut-hint">
                    Select the field. Press a modifier and a letter or number.
                  </small>
                  {sidebarShortcutError && (
                    <small role="status">{sidebarShortcutError}</small>
                  )}
                </SettingsRow>
              </section>
            </Tabs.Panel>
          </Tabs>
          {studioPreferencesError && (
            <p className="studio-preferences-error" role="alert">
              {studioPreferencesError}
            </p>
          )}
        </div>
      </Modal>
      <Modal
        opened={settingsOpen}
        closeOnEscape={
          !accountModalOpen &&
          !mainSettingsOpen &&
          !workerSettingsOpen &&
          !reviewSettingsOpen
        }
        closeOnClickOutside={
          !accountModalOpen &&
          !mainSettingsOpen &&
          !workerSettingsOpen &&
          !reviewSettingsOpen
        }
        onClose={() => setSettingsOpen(false)}
        title="Chat settings"
        size={modalSizes.settings}
      >
        <div className="chat-settings-panel chat-settings-rows">
          {agent?.source === "managed" && (
            <>
              <SettingsRow label="Model">
                <UnifiedAgentSettings
                  key={"execution:" + agent.id}
                  state={accounts}
                  notify={notify}
                  settingsRow
                  permissionsTargetId="chat-settings-permissions"
                  extrasTargetId="chat-settings-modes"
                  onOpenChange={setMainSettingsOpen}
                  onAccountModalOpenChange={setAccountModalOpen}
                  agent={agent}
                  catalog={agentModels}
                  team={data?.runtime?.agents || []}
                  refresh={refresh}
                />
              </SettingsRow>
              {lead?.isLead &&
                (["worker", "review"] as const).map((role) => (
                  <SettingsRow
                    key={role}
                    label={role === "worker" ? "Workers" : "Review"}
                  >
                    <UnifiedAgentSettings
                      state={accounts}
                      notify={notify}
                      settingsRow
                      initialRole={role}
                      onOpenChange={
                        role === "worker"
                          ? setWorkerSettingsOpen
                          : setReviewSettingsOpen
                      }
                      onAccountModalOpenChange={setAccountModalOpen}
                      agent={lead}
                      catalog={agentModels}
                      team={data?.runtime?.agents || []}
                      refresh={refresh}
                    />
                  </SettingsRow>
                ))}
            </>
          )}
          {lead?.source === "managed" && (
            <SettingsRow label="Parallel">
              <SubagentConcurrencyControl
                compact
                lead={lead}
                stateDir={data.stateDir}
                workspaceId={workspaceId}
                refresh={refresh}
              />
            </SettingsRow>
          )}
          <div id="chat-settings-permissions" />
          <BrowserAccessNotice
            compact
            accountKey={accountKey}
            active={settingsOpen && (agent || lead)?.provider !== "claude"}
          />
          <div className="chat-settings-footer">
            {agent?.cwd && (
              <Button
                variant="default"
                className="settings-new-chat"
                leftSection={<Plus size={14} />}
                aria-label="New chat in this project"
                disabled={creating}
                onClick={() => {
                  setSettingsOpen(false);
                  void newChat(agent.cwd ?? undefined);
                }}
              >
                New chat
              </Button>
            )}
            {agent?.source === "managed" &&
              (["compact", "review"] as const)
                .filter((action) =>
                  menuActions(agent.provider ?? undefined).includes(action),
                )
                .map((action) => {
                  const disabled =
                    busy.has(agent.status ?? "") ||
                    !!agent.inFlight ||
                    !!nativeThreadError(agent) ||
                    !agent.threadId;
                  return (
                    <Tooltip
                      key={action}
                      disabled={!disabled}
                      label={
                        !agent.threadId
                          ? "Available after the chat starts."
                          : "Wait until the chat is ready."
                      }
                    >
                      <span>
                        <Button
                          variant="default"
                          disabled={disabled}
                          onClick={() => {
                            setSettingsOpen(false);
                            void run(() => submitNativeAction(agent, action));
                          }}
                        >
                          {action === "compact" ? "Compact" : "Review"}
                        </Button>
                      </span>
                    </Tooltip>
                  );
                })}
          </div>
          <details className="chat-settings-more">
            <summary>More</summary>
            <div id="chat-settings-modes" />
            {agent?.cwd && (
              <Button
                leftSection={<Folder size={14} />}
                aria-label="Choose project folder"
                onClick={() => {
                  setSettingsOpen(false);
                  project();
                }}
              >
                Folder
              </Button>
            )}
            {agent?.provider === "claude" && (
              <Suspense fallback={null}>
                <ClaudeSettings
                  agent={agent}
                  account={selectedAccount}
                  permissionsTargetId="chat-settings-permissions"
                  onSignIn={(key) => {
                    setSettingsOpen(false);
                    setClaudeLoginKey(key);
                  }}
                />
              </Suspense>
            )}
          </details>
        </div>
      </Modal>
      <Modal
        opened={!!modal}
        onClose={() => setModal(null)}
        title={modal?.title}
      >
        <div className="picker">{modal?.body}</div>
      </Modal>
      <Modal
        opened={!!sharedCreate}
        onClose={() => setSharedCreate(null)}
        title="New shared chat"
        size="md"
      >
        {sharedCreate && (
          <SharedChatCreate
            data={data}
            accounts={accounts.data}
            initialPath={sharedCreate.path}
            refresh={refresh}
            created={(id) => {
              setSharedCreate(null);
              open(id);
            }}
          />
        )}
      </Modal>
      {schemaMismatchDialog}
      {toast && (
        <div id="toast" role="status">
          {toast}
        </div>
      )}
      <FilePreview target={filePreview} onClose={() => setFilePreview(null)} />
    </>
  );
}
