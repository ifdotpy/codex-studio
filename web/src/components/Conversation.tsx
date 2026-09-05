import { ActionIcon, Button, Textarea } from "@mantine/core";
import {
  ArrowDown,
  ArrowUp,
  Copy,
  Folder,
  Square,
  Terminal,
} from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { api, errorText } from "../api";
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
import Activity from "./Activity";
import StreamingText from "./StreamingText";
import AgentPhase from "./AgentPhase";
export default function Conversation(p: {
  id: string | null;
  agent?: Agent;
  room?: Room;
  legacy?: Json;
  data: Snapshot;
  draft: string;
  setDraft: (v: string) => void;
  send: () => Promise<void>;
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
  useEffect(() => setFollow(true), [p.id]);
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
              </div>
            </article>
          ),
        )}
        {!p.room && <AgentPhase agent={agent} connection={connection} />}
      </div>
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
          <form
            id="composer"
            onSubmit={(e) => {
              e.preventDefault();
              void p.send();
            }}
          >
            <Textarea
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
                  void p.send();
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
                disabled={!canSend || p.sending || !p.draft.trim()}
                aria-label="Send message"
              >
                <ArrowUp size={19} />
              </ActionIcon>
            </div>
          </form>
          {agent?.source === "managed" && (
            <Usage agent={agent} limits={p.limits} reload={p.reloadLimits} />
          )}
          <p className="composer-hint">
            Enter to send · Shift + Enter for a new line
          </p>
        </>
      )}
    </section>
  );
}
