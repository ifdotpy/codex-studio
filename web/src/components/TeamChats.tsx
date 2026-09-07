import {
  ActionIcon,
  Avatar,
  Button,
  Loader,
  TextInput,
  UnstyledButton,
} from "@mantine/core";
import {
  ArrowDown,
  ArrowLeft,
  Megaphone,
  MessageCircle,
  Search,
} from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { errorText, save, saved } from "../api";
import { useMessages } from "../hooks";
import type { Room, Snapshot } from "../types";
import StreamingText from "./StreamingText";
import { useConversationScroll } from "./useConversationScroll";
import "./team-chats.css";

const time = (value?: number) =>
  value
    ? new Date(value * 1000).toLocaleTimeString([], {
        hour: "2-digit",
        minute: "2-digit",
      })
    : "";

export default function TeamChats({
  data,
  leadId,
}: {
  data: Snapshot;
  leadId?: string;
}) {
  const [query, setQuery] = useState("");
  const [limit, setLimit] = useState(60);
  const [selected, setSelected] = useState<string | null>(null);
  const [detail, setDetail] = useState(false);
  const seenKey = `codex-chat-seen:${data.stateDir}`;
  const [seen, setSeen] = useState<Record<string, number>>(() =>
    saved(seenKey, {}),
  );
  const markRead = useCallback(
    (id: string, seq: number) => {
      setSeen((previous) => {
        if ((previous[id] || 0) >= seq) return previous;
        const next = {
          ...saved<Record<string, number>>(seenKey, {}),
          ...previous,
          [id]: seq,
        };
        save(seenKey, next);
        return next;
      });
    },
    [seenKey],
  );
  const members = new Set(
    data.threads
      .filter(
        (agent) => !!leadId && (agent.id === leadId || agent.rootId === leadId),
      )
      .map((agent) => agent.id),
  );
  const rooms = data.runtime.rooms.filter(
    (room) =>
      !!leadId &&
      (room.kind === "broadcast"
        ? room.rootId === leadId
        : room.members.length > 0 &&
          room.members.every((id) => members.has(id))),
  );
  const name = (room: Room) =>
    room.kind === "broadcast" ? "Team broadcast" : room.name;
  const filtered = rooms
    .filter((room) =>
      `${name(room)} ${room.lastMessage?.text || ""}`
        .toLowerCase()
        .includes(query.toLowerCase()),
    )
    .sort(
      (a, b) =>
        Number(b.kind === "broadcast") - Number(a.kind === "broadcast") ||
        (b.lastMessage?.created || b.updated) -
          (a.lastMessage?.created || a.updated),
    );
  const room = rooms.find((item) => item.id === selected) || filtered[0];
  return (
    <div className={`team-chats ${detail && room ? "show-room" : ""}`}>
      <section className="team-room-list" aria-label="Team conversations">
        <div className="team-room-search">
          <TextInput
            aria-label="Search team chats"
            placeholder="Search team chats"
            leftSection={<Search size={15} />}
            value={query}
            onChange={(event) => {
              setQuery(event.target.value);
              setLimit(60);
            }}
          />
        </div>
        <div className="team-room-rows">
          {filtered.slice(0, limit).map((item) => {
            const unread =
              !!item.lastMessage && (seen[item.id] || 0) < item.lastMessage.seq;
            return (
              <UnstyledButton
                key={item.id}
                data-room={item.id}
                className={`team-room-row ${room?.id === item.id ? "selected" : ""}`}
                aria-pressed={room?.id === item.id}
                onClick={() => {
                  setSelected(item.id);
                  setDetail(true);
                }}
              >
                <Avatar
                  size={38}
                  radius="xl"
                  color={item.kind === "broadcast" ? "indigo" : "gray"}
                >
                  {item.kind === "broadcast" ? (
                    <Megaphone size={18} />
                  ) : (
                    <MessageCircle size={18} />
                  )}
                </Avatar>
                <span className="team-room-copy">
                  <span className="team-room-title">
                    <strong>{name(item)}</strong>
                    <time>{time(item.lastMessage?.created)}</time>
                  </span>
                  <span className="team-room-preview">
                    <span>{item.lastMessage?.text || "No messages yet"}</span>
                    {unread && (
                      <i
                        className="team-room-unread"
                        aria-label="Unread messages"
                      />
                    )}
                  </span>
                </span>
              </UnstyledButton>
            );
          })}
          {filtered.length > limit && (
            <Button
              variant="subtle"
              fullWidth
              onClick={() => setLimit(limit + 60)}
            >
              Show more chats
            </Button>
          )}
          {!filtered.length && (
            <p className="team-chat-empty">
              {query
                ? "No matching chats."
                : "Agent conversations appear here when this team exchanges messages."}
            </p>
          )}
        </div>
      </section>
      {room ? (
        <RoomMessages
          key={room.id}
          room={room}
          data={data}
          visible={detail}
          markRead={markRead}
          back={() => setDetail(false)}
        />
      ) : (
        <div className="team-chat-placeholder">
          <MessageCircle size={30} />
          <strong>No team conversations yet</strong>
          <p>Broadcasts and private messages stay with this chat.</p>
        </div>
      )}
    </div>
  );
}

function RoomMessages({
  room,
  data,
  back,
  markRead,
  visible,
}: {
  room: Room;
  data: Snapshot;
  back: () => void;
  markRead: (id: string, seq: number) => void;
  visible: boolean;
}) {
  const { items, loaded, before, older, notice } = useMessages(
    room.id,
    "room",
    false,
    data.stateDir,
  );
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [historyError, setHistoryError] = useState("");
  const { scroll, content, follow, setFollow, onScroll } =
    useConversationScroll(`${data.stateDir}:team-room:${room.id}`, loaded);
  const participants = room.members
    .map(
      (id) =>
        data.threads.find((agent) => agent.id === id)?.name || "Former agent",
    )
    .join(" · ");
  useEffect(() => {
    if (loaded && follow && scroll.current?.getClientRects().length) {
      const seq = Math.max(0, ...items.map((item) => item.seq || 0));
      if (seq) markRead(room.id, seq);
    }
  }, [items, loaded, follow, markRead, room.id, visible]);
  return (
    <section
      className="team-room-detail"
      aria-label="Agent conversation"
      data-room-detail={room.id}
    >
      <header className="team-room-header">
        <ActionIcon
          className="team-room-back"
          aria-label="Back to team chats"
          onClick={back}
        >
          <ArrowLeft size={18} />
        </ActionIcon>
        <div>
          <strong>
            {room.kind === "broadcast" ? "Team broadcast" : room.name}
          </strong>
          <p>
            {room.kind === "broadcast" ? "Everyone in this team" : participants}
          </p>
        </div>
      </header>
      <div
        className="team-room-messages"
        ref={scroll}
        onScroll={onScroll}
        tabIndex={0}
        aria-label="Messages"
      >
        <div ref={content}>
          {before != null && (
            <Button
              className="team-older"
              variant="subtle"
              size="xs"
              loading={loadingOlder}
              onClick={async () => {
                setFollow(false);
                setLoadingOlder(true);
                try {
                  await older();
                  setHistoryError("");
                } catch (error) {
                  setHistoryError(errorText(error));
                } finally {
                  setLoadingOlder(false);
                }
              }}
            >
              Earlier messages
            </Button>
          )}
          {historyError && (
            <p role="alert" className="team-chat-empty">
              Could not load earlier messages: {historyError}
            </p>
          )}
          {notice && (
            <p role="alert" className="team-chat-empty">
              {notice}
            </p>
          )}
          {!loaded && (
            <div className="team-chat-loading">
              <Loader size="sm" />
              <span>Loading messages</span>
            </div>
          )}
          {loaded && !items.length && (
            <p className="team-chat-empty">No messages yet.</p>
          )}
          {items.map((message) => (
            <article
              key={message.id}
              data-message={message.id}
              className={`team-message ${message.sender === room.members[0] && room.kind === "private" ? "from-first" : ""}`}
            >
              <strong className="team-message-sender">
                {message.senderName ||
                  data.threads.find((agent) => agent.id === message.sender)
                    ?.name ||
                  "Agent"}
              </strong>
              <StreamingText
                text={message.text || ""}
                agentId={message.sender}
              />
              <time
                title={
                  message.created
                    ? new Date(message.created * 1000).toLocaleString()
                    : undefined
                }
              >
                {time(message.created)}
              </time>
            </article>
          ))}
        </div>
      </div>
      {!follow && (
        <Button
          className="team-latest"
          size="compact-xs"
          variant="light"
          leftSection={<ArrowDown size={13} />}
          onClick={() => setFollow(true)}
        >
          Latest messages
        </Button>
      )}
      <footer className="team-room-footer">
        {room.kind === "private"
          ? "Private between these agents · Visible to you"
          : "Messages to the whole team · Visible to you"}
      </footer>
    </section>
  );
}
