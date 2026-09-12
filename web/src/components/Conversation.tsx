import { onResume } from "../sync/resume";
import { displayError } from "../errorPresentation";
import {
  NativeError,
  NativeNotice,
  NativeAccountNotices,
} from "./NativeNotice";
import { ActionIcon, Button, Loader, Modal, Textarea } from "@mantine/core";
import { useMediaQuery } from "@mantine/hooks";
import {
  ArrowDown,
  ArrowUp,
  Copy,
  GitBranch,
  Quote,
  Pencil,
  Trash2,
  ChevronUp,
  Square,
  Terminal,
  ListEnd,
  RotateCcw,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { api, errorText, save, saved } from "../api";
import SafetyBuffering from "./SafetyBuffering";
import { currentCapacityRetry } from "../capacityRetry";
import { nativeErrorKind, nativeThreadError } from "../nativeErrors";
import { useMessages } from "../hooks";
import DraftVersions from "./DraftVersions";
import type { DraftVersion } from "../sync/drafts";
import { changeOutbox, type OutgoingMessage } from "../sync/send";
import { useDisplayPhase } from "./useDisplayPhase";
import {
  outgoingTranscript,
  deliveryLabel,
  messageRenderKey,
} from "./messageDelivery";
import { useRemovedMessages } from "./removedMessages";
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
import PromptNavigator from "./PromptNavigator";
import { usePromptRecall } from "./usePromptRecall";
import Requests from "./Requests";
import UserTasks from "./UserTasks";
import TurnHistory from "./TurnHistory";
import { Dictation } from "./Dictation";
import RealtimeVoice from "./RealtimeVoice";
import OutboxControls from "./OutboxControls";
import StreamingText from "./StreamingText";
import SelectionQuote, { selectedExcerpt } from "./SelectionQuote";
import AgentPhase from "./AgentPhase";
import AgentPanel from "./AgentPanel";
import ComposerAttachments, {
  MessageAttachments,
  type Attachment,
} from "./ComposerAttachments";
import "./chat-controls.css";
export default function Conversation(p: {
  id: string | null;
  syncWorkspaceId?: string;
  agent?: Agent;
  room?: Room;
  legacy?: Json;
  data: Snapshot;
  draft: string;
  setDraft: (v: string | ((current: string) => string), id?: string) => void;
  draftConflicts?: DraftVersion[];
  dismissDraft?: (version: DraftVersion) => void;
  send: (options?: {
    assets?: string[];
    delivery?: "queue" | "steer" | "after_tool";
    attachments?: Attachment[];
    onPersist?: () => void | Promise<void>;
  }) => Promise<void>;
  outgoing?: OutgoingMessage[];
  onObserved?: (ids: string[]) => void;
  onOutgoingEdit?: (id: string, text: string) => void;
  onSelect?: (id: string) => void;
  onBranchCreated?: (id: string) => void;
  onNewChat?: () => void;
  onChooseChat?: () => void;
  sending: boolean;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
  limits: Json | null;
  limitsLoading?: boolean;
  jumpTarget?: { messageId: string; requestId: string };
  onJumpHandled?: (requestId: string) => void;
  limitsAccountLabel?: string;
  reloadLimits: () => void;
  onPhase: (id: string | null, label: string) => void;
}) {
  const mobileClient = useMediaQuery("(max-width: 760px)");
  const shortViewport = useMediaQuery(
    "(max-width: 760px) and (max-height: 750px)",
  );
  const kind = p.room ? "room" : p.legacy ? "legacy" : "agent";
  const {
    items: history,
    notice,
    before,
    older,
    newer,
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
  const delivery = outgoingTranscript(
    history,
    (p.outgoing || []).filter((entry) => entry.body.room === p.id),
  );
  const removed = useRemovedMessages(p.data.stateDir, kind, p.id);
  const items = delivery.items.filter((message) => !removed.hidden(message));
  const promptRecall = usePromptRecall(
    `${p.data.stateDir}:${kind}:${p.id}`,
    p.room ? [] : items,
    p.draft,
    p.setDraft,
  );
  const observed = delivery.observed.join(",");
  useEffect(() => {
    if (observed) p.onObserved?.(observed.split(","));
  }, [observed, p.onObserved]);
  const { scroll, content, follow, setFollow, onScroll, remember } =
    useConversationScroll(`${p.data.stateDir}:${kind}:${p.id}`, loaded);
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
  const jumpToPrompt = (id: string) => {
    void navigateToMessage(id).catch((error) => p.notify(errorText(error)));
  };
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
  const [queue, setQueue] = useState<Json[]>([]);
  const [limitsOpen, setLimitsOpen] = useState(false);
  const refreshedFailure = useRef("");
  const [editing, setEditing] = useState<string | null>(null);
  const [queuedText, setQueuedText] = useState("");
  const [queueOriginal, setQueueOriginal] = useState("");
  const [queueError, setQueueError] = useState("");
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
  const draftTooLong = p.draft.length > 12000;
  const agent =
    p.agent && liveAgent?.id === p.id
      ? ({ ...p.agent, ...liveAgent } as Agent)
      : p.agent;
  const threadBlock = nativeThreadError(agent);
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
            : displayError(agent?.nativeStatus?.error?.message) ||
              displayError(agent?.nativeStatus?.message) ||
              (["starting", "running"].includes(agent?.status || "")
                ? "Working"
                : statusLabel(agent?.status || "idle")),
    );
  }, [
    p.id,
    agent?.status,
    agent?.activity?.phase,
    agent?.nativeStatus?.error?.message,
    agent?.nativeStatus?.message,
    threadBlock,
    capacityRetry?.status,
    connection,
    p.onPhase,
  ]);
  useEffect(() => {
    setEditing(null);
    setBranchDraft(null);
    editAttempt.current++;
    setEditLoading(null);
    setHighlighted(null);
    setLimitsOpen(false);
  }, [p.id]);
  const managed = agent?.source === "managed";
  const loadQueue = async (id: string) => {
    const result = await api(`/api/queue?agent=${encodeURIComponent(id)}`);
    if (activeId.current === id) setQueue(result.items || []);
  };
  useEffect(() => {
    setQueue([]);
    if (!managed || !p.id) return;
    const id = p.id;
    let live = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let polling = false;
    const poll = async () => {
      if (!live || polling) return;
      clearTimeout(timer);
      if (document.hidden || navigator.onLine === false) return;
      polling = true;
      try {
        const result = await api(`/api/queue?agent=${encodeURIComponent(id)}`);
        if (live) {
          setQueue(result.items || []);
          setQueueError("");
        }
      } catch (error) {
        if (live) setQueueError(errorText(error));
      } finally {
        polling = false;
        if (live) timer = setTimeout(poll, 5000);
      }
    };
    void poll();
    const stopResume = onResume(() => void poll());
    return () => {
      live = false;
      clearTimeout(timer);
      stopResume();
    };
  }, [p.id, managed]);
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
  const submit = async (delivery: "queue" | "after_tool" = "after_tool") => {
    if (
      sendLock.current ||
      p.sending ||
      threadBlock ||
      uploading ||
      draftTooLong ||
      (!p.draft.trim() && !assets.length)
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
        assets: assets.map((asset) => asset.id),
        attachments: assets.map(({ preview: _preview, ...asset }) => asset),
        delivery,
        onPersist: async () => {
          await setAttachments((current) => ({
            ...current,
            [id]: (current[id] || []).filter((asset) => !sent.has(asset.id)),
          }));
          release();
        },
      });
      if (p.id && managed)
        void loadQueue(p.id).catch((error) => p.notify(errorText(error)));
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
    const text = p.draft;
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
  const queueAction = async (event: Json, action: string) => {
    try {
      await api("/api/queue", {
        agent: p.id,
        id: event.id,
        action,
        text: queuedText,
        expectedText: action === "edit" ? queueOriginal : event.text,
      });
      if (action === "cancel") p.onObserved?.([event.id]);
      if (action === "edit") p.onOutgoingEdit?.(event.id, queuedText);
      setEditing(null);
      if (p.id) await loadQueue(p.id);
      await p.refresh();
    } catch (error) {
      p.notify(errorText(error));
    }
  };
  const editMessage = async (message: Message) => {
    const attempt = ++editAttempt.current;
    const chat = p.id;
    setEditLoading(message.id);
    try {
      let original = message;
      if (message.truncated) {
        const query = new URLSearchParams({
          id: chat || "",
          message_id: message.id,
        });
        const full = await api(`/api/transcript/item?${query}`, undefined, {
          timeoutMs: 15000,
        });
        if (full.truncated || typeof full.text !== "string")
          throw new Error(
            "The full original message is unavailable. Copy the visible text into a new message instead.",
          );
        original = { ...message, ...full };
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
  };
  const branch = async (
    message: Message,
    draft?: { before: boolean; text: string },
  ) => {
    if (branchLock.current) return;
    branchLock.current = true;
    const sourceId = draft?.before
      ? message.id
      : message.sourceId || message.id;
    const key = `${p.id}:${sourceId}:${draft?.before ? "before" : "after"}`;
    branchRequests.current[key] ||= crypto.randomUUID();
    setBranching(true);
    try {
      const response = await api("/api/branch", {
        agent: p.id,
        message_id: sourceId,
        id: branchRequests.current[key],
        ...(draft?.before ? { before: true } : {}),
      });
      const branchId = response.agent?.id || response.id;
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
        if (draft.before && response.draft?.assets?.length)
          setAttachments((current) => ({
            ...current,
            [branchId]: response.draft.assets,
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
  };
  const copy = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      p.notify("Copied.");
    } catch {
      p.notify("Clipboard access failed.");
    }
  };
  const quote = (text: string) => {
    const quoted = text
      .split(/\r?\n/)
      .map((line) => `> ${line}`)
      .join("\n");
    const separator =
      !p.draft || p.draft.endsWith("\n\n")
        ? ""
        : p.draft.endsWith("\n")
          ? "\n"
          : "\n\n";
    p.setDraft(`${p.draft}${separator}${quoted}\n\n`);
    input.current?.focus();
  };
  const team = p.data.threads.filter(
    (a) => a.rootId === (agent?.rootId || p.room?.rootId),
  );
  const requests = p.data.runtime.requests.filter(
    (r) =>
      !r.agent ||
      r.agent === p.id ||
      (!mobileClient && team.some((a) => a.id === r.agent)),
  );
  const first = p.room?.members[0] || items.find((m) => m.sender)?.sender;
  const canSend = !threadBlock && (!!agent?.canSend || !!p.legacy || !p.id);
  const lastAssistantByTurn = useMemo(() => {
    const last = new Map<string, string>();
    for (const item of items)
      if (item.role === "assistant" && item.turnId)
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
  const queueControls = (m: Message) => {
    const event = queueEntry(m);
    if (!event || editing === event.id) return null;
    const index = queue.findIndex((item) => item.id === event.id);
    return (
      <div className="inline-queue-actions" data-queue-position={index + 1}>
        {queue.length > 1 && index > 0 && (
          <ActionIcon
            size="sm"
            aria-label="Move message first"
            title="Move message first"
            onClick={() => void queueAction(event, "first")}
          >
            <ChevronUp size={14} />
          </ActionIcon>
        )}
        <ActionIcon
          size="sm"
          aria-label="Edit queued message"
          title="Edit queued message"
          onClick={() => {
            setEditing(event.id);
            setQueuedText(event.text);
            setQueueOriginal(event.text);
          }}
        >
          <Pencil size={13} />
        </ActionIcon>
        <ActionIcon
          size="sm"
          aria-label="Cancel queued message"
          title="Cancel queued message"
          onClick={() => void queueAction(event, "cancel")}
        >
          <Trash2 size={13} />
        </ActionIcon>
      </div>
    );
  };
  const userText = (m: Message) => {
    const event = queueEntry(m);
    if (!event || editing !== event.id)
      return <div className="prose plain">{m.text}</div>;
    return (
      <div className="queue-editor">
        <Textarea
          aria-label="Edit queued message"
          value={queuedText}
          onChange={(e) => setQueuedText(e.target.value)}
          autosize
          maxRows={5}
          autoFocus
        />
        <Button
          size="compact-xs"
          disabled={!queuedText.trim()}
          onClick={() => void queueAction(event, "edit")}
        >
          Save
        </Button>
        <Button
          size="compact-xs"
          variant="subtle"
          onClick={() => setEditing(null)}
        >
          Cancel
        </Button>
      </div>
    );
  };
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
          Sending…
        </span>
      )}
      {p.room && m.senderName && (
        <span className="message-label">{m.senderName}</span>
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
            {queueControls(m)}
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
                  onClick={() => {
                    try {
                      removed.remove(m);
                      p.notify(
                        "Message removed from this device. Delivery was not cancelled.",
                      );
                    } catch (error) {
                      p.notify(
                        `Could not remove this message: ${errorText(error)}`,
                      );
                    }
                  }}
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
          text={m.text}
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
          edit={async (entry) => {
            if (p.draft.trim() || assets.length)
              throw new Error("Send or clear the current draft first.");
            await changeOutbox(entry.id, "cancel");
            p.setDraft(
              (current) =>
                current.trim()
                  ? `${current}\n\n${entry.body.text}`
                  : entry.body.text,
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
          }}
        />
      )}
      {m.deliveryError &&
        ["failed", "uncertain", "queued"].includes(m.deliveryStatus) && (
          <div className="message-delivery-error">
            <p>{m.deliveryError}</p>
            {m.deliveryStatus === "failed" && (
              <Button
                size="compact-xs"
                variant="subtle"
                onClick={() => {
                  const separator =
                    p.draft.trim() && p.draft.trim() !== m.text.trim()
                      ? "\n\n"
                      : "";
                  p.setDraft(separator ? p.draft + separator + m.text : m.text);
                  setAttachments((current) => ({
                    ...current,
                    [p.id || ""]: [
                      ...new Map(
                        [
                          ...(current[p.id || ""] || []),
                          ...(m.assets || []),
                        ].map((asset: Attachment) => [asset.id, asset]),
                      ).values(),
                    ],
                  }));
                  input.current?.focus({ preventScroll: true });
                }}
              >
                Restore draft
              </Button>
            )}
          </div>
        )}
      {m.truncated && <p className="notice">This message is clipped.</p>}
      <div className="message-bottom" hidden={!!m.streaming}>
        {p.room && m.created && (
          <time>
            {new Date(m.created * 1000).toLocaleTimeString([], {
              hour: "2-digit",
              minute: "2-digit",
            })}
          </time>
        )}
        <ActionIcon
          size="sm"
          className="copy-message"
          aria-label="Copy message"
          onClick={() => void copy(m.text)}
        >
          <Copy size={14} />
        </ActionIcon>
        {!p.room && !m.pending && (
          <>
            <ActionIcon
              size="sm"
              aria-label="Quote message"
              onPointerDown={(event) => event.preventDefault()}
              onClick={() => {
                const excerpt = selectedExcerpt(scroll.current);
                quote(excerpt?.messageId === m.id ? excerpt.text : m.text);
                window.getSelection()?.removeAllRanges();
              }}
            >
              <Quote size={14} />
            </ActionIcon>
            {managed &&
              m.role === "user" &&
              m.turnId &&
              (!m.deliveryStatus || m.deliveryStatus === "accepted") && (
                <ActionIcon
                  size="sm"
                  aria-label="Edit in a new chat"
                  title="Edit in a new chat"
                  disabled={
                    branching || !!editLoading || m.turnId === agent?.turnId
                  }
                  onClick={() => void editMessage(m)}
                >
                  {editLoading === m.id ? (
                    <Loader size={14} />
                  ) : (
                    <Pencil size={14} />
                  )}
                </ActionIcon>
              )}
            {managed &&
              m.role === "assistant" &&
              m.turnId &&
              m.turnId !== agent?.turnId &&
              lastAssistantByTurn.get(m.turnId) === m.id && (
                <ActionIcon
                  size="sm"
                  aria-label="Branch after this turn"
                  disabled={m.turnId === agent?.turnId && !!agent?.inFlight}
                  onClick={() => void branch(m)}
                >
                  <GitBranch size={14} />
                </ActionIcon>
              )}
            {managed &&
              m.role === "assistant" &&
              m.turnId &&
              m.turnId !== agent?.turnId &&
              lastAssistantByTurn.get(m.turnId) === m.id && (
                <ActionIcon
                  size="sm"
                  aria-label="Another answer in a new chat"
                  title="Another answer in a new chat"
                  disabled={branching}
                  onClick={() =>
                    setBranchDraft({
                      message: m,
                      before: false,
                      text: "Give another answer to my previous request. Use the existing results. Do not run tools or commands unless I explicitly ask.",
                    })
                  }
                >
                  <RotateCcw size={14} />
                </ActionIcon>
              )}
          </>
        )}
      </div>
    </article>
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
          This creates a new chat with a draft. Review the draft before you send
          it. Your files are not restored.
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
          planType={p.limits?.data?.rateLimits?.planType}
          limits={p.limits}
          openLimits={() => {
            setLimitsOpen(true);
            p.reloadLimits();
          }}
          newChat={p.onNewChat}
          chooseChat={p.onChooseChat}
        />
      )}
      {agent && (
        <NativeAccountNotices
          notices={p.data.runtime.nativeNotices}
          accountKey={agent.accountKey || "default"}
        />
      )}
      <div className="conversation-navigation">
        {!p.room && p.id && (
          <PromptNavigator
            compact
            key={p.id}
            messages={items}
            agentId={managed ? p.id || undefined : undefined}
            container={scroll}
            storageKey={`studio-prompt-bookmarks:${p.data.stateDir}:${p.id}`}
            jump={jumpToPrompt}
          />
        )}
      </div>
      <div id="messages" ref={scroll} onScroll={onScroll}>
        <div ref={content} className="message-content">
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
              loading={pageLoading}
              onClick={() => {
                setFollow(false);
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
                <h2>
                  {p.room ? "No messages yet" : "What should we work on?"}
                </h2>
                <p>
                  {p.room
                    ? "Agent messages will appear here."
                    : "Give your lead a task. It can delegate work and report the results here."}
                </p>
              </div>
            )}
          <TurnHistory
            key={`${p.data.stateDir}:${p.id}`}
            items={items}
            agent={managed && !p.room ? agent : undefined}
            currentTurn={agent?.turnId}
            enabled={managed && !p.room}
            storageKey={`studio-turns:${p.data.stateDir}:${p.id}`}
            renderMessage={(item) =>
              item.nativeNotice ? (
                <NativeNotice
                  key={item.id}
                  item={item}
                  planType={p.limits?.data?.rateLimits?.planType}
                />
              ) : (
                renderMessage(item)
              )
            }
            agentId={managed ? agent?.id : undefined}
            onJump={jumpToPrompt}
          />
          {after && (
            <Button
              id="newer-messages"
              loading={pageLoading}
              onClick={() =>
                void newer().catch((error) => p.notify(errorText(error)))
              }
            >
              Later messages
            </Button>
          )}
          {!p.room && agent && (
            <SafetyBuffering key={`safety:${agent.id}`} agent={agent} />
          )}
          {!p.room && detailedActivity && (
            <AgentPhase agent={agent} connection={connection} />
          )}
          {mobileClient && agent?.source === "managed" && !p.room && (
            <UserTasks
              key={`tasks:${agent.rootId || agent.id}`}
              data={{
                ...p.data,
                runtime: {
                  ...p.data.runtime,
                  userTasks: p.data.runtime.userTasks?.filter(
                    (task) => task.agent === agent.id,
                  ),
                },
              }}
              agent={agent}
              compact
              refresh={p.refresh}
              notify={p.notify}
              onSelect={p.onSelect}
            />
          )}
          <Requests
            scope={p.data.stateDir}
            allRequests={p.data.runtime.requests}
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
      {!follow && (
        <div className="jump-slot">
          <Button
            id="jump-latest"
            className="jump"
            variant="default"
            radius="xl"
            leftSection={<ArrowDown size={14} />}
            onClick={returnToLatest}
          >
            Latest
          </Button>
        </div>
      )}
      {p.room ? (
        <p className="room-footer">
          {p.room.kind === "private"
            ? "Private between participants. Visible to you."
            : "Broadcast to " +
              (p.room.rootId === "all" ? "all teams." : "this team.")}
        </p>
      ) : (
        <>
          {queueError && (
            <p className="notice queue-notice">
              Message queue unavailable: {queueError}
            </p>
          )}
          {managed && !p.legacy && agent && (
            <AgentPanel
              key={agent.id}
              agentId={agent.id}
              token={p.data.token}
              dataVersion={Math.max(
                p.agent?.panelDataVersion || 0,
                agent.panelDataVersion || 0,
              )}
              version={Math.max(
                p.agent?.panelVersion || 0,
                agent.panelVersion || 0,
              )}
            />
          )}
          <form
            id="composer"
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
            <Textarea
              onPaste={(event) => {
                const files = Array.from(event.clipboardData.files);
                if (managed && files.length) {
                  event.preventDefault();
                  void addFiles(files).catch((error) =>
                    p.notify(errorText(error)),
                  );
                }
              }}
              variant="unstyled"
              autosize
              minRows={1}
              maxRows={shortViewport ? 3 : 8}
              id="message"
              ref={input}
              aria-label="Message"
              aria-description={
                mobileClient
                  ? "Use the send button to send."
                  : managed
                    ? "Enter sends after tool calls. Use Queue after turn to wait for the current turn. Shift + Enter adds a new line."
                    : "Enter to send. Shift + Enter for a new line."
              }
              placeholder={
                threadBlock
                  ? "Start a new chat or open another chat."
                  : canSend
                    ? "What should we work on?"
                    : "This session has no live mailbox"
              }
              disabled={!canSend}
              value={p.draft}
              onChange={(e) => {
                promptRecall.reset();
                p.setDraft(e.target.value);
              }}
              error={draftTooLong}
              aria-describedby={draftTooLong ? "draft-length-error" : undefined}
              rows={1}
              onKeyDown={(e) => {
                if (promptRecall.onKeyDown(e)) return;
                if (
                  !mobileClient &&
                  e.key === "Enter" &&
                  !e.shiftKey &&
                  !e.nativeEvent.isComposing
                ) {
                  e.preventDefault();
                  void submit();
                }
              }}
            />
            {draftTooLong && (
              <div
                id="draft-length-error"
                className="draft-length-error"
                role="alert"
              >
                <span>
                  {p.draft.length.toLocaleString()} characters. The message
                  limit is 12,000. Your full draft is preserved.
                </span>
                {managed && (
                  <Button
                    type="button"
                    size="compact-xs"
                    variant="subtle"
                    disabled={
                      !canSend || p.sending || uploading || assets.length >= 8
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
              {managed && (
                <>
                  {!!pendingFiles.length && (
                    <div className="attachment-list" role="status">
                      {pendingFiles.map((file) => (
                        <div className="attachment-chip" key={file.id}>
                          <span>
                            {file.name}: saved on this device, waiting for
                            upload
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
                                    ).filter((asset) => asset.id !== file.id),
                                  }));
                                })
                                .catch((error) => p.notify(errorText(error)));
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
                      <button type="button" onClick={uploadRecovery.retry}>
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
                      p.draft +
                      (p.draft && !p.draft.endsWith("\n") ? "\n" : "") +
                      text;
                    p.setDraft(next);
                  }}
                />
              )}
              {managed && agent?.isLead && p.id && !threadBlock && (
                <RealtimeVoice
                  key={`voice:${p.id}`}
                  agentId={p.id}
                  notify={p.notify}
                />
              )}
              <span id="send-state" role="status" aria-live="polite">
                {p.sending ? "Sending…" : ""}
              </span>
              <div className="composer-activity">
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
                    />
                  )}
              </div>
              <div className="composer-submit-actions">
                {managed && (
                  <ActionIcon
                    type="button"
                    variant="subtle"
                    aria-label="Queue after turn"
                    title="Queue after the current turn"
                    disabled={
                      !canSend ||
                      p.sending ||
                      uploading ||
                      draftTooLong ||
                      (!p.draft.trim() && !assets.length)
                    }
                    onClick={() => void submit("queue")}
                  >
                    <ListEnd size={18} />
                  </ActionIcon>
                )}
                {agent && (
                  <ActionIcon
                    type="button"
                    id="stop"
                    aria-label="Stop agent"
                    title={
                      ["running", "starting", "approval"].includes(agent.status)
                        ? "Stop agent"
                        : "No active turn to stop"
                    }
                    disabled={
                      stopping ||
                      !["running", "starting", "approval"].includes(
                        agent.status,
                      )
                    }
                    aria-busy={stopping}
                    onClick={() => {
                      if (stopping) return;
                      const attempt = ++stopAttempt.current;
                      setStopping(true);
                      void api("/api/stop", { id: p.id, descendants: false })
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
                    (!p.draft.trim() && !assets.length)
                  }
                  aria-label="Send message"
                  title={
                    managed ? "Send after tool calls (Enter)" : "Send message"
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
          </form>
          {agent?.source === "managed" && (
            <Usage
              key={p.agent?.accountKey || "default"}
              agent={{ ...agent, accountKey: p.agent?.accountKey || "default" }}
              limits={p.limits}
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
