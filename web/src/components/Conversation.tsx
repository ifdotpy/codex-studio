import { ActionIcon, Button, Menu, Textarea } from "@mantine/core";
import {
  ArrowDown,
  ArrowUp,
  Copy,
  GitBranch,
  Quote,
  ListOrdered,
  Zap,
  Pencil,
  Trash2,
  ChevronUp,
  Folder,
  Square,
  Terminal,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api, errorText, save, saved } from "../api";
import { useMessages } from "../hooks";
import {
  statusLabel,
  type Agent,
  type Json,
  type Message,
  type Room,
  type Snapshot,
} from "../types";
import Usage from "./Usage";
import Requests from "./Requests";
import UserTasks from "./UserTasks";
import Activity from "./Activity";
import StreamingText from "./StreamingText";
import SelectionQuote, { selectedExcerpt } from "./SelectionQuote";
import AgentPhase from "./AgentPhase";
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
  send: (options?: {
    assets?: string[];
    delivery?: "queue" | "steer";
  }) => Promise<void>;
  onSelect?: (id: string) => void;
  sending: boolean;
  refresh: () => Promise<void>;
  notify: (s: string) => void;
  project: () => void;
  limits: Json | null;
  reloadLimits: () => void;
  onPhase: (id: string | null, label: string) => void;
}) {
  const kind = p.room ? "room" : p.legacy ? "legacy" : "agent";
  const { items, notice, before, older, liveAgent, connection } = useMessages(
      p.id,
      kind,
      p.agent?.source === "managed",
    ),
    [follow, setFollow] = useState(true),
    scroll = useRef<HTMLDivElement>(null),
    input = useRef<HTMLTextAreaElement>(null);
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
  const [delivery, setDelivery] = useState<"queue" | "steer">("queue");
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
    setFollow(true);
    setDelivery("queue");
    setEditing(null);
    setQueueOpen(false);
  }, [p.id]);
  const managed = agent?.source === "managed";
  const canSteer = managed && agent?.status === "running" && !!agent?.turnId;
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
  const submit = async () => {
    if (
      sendLock.current ||
      p.sending ||
      uploading ||
      (!p.draft.trim() && !assets.length)
    )
      return;
    sendLock.current = true;
    const id = p.id || "";
    try {
      await p.send({
        assets: assets.map((asset) => asset.id),
        delivery: canSteer ? delivery : "queue",
      });
      setAttachments((current) => ({ ...current, [id]: [] }));
      if (p.id && managed) await loadQueue(p.id);
    } catch (error) {
      p.notify(errorText(error));
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
  useEffect(() => {
    if (follow && scroll.current)
      scroll.current.scrollTop = scroll.current.scrollHeight;
  }, [items, follow]);
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
    (r) => !r.agent || r.agent === p.id || team.some((a) => a.id === r.agent),
  );
  const first = p.room?.members[0] || items.find((m) => m.sender)?.sender;
  const canSend = !!agent?.canSend || !!p.legacy || !p.id;
  const groups: (Message | Message[])[] = [];
  for (const item of items) {
    if (["output", "tool"].includes(item.role)) {
      const last = groups.at(-1);
      if (Array.isArray(last)) last.push(item);
      else groups.push([item]);
    } else groups.push(item);
  }
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
      <div
        id="messages"
        ref={scroll}
        onScroll={(e) => {
          const t = e.currentTarget;
          setFollow(t.scrollHeight - t.scrollTop - t.clientHeight < 80);
        }}
      >
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
        {!items.length &&
          !notice &&
          !["running", "starting"].includes(agent?.status || "") && (
            <div className="empty-chat">
              {!p.room && (
                <span className="empty-mark">
                  <Terminal size={28} />
                </span>
              )}
              <h2>{p.room ? "No messages yet" : "What should we work on?"}</h2>
              <p>
                {p.room
                  ? "Agent messages will appear here."
                  : "Give your lead a task. It can delegate work and report the results here."}
              </p>
            </div>
          )}
        {groups.map((m) =>
          Array.isArray(m) ? (
            <Activity key={m[0].id} items={m} />
          ) : (
            <article
              key={m.id}
              data-message={m.id}
              className={`message ${m.role === "user" ? "user" : "assistant"} ${p.room ? "bubble " + (m.sender !== first ? "outgoing" : "incoming") : ""}`}
            >
              {p.room && m.senderName && (
                <span className="message-label">{m.senderName}</span>
              )}
              {m.pending && <span className="message-label">Queued</span>}
              {m.role === "user" ? (
                <div className="prose plain">{m.text}</div>
              ) : (
                <StreamingText text={m.text} streaming={!!m.streaming} />
              )}
              {Array.isArray(m.assets) && (
                <MessageAttachments assets={m.assets} notify={p.notify} />
              )}
              {m.truncated && (
                <p className="notice">This message is clipped.</p>
              )}
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
                        quote(
                          excerpt?.messageId === m.id ? excerpt.text : m.text,
                        );
                        window.getSelection()?.removeAllRanges();
                      }}
                    >
                      <Quote size={14} />
                    </ActionIcon>
                    {managed &&
                      m.role === "assistant" &&
                      m.turnId &&
                      m.turnId !== agent?.turnId &&
                      items
                        .filter(
                          (item) =>
                            item.role === "assistant" &&
                            item.turnId === m.turnId,
                        )
                        .at(-1)?.id === m.id && (
                        <ActionIcon
                          size="sm"
                          aria-label="Branch after this turn"
                          disabled={
                            m.turnId === agent?.turnId && !!agent?.inFlight
                          }
                          onClick={() => void branch(m)}
                        >
                          <GitBranch size={14} />
                        </ActionIcon>
                      )}
                  </>
                )}
              </div>
            </article>
          ),
        )}
        {!p.room && <AgentPhase agent={agent} connection={connection} />}
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
          data={p.data}
          agent={agent}
          compact
          refresh={p.refresh}
          notify={p.notify}
          onSelect={p.onSelect}
        />
      )}
      <Requests
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
              minRows={2}
              maxRows={8}
              id="message"
              ref={input}
              aria-label="Message"
              placeholder={
                canSend
                  ? "What should we work on?"
                  : "This session has no live mailbox"
              }
              disabled={!canSend}
              value={p.draft}
              onChange={(e) => p.setDraft(e.target.value)}
              maxLength={12000}
              rows={2}
              onKeyDown={(e) => {
                if (
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
              <Button
                type="button"
                id="project"
                size="compact-sm"
                leftSection={<Folder size={15} />}
                onClick={p.project}
                disabled={!agent?.cwd}
              >
                {agent?.cwd?.split("/").filter(Boolean).at(-1) || "Project"}
              </Button>
              {managed && canSteer && (
                <Menu position="top-end" withinPortal>
                  <Menu.Target>
                    <Button
                      type="button"
                      size="compact-xs"
                      variant="subtle"
                      aria-label="Message delivery"
                      leftSection={
                        delivery === "steer" ? (
                          <Zap size={13} />
                        ) : (
                          <ListOrdered size={13} />
                        )
                      }
                    >
                      {delivery === "steer" ? "Send now" : "Queue"}
                    </Button>
                  </Menu.Target>
                  <Menu.Dropdown>
                    <Menu.Item
                      leftSection={<ListOrdered size={14} />}
                      onClick={() => setDelivery("queue")}
                    >
                      Queue after the current turn
                    </Menu.Item>
                    <Menu.Item
                      leftSection={<Zap size={14} />}
                      onClick={() => setDelivery("steer")}
                    >
                      Correct the current turn
                    </Menu.Item>
                  </Menu.Dropdown>
                </Menu>
              )}
              <span id="send-state">{p.sending ? "Sending…" : ""}</span>
              {agent &&
                ["running", "starting", "approval"].includes(agent.status) && (
                  <ActionIcon
                    type="button"
                    id="stop"
                    aria-label="Stop agent"
                    onClick={() =>
                      void api("/api/stop", { id: p.id, descendants: false })
                        .then(p.refresh)
                        .catch((e) => p.notify(errorText(e)))
                    }
                  >
                    <Square size={14} fill="currentColor" />
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
              >
                <ArrowUp size={19} />
              </ActionIcon>
            </div>
          </form>
          {agent?.source === "managed" && (
            <Usage
              key={p.agent?.accountKey || "default"}
              agent={{ ...agent, accountKey: p.agent?.accountKey || "default" }}
              limits={p.limits}
              reload={p.reloadLimits}
            />
          )}
          <p className="composer-hint">
            Enter to send · Shift + Enter for a new line
          </p>
        </>
      )}
    </section>
  );
}
