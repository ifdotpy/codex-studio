import {
  ActionIcon,
  Avatar,
  Button,
  Loader,
  TextInput,
  Tabs,
  UnstyledButton,
} from "@mantine/core";
import {
  ArrowDown,
  ArrowLeft,
  Megaphone,
  MessageCircle,
  Search,
} from "lucide-react";
import { useCallback, useEffect, useState, type ReactNode } from "react";
import { errorText, save, saved } from "../api";
import { useMediaQuery } from "@mantine/hooks";
import { messageAttentionCount } from "../chatScope";
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
  forYou,
  forLead,
  focusRequestId,
  focusRoomId,
}: {
  data: Snapshot;
  leadId?: string;
  forYou: ReactNode;
  forLead: ReactNode;
  focusRequestId?: string;
  focusRoomId?: string;
}) {
  const [view, setView] = useState<string | null>("you");
  const narrow = useMediaQuery("(max-width: 640px)");
  const scope = JSON.stringify([data.stateDir, leadId]);
  useEffect(() => {
    if (focusRequestId) setView("you");
  }, [focusRequestId]);
  const [query, setQuery] = useState("");
  const [limit, setLimit] = useState(60);
  const [selection, setSelection] = useState<{
    scope: string;
    id: string;
  } | null>(null);
  const selected = selection?.scope === scope ? selection.id : null;
  const setSelected = (id: string) => setSelection({ scope, id });
  const [detail, setDetail] = useState(false);
  useEffect(() => {
    setQuery("");
    setLimit(60);
    setDetail(false);
    setView("you");
  }, [scope]);
  useEffect(() => {
    if (!focusRoomId) return;
    setView("team");
    setSelected(focusRoomId);
    setDetail(true);
  }, [focusRoomId, focusRequestId, scope]);
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
          (room.members.every((id) => members.has(id)) ||
            (!!room.reviewTargets?.length &&
              room.members.some((id) => members.has(id))))),
  );
  const name = (room: Room) => {
    if (room.kind === "broadcast") return "Team broadcast";
    if (room.reviewTargets?.length) return room.name;
    const peers = room.members.filter((id) => id !== leadId);
    return peers.length === 1
      ? data.threads.find((agent) => agent.id === peers[0])?.name || room.name
      : room.name;
  };
  const filtered = rooms
    .filter((room) =>
      `${name(room)} ${room.name} ${room.lastMessage?.text || ""}`
        .toLowerCase()
        .includes(query.toLowerCase()),
    )
    .sort(
      (a, b) =>
        (b.lastMessage?.created || b.updated) -
        (a.lastMessage?.created || a.updated),
    );
  const room = rooms.find((item) => item.id === selected);
  useEffect(() => {
    if (
      view !== "team" ||
      narrow ||
      room ||
      !rooms.length ||
      (focusRoomId && selected === focusRoomId)
    )
      return;
    const latest = [...rooms].sort(
      (a, b) =>
        (b.lastMessage?.created || b.updated) -
        (a.lastMessage?.created || a.updated),
    )[0];
    setSelection({ scope, id: latest.id });
  }, [view, narrow, room, rooms, scope, focusRoomId, selected]);
  const attention = messageAttentionCount(data);
  const groups = [
    {
      id: "reviews",
      title: "Reviews",
      rooms: filtered.filter((room) => !!room.reviewTargets?.length),
    },
    {
      id: "orchestrator",
      title: "To orchestrator",
      rooms: filtered.filter(
        (room) =>
          room.kind === "private" &&
          !room.reviewTargets?.length &&
          room.members.includes(leadId || ""),
      ),
    },
    {
      id: "broadcast",
      title: "Team broadcast",
      rooms: filtered.filter((room) => room.kind === "broadcast"),
    },
    {
      id: "agents",
      title: "Between agents",
      rooms: filtered.filter(
        (room) =>
          room.kind === "private" &&
          !room.reviewTargets?.length &&
          !room.members.includes(leadId || ""),
      ),
    },
  ];
  const visibleRooms = new Set(
    groups
      .flatMap((group) => group.rooms)
      .slice(0, limit)
      .map((room) => room.id),
  );
  return (
    <Tabs
      className="messages-views"
      value={view}
      onChange={setView}
      keepMounted={false}
    >
      <Tabs.List className="messages-view-tabs" aria-label="Message recipients">
        <Tabs.Tab value="you" aria-label="For you">
          For you{" "}
          {attention > 0 && (
            <span className="messages-tab-count">{attention}</span>
          )}
        </Tabs.Tab>
        <Tabs.Tab value="team" aria-label="Team">
          Team <span className="messages-tab-count">{rooms.length}</span>
        </Tabs.Tab>
      </Tabs.List>
      <Tabs.Panel
        value="you"
        className="messages-for-you for-you"
        aria-label="For you"
      >
        {forYou}
      </Tabs.Panel>
      <Tabs.Panel value="team" className="messages-team-panel">
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
              {groups
                .filter(
                  (group) =>
                    group.rooms.length > 0 ||
                    (group.id === "orchestrator" &&
                      data.runtime.complaints.some(
                        (item) => item.recipient === "lead",
                      )),
                )
                .map((group) => (
                  <details
                    className="message-group"
                    key={group.id}
                    data-message-group={group.id}
                    open
                  >
                    <summary>
                      {group.title} <span>{group.rooms.length}</span>
                    </summary>
                    {group.id === "orchestrator" && forLead}
                    {group.rooms
                      .filter((item) => visibleRooms.has(item.id))
                      .map((item) => {
                        const unread =
                          !!item.lastMessage &&
                          (seen[item.id] || 0) < item.lastMessage.seq;
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
                              size={32}
                              radius="xl"
                              color={
                                item.kind === "broadcast" ? "indigo" : "gray"
                              }
                            >
                              {item.kind === "broadcast" ? (
                                <Megaphone size={18} />
                              ) : (
                                <MessageCircle size={18} />
                              )}
                            </Avatar>
                            <span className="team-room-copy">
                              <span className="team-room-title">
                                <strong title={item.name}>{name(item)}</strong>
                                <time>{time(item.lastMessage?.created)}</time>
                              </span>
                              <span className="team-room-preview">
                                <span>
                                  {item.lastMessage?.text || "No messages yet"}
                                </span>
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
                  </details>
                ))}
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
              title={name(room)}
              data={data}
              visible={detail}
              markRead={markRead}
              back={() => setDetail(false)}
            />
          ) : (
            <div className="team-chat-placeholder">
              <MessageCircle size={30} />
              <strong>Select a conversation</strong>
              <p>Select a team conversation to read its messages.</p>
            </div>
          )}
        </div>
      </Tabs.Panel>
    </Tabs>
  );
}

function RoomMessages({
  room,
  title,
  data,
  back,
  markRead,
  visible,
}: {
  room: Room;
  title: string;
  data: Snapshot;
  back: () => void;
  markRead: (id: string, seq: number) => void;
  visible: boolean;
}) {
  const { items, loaded, before, older, notice, reload } = useMessages(
    room.id,
    "room",
    false,
    data.stateDir,
  );
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [retrying, setRetrying] = useState(false);
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
          <strong title={participants}>{title}</strong>
          <span className="team-room-kind">
            {room.kind === "broadcast"
              ? "Team broadcast"
              : room.reviewTargets?.length
                ? "Review discussion"
                : "Agent conversation"}
          </span>
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
            <div role="alert" className="team-chat-error">
              <p>{notice}</p>
              <Button
                size="compact-xs"
                variant="light"
                loading={retrying}
                onClick={async () => {
                  setRetrying(true);
                  try {
                    await reload();
                  } finally {
                    setRetrying(false);
                  }
                }}
              >
                Retry messages
              </Button>
            </div>
          )}
          {!loaded && (
            <div className="team-chat-loading">
              <Loader size="sm" />
              <span>Loading messages</span>
            </div>
          )}
          {loaded && !items.length && !notice && (
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
