import { useServerActivity } from "../servers/activity";
import { chatWaitState } from "./chat-status/chatStatusModel";
import { serviceTimeText } from "../local-time";
import { nativeStatusMessage } from "./conversation/nativeStatus";
import AgentAvatar from "./agents/AgentAvatar";
import MessageQueue, {
  type LocalQueueItem,
  type QueueItem,
} from "./MessageQueue";
import { useMessageQueue } from "./useMessageQueue";
import { useMessageReceipts } from "./useMessageReceipts";
import { receiptOutgoing, receiptTranscript } from "../sync/messageReceipts";
import { useVisibleChatResult, type ChatReadProof } from "./useChatReadState";
import { displayError } from "../errorPresentation";
import {
  NativeError,
  NativeNotice,
  isNonBlockingWarning,
} from "./NativeNotice";
import { ActionIcon, Button, Loader, Modal, Textarea } from "@mantine/core";
import { useMediaQuery } from "@mantine/hooks";
import { createPortal } from "react-dom";
import {
  ArrowDown,
  ArrowUp,
  ListTree,
  Trash2,
  Square,
  Terminal,
  ListEnd,
} from "lucide-react";
import {
  useCallback,
  useLayoutEffect,
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
} from "react";
import { get, post, errorText } from "../api";
import SafetyBuffering from "./conversation/transcript/SafetyBuffering";
import { currentCapacityRetry } from "../capacityRetry";
import {
  isCyberPolicyRefusal,
  nativeErrorKind,
  nativeThreadError,
} from "../nativeErrors";
import { useMessages, transcriptMessages } from "../hooks";
import DraftVersions from "./DraftVersions";
import type { DraftVersion } from "../sync/drafts";
import { changeOutbox, type OutgoingMessage } from "../sync/send";
import { useDisplayPhase } from "./useDisplayPhase";
import {
  outgoingTranscript,
  deliveryLabel,
  applySendingOverlay,
  messageRenderKey,
  explicitQueue,
  dispatchedMessage,
  mergeQueueOrder,
} from "./message-delivery/messageDelivery";
import { useRemovedMessages } from "./removedMessages";
import {
  isSendingMessage,
  removeSendingMessage,
} from "./removeSendingMessages";

import { useConversationScroll } from "./useConversationScroll";
import { useAttachmentDrafts } from "./useAttachmentDrafts";
import { useUploadRecovery } from "./useUploadRecovery";
import { queueUploads } from "../sync/uploads";
import {
  statusLabel,
  type Agent,
  type Json,
  type Message,
  type Room,
  type Snapshot,
} from "../types";
import Usage from "./Usage";
import ConversationWarnings from "./ConversationWarnings";
import type { UsageAccount } from "./Usage";
import PromptNavigator from "./PromptNavigator";
import { usePromptRecall } from "./usePromptRecall";
import PromptComposer, {
  type DraftReader,
  type DraftSubscription,
} from "./prompt-composer/PromptComposer";
import PromptInput from "./prompt-composer/PromptInput";
import { reportPromptComposerRender } from "./prompt-composer/renderProbe";
import Requests from "./questions/Requests";
import MessageDate from "./conversation/transcript/MessageDate";
import TurnHistory from "./conversation/transcript/TurnHistory";
import { revealHistoryMessage } from "./conversation/transcript/historyWindowModel";
import { isEmptyAssistantMessage } from "./turnHistoryModel";
import { Dictation } from "./Dictation";
import RealtimeVoice from "./RealtimeVoice";
import OutboxControls from "./OutboxControls";
import StreamingText from "./conversation/transcript/StreamingText";
import SelectionQuote, {
  selectedExcerpt,
} from "./conversation/transcript/SelectionQuote";
import MessageActions from "./conversation/transcript/MessageActions";
import AgentPhase from "./agents/AgentPhase";
import AgentPanel from "./agents/AgentPanel";
import { UnifiedAgentSettings } from "./agents/UnifiedAgentSettings";
import type { useAccounts } from "./Accounts";
import { useWorkerModels } from "./agents/WorkerModelPicker";
import ComposerAttachments, {
  MessageAttachments,
  type Attachment,
} from "./ComposerAttachments";
import "./chat-controls.css";
import "./conversation/empty-chat-settings.css";
import { copyText } from "../clipboard/clipboard";

function isJsonObject(value: unknown): value is Json {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

// Message controls keep stable identities while their actions read the latest
// committed draft and chat. These callbacks run from events, never during render.
function useMessageAction<Args extends unknown[], Result>(
  action: (...args: Args) => Result,
): (...args: Args) => Result {
  const current = useRef(action);
  useLayoutEffect(() => {
    current.current = action;
  });
  return useCallback((...args: Args) => current.current(...args), []);
}

function FollowLatest(p: {
  subscribe: (listener: () => void) => () => void;
  getSnapshot: () => boolean;
  onClick: () => void;
}) {
  const following = useSyncExternalStore(
    p.subscribe,
    p.getSnapshot,
    p.getSnapshot,
  );
  if (following) return null;
  return (
    <div className="jump-slot">
      <Button
        id="jump-latest"
        className="jump"
        variant="default"
        radius="xl"
        leftSection={<ArrowDown size={14} />}
        onClick={p.onClick}
      >
        Latest
      </Button>
    </div>
  );
}

const compactToolsBreakpoint = "(max-width: 760px)";
const compactToolsWorkspaceWidthPx = 900;
const compactToolsFontThresholdPx = 20;
const compactToolsMenuMaxWidthPx = 360;
const compactToolsMenuViewportGutterPx = 8;
const compactToolsMenuGapPx = 4;

function useCompactHeaderTools() {
  const isCompact = () => {
    const fontSize = Number.parseFloat(
      getComputedStyle(document.documentElement).getPropertyValue(
        "--studio-main-font-size",
      ),
    );
    const workspaceWidth =
      document.querySelector<HTMLElement>(".workspace")?.clientWidth ??
      window.innerWidth;
    return (
      window.matchMedia(compactToolsBreakpoint).matches ||
      fontSize >= compactToolsFontThresholdPx ||
      workspaceWidth < compactToolsWorkspaceWidthPx
    );
  };
  const [compact, setCompact] = useState(() =>
    typeof window === "undefined" ? false : isCompact(),
  );
  useEffect(() => {
    const viewport = window.matchMedia(compactToolsBreakpoint);
    const workspace = document.querySelector<HTMLElement>(".workspace");
    const update = () => setCompact(isCompact());
    const preferences = new MutationObserver(update);
    const workspaceObserver = workspace
      ? new ResizeObserver(update)
      : undefined;
    if (workspace) workspaceObserver?.observe(workspace);
    viewport.addEventListener("change", update);
    preferences.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["style"],
    });
    update();
    return () => {
      viewport.removeEventListener("change", update);
      workspaceObserver?.disconnect();
      preferences.disconnect();
    };
  }, []);
  return compact;
}

export default function Conversation(p: {
  id: string | null;
  syncWorkspaceId?: string;
  agent?: Agent;
  room?: Room;
  legacy?: Json;
  data: Snapshot;
  getDraft: DraftReader;
  subscribeDraft: DraftSubscription;
  setDraft: (v: string | ((current: string) => string), id?: string) => void;
  draftConflicts?: DraftVersion[];
  dismissDraft?: (version: DraftVersion) => void;
  send: (options?: {
    assets?: string[];
    attachments?: Attachment[];
    onPersist?: () => void | Promise<void>;
    delivery?: "after_tool" | "after_turn";
  }) => Promise<void>;
  outgoing?: OutgoingMessage[];
  onObserved?: (ids: string[]) => void;
  onReadResult?: (proof: ChatReadProof) => void;
  onOutgoingEdit?: (id: string, text: string) => void;
  onSelect?: (id: string) => void;
  onBranchCreated?: (id: string) => void;
  onNewChat?: () => void;
  onChooseChat?: () => void;
  sending: boolean;
  schemaMismatch?: boolean;
  refresh: () => Promise<void>;
  accountsState: ReturnType<typeof useAccounts>;
  notify: (s: string) => void;
  limits: Json | null;
  limitsAccounts?: UsageAccount[];
  limitsLoading?: boolean;
  jumpTarget?: { messageId: string; requestId: string };
  onJumpHandled?: (requestId: string) => void;
  limitsAccountLabel?: string;
  reloadLimits: () => void;
  onPhase: (id: string | null, label: string) => void;
}) {
  reportPromptComposerRender("conversation");
  const limitsData = isJsonObject(p.limits?.data) ? p.limits.data : undefined;
  const rateLimits = isJsonObject(limitsData?.rateLimits)
    ? limitsData.rateLimits
    : undefined;
  const planType =
    typeof rateLimits?.planType === "string" ? rateLimits.planType : undefined;
  const mobileClient = useMediaQuery("(max-width: 760px)");
  const kind = p.room ? "room" : p.legacy ? "legacy" : "agent";
  const compactHeaderTools = useCompactHeaderTools();
  const [headerTools, setHeaderTools] = useState<HTMLElement | null>(null);
  const [toolsExpanded, setToolsExpanded] = useState(false);
  const [toolsMenuPosition, setToolsMenuPosition] = useState<{
    left: number;
    top: number;
    width: number;
  } | null>(null);
  useLayoutEffect(() => {
    setHeaderTools(document.getElementById("conversation-header-tools"));
  }, []);
  useEffect(() => {
    if (!compactHeaderTools) setToolsExpanded(false);
  }, [compactHeaderTools]);
  useEffect(() => {
    if (!compactHeaderTools || !toolsExpanded || !headerTools) return;
    const menu = headerTools.querySelector(".conversation-header-tools-menu");
    if (!menu) return;
    const dismissOutside = (event: PointerEvent) => {
      const target = event.target;
      if (!(target instanceof Element)) return;
      if (menu.contains(target) || target.closest(".mantine-Popover-dropdown"))
        return;
      setToolsExpanded(false);
    };
    const dismissEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") setToolsExpanded(false);
    };
    document.addEventListener("pointerdown", dismissOutside, true);
    document.addEventListener("keydown", dismissEscape, true);
    return () => {
      document.removeEventListener("pointerdown", dismissOutside, true);
      document.removeEventListener("keydown", dismissEscape, true);
    };
  }, [compactHeaderTools, headerTools, toolsExpanded]);
  useLayoutEffect(() => {
    if (!compactHeaderTools || !toolsExpanded || !headerTools) {
      setToolsMenuPosition(null);
      return;
    }
    const summary = headerTools.querySelector(
      ".conversation-header-tools-summary",
    );
    if (!summary) return;
    const updatePosition = () => {
      const anchor = summary.getBoundingClientRect();
      const viewportWidth =
        document.documentElement.clientWidth || window.innerWidth;
      const menuWidth = Math.min(
        compactToolsMenuMaxWidthPx,
        Math.max(1, viewportWidth - compactToolsMenuViewportGutterPx * 2),
      );
      const minLeft = compactToolsMenuViewportGutterPx;
      const maxLeft = Math.max(minLeft, viewportWidth - menuWidth - minLeft);
      setToolsMenuPosition({
        left: Math.max(minLeft, Math.min(anchor.left, maxLeft)),
        top: anchor.bottom + compactToolsMenuGapPx,
        width: menuWidth,
      });
    };
    updatePosition();
    const observer = new ResizeObserver(updatePosition);
    observer.observe(summary);
    const workspace = headerTools.closest<HTMLElement>(".workspace");
    if (workspace) observer.observe(workspace);
    window.addEventListener("resize", updatePosition);
    window.visualViewport?.addEventListener("resize", updatePosition);
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", updatePosition);
      window.visualViewport?.removeEventListener("resize", updatePosition);
    };
  }, [compactHeaderTools, headerTools, toolsExpanded]);
  const {
    items: history,
    historyVersion,
    notice,
    before,
    older,
    newer,
    setPageAnchor,
    after,
    historical,
    pageLoading,
    showLatest,
    ensureMessage,
    liveAgent,
    connection,
    loaded,
  } = useMessages(
    p.id,
    kind,
    p.agent?.source === "managed",
    p.data.stateDir,
    p.syncWorkspaceId,
  );
  const outgoing = (p.outgoing || []).filter(
    (entry) => entry.body.room === p.id,
  );
  const receiptIds = [
    ...new Set([
      ...outgoing
        .filter(
          (entry) =>
            ["accepted", "uncertain"].includes(entry.status) &&
            (entry.receipt?.status !== "delivered" ||
              entry.receipt.materialized !== true),
        )
        .map((entry) => entry.id),
      ...history
        .filter(
          (item) =>
            item.role === "user" &&
            (item.pending ||
              ["pending", "reserved", "dispatching", "uncertain"].includes(
                item.deliveryStatus || "",
              )),
        )
        .map(
          (item) =>
            item.clientMessageId ||
            (item.id.startsWith(p.id + ":")
              ? item.id.slice(p.id!.length + 1)
              : item.id),
        ),
    ]),
  ].sort();
  const receipts = useMessageReceipts(
    p.agent?.source === "managed" ? p.id : null,
    `${p.data.stateDir}:${p.syncWorkspaceId}:${p.id}`,
    p.syncWorkspaceId,
    receiptIds,
  );
  const delivery = useMemo(
    () =>
      outgoingTranscript(
        receiptTranscript(history, p.id || "", receipts),
        (p.outgoing || [])
          .filter((entry) => entry.body.room === p.id)
          .map((entry) => receiptOutgoing(entry, receipts.get(entry.id))),
      ),
    [history, p.outgoing, p.id, receipts],
  );
  const removed = useRemovedMessages(p.data.stateDir, kind, p.id);
  const items = useMemo(
    () =>
      [
        ...delivery.items.map((message) => {
          if (!isSendingMessage(message)) return message;
          // A stale receipt must not make a restored display copy send again.
          return (
            removed.restored.find(
              (copy) =>
                message.id === copy.id ||
                (copy.clientMessageId &&
                  copy.clientMessageId === message.clientMessageId),
            ) || message
          );
        }),
        ...removed.restored.filter(
          (copy) =>
            !delivery.items.some(
              (message) =>
                message.id === copy.id ||
                (copy.clientMessageId &&
                  copy.clientMessageId === message.clientMessageId),
            ),
        ),
      ].filter((message) => !removed.hidden(message)),
    [delivery.items, removed.hidden, removed.restored],
  );
  const historyAgentId = p.id;
  const promptRecall = usePromptRecall(
    `${p.data.stateDir}:${kind}:${p.id}:${historyVersion}`,
    p.room ? [] : items,
    () => p.getDraft(p.id || "new"),
    p.setDraft,
    p.agent?.source === "managed" && kind === "agent" && historyAgentId
      ? {
          before: before ? String(before) : null,
          load: async (cursor) => {
            const page = await get("/api/transcript/page", {
              query: { id: historyAgentId, before: cursor, limit: 500 },
            });
            if (page.unavailable) throw new Error(page.unavailable);
            if (
              historyVersion &&
              page.historyVersion &&
              historyVersion !== page.historyVersion
            )
              throw new Error(
                "The conversation changed. Open its message history again.",
              );
            return {
              messages: transcriptMessages(
                page.items || [],
                historyAgentId,
              ).filter((item) => !removed.hidden(item)),
              before: page.nextCursor || null,
            };
          },
          onError: (error) => p.notify(errorText(error)),
        }
      : undefined,
  );
  const observed = delivery.observed.join(",");
  useEffect(() => {
    if (observed) p.onObserved?.(observed.split(","));
  }, [observed, p.onObserved]);
  const {
    scroll,
    content,
    getFollow,
    subscribeFollow,
    setFollow,
    onScroll,
    remember,
    getAnchorId,
  } = useConversationScroll(`${p.data.stateDir}:${kind}:${p.id}`, loaded);
  const input = useRef<HTMLTextAreaElement>(null);
  const navigationAttempt = useRef(0);
  const handledJump = useRef("");
  const [highlighted, setHighlighted] = useState<string | null>(null);
  const navigateToMessage = async (id: string) => {
    const root = scroll.current;
    if (!root) return;
    const chat = p.id;
    const attempt = ++navigationAttempt.current;
    setFollow(false);
    setPageAnchor(getAnchorId());
    const available = await ensureMessage(id);
    if (activeId.current !== chat || navigationAttempt.current !== attempt)
      return;
    if (!available)
      throw new Error("This message is unavailable in this conversation.");
    for (let frame = 0; frame < 20; frame++) {
      if (
        scroll.current !== root ||
        activeId.current !== chat ||
        navigationAttempt.current !== attempt
      )
        return;
      revealHistoryMessage(root, id);
      const target: HTMLElement | undefined = Array.from(
        root.querySelectorAll<HTMLElement>("[data-message]"),
      ).find(
        (element) =>
          element.dataset.message === id ||
          element.dataset.sourceMessage === id,
      );
      if (target) {
        let container: HTMLElement | null = target.parentElement;
        while (container && container !== root) {
          if (container instanceof HTMLDetailsElement) container.open = true;
          container = container.parentElement;
        }
        if (!target.hasAttribute("data-lazy-message")) {
          root.scrollTop +=
            target.getBoundingClientRect().top -
            root.getBoundingClientRect().top -
            16;
          setHighlighted(target.dataset.message || null);
          remember();
          return;
        }
      }
      await new Promise<void>((resolve) =>
        requestAnimationFrame(() => resolve()),
      );
    }
    throw new Error("The message could not be displayed. Search again.");
  };
  const jumpToPrompt = useMessageAction((id: string) => {
    void navigateToMessage(id).catch((error) => p.notify(errorText(error)));
  });
  const returnToLatest = () => {
    navigationAttempt.current++;
    showLatest();
    setHighlighted(null);
    setFollow(true);
  };
  useEffect(() => {
    if (!p.jumpTarget || handledJump.current === p.jumpTarget.requestId) return;
    const target = p.jumpTarget;
    handledJump.current = target.requestId;
    void navigateToMessage(target.messageId)
      .catch((error) => p.notify(errorText(error)))
      .finally(() => p.onJumpHandled?.(target.requestId));
  }, [p.jumpTarget?.requestId, p.id]);
  const attachmentKey = `codex-agent-attachments:${p.data.stateDir}`;
  const [attachments, setAttachments] = useAttachmentDrafts(
    attachmentKey,
    p.notify,
  );
  const [addingFiles, setUploading] = useState(false);
  const uploadRecovery = useUploadRecovery(
    p.data.stateDir,
    p.syncWorkspaceId || "",
    setAttachments,
  );
  const pendingFiles = uploadRecovery.pending.filter(
    (row) => row.agent === p.id,
  );
  const uploading = addingFiles || pendingFiles.length > 0;
  useServerActivity(addingFiles || uploadRecovery.pending.length > 0);
  const [limitsOpen, setLimitsOpen] = useState(false);
  const refreshedFailure = useRef("");
  const sendLock = useRef<symbol | null>(null);
  const latestSend = useRef<Record<string, symbol>>({});
  const branchLock = useRef(false);
  const branchRequests = useRef<Record<string, string>>({});
  const [branchDraft, setBranchDraft] = useState<{
    message: Message;
    before: boolean;
    text: string;
  } | null>(null);
  const [branching, setBranching] = useState(false);
  const [editLoading, setEditLoading] = useState<string | null>(null);
  const editAttempt = useRef(0);
  const [dragging, setDragging] = useState(false);
  const [stopping, setStopping] = useState(false);
  const stopAttempt = useRef(0);
  useEffect(() => {
    stopAttempt.current += 1;
    setStopping(false);
  }, [p.id]);
  const uploadLock = useRef(false);
  const activeId = useRef(p.id);
  activeId.current = p.id;
  const assets = attachments[p.id || ""] || [];
  const agent = useMemo(
    () =>
      p.agent && liveAgent?.id === p.id
        ? ({ ...p.agent, ...liveAgent } as Agent)
        : p.agent,
    [p.agent, liveAgent, p.id],
  );
  const wait = useMemo(
    () => (agent ? chatWaitState(p.data, agent) : undefined),
    [p.data, agent],
  );
  useVisibleChatResult(
    scroll,
    p.agent,
    items,
    loaded,
    p.onReadResult,
    p.syncWorkspaceId,
    !historical && !after,
  );
  const threadBlock = nativeThreadError(agent);
  const [, setModelCommandOpen] = useState(false);
  const [modelCommandRequest, setModelCommandRequest] = useState(0);
  // Load the chat account catalog up front: the closed composer trigger
  // needs it to name the model behind a default alias.
  const modelCatalog = useWorkerModels(
    p.agent?.accountKey || "default",
    p.agent?.source === "managed",
  );
  useEffect(() => {
    setModelCommandOpen(false);
    setModelCommandRequest(0);
  }, [p.id, p.agent?.accountKey, p.syncWorkspaceId, p.data.stateDir]);
  const capacityRetry = agent ? currentCapacityRetry(agent) : null;
  const errorKind = nativeErrorKind(agent?.error);
  const failureKey = [
    p.id,
    agent?.nativeTurnError?.turnId || agent?.turnId || agent?.lastCompletedTurn,
    errorKind,
  ].join(":");
  useEffect(() => {
    if (!["usageLimitExceeded", "rateLimitExceeded"].includes(errorKind))
      return;
    if (refreshedFailure.current === failureKey) return;
    refreshedFailure.current = failureKey;
    p.reloadLimits();
  }, [errorKind, failureKey, p.reloadLimits]);
  const displayPhase = useDisplayPhase(
    p.id,
    agent?.activity?.phase || "thinking",
  );
  const detailedActivity =
    !!threadBlock ||
    capacityRetry?.status === "scheduled" ||
    connection === "reconnecting" ||
    !!agent?.error ||
    !!agent?.startAttempt?.prepareError ||
    !!agent?.startAttempt?.responseError ||
    ["retrying", "auth", "safety", "error"].includes(
      agent?.activity?.phase || "",
    ) ||
    !["starting", "running", "queued", "idle", "completed"].includes(
      agent?.status || "idle",
    );
  const reasoningVisible =
    agent?.activity?.phase === "thinking" &&
    items.at(-1)?.turnId === agent?.turnId &&
    items.at(-1)?.role === "reasoning" &&
    typeof items.at(-1)?.reasoningSince === "number";
  useEffect(() => {
    p.onPhase(
      p.id,
      connection === "reconnecting"
        ? "Reconnecting"
        : threadBlock
          ? "Chat stopped as a precaution"
          : capacityRetry?.status === "scheduled"
            ? "Waiting to retry model"
            : displayError(nativeStatusMessage(agent?.nativeStatus)) ||
              (["starting", "running"].includes(agent?.status || "")
                ? "Working"
                : ["waiting", "parked"].includes(agent?.status || "")
                  ? wait?.label || "Turn ended"
                  : statusLabel(agent?.status || "idle")),
    );
  }, [
    p.id,
    agent?.status,
    wait?.label,
    agent?.activity?.phase,
    agent?.nativeStatus,
    threadBlock,
    capacityRetry?.status,
    connection,
    p.onPhase,
  ]);
  useEffect(() => {
    setBranchDraft(null);
    editAttempt.current++;
    setEditLoading(null);
    setHighlighted(null);
    setLimitsOpen(false);
  }, [p.id]);
  const managed = agent?.source === "managed";
  const emptyMainChat =
    loaded && !items.length && !before && !after && !p.room && p.agent?.isLead;
  const queueScope = `${p.data.stateDir}:${p.syncWorkspaceId || ""}:${p.id}`;
  const messageQueue = useMessageQueue({
    id: p.id,
    enabled: managed,
    scope: queueScope,
    workspaceId: p.syncWorkspaceId,
    observed: p.onObserved,
    edited: p.onOutgoingEdit,
    refresh: p.refresh,
  });
  const queued = useMemo<QueueItem[]>(
    () =>
      [
        ...messageQueue.items,
        ...items
          .filter(
            (item) =>
              item.role === "user" &&
              explicitQueue(item) &&
              !dispatchedMessage(item) &&
              ["sending", "pending", "queued", "accepted"].includes(
                item.deliveryStatus || (item.pending ? "pending" : ""),
              ) &&
              !messageQueue.items.some(
                (row) =>
                  row.id === item.clientMessageId ||
                  row.id === item.id ||
                  `${p.id}:${row.id}` === item.id,
              ),
          )
          .map((item): LocalQueueItem => ({
            id: item.clientMessageId || item.id,
            text: item.text,
            ...(item.assets ? { assets: item.assets } : {}),
            requestedDelivery: item.requestedDelivery,
            localDelivery: true,
          })),
      ].filter(
        (entry) =>
          (("localDelivery" in entry && entry.localDelivery === true) ||
            explicitQueue(entry)) &&
          !items.some(
            (item) =>
              item.role === "user" &&
              dispatchedMessage(item) &&
              (item.clientMessageId === entry.id ||
                item.id === entry.id ||
                item.id === `${p.id}:${entry.id}`),
          ),
      ),
    [messageQueue.items, items, p.id],
  );
  // The queue holds only after_turn messages (Tab). Other input is
  // delivered at once and stays in the chat while it is sent.
  const { queue, sending } = useMemo(() => {
    const shown: typeof queued = [];
    const held: typeof queued = [];
    for (const entry of queued) {
      const mode =
        entry.requestedDelivery ||
        ("delivery" in entry ? entry.delivery : undefined);
      (mode === "after_turn" ? shown : held).push(entry);
    }
    return { queue: shown, sending: held };
  }, [queued]);
  const addFiles = async (files: globalThis.File[]) => {
    if (!p.id || !managed)
      throw new Error("Select a managed chat before attaching files.");
    if (uploadLock.current)
      throw new Error("Wait for the selected files to be saved, then retry.");
    if (!files.length) throw new Error("Select at least one file.");
    if (files.length + assets.length + pendingFiles.length > 8)
      throw new Error("Attach at most eight files.");
    const id = p.id;
    uploadLock.current = true;
    setUploading(true);
    try {
      uploadRecovery.include(
        await queueUploads(p.data.stateDir, id, files, p.syncWorkspaceId || ""),
      );
      uploadRecovery.retry();
    } finally {
      uploadLock.current = false;
      setUploading(false);
    }
  };
  const submit = async (
    delivery: "after_tool" | "after_turn" = "after_tool",
  ) => {
    const draft = p.getDraft(p.id || "new");
    const draftTooLong = draft.length > 12000;
    const modelCommand = /^\/model(?:\s|$)/i.test(draft.trim());
    const exactModelCommand = /^\/model$/i.test(draft.trim());
    if (modelCommand) {
      if (!exactModelCommand) {
        p.notify("Use /model without arguments to choose a model.");
      } else if (!managed || !agent) {
        p.notify("Select a managed chat to choose a model.");
      } else {
        promptRecall.reset();
        input.current?.focus({ preventScroll: true });
        p.setDraft("");
        setModelCommandOpen(true);
        setModelCommandRequest((request) => request + 1);
      }
      return;
    }
    if (
      sendLock.current ||
      p.sending ||
      threadBlock ||
      uploading ||
      draftTooLong ||
      (!draft.trim() && !assets.length)
    )
      return;
    const attempt = Symbol();
    sendLock.current = attempt;
    const id = p.id || "";
    latestSend.current[id] = attempt;
    const release = () => {
      if (sendLock.current === attempt) sendLock.current = null;
    };
    // Sending expresses a new scroll intent. Streaming still respects a later
    // manual scroll away from the bottom.
    returnToLatest();
    input.current?.focus({ preventScroll: true });
    const sent = new Set(assets.map((asset) => asset.id));
    try {
      await p.send({
        delivery,
        assets: assets.map((asset) => asset.id),
        attachments: assets.map(({ preview: _preview, ...asset }) => asset),
        onPersist: async () => {
          await setAttachments((current) => ({
            ...current,
            [id]: (current[id] || []).filter((asset) => !sent.has(asset.id)),
          }));
          release();
        },
      });
      if (p.id && managed)
        void messageQueue.reload().catch((error) => p.notify(errorText(error)));
    } catch {
      setAttachments((current) =>
        latestSend.current[id] !== attempt
          ? current
          : {
              ...current,
              [id]: Array.from(
                new Map(
                  [...assets, ...(current[id] || [])].map((asset) => [
                    asset.id,
                    asset,
                  ]),
                ).values(),
              ),
            },
      );
      // The send owner restores the draft and renders its delivery error.
    } finally {
      release();
    }
  };
  const attachDraft = async () => {
    if (
      !p.id ||
      !managed ||
      threadBlock ||
      p.sending ||
      uploadLock.current ||
      assets.length >= 8
    )
      return;
    const id = p.id;
    const text = p.getDraft(p.id || "new");
    uploadLock.current = true;
    setUploading(true);
    try {
      uploadRecovery.include(
        await queueUploads(
          p.data.stateDir,
          id,
          [new File([text], "message.txt", { type: "text/plain" })],
          p.syncWorkspaceId || "",
        ),
      );
      uploadRecovery.retry();
      p.setDraft((current) => (current === text ? "" : current), id);
    } catch (error) {
      p.notify(errorText(error));
    } finally {
      uploadLock.current = false;
      setUploading(false);
    }
  };
  const editMessage = useMessageAction(async (message: Message) => {
    const attempt = ++editAttempt.current;
    const chat = p.id;
    setEditLoading(message.id);
    try {
      let original = message;
      if (message.truncated) {
        const full = await get("/api/transcript/item", {
          query: { id: chat || "", message_id: message.id },
          timeoutMs: 15000,
        });
        if (full.truncated || typeof full.text !== "string")
          throw new Error(
            "The full original message is unavailable. Copy the visible text into a new message instead.",
          );
        original = {
          ...message,
          ...full,
          role: full.role || message.role,
          text: typeof full.text === "string" ? full.text : message.text,
        };
      }
      if (activeId.current === chat && attempt === editAttempt.current)
        setBranchDraft({
          message: original,
          before: true,
          text: original.text,
        });
    } catch (error) {
      if (activeId.current === chat && attempt === editAttempt.current)
        p.notify(errorText(error));
    } finally {
      if (attempt === editAttempt.current) setEditLoading(null);
    }
  });
  const branch = useMessageAction(
    async (message: Message, draft?: { before: boolean; text: string }) => {
      if (branchLock.current) return;
      const agentId = p.id;
      if (!agentId) return;
      branchLock.current = true;
      const sourceId = draft?.before
        ? message.id
        : message.sourceId || message.id;
      const key = `${agentId}:${sourceId}:${draft?.before ? "before" : "after"}`;
      branchRequests.current[key] ||= crypto.randomUUID();
      setBranching(true);
      try {
        const response = await post("/api/branch", {
          agent: agentId,
          message_id: sourceId,
          id: branchRequests.current[key],
          ...(draft?.before ? { before: true } : {}),
        });
        const branchId = response.id;
        if (!branchId)
          throw new Error("The server did not return the new chat identity.");
        if (draft) {
          const reviewedText =
            draft.before &&
            draft.text === message.text &&
            typeof response.draft?.text === "string"
              ? response.draft.text
              : draft.text;
          const prefix =
            draft.before && typeof response.draft?.prefixText === "string"
              ? response.draft.prefixText
              : "";
          p.setDraft(
            prefix ? `${prefix}\n\n${reviewedText}` : reviewedText,
            branchId,
          );
          const branchAssets = response.draft?.assets;
          if (draft.before && branchAssets?.length)
            setAttachments((current) => ({
              ...current,
              [branchId]: branchAssets,
            }));
        }
        await p.refresh();
        (p.onBranchCreated || p.onSelect)?.(branchId);
        setBranchDraft(null);
        delete branchRequests.current[key];
      } catch (error) {
        p.notify(errorText(error));
      } finally {
        branchLock.current = false;
        setBranching(false);
      }
    },
  );
  const copy = useMessageAction(async (text: string) => {
    try {
      await copyText(text);
      p.notify("Copied.");
    } catch {
      p.notify("Clipboard access failed.");
    }
  });
  const quote = useMessageAction((text: string) => {
    const quoted = text
      .split(/\r?\n/)
      .map((line) => `> ${line}`)
      .join("\n");
    const draft = p.getDraft(p.id || "new");
    const separator =
      !draft || draft.endsWith("\n\n")
        ? ""
        : draft.endsWith("\n")
          ? "\n"
          : "\n\n";
    p.setDraft(`${draft}${separator}${quoted}\n\n`);
    input.current?.focus();
  });
  const team = p.data.threads.filter(
    (a) => a.rootId === (agent?.rootId || p.room?.rootId),
  );
  const runtime = p.data.runtime;
  const requests = (runtime?.requests || []).filter(
    (r) =>
      !r.agent ||
      r.agent === p.id ||
      (!mobileClient && team.some((a) => a.id === r.agent)),
  );
  const first = p.room?.members?.[0] || items.find((m) => m.sender)?.sender;
  const canSend =
    !p.schemaMismatch &&
    !threadBlock &&
    (!!agent?.canSend || !!p.legacy || !p.id);
  const lastAssistantByTurn = useMemo(() => {
    const last = new Map<string, string>();
    for (const item of items)
      if (
        item.role === "assistant" &&
        item.turnId &&
        !isEmptyAssistantMessage(item)
      )
        last.set(item.turnId, item.id);
    return last;
  }, [items]);
  const queueEntry = (m: Message) =>
    m.role === "user"
      ? queue.find(
          (event) =>
            event.id === m.clientMessageId ||
            event.id === m.id ||
            `${p.id}:${event.id}` === m.id,
        )
      : undefined;
  const sendingEntry = (m: Message) =>
    m.role === "user"
      ? sending.find(
          (event) =>
            event.id === m.clientMessageId ||
            event.id === m.id ||
            `${p.id}:${event.id}` === m.id,
        )
      : undefined;
  const transcriptItems = useMemo(
    () =>
      [
        ...items
          .filter((item) => !queueEntry(item))
          .map((item): Message =>
            applySendingOverlay(item, Boolean(sendingEntry(item))),
          ),
        ...sending
          .filter(
            (entry) => !items.some((item) => sendingEntry(item) === entry),
          )
          .map((entry): Message => ({
            id: entry.id,
            clientMessageId: entry.id,
            role: "user",
            text: entry.text,
            ...("assets" in entry ? { assets: entry.assets } : {}),
            pending: true,
            localDelivery: true,
            deliveryStatus: "sending",
          })),
      ].filter((message) => !removed.hidden(message)),
    [items, queue, sending, p.id, removed.hidden],
  );
  const [removingSending, setRemovingSending] = useState(false);
  const removeSending = useMessageAction(async (messages: Message[]) => {
    if (removingSending || !p.id) return;
    setRemovingSending(true);
    try {
      for (const message of messages)
        await removeSendingMessage(
          { stateDir: p.data.stateDir, workspaceId: p.syncWorkspaceId, kind },
          { chat: p.id, message },
        );
      void messageQueue.reload().catch(() => {});
      p.notify(
        `Removed ${messages.length} messages. Work already sent continues.`,
      );
    } catch (error) {
      p.notify(errorText(error));
    } finally {
      setRemovingSending(false);
    }
  });
  const userText = (m: Message) => <div className="prose plain">{m.text}</div>;
  const editOutgoing = useMessageAction(async (entry: OutgoingMessage) => {
    if (p.getDraft(p.id || "new").trim() || assets.length)
      throw new Error("Send or clear the current draft first.");
    await changeOutbox(entry.id, "cancel");
    p.setDraft(
      (current) =>
        current.trim() ? `${current}\n\n${entry.body.text}` : entry.body.text,
      entry.body.room,
    );
    setAttachments((current) => ({
      ...current,
      [entry.body.room]: [
        ...new Map(
          [
            ...(current[entry.body.room] || []),
            ...(entry.attachments || []),
          ].map((asset: Attachment) => [asset.id, asset]),
        ).values(),
      ],
    }));
    if (activeId.current === entry.body.room)
      input.current?.focus({ preventScroll: true });
  });
  const restoreDraft = useMessageAction((m: Message) => {
    const draft = p.getDraft(p.id || "new");
    const separator =
      draft.trim() && draft.trim() !== m.text.trim() ? "\n\n" : "";
    p.setDraft(separator ? draft + separator + m.text : m.text);
    setAttachments((current) => ({
      ...current,
      [p.id || ""]: [
        ...new Map(
          [...(current[p.id || ""] || []), ...(m.assets || [])].map(
            (asset: Attachment) => [asset.id, asset],
          ),
        ).values(),
      ],
    }));
    input.current?.focus({ preventScroll: true });
  });
  const removeMessage = useMessageAction((m: Message) => {
    try {
      removed.remove(m);
      p.notify("Message removed from this device. Delivery was not cancelled.");
    } catch (error) {
      p.notify(`Could not remove this message: ${errorText(error)}`);
    }
  });
  const renderMessage = (m: Message) => (
    <article
      key={messageRenderKey(m)}
      data-message={m.id}
      data-source-message={m.sourceId}
      data-found={highlighted === m.id || undefined}
      className={`message ${m.role === "user" ? "user" : "assistant"} ${p.room ? "bubble " + (m.sender !== first ? "outgoing" : "incoming") : ""}`}
    >
      {["sending", "reserved", "dispatching"].includes(
        m.deliveryStatus || "",
      ) && (
        <span
          className="message-delivery-status message-delivery-inline"
          role="status"
        >
          {deliveryLabel(m)}
          {m.role === "user" && (
            <ActionIcon
              size="sm"
              aria-label="Remove sending message"
              title="Remove from this device and stop retries. Work already sent continues."
              disabled={removingSending}
              onClick={() => void removeSending([m])}
            >
              <Trash2 size={14} />
            </ActionIcon>
          )}
        </span>
      )}
      {/* The main chat has two speakers; only team rooms name each sender. */}
      {p.room && (
        <span className="chat-message-author">
          <AgentAvatar
            id={m.sender || p.agent?.id || p.id || "agent"}
            size={24}
          />
        </span>
      )}
      {deliveryLabel(m) &&
        !["sending", "reserved", "dispatching", "accepted"].includes(
          m.deliveryStatus || "",
        ) && (
          <div className="message-delivery-heading">
            <span
              className="message-label message-delivery-status"
              role="status"
            >
              {deliveryLabel(m)}
            </span>
            {m.role === "user" &&
              (["uncertain", "failed", "cancelled"].includes(
                m.deliveryStatus || "",
              ) ||
                (m.localDelivery &&
                  m.deliveryStatus === "pending" &&
                  !queueEntry(m))) && (
                <ActionIcon
                  size="sm"
                  variant="subtle"
                  aria-label="Remove message"
                  title="Remove from this device. Delivery is not cancelled."
                  onClick={() => removeMessage(m)}
                >
                  <Trash2 size={14} />
                </ActionIcon>
              )}
          </div>
        )}
      {m.role === "user" ? (
        userText(m)
      ) : (
        <StreamingText
          text={serviceTimeText(
            m.text,
            m.at
              ? new Date(typeof m.at === "number" ? m.at * 1000 : m.at)
              : undefined,
          )}
          streaming={!!m.streaming}
          agentId={
            p.room
              ? p.data.threads.find((item) => item.id === m.sender)?.id
              : managed
                ? agent?.id
                : undefined
          }
        />
      )}
      {Array.isArray(m.assets) && (
        <MessageAttachments assets={m.assets} notify={p.notify} />
      )}
      {m.localDelivery && (
        <OutboxControls
          entry={p.outgoing?.find((entry) => entry.id === m.clientMessageId)}
          edit={editOutgoing}
        />
      )}
      {m.deliveryError &&
        ["failed", "uncertain", "queued"].includes(m.deliveryStatus || "") && (
          <div className="message-delivery-error">
            <p>{m.deliveryError}</p>
            {m.deliveryStatus === "failed" && (
              <Button
                size="compact-xs"
                variant="subtle"
                onClick={() => restoreDraft(m)}
              >
                Restore draft
              </Button>
            )}
          </div>
        )}
      {m.truncated && <p className="notice">This message is clipped.</p>}
      {(p.room || m.role === "user") && (
        <MessageDate at={m.at ?? m.created ?? m.timestamp} />
      )}
      <MessageActions
        hidden={!!m.streaming}
        info={
          m.role === "assistant"
            ? {
                message: m,
                agentId: !p.room && managed ? agent?.id : undefined,
                stateDir: p.data.stateDir,
                rootId:
                  !p.room && managed ? agent?.rootId || agent?.id : undefined,
                accounts: p.limitsAccounts,
                onCopy: (value) => void copy(value),
              }
            : undefined
        }
        onCopy={() => void copy(m.text)}
        onQuote={
          !p.room && !m.pending
            ? () => {
                const excerpt = selectedExcerpt(scroll.current);
                quote(excerpt?.messageId === m.id ? excerpt.text : m.text);
                window.getSelection()?.removeAllRanges();
              }
            : undefined
        }
        onEdit={
          !p.room &&
          !m.pending &&
          managed &&
          m.role === "user" &&
          m.turnId &&
          (!m.deliveryStatus || m.deliveryStatus === "accepted")
            ? () => void editMessage(m)
            : undefined
        }
        editLoading={editLoading === m.id}
        editDisabled={branching || !!editLoading || m.turnId === agent?.turnId}
        onBranch={
          !p.room &&
          !m.pending &&
          managed &&
          m.role === "assistant" &&
          m.turnId &&
          m.turnId !== agent?.turnId &&
          lastAssistantByTurn.get(m.turnId) === m.id
            ? () => void branch(m)
            : undefined
        }
        branchDisabled={m.turnId === agent?.turnId && !!agent?.inFlight}
        onAnotherAnswer={
          !p.room &&
          !m.pending &&
          managed &&
          m.role === "assistant" &&
          m.turnId &&
          m.turnId !== agent?.turnId &&
          lastAssistantByTurn.get(m.turnId) === m.id
            ? () =>
                setBranchDraft({
                  message: m,
                  before: false,
                  text: "Give another answer to my previous request. Use the existing results. Do not run tools or commands unless I explicitly ask.",
                })
            : undefined
        }
        anotherAnswerDisabled={branching}
      />
    </article>
  );
  const transcript = useMemo(
    () => (
      <TurnHistory
        key={`${p.data.stateDir}:${p.id}`}
        items={transcriptItems}
        scrollContainer={scroll}
        rememberScroll={remember}
        agent={managed && !p.room ? agent : undefined}
        currentTurn={agent?.turnId || undefined}
        enabled={managed && !p.room}
        storageKey={`studio-turns:${p.data.stateDir}:${p.id}`}
        renderMessage={(item) =>
          item.nativeNotice ? (
            isNonBlockingWarning(item) ? null : (
              <NativeNotice key={item.id} item={item} planType={planType} />
            )
          ) : (
            renderMessage(item)
          )
        }
        agentId={managed ? agent?.id : undefined}
        onJump={jumpToPrompt}
      />
    ),
    [
      items,
      agent,
      managed,
      p.room,
      p.id,
      p.data.stateDir,
      p.data.threads,
      planType,
      p.outgoing,
      p.notify,
      highlighted,
      transcriptItems,
      queue,
      branching,
      editLoading,
      first,
      lastAssistantByTurn,
      editOutgoing,
      restoreDraft,
      removeMessage,
      removeSending,
      removingSending,
      jumpToPrompt,
      editMessage,
      branch,
      copy,
      quote,
    ],
  );
  return (
    <section
      id="conversation"
      className={p.room ? "agent-conversation" : "ai-conversation"}
    >
      <Modal
        opened={!!branchDraft}
        onClose={() => {
          if (!branching) setBranchDraft(null);
        }}
        title={
          branchDraft?.before
            ? "Edit in a new chat"
            : "Another answer in a new chat"
        }
      >
        <p>
          Opens a new chat with this message as a draft. Files are not included.
        </p>
        <Textarea
          label="Draft for the new chat"
          value={branchDraft?.text || ""}
          autosize
          minRows={3}
          maxRows={10}
          disabled={branching || !!branchDraft?.message.truncated}
          onChange={(event) => {
            const text = event.currentTarget.value;
            setBranchDraft((current) =>
              current ? { ...current, text } : current,
            );
          }}
        />
        {branchDraft?.before && branchDraft.message.truncated && (
          <p>
            The preview is clipped. Create the draft to edit the full message.
          </p>
        )}
        <div className="branch-draft-actions">
          <Button
            variant="subtle"
            disabled={branching}
            onClick={() => setBranchDraft(null)}
          >
            Cancel
          </Button>
          <Button
            loading={branching}
            disabled={!branchDraft?.text.trim()}
            onClick={() => {
              if (branchDraft) void branch(branchDraft.message, branchDraft);
            }}
          >
            Create draft in new chat
          </Button>
        </div>
      </Modal>
      {agent && (agent.error || threadBlock || capacityRetry) && (
        <NativeError
          agent={agent}
          refresh={p.refresh}
          planType={planType}
          limits={p.limits}
          openLimits={() => {
            setLimitsOpen(true);
            p.reloadLimits();
          }}
          newChat={p.onNewChat}
          chooseChat={p.onChooseChat}
        />
      )}
      {headerTools &&
        agent &&
        !p.room &&
        createPortal(
          <ConversationWarnings
            key={`${p.data.stateDir}:${p.id}:${agent.accountKey || "default"}`}
            scope={`${p.data.stateDir}:${p.id}:${agent.accountKey || "default"}`}
            notices={runtime?.nativeNotices ?? []}
            accountKey={agent.accountKey || "default"}
            messages={items}
          />,
          headerTools,
        )}
      {headerTools &&
        !p.room &&
        (p.id || managed) &&
        createPortal(
          <details
            className="conversation-header-tools-menu"
            data-compact={compactHeaderTools ? "yes" : "no"}
            open={!compactHeaderTools || toolsExpanded}
            onToggle={(event) => {
              if (compactHeaderTools)
                setToolsExpanded(event.currentTarget.open);
            }}
          >
            <summary
              className="conversation-header-tools-summary"
              aria-label="Conversation tools"
              title="Conversation tools"
            >
              <ListTree size={18} />
              <span className="sr-only">Conversation tools</span>
            </summary>
            <div
              className="conversation-header-tools-content"
              style={
                compactHeaderTools && toolsExpanded && toolsMenuPosition
                  ? {
                      left: toolsMenuPosition.left,
                      top: toolsMenuPosition.top,
                      width: toolsMenuPosition.width,
                    }
                  : compactHeaderTools
                    ? { visibility: "hidden" }
                    : undefined
              }
            >
              {!p.room && p.id && (
                <div className="conversation-prompt-navigation-slot">
                  <PromptNavigator
                    compact
                    key={p.id}
                    messages={items}
                    loadOlder={
                      before
                        ? () => {
                            setFollow(false);
                            setPageAnchor(getAnchorId());
                            void older().catch((e) => p.notify(errorText(e)));
                          }
                        : undefined
                    }
                    loadingOlder={pageLoading}
                    agentId={managed ? p.id || undefined : undefined}
                    container={scroll}
                    storageKey={`studio-prompt-bookmarks:${p.data.stateDir}:${p.id}`}
                    jump={jumpToPrompt}
                  />
                </div>
              )}
            </div>
          </details>,
          headerTools,
        )}
      <div
        id="messages"
        ref={scroll}
        onScroll={() => {
          onScroll();
          if (!getFollow()) setPageAnchor(getAnchorId());
        }}
      >
        <div ref={content} className="message-content">
          {removed.hasRemoved && (
            <Button size="compact-xs" onClick={() => removed.restore()}>
              Restore removed messages
            </Button>
          )}
          {notice && <p className="notice">{notice}</p>}
          {historical && (
            <p className="historical-chat-note">
              Earlier part of this chat.
              <Button
                type="button"
                size="compact-xs"
                variant="subtle"
                onClick={returnToLatest}
              >
                Return to latest messages
              </Button>
            </p>
          )}
          {before && (
            <Button
              id="earlier-messages"
              className="transcript-page-link"
              variant="subtle"
              size="compact-sm"
              loading={pageLoading}
              onClick={() => {
                setFollow(false);
                setPageAnchor(getAnchorId());
                void older().catch((e) => p.notify(errorText(e)));
              }}
            >
              Earlier messages
            </Button>
          )}
          {loaded &&
            !items.length &&
            !notice &&
            !["running", "starting"].includes(agent?.status || "") && (
              <div className="empty-chat">
                {!p.room && (
                  <span className="empty-mark">
                    <Terminal size={28} />
                  </span>
                )}
                <h2>{p.room ? "No messages yet" : "New chat"}</h2>
                {p.room && <p>Agent messages will appear here.</p>}
              </div>
            )}
          {transcript}
          {after && (
            <Button
              id="newer-messages"
              className="transcript-page-link"
              variant="subtle"
              size="compact-sm"
              loading={pageLoading}
              onClick={() => {
                const anchorId = getAnchorId();
                setFollow(false);
                setPageAnchor(anchorId);
                void newer().catch((error) => p.notify(errorText(error)));
              }}
            >
              Later messages
            </Button>
          )}
          {!p.room && agent && (
            <SafetyBuffering key={`safety:${agent.id}`} agent={agent} />
          )}
          <div className="chat-activity">
            {!detailedActivity &&
              !reasoningVisible &&
              agent &&
              !(agent.status === "queued" && queue.length > 0) && (
                <AgentPhase
                  agent={{
                    ...agent,
                    activity: {
                      ...agent.activity,
                      phase: displayPhase,
                    },
                  }}
                  connection={connection}
                  wait={wait}
                />
              )}
          </div>
          {!p.room && detailedActivity && (
            <AgentPhase agent={agent} connection={connection} wait={wait} />
          )}
          <Requests
            onAnswerOpen={() => setFollow(false)}
            mainAgentId={
              !p.room && agent?.isLead === false
                ? agent.rootId || undefined
                : undefined
            }
            showDates={!!p.room}
            scope={p.data.stateDir}
            allRequests={runtime?.requests || []}
            requests={requests}
            agents={p.data.threads}
            refresh={p.refresh}
            notify={p.notify}
          />
        </div>
      </div>
      {!p.room && (
        <SelectionQuote
          key={`selection:${p.id}`}
          chatId={p.id}
          root={scroll}
          onQuote={quote}
        />
      )}
      <FollowLatest
        subscribe={subscribeFollow}
        getSnapshot={getFollow}
        onClick={returnToLatest}
      />
      {p.room ? (
        <p className="room-footer">
          {p.room.kind === "private"
            ? "Private agent chat."
            : "Broadcast to " +
              (p.room.rootId === "all"
                ? "all teams (historical, read only)."
                : "this team.")}
        </p>
      ) : (
        <>
          {managed && !p.legacy && agent && agent.isLead === true && (
            <AgentPanel
              key={`${p.data.stateDir}:${agent.id}`}
              agentId={agent.id}
              stateDir={p.data.stateDir}
            />
          )}
          {managed && (
            <>
              {Boolean(messageQueue.pending) && !messageQueue.busy && (
                <div className="notice queue-notice" role="status">
                  The last queue change needs confirmation.
                  <Button
                    size="compact-xs"
                    onClick={() =>
                      void messageQueue
                        .retry()
                        .catch((error) => p.notify(errorText(error)))
                    }
                  >
                    Retry queue change
                  </Button>
                </div>
              )}
              <MessageQueue
                loading={messageQueue.loading}
                items={queue}
                scope={queueScope}
                onEdit={messageQueue.edit}
                onCancel={messageQueue.cancel}
                onSendNow={messageQueue.sendNow}
                onReorder={(ids) =>
                  messageQueue.reorder(
                    mergeQueueOrder(
                      messageQueue.items.map((item) => item.id),
                      queue.map((item) => item.id),
                      ids,
                    ),
                  )
                }
                canReorder={
                  messageQueue.canReorder &&
                  !queue.some((item) => item.localDelivery)
                }
                refreshing={messageQueue.busy}
                error={messageQueue.error}
              />
            </>
          )}
          {agent && isCyberPolicyRefusal(agent.error) && (
            <div className="native-policy-composer-warning" role="note">
              <strong>Before you continue</strong>
              <span>
                Continuing this chat sends the same history and can be refused
                again.
              </span>
            </div>
          )}
          <PromptComposer
            session={p.id || "new"}
            getDraft={p.getDraft}
            subscribeDraft={p.subscribeDraft}
            className={dragging ? "attachment-drop" : ""}
            onDragOver={(event) => {
              if (managed) {
                event.preventDefault();
                setDragging(true);
              }
            }}
            onDragLeave={(event) => {
              if (!event.currentTarget.contains(event.relatedTarget as Node))
                setDragging(false);
            }}
            onDrop={(event) => {
              event.preventDefault();
              setDragging(false);
              void addFiles(Array.from(event.dataTransfer.files)).catch(
                (error) => p.notify(errorText(error)),
              );
            }}
            onSubmit={(e) => {
              e.preventDefault();
              void submit();
            }}
          >
            {(draft) => {
              const draftTooLong = draft.length > 12000;
              const modelCommand = /^\/model(?:\s|$)/i.test(draft.trim());
              const exactModelCommand = /^\/model$/i.test(draft.trim());
              return (
                <>
                  <PromptInput
                    value={draft}
                    session={p.id || "new"}
                    getDraft={p.getDraft}
                    setDraft={p.setDraft}
                    input={input}
                    mobile={mobileClient}
                    managed={!!managed}
                    canSend={canSend}
                    blocked={!!threadBlock}
                    sending={p.sending}
                    uploading={uploading}
                    draftTooLong={draftTooLong}
                    modelCommand={modelCommand}
                    hasAttachments={!!assets.length}
                    onChange={(value) => {
                      promptRecall.reset();
                      p.setDraft(value);
                    }}
                    onPasteFiles={(files) =>
                      void addFiles(files).catch((error) =>
                        p.notify(errorText(error)),
                      )
                    }
                    onSend={() => void submit()}
                    onQueue={() => void submit("after_turn")}
                    onRecallKeyDown={promptRecall.onKeyDown}
                    skillCatalog={{
                      enabled:
                        !!managed && !p.room && !!p.id && p.id === agent?.id,
                      agentId: p.id || "",
                      workspace: p.syncWorkspaceId || p.data.stateDir,
                      account: p.agent?.accountKey || "default",
                      cwd: p.agent?.cwd || "",
                      provider: String(p.agent?.provider || "codex"),
                    }}
                  />
                  {exactModelCommand && (
                    <Button
                      type="button"
                      variant="subtle"
                      size="compact-sm"
                      aria-label="Choose model with /model"
                      onClick={() => void submit()}
                    >
                      /model · Choose model and reasoning
                    </Button>
                  )}
                  {draftTooLong && (
                    <div
                      id="draft-length-error"
                      className="draft-length-error"
                      role="alert"
                    >
                      <span>
                        {draft.length.toLocaleString()} characters. The message
                        limit is 12,000. Your full draft is preserved.
                      </span>
                      {managed && (
                        <Button
                          type="button"
                          size="compact-xs"
                          variant="subtle"
                          disabled={
                            !canSend ||
                            p.sending ||
                            uploading ||
                            assets.length >= 8
                          }
                          onClick={() => void attachDraft()}
                        >
                          Attach text as a file
                        </Button>
                      )}
                    </div>
                  )}
                  <div className="composer-bar">
                    <DraftVersions
                      key={`drafts:${p.id || "new"}`}
                      versions={p.draftConflicts || []}
                      useVersion={(version, mode) =>
                        p.setDraft((current) =>
                          mode === "append" && current
                            ? `${current}\n\n${version.text}`
                            : version.text,
                        )
                      }
                      dismiss={(version) => p.dismissDraft?.(version)}
                    />
                    <div className="composer-tools">
                      {managed && (
                        <>
                          {!!pendingFiles.length && (
                            <div className="attachment-list" role="status">
                              {pendingFiles.map((file) => (
                                <div className="attachment-chip" key={file.id}>
                                  <span>
                                    {file.name}: saved on this device, waiting
                                    for upload
                                  </span>
                                  <Button
                                    size="compact-xs"
                                    onClick={() => {
                                      void uploadRecovery
                                        .remove(file.id, () => {
                                          setAttachments((current) => ({
                                            ...current,
                                            [file.agent]: (
                                              current[file.agent] || []
                                            ).filter(
                                              (asset) => asset.id !== file.id,
                                            ),
                                          }));
                                        })
                                        .catch((error) =>
                                          p.notify(errorText(error)),
                                        );
                                    }}
                                  >
                                    Remove pending {file.name}
                                  </Button>
                                </div>
                              ))}
                            </div>
                          )}
                          {uploadRecovery.error && (
                            <p role="status">
                              {uploadRecovery.error}{" "}
                              <button
                                type="button"
                                onClick={uploadRecovery.retry}
                              >
                                Retry file upload
                              </button>
                            </p>
                          )}
                          <ComposerAttachments
                            notify={p.notify}
                            assets={assets}
                            uploading={uploading}
                            disabled={!canSend || p.sending}
                            add={addFiles}
                            remove={(id) => {
                              const agentId = p.id || "";
                              void uploadRecovery
                                .remove(id, () => {
                                  setAttachments((current) => ({
                                    ...current,
                                    [agentId]: (current[agentId] || []).filter(
                                      (asset) => asset.id !== id,
                                    ),
                                  }));
                                })
                                .catch((error) => p.notify(errorText(error)));
                            }}
                          />
                        </>
                      )}
                      {managed && p.id && (
                        <Dictation
                          key={`${p.data.stateDir}:${p.id}`}
                          chatId={`${p.data.stateDir}:${p.id}`}
                          disabled={!canSend || p.sending}
                          onInsert={(text) => {
                            const next =
                              draft +
                              (draft && !draft.endsWith("\n") ? "\n" : "") +
                              text;
                            p.setDraft(next);
                          }}
                        />
                      )}
                      {managed &&
                        agent?.isLead &&
                        agent?.provider !== "claude" &&
                        p.id &&
                        !threadBlock && (
                          <RealtimeVoice
                            key={`voice:${p.id}`}
                            agentId={p.id}
                            notify={p.notify}
                          />
                        )}
                      {p.agent?.source === "managed" && !emptyMainChat && (
                        <div className="composer-context-row">
                          <UnifiedAgentSettings
                            state={p.accountsState}
                            notify={p.notify}
                            team={p.data.runtime?.agents || []}
                            // Transcript metadata can predate a settings change.
                            agent={p.agent}
                            catalog={modelCatalog}
                            refresh={p.refresh}
                            openRequest={modelCommandRequest}
                            onOpenChange={setModelCommandOpen}
                          />
                          {!items.length && (
                            <span className="composer-context-values">
                              <span>
                                {p.limitsAccountLabel ||
                                  "Account name unavailable"}
                              </span>
                              {p.agent.cwd && (
                                <span title={p.agent.cwd}>
                                  {p.data.runtime?.projects?.find(
                                    (project) => project.path === p.agent?.cwd,
                                  )?.name ||
                                    p.agent.cwd
                                      .split("/")
                                      .filter(Boolean)
                                      .pop()}
                                </span>
                              )}
                            </span>
                          )}
                        </div>
                      )}
                    </div>
                    <span id="send-state" role="status" aria-live="polite">
                      {p.sending ? "Sending…" : ""}
                    </span>
                    <div className="composer-submit-actions">
                      {managed && (
                        <span className="composer-timing">
                          Send after tools · Queue after turn
                        </span>
                      )}
                      {managed && (
                        <ActionIcon
                          type="button"
                          variant="subtle"
                          aria-label="Queue after turn"
                          title="Queue after the current turn (Tab)"
                          aria-keyshortcuts="Tab"
                          disabled={
                            !canSend ||
                            p.sending ||
                            uploading ||
                            draftTooLong ||
                            (!draft.trim() && !assets.length)
                          }
                          onClick={() => void submit("after_turn")}
                        >
                          <ListEnd size={18} />
                        </ActionIcon>
                      )}
                      {agent &&
                        ["running", "starting", "approval"].includes(
                          agent.status || "",
                        ) && (
                          <ActionIcon
                            type="button"
                            id="stop"
                            aria-label="Stop agent"
                            title={
                              ["running", "starting", "approval"].includes(
                                agent.status || "",
                              )
                                ? "Stop agent"
                                : "No active turn to stop"
                            }
                            disabled={
                              stopping ||
                              !["running", "starting", "approval"].includes(
                                agent.status || "",
                              )
                            }
                            aria-busy={stopping}
                            onClick={() => {
                              const agentId = p.id;
                              if (stopping || !agentId) return;
                              const attempt = ++stopAttempt.current;
                              setStopping(true);
                              void post("/api/stop", {
                                id: agentId,
                                descendants: false,
                              })
                                .then(p.refresh)
                                .catch((e) => p.notify(errorText(e)))
                                .finally(() => {
                                  if (stopAttempt.current === attempt)
                                    setStopping(false);
                                });
                            }}
                          >
                            {stopping ? (
                              <Loader size={14} color="currentColor" />
                            ) : (
                              <Square size={14} fill="currentColor" />
                            )}
                          </ActionIcon>
                        )}
                      <ActionIcon
                        type="submit"
                        variant="filled"
                        color="gray"
                        radius="xl"
                        id="send"
                        className="send"
                        disabled={
                          !canSend ||
                          p.sending ||
                          uploading ||
                          draftTooLong ||
                          (!draft.trim() && !assets.length)
                        }
                        aria-label="Send message"
                        title={
                          managed
                            ? "Send after tool calls (Enter)"
                            : "Send message"
                        }
                      >
                        {p.sending ? (
                          <Loader size={19} color="currentColor" />
                        ) : (
                          <ArrowUp size={19} />
                        )}
                      </ActionIcon>
                    </div>
                  </div>
                </>
              );
            }}
          </PromptComposer>
          {emptyMainChat && p.agent?.source === "managed" && (
            <div className="empty-chat-settings" aria-label="Agent settings">
              {(["orchestrator", "worker"] as const).map((role) => (
                <div className="empty-chat-setting" key={role}>
                  <span>{role === "orchestrator" ? "Model" : "Workers"}</span>
                  <UnifiedAgentSettings
                    state={p.accountsState}
                    notify={p.notify}
                    team={p.data.runtime?.agents || []}
                    agent={p.agent!}
                    catalog={modelCatalog}
                    refresh={p.refresh}
                    settingsRow
                    showAccountSummary
                    initialRole={role}
                    openRequest={
                      role === "orchestrator" ? modelCommandRequest : 0
                    }
                    onOpenChange={
                      role === "orchestrator" ? setModelCommandOpen : undefined
                    }
                  />
                </div>
              ))}
            </div>
          )}
          {agent?.source === "managed" && (
            <Usage
              key={p.agent?.accountKey || "default"}
              agent={{ ...agent, accountKey: p.agent?.accountKey || "default" }}
              stateDir={p.data.stateDir}
              limits={p.limits}
              accounts={p.limitsAccounts}
              limitsLoading={p.limitsLoading}
              accountLabel={p.limitsAccountLabel}
              reload={p.reloadLimits}
              opened={limitsOpen}
              onChange={setLimitsOpen}
            />
          )}
        </>
      )}
    </section>
  );
}
