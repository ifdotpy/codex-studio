import { ActionIcon, Button, Loader, Textarea } from "@mantine/core";
import { useMediaQuery } from "@mantine/hooks";
import {
  ArrowDown,
  ArrowUp,
  Copy,
  GitBranch,
  Quote,
  ListOrdered,
  Pencil,
  Trash2,
  ChevronUp,
  Square,
  Terminal,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { api, errorText, save, saved } from "../api";
import { useMessages } from "../hooks";
import DraftVersions from "./DraftVersions";
import type { DraftVersion } from "../sync/drafts";
import type { OutgoingMessage } from "../sync/send";
import { outgoingTranscript, deliveryLabel } from "./messageDelivery";
import { useRemovedMessages } from "./removedMessages";
import { useConversationScroll } from "./useConversationScroll";
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
import Requests from "./Requests";
import UserTasks from "./UserTasks";
import TurnHistory from "./TurnHistory";
import { Dictation } from "./Dictation";
import RealtimeVoice from "./RealtimeVoice";
import StreamingText from "./StreamingText";
import SelectionQuote, { selectedExcerpt } from "./SelectionQuote";
import AgentPhase from "./AgentPhase";
import AgentPanel from "./AgentPanel";
import ComposerAttachments, {
  MessageAttachments,
  uploadAttachment,
  type Attachment,
} from "./ComposerAttachments";
import "./chat-controls.css";
export default function Conversation(p: {
  id: string | null;
  agent?: Agent;
  room?: Room;
  legacy?: Json;
  data: Snapshot;
  draft: string;
  setDraft: (v: string) => void;
  draftConflicts?: DraftVersion[];
  dismissDraft?: (version: DraftVersion) => void;
  send: (options?: {
    assets?: string[];
    delivery?: "queue" | "steer";
    attachments?: Attachment[];
  }) => Promise<void>;
  outgoing?: OutgoingMessage[];
  onObserved?: (ids: string[]) => void;
  onOutgoingEdit?: (id: string, text: string) => void;
  onSelect?: (id: string) => void;
  sending: boolean;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
  limits: Json | null;
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
    liveAgent,
    connection,
    loaded,
  } = useMessages(p.id, kind, p.agent?.source === "managed", p.data.stateDir);
  const delivery = outgoingTranscript(
    history,
    (p.outgoing || []).filter((entry) => entry.body.room === p.id),
  );
  const removed = useRemovedMessages(p.data.stateDir, kind, p.id);
  const items = delivery.items.filter((message) => !removed.hidden(message));
  const observed = delivery.observed.join(",");
  useEffect(() => {
    if (observed) p.onObserved?.(observed.split(","));
  }, [observed, p.onObserved]);
  const { scroll, content, follow, setFollow, onScroll, remember } =
    useConversationScroll(`${p.data.stateDir}:${kind}:${p.id}`, loaded);
  const input = useRef<HTMLTextAreaElement>(null);
  const jumpToPrompt = (id: string) => {
    const root = scroll.current;
    if (!root) return;
    const chat = p.id;
    setFollow(false);
    const reveal = (attempt = 0) => {
      if (scroll.current !== root || activeId.current !== chat) return;
      const target = Array.from(
        root.querySelectorAll<HTMLElement>("[data-message]"),
      ).find((element) => element.dataset.message === id);
      if (!target) return;
      let container = target.parentElement;
      while (container && container !== root) {
        if (container instanceof HTMLDetailsElement) container.open = true;
        container = container.parentElement;
      }
      if (target.hasAttribute("data-lazy-message")) {
        // A hidden turn can contain a hidden tool group. Reveal each boundary
        // after its native toggle event mounts the next level of history.
        if (attempt < 8) requestAnimationFrame(() => reveal(attempt + 1));
        return;
      }
      root.scrollTop +=
        target.getBoundingClientRect().top -
        root.getBoundingClientRect().top -
        16;
      remember();
    };
    reveal();
  };
  const attachmentKey = `codex-agent-attachments:${p.data.stateDir}`;
  const [attachments, setAttachments] = useState<Record<string, Attachment[]>>(
    () => saved(attachmentKey, {}),
  );
  useEffect(() => {
    save(
      attachmentKey,
      Object.fromEntries(
        Object.entries(attachments).map(([id, files]) => [
          id,
          files.map(({ preview: _preview, ...asset }) => asset),
        ]),
      ),
    );
  }, [attachments, attachmentKey]);
  const [uploading, setUploading] = useState(false);
  const [queue, setQueue] = useState<Json[]>([]);
  const [queueOpen, setQueueOpen] = useState(false);
  const [editing, setEditing] = useState<string | null>(null);
  const [queuedText, setQueuedText] = useState("");
  const [queueOriginal, setQueueOriginal] = useState("");
  const [queueError, setQueueError] = useState("");
  const sendLock = useRef(false);
  const branchLock = useRef(false);
  const branchRequests = useRef<Record<string, string>>({});
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
  const agent =
    p.agent && liveAgent?.id === p.id
      ? ({ ...p.agent, ...liveAgent } as Agent)
      : p.agent;
  useEffect(() => {
    p.onPhase(
      p.id,
      connection === "reconnecting"
        ? "Reconnecting"
        : statusLabel(agent?.status || "idle", agent?.activity?.phase),
    );
  }, [p.id, agent?.status, agent?.activity?.phase, connection, p.onPhase]);
  useEffect(() => {
    setEditing(null);
    setQueueOpen(false);
  }, [p.id]);
  const managed = agent?.source === "managed";
  const canSteer = managed && !!agent?.inFlight && !!agent?.turnId;
  const loadQueue = async (id: string) => {
    const result = await api(`/api/queue?agent=${encodeURIComponent(id)}`);
    if (activeId.current === id) setQueue(result.items || []);
  };
  useEffect(() => {
    setQueue([]);
    if (!managed || !p.id) return;
    const id = p.id;
    let live = true;
    const poll = () =>
      api(`/api/queue?agent=${encodeURIComponent(id)}`)
        .then((result) => {
          if (live) {
            setQueue(result.items || []);
            setQueueError("");
          }
        })
        .catch((error) => {
          if (live) setQueueError(errorText(error));
        });
    void poll();
    const timer = setInterval(poll, 2000);
    return () => {
      live = false;
      clearInterval(timer);
    };
  }, [p.id, managed]);
  const addFiles = async (files: globalThis.File[]) => {
    if (!p.id || !managed || uploadLock.current || !files.length) return;
    if (files.length + assets.length > 8) {
      p.notify("Attach at most eight files.");
      return;
    }
    const id = p.id;
    uploadLock.current = true;
    setUploading(true);
    try {
      for (const file of files) {
        const asset = await uploadAttachment(id, file);
        setAttachments((current) => ({
          ...current,
          [id]: [...(current[id] || []), asset],
        }));
      }
    } catch (error) {
      p.notify(errorText(error));
    } finally {
      uploadLock.current = false;
      setUploading(false);
    }
  };
  const submit = async (delivery: "queue" | "steer" = "steer") => {
    if (
      sendLock.current ||
      p.sending ||
      uploading ||
      (!p.draft.trim() && !assets.length)
    )
      return;
    sendLock.current = true;
    const id = p.id || "";
    // Sending expresses a new scroll intent. Streaming still respects a later
    // manual scroll away from the bottom.
    setFollow(true);
    input.current?.focus({ preventScroll: true });
    try {
      await p.send({
        assets: assets.map((asset) => asset.id),
        attachments: assets.map(({ preview: _preview, ...asset }) => asset),
        delivery: canSteer ? delivery : "queue",
      });
      const sent = new Set(assets.map((asset) => asset.id));
      setAttachments((current) => ({
        ...current,
        [id]: (current[id] || []).filter((asset) => !sent.has(asset.id)),
      }));
      if (p.id && managed)
        void loadQueue(p.id).catch((error) => p.notify(errorText(error)));
    } catch {
      // The send owner keeps the draft and renders its delivery error.
    } finally {
      sendLock.current = false;
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
  const branch = async (message: Message) => {
    if (branchLock.current) return;
    branchLock.current = true;
    const sourceId = message.sourceId || message.id;
    const key = `${p.id}:${sourceId}`;
    branchRequests.current[key] ||= crypto.randomUUID();
    try {
      const response = await api("/api/branch", {
        agent: p.id,
        message_id: sourceId,
        id: branchRequests.current[key],
      });
      await p.refresh();
      p.onSelect?.(response.agent?.id || response.id);
      delete branchRequests.current[key];
    } catch (error) {
      p.notify(errorText(error));
    } finally {
      branchLock.current = false;
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
  const canSend = !!agent?.canSend || !!p.legacy || !p.id;
  const lastAssistantByTurn = useMemo(() => {
    const last = new Map<string, string>();
    for (const item of items)
      if (item.role === "assistant" && item.turnId)
        last.set(item.turnId, item.id);
    return last;
  }, [items]);
  const renderMessage = (m: Message) => (
    <article
      key={m.id}
      data-message={m.id}
      className={`message ${m.role === "user" ? "user" : "assistant"} ${p.room ? "bubble " + (m.sender !== first ? "outgoing" : "incoming") : ""}`}
    >
      {p.room && m.senderName && (
        <span className="message-label">{m.senderName}</span>
      )}
      {deliveryLabel(m) && (
        <div className="message-delivery-heading">
          <span className="message-label message-delivery-status" role="status">
            {deliveryLabel(m)}
          </span>
          {m.role === "user" &&
            ["uncertain", "failed", "cancelled"].includes(
              m.deliveryStatus || "",
            ) && (
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
        <div className="prose plain">{m.text}</div>
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
      {agent?.error && (
        <p className="agent-error" role="alert">
          {typeof agent.error === "string"
            ? agent.error
            : JSON.stringify(agent.error)}
        </p>
      )}
      <div className="prompt-navigation-slot">
        {!p.room && p.id && (
          <PromptNavigator
            key={p.id}
            messages={items}
            container={scroll}
            storageKey={`studio-prompt-bookmarks:${p.data.stateDir}:${p.id}`}
            jump={jumpToPrompt}
          />
        )}
      </div>
      <div id="messages" ref={scroll} onScroll={onScroll}>
        <div ref={content} className="message-content">
          {notice && <p className="notice">{notice}</p>}
          {before && (
            <Button
              id="earlier-messages"
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
            currentTurn={agent?.turnId}
            enabled={managed && !p.room}
            storageKey={`studio-turns:${p.data.stateDir}:${p.id}`}
            renderMessage={renderMessage}
            agentId={managed ? agent?.id : undefined}
            onJump={jumpToPrompt}
          />
          {!p.room && <AgentPhase agent={agent} connection={connection} />}
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
            onClick={() => setFollow(true)}
          >
            Latest
          </Button>
        </div>
      )}
      {agent?.source === "managed" && !p.room && (
        <UserTasks
          key={agent.rootId || agent.id}
          data={
            mobileClient
              ? {
                  ...p.data,
                  runtime: {
                    ...p.data.runtime,
                    userTasks: p.data.runtime.userTasks?.filter(
                      (task) => task.agent === agent.id,
                    ),
                  },
                }
              : p.data
          }
          agent={agent}
          compact
          refresh={p.refresh}
          notify={p.notify}
          onSelect={p.onSelect}
        />
      )}
      <Requests
        scopeAgentId={managed && !p.room ? agent?.id : undefined}
        requests={requests}
        agents={p.data.threads}
        refresh={p.refresh}
        notify={p.notify}
      />
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
          {queue.length > 0 && (
            <div className="message-queue">
              <Button
                variant="subtle"
                size="compact-xs"
                leftSection={<ListOrdered size={14} />}
                onClick={() => setQueueOpen(!queueOpen)}
                aria-expanded={queueOpen}
                title="These messages start a new turn after the current turn ends."
              >
                {queue.length} queued{" "}
                {queue.length === 1 ? "message" : "messages"}
              </Button>
              {queueOpen && (
                <div className="queued-items">
                  {queue.map((event, index) => (
                    <div key={event.id} className="queued-item">
                      <span className="queue-position">{index + 1}</span>
                      {editing === event.id ? (
                        <div className="queue-editor">
                          <Textarea
                            aria-label="Edit queued message"
                            value={queuedText}
                            onChange={(e) => setQueuedText(e.target.value)}
                            autosize
                            maxRows={5}
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
                      ) : (
                        <p>{event.text || "Attachments"}</p>
                      )}
                      <div className="queue-actions">
                        <ActionIcon
                          size="sm"
                          aria-label="Move message first"
                          disabled={index === 0}
                          onClick={() => void queueAction(event, "first")}
                        >
                          <ChevronUp size={14} />
                        </ActionIcon>
                        <ActionIcon
                          size="sm"
                          aria-label="Edit queued message"
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
                          onClick={() => void queueAction(event, "cancel")}
                        >
                          <Trash2 size={13} />
                        </ActionIcon>
                      </div>
                    </div>
                  ))}
                </div>
              )}
            </div>
          )}
          {!mobileClient && managed && !p.legacy && agent && (
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
              void addFiles(Array.from(event.dataTransfer.files));
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
                  void addFiles(files);
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
                    ? "Enter sends after tool calls. Tab queues after the turn. Shift + Enter adds a new line."
                    : "Enter to send. Shift + Enter for a new line."
              }
              placeholder={
                canSend
                  ? "What should we work on?"
                  : "This session has no live mailbox"
              }
              disabled={!canSend}
              value={p.draft}
              onChange={(e) => p.setDraft(e.target.value)}
              maxLength={12000}
              rows={1}
              onKeyDown={(e) => {
                if (
                  managed &&
                  canSend &&
                  e.key === "Tab" &&
                  !e.shiftKey &&
                  !e.ctrlKey &&
                  !e.metaKey &&
                  !e.altKey &&
                  !e.nativeEvent.isComposing &&
                  (p.draft.trim() || assets.length)
                ) {
                  e.preventDefault();
                  void submit("queue");
                  return;
                }
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
            <div className="composer-bar">
              <DraftVersions
                key={p.id || "new"}
                versions={p.draftConflicts || []}
                useVersion={(version) => p.setDraft(version.text)}
                dismiss={(version) => p.dismissDraft?.(version)}
              />
              {managed && (
                <ComposerAttachments
                  notify={p.notify}
                  assets={assets}
                  uploading={uploading}
                  disabled={!canSend || p.sending}
                  add={(files) => void addFiles(files)}
                  remove={(id) =>
                    setAttachments((current) => ({
                      ...current,
                      [p.id || ""]: assets.filter((asset) => asset.id !== id),
                    }))
                  }
                />
              )}
              {!mobileClient && managed && p.id && (
                <Dictation
                  key={`${p.data.stateDir}:${p.id}`}
                  chatId={`${p.data.stateDir}:${p.id}`}
                  disabled={!canSend || p.sending}
                  onInsert={(text) => {
                    const next =
                      p.draft +
                      (p.draft && !p.draft.endsWith("\n") ? "\n" : "") +
                      text;
                    if (next.length > 12000) {
                      p.notify(
                        "The message exceeds 12,000 characters. Your transcript remains in Dictation.",
                      );
                      return;
                    }
                    p.setDraft(next);
                  }}
                />
              )}
              {managed && agent?.isLead && p.id && (
                <RealtimeVoice key={p.id} agentId={p.id} notify={p.notify} />
              )}
              <span id="send-state" role="status" aria-live="polite">
                {p.sending ? "Sending…" : ""}
              </span>
              <div className="composer-submit-actions">
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
                    (!p.draft.trim() && !assets.length)
                  }
                  aria-label="Send message"
                  title={
                    managed
                      ? "Send after tool calls (Enter). Queue after turn (Tab)."
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
          </form>
          {!mobileClient && agent?.source === "managed" && (
            <Usage
              key={p.agent?.accountKey || "default"}
              agent={{ ...agent, accountKey: p.agent?.accountKey || "default" }}
              limits={p.limits}
              reload={p.reloadLimits}
            />
          )}
        </>
      )}
    </section>
  );
}
